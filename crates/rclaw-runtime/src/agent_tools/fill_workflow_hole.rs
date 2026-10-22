//! `FillWorkflowHole`：给运行中 hole 补函数体（预检 → CAS → 停旧 exec → 新快照 → 新 driver）。

use async_trait::async_trait;
use serde_json::{json, Value};

use rclaw_tools::{access, Tool, ToolResult};

use super::subagent::get_str;
use super::workflow_service::{self, WorkflowService};

pub struct FillWorkflowHoleTool {
    service: WorkflowService,
}

impl FillWorkflowHoleTool {
    pub fn new(service: WorkflowService) -> Self {
        FillWorkflowHoleTool { service }
    }
}

#[async_trait]
impl Tool for FillWorkflowHoleTool {
    fn name(&self) -> &str {
        "FillWorkflowHole"
    }

    fn description(&self) -> &str {
        "Fill a waiting workflow hole with a function body. Call only when a hole notification or \
         GetWorkflowRun.holes says the run is waiting. Tail, branch-level and nested holes are all \
         supported: the body replaces that hole(...) call expression and the run resumes with \
         prefix asks/effects replayed."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "run_id": {"type": "string"},
                "hole_id": {"type": "string", "description": "From hole notification / GetWorkflowRun.holes."},
                "expected_script_generation": {"type": "integer"},
                "script": {"type": "string", "description": "Function body statements only."},
                "path": {"type": "string", "description": "Project-relative fill file."}
            },
            "required": ["run_id", "hole_id", "expected_script_generation"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(run_id) = get_str(&input, "run_id")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return ToolResult::err("run_id is required");
        };
        let Some(hole_id) = get_str(&input, "hole_id")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return ToolResult::err("hole_id is required");
        };
        let Some(generation) = input
            .get("expected_script_generation")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
        else {
            return ToolResult::err("expected_script_generation is required");
        };
        let body = match (get_str(&input, "script"), get_str(&input, "path")) {
            (Some(_), Some(_)) => return ToolResult::err("script 与 path 互斥"),
            (Some(script), None) => script.to_string(),
            (None, Some(path)) => match self.service.read_script_file(path) {
                Ok((_, text)) => text,
                Err(error) => return workflow_service::error_result(error),
            },
            (None, None) => return ToolResult::err("script 或 path 必须给一个"),
        };
        if body.chars().count() > super::workflow::MAX_SCRIPT_CHARS {
            return workflow_service::error_result(workflow_service::WorkflowError::new(
                "script_too_large",
                "validation",
                format!("script 超过 {} 字符", super::workflow::MAX_SCRIPT_CHARS),
            ));
        }
        let (channel, chat_id) = crate::turn::rewind::current_chat()
            .unwrap_or_else(|| ("cli".to_string(), "console".to_string()));
        match self
            .service
            .clone()
            .fill_hole(
                run_id,
                hole_id,
                generation,
                body,
                channel,
                chat_id,
                access::current_access(),
                true,
            )
            .await
        {
            Ok(value) => ToolResult::ok(value.to_string()),
            Err(error) => workflow_service::error_result(error),
        }
    }
}
