//! `session_search` 工具：跨会话全文检索（hermes-vs-rclaw 自进化分析 B1）。
//!
//! RClaw 的会话是 `agent/sessions/*.jsonl`，此前读取路径只有「最近 N 条」，
//! agent 无法召回自己的历史经历（"上次那个问题怎么解决的"）。本工具对全部
//! 会话文件做按需扫描检索（AND 语义 + 词频计分 + 上下文摘要），无索引、无
//! 新依赖——桌面/单用户规模（数十 MB JSONL）下单次查询为毫秒-百毫秒级，
//! 换来零损坏面与零回填。Hermes 用 SQLite FTS5 + 触发器增量索引；若后续
//! 会话规模增长到扫描不可接受，再升级为 FTS5（工具接口不变）。
//!
//! 检索结果经工具结果注入对话（模型主动调用，非自动注入），与 memory-context
//! 同一防注入纪律：结果是历史素材，不是指令。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{json, Value};

use rclaw_core::{Message, Role};

use crate::{Tool, ToolResult};

/// 单次扫描的单文件大小上限（防异常巨型文件拖垮查询）。
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// 摘要窗口：命中词前后的字符数。
const SNIPPET_CONTEXT_CHARS: usize = 160;
/// 单文件贡献的最大命中条数（防长会话刷榜）。
const MAX_MATCHES_PER_FILE: usize = 32;

/// 跨会话历史检索工具（只读）。
pub struct SessionSearchTool {
    sessions_dir: PathBuf,
}

impl SessionSearchTool {
    pub fn new(sessions_dir: PathBuf) -> Self {
        SessionSearchTool { sessions_dir }
    }
}

#[async_trait]
impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }

    fn description(&self) -> &str {
        "Full-text search across ALL persisted chat session histories (including archived). \
         Use this to recall your own past work: how a problem was solved before, what was \
         decided, prior commands/configs. Query uses AND semantics over whitespace-separated \
         terms (case-insensitive); results are ranked by term frequency. Returns matches with \
         chat_id, role, timestamp and a snippet. Read-only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Space-separated search terms (all terms must match). e.g. 'deploy nginx 502'"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max matches to return (1-10, default 3)."
                }
            },
            "required": ["query"]
        })
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if query.is_empty() {
            return ToolResult::err("query is required".to_string());
        }
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(3)
            .clamp(1, 10) as usize;
        let terms: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();

        // 活跃会话 + 归档会话（archived/ 子目录）都检索。
        let mut matches: Vec<Match> = Vec::new();
        for path in candidate_files(&self.sessions_dir) {
            collect_matches(&path, false, &terms, &mut matches);
        }
        for path in candidate_files(&self.sessions_dir.join("archived")) {
            collect_matches(&path, true, &terms, &mut matches);
        }

        matches.sort_by(|a, b| b.score.cmp(&a.score).then(b.ts.cmp(&a.ts)));
        let total = matches.len();
        let results: Vec<Value> = matches
            .into_iter()
            .take(limit)
            .map(|m| {
                json!({
                    "chat_id": m.chat_id,
                    "archived": m.archived,
                    "role": m.role,
                    "ts": m.ts,
                    "score": m.score,
                    "snippet": m.snippet,
                })
            })
            .collect();
        ToolResult::ok(
            json!({
                "query": query,
                "total_matches": total,
                "returned": results.len(),
                "matches": results,
            })
            .to_string(),
        )
    }
}

struct Match {
    chat_id: String,
    archived: bool,
    role: String,
    ts: i64,
    score: usize,
    snippet: String,
}

/// 枚举目录下的会话文件（`*.jsonl`）。
fn candidate_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .collect()
}

/// 单文件扫描：逐行解析 Message，按 AND 语义计分并生成摘要。
fn collect_matches(path: &Path, archived: bool, terms: &[String], out: &mut Vec<Match>) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() > MAX_FILE_BYTES {
        return;
    }
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let chat_id = parse_chat_id(path);
    let mut per_file = 0usize;
    for line in raw.lines() {
        if per_file >= MAX_MATCHES_PER_FILE {
            return;
        }
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
        let text = text.trim();
        if text.is_empty() || text.starts_with("[CONTEXT COMPACTION") {
            continue;
        }
        let score = match and_score(text, terms) {
            Some(score) => score,
            None => continue,
        };
        out.push(Match {
            chat_id: chat_id.clone(),
            archived,
            role: match msg.role {
                Role::User => "user".to_string(),
                Role::Assistant => "assistant".to_string(),
                other => format!("{other:?}").to_lowercase(),
            },
            ts: msg.ts,
            score,
            snippet: build_snippet(text, &terms[0]),
        });
        per_file += 1;
    }
}

/// AND 语义计分：全部词命中返回总词频，否则 None。
fn and_score(text: &str, terms: &[String]) -> Option<usize> {
    let lower = text.to_lowercase();
    let mut score = 0usize;
    for term in terms {
        let count = lower.matches(term.as_str()).count();
        if count == 0 {
            return None;
        }
        score += count;
    }
    Some(score)
}

/// 从会话文件名解析 chat_id：`<enc_channel>_<enc_chatid>.jsonl` → percent-decode 后半段。
fn parse_chat_id(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let stem = name.strip_suffix(".jsonl").unwrap_or(name);
    let chat_enc = stem.split_once('_').map(|(_, id)| id).unwrap_or(stem);
    percent_decode(chat_enc)
}

/// 最小 percent-decoder（%XX → 字节；非法序列原样保留）。chat_id 字符集有限，
/// 无需引入 percent-encoding 依赖。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            if let Some(v) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 以首个命中词为中心截取摘要窗口（按字符索引折算，避免多字节/大小写变换
/// 导致的字节偏移错位——`to_lowercase` 可能改变字符数，两份文本的字节偏移
/// 不可互换，统一经 char_indices 折算成字符序号后取窗）。
fn build_snippet(text: &str, term: &str) -> String {
    let lower = text.to_lowercase();
    let Some(hit_byte) = lower.find(term) else {
        return text.chars().take(SNIPPET_CONTEXT_CHARS * 2).collect();
    };
    let hit_char = lower[..hit_byte].chars().count();
    let start = hit_char.saturating_sub(SNIPPET_CONTEXT_CHARS);
    let snippet: String = text
        .chars()
        .skip(start)
        .take(SNIPPET_CONTEXT_CHARS * 2)
        .collect();
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if hit_char + SNIPPET_CONTEXT_CHARS < text.chars().count() {
        "…"
    } else {
        ""
    };
    format!("{prefix}{snippet}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(dir: &Path, name: &str, msgs: &[Message]) {
        let mut body = String::new();
        for m in msgs {
            body.push_str(&serde_json::to_string(m).unwrap());
            body.push('\n');
        }
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[tokio::test]
    async fn and_search_finds_across_sessions_and_archives() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        write_session(
            &sessions,
            "desktop_main.jsonl",
            &[
                Message::plain(Role::User, "deploy nginx got 502 error today"),
                Message::plain(Role::Assistant, "the fix was upstream keepalive settings"),
            ],
        );
        write_session(
            &sessions,
            "desktop_side.jsonl",
            &[Message::plain(Role::User, "python script for csv cleanup")],
        );
        write_session(
            &sessions.join("archived"),
            "desktop_old.jsonl",
            &[Message::plain(Role::User, "nginx retry and python notes")],
        );

        let tool = SessionSearchTool::new(sessions.clone());
        // AND 语义：两词都命中才返回。
        let hit = tool
            .execute(json!({"query": "nginx 502", "limit": 5}))
            .await;
        assert!(!hit.is_error, "{}", hit.content);
        assert!(hit.content.contains("main"), "{}", hit.content);
        // assistant 回复不含查询词，AND 语义下不命中——召回的是 user 消息本身。
        assert!(
            hit.content.contains("deploy nginx got 502"),
            "{}",
            hit.content
        );
        // 单独搜 assistant 侧关键词也能召回。
        let fix = tool.execute(json!({"query": "upstream keepalive"})).await;
        assert!(fix.content.contains("the fix was"), "{}", fix.content);

        // 归档会话也被检索。
        let arch = tool.execute(json!({"query": "python notes"})).await;
        assert!(
            arch.content.contains("\"archived\":true"),
            "{}",
            arch.content
        );

        // 缺词 → 无命中。
        let miss = tool.execute(json!({"query": "nginx kubernetes"})).await;
        assert!(
            miss.content.contains("\"total_matches\":0"),
            "{}",
            miss.content
        );
    }

    #[tokio::test]
    async fn chat_id_is_percent_decoded_and_cron_skipped_marker_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        write_session(
            &sessions,
            "wx_%E4%B8%AD%E6%96%87.jsonl",
            &[Message::plain(Role::User, "recall marker hello")],
        );
        // 压缩摘要行应被排除。
        write_session(
            &sessions,
            "desktop_main.jsonl",
            &[Message::plain(
                Role::User,
                "[CONTEXT COMPACTION — REFERENCE ONLY] summary text",
            )],
        );
        let tool = SessionSearchTool::new(sessions);
        let hit = tool.execute(json!({"query": "marker hello"})).await;
        assert!(hit.content.contains("中文"), "{}", hit.content);
        assert_eq!(
            hit.content.matches("\"role\"").count(),
            1,
            "压缩摘要行不得命中"
        );
    }

    #[test]
    fn percent_decode_handles_plain_and_encoded() {
        assert_eq!(percent_decode("main"), "main");
        assert_eq!(percent_decode("%E4%B8%AD"), "中");
        assert_eq!(percent_decode("100%zz"), "100%zz");
    }
}
