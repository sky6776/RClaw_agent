//! `CancelWorkflowRun`：取消 running/queued run；确认引擎退出后才写 stopped。

use async_trait::async_trait;
use serde_json::{json, Value};

use rclaw_tools::{access, Tool, ToolResult};

use super::subagent::get_str;
use super::workflow_service::{self, WorkflowService};

pub struct CancelWorkflowRunTool {
    service: WorkflowService,
}

impl CancelWorkflowRunTool {
    pub fn new(service: WorkflowService) -> Self {
        CancelWorkflowRunTool { service }
    }
}

#[async_trait]
impl Tool for CancelWorkflowRunTool {
    fn name(&self) -> &str {
        "CancelWorkflowRun"
    }

    fn description(&self) -> &str {
        "Cancel a running or queued workflow run. A queued run stops immediately; a running run \
         enters stopping and only becomes stopped after the engine (and sub-agents) confirm exit. \
         `reason` defaults to `model` (UI uses `user`)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "run_id": {"type": "string"},
                "reason": {"type": "string", "enum": ["user", "model"], "description": "Default model."}
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
        // 模型工具不能声明 user 来源（否则可伪造“用户取消”并绕过 fresh-auth 语义）。
        // UI 经 workflow_api 走 user 路径。
        let _ = get_str(&input, "reason");
        let reason = "model";
        let (channel, chat_id) = crate::turn::rewind::current_chat()
            .unwrap_or_else(|| ("cli".to_string(), "console".to_string()));
        match self
            .service
            .clone()
            .cancel(run_id, reason, channel, chat_id, access::current_access())
            .await
        {
            Ok(value) => ToolResult::ok(value.to_string()),
            Err(error) => workflow_service::error_result(error),
        }
    }
}
