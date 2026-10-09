use super::*;
use rclaw_core::{ContentBlock, Role};
use serde_json::json;
use tempfile::tempdir;

fn store() -> (tempfile::TempDir, SessionStore) {
    let dir = tempdir().unwrap();
    let layout = Layout::new(dir.path().join("agent"));
    fs::create_dir_all(&layout.session_dir).unwrap();
    (dir, SessionStore::new(layout))
}

#[test]
fn append_and_load_plain() {
    let (_g, s) = store();
    s.append("cli", "console", &Message::plain(Role::User, "hi"))
        .unwrap();
    s.append("cli", "console", &Message::plain(Role::Assistant, "hello"))
        .unwrap();
    let h = s.load_history("cli", "console", 10, 1024 * 1024).unwrap();
    assert_eq!(h.len(), 2);
    assert_eq!(h[0].content[0].as_text().unwrap(), "hi");
}

#[test]
fn tool_chain_is_atomic_single_file() {
    let (_g, s) = store();
    let asst = Message {
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
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: "ok".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    s.append_tool_chain("cli", "console", &asst, &tool_result)
        .unwrap();

    let content = fs::read_to_string(s.layout.session_path("cli", "console")).unwrap();
    // 两条记录都在，且严格相邻。
    let lines: Vec<&str> = content.trim().lines().collect();
    assert_eq!(lines.len(), 2);
}

#[test]
fn orphan_tool_result_at_head_is_skipped() {
    let (_g, s) = store();
    // 模拟裁剪场景：开头是孤儿 tool_result，后跟普通消息。
    let orphan = Message {
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
    // 孤儿被跳过，只剩 assistant。
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
}

/// 模拟崩溃残留：文件尾部留下一条「带 tool_use 的 assistant」而无配对 tool_result
/// （write_all 在两条记录之间被打断）。ADR-0004（A2）修订：结构合法的 tool_use +
/// 缺失结果属于「本地中断遗留」，repair 合成 aborted 占位补齐配对、保留整条链，
/// 不再把链整段丢弃（占位回放合法，不会 400）。
#[test]
fn dangling_trailing_tool_use_gets_aborted_placeholder() {
    let (_g, s) = store();
    // 正常的一问一答。
    s.append("cli", "c", &Message::plain(Role::User, "hi"))
        .unwrap();
    s.append("cli", "c", &Message::plain(Role::Assistant, "hello"))
        .unwrap();
    // 崩溃残留：只写进了 assistant(tool_use)，没有对应的 tool_result 行。
    let dangling = Message {
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
    // 链保留：user + assistant(hello) + assistant(tool_use) + 合成占位载体。
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

/// 中间一条 JSONL 行损坏被跳过时，call_1 的结果缺失属于「本地中断遗留」
/// （ADR-0004 / A2）：assistant(call_1) 保留并合成 aborted 占位；后面的合法
/// 工具链（call_2）原样保留、不受影响。
#[test]
fn middle_corrupt_tool_result_gets_aborted_placeholder() {
    let (_g, s) = store();
    let path = s.layout.session_path("cli", "c");

    // 构造 JSONL：user → assistant(call_1) → 【损坏行】 → assistant(call_2) → user(result call_2)。
    let user_msg = Message::plain(Role::User, "hi");
    let asst1 = Message {
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
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_2".into(),
            content: "ok".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    let corrupt = "{ this is not valid json";
    let content = format!(
        "{}\n{}\n{}\n{}\n{}\n",
        serde_json::to_string(&user_msg).unwrap(),
        serde_json::to_string(&asst1).unwrap(),
        corrupt,
        serde_json::to_string(&asst2).unwrap(),
        serde_json::to_string(&res2).unwrap(),
    );
    fs::write(&path, content).unwrap();

    let h = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();
    // 损坏导致 call_1 的 tool_result 缺失：链保留 + 占位补齐（ADR-0004）。
    assert!(
        h.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolUse { id, .. } if id == "call_1"
        ))),
        "call_1 的 assistant(tool_use) 应保留"
    );
    assert!(
        h.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolResult { tool_use_id, content, is_error }
                if tool_use_id == "call_1" && *is_error && content.contains("工具执行被中断")
        ))),
        "call_1 应有合成的 aborted 占位结果"
    );
    // 合法的 call_2 工具链应保留：assistant(call_2) + user(result call_2)。
    assert!(
        h.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolUse { id, .. } if id == "call_2"
        ))),
        "合法的 call_2 工具链应保留"
    );
    assert!(
        h.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call_2"
        ))),
        "call_2 的 tool_result 应保留"
    );
}

/// P2：以独立 `role=tool` 形态存储的工具结果，必须被识别为「工具结果载体」并正确配对——
/// assistant(tool_calls call_1) + role=tool(call_1) 两条都应保留（旧实现只认 user+ToolResult，
/// 会留下孤儿 role=tool 致 400）。
#[test]
fn role_tool_message_pairs_with_preceding_tool_use() {
    let (_g, s) = store();
    let asst = Message {
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
        role: Role::Tool,
        content: vec![ContentBlock::text("result")],
        tool_call_id: Some("call_1".into()),
        ts: 0,
    };
    s.append("cli", "c", &asst).unwrap();
    s.append("cli", "c", &tool).unwrap();
    let h = s.load_history("cli", "c", 10, 1024 * 1024).unwrap();
    assert_eq!(
        h.len(),
        2,
        "配对的 assistant(tool_calls)+role=tool 都应保留"
    );
    assert!(h.iter().any(|m| m.has_tool_calls()));
    assert!(h.iter().any(|m| m.role == Role::Tool));
}

/// 一个 assistant 可以一次发起多个 tool call；OpenAI 兼容历史里通常会跟随多条
/// `role=tool`，每条回答一个 call id。这是合法链，不能只看第一条结果就误删。
#[test]
fn multiple_role_tool_messages_pair_with_one_assistant() {
    let (_g, s) = store();
    let asst = Message {
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
        role: Role::Tool,
        content: vec![ContentBlock::text("result 1")],
        tool_call_id: Some("call_1".into()),
        ts: 0,
    };
    let tool2 = Message {
        role: Role::Tool,
        content: vec![ContentBlock::text("result 2")],
        tool_call_id: Some("call_2".into()),
        ts: 0,
    };
    s.append("cli", "c", &asst).unwrap();
    s.append("cli", "c", &tool1).unwrap();
    s.append("cli", "c", &tool2).unwrap();

    let h = s.load_history("cli", "c", 10, 1024 * 1024).unwrap();
    assert_eq!(h.len(), 3, "assistant + 两条 role=tool 应完整保留");
    assert!(h[0].has_tool_calls());
    assert_eq!(h[1].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(h[2].tool_call_id.as_deref(), Some("call_2"));
}

/// P2：开头孤儿 `role=tool`（无前序 assistant tool_calls）必须被跳过，避免 400。
#[test]
fn orphan_role_tool_at_head_is_skipped() {
    let (_g, s) = store();
    let orphan = Message {
        role: Role::Tool,
        content: vec![ContentBlock::text("r")],
        tool_call_id: Some("call_x".into()),
        ts: 0,
    };
    s.append("cli", "c", &orphan).unwrap();
    s.append("cli", "c", &Message::plain(Role::Assistant, "ans"))
        .unwrap();
    let h = s.load_history("cli", "c", 10, 1024 * 1024).unwrap();
    assert_eq!(h.len(), 1, "开头孤儿 role=tool 应被跳过");
    assert_eq!(h[0].role, Role::Assistant);
}

/// P2：assistant(tool_calls) 后跟的 role=tool 若 id 不匹配，repair 必须删除该 assistant
/// （留下的 role=tool 会在后续扫描中被判为孤儿一并删除），最终历史不含悬空 tool_use。
#[test]
fn mismatched_role_tool_drops_preceding_tool_use() {
    let (_g, s) = store();
    let asst = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read_file".into(),
            input: json!({}),
        }],
        tool_call_id: None,
        ts: 0,
    };
    // role=tool 的配对 id 与 assistant 声明的 call_1 不匹配。
    let tool = Message {
        role: Role::Tool,
        content: vec![ContentBlock::text("r")],
        tool_call_id: Some("call_other".into()),
        ts: 0,
    };
    s.append("cli", "c", &asst).unwrap();
    s.append("cli", "c", &tool).unwrap();
    let h = s.load_history("cli", "c", 10, 1024 * 1024).unwrap();
    assert!(
        !h.iter().any(|m| m.has_tool_calls()),
        "id 不匹配时 assistant(tool_use) 应被删除"
    );
}

/// P2：坏 tool_use（空 name / 非 object input）会让 openai_compat 每一轮构造请求都失败。
/// repair 必须在 load_history 阶段丢弃【整条坏 assistant 工具链及其连续 tool_result】，
/// 让用户能通过继续对话自愈——否则坏数据永远留在历史里、每一轮请求都构造失败。
///
/// 这里同时覆盖「空 name」与「null input」两种坏形态，并验证紧随其后的合法工具链仍完整保留。
#[test]
fn bad_tool_use_drops_whole_chain_and_keeps_valid() {
    let (_g, s) = store();
    let path = s.layout.session_path("cli", "c");

    // 坏链 1：assistant 的 tool_use name 为空（openai_compat 回放会拒绝）。
    let bad_asst_empty_name = Message {
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
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_bad1".into(),
            content: "r".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    // 坏链 2：assistant 的 tool_use input 为 null（非 object，openai_compat 回放会拒绝）。
    let bad_asst_null_input = Message {
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
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_bad2".into(),
            content: "r".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    // 合法链：应完整保留。
    let good_asst = Message {
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
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_good".into(),
            content: "ok".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };

    // 用原始 JSON 直接写入文件，模拟「旧 JSONL 或手工编辑的 session」直接包含坏 tool_use
    // （正常流程不会写入这类坏数据，故绕过 append 直接落盘）。
    let content = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n",
        serde_json::to_string(&bad_asst_empty_name).unwrap(),
        serde_json::to_string(&bad_res1).unwrap(),
        serde_json::to_string(&bad_asst_null_input).unwrap(),
        serde_json::to_string(&bad_res2).unwrap(),
        serde_json::to_string(&good_asst).unwrap(),
        serde_json::to_string(&good_res).unwrap(),
    );
    fs::write(&path, content).unwrap();

    let h = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();

    // 两条坏链（assistant + tool_result）必须全部被丢弃：assistant 不留、其 tool_result
    // 也不留（否则会变成无前序 tool_calls 的孤儿 role=tool，下一轮仍 400）。
    for bad_id in ["call_bad1", "call_bad2"] {
        assert!(
            !h.iter().any(|m| m.content.iter().any(|b| matches!(
                b,
                ContentBlock::ToolUse { id, .. } if id == bad_id
            ))),
            "坏 tool_use（{bad_id}）的 assistant 必须被删除"
        );
        assert!(
            !h.iter().any(|m| m.content.iter().any(|b| matches!(
                b,
                ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == bad_id
            ))),
            "坏 tool_use（{bad_id}）的连续 tool_result 必须随坏链一并删除（不留孤儿）"
        );
    }
    // 合法链应完整保留：assistant(call_good) + user(result call_good)。
    assert_eq!(h.len(), 2, "仅合法工具链应保留");
    assert!(h[0].has_tool_calls(), "合法链的 assistant(tool_use) 应保留");
    assert!(
        h.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call_good"
        ))),
        "合法链的 tool_result 应保留"
    );
}

/// P2：同一条 User 结果消息【内部】携带重复 tool_use_id 的 ToolResult（如两个 call_1）时，
/// repair 必须把整条坏链（assistant + 该结果载体）丢弃——否则 convert_messages 会把它
/// 序列化成两条相同 tool_call_id 的 role=tool 消息，OpenAI 兼容接口通常 400。
/// 旧实现只校验「跨结果重复」（ids 是否已出现在 answered），漏掉了「同条内部重复」。
#[test]
fn duplicate_tool_result_within_one_carrier_drops_chain() {
    let (_g, s) = store();
    let path = s.layout.session_path("cli", "c");

    // assistant 声明 call_1。
    let asst = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read_file".into(),
            input: json!({"path": "x"}),
        }],
        tool_call_id: None,
        ts: 0,
    };
    // 同一条 User 消息里塞了【两个相同 tool_use_id 的 ToolResult】（旧 / 手工编辑
    // session 才会出现这种结构，正常流程不会写入）。
    let dup_carrier = Message {
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
    // 后续合法普通消息：验证坏链被丢弃后历史仍可继续（用户能自愈）。
    let follow = Message::plain(Role::Assistant, "done");

    let content = format!(
        "{}\n{}\n{}\n",
        serde_json::to_string(&asst).unwrap(),
        serde_json::to_string(&dup_carrier).unwrap(),
        serde_json::to_string(&follow).unwrap(),
    );
    fs::write(&path, content).unwrap();

    let h = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();
    // 坏链（assistant + 重复结果载体）必须全部被丢弃。
    assert!(
        !h.iter().any(|m| m.has_tool_calls()),
        "内部重复 tool_result 的 assistant(tool_use) 必须被删除"
    );
    assert!(
        !h.iter().any(|m| m.role == Role::User
            && m.content.iter().any(|b| matches!(
                b,
                ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call_1"
            ))),
        "携带重复 tool_result 的载体必须随坏链一并删除（否则会序列化出重复 tool_call_id 的孤儿）"
    );
    // 后续合法消息保留 → 用户可通过继续对话自愈。
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].content[0].as_text().unwrap(), "done");
}

/// P2：畸形混合形态——`role=tool` 且 `tool_call_id` 为空，但 content 里塞了 ToolResult。
/// 旧实现会从 content 抽出 tool_use_id，误以为 call 已配对而保留整条链；但
/// `convert_messages` 回放 role=tool 时仍因缺 tool_call_id 报错，每一轮请求都失败。
/// repair 必须把这种畸形消息判为「载体却取不到合法 id」→ assistant 判坏链丢弃、畸形载体
/// 作孤儿丢弃，让用户能通过继续对话自愈。
#[test]
fn role_tool_missing_call_id_with_content_tool_result_drops_chain() {
    let (_g, s) = store();
    let path = s.layout.session_path("cli", "c");

    let asst = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read_file".into(),
            input: json!({"path": "x"}),
        }],
        tool_call_id: None,
        ts: 0,
    };
    // 畸形：role=tool，tool_call_id 为 None，但 content 里塞了 ToolResult(call_1)。
    // 旧 / 手工编辑的 session 才会出现这种结构，正常流程不会写入。
    let malformed = Message {
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

    let content = format!(
        "{}\n{}\n{}\n",
        serde_json::to_string(&asst).unwrap(),
        serde_json::to_string(&malformed).unwrap(),
        serde_json::to_string(&follow).unwrap(),
    );
    fs::write(&path, content).unwrap();

    let h = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();
    // 畸形 role=tool 取不到合法 id → assistant(call_1) 判坏链丢弃。
    assert!(
        !h.iter().any(|m| m.has_tool_calls()),
        "缺 tool_call_id 的 role=tool 不得让 assistant(tool_use) 误判为已配对"
    );
    // 畸形载体作为孤儿一并丢弃。
    assert!(
        !h.iter().any(|m| m.role == Role::Tool),
        "畸形 role=tool（无 tool_call_id）应作孤儿丢弃"
    );
    // 后续合法消息保留 → 用户可自愈。
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].content[0].as_text().unwrap(), "done");
}

// ---------------------------------------------------------------------------
// A2（ADR-0004）：本地中断缺失结果 → 合成 aborted 占位保留整链；
// 数据损坏（外来 id 等）→ 维持 fail-loud 整链丢弃。
// ---------------------------------------------------------------------------

fn tool_use_assistant(id: &str) -> Message {
    Message {
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

/// assistant(tool_calls) 之后结果载体整行丢失（崩溃/取消遗留）→ 链保留 + 占位补齐。
#[test]
fn missing_tool_result_synthesizes_aborted_placeholder() {
    let (_g, s) = store();
    let path = s.layout.session_path("cli", "c");
    let asst = tool_use_assistant("call_1");
    let follow = Message::plain(Role::Assistant, "后续消息");
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&asst).unwrap(),
            serde_json::to_string(&follow).unwrap()
        ),
    )
    .unwrap();

    let h = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();
    assert_eq!(
        h.len(),
        3,
        "assistant + 合成占位载体 + 后续消息都应保留: {:?}",
        h.len()
    );
    assert!(h[0].has_tool_calls(), "合法 assistant 不应被丢弃");
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
    assert!(placeholder.2, "占位结果应为 error 语义");
    assert!(
        placeholder.1.contains("工具执行被中断"),
        "占位文案应说明中断: {}",
        placeholder.1
    );
}

/// 多个 call 中只缺一个结果 → 保留已有结果 + 只为缺失 id 合成占位。
#[test]
fn partially_answered_chain_gets_placeholder_for_missing_only() {
    let (_g, s) = store();
    let path = s.layout.session_path("cli", "c");
    let asst = Message {
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
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_a".into(),
            content: "已有结果".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&asst).unwrap(),
            serde_json::to_string(&carrier).unwrap()
        ),
    )
    .unwrap();

    let h = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();
    assert_eq!(h.len(), 3, "assistant + 已有结果载体 + 占位载体");
    let synth = &h[2];
    let ids: Vec<&str> = synth
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["call_b"], "只应为缺失的 call_b 合成占位");
}

/// 数据损坏（外来 id 载体）仍维持 fail-loud：整链丢弃、不合成占位。
#[test]
fn foreign_id_carrier_still_drops_whole_chain() {
    let (_g, s) = store();
    let path = s.layout.session_path("cli", "c");
    let asst = tool_use_assistant("call_1");
    let foreign = Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_other".into(),
            content: "串错的结果".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&asst).unwrap(),
            serde_json::to_string(&foreign).unwrap()
        ),
    )
    .unwrap();

    let h = s.load_history("cli", "c", 100, 1024 * 1024).unwrap();
    assert!(
        !h.iter().any(|m| m.has_tool_calls()),
        "外来 id 载体 = 数据损坏，应维持整链丢弃"
    );
    assert!(
        !h.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolResult { content, .. } if content.contains("工具执行被中断")
        ))),
        "损坏场景不得合成占位"
    );
}

#[test]
fn load_history_view_does_not_compact_but_load_history_does() {
    let (_dir, s) = store();
    // 写入 10 条消息（> max_msgs=4 的 2 倍阈值，触发压缩条件）。
    for i in 0..10 {
        s.append("cli", "c", &Message::plain(Role::User, format!("m{i}")))
            .unwrap();
    }
    let path = s.session_path_pub("cli", "c");
    let lines_before = fs::read_to_string(&path).unwrap().lines().count();

    // 只读视图：裁剪到最近 4 条，但不得写回文件。
    let view = s.load_history_view("cli", "c", 4, 1024 * 1024).unwrap();
    assert_eq!(view.len(), 4);
    assert_eq!(
        fs::read_to_string(&path).unwrap().lines().count(),
        lines_before,
        "load_history_view 不得触发物理压缩写回"
    );

    // 普通 load_history：同样裁剪，且文件被压缩写回。
    let h = s.load_history("cli", "c", 4, 1024 * 1024).unwrap();
    assert_eq!(h.len(), 4);
    assert_eq!(
        fs::read_to_string(&path).unwrap().lines().count(),
        4,
        "load_history 应把膨胀文件压缩回保留的消息数"
    );
}
