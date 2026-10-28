//! agent 运行参数（循环相关）。

use std::time::Duration;

/// Agent 主循环与历史的可调参数。
#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// 是否 daemon 后台模式（C3：替代 RCLAW_DAEMON 环境变量；供 shell/skill 脚本
    /// 感知「避免打开浏览器等交互动作」）。
    pub daemon: bool,
    /// 历史保留的最大消息条数（裁剪窗口）。
    pub max_history: usize,
    /// 可同时执行的顶层 turn 数量。同一普通会话仍由 [`crate::turn::TurnManager`] 保证串行。
    ///
    /// 大规模并行的直接闸门（第十二轮提升 6→8）：唤醒合批（Layer 4）已消除完成通知的
    /// 线性放大，turn 之间的模型请求由 [`Self::subagent_request_stagger`] 错峰，提高并发
    /// 不再按 turn 数放大请求尖峰。运行时按 `Semaphore::MAX_PERMITS` 夹上限。
    pub max_concurrent_turns: usize,
    /// 单 turn 内工具调用迭代上限。
    pub max_tool_iter: usize,
    /// 历史序列化后的字节上限（超限从头裁剪）。
    pub max_history_bytes: usize,
    /// 是否启用 SSE 流式输出。
    pub stream_enabled: bool,
    /// 是否在处理前推送 "agent is working..." 状态。
    pub send_working_status: bool,
    /// 系统提示里包含最近多少天 daily notes。
    pub recent_days: u32,
    /// LLM 调用失败（连不上 / 超时 / 5xx·429 / 错误响应）时的最大重试次数（不含首次）。
    pub llm_max_retries: u32,
    /// 指数退避起始延迟（每次 ×2，直到 [`AgentOptions::llm_retry_max_delay`]）。
    pub llm_retry_base_delay: Duration,
    /// 指数退避单次等待上限。
    pub llm_retry_max_delay: Duration,
    /// 单轮模型调用重试序列的硬性总预算（自序列首次尝试起算，按轮重置）。
    ///
    /// 覆盖每次请求、退避与错峰等待：预算耗尽后不再发起新请求，在途请求被截断并按
    /// 失败收尾（子代理 → `RunTaskFailure::Failed`；主 turn → 沿 `llm_error` 路径失败），
    /// 不再出现「端点挂起 × 300s 超时 × 11 次尝试 ≈ 58 分钟无出口」的僵局。用户取消
    /// 仍优先于预算（终态是 Cancelled 而非 Failed）。必须大于 provider 的
    /// `request_timeout_secs`，否则会截断单次合法长调用——接线方按
    /// [`derive_llm_retry_budget`] 从实际超时派生，不要直接写死。
    pub llm_retry_budget: Duration,
    /// 相邻子 agent 模型请求的最小起始间隔。只错峰模型请求，不串行化工具执行。
    pub subagent_request_stagger: Duration,
    /// 前台子代理的【自动转后台】预算（ZCode 移植 · Layer 2a，`autoBackgroundMs` 同源）。
    ///
    /// 前台 Agent 调用运行超过该时长且未完成时：任务登记为后台执行快照继续跑
    /// （父取消不再波及，等价 ZCode `taskAbort.detachParent()`），父工具调用立即
    /// 返回 `{status:"backgrounded"}`，完成通知经团队邮箱 + 合批唤醒接续。
    /// `Duration::ZERO` = 关闭（保持旧行为：前台子代理随父 turn 生命周期）。
    pub subagent_auto_background: Duration,
    /// escalate 提问的实时等待窗口（秒）（ZCode 移植 · Layer 3）。
    ///
    /// PlanRun 前台运行期间编排者被工具调用阻塞、无法实时应答，默认 0 = 提交即挂起
    /// （走 journal 协议：ResolveWorkflowQuestion + resume_plan_id 在下一轮注入答案）。
    /// 未来 PlanRun 后台化后可调大，启用真·阻塞式实时应答。
    pub plan_escalation_wait_secs: u64,
    /// 模型上下文窗口大小（token）。`0` = 未声明，按 [`COMPRESSION_DEFAULT_ASSUMED_CONTEXT_LENGTH`]
    /// （128K）假设。
    ///
    /// 这是压缩预算（[`AgentOptions::compression_threshold_tokens`] /
    /// [`AgentOptions::compression_tail_token_budget`] /
    /// [`AgentOptions::compression_summary_context_ceiling`]）的派生输入——`rclaw-cli`
    /// 装配时由 `Config::selected_context_length()` 写入。
    /// 同时供 `/context` 斜杠命令显示「上下文窗口 / 已用 / 剩余 / 使用率」。运行期只读、
    /// 启动期固定（`/llmodel` 切换模型仅持久化到 rclaw.toml，下次启动才重派生，与本字段口径一致）。
    pub context_length: usize,
    /// 是否启用会话语义压缩（把旧历史摘要化，而不是只做硬裁剪）。
    pub session_compression_enabled: bool,
    /// 首次压缩时原样保留的开头消息数。
    pub compression_protect_first: usize,
    /// 压缩后原样保留的最近消息数。
    pub compression_keep_recent: usize,
    /// 触发语义压缩的 token 估算阈值（0 = 不自动触发语义压缩）。
    ///
    /// 运行期默认值由 [`derive_compression_token_budgets`]
    /// 从模型 `context_length` 派生（见 `rclaw-cli` 装配处）；结构体默认值取
    /// `derive_compression_token_budgets(128_000, 4096)` = 64000，使该触发条件默认生效。
    pub compression_threshold_tokens: usize,
    /// 压缩时「最近窗口」的 token 预算上限。
    ///
    /// 当 [`AgentOptions::compression_keep_recent`] 条最近消息的估算 token 超过此预算
    /// （大段 tool result / 图片），从窗口头部继续丢弃直到达标——但不少于
    /// [`AgentOptions::compression_tail_min_floor`] 条。普通短文本不会触发，行为与
    /// 旧「固定 keep_recent 窗口」一致。运行期默认值由 [`derive_compression_token_budgets`]
    /// 派生（默认假设 128K 窗口 → 12800）。
    pub compression_tail_token_budget: usize,
    /// token 预算尾窗的最少消息条数下限。
    pub compression_tail_min_floor: usize,
    /// 预剪枝（tool result prune）保护的最近消息条数，默认 20。
    ///
    /// 这是【预剪枝】尾部保护条数，与 [`AgentOptions::compression_tail_min_floor`]（tail cut
    /// 的最小尾窗 = 8）是不同概念：前者决定哪些最近消息的 tool result 不被摘要化，后者决定
    /// 摘要尾窗的最少条数。预剪枝默认保护最近 20 条消息。
    pub compression_prune_protect_count: usize,
    /// 压缩摘要输出 token 的【绝对上限】，默认 12_000。
    ///
    /// 实际摘要 max_tokens 由 [`crate::summary_max_tokens`] 按「被压缩内容 token × 20%」动态
    /// 计算，再夹到 `[2000, 本上限]`（且不超过上下文相关上限与 provider 自身上限），最后 ×1.3
    /// 作为发送给 LLM 的 max_tokens，避免大上下文压缩时因固定小上限丢失细节。
    pub compression_summary_max_tokens: u32,
    /// 压缩摘要的「上下文相关上限」，独立于压缩触发阈值。
    ///
    /// 用 `min(context_length * 0.05, compression_summary_max_tokens)` 直接按模型上下文窗口算，
    /// 不从触发阈值反推——否则用户调低 [`Self::compression_threshold_tokens`]、或小窗口走 85%
    /// 触发分支时，摘要 ceiling 会偏离模型窗口。由 CLI 经 [`derive_summary_token_ceiling`] 从
    /// `context_length` 派生后传入；嵌入方未派生时取假设 128K 窗口的派生值（=6400）。
    /// [`crate::summary_max_tokens`] 把它与 [`Self::compression_summary_max_tokens`] 取 min 作为
    /// 摘要 budget 的实际上限。
    pub compression_summary_context_ceiling: usize,
    /// 防抖：单次压缩节省比例低于此值视为「低效」，连续低效会暂停自动压缩。
    pub compression_min_savings_pct: f32,
    /// 摘要失败（走确定性 fallback）后，自动压缩的冷却秒数。
    pub compression_failure_cooldown_secs: u64,
    /// LLM 摘要失败时是否使用确定性 fallback（true，默认）替代丢弃中间窗口；
    /// 关闭则回退到硬裁剪（保留旧路径作为 escape hatch）。
    pub compression_fallback_enabled: bool,
    /// 是否启用【轮内】上下文管理（ZCode 长程能力移植 · Layer 1）：
    /// - microcompact：每次模型请求前对请求视图做本地 tool result 剪枝（不落盘）；
    /// - auto compact：每次迭代前用 anchored 估算检查阈值，超限就地语义压缩并向
    ///   `state.sqlite` 追加一条 compaction boundary（复用 load 期同一套防抖 / fallback）；
    /// - reactive compact：LLM 报 context-exceeded 时压缩后原地重试一次。
    ///
    /// 关闭时退回旧行为：压缩只发生在轮次开始（`HistoryLoader::load`），长 turn
    /// 内部没有压缩。默认开启；[`Self::session_compression_enabled`] 关闭时整体不生效。
    pub in_turn_compression_enabled: bool,
    /// 后台子 Agent 完成后是否发送【批量唤醒】消息（ZCode 移植 · Layer 4）。
    ///
    /// 完成通知仍先入团队邮箱（未读计数），唤醒器在防抖窗口内合并在同一窗口完成的
    /// 多个通知，向发起任务的会话投一条 Heartbeat 来源的系统消息驱动新轮次收集结果。
    /// 火化时未读为 0（已被 TaskList 消费 / 任务无团队邮箱）则降级为一条轻量核对提醒，
    /// 不驱动专门的收集轮次。默认开启；防抖窗口见 [`AgentOptions::background_wake_debounce`]。
    pub background_wake_enabled: bool,
    /// 后台完成通知的唤醒防抖窗口。窗口内的多个完成合并为一条唤醒消息，
    /// 避免大规模并行时按完成数线性放大请求数。
    pub background_wake_debounce: std::time::Duration,
    /// 工作流是否后台执行（默认 true）。false = 同步契约，但仍写 v3 投影表。
    pub workflow_background: bool,
    /// 是否注册 `WorkflowRun` 旧工具别名（默认 false，仅回滚用）。
    pub workflow_legacy_tool: bool,
    /// 同时持有执行许可的工作流 run 上限（桌面默认 4、Android 默认 2）。
    pub max_active_workflow_runs: usize,
    /// `world.run` 是否逐命令向前端确认（P5；默认 false = 仅 ToolAccess）。
    pub workflow_command_confirm: bool,
    /// 是否启用后台自动记忆 review。
    pub auto_memory_enabled: bool,
    /// 是否启用后台自动自进化 review。
    pub auto_evolve_enabled: bool,
    /// 是否用「带工具的 fork agent」做后台 review。
    ///
    /// 关闭时沿用「单次无工具 JSON 提取」的旧路径；开启时 spawn 一个受限工具集
    /// （memory / read / skill_manage）的 mini ReAct 循环，能 read-before-write、精确改写
    /// 记忆、甚至落地技能改动。CLI 缺省会由 `auto_evolve`
    /// 派生：`auto_evolve=true` 时默认开启；结构体默认值仍保留 `false` 方便嵌入方显式选择。
    pub auto_review_tooled: bool,
    /// 带工具 review 的最大工具迭代数，默认 32。
    ///
    /// 旧默认 256 明显过大：评审 fork 每迭代重放整段会话历史，Hermes 在生产
    /// 复盘（一次评审重放 1.49M input token）后把上限收敛到 16；RClaw 的受限
    /// 工具面（memory/read/skill_manage）一轮评审通常 3-6 次迭代即可完成。
    pub auto_review_max_iters: usize,
    /// 带工具 review 的聚合输入 token 预算（跨迭代累计），默认 600_000。
    ///
    /// 每次推理前估算请求体 token（chars/4，无 usage 时）并累加 provider 返回的
    /// 真实 prompt tokens；超过预算则在下次 provider 调用【前】终止评审——已完成的
    /// 工具写入保留（Hermes `auxiliary.background_review.max_input_tokens` 同源设计）。
    pub auto_review_max_input_tokens: u64,
    /// 自动记忆 review 的 nudge 间隔：按成功用户 turn 计数，默认 10。
    pub memory_review_nudge_interval: u32,
    /// 自动自进化 review 的 nudge 间隔：按 ReAct/API 迭代计数，默认 10。
    pub skill_review_nudge_interval: u32,
    /// Enable deterministic usage-based Skill curation.
    pub skill_curator_enabled: bool,
    pub skill_curator_interval_hours: u64,
    pub skill_stale_after_days: u64,
    pub skill_archive_after_days: u64,
    /// 是否启用【/goal 完成审计】（第十二轮后续项）：有活动目标的会话在每次成功
    /// 收尾后，用一次无工具的小推理审计本轮汇报是否以可核对证据证实目标达成——
    /// 达成则自动清除目标并投系统回执；未达成 / 审计失败保持目标不动（绝不误清）。
    /// 关闭后退回纯模型自评（提醒文案驱动 + 用户 /goal clear 收尾）。
    pub goal_audit_enabled: bool,
    /// 是否启用【工具结果落盘 + 引用句柄】（ZCode artifact 策略，第十二轮）。
    ///
    /// 工具结果超过 [`Self::tool_result_artifact_bytes`] 时，全文写入
    /// `<agent根>/artifacts/<会话键>/<序号>-<工具名>.txt`（写入的是脱敏后的内容），
    /// 模型只收到头尾预览（4KB）+ 文件路径，需要中段时用 `read` 分页取回——与
    /// microcompact 互补：超预算结果从进入上下文的第一刻起就是小句柄，而不是先膨胀
    /// 再等旧消息被剪枝。落盘失败静默回退原内容（不让一次成功的工具调用变失败）。
    pub tool_result_artifact_enabled: bool,
    /// 触发工具结果落盘的字节预算（默认 32KB；0 视为 1）。
    pub tool_result_artifact_bytes: usize,
    /// candidate_patch 编码子代理完成后，是否跑一次只读交叉验证
    /// （read/list/grep mini 循环核实结构化交接，结论附在 handoff 后）。
    ///
    /// 默认关闭：验证会为每个编码子代理额外产生最多 4 次小推理；追求交付质量时开启。
    pub verify_candidate_patches: bool,
}

impl Default for AgentOptions {
    fn default() -> Self {
        AgentOptions {
            daemon: false,
            max_history: 256,
            max_concurrent_turns: 8,
            max_tool_iter: 256,
            max_history_bytes: 512 * 1024,
            stream_enabled: true,
            send_working_status: true,
            recent_days: 3,
            llm_max_retries: 10,
            llm_retry_base_delay: Duration::from_secs(1),
            llm_retry_max_delay: Duration::from_secs(30),
            llm_retry_budget: LLM_RETRY_BUDGET_FLOOR,
            subagent_request_stagger: Duration::from_secs(2),
            subagent_auto_background: Duration::from_secs(600),
            plan_escalation_wait_secs: 0,
            context_length: COMPRESSION_DEFAULT_ASSUMED_CONTEXT_LENGTH,
            session_compression_enabled: true,
            compression_protect_first: 3,
            compression_keep_recent: 80,
            compression_threshold_tokens: 64_000,
            compression_tail_token_budget: 12_800,
            compression_tail_min_floor: 8,
            compression_prune_protect_count: 20,
            compression_summary_max_tokens: 12_000,
            // 假设 128K 窗口派生：min(128000*5%, 12000)=6400（与 threshold 默认 64000 同源）。
            compression_summary_context_ceiling: derive_summary_token_ceiling(
                COMPRESSION_DEFAULT_ASSUMED_CONTEXT_LENGTH,
                12_000,
            ),
            compression_min_savings_pct: 0.10,
            compression_failure_cooldown_secs: 600,
            compression_fallback_enabled: true,
            in_turn_compression_enabled: true,
            background_wake_enabled: true,
            background_wake_debounce: Duration::from_secs(3),
            workflow_background: true,
            workflow_legacy_tool: false,
            max_active_workflow_runs: 4,
            workflow_command_confirm: false,
            auto_memory_enabled: false,
            auto_evolve_enabled: false,
            auto_review_tooled: false,
            auto_review_max_iters: 32,
            auto_review_max_input_tokens: 600_000,
            memory_review_nudge_interval: 10,
            skill_review_nudge_interval: 10,
            skill_curator_enabled: true,
            skill_curator_interval_hours: 168,
            // 14/30：跟进 Hermes 生产调参（curator.py DEFAULT_STALE/ARCHIVE_AFTER_DAYS，
            // 2026-09 由 30/90 收紧——闲置技能更快让位）。
            skill_stale_after_days: 14,
            skill_archive_after_days: 30,
            goal_audit_enabled: true,
            tool_result_artifact_enabled: true,
            tool_result_artifact_bytes: 32 * 1024,
            verify_candidate_patches: false,
        }
    }
}

// ---------------------------------------------------------------------------
// LLM 重试总预算派生
// ---------------------------------------------------------------------------

/// [`AgentOptions::llm_retry_budget`] 的地板（默认 provider 超时 300s 的两倍）。
pub const LLM_RETRY_BUDGET_FLOOR: Duration = Duration::from_secs(600);

/// 按 provider 单次请求超时派生 [`AgentOptions::llm_retry_budget`]：
/// `max(LLM_RETRY_BUDGET_FLOOR, 2 × request_timeout)`。
///
/// 预算必须容得下至少一次满长的合法请求再加一次重试——否则用户把
/// `[llm] request_timeout_secs` 调高（本地模型 / 长推理模型，示例配置也建议深度投研
/// 调大）后，每次长调用都会在写死的 600s 处被预算截断：本来 12 分钟能成功的调用变成
/// 「重试总预算耗尽」。两倍超时仍是硬上界（最坏 ≈ 两次满长请求 + 退避），不回到
/// 「11 次 × 超时」的无出口。
pub fn derive_llm_retry_budget(request_timeout: Duration) -> Duration {
    LLM_RETRY_BUDGET_FLOOR.max(request_timeout.saturating_mul(2))
}

// ---------------------------------------------------------------------------
// 压缩 token 预算派生
// ---------------------------------------------------------------------------
//
// `tail_token_budget` 与 `threshold_tokens` 都从模型 `context_length` 派生，带
// `MINIMUM_CONTEXT_LENGTH` 地板与「地板≥窗口则按 85% 触发」的小窗口兜底。不做
// 联网模型元数据查询（避免 tiktoken 式首包下载在国内 DeepSeek/智谱环境下失败），故由
// 用户在 `[[llm.providers]]` 声明 `context_length`；未声明时按内置假设窗口派生，使 token
// 触发条件默认生效。

/// 地板：大窗口模型不在 50% 处过早压缩。
pub const COMPRESSION_MINIMUM_CONTEXT_LENGTH: usize = 64_000;
/// 用户未声明 `context_length` 时的假设窗口（典型 DeepSeek / 智谱 GLM / Claude ≥128K）。
pub const COMPRESSION_DEFAULT_ASSUMED_CONTEXT_LENGTH: usize = 128_000;
const COMPRESSION_THRESHOLD_PERCENT: f64 = 0.50;
const COMPRESSION_SUMMARY_TARGET_RATIO: f64 = 0.20;
const COMPRESSION_MIN_CTX_TRIGGER_RATIO: f64 = 0.85;

/// 按 `context_length` + `max_tokens` 派生 `(threshold_tokens, tail_token_budget)`。
///
/// 派生规则：
/// - `effective_window = context_length - max_tokens`（`<=0` 则回退为 `context_length`）；
/// - `threshold = max(effective * 50%, MINIMUM_CONTEXT_LENGTH)`；若该 floor ≥ effective（小
///   窗口模型），改取 `effective * 85%`，确保接近用满时仍能触发、而非永远触不到（#14690）；
/// - `tail_token_budget = threshold * 20%`。
///
/// `context_length = 0`（未声明）时按 [`COMPRESSION_DEFAULT_ASSUMED_CONTEXT_LENGTH`] 派生。
pub fn derive_compression_token_budgets(context_length: usize, max_tokens: u32) -> (usize, usize) {
    let ctx = if context_length == 0 {
        COMPRESSION_DEFAULT_ASSUMED_CONTEXT_LENGTH
    } else {
        context_length
    };
    let mt = max_tokens as usize;
    let mut effective = ctx.saturating_sub(mt);
    if effective == 0 {
        effective = ctx;
    }
    let pct = (effective as f64 * COMPRESSION_THRESHOLD_PERCENT) as usize;
    let floored = pct.max(COMPRESSION_MINIMUM_CONTEXT_LENGTH);
    let threshold = if effective > 0 && floored >= effective {
        let trig = (effective as f64 * COMPRESSION_MIN_CTX_TRIGGER_RATIO) as usize;
        trig.max(1).min(effective.saturating_sub(1))
    } else {
        floored
    };
    let tail = (threshold as f64 * COMPRESSION_SUMMARY_TARGET_RATIO) as usize;
    (threshold, tail)
}

/// 按 `context_length` 派生压缩摘要的「上下文相关上限」：
/// `max_summary_tokens = min(context_length * 0.05, absolute_ceiling)`。
///
/// **独立于压缩触发阈值**——直接按模型上下文窗口算，不从 threshold 反推。旧实现用
/// `compression_threshold_tokens / 10` 近似 `5% context`，在默认 128K/大窗口基本等价，但：
///   - 用户显式调低 `compression_threshold_tokens` 时，摘要 ceiling 被错误压低；
///   - 小窗口模型走 85% 触发分支时（threshold = 0.85·effective），`/10` 高估成 ~8.5% context。
///
/// 故改为按 `context_length` 直接派生。`context_length = 0`（未声明）时按
/// [`COMPRESSION_DEFAULT_ASSUMED_CONTEXT_LENGTH`]（128K）派生。
pub fn derive_summary_token_ceiling(context_length: usize, absolute_ceiling: usize) -> usize {
    let ctx = if context_length == 0 {
        COMPRESSION_DEFAULT_ASSUMED_CONTEXT_LENGTH
    } else {
        context_length
    };
    // 0.05 = 1/20（整数除法足够精确：128000/20=6400，1000000/20=50000）。
    let five_pct = ctx / 20;
    five_pct.min(absolute_ceiling)
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn main_loop_caps_at_256_iters_review_at_32_with_input_budget() {
        let opts = AgentOptions::default();
        assert_eq!(opts.max_tool_iter, 256);
        // 评审 fork 迭代上限收敛为 32（旧 256 过大）；输入预算默认 600k。
        assert_eq!(opts.auto_review_max_iters, 32);
        assert_eq!(opts.auto_review_max_input_tokens, 600_000);
    }

    #[test]
    fn subagent_requests_default_to_two_second_stagger() {
        assert_eq!(
            AgentOptions::default().subagent_request_stagger,
            Duration::from_secs(2)
        );
    }

    #[test]
    fn retry_budget_tracks_request_timeout_above_floor() {
        // 默认超时 300s：预算维持地板 600s。
        assert_eq!(
            derive_llm_retry_budget(Duration::from_secs(300)),
            Duration::from_secs(600)
        );
        assert_eq!(
            AgentOptions::default().llm_retry_budget,
            LLM_RETRY_BUDGET_FLOOR
        );
        // 用户调高超时（900s 长推理模型）：预算随之放大，单次合法长调用不被截断。
        let timeout = Duration::from_secs(900);
        let budget = derive_llm_retry_budget(timeout);
        assert_eq!(budget, Duration::from_secs(1800));
        assert!(budget > timeout);
    }

    #[test]
    fn default_assumption_yields_effective_threshold() {
        // 未声明 context_length + 默认 max_tokens=4096：默认假设 128K 窗口。
        let (thr, tail) = derive_compression_token_budgets(0, 4096);
        assert_eq!(thr, 64_000);
        assert_eq!(tail, 12_800);
        // threshold 必须 > 0（P2：默认生效，不再是禁用的 0）。
        assert!(thr > 0);
    }

    #[test]
    fn large_window_scales_threshold_and_tail() {
        // 1M 窗口（如 glm-5.3/codex 长上下文）：effective = 1_000_000 - 4096 = 995_904，
        // 50% = 497_952，floor(64K) 不顶 → threshold=497_952，tail=99_590 —— 预算随窗口伸缩。
        let (thr, tail) = derive_compression_token_budgets(1_000_000, 4096);
        assert_eq!(thr, 497_952);
        assert_eq!(tail, (497_952.0_f64 * 0.20) as usize);
    }

    #[test]
    fn small_window_uses_85pct_trigger_to_stay_reachable() {
        // 64K 窗口：effective=59904，50%=29952，floor=64000 ≥ effective → 走 85% 分支。
        let (thr, tail) = derive_compression_token_budgets(64_000, 4096);
        assert_eq!(thr, (59_904.0_f64 * 0.85) as usize);
        assert!(
            thr < 59_904,
            "小窗口 threshold 必须低于 effective 才能触得到"
        );
        assert_eq!(tail, (thr as f64 * 0.20) as usize);
    }

    #[test]
    fn max_tokens_larger_than_window_falls_back_to_full_window() {
        // max_tokens ≥ context_length（如把 128K 全分给输出）：effective 回退为 context_length。
        let (thr, _tail) = derive_compression_token_budgets(128_000, 131_072);
        assert_eq!(thr, 64_000, "effective 回退为 128K 后 threshold=64K");
    }

    #[test]
    fn summary_ceiling_derives_directly_from_context_length() {
        // max_summary_tokens = min(context_length * 0.05, ceiling)，独立于触发阈值。
        // 默认 128K → 5% = 6400。
        assert_eq!(derive_summary_token_ceiling(128_000, 12_000), 6400);
        // 1M 窗口 → 50000，被绝对上限 12000 夹住。
        assert_eq!(derive_summary_token_ceiling(1_000_000, 12_000), 12_000);
        // 小窗口 32K → 1600（ceiling 本身不 floor；budget 再由 summary_max_tokens floor 到 2000）。
        assert_eq!(derive_summary_token_ceiling(32_000, 12_000), 1600);
        // context_length=0（未声明）→ 按假设 128K 派生 = 6400。
        assert_eq!(derive_summary_token_ceiling(0, 12_000), 6400);
    }
}
