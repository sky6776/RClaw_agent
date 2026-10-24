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
    cited_receipt_ids, claimed_external_actions, contains_generic_completion_claim,
    requested_external_actions, tool_receipt_matches_action, ExternalAction,
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

/// 跨 turn 账本条目：成功副作用回执 + 归档时按【完整输入】判定的动作类别集合。
///
/// 类别必须在入账时冻结：账本里的 input 是缩减后的（防内存膨胀），截断可能丢掉
/// `--dry-run` 这类安全限定词或动作本身，用缩减值重新判定会改变回执能证明什么
/// （codex r3：`git commit … --dry-run && mkdir` 截断后被误当成提交凭据）。
#[derive(Debug, Clone)]
pub(crate) struct ArchivedEffect {
    pub(crate) receipt: ToolReceipt,
    pub(crate) actions: HashSet<ExternalAction>,
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

/// 从文本粗提取「名字.后缀」形文件名 token（小写化，ASCII 范围）。
///
/// 跨 turn 对账的守卫信号：目标文本点名了文件（「写入 new.csv」「Append another
/// row to report.md」）时，本 turn 很可能被要求对该文件做新动作，历史回执不得
/// 代为作证。后缀要求纯字母且 ≤10 字符（「report.markdown」也要命中，codex r5；
/// 纯数字后缀是版本号「v2.8.12」不是文件名），尾部句点剥掉（「report.md.」，r5）。
/// URL 先整段摘除——主机名（example.com）不是文件点名，否则「总结发到 example.com
/// 的部署」会误禁历史对账（codex r6）。已知边界：纯中文文件名（无 ASCII「名字.
/// 后缀」形态）提取不到——守卫 fail-open（codex r4 #3），此时退化为类别级对账。
fn filename_tokens(text: &str) -> Vec<String> {
    fn push_if_filename(tokens: &mut Vec<String>, token: &str) {
        let trimmed = token.trim_matches('-').trim_end_matches('.');
        if let Some((name, ext)) = trimmed.rsplit_once('.') {
            let ext_len = ext.len();
            // dotfile（.env/.gitignore）：点前名字为空也算文件点名（codex r20）。
            let is_dotfile = name.is_empty() && trimmed.len() > 1;
            if (!name.is_empty() || is_dotfile)
                && (1..=10).contains(&ext_len)
                && ext.chars().all(|c| c.is_ascii_alphabetic())
            {
                tokens.push(trimmed.to_string());
            }
        }
    }
    /// 摘掉 http(s):// 起、到空白 / 引号 / 括号 / 非 ASCII 为止的 URL 跨度。
    fn strip_urls(lower: &str) -> String {
        fn url_start(rest: &str) -> Option<usize> {
            match (rest.find("http://"), rest.find("https://")) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            }
        }
        let mut out = String::with_capacity(lower.len());
        let mut rest = lower;
        while let Some(start) = url_start(rest) {
            out.push_str(&rest[..start]);
            let after = &rest[start..];
            let end = after
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(c, '(' | ')' | '<' | '>' | '"' | '\'')
                        || !c.is_ascii()
                })
                .unwrap_or(after.len());
            rest = &after[end..];
        }
        out.push_str(rest);
        out
    }
    let stripped = strip_urls(&text.to_ascii_lowercase());
    let mut tokens = Vec::new();
    let mut current = String::new();
    for c in stripped.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
            current.push(c);
        } else {
            push_if_filename(&mut tokens, &current);
            current.clear();
        }
    }
    push_if_filename(&mut tokens, &current);
    tokens
}

/// 仅存在于当前 turn 内、每次推理前重新渲染的 Harness 状态。
///
/// 它不写入 canonical session；上下文压缩与后续会话都不会把旧状态栏当成用户消息。
/// 内核私有：循环只能经 [`Run`] 的意图方法读写。
#[derive(Debug, Clone)]
struct AgentStatus {
    /// 完整目标原文（含 turn 中插队补充）。目标要求的动作判定以此为源——截断版会
    /// 漏掉靠后的动作请求（codex r2：600 字符外的「写入 new.csv」逃过守卫）。
    goal_source: String,
    /// 状态栏渲染用的截断目标。
    goal: String,
    /// 目标点名要求的外部动作（随 [`Self::extend_goal`] 重算）。
    requested_actions: HashSet<ExternalAction>,
    effect_base: PathBuf,
    iteration: usize,
    max_iterations: usize,
    total_tool_calls: u32,
    fingerprints: HashMap<String, ToolFingerprintState>,
    receipts: Vec<ToolReceipt>,
    /// 上一批 turn 末尾归档的成功副作用回执（见 [`Run::seed_effect_receipts`]）。
    seeded_receipts: Vec<ArchivedEffect>,
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
            goal_source: goal.to_string(),
            goal: truncate_status_text(goal, 600),
            requested_actions: requested_external_actions(goal),
            effect_base: effect_base.to_path_buf(),
            iteration: 0,
            max_iterations,
            total_tool_calls: 0,
            fingerprints: HashMap::new(),
            receipts: Vec::new(),
            seeded_receipts: Vec::new(),
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

    /// 并入 turn 中到达的用户补充（插队消息），目标要求的动作集合随之重算——
    /// 否则补充里点名的动作（「写入 new.csv」）逃过完成闸门的目标约束（codex r2）。
    fn extend_goal(&mut self, extras: &[String]) {
        for extra in extras {
            self.goal_source.push('\n');
            self.goal_source.push_str(extra);
        }
        self.requested_actions = requested_external_actions(&self.goal_source);
        self.goal = truncate_status_text(&self.goal_source, 600);
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
        // 方案 A：声明只认【交付文本】。reasoning 不作为正式回复出站，其中的历史
        // 复述（错误复盘轮引用「CSV 已写入」「异常记录已更新」）不构成对用户的
        // 声明——真机实测误拒正是被思考文本引爆的。质量检查（内部帧泄漏）仍看
        // reasoning：它会以 💭 过程视图流到桌面端。
        let explicitly_claimed_actions = claimed_external_actions(final_text);
        let generic_completion_claimed = contains_generic_completion_claim(final_text);
        let requested_actions = self.requested_actions.clone();
        let goal_names_file = !filename_tokens(&self.goal_source).is_empty();
        let mut claimed_actions = explicitly_claimed_actions;
        if generic_completion_claimed {
            claimed_actions.extend(requested_actions.iter().copied());
        }
        let claims_external_completion = !claimed_actions.is_empty();
        // 结构化 Evidence 试点（write/edit 先行）：回复中显式引用回执号 =
        // ReportedFact——按 id 精确绑定到真实回执，不经关键词类别猜测。引用只
        // 加宽「回执匹配」通道（见下方过滤器与目标文件守卫的豁免口），不放宽
        // requested 守卫、也不产生新的声明；长期方向是 Evidence 协议替代整个
        // 关键词匹配器（codex 设计评审 Fix6/7）。
        let cited_ids = cited_receipt_ids(final_text);
        let mut successful_effects = HashSet::new();
        let mut unverified_actions = Vec::new();
        for action in &claimed_actions {
            let matching = self
                .receipts
                .iter()
                .filter(|receipt| {
                    // 引用通道不放宽类别核对：write 回执不得为 已部署 类声明背书
                    // （codex r30）。回复中出现回执号引用时，只有被引用的回执参与
                    // 对账——伪造/张冠李戴的引用号不通宽匹配（codex r36，与归档
                    // 回退的引用契约一致）。
                    receipt.succeeded
                        && receipt.effectful
                        && tool_receipt_matches_action(receipt, *action)
                        && (cited_ids.is_empty()
                            || cited_ids.iter().any(|cited| cited == &receipt.id))
                })
                .map(|receipt| receipt.id.clone())
                .collect::<Vec<_>>();
            if matching.is_empty() {
                // 本 turn 目标点名要求的动作不得回退：历史回执不能替新活买单（上一 turn
                // 写过 report.md 不证明这一 turn「new.csv 已写入」）。
                if requested_actions.contains(action) {
                    unverified_actions.push(action.label());
                    continue;
                }
                // 目标点名了任何文件时也不回退：点名对象 ⇒ 本 turn 很可能被要求对它
                // 做新动作（写 / 改 / 追加），声明必须由本 turn 回执证明——目标措辞
                // 千变万化，requested 关键词判定有漏网，点名文件是更硬的信号
                // （codex r3/r4：「Write a report to new.csv」「Append another row
                // to report.md」都无需走历史账本）。纯讨论型目标（真机误拒现场
                // 「请将你遇到的错误信息详细发给我」）不含文件名，回退照常生效。
                if goal_names_file {
                    // 目标点名文件时账本回退整体关闭（codex r32：引用豁免可被
                    // 「参考 report.md，回执 old-id」式措辞绕过——path 出现在回复
                    // 任何位置都算命中，故引用通道不豁免本守卫；引用只在当前
                    // turn 的回执匹配中生效，见上方 filter）。
                    unverified_actions.push(action.label());
                    continue;
                }
                // 本 turn 没有对应回执时回退上一批归档：纯报告型回复（零工具调用）如实
                // 引用先前 turn 完成的动作（「报告已写入」「代码已提交」）同样成立——
                // 回执表每 turn 清空，没有这道回退它们会被误判为无凭据声明。
                // 已知边界：按归档时冻结的动作【类别】对账，不核对声明里的对象——
                // 「A.md 已写入，B.csv 已写入」在目标未点名文件时，B.csv 会借 A.md
                // 的历史写入通过对账（codex r4 #2）；自由文本里区分「提交对象」与
                // 「顺带提及」不可判定，按误拒成本选择放行。
                let archived = self
                    .seeded_receipts
                    .iter()
                    .filter(|archived| {
                        archived.receipt.succeeded
                            && archived.receipt.effectful
                            && archived.actions.contains(action)
                            // 引用通道（结构化 Evidence 试点）：回复中出现回执号
                            // 引用时只认被引用的那条——按 id 精确绑定，类别宽匹配
                            // 的 r4-#2 残余在引用场景下被消除；未引用任何回执号 →
                            // 维持类别宽匹配（既有语义，codex 设计评审 Fix6/7）。
                            && (cited_ids.is_empty()
                                || cited_ids
                                    .iter()
                                    .any(|cited| cited == &archived.receipt.id))
                    })
                    .map(|archived| format!("prev-{}", archived.receipt.id))
                    .collect::<Vec<_>>();
                if archived.is_empty() {
                    unverified_actions.push(action.label());
                } else {
                    successful_effects.extend(archived);
                }
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
                // 结构化 Evidence 试点（write/edit 先行）：模型可在最终回复中引用
                // 回执号，完成闸门据此精确核对声明（回执存在且成功 = 事实受支持）。
                format!(
                    "- {}\n- 可在最终回复中引用回执号（如 r2-1-abc）核实对应声明",
                    receipts.join("\n- ")
                )
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

    /// 注入上一批 turn 归档的成功副作用回执，供完成闸门在本 turn 无对应回执时
    /// 核对历史动作声明（跨 turn 对账）。只影响验证，不影响重复熔断等本 turn 状态。
    pub(crate) fn seed_effect_receipts(&mut self, receipts: Vec<ArchivedEffect>) {
        self.status.seeded_receipts = receipts;
    }

    /// 并入 turn 中到达的用户补充消息（插队）：目标要求的动作集合随之重算，
    /// 完成闸门对「目标点名的动作必须本 turn 回执证明」的约束覆盖补充内容。
    pub(crate) fn extend_goal(&mut self, extras: &[String]) {
        self.status.extend_goal(extras);
    }

    /// 本 turn 已记录的成功副作用回执（turn 末尾归档进跨 turn 账本的原料）。
    /// 动作类别按【完整输入】逐一判定后随条目冻结——账本入账会缩减 input，用
    /// 缩减值重新判定会改变回执能证明什么（见 [`ArchivedEffect`]）。
    pub(crate) fn effect_receipt_ledger(&self) -> Vec<ArchivedEffect> {
        self.status
            .receipts
            .iter()
            .filter(|receipt| receipt.succeeded && receipt.effectful)
            .map(|receipt| ArchivedEffect {
                actions: ExternalAction::ALL
                    .iter()
                    .copied()
                    .filter(|action| tool_receipt_matches_action(receipt, *action))
                    .collect(),
                receipt: receipt.clone(),
            })
            .collect()
    }

    /// 三层完成验证（不带媒体认领闸门；review / 压缩等非交付路径直接使用）。
    pub(crate) fn evaluate_completion(&mut self, text: &str) -> CompletionGateReport {
        self.status.evaluate_completion(text)
    }

    /// 带 reasoning 的完成验证（生产路径经 [`Self::evaluate_final_gate`]；测试直用）。
    #[cfg(test)]
    pub(crate) fn evaluate_completion_with_reasoning(
        &mut self,
        text: &str,
        reasoning: &str,
    ) -> CompletionGateReport {
        self.status
            .evaluate_reply_completion_with_reasoning(text, Some(reasoning))
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
