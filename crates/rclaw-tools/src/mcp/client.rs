//! MCP 客户端：协议核心 + 传输前端的组合外壳。
//!
//! [`McpClient`] 是一个极薄的外壳，持有传输无关的协议核心 [`Core`](super::jsonrpc::Core)
//! 与一个 [`McpTransport`](super::transport::McpTransport) 实现（stdio 或 streamable-http），
//! 把公共方法委托给核心。这样 [`provider::McpToolWrapper`](super::provider::McpToolWrapper)
//! 与 [`manager::McpManager`](super::manager::McpManager) 无需感知传输类型。
//!
//! 历史上本文件包含全部 stdio 逻辑；现按职责拆分：
//! - 协议（握手/tools/call/分发）→ [`super::jsonrpc`]
//! - stdio 传输（子进程/读线程）→ [`super::transport::stdio`]
//! - http 传输（streamable-http）→ [`super::transport::http`]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::jsonrpc::Core;
use super::transport::{HttpTransport, McpTransport, StdioTransport};
use super::{McpError, Result as McpResult};

// 兼容 re-export：provider.rs 等经 `super::client::{McpTool, McpToolResult}` 访问。
pub use super::jsonrpc::{McpTool, McpToolResult};

/// 一个已连接的 MCP server（协议核心 + 传输）。
///
/// 公共 API 与重构前一致：[`Self::list_tools`] / [`Self::call_tool`] / [`Self::is_alive`] 等，
/// 故 `provider` / `manager` 层无改动。
pub struct McpClient {
    core: Arc<Core>,
    transport: Arc<dyn McpTransport>,
}

impl McpClient {
    /// stdio 入口：spawn 子进程 + 读线程，完成 initialize 握手与首次 tools/list。
    pub async fn start_stdio(
        name: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: Option<&str>,
        startup_timeout: Duration,
        call_timeout: Duration,
    ) -> McpResult<Self> {
        let core = Core::new(name.to_string(), call_timeout);
        let alive = core.alive_handle();
        let transport: Arc<dyn McpTransport> = Arc::new(StdioTransport::spawn(
            Arc::downgrade(&core),
            alive,
            name,
            command,
            args,
            env,
            cwd,
        )?);
        core.set_transport(Arc::clone(&transport));

        timeout_handshake(&core, name, startup_timeout, "stdio").await?;
        Ok(Self { core, transport })
    }

    /// streamable-http 入口：构造 HTTP 传输，完成 initialize 握手与首次 tools/list。
    pub async fn start_http(
        name: &str,
        url: &str,
        headers: &Option<HashMap<String, String>>,
        startup_timeout: Duration,
        call_timeout: Duration,
    ) -> McpResult<Self> {
        let core = Core::new(name.to_string(), call_timeout);
        let http = HttpTransport::new(
            name.to_string(),
            url.to_string(),
            headers.clone(),
            call_timeout,
        )
        .await?;
        let transport: Arc<dyn McpTransport> = Arc::new(http);
        core.set_transport(Arc::clone(&transport));

        timeout_handshake(&core, name, startup_timeout, "http").await?;
        Ok(Self { core, transport })
    }

    pub async fn list_tools(&self) -> Vec<McpTool> {
        self.core.list_tools().await
    }

    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> McpResult<McpToolResult> {
        self.core.call_tool(tool_name, arguments).await
    }

    pub fn is_alive(&self) -> bool {
        // 读 Core 的共享存活标志：stdio 下与读线程/StdioTransport 同一把 Arc；http 下由
        // Core::call 在 send_raw 失败时 mark_dead。注意不能委托 transport.is_alive()——
        // HttpTransport::is_alive 恒为 true，会导致 provider 的「中断重建重试」对 http 永不触发。
        self.core.is_alive()
    }

    pub fn protocol_version(&self) -> Option<String> {
        self.core.protocol_version()
    }

    pub fn server_capabilities(&self) -> serde_json::Value {
        self.core.server_capabilities()
    }
}

/// 在 `startup_timeout` 内完成 `initialize` + 首次 `tools/list`；超时即返回错误，避免卡死 agent。
async fn timeout_handshake(
    core: &Arc<Core>,
    name: &str,
    startup_timeout: Duration,
    kind: &str,
) -> McpResult<()> {
    match tokio::time::timeout(startup_timeout, async {
        core.initialize().await?;
        core.refresh_tools().await?;
        Ok::<(), McpError>(())
    })
    .await
    {
        Ok(res) => res,
        Err(_) => Err(McpError::Tool(format!(
            "MCP[{name}]: {kind} 启动超时（{}s 内未完成握手/工具列表）",
            startup_timeout.as_secs()
        ))),
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // 同步关闭（不能 await）：stdio kill 子进程；http best-effort spawn 一次 DELETE 终止会话。
        self.transport.shutdown_blocking();
    }
}
