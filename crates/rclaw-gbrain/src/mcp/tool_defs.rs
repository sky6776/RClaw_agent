//! MCP 工具定义 — artifact 统一对外接口 + OCR 扩展工具
//!
//! 暴露 artifact_* 工具（知识操作统一入口）和 kb_ocr_* 工具（OCR 扩展）。
//!
//! 设计文档 §8.2: MCP tools 收口

use crate::operations::{OperationDef, ParamDef, ParamType};
use serde_json::Value;

/// Tool definition (MCP-compatible)
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl From<&OperationDef> for ToolDef {
    fn from(op: &OperationDef) -> Self {
        ToolDef {
            name: op.name.to_string(),
            description: op.description.to_string(),
            input_schema: op.to_mcp_schema(),
        }
    }
}

/// Artifact facade 工具定义 — 统一知识操作入口（设计文档 §4.2）
///
/// 默认暴露给用户，包含所有 artifact_* 命名空间的 MCP tools。
pub(crate) static ARTIFACT_FACADE_DEFS: &[OperationDef] = &[
    OperationDef {
        name: "artifact_put",
        description: "手动写入长期记忆。创建 text/manual artifact 并投影到 gbrain 页面。支持 --content 直接输入或 --file 从文件读取。",
        params: &[
            ParamDef { name: "slug", description: "目标页面 slug（如 people/alice）", required: true, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "content", description: "直接输入的文本内容（与 file 二选一）", required: false, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "file", description: "本地文本文件路径（与 content 二选一，仅支持 txt/md/csv/json/yaml 等纯文本格式，上限 1MB）", required: false, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "title", description: "页面标题（可选，默认从 slug 推断）", required: false, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "intent", description: "意图: memory(默认, 稳定brain页面+可选KB+低风险自动应用), evidence(仅KB证据), promote(影子页面+KB+建议变更)", required: false, param_type: ParamType::String, enum_values: Some(&["memory", "evidence", "promote"]), items_type: None },
            ParamDef { name: "dry_run", description: "仅返回路由计划，不实际写入", required: false, param_type: ParamType::Boolean, enum_values: None, items_type: None },
            ParamDef { name: "force", description: "强制覆盖已被人工修改的页面（默认不覆盖，返回冲突信息）", required: false, param_type: ParamType::Boolean, enum_values: None, items_type: None },
        ],
    },

    OperationDef {
        name: "artifact_upload",
        description: "上传文件作为知识源。系统根据文件类型和意图自动路由：文档进KB+影子页，图片/二进制走附件，带--target生成建议变更。",
        params: &[
            ParamDef { name: "path", description: "本地文件路径", required: true, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "intent", description: "上传意图: auto(自动路由), evidence(文档证据，别名 document), memory(整理进记忆), attachment(仅附件), promote(明确提升)", required: false, param_type: ParamType::String, enum_values: Some(&["auto", "evidence", "document", "memory", "attachment", "promote"]), items_type: None },
            ParamDef { name: "target_slug", description: "目标 gbrain 页面 slug（用于生成建议变更）", required: false, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "page_slug", description: "关联页面 slug（用于附件）", required: false, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "library_id", description: "KB 库 ID（可选，默认自动选择 Inbox）", required: false, param_type: ParamType::Integer, enum_values: None, items_type: None },
            ParamDef { name: "folder_id", description: "KB 文件夹 ID", required: false, param_type: ParamType::Integer, enum_values: None, items_type: None },
            ParamDef { name: "promotion", description: "提升策略: none, shadow, candidate, auto-low-risk, auto-apply", required: false, param_type: ParamType::String, enum_values: Some(&["none", "shadow", "candidate", "auto", "auto-low-risk", "auto_accept_low_risk", "auto-apply", "auto_apply", "auto_all", "auto-all", "auto-apply-all"]), items_type: None },
            ParamDef { name: "dry_run", description: "仅返回路由计划，不实际写入", required: false, param_type: ParamType::Boolean, enum_values: None, items_type: None },
        ],
    },

    OperationDef {
        name: "artifact_query",
        description: "统一知识查询。自动混合 gbrain 长期记忆、KB 文档证据和时间线事件。返回聚焦证据、命中词和 fallback meta；优先用命中片段问答，不要仅为“更完整”而读取全文。默认隐藏内部 ID。",
        params: &[
            ParamDef { name: "query", description: "查询文本", required: true, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "mode", description: "查询模式: auto(自动), memory(优先记忆), evidence(优先证据), timeline(优先时间线)", required: false, param_type: ParamType::String, enum_values: Some(&["auto", "memory", "evidence", "timeline"]), items_type: None },
            ParamDef { name: "limit", description: "最大结果数", required: false, param_type: ParamType::Integer, enum_values: None, items_type: None },
            ParamDef { name: "filter_slug", description: "过滤到指定页面 slug", required: false, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "include_sources", description: "显示来源追溯（artifact 来源和引用）", required: false, param_type: ParamType::Boolean, enum_values: None, items_type: None },
        ],
    },

    OperationDef {
        name: "artifact_list",
        description: "列出知识源（source artifacts）",
        params: &[
            ParamDef { name: "limit", description: "最大结果数（默认 50）", required: false, param_type: ParamType::Integer, enum_values: None, items_type: None },
            ParamDef { name: "offset", description: "偏移量", required: false, param_type: ParamType::Integer, enum_values: None, items_type: None },
        ],
    },

    OperationDef {
        name: "artifact_get",
        description: "获取知识源详情。支持 ID 或 UID 查询，可选展示投影、来源追溯，以及按 query 聚焦读取片段。",
        params: &[
            ParamDef { name: "id_or_uid", description: "Artifact ID 或 UID（如 '1' 或 'art_ab12cd34ef56'）", required: true, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "include_projections", description: "包含投影详情", required: false, param_type: ParamType::Boolean, enum_values: None, items_type: None },
            ParamDef { name: "include_sources", description: "包含来源追溯", required: false, param_type: ParamType::Boolean, enum_values: None, items_type: None },
            ParamDef { name: "include_content", description: "Include full content. Prefer artifact_query excerpts for Q&A; use this only when focused search results are insufficient or the user explicitly needs the whole artifact.", required: false, param_type: ParamType::Boolean, enum_values: None, items_type: None },
            ParamDef { name: "content_mode", description: "正文读取模式: focused(按 query/passage 取片段) 或 full(全文；需 include_content=true)", required: false, param_type: ParamType::String, enum_values: Some(&["focused", "full"]), items_type: None },
            ParamDef { name: "content_query", description: "focused 模式的查询词；提供后只返回相关 KB passage，不读取整篇 artifact", required: false, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "max_chars", description: "focused 内容最大字符数（默认 1600）", required: false, param_type: ParamType::Integer, enum_values: None, items_type: None },
            ParamDef { name: "passage_id", description: "直接读取 artifact_query 返回的 KB passage ID", required: false, param_type: ParamType::Integer, enum_values: None, items_type: None },
        ],
    },

    OperationDef {
        name: "artifact_delete",
        description: "软删除知识源。KB 证据和影子页随生命周期 stale，稳定 gbrain 页面内容不默认删除。支持 dry_run 预览。",
        params: &[
            ParamDef { name: "id_or_uid", description: "Artifact ID 或 UID", required: true, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "dry_run", description: "预览删除影响，不实际执行", required: false, param_type: ParamType::Boolean, enum_values: None, items_type: None },
        ],
    },

    OperationDef {
        name: "artifact_detach",
        description: "移除知识源与某次使用的关联（occurrence/projection 级），不删除 source artifact 本身，不影响其它页面使用。",
        params: &[
            ParamDef { name: "id_or_uid", description: "Artifact ID 或 UID", required: true, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "from", description: "目标页面 slug", required: true, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "dry_run", description: "预览影响，不实际执行", required: false, param_type: ParamType::Boolean, enum_values: None, items_type: None },
        ],
    },

    OperationDef {
        name: "artifact_restore",
        description: "恢复已软删除的知识源及其可恢复投影",
        params: &[
            ParamDef { name: "id_or_uid", description: "Artifact ID 或 UID", required: true, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "dry_run", description: "预览恢复影响，不实际执行", required: false, param_type: ParamType::Boolean, enum_values: None, items_type: None },
        ],
    },

    OperationDef {
        name: "artifact_reprocess",
        description: "重新处理知识源：重跑 KB pipeline、投影生成和 promotion 提取",
        params: &[
            ParamDef { name: "id_or_uid", description: "Artifact ID 或 UID", required: true, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "dry_run", description: "预览重新处理影响，不实际执行", required: false, param_type: ParamType::Boolean, enum_values: None, items_type: None },
        ],
    },

    OperationDef {
        name: "artifact_health",
        description: "检查知识源一致性：孤立投影、stale 投影、待审核变更、来源追溯状态",
        params: &[],
    },

    OperationDef {
        name: "artifact_review_list",
        description: "列出建议变更（suggested changes）。对用户展示为来源证据驱动的变更建议，而非内部 promotion candidate。",
        params: &[
            ParamDef { name: "status", description: "过滤状态: pending, accepted, rejected, applied, rolled_back", required: false, param_type: ParamType::String, enum_values: Some(&["pending", "accepted", "rejected", "applied", "rolled_back"]), items_type: None },
            ParamDef { name: "target_slug", description: "过滤目标页面 slug", required: false, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "limit", description: "最大结果数", required: false, param_type: ParamType::Integer, enum_values: None, items_type: None },
        ],
    },

    OperationDef {
        name: "artifact_review_get",
        description: "获取建议变更详情，包含证据、目标记忆、风险等级和来源追溯",
        params: &[
            ParamDef { name: "change_id", description: "变更 ID", required: true, param_type: ParamType::Integer, enum_values: None, items_type: None },
        ],
    },

    OperationDef {
        name: "artifact_review_apply",
        description: "应用建议变更到 gbrain 长期记忆",
        params: &[
            ParamDef { name: "change_id", description: "变更 ID", required: true, param_type: ParamType::Integer, enum_values: None, items_type: None },
        ],
    },

    OperationDef {
        name: "artifact_review_reject",
        description: "拒绝建议变更",
        params: &[
            ParamDef { name: "change_id", description: "变更 ID", required: true, param_type: ParamType::Integer, enum_values: None, items_type: None },
            ParamDef { name: "reason", description: "拒绝原因", required: false, param_type: ParamType::String, enum_values: None, items_type: None },
            ParamDef { name: "reviewer", description: "审核者标识，默认值为 mcp", required: false, param_type: ParamType::String, enum_values: None, items_type: None },
        ],
    },

    OperationDef {
        name: "artifact_review_rollback",
        description: "回滚已应用的建议变更，撤销影子页更新并标记来源追溯为 stale",
        params: &[
            ParamDef { name: "change_id", description: "变更 ID", required: true, param_type: ParamType::Integer, enum_values: None, items_type: None },
        ],
    },
];

/// OCR 扩展工具定义 — PDF 文档 OCR 状态查询与手动触发
///
/// 设计文档 OCR MCP 扩展：kb_document_status / kb_ocr_run / kb_ocr_retry
pub(crate) static OCR_TOOL_DEFS: &[OperationDef] = &[
    OperationDef {
        name: "kb_document_status",
        description: "查询 KB 文档处理状态，包括 OCR 状态、页级 OCR 进度和处理错误。返回文档级和页级状态信息。",
        params: &[
            ParamDef { name: "document_id", description: "KB 文档 ID", required: true, param_type: ParamType::Integer, enum_values: None, items_type: None },
        ],
    },
    OperationDef {
        name: "kb_ocr_run",
        description: "手动触发或重新触发文档的 OCR 处理。可选择指定页码范围，不指定则自动检测需要 OCR 的页。仅支持 PDF 与图片文档（png/jpg/jpeg）；DOCX/XLSX 等文档的内嵌图片 OCR 会在解析管线中自动执行，如需重新解析请改用 artifact_reprocess。",
        params: &[
            ParamDef { name: "document_id", description: "KB 文档 ID", required: true, param_type: ParamType::Integer, enum_values: None, items_type: None },
            ParamDef { name: "pages", description: "页码范围（如 '1-3,5,7-10'），不指定则自动检测", required: false, param_type: ParamType::String, enum_values: None, items_type: None },
        ],
    },
    OperationDef {
        name: "kb_ocr_retry",
        description: "重试文档中失败或空的 OCR 页。仅重试状态为 failed/empty_ocr/needed 的页。仅支持 PDF 与图片文档（png/jpg/jpeg）；DOCX/XLSX 等文档请改用 artifact_reprocess。",
        params: &[
            ParamDef { name: "document_id", description: "KB 文档 ID", required: true, param_type: ParamType::Integer, enum_values: None, items_type: None },
            ParamDef { name: "pages", description: "指定重试的页码范围（如 '1-3,5'），不指定则重试所有失败页", required: false, param_type: ParamType::String, enum_values: None, items_type: None },
        ],
    },
];

/// 构建工具定义列表（artifact + OCR 扩展）
pub fn build_tool_defs() -> Vec<ToolDef> {
    ARTIFACT_FACADE_DEFS
        .iter()
        .chain(OCR_TOOL_DEFS.iter())
        .map(|op| op.into())
        .collect()
}

/// 获取操作定义（artifact + OCR 扩展）
pub fn get_operation_def(name: &str) -> Option<&'static OperationDef> {
    ARTIFACT_FACADE_DEFS
        .iter()
        .chain(OCR_TOOL_DEFS.iter())
        .find(|op| op.name == name)
}

/// 获取所有操作定义（artifact + OCR 扩展）
pub fn get_operation_defs() -> Vec<&'static OperationDef> {
    ARTIFACT_FACADE_DEFS
        .iter()
        .chain(OCR_TOOL_DEFS.iter())
        .collect()
}

/// 获取 artifact facade 操作定义
pub fn get_artifact_facade_defs() -> &'static [OperationDef] {
    ARTIFACT_FACADE_DEFS
}
