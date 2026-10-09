//! `memory` 工具：结构化记忆操作。
//!
//! 一个工具、一个 `action` 枚举（add/replace/remove/list）、一个 `target`
//! （memory=MEMORY.md / user=USER.md）。replace/remove 用 `old_text` 子串定位单条
//! bullet；多义/未命中/漂移由 store 层拒绝并把当前条目回显给模型自纠。所有写入
//! 都过威胁扫描 + 脱敏 + 总字符上限（见 [`rclaw_store::MemoryStore`]）。
//!
//! 与既有「自动 review 追加」互补：自动 review 是后台被动提炼；本工具让模型在
//! turn 中主动、精确地维护记忆（读后改、去重、纠正过期条目）。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rclaw_store::{MemOpResult, MemoryBatchOp, MemoryStore, MemoryTarget};
use serde_json::{json, Value};

use crate::{turn_state, Tool, ToolResult};

/// 同一 turn 内允许的「整合失败」（越限 / 未命中 / 漂移等 store 层写入失败）次数上限。
///
/// 超过后返回 terminal
/// `done=true`，阻止模型在本 turn 反复修内存、把用户回复挤掉（issue #42405）。
const MAX_CONSOLIDATION_FAILURES_PER_TURN: u32 = 3;

/// 结构化记忆工具。
pub struct MemoryTool {
    store: MemoryStore,
    /// 未处于 turn task-local scope 时使用的实例级回退计数。正常顶层 turn 和 sub-agent
    /// 通过 [`turn_state`] 隔离计数；独立工具调用与 review fork 仍可沿用此回退。
    failures: Arc<AtomicU32>,
}

impl MemoryTool {
    pub fn new(store: MemoryStore) -> Self {
        MemoryTool {
            store,
            failures: Arc::new(AtomicU32::new(0)),
        }
    }

    /// 本次调用应操作的 store：Bot Mode 的 bot 会话经 task-local 路由到
    /// bot 自己的 MemoryStore（ADR-0006）；其余回落构造期绑定的默认 store。
    fn effective_store(&self) -> MemoryStore {
        turn_state::persona_memory().unwrap_or_else(|| self.store.clone())
    }

    /// 记一次 store 层整合失败（越限 / 未命中 / 漂移等），返回应回送给模型的 ToolResult。
    ///
    /// 配额内：照常回显错误（含 live entries）供模型自纠重试。超配额：返回 terminal
    /// `done=true`（success=false，但 is_error=false——这是「停止重试」信号，不是新错误），
    /// 让模型停止循环 memory 调用、继续回复用户。
    fn record_consolidation_failure(&self, err: impl std::fmt::Display) -> ToolResult {
        let n = turn_state::record_memory_failure()
            .unwrap_or_else(|| self.failures.fetch_add(1, Ordering::SeqCst) + 1);
        if n <= MAX_CONSOLIDATION_FAILURES_PER_TURN {
            ToolResult::err(err.to_string())
        } else {
            ToolResult::ok(
                json!({
                    "success": false,
                    "done": true,
                    "error": format!(
                        "Memory consolidation failed {n} times this turn. Stop retrying memory \
                         calls — leave memory unchanged for now and continue with your reply to \
                         the user. The fact can be saved in a later turn."
                    ),
                })
                .to_string(),
            )
        }
    }

    /// 解析 `operations` 数组并交给 store 原子批量应用。
    fn execute_batch(&self, target: MemoryTarget, ops: &Value) -> ToolResult {
        let store = self.effective_store();
        let arr = match ops.as_array() {
            Some(a) => a,
            None => {
                return ToolResult::err(
                    "operations must be a list of {action, content?, old_text?} objects"
                        .to_string(),
                )
            }
        };
        if arr.is_empty() {
            return ToolResult::err("operations list is empty".to_string());
        }
        let mut plan: Vec<MemoryBatchOp> = Vec::with_capacity(arr.len());
        for (i, item) in arr.iter().enumerate() {
            let action = item
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if action.is_empty() {
                return ToolResult::err(format!(
                    "operations[{i}]: missing action (add|replace|remove)"
                ));
            }
            let content = item
                .get("content")
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            let old_text = item
                .get("old_text")
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            plan.push(MemoryBatchOp {
                action,
                content,
                old_text,
            });
        }
        match store.apply_batch(target, &plan) {
            Ok(r) => ToolResult::ok(
                json!({
                    "success": true,
                    "done": true,
                    "target": target_name(target),
                    "usage": r.usage,
                    "entry_count": r.entry_count,
                    "changed": r.changed,
                    "message": r.message,
                    "note": "Batch applied. This update is complete — do not repeat it."
                })
                .to_string(),
            ),
            Err(e) => self.record_consolidation_failure(e),
        }
    }
}

fn parse_target(v: &str) -> Option<MemoryTarget> {
    match v {
        "memory" | "" => Some(MemoryTarget::Memory),
        "user" => Some(MemoryTarget::User),
        _ => None,
    }
}

fn target_name(t: MemoryTarget) -> &'static str {
    match t {
        MemoryTarget::Memory => "memory",
        MemoryTarget::User => "user",
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Manage persistent memory (MEMORY.md / USER.md) with structured operations. \
         Make ALL changes in ONE call via an `operations` array (each item: {action, content?, old_text?}). \
         The batch applies ATOMICALLY and the char limit is checked only on the FINAL result — so a single \
         call can remove/replace stale entries to free room AND add new ones, even when an add alone would \
         overflow. If any op is malformed / unmatched / ambiguous, or the net result exceeds the limit, \
         NOTHING is written and current_entries are returned for self-correction. \
         Single-op shape (action=add|replace|remove|list + content/old_text) is fine for a lone change. \
         action=list reads current entries — call it before editing when unsure. \
         target=memory (durable facts/preferences) | user (user profile)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "replace", "remove", "list"],
                    "description": "Single-op action. Omit when using `operations` (batch)."
                },
                "target": {
                    "type": "string",
                    "enum": ["memory", "user"],
                    "default": "memory",
                    "description": "Which store: memory (durable facts) or user (user profile)."
                },
                "content": {
                    "type": "string",
                    "description": "Entry text. Required for add/replace (single-op). One concise bullet (a single fact), not a paragraph."
                },
                "old_text": {
                    "type": "string",
                    "description": "A unique substring identifying the existing entry. Required for replace/remove (single-op)."
                },
                "operations": {
                    "type": "array",
                    "description": "Batch shape: a list of ops applied atomically in one call against the final char budget. Preferred for multiple changes or consolidating to make room. Each item is {action, content?, old_text?}.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "action": {"type": "string", "enum": ["add", "replace", "remove"]},
                            "content": {"type": "string", "description": "Entry content for add/replace."},
                            "old_text": {"type": "string", "description": "Substring identifying the entry for replace/remove."}
                        },
                        "required": ["action"]
                    }
                }
            },
            "required": ["target"]
        })
    }

    /// turn 开始前重置整合失败计数。
    fn reset_turn_state(&self) {
        self.failures.store(0, Ordering::SeqCst);
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let target_raw = input
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or("memory");
        let target = match parse_target(target_raw) {
            Some(t) => t,
            None => return ToolResult::err(format!("unknown target: {target_raw}")),
        };
        let store = self.effective_store();

        // list：直接回显条目，供模型 read-before-write。
        if action == "list" {
            return match store.list_entries(target) {
                Ok(entries) => ToolResult::ok(
                    json!({
                        "target": target_name(target),
                        "entry_count": entries.len(),
                        "entries": entries,
                    })
                    .to_string(),
                ),
                Err(e) => ToolResult::err(e.to_string()),
            };
        }

        // batch 路径：operations 数组，原子应用 + 最终预算校验。
        if let Some(ops) = input.get("operations") {
            return self.execute_batch(target, ops);
        }

        let content = input
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let old_text = input
            .get("old_text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let result: std::result::Result<MemOpResult, _> = match action {
            "add" => {
                if content.trim().is_empty() {
                    return ToolResult::err("add requires content");
                }
                store.add_entry(target, &content)
            }
            "replace" => {
                if old_text.trim().is_empty() {
                    return ToolResult::err(
                        "replace requires old_text (unique substring of an existing entry)",
                    );
                }
                if content.trim().is_empty() {
                    return ToolResult::err("replace requires content");
                }
                store.replace_entry(target, &old_text, &content)
            }
            "remove" => {
                if old_text.trim().is_empty() {
                    return ToolResult::err(
                        "remove requires old_text (unique substring of an existing entry)",
                    );
                }
                store.remove_entry(target, &old_text)
            }
            other => return ToolResult::err(format!("unknown action: {other}")),
        };

        match result {
            Ok(r) => ToolResult::ok(
                json!({
                    "success": true,
                    "done": true,
                    "target": target_name(target),
                    "usage": r.usage,
                    "entry_count": r.entry_count,
                    "changed": r.changed,
                    "message": r.message,
                    "note": "Write saved. This update is complete — do not repeat it. Read context is injected from a frozen snapshot at session start."
                })
                .to_string(),
            ),
            Err(e) => self.record_consolidation_failure(e),
        }
    }
}
