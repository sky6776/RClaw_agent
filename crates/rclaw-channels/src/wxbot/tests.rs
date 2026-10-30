use super::*;

fn cfg() -> WxbotConfig {
    WxbotConfig {
        enabled: true,
        api_base: "http://127.0.0.1:3001".into(),
        api_token: "tok".into(),
        webhook_listen: "127.0.0.1:38081".into(),
        webhook_path: "/wxbot/webhook".into(),
        webhook_secret: "sec".into(),
        allow_from: String::new(),
        allow_groups: String::new(),
        group_reply_policy: "mention".into(),
        poll_fallback_enabled: true,
        poll_timeout_sec: 60,
        retry_sec: 1,
        backoff_sec: 1,
        max_failures: 2,
        send_chunk_bytes: 20,
        send_min_gap_sec: 4,
        send_jitter_sec: 4,
        send_window_max: 10,
        send_failure_fuse: 5,
        media_dir: PathBuf::new(),
    }
}

fn bot() -> Wxbot {
    Wxbot::new(cfg()).expect("合法配置应构造成功")
}

fn event(v: Value) -> CollectorEvent {
    serde_json::from_value(v).expect("事件解析")
}

// ---------------------------------------------------------------------------
// chat_id 账号段：会话隔离 + 发送路径解析
// ---------------------------------------------------------------------------

#[test]
fn chat_id_account_scoping_and_conv_roundtrip() {
    use super::parse_chat_id;
    // 带账号：chat_id 嵌账号段，解析带出账号段 + 纯 talker。
    let ev = event(json!({
        "type": "text", "talker": "wxid_friend", "account": "wxid_self", "content": "hi",
    }));
    assert_eq!(inbound_chat_id(&ev), "user:wxid_self:wxid_friend");
    assert_eq!(
        parse_chat_id("user:wxid_self:wxid_friend"),
        Some((Some("wxid_self"), "wxid_friend"))
    );

    // 群聊同样嵌账号段。
    let ev = event(json!({
        "type": "text", "talker": "123@chatroom", "is_group": true, "account": "wxid_self",
    }));
    assert_eq!(inbound_chat_id(&ev), "group:wxid_self:123@chatroom");
    assert_eq!(
        parse_chat_id("group:wxid_self:123@chatroom"),
        Some((Some("wxid_self"), "123@chatroom"))
    );

    // 账号未知：退化为 legacy 形态（无账号段），出站无法做归属校验但可发送。
    let ev = event(json!({ "type": "text", "talker": "wxid_friend" }));
    assert_eq!(inbound_chat_id(&ev), "user:wxid_friend");
    assert_eq!(parse_chat_id("user:wxid_friend"), Some((None, "wxid_friend")));

    // 非 wxbot 前缀拒绝。
    assert_eq!(parse_chat_id("other:x"), None);
}

#[test]
fn outbound_ownership_rejects_foreign_account_chat_ids() {
    let b = bot();
    {
        let mut st = b.state.lock().unwrap();
        st.bind("wxid_self");
    }
    // chat_id 账号段 = 绑定账号 → 归属校验通过（模拟 send_loop 的判定）。
    let owned = b.state.lock().map(|st| st.account == "wxid_self").unwrap_or(false);
    assert!(owned, "同账号会话应放行");
    // 账号段是别的账号（换号后旧定时任务找上门）→ 拒发。
    let foreign = b.state.lock().map(|st| st.account == "wxid_other").unwrap_or(false);
    assert!(!foreign, "跨账号会话必须拒绝发送");
}

// ---------------------------------------------------------------------------
// 安全前置：回环强制 + 凭据非空
// ---------------------------------------------------------------------------

#[test]
fn new_rejects_empty_credentials() {
    let mut c = cfg();
    c.api_token = String::new();
    assert!(Wxbot::new(c).is_none(), "api_token 为空必须拒绝启动");

    let mut c = cfg();
    c.webhook_secret = "  ".into();
    assert!(Wxbot::new(c).is_none(), "webhook_secret 为空必须拒绝启动");
}

#[test]
fn new_rejects_non_loopback_addresses() {
    let mut c = cfg();
    c.api_base = "http://192.168.1.5:3001".into();
    assert!(Wxbot::new(c).is_none(), "api_base 非回环必须拒绝启动");

    let mut c = cfg();
    c.webhook_listen = "0.0.0.0:38081".into();
    assert!(Wxbot::new(c).is_none(), "webhook_listen 非回环必须拒绝启动");

    let mut c = cfg();
    c.enabled = false;
    assert!(Wxbot::new(c).is_none(), "未启用直接不构造");
}

#[test]
fn loopback_detection() {
    assert!(is_loopback_url("http://127.0.0.1:3001"));
    assert!(is_loopback_url("http://localhost:3001/"));
    assert!(is_loopback_url("http://[::1]:3001"));
    assert!(!is_loopback_url("http://192.168.1.2:3001"));
    assert!(!is_loopback_url("http://evil.com/api"));
    assert!(!is_loopback_url("not a url"));

    assert!(is_loopback_listen("127.0.0.1:38081"));
    assert!(is_loopback_listen("localhost:38081"));
    assert!(is_loopback_listen("[::1]:38081"));
    assert!(!is_loopback_listen("192.168.1.2:38081"));
    assert!(!is_loopback_listen("0.0.0.0:38081"));
}

// ---------------------------------------------------------------------------
// 白名单 / 群触发
// ---------------------------------------------------------------------------

#[test]
fn private_allow_empty_means_allow_all() {
    let b = bot();
    let ev = event(json!({ "talker": "wxid_stranger", "type": "text" }));
    assert!(b.allowed(&ev), "allow_from 为空 = 放行全部私聊");

    let mut c = cfg();
    c.allow_from = "wxid_a, wxid_b".into();
    let b = Wxbot::new(c).unwrap();
    assert!(b.allowed(&event(json!({ "talker": "wxid_a", "type": "text" }))));
    assert!(!b.allowed(&event(json!({ "talker": "wxid_c", "type": "text" }))));
}

#[test]
fn group_requires_allowlist_and_mention() {
    let mut c = cfg();
    c.allow_groups = "123@chatroom".into();
    c.group_reply_policy = "all".into();
    let b = Wxbot::new(c).unwrap();
    assert!(
        b.allowed(&event(json!({ "talker": "123@chatroom", "is_group": true, "type": "text" }))),
        "群在白名单 + all 策略 → 放行"
    );
    assert!(
        !b.allowed(&event(json!({ "talker": "456@chatroom", "is_group": true, "type": "text" }))),
        "群不在白名单 → 拒绝"
    );

    // 默认 mention 策略：群内也要精确 @ 绑定账号。
    let b2 = bot();
    {
        let mut st = b2.state.lock().unwrap();
        st.bind("wxid_self");
    }
    assert!(
        !b2.allowed(&event(json!({
            "talker": "123@chatroom", "is_group": true, "type": "text",
        }))),
        "allow_groups 为空 = 所有群拒绝（即使被 @）"
    );

    let mut c3 = cfg();
    c3.allow_groups = "123@chatroom".into();
    let b3 = Wxbot::new(c3).unwrap();
    {
        let mut st = b3.state.lock().unwrap();
        st.bind("wxid_self");
    }
    // 触发判定在 group_reaches_llm（allowed 只做白名单门槛）。
    assert!(
        !b3.group_reaches_llm(&event(json!({
            "talker": "123@chatroom", "is_group": true, "type": "text",
            "at_users": ["wxid_other"],
        }))),
        "mention 策略：@ 他人不触发"
    );
    assert!(
        b3.group_reaches_llm(&event(json!({
            "talker": "123@chatroom", "is_group": true, "type": "text",
            "at_users": ["wxid_self"],
        }))),
        "mention 策略：精确 @ 本账号触发"
    );
    assert!(
        !b3.group_reaches_llm(&event(json!({
            "talker": "123@chatroom", "is_group": true, "type": "text",
            "content": "加我 a@b.com", "at_users": [],
        }))),
        "mention 策略：无 at 元数据（含邮箱样式文本）不触发"
    );

    // all / none 策略。
    let mut c4 = cfg();
    c4.allow_groups = "123@chatroom".into();
    c4.group_reply_policy = "all".into();
    let b4 = Wxbot::new(c4).unwrap();
    assert!(
        b4.group_reaches_llm(&event(json!({
            "talker": "123@chatroom", "is_group": true, "type": "text",
        }))),
        "all 策略：群消息全部投递"
    );
    let mut c5 = cfg();
    c5.allow_groups = "123@chatroom".into();
    c5.group_reply_policy = "none".into();
    let b5 = Wxbot::new(c5).unwrap();
    assert!(
        !b5.group_reaches_llm(&event(json!({
            "talker": "123@chatroom", "is_group": true, "type": "text",
            "at_users": ["wxid_self"],
        }))),
        "none 策略：即使被 @ 也不投递（全部静默接收归档）"
    );
}

// ---------------------------------------------------------------------------
// 去重账本（容量 / 驱逐 / 提交）
// ---------------------------------------------------------------------------

#[test]
fn dedup_ledger_commit_and_contains() {
    let mut st = WxbotState::default();
    assert!(!st.contains("k1"));
    st.commit("k1".into());
    assert!(st.contains("k1"));
    st.commit("k1".into()); // 重复提交无害
    assert_eq!(st.order.len(), 1);
    st.commit(String::new()); // 空 key 不入账
    assert_eq!(st.order.len(), 1);
}

#[test]
fn dedup_ledger_evicts_oldest_half() {
    let mut st = WxbotState::default();
    let total = SEEN_CAP + 10;
    for i in 0..total {
        st.commit(format!("k{i}"));
    }
    // 首次溢出（提交 k8192 时 len=8193）驱逐最旧一半：k0..k4095 出局。
    assert!(!st.contains("k0"), "最旧的键应被驱逐");
    assert!(!st.contains("k4095"), "驱逐边界（含）之前的键应被驱逐");
    assert!(st.contains("k4096"), "驱逐边界之后的键必须保留");
    assert!(st.contains(&format!("k{}", total - 1)), "最新键必须保留");
}

#[test]
fn effective_key_passthrough_and_fallback() {
    let ev = event(json!({ "dedup_key": "s:acc:123" }));
    assert_eq!(effective_key(&ev), "s:acc:123");

    let a = event(json!({ "talker": "t", "create_time": 5, "seq": 1, "raw_content": "hi" }));
    let b = event(json!({ "talker": "t", "create_time": 5, "seq": 1, "raw_content": "hi" }));
    assert_eq!(effective_key(&a), effective_key(&b), "相同事件退化键必须一致");
    assert!(!effective_key(&a).is_empty());

    let c = event(json!({ "talker": "t", "create_time": 5, "seq": 2, "raw_content": "hi" }));
    assert_ne!(effective_key(&a), effective_key(&c), "不同事件退化键必须不同");
}

#[test]
fn account_mismatch_latches_send_gate() {
    let mut st = WxbotState::default();
    st.bind("wxid_a");
    assert!(st.account_ok("wxid_a"));
    assert!(!st.account_locked, "未检测到换号前不锁停");

    // 不带 account 的事件不触发锁停。
    assert!(st.account_ok(""));

    // 换号：一次不匹配即锁停，且此后连同账号与空账号事件全部拒绝。
    assert!(!st.account_ok("wxid_b"), "换号事件必须被拒绝");
    assert!(st.account_locked, "锁停后禁止一切发送（发送闸门读此字段）");
    assert!(!st.account_ok("wxid_a"), "锁停后原账号事件同样拒绝");
    assert!(!st.account_ok(""), "锁停后空账号事件同样拒绝");
}

// ---------------------------------------------------------------------------
// 事件解析（serde 契约）
// ---------------------------------------------------------------------------

#[test]
fn event_parses_with_renames_and_defaults() {
    let ev = event(json!({
        "event": "message", "seq": 7, "dedup_key": "s:a:1",
        "account": "acc", "type": "image", "type_code": 3,
        "msg_svr_id": 999, "talker": "wxid_x", "sender": "wxid_x",
        "is_group": false, "content": "正文",
        "at_users": ["wxid_self"], "create_time": 1730000000,
    }));
    assert_eq!(ev.kind, "image");
    assert_eq!(ev.svr_id, 999);
    assert_eq!(ev.seq, 7);
    assert!(ev.title.is_none());
    assert!(ev.appmsg_type.is_none());

    let minimal = event(json!({}));
    assert_eq!(minimal.kind, "");
    assert_eq!(minimal.svr_id, 0);
    assert!(!minimal.is_group);
    assert!(minimal.at_users.is_empty());
    assert_eq!(minimal.event, "");
}

// ---------------------------------------------------------------------------
// 正文组装（标注 / 降级）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn build_content_text_and_group_sender() {
    let b = bot();
    let ev = event(json!({ "type": "text", "content": "你好" }));
    let mut notes = Vec::new();
    let out = b.build_content(&ev, &mut notes).await.unwrap();
    assert_eq!(out, "你好");

    let ev = event(json!({
        "type": "text", "content": "大家好", "is_group": true, "sender": "wxid_p",
    }));
    let mut notes = Vec::new();
    let out = b.build_content(&ev, &mut notes).await.unwrap();
    assert!(out.contains("[来自 wxid_p]"));
    assert!(out.contains("大家好"));

    let ev = event(json!({ "type": "text", "content": "  " }));
    assert!(b.build_content(&ev, &mut Vec::new()).await.is_none(), "空正文不投递");
}

#[tokio::test]
async fn build_content_media_degrades_without_media_dir() {
    let b = bot(); // media_dir 为空
    let ev = event(json!({ "type": "image", "msg_svr_id": 5, "content": "" }));
    let mut notes = Vec::new();
    let out = b.build_content(&ev, &mut notes).await.unwrap();
    assert!(out.contains("[图片]"), "无媒体目录时降级为纯标注: {out}");
    assert!(out.contains("仅通知"));
}

#[tokio::test]
async fn build_content_media_rejects_zero_svr_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut c = cfg();
    c.media_dir = dir.path().to_path_buf();
    let b = Wxbot::new(c).expect("合法配置应构造成功");
    let ev = event(json!({ "type": "image", "msg_svr_id": 0, "content": "" }));
    let mut notes = Vec::new();
    let out = b.build_content(&ev, &mut notes).await.unwrap();
    assert!(out.contains("无服务端 id"), "零 svr id 无法拉媒体: {out}");
}

#[tokio::test]
async fn build_content_annotation_kinds() {
    let b = bot();
    let link = event(json!({
        "type": "link", "title": "新闻", "url": "https://x.com/a",
        "des": "摘要", "content": "",
    }));
    let mut notes = Vec::new();
    let out = b.build_content(&link, &mut notes).await.unwrap();
    assert!(out.contains("[链接]"));
    assert!(out.contains("https://x.com/a"));
    assert!(out.contains("摘要"));

    let red = event(json!({ "type": "red_packet", "content": "恭喜发财" }));
    let mut notes = Vec::new();
    let out = b.build_content(&red, &mut notes).await.unwrap();
    assert!(out.contains("[红包]"));
    assert!(out.contains("恭喜发财"));

    let unknown = event(json!({ "type": "other", "type_code": 123456, "raw_content": "神秘消息" }));
    let mut notes = Vec::new();
    let out = b.build_content(&unknown, &mut notes).await.unwrap();
    assert!(out.contains("type_code=123456"));
    assert!(out.contains("神秘消息"));

    let video = event(json!({ "type": "video", "title": "片名" }));
    let mut notes = Vec::new();
    let out = b.build_content(&video, &mut notes).await.unwrap();
    assert!(out.contains("[视频]"));
    assert!(out.contains("片名"));
}

// ---------------------------------------------------------------------------
// 出站 marker 解析与分段
// ---------------------------------------------------------------------------

#[test]
fn parse_markers_with_name_and_caption() {
    let (intents, text) = parse_media_markers(
        "前置说明\n[:wxbot-media type=file path=\"agent/media/out/x.pdf\" name=\"报告.pdf\" caption=\"报价\"]\n尾注",
    );
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].kind, MediaKind::File);
    assert_eq!(intents[0].path, "agent/media/out/x.pdf");
    assert_eq!(intents[0].name.as_deref(), Some("报告.pdf"));
    assert_eq!(intents[0].caption.as_deref(), Some("报价"));
    assert_eq!(text, "前置说明\n尾注");
}

#[test]
fn parse_markers_rejects_forged_prefix_and_bad_intent() {
    let (intents, text) = parse_media_markers("[:wxbot-media-forged type=file path=/etc/passwd]");
    assert!(intents.is_empty(), "伪造前缀不得成为发送意图");
    assert!(text.contains("wxbot-media-forged"), "解析失败的行保留为可见文本");

    let (intents, text) = parse_media_markers("[:wxbot-media type=file]"); // 缺 path
    assert!(intents.is_empty());
    assert!(text.contains("type=file"));
}

#[test]
fn utf8_chunks_safe_and_terminating() {
    let s = "你好世界，这是一段用来测试中文字符切分的文本".repeat(3);
    for chunk in utf8_chunks(&s, 20) {
        assert!(chunk.len() <= 20);
    }
    assert_eq!(utf8_chunks("abc", 0), vec!["abc"]);
    let two = utf8_chunks("你好", 1);
    assert_eq!(two.len(), 2);
    assert_eq!(two.concat(), "你好");
}

// ---------------------------------------------------------------------------
// 辅助函数
// ---------------------------------------------------------------------------

#[test]
fn helpers_basics() {
    assert_eq!(truncate("abcdef", 3), "abc…");
    assert_eq!(truncate("abc", 5), "abc");
    assert_eq!(plain_label("[红包]", "", "恭喜"), "[红包] 恭喜");
    assert_eq!(plain_label("[视频]", "片名", ""), "[视频] 片名");
    assert_eq!(ext_for_media("voice", None), "mp3");
    assert_eq!(ext_for_media("sticker", None), "gif");
    assert_eq!(ext_for_media("file", Some("报告.PDF")), "pdf");
    assert_eq!(ext_for_media("file", None), "bin");
}

#[test]
fn capabilities_declare_media_only() {
    let b = bot();
    let caps = b.capabilities();
    assert_eq!(caps.name, "wxbot");
    assert!(caps.media);
    assert!(!caps.working_status);
    assert!(!caps.intermediate_output);
    assert!(!caps.reasoning_stream);
}

#[test]
fn webhook_path_normalization() {
    let mut c = cfg();
    c.webhook_path = "wxbot/webhook".into();
    let b = Wxbot::new(c).unwrap();
    assert_eq!(b.cfg.webhook_path, "/wxbot/webhook");
}

// ---------------------------------------------------------------------------
// SQLite 持久化（私聊/群聊全量落盘 + 静默归档 + 引用回填）
// ---------------------------------------------------------------------------

/// 带持久化库的通道（tempdir media_dir）。
fn bot_with_db() -> (Wxbot, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut c = cfg();
    c.media_dir = dir.path().to_path_buf();
    c.allow_groups = "123@chatroom".into();
    let b = Wxbot::new(c).expect("合法配置应构造成功");
    (b, dir)
}

#[tokio::test]
async fn db_persists_private_and_group_messages() {
    let (b, dir) = bot_with_db();
    // 私聊：正常投递路径，archived=0。
    let ev = event(json!({
        "event": "message", "seq": 1, "dedup_key": "k1", "msg_svr_id": 101,
        "type": "text", "talker": "wxid_friend", "content": "私聊你好",
    }));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<BusMessage>(8);
    b.ingest_one(ev, &tx).await;
    let msg = rx.try_recv().expect("私聊消息应投递");
    assert_eq!(msg.chat_id, "user:wxid_friend");
    assert_eq!(b.db.as_ref().unwrap().lock().unwrap()
        .query_row("SELECT archived FROM inbound_messages WHERE dedup_key = 'k1'", [], |r| r.get::<_, i64>(0))
        .unwrap(), 0, "私聊投递消息 archived=0");

    // 群聊 mention 未命中：静默接收，archived=1，且不投递。
    {
        let mut st = b.state.lock().unwrap();
        st.bind("wxid_self");
    }
    let ev = event(json!({
        "event": "message", "seq": 2, "dedup_key": "k2", "msg_svr_id": 202,
        "type": "text", "talker": "123@chatroom", "is_group": true, "sender": "wxid_p",
        "content": "普通群聊灌水", "at_users": [],
    }));
    b.ingest_one(ev, &tx).await;
    assert!(rx.try_recv().is_err(), "mention 未命中的群消息不得投递");
    assert_eq!(b.db.as_ref().unwrap().lock().unwrap()
        .query_row("SELECT archived FROM inbound_messages WHERE dedup_key = 'k2'", [], |r| r.get::<_, i64>(0))
        .unwrap(), 1, "静默接收的群消息 archived=1");
    // 静默接收完成后同样提交去重账本：双路（webhook/wait）重复事件不再重复处理。
    assert!(
        b.state.lock().unwrap().contains("k2"),
        "静默群消息处理完必须提交账本"
    );
    drop(dir);
}

#[tokio::test]
async fn quote_backfill_from_db_private_and_group() {
    let (b, _dir) = bot_with_db();
    // 先落一条群文本消息（account 必填：引用回填按账号隔离点查）。
    let ev = event(json!({
        "event": "message", "seq": 1, "dedup_key": "q1", "msg_svr_id": 9001,
        "account": "wxid_acc",
        "type": "text", "talker": "123@chatroom", "is_group": true, "sender": "wxid_p",
        "content": "被引用的原始发言",
    }));
    let (tx, _rx) = tokio::sync::mpsc::channel::<BusMessage>(8);
    b.ingest_one(ev, &tx).await;

    // 再来一条引用它的消息（mention 命中，触发回填）。
    let quote = event(json!({
        "event": "message", "seq": 2, "dedup_key": "q2", "msg_svr_id": 9002,
        "account": "wxid_acc",
        "type": "quote", "title": "wxid_p: 被引用的原始发言",
        "talker": "123@chatroom", "is_group": true, "sender": "wxid_self",
        "content": "回复内容", "quote_svrid": 9001, "at_users": ["wxid_self"],
    }));
    let mut notes = Vec::new();
    let out = b.build_content(&quote, &mut notes).await.unwrap();
    assert!(out.contains("[引用]"), "引用标注应在: {out}");
    assert!(out.contains("[引用消息] 被引用的原始发言"), "应从库回填被引用正文: {out}");

    // 私聊同样回填。
    let priv0 = event(json!({
        "event": "message", "seq": 3, "dedup_key": "q3", "msg_svr_id": 9003,
        "account": "wxid_acc",
        "type": "text", "talker": "wxid_friend", "content": "私聊历史",
    }));
    b.ingest_one(priv0, &tx).await;
    let priv_quote = event(json!({
        "event": "message", "seq": 4, "dedup_key": "q4", "msg_svr_id": 9004,
        "account": "wxid_acc",
        "type": "quote", "talker": "wxid_friend", "content": "回复",
        "quote_svrid": 9003,
    }));
    let out = b.build_content(&priv_quote, &mut notes).await.unwrap();
    assert!(out.contains("[引用消息] 私聊历史"), "私聊引用也应回填: {out}");
}

#[tokio::test]
async fn quote_backfill_is_account_scoped() {
    let (b, _dir) = bot_with_db();
    // 账号 A 落一条群消息。
    let ev = event(json!({
        "event": "message", "seq": 1, "dedup_key": "acc1", "msg_svr_id": 9101,
        "account": "wxid_a",
        "type": "text", "talker": "123@chatroom", "is_group": true, "sender": "wxid_p",
        "content": "A 账号时期的发言",
    }));
    let (tx, _rx) = tokio::sync::mpsc::channel::<BusMessage>(8);
    b.ingest_one(ev, &tx).await;
    let mut notes = Vec::new();

    // 账号 B 在同一群引用同 svr_id：不得回填 A 的历史（换号隔离）。
    let quote_b = event(json!({
        "event": "message", "seq": 2, "dedup_key": "acc2", "msg_svr_id": 9102,
        "account": "wxid_b",
        "type": "quote", "talker": "123@chatroom", "is_group": true, "sender": "wxid_self",
        "content": "B 的回复", "quote_svrid": 9101,
    }));
    let out = b.build_content(&quote_b, &mut notes).await.unwrap();
    assert!(!out.contains("A 账号时期的发言"), "跨账号不得回填: {out}");

    // 账号未知（空 account）同样禁用回填。
    let quote_unknown = event(json!({
        "event": "message", "seq": 3, "dedup_key": "acc3", "msg_svr_id": 9103,
        "type": "quote", "talker": "123@chatroom", "is_group": true, "sender": "wxid_self",
        "content": "未知账号的回复", "quote_svrid": 9101,
    }));
    let out = b.build_content(&quote_unknown, &mut notes).await.unwrap();
    assert!(!out.contains("A 账号时期的发言"), "账号未知不得回填: {out}");

    // 同账号仍然回填。
    let quote_a = event(json!({
        "event": "message", "seq": 4, "dedup_key": "acc4", "msg_svr_id": 9104,
        "account": "wxid_a",
        "type": "quote", "talker": "123@chatroom", "is_group": true, "sender": "wxid_self",
        "content": "A 的回复", "quote_svrid": 9101,
    }));
    let out = b.build_content(&quote_a, &mut notes).await.unwrap();
    assert!(out.contains("A 账号时期的发言"), "同账号应回填: {out}");
}

#[tokio::test]
async fn db_record_is_idempotent_on_duplicate_dedup_key() {
    let (b, _dir) = bot_with_db();
    let mk = |seq: u64| event(json!({
        "event": "message", "seq": seq, "dedup_key": "dup", "msg_svr_id": 77,
        "type": "text", "talker": "wxid_friend", "content": "同一条",
    }));
    let (tx, _rx) = tokio::sync::mpsc::channel::<BusMessage>(8);
    b.ingest_one(mk(1), &tx).await;
    b.ingest_one(mk(2), &tx).await; // 同 dedup_key 双投（webhook+wait）
    let n: i64 = b.db.as_ref().unwrap().lock().unwrap()
        .query_row("SELECT COUNT(*) FROM inbound_messages WHERE dedup_key = 'dup'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "dedup_key 唯一约束保证只落一行");
}

#[test]
fn db_disabled_when_media_dir_empty() {
    let b = bot(); // media_dir 为空
    assert!(b.db.is_none(), "未配置媒体目录时持久化降级（None）");
    let ev = event(json!({ "type": "text", "talker": "t", "content": "x" }));
    // 不 panic：降级路径下落库/回填都是 no-op。
    b.db_record(&ev, "k", &[], false, "x");
    assert!(b.db_quote_lookup("", "user:t", 1).is_none());
}

#[test]
fn message_db_migrates_v1_to_v2_account_column() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("wxbot").join("message_index.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).expect("mkdir");
    // 手工造一个 v1 库（无 account 列 + 旧索引 + 一行旧数据）。
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE inbound_messages (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                dedup_key   TEXT    NOT NULL UNIQUE,
                chat_id     TEXT    NOT NULL,
                is_group    INTEGER NOT NULL DEFAULT 0,
                sender      TEXT    NOT NULL DEFAULT '',
                svr_id      INTEGER NOT NULL DEFAULT 0,
                kind        TEXT    NOT NULL DEFAULT '',
                text        TEXT    NOT NULL DEFAULT '',
                media_json  TEXT    NOT NULL DEFAULT '[]',
                archived    INTEGER NOT NULL DEFAULT 0,
                create_time INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX idx_inbound_chat_svr ON inbound_messages(chat_id, svr_id);
            INSERT INTO inbound_messages (dedup_key, chat_id, svr_id, text)
                VALUES ('old', 'user:x', 1, 'v1 遗留');",
        )
        .unwrap();
    }
    // 重新打开：应迁移到 v2（account 列出现、旧索引弃置、v1 数据弃置）。
    let (b, _dir_guard) = {
        let mut c = cfg();
        c.media_dir = dir.path().to_path_buf();
        let b = Wxbot::new(c).expect("构造成功");
        (b, dir)
    };
    let conn = b.db.as_ref().expect("库应打开成功").lock().unwrap();
    let has_account: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('inbound_messages') WHERE name = 'account'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_account, 1, "v2 表应有 account 列");
    let old_index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_inbound_chat_svr'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(old_index, 0, "v1 旧索引应已弃置");
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM inbound_messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0, "v1 遗留数据随迁移弃置（随消息流自然重建）");
}

#[test]
fn sanitize_filename_caps_bytes_on_utf8_boundary() {
    // 240 字节的合法长标题（80 个三字节汉字）→ stem 截到预算内、不切半个字符。
    let long = "文".repeat(80);
    let out = super::media::sanitize_filename(&long);
    assert!(out.len() <= 100, "stem 应限字节: {}", out.len());
    assert!(std::str::from_utf8(out.as_bytes()).is_ok(), "必须仍是合法 UTF-8");
    // ASCII 长名同样受限。
    let ascii = "a".repeat(300);
    assert_eq!(super::media::sanitize_filename(&ascii).len(), 100);
    // 常规名字不受影响。
    assert_eq!(super::media::sanitize_filename("报告.pdf"), "报告.pdf");
}

#[test]
fn sniff_image_ext_detects_real_format() {
    // PNG 魔数 → png（wcx 原样透传时固定 .jpg 会让 MIME 与内容不符）。
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0u8; 16]);
    assert_eq!(super::media::sniff_image_ext(&png), Some("png"));
    // GIF89a / GIF87a → gif。
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&[0u8; 16]);
    assert_eq!(super::media::sniff_image_ext(&gif), Some("gif"));
    // RIFF/WEBP → webp。
    let mut webp = b"RIFF".to_vec();
    webp.extend_from_slice(&[0u8; 4]);
    webp.extend_from_slice(b"WEBP");
    webp.extend_from_slice(&[0u8; 8]);
    assert_eq!(super::media::sniff_image_ext(&webp), Some("webp"));
    // JPEG → jpg。
    let mut jpg = b"\xff\xd8\xff\xe0".to_vec();
    jpg.extend_from_slice(&[0u8; 16]);
    assert_eq!(super::media::sniff_image_ext(&jpg), Some("jpg"));
    // 识别不了 → None（调用方沿用类型推断的扩展名）。
    assert_eq!(super::media::sniff_image_ext(b"not an image at all"), None);
    assert_eq!(super::media::sniff_image_ext(b""), None);
}

// ---------------------------------------------------------------------------
// 风控提示守卫（21cefab）
// ---------------------------------------------------------------------------

#[test]
fn risk_notice_matches_normalized_system_event() {
    // wcx 归一化 type=system；原始 content 不带 wxbot 展示层补的 `[系统]` 前缀。
    let ev = event(json!({
        "type": "system",
        "type_code": 10000,
        "talker": "wxid_security",
        "is_group": false,
        "content": "你的账号被限制与对方聊天，轻触 https://weixin110.qq.com/security/acct/newreadtemplate?t=unban/index 了解详情",
    }));
    assert!(Wxbot::is_risk_notice(&ev));
}

#[test]
fn risk_notice_checks_raw_content_and_url_fields() {
    // 正文为空但 raw_content / url 带链接（不同 wcx 版本字段落点不同）。
    let raw = event(json!({
        "type": "system", "raw_content": "see https://weixin110.qq.com/security/acct",
    }));
    assert!(Wxbot::is_risk_notice(&raw));

    let url = event(json!({
        "type": "system", "url": "https://WEIXIN110.qq.com/security/acct",
    }));
    assert!(Wxbot::is_risk_notice(&url), "URL 匹配应大小写不敏感");
}

#[test]
fn risk_notice_does_not_match_user_text_or_lookalike_hosts() {
    // 用户正文（type=text）即使带 weixin110 链接也不能被吞——否则是可控的消息屏蔽面。
    let text = event(json!({
        "type": "text", "content": "https://weixin110.qq.com/security/acct",
    }));
    assert!(!Wxbot::is_risk_notice(&text));

    // 群系统通知同样要拦：group_reply_policy=all 时它会进入 LLM 并触发同类循环。
    let group = event(json!({
        "type": "system", "is_group": true,
        "content": "https://weixin110.qq.com/security/acct",
    }));
    assert!(Wxbot::is_risk_notice(&group));

    // 只认 http(s) URL 的 host：notweixin110 / 子域名 / 后缀拼接 / 裸昵称都不能命中。
    let lookalike = event(json!({
        "type": "system", "content": "https://notweixin110.qq.com/security",
    }));
    assert!(!Wxbot::is_risk_notice(&lookalike));
    let subdomain = event(json!({
        "type": "system", "content": "https://evil.weixin110.qq.com/security",
    }));
    assert!(!Wxbot::is_risk_notice(&subdomain));
    let suffix = event(json!({
        "type": "system", "content": "https://weixin110.qq.com.evil.com/security",
    }));
    assert!(!Wxbot::is_risk_notice(&suffix));
    let nickname = event(json!({
        "type": "system", "content": "好友 weixin110.qq.com 申请加你为好友",
    }));
    assert!(
        !Wxbot::is_risk_notice(&nickname),
        "裸字符串/昵称不是 URL host"
    );
    let bare_url = event(json!({
        "type": "system", "content": "访问 https://weixin110.qq.com 了解详情",
    }));
    assert!(Wxbot::is_risk_notice(&bare_url));
    let nested = event(json!({
        "type": "system",
        "content": "https://example.com/r?next=https://weixin110.qq.com/security",
    }));
    assert!(!Wxbot::is_risk_notice(&nested), "外层 URL 的嵌套目标不算");

    // system 但不含 weixin110 链接（普通系统提示）仍应正常投递。
    let benign = event(json!({
        "type": "system", "content": "[系统] 你已加入群聊",
    }));
    assert!(!Wxbot::is_risk_notice(&benign));
}

#[test]
fn risk_notice_matches_legacy_prefixed_content() {
    // 兼容旧版本/手工测试把 `[系统]` 写进原始正文的情况。
    let ev = event(json!({
        "type": "system",
        "content": "[系统] 你的账号被限制与对方聊天 https://weixin110.qq.com/security/acct",
    }));
    assert!(Wxbot::is_risk_notice(&ev));
}

// ---------------------------------------------------------------------------
// wait 空体/无 head 容错（21cefab）
// ---------------------------------------------------------------------------

#[test]
fn wait_body_empty_and_non_json_are_idle() {
    assert!(parse_wait_body("").is_none());
    assert!(parse_wait_body(" \r\n\t").is_none());
    assert!(parse_wait_body("<html>502 Bad Gateway</html>").is_none());

    let idle = parse_wait_body(r#"{"event":null,"head_seq":7}"#).expect("合法 JSON");
    assert_eq!(wait_idle_head(&idle), Some(7));
    // 缺 event 但带 head_seq 也必须消费起点，避免无 cursor 永远重试。
    let head_only = parse_wait_body(r#"{"head_seq":42}"#).expect("合法 JSON");
    assert_eq!(wait_idle_head(&head_only), Some(42));
    assert_eq!(wait_idle_head(&json!({})), None);
}

#[test]
fn idle_gets_retry_delay_and_zero_is_clamped() {
    // 空体/非 JSON（以及只有 head_seq 的异常即时 200）都必须按 retry_sec 重试；
    // 0 也要钳到 1s，否则 loopback 请求会热循环（21cefab 要修的就是这个）。
    assert_eq!(idle_retry_delay(0), Duration::from_secs(1));
    assert_eq!(idle_retry_delay(7), Duration::from_secs(7));
}

// ---------------------------------------------------------------------------
// 出站限速器（SendPacer）
// ---------------------------------------------------------------------------

#[test]
fn pacer_first_send_no_wait_then_min_gap_plus_jitter() {
    let mut p = SendPacer::new(&cfg());
    let t0 = Instant::now();
    assert!(p.next_delay(t0, 0).is_zero(), "首条不等待");

    p.record(t0);
    // 已过 1s；基础间隔 4s + 抖动采样 2s → 还差 5s。
    assert_eq!(
        p.next_delay(t0 + Duration::from_secs(1), 2),
        Duration::from_secs(5)
    );
    // 抖动采样被 clamp 到 jitter 上限（4s）：4+4-1=7s。
    assert_eq!(
        p.next_delay(t0 + Duration::from_secs(1), 99),
        Duration::from_secs(7)
    );
}

#[test]
fn pacer_window_waits_until_oldest_leaves_60s() {
    let mut c = cfg();
    c.send_min_gap_sec = 0;
    c.send_jitter_sec = 0;
    c.send_window_max = 2;
    let mut p = SendPacer::new(&c);
    let t0 = Instant::now();
    p.record(t0);
    p.record(t0 + Duration::from_secs(1));

    // 窗口已满 2 条；t0+2s 时最早一条还要 58s 才出窗。
    assert_eq!(
        p.next_delay(t0 + Duration::from_secs(2), 0),
        Duration::from_secs(58)
    );

    // 窗口关闭（0）时不受限制。
    c.send_window_max = 0;
    let mut p = SendPacer::new(&c);
    p.record(t0);
    p.record(t0 + Duration::from_secs(1));
    assert!(p.next_delay(t0 + Duration::from_secs(2), 0).is_zero());
}

#[test]
fn pacer_failure_fuse_counts_only_terminal_failures_and_resets_on_success() {
    let mut c = cfg();
    c.send_failure_fuse = 2;
    let mut p = SendPacer::new(&c);
    assert!(!p.note_failure(), "第 1 次未达阈值");
    assert!(p.note_failure(), "第 2 次达到阈值");
    p.note_success();
    assert!(!p.note_failure(), "成功清零后重新计数");

    c.send_failure_fuse = 0;
    let mut p = SendPacer::new(&c);
    for _ in 0..10 {
        assert!(!p.note_failure(), "0 = 关闭熔断");
    }
}

#[test]
fn pacer_clamps_huge_gap_config_and_never_panics() {
    let mut c = cfg();
    c.send_min_gap_sec = u64::MAX;
    c.send_jitter_sec = u64::MAX;
    let mut p = SendPacer::new(&c);
    assert_eq!(p.min_gap_sec, PACER_MAX_GAP_SEC);
    assert_eq!(p.jitter_sec, PACER_MAX_GAP_SEC);
    let t0 = Instant::now();
    p.record(t0);
    // 最大 2*3600s，Duration/Instant 不溢出。
    assert!(p.next_delay(t0, u64::MAX) <= Duration::from_secs(2 * PACER_MAX_GAP_SEC));
}

#[test]
fn fuse_helper_locks_account_at_threshold() {
    let b = bot();
    b.state.lock().unwrap().bind("wxid_self");
    let mut c = cfg();
    c.send_failure_fuse = 1;
    let mut p = SendPacer::new(&c);
    b.record_send_failure(&mut p);
    assert!(b.state.lock().unwrap().account_locked, "熔断应锁停账号");
}

#[tokio::test]
async fn post_json_ok_rechecks_gate_after_pacer_sleep() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn hit(axum::extract::State(n): axum::extract::State<Arc<AtomicUsize>>) -> &'static str {
        n.fetch_add(1, Ordering::SeqCst);
        "ok"
    }

    let hits = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route("/api/messages/text", axum::routing::post(hit))
        .with_state(hits.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let mut c = cfg();
    c.api_base = format!("http://{addr}");
    c.send_min_gap_sec = 1;
    c.send_jitter_sec = 0;
    c.send_window_max = 0;
    let b = Wxbot::new(c).expect("mock 配置合法");
    b.state.lock().unwrap().bind("wxid_self");
    let mut pacer = SendPacer::new(&b.cfg);
    // 模拟上一条已发送：下一条必须等 1s，给并发锁停留窗口。
    pacer.last_send = Some(Instant::now());

    let state = b.state.clone();
    let locker = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        state.lock().unwrap().account_locked = true;
    });

    let res = b
        .post_json_ok(
            "messages/text",
            &json!({ "content": "x" }),
            "test",
            &mut pacer,
            true,
        )
        .await;
    locker.await.unwrap();
    assert!(res.is_err(), "锁停后必须拒绝");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "pacer 等待期间锁停后不得再发出 HTTP 请求"
    );
    server.abort();
}

#[tokio::test]
async fn media_upload_failure_counts_toward_fuse() {
    use axum::http::StatusCode;

    async fn fail() -> StatusCode {
        StatusCode::INTERNAL_SERVER_ERROR
    }

    let app = axum::Router::new().route("/api/media/upload", axum::routing::post(fail));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upload server");
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let dir = std::env::temp_dir().join(format!("rclaw_wxbot_upload_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("a.png");
    std::fs::write(&file, b"not a real png").unwrap();

    let mut c = cfg();
    c.api_base = format!("http://{addr}");
    c.media_dir = dir.clone();
    c.send_failure_fuse = 1;
    let b = Wxbot::new(c).expect("mock 配置合法");
    b.state.lock().unwrap().bind("wxid_self");
    let intent = MediaIntent {
        kind: MediaKind::Image,
        path: file.to_string_lossy().into_owned(),
        name: None,
        caption: None,
    };
    let mut pacer = SendPacer::new(&b.cfg);
    let res = b.send_media("conv", &intent, &mut pacer).await;
    assert!(res.is_err(), "upload 500 应失败");
    assert!(
        b.state.lock().unwrap().account_locked,
        "upload 终态失败必须计入熔断"
    );

    server.abort();
    drop(b);
    let _ = std::fs::remove_dir_all(&dir);
}
