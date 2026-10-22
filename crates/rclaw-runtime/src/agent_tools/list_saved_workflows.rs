//! `ListSavedWorkflows`：列出项目 / 全局存档；项目同名覆盖全局时标记 shadowed。

use async_trait::async_trait;
use serde_json::{json, Value};

use rclaw_core::ToolAccess;
use rclaw_tools::{access, Tool, ToolResult};

use super::subagent::get_str;
use super::workflow_saved;
use super::workflow_service::WorkflowService;

pub struct ListSavedWorkflowsTool {
    service: WorkflowService,
}

impl ListSavedWorkflowsTool {
    pub fn new(service: WorkflowService) -> Self {
        ListSavedWorkflowsTool { service }
    }
}

#[async_trait]
impl Tool for ListSavedWorkflowsTool {
    fn name(&self) -> &str {
        "ListSavedWorkflows"
    }

    fn description(&self) -> &str {
        "List saved workflows. Project scope shadows global entries with the same name. Global \
         entries are only visible to unrestricted sessions."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": {"type": "string", "enum": ["project", "global"]}
            }
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let scope = get_str(&input, "scope");
        let access = access::current_access();
        if scope == Some("global") && access != ToolAccess::Unrestricted {
            return ToolResult::err(
                super::workflow_service::WorkflowError::new(
                    "forbidden_global",
                    "permission",
                    "global 存档仅 Unrestricted 会话可见",
                )
                .to_json()
                .to_string(),
            );
        }
        let mut project = Vec::new();
        let mut global = Vec::new();
        if scope != Some("global") {
            project = workflow_saved::list_saved(&self.service.inner.paths.saved_dir, "project");
        }
        if scope != Some("project") && access == ToolAccess::Unrestricted {
            global =
                workflow_saved::list_saved(&self.service.inner.paths.global_saved_dir, "global");
        }
        let project_names: std::collections::HashSet<String> = project
            .iter()
            .filter_map(|entry| {
                entry
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        for entry in &mut global {
            let shadowed = entry
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| project_names.contains(name));
            if let Some(object) = entry.as_object_mut() {
                object.insert("shadowed".to_string(), json!(shadowed));
            }
        }
        let mut workflows = project;
        workflows.extend(global);
        ToolResult::ok(json!({"workflows": workflows}).to_string())
    }
}
