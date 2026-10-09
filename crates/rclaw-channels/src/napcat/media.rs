use super::*;

// OneBot 11 消息解析（入站）
// ---------------------------------------------------------------------------

/// 媒体种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum MediaKind {
    Image,
    Video,
    Voice,
    File,
}

impl MediaKind {
    pub(super) fn as_key(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
            MediaKind::Voice => "voice",
            MediaKind::File => "file",
        }
    }

    /// 标注里展示的中文名。
    pub(super) fn label(self) -> &'static str {
        match self {
            MediaKind::Image => "图片",
            MediaKind::Video => "视频",
            MediaKind::Voice => "语音",
            MediaKind::File => "文件",
        }
    }

    /// 无文件名时的默认落盘名（带扩展，便于 agent/系统识别）。
    pub(super) fn default_name(self) -> &'static str {
        match self {
            MediaKind::Image => "image.png",
            MediaKind::Video => "video.mp4",
            MediaKind::Voice => "voice.silk",
            MediaKind::File => "file.bin",
        }
    }

    pub(super) fn from_str(s: &str) -> Option<Self> {
        match s {
            "image" => Some(MediaKind::Image),
            "video" => Some(MediaKind::Video),
            "voice" => Some(MediaKind::Voice),
            "file" => Some(MediaKind::File),
            _ => None,
        }
    }
}

/// 根据文件签名推断常见图片扩展名，不盲信 NapCat 的可选文件名。
/// QQ 图片 URL 经常在缺少文件名时返回 JPEG 字节；历史 `image.png`
/// 回退会误导下游视觉工具，因此必须按内容修正扩展名。
pub(super) fn detected_image_extension(header: &[u8]) -> Option<&'static str> {
    if header.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if header.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
        Some("gif")
    } else if header.len() >= 12 && header.starts_with(b"RIFF") && &header[8..12] == b"WEBP" {
        Some("webp")
    } else if header.starts_with(b"BM") {
        Some("bmp")
    } else if header.starts_with(b"II*\0") || header.starts_with(b"MM\0*") {
        Some("tif")
    } else {
        None
    }
}

pub(super) fn normalize_downloaded_media_extension(
    kind: MediaKind,
    path: &Path,
) -> Result<PathBuf, String> {
    if kind != MediaKind::Image {
        return Ok(path.to_path_buf());
    }

    let mut file =
        std::fs::File::open(path).map_err(|e| format!("打开已下载图片 {}: {e}", path.display()))?;
    let mut header = [0u8; 16];
    let len = file
        .read(&mut header)
        .map_err(|e| format!("读取已下载图片 {}: {e}", path.display()))?;
    let Some(extension) = detected_image_extension(&header[..len]) else {
        return Ok(path.to_path_buf());
    };
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
    {
        return Ok(path.to_path_buf());
    }

    let corrected = path.with_extension(extension);
    if corrected.exists() {
        return Err(format!(
            "校正图片扩展名时目标已存在: {}",
            corrected.display()
        ));
    }
    std::fs::rename(path, &corrected).map_err(|e| {
        format!(
            "校正图片扩展名 {} -> {}: {e}",
            path.display(),
            corrected.display()
        )
    })?;
    Ok(corrected)
}

pub(super) fn format_cached_media_note(media: &CachedMedia, prefix: &str) -> String {
    let label = format!("{prefix}{}", media.kind.label());
    let title = media
        .name
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|n| format!(": {n}"))
        .unwrap_or_default();
    let p = media.path.to_string_lossy().replace('\\', "/");
    let hint = match media.kind {
        MediaKind::File => "（可用 read 读取文本内容）",
        _ => "",
    };
    format!("[{label}{title}] 已下载到 {p}{hint}")
}

pub(super) fn format_cached_text_note(text: &CachedText) -> String {
    let suffix = if text.truncated {
        "（已截断）"
    } else {
        ""
    };
    format!("[引用消息] {}{}", text.text, suffix)
}

pub(super) struct MediaDownloadContext<'a> {
    pub(super) target: &'a str,
    pub(super) target_id: &'a str,
    pub(super) message_id: i64,
}

pub(super) struct OutboundTarget<'a> {
    pub(super) target: &'a str,
    pub(super) id: &'a str,
    pub(super) action: &'a str,
}

/// 入站媒体引用（下载 URL 或 file_id + 解析信息）。
#[derive(Debug, Clone)]
pub(super) struct MediaRef {
    pub(super) kind: MediaKind,
    /// 媒体段自带的直链（image/video/record 常有；file 常无）。
    pub(super) url: Option<String>,
    /// 下载用唯一标识。NapCat 文件段的 `file` 是文件名，不能当成 `file_id`。
    /// 非文件媒体兼容旧上报，允许 `file_id` 缺失时回退到 `file`。
    pub(super) file_id: Option<String>,
    /// NapCat 可能在语音段给出本地 path；get_file/get_record 也可能返回本地路径。
    pub(super) path: Option<String>,
    /// 展示用文件名。
    pub(super) name: Option<String>,
    /// 群文件直链接口需要 busid。
    pub(super) busid: Option<i64>,
}

/// 从媒体段 data 抽取引用。
pub(super) fn media_ref(data: &Value, kind: MediaKind) -> MediaRef {
    let url = data
        .get("url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let file_field = data
        .get("file")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let file_id = data
        .get("file_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if kind == MediaKind::File {
                None
            } else {
                file_field
            }
        })
        .map(String::from);
    let path = data
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let name = data
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            data.get("file_name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            if kind == MediaKind::File {
                file_field
            } else {
                None
            }
        })
        .map(String::from);
    let busid = value_i64(data.get("busid"));
    MediaRef {
        kind,
        url,
        file_id,
        path,
        name,
        busid,
    }
}

pub(super) fn group_upload_media_ref(file: &Value) -> Option<MediaRef> {
    let file_id = first_string(file, &["id", "file_id"])?;
    let name = first_string(file, &["name", "file_name"]);
    let busid = value_i64(file.get("busid"));
    Some(MediaRef {
        kind: MediaKind::File,
        url: first_string(file, &["url", "download_url"]),
        file_id: Some(file_id),
        path: first_string(file, &["path", "file_path"]),
        name,
        busid,
    })
}

pub(super) fn group_message_reaches_llm(mode: &str, at_me: bool, at_all: bool) -> bool {
    match mode {
        "none" => false,
        "at_only" => at_me || at_all,
        _ => true,
    }
}

pub(super) fn text_cache_key(group_id: &str, message_id: i64) -> String {
    format!("{group_id}:{message_id}")
}

pub(super) fn media_message_cache_key(target: &str, target_id: &str, message_id: i64) -> String {
    format!("{target}:{target_id}:{message_id}")
}

pub(super) fn trim_text_for_index(text: &str) -> (String, bool) {
    let text = text.trim();
    if text.len() <= TEXT_INDEX_MAX_BYTES {
        return (text.to_string(), false);
    }
    let mut end = TEXT_INDEX_MAX_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

pub(super) fn trim_text_cache(cache: &mut HashMap<String, CachedText>) {
    if cache.len() <= TEXT_CACHE_CAP {
        return;
    }
    let remove = cache.len() / 2;
    let keys: Vec<String> = cache.keys().take(remove).cloned().collect();
    for key in keys {
        cache.remove(&key);
    }
}

pub(super) fn message_index_group_dir(media_dir: &Path, group_id: &str) -> Option<PathBuf> {
    if media_dir.as_os_str().is_empty() {
        None
    } else {
        Some(
            media_dir
                .join("napcat")
                .join("message_index")
                .join(format!("group_{}", sanitize_filename(group_id))),
        )
    }
}

pub(super) fn append_cached_text_to_disk(
    media_dir: &Path,
    cached: &CachedText,
    event_time: i64,
) -> Result<(), String> {
    let Some(dir) = message_index_group_dir(media_dir, &cached.group_id) else {
        return Ok(());
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录 {}: {e}", dir.display()))?;
    let path = dir.join(format!("{}.jsonl", date_string_from_unix_secs(event_time)));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("打开 {}: {e}", path.display()))?;
    let line = serde_json::to_string(cached).map_err(|e| e.to_string())?;
    file.write_all(line.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|e| format!("写入 {}: {e}", path.display()))
}

pub(super) fn find_cached_text_on_disk(
    media_dir: &Path,
    group_id: &str,
    message_id: i64,
) -> Option<CachedText> {
    let dir = message_index_group_dir(media_dir, group_id)?;
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|v| v.to_str())
                .is_some_and(|ext| ext == "jsonl")
        })
        .collect();
    files.sort();
    files.reverse();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut found = None;
        for line in text.lines() {
            let Ok(cached) = serde_json::from_str::<CachedText>(line) else {
                continue;
            };
            if cached.group_id == group_id && cached.message_id == message_id {
                found = Some(cached);
            }
        }
        if found.is_some() {
            return found;
        }
    }
    None
}

pub(super) fn current_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(super) fn date_string_from_unix_secs(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

pub(super) fn civil_from_days(days_since_unix_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}

pub(super) fn media_cache_keys(m: &MediaRef, target: &str, target_id: &str) -> Vec<String> {
    let scope = format!("{target}:{target_id}:{}", m.kind.as_key());
    let mut keys = Vec::new();
    if let Some(fid) = m.file_id.as_deref().filter(|s| !s.is_empty()) {
        keys.push(format!("{scope}:file_id:{fid}"));
    }
    if let Some(url) = m.url.as_deref().filter(|s| !s.is_empty()) {
        keys.push(format!("{scope}:url:{url}"));
    }
    if let Some(path) = m.path.as_deref().filter(|s| !s.is_empty()) {
        keys.push(format!("{scope}:path:{}", path.replace('\\', "/")));
    }
    if let (Some(name), Some(busid)) = (m.name.as_deref(), m.busid) {
        if !name.is_empty() {
            keys.push(format!("{scope}:busid:{busid}:name:{name}"));
        }
    }
    keys
}

pub(super) fn remember_cached_media_for_message(
    cache: &mut MediaCache,
    target: &str,
    target_id: &str,
    message_id: i64,
    cached: CachedMedia,
) {
    if message_id == 0 {
        return;
    }
    let items = cache
        .by_message_id
        .entry(media_message_cache_key(target, target_id, message_id))
        .or_default();
    if !items.iter().any(|existing| existing.path == cached.path) {
        items.push(cached);
    }
}

pub(super) fn media_cache_index_path(media_dir: &Path) -> Option<PathBuf> {
    if media_dir.as_os_str().is_empty() {
        None
    } else {
        Some(media_dir.join("napcat").join("media_index.json"))
    }
}

pub(super) fn load_media_cache(media_dir: &Path) -> MediaCache {
    let Some(path) = media_cache_index_path(media_dir) else {
        return MediaCache::default();
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return MediaCache::default(),
    };
    let mut cache: MediaCache = match serde_json::from_slice(&bytes) {
        Ok(cache) => cache,
        Err(e) => {
            warn!("NapCat 媒体索引读取失败 {}: {e}", path.display());
            return MediaCache::default();
        }
    };
    prune_missing_cached_media(&mut cache);
    trim_media_cache(&mut cache);
    cache
}

pub(super) fn prune_missing_cached_media(cache: &mut MediaCache) {
    cache.by_key.retain(|_, media| media.path.is_file());
    cache.by_message_id.retain(|_, items| {
        items.retain(|media| media.path.is_file());
        !items.is_empty()
    });
}

pub(super) fn trim_media_cache(cache: &mut MediaCache) {
    if cache.by_key.len() > MEDIA_CACHE_CAP {
        let remove = cache.by_key.len() / 2;
        let keys: Vec<String> = cache.by_key.keys().take(remove).cloned().collect();
        for key in keys {
            cache.by_key.remove(&key);
        }
    }
    if cache.by_message_id.len() > MEDIA_CACHE_CAP {
        let remove = cache.by_message_id.len() / 2;
        let ids: Vec<String> = cache.by_message_id.keys().take(remove).cloned().collect();
        for id in ids {
            cache.by_message_id.remove(&id);
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedMediaSource {
    pub(super) url: Option<String>,
    pub(super) path: Option<String>,
    pub(super) name: Option<String>,
}

impl ResolvedMediaSource {
    pub(super) fn from_ref(m: &MediaRef) -> Self {
        ResolvedMediaSource {
            url: m.url.clone(),
            path: m.path.clone(),
            name: m.name.clone(),
        }
    }

    pub(super) fn merge_api_data(&mut self, data: &Value) {
        if self.url.is_none() {
            self.url = first_string(data, &["url", "download_url"]);
        }
        if self.path.is_none() {
            self.path = first_string(data, &["file", "path"]);
        }
        if self.name.is_none() {
            self.name = first_string(data, &["file_name", "name"]);
        }
        if self.name.is_none() {
            self.name = self
                .url
                .as_deref()
                .and_then(filename_from_source)
                .or_else(|| self.path.as_deref().and_then(filename_from_source));
        }
    }

    pub(super) fn has_source(&self) -> bool {
        self.url.as_deref().is_some_and(|s| !s.is_empty())
            || self.path.as_deref().is_some_and(|s| !s.is_empty())
    }

    pub(super) fn candidates(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(url) = self.url.as_deref().filter(|s| !s.is_empty()) {
            out.push(url.to_string());
        }
        if let Some(path) = self.path.as_deref().filter(|s| !s.is_empty()) {
            if !out.iter().any(|s| s == path) {
                out.push(path.to_string());
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
pub(super) struct MediaBytes {
    pub(super) bytes: Vec<u8>,
    pub(super) name: Option<String>,
}

#[derive(Debug, Default)]
pub(super) struct StreamDownloadState {
    pub(super) chunks: HashMap<usize, Vec<u8>>,
    pub(super) complete_seen: bool,
    pub(super) total_chunks: Option<usize>,
    pub(super) total_bytes: Option<u64>,
    pub(super) name: Option<String>,
}

impl StreamDownloadState {
    pub(super) fn ingest(&mut self, data: &Value) -> Result<(), String> {
        match data.get("type").and_then(|v| v.as_str()) {
            Some("stream") => match data.get("data_type").and_then(|v| v.as_str()) {
                Some("file_info") => {
                    if self.name.is_none() {
                        self.name = first_string(data, &["file_name", "name"]);
                    }
                    if self.total_bytes.is_none() {
                        self.total_bytes = value_u64(data.get("file_size"));
                    }
                }
                Some("file_chunk") => {
                    let index = value_usize(data.get("index"))
                        .or_else(|| value_usize(data.get("chunk_index")))
                        .ok_or_else(|| "stream chunk 缺少 index".to_string())?;
                    let encoded = first_string(data, &["data", "chunk_data"])
                        .ok_or_else(|| "stream chunk 缺少 data".to_string())?;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|e| format!("stream chunk base64 解码失败: {e}"))?;
                    self.chunks.insert(index, bytes);
                }
                Some(other) => {
                    warn!("NapCat Stream 未识别 data_type: {other}");
                }
                None => {}
            },
            Some("response") => {
                self.complete_seen = true;
                if self.total_chunks.is_none() {
                    self.total_chunks = value_usize(data.get("total_chunks"));
                }
                if self.total_bytes.is_none() {
                    self.total_bytes = value_u64(data.get("total_bytes"));
                }
            }
            Some("error") | Some("reset") => {
                let message = first_string(data, &["message", "wording", "data"])
                    .unwrap_or_else(|| "stream error".into());
                return Err(message);
            }
            Some(other) => warn!("NapCat Stream 未识别 type: {other}"),
            None => {}
        }
        Ok(())
    }

    pub(super) fn try_finish(&mut self) -> Result<Option<MediaBytes>, String> {
        if !self.complete_seen {
            return Ok(None);
        }
        let Some(total) = self.total_chunks else {
            return Ok(None);
        };
        if self.chunks.len() < total {
            return Ok(None);
        }

        let expected_total = self.total_bytes.unwrap_or_default() as usize;
        let mut bytes = if expected_total > 0 {
            Vec::with_capacity(expected_total)
        } else {
            Vec::new()
        };
        for i in 0..total {
            let chunk = self
                .chunks
                .remove(&i)
                .ok_or_else(|| format!("stream 缺少分块 {i}/{total}"))?;
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some(MediaBytes {
            bytes,
            name: self.name.clone(),
        }))
    }
}

/// 从 `message` 字段抽取（拼接文本, 媒体引用列表, 引用消息 id, @机器人标记, @all 标记）。
///
/// `message` 在 OneBot 11 中可以是纯字符串，也可以是消息段数组。
/// - `text` 段拼入正文；
/// - `at` 段：qq == 机器人自身（self_id）则【剥离】（只置 at_me 标记），qq=="all" 置 at_all
///   并保留 `[@all]`，其它 @ 保留 `[@qq]`；
/// - image/record/video/file 收集为媒体引用（下载在后续步骤）；
/// - `reply` 引用回复只记录 id，不直接注入正文；表情降级为 `[表情]`；其余类型忽略。
pub(super) struct Extracted {
    pub(super) text: String,
    pub(super) media: Vec<MediaRef>,
    pub(super) reply_ids: Vec<i64>,
    pub(super) at_me: bool,
    pub(super) at_all: bool,
}

pub(super) fn extract_message(message: &Value, self_id: &str) -> Extracted {
    let mut ext = Extracted {
        text: String::new(),
        media: Vec::new(),
        reply_ids: Vec::new(),
        at_me: false,
        at_all: false,
    };
    match message {
        Value::String(s) => ext.text.push_str(s),
        Value::Array(segs) => {
            for seg in segs {
                let ty = seg.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let data = seg.get("data").unwrap_or(&Value::Null);
                match ty {
                    "text" => {
                        if let Some(t) = data.get("text").and_then(|v| v.as_str()) {
                            ext.text.push_str(t);
                        }
                    }
                    "at" => {
                        let qq = id_str(data.get("qq"));
                        if qq.is_empty() {
                            // 缺 qq 的 at 段：忽略。
                        } else if qq == self_id {
                            ext.at_me = true; // 剥离 @机器人
                        } else if qq == "all" {
                            ext.at_all = true;
                            ext.text.push_str("[@all]");
                        } else {
                            ext.text.push_str(&format!("[@{qq}]"));
                        }
                    }
                    "reply" => {
                        if let Some(id) = value_i64(data.get("id")) {
                            ext.reply_ids.push(id);
                        }
                    }
                    "image" => ext.media.push(media_ref(data, MediaKind::Image)),
                    "record" => ext.media.push(media_ref(data, MediaKind::Voice)),
                    "video" => ext.media.push(media_ref(data, MediaKind::Video)),
                    "file" => ext.media.push(media_ref(data, MediaKind::File)),
                    "mface" | "face" => ext.text.push_str("[表情]"),
                    _ => {} // 其余段类型忽略，保持通道最小。
                }
            }
        }
        _ => {}
    }
    ext
}

/// OneBot 11 ID 可能是数字或字符串，统一成字符串。
pub(super) fn id_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

pub(super) fn value_i64(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

pub(super) fn value_u64(v: Option<&Value>) -> Option<u64> {
    match v {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

pub(super) fn value_usize(v: Option<&Value>) -> Option<usize> {
    value_u64(v).and_then(|n| usize::try_from(n).ok())
}

pub(super) fn echo_str(v: Option<&Value>) -> Option<String> {
    v.and_then(|v| {
        v.as_str()
            .map(|s| s.to_string())
            .or_else(|| v.as_i64().map(|n| n.to_string()))
    })
}

pub(super) fn is_stream_packet(resp: &Value) -> bool {
    if resp.get("stream").and_then(|v| v.as_str()) == Some("stream-action") {
        return true;
    }
    matches!(
        resp.get("data")
            .and_then(|d| d.get("type"))
            .and_then(|v| v.as_str()),
        Some("stream" | "response" | "error" | "reset")
    )
}

pub(super) fn api_error_wording(resp: &Value) -> String {
    resp.get("wording")
        .and_then(|v| v.as_str())
        .or_else(|| resp.get("message").and_then(|v| v.as_str()))
        .unwrap_or("unknown error")
        .to_string()
}

pub(super) fn is_http_url(candidate: &str) -> bool {
    candidate.starts_with("http://") || candidate.starts_with("https://")
}

pub(super) fn should_try_stream_before_local(kind: MediaKind, candidate: &str) -> bool {
    matches!(kind, MediaKind::File | MediaKind::Video)
        && !is_http_url(candidate)
        && !candidate.starts_with("base64://")
}

pub(super) fn should_try_stream_after_http_failure(kind: MediaKind) -> bool {
    matches!(kind, MediaKind::Image | MediaKind::File | MediaKind::Video)
}

pub(super) fn should_try_stream_by_file_id(kind: MediaKind) -> bool {
    matches!(kind, MediaKind::Image | MediaKind::File | MediaKind::Video)
}

pub(super) fn first_string(data: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        data.get(*key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    })
}

pub(super) fn filename_from_source(source: &str) -> Option<String> {
    let raw = if source.starts_with("file://") {
        file_url_to_path(source).to_string_lossy().into_owned()
    } else {
        source
            .split(['?', '#'])
            .next()
            .unwrap_or(source)
            .to_string()
    };
    let name = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    (!name.is_empty()).then_some(name)
}

pub(super) fn file_url_to_path(url: &str) -> PathBuf {
    let mut s = url.strip_prefix("file://").unwrap_or(url).to_string();
    if cfg!(windows) {
        if s.len() >= 3 && s.as_bytes()[0] == b'/' && s.as_bytes()[2] == b':' {
            s.remove(0);
        }
        return PathBuf::from(s.replace('/', "\\"));
    }
    PathBuf::from(s)
}

// ---------------------------------------------------------------------------
// 出站媒体意图（标记协议）
// ---------------------------------------------------------------------------

/// 出站媒体发送意图（由回复正文里的标记行解析而来）。
#[derive(Debug, Clone)]
pub(super) struct MediaIntent {
    pub(super) kind: MediaKind,
    /// 本地可读文件绝对路径（agent/ 前缀，由 write 落盘）。
    pub(super) path: String,
    /// 可选：作为前置文本单独发送的说明。
    pub(super) caption: Option<String>,
    /// 可选：发送文件时展示的文件名（默认取 path 的文件名部分）。
    pub(super) name: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct PreparedMedia {
    /// 本地文件 URL；Stream 失败后的普通回退会再按大小转成 `base64://` 或保留 `file:///`。
    pub(super) file_ref: String,
    pub(super) name: String,
    pub(super) path: PathBuf,
    pub(super) size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MediaSendPhase {
    Prepare,
    Deliver,
}

#[derive(Debug)]
pub(super) struct MediaSendError {
    pub(super) phase: MediaSendPhase,
    pub(super) message: String,
}

impl MediaSendError {
    pub(super) fn prepare(message: String) -> Self {
        Self {
            phase: MediaSendPhase::Prepare,
            message,
        }
    }

    pub(super) fn deliver(message: String) -> Self {
        Self {
            phase: MediaSendPhase::Deliver,
            message,
        }
    }
}

pub(super) fn delivery_status_unknown(message: &str) -> bool {
    let message = message.trim();
    message == "api 超时"
        || message == "api 响应通道关闭"
        || message.starts_with("Timeout: NTEvent")
}

pub(super) fn napcat_send_msg_timeout(message: &str) -> bool {
    let message = message.trim();
    message.starts_with("Timeout: NTEvent") && message.contains("NodeIKernelMsgService/sendMsg")
}

pub(super) fn text_send_timeout_retryable(message: &str) -> bool {
    message.trim() == "api 超时" || napcat_send_msg_timeout(message)
}

pub(super) fn should_fallback_after_media_error(error: &MediaSendError) -> bool {
    error.phase == MediaSendPhase::Prepare || !delivery_status_unknown(&error.message)
}

/// 从回复正文解析媒体标记，返回（媒体意图, 剥离标记后的可见文本）。
///
/// 标记行形如（独占一行）：
/// `[:napcat-media type=file path=agent/media/out/report.html caption=报价单]`
/// `caption` 可选；`path`/`type` 必填。值可用双引号包裹以含空格。
/// 解析失败的标记行原样保留为可见文本（便于发现格式问题）。
pub(super) fn parse_media_markers(content: &str) -> (Vec<MediaIntent>, String) {
    let mut intents = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    for line in content.split('\n') {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("[:napcat-media") {
            // marker 名必须在 `napcat-media` 后立即结束。没有边界检查时，
            // `[:napcat-media-forged ...]` 也会进入属性解析并被当成真实发送意图，
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

/// 解析标记内部 `type=.. path=.. caption=.. name=..` 文本为意图（值可用双引号含空格）。
pub(super) fn parse_intent_inner(inner: &str) -> Option<MediaIntent> {
    let mut kind: Option<MediaKind> = None;
    let mut path: Option<String> = None;
    let mut caption: Option<String> = None;
    let mut name: Option<String> = None;
    for (k, v) in tokenize_kv(inner) {
        match k.as_str() {
            "type" => kind = MediaKind::from_str(&v),
            "path" => path = Some(v),
            "caption" => caption = Some(v),
            "name" => name = Some(v),
            _ => {}
        }
    }
    Some(MediaIntent {
        kind: kind?,
        path: path.filter(|s| !s.is_empty())?,
        caption: caption.filter(|s| !s.is_empty()),
        name: name.filter(|s| !s.is_empty()),
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
// 出站媒体段构造 + 沙箱校验
// ---------------------------------------------------------------------------

/// 按 [`MediaIntent`] 构造一个 OneBot 消息段。
///
/// 路径必须落在 `media_dir` 沙箱内（见 [`resolve_media_path`]）；否则返回 `None`。
/// 这是 Stream 失败后的回退构造；常规大小文件使用 `base64://`，超大文件回退 `file:///`。
#[cfg(test)]
pub(super) fn build_media_segment(intent: &MediaIntent, media_dir: &Path) -> Option<Value> {
    let prepared = prepare_outbound_media(intent, media_dir)?;
    Some(media_segment_value(
        intent.kind,
        &fallback_file_ref(&prepared),
        &prepared.name,
    ))
}

pub(super) fn prepare_outbound_media(
    intent: &MediaIntent,
    media_dir: &Path,
) -> Option<PreparedMedia> {
    let abs = resolve_media_path(media_dir, &intent.path)?;
    if !abs.is_file() {
        return None;
    }
    // canonicalize 在 Windows 上会给路径加 `\\?\` 前缀，直接拼进 file:// 会得到
    // 形如 `file:////?/C:/...` 的畸形 URL，NapCat 无法读取。这里剥掉该前缀。
    let clean = strip_verbatim_prefix(abs);
    let file_url = local_file_url(&clean);
    let size = clean.metadata().ok()?.len();
    let name = sanitize_filename(&intent.name.clone().unwrap_or_else(|| {
        clean
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into())
    }));
    Some(PreparedMedia {
        file_ref: file_url,
        name,
        path: clean,
        size,
    })
}

pub(super) fn fallback_file_ref(prepared: &PreparedMedia) -> String {
    outbound_file_ref(&prepared.path).unwrap_or_else(|| prepared.file_ref.clone())
}

pub(super) fn outbound_file_ref(path: &Path) -> Option<String> {
    let len = path.metadata().ok()?.len();
    if len <= OUTBOUND_BASE64_MAX_BYTES {
        let bytes = std::fs::read(path).ok()?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        Some(format!("base64://{encoded}"))
    } else {
        Some(local_file_url(path))
    }
}

pub(super) fn sha256_file_hex(path: &Path) -> Result<String, String> {
    let mut file =
        File::open(path).map_err(|e| format!("无法打开文件计算 SHA256 {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 1024 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("读取文件计算 SHA256 失败 {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub(super) fn media_segment_value(kind: MediaKind, file_ref: &str, name: &str) -> Value {
    match kind {
        MediaKind::File => json!({ "type": "file", "data": { "file": file_ref, "name": name } }),
        MediaKind::Image => json!({ "type": "image", "data": { "file": file_ref } }),
        MediaKind::Video => json!({ "type": "video", "data": { "file": file_ref } }),
        MediaKind::Voice => json!({ "type": "record", "data": { "file": file_ref } }),
    }
}

pub(super) fn local_file_url(path: &Path) -> String {
    format!("file:///{}", path.to_string_lossy().replace('\\', "/"))
}

/// 剥离 Windows canonicalize 给出的 `\\?\` / `\\?\UNC\` 前缀，得到普通 DOS 路径。
/// 在非 Windows 平台是 no-op（前缀不会出现）。
pub(super) fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if let Some(unc) = rest.strip_prefix(r"UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        return PathBuf::from(rest);
    }
    path
}

// ---------------------------------------------------------------------------
// 请求构造 / chat_id 解析 / 文本分段
// ---------------------------------------------------------------------------

/// 解析 chat_id 为 (target, id)：`group:<g>` → ("group", g)；`user:<u>` → ("private", u)；
/// 无前缀按私聊处理。
pub(super) fn parse_chat_id(chat_id: &str) -> (&'static str, String) {
    if let Some(g) = chat_id.strip_prefix("group:") {
        ("group", g.to_string())
    } else if let Some(u) = chat_id.strip_prefix("user:") {
        ("private", u.to_string())
    } else {
        ("private", chat_id.to_string())
    }
}

/// 构造 WS 连接请求：补齐尾部 `/`，附加 `access_token` 查询参数与 `Authorization: Bearer` 头。
///
/// NapCat 要求 URL 以 `/` 结尾（如 `ws://127.0.0.1:3001/`）；查询参数中已有 `?` 时在它前面插 `/`。
pub(super) fn build_ws_request(
    ws_url: &str,
    access_token: &str,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
    let base = if ws_url.ends_with('/') {
        ws_url.to_string()
    } else if ws_url.contains('?') {
        ws_url.replacen('?', "/?", 1)
    } else {
        format!("{ws_url}/")
    };
    let url = if !access_token.is_empty() {
        if base.contains('?') {
            format!("{base}&access_token={access_token}")
        } else {
            format!("{base}?access_token={access_token}")
        }
    } else {
        base
    };

    let mut req = url
        .into_client_request()
        .map_err(|e| format!("非法 ws_url: {e}"))?;
    if !access_token.is_empty() {
        if let Ok(hv) = HeaderValue::from_str(&format!("Bearer {access_token}")) {
            req.headers_mut().insert("Authorization", hv);
        }
    }
    Ok(req)
}

/// 按 `max_bytes` 在 UTF-8 字符边界上安全切分（避免切断多字节中文）。
///
/// `max_bytes == 0` 时整段返回。窗口内找不到字符边界（`max_bytes` 小于下一字符字节数）时，
/// 把该超限字符单独成段，保证 `start` 必然推进、循环必然终止。
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

/// 把长文切成不超过 `max_bytes` 的 node 文本片段，按"段落(换行) → 句末标点 → UTF-8 字符"优先级
/// 寻找切点：尽量把多个短段落并入同一个 node，且不切断整句。合并转发卡片每个 node 用一片。
pub(super) fn split_forward_nodes(text: &str, max_bytes: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.len() <= max_bytes {
            out.push(remaining.to_string());
            break;
        }

        // 在 [0, max_bytes] 内分别记录最靠后的段落和句末边界，段落边界优先。
        let mut paragraph_cut = None;
        let mut sentence_cut = None;
        let mut has_content = false;
        for (i, ch) in remaining.char_indices() {
            let end = i + ch.len_utf8();
            if end > max_bytes {
                break;
            }
            if !ch.is_whitespace() {
                has_content = true;
            }
            if ch == '\n' && has_content {
                paragraph_cut = Some(end);
            } else if "。！？!?…；;".contains(ch)
                || (ch == '.'
                    && remaining[end..]
                        .chars()
                        .next()
                        .is_none_or(char::is_whitespace))
            {
                sentence_cut = Some(end);
            }
        }

        // 该区间无软切点时按 UTF-8 边界硬切；阈值小于单个字符时仍至少前进一个字符。
        let cut = paragraph_cut.or(sentence_cut).unwrap_or_else(|| {
            let mut end = max_bytes.min(remaining.len());
            while end > 0 && !remaining.is_char_boundary(end) {
                end -= 1;
            }
            if end > 0 {
                end
            } else {
                remaining
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(remaining.len())
            }
        });
        out.push(remaining[..cut].to_string());
        remaining = &remaining[cut..];
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
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

pub(super) fn temp_download_path(target: &Path) -> PathBuf {
    let mut tmp = target.to_path_buf();
    let name = target
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("media");
    tmp.set_file_name(format!(
        ".{name}.{:08x}.tmp",
        rand::thread_rng().gen::<u32>()
    ));
    tmp
}

pub(super) async fn write_bytes_to_path(target: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = temp_download_path(target);
    if let Err(e) = tokio::fs::write(&tmp, bytes).await {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    if let Err(e) = tokio::fs::rename(&tmp, target).await {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}

pub(super) async fn copy_local_file_to_path(source: &Path, target: &Path) -> Result<(), String> {
    let tmp = temp_download_path(target);
    if let Err(e) = tokio::fs::copy(source, &tmp).await {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    if let Err(e) = tokio::fs::rename(&tmp, target).await {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}

/// 出站媒体路径沙箱校验：`candidate` 必须落在 `media_dir` 之内。
///
/// 先做词法前缀与 `..` 检查，挡住明显越界和兄弟目录前缀；再解析真实路径，避免
/// `media_dir/link -> /outside` 这类符号链接逃逸被 NapCat 经 `file:///` 跟随后外发。
/// `media_dir` 为空或不存在时一律拒绝（防空前缀放行任意路径）。
pub(super) fn resolve_media_path(media_dir: &Path, candidate: &str) -> Option<PathBuf> {
    if media_dir.as_os_str().is_empty() || candidate.is_empty() {
        return None;
    }
    let base = PathBuf::from(media_dir.to_string_lossy().replace('\\', "/"));
    let raw_target = PathBuf::from(candidate.replace('\\', "/"));
    if raw_target
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }

    let mut candidates = Vec::new();
    candidates.push(raw_target.clone());
    if raw_target.is_relative() {
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join(&raw_target));
        }
        candidates.push(base.join(&raw_target));
    }

    for target in candidates {
        if target.starts_with(&base) {
            if let Some(resolved) = resolve_path_within_base(&base, &target) {
                return Some(resolved);
            }
        }
    }
    None
}

/// 解析 `target` 的最长已存在祖先，确认最终真实路径仍在 `base` 的真实路径之下。
///
/// `target` 可以是尚未创建的后代路径；已存在的祖先会被 canonicalize，从而展开沙箱内
/// 已存在的符号链接。调用方必须先完成词法前缀与 `..` 校验。
pub(super) fn resolve_path_within_base(base: &Path, target: &Path) -> Option<PathBuf> {
    let canonical_base = base.canonicalize().ok()?;
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
    let mut resolved = existing.canonicalize().ok()?;
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    resolved.starts_with(canonical_base).then_some(resolved)
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------
