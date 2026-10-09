use super::*;

// ---------------------------------------------------------------------------
// 通用辅助
// ---------------------------------------------------------------------------

/// 共享的 base 目录（路径校验基准 + 相对解析）。
pub(super) fn base_str(base: &Path) -> String {
    base.to_string_lossy().replace('\\', "/")
}

/// 解析文件工具路径并执行沙箱校验。
///
/// 绝对路径保持原义；相对路径统一相对于 agent 存储根解析。为兼容旧调用，
/// 已显式带 `agent/` 前缀的相对路径不会再重复拼接一层。
pub(super) async fn resolve_path(base: &Path, path: &str) -> Result<PathBuf, ToolResult> {
    if path.is_empty() {
        return Err(ToolResult::err("路径不能为空"));
    }
    let raw = Path::new(path);
    let resolved = if raw.is_absolute() || raw.starts_with(base) {
        raw.to_path_buf()
    } else if base
        .file_name()
        .is_some_and(|base_name| raw.starts_with(base_name))
    {
        match base.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.join(raw),
            _ => raw.to_path_buf(),
        }
    } else {
        base.join(raw)
    };
    // Unrestricted（max 权限）直放任意路径；工作区内直放；工作区外在宿主注册了
    // 确认钩子时（桌面版）弹窗询问用户，允许才放行——见 [`ensure_path_allowed`]。
    crate::access::ensure_path_allowed(
        base,
        &resolved,
        &format!("路径必须位于 {}/ 内且不能包含 '..'", base_str(base)),
    )
    .await?;
    Ok(resolved)
}

/// 从 JSON 入参取字符串字段。
pub(super) fn get_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(|v| v.as_str())
}

pub(super) fn parse_optional_bool(input: &Value, key: &str) -> Result<bool, ToolResult> {
    match input.get(key) {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| ToolResult::err(format!("{key} 必须是布尔值"))),
    }
}

pub(super) type FileMutationLock = tokio::sync::Mutex<()>;

pub(super) fn file_mutation_lock(path: &str) -> Arc<FileMutationLock> {
    static LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Weak<FileMutationLock>>>> = OnceLock::new();
    let key = normalize_lock_path(path);
    let mut locks = LOCKS
        .get_or_init(|| StdMutex::new(HashMap::new()))
        .lock()
        .expect("file mutation lock map not poisoned");
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = Arc::new(FileMutationLock::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

pub(super) fn normalize_lock_path(path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    let normalized = if let Ok(canonical) = fs::canonicalize(&path) {
        canonical
    } else {
        let absolute = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };
        if let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) {
            if let Ok(canonical_parent) = fs::canonicalize(parent) {
                canonical_parent.join(name)
            } else {
                absolute
            }
        } else {
            absolute
        }
    };
    platform_lock_key(normalized)
}

#[cfg(windows)]
pub(super) fn platform_lock_key(path: PathBuf) -> PathBuf {
    // Windows paths are case-insensitive by default. Case-fold missing paths too:
    // `canonicalize` cannot normalize their spelling before create-only checks.
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

#[cfg(not(windows))]
pub(super) fn platform_lock_key(path: PathBuf) -> PathBuf {
    path
}

pub(super) fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn file_sha256(path: &str) -> Result<(String, u64), ToolResult> {
    let mut file =
        fs::File::open(path).map_err(|e| ToolResult::err(format!("读取版本失败 {path}: {e}")))?;
    let metadata = file
        .metadata()
        .map_err(|e| ToolResult::err(format!("读取版本失败 {path}: {e}")))?;
    if !metadata.is_file() {
        return Err(ToolResult::err(format!(
            "读取版本失败 {path}: 目标不是普通文件"
        )));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| ToolResult::err(format!("读取版本失败 {path}: {e}")))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok((format!("{:x}", hasher.finalize()), metadata.len()))
}

pub(super) fn verify_expected_sha256(path: &str, expected: Option<&str>) -> Result<(), ToolResult> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if expected == "missing" {
        return match fs::metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ToolResult::err(format!("版本校验失败 {path}: {error}"))),
            Ok(_) => Err(ToolResult::err(format!(
                "VERSION_CONFLICT: expected_sha256=missing，但 {path} 已存在"
            ))),
        };
    }
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ToolResult::err(
            "expected_sha256 必须是 64 位十六进制 SHA-256，或字符串 \"missing\"",
        ));
    }
    let (actual, _) = match file_sha256(path) {
        Ok(value) => value,
        Err(_error) if !Path::new(path).exists() => {
            return Err(ToolResult::err(format!(
                "VERSION_CONFLICT: expected_sha256={expected}，但 {path} 不存在"
            )))
        }
        Err(error) => return Err(error),
    };
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(ToolResult::err(format!(
            "VERSION_CONFLICT: expected_sha256={expected}，actual_sha256={actual}；文件未修改"
        )))
    }
}

/// 判断本地文件/URL 的媒体类型和 MIME。
pub(super) fn media_type_from_name(name: &str) -> Option<(MediaKind, &'static str)> {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some((MediaKind::Image, "image/png")),
        "jpg" | "jpeg" => Some((MediaKind::Image, "image/jpeg")),
        "webp" => Some((MediaKind::Image, "image/webp")),
        "gif" => Some((MediaKind::Image, "image/gif")),
        "bmp" => Some((MediaKind::Image, "image/bmp")),
        "mp4" => Some((MediaKind::Video, "video/mp4")),
        "mov" => Some((MediaKind::Video, "video/quicktime")),
        "webm" => Some((MediaKind::Video, "video/webm")),
        "mpeg" | "mpg" => Some((MediaKind::Video, "video/mpeg")),
        "avi" => Some((MediaKind::Video, "video/x-msvideo")),
        "pdf" => Some((MediaKind::File, "application/pdf")),
        "doc" => Some((MediaKind::File, "application/msword")),
        "docx" => Some((
            MediaKind::File,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        )),
        "xls" => Some((MediaKind::File, "application/vnd.ms-excel")),
        "xlsx" => Some((
            MediaKind::File,
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        )),
        "ppt" => Some((MediaKind::File, "application/vnd.ms-powerpoint")),
        "pptx" => Some((
            MediaKind::File,
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        )),
        _ => None,
    }
}

pub(super) fn read_media_as_data_uri(path: &str, kind: MediaKind, mime: &str) -> ToolResult {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return ToolResult::err(format!("读取失败 {path}: {e}")),
    };
    if !meta.is_file() {
        return ToolResult::err(format!("无法读取 {path}: 目标不是普通文件"));
    }
    if meta.len() > MAX_MEDIA_FILE_SIZE {
        return ToolResult::err(format!(
            "媒体文件过大 ({} 字节 > 上限 {MAX_MEDIA_FILE_SIZE})。请改用 HTTPS URL 附加大文件/视频。",
            meta.len()
        ));
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => return ToolResult::err(format!("读取失败 {path}: {e}")),
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let url = format!("data:{mime};base64,{encoded}");
    let block = ContentBlock::media_url(kind, url);
    ToolResult::ok_with_attachments(
        format!("OK: 已读取媒体文件 {path}，类型 {mime}，将作为多模态输入附加给模型。"),
        vec![block],
    )
}

pub(super) fn attach_remote_media(url: &str) -> ToolResult {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return ToolResult::err("url 必须是 http(s) 地址");
    }
    let name = url.split('?').next().unwrap_or(url);
    let media = media_type_from_name(name).or_else(|| {
        let ext = Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match ext.as_str() {
            "txt" => Some((MediaKind::File, "text/plain")),
            "md" => Some((MediaKind::File, "text/markdown")),
            "csv" => Some((MediaKind::File, "text/csv")),
            "json" => Some((MediaKind::File, "application/json")),
            "html" | "htm" => Some((MediaKind::File, "text/html")),
            _ => None,
        }
    });
    let Some((kind, mime)) = media else {
        return ToolResult::err("无法从 url 扩展名判断媒体类型");
    };
    ToolResult::ok_with_attachments(
        format!("OK: 已附加远程媒体 {url}，推断类型 {mime}。"),
        vec![ContentBlock::media_url(kind, url.to_string())],
    )
}

/// 从 JSON 入参取可选的非负整数字段（offset 等）。
pub(super) fn parse_nonnegative_usize(
    input: &Value,
    key: &str,
) -> Result<Option<usize>, ToolResult> {
    let Some(v) = input.get(key) else {
        return Ok(None);
    };
    let Some(n) = v.as_u64() else {
        return Err(ToolResult::err(format!("{key} 必须是非负整数")));
    };
    usize::try_from(n)
        .map(Some)
        .map_err(|_| ToolResult::err(format!("{key} 过大")))
}

/// 从 JSON 入参取可选的正整数字段（limit / max_results 等）。
///
/// 调用方如果显式传了字段但值不是正整数，应得到清晰报错；否则 `limit=0` 会让
/// `read` 返回“offset 超出范围”，`max_results=0` 会让 `grep` 在未扫描任何内容时
/// 误报“未找到匹配”。
pub(super) fn parse_positive_usize(input: &Value, key: &str) -> Result<Option<usize>, ToolResult> {
    let Some(v) = input.get(key) else {
        return Ok(None);
    };
    let Some(n) = v.as_u64() else {
        return Err(ToolResult::err(format!("{key} 必须是正整数")));
    };
    if n == 0 {
        return Err(ToolResult::err(format!("{key} 必须大于 0")));
    }
    usize::try_from(n)
        .map(Some)
        .map_err(|_| ToolResult::err(format!("{key} 过大")))
}

/// 取目标元信息并确保是【普通文件】：拒绝目录 / FIFO / 字符设备等。
///
/// 目录、FIFO、设备文件在 `File::open` / `read_to_end` 上可能阻塞或读取特殊流，导致
/// 工具调用挂起。用 `metadata`（跟随符号链接）判断最终目标类型——指向普通文件的合法
/// 符号链接不会被误拒。[`read_text_bounded`]、[`read_lines_window`] 共用。
pub(super) fn regular_file_meta(path: &str) -> Result<fs::Metadata, ToolResult> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return Err(ToolResult::err(format!("读取失败 {path}: {e}"))),
    };
    if !meta.is_file() {
        let kind = if meta.is_dir() {
            "目录"
        } else {
            "特殊文件"
        };
        return Err(ToolResult::err(format!(
            "无法读取 {path}: 目标为{kind}，仅支持普通文本文件"
        )));
    }
    Ok(meta)
}

/// 返回 ≤ `idx` 的最大 UTF-8 字符边界下标，用于「按字节截断但不劈开多字节字符」。
pub(super) fn floor_char_boundary(bytes: &[u8], mut idx: usize) -> usize {
    if idx > bytes.len() {
        idx = bytes.len();
    }
    // `idx` 是字符边界 ⟺ `bytes[..idx]` 是合法 UTF-8 前缀。UTF-8 最长 4 字节，
    // 故最多回退 3 次，整体开销极小。
    while idx > 0 && std::str::from_utf8(&bytes[..idx]).is_err() {
        idx -= 1;
    }
    idx
}

/// 整篇限流读取文本文件：先 metadata 拦截超大文件，再 `take(MAX+1)` 兜底读取。
///
/// 当前仅供 [`EditFile`] 使用——edit 必须拿到【完整原文】才能安全替换，故对超过
/// [`MAX_FILE_SIZE`] 的文件直接拒绝（无法整篇载入即无法可靠编辑）。[`Read`] 改用
/// [`read_lines_window`] 分页读取，不再受 32KB 整篇上限约束。
///
/// 体积拦截：超过 [`MAX_FILE_SIZE`] 的文件在 metadata 阶段即拒绝（不读不分配）；
/// 限额内文件才会整篇 `read_to_end` 载入（edit 需要完整原文）。`take(MAX+1)` 兜底防
/// metadata 对管道/特殊文件失真——读到 `MAX_FILE_SIZE + 1` 字节即判超限。
pub(super) fn read_text_bounded(path: &str) -> Result<String, ToolResult> {
    let meta = regular_file_meta(path)?;
    if meta.len() > MAX_FILE_SIZE as u64 {
        return Err(ToolResult::err(format!(
            "文件过大 ({} 字节 > 上限 {MAX_FILE_SIZE})",
            meta.len()
        )));
    }
    let f = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return Err(ToolResult::err(format!("读取失败 {path}: {e}"))),
    };
    let mut bytes = Vec::new();
    if let Err(e) = f.take((MAX_FILE_SIZE + 1) as u64).read_to_end(&mut bytes) {
        return Err(ToolResult::err(format!("读取失败 {path}: {e}")));
    }
    if bytes.len() > MAX_FILE_SIZE {
        return Err(ToolResult::err(format!(
            "文件过大 ({} 字节 > 上限 {MAX_FILE_SIZE})",
            bytes.len()
        )));
    }
    match String::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(_) => Err(ToolResult::err("文件不是合法 UTF-8 文本")),
    }
}

/// `read` 分页读取的返回窗口。
pub(super) struct ReadWindow {
    /// 选中的行内容（保留原始换行）。
    pub(super) content: String,
    /// 本次扫描是否已读到 EOF。
    pub(super) reached_end: bool,
    /// 窗口起始行号（0 基）。
    pub(super) start_line: usize,
    /// 实际返回的行数。
    pub(super) lines_returned: usize,
    /// 是否因扫描字节数触顶而提前停止（offset 过大 / 文件病态冗长）。
    pub(super) scan_truncated: bool,
}

/// 流式分页读取：跳过 `offset` 行，返回至多 `limit` 行。
///
/// 与 [`read_text_bounded`] 不同，本函数【不把整篇读进内存再判超限】，而是用
/// `BufReader` 逐行扫描——只缓存当前行与已选中窗口，因而可安全处理远超 32KB 的大文件
/// （这正是旧实现 在 Termux 上 38KB 即报错的根因）。
///
/// 三层界限（与 list 的「深度+条目」双限流同源，保护 LLM 上下文预算）：
/// 1. `limit`：本次返回的最大行数（默认 [`READ_DEFAULT_LIMIT`]）；
/// 2. [`READ_MAX_OUTPUT_BYTES`]：本次输出的最大字节数——等价于旧的 32KB 上下文预算，
///    仅改为按「每次调用」而非「整个文件」计量；
/// 3. [`READ_MAX_SCAN_BYTES`]：本次扫描（含跳过的 offset 行）的最大字节数——兜底
///    「亿万空行」这类让 offset 跳过阶段空转的病态文件，避免工具调用挂起。
pub(super) fn read_lines_window(
    path: &str,
    offset: usize,
    limit: usize,
) -> Result<ReadWindow, ToolResult> {
    regular_file_meta(path)?;
    let f = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return Err(ToolResult::err(format!("读取失败 {path}: {e}"))),
    };
    let mut reader = std::io::BufReader::new(f);

    let mut out: Vec<u8> = Vec::new();
    let mut line: Vec<u8> = Vec::new();
    let mut idx = 0usize; // 当前行号（0 基）
    let mut kept = 0usize; // 已纳入窗口的行数
    let mut bytes_out = 0usize; // 窗口输出字节数
    let mut scanned = 0usize; // 累计扫描字节数（含跳过行）
    let mut reached_end = false;
    let mut scan_truncated = false;

    loop {
        line.clear();
        let n = match reader.read_until(b'\n', &mut line) {
            Ok(n) => n,
            Err(e) => return Err(ToolResult::err(format!("读取失败 {path}: {e}"))),
        };
        if n == 0 {
            reached_end = true; // EOF
            break;
        }
        scanned += n;
        if idx >= offset {
            // 已进入请求窗口。
            if kept >= limit {
                break; // 行数预算用尽：其后是否还有更多未知 → 视为「可能未完」
            }
            if bytes_out + n > READ_MAX_OUTPUT_BYTES {
                // 输出字节触顶。单行即超预算时至少截断返回一段，避免空响应。
                if kept == 0 {
                    let s = match std::str::from_utf8(&line) {
                        Ok(s) => s,
                        Err(_) => return Err(ToolResult::err("文件不是合法 UTF-8 文本")),
                    };
                    let cut = floor_char_boundary(s.as_bytes(), READ_MAX_OUTPUT_BYTES.min(s.len()));
                    out.extend_from_slice(&s.as_bytes()[..cut]);
                    out.extend_from_slice("\n[单行过长，已截断]\n".as_bytes());
                    kept = 1;
                }
                break;
            }
            // 逐行 UTF-8 校验：`\n` 必为 UTF-8 边界，逐行校验等价于整篇校验。
            if std::str::from_utf8(&line).is_err() {
                return Err(ToolResult::err("文件不是合法 UTF-8 文本"));
            }
            out.extend_from_slice(&line);
            bytes_out += n;
            kept += 1;
        }
        idx += 1;
        if scanned >= READ_MAX_SCAN_BYTES {
            scan_truncated = true;
            break;
        }
    }

    let content = String::from_utf8(out).map_err(|_| ToolResult::err("文件不是合法 UTF-8 文本"))?;
    Ok(ReadWindow {
        content,
        reached_end,
        start_line: offset,
        lines_returned: kept,
        scan_truncated,
    })
}

/// 为 `read` 的分页结果拼「翻页提示」。
pub(super) fn read_window_note(w: &ReadWindow) -> String {
    let start = w.start_line;
    let end = start + w.lines_returned; // 半开区间末尾 = 下一段的 offset
    if w.scan_truncated {
        return format!(
            "[read: 已扫描上限 {READ_MAX_SCAN_BYTES} 字节仍未取得足够内容；\
             文件过大或 offset={start} 过大，请减小 offset 或改用 grep 搜索]"
        );
    }
    if w.lines_returned == 0 {
        return format!("[read: offset={start} 超出范围，无内容返回（请减小 offset）]");
    }
    if w.reached_end {
        format!("[read: 返回第 {}–{} 行，已到文件末尾]", start + 1, end)
    } else {
        format!(
            "[read: 返回第 {}–{} 行，仍有后续内容；继续读取请传 offset={end}]",
            start + 1,
            end
        )
    }
}
