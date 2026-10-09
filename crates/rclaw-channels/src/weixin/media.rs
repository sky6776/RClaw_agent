use super::*;

// ---------------------------------------------------------------------------
// 出站媒体意图（标记协议）
// ---------------------------------------------------------------------------

/// 出站媒体种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MediaKind {
    File,
    Image,
    Video,
    Voice,
}

impl MediaKind {
    /// 标注里展示的中文名。
    pub(super) fn label(self) -> &'static str {
        match self {
            MediaKind::File => "文件",
            MediaKind::Image => "图片",
            MediaKind::Video => "视频",
            MediaKind::Voice => "语音",
        }
    }

    /// `getuploadurl.media_type`：1=图片、2=视频、3=文件、4=语音。
    pub(super) fn upload_media_type(self) -> u8 {
        match self {
            MediaKind::Image => 1,
            MediaKind::Video => 2,
            MediaKind::File => 3,
            MediaKind::Voice => 4,
        }
    }

    /// `MessageItem.type`：1=文本、2=图片、3=语音、4=文件、5=视频。
    pub(super) fn item_type(self) -> u8 {
        match self {
            MediaKind::Image => 2,
            MediaKind::Voice => 3,
            MediaKind::File => 4,
            MediaKind::Video => 5,
        }
    }

    pub(super) fn from_str(s: &str) -> Option<Self> {
        match s {
            "file" => Some(MediaKind::File),
            "image" => Some(MediaKind::Image),
            "video" => Some(MediaKind::Video),
            "voice" => Some(MediaKind::Voice),
            _ => None,
        }
    }
}

/// 出站媒体发送意图（由回复正文里的标记行解析而来）。
#[derive(Debug, Clone)]
pub(super) struct MediaIntent {
    pub(super) kind: MediaKind,
    /// 本地可读文件绝对路径（agent/ 前缀，由 write 落盘）。
    pub(super) path: String,
    /// 可选：作为前置文本单独发送的说明。
    pub(super) caption: Option<String>,
}

/// 从回复正文解析媒体标记，返回（媒体意图, 剥离标记后的可见文本）。
///
/// 标记行形如（独占一行）：
/// `[:weixin-media type=file path=agent/media/out/x.pdf caption=这是报价单]`
/// `caption` 可选；`path`/`type` 必填。值可用双引号包裹以含空格。
/// 解析失败的标记行原样保留为可见文本（便于发现格式问题）。
pub(super) fn parse_media_markers(content: &str) -> (Vec<MediaIntent>, String) {
    let mut intents = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    for line in content.split('\n') {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("[:weixin-media") {
            // marker 名必须在 `weixin-media` 后立即结束。没有边界检查时，
            // `[:weixin-media-forged ...]` 也会进入属性解析并被当成真实发送意图，
            // 从而绕过运行时只信任发布工具回执的约束。
            if !rest.starts_with(char::is_whitespace) {
                kept.push(line.to_string());
                continue;
            }
            if let Some(end) = rest.find(']') {
                if let Some(i) = parse_intent_inner(&rest[..end]) {
                    intents.push(i);
                    // 标记 `]` 之后的同行剩余文本保留为可见文本（标记独占一行是约定，但别丢尾巴）。
                    let trailing = rest[end + 1..].trim_start();
                    if !trailing.is_empty() {
                        kept.push(trailing.to_string());
                    }
                    continue;
                }
            }
        }
        kept.push(line.to_string());
    }
    (intents, kept.join("\n"))
}

/// 解析标记内部 `type=.. path=.. caption=..` 文本为意图（值可用双引号含空格）。
pub(super) fn parse_intent_inner(inner: &str) -> Option<MediaIntent> {
    let mut kind: Option<MediaKind> = None;
    let mut path: Option<String> = None;
    let mut caption: Option<String> = None;
    for (k, v) in tokenize_kv(inner) {
        match k.as_str() {
            "type" => kind = MediaKind::from_str(&v),
            "path" => path = Some(v),
            "caption" => caption = Some(v),
            _ => {}
        }
    }
    Some(MediaIntent {
        kind: kind?,
        path: path.filter(|s| !s.is_empty())?,
        caption: caption.filter(|s| !s.is_empty()),
    })
}

/// 按 `k=v` 切分标记内部文本：双引号内的空格不切分（引号本身不进入值）。
pub(super) fn tokenize_kv(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                push_kv(&mut out, &cur);
                cur.clear();
            }
            c => cur.push(c),
        }
    }
    push_kv(&mut out, &cur);
    out
}

pub(super) fn push_kv(out: &mut Vec<(String, String)>, tok: &str) {
    if tok.is_empty() {
        return;
    }
    if let Some((k, v)) = tok.split_once('=') {
        out.push((k.to_string(), v.trim().to_string()));
    }
}

// ---------------------------------------------------------------------------
// 入站媒体分类
// ---------------------------------------------------------------------------

/// 入站媒体引用（CDN 定位 + 解密信息）。
#[derive(Debug, Clone)]
pub(super) struct InboundMedia {
    pub(super) kind: MediaKind,
    pub(super) encrypt_query_param: String,
    /// `media.aes_key`：base64(原始16字节) 或 base64(hex串) 两种编码（协议 §8.4）。
    pub(super) aes_key_field: Option<String>,
    /// 图片有时直给的 32 位 hex `aeskey`，优先用它解密。
    pub(super) direct_aeskey: Option<String>,
    pub(super) file_name: Option<String>,
    /// 语音转写文本。
    pub(super) voice_text: Option<String>,
}

/// 从 `item_list` 抽出（拼接文本, 媒体引用列表）。纯解析、无 IO。
pub(super) fn classify_items(msg: &Value) -> (String, Vec<InboundMedia>) {
    let mut text_parts: Vec<String> = Vec::new();
    let mut media: Vec<InboundMedia> = Vec::new();

    let Some(items) = msg.get("item_list").and_then(|v| v.as_array()) else {
        return (String::new(), media);
    };
    for item in items {
        let itype = item.get("type").and_then(|v| v.as_i64()).unwrap_or(0);
        match itype {
            1 => {
                if let Some(t) = item
                    .get("text_item")
                    .and_then(|x| x.get("text"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    text_parts.push(t.to_string());
                }
            }
            3 => {
                // 语音：直接用服务端转写文本，作为「文本」处理（不下载 SILK 原始流）。
                // 不产生 media 记录，避免与文本标注重复。
                if let Some(t) = item
                    .get("voice_item")
                    .and_then(|x| x.get("text"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    text_parts.push(format!("[语音] {t}"));
                }
            }
            2 | 4 | 5 => {
                let kind = match itype {
                    2 => MediaKind::Image,
                    4 => MediaKind::File,
                    _ => MediaKind::Video,
                };
                let sub = match itype {
                    2 => "image_item",
                    4 => "file_item",
                    _ => "video_item",
                };
                if let Some(obj) = item.get(sub) {
                    let (eqp, aes_key_field) = media_ref(obj);
                    let direct = if kind == MediaKind::Image {
                        obj.get("aeskey")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    } else {
                        None
                    };
                    media.push(InboundMedia {
                        kind,
                        encrypt_query_param: eqp,
                        aes_key_field,
                        direct_aeskey: direct,
                        file_name: obj
                            .get("file_name")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        voice_text: None,
                    });
                }
            }
            _ => {}
        }
        // 引用消息：把被引用摘要也拼进文本（保持旧行为）。
        if let Some(title) = item
            .get("ref_msg")
            .and_then(|x| x.get("title"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            text_parts.push(format!("[引用] {title}"));
        }
    }
    (text_parts.join("\n"), media)
}

/// 从媒体子对象取 `(encrypt_query_param, aes_key)`。
pub(super) fn media_ref(obj: &Value) -> (String, Option<String>) {
    let media = obj.get("media");
    let eqp = media
        .and_then(|m| m.get("encrypt_query_param"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let aes_key = media
        .and_then(|m| m.get("aes_key"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    (eqp, aes_key)
}

// ---------------------------------------------------------------------------
// 加解密 + 摘要辅助（AES-128-ECB + PKCS7）
// ---------------------------------------------------------------------------

/// AES-128-ECB + PKCS7 加密，返回密文（长度恒为 16 的倍数）。
pub(super) fn aes_ecb_encrypt(plain: &[u8], key: &[u8; 16]) -> Vec<u8> {
    use cipher::{BlockEncrypt, KeyInit};
    use generic_array::GenericArray;

    let cipher = aes::Aes128::new(GenericArray::from_slice(key));
    // PKCS7：pad = 16 - len%16，恒为 1..=16（满块时补整块）。
    let pad = 16 - (plain.len() % 16);
    let mut buf = Vec::with_capacity(plain.len() + pad);
    buf.extend_from_slice(plain);
    buf.extend(std::iter::repeat_n(pad as u8, pad));
    for chunk in buf.chunks_exact_mut(16) {
        let block = GenericArray::from_mut_slice(chunk);
        cipher.encrypt_block(block);
    }
    buf
}

/// AES-128-ECB + PKCS7 解密。密文长度非 16 倍数或填充非法时返回 `None`。
pub(super) fn aes_ecb_decrypt(cipher_bytes: &[u8], key: &[u8; 16]) -> Option<Vec<u8>> {
    use cipher::{BlockDecrypt, KeyInit};
    use generic_array::GenericArray;

    if cipher_bytes.is_empty() || !cipher_bytes.len().is_multiple_of(16) {
        return None;
    }
    let cipher = aes::Aes128::new(GenericArray::from_slice(key));
    let mut buf = cipher_bytes.to_vec();
    for chunk in buf.chunks_exact_mut(16) {
        let block = GenericArray::from_mut_slice(chunk);
        cipher.decrypt_block(block);
    }
    // 校验并剥离 PKCS7 填充。
    let pad = *buf.last()? as usize;
    if pad == 0 || pad > 16 || pad > buf.len() {
        return None;
    }
    if !buf[buf.len() - pad..].iter().all(|&b| b as usize == pad) {
        return None;
    }
    buf.truncate(buf.len() - pad);
    Some(buf)
}

/// 把 `media.aes_key` 解码为 16 字节 AES key（兼容两种编码，协议 §8.4）。
pub(super) fn decode_media_aes_key(field: &str) -> Option<[u8; 16]> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(field)
        .ok()?;
    if raw.len() == 16 {
        let mut k = [0u8; 16];
        k.copy_from_slice(&raw);
        return Some(k);
    }
    if raw.len() == 32 {
        // base64(hex 串)：32 个 ASCII hex 字节 → 再 hex 解码成 16 字节。
        if let Ok(s) = std::str::from_utf8(&raw) {
            if let Some(b) = hex::decode(s).ok().filter(|b| b.len() == 16) {
                let mut k = [0u8; 16];
                k.copy_from_slice(&b);
                return Some(k);
            }
        }
    }
    None
}

/// 把图片直给的 32 位 hex `aeskey` 解码为 16 字节 AES key。
pub(super) fn decode_hex_key(s: &str) -> Option<[u8; 16]> {
    let b = hex::decode(s).ok()?;
    if b.len() == 16 {
        let mut k = [0u8; 16];
        k.copy_from_slice(&b);
        Some(k)
    } else {
        None
    }
}

/// 把文件名里的路径分隔符 / 控制字符替换为 `_`，避免落盘越界。
pub(super) fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'..='\u{1F}' => '_',
            _ => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "media".into()
    } else {
        trimmed.into()
    }
}

/// 取路径的文件名部分（失败回退到整段），用于出站 `file_name` 字段。
pub(super) fn path_file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// 出站媒体路径沙箱校验：`candidate` 必须落在 `media_dir` 之内。
///
/// 先做词法前缀与 `..` 检查，挡住明显越界和兄弟目录前缀；再解析真实路径，避免
/// `media_dir/link -> /outside` 这类符号链接逃逸被 `std::fs::read` 跟随后外发。
/// `media_dir` 为空或不存在时一律拒绝（防空前缀放行任意路径）。
pub(super) fn resolve_media_path(media_dir: &Path, candidate: &str) -> Option<PathBuf> {
    if media_dir.as_os_str().is_empty() || candidate.is_empty() {
        return None;
    }
    let base = PathBuf::from(media_dir.to_string_lossy().replace('\\', "/"));
    let target = PathBuf::from(candidate.replace('\\', "/"));
    if target
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    if !target.starts_with(&base) {
        return None;
    }
    resolve_path_within_base(&base, &target)
}

/// 解析 `target` 的最长已存在祖先，确认最终真实路径仍在 `base` 的真实路径之下。
///
/// `target` 可以是尚未创建的后代路径；已存在的祖先会被 canonicalize，从而展开沙箱内
/// 已存在的符号链接。调用方必须先完成词法前缀与 `..` 校验。
pub(super) fn resolve_path_within_base(base: &Path, target: &Path) -> Option<PathBuf> {
    let canonical_base = match base.canonicalize() {
        Ok(path) => path,
        Err(_) => return None,
    };
    let mut existing = target.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) if parent != existing => {
                tail.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut resolved = match existing.canonicalize() {
        Ok(path) => path,
        Err(_) => return None,
    };
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    resolved.starts_with(canonical_base).then_some(resolved)
}

// ---------------------------------------------------------------------------
// 请求体构造
// ---------------------------------------------------------------------------

/// 构造文本 sendmessage 请求体（带随机 client_id + context_token）。
pub(super) fn build_send_body(to_user_id: &str, text: &str, context_token: &str) -> Value {
    let client_id = format!("wcc-{}-{:08x}", now_secs(), rand::thread_rng().gen::<u32>());
    let text = normalize_weixin_text_newlines(text);
    json!({
        "msg": {
            "from_user_id": "",
            "to_user_id": to_user_id,
            "client_id": client_id,
            "message_type": 2,
            "message_state": 2,
            "context_token": context_token,
            "item_list": [{
                "type": 1,
                "text_item": { "text": text.as_ref() }
            }]
        }
    })
}

pub(super) fn normalize_weixin_text_newlines(text: &str) -> Cow<'_, str> {
    if !text.contains('\n') {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len() + text.matches('\n').count());
    let mut prev_was_cr = false;
    for ch in text.chars() {
        if ch == '\n' && !prev_was_cr {
            out.push('\r');
        }
        out.push(ch);
        prev_was_cr = ch == '\r';
    }
    Cow::Owned(out)
}

/// 构造携带单个媒体 item 的 sendmessage 请求体。
pub(super) fn build_send_body_with_item(
    to_user_id: &str,
    item: &Value,
    context_token: &str,
) -> Value {
    let client_id = format!("wcc-{}-{:08x}", now_secs(), rand::thread_rng().gen::<u32>());
    json!({
        "msg": {
            "from_user_id": "",
            "to_user_id": to_user_id,
            "client_id": client_id,
            "message_type": 2,
            "message_state": 2,
            "context_token": context_token,
            "item_list": [item]
        }
    })
}

/// 按媒体种类构造一个 `MessageItem`（媒体项）。
pub(super) fn build_media_item(
    kind: MediaKind,
    encrypt_query_param: &str,
    aes_key: &str,
    size: usize,
    file_name: &str,
) -> Value {
    let media = json!({
        "encrypt_query_param": encrypt_query_param,
        "aes_key": aes_key,
        "encrypt_type": 1
    });
    let item_type = kind.item_type();
    match kind {
        MediaKind::File => json!({
            "type": item_type,
            "file_item": { "media": media, "file_name": file_name, "len": size.to_string() }
        }),
        MediaKind::Image => json!({
            "type": item_type,
            "image_item": { "media": media, "mid_size": size }
        }),
        MediaKind::Video => json!({
            "type": item_type,
            "video_item": { "media": media, "video_size": size }
        }),
        // 语音发送：协议支持但官方实现未稳定覆盖（§8.5），尽量拼出可用结构。
        MediaKind::Voice => json!({
            "type": item_type,
            "voice_item": { "media": media }
        }),
    }
}

// ---------------------------------------------------------------------------
// 业务码 / 游标辅助
// ---------------------------------------------------------------------------

/// 从 iLink 响应对象取业务码 `(ret, errcode)`，缺失字段视为 0（兼容老接口）。
///
/// iLink 系接口（getupdates / sendmessage）即使在 HTTP 200 下，也用 `ret` / `errcode`
/// 表达业务结果，非 0 即失败。提取为一个独立函数，供 [`Weixin::poll_once`]
/// 与 [`Weixin::send`] 复用，保证「成功判定」在收/发两条路径上完全一致（P2）。
pub(super) fn business_codes(val: &Value) -> (i64, i64) {
    let ret = val.get("ret").and_then(|v| v.as_i64()).unwrap_or(0);
    let errcode = val.get("errcode").and_then(|v| v.as_i64()).unwrap_or(0);
    (ret, errcode)
}

pub(super) fn business_reason(val: &Value) -> Option<&str> {
    ["errmsg", "err_msg", "error_msg", "message", "msg"]
        .iter()
        .find_map(|key| val.get(*key).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
}

/// 业务是否成功：`ret == 0 && errcode == 0`（iLink 用业务码而非仅 HTTP 状态表达成败）。
pub(super) fn is_business_ok(val: &Value) -> bool {
    let (ret, errcode) = business_codes(val);
    ret == 0 && errcode == 0
}

/// 当前 Unix 秒（绕过 chrono 依赖：直接用 std 时间）。
pub(super) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 生成 X-WECHAT-UIN 头：base64(随机数字串)。
pub(super) fn generate_uin() -> String {
    let n: u32 = rand::thread_rng().gen();
    let s = n.to_string();
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

/// 按 `max_bytes` 在 UTF-8 字符边界上安全切分（避免切断多字节中文）。
///
/// `max_bytes == 0` 时【整段返回】（视为「不分段」）——这是对死循环的防御之一。
///
/// **`max_bytes` 小于单个字符字节数时的死循环修复（P2）**：旧实现在回退到字符边界后，
/// 若 `end == start`（例如 `chunk_bytes=1` 或 `2` 遇到中文 3 字节 / emoji 4 字节），
/// 会 push 一个【空切片】且 `start` 永不推进，`send_loop` 因此卡死。现在：当窗口内
/// 找不到字符边界（`end == start`）时，直接前进到【下一个字符边界】，把这一个超限
/// 字符单独成段——虽然该段超过了 `max_bytes`，但保证 `start` 必然推进，循环必然终止。
/// 正常配置（`chunk_bytes` >= 4，足以容纳单个 UTF-8 字符）不会进入该分支。
pub(super) fn utf8_chunks(text: &str, max_bytes: usize) -> Vec<&str> {
    if max_bytes == 0 || text.len() <= max_bytes {
        return vec![text];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + max_bytes).min(text.len());
        // 回退到字符边界：找 <= end 的最后一个边界。
        if end < text.len() {
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }
        }
        if end == start {
            // 窗口内无字符边界（max_bytes 小于下一字符的字节数）：前进到【下一个字符边界】，
            // 把这一个超限字符单独成段，保证 start 必然推进，杜绝死循环。
            // `start` 始终是字符边界（初始 0 是边界，之后每次更新到边界），故 `text[start..]` 合法。
            let char_len = text[start..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            let next = start + char_len;
            out.push(&text[start..next]);
            start = next;
        } else {
            out.push(&text[start..end]);
            start = end;
        }
    }
    out
}
