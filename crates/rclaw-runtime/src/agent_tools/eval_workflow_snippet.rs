//! `EvalWorkflowSnippet`：同步跑片段（无 agent/report/artifact/hole/phase，不落 journal）。

use async_trait::async_trait;
use serde_json::{json, Value};

use rclaw_tools::{access, Tool, ToolResult};

use super::subagent::get_str;
use super::workflow_service::WorkflowService;

pub struct EvalWorkflowSnippetTool {
    service: WorkflowService,
}

impl EvalWorkflowSnippetTool {
    pub fn new(service: WorkflowService) -> Self {
        EvalWorkflowSnippetTool { service }
    }
}

#[async_trait]
impl Tool for EvalWorkflowSnippetTool {
    fn name(&self) -> &str {
        "EvalWorkflowSnippet"
    }

    fn description(&self) -> &str {
        "Run a small deterministic snippet through the same sandbox/compiler as a real workflow. \
         No agent()/report/artifact/hole/phase; nothing is journaled and no run is created. \
         files/git are available; world.run requires an unrestricted session."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {"type": "string"},
                "path": {"type": "string"},
                "timeout_ms": {"type": "integer", "minimum": 1000, "maximum": 600000, "description": "Default 60000."}
            }
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        // 路由矩阵（文档 §5.0）：EvalWorkflowSnippet 过技能门。
        if let Err(error) = self.service.check_skill_gate_for_current_session() {
            return super::workflow_service::error_result(error);
        }
        let code = match (get_str(&input, "code"), get_str(&input, "path")) {
            (Some(_), Some(_)) => return ToolResult::err("code 与 path 互斥"),
            (Some(code), None) => code.to_string(),
            (None, Some(path)) => match self.service.read_script_file(path) {
                Ok((_, text)) => text,
                Err(error) => return super::workflow_service::error_result(error),
            },
            (None, None) => return ToolResult::err("code 或 path 必须给一个"),
        };
        let timeout = input
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(60_000)
            .clamp(1_000, 600_000);
        match super::workflow::eval_snippet(
            code,
            std::time::Duration::from_millis(timeout),
            access::current_access(),
            self.service.inner.paths.project_root.clone(),
        )
        .await
        {
            Ok(value) => ToolResult::ok(value.to_string()),
            // §5.10：Sandboxed 会话触碰 world.run → 结构化 `forbidden_world`。
            Err(error) if error.starts_with("forbidden_world") => {
                super::workflow_service::error_result(super::workflow_service::WorkflowError::new(
                    "forbidden_world",
                    "permission",
                    error,
                ))
            }
            Err(error) => ToolResult::err(error),
        }
    }
}
