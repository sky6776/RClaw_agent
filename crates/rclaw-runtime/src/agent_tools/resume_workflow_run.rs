//! `ResumeWorkflowRun`：仅 `stopped` 且未被取代的 run 可续；同 run id、exec_generation+1。

use async_trait::async_trait;
use serde_json::{json, Value};

use rclaw_tools::{access, Tool, ToolResult};

use super::subagent::get_str;
use super::workflow_service::{self, WorkflowService};

pub struct ResumeWorkflowRunTool {
    service: WorkflowService,
}

impl ResumeWorkflowRunTool {
    pub fn new(service: WorkflowService) -> Self {
        ResumeWorkflowRunTool { service }
    }
}

#[async_trait]
impl Tool for ResumeWorkflowRunTool {
    fn name(&self) -> &str {
        "ResumeWorkflowRun"
    }

    fn description(&self) -> &str {
        "Resume a stopped workflow run. Only `stopped` runs whose superseded_by is null can be \
         resumed; errored runs must be amended. A run cancelled by the user requires a NEW \
         explicit user request (and auth_token when fresh authorization is required). Unknown \
         effects block resume until the UI confirms them."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "run_id": {"type": "string"},
                "request_id": {"type": "string"},
                "auth_token": {"type": "string"}
            },
            "required": ["run_id"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(run_id) = get_str(&input, "run_id")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return ToolResult::err("run_id is required");
        };
        let (channel, chat_id) = crate::turn::rewind::current_chat()
            .unwrap_or_else(|| ("cli".to_string(), "console".to_string()));
        match self
            .service
            .clone()
            .resume(
                run_id,
                get_str(&input, "request_id").map(str::to_string),
                get_str(&input, "auth_token").map(str::to_string),
                channel,
                chat_id,
                access::current_access(),
            )
            .await
        {
            Ok(value) => workflow_service::run_result(value),
            Err(error) => workflow_service::error_result(error),
        }
    }
}
