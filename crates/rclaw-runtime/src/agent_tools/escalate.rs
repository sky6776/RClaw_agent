//! `escalate` / `ResolveWorkflowQuestion`：计划步骤与编排者之间的提问通道
//! （ZCode 长程能力移植 · Layer 3 最小内核第一步）。
//!
//! ZCode 动态工作流里子代理被卡住时可以「无限时阻塞着向主代理提问而不是编造答案」。
//! RClaw 的 PlanRun 前台运行期间编排者（父 turn）被工具调用阻塞、无法实时应答，因此
//! v1 采用【journal 协议】：
//!
//! 1. 计划步骤内调用 `escalate` → 追加 `step_escalated` 事件到计划 journal
//!    （`agent/state.sqlite` journal `plan/<plan_id>`）并登记待应答问题；
//!    `plan_escalation_wait_secs` > 0 时阻塞等待实时答案（为将来的
//!    后台化编排者预留），默认 0 = 提交即挂起，步骤立即以结构化 handoff 收尾。
//!    PlanRun 把本轮提问且未应答的步骤判为 skipped（`awaiting_escalation`）：不交给
//!    验证者、不被依赖方当事实消费。
//! 2. 编排者调用 `ResolveWorkflowQuestion {question_id, answer}` → 校验该问题确实存在于
//!    计划 journal（未知 / 已应答均拒绝，照 ZCode `unknown_question` / `already_resolved`）
//!    后追加 `escalation_resolved` 事件；若步骤仍在等待（进程内）则实时解锁。
//! 3. `PlanRun(resume_plan_id=…)` 重放：已解答的升级注入「编排者答案」重跑该步骤，
//!    未决步骤跳过并提示先应答。
//!
//! 防注入纪律：答案进入步骤任务文本时显式标注为「编排者输入」，与上游结果同源；
//! question_id 来自模型输入，其 plan_id 段必须与 sanitize 结果逐字相同才允许定位 journal
//! 流（且只能命中已存在流里确实提过的问题，不会凭空创建新流）。

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::plan::{append_plan_event, try_append_plan_event};
use super::subagent::get_str;
use super::team::{current_plan_context, next_execution_id, sanitize_agent_name};
use super::*;

/// question_id 形如 `<plan_id>:<step_id>:q-<ts>-<n>`：冒号分隔便于从 question_id
/// 反查计划 journal（plan_id / step_id 本身不含冒号——均经 sanitize）。
fn make_question_id(plan_id: &str, step_id: &str) -> String {
    format!("{plan_id}:{step_id}:{}", next_execution_id("q"))
}

fn plan_log(runtime: &AgentToolRuntime, plan_id: &str) -> EventLog {
    EventLog::plan(&runtime.journal, plan_id)
}

/// 提问 / 应答的入口上限；plan 重跑时注入步骤的问答也按这两个上限截断
/// （入口已校验，正常数据不会被截；只防手工写入 journal 的超长行）。
pub(super) const MAX_QUESTION_CHARS: usize = 2_000;
pub(super) const MAX_ANSWER_CHARS: usize = 4_000;

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
        // 先登记等待者再写 journal：应答者以 journal 里的 step_escalated 为线索，若先落盘
        // 后登记，落在间隙里的答案会错过实时通道、步骤白等满 wait_secs。
        let wait_secs = self.runtime.opts.plan_escalation_wait_secs;
        let waiter =
            (wait_secs > 0).then(|| EscalationWaiter::register(&self.runtime, &question_id));
        // journal 是「提了问题」的唯一事实源：写不进去就不能告诉步骤「已提交」——否则
        // 步骤按挂起收尾，而 PlanRun / resume 在 journal 里看不到提问，会把 handoff 当成
        // 正常产出消费。此时返回工具错误，让步骤自己决定重试或如实失败。
        if let Err(error) = try_append_plan_event(
            &plan_log(&self.runtime, &plan_id),
            "step_escalated",
            // question 已按上限校验过、原样落盘；context 是自由文本，按同一上限裁短
            // （`truncate_text`：它不是工具输出，不该带 OUTPUT_TRUNCATED_MARKER）。
            json!({
                "question_id": question_id,
                "step": step_id,
                "question": question,
                "context": truncate_text(&context, MAX_QUESTION_CHARS),
            }),
        ) {
            drop(waiter);
            return ToolResult::err(format!(
                "escalate 未能写入计划 journal（{error}）：问题【未提交】，不要按已提交收尾；\
                 可重试一次，仍失败则如实报告无法提问并说明阻塞点"
            ));
        }

        let Some(mut waiter) = waiter else {
            // 提交即挂起：前台 PlanRun 期间编排者无法实时应答，等待只会空耗——
            // 答案经 ResolveWorkflowQuestion + resume_plan_id 在下一轮注入。
            return Self::submitted_result(&question_id, false);
        };
        let answer = tokio::select! {
            biased;
            answer = &mut waiter.rx => answer.ok(),
            _ = tokio::time::sleep(Duration::from_secs(wait_secs)) => None,
        };
        // 超时与应答可能同时发生：先关闭接收端（之后的 send 必失败），再把关闭前已到达的
        // 答案取走——应答方看到的「已实时投递」与步骤「确实拿到答案」严格一致，不会出现
        // 答案落进通道却没人读、journal 又没有送达记录之外的第三种状态。
        let answer = answer.or_else(|| {
            waiter.rx.close();
            waiter.rx.try_recv().ok()
        });
        // 守卫 drop 即摘除登记：超时、应答后（已被 resolve 摘除，幂等）、以及步骤 future
        // 在外层 select! 中被整体丢弃时都不会在 pending_escalations 里留下悬挂 sender。
        drop(waiter);
        match answer {
            Some(answer) => {
                // 送达记录由拿到答案的一方落：PlanRun 波次收尾时据此区分「步骤带着答案
                // 继续了」与「答案在超时之后才到、步骤没见过」（后者仍按挂起处理、resume
                // 带答案重跑）。写失败只会让计划多挂起一次（安全方向），不阻断步骤。
                append_plan_event(
                    &plan_log(&self.runtime, &plan_id),
                    "escalation_delivered",
                    json!({"question_id": question_id, "step": step_id}),
                );
                ToolResult::ok(
                    json!({
                        "status": "answered",
                        "question_id": question_id,
                        "answer": answer,
                        "note": "这是编排者的实时输入，与其它工具结果同源；据此继续任务。"
                    })
                    .to_string(),
                )
            }
            None => Self::submitted_result(&question_id, true),
        }
    }
}

/// 等待中的升级登记守卫：持有实时应答接收端，drop 时从运行时登记表摘除该问题。
struct EscalationWaiter {
    runtime: AgentToolRuntime,
    question_id: String,
    rx: tokio::sync::oneshot::Receiver<String>,
}

impl EscalationWaiter {
    fn register(runtime: &AgentToolRuntime, question_id: &str) -> Self {
        EscalationWaiter {
            runtime: runtime.clone(),
            question_id: question_id.to_string(),
            rx: runtime.register_escalation(question_id),
        }
    }
}

impl Drop for EscalationWaiter {
    fn drop(&mut self) {
        self.runtime.cancel_escalation(&self.question_id);
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
                         本步骤会被计划判为 skipped（awaiting_escalation），编排者应答并 resume 后\
                         该步骤会带着答案重跑。"
            })
            .to_string(),
        )
    }
}

/// question_id 在计划 journal 中的状态（ResolveWorkflowQuestion 的准入判据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JournalQuestionState {
    /// journal 不存在或没有这个 question_id 的 step_escalated 记录。
    Unknown,
    /// 已提问、尚未应答。
    Open,
    /// 已有 escalation_resolved 记录。
    Resolved,
}

/// 从计划 journal 的事件里判定一个提问的状态（纯函数；读失败由调用方处理——分不清有没有
/// 这个问题时不能报成 `unknown_question`，编排者会据此放弃一个其实有效的提问）。
fn question_state_from_events(events: &[(u64, Value)], question_id: &str) -> JournalQuestionState {
    let mut state = JournalQuestionState::Unknown;
    for (_, event) in events {
        if event.get("question_id").and_then(Value::as_str) != Some(question_id) {
            continue;
        }
        match event.get("event").and_then(Value::as_str) {
            Some("step_escalated") => {
                if state == JournalQuestionState::Unknown {
                    state = JournalQuestionState::Open;
                }
            }
            Some("escalation_resolved") => state = JournalQuestionState::Resolved,
            _ => {}
        }
    }
    state
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
         (surfaced in a PlanRun report, the step's handoff, or the plan journal); unknown or \
         already-answered ids are rejected. The answer is journaled into the plan journal \
         (agent/state.sqlite, plan/<plan_id>) and, if the step is still blocked waiting in this process \
         (plan_escalation_wait_secs > 0), delivered to it immediately. Otherwise the step was \
         parked as skipped (awaiting_escalation): resume the plan with PlanRun \
         {resume_plan_id} — answered escalations re-run their step with your answer injected; \
         unresolved ones stay skipped."
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
        // plan_id 段是模型输入、将用于定位 journal 流：必须与 sanitize 结果逐字一致
        // （拒绝大小写 / 空白 / 路径样式变体），且只能命中【已存在】journal 里【确实提过】
        // 的问题——否则会凭空创建或追加任意 plan 流。
        if plan_id.is_empty() || sanitize_agent_name(plan_id) != plan_id {
            return ToolResult::err(format!(
                "unknown_question: question_id 的 plan_id 段 '{plan_id}' 不合法；请原样使用 \
                 escalate/PlanRun 报告中的 id"
            ));
        }
        let plan_log = plan_log(&self.runtime, plan_id);
        // 「查状态 → 追加应答」在 journal 的同一个写事务里完成（`EventLog::append_guarded`）：
        // 两个应答者同时看到 Open 就会各写一条 escalation_resolved，而读端口径不一（装载取
        // 最后一条、收尾判定取第一条、实时投递只到先到者），双方却都拿到 resolved。写锁先于
        // 读取，同进程另一 turn 与另一进程（桌面 + CLI 共用一个 state.sqlite）的应答都只能
        // 排在后面、看到本次结果而拿到 already_resolved。
        //
        // journal 先行（重放唯一事实源）：先落 escalation_resolved，再解锁等待中的步骤。
        // 反过来会出现「步骤拿着答案继续了，journal 却没有应答记录」——下一次 resume
        // 把同一个问题又当未决提问挂起。读 / 写失败直接拒绝，答案一个字都不投递。
        // answer 已按上限校验过，原样落盘。
        let record = super::plan::plan_event_record(
            "escalation_resolved",
            json!({
                "question_id": question_id,
                "answer": answer,
            }),
        );
        let outcome = plan_log.append_guarded(|events| {
            let state = question_state_from_events(events, &question_id);
            let payload = (state == JournalQuestionState::Open).then_some(record);
            (payload, state)
        });
        match outcome {
            Ok((_, JournalQuestionState::Unknown)) => {
                return ToolResult::err(format!(
                    "unknown_question: 计划 journal（{}）中没有 id 为 '{question_id}' 的升级提问；\
                     请检查 PlanRun 报告里列出的待应答 question_id",
                    plan_log.locator()
                ));
            }
            Ok((_, JournalQuestionState::Resolved)) => {
                return ToolResult::err(format!(
                    "already_resolved: 升级 '{question_id}' 已有应答记录，不能重复应答；\
                     如需更正，请修改该步骤的任务文本后用 PlanRun resume_plan_id 重跑"
                ));
            }
            Ok((Some(_), JournalQuestionState::Open)) => {}
            Ok((None, JournalQuestionState::Open)) => {
                return ToolResult::err(
                    "应答未能写入计划 journal（内部不变量被破坏：判定为 Open 却未追加）：\
                     本次应答【未生效】，请稍后重试"
                        .to_string(),
                );
            }
            Err(error) => {
                // 读失败与写失败一个口径：问题是否存在 / 应答有没有落都无法判定，不能报成
                // unknown_question（编排者会据此放弃一个其实有效的提问）。
                return ToolResult::err(format!(
                    "应答暂不可用：计划 journal 读写失败（{error}）；本次应答【未生效】，请稍后重试"
                ));
            }
        }
        let delivered = self.runtime.resolve_escalation(&question_id, &answer);
        let guidance = if delivered {
            "答案已实时投递给仍在阻塞等待的步骤，它会自行继续。"
        } else {
            "该问题当前没有等待中的步骤（步骤已以 skipped/awaiting_escalation 收尾，或进程已重启）。\
             答案已记入计划 journal：请用 PlanRun {\"resume_plan_id\": \"<plan_id>\"} 恢复计划——\
             已解答的升级会把你的答案注入该步骤重跑，未决步骤仍会被跳过。"
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

#[cfg(test)]
mod tests {
    use super::*;

    struct NullProvider;

    #[async_trait]
    impl Provider for NullProvider {
        fn name(&self) -> &str {
            "null"
        }

        async fn chat(&self, _req: ChatRequest) -> rclaw_llm::Result<rclaw_core::ChatOutcome> {
            Ok(rclaw_core::ChatOutcome {
                text: "ok".to_string(),
                reasoning: None,
                tool_calls: Vec::new(),
                stop: Some(rclaw_core::FinishReason::Stop),
                usage: None,
            })
        }

        async fn chat_stream(
            &self,
            req: ChatRequest,
            _on_text: rclaw_llm::OnText,
        ) -> rclaw_llm::Result<rclaw_core::ChatOutcome> {
            self.chat(req).await
        }
    }

    fn runtime_in_temp(name: &str) -> (std::path::PathBuf, AgentToolRuntime) {
        let dir = std::env::temp_dir().join(format!(
            "rclaw-escalate-{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let layout = Layout::new(dir.join("agent"));
        let registry: RegistryLink = Arc::new(OnceLock::new());
        registry.set(Arc::new(ToolRegistry::new())).ok();
        let runtime = AgentToolRuntime::new(
            Arc::new(NullProvider),
            MemoryStore::new(layout.clone()),
            layout.clone(),
            JournalStore::open(&layout).expect("open state.sqlite"),
            AgentOptions::default(),
            registry,
            rclaw_core::RuntimeChannelInfo::default(),
            None,
        );
        (dir, runtime)
    }

    /// 某 plan 流里 `escalation_resolved` 事件数。
    fn resolved_count(runtime: &AgentToolRuntime, plan_id: &str) -> usize {
        plan_log(runtime, plan_id)
            .load()
            .unwrap()
            .iter()
            .filter(|(_, event)| event["event"] == "escalation_resolved")
            .count()
    }

    #[tokio::test]
    async fn resolve_only_accepts_open_questions_from_existing_journals() {
        let (dir, runtime) = runtime_in_temp("resolve");
        append_plan_event(
            &plan_log(&runtime, "plan-r"),
            "step_escalated",
            json!({"question_id": "plan-r:s1:q-1", "step": "s1", "question": "用哪个口径？"}),
        );
        let resolver = ResolveWorkflowQuestionTool::new(runtime.clone());
        // plan_id 段带路径样式 / 大小写变体：拒绝，且不得创建任何 journal 流。
        for bad in [
            "../evil:s1:q-1",
            "Plan-R:s1:q-1",
            ":s1:q-1",
            "plan r:s1:q-1",
        ] {
            let result = resolver
                .execute(json!({"question_id": bad, "answer": "x"}))
                .await;
            assert!(result.is_error, "{bad}: {}", result.content);
            assert!(
                result.content.starts_with("unknown_question"),
                "{bad}: {}",
                result.content
            );
        }
        assert_eq!(
            runtime.journal.streams(JOURNAL_PLAN).unwrap(),
            vec!["plan-r".to_string()],
            "非法 plan_id 不得创建新的 journal 流"
        );
        // journal 不存在 / 问题不存在：unknown_question，不凭空创建 journal。
        let result = resolver
            .execute(json!({"question_id": "plan-ghost:s1:q-1", "answer": "x"}))
            .await;
        assert!(
            result.is_error && result.content.starts_with("unknown_question"),
            "{}",
            result.content
        );
        assert!(!plan_log(&runtime, "plan-ghost").exists().unwrap());
        let result = resolver
            .execute(json!({"question_id": "plan-r:s1:q-nope", "answer": "x"}))
            .await;
        assert!(
            result.is_error && result.content.starts_with("unknown_question"),
            "{}",
            result.content
        );
        // 真实提问：记 journal，无等待者 → delivered_live=false + resume 指引。
        let result = resolver
            .execute(json!({"question_id": "plan-r:s1:q-1", "answer": "按新口径"}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        let parsed: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["delivered_live"], false);
        assert!(parsed["guidance"]
            .as_str()
            .unwrap()
            .contains("resume_plan_id"));
        assert_eq!(resolved_count(&runtime, "plan-r"), 1);
        // 重复应答：already_resolved，journal 不再追加。
        let result = resolver
            .execute(json!({"question_id": "plan-r:s1:q-1", "answer": "再答一次"}))
            .await;
        assert!(
            result.is_error && result.content.starts_with("already_resolved"),
            "{}",
            result.content
        );
        assert_eq!(resolved_count(&runtime, "plan-r"), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端到端的不动点校验：PlanRun 用 `next_execution_id(sanitize(计划名))` 生成 plan_id，
    /// escalate 再拼成 question_id；带多字符小写映射的计划名（'İ'）生成的 id 必须能原样通过
    /// `sanitize(plan_id) == plan_id` 准入，否则我们自己的升级永远无法应答。
    #[tokio::test]
    async fn resolve_accepts_question_ids_derived_from_unicode_plan_names() {
        let (dir, runtime) = runtime_in_temp("resolve-unicode");
        let plan_id = next_execution_id(&sanitize_agent_name("İstanbul Plan 腾讯 ΣΊΣΥΦΟΣ"));
        assert_eq!(sanitize_agent_name(&plan_id), plan_id);
        let question_id = make_question_id(&plan_id, "s1");
        append_plan_event(
            &plan_log(&runtime, &plan_id),
            "step_escalated",
            json!({"question_id": question_id, "step": "s1", "question": "用哪个口径？"}),
        );
        let result = ResolveWorkflowQuestionTool::new(runtime.clone())
            .execute(json!({"question_id": question_id, "answer": "按新口径"}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(resolved_count(&runtime, &plan_id), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn escalation_waiter_unregisters_on_drop_and_delivers_while_alive() {
        let (dir, runtime) = runtime_in_temp("waiter");
        let mut waiter = EscalationWaiter::register(&runtime, "p:s:q-live");
        assert!(
            runtime.resolve_escalation("p:s:q-live", "答案"),
            "存活的等待者应收到实时投递"
        );
        assert_eq!((&mut waiter.rx).await.unwrap(), "答案");
        drop(waiter);
        // 守卫被丢弃（select! 整体取消 / 超时）后登记必须被摘除，不留悬挂 sender。
        let waiter = EscalationWaiter::register(&runtime, "p:s:q-drop");
        drop(waiter);
        assert!(!runtime.resolve_escalation("p:s:q-drop", "答案"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
