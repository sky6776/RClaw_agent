//! `ListWorkflowRuns`：按项目根枚举（含跨会话），逐行按权限快照过滤。

use async_trait::async_trait;
use serde_json::{json, Value};

use rclaw_tools::{access, Tool, ToolResult};

use super::workflow_service::{self, WorkflowService};

pub struct ListWorkflowRunsTool {
    service: WorkflowService,
}

impl ListWorkflowRunsTool {
    pub fn new(service: WorkflowService) -> Self {
        ListWorkflowRunsTool { service }
    }
}

#[async_trait]
impl Tool for ListWorkflowRunsTool {
    fn name(&self) -> &str {
        "ListWorkflowRuns"
    }

    fn description(&self) -> &str {
        "List workflow runs in this project (all sessions share project_root = Layout.base). \
         Read-only. Rows above the caller's ToolAccess are filtered out; legacy runs (cwd='') \
         only appear for unrestricted callers with include_legacy=true."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Default 20."},
                "statuses": {"type": "array", "items": {"type": "string"}},
                "include_legacy": {"type": "boolean", "description": "Default false; only unrestricted callers can enable."}
            }
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value.clamp(1, 50) as usize)
            .unwrap_or(20);
        let statuses = input
            .get("statuses")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let include_legacy = input
            .get("include_legacy")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let (channel, chat_id) = crate::turn::rewind::current_chat()
            .unwrap_or_else(|| ("cli".to_string(), "console".to_string()));
        match self.service.list(
            limit,
            statuses,
            include_legacy,
            &channel,
            &chat_id,
            access::current_access(),
        ) {
            Ok(value) => ToolResult::ok(value.to_string()),
            Err(error) => workflow_service::error_result(error),
        }
    }
}
