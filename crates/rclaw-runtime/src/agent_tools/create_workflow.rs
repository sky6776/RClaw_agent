//! `CreateWorkflow`：校验脚本、落不可变快照、预留配额并启动后台 run。
//!
//! `WorkflowRun` 是 deprecated 别名（P0 兼容层，`workflow_legacy_tool` 开关控制注册）；
//! 接受旧 `resume_run_id` / `amend_run_id` 参数，行为映射到新的三工具。

use async_trait::async_trait;
use serde_json::{json, Value};

use rclaw_tools::{access, Tool, ToolResult};

use super::subagent::get_str;
use super::workflow_service::{self, CreateInput, SavedRef, WorkflowService};

fn current_session() -> (String, String) {
    crate::turn::rewind::current_chat()
        .unwrap_or_else(|| ("cli".to_string(), "console".to_string()))
}

fn parse_saved(input: &Value) -> Result<Option<SavedRef>, String> {
    let Some(saved) = input.get("saved").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(name) = saved
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
    else {
        return Err("saved.name is required".to_string());
    };
    let args = saved.get("args").cloned();
    let scope = saved
        .get("scope")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Some(SavedRef {
        name: name.to_string(),
        args,
        scope,
    }))
}

fn parse_u32(input: &Value, key: &str) -> Result<Option<u32>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| format!("{key} 必须是整数")),
    }
}

pub struct CreateWorkflowTool {
    service: WorkflowService,
}

impl CreateWorkflowTool {
    pub fn new(service: WorkflowService) -> Self {
        CreateWorkflowTool { service }
    }
}

#[async_trait]
impl Tool for CreateWorkflowTool {
    fn name(&self) -> &str {
        "CreateWorkflow"
    }

    fn description(&self) -> &str {
        "Start a DETERMINISTIC JS workflow that orchestrates sub-agents and world-read \
         operations. When to use: only when the user explicitly asks for a workflow — \
         \"use a workflow\", \"with a workflow\", \"使用 workflow\", \"用工作流\", or any phrasing \
         that names workflow/工作流 as the means: this tool is mandatory. Do not substitute the \
         Agent tool and do not do the work inline. Without such an explicit request, do not \
         start a workflow: delegate with the Agent tool or do the work yourself, even for \
         multi-step or multi-subagent tasks. Read the dynamic-workflows skill first. Returns \
         {status:\"backgrounded\", run_id}; inspect with GetWorkflowRun."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Short run label (default 'workflow')."},
                "script": {"type": "string", "description": "Deterministic JS orchestration script (<=100k chars). One of script / path / saved."},
                "path": {"type": "string", "description": "Project-relative path to a script; read once and snapshotted before execution."},
                "saved": {
                    "type": "object",
                    "description": "Run a saved workflow (P2). Exempt from the skill gate.",
                    "properties": {
                        "name": {"type": "string"},
                        "args": {"type": "object"},
                        "scope": {"type": "string", "enum": ["project", "global"]}
                    },
                    "required": ["name"]
                },
                "args": {"type": "object", "description": "Arguments for the `path` source only; saved uses saved.args."},
                "max_parallel_asks": {"type": "integer", "minimum": 1, "maximum": 16, "description": "Concurrency cap for asks (default 8)."},
                "max_concurrency": {"type": "integer", "minimum": 1, "maximum": 16, "description": "Alias for max_parallel_asks."},
                "subagent_model": {"type": "string", "description": "Default subagent model (must be in allowed_models)."},
                "max_run_secs": {"type": "integer", "minimum": 1, "maximum": 86400, "description": "Wall-clock budget across generations (default 3600)."},
                "type_check": {"type": "boolean", "description": "Run the optional tsc facade check (default true)."},
                "request_id": {"type": "string", "description": "Idempotency key (<=128 chars)."}
            }
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let (channel, chat_id) = current_session();
        let saved = match parse_saved(&input) {
            Ok(saved) => saved,
            Err(error) => return ToolResult::err(error),
        };
        let max_parallel_asks = match parse_u32(&input, "max_parallel_asks") {
            Ok(value) => value,
            Err(error) => return ToolResult::err(error),
        };
        let max_concurrency = match parse_u32(&input, "max_concurrency") {
            Ok(value) => value,
            Err(error) => return ToolResult::err(error),
        };
        let request = CreateInput {
            name: get_str(&input, "name").map(str::to_string),
            script: get_str(&input, "script").map(str::to_string),
            path: get_str(&input, "path").map(str::to_string),
            saved,
            args: input.get("args").filter(|value| !value.is_null()).cloned(),
            max_parallel_asks,
            max_concurrency,
            subagent_model: get_str(&input, "subagent_model").map(str::to_string),
            max_run_secs: input.get("max_run_secs").and_then(Value::as_u64),
            type_check: input.get("type_check").and_then(Value::as_bool),
            request_id: get_str(&input, "request_id").map(str::to_string),
            channel,
            chat_id,
            tool_access: access::current_access(),
        };
        match self.service.clone().create(request).await {
            Ok(response) => workflow_service::run_result(response),
            Err(error) => workflow_service::error_result(error),
        }
    }
}

/// deprecated 别名：旧 `WorkflowRun` 调用（P0 兼容层，默认不注册）。
pub struct WorkflowRunTool {
    service: WorkflowService,
}

impl WorkflowRunTool {
    pub fn new(service: WorkflowService) -> Self {
        WorkflowRunTool { service }
    }
}

#[async_trait]
impl Tool for WorkflowRunTool {
    fn name(&self) -> &str {
        "WorkflowRun"
    }

    fn description(&self) -> &str {
        "DEPRECATED: use CreateWorkflow / AmendWorkflow / ResumeWorkflowRun. Kept for tool-surface \
         rollback only (workflow_legacy_tool=true). Accepts the old resume_run_id / amend_run_id."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "script": {"type": "string", "description": "Script (or the original script when resuming)."},
                "resume_run_id": {"type": "string"},
                "amend_run_id": {"type": "string"},
                "max_parallel_asks": {"type": "integer", "minimum": 1, "maximum": 16},
                "max_run_secs": {"type": "integer", "minimum": 1, "maximum": 86400},
                "type_check": {"type": "boolean"}
            }
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let (channel, chat_id) = current_session();
        let access = access::current_access();
        let resume = get_str(&input, "resume_run_id")
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let amend = get_str(&input, "amend_run_id")
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if resume.is_some() && amend.is_some() {
            return ToolResult::err("resume_run_id 与 amend_run_id 互斥");
        }
        if let Some(run_id) = resume {
            return match self
                .service
                .clone()
                .resume(run_id, None, None, channel, chat_id, access)
                .await
            {
                Ok(value) => workflow_service::run_result(value),
                Err(error) => workflow_service::error_result(error),
            };
        }
        if let Some(run_id) = amend {
            let script = get_str(&input, "script").map(str::to_string);
            return match self
                .service
                .clone()
                .amend(
                    run_id,
                    script,
                    None,
                    None,
                    None,
                    None,
                    input.get("type_check").and_then(Value::as_bool),
                    None,
                    None,
                    channel,
                    chat_id,
                    access,
                )
                .await
            {
                Ok(value) => workflow_service::run_result(value),
                Err(error) => workflow_service::error_result(error),
            };
        }
        let Some(script) = get_str(&input, "script").map(str::to_string) else {
            return ToolResult::err("script is required for WorkflowRun");
        };
        let request = CreateInput {
            name: get_str(&input, "name").map(str::to_string),
            script: Some(script),
            path: None,
            saved: None,
            args: None,
            max_parallel_asks: input
                .get("max_parallel_asks")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
            max_concurrency: None,
            subagent_model: None,
            max_run_secs: input.get("max_run_secs").and_then(Value::as_u64),
            type_check: input.get("type_check").and_then(Value::as_bool),
            request_id: None,
            channel,
            chat_id,
            tool_access: access,
        };
        match self.service.clone().create(request).await {
            Ok(response) => workflow_service::run_result(response),
            Err(error) => workflow_service::error_result(error),
        }
    }
}
