//! MCP（Model Context Protocol）支持：协议核心 + 传输（stdio / streamable-http）+ 工具集成。
//!
//! - [`jsonrpc::Core`]：传输无关的 JSON-RPC 协议核心（握手 / tools / 分发 / pending 路由）。
//! - [`transport`]：[`McpTransport`] trait + [`transport::StdioTransport`] / [`transport::HttpTransport`]。
//! - [`client::McpClient`]：核心 + 传输的薄外壳，公共 API 与传输无关。
//! - [`provider::McpToolWrapper`]：把远端 MCP 工具适配为 RClaw 的 [`crate::Tool`]，限定名
//!   `<server>__<tool>`，与内置工具并列注册进 [`crate::ToolRegistry`]。
//! - [`manager::McpManager`]：加载配置、自动/按需启动服务器、把工具注册进注册表。
//! - [`config`]：配置结构。主配置来自 `rclaw.toml` 的 `[mcp]` 段。
//!
//! 传输层支持 stdio（子进程，零网络依赖）与 streamable-http（远程，per-request POST）。
//! 远程 server 在 `[mcp.servers.<name>]` 配置 `url`（+ `headers`）即可，transport 自动推断为 http。

pub mod client;
pub mod config;
pub mod jsonrpc;
pub mod manager;
pub mod provider;
pub mod transport;

pub use client::McpClient;
pub use config::{McpConfig, McpResolved, McpResolvedServer, McpServerDef, TransportKind};
pub use jsonrpc::McpTool;
pub use manager::{mcp_access_allows, McpManager, McpToolTarget};
pub use provider::McpToolWrapper;

// summarize_json 定义在 jsonrpc；此处 re-export 供 provider 经 `super::summarize_json` 访问。
pub(crate) use jsonrpc::summarize_json;

use thiserror::Error;

/// MCP 层错误。
///
/// - [`McpError::Tool`]：与单个 MCP 服务器交互时的运行时错误（spawn/IO/超时/协议/HTTP），
///   消息已带 `[<server>]` 前缀，便于在多服务器并存时定位。
/// - [`McpError::Config`]：配置解析/合并错误（启动期 fail-fast）。
/// - [`McpError::NotFound`]：按名查找服务器但不存在。
#[derive(Debug, Error)]
pub enum McpError {
    #[error("{0}")]
    Tool(String),
    #[error("MCP 配置错误: {0}")]
    Config(String),
    #[error("MCP 服务器 '{0}' 未找到")]
    NotFound(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, McpError>;
