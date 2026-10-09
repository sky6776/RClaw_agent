//! MCP 传输层抽象：把 JSON-RPC 的「出站写一行 / 连接存活 / 关闭」从协议核心剥离。
//!
//! 当前实现：
//! - [`StdioTransport`]：spawn 子进程，stdin/stdout + 阻塞读线程（迁自原 client.rs）。
//! - [`HttpTransport`]：streamable-http，per-request POST，响应可为 JSON 或 SSE。
//!
//! 协议状态与路由在 [`crate::mcp::jsonrpc::Core`]；传输只负责搬字节。
//!
//! Core ↔ transport 的 Arc 环：传输持 `Weak<Core>`（经 [`McpTransport::bind_core`] 注入），
//! 读到消息时 upgrade 回放给 [`Core::route_inbound`](crate::mcp::jsonrpc::Core::route_inbound)。

use std::sync::Weak;

use async_trait::async_trait;

use super::jsonrpc::Core;
use super::Result as McpResult;

pub mod http;
pub mod stdio;

pub use http::HttpTransport;
pub use stdio::StdioTransport;

/// MCP 传输：出站一行 JSON-RPC + 存活判定 + 优雅关闭。
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// 发送一行（请求/响应/通知，含换行外的纯 JSON 字符串）。
    async fn send_raw(&self, line: &str) -> McpResult<()>;
    /// 连接是否仍可用。
    fn is_alive(&self) -> bool;
    /// 优雅关闭（async 路径；stdio kill 子进程，http 发 DELETE）。
    async fn shutdown(&self);
    /// 同步关闭（`McpClient::drop` 用，无法 await）。默认空；stdio 覆盖为 kill 子进程。
    fn shutdown_blocking(&self) {}
    /// 注入协议核心的弱引用，供传输在收到消息时回放（http 用；stdio 默认空——它在 spawn
    /// 时已拿到 weak core）。
    fn bind_core(&self, _core: Weak<Core>) {}
}
