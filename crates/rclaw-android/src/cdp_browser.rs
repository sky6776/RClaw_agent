//! CDP 浏览器（Android 宿主）：长驻调试 WebView 的 DevTools socket 直连。
//!
//! 链路：`BrowserSession`（Java，长驻 WebView + `setWebContentsDebuggingEnabled`）
//! 暴露 abstract unix socket `webview_devtools_remote_<pid>` → 本模块经它
//! `GET /json/list` 找 page target → WebSocket 连 `/devtools/page/<id>` → CDP。
//!
//! 为什么走 CDP 而不是 evaluateJavascript 注入：
//! 1. `Fetch` 域拦截可以在请求级改写头——摘掉 WebView 网络栈强加的
//!    `X-Requested-With: <应用包名>`（Google 识别 WebView 的头号信号，任何
//!    公开 Java API 都移不掉它），页面请求从头到尾就是干净的真实浏览器请求；
//! 2. `Input` 域真实事件比注入 JS 合成事件可靠（React/Vue 受控组件）；
//! 3. Cookie/会话由真实网络栈管理，跨调用保持（Google 的 NID 越养越顺）。
//!
//! 截图是例外：离屏 WebView（不挂窗口）没有合成器帧，`Page.captureScreenshot`
//! 常出空白——走 `BrowserSession.screenshot`（View.draw 软件位图）。
//!
//! 平台门：abstract socket 与连接层是 unix 专属；非 unix 宿主（workspace 里
//! rclaw-android 也要过 Windows 编译）整条链路编译保留、运行时报「仅 Android」。

use std::time::Duration;

use anyhow::anyhow;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch, Mutex as AsyncMutex};

#[cfg(unix)]
use std::collections::HashMap;
#[cfg(unix)]
use futures::{SinkExt, StreamExt};
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(unix)]
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
#[cfg(unix)]
use tokio_tungstenite::tungstenite::Message;
#[cfg(unix)]
use tokio_tungstenite::WebSocketStream;

use rclaw_tools::Tool;

/// 进程级共享会话（web_browser 与 browser_* 工具共用同一个 WebView）。
static BROWSER: std::sync::OnceLock<CdpBrowser> = std::sync::OnceLock::new();

pub fn shared() -> &'static CdpBrowser {
    BROWSER.get_or_init(CdpBrowser::new)
}

/// `/json/list` 应答里挑 page target（跳过 service_worker 等）。
#[cfg(any(unix, test))]
fn pick_page_target(body: &str) -> anyhow::Result<String> {
    let targets: Vec<Value> =
        serde_json::from_str(body).map_err(|e| anyhow!("解析 /json/list 失败: {e}"))?;
    for target in targets {
        if target.get("type").and_then(Value::as_str) == Some("page") {
            return target
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow!("page target 缺少 id"));
        }
    }
    Err(anyhow!("DevTools 没有可用的 page target"))
}

// ---------------------------------------------------------------------------
// 连接层：abstract socket → HTTP 探测 → WebSocket（unix 专属）
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod socket_layer {
    use super::*;

    /// 连接 DevTools abstract unix socket（名字不带前导 @ / \0）。
    /// std 1.70+ 的 `SocketAddrExt::from_abstract_name`（linux/android 各自的
    /// os 模块提供；socket2 0.6 没有抽象地址构造器，故直接用 std）。
    pub(super) async fn connect_abstract_socket(
        socket_name: &str,
    ) -> anyhow::Result<tokio::net::UnixStream> {
        use std::os::unix::net::SocketAddr as StdSocketAddr;
        #[cfg(target_os = "linux")]
        use std::os::linux::net::SocketAddrExt as _;
        #[cfg(target_os = "android")]
        use std::os::android::net::SocketAddrExt as _;
        let addr = StdSocketAddr::from_abstract_name(socket_name.as_bytes())?;
        Ok(tokio::net::UnixStream::connect_addr(&addr).await?)
    }

    /// 探测 page target id：HTTP 探测走完即断（Connection: close）。WebView
    /// 创建后调试 socket 就绪有延迟，整体带重试。
    async fn probe_page_target(socket_name: &str) -> anyhow::Result<String> {
        let mut last_err = String::new();
        for _ in 0..12 {
            match try_probe(socket_name).await {
                Ok(id) => return Ok(id),
                Err(error) => {
                    last_err = error.to_string();
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
        Err(anyhow!(
            "连接 WebView DevTools 失败（连续重试超限）: {last_err}"
        ))
    }

    async fn try_probe(socket_name: &str) -> anyhow::Result<String> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut stream = connect_abstract_socket(socket_name).await?;
        stream
            .write_all(b"GET /json/list HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await??;
        let text = String::from_utf8_lossy(&buf);
        let body = text
            .split("\r\n\r\n")
            .nth(1)
            .ok_or_else(|| anyhow!("DevTools HTTP 应答缺少响应体"))?;
        super::pick_page_target(body)
    }

    /// 连上 page target 的 WebSocket。
    pub(super) async fn connect_page_ws(
        socket_name: &str,
    ) -> anyhow::Result<WebSocketStream<tokio::net::UnixStream>> {
        let target = probe_page_target(socket_name).await?;
        let stream = connect_abstract_socket(socket_name).await?;
        let request: tokio_tungstenite::tungstenite::handshake::client::Request =
            format!("ws://localhost/devtools/page/{target}")
                .into_client_request()
                .map_err(|e| anyhow!("构造 CDP WebSocket 请求失败: {e}"))?;
        let (ws, _response) = tokio_tungstenite::client_async(request, stream)
            .await
            .map_err(|e| anyhow!("CDP WebSocket 握手失败: {e}"))?;
        Ok(ws)
    }
}

// ---------------------------------------------------------------------------
// CDP 协议：manager 任务（串行命令 + 事件处理）
// ---------------------------------------------------------------------------

/// 一条待发送的 CDP 命令；应答经 oneshot 回给调用方（调用方自带超时）。
struct CdpCommand {
    method: String,
    params: Value,
    resp: oneshot::Sender<Result<Value, String>>,
}

/// 会话句柄：clone 共用同一条连接。
#[derive(Clone)]
struct Session {
    tx: mpsc::Sender<CdpCommand>,
    load_rx: watch::Receiver<u64>,
}

impl Session {
    async fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.tx
            .clone()
            .send(CdpCommand {
                method: method.to_string(),
                params,
                resp: resp_tx,
            })
            .await
            .map_err(|_| "CDP 连接已断开".to_string())?;
        let reply = tokio::time::timeout(timeout, resp_rx)
            .await
            .map_err(|_| format!("CDP 命令 {method} 超时"))?
            .map_err(|_| "CDP 连接已断开".to_string())??;
        Ok(reply)
    }

    /// 等待一次新的页面加载完成（`Page.loadEventFenced` 计数超过 `after`）。
    async fn wait_load(&self, after: u64, timeout: Duration) -> Result<(), String> {
        let mut rx = self.load_rx.clone();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if *rx.borrow_and_update() > after {
                return Ok(());
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err("页面加载超时".to_string());
            }
            match tokio::time::timeout(deadline - now, rx.changed()).await {
                Err(_) => return Err("页面加载超时".to_string()),
                Ok(Err(_)) => return Err("CDP 连接已断开".to_string()),
                Ok(Ok(())) => {
                    if *rx.borrow_and_update() > after {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Fetch.interception 的改写：摘掉 WebView 网络栈强加的 `X-Requested-With`
/// （应用包名指纹），其余头原样保留。入参是 paused 事件里 request.headers 的
/// object map。
#[cfg(any(unix, test))]
fn strip_x_requested_with(headers: &Value) -> Value {
    let mut map = headers.as_object().cloned().unwrap_or_default();
    let doomed: Vec<String> = map
        .keys()
        .filter(|k| k.eq_ignore_ascii_case("X-Requested-With"))
        .cloned()
        .collect();
    for key in doomed {
        map.remove(&key);
    }
    Value::Object(map)
}

/// 把改写后的 object map 变成 Fetch.continueRequest 的 `headers` 数组形态
/// （CDP ≥ 126；旧版 WebView 收 object，两种形态按序降级，见 run_connection）。
#[cfg(any(unix, test))]
fn header_entries(headers: &Value) -> Value {
    Value::Array(
        headers
            .as_object()
            .map(|map| {
                map.iter()
                    .map(|(name, value)| {
                        json!({ "name": name, "value": value.as_str().unwrap_or_default() })
                    })
                    .collect()
            })
            .unwrap_or_default(),
    )
}

/// 某条放行命令失败后的降级队列：requestId + 剩余尝试。
#[cfg(any(unix, test))]
struct FetchRetry {
    request_id: String,
    attempts: std::vec::IntoIter<Value>,
}

/// manager：持有 WebSocket 写半边，串行发命令、按 id 回应答、处理事件。
/// `Page.loadEventFenced` 驱动 load 计数（wait_load 等它）；Fetch 拦截的
/// 放行命令失败时按 attempts 降级，最终以无头放行兜底——宁可带上指纹也不让
/// 页面卡死在拦截器里。
#[cfg(unix)]
async fn run_connection(
    ws: WebSocketStream<tokio::net::UnixStream>,
    mut rx: mpsc::Receiver<CdpCommand>,
    load_tx: watch::Sender<u64>,
) {
    let (mut sink, mut stream) = ws.split();
    let mut next_id: u64 = 0;
    let mut pending: HashMap<u64, oneshot::Sender<Result<Value, String>>> = HashMap::new();
    let mut fetch_retry: HashMap<u64, FetchRetry> = HashMap::new();
    loop {
        enum Item {
            Msg(Message),
            Cmd(CdpCommand),
        }
        let item = tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(message)) => Item::Msg(message),
                Some(Err(error)) => {
                    tracing::warn!("CDP WebSocket 读错误: {error}");
                    break;
                }
                None => break,
            },
            cmd = rx.recv() => match cmd {
                Some(cmd) => Item::Cmd(cmd),
                None => break,
            },
        };
        match item {
            Item::Cmd(cmd) => {
                next_id += 1;
                let id = next_id;
                pending.insert(id, cmd.resp);
                let frame = json!({ "id": id, "method": cmd.method, "params": cmd.params });
                if let Err(error) = sink.send(Message::Text(frame.to_string())).await {
                    tracing::warn!("CDP WebSocket 写失败: {error}");
                    break;
                }
            }
            Item::Msg(Message::Text(text)) => {
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(id) = value.get("id").and_then(Value::as_u64) {
                    let resp = pending.remove(&id);
                    let failed = value.get("error").is_some();
                    // 放行命令降级：失败且有剩余尝试 → 发下一种形态；形态全部
                    // 失败 → 原样放行兜底（不带改写头）。
                    if failed {
                        if let Some(FetchRetry {
                            request_id,
                            mut attempts,
                        }) = fetch_retry.remove(&id)
                        {
                            if let Some(params) = attempts.next() {
                                send_fire_and_forget(
                                    &mut sink, &mut next_id, &mut fetch_retry,
                                    &request_id, attempts, params,
                                )
                                .await;
                            } else {
                                send_fire_and_forget(
                                    &mut sink, &mut next_id, &mut fetch_retry,
                                    &request_id, std::vec::IntoIter::new(Vec::new()),
                                    json!({ "requestId": request_id }),
                                )
                                .await;
                            }
                            continue;
                        }
                    }
                    if let Some(resp) = resp {
                        let result = match value.get("error") {
                            Some(error) => Err(format!(
                                "CDP 错误: {}",
                                error
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("未知")
                            )),
                            None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = resp.send(result);
                    }
                    continue;
                }
                match value.get("method").and_then(Value::as_str).unwrap_or("") {
                    "Fetch.requestPaused" => {
                        handle_fetch_paused(
                            &mut sink,
                            &mut next_id,
                            &mut fetch_retry,
                            &value,
                        )
                        .await;
                    }
                    "Page.loadEventFenced" => {
                        load_tx.send_modify(|count| *count += 1);
                    }
                    _ => {}
                }
            }
            Item::Msg(Message::Ping(payload)) => {
                let _ = sink.send(Message::Pong(payload)).await;
            }
            Item::Msg(Message::Close(_)) => break,
            Item::Msg(_) => {}
        }
    }
    // pending 的 sender 随 manager 结束一起丢弃：等待方收到 oneshot 断开错误。
}

/// 发一条不等待应答的命令（应答到达时按不在 pending 的 id 丢弃；fetch_retry
/// 只为失败降级记录队列）。
#[cfg(unix)]
async fn send_fire_and_forget(
    sink: &mut futures::stream::SplitSink<WebSocketStream<tokio::net::UnixStream>, Message>,
    next_id: &mut u64,
    fetch_retry: &mut HashMap<u64, FetchRetry>,
    request_id: &str,
    remaining: std::vec::IntoIter<Value>,
    params: Value,
) {
    *next_id += 1;
    let id = *next_id;
    if !remaining.as_slice().is_empty() {
        fetch_retry.insert(
            id,
            FetchRetry {
                request_id: request_id.to_string(),
                attempts: remaining,
            },
        );
    }
    let frame = json!({ "id": id, "method": "Fetch.continueRequest", "params": params });
    if let Err(error) = sink.send(Message::Text(frame.to_string())).await {
        tracing::warn!("CDP WebSocket 写失败（Fetch 放行）: {error}");
    }
}

/// Fetch.requestPaused：改写头后放行。形态序列：HeaderEntry[]（CDP ≥ 126）→
/// object（旧版）→ 无头原样放行。
#[cfg(unix)]
#[cfg(unix)]
async fn handle_fetch_paused(
    sink: &mut futures::stream::SplitSink<WebSocketStream<tokio::net::UnixStream>, Message>,
    next_id: &mut u64,
    fetch_retry: &mut HashMap<u64, FetchRetry>,
    event: &Value,
) {
    let Some(request_id) = event
        .get("params")
        .and_then(|p| p.get("requestId"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let headers = event
        .get("params")
        .and_then(|p| p.get("request"))
        .and_then(|r| r.get("headers"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let stripped = strip_x_requested_with(&headers);
    let attempts = vec![
        json!({ "requestId": request_id, "headers": header_entries(&stripped) }),
        json!({ "requestId": request_id, "headers": stripped }),
    ];
    let mut iter = attempts.into_iter();
    let Some(first) = iter.next() else {
        return;
    };
    send_fire_and_forget(
        sink,
        next_id,
        fetch_retry,
        request_id,
        iter,
        first,
    )
    .await;
}

// ---------------------------------------------------------------------------
// 会话编排：确保 Java 侧 WebView + 连接 + 域启用
// ---------------------------------------------------------------------------

/// 只拦文档与页面自身发起的 API 请求（Google 风控看的就是这些）；图片/样式/
/// 脚本不进拦截器，不给页面加载添延迟。
#[cfg(any(unix, test))]
fn fetch_patterns() -> Value {
    json!([
        { "urlPattern": "*", "resourceType": "Document", "requestStage": "Request" },
        { "urlPattern": "*", "resourceType": "XHR", "requestStage": "Request" },
        { "urlPattern": "*", "resourceType": "Fetch", "requestStage": "Request" }
    ])
}

async fn connect_session() -> Result<Session, String> {
    #[cfg(not(unix))]
    {
        return Err("CDP 浏览器仅在 Android 宿主可用".to_string());
    }
    #[cfg(unix)]
    {
        let pid = tokio::task::spawn_blocking(crate::browser_session_start)
            .await
            .map_err(|e| format!("JNI 调度失败: {e}"))?
            .map_err(|e| e.to_string())?;
        let socket_name = format!("webview_devtools_remote_{pid}");
        let ws = socket_layer::connect_page_ws(&socket_name)
            .await
            .map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::channel(64);
        let (load_tx, load_rx) = watch::channel(0u64);
        tokio::spawn(run_connection(ws, rx, load_tx));
        let session = Session { tx, load_rx };
        session
            .call("Page.enable", json!({}), Duration::from_secs(10))
            .await?;
        session
            .call(
                "Fetch.enable",
                json!({ "patterns": fetch_patterns() }),
                Duration::from_secs(10),
            )
            .await?;
        Ok(session)
    }
}

// ---------------------------------------------------------------------------
// 共享内核：CdpBrowser
// ---------------------------------------------------------------------------

/// CDP 浏览器共享内核（web_browser 与 browser_* 工具共用）。
pub struct CdpBrowser {
    session: AsyncMutex<Option<Session>>,
}

impl Default for CdpBrowser {
    fn default() -> Self {
        Self::new()
    }
}

impl CdpBrowser {
    pub fn new() -> Self {
        CdpBrowser {
            session: AsyncMutex::new(None),
        }
    }

    /// 取（必要时重建）连接。断开自动重连——Java 侧 WebView 仍在，重连不重建。
    async fn session(&self) -> Result<Session, String> {
        let mut guard = self.session.lock().await;
        if let Some(existing) = guard.as_ref() {
            if !existing.tx.is_closed() {
                return Ok(existing.clone());
            }
            *guard = None;
        }
        let session = connect_session().await?;
        *guard = Some(session.clone());
        Ok(session)
    }

    /// 导航公共段：Page.navigate → 等 loadEventFenced → 静置 800ms。
    async fn navigate_wait(&self, url: &str, load_timeout: Duration) -> Result<Session, String> {
        let session = self.session().await?;
        let pre = *session.load_rx.borrow();
        session
            .call(
                "Page.navigate",
                json!({ "url": url }),
                Duration::from_secs(20),
            )
            .await?;
        session.wait_load(pre, load_timeout).await?;
        tokio::time::sleep(Duration::from_millis(800)).await;
        Ok(session)
    }

    /// 打开 URL 并等加载完成，返回人读报告（含 Google 风控页/同意页检测）。
    pub async fn navigate(&self, url: &str) -> Result<String, String> {
        let session = self.navigate_wait(url, Duration::from_secs(45)).await?;
        let info = self.page_info(&session).await?;
        Ok(format_report(&info))
    }

    /// web_browser 后端：导航 + 提取 JS（结果字段与原 HeadlessBrowser 一致）。
    pub async fn navigate_and_extract(
        &self,
        url: &str,
    ) -> Result<rclaw_tools::web_browser::PagePayload, String> {
        let session = self.navigate_wait(url, Duration::from_secs(40)).await?;
        let reply = session
            .call(
                "Runtime.evaluate",
                json!({ "expression": EXTRACT_JS, "returnByValue": true }),
                Duration::from_secs(15),
            )
            .await?;
        let value = evaluate_string(&reply)?;
        serde_json::from_str(&value).map_err(|e| format!("解析页面提取结果失败: {e}"))
    }

    /// 紧凑可交互元素清单（每项带 ref，供 browser_click / browser_type 定位）。
    pub async fn snapshot(&self) -> Result<String, String> {
        let session = self.session().await?;
        let reply = session
            .call(
                "Runtime.evaluate",
                json!({ "expression": SNAPSHOT_JS, "returnByValue": true }),
                Duration::from_secs(15),
            )
            .await?;
        let value = evaluate_string(&reply)?;
        let parsed: Value = serde_json::from_str(&value).map_err(|e| format!("{e}"))?;
        if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(parsed
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("页面快照失败")
                .to_string());
        }
        Ok(parsed
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    /// 点按 ref 元素（CDP 真实鼠标事件，非 JS 合成点击）。
    pub async fn click(&self, r: u32) -> Result<String, String> {
        let session = self.session().await?;
        let (x, y) = self.element_center(&session, r).await?;
        for kind in ["mousePressed", "mouseReleased"] {
            session
                .call(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": kind, "x": x, "y": y,
                        "button": "left", "buttons": 1, "clickCount": 1,
                    }),
                    Duration::from_secs(10),
                )
                .await?;
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        let info = self.page_info(&session).await?;
        Ok(format!(
            "已在 ({x},{y}) 点按 [ref {r}]。\n{}",
            format_report(&info)
        ))
    }

    /// 聚焦 ref 输入框并输入文本（先清空；InsertText 走 IME 通路，事件完整）。
    pub async fn type_text(&self, r: u32, text: &str) -> Result<String, String> {
        let session = self.session().await?;
        let (x, y) = self.element_center(&session, r).await?;
        for kind in ["mousePressed", "mouseReleased"] {
            session
                .call(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": kind, "x": x, "y": y,
                        "button": "left", "buttons": 1, "clickCount": 1,
                    }),
                    Duration::from_secs(10),
                )
                .await?;
        }
        let clear = format!(
            "(function(ref){{var el=document.querySelector('[data-rclaw-ref=\"'+ref+'\"]');if(!el)return JSON.stringify({{ok:false,error:'引用不存在'}});el.focus();el.value='';el.dispatchEvent(new Event('input',{{bubbles:true}}));return JSON.stringify({{ok:true}});}})({r})"
        );
        let reply = session
            .call(
                "Runtime.evaluate",
                json!({ "expression": clear, "returnByValue": true }),
                Duration::from_secs(10),
            )
            .await?;
        check_eval_ok(&reply)?;
        session
            .call(
                "Input.insertText",
                json!({ "text": text }),
                Duration::from_secs(10),
            )
            .await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        Ok(format!(
            "已在 [ref {r}] 输入 {} 个字符。",
            text.chars().count()
        ))
    }

    /// 离屏截图（Java View.draw 软件位图）到工作区 browser/ 下，返回相对路径。
    pub async fn screenshot(&self) -> Result<String, String> {
        let ctx = crate::host_context()
            .ok_or_else(|| "应用未就绪（宿主上下文未初始化）".to_string())?;
        // 确保会话活着（截图前没导航过也要有 WebView）。
        let _ = self.session().await?;
        let dir = ctx.files_dir.join("agent").join("browser");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| format!("创建截图目录失败: {e}"))?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let name = format!("screenshot_{stamp}.png");
        let abs = dir.join(&name);
        let abs_str = abs.to_string_lossy().into_owned();
        tokio::task::spawn_blocking(move || crate::browser_session_screenshot(&abs_str))
            .await
            .map_err(|e| format!("JNI 调度失败: {e}"))?
            .map_err(|e| e.to_string())?;
        Ok(format!(
            "已截图：browser/{name}（工作区内，可用 read 读取或 openMedia 查看）"
        ))
    }

    /// 销毁会话（下次任何 browser 工具使用时自动重建）。
    pub async fn close(&self) -> Result<String, String> {
        {
            let mut guard = self.session.lock().await;
            *guard = None;
        }
        tokio::task::spawn_blocking(crate::browser_session_stop)
            .await
            .map_err(|e| format!("JNI 调度失败: {e}"))?
            .map_err(|e| e.to_string())?;
        Ok("浏览器会话已关闭（资源已释放，下次使用自动重建）。".to_string())
    }

    async fn element_center(&self, session: &Session, r: u32) -> Result<(i64, i64), String> {
        let js = format!(
            "(function(ref){{var el=document.querySelector('[data-rclaw-ref=\"'+ref+'\"]');if(!el)return JSON.stringify({{ok:false,error:'引用 #'+ref+' 不存在（页面可能已跳转，请重新 browser_snapshot）'}});el.scrollIntoView({{block:'center'}});var box=el.getBoundingClientRect();if(box.width===0&&box.height===0)return JSON.stringify({{ok:false,error:'元素无尺寸（不可见）'}});return JSON.stringify({{ok:true,x:Math.round(box.left+box.width/2),y:Math.round(box.top+box.height/2)}});}})({r})"
        );
        let reply = session
            .call(
                "Runtime.evaluate",
                json!({ "expression": js, "returnByValue": true }),
                Duration::from_secs(10),
            )
            .await?;
        let value = evaluate_string(&reply)?;
        let parsed: Value = serde_json::from_str(&value).map_err(|e| format!("{e}"))?;
        if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(parsed
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("定位元素失败")
                .to_string());
        }
        Ok((
            parsed.get("x").and_then(Value::as_i64).unwrap_or(0),
            parsed.get("y").and_then(Value::as_i64).unwrap_or(0),
        ))
    }

    async fn page_info(&self, session: &Session) -> Result<PageInfo, String> {
        let reply = session
            .call(
                "Runtime.evaluate",
                json!({ "expression": PAGE_INFO_JS, "returnByValue": true }),
                Duration::from_secs(15),
            )
            .await?;
        let value = evaluate_string(&reply)?;
        serde_json::from_str(&value).map_err(|e| format!("解析页面信息失败: {e}"))
    }
}

/// Runtime.evaluate 的应答里取字符串值（有异常则报错）。
fn evaluate_string(reply: &Value) -> Result<String, String> {
    if let Some(exception) = reply.get("exceptionDetails") {
        let text = exception
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(Value::as_str)
            .or_else(|| exception.get("text").and_then(Value::as_str))
            .unwrap_or("未知脚本异常");
        return Err(format!("页面脚本异常: {text}"));
    }
    reply
        .get("result")
        .and_then(|r| r.get("result"))
        .and_then(|r| r.get("value"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "页面脚本无返回".to_string())
}

/// `{ok:true}` 形态的脚本应答校验。
fn check_eval_ok(reply: &Value) -> Result<(), String> {
    let value = evaluate_string(reply)?;
    let parsed: Value = serde_json::from_str(&value).map_err(|e| format!("{e}"))?;
    if parsed.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    Err(parsed
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("脚本执行失败")
        .to_string())
}

#[derive(serde::Deserialize)]
struct PageInfo {
    url: String,
    title: String,
    #[serde(default)]
    blocked: String,
}

fn format_report(info: &PageInfo) -> String {
    let mut out = format!("已打开：{}\n标题：{}", info.url, info.title);
    if !info.blocked.is_empty() {
        let reason = &info.blocked;
        out.push_str(&format!(
            "\n\n⚠️ 检测到{reason}——这是 Google 的风控/同意页，非程序故障。风控页（CAPTCHA）\
             与出口 IP 信誉相关，立刻重试通常加重；同意页可 browser_snapshot 后点按勾选框继续。"
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// 页面脚本常量
// ---------------------------------------------------------------------------

/// 页面信息（URL / 标题 / 风控与同意页检测）。
const PAGE_INFO_JS: &str = "(function(){var href=location.href||'';var text=(document.body&&document.body.innerText)||'';var blocked='';if(/google\\.[a-z.]+\\/sorry/i.test(href)||/unusual traffic/i.test(text))blocked='Google CAPTCHA 风控页（/sorry/）';else if(/^consent\\./.test(location.hostname)||/before you continue/i.test(text))blocked='同意页（consent）';return JSON.stringify({url:href,title:document.title||'',blocked:blocked});})()";

/// 紧凑可交互元素清单：给每个可交互/标题元素盖 data-rclaw-ref 章，输出
/// `[ref] 角色: 文本 → 链接` 扁平列表（模型按 ref 定位操作）。
const SNAPSHOT_JS: &str = "(function(){function clean(s){return (s||'').replace(/\\s+/g,' ').trim();}var out=[];var n=0;var roles={a:'link',button:'button',input:'textbox',textarea:'textbox',select:'combobox',h1:'heading',h2:'heading',h3:'heading',img:'img'};var nodes=document.querySelectorAll('a,button,input,textarea,select,[role],[onclick],h1,h2,h3');for(var i=0;i<nodes.length&&out.length<250;i++){var el=nodes[i];var tag=el.tagName.toLowerCase();var role=el.getAttribute('role')||roles[tag]||'';var text=tag==='input'||tag==='textarea'?(el.value||el.placeholder||''):clean(el.innerText||'');var href=el.href||'';if(!text&&!href)continue;var ref=++n;try{el.setAttribute('data-rclaw-ref',ref);}catch(e){}var line='['+ref+'] '+role+': '+text.slice(0,120);if(href)line+=' → '+String(href).slice(0,150);out.push(line);}return JSON.stringify({ok:out.length>0,text:out.length?('共 '+out.length+' 项（截前 250）\\n'+out.join('\\n')):'页面无可交互元素'});})()";

/// 页面提取（web_browser）：与原 HeadlessBrowser 提取 JS 同口径——引擎感知的
/// 搜索结果列表（bing/baidu/google，过滤广告）+ Readability 式主文正文。
/// UA 是真实移动 WebView：google 分支的摘要选择器加 gbox 兜底（移动版没有
/// 桌面版的 .VwiC3b 时取整个结果容器）。
const EXTRACT_JS: &str = r##"(function () {
  var out = { title: document.title || "", url: location.href, text: "", results: [] };
  function clean(s) { return (s || "").replace(/\s+/g, " ").trim(); }
  function adMarked(el) {
    return !!el.querySelector("[class*='ec-'], [class*='ec_'], [class*='tuiguang'], [class*='b_ad']");
  }
  var host = location.hostname || "";
  var seen = {};
  function safeDecode(s) { try { return decodeURIComponent(s); } catch (e) { return ""; } }
  function push(title, url, snippet) {
    if (!url || seen[url]) return;
    seen[url] = true;
    out.results.push({ title: clean(title).slice(0, 120), url: url, snippet: clean(snippet).slice(0, 240) });
  }
  if (host.indexOf("bing.com") >= 0) {
    var items = document.querySelectorAll("#b_results li.b_algo");
    for (var i = 0; i < items.length && out.results.length < 10; i++) {
      var li = items[i];
      if (adMarked(li)) continue;
      var a = li.querySelector("h2 a");
      if (!a) continue;
      var cap = li.querySelector(".b_caption p, .b_algoSlug, .b_caption");
      push(a.textContent, a.href, cap ? cap.textContent : "");
    }
  } else if (host.indexOf("google.") >= 0) {
    var links = document.querySelectorAll("a h3");
    for (var g = 0; g < links.length && out.results.length < 10; g++) {
      var gh = links[g];
      var ga = gh.closest("a");
      if (!ga || !ga.href) continue;
      var gurl = ga.href;
      if (gurl.indexOf("/aclk") >= 0 || gurl.indexOf("googleadservices") >= 0) continue;
      var gm = gurl.match(/^.*\/url\?q=([^&]+)/);
      if (gm) { gurl = safeDecode(gm[1]); if (!gurl) continue; }
      if (gurl.indexOf("http") !== 0) continue;
      var gbox = gh.closest("div.g") || gh.parentElement;
      var gsnip = gbox ? (gbox.querySelector(".VwiC3b") || gbox) : null;
      push(gh.textContent, gurl, gsnip ? gsnip.textContent : "");
    }
  } else if (host.indexOf("baidu.com") >= 0) {
    var blinks = document.querySelectorAll("#content_left h3 a");
    for (var j = 0; j < blinks.length && out.results.length < 10; j++) {
      var a2 = blinks[j];
      var box = a2.closest(".result, .result-op, .c-container");
      if (!box || adMarked(box)) continue;
      if (/e\.baidu\.com|promotion\.baidu\.com/.test(a2.href)) continue;
      var abs = box.querySelector(".c-abstract, [class*=content-right]");
      push(a2.textContent, a2.href, abs ? abs.textContent : "");
    }
  }
  function boilerplate(el) {
    var sig = ((el.className && el.className.toString()) || "") + " " + (el.id || "");
    return /(nav|menu|footer|sidebar|comment|advert|ads?-|promo|banner|share|breadcrumb|copyright|related|recommend)/i.test(sig);
  }
  var nodes = document.querySelectorAll("article, main, [role=main]");
  if (!nodes.length) nodes = document.querySelectorAll("div, section");
  var best = null, bestScore = 0;
  for (var k = 0; k < nodes.length && k < 800; k++) {
    var node = nodes[k];
    if (boilerplate(node)) continue;
    var ps = node.querySelectorAll("p");
    var score = 0;
    for (var m = 0; m < ps.length && m < 200; m++) {
      score += Math.min(ps[m].textContent.length, 1000);
    }
    if (score > bestScore) { bestScore = score; best = node; }
  }
  var root = (best && bestScore > 200) ? best : document.body;
  var body = "";
  if (root) {
    body = root.innerText || root.textContent || "";
  }
  out.text = body.replace(/[ \t]+\n/g, "\n").replace(/\n{3,}/g, "\n\n").trim().slice(0, 20000);
  return JSON.stringify(out);
})()"##;

// ---------------------------------------------------------------------------
// 工具面
// ---------------------------------------------------------------------------

/// 把 browser_* 交互工具挂进注册表（全部共享 [`shared`] 的同一会话内核）。
/// web_browser 后端的替换在 `crate::web_browser::register`。
pub fn register_tools(tools: &mut rclaw_tools::ToolRegistry) {
    let browser = shared();
    tools.register(Box::new(BrowserNavigateTool { browser }));
    tools.register(Box::new(BrowserSnapshotTool { browser }));
    tools.register(Box::new(BrowserClickTool { browser }));
    tools.register(Box::new(BrowserTypeTool { browser }));
    tools.register(Box::new(BrowserScreenshotTool { browser }));
    tools.register(Box::new(BrowserCloseTool { browser }));
}

/// web_browser 的 CDP 后端：导航 + 提取（`PageLoader` 是阻塞 trait，调用方已在
/// spawn_blocking 里，这里 block_on 进异步会话）。
pub struct CdpPageLoader {
    browser: &'static CdpBrowser,
}

impl rclaw_tools::web_browser::PageLoader for CdpPageLoader {
    fn load(&self, url: &str) -> Result<rclaw_tools::web_browser::PagePayload, String> {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(self.browser.navigate_and_extract(url))
    }
}

/// URL 归一：无 scheme 补 https://；只收 http(s)。
fn normalize_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("URL 不能为空".to_string());
    }
    if url.contains("://") && !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!("仅支持 http(s) URL: {url}"));
    }
    Ok(if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("https://{url}")
    })
}

/// 工具样板：name / description / 串行安全位（共享长驻会话，保守不并发）+
/// input_schema（各工具自带 `input_schema_impl`）。
macro_rules! browser_tool_impl {
    ($name:ident, $tool_name:literal, $doc:expr, $execute:item) => {
        #[async_trait::async_trait]
        impl Tool for $name {
            fn name(&self) -> &str {
                $tool_name
            }
            fn description(&self) -> &str {
                $doc
            }
            fn input_schema(&self) -> Value {
                Self::input_schema_impl()
            }
            fn parallel_safe(&self) -> bool {
                false
            }
            $execute
        }
    };
}

pub struct BrowserNavigateTool {
    browser: &'static CdpBrowser,
}

browser_tool_impl!(
    BrowserNavigateTool,
    "browser_navigate",
    "CDP 浏览器：打开 URL 并等待加载完成（Android 应用内 WebView 真实内核；请求头已去自动化指纹，可正常访问 Google）。返回页面标题与 URL，检测到 Google 风控页（CAPTCHA）/同意页时明确标注。只取正文用 web_browser 即可；需要交互（点击/输入/截图）用 browser_* 系列。",
    async fn execute(&self, input: Value) -> rclaw_tools::ToolResult {
        let url = match input.get("url").and_then(Value::as_str) {
            Some(url) => url,
            None => return rclaw_tools::ToolResult::err("缺少参数 url"),
        };
        match normalize_url(url) {
            Ok(url) => match self.browser.navigate(&url).await {
                Ok(report) => rclaw_tools::ToolResult::ok(report),
                Err(error) => rclaw_tools::ToolResult::err(error),
            },
            Err(error) => rclaw_tools::ToolResult::err(error),
        }
    }
);

impl BrowserNavigateTool {
    fn input_schema_impl() -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "目标页面 URL（无 scheme 自动补 https://）" }
            },
            "required": ["url"]
        })
    }
}

pub struct BrowserSnapshotTool {
    browser: &'static CdpBrowser,
}

browser_tool_impl!(
    BrowserSnapshotTool,
    "browser_snapshot",
    "CDP 浏览器：列出当前页面可交互元素（链接/按钮/输入框/标题），每项带 [ref] 编号。browser_click / browser_type 按 ref 定位。页面跳转后 ref 失效，需重新快照。",
    async fn execute(&self, _input: Value) -> rclaw_tools::ToolResult {
        match self.browser.snapshot().await {
            Ok(text) => rclaw_tools::ToolResult::ok(text),
            Err(error) => rclaw_tools::ToolResult::err(error),
        }
    }
);

impl BrowserSnapshotTool {
    fn input_schema_impl() -> Value {
        json!({ "type": "object", "properties": {} })
    }
}

pub struct BrowserClickTool {
    browser: &'static CdpBrowser,
}

browser_tool_impl!(
    BrowserClickTool,
    "browser_click",
    "CDP 浏览器：点按 snapshot 给出的 [ref] 元素（真实鼠标事件，非 JS 合成点击）。返回点按位置与当前页面标题/URL。",
    async fn execute(&self, input: Value) -> rclaw_tools::ToolResult {
        let Some(r) = input.get("ref").and_then(Value::as_u64) else {
            return rclaw_tools::ToolResult::err("缺少参数 ref（browser_snapshot 列表里的编号）");
        };
        match u32::try_from(r) {
            Ok(r) => match self.browser.click(r).await {
                Ok(report) => rclaw_tools::ToolResult::ok(report),
                Err(error) => rclaw_tools::ToolResult::err(error),
            },
            Err(_) => rclaw_tools::ToolResult::err("ref 超出范围"),
        }
    }
);

impl BrowserClickTool {
    fn input_schema_impl() -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref": { "type": "integer", "description": "browser_snapshot 列表里的 [ref] 编号" }
            },
            "required": ["ref"]
        })
    }
}

pub struct BrowserTypeTool {
    browser: &'static CdpBrowser,
}

browser_tool_impl!(
    BrowserTypeTool,
    "browser_type",
    "CDP 浏览器：聚焦 [ref] 输入框，先清空原内容再输入文本（IME 通路，事件完整；适合搜索框/表单）。",
    async fn execute(&self, input: Value) -> rclaw_tools::ToolResult {
        let (Some(r), Some(text)) = (
            input.get("ref").and_then(Value::as_u64),
            input.get("text").and_then(Value::as_str),
        ) else {
            return rclaw_tools::ToolResult::err("缺少参数 ref / text");
        };
        match u32::try_from(r) {
            Ok(r) => match self.browser.type_text(r, text).await {
                Ok(report) => rclaw_tools::ToolResult::ok(report),
                Err(error) => rclaw_tools::ToolResult::err(error),
            },
            Err(_) => rclaw_tools::ToolResult::err("ref 超出范围"),
        }
    }
);

impl BrowserTypeTool {
    fn input_schema_impl() -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref": { "type": "integer", "description": "browser_snapshot 列表里的 [ref] 编号（输入框）" },
                "text": { "type": "string", "description": "要输入的文本（输入前先清空原内容）" }
            },
            "required": ["ref", "text"]
        })
    }
}

pub struct BrowserScreenshotTool {
    browser: &'static CdpBrowser,
}

browser_tool_impl!(
    BrowserScreenshotTool,
    "browser_screenshot",
    "CDP 浏览器：对当前页面离屏截图（PNG，存工作区 browser/ 下）。返回文件路径，可用 read 读图或 openMedia 分享。",
    async fn execute(&self, _input: Value) -> rclaw_tools::ToolResult {
        match self.browser.screenshot().await {
            Ok(report) => rclaw_tools::ToolResult::ok(report),
            Err(error) => rclaw_tools::ToolResult::err(error),
        }
    }
);

impl BrowserScreenshotTool {
    fn input_schema_impl() -> Value {
        json!({ "type": "object", "properties": {} })
    }
}

pub struct BrowserCloseTool {
    browser: &'static CdpBrowser,
}

browser_tool_impl!(
    BrowserCloseTool,
    "browser_close",
    "CDP 浏览器：销毁长驻 WebView 会话并释放内存（下次使用任一 browser_* 工具自动重建）。",
    async fn execute(&self, _input: Value) -> rclaw_tools::ToolResult {
        match self.browser.close().await {
            Ok(report) => rclaw_tools::ToolResult::ok(report),
            Err(error) => rclaw_tools::ToolResult::err(error),
        }
    }
);

impl BrowserCloseTool {
    fn input_schema_impl() -> Value {
        json!({ "type": "object", "properties": {} })
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
#[cfg(all(test, unix))]
    fn strip_removes_requested_with_case_insensitively_and_keeps_rest() {
        let headers = json!({
            "User-Agent": "Mozilla/5.0",
            "x-requested-with": "ai.rclaw.app",
            "Accept-Language": "zh-CN",
        });
        let stripped = strip_x_requested_with(&headers);
        let map = stripped.as_object().unwrap();
        assert_eq!(map.len(), 2);
        assert!(map.get("User-Agent").is_some());
        assert!(map.get("Accept-Language").is_some());
        assert!(map
            .keys()
            .all(|k| !k.eq_ignore_ascii_case("X-Requested-With")));
    }

    #[test]
#[cfg(all(test, unix))]
    fn strip_passes_through_missing_headers_object() {
        assert_eq!(strip_x_requested_with(&Value::Null), json!({}));
    }

    #[test]
#[cfg(all(test, unix))]
    fn header_entries_map_to_name_value_pairs() {
        let entries = header_entries(&json!({ "A": "1", "B": "2" }));
        let list = entries.as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert!(list
            .iter()
            .all(|e| e.get("name").is_some() && e.get("value").is_some()));
    }

    #[test]
#[cfg(all(test, unix))]
    fn pick_page_target_skips_non_page_entries() {
        let body = r#"[
            {"description":"","devtoolsFrontendUrl":"x","id":"SW1","type":"service_worker","url":"https://sw"},
            {"description":"","id":"PAGE1","type":"page","title":"Google","url":"https://www.google.com/"},
            {"description":"","id":"PAGE2","type":"page","title":"Other","url":"https://other/"}
        ]"#;
        assert_eq!(super::pick_page_target(body).unwrap(), "PAGE1");
    }

    #[test]
#[cfg(all(test, unix))]
    fn pick_page_target_errors_when_no_page() {
        let body = r#"[{"id":"SW1","type":"service_worker","url":"https://sw"}]"#;
        assert!(super::pick_page_target(body).is_err());
        assert!(super::pick_page_target("not json").is_err());
    }

    #[test]
    fn normalize_url_adds_scheme_and_rejects_non_http() {
        assert_eq!(
            normalize_url("www.google.com/search?q=1").unwrap(),
            "https://www.google.com/search?q=1"
        );
        assert_eq!(
            normalize_url("http://example.com").unwrap(),
            "http://example.com"
        );
        assert!(normalize_url("ftp://x").is_err());
        assert!(normalize_url("  ").is_err());
    }
}
