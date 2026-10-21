//! 轮内上下文管理（ZCode 长程能力移植 · Layer 1）。
//!
//! 移植前的 RClaw 只在轮次开始（[`HistoryLoader::load`]）做压缩：一个长 ReAct turn
//! 内部（`max_tool_iter` 默认 256 次迭代）完全没有压缩，巨大的工具输出会一路堆积，
//! 单 turn 就可能撑爆上下文窗口。ZCode 在每个模型 step 前做 microcompact、每迭代检查
//! auto compact、撞上 context-exceeded 后 reactive compact——本模块按 RClaw 的既有
//! 基础设施（prune / assemble / summarize / 防抖 / fallback）移植这三层：
//!
//! 1. **microcompact**（[`microcompact_view`]）：对每次请求的【视图】做本地剪枝，
//!    不改内存 `messages`、不落盘——与 load 期预剪枝「只改内存视图」的语义一致；
//! 2. **auto compact**（[`maybe_auto_compress`]）：每迭代顶部用 anchored 估算检查
//!    阈值，超限则压缩【上一轮历史区】，当前 turn 的消息永远保留在尾部；
//! 3. **reactive compact**（[`reactive_compress`]）：LLM 报 context-exceeded 时
//!    忽略阈值强制压缩一次，成功后调用方原地重试请求。
//!
//! # 头区 / 尾区边界
//!
//! `turn_start` 是「本轮 user 消息」在 `messages` 里的下标（`run_turn_inner` push 完
//! 本轮 user 消息后取 `len() - 1`）：`[..turn_start]` 是加载期历史（store 活动视图经
//! [`fold_compaction_view`] 归一后的形态），`[turn_start..]` 是本轮区——本轮 user 消息
//! 与之后追加的消息。轮内压缩只对头区做摘要（头区内部仍按 keep_recent / 锚点保留
//! 一段尾窗），本轮消息永远原样留在尾区，避免把「正在进行的工作」卷进摘要（对齐
//! ZCode auto compact 强制保留最近一轮原文的设计）；本轮 user 消息既不进摘要素材，
//! 也不会成为摘要的 merge 目标（merge 只落在头区尾窗首条 user 上）。本轮的
//! `skill_context` / 媒体附件消息是【不落盘】的临时上下文（id 为 0）：它们也在尾区，
//! 随尾区整段原样保留，store 里本就没有它们，不需要单独跟踪。压缩后只有 `turn_start`
//! 按新头区长度重新换算。
//!
//! 落库是非破坏性的：只追加一条 compaction boundary（锚点 = 头区尾窗首条的 id），
//! 本轮已持久化的消息 id 天然落在 `[tail_from, boundary)`，store 视图 = 压缩头区 ++
//! 本轮尾区，与内存视图一致。
//!
//! # 视图来源：头区不一定是会话开头
//!
//! load 期不压缩时返回的是活动视图按 `max_history` / `max_history_bytes` 裁掉前缀后的
//! **后缀**（[`ViewProvenance::complete`] = false）。此时头区的前几条不是会话真正的开头：
//! 若仍按 `protect_first` 保护它们，`head_until` 会锚在会话中段——库里已有 boundary 时
//! store 的锚点校验会拒绝（白付一次摘要调用、只压内存），没有 boundary 时则把被裁掉
//! 的整段前缀当成「受保护头部」复活进视图。因此不完整视图上的轮内压缩一律
//! `protect_first = 0`（只落 `tail_from`，与 ZCode auto compact 不在轮中重新保护头部一致）；
//! 被裁掉的前缀若带走了上一 boundary 的摘要，则以它为 previous_summary 做增量更新
//! （[`ViewProvenance::trimmed_summary`]），而不是让旧摘要随新 boundary 消失。被裁掉
//! 的普通消息虽不在模型视野里，却仍是会话历史——新 boundary 一落库，它们就只剩这份
//! 摘要代表，因此从库里取回、剪枝后并入摘要素材的最前面（[`load_trimmed_prefix`]）：
//! 压缩计划（尾窗 / 锚点 / boundary）只在视图头区上算，前缀不进压缩视图；transcript
//! 预算由头区优先占用，其次是前缀开头属于上一 boundary 受保护头部的几条（会话开头的
//! 任务原文，从未被摘要过、本次 boundary 落库后也不再在视图里），其余前缀放不下时从
//! 最早的让起（`merge_prefix_into_summary_material`）。
//!
//! 防抖口径：`compression_skip_reason` / `record_*` 统一传历史水位
//! （[`history_watermark`]：视图里最大的已持久化消息 id），与 load 期同一把尺子——
//! 按条数比会因「load 期是完整视图条数、轮内是裁剪后视图 + 本轮条数」而失真，裁剪过
//! 的会话里轮内压缩会被「连续低效」长期压住。跨迭代的重复触发另由两道闸收敛：
//! 成功压缩后的 rapid-refill 地板，以及摘要 LLM 失败后的时间冷却。没有 LLM 调用的
//! 「无窗口 / 头区过短」分支每迭代重算一次（开销可忽略），且【不】记为低效压缩：
//! 它们只说明头区没有可摘要窗口，不代表下一次 load 在完整视图上也没有——按低效记会
//! 在本轮没有再持久化任何消息（如最终 LLM 调用失败）时把下一次 load 期压缩无故压住。
//!
//! # 为什么不拿 `execution_limit` 信号量
//!
//! load 期压缩发生在 turn 持槽【前】；轮内压缩发生在持槽【后】。若再 acquire 一枚
//! 槽位，`max_concurrent_turns` 个 turn 同时压缩时会互相等待、死锁——故轮内压缩
//! 不再 acquire（压缩频率低且有防抖，放大并发峰值的风险可忽略）。

use std::sync::atomic::{AtomicBool, Ordering};

use rclaw_core::{BusMessage, Message, Role, ToolSchema};
use rclaw_llm::LlmError;
use tokio_util::sync::CancellationToken;

use crate::options::AgentOptions;

use super::compression::*;
use super::history::{CompressionSink, HistoryLoader};
use super::TurnKey;

/// 轮内压缩的共享环境（一次 turn 的不可变引用集合）。
pub(super) struct InTurnContext<'a> {
    pub(super) history: &'a HistoryLoader,
    pub(super) opts: &'a AgentOptions,
    pub(super) key: &'a TurnKey,
    pub(super) msg: &'a BusMessage,
    pub(super) cancel: &'a CancellationToken,
    pub(super) system: &'a str,
    pub(super) schemas: &'a [ToolSchema],
    pub(super) sink: &'a CompressionSink,
    /// 本轮内存视图的来源（完整 / 裁剪后缀），决定头区能否再保护（见模块注释）。
    pub(super) view: &'a ViewProvenance,
    /// 本轮已确认「被裁掉的前缀取不回来」（读库失败 / 本轮 load 后有并发 boundary，见
    /// [`load_trimmed_prefix`]）：一个 turn 内不会自愈，置位后 auto / reactive 压缩都直接放弃，
    /// 不再每迭代重读一遍完整活动视图。turn 级状态，由调用方按 turn 持有（原子量只为让
    /// 持有它的 turn future 保持 `Send`，没有跨线程竞争）。
    pub(super) prefix_unavailable: &'a AtomicBool,
}

/// microcompact：请求视图的本地剪枝（ZCode `microcompact.ts` 的移植）。
///
/// 对即将发给模型的请求消息做 load 期同一套 prune（剥离旧媒体、去重/摘要化旧 tool
/// result、裁剪大 tool_use input），保护窗口（条数 + token 预算）内的最近消息原样
/// 保留。只影响本次请求视图：内存 `messages` 与 session 磁盘都不改写，下一轮 load
/// 仍能取回完整内容。幂等——已剪枝的内容再次剪枝结果不变。
pub(super) fn microcompact_view(messages: &[Message], opts: &AgentOptions) -> Vec<Message> {
    let has_prev_summary = find_latest_context_summary(messages, 0, messages.len()).is_some();
    let protect_head =
        protected_head_end(messages, opts.compression_protect_first, has_prev_summary);
    let (view, pruned) = prune_old_tool_results(
        messages,
        opts.compression_prune_protect_count,
        opts.compression_tail_token_budget,
        protect_head,
    );
    if pruned > 0 {
        tracing::debug!("轮内 microcompact：请求视图剪枝 {pruned} 个旧 tool/media 输出");
    }
    view
}

/// 判断 LLM 错误是否为「上下文超限」（reactive compact 的触发判据）。
///
/// RClaw 的 context-exceeded 以两种形态到达：`StreamAborted`（流内 error 事件）或
/// `BadStatus`（非 2xx，典型 400 / 413），都被归类 NonRetryable——原样重试无意义，但压缩后
/// 重试有意义。判据走错误全文（含 body）的文本特征，对齐 `stream_error_is_deterministic`
/// 的做法：中文 provider（GLM/DeepSeek）与 OpenAI 兼容网关的文案都覆盖。
///
/// 可重试的 HTTP 状态（429 / 408 / 5xx）一律不算：TPM 限流的 429 body 也会写
/// "too many tokens" / "request too large"，那是等一等就好的配额问题，不该触发一次
/// 摘要调用 + 落一条 boundary；它们走原有的退避重试。
pub(crate) fn error_indicates_context_exceeded(err: &LlmError) -> bool {
    if matches!(err, LlmError::BadStatus { .. }) && err.is_retryable() {
        return false;
    }
    let text = err.to_string().to_ascii_lowercase();
    [
        "context length",
        "maximum context",
        "context_length_exceeded",
        "context window",
        "prompt is too long",
        "too many tokens",
        "input length exceeds",
        "request too large",
        "上下文长度",
        "超过模型token上限",
        "token上限",
        "请求的tokens超过",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// auto compact：迭代顶部的阈值检查 + 就地压缩。返回是否发生了压缩。
///
/// 估算与触发口径与 load 期一致（anchored 估算 + `compression_threshold_tokens`），
/// 防抖 / 失败冷却共用 [`HistoryLoader`] 的每会话状态。动态尾部消息不计入估算
/// （它每 attempt 重渲染，取其期望值成本高；低估只让触发稍晚，由 reactive compact
/// 兜底）。
pub(super) async fn maybe_auto_compress(
    ctx: &InTurnContext<'_>,
    messages: &mut Vec<Message>,
    turn_start: &mut usize,
) -> bool {
    if !in_turn_compression_allowed(ctx.opts) || ctx.opts.compression_threshold_tokens == 0 {
        return false;
    }
    let estimate = ctx
        .history
        .anchored_estimate(ctx.key, ctx.system, messages, ctx.schemas);
    if estimate < ctx.opts.compression_threshold_tokens {
        return false;
    }
    if ctx
        .history
        .compression_skip_reason(ctx.key, history_watermark(messages))
        .is_some()
    {
        tracing::debug!(
            "轮内 auto compact 被防抖/冷却跳过 ({}:{})",
            ctx.key.channel,
            ctx.key.chat_id
        );
        return false;
    }
    // rapid-refill breaker：估算未从最近一次压缩的地板回升 5% 之前不再压——
    // 否则「压缩后仍超阈值」的会话（低阈值配置 / 巨型尾窗）会每迭代白耗一次摘要调用。
    if ctx
        .history
        .in_turn_blocked_by_rapid_refill(ctx.key, estimate)
    {
        tracing::debug!(
            "轮内 auto compact 被 rapid-refill breaker 跳过 ({}:{})",
            ctx.key.channel,
            ctx.key.chat_id
        );
        return false;
    }
    compress_head_region(ctx, messages, turn_start, false).await
}

/// reactive compact：LLM 报 context-exceeded 后的强制压缩。返回是否发生了压缩。
///
/// 与 auto compact 唯一的区别是忽略阈值（provider 已经用错误告诉我们超限了）；
/// 压缩失败/无窗口时返回 false，调用方按普通错误收尾。
pub(super) async fn reactive_compress(
    ctx: &InTurnContext<'_>,
    messages: &mut Vec<Message>,
    turn_start: &mut usize,
) -> bool {
    if !in_turn_compression_allowed(ctx.opts) {
        return false;
    }
    if let Some(reason) = ctx
        .history
        .reactive_skip_reason(ctx.key, history_watermark(messages))
    {
        // 摘要 LLM 刚致命失败（冷却中）或连续低效：再压一次只会再撞一次同样的失败。
        // 「成功的 fallback 压缩」留下的冷却不在此列（见 reactive_skip_reason）。
        tracing::debug!(
            "轮内 reactive compact 被防抖/冷却跳过（{reason}）({}:{})",
            ctx.key.channel,
            ctx.key.chat_id
        );
        return false;
    }
    if prefix_known_unavailable(ctx) {
        // 本轮已确认压不了（见 compress_head_region 开头）：不再宣告「执行 reactive compact」
        // 然后什么都不做。
        tracing::debug!(
            "LLM 报告上下文超限，但本轮已确认被裁掉的前缀不可取回，reactive compact 跳过 ({}:{})",
            ctx.key.channel,
            ctx.key.chat_id
        );
        return false;
    }
    tracing::warn!(
        "LLM 报告上下文超限，执行轮内 reactive compact ({}:{})",
        ctx.key.channel,
        ctx.key.chat_id
    );
    compress_head_region(ctx, messages, turn_start, true).await
}

/// 本轮早先已确认取不回被裁掉的前缀（见 [`load_trimmed_prefix`]）：不落 boundary 的决定在
/// 本 turn 内不会改变，auto / reactive 都直接放弃，省掉剪枝与整库重读。
fn prefix_known_unavailable(ctx: &InTurnContext<'_>) -> bool {
    !ctx.view.complete && ctx.prefix_unavailable.load(Ordering::Relaxed)
}

/// 轮内上下文管理的总开关（microcompact / auto / reactive 共用）：会话压缩整体关闭时
/// （`session_compression_enabled = false`，load 期只做硬裁剪、不剪枝）轮内也不得剪枝
/// 请求视图或追加 boundary，否则用户关掉的功能仍在长 turn 里偷偷改写请求 / 历史。
pub(super) fn in_turn_compression_allowed(opts: &AgentOptions) -> bool {
    opts.session_compression_enabled && opts.in_turn_compression_enabled
}

/// 压缩头区（`[..turn_start]`），保留本轮尾区，成功后追加 compaction boundary。
///
/// `force=true`（reactive）跳过「剪枝后是否仍超阈值」的复检。返回是否压缩成功。
async fn compress_head_region(
    ctx: &InTurnContext<'_>,
    messages: &mut Vec<Message>,
    turn_start: &mut usize,
    force: bool,
) -> bool {
    if prefix_known_unavailable(ctx) {
        tracing::debug!(
            "轮内压缩跳过：本轮已确认被裁掉的前缀不可取回 ({}:{})",
            ctx.key.channel,
            ctx.key.chat_id
        );
        return false;
    }
    let head_len = (*turn_start).min(messages.len());
    // 边界约定（见模块注释）：尾区首条必须是本轮 user 消息。约定被破坏时宁可不压——按错误
    // 的边界落 boundary 会把本轮任务原文卷进摘要，比多撑一会儿上下文严重得多。
    if !messages
        .get(head_len)
        .is_some_and(|m| m.role == Role::User && !m.is_tool_result_carrier())
    {
        tracing::error!(
            "轮内压缩跳过：turn_start={head_len} 没有指向本轮 user 消息（messages={}）({}:{})",
            messages.len(),
            ctx.key.channel,
            ctx.key.chat_id
        );
        return false;
    }
    let watermark = history_watermark(messages);
    let keep_recent = ctx.opts.compression_keep_recent.max(8);

    // 视图是裁剪后缀时头区开头不是会话开头，不能再保护（见模块注释「视图来源」）。
    let mut policy = ctx.history.tail_policy(keep_recent);
    if !ctx.view.complete {
        policy.protect_first = 0;
    }
    if history_too_short_to_compress(head_len, &policy) {
        // 头区没有可摘要窗口（与 load 期「历史太短」同一口径，按尾窗策略推导：
        // 尾窗带 token 预算时几十条巨型消息也可能需要压，不能按 keep_recent 条数一刀切）。
        // 结构性原因，不记低效（见模块注释「防抖口径」）。
        tracing::debug!(
            "轮内压缩跳过：头区 {head_len} 条没有可摘要窗口 ({}:{})",
            ctx.key.channel,
            ctx.key.chat_id
        );
        return false;
    }
    let head: Vec<Message> = messages[..head_len].to_vec();

    // 复检要用的剪枝前估算（口径 = 即将发出的整个请求：头区 + 本轮尾区；microcompact 对
    // 尾区的剪枝每次请求独立完成，这里按原样估计偏保守）。只看头区会漏判：头区本身很小、
    // 尾区把上下文撑爆时，头区压缩确实帮不上忙，但那由「节省比例低 → 连续低效」防抖去
    // 收敛，而不是在这里把触发条件静默改成「头区超阈值」。
    let estimate_before = (!force).then(|| {
        ctx.history
            .anchored_estimate(ctx.key, ctx.system, messages, ctx.schemas)
    });

    // 便宜预剪枝（对齐 load 期：摘要 LLM 之前先本地剪枝，可能直接降到阈值以下）。
    let has_prev_summary = find_latest_context_summary(&head, 0, head.len()).is_some();
    let protect_head = protected_head_end(&head, policy.protect_first, has_prev_summary);
    let (pruned_head, pruned) = prune_old_tool_results(
        &head,
        ctx.opts.compression_prune_protect_count,
        ctx.opts.compression_tail_token_budget,
        protect_head,
    );
    if pruned > 0 {
        tracing::debug!(
            "轮内压缩预剪枝：压缩 {pruned} 个旧 tool/media 输出 ({}:{})",
            ctx.key.channel,
            ctx.key.chat_id
        );
    }
    if let Some(estimate_before) = estimate_before {
        // 尾区未动，剪枝节省只来自头区；用「剪枝前估算 − 节省」复检而不是对剪枝后的视图
        // 重算。锚点之前的剪枝不再扣（锚点请求本就是 microcompact 过的视图，那部分收益已在
        // 真实用量里；锚点落在本轮尾区时头区收益全部不计——microcompact 每次请求都已剪掉
        // 头区旧输出，预剪枝确实省不下真实体量）。见 HistoryLoader::prune_savings_start。
        let savings_start = ctx.history.prune_savings_start(ctx.key, messages);
        if HistoryLoader::estimate_after_prune(
            estimate_before,
            &messages[..head_len],
            &pruned_head,
            savings_start,
        ) < ctx.opts.compression_threshold_tokens
        {
            // 剪枝后已低于阈值：省掉一次摘要 LLM 调用（不改写 messages / session）。
            return false;
        }
    }
    let head = pruned_head;

    // 防抖口径按【视图】头区算节省比例（前缀不在请求里，不该算进「压缩收益」）。
    let before_bytes = history_serialized_bytes(&head);
    // 裁剪后缀视图：被裁掉的前缀虽不在模型视野里，却仍是会话历史——新 boundary 一落库，
    // 库里 `tail_from` 之前的所有消息（含这段前缀）都只剩这份摘要代表，不把它们喂给摘要
    // 器就等于悄悄从历史里抹掉。此处取回库里 id 小于视图首条持久化 id 的活动消息作摘要
    // 素材（只进摘要输入，不进压缩计划 / `compressed` 视图：protect_first 仍为 0，尾窗
    // 锚点不会被它拉回，视图形态不变）。读不到库就不压：落一条没见过前缀的 boundary 会
    // 永久丢掉那段历史，而放弃本次只是下一次 load 再压。视图首条已不在库的活动视图里
    // 是本 turn 内不会自愈的状态，记住后后续迭代不再重试（见函数开头）；一次读库失败
    // 则只放弃本次。
    let trimmed_prefix = if ctx.view.complete {
        TrimmedPrefix::default()
    } else {
        match load_trimmed_prefix(ctx, &head) {
            TrimmedPrefixLoad::Loaded(prefix) => prefix,
            TrimmedPrefixLoad::Superseded => {
                ctx.prefix_unavailable.store(true, Ordering::Relaxed);
                return false;
            }
            TrimmedPrefixLoad::ReadFailed => return false,
        }
    };

    // 视图里没有上一轮摘要（随被裁掉的前缀一起消失）时，用裁剪前记下的那份做增量底本。
    let fallback_previous_summary = if has_prev_summary {
        None
    } else {
        ctx.view.trimmed_summary.as_deref()
    };
    let completed = match ctx
        .history
        .summarize(
            &head,
            ctx.msg,
            ctx.cancel,
            policy,
            SummaryExtras {
                fallback_previous_summary,
                trimmed_prefix: &trimmed_prefix.messages,
                trimmed_prefix_opening: trimmed_prefix.opening_len,
            },
            ctx.sink,
        )
        .await
    {
        SummaryLoad::Completed(completed) => completed,
        SummaryLoad::Cancelled => return false,
        SummaryLoad::IneffectivePlan => {
            // 头区没有可摘要窗口（没有 LLM 调用）：结构性原因，不记低效（见模块注释
            // 「防抖口径」）。
            tracing::debug!(
                "轮内压缩跳过：头区没有可摘要窗口 ({}:{})",
                ctx.key.channel,
                ctx.key.chat_id
            );
            return false;
        }
        SummaryLoad::Failed => {
            // 摘要 LLM 鉴权 / 网络失败：与 load 期同样进入失败冷却——同一坏 key / 断网
            // 在本 turn 后续迭代与下一次 load 都不再反复撞摘要调用；历史不改写。
            ctx.history.record_summary_failure(ctx.key);
            return false;
        }
    };
    if ctx.cancel.is_cancelled() {
        return false;
    }

    let after_bytes = history_serialized_bytes(&completed.compressed);
    ctx.history.record_compression_outcome(
        ctx.key,
        before_bytes,
        after_bytes,
        watermark,
        completed.used_fallback,
    );
    let summary_len = completed.summary.len();

    // 落库 compaction boundary（本轮已持久化的尾区消息自然落在 kept 段）；
    // 失败只压内存，库中保留原历史（下次 load 再压）。
    let (compressed_head, _persist_error) =
        ctx.history.persist_compaction(ctx.key, completed, true);

    // 重建内存历史：compressed_head ++ 本轮尾区（含不落盘的临时上下文，整段原样保留）。
    let tail = messages.split_off(head_len);
    let tail_len = tail.len();
    *messages = compressed_head;
    let new_head_len = messages.len();
    messages.extend(tail);
    *turn_start = new_head_len;
    // rapid-refill breaker：以压缩后的估算为新地板（reactive 路径同样登记，防连压）。
    let post_estimate = ctx
        .history
        .anchored_estimate(ctx.key, ctx.system, messages, ctx.schemas);
    ctx.history.record_compression_floor(ctx.key, post_estimate);
    // 新头区 = [受保护头部] + 摘要 + 头区尾窗；被摘要化的条数 = 旧头区 - (新头区 - 摘要 1 条)。
    let summarized = head_len.saturating_sub(new_head_len.saturating_sub(1));
    tracing::info!(
        "轮内上下文压缩完成 ({}:{})：头区 {} 条（其中摘要化 {} 条）-> {} 条，本轮消息 {} 条保留；摘要 {} 字节",
        ctx.key.channel,
        ctx.key.chat_id,
        head_len,
        summarized,
        new_head_len,
        tail_len,
        summary_len
    );
    true
}

/// 裁剪后缀视图上的轮内压缩：取库里位于视图首条持久化消息之前的活动消息（load 期按
/// `max_history` / `max_history_bytes` 裁掉的前缀）作摘要素材。
///
/// 前缀全部按「旧消息」剪枝（媒体剥离、tool result 摘要化 / 去重）：它只是摘要素材，不进
/// 请求视图、不参与压缩计划（尾窗 / 锚点只在头区上算，见 [`HistoryLoader::summarize`]）。
/// 前缀里若带着上一 boundary 的摘要，并入素材时会被剥成 previous_summary 口径。
///
/// 两种「不能落 boundary」：
/// - [`TrimmedPrefixLoad::ReadFailed`]：读库失败，只放弃本次（下一迭代可再试）；
/// - [`TrimmedPrefixLoad::Superseded`]：视图首条在库里的活动视图中已找不到——视图与库走
///   的是同一条 `load_active` + 折叠路径，找不到只能是本轮 load 之后有别的 boundary
///   （`/compact` 残余窗口）把它折到了 `tail_from` 之前，此时继续压缩会落一个没见过前缀、
///   也没带上那条 boundary 摘要的 boundary；本 turn 内不会自愈，调用方置位
///   `prefix_unavailable`。
///
/// 视图首条本就是会话开头时没有前缀（空的 `Loaded`）。
fn load_trimmed_prefix(ctx: &InTurnContext<'_>, head: &[Message]) -> TrimmedPrefixLoad {
    let Some(first_id) = head.iter().map(|m| m.id).find(|id| *id > 0) else {
        return TrimmedPrefixLoad::Loaded(TrimmedPrefix::default());
    };
    let full = match ctx.history.load_active_view(ctx.msg) {
        Ok(full) => full,
        Err(e) => {
            tracing::warn!(
                "轮内压缩读取完整活动视图失败，本次压缩放弃（被裁掉的前缀必须进摘要）({}:{}): {e}",
                ctx.key.channel,
                ctx.key.chat_id
            );
            return TrimmedPrefixLoad::ReadFailed;
        }
    };
    let Some(split) = full.iter().position(|m| m.id == first_id) else {
        tracing::warn!(
            "轮内压缩：视图首条消息 #{first_id} 已不在库的活动视图里（本轮 load 后有并发 boundary），\
             本次压缩放弃 ({}:{})",
            ctx.key.channel,
            ctx.key.chat_id
        );
        return TrimmedPrefixLoad::Superseded;
    };
    if split == 0 {
        return TrimmedPrefixLoad::Loaded(TrimmedPrefix::default());
    }
    // 上一 boundary 的受保护头部 = 活动视图里摘要载体之前的那几条（store 按 `head_until`
    // 拼在最前面）。它们落在前缀里的部分从未被摘要过，本次 boundary（`protect_first = 0`）
    // 落库后也不再在视图里，并入素材时优先占预算（见 `merge_prefix_into_summary_material`）。
    // 没有上一 boundary（首次压缩就发生在裁剪视图上）时，会话开头 `protect_first` 条——
    // load 期首压会逐字保护进 `head_until` 的那几条任务原文——同样按受保护头部对待：
    // 它们既没机会进 `head_until`，又是前缀里最早、按「从前端让起」最先被省略的；头区
    // 稍大就把 transcript 预算占满，任务原文就既不保护也不进摘要。
    let opening_len = find_latest_context_summary(&full, 0, full.len())
        .map(|(carrier, _)| carrier)
        .unwrap_or(ctx.opts.compression_protect_first)
        .min(split);
    let (messages, pruned) = prune_old_tool_results(&full[..split], 0, 0, 0);
    tracing::debug!(
        "轮内压缩取回被裁掉的前缀 {} 条（其中上一 boundary 受保护头部 {opening_len} 条，剪枝 {pruned} 处）作摘要素材 ({}:{})",
        messages.len(),
        ctx.key.channel,
        ctx.key.chat_id
    );
    TrimmedPrefixLoad::Loaded(TrimmedPrefix {
        messages,
        opening_len,
    })
}

/// 裁剪后缀视图上被裁掉的前缀（[`load_trimmed_prefix`] 的产物）。
#[derive(Default)]
struct TrimmedPrefix {
    /// 剪枝后的前缀消息（时间序）。
    messages: Vec<Message>,
    /// `messages` 开头按受保护头部对待的条数：上一 boundary 的受保护头部；没有上一 boundary
    /// 时是会话开头 `protect_first` 条。
    opening_len: usize,
}

enum TrimmedPrefixLoad {
    Loaded(TrimmedPrefix),
    /// 读库失败：只放弃本次。
    ReadFailed,
    /// 视图首条已被本轮 load 之后的 boundary 折掉：本 turn 内不会自愈。
    Superseded,
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex as StdMutex};

    use async_trait::async_trait;
    use rclaw_core::{
        ChatOutcome, ChatRequest, ContentBlock, FinishReason, Role, RuntimeChannelInfo,
    };
    use rclaw_llm::{OnText, Provider};
    use rclaw_store::fs::Layout;
    use rclaw_store::memory::MemoryStore;
    use rclaw_store::session::SessionStore;
    use tokio::sync::Semaphore;

    /// 固定回复的摘要 provider，记录收到的请求（用于核对 previous_summary 是否带上）。
    struct FixedProvider {
        reply: &'static str,
        requests: Arc<StdMutex<Vec<ChatRequest>>>,
    }

    #[async_trait]
    impl Provider for FixedProvider {
        fn name(&self) -> &str {
            "fixed"
        }

        async fn chat(&self, req: ChatRequest) -> rclaw_llm::Result<ChatOutcome> {
            self.requests.lock().unwrap().push(req);
            Ok(ChatOutcome {
                text: self.reply.to_string(),
                reasoning: None,
                tool_calls: Vec::new(),
                stop: Some(FinishReason::Stop),
                usage: None,
            })
        }

        async fn chat_stream(
            &self,
            req: ChatRequest,
            _on_text: OnText,
        ) -> rclaw_llm::Result<ChatOutcome> {
            self.chat(req).await
        }
    }

    fn options(max_history: usize, threshold_tokens: usize) -> AgentOptions {
        AgentOptions {
            max_history,
            max_history_bytes: usize::MAX,
            session_compression_enabled: true,
            in_turn_compression_enabled: true,
            compression_threshold_tokens: threshold_tokens,
            compression_protect_first: 3,
            compression_keep_recent: 8,
            compression_tail_token_budget: 0,
            ..AgentOptions::default()
        }
    }

    fn loader(session: &SessionStore, layout: &Layout, opts: &AgentOptions) -> HistoryLoader {
        HistoryLoader::new(
            session,
            layout,
            opts,
            &RuntimeChannelInfo::default(),
            Arc::new(Semaphore::new(1)),
        )
    }

    fn sink(
        layout: &Layout,
        reply: &'static str,
    ) -> (CompressionSink, Arc<StdMutex<Vec<ChatRequest>>>) {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let sink = CompressionSink {
            memory: MemoryStore::new(layout.clone()),
            provider: Arc::new(FixedProvider {
                reply,
                requests: requests.clone(),
            }),
        };
        (sink, requests)
    }

    /// 追加 `count` 条交替 user / assistant 消息（奇数序号 user），返回它们的 id。
    fn append_turns(session: &SessionStore, start: usize, count: usize) -> Vec<i64> {
        (start..start + count)
            .map(|n| {
                let role = if n % 2 == 1 {
                    Role::User
                } else {
                    Role::Assistant
                };
                session
                    .append(
                        "cli",
                        "console",
                        &Message::plain(role, format!("消息 {n}：一些占位正文")),
                    )
                    .expect("写入测试消息")
            })
            .collect()
    }

    async fn load_view(loader: &HistoryLoader, sink: CompressionSink) -> LoadedHistory {
        let msg = BusMessage::new("cli", "console", "本轮输入");
        match loader
            .load(
                &msg,
                &CancellationToken::new(),
                "system",
                &[],
                None,
                Some(sink),
            )
            .await
            .expect("加载历史")
        {
            HistoryLoad::Messages(loaded) => loaded,
            HistoryLoad::Cancelled => panic!("不应取消"),
        }
    }

    /// 加载期不压缩时按 `max_history` 裁掉前缀：视图标记为不完整；被裁掉的前缀若带走了上一
    /// boundary 的摘要，摘要正文随视图返回；未裁剪的小会话标记为完整。
    #[tokio::test]
    async fn load_reports_view_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("agent"));
        let session = SessionStore::new(layout.clone());
        let opts = options(6, usize::MAX);
        let loader = loader(&session, &layout, &opts);

        let ids = append_turns(&session, 1, 4);
        let loaded = load_view(&loader, sink(&layout, "unused").0).await;
        assert_eq!(loaded.messages.len(), 4);
        assert!(loaded.provenance.complete, "未裁剪的视图应为完整");
        assert!(loaded.provenance.trimmed_summary.is_none());

        append_turns(&session, 5, 10);
        let loaded = load_view(&loader, sink(&layout, "unused").0).await;
        assert_eq!(loaded.messages.len(), 6, "按 max_history 裁剪");
        assert!(!loaded.provenance.complete, "裁掉前缀后视图不完整");
        assert!(
            loaded.provenance.trimmed_summary.is_none(),
            "没有 boundary 就没有旧摘要"
        );

        // 落一条 boundary（尾窗从第 9 条起）后再追加 8 条：活动视图 = 摘要 + 第 9..22 条，
        // 裁到最后 6 条时摘要被裁掉 → 摘要正文随视图返回。
        session
            .append_compaction(
                "cli",
                "console",
                &context_summary_message("旧摘要正文"),
                None,
                ids[0] + 8,
            )
            .expect("写入 boundary");
        append_turns(&session, 15, 8);
        let loaded = load_view(&loader, sink(&layout, "unused").0).await;
        assert_eq!(loaded.messages.len(), 6);
        assert!(!loaded.provenance.complete);
        assert_eq!(
            loaded.provenance.trimmed_summary.as_deref(),
            Some("旧摘要正文")
        );
        assert!(
            loaded
                .messages
                .iter()
                .all(|m| context_summary_body(m).is_none()),
            "被裁掉的摘要不在视图里"
        );
    }

    /// 回归：视图是裁剪后缀时，轮内压缩不得把后缀的前几条当受保护头部——否则 `head_until`
    /// 锚在会话中段，库里已有 boundary 时 store 拒绝落库（白付一次摘要、只压内存）。
    /// 现在不完整视图 `protect_first = 0`，boundary 只落 `tail_from`，并以被裁掉的旧摘要
    /// 为底本做增量更新。
    #[tokio::test]
    async fn in_turn_compaction_on_trimmed_view_persists_without_protected_head() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("agent"));
        let session = SessionStore::new(layout.clone());

        // 20 条历史 + 上一 boundary（受保护头部 = 前 3 条，尾窗从第 9 条起）+ 4 条新历史。
        let ids = append_turns(&session, 1, 20);
        session
            .append_compaction(
                "cli",
                "console",
                &context_summary_message("旧摘要正文"),
                Some(ids[2]),
                ids[8],
            )
            .expect("写入上一 boundary");
        append_turns(&session, 21, 4);

        // 加载期不压缩（阈值极高），按 max_history=12 裁成后缀：旧摘要与受保护头部都被裁掉。
        let load_opts = options(12, usize::MAX);
        let loaded = load_view(
            &loader(&session, &layout, &load_opts),
            sink(&layout, "unused").0,
        )
        .await;
        assert!(!loaded.provenance.complete);
        assert_eq!(
            loaded.provenance.trimmed_summary.as_deref(),
            Some("旧摘要正文")
        );
        let mut messages = loaded.messages;
        assert_eq!(messages.len(), 12);
        let turn_start_id = messages[0].id;

        // 本轮 user 消息已持久化并追加到尾区。
        let mut current = Message::plain(Role::User, "本轮输入");
        current.id = session.append("cli", "console", &current).unwrap();
        messages.push(current);
        let mut turn_start = 12;

        // 轮内压缩：阈值 1 → 必触发。
        let turn_opts = options(12, 1);
        let turn_loader = loader(&session, &layout, &turn_opts);
        let (sink, requests) = sink(&layout, "新摘要正文");
        let msg = BusMessage::new("cli", "console", "本轮输入");
        let key = TurnKey::from_msg(&msg);
        let cancel = CancellationToken::new();
        let prefix_unavailable = AtomicBool::new(false);
        let ctx = InTurnContext {
            history: &turn_loader,
            opts: &turn_opts,
            key: &key,
            msg: &msg,
            cancel: &cancel,
            system: "system",
            schemas: &[],
            sink: &sink,
            view: &loaded.provenance,
            prefix_unavailable: &prefix_unavailable,
        };
        assert!(
            maybe_auto_compress(&ctx, &mut messages, &mut turn_start).await,
            "应完成一次轮内压缩"
        );
        assert!(
            !prefix_unavailable.load(Ordering::Relaxed),
            "前缀取回成功不置位"
        );

        // 落库成功：活动视图 = [新 boundary] ++ 尾窗（从裁剪后缀的第 5 条起，keep_recent=8），
        // 没有任何受保护头部行、也没有复活被裁掉的前缀。
        let view = session.load_all("cli", "console").unwrap();
        assert_eq!(
            context_summary_body(&view[0]).as_deref(),
            Some("新摘要正文"),
            "首条应为新 boundary 的摘要：{view:#?}"
        );
        assert_eq!(
            view[1].id,
            turn_start_id + 4,
            "尾窗起点 = 裁剪后缀的第 5 条"
        );
        assert!(view.iter().skip(1).all(|m| m.id >= turn_start_id + 4));
        assert_eq!(
            view.last().unwrap().id,
            messages.last().unwrap().id,
            "本轮消息仍在尾区"
        );

        // 内存视图与库一致（库视图经 fold 归一后同形）：摘要 + 尾窗 + 本轮消息，
        // turn_start 指向本轮消息。
        assert_eq!(messages.len(), fold_compaction_view(view.clone()).len());
        assert_eq!(turn_start, messages.len() - 1);

        // 摘要请求以被裁掉的旧摘要为底本做增量更新。
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let prompt = requests[0]
            .messages
            .first()
            .and_then(|m| {
                m.content
                    .iter()
                    .filter_map(rclaw_core::ContentBlock::as_text)
                    .next()
            })
            .map(str::to_string)
            .unwrap_or_default();
        assert!(
            prompt.contains("PREVIOUS COMPACTED SUMMARY"),
            "应为增量更新模式"
        );
        assert!(prompt.contains("旧摘要正文"), "应带上被裁掉的旧摘要");
        // 被裁掉的前缀（第 1..3、9..12 条）与头区被摘要区（第 13..16 条）都是摘要素材；
        // 尾窗（第 17 条起）与本轮消息不进；前缀里的旧摘要载体不作为正文重复出现
        // （只在 PREVIOUS 块里出现一次）。
        for n in [1, 3, 10, 12, 13, 16] {
            assert!(
                prompt.contains(&format!("消息 {n}：")),
                "第 {n} 条应进摘要素材"
            );
        }
        for n in [17, 20, 24] {
            assert!(
                !prompt.contains(&format!("消息 {n}：")),
                "第 {n} 条在尾窗，不进摘要"
            );
        }
        assert!(!prompt.contains("本轮输入"), "本轮消息不进摘要");
        assert_eq!(
            prompt.matches("旧摘要正文").count(),
            1,
            "旧摘要只作为 previous_summary 出现"
        );
    }

    /// 首条 boundary 由裁剪后缀视图上的轮内压缩产生（库里没有上一 boundary）时，会话开头
    /// `protect_first` 条按受保护头部优先进摘要素材：它们没机会进 `head_until`、落库后永久
    /// 移出活动视图，若再按「从前端让起」第一个被省略，任务原文就既不保护也不进摘要。
    #[tokio::test]
    async fn first_in_turn_compaction_on_trimmed_view_keeps_session_opening_in_summary_material() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("agent"));
        let session = SessionStore::new(layout.clone());

        // 60 条大消息（每条 ~1900 字，渲染上限内）：裁成 12 条后缀后前缀 48 条远超 transcript
        // 预算，只能部分纳入。
        for n in 1..=60usize {
            let role = if n % 2 == 1 {
                Role::User
            } else {
                Role::Assistant
            };
            session
                .append(
                    "cli",
                    "console",
                    &Message::plain(role, format!("消息 {n}：{}", "占".repeat(1900))),
                )
                .expect("写入测试消息");
        }
        let load_opts = options(12, usize::MAX);
        let loaded = load_view(
            &loader(&session, &layout, &load_opts),
            sink(&layout, "unused").0,
        )
        .await;
        assert!(!loaded.provenance.complete);
        assert!(
            loaded.provenance.trimmed_summary.is_none(),
            "没有上一 boundary"
        );
        let mut messages = loaded.messages;
        assert_eq!(messages.len(), 12);
        let mut current = Message::plain(Role::User, "本轮输入");
        current.id = session.append("cli", "console", &current).unwrap();
        messages.push(current);
        let mut turn_start = 12;

        let turn_opts = options(12, 1);
        let turn_loader = loader(&session, &layout, &turn_opts);
        let (sink, requests) = sink(&layout, "新摘要正文");
        let msg = BusMessage::new("cli", "console", "本轮输入");
        let key = TurnKey::from_msg(&msg);
        let cancel = CancellationToken::new();
        let prefix_unavailable = AtomicBool::new(false);
        let ctx = InTurnContext {
            history: &turn_loader,
            opts: &turn_opts,
            key: &key,
            msg: &msg,
            cancel: &cancel,
            system: "system",
            schemas: &[],
            sink: &sink,
            view: &loaded.provenance,
            prefix_unavailable: &prefix_unavailable,
        };
        assert!(
            maybe_auto_compress(&ctx, &mut messages, &mut turn_start).await,
            "应完成一次轮内压缩"
        );

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let prompt = requests[0]
            .messages
            .first()
            .and_then(|m| {
                m.content
                    .iter()
                    .filter_map(rclaw_core::ContentBlock::as_text)
                    .next()
            })
            .map(str::to_string)
            .unwrap_or_default();
        // 会话开头 3 条（受保护头部口径）与紧挨被摘要区的前缀尾段、被摘要区（第 49..52 条）
        // 都在素材里；前缀中段因预算让位；尾窗（第 53 条起）与本轮消息不进。
        for n in [1, 2, 3, 48, 49, 52] {
            assert!(
                prompt.contains(&format!("消息 {n}：")),
                "第 {n} 条应进摘要素材"
            );
        }
        for n in [4, 5, 10, 20, 53, 60] {
            assert!(
                !prompt.contains(&format!("消息 {n}：")),
                "第 {n} 条不应进摘要素材"
            );
        }
        assert!(!prompt.contains("本轮输入"), "本轮消息不进摘要");
        assert!(prompt.contains("更早的"), "让位的前缀应有提示：{prompt}");
    }

    /// 裁剪后缀视图上取不回被裁掉的前缀（本轮 load 后有并发 boundary 把视图首条折掉）时，
    /// 本次压缩放弃并置位 `prefix_unavailable`；本 turn 后续的 auto / reactive 压缩直接
    /// 跳过，不再每迭代重读整库、也不发摘要请求。
    #[tokio::test]
    async fn trimmed_prefix_unavailable_is_sticky_for_the_rest_of_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("agent"));
        let session = SessionStore::new(layout.clone());
        let ids = append_turns(&session, 1, 24);

        // 加载期不压缩，按 max_history=12 裁成后缀（首条 = 第 13 条）。
        let load_opts = options(12, usize::MAX);
        let loaded = load_view(
            &loader(&session, &layout, &load_opts),
            sink(&layout, "unused").0,
        )
        .await;
        assert!(!loaded.provenance.complete);
        let mut messages = loaded.messages;
        assert_eq!(messages[0].id, ids[12]);
        let mut current = Message::plain(Role::User, "本轮输入");
        current.id = session.append("cli", "console", &current).unwrap();
        messages.push(current);
        let mut turn_start = 12;

        // 并发 boundary：尾窗从第 15 条起 → 视图首条（第 13 条）不再在库的活动视图里。
        session
            .append_compaction(
                "cli",
                "console",
                &context_summary_message("并发摘要"),
                None,
                ids[14],
            )
            .expect("写入并发 boundary");

        let turn_opts = options(12, 1);
        let turn_loader = loader(&session, &layout, &turn_opts);
        let (sink1, requests) = sink(&layout, "新摘要正文");
        let msg = BusMessage::new("cli", "console", "本轮输入");
        let key = TurnKey::from_msg(&msg);
        let cancel = CancellationToken::new();
        let prefix_unavailable = AtomicBool::new(false);
        let ctx = InTurnContext {
            history: &turn_loader,
            opts: &turn_opts,
            key: &key,
            msg: &msg,
            cancel: &cancel,
            system: "system",
            schemas: &[],
            sink: &sink1,
            view: &loaded.provenance,
            prefix_unavailable: &prefix_unavailable,
        };
        let before = messages.clone();
        assert!(!maybe_auto_compress(&ctx, &mut messages, &mut turn_start).await);
        assert!(
            prefix_unavailable.load(Ordering::Relaxed),
            "取不回前缀应置位"
        );
        assert!(
            requests.lock().unwrap().is_empty(),
            "没有前缀就不该发摘要请求"
        );
        assert_eq!(messages, before, "内存视图不改写");
        assert_eq!(turn_start, 12);

        // 置位后：auto / reactive 都直接跳过（即使把库恢复成可取回前缀的样子）。
        let dir2 = tempfile::tempdir().unwrap();
        let layout2 = Layout::new(dir2.path().join("agent"));
        let session2 = SessionStore::new(layout2.clone());
        append_turns(&session2, 1, 24);
        let loader2 = loader(&session2, &layout2, &turn_opts);
        let (sink2, requests2) = sink(&layout2, "新摘要正文");
        let sticky = AtomicBool::new(true);
        let ctx2 = InTurnContext {
            history: &loader2,
            opts: &turn_opts,
            key: &key,
            msg: &msg,
            cancel: &cancel,
            system: "system",
            schemas: &[],
            sink: &sink2,
            view: &loaded.provenance,
            prefix_unavailable: &sticky,
        };
        assert!(!maybe_auto_compress(&ctx2, &mut messages, &mut turn_start).await);
        assert!(!reactive_compress(&ctx2, &mut messages, &mut turn_start).await);
        assert!(requests2.lock().unwrap().is_empty(), "置位后不再尝试压缩");
        assert_eq!(messages, before);
        assert!(
            session2.load_all("cli", "console").unwrap().len() == 24,
            "没有落 boundary"
        );
    }

    /// 回归：头区只有几条、但每条都是巨型消息（尾窗按 token 预算收到 `min_floor`）时，
    /// reactive compact 必须能压并落 boundary——按 `keep_recent + 2` 条数一刀切会判成
    /// 「头区过短」，会话卡死在 context-exceeded 上只能 /clear。同时核对本轮 user 消息
    /// 原样留在尾区（不进摘要、不被摘要 merge），`turn_start` 仍指向它。
    #[tokio::test]
    async fn reactive_compaction_works_on_a_short_head_of_huge_messages() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("agent"));
        let session = SessionStore::new(layout.clone());
        // 9 条巨型消息（每条约 4000 token）：旧门槛 9 <= keep_recent(8) + 2 直接放弃。
        let ids: Vec<i64> = (1..=9)
            .map(|n| {
                let role = if n % 2 == 1 {
                    Role::User
                } else {
                    Role::Assistant
                };
                session
                    .append(
                        "cli",
                        "console",
                        &Message::plain(role, format!("消息 {n}：{}", "字".repeat(4_000))),
                    )
                    .unwrap()
            })
            .collect();
        let opts = AgentOptions {
            compression_tail_token_budget: 4_000,
            compression_tail_min_floor: 2,
            ..options(64, 1)
        };
        let load_opts = AgentOptions {
            compression_threshold_tokens: usize::MAX,
            ..opts.clone()
        };
        let loaded = load_view(
            &loader(&session, &layout, &load_opts),
            sink(&layout, "unused").0,
        )
        .await;
        assert!(loaded.provenance.complete);
        let mut messages = loaded.messages;
        assert_eq!(messages.len(), 9);
        let mut current = Message::plain(Role::User, "本轮输入");
        current.id = session.append("cli", "console", &current).unwrap();
        messages.push(current.clone());
        let mut turn_start = 9;

        let turn_loader = loader(&session, &layout, &opts);
        let (sink, requests) = sink(&layout, "新摘要正文");
        let msg = BusMessage::new("cli", "console", "本轮输入");
        let key = TurnKey::from_msg(&msg);
        let cancel = CancellationToken::new();
        let prefix_unavailable = AtomicBool::new(false);
        let ctx = InTurnContext {
            history: &turn_loader,
            opts: &opts,
            key: &key,
            msg: &msg,
            cancel: &cancel,
            system: "system",
            schemas: &[],
            sink: &sink,
            view: &loaded.provenance,
            prefix_unavailable: &prefix_unavailable,
        };
        assert!(
            reactive_compress(&ctx, &mut messages, &mut turn_start).await,
            "巨型短头区也应完成 reactive 压缩"
        );
        assert_eq!(requests.lock().unwrap().len(), 1, "恰好一次摘要调用");

        // 落库：受保护头部 3 条 + boundary + 按 min_floor 收窄的尾窗（第 8、9 条）+ 本轮消息。
        let view = session.load_all("cli", "console").unwrap();
        assert_eq!(view[0].id, ids[0]);
        assert_eq!(view[2].id, ids[2]);
        assert_eq!(
            context_summary_body(&view[3]).as_deref(),
            Some("新摘要正文"),
            "{view:#?}"
        );
        assert_eq!(view[4].id, ids[7], "尾窗从第 8 条起");
        assert_eq!(view.last().unwrap().id, current.id, "本轮消息仍在尾区");

        // 本轮 user 消息原样保留在尾区首位：不是摘要的 merge 目标，也没进摘要素材。
        assert_eq!(turn_start, messages.len() - 1);
        assert_eq!(messages[turn_start], current, "本轮 user 消息必须逐字保留");
        let prompt = requests.lock().unwrap()[0]
            .messages
            .first()
            .and_then(|m| {
                m.content
                    .iter()
                    .filter_map(rclaw_core::ContentBlock::as_text)
                    .next()
            })
            .map(str::to_string)
            .unwrap_or_default();
        assert!(!prompt.contains("本轮输入"), "本轮消息不进摘要");
        for n in [4, 7] {
            assert!(
                prompt.contains(&format!("消息 {n}：")),
                "第 {n} 条应进摘要素材"
            );
        }
        for n in [1, 3, 8, 9] {
            assert!(
                !prompt.contains(&format!("消息 {n}：")),
                "第 {n} 条在受保护头部 / 尾窗，不进摘要"
            );
        }
    }

    /// 回归：预剪枝复查不得把 usage 锚点已经反映的剪枝收益再扣一次——否则锚定估算被压到
    /// 真实值之下，自动压缩被静默抑制直到撞 context-exceeded。
    #[test]
    fn prune_recheck_only_discounts_savings_after_the_usage_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("agent"));
        let session = SessionStore::new(layout.clone());
        let key = TurnKey {
            channel: "cli".into(),
            chat_id: "console".into(),
            cron_job_id: None,
        };
        let plain = |id: i64, role: Role| {
            let mut message = Message::plain(role, format!("消息 {id}：占位正文"));
            message.id = id;
            message
        };
        // 旧 tool 链（大输出）在前，普通消息在后。
        let mut messages = vec![
            Message {
                id: 1,
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"command": "ls"}),
                }],
                tool_call_id: None,
                ts: 0,
            },
            Message {
                id: 2,
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "x".repeat(4_000),
                    is_error: false,
                }],
                tool_call_id: None,
                ts: 0,
            },
        ];
        for id in 3..=12 {
            messages.push(plain(
                id,
                if id % 2 == 1 {
                    Role::User
                } else {
                    Role::Assistant
                },
            ));
        }
        let (pruned, pruned_count) = prune_old_tool_results(&messages, 2, 0, 0);
        assert!(pruned_count >= 1, "旧 tool 输出应被摘要化");
        let saved = estimate_history_tokens(&messages) - estimate_history_tokens(&pruned);
        assert!(saved > 500, "剪枝应有可观收益：{saved}");

        // 无锚点：全部收益计入，与对剪枝后的历史直接重算一致。
        let opts = options(256, 1_000);
        let anchored = loader(&session, &layout, &opts);
        assert_eq!(anchored.prune_savings_start(&key, &messages), 0);
        let before = estimate_request_tokens(Some("system"), &messages, &[]);
        assert_eq!(
            HistoryLoader::estimate_after_prune(before, &messages, &pruned, 0),
            estimate_request_tokens(Some("system"), &pruned, &[])
        );

        // 锚点在最后一条消息上（该请求本身就是 microcompact 过的视图）：剪枝收益全部落在
        // 锚点之前，复查估算 = 剪枝前估算。
        anchored.record_usage_anchor(&key, &messages, 9_000);
        let before = anchored.anchored_estimate(&key, "system", &messages, &[]);
        assert_eq!(before, 9_000);
        let from = anchored.prune_savings_start(&key, &messages);
        assert_eq!(from, messages.len());
        assert_eq!(
            HistoryLoader::estimate_after_prune(before, &messages, &pruned, from),
            before
        );
        // 锚点在中间：只扣锚点之后的收益（这里锚点后没有 tool 输出 → 不扣）。
        anchored.record_usage_anchor(&key, &messages[..6], 5_000);
        assert_eq!(anchored.prune_savings_start(&key, &messages), 6);
        let before = anchored.anchored_estimate(&key, "system", &messages, &[]);
        assert_eq!(
            HistoryLoader::estimate_after_prune(before, &messages, &pruned, 6),
            before
        );

        // microcompact 关闭：load() 越过阈值后预剪枝拉回阈值之下时，返回并原样发出的就是
        // 剪枝后的视图，锚点同样反映剪枝后体量——规则不随开关变化，仍只计锚点之后的收益
        // （若锚点恰好是未剪枝请求，少扣只会让压缩略早触发）。
        let raw_opts = AgentOptions {
            in_turn_compression_enabled: false,
            ..options(256, 1_000)
        };
        let raw = loader(&session, &layout, &raw_opts);
        raw.record_usage_anchor(&key, &messages, 9_000);
        let from = raw.prune_savings_start(&key, &messages);
        assert_eq!(from, messages.len());
        assert_eq!(
            HistoryLoader::estimate_after_prune(9_000, &messages, &pruned, from),
            9_000,
            "锚点之前的剪枝收益已在真实用量里，不得再扣一次"
        );
        raw.record_usage_anchor(&key, &messages[..6], 5_000);
        assert_eq!(raw.prune_savings_start(&key, &messages), 6);
        assert_eq!(
            HistoryLoader::estimate_after_prune(9_000, &messages, &pruned, 0),
            9_000 - saved,
            "无锚点口径：全部收益计入"
        );
    }

    #[test]
    fn context_exceeded_detection_covers_common_phrasings() {
        let cases = [
            LlmError::StreamAborted("maximum context length exceeded".into()),
            LlmError::StreamAborted("This model's maximum context length is 131072 tokens".into()),
            LlmError::BadStatus {
                status: 400,
                body:
                    r#"{"error":{"code":"context_length_exceeded","message":"prompt is too long"}}"#
                        .into(),
            },
            LlmError::BadStatus {
                status: 400,
                body: "请求的tokens超过模型token上限".into(),
            },
            LlmError::BadStatus {
                status: 413,
                body: "request too large".into(),
            },
        ];
        for case in &cases {
            assert!(
                error_indicates_context_exceeded(case),
                "应识别为上下文超限: {case}"
            );
        }
        let negatives = [
            LlmError::StreamAborted("invalid api key".into()),
            LlmError::BadStatus {
                status: 500,
                body: "internal server error".into(),
            },
            // TPM 限流：body 也写 "too many tokens"，但 429 可重试，走退避而非压缩。
            LlmError::BadStatus {
                status: 429,
                body: r#"{"error":{"message":"Rate limit reached: too many tokens per minute, request too large"}}"#.into(),
            },
            LlmError::MissingApiKey,
        ];
        for case in &negatives {
            assert!(
                !error_indicates_context_exceeded(case),
                "不应误判为上下文超限: {case}"
            );
        }
    }
}
