//! streamable-http 传输（MCP 2025-03-25+）：per-request POST，响应可为 JSON 或 SSE。
//!
//! - [`HttpTransport::send_raw`] 每次把一行 JSON-RPC POST 到 `url`（`Accept: application/json,
//!   text/event-stream`），按响应 `Content-Type` 分派：`application/json` → 单条；`text/event-stream`
//!   → SSE 多事件（[`parse_sse`]）。每条消息交 [`Core::route_inbound`] 路由；server 主动请求
//!   （如 ping）的回写收集后再 POST 出去。
//! - 会话：响应里的 `Mcp-Session-Id` 头被捕获，后续请求（含 [`HttpTransport::shutdown`] 的
//!   DELETE）回带。
//! - 无长连接、无独立 GET 流、无 Last-Event-ID（v1 范围；见 spec §2）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use tracing::{debug, warn};

use super::super::jsonrpc::{summarize_text, Core};
use super::super::{McpError, Result as McpResult};
use super::McpTransport;

const SESSION_HEADER: &str = "mcp-session-id";
const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

pub struct HttpTransport {
    name: String,
    url: String,
    client: reqwest::Client,
    base_headers: HeaderMap,
    session_id: Mutex<Option<String>>,
    core: Mutex<Weak<Core>>,
    closed: AtomicBool,
}

impl HttpTransport {
    pub async fn new(
        name: String,
        url: String,
        headers: Option<HashMap<String, String>>,
        request_timeout: Duration,
    ) -> McpResult<Self> {
        let mut base_headers = HeaderMap::new();
        base_headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        if let Some(map) = headers {
            for (k, v) in map {
                let hname = HeaderName::from_bytes(k.as_bytes())
                    .map_err(|e| McpError::Config(format!("非法 header 名 '{k}': {e}")))?;
                let hval = HeaderValue::from_str(&v)
                    .map_err(|e| McpError::Config(format!("非法 header 值 '{v}': {e}")))?;
                base_headers.insert(hname, hval);
            }
        }
        let client = reqwest::Client::builder()
            .timeout(request_timeout)
            .build()
            .map_err(|e| McpError::Tool(format!("MCP[{name}]: 创建 HTTP client 失败: {e}")))?;
        Ok(Self {
            name,
            url,
            client,
            base_headers,
            session_id: Mutex::new(None),
            core: Mutex::new(Weak::new()),
            closed: AtomicBool::new(false),
        })
    }

    fn session_id_get(&self) -> Option<String> {
        self.session_id.lock().ok().and_then(|g| g.clone())
    }

    fn protocol_version_get(&self) -> Option<String> {
        self.core
            .lock()
            .ok()
            .and_then(|g| g.upgrade())
            .and_then(|core| core.protocol_version())
    }

    fn add_session_headers(&self, hdrs: &mut HeaderMap) {
        if let Some(sid) = self.session_id_get() {
            if let Ok(v) = HeaderValue::from_str(&sid) {
                hdrs.insert(SESSION_HEADER, v);
            }
        }
        if let Some(version) = self.protocol_version_get() {
            if let Ok(v) = HeaderValue::from_str(&version) {
                hdrs.insert(PROTOCOL_VERSION_HEADER, v);
            }
        }
    }

    async fn post(&self, body: &str) -> McpResult<reqwest::Response> {
        let mut hdrs = self.base_headers.clone();
        hdrs.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        self.add_session_headers(&mut hdrs);
        self.client
            .post(&self.url)
            .headers(hdrs)
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| McpError::Tool(format!("MCP[{}]: HTTP 请求失败: {e}", self.name)))
    }

    /// 处理一次 POST 的完整响应：捕获 session id、按 Content-Type 解析消息、route_inbound、
    /// 回写 server 主动请求的回复。
    async fn handle_response(&self, resp: reqwest::Response) -> McpResult<()> {
        if !resp.status().is_success() {
            return Err(McpError::Tool(format!(
                "MCP[{}]: HTTP {}",
                self.name,
                resp.status()
            )));
        }
        // 任意响应都可能携带会话 id（实际仅 initialize 携带）；捕获即幂等。
        if let Some(sid) = resp
            .headers()
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
        {
            if let Ok(mut g) = self.session_id.lock() {
                *g = Some(sid.to_string());
            }
        }
        let is_sse = resp
            .headers()
            .get_all(CONTENT_TYPE)
            .iter()
            .any(|v| v.to_str().is_ok_and(|s| s.contains("text/event-stream")));
        let text = resp
            .text()
            .await
            .map_err(|e| McpError::Tool(format!("MCP[{}]: 读取响应失败: {e}", self.name)))?;
        let trimmed = text.trim();
        let messages: Vec<String> = if is_sse {
            parse_sse(&text)
        } else if trimmed.is_empty() {
            Vec::new()
        } else {
            vec![trimmed.to_string()]
        };
        let mut replies: Vec<String> = Vec::new();
        for msg in messages {
            debug!(server = %self.name, "MCP ← {}", summarize_text(&msg, 200));
            if let Some(core) = self.core.lock().ok().and_then(|g| g.upgrade()) {
                core.route_inbound(&msg, &mut replies);
            }
        }
        for r in replies {
            // server 主动请求（如 ping）的回写：best-effort。回写失败不应让用户当次的
            // tools/call 整体失败（stdio 的 write_back 同样忽略失败）。
            let _ = self.send_raw(&r).await;
        }
        Ok(())
    }

    /// 构造终止会话的 DELETE 请求（仅在已捕获 session id 时）。无 session 则返回 None。
    fn delete_request(&self) -> Option<reqwest::RequestBuilder> {
        let sid = self.session_id_get()?;
        let mut hdrs = self.base_headers.clone();
        if let Ok(v) = HeaderValue::from_str(&sid) {
            hdrs.insert(SESSION_HEADER, v);
        }
        self.add_session_headers(&mut hdrs);
        Some(self.client.delete(&self.url).headers(hdrs))
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn send_raw(&self, line: &str) -> McpResult<()> {
        let resp = self.post(line).await?;
        match self.handle_response(resp).await {
            Ok(()) => Ok(()),
            Err(e) => {
                warn!(server = %self.name, error = %e, "http 传输请求失败");
                Err(e)
            }
        }
    }

    fn is_alive(&self) -> bool {
        true // 无长连接；成败按 per-request 判定（失败已由 send_raw 返回 Err → Core::call mark_dead）
    }

    async fn shutdown(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(req) = self.delete_request() {
            let _ = req.send().await;
        }
    }

    /// Drop 路径（不能 await）：若有 tokio 运行时，则 fire-and-forget spawn 一次 best-effort
    /// DELETE；无运行时则放弃（session 由 server 端超时清理）。
    fn shutdown_blocking(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(req) = self.delete_request() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            // fire-and-forget：丢弃 JoinHandle 即 detach 任务（tokio 语义），DELETE 在后台发出。
            let _detached = handle.spawn(async move {
                let _ = req.send().await;
            });
        }
    }

    fn bind_core(&self, core: Weak<Core>) {
        if let Ok(mut g) = self.core.lock() {
            *g = core;
        }
    }
}

impl Drop for HttpTransport {
    fn drop(&mut self) {
        self.shutdown_blocking();
    }
}

/// 解析 SSE 流文本，返回每个"事件"聚合后的 data 字符串（多行 data 按 SSE 规范用 `\n` 拼接）。
/// 忽略 event/id/retry 字段；无 data 的事件被跳过。按空行划分事件；末尾未以空行收尾也 flush。
pub(crate) fn parse_sse(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() {
            if !data_lines.is_empty() {
                out.push(data_lines.join("\n"));
                data_lines.clear();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            // SSE 规范：值若以单个空格开头则移除它。
            let val = rest.strip_prefix(' ').unwrap_or(rest);
            data_lines.push(val.to_string());
        }
        // event/id/retry/注释(:) 忽略。
    }
    if !data_lines.is_empty() {
        out.push(data_lines.join("\n"));
    }
    out
}

#[cfg(test)]
mod sse_tests {
    use super::parse_sse;

    #[test]
    fn single_data_event() {
        let body = "data: {\"id\":1,\"result\":{}}\n\n";
        assert_eq!(parse_sse(body), vec![r#"{"id":1,"result":{}}"#.to_string()]);
    }

    #[test]
    fn multi_line_data_joined_with_newline() {
        // SSE 规范：同一事件的多行 data 用 "\n" 拼接。
        let body = "data: line1\ndata: line2\n\n";
        assert_eq!(parse_sse(body), vec!["line1\nline2".to_string()]);
    }

    #[test]
    fn multiple_events() {
        let body = "data: {\"id\":1}\n\ndata: {\"id\":2}\n\n";
        assert_eq!(
            parse_sse(body),
            vec![r#"{"id":1}"#.to_string(), r#"{"id":2}"#.to_string()]
        );
    }

    #[test]
    fn ignores_event_id_retry_fields() {
        let body = "id: 42\nevent: message\ndata: {\"x\":1}\n\n";
        assert_eq!(parse_sse(body), vec![r#"{"x":1}"#.to_string()]);
    }

    #[test]
    fn skips_events_without_data() {
        let body = "event: ping\n\ndata: {\"ok\":1}\n\n";
        assert_eq!(parse_sse(body), vec![r#"{"ok":1}"#.to_string()]);
    }

    #[test]
    fn empty_body_yields_nothing() {
        assert!(parse_sse("").is_empty());
    }
}

#[cfg(test)]
mod http_tests {
    use super::HttpTransport;
    use crate::mcp::jsonrpc::Core;
    use crate::mcp::transport::McpTransport;
    use std::sync::Arc;
    use std::time::Duration;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const INIT_RESULT: &str =
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{}}}"#;

    /// 挂载 initialize（带 session id）、initialized 通知（204）、tools/list（空工具）三组 mock。
    async fn mount_init_and_empty_tools(server: &MockServer, session_id: &str) {
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(r#""method":"initialize""#))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Mcp-Session-Id", session_id)
                    .insert_header("Content-Type", "application/json")
                    .set_body_string(INIT_RESULT),
            )
            .up_to_n_times(1)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(
                r#""method":"notifications/initialized""#,
            ))
            .and(header("mcp-protocol-version", "2025-11-25"))
            .respond_with(ResponseTemplate::new(204))
            .up_to_n_times(1)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(r#""method":"tools/list""#))
            .and(header("mcp-protocol-version", "2025-11-25"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/json")
                    .set_body_string(r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#),
            )
            .up_to_n_times(1)
            .mount(server)
            .await;
    }

    async fn boot(server_uri: &str) -> (Arc<Core>, Arc<HttpTransport>) {
        // Core 的 call_timeout 与 HttpTransport 的 request_timeout 取一致值。
        let timeout = Duration::from_secs(5);
        let core = Core::new("t".into(), timeout);
        let http = Arc::new(
            HttpTransport::new("t".into(), format!("{server_uri}/mcp"), None, timeout)
                .await
                .unwrap(),
        );
        core.set_transport(Arc::clone(&http) as Arc<dyn McpTransport>);
        core.initialize().await.unwrap();
        core.refresh_tools().await.unwrap();
        (core, http)
    }

    #[tokio::test]
    async fn initialize_captures_session_id() {
        let server = MockServer::start().await;
        mount_init_and_empty_tools(&server, "sess-1").await;
        let (_core, http) = boot(&server.uri()).await;
        assert_eq!(http.session_id_get().as_deref(), Some("sess-1"));
    }

    #[tokio::test]
    async fn json_tools_list_routed() {
        let server = MockServer::start().await;
        // 覆盖默认的空工具 mock：返回一个工具。session id 回带验证。
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(r#""method":"initialize""#))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Mcp-Session-Id", "s")
                    .insert_header("Content-Type", "application/json")
                    .set_body_string(INIT_RESULT),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(
                r#""method":"notifications/initialized""#,
            ))
            .and(header("mcp-protocol-version", "2025-11-25"))
            .respond_with(ResponseTemplate::new(204))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(r#""method":"tools/list""#))
            .and(header("mcp-session-id", "s"))
            .and(header("mcp-protocol-version", "2025-11-25"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/json")
                    .set_body_string(
                        r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"webSearchPrime","description":"d","inputSchema":{"type":"object"}}]}}"#,
                    ),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let (core, _http) = boot(&server.uri()).await;
        let tools = core.list_tools().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "webSearchPrime");
    }

    #[tokio::test]
    async fn sse_response_routed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(r#""method":"initialize""#))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(format!("data: {INIT_RESULT}\n\n"), "text/event-stream"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(
                r#""method":"notifications/initialized""#,
            ))
            .and(header("mcp-protocol-version", "2025-11-25"))
            .respond_with(ResponseTemplate::new(204))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // tools/list 走 SSE，含一条无关 progress 通知 + 真正的 tools 响应。
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(r#""method":"tools/list""#))
            .and(header("mcp-protocol-version", "2025-11-25"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(
                    "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\n\n",
                    "text/event-stream",
                ),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let (core, _http) = boot(&server.uri()).await;
        assert!(core.list_tools().await.is_empty());
    }

    #[tokio::test]
    async fn server_ping_on_sse_replied() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(r#""method":"initialize""#))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(format!("data: {INIT_RESULT}\n\n"), "text/event-stream"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(
                r#""method":"notifications/initialized""#,
            ))
            .and(header("mcp-protocol-version", "2025-11-25"))
            .respond_with(ResponseTemplate::new(204))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // tools/list 的 SSE 里夹带 server 的 ping(id=9)，客户端需再 POST 一条 ping 回复。
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(r#""method":"tools/list""#))
            .and(header("mcp-protocol-version", "2025-11-25"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(
                    "data: {\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\n\n",
                    "text/event-stream",
                ),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // ping 回复（body 含 "id":9 且 result:{}）必须被 POST 出去。
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(r#""id":9"#))
            .and(header("mcp-protocol-version", "2025-11-25"))
            .respond_with(ResponseTemplate::new(204))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let (core, _http) = boot(&server.uri()).await;
        assert!(core.list_tools().await.is_empty());
    }

    #[tokio::test]
    async fn http_failure_marks_core_dead() {
        // Bug#1 回归：tools/call 返回 500 时，Core::call 经 send_raw 失败须 mark_dead，
        // 使 McpClient::is_alive() 读到 false，进而触发 provider 的「中断重建重试」。
        let server = MockServer::start().await;
        mount_init_and_empty_tools(&server, "s").await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(r#""method":"tools/call""#))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let (core, _http) = boot(&server.uri()).await;
        assert!(core.is_alive(), "握手成功后应存活");
        let res = core.call_tool("any", serde_json::json!({})).await;
        assert!(res.is_err(), "tools/call 500 应返回错误");
        assert!(!core.is_alive(), "传输失败后 Core 须 mark_dead");
    }
}
