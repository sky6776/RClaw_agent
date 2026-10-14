//! Android 宿主的 browser 工具装配入口（web_browser 换 CDP 后端 + browser_*
//! 交互工具；协议与工具面在 `rclaw_tools::cdp_browser`，传输实现在本 crate
//! `cdp_browser::AndroidTransport`）。

pub fn register(tools: &mut rclaw_tools::ToolRegistry) {
    crate::cdp_browser::register(tools);
}
