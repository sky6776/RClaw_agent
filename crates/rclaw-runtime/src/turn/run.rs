//! agent-run 内核：循环策略的唯一所有者。
//!
//! 主 turn 循环与子 Agent 循环共用同一套策略机器——工具循环熔断、效果回执验证、
//! 完成闸门（拒绝预算）、媒体认领闸门——此前以复制粘贴的形式分居两处且文案已漂移。
//! 内核把它们收编为 [`Run`] 的意图方法；两个循环只保留结构差异（插话、会话持久化、
//! 执行状态迁移、请求错峰），不再触碰策略内部状态。
//!
//! [`RunAdapter`] 是循环差异的接缝：目前唯一的钩子是完成闸门接受后的附加检查
//! （子 Agent 的团队任务闸门）；主 turn 无附加闸门，用默认实现。
//!
//! 注意：主循环的信号量（每 turn 一次、覆盖历史加载）与子循环的推理错峰（每请求一次）
//! 粒度不对称，强行收编进内核会造出接口与实现同宽的浅模块——保留为循环结构差异，
//! 不视为内核策略。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use rclaw_core::{ContentBlock, Message, ToolCall};
use rclaw_llm::{LlmError, LlmErrorClass};

use super::effect::{
    classify_tool_error, is_blocking_mailbox_wait, short_fingerprint, tool_call_fingerprint,
    tool_call_is_effectful, tool_effect_key, truncate_status_text,
};
use super::external_action::{
    claimed_external_actions, contains_generic_completion_claim, requested_external_actions,
    tool_receipt_matches_action,
};
use super::media::{
    finalize_media_reply, is_media_control_line, media_channel_available,
    required_media_markers_for_claim, revalidated_pending_media_markers,
};

pub(crate) const REPEATED_TOOL_CALL_LIMIT: u32 = 3;
pub(crate) const MAX_COMPLETION_GATE_REJECTIONS: u32 = 2;

#[derive(Debug, Clone)]
struct StatusError {
    category: &'static str,
    message: String,
}

#[derive(Debug, Clone)]
struct ToolFingerprintState {
    tool: String,
    effectful: bool,
    count: u32,
    short_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolReceipt {
    pub(crate) id: String,
    pub(crate) tool: String,
    pub(crate) input: String,
    pub(crate) succeeded: bool,
    pub(crate) effectful: bool,
    pub(crate) error_category: Option<&'static str>,
    pub(crate) summary: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CompletionGateReport {
    pub(crate) result_ok: bool,
    pub(crate) process_ok: bool,
    pub(crate) quality_ok: bool,
    pub(crate) receipt_ids: Vec<String>,
    pub(crate) reasons: Vec<String>,
}

impl CompletionGateReport {
    pub(crate) fn accepted(&self) -> bool {
        self.result_ok && self.process_ok && self.quality_ok
    }

    pub(crate) fn reason_text(&self) -> String {
        self.reasons.join(" ")
    }
}

#[derive(Debug, Clone)]
pub(crate) enum LoopDecision {
    Continue,
    Trip {
        fingerprints: Vec<String>,
        hard_stop: bool,
    },
}

/// 仅存在于当前 turn 内、每次推理前重新渲染的 Harness 状态。
///
/// 它不写入 canonical session；上下文压缩与后续会话都不会把旧状态栏当成用户消息。
/// 内核私有：循环只能经 [`Run`] 的意图方法读写。
#[derive(Debug, Clone)]
struct AgentStatus {
    goal: String,
    effect_base: PathBuf,
    iteration: usize,
    max_iterations: usize,
    total_tool_calls: u32,
    fingerprints: HashMap<String, ToolFingerprintState>,
    receipts: Vec<ToolReceipt>,
    unresolved_effect_errors: HashMap<String, ToolReceipt>,
    last_error: Option<StatusError>,
    circuit_trips: u32,
    completion_rejections: u32,
    last_gate: Option<CompletionGateReport>,
    active_manifest_tools: Vec<String>,
}

impl AgentStatus {
    fn new_with_base(goal: &str, max_iterations: usize, effect_base: &Path) -> Self {
        AgentStatus {
            goal: truncate_status_text(goal, 600),
            effect_base: effect_base.to_path_buf(),
            iteration: 0,
            max_iterations,
            total_tool_calls: 0,
            fingerprints: HashMap::new(),
            receipts: Vec::new(),
            unresolved_effect_errors: HashMap::new(),
            last_error: None,
            circuit_trips: 0,
            completion_rejections: 0,
            last_gate: None,
            active_manifest_tools: Vec::new(),
        }
    }

    fn set_active_manifest_tools<'a>(&mut self, tools: impl IntoIterator<Item = &'a String>) {
        self.active_manifest_tools = tools.into_iter().cloned().collect();
        self.active_manifest_tools.sort();
    }

    fn begin_iteration(&mut self, iteration: usize) {
        self.iteration = iteration;
    }

    fn record_llm_error(&mut self, error: &LlmError) {
        let category = match error.class() {
            LlmErrorClass::Retryable => "retryable",
            LlmErrorClass::NonRetryable => "non_retryable",
            LlmErrorClass::Protocol => "protocol",
        };
        self.last_error = Some(StatusError {
            category,
            message: truncate_status_text(&error.to_string(), 320),
        });
    }

    fn record_tool_calls(&mut self, calls: &[ToolCall]) -> LoopDecision {
        self.total_tool_calls = self
            .total_tool_calls
            .saturating_add(calls.len().min(u32::MAX as usize) as u32);
        let mut tripped = Vec::new();
        let mut hard_stop = false;
        for call in calls {
            if is_blocking_mailbox_wait(call) {
                continue;
            }
            let key = tool_call_fingerprint(call);
            let short_id = short_fingerprint(&key);
            let state = self
                .fingerprints
                .entry(key)
                .or_insert_with(|| ToolFingerprintState {
                    tool: call.name.clone(),
                    effectful: tool_call_is_effectful(call),
                    count: 0,
                    short_id,
                });
            state.count = state.count.saturating_add(1);
            if state.count >= REPEATED_TOOL_CALL_LIMIT {
                tripped.push(format!("{}:{}", state.tool, state.short_id));
                hard_stop |= state.count > REPEATED_TOOL_CALL_LIMIT;
            }
        }
        if tripped.is_empty() {
            LoopDecision::Continue
        } else {
            self.circuit_trips = self.circuit_trips.saturating_add(1);
            self.last_error = Some(StatusError {
                category: "loop_repetition",
                message: format!(
                    "重复工具调用达到阈值 {}：{}",
                    REPEATED_TOOL_CALL_LIMIT,
                    tripped.join(", ")
                ),
            });
            LoopDecision::Trip {
                fingerprints: tripped,
                hard_stop,
            }
        }
    }

    fn record_tool_results(&mut self, outcome: &rclaw_core::ChatOutcome, message: &Message) {
        let blocks = message.content.iter().filter_map(|block| match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((content.as_str(), *is_error)),
            _ => None,
        });
        for (index, (call, (content, is_error))) in
            outcome.tool_calls.iter().zip(blocks).enumerate()
        {
            let effectful = tool_call_is_effectful(call);
            let receipt = ToolReceipt {
                id: format!(
                    "r{}-{}-{}",
                    self.iteration,
                    index + 1,
                    short_fingerprint(&tool_call_fingerprint(call))
                ),
                tool: call.name.clone(),
                input: call.input.clone(),
                succeeded: !is_error,
                effectful,
                error_category: is_error.then(|| classify_tool_error(content)),
                summary: truncate_status_text(content, 240),
            };
            if receipt.succeeded {
                if effectful {
                    let effect_key = tool_effect_key(call, &self.effect_base);
                    self.unresolved_effect_errors.remove(&effect_key);
                    // A successful state change can make an earlier identical read/check
                    // meaningful again (for example: edit report -> rerun audit extract).
                    // Preserve all effectful fingerprints so alternating writes/actions
                    // cannot evade the repetition circuit, but forget read/check calls
                    // that observed the previous state.
                    self.fingerprints.retain(|_, state| state.effectful);
                    self.circuit_trips = 0;
                    if self
                        .last_error
                        .as_ref()
                        .is_some_and(|error| error.category == "loop_repetition")
                    {
                        self.last_error = None;
                    }
                }
            } else {
                if effectful {
                    let effect_key = tool_effect_key(call, &self.effect_base);
                    self.unresolved_effect_errors
                        .insert(effect_key, receipt.clone());
                }
                self.last_error = Some(StatusError {
                    category: receipt.error_category.unwrap_or("tool_execution"),
                    message: receipt.summary.clone(),
                });
            }
            self.receipts.push(receipt);
        }
    }

    fn evaluate_completion(&mut self, text: &str) -> CompletionGateReport {
        self.evaluate_completion_impl(text, None)
    }

    fn evaluate_reply_completion_with_reasoning(
        &mut self,
        final_text: &str,
        reasoning: Option<&str>,
    ) -> CompletionGateReport {
        self.evaluate_completion_impl(final_text, reasoning)
    }

    fn reject_completion(&mut self, reason: String) -> CompletionGateReport {
        let report = CompletionGateReport {
            result_ok: false,
            process_ok: true,
            quality_ok: true,
            receipt_ids: Vec::new(),
            reasons: vec![reason],
        };
        self.completion_rejections = self.completion_rejections.saturating_add(1);
        self.last_error = Some(StatusError {
            category: "completion_verification",
            message: truncate_status_text(&report.reasons.join(" "), 420),
        });
        self.last_gate = Some(report.clone());
        report
    }

    fn evaluate_completion_impl(
        &mut self,
        final_text: &str,
        reasoning: Option<&str>,
    ) -> CompletionGateReport {
        let verification_text = match reasoning.filter(|value| !value.trim().is_empty()) {
            Some(reasoning) => format!("{final_text}\n{reasoning}"),
            None => final_text.to_string(),
        };
        let explicitly_claimed_actions = claimed_external_actions(&verification_text);
        let generic_completion_claimed = contains_generic_completion_claim(&verification_text);
        let requested_actions = if generic_completion_claimed {
            requested_external_actions(&self.goal)
        } else {
            HashSet::new()
        };
        let mut claimed_actions = explicitly_claimed_actions;
        if generic_completion_claimed {
            claimed_actions.extend(requested_actions);
        }
        let claims_external_completion = !claimed_actions.is_empty();
        let mut successful_effects = HashSet::new();
        let mut unverified_actions = Vec::new();
        for action in &claimed_actions {
            let matching = self
                .receipts
                .iter()
                .filter(|receipt| {
                    receipt.succeeded
                        && receipt.effectful
                        && tool_receipt_matches_action(receipt, *action)
                })
                .map(|receipt| receipt.id.clone())
                .collect::<Vec<_>>();
            if matching.is_empty() {
                unverified_actions.push(action.label());
            } else {
                successful_effects.extend(matching);
            }
        }
        let mut successful_effects = successful_effects.into_iter().collect::<Vec<_>>();
        successful_effects.sort();
        unverified_actions.sort_unstable();

        let mut reasons = Vec::new();
        let result_ok = !claims_external_completion || unverified_actions.is_empty();
        if !result_ok {
            reasons.push(format!(
                "结果验证失败：以下外部动作声明没有对应的成功工具回执：{}。",
                unverified_actions.join(", ")
            ));
        }
        let process_ok = !claims_external_completion || self.unresolved_effect_errors.is_empty();
        if !process_ok {
            reasons.push(format!(
                "过程验证失败：仍有未解决的副作用工具错误：{}。",
                self.unresolved_effect_errors
                    .values()
                    .map(|receipt| format!("{}:{}", receipt.tool, receipt.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let verification_lower = verification_text.to_ascii_lowercase();
        let quality_ok = !final_text.trim().is_empty()
            && !verification_lower.contains("<agent-status>")
            && !verification_lower.contains("<rclaw-runtime-context");
        if !quality_ok {
            reasons.push("质量验证失败：最终回复为空或泄露了内部状态帧。".to_string());
        }

        let report = CompletionGateReport {
            result_ok,
            process_ok,
            quality_ok,
            receipt_ids: successful_effects,
            reasons,
        };
        if !report.accepted() {
            self.completion_rejections = self.completion_rejections.saturating_add(1);
            self.last_error = Some(StatusError {
                category: "completion_verification",
                message: truncate_status_text(&report.reasons.join(" "), 420),
            });
        }
        self.last_gate = Some(report.clone());
        report
    }

    fn render(&self, environment_version: &str) -> String {
        let remaining = self.max_iterations.saturating_sub(self.iteration);
        let mut todo = vec!["完成当前用户目标，并只在验证通过后结束".to_string()];
        if !self.unresolved_effect_errors.is_empty() {
            todo.push("解决或明确报告尚未解决的工具错误".to_string());
        }
        if self.circuit_trips > 0 {
            todo.push("停止重复调用，改用不同策略或请求必要信息".to_string());
        }
        if self.completion_rejections > 0 {
            todo.push("修复上一次完成验证指出的问题".to_string());
        }

        let mut fingerprints = self
            .fingerprints
            .values()
            .filter(|state| state.count > 1)
            .map(|state| format!("{}:{}×{}", state.tool, state.short_id, state.count))
            .collect::<Vec<_>>();
        fingerprints.sort();
        let receipts = self
            .receipts
            .iter()
            .rev()
            .take(8)
            .map(|receipt| {
                format!(
                    "{} {} {}{} — {}",
                    receipt.id,
                    receipt.tool,
                    if receipt.succeeded { "ok" } else { "error" },
                    receipt
                        .error_category
                        .map(|category| format!("({category})"))
                        .unwrap_or_default(),
                    receipt.summary
                )
            })
            .collect::<Vec<_>>();
        let last_error = self
            .last_error
            .as_ref()
            .map(|error| format!("{}: {}", error.category, error.message))
            .unwrap_or_else(|| "none".to_string());
        let gate = self
            .last_gate
            .as_ref()
            .map(|gate| {
                format!(
                    "result={} process={} quality={} receipts=[{}]",
                    gate.result_ok,
                    gate.process_ok,
                    gate.quality_ok,
                    gate.receipt_ids.join(",")
                )
            })
            .unwrap_or_else(|| "not_run".to_string());

        format!(
            "goal: {}\n\
             todo:\n- {}\n\
             budget: iteration={}/{} remaining={} tool_calls={} completion_rejections={}/{}\n\
             repeated_fingerprints: {}\n\
             last_error: {}\n\
             environment_version: {}\n\
             active_manifest_tools: {}\n\
             skill_loading_rule: Manifest tools are current-turn state. Every new turn starts with none; read SKILL.md again before use. skill_manage only edits the library and never activates a Skill.\n\
             completion_gate: {}\n\
             recent_tool_receipts:\n{}",
            self.goal,
            todo.join("\n- "),
            self.iteration,
            self.max_iterations,
            remaining,
            self.total_tool_calls,
            self.completion_rejections,
            MAX_COMPLETION_GATE_REJECTIONS,
            if fingerprints.is_empty() {
                "none".to_string()
            } else {
                fingerprints.join(", ")
            },
            last_error,
            environment_version,
            if self.active_manifest_tools.is_empty() {
                "none".to_string()
            } else {
                self.active_manifest_tools.join(", ")
            },
            gate,
            if receipts.is_empty() {
                "- none".to_string()
            } else {
                receipts
                    .into_iter()
                    .map(|line| format!("- {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        )
    }
}

/// 循环差异的接缝：适配器只贡献循环结构差异，默认实现全部直通。
#[async_trait]
pub(crate) trait RunAdapter: Send + Sync {
    /// 完成闸门接受后的附加检查；返回 `Some(reason)` 表示拒绝完成。
    /// 主 turn 无附加闸门；子 Agent 用它实现团队任务闸门。
    async fn extra_completion_gates(&self) -> Option<String> {
        None
    }
}

/// 无附加闸门的默认适配器（主 turn 循环）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct NoopAdapter;

#[async_trait]
impl RunAdapter for NoopAdapter {}

/// agent-run 内核门面：循环经它消费全部共享策略，适配器差异经钩子注入。
#[derive(Debug, Clone)]
pub(crate) struct Run<A: RunAdapter = NoopAdapter> {
    status: AgentStatus,
    adapter: A,
}

impl Run<NoopAdapter> {
    /// 以给定目标、迭代上限与效果判定基准目录构造内核（默认适配器，主 turn 用）。
    pub(crate) fn new_with_base(goal: &str, max_iterations: usize, effect_base: &Path) -> Self {
        Run {
            status: AgentStatus::new_with_base(goal, max_iterations, effect_base),
            adapter: NoopAdapter,
        }
    }

    #[cfg(test)]
    pub(crate) fn new(goal: &str, max_iterations: usize) -> Self {
        Self::new_with_base(goal, max_iterations, Path::new("agent"))
    }
}

impl<A: RunAdapter> Run<A> {
    /// 携带循环适配器构造内核。
    pub(crate) fn with_adapter(
        goal: &str,
        max_iterations: usize,
        effect_base: &Path,
        adapter: A,
    ) -> Self {
        Run {
            status: AgentStatus::new_with_base(goal, max_iterations, effect_base),
            adapter,
        }
    }

    pub(crate) fn set_active_manifest_tools<'a>(
        &mut self,
        tools: impl IntoIterator<Item = &'a String>,
    ) {
        self.status.set_active_manifest_tools(tools);
    }

    pub(crate) fn begin_iteration(&mut self, iteration: usize) {
        self.status.begin_iteration(iteration);
    }

    pub(crate) fn record_llm_error(&mut self, error: &LlmError) {
        self.status.record_llm_error(error);
    }

    pub(crate) fn record_tool_calls(&mut self, calls: &[ToolCall]) -> LoopDecision {
        self.status.record_tool_calls(calls)
    }

    pub(crate) fn record_tool_results(
        &mut self,
        outcome: &rclaw_core::ChatOutcome,
        message: &Message,
    ) {
        self.status.record_tool_results(outcome, message);
    }

    /// 三层完成验证（不带媒体认领闸门；review / 压缩等非交付路径直接使用）。
    pub(crate) fn evaluate_completion(&mut self, text: &str) -> CompletionGateReport {
        self.status.evaluate_completion(text)
    }

    pub(crate) fn render(&self, environment_version: &str) -> String {
        self.status.render(environment_version)
    }

    /// 当前完成闸门拒绝次数（循环用它决定「继续改」还是「耗尽预算出局」）。
    pub(crate) fn completion_rejections(&self) -> u32 {
        self.status.completion_rejections
    }

    /// 是否存在尚未解决的副作用工具错误（过程验证的输入之一）。
    #[cfg(test)]
    pub(crate) fn has_unresolved_effect_errors(&self) -> bool {
        !self.status.unresolved_effect_errors.is_empty()
    }

    /// 完成闸门接受后的附加检查（委托适配器）。
    pub(crate) async fn extra_completion_gates(&self) -> Option<String> {
        self.adapter.extra_completion_gates().await
    }

    /// 媒体认领闸门 + 完成闸门的统一策略：两循环共用的最终候选判定。
    ///
    /// 顺序（与拆分前两循环一致）：
    /// 1. 复核 pending 媒体 marker（丢弃失效项，原地回写存活项）；
    /// 2. 从文本剥离已验证 marker 得到最终回复；
    /// 3. 若剥离后为空 → 拒绝（媒体交付验证失败）；
    /// 4. 若回复声称交付数 > 已验证数，且通道支持媒体 / 本 turn 发起过发布 / 文本含
    ///    媒体控制行 → 拒绝（不得夸大发送数量）；
    /// 5. 否则走完成闸门三层验证。
    ///
    /// 返回（最终回复，闸门报告）。
    pub(crate) fn evaluate_final_gate(
        &mut self,
        text: &str,
        reasoning: Option<&str>,
        pending_markers: &mut Vec<String>,
        channel: &str,
        info: &rclaw_core::RuntimeChannelInfo,
        media_attempted: bool,
    ) -> (String, CompletionGateReport) {
        // 1) 复核 pending 媒体 marker（丢弃失效项，原地回写存活项）。
        let current_markers = revalidated_pending_media_markers(pending_markers, channel, info);
        if current_markers.len() != pending_markers.len() {
            tracing::warn!(
                "媒体 marker 首次验证后已失效；最终出站前已丢弃 {} 个",
                pending_markers.len() - current_markers.len()
            );
        }
        *pending_markers = current_markers;
        // 2) 从文本剥离已验证 marker 得到最终回复。
        let finalized_reply = finalize_media_reply(text, pending_markers);
        // 3) 剥离后为空 → 拒绝：marker 之外没有任何真实内容可交付。
        if finalized_reply.trim().is_empty() {
            return (
                finalized_reply,
                self.status.reject_completion(
                    "媒体交付验证失败：移除未验证的媒体标记后，最终回复为空；请报告实际结果，不要手写或复用媒体标记。"
                        .to_string(),
                ),
            );
        }
        // 4) 声称交付数 > 已验证数，且媒体参与可判定（通道支持 / 本 turn 发起过发布 /
        //    文本含媒体控制行）→ 拒绝：不得夸大发送数量。
        let required_media_markers = required_media_markers_for_claim(text);
        let unverified_media_claim = required_media_markers > pending_markers.len()
            && (media_channel_available(channel, info)
                || media_attempted
                || text.lines().any(is_media_control_line));
        if unverified_media_claim {
            return (
                finalized_reply,
                self.status.reject_completion(format!(
                    "媒体交付验证失败：回复声称已交付至少 {required_media_markers} 个媒体文件，\
但发布工具只返回 {} 个当前通道可用、且指向共享媒体沙箱非空真实文件的 MEDIA_MARKER；\
不得夸大发送数量。请报告实际发布结果和本地路径。",
                    pending_markers.len()
                )),
            );
        }
        // 5) 三层完成验证（reasoning 参与验证文本）。
        let gate = match reasoning {
            Some(reasoning) if !reasoning.trim().is_empty() => self
                .status
                .evaluate_reply_completion_with_reasoning(text, Some(reasoning)),
            _ => self.status.evaluate_completion(text),
        };
        (finalized_reply, gate)
    }
}

#[cfg(test)]
mod tests;
