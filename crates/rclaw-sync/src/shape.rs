//! 形态快照（P0 验收）。
//!
//! `Message`、`ContentBlock` 和 `message.meta` 的序列化形态是同步协议的一部分：消息原样
//! 装进 op，别的设备、别的版本要读得懂。改了其中任何一个，这里的快照会对不上——更新
//! 快照之前先想清楚：旧版本的设备收到新形态会怎样（它会隔离那个线程，§9.4），需不需要
//! 提升空间格式版本。

use std::collections::BTreeSet;

use rclaw_core::{ContentBlock, MediaKind, Message, Role};
use rclaw_store::session::raw;
use serde_json::json;

use crate::db;
use crate::map;

/// 新增内容块变体时这里编译不过：给它补上快照。
fn variant(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text { .. } => "text",
        ContentBlock::MediaUrl { .. } => "media_url",
        ContentBlock::Reasoning { .. } => "reasoning",
        ContentBlock::ToolUse { .. } => "tool_use",
        ContentBlock::ToolResult { .. } => "tool_result",
    }
}

/// 同上：角色、多模态资源类型。
fn role_name(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn media_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image",
        MediaKind::Video => "video",
        MediaKind::File => "file",
    }
}

#[test]
fn content_block_shapes_are_pinned() {
    let cases = [
        (
            ContentBlock::text("你好"),
            r#"{"type":"text","text":"你好"}"#,
        ),
        (
            ContentBlock::media_url(MediaKind::Image, "data:image/png;base64,AA=="),
            r#"{"type":"media_url","kind":"image","url":"data:image/png;base64,AA=="}"#,
        ),
        (
            ContentBlock::reasoning("先想一想"),
            r#"{"type":"reasoning","reasoning_content":"先想一想"}"#,
        ),
        (
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "read".into(),
                input: json!({ "path": "a.txt" }),
            },
            r#"{"type":"tool_use","id":"call_1","name":"read","input":{"path":"a.txt"}}"#,
        ),
        (
            ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "内容".into(),
                is_error: false,
            },
            r#"{"type":"tool_result","tool_use_id":"call_1","content":"内容"}"#,
        ),
        (
            ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "坏了".into(),
                is_error: true,
            },
            r#"{"type":"tool_result","tool_use_id":"call_1","content":"坏了","is_error":true}"#,
        ),
    ];
    let mut covered = BTreeSet::new();
    for (block, snapshot) in &cases {
        assert_eq!(
            serde_json::to_string(block).unwrap(),
            *snapshot,
            "{}",
            variant(block)
        );
        assert_eq!(
            &serde_json::from_str::<ContentBlock>(snapshot).unwrap(),
            block
        );
        covered.insert(variant(block));
    }
    assert_eq!(
        covered.into_iter().collect::<Vec<_>>(),
        ["media_url", "reasoning", "text", "tool_result", "tool_use"]
    );
    for kind in [MediaKind::Image, MediaKind::Video, MediaKind::File] {
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(media_name(kind)));
    }
    // 不认识的块类型读不进来（合入时所属线程被隔离），不会被悄悄丢掉。
    assert!(serde_json::from_str::<ContentBlock>(r#"{"type":"hologram","data":"x"}"#).is_err());
}

#[test]
fn message_shapes_are_pinned() {
    let full = Message {
        id: 7,
        role: Role::Tool,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: "内容".into(),
            is_error: false,
        }],
        tool_call_id: Some("call_1".into()),
        ts: 1_700_000_000,
    };
    assert_eq!(
        serde_json::to_string(&full).unwrap(),
        r#"{"id":7,"role":"tool","content":[{"type":"tool_result","tool_use_id":"call_1","content":"内容"}],"tool_call_id":"call_1","ts":1700000000}"#
    );
    // 没有的字段不写出来。
    let bare = Message::plain(Role::User, "你好");
    assert_eq!(
        serde_json::to_string(&bare).unwrap(),
        r#"{"role":"user","content":[{"type":"text","text":"你好"}]}"#
    );
    for message in [&full, &bare] {
        let text = serde_json::to_string(message).unwrap();
        assert_eq!(&serde_json::from_str::<Message>(&text).unwrap(), message);
    }
    for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
        assert_eq!(
            serde_json::to_value(&role).unwrap(),
            json!(role_name(&role))
        );
    }
}

#[test]
fn meta_shapes_are_pinned() {
    let (_guard, ctx) = crate::testing::ctx();
    let sessions = &ctx.env.sessions;
    let first = sessions
        .append("desktop", "c1", &Message::plain(Role::User, "一"))
        .unwrap();
    let second = sessions
        .append("desktop", "c1", &Message::plain(Role::Assistant, "二"))
        .unwrap();
    sessions
        .append_compaction(
            "desktop",
            "c1",
            &Message::plain(Role::User, "摘要"),
            Some(first),
            second,
        )
        .unwrap();
    let metas: Vec<String> = db::read(ctx.db(), |conn| {
        let mut stmt =
            conn.prepare("SELECT meta FROM message WHERE meta IS NOT NULL ORDER BY id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .unwrap();
    assert_eq!(metas.len(), 1);
    // 压缩边界：两个锚点都是本机的消息行 id，上线前换成全局 id（§7.2）。
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&metas[0]).unwrap(),
        json!({ "compaction": { "head_until": first, "tail_from": second } })
    );
    assert_eq!(
        raw::compaction_anchors(&metas[0]),
        Some((Some(first), second))
    );

    // 分叉告知行：父会话的键与锚点消息的行 id。
    let notice = raw::fork_notice_meta(Some(("parent-chat", 5)));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&notice).unwrap(),
        json!({ "fork_notice": { "parent": "parent-chat", "anchor": 5 } })
    );
    assert_eq!(
        raw::fork_notice_anchor(&notice),
        Some((Some("parent-chat".to_string()), Some(5)))
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&raw::fork_notice_meta(None)).unwrap(),
        json!({ "fork_notice": { "parent": null, "anchor": null } })
    );

    // 同步只认识这两种形态；别的形态出现时，那个会话从这一行起暂停发布（§9.4）。
    assert!(map::is_known_meta(&metas[0]));
    assert!(map::is_known_meta(&notice));
    assert!(!map::is_known_meta(r#"{"something_new":{"x":1}}"#));
}
