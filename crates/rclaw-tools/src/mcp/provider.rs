//! 把单个 MCP 工具适配为 RClaw 的 [`Tool`]。
//!
//! 暴露名采用 `<server>__<tool>`，与内置工具（read_file 等）并列注册进
//! [`crate::ToolRegistry`]。多个 wrapper 共享同一个 `Arc<McpClient>`（同一 server
//! 的所有工具复用同一条子进程连接）。
//!
//! **命名净化与消歧（P2）**：暴露名会经由 `openai_compat::convert_tools()` 原样进入
//! OpenAI 兼容请求的 `function.name`，该字段要求 `^[a-zA-Z0-9_-]{1,64}$`。配置的
//! server 名与 server 上报的 tool 名可能含 `.`、`/`、空格，server 名自身可能含 `__`
//! （会让 [`crate::mcp::manager::McpToolTarget::parse`] 错位），也可能过长。因此：
//! - [`build_exposed_name`] 先把两段净化为 OpenAI 合法、且段内不含 `__` 的形式；
//! - 若净化或截断**改变了**原名（即原始 `<server>__<tool>` 不直接可用），附加 6 位
//!   hex 哈希后缀（对原始 `server\0tool` 取 FNV-1a）做消歧——不同原始对哈希不同，避免
//!   撞名导致 [`crate::ToolRegistry`] 覆盖、丢失工具；
//! - 调用 server 时仍用**原始** tool 名（保存在 [`McpToolWrapper::mcp_tool_name`]），
//!   保证回传正确。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tracing::{info, warn};

use super::client::{McpTool, McpToolResult};
use super::manager::McpManager;
use super::summarize_json;
use crate::{Tool, ToolResult};

/// OpenAI 兼容 `function.name` 的长度上限（字符集为 `^[a-zA-Z0-9_-]{1,64}$`）。
const MAX_FUNCTION_NAME_LEN: usize = 64;

/// 把单个名字段（server 名或 tool 名）净化为 OpenAI `function.name` 合法的形式。
///
/// 规则：
/// - 仅保留 ASCII 字母数字与 `_`、`-`，其余字符替换为 `_`；
/// - 把连续 2 个及以上的 `_` 折叠为单个 `_`——保证段内不含 `__`，使 `<server>__<tool>`
///   的分隔符唯一，[`crate::mcp::manager::McpToolTarget::parse`] 不会因 server 名含 `__`
///   而错位；
/// - 净化后为空（如原值全是非法字符）则回退为 `_`。
///
/// 净化结果恒为纯 ASCII，故字节长度 == 字符长度，后续按字节截断是安全的。
pub(crate) fn sanitize_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_under = false;
    for c in raw.chars() {
        let emitted = if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            c
        } else {
            '_'
        };
        if emitted == '_' {
            if prev_under {
                continue; // 折叠连续下划线，杜绝段内 "__"
            }
            prev_under = true;
        } else {
            prev_under = false;
        }
        out.push(emitted);
    }
    if out.is_empty() {
        "_".to_string()
    } else {
        out
    }
}

/// 轻量确定性哈希（FNV-1a 32-bit），仅用于暴露名消歧，**非**密码学用途。
fn fnv1a32(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// 由原始 server/tool 名构造对外暴露的工具名，确保 OpenAI 合法、长度不超限、且不同
/// 原始对不撞名。
///
/// - 若原始 `<server>__<tool>` 本就合法且不超长（常见情况），直接使用，**无**后缀；
/// - 否则（净化或截断改变了名字）附加 6 位 hex 哈希后缀 `_xxxxxx`（对原始
///   `server + '\0' + tool` 取 FNV-1a），使不同原始对在净化/截断后仍可区分，避免
///   [`crate::ToolRegistry`] 因重名覆盖而丢失工具。后缀长度计入 64 字符预算。
///
/// 同一原始对恒产生同一暴露名（FNV-1a 确定性），保证「LLM 看到的名 == 回传的名」。
pub(crate) fn build_exposed_name(server: &str, tool: &str) -> String {
    let server_san = sanitize_segment(server);
    let tool_san = sanitize_segment(tool);
    let raw_total = server_san.len() + 2 + tool_san.len();

    if server_san == server && tool_san == tool && raw_total <= MAX_FUNCTION_NAME_LEN {
        // 原名即合法且不超长：直接用，无需净化/截断/消歧。
        return format!("{server_san}__{tool_san}");
    }

    // 需要净化或截断：附加确定性哈希后缀以防与其它原始对撞名。
    let digest = fnv1a32(&format!("{server}\u{0}{tool}"));
    let suffix = format!("_{digest:06x}"); // '_' + 6 hex = 7 字节（均 ASCII）

    // 预算 64：扣除分隔符 "__"(2) 与后缀后，server 段取剩余空间的一半（至少 1），
    // tool 段取余量。两段分别截断。
    let remaining = MAX_FUNCTION_NAME_LEN.saturating_sub(2 + suffix.len());
    let max_server = (remaining / 2).max(1);
    let mut s = server_san;
    if s.len() > max_server {
        s.truncate(max_server);
    }
    let budget_tool = MAX_FUNCTION_NAME_LEN.saturating_sub(s.len() + 2 + suffix.len());
    let mut t = tool_san;
    if t.len() > budget_tool {
        t.truncate(budget_tool);
    }
    format!("{s}__{t}{suffix}")
}

/// 单个 MCP 工具的本地包装。
///
/// 注意：本包装**不**直接持有 `Arc<McpClient>`，而是持有 manager 句柄 + server 名。
/// 这样在 server 进程退出后，[`Tool::execute`] 能经由 [`McpManager::client_for`] 重新解析到
/// （重建的）client，而不是永久绑定一个已经死掉的连接。
pub struct McpToolWrapper {
    /// 对外暴露的净化名（OpenAI 合法、已消歧）。
    schema_name: String,
    /// 工具描述（来自 MCP server）。
    schema_desc: String,
    /// 该工具所属的 server 名（原始、非净化）。
    server_name: String,
    /// 调用 server 时用的原始（非净化）工具名。
    mcp_tool_name: String,
    /// 工具入参 JSON Schema（来自 MCP server，原样透传）。
    input_schema: Value,
    /// 所属 manager 句柄：执行时按需 `client_for` 拿到（可能重建的）client。
    manager: Arc<McpManager>,
}

impl McpToolWrapper {
    pub fn new(server_name: &str, tool: McpTool, manager: Arc<McpManager>) -> Self {
        Self {
            schema_name: build_exposed_name(server_name, &tool.name),
            schema_desc: tool.description.unwrap_or_default(),
            server_name: server_name.to_string(),
            mcp_tool_name: tool.name,
            input_schema: tool.input_schema,
            manager,
        }
    }
}

/// 规范化 MCP 工具入参里的本地文件路径字段。
///
/// RClaw 自己有一套 agent 根目录，例如 `target/debug/agent`；MCP server 又可以通过
/// `[mcp.servers.<name>].cwd` 配置成另一个工作目录。模型看到的文件清单通常来自
/// RClaw 的 `list`，里面可能是 `agent/foo.md` 这样的相对路径。如果原样传给
/// 子进程，子进程会按自己的 cwd 解析，容易变成 `agent/agent/foo.md`。
///
/// 这里在 RClaw 侧把“确实存在的本地路径”转成绝对路径，再交给 MCP server：
/// - 这不是放宽 MCP server 的安全策略，server 仍然会做自己的路径/权限校验。
/// - 只处理常见文件字段 `path` 和 `file`，避免误改业务文本。
/// - 只有能在本机成功 `canonicalize` 的路径才会被替换；解析不到就保持原样，
///   让远端工具返回它自己的错误信息。
fn normalize_mcp_path_arguments(input: Value, server_cwd: Option<&str>) -> Value {
    let mut input = input;
    let Some(obj) = input.as_object_mut() else {
        // 绝大多数 MCP 工具入参是 JSON object。若不是对象，说明不是路径类调用，
        // 直接透传，保持包装层对通用工具的兼容性。
        return input;
    };

    for key in ["path", "file"] {
        let Some(raw) = obj.get(key).and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(normalized) = resolve_existing_path(raw, server_cwd) {
            // 用规范绝对路径替换原入参，后续日志和远端工具看到的都是稳定路径。
            // 例如：agent/a.md -> E:\...\target\debug\agent\a.md。
            obj.insert(key.to_string(), Value::String(normalized));
        }
    }

    input
}

/// 尝试把用户/模型给出的路径解析为本机真实存在的规范绝对路径。
///
/// 候选目录的顺序刻意贴合 RClaw + MCP 的实际运行方式：
/// 1. `server_cwd/raw`：模型若给的是相对于 MCP server cwd 的路径，应优先命中。
/// 2. `server_cwd.parent()/raw`：现场常见配置是 server cwd 指向 `.../agent`，
///    但模型给出 `agent/...`，需要回到父目录后再拼接。
/// 3. `std::env::current_dir()/raw`：兼容从项目根或其它目录直接运行 RClaw 的情况。
/// 4. `current_exe().parent()/raw`：兼容用户双击或从二进制目录运行
///    `target/debug/rclaw.exe` 时，相对路径实际是围绕 exe 所在目录组织的情况。
fn resolve_existing_path(raw: &str, server_cwd: Option<&str>) -> Option<String> {
    // 空路径和 NUL 字节不参与本地解析。NUL 在 Rust Path 层可能表现不稳定，
    // 且远端工具本身也应拒绝它，所以这里保持原样透传更安全。
    if raw.is_empty() || raw.contains('\0') {
        return None;
    }
    // UNC / 网络路径（`\\host\share\..`、`//host/share/..`）不在本进程解析：Windows 上
    // `canonicalize` 会真的去连 445 端口 / WebDAV 并自动带上当前用户的 NTLM 凭据——模型
    // 被注入一个指向攻击者主机的 path 就能把凭据哈希送出去。原样透传给 server 自行处理。
    // 两个前导分隔符任意混用（`/\host\..`、`\/host\..`）在 Windows 和 Rust 的前缀解析里
    // 同样是 UNC，所以按字节判断而不是只看 `\\` / `//` 两种写法。
    let bytes = raw.as_bytes();
    let is_separator = |byte: u8| byte == b'\\' || byte == b'/';
    if bytes.len() >= 2 && is_separator(bytes[0]) && is_separator(bytes[1]) {
        return None;
    }

    let raw_path = Path::new(raw);
    if raw_path.is_absolute() {
        // 已经是绝对路径时，只做 canonicalize 和 Windows 前缀清理。
        // 若文件不存在，返回 None，保留原始字符串给远端工具报错。
        return canonical_path_string(raw_path);
    }

    let mut candidates = Vec::new();
    if let Some(cwd) = non_empty_path(server_cwd) {
        // 候选 1：按 MCP server 自己的 cwd 解析。
        candidates.push(cwd.join(raw_path));
        if let Some(parent) = cwd.parent() {
            // 候选 2：按 MCP server cwd 的父目录解析。
            // 这正是 `cwd = .../agent` 且入参为 `agent/...` 时需要的路径。
            candidates.push(parent.join(raw_path));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        // 候选 3：按 RClaw 当前进程 cwd 解析。
        candidates.push(cwd.join(raw_path));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            // 候选 4：按 rclaw.exe 所在目录解析。
            candidates.push(exe_dir.join(raw_path));
        }
    }

    // 只接受第一个真实存在且可规范化的候选路径。canonicalize 会解析 `.`、
    // 已存在路径中的符号链接，并在目标不存在时失败。
    candidates
        .into_iter()
        .find_map(|candidate| canonical_path_string(&candidate))
}

/// 把可选 cwd 字符串转成 PathBuf，同时过滤空字符串和纯空白配置。
fn non_empty_path(path: Option<&str>) -> Option<PathBuf> {
    path.and_then(|p| (!p.trim().is_empty()).then(|| PathBuf::from(p)))
}

/// 返回规范路径字符串。路径不存在或无权限解析时返回 None。
///
/// Windows 上带网络 / 设备前缀（UNC、`\\?\UNC\`、`\\.\`、`\\?\` 裸前缀）的候选一律不
/// canonicalize：`resolve_existing_path` 已拒绝原始入参里的 UNC 写法，这里是最后一道闸，
/// 防止拼接 / 解析规则变化后仍有网络路径漏到 `canonicalize`（会对外连接并自动送出 NTLM 凭据）。
fn canonical_path_string(path: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if let Some(Component::Prefix(prefix)) = path.components().next() {
            if matches!(
                prefix.kind(),
                Prefix::UNC(..)
                    | Prefix::VerbatimUNC(..)
                    | Prefix::DeviceNS(..)
                    | Prefix::Verbatim(..)
            ) {
                return None;
            }
        }
    }
    let canonical = path.canonicalize().ok()?;
    Some(display_path_string(&canonical))
}

/// 把 Path 转成传给 MCP server 的字符串。
///
/// Windows 的 `canonicalize` 可能返回 `\\?\C:\...` 或 `\\?\UNC\...` 形式。
/// 这类 extended-length 前缀对 Win32 API 有用，但很多 CLI、日志和安全校验更习惯普通
/// `C:\...` / `\\server\share\...` 形式，所以在传参前去掉前缀。
fn display_path_string(path: &Path) -> String {
    let s = path.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return rest.to_string();
        }
    }
    s
}

#[async_trait]
impl Tool for McpToolWrapper {
    fn name(&self) -> &str {
        &self.schema_name
    }
    fn description(&self) -> &str {
        &self.schema_desc
    }
    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(&self, input: Value) -> ToolResult {
        // MCP 工具执行前先做一次“本地路径稳定化”。模型可能从 RClaw 的 list
        // 拿到相对路径，但真正消费路径的是另一个子进程；在边界处转成绝对路径，
        // 可以避免两个进程 cwd 不一致造成的 `path does not exist`。
        let server_cwd = self
            .manager
            .resolved_config()
            .servers
            .get(&self.server_name)
            .and_then(|cfg| cfg.cwd.clone());
        let input = normalize_mcp_path_arguments(input, server_cwd.as_deref());
        info!(
            local = %self.schema_name,
            remote = %self.mcp_tool_name,
            params = %summarize_json(&input, 800),
            "执行 MCP 工具"
        );
        // 通过 manager 解析 client（而非持有固定 client）：server 退出后 manager 会按 is_alive 重建连接。
        let client = match self.manager.client_for(&self.server_name).await {
            Ok(c) => c,
            Err(e) => {
                // 传输错误文案可能带完整 URL（reqwest 的 Display 含 query，里面常有
                // token）：进日志、回给模型前都先脱敏。
                let text = rclaw_store::redact_sensitive_text(&e.to_string());
                warn!(
                    local = %self.schema_name,
                    remote = %self.mcp_tool_name,
                    error = %text,
                    "MCP client 获取失败"
                );
                return ToolResult::err(text);
            }
        };

        // 为「连接中断后重试」预留一份入参：首次调用会 move 掉 input，故先 clone。
        // 代价是每次调用多一次 Value 克隆，相对一次 LLM 工具往返可忽略。
        let result = client.call_tool(&self.mcp_tool_name, input.clone()).await;
        match result {
            Ok(result) => {
                // isError 是工具执行级「正常结果」（非传输错误）：完整渲染 content/structuredContent
                // 后产出 ok/err——错误结果也保留全部内容，而非只取首段文本。
                let rendered = render_tool_result(&result);
                if result.is_error {
                    ToolResult::err(rendered)
                } else {
                    ToolResult::ok(rendered)
                }
            }
            Err(e) => {
                // 同上：错误文案先脱敏再进日志 / 回给模型。
                let e = rclaw_store::redact_sensitive_text(&e.to_string());
                // 仅当 client 已判定死亡（server 退出）时，重建连接并重试一次；
                // 普通失败（工具 isError、超时但连接仍存活）不重试，避免无谓重复。
                if !client.is_alive() {
                    warn!(
                        local = %self.schema_name,
                        remote = %self.mcp_tool_name,
                        error = %e,
                        "MCP 连接中断，重建后重试一次"
                    );
                    if let Ok(client2) = self.manager.client_for(&self.server_name).await {
                        if let Ok(result) = client2.call_tool(&self.mcp_tool_name, input).await {
                            // 与首次调用同一口径：isError 的结果必须以 err 交给模型，
                            // 否则重试路径会把工具失败伪装成成功。
                            let rendered = render_tool_result(&result);
                            return if result.is_error {
                                ToolResult::err(rendered)
                            } else {
                                ToolResult::ok(rendered)
                            };
                        }
                    }
                }
                warn!(
                    local = %self.schema_name,
                    remote = %self.mcp_tool_name,
                    error = %e,
                    "MCP 工具调用失败"
                );
                ToolResult::err(e)
            }
        }
    }
}

/// 把 [`McpToolResult`] 渲染为模型可见的纯文本（[`ToolResult::content`] 为 `String`）。
///
/// 采用**分段拼接**，确保任何一类信息都不会被静默丢弃：
/// 1. **文本段**：content 里所有 `type=text` 块（多段 `\n` 拼接）；
/// 2. **非文本 content 段**：当 content 是数组且含图片/资源等**非文本块**时，把完整 content
///    以 `[content]` JSON 段追加——否则这些块会被 [`extract_text_from_content`] 过滤掉，
///    模型永远看不到；
/// 3. **结构化段**：若 server 返回了 `structuredContent`，以 `[structuredContent]` JSON 段追加。
///
/// 关键修复：旧实现「只要有 text 就只返回 text」，会把 text + image 结果里的图片丢掉。
/// 新实现仅在 content 不含非文本块时才走纯文本快速路径（含 `call_tool` 折叠出的
/// `Value::String`，那必然是全文本）。
pub(crate) fn render_tool_result(result: &McpToolResult) -> String {
    let content = &result.content;
    let text = extract_text_from_content(content);

    // content 是数组且含任何非文本块（type != "text"）→ 需要保留完整 content，避免丢图片/资源。
    let has_non_text = content
        .as_array()
        .map(|arr| {
            arr.iter()
                .any(|item| item.get("type").and_then(|t| t.as_str()) != Some("text"))
        })
        .unwrap_or(false);

    let mut parts: Vec<String> = Vec::new();

    // 1) 文本段（可能为空）。
    if !text.trim().is_empty() {
        parts.push(text);
    }

    // 2) 非文本块段：把完整 content 序列化为 JSON 追加（pretty，便于模型阅读）。
    if has_non_text {
        let json = serde_json::to_string_pretty(content).unwrap_or_else(|_| content.to_string());
        parts.push(format!("[content]\n{json}"));
    }

    // 3) 结构化内容段。
    if let Some(structured) = &result.structured_content {
        let json =
            serde_json::to_string_pretty(structured).unwrap_or_else(|_| structured.to_string());
        parts.push(format!("[structuredContent]\n{json}"));
    }

    if parts.is_empty() {
        // 既无文本、也无非文本块、也无结构化内容：回退为 content 的 JSON 串（不应常见）。
        serde_json::to_string(content).unwrap_or_else(|_| content.to_string())
    } else {
        // 多段之间用空行分隔，便于模型区分边界。
        parts.join("\n\n")
    }
}

/// 从 MCP `content`（可能是折叠后的 String，也可能是 content 块数组）里提取所有 text 段。
fn extract_text_from_content(content: &Value) -> String {
    match content {
        // call_tool 已把「全文本且无 structuredContent」折叠为 String。
        Value::String(s) => s.clone(),
        // 含非文本块的原始数组：仅取 type=text 的段，其余忽略（由上层 JSON 兜底）。
        Value::Array(arr) => arr
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    item.get("text").and_then(|t| t.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        // 其它形态（对象、数字等）：整体 stringify。
        other => other.to_string(),
    }
}

// ===========================================================================
// 单元测试：聚焦「名字净化 / 消歧 / OpenAI 合法性 / 长度上限 / 确定性」纯函数路径。
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize_replaces_invalid_chars() {
        assert_eq!(sanitize_segment("read_file"), "read_file");
        assert_eq!(sanitize_segment("read.file"), "read_file");
        assert_eq!(sanitize_segment("a/b c"), "a_b_c");
        assert_eq!(sanitize_segment("UPPER-1"), "UPPER-1");
        // 中文等非 ASCII 一律替成 _（连续 _ 仍折叠）。
        assert_eq!(sanitize_segment("工具"), "_");
        assert_eq!(sanitize_segment("a工b"), "a_b");
    }

    #[test]
    fn normalizes_relative_path_against_server_parent() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        let docs = agent.join("知识库测试文档2");
        std::fs::create_dir_all(&docs).unwrap();
        let file = docs.join("doc.md");
        std::fs::write(&file, "hello").unwrap();

        let input = json!({
            "path": "agent/知识库测试文档2/doc.md",
            "intent": "auto"
        });
        let out = normalize_mcp_path_arguments(input, Some(agent.to_str().unwrap()));
        let normalized = out["path"].as_str().unwrap();

        assert_eq!(
            PathBuf::from(normalized).canonicalize().unwrap(),
            file.canonicalize().unwrap()
        );
        assert_eq!(out["intent"], "auto");
    }

    #[test]
    fn normalizes_file_argument_too() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let file = agent.join("note.md");
        std::fs::write(&file, "hello").unwrap();

        let input = json!({ "file": "note.md" });
        let out = normalize_mcp_path_arguments(input, Some(agent.to_str().unwrap()));
        let normalized = out["file"].as_str().unwrap();

        assert_eq!(
            PathBuf::from(normalized).canonicalize().unwrap(),
            file.canonicalize().unwrap()
        );
    }

    #[test]
    fn leaves_missing_paths_unchanged() {
        let input = json!({ "path": "agent/missing/doc.md" });
        let out = normalize_mcp_path_arguments(input, Some("agent"));

        assert_eq!(out["path"], "agent/missing/doc.md");
    }

    #[test]
    fn network_paths_are_never_resolved_locally() {
        // 任意两个前导分隔符的组合都是 UNC 写法：不得进入 canonicalize（会对外连接并带出
        // NTLM 凭据），原样透传。
        for raw in [
            r"\\attacker.example\share\x",
            "//attacker.example/share/x",
            r"/\attacker.example\share\x",
            r"\/attacker.example\share\x",
            r"\\?\UNC\attacker.example\share\x",
            r"\\.\pipe\x",
        ] {
            assert_eq!(resolve_existing_path(raw, Some("agent")), None, "{raw}");
        }
    }

    #[test]
    fn sanitize_collapses_double_underscore() {
        // 段内不得残留 "__"，否则分隔符歧义。
        assert_eq!(sanitize_segment("my__server"), "my_server");
        assert_eq!(sanitize_segment("a___b"), "a_b");
        // 非法字符与下划线相邻也要折叠。
        assert_eq!(sanitize_segment("a./b"), "a_b");
        assert_eq!(sanitize_segment("a..b"), "a_b");
    }

    #[test]
    fn sanitize_empty_falls_back() {
        assert_eq!(sanitize_segment(""), "_");
        assert_eq!(sanitize_segment("///"), "_");
        assert_eq!(sanitize_segment("... ..."), "_");
    }

    #[test]
    fn clean_names_need_no_disambiguator() {
        // 原名合法且不超长：直接用，无哈希后缀（最常见路径，名字保持干净）。
        assert_eq!(build_exposed_name("srv", "tool"), "srv__tool");
        assert_eq!(build_exposed_name("fs", "read_file"), "fs__read_file");
    }

    #[test]
    fn sanitization_disambiguates_colliding_tools() {
        // P2-B：read.file 与 read/file 都净化为 read_file，但哈希不同 → 暴露名不同。
        let a = build_exposed_name("srv", "read.file");
        let b = build_exposed_name("srv", "read/file");
        assert_ne!(a, b, "净化撞名必须被消歧: {a} == {b}");
        assert!(a.starts_with("srv__read_file"), "got {a}");
        assert!(b.starts_with("srv__read_file"), "got {b}");
        assert_eq!(a.matches("__").count(), 1, "分隔符应唯一: {a}");
    }

    #[test]
    fn truncation_disambiguates_long_tools() {
        // P2-B：两个仅尾部不同的超长工具，截断后前缀相同，但哈希不同 → 不撞名。
        let a = build_exposed_name("srv", &format!("tool{}", "a".repeat(200)));
        let b = build_exposed_name("srv", &format!("tool{}", "b".repeat(200)));
        assert_ne!(a, b);
        assert!(
            a.len() <= MAX_FUNCTION_NAME_LEN,
            "len={} > 64: {a}",
            a.len()
        );
        assert!(
            b.len() <= MAX_FUNCTION_NAME_LEN,
            "len={} > 64: {b}",
            b.len()
        );
    }

    #[test]
    fn exposed_name_is_openai_valid() {
        let n = build_exposed_name("my.srv", "read.file");
        // 净化触发哈希后缀；仍只含一个 "__" 分隔符，全字符合法。
        assert_eq!(n.matches("__").count(), 1, "分隔符应唯一: {n}");
        assert!(
            n.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "非法字符: {n}"
        );
        assert!(n.len() <= MAX_FUNCTION_NAME_LEN);
    }

    #[test]
    fn exposed_name_server_with_double_underscore_parses_cleanly() {
        // P2：server 名含 "__" 不应让 McpToolTarget::parse 错位。
        let n = build_exposed_name("a__b", "tool");
        let t = crate::mcp::McpToolTarget::parse(&n).unwrap();
        assert_eq!(t.server_name, "a_b"); // 不会错位成 "a"
        assert!(t.tool_name.starts_with("tool"));
    }

    #[test]
    fn exposed_name_clamps_to_64() {
        for (s, t) in [
            ("s".to_string(), "x".repeat(200)),
            ("s".repeat(200), "t".to_string()),
            ("my.srv".to_string(), "read.file".to_string()),
        ] {
            let n = build_exposed_name(&s, &t);
            assert!(
                n.len() <= MAX_FUNCTION_NAME_LEN,
                "len={} > 64: {n}",
                n.len()
            );
        }
    }

    #[test]
    fn exposed_name_is_deterministic() {
        // 同一原始对恒映射到同一暴露名（LLM 看到的名 == 回传的名）。
        assert_eq!(
            build_exposed_name("my.srv", "read.file"),
            build_exposed_name("my.srv", "read.file")
        );
    }

    #[test]
    fn exposed_name_preserves_original_for_callback() {
        // 暴露名被净化，但原始 tool 名不变（回传 server 时用）。
        let tool = McpTool {
            name: "read.file".to_string(),
            description: Some("d".to_string()),
            input_schema: Value::Null,
        };
        let exposed = build_exposed_name("my.srv", &tool.name);
        assert!(exposed.starts_with("my_srv__read_file"), "got {exposed}");
        assert_eq!(tool.name, "read.file"); // 原始名未被修改
    }

    #[test]
    fn render_plain_text_result() {
        // 纯文本（call_tool 折叠为 String）：直接输出。
        let r = McpToolResult {
            content: Value::String("hello".into()),
            structured_content: None,
            is_error: false,
        };
        assert_eq!(render_tool_result(&r), "hello");
    }

    #[test]
    fn render_appends_structured_content() {
        // 有 text 且有 structuredContent：结构化 JSON 追加在文本后，确保不丢。
        let r = McpToolResult {
            content: Value::String("rows: 2".into()),
            structured_content: Some(serde_json::json!({ "rows": [{ "id": 1 }, { "id": 2 }] })),
            is_error: false,
        };
        let out = render_tool_result(&r);
        assert!(out.starts_with("rows: 2"), "应保留文本: {out}");
        assert!(out.contains("[structuredContent]"), "应标注结构化段: {out}");
        assert!(out.contains("\"rows\""), "应包含结构化字段: {out}");
    }

    #[test]
    fn render_structured_only() {
        // 无 text、仅有 structuredContent：直接输出其 JSON。
        let r = McpToolResult {
            content: Value::Array(vec![]),
            structured_content: Some(serde_json::json!({ "ok": true })),
            is_error: false,
        };
        let out = render_tool_result(&r);
        assert!(out.contains("\"ok\""), "应输出结构化 JSON: {out}");
    }

    #[test]
    fn render_preserves_non_text_blocks() {
        // P2：text + image 的混合 content，旧实现只返回 text 会丢掉图片；新实现追加 [content] 段。
        let r = McpToolResult {
            content: serde_json::json!([
                { "type": "text", "text": "这是截图" },
                { "type": "image", "data": "BASE64...", "mimeType": "image/png" }
            ]),
            structured_content: None,
            is_error: false,
        };
        let out = render_tool_result(&r);
        assert!(out.contains("这是截图"), "应保留文本段: {out}");
        assert!(out.contains("[content]"), "应追加完整 content 段: {out}");
        assert!(
            out.contains("\"mimeType\""),
            "完整 content 段应含图片信息: {out}"
        );
    }

    #[test]
    fn render_pure_text_array_uses_fast_path() {
        // 全 text 数组（无非文本块、无 structuredContent）：走纯文本快速路径，不附 [content]。
        let r = McpToolResult {
            content: serde_json::json!([
                { "type": "text", "text": "第一行" },
                { "type": "text", "text": "第二行" }
            ]),
            structured_content: None,
            is_error: false,
        };
        let out = render_tool_result(&r);
        assert_eq!(out, "第一行\n第二行", "全文本应折叠为纯文本: {out}");
        assert!(!out.contains("[content]"), "全文本不应附 content 段: {out}");
    }

    #[test]
    fn render_error_preserves_full_content() {
        // P2：isError 是工具级「正常结果」，应完整渲染多段文本/结构化内容，而非只取首段。
        // 这里用 is_error=true 的多段 + structuredContent 结果，验证渲染不丢信息（上层据此产出 err）。
        let r = McpToolResult {
            content: serde_json::json!([
                { "type": "text", "text": "原因：参数非法" },
                { "type": "text", "text": "建议：检查 path" }
            ]),
            structured_content: Some(serde_json::json!({ "code": "EINVAL", "field": "path" })),
            is_error: true,
        };
        let out = render_tool_result(&r);
        assert!(out.contains("原因：参数非法"), "应保留第一段文本: {out}");
        assert!(out.contains("建议：检查 path"), "应保留第二段文本: {out}");
        assert!(out.contains("[structuredContent]"), "应附结构化段: {out}");
        assert!(out.contains("\"field\""), "结构化段应含字段: {out}");
    }
}
