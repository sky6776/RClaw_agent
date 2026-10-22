//! `GetWorkflowRun`：单 run 的瞬时快照（进度 / log 尾 / phases / reports / artifacts /
//! holes / pending questions / result / error / effects）。

use async_trait::async_trait;
use serde_json::{json, Value};

use rclaw_tools::{access, Tool, ToolResult};

use super::subagent::get_str;
use super::workflow_service::{self, GetOptions, WorkflowService};

pub struct GetWorkflowRunTool {
    service: WorkflowService,
}

impl GetWorkflowRunTool {
    pub fn new(service: WorkflowService) -> Self {
        GetWorkflowRunTool { service }
    }
}

#[async_trait]
impl Tool for GetWorkflowRunTool {
    fn name(&self) -> &str {
        "GetWorkflowRun"
    }

    fn description(&self) -> &str {
        "Read a workflow run snapshot: status, progress, ask stats, log tail, phases, reports, \
         artifact metadata, waiting holes, pending questions, result/error, recovery actions, \
         notifications and (optionally) effects. Does not wait."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "run_id": {"type": "string"},
                "include_legacy": {"type": "boolean", "description": "Default false; true only effective for unrestricted callers."},
                "include_effects": {"type": "boolean", "description": "Include effect rows for UI adjudication."},
                "reports_cursor": {"type": "integer", "description": "Next-page cursor returned by a previous call (reports are 50/page)."},
                "effects_cursor": {"type": "integer", "description": "Next-page cursor for effects (50/page)."},
                "artifacts_cursor": {"type": "integer", "description": "Next-page cursor for artifact metadata."},
                "log_cursor": {"type": "integer", "description": "Cursor returned as log_cursor to page further back in the log tail."}
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
        let cursor = |key: &str| {
            input
                .get(key)
                .and_then(Value::as_u64)
                .map(|value| value.min(usize::MAX as u64) as usize)
        };
        let options = GetOptions {
            include_legacy: input
                .get("include_legacy")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            include_effects: input
                .get("include_effects")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            reports_cursor: cursor("reports_cursor").unwrap_or(0),
            effects_cursor: cursor("effects_cursor").unwrap_or(0),
            artifacts_cursor: cursor("artifacts_cursor").unwrap_or(0),
            log_cursor: cursor("log_cursor"),
        };
        match self.service.get(
            run_id,
            options,
            &channel,
            &chat_id,
            access::current_access(),
        ) {
            Ok(value) => ToolResult::ok(value.to_string()),
            Err(error) => workflow_service::error_result(error),
        }
    }
}
