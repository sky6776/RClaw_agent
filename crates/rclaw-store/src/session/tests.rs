use super::*;
use rclaw_core::{ContentBlock, Role};
use serde_json::json;
use tempfile::tempdir;

fn store() -> (tempfile::TempDir, SessionStore) {
    let dir = tempdir().unwrap();
    let layout = Layout::new(dir.path().join("agent"));
    (dir, SessionStore::new(layout))
}

/// 绕过 `append`，直接写一行原始 `data`（模拟旧数据 / 手工编辑 / 损坏行）。
fn raw_insert(s: &SessionStore, channel: &str, chat_id: &str, role: &str, data: &str) -> i64 {
    s.db()
        .transaction(|tx| {
            let sid = ensure_session(tx, channel, chat_id)?;
            tx.execute(
                "INSERT INTO message(session_id, role, ts, data) VALUES (?1, ?2, 1, ?3)",
                params![sid, role, data],
            )?;
            Ok(tx.last_insert_rowid())
        })
        .unwrap()
}

fn raw_insert_msg(s: &SessionStore, channel: &str, chat_id: &str, m: &Message) -> i64 {
    raw_insert(
        s,
        channel,
        chat_id,
        role_name(&m.role),
        &serde_json::to_string(m).unwrap(),
    )
}

fn message_count(s: &SessionStore) -> i64 {
    s.db()
        .with(|c| Ok(c.query_row("SELECT count(*) FROM message", [], |r| r.get(0))?))
        .unwrap()
}

fn texts(h: &[Message]) -> Vec<String> {
    h.iter()
        .map(|m| {
            m.content
                .iter()
                .filter_map(ContentBlock::as_text)
                .collect::<Vec<_>>()
                .join("")
        })
        .collect()
}

#[test]
fn append_assigns_ids_and_stamps_ts() {
    let (_g, s) = store();
    let a = s
        .append("cli", "console", &Message::plain(Role::User, "hi"))
        .unwrap();
    let b = s
        .append("cli", "console", &Message::plain(Role::Assistant, "hello"))
        .unwrap();
    assert!(a > 0 && b > a, "id 单调递增: {a} {b}");
    let h = s.load_history("cli", "console", 10, 1024 * 1024).unwrap();
    assert_eq!(h.len(), 2);
    assert_eq!(h[0].id, a);
    assert_eq!(h[1].id, b);
    assert_eq!(h[0].content[0].as_text().unwrap(), "hi");
    assert!(h[0].ts > 0, "ts 为 0 时应盖章");
    // 另一会话的 id 继续全局递增，互不可见。
    let c = s
        .append("cli", "other", &Message::plain(Role::User, "x"))
        .unwrap();
    assert!(c > b);
    assert_eq!(s.load_all("cli", "other").unwrap().len(), 1);
    assert_eq!(s.load_all("cli", "console").unwrap().len(), 2);
}

#[test]
fn stored_data_omits_id_and_load_restores_it() {
    let (_g, s) = store();
    let mut m = Message::plain(Role::User, "hi");
    m.id = 999; // 调用方残留的 id 不得入列
    let id = s.append("cli", "c", &m).unwrap();
    let data: String = s
        .db()
        .with(|c| {
            Ok(c.query_row("SELECT data FROM message WHERE id = ?1", params![id], |r| {
                r.get(0)
            })?)
        })
        .unwrap();
    assert!(!data.contains("\"id\""), "data 不应含 id: {data}");
    assert_eq!(s.load_transcript("cli", "c").unwrap()[0].id, id);
}

#[test]
fn tool_chain_is_one_transaction() {
    let (_g, s) = store();
    let asst = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path":"agent/x"}),
        }],
        tool_call_id: None,
        ts: 0,
    };
    let tool_result = Message {
        id: 0,
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: "ok".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    let (a, t) = s
        .append_tool_chain("cli", "console", &asst, &tool_result)
        .unwrap();
    assert_eq!(t, a + 1);
    let h = s.load_history("cli", "console", 10, 1024 * 1024).unwrap();
    assert_eq!(h.len(), 2);
    assert!(h[0].has_tool_calls());

    // 带 checkpoint 的变体：工具链 + checkpoint 同事务。
    let cp = FileCheckpointRecord {
        id: 0,
        group_id: 0,
        path: "D:/x.txt".into(),
        before: Some("old".into()),
        before_hash: Some("h0".into()),
        after_hash: Some("h1".into()),
        is_rewind: false,
        anchor_message_id: None,
    };
    s.append_tool_chain_with_checkpoints("cli", "console", &asst, &tool_result, 77, &[cp])
        .unwrap();
    let cps = s.load_file_checkpoints("cli", "console").unwrap();
    assert_eq!(cps.len(), 1);
    assert_eq!(cps[0].group_id, 77);
    assert_eq!(cps[0].path, "D:/x.txt");
    assert_eq!(cps[0].before.as_deref(), Some("old"));
    assert!(cps[0].id > 0);
}

#[test]
fn orphan_tool_result_at_head_is_skipped() {
    let (_g, s) = store();
    let orphan = Message {
        id: 0,
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_x".into(),
            content: "r".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    s.append("cli", "c", &orphan).unwrap();
    s.append("cli", "c", &Message::plain(Role::Assistant, "ans"))
        .unwrap();
    let h = s.load_history("cli", "c", 10, 1024 * 1024).unwrap();
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].role, Role::Assistant);
}

#[test]
fn max_msgs_trims_to_tail() {
    let (_g, s) = store();
    for i in 0..5 {
        s.append("cli", "c", &Message::plain(Role::User, format!("m{i}")))
            .unwrap();
    }
    let h = s.load_history("cli", "c", 2, 1024 * 1024).unwrap();
    assert_eq!(h.len(), 2);
    assert_eq!(h[0].content[0].as_text().unwrap(), "m3");
    // 裁剪只影响视图，库中原始行不变。
    assert_eq!(message_count(&s), 5);
    assert_eq!(s.load_transcript("cli", "c").unwrap().len(), 5);
}

#[test]
fn max_bytes_trims_from_head() {
    let (_g, s) = store();
    for i in 0..5 {
        s.append("cli", "c", &Message::plain(Role::User, "x".repeat(200) + &i.to_string()))
            .unwrap();
    }
    let h = s.load_history("cli", "c", 100, 700).unwrap();
    assert!(h.len() < 5 && !h.is_empty(), "{}", h.len());
    assert!(texts(&h).last().unwrap().ends_with('4'), "应保留最新消息");
}

/// 崩溃残留：尾部留下一条「带 tool_use 的 assistant」而无配对 tool_result。
/// ADR-0004（A2）：合成 aborted 占位补齐配对、保留整条链。
#[test]
fn dangling_trailing_tool_use_gets_aborted_placeholder() {
    let (_g, s) = store();
    s.append("cli", "c", &Message::plain(Role::User, "hi"))
        .unwrap();
    s.append("cli", "c", &Message::plain(Role::Assistant, "hello"))
        .unwrap();
    let dangling = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_z".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path":"agent/x"}),
        }],
        tool_call_id: None,
        ts: 0,
    };
    s.append("cli", "c", &dangling).unwrap();

    let h = s.load_history("cli", "c", 10, 1024 * 1024).unwrap();
    assert_eq!(h.len(), 4, "{h:?}");
    assert!(h[2].has_tool_calls());
    let placeholder = h.iter().rev().find_map(|m| {
        m.content.iter().find_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.clone(), content.clone(), *is_error)),
            _ => None,
        })
    });
    let (id, content, is_error) = placeholder.expect("应为 call_z 合成 aborted 占位");
    assert_eq!(id, "call_z");
    assert!(is_error);
    assert!(content.contains("工具执行被中断"), "{content}");
}

/// 中间一行 `data` 损坏被跳过时，call_1 的结果缺失属于「本地中断遗留」：保留并合成占位；
/// 后面的合法工具链（call_2）原样保留。
#[test]
fn middle_corrupt_row_gets_aborted_placeholder() {
    let (_g, s) = store();
    let asst1 = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read_file".into(),
            input: json!({"path":"agent/x"}),
        }],
        tool_call_id: None,
        ts: 0,
    };
    let asst2 = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_2".into(),
            name: "read_file".into(),
            input: json!({"path":"agent/y"}),
        }],
        tool_call_id: None,
        ts: 0,
    };
    let res2 = Message {
        id: 0,
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_2".into(),
            content: "ok".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    s.append("cli", "c", &Message::plain(Role::User, "hi"))
        .unwrap();
    s.append("cli", "c", &asst1).unwrap();
    raw_insert(&s, "cli", "c", "user", "{ this is not valid json");
    s.append("cli", "c", &asst2).unwrap();
    s.append("cli", "c", &res2).unwrap();

    let h = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();
    assert!(h.iter().any(|m| m.content.iter().any(|b| matches!(
        b,
        ContentBlock::ToolUse { id, .. } if id == "call_1"
    ))));
    assert!(h.iter().any(|m| m.content.iter().any(|b| matches!(
        b,
        ContentBlock::ToolResult { tool_use_id, content, is_error }
            if tool_use_id == "call_1" && *is_error && content.contains("工具执行被中断")
    ))));
    assert!(h.iter().any(|m| m.content.iter().any(|b| matches!(
        b,
        ContentBlock::ToolUse { id, .. } if id == "call_2"
    ))));
    assert!(h.iter().any(|m| m.content.iter().any(|b| matches!(
        b,
        ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call_2"
    ))));
}

#[test]
fn role_tool_message_pairs_with_preceding_tool_use() {
    let (_g, s) = store();
    let asst = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read_file".into(),
            input: json!({}),
        }],
        tool_call_id: None,
        ts: 0,
    };
    let tool = Message {
        id: 0,
        role: Role::Tool,
        content: vec![ContentBlock::text("result")],
        tool_call_id: Some("call_1".into()),
        ts: 0,
    };
    s.append("cli", "c", &asst).unwrap();
    s.append("cli", "c", &tool).unwrap();
    let h = s.load_history("cli", "c", 10, 1024 * 1024).unwrap();
    assert_eq!(h.len(), 2);
    assert!(h.iter().any(|m| m.has_tool_calls()));
    assert!(h.iter().any(|m| m.role == Role::Tool));
}

#[test]
fn multiple_role_tool_messages_pair_with_one_assistant() {
    let (_g, s) = store();
    let asst = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "read_file".into(),
                input: json!({}),
            },
            ContentBlock::ToolUse {
                id: "call_2".into(),
                name: "list_dir".into(),
                input: json!({}),
            },
        ],
        tool_call_id: None,
        ts: 0,
    };
    let tool1 = Message {
        id: 0,
        role: Role::Tool,
        content: vec![ContentBlock::text("result 1")],
        tool_call_id: Some("call_1".into()),
        ts: 0,
    };
    let tool2 = Message {
        id: 0,
        role: Role::Tool,
        content: vec![ContentBlock::text("result 2")],
        tool_call_id: Some("call_2".into()),
        ts: 0,
    };
    s.append("cli", "c", &asst).unwrap();
    s.append("cli", "c", &tool1).unwrap();
    s.append("cli", "c", &tool2).unwrap();

    let h = s.load_history("cli", "c", 10, 1024 * 1024).unwrap();
    assert_eq!(h.len(), 3);
    assert!(h[0].has_tool_calls());
    assert_eq!(h[1].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(h[2].tool_call_id.as_deref(), Some("call_2"));
}

#[test]
fn orphan_role_tool_at_head_is_skipped() {
    let (_g, s) = store();
    let orphan = Message {
        id: 0,
        role: Role::Tool,
        content: vec![ContentBlock::text("r")],
        tool_call_id: Some("call_x".into()),
        ts: 0,
    };
    s.append("cli", "c", &orphan).unwrap();
    s.append("cli", "c", &Message::plain(Role::Assistant, "ans"))
        .unwrap();
    let h = s.load_history("cli", "c", 10, 1024 * 1024).unwrap();
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].role, Role::Assistant);
}

#[test]
fn mismatched_role_tool_drops_preceding_tool_use() {
    let (_g, s) = store();
    let asst = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read_file".into(),
            input: json!({}),
        }],
        tool_call_id: None,
        ts: 0,
    };
    let tool = Message {
        id: 0,
        role: Role::Tool,
        content: vec![ContentBlock::text("r")],
        tool_call_id: Some("call_other".into()),
        ts: 0,
    };
    s.append("cli", "c", &asst).unwrap();
    s.append("cli", "c", &tool).unwrap();
    let h = s.load_history("cli", "c", 10, 1024 * 1024).unwrap();
    assert!(!h.iter().any(|m| m.has_tool_calls()));
}

/// 坏 tool_use（空 name / 非 object input）：整条坏链丢弃，合法链保留。
#[test]
fn bad_tool_use_drops_whole_chain_and_keeps_valid() {
    let (_g, s) = store();
    let bad_asst_empty_name = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_bad1".into(),
            name: String::new(),
            input: json!({"path": "x"}),
        }],
        tool_call_id: None,
        ts: 0,
    };
    let bad_res1 = Message {
        id: 0,
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_bad1".into(),
            content: "r".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    let bad_asst_null_input = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_bad2".into(),
            name: "read_file".into(),
            input: json!(null),
        }],
        tool_call_id: None,
        ts: 0,
    };
    let bad_res2 = Message {
        id: 0,
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_bad2".into(),
            content: "r".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    let good_asst = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_good".into(),
            name: "read_file".into(),
            input: json!({"path": "y"}),
        }],
        tool_call_id: None,
        ts: 0,
    };
    let good_res = Message {
        id: 0,
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_good".into(),
            content: "ok".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    for m in [
        &bad_asst_empty_name,
        &bad_res1,
        &bad_asst_null_input,
        &bad_res2,
        &good_asst,
        &good_res,
    ] {
        raw_insert_msg(&s, "cli", "c", m);
    }

    let h = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();
    for bad_id in ["call_bad1", "call_bad2"] {
        assert!(!h.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolUse { id, .. } if id == bad_id
        ))));
        assert!(!h.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == bad_id
        ))));
    }
    assert_eq!(h.len(), 2, "仅合法工具链应保留");
    assert!(h[0].has_tool_calls());
}

#[test]
fn duplicate_tool_result_within_one_carrier_drops_chain() {
    let (_g, s) = store();
    let asst = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read_file".into(),
            input: json!({"path": "x"}),
        }],
        tool_call_id: None,
        ts: 0,
    };
    let dup_carrier = Message {
        id: 0,
        role: Role::User,
        content: vec![
            ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "r1".into(),
                is_error: false,
            },
            ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "r2".into(),
                is_error: false,
            },
        ],
        tool_call_id: None,
        ts: 0,
    };
    let follow = Message::plain(Role::Assistant, "done");
    for m in [&asst, &dup_carrier, &follow] {
        raw_insert_msg(&s, "cli", "c", m);
    }

    let h = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();
    assert!(!h.iter().any(|m| m.has_tool_calls()));
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].content[0].as_text().unwrap(), "done");
}

#[test]
fn role_tool_missing_call_id_with_content_tool_result_drops_chain() {
    let (_g, s) = store();
    let asst = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read_file".into(),
            input: json!({"path": "x"}),
        }],
        tool_call_id: None,
        ts: 0,
    };
    let malformed = Message {
        id: 0,
        role: Role::Tool,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: "r".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    let follow = Message::plain(Role::Assistant, "done");
    for m in [&asst, &malformed, &follow] {
        raw_insert_msg(&s, "cli", "c", m);
    }

    let h = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();
    assert!(!h.iter().any(|m| m.has_tool_calls()));
    assert!(!h.iter().any(|m| m.role == Role::Tool));
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].content[0].as_text().unwrap(), "done");
}

fn tool_use_assistant(id: &str) -> Message {
    Message {
        id: 0,
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: id.into(),
            name: "read_file".into(),
            input: json!({"path": "x"}),
        }],
        tool_call_id: None,
        ts: 0,
    }
}

#[test]
fn missing_tool_result_synthesizes_aborted_placeholder() {
    let (_g, s) = store();
    raw_insert_msg(&s, "cli", "c", &tool_use_assistant("call_1"));
    raw_insert_msg(&s, "cli", "c", &Message::plain(Role::Assistant, "后续消息"));

    let h = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();
    assert_eq!(h.len(), 3, "{}", h.len());
    assert!(h[0].has_tool_calls());
    let placeholder = h[1]
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.as_str(), content.as_str(), *is_error)),
            _ => None,
        })
        .next()
        .expect("应合成占位 tool_result");
    assert_eq!(placeholder.0, "call_1");
    assert!(placeholder.2);
    assert!(placeholder.1.contains("工具执行被中断"));
}

#[test]
fn partially_answered_chain_gets_placeholder_for_missing_only() {
    let (_g, s) = store();
    let asst = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![
            ContentBlock::ToolUse {
                id: "call_a".into(),
                name: "read_file".into(),
                input: json!({}),
            },
            ContentBlock::ToolUse {
                id: "call_b".into(),
                name: "grep".into(),
                input: json!({}),
            },
        ],
        tool_call_id: None,
        ts: 0,
    };
    let carrier = Message {
        id: 0,
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_a".into(),
            content: "已有结果".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    raw_insert_msg(&s, "cli", "c", &asst);
    raw_insert_msg(&s, "cli", "c", &carrier);

    let h = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();
    assert_eq!(h.len(), 3);
    let ids: Vec<&str> = h[2]
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["call_b"]);
}

#[test]
fn foreign_id_carrier_still_drops_whole_chain() {
    let (_g, s) = store();
    let foreign = Message {
        id: 0,
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_other".into(),
            content: "串错的结果".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    raw_insert_msg(&s, "cli", "c", &tool_use_assistant("call_1"));
    raw_insert_msg(&s, "cli", "c", &foreign);

    let h = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();
    assert!(!h.iter().any(|m| m.has_tool_calls()));
    assert!(!h.iter().any(|m| m.content.iter().any(|b| matches!(
        b,
        ContentBlock::ToolResult { content, .. } if content.contains("工具执行被中断")
    ))));
}

// ---------------------------------------------------------------------------
// 非破坏性 compaction boundary
// ---------------------------------------------------------------------------

#[test]
fn compaction_boundary_splices_active_view_and_keeps_transcript() {
    let (_g, s) = store();
    let mut ids = Vec::new();
    for i in 0..8 {
        let role = if i % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        ids.push(s.append("cli", "c", &Message::plain(role, format!("m{i}"))).unwrap());
    }
    // 受保护头部 = m0,m1；摘要掉 m2..m5；尾窗 = m6,m7。
    let summary = Message::plain(Role::User, "[CONTEXT COMPACTION] summary");
    let b = s
        .append_compaction("cli", "c", &summary, Some(ids[1]), ids[6])
        .unwrap();
    assert!(b > ids[7]);

    let view = s.load_all("cli", "c").unwrap();
    assert_eq!(
        texts(&view),
        vec!["m0", "m1", "[CONTEXT COMPACTION] summary", "m6", "m7"]
    );
    assert_eq!(view[2].id, b, "boundary 行回填自己的 id");

    // 压缩后继续追加：落在 after 段。
    s.append("cli", "c", &Message::plain(Role::User, "m8")).unwrap();
    let view = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();
    assert_eq!(
        texts(&view),
        vec!["m0", "m1", "[CONTEXT COMPACTION] summary", "m6", "m7", "m8"]
    );

    // transcript 完整保留（8 + boundary + 1）。
    let full = s.load_transcript("cli", "c").unwrap();
    assert_eq!(full.len(), 10);
    assert_eq!(texts(&full)[8], "[CONTEXT COMPACTION] summary");

    // 转录末尾：按 transcript（含被摘要的 m2..m5）取最近 N 条可回放行，不是压缩视图；
    // boundary 行（摘要）与工具结果载体在 SQL 层就筛掉、不占 N。
    s.append("cli", "c", &tool_use_assistant("call_9")).unwrap();
    s.append(
        "cli",
        "c",
        &Message {
            id: 0,
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_9".into(),
                content: "ok".into(),
                is_error: false,
            }],
            tool_call_id: None,
            ts: 0,
        },
    )
    .unwrap();
    s.append("cli", "c", &Message::plain(Role::Assistant, "m9")).unwrap();
    let tail = s.load_transcript_tail("cli", "c", 100, 1024 * 1024).unwrap();
    assert_eq!(
        texts(&tail),
        vec!["m0", "m1", "m2", "m3", "m4", "m5", "m6", "m7", "m8", "", "m9"],
        "boundary 行与 tool_result 载体不出现；带 tool_use 的 assistant 行保留（回放渲染工具名）"
    );
    let tail = s.load_transcript_tail("cli", "c", 6, 1024 * 1024).unwrap();
    assert_eq!(texts(&tail), vec!["m5", "m6", "m7", "m8", "", "m9"]);
    // 字节上限从最新一端往回累计裁，至少留一条。
    let tail = s.load_transcript_tail("cli", "c", 6, 1).unwrap();
    assert_eq!(texts(&tail), vec!["m9"]);
    // 预算恰好装下最新两条（逐条序列化累计）：第三条起不再纳入。
    let all = s.load_transcript_tail("cli", "c", 6, 1024 * 1024).unwrap();
    let size = |m: &Message| serde_json::to_string(std::slice::from_ref(m)).unwrap().len();
    let two = size(&all[all.len() - 1]) + size(&all[all.len() - 2]);
    let tail = s.load_transcript_tail("cli", "c", 6, two).unwrap();
    assert_eq!(texts(&tail), vec!["", "m9"]);
    assert_eq!(texts(&s.load_transcript_tail("cli", "c", 6, two - 1).unwrap()), vec!["m9"]);
    assert!(s.load_transcript_tail("cli", "none", 6, 1024).unwrap().is_empty());

    // 手改坏的行不让 JSON 函数把整条查询报错：非法 JSON 由 `json_valid` 挡在 SQL 层；
    // `content` 不是数组（标量）时 `json_each` 给出一行非对象值，`CASE` 不去 `json_extract`
    // 它——该行留给 `parse_message`，而 serde 接受「content 为字符串」的简写，所以它照常
    // 出现在 tail 里；数组里的非对象元素同样不被 `json_extract`，解析失败才被跳过。
    raw_insert(&s, "cli", "c", "user", "{not json");
    raw_insert(&s, "cli", "c", "user", r#"{"role":"user","content":"plain string","ts":1}"#);
    raw_insert(&s, "cli", "c", "user", r#"{"role":"user","content":["bare", 1, null],"ts":1}"#);
    s.append("cli", "c", &Message::plain(Role::User, "m10")).unwrap();
    let tail = s.load_transcript_tail("cli", "c", 100, 1024 * 1024).unwrap();
    assert_eq!(
        texts(&tail),
        vec!["m0", "m1", "m2", "m3", "m4", "m5", "m6", "m7", "m8", "", "m9", "plain string", "m10"]
    );
    assert_eq!(texts(&s.load_transcript_tail("cli", "c", 1, 1024 * 1024).unwrap()), vec!["m10"]);
}

/// 受保护头部以一条没有 tool_result 的 tool_use 收尾（工具执行被中断后压缩，`head_until`
/// 正好锚在它上面）：boundary 落库后，活动视图在 head / boundary 的拼接处重新合成
/// 「执行被中断」占位结果——工具链配对不因拼接而断，provider 不会收到孤儿 tool_use。
#[test]
fn compaction_splice_resynthesizes_placeholder_after_dangling_head_tool_use() {
    let (_g, s) = store();
    let m0 = s.append("cli", "c", &Message::plain(Role::User, "m0")).unwrap();
    let dangling = raw_insert_msg(&s, "cli", "c", &tool_use_assistant("call_1"));
    let mut ids = vec![m0, dangling];
    for i in 2..8 {
        let role = if i % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        ids.push(s.append("cli", "c", &Message::plain(role, format!("m{i}"))).unwrap());
    }
    let summary = Message::plain(Role::User, "[CONTEXT COMPACTION] summary");
    let b = s
        .append_compaction("cli", "c", &summary, Some(dangling), ids[6])
        .unwrap();

    let view = s.load_active("cli", "c").unwrap();
    // m0, tool_use, <合成占位 tool_result>, boundary 摘要, m6, m7
    assert_eq!(view.len(), 6, "{:?}", texts(&view));
    assert_eq!(view[1].id, dangling);
    assert!(view[1].has_tool_calls());
    assert_eq!(view[2].id, 0, "合成占位行没有持久化 id");
    let placeholder = view[2]
        .content
        .iter()
        .find_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.as_str(), content.as_str(), *is_error)),
            _ => None,
        })
        .expect("拼接处应合成占位 tool_result");
    assert_eq!(placeholder.0, "call_1");
    assert!(placeholder.2);
    assert!(placeholder.1.contains("工具执行被中断"));
    assert_eq!(view[3].id, b);
    assert_eq!(
        texts(&view[3..]),
        vec!["[CONTEXT COMPACTION] summary", "m6", "m7"]
    );
    // 裁剪加载走同一条修复路径。
    let trimmed = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();
    assert_eq!(trimmed.len(), 6, "{:?}", texts(&trimmed));
}

/// `first_user_messages` 在 SQL 层就跳过 boundary 行与 tool_result 载体：它们不能占掉
/// `limit`，否则压缩几次 / 开头全是工具轮的会话抽不到标题。
#[test]
fn first_user_messages_skip_boundary_rows_and_tool_result_carriers() {
    let (_g, s) = store();
    // 开头：一条 tool_use/tool_result 链，然后才是真实提问。
    s.append("cli", "c", &tool_use_assistant("t1")).unwrap();
    s.append(
        "cli",
        "c",
        &Message {
            id: 0,
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "ok".into(),
                is_error: false,
            }],
            tool_call_id: None,
            ts: 0,
        },
    )
    .unwrap();
    let q1 = s.append("cli", "c", &Message::plain(Role::User, "第一问")).unwrap();
    s.append("cli", "c", &Message::plain(Role::Assistant, "答")).unwrap();
    let q2 = s.append("cli", "c", &Message::plain(Role::User, "第二问")).unwrap();
    s.append("cli", "c", &Message::plain(Role::Assistant, "答2")).unwrap();
    // boundary 行（role=user 的摘要）排在最前面也不会被选中。
    s.append_compaction(
        "cli",
        "c",
        &Message::plain(Role::User, "[CONTEXT COMPACTION] summary"),
        None,
        q2,
    )
    .unwrap();

    let firsts = s.first_user_messages("cli", "c", 1).unwrap();
    assert_eq!(firsts.len(), 1);
    assert_eq!(firsts[0].id, q1, "limit=1 应直接命中第一条真实提问");
    let firsts = s.first_user_messages("cli", "c", 10).unwrap();
    assert_eq!(texts(&firsts), vec!["第一问", "第二问"]);
}

#[test]
fn second_compaction_without_head_supersedes_first() {
    let (_g, s) = store();
    let mut ids = Vec::new();
    for i in 0..6 {
        ids.push(s.append("cli", "c", &Message::plain(Role::User, format!("m{i}"))).unwrap());
    }
    let b1 = s
        .append_compaction("cli", "c", &Message::plain(Role::User, "S1"), Some(ids[0]), ids[4])
        .unwrap();
    let m6 = s.append("cli", "c", &Message::plain(Role::User, "m6")).unwrap();
    let m7 = s.append("cli", "c", &Message::plain(Role::User, "m7")).unwrap();
    // 视图：m0 S1 m4 m5 m6 m7 → 第二次压缩：无受保护头部，尾窗从 m6 起（S1、m4、m5 被摘要）。
    assert_eq!(texts(&s.load_all("cli", "c").unwrap()), vec!["m0", "S1", "m4", "m5", "m6", "m7"]);
    let b2 = s
        .append_compaction("cli", "c", &Message::plain(Role::User, "S2"), None, m6)
        .unwrap();
    assert!(b2 > b1 && b1 > ids[5] && m7 > m6);
    assert_eq!(texts(&s.load_all("cli", "c").unwrap()), vec!["S2", "m6", "m7"]);
}

#[test]
fn compaction_rejects_inverted_anchors() {
    let (_g, s) = store();
    let a = s.append("cli", "c", &Message::plain(Role::User, "a")).unwrap();
    let err = s
        .append_compaction("cli", "c", &Message::plain(Role::User, "S"), Some(a), a)
        .unwrap_err();
    assert!(err.to_string().contains("compaction 锚点非法"), "{err}");
}

#[test]
fn in_turn_compaction_keeps_current_turn_rows() {
    // 轮内压缩：头区 = 历史；本轮已追加的消息（id 在尾窗之后、boundary 之前）必须进入 kept。
    let (_g, s) = store();
    let mut ids = Vec::new();
    for i in 0..4 {
        ids.push(s.append("cli", "c", &Message::plain(Role::User, format!("h{i}"))).unwrap());
    }
    let turn_user = s.append("cli", "c", &Message::plain(Role::User, "turn-user")).unwrap();
    let (a, _t) = s
        .append_tool_chain(
            "cli",
            "c",
            &tool_use_assistant("call_1"),
            &Message {
                id: 0,
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
                tool_call_id: None,
                ts: 0,
            },
        )
        .unwrap();
    assert!(a > turn_user);
    // 头区压缩：保留 h3 起的尾窗；本轮三条自然落在 kept。
    s.append_compaction("cli", "c", &Message::plain(Role::User, "S"), None, ids[3])
        .unwrap();
    let view = s.load_all("cli", "c").unwrap();
    let t = texts(&view);
    assert_eq!(t[0], "S");
    assert_eq!(t[1], "h3");
    assert_eq!(t[2], "turn-user");
    assert!(view[3].has_tool_calls());
    assert_eq!(view.len(), 5);
}

// ---------------------------------------------------------------------------
// 会话管理
// ---------------------------------------------------------------------------

#[test]
fn list_archive_delete_and_stats() {
    let (_g, s) = store();
    assert!(s.list_sessions(None).unwrap().is_empty());
    assert!(!s.session_exists("desktop", "a").unwrap());
    s.append("desktop", "a", &Message::plain(Role::User, "1")).unwrap();
    s.append("desktop", "a", &Message::plain(Role::Assistant, "2")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
    s.append("desktop", "b", &Message::plain(Role::User, "3")).unwrap();
    s.append("cli", "console", &Message::plain(Role::User, "4")).unwrap();

    let desktop = s.list_sessions(Some("desktop")).unwrap();
    assert_eq!(desktop.len(), 2);
    assert_eq!(desktop[0].chat_id, "b", "updated_at 降序");
    assert_eq!(desktop[1].message_count, 2);
    assert!(!desktop[0].archived);
    assert!(desktop[0].created_at > 0 && desktop[0].updated_at >= desktop[0].created_at);
    assert_eq!(s.list_sessions(None).unwrap().len(), 3);

    assert!(s.set_archived("desktop", "a", true).unwrap());
    assert!(!s.set_archived("desktop", "ghost", true).unwrap());
    assert_eq!(s.session_stats().unwrap(), SessionStats { active: 2, archived: 1 });
    let a = s.session_summary("desktop", "a").unwrap().unwrap();
    assert!(a.archived);
    assert!(s.set_archived("desktop", "a", false).unwrap());
    assert!(!s.session_summary("desktop", "a").unwrap().unwrap().archived);

    s.save_usage_anchor(
        "desktop",
        "a",
        "",
        &UsageAnchorRecord {
            total_tokens: 10,
            anchor_message_id: 1,
        },
    )
    .unwrap();
    assert!(s.delete_session("desktop", "a").unwrap());
    assert!(!s.delete_session("desktop", "a").unwrap());
    assert!(s.load_transcript("desktop", "a").unwrap().is_empty());
    assert_eq!(s.load_usage_anchor("desktop", "a", "").unwrap(), None);
    assert_eq!(message_count(&s), 2, "级联删除只影响该会话");
    assert_eq!(s.list_sessions(None).unwrap().len(), 2);
}

#[test]
fn clear_all_sessions_empties_tables_and_keeps_journal() {
    let (_g, s) = store();
    s.append("cli", "c", &Message::plain(Role::User, "1")).unwrap();
    s.append_file_checkpoints(
        "cli",
        "c",
        5,
        &[FileCheckpointRecord {
            id: 0,
            group_id: 0,
            path: "p".into(),
            before: None,
            before_hash: None,
            after_hash: Some("h".into()),
            is_rewind: false,
            anchor_message_id: None,
        }],
    )
    .unwrap();
    s.save_usage_anchor(
        "cli",
        "c",
        "",
        &UsageAnchorRecord {
            total_tokens: 1,
            anchor_message_id: 1,
        },
    )
    .unwrap();
    s.journal()
        .append(crate::JOURNAL_PLAN, "p", &json!({"e": 1}))
        .unwrap();

    s.clear_all_sessions().unwrap();
    assert!(s.list_sessions(None).unwrap().is_empty());
    assert_eq!(message_count(&s), 0);
    assert!(s.load_file_checkpoints("cli", "c").unwrap().is_empty());
    assert_eq!(s.load_usage_anchor("cli", "c", "").unwrap(), None);
    assert_eq!(s.journal().load(crate::JOURNAL_PLAN, "p").unwrap().len(), 1);
    // 清空后可继续使用。
    let id = s.append("cli", "c", &Message::plain(Role::User, "again")).unwrap();
    assert!(id > 0);
}

#[test]
fn first_user_messages_and_scan() {
    let (_g, s) = store();
    s.append("desktop", "a", &Message::plain(Role::Assistant, "greeting")).unwrap();
    s.append("desktop", "a", &Message::plain(Role::User, "title here")).unwrap();
    s.append("desktop", "a", &Message::plain(Role::User, "second")).unwrap();
    let firsts = s.first_user_messages("desktop", "a", 1).unwrap();
    assert_eq!(texts(&firsts), vec!["title here"]);
    assert!(s.first_user_messages("desktop", "none", 5).unwrap().is_empty());

    let mut seen = Vec::new();
    s.scan_messages(Some("desktop"), |summary, m| {
        seen.push((summary.chat_id.clone(), texts(std::slice::from_ref(m))[0].clone()));
    })
    .unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0].0, "a");
}

// ---------------------------------------------------------------------------
// checkpoint / 锚点 / 导出 / 搜索
// ---------------------------------------------------------------------------

#[test]
fn file_checkpoints_keep_insert_order_and_group() {
    let (_g, s) = store();
    let cp = |path: &str, gid: i64| FileCheckpointRecord {
        id: 0,
        group_id: gid,
        path: path.into(),
        before: None,
        before_hash: None,
        after_hash: Some("h".into()),
        is_rewind: gid == 0,
        anchor_message_id: None,
    };
    s.append_file_checkpoints("cli", "c", 10, &[cp("a", 0), cp("b", 0)]).unwrap();
    s.append_file_checkpoints("cli", "c", 20, &[cp("c", 0)]).unwrap();
    // 记录自带 group_id 优先于参数。
    s.append_file_checkpoints("cli", "c", 30, &[cp("d", 99)]).unwrap();
    let all = s.load_file_checkpoints("cli", "c").unwrap();
    assert_eq!(
        all.iter().map(|c| (c.path.as_str(), c.group_id)).collect::<Vec<_>>(),
        vec![("a", 10), ("b", 10), ("c", 20), ("d", 99)]
    );
    assert!(all.windows(2).all(|w| w[0].id < w[1].id));
    assert!(all[0].is_rewind && !all[3].is_rewind);
    assert!(s.load_file_checkpoints("cli", "other").unwrap().is_empty());
}

#[test]
fn usage_anchor_upsert_and_delete() {
    let (_g, s) = store();
    assert_eq!(s.load_usage_anchor("cli", "c", "").unwrap(), None);
    let a1 = UsageAnchorRecord {
        total_tokens: 1200,
        anchor_message_id: 7,
    };
    s.save_usage_anchor("cli", "c", "", &a1).unwrap();
    assert_eq!(s.load_usage_anchor("cli", "c", "").unwrap(), Some(a1));
    let a2 = UsageAnchorRecord {
        total_tokens: 1500,
        anchor_message_id: 9,
    };
    s.save_usage_anchor("cli", "c", "", &a2).unwrap();
    assert_eq!(s.load_usage_anchor("cli", "c", "").unwrap(), Some(a2));
    // cron 维度独立。
    s.save_usage_anchor("cli", "c", "job1", &a1).unwrap();
    s.delete_usage_anchor("cli", "c", "").unwrap();
    assert_eq!(s.load_usage_anchor("cli", "c", "").unwrap(), None);
    assert_eq!(s.load_usage_anchor("cli", "c", "job1").unwrap(), Some(a1));
}

#[test]
fn export_transcript_jsonl_includes_ids_and_all_rows() {
    let (_g, s) = store();
    let a = s.append("cli", "c", &Message::plain(Role::User, "hi")).unwrap();
    s.append_compaction("cli", "c", &Message::plain(Role::User, "S"), None, a)
        .unwrap();
    let out = s.export_transcript_jsonl("cli", "c").unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2);
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["id"], json!(a));
    assert_eq!(first["role"], json!("user"));
    assert!(first["ts"].as_i64().unwrap() > 0);
    // 可反序列化回 Message。
    let back: Message = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(back.content[0].as_text().unwrap(), "S");
    assert_eq!(s.export_transcript_jsonl("cli", "none").unwrap(), "");
}

#[test]
fn search_messages_prefilters_all_terms_case_insensitively() {
    let (_g, s) = store();
    s.append("cli", "c1", &Message::plain(Role::User, "Rust 的 borrow checker")).unwrap();
    s.append("cli", "c2", &Message::plain(Role::User, "python borrow")).unwrap();
    s.append("cli", "c3", &Message::plain(Role::Assistant, "rust only")).unwrap();
    s.set_archived("cli", "c3", true).unwrap();

    let rows = s
        .search_messages(&["rust".to_string(), "BORROW".to_string()], 100)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].chat_id, "c1");
    assert!(!rows[0].archived);
    assert!(rows[0].message.id > 0);

    let rows = s.search_messages(&["rust".to_string()], 100).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|r| r.chat_id == "c3" && r.archived));
    assert_eq!(s.search_messages(&["rust".to_string()], 1).unwrap().len(), 1);
    assert!(s.search_messages(&[String::new()], 10).unwrap().is_empty());
}

#[test]
fn search_messages_folds_unicode_case_and_only_looks_at_text_blocks() {
    let (_g, s) = store();
    // Unicode 大小写：SQLite 内建 lower() 只折 ASCII，这里必须走 rclaw_lower。
    s.append("cli", "c1", &Message::plain(Role::User, "ПРИВЕТ, Ärger mit Ｎｇｉｎｘ")).unwrap();
    // 只在 JSON 骨架 / 工具参数里出现的词不算命中。
    s.append("cli", "c2", &tool_use_assistant("call_1")).unwrap();
    // 文本里含需要 JSON 转义的字符：按解码后的文本匹配。
    s.append("cli", "c3", &Message::plain(Role::User, r#"path "C:\tmp" quoted"#)).unwrap();
    // 工具结果载体不参与搜索。
    s.append(
        "cli",
        "c4",
        &Message {
            id: 0,
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_9".into(),
                content: "привет from tool".into(),
                is_error: false,
            }],
            tool_call_id: None,
            ts: 0,
        },
    )
    .unwrap();
    // compaction boundary 行不参与搜索。
    let a = s.append("cli", "c5", &Message::plain(Role::User, "plain")).unwrap();
    s.append_compaction("cli", "c5", &Message::plain(Role::User, "привет summary"), None, a)
        .unwrap();

    let hit = |term: &str| {
        s.search_messages(&[term.to_string()], 100)
            .unwrap()
            .into_iter()
            .map(|r| r.chat_id)
            .collect::<Vec<_>>()
    };
    assert_eq!(hit("привет"), vec!["c1"], "西里尔大小写折叠；tool 结果与 boundary 不计");
    assert_eq!(hit("ärger"), vec!["c1"]);
    assert_eq!(hit("ｎｇｉｎｘ"), vec!["c1"], "全角字母大小写折叠");
    assert!(hit("read_file").is_empty(), "工具名只在 tool_use 块里，不算文本命中");
    assert!(hit("text").is_empty(), "JSON 骨架字段名不算命中");
    assert_eq!(hit(r#""c:\tmp""#), vec!["c3"], "按解码后的文本匹配转义字符");
}

#[test]
fn compaction_rejects_anchors_outside_session_or_on_boundary_rows() {
    let (_g, s) = store();
    let a = s.append("cli", "c", &Message::plain(Role::User, "a")).unwrap();
    let b = s.append("cli", "c", &Message::plain(Role::User, "b")).unwrap();
    let other = s.append("cli", "other", &Message::plain(Role::User, "o")).unwrap();

    // tail_from 属于别的会话。
    let err = s
        .append_compaction("cli", "c", &Message::plain(Role::User, "S"), None, other)
        .unwrap_err();
    assert!(err.to_string().contains("tail_from"), "{err}");
    // head_until 不存在。
    let err = s
        .append_compaction("cli", "c", &Message::plain(Role::User, "S"), Some(a - 1), b)
        .unwrap_err();
    assert!(err.to_string().contains("head_until"), "{err}");
    // 会话不存在。
    let err = s
        .append_compaction("cli", "ghost", &Message::plain(Role::User, "S"), None, a)
        .unwrap_err();
    assert!(err.to_string().contains("不存在"), "{err}");

    let b1 = s
        .append_compaction("cli", "c", &Message::plain(Role::User, "S1"), Some(a), b)
        .unwrap();
    let c = s.append("cli", "c", &Message::plain(Role::User, "c")).unwrap();
    // tail_from 指向 boundary 行本身。
    let err = s
        .append_compaction("cli", "c", &Message::plain(Role::User, "S2"), None, b1)
        .unwrap_err();
    assert!(err.to_string().contains("普通消息"), "{err}");
    // tail_from 回退到上一 boundary 的尾窗之前：被摘要的中段会复活。
    let err = s
        .append_compaction("cli", "c", &Message::plain(Role::User, "S2"), None, a)
        .unwrap_err();
    assert!(err.to_string().contains("回退"), "{err}");
    // 受保护头部不得扩大（上一轮为 a，这轮想保护到 b）。
    let err = s
        .append_compaction("cli", "c", &Message::plain(Role::User, "S2"), Some(b), c)
        .unwrap_err();
    assert!(err.to_string().contains("受保护头部"), "{err}");
    // 合法：无头部、尾窗从 c 起。
    s.append_compaction("cli", "c", &Message::plain(Role::User, "S2"), None, c)
        .unwrap();
    assert_eq!(texts(&s.load_all("cli", "c").unwrap()), vec!["S2", "c"]);
    // 已无受保护头部后不得再引入头部。
    let d = s.append("cli", "c", &Message::plain(Role::User, "d")).unwrap();
    let err = s
        .append_compaction("cli", "c", &Message::plain(Role::User, "S3"), Some(c), d)
        .unwrap_err();
    assert!(err.to_string().contains("受保护头部"), "{err}");
}

#[test]
fn active_view_never_includes_older_boundary_rows() {
    // 第二次压缩的尾窗若从上一 boundary 之前的普通行起算，上一 boundary 行落在
    // kept 区间内：它必须被过滤掉，否则两份摘要同时进 prompt。
    let (_g, s) = store();
    let mut ids = Vec::new();
    for i in 0..4 {
        ids.push(s.append("cli", "c", &Message::plain(Role::User, format!("m{i}"))).unwrap());
    }
    s.append_compaction("cli", "c", &Message::plain(Role::User, "S1"), None, ids[2])
        .unwrap();
    s.append("cli", "c", &Message::plain(Role::User, "m4")).unwrap();
    // 尾窗仍从 m2 起（允许：未回退），区间 [m2, B2) 内含 m2 m3 B1 m4。
    s.append_compaction("cli", "c", &Message::plain(Role::User, "S2"), None, ids[2])
        .unwrap();
    assert_eq!(
        texts(&s.load_all("cli", "c").unwrap()),
        vec!["S2", "m2", "m3", "m4"],
        "S1 不得混进视图"
    );
}

#[test]
fn export_transcript_marks_boundary_rows_with_meta() {
    let (_g, s) = store();
    let a = s.append("cli", "c", &Message::plain(Role::User, "hi")).unwrap();
    s.append_compaction("cli", "c", &Message::plain(Role::User, "S"), None, a)
        .unwrap();
    let out = s.export_transcript_jsonl("cli", "c").unwrap();
    let lines: Vec<serde_json::Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(lines[0].get("meta").is_none(), "普通行不带 meta");
    assert_eq!(lines[1]["meta"]["compaction"]["tail_from"], json!(a));
    assert!(lines[1]["meta"]["compaction"]["head_until"].is_null());
}

#[test]
fn checkpoint_meta_and_range_loaders() {
    let (_g, s) = store();
    let cp = |path: &str, before: &str| FileCheckpointRecord {
        id: 0,
        group_id: 0,
        path: path.into(),
        before: Some(before.into()),
        before_hash: Some(format!("h-{before}")),
        after_hash: Some("after".into()),
        is_rewind: false,
        anchor_message_id: None,
    };
    s.append_file_checkpoints("cli", "c", 10, &[cp("a", "A0"), cp("b", "B0")]).unwrap();
    s.append_file_checkpoints("cli", "c", 20, &[cp("a", "A1")]).unwrap();

    let meta = s.load_file_checkpoint_meta("cli", "c").unwrap();
    assert_eq!(meta.len(), 3);
    assert_eq!(
        meta.iter().map(|m| (m.path.as_str(), m.group_id)).collect::<Vec<_>>(),
        vec![("a", 10), ("b", 10), ("a", 20)]
    );
    assert_eq!(meta[2].before_hash.as_deref(), Some("h-A1"));

    let from_second_group = s.load_file_checkpoints_from("cli", "c", meta[2].id).unwrap();
    assert_eq!(from_second_group.len(), 1);
    assert_eq!(from_second_group[0].before.as_deref(), Some("A1"));
    assert_eq!(
        s.load_file_checkpoints_from("cli", "c", 0).unwrap().len(),
        3,
        "min_id=0 等价于全部"
    );
    assert!(s.load_file_checkpoint_meta("cli", "none").unwrap().is_empty());
}

#[test]
fn checkpoint_only_writes_do_not_touch_session_updated_at() {
    let (_g, s) = store();
    s.append("cli", "a", &Message::plain(Role::User, "1")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
    s.append("cli", "b", &Message::plain(Role::User, "2")).unwrap();
    assert_eq!(s.list_sessions(None).unwrap()[0].chat_id, "b");
    std::thread::sleep(std::time::Duration::from_millis(3));

    let before = s.session_summary("cli", "a").unwrap().unwrap().updated_at;
    s.append_file_checkpoints(
        "cli",
        "a",
        1,
        &[FileCheckpointRecord {
            id: 0,
            group_id: 0,
            path: "p".into(),
            before: None,
            before_hash: None,
            after_hash: Some("h".into()),
            is_rewind: false,
            anchor_message_id: None,
        }],
    )
    .unwrap();
    assert_eq!(s.session_summary("cli", "a").unwrap().unwrap().updated_at, before);
    assert_eq!(s.list_sessions(None).unwrap()[0].chat_id, "b", "a 不应被顶到最前");
    // 不存在的会话：checkpoint 写入仍会创建会话行。
    s.append_file_checkpoints(
        "cli",
        "fresh",
        1,
        &[FileCheckpointRecord {
            id: 0,
            group_id: 0,
            path: "p".into(),
            before: None,
            before_hash: None,
            after_hash: None,
            is_rewind: false,
            anchor_message_id: None,
        }],
    )
    .unwrap();
    assert!(s.session_exists("cli", "fresh").unwrap());
}

#[test]
fn ids_keep_growing_after_clear_all_sessions() {
    // AUTOINCREMENT：清空后新 id 仍大于清空前的最大 id，旧的 usage / 压缩锚点不会被
    // 新行“顶替”。
    let (_g, s) = store();
    let before = s.append("cli", "c", &Message::plain(Role::User, "1")).unwrap();
    s.clear_all_sessions().unwrap();
    let after = s.append("cli", "c", &Message::plain(Role::User, "2")).unwrap();
    assert!(after > before, "{after} <= {before}");
}

#[test]
fn tool_chain_failure_rolls_back_whole_transaction() {
    // 把 file_checkpoint 表改名制造插入失败：assistant 行不得单独落库。
    let (_g, s) = store();
    s.append("cli", "c", &Message::plain(Role::User, "u")).unwrap();
    s.db()
        .with(|c| {
            c.execute_batch("ALTER TABLE file_checkpoint RENAME TO file_checkpoint_gone")?;
            Ok(())
        })
        .unwrap();
    let err = s
        .append_tool_chain_with_checkpoints(
            "cli",
            "c",
            &tool_use_assistant("call_1"),
            &Message {
                id: 0,
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
                tool_call_id: None,
                ts: 0,
            },
            7,
            &[FileCheckpointRecord {
                id: 0,
                group_id: 0,
                path: "p".into(),
                before: None,
                before_hash: None,
                after_hash: Some("h".into()),
                is_rewind: false,
                anchor_message_id: None,
            }],
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::Sqlite(_)), "{err}");
    assert_eq!(message_count(&s), 1, "assistant / tool 行已随事务回滚");
    s.db()
        .with(|c| {
            c.execute_batch("ALTER TABLE file_checkpoint_gone RENAME TO file_checkpoint")?;
            Ok(())
        })
        .unwrap();
}

#[test]
fn tool_use_counts_only_counts_assistant_tool_uses() {
    let msgs = vec![
        tool_use_assistant("call_1"),
        Message::plain(Role::User, "x"),
        tool_use_assistant("call_2"),
    ];
    let counts = tool_use_counts(&msgs);
    assert_eq!(counts.get("read_file"), Some(&2));
}

// ---------------------------------------------------------------------------
// 会话任意锚点 fork（第十二轮）
// ---------------------------------------------------------------------------

/// 造一段 user/assistant 交替的会话，返回消息 id。
fn chat_sequence(s: &SessionStore, n: usize) -> Vec<i64> {
    let mut ids = Vec::new();
    for i in 0..n {
        let role = if i % 2 == 0 { Role::User } else { Role::Assistant };
        ids.push(s.append("cli", "c", &Message::plain(role, format!("m{i}"))).unwrap());
    }
    ids
}

#[test]
fn fork_copies_active_view_prefix_and_keeps_source_untouched() {
    let (_g, s) = store();
    let ids = chat_sequence(&s, 6); // m0(u) m1(a) m2(u) m3(a) m4(u) m5(a)

    let fork = s
        .fork_session("cli", "c", ids[2], "c-fork-1", false)
        .expect("fork 应成功");
    assert_eq!(fork.copied, 3, "锚点 m2 及其前共 3 条");
    assert_eq!(fork.anchor_message_id, ids[2]);

    // 源会话原样：可见历史一条不缺、不被折叠。
    assert_eq!(
        texts(&s.load_all("cli", "c").unwrap()),
        vec!["m0", "m1", "m2", "m3", "m4", "m5"]
    );
    // 新会话可见历史 = 源活动视图前缀。
    assert_eq!(
        texts(&s.load_all("cli", "c-fork-1").unwrap()),
        vec!["m0", "m1", "m2"]
    );
    // 血缘可查回。
    let lineage = s.fork_parent("cli", "c-fork-1").unwrap().expect("有血缘");
    assert_eq!(lineage.parent_chat_id, "c");
    assert_eq!(lineage.anchor_message_id, ids[2]);
    // 非 fork 会话无血缘。
    assert!(s.fork_parent("cli", "c").unwrap().is_none());
}

#[test]
fn fork_rejects_unknown_or_hidden_anchor_and_duplicate_target() {
    let (_g, s) = store();
    let ids = chat_sequence(&s, 8);
    // 摘要掉 m2..m5（受保护头部 m0,m1；尾窗 m6,m7）。
    let summary = Message::plain(Role::User, "[CONTEXT COMPACTION] summary");
    s.append_compaction("cli", "c", &summary, Some(ids[1]), ids[6])
        .unwrap();

    // 被折叠的消息不可作锚（不在活动视图）。
    let err = s.fork_session("cli", "c", ids[3], "c-fork-x", false).unwrap_err();
    assert!(err.to_string().contains("不在"), "{err}");

    // 视图内的锚点（尾窗 m6）照常。
    let fork = s.fork_session("cli", "c", ids[6], "c-fork-a", false).unwrap();
    assert_eq!(fork.copied, 4, "m0,m1,summary,m6");
    // 目标已存在 → 拒绝覆盖。
    let err = s.fork_session("cli", "c", ids[6], "c-fork-a", false).unwrap_err();
    assert!(err.to_string().contains("已存在"), "{err}");
    // 源会话不存在。
    let err = s.fork_session("cli", "nope", ids[0], "x", false).unwrap_err();
    assert!(err.to_string().contains("不存在"), "{err}");
}

#[test]
fn fork_copies_boundary_summary_as_plain_row() {
    let (_g, s) = store();
    let ids = chat_sequence(&s, 8);
    let summary = Message::plain(Role::User, "[CONTEXT COMPACTION] summary");
    s.append_compaction("cli", "c", &summary, Some(ids[1]), ids[6])
        .unwrap();
    s.fork_session("cli", "c", ids[7], "c-fork-b", false).unwrap();

    // 新会话里 boundary 是普通行（meta NULL）：可见历史是 fork 时刻的完整可见视图，
    // 且新会话自己后续再压缩时不受旧锚点污染。
    let meta_rows: i64 = s
        .db()
        .with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM message m JOIN session s ON s.id = m.session_id \
                 WHERE s.chat_id = 'c-fork-b' AND m.meta IS NOT NULL",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(meta_rows, 0, "fork 会话不得携带源会话的 boundary meta");
    assert_eq!(
        texts(&s.load_all("cli", "c-fork-b").unwrap()),
        vec!["m0", "m1", "[CONTEXT COMPACTION] summary", "m6", "m7"]
    );
}

#[test]
fn recent_user_anchors_list_real_user_messages_newest_first() {
    let (_g, s) = store();
    let ids = chat_sequence(&s, 4);
    // 工具结果载体（role=user 但全是 tool_result 块）与 boundary 不进列表。
    let carrier = Message {
        id: 0,
        role: Role::User,
        content: vec![rclaw_core::ContentBlock::ToolResult {
            tool_use_id: "t".into(),
            content: "tool output".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    s.append("cli", "c", &carrier).unwrap();
    let summary = Message::plain(Role::User, "[CONTEXT COMPACTION] summary");
    s.append_compaction("cli", "c", &summary, None, ids[1]).unwrap();

    let anchors = s.recent_user_anchors("cli", "c", 10).unwrap();
    let preview_ids: Vec<i64> = anchors.iter().map(|(id, _, _)| *id).collect();
    // 只有真实 user 消息：m0、m2（boundary 与载体被排除），新在前。
    assert_eq!(preview_ids, vec![ids[2], ids[0]], "{anchors:?}");
    assert_eq!(anchors[0].2, "m2");
    // 长文本截断到 120 字符。
    let long = s
        .append(
            "cli",
            "c",
            &Message::plain(Role::User, "长".repeat(200)),
        )
        .unwrap();
    let anchors = s.recent_user_anchors("cli", "c", 10).unwrap();
    assert_eq!(anchors[0].0, long);
    assert_eq!(anchors[0].2.chars().count(), 120);
    // 不存在的会话 → 空列表。
    assert!(s.recent_user_anchors("cli", "nope", 5).unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// artifact 目录级联清理（第十二轮后续项）
// ---------------------------------------------------------------------------

#[test]
fn delete_session_cascades_artifact_dir_and_clear_wipes_root() {
    let (_g, s) = store();
    let base = s.layout().base.clone();
    let a_dir = base.join("artifacts").join("cli-console");
    std::fs::create_dir_all(&a_dir).unwrap();
    std::fs::write(a_dir.join("1799-read.txt"), "big output").unwrap();
    let other_dir = base.join("artifacts").join("cli-other");
    std::fs::create_dir_all(&other_dir).unwrap();
    // 无关会话的 artifact 也在（键规则与 runtime artifact.rs 同源）。
    s.append("cli", "console", &Message::plain(Role::User, "m")).unwrap();

    assert!(s.delete_session("cli", "console").unwrap());
    assert!(!a_dir.exists(), "会话删除应级联清理其 artifact 目录");
    assert!(other_dir.exists(), "无关会话的 artifact 不受影响");

    // 会话不存在 → 幂等成功，不动目录。
    std::fs::create_dir_all(&a_dir).unwrap();
    assert!(!s.delete_session("cli", "console").unwrap());
    assert!(a_dir.exists(), "删除不存在的会话不得误清目录");

    // /clear：全部会话清空时 artifacts 根一并移除。
    s.clear_all_sessions().unwrap();
    assert!(!base.join("artifacts").exists(), "clear 应清空 artifacts 根");
}

#[test]
fn fork_full_transcript_carries_folded_messages_and_hidden_anchor() {
    let (_g, s) = store();
    let ids = chat_sequence(&s, 8);
    // 摘要掉 m2..m5（受保护头部 m0,m1；尾窗 m6,m7）。
    let summary = Message::plain(Role::User, "[CONTEXT COMPACTION] summary");
    s.append_compaction("cli", "c", &summary, Some(ids[1]), ids[6])
        .unwrap();

    // 被折叠的消息（m3）作锚：视图分叉拒绝，full 分叉成功并携带被折叠原文。
    let err = s
        .fork_session("cli", "c", ids[3], "c-fork-view", false)
        .unwrap_err();
    assert!(err.to_string().contains("不在会话"), "{err}");
    let fork = s
        .fork_session("cli", "c", ids[3], "c-fork-full", true)
        .expect("full 分叉应成功");
    // 完整转录前缀 = m0..m3（含被折叠的 m2、m3 原文）。
    assert_eq!(fork.copied, 4);
    assert_eq!(
        texts(&s.load_all("cli", "c-fork-full").unwrap()),
        vec!["m0", "m1", "m2", "m3"],
        "full 分叉应包含被压缩折叠的原文"
    );
    // 源会话视图不变。
    assert_eq!(
        texts(&s.load_all("cli", "c").unwrap()),
        vec!["m0", "m1", "[CONTEXT COMPACTION] summary", "m6", "m7"]
    );

    // full 分叉锚点在视图内也成立（尾窗 m6）：前缀含全部行（折叠段 + boundary 摘要行）。
    let fork_tail = s
        .fork_session("cli", "c", ids[6], "c-fork-tail", true)
        .unwrap();
    // boundary 行 id 最大（> m7），id ≤ m6 的转录前缀不含它：m0..m6 共 7 行。
    assert_eq!(fork_tail.copied, 7);
}
