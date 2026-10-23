//! Android 传输（CDP 浏览器）：长驻调试 WebView 的 abstract unix socket。
//!
//! 协议层/工具面在 `rclaw_tools::cdp_browser`（与桌面无头 Chromium 共用）；
//! 本模块只提供 [`DevtoolsTransport`] 的安卓实现——确保 `BrowserSession`
//! （Java，`setWebContentsDebuggingEnabled`）在跑，连其 DevTools 抽象套接字
//! `webview_devtools_remote_<pid>`。截图双轨：WebView 挂在悬浮窗面板里时
//! （`browser_overlay_attached`）有真实合成器帧，走 CDP `Page.captureScreenshot`；
//! 离屏时走 `View.draw` 软件位图（`Page.captureScreenshot` 出空白）。Google
//! 风控页命中时外呼 `notifyCaptchaDetected`（高优通知 + 自动展开悬浮窗）。

use std::sync::Arc;
use std::time::Duration;

use rclaw_tools::cdp_browser::{
    capture_screenshot_via_cdp, probe_page_target, DevtoolsTransport, RawStream, Session,
};

/// Android 传输实现 + 拟人化 browser_* 工具面注册。
pub fn register(tools: &mut rclaw_tools::ToolRegistry) {
    let screenshot_dir = crate::host_context()
        .map(|ctx| ctx.files_dir.join("agent").join("browser"))
        .unwrap_or_else(std::env::temp_dir);
    rclaw_tools::cdp_browser::register(
        tools,
        Arc::new(AndroidTransport {
            screenshot_dir,
            metrics: std::sync::Mutex::new(None),
        }),
    );
    // 媒体打开：APK 沙箱文件经 FileProvider 只读 URI 交给系统查看器/播放器
    //（聊天面板是纯文本出站，图片/视频「直接看到/播放」走这条通路）。
    tools.register(Box::new(OpenMediaTool));
}

/// 打开工作区媒体文件（系统查看器/播放器）。
struct OpenMediaTool;

#[async_trait::async_trait]
impl rclaw_tools::Tool for OpenMediaTool {
    fn name(&self) -> &str {
        "open_media"
    }
    fn description(&self) -> &str {
        "打开工作区内的媒体文件（图片/视频/音频——经 FileProvider 只读 URI 交给系统查看器或播放器，即「用其他应用打开」）。参数 path 为工作区相对路径（如 browser/xxx.png）。把搜索到的图文/视频展示给用户时用；桌面宿主无此工具。"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "工作区相对路径（如 browser/screenshot_1.png、media/note.mp4）" }
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> rclaw_tools::ToolResult {
        let Some(path) = input.get("path").and_then(serde_json::Value::as_str) else {
            return rclaw_tools::ToolResult::err("缺少参数 path");
        };
        if path.starts_with('/')
            || path.starts_with('\\')
            || path.split(['/', '\\']).any(|seg| seg == "..")
        {
            return rclaw_tools::ToolResult::err(
                "path 必须是工作区相对路径（禁止绝对路径与 .. 越界）",
            );
        }
        let owned = path.to_string();
        match tokio::task::spawn_blocking(move || crate::open_media(&owned)).await {
            Ok(Ok(())) => rclaw_tools::ToolResult::ok(format!("已交给系统打开：{path}")),
            Ok(Err(error)) => rclaw_tools::ToolResult::err(format!("{error:#}")),
            Err(error) => rclaw_tools::ToolResult::err(format!("JNI 调度失败: {error}")),
        }
    }
}

pub struct AndroidTransport {
    screenshot_dir: std::path::PathBuf,
    /// 会话启动时 Java 带回的设备视口（CSS 尺寸 + dpr），供内核钉住渲染视口
    /// （connect 时写入；同一连接的 humanize/视口覆写随即读取）。
    metrics: std::sync::Mutex<Option<(i64, i64, f64)>>,
}

#[async_trait::async_trait]
impl DevtoolsTransport for AndroidTransport {
    async fn connect(&self) -> Result<(RawStream, String), String> {
        let info = tokio::task::spawn_blocking(crate::browser_session_start)
            .await
            .map_err(|e| format!("JNI 调度失败: {e}"))?
            .map_err(|e| e.to_string())?;
        // CSS 视口 = 物理/dpr（BrowserSession 布局用物理像素；WebView 渲染器
        // 的 app-context 度量不可信，内核用这组真实值覆写）。
        let css_w = (info.width as f64 / info.dpr).round().max(320.0) as i64;
        let css_h = (info.height as f64 / info.dpr).round().max(480.0) as i64;
        if let Ok(mut slot) = self.metrics.lock() {
            *slot = Some((css_w, css_h, info.dpr));
        }
        tracing::info!(
            pid = info.pid,
            width = info.width,
            height = info.height,
            dpr = info.dpr,
            "WebView 会话就绪（离屏布局物理像素；CSS 视口 = 物理/dpr）"
        );
        let socket_name = format!("webview_devtools_remote_{}", info.pid);
        // WebView 创建后调试 socket 就绪有延迟：整体带重试。
        let mut last = String::new();
        for _ in 0..12 {
            match Self::try_once(&socket_name).await {
                Ok(pair) => return Ok(pair),
                Err(error) => {
                    last = error;
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
        Err(format!(
            "连接 WebView DevTools 失败（连续重试超限）: {last}"
        ))
    }

    async fn shutdown(&self) -> Result<(), String> {
        tokio::task::spawn_blocking(crate::browser_session_stop)
            .await
            .map_err(|e| format!("JNI 调度失败: {e}"))?
            .map_err(|e| e.to_string())
    }

    fn screenshot_dir(&self) -> std::path::PathBuf {
        self.screenshot_dir.clone()
    }

    async fn capture_screenshot(
        &self,
        session: &Session,
        dest: &std::path::Path,
    ) -> Result<String, String> {
        // 双轨：attached（悬浮窗面板里）有合成器帧 → CDP 截图更真实；
        // 离屏合成器无帧 → View.draw 软件位图兜底。
        let attached = tokio::task::spawn_blocking(crate::browser_overlay_attached)
            .await
            .unwrap_or(false);
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if attached {
            capture_screenshot_via_cdp(session, dest).await?;
            return Ok(format!(
                "已截图：browser/{name}（工作区内，可用 read 读取或 openMedia 查看）"
            ));
        }
        let abs = dest.to_string_lossy().into_owned();
        tokio::task::spawn_blocking(move || crate::browser_session_screenshot(&abs))
            .await
            .map_err(|e| format!("JNI 调度失败: {e}"))?
            .map_err(|e| e.to_string())?;
        Ok(format!(
            "已截图：browser/{name}（工作区内，可用 read 读取或 openMedia 查看）"
        ))
    }

    async fn notify_captcha(&self, url: &str) {
        if let Err(error) = crate::notify_captcha_detected(url) {
            tracing::warn!("过码通知外呼失败（park 照常等待）: {error}");
        }
    }

    fn device_metrics(&self) -> Option<(i64, i64, f64)> {
        match self.metrics.lock() {
            Ok(slot) => *slot,
            Err(_) => None,
        }
    }

    async fn overlay_attached(&self) -> bool {
        tokio::task::spawn_blocking(crate::browser_overlay_attached)
            .await
            .unwrap_or(false)
    }
}

impl AndroidTransport {
    /// 一次尝试：连 abstract socket → /json/list 探测（一发一收）→ 为 WS
    /// 握手再开一条流（协议层在原始流上统一握手）。
    async fn try_once(socket_name: &str) -> Result<(RawStream, String), String> {
        let mut probe_stream = socket_layer::connect_abstract_socket(socket_name)
            .await
            .map_err(|e| e.to_string())?;
        let target = probe_page_target(&mut probe_stream)
            .await
            .map_err(|e| e.to_string())?;
        drop(probe_stream);
        let ws_stream = socket_layer::connect_abstract_socket(socket_name)
            .await
            .map_err(|e| e.to_string())?;
        Ok((Box::new(ws_stream), target))
    }
}

mod socket_layer {
    //! DevTools abstract unix socket（std 1.70+ 的 SocketAddrExt；linux/android
    //! 各自的 os 模块提供——socket2 0.6 没有抽象地址构造器）。

    /// 连接 abstract unix socket（名字不带前导 @ / \0）。
    pub(super) async fn connect_abstract_socket(
        socket_name: &str,
    ) -> anyhow::Result<tokio::net::UnixStream> {
        #[cfg(target_os = "android")]
        use std::os::android::net::SocketAddrExt as _;
        #[cfg(target_os = "linux")]
        use std::os::linux::net::SocketAddrExt as _;
        use std::os::unix::net::SocketAddr as StdSocketAddr;
        let addr = StdSocketAddr::from_abstract_name(socket_name.as_bytes())?;
        // tokio 的 UnixStream 没有暴露 connect_addr：std 连接（本地套接字，
        // 毫秒级）后转 tokio。
        let stream = std::os::unix::net::UnixStream::connect_addr(&addr)?;
        stream.set_nonblocking(true)?;
        Ok(tokio::net::UnixStream::from_std(stream)?)
    }
}
