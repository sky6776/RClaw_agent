use super::*;
use tokio::net::TcpListener;

async fn next_send_message(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> Value {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(1), ws.next())
            .await
            .expect("等待 OneBot 请求超时")
            .expect("WS 流意外关闭")
            .expect("读取 WS 帧失败");
        let WsMessage::Text(text) = frame else {
            continue;
        };
        let request: Value = serde_json::from_str(&text).expect("OneBot 请求必须是 JSON");
        if request.get("action").and_then(Value::as_str) == Some("send_private_msg") {
            return request;
        }
    }
}

/// 读取下一条非心跳 OneBot 请求，供合并转发等非 send_msg 用例。
async fn next_non_heartbeat_request(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> Value {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("等待 OneBot 请求超时")
            .expect("WS 流意外关闭")
            .expect("读取 WS 帧失败");
        let WsMessage::Text(text) = frame else {
            continue;
        };
        let request: Value = serde_json::from_str(&text).expect("OneBot 请求必须是 JSON");
        if request["action"] == "get_status" {
            respond_ok(ws, &request).await;
            continue;
        }
        return request;
    }
}

async fn respond_ok(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    request: &Value,
) {
    let echo = request.get("echo").cloned().expect("请求必须带 echo");
    ws.send(WsMessage::Text(
        json!({ "status": "ok", "retcode": 0, "data": {}, "echo": echo }).to_string(),
    ))
    .await
    .expect("写入 OneBot 成功响应");
}

async fn respond_error(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    request: &Value,
) {
    let echo = request.get("echo").cloned().expect("请求必须带 echo");
    ws.send(WsMessage::Text(
        json!({
            "status": "failed",
            "retcode": -1,
            "wording": "Timeout: NTEvent serviceAndMethod:NodeIKernelMsgService/sendMsg",
            "echo": echo,
        })
        .to_string(),
    ))
    .await
    .expect("写入 OneBot 失败响应");
}

#[tokio::test]
async fn outbound_messages_wait_for_prior_api_response() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (connected_tx, connected_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = connected_tx.send(());

        let thinking = next_send_message(&mut ws).await;
        assert_eq!(
            thinking["params"]["message"],
            "💭 思考中...\n先简单打个招呼"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), next_send_message(&mut ws))
                .await
                .is_err(),
            "上一条消息确认前，不得提前发送正式回复"
        );
        respond_ok(&mut ws, &thinking).await;

        let reply = next_send_message(&mut ws).await;
        assert_eq!(reply["params"]["message"], "晚上好！");
        respond_ok(&mut ws, &reply).await;
    });

    let nap = NapCat::new(NapCatConfig {
        enabled: true,
        ws_url: format!("ws://{addr}"),
        heartbeat_interval_sec: 60,
        ..NapCatConfig::default()
    })
    .unwrap();
    let (in_tx, _in_rx) = mpsc::channel(1);
    let (out_tx, out_rx) = mpsc::channel(4);
    let channel = tokio::spawn(nap.run(in_tx, out_rx));

    connected_rx.await.expect("NapCat 应连接测试服务器");
    out_tx
        .send(BusMessage::new(
            CHANNEL_NAPCAT,
            "user:123",
            "💭 思考中...\n先简单打个招呼",
        ))
        .await
        .unwrap();
    out_tx
        .send(BusMessage::new(CHANNEL_NAPCAT, "user:123", "晚上好！"))
        .await
        .unwrap();

    server.await.unwrap();
    channel.abort();
    let _ = channel.await;
}

#[tokio::test]
async fn outbound_text_resends_then_skips_on_persistent_ntevent_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (connected_tx, connected_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = connected_tx.send(());

        // 单条短消息 NTEvent 超时 → 重试一次（同一条再发）。
        let first = next_send_message(&mut ws).await;
        assert_eq!(first["params"]["message"], "短消息重试测试");
        respond_error(&mut ws, &first).await;

        let resend = next_send_message(&mut ws).await;
        assert_eq!(resend["params"]["message"], "短消息重试测试");
        respond_error(&mut ws, &resend).await;

        // 重试耗尽后跳过该消息，不再发送。
        assert!(
            tokio::time::timeout(Duration::from_millis(300), next_send_message(&mut ws))
                .await
                .is_err(),
            "重试耗尽后应跳过该消息，不再发送"
        );
    });

    let nap = NapCat::new(NapCatConfig {
        enabled: true,
        ws_url: format!("ws://{addr}"),
        heartbeat_interval_sec: 60,
        ..NapCatConfig::default()
    })
    .unwrap();
    let (in_tx, _in_rx) = mpsc::channel(1);
    let (out_tx, out_rx) = mpsc::channel(1);
    let channel = tokio::spawn(nap.run(in_tx, out_rx));

    connected_rx.await.expect("NapCat 应连接测试服务器");
    out_tx
        .send(BusMessage::new(
            CHANNEL_NAPCAT,
            "user:123",
            "短消息重试测试",
        ))
        .await
        .unwrap();

    server.await.unwrap();
    channel.abort();
    let _ = channel.await;
}

#[tokio::test]
async fn outbound_text_recovers_via_resend_on_ntevent_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (connected_tx, connected_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = connected_tx.send(());

        // 单条短消息首次 NTEvent 超时 → 重试成功，挽回该消息。
        let first = next_send_message(&mut ws).await;
        assert_eq!(first["params"]["message"], "短消息重试测试");
        respond_error(&mut ws, &first).await;

        let resend = next_send_message(&mut ws).await;
        assert_eq!(resend["params"]["message"], "短消息重试测试");
        respond_ok(&mut ws, &resend).await;

        // 重试成功后不再发送。
        assert!(
            tokio::time::timeout(Duration::from_millis(300), next_send_message(&mut ws))
                .await
                .is_err(),
            "重试成功后不应再发送"
        );
    });

    let nap = NapCat::new(NapCatConfig {
        enabled: true,
        ws_url: format!("ws://{addr}"),
        heartbeat_interval_sec: 60,
        ..NapCatConfig::default()
    })
    .unwrap();
    let (in_tx, _in_rx) = mpsc::channel(1);
    let (out_tx, out_rx) = mpsc::channel(1);
    let channel = tokio::spawn(nap.run(in_tx, out_rx));

    connected_rx.await.expect("NapCat 应连接测试服务器");
    out_tx
        .send(BusMessage::new(
            CHANNEL_NAPCAT,
            "user:123",
            "短消息重试测试",
        ))
        .await
        .unwrap();

    server.await.unwrap();
    channel.abort();
    let _ = channel.await;
}

#[tokio::test]
async fn outbound_text_aborts_on_non_ntevent_send_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (connected_tx, connected_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = connected_tx.send(());

        let first = next_send_message(&mut ws).await;
        assert_eq!(first["params"]["message"], "短消息中止测试");
        // 非 NTEvent 错误（如账号封禁）应中止整条消息，不重试。
        let echo = first.get("echo").cloned().expect("请求必须带 echo");
        ws.send(WsMessage::Text(
            json!({
                "status": "failed",
                "retcode": -1,
                "wording": "账号被冻结",
                "echo": echo,
            })
            .to_string(),
        ))
        .await
        .expect("写入非 NTEvent 错误响应");

        assert!(
            tokio::time::timeout(Duration::from_millis(300), next_send_message(&mut ws))
                .await
                .is_err(),
            "非 NTEvent 错误应中止整条消息，不得继续发送后续分段"
        );
    });

    let nap = NapCat::new(NapCatConfig {
        enabled: true,
        ws_url: format!("ws://{addr}"),
        heartbeat_interval_sec: 60,
        ..NapCatConfig::default()
    })
    .unwrap();
    let (in_tx, _in_rx) = mpsc::channel(1);
    let (out_tx, out_rx) = mpsc::channel(1);
    let channel = tokio::spawn(nap.run(in_tx, out_rx));

    connected_rx.await.expect("NapCat 应连接测试服务器");
    out_tx
        .send(BusMessage::new(
            CHANNEL_NAPCAT,
            "user:123",
            "短消息中止测试",
        ))
        .await
        .unwrap();

    server.await.expect("测试服务器应完成");
    channel.abort();
    let _ = channel.await;
}

#[tokio::test]
async fn outbound_long_text_uses_forward_card() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (connected_tx, connected_rx) = oneshot::channel();
    let long_text = "这是一段用于测试合并转发卡片的正文内容。".repeat(400);
    let expected_text = long_text.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = connected_tx.send(());

        // 直接发合并转发卡片；发送者 UIN 由 NapCat 缺省为机器人自身账号。
        let fwd = next_non_heartbeat_request(&mut ws).await;
        assert_eq!(fwd["action"], "send_private_forward_msg");
        let msgs = fwd["params"]["messages"]
            .as_array()
            .expect("messages 应为数组");
        assert!(msgs.len() >= 2, "长文应切成多个 node，实际 {}", msgs.len());
        assert!(msgs.iter().all(|n| n["type"] == "node"));
        assert!(msgs.iter().all(|n| n["data"]["nickname"] == "RClaw"));
        let actual_text: String = msgs
            .iter()
            .map(|n| {
                n["data"]["content"]
                    .as_str()
                    .expect("node content 应为文本")
            })
            .collect();
        assert_eq!(actual_text, expected_text, "node 拼接后必须完整还原原文");
        respond_ok(&mut ws, &fwd).await;
    });

    let nap = NapCat::new(NapCatConfig {
        enabled: true,
        ws_url: format!("ws://{addr}"),
        heartbeat_interval_sec: 60,
        ..NapCatConfig::default()
    })
    .unwrap();
    let (in_tx, _in_rx) = mpsc::channel(1);
    let (out_tx, out_rx) = mpsc::channel(1);
    let channel = tokio::spawn(nap.run(in_tx, out_rx));

    connected_rx.await.expect("NapCat 应连接测试服务器");
    // 超过 send_chunk_bytes(4500)，触发合并转发。
    out_tx
        .send(BusMessage::new(CHANNEL_NAPCAT, "user:123", long_text))
        .await
        .unwrap();

    server.await.unwrap();
    channel.abort();
    let _ = channel.await;
}

#[tokio::test]
async fn outbound_long_text_falls_back_to_chunks_when_forward_fails() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (connected_tx, connected_rx) = oneshot::channel();
    let long_text = "这是一段用于测试合并转发回退的正文内容。".repeat(400);
    let expected_text = long_text.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = connected_tx.send(());

        let fwd = next_non_heartbeat_request(&mut ws).await;
        assert_eq!(fwd["action"], "send_private_forward_msg");
        let echo = fwd.get("echo").cloned().expect("请求必须带 echo");
        // 合并转发不可用 → 触发回退。
        ws.send(WsMessage::Text(
            json!({ "status": "failed", "retcode": -1, "wording": "合并转发不可用", "echo": echo })
                .to_string(),
        ))
        .await
        .unwrap();

        let mut actual_text = String::new();
        while actual_text.len() < expected_text.len() {
            let req = next_non_heartbeat_request(&mut ws).await;
            assert_eq!(req["action"], "send_private_msg");
            let chunk = req["params"]["message"]
                .as_str()
                .expect("fallback message 应为文本");
            assert!(chunk.len() <= SEND_CHUNK_BYTES);
            actual_text.push_str(chunk);
            respond_ok(&mut ws, &req).await;
        }
        assert_eq!(actual_text, expected_text, "fallback 分段必须完整还原原文");
    });

    let nap = NapCat::new(NapCatConfig {
        enabled: true,
        ws_url: format!("ws://{addr}"),
        heartbeat_interval_sec: 60,
        ..NapCatConfig::default()
    })
    .unwrap();
    let (in_tx, _in_rx) = mpsc::channel(1);
    let (out_tx, out_rx) = mpsc::channel(1);
    let channel = tokio::spawn(nap.run(in_tx, out_rx));

    connected_rx.await.expect("NapCat 应连接测试服务器");
    out_tx
        .send(BusMessage::new(CHANNEL_NAPCAT, "user:123", long_text))
        .await
        .unwrap();

    server.await.unwrap();
    channel.abort();
    let _ = channel.await;
}

#[test]
fn napcat_is_max_user_semantics() {
    // 空（含纯空白）= 无人拥有 max 权限（与 allow_from「空=放行全部」相反）。
    assert!(!NapCat::is_max_user("", "123"));
    assert!(!NapCat::is_max_user("   ", "123"));
    // 命中。
    assert!(NapCat::is_max_user("123,456", "123"));
    assert!(NapCat::is_max_user("123,456", "456"));
    // 未命中。
    assert!(!NapCat::is_max_user("123,456", "789"));
    // `*` = 全部 QQ 都是 max。
    assert!(NapCat::is_max_user("*", "anyone"));
    // 容错空白。
    assert!(NapCat::is_max_user(" 123 , 456 ", "123"));
}

#[test]
fn napcat_zero_send_chunk_bytes_uses_default() {
    let nap = NapCat::new(NapCatConfig {
        enabled: true,
        ws_url: "ws://127.0.0.1:3001".into(),
        send_chunk_bytes: 0,
        ..NapCatConfig::default()
    })
    .unwrap();

    assert_eq!(nap.cfg.send_chunk_bytes, SEND_CHUNK_BYTES);
}

#[test]
fn max_access_is_limited_to_private_messages() {
    let nap = NapCat::new(NapCatConfig {
        enabled: true,
        ws_url: "ws://127.0.0.1:3001".into(),
        max_from: "123".into(),
        ..NapCatConfig::default()
    })
    .unwrap();

    assert_eq!(nap.tool_access_for("123", false), ToolAccess::Unrestricted);
    assert_eq!(nap.tool_access_for("123", true), ToolAccess::Sandboxed);
    assert_eq!(nap.tool_access_for("456", false), ToolAccess::Sandboxed);
}

fn media_paths_with_suffix(media_dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let dir = media_dir.join("napcat");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|v| v.to_str())
                .is_some_and(|name| name.ends_with(suffix))
        })
        .collect();
    paths.sort();
    paths
}

#[test]
fn parse_chat_id_variants() {
    assert_eq!(parse_chat_id("group:123"), ("group", "123".into()));
    assert_eq!(parse_chat_id("user:456"), ("private", "456".into()));
    assert_eq!(parse_chat_id("plain"), ("private", "plain".into()));
}

#[test]
fn utf8_chunks_never_split_char() {
    let s = "你好世界，这是一段用来测试中文字符切分的文本".repeat(3);
    for chunk in utf8_chunks(&s, 20) {
        assert!(std::str::from_utf8(chunk.as_bytes()).is_ok());
        assert!(chunk.len() <= 20);
    }
}

#[test]
fn utf8_chunks_zero_max_no_loop() {
    assert_eq!(utf8_chunks("abc", 0), vec!["abc"]);
}

#[test]
fn utf8_chunks_tiny_max_no_loop() {
    let s = "你好"; // 6 字节
    let chunks = utf8_chunks(s, 1);
    assert_eq!(chunks.concat(), s);
    for c in &chunks {
        assert!(std::str::from_utf8(c.as_bytes()).is_ok());
    }
}

#[test]
fn split_forward_nodes_keeps_sentences_intact_and_within_budget() {
    // 每句 27 字节、以句号结尾；max=60 时每节点累积 2 句（54 字节）。
    let sentence = "这是一句完整的话。";
    let text = sentence.repeat(6);
    let nodes = split_forward_nodes(&text, 60);
    assert!(
        nodes.iter().all(|n| n.len() <= 60),
        "node 超出字节预算: {:?}",
        nodes.iter().map(|n| n.len()).collect::<Vec<_>>()
    );
    assert!(
        nodes.iter().all(|n| n.ends_with('。')),
        "node 必须在句末切分、不得切断整句: {:?}",
        nodes
    );
    assert_eq!(nodes.concat(), text, "拼接应还原原文，不丢字、不加字");
    assert!(nodes.len() >= 3, "应切成多个 node: {}", nodes.len());
}

#[test]
fn split_forward_nodes_hard_splits_without_breaking_utf8() {
    // 无标点长串：只能按字节硬切，但不得切断多字节字符。
    let text = "爱".repeat(100); // 300 字节
    let nodes = split_forward_nodes(&text, 30);
    assert!(
        nodes.iter().all(|n| n.len() <= 30),
        "硬切也须受字节预算约束"
    );
    for n in &nodes {
        assert!(
            std::str::from_utf8(n.as_bytes()).is_ok(),
            "硬切不得破坏 UTF-8 字符"
        );
    }
    assert_eq!(nodes.concat(), text);
    assert!(
        nodes.len() >= 9,
        "300/30 应切约 10 个 node: {}",
        nodes.len()
    );
}

#[test]
fn split_forward_nodes_prefers_paragraph_boundary() {
    let text = "第一段。\n第二段第一句。第二段第二句。";
    let paragraph_end = text.find('\n').unwrap() + 1;
    let later_sentence_end = text
        .match_indices('。')
        .nth(1)
        .map(|(index, _)| index + '。'.len_utf8())
        .expect("预算内应有句末标点");
    assert!(later_sentence_end > paragraph_end);

    let nodes = split_forward_nodes(text, later_sentence_end);

    assert_eq!(nodes[0], &text[..paragraph_end]);
    assert_eq!(nodes.concat(), text);
}

#[test]
fn split_forward_nodes_uses_ascii_sentence_boundary() {
    let first = "First sentence.";
    let text = format!("{first} Second sentence.");

    let nodes = split_forward_nodes(&text, first.len());

    assert_eq!(nodes[0], first);
    assert_eq!(nodes.concat(), text);
}

#[test]
fn split_forward_nodes_does_not_split_at_decimal_point() {
    let first_budget = "Price 12.34";
    let text = format!("{first_budget} keeps moving without a stop");

    let nodes = split_forward_nodes(&text, first_budget.len());

    assert_eq!(nodes[0], first_budget);
    assert_eq!(nodes.concat(), text);
}

#[test]
fn split_forward_nodes_does_not_create_leading_blank_node() {
    let text = format!("\n{}", "正文".repeat(20));

    let nodes = split_forward_nodes(&text, 30);

    assert_ne!(nodes[0], "\n");
    assert_eq!(nodes.concat(), text);
}

#[test]
fn extract_text_string_message() {
    let ext = extract_message(&Value::String("hello".into()), "999");
    assert_eq!(ext.text, "hello");
    assert!(!ext.at_me);
    assert!(ext.media.is_empty());
}

#[test]
fn extract_segments_strip_at_me_and_collect_media() {
    let msg = json!([
        {"type":"at","data":{"qq":"999"}},
        {"type":"text","data":{"text":"hi "}},
        {"type":"image","data":{"url":"http://x/a.png","file":"a.png"}},
        {"type":"file","data":{"file":"report.pdf","file_id":"fid1","busid":102}}
    ]);
    let ext = extract_message(&msg, "999");
    assert!(ext.at_me);
    assert_eq!(ext.text, "hi "); // @机器人 被剥离
    assert_eq!(ext.media.len(), 2);
    assert_eq!(ext.media[0].kind, MediaKind::Image);
    assert_eq!(ext.media[0].url.as_deref(), Some("http://x/a.png"));
    assert_eq!(ext.media[1].kind, MediaKind::File);
    assert_eq!(ext.media[1].file_id.as_deref(), Some("fid1"));
    assert_eq!(ext.media[1].name.as_deref(), Some("report.pdf"));
    assert_eq!(ext.media[1].busid, Some(102));
    assert_eq!(ext.media[1].url, None);
}

#[test]
fn group_upload_media_ref_preserves_id_name_and_busid() {
    let file = json!({
        "id": "fid1",
        "name": "report.pdf",
        "size": 12345,
        "busid": 102
    });
    let m = group_upload_media_ref(&file).expect("group_upload file 应可解析");
    assert_eq!(m.kind, MediaKind::File);
    assert_eq!(m.file_id.as_deref(), Some("fid1"));
    assert_eq!(m.name.as_deref(), Some("report.pdf"));
    assert_eq!(m.busid, Some(102));
}

#[test]
fn group_message_reaches_llm_respects_mode() {
    assert!(!group_message_reaches_llm("none", true, false));
    assert!(!group_message_reaches_llm("none", false, true));
    assert!(!group_message_reaches_llm("at_only", false, false));
    assert!(group_message_reaches_llm("at_only", true, false));
    assert!(group_message_reaches_llm("at_only", false, true));
    assert!(group_message_reaches_llm("all", false, false));
}

#[tokio::test]
async fn group_upload_notice_is_delivered_as_inbound_media_notice_in_all_mode() {
    let cfg = NapCatConfig {
        enabled: true,
        ws_url: "ws://127.0.0.1:3001".into(),
        group_response_mode: "all".into(),
        media_dir: PathBuf::new(),
        ..NapCatConfig::default()
    };
    let nap = Arc::new(NapCat::new(cfg).unwrap());
    let (in_tx, mut in_rx) = mpsc::channel(1);
    let (api_tx, _api_rx) = mpsc::channel(1);
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let stream_pending: StreamPendingMap = Arc::new(Mutex::new(HashMap::new()));
    let dedup: DedupSet = Arc::new(Mutex::new(HashSet::new()));
    let event = json!({
        "post_type": "notice",
        "notice_type": "group_upload",
        "group_id": 123,
        "user_id": 456,
        "file": {
            "id": "fid1",
            "name": "report.pdf",
            "size": 12345,
            "busid": 102
        }
    });

    nap.handle_ws_text(
        &event.to_string(),
        &in_tx,
        &api_tx,
        &pending,
        &stream_pending,
        &dedup,
    )
    .await;

    let msg = timeout(Duration::from_secs(1), in_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.chat_id, "group:123");
    assert!(msg.content.contains("[文件: report.pdf]"));
}

#[tokio::test]
async fn group_upload_notice_is_not_delivered_to_llm_in_at_only_mode() {
    let cfg = NapCatConfig {
        enabled: true,
        ws_url: "ws://127.0.0.1:3001".into(),
        group_response_mode: "at_only".into(),
        media_dir: PathBuf::new(),
        ..NapCatConfig::default()
    };
    let nap = Arc::new(NapCat::new(cfg).unwrap());
    let (in_tx, mut in_rx) = mpsc::channel(1);
    let (api_tx, _api_rx) = mpsc::channel(1);
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let stream_pending: StreamPendingMap = Arc::new(Mutex::new(HashMap::new()));
    let dedup: DedupSet = Arc::new(Mutex::new(HashSet::new()));
    let event = json!({
        "post_type": "notice",
        "notice_type": "group_upload",
        "group_id": 123,
        "user_id": 456,
        "file": {
            "id": "fid1",
            "name": "report.pdf",
            "size": 12345,
            "busid": 102
        }
    });

    nap.handle_ws_text(
        &event.to_string(),
        &in_tx,
        &api_tx,
        &pending,
        &stream_pending,
        &dedup,
    )
    .await;

    assert!(timeout(Duration::from_millis(50), in_rx.recv())
        .await
        .is_err());
}

#[tokio::test]
async fn group_message_in_at_only_reaches_llm_only_when_mentioned() {
    let cfg = NapCatConfig {
        enabled: true,
        ws_url: "ws://127.0.0.1:3001".into(),
        group_response_mode: "at_only".into(),
        media_dir: PathBuf::new(),
        ..NapCatConfig::default()
    };
    let nap = Arc::new(NapCat::new(cfg).unwrap());
    let (in_tx, mut in_rx) = mpsc::channel(2);
    let (api_tx, _api_rx) = mpsc::channel(1);
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let stream_pending: StreamPendingMap = Arc::new(Mutex::new(HashMap::new()));
    let dedup: DedupSet = Arc::new(Mutex::new(HashSet::new()));

    let event_without_at = json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 123,
        "user_id": 456,
        "self_id": 999,
        "message_id": 1,
        "message": [{"type":"text","data":{"text":"hello"}}]
    });
    nap.clone()
        .handle_ws_text(
            &event_without_at.to_string(),
            &in_tx,
            &api_tx,
            &pending,
            &stream_pending,
            &dedup,
        )
        .await;
    assert!(timeout(Duration::from_millis(50), in_rx.recv())
        .await
        .is_err());

    let event_with_at = json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 123,
        "user_id": 456,
        "self_id": 999,
        "message_id": 2,
        "message": [
            {"type":"at","data":{"qq":"999"}},
            {"type":"text","data":{"text":"hello"}}
        ]
    });
    nap.handle_ws_text(
        &event_with_at.to_string(),
        &in_tx,
        &api_tx,
        &pending,
        &stream_pending,
        &dedup,
    )
    .await;

    let msg = timeout(Duration::from_secs(1), in_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.chat_id, "group:123");
    assert_eq!(msg.content, "hello");
}

#[tokio::test]
async fn at_only_reuses_suppressed_media_when_mentioned_with_same_file_id() {
    let dir = tempfile::tempdir().unwrap();
    let media_dir = dir.path().join("media");
    let source_dir = dir.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let source = source_dir.join("doc.txt");
    std::fs::write(&source, b"hello").unwrap();

    let cfg = NapCatConfig {
        enabled: true,
        ws_url: "ws://127.0.0.1:3001".into(),
        group_response_mode: "at_only".into(),
        media_dir: media_dir.clone(),
        ..NapCatConfig::default()
    };
    let nap = Arc::new(NapCat::new(cfg).unwrap());
    let (in_tx, mut in_rx) = mpsc::channel(2);
    let (api_tx, _api_rx) = mpsc::channel(1);
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let stream_pending: StreamPendingMap = Arc::new(Mutex::new(HashMap::new()));
    let dedup: DedupSet = Arc::new(Mutex::new(HashSet::new()));

    let first = json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 123,
        "user_id": 456,
        "self_id": 999,
        "message_id": 10,
        "message": [{"type":"file","data":{
            "file":"doc.txt",
            "file_id":"fid-doc",
            "path": source.to_string_lossy()
        }}]
    });
    nap.clone()
        .handle_ws_text(
            &first.to_string(),
            &in_tx,
            &api_tx,
            &pending,
            &stream_pending,
            &dedup,
        )
        .await;
    assert!(timeout(Duration::from_millis(50), in_rx.recv())
        .await
        .is_err());
    let first_paths = media_paths_with_suffix(&media_dir, "doc.txt");
    assert_eq!(first_paths.len(), 1);
    let first_path = first_paths[0].to_string_lossy().replace('\\', "/");

    let second = json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 123,
        "user_id": 456,
        "self_id": 999,
        "message_id": 11,
        "message": [
            {"type":"at","data":{"qq":"999"}},
            {"type":"text","data":{"text":"请看这个"}},
            {"type":"file","data":{
                "file":"doc.txt",
                "file_id":"fid-doc",
                "path": source.to_string_lossy()
            }}
        ]
    });
    nap.handle_ws_text(
        &second.to_string(),
        &in_tx,
        &api_tx,
        &pending,
        &stream_pending,
        &dedup,
    )
    .await;

    let msg = timeout(Duration::from_secs(1), in_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(msg.content.contains(&first_path));
    assert_eq!(media_paths_with_suffix(&media_dir, "doc.txt").len(), 1);
}

#[tokio::test]
async fn at_only_reply_to_suppressed_media_uses_cached_path() {
    let dir = tempfile::tempdir().unwrap();
    let media_dir = dir.path().join("media");
    let source_dir = dir.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let source = source_dir.join("reply-doc.txt");
    std::fs::write(&source, b"reply").unwrap();

    let cfg = NapCatConfig {
        enabled: true,
        ws_url: "ws://127.0.0.1:3001".into(),
        group_response_mode: "at_only".into(),
        media_dir: media_dir.clone(),
        ..NapCatConfig::default()
    };
    let nap = Arc::new(NapCat::new(cfg).unwrap());
    let (in_tx, mut in_rx) = mpsc::channel(2);
    let (api_tx, _api_rx) = mpsc::channel(1);
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let stream_pending: StreamPendingMap = Arc::new(Mutex::new(HashMap::new()));
    let dedup: DedupSet = Arc::new(Mutex::new(HashSet::new()));

    let first = json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 123,
        "user_id": 456,
        "self_id": 999,
        "message_id": 20,
        "message": [{"type":"file","data":{
            "file":"reply-doc.txt",
            "file_id":"fid-reply-doc",
            "path": source.to_string_lossy()
        }}]
    });
    nap.clone()
        .handle_ws_text(
            &first.to_string(),
            &in_tx,
            &api_tx,
            &pending,
            &stream_pending,
            &dedup,
        )
        .await;
    assert!(timeout(Duration::from_millis(50), in_rx.recv())
        .await
        .is_err());
    let first_paths = media_paths_with_suffix(&media_dir, "reply-doc.txt");
    assert_eq!(first_paths.len(), 1);
    let first_path = first_paths[0].to_string_lossy().replace('\\', "/");

    let second = json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 123,
        "user_id": 456,
        "self_id": 999,
        "message_id": 21,
        "message": [
            {"type":"reply","data":{"id":20}},
            {"type":"at","data":{"qq":"999"}},
            {"type":"text","data":{"text":"请看引用文档"}}
        ]
    });
    nap.handle_ws_text(
        &second.to_string(),
        &in_tx,
        &api_tx,
        &pending,
        &stream_pending,
        &dedup,
    )
    .await;

    let msg = timeout(Duration::from_secs(1), in_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(msg.content.contains("引用文件: reply-doc.txt"));
    assert!(msg.content.contains(&first_path));
    assert_eq!(
        media_paths_with_suffix(&media_dir, "reply-doc.txt").len(),
        1
    );
}

#[tokio::test]
async fn reply_media_cache_is_scoped_by_group() {
    let dir = tempfile::tempdir().unwrap();
    let media_dir = dir.path().join("media");
    let source_dir = dir.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let source = source_dir.join("scoped-doc.txt");
    std::fs::write(&source, b"scoped").unwrap();

    let cfg = NapCatConfig {
        enabled: true,
        ws_url: "ws://127.0.0.1:3001".into(),
        group_response_mode: "at_only".into(),
        media_dir: media_dir.clone(),
        ..NapCatConfig::default()
    };
    let nap = Arc::new(NapCat::new(cfg).unwrap());
    let (in_tx, mut in_rx) = mpsc::channel(2);
    let (api_tx, _api_rx) = mpsc::channel(1);
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let stream_pending: StreamPendingMap = Arc::new(Mutex::new(HashMap::new()));
    let dedup: DedupSet = Arc::new(Mutex::new(HashSet::new()));

    let first = json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 123,
        "user_id": 456,
        "self_id": 999,
        "message_id": 50,
        "message": [{"type":"file","data":{
            "file":"scoped-doc.txt",
            "file_id":"fid-scoped-doc",
            "path": source.to_string_lossy()
        }}]
    });
    nap.clone()
        .handle_ws_text(
            &first.to_string(),
            &in_tx,
            &api_tx,
            &pending,
            &stream_pending,
            &dedup,
        )
        .await;
    assert!(timeout(Duration::from_millis(50), in_rx.recv())
        .await
        .is_err());
    let first_path = media_paths_with_suffix(&media_dir, "scoped-doc.txt")[0]
        .to_string_lossy()
        .replace('\\', "/");

    let second = json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 999,
        "user_id": 456,
        "self_id": 999,
        "message_id": 51,
        "message": [
            {"type":"reply","data":{"id":50}},
            {"type":"at","data":{"qq":"999"}},
            {"type":"text","data":{"text":"另一个群的同 id 引用"}}
        ]
    });
    nap.handle_ws_text(
        &second.to_string(),
        &in_tx,
        &api_tx,
        &pending,
        &stream_pending,
        &dedup,
    )
    .await;

    let msg = timeout(Duration::from_secs(1), in_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.chat_id, "group:999");
    assert!(msg.content.contains("另一个群的同 id 引用"));
    assert!(!msg.content.contains("引用文件"));
    assert!(!msg.content.contains(&first_path));
}

#[tokio::test]
async fn persisted_media_index_survives_restart_for_reply_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let media_dir = dir.path().join("media");
    let source_dir = dir.path().join("source");
    std::fs::create_dir_all(&source_dir).unwrap();
    let source = source_dir.join("persisted-doc.txt");
    std::fs::write(&source, b"persisted").unwrap();

    let cfg = NapCatConfig {
        enabled: true,
        ws_url: "ws://127.0.0.1:3001".into(),
        group_response_mode: "at_only".into(),
        media_dir: media_dir.clone(),
        ..NapCatConfig::default()
    };
    let nap = Arc::new(NapCat::new(cfg.clone()).unwrap());
    let (in_tx, mut in_rx) = mpsc::channel(2);
    let (api_tx, _api_rx) = mpsc::channel(1);
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let stream_pending: StreamPendingMap = Arc::new(Mutex::new(HashMap::new()));
    let dedup: DedupSet = Arc::new(Mutex::new(HashSet::new()));

    let first = json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 123,
        "user_id": 456,
        "self_id": 999,
        "message_id": 30,
        "message": [{"type":"file","data":{
            "file":"persisted-doc.txt",
            "file_id":"fid-persisted-doc",
            "path": source.to_string_lossy()
        }}]
    });
    nap.handle_ws_text(
        &first.to_string(),
        &in_tx,
        &api_tx,
        &pending,
        &stream_pending,
        &dedup,
    )
    .await;
    assert!(timeout(Duration::from_millis(50), in_rx.recv())
        .await
        .is_err());
    let first_paths = media_paths_with_suffix(&media_dir, "persisted-doc.txt");
    assert_eq!(first_paths.len(), 1);
    let first_path = first_paths[0].to_string_lossy().replace('\\', "/");
    assert!(media_cache_index_path(&media_dir).unwrap().is_file());

    let nap_after_restart = Arc::new(NapCat::new(cfg).unwrap());
    let (in_tx2, mut in_rx2) = mpsc::channel(1);
    let (api_tx2, _api_rx2) = mpsc::channel(1);
    let pending2: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let stream_pending2: StreamPendingMap = Arc::new(Mutex::new(HashMap::new()));
    let dedup2: DedupSet = Arc::new(Mutex::new(HashSet::new()));
    let second = json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 123,
        "user_id": 456,
        "self_id": 999,
        "message_id": 31,
        "message": [
            {"type":"reply","data":{"id":30}},
            {"type":"at","data":{"qq":"999"}},
            {"type":"text","data":{"text":"重启后看引用"}}
        ]
    });
    nap_after_restart
        .handle_ws_text(
            &second.to_string(),
            &in_tx2,
            &api_tx2,
            &pending2,
            &stream_pending2,
            &dedup2,
        )
        .await;

    let msg = timeout(Duration::from_secs(1), in_rx2.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(msg.content.contains("引用文件: persisted-doc.txt"));
    assert!(msg.content.contains(&first_path));
    assert_eq!(
        media_paths_with_suffix(&media_dir, "persisted-doc.txt").len(),
        1
    );
}

#[tokio::test]
async fn persisted_text_index_survives_restart_for_reply_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let media_dir = dir.path().join("media");
    let cfg = NapCatConfig {
        enabled: true,
        ws_url: "ws://127.0.0.1:3001".into(),
        group_response_mode: "at_only".into(),
        media_dir: media_dir.clone(),
        ..NapCatConfig::default()
    };
    let nap = Arc::new(NapCat::new(cfg.clone()).unwrap());
    let (in_tx, mut in_rx) = mpsc::channel(1);
    let (api_tx, _api_rx) = mpsc::channel(1);
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let stream_pending: StreamPendingMap = Arc::new(Mutex::new(HashMap::new()));
    let dedup: DedupSet = Arc::new(Mutex::new(HashSet::new()));
    let first = json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 123,
        "user_id": 456,
        "self_id": 999,
        "message_id": 40,
        "message": [{"type":"text","data":{"text":"前面这条普通文本"}}]
    });
    nap.handle_ws_text(
        &first.to_string(),
        &in_tx,
        &api_tx,
        &pending,
        &stream_pending,
        &dedup,
    )
    .await;
    assert!(timeout(Duration::from_millis(50), in_rx.recv())
        .await
        .is_err());
    let text_index_dir = message_index_group_dir(&media_dir, "123").unwrap();
    assert!(std::fs::read_dir(&text_index_dir)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry.path().extension().and_then(|v| v.to_str()) == Some("jsonl")));

    let nap_after_restart = Arc::new(NapCat::new(cfg).unwrap());
    let (in_tx2, mut in_rx2) = mpsc::channel(1);
    let (api_tx2, _api_rx2) = mpsc::channel(1);
    let pending2: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let stream_pending2: StreamPendingMap = Arc::new(Mutex::new(HashMap::new()));
    let dedup2: DedupSet = Arc::new(Mutex::new(HashSet::new()));
    let second = json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 123,
        "user_id": 456,
        "self_id": 999,
        "message_id": 41,
        "message": [
            {"type":"reply","data":{"id":40}},
            {"type":"at","data":{"qq":"999"}},
            {"type":"text","data":{"text":"请回答这条引用"}}
        ]
    });
    nap_after_restart
        .handle_ws_text(
            &second.to_string(),
            &in_tx2,
            &api_tx2,
            &pending2,
            &stream_pending2,
            &dedup2,
        )
        .await;

    let msg = timeout(Duration::from_secs(1), in_rx2.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(msg.content.contains("请回答这条引用"));
    assert!(msg.content.contains("[引用消息] 前面这条普通文本"));
}

#[test]
fn text_cache_trim_bounds_memory() {
    let mut cache = HashMap::new();
    for i in 0..=TEXT_CACHE_CAP {
        cache.insert(
            format!("g:{i}"),
            CachedText {
                message_id: i as i64,
                group_id: "g".into(),
                user_id: "u".into(),
                text: "x".into(),
                truncated: false,
            },
        );
    }
    trim_text_cache(&mut cache);
    assert!(cache.len() <= TEXT_CACHE_CAP);
}

#[tokio::test]
async fn group_message_in_none_mode_is_archived_without_llm_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let media_dir = dir.path().join("media");
    let cfg = NapCatConfig {
        enabled: true,
        ws_url: "ws://127.0.0.1:3001".into(),
        group_response_mode: "none".into(),
        media_dir: media_dir.clone(),
        ..NapCatConfig::default()
    };
    let nap = Arc::new(NapCat::new(cfg).unwrap());
    let (in_tx, mut in_rx) = mpsc::channel(1);
    let (api_tx, _api_rx) = mpsc::channel(1);
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let stream_pending: StreamPendingMap = Arc::new(Mutex::new(HashMap::new()));
    let dedup: DedupSet = Arc::new(Mutex::new(HashSet::new()));
    let event = json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 123,
        "user_id": 456,
        "self_id": 999,
        "message_id": 1,
        "message": [{"type":"text","data":{"text":"hello"}}]
    });

    nap.handle_ws_text(
        &event.to_string(),
        &in_tx,
        &api_tx,
        &pending,
        &stream_pending,
        &dedup,
    )
    .await;

    assert!(timeout(Duration::from_millis(50), in_rx.recv())
        .await
        .is_err());
    let files = std::fs::read_dir(media_dir.join("napcat"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file() && entry.file_name() != "media_index.json")
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 1);
    let archived = std::fs::read_to_string(files[0].path()).unwrap();
    assert!(archived.contains("group_id: 123"));
    assert!(archived.contains("user_id: 456"));
    assert!(archived.contains("hello"));
}

#[test]
fn extract_at_all_and_voice_video() {
    let msg = json!([
        {"type":"text","data":{"text":"x"}},
        {"type":"at","data":{"qq":"all"}},
        {"type":"record","data":{"url":"http://x/v.mp3"}},
        {"type":"video","data":{"file":"v.mp4"}}
    ]);
    let ext = extract_message(&msg, "999");
    assert!(ext.at_all);
    assert!(ext.text.contains("[@all]"));
    assert_eq!(ext.media.len(), 2);
    assert_eq!(ext.media[0].kind, MediaKind::Voice);
    assert_eq!(ext.media[1].kind, MediaKind::Video);
    // video 段无 url，但 file 字段被收为 file_id（供 get_file 换链）。
    assert_eq!(ext.media[1].file_id.as_deref(), Some("v.mp4"));
}

#[test]
fn extract_reply_skipped() {
    let msg = json!([
        {"type":"reply","data":{"id":"1"}},
        {"type":"file","data":{"name":"a.pdf"}}
    ]);
    let ext = extract_message(&msg, "999");
    assert_eq!(ext.reply_ids, vec![1]);
    assert_eq!(ext.media.len(), 1);
    assert!(ext.text.is_empty()); // reply 不注入
}

#[test]
fn id_str_handles_number_and_string() {
    assert_eq!(id_str(Some(&json!(123456))), "123456");
    assert_eq!(id_str(Some(&json!("789"))), "789");
    assert_eq!(id_str(None), "");
}

#[test]
fn build_ws_request_trailing_slash_and_token() {
    let req = build_ws_request("ws://127.0.0.1:3001", "tok").unwrap();
    let uri = req.uri().to_string();
    assert!(uri.starts_with("ws://127.0.0.1:3001/?access_token=tok"));
    assert_eq!(req.headers().get("Authorization").unwrap(), "Bearer tok");
}

#[test]
fn build_ws_request_preserves_existing_query() {
    let req = build_ws_request("ws://h:1/p?x=1", "").unwrap();
    assert_eq!(req.uri().to_string(), "ws://h:1/p/?x=1");
}

#[test]
fn allow_block_lists() {
    // allowlist：空=放行全部；精确匹配；列表含 `*` 通配时放行任意 id。
    assert!(NapCat::allow_listed("", "anyone"));
    assert!(NapCat::allow_listed("111,222", "111"));
    assert!(NapCat::allow_listed("111,*", "999"));
    assert!(!NapCat::allow_listed("111,222", "333"));

    // blocklist：空=不拦；精确命中或列表含 `*` 通配时拦任意 id。
    assert!(!NapCat::is_blocked("", "anyone"));
    assert!(NapCat::is_blocked("333", "333"));
    assert!(!NapCat::is_blocked("333", "other"));
    assert!(NapCat::is_blocked("*", "anyone"));
}

#[test]
fn mark_seen_dedup() {
    let dedup: DedupSet = Arc::new(Mutex::new(HashSet::new()));
    assert!(!NapCat::mark_seen(&dedup, 1)); // 首次
    assert!(NapCat::mark_seen(&dedup, 1)); // 重复
    assert!(!NapCat::mark_seen(&dedup, 2));
}

#[test]
fn mark_seen_zero_id_not_deduped() {
    let dedup: DedupSet = Arc::new(Mutex::new(HashSet::new()));
    assert!(!NapCat::mark_seen(&dedup, 0));
    assert!(!NapCat::mark_seen(&dedup, 0));
    assert!(!NapCat::mark_seen(&dedup, 1));
}

#[test]
fn media_delivery_timeout_does_not_fallback_and_duplicate() {
    for message in [
        "api 超时",
        "api 响应通道关闭",
        "Timeout: NTEvent serviceAndMethod:NodeIKernelMsgService/sendMsg",
        "  Timeout: NTEvent serviceAndMethod:NodeIKernelMsgService/sendMsg\n",
    ] {
        let error = MediaSendError::deliver(message.to_string());
        assert!(
            !should_fallback_after_media_error(&error),
            "发送结果未知时不得再次投递: {message}"
        );
    }
}

#[test]
fn media_prepare_failure_or_definite_rejection_still_falls_back() {
    let upload_timeout = MediaSendError::prepare("api 超时".to_string());
    assert!(should_fallback_after_media_error(&upload_timeout));

    let rejected = MediaSendError::deliver("不支持的文件格式".to_string());
    assert!(should_fallback_after_media_error(&rejected));
}

#[test]
fn only_napcat_send_msg_timeout_is_retryable() {
    assert!(napcat_send_msg_timeout(
        "Timeout: NTEvent serviceAndMethod:NodeIKernelMsgService/sendMsg"
    ));
    assert!(napcat_send_msg_timeout(
        "  Timeout: NTEvent serviceAndMethod:NodeIKernelMsgService/sendMsg\n"
    ));
    assert!(!napcat_send_msg_timeout("api 超时"));
    assert!(!napcat_send_msg_timeout("api 响应通道关闭"));
    assert!(!napcat_send_msg_timeout(
        "Timeout: NTEvent serviceAndMethod:NodeIKernelMsgService/getMsgs"
    ));
}

#[test]
fn text_send_retries_client_api_timeout_but_not_closed_response_channel() {
    assert!(text_send_timeout_retryable("api 超时"));
    assert!(text_send_timeout_retryable(
        "Timeout: NTEvent serviceAndMethod:NodeIKernelMsgService/sendMsg"
    ));
    assert!(!text_send_timeout_retryable("api 响应通道关闭"));
    assert!(!text_send_timeout_retryable(
        "Timeout: NTEvent serviceAndMethod:NodeIKernelMsgService/getMsgs"
    ));
}

#[tokio::test]
async fn group_file_does_not_retry_when_response_channel_closes() {
    let nap = NapCat::new(NapCatConfig {
        enabled: true,
        ws_url: "ws://127.0.0.1:1".into(),
        ..NapCatConfig::default()
    })
    .unwrap();
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let responder_pending = pending.clone();
    let (api_tx, mut api_rx) = mpsc::channel::<String>(4);
    let responder = tokio::spawn(async move {
        let mut requests = Vec::new();
        while let Some(text) = api_rx.recv().await {
            let request: Value = serde_json::from_str(&text).unwrap();
            let echo = request["echo"].as_str().unwrap();
            let sender = responder_pending
                .lock()
                .unwrap()
                .remove(echo)
                .expect("请求应已注册 pending echo");
            drop(sender);
            requests.push(request);
        }
        requests
    });

    let error = nap
        .upload_file_ref(
            &api_tx,
            &pending,
            "group",
            "456",
            "/tmp/report.html",
            "分析报告.html",
        )
        .await
        .unwrap_err();
    assert_eq!(error, "api 响应通道关闭");
    drop(api_tx);

    let requests = responder.await.unwrap();
    assert_eq!(requests.len(), 1, "连接失效时不得在旧通道上重试");
    assert_eq!(requests[0]["action"], "upload_group_file");
    assert_eq!(requests[0]["params"]["group_id"], "456");
    assert_eq!(requests[0]["params"]["file"], "/tmp/report.html");
    assert_eq!(requests[0]["params"]["name"], "分析报告.html");
}

#[tokio::test]
async fn outbound_file_stops_without_cascade_on_ambiguous_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let media_dir = dir.path().join("media");
    let out_dir = media_dir.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let file = out_dir.join("report.md");
    std::fs::write(&file, b"report").unwrap();

    let nap = Arc::new(
        NapCat::new(NapCatConfig {
            enabled: true,
            ws_url: "ws://127.0.0.1:1".into(),
            media_dir: media_dir.clone(),
            ..NapCatConfig::default()
        })
        .unwrap(),
    );
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let responder_pending = pending.clone();
    let (api_tx, mut api_rx) = mpsc::channel::<String>(8);
    // NTEvent 超时的投递状态不确定：附件不得自动重发，也不得级联到 upload_file /
    // file 消息段（避免重复投递），但要发送一条可见的文本提示。
    let responder = tokio::spawn(async move {
        let mut actions = Vec::new();
        let mut notices = Vec::new();
        while let Some(text) = api_rx.recv().await {
            let request: Value = serde_json::from_str(&text).unwrap();
            let action = request["action"].as_str().unwrap().to_string();
            actions.push(action.clone());
            if action == "send_private_msg" {
                notices.push(
                    request["params"]["message"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                );
            }
            let echo = request["echo"].clone();
            let response = if action == "upload_file_stream"
                && request["params"]["is_complete"] == true
            {
                json!({
                    "status": "ok",
                    "retcode": 0,
                    "data": {"status": "file_complete", "file_path": "/tmp/report.md"},
                    "echo": echo,
                })
            } else if action == "upload_private_file" {
                json!({
                    "status": "failed",
                    "retcode": -1,
                    "wording": "Timeout: NTEvent serviceAndMethod:NodeIKernelMsgService/sendMsg",
                    "echo": echo,
                })
            } else {
                json!({"status": "ok", "retcode": 0, "data": {}, "echo": echo})
            };
            let sender = responder_pending
                .lock()
                .unwrap()
                .remove(response["echo"].as_str().unwrap())
                .expect("请求应已注册 pending echo");
            sender.send(response).unwrap();
        }
        (actions, notices)
    });

    let marker = format!(
        "[:napcat-media type=file path=\"{}\" name=\"report.md\"]",
        file.to_string_lossy()
    );
    // 单个附件失败被吞掉（仅告警），整条出站仍返回 Ok。
    let media_started = std::sync::atomic::AtomicBool::new(false);
    nap.send_outbound(
        &api_tx,
        &pending,
        BusMessage::new(CHANNEL_NAPCAT, "user:123", marker),
        &media_started,
    )
    .await
    .unwrap();
    drop(api_tx);
    assert!(
        media_started.load(std::sync::atomic::Ordering::Relaxed),
        "带附件的出站进入附件阶段后必须置位进度标记"
    );

    let (actions, notices) = responder.await.unwrap();
    // upload_file_stream ×2（1 个数据块 + is_complete），upload_private_file 只调用一次；
    // 之后停止，不出现 upload_file 回退或 send_msg file 段；send_private_msg 仅承载
    // “状态不确定、可要求重发”的文本提示。
    assert_eq!(
        actions,
        [
            "upload_file_stream",
            "upload_file_stream",
            "upload_private_file",
            "send_private_msg",
        ]
    );
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("发送确认超时"));
    assert!(notices[0].contains("重发附件"));
}

#[tokio::test]
async fn private_file_does_not_resend_on_ambiguous_nt_event_timeout() {
    let nap = NapCat::new(NapCatConfig {
        enabled: true,
        ws_url: "ws://127.0.0.1:1".into(),
        ..NapCatConfig::default()
    })
    .unwrap();
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let responder_pending = pending.clone();
    let (api_tx, mut api_rx) = mpsc::channel::<String>(4);
    // upload_private_file 返回 NTEvent 超时（投递状态不确定）；不得自动进行第二次投递。
    let responder = tokio::spawn(async move {
        let mut requests = Vec::new();
        while let Some(text) = api_rx.recv().await {
            let request: Value = serde_json::from_str(&text).unwrap();
            let echo = request["echo"].clone();
            let response = json!({
                "status": "failed",
                "retcode": -1,
                "wording": "Timeout: NTEvent serviceAndMethod:NodeIKernelMsgService/sendMsg",
                "echo": echo,
            });
            let sender = responder_pending
                .lock()
                .unwrap()
                .remove(response["echo"].as_str().unwrap())
                .expect("请求应已注册 pending echo");
            sender.send(response).unwrap();
            requests.push(request);
        }
        requests
    });

    let error = nap
        .upload_file_ref(
            &api_tx,
            &pending,
            "private",
            "123",
            "/tmp/report.html",
            "分析报告.html",
        )
        .await
        .unwrap_err();
    assert!(error.contains("NodeIKernelMsgService/sendMsg"));
    drop(api_tx);

    let requests = responder.await.unwrap();
    assert_eq!(requests.len(), 1, "状态不明的附件不得自动重发");
    assert!(requests
        .iter()
        .all(|request| request["action"] == "upload_private_file"));
    assert!(requests
        .iter()
        .all(|request| request["params"]["user_id"] == "123"));
}

#[test]
fn sanitize_drops_separators() {
    assert_eq!(sanitize_filename("报价/Q1.pdf"), "报价_Q1.pdf");
    assert_eq!(sanitize_filename(""), "media");
    assert_eq!(sanitize_filename("   "), "media");
    let s = sanitize_filename("../../etc");
    assert!(!s.contains('/') && !s.is_empty());
}

#[test]
fn parse_markers_basic_and_caption() {
    let content =
        "这是说明\n[:napcat-media type=file path=agent/media/out/r.html caption=报告]\n收尾";
    let (intents, text) = parse_media_markers(content);
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].kind, MediaKind::File);
    assert_eq!(intents[0].path, "agent/media/out/r.html");
    assert_eq!(intents[0].caption.as_deref(), Some("报告"));
    assert!(text.contains("这是说明"));
    assert!(text.contains("收尾"));
    assert!(!text.contains("[:napcat-media"));
}

#[test]
fn parse_markers_multiple_and_quoted() {
    let content = "[:napcat-media type=image path=agent/a.png]\n[:napcat-media type=video path=\"agent/v 1.mp4\" name=\"clip\"]";
    let (intents, text) = parse_media_markers(content);
    assert_eq!(intents.len(), 2);
    assert_eq!(intents[0].kind, MediaKind::Image);
    assert_eq!(intents[1].kind, MediaKind::Video);
    assert_eq!(intents[1].path, "agent/v 1.mp4");
    assert!(text.trim().is_empty());
}

#[test]
fn parse_markers_malformed_kept_as_text() {
    // 缺 type / path → 解析失败，原样保留为可见文本。
    let (intents, text) = parse_media_markers("[:napcat-media path=only]");
    assert!(intents.is_empty());
    assert!(text.contains("[:napcat-media"));
}

#[test]
fn parse_markers_rejects_forged_marker_name_prefix() {
    let forged = "[:napcat-media-forged type=file path=agent/media/out/report.md]";
    let (intents, text) = parse_media_markers(forged);
    assert!(intents.is_empty());
    assert_eq!(text, forged);
}

#[test]
fn parse_markers_preserve_trailing_text() {
    let (intents, text) =
        parse_media_markers("[:napcat-media type=image path=agent/media/out/a.png] 收工");
    assert_eq!(intents.len(), 1);
    assert_eq!(text.trim(), "收工");
}

/// build_media_segment 仅对沙箱内路径产出可发送媒体段；沙箱外一律 None。
#[test]
fn build_media_segment_sandbox_and_base64() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("agent").join("media");
    std::fs::create_dir_all(base.join("out")).unwrap();
    let file = base.join("out").join("r.html");
    std::fs::write(&file, b"<html/>").unwrap();

    let intent = MediaIntent {
        kind: MediaKind::File,
        path: file.to_string_lossy().into_owned(),
        caption: None,
        name: Some("r.html".into()),
    };
    let seg = build_media_segment(&intent, &base).expect("沙箱内应通过");
    assert_eq!(seg["type"], "file");
    let file_ref = seg["data"]["file"].as_str().unwrap();
    assert!(
        file_ref.starts_with("base64://"),
        "小文件应以内联 base64 发送: {file_ref}"
    );
    assert_eq!(seg["data"]["name"], "r.html");

    // 图片段不带 name。
    let img = build_media_segment(
        &MediaIntent {
            kind: MediaKind::Image,
            path: file.to_string_lossy().into_owned(),
            caption: None,
            name: None,
        },
        &base,
    )
    .unwrap();
    assert_eq!(img["type"], "image");
    assert!(img["data"].get("name").is_none());

    // 沙箱外拒绝。
    let outside = dir.path().join("secret.txt");
    std::fs::write(&outside, b"x").unwrap();
    assert!(build_media_segment(
        &MediaIntent {
            kind: MediaKind::File,
            path: outside.to_string_lossy().into_owned(),
            caption: None,
            name: None,
        },
        &base,
    )
    .is_none());

    // 空沙箱一律拒绝。
    assert!(build_media_segment(
        &MediaIntent {
            kind: MediaKind::File,
            path: "anything".into(),
            caption: None,
            name: None,
        },
        &PathBuf::new(),
    )
    .is_none());
}

#[test]
fn prepare_outbound_small_media_keeps_local_ref_until_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("agent").join("media");
    std::fs::create_dir_all(base.join("out")).unwrap();
    let file = base.join("out").join("small.txt");
    std::fs::write(&file, b"hello").unwrap();

    let intent = MediaIntent {
        kind: MediaKind::Image,
        path: file.to_string_lossy().into_owned(),
        caption: None,
        name: None,
    };
    let prepared = prepare_outbound_media(&intent, &base).expect("沙箱内小文件应通过");
    assert!(
        prepared.file_ref.starts_with("file:///"),
        "Stream 优先时准备阶段只保留本地引用: {}",
        prepared.file_ref
    );
    assert!(
        fallback_file_ref(&prepared).starts_with("base64://"),
        "只有普通回退才应生成 base64"
    );
}

#[test]
fn prepare_outbound_large_media_keeps_path_for_stream_upload() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("agent").join("media");
    std::fs::create_dir_all(base.join("out")).unwrap();
    let file = base.join("out").join("large.bin");
    let f = std::fs::File::create(&file).unwrap();
    f.set_len(OUTBOUND_BASE64_MAX_BYTES + 1).unwrap();

    let intent = MediaIntent {
        kind: MediaKind::File,
        path: file.to_string_lossy().into_owned(),
        caption: None,
        name: Some("large.bin".into()),
    };
    let prepared = prepare_outbound_media(&intent, &base).expect("沙箱内大文件应通过");
    assert_eq!(prepared.size, OUTBOUND_BASE64_MAX_BYTES + 1);
    assert_eq!(prepared.name, "large.bin");
    assert_eq!(prepared.path.file_name().unwrap(), "large.bin");
    assert!(
        prepared.file_ref.starts_with("file:///"),
        "大文件普通兜底仍是 file://，实际发送优先 upload_file_stream: {}",
        prepared.file_ref
    );
}

#[test]
fn stream_download_state_collects_chunks_after_complete() {
    let mut state = StreamDownloadState::default();
    state
        .ingest(&json!({
            "type": "stream",
            "data_type": "file_info",
            "file_name": "report.txt",
            "file_size": 10,
            "chunk_size": 5
        }))
        .unwrap();
    state
        .ingest(&json!({
            "type": "response",
            "data_type": "file_complete",
            "total_chunks": 2,
            "total_bytes": 10
        }))
        .unwrap();
    assert!(state.try_finish().unwrap().is_none());

    state
        .ingest(&json!({
            "type": "stream",
            "data_type": "file_chunk",
            "index": 1,
            "data": base64::engine::general_purpose::STANDARD.encode(b"world")
        }))
        .unwrap();
    assert!(state.try_finish().unwrap().is_none());

    state
        .ingest(&json!({
            "type": "stream",
            "data_type": "file_chunk",
            "index": 0,
            "data": base64::engine::general_purpose::STANDARD.encode(b"hello")
        }))
        .unwrap();
    let result = state.try_finish().unwrap().expect("两块齐后应完成");
    assert_eq!(result.name.as_deref(), Some("report.txt"));
    assert_eq!(result.bytes, b"helloworld");
}

#[test]
fn stream_packet_detection_matches_napcat_shape() {
    assert!(is_stream_packet(&json!({
        "status": "ok",
        "retcode": 0,
        "data": { "type": "stream", "data_type": "file_chunk" },
        "echo": "e",
        "stream": "stream-action"
    })));
    assert!(is_stream_packet(&json!({
        "status": "ok",
        "retcode": 0,
        "data": { "type": "response", "data_type": "file_complete" },
        "echo": "e"
    })));
    assert!(!is_stream_packet(&json!({
        "status": "ok",
        "retcode": 0,
        "data": { "online": true },
        "echo": "e"
    })));
}

#[test]
fn stream_download_policy_prefers_http_direct_links() {
    assert!(is_http_url("https://example.com/a.zip"));
    assert!(!should_try_stream_before_local(
        MediaKind::File,
        "https://example.com/a.zip"
    ));
    assert!(!should_try_stream_before_local(
        MediaKind::Video,
        "https://example.com/a.mp4"
    ));
    assert!(should_try_stream_before_local(
        MediaKind::File,
        "file:///tmp/a.zip"
    ));
    assert!(should_try_stream_before_local(
        MediaKind::Video,
        "file:///tmp/a.mp4"
    ));
    assert!(!should_try_stream_before_local(
        MediaKind::Image,
        "file:///tmp/a.png"
    ));
    assert!(should_try_stream_after_http_failure(MediaKind::Image));
    assert!(should_try_stream_after_http_failure(MediaKind::File));
    assert!(should_try_stream_by_file_id(MediaKind::Video));
    assert!(!should_try_stream_by_file_id(MediaKind::Voice));
}

#[tokio::test]
async fn media_inflight_waiters_receive_download_result() {
    let cfg = NapCatConfig {
        enabled: true,
        ws_url: "ws://127.0.0.1:3001".into(),
        ..NapCatConfig::default()
    };
    let nap = NapCat::new(cfg).unwrap();
    let keys = vec!["group:1:file:file_id:fid".to_string()];
    assert!(matches!(
        nap.join_or_start_media_download(&keys),
        MediaDownloadSlot::Leader
    ));
    let rx = match nap.join_or_start_media_download(&keys) {
        MediaDownloadSlot::Follower(rx) => rx,
        _ => panic!("第二个同 key 下载应等待第一个"),
    };
    let cached = CachedMedia {
        kind: MediaKind::File,
        name: Some("a.txt".into()),
        path: PathBuf::from("agent/media/napcat/a.txt"),
    };
    nap.finish_media_download(&keys, Some(cached.clone()));
    let got = rx.await.unwrap().unwrap();
    assert_eq!(got.path, cached.path);
}

#[test]
fn local_file_url_strips_windows_verbatim_prefix() {
    let path = strip_verbatim_prefix(PathBuf::from(r"\\?\C:\agent\media\out\r.html"));
    let url = local_file_url(&path);
    assert!(url.starts_with("file:///"), "file:// 前缀: {url}");
    assert!(
        !url.contains("//?/"),
        "file:// URL 不得带 verbatim 前缀: {url}"
    );
    assert!(!url.contains("\\\\"), "file:// URL 不得含反斜杠: {url}");
}

#[test]
fn resolve_media_path_enforces_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("agent").join("media");
    std::fs::create_dir_all(base.join("out")).unwrap();
    let file = base.join("out").join("x.pdf");
    std::fs::write(&file, b"x").unwrap();
    let file_s = file.to_string_lossy();
    assert!(resolve_media_path(&base, &file_s).is_some());
    assert!(resolve_media_path(&base, &file_s.replace('/', "\\")).is_some());
    // 尚不存在的子路径也允许（便于后续读取前由调用方得到明确的 IO 错误）。
    let future = base.join("out").join("future.pdf");
    assert!(resolve_media_path(&base, &future.to_string_lossy()).is_some());
    // 穿越 / 越界 / 兄弟目录前缀一律拒绝。
    let traversal = base.join("..").join("secret");
    assert!(resolve_media_path(&base, &traversal.to_string_lossy()).is_none());
    let outside = dir.path().join("outside.txt");
    assert!(resolve_media_path(&base, &outside.to_string_lossy()).is_none());
    let sibling = dir.path().join("agent").join("media2").join("x");
    assert!(resolve_media_path(&base, &sibling.to_string_lossy()).is_none());
    // 空沙箱 → 一律拒绝。
    assert!(resolve_media_path(&PathBuf::new(), "anything").is_none());
}

#[test]
fn resolve_media_path_accepts_cwd_relative_agent_media_path() {
    let cwd = std::env::current_dir().unwrap();
    let dir = tempfile::tempdir_in(&cwd).unwrap();
    let relative_root = dir.path().strip_prefix(&cwd).unwrap();
    let base = dir.path().join("agent").join("media");
    std::fs::create_dir_all(base.join("napcat")).unwrap();
    let file = base.join("napcat").join("x.txt");
    std::fs::write(&file, b"x").unwrap();

    let candidate = relative_root
        .join("agent")
        .join("media")
        .join("napcat")
        .join("x.txt");
    let resolved = resolve_media_path(&base, &candidate.to_string_lossy())
        .expect("cwd 相对 agent/media 路径应通过");
    assert_eq!(resolved, file.canonicalize().unwrap());
}

#[test]
fn resolve_media_path_rejects_symlink_escape() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("agent").join("media");
    std::fs::create_dir_all(&base).unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let link = base.join("link");
    if create_dir_symlink(&outside, &link).is_err() {
        // Windows 未开启开发者模式/管理员时无法创建符号链接，跳过。
        return;
    }
    let via_link = link.join("secret.txt");
    assert!(
        resolve_media_path(&base, &via_link.to_string_lossy()).is_none(),
        "经逃逸链接访问沙箱外路径必须被拒绝"
    );
}

#[cfg(unix)]
fn create_dir_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn create_dir_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(src, dst)
}

#[test]
fn media_kind_from_str_and_labels() {
    assert_eq!(MediaKind::from_str("image"), Some(MediaKind::Image));
    assert_eq!(MediaKind::from_str("voice"), Some(MediaKind::Voice));
    assert_eq!(MediaKind::from_str("video"), Some(MediaKind::Video));
    assert_eq!(MediaKind::from_str("file"), Some(MediaKind::File));
    assert_eq!(MediaKind::from_str("unknown"), None);
    assert_eq!(MediaKind::File.label(), "文件");
    assert!(MediaKind::Image.default_name().ends_with(".png"));
}

#[test]
fn image_signature_detection_covers_common_qq_formats() {
    assert_eq!(
        detected_image_extension(b"\xff\xd8\xff\xe0JFIF"),
        Some("jpg")
    );
    assert_eq!(
        detected_image_extension(b"\x89PNG\r\n\x1a\nmore"),
        Some("png")
    );
    assert_eq!(detected_image_extension(b"GIF89amore"), Some("gif"));
    assert_eq!(
        detected_image_extension(b"RIFF\x00\x00\x00\x00WEBPmore"),
        Some("webp")
    );
    assert_eq!(detected_image_extension(b"not-an-image"), None);
}

#[test]
fn downloaded_jpeg_with_png_name_is_renamed_from_signature() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("image.png");
    std::fs::write(&original, b"\xff\xd8\xff\xe0JFIF-test").unwrap();

    let corrected = normalize_downloaded_media_extension(MediaKind::Image, &original).unwrap();

    assert_eq!(
        corrected.extension().and_then(|value| value.to_str()),
        Some("jpg")
    );
    assert!(corrected.is_file());
    assert!(!original.exists());
}
