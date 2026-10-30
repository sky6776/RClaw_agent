use super::*;

// ---------------------------------------------------------------------------
// 出站媒体意图（标记协议）
//
// marker 由 RClaw 运行时的 publish_media 回执机制生成（rclaw-runtime/src/turn/media.rs
// 对任意声明了媒体能力的 channel 通用），本文件只负责解析与发送意图提取。语法与
// weixin 通道一致，额外接受 `name` 属性（运行时生成文件名）：
//   [:wxbot-media type=file path="agent/media/out/x.pdf" name="报告.pdf" caption="报价"]
// ---------------------------------------------------------------------------

/// 出站媒体种类。运行时 publish_media 只支持这四种（无 emoji）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MediaKind {
    File,
    Image,
    Video,
    Voice,
}

impl MediaKind {
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
    /// 本地可读文件绝对路径（agent 沙箱内，由 write/publish 落盘）。
    pub(super) path: String,
    /// 可选：发送给微信时的展示文件名（`name=` 属性，file 类型用）。
    pub(super) name: Option<String>,
    /// 可选：作为前置文本单独发送的说明。
    pub(super) caption: Option<String>,
}

/// 从回复正文解析媒体标记，返回（媒体意图, 剥离标记后的可见文本）。
///
/// 标记行形如（独占一行）：`[:wxbot-media type=file path=… name=… caption=…]`。
/// `type`/`path` 必填；值可用双引号包裹以含空格。解析失败的标记行原样保留为可见
/// 文本（便于发现格式问题）。
pub(super) fn parse_media_markers(content: &str) -> (Vec<MediaIntent>, String) {
    let mut intents = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    for line in content.split('\n') {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("[:wxbot-media") {
            // marker 名必须在 `wxbot-media` 后立即结束。没有边界检查时，
            // `[:wxbot-media-forged ...]` 也会进入属性解析并被当成真实发送意图。
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

/// 解析标记内部 `type=.. path=.. name=.. caption=..` 文本为意图（值可用双引号含空格）。
pub(super) fn parse_intent_inner(inner: &str) -> Option<MediaIntent> {
    let mut kind: Option<MediaKind> = None;
    let mut path: Option<String> = None;
    let mut name: Option<String> = None;
    let mut caption: Option<String> = None;
    for (k, v) in tokenize_kv(inner) {
        match k.as_str() {
            "type" => kind = MediaKind::from_str(&v),
            "path" => path = Some(v),
            "name" => name = Some(v),
            "caption" => caption = Some(v),
            _ => {}
        }
    }
    Some(MediaIntent {
        kind: kind?,
        path: path.filter(|s| !s.is_empty())?,
        name: name.filter(|s| !s.is_empty()),
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
// 通用辅助（与 weixin/media.rs 同款：分段 / 文件名 / 沙箱）
// ---------------------------------------------------------------------------

/// 按 `max_bytes` 在 UTF-8 字符边界上安全切分（避免切断多字节中文）。
///
/// `max_bytes == 0` 时整段返回（视为「不分段」）；窗口内无字符边界时把单个超限字符
/// 单独成段，保证 `start` 必然推进、循环必然终止（chunk_bytes=1 遇中文不死循环）。
pub(super) fn utf8_chunks(text: &str, max_bytes: usize) -> Vec<&str> {
    if max_bytes == 0 || text.len() <= max_bytes {
        return vec![text];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + max_bytes).min(text.len());
        if end < text.len() {
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }
        }
        if end == start {
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

/// 落盘文件名 stem 的字节上限：加上哈希前缀（16 字符 + `_`）与扩展名后仍留足
/// 余量，不触碰 Android/Linux 常见的 255 字节文件名上限。
const FILENAME_STEM_MAX_BYTES: usize = 100;

/// 把文件名里的路径分隔符 / 控制字符替换为 `_`，避免落盘越界；
/// 超长时按 UTF-8 边界截断（展示用标题保留原值，不受影响）。
pub(super) fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'..='\u{1F}' => '_',
            _ => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    // 字节预算截断：多字节字符整体丢弃（不切半个字符）。
    let mut end = trimmed.len().min(FILENAME_STEM_MAX_BYTES);
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let trimmed = &trimmed[..end];
    if trimmed.is_empty() {
        "media".into()
    } else {
        trimmed.into()
    }
}

/// 取路径的文件名部分（失败回退到整段）。
pub(super) fn path_file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// 出站媒体路径沙箱校验：`candidate` 必须落在 `media_dir` 之内。
///
/// 只做 `..` 词法拒绝 + 真实路径包含判定（不做词法前缀预检）：Android 上同一目录
/// 存在 `/data/user/0/...` 与 `/data/data/...` 两套等价前缀，运行时发布回执用的是
/// 物理等价形式，词法前缀会误拒合法路径；真正的外逃（沙箱内符号链接指向外部）
/// 由 [`resolve_path_within_base`] 的 canonicalize 包含判定挡住。
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

/// 入站媒体落盘的扩展名推断（raw 端点的产出格式：image=解码图、sticker=GIF、voice=mp3）。
/// file 类型优先用 title 的扩展名（1..=8 位字母数字），否则 bin。
pub(super) fn ext_for_media(kind: &str, title: Option<&str>) -> String {
    match kind {
        "image" => "jpg".into(),
        "sticker" => "gif".into(),
        "voice" => "mp3".into(),
        "file" => match title.and_then(|t| t.rsplit_once('.')) {
            Some((_, e))
                if (1..=8).contains(&e.len()) && e.bytes().all(|b| b.is_ascii_alphanumeric()) =>
            {
                e.to_ascii_lowercase()
            }
            _ => "bin".into(),
        },
        _ => "bin".into(),
    }
}

/// 图片字节流的真实格式（magic bytes）。wcx 的图片下载不做统一转码（只解 WXGF 为
/// GIF，失败原样透传字节），固定 `.jpg` 会让多模态标注的 MIME 与实际内容不符——
/// PNG/WebP/GIF/JPEG 按真实格式落盘，识别不了沿用调用方推断的扩展名。
pub(super) fn sniff_image_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("jpg")
    } else {
        None
    }
}
