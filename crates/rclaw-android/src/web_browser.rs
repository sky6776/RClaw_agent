//! Android 取页后端（`web_browser` 工具的 [`PageLoader`] 实现）：
//! 经 `jni_bridge::browse` 调 `NativeBridge.browse` → `HeadlessBrowser`
//! 用系统 WebView 离屏渲染 + JS 提取，返回 JSON 由这里解析成
//! [`PagePayload`]。
//!
//! 工具面（参数校验/URL 构建/结果格式化/单测）在 `rclaw-tools::web_browser`，
//! 与桌面/CLI 共用；本模块只负责把 APK 宿主的默认后端（spawn 无头浏览器，
//! Android 上不可用）替换成 WebView 后端。

use std::sync::Arc;

use rclaw_tools::web_browser::{PageLoader, PagePayload, WebBrowserTool};

/// Java 侧整体超时（含页面加载与 JS 提取；Rust 侧外层还有 45s 兜底）。
const BROWSE_TIMEOUT_MS: u64 = 30_000;

/// 替换装配期注册的默认 web_browser 后端为 WebView 实现。
pub fn register(tools: &mut rclaw_tools::ToolRegistry) {
    tools.remove("web_browser");
    tools.register(Box::new(WebBrowserTool::new(Arc::new(AndroidPageLoader))));
}

struct AndroidPageLoader;

#[derive(serde::Deserialize)]
struct BrowseReply {
    ok: bool,
    #[serde(default)]
    page: Option<PagePayload>,
    #[serde(default)]
    error: Option<String>,
}

impl PageLoader for AndroidPageLoader {
    fn load(&self, url: &str) -> Result<PagePayload, String> {
        let payload = crate::jni_bridge::browse(url, BROWSE_TIMEOUT_MS).map_err(|e| e.to_string())?;
        let reply: BrowseReply = serde_json::from_str(&payload)
            .map_err(|e| format!("解析无头浏览器返回失败: {e}"))?;
        if !reply.ok {
            return Err(format!(
                "无头浏览器失败: {}",
                reply.error.as_deref().unwrap_or("未知原因")
            ));
        }
        reply
            .page
            .ok_or_else(|| "无头浏览器无页面数据".to_string())
    }
}
