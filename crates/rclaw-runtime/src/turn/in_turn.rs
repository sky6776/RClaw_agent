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
//! `turn_start` 是「本轮 user 消息」在 `messages` 里的下标：`[..turn_start]` 是加载
//! 期历史（与 session 磁盘内容一致），`[turn_start..]` 是本轮追加的消息。轮内压缩只
//! 摘要化头区，避免把「正在进行的工作」卷进摘要（对齐 ZCode auto compact 强制保留
//! 最近一轮原文的设计）。头区的尾窗锚点（最新真实 user / assistant 留在尾窗）由
//! [`assemble_compressed_history`] 保证，因此本轮的原始任务消息不会被摘要化。
//! 本轮的 `skill_context` / 媒体附件消息是【不落盘】的临时上下文（调用方以
//! `ephemeral` 下标跟踪）：内存保留、磁盘重写时剔除。
//!
//! # 为什么不拿 `execution_limit` 信号量
//!
//! load 期压缩发生在 turn 持槽【前】；轮内压缩发生在持槽【后】。若再 acquire 一枚
//! 槽位，`max_concurrent_turns` 个 turn 同时压缩时会互相等待、死锁——故轮内压缩
//! 不再 acquire（压缩频率低且有防抖，放大并发峰值的风险可忽略）。

use rclaw_core::{BusMessage, Message, ToolSchema};
use rclaw_llm::LlmError;
use rclaw_store::session::SessionStore;
use tokio_util::sync::CancellationToken;

use crate::options::AgentOptions;

use super::compression::*;
use super::history::{CompressionSink, HistoryLoader};
use super::TurnKey;

/// 轮内压缩的共享环境（一次 turn 的不可变引用集合）。
pub(super) struct InTurnContext<'a> {
    pub(super) session: &'a SessionStore,
    pub(super) history: &'a HistoryLoader,
    pub(super) opts: &'a AgentOptions,
    pub(super) key: &'a TurnKey,
    pub(super) msg: &'a BusMessage,
    pub(super) cancel: &'a CancellationToken,
    pub(super) system: &'a str,
    pub(super) schemas: &'a [ToolSchema],
    pub(super) sink: &'a CompressionSink,
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
/// `BadStatus`（非 2xx，典型 400），都被归类 NonRetryable——原样重试无意义，但压缩后
/// 重试有意义。判据走错误全文（含 body）的文本特征，对齐 `stream_error_is_deterministic`
/// 的做法：中文 provider（GLM/DeepSeek）与 OpenAI 兼容网关的文案都覆盖。
pub(crate) fn error_indicates_context_exceeded(err: &LlmError) -> bool {
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
    ephemeral: &mut Vec<usize>,
) -> bool {
    if !ctx.opts.in_turn_compression_enabled || ctx.opts.compression_threshold_tokens == 0 {
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
        .compression_skip_reason(ctx.key, messages.len())
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
    if ctx.history.in_turn_blocked_by_rapid_refill(ctx.key, estimate) {
        tracing::debug!(
            "轮内 auto compact 被 rapid-refill breaker 跳过 ({}:{})",
            ctx.key.channel,
            ctx.key.chat_id
        );
        return false;
    }
    compress_head_region(ctx, messages, turn_start, ephemeral, false).await
}

/// reactive compact：LLM 报 context-exceeded 后的强制压缩。返回是否发生了压缩。
///
/// 与 auto compact 唯一的区别是忽略阈值（provider 已经用错误告诉我们超限了）；
/// 压缩失败/无窗口时返回 false，调用方按普通错误收尾。
pub(super) async fn reactive_compress(
    ctx: &InTurnContext<'_>,
    messages: &mut Vec<Message>,
    turn_start: &mut usize,
    ephemeral: &mut Vec<usize>,
) -> bool {
    tracing::warn!(
        "LLM 报告上下文超限，执行轮内 reactive compact ({}:{})",
        ctx.key.channel,
        ctx.key.chat_id
    );
    compress_head_region(ctx, messages, turn_start, ephemeral, true).await
}

/// 压缩头区（`[..turn_start]`），保留本轮尾区，成功后 rewrite session。
///
/// `force=true`（reactive）跳过「剪枝后是否仍超阈值」的复检。返回是否压缩成功。
async fn compress_head_region(
    ctx: &InTurnContext<'_>,
    messages: &mut Vec<Message>,
    turn_start: &mut usize,
    ephemeral: &mut Vec<usize>,
    force: bool,
) -> bool {
    let head_len = (*turn_start).min(messages.len());
    let keep_recent = ctx.opts.compression_keep_recent.max(8);
    if head_len <= keep_recent + 2 {
        // 头区没有可摘要窗口（对齐 load 期「history too short」分支）。
        ctx.history
            .record_ineffective_compression(ctx.key, "in-turn head too short", head_len);
        return false;
    }
    let head: Vec<Message> = messages[..head_len].to_vec();

    // 便宜预剪枝（对齐 load 期：摘要 LLM 之前先本地剪枝，可能直接降到阈值以下）。
    let has_prev_summary = find_latest_context_summary(&head, 0, head.len()).is_some();
    let protect_head =
        protected_head_end(&head, ctx.opts.compression_protect_first, has_prev_summary);
    let (head, pruned) = prune_old_tool_results(
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
    if !force
        && ctx
            .history
            .anchored_estimate(ctx.key, ctx.system, &head, ctx.schemas)
            < ctx.opts.compression_threshold_tokens
    {
        // 剪枝后已低于阈值：省掉一次摘要 LLM 调用（请求视图的剪枝由
        // microcompact 每次请求独立完成，这里不改写 messages / session）。
        return false;
    }

    let (summary, compressed_head, used_fallback) = match ctx
        .history
        .summarize(&head, ctx.msg, ctx.cancel, keep_recent, ctx.sink)
        .await
    {
        SummaryLoad::Completed {
            summary,
            compressed,
            used_fallback,
        } => (summary, compressed, used_fallback),
        SummaryLoad::Cancelled => return false,
        SummaryLoad::IneffectivePlan | SummaryLoad::Failed => {
            // 摘要失败不 rewrite（对齐 load 期：一次坏 key / 网络抖动不应把中间
            // 历史永久降级）；防抖状态由 record_ineffective 累积。
            ctx.history
                .record_ineffective_compression(ctx.key, "in-turn summarize failed", head.len());
            return false;
        }
    };
    if ctx.cancel.is_cancelled() {
        return false;
    }

    let before_bytes = history_serialized_bytes(&head);
    let after_bytes = history_serialized_bytes(&compressed_head);
    ctx.history.record_compression_outcome(
        ctx.key,
        before_bytes,
        after_bytes,
        head.len(),
        used_fallback,
    );

    // 重建内存历史：compressed_head ++ 本轮尾区（含 ephemeral 临时上下文）。
    let tail = messages.split_off(head_len);
    let tail_len = tail.len();
    *messages = compressed_head;
    let new_head_len = messages.len();
    messages.extend(tail);

    // ephemeral 下标从旧坐标换算到新坐标（全部位于旧尾区，因此必然 >= head_len）。
    for index in ephemeral.iter_mut() {
        debug_assert!(*index >= head_len, "ephemeral 下标必须落在本轮尾区");
        *index = *index - head_len + new_head_len;
    }
    let ephemeral_set: std::collections::HashSet<usize> = ephemeral.iter().copied().collect();

    // 磁盘历史 = 新头区 + 尾区剔除 ephemeral（临时上下文不落盘，维持既有行为）。
    let mut disk: Vec<Message> = Vec::with_capacity(messages.len());
    disk.extend(messages[..new_head_len].iter().cloned());
    for (offset, message) in messages[new_head_len..].iter().enumerate() {
        if !ephemeral_set.contains(&(new_head_len + offset)) {
            disk.push(message.clone());
        }
    }
    if let Err(e) = ctx.session.rewrite(&ctx.msg.channel, &ctx.msg.chat_id, &disk) {
        // 内存已切换为压缩视图，本轮继续；磁盘保留原历史（下次 load 全量重建），
        // 与「摘要失败不 rewrite」的 fallback 语义一致，只影响落盘体量不影响正确性。
        tracing::error!(
            "轮内压缩 session rewrite 失败（内存已压缩，磁盘保留原历史）: {e}"
        );
    }
    *turn_start = new_head_len;
    // rapid-refill breaker：以压缩后的估算为新地板（reactive 路径同样登记，防连压）。
    let post_estimate =
        ctx.history
            .anchored_estimate(ctx.key, ctx.system, messages, ctx.schemas);
    ctx.history.record_compression_floor(ctx.key, post_estimate);
    tracing::info!(
        "轮内上下文压缩完成 ({}:{})：头区 {} 条（其中摘要化 {} 条）-> {} 条，本轮消息 {} 条保留；摘要 {} 字节",
        ctx.key.channel,
        ctx.key.chat_id,
        head_len,
        head_len,
        new_head_len,
        tail_len,
        summary.len()
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_exceeded_detection_covers_common_phrasings() {
        let cases = [
            LlmError::StreamAborted("maximum context length exceeded".into()),
            LlmError::StreamAborted("This model's maximum context length is 131072 tokens".into()),
            LlmError::BadStatus {
                status: 400,
                body: r#"{"error":{"code":"context_length_exceeded","message":"prompt is too long"}}"#.into(),
            },
            LlmError::BadStatus {
                status: 400,
                body: "请求的tokens超过模型token上限".into(),
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
