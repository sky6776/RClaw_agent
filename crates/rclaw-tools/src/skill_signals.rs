//! Skill 自进化信号日志：把运行期过程事件（熔断 / 完成门禁拒绝 / 团队任务闸门 /
//! 用户纠正等）以 append-only JSONL 落盘，供后台 review fork 优先消费。
//!
//! 设计取舍（对齐 JiuwenSwarm 的信号驱动进化 + RClaw 的确定性哲学）：
//! - 记录零 LLM 成本：信号全部来自既有门禁（工具循环熔断、三层完成验证、团队任务
//!   闸门）与保守的纯文本规则（用户纠正语气），不新增任何模型调用；
//! - 信号 ≠ 结论：消费方（review fork）仍需自行 read 核实后才允许改技能；
//! - 归因到顶层技能名（`skills/<name>/...` 的 `<name>`）；无法归因时 `skills` 为空
//!   （全局信号，仍对「流程类」经验有价值）；
//! - 有限窗口：文件超过上限时环形裁剪最旧一半，防止无界增长。

use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde::{Deserialize, Serialize};

const SIGNALS_FILE: &str = ".signals.jsonl";
/// 环形上限：条数超过该值时裁掉最旧的一半（与 SkillLedger 的窗口思路一致）。
const SIGNALS_KEEP_MAX: usize = 300;
/// 单条 detail 的字符上限（写盘前截断）。
const DETAIL_MAX_CHARS: usize = 400;

static SIGNALS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn signals_lock() -> &'static Mutex<()> {
    SIGNALS_LOCK.get_or_init(|| Mutex::new(()))
}

fn lock_signals() -> MutexGuard<'static, ()> {
    signals_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 一条自进化信号。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSignal {
    pub ts: String,
    /// 信号类型：circuit_breaker / completion_rejected / team_gate_rejected /
    /// iter_limit / user_correction / failed。
    pub kind: String,
    /// 归因到的顶层技能名；空 = 无法归因的全局信号。
    pub skills: Vec<String>,
    /// 触发上下文（已截断 + 脱敏）。
    pub detail: String,
    /// 来源（channel:chat 或子代理名）。
    pub source: String,
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 追加一条信号。best-effort：任何 IO / 序列化失败只打 warn 日志，绝不影响前台轮。
pub fn record_signal(skills_dir: &Path, kind: &str, skills: &[String], detail: &str, source: &str) {
    let signal = SkillSignal {
        ts: chrono::Utc::now().to_rfc3339(),
        kind: kind.to_string(),
        skills: skills.to_vec(),
        detail: truncate_chars(
            &rclaw_store::redact_sensitive_text(detail),
            DETAIL_MAX_CHARS,
        ),
        source: truncate_chars(source, 80),
    };
    let _guard = lock_signals();
    let path = skills_dir.join(SIGNALS_FILE);
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .map(|text| {
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    match serde_json::to_string(&signal) {
        Ok(encoded) => lines.push(encoded),
        Err(error) => {
            tracing::warn!(%error, "Skill 信号序列化失败，已丢弃该条");
            return;
        }
    }
    if lines.len() > SIGNALS_KEEP_MAX {
        let keep = SIGNALS_KEEP_MAX / 2;
        lines.drain(..lines.len() - keep);
    }
    let body = lines.join("\n") + "\n";
    if let Err(error) = rclaw_store::fs::atomic_write(&path, body.as_bytes()) {
        tracing::warn!(%error, "Skill 信号日志写入失败");
    }
}

/// 读取最近 `limit` 条信号（旧的在前）。
pub fn recent_signals(skills_dir: &Path, limit: usize) -> Vec<SkillSignal> {
    let _guard = lock_signals();
    let text = match std::fs::read_to_string(skills_dir.join(SIGNALS_FILE)) {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    let parsed: Vec<SkillSignal> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    if parsed.len() <= limit {
        parsed
    } else {
        parsed[parsed.len() - limit..].to_vec()
    }
}

/// 渲染供 review fork 消费的信号摘要：按技能聚合，列出各类型计数与最近一条细节。
///
/// 返回 `None` 表示无信号（调用方跳过该段，不往评审 prompt 里塞空段）。
pub fn signal_digest_for_review(skills_dir: &Path, max_entries: usize) -> Option<String> {
    let signals = recent_signals(skills_dir, max_entries);
    if signals.is_empty() {
        return None;
    }
    // (skill, kind) -> (count, latest detail)
    let mut agg: std::collections::BTreeMap<(String, String), (usize, String)> =
        std::collections::BTreeMap::new();
    for signal in &signals {
        let skills = if signal.skills.is_empty() {
            vec!["(global)".to_string()]
        } else {
            signal.skills.clone()
        };
        for skill in skills {
            let entry = agg
                .entry((skill, signal.kind.clone()))
                .or_insert((0, String::new()));
            entry.0 += 1;
            entry.1 = signal.detail.clone();
        }
    }
    let mut out = String::from(
        "[Skill evolution signals] 最近窗口内的过程信号（信号≠结论，改技能前仍需 read 核实）：\n",
    );
    for ((skill, kind), (count, latest)) in agg {
        out.push_str(&format!(
            "- {skill}: {kind} ×{count}（最近：{}）\n",
            truncate_chars(&latest, 120)
        ));
    }
    Some(out)
}

/// 从 turn / 子代理的最终失败文案推断信号类型。
///
/// 文案来自 `turn.rs` / `agent_tools.rs` 的固定错误前缀，匹配是确定性的；
/// 未识别的失败统一记 `failed`。顺序有讲究：主循环迭代上限文案是
/// 「达到 ReAct 迭代上限…任务未通过完成验证」，同时含两个关键词，「迭代上限」
/// 必须先于「完成验证」判，否则 iter_limit 永远打不出来。
pub fn classify_failure_signal(error: &str) -> &'static str {
    if error.contains("熔断") {
        "circuit_breaker"
    } else if error.contains("团队任务") {
        "team_gate_rejected"
    } else if error.contains("迭代上限") {
        "iter_limit"
    } else if error.contains("完成验证") {
        "completion_rejected"
    } else {
        "failed"
    }
}

/// 用户纠正语气启发式（中英，保守列表）。
///
/// 只收录明确指向「你错了 / 别这样 / 太啰嗦」的短语；「应该是」「不对吧」这类
/// 弱信号刻意不收，避免普通讨论被误记为纠正。命中即记录 `user_correction` 信号。
pub fn looks_like_user_correction(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "你搞错了",
        "搞错了",
        "说错了",
        "做错了",
        "不是这样",
        "别这样",
        "不要再这样",
        "太啰嗦",
        "太长了",
        "太复杂了",
        "废话太多",
        "wrong",
        "incorrect",
        "stop doing",
        "too verbose",
        "don't do that",
        "do not do that",
        "not what i asked",
        "i said no",
    ];
    let lower = text.to_ascii_lowercase();
    MARKERS
        .iter()
        .any(|marker| text.contains(marker) || lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_skills() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        (dir, skills)
    }

    #[test]
    fn signals_roundtrip_and_ring_trim() {
        let (_temp, skills) = temp_skills();
        for i in 0..(SIGNALS_KEEP_MAX + 20) {
            record_signal(
                &skills,
                "circuit_breaker",
                &[format!("skill-{}", i % 3)],
                &format!("detail {i}"),
                "cli:console",
            );
        }
        let recent = recent_signals(&skills, usize::MAX);
        // 滚动裁剪不变量：条数不超过上限，且保底保留上限一半（溢出点之后新写入
        // 继续累积到下一次溢出，因此精确条数取决于写入总量落点）。
        assert!(
            recent.len() <= SIGNALS_KEEP_MAX && recent.len() >= SIGNALS_KEEP_MAX / 2,
            "裁剪后条数应落在 [半量, 上限] 内，实际 {}",
            recent.len()
        );
        assert!(recent.last().unwrap().detail.contains("detail 319"));

        let digest = signal_digest_for_review(&skills, 50).unwrap();
        assert!(digest.contains("circuit_breaker"), "{digest}");
        assert!(
            !digest.contains("(global)"),
            "有归因时不应出现全局行: {digest}"
        );
    }

    #[test]
    fn global_signal_uses_placeholder_skill() {
        let (_temp, skills) = temp_skills();
        record_signal(
            &skills,
            "user_correction",
            &[],
            "不对的回复风格",
            "napcat:123",
        );
        let digest = signal_digest_for_review(&skills, 10).unwrap();
        assert!(digest.contains("(global): user_correction"), "{digest}");
    }

    #[test]
    fn empty_log_renders_none() {
        let (_temp, skills) = temp_skills();
        assert!(signal_digest_for_review(&skills, 10).is_none());
    }

    #[test]
    fn failure_classifier_is_deterministic() {
        assert_eq!(
            classify_failure_signal("工具循环熔断：同一调用重复出现"),
            "circuit_breaker"
        );
        assert_eq!(
            classify_failure_signal("sub-agent 完成验证失败：仍有未完成的团队任务 #1"),
            "team_gate_rejected"
        );
        assert_eq!(
            classify_failure_signal("完成验证未通过：结果验证失败"),
            "completion_rejected"
        );
        assert_eq!(
            classify_failure_signal("达到 ReAct 迭代上限 256"),
            "iter_limit"
        );
        // 主循环生产文案：同时含「迭代上限」与「完成验证」，必须按迭代上限归类。
        assert_eq!(
            classify_failure_signal("达到 ReAct 迭代上限 256，任务未通过完成验证。"),
            "iter_limit"
        );
        assert_eq!(classify_failure_signal("其它错误"), "failed");
    }

    #[test]
    fn correction_heuristic_stays_conservative() {
        assert!(looks_like_user_correction(
            "不对，你搞错了，应该是 A 而不是 B"
        ));
        assert!(looks_like_user_correction("That's wrong, stop doing that."));
        assert!(!looks_like_user_correction("应该是明天开会吧？"));
        assert!(!looks_like_user_correction("帮我看一下这段代码"));
    }
}
