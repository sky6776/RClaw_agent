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

#[derive(Debug, Clone)]
struct PlanStep {
    id: String,
    task: String,
    role: String,
    depends_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum StepStatus {
    Pending,
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
    Ok(steps)
}

fn append_plan_event(log_path: &Path, event: &str, payload: Value) {
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
         plans. Failed steps are reported, not retried; you decide how to follow up."
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
                            "depends_on": {"type": "array", "items": {"type": "string"}, "description": "Step ids that must succeed before this step runs."}
                        },
                        "required": ["id", "task"]
                    }
                },
                "max_parallel": {"type": "integer", "minimum": 1, "maximum": 4}
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
        let plan_id = next_execution_id(&plan_name);
        let log_path = self
            .runtime
            .layout
            .base
            .join("plans")
            .join(format!("{plan_id}.jsonl"));
        append_plan_event(
            &log_path,
            "plan_started",
            json!({"plan_id": plan_id, "name": plan_name, "steps": steps.iter().map(|s| &s.id).collect::<Vec<_>>(), "max_parallel": max_parallel}),
        );

        let mut status: HashMap<String, StepStatus> = steps
            .iter()
            .map(|step| (step.id.clone(), StepStatus::Pending))
            .collect();
        let mut summaries: HashMap<String, String> = HashMap::new();

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
            let wave: Vec<&PlanStep> = steps
                .iter()
                .filter(|step| {
                    status.get(&step.id) == Some(&StepStatus::Pending)
                        && step
                            .depends_on
                            .iter()
                            .all(|dep| status.get(dep) == Some(&StepStatus::Succeeded))
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
                let spec = plan_step_spec(&plan_name, step, &dep_summaries);
                async move {
                    let _permit = semaphore.acquire().await.expect("plan semaphore 不会关闭");
                    let result = runtime.run_task(spec, None, None).await;
                    (step.id.clone(), result)
                }
            });
            for (step_id, result) in join_all(futures).await {
                status.insert(
                    step_id.clone(),
                    if result.is_error || result.cancelled {
                        StepStatus::Failed
                    } else {
                        StepStatus::Succeeded
                    },
                );
                let summary = if result.is_error || result.cancelled {
                    result.content
                } else {
                    result.handoff.summary
                };
                summaries.insert(step_id.clone(), summary.clone());
                append_plan_event(
                    &log_path,
                    "step_finished",
                    json!({"step": step_id, "outcome": if result.is_error || result.cancelled { "failed" } else { "succeeded" }, "summary": truncate_chars(&summary, 400)}),
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
            "计划 '{plan_name}'（{plan_id}）执行完毕：{succeeded} 成功 / {failed} 失败 / {skipped} 跳过。\n事件日志：{}\n",
            log_path.display(),
        );
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
        report.push_str(
            "\n\n处置提示：失败步骤不会自动重试；请阅读失败摘要后决定重发单步（Agent/Task）、\
             修改计划或亲自完成。skipped 步骤的依赖未成功。",
        );
        ToolResult::ok(report)
    }
}

/// 由计划步骤构造子代理任务：依赖摘要并入任务文本，编码类步骤沿用 candidate_patch 隔离。
fn plan_step_spec(
    plan_name: &str,
    step: &PlanStep,
    dep_summaries: &[(String, String)],
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
    AgentTaskSpec {
        name: format!("{plan_name}-{}", step.id),
        role: step.role.clone(),
        task,
        context: format!("deterministic plan '{plan_name}'"),
        team_name: None,
        subagent_type: None,
        split_reason: None,
        source_channel: String::new(),
        isolation: if looks_like_complex_coding_task(&step.task) {
            "candidate_patch".to_string()
        } else {
            "shared_cas".to_string()
        },
        max_tool_iter: PLAN_STEP_MAX_TOOL_ITER,
        max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
    }
}
