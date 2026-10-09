//! RClaw tool-registry adapter for the in-process gbrain runtime.

use crate::config::Config;
use crate::embedded::EmbeddedGBrain;
use crate::error::GBrainError;
use crate::mcp::tool_defs::ToolDef;
use async_trait::async_trait;
use rclaw_tools::{Tool, ToolRegistry, ToolResult};
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Owns the embedded gbrain runtime and registers its tools in RClaw.
pub struct GBrainTools {
    service: Arc<Mutex<EmbeddedGBrain>>,
    defs: Vec<ToolDef>,
    healthy: Arc<AtomicBool>,
}

impl GBrainTools {
    /// Opens gbrain directly inside the RClaw process without stdio or a child process.
    pub fn open(
        db_path: impl AsRef<Path>,
        working_dir: impl AsRef<Path>,
        config: Config,
    ) -> Result<Self, GBrainError> {
        let service = EmbeddedGBrain::open(db_path, working_dir, config)?;
        let defs = service.tool_defs();
        Ok(Self {
            service: Arc::new(Mutex::new(service)),
            defs,
            healthy: Arc::new(AtomicBool::new(true)),
        })
    }

    /// Registers all tools as `gbrain__<tool>` to avoid name collisions.
    pub fn extend_registry(&self, registry: &mut ToolRegistry) -> usize {
        self.extend_registry_with_timeout(registry, None)
    }

    /// Registers the embedded tools with an optional per-call timeout. The
    /// timeout mirrors the host MCP `call_timeout_secs` setting.
    pub fn extend_registry_with_timeout(
        &self,
        registry: &mut ToolRegistry,
        call_timeout: Option<Duration>,
    ) -> usize {
        let before = registry.schemas().len();
        for def in &self.defs {
            registry.register(Box::new(GBrainTool {
                exposed_name: format!("gbrain__{}", def.name),
                remote_name: def.name.clone(),
                description: def.description.clone(),
                input_schema: def.input_schema.clone(),
                service: self.service.clone(),
                call_timeout,
                healthy: self.healthy.clone(),
            }));
        }
        registry.schemas().len() - before
    }

    pub fn shutdown(&self) {
        // A timed-out blocking call may still own the mutex. Process-level
        // recovery is the cancellation boundary in that state, so graceful
        // shutdown must not wait forever for the poisoned runtime.
        if !self.healthy.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(service) = self.service.lock() {
            service.shutdown();
        }
    }

    /// Shared health flag for embedded hosts which can restart their process.
    pub fn health_flag(&self) -> Arc<AtomicBool> {
        self.healthy.clone()
    }
}

impl Drop for GBrainTools {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct GBrainTool {
    exposed_name: String,
    remote_name: String,
    description: String,
    input_schema: Value,
    service: Arc<Mutex<EmbeddedGBrain>>,
    call_timeout: Option<Duration>,
    healthy: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for GBrainTool {
    fn name(&self) -> &str {
        &self.exposed_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(&self, input: Value) -> ToolResult {
        if !self.healthy.load(Ordering::Relaxed) {
            return ToolResult::err(
                "gbrain runtime is unhealthy after a timed-out call; waiting for host recovery",
            );
        }
        let service = self.service.clone();
        let task_health = self.healthy.clone();
        let remote_name = self.remote_name.clone();
        let call = tokio::task::spawn_blocking(move || {
            let mut service = service.lock().map_err(|_| {
                task_health.store(false, Ordering::Relaxed);
                "gbrain tool service mutex is poisoned".to_string()
            })?;
            service
                .call_tool(&remote_name, input)
                .map_err(|error| error.to_string())
        });
        let result = if let Some(timeout) = self.call_timeout {
            match tokio::time::timeout(timeout, call).await {
                Ok(result) => result,
                Err(_) => {
                    self.healthy.store(false, Ordering::Relaxed);
                    return ToolResult::err(format!(
                        "gbrain tool call timed out after {} seconds",
                        timeout.as_secs()
                    ));
                }
            }
        } else {
            call.await
        };
        match result {
            Ok(Ok(value)) => ToolResult::ok(
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
            ),
            Ok(Err(error)) => ToolResult::err(error),
            Err(error) => {
                self.healthy.store(false, Ordering::Relaxed);
                ToolResult::err(format!("gbrain tool task failed: {error}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registers_and_executes_gbrain_without_child_process() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = Config {
            kb_worker_enabled: false,
            autopilot_enabled: false,
            artifact_manual_memory_to_kb: false,
            ..Config::default()
        };

        let tools = GBrainTools::open(
            temp.path().join("data").join("brain.db"),
            temp.path().join("workspace"),
            config,
        )
        .expect("open gbrain");
        let mut registry = ToolRegistry::new();
        assert!(tools.extend_registry(&mut registry) > 0);

        let result = registry
            .execute(
                "gbrain__artifact_put",
                serde_json::json!({
                    "slug": "wiki/android",
                    "content": "in-process gbrain"
                }),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("artifact_id"));
    }
}
