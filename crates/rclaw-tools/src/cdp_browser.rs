//! CDP 浏览器共享内核：协议层 + 传输抽象 + 工具面（桌面 / 安卓共用）。
//!
//! 传输两实现（[`DevtoolsTransport`]）：
//! - 桌面 [`HeadlessChromiumTransport`]：拉起本机 Chrome/Edge 无头实例
//!   （`--headless --remote-debugging-port=0`），经 TCP DevTools 端口驱动——
//!   与 Playwright 的 cdp 后端同源机制，但不需要 Node 运行时；
//! - 安卓（rclaw-android::cdp_browser::AndroidTransport）：长驻调试 WebView，
//!   经 abstract unix socket 直连。
//!
//! Google 拒绝的根因治理：`Fetch` 拦截摘掉 WebView 网络栈强加的
//! `X-Requested-With`（应用包名指纹）；UA 保持内核真实身份；Cookie 由真实
//! 网络栈跨调用保持；navigate 结果带 `/sorry/`（CAPTCHA）与 consent 同意页
//! 检测，如实上报并给出建议。
//!
//! 截图是例外：安卓离屏 WebView 没有合成器帧（`Page.captureScreenshot` 出
//! 空白），走 `View.draw` 软件位图；桌面无头实例可直接 CDP 截图。

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

use crate::web_browser::{PageLoader, PagePayload, WebBrowserTool};
use crate::{Tool, ToolResult};

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
const HTTP_JSON_LIST: &[u8] = b"GET /json/list HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

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
    stream.write_all(HTTP_JSON_LIST).await.map_err(|e| format!("探测请求失败: {e}"))?;
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
                    Some(_) => {} // 响应体未读满，继续收
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
                                    &mut sink, &mut next_id, &mut fetch_retry,
                                    &request_id, attempts, params,
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
                        handle_fetch_paused(
                            &mut sink,
                            &mut next_id,
                            &mut fetch_retry,
                            &value,
                        )
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
    /// 离屏截图到 dest（PNG）。安卓走 View.draw 软件位图（离屏无合成器帧），
    /// 桌面走 Page.captureScreenshot。返回人读结果文案。
    async fn capture_screenshot(
        &self,
        session: &Session,
        dest: &std::path::Path,
    ) -> Result<String, String>;
}

/// 桌面端传输：拉起本机 Chrome/Edge 无头实例，经 TCP DevTools 端口驱动。
/// 浏览器可执行文件按 zcode cdp 后端同款的候选路径表解析，可用
/// `RCLAW_BROWSER_EXE` 覆盖。
pub struct HeadlessChromiumTransport {
    inner: AsyncMutex<Option<RunningChromium>>,
    screenshot_dir: std::path::PathBuf,
    profile_dir: std::path::PathBuf,
}

struct RunningChromium {
    /// `http://127.0.0.1:<port>`（/json/list 与 WS 握手同端口）。
    endpoint: String,
}

/// 进程级登记的子进程：app 退出钩子（[`kill_headless_processes`]）唯一所有者，
/// 实例侧只留 endpoint（重连复用）。
static HEADLESS_CHILD: std::sync::Mutex<Option<std::process::Child>> = std::sync::Mutex::new(None);

/// 杀掉登记的无头 Chromium（app 退出钩子 / 会话关闭共用）。
pub fn kill_headless_processes() {
    if let Ok(mut guard) = HEADLESS_CHILD.lock() {
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 清扫历史崩溃残留的无头浏览器与 profile 目录：profile 按进程 id 隔离后，
/// 崩溃实例的孤儿锁的是「死 pid」的目录，正常启动不会再撞锁，孤儿会无限
/// 积累。目录枚举与属主判活全在 Rust 里做（扫描目录与 [`std::env::temp_dir`]
/// 同源，不存在 $TEMP/$TMPDIR 分歧；怪异命名的目录一律不动）；仅「按命令
/// 行找持有进程」借平台机制，匹配用字面量+边界（不用 -like/pkill 通配或正
/// 则，无注入面、无 -120 误中 -1200 的前缀问题）。后台线程执行，静默失败
/// （清扫只是卫生问题，不阻塞启动）。
fn sweep_stale_orphans() {
    let own_pid = std::process::id();
    let mut stale: Vec<std::path::PathBuf> = Vec::new();
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(rest) = name.strip_prefix("rclaw-browser-profile") else {
            continue;
        };
        let is_stale = match rest.strip_prefix('-') {
            // 无后缀 = 旧版共享 profile，必然无主，必清。
            None => true,
            Some(pid) => match pid.parse::<u32>() {
                Ok(pid) => pid != own_pid && !pid_alive(pid),
                // 非数字后缀不是我们的产物，不碰。
                Err(_) => false,
            },
        };
        if is_stale {
            stale.push(entry.path());
        }
    }
    if stale.is_empty() {
        return;
    }
    kill_profile_holders(&stale);
    for dir in &stale {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// 平台判活：拿不到可靠判活就一律视为存活（宁漏勿杀）。
#[cfg(any(target_os = "linux", target_os = "android"))]
fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn pid_alive(_pid: u32) -> bool {
    true
}

/// 命令行是否把 dir 作为完整参数边界引用（引用点后随空白/引号/结尾）——
/// 防 profile-120 的前缀误中 profile-1200 的命令行。unix 侧查杀直接用它；
/// Windows 侧等价逻辑在 PowerShell 里（cmdline 枚举无 std 通道）。
#[cfg_attr(windows, allow(dead_code))]
fn cmdline_holds_dir(cmdline: &str, dir: &str) -> bool {
    let mut from = 0;
    while let Some(at) = cmdline[from..].find(dir) {
        let end = from + at + dir.len();
        if matches!(cmdline[end..].chars().next(), None | Some(' ') | Some('"') | Some('\'')) {
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
        let Ok(pid) = entry.file_name().into_string().and_then(|s| s.parse::<u32>()) else {
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

/// Windows：目录清单经 base64+JSON 传入（无引号注入面）；IndexOf 字面量
/// 匹配 + 边界检查；删除用 -LiteralPath（不展开通配）。
#[cfg(windows)]
fn kill_profile_holders(dirs: &[std::path::PathBuf]) {
    use std::os::windows::process::CommandExt as _;
    use base64::Engine as _;
    let list: Vec<String> = dirs
        .iter()
        .map(|d| d.to_string_lossy().into_owned())
        .collect();
    let b64 = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_string(&list).unwrap_or_default().as_bytes());
    let script = format!(
        "$dirs = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{b64}')) | \
         ConvertFrom-Json; foreach ($d in $dirs) {{ \
         Get-CimInstance Win32_Process -Filter \"Name='msedge.exe' or Name='chrome.exe'\" | \
         ForEach-Object {{ $cl = $_.CommandLine; if ($cl) {{ \
         $i = $cl.IndexOf($d, [System.StringComparison]::Ordinal); \
         while ($i -ge 0) {{ $e = $i + $d.Length; \
         $after = if ($e -lt $cl.Length) {{ $cl[$e] }} else {{ [char]0 }}; \
         if ($after -eq [char]0 -or $after -eq ' ' -or $after -eq '\"' -or $after -eq \"'\") {{ \
         Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue; break }} \
         $i = $cl.IndexOf($d, $e, [System.StringComparison]::Ordinal) }} }} }}; \
         Remove-Item -LiteralPath $d -Recurse -Force -ErrorAction SilentlyContinue }}"
    );
    // CREATE_NO_WINDOW：不闪控制台。
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .creation_flags(0x0800_0000)
        .output();
}

/// 清扫只跑一次（进程生命周期内一次即可）。
static SWEEP_ONCE: std::sync::Once = std::sync::Once::new();

/// 杀掉命令行里引用了本 profile 目录的【无头】浏览器实例：启动失败的清场
/// 路径用（重置 profile 前先清占用者），与清扫共用同一套字面量查杀。
fn kill_orphans_holding_profile(profile_dir: &std::path::Path) {
    let owned = profile_dir.to_path_buf();
    kill_profile_holders(std::slice::from_ref(&owned));
}

impl HeadlessChromiumTransport {
    pub fn new(screenshot_dir: impl Into<std::path::PathBuf>) -> Self {
        HeadlessChromiumTransport {
            inner: AsyncMutex::new(None),
            screenshot_dir: screenshot_dir.into(),
            // 按进程隔离：两个应用实例各用各的 profile，不互相抢锁（崩溃残留的
            // 孤儿由 kill_orphans_holding_profile 按各自目录特征清杀）。
            profile_dir: std::env::temp_dir().join(format!("rclaw-browser-profile-{}", std::process::id())),
        }
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
        ) -> Result<(std::process::Child, u16), String> {
            use std::io::BufRead as _;
            let mut command = std::process::Command::new(&exe);
            command
                .arg("--headless=new")
                .arg("--remote-debugging-port=0")
                // Chromium 开关值必须同参数（= 连接）：拆成两个参数时路径会被当
                // 成位置参数（URL），profile 落回默认目录——撞上已运行的实例
                // 就立即退出，stderr 随之关闭。
                .arg(format!("--user-data-dir={}", profile.display()))
                .args([
                    "--no-first-run",
                    "--no-default-browser-check",
                    "--disable-gpu",
                    "--hide-scrollbars",
                    "about:blank",
                ])
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
            // （Playwright 同款后台泵）。泵线程读到 DevTools 行即经通道通知本路径
            // 立刻返回，之后继续排到 EOF（随浏览器关闭自然结束）。
            let (tx, rx) = std::sync::mpsc::channel::<(Option<u16>, Vec<String>)>();
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
                        if let Some(found) =
                            authority.rsplit(':').next().and_then(|p| p.parse::<u16>().ok())
                        {
                            port = Some(found);
                            let _ = tx.send((port, std::mem::take(&mut seen)));
                            continue;
                        }
                    }
                    if seen.len() < 3 && !line.trim().is_empty() {
                        seen.push(line);
                    }
                }
                // EOF = 进程已退出（或从未报端口）：把结局带回主路径。
                let _ = tx.send((port, seen));
            });
            match rx.recv_timeout(Duration::from_secs(20)) {
                Ok((Some(port), _)) => Ok((child, port)),
                Ok((None, seen)) => {
                    // EOF = 进程已退出：带上退出状态便于定位（策略拦截 / 参数
                    // 不识别 / profile 锁等都会秒退）。
                    let status = child.wait().map_err(|e| format!("wait 失败: {e}"))?;
                    Err(format!(
                        "无头浏览器未报 DevTools 端口即退出（前几行: {seen:?}，退出状态: {status}）"
                    ))
                }
                Err(_) => {
                    // 超时：显式杀掉，防孤儿实例锁住 profile 卡死下次启动。
                    let _ = child.kill();
                    let _ = child.wait();
                    Err("等待无头浏览器调试端口超时".to_string())
                }
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
        let job = tokio::task::spawn_blocking(move || launch_blocking(exe, profile));
        // stderr 行读取是阻塞循环：外层限时兜底（内核异常时不无限等）。
        let (child, port) = tokio::time::timeout(Duration::from_secs(20), job)
            .await
            .map_err(|_| "等待无头浏览器调试端口超时".to_string())?
            .map_err(|e| format!("调度无头浏览器启动失败: {e}"))??;
        tracing::info!(port, "CDP 无头浏览器已拉起");
        if let Ok(mut guard) = HEADLESS_CHILD.lock() {
            *guard = Some(child);
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
                kill_orphans_holding_profile(&self.profile_dir);
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
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(format!("已截图：browser/{name}（工作区内，可用 read 读取）"))
    }
}

// ---------------------------------------------------------------------------
// 共享内核：CdpBrowser
// ---------------------------------------------------------------------------

/// CDP 浏览器共享内核（web_browser 与 browser_* 工具共用）。
pub struct CdpBrowser {
    transport: Arc<dyn DevtoolsTransport>,
    session: AsyncMutex<Option<Session>>,
    /// 操作互斥：浏览器是进程级单例（对话 turn / cron turn 可能并发调工具），
    /// 「导航→等加载→提取」这类多步事务必须整体串行，否则两个并发取页会互相
    /// 覆盖页面、各自提取到对方的 URL。
    op: AsyncMutex<()>,
}

impl CdpBrowser {
    pub fn new(transport: Arc<dyn DevtoolsTransport>) -> Self {
        CdpBrowser {
            transport,
            session: AsyncMutex::new(None),
            op: AsyncMutex::new(()),
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
        *guard = Some(session.clone());
        Ok(session)
    }

    /// 导航公共段：Page.navigate → 等 loadEventFired → 静置 800ms。
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
        let _op = self.op.lock().await;
        let session = self.navigate_wait(url, Duration::from_secs(45)).await?;
        let info = self.page_info(&session).await?;
        Ok(format_report(&info))
    }

    /// web_browser 后端：导航 + 提取 JS（结果字段与取页内核约定一致）。
    pub async fn navigate_and_extract(&self, url: &str) -> Result<PagePayload, String> {
        let _op = self.op.lock().await;
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
        let _op = self.op.lock().await;
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
        let _op = self.op.lock().await;
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

/// 页面提取（web_browser）：引擎感知的搜索结果列表（bing/baidu/google，过滤
/// 广告）+ Readability 式主文正文。google 分支的摘要选择器加 gbox 兜底。
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

/// 进程级共享内核：注册可能发生多次（安卓服务重启重建注册表、桌面重装配），
/// 而底层会话是进程单例（安卓 BrowserSession 静态字段 / 桌面 HEADLESS_CHILD），
/// 两个内核实例驱动同一会话会互相串页、互相关闭——首个 transport 定格复用。
static SHARED_BROWSER: std::sync::OnceLock<Arc<CdpBrowser>> = std::sync::OnceLock::new();

/// web_browser 换 CDP 会话后端，并挂 browser_* 交互工具（全部共享同一内核）。
pub fn register(tools: &mut crate::ToolRegistry, transport: Arc<dyn DevtoolsTransport>) {
    tools.remove("web_browser");
    let browser = SHARED_BROWSER
        .get_or_init(|| Arc::new(CdpBrowser::new(transport)))
        .clone();
    tools.register(Box::new(WebBrowserTool::new(Arc::new(CdpPageLoader {
        browser: browser.clone(),
    }))));
    tools.register(Box::new(BrowserNavigateTool {
        browser: browser.clone(),
    }));
    tools.register(Box::new(BrowserSnapshotTool {
        browser: browser.clone(),
    }));
    tools.register(Box::new(BrowserClickTool {
        browser: browser.clone(),
    }));
    tools.register(Box::new(BrowserTypeTool {
        browser: browser.clone(),
    }));
    tools.register(Box::new(BrowserScreenshotTool {
        browser: browser.clone(),
    }));
    tools.register(Box::new(BrowserCloseTool { browser }));
}

/// web_browser 的 CDP 后端：导航 + 提取（`PageLoader` 是阻塞 trait，调用方已在
/// spawn_blocking 里，这里 block_on 进异步会话）。
pub struct CdpPageLoader {
    browser: Arc<CdpBrowser>,
}

impl PageLoader for CdpPageLoader {
    fn load(&self, url: &str) -> Result<PagePayload, String> {
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

browser_tool_impl!(
    BrowserNavigateTool,
    "browser_navigate",
    "CDP 浏览器：打开 URL 并等待加载完成（真实 Chromium 内核；请求头已去自动化指纹，可正常访问 Google）。返回页面标题与 URL，检测到 Google 风控页（CAPTCHA）/同意页时明确标注。只取正文用 web_browser 即可；需要交互（点击/输入/截图）用 browser_* 系列。",
    async fn execute(&self, input: Value) -> ToolResult {
        let url = match input.get("url").and_then(Value::as_str) {
            Some(url) => url,
            None => return ToolResult::err("缺少参数 url"),
        };
        match normalize_url(url) {
            Ok(url) => match self.browser.navigate(&url).await {
                Ok(report) => ToolResult::ok(report),
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
                "url": { "type": "string", "description": "目标页面 URL（无 scheme 自动补 https://）" }
            },
            "required": ["url"]
        })
    }
}

pub struct BrowserSnapshotTool {
    browser: Arc<CdpBrowser>,
}

browser_tool_impl!(
    BrowserSnapshotTool,
    "browser_snapshot",
    "CDP 浏览器：列出当前页面可交互元素（链接/按钮/输入框/标题），每项带 [ref] 编号。browser_click / browser_type 按 ref 定位。页面跳转后 ref 失效，需重新快照。",
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

pub struct BrowserClickTool {
    browser: Arc<CdpBrowser>,
}

browser_tool_impl!(
    BrowserClickTool,
    "browser_click",
    "CDP 浏览器：点按 snapshot 给出的 [ref] 元素（真实鼠标事件，非 JS 合成点击）。返回点按位置与当前页面标题/URL。",
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
    "CDP 浏览器：聚焦 [ref] 输入框，先清空原内容再输入文本（IME 通路，事件完整；适合搜索框/表单）。",
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
}

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
        let report = browser.navigate("https://example.com").await?;
        eprintln!("[cdp] navigate 完成: {report}");
        assert!(report.contains("example"), "{report}");
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
