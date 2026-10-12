//! `escalate` / `ResolveWorkflowQuestion`：计划步骤与编排者之间的提问通道
//! （ZCode 长程能力移植 · Layer 3 最小内核第一步）。
//!
//! ZCode 动态工作流里子代理被卡住时可以「无限时阻塞着向主代理提问而不是编造答案」。
//! RClaw 的 PlanRun 前台运行期间编排者（父 turn）被工具调用阻塞、无法实时应答，因此
//! v1 采用【journal 协议】：
//!
//! 1. 计划步骤内调用 `escalate` → 追加 `step_escalated` 事件到 `agent/plans/<plan_id>.jsonl`
//!    并登记待应答问题；`plan_escalation_wait_secs` > 0 时阻塞等待实时答案（为将来的
//!    后台化编排者预留），默认 0 = 提交即挂起，步骤立即以结构化 handoff 收尾。
//! 2. 编排者调用 `ResolveWorkflowQuestion {question_id, answer}` → 追加
//!    `escalation_resolved` 事件；若步骤仍在等待（进程内）则实时解锁。
//! 3. `PlanRun(resume_plan_id=…)` 重放：已解答的升级注入「编排者答案」重跑该步骤，
//!    未决步骤跳过并提示先应答。
//!
//! 防注入纪律：答案进入步骤任务文本时显式标注为「编排者输入」，与上游结果同源。

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::plan::append_plan_event;
use super::subagent::get_str;
use super::team::{current_plan_context, next_execution_id};
use super::*;

/// question_id 形如 `<plan_id>:<step_id>:q-<ts>-<n>`：冒号分隔便于从 question_id
/// 反查计划 journal（plan_id / step_id 本身不含冒号——均经 sanitize）。
fn make_question_id(plan_id: &str, step_id: &str) -> String {
    format!("{plan_id}:{step_id}:{}", next_execution_id("q"))
}

fn plan_log_path(runtime: &AgentToolRuntime, plan_id: &str) -> std::path::PathBuf {
    runtime.layout.base.join("plans").join(format!("{plan_id}.jsonl"))
}

const MAX_QUESTION_CHARS: usize = 2_000;
const MAX_ANSWER_CHARS: usize = 4_000;

/// 计划步骤的升级提问工具（子代理侧）。
pub struct EscalateTool {
    runtime: AgentToolRuntime,
}

impl EscalateTool {
    pub fn new(runtime: AgentToolRuntime) -> Self {
        EscalateTool { runtime }
    }
}

#[async_trait]
impl Tool for EscalateTool {
    fn name(&self) -> &str {
        "escalate"
    }

    fn description(&self) -> &str {
        "Ask the orchestrator a question you cannot resolve yourself (missing credentials, \
         ambiguous requirement, contradictory upstream results, missing file/state). Only \
         usable inside a PlanRun step. Do NOT fabricate an answer and do NOT guess: submit \
         the question, then END YOUR TURN with a structured handoff that states what was \
         completed and what is pending on the answer. The orchestrator answers via \
         ResolveWorkflowQuestion and resumes the plan; your step re-runs with the answer \
         injected. Read the answer critically when it arrives."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {"type": "string", "description": "Specific, self-contained question (<=2000 chars)."},
                "context": {"type": "string", "description": "Optional supporting context/evidence for the orchestrator."}
            },
            "required": ["question"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(question) = get_str(&input, "question")
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_string)
        else {
            return ToolResult::err("question is required for escalate".to_string());
        };
        if question.chars().count() > MAX_QUESTION_CHARS {
            return ToolResult::err(format!(
                "question 超过 {MAX_QUESTION_CHARS} 字符；请精炼到问题本身"
            ));
        }
        let context = get_str(&input, "context")
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .unwrap_or_default()
            .to_string();
        let Some((plan_id, step_id)) = current_plan_context() else {
            return ToolResult::err(
                "escalate 只能在 PlanRun 计划步骤的子代理内使用（缺少计划上下文）".to_string(),
            );
        };
        let question_id = make_question_id(&plan_id, &step_id);
        append_plan_event(
            &plan_log_path(&self.runtime, &plan_id),
            "step_escalated",
            json!({
                "question_id": question_id,
                "step": step_id,
                "question": truncate_chars(&question, MAX_QUESTION_CHARS),
                "context": truncate_chars(&context, MAX_QUESTION_CHARS),
            }),
        );

        let wait_secs = self.runtime.opts.plan_escalation_wait_secs;
        if wait_secs == 0 {
            // 提交即挂起：前台 PlanRun 期间编排者无法实时应答，等待只会空耗——
            // 答案经 ResolveWorkflowQuestion + resume_plan_id 在下一轮注入。
            return Self::submitted_result(&question_id, false);
        }
        let mut rx = self.runtime.register_escalation(&question_id);
        let answer = tokio::select! {
            biased;
            answer = &mut rx => answer.ok(),
            _ = tokio::time::sleep(Duration::from_secs(wait_secs)) => None,
        };
        match answer {
            Some(answer) => ToolResult::ok(
                json!({
                    "status": "answered",
                    "question_id": question_id,
                    "answer": answer,
                    "note": "这是编排者的实时输入，与其它工具结果同源；据此继续任务。"
                })
                .to_string(),
            ),
            None => {
                // 等待窗口内无人应答：摘除登记（避免悬挂 sender），转为 journal 协议。
                self.runtime.cancel_escalation(&question_id);
                Self::submitted_result(&question_id, true)
            }
        }
    }
}

impl EscalateTool {
    fn submitted_result(question_id: &str, timed_out: bool) -> ToolResult {
        ToolResult::ok(
            json!({
                "status": "submitted",
                "question_id": question_id,
                "timed_out": timed_out,
                "note": "问题已提交给编排者。当前流程无法立即获得答案：不要编造答案、不要基于猜测继续。\
                         请立即以结构化 handoff 收尾，说明已完成的工作与等待此答案的原因；\
                         编排者应答后该步骤会带着答案重跑。"
            })
            .to_string(),
        )
    }
}

/// 编排者的升级应答工具（主代理侧；对子代理屏蔽）。
pub struct ResolveWorkflowQuestionTool {
    runtime: AgentToolRuntime,
}

impl ResolveWorkflowQuestionTool {
    pub fn new(runtime: AgentToolRuntime) -> Self {
        ResolveWorkflowQuestionTool { runtime }
    }
}

#[async_trait]
impl Tool for ResolveWorkflowQuestionTool {
    fn name(&self) -> &str {
        "ResolveWorkflowQuestion"
    }

    fn description(&self) -> &str {
        "Answer a plan-step escalation. question_id comes from an escalate submission \
         (surfaced in a PlanRun report, the step's handoff, or the plan journal). The answer \
         is journaled into agent/plans/<plan_id>.jsonl and, if the step is still waiting in \
         this process, delivered to it immediately. After answering, resume the plan with \
         PlanRun {resume_plan_id} — answered escalations re-run their step with your answer \
         injected; unresolved ones are skipped."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question_id": {"type": "string", "description": "Escalation id (<plan_id>:<step>:q-…)."},
                "answer": {"type": "string", "description": "Authoritative answer/decision for the step (<=4000 chars)."}
            },
            "required": ["question_id", "answer"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(question_id) = get_str(&input, "question_id")
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_string)
        else {
            return ToolResult::err("question_id is required".to_string());
        };
        let Some(answer) = get_str(&input, "answer")
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string)
        else {
            return ToolResult::err("answer is required".to_string());
        };
        if answer.chars().count() > MAX_ANSWER_CHARS {
            return ToolResult::err(format!("answer 超过 {MAX_ANSWER_CHARS} 字符；请精炼"));
        }
        let Some((plan_id, _step)) = question_id.split_once(':') else {
            return ToolResult::err(
                "question_id 形如 <plan_id>:<step>:q-…；请原样使用 escalate/PlanRun 报告中的 id"
                    .to_string(),
            );
        };
        // 先解锁等待中的步骤（实时通道），journal 由本工具单点记录（重放唯一事实源）。
        let delivered = self.runtime.resolve_escalation(&question_id, &answer);
        append_plan_event(
            &plan_log_path(&self.runtime, plan_id),
            "escalation_resolved",
            json!({
                "question_id": question_id,
                "answer": truncate_chars(&answer, MAX_ANSWER_CHARS),
            }),
        );
        let guidance = if delivered {
            "答案已实时投递给等待中的步骤，它会自行继续。"
        } else {
            "该问题当前没有等待中的步骤（步骤可能已收尾或进程已重启）。答案已记入计划 journal：\
             请用 PlanRun {\"resume_plan_id\": \"<plan_id>\"} 恢复计划——已解答的升级会把你的答案\
             注入该步骤重跑，未决步骤会被跳过。"
        };
        ToolResult::ok(
            json!({
                "status": "resolved",
                "question_id": question_id,
                "plan_id": plan_id,
                "delivered_live": delivered,
                "guidance": guidance,
            })
            .to_string(),
        )
    }
}

