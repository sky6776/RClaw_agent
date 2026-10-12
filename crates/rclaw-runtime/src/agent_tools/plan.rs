//! PlanRun：确定性计划工作流（plan 工件 + 依赖序执行）。
//!
//! RClaw 的团队编排是 LLM 驱动的（主循环用 Agent/Task/看板工具自行组织），适合开放
//! 任务；但对 cron 型固定流程（每日报告、定期巡检），「确定性骨架 + LLM 节点」更省
//! 更稳：本工具让主代理一次性给出 plan 工件（步骤 + 依赖），执行器按拓扑序确定地
//! 跑完——并行波次、失败传染为 skipped、不重试、逐步留事件日志（对齐 JiuwenSwarm
//! Swarmflow 的「图结构是代码、LLM 只在节点内」哲学，但收敛为最小工件：JSON 步骤表
//! 而非脚本引擎）。步骤本体复用 [`AgentToolRuntime::run_task`]，因此继承子代理的全部
//! 门禁（熔断 / 三层完成验证 / candidate_patch 隔离 / 推理错峰 / 沙箱 fail-safe）。
//!
//! 与 Agent/Task 的分工：单点委派用 Agent/Task；「已知流程、多步、有依赖」用 PlanRun。
//! 子代理内禁调 PlanRun（[`is_subagent_blocked_tool`]），计划不可嵌套。

use super::*;
use futures::future::join_all;

/// 计划的步骤数上限：计划应是确定的中等流程，不是任务分解的黑洞。
const MAX_PLAN_STEPS: usize = 12;
/// 单步任务文本上限（字符）。
const MAX_STEP_TASK_CHARS: usize = 4_000;
/// 单步子代理的工具迭代上限（比 Agent 默认 256 收敛：计划步骤应当聚焦）。
const PLAN_STEP_MAX_TOOL_ITER: usize = 64;
/// 依赖摘要注入单步 prompt 的字符上限。
const DEP_SUMMARY_MAX_CHARS: usize = 1_500;
/// 默认 / 最大并行步骤数。
const DEFAULT_MAX_PARALLEL: usize = 2;
const MAX_PARALLEL_STEPS: usize = 4;
/// 对抗环 verdict 失败轮次上限（默认 / 硬顶）。`while (!verdict.pass)` 的有界化：
/// 每轮失败摘要注入目标重跑，超过上限即判 Failed（依赖方按既有策略传染 skipped）。
const DEFAULT_VERDICT_ROUNDS: u32 = 2;
const MAX_VERDICT_ROUNDS: u32 = 4;

#[derive(Debug, Clone)]
struct PlanStep {
    id: String,
    task: String,
    role: String,
    depends_on: Vec<String>,
    /// Layer 3 对抗环：本步骤的产出由哪一个步骤验证（Programmer/Prover 的 Prover）。
    /// 被验证步骤成功后不直接 Succeeded，而是进入 Verifying，等待验证步骤给出
    /// verdict：pass → 双双 Succeeded；fail → 失败摘要注入本步骤任务文本重跑，
    /// 直到 pass 或轮次耗尽（`max_verdict_rounds`）。
    verified_by: Option<String>,
    /// 验证步骤的额外通过标准（注入验证步骤的任务文本）。
    pass_condition: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum StepStatus {
    Pending,
    /// 已执行、等待验证步骤的 verdict（仅 verified_by 步骤会进入）。
    Verifying,
    Succeeded,
    Failed,
    Skipped,
}

pub struct PlanRunTool {
    runtime: AgentToolRuntime,
}

impl PlanRunTool {
    pub fn new(runtime: AgentToolRuntime) -> Self {
        PlanRunTool { runtime }
    }
}

fn sanitize_step_id(raw: &str) -> Result<String, String> {
    let id = raw.trim();
    if id.is_empty()
        || id.len() > 32
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(format!("step id '{id}' 必须是 1-32 个字母/数字/-/_ 字符"));
    }
    Ok(id.to_ascii_lowercase())
}

fn parse_steps(input: &Value) -> Result<Vec<PlanStep>, String> {
    let raw_steps = input
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| "steps 是必填数组".to_string())?;
    if raw_steps.is_empty() || raw_steps.len() > MAX_PLAN_STEPS {
        return Err(format!("steps 必须有 1-{MAX_PLAN_STEPS} 个步骤"));
    }
    let mut steps = Vec::with_capacity(raw_steps.len());
    for (idx, raw) in raw_steps.iter().enumerate() {
        let id = sanitize_step_id(raw.get("id").and_then(Value::as_str).unwrap_or_default())
            .map_err(|error| format!("steps[{idx}].id: {error}"))?;
        let task = raw
            .get("task")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|task| !task.is_empty())
            .ok_or_else(|| format!("steps[{idx}].task 缺失或为空"))?
            .to_string();
        if task.chars().count() > MAX_STEP_TASK_CHARS {
            return Err(format!(
                "steps[{idx}].task 超过 {MAX_STEP_TASK_CHARS} 字符；过长步骤应拆分"
            ));
        }
        // 依赖去重：重复项会让 Kahn 入度多计、递减漏计，被误报成「依赖环」。
        let depends_on = raw
            .get("depends_on")
            .and_then(Value::as_array)
            .map(|deps| {
                deps.iter()
                    .filter_map(Value::as_str)
                    .map(sanitize_step_id)
                    .collect::<Result<BTreeSet<_>, _>>()
            })
            .unwrap_or(Ok(BTreeSet::new()))
            .map_err(|error| format!("steps[{idx}].depends_on: {error}"))?
            .into_iter()
            .collect::<Vec<_>>();
        if depends_on.len() > MAX_PLAN_STEPS {
            return Err(format!("steps[{idx}].depends_on 过多（>{MAX_PLAN_STEPS}）"));
        }
        steps.push(PlanStep {
            id,
            task,
            role: raw
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string(),
            depends_on,
            verified_by: raw
                .get("verified_by")
                .and_then(Value::as_str)
                .map(sanitize_step_id)
                .transpose()?,
            pass_condition: raw
                .get("pass_condition")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_string),
        });
    }
    // 唯一 id + 依赖存在性。
    let mut seen = BTreeSet::new();
    for step in &steps {
        if !seen.insert(&step.id) {
            return Err(format!("step id '{}' 重复", step.id));
        }
    }
    let ids: BTreeSet<&String> = steps.iter().map(|step| &step.id).collect();
    for step in &steps {
        for dep in &step.depends_on {
            if !ids.contains(dep) {
                return Err(format!("step '{}' 依赖不存在的步骤 '{dep}'", step.id));
            }
        }
    }
    // 环检测（Kahn）：能拓扑处理的步骤数 < 总数 → 有环。
    let mut indegree: HashMap<&String, usize> = steps
        .iter()
        .map(|step| (&step.id, step.depends_on.len()))
        .collect();
    let mut queue: Vec<&String> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut processed = 0usize;
    while let Some(current) = queue.pop() {
        processed += 1;
        for step in &steps {
            if step.depends_on.iter().any(|dep| dep == current) {
                if let Some(degree) = indegree.get_mut(&step.id) {
                    *degree -= 1;
                    if *degree == 0 {
                        queue.push(&step.id);
                    }
                }
            }
        }
    }
    if processed != steps.len() {
        return Err("steps 存在依赖环（depends_on 必须构成 DAG）".to_string());
    }
    // Layer 3 对抗环规则：验证关系必须是一层、无自引用、无依赖死锁。
    let by_id: HashMap<&String, &PlanStep> = steps.iter().map(|s| (&s.id, s)).collect();
    for step in &steps {
        let Some(verifier_id) = &step.verified_by else {
            continue;
        };
        if verifier_id == &step.id {
            return Err(format!(
                "step '{}' 的 verified_by 不能指向自己（对抗环由执行器驱动，无需自验证）",
                step.id
            ));
        }
        let Some(verifier) = by_id.get(verifier_id) else {
            return Err(format!(
                "step '{}' 的 verified_by 指向不存在的步骤 '{verifier_id}'",
                step.id
            ));
        };
        if verifier.verified_by.is_some() {
            return Err(format!(
                "step '{verifier_id}' 既是验证者又声明了 verified_by：v1 只支持一层验证关系"
            ));
        }
        if verifier.depends_on.contains(&step.id) {
            return Err(format!(
                "验证步骤 '{verifier_id}' 不能 depends_on 被验证步骤 '{}'：验证依赖是隐式的，\
                 显式声明会让双方互相等待（死锁）",
                step.id
            ));
        }
    }
    Ok(steps)
}

pub(super) fn append_plan_event(log_path: &Path, event: &str, payload: Value) {
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut record = json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "event": event,
    });
    if let (Some(target), Some(source)) = (record.as_object_mut(), payload.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    else {
        tracing::warn!(path = %log_path.display(), "计划事件日志打开失败");
        return;
    };
    if let Err(error) = writeln!(file, "{record}") {
        tracing::warn!(%error, "计划事件日志写入失败");
    }
}

/// 一条计划步骤的升级记录（Layer 3 重放素材）。
pub(super) struct EscalationRecord {
    pub(super) step: String,
    pub(super) question_id: String,
    pub(super) question: String,
    pub(super) answer: Option<String>,
}

/// 一条对抗环 verdict 记录（Layer 3 重放素材：轮次预算跨 resume 续算的依据）。
pub(super) struct VerdictRecord {
    pub(super) verifier: String,
    pub(super) target: String,
    pub(super) round: u32,
    pub(super) passed: bool,
    pub(super) reasons: Vec<String>,
}

/// 计划事件日志的解析结果。
pub(super) struct PlanJournal {
    pub(super) steps: Vec<(String, bool, String)>,
    pub(super) escalations: Vec<EscalationRecord>,
    pub(super) verdicts: Vec<VerdictRecord>,
}

/// 从验证步骤的输出提取 verdict（Programmer/Prover 协议）。
///
/// 取正文第一个 `{` 到最后一个 `}` 之间的 JSON：`{"pass": bool, "reasons": [...]}`。
/// 提取/解析失败或缺少 `pass` 一律按 fail 处理并给出可操作的原因——验证者输出
/// 不规范等同于「未能证明」，绝不能默认通过。
fn parse_verdict(content: &str) -> (bool, Vec<String>) {
    let Some(start) = content.find('{') else {
        return (
            false,
            vec!["验证步骤未返回 JSON verdict 对象（需 {\"pass\": bool, \"reasons\": [...]}）"
                .to_string()],
        );
    };
    let Some(end) = content.rfind('}') else {
        return (false, vec!["验证步骤输出缺少 verdict JSON 的右花括号".to_string()]);
    };
    if end < start {
        return (false, vec!["验证步骤输出中没有完整的 verdict JSON".to_string()]);
    }
    let parsed: Value = match serde_json::from_str(&content[start..=end]) {
        Ok(value) => value,
        Err(error) => {
            return (
                false,
                vec![format!("verdict JSON 不可解析: {error}")],
            );
        }
    };
    let Some(pass) = parsed.get("pass").and_then(Value::as_bool) else {
        return (
            false,
            vec!["verdict JSON 缺少 \"pass\" 布尔字段".to_string()],
        );
    };
    let reasons = parsed
        .get("reasons")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if pass {
        (true, reasons)
    } else {
        let reasons = if reasons.is_empty() {
            vec!["verdict 为 fail 但未给出 reasons；请假设上一轮产出未满足验收标准并全面复查"
                .to_string()]
        } else {
            reasons
        };
        (false, reasons)
    }
}

/// 解析计划事件日志：步骤终态（每步取最后一次 step_finished）+ 升级问答 + 对抗环 verdict。
///
/// resume 按 id 匹配、以最新结果为准（同一 plan 重跑过多次时取最后一次）；
/// 日志不存在 / 没有任何 step_finished 记录 → Err（调用方报给模型，
/// 而不是静默从零开始——错误重定向比静默重跑危险）。
pub(super) fn load_plan_journal(log_path: &Path) -> Result<PlanJournal, String> {
    let text = std::fs::read_to_string(log_path).map_err(|error| {
        format!(
            "读取计划事件日志失败（{}）：{error}。resume 需要一个已存在 plan 的 id（见先前 PlanRun 结果里的事件日志路径）。",
            log_path.display()
        )
    })?;
    let mut latest: HashMap<String, (bool, String)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    // 升级问答：question_id → 记录；escalation_resolved 事后补答案（按 id 匹配）。
    let mut escalations: HashMap<String, EscalationRecord> = HashMap::new();
    let mut escalation_order: Vec<String> = Vec::new();
    let mut verdicts: Vec<VerdictRecord> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event.get("event").and_then(Value::as_str) {
            Some("step_finished") => {
                let Some(step) = event.get("step").and_then(Value::as_str) else {
                    continue;
                };
                let succeeded =
                    event.get("outcome").and_then(Value::as_str) == Some("succeeded");
                let summary = event
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !latest.contains_key(step) {
                    order.push(step.to_string());
                }
                latest.insert(step.to_string(), (succeeded, summary));
            }
            Some("step_escalated") => {
                let (Some(step), Some(question_id), Some(question)) = (
                    event.get("step").and_then(Value::as_str),
                    event.get("question_id").and_then(Value::as_str),
                    event.get("question").and_then(Value::as_str),
                ) else {
                    continue;
                };
                if !escalations.contains_key(question_id) {
                    escalation_order.push(question_id.to_string());
                }
                escalations.insert(
                    question_id.to_string(),
                    EscalationRecord {
                        step: step.to_string(),
                        question_id: question_id.to_string(),
                        question: question.to_string(),
                        answer: None,
                    },
                );
            }
            Some("escalation_resolved") => {
                let (Some(question_id), Some(answer)) = (
                    event.get("question_id").and_then(Value::as_str),
                    event.get("answer").and_then(Value::as_str),
                ) else {
                    continue;
                };
                if let Some(record) = escalations.get_mut(question_id) {
                    record.answer = Some(answer.to_string());
                }
            }
            Some("verdict_failed") | Some("verdict_passed") => {
                let passed = event.get("event").and_then(Value::as_str) == Some("verdict_passed");
                let (Some(verifier), Some(target), Some(round)) = (
                    event.get("verifier").and_then(Value::as_str),
                    event.get("target").and_then(Value::as_str),
                    event.get("round").and_then(Value::as_u64),
                ) else {
                    continue;
                };
                let reasons = event
                    .get("reasons")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                verdicts.push(VerdictRecord {
                    verifier: verifier.to_string(),
                    target: target.to_string(),
                    round: round as u32,
                    passed,
                    reasons,
                });
            }
            _ => {}
        }
    }
    if latest.is_empty() {
        return Err(format!(
            "计划事件日志（{}）里没有任何 step_finished 记录；resume 只对至少执行过一步的 plan 有意义。",
            log_path.display()
        ));
    }
    let steps = order
        .into_iter()
        .filter_map(|id| {
            latest
                .remove(&id)
                .map(|(succeeded, summary)| (id, succeeded, summary))
        })
        .collect();
    let escalations = escalation_order
        .into_iter()
        .filter_map(|id| escalations.remove(&id))
        .collect();
    Ok(PlanJournal {
        steps,
        escalations,
        verdicts,
    })
}

/// Layer 3 重放：把升级记录种进步骤状态。
///
/// - 已解答 → 强制回 Pending（答案改变前提，即使 journal 里已 succeeded）并把
///   (question, answer) 收进返回表，供重跑时注入任务文本；
/// - 未决 → Pending 降为 Skipped（等待编排者应答），返回其步骤 id 供事件日志与报告。
pub(super) fn apply_escalation_seeding(
    status: &mut HashMap<String, StepStatus>,
    escalations: &[EscalationRecord],
) -> (
    HashMap<String, Vec<(String, String)>>,
    Vec<(String, String)>,
) {
    let mut answers: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut awaiting: Vec<(String, String)> = Vec::new();
    for escalation in escalations {
        if !status.contains_key(&escalation.step) {
            continue;
        }
        match &escalation.answer {
            Some(answer) => {
                answers
                    .entry(escalation.step.clone())
                    .or_default()
                    .push((escalation.question.clone(), answer.clone()));
                status.insert(escalation.step.clone(), StepStatus::Pending);
            }
            None => {
                if status.get(&escalation.step) == Some(&StepStatus::Pending)
                    && !awaiting.iter().any(|(id, _)| id == &escalation.step)
                {
                    status.insert(escalation.step.clone(), StepStatus::Skipped);
                    awaiting.push((
                        escalation.step.clone(),
                        escalation.question_id.clone(),
                    ));
                }
            }
        }
    }
    (answers, awaiting)
}

/// Layer 3 对抗环重放：verdict 状态决定 target/verifier 在本次恢复中的起点。
///
/// - 最新 verdict 通过 → 维持 journal 继承的 succeeded，轮次入账；
/// - 失败且轮次耗尽 → 双双 Failed（预算持久，resume 不自动加码）；
/// - 失败未耗尽 → 双方回 Pending，最后失败原因注入 feedback，轮次续算；
/// - 声明了验证但无 verdict 记录（旧日志/中断）→ 视为未验证，重跑。
fn seed_verdict_replay(
    verdicts: &[VerdictRecord],
    verifier_target: &HashMap<String, String>,
    max_rounds: u32,
    status: &mut HashMap<String, StepStatus>,
    verdict_feedback: &mut HashMap<String, String>,
    verdict_rounds: &mut HashMap<String, u32>,
) {
    for (verifier_id, target_id) in verifier_target {
        let last = verdicts.iter().rev().find(|v| &v.target == target_id);
        match last {
            Some(v) if v.passed => {
                verdict_rounds.insert(target_id.clone(), v.round);
            }
            Some(v) if v.round >= max_rounds => {
                status.insert(target_id.clone(), StepStatus::Failed);
                status.insert(verifier_id.clone(), StepStatus::Failed);
                verdict_rounds.insert(target_id.clone(), v.round);
                if !v.reasons.is_empty() {
                    verdict_feedback.insert(target_id.clone(), v.reasons.join("; "));
                }
            }
            Some(v) => {
                status.insert(target_id.clone(), StepStatus::Pending);
                status.insert(verifier_id.clone(), StepStatus::Pending);
                verdict_rounds.insert(target_id.clone(), v.round);
                verdict_feedback.insert(target_id.clone(), v.reasons.join("; "));
            }
            None => {
                status.insert(target_id.clone(), StepStatus::Pending);
                status.insert(verifier_id.clone(), StepStatus::Pending);
            }
        }
    }
}

/// 扫描计划 journal，返回【未应答】的升级提问 `(question_id, step, question)`。
///
/// 父模型是应答者：报告必须带问题原文，否则它只能看到 id 无法作答。
fn open_escalations_from_journal(log_path: &Path) -> Vec<(String, String, String)> {
    let Ok(text) = std::fs::read_to_string(log_path) else {
        return Vec::new();
    };
    let mut asked: Vec<(String, String, String)> = Vec::new();
    let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event.get("event").and_then(Value::as_str) {
            Some("step_escalated") => {
                if let (Some(question_id), Some(step), Some(question)) = (
                    event.get("question_id").and_then(Value::as_str),
                    event.get("step").and_then(Value::as_str),
                    event.get("question").and_then(Value::as_str),
                ) {
                    asked.push((question_id.to_string(), step.to_string(), question.to_string()));
                }
            }
            Some("escalation_resolved") => {
                if let Some(question_id) = event.get("question_id").and_then(Value::as_str) {
                    answered.insert(question_id.to_string());
                }
            }
            _ => {}
        }
    }
    asked
        .into_iter()
        .filter(|(question_id, _, _)| !answered.contains(question_id))
        .collect()
}

#[async_trait]
impl Tool for PlanRunTool {
    fn name(&self) -> &str {
        "PlanRun"
    }

    fn description(&self) -> &str {
        "Run a deterministic multi-step plan: you provide the plan artifact (steps with \
         dependencies), the executor runs it in topological order — parallel waves, failed \
         steps mark their dependents skipped, no retries, per-step event log under \
         agent/plans/. Each step runs as a bounded sub-agent with all RClaw gates. Use this \
         instead of Agent/Task fan-out when the workflow itself is known and fixed (e.g. cron \
         routines: gather -> analyze -> render). Steps cannot spawn sub-agents or nested \
         plans. Failed steps are reported, not retried; you decide how to follow up.          Declare \"verified_by\" on a step to form a Programmer/Prover loop: the verifier          returns a JSON verdict; on fail the target re-runs with the failure reasons injected          (bounded by max_verdict_rounds, round budget persisted in the journal)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Short plan name for logs (default 'plan')."},
                "steps": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 12,
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "Unique step id: 1-32 chars of [a-z0-9-_]."},
                            "task": {"type": "string", "description": "Self-contained instruction for the step's sub-agent (<=4000 chars)."},
                            "role": {"type": "string"},
                            "depends_on": {"type": "array", "items": {"type": "string"}, "description": "Step ids that must succeed before this step runs."},
                            "verified_by": {"type": "string", "description": "Programmer/Prover loop: id of another step that VERIFIES this step's output. This step enters 'verifying' after running; the verifier must return a JSON verdict {\"pass\": bool, \"reasons\": [...]}. On fail, the failure reasons are injected back into this step and it re-runs, until pass or max_verdict_rounds."},
                            "pass_condition": {"type": "string", "description": "Extra pass criteria injected into the verifier's task (use with verified_by)."}
                        },
                        "required": ["id", "task"]
                    }
                },
                "max_parallel": {"type": "integer", "minimum": 1, "maximum": 4},
                "max_verdict_rounds": {"type": "integer", "minimum": 1, "maximum": 4, "description": "Allowed FAILED verdict rounds per verified pair before both steps are marked failed (default 2). Budget persists across resume via the journal."},
                "resume_plan_id": {"type": "string", "description": "Resume a previous plan by id (see the event log path in an earlier PlanRun result): steps recorded 'succeeded' in that journal inherit their summaries and do NOT re-run; failed/skipped steps become pending again so you can retry them with (possibly edited) task text. Steps are matched by id."}
            },
            "required": ["steps"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let steps = match parse_steps(&input) {
            Ok(steps) => steps,
            Err(error) => return ToolResult::err(error),
        };
        let plan_name = sanitize_agent_name(
            get_str(&input, "name")
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("plan"),
        );
        let max_parallel = input
            .get("max_parallel")
            .and_then(Value::as_u64)
            .map(|value| value.clamp(1, MAX_PARALLEL_STEPS as u64) as usize)
            .unwrap_or(DEFAULT_MAX_PARALLEL);
        // Layer 3（ZCode 移植）：journal 断点续跑 + amend。同一 plan_id 重提交时，
        // 事件日志里已 succeeded 的步骤按 id 继承结果摘要、不再重跑（等价 ZCode
        // AmendWorkflow 的「按名字导入已完成工作」）；failed/skipped 的步骤回到
        // Pending，允许以（可能已修改的）任务文本重试。崩溃 / 上限中断的计划由此
        // 变成可恢复工件，而不是一次性执行。
        let resume_request = get_str(&input, "resume_plan_id")
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let (seeded, escalations, journal_verdicts): (
            Vec<(String, bool, String)>,
            Vec<EscalationRecord>,
            Vec<VerdictRecord>,
        ) = match resume_request {
            Some(raw_id) => {
                let id = sanitize_agent_name(raw_id);
                let path = self
                    .runtime
                    .layout
                    .base
                    .join("plans")
                    .join(format!("{id}.jsonl"));
                match load_plan_journal(&path) {
                    Ok(journal) => (journal.steps, journal.escalations, journal.verdicts),
                    Err(error) => return ToolResult::err(error),
                }
            }
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        let plan_id = match resume_request {
            Some(raw_id) => sanitize_agent_name(raw_id),
            None => next_execution_id(&plan_name),
        };
        let log_path = self
            .runtime
            .layout
            .base
            .join("plans")
            .join(format!("{plan_id}.jsonl"));
        append_plan_event(
            &log_path,
            "plan_started",
            json!({"plan_id": plan_id, "name": plan_name, "steps": steps.iter().map(|s| &s.id).collect::<Vec<_>>(), "max_parallel": max_parallel, "resumed": !seeded.is_empty()}),
        );

        let mut status: HashMap<String, StepStatus> = steps
            .iter()
            .map(|step| (step.id.clone(), StepStatus::Pending))
            .collect();
        let mut summaries: HashMap<String, String> = HashMap::new();
        let mut resumed_count = 0usize;
        for (step_id, succeeded, summary) in &seeded {
            if status.contains_key(step_id) && *succeeded {
                status.insert(step_id.clone(), StepStatus::Succeeded);
                summaries.insert(step_id.clone(), summary.clone());
                resumed_count += 1;
            }
        }
        if !seeded.is_empty() {
            append_plan_event(
                &log_path,
                "plan_resumed",
                json!({
                    "plan_id": plan_id,
                    "inherited": resumed_count,
                    "journal_entries": seeded.len(),
                    "note": "succeeded 步骤已从 journal 继承，failed/skipped 回到 Pending"
                }),
            );
        }
        // Layer 3：升级重放——已解答的升级把答案注入步骤任务并强制重跑；
        // 未决的升级把步骤标 skipped（报告提示先 ResolveWorkflowQuestion 再 resume）。
        let (step_answers, awaiting_input) = apply_escalation_seeding(&mut status, &escalations);
        for (step_id, question_id) in &awaiting_input {
            append_plan_event(
                &log_path,
                "step_skipped",
                json!({"step": step_id, "reason": format!("awaiting orchestrator answer for {question_id}")}),
            );
        }
        // Layer 3 对抗环（Programmer/Prover）：verified_by 声明验证关系；
        // verdict 轮次预算从 journal 续算（跨 resume 持久），失败摘要注入重跑。
        let verifier_target: HashMap<String, String> = steps
            .iter()
            .filter_map(|step| {
                step.verified_by
                    .as_ref()
                    .map(|verifier| (verifier.clone(), step.id.clone()))
            })
            .collect();
        let max_verdict_rounds = input
            .get("max_verdict_rounds")
            .and_then(Value::as_u64)
            .map(|value| value.clamp(1, MAX_VERDICT_ROUNDS as u64) as u32)
            .unwrap_or(DEFAULT_VERDICT_ROUNDS);
        let mut verdict_feedback: HashMap<String, String> = HashMap::new();
        let mut verdict_rounds: HashMap<String, u32> = HashMap::new();
        seed_verdict_replay(
            &journal_verdicts,
            &verifier_target,
            max_verdict_rounds,
            &mut status,
            &mut verdict_feedback,
            &mut verdict_rounds,
        );

        loop {
            // 失败传染：依赖未全部成功的待执行步骤标 skipped（无重试是显式策略）。
            for step in &steps {
                if status.get(&step.id) == Some(&StepStatus::Pending) {
                    let failed_dep = step.depends_on.iter().find(|dep| {
                        matches!(
                            status.get(*dep),
                            Some(StepStatus::Failed) | Some(StepStatus::Skipped)
                        )
                    });
                    if let Some(dep) = failed_dep {
                        status.insert(step.id.clone(), StepStatus::Skipped);
                        append_plan_event(
                            &log_path,
                            "step_skipped",
                            json!({"step": step.id, "reason": format!("dependency '{dep}' did not succeed")}),
                        );
                    }
                }
            }
            // 就绪波次：依赖全部成功的 Pending 步骤并发执行（信号量限流）。
            // 验证步骤只在被验证目标处于 Verifying 时上场（对抗环调度）。
            let wave: Vec<&PlanStep> = steps
                .iter()
                .filter(|step| {
                    if status.get(&step.id) != Some(&StepStatus::Pending) {
                        return false;
                    }
                    if !step
                        .depends_on
                        .iter()
                        .all(|dep| status.get(dep) == Some(&StepStatus::Succeeded))
                    {
                        return false;
                    }
                    match verifier_target.get(&step.id) {
                        Some(target) => status.get(target) == Some(&StepStatus::Verifying),
                        None => true,
                    }
                })
                .collect();
            if wave.is_empty() {
                break;
            }
            let semaphore = Arc::new(tokio::sync::Semaphore::new(max_parallel));
            let futures = wave.into_iter().map(|step| {
                let runtime = self.runtime.clone();
                let semaphore = Arc::clone(&semaphore);
                let dep_summaries: Vec<(String, String)> = step
                    .depends_on
                    .iter()
                    .map(|dep| (dep.clone(), summaries.get(dep).cloned().unwrap_or_default()))
                    .collect();
                let step_escalations =
                    step_answers.get(&step.id).cloned().unwrap_or_default();
                let mut spec = plan_step_spec(
                    &plan_name,
                    plan_id.clone(),
                    step,
                    &dep_summaries,
                    &step_escalations,
                );
                // 验证步骤：注入对抗协议 + 目标最新产出摘要 + 通过标准。
                if let Some(target_id) = verifier_target.get(&step.id) {
                    let target_summary =
                        summaries.get(target_id).cloned().unwrap_or_default();
                    let pass_condition = steps
                        .iter()
                        .find(|candidate| &candidate.id == target_id)
                        .and_then(|target| target.pass_condition.clone())
                        .unwrap_or_else(|| "以被验证步骤的任务文本为准".to_string());
                    spec.task.push_str(&format!(
                        "\n\n## Verification mission\n你是步骤 {target_id} 的验证者（Prover）。\
                         被验证步骤的产出摘要：\n{target_summary}\n\n\
                         通过标准：{pass_condition}\n\
                         只验证、不修复。必须在回复中给出 JSON verdict（正文第一个 {{ 到最后一个 }}）：\n\
                         {{\"pass\": true|false, \"reasons\": [\"...\"]}}\n\
                         pass=false 时 reasons 必须给出可操作的修复方向；输出不规范视为未通过。"
                    ));
                }
                // 被验证步骤重跑：注入上一轮 verdict 的失败摘要。
                if let Some(feedback) = verdict_feedback.get(&step.id) {
                    spec.task.push_str(&format!(
                        "\n\n## Verification feedback（上一轮产出被验证者否决）\n失败原因：{feedback}\n\
                         请针对性修复，不要重复已失败的做法。"
                    ));
                }
                async move {
                    let _permit = semaphore.acquire().await.expect("plan semaphore 不会关闭");
                    let result = runtime.run_task(spec, None, None).await;
                    (step.id.clone(), result)
                }
            });
            for (step_id, result) in join_all(futures).await {
                let failed_run = result.is_error || result.cancelled;
                let summary = if failed_run {
                    result.content.clone()
                } else {
                    result.handoff.summary
                };
                // 验证步骤：解析 verdict 并驱动对抗环。
                if let Some(target_id) = verifier_target.get(&step_id).cloned() {
                    let (pass, reasons) = if failed_run {
                        (
                            false,
                            vec![format!(
                                "验证步骤执行失败: {}",
                                truncate_chars(&summary, 200)
                            )],
                        )
                    } else {
                        parse_verdict(&result.content)
                    };
                    let round = verdict_rounds.get(&target_id).copied().unwrap_or(0) + 1;
                    verdict_rounds.insert(target_id.clone(), round);
                    summaries.insert(step_id.clone(), truncate_chars(&summary, 400));
                    if pass {
                        status.insert(target_id.clone(), StepStatus::Succeeded);
                        status.insert(step_id.clone(), StepStatus::Succeeded);
                        append_plan_event(
                            &log_path,
                            "verdict_passed",
                            json!({"verifier": step_id, "target": target_id, "round": round}),
                        );
                    } else if round >= max_verdict_rounds {
                        status.insert(target_id.clone(), StepStatus::Failed);
                        status.insert(step_id.clone(), StepStatus::Failed);
                        append_plan_event(
                            &log_path,
                            "verdict_failed",
                            json!({"verifier": step_id, "target": target_id, "round": round, "exhausted": true, "reasons": reasons}),
                        );
                    } else {
                        verdict_feedback.insert(target_id.clone(), reasons.join("; "));
                        status.insert(target_id.clone(), StepStatus::Pending);
                        status.insert(step_id.clone(), StepStatus::Pending);
                        append_plan_event(
                            &log_path,
                            "verdict_failed",
                            json!({"verifier": step_id, "target": target_id, "round": round, "reasons": reasons}),
                        );
                    }
                    continue;
                }
                // 被验证步骤：执行成功 → Verifying（等待 verdict），依赖方暂不消费。
                let is_verified_target = steps
                    .iter()
                    .find(|step| step.id == step_id)
                    .and_then(|step| step.verified_by.as_ref())
                    .is_some();
                if !failed_run && is_verified_target {
                    status.insert(step_id.clone(), StepStatus::Verifying);
                    summaries.insert(step_id.clone(), truncate_chars(&summary, 400));
                    append_plan_event(
                        &log_path,
                        "step_finished",
                        json!({"step": step_id, "outcome": "succeeded", "awaiting_verdict": true, "summary": truncate_chars(&summary, 400)}),
                    );
                    continue;
                }
                status.insert(
                    step_id.clone(),
                    if failed_run {
                        StepStatus::Failed
                    } else {
                        StepStatus::Succeeded
                    },
                );
                summaries.insert(step_id.clone(), summary.clone());
                append_plan_event(
                    &log_path,
                    "step_finished",
                    json!({"step": step_id, "outcome": if failed_run { "failed" } else { "succeeded" }, "summary": truncate_chars(&summary, 400)}),
                );
            }
        }

        let succeeded = status
            .values()
            .filter(|s| **s == StepStatus::Succeeded)
            .count();
        let failed = status
            .values()
            .filter(|s| **s == StepStatus::Failed)
            .count();
        let skipped = status
            .values()
            .filter(|s| **s == StepStatus::Skipped)
            .count();
        append_plan_event(
            &log_path,
            "plan_completed",
            json!({"succeeded": succeeded, "failed": failed, "skipped": skipped}),
        );
        let mut report = format!(
            "计划 '{plan_name}'（{plan_id}）执行完毕：{succeeded} 成功 / {failed} 失败 / {skipped} 跳过{}。\n事件日志：{}\n",
            if resumed_count > 0 {
                format!("（从 journal 继承 {resumed_count} 步，未重跑）")
            } else {
                String::new()
            },
            log_path.display(),
        );
        // Layer 3 对抗环轮次汇总（journal 重放视角）。
        if !verdict_rounds.is_empty() {
            report.push_str("

## 对抗环（Programmer/Prover）");
            for (target_id, round) in &verdict_rounds {
                let verifier_id = verifier_target
                    .iter()
                    .find(|(_, target)| **target == *target_id)
                    .map(|(verifier, _)| verifier.clone())
                    .unwrap_or_default();
                // 轮次与终态以本地执行状态为准（fresh 与 resume 同一口径）。
                let verdict_text = match status.get(target_id) {
                    Some(StepStatus::Succeeded) => format!("第 {round} 轮通过"),
                    Some(StepStatus::Failed) => {
                        format!("{round} 轮后仍未通过（轮次耗尽）")
                    }
                    Some(StepStatus::Pending) | Some(StepStatus::Verifying) => {
                        format!("第 {round} 轮进行中")
                    }
                    _ => format!("{round} 轮"),
                };
                report.push_str(&format!(
                    "
- {target_id} ⟷ {verifier_id}: {verdict_text}"
                ));
            }
        }
        let open_escalations = open_escalations_from_journal(&log_path);
        if !open_escalations.is_empty() {
            report.push_str("

⏸ 计划存在待应答的升级提问（子代理拒绝编造答案；请逐条 ResolveWorkflowQuestion                              {question_id, answer} 应答，再用 PlanRun resume_plan_id 恢复）：");
            for (question_id, step_id, question) in &open_escalations {
                report.push_str(&format!(
                    "
- [{step_id}] {question_id}: {}",
                    truncate_chars(question, 300)
                ));
            }
        }
        if !awaiting_input.is_empty() {
            report.push_str("

以下步骤因等待编排者答案在本次恢复中被跳过：");
            for (step_id, question_id) in &awaiting_input {
                report.push_str(&format!("
- {step_id} [awaiting_input]: {question_id}"));
            }
        }
        for step in &steps {
            let label = match status.get(&step.id) {
                Some(StepStatus::Succeeded) => "succeeded",
                Some(StepStatus::Failed) => "failed",
                Some(StepStatus::Skipped) => "skipped",
                _ => "not_run",
            };
            let summary = summaries
                .get(&step.id)
                .map(|summary| truncate_chars(summary, 300))
                .unwrap_or_else(|| match label {
                    "skipped" => "依赖步骤未成功".to_string(),
                    _ => "无结果".to_string(),
                });
            report.push_str(&format!("\n- {} [{}]: {}", step.id, label, summary));
        }
        report.push_str(&format!(
            "\n\n处置提示：失败步骤不会自动重试。如需恢复或重试，用相同的 steps 重新提交并带上 \
             resume_plan_id=\"{plan_id}\"（已成功步骤按 id 继承结果、只重跑失败/跳过步骤，\
             失败步骤的任务文本可以修改）；单点补救则用 Agent/Task 重发单步。skipped 步骤的依赖未成功。"
        ));
        ToolResult::ok(report)
    }
}

/// 由计划步骤构造子代理任务：依赖摘要与已解答的升级答案并入任务文本，
/// 编码类步骤沿用 candidate_patch 隔离；plan_id/step_id 注入 spec 供 escalate 定位 journal。
fn plan_step_spec(
    plan_name: &str,
    plan_id: String,
    step: &PlanStep,
    dep_summaries: &[(String, String)],
    escalations: &[(String, String)],
) -> AgentTaskSpec {
    let mut task = step.task.clone();
    if !dep_summaries.is_empty() {
        task.push_str("\n\n## Upstream step results\n");
        for (dep_id, summary) in dep_summaries {
            let summary = if summary.trim().is_empty() {
                "（上游未给出摘要）"
            } else {
                summary.trim()
            };
            task.push_str(&format!(
                "\n### {dep_id}\n{}\n",
                truncate_chars(summary, DEP_SUMMARY_MAX_CHARS)
            ));
        }
        task.push_str("\n上游结果是事实输入：与其矛盾时以最新证据为准并在结论中说明。\n");
    }
    if !escalations.is_empty() {
        task.push_str("\n## Orchestrator answers (resolved escalations)\n");
        for (question, answer) in escalations {
            task.push_str(&format!(
                "\n### Q: {}\nA: {}（编排者输入，视为最新事实依据）\n",
                truncate_chars(question, 500),
                truncate_chars(answer, DEP_SUMMARY_MAX_CHARS)
            ));
        }
    }
    AgentTaskSpec {
        name: format!("{plan_name}-{}", step.id),
        role: step.role.clone(),
        task,
        context: format!("deterministic plan '{plan_name}'"),
        team_name: None,
        subagent_type: None,
        split_reason: None,
        source_channel: String::new(),
        origin_chat_id: String::new(),
        model: None,
        plan_id: Some(plan_id),
        step_id: Some(step.id.clone()),
        isolation: if looks_like_complex_coding_task(&step.task) {
            "candidate_patch".to_string()
        } else {
            "shared_cas".to_string()
        },
        max_tool_iter: PLAN_STEP_MAX_TOOL_ITER,
        max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_plan_journal_takes_latest_result_per_step() {
        let dir = std::env::temp_dir().join(format!(
            "rclaw-plan-journal-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plan-test.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"event\":\"plan_started\",\"steps\":[\"a\",\"b\"]}\n",
                "{\"event\":\"step_finished\",\"step\":\"a\",\"outcome\":\"failed\",\"summary\":\"第一次失败\"}\n",
                "{\"event\":\"step_finished\",\"step\":\"a\",\"outcome\":\"succeeded\",\"summary\":\"重跑成功\"}\n",
                "{\"event\":\"step_finished\",\"step\":\"b\",\"outcome\":\"failed\",\"summary\":\"依赖缺失\"}\n",
                "不是 JSON 的损坏行\n",
            ),
        )
        .unwrap();
        let journal = load_plan_journal(&path).unwrap();
        assert_eq!(journal.steps.len(), 2);
        let a = journal.steps.iter().find(|(id, _, _)| id == "a").unwrap();
        assert!(a.1, "a 应取最后一次结果（succeeded）");
        assert_eq!(a.2, "重跑成功");
        let b = journal.steps.iter().find(|(id, _, _)| id == "b").unwrap();
        assert!(!b.1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_plan_journal_rejects_missing_or_empty_logs() {
        let dir = std::env::temp_dir().join(format!(
            "rclaw-plan-journal-missing-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let missing = dir.join("nope.jsonl");
        assert!(load_plan_journal(&missing).is_err(), "日志不存在应报错");
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("empty.jsonl");
        std::fs::write(&empty, "").unwrap();
        assert!(load_plan_journal(&empty).is_err(), "无 step_finished 应报错");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn parse_verdict_extracts_pass_and_reasons() {
        // 正文内嵌 JSON（第一个 { 到最后一个 }）。
        let (pass, reasons) = parse_verdict(
            "验证结论如下 {\"pass\": true, \"reasons\": [\"全部满足\"]} 请查收",
        );
        assert!(pass);
        assert_eq!(reasons, vec!["全部满足".to_string()]);

        // fail 且给出 reasons。
        let (pass, reasons) = parse_verdict(
            "{\"pass\": false, \"reasons\": [\"边界条件未覆盖\", \"测试未跑\"]}",
        );
        assert!(!pass);
        assert_eq!(reasons.len(), 2);

        // fail 无 reasons → 注入兜底提示。
        let (pass, reasons) = parse_verdict("{\"pass\": false}");
        assert!(!pass);
        assert!(reasons[0].contains("未给出 reasons"));

        // 提取失败 / 解析失败 / 缺 pass 字段 → 一律 fail（验证者输出不规范 ≠ 通过）。
        for (index, content) in [
            "没有任何 JSON",
            "{not json",
            "{\"reasons\": []}",
        ]
        .iter()
        .enumerate()
        {
            let (pass, reasons) = parse_verdict(content);
            assert!(!pass, "case {index} 应判 fail");
            assert!(!reasons.is_empty(), "case {index} 应给出原因");
        }
    }

    #[test]
    fn parse_steps_rejects_invalid_verification_relations() {
        let merge = |mut base: Value, extra: Value| {
            if let (Some(target), Some(source)) =
                (base.as_object_mut(), extra.as_object())
            {
                for (key, value) in source {
                    target.insert(key.clone(), value.clone());
                }
            }
            base
        };
        let base = |extra_a: Value, extra_b: Value| {
            json!({
                "steps": [
                    merge(json!({"id": "a", "task": "t"}), extra_a),
                    merge(json!({"id": "b", "task": "t"}), extra_b),
                ]
            })
        };
        // 自引用。
        let error = parse_steps(&base(
            json!({"verified_by": "a"}),
            json!({}),
        )).unwrap_err();
        assert!(error.contains("不能指向自己"), "{error}");
        // 指向不存在的步骤。
        let error = parse_steps(&base(
            json!({"verified_by": "ghost"}),
            json!({}),
        )).unwrap_err();
        assert!(error.contains("不存在"), "{error}");
        // 链式验证（验证者自己也声明 verified_by）。
        let error = parse_steps(&base(
            json!({"verified_by": "b"}),
            json!({"verified_by": "a"}),
        )).unwrap_err();
        assert!(error.contains("一层验证关系"), "{error}");
        // 验证者 depends_on 被验证者（死锁）。
        let error = parse_steps(&base(
            json!({"verified_by": "b"}),
            json!({"depends_on": ["a"]}),
        )).unwrap_err();
        assert!(error.contains("死锁"), "{error}");
    }

    #[test]
    fn verdict_replay_seeds_resume_state() {
        let verifier_target = HashMap::from([("prove".to_string(), "impl".to_string())]);
        let record = |round: u32, passed: bool| VerdictRecord {
            verifier: "prove".to_string(),
            target: "impl".to_string(),
            round,
            passed,
            reasons: vec!["实现缺少测试".to_string()],
        };
        // 未耗尽的失败：双方回 Pending + feedback + 轮次续算。
        let mut status = HashMap::from([("impl".to_string(), StepStatus::Succeeded), ("prove".to_string(), StepStatus::Succeeded)]);
        let mut feedback = HashMap::new();
        let mut rounds = HashMap::new();
        seed_verdict_replay(&[record(1, false)], &verifier_target, 2, &mut status, &mut feedback, &mut rounds);
        assert_eq!(status["impl"], StepStatus::Pending);
        assert_eq!(status["prove"], StepStatus::Pending);
        assert_eq!(rounds["impl"], 1);
        assert!(feedback["impl"].contains("实现缺少测试"));
        // 轮次耗尽：双双 Failed。
        let mut status = HashMap::new();
        let mut feedback = HashMap::new();
        let mut rounds = HashMap::new();
        seed_verdict_replay(&[record(1, false), record(2, false)], &verifier_target, 2, &mut status, &mut feedback, &mut rounds);
        assert_eq!(status["impl"], StepStatus::Failed);
        assert_eq!(status["prove"], StepStatus::Failed);
        assert_eq!(rounds["impl"], 2);
        // 已通过：维持继承，不再扰动。
        let mut status = HashMap::from([("impl".to_string(), StepStatus::Succeeded), ("prove".to_string(), StepStatus::Succeeded)]);
        let mut feedback = HashMap::new();
        let mut rounds = HashMap::new();
        seed_verdict_replay(&[record(1, false), record(2, true)], &verifier_target, 2, &mut status, &mut feedback, &mut rounds);
        assert_eq!(status["impl"], StepStatus::Succeeded);
        assert_eq!(status["prove"], StepStatus::Succeeded);
        assert!(feedback.is_empty());
    }

}
