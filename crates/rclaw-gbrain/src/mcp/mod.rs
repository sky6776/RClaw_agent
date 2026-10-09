//! MCP stdio server — JSON-RPC 2.0 over stdio
//! Mirrors gbrain's src/mcp/server.ts
//!
//! Implements the Model Context Protocol for agent integration.
//! All operations are dispatched through the Operations layer with
//! OperationContext.remote = true (untrusted callers).

pub mod tool_defs;

use crate::config::Config;
use crate::error::{GBrainError, OperationError, Result};
use crate::mcp::tool_defs::get_operation_def;
use crate::operations::{OpContext, Operations, ParamType};
use crate::sqlite_engine::SqliteEngine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// JSON-RPC 2.0 request
#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

/// JSON-RPC 2.0 response — or None for notifications (no response should be sent)
enum HandleResult {
    Response(JsonRpcResponse),
    NoResponse, // For JSON-RPC notifications — server must not reply
}
#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

/// JSON-RPC 2.0 error
#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

/// MCP 分页参数安全边界：limit <= 0 视为未指定，回退到 default；
/// limit 超过 max 则 clamp 到 max，防止远程调用绕过限制全量 dump。
fn normalize_limit(limit: i64, default: i64, max: i64) -> i64 {
    if limit <= 0 {
        default
    } else {
        limit.min(max)
    }
}

/// offset 不能为负数，否则可能导致 SQL 行为异常或绕过安全限制。
fn normalize_offset(offset: i64) -> i64 {
    offset.max(0)
}

fn parse_mcp_page_ranges(input: &str, max_page: i32) -> Result<Vec<i32>> {
    if max_page <= 0 {
        return Err(GBrainError::InvalidInput(
            "无法确定 PDF 总页数，不能校验 OCR 页码范围".to_string(),
        ));
    }

    let mut pages = Vec::new();
    for raw_part in input.split(',') {
        let part = raw_part.trim();
        if part.is_empty() {
            return Err(GBrainError::InvalidInput(
                "OCR 页码范围包含空片段".to_string(),
            ));
        }

        let (start, end) = if let Some((left, right)) = part.split_once('-') {
            if left.trim().is_empty() || right.trim().is_empty() {
                return Err(GBrainError::InvalidInput(format!(
                    "OCR 页码范围格式无效: {}",
                    part
                )));
            }
            let start = left.trim().parse::<i32>().map_err(|_| {
                GBrainError::InvalidInput(format!("OCR 页码不是有效整数: {}", left.trim()))
            })?;
            let end = right.trim().parse::<i32>().map_err(|_| {
                GBrainError::InvalidInput(format!("OCR 页码不是有效整数: {}", right.trim()))
            })?;
            (start, end)
        } else {
            let page = part.parse::<i32>().map_err(|_| {
                GBrainError::InvalidInput(format!("OCR 页码不是有效整数: {}", part))
            })?;
            (page, page)
        };

        if start < 1 || end < 1 {
            return Err(GBrainError::InvalidInput(format!(
                "OCR 页码必须从 1 开始: {}",
                part
            )));
        }
        if start > end {
            return Err(GBrainError::InvalidInput(format!(
                "OCR 页码范围起始页大于结束页: {}",
                part
            )));
        }
        if end > max_page {
            return Err(GBrainError::InvalidInput(format!(
                "OCR 页码范围超出 PDF 总页数 {}: {}",
                max_page, part
            )));
        }

        for page in start..=end {
            if !pages.contains(&page) {
                pages.push(page);
            }
        }
    }

    if pages.is_empty() {
        return Err(GBrainError::InvalidInput("OCR 页码范围为空".to_string()));
    }

    pages.sort_unstable();
    Ok(pages)
}

fn pdf_page_analyses_from_metadata(
    parsed: &crate::kb::parser::ParsedDocument,
) -> Result<Vec<crate::kb::ocr_detector::PdfPageAnalysis>> {
    let page_analyses_raw = parsed
        .metadata
        .get("page_analyses")
        .and_then(|v| serde_json::from_str::<Vec<serde_json::Value>>(v).ok())
        .unwrap_or_default();

    if page_analyses_raw.is_empty() {
        return Err(GBrainError::InvalidInput(
            "无法读取 PDF 页级分析，不能自动检测 OCR 页码".to_string(),
        ));
    }

    Ok(page_analyses_raw
        .iter()
        .map(|pa| {
            let page_number = pa.get("page_number").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let text = pa
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let char_count = pa.get("char_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let image_area_ratio = pa
                .get("image_area_ratio")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let image_count = pa.get("image_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let has_vector = pa
                .get("has_vector_or_unknown_objects")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let image_regions = if image_count > 0 {
                (0..image_count)
                    .map(|_| crate::kb::ocr_detector::PdfImageRegion {
                        bbox: None,
                        area_ratio: image_area_ratio / image_count as f64,
                    })
                    .collect()
            } else if image_area_ratio > 0.0 {
                vec![crate::kb::ocr_detector::PdfImageRegion {
                    bbox: None,
                    area_ratio: image_area_ratio,
                }]
            } else {
                vec![]
            };

            crate::kb::ocr_detector::PdfPageAnalysis {
                page_number,
                text,
                text_blocks: vec![],
                char_count,
                image_regions,
                image_area_ratio,
                has_vector_or_unknown_objects: has_vector,
                width: pa.get("width").and_then(|v| v.as_u64()).map(|v| v as u32),
                height: pa.get("height").and_then(|v| v.as_u64()).map(|v| v as u32),
                content_parse_failed: pa
                    .get("content_parse_failed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                has_vector_drawing_ops: pa
                    .get("has_vector_drawing_ops")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                has_invisible_text: pa
                    .get("has_invisible_text")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                font_encoding_suspected: pa
                    .get("font_encoding_suspected")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            }
        })
        .collect())
}

/// 校验参数：仅校验 OperationDef 中 required=true 的参数是否必填，
/// 可选参数仅在传入时校验类型。复用 tool_defs 的 OperationDef/ParamDef，
/// 避免手写校验规则与 schema 漂移。
fn validate_params(tool_name: &str, arguments: &Value) -> Option<String> {
    // 从 OperationDef 查找工具定义
    let op_def = get_operation_def(tool_name)?;

    for param in op_def.params.iter() {
        match arguments.get(param.name) {
            None => {
                // 仅 required 参数缺失时报错
                if param.required {
                    return Some(format!(
                        "缺少必填参数 '{}' (工具 '{}')",
                        param.name, tool_name
                    ));
                }
            }
            Some(val) => {
                // 传入时校验类型是否匹配
                let type_ok = match param.param_type {
                    ParamType::String => val.is_string(),
                    ParamType::Integer => val.is_u64() || val.is_i64(),
                    ParamType::Boolean => val.is_boolean(),
                    ParamType::Number => val.is_f64() || val.is_i64() || val.is_u64(),
                    ParamType::Array => val.is_array(),
                    ParamType::Object => val.is_object(),
                };
                if !type_ok {
                    return Some(format!(
                        "参数 '{}' (工具 '{}') 应为 {}，实际为 {}",
                        param.name,
                        tool_name,
                        param.param_type.json_type_name(),
                        val
                    ));
                }
                // 校验 enum_values（如果有声明），防止 query.mode=grap、review.status=pendng
                // 等拼写错误穿透到 service 层被静默 fallback
                if let Some(enums) = param.enum_values {
                    if let Some(s) = val.as_str() {
                        if !enums.contains(&s) {
                            return Some(format!(
                                "参数 '{}' (工具 '{}') 无效值 '{}'，有效值: {}",
                                param.name,
                                tool_name,
                                s,
                                enums.join(", ")
                            ));
                        }
                    }
                }
            }
        }
    }

    None
}

/// MCP server running over stdio
pub struct McpServer {
    engine: SqliteEngine,
    config: Config,
    working_dir: PathBuf,
}

impl McpServer {
    pub fn new(engine: SqliteEngine) -> Self {
        let config = Config::load().unwrap_or_else(|e| {
            panic!("gbrain 配置加载失败，必须通过环境变量显式配置必选项: {}", e);
        });
        Self::with_config(engine, config)
    }

    pub fn with_config(engine: SqliteEngine, config: Config) -> Self {
        let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::with_config_and_working_dir(engine, config, working_dir)
    }

    /// 构造可嵌入的工具服务，并显式指定远程文件访问根目录。
    ///
    /// Android 等宿主没有可靠的进程 current_dir，必须由宿主传入 app 私有工作目录；
    /// stdio CLI 仍通过 [`with_config`](Self::with_config) 保持原行为。
    pub fn with_config_and_working_dir(
        engine: SqliteEngine,
        config: Config,
        working_dir: impl AsRef<Path>,
    ) -> Self {
        Self {
            engine,
            config,
            working_dir: working_dir.as_ref().to_path_buf(),
        }
    }

    /// 测试辅助：直接调用 MCP tools/call dispatch 路径
    /// 不走 stdio，直接传入工具名和参数，返回 dispatch 结果。
    /// 仅用于集成测试验证参数映射、内部工具拦截和返回包装。
    pub fn dispatch_tool_call(
        &mut self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let params = serde_json::json!({
            "name": tool_name,
            "arguments": arguments,
        });
        self.handle_tool_call(Some(params))
    }

    /// Run the MCP server, reading JSON-RPC from stdin and writing to stdout
    pub fn run(&mut self) -> Result<()> {
        info!("MCP server starting (stdio transport)");
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        let reader = stdin.lock();

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!("MCP stdin read error: {}", e);
                    break;
                }
            };

            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let request: JsonRpcRequest = match serde_json::from_str(line) {
                Ok(r) => r,
                Err(e) => {
                    let response = JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: None,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32700,
                            message: format!("Parse error: {}", e),
                            data: None,
                        }),
                    };
                    if let Ok(resp_str) = serde_json::to_string(&response) {
                        let _ = writeln!(stdout, "{}", resp_str);
                    }
                    let _ = stdout.flush();
                    continue;
                }
            };

            let result = self.handle_request(request);

            match result {
                HandleResult::Response(response) => {
                    if let Ok(resp_str) = serde_json::to_string(&response) {
                        let _ = writeln!(stdout, "{}", resp_str);
                        let _ = stdout.flush();
                    }
                }
                HandleResult::NoResponse => {
                    // JSON-RPC 2.0: "The Server MUST NOT reply to a Notification"
                    // No output for notifications
                }
            }
        }

        Ok(())
    }

    fn handle_request(&mut self, request: JsonRpcRequest) -> HandleResult {
        let id = request.id.clone();
        debug!(method = %request.method, "Handling MCP request");

        match request.method.as_str() {
            "initialize" => HandleResult::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": { "listChanged": false },
                    },
                    "serverInfo": {
                        "name": "gbrain",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                })),
                error: None,
            }),

            "tools/list" => {
                let tools = tool_defs::build_tool_defs();
                let tools_json: Vec<Value> = tools
                    .into_iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": t.input_schema,
                        })
                    })
                    .collect();

                HandleResult::Response(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    result: Some(serde_json::json!({ "tools": tools_json })),
                    error: None,
                })
            }

            "tools/call" => {
                let result = self.handle_tool_call(request.params);
                match result {
                    Ok(value) => {
                        info!(tool = "tools/call", "MCP tool call completed successfully");
                        HandleResult::Response(JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id,
                            result: Some(serde_json::json!({
                                "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }],
                            })),
                            error: None,
                        })
                    }
                    Err(e) => {
                        warn!(error = %e, "MCP tool call failed, sending error response");
                        // P2-9: Convert GBrainError to OperationError for structured response
                        let op_err = e.to_operation_error();
                        let error_data = serde_json::json!({
                            "code": match &op_err {
                                OperationError::NotFound { .. } => "NOT_FOUND",
                                OperationError::Forbidden { .. } => "FORBIDDEN",
                                OperationError::Validation { .. } => "VALIDATION",
                                OperationError::Failed { .. } => "INTERNAL",
                            },
                            "message": op_err.to_string(),
                            "suggestion": match &op_err {
                                OperationError::NotFound { suggestion, .. } => suggestion,
                                OperationError::Forbidden { suggestion, .. } => suggestion,
                                OperationError::Validation { suggestion, .. } => suggestion,
                                OperationError::Failed { suggestion, .. } => suggestion,
                            },
                            "docs_url": match &op_err {
                                OperationError::NotFound { docs_url, .. } => docs_url,
                                OperationError::Forbidden { docs_url, .. } => docs_url,
                                OperationError::Validation { docs_url, .. } => docs_url,
                                OperationError::Failed { docs_url, .. } => docs_url,
                            },
                        });
                        HandleResult::Response(JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id,
                            result: Some(serde_json::json!({
                                "content": [{ "type": "text", "text": format!("Error: {}", e) }],
                                "isError": true,
                                "errorData": error_data,
                            })),
                            error: None,
                        })
                    }
                }
            }

            "ping" => HandleResult::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({})),
                error: None,
            }),

            // JSON-RPC 2.0: "The Server MUST NOT reply to a Notification"
            "notifications/initialized" => HandleResult::NoResponse,

            _ => HandleResult::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("Method not found: {}", request.method),
                    data: None,
                }),
            }),
        }
    }

    fn handle_tool_call(&mut self, params: Option<Value>) -> Result<Value> {
        let params = params.unwrap_or_default();
        let tool_name = params["name"].as_str().unwrap_or("").to_string();
        let arguments = params.get("arguments").cloned().unwrap_or_default();

        debug!(tool = %tool_name, "Dispatching MCP tool call");

        // Validate required parameters before dispatching
        if let Some(err) = validate_params(&tool_name, &arguments) {
            debug!(tool = %tool_name, error = %err, "Parameter validation failed");
            return Err(crate::error::GBrainError::InvalidInput(err));
        }

        let ctx = OpContext {
            remote: true, // MCP callers are untrusted
            working_dir: self.working_dir.clone(),
            dry_run: false,
            subagent_id: None,
        };
        let ops = Operations::with_config(&self.engine, ctx.clone(), self.config.clone());

        match tool_name.as_str() {
            // ========================================================================
            // artifact_* facade — 统一知识操作入口（设计文档 §8.2）
            // 默认暴露给用户，参数映射到现有内部 handler
            // ========================================================================

            // artifact_put: 手动写入长期记忆（设计文档 §4.1.2）
            "artifact_put" => {
                let slug = arguments["slug"].as_str().unwrap_or("").to_string();
                let title = arguments["title"].as_str().map(|s| s.to_string());
                let intent = arguments["intent"].as_str().map(|s| s.to_string());
                let dry_run = arguments["dry_run"].as_bool().unwrap_or(false);

                // 安全校验：slug 不能为空
                if slug.is_empty() {
                    return Err(GBrainError::InvalidInput("slug 不能为空".to_string()));
                }

                // 支持 content 和 file 两种输入方式
                let content = if let Some(c) = arguments["content"].as_str() {
                    c.to_string()
                } else if let Some(f) = arguments["file"].as_str() {
                    // 从文件读取内容
                    let file_path = std::path::PathBuf::from(f);
                    // P2-12 修复：artifact_put --file 使用与 put_memory 相同的 1MB 大小上限
                    // 和文本文件专用扩展名白名单，而非 KB 上传的 50MB 上限和 KB 扩展名列表。
                    // 之前使用 kb_max_file_size_mb（默认 50MB），导致 1MB~50MB 的文本文件
                    // 先完整读入，再在 service 层被拒绝。
                    let allowed_extensions: Vec<String> =
                        crate::artifact::service::TEXT_FILE_EXTENSIONS
                            .iter()
                            .map(|s| s.to_string())
                            .collect();
                    let max_file_bytes = crate::artifact::service::MAX_PUT_MEMORY_CONTENT_BYTES;
                    let validated_path = crate::kb::security::validate_upload_source(
                        &file_path,
                        true,
                        &ctx.working_dir,
                        max_file_bytes,
                        &allowed_extensions,
                    )?;
                    let ext = validated_path
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    let raw_content = std::fs::read(&validated_path).map_err(|e| {
                        GBrainError::FileError(format!("读取文件 {} 失败: {}", f, e))
                    })?;
                    let _mime = crate::kb::security::detect_and_validate_mime(&raw_content, &ext)?;
                    // artifact_put 只接受文本内容，需要转为 String
                    String::from_utf8(raw_content).map_err(|e| {
                        GBrainError::InvalidInput(format!("文件内容不是有效 UTF-8 文本: {}", e))
                    })?
                } else {
                    return Err(GBrainError::InvalidInput(
                        "必须提供 content 或 file 参数".to_string(),
                    ));
                };

                // 安全校验：内容不能为空
                if content.is_empty() {
                    return Err(GBrainError::InvalidInput("内容不能为空".to_string()));
                }

                let svc = ops.artifact_service();
                // P1 修复：force 参数允许强制覆盖已被人工修改的页面
                let force = arguments
                    .get("force")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                svc.put_memory(
                    &slug,
                    &content,
                    title.as_deref(),
                    intent.as_deref(),
                    dry_run,
                    force,
                )
            }

            // artifact_upload: 委派到 ArtifactService.upload_file（设计文档 §8.3）
            "artifact_upload" => {
                let path = arguments["path"].as_str().unwrap_or("").to_string();
                let file_path = std::path::PathBuf::from(&path);

                let intent_str = arguments["intent"].as_str().unwrap_or("auto");
                // 使用 FromStr 严格解析，MCP 客户端传错枚举值直接报错返回
                let intent: crate::artifact::types::UploadIntent = match intent_str.parse() {
                    Ok(i) => i,
                    Err(e) => {
                        return Err(crate::error::GBrainError::InvalidInput(e));
                    }
                };

                // 根据扩展名推断 route plan，再选择允许列表
                let ext_for_route = file_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                let route_plan =
                    crate::artifact::types::infer_route_plan(&ext_for_route, "", &intent);
                let allowed_extensions: Vec<String> = if route_plan.to_file {
                    let mut exts = self.config.kb_allowed_extensions.clone();
                    for extra in &[
                        "png", "jpg", "jpeg", "gif", "bmp", "svg", "webp", "avif", "ico", "tiff",
                        "tif", "zip", "tar", "gz", "json", "xml", "yaml", "yml", "toml",
                    ] {
                        let s = extra.to_string();
                        if !exts.contains(&s) {
                            exts.push(s);
                        }
                    }
                    exts
                } else {
                    self.config.kb_allowed_extensions.clone()
                };
                let max_file_bytes = self.config.kb_max_file_size_mb * 1024 * 1024;

                let validated_path = crate::kb::security::validate_upload_source(
                    &file_path,
                    true,
                    &ctx.working_dir,
                    max_file_bytes,
                    &allowed_extensions,
                )?;

                let ext = validated_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                let content = std::fs::read(&validated_path)?;
                let _mime = crate::kb::security::detect_and_validate_mime(&content, &ext)?;

                let original_name = validated_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string();

                let promotion_policy: Option<crate::artifact::types::PromotionPolicy> = arguments
                    .get("promotion")
                    .and_then(|v| v.as_str())
                    .map(|s| s.parse())
                    .transpose()
                    .map_err(crate::error::GBrainError::InvalidInput)?;

                let input = crate::artifact::types::UploadSourceInput {
                    content,
                    original_name: original_name.clone(),
                    source_kind: crate::artifact::types::SourceKind::Mcp,
                    source_uri: path,
                    intent,
                    target_slug: arguments["target_slug"].as_str().map(|s| s.to_string()),
                    page_slug: arguments["page_slug"].as_str().map(|s| s.to_string()),
                    library_id: arguments["library_id"].as_i64(),
                    folder_id: arguments["folder_id"].as_i64(),
                    promotion_policy,
                    owner_ref: None,
                    metadata: None,
                    path: Some(validated_path.clone()),
                    dry_run: arguments["dry_run"].as_bool().unwrap_or(false),
                };

                let svc = ops.artifact_service();
                let result = svc.upload_file(input)?;
                Ok(serde_json::to_value(result)?)
            }

            // artifact_query: 统一知识查询（设计文档 §7）
            // 返回 ArtifactQueryOutput，隐藏内部 ID
            "artifact_query" => {
                let query = arguments["query"].as_str().unwrap_or("").to_string();
                let mode = arguments["mode"].as_str().map(|s| s.to_string());
                let limit = arguments["limit"].as_u64().map(|l| l as usize);
                let filter_slug = arguments["filter_slug"].as_str().map(|s| s.to_string());
                let include_sources = arguments["include_sources"].as_bool();

                let input = crate::artifact::types::ArtifactQueryInput {
                    query,
                    mode,
                    limit,
                    filter_slug,
                    include_sources,
                };

                let svc = ops.artifact_service();
                let result = svc.query_facade(&input)?;
                Ok(serde_json::to_value(result)?)
            }

            // P1-1 修复：artifact_list 改走 ArtifactService::list_artifacts DTO，
            // 不再返回 raw SourceArtifact（含内部 id/storage_path/metadata_json 等字段）
            "artifact_list" => {
                // 安全边界：clamp limit/offset 防止远程调用负数绕过
                let limit = normalize_limit(arguments["limit"].as_i64().unwrap_or(50), 50, 100);
                let offset = normalize_offset(arguments["offset"].as_i64().unwrap_or(0));
                let svc = ops.artifact_service();
                let items = svc.list_artifacts(limit, offset)?;
                Ok(serde_json::to_value(items)?)
            }

            // artifact_get: 获取 Artifact 详情（设计文档 §4.1.1）
            "artifact_get" => {
                let id_or_uid = arguments["id_or_uid"].as_str().unwrap_or("").to_string();
                let include_projections =
                    arguments["include_projections"].as_bool().unwrap_or(false);
                let include_sources = arguments["include_sources"].as_bool().unwrap_or(false);
                let include_content = arguments["include_content"].as_bool().unwrap_or(false);
                let content_query = arguments["content_query"].as_str();
                let content_mode = arguments["content_mode"].as_str();
                let max_chars = arguments["max_chars"].as_u64().map(|n| n as usize);
                let passage_id = arguments["passage_id"].as_i64();

                let svc = ops.artifact_service();
                let detail = svc.get_artifact_detail_with_content_options(
                    &id_or_uid,
                    include_projections,
                    include_sources,
                    include_content,
                    crate::artifact::service::ArtifactContentOptions {
                        content_query,
                        content_mode,
                        max_chars,
                        passage_id,
                    },
                )?;

                match detail {
                    Some(d) => Ok(serde_json::to_value(d)?),
                    None => Ok(serde_json::json!({
                        "error": format!("未找到 artifact '{}'", id_or_uid)
                    })),
                }
            }

            // artifact_delete: 委派到现有 artifact_delete handler
            // facade 版本用 id_or_uid，内部版本用 artifact_id
            "artifact_delete" => {
                let id_or_uid = arguments["id_or_uid"].as_str().unwrap_or("").to_string();
                let dry_run = arguments["dry_run"].as_bool().unwrap_or(false);

                let svc = ops.artifact_service();
                if dry_run {
                    // P1-5 修复：MCP dry_run 也使用 delete_artifact_dry_run 影响预览，
                    // 与 CLI 行为一致，让 MCP caller 能看到 occurrence/projection/KB/provenance 影响明细
                    let preview = svc.delete_artifact_dry_run(&id_or_uid)?;
                    Ok(serde_json::to_value(preview)?)
                } else {
                    let artifact_id = svc.resolve_artifact_id(&id_or_uid)?;
                    svc.delete_artifact(artifact_id)?;
                    Ok(serde_json::json!({"artifact_id": artifact_id, "status": "deleted"}))
                }
            }

            // artifact_detach: 移除知识源与页面的关联（设计文档 §4.1.4）
            "artifact_detach" => {
                let id_or_uid = arguments["id_or_uid"].as_str().unwrap_or("").to_string();
                let from_slug = arguments["from"].as_str().unwrap_or("").to_string();
                let dry_run = arguments["dry_run"].as_bool().unwrap_or(false);

                if from_slug.is_empty() {
                    return Err(GBrainError::InvalidInput(
                        "from (目标页面 slug) 不能为空".to_string(),
                    ));
                }

                let svc = ops.artifact_service();
                svc.detach(&id_or_uid, &from_slug, dry_run)
            }

            // artifact_restore: 恢复已软删除的知识源（设计文档 §4.1.4）
            "artifact_restore" => {
                let id_or_uid = arguments["id_or_uid"].as_str().unwrap_or("").to_string();
                let dry_run = arguments["dry_run"].as_bool().unwrap_or(false);

                let svc = ops.artifact_service();
                svc.restore(&id_or_uid, dry_run)
            }

            // artifact_reprocess: 重新处理知识源（设计文档 §4.1.4）
            "artifact_reprocess" => {
                let id_or_uid = arguments["id_or_uid"].as_str().unwrap_or("").to_string();
                let dry_run = arguments["dry_run"].as_bool().unwrap_or(false);

                let svc = ops.artifact_service();
                svc.reprocess(&id_or_uid, dry_run)
            }

            // artifact_health: 委派到 ArtifactService.health_check
            "artifact_health" => {
                let svc = ops.artifact_service();
                let report = svc.health_check()?;
                Ok(serde_json::to_value(report)?)
            }

            // artifact_review_list: 列出建议变更，返回用户友好的 ArtifactReviewItem
            "artifact_review_list" => {
                let status = arguments["status"].as_str();
                let target_slug = arguments["target_slug"].as_str();
                // 安全边界：clamp limit 防止远程调用负数绕过
                let limit = normalize_limit(arguments["limit"].as_i64().unwrap_or(50), 50, 100);

                let svc = ops.artifact_service();
                let items = svc.list_suggested_changes(status, target_slug, limit, 0)?;
                Ok(serde_json::to_value(items)?)
            }

            // artifact_review_get: 获取建议变更详情，返回用户友好的 ArtifactReviewItem
            "artifact_review_get" => {
                let change_id = arguments["change_id"].as_i64().unwrap_or(0);

                let svc = ops.artifact_service();
                let item = svc.get_suggested_change(change_id)?;
                Ok(serde_json::to_value(item)?)
            }

            // artifact_review_apply: 应用建议变更
            "artifact_review_apply" => {
                let change_id = arguments["change_id"].as_i64().unwrap_or(0);

                let svc = ops.artifact_service();
                let result = svc.apply_suggested_change(change_id)?;
                Ok(serde_json::to_value(result)?)
            }

            // artifact_review_reject: 拒绝建议变更
            "artifact_review_reject" => {
                let change_id = arguments["change_id"].as_i64().unwrap_or(0);
                let reviewer = arguments["reviewer"].as_str().unwrap_or("mcp").to_string();
                let reason = arguments["reason"].as_str().map(|s| s.to_string());

                let input = crate::artifact::types::ReviewCandidateInput {
                    candidate_id: change_id,
                    action: "reject".to_string(),
                    reviewer,
                    notes: reason,
                };

                let svc = ops.artifact_service();
                let result = svc.reject_suggested_change(input)?;
                Ok(serde_json::to_value(result)?)
            }

            // artifact_review_rollback: 回滚已应用的建议变更
            "artifact_review_rollback" => {
                let change_id = arguments["change_id"].as_i64().unwrap_or(0);

                let svc = ops.artifact_service();
                let result = svc.rollback_suggested_change(change_id)?;
                Ok(serde_json::to_value(result)?)
            }

            // ========================================================================
            // kb_document_status: 查询 KB 文档处理和 OCR 状态
            // ========================================================================
            "kb_document_status" => {
                let doc_id = arguments["document_id"].as_i64().unwrap_or(0);
                if doc_id <= 0 {
                    return Err(crate::error::GBrainError::InvalidInput(
                        "document_id 必须为正整数".to_string(),
                    ));
                }
                let conn = self.engine.connection()?;
                let kb = crate::kb::engine::KbEngine::new(conn);
                let doc = kb.get_document(doc_id)?;

                let mut result = serde_json::json!({
                    "document_id": doc.id,
                    "title": doc.title,
                    "ocr_status": doc.ocr_status,
                    "ocr_text_coverage": doc.ocr_text_coverage,
                    "parsing_status": doc.parsing_status,
                    "embedding_status": doc.embedding_status,
                });

                // 查询页级 OCR 状态
                let conn = self.engine.connection()?;
                let mut stmt = conn.prepare(
                    "SELECT page_number, status, provider, model, confidence, error \
                     FROM kb_document_ocr_pages WHERE document_id = ?1 AND processing_run_id = ?2 ORDER BY page_number",
                )?;
                let pages: Vec<serde_json::Value> = stmt
                    .query_map(rusqlite::params![doc_id, &doc.processing_run_id], |row| {
                        let page_number: i32 = row.get(0)?;
                        let status: String = row.get(1)?;
                        let provider: String = row.get(2)?;
                        let model: String = row.get(3)?;
                        let confidence: Option<f64> = row.get(4)?;
                        let error: Option<String> = row.get(5)?;
                        Ok(serde_json::json!({
                            "page_number": page_number,
                            "status": status,
                            "provider": provider,
                            "model": model,
                            "confidence": confidence,
                            "error": error,
                        }))
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                let failed_pages: Vec<i64> = pages
                    .iter()
                    .filter(|page| page["status"].as_str() == Some("failed"))
                    .filter_map(|page| page["page_number"].as_i64())
                    .collect();
                result["ocr_pages"] = serde_json::json!(pages);
                result["ocr_failed_pages"] = serde_json::json!(failed_pages);

                let mut stmt = conn.prepare(
                    "SELECT label, COUNT(*) FROM kb_document_ocr_blocks \
                     WHERE document_id = ?1 AND processing_run_id = ?2 GROUP BY label ORDER BY label",
                )?;
                let block_counts: serde_json::Map<String, serde_json::Value> = stmt
                    .query_map(rusqlite::params![doc_id, &doc.processing_run_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })?
                    .filter_map(|row| row.ok())
                    .map(|(label, count)| (label, serde_json::json!(count)))
                    .collect();
                result["ocr_block_counts"] = serde_json::Value::Object(block_counts);

                // 从文档 metadata 中提取 OCR 检测原因（ocr_reasons_by_page）
                let reasons_by_page: serde_json::Value = conn
                    .query_row(
                        "SELECT json_extract(COALESCE(metadata_json, '{}'), '$.ocr_reasons_by_page') \
                         FROM kb_documents WHERE id = ?1",
                        rusqlite::params![doc_id],
                        |row| {
                            let val: String = row.get(0)?;
                            Ok(serde_json::from_str(&val).unwrap_or(serde_json::Value::Null))
                        },
                    )
                    .unwrap_or(serde_json::Value::Null);
                if !reasons_by_page.is_null() {
                    result["ocr_reasons_by_page"] = reasons_by_page;
                }

                Ok(result)
            }

            // ========================================================================
            // kb_ocr_run: 手动触发或重新触发文档 OCR
            // ========================================================================
            "kb_ocr_run" => {
                let doc_id = arguments["document_id"].as_i64().unwrap_or(0);
                if doc_id <= 0 {
                    return Err(crate::error::GBrainError::InvalidInput(
                        "document_id 必须为正整数".to_string(),
                    ));
                }
                let pages_str = arguments["pages"].as_str().map(|s| s.to_string());
                let conn = self.engine.connection()?;

                // 查询文档信息
                let doc_row: (String, String, i64, String, String) = conn
                    .query_row(
                        "SELECT title, storage_path, library_id, processing_run_id, extension \
                         FROM kb_documents WHERE id = ?1",
                        rusqlite::params![doc_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                            ))
                        },
                    )
                    .map_err(|e| {
                        crate::error::GBrainError::Database(format!("查询文档失败: {}", e))
                    })?;

                let (title, storage_path, library_id, run_id, extension) = doc_row;
                let ext_lower = extension.to_lowercase();
                let is_image = crate::artifact::types::is_ocr_image_file(&ext_lower);

                // OCR 只针对 PDF/图片文档；DOCX 等富文本文档没有页级 OCR 语义，
                // 其内嵌图片 OCR 由解析管线自动执行。此前这里对任意非图片文档
                // 硬调 count_pdf_pages，DOCX 会报误导性的
                // "PDF load failed: Invalid file header"。
                if !is_image && ext_lower != "pdf" {
                    return Err(crate::error::GBrainError::InvalidInput(format!(
                        "kb_ocr_run 仅支持 PDF 与图片文档（当前扩展名: {}）；\
                         DOCX/XLSX 等文档的内嵌图片 OCR 会在解析管线中自动执行，\
                         如需重新解析请使用 artifact_reprocess",
                        extension
                    )));
                }

                // 外部 OCR 始终允许，脱敏已关闭 — 不再检查库级策略

                let mut cached_pdf_data: Option<Vec<u8>> = None;
                let total_pages = if is_image {
                    // 图片文档固定 1 页，不需要 PDF 解析器
                    1
                } else {
                    let pdf_data = std::fs::read(&storage_path).map_err(|e| {
                        crate::error::GBrainError::FileError(format!(
                            "读取 PDF 文件以确定页数失败: {}",
                            e
                        ))
                    })?;
                    let total = crate::kb::parser::pdf::count_pdf_pages(&pdf_data)? as i32;
                    if total <= 0 {
                        return Err(crate::error::GBrainError::InvalidInput(
                            "无法确定 PDF 总页数".to_string(),
                        ));
                    }
                    // 缓存给下方自动检测分支复用：同一 PDF 不再第二次全量读入
                    // （大文档内存峰值减半）。
                    cached_pdf_data = Some(pdf_data);
                    total
                };

                let config = self.config.clone();

                // 解析页码范围或自动检测：区分显式指定与自动检测
                let (ocr_pages, _explicit_pages, detection_reasons) = if is_image {
                    if let Some(ref ps) = pages_str {
                        // 图片文档：用户显式传入页码时，仍校验（max_page=1，非法页码会报错）
                        let parsed_pages = parse_mcp_page_ranges(ps, 1)?;
                        let reasons: std::collections::BTreeMap<String, Vec<String>> = parsed_pages
                            .iter()
                            .map(|p| (p.to_string(), vec!["image_input".to_string()]))
                            .collect();
                        (parsed_pages, true, reasons)
                    } else {
                        // 图片文档未指定页码：默认第 1 页
                        let reasons: std::collections::BTreeMap<String, Vec<String>> =
                            std::iter::once(("1".to_string(), vec!["image_input".to_string()]))
                                .collect();
                        (vec![1], true, reasons)
                    }
                } else if let Some(ref ps) = pages_str {
                    let parsed_pages = parse_mcp_page_ranges(ps, total_pages)?;
                    // 显式指定：原因统一为 manual_requested
                    let reasons: std::collections::BTreeMap<String, Vec<String>> = parsed_pages
                        .iter()
                        .map(|p| (p.to_string(), vec!["manual_requested".to_string()]))
                        .collect();
                    (parsed_pages, true, reasons)
                } else {
                    // 自动检测：使用 detector 返回的真实原因（复用页数阶段的字节）
                    let pdf_data = match cached_pdf_data.take() {
                        Some(data) => data,
                        None => std::fs::read(&storage_path).map_err(|e| {
                            crate::error::GBrainError::FileError(format!(
                                "读取 PDF 文件以自动检测 OCR 页失败: {}",
                                e
                            ))
                        })?,
                    };
                    let registry = crate::kb::parser::ParserRegistry::new();
                    let parsed = registry.parse("pdf", &pdf_data)?;
                    let page_analyses = pdf_page_analyses_from_metadata(&parsed)?;
                    let ocr_mode = crate::kb::ocr_provider::OcrMode::from_str(&config.ocr_mode);
                    let detection = crate::kb::ocr_detector::detect_ocr_pages(
                        &page_analyses,
                        config.ocr_text_density_threshold,
                        config.ocr_image_area_threshold,
                        config.ocr_image_count_threshold,
                        &ocr_mode,
                    );
                    let reasons: std::collections::BTreeMap<String, Vec<String>> = detection
                        .reasons_by_page
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.to_string(),
                                v.iter()
                                    .map(|r| {
                                        serde_json::to_string(r)
                                            .unwrap_or_default()
                                            .trim_matches('"')
                                            .to_string()
                                    })
                                    .collect(),
                            )
                        })
                        .collect();
                    (detection.ocr_pages, false, reasons)
                };

                if ocr_pages.is_empty() {
                    // 持久化本次 none 检测结论，防止状态接口展示上一次运行的选择结果
                    conn.execute(
                        "UPDATE kb_documents SET \
                         metadata_json = json_set(COALESCE(metadata_json, '{}'), \
                         '$.needs_ocr_pages', json('[]'), \
                         '$.ocr_reasons_by_page', json('{}'), \
                         '$.ocr_scope', 'none'), \
                         updated_at = datetime('now') \
                         WHERE id = ?1 AND processing_run_id = ?2",
                        rusqlite::params![doc_id, run_id],
                    )?;
                    return Ok(serde_json::json!({
                        "enqueued": false,
                        "document_id": doc_id,
                        "title": title,
                        "message": "自动检测未发现需要 OCR 的页面",
                    }));
                }

                if !config.ocr_enabled {
                    return Err(crate::error::GBrainError::InvalidInput(
                        "全局 OCR 已关闭".to_string(),
                    ));
                }

                // 检查 OCR API key，与 worker/retry 路径一致
                if config.ocr_api_key.is_none() {
                    return Err(crate::error::GBrainError::InvalidInput(
                        "未配置 OCR API key (GBRAIN_OCR_API_KEY)".to_string(),
                    ));
                }

                let payload = crate::kb::jobs::KbOcrPayload {
                    kind: "kb_ocr_document".to_string(),
                    document_id: doc_id,
                    library_id,
                    processing_run_id: run_id.clone(),
                    storage_path,
                    pages: ocr_pages.clone(),
                    submit_mode: config.ocr_submit_mode.clone(),
                    provider: "glm_ocr".to_string(),
                    model: config.ocr_model.clone(),
                    return_crop_images: config.ocr_return_crop_images,
                    need_layout_visualization: config.ocr_need_layout_visualization,
                };

                let conn = self.engine.connection()?;
                let tx = conn.unchecked_transaction()?;
                let enqueue_result = (|| -> Result<i64> {
                    let rows = conn.execute(
                        "UPDATE kb_documents SET ocr_status = 'queued', ocr_text_coverage = 0.0, \
                         document_status = CASE WHEN ?3 THEN 'ocr_pending' ELSE document_status END, \
                         index_status = CASE WHEN ?3 THEN 'pending' ELSE index_status END, \
                         parsing_error = CASE WHEN ?3 THEN '' ELSE parsing_error END, \
                         parsing_status = CASE WHEN ?3 THEN 1 ELSE parsing_status END, \
                         parsing_progress = CASE WHEN ?3 THEN 0 ELSE parsing_progress END, \
                         updated_at = datetime('now') WHERE id = ?1 AND processing_run_id = ?2",
                        rusqlite::params![doc_id, run_id, is_image],
                    )?;
                    if rows == 0 {
                        return Err(crate::error::GBrainError::InvalidInput(
                            "文档 processing_run_id 已变化，跳过 OCR 入队".to_string(),
                        ));
                    }

                    // 同步更新 OCR 检测元数据：区分显式手动选页与自动检测，
                    // ocr_scope 依据选中页数与总页数计算为 none/partial/full。
                    let needs_ocr_pages_str =
                        serde_json::to_string(&ocr_pages).unwrap_or_else(|_| "[]".to_string());
                    let reasons_str = serde_json::to_string(&detection_reasons)
                        .unwrap_or_else(|_| "{}".to_string());
                    let ocr_scope = if ocr_pages.is_empty() {
                        "none"
                    } else if ocr_pages.len() as i32 >= total_pages {
                        "full"
                    } else {
                        "partial"
                    };
                    conn.execute(
                        "UPDATE kb_documents SET \
                         metadata_json = json_set(COALESCE(metadata_json, '{}'), \
                         '$.needs_ocr_pages', json(?1), \
                         '$.ocr_reasons_by_page', json(?2), \
                         '$.ocr_scope', ?3), \
                         updated_at = datetime('now') \
                         WHERE id = ?4 AND processing_run_id = ?5",
                        rusqlite::params![
                            needs_ocr_pages_str,
                            reasons_str,
                            ocr_scope,
                            doc_id,
                            run_id
                        ],
                    )?;

                    // 先为目标页创建 pending 状态记录，再入队，避免 worker 完成后被
                    // delayed pending 初始化用 INSERT OR REPLACE 覆盖 OCR 结果。
                    for &page_num in &ocr_pages {
                        crate::kb::ocr::update_ocr_page_status(
                            conn,
                            doc_id,
                            page_num,
                            "pending",
                            "MCP 手动触发 OCR，等待处理",
                            "glm_ocr",
                            &config.ocr_model,
                            &run_id,
                        )?;
                    }

                    let queue = crate::jobs::JobQueue::new(conn);
                    queue.enqueue(crate::jobs::JobInput {
                        job_type: "kb_ocr_document".to_string(),
                        payload: serde_json::to_value(&payload)?,
                        priority: Some(0),
                        max_attempts: Some(3),
                    })
                })();
                let job_row_id = match enqueue_result {
                    Ok(id) => {
                        tx.commit()?;
                        id
                    }
                    Err(e) => {
                        let _ = tx.rollback();
                        return Err(e);
                    }
                };

                Ok(serde_json::json!({
                    "enqueued": true,
                    "document_id": doc_id,
                    "title": title,
                    "ocr_pages": ocr_pages,
                    "job_row_id": job_row_id,
                }))
            }

            // ========================================================================
            // kb_ocr_retry: 重试文档中失败的 OCR 页
            // ========================================================================
            "kb_ocr_retry" => {
                let doc_id = arguments["document_id"].as_i64().unwrap_or(0);
                if doc_id <= 0 {
                    return Err(crate::error::GBrainError::InvalidInput(
                        "document_id 必须为正整数".to_string(),
                    ));
                }
                let pages_str = arguments["pages"].as_str().map(|s| s.to_string());
                let conn = self.engine.connection()?;

                let doc_row: (String, i64, String, i32, String, String) = conn
                    .query_row(
                        "SELECT title, library_id, processing_run_id, page_count, storage_path, extension \
                         FROM kb_documents WHERE id = ?1",
                        rusqlite::params![doc_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, i32>(3)?,
                                row.get::<_, String>(4)?,
                                row.get::<_, String>(5)?,
                            ))
                        },
                    )
                    .map_err(|e| {
                        crate::error::GBrainError::Database(format!("查询文档失败: {}", e))
                    })?;

                let (title, library_id, run_id, page_count, storage_path, extension) = doc_row;
                let ext_lower = extension.to_lowercase();
                let is_image = crate::artifact::types::is_ocr_image_file(&ext_lower);

                // 与 kb_ocr_run 一致：只支持 PDF/图片文档，避免对 DOCX 等
                // 富文本格式硬调 count_pdf_pages 报误导性错误。
                if !is_image && ext_lower != "pdf" {
                    return Err(crate::error::GBrainError::InvalidInput(format!(
                        "kb_ocr_retry 仅支持 PDF 与图片文档（当前扩展名: {}）；\
                         DOCX/XLSX 等文档的内嵌图片 OCR 会在解析管线中自动执行，\
                         如需重新解析请使用 artifact_reprocess",
                        extension
                    )));
                }

                // 外部 OCR 始终允许，脱敏已关闭 — 不再检查库级策略

                // 全局 OCR 开关和 API key 检查（与 kb_ocr_run 保持一致）
                let config = self.config.clone();
                if !config.ocr_enabled {
                    return Err(crate::error::GBrainError::InvalidInput(
                        "全局 OCR 已关闭".to_string(),
                    ));
                }
                if config.ocr_api_key.is_none() {
                    return Err(crate::error::GBrainError::InvalidInput(
                        "未配置 OCR API key (GBRAIN_OCR_API_KEY)".to_string(),
                    ));
                }

                // 查询需要重试的页（failed、empty_ocr 或 needed）
                // 支持页码范围格式（如 "1-3,5,7-10"），与 kb_ocr_run 保持一致
                let retry_pages = if let Some(ref ps) = pages_str {
                    let retry_page_limit = if is_image {
                        // 图片文档固定 1 页
                        1
                    } else if page_count > 0 {
                        page_count
                    } else {
                        let data = std::fs::read(&storage_path).map_err(|e| {
                            crate::error::GBrainError::FileError(format!(
                                "读取 PDF 文件以确定页数失败: {}",
                                e
                            ))
                        })?;
                        let total = crate::kb::parser::pdf::count_pdf_pages(&data)? as i32;
                        if total <= 0 {
                            return Err(crate::error::GBrainError::InvalidInput(
                                "无法确定 PDF 总页数".to_string(),
                            ));
                        }
                        total
                    };
                    let specified = parse_mcp_page_ranges(ps, retry_page_limit)?;

                    // 仅选其中 failed/empty_ocr 的页
                    let conn2 = self.engine.connection()?;
                    let mut retry = Vec::new();
                    for p in &specified {
                        let status: String = conn2
                            .query_row(
                                "SELECT status FROM kb_document_ocr_pages \
                                 WHERE document_id = ?1 AND page_number = ?2 AND processing_run_id = ?3",
                                rusqlite::params![doc_id, p, run_id],
                                |row| row.get(0),
                            )
                            .unwrap_or_default();
                        if status == "failed" || status == "empty_ocr" || status == "needed" {
                            retry.push(*p);
                        }
                    }
                    retry
                } else {
                    // 重试所有 failed/empty_ocr 页
                    let conn2 = self.engine.connection()?;
                    let mut failed = Vec::new();
                    let mut stmt = conn2.prepare(
                        "SELECT page_number FROM kb_document_ocr_pages \
                         WHERE document_id = ?1 AND processing_run_id = ?2 AND status IN ('failed', 'empty_ocr', 'needed') ORDER BY page_number",
                    )?;
                    let rows = stmt.query_map(rusqlite::params![doc_id, run_id], |row| {
                        row.get::<_, i32>(0)
                    })?;
                    for p in rows.flatten() {
                        failed.push(p);
                    }
                    failed
                };

                if retry_pages.is_empty() {
                    return Ok(serde_json::json!({
                        "enqueued": false,
                        "document_id": doc_id,
                        "title": title,
                        "message": "无需重试的 OCR 页",
                    }));
                }

                // 仅重置 retry_pages 中失败/empty_ocr 的页状态为 pending
                let conn = self.engine.connection()?;
                // 构建页码占位符列表
                let placeholders: Vec<String> = retry_pages
                    .iter()
                    .enumerate()
                    .map(|(i, _)| format!("?{}", i + 3))
                    .collect();
                let sql = format!(
                    "UPDATE kb_document_ocr_pages SET status = 'pending', error = '' \
                     WHERE document_id = ?1 AND processing_run_id = ?2 \
                     AND page_number IN ({}) AND status IN ('failed', 'empty_ocr', 'needed')",
                    placeholders.join(",")
                );
                let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> =
                    vec![Box::new(doc_id), Box::new(run_id.clone())];
                for &p in &retry_pages {
                    params_vec.push(Box::new(p));
                }
                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params_vec.iter().map(|p| p.as_ref()).collect();

                let payload = crate::kb::jobs::KbOcrPayload {
                    kind: "kb_ocr_document".to_string(),
                    document_id: doc_id,
                    library_id,
                    processing_run_id: run_id.clone(),
                    storage_path,
                    pages: retry_pages.clone(),
                    submit_mode: config.ocr_submit_mode.clone(),
                    provider: "glm_ocr".to_string(),
                    model: config.ocr_model.clone(),
                    return_crop_images: config.ocr_return_crop_images,
                    need_layout_visualization: config.ocr_need_layout_visualization,
                };

                let tx = conn.unchecked_transaction()?;
                let enqueue_result = (|| -> Result<i64> {
                    conn.execute(&sql, params_refs.as_slice())?;

                    let rows = conn.execute(
                        "UPDATE kb_documents SET ocr_status = 'queued', ocr_text_coverage = 0.0, \
                         document_status = CASE WHEN ?3 THEN 'ocr_pending' ELSE document_status END, \
                         index_status = CASE WHEN ?3 THEN 'pending' ELSE index_status END, \
                         parsing_error = CASE WHEN ?3 THEN '' ELSE parsing_error END, \
                         parsing_status = CASE WHEN ?3 THEN 1 ELSE parsing_status END, \
                         parsing_progress = CASE WHEN ?3 THEN 0 ELSE parsing_progress END, \
                         updated_at = datetime('now') WHERE id = ?1 AND processing_run_id = ?2",
                        rusqlite::params![doc_id, run_id, is_image],
                    )?;
                    if rows == 0 {
                        return Err(crate::error::GBrainError::InvalidInput(
                            "文档 processing_run_id 已变化，跳过 OCR 重试入队".to_string(),
                        ));
                    }

                    let queue = crate::jobs::JobQueue::new(conn);
                    queue.enqueue(crate::jobs::JobInput {
                        job_type: "kb_ocr_document".to_string(),
                        payload: serde_json::to_value(&payload)?,
                        priority: Some(0),
                        max_attempts: Some(3),
                    })
                })();
                let job_row_id = match enqueue_result {
                    Ok(id) => {
                        tx.commit()?;
                        id
                    }
                    Err(e) => {
                        let _ = tx.rollback();
                        return Err(e);
                    }
                };

                Ok(serde_json::json!({
                    "enqueued": true,
                    "document_id": doc_id,
                    "title": title,
                    "retry_pages": retry_pages,
                    "job_row_id": job_row_id,
                }))
            }

            _ => Err(crate::error::GBrainError::InvalidInput(format!(
                "Unknown tool: {}",
                tool_name
            ))),
        }
    }
}
