//! 传输无关的 JSON-RPC 核心：类型、消息分发、握手/tools 协议、共享状态 [`Core`]。
//!
//! 设计要点：
//! - [`Core`] 持有协议状态（id 计数、pending 表、工具缓存、协商结果、存活标志）与协议方法
//!   （`initialize` / `refresh_tools` / `list_tools` / `call_tool` / `call`）。
//! - 出站（client → server）经注入的 [`crate::mcp::transport::McpTransport`]：`Core::call`
//!   分配 id、注册 pending、`transport.send_raw` 发请求、带超时等 oneshot。
//! - 入站（server → client）由传输读取后调 [`Core::route_inbound`]：响应按 id 路由到 pending，
//!   server 主动请求的回写收集进 `replies`（交还传输发出），通知 `tools/list_changed` 递增代数。
//! - Core ↔ transport 的 Arc 环：transport 持 `Weak<Core>`（读消息时 upgrade），Core 持
//!   `Arc<dyn McpTransport>`（经 [`Core::set_transport`] 注入）。
//!
//! 参考：MCP `2025-11-25` 稳定规范 —— Lifecycle / Tools / Ping。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::mcp::transport::McpTransport;
use crate::mcp::{McpError, Result as McpResult};

/// 握手时**声明**的协议版本（MCP 官方最新稳定版）。
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// 客户端**可接受**的协议版本白名单（协商校验用）。
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// `tools/list` 分页拉取的迭代上限，防御「永远返回 `nextCursor`」的异常 server。
const MAX_TOOLS_LIST_PAGES: usize = 256;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 类型
// ---------------------------------------------------------------------------

/// 客户端→server 的请求。`id` 为自增 u64，确保可路由回包。
#[derive(Debug, Serialize)]
pub(crate) struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

/// 通用 JSON-RPC 消息（请求/响应/通知三态合一），用于解析 server 发来的任意行。
#[derive(Debug, Deserialize)]
pub(crate) struct JsonRpcMessage {
    #[serde(default, rename = "jsonrpc")]
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

// ---------------------------------------------------------------------------
// MCP 工具 schema
// ---------------------------------------------------------------------------

/// MCP server 通过 `tools/list` 上报的单个工具描述。
#[derive(Debug, Clone, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

// ---------------------------------------------------------------------------
// pending 表：id → oneshot sender
// ---------------------------------------------------------------------------

pub(crate) type PendingMap =
    Arc<Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>>>;

// ---------------------------------------------------------------------------
// 协商结果与工具调用结果
// ---------------------------------------------------------------------------

/// `initialize` 握手后保存的协商状态：server 选定的协议版本 + 其声明的能力。
#[derive(Debug, Clone, Default)]
struct Negotiated {
    protocol_version: Option<String>,
    capabilities: Value,
}

/// `tools/call` 的完整结果，保留 MCP 定义的全部字段（content / structuredContent / isError）。
#[derive(Debug, Clone)]
pub struct McpToolResult {
    pub content: Value,
    pub structured_content: Option<Value>,
    pub is_error: bool,
}

// ---------------------------------------------------------------------------
// Core：协议状态 + 协议方法
// ---------------------------------------------------------------------------

/// 传输无关的 MCP 协议核心。被 stdio / http 两种传输共享。
///
/// - 出站经 `set_transport` 注入的 `Arc<dyn McpTransport>`。
/// - 入站由传输读取后调 [`Core::route_inbound`]。
/// - `alive` 是与传输共享的 `Arc<AtomicBool>`：传输的读循环在连接断开时置 false，
///   [`Core::call`] 在写入失败时 [`Core::mark_dead`]；二者同一把 Arc，始终一致。
pub struct Core {
    name: String,
    next_id: AtomicU64,
    pending: PendingMap,
    tools: Arc<Mutex<Vec<McpTool>>>,
    tools_gen: Arc<AtomicU64>,
    tools_acked_gen: AtomicU64,
    negotiated: Mutex<Negotiated>,
    alive: Arc<AtomicBool>,
    call_timeout: Duration,
    transport: Mutex<Option<Arc<dyn McpTransport>>>,
}

impl Core {
    pub fn new(name: String, call_timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            name,
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            tools: Arc::new(Mutex::new(Vec::new())),
            tools_gen: Arc::new(AtomicU64::new(0)),
            tools_acked_gen: AtomicU64::new(0),
            negotiated: Mutex::new(Negotiated::default()),
            alive: Arc::new(AtomicBool::new(true)),
            call_timeout,
            transport: Mutex::new(None),
        })
    }

    /// 暴露共享的存活标志句柄，供传输（如 stdio 读线程）在连接断开时置 false。
    pub(crate) fn alive_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.alive)
    }

    /// 注入传输并完成双向绑定（transport 持 Weak<Core> 经 bind_core）。
    pub fn set_transport(self: &Arc<Self>, t: Arc<dyn McpTransport>) {
        t.bind_core(Arc::downgrade(self));
        if let Ok(mut g) = self.transport.lock() {
            *g = Some(t);
        }
    }

    async fn transport(&self) -> McpResult<Arc<dyn McpTransport>> {
        self.transport
            .lock()
            .ok()
            .and_then(|g| g.as_ref().cloned())
            .ok_or_else(|| McpError::Tool(format!("MCP[{}]: transport 未注入", self.name)))
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn mark_dead(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }

    pub fn protocol_version(&self) -> Option<String> {
        self.negotiated
            .lock()
            .ok()
            .and_then(|g| g.protocol_version.clone())
    }

    pub fn server_capabilities(&self) -> Value {
        self.negotiated
            .lock()
            .map(|g| g.capabilities.clone())
            .unwrap_or(Value::Null)
    }

    /// 入站一行：路由响应/通知；server 主动请求的回写行 push 进 `replies`。
    pub fn route_inbound(&self, line: &str, replies: &mut Vec<String>) {
        let name = self.name.clone();
        let mut sink = |r: &str| replies.push(r.to_string());
        dispatch_message(line, &self.pending, &name, &mut sink, &self.tools_gen);
    }

    /// 读线程在 stdout/SSE 关闭时调用：标记失效并唤醒所有等待中的调用。
    pub fn fail_all_pending(&self, reason: &str) {
        self.alive.store(false, Ordering::SeqCst);
        if let Ok(mut map) = self.pending.lock() {
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(reason.to_string()));
            }
        }
    }

    /// 发一个 JSON-RPC 请求并等待（带超时）其对应 id 的响应。
    pub async fn call(&self, method: &str, params: Option<Value>) -> McpResult<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };

        let (tx, rx) = oneshot::channel();
        {
            let mut map = self
                .pending
                .lock()
                .map_err(|_| McpError::Tool(format!("MCP[{}]: pending 表锁中毒", self.name)))?;
            map.insert(id, tx);
        }

        let line = serde_json::to_string(&req)
            .map_err(|e| McpError::Tool(format!("MCP[{}]: 序列化请求失败: {e}", self.name)))?;
        debug!(server = %self.name, id, method, "MCP → 请求");
        let transport = self.transport().await?;
        // 发送同样受调用超时约束：stdio 的 write_all 在 server 不读 stdin（管道写满）时会
        // 无限阻塞，仅靠「等响应」的超时救不回来——调用方会永久挂起。
        let sent = match timeout(self.call_timeout, transport.send_raw(&line)).await {
            Ok(r) => r,
            Err(_) => {
                // 发送超时 ≠ 连接已死：http 传输的 send_raw 包含「等 server 执行完并读回
                // 响应」，tools/call 跑得慢就会走到这里。若据此 mark_dead，provider 会重建
                // 连接再发一次同样的 tools/call，而首次仍在 server 上执行——非幂等工具被执行
                // 两次。与下方「等响应超时」同口径：撤销 pending、尽力通知取消、保留连接。
                if let Ok(mut map) = self.pending.lock() {
                    map.remove(&id);
                }
                if method != "initialize" {
                    let _ = self.notify_cancelled(id).await;
                }
                return Err(McpError::Tool(format!(
                    "MCP[{}]: 发送 '{method}' 超时（{}s）",
                    self.name,
                    self.call_timeout.as_secs()
                )));
            }
        };
        if let Err(e) = sent {
            if let Ok(mut map) = self.pending.lock() {
                map.remove(&id);
            }
            self.mark_dead();
            return Err(e);
        }

        let response = match timeout(self.call_timeout, rx).await {
            Ok(r) => r,
            Err(_) => {
                if let Ok(mut map) = self.pending.lock() {
                    map.remove(&id);
                }
                if method != "initialize" {
                    let _ = self.notify_cancelled(id).await;
                }
                return Err(McpError::Tool(format!(
                    "MCP[{}]: 调用 '{method}' 超时（{}s）",
                    self.name,
                    self.call_timeout.as_secs()
                )));
            }
        };

        response
            .map_err(|_| {
                self.mark_dead();
                McpError::Tool(format!("MCP[{}]: server 已关闭", self.name))
            })?
            .map_err(|e| McpError::Tool(format!("MCP[{}]: {e}", self.name)))
    }

    /// Best-effort 发送 `notifications/cancelled`，通知 server 取消某个仍在途的请求。
    async fn notify_cancelled(&self, id: u64) {
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": id, "reason": "client timeout" }
        });
        if let Ok(line) = serde_json::to_string(&notif) {
            if let Ok(t) = self.transport().await {
                // 走到这里 server 多半已无响应，写入可能同样卡住：短超时兜底，不让
                // 「取消通知」本身把超时返回再拖住。
                let _ = timeout(Duration::from_secs(5), t.send_raw(&line)).await;
            }
        }
    }

    /// MCP `initialize` 请求 + `notifications/initialized` 通知。
    pub async fn initialize(&self) -> McpResult<()> {
        let params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "rclaw", "version": env!("CARGO_PKG_VERSION") }
        });
        let result = self.call("initialize", Some(params)).await?;

        let version = result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if version.is_empty() {
            return Err(McpError::Tool(format!(
                "MCP[{}]: initialize 响应缺少 protocolVersion",
                self.name
            )));
        }
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
            return Err(McpError::Tool(format!(
                "MCP[{}]: 不支持的协议版本 '{version}'（本客户端支持: {:?}）",
                self.name, SUPPORTED_PROTOCOL_VERSIONS
            )));
        }

        let capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
        if let Ok(mut g) = self.negotiated.lock() {
            g.protocol_version = Some(version.to_string());
            g.capabilities = capabilities.clone();
        }
        debug!(
            server = %self.name,
            protocol_version = %version,
            capabilities = %summarize_json(&capabilities, 400),
            "MCP 已初始化（协议版本已校验）"
        );

        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let line = serde_json::to_string(&notif).unwrap_or_default();
        if let Ok(t) = self.transport().await {
            let _ = t.send_raw(&line).await;
        }

        Ok(())
    }

    /// 拉取 `tools/list` 并刷新本地缓存（分页，带代数 ack 语义）。
    pub async fn refresh_tools(&self) -> McpResult<()> {
        let observed_gen = self.tools_gen.load(Ordering::SeqCst);
        let mut all: Vec<McpTool> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOLS_LIST_PAGES {
            let params = cursor.as_ref().map(|c| serde_json::json!({ "cursor": c }));
            let result = self.call("tools/list", params).await?;
            let page: Vec<McpTool> = serde_json::from_value(
                result.get("tools").cloned().unwrap_or(Value::Array(vec![])),
            )
            .map_err(|e| {
                McpError::Tool(format!("MCP[{}]: 解析 tools/list 结果失败: {e}", self.name))
            })?;
            all.extend(page);
            match result.get("nextCursor").and_then(|v| v.as_str()) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => {
                    cursor = None;
                    break;
                }
            }
        }
        if cursor.is_some() {
            warn!(
                server = %self.name,
                pages = MAX_TOOLS_LIST_PAGES,
                "tools/list 分页超过上限，疑似异常 server，已截断"
            );
        }
        debug!(server = %self.name, count = all.len(), "MCP 工具已加载");
        let mut guard = self
            .tools
            .lock()
            .map_err(|_| McpError::Tool(format!("MCP[{}]: tools 锁中毒", self.name)))?;
        *guard = all;
        self.tools_acked_gen.store(observed_gen, Ordering::SeqCst);
        Ok(())
    }

    /// 返回缓存的工具表（克隆）。存在未确认的变化时先惰性刷新再返回。
    pub async fn list_tools(&self) -> Vec<McpTool> {
        if needs_refresh(
            self.tools_gen.load(Ordering::SeqCst),
            self.tools_acked_gen.load(Ordering::SeqCst),
        ) {
            if let Err(e) = self.refresh_tools().await {
                warn!(server = %self.name, error = %e, "tools/list_changed 后刷新失败，返回旧清单");
            }
        }
        self.tools
            .lock()
            .map(|tools| tools.clone())
            .unwrap_or_default()
    }

    /// 调用 `tools/call`，返回保留 `structuredContent` 的完整结果。
    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> McpResult<McpToolResult> {
        info!(
            server = %self.name,
            tool = %tool_name,
            args = %summarize_json(&arguments, 800),
            "MCP 工具调用开始"
        );
        let params = serde_json::json!({ "name": tool_name, "arguments": arguments });
        let result = self.call("tools/call", Some(params)).await?;

        let structured_content = result.get("structuredContent").cloned();
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let content = result.get("content").cloned().unwrap_or(Value::Null);

        let foldable = structured_content.is_none()
            && content.as_array().is_some_and(|arr| {
                !arr.is_empty()
                    && arr
                        .iter()
                        .all(|item| item.get("type").and_then(|t| t.as_str()) == Some("text"))
            });

        let final_content = if foldable {
            let text: String = content
                .as_array()
                .expect("foldable 已保证为数组")
                .iter()
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()).map(String::from))
                .collect::<Vec<_>>()
                .join("\n");
            Value::String(text)
        } else {
            content
        };

        if is_error {
            warn!(
                server = %self.name,
                tool = %tool_name,
                has_structured = structured_content.is_some(),
                error = %summarize_json(&final_content, 800),
                "MCP 工具调用失败(isError)"
            );
        } else {
            info!(
                server = %self.name,
                tool = %tool_name,
                has_structured = structured_content.is_some(),
                result = %summarize_json(&final_content, 800),
                "MCP 工具调用成功"
            );
        }

        Ok(McpToolResult {
            content: final_content,
            structured_content,
            is_error,
        })
    }
}

// ---------------------------------------------------------------------------
// 消息分发（纯函数，便于无子进程单测）
// ---------------------------------------------------------------------------

/// 解析一行 JSON-RPC 消息，按 response / request / notification 分别处理。
///
/// - **response**（无 method 且带 id）：按数字 id 路由到 pending 表里对应的 oneshot。
///   用「method 缺失」判响应（兼容显式 `result:null`）。
/// - **server 请求**（带 id 且含 method）：`ping` → 回写空 result；其它 → `-32601`。
///   回写行经 `write_back` 交还调用方（传输负责实际发出）。
/// - **通知**（无 id 的 method）：`notifications/tools/list_changed` → 递增 `tools_gen`；
///   其余忽略。
pub(crate) fn dispatch_message<W: FnMut(&str)>(
    line: &str,
    pending: &PendingMap,
    server_name: &str,
    write_back: &mut W,
    tools_gen: &AtomicU64,
) {
    let msg: JsonRpcMessage = match serde_json::from_str(line) {
        Ok(m) => m,
        Err(e) => {
            warn!(
                server = %server_name,
                line = %summarize_text(line, 200),
                "MCP: 无法解析消息: {e}"
            );
            return;
        }
    };

    let Some(method) = msg.method.as_deref() else {
        if let Some(id) = msg.id.as_ref().and_then(|v| v.as_u64()) {
            let payload = match msg.error {
                Some(err) => Err(format!("JSON-RPC error {}: {}", err.code, err.message)),
                None => Ok(msg.result.unwrap_or(Value::Null)),
            };
            if let Some(tx) = pending.lock().ok().and_then(|mut map| map.remove(&id)) {
                let _ = tx.send(payload);
            }
        }
        return;
    };

    if let Some(id) = msg.id {
        match method {
            "ping" => {
                let reply = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {} });
                write_back(&serde_json::to_string(&reply).unwrap_or_default());
            }
            other => {
                warn!(
                    server = %server_name,
                    method = other,
                    "MCP: 收到未声明的 server 请求，回 Method not found"
                );
                let reply = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "method not found" }
                });
                write_back(&serde_json::to_string(&reply).unwrap_or_default());
            }
        }
        return;
    }

    match method {
        "notifications/tools/list_changed" => {
            debug!(server = %server_name, "MCP: 工具列表已变化，递增变化代（仅刷新 client 缓存）");
            tools_gen.fetch_add(1, Ordering::SeqCst);
        }
        _ => {
            debug!(server = %server_name, method, "MCP: 忽略 server 通知");
        }
    }
}

/// 判定是否需要刷新工具列表：观察代与已确认代不一致即需刷新。
pub(crate) fn needs_refresh(tools_gen: u64, tools_acked_gen: u64) -> bool {
    tools_gen != tools_acked_gen
}

/// 把任意 JSON 压缩为不超过 `max_len` 字符的预览串（用于日志）。
pub(crate) fn summarize_json(value: &Value, max_len: usize) -> String {
    let raw = serde_json::to_string(value).unwrap_or_else(|_| "<json-serialize-error>".into());
    summarize_text(&raw, max_len)
}

/// 把文本截断为不超过 `max_len` 字符（按 char 边界），超长追加 "..."。
pub(crate) fn summarize_text(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len {
        return text.to_string();
    }
    text.chars().take(max_len).collect::<String>() + "..."
}

/// 旧入口（仅处理响应的薄封装），保留以兼容既有单测。
#[cfg(test)]
fn dispatch_response(line: &str, pending: &PendingMap, server_name: &str) {
    let mut noop = |_: &str| {};
    let gen = AtomicU64::new(0);
    dispatch_message(line, pending, server_name, &mut noop, &gen);
}

// ===========================================================================
// 单元测试：覆盖「消息分发 / 代数不变式」确定性逻辑（无子进程依赖）。
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatch_response_delivers_result() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert(7, tx);

        dispatch_response(
            r#"{"jsonrpc":"2.0","id":7,"result":{"hello":1}}"#,
            &pending,
            "test",
        );

        let r = rx.await.expect("应收到结果");
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), serde_json::json!({"hello":1}));
    }

    #[tokio::test]
    async fn dispatch_response_delivers_error() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert(3, tx);

        dispatch_response(
            r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"method not found"}}"#,
            &pending,
            "test",
        );

        let r = rx.await.expect("应收到错误");
        assert!(r.is_err(), "error 响应应映射为 Err");
    }

    #[tokio::test]
    async fn dispatch_response_delivers_null_result() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert(11, tx);

        dispatch_response(
            r#"{"jsonrpc":"2.0","id":11,"result":null}"#,
            &pending,
            "test",
        );

        let r = rx.await.expect("result:null 响应应被路由");
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), Value::Null);
    }

    #[test]
    fn supported_versions_include_known_releases() {
        for v in ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"] {
            assert!(SUPPORTED_PROTOCOL_VERSIONS.contains(&v), "白名单应包含 {v}");
        }
    }

    #[test]
    fn dispatch_response_ignores_unparseable_line() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        dispatch_response("not json at all", &pending, "test");
        dispatch_response("", &pending, "test");
        assert!(pending.lock().unwrap().is_empty());
    }

    #[test]
    fn dispatch_response_drops_notification_without_id() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        dispatch_response(
            r#"{"jsonrpc":"2.0","method":"notifications/progress"}"#,
            &pending,
            "test",
        );
        assert!(pending.lock().unwrap().is_empty());
    }

    /// 捕获 write_back 回写内容：解析一行消息，返回所有回写行与变化代终值。
    fn captured_dispatch(line: &str, pending: &PendingMap) -> (Vec<String>, u64) {
        let mut out: Vec<String> = Vec::new();
        let gen = Arc::new(AtomicU64::new(0));
        {
            let gen_ref = gen.clone();
            let mut sink = |l: &str| out.push(l.to_string());
            dispatch_message(line, pending, "test", &mut sink, &gen_ref);
        }
        (out, gen.load(Ordering::SeqCst))
    }

    #[test]
    fn dispatch_replies_to_ping_with_empty_result() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (replies, _) =
            captured_dispatch(r#"{"jsonrpc":"2.0","id":42,"method":"ping"}"#, &pending);
        assert_eq!(replies.len(), 1);
        let v: Value = serde_json::from_str(&replies[0]).unwrap();
        assert_eq!(v["id"], 42);
        assert_eq!(v["result"], serde_json::json!({}));
        assert!(v.get("error").is_none());
    }

    #[test]
    fn dispatch_replies_method_not_found_for_unknown_request() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (replies, _) = captured_dispatch(
            r#"{"jsonrpc":"2.0","id":"req-1","method":"sampling/createMessage","params":{}}"#,
            &pending,
        );
        assert_eq!(replies.len(), 1);
        let v: Value = serde_json::from_str(&replies[0]).unwrap();
        assert_eq!(v["id"], "req-1");
        assert_eq!(v["error"]["code"], -32601);
    }

    #[test]
    fn dispatch_marks_tools_dirty_on_list_changed() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (replies, gen) = captured_dispatch(
            r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#,
            &pending,
        );
        assert!(replies.is_empty());
        assert_eq!(gen, 1);
    }

    #[test]
    fn dispatch_accumulates_repeated_list_changed() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let gen = Arc::new(AtomicU64::new(0));
        let line = r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;
        for _ in 0..3 {
            let g = gen.clone();
            let mut sink = |_: &str| ();
            dispatch_message(line, &pending, "test", &mut sink, &g);
        }
        assert_eq!(gen.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn dispatch_ignores_other_notifications() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (replies, gen) = captured_dispatch(
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{}}"#,
            &pending,
        );
        assert!(replies.is_empty());
        assert_eq!(gen, 0);
    }

    #[test]
    fn needs_refresh_compares_generations() {
        assert!(!needs_refresh(0, 0));
        assert!(!needs_refresh(5, 5));
        assert!(needs_refresh(1, 0));
        assert!(needs_refresh(6, 5));
    }

    #[test]
    fn generation_survives_failed_refresh() {
        let mut gen = 1u64;
        let mut acked = 0u64;
        assert!(needs_refresh(gen, acked));
        assert!(needs_refresh(gen, acked));
        gen = 2;
        acked = 1;
        assert!(needs_refresh(gen, acked));
        acked = 2;
        assert!(!needs_refresh(gen, acked));
    }
}
