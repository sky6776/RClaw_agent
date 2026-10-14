//! Android 传输（CDP 浏览器）：长驻调试 WebView 的 abstract unix socket。
//!
//! 协议层/工具面在 `rclaw_tools::cdp_browser`（与桌面无头 Chromium 共用）；
//! 本模块只提供 [`DevtoolsTransport`] 的安卓实现——确保 `BrowserSession`
//! （Java，`setWebContentsDebuggingEnabled`）在跑，连其 DevTools 抽象套接字
//! `webview_devtools_remote_<pid>`，截图走 `View.draw` 软件位图（离屏无合成
//! 器帧，CDP 截图空白）。

use std::sync::Arc;
use std::time::Duration;

use rclaw_tools::cdp_browser::{probe_page_target, DevtoolsTransport, RawStream};

/// Android 传输实现 + web_browser / browser_* 工具注册。
pub fn register(tools: &mut rclaw_tools::ToolRegistry) {
    let screenshot_dir = crate::host_context()
        .map(|ctx| ctx.files_dir.join("agent").join("browser"))
        .unwrap_or_else(std::env::temp_dir);
    rclaw_tools::cdp_browser::register(
        tools,
        Arc::new(AndroidTransport { screenshot_dir }),
    );
}

pub struct AndroidTransport {
    screenshot_dir: std::path::PathBuf,
}

#[async_trait::async_trait]
impl DevtoolsTransport for AndroidTransport {
    async fn connect(&self) -> Result<(RawStream, String), String> {
        let pid = tokio::task::spawn_blocking(crate::browser_session_start)
            .await
            .map_err(|e| format!("JNI 调度失败: {e}"))?
            .map_err(|e| e.to_string())?;
        let socket_name = format!("webview_devtools_remote_{pid}");
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
        Err(format!("连接 WebView DevTools 失败（连续重试超限）: {last}"))
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
        _session: &rclaw_tools::cdp_browser::Session,
        dest: &std::path::Path,
    ) -> Result<String, String> {
        let abs = dest.to_string_lossy().into_owned();
        tokio::task::spawn_blocking(move || crate::browser_session_screenshot(&abs))
            .await
            .map_err(|e| format!("JNI 调度失败: {e}"))?
            .map_err(|e| e.to_string())?;
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(format!(
            "已截图：browser/{name}（工作区内，可用 read 读取或 openMedia 查看）"
        ))
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
        use std::os::unix::net::SocketAddr as StdSocketAddr;
        #[cfg(target_os = "linux")]
        use std::os::linux::net::SocketAddrExt as _;
        #[cfg(target_os = "android")]
        use std::os::android::net::SocketAddrExt as _;
        let addr = StdSocketAddr::from_abstract_name(socket_name.as_bytes())?;
        // tokio 的 UnixStream 没有暴露 connect_addr：std 连接（本地套接字，
        // 毫秒级）后转 tokio。
        let stream = std::os::unix::net::UnixStream::connect_addr(&addr)?;
        stream.set_nonblocking(true)?;
        Ok(tokio::net::UnixStream::from_std(stream)?)
    }
}
