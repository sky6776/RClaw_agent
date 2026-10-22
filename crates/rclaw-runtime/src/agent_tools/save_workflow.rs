//! `SaveWorkflow`：把脚本 + 元数据保存到项目 / 全局存档（原子替换、hash 防并发）。

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::{json, Value};

use rclaw_core::ToolAccess;
use rclaw_tools::{access, Tool, ToolResult};

use super::subagent::get_str;
use super::workflow_saved::{self, ArgDecl, SavedMeta};
use super::workflow_service::{self, WorkflowService};

pub struct SaveWorkflowTool {
    service: WorkflowService,
}

impl SaveWorkflowTool {
    pub fn new(service: WorkflowService) -> Self {
        SaveWorkflowTool { service }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

#[async_trait]
impl Tool for SaveWorkflowTool {
    fn name(&self) -> &str {
        "SaveWorkflow"
    }

    fn description(&self) -> &str {
        "Save a workflow script + metadata. NEVER call this tool unsolicited: saving writes a \
         file into the user's project (or global archive). Overwriting an existing name requires \
         overwrite=true (and expected_hash when racing another writer)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "[A-Za-z0-9._-]+, <=64."},
                "description": {"type": "string"},
                "when_to_use": {"type": "string"},
                "scope": {"type": "string", "enum": ["project", "global"]},
                "script": {"type": "string"},
                "script_path": {"type": "string"},
                "overwrite": {"type": "boolean"},
                "expected_hash": {"type": "string", "description": "SHA-256 of the current file."},
                "args": {"type": "object", "description": "Argument declarations: {name: {type, required?, default?}}."}
            },
            "required": ["name", "description", "when_to_use", "scope"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(name) = get_str(&input, "name")
            .map(str::trim)
            .filter(|value| valid_name(value))
        else {
            return ToolResult::err("name 必须匹配 [A-Za-z0-9._-]+ 且 <=64");
        };
        let Some(description) = get_str(&input, "description") else {
            return ToolResult::err("description is required");
        };
        let Some(when_to_use) = get_str(&input, "when_to_use") else {
            return ToolResult::err("when_to_use is required");
        };
        // §5.8：scope 必填、无默认（project / global 落点不同，不能替模型猜）。
        let Some(scope) =
            get_str(&input, "scope").filter(|scope| matches!(*scope, "project" | "global"))
        else {
            return ToolResult::err("scope 必填，且只能是 project / global");
        };
        // 路由矩阵（文档 §5.0）：SaveWorkflow 过技能门。
        if let Err(error) = self.service.check_skill_gate_for_current_session() {
            return workflow_service::error_result(error);
        }
        let tool_access = access::current_access();
        if scope == "global" && tool_access != ToolAccess::Unrestricted {
            return ToolResult::err(
                workflow_service::WorkflowError::new(
                    "forbidden_global",
                    "permission",
                    "global 存档仅 Unrestricted 会话可用",
                )
                .to_json()
                .to_string(),
            );
        }
        let script = match (get_str(&input, "script"), get_str(&input, "script_path")) {
            (Some(_), Some(_)) => return ToolResult::err("script 与 script_path 互斥"),
            (Some(script), None) => script.to_string(),
            (None, Some(path)) => match self.service.read_script_file(path) {
                Ok((_, text)) => text,
                Err(error) => return workflow_service::error_result(error),
            },
            (None, None) => return ToolResult::err("script 或 script_path 必须给一个"),
        };
        if script.chars().count() > super::workflow::MAX_SCRIPT_CHARS {
            return workflow_service::error_result(workflow_service::WorkflowError::new(
                "script_too_large",
                "validation",
                format!("script 超过 {} 字符", super::workflow::MAX_SCRIPT_CHARS),
            ));
        }
        // §5.8：保存前跑与 Create 默认同一口径的语法 / type_check（tsc 不在位时静默降级）。
        if let Err(error) = super::workflow::prepare_script(&script, true).await {
            return workflow_service::error_result(workflow_service::WorkflowError::new(
                "script_diagnostics",
                "validation",
                error,
            ));
        }
        let args: BTreeMap<String, ArgDecl> = match input.get("args") {
            None | Some(Value::Null) => BTreeMap::new(),
            Some(value) => match serde_json::from_value(value.clone()) {
                Ok(args) => args,
                Err(error) => {
                    return ToolResult::err(format!("args 声明解析失败: {error}"));
                }
            },
        };
        if let Err(error) = workflow_saved::validate_args_decl(&args) {
            return ToolResult::err(
                workflow_service::WorkflowError::new("args_decl_invalid", "validation", error)
                    .to_json()
                    .to_string(),
            );
        }
        let meta = SavedMeta {
            name: name.to_string(),
            description: description.to_string(),
            when_to_use: when_to_use.to_string(),
            args,
        };
        let dir = if scope == "global" {
            self.service.inner.paths.global_saved_dir.clone()
        } else {
            self.service.inner.paths.saved_dir.clone()
        };
        let path = dir.join(format!("{name}.dwf.js"));
        // 同名保存串行化（跨进程文件锁）：存在性 / hash 校验与替换在同一临界区内。
        let lock_path = dir.join(format!(".{name}.lock"));
        if let Err(error) = std::fs::create_dir_all(&dir) {
            return ToolResult::err(format!("创建存档目录失败: {error}"));
        }
        let lock_file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(error) => return ToolResult::err(format!("打开存档锁失败: {error}")),
        };
        if let Err(error) = fs2::FileExt::lock_exclusive(&lock_file) {
            return ToolResult::err(format!("锁定存档失败: {error}"));
        }
        let existed = path.exists();
        let overwrite = input
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if existed {
            if !overwrite {
                return ToolResult::err(
                    workflow_service::WorkflowError::new(
                        "save_conflict",
                        "state",
                        "同名存档已存在；覆盖需要 overwrite=true",
                    )
                    .to_json()
                    .to_string(),
                );
            }
            if let Some(expected) = get_str(&input, "expected_hash") {
                let current = super::workflow_skill::file_hash(&path).unwrap_or_default();
                if current != expected {
                    return ToolResult::err(
                        workflow_service::WorkflowError::new(
                            "save_conflict",
                            "state",
                            "expected_hash 与当前文件不符",
                        )
                        .to_json()
                        .to_string(),
                    );
                }
            }
        } else if get_str(&input, "expected_hash").is_some() {
            return ToolResult::err("expected_hash 只用于覆盖已存在文件");
        }
        match workflow_saved::write_saved(&dir, &meta, &script) {
            Ok((path, written)) => ToolResult::ok(
                json!({
                    "status": "saved",
                    "path": path.to_string_lossy(),
                    "scope": scope,
                    // §5.8「无变更不写」：内容与现有文件一致时不落盘，也不算覆盖。
                    "overwritten": existed && written,
                    "unchanged": existed && !written,
                })
                .to_string(),
            ),
            Err(error) => ToolResult::err(error),
        }
    }
}
