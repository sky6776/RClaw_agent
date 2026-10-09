//! B6 学习洞察：从既有持久化产物聚合桌面「自进化」面板的使用统计。
//!
//! 数据源（零新埋点、零索引，按需有界扫描）：
//! - `agent/sessions/**/*.jsonl`：逐条统计日活跃（user/assistant 消息数）与
//!   工具调用 Top N（assistant 消息的 ToolUse 块）；跳过 system 通道（心跳）。
//! - 技能 lifecycle 元数据：use/view/patch 计数 Top N（SkillUsageStore::records）。
//! - `skills/.ledger/ledger.jsonl`：最近技能变更时间线（M2 台账）。
//! - MEMORY.md 占用 / 预算（usage_report）。

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{Local, TimeZone};
use serde::Serialize;

use rclaw_core::{ContentBlock, Message, Role};
use rclaw_store::{MemoryStore, MemoryTarget};
use rclaw_tools::{SkillLifecycleRecord, SkillUsageStore};

/// 活跃统计的天数窗口。
const DAYS: usize = 14;
/// 工具 / 技能 Top 榜长度。
const TOP_N: usize = 8;
/// 最近技能变更条数。
const RECENT_MUTATIONS: usize = 8;
/// 单会话文件扫描上限（防异常巨型文件）。
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// 单次聚合扫描的文件数上限。
const MAX_FILES: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct DayActivity {
    /// 本地日期 `YYYY-MM-DD`。
    pub date: String,
    pub user_msgs: u64,
    pub assistant_msgs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NameCount {
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillUsageDto {
    pub name: String,
    pub use_count: u64,
    pub view_count: u64,
    pub patch_count: u64,
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LedgerLineDto {
    pub id: String,
    pub ts: String,
    pub action: String,
    pub skill: String,
    pub actor: String,
    pub undo_of: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InsightsDto {
    /// 活跃会话数（sessions/ 根下的 jsonl 文件数，不含归档与心跳）。
    pub total_chats: u64,
    pub archived_chats: u64,
    /// 扫描窗口内统计到的消息数（排除 tool_result 载体与压缩摘要行）。
    pub total_messages: u64,
    /// 最近 [`DAYS`] 天活跃（旧 → 新，空洞补零）。
    pub days: Vec<DayActivity>,
    pub top_tools: Vec<NameCount>,
    pub top_skills: Vec<SkillUsageDto>,
    /// MEMORY.md 当前占用 / 预算（chars）。
    pub memory_used: usize,
    pub memory_limit: usize,
    /// 最近技能变更（台账尾部，旧 → 新）。
    pub recent_mutations: Vec<LedgerLineDto>,
}

/// 聚合全部洞察数据。IO 失败均降级为空值（面板显示为零），不报错阻断。
pub fn compute(sessions_dir: &Path, skills_dir: &Path, memory: &MemoryStore) -> InsightsDto {
    let mut agg = SessionAgg::default();
    for (path, archived) in session_files(sessions_dir) {
        if archived {
            agg.archived_chats += 1;
        } else {
            agg.total_chats += 1;
        }
        scan_session_file(&path, &mut agg);
    }

    let days = day_buckets(&agg.day_user, &agg.day_assistant);

    let usage = SkillUsageStore::new(skills_dir.to_path_buf());
    let mut skills: Vec<SkillUsageDto> = usage
        .records()
        .unwrap_or_default()
        .into_iter()
        .map(|(name, record)| skill_dto(&name, &record))
        .collect();
    skills.sort_by(|a, b| {
        b.use_count
            .cmp(&a.use_count)
            .then(b.patch_count.cmp(&a.patch_count))
            .then(a.name.cmp(&b.name))
    });
    skills.truncate(TOP_N);

    let (memory_used, memory_limit) = memory.usage_report(MemoryTarget::Memory);

    InsightsDto {
        total_chats: agg.total_chats,
        archived_chats: agg.archived_chats,
        total_messages: agg.total_messages,
        days,
        top_tools: tool_top(&agg.tool_counts),
        top_skills: skills,
        memory_used,
        memory_limit,
        recent_mutations: ledger_tail(&skills_dir.join(".ledger").join("ledger.jsonl")),
    }
}

#[derive(Default)]
struct SessionAgg {
    total_chats: u64,
    archived_chats: u64,
    total_messages: u64,
    tool_counts: BTreeMap<String, u64>,
    day_user: BTreeMap<String, u64>,
    day_assistant: BTreeMap<String, u64>,
}

/// 枚举 sessions 根 + archived/ 的会话文件，返回 (路径, 是否归档)。
fn session_files(sessions_dir: &Path) -> Vec<(std::path::PathBuf, bool)> {
    let mut out = Vec::new();
    for (dir, archived) in [
        (sessions_dir.to_path_buf(), false),
        (sessions_dir.join("archived"), true),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            // 心跳（system 通道）不是用户会话：不计入会话数，也不参与扫描。
            if p.extension().and_then(|x| x.to_str()) == Some("jsonl") && !is_heartbeat_session(&p)
            {
                out.push((p, archived));
            }
        }
    }
    out.truncate(MAX_FILES);
    out
}

/// 单文件扫描：角色计数 + 日活跃 + 工具调用计数。心跳（system 通道）在文件层跳过。
fn scan_session_file(path: &Path, agg: &mut SessionAgg) {
    if is_heartbeat_session(path) {
        return;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() > MAX_FILE_BYTES {
        return;
    }
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    for line in raw.lines() {
        let Ok(msg) = serde_json::from_str::<Message>(line) else {
            continue;
        };
        if msg.is_tool_result_carrier() {
            continue;
        }
        let text: String = msg
            .content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join(" ");
        if text.trim().starts_with("[CONTEXT COMPACTION") {
            continue;
        }
        match msg.role {
            Role::User => {
                bump_day(&mut agg.day_user, msg.ts);
                agg.total_messages += 1;
            }
            Role::Assistant => {
                bump_day(&mut agg.day_assistant, msg.ts);
                agg.total_messages += 1;
                for block in &msg.content {
                    if let ContentBlock::ToolUse { name, .. } = block {
                        *agg.tool_counts.entry(name.clone()).or_default() += 1;
                    }
                }
            }
            _ => {}
        }
    }
}

/// 心跳会话文件：`<enc("system")>_heartbeat.jsonl`。
fn is_heartbeat_session(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with("system_"))
        .unwrap_or(false)
}

fn bump_day(map: &mut BTreeMap<String, u64>, ts: i64) {
    let Some(day) = local_date(ts) else {
        return;
    };
    *map.entry(day).or_default() += 1;
}

/// unix 秒 → 本地日期串；0（未盖戳）与非法值返回 None。
fn local_date(ts: i64) -> Option<String> {
    if ts <= 0 {
        return None;
    }
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d").to_string())
}

/// 固定 DAYS 天的桶（旧 → 新），无数据的日期补零。
fn day_buckets(
    day_user: &BTreeMap<String, u64>,
    day_assistant: &BTreeMap<String, u64>,
) -> Vec<DayActivity> {
    (0..DAYS)
        .map(|i| {
            let date = (Local::now() - chrono::Duration::days((DAYS - 1 - i) as i64))
                .format("%Y-%m-%d")
                .to_string();
            DayActivity {
                user_msgs: day_user.get(&date).copied().unwrap_or(0),
                assistant_msgs: day_assistant.get(&date).copied().unwrap_or(0),
                date,
            }
        })
        .collect()
}

fn tool_top(counts: &BTreeMap<String, u64>) -> Vec<NameCount> {
    let mut v: Vec<NameCount> = counts
        .iter()
        .map(|(name, count)| NameCount {
            name: name.clone(),
            count: *count,
        })
        .collect();
    v.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
    v.truncate(TOP_N);
    v
}

fn skill_dto(name: &str, record: &SkillLifecycleRecord) -> SkillUsageDto {
    SkillUsageDto {
        name: name.to_string(),
        use_count: record.use_count,
        view_count: record.view_count,
        patch_count: record.patch_count,
        state: record.state.to_string(),
    }
}

/// 台账尾部（旧 → 新）；文件缺失 / 损坏行跳过。
fn ledger_tail(path: &Path) -> Vec<LedgerLineDto> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    lines
        .iter()
        .rev()
        .take(RECENT_MUTATIONS)
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|v| LedgerLineDto {
            id: v
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            ts: v
                .get("ts")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            action: v
                .get("action")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            skill: v
                .get("skill")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            actor: v
                .get("actor")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            undo_of: v
                .get("undo_of")
                .and_then(|x| x.as_str())
                .map(str::to_string),
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg_user(text: &str, ts: i64) -> Message {
        let mut m = Message::plain(Role::User, text);
        m.ts = ts;
        m
    }

    fn msg_assistant_tool(ts: i64) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "calling tool".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            ],
            tool_call_id: None,
            ts,
        }
    }

    #[test]
    fn compute_aggregates_sessions_skills_and_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        let sessions = agent.join("sessions");
        let skills = agent.join("skills");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&skills).unwrap();

        // 主会话：user×2 + assistant 工具轮；心跳文件应被跳过；归档 1 条。
        let today = Local::now().format("%Y-%m-%d").to_string();
        let ts = Local
            .with_ymd_and_hms(
                today.get(0..4).unwrap().parse().unwrap(),
                today.get(5..7).unwrap().parse().unwrap(),
                today.get(8..10).unwrap().parse().unwrap(),
                12,
                0,
                0,
            )
            .single()
            .unwrap()
            .timestamp();
        let mut body = String::new();
        for m in [
            msg_user("hello world", ts),
            msg_user("[CONTEXT COMPACTION — REFERENCE ONLY] summary", ts),
            msg_assistant_tool(ts),
        ] {
            body.push_str(&serde_json::to_string(&m).unwrap());
            body.push('\n');
        }
        std::fs::write(sessions.join("desktop_main.jsonl"), body).unwrap();
        std::fs::write(
            sessions.join("system_heartbeat.jsonl"),
            format!(
                "{}\n",
                serde_json::to_string(&msg_user("heartbeat", ts)).unwrap()
            ),
        )
        .unwrap();
        std::fs::create_dir_all(sessions.join("archived")).unwrap();
        std::fs::write(
            sessions.join("archived").join("desktop_old.jsonl"),
            format!(
                "{}\n",
                serde_json::to_string(&msg_user("old chat", ts)).unwrap()
            ),
        )
        .unwrap();

        // 技能 lifecycle + 台账。
        let usage = SkillUsageStore::new(skills.clone());
        usage
            .record_created("demo", rclaw_tools::SkillOwner::Agent)
            .unwrap();
        usage.record_view_and_use("demo").unwrap();
        std::fs::create_dir_all(skills.join(".ledger")).unwrap();
        std::fs::write(
            skills.join(".ledger").join("ledger.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "id": "e1", "ts": "2026-08-29 10:00:00", "action": "patch",
                    "skill": "demo", "actor": "background_review",
                    "files": []
                })
            ),
        )
        .unwrap();

        let memory = MemoryStore::new(rclaw_store::Layout::new(&agent));
        let out = compute(&sessions, &skills, &memory);

        // 心跳不计、归档单独计、压缩摘要行不计数。
        assert_eq!(out.total_chats, 1);
        assert_eq!(out.archived_chats, 1);
        assert_eq!(out.total_messages, 3, "2 user(去摘要) + 1 assistant");
        assert_eq!(out.days.len(), DAYS);
        let today_bucket = out.days.last().unwrap();
        assert_eq!(
            today_bucket.user_msgs, 2,
            "主会话 1（压缩摘要不计）+ 归档会话 1"
        );
        assert_eq!(today_bucket.assistant_msgs, 1);
        assert_eq!(out.top_tools.len(), 1);
        assert_eq!(out.top_tools[0].name, "shell");
        assert_eq!(out.top_tools[0].count, 1);
        assert_eq!(out.top_skills.len(), 1);
        assert_eq!(out.top_skills[0].name, "demo");
        assert_eq!(out.top_skills[0].use_count, 1);
        assert_eq!(out.recent_mutations.len(), 1);
        assert_eq!(out.recent_mutations[0].skill, "demo");
        assert!(out.memory_limit > 0);
    }
}
