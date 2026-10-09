use super::*;

#[test]
fn utf8_chunks_never_split_char() {
    let s = "你好世界，这是一段用来测试中文字符切分的文本".repeat(3);
    for chunk in utf8_chunks(&s, 20) {
        // 每段都必须是合法 UTF-8（String::from_utf8 不 panic 即可，这里直接用 str）。
        assert!(std::str::from_utf8(chunk.as_bytes()).is_ok());
        assert!(chunk.len() <= 20);
    }
}

/// max_bytes=0 不得导致死循环：应整段返回一个非空切片。
#[test]
fn utf8_chunks_zero_max_no_loop() {
    let s = "abc";
    let chunks = utf8_chunks(s, 0);
    assert_eq!(chunks, vec!["abc"]);
}

/// P2：max_bytes 小于单个 CJK 字符字节数（如 chunk_bytes=1 遇到 3 字节中文）时，
/// 不得死循环；应把每个超限字符单独成段（虽超过 max_bytes，但保证 start 推进），
/// 每段是合法字符，内容不丢。
#[test]
fn utf8_chunks_tiny_max_no_loop() {
    let s = "你好"; // 6 字节，每个中文 3 字节
    let chunks = utf8_chunks(s, 1);
    assert_eq!(chunks.len(), 2, "每个超限字符应单独成段");
    assert_eq!(chunks.concat(), s, "内容不得丢失");
    for c in &chunks {
        assert!(
            std::str::from_utf8(c.as_bytes()).is_ok(),
            "每段必须是合法 UTF-8"
        );
    }
}

#[test]
fn classifies_text_and_ref() {
    let msg = json!({
        "item_list":[
            {"type":1,"text_item":{"text":"hello"}},
            {"type":1,"ref_msg":{"title":"原消息"}}
        ]
    });
    let (text, media) = classify_items(&msg);
    assert!(text.contains("hello"));
    assert!(text.contains("原消息"));
    assert!(media.is_empty());
}

#[test]
fn classifies_file_image_voice() {
    let msg = json!({
        "item_list":[
            {"type":4,"file_item":{"media":{"encrypt_query_param":"AA==","aes_key":"KAA="},"file_name":"a.pdf"}},
            {"type":2,"image_item":{"media":{"encrypt_query_param":"BB==","aes_key":"KAA="},"aeskey":"00112233445566778899aabbccddeeff"}},
            {"type":3,"voice_item":{"text":"你好"}}
        ]
    });
    let (text, media) = classify_items(&msg);
    // 语音作为文本（转写），不进 media。
    assert!(text.contains("[语音] 你好"));
    assert_eq!(media.len(), 2);
    assert_eq!(media[0].kind, MediaKind::File);
    assert_eq!(media[0].encrypt_query_param, "AA==");
    assert_eq!(media[0].file_name.as_deref(), Some("a.pdf"));
    assert_eq!(media[1].kind, MediaKind::Image);
    assert_eq!(
        media[1].direct_aeskey.as_deref(),
        Some("00112233445566778899aabbccddeeff")
    );
}

#[test]
fn aes_ecb_roundtrip_various_lengths() {
    let key = [7u8; 16];
    for &n in &[0usize, 1, 15, 16, 17, 100, 256] {
        let plain: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        let cipher = aes_ecb_encrypt(&plain, &key);
        assert_eq!(cipher.len() % 16, 0, "密文必须是 16 的倍数");
        assert!(cipher.len() > plain.len() || n == 0 || n % 16 == 0);
        let back = aes_ecb_decrypt(&cipher, &key).expect("解密成功");
        assert_eq!(back, plain, "往返一致 n={n}");
    }
}

#[test]
fn aes_decrypt_rejects_bad_ciphertext() {
    let key = [7u8; 16];
    // 长度非 16 倍数。
    assert!(aes_ecb_decrypt(&[0u8; 15], &key).is_none());
    // 解出来的填充不合法（全 0，pad=0）。
    assert!(aes_ecb_decrypt(&[0u8; 16], &key).is_none());
}

#[test]
fn decode_aes_key_both_formats() {
    let raw: [u8; 16] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    // 格式 A：base64(原始16字节)。
    let a = base64::engine::general_purpose::STANDARD.encode(raw);
    assert_eq!(decode_media_aes_key(&a), Some(raw));
    // 格式 B：base64(hex 串)。
    let hex_str = hex::encode(raw);
    let b = base64::engine::general_purpose::STANDARD.encode(hex_str.as_bytes());
    assert_eq!(decode_media_aes_key(&b), Some(raw));
    // 直给 hex。
    assert_eq!(decode_hex_key(&hex_str), Some(raw));
}

#[test]
fn sanitize_drops_separators() {
    // 分隔符→_，扩展名保留；不含路径分隔符即安全（目录穿越由 dir.join 固定前缀兜住）。
    assert_eq!(sanitize_filename("报价/Q1.pdf"), "报价_Q1.pdf");
    let s = sanitize_filename("../../etc");
    assert!(!s.contains('/') && !s.is_empty());
    assert_eq!(sanitize_filename(""), "media");
    assert_eq!(sanitize_filename("   "), "media");
}

#[test]
fn parse_markers_basic_and_caption() {
    let content =
        "这是说明\n[:weixin-media type=file path=agent/media/out/x.pdf caption=报价单]\n收尾";
    let (intents, text) = parse_media_markers(content);
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].kind, MediaKind::File);
    assert_eq!(intents[0].path, "agent/media/out/x.pdf");
    assert_eq!(intents[0].caption.as_deref(), Some("报价单"));
    // 标记行被剥离，其余文本保留。
    assert!(text.contains("这是说明"));
    assert!(text.contains("收尾"));
    assert!(!text.contains("[:weixin-media"));
}

#[test]
fn parse_markers_multiple_and_quoted() {
    let content = "[:weixin-media type=image path=agent/a.png]\n[:weixin-media type=video path=\"agent/v 1.mp4\"]";
    let (intents, text) = parse_media_markers(content);
    assert_eq!(intents.len(), 2);
    assert_eq!(intents[0].kind, MediaKind::Image);
    assert_eq!(intents[1].kind, MediaKind::Video);
    assert_eq!(intents[1].path, "agent/v 1.mp4");
    assert!(text.trim().is_empty(), "无剩余可见文本");
}

#[test]
fn parse_markers_malformed_kept_as_text() {
    // 缺 type / path → 解析失败，原样保留为可见文本。
    let content = "[:weixin-media path=only]";
    let (intents, text) = parse_media_markers(content);
    assert!(intents.is_empty());
    assert!(text.contains("[:weixin-media"));
}

#[test]
fn parse_markers_rejects_forged_marker_name_prefix() {
    let forged = "[:weixin-media-forged type=file path=agent/media/out/report.md]";
    let (intents, text) = parse_media_markers(forged);
    assert!(intents.is_empty());
    assert_eq!(text, forged);
}

#[test]
fn parse_markers_preserve_trailing_text() {
    // 标记 `]` 之后的同行尾巴文本不得丢失。
    let (intents, text) =
        parse_media_markers("[:weixin-media type=image path=agent/media/out/a.png] 收工");
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].kind, MediaKind::Image);
    assert_eq!(text.trim(), "收工");
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
    // 尚不存在的子路径也允许，便于后续读取前由调用方得到明确的 IO 错误。
    let future = base.join("out").join("future.pdf");
    assert!(resolve_media_path(&base, &future.to_string_lossy()).is_some());
    // 穿越 / 越界 / 兄弟目录前缀一律拒绝。
    let traversal = base.join("..").join("secret");
    assert!(resolve_media_path(&base, &traversal.to_string_lossy()).is_none());
    let outside = dir.path().join("outside.txt");
    assert!(resolve_media_path(&base, &outside.to_string_lossy()).is_none());
    let sibling = dir.path().join("agent").join("media2").join("x");
    assert!(resolve_media_path(&base, &sibling.to_string_lossy()).is_none());
    // 空沙箱 → 一律拒绝（防空前缀放行任意路径）。
    assert!(resolve_media_path(&PathBuf::new(), "anything").is_none());
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
        // Windows without Developer Mode/Admin may not allow symlink creation.
        return;
    }
    let via_link = link.join("secret.txt");
    assert!(
        resolve_media_path(&base, &via_link.to_string_lossy()).is_none(),
        "经逃逸链接访问沙箱外路径必须被拒绝"
    );
}

#[test]
fn resolve_media_path_returns_canonical_path_for_safe_link() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("agent").join("media");
    let real = base.join("real");
    std::fs::create_dir_all(&real).unwrap();
    let file = real.join("x.txt");
    std::fs::write(&file, b"x").unwrap();
    let link = base.join("link");
    if create_dir_symlink(&real, &link).is_err() {
        // Windows without Developer Mode/Admin may not allow symlink creation.
        return;
    }
    let via_link = link.join("x.txt");
    let resolved = resolve_media_path(&base, &via_link.to_string_lossy()).unwrap();
    assert_eq!(resolved, file.canonicalize().unwrap());
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
fn build_send_body_carries_context_token() {
    let body = build_send_body("u@im.wechat", "hi", "TOK");
    assert_eq!(body["msg"]["context_token"], "TOK");
    assert_eq!(body["msg"]["to_user_id"], "u@im.wechat");
    assert_eq!(body["msg"]["item_list"][0]["type"], 1);
}

#[test]
fn build_media_item_shapes() {
    let f = build_media_item(MediaKind::File, "EQP", "AK", 1234, "a.pdf");
    assert_eq!(f["type"], 4);
    assert_eq!(f["file_item"]["media"]["encrypt_query_param"], "EQP");
    assert_eq!(f["file_item"]["file_name"], "a.pdf");
    let i = build_media_item(MediaKind::Image, "EQP", "AK", 1234, "a.png");
    assert_eq!(i["type"], 2);
    assert_eq!(i["image_item"]["mid_size"], 1234);
    let v = build_media_item(MediaKind::Video, "EQP", "AK", 1234, "a.mp4");
    assert_eq!(v["type"], 5);
    assert_eq!(v["video_item"]["video_size"], 1234);
}

/// 上下文 token 缓存：写入后可读回；空 token 不写入。
#[test]
fn context_token_cache_roundtrip() {
    let cfg = WeixinConfig {
        enabled: true,
        token: "t".into(),
        ..WeixinConfig::default()
    };
    let wx = Weixin::new(cfg).unwrap();
    assert_eq!(wx.context_token("u"), "");
    wx.remember_context_token("u", "TOK");
    assert_eq!(wx.context_token("u"), "TOK");
    // 覆盖最新。
    wx.remember_context_token("u", "TOK2");
    assert_eq!(wx.context_token("u"), "TOK2");
    // 空 token 不覆盖已有。
    wx.remember_context_token("u", "");
    assert_eq!(wx.context_token("u"), "TOK2");
}
