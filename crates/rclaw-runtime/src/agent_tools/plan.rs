//! PlanRun：确定性计划工作流（plan 工件 + 依赖序执行）。
//!
//! RClaw 的团队编排是 LLM 驱动的（主循环用 Agent/Task/看板工具自行组织），适合开放
//! 任务；但对 cron 型固定流程（每日报告、定期巡检），「确定性骨架 + LLM 节点」更省
//! 更稳：本工具让主代理一次性给出 plan 工件（步骤 + 依赖），执行器按拓扑序确定地
//! 跑完——并行波次、失败传染为 skipped、不盲目重试（只有对抗环 verdict 驱动的有界重跑
//! 与升级应答后的 resume 重跑）、逐步留事件日志（对齐 JiuwenSwarm
//! Swarmflow 的「图结构是代码、LLM 只在节点内」哲学，但收敛为最小工件：JSON 步骤表
//! 而非脚本引擎）。步骤本体复用 [`AgentToolRuntime::run_task`]，因此继承子代理的全部
//! 门禁（熔断 / 三层完成验证 / candidate_patch 隔离 / 推理错峰 / 沙箱 fail-safe）。
//!
//! 步骤内 `escalate` 提问是一等终态：本轮提出且未应答的步骤判 skipped
//! （`awaiting_escalation`），不交给验证者、不被依赖方当事实消费；编排者应答后
//! resume 带答案重跑。
//!
//! 与 Agent/Task 的分工：单点委派用 Agent/Task；「已知流程、多步、有依赖」用 PlanRun。
//! 子代理内禁调 PlanRun（[`is_subagent_blocked_tool`]），计划不可嵌套。

use super::*;
use futures::future::join_all;
use std::collections::HashSet;

/// 计划的步骤数上限：计划应是确定的中等流程，不是任务分解的黑洞。
const MAX_PLAN_STEPS: usize = 12;
/// 单步任务文本上限（字符）；pass_condition 同限。
const MAX_STEP_TASK_CHARS: usize = 4_000;
/// 单步子代理的工具迭代上限（比 Agent 默认 256 收敛：计划步骤应当聚焦）。
const PLAN_STEP_MAX_TOOL_ITER: usize = 64;
/// 依赖摘要注入单步 prompt 的字符上限；journal 的 step_finished.summary 同限
/// （内存里保留全文，只在注入与落盘时截断）。
const DEP_SUMMARY_MAX_CHARS: usize = 1_500;
/// verdict reasons 的单条 / 总量字符上限（注入 journal 与重跑任务文本前收敛）。
const MAX_VERDICT_REASON_CHARS: usize = 300;
const MAX_VERDICT_REASONS_TOTAL_CHARS: usize = 1_500;
/// 默认 / 最大并行步骤数（第十二轮 2/4 → 4/8：唤醒合批消除通知放大后，波次上限是
/// 大规模并行的直接瓶颈；步骤仍共享子代理推理错峰 `subagent_request_stagger`）。
const DEFAULT_MAX_PARALLEL: usize = 4;
const MAX_PARALLEL_STEPS: usize = 8;
/// 对抗环 verdict 失败轮次上限（默认 / 硬顶）。`while (!verdict.pass)` 的有界化：
/// 每轮失败摘要注入目标重跑，超过上限即判 Failed（依赖方按既有策略传染 skipped）。
/// 轮次按【对抗周期】计：一次 pass 收口一个周期；目标之后因编排者答案重跑（旧 pass 被
/// `superseded`）开启新周期、预算从零起算——旧周期里的失败不吃新周期的额度。周期内
/// pass 之前的 verdict 全是 fail，所以「周期内第 n 次 verdict」就是「已失败 n-1 次」。
const DEFAULT_VERDICT_ROUNDS: u32 = 2;
const MAX_VERDICT_ROUNDS: u32 = 4;
/// 注入验证者任务的目标产出简报上限（字符）。fresh 与 resume 必须看到同一份：首跑时简报
/// 就按这个上限裁好、原样写进目标的 awaiting_verdict 记录，resume 补跑验证者时从记录里
/// 取回——否则首跑的验证者看全文、resume 的验证者只看 journal 里 1500 字的依赖摘要，
/// 同一份产出两种判法。
const VERIFICATION_BRIEF_MAX_CHARS: usize = 12_000;
/// 简报里 handoff 列表（evidence / artifacts / validation / open_risks）的条数与单条上限。
const BRIEF_LIST_MAX_ITEMS: usize = 20;
const BRIEF_ITEM_MAX_CHARS: usize = 400;

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

/// 计划暂停标志的 kv 键（存在 = 暂停；`plan_pause` 命令 / 桌面按钮写入，
/// 执行器在波次边界读取——跨进程一致的协作式暂停）。
pub(crate) fn plan_pause_key(plan_id: &str) -> String {
    format!("plan-pause/{plan_id}")
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
        let verified_by = raw
            .get("verified_by")
            .and_then(Value::as_str)
            .map(sanitize_step_id)
            .transpose()
            .map_err(|error| format!("steps[{idx}].verified_by: {error}"))?;
        let pass_condition = raw
            .get("pass_condition")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_string);
        if let Some(condition) = &pass_condition {
            if verified_by.is_none() {
                return Err(format!(
                    "steps[{idx}].pass_condition 必须与 verified_by 同时声明（没有验证者就没人执行通过标准）"
                ));
            }
            if condition.chars().count() > MAX_STEP_TASK_CHARS {
                return Err(format!(
                    "steps[{idx}].pass_condition 超过 {MAX_STEP_TASK_CHARS} 字符"
                ));
            }
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
            verified_by,
            pass_condition,
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
    // Layer 3 对抗环规则：验证关系必须是一层、无自引用、无依赖死锁、一个验证者只验一个目标
    // （两个目标共用验证者时，验证者只会为其中一个上场，另一个永远停在 Verifying）。
    // 先于环检测：隐式边「验证者依赖被验证者」要建立在这些关系成立之上。
    let by_id: HashMap<&String, &PlanStep> = steps.iter().map(|s| (&s.id, s)).collect();
    let mut target_of_verifier: HashMap<&String, &String> = HashMap::new();
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
        if let Some(other) = target_of_verifier.insert(verifier_id, &step.id) {
            return Err(format!(
                "验证步骤 '{verifier_id}' 同时被 '{other}' 与 '{}' 声明为 verified_by：\
                 一个验证者只能验证一个步骤，请为每个被验证步骤各设一个验证者",
                step.id
            ));
        }
    }
    // 环检测（Kahn）。被验证步骤 S 要等验证者 V 判 pass 才算 Succeeded，所以依赖 S 的
    // 步骤 X 运行期实际等的是 V：图里的边 = S→V（隐式「验证者依赖被验证者」）+ 每条
    // depends_on 边按「有效前驱」记——X 依赖 D，D 若有验证者 V(D) 则记 V(D)→X，否则
    // D→X。这样「目标直接或间接依赖自己的验证者」和「验证者直接或间接依赖某个依赖
    // 目标的步骤」这两类运行期必然卡在 Verifying 的计划都会被判成环。
    // 能拓扑处理的步骤数 < 总数 → 有环。
    let mut successors: HashMap<&String, Vec<&String>> = HashMap::new();
    let mut indegree: HashMap<&String, usize> = steps.iter().map(|step| (&step.id, 0)).collect();
    for step in &steps {
        for dep in &step.depends_on {
            let predecessor: &String = by_id
                .get(dep)
                .and_then(|target| target.verified_by.as_ref())
                .unwrap_or(dep);
            successors.entry(predecessor).or_default().push(&step.id);
            if let Some(degree) = indegree.get_mut(&step.id) {
                *degree += 1;
            }
        }
        if let Some(verifier_id) = &step.verified_by {
            successors.entry(&step.id).or_default().push(verifier_id);
            if let Some(degree) = indegree.get_mut(verifier_id) {
                *degree += 1;
            }
        }
    }
    let mut queue: Vec<&String> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut processed = 0usize;
    while let Some(current) = queue.pop() {
        processed += 1;
        let Some(next) = successors.get(current) else {
            continue;
        };
        for successor in next {
            if let Some(degree) = indegree.get_mut(successor) {
                *degree -= 1;
                if *degree == 0 {
                    queue.push(successor);
                }
            }
        }
    }
    if processed != steps.len() {
        return Err(
            "steps 存在依赖环（depends_on 必须构成 DAG；verified_by 隐含「验证者依赖被验证者」\
             且「依赖被验证步骤 = 等它的验证者判 pass」：被验证步骤不能直接或间接 depends_on \
             自己的验证者，验证者也不能直接或间接 depends_on 任何依赖被验证步骤的步骤）"
                .to_string(),
        );
    }
    Ok(steps)
}

/// 追加一条计划事件（`state.sqlite` journal，stream = plan_id）；写失败只告警。
///
/// 每条事件是一行独立的 SQLite 记录，并发追加者（步骤内 escalate 与执行器同时写）
/// 由事务串行化，不会像文件追加那样交错成碎片。进度类事件（step_started /
/// step_finished / verdict …）丢失的后果是 resume 时多重跑一步——安全方向，不阻断执行；
/// 决定 resume 语义的事件用 [`try_append_plan_event`]。
pub(super) fn append_plan_event(plan_log: &EventLog, event: &str, payload: Value) {
    if let Err(error) = try_append_plan_event(plan_log, event, payload) {
        tracing::warn!("{error}");
    }
}

/// 追加一条计划事件并把写失败交给调用方（`step_escalated` / `escalation_resolved`：
/// 写不进 journal 就等于没提问 / 没应答，调用方必须拒绝而不是假装成功）。
pub(super) fn try_append_plan_event(
    plan_log: &EventLog,
    event: &str,
    payload: Value,
) -> Result<u64, String> {
    plan_log.append(&plan_event_record(event, payload))
}

/// 计划事件的落库形态：`ts` + `event` + payload 的各字段（payload 不是对象时只留前两者）。
pub(super) fn plan_event_record(event: &str, payload: Value) -> Value {
    let mut record = json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "event": event,
    });
    if let (Some(target), Some(source)) = (record.as_object_mut(), payload.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    record
}

/// 一条计划步骤的升级记录（Layer 3 重放素材）。
#[derive(Debug)]
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
    /// 目标步骤在该 verdict 之后又有 step_finished 记录（中断后重跑 / 升级答案触发重跑）：
    /// verdict 不再描述当前产出，只有轮次计数仍然有效。
    pub(super) superseded: bool,
}

/// 计划事件日志的解析结果。
pub(super) struct PlanJournal {
    pub(super) steps: Vec<(String, bool, String)>,
    /// 仍需处理的升级：未应答，或已应答但步骤尚未在应答之后 succeeded（答案未被消费）。
    pub(super) escalations: Vec<EscalationRecord>,
    /// 已消费的应答（步骤在应答之后已 succeeded）：不再强制重跑，但若步骤因别的原因
    /// 重跑，答案仍作为事实依据注入——否则步骤会把同一个问题再问一遍。
    pub(super) consumed_answers: Vec<EscalationRecord>,
    pub(super) verdicts: Vec<VerdictRecord>,
    /// 被验证步骤最后一次 step_finished 里的产出简报（`verification_brief`，见
    /// [`verification_brief`]）：resume 补跑验证者时注入，与首跑看到的是同一份。
    pub(super) briefs: HashMap<String, String>,
}

/// 正文的 JSON 对象扫描结果（见 `scan_json_objects`）。
struct JsonObjectScan {
    /// 所有平衡的 `{...}` 片段（字节区间，按起点排序；跳过 JSON 字符串内的花括号）。
    spans: Vec<(usize, usize)>,
    /// 扫到正文末尾仍未闭合的 JSON 对象起点。典型成因是输出被 max_tokens / max_output_chars
    /// 截断。未闭合的 `{` 会一直留在栈上，它之后的所有片段都是它的子值。
    unclosed_json_starts: Vec<usize>,
}

/// `index` 处的 `{` 是否像 JSON 对象的开头：后面首个非空白字符是 `"`（键）或 `}`（空对象）。
/// 散文里的 `见 { 下文`、代码里的 `fn main() {` 不算。
fn looks_like_json_object_start(content: &str, index: usize) -> bool {
    let rest = content[index + 1..].trim_start();
    rest.starts_with('"') || rest.starts_with('}')
}

/// `index` 处的 `{` 是否像「紧跟着键」的 JSON 对象开头：后面首个非空白字符是 `"`，且这个引号
/// 之后还跟着键的首字符（非空白、非 `,` `}` `]` `:`、非文末）。只给相位自愈用
/// （[`ScanMode::PhaseRobust`]）：字符串状态里的 `{"` 有两种可能——相位已经错位、这是真正的
/// 对象起点；或者这个 `"` 就是当前字符串自己的收尾引号（`"impl Foo {"`、`"a{"`）。合法 JSON
/// 里收尾引号之后只可能是空白 / `,` / `}` / `]` / `:` / 文末，而键的首字符不会是这些，据此
/// 区分；空键 `{""` 也算键开头。
fn looks_like_json_key_start(content: &str, index: usize) -> bool {
    let rest = content[index + 1..].trim_start();
    let Some(after_quote) = rest.strip_prefix('"') else {
        return false;
    };
    after_quote
        .chars()
        .next()
        .is_some_and(|next| !next.is_whitespace() && !matches!(next, ',' | '}' | ']' | ':'))
}

/// [`scan_json_objects`] 的扫描模式。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanMode {
    /// 严格按 JSON 字符串状态配对。给验证者输出的候选扫描用：最外层 / 「未闭合对象之后都是
    /// 子值」的语义要稳定，不能因为相位修正多出候选。
    Strict,
    /// 相位自愈：处在字符串状态时遇到「紧跟着键的 `{"`」（[`looks_like_json_key_start`]）也当
    /// 对象起点入栈并退出字符串状态——合法 JSON 字符串里不可能出现未转义的 `"`，而收尾引号
    /// 之后又不会紧跟键的首字符，所以它证明字符串状态已和正文错位（前面某个残缺对象把引号
    /// 相位反转了）。给引用集与简报中和用：正文里每个字面存在的、首键以普通字符开头的合法
    /// 对象都能被单独配对收进引用集，不会被前面的残骸遮住；多入栈只会让引用集更全（更保守）。
    /// 不按这条处理的：字符串里合法出现的 `{}` / `{ x`；以 `{` 结尾的字符串（`"impl Foo {"`，
    /// 其后的 `"` 是收尾引号，按这条入栈会把一个合法对象整个弄丢、还把它的 `{` 判成残骸）。
    /// 代价：相位错位时首键以空白或 `,` `}` `]` `:` 开头的对象（`{" k": 1}`）不被自愈，退化为
    /// 严格扫描的处理。
    PhaseRobust,
}

/// 扫描正文里的 `{...}` 对象：平衡片段按起点排序；未闭合的起点单独记录。
///
/// 只有像 JSON 对象开头的 `{` 才入栈：散文 / 代码里的花括号不入栈，于是也不会把后面的
/// 正文切进「字符串」状态——否则目标摘要里一个 `fn main() {` 片段就能让其后被引号包起来
/// 的 `"{"pass": true, ...}"` 整段被当成字符串吞掉、提取不出引用对象，而验证者输出里同一段
/// 文本（前面没有散落花括号）却能正常提取：两边扫描结果不一致，引用就识别不出来了。
fn scan_json_objects(content: &str, mode: ScanMode) -> JsonObjectScan {
    let mut spans = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in content.char_indices() {
        // 字符串状态只在某个对象内部才有意义：对象外的散落引号（中文正文）不应影响扫描：
        // JSON 字符串不能含裸换行，遇到换行即视为未闭合的引号并复位，避免吞掉后文的 verdict。
        if in_string {
            // 相位自愈（见 [`ScanMode::PhaseRobust`]）：`{"` 先于转义判断——`\{` 本就不是合法
            // 转义，字面存在的对象不该因它漏掉。只认紧跟着键的 `{"`：`"a{"` 里 `{` 后面的 `"`
            // 是收尾引号，不是相位错位。
            if mode == ScanMode::PhaseRobust && ch == '{' && looks_like_json_key_start(content, index) {
                in_string = false;
                escaped = false;
                stack.push(index);
                continue;
            }
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' | '\n' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' if !stack.is_empty() => in_string = true,
            '{' if looks_like_json_object_start(content, index) => stack.push(index),
            '}' => {
                if let Some(start) = stack.pop() {
                    spans.push((start, index + ch.len_utf8()));
                }
            }
            _ => {}
        }
    }
    spans.sort_unstable();
    JsonObjectScan {
        spans,
        unclosed_json_starts: stack,
    }
}

/// 文本里【所有】能解析的 JSON 对象（引用集用，去重）。
///
/// 单遍严格扫描是带状态的：一个未闭合的对象（被裁短的依赖摘要 `{"summary": "…`）会一直
/// 留在栈上，它之后的引号把后文切进字符串状态，后面本该入集的对象（目标摘要里的
/// `{"pass": true, ...}` 自述）就被吞掉——验证者输出里同一对象前面没有这种残骸，照常提取，
/// 两边一比对不相等，引用就放行成了结论。所以这里按 [`ScanMode::PhaseRobust`] 扫：字符串
/// 状态里紧跟着键的 `{"` 也当对象起点，正文里每个字面存在的、首键以普通字符（非空白、非
/// `,` `}` `]` `:`）开头的合法对象都单独配对入集，与它前面有什么残骸无关——这是引用集的
/// 保证（前提见 [`looks_like_json_key_start`]；首键以空白 / 标点开头的对象退化为严格处理，
/// 残骸之后才可能漏），简报中和（[`neutralize_unclosed_json`]）只负责别让残骸耗尽重扫上限，
/// 不负责「保住」任何对象。扫到未闭合对象仍从最早的那个 `{` 之后重扫，直到没有未闭合对象
/// 为止：重扫只会多找到对象，引用集越全越保守。
///
/// 每个未闭合对象触发一次对其后全文的重扫：文本是步骤产出 / 模型可控的，塞满 `{"{"{"…`
/// 这类残骸时重扫退化成平方级 CPU（几万字符就是上亿次字符迭代，同步占着执行器）。重扫
/// 次数超过 [`MAX_JSON_RESCANS`] 就放弃（`None`）：调用方按「引用集建不起来」fail-closed，
/// 而不是悄悄缩小引用集放行结论。
fn all_json_values(text: &str) -> Option<Vec<Value>> {
    let mut values: Vec<Value> = Vec::new();
    // 去重按原文片段与序列化文本做哈希：每次重扫都会重新收集残骸之后的全部片段，
    // `values.contains` 的线性比对会把「几十次重扫 × 几千个小对象」放大成上亿次 `Value` 比较。
    let mut seen_spans: HashSet<&str> = HashSet::new();
    let mut seen_values: HashSet<String> = HashSet::new();
    let mut offset = 0usize;
    let mut rescans = 0usize;
    loop {
        let slice = &text[offset..];
        let scan = scan_json_objects(slice, ScanMode::PhaseRobust);
        for (start, end) in &scan.spans {
            let fragment = &slice[*start..*end];
            if !seen_spans.insert(fragment) {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(fragment) {
                if seen_values.insert(value.to_string()) {
                    values.push(value);
                }
            }
        }
        let Some(restart) = scan.unclosed_json_starts.iter().copied().min() else {
            break;
        };
        rescans += 1;
        if rescans > MAX_JSON_RESCANS {
            return None;
        }
        // `{` 是单字节 ASCII：跳过它，从它之后重扫。
        offset += restart + 1;
    }
    Some(values)
}

/// [`all_json_values`] 的重扫次数上限（= 文本里允许的未闭合 JSON 对象数）。正常任务文本
/// 里的未闭合对象只来自被裁短的依赖摘要 / 简报，个位数；上限留足余量又把最坏情况限在
/// 「几十遍线性扫描」。
pub(super) const MAX_JSON_RESCANS: usize = 64;

/// 只留【最外层】的平衡片段（输入已按起点排序；平衡片段之间只会嵌套或互不相交）。
/// 嵌套片段是外层对象的子值——测试结果行、handoff 里转述的目标自述等——不是独立
/// verdict；外层对象里合法的内嵌 verdict 由 `verdict_fields` 经 `verdict` 键下探。
fn outermost_spans(spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let mut outer: Vec<(usize, usize)> = Vec::new();
    let mut covered_until = 0usize;
    for (start, end) in spans {
        if end <= covered_until {
            continue;
        }
        outer.push((start, end));
        covered_until = end;
    }
    outer
}

/// 对象是否长成 verdict 的样子：布尔 `pass`，且带 `reasons` 数组或只有 `pass` 一个键。
/// `{"name": "t1", "pass": true}` 这类测试结果行带布尔 pass 却不是 verdict，不能参与裁决。
fn is_verdict_shaped(object: &Value) -> bool {
    let Some(map) = object.as_object() else {
        return false;
    };
    map.get("pass").is_some_and(Value::is_boolean)
        && (map.get("reasons").is_some_and(Value::is_array) || map.len() == 1)
}

/// 在 JSON 值里找 verdict 对象：顶层 verdict 形状的对象，或 handoff 对象里嵌套的 `verdict` 对象。
fn verdict_object(value: &Value) -> Option<&Value> {
    if is_verdict_shaped(value) {
        Some(value)
    } else {
        value.get("verdict").filter(|verdict| is_verdict_shaped(verdict))
    }
}

/// 从 verdict 形状的对象里取 `pass` 与字符串 `reasons`。
fn verdict_fields(object: &Value) -> Option<(bool, Vec<String>)> {
    let pass = object.get("pass")?.as_bool()?;
    let reasons = object
        .get("reasons")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some((pass, reasons))
}

/// 验证步骤输出的解析结果。
///
/// `Malformed` 是解析器诊断——没有 verdict、形状不对、只是引用、相互矛盾、疑似截断——
/// 责任在验证者而不是目标：它不计 verdict 轮次，反馈注入验证者的重跑任务（目标重跑
/// 也改不了验证者的输出格式），与基础设施失败共用「连续两次」预算。`Verdict` 才是
/// 模型给出的结论。两者都不会默认通过。
enum VerdictParse {
    Verdict { pass: bool, reasons: Vec<String> },
    Malformed(String),
}

/// 验证者一次尝试没能产出 verdict 的原因；两种原因共用「连续两次」预算。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VerifierProblem {
    /// 基础设施失败：is_error / cancelled，没有可改的输出。
    Errored,
    /// 输出里没有可接受的 verdict（格式 / 引用 / 截断 / 无证据）。
    Malformed,
}

impl VerifierProblem {
    fn event(self) -> &'static str {
        match self {
            Self::Errored => "verifier_errored",
            Self::Malformed => "verifier_malformed",
        }
    }

    /// 预算耗尽时写进目标摘要 / 报告 / 目标反馈的措辞：按本轮连续尝试的实际组合区分：
    /// 「执行失败 + 格式不规范」的混合序列不冒充纯粹的一种。
    fn exhausted_label(attempts: &[Self]) -> &'static str {
        let errored = attempts.contains(&Self::Errored);
        let malformed = attempts.contains(&Self::Malformed);
        match (errored, malformed) {
            (true, false) => "连续两次执行失败",
            (false, true) => "连续两次未给出有效 verdict",
            _ => "连续两次未能给出 verdict（一次执行失败、一次输出不规范）",
        }
    }
}

/// 派发验证者时固定下来的输入：注入它的任务全文与目标产出简报。verdict 的引用集按这两份
/// 建（见 [`classify_verdict`]），派发前先确认建得起来（[`Self::reference_set_failure`]），
/// 收尾时用的必须是同一份——而不是收尾时再从 `summaries` 里取一份可能已不同的。
struct VerifierInputs {
    /// 被验证步骤 id。
    target: String,
    /// 验证者任务全文（`spec.task`）。
    task_text: String,
    /// 注入任务的目标产出简报。
    target_brief: String,
}

impl VerifierInputs {
    /// 哪份文本的 JSON 引用集在 [`MAX_JSON_RESCANS`] 内建不起来（`None` = 都建得起来）。两份
    /// 分开检查：诊断要指向能改它的
    /// 那一侧——简报是目标产出（重跑目标才会变），任务文本是编排者写的任务 / 依赖摘要 /
    /// 升级答案（重跑目标改变不了它们）。
    fn reference_set_failure(&self) -> Option<ReferenceSetFailure> {
        let brief_unavailable = all_json_values(&self.target_brief).is_none();
        let task_text_unavailable = all_json_values(&self.task_text).is_none();
        match (brief_unavailable, task_text_unavailable) {
            (false, false) => None,
            (true, _) => Some(ReferenceSetFailure::Brief),
            (false, true) => Some(ReferenceSetFailure::TaskText),
        }
    }
}

/// 派发前预检失败的归因（见 [`VerifierInputs::reference_set_failure`]）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReferenceSetFailure {
    /// 目标产出简报（或简报与任务文本都）建不起引用集：重跑目标会产出新简报。
    Brief,
    /// 只有验证任务文本建不起引用集：残骸来自目标任务文本 / 依赖摘要 / 通过标准 / 升级
    /// 答案，重跑目标改变不了它们，得先改任务或去掉那条依赖。
    TaskText,
}

impl ReferenceSetFailure {
    /// 注入报告与 journal 的诊断全文。
    fn diagnostic(self, verifier: &str) -> String {
        match self {
            Self::Brief => format!(
                "目标产出简报里未闭合的 JSON 对象超过 {MAX_JSON_RESCANS} 个，无法建立引用集来区分\
                 验证者的结论与转述，验证者 '{verifier}' 未派发；resume 会重跑该步骤，请让产出摘要与 \
                 handoff 列表项避免大段残缺 JSON"
            ),
            Self::TaskText => format!(
                "验证任务文本（目标任务文本 / 依赖摘要 / 通过标准 / 升级答案）里未闭合的 JSON 对象超过 \
                 {MAX_JSON_RESCANS} 个，无法建立引用集来区分验证者的结论与转述，验证者 '{verifier}' 未派发；\
                 resume 虽会重跑该步骤，但新产出改变不了这些输入——请先修改任务文本、或去掉带大段残缺 \
                 JSON 的依赖，再 resume"
            ),
        }
    }

    /// 目标摘要的前缀（报告的步骤行）。
    fn summary_prefix(self, verifier: &str) -> String {
        match self {
            Self::Brief => format!(
                "【无法验证：产出无法建立 verdict 引用集，验证者 '{verifier}' 未派发，resume 将重跑本步骤】"
            ),
            Self::TaskText => format!(
                "【无法验证：验证任务文本无法建立 verdict 引用集（任务文本 / 依赖摘要含大段残缺 JSON），\
                 验证者 '{verifier}' 未派发，需先修改任务文本或依赖再 resume】"
            ),
        }
    }

    /// journal `step_unverified.source`：建不起引用集的是哪份文本。
    fn source(self) -> &'static str {
        match self {
            Self::Brief => "brief",
            Self::TaskText => "task_text",
        }
    }
}

/// 从验证步骤的输出提取 verdict（Programmer/Prover 协议）。
///
/// 扫描正文里所有平衡的 `{...}` 片段，收集每个带布尔 `pass` 字段（顶层或 handoff
/// 对象内嵌的 `verdict`）的 JSON 对象：验证者常把结构化 handoff 与 verdict 并排输出，
/// 或在附言里带含花括号的代码，「首个 { 到末个 }」的粗跨度会把它们粘成不可解析的整体。
/// `pass` 必须是布尔（`"true"` 字符串不算）；提取/解析失败一律 `Malformed` 并给出
/// 可操作的诊断——验证者输出不规范等同于「未能证明」，绝不能默认通过。
///
/// 多个候选必须【一致】：验证者的任务文本里嵌着目标步骤的产出摘要，目标若自带一段
/// `{"pass": true, ...}`（模型习惯 / 上游注入），验证者在解释自己的 `pass: false` 时原样
/// 引用它，取「第一个」就会把否决读成通过。候选 pass 值不一致 → `Malformed`。
///
/// 「一致」规则只在验证者自己的 verdict 也被解析到时起作用；它恰恰缺失（被截断 / 格式不
/// 规范）时，散落的 `pass: true` 会成为唯一候选而放行。因此候选还要过几道闸：
/// - 只取最外层对象（嵌套的测试结果行 / handoff 内转述的目标自述不算，见 `outermost_spans`）；
///   落在未闭合的 JSON 对象之后的片段同样是嵌套值（外层被截断、永远不会闭合，见
///   `JsonObjectScan::unclosed_json_starts`），不是候选；
/// - 对象必须长成 verdict 的样子（见 `is_verdict_shaped`）；
/// - 与 `task_text`（注入验证者任务的全文：目标任务文本、目标产出简报、依赖摘要、通过标准、
///   升级答案）里出现过的 JSON 对象完全相同的候选视为引用而非结论——整个对象相同、或它
///   内嵌的 `verdict` 对象与引用相同都算。任务文本按 `all_json_values` 相位自愈地扫描：其中
///   被裁短的依赖摘要留下的未闭合对象不会遮住后面的目标自述；
/// - 正文带 `OUTPUT_TRUNCATED_MARKER`（按 max_output_chars 截断）时无法确认 verdict 完整，
///   直接 `Malformed`；
/// - `pass=true` 必须带至少一条实质的 reason（提示词里就是这么要求的：空数组、空串、模板
///   占位 `"..."` 都不算证据）——否则「照抄模板」就是最省事的通过方式。
///
/// 两道截断闸门故意不对称：未闭合对象只排除它【之后】的候选——它之前完整闭合的 verdict
/// 是模型在截断点之前说完的话，照常采信；`OUTPUT_TRUNCATED_MARKER` 则整段拒收——它说明
/// 模型没有自行停下，后面被裁掉的部分可能还在修正前面的结论（先 pass 再「等等，fail」），
/// 前面那个看似完整的 verdict 未必是它最终想给的。
///
/// 一个候选都不剩时：被剔除的引用若一致为 `pass=false` 且出自【目标产出简报】
/// （`target_summary`），那是验证者认同目标自述的失败，保留其 reasons 作为真实 fail；
/// 引用自别处（上游依赖的测试结果、通过标准里的示例）说明不了目标的状态，仍按 `Malformed`
/// 走验证者反馈回路。
fn classify_verdict(content: &str, task_text: &str, target_summary: &str) -> VerdictParse {
    if content.ends_with(OUTPUT_TRUNCATED_MARKER) {
        return VerdictParse::Malformed(
            "验证步骤输出超过 max_output_chars 被截断，无法确认 verdict 完整；请精简输出，只给出\
             一个独立的 {\"pass\": bool, \"reasons\": [...]} 对象和必要的说明"
                .to_string(),
        );
    }
    let scan = scan_json_objects(content, ScanMode::Strict);
    let truncated_from = scan.unclosed_json_starts.iter().copied().min();
    if scan.spans.is_empty() {
        return VerdictParse::Malformed(if truncated_from.is_some() {
            "验证步骤输出里只有未闭合的 JSON 对象（疑似被截断），没有完整的 verdict；请精简输出，\
             只给出一个独立的 {\"pass\": bool, \"reasons\": [...]} 对象"
                .to_string()
        } else {
            "验证步骤未返回 JSON verdict 对象（需 {\"pass\": bool, \"reasons\": [...]}）".to_string()
        });
    }
    // 引用集建不起来（任务文本 / 简报里未闭合的 JSON 残骸多到重扫上限）就不判：这不是
    // 验证者的输出问题，诊断里说清要改的是目标产出 / 依赖摘要那一侧。派发前已按同一对
    // 文本预检过（`VerifierInputs::reference_set_failure`，建不起来的验证者根本不上场），
    // 这里只是兜底。
    let (Some(echoed_values), Some(target_values)) =
        (all_json_values(task_text), all_json_values(target_summary))
    else {
        return VerdictParse::Malformed(format!(
            "验证任务文本 / 目标产出简报里未闭合的 JSON 对象超过 {MAX_JSON_RESCANS} 个，无法建立\
             引用集来区分结论与转述；请让目标步骤的产出摘要与依赖摘要避免大段残缺 JSON 后重跑验证"
        ));
    };
    // 被剔除的引用：(pass, reasons, 是否出自目标产出摘要)。
    let mut echoed_candidates: Vec<(bool, Vec<String>, bool)> = Vec::new();
    let mut nested_in_truncated = 0usize;
    let candidates: Vec<(bool, Vec<String>)> = outermost_spans(scan.spans)
        .into_iter()
        .filter_map(|(start, end)| {
            let value = serde_json::from_str::<Value>(&content[start..end]).ok()?;
            let object = verdict_object(&value)?;
            let fields = verdict_fields(object)?;
            if truncated_from.is_some_and(|from| start > from) {
                nested_in_truncated += 1;
                return None;
            }
            if echoed_values.contains(&value) || echoed_values.contains(object) {
                let from_target = target_values.contains(&value) || target_values.contains(object);
                echoed_candidates.push((fields.0, fields.1, from_target));
                return None;
            }
            Some(fields)
        })
        .collect();
    let Some((pass, _)) = candidates.first() else {
        if !echoed_candidates.is_empty() {
            if echoed_candidates
                .iter()
                .all(|(pass, _, from_target)| !pass && *from_target)
            {
                // 验证者原样认同目标自述的失败：结论没有歧义，保留可操作的原因。
                let reasons = merge_reasons(
                    echoed_candidates
                        .into_iter()
                        .map(|(pass, reasons, _)| (pass, reasons))
                        .collect(),
                );
                return VerdictParse::Verdict {
                    pass: false,
                    reasons: fail_reasons_or_fallback(reasons),
                };
            }
            return VerdictParse::Malformed(
                "验证步骤输出里的 JSON verdict 与注入验证任务的 JSON 对象（被验证产出摘要 / 依赖摘要 / \
                 通过标准 / 示例）完全相同，无法区分是引用还是独立结论；验证者必须给出自己的 verdict，\
                 引用被验证产出时不要原样复述其中的 JSON，pass=true 时 reasons 也要列出自己核对到的证据"
                    .to_string(),
            );
        }
        if nested_in_truncated > 0 {
            return VerdictParse::Malformed(
                "验证步骤输出里的 pass 字段都嵌在一个未闭合的 JSON 对象之内（疑似被截断），无法当作独立 \
                 verdict；请精简输出，把 {\"pass\": bool, \"reasons\": [...]} 作为独立对象给出"
                    .to_string(),
            );
        }
        return VerdictParse::Malformed(
            "验证步骤输出中没有符合 verdict 形状的 JSON 对象（需 {\"pass\": bool, \"reasons\": [...]}：\
             pass 必须是 true/false 而非字符串，且须带 reasons 数组；嵌在别的对象里的 pass、\
             或 {\"name\": ..., \"pass\": true} 这类不带 reasons 的测试结果行都不算）"
                .to_string(),
        );
    };
    let pass = *pass;
    if candidates.iter().any(|(other, _)| *other != pass) {
        return VerdictParse::Malformed(
            "验证步骤输出里出现了多个 pass 值相互矛盾的 JSON verdict 对象（既有 pass=true 又有 \
             pass=false）；验证者必须只给出一个独立的 verdict，引用被验证产出时不要原样复述其中的 JSON"
                .to_string(),
        );
    }
    let reasons = merge_reasons(candidates);
    if pass {
        if reasons.is_empty() {
            return VerdictParse::Malformed(
                "verdict 为 pass 但没有给出任何实质的 reasons；pass=true 必须附带至少一条你自己核对到的\
                 证据（跑了什么、看到了什么），不能照抄模板里的占位符"
                    .to_string(),
            );
        }
        VerdictParse::Verdict { pass, reasons }
    } else {
        VerdictParse::Verdict {
            pass,
            reasons: fail_reasons_or_fallback(reasons),
        }
    }
}

/// 一致的候选合并各自的 reasons（去重、保序；空串 / 模板占位符不算 reason）：handoff 内嵌
/// verdict 与独立 verdict 常各带一份。
fn merge_reasons(candidates: Vec<(bool, Vec<String>)>) -> Vec<String> {
    let mut reasons: Vec<String> = Vec::new();
    for (_, candidate_reasons) in candidates {
        for reason in candidate_reasons {
            if is_placeholder_reason(&reason) {
                continue;
            }
            if !reasons.contains(&reason) {
                reasons.push(reason);
            }
        }
    }
    reasons
}

/// 空白、省略号、提示词模板里的 `"..."` 占位：不是证据。
fn is_placeholder_reason(reason: &str) -> bool {
    let trimmed = reason.trim();
    trimmed.is_empty() || trimmed.chars().all(|ch| matches!(ch, '.' | '…' | '。' | '-' | '_'))
}

/// fail 而没给 reasons 时的兜底提示（目标重跑时总得有可注入的方向）。
fn fail_reasons_or_fallback(reasons: Vec<String>) -> Vec<String> {
    if reasons.is_empty() {
        vec!["verdict 为 fail 但未给出 reasons；请假设上一轮产出未满足验收标准并全面复查".to_string()]
    } else {
        reasons
    }
}

/// `classify_verdict` 的扁平视图（测试用）：`echoed` 既当任务全文也当目标摘要，
/// `Malformed` 诊断按 fail 原因返回。
#[cfg(test)]
fn parse_verdict(content: &str, echoed: &str) -> (bool, Vec<String>) {
    match classify_verdict(content, echoed, echoed) {
        VerdictParse::Verdict { pass, reasons } => (pass, reasons),
        VerdictParse::Malformed(diagnostic) => (false, vec![diagnostic]),
    }
}

/// 把 verdict reasons 收敛到可注入的体量（单条 / 总量上限）：验证者输出不受控，
/// 原样进 journal 与重跑任务文本会把两者都撑爆。
fn bound_verdict_reasons(reasons: Vec<String>) -> Vec<String> {
    let mut total = 0usize;
    let mut bounded = Vec::new();
    for reason in reasons {
        if total >= MAX_VERDICT_REASONS_TOTAL_CHARS {
            break;
        }
        let reason = truncate_text(
            reason.trim(),
            MAX_VERDICT_REASON_CHARS.min(MAX_VERDICT_REASONS_TOTAL_CHARS - total),
        );
        total += reason.chars().count();
        bounded.push(reason);
    }
    bounded
}

/// 被验证步骤的产出简报：handoff 摘要 + 它自报的证据 / 产物 / 验证 / 风险。验证者据此决定
/// 复核什么（跑哪条验证命令、看哪个产物）——光有一段摘要时它只能对着文字猜。
/// 按 [`VERIFICATION_BRIEF_MAX_CHARS`] 裁好后写进 awaiting_verdict 记录并注入验证者任务，
/// 首跑与 resume 看到的是同一份。
fn verification_brief(summary: &str, handoff: &StructuredHandoff) -> String {
    // 摘要单独中和：handoff 解析失败时它就是整段原始输出，里面的残骸再多也只影响摘要
    // 自身（最坏退回裁切摘要），不波及下面的自报列表。
    let mut brief = neutralize_unclosed_json(summary.trim());
    for (title, items) in [
        ("自报证据（evidence）", &handoff.evidence),
        ("自报产物（artifacts）", &handoff.artifacts),
        ("自报验证（validation）", &handoff.validation),
        ("自报风险（open_risks）", &handoff.open_risks),
    ] {
        let items: Vec<&str> = items
            .iter()
            .map(|item| item.trim())
            .filter(|item| !item.is_empty())
            .take(BRIEF_LIST_MAX_ITEMS)
            .collect();
        if items.is_empty() {
            continue;
        }
        brief.push_str(&format!("\n\n{title}："));
        for item in items {
            brief.push_str(&format!(
                "\n- {}",
                neutralize_unclosed_json(&truncate_text(item, BRIEF_ITEM_MAX_CHARS))
            ));
        }
    }
    // 各段都已干净、拼接用的标题 / `- ` 前缀不含花括号：这一遍只为总量截断点可能切开的
    // 那一个对象。
    neutralize_unclosed_json(&truncate_text(&brief, VERIFICATION_BRIEF_MAX_CHARS))
}

/// 中和文本里未闭合的 JSON 样式 `{`（截断点落在对象中间；或原文本就带残缺 JSON）。
///
/// 残缺对象解析不出来，留着却会在 `classify_verdict` 建引用集时各触发一次重扫：一条超长的
/// JSON 列表项被裁到 400 字就留下一个 `{"`，几十条就把 [`MAX_JSON_RESCANS`] 吃光，验证者
/// 还没上场这一对就判不了了。处理办法是把未闭合的 `{` 换成全角 `｛`，而不是从它那里把后文
/// 裁掉：正文照旧可读（验证者还能看到残缺对象里的字段名 / 后面的说明），扫描器不再把它当
/// 对象起点。早先的做法是直接裁掉后文：摘要（handoff 解析失败时就是整段原始输出）里一个
/// `{"level": "info …` 日志片段就能把摘要后文连同所有自报列表一起切没。
///
/// 中和改变的只是扫描器的入栈点，不是引用集的来源：中和前后正文里字面存在的合法对象一个
/// 不少，而 [`all_json_values`] 按相位自愈模式把每个首键以普通字符开头的合法对象都单独配对
/// 收进引用集，与它前面的残骸（或残骸被换成了 `｛`）无关。所以中和不会让简报里这样的对象从
/// 引用集里掉出来——这一点由引用集扫描保证（前提见 [`looks_like_json_key_start`]），不依赖
/// 「中和等价于重扫」之类的论证（它不成立：残骸之后的引号相位在两种扫法下可能相反）。
/// 同一前提也保证中和不会改坏合法对象：以 `{` 结尾的字符串（`"impl Foo {"`）不被当成残骸。
///
/// 从最早的未闭合起点起逐个中和（与引用集同一种扫描模式，中和干净的简报在引用集那边也
/// 干净）；超过 [`MAX_JSON_RESCANS`] 个（模型可控文本塞满 `{"…`）才退回从最早残骸处裁一刀
/// 并补截断后缀——裁出的前缀必然干净：比它更早入栈的 `{` 都已在它之前闭合，否则最早的
/// 残骸就不是它；字符串状态也只可能在某个未闭合对象之内，所以前缀末尾不在字符串里。
fn neutralize_unclosed_json(text: &str) -> String {
    let mut text = text.to_string();
    for _ in 0..MAX_JSON_RESCANS {
        let Some(start) = earliest_unclosed_json_start(&text) else {
            return text;
        };
        // `{` 是单字节 ASCII，按它切片不会落在字符中间。
        text.replace_range(start..start + 1, "｛");
    }
    match earliest_unclosed_json_start(&text) {
        Some(start) => format!("{}{TEXT_TRUNCATED_SUFFIX}", text[..start].trim_end()),
        None => text,
    }
}

/// 文本里最早一个未闭合的 JSON 样式 `{` 的字节偏移（与 [`all_json_values`] 同一种扫描模式）。
fn earliest_unclosed_json_start(text: &str) -> Option<usize> {
    scan_json_objects(text, ScanMode::PhaseRobust)
        .unclosed_json_starts
        .iter()
        .copied()
        .min()
}

/// journal 事件里的 verdict 轮次：超出 u32 的值丢弃（而不是截断成别的轮次）。
fn event_round(event: &Value) -> Option<u32> {
    event
        .get("round")
        .and_then(Value::as_u64)
        .and_then(|round| u32::try_from(round).ok())
}

/// journal 事件里的 verdict reasons（缺省为空）。
fn event_reasons(event: &Value) -> Vec<String> {
    event
        .get("reasons")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 解析计划事件日志：步骤终态（每步取最后一次 step_finished）+ 升级问答 + 对抗环 verdict。
///
/// resume 按 id 匹配、以最新结果为准（同一 plan 重跑过多次时取最后一次）；journal 的
/// `seq` 即事件先后，用来判定升级 / verdict 相对步骤终态的新旧：
/// - 应答早于步骤最后一次 succeeded → 答案已消费（进 `consumed_answers`，不再强制重跑）；
/// - 未应答的提问晚于步骤最后一次 succeeded → 该 succeeded 不可信（旧执行器把挂起当成功），
///   步骤降级为未完成；
/// - 目标在 verdict 之后又有 step_finished → verdict `superseded`。
///
/// journal 不存在（无事件）→ Err（调用方报给模型，而不是静默从零开始——错误重定向
/// 比静默重跑危险）。只有 plan_started / step_escalated 而没有任何 step_finished 的
/// journal 是合法的：首步就 escalate 的计划正是这种形态，应答后必须能 resume。
pub(super) fn load_plan_journal(plan_log: &EventLog) -> Result<PlanJournal, String> {
    let events = plan_log.load().map_err(|error| {
        format!("{error}。resume 需要一个已存在 plan 的 id（见先前 PlanRun 结果里的事件日志标识）。")
    })?;
    if events.is_empty() {
        return Err(format!(
            "计划事件日志（{}）不存在。resume 需要一个已存在 plan 的 id（见先前 PlanRun 结果里的事件日志标识）。",
            plan_log.locator()
        ));
    }
    struct Asked {
        record: EscalationRecord,
        asked_at: u64,
        resolved_at: Option<u64>,
    }
    // step → (succeeded, summary, seq)。
    let mut latest: HashMap<String, (bool, String, u64)> = HashMap::new();
    // step → 最后一次 step_finished 带的产出简报（没带就是 None：以最新记录为准，
    // 旧记录的简报不能配给新产出）。
    let mut briefs: HashMap<String, Option<String>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    // 升级问答：question_id → 记录；escalation_resolved 事后补答案（按 id 匹配）。
    let mut escalations: HashMap<String, Asked> = HashMap::new();
    let mut escalation_order: Vec<String> = Vec::new();
    let mut verdicts: Vec<(VerdictRecord, u64)> = Vec::new();
    for (line_no, event) in events {
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
                latest.insert(step.to_string(), (succeeded, summary, line_no));
                briefs.insert(
                    step.to_string(),
                    event
                        .get("verification_brief")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                );
                // 验证者的终态事件自带 verdict / target / round / reasons：`verdict_*` 与它都是
                // 宽松写入，任一条丢了另一条就是备份——否则耗尽轮次的 `verdict_failed` 一旦
                // 没写进去，resume 只看到上一轮，会在预算之外多跑一轮。同轮已有 verdict
                // 事件则不重复登记——「同轮」只在本周期内比：轮次按对抗周期从 1 重新计，
                // 新周期的第 1 轮与旧周期的第 1 轮同号，只有晚于目标最近一次 step_finished
                // 的记录才是本周期的（verdict 事件与验证者终态之间不会再夹一次目标重跑）；
                // 否则新周期丢了 `verdict_failed` 时它的备份会被旧周期的同号记录吃掉。
                if let (Some(verdict), Some(target), Some(round)) = (
                    event.get("verdict").and_then(Value::as_str),
                    event.get("target").and_then(Value::as_str),
                    event_round(&event),
                ) {
                    let target_finished_at = latest.get(target).map(|(_, _, at)| *at).unwrap_or(0);
                    let duplicate = verdicts.iter().any(|(existing, at)| {
                        *at > target_finished_at
                            && existing.verifier == step
                            && existing.target == target
                            && existing.round == round
                    });
                    if !duplicate {
                        verdicts.push((
                            VerdictRecord {
                                verifier: step.to_string(),
                                target: target.to_string(),
                                round,
                                passed: verdict == "pass",
                                reasons: bound_verdict_reasons(event_reasons(&event)),
                                superseded: false,
                            },
                            line_no,
                        ));
                    }
                }
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
                    Asked {
                        record: EscalationRecord {
                            step: step.to_string(),
                            question_id: question_id.to_string(),
                            question: question.to_string(),
                            answer: None,
                        },
                        asked_at: line_no,
                        resolved_at: None,
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
                if let Some(asked) = escalations.get_mut(question_id) {
                    asked.record.answer = Some(answer.to_string());
                    asked.resolved_at = Some(line_no);
                }
            }
            Some("verdict_failed") | Some("verdict_passed") => {
                let passed = event.get("event").and_then(Value::as_str) == Some("verdict_passed");
                let (Some(verifier), Some(target), Some(round)) = (
                    event.get("verifier").and_then(Value::as_str),
                    event.get("target").and_then(Value::as_str),
                    event_round(&event),
                ) else {
                    continue;
                };
                verdicts.push((
                    VerdictRecord {
                        verifier: verifier.to_string(),
                        target: target.to_string(),
                        round,
                        passed,
                        reasons: bound_verdict_reasons(event_reasons(&event)),
                        superseded: false,
                    },
                    line_no,
                ));
            }
            _ => {}
        }
    }
    // 升级与步骤终态的先后关系（见函数文档）。
    let mut actionable: Vec<EscalationRecord> = Vec::new();
    let mut consumed_answers: Vec<EscalationRecord> = Vec::new();
    for id in escalation_order {
        let Some(asked) = escalations.remove(&id) else {
            continue;
        };
        let finished_at = latest
            .get(&asked.record.step)
            .filter(|(succeeded, _, _)| *succeeded)
            .map(|(_, _, at)| *at);
        match (asked.resolved_at, finished_at) {
            (Some(resolved_at), Some(finished_at)) if finished_at > resolved_at => {
                consumed_answers.push(asked.record);
            }
            (None, Some(finished_at)) if asked.asked_at > finished_at => {
                if let Some(entry) = latest.get_mut(&asked.record.step) {
                    entry.0 = false;
                }
                actionable.push(asked.record);
            }
            _ => actionable.push(asked.record),
        }
    }
    let verdicts = verdicts
        .into_iter()
        .map(|(mut verdict, at)| {
            verdict.superseded = latest
                .get(&verdict.target)
                .map(|(_, _, finished_at)| *finished_at > at)
                .unwrap_or(false);
            verdict
        })
        .collect();
    let steps = order
        .into_iter()
        .filter_map(|id| {
            latest
                .remove(&id)
                .map(|(succeeded, summary, _)| (id, succeeded, summary))
        })
        .collect();
    Ok(PlanJournal {
        steps,
        escalations: actionable,
        consumed_answers,
        verdicts,
        briefs: briefs
            .into_iter()
            .filter_map(|(step, brief)| brief.map(|brief| (step, brief)))
            .collect(),
    })
}

/// 步骤 id → 已解答但尚未消费的 (question, answer) 列表（重跑时注入任务文本）。
pub(super) type SeededAnswers = HashMap<String, Vec<(String, String)>>;
/// 因未决提问而被挂起的 (step, question_id) 列表（供事件日志与报告）。
pub(super) type AwaitingSteps = Vec<(String, String)>;

/// Layer 3 重放：把【仍需处理】的升级记录（见 [`load_plan_journal`]）种进步骤状态。
///
/// - 已解答且答案尚未被消费 → 强制回 Pending（答案改变前提，即使 journal 里已
///   succeeded）并把 (question, answer) 收进返回表，供重跑时注入任务文本；
/// - 未决 → Pending 降为 Skipped（等待编排者应答），返回 (step, question_id) 供事件日志
///   与报告。
///
/// 同一步骤既有已解答又有未决提问时，无论 journal 里先后如何都挂起：先把答案种进去
/// （回 Pending），再让未决提问把它降级——现在跑它只会再次卡在同一个未决问题上。
///
/// 调用方在 verdict 重放【之后】调用：答案触发的重跑优先于「轮次耗尽」的 Failed。
pub(super) fn apply_escalation_seeding(
    status: &mut HashMap<String, StepStatus>,
    escalations: &[EscalationRecord],
) -> (SeededAnswers, AwaitingSteps) {
    let mut answers: SeededAnswers = HashMap::new();
    let mut awaiting: AwaitingSteps = Vec::new();
    let known: Vec<&EscalationRecord> = escalations
        .iter()
        .filter(|escalation| status.contains_key(&escalation.step))
        .collect();
    for escalation in &known {
        if let Some(answer) = &escalation.answer {
            answers
                .entry(escalation.step.clone())
                .or_default()
                .push((escalation.question.clone(), answer.clone()));
            status.insert(escalation.step.clone(), StepStatus::Pending);
        }
    }
    for escalation in &known {
        if escalation.answer.is_none()
            && status.get(&escalation.step) == Some(&StepStatus::Pending)
            && !awaiting.iter().any(|(id, _)| id == &escalation.step)
        {
            status.insert(escalation.step.clone(), StepStatus::Skipped);
            awaiting.push((escalation.step.clone(), escalation.question_id.clone()));
        }
    }
    (answers, awaiting)
}

/// Layer 3 对抗环重放：(verifier, target) 的最新 verdict 决定双方在本次恢复中的起点
/// （按验证者+目标匹配：amend 换了验证者，旧验证者的 verdict 不能复用）。
///
/// - 通过且目标继承为 succeeded → 双双 Succeeded（旧日志里验证者没有 step_finished 时补齐），
///   轮次入账；
/// - 失败且轮次耗尽 → 双双 Failed（预算持久，resume 不自动加码）；
/// - 失败未耗尽 → 双方回 Pending，最后失败原因注入 feedback，轮次续算；
/// - 无 verdict / verdict 已 `superseded`（目标之后重跑过）/ 通过但目标产出已被降级 →
///   目标若有 succeeded 产出则进 Verifying 只补跑验证者（产出不丢），否则双方 Pending；
///   已有的轮次计数仍续算——除非最后一条是被 `superseded` 的 pass：pass 收口了一个对抗
///   周期，之后的重跑是新周期，预算从零起算（见 `DEFAULT_VERDICT_ROUNDS` 的说明），否则
///   「fail、pass、答案触发重跑、再 fail」会在新产出只被否决一次时就耗尽预算。
fn seed_verdict_replay(
    verdicts: &[VerdictRecord],
    verifier_target: &HashMap<String, String>,
    max_rounds: u32,
    status: &mut HashMap<String, StepStatus>,
    verdict_feedback: &mut HashMap<String, String>,
    verdict_rounds: &mut HashMap<String, u32>,
) {
    for (verifier_id, target_id) in verifier_target {
        let last = verdicts
            .iter()
            .rev()
            .find(|v| &v.verifier == verifier_id && &v.target == target_id);
        if let Some(v) = last.filter(|v| !(v.passed && v.superseded)) {
            verdict_rounds.insert(target_id.clone(), v.round);
        }
        match last {
            Some(v)
                if !v.superseded
                    && v.passed
                    && status.get(target_id) == Some(&StepStatus::Succeeded) =>
            {
                status.insert(verifier_id.clone(), StepStatus::Succeeded);
            }
            Some(v) if !v.superseded && !v.passed && v.round >= max_rounds => {
                status.insert(target_id.clone(), StepStatus::Failed);
                status.insert(verifier_id.clone(), StepStatus::Failed);
                if !v.reasons.is_empty() {
                    verdict_feedback.insert(target_id.clone(), v.reasons.join("; "));
                }
            }
            Some(v) if !v.superseded && !v.passed => {
                status.insert(target_id.clone(), StepStatus::Pending);
                status.insert(verifier_id.clone(), StepStatus::Pending);
                verdict_feedback.insert(target_id.clone(), v.reasons.join("; "));
            }
            _ => {
                let target_state = if status.get(target_id) == Some(&StepStatus::Succeeded) {
                    StepStatus::Verifying
                } else {
                    StepStatus::Pending
                };
                status.insert(target_id.clone(), target_state);
                status.insert(verifier_id.clone(), StepStatus::Pending);
            }
        }
    }
}

/// 波次为空时收口对抗环残局；返回是否改变了状态（改变后主循环再跑一遍失败传染）。
///
/// - Verifying 目标的验证者 Failed / 因依赖被 Skipped → 目标 Failed（验证者不可用等同
///   「未能证明」）；验证者因等待升级应答被 Skipped → 目标 Skipped（应答后 resume 只补跑
///   验证者）。两者都不写 step_finished：journal 里目标仍是 awaiting_verdict 的 succeeded，
///   resume 时进 Verifying 复用产出。目标 Failed 时把「验证者不可用」记进 `verdict_feedback`
///   ——报告的对抗环小节按它列出这一对，否则这个 Failed 在报告里没有解释。
/// - Pending 验证者的目标 Failed / Skipped → 验证者 Skipped。
fn resolve_stalled_pairs(
    steps: &[PlanStep],
    verifier_target: &HashMap<String, String>,
    awaiting_input: &[(String, String)],
    status: &mut HashMap<String, StepStatus>,
    summaries: &mut HashMap<String, String>,
    verdict_feedback: &mut HashMap<String, String>,
    plan_log: &EventLog,
) -> bool {
    let mut changed = false;
    for verifier in steps {
        let Some(target_id) = verifier_target.get(&verifier.id) else {
            continue;
        };
        let verifier_id = &verifier.id;
        let verifier_state = status.get(verifier_id).cloned();
        match (status.get(target_id).cloned(), verifier_state) {
            (Some(StepStatus::Verifying), Some(verifier_state @ (StepStatus::Failed | StepStatus::Skipped))) => {
                let previous = summaries.get(target_id).cloned().unwrap_or_default();
                let verifier_awaiting = awaiting_input.iter().any(|(id, _)| id == verifier_id);
                if verifier_awaiting {
                    status.insert(target_id.clone(), StepStatus::Skipped);
                    summaries.insert(
                        target_id.clone(),
                        format!("【待验证：验证者 '{verifier_id}' 等待编排者应答】{previous}"),
                    );
                    append_plan_event(
                        plan_log,
                        "step_skipped",
                        json!({"step": target_id, "reason": format!("verifier '{verifier_id}' awaiting escalation answer")}),
                    );
                } else {
                    status.insert(target_id.clone(), StepStatus::Failed);
                    summaries.insert(
                        target_id.clone(),
                        format!(
                            "【未验证：验证者 '{verifier_id}' 不可用，产出未能被证明】{previous}"
                        ),
                    );
                    // 报告里的「最后原因」：这一对没进过 verdict 轮次，Failed 的解释是验证者
                    // 不可用（因依赖未成功被跳过 / 自身失败），不是产出不达标。
                    let why = if verifier_state == StepStatus::Skipped {
                        "因依赖未成功被跳过"
                    } else {
                        "执行失败"
                    };
                    verdict_feedback.insert(
                        target_id.clone(),
                        format!("验证者 '{verifier_id}' {why}，产出未能被证明"),
                    );
                    append_plan_event(
                        plan_log,
                        "step_unverified",
                        json!({"step": target_id, "verifier": verifier_id, "reason": "verifier unavailable"}),
                    );
                }
                changed = true;
            }
            (Some(StepStatus::Failed | StepStatus::Skipped), Some(StepStatus::Pending)) => {
                status.insert(verifier_id.clone(), StepStatus::Skipped);
                summaries.insert(
                    verifier_id.clone(),
                    format!("被验证步骤 '{target_id}' 未成功，无需验证"),
                );
                append_plan_event(
                    plan_log,
                    "step_skipped",
                    json!({"step": verifier_id, "reason": format!("target '{target_id}' did not succeed")}),
                );
                changed = true;
            }
            _ => {}
        }
    }
    changed
}

/// 步骤尚未拿到答案的升级提问（见 [`open_escalations_from_journal`]）。
struct OpenEscalation {
    question_id: String,
    step: String,
    question: String,
    /// 步骤随问题附上的佐证 / 上下文（可为空）：应答者是父模型，报告里带上它才不用
    /// 翻 journal。
    context: String,
    /// 已有应答记录、但提问步骤没有收到：答案落在步骤收尾之后（本代开始后应答、又没有
    /// `escalation_delivered`——默认 `plan_escalation_wait_secs = 0` 时步骤根本不实时等待，
    /// 开了等待则是超时后才到）。该步骤当前的产出仍是「等待答案」的 handoff，不能当事实
    /// 消费；resume 时答案随任务文本注入重跑，编排者不必再次应答。
    answered_late: bool,
}

/// 扫描计划 journal，返回【步骤尚未拿到答案】的升级提问。答案到达步骤只有两条路：
/// - 本代（最近一次 plan_started / plan_resumed）开始前已应答 → resume 装载时注入该步骤
///   的任务文本（consumed / seeded 两类答案都注入），视为已送达；
/// - 本代开始后应答 → 只有步骤实时等到答案时自己落的 `escalation_delivered` 算送达，
///   否则步骤是在超时后拿「等待答案」的 handoff 收的尾，答案它没见过（`answered_late`）。
///
/// `injected`：本次执行装载 journal 时已注入步骤任务文本的 question_id。锚点先于装载写入，
/// 落在「锚点之后、装载之前」的应答按 seq 属于本代、却已经随装载送达了，只能由执行器
/// 自己告诉这里；不传就会被误报成 `answered_late`。
///
/// 父模型是应答者：报告必须带问题原文，否则它只能看到 id 无法作答。读失败返回 Err，
/// 读不到 journal 就分不清「正常收尾」与「提了问题挂起」，调用方不能当成「没有提问」。
fn open_escalations_from_journal(
    plan_log: &EventLog,
    injected: &HashSet<String>,
) -> Result<Vec<OpenEscalation>, String> {
    let events = plan_log.load()?;
    let mut asked: Vec<OpenEscalation> = Vec::new();
    let mut answered_at: HashMap<String, u64> = HashMap::new();
    let mut delivered: HashSet<String> = injected.clone();
    let mut generation_start = 0u64;
    for (seq, event) in events {
        let question_id = event.get("question_id").and_then(Value::as_str);
        match event.get("event").and_then(Value::as_str) {
            Some("plan_started") | Some("plan_resumed") => generation_start = seq,
            Some("step_escalated") => {
                if let (Some(question_id), Some(step), Some(question)) = (
                    question_id,
                    event.get("step").and_then(Value::as_str),
                    event.get("question").and_then(Value::as_str),
                ) {
                    asked.push(OpenEscalation {
                        question_id: question_id.to_string(),
                        step: step.to_string(),
                        question: question.to_string(),
                        context: event
                            .get("context")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        answered_late: false,
                    });
                }
            }
            Some("escalation_resolved") => {
                if let Some(question_id) = question_id {
                    answered_at.entry(question_id.to_string()).or_insert(seq);
                }
            }
            Some("escalation_delivered") => {
                if let Some(question_id) = question_id {
                    delivered.insert(question_id.to_string());
                }
            }
            _ => {}
        }
    }
    Ok(asked
        .into_iter()
        .filter_map(|mut open| match answered_at.get(&open.question_id) {
            None => Some(open),
            Some(resolved_at) if *resolved_at < generation_start => None,
            Some(_) if delivered.contains(&open.question_id) => None,
            Some(_) => {
                open.answered_late = true;
                Some(open)
            }
        })
        .collect())
}

#[async_trait]
impl Tool for PlanRunTool {
    fn name(&self) -> &str {
        "PlanRun"
    }

    fn description(&self) -> &str {
        "Run a deterministic multi-step plan: you provide the plan artifact (steps with \
         dependencies), the executor runs it in topological order — parallel waves, failed \
         steps mark their dependents skipped, no blind retries, per-step event log in \
         agent/state.sqlite (journal plan/<plan_id>). Each step runs as a bounded sub-agent with all RClaw gates. Use this \
         instead of Agent/Task fan-out when the workflow itself is known and fixed (e.g. cron \
         routines: gather -> analyze -> render). Steps cannot spawn sub-agents or nested \
         plans. Failed steps are reported, not retried; you decide how to follow up (resume \
         with resume_plan_id). A step that escalates a question is parked as skipped \
         (awaiting_escalation) — answer via ResolveWorkflowQuestion, then resume. \
         Declare \"verified_by\" on a step to form a Programmer/Prover loop: the verifier \
         returns a JSON verdict; on fail the target re-runs with the failure reasons injected \
         (bounded by max_verdict_rounds, round budget persisted in the journal)."
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
                "max_parallel": {"type": "integer", "minimum": 1, "maximum": 8, "description": "Max steps running concurrently within a wave (default 4)."},
                "max_verdict_rounds": {"type": "integer", "minimum": 1, "maximum": 4, "description": "Allowed FAILED verdict rounds per verified pair before both steps are marked failed (default 2). Counted per adversarial cycle: a pass closes the cycle, so a target re-run forced after a pass (e.g. by an orchestrator answer) starts a fresh budget; fails within a cycle keep counting. The budget persists across resume via the journal: a pair whose budget is exhausted stays failed on resume (editing the target's task text does not reset it) unless you resume with a larger max_verdict_rounds than the recorded round, or change its verified_by."},
                "resume_plan_id": {"type": "string", "description": "Resume a previous plan by id (the plan_id / event-log locator shown in an earlier PlanRun result): steps recorded 'succeeded' in that journal inherit their summaries and do NOT re-run; failed/skipped steps become pending again so you can retry them with (possibly edited) task text — except a verified pair whose verdict budget is exhausted, which stays failed unless max_verdict_rounds is raised above the recorded round (or its verified_by changed). Steps are matched by id."}
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
        let plan_id = match resume_request {
            Some(raw_id) => sanitize_agent_name(raw_id),
            None => next_execution_id(&plan_name),
        };
        // 同一 plan_id 不允许并发执行（只有 resume 会复用 id）：两份执行器交错写同一条
        // journal，后写的 plan_resumed 会成为另一方收尾时的「本代起点」，把它波次里刚提问、
        // 刚被应答的步骤误判成「已过代」而当作成功收尾（把「问题已提交」的交接当成结果），
        // 且每个 Pending 步骤都会被双倍执行。守卫持有到本次执行返回。
        let Some(_active_plan) = super::workflow::ActiveRunGuard::acquire_plan(&plan_id) else {
            return ToolResult::err(format!(
                "plan '{plan_id}' 正在执行中，不能并发 resume 同一个计划；等它结束后再续跑"
            ));
        };
        let plan_log = EventLog::plan(&self.runtime.journal, &plan_id);
        // resume 只能命中已存在的计划：锚点写入之前先验存在性，否则 resume 一个不存在的 id
        // 会凭空造出一条只有锚点的 journal（错误重定向比静默从零开始危险）。
        if resume_request.is_some() {
            match plan_log.exists() {
                Ok(true) => {}
                Ok(false) => {
                    return ToolResult::err(format!(
                        "计划事件日志（{}）不存在。resume 需要一个已存在 plan 的 id（见先前 PlanRun 结果里的事件日志标识）。",
                        plan_log.locator()
                    ));
                }
                Err(error) => return ToolResult::err(error),
            }
        }
        // plan_started / plan_resumed 是「本代」的锚点（收尾时据它判定一个提问是本代提出的
        // 还是上一代遗留的）：写不进 journal 就不能开跑——否则上一代已应答的提问会被当成
        // 「答案来晚了」而把本代刚跑完的步骤改判成 skipped。与 workflow 的 run_started 同口径。
        //
        // 锚点必须先于装载 journal 写入：反过来，落在「装载之后、锚点之前」的应答既没被装载
        // 注入、收尾时又因 seq 早于锚点被当成上一代已送达的答案——该步骤只剩一个
        // awaiting_escalation，报告里既不在待应答也不在 answered_late，再答又是 already_resolved
        // （模型把 ResolveWorkflowQuestion 和 PlanRun resume 放进同一批工具并发执行时就会撞上）。
        // 锚点先写，装载看到的就是「本代开始后」的完整快照；装载注入过的 question_id 由
        // `injected` 告诉收尾判定，不会反过来被误报成 answered_late。
        if let Err(error) = try_append_plan_event(
            &plan_log,
            "plan_started",
            json!({"plan_id": plan_id, "name": plan_name, "steps": steps.iter().map(|s| json!({"id": s.id, "task": truncate_text(&s.task, 80)})).collect::<Vec<_>>(), "max_parallel": max_parallel, "resumed": resume_request.is_some()}),
        ) {
            return ToolResult::err(format!(
                "计划未启动：{error}。journal 写不进去时无法判定提问归属，拒绝执行以免误判步骤"
            ));
        }
        if resume_request.is_some() {
            if let Err(error) = try_append_plan_event(
                &plan_log,
                "plan_resumed",
                json!({
                    "plan_id": plan_id,
                    "note": "succeeded 步骤将从 journal 继承，failed/skipped/awaiting_escalation 回到 Pending；装载结果见随后的 plan_seeded"
                }),
            ) {
                return ToolResult::err(format!(
                    "计划未恢复：{error}。journal 写不进去时无法判定提问归属，拒绝执行以免误判步骤"
                ));
            }
        }
        let journal = match resume_request {
            Some(_) => match load_plan_journal(&plan_log) {
                Ok(journal) => journal,
                Err(error) => return ToolResult::err(error),
            },
            None => PlanJournal {
                steps: Vec::new(),
                escalations: Vec::new(),
                consumed_answers: Vec::new(),
                verdicts: Vec::new(),
                briefs: HashMap::new(),
            },
        };
        let PlanJournal {
            steps: seeded,
            escalations,
            consumed_answers,
            verdicts: journal_verdicts,
            briefs: journal_briefs,
        } = journal;
        // 装载时注入步骤任务文本的应答（已解答未消费 + 已消费）：收尾判定按已送达处理。
        let injected: HashSet<String> = escalations
            .iter()
            .chain(consumed_answers.iter())
            .filter(|record| record.answer.is_some())
            .map(|record| record.question_id.clone())
            .collect();

        let mut status: HashMap<String, StepStatus> = steps
            .iter()
            .map(|step| (step.id.clone(), StepStatus::Pending))
            .collect();
        let mut summaries: HashMap<String, String> = HashMap::new();
        for (step_id, succeeded, summary) in &seeded {
            if status.contains_key(step_id) && *succeeded {
                status.insert(step_id.clone(), StepStatus::Succeeded);
                summaries.insert(step_id.clone(), summary.clone());
            }
        }
        // 被验证步骤的产出简报（见 `verification_brief`）：继承的产出带回它首跑时的简报，
        // 本次执行里目标每次成功收尾都会刷新。
        let mut target_briefs: HashMap<String, String> = journal_briefs
            .into_iter()
            .filter(|(step_id, _)| status.get(step_id) == Some(&StepStatus::Succeeded))
            .collect();
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
        // Layer 3：升级重放（在 verdict 重放之后：答案改变前提，优先于「轮次耗尽」）——
        // 已解答且未消费的升级把答案注入步骤任务并强制重跑；未决的升级把步骤标 skipped
        // （报告提示先 ResolveWorkflowQuestion 再 resume）。已消费的答案只注入、不触发重跑。
        let (seeded_answers, mut awaiting_input) =
            apply_escalation_seeding(&mut status, &escalations);
        for (step_id, question_id) in &awaiting_input {
            append_plan_event(
                &plan_log,
                "step_skipped",
                json!({"step": step_id, "reason": "awaiting_escalation", "question_id": question_id}),
            );
        }
        let mut step_answers: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for record in &consumed_answers {
            if let Some(answer) = &record.answer {
                step_answers
                    .entry(record.step.clone())
                    .or_default()
                    .push((record.question.clone(), answer.clone()));
            }
        }
        for (step_id, answers) in seeded_answers {
            step_answers.entry(step_id).or_default().extend(answers);
        }
        // 答案触发目标重跑时验证者必须随之回 Pending（否则目标停在 Verifying 等不到验证者），
        // 验证者自己在等应答的除外。
        for (verifier_id, target_id) in &verifier_target {
            if status.get(target_id) == Some(&StepStatus::Pending)
                && status.get(verifier_id) != Some(&StepStatus::Pending)
                && !awaiting_input.iter().any(|(id, _)| id == verifier_id)
            {
                status.insert(verifier_id.clone(), StepStatus::Pending);
            }
            // 对抗周期预算：这一对最后一条 verdict 是 pass、而目标现在要重跑（答案改变前提），
            // 重跑就是新周期，预算从零起算。seed_verdict_replay 只在 pass 已被 `superseded`
            // 时能看出这一点（目标重跑发生在上一次执行里）；答案在【本次】执行里才触发重跑时
            // 它刚把该 pass 的轮次入了账，这里清掉——否则新产出第一次被否决就按「第 n+1 轮」
            // 耗尽预算。周期内的 fail 之后被答案触发重跑仍续算：没有 pass 收口，还是同一周期。
            if status.get(target_id) == Some(&StepStatus::Pending)
                && journal_verdicts
                    .iter()
                    .rev()
                    .find(|v| &v.verifier == verifier_id && &v.target == target_id)
                    .is_some_and(|v| v.passed)
            {
                verdict_rounds.remove(target_id);
            }
            // 验证者自己被答案（或缺了 step_finished 的 verdict 记录）拉回 Pending、而目标已按
            // 未被取代的 pass 判 Succeeded：目标回 Verifying 让验证者带着新前提重验——否则波次
            // 只挑目标在 Verifying 的验证者，这一对卡成 stalled、计划以「调度不变量被破坏」收场。
            // 工具路径到不了这里（验证者再跑必然伴随目标重跑、pass 被取代），只防手工改过的
            // journal。重验是新周期，预算从零起算。
            if status.get(verifier_id) == Some(&StepStatus::Pending)
                && status.get(target_id) == Some(&StepStatus::Succeeded)
            {
                status.insert(target_id.clone(), StepStatus::Verifying);
                verdict_rounds.remove(target_id);
            }
        }
        // 「继承、未重跑」的步骤数必须在 verdict / 升级重放【之后】统计：两者都可能把 journal
        // 里 succeeded 的步骤翻回 Pending（答案改变前提、verdict 未耗尽重跑）。目标进 Verifying
        // 仍算继承（产出复用、只补跑验证者）。`plan_seeded.inherited` 记装载时的数字；报告头
        // 用执行完时仍成立的集合——继承的 Verifying 目标在本次执行里被否决重跑、或被派发前
        // 预检判失败（产出作废）后就不再是「继承、未重跑」。
        let mut inherited_steps: HashSet<String> = seeded
            .iter()
            .filter(|(step_id, succeeded, _)| {
                *succeeded
                    && matches!(
                        status.get(step_id),
                        Some(StepStatus::Succeeded) | Some(StepStatus::Verifying)
                    )
            })
            .map(|(step_id, _, _)| step_id.clone())
            .collect();
        let resumed_count = inherited_steps.len();
        if resume_request.is_some() {
            // 装载统计是诊断信息（锚点已经落了），写失败不阻断。待应答数只计当前计划里
            // 仍存在的步骤：amend 移除的步骤留下的提问不会被任何步骤消费。
            let pending_escalations = escalations
                .iter()
                .filter(|record| record.answer.is_none() && status.contains_key(&record.step))
                .count();
            append_plan_event(
                &plan_log,
                "plan_seeded",
                json!({
                    "plan_id": plan_id,
                    "inherited": resumed_count,
                    "journal_entries": seeded.len(),
                    "pending_escalations": pending_escalations,
                    "injected_answers": injected.len(),
                }),
            );
        }
        // 验证步骤连续「没给出 verdict」的各次原因（基础设施失败 is_error / cancelled，或输出里
        // 没有可接受的 verdict）：不计 verdict 轮次，重试一次；两种原因共用预算，耗尽时的标签
        // 按实际组合措辞（见 `VerifierProblem::exhausted_label`）。
        let mut verifier_errors: HashMap<String, Vec<VerifierProblem>> = HashMap::new();
        // 验证者上一次输出被判「不是 verdict」的诊断：注入验证者（而非目标）的重跑任务。
        let mut verifier_feedback: HashMap<String, String> = HashMap::new();
        // 波次收尾时 journal 读失败 → 计划提前终止的原因（报告为错误）。
        let mut journal_failure: Option<String> = None;

        loop {
            // 任务暂停 / 继续（ZCode 同款语义，第十二轮）：kv 里挂了暂停标志时，当前波
            // 跑完、后续波不派发——在波次边界轮询（300ms），恢复后继续。进入暂停与
            // 恢复各写一次 journal 事件（暂停态的持久事实源；plan_status 投影显示）。
            // 工具 future 被丢弃（父 /cancel）时本循环随 future 一并终止，暂停不改
            // 取消语义。失败传染等收口逻辑都在暂停检查之后——暂停期间不推进状态机。
            {
                let mut announced = false;
                while self
                    .runtime
                    .journal
                    .kv_get(&plan_pause_key(&plan_id))
                    .map(|value| value.is_some())
                    .unwrap_or(false)
                {
                    if !announced {
                        append_plan_event(
                            &plan_log,
                            "plan_paused",
                            json!({"plan_id": plan_id, "note": "当前波跑完后暂停，后续波次等待恢复"}),
                        );
                        announced = true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
                if announced {
                    append_plan_event(&plan_log, "plan_resumed", json!({"plan_id": plan_id}));
                }
            }
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
                            &plan_log,
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
                // 对抗环残局（目标 Verifying 而验证者不可用 / 验证者 Pending 而目标已败）
                // 收口后再跑一遍失败传染；没有可收口的才算跑完。
                if resolve_stalled_pairs(
                    &steps,
                    &verifier_target,
                    &awaiting_input,
                    &mut status,
                    &mut summaries,
                    &mut verdict_feedback,
                    &plan_log,
                ) {
                    continue;
                }
                break;
            }
            let semaphore = Arc::new(tokio::sync::Semaphore::new(max_parallel));
            let mut futures = Vec::with_capacity(wave.len());
            for step in wave {
                let runtime = self.runtime.clone();
                let semaphore = Arc::clone(&semaphore);
                let dep_summaries: Vec<(String, String)> = step
                    .depends_on
                    .iter()
                    .map(|dep| (dep.clone(), summaries.get(dep).cloned().unwrap_or_default()))
                    .collect();
                let step_escalations =
                    step_answers.get(&step.id).cloned().unwrap_or_default();
                let verifier_of = verifier_target.get(&step.id);
                let mut spec = plan_step_spec(
                    &plan_name,
                    plan_id.clone(),
                    step,
                    &dep_summaries,
                    &step_escalations,
                    verifier_of.is_some(),
                );
                // 验证者收尾时建引用集用的目标简报（与注入任务的是同一份）。
                let mut verifier_brief: Option<String> = None;
                // 验证步骤：注入对抗协议 + 目标最新产出简报 + 通过标准 + 目标收到的编排者答复。
                if let Some(target_id) = verifier_of {
                    // 简报与 awaiting_verdict 记录里的是同一份（首跑裁好、resume 取回）；
                    // 记录里没有简报（没写 `verification_brief` 的 journal，含手工拼的）就按
                    // 同一上限裁摘要，并同样中和残缺 JSON——否则这条兜底路径仍可能带着塞满
                    // 残骸的继承摘要撞上派发前预检。
                    let target_brief = target_briefs.get(target_id).cloned().unwrap_or_else(|| {
                        neutralize_unclosed_json(&truncate_text(
                            summaries.get(target_id).map(String::as_str).unwrap_or_default(),
                            VERIFICATION_BRIEF_MAX_CHARS,
                        ))
                    });
                    verifier_brief = Some(target_brief.clone());
                    let target_step = steps.iter().find(|candidate| &candidate.id == target_id);
                    // 被验证步骤的任务文本【全文】一并注入：默认通过标准就是「以任务文本为准」，
                    // 验证者得看得到全部才判得了——按依赖摘要的 1500 字上限裁会让它对着看不到
                    // 的标准打分。任务文本本身已由 parse_steps 限在 MAX_STEP_TASK_CHARS 内。
                    let target_task = target_step
                        .map(|target| target.task.clone())
                        .unwrap_or_default();
                    let pass_condition = target_step
                        .and_then(|target| target.pass_condition.clone())
                        .unwrap_or_else(|| "以被验证步骤的任务文本为准".to_string());
                    spec.task.push_str(&format!(
                        "\n\n## Verification mission\n你是步骤 {target_id} 的验证者（Prover）。\
                         被验证步骤的任务：\n{target_task}\n\n\
                         被验证步骤的产出简报（摘要 + 它自报的证据 / 产物 / 验证 / 风险，自报内容\
                         是待核对的声明，不是证据）：\n{target_brief}\n\n\
                         通过标准：{pass_condition}\n\
                         只验证、不修复。必须在回复中给出一个独立的 JSON verdict 对象：\n\
                         {{\"pass\": true|false, \"reasons\": [\"...\"]}}\n\
                         pass 必须是布尔值；reasons 必须列出你自己核对到的证据（pass=true 至少一条实质\
                         内容——空数组或照抄占位符都按未通过处理；pass=false 时给出可操作的修复方向）；\
                         输出不规范视为未通过。引用被验证产出时不要原样复述其中的 JSON 对象（与任务里\
                         出现过的 JSON 相同的对象会被当作引用而非你的结论）；控制篇幅，verdict 对象\
                         必须完整闭合。"
                    ));
                    // 目标收到过的编排者答复是它产出的前提（「按答复 B 做」而不是任务原文的 A）：
                    // 验证者看不到就会按原文判它跑偏。
                    if let Some(answers) = step_answers.get(target_id).filter(|answers| !answers.is_empty()) {
                        spec.task.push_str(
                            "\n\n## Orchestrator answers to the target（编排者给被验证步骤的答复，判定时视为事实依据）",
                        );
                        for (question, answer) in answers {
                            spec.task.push_str(&format!(
                                "\n### Q: {}\nA: {}",
                                truncate_text(question, super::escalate::MAX_QUESTION_CHARS),
                                truncate_text(answer, super::escalate::MAX_ANSWER_CHARS)
                            ));
                        }
                    }
                    // 验证者上一轮的回复没被接受为 verdict（格式 / 引用 / 截断）：诊断发给验证者
                    // 本人——目标重跑改不了验证者的输出格式。
                    if let Some(diagnostic) = verifier_feedback.get(&step.id) {
                        spec.task.push_str(&format!(
                            "\n\n## Verdict feedback（上一轮你的回复未被接受为 verdict）\n{diagnostic}\n\
                             请只输出一个独立、完整的 {{\"pass\": true|false, \"reasons\": [\"...\"]}} \
                             对象，不要复述任务里出现过的 JSON。"
                        ));
                    }
                }
                // 被验证步骤重跑：注入上一轮 verdict 的失败摘要。
                if let Some(feedback) = verdict_feedback.get(&step.id) {
                    spec.task.push_str(&format!(
                        "\n\n## Verification feedback（上一轮产出被验证者否决）\n失败原因：{feedback}\n\
                         请针对性修复，不要重复已失败的做法。"
                    ));
                }
                // 验证者任务全文：其中出现过的 JSON 对象（目标任务文本与产出简报、依赖摘要、
                // 通过标准、升级答案）在验证者输出里再现时只是引用，不是 verdict（见 classify_verdict）。
                // 引用集必须在派发前就建得起来：建不起来验证者跑了也判不了——输入与它的输出无关，
                // 重跑一次只是白费一次调用；记成验证者的 Malformed 更会让这一对在 resume 时
                // （目标继承同一份简报、只补跑验证者）次次同样失败。责任在目标那一侧：目标按
                // 失败收口并写 step_finished（resume 重跑目标、产出新简报），验证者本次不上场。
                let verifier_inputs = verifier_of.map(|target_id| VerifierInputs {
                    target: target_id.clone(),
                    task_text: spec.task.clone(),
                    target_brief: verifier_brief.unwrap_or_default(),
                });
                if let Some((inputs, failure)) = verifier_inputs
                    .as_ref()
                    .and_then(|inputs| inputs.reference_set_failure().map(|failure| (inputs, failure)))
                {
                    let target_id = inputs.target.clone();
                    // 诊断分两种归因：简报建不起来（重跑目标会变）与只有任务文本建不起来（目标
                    // 任务文本 / 依赖摘要 / 升级答案——重跑目标改变不了，别许诺 resume 能修好）。
                    let diagnostic = failure.diagnostic(&step.id);
                    status.insert(target_id.clone(), StepStatus::Failed);
                    status.insert(step.id.clone(), StepStatus::Skipped);
                    let previous = summaries.get(&target_id).cloned().unwrap_or_default();
                    // 前缀不用「未验证」：那是「验证者不可用、产出保留待 resume 补验」的口径
                    // （见 resolve_stalled_pairs 与报告里的 resume 提示），这里的产出不会被保留。
                    summaries.insert(
                        target_id.clone(),
                        format!("{}{previous}", failure.summary_prefix(&step.id)),
                    );
                    summaries.insert(
                        step.id.clone(),
                        match failure {
                            ReferenceSetFailure::Brief => format!(
                                "被验证步骤 '{target_id}' 的产出无法建立 verdict 引用集，验证者未派发"
                            ),
                            ReferenceSetFailure::TaskText => format!(
                                "验证 '{target_id}' 的任务文本无法建立 verdict 引用集，验证者未派发"
                            ),
                        },
                    );
                    // 报告里的「最后原因」：这一对没进过 verdict 轮次。
                    verdict_feedback.insert(target_id.clone(), diagnostic.clone());
                    target_briefs.remove(&target_id);
                    // 继承的产出就此作废：报告头不再把它算进「继承、未重跑」。
                    inherited_steps.remove(&target_id);
                    // `detail` 让 journal 自解释（不必翻目标的 step_finished 摘要）；`source`
                    // 指明建不起来的是哪份文本。
                    append_plan_event(
                        &plan_log,
                        "step_unverified",
                        json!({
                            "step": target_id,
                            "verifier": step.id,
                            "reason": "reference set unavailable",
                            "source": failure.source(),
                            "detail": diagnostic,
                        }),
                    );
                    // 目标的终态改写成 failed：latest 记录决定 resume 时重跑谁，旧的 awaiting_verdict
                    // 简报随之作废（新记录不带 verification_brief）。
                    append_plan_event(
                        &plan_log,
                        "step_finished",
                        json!({
                            "step": target_id,
                            "outcome": "failed",
                            "summary": truncate_text(&format!("{diagnostic}\n{previous}"), DEP_SUMMARY_MAX_CHARS),
                        }),
                    );
                    append_plan_event(
                        &plan_log,
                        "step_skipped",
                        json!({"step": step.id, "reason": format!("target '{target_id}' unverifiable: reference set unavailable")}),
                    );
                    continue;
                }
                // 真要派发了：继承的产出（若有）从此被重跑取代。
                inherited_steps.remove(&step.id);
                // step_started（第十二轮后续项）：步骤级「运行中」状态的持久事实源——
                // 投影端「有 started 无终态」= running（重跑也重记，事件流按序覆盖）。
                append_plan_event(&plan_log, "step_started", json!({"step": step.id}));
                futures.push(async move {
                    let _permit = semaphore.acquire().await.expect("plan semaphore 不会关闭");
                    let result = runtime.run_task(spec, None, None).await;
                    (step.id.clone(), result, verifier_inputs)
                });
            }
            if futures.is_empty() {
                // 本波只有被预检拦下的验证者：没有东西可等，回到循环头做失败传染 / 收口。
                continue;
            }
            let results = join_all(futures).await;
            // 本波步骤在 journal 里留下的、仍未应答的升级提问：步骤能进波次说明此前没有
            // 未决提问（有则已被 awaiting 跳过），因此此刻的未决提问都是本次执行提出的。
            let open_now = match open_escalations_from_journal(&plan_log, &injected) {
                Ok(open_now) => open_now,
                Err(error) => {
                    // journal 读不到就分不清「正常收尾」与「提了问题挂起」：本波全部按
                    // skipped 收口（安全方向：不交给验证者、不被依赖方消费），终止计划并把
                    // 原因写进报告——resume 时这些步骤回到 Pending 重跑。
                    tracing::warn!("计划 {plan_id} 本波收尾时读取 journal 失败，整波按 skipped 处理: {error}");
                    for (step_id, result, _) in results {
                        status.insert(step_id.clone(), StepStatus::Skipped);
                        summaries.insert(
                            step_id,
                            format!(
                                "【journal 不可读，无法判定本步是否提了问题，产出未采纳：{error}】{}",
                                if result.is_error || result.cancelled {
                                    result.content
                                } else {
                                    result.handoff.summary
                                }
                            ),
                        );
                    }
                    journal_failure = Some(error);
                    break;
                }
            };
            for (step_id, result, verifier_inputs) in results {
                let failed_run = result.is_error || result.cancelled;
                let summary = if failed_run {
                    result.content.clone()
                } else {
                    result.handoff.summary.clone()
                };
                let journal_summary = truncate_text(&summary, DEP_SUMMARY_MAX_CHARS);
                // 升级是一等终态：步骤提了问题而没拿到答案 → skipped（awaiting_escalation），
                // 不进 Verifying、不交给验证者、依赖方不把挂起的 handoff 当事实消费。
                // 「实时等待超时后答案才到」也算没拿到：它收尾时交的仍是等待答案的 handoff。
                let open_questions: Vec<&OpenEscalation> = open_now
                    .iter()
                    .filter(|open| open.step == step_id)
                    .collect();
                if let (false, Some(first_open)) = (failed_run, open_questions.first()) {
                    let first_question = first_open.question_id.clone();
                    status.insert(step_id.clone(), StepStatus::Skipped);
                    summaries.insert(step_id.clone(), summary.clone());
                    append_plan_event(
                        &plan_log,
                        "step_skipped",
                        json!({
                            "step": step_id,
                            "reason": "awaiting_escalation",
                            "question_id": first_question,
                            "question_ids": open_questions.iter().map(|open| &open.question_id).collect::<Vec<_>>(),
                            "answered_late": open_questions.iter().filter(|open| open.answered_late).map(|open| &open.question_id).collect::<Vec<_>>(),
                            "summary": journal_summary,
                        }),
                    );
                    awaiting_input.push((step_id.clone(), first_question));
                    continue;
                }
                // 验证步骤：解析 verdict 并驱动对抗环。
                if let Some(target_id) = verifier_target.get(&step_id).cloned() {
                    summaries.insert(step_id.clone(), summary.clone());
                    // 没拿到 verdict 的两种情况都不是目标的责任、都不计轮次：基础设施失败
                    // （is_error / cancelled）与格式类失败（输出里没有可接受的 verdict）。
                    // 重试一次（目标保持 Verifying、不重跑），再失败则双双 Failed——不写目标的
                    // step_finished，resume 时目标回 Verifying 只补跑验证者。
                    let verdict = if failed_run {
                        Err((VerifierProblem::Errored, summary.clone()))
                    } else {
                        // 引用集按派发时固定下来的任务全文与目标简报建（与预检、与验证者看到的
                        // 是同一份）：「出自目标」的引用按那份简报判定。
                        let (task_text, target_brief) = verifier_inputs
                            .as_ref()
                            .map(|inputs| (inputs.task_text.as_str(), inputs.target_brief.as_str()))
                            .unwrap_or_default();
                        match classify_verdict(&result.content, task_text, target_brief) {
                            VerdictParse::Verdict { pass, reasons } => Ok((pass, reasons)),
                            VerdictParse::Malformed(diagnostic) => {
                                Err((VerifierProblem::Malformed, diagnostic))
                            }
                        }
                    };
                    let (pass, reasons) = match verdict {
                        Ok(verdict) => verdict,
                        Err((problem, detail)) => {
                            let attempts = verifier_errors.entry(step_id.clone()).or_default();
                            attempts.push(problem);
                            let attempt = attempts.len();
                            let label = VerifierProblem::exhausted_label(attempts);
                            append_plan_event(
                                &plan_log,
                                problem.event(),
                                json!({"verifier": step_id, "target": target_id, "attempt": attempt, "error": truncate_text(&detail, 400)}),
                            );
                            if attempt < 2 {
                                status.insert(step_id.clone(), StepStatus::Pending);
                                // 格式类诊断注入验证者的重跑任务；基础设施失败没有可改的输出。
                                if problem == VerifierProblem::Malformed {
                                    verifier_feedback.insert(
                                        step_id.clone(),
                                        truncate_text(&detail, MAX_VERDICT_REASON_CHARS),
                                    );
                                }
                            } else {
                                status.insert(target_id.clone(), StepStatus::Failed);
                                status.insert(step_id.clone(), StepStatus::Failed);
                                let previous =
                                    summaries.get(&target_id).cloned().unwrap_or_default();
                                summaries.insert(
                                    target_id.clone(),
                                    format!("【未验证：验证者 '{step_id}' {label}】{previous}"),
                                );
                                // 报告里的「最后原因」：对抗环未分胜负，责任在验证者。
                                verdict_feedback.insert(
                                    target_id.clone(),
                                    format!(
                                        "验证者 '{step_id}' {label}：{}",
                                        truncate_text(&detail, MAX_VERDICT_REASON_CHARS)
                                    ),
                                );
                                // 目标的终局与 resolve_stalled_pairs 的「验证者不可用」同一口径，
                                // 不写目标的 step_finished（产出留在 awaiting_verdict 记录里，
                                // resume 只补跑验证者），用 step_unverified 说明它为什么 Failed。
                                append_plan_event(
                                    &plan_log,
                                    "step_unverified",
                                    json!({"step": target_id, "verifier": step_id, "reason": label}),
                                );
                                append_plan_event(
                                    &plan_log,
                                    "step_finished",
                                    json!({"step": step_id, "outcome": "failed", "target": target_id, "summary": journal_summary}),
                                );
                            }
                            continue;
                        }
                    };
                    // 拿到 verdict 即清零「未给出 verdict」计数：预算是「连续两次」，不是整个计划
                    // 执行期内累计两次（第 1 轮一次 is_error、第 2 轮再一次不该直接双双 Failed）。
                    verifier_errors.remove(&step_id);
                    verifier_feedback.remove(&step_id);
                    let reasons = bound_verdict_reasons(reasons);
                    // 轮次来自 journal 回放（`event_round` 收任意 u32）：手工改过的记录到了顶就停在
                    // 顶上按耗尽处理，不能回绕成 0 重置预算（debug 下更是直接 panic）。
                    let round = verdict_rounds
                        .get(&target_id)
                        .copied()
                        .unwrap_or(0)
                        .saturating_add(1);
                    verdict_rounds.insert(target_id.clone(), round);
                    let exhausted = !pass && round >= max_verdict_rounds;
                    if pass {
                        status.insert(target_id.clone(), StepStatus::Succeeded);
                        status.insert(step_id.clone(), StepStatus::Succeeded);
                        append_plan_event(
                            &plan_log,
                            "verdict_passed",
                            json!({"verifier": step_id, "target": target_id, "round": round}),
                        );
                    } else if exhausted {
                        status.insert(target_id.clone(), StepStatus::Failed);
                        status.insert(step_id.clone(), StepStatus::Failed);
                        // 报告里的「最后原因」（目标已 Failed，本次执行不会再重跑消费它）。
                        verdict_feedback.insert(target_id.clone(), reasons.join("; "));
                        append_plan_event(
                            &plan_log,
                            "verdict_failed",
                            json!({"verifier": step_id, "target": target_id, "round": round, "exhausted": true, "reasons": reasons}),
                        );
                    } else {
                        verdict_feedback.insert(target_id.clone(), reasons.join("; "));
                        status.insert(target_id.clone(), StepStatus::Pending);
                        status.insert(step_id.clone(), StepStatus::Pending);
                        append_plan_event(
                            &plan_log,
                            "verdict_failed",
                            json!({"verifier": step_id, "target": target_id, "round": round, "reasons": reasons}),
                        );
                    }
                    // 验证者自己的终态记录：没有它，resume 时验证者及其依赖方永远不会被调度。
                    // 同时带全 verdict / round / reasons：与上面的 verdict_* 事件互为备份
                    // （见 load_plan_journal），任一条写丢，轮次预算与失败原因仍能从另一条恢复。
                    append_plan_event(
                        &plan_log,
                        "step_finished",
                        json!({
                            "step": step_id,
                            "outcome": if pass { "succeeded" } else { "failed" },
                            "verdict": if pass { "pass" } else { "fail" },
                            "target": target_id,
                            "round": round,
                            "exhausted": exhausted,
                            "reasons": reasons,
                            "summary": journal_summary,
                        }),
                    );
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
                    summaries.insert(step_id.clone(), summary.clone());
                    // 产出简报随 awaiting_verdict 记录落盘：resume 补跑的验证者与现在要上场的
                    // 验证者读同一份（见 `verification_brief`）。
                    let brief = verification_brief(&summary, &result.handoff);
                    target_briefs.insert(step_id.clone(), brief.clone());
                    append_plan_event(
                        &plan_log,
                        "step_finished",
                        json!({
                            "step": step_id,
                            "outcome": "succeeded",
                            "awaiting_verdict": true,
                            "summary": journal_summary,
                            "verification_brief": brief,
                        }),
                    );
                    continue;
                }
                target_briefs.remove(&step_id);
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
                    &plan_log,
                    "step_finished",
                    json!({"step": step_id, "outcome": if failed_run { "failed" } else { "succeeded" }, "summary": journal_summary}),
                );
            }
        }

        if journal_failure.is_some() {
            // 提前终止：尚未调度的步骤统一按 skipped 收口（不是调度不变量被破坏）。
            for step in &steps {
                if matches!(
                    status.get(&step.id),
                    Some(StepStatus::Pending) | Some(StepStatus::Verifying)
                ) {
                    status.insert(step.id.clone(), StepStatus::Skipped);
                    summaries
                        .entry(step.id.clone())
                        .or_insert_with(|| "计划因 journal 读取失败提前终止，未执行".to_string());
                }
            }
        }
        let count = |wanted: StepStatus| status.values().filter(|s| **s == wanted).count();
        let succeeded = count(StepStatus::Succeeded);
        let failed = count(StepStatus::Failed);
        let skipped = count(StepStatus::Skipped);
        // 循环结束仍 Pending / Verifying 的步骤：调度不变量被破坏（不应发生），必须作为
        // 错误暴露而不是伪装成「未执行」的成功报告。
        let stalled: Vec<&String> = steps
            .iter()
            .filter(|step| {
                matches!(
                    status.get(&step.id),
                    Some(StepStatus::Pending) | Some(StepStatus::Verifying)
                )
            })
            .map(|step| &step.id)
            .collect();
        append_plan_event(
            &plan_log,
            "plan_completed",
            json!({"succeeded": succeeded, "failed": failed, "skipped": skipped, "stalled": stalled.len()}),
        );
        let inherited_now = inherited_steps.len();
        let mut report = format!(
            "计划 '{plan_name}'（{plan_id}）执行完毕：{succeeded} 成功 / {failed} 失败 / {skipped} 跳过{}。\n事件日志：{}\n",
            if inherited_now > 0 {
                format!("（从 journal 继承 {inherited_now} 步产出，未重跑）")
            } else {
                String::new()
            },
            plan_log.locator(),
        );
        // Layer 3 对抗环轮次汇总（按 steps 声明顺序，fresh 与 resume 同一口径）。
        // 进过 verdict 轮次、验证者在第一轮就耗尽预算 / 因依赖被跳过（轮次为 0 但留下了反馈）、
        // 或验证者在等编排者应答的目标都列出：不列的话，「验证者连续两次没给出 verdict /
        // 验证者被跳过导致目标 Failed」在报告里就只剩一个没有解释的 Failed。
        let verifier_awaiting =
            |verifier_id: &String| awaiting_input.iter().any(|(id, _)| id == verifier_id);
        let verified_pairs: Vec<(&PlanStep, &String)> = steps
            .iter()
            .filter_map(|target| {
                let verifier_id = target.verified_by.as_ref()?;
                (verdict_rounds.contains_key(&target.id)
                    || verdict_feedback.contains_key(&target.id)
                    || verifier_awaiting(verifier_id))
                .then_some((target, verifier_id))
            })
            .collect();
        if !verified_pairs.is_empty() {
            report.push_str("\n\n## 对抗环（Programmer/Prover）");
            for (target, verifier_id) in verified_pairs {
                let target_id = &target.id;
                let round = verdict_rounds.get(target_id).copied().unwrap_or(0);
                // 轮次与终态以本地执行状态为准；失败带上最后一次的原因（编排者据此分辨
                // 是产出不达标还是验证者格式问题）。
                let verdict_text = match status.get(target_id) {
                    Some(StepStatus::Succeeded) => format!("第 {round} 轮通过"),
                    Some(StepStatus::Failed) => {
                        let last_reasons = verdict_feedback
                            .get(target_id)
                            .filter(|reasons| !reasons.trim().is_empty())
                            .map(|reasons| {
                                format!("；最后原因：{}", truncate_text(reasons, MAX_VERDICT_REASON_CHARS))
                            })
                            .unwrap_or_default();
                        if round == 0 {
                            format!("未进入 verdict 轮次（验证者不可用）{last_reasons}")
                        } else {
                            format!("{round} 轮后仍未通过（轮次耗尽或验证者不可用）{last_reasons}")
                        }
                    }
                    Some(StepStatus::Pending) | Some(StepStatus::Verifying) => {
                        format!("第 {round} 轮进行中")
                    }
                    // 目标被跳过：验证者在等编排者应答（目标产出保留，应答后 resume 只补跑
                    // 验证者），或目标自己因依赖未成功 / 等待应答没跑起来。
                    Some(StepStatus::Skipped) if verifier_awaiting(verifier_id) => {
                        format!("已 {round} 轮，等待验证者 '{verifier_id}' 的编排者应答")
                    }
                    Some(StepStatus::Skipped) => format!("已 {round} 轮，目标被跳过（未进入验证）"),
                    _ => format!("{round} 轮"),
                };
                report.push_str(&format!("\n- {target_id} ⟷ {verifier_id}: {verdict_text}"));
            }
        }
        if let Some(error) = &journal_failure {
            report.push_str(&format!(
                "\n\n⚠ 计划提前终止：波次收尾时读取事件日志失败（{error}）。本波产出未采纳、\
                 后续步骤未执行；请排查 state.sqlite 后用 resume_plan_id 恢复。"
            ));
        }
        let open_escalations = match open_escalations_from_journal(&plan_log, &injected) {
            Ok(open) => open,
            Err(error) => {
                report.push_str(&format!(
                    "\n\n⚠ 无法读取事件日志以列出待应答的升级提问：{error}"
                ));
                Vec::new()
            }
        };
        // amend 移除 / 改名的步骤留下的提问：没有步骤会消费它的答案，「应答后 resume 重跑」
        // 的指引不适用，单独列出，不混进待应答清单。
        let (in_plan, orphaned): (Vec<&OpenEscalation>, Vec<&OpenEscalation>) = open_escalations
            .iter()
            .partition(|open| status.contains_key(&open.step));
        let (answered_late, unanswered): (Vec<&OpenEscalation>, Vec<&OpenEscalation>) =
            in_plan.into_iter().partition(|open| open.answered_late);
        // 提问行：问题原文 + 步骤附的佐证（有才带；应答者据此作答，不必翻 journal）。
        let escalation_row = |open: &OpenEscalation, suffix: &str| {
            let context = match open.context.trim() {
                "" => String::new(),
                context => format!("\n  佐证：{}", truncate_text(context, 300)),
            };
            format!(
                "\n- [{}] {}{suffix}: {}{context}",
                open.step,
                open.question_id,
                truncate_text(&open.question, 300)
            )
        };
        if !unanswered.is_empty() {
            report.push_str(
                "\n\n⏸ 计划存在待应答的升级提问（子代理拒绝编造答案；请逐条 \
                 ResolveWorkflowQuestion {question_id, answer} 应答，再用 PlanRun \
                 resume_plan_id 恢复，该步骤会带着答案重跑）：",
            );
            for open in &unanswered {
                report.push_str(&escalation_row(open, ""));
            }
        }
        if !answered_late.is_empty() {
            report.push_str(
                "\n\n⏸ 以下提问已有应答，但答案是在步骤收尾之后才到的（步骤未实时等待，或等待已超时）、\
                 步骤未收到；无需再次应答，直接用 PlanRun resume_plan_id 恢复，该步骤会带着答案重跑：",
            );
            for open in &answered_late {
                report.push_str(&escalation_row(open, ""));
            }
        }
        if !orphaned.is_empty() {
            report.push_str(
                "\n\n以下提问所属的步骤已不在当前计划里（resume 时被移除或改名），不会被任何步骤消费、\
                 无需应答；若仍需要它的答案，把该步骤按原 id 加回 steps 再 resume：",
            );
            for open in &orphaned {
                report.push_str(&escalation_row(
                    open,
                    if open.answered_late { "（已有应答）" } else { "" },
                ));
            }
        }
        if !awaiting_input.is_empty() {
            report.push_str(
                "\n\n以下步骤因等待编排者应答被跳过（未交给验证者、依赖方未消费其产出）：",
            );
            for (step_id, question_id) in &awaiting_input {
                report.push_str(&format!(
                    "\n- {step_id} [awaiting_escalation]: {question_id}"
                ));
            }
        }
        if !stalled.is_empty() {
            report.push_str(
                "\n\n⚠ 以下步骤既未执行也未被判定（执行器调度不变量被破坏，请连同事件日志报告）：",
            );
            for step_id in &stalled {
                report.push_str(&format!("\n- {step_id}"));
            }
        }
        for step in &steps {
            let label = match status.get(&step.id) {
                Some(StepStatus::Succeeded) => "succeeded",
                Some(StepStatus::Failed) => "failed",
                Some(StepStatus::Skipped) => "skipped",
                Some(StepStatus::Verifying) => "stalled_verifying",
                _ => "stalled",
            };
            let summary = summaries
                .get(&step.id)
                .map(|summary| truncate_text(summary, 300))
                .unwrap_or_else(|| {
                    // 装载期因未决提问被挂起的步骤没有本次产出：它不是「依赖未成功」，
                    // 别把编排者引向一个不存在的依赖失败。
                    if let Some((_, question_id)) =
                        awaiting_input.iter().find(|(id, _)| id == &step.id)
                    {
                        return format!("等待编排者应答：{question_id}");
                    }
                    match label {
                        "skipped" => "依赖步骤未成功".to_string(),
                        _ => "无结果".to_string(),
                    }
                });
            report.push_str(&format!("\n- {} [{}]: {}", step.id, label, summary));
        }
        report.push_str(&format!(
            "\n\n处置提示：失败步骤不会自动重试。如需恢复或重试，用相同的 steps 重新提交并带上 \
             resume_plan_id=\"{plan_id}\"（已成功步骤按 id 继承结果、只重跑失败/跳过步骤，\
             失败步骤的任务文本可以修改；因验证者不可用而失败的【未验证】目标保留本次产出，\
             resume 只重跑验证者；verdict 轮次耗尽的对抗环在 resume 时仍判失败——改任务文本\
             不重置预算，需带更大的 max_verdict_rounds 或更换 verified_by 才会重跑）；\
             单点补救则用 Agent/Task 重发单步。skipped 步骤的依赖未成功。"
        ));
        if stalled.is_empty() && journal_failure.is_none() {
            ToolResult::ok(report)
        } else {
            ToolResult::err(report)
        }
    }
}

/// 由计划步骤构造子代理任务：依赖摘要与已解答的升级答案并入任务文本，
/// 编码类步骤沿用 candidate_patch 隔离；plan_id/step_id 注入 spec 供 escalate 定位 journal。
///
/// 验证者（`is_verifier`）一律 shared_cas：candidate_patch 沙箱没有 shell，而验证者的任务
/// 恰恰是跑测试 / 复现 / 核对产物——按任务文本长相把它判成「编码任务」会让它只能对着
/// 文字猜，verdict 退化成读后感。
fn plan_step_spec(
    plan_name: &str,
    plan_id: String,
    step: &PlanStep,
    dep_summaries: &[(String, String)],
    escalations: &[(String, String)],
    is_verifier: bool,
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
                truncate_text(summary, DEP_SUMMARY_MAX_CHARS)
            ));
        }
        task.push_str("\n上游结果是事实输入：与其矛盾时以最新证据为准并在结论中说明。\n");
    }
    if !escalations.is_empty() {
        task.push_str("\n## Orchestrator answers (resolved escalations)\n");
        // 答案是编排者给步骤的权威输入，按 escalate 入口的同一上限保留全文，
        // 不能套依赖摘要的 1500 字上限（默认 wait_secs=0 时所有答案都走这条注入路径，
        // 截掉就等于编排者的话只到了一部分）。
        for (question, answer) in escalations {
            task.push_str(&format!(
                "\n### Q: {}\nA: {}（编排者输入，视为最新事实依据）\n",
                truncate_text(question, super::escalate::MAX_QUESTION_CHARS),
                truncate_text(answer, super::escalate::MAX_ANSWER_CHARS)
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
        isolation: if !is_verifier && looks_like_complex_coding_task(&step.task) {
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

    /// 临时 agent 目录 + 其 state.sqlite 的 journal 句柄（目录由调用方负责清理）。
    fn temp_layout(name: &str) -> (std::path::PathBuf, Layout, JournalStore) {
        let dir = std::env::temp_dir().join(format!(
            "rclaw-plan-{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let layout = Layout::new(dir.join("agent"));
        let journal = JournalStore::open(&layout).expect("open state.sqlite");
        (dir, layout, journal)
    }

    /// 预置好事件的计划 journal 流（原样写入，不经 `append_plan_event` 补 ts）。
    fn journal_in_temp(name: &str, lines: &[Value]) -> (std::path::PathBuf, EventLog) {
        let (dir, _layout, journal) = temp_layout(name);
        let plan_log = EventLog::plan(&journal, name);
        for line in lines {
            plan_log.append(line).expect("写入测试 journal");
        }
        (dir, plan_log)
    }

    /// 事件流里的 `event` 名序列。
    fn event_names(plan_log: &EventLog) -> Vec<String> {
        plan_log
            .load()
            .unwrap()
            .into_iter()
            .filter_map(|(_, event)| {
                event
                    .get("event")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    }

    #[test]
    fn load_plan_journal_takes_latest_result_per_step() {
        let (dir, plan_log) = journal_in_temp(
            "latest",
            &[
                json!({"event": "plan_started", "steps": ["a", "b"]}),
                json!({"event": "step_finished", "step": "a", "outcome": "failed", "summary": "第一次失败"}),
                json!({"event": "step_finished", "step": "a", "outcome": "succeeded", "summary": "重跑成功"}),
                json!({"event": "step_finished", "step": "b", "outcome": "failed", "summary": "依赖缺失"}),
                // 非对象事件（无 event 字段）应被跳过而不是让解析失败。
                json!("不是事件对象"),
            ],
        );
        let journal = load_plan_journal(&plan_log).unwrap();
        assert_eq!(journal.steps.len(), 2);
        let a = journal.steps.iter().find(|(id, _, _)| id == "a").unwrap();
        assert!(a.1, "a 应取最后一次结果（succeeded）");
        assert_eq!(a.2, "重跑成功");
        let b = journal.steps.iter().find(|(id, _, _)| id == "b").unwrap();
        assert!(!b.1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_plan_journal_rejects_missing_logs_but_accepts_unfinished_plans() {
        let (dir, _layout, journal) = temp_layout("missing");
        let missing = EventLog::plan(&journal, "nope");
        let Err(error) = load_plan_journal(&missing) else {
            panic!("journal 不存在应报错");
        };
        assert!(error.contains("不存在"), "journal 不存在应报错: {error}");
        // 首步就 escalate 的计划（只有 plan_started + step_escalated，没有 step_finished）
        // 应答后必须能 resume——不能因为「没执行完过一步」被拒。
        let unfinished = journal_in_temp(
            "unfinished",
            &[
                json!({"event": "plan_started", "steps": ["ask"]}),
                json!({"event": "step_escalated", "step": "ask", "question_id": "p:ask:q-1", "question": "凭据在哪？"}),
            ],
        )
        .1;
        let loaded = load_plan_journal(&unfinished).expect("无 step_finished 的 journal 也应可加载");
        assert!(loaded.steps.is_empty());
        assert_eq!(loaded.escalations.len(), 1);
        assert_eq!(loaded.escalations[0].step, "ask");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn parse_verdict_extracts_pass_and_reasons() {
        // 正文内嵌 JSON 对象。
        let (pass, reasons) = parse_verdict(
            "验证结论如下 {\"pass\": true, \"reasons\": [\"全部满足\"]} 请查收",
            "",
        );
        assert!(pass);
        assert_eq!(reasons, vec!["全部满足".to_string()]);

        // fail 且给出 reasons。
        let (pass, reasons) = parse_verdict(
            "{\"pass\": false, \"reasons\": [\"边界条件未覆盖\", \"测试未跑\"]}",
            "",
        );
        assert!(!pass);
        assert_eq!(reasons.len(), 2);

        // fail 无 reasons → 注入兜底提示（只带 pass 一个键的对象仍算 verdict）。
        let (pass, reasons) = parse_verdict("{\"pass\": false}", "");
        assert!(!pass);
        assert!(reasons[0].contains("未给出 reasons"));
        // pass 无证据 → 不放行：空数组 / 缺 reasons / 空串 / 照抄模板占位符都不算证据。
        for (index, content) in [
            "{\"pass\": true}",
            "{\"pass\": true, \"reasons\": []}",
            "{\"pass\": true, \"reasons\": [\"\", \"   \"]}",
            "{\"pass\": true, \"reasons\": [\"...\"]}",
            "{\"pass\": true, \"reasons\": [\"…\"]}",
        ]
        .iter()
        .enumerate()
        {
            let (pass, reasons) = parse_verdict(content, "");
            assert!(!pass, "case {index} 无证据的 pass 应判 fail");
            assert!(reasons[0].contains("证据"), "case {index}: {reasons:?}");
        }
        // 占位符混在真实证据里：只剔除占位符。
        let (pass, reasons) = parse_verdict(
            "{\"pass\": true, \"reasons\": [\"...\", \"cargo test 12 passed\"]}",
            "",
        );
        assert!(pass, "{reasons:?}");
        assert_eq!(reasons, vec!["cargo test 12 passed".to_string()]);

        // 提取失败 / 解析失败 / 缺 pass 字段 → 一律 fail（验证者输出不规范 ≠ 通过）。
        for (index, content) in [
            "没有任何 JSON",
            "{not json",
            "{\"reasons\": []}",
        ]
        .iter()
        .enumerate()
        {
            let (pass, reasons) = parse_verdict(content, "");
            assert!(!pass, "case {index} 应判 fail");
            assert!(!reasons.is_empty(), "case {index} 应给出原因");
        }
    }

    #[test]
    fn parse_verdict_scans_balanced_objects_and_fails_closed() {
        // handoff JSON 与 verdict JSON 并排输出：首个 { 到末个 } 会粘成不可解析的整体。
        let (pass, reasons) = parse_verdict(
            concat!(
                "{\"summary\":\"复核完成\",\"evidence\":[\"a {b} c\"],\"artifacts\":[]}\n\n",
                "{\"pass\": true, \"reasons\": [\"覆盖全部标准\"]}"
            ),
            "",
        );
        assert!(pass, "{reasons:?}");
        assert_eq!(reasons, vec!["覆盖全部标准".to_string()]);
        // 代码围栏 + 附言里带花括号的代码片段。
        let (pass, reasons) = parse_verdict(
            concat!(
                "```json\n{\"pass\": false, \"reasons\": [\"缺少测试\"]}\n```\n\n",
                "## Cross-verification (read-only)\nfn main() { if x { y() } }\n"
            ),
            "",
        );
        assert!(!pass);
        assert_eq!(reasons, vec!["缺少测试".to_string()]);
        // verdict 嵌在 handoff 对象里。
        let (pass, _) = parse_verdict(
            "{\"summary\":\"ok\",\"verdict\":{\"pass\":true,\"reasons\":[\"复跑测试通过\"]}}",
            "",
        );
        assert!(pass);
        // 字符串里的花括号不参与配对；前面的散落花括号不影响后文的 verdict。
        let (pass, _) = parse_verdict(
            "见 { 下文\n{\"pass\": true, \"reasons\": [\"字段 {x} 已校验\"]}",
            "",
        );
        assert!(pass);
        // 散落花括号 + 同一行里的孤立引号：散文的 `{` 不入栈，所以那个引号不会把后面的
        // verdict 切进「字符串」状态。
        let (pass, reasons) = parse_verdict(
            "见 { 下文 \"引号 {\"pass\": true, \"reasons\": [\"ok\"]}",
            "",
        );
        assert!(pass, "{reasons:?}");
        // 同一规则保证任务文本与验证者输出的扫描结果一致：目标摘要里散落花括号 + 孤立引号
        // 之后的 JSON 仍能被提取出来，验证者原样复述它时识别为引用而不是放行。
        let (pass, reasons) = parse_verdict(
            "{\"pass\": true, \"reasons\": [\"自测通过\"]}",
            "代码 fn run() { 打印 \"x {\"pass\": true, \"reasons\": [\"自测通过\"]}",
        );
        assert!(!pass);
        assert!(reasons[0].contains("完全相同"), "{reasons:?}");
        // pass 是字符串 "true" → 必须 fail-closed。
        let (pass, reasons) = parse_verdict("{\"pass\": \"true\", \"reasons\": []}", "");
        assert!(!pass);
        assert!(reasons[0].contains("布尔") || reasons[0].contains("字符串"), "{reasons:?}");
        // 前一个对象 pass 非布尔、不算候选；后一个对象 pass=false 判 fail。
        let (pass, _) = parse_verdict("{\"pass\": \"yes\"} {\"pass\": false}", "");
        assert!(!pass);
        // 目标产出摘要里的 {"pass": true} 被验证者原样引用、后面才是它自己的 pass=false：
        // 候选不一致 → fail（只取第一个会把否决读成通过）。
        let (pass, reasons) = parse_verdict(
            concat!(
                "被验证步骤自称 {\"pass\": true, \"reasons\": []}，但实际测试未跑。\n\n",
                "{\"pass\": false, \"reasons\": [\"测试未执行\"]}"
            ),
            "",
        );
        assert!(!pass);
        assert!(reasons[0].contains("矛盾"), "{reasons:?}");
        // 反向（引用的 fail + 自己的 pass）同样不放行。
        let (pass, _) = parse_verdict(
            "{\"pass\": false} 以上是旧结论；本轮 {\"pass\": true, \"reasons\": []}",
            "",
        );
        assert!(!pass);
        // handoff 内嵌 verdict 与独立 verdict 一致：按一致结论处理，reasons 去重合并。
        let (pass, reasons) = parse_verdict(
            concat!(
                "{\"summary\":\"ok\",\"verdict\":{\"pass\":false,\"reasons\":[\"缺测试\"]}}\n",
                "{\"pass\": false, \"reasons\": [\"缺测试\", \"文档未更新\"]}"
            ),
            "",
        );
        assert!(!pass);
        assert_eq!(reasons, vec!["缺测试".to_string(), "文档未更新".to_string()]);
    }

    #[test]
    fn parse_verdict_ignores_nested_rows_and_echoed_target_json() {
        // 嵌在外层对象里的 pass 不是候选：没有 verdict 键的 handoff 内转述目标自述 → fail-closed，
        // 而不是把那段 {"pass": true} 当成唯一候选放行。
        let (pass, reasons) = parse_verdict(
            "{\"summary\":\"复核完成\",\"target_claim\":{\"pass\":true,\"reasons\":[]}}",
            "",
        );
        assert!(!pass);
        assert!(reasons[0].contains("verdict 形状"), "{reasons:?}");
        // 外层 verdict 里的逐项检查行（pass=false 的可选项）不构成「矛盾」。
        let (pass, reasons) = parse_verdict(
            concat!(
                "{\"pass\": true, \"reasons\": [\"必测项通过\"], \"checks\": [",
                "{\"name\": \"docs\", \"pass\": false, \"optional\": true},",
                "{\"name\": \"tests\", \"pass\": true}]}"
            ),
            "",
        );
        assert!(pass, "{reasons:?}");
        // 独立成行的测试结果行带布尔 pass 但没有 reasons：不长成 verdict 的样子，不参与裁决。
        let (pass, reasons) = parse_verdict(
            concat!(
                "{\"name\": \"t1\", \"pass\": true}\n{\"name\": \"t2\", \"pass\": false}\n",
                "{\"pass\": true, \"reasons\": [\"必测项全部通过\"]}"
            ),
            "",
        );
        assert!(pass, "{reasons:?}");
        assert_eq!(reasons, vec!["必测项全部通过".to_string()]);
        let (pass, reasons) = parse_verdict("{\"name\": \"t1\", \"pass\": true}", "");
        assert!(!pass);
        assert!(reasons[0].contains("测试结果行"), "{reasons:?}");
        // 目标产出摘要里的 JSON 被验证者原样复述、而它自己的 verdict 丢了（截断 / 不规范），
        // 唯一候选是引用 → 不能放行，并点明原因。
        let echoed = "完成实现。自检：{\"pass\": true, \"reasons\": []}";
        let (pass, reasons) = parse_verdict(
            "目标自称 {\"pass\": true, \"reasons\": []}，本轮核查发现 {\"pass\": \"false\"}",
            echoed,
        );
        assert!(!pass);
        assert!(reasons[0].contains("完全相同"), "{reasons:?}");
        // 复述的是目标整个 handoff（verdict 内嵌其中）同样算引用。
        let echoed_handoff =
            "{\"summary\":\"done\",\"verdict\":{\"pass\":true,\"reasons\":[\"自测通过\"]}}";
        let (pass, _) = parse_verdict(echoed_handoff, echoed_handoff);
        assert!(!pass);
        let (pass, _) = parse_verdict("{\"pass\":true,\"reasons\":[\"自测通过\"]}", echoed_handoff);
        assert!(!pass);
        // 验证者自己的 verdict 与引用不同：引用被剔除，不再构成「矛盾」，以自己的结论为准。
        let (pass, reasons) = parse_verdict(
            concat!(
                "目标自称 {\"pass\": true, \"reasons\": []}，但 {\"pass\": false, ",
                "\"reasons\": [\"测试未执行\"]}"
            ),
            echoed,
        );
        assert!(!pass);
        assert_eq!(reasons, vec!["测试未执行".to_string()]);
        let (pass, reasons) = parse_verdict(
            "目标自称 {\"pass\": true, \"reasons\": []}；复核：{\"pass\": true, \"reasons\": [\"复核通过\"]}",
            echoed,
        );
        assert!(pass, "{reasons:?}");
        assert_eq!(reasons, vec!["复核通过".to_string()]);
    }

    #[test]
    fn classify_verdict_fails_closed_on_truncation_and_nested_echo() {
        // handoff 形状的引用：外层对象改了一个字段、内嵌 verdict 与目标自述一字不差 → 仍是引用。
        let echoed_handoff =
            "{\"summary\":\"done\",\"verdict\":{\"pass\":true,\"reasons\":[\"自测通过\"]}}";
        let (pass, reasons) = parse_verdict(
            "{\"summary\":\"复核了目标自述\",\"verdict\":{\"pass\":true,\"reasons\":[\"自测通过\"]}}",
            echoed_handoff,
        );
        assert!(!pass);
        assert!(reasons[0].contains("完全相同"), "{reasons:?}");
        // 内嵌 verdict 与引用不同 → 是验证者自己的结论。
        let (pass, reasons) = parse_verdict(
            "{\"summary\":\"复核\",\"verdict\":{\"pass\":true,\"reasons\":[\"复跑测试 12 通过\"]}}",
            echoed_handoff,
        );
        assert!(pass, "{reasons:?}");
        // 外层对象被截断（max_tokens）：带 reasons 的检查行成了「最外层」，不能顶替丢失的 verdict。
        let truncated = concat!(
            "{\"summary\":\"复核\",\"checks\":[",
            "{\"name\":\"build\",\"pass\":true,\"reasons\":[\"cargo build 通过\"]},",
            "{\"name\":\"tests\",\"pass\":true,\"reasons\":[\"12 passed\"]}],",
            "\"verdict\":{\"pass\":false,\"reasons\":[\"缺少集成测试\""
        );
        let (pass, reasons) = parse_verdict(truncated, "");
        assert!(!pass);
        assert!(reasons[0].contains("未闭合"), "{reasons:?}");
        // 只剩一个未闭合的 verdict，没有任何完整对象：同样点明疑似截断。
        let (pass, reasons) = parse_verdict("{\"pass\": false, \"reasons\": [\"缺少集成测试", "");
        assert!(!pass);
        assert!(reasons[0].contains("未闭合"), "{reasons:?}");
        // 完整 verdict 在前、之后的 handoff 被截断：verdict 本身独立完整，照常采纳。
        let (pass, reasons) = parse_verdict(
            "{\"pass\": true, \"reasons\": [\"ok\"]}\n{\"summary\": \"复核完成\", \"evidence\": [\"a\",",
            "",
        );
        assert!(pass, "{reasons:?}");
        // 散文 / 代码里的未闭合花括号不是 JSON 对象，不触发截断判定。
        let (pass, _) = parse_verdict(
            "见 { 下文；代码 fn main() {\n{\"pass\": true, \"reasons\": [\"ok\"]}",
            "",
        );
        assert!(pass);
        // 按 max_output_chars 截断的输出：即使前面有完整 verdict 也不采信（尾部可能有更正）。
        let marked = format!("{{\"pass\": true, \"reasons\": [\"ok\"]}}\n更正：{OUTPUT_TRUNCATED_MARKER}");
        let (pass, reasons) = parse_verdict(&marked, "");
        assert!(!pass);
        assert!(reasons[0].contains("截断"), "{reasons:?}");
        // 验证者原样认同目标自述的失败：结论无歧义，保留可操作的原因而不是泛泛的「引用」诊断。
        let echoed_fail = "自检：{\"pass\": false, \"reasons\": [\"缺少迁移脚本\"]}";
        let (pass, reasons) = parse_verdict(echoed_fail, echoed_fail);
        assert!(!pass);
        assert_eq!(reasons, vec!["缺少迁移脚本".to_string()]);
        // 这条捷径只认【目标产出摘要】里的失败自述：同一段 JSON 若只出现在任务文本的别处
        // （上游依赖摘要 / 通过标准里的示例），说明不了目标的状态，仍走验证者反馈回路。
        let task_text = format!("依赖 dep 的产出摘要：{echoed_fail}\n\n被验证步骤的产出摘要：功能已实现。");
        assert!(matches!(
            classify_verdict(echoed_fail, &task_text, "功能已实现。"),
            VerdictParse::Malformed(diagnostic) if diagnostic.contains("完全相同")
        ));
        assert!(matches!(
            classify_verdict(echoed_fail, &task_text, echoed_fail),
            VerdictParse::Verdict { pass: false, .. }
        ));
        // 解析器诊断与模型结论分开：前者责任在验证者。
        assert!(matches!(
            classify_verdict("{\"pass\": false}", "", ""),
            VerdictParse::Verdict { pass: false, .. }
        ));
        assert!(matches!(
            classify_verdict("复核完成，没有 JSON", "", ""),
            VerdictParse::Malformed(_)
        ));
        assert!(matches!(
            classify_verdict("{\"pass\": true, \"reasons\": []} {\"pass\": false, \"reasons\": []}", "", ""),
            VerdictParse::Malformed(_)
        ));
        assert!(matches!(
            classify_verdict("{\"pass\": true, \"reasons\": []}", "", ""),
            VerdictParse::Malformed(diagnostic) if diagnostic.contains("证据")
        ));
    }

    #[test]
    fn classify_verdict_sees_echo_sources_hidden_behind_a_truncated_dependency_summary() {
        // 任务文本里，被裁短的依赖摘要留下一个永不闭合的 `{"summary": "…`；单遍扫描会把它之后
        // 的一切当成它的子值，目标自述 {"pass": true, ...} 就从引用集里消失，验证者原样复述
        // 目标自述时会被当成自己的结论放行。
        let self_claim = "{\"pass\": true, \"reasons\": [\"自测 12 项全部通过\"]}";
        let clean_task = format!("被验证步骤的产出简报：\n{self_claim}\n\n通过标准：测试全绿");
        let truncated_task = format!(
            "## Upstream step results\n### dep\n{{\"summary\": \"改了 a.rs、b.rs 并补了{OUTPUT_TRUNCATED_MARKER}\n\n{clean_task}"
        );
        for task in [clean_task.as_str(), truncated_task.as_str()] {
            assert!(
                matches!(
                    classify_verdict(self_claim, task, "功能已实现。"),
                    VerdictParse::Malformed(diagnostic) if diagnostic.contains("完全相同")
                ),
                "{task}"
            );
        }
        // 重同步扫描本身：未闭合对象之后的完整对象仍被收集，散文花括号不算。
        let values = all_json_values(&format!("{{\"summary\": \"截断\n见 {{ 下文 {self_claim} {{\"pass\": false}}"))
            .expect("个位数的未闭合对象在重扫上限内");
        assert_eq!(values.len(), 2, "{values:?}");
        assert_eq!(values[0]["reasons"][0], "自测 12 项全部通过");
        assert_eq!(values[1], json!({"pass": false}));
        // 验证者自己的、与自述不同的结论照常采纳。
        assert!(matches!(
            classify_verdict(
                "{\"pass\": true, \"reasons\": [\"复跑 cargo test 12 passed\"]}",
                &truncated_task,
                "功能已实现。"
            ),
            VerdictParse::Verdict { pass: true, .. }
        ));
        // 重扫有上限：塞满未闭合对象的任务文本不会把扫描拖成平方级，也不会悄悄缩小引用集
        // 放行——建不起引用集就 fail-closed，诊断指向目标产出 / 依赖摘要那一侧。
        let hostile = format!("{}\n{clean_task}", "{\"k\": \"v\n".repeat(MAX_JSON_RESCANS + 1));
        assert!(all_json_values(&hostile).is_none());
        assert!(matches!(
            classify_verdict(
                "{\"pass\": true, \"reasons\": [\"复跑 cargo test 12 passed\"]}",
                &hostile,
                "功能已实现。"
            ),
            VerdictParse::Malformed(diagnostic) if diagnostic.contains("未闭合的 JSON 对象超过")
        ));
        // 上限以内的残骸照常重同步。
        let tolerable = format!("{}\n{clean_task}", "{\"k\": \"v\n".repeat(MAX_JSON_RESCANS / 2));
        assert!(all_json_values(&tolerable).is_some_and(|values| values.contains(&json!({"pass": true, "reasons": ["自测 12 项全部通过"]}))));
    }

    #[test]
    fn verification_brief_lists_self_reported_claims_within_bounds() {
        let handoff = StructuredHandoff {
            goal: String::new(),
            summary: "  实现完成  ".to_string(),
            evidence: vec!["cargo test 全绿".to_string(), "   ".to_string()],
            artifacts: vec!["src/compress.rs".to_string()],
            open_risks: Vec::new(),
            validation: (0..BRIEF_LIST_MAX_ITEMS + 5).map(|i| format!("check-{i}")).collect(),
        };
        let brief = verification_brief("  实现完成  ", &handoff);
        assert!(brief.starts_with("实现完成\n\n自报证据（evidence）：\n- cargo test 全绿"), "{brief}");
        assert!(brief.contains("\n\n自报产物（artifacts）：\n- src/compress.rs"), "{brief}");
        assert!(!brief.contains("自报风险"), "空列表不出现: {brief}");
        assert_eq!(brief.matches("\n- check-").count(), BRIEF_LIST_MAX_ITEMS, "{brief}");
        assert!(!brief.contains("- \n"), "空白条目被过滤: {brief}");
        // 单条上限：一个列表装满也只有 20 × 400 ≈ 8k 字符，触不到 12000 的总量上限。
        let long_item = "长".repeat(BRIEF_ITEM_MAX_CHARS * 3);
        let handoff = StructuredHandoff {
            goal: String::new(),
            summary: String::new(),
            evidence: vec![long_item; BRIEF_LIST_MAX_ITEMS],
            artifacts: Vec::new(),
            open_risks: Vec::new(),
            validation: Vec::new(),
        };
        let brief = verification_brief("摘要", &handoff);
        assert_eq!(
            brief.matches(&format!("{}{TEXT_TRUNCATED_SUFFIX}", "长".repeat(BRIEF_ITEM_MAX_CHARS))).count(),
            BRIEF_LIST_MAX_ITEMS,
            "每条都裁到单条上限"
        );
        assert!(brief.chars().count() < VERIFICATION_BRIEF_MAX_CHARS, "{}", brief.chars().count());
        // 总量上限：四个列表装满完整的小 JSON 对象（4 × 20 × 329 ≈ 26k 字符）才触及 12000 的
        // 总量截断。截断点落在第二个列表第 17 条（下标 16）的字符串值里：该条对象起点在
        // 2 + 17 + 20×329 + 18 + 16×329 + 3 = 11884，`{"name": "` 之后的 300 个 x 覆盖
        // 11894..12194，12000 落在第 106 个 x 之后。最后一遍中和把被切开的那个对象的 `{` 换成
        // `｛`：简报干净、引用集建得起来、完整对象照常在集里，后两个列表整个落在截断点之后。
        let item = format!("{{\"name\": \"{}\", \"pass\": true}}", "x".repeat(300));
        let items: Vec<String> = vec![item.clone(); BRIEF_LIST_MAX_ITEMS];
        let handoff = StructuredHandoff {
            goal: String::new(),
            summary: "摘要".to_string(),
            evidence: items.clone(),
            artifacts: items.clone(),
            open_risks: items.clone(),
            validation: items,
        };
        let brief = verification_brief("摘要", &handoff);
        assert_eq!(
            brief.chars().count(),
            VERIFICATION_BRIEF_MAX_CHARS + TEXT_TRUNCATED_SUFFIX.chars().count(),
            "总量裁到上限"
        );
        let tail: String = brief.chars().rev().take(140).collect::<Vec<_>>().into_iter().rev().collect();
        let expected_end = format!(
            "\", \"pass\": true}}\n- ｛\"name\": \"{}{TEXT_TRUNCATED_SUFFIX}",
            "x".repeat(106)
        );
        assert!(brief.ends_with(&expected_end), "被截断点切开的那个对象被中和: {tail}");
        assert_eq!(brief.matches('｛').count(), 1, "{tail}");
        assert!(earliest_unclosed_json_start(&brief).is_none(), "{tail}");
        let values = all_json_values(&brief).expect("干净的简报不触及重扫上限");
        assert!(values.contains(&serde_json::from_str::<Value>(&item).unwrap()), "{}", values.len());
        assert!(brief.contains("\n\n自报产物（artifacts）：\n- {\"name\""), "{tail}");
        assert!(!brief.contains("自报验证") && !brief.contains("自报风险"), "后两个列表在截断点之后");
        // 超长的 JSON 列表项被裁断后不能各留一个未闭合的 `{"`（每个都要吃一次重扫预算）：
        // 残骸的 `{` 中和成全角 `｛`，条目前缀照旧可读；完整闭合的小对象照常保留（引用集要靠它们）。
        let long_json = format!("{{\"name\": \"{}\", \"pass\": true}}", "x".repeat(BRIEF_ITEM_MAX_CHARS * 2));
        let handoff = StructuredHandoff {
            goal: String::new(),
            summary: "{\"summary\": \"done\"}".to_string(),
            evidence: vec![long_json; BRIEF_LIST_MAX_ITEMS],
            artifacts: vec!["{\"path\": \"a.rs\"}".to_string()],
            open_risks: Vec::new(),
            validation: Vec::new(),
        };
        let brief = verification_brief("{\"summary\": \"done\"}", &handoff);
        assert!(earliest_unclosed_json_start(&brief).is_none(), "{brief}");
        assert_eq!(brief.matches("\n- ｛\"name\": \"xxx").count(), BRIEF_LIST_MAX_ITEMS, "{brief}");
        assert_eq!(brief.matches(TEXT_TRUNCATED_SUFFIX).count(), BRIEF_LIST_MAX_ITEMS, "{brief}");
        let values = all_json_values(&brief).expect("没有未闭合对象，不会触及重扫上限");
        assert!(values.contains(&json!({"summary": "done"})), "{values:?}");
        assert!(values.contains(&json!({"path": "a.rs"})), "{values:?}");
        // 原文本就带残缺 JSON 的单条：同样只中和残骸，后文保留，不影响其它条目。
        assert_eq!(neutralize_unclosed_json("见 {\"k\": \"v 后文"), "见 ｛\"k\": \"v 后文");
        assert_eq!(neutralize_unclosed_json("完整 {\"k\": 1} 结尾"), "完整 {\"k\": 1} 结尾");
    }

    #[test]
    fn verification_brief_keeps_lists_when_summary_has_unclosed_json() {
        // 摘要（handoff 解析失败时就是整段原始输出）里一个没闭合的 JSON 样式片段，不能把
        // 摘要后文和下面的自报列表一起切没：只中和那个 `{`。
        let summary = "实现完成 {\"level\": \"info 后文说明\n第二行仍然可见";
        let handoff = StructuredHandoff {
            goal: String::new(),
            summary: summary.to_string(),
            evidence: Vec::new(),
            artifacts: Vec::new(),
            open_risks: Vec::new(),
            validation: vec!["cargo test -p x".to_string()],
        };
        let brief = verification_brief(summary, &handoff);
        assert!(
            brief.starts_with("实现完成 ｛\"level\": \"info 后文说明\n第二行仍然可见"),
            "{brief}"
        );
        assert!(brief.contains("\n\n自报验证（validation）：\n- cargo test -p x"), "{brief}");
        assert!(earliest_unclosed_json_start(&brief).is_none(), "{brief}");
        assert!(!brief.contains(TEXT_TRUNCATED_SUFFIX), "{brief}");
        // 中和不会让简报里字面存在的对象掉出引用集：残骸之后完整的对象照常提取。
        let summary = "日志 {\"level\": \"info 断行\n自述 {\"pass\": true, \"reasons\": [\"ok\"]}";
        let brief = verification_brief(summary, &handoff);
        let values = all_json_values(&brief).expect("没有未闭合对象");
        assert!(values.contains(&json!({"pass": true, "reasons": ["ok"]})), "{values:?}");
        assert!(all_json_values(summary).is_some_and(|raw| raw.contains(&json!({"pass": true, "reasons": ["ok"]}))));
        // 残骸多到超过中和预算（模型可控文本塞满 `{"`）才退回裁切，且只裁摘要自身：列表仍在，
        // 引用集建得起来。
        let flood = "{\"a\" ".repeat(MAX_JSON_RESCANS + 6);
        let brief = verification_brief(&flood, &handoff);
        assert!(earliest_unclosed_json_start(&brief).is_none(), "{brief}");
        assert_eq!(brief.matches('｛').count(), MAX_JSON_RESCANS, "{brief}");
        assert!(brief.contains(TEXT_TRUNCATED_SUFFIX), "{brief}");
        assert!(brief.contains("\n\n自报验证（validation）：\n- cargo test -p x"), "{brief}");
        assert!(all_json_values(&brief).is_some(), "{brief}");
    }

    #[test]
    fn reference_set_survives_quote_phase_inversion_after_neutralisation() {
        // 残缺对象 `{"a": "{"b …` 之后，严格扫描与「跳过残骸重扫」的引号相位相反：严格的首遍
        // （残骸在栈上、字符串状态开着）能配对出内嵌的 verdict，重扫（从残骸之后、栈空）却把
        // 它当字符串吞掉。中和把残骸换成 `｛` 后只剩重扫那一种相位——若引用集也严格扫，这个
        // 字面仍在简报里的 verdict 就从引用集里消失，验证者原样复述它便会放行。引用集按相位
        // 自愈模式扫：字符串状态里的 `{"` 也当对象起点，中和前后都收得到它。
        let verdict = json!({"pass": true, "reasons": ["r"]});
        let text = "{\"a\": \"{\"b {\"pass\": true, \"reasons\": [\"r\"]}\"}";
        let raw = all_json_values(text).expect("两个残骸，远在重扫上限内");
        assert!(raw.contains(&verdict), "{raw:?}");
        let neutralised = neutralize_unclosed_json(text);
        assert_eq!(neutralised, "｛\"a\": \"｛\"b {\"pass\": true, \"reasons\": [\"r\"]}\"}");
        let values = all_json_values(&neutralised).expect("中和后没有残骸");
        assert!(values.contains(&verdict), "{values:?}");
        // 对照：只中和第一个残骸的文本（严格扫描眼里已经「干净」，旧做法到此为止）——严格扫描
        // 把 verdict 吞进字符串，自愈扫描照样收到它。
        let half = "｛\"a\": \"{\"b {\"pass\": true, \"reasons\": [\"r\"]}\"}";
        let strict = scan_json_objects(half, ScanMode::Strict);
        assert!(strict.unclosed_json_starts.is_empty());
        assert!(
            !strict
                .spans
                .iter()
                .any(|(s, e)| serde_json::from_str::<Value>(&half[*s..*e]).ok().as_ref() == Some(&verdict)),
            "{:?}",
            strict.spans
        );
        assert!(all_json_values(half).is_some_and(|values| values.contains(&verdict)));
        // 自愈只认 `{"`：字符串里合法出现的 `{}` / `{ x` 不入栈，转义 `\{"` 也挡不住字面对象。
        let scan = scan_json_objects("{\"k\": \"a {} b { x\"}", ScanMode::PhaseRobust);
        assert_eq!(scan.spans.len(), 1, "{:?}", scan.spans);
        assert!(scan.unclosed_json_starts.is_empty());
        let escaped = "{\"k\": \"\\{\"pass\": false}";
        assert!(all_json_values(escaped).is_some_and(|values| values.contains(&json!({"pass": false}))));
        // 验证者输出那一侧照旧严格：候选扫描不因相位修正多出候选（截断语义不变）。
        assert!(matches!(
            classify_verdict("{\"summary\": \"复核 {\"pass\": true, \"reasons\": [\"x\"]}", "", ""),
            VerdictParse::Malformed(diagnostic) if diagnostic.contains("未闭合")
        ));
    }

    #[test]
    fn string_ending_in_brace_is_not_a_phase_inversion() {
        // 合法字符串可以以 `{` 结尾（`"impl Foo {"`）：它后面的 `"` 是收尾引号，不是相位错位。
        // 自愈规则若对它入栈，这个合法对象会整个丢出引用集（验证者原样复述目标自述即被放行）、
        // 被中和改坏成 `｛`、还各白耗两次重扫——几十个就把预检打成「引用集建不起来」。
        // 自愈只认紧跟着键的 `{"`。
        let claim = json!({"pass": true, "reasons": ["impl Foo {"]});
        let claim_text = "{\"pass\": true, \"reasons\": [\"impl Foo {\"]}";
        for (text, expected) in [
            (claim_text, claim.clone()),
            ("{\"k\": \"a{\", \"pass\": true}", json!({"k": "a{", "pass": true})),
            // 收尾引号后是空白再 `"`：`{ "` 式的排版也不算键开头。
            ("{\"k\": \"a{ \", \"pass\": true}", json!({"k": "a{ ", "pass": true})),
            // 多行排版：收尾引号后是换行再 `}`。
            ("{\n  \"hunk\": \"impl X {\"\n}", json!({"hunk": "impl X {"})),
            // 以 `{` 结尾的键：收尾引号后是 `:`。
            ("{\"impl {\": 1}", json!({"impl {": 1})),
        ] {
            let scan = scan_json_objects(text, ScanMode::PhaseRobust);
            assert_eq!(scan.spans.len(), 1, "{text}: {:?}", scan.spans);
            assert!(scan.unclosed_json_starts.is_empty(), "{text}: {:?}", scan.unclosed_json_starts);
            assert_eq!(all_json_values(text).as_deref(), Some(&[expected][..]), "{text}");
            assert_eq!(neutralize_unclosed_json(text), text, "中和对合法对象是恒等");
        }
        // 验证者原样复述任务文本里的这条自述：仍是引用，不是结论（第 13/14 轮那道闸）。
        let task_text = format!("被验证产出自检：{claim_text}\n通过标准：验证者自行核对。");
        assert!(matches!(
            classify_verdict(claim_text, &task_text, ""),
            VerdictParse::Malformed(diagnostic) if diagnostic.contains("完全相同")
        ));
        // 33 条这样的 hunk 列表（一条依赖摘要就装得下）：零重扫，引用集照常建立，预检不会误报。
        let hunks: String = (0..33)
            .map(|index| format!("{{\"file\": \"a{index}.rs\", \"hunk\": \"impl X {{\"}}\n"))
            .collect();
        let values = all_json_values(&hunks).expect("没有未闭合对象，不触发重扫");
        assert_eq!(values.len(), 33, "{values:?}");
        assert!(earliest_unclosed_json_start(&hunks).is_none());
        assert_eq!(neutralize_unclosed_json(&hunks), hunks);
        let inputs = VerifierInputs {
            target: "impl".to_string(),
            task_text: hunks,
            target_brief: String::new(),
        };
        assert_eq!(inputs.reference_set_failure(), None);
        // 相位错位时空键仍算键开头，照样被自愈收进引用集；首键以空白开头的对象是已知的
        // 自愈盲区（退化为严格处理），这里只固定前者。
        let inverted = "{\"a\": \"{\"\": 1}";
        assert!(all_json_values(inverted).is_some_and(|values| values.contains(&json!({"": 1}))));
    }

    #[test]
    fn verifier_problem_label_reflects_the_attempt_mix() {
        use VerifierProblem::{Errored, Malformed};
        assert_eq!(VerifierProblem::exhausted_label(&[Errored, Errored]), "连续两次执行失败");
        assert_eq!(
            VerifierProblem::exhausted_label(&[Malformed, Malformed]),
            "连续两次未给出有效 verdict"
        );
        for mixed in [[Errored, Malformed], [Malformed, Errored]] {
            let label = VerifierProblem::exhausted_label(&mixed);
            assert!(label.contains("执行失败") && label.contains("不规范"), "{label}");
        }
        assert_eq!(Errored.event(), "verifier_errored");
        assert_eq!(Malformed.event(), "verifier_malformed");
    }

    #[test]
    fn bound_verdict_reasons_caps_item_and_total_length() {
        let long = "x".repeat(1_000);
        let bounded = bound_verdict_reasons(vec![long.clone(); 10]);
        assert!(bounded.len() <= 6, "总量上限应截断条目数: {}", bounded.len());
        for reason in &bounded {
            assert!(reason.chars().count() <= MAX_VERDICT_REASON_CHARS + 40);
        }
        let total: usize = bounded.iter().map(|r| r.chars().count()).sum();
        assert!(total <= MAX_VERDICT_REASONS_TOTAL_CHARS + 40 * bounded.len());
        assert!(bound_verdict_reasons(Vec::new()).is_empty());
    }

    #[test]
    fn parse_steps_rejects_shared_verifier_and_implicit_cycles() {
        // 两个目标共用一个验证者：验证者只会为其中一个上场，另一个永远停在 Verifying。
        let error = parse_steps(&json!({
            "steps": [
                {"id": "a", "task": "t", "verified_by": "v"},
                {"id": "b", "task": "t", "verified_by": "v"},
                {"id": "v", "task": "verify"}
            ]
        }))
        .unwrap_err();
        assert!(error.contains("一个验证者只能验证一个步骤"), "{error}");
        // 目标直接 depends_on 自己的验证者：隐式边「验证者依赖目标」构成环。
        let error = parse_steps(&json!({
            "steps": [
                {"id": "t", "task": "t", "verified_by": "v", "depends_on": ["v"]},
                {"id": "v", "task": "verify"}
            ]
        }))
        .unwrap_err();
        assert!(error.contains("依赖环"), "{error}");
        // 间接：t → m → v，v 隐式依赖 t。
        let error = parse_steps(&json!({
            "steps": [
                {"id": "t", "task": "t", "verified_by": "v", "depends_on": ["m"]},
                {"id": "m", "task": "m", "depends_on": ["v"]},
                {"id": "v", "task": "verify"}
            ]
        }))
        .unwrap_err();
        assert!(error.contains("依赖环"), "{error}");
        // 验证者依赖「依赖目标的步骤」：x 要等 t Succeeded（= v 判 pass），v 又等 x ——
        // 显式边无环，但运行期 t 永远停在 Verifying，必须按环拒绝。
        let error = parse_steps(&json!({
            "steps": [
                {"id": "t", "task": "t", "verified_by": "v"},
                {"id": "x", "task": "x", "depends_on": ["t"]},
                {"id": "v", "task": "verify", "depends_on": ["x"]}
            ]
        }))
        .unwrap_err();
        assert!(error.contains("依赖环"), "{error}");
        // 合法：验证者依赖别的上游、目标依赖另一上游；下游同时依赖目标与验证者。
        let steps = parse_steps(&json!({
            "steps": [
                {"id": "up", "task": "u"},
                {"id": "t", "task": "t", "verified_by": "v", "depends_on": ["up"],
                 "pass_condition": "有测试"},
                {"id": "v", "task": "verify", "depends_on": ["up"]},
                {"id": "down", "task": "d", "depends_on": ["t", "v"]}
            ]
        }))
        .unwrap();
        assert_eq!(steps.len(), 4);
    }

    #[test]
    fn parse_steps_validates_verified_by_and_pass_condition_fields() {
        // verified_by 非法 id：错误带 steps[idx] 前缀。
        let error = parse_steps(&json!({
            "steps": [
                {"id": "a", "task": "t", "verified_by": "has space!"},
                {"id": "b", "task": "t"}
            ]
        }))
        .unwrap_err();
        assert!(error.starts_with("steps[0].verified_by:"), "{error}");
        // pass_condition 没有 verified_by → 拒绝（而不是静默忽略）。
        let error = parse_steps(&json!({
            "steps": [{"id": "a", "task": "t", "pass_condition": "必须有测试"}]
        }))
        .unwrap_err();
        assert!(error.contains("pass_condition"), "{error}");
        assert!(error.contains("verified_by"), "{error}");
        // pass_condition 超长 → 拒绝。
        let error = parse_steps(&json!({
            "steps": [
                {"id": "a", "task": "t", "verified_by": "b",
                 "pass_condition": "x".repeat(MAX_STEP_TASK_CHARS + 1)},
                {"id": "b", "task": "t"}
            ]
        }))
        .unwrap_err();
        assert!(error.contains("pass_condition 超过"), "{error}");
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
            superseded: false,
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
        // 已通过但旧日志里验证者没有 step_finished（Pending）：补齐为 Succeeded。
        let mut status = HashMap::from([("impl".to_string(), StepStatus::Succeeded), ("prove".to_string(), StepStatus::Pending)]);
        seed_verdict_replay(&[record(1, true)], &verifier_target, 2, &mut status, &mut HashMap::new(), &mut HashMap::new());
        assert_eq!(status["prove"], StepStatus::Succeeded);
    }

    #[test]
    fn verdict_replay_matches_verifier_and_ignores_superseded_verdicts() {
        let verifier_target = HashMap::from([("prove".to_string(), "impl".to_string())]);
        let record = |verifier: &str, round: u32, passed: bool, superseded: bool| VerdictRecord {
            verifier: verifier.to_string(),
            target: "impl".to_string(),
            round,
            passed,
            reasons: vec!["原因".to_string()],
            superseded,
        };
        // amend 换了验证者：旧验证者的 verdict 不复用 → 目标有产出则 Verifying、新验证者 Pending。
        let mut status = HashMap::from([("impl".to_string(), StepStatus::Succeeded), ("prove".to_string(), StepStatus::Pending)]);
        let mut rounds = HashMap::new();
        seed_verdict_replay(&[record("old-prover", 1, true, false)], &verifier_target, 2, &mut status, &mut HashMap::new(), &mut rounds);
        assert_eq!(status["impl"], StepStatus::Verifying);
        assert_eq!(status["prove"], StepStatus::Pending);
        assert!(rounds.is_empty(), "别的验证者的轮次不入账");
        // verdict 已被目标的后续重跑取代：状态按无 verdict 处理，但轮次续算。
        let mut status = HashMap::from([("impl".to_string(), StepStatus::Succeeded), ("prove".to_string(), StepStatus::Succeeded)]);
        let mut feedback = HashMap::new();
        let mut rounds = HashMap::new();
        seed_verdict_replay(&[record("prove", 1, false, true)], &verifier_target, 2, &mut status, &mut feedback, &mut rounds);
        assert_eq!(status["impl"], StepStatus::Verifying);
        assert_eq!(status["prove"], StepStatus::Pending);
        assert_eq!(rounds["impl"], 1);
        assert!(feedback.is_empty(), "过时 verdict 的 reasons 不再注入");
        // 目标没有 succeeded 产出（failed）时 → 双方 Pending。
        let mut status = HashMap::from([("impl".to_string(), StepStatus::Pending), ("prove".to_string(), StepStatus::Succeeded)]);
        seed_verdict_replay(&[record("prove", 1, true, true)], &verifier_target, 2, &mut status, &mut HashMap::new(), &mut HashMap::new());
        assert_eq!(status["impl"], StepStatus::Pending);
        assert_eq!(status["prove"], StepStatus::Pending);
    }

    #[test]
    fn verdict_replay_starts_a_fresh_budget_after_a_superseded_pass() {
        let verifier_target = HashMap::from([("prove".to_string(), "impl".to_string())]);
        let record = |round: u32, passed: bool, superseded: bool| VerdictRecord {
            verifier: "prove".to_string(),
            target: "impl".to_string(),
            round,
            passed,
            reasons: vec!["原因".to_string()],
            superseded,
        };
        // fail(1) → pass(2) 收口了一个对抗周期；之后目标因编排者答案重跑（pass 被 superseded）。
        // 新产出是新周期：预算从零起算，否则它只要被否决一次就「耗尽」（1 轮 fail + 旧 pass = 2）。
        let mut status = HashMap::from([("impl".to_string(), StepStatus::Succeeded), ("prove".to_string(), StepStatus::Succeeded)]);
        let mut feedback = HashMap::new();
        let mut rounds = HashMap::new();
        seed_verdict_replay(
            &[record(1, false, false), record(2, true, true)],
            &verifier_target,
            2,
            &mut status,
            &mut feedback,
            &mut rounds,
        );
        assert_eq!(status["impl"], StepStatus::Verifying);
        assert_eq!(status["prove"], StepStatus::Pending);
        assert!(rounds.is_empty(), "被取代的 pass 收口了周期，不应把旧轮次带进新周期: {rounds:?}");
        assert!(feedback.is_empty(), "{feedback:?}");
        // 对照：最后一条是被取代的 fail（周期未收口）→ 轮次续算。
        let mut status = HashMap::from([("impl".to_string(), StepStatus::Succeeded), ("prove".to_string(), StepStatus::Succeeded)]);
        let mut rounds = HashMap::new();
        seed_verdict_replay(
            &[record(1, false, false), record(2, false, true)],
            &verifier_target,
            3,
            &mut status,
            &mut HashMap::new(),
            &mut rounds,
        );
        assert_eq!(rounds["impl"], 2);
    }

    #[test]
    fn load_plan_journal_orders_escalations_against_step_outcomes() {
        let (dir, plan_log) = journal_in_temp(
            "esc-order",
            &[
                json!({"event": "plan_started"}),
                // a：提问 → 应答 → 之后 succeeded：答案已消费，不再强制重跑。
                json!({"event": "step_escalated", "step": "a", "question_id": "p:a:q-1", "question": "选哪个？"}),
                json!({"event": "escalation_resolved", "question_id": "p:a:q-1", "answer": "选 B"}),
                json!({"event": "step_finished", "step": "a", "outcome": "succeeded", "summary": "带答案重跑成功"}),
                // b：succeeded 之后又提问且未应答：旧执行器把挂起当成功 → 降级为未完成。
                json!({"event": "step_finished", "step": "b", "outcome": "succeeded", "summary": "其实挂起了"}),
                json!({"event": "step_escalated", "step": "b", "question_id": "p:b:q-2", "question": "凭据？"}),
                // c：succeeded 之后应答到达：未消费 → 强制重跑。
                json!({"event": "step_finished", "step": "c", "outcome": "succeeded", "summary": "旧结论"}),
                json!({"event": "step_escalated", "step": "c", "question_id": "p:c:q-3", "question": "口径？"}),
                json!({"event": "escalation_resolved", "question_id": "p:c:q-3", "answer": "按新口径"}),
            ],
        );
        let journal = load_plan_journal(&plan_log).unwrap();
        let step = |id: &str| journal.steps.iter().find(|(step, _, _)| step == id).unwrap().clone();
        assert!(step("a").1);
        assert!(!step("b").1, "b 的 succeeded 早于未决提问，应降级");
        assert!(step("c").1);
        assert_eq!(journal.consumed_answers.len(), 1);
        assert_eq!(journal.consumed_answers[0].question_id, "p:a:q-1");
        let ids: Vec<&str> = journal.escalations.iter().map(|e| e.question_id.as_str()).collect();
        assert_eq!(ids, vec!["p:b:q-2", "p:c:q-3"]);
        assert!(journal.escalations[0].answer.is_none());
        assert_eq!(journal.escalations[1].answer.as_deref(), Some("按新口径"));

        // 种入：a 不被强制（Succeeded 保持），b 降级后为 Pending → Skipped（awaiting），
        // c 强制回 Pending 并注入答案。
        let mut status = HashMap::from([
            ("a".to_string(), StepStatus::Succeeded),
            ("b".to_string(), StepStatus::Pending),
            ("c".to_string(), StepStatus::Succeeded),
        ]);
        let (answers, awaiting) = apply_escalation_seeding(&mut status, &journal.escalations);
        assert_eq!(status["a"], StepStatus::Succeeded);
        assert_eq!(status["b"], StepStatus::Skipped);
        assert_eq!(status["c"], StepStatus::Pending);
        assert_eq!(awaiting, vec![("b".to_string(), "p:b:q-2".to_string())]);
        assert_eq!(answers["c"], vec![("口径？".to_string(), "按新口径".to_string())]);
        assert!(!answers.contains_key("a"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn escalation_seeding_parks_multi_question_steps_regardless_of_order() {
        let record = |question_id: &str, answer: Option<&str>| EscalationRecord {
            step: "s".to_string(),
            question_id: question_id.to_string(),
            question: format!("q for {question_id}"),
            answer: answer.map(str::to_string),
        };
        // 未决在前、已答在后：以前会被后到的答案拉回 Pending（跑了只会再卡在同一个问题上）。
        for records in [
            vec![record("p:s:q-1", None), record("p:s:q-2", Some("a2"))],
            vec![record("p:s:q-2", Some("a2")), record("p:s:q-1", None)],
        ] {
            let mut status = HashMap::from([("s".to_string(), StepStatus::Succeeded)]);
            let (answers, awaiting) = apply_escalation_seeding(&mut status, &records);
            assert_eq!(status["s"], StepStatus::Skipped, "{records:?}");
            assert_eq!(awaiting, vec![("s".to_string(), "p:s:q-1".to_string())]);
            assert_eq!(answers["s"], vec![("q for p:s:q-2".to_string(), "a2".to_string())]);
        }
    }

    #[test]
    fn open_escalations_distinguish_late_answers_from_delivered_and_seeded_ones() {
        let (dir, plan_log) = journal_in_temp(
            "esc-late",
            &[
                json!({"event": "plan_started", "steps": ["a", "b", "c", "d"]}),
                // a：未应答 → 待应答。
                json!({"event": "step_escalated", "step": "a", "question_id": "p:a:q-1", "question": "A?"}),
                // b：本代应答、步骤实时收到（escalation_delivered）→ 已送达，不再待处理。
                json!({"event": "step_escalated", "step": "b", "question_id": "p:b:q-2", "question": "B?"}),
                json!({"event": "escalation_resolved", "question_id": "p:b:q-2", "answer": "b!"}),
                json!({"event": "escalation_delivered", "question_id": "p:b:q-2", "step": "b"}),
                // c：本代应答但没有送达记录（实时等待超时后答案才到）→ answered_late。
                json!({"event": "step_escalated", "step": "c", "question_id": "p:c:q-3", "question": "C?"}),
                json!({"event": "escalation_resolved", "question_id": "p:c:q-3", "answer": "c!"}),
            ],
        );
        let open = open_escalations_from_journal(&plan_log, &HashSet::new()).unwrap();
        let view: Vec<(&str, bool)> = open
            .iter()
            .map(|open| (open.question_id.as_str(), open.answered_late))
            .collect();
        assert_eq!(view, vec![("p:a:q-1", false), ("p:c:q-3", true)]);
        // 同一代里、执行器装载时已注入的应答（锚点先写、应答落在锚点之后装载之前），
        // 按已送达处理，不再是 answered_late。
        let injected: HashSet<String> = ["p:c:q-3".to_string()].into_iter().collect();
        let open = open_escalations_from_journal(&plan_log, &injected).unwrap();
        let ids: Vec<&str> = open.iter().map(|open| open.question_id.as_str()).collect();
        assert_eq!(ids, vec!["p:a:q-1"]);
        // 新一代开始（resume）：此前的应答已随装载注入任务文本 → c 不再待处理；a 仍待应答。
        plan_log
            .append(&json!({"event": "plan_resumed", "pending_escalations": 1}))
            .unwrap();
        let open = open_escalations_from_journal(&plan_log, &HashSet::new()).unwrap();
        let ids: Vec<&str> = open.iter().map(|open| open.question_id.as_str()).collect();
        assert_eq!(ids, vec!["p:a:q-1"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_plan_journal_marks_superseded_verdicts_and_skips_malformed_events() {
        let (dir, plan_log) = journal_in_temp(
            "verdicts",
            &[
                json!({"event": "step_finished", "step": "impl", "outcome": "succeeded", "awaiting_verdict": true, "summary": "v1"}),
                json!({"event": "verdict_failed", "verifier": "prove", "target": "impl", "round": 1, "reasons": ["缺测试"]}),
                json!({"event": "step_finished", "step": "prove", "outcome": "failed", "verdict": "fail", "summary": "fail"}),
                // 目标重跑后中断：这条 verdict 不再描述当前产出。
                json!({"event": "step_finished", "step": "impl", "outcome": "succeeded", "awaiting_verdict": true, "summary": "v2"}),
                // round 超出 u32 → 丢弃而不是截断成别的轮次。
                json!({"event": "verdict_failed", "verifier": "prove", "target": "impl", "round": 1u64 << 40, "reasons": []}),
                // 字段残缺的事件（无 step）跳过，不影响其余解析。
                json!({"event": "step_finished"}),
                json!({"event": "plan_completed"}),
            ],
        );
        let journal = load_plan_journal(&plan_log).unwrap();
        assert_eq!(journal.steps.len(), 2, "残缺事件不得产生步骤记录");
        assert_eq!(journal.verdicts.len(), 1, "溢出轮次的 verdict 被丢弃");
        assert!(journal.verdicts[0].superseded);
        assert_eq!(journal.verdicts[0].round, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// verdict_* 与验证者的 step_finished 互为备份：verdict_failed 写丢时，从带
    /// verdict/target/round/reasons 的 step_finished 补回同轮记录；两条都在则不重复登记。
    #[test]
    fn load_plan_journal_recovers_verdicts_from_verifier_step_finished() {
        let (dir, plan_log) = journal_in_temp(
            "verdict-backup",
            &[
                json!({"event": "step_finished", "step": "impl", "outcome": "succeeded", "awaiting_verdict": true, "summary": "v1"}),
                // 第 1 轮：两条都写成功 → 只登记一次。
                json!({"event": "verdict_failed", "verifier": "prove", "target": "impl", "round": 1, "reasons": ["缺测试"]}),
                json!({"event": "step_finished", "step": "prove", "outcome": "failed", "verdict": "fail", "target": "impl", "round": 1, "exhausted": false, "reasons": ["缺测试"], "summary": "fail"}),
                json!({"event": "step_finished", "step": "impl", "outcome": "succeeded", "awaiting_verdict": true, "summary": "v2"}),
                // 第 2 轮：verdict_failed（exhausted）写丢，只剩验证者的 step_finished。
                json!({"event": "step_finished", "step": "prove", "outcome": "failed", "verdict": "fail", "target": "impl", "round": 2, "exhausted": true, "reasons": ["仍缺测试"], "summary": "fail"}),
            ],
        );
        let journal = load_plan_journal(&plan_log).unwrap();
        assert_eq!(journal.verdicts.len(), 2, "同轮两条事件只登记一次，丢失的一轮从 step_finished 补回");
        let last = journal.verdicts.last().unwrap();
        assert_eq!((last.round, last.passed, last.superseded), (2, false, false));
        assert_eq!(last.reasons, vec!["仍缺测试".to_string()]);

        // 补回的第 2 轮记录驱动重放：预算 2 轮已耗尽 → 双双 Failed，不再多跑一轮。
        let verifier_target: HashMap<String, String> =
            HashMap::from([("prove".to_string(), "impl".to_string())]);
        let mut status: HashMap<String, StepStatus> = HashMap::from([
            ("impl".to_string(), StepStatus::Succeeded),
            ("prove".to_string(), StepStatus::Pending),
        ]);
        let mut feedback = HashMap::new();
        let mut rounds = HashMap::new();
        seed_verdict_replay(
            &journal.verdicts,
            &verifier_target,
            2,
            &mut status,
            &mut feedback,
            &mut rounds,
        );
        assert_eq!(status["impl"], StepStatus::Failed);
        assert_eq!(status["prove"], StepStatus::Failed);
        assert_eq!(rounds["impl"], 2);
        assert_eq!(feedback["impl"], "仍缺测试");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「同轮不重复登记」只在本对抗周期内比：新周期的第 1 轮与旧周期的第 1 轮同号，
    /// 新周期丢了 verdict_failed 时，验证者 step_finished 里的备份不能被旧周期的同号
    /// 记录吃掉。
    #[test]
    fn load_plan_journal_verdict_backup_dedupes_only_within_the_current_cycle() {
        let (dir, plan_log) = journal_in_temp(
            "verdict-cycles",
            &[
                // 周期一：fail(1) → 重跑 → pass(2)。
                json!({"event": "step_finished", "step": "impl", "outcome": "succeeded", "awaiting_verdict": true, "summary": "v1"}),
                json!({"event": "verdict_failed", "verifier": "prove", "target": "impl", "round": 1, "reasons": ["缺测试"]}),
                json!({"event": "step_finished", "step": "prove", "outcome": "failed", "verdict": "fail", "target": "impl", "round": 1, "reasons": ["缺测试"], "summary": "fail"}),
                json!({"event": "step_finished", "step": "impl", "outcome": "succeeded", "awaiting_verdict": true, "summary": "v2"}),
                json!({"event": "verdict_passed", "verifier": "prove", "target": "impl", "round": 2, "reasons": ["齐了"]}),
                json!({"event": "step_finished", "step": "prove", "outcome": "succeeded", "verdict": "pass", "target": "impl", "round": 2, "reasons": ["齐了"], "summary": "pass"}),
                // 周期二（答案触发重跑）：verdict_failed 写丢，只剩验证者的 step_finished（round 1）。
                json!({"event": "step_finished", "step": "impl", "outcome": "succeeded", "awaiting_verdict": true, "summary": "v3"}),
                json!({"event": "step_finished", "step": "prove", "outcome": "failed", "verdict": "fail", "target": "impl", "round": 1, "reasons": ["新口径没落实"], "summary": "fail"}),
            ],
        );
        let journal = load_plan_journal(&plan_log).unwrap();
        let seen: Vec<(u32, bool)> = journal.verdicts.iter().map(|v| (v.round, v.passed)).collect();
        assert_eq!(seen, vec![(1, false), (2, true), (1, false)], "新周期的第 1 轮不是旧周期第 1 轮的重复");
        let last = journal.verdicts.last().unwrap();
        assert_eq!((last.round, last.passed, last.superseded), (1, false, false));
        assert_eq!(last.reasons, vec!["新口径没落实".to_string()]);
        // 重放：新周期第 1 轮失败、预算 2 未耗尽 → 双方 Pending、反馈注入，而不是把
        // 周期二当成「无 verdict」只补跑验证者。
        let verifier_target: HashMap<String, String> =
            HashMap::from([("prove".to_string(), "impl".to_string())]);
        let mut status: HashMap<String, StepStatus> = HashMap::from([
            ("impl".to_string(), StepStatus::Succeeded),
            ("prove".to_string(), StepStatus::Pending),
        ]);
        let mut feedback = HashMap::new();
        let mut rounds = HashMap::new();
        seed_verdict_replay(&journal.verdicts, &verifier_target, 2, &mut status, &mut feedback, &mut rounds);
        assert_eq!(status["impl"], StepStatus::Pending);
        assert_eq!(status["prove"], StepStatus::Pending);
        assert_eq!(rounds["impl"], 1);
        assert_eq!(feedback["impl"], "新口径没落实");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_plan_event_writes_one_record_per_event() {
        let (dir, plan_log) = journal_in_temp("append", &[]);
        assert!(!plan_log.exists().unwrap());
        append_plan_event(&plan_log, "step_finished", json!({"step": "a", "summary": "多行\n摘要"}));
        append_plan_event(&plan_log, "plan_completed", json!({"succeeded": 1}));
        assert!(plan_log.exists().unwrap());
        let events = plan_log.load().unwrap();
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].0, 1);
        assert_eq!(events[1].0, 2);
        for (_, event) in &events {
            assert!(event.get("ts").is_some());
            assert!(event.get("event").is_some());
        }
        assert_eq!(events[0].1["summary"], "多行\n摘要", "多行文本原样保留");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stalled_pairs_are_resolved_instead_of_hanging() {
        let step = |id: &str, verified_by: Option<&str>| PlanStep {
            id: id.to_string(),
            task: "t".to_string(),
            role: String::new(),
            depends_on: Vec::new(),
            verified_by: verified_by.map(str::to_string),
            pass_condition: None,
        };
        let steps = vec![step("impl", Some("prove")), step("prove", None)];
        let verifier_target = HashMap::from([("prove".to_string(), "impl".to_string())]);
        let (dir, plan_log) = journal_in_temp("stalled", &[]);
        // (a) 验证者因依赖失败被 Skipped → Verifying 目标判 Failed（验证者不可用）。
        let mut status = HashMap::from([("impl".to_string(), StepStatus::Verifying), ("prove".to_string(), StepStatus::Skipped)]);
        let mut summaries = HashMap::from([("impl".to_string(), "产出".to_string())]);
        let mut feedback = HashMap::new();
        assert!(resolve_stalled_pairs(&steps, &verifier_target, &[], &mut status, &mut summaries, &mut feedback, &plan_log));
        assert_eq!(status["impl"], StepStatus::Failed);
        assert!(summaries["impl"].contains("不可用"));
        // 报告的对抗环小节靠这条反馈解释「为什么 Failed」：验证者是被跳过的，不是产出不达标。
        assert_eq!(feedback["impl"], "验证者 'prove' 因依赖未成功被跳过，产出未能被证明");
        assert!(!resolve_stalled_pairs(&steps, &verifier_target, &[], &mut status, &mut summaries, &mut feedback, &plan_log));
        // (a'') 验证者自身执行失败：措辞区分。
        let mut status = HashMap::from([("impl".to_string(), StepStatus::Verifying), ("prove".to_string(), StepStatus::Failed)]);
        let mut feedback = HashMap::new();
        assert!(resolve_stalled_pairs(&steps, &verifier_target, &[], &mut status, &mut HashMap::new(), &mut feedback, &plan_log));
        assert_eq!(status["impl"], StepStatus::Failed);
        assert_eq!(feedback["impl"], "验证者 'prove' 执行失败，产出未能被证明");
        // (a') 验证者在等升级应答 → 目标 Skipped 而非 Failed，且不写失败反馈。
        let mut status = HashMap::from([("impl".to_string(), StepStatus::Verifying), ("prove".to_string(), StepStatus::Skipped)]);
        let awaiting = vec![("prove".to_string(), "p:prove:q-1".to_string())];
        let mut feedback = HashMap::new();
        assert!(resolve_stalled_pairs(&steps, &verifier_target, &awaiting, &mut status, &mut HashMap::new(), &mut feedback, &plan_log));
        assert_eq!(status["impl"], StepStatus::Skipped);
        assert!(feedback.is_empty(), "{feedback:?}");
        // (b) 目标失败 → Pending 验证者 Skipped。
        let mut status = HashMap::from([("impl".to_string(), StepStatus::Failed), ("prove".to_string(), StepStatus::Pending)]);
        assert!(resolve_stalled_pairs(&steps, &verifier_target, &[], &mut status, &mut HashMap::new(), &mut HashMap::new(), &plan_log));
        assert_eq!(status["prove"], StepStatus::Skipped);
        let text = plan_log
            .load()
            .unwrap()
            .into_iter()
            .map(|(_, event)| event.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("step_unverified"));
        assert!(text.contains("verifier unavailable"));
        assert!(text.contains("awaiting escalation answer"));
        assert!(text.contains("did not succeed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 模拟「步骤内 escalate」：第一次被调用时把 step_escalated 写进计划 journal
    /// （等价 escalate 工具的落盘副作用），其余调用只返回普通 handoff；记录每次 prompt。
    struct EscalatingProvider {
        plan_log: EventLog,
        calls: std::sync::atomic::AtomicU32,
        prompts: StdMutex<Vec<String>>,
    }

    #[async_trait]
    impl Provider for EscalatingProvider {
        fn name(&self) -> &str {
            "escalating"
        }

        async fn chat(&self, req: ChatRequest) -> rclaw_llm::Result<rclaw_core::ChatOutcome> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let prompt = req
                .messages
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            self.prompts.lock().unwrap().push(prompt);
            if n == 1 {
                append_plan_event(
                    &self.plan_log,
                    "step_escalated",
                    json!({"question_id": "plan-esc-e2e:ask:q-1", "step": "ask", "question": "部署凭据放在哪里？", "context": "deploy.sh 读取 DEPLOY_TOKEN，环境里没有"}),
                );
            }
            Ok(rclaw_core::ChatOutcome {
                text: format!("{{\"summary\":\"第 {n} 次调用的产出\"}}"),
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

    #[tokio::test]
    async fn escalating_step_is_parked_then_reruns_with_answer() {
        let (dir, layout, journal) = temp_layout("esc-e2e");
        let plan_log = EventLog::plan(&journal, "plan-esc-e2e");
        // 用固定 plan_id 走 resume 路径（EscalatingProvider 要往这条 journal 写
        // step_escalated）：journal 只需存在，不要求有 step_finished——首步就
        // escalate 的计划正是「只有 plan_started 没有 step_finished」的形态。
        plan_log
            .append(&json!({"event": "plan_started", "steps": ["ask", "use"]}))
            .expect("写入测试 journal");
        let provider = Arc::new(EscalatingProvider {
            plan_log: plan_log.clone(),
            calls: std::sync::atomic::AtomicU32::new(0),
            prompts: StdMutex::new(Vec::new()),
        });
        let registry: RegistryLink = Arc::new(OnceLock::new());
        registry.set(Arc::new(ToolRegistry::new())).ok();
        let runtime = AgentToolRuntime::new(
            provider.clone(),
            MemoryStore::new(layout.clone()),
            layout.clone(),
            journal,
            AgentOptions::default(),
            registry,
            rclaw_core::RuntimeChannelInfo::default(),
            None,
        );
        let tool = PlanRunTool::new(runtime);
        let steps = json!([
            {"id": "ask", "task": "ASK"},
            {"id": "use", "task": "USE", "depends_on": ["ask"]}
        ]);

        // 第一轮：ask 提问未得到答案 → skipped（awaiting_escalation），use 不消费挂起的 handoff。
        let first = tool
            .execute(json!({"steps": steps, "resume_plan_id": "plan-esc-e2e"}))
            .await;
        assert!(!first.is_error, "{}", first.content);
        assert!(first.content.contains("ask [skipped]"), "{}", first.content);
        assert!(first.content.contains("use [skipped]"), "{}", first.content);
        assert!(first.content.contains("ask [awaiting_escalation]"), "{}", first.content);
        assert!(first.content.contains("部署凭据放在哪里？"), "报告须带问题原文: {}", first.content);
        assert!(
            first.content.contains("\n  佐证：deploy.sh 读取 DEPLOY_TOKEN，环境里没有"),
            "报告须带步骤附的佐证: {}",
            first.content
        );
        assert!(first.content.contains("ResolveWorkflowQuestion"), "{}", first.content);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1, "依赖方不得执行");
        let events = plan_log.load().unwrap();
        assert!(
            events
                .iter()
                .any(|(_, event)| event["reason"] == "awaiting_escalation"),
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|(_, event)| event["event"] == "step_finished" && event["step"] == "ask"),
            "挂起的步骤不得记成 step_finished: {events:?}"
        );
        assert!(event_names(&plan_log).contains(&"plan_started".to_string()));

        // 编排者应答后 resume：ask 带答案重跑，use 随后消费其新产出。
        append_plan_event(
            &plan_log,
            "escalation_resolved",
            json!({"question_id": "plan-esc-e2e:ask:q-1", "answer": "用 vault 里的 deploy-key"}),
        );
        let second = tool
            .execute(json!({"steps": steps, "resume_plan_id": "plan-esc-e2e"}))
            .await;
        assert!(!second.is_error, "{}", second.content);
        assert!(second.content.contains("ask [succeeded]"), "{}", second.content);
        assert!(second.content.contains("use [succeeded]"), "{}", second.content);
        assert!(!second.content.contains("待应答"), "{}", second.content);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
        let prompts = provider.prompts.lock().unwrap();
        assert!(prompts[1].contains("Orchestrator answers"), "{}", prompts[1]);
        assert!(prompts[1].contains("用 vault 里的 deploy-key"), "{}", prompts[1]);
        assert!(prompts[2].contains("第 2 次调用的产出"), "use 应消费 ask 重跑后的摘要: {}", prompts[2]);
        drop(prompts);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 验证者侧的「步骤内 escalate」：第一次验证调用把 step_escalated 写进 journal 并返回
    /// 没有 verdict 的说明（挂起的验证者不给结论），之后的验证调用给 pass verdict；实现
    /// 步骤返回普通 handoff。记录每次 prompt。
    struct EscalatingVerifierProvider {
        plan_log: EventLog,
        calls: std::sync::atomic::AtomicU32,
        verifier_calls: std::sync::atomic::AtomicU32,
        prompts: StdMutex<Vec<String>>,
    }

    #[async_trait]
    impl Provider for EscalatingVerifierProvider {
        fn name(&self) -> &str {
            "escalating-verifier"
        }

        async fn chat(&self, req: ChatRequest) -> rclaw_llm::Result<rclaw_core::ChatOutcome> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let prompt = req
                .messages
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let text = if prompt.contains("Verification mission") {
                let k = self.verifier_calls.fetch_add(1, Ordering::SeqCst) + 1;
                if k == 1 {
                    append_plan_event(
                        &self.plan_log,
                        "step_escalated",
                        json!({"question_id": "plan-vesc-e2e:prove:q-1", "step": "prove", "question": "测试要连真实数据库吗？", "context": "CI 里没有 DATABASE_URL"}),
                    );
                    "先要编排者确认测试环境，暂不给结论。".to_string()
                } else {
                    "{\"pass\": true, \"reasons\": [\"按编排者答复用内存库跑测试，12 个用例全部通过\"]}"
                        .to_string()
                }
            } else {
                format!("{{\"summary\":\"第 {n} 次实现产出\"}}")
            };
            self.prompts.lock().unwrap().push(prompt);
            Ok(rclaw_core::ChatOutcome {
                text,
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

    /// 验证者（Prover）自己 escalate 的端到端链路：首轮验证者挂起 → 目标被标 skipped
    /// （【待验证】前缀、step_skipped 记「验证者在等应答」）、报告列「等待验证者的编排者
    /// 应答」、目标产出留在 awaiting_verdict 记录里；应答后 resume 只补跑验证者（目标产出
    /// 与简报继承进 Verifying、不重跑），验证者提示词带答案与简报，verdict 通过记第 1 轮。
    #[tokio::test]
    async fn escalating_verifier_parks_target_then_verifies_with_answer_on_resume() {
        let (dir, layout, journal) = temp_layout("vesc-e2e");
        let plan_log = EventLog::plan(&journal, "plan-vesc-e2e");
        plan_log
            .append(&json!({"event": "plan_started", "steps": ["impl", "prove"]}))
            .expect("写入测试 journal");
        let provider = Arc::new(EscalatingVerifierProvider {
            plan_log: plan_log.clone(),
            calls: std::sync::atomic::AtomicU32::new(0),
            verifier_calls: std::sync::atomic::AtomicU32::new(0),
            prompts: StdMutex::new(Vec::new()),
        });
        let registry: RegistryLink = Arc::new(OnceLock::new());
        registry.set(Arc::new(ToolRegistry::new())).ok();
        let runtime = AgentToolRuntime::new(
            provider.clone(),
            MemoryStore::new(layout.clone()),
            layout.clone(),
            journal,
            AgentOptions::default(),
            registry,
            rclaw_core::RuntimeChannelInfo::default(),
            None,
        );
        let tool = PlanRunTool::new(runtime);
        let steps = json!([
            {"id": "impl", "task": "实现登录接口", "verified_by": "prove"},
            {"id": "prove", "task": "验证登录接口"}
        ]);

        // 第一轮：impl 成功进 Verifying，prove 提问未得应答 → prove skipped（awaiting_escalation），
        // impl 随之 skipped（待验证），报告说明在等验证者的应答。
        let first = tool
            .execute(json!({"steps": steps, "resume_plan_id": "plan-vesc-e2e"}))
            .await;
        assert!(!first.is_error, "{}", first.content);
        assert!(first.content.contains("prove [skipped]"), "{}", first.content);
        assert!(first.content.contains("impl [skipped]"), "{}", first.content);
        assert!(first.content.contains("prove [awaiting_escalation]"), "{}", first.content);
        assert!(first.content.contains("测试要连真实数据库吗？"), "{}", first.content);
        assert!(
            first.content.contains("等待验证者 'prove' 的编排者应答"),
            "{}",
            first.content
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2, "impl 一次 + 验证者一次");
        let events = plan_log.load().unwrap();
        assert!(
            events.iter().any(|(_, event)| event["event"] == "step_skipped"
                && event["step"] == "impl"
                && event["reason"] == "verifier 'prove' awaiting escalation answer"),
            "{events:?}"
        );
        assert!(
            events.iter().any(|(_, event)| event["event"] == "step_finished"
                && event["step"] == "impl"
                && event["awaiting_verdict"] == true),
            "目标产出须留在 awaiting_verdict 记录里: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|(_, event)| event["event"] == "step_finished" && event["step"] == "prove"),
            "挂起的验证者不得记成 step_finished: {events:?}"
        );

        // 答案未到就 resume：目标继承进 Verifying（装载统计记 1 步继承、1 条待应答），验证者
        // 仍因等待应答被挂起 → 双双再次 skipped、零次模型调用、报告照旧列「等待应答」，
        // 不能误报「调度不变量被破坏」。
        let parked = tool
            .execute(json!({"steps": steps, "resume_plan_id": "plan-vesc-e2e"}))
            .await;
        assert!(!parked.is_error, "{}", parked.content);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2, "答案未到：目标与验证者都不该跑");
        assert!(parked.content.contains("impl [skipped]"), "{}", parked.content);
        assert!(parked.content.contains("prove [skipped]"), "{}", parked.content);
        assert!(
            parked.content.contains("等待验证者 'prove' 的编排者应答"),
            "{}",
            parked.content
        );
        assert!(!parked.content.contains("调度不变量"), "{}", parked.content);
        let events = plan_log.load().unwrap();
        let seeded = events
            .iter()
            .rfind(|(_, event)| event["event"] == "plan_seeded")
            .expect("resume 须写 plan_seeded");
        assert_eq!(seeded.1["inherited"], 1, "{:?}", seeded.1);
        assert_eq!(seeded.1["pending_escalations"], 1, "{:?}", seeded.1);
        assert!(
            !events
                .iter()
                .any(|(_, event)| event["event"] == "step_finished" && event["step"] == "prove"),
            "{events:?}"
        );

        // 应答后 resume：只补跑验证者，带答案与继承的简报，verdict 通过记第 1 轮。
        append_plan_event(
            &plan_log,
            "escalation_resolved",
            json!({"question_id": "plan-vesc-e2e:prove:q-1", "answer": "用内存 SQLite，不连真实库"}),
        );
        let second = tool
            .execute(json!({"steps": steps, "resume_plan_id": "plan-vesc-e2e"}))
            .await;
        assert!(!second.is_error, "{}", second.content);
        assert!(second.content.contains("impl [succeeded]"), "{}", second.content);
        assert!(second.content.contains("prove [succeeded]"), "{}", second.content);
        assert!(second.content.contains("impl ⟷ prove: 第 1 轮通过"), "{}", second.content);
        assert!(!second.content.contains("待应答"), "{}", second.content);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 3, "resume 只补跑验证者，目标不重跑");
        let prompts = provider.prompts.lock().unwrap();
        assert!(prompts[2].contains("Verification mission"), "{}", prompts[2]);
        assert!(
            prompts[2].contains("### Q: 测试要连真实数据库吗？\nA: 用内存 SQLite，不连真实库"),
            "{}",
            prompts[2]
        );
        assert!(prompts[2].contains("第 1 次实现产出"), "验证者须拿到继承的目标简报: {}", prompts[2]);
        drop(prompts);
        let events = plan_log.load().unwrap();
        assert!(
            events
                .iter()
                .any(|(_, event)| event["event"] == "verdict_passed" && event["round"] == 1),
            "{events:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 阻塞 provider：每次调用先等一张许可（测试方 `add_permits` 放行），再返回普通 handoff。
    struct GatedProvider {
        gate: tokio::sync::Semaphore,
        calls: std::sync::atomic::AtomicU32,
    }

    #[async_trait]
    impl Provider for GatedProvider {
        fn name(&self) -> &str {
            "gated"
        }

        async fn chat(&self, _req: ChatRequest) -> rclaw_llm::Result<rclaw_core::ChatOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let permit = self.gate.acquire().await.expect("gate 不会关闭");
            permit.forget();
            Ok(rclaw_core::ChatOutcome {
                text: "{\"summary\":\"done\"}".to_string(),
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

    /// 同一 plan_id 正在执行时，第二个 resume 被拒绝（不交错写同一条 journal、不双倍执行步骤）；
    /// 第一个结束后守卫释放，再次 resume 可以进行。
    #[tokio::test]
    async fn concurrent_resume_of_same_plan_is_rejected() {
        let (dir, layout, journal) = temp_layout("plan-guard-e2e");
        let plan_log = EventLog::plan(&journal, "plan-guard-e2e");
        plan_log
            .append(&json!({"event": "plan_started", "steps": ["a"]}))
            .expect("写入测试 journal");
        let provider = Arc::new(GatedProvider {
            gate: tokio::sync::Semaphore::new(0),
            calls: std::sync::atomic::AtomicU32::new(0),
        });
        let registry: RegistryLink = Arc::new(OnceLock::new());
        registry.set(Arc::new(ToolRegistry::new())).ok();
        let runtime = AgentToolRuntime::new(
            provider.clone(),
            MemoryStore::new(layout.clone()),
            layout.clone(),
            journal,
            AgentOptions::default(),
            registry,
            rclaw_core::RuntimeChannelInfo::default(),
            None,
        );
        let tool = Arc::new(PlanRunTool::new(runtime));
        let steps = json!([{"id": "a", "task": "A"}]);

        let first = {
            let tool = tool.clone();
            let steps = steps.clone();
            tokio::spawn(async move {
                tool.execute(json!({"steps": steps, "resume_plan_id": "plan-guard-e2e"}))
                    .await
            })
        };
        // 等第一个执行真正跑进步骤（守卫已持有、provider 卡在门上）。
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while provider.calls.load(Ordering::SeqCst) == 0 {
            assert!(tokio::time::Instant::now() < deadline, "第一个 PlanRun 未开始执行步骤");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let second = tool
            .execute(json!({"steps": steps, "resume_plan_id": "plan-guard-e2e"}))
            .await;
        assert!(second.is_error, "{}", second.content);
        assert!(second.content.contains("正在执行中"), "{}", second.content);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1, "被拒的 resume 不得执行任何步骤");
        // 另一个 plan 不受影响（放两张许可：排队在前的第一个执行先拿一张，另一个计划拿一张）。
        let other_log = EventLog::plan(&tool.runtime.journal, "plan-guard-other");
        other_log
            .append(&json!({"event": "plan_started", "steps": ["a"]}))
            .expect("写入测试 journal");
        provider.gate.add_permits(2);
        let other = tool
            .execute(json!({"steps": steps, "resume_plan_id": "plan-guard-other"}))
            .await;
        assert!(!other.is_error, "{}", other.content);

        let first = first.await.expect("第一个 PlanRun 任务不应 panic");
        assert!(!first.is_error, "{}", first.content);
        assert!(first.content.contains("a [succeeded]"), "{}", first.content);
        // 守卫已释放：同一 plan 可以再次 resume（a 已成功、直接继承，不再调用 provider）。
        let again = tool
            .execute(json!({"steps": steps, "resume_plan_id": "plan-guard-e2e"}))
            .await;
        assert!(!again.is_error, "{}", again.content);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
