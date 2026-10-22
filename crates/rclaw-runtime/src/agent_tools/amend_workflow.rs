//! `AmendWorkflow`：修订脚本 / 改名 / 运行中调参（retune）/ 后继重建。

use async_trait::async_trait;
use serde_json::{json, Value};

use rclaw_tools::{access, Tool, ToolResult};

use super::subagent::get_str;
use super::workflow_service::{self, WorkflowService};

fn current_session() -> (String, String) {
    crate::turn::rewind::current_chat()
        .unwrap_or_else(|| ("cli".to_string(), "console".to_string()))
}

fn nullable_u32(input: &Value, key: &str) -> Result<Option<Option<u32>>, String> {
    match input.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(value) => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(|value| Some(Some(value)))
            .ok_or_else(|| format!("{key} 必须是整数或 null")),
    }
}

fn nullable_string(input: &Value, key: &str) -> Result<Option<Option<String>>, String> {
    match input.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(value) => value
            .as_str()
            .map(|value| Some(Some(value.to_string())))
            .ok_or_else(|| format!("{key} 必须是字符串或 null")),
    }
}

pub struct AmendWorkflowTool {
    service: WorkflowService,
}

impl AmendWorkflowTool {
    pub fn new(service: WorkflowService) -> Self {
        AmendWorkflowTool { service }
    }
}

#[async_trait]
impl Tool for AmendWorkflowTool {
    fn name(&self) -> &str {
        "AmendWorkflow"
    }

    fn description(&self) -> &str {
        "Revise a workflow run without stopping it: provide a new script/path (successor run, \
         completed asks imported), or only max_parallel_asks / subagent_model to retune a \
         RUNNING run in place. Do not cancel first and do not rewrite from scratch. A run with \
         write effects requires UI confirmation before a script amendment."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "run_id": {"type": "string"},
                "script": {"type": "string"},
                "path": {"type": "string"},
                "name": {"type": "string"},
                "max_parallel_asks": {"type": ["integer", "null"], "minimum": 1, "maximum": 16, "description": "null clears the run-level setting."},
                "subagent_model": {"type": ["string", "null"], "description": "null falls back to the session model."},
                "type_check": {"type": "boolean"},
                "request_id": {"type": "string"},
                "auth_token": {"type": "string", "description": "Required when the run needs fresh authorization."}
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
        let max_parallel_asks = match nullable_u32(&input, "max_parallel_asks") {
            Ok(value) => value,
            Err(error) => return ToolResult::err(error),
        };
        let subagent_model = match nullable_string(&input, "subagent_model") {
            Ok(value) => value,
            Err(error) => return ToolResult::err(error),
        };
        let (channel, chat_id) = current_session();
        match self
            .service
            .clone()
            .amend(
                run_id,
                get_str(&input, "script").map(str::to_string),
                get_str(&input, "path").map(str::to_string),
                get_str(&input, "name").map(str::to_string),
                max_parallel_asks,
                subagent_model,
                input.get("type_check").and_then(Value::as_bool),
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
