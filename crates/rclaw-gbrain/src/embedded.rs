//! 进程内 gbrain 工具运行时。
//!
//! Android 等宿主直接链接本 crate，通过此 API 枚举和调用工具；不会启动 MCP stdio
//! server，也不会创建 `gbrain` 子进程。stdio MCP 仍由 [`crate::mcp::McpServer`] 提供。

use crate::autopilot::spawn_autopilot_thread;
use crate::config::Config;
use crate::engine::BrainEngine;
use crate::error::Result;
use crate::kb::worker::spawn_kb_worker_pool;
use crate::mcp::tool_defs::{build_tool_defs, ToolDef};
use crate::mcp::McpServer;
use crate::sqlite_engine::SqliteEngine;
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 可嵌入宿主使用的 gbrain 运行时。
pub struct EmbeddedGBrain {
    service: McpServer,
    worker_shutdown: Arc<AtomicBool>,
    autopilot_shutdown: Option<Arc<AtomicBool>>,
}

impl EmbeddedGBrain {
    /// 打开或创建数据库，并按配置启动可停止的后台维护任务。
    pub fn open(
        db_path: impl AsRef<Path>,
        working_dir: impl AsRef<Path>,
        mut config: Config,
    ) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        let working_dir = working_dir.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(&working_dir)?;

        config.database_path = Some(db_path.to_string_lossy().to_string());
        if config.artifact_storage_dir.is_none() {
            config.artifact_storage_dir =
                Some(working_dir.join("artifacts").to_string_lossy().to_string());
        }
        if config.kb_storage_dir.is_none() {
            config.kb_storage_dir =
                Some(working_dir.join("kb_files").to_string_lossy().to_string());
        }

        let mut engine = SqliteEngine::with_config(&db_path, config.clone());
        engine.connect()?;
        engine.init_schema()?;

        let worker_shutdown = Arc::new(AtomicBool::new(false));
        if config.kb_enabled && config.kb_worker_enabled {
            spawn_kb_worker_pool(
                db_path.clone(),
                config.clone(),
                config.kb_worker_poll_interval_secs,
                1,
                worker_shutdown.clone(),
            );
        }
        let autopilot_shutdown = if config.autopilot_enabled {
            spawn_autopilot_thread(db_path, config.clone(), config.autopilot_interval_secs)
        } else {
            None
        };

        Ok(Self {
            service: McpServer::with_config_and_working_dir(engine, config, working_dir),
            worker_shutdown,
            autopilot_shutdown,
        })
    }

    /// 返回与 stdio MCP 完全相同的工具定义。
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        build_tool_defs()
    }

    /// 在当前进程内调用工具，不经过 JSON-RPC/stdio。
    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.service.dispatch_tool_call(name, arguments)
    }

    /// 通知所有可停止的后台循环退出。
    pub fn shutdown(&self) {
        self.worker_shutdown.store(true, Ordering::Relaxed);
        if let Some(shutdown) = &self.autopilot_shutdown {
            shutdown.store(true, Ordering::Relaxed);
        }
    }
}

impl Drop for EmbeddedGBrain {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_runtime_lists_and_calls_tools_without_stdio() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = Config {
            kb_worker_enabled: false,
            autopilot_enabled: false,
            artifact_manual_memory_to_kb: false,
            ..Config::default()
        };

        let mut runtime = EmbeddedGBrain::open(
            tmp.path().join("data").join("brain.db"),
            tmp.path().join("workspace"),
            config,
        )
        .expect("open embedded gbrain");

        assert!(runtime
            .tool_defs()
            .iter()
            .any(|tool| tool.name == "artifact_put"));
        let result = runtime
            .call_tool(
                "artifact_put",
                serde_json::json!({
                    "slug": "wiki/android",
                    "content": "embedded call"
                }),
            )
            .expect("call artifact_put");
        assert!(result.get("artifact_id").is_some());
    }
}
