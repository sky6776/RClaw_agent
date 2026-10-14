//! Android 取页后端（`web_browser` 工具的 [`PageLoader`] 实现）：
//! 经 [`cdp_browser`] 的长驻 CDP 会话导航 + 提取。
//!
//! 相比旧版「一次性离屏 WebView + JS 提取」，CDP 会话带来三件事：
//! 真实 WebView UA 与会话 Cookie（Google 放行的前提）、`Fetch` 拦截摘掉
//! 网络栈强加的 `X-Requested-With`（应用包名指纹）、以及与 browser_* 交互
//! 工具共用同一个内核（不再每调用起一个 WebView）。
//!
//! 工具面（参数校验/URL 构建/结果格式化/单测）在 `rclaw-tools::web_browser`，
//! 与桌面/CLI 共用；本模块只负责换掉 APK 宿主的默认后端，并挂上交互工具。

use std::sync::Arc;

use rclaw_tools::web_browser::{PageLoader, PagePayload};

use crate::cdp_browser;

/// 替换装配期注册的默认 web_browser 后端为 CDP 会话实现，并注册 browser_*
/// 交互工具（navigate / snapshot / click / type / screenshot / close）。
pub fn register(tools: &mut rclaw_tools::ToolRegistry) {
    tools.remove("web_browser");
    let browser = Arc::new(cdp_browser::shared());
    tools.register(Box::new(rclaw_tools::web_browser::WebBrowserTool::new(
        Arc::new(CdpPageLoader { browser }),
    )));
    cdp_browser::register_tools(tools);
}

/// web_browser 的 CDP 后端：导航 + 提取（`PageLoader` 是阻塞 trait，调用方已在
/// spawn_blocking 里，这里 block_on 进异步会话）。
struct CdpPageLoader {
    browser: &'static cdp_browser::CdpBrowser,
}

impl PageLoader for CdpPageLoader {
    fn load(&self, url: &str) -> Result<PagePayload, String> {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(self.browser.navigate_and_extract(url))
    }
}
