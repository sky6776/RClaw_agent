//! CDP 浏览器共享内核：协议层 + 传输抽象 + 工具面（桌面 / 安卓共用）。
//!
//! 传输两实现（[`DevtoolsTransport`]）：
//! - 桌面 [`HeadlessChromiumTransport`]：拉起本机 Chrome/Edge 无头实例
//!   （`--headless --remote-debugging-port=0`），经 TCP DevTools 端口驱动——
//!   与 Playwright 的 cdp 后端同源机制，但不需要 Node 运行时；
//! - 安卓（rclaw-android::cdp_browser::AndroidTransport）：长驻调试 WebView，
//!   经 abstract unix socket 直连。
//!
//! Google 拒绝的根因治理 + 拟人化（反爬优先策略）：
//! - 会话建立：UA 清洗覆写（桌面去 `HeadlessChrome`、安卓去 WebView `; wv`，
//!   带一致性 Client Hints 元数据）；`Emulation.setFocusEmulationEnabled` +
//!   stealth 注入（`navigator.webdriver` 掩蔽、`document.hasFocus()` 前台化、
//!   `window.chrome` shim、PDF Viewer 三件套 plugins/mimeTypes——均为真机
//!   bot.sannysoft 实测暴露项；WebGL 渲染器实测已是真 GPU【Adreno 616，
//!   离屏 + LAYER_TYPE_SOFTWARE 不影响 WebGL 的 GL 上下文】，不做 hook，
//!   canvas 噪声/字体/AudioContext 指纹破坏正常页面或性价比低，明确不做）；
//! - `Fetch` 拦截摘掉 WebView 网络栈强加的 `X-Requested-With`（应用包名指纹）；
//! - 交互事件词汇随 UA 与挂载态分流（真机 vivo WebView83 实测）：移动端
//!   attached（面板展开）用 `Input.synthesize*` 合成手势（tap/scroll，真实
//!   手势管线 → trusted 输入）；离屏（面板收起、合成器不活跃）一切注入事件
//!   ack 照常但滚动/点击零效果——滚动/点击改走页内 JS（scrollBy / click，
//!   isTrusted=false），输入的 insertText/键盘通路不受影响；桌面保持鼠标/
//!   滚轮。`Input.dispatchTouchEvent` 在本机 attached/离屏都不可靠（拖拽链
//!   动辄秒级 ack 甚至超时），已弃用；鼠标滚轮在手机上不作为回退（非人形
//!   输入且离屏无效）；
//! - Cookie 由真实网络栈跨调用保持（Java 侧 teardown 时 `CookieManager.flush`
//!   落盘，进程被杀不丢已养的风控信誉）。
//!
//! Google 风控页（CAPTCHA）park/long-poll：browser_navigate 命中风控页时本次
//! 调用不返回——外呼 transport 通知用户（安卓：高优通知 + 悬浮窗展开人工过码；
//! 桌面：无外呼），然后 select 三臂等待：1.5s 轮询页面恢复（用户过完码 Google
//! 自动跳回，无需 Java→Rust 信号）/ 180s 超时标注返回 / 连接断开（悬浮窗「关闭
//! 会话」）报取消。恢复后重新提取，agent 同回合无缝继续。
//!
//! 检索不设专用工具：模型直接 navigate 到任意搜索引擎的结果页 URL
//! （EXTRACT_JS 按主机名解析 SERP 并过滤广告），引擎自由选择。
//!
//! 截图：安卓按悬浮窗挂载状态分流——attached 有真实合成器帧走
//! `Page.captureScreenshot`，离屏走 `View.draw` 软件位图（CDP 截图空白）；
//! 桌面无头实例直接 CDP 截图。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch, Mutex as AsyncMutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::{Tool, ToolResult};

/// open 动作返回正文的字符上限（保上下文预算）。
const PAGE_TEXT_MAX_CHARS: usize = 6_000;

/// 页面提取结果（提取 JS 的返回契约：字段一一对应）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct PagePayload {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub results: Vec<SearchEntry>,
    /// 风控/同意页标注（提取 JS 填写；页面正常时为空串）。
    #[serde(default)]
    pub blocked: String,
    /// park 过码等待的结果说明（Rust 侧填写；非提取 JS 产出，未 park 为空）。
    #[serde(default)]
    pub park_note: String,
}

/// SERP 单条结果（提取 JS 解析并过滤广告后产出）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct SearchEntry {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub snippet: String,
}

/// 单行化：进度/结果行不允许内嵌换行破坏格式。
fn single_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// 按字符截断（CJK 安全），超长补省略说明。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars).collect();
    truncated.push_str(&format!(
        "\n\n（正文共 {count} 字符，已截断至 {max_chars}）"
    ));
    truncated
}

/// 装箱后的 DevTools 原始流（unix socket / tcp 统一形态；WS 握手前交给
/// 协议层，避免 WebSocketStream 的类型参数分裂）。
/// AsyncRead + AsyncWrite + Unpin + Send 的合并（trait object 只允许一个
/// 非 auto trait，聚合后再 Box）。
pub trait RawDuplex: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> RawDuplex for T {}

pub type RawStream = Box<dyn RawDuplex>;

/// 握手完成后的 CDP 连接。
pub type Ws = WebSocketStream<RawStream>;

/// `/json/list` 探测请求（Connection: close，一发一收读到 EOF）。
const HTTP_JSON_LIST: &[u8] =
    b"GET /json/list HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

/// `/json/list` 应答里挑 page target。调试开关会暴露同进程的其它 WebView
/// （安卓的 Tauri 主界面 / 桌面的扩展页）：按 URL 排除应用界面
/// （tauri/file/data/devtools），优先 about:blank（会话初始态）。
fn pick_page_target(body: &str) -> Result<String, String> {
    let targets: Vec<Value> =
        serde_json::from_str(body).map_err(|e| format!("解析 /json/list 失败: {e}"))?;
    let mut fallback: Option<String> = None;
    for target in &targets {
        if target.get("type").and_then(Value::as_str) != Some("page") {
            continue;
        }
        let url = target.get("url").and_then(Value::as_str).unwrap_or("");
        let is_app_ui = url.starts_with("tauri://")
            || url.starts_with("http://tauri.localhost")
            || url.starts_with("https://tauri.localhost")
            || url.starts_with("file://")
            || url.starts_with("data:")
            || url.starts_with("devtools://")
            || url.starts_with("about:tauri");
        if is_app_ui {
            continue;
        }
        if url == "about:blank" {
            return target
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "page target 缺少 id".to_string());
        }
        if fallback.is_none() {
            fallback = target.get("id").and_then(Value::as_str).map(str::to_string);
        }
    }
    fallback.ok_or_else(|| "DevTools 没有可用的 page target（应用界面除外）".to_string())
}

/// 对已连上的 DevTools 流做 `/json/list` 探测，返回 page target id。
/// 实测 Chromium 的 DevTools HTTP 服务是 keep-alive（`Connection: close`
/// 也不断开），不能等 EOF——按 Content-Length 读满即认为应答完成。
pub async fn probe_page_target<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
) -> Result<String, String> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    stream
        .write_all(HTTP_JSON_LIST)
        .await
        .map_err(|e| format!("探测请求失败: {e}"))?;
    let mut buf: Vec<u8> = Vec::new();
    let read_all = async {
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let content_length = String::from_utf8_lossy(&buf[..at])
                    .split('\n')
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok());
                match content_length {
                    Some(len) if at + 4 + len <= buf.len() => return Ok(()),
                    Some(_) => {}          // 响应体未读满，继续收
                    None => return Ok(()), // 无长度声明：读到哪算哪（服务关流即完）
                }
            }
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    };
    tokio::time::timeout(Duration::from_secs(5), read_all)
        .await
        .map_err(|_| "探测超时".to_string())?
        .map_err(|e: std::io::Error| format!("探测读取失败: {e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let body = text
        .split("\r\n\r\n")
        .nth(1)
        .ok_or_else(|| "DevTools HTTP 应答缺少响应体".to_string())?;
    pick_page_target(body)
}

/// 在原始流上做 WebSocket 握手，连到指定 page target。
async fn ws_over(stream: RawStream, target: &str) -> Result<Ws, String> {
    let request: tokio_tungstenite::tungstenite::handshake::client::Request =
        format!("ws://localhost/devtools/page/{target}")
            .into_client_request()
            .map_err(|e| format!("构造 CDP WebSocket 请求失败: {e}"))?;
    // 握手无响应（内核卡死）不能无限等。
    let (ws, response) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::client_async(request, stream),
    )
    .await
    .map_err(|_| "CDP WebSocket 握手超时".to_string())?
    .map_err(|e| format!("CDP WebSocket 握手失败: {e}"))?;
    tracing::debug!(status = %response.status(), "CDP WebSocket 已握手");
    Ok(ws)
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

/// 会话句柄：clone 共用同一条连接（pub 因传输 trait 的截图方法需要它；
/// 字段私有，外部只能透传）。
#[derive(Clone)]
pub struct Session {
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

    /// 等待一次新的页面加载完成（`Page.loadEventFired` 计数超过 `after`）。
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
struct FetchRetry {
    request_id: String,
    attempts: std::vec::IntoIter<Value>,
}

/// manager：持有 WebSocket 写半边，串行发命令、按 id 回应答、处理事件。
/// `Page.loadEventFired` 驱动 load 计数（wait_load 等它）；Fetch 拦截的
/// 放行命令失败时按 attempts 降级，最终以无头放行兜底——宁可带上指纹也不让
/// 页面卡死在拦截器里。
async fn run_connection(ws: Ws, mut rx: mpsc::Receiver<CdpCommand>, load_tx: watch::Sender<u64>) {
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
                    // 放行应答（无论成败）都先清跟踪记录——成功即完事；失败按
                    // 剩余形态降级：数组形态 → object 形态 → 原样放行兜底
                    // （兜底本身不再跟踪，它是终态）。
                    if let Some(FetchRetry {
                        request_id,
                        mut attempts,
                    }) = fetch_retry.remove(&id)
                    {
                        if failed {
                            if let Some(params) = attempts.next() {
                                send_fire_and_forget(
                                    &mut sink,
                                    &mut next_id,
                                    &mut fetch_retry,
                                    &request_id,
                                    attempts,
                                    params,
                                )
                                .await;
                                continue;
                            }
                            next_id += 1;
                            let frame = json!({
                                "id": next_id,
                                "method": "Fetch.continueRequest",
                                "params": { "requestId": request_id },
                            });
                            if let Err(error) = sink.send(Message::Text(frame.to_string())).await {
                                tracing::warn!("CDP WebSocket 写失败（放行兜底）: {error}");
                                break;
                            }
                            continue;
                        }
                        // 放行成功：记录已清，无事可做。
                        continue;
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
                        handle_fetch_paused(&mut sink, &mut next_id, &mut fetch_retry, &value)
                            .await;
                    }
                    "Page.loadEventFired" => {
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

/// 发一条不等待应答的命令。无论有无剩余形态都登记跟踪——应答（成功或失败）
/// 都要清记录，长期页面高频 XHR 不清理就是无界增长。
async fn send_fire_and_forget(
    sink: &mut futures::stream::SplitSink<Ws, Message>,
    next_id: &mut u64,
    fetch_retry: &mut HashMap<u64, FetchRetry>,
    request_id: &str,
    remaining: std::vec::IntoIter<Value>,
    params: Value,
) {
    *next_id += 1;
    let id = *next_id;
    fetch_retry.insert(
        id,
        FetchRetry {
            request_id: request_id.to_string(),
            attempts: remaining,
        },
    );
    let frame = json!({ "id": id, "method": "Fetch.continueRequest", "params": params });
    if let Err(error) = sink.send(Message::Text(frame.to_string())).await {
        tracing::warn!("CDP WebSocket 写失败（Fetch 放行）: {error}");
    }
}

/// Fetch.requestPaused：改写头后放行。形态序列：HeaderEntry[]（CDP ≥ 126）→
/// object（旧版）→ 无头原样放行。
async fn handle_fetch_paused(
    sink: &mut futures::stream::SplitSink<Ws, Message>,
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
    send_fire_and_forget(sink, next_id, fetch_retry, request_id, iter, first).await;
}

// ---------------------------------------------------------------------------
// 传输抽象 + 桌面实现
// ---------------------------------------------------------------------------

/// 只拦文档与页面自身发起的 API 请求（Google 风控看的就是这些）；图片/样式/
/// 脚本不进拦截器，不给页面加载添延迟。
fn fetch_patterns() -> Value {
    json!([
        { "urlPattern": "*", "resourceType": "Document", "requestStage": "Request" },
        { "urlPattern": "*", "resourceType": "XHR", "requestStage": "Request" },
        { "urlPattern": "*", "resourceType": "Fetch", "requestStage": "Request" }
    ])
}

/// DevTools 传输：确保浏览器内核在跑，交出「已连到 DevTools 服务器、完成
/// /json/list 探测」的原始流 + page target id（WS 握手由协议层统一做）。
#[async_trait::async_trait]
pub trait DevtoolsTransport: Send + Sync {
    async fn connect(&self) -> Result<(RawStream, String), String>;
    /// 销毁内核（browser_close；安卓销 WebView、桌面杀无头进程）。
    async fn shutdown(&self) -> Result<(), String>;
    /// 截图落点目录（工作区内，如 <agent>/browser）。
    fn screenshot_dir(&self) -> std::path::PathBuf;
    /// 截图到 dest（PNG）。安卓按悬浮窗挂载状态分流（attached→CDP capture /
    /// 离屏→View.draw），桌面走 Page.captureScreenshot。返回人读结果文案。
    async fn capture_screenshot(
        &self,
        session: &Session,
        dest: &std::path::Path,
    ) -> Result<String, String>;
    /// Google 风控页命中时的宿主外呼（安卓：高优通知 + 自动展开悬浮窗供人工
    /// 过码；桌面无悬浮窗能力，空实现——park 仍等待 reCAPTCHA 自动放行/超时）。
    async fn notify_captcha(&self, _url: &str) {}
    /// 宿主设备的真实 CSS 视口（宽, 高, dpr）：会话建立时用
    /// Emulation.setDeviceMetricsOverride 钉住渲染视口，使 innerWidth/innerHeight
    /// 与真机 Chrome Mobile 一致。桌面无头实例窗口自洽，返回 None 跳过。
    fn device_metrics(&self) -> Option<(i64, i64, f64)> {
        None
    }
    /// WebView 是否挂载在悬浮窗面板里（attached = 有真实窗口 + 活跃合成器）。
    /// 移动端输入分流依据：attached 态合成手势（synthesize*）真实生效；离屏
    /// 态一切注入事件 ack 照常但滚动/点击零效果（真机实测：合成器不活跃），
    /// 须走 JS 兜底。桌面无此概念，默认 false（桌面不查此标志）。
    async fn overlay_attached(&self) -> bool {
        false
    }
}

/// CDP `Page.captureScreenshot` → PNG 落盘（传输双轨的 CDP 轨：桌面无头实例
/// 与安卓悬浮窗 attached 态都有真实合成器帧）。返回 base64 解码字节数。
pub async fn capture_screenshot_via_cdp(
    session: &Session,
    dest: &std::path::Path,
) -> Result<usize, String> {
    let reply = session
        .call(
            "Page.captureScreenshot",
            json!({ "format": "png" }),
            Duration::from_secs(20),
        )
        .await?;
    let data = reply
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| "截图无数据（离屏渲染未就绪？）".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| format!("解码截图失败: {e}"))?;
    tokio::fs::write(dest, &bytes)
        .await
        .map_err(|e| format!("写入截图失败: {e}"))?;
    Ok(bytes.len())
}

/// 桌面端传输：拉起本机 Chrome/Edge 实例，经 TCP DevTools 端口驱动。
/// 浏览器可执行文件按 zcode cdp 后端同款的候选路径表解析，可用
/// `RCLAW_BROWSER_EXE` 覆盖。两种形态：无头（默认，agent 专用）与可见
/// （`set_visible(true)`：真实浏览器窗口，用户手动上网与 agent 共用同一
/// 实例/会话——「像 Firefox 一样上网」的桌面形态；切换经 close + 重连生效）。
pub struct HeadlessChromiumTransport {
    inner: AsyncMutex<Option<RunningChromium>>,
    screenshot_dir: std::path::PathBuf,
    profile_dir: std::path::PathBuf,
    /// 可见模式：true = 下次拉起带窗口的真实浏览器（共享给人用）。
    visible: std::sync::atomic::AtomicBool,
}

struct RunningChromium {
    /// `http://127.0.0.1:<port>`（/json/list 与 WS 握手同端口）。
    endpoint: String,
}

/// 进程级登记的子进程：app 退出钩子（[`kill_headless_processes`]）唯一所有者，
/// 实例侧只留 endpoint（重连复用）。Edge 155 起「启动器进程」会立即退出并
/// 把真浏览器拉成分离进程——child 句柄随之失效，故连 profile 目录一起登记，
/// 关闭时按命令行引用兜底查杀分离进程。
static HEADLESS_CHILD: std::sync::Mutex<Option<(std::process::Child, std::path::PathBuf)>> =
    std::sync::Mutex::new(None);

/// 杀掉登记的无头 Chromium（app 退出钩子 / 会话关闭共用）。
pub fn kill_headless_processes() {
    if let Ok(mut guard) = HEADLESS_CHILD.lock() {
        if let Some((mut child, profile)) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
            // 启动器交接后真浏览器是分离进程，child 杀不到：按命令行引用
            // profile 目录兜底查杀（启动清扫同款机制；同步执行，关闭路径
            // 多花 ~1s 可接受）。只杀不删——profile 留着养 Cookie 信誉。
            kill_orphans_holding_profile(&profile, false);
        }
    }
}

/// 目录名 → 属主：非本产品前缀 → `None`（不碰）；无后缀旧版共享目录 →
/// `Some(None)`（必无主）；`-<pid>` 后缀 → `Some(Some(pid))`；其余怪异命名
/// （`-backup` 之类）不是我们的产物 → `None`。
fn classify_profile_dir(name: &str) -> Option<Option<u32>> {
    let rest = name.strip_prefix("rclaw-browser-profile")?;
    if rest.is_empty() {
        return Some(None);
    }
    let pid = rest.strip_prefix('-')?.parse::<u32>().ok()?;
    Some(Some(pid))
}

/// 清扫历史崩溃残留的无头浏览器与 profile 目录：profile 按进程 id 隔离后，
/// 崩溃实例的孤儿锁的是「死 pid」的目录，正常启动不会再撞锁，孤儿会无限
/// 积累。只清属主已死的目录（含无后缀旧版共享目录）；运行中实例的 profile
/// 属主存活，不受影响。目录枚举与 [`std::env::temp_dir`] 同源（不存在
/// $TEMP/$TMPDIR 分歧）；仅「按命令行找持有进程」借平台机制，匹配一律字面
/// 量+边界（无通配/正则，无注入面、无 -120 误中 -1200）。后台线程执行，
/// 静默失败（清扫只是卫生问题，不阻塞启动）。
fn sweep_stale_orphans() {
    let own_pid = std::process::id();
    let mut candidates: Vec<(std::path::PathBuf, Option<u32>)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        // 只认目录：同名普通文件不是浏览器 profile，绝不能进查杀/删除流程。
        let Ok(is_dir) = entry.file_type().map(|t| t.is_dir()) else {
            continue;
        };
        if !is_dir {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if let Some(owner) = classify_profile_dir(&name) {
            candidates.push((entry.path(), owner));
        }
    }
    if candidates.is_empty() {
        return;
    }
    #[cfg(windows)]
    {
        // 判活下沉进 PowerShell（Get-Process）；force=true 的条目跳过判活。
        // 清扫=连目录一起删（属主已死的残留）。
        let list: Vec<serde_json::Value> = candidates
            .iter()
            .map(|(path, owner)| {
                json!({"path": path.to_string_lossy(), "owner": owner, "force": false, "remove": true})
            })
            .collect();
        windows_kill_and_remove(&list, own_pid);
    }
    #[cfg(not(windows))]
    {
        let stale: Vec<std::path::PathBuf> = candidates
            .into_iter()
            .filter(|(_, owner)| match owner {
                None => true,
                Some(pid) => *pid != own_pid && !pid_alive(*pid),
            })
            .map(|(path, _)| path)
            .collect();
        if !stale.is_empty() {
            kill_profile_holders(&stale);
            for dir in &stale {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }
}

/// 平台判活（unix）：kill -0 只探测信号权限，不实际发送；EPERM 同样证明
/// 进程存在（他人归属）。
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// 命令行是否把 dir 作为完整参数边界引用（引用点后随空白/引号/结尾）——
/// 防 profile-120 的前缀误中 profile-1200 的命令行。unix 侧查杀直接用它；
/// Windows 侧等价逻辑在 PowerShell 里（cmdline 枚举无 std 通道）。
#[cfg_attr(windows, allow(dead_code))]
fn cmdline_holds_dir(cmdline: &str, dir: &str) -> bool {
    let mut from = 0;
    while let Some(at) = cmdline[from..].find(dir) {
        let end = from + at + dir.len();
        if matches!(
            cmdline[end..].chars().next(),
            None | Some(' ') | Some('"') | Some('\'')
        ) {
            return true;
        }
        from += at + 1;
    }
    false
}

/// 杀掉命令行里按完整参数边界引用了任一目录的进程（不含本进程）。
#[cfg(any(target_os = "linux", target_os = "android"))]
fn kill_profile_holders(dirs: &[std::path::PathBuf]) {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .into_string()
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        // 参数以 NUL 分隔：换成空白保持「参数边界」语义。
        let cmdline = String::from_utf8_lossy(&cmd).replace('\0', " ");
        if dirs
            .iter()
            .any(|d| cmdline_holds_dir(&cmdline, &d.to_string_lossy()))
        {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

/// macOS 没有 /proc：用 ps 拉全量命令行（参数数组直传，无 shell、无注入面）。
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn kill_profile_holders(dirs: &[std::path::PathBuf]) {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
    else {
        return;
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((pid_text, command)) = line.trim_start().split_once(' ') else {
            continue;
        };
        let Ok(pid) = pid_text.trim().parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        if dirs
            .iter()
            .any(|d| cmdline_holds_dir(command.trim_start(), &d.to_string_lossy()))
        {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

/// Windows 查杀/删除执行器：条目清单经 base64+JSON 传入（无引号注入面）。
/// 每条 `{path, owner, force}`：owner 为 null 或 force，或属主进程已死
/// （Get-Process 判活）时——按字面量 IndexOf+边界匹配杀持有者，
/// `Remove-Item -LiteralPath` 删目录（不展开通配）。
#[cfg(windows)]
fn windows_kill_and_remove(entries: &[serde_json::Value], own_pid: u32) {
    use base64::Engine as _;
    use std::os::windows::process::CommandExt as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(
        serde_json::to_string(entries)
            .unwrap_or_default()
            .as_bytes(),
    );
    let script = format!(
        "$own = {own}; \
         $entries = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{b64}')) | \
         ConvertFrom-Json; foreach ($e in $entries) {{ \
         $stale = $false; \
         if ($null -eq $e.owner -or $e.force) {{ $stale = $true }} \
         elseif (([int]$e.owner -ne $own) -and (-not (Get-Process -Id ([int]$e.owner) -ErrorAction SilentlyContinue))) {{ $stale = $true }} \
         if ($stale) {{ $d = $e.path; \
         Get-CimInstance Win32_Process -Filter \"Name='msedge.exe' or Name='chrome.exe'\" | \
         ForEach-Object {{ $cl = $_.CommandLine; if ($cl) {{ \
         $i = $cl.IndexOf($d, [System.StringComparison]::Ordinal); \
         while ($i -ge 0) {{ $e2 = $i + $d.Length; \
         $after = if ($e2 -lt $cl.Length) {{ $cl[$e2] }} else {{ [char]0 }}; \
         if ($after -eq [char]0 -or $after -eq ' ' -or $after -eq '\"' -or $after -eq \"'\") {{ \
         Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue; break }} \
         $i = $cl.IndexOf($d, $e2, [System.StringComparison]::Ordinal) }} }} }}; \
         if ($e.remove) {{ Remove-Item -LiteralPath $d -Recurse -Force -ErrorAction SilentlyContinue }} }} }}",
        own = own_pid
    );
    // CREATE_NO_WINDOW：不闪控制台。脚本经 stdin 传入（`-Command -`）：
    // -Command 参数受 32767 字符进程命令行上限约束，目录积累多了会被截断
    // 静默失效；stdin 无此限制。
    let Ok(mut child) = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", "-"])
        .stdin(std::process::Stdio::piped())
        .creation_flags(0x0800_0000)
        .spawn()
    else {
        return;
    };
    use std::io::Write as _;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(script.as_bytes());
    }
    let _ = child.wait_with_output();
}

/// 清扫只跑一次（进程生命周期内一次即可）。
static SWEEP_ONCE: std::sync::Once = std::sync::Once::new();

/// 杀掉命令行里引用了本 profile 目录的【无头】浏览器实例。`remove` 决定是否
/// 连目录一起删：profile 重置前的清场 = true；正常关闭/重连 = false——关闭
/// 路径删 profile 会把养好的 Cookie/登录态一起抹掉（风控信誉清零）。
fn kill_orphans_holding_profile(profile_dir: &std::path::Path, remove: bool) {
    #[cfg(windows)]
    {
        let list = vec![json!({
            "path": profile_dir.to_string_lossy(),
            "owner": null,
            "force": true,
            "remove": remove,
        })];
        windows_kill_and_remove(&list, std::process::id());
    }
    #[cfg(not(windows))]
    {
        // 非 Windows 的查杀只杀不删；需要删目录由调用方自行 remove_dir_all。
        let _ = remove;
        kill_profile_holders(std::slice::from_ref(&profile_dir.to_path_buf()));
    }
}

impl HeadlessChromiumTransport {
    pub fn new(screenshot_dir: impl Into<std::path::PathBuf>) -> Self {
        HeadlessChromiumTransport {
            inner: AsyncMutex::new(None),
            screenshot_dir: screenshot_dir.into(),
            // 按进程隔离：两个应用实例各用各的 profile，不互相抢锁（崩溃残留的
            // 孤儿由 kill_orphans_holding_profile 按各自目录特征清杀）。
            profile_dir: std::env::temp_dir()
                .join(format!("rclaw-browser-profile-{}", std::process::id())),
            visible: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 切换可见模式（true = 用户共享的真实浏览器窗口）。已在跑的实例不受影响：
    /// 调用方随后 close + 重连即以新形态拉起（profile 延续，cookie 信誉保留）。
    pub fn set_visible(&self, visible: bool) {
        self.visible
            .store(visible, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn is_visible(&self) -> bool {
        self.visible.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 候选可执行文件：环境覆盖 → Windows 常规安装位 → Linux/macOS 常规位。
    fn candidate_exes() -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        if let Ok(override_exe) = std::env::var("RCLAW_BROWSER_EXE") {
            if !override_exe.trim().is_empty() {
                out.push(std::path::PathBuf::from(override_exe));
            }
        }
        let mut push = |path: Option<std::path::PathBuf>| {
            if let Some(path) = path {
                out.push(path);
            }
        };
        #[cfg(windows)]
        {
            let roots = [
                std::env::var("PROGRAMFILES").ok(),
                std::env::var("ProgramFiles(x86)").ok(),
                std::env::var("LOCALAPPDATA").ok(),
            ];
            for root in roots.into_iter().flatten() {
                let root = std::path::PathBuf::from(root);
                push(Some(root.join("Google/Chrome/Application/chrome.exe")));
                push(Some(root.join("Chromium/Application/chrome.exe")));
                push(Some(root.join("Microsoft/Edge/Application/msedge.exe")));
            }
        }
        #[cfg(not(windows))]
        {
            for path in [
                "/usr/bin/google-chrome-stable",
                "/usr/bin/google-chrome",
                "/usr/bin/chromium",
                "/usr/bin/chromium-browser",
                "/snap/bin/chromium",
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                "/Applications/Chromium.app/Contents/MacOS/Chromium",
                "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            ] {
                push(Some(std::path::PathBuf::from(path)));
            }
        }
        out
    }

    /// 拉起无头实例：stderr 上等 `DevTools listening on ws://127.0.0.1:<port>/…`
    /// 行拿调试端口（--remote-debugging-port=0 由内核自选）。
    async fn launch(&self) -> Result<RunningChromium, String> {
        // 首次拉起前顺手清一次历史崩溃孤儿（后台线程，不阻塞启动）。
        SWEEP_ONCE.call_once(|| {
            std::thread::spawn(sweep_stale_orphans);
        });
        fn launch_blocking(
            exe: std::path::PathBuf,
            profile: std::path::PathBuf,
            visible: bool,
        ) -> Result<(std::process::Child, u16), String> {
            use std::io::BufRead as _;
            // 上次崩溃实例可能遗留旧端口文件：启动前先清（spawn 之后再清会
            // 竞态删除本次的就绪信号——Edge 写文件早于我们的删除）。NotFound
            // 属正常；删除失败也只是退回 stderr 行信号，不阻断。
            let port_file = profile.join("DevToolsActivePort");
            match std::fs::remove_file(&port_file) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => tracing::debug!("清理旧 DevToolsActivePort 失败: {error}"),
            }
            let mut command = std::process::Command::new(&exe);
            if visible {
                // 可见模式 = 真实浏览器窗口（用户手动上网共享）：无头/禁 GPU/
                // 隐藏滚动条一律不设（真实渲染与滚动条），窗口拉起即最大化。
                command.arg("--start-maximized");
            } else {
                command.arg("--headless=new");
            }
            command
                .arg("--remote-debugging-port=0")
                // Chromium 开关值必须同参数（= 连接）：拆成两个参数时路径会被当
                // 成位置参数（URL），profile 落回默认目录——撞上已运行的实例
                // 就立即退出，stderr 随之关闭。
                .arg(format!("--user-data-dir={}", profile.display()))
                .args([
                    "--no-first-run",
                    "--no-default-browser-check",
                    "about:blank",
                ]);
            if !visible {
                command.args(["--disable-gpu", "--hide-scrollbars"]);
            }
            command
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped());
            let mut child = command
                .spawn()
                .map_err(|e| format!("拉起无头浏览器失败: {e}"))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| "无头浏览器 stderr 不可读".to_string())?;
            // stderr 必须持续排空——管道写满（64KB）会反过来阻塞浏览器进程本身
            // （Playwright 同款后台泵）。DevTools 端口的就绪信号有 两 路（先到
            // 先得）：stderr 的 "DevTools listening on ws://…" 行（老内核），
            // 以及 profile 目录下 DevToolsActivePort 文件首行（Chromium 标准
            // 行为；实测 Edge 155 起 stderr 行不再输出，只剩文件信号）。
            let (tx, rx) = std::sync::mpsc::channel::<(Option<u16>, Vec<String>)>();
            let mut sent = false;
            std::thread::spawn(move || {
                let mut seen: Vec<String> = Vec::new();
                let mut port: Option<u16> = None;
                for line in std::io::BufReader::new(stderr).lines() {
                    let line = match line {
                        Ok(line) => line,
                        Err(_) => break,
                    };
                    if port.is_some() {
                        continue;
                    }
                    if let Some(rest) = line.strip_prefix("DevTools listening on ws://") {
                        let authority = rest.split('/').next().unwrap_or_default();
                        if let Some(found) = authority
                            .rsplit(':')
                            .next()
                            .and_then(|p| p.parse::<u16>().ok())
                        {
                            port = Some(found);
                            sent = true;
                            let _ = tx.send((port, std::mem::take(&mut seen)));
                            continue;
                        }
                    }
                    if seen.len() < 3 && !line.trim().is_empty() {
                        seen.push(line);
                    }
                }
                // EOF = 进程已退出（或从未报端口）：把结局带回主路径（端口已
                // 发过则不重发——主路径可能已消费）。
                if !sent {
                    let _ = tx.send((port, seen));
                }
            });
            let port_file = profile.join("DevToolsActivePort");
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            loop {
                if let Ok((Some(port), _)) = rx.try_recv() {
                    return Ok((child, port));
                }
                if let Ok(content) = std::fs::read_to_string(&port_file) {
                    if let Some(port) = content
                        .lines()
                        .next()
                        .and_then(|line| line.trim().parse::<u16>().ok())
                    {
                        return Ok((child, port));
                    }
                }
                match child.try_wait() {
                    // 非零退出 = 真失败（策略拦截 / profile 锁 / 参数不识别），
                    // 快速报错带回 stderr 前几行。
                    Ok(Some(status)) if !status.success() => {
                        let mut seen = Vec::new();
                        while let Ok((_, lines)) = rx.try_recv() {
                            if seen.is_empty() {
                                seen = lines;
                            }
                        }
                        return Err(format!(
                            "无头浏览器未报调试端口即退出（前几行: {seen:?}，退出状态: {status}）"
                        ));
                    }
                    // 退出码 0 = Edge 155+ 启动器交接：真浏览器是分离进程，
                    // 端口文件随后出现——继续等文件到 deadline，不判失败。
                    Ok(Some(_)) => {}
                    Ok(None) => {}
                    Err(error) => {
                        return Err(format!("探测无头浏览器状态失败: {error}"));
                    }
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(
                        "等待无头浏览器调试端口超时（20s 内未读到 DevToolsActivePort / stderr 端口行）"
                            .to_string(),
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        let exe = Self::candidate_exes()
            .into_iter()
            .find(|path| path.is_file())
            .ok_or_else(|| {
                "未找到 Chrome/Edge：请安装（或用 RCLAW_BROWSER_EXE 指定可执行文件路径）"
                    .to_string()
            })?;
        let profile = self.profile_dir.clone();
        tokio::fs::create_dir_all(&profile)
            .await
            .map_err(|e| format!("创建浏览器 profile 目录失败: {e}"))?;
        let visible = self.is_visible();
        let job = tokio::task::spawn_blocking(move || launch_blocking(exe, profile, visible));
        // stderr 行读取是阻塞循环：外层限时兜底（内核异常时不无限等）。
        let (child, port) = tokio::time::timeout(Duration::from_secs(20), job)
            .await
            .map_err(|_| "等待无头浏览器调试端口超时".to_string())?
            .map_err(|e| format!("调度无头浏览器启动失败: {e}"))??;
        tracing::info!(port, "CDP 无头浏览器已拉起");
        if let Ok(mut guard) = HEADLESS_CHILD.lock() {
            *guard = Some((child, self.profile_dir.clone()));
        }
        Ok(RunningChromium {
            endpoint: format!("http://127.0.0.1:{port}"),
        })
    }

    /// 对 endpoint 做探测 + page WS 原始流（两次连接：探测一发一收，WS 独占）。
    async fn try_connect_page(endpoint: &str) -> Result<(RawStream, String), String> {
        let authority = endpoint
            .split("//")
            .nth(1)
            .ok_or_else(|| format!("调试端点非法: {endpoint}"))?;
        let mut parts = authority.split(':');
        let host = parts.next().unwrap_or("127.0.0.1").to_string();
        let port: u16 = parts
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| format!("调试端点缺少端口: {endpoint}"))?;
        {
            let mut stream = tokio::net::TcpStream::connect((host.as_str(), port))
                .await
                .map_err(|e| format!("连接调试端口失败: {e}"))?;
            let target = probe_page_target(&mut stream).await?;
            drop(stream);
            let ws_stream = tokio::net::TcpStream::connect((host.as_str(), port))
                .await
                .map_err(|e| format!("连接调试端口失败: {e}"))?;
            Ok((Box::new(ws_stream), target))
        }
    }
}

#[async_trait::async_trait]
impl DevtoolsTransport for HeadlessChromiumTransport {
    async fn connect(&self) -> Result<(RawStream, String), String> {
        let mut guard = self.inner.lock().await;
        if let Some(run) = guard.as_ref() {
            match Self::try_connect_page(&run.endpoint).await {
                Ok(pair) => return Ok(pair),
                Err(error) => {
                    tracing::warn!("无头浏览器连接失效（{error}），重新拉起");
                    guard.take();
                    kill_headless_processes();
                }
            }
        }
        let run = match self.launch().await {
            Ok(run) => run,
            Err(error) => {
                // profile 被上一次崩溃的孤儿实例锁住时 Edge 会秒退（exit 21）；
                // profile 半删除损坏时则静默卡死。清孤儿 → 重置 → 重试一次。
                tracing::warn!("无头浏览器启动失败（{error}），清孤儿并重置 profile 后重试");
                kill_headless_processes();
                kill_orphans_holding_profile(&self.profile_dir, false);
                let _ = tokio::fs::remove_dir_all(&self.profile_dir).await;
                self.launch().await?
            }
        };
        let pair = Self::try_connect_page(&run.endpoint).await?;
        *guard = Some(run);
        Ok(pair)
    }

    async fn shutdown(&self) -> Result<(), String> {
        {
            let mut guard = self.inner.lock().await;
            guard.take(); // 清 endpoint：下次 connect 重新拉起
        }
        kill_headless_processes();
        Ok(())
    }

    fn screenshot_dir(&self) -> std::path::PathBuf {
        self.screenshot_dir.clone()
    }

    async fn capture_screenshot(
        &self,
        session: &Session,
        dest: &std::path::Path,
    ) -> Result<String, String> {
        capture_screenshot_via_cdp(session, dest).await?;
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(format!(
            "已截图：browser/{name}（工作区内，可用 read 读取）"
        ))
    }
}

// ---------------------------------------------------------------------------
// 共享内核：CdpBrowser
// ---------------------------------------------------------------------------

/// CDP 浏览器共享内核（browser_* 工具面共用）。
pub struct CdpBrowser {
    transport: Arc<dyn DevtoolsTransport>,
    session: AsyncMutex<Option<Session>>,
    /// 操作互斥：浏览器是进程级单例（对话 turn / cron turn 可能并发调工具），
    /// 「导航→等加载→提取」这类多步事务必须整体串行，否则两个并发取页会互相
    /// 覆盖页面、各自提取到对方的 URL。Arc 包装：browser_extract 的清理任务
    /// 需要 'static 的锁句柄（见 evaluate）。
    op: Arc<AsyncMutex<()>>,
    /// 会话 UA 的事件词汇判定：true = 移动端（Android UA → 合成手势 + JS
    /// 兜底 + 逐字符键盘事件），false = 桌面（鼠标/滚轮）。会话建立时由
    /// humanize 的 UA 读取更新（UA 是会话级覆写，词汇必须同源）。
    mobile_input: std::sync::atomic::AtomicBool,
}

/// park 过码等待：页面恢复轮询间隔。
const PARK_POLL_INTERVAL: Duration = Duration::from_millis(1500);
/// park 过码等待总超时（到点标注返回，agent 指引报告用户并停止重试）。
const PARK_TIMEOUT: Duration = Duration::from_secs(180);

/// park 等待的结局。
enum ParkOutcome {
    /// 页面已离开风控页（用户过码或 reCAPTCHA 自动放行），携带等待秒数与
    /// 恢复页的重新提取结果。
    Recovered(u64, PagePayload),
    /// 超时未恢复：带当前（风控）页的提取结果。
    Timeout(PagePayload),
    /// 连接断开：悬浮窗「关闭会话」销毁了 WebView。
    Cancelled,
}

impl CdpBrowser {
    pub fn new(transport: Arc<dyn DevtoolsTransport>) -> Self {
        CdpBrowser {
            transport,
            session: AsyncMutex::new(None),
            op: Arc::new(AsyncMutex::new(())),
            mobile_input: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 取（必要时重建）连接。断开自动重连——内核实例仍在，重连不重建。
    async fn session(&self) -> Result<Session, String> {
        let mut guard = self.session.lock().await;
        if let Some(existing) = guard.as_ref() {
            if !existing.tx.is_closed() {
                return Ok(existing.clone());
            }
            *guard = None;
        }
        let (raw_stream, target) = self.transport.connect().await?;
        let ws = ws_over(raw_stream, &target).await?;
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
        let mobile = humanize_session(&session).await;
        self.mobile_input
            .store(mobile, std::sync::atomic::Ordering::Relaxed);
        // 视口一致性：老 WebView 的 app-context 度量把 CSS 视口折成非标准值
        // （真机实测 480/1440，真手机上不可能出现）。用设备真实 Display 的
        // CSS 视口钉住渲染器（mobile=true 同时启用移动视口语义），Playwright
        // 同款机制；失败仅记日志（总比 1702×3189 的矛盾视口好）。
        if let Some((w, h, dpr)) = self.transport.device_metrics() {
            if let Err(error) = session
                .call(
                    "Emulation.setDeviceMetricsOverride",
                    json!({
                        "width": w, "height": h,
                        "deviceScaleFactor": dpr, "mobile": true,
                    }),
                    Duration::from_secs(10),
                )
                .await
            {
                tracing::warn!("CDP 拟人化：视口覆写失败（沿用内核视口）: {error}");
            } else {
                tracing::info!(w, h, dpr, "CDP 拟人化：渲染视口已钉住（设备真实值）");
            }
        }
        *guard = Some(session.clone());
        Ok(session)
    }

    /// 建立（或复用）共享会话：连接 + 拟人化 + 视口钉住。桌面「打开浏览器」
    /// 立即拉起可见窗口用；失败原样上抛。
    pub async fn ensure_session(&self) -> Result<(), String> {
        self.session().await.map(|_| ())
    }

    /// 导航公共段：Page.navigate → 等 loadEventFired → 静置 800ms。
    /// `referrer` 是来源页 URL：CDP 命令式导航没有 initiator，请求不带
    /// Referer 头、document.referrer 为空（等价地址栏直达）；站内跳转传
    /// 上一页 URL 才像真人点链。None = 与现状相同的无 referrer 直达。
    async fn navigate_wait(
        &self,
        url: &str,
        referrer: Option<&str>,
        load_timeout: Duration,
    ) -> Result<Session, String> {
        let session = self.session().await?;
        let pre = *session.load_rx.borrow();
        let mut params = json!({ "url": url });
        if let Some(from) = referrer {
            params["referrer"] = json!(from);
        }
        session
            .call("Page.navigate", params, Duration::from_secs(20))
            .await?;
        session.wait_load(pre, load_timeout).await?;
        tokio::time::sleep(Duration::from_millis(800)).await;
        Ok(session)
    }

    /// EXTRACT_JS 提取当前页面（正文 + SERP 结果 + 风控/同意页检测标注）。
    async fn extract_page(&self, session: &Session) -> Result<PagePayload, String> {
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

    /// 打开 URL 并等加载完成，返回页面提取结果（正文 + SERP 结果 +
    /// Google 风控页/同意页检测标注）。命中风控页进入 park 等待（见模块注释）。
    /// `referrer` 语义见 [`Self::navigate_wait`]。
    pub async fn navigate_and_extract(
        &self,
        url: &str,
        referrer: Option<&str>,
    ) -> Result<PagePayload, String> {
        let _op = self.op.lock().await;
        let session = self
            .navigate_wait(url, referrer, Duration::from_secs(40))
            .await?;
        let mut payload = self.extract_page(&session).await?;
        if payload.blocked.contains("CAPTCHA") {
            // 单点通知（通知+悬浮窗展开失败只记日志，park 照常等待）。
            self.transport.notify_captcha(&payload.url).await;
            match self.park_until_recovered(&session).await {
                ParkOutcome::Recovered(waited, recovered) => {
                    payload = recovered;
                    payload.park_note = format!("已通过人工过码（等待 {waited} 秒），页面已恢复。");
                }
                ParkOutcome::Timeout(captcha_page) => {
                    payload = captcha_page;
                    payload.park_note = format!(
                        "已通知用户人工过码，但等待 {} 秒仍未通过——向用户报告，不要立刻重试（重复触发会加重出口 IP 封锁）。",
                        PARK_TIMEOUT.as_secs()
                    );
                }
                ParkOutcome::Cancelled => {
                    return Err(
                        "用户在悬浮窗关闭了浏览器会话（过码取消）；下次任一 browser_* 调用会自动重建会话。"
                            .to_string(),
                    )
                }
            }
        }
        Ok(payload)
    }

    /// park/long-poll：风控页命中后的等待循环。三臂竞争——1.5s 轮询探针
    /// （页面恢复即重新提取返回）/ 180s 总超时 / 连接断开（悬浮窗「关闭会话」
    /// → WebView 销毁 → DevTools socket 失效 → manager 退出）。轮询是只读
    /// evaluate，与用户在悬浮窗里的手动操作互不冲突。
    async fn park_until_recovered(&self, session: &Session) -> ParkOutcome {
        let started = std::time::Instant::now();
        let deadline = tokio::time::Instant::now() + PARK_TIMEOUT;
        loop {
            let tick = tokio::time::sleep(PARK_POLL_INTERVAL);
            tokio::select! {
                _ = tick => {
                    if session.tx.is_closed() {
                        return ParkOutcome::Cancelled;
                    }
                    match self.captcha_probe(session).await {
                        Ok(still_captcha) if !still_captcha => {
                            let waited = started.elapsed().as_secs();
                            // 恢复确认：重新提取成功才算恢复——探针与提取之间
                            // 页面可能再跳转；暂态失败继续轮询下一拍，断连报取消，
                            // 绝不把提取失败伪装成「已通过人工过码」的空结果。
                            match self.extract_page(session).await {
                                Ok(recovered) => {
                                    return ParkOutcome::Recovered(waited, recovered);
                                }
                                Err(error) => {
                                    if session.tx.is_closed() || error.contains("已断开") {
                                        return ParkOutcome::Cancelled;
                                    }
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            // 连接已断（关闭会话）→ 取消；页面跳转瞬间的单次
                            // evaluate 失败不构成取消，继续等下一拍。
                            if session.tx.is_closed() || error.contains("已断开") {
                                return ParkOutcome::Cancelled;
                            }
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let payload = self.extract_page(session).await.unwrap_or_default();
                    return ParkOutcome::Timeout(payload);
                }
                _ = session.tx.closed() => {
                    return ParkOutcome::Cancelled;
                }
            }
        }
    }

    /// 风控探针（PAGE_INFO_JS 轻量读取；true = 仍在风控/同意页）。
    async fn captcha_probe(&self, session: &Session) -> Result<bool, String> {
        let reply = session
            .call(
                "Runtime.evaluate",
                json!({ "expression": PAGE_INFO_JS, "returnByValue": true }),
                Duration::from_secs(3),
            )
            .await?;
        let value = evaluate_string(&reply)?;
        let parsed: Value = serde_json::from_str(&value).map_err(|e| format!("{e}"))?;
        Ok(parsed
            .get("blocked")
            .and_then(Value::as_str)
            .is_some_and(|blocked| blocked.contains("CAPTCHA")))
    }

    /// 媒体资源清单（MEDIA_JS 提取；只读，不导航）。
    pub async fn extract_media(&self) -> Result<String, String> {
        let _op = self.op.lock().await;
        let session = self.session().await?;
        let reply = session
            .call(
                "Runtime.evaluate",
                json!({ "expression": MEDIA_JS, "returnByValue": true }),
                Duration::from_secs(15),
            )
            .await?;
        let value = evaluate_string(&reply)?;
        let parsed: Value = serde_json::from_str(&value).map_err(|e| format!("{e}"))?;
        if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(parsed
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("媒体提取失败")
                .to_string());
        }
        Ok(parsed
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("页面未发现可提取的图片/视频")
            .to_string())
    }

    /// 紧凑可交互元素清单（每项带 ref，供 browser_click / browser_type 定位）。
    pub async fn snapshot(&self) -> Result<String, String> {
        let _op = self.op.lock().await;
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
        let text = parsed
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if parsed.get("ok").and_then(Value::as_bool) != Some(true) && text.is_empty() {
            return Err("页面快照失败".to_string());
        }
        // ok=false（无可交互元素）不是故障：把 JS 侧的页面说明如实带给模型，
        // 笼统的「页面快照失败」会把它推向误判；盲区提示两种形态都附。
        Ok(append_snapshot_hint(
            text,
            parsed.get("non_interactive").and_then(Value::as_u64),
        ))
    }

    /// 结构化提取（browser_extract）：只读 DOM 采集原语——按选择器枚举元素，
    /// 读取 id、指定属性与去空白 innerText。JS 模板固定、在 Rust 侧拼装，模型
    /// 只传选择器/属性名/skip/条数，不注入可执行 JS，从构造上保证只读（codex
    /// review 2026-10-20 四轮：任意 JS 求值的"只读"无法用黑名单穷尽，约束
    /// 接口才是正解）。querySelectorAll 的选择器参数不具执行能力。
    /// skip：跳过前 N 个匹配——无限滚动页保留早先卡片，固定窗口会恒取同一批，
    /// 由调用方按已采集条数推进窗口（codex review 2026-10-20）。
    /// 取消安全：预提交（抢 op 锁 + 建会话）在外层 future 内，/cancel drop 即
    /// 放弃，排队中的提取不会稍后偷跑；提交后移交 OwnedMutexGuard 进
    /// detached worker，求值与超时清理（terminateExecution + no-op 吸收）必然
    /// 持锁跑完，不与其他浏览器操作交错。
    pub async fn extract(
        self: Arc<Self>,
        selector: &str,
        attributes: &[String],
        skip: usize,
        limit: usize,
    ) -> Result<String, String> {
        let selector = selector.trim();
        if selector.is_empty() {
            return Err("selector 不能为空".to_string());
        }
        let limit = limit.clamp(1, 200);
        let selector_json = serde_json::to_string(selector).unwrap_or_default();
        // 保留名拒绝：调用方传 "text"/"id" 会覆盖模板内建字段（codex review
        // 2026-10-20）。
        let attributes: Vec<String> = attributes
            .iter()
            .filter(|a| a.as_str() != "text" && a.as_str() != "id")
            .take(8)
            .cloned()
            .collect();
        let attrs_json = serde_json::to_string(&attributes).unwrap_or_default();
        let script = format!(
            r#"(function(){{
var out=[];
var els=document.querySelectorAll({selector_json});
var attrs={attrs_json};
for(var i={skip};i<els.length&&out.length<{limit};i++){{
var el=els[i];
// 截断按 Unicode 码点（Array.from）而非 UTF-16 单元——slice 劈开代理对会产生
// 孤立代理，JSON.stringify 后 serde_json 拒解析（codex review 2026-10-20）。
var cps=function(s,n){{var a=Array.from(s);return a.length>n?a.slice(0,n).join(''):s;}};
var item={{id:cps(el.id||'',64),text:cps((el.innerText||'').replace(/\s+/g,' ').trim(),300)}};
for(var j=0;j<attrs.length;j++){{item[attrs[j]]=cps(el.getAttribute(attrs[j])||'',300);}}
out.push(item);
}}
return JSON.stringify({{count:out.length,total:els.length,skip:{skip},items:out}});
}})()"#
        );
        // 预提交阶段（抢 op 锁 + 建会话）放在外层、可取消：turn /cancel drop
        // 本 future 时直接放弃，不会稍后被排队执行。
        let op_guard = self.op.clone().lock_owned().await;
        let session = self.session().await?;
        // 提交后的清理必须强保证：移交 OwnedMutexGuard 进 detached worker——
        // 即使本 future 被 drop，worker 也会持锁跑完求值与清理。
        let worker = tokio::spawn(async move {
            let _op = op_guard;
            let reply = match session
                .call(
                    "Runtime.evaluate",
                    json!({
                        "expression": script,
                        "returnByValue": true
                    }),
                    Duration::from_secs(15),
                )
                .await
            {
                Ok(reply) => reply,
                Err(error) => {
                    // 超时只是本端放弃等待：渲染进程里的脚本仍在跑，会一直阻塞
                    // 共享 WebView——显式终止当前执行；再跑一个 no-op 吸收可能
                    // 挂起的终止状态。固定脚本无延迟回调，terminate 即恢复。
                    if error.starts_with("CDP 命令 Runtime.evaluate 超时") {
                        let _ = session
                            .call(
                                "Runtime.terminateExecution",
                                json!({}),
                                Duration::from_secs(3),
                            )
                            .await;
                        let _ = session
                            .call(
                                "Runtime.evaluate",
                                json!({ "expression": "0", "returnByValue": true }),
                                Duration::from_secs(3),
                            )
                            .await;
                    }
                    return Err(error);
                }
            };
            if let Some(exception) = reply.get("exceptionDetails") {
                let text = exception
                    .get("exception")
                    .and_then(|e| e.get("description"))
                    .and_then(Value::as_str)
                    .or_else(|| exception.get("text").and_then(Value::as_str))
                    .unwrap_or("未知脚本异常");
                return Err(format!("页面脚本异常: {}", single_line(text)));
            }
            // 预算内截断：解析后从尾部丢完整条目，保证返回恒为合法 JSON——
            // 文本级 truncate 会把对象拦腰截断（codex review 2026-10-20）。
            const BUDGET: usize = 40_000;
            let value = match reply.get("result").and_then(|r| r.get("value")) {
                Some(Value::String(s)) => {
                    serde_json::from_str::<Value>(s).unwrap_or_else(|_| Value::String(s.clone()))
                }
                Some(other) => other.clone(),
                Some(Value::Null) | None => Value::Null,
            };
            if value.get("items").and_then(Value::as_array).is_some() {
                let mut value = value;
                while serde_json::to_string(&value).unwrap_or_default().len() > BUDGET {
                    let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) else {
                        break;
                    };
                    if items.len() <= 1 {
                        break;
                    }
                    items.pop();
                    if let Some(obj) = value.as_object_mut() {
                        obj.insert("truncated".to_string(), Value::Bool(true));
                    }
                }
                // 丢尾后重算 count：调用方按 items.length 推进 skip，count 与
                // 返回条目不一致会让它静默跳过被丢弃的卡片（codex review
                // 2026-10-20）。
                let retained = value
                    .get("items")
                    .and_then(Value::as_array)
                    .map(|items| items.len())
                    .unwrap_or(0);
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("count".to_string(), serde_json::json!(retained));
                }
                let text = serde_json::to_string(&value).unwrap_or_default();
                return Ok(text);
            }
            let text = match reply.get("result").and_then(|r| r.get("value")) {
                Some(Value::String(s)) => s.clone(),
                Some(other) => serde_json::to_string(other).unwrap_or_default(),
                Some(Value::Null) | None => "页面脚本无返回值".to_string(),
            };
            Ok(truncate_chars(&text, 40_000))
        });
        match worker.await {
            Ok(result) => result,
            Err(join_error) => Err(format!("extract worker: {join_error}")),
        }
    }

    /// 点按 ref 元素。事件词汇随会话 UA 与挂载态分流（真机 vivo WebView83
    /// 实测）：移动端 attached（面板展开、合成器活跃）走
    /// `Input.synthesizeTapGesture` 合成手势（真实手势管线 → trusted click，
    /// 0.2s ack）；离屏（面板收起）注入点击在本机无效或超时，走页内 JS
    /// click（isTrusted=false，页面理论可辨别——离屏态这是唯一可靠通路）；
    /// 桌面保持鼠标词汇——点按前先落两步鼠标移动（目标附近小偏移 → 目标
    /// 中心），按下与抬起之间随机静置——反爬会查“零轨迹瞬移 + 恒定间隔”的
    /// 点按。合成手势超时沿用触摸超时语义：事件可能已送达，**不得**重放
    /// （会二次执行动作：误下单/误跳转），只上报歧义让 agent 复核。
    pub async fn click(&self, r: u32) -> Result<String, String> {
        let _op = self.op.lock().await;
        let session = self.session().await?;
        let (x, y) = self.element_center(&session, r).await?;
        let mobile = self.mobile_input.load(std::sync::atomic::Ordering::Relaxed);
        let mut note = String::new();
        if mobile {
            let attached = self.transport.overlay_attached().await;
            let mut clicked = false;
            if attached {
                match session
                    .call(
                        "Input.synthesizeTapGesture",
                        json!({ "x": x, "y": y }),
                        Duration::from_secs(15),
                    )
                    .await
                {
                    Ok(_) => clicked = true,
                    Err(error) => {
                        if error.contains("超时") {
                            return Err(format!(
                                "点击已派发但未确认生效（合成手势超时）——不要重放，\
                                 请用 browser_snapshot 核实页面状态后再决定是否重试: {error}"
                            ));
                        }
                        note = format!("（合成手势点击失败: {error}，已改用页内点击）");
                    }
                }
            } else {
                note = "（面板收起：本机离屏态注入点击无效果，已用页内点击）".to_string();
            }
            if !clicked {
                let js = format!(
                    "(function(){{var el=document.querySelector('[data-rclaw-ref=\"{r}\"]');\
                     if(!el)return 'MISSING';\
                     if(typeof el.click==='function'){{el.click();}}\
                     else{{el.dispatchEvent(new MouseEvent('click',{{bubbles:true,cancelable:true}}));}}\
                     return 'OK';}})()"
                );
                let reply = session
                    .call(
                        "Runtime.evaluate",
                        json!({ "expression": js, "returnByValue": true }),
                        Duration::from_secs(10),
                    )
                    .await?;
                if evaluate_string(&reply)? != "OK" {
                    return Err("元素不存在（页面可能已跳转，请重新 browser_snapshot）".to_string());
                }
            }
        } else {
            for (mx, my) in [(x + jitter(-24, 24), y + jitter(-18, 18)), (x, y)] {
                session
                    .call(
                        "Input.dispatchMouseEvent",
                        json!({ "type": "mouseMoved", "x": mx, "y": my }),
                        Duration::from_secs(10),
                    )
                    .await?;
            }
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
                if kind == "mousePressed" {
                    tokio::time::sleep(Duration::from_millis(jitter(50, 140) as u64)).await;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        let info = self.page_info(&session).await?;
        Ok(format!(
            "已在 ({x},{y}) 点按 [ref {r}]。{note}\n{}",
            format_report(&info)
        ))
    }

    /// 聚焦 ref 输入框并输入文本（先清空）。事件词汇随会话 UA：移动端逐字符
    /// `Input.dispatchKeyEvent`（keyDown 带 text → Chromium 合成 keypress +
    /// input + keyUp 完整键盘事件流；`insertText` 只有 input 事件，是已知
    /// 自动化特征），单字符派发失败回退 `insertText`；桌面保持 IME 通路
    /// `insertText`（桌面真人键盘场景反爬不查 key 流，且 IME 通路对中文最稳）。
    pub async fn type_text(&self, r: u32, text: &str) -> Result<String, String> {
        let _op = self.op.lock().await;
        let session = self.session().await?;
        let (x, y) = self.element_center(&session, r).await?;
        // 下拉框（select）分支：对 select 的「输入」语义是选中含指定文本的
        // 选项（登录页国家区号等）。触摸 tap 会弹原生选择器阻塞流程，
        // clear+逐字符键盘对 select 也无意义——都不做，直接 JS 选中并派发
        // input/change。【必须用原生 value setter 赋值】：selectedIndex 直接
        // 赋值绕过 React/Angular 的值跟踪——事件被去重、页面状态与原生控件
        // 绘制的标签都不刷新（真机淘宝登录页实测：select 停在 +86、页面状态
        // +1，selectedIndex 法后可见区号仍是 +1；原生 setter + change 才真正
        // 同步）。setter 按 value 落首个匹配项，重复 value 的选项随后按索引
        // 校正。changed=false（本就停在目标项）时页面可见状态不可信，须
        // 截图目检——Rust 侧返回文案会提醒。
        let select_check = session
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": format!(
                        "(function(ref){{var el=document.querySelector('[data-rclaw-ref=\"'+ref+'\"]');return el?(el.tagName==='SELECT'?'SELECT':'OTHER'):'MISSING';}})({r})"
                    ),
                    "returnByValue": true
                }),
                Duration::from_secs(10),
            )
            .await;
        if let Ok(reply) = &select_check {
            if evaluate_string(reply)? == "SELECT" {
                let select_js = format!(
                    "(function(ref,text){{var el=document.querySelector('[data-rclaw-ref=\"'+ref+'\"]');if(!el)return JSON.stringify({{ok:false,error:'引用不存在（页面可能已跳转，请重新 browser_snapshot）'}});var t=text.toLowerCase();var best=-1;for(var i=0;i<el.options.length;i++){{var hay=((el.options[i].text||'')+' '+(el.options[i].value||'')).toLowerCase();if(hay.indexOf(t)>=0){{best=i;break;}}}}if(best<0)return JSON.stringify({{ok:false,error:'下拉框没有含「'+text+'」的选项（共 '+el.options.length+' 项）'}});var label=(el.options[best].text||'').slice(0,40);var changed=el.selectedIndex!==best;var desc=Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el),'value');if(desc&&desc.set){{desc.set.call(el,el.options[best].value);}}if(el.selectedIndex!==best){{el.selectedIndex=best;}}el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));return JSON.stringify({{ok:true,selected:label,changed:changed}});}})({r},{})",
                    json!(text)
                );
                let reply = session
                    .call(
                        "Runtime.evaluate",
                        json!({ "expression": select_js, "returnByValue": true }),
                        Duration::from_secs(10),
                    )
                    .await
                    .map_err(|error| format!("下拉选中失败: {error}"))?;
                let value = evaluate_string(&reply)?;
                let parsed: Value = serde_json::from_str(&value).map_err(|e| format!("{e}"))?;
                if parsed.get("ok").and_then(Value::as_bool) == Some(true) {
                    let selected = parsed
                        .get("selected")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let changed = parsed
                        .get("changed")
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    if changed {
                        return Ok(format!("已在 [ref {r}] 下拉框选中「{selected}」。"));
                    }
                    // changed=false：select 此前就停在目标项，本次没有任何值变化。
                    // 页面若有自绘标签/原生控件绘制的可见值，它可能是陈旧状态
                    // （真机淘宝登录页实测：select 停 +86、页面仍显 +1）。
                    return Ok(format!(
                        "已在 [ref {r}] 下拉框选中「{selected}」（下拉框此前就停在该项，值未变化）。\
                         注意：页面可见的选项标签可能仍是陈旧状态，向用户报告前请先用 \
                         browser_screenshot 目检可见文本核实。"
                    ));
                }
                return Err(parsed
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("下拉选中失败")
                    .to_string());
            }
        }
        // 触摸聚焦是行为拟真，不是功能必需——下方 JS `el.focus()` 才是真正的
        // 聚焦通路。仅 attached 态做合成 tap 聚焦（真实手势；离屏态注入点击
        // 在本机无效/超时，直接跳过），失败照常输入。
        let mut touch_note = String::new();
        let mobile = self.mobile_input.load(std::sync::atomic::Ordering::Relaxed);
        if mobile {
            if self.transport.overlay_attached().await
                && session
                    .call(
                        "Input.synthesizeTapGesture",
                        json!({ "x": x, "y": y }),
                        Duration::from_secs(10),
                    )
                    .await
                    .is_err()
            {
                touch_note = "（触摸聚焦失败，已用 JS focus 继续输入）".to_string();
            }
        } else {
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
        let mut typed = 0usize;
        if self.mobile_input.load(std::sync::atomic::Ordering::Relaxed) {
            for ch in text.chars() {
                let unit = ch.to_string();
                // keyDown 带 text：Chromium 侧合成 char 事件（keypress/input）。
                let down = json!({
                    "type": "keyDown", "key": unit,
                    "text": unit, "unmodifiedText": unit,
                });
                match session
                    .call("Input.dispatchKeyEvent", down, Duration::from_secs(10))
                    .await
                {
                    Ok(_) => {}
                    Err(error) => {
                        // 内核拒收（老版本对非 ASCII text 有限制等）：整段回退
                        // insertText，保住输入能力。
                        tracing::debug!("逐字符键盘事件失败（回退 insertText）: {error}");
                        break;
                    }
                }
                session
                    .call(
                        "Input.dispatchKeyEvent",
                        json!({ "type": "keyUp", "key": unit }),
                        Duration::from_secs(10),
                    )
                    .await?;
                typed += 1;
                // 真人击键节奏（0.03-0.12s 随机间隔）。
                tokio::time::sleep(Duration::from_millis(jitter(30, 120) as u64)).await;
            }
        }
        if typed < text.chars().count() {
            // 逐字符通路失败（首字符即拒或中途拒）：只补未入场的剩余部分，
            // 已派发的字符不重复。
            let remaining: String = text.chars().skip(typed).collect();
            session
                .call(
                    "Input.insertText",
                    json!({ "text": remaining }),
                    Duration::from_secs(10),
                )
                .await?;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        Ok(format!(
            "已在 [ref {r}] 输入 {} 个字符。{touch_note}",
            text.chars().count()
        ))
    }

    /// 滑动页面。事件词汇随会话 UA 与挂载态分流（真机 vivo WebView83 实测）：
    /// 桌面走真实滚轮事件（`mouseWheel`，逐格带拟人抖动）；移动端 attached
    /// （面板展开、合成器活跃）走 `Input.synthesizeScrollGesture` 合成手势
    /// （唯一有效的注入滚动通路：0.2s ack + 真实滚动与惯性）；离屏（面板
    /// 收起、agent 自主驱动）合成器不活跃——touch/wheel/synthesize 全部
    /// ack 照常但位移为零，直接走页内程序化滚动（滚动事件照常派发，懒加载
    /// 正常触发）。鼠标滚轮在手机上既非人形输入、离屏又无效，不作为回退。
    pub async fn scroll(&self, notches: i32) -> Result<String, String> {
        let _op = self.op.lock().await;
        let session = self.session().await?;
        let (inner, visual) = self.viewports(&session).await?;
        // 事件坐标必须落在可见区域内；起点取视口中部最接近真人。
        let mobile = self.mobile_input.load(std::sync::atomic::Ordering::Relaxed);
        let (w, h) = if mobile { visual } else { inner };
        let (x, y) = (w / 2, h / 2);
        let mut note = String::new();
        let mut actual_px: i64 = 0;
        if mobile {
            let attached = self.transport.overlay_attached().await;
            let sign: i64 = if notches > 0 { 1 } else { -1 };
            let total_px = 120i64 * notches.unsigned_abs() as i64;
            let mut ambiguous = false;
            let mut attempted = false;
            let mut js_ran = false;
            if attached {
                // 逐段手势、逐段登记测量：600px 手势之间视口内容会移动（落点
                // 命中的容器可能变化），每段都对当前内容重新登记滚动链、实测
                // 该段位移——跨段复用首段登记会把新容器的位移记到旧链头上，
                // 造成漏记进度与重复补滚。登记与手势共用同一个抖动后的落点：
                // 中心贴着相邻容器边界时，差几个像素就是另一个滚动容器。
                while actual_px < total_px {
                    let chunk = (total_px - actual_px).min(600);
                    let gx = x + jitter(-6, 6);
                    let gy = y + jitter(-6, 6);
                    let before = self.scroll_target_begin(&session, gx, gy).await?;
                    // 登记即算「手势阶段已在此链上执行」：手势显式失败（事件
                    // 确定未送达）时补滚沿用这条链，而不是换落点重登记——抖动
                    // 落点可能命中 contain 边界面板，换中心点会绕过边界。
                    attempted = true;
                    if let Err(error) = self
                        .synth_scroll(&session, gx, gy, chunk, notches > 0)
                        .await
                    {
                        if error.contains("超时") {
                            ambiguous = true;
                            note = format!(
                                "（触摸手势超时，滚动可能仍在进行——未追加补滚；请用 browser_snapshot 核实位置: {error}）"
                            );
                        } else {
                            note = format!("（触摸手势滚动失败: {error}，缺口改用页内程序化滚动）");
                        }
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    let after = self.scroll_target_pos(&session).await?;
                    // 只设下限不设上限：scroll-snap 等会把终点调过头，实测超量
                    // 也是真实发生的位移，钳掉会少报并多滚缺口。
                    let gained = ((after - before) * sign).max(0);
                    if gained == 0 {
                        // 首段就零位移：手势被内核丢弃/被页面吞掉，直接转 JS。
                        if actual_px == 0 {
                            note = "（触摸手势未产生滚动，已改用页内程序化滚动）".to_string();
                        }
                        break;
                    }
                    actual_px += gained;
                }
            } else {
                note = "（面板收起：本机离屏态注入滚动无效果，已用页内程序化滚动——滚动事件正常触发懒加载）"
                    .to_string();
            }
            if !ambiguous && actual_px < total_px {
                if attempted && note.is_empty() {
                    note = "（触摸手势部分生效，缺口已用页内程序化滚动）".to_string();
                }
                // 补滚沿用登记链（手势阶段就在那条链上执行——含显式失败与
                // overscroll 截断链；换落点重新登记会命中旁边的背景容器，
                // 绕过手势被正确拦下的边界）。仅当手势阶段未执行（面板收起）
                // 时才按 (x,y) 新登记。
                if !attempted {
                    let _ = self.scroll_target_begin(&session, x, y).await?;
                }
                let moved = self
                    .js_scroll(&session, sign * (total_px - actual_px))
                    .await?;
                js_ran = true;
                actual_px += moved;
                if actual_px < total_px {
                    note.push_str(&format!(
                        "（仍有 {}px 未能滚动：已到页面边界或滚动容器特殊）",
                        total_px - actual_px
                    ));
                }
            }
            if !ambiguous {
                let path = if !attempted || actual_px == 0 {
                    "页内程序化滚动"
                } else if js_ran {
                    "触摸手势+页内程序化滚动"
                } else {
                    "触摸手势"
                };
                note = format!("（通路：{path}；实际位移 {actual_px}px/请求 {total_px}px）{note}");
            }
        } else {
            for _ in 0..notches.unsigned_abs() {
                session
                    .call(
                        "Input.dispatchMouseEvent",
                        json!({
                            "type": "mouseWheel",
                            "x": x + jitter(-8, 8),
                            "y": y + jitter(-8, 8),
                            "deltaX": 0,
                            "deltaY": 120 * notches.signum(),
                        }),
                        Duration::from_secs(10),
                    )
                    .await?;
                tokio::time::sleep(Duration::from_millis(jitter(60, 160) as u64)).await;
            }
        }
        // 静置一拍让滚动/懒加载稳定，再回页面信息。
        tokio::time::sleep(Duration::from_millis(500)).await;
        let info = self.page_info(&session).await?;
        Ok(format!(
            "已{}滑动 {} 格。{note}\n{}",
            if notches > 0 { "向下" } else { "向上" },
            notches.unsigned_abs(),
            format_report(&info)
        ))
    }

    /// attached 态合成触摸滚动：整段位移切块为 ≤600px/段的手势，段间
    /// 80-200ms 抖动停顿、speed 抖动拟人。方向：`down` 为真 = 手指上移 =
    /// yDistance 负。`(x,y)` 必须与 [`Self::scroll_target_begin`] 的登记落点
    /// 完全一致（由调用方抖动一次后共用）——落点差几个像素可能命中相邻的
    /// 另一个滚动容器，登记 A 实际滚 B 会漏记进度。任一段失败即中止返回
    /// Err（已完成进度由调用方按实测位移校准，不在此累计）。
    async fn synth_scroll(
        &self,
        session: &Session,
        x: i64,
        y: i64,
        total_px: i64,
        down: bool,
    ) -> Result<(), String> {
        let mut remaining = total_px;
        while remaining > 0 {
            let step = remaining.min(600);
            let distance = if down { -step } else { step };
            session
                .call(
                    "Input.synthesizeScrollGesture",
                    json!({
                        "x": x,
                        "y": y,
                        "xDistance": 0,
                        "yDistance": distance,
                        "speed": jitter(1600, 2800),
                    }),
                    Duration::from_secs(15),
                )
                .await?;
            remaining -= step;
            if remaining > 0 {
                tokio::time::sleep(Duration::from_millis(jitter(80, 200) as u64)).await;
            }
        }
        Ok(())
    }

    /// 滚动链登记（每次 scroll 调用开始时执行一次）：从手势落点 (x,y) 向上收
    /// 集滚动容器链。入链条件：computed overflowY ∈ auto/scroll/overlay（是
    /// 滚动容器）且内容超高——overflow visible/hidden 不是滚动容器，不算边
    /// 界也不入链。命中 `overscroll-behavior-y: contain/none` 的滚动容器即截
    /// 断向上收集（根也不追加）——原生手势在此边界停住，进度测量与 JS 补滚
    /// 同样不得越过它滚背景；无溢出的滚动容器同样构成边界。遍历到根
    /// （scrollingElement）即止且根只以链尾身份出现一次。反向容器标志只认
    /// 真实 flex 布局的纵向反转（display 含 flex 且 flex-direction 含 column
    /// 与 reverse；row-reverse / 非 flex 元素的纵向 scrollTop 仍是常规
    /// [0,max]）。链节点与标志存 `window.__rclawSc`，后续读数与补滚全部引用
    /// 同一登记：前后两次 elementFromPoint 会因滚动把原容器移出落点，跨登记
    /// 读数会错账。返回链上 scrollTop 基线之和（是否截断保存在登记状态里，
    /// js_scroll 的 END 语义天然尊重它）。
    /// 表达式必须返回字符串（[`evaluate_string`] 契约）。
    async fn scroll_target_begin(&self, session: &Session, x: i64, y: i64) -> Result<i64, String> {
        let js = format!(
            "(function(){{var se=document.scrollingElement||document.documentElement;\
             var chain=[],rev=[],el=document.elementFromPoint({x},{y}),bounded=false;\
             while(el&&el!==se){{\
             var cs=null;try{{cs=window.getComputedStyle(el);}}catch(e){{cs=null;}}\
             var oy=cs?cs.overflowY:'';\
             if(oy==='auto'||oy==='scroll'||oy==='overlay'){{\
             if(el.scrollHeight>el.clientHeight+1){{\
             var disp=cs?(cs.display||''):'';var fd=cs?(cs.flexDirection||''):'';\
             chain.push(el);rev.push(disp.indexOf('flex')>=0&&fd.indexOf('column')>=0&&fd.indexOf('reverse')>=0);}}\
             var ob=cs?(cs.overscrollBehaviorY||''):'';\
             if(ob==='contain'||ob==='none'){{bounded=true;break;}}}}\
             el=el.parentElement;}}\
             if(!bounded){{chain.push(se);rev.push(false);}}\
             window.__rclawSc={{c:chain,r:rev}};\
             var sum=0;for(var i=0;i<chain.length;i++)sum+=(chain[i].scrollTop|0);\
             return String(sum+'|'+(bounded?'1':'0'));}})()"
        );
        let reply = session
            .call(
                "Runtime.evaluate",
                json!({ "expression": js, "returnByValue": true }),
                Duration::from_secs(10),
            )
            .await?;
        let value = evaluate_string(&reply)?;
        value
            .split('|')
            .next()
            .and_then(|v| v.parse::<i64>().ok())
            .ok_or_else(|| format!("解析滚动进度失败: {value}"))
    }

    /// 读登记滚动链（[`Self::scroll_target_begin`]）全部节点 scrollTop 之和。
    /// 单方向滚动下「和之差 = 各节点位移之和」= 内容实际移动距离（内层到边
    /// 链到外层的情形由根节点项覆盖）。
    async fn scroll_target_pos(&self, session: &Session) -> Result<i64, String> {
        let reply = session
            .call(
                "Runtime.evaluate",
                json!({
                    "expression":
                        "(function(){var s=window.__rclawSc;if(!s)return '0';\
                         var t=0;for(var i=0;i<s.c.length;i++)t+=(s.c[i].scrollTop|0);\
                         return String(t);})()",
                    "returnByValue": true
                }),
                Duration::from_secs(10),
            )
            .await?;
        evaluate_string(&reply)?
            .parse::<i64>()
            .map_err(|e| format!("解析滚动进度失败: {e}"))
    }

    /// 页内程序化滚动：对 [`Self::scroll_target_begin`] 登记的滚动链分步滚动
    /// （每步申请 ≤240px，30-90ms 节奏）。每步从链最内层起找第一个「当前方向
    /// 仍有剩余空间」的节点执行滚动并**回传实际位移**（容量可能小于申请步长：
    /// 容器钳位后余量自动落到链上更外层），Rust 按实际位移扣账，避免「扣了
    /// 整步、只滚了几像素」的距离丢失。容量按登记的反向标志（flex-direction
    /// 含 reverse，如 column-reverse 的 scrollTop ∈ [-max,0]）选择公式，常规
    /// 容器不受影响。根（scrollingElement）用 window.scrollBy，普通元素用
    /// `Element.scrollBy({behavior:'instant'})` 强制瞬时（容器声明
    /// scroll-behavior:smooth 时默认动画会被步进节奏打断而丢距离；老内核无
    /// Element.scrollBy 时回退 scrollTop 直写）。滚动事件照常派发（懒加载/
    /// IntersectionObserver 正常触发）；返回实际位移的绝对值。
    async fn js_scroll(&self, session: &Session, px: i64) -> Result<i64, String> {
        let dir = px.signum();
        let mut moved_total: i64 = 0;
        let mut remaining = px.abs();
        while remaining > 0 {
            let step = remaining.min(240);
            let js = format!(
                "(function(){{var s=window.__rclawSc;if(!s||!s.c)return 'NO';\
                 function sum(){{var t=0;for(var i=0;i<s.c.length;i++)t+=(s.c[i].scrollTop|0);return t;}}\
                 var b=sum();\
                 for(var i=0;i<s.c.length;i++){{var n=s.c[i];\
                 var max=n.scrollHeight-n.clientHeight,st=n.scrollTop,cap;\
                 if(s.r[i]){{cap=({dir}>0)?(-st):(st+max);}}\
                 else{{cap=({dir}>0)?(max-st):st;}}\
                 if(cap>1){{\
                 if(n===document.scrollingElement||n===document.documentElement){{\
                 window.scrollBy({{left:0,top:{},behavior:'instant'}});}}\
                 else if(typeof n.scrollBy==='function'){{\
                 n.scrollBy({{left:0,top:{},behavior:'instant'}});}}\
                 else{{n.scrollTop+={};}}\
                 return String(sum()-b);}}}}\
                 return 'END';}})()",
                dir * step,
                dir * step,
                dir * step
            );
            let reply = session
                .call(
                    "Runtime.evaluate",
                    json!({ "expression": js, "returnByValue": true }),
                    Duration::from_secs(10),
                )
                .await?;
            let value = evaluate_string(&reply)?;
            if value == "END" {
                break; // 整条链在当前方向都已到边：剩余距离滚不动，如实上报。
            }
            let moved = value.parse::<i64>().unwrap_or(0).abs();
            moved_total += moved;
            remaining -= moved;
            if moved == 0 {
                break; // 该步实际没滚到任何距离：继续只会空转，如实上报。
            }
            tokio::time::sleep(Duration::from_millis(jitter(30, 90) as u64)).await;
        }
        Ok(moved_total)
    }

    /// 事件坐标基准视口（双口径）：桌面滚轮用窗口 innerWidth/innerHeight
    /// （滚轮坐标必须落在窗口视口内，screen 是整个显示器）；移动触摸用可视
    /// 视口——页面被缩小适配时（无 viewport meta 的桌面页，Chrome 手机版按
    /// 980 布局回退再缩小）innerWidth 是布局宽度而非可视宽度，可视宽度恒等
    /// 于 screen.width，可视高度 = visualViewport.height × scale（未缩放时
    /// ≈ innerHeight）。
    async fn viewports(&self, session: &Session) -> Result<((i64, i64), (i64, i64)), String> {
        let reply = session
            .call(
                "Runtime.evaluate",
                json!({
                    "expression":
                        "JSON.stringify({iw:window.innerWidth||800,ih:window.innerHeight||600,sw:(screen&&screen.width)||0,vh:(function(){var vv=window.visualViewport;return vv?Math.round(vv.height*(vv.scale||1)):0;})()})",
                    "returnByValue": true
                }),
                Duration::from_secs(10),
            )
            .await?;
        let value = evaluate_string(&reply)?;
        let parsed: Value =
            serde_json::from_str(&value).map_err(|e| format!("解析视口尺寸失败: {e}"))?;
        let inner = (
            parsed.get("iw").and_then(Value::as_i64).unwrap_or(800),
            parsed.get("ih").and_then(Value::as_i64).unwrap_or(600),
        );
        let screen_w = parsed.get("sw").and_then(Value::as_i64).unwrap_or(0);
        let visual_h = parsed.get("vh").and_then(Value::as_i64).unwrap_or(0);
        let visual = (
            if screen_w > 0 { screen_w } else { inner.0 },
            if visual_h > 0 { visual_h } else { inner.1 },
        );
        Ok((inner, visual))
    }

    /// 截图到工作区 browser/ 下（内核相关实现见
    /// [`DevtoolsTransport::capture_screenshot`]），返回人读结果。
    pub async fn screenshot(&self) -> Result<String, String> {
        let _op = self.op.lock().await;
        let session = self.session().await?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let name = format!("screenshot_{stamp}.png");
        let dest = self.transport.screenshot_dir().join(&name);
        // 全新工作区还没有 browser/ 目录，先建（幂等）。
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("创建截图目录失败: {e}"))?;
        }
        self.transport.capture_screenshot(&session, &dest).await
    }

    /// 销毁会话（下次任何 browser 工具使用时自动重建）。
    pub async fn close(&self) -> Result<String, String> {
        let _op = self.op.lock().await;
        {
            let mut guard = self.session.lock().await;
            *guard = None;
        }
        self.transport.shutdown().await?;
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

// ---------------------------------------------------------------------------
// 拟人化：UA 清洗、webdriver 掩蔽、交互抖动
// ---------------------------------------------------------------------------

/// `navigator.webdriver` 掩蔽脚本：挂在 `Navigator.prototype` 上，经
/// `Page.addScriptToEvaluateOnNewDocument` 对之后每次新文档生效。自动化
/// 最经典的 JS 指纹，反爬站点第一查它。
const WEBDRIVER_MASK_JS: &str =
    "Object.defineProperty(Navigator.prototype, 'webdriver', { get: () => undefined });";

/// stealth 注入（真机 bot.sannysoft.com + Runtime.evaluate 探针实测的暴露项，
/// 每项对应基线证据；新文档生效）：
/// - `document.hasFocus()`：离屏 WebView / 无焦点悬浮窗实测 false，前台浏览器
///   恒 true——CDP 焦点模拟（Emulation.setFocusEmulationEnabled）是主修，此
///   覆写兜底内核不支持模拟时的老版本 WebView；
/// - `window.chrome`：Chrome Mobile 有、WebView 无（最经典的 WebView 判据，
///   sannysoft「Chrome (New)」红），注入只读最小 shim（app/runtime/csi/loadTimes）；
/// - `navigator.plugins/mimeTypes`：WebView 空数组（sannysoft 两项红），补齐
///   Chrome Mobile 标准 PDF Viewer 三件套（PluginArray/MimeTypeArray 原型链
///   保真，instanceof 检测过）。
/// 明确不做（实测无需或有意放弃，防后人补）：WebGL UNMASKED_RENDERER hook——
/// 真机实测已是真 GPU（Adreno 616，离屏 + 软件层不影响 WebGL 的 GL 上下文，
/// sannysoft 该项绿）；canvas 噪声注入破坏正常页面渲染；字体/AudioContext
/// 指纹性价比低；Runtime.enable 深层检测对抗无干净解法。
const STEALTH_JS: &str = r#"(function(){
  try { Object.defineProperty(document, 'hasFocus', { value: function(){ return true; }, configurable: true }); } catch (e) {}
  try {
    if (typeof window.chrome === 'undefined') {
      var noop = function(){};
      var chrome = {
        app: { isInstalled: false,
               InstallState: { DISABLED: 'disabled', INSTALLED: 'installed', NOT_INSTALLED: 'not_installed' },
               RunningState: { CANNOT_RUN: 'cannot_run', READY_TO_RUN: 'ready_to_run', RUNNING: 'running' },
               getDetails: function(){ return null; }, getIsInstalled: function(){ return false; },
               installState: function(){ return 'disabled'; } },
        runtime: { PlatformOs: { MAC: 'mac', WIN: 'win', ANDROID: 'android', CROS: 'cros', LINUX: 'linux', OPENBSD: 'openbsd' },
                   PlatformArch: { ARM: 'arm', ARM64: 'arm64', MIPS: 'mips', MIPS64: 'mips64', X86_32: 'x86-32', X86_64: 'x86-64' },
                   PlatformNaclArch: { ARM: 'arm', MIPS: 'mips', MIPS64: 'mips64', X86_32: 'x86-32', X86_64: 'x86-64' },
                   OnInstalledReason: { CHROME_UPDATE: 'chrome_update', INSTALL: 'install', SHARED_MODULE_UPDATE: 'shared_module_update', UPDATE: 'update' },
                   OnRestartReason: { APP_UPDATE: 'app_update', OS_UPDATE: 'os_update', SESSION_UPDATE: 'session_update' },
                   RequestUpdateCheckStatus: { NO_UPDATE: 'no_update', THROTTLED: 'throttled', UPDATE_AVAILABLE: 'update_available' },
                   connect: noop, sendMessage: noop },
        csi: function(){ return { startE: Date.now(), onloadT: Date.now(), pageT: 0, tran: 0 }; },
        loadTimes: function(){ return { requestTime: Date.now() / 1000, startLoadTime: Date.now() / 1000, commitLoadTime: Date.now() / 1000, finishDocumentLoadTime: Date.now() / 1000, finishLoadTime: Date.now() / 1000, firstPaintTime: Date.now() / 1000, firstPaintAfterLoadTime: 0, navigationType: 'Other', wasFetchedViaSpdy: false, wasNpnNegotiated: true, npnNegotiatedProtocol: 'h2', wasAlternateProtocolAvailable: false, connectionInfo: 'h2' }; }
      };
      Object.defineProperty(window, 'chrome', { value: chrome, writable: false, configurable: false });
    }
  } catch (e) {}
  try {
    if (navigator.plugins && navigator.plugins.length === 0) {
      var makeMime = function(type){
        var m = Object.create(window.MimeType ? MimeType.prototype : Object.prototype);
        Object.defineProperties(m, {
          type: { value: type }, suffixes: { value: 'pdf' },
          description: { value: 'Portable Document Format' }
        });
        return m;
      };
      var pdf = makeMime('application/pdf');
      var textPdf = makeMime('text/pdf');
      var makePlugin = function(name){
        var p = Object.create(window.Plugin ? Plugin.prototype : Object.prototype);
        Object.defineProperties(p, {
          name: { value: name }, filename: { value: 'internal-pdf-viewer' },
          description: { value: 'Portable Document Format' }, length: { value: 2 },
          0: { value: pdf }, 1: { value: textPdf }
        });
        return p;
      };
      var list = ['PDF Viewer', 'Chrome PDF Viewer', 'Chromium PDF Viewer'].map(makePlugin);
      // MimeType.enabledPlugin 反向引用：不给自有值的话，页面读它会命中原生
      // getter 并因接收者无内部槽抛 TypeError（PDF 探测页常见路径）。
      try {
        Object.defineProperty(pdf, 'enabledPlugin', { value: list[0] });
        Object.defineProperty(textPdf, 'enabledPlugin', { value: list[0] });
      } catch (e) {}
      var plugins = Object.create(window.PluginArray ? PluginArray.prototype : Object.prototype);
      Object.defineProperty(plugins, 'length', { value: list.length });
      for (var i = 0; i < list.length; i++) {
        (function(idx){
          Object.defineProperty(plugins, idx, { value: list[idx], enumerable: true });
          Object.defineProperty(plugins, list[idx].name, { value: list[idx] });
        })(i);
      }
      Object.defineProperty(plugins, 'item', { value: function(i){ return list[i] || null; } });
      Object.defineProperty(plugins, 'namedItem', { value: function(n){
        for (var j = 0; j < list.length; j++) { if (list[j].name === n) return list[j]; }
        return null;
      } });
      Object.defineProperty(navigator, 'plugins', { value: plugins, configurable: true });
      var mimes = Object.create(window.MimeTypeArray ? MimeTypeArray.prototype : Object.prototype);
      var mList = [pdf, textPdf];
      Object.defineProperty(mimes, 'length', { value: mList.length });
      for (var k = 0; k < mList.length; k++) {
        (function(idx){ Object.defineProperty(mimes, idx, { value: mList[idx], enumerable: true }); })(k);
      }
      Object.defineProperty(mimes, 'item', { value: function(i){ return mList[i] || null; } });
      Object.defineProperty(mimes, 'namedItem', { value: function(n){
        for (var l = 0; l < mList.length; l++) { if (mList[l].type === n) return mList[l]; }
        return null;
      } });
      Object.defineProperty(navigator, 'mimeTypes', { value: mimes, configurable: true });
    }
  } catch (e) {}
})()"#;

/// 会话建立后的拟人化注入（各步骤失败仅记日志不阻断——内核/版本能力有差异）：
/// UA 清洗覆写（带一致性的 Client Hints 元数据）+ 焦点模拟 + webdriver 掩蔽 +
/// stealth 注入。返回移动端判定（Android UA → touch/键盘事件词汇），与 UA
/// 覆写同源（同一份 UA 判定词汇才自洽）。
async fn humanize_session(session: &Session) -> bool {
    let ua = match session
        .call(
            "Runtime.evaluate",
            json!({ "expression": "navigator.userAgent", "returnByValue": true }),
            Duration::from_secs(10),
        )
        .await
    {
        Ok(reply) => evaluate_string(&reply).unwrap_or_default(),
        Err(error) => {
            tracing::warn!("CDP 拟人化：读取 UA 失败（跳过 UA 覆写）: {error}");
            return false;
        }
    };
    if ua.trim().is_empty() {
        tracing::warn!("CDP 拟人化：UA 为空（跳过 UA 覆写）");
        return false;
    }
    let mobile = ua_is_mobile(&ua);
    let cleaned = humanize_ua(&ua);
    let mut params = json!({ "userAgent": cleaned });
    // Client Hints 元数据优先沿用内核真实值（UA 只摘了 Headless/wv 标记，版本/
    // 平台本就与内核一致，真实值最不容易露馅）；拿不到或键不合 CDP 契约时退回
    // 从 UA 字符串合成的保守值。
    let metadata = match read_ua_metadata(session).await {
        Some(metadata) if valid_ua_metadata(&metadata) => Some(metadata),
        _ => user_agent_metadata(&cleaned),
    };
    if let Some(metadata) = metadata {
        params["userAgentMetadata"] = metadata;
    }
    match session
        .call(
            "Network.setUserAgentOverride",
            params,
            Duration::from_secs(10),
        )
        .await
    {
        Ok(_) => tracing::info!(ua = %cleaned, "CDP 拟人化：UA 已覆写"),
        Err(error) => {
            // 元数据不合 CDP 契约会被整个拒绝：退回仅覆写 UA 字符串，保住
            // 去 Headless/wv 标记的主要收益。
            tracing::warn!("CDP 拟人化：带元数据覆写失败（{error}），退回仅 UA 字符串覆写");
            match session
                .call(
                    "Network.setUserAgentOverride",
                    json!({ "userAgent": cleaned }),
                    Duration::from_secs(10),
                )
                .await
            {
                Ok(_) => {
                    tracing::info!(ua = %cleaned, "CDP 拟人化：UA 已覆写（无 Client Hints 元数据）")
                }
                Err(error) => tracing::warn!("CDP 拟人化：UA 覆写失败: {error}"),
            }
        }
    }
    if let Err(error) = session
        .call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": WEBDRIVER_MASK_JS }),
            Duration::from_secs(10),
        )
        .await
    {
        tracing::warn!("CDP 拟人化：webdriver 掩蔽注入失败: {error}");
    }
    // 焦点模拟：离屏 WebView / 无焦点悬浮窗的 document.hasFocus() 实测 false，
    // 前台浏览器恒 true。CDP 焦点模拟开启后内核还会派发真实 focus 事件。
    if let Err(error) = session
        .call(
            "Emulation.setFocusEmulationEnabled",
            json!({ "enabled": true }),
            Duration::from_secs(10),
        )
        .await
    {
        tracing::warn!("CDP 拟人化：焦点模拟开启失败（hasFocus 覆写兜底）: {error}");
    }
    if let Err(error) = session
        .call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": STEALTH_JS }),
            Duration::from_secs(10),
        )
        .await
    {
        tracing::warn!("CDP 拟人化：stealth 注入失败: {error}");
    }
    mobile
}

/// UA 拟人化清洗：桌面无头实例自报 `HeadlessChrome/…`、安卓 WebView 自报
/// `; wv`——都是反爬体系的头号特征，清洗成同内核的普通浏览器身份。
fn humanize_ua(ua: &str) -> String {
    let cleaned = ua.replace("HeadlessChrome/", "Chrome/");
    if cleaned.contains("Android") {
        cleaned.replace("; wv", "")
    } else {
        cleaned
    }
}

/// 交互事件词汇判定：Android UA（手机/平板）→ touch 拖拽 + 逐字符键盘
/// （真机没有滚轮，也不发鼠标事件——事件词汇与 UA 矛盾是行为检测证据）；
/// 其余（桌面无头实例）→ 鼠标/滚轮/IME 通路。
fn ua_is_mobile(ua: &str) -> bool {
    ua.contains("Android")
}

/// 读取内核真实的 Client Hints 元数据：getHighEntropyValues 只回高熵部分，
/// brands/mobile 在低熵的 `navigator.userAgentData` 上，合在一起才是 CDP
/// setUserAgentOverride 需要的完整形态。
async fn read_ua_metadata(session: &Session) -> Option<Value> {
    let js = "(async()=>{try{if(!navigator.userAgentData)return '';const d=await navigator.userAgentData.getHighEntropyValues(['architecture','model','platform','platformVersion','uaFullVersion','fullVersionList']);return JSON.stringify({brands:navigator.userAgentData.brands,mobile:navigator.userAgentData.mobile,...d})}catch(e){return ''}})()";
    let reply = session
        .call(
            "Runtime.evaluate",
            json!({ "expression": js, "returnByValue": true, "awaitPromise": true }),
            Duration::from_secs(10),
        )
        .await
        .ok()?;
    let value = evaluate_string(&reply).ok()?;
    if value.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&value).ok()
}

/// CDP `userAgentMetadata` 的必填键：缺任何一个整个覆写调用都会被拒。
/// 页面可用 Permissions-Policy 削减高熵字段，真实值必须过这道校验才可用。
fn valid_ua_metadata(metadata: &Value) -> bool {
    const REQUIRED: [&str; 7] = [
        "brands",
        "platform",
        "platformVersion",
        "architecture",
        "model",
        "mobile",
        "fullVersionList",
    ];
    let Some(map) = metadata.as_object() else {
        return false;
    };
    REQUIRED.iter().all(|key| map.contains_key(*key))
        && map
            .get("brands")
            .and_then(Value::as_array)
            .is_some_and(|brands| !brands.is_empty())
}

/// 从清洗后的 UA 合成 Client Hints 元数据（内核真实值拿不到时的兜底）：
/// `Sec-CH-UA` 系列请求头由它生成，只改 UA 字符串不动 Client Hints，两边
/// 对不上等于自报覆写。只陈述 UA 里可考证的事实——平台/OS 版本/架构/机型
/// 全部取自 UA 本身，拿不准的字段宁缺毋滥，不编造高熵指纹。
fn user_agent_metadata(ua: &str) -> Option<Value> {
    let rest = ua.split("Chrome/").nth(1)?;
    let full = rest.split(' ').next()?;
    let major = full.split('.').next()?;
    if major.is_empty() {
        return None;
    }
    // Edge 的 UA 额外带 Edg/<full>：品牌与完整版本必须跟 Edge 走，否则
    // UA 自报 Edge、Sec-CH-UA 却说 Chrome。
    let edge_full = ua
        .split("Edg/")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .filter(|version| !version.is_empty());
    let brand_name = if edge_full.is_some() {
        "Microsoft Edge"
    } else {
        "Google Chrome"
    };
    let brand_full = edge_full.unwrap_or(full);
    // 平台与 OS 版本考证自 UA 本身（浏览器就跑在本机，UA 即事实）；
    // macOS 的 UA 冻结在 10_15_7，照实换算不编造真实系统版本；Linux 无
    // 版本信息，platformVersion 留空。
    let android = ua.contains("Android");
    let (platform, platform_version) = if android {
        // "…; Android 13; …" → "13.0.0"；已有小数段的（如 "8.1.0"）照原样保留，
        // 不能拼成 "8.1.0.0.0"。
        let version = ua
            .split("Android ")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .map(str::trim)
            .unwrap_or_default();
        let platform_version = if version.is_empty() {
            String::new()
        } else if version.contains('.') {
            version.to_string()
        } else {
            format!("{version}.0.0")
        };
        ("Android", platform_version)
    } else if ua.contains("Windows NT") {
        // "Windows NT 10.0"（Win10/11 同版号）→ "10.0.0"。
        let version = ua
            .split("Windows NT ")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .map(str::trim)
            .unwrap_or("10.0");
        ("Windows", format!("{version}.0"))
    } else if ua.contains("Mac OS X") {
        let version = ua
            .split("Mac OS X ")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .unwrap_or("10_15_7")
            .replace('_', ".");
        ("macOS", version)
    } else if ua.contains("Linux") {
        ("Linux", String::new())
    } else {
        return None;
    };
    // 架构只在 UA 明说时才填（安卓 UA 一般不写，留空不编造——x86 模拟器
    // 配 "arm" 是自曝）；桌面 UA 的 64 位标记映射 x86。
    let architecture = if ua.contains("x86_64") || ua.contains("Win64") || ua.contains("WOW64") {
        "x86"
    } else if ua.contains("aarch64") || ua.contains("arm") {
        "arm"
    } else if ua.contains("x86") {
        "x86"
    } else {
        ""
    };
    // 手机 / 平板按 Chrome 的 UA 规则：Mobile 令牌区分（平板 UA 无 Mobile）。
    let mobile = android && ua.contains("Mobile");
    // 安卓 UA 括号段带机型（如 "…; Android 13; Pixel 7"），model 字段与之一致；
    // 真实 WebView 常带 Build 指纹（"Pixel 7 Build/TQ3A…"），只留机型名。
    let model = if android {
        ua.split('(')
            .nth(1)
            .and_then(|inner| inner.split(')').next())
            .and_then(|seg| seg.split(';').nth(2))
            // 先剥 Build 指纹再 trim：无机型的预览版 UA（"...; Build/BP22…"）
            // 截完是空串，不能把 "Build/…" 当机型写进 Client Hints。
            .and_then(|model| model.split(" Build/").next())
            .unwrap_or_default()
            .trim()
            .to_string()
    } else {
        String::new()
    };
    Some(json!({
        "brands": [
            { "brand": "Not A;Brand", "version": "99" },
            { "brand": "Chromium", "version": major },
            { "brand": brand_name, "version": brand_full.split('.').next().unwrap_or(major) },
        ],
        "fullVersion": brand_full,
        "fullVersionList": [
            { "brand": "Not A;Brand", "version": "99.0.0.0" },
            { "brand": "Chromium", "version": full },
            { "brand": brand_name, "version": brand_full },
        ],
        "platform": platform,
        "platformVersion": platform_version,
        "architecture": architecture,
        "model": model,
        "mobile": mobile,
    }))
}

/// 区间内随机整数（交互拟人抖动用；时钟纳秒作熵源，失败取下界）。
fn jitter(lo: i64, hi: i64) -> i64 {
    let span = (hi - lo + 1).max(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as i64)
        .unwrap_or(0);
    lo + nanos % span
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
        push_blocked_note(&mut out, &info.blocked);
    }
    out
}

/// 风控/同意页标注的人读说明（browser_navigate 与 click 后的页面报告共用）。
fn push_blocked_note(out: &mut String, reason: &str) {
    out.push_str(&format!(
        "\n\n⚠️ 检测到{reason}——这是 Google 的风控/同意页，非程序故障。风控页（CAPTCHA）\
         与出口 IP 信誉相关，立刻重试通常加重；同意页可 browser_snapshot 后点按勾选框继续。"
    ));
}

// ---------------------------------------------------------------------------
// 页面脚本常量
// ---------------------------------------------------------------------------

/// 页面信息（URL / 标题 / 风控与同意页检测）。
const PAGE_INFO_JS: &str = "(function(){var href=location.href||'';var text=(document.body&&document.body.innerText)||'';var blocked='';if(/google\\.[a-z.]+\\/sorry/i.test(href)||/unusual traffic/i.test(text))blocked='Google CAPTCHA 风控页（/sorry/）';else if(/^consent\\./.test(location.hostname)||/before you continue/i.test(text))blocked='同意页（consent）';return JSON.stringify({url:href,title:document.title||'',blocked:blocked});})()";

/// 紧凑可交互元素清单：给每个可交互/标题元素盖 data-rclaw-ref 章，输出
/// `[ref] 角色: 文本 → 链接` 扁平列表（模型按 ref 定位操作）。
/// 盲区提示触发阈值：含 ≥30 字文本的非交互元素达到该数量才提示。表单页/
/// 空白加载页不触发；2026-10-21 华为真机京东结果页 12 张商品卡实测命中。
const SNAPSHOT_HINT_MIN_NON_INTERACTIVE: u64 = 5;

/// 快照盲区提示：快照只收录可交互元素，「快照看不见」与「页面没有」在返回
/// 值里无法区分——2026-10-21 京东真机实踩：手机结果页商品卡是无 href 的
/// div，模型拿快照 0 商品链接当「商品接口软返空」证据收尾，页面实有 12 张
/// 可提取卡片。在快照尾部如实标注这批内容的存在与出路，让误判在工具返回
/// 值内部就被反驳，而不是依赖技能文本里的远处规则。
fn append_snapshot_hint(text: String, non_interactive: Option<u64>) -> String {
    let Some(n) = non_interactive else {
        return text;
    };
    if n < SNAPSHOT_HINT_MIN_NON_INTERACTIVE {
        return text;
    }
    format!(
        "{text}\n\n提示：本页另有 {n} 处含文本的非交互内容（图文卡片/数据行/文章段落等），\
         本快照只收录可交互元素、看不到它们——判定「页面是否为空、有没有目标内容」\
         请用 browser_extract 按 CSS 选择器提取，不要以本快照为准。"
    )
}

/// 快照盲区普查（SNAPSHOT_JS 尾部的 non_interactive 计数）三条不变量：
/// ① 排除表与快照收录选择器对齐（含 h1-h3——已可见的标题不是盲区，codex r1 P3）；
/// ② 只数「直接文本」——元素自己的文本节点合计 ≥30 字。textContent 不行：它含全部
///    子孙文本，会把快照已可见的 button 文本经父容器重复计入（codex r2 P3），也会让
///    公共容器一次吞掉全部卡片的计数（codex r2 P2）；③ 不做祖先去重——按直接文本
///    计数天然无父子重复。已知边界：标题拆成多个 <30 字短 span 的卡片不计（阈值
///    信号，不求精确）；计数是「≥5 触发提示」的信号，不求精确 inventory。
const SNAPSHOT_JS: &str = "(function(){function clean(s){return (s||'').replace(/\\s+/g,' ').trim();}var out=[];var n=0;var roles={a:'link',button:'button',input:'textbox',textarea:'textbox',select:'combobox',h1:'heading',h2:'heading',h3:'heading',img:'img'};var nodes=document.querySelectorAll('a,button,input,textarea,select,[role],[onclick],h1,h2,h3');for(var i=0;i<nodes.length&&out.length<250;i++){var el=nodes[i];var tag=el.tagName.toLowerCase();var role=el.getAttribute('role')||roles[tag]||'';var text=tag==='input'||tag==='textarea'?(el.value||el.placeholder||''):(tag==='select'?(el.selectedIndex>=0?clean(el.options[el.selectedIndex].text||''):'')||('[下拉·共'+el.options.length+'项]'):clean(el.innerText||''));var href=el.href||'';if(!text&&!href)continue;var ref=++n;try{el.setAttribute('data-rclaw-ref',ref);}catch(e){}var line='['+ref+'] '+role+': '+text.slice(0,120);if(href)line+=' → '+String(href).slice(0,150);out.push(line);}var ni=0;try{var skip={SCRIPT:1,STYLE:1,NOSCRIPT:1,TEMPLATE:1};var all=document.body?document.body.querySelectorAll('*'):[];for(var j=0;j<all.length;j++){var e=all[j];if(skip[e.tagName])continue;if(e.closest&&e.closest('a,button,input,textarea,select,[role],[onclick],h1,h2,h3'))continue;var t='';for(var c=e.firstChild;c;c=c.nextSibling){if(c.nodeType===3)t+=c.nodeValue||'';}if(clean(t).length>=30)ni++;}}catch(e2){}return JSON.stringify({ok:out.length>0,text:out.length?('共 '+out.length+' 项（截前 250）\\n'+out.join('\\n')):'页面无可交互元素',non_interactive:ni});})()";

/// 媒体资源提取（browser_media）：img + video（含 source 子元素）+ og 元信息，
/// 按 DOM 顺序统一遍历后再限量 40 条（分开遍历会让图片先耗尽配额、视频整页
/// 丢失）。小图过滤只认「已加载内容的真实渲染尺寸」（currentSrc 为 HTTP 且
/// naturalWidth<100）——懒加载占位图的 1×1 尺寸不描述真图，不能参与否决；
/// 相对/协议相对 URL 按 document.baseURI 解析；src 为 data: 占位时回退
/// data-src 取真链——给「图文/视频推送到聊天」提供 URL 素材（snapshot 只列
/// 可交互元素，媒体 URL 只有这里拿得到）。
const MEDIA_JS: &str = "(function(){var LIMIT=40,out=[],seen={};function abs(u){try{return new URL(u,document.baseURI).href;}catch(e){return '';}}function usable(u){u=(u||'').trim();if(!u||/^(data|blob):/i.test(u))return '';var a=abs(u);return /^https?:/.test(a)?a:'';}function push(kind,url,alt){url=usable(url);if(!url)return;var k=kind+' '+url;if(seen[k])return;seen[k]=1;if(out.length>=LIMIT)return;out.push((out.length+1)+'. ['+kind+'] '+url+(alt?(' | '+(alt||'').replace(/\\s+/g,' ').slice(0,60)):''));}var nodes=document.querySelectorAll('img,video,meta[property=\"og:image\"],meta[property=\"og:video\"],meta[property=\"og:video:url\"]');for(var i=0;i<nodes.length;i++){var el=nodes[i];var tag=el.tagName;if(tag==='IMG'){var prim=usable(el.currentSrc)||usable(el.getAttribute('src'));var lazy=usable(el.getAttribute('data-src'));var cur=usable(el.currentSrc);var u=lazy||prim;var w=el.naturalWidth||0;if(cur&&w&&w<100&&cur===u)continue;push('图',u,el.alt||el.title||'');}else if(tag==='VIDEO'){var vc=[el.currentSrc,el.getAttribute('src')];var vu='';for(var c2=0;c2<vc.length&&!vu;c2++)vu=usable(vc[c2]);if(!vu){var ss=el.querySelectorAll('source');for(var s2=0;s2<ss.length&&!vu;s2++)vu=usable(ss[s2].getAttribute('src'));}push('视频',vu,'时长约'+Math.round(el.duration||0)+'秒');}else{var mc=el.getAttribute('content')||'';var mp=el.getAttribute('property')||'';push(/video/i.test(mp)?'视频':'og图',mc,'');}}return JSON.stringify({ok:true,text:out.length?out.join('\\n'):'页面未发现可提取的图片/视频'});})()";

/// 页面提取（browser_navigate）：引擎感知的搜索结果列表
/// （bing/baidu/google，过滤广告）+ Readability 式主文正文 + 风控/同意页
/// 检测。google 分支的摘要选择器加 gbox 兜底。
const EXTRACT_JS: &str = r##"(function () {
  var out = { title: document.title || "", url: location.href, text: "", results: [], blocked: "" };
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
  if (/(^|\.)bing\.com$/.test(host)) {
    var items = document.querySelectorAll("#b_results li.b_algo");
    for (var i = 0; i < items.length && out.results.length < 10; i++) {
      var li = items[i];
      if (adMarked(li)) continue;
      var a = li.querySelector("h2 a");
      if (!a) continue;
      var cap = li.querySelector(".b_caption p, .b_algoSlug, .b_caption");
      push(a.textContent, a.href, cap ? cap.textContent : "");
    }
  } else if (/(^|\.)google\.com$/.test(host)) {
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
  } else if (/(^|\.)baidu\.com$/.test(host)) {
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
  var allText = (document.body && document.body.innerText) || "";
  if (/google\.[a-z.]+\/sorry/i.test(location.href) || /unusual traffic/i.test(allText)) out.blocked = "Google CAPTCHA 风控页（/sorry/）";
  else if (/^consent\./.test(location.hostname) || /before you continue/i.test(allText)) out.blocked = "同意页（consent）";
  return JSON.stringify(out);
})()"##;

// ---------------------------------------------------------------------------
// 工具面
// ---------------------------------------------------------------------------

/// 进程级共享内核：注册可能发生多次（安卓服务重启重建注册表、桌面重装配），
/// 而底层会话是进程单例（安卓 BrowserSession 静态字段 / 桌面 HEADLESS_CHILD），
/// 两个内核实例驱动同一会话会互相串页、互相关闭——首个 transport 定格复用。
static SHARED_BROWSER: std::sync::OnceLock<Arc<CdpBrowser>> = std::sync::OnceLock::new();

/// 注册 CDP 浏览器工具面：browser_* 拟人化浏览器工具（全部共享同一内核会话）。
/// 检索不设专用工具——模型直接 browser_navigate 到任意搜索引擎的结果页 URL。
/// 这是唯一的浏览器工具面；CLI/Termux 不注册（无浏览器能力）。
pub fn register(tools: &mut crate::ToolRegistry, transport: Arc<dyn DevtoolsTransport>) {
    let browser = SHARED_BROWSER
        .get_or_init(|| Arc::new(CdpBrowser::new(transport)))
        .clone();
    tools.register(Box::new(BrowserNavigateTool {
        browser: browser.clone(),
    }));
    tools.register(Box::new(BrowserSnapshotTool {
        browser: browser.clone(),
    }));
    tools.register(Box::new(BrowserExtractTool {
        browser: browser.clone(),
    }));
    tools.register(Box::new(BrowserClickTool {
        browser: browser.clone(),
    }));
    tools.register(Box::new(BrowserTypeTool {
        browser: browser.clone(),
    }));
    tools.register(Box::new(BrowserScrollTool {
        browser: browser.clone(),
    }));
    tools.register(Box::new(BrowserScreenshotTool {
        browser: browser.clone(),
    }));
    tools.register(Box::new(BrowserMediaTool {
        browser: browser.clone(),
    }));
    tools.register(Box::new(BrowserCloseTool { browser }));
}

/// 关闭共享浏览器会话（桌面可见形态切换用：先 set_visible 再关，下次任一
/// browser_* 调用/打开浏览器即以新形态重建；profile 延续，cookie 信誉保留）。
pub async fn close_shared_browser() -> Result<(), String> {
    match SHARED_BROWSER.get() {
        Some(browser) => browser.close().await.map(|_| ()),
        None => Ok(()),
    }
}

/// 共享内核句柄（命令层立即拉起浏览器窗口用；未注册返回 None）。
pub fn shared_browser() -> Option<Arc<CdpBrowser>> {
    SHARED_BROWSER.get().cloned()
}

impl CdpBrowser {
    /// 仅作废缓存的 CDP 会话连接（不关内核、不抢 op 锁）。可见形态切换用：
    /// transport.shutdown 后 manager 可能尚未退出，直接 ensure_session 会复用
    /// 已死通道——先作废，下次 session() 必以新形态重连。
    pub async fn invalidate_session(&self) {
        let mut guard = self.session.lock().await;
        *guard = None;
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
    Ok(
        if url.starts_with("http://") || url.starts_with("https://") {
            url.to_string()
        } else {
            format!("https://{url}")
        },
    )
}

/// 工具样板：name / description / schema / 串行安全位（共享长驻会话，保守
/// 不并发）+ execute（各工具自带）。
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
    browser: Arc<CdpBrowser>,
}

pub struct BrowserMediaTool {
    browser: Arc<CdpBrowser>,
}

browser_tool_impl!(
    BrowserMediaTool,
    "browser_media",
    "CDP 浏览器：提取当前页面的图片/视频资源 URL（含 og 元信息；过滤小图标，按 DOM 顺序最多 40 条）。把图文/视频链接转给用户、给媒体下载/分享流程提供 URL 素材时用。只读当前页面，不导航；需要元素交互用 browser_snapshot。",
    async fn execute(&self, _input: Value) -> ToolResult {
        match self.browser.extract_media().await {
            Ok(text) => ToolResult::ok(text),
            Err(error) => ToolResult::err(error),
        }
    }
);

impl BrowserMediaTool {
    fn input_schema_impl() -> Value {
        json!({ "type": "object", "properties": {} })
    }
}

browser_tool_impl!(
    BrowserNavigateTool,
    "browser_navigate",
    "CDP 浏览器：打开 URL 并读取页面（真实 Chromium 内核，UA 与交互已拟人化，可正常访问 Google 等风控站点）。普通页返回页面标题、最终 URL 与正文文本；搜索引擎结果页 URL（引擎自由选择，如 https://www.bing.com/search?q=检索词 或 google/baidu 的搜索 URL）返回解析后的结果列表（标题/链接/摘要，广告已过滤）。站内跳转（如首页→列表页→详情页）建议带 referrer=上一页 URL，导航会携带真实 Referer（不带则等价地址栏直达，部分站点风控会拦无来源的深链直达）。命中 Google 风控页（CAPTCHA）时本调用会挂起等待人工过码（已自动通知用户并展开悬浮窗），恢复后返回结果并标注；最多等 180 秒，超时返回风控页标注——超时后不要立即重试，向用户报告即可。同意页（consent）仅标注，可用 browser_snapshot 后点按勾选框继续。需要交互（点击/输入/滚动/截图）用 browser_snapshot / browser_click / browser_type / browser_scroll / browser_screenshot。",
    async fn execute(&self, input: Value) -> ToolResult {
        let url = match input.get("url").and_then(Value::as_str) {
            Some(url) => url,
            None => return ToolResult::err("缺少参数 url"),
        };
        let referrer = match input.get("referrer").and_then(Value::as_str) {
            Some(from) if !from.trim().is_empty() => match normalize_url(from) {
                Ok(from) => Some(from),
                Err(error) => return ToolResult::err(format!("referrer 非法: {error}")),
            },
            _ => None,
        };
        match normalize_url(url) {
            Ok(url) => match self.browser.navigate_and_extract(&url, referrer.as_deref()).await {
                Ok(page) => ToolResult::ok(format_navigated_page(&page)),
                Err(error) => ToolResult::err(error),
            },
            Err(error) => ToolResult::err(error),
        }
    }
);

impl BrowserNavigateTool {
    fn input_schema_impl() -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "目标页面 URL（无 scheme 自动补 https://）" },
                "referrer": { "type": "string", "description": "可选来源页 URL（如上一页/站点首页）——携带真实 Referer，站内跳转建议传；缺省 = 无来源直达" }
            },
            "required": ["url"]
        })
    }
}

/// browser_navigate 输出：搜索引擎结果页走结构化结果列表（EXTRACT_JS 按主机名
/// 分派解析并过滤广告；SERP 的全文 dump 是噪音，不输出）；普通页走导航报告
/// （标题/最终 URL/风控标注）+ 截断正文。正文为空不报错（可能是交互页/纯媒体页），
/// 引导走 browser_snapshot。风控/同意页标注在两种形态下都保留；park 过码等待
/// 的结果说明置顶。
fn format_navigated_page(page: &PagePayload) -> String {
    let mut prefix = String::new();
    if !page.park_note.is_empty() {
        prefix.push_str(&page.park_note);
        prefix.push_str("\n\n");
    }
    if !page.results.is_empty() && looks_like_serp(&page.url) {
        let mut out = format_serp_page(page);
        if !page.blocked.is_empty() {
            push_blocked_note(&mut out, &page.blocked);
        }
        return format!("{prefix}{out}");
    }
    let mut out = format!("已打开：{}\n标题：{}", page.url, single_line(&page.title));
    if !page.blocked.is_empty() {
        push_blocked_note(&mut out, &page.blocked);
    }
    if page.text.trim().is_empty() {
        out.push_str(
            "\n\n（页面没有可读正文——可能是交互页/纯媒体页，可用 browser_snapshot 查看可交互元素）",
        );
    } else {
        out.push('\n');
        out.push('\n');
        out.push_str(&truncate_chars(&page.text, PAGE_TEXT_MAX_CHARS));
    }
    format!("{prefix}{out}")
}

/// URL 是否为搜索引擎结果页（google.com /search、bing.com /search、
/// baidu.com /s）。EXTRACT_JS 的 SERP 解析按主机名分派，但普通内容页（如
/// docs.google.com 的文档）也可能带 h3 链接被误收集——这里按域名 label 边界
/// 与路径精确匹配二次确认，非 SERP 宁回正文。判定域必须与 EXTRACT_JS 一致。
fn looks_like_serp(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let Some(after_scheme) = lower.split("://").nth(1) else {
        return false;
    };
    let authority_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    let (authority, path) = after_scheme.split_at(authority_end);
    // 去 userinfo、去端口，域名按 label 边界匹配（www. / m. 子域放行，
    // notgoogle.com / notbaidu.com 这类假同尾域拒绝）。
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .split(':')
        .next()
        .unwrap_or(authority);
    let host_is = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    // 路径精确匹配：/search、/s 后只允许查询串（/searching 是普通路径）。
    let path_is = |segment: &str| path == segment || path.starts_with(&format!("{segment}?"));
    if host_is("baidu.com") {
        return path_is("/s");
    }
    if host_is("google.com") || host_is("bing.com") {
        return path_is("/search");
    }
    false
}

/// 搜索引擎结果页输出：解析出的结果列表（EXTRACT_JS 上限 10 条，标题/摘要
/// 已在 JS 侧截断）。
fn format_serp_page(page: &PagePayload) -> String {
    let mut out = format!(
        "已打开：{}\n标题：{}\n\n检索结果 {} 条：",
        page.url,
        single_line(&page.title),
        page.results.len()
    );
    for (index, entry) in page.results.iter().enumerate() {
        out.push_str(&format!(
            "\n\n[{}] {}",
            index + 1,
            single_line(&entry.title)
        ));
        if !entry.url.is_empty() {
            out.push_str(&format!("\nURL: {}", single_line(&entry.url)));
        }
        if !entry.snippet.is_empty() {
            out.push_str(&format!("\n{}", single_line(&entry.snippet)));
        }
    }
    out.push_str("\n\n（需要某条结果的正文时用 browser_navigate 打开对应 URL）");
    out
}

pub struct BrowserSnapshotTool {
    browser: Arc<CdpBrowser>,
}

browser_tool_impl!(
    BrowserSnapshotTool,
    "browser_snapshot",
    "CDP 浏览器：列出当前页面可交互元素（链接/按钮/输入框/标题），每项带 [ref] 编号。browser_click / browser_type 按 ref 定位。页面跳转后 ref 失效，需重新快照。页面存在大量非交互内容（图文卡片/数据行等）时快照尾部附盲区提示：判定页面是否为空、有没有目标内容，以 browser_extract 的提取结果为准。",
    async fn execute(&self, _input: Value) -> ToolResult {
        match self.browser.snapshot().await {
            Ok(text) => ToolResult::ok(text),
            Err(error) => ToolResult::err(error),
        }
    }
);

impl BrowserSnapshotTool {
    fn input_schema_impl() -> Value {
        json!({ "type": "object", "properties": {} })
    }
}

pub struct BrowserExtractTool {
    browser: Arc<CdpBrowser>,
}

browser_tool_impl!(
    BrowserExtractTool,
    "browser_extract",
    "CDP 浏览器：按 CSS 选择器枚举元素并读取 id、指定属性与文本（只读结构化提取）。部分站点把可点击内容渲染成无 href 的元素（如京东搜索结果页的商品卡：纯 div、商品链接只在 pure_item_link 属性里、点击走事件委托），browser_snapshot 的可交互元素清单看不到它们——用本工具按 DOM 属性取结构化数据。返回 JSON：{count, total, skip, items:[{id, text, <指定属性>…}]}，text 为去空白 innerText（截 300 字符）。skip=跳过的匹配数（无限滚动页保留早先卡片，重复提取时按已采集条数推进 skip 才能拿到新卡）；items 上限 200，超预算自动丢尾部条目并置 truncated=true。只读、不执行任何模型提供的 JS；需要交互用 browser_click/type/scroll，需要正文用 browser_navigate。",
    async fn execute(&self, input: Value) -> ToolResult {
        let Some(selector) = input.get("selector").and_then(Value::as_str) else {
            return ToolResult::err("缺少参数 selector（CSS 选择器）");
        };
        let attributes: Vec<String> = input
            .get("attributes")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let skip = input.get("skip").and_then(Value::as_u64).unwrap_or(0);
        let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(100);
        let (Ok(skip), Ok(limit)) = (usize::try_from(skip), usize::try_from(limit)) else {
            return ToolResult::err("skip/limit 超出范围");
        };
        match self
            .browser
            .clone()
            .extract(selector, &attributes, skip, limit)
            .await
        {
            Ok(text) => ToolResult::ok(text),
            Err(error) => ToolResult::err(error),
        }
    }
);

impl BrowserExtractTool {
    fn input_schema_impl() -> Value {
        json!({
            "type": "object",
            "properties": {
                "selector": { "type": "string", "description": "CSS 选择器（只用于 querySelectorAll，不具执行能力）" },
                "attributes": { "type": "array", "items": { "type": "string" }, "description": "要读取的属性名列表（可选，如 [\"pure_item_link\",\"tourl\"]）" },
                "skip": { "type": "integer", "description": "跳过前 N 个匹配（可选；无限滚动页重复提取时推进此值拿新卡片）" },
                "limit": { "type": "integer", "description": "最多返回条数（可选，默认 100，上限 200）" }
            },
            "required": ["selector"]
        })
    }
}

pub struct BrowserClickTool {
    browser: Arc<CdpBrowser>,
}

browser_tool_impl!(
    BrowserClickTool,
    "browser_click",
    "CDP 浏览器：点按 snapshot 给出的 [ref] 元素（桌面/手机面板展开时是真实输入事件；手机面板收起时自动用页内点击，结果里标注）。返回点按位置与当前页面标题/URL。",
    async fn execute(&self, input: Value) -> ToolResult {
        let Some(r) = input.get("ref").and_then(Value::as_u64) else {
            return ToolResult::err("缺少参数 ref（browser_snapshot 列表里的编号）");
        };
        match u32::try_from(r) {
            Ok(r) => match self.browser.click(r).await {
                Ok(report) => ToolResult::ok(report),
                Err(error) => ToolResult::err(error),
            },
            Err(_) => ToolResult::err("ref 超出范围"),
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
    browser: Arc<CdpBrowser>,
}

browser_tool_impl!(
    BrowserTypeTool,
    "browser_type",
    "CDP 浏览器：聚焦 [ref] 输入框，先清空原内容再输入文本（IME 通路，事件完整；适合搜索框/表单）。目标若是下拉框（select，如登录页国家区号），则改为选中含指定文本的选项并派发 change（传「中国大陆」或「+86」即可）。",
    async fn execute(&self, input: Value) -> ToolResult {
        let (Some(r), Some(text)) = (
            input.get("ref").and_then(Value::as_u64),
            input.get("text").and_then(Value::as_str),
        ) else {
            return ToolResult::err("缺少参数 ref / text");
        };
        match u32::try_from(r) {
            Ok(r) => match self.browser.type_text(r, text).await {
                Ok(report) => ToolResult::ok(report),
                Err(error) => ToolResult::err(error),
            },
            Err(_) => ToolResult::err("ref 超出范围"),
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

pub struct BrowserScrollTool {
    browser: Arc<CdpBrowser>,
}

browser_tool_impl!(
    BrowserScrollTool,
    "browser_scroll",
    "CDP 浏览器：在当前页面滚动（桌面滚轮事件；手机 attached 走触摸手势、面板收起自动用页内程序化滚动，懒加载均正常触发；结果里会标注实际采用的通路）。返回当前页面标题/URL。长页面读正文、翻找内容、往下加载更多时用。",
    async fn execute(&self, input: Value) -> ToolResult {
        let direction = input
            .get("direction")
            .and_then(Value::as_str)
            .unwrap_or("down");
        // amount 只在缺省时取默认值；显式传入的非法值（负数/小数/字符串）报错，
        // 静默回退默认会让“滑 1 格”变成“滑 5 格”这类偏差无人察觉。
        let amount = match input.get("amount") {
            None | Some(Value::Null) => 5_i64,
            Some(value) => match value.as_i64() {
                Some(n) if (1..=30).contains(&n) => n,
                _ => return ToolResult::err("amount 必须是 1-30 的整数"),
            },
        };
        let notches = match direction {
            "down" => amount,
            "up" => -amount,
            other => {
                return ToolResult::err(format!(
                    "未知 direction {other:?}（支持 up / down）"
                ))
            }
        };
        match self.browser.scroll(notches as i32).await {
            Ok(report) => ToolResult::ok(report),
            Err(error) => ToolResult::err(error),
        }
    }
);

impl BrowserScrollTool {
    fn input_schema_impl() -> Value {
        json!({
            "type": "object",
            "properties": {
                "direction": { "type": "string", "enum": ["up", "down"], "description": "滑动方向，缺省 down" },
                "amount": { "type": "integer", "minimum": 1, "maximum": 30, "description": "格数 1-30，缺省 5（一格约 120px）" }
            }
        })
    }
}

pub struct BrowserScreenshotTool {
    browser: Arc<CdpBrowser>,
}

browser_tool_impl!(
    BrowserScreenshotTool,
    "browser_screenshot",
    "CDP 浏览器：对当前页面截图（PNG，存工作区 browser/ 下）。返回文件路径，可用 read 读取或 openMedia 分享。",
    async fn execute(&self, _input: Value) -> ToolResult {
        match self.browser.screenshot().await {
            Ok(report) => ToolResult::ok(report),
            Err(error) => ToolResult::err(error),
        }
    }
);

impl BrowserScreenshotTool {
    fn input_schema_impl() -> Value {
        json!({ "type": "object", "properties": {} })
    }
}

pub struct BrowserCloseTool {
    browser: Arc<CdpBrowser>,
}

browser_tool_impl!(
    BrowserCloseTool,
    "browser_close",
    "CDP 浏览器：销毁浏览器会话并释放内存（下次使用任一 browser_* 工具自动重建）。",
    async fn execute(&self, _input: Value) -> ToolResult {
        match self.browser.close().await {
            Ok(report) => ToolResult::ok(report),
            Err(error) => ToolResult::err(error),
        }
    }
);

impl BrowserCloseTool {
    fn input_schema_impl() -> Value {
        json!({ "type": "object", "properties": {} })
    }
}

// ---------------------------------------------------------------------------
// 测试（纯函数：双端宿主都可跑）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_profile_dir_distinguishes_legacy_pid_and_foreign_names() {
        assert_eq!(classify_profile_dir("rclaw-browser-profile"), Some(None));
        assert_eq!(
            classify_profile_dir("rclaw-browser-profile-120"),
            Some(Some(120))
        );
        // 非本产品命名：不碰。
        assert_eq!(classify_profile_dir("rclaw-browser-profiles"), None);
        assert_eq!(classify_profile_dir("rclaw-browser-profile-backup"), None);
        assert_eq!(classify_profile_dir("rclaw-browser-profile-[12]000"), None);
        assert_eq!(classify_profile_dir("other"), None);
    }

    #[test]
    fn cmdline_holds_dir_requires_argument_boundary() {
        let dir = "/tmp/rclaw-browser-profile-120";
        // 完整参数出现：命中。
        assert!(cmdline_holds_dir(
            &format!("--headless --user-data-dir={dir} about:blank"),
            dir
        ));
        // 前缀更长的兄弟目录：不得误中。
        assert!(!cmdline_holds_dir(
            "--headless --user-data-dir=/tmp/rclaw-browser-profile-1200 about:blank",
            dir
        ));
        // 引号包裹的参数：命中。
        assert!(cmdline_holds_dir(
            &format!("--user-data-dir=\"{dir}\" --headless"),
            dir
        ));
        // 毫不相关的命令行：不命中。
        assert!(!cmdline_holds_dir("msedge.exe --homepage about:blank", dir));
    }

    #[test]
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
    fn strip_passes_through_missing_headers_object() {
        assert_eq!(strip_x_requested_with(&Value::Null), json!({}));
    }

    #[test]
    fn header_entries_map_to_name_value_pairs() {
        let entries = header_entries(&json!({ "A": "1", "B": "2" }));
        let list = entries.as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert!(list
            .iter()
            .all(|e| e.get("name").is_some() && e.get("value").is_some()));
    }

    #[test]
    fn pick_page_target_skips_non_page_entries() {
        let body = r#"[
            {"description":"","devtoolsFrontendUrl":"x","id":"SW1","type":"service_worker","url":"https://sw"},
            {"description":"","id":"PAGE1","type":"page","title":"Google","url":"https://www.google.com/"},
            {"description":"","id":"PAGE2","type":"page","title":"Other","url":"https://other/"}
        ]"#;
        assert_eq!(pick_page_target(body).unwrap(), "PAGE1");
    }

    #[test]
    fn pick_page_target_errors_when_no_page() {
        let body = r#"[{"id":"SW1","type":"service_worker","url":"https://sw"}]"#;
        assert!(pick_page_target(body).is_err());
        assert!(pick_page_target("not json").is_err());
    }

    #[test]
    fn pick_page_target_skips_tauri_ui_and_prefers_about_blank() {
        // 调试开关暴露进程内所有 WebView：Tauri 主界面（排在最前）必须跳过，
        // 会话 WebView 初始态是 about:blank，优先选中。
        let body = r#"[
            {"id":"UI","type":"page","title":"RClaw","url":"http://tauri.localhost/"},
            {"id":"BLANK","type":"page","title":"","url":"about:blank"}
        ]"#;
        assert_eq!(pick_page_target(body).unwrap(), "BLANK");
    }

    #[test]
    fn pick_page_target_falls_back_to_non_app_ui_page() {
        // 会话已在导航后：没有 about:blank，取剩下的非应用界面 page。
        let body = r#"[
            {"id":"UI","type":"page","title":"RClaw","url":"https://tauri.localhost/"},
            {"id":"PAGE","type":"page","title":"Google","url":"https://www.google.com/search?q=1"}
        ]"#;
        assert_eq!(pick_page_target(body).unwrap(), "PAGE");

        // 只剩应用界面 = 找不到会话 target。
        let only_ui = r#"[{"id":"UI","type":"page","title":"RClaw","url":"tauri://localhost"}]"#;
        assert!(pick_page_target(only_ui).is_err());
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

    #[test]
    fn humanize_ua_strips_headless_and_webview_markers() {
        // 桌面无头实例：HeadlessChrome → Chrome。
        assert_eq!(
            humanize_ua(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) HeadlessChrome/120.0.0.0 Safari/537.36"
            ),
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
        );
        // 安卓 WebView：`; wv` 标记摘除，其余不动。
        assert_eq!(
            humanize_ua(
                "Mozilla/5.0 (Linux; Android 13; Pixel 7; wv) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36"
            ),
            "Mozilla/5.0 (Linux; Android 13; Pixel 7) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36"
        );
        // 普通浏览器 UA 原样保留。
        let normal = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                      (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
        assert_eq!(humanize_ua(normal), normal);
    }

    #[test]
    fn user_agent_metadata_parses_chrome_edge_and_android() {
        let desktop = user_agent_metadata(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/120.0.6099.130 Safari/537.36",
        )
        .unwrap();
        assert_eq!(desktop["brands"][1]["version"], "120");
        assert_eq!(desktop["brands"][2]["brand"], "Google Chrome");
        assert_eq!(desktop["fullVersion"], "120.0.6099.130");
        assert_eq!(desktop["fullVersionList"][2]["version"], "120.0.6099.130");
        // 平台/OS 版本/架构全部考证自 UA 本身（跨编译平台确定性）。
        assert_eq!(desktop["platform"], "Windows");
        assert_eq!(desktop["platformVersion"], "10.0.0");
        assert_eq!(desktop["architecture"], "x86");
        assert_eq!(desktop["mobile"], false);
        // 合成值必须能过 CDP 必填键校验（否则覆写会被整个拒绝）。
        assert!(valid_ua_metadata(&desktop));

        // Edge：品牌与完整版本跟 Edg/ 走。
        let edge = user_agent_metadata(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.2420.65",
        )
        .unwrap();
        assert_eq!(edge["brands"][2]["brand"], "Microsoft Edge");
        assert_eq!(edge["fullVersion"], "120.0.2420.65");

        // 安卓：手机 UA 带 Mobile 令牌 → mobile=true；OS 版本与机型取自 UA
        // 括号段；架构 UA 未写则留空不编造。
        let android = user_agent_metadata(
            "Mozilla/5.0 (Linux; Android 13; Pixel 7) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36",
        )
        .unwrap();
        assert_eq!(android["mobile"], true);
        assert_eq!(android["platform"], "Android");
        assert_eq!(android["platformVersion"], "13.0.0");
        assert_eq!(android["model"], "Pixel 7");
        assert_eq!(android["architecture"], "");

        // 真实 WebView 格式：多段 OS 版本不重复补零；Build 指纹不进 model；
        // 无 Mobile 令牌（平板形态）→ mobile=false。
        let webview = user_agent_metadata(
            "Mozilla/5.0 (Linux; Android 8.1.0; SM-G970F Build/QQ3A.190805.037; wv) \
             AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/120.0.0.0 \
             Mobile Safari/537.36",
        )
        .unwrap();
        assert_eq!(webview["platformVersion"], "8.1.0");
        assert_eq!(webview["model"], "SM-G970F");
        assert_eq!(webview["mobile"], true);
        let tablet = user_agent_metadata(
            "Mozilla/5.0 (Linux; Android 13; SM-X710) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        )
        .unwrap();
        assert_eq!(tablet["mobile"], false);

        // 无机型的预览版 WebView UA：只有 Build 指纹，model 应为空。
        let preview = user_agent_metadata(
            "Mozilla/5.0 (Linux; Android 16; Build/BP22.250325.007; wv)              AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/120.0.0.0              Mobile Safari/537.36",
        )
        .unwrap();
        assert_eq!(preview["model"], "");

        // 非 Chromium UA 解析不出版本：不给元数据（覆写降级为只改 UA 字符串）。
        assert!(user_agent_metadata("Mozilla/5.0 (X11; Linux) Gecko/1.0").is_none());
    }

    #[test]
    fn valid_ua_metadata_requires_cdp_contract_keys() {
        assert!(!valid_ua_metadata(&serde_json::json!({})));
        // 缺任一必填键（如 Permissions-Policy 削减后的高熵应答）都不可用。
        let partial = serde_json::json!({
            "brands": [{ "brand": "Chromium", "version": "120" }],
            "platform": "Android",
            "platformVersion": "13.0.0",
            "architecture": "arm",
            "model": "",
            "mobile": true,
            // fullVersionList 缺失
        });
        assert!(!valid_ua_metadata(&partial));
        // brands 空数组同样不可用。
        let empty_brands = serde_json::json!({
            "brands": [],
            "platform": "Windows",
            "platformVersion": "10.0.0",
            "architecture": "x86",
            "model": "",
            "mobile": false,
            "fullVersionList": [],
        });
        assert!(!valid_ua_metadata(&empty_brands));
    }

    #[test]
    fn looks_like_serp_matches_search_paths_only() {
        assert!(looks_like_serp("https://www.bing.com/search?q=x"));
        assert!(looks_like_serp("https://www.google.com/search?q=x"));
        assert!(looks_like_serp("https://m.baidu.com/s?wd=x"));
        assert!(looks_like_serp("https://bing.com/search"));
        // 普通内容页：即使被 EXTRACT_JS 收集到 h3 链接也不按 SERP 输出。
        assert!(!looks_like_serp(
            "https://docs.google.com/document/d/1/edit"
        ));
        assert!(!looks_like_serp("https://www.bing.com/images/feed"));
        assert!(!looks_like_serp("https://example.com/search"));
        assert!(!looks_like_serp("not a url"));
        // 假同尾域与路径前缀误伤：label/路径必须有边界。
        assert!(!looks_like_serp("https://notgoogle.com/searching/article"));
        assert!(!looks_like_serp("https://notbaidu.com/s"));
        assert!(!looks_like_serp("https://www.bing.com/searching/x"));
    }

    #[test]
    fn ua_is_mobile_selects_event_vocabulary() {
        // Android（手机/平板 UA 一律触屏词汇）；桌面无头/普通桌面 UA 鼠标词汇。
        assert!(ua_is_mobile(
            "Mozilla/5.0 (Linux; Android 10; V1911A) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/83.0.4103.106 Mobile Safari/537.36"
        ));
        assert!(ua_is_mobile(
            "Mozilla/5.0 (Linux; Android 13; SM-X710) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
        ));
        assert!(!ua_is_mobile(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
        ));
        assert!(!ua_is_mobile(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
             (KHTML, like Gecko) HeadlessChrome/120.0.0.0 Safari/537.36"
        ));
    }

    #[test]
    fn navigated_page_prefixes_park_note() {
        // park 过码等待的结果说明置顶（恢复/超时两种），后续形态不被破坏。
        let mut page = serp_payload("https://www.bing.com/search?q=x");
        page.park_note = "已通过人工过码（等待 12 秒），页面已恢复。".into();
        let out = format_navigated_page(&page);
        assert!(out.starts_with("已通过人工过码"), "{out}");
        assert!(out.contains("检索结果 1 条"), "{out}");

        let mut timeout = serp_payload("https://www.google.com/sorry/index");
        timeout.results.clear();
        timeout.blocked = "Google CAPTCHA 风控页（/sorry/）".into();
        timeout.park_note = "已通知用户人工过码，但等待 180 秒仍未通过".into();
        let out = format_navigated_page(&timeout);
        assert!(out.starts_with("已通知用户人工过码"), "{out}");
        assert!(out.contains("检测到Google CAPTCHA"), "{out}");

        // 未 park（空标注）不引入多余前缀。
        let page = serp_payload("https://www.bing.com/search?q=x");
        assert!(format_navigated_page(&page).starts_with("已打开："));
    }

    fn serp_payload(url: &str) -> PagePayload {
        PagePayload {
            title: "search - 搜索".into(),
            url: url.into(),
            text: "SERP 全文噪音".into(),
            results: vec![SearchEntry {
                title: "结果一".into(),
                url: "https://example.com/a".into(),
                snippet: "摘要".into(),
            }],
            blocked: String::new(),
            park_note: String::new(),
        }
    }

    #[test]
    fn navigated_page_formats_serp_with_blocked_note() {
        // SERP + 风控标注同时存在：标注不能丢。
        let mut page = serp_payload("https://www.bing.com/search?q=x");
        page.blocked = "同意页（consent）".into();
        let out = format_navigated_page(&page);
        assert!(out.contains("检索结果 1 条"), "{out}");
        assert!(out.contains("结果一"), "{out}");
        assert!(out.contains("检测到同意页"), "{out}");
    }

    #[test]
    fn navigated_page_keeps_article_for_non_serp_pages() {
        // docs.google.com 带标题链接被误收集时：按路径二次确认，仍走正文形态。
        let mut page = serp_payload("https://docs.google.com/document/d/1/edit");
        page.text = "文档正文".into();
        let out = format_navigated_page(&page);
        assert!(out.contains("文档正文"), "{out}");
        assert!(!out.contains("检索结果"), "{out}");

        // 空正文 + 无结果：给交互引导而非报错。
        let empty = PagePayload {
            title: "t".into(),
            url: "https://example.com".into(),
            text: String::new(),
            results: vec![],
            blocked: String::new(),
            park_note: String::new(),
        };
        let out = format_navigated_page(&empty);
        assert!(out.contains("browser_snapshot"), "{out}");
    }

    /// connect 必败传输：单测只验参数校验路径（不真拉浏览器）。
    struct UnreachableTransport;

    #[async_trait::async_trait]
    impl DevtoolsTransport for UnreachableTransport {
        async fn connect(&self) -> Result<(RawStream, String), String> {
            Err("unreachable".into())
        }
        async fn shutdown(&self) -> Result<(), String> {
            Ok(())
        }
        async fn capture_screenshot(
            &self,
            _session: &Session,
            _dest: &std::path::Path,
        ) -> Result<String, String> {
            Err("unreachable".into())
        }
        fn screenshot_dir(&self) -> std::path::PathBuf {
            std::env::temp_dir()
        }
    }

    #[tokio::test]
    async fn scroll_rejects_invalid_amount_before_touching_browser() {
        let tool = BrowserScrollTool {
            browser: Arc::new(CdpBrowser::new(Arc::new(UnreachableTransport))),
        };
        for bad in [-1i64, 0, 31] {
            let result = tool.execute(serde_json::json!({ "amount": bad })).await;
            assert!(result.is_error, "{bad}");
            assert!(result.content.contains("1-30"), "{bad}: {}", result.content);
        }
        let result = tool.execute(serde_json::json!({ "amount": "很多" })).await;
        assert!(result.is_error);
        // 缺省值放行（后续 connect 失败与参数校验无关，错误来自传输层）。
        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("unreachable"));
    }

    #[test]
    fn snapshot_hint_appended_only_above_threshold() {
        let base = "共 3 项\n[1] link: 示例".to_string();
        // 字段缺失（旧内核返回体不含 non_interactive）与阈值下：原文返回。
        assert_eq!(append_snapshot_hint(base.clone(), None), base);
        assert_eq!(append_snapshot_hint(base.clone(), Some(0)), base);
        assert_eq!(append_snapshot_hint(base.clone(), Some(4)), base);
        // 阈值上：保留原文、附提示，提示点名 browser_extract 与数量。
        let above = append_snapshot_hint(base.clone(), Some(12));
        assert!(above.starts_with(&base));
        assert!(above.contains("browser_extract"), "{above}");
        assert!(above.contains("12"), "{above}");
    }
}

/// 无头测试串行锁：#[ignore] 测试并行跑时共用全局子进程登记与
/// kill_headless_processes 全局清理，互相杀浏览器会出假失败——必须串行。
#[cfg(test)]
static HEADLESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 本机真浏览器的端到端冒烟（需装 Chrome/Edge，默认跳过）：
/// cargo test -p rclaw-tools cdp_browser -- --ignored
#[test]
#[ignore = "需要本机 Chrome/Edge（会拉起真实无头进程并访问外网）"]
fn headless_browser_end_to_end_smoke() {
    // 无论成败（含 panic 展开丢弃 future）都要杀掉无头浏览器：其一不留孤儿
    // 锁 profile，其二 Edge 的子进程继承了测试进程的 stdout 管道，不杀会把
    // 外层 shell 任务挂到天荒地老。
    struct KillBrowserOnDrop;
    impl Drop for KillBrowserOnDrop {
        fn drop(&mut self) {
            kill_headless_processes();
        }
    }
    let _serial = HEADLESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    // launch_blocking 的 stderr 读线程会在内核存活期间阻塞在 lines() 上：
    // panic / 失败路径下 runtime drop 会等阻塞线程，必须限时关停（泄漏一条
    // 读线程无害——进程退出即回收）。
    let outcome: Result<(), String> = rt.block_on(async {
        let _guard = KillBrowserOnDrop;
        let transport = HeadlessChromiumTransport::new(std::env::temp_dir());
        let browser = CdpBrowser::new(Arc::new(transport) as Arc<dyn DevtoolsTransport>);
        eprintln!("[cdp] navigate example.com…");
        let page = browser
            .navigate_and_extract("https://example.com", None)
            .await?;
        eprintln!(
            "[cdp] navigate 完成: {}（{} 字符正文）",
            page.url,
            page.text.chars().count()
        );
        assert!(page.url.contains("example"), "{:?}", page.url);
        assert!(!page.text.is_empty(), "正文为空");
        eprintln!("[cdp] snapshot…");
        let snapshot = browser.snapshot().await?;
        eprintln!("[cdp] snapshot 完成（{} 字符）", snapshot.chars().count());
        assert!(!snapshot.is_empty());
        browser.close().await?;
        Ok(())
    });
    rt.shutdown_timeout(Duration::from_millis(500));
    if let Err(error) = outcome {
        panic!("{error}");
    }
}

#[cfg(test)]
impl CdpBrowser {
    /// 测试助手：导航到 URL（data: 即可）后取快照。锁分段持有——navigate_wait
    /// 与 snapshot 各自取 op 锁，静态测试页两步之间无变化。
    async fn snapshot_after_load(&self, url: &str) -> Result<String, String> {
        {
            let _op = self.op.lock().await;
            self.navigate_wait(url, None, Duration::from_secs(20))
                .await?;
        }
        self.snapshot().await
    }
}

/// data: URL 编码（页面内嵌 HTML，免起本地服务；中文按 UTF-8 百分号编码）。
#[cfg(test)]
fn data_url(html: &str) -> String {
    let mut out = String::from("data:text/html;charset=utf-8,");
    for b in html.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 盲区提示双向无头验证（提交 24dcbe0 的判定条件回归，codex r1 P2/P3）：
/// A. 仿京东卡片页（12 张非叶卡片：div+img+30 字 span 标题）→ 必须附提示
///    （旧实现的 childElementCount>0 跳过非叶节点会漏计成 0）；
/// B. 普通表单页 → 不得附提示；
/// C. 纯长标题 h2 页 → 不得附提示（h1-h3 在快照收录集合内，不是盲区——
///    旧实现的排除表缺 h1-h3 会误报）。
#[test]
#[ignore = "需要本机 Chrome/Edge（会拉起真实无头进程）"]
fn headless_snapshot_hint_jd_cards_vs_form_page() {
    struct KillBrowserOnDrop;
    impl Drop for KillBrowserOnDrop {
        fn drop(&mut self) {
            kill_headless_processes();
        }
    }
    let _serial = HEADLESS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let outcome: Result<(), String> = rt.block_on(async {
        let _guard = KillBrowserOnDrop;
        let transport = HeadlessChromiumTransport::new(std::env::temp_dir());
        let browser = CdpBrowser::new(Arc::new(transport) as Arc<dyn DevtoolsTransport>);

        let title = "小米 MIX Flip 2 贝壳白 12+256 骁龙8至尊版 1.5K 内屏 5000万像素";
        // A1 直排文本形态（codex r1 P2 回归点）：标题直接躺在带 img 的非叶 div
        // 里——childElementCount>0 跳过非叶节点的旧实现会漏计成 0。
        let cards: String = (0..12)
            .map(|i| {
                format!(
                    r#"<div id="link_1001234567{i:02}" pure_item_link="https://item.m.jd.com/product/1001234567{i:02}.html"><img alt="">{title}</div>"#
                )
            })
            .collect();
        // 公共容器包裹（codex r3）：textContent+祖先去重的错误实现会把容器计 1
        // 吞掉全部卡片、提示消失——本断言拦住该回归。
        let snapshot = browser
            .snapshot_after_load(&data_url(&format!(
                "<html><body><div id=\"wrap\">{cards}</div></body></html>"
            )))
            .await?;
        assert!(
            snapshot.contains("非交互内容") && snapshot.contains("browser_extract"),
            "直排文本卡片页必须附盲区提示: {snapshot}"
        );
        // A2 span 形态（真机京东卡片的常见结构）：标题在 span 里，span 自身计 1。
        let cards: String = (0..12)
            .map(|i| {
                format!(
                    r#"<div id="link_1001234568{i:02}" pure_item_link="https://item.m.jd.com/product/1001234568{i:02}.html"><img alt=""><span>{title}</span></div>"#
                )
            })
            .collect();
        let snapshot = browser
            .snapshot_after_load(&data_url(&format!(
                "<html><body><div id=\"wrap\">{cards}</div></body></html>"
            )))
            .await?;
        assert!(
            snapshot.contains("非交互内容"),
            "span 卡片页必须附盲区提示: {snapshot}"
        );

        let form = "<html><body><form><input placeholder='搜索'><button>提交</button>\
                    <select><option>选项</option></select><label>备注</label>\
                    <textarea></textarea></form></body></html>";
        let snapshot = browser.snapshot_after_load(&data_url(form)).await?;
        assert!(
            !snapshot.contains("非交互内容"),
            "表单页不得附盲区提示: {snapshot}"
        );

        let headings: String = (0..6)
            .map(|i| format!("<h2>第 {i} 节 标题文本足够长以满足三十字门槛的判定条件示例内容</h2>"))
            .collect();
        let snapshot = browser
            .snapshot_after_load(&data_url(&format!(
                "<html><body>{headings}</body></html>"
            )))
            .await?;
        assert!(
            !snapshot.contains("非交互内容"),
            "h2 标题在快照收录集合内，不得计入盲区: {snapshot}"
        );

        // D 负例（codex r3）：快照已可见的 button 长文本，经父容器 textContent
        // 重复计入盲区是 r2-P3 回归——textContent 实现会在此误报，直接文本实现不计。
        let wrapped_buttons: String = (0..6)
            .map(|i| {
                format!(
                    "<div><button>第 {i} 个确认按钮的说明文本足够长以满足三十字门槛判定条件示例</button></div>"
                )
            })
            .collect();
        let snapshot = browser
            .snapshot_after_load(&data_url(&format!(
                "<html><body>{wrapped_buttons}</body></html>"
            )))
            .await?;
        assert!(
            !snapshot.contains("非交互内容"),
            "已可见 button 文本不得经父容器计入盲区: {snapshot}"
        );

        browser.close().await?;
        Ok(())
    });
    rt.shutdown_timeout(Duration::from_millis(500));
    if let Err(error) = outcome {
        panic!("{error}");
    }
}
