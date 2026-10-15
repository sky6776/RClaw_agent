//! Android 宿主的 browser 工具装配入口（拟人化 browser_* 工具面）。
//! 协议与工具面在 `rclaw_tools::cdp_browser`，传输实现在本 crate
//! `cdp_browser::AndroidTransport`。

pub fn register(tools: &mut rclaw_tools::ToolRegistry) {
    crate::cdp_browser::register(tools);
}
