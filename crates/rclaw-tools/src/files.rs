//! 文件类内置工具：read / write / edit / list / grep。
//!
//! 安全约束：
//! - Sandboxed 权限下路径默认必须位于 `base`（agent/）内，且不含 `..`（防目录穿越）；
//!   工作区之外的路径只有宿主注册了确认钩子（桌面版弹窗）且用户逐个放行时才可访问
//!   （见 [`crate::access::ensure_path_allowed`]；CLI 等无钩子环境一律拒绝）。Unrestricted
//!   权限可访问任意路径。Sandboxed 的符号链接逃逸由 [`rclaw_store::validate_agent_path`]
//!   的解析层兜底。盘符相对路径（`C:foo`）任何权限下都拒绝；
//! - 任何权限下都不经文件工具读写工作区根下的运行时数据库 `state.sqlite`（及伴生文件），
//!   不删工作区根 / 文件系统根 / 挂载点；
//! - `read` 分页流式读取（offset/limit），不再以 32KB 整篇上限硬拒大文件，改为按
//!   「每次调用」的行数/字节预算保护上下文；
//! - `edit` 需要完整原文才能安全替换，保留 128MB 整篇载入上限；
//! - `grep` 进程内正则搜索（regex + walkdir，不调用外部 rg），目录递归不跟随符号链接。

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, Read as IoRead};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};

use async_trait::async_trait;
use base64::Engine;
use rclaw_core::{ContentBlock, MediaKind};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use rclaw_store::StoreError;

use crate::access::PathOp;
use crate::{Tool, ToolResult};

/// `edit` 整篇载入的最大字节数（edit 需要完整原文才能安全替换）。
///
/// 此值为【峰值内存】界限而非上下文预算：`edit` 会把整个文件 `read_to_end` 进
/// 内存、替换后再整篇写回，故单次编辑的峰值内存约为该值的 1–2 倍（128MB 文件 ≈ 256MB
/// 峰值）。调大可编辑更大的文件，但请在目标设备（如 Termux/手机）的可用内存内取舍。
const MAX_FILE_SIZE: usize = 128 * 1024 * 1024;

/// `list` 递归深度上限：防御 symlink 环 / 异常深的目录结构导致栈溢出或超长耗时。
const MAX_WALK_DEPTH: usize = 32;
/// `list` 输出条目上限：避免把海量小文件的清单灌进上下文。
const MAX_WALK_ENTRIES: usize = 4096;

/// `read` 默认每次返回的行数。
const READ_DEFAULT_LIMIT: usize = 2000;
/// `read` 单次输出的最大字节数（流式读取，仅此缓冲进内存；与上下文预算解耦）。
const READ_MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// `read` 单次扫描（含跳过的 offset 行）的最大字节数——纯时间阀，兜底「亿万空行」等病态文件。
const READ_MAX_SCAN_BYTES: usize = 64 * 1024 * 1024;
/// 单次附加到多模态请求的本地媒体最大字节数。
const MAX_MEDIA_FILE_SIZE: u64 = 20 * 1024 * 1024;

/// `grep` 默认最多返回的匹配数。
const MAX_GREP_RESULTS: usize = 1000;
/// `grep` 单次输出的最大字节数。
const MAX_GREP_OUTPUT_BYTES: usize = 256 * 1024;
/// `grep` 目录递归的最大深度（与 `list` 的 [`MAX_WALK_DEPTH`] 同源）。
const MAX_GREP_DEPTH: usize = 32;
/// `grep` 单文件扫描的最大字节数——纯时间阀，大文件只搜前缀。
const MAX_GREP_FILE_SCAN: usize = 64 * 1024 * 1024;
/// `grep` 单行输出的最大字节数，超出截断。
const MAX_GREP_LINE_BYTES: usize = 2048;

pub(crate) mod helpers;
use helpers::*;
pub use helpers::{is_dos_device_name, open_regular_file};

mod pdf;
// ===========================================================================
// read
// ===========================================================================

pub struct Read {
    base: PathBuf,
}

impl Read {
    pub fn new(base: PathBuf) -> Self {
        Read { base }
    }
}

#[async_trait]
impl Tool for Read {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read text files or attach image/video/file media. Paths outside agent storage require unrestricted access. Remote media may use url=http(s)://... For PDFs, local detection routes text-based PDFs to extracted text (paginated) and scanned/mixed ones to native media attachment."
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "path":{"type":"string","description":format!("Absolute local path, or path relative to {}/ (outside it requires unrestricted access)", base_str(&self.base))},
                "url":{"type":"string","description":"Optional http(s) media URL to attach directly"},
                "offset":{"type":"integer","minimum":0,"description":"起始行号（0 基，默认 0），用于大文件分页读取"},
                "limit":{"type":"integer","minimum":1,"description":format!("最多返回的行数（默认 {READ_DEFAULT_LIMIT}）")},
                "include_metadata":{"type":"boolean","description":"为文本读取附加 sha256/bytes 版本元数据，供后续 write/edit 的 expected_sha256 使用"}
            }
        })
    }
    async fn execute(&self, input: Value) -> ToolResult {
        if let Some(url) = get_str(&input, "url") {
            return attach_remote_media(url);
        }
        let path = match get_str(&input, "path") {
            Some(p) => p,
            None => return ToolResult::err("缺少 'path' 或 'url' 字段"),
        };
        let path = match resolve_path(&self.base, path, PathOp::Read).await {
            Ok(path) => path,
            Err(error) => return error,
        };
        let path = path.to_string_lossy().into_owned();
        // offset/limit 解析前移到媒体分支之前：PDF 文本路径需要它们做行分页。
        // 副作用（既定设计决策，ADR-0003）：无效 offset/limit 在媒体路径上现在会先报
        // 参数错误（此前被媒体分支忽略）。
        let offset = match parse_nonnegative_usize(&input, "offset") {
            Ok(Some(n)) => n,
            Ok(None) => 0,
            Err(e) => return e,
        };
        let limit = match parse_positive_usize(&input, "limit") {
            Ok(Some(n)) => n,
            Ok(None) => READ_DEFAULT_LIMIT,
            Err(e) => return e,
        };
        if let Some((kind, mime)) = media_type_from_name(&path) {
            // PDF 按扩展名透明路由（ADR-0003）：文本型 → 本地提取文本（行分页），
            // 扫描/图像/混合/损坏 → 既有附件路径（降级）。其余媒体维持原样。
            if Path::new(&path)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
            {
                return pdf::read_pdf(&path, kind, mime, offset, limit);
            }
            return read_media_as_data_uri(&path, kind, mime);
        }
        let include_metadata = match parse_optional_bool(&input, "include_metadata") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let path_lock = include_metadata.then(|| file_mutation_lock(&path));
        let _guard = match path_lock.as_ref() {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        let metadata = if include_metadata {
            match file_sha256(&path) {
                Ok((sha256, bytes)) => {
                    Some(format!("[read-metadata: sha256={sha256} bytes={bytes}]"))
                }
                Err(error) => return error,
            }
        } else {
            None
        };
        let win = match read_lines_window(&path, offset, limit) {
            Ok(w) => w,
            Err(e) => return e,
        };
        // 整文件（从 0 起、读到末尾、未触扫描上限）→ 原样返回，与旧行为一致。
        if offset == 0 && win.reached_end && !win.scan_truncated {
            return ToolResult::ok(match metadata {
                Some(metadata) => format!("{metadata}\n{}", win.content),
                None => win.content,
            });
        }
        // 否则附翻页提示，便于 agent 决定下一步 offset。先借 win 算提示，再 move 内容。
        let note = read_window_note(&win);
        let mut content = win.content;
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&note);
        if let Some(metadata) = metadata {
            content = format!("{metadata}\n{content}");
        }
        ToolResult::ok(content)
    }
}

// ===========================================================================
// write
// ===========================================================================

pub struct WriteFile {
    base: PathBuf,
}

impl WriteFile {
    pub fn new(base: PathBuf) -> Self {
        WriteFile { base }
    }
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Write or overwrite a file. Pass expected_sha256 from read metadata to reject stale concurrent writes; use \"missing\" for create-only. Paths outside agent storage require unrestricted access."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "path":{"type":"string","description":format!("Absolute path, or path relative to {}/ (outside it requires unrestricted access)", base_str(&self.base))},
                "content":{"type":"string","description":"File content to write"},
                "expected_sha256":{"type":"string","description":"Optional optimistic-concurrency version: 64-hex SHA-256 of current file, or \"missing\" to require nonexistence"}
            },
            "required":["path","content"]
        })
    }
    async fn execute(&self, input: Value) -> ToolResult {
        let path = match get_str(&input, "path") {
            Some(p) => p,
            None => return ToolResult::err("缺少 'path' 字段"),
        };
        let content = match get_str(&input, "content") {
            Some(c) => c,
            None => return ToolResult::err("缺少 'content' 字段"),
        };
        // 与 edit 的整篇载入上限同源：超限内容直接拒绝，避免把异常巨大的
        // 单次输出整篇写盘（读侧校验与哈希同样要全量过一遍）。
        if content.len() > MAX_FILE_SIZE {
            return ToolResult::err(format!(
                "content 超过单次写入上限（{} MB）",
                MAX_FILE_SIZE / 1024 / 1024
            ));
        }
        let path = match resolve_path(&self.base, path, PathOp::Write).await {
            Ok(path) => path,
            Err(error) => return error,
        };
        // 工作区根自己的条目（`agent` / `./agent` / 其绝对路径）不能当文件写：根是真实目录时
        // atomic_write 会拒绝目录，但根是符号链接（`agent -> /data/real`）时 rename 会把这个
        // 入口换成一个普通文件，工作区就此失联。
        if is_workspace_root_entry(&self.base, &path) {
            return ToolResult::err(format!(
                "不能把工作区根目录 {} 当文件写入",
                base_str(&self.base)
            ));
        }
        let path = path.to_string_lossy().into_owned();
        let expected_sha256 = get_str(&input, "expected_sha256");
        if input.get("expected_sha256").is_some() && expected_sha256.is_none() {
            return ToolResult::err("expected_sha256 必须是字符串");
        }
        let path_lock = file_mutation_lock(&path);
        let _guard = path_lock.lock().await;
        if let Err(error) = verify_expected_sha256(&path, expected_sha256) {
            return error;
        }
        // 确保父目录存在，然后原子写。
        if let Err(e) = rclaw_store::ensure_parent_dir(&path) {
            return ToolResult::err(format!("无法创建父目录: {e}"));
        }
        match rclaw_store::atomic_write(&path, content.as_bytes()) {
            Ok(_) => {
                crate::skill_entrypoint::notify_skill_script_changed(Path::new(&path));
                ToolResult::ok(format!(
                    "OK: 已写入 {} 字节到 {path}; sha256={}",
                    content.len(),
                    sha256_bytes(content.as_bytes())
                ))
            }
            Err(StoreError::Io(e)) => ToolResult::err(format!("写入失败: {e}")),
            Err(e) => ToolResult::err(format!("写入失败: {e}")),
        }
    }
}

// ===========================================================================
// delete
// ===========================================================================

pub struct DeleteFile {
    base: PathBuf,
}

impl DeleteFile {
    pub fn new(base: PathBuf) -> Self {
        DeleteFile { base }
    }
}

#[async_trait]
impl Tool for DeleteFile {
    fn name(&self) -> &str {
        "delete"
    }
    fn description(&self) -> &str {
        "Delete a file or directory (directories removed recursively). Paths outside agent storage require unrestricted access."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "path":{"type":"string","description":format!("Absolute path, or path relative to {}/ (outside it requires unrestricted access)", base_str(&self.base))}
            },
            "required":["path"]
        })
    }
    async fn execute(&self, input: Value) -> ToolResult {
        let path = match get_str(&input, "path") {
            Some(p) => p,
            None => return ToolResult::err("缺少 'path' 字段"),
        };
        // 根下叫保留名的【真实目录】允许删（它不可能是库文件，却会让 SQLite 开不了库；
        // 没有这个口子它就永远删不掉）；库文件本身与指向它的链接仍拒绝。
        let path = match resolve_path_with(
            &self.base,
            path,
            PathOp::Delete,
            ReservedDirPolicy::AllowRealDirectory,
        )
        .await
        {
            Ok(p) => p,
            Err(error) => return error,
        };
        // 工作区根（`.` / `agent` / 其绝对路径）及其祖先不能删：目录会连同打开着的
        // state.sqlite 一起被 remove_dir_all，等于绕过数据库守卫删库——与桌面端
        // 「不能删除工作区根目录」同口径。
        if contains_workspace_root(&self.base, &path) {
            return ToolResult::err(format!(
                "不能删除工作区根目录 {}（或其上级目录）：运行时数据库 state.sqlite 在其中；\
                 请逐个删除其下的文件 / 子目录",
                base_str(&self.base)
            ));
        }
        // 文件系统根 / 盘符根 / 挂载点与工作区是什么关系都不能删（Unrestricted 下别的盘的
        // 根不是工作区祖先，上一道拦不住）。
        if is_filesystem_root(&path) {
            return ToolResult::err(format!(
                "不能删除文件系统根 / 盘符根 / 挂载点 {}",
                path.display()
            ));
        }
        // 用 symlink_metadata 判类型（不跟随符号链接）：把符号链接本身当作条目删除，
        // 而非顺着链接删除其目标内容。
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolResult::err(format!("文件不存在：{}", path.display()));
            }
            Err(e) => return ToolResult::err(format!("读取元数据失败：{e}")),
        };
        // 解析时按「真实目录」放行的保留名，到这里若已不是目录（两次 stat 之间被换成了
        // 文件），按库文件口径重新拒绝：remove_file 删掉的会是一个叫保留名的文件。
        if !meta.is_dir() && is_state_db_file(&self.base, &path) {
            return ToolResult::err(format!(
                "拒绝删除运行时数据库文件 {}（state.sqlite 及其伴生文件）",
                path.display()
            ));
        }
        let path = path.to_string_lossy().into_owned();
        let kind = if meta.is_dir() { "目录" } else { "文件" };
        let skill_scripts_before_remove = if meta.is_dir() {
            collect_skill_scripts_under(&path)
        } else {
            Vec::new()
        };
        let result = if meta.is_dir() {
            fs::remove_dir_all(&path)
        } else if meta.file_type().is_symlink() {
            remove_symlink(Path::new(&path))
        } else {
            fs::remove_file(&path)
        };
        match result {
            Ok(()) => {
                // 目录删除时其中的 skills 树脚本同样要摘审批：notify 只认 .py
                // 文件路径，删除前先收集（ collected 在 remove 之前拿好）。
                for script in &skill_scripts_before_remove {
                    crate::skill_entrypoint::notify_skill_script_changed(Path::new(script));
                }
                if !meta.is_dir() {
                    crate::skill_entrypoint::notify_skill_script_changed(Path::new(&path));
                }
                ToolResult::ok(format!("OK: 已删除{kind} {path}"))
            }
            Err(e) => ToolResult::err(format!("删除失败：{e}")),
        }
    }
}

// ===========================================================================
// edit
// ===========================================================================

pub struct EditFile {
    base: PathBuf,
}

impl EditFile {
    pub fn new(base: PathBuf) -> Self {
        EditFile { base }
    }
}

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Find and replace text in a file. Pass expected_sha256 from read metadata to reject stale concurrent edits. Paths outside agent storage require unrestricted access. Replaces the first occurrence."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "path":{"type":"string","description":format!("Absolute path, or path relative to {}/ (outside it requires unrestricted access)", base_str(&self.base))},
                "old_string":{"type":"string","description":"Text to find"},
                "new_string":{"type":"string","description":"Replacement text"},
                "expected_sha256":{"type":"string","description":"Optional optimistic-concurrency version: 64-hex SHA-256 of current file"}
            },
            "required":["path","old_string","new_string"]
        })
    }
    async fn execute(&self, input: Value) -> ToolResult {
        let path = match get_str(&input, "path") {
            Some(p) => p,
            None => return ToolResult::err("缺少 'path' 字段"),
        };
        let old_str = match get_str(&input, "old_string") {
            Some(s) => s,
            None => return ToolResult::err("缺少 'old_string' 字段"),
        };
        // 显式拒绝空 old_string：`str::replacen("", ...)` 会在文件开头插入内容而非
        // 「未找到」，空 old_string 会变成意料之外的写入。要求必须给出非空查找串。
        if old_str.is_empty() {
            return ToolResult::err("old_string 不能为空");
        }
        let new_str = match get_str(&input, "new_string") {
            Some(s) => s,
            None => return ToolResult::err("缺少 'new_string' 字段"),
        };
        let path = match resolve_path(&self.base, path, PathOp::Write).await {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(error) => return error,
        };
        let expected_sha256 = get_str(&input, "expected_sha256");
        if input.get("expected_sha256").is_some() && expected_sha256.is_none() {
            return ToolResult::err("expected_sha256 必须是字符串");
        }
        let path_lock = file_mutation_lock(&path);
        let _guard = path_lock.lock().await;
        if let Err(error) = verify_expected_sha256(&path, expected_sha256) {
            return error;
        }
        // 限流读取（与 read 同源）：避免把超大文件整篇 read_to_string 进内存后才判超限。
        // read_text_bounded 内部已做 metadata + take(MAX+1) 的体积拦截。
        let content = match read_text_bounded(&path) {
            Ok(c) => c,
            Err(e) => return e,
        };
        // 只替换第一处。
        let result = match content.replacen(old_str, new_str, 1) {
            r if r.len() == content.len() && !content.contains(old_str) => {
                return ToolResult::err(format!("未在 {path} 中找到 old_string"));
            }
            r => r,
        };
        if let Err(e) = rclaw_store::atomic_write(&path, result.as_bytes()) {
            return ToolResult::err(format!("写回失败: {e}"));
        }
        crate::skill_entrypoint::notify_skill_script_changed(Path::new(&path));
        ToolResult::ok(format!(
            "OK: 已编辑 {path}（替换 {} 字节为 {} 字节）；sha256={}",
            old_str.len(),
            new_str.len(),
            sha256_bytes(result.as_bytes())
        ))
    }
}

// ===========================================================================
// list
// ===========================================================================

pub struct ListDir {
    base: PathBuf,
}

impl ListDir {
    pub fn new(base: PathBuf) -> Self {
        ListDir { base }
    }
}

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "list"
    }
    fn description(&self) -> &str {
        "List files under agent storage, optionally filtered by prefix. Paths outside agent storage require unrestricted access."
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "path":{"type":"string","description":"Optional absolute root or agent-storage-relative directory to list (defaults to agent storage). Outside paths require unrestricted access."},
                "prefix":{"type":"string","description":format!("Optional path prefix filter, e.g. {}/memory/", base_str(&self.base))}
            },
            "required":[]
        })
    }
    async fn execute(&self, input: Value) -> ToolResult {
        let prefix = get_str(&input, "prefix").unwrap_or("");
        // 可选 path：遍历根（默认 agent 存储根）。agent/ 之外路径需 unrestricted 访问——
        // Sandboxed 下 resolve_path 会拒绝；Unrestricted 下可列任意目录。
        let root: PathBuf = match get_str(&input, "path") {
            Some(p) if !p.is_empty() => match resolve_path(&self.base, p, PathOp::List).await {
                Ok(path) => path,
                Err(error) => return error,
            },
            _ => {
                let _ = rclaw_store::ensure_dir(&self.base);
                self.base.clone()
            }
        };
        // Android 路径别名（/data/user/0/<pkg> ↔ /data/data/<pkg>）：resolve_path
        // 归一后的遍历根与调用者书写的 base 域可能不同域，条目展示路径若用归一域，
        // 调用者按 base 域书写的 prefix 永远匹配不上。展示统一重映射回调用者域。
        let display_root = normalize_alias(&self.base, &root);
        let entries = match collect_entries(&root, &display_root, prefix) {
            Ok(e) => e,
            Err(e) => return ToolResult::err(format!("无法列出 {}: {e}", root.display())),
        };
        if entries.is_empty() {
            return ToolResult::ok("(未找到文件)");
        }
        ToolResult::ok(entries.join("\n"))
    }
}

/// 把 root 中「canonical(base) 的前缀」重写回 base 的字符串域；无别名时原样返回。
fn normalize_alias(base: &Path, root: &Path) -> PathBuf {
    let Ok(canonical_base) = base.canonicalize() else {
        return root.to_path_buf();
    };
    if canonical_base == base {
        return root.to_path_buf();
    }
    match root.strip_prefix(&canonical_base) {
        Ok(tail) => base.join(tail),
        Err(_) => root.to_path_buf(),
    }
}

/// 递归收集 base 下相对路径以 prefix 过滤的文件清单。
/// `display_base` 是条目展示路径的前缀域（可能与遍历用的真实根存在别名差异）。
fn collect_entries(base: &Path, display_base: &Path, prefix: &str) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    // 从深度 0（base 自身这一层）开始递归。
    walk_depth(base, base, display_base, prefix, &mut out, 0)?;
    Ok(out)
}

/// `collect_entries` 的递归实现（带深度与输出上限）。
///
/// 不再使用单参 `walk`：加上 `depth` 既能做递归深度上限，也让签名清晰表达「这是
/// 有界递归」。
fn walk_depth(
    base: &Path,
    dir: &Path,
    display_base: &Path,
    prefix: &str,
    out: &mut Vec<String>,
    depth: usize,
) -> std::io::Result<()> {
    // 深度上限：阻断 symlink 环（指向自身或祖先的符号链接）与异常嵌套导致的无限递归。
    if depth >= MAX_WALK_DEPTH {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        // 输出条目上限：达到即停止追加，避免海量结果灌满上下文。
        if out.len() >= MAX_WALK_ENTRIES {
            return Ok(());
        }
        let entry = entry?;
        let path = entry.path();
        // 关键安全点：用 symlink_metadata（【不跟随】符号链接）判断类型。
        // 旧的 `path.is_dir()` / `fs::metadata` 会跟随 symlink——若 agent/ 下存在指向
        // base 之外的符号链接（或指向祖先形成环），就会把外部文件当作 base 内文件列出、
        // 甚至无限递归。这里绝不跟随符号链接目录。
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            // 条目元信息获取失败（权限/竞态删除等）跳过，不影响其余条目。
            Err(_) => continue,
        };
        let is_symlink = meta.file_type().is_symlink();
        let is_real_dir = meta.is_dir() && !is_symlink;
        // 以 display_base 为根的相对展示路径（条目自身的路径仍在 base 下，列出它是安全的）。
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let full = format!("{}/{}", base_str(display_base), rel);
        // 把条目本身列出（其路径位于 base 下）；符号链接【绝不递归跟随】。
        if prefix.is_empty() || full.starts_with(prefix) {
            out.push(full);
        }
        if is_real_dir {
            walk_depth(base, &path, display_base, prefix, out, depth + 1)?;
        }
    }
    Ok(())
}

// ===========================================================================
// grep
// ===========================================================================

/// ripgrep 风格的进程内正则搜索工具（不调用外部 `rg` 二进制）。
///
/// 动机：`read` 受上下文预算约束，无法整篇灌入大文件；搜索场景下 agent 需要
/// 「在文件（树）中定位匹配行」而非「读全文」。本工具用 [`Regex`] + [`Walkdir`] 进程内
/// 完成，并通过 [`resolve_path`] 应用当前权限的路径解析与限制，跨平台零外部依赖
/// （Termux / Windows / Linux 行为一致），且不引入命令执行面。
///
/// 安全约束（与 list 同源）：
/// - 搜索根 `path` 须经 [`resolve_path`] 校验；Sandboxed 权限只能访问 base 内路径；
/// - 目录递归 `follow_links(false)` + 深度上限 [`MAX_GREP_DEPTH`]，【不跟随符号链接目录】，
///   杜绝经符号链接逃逸或成环递归；
/// - 子符号链接文件不被搜索（`is_file()` 过滤掉 symlink），避免读到 base 之外。
pub struct Grep {
    base: PathBuf,
}

impl Grep {
    pub fn new(base: PathBuf) -> Self {
        Grep { base }
    }
}

/// grep 的命中累加器（贯穿单文件搜索与目录遍历）。
struct GrepAccum {
    out: String,
    matches: usize,
    bytes_out: usize,
    capped_files: u32,
    /// 已达 max_results 或输出字节上限，应停止后续扫描。
    full: bool,
}

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents with a regex (ripgrep-style, in-process). \
         Path may be a file or a directory (searched recursively). \
         Paths outside agent storage require unrestricted access. \
         Returns `path:line: matched-line`, capped."
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "pattern":{"type":"string","description":"正则表达式（Rust regex 语法）"},
                "path":{"type":"string","description":format!("Absolute search root, or file/directory relative to {}/; outside paths require unrestricted access", base_str(&self.base))},
                "max_results":{"type":"integer","minimum":1,"description":format!("最多返回匹配数（默认 {MAX_GREP_RESULTS}）")}
            },
            "required":["pattern","path"]
        })
    }
    async fn execute(&self, input: Value) -> ToolResult {
        let pattern = match get_str(&input, "pattern") {
            Some(p) => p,
            None => return ToolResult::err("缺少 'pattern' 字段"),
        };
        let path = match get_str(&input, "path") {
            Some(p) => p,
            None => return ToolResult::err("缺少 'path' 字段"),
        };
        let max_results = match parse_positive_usize(&input, "max_results") {
            Ok(Some(n)) => n.min(MAX_GREP_RESULTS),
            Ok(None) => MAX_GREP_RESULTS,
            Err(e) => return e,
        };
        let re = match Regex::new(pattern) {
            Ok(re) => re,
            Err(e) => return ToolResult::err(format!("非法正则: {e}")),
        };
        let path = match resolve_path(&self.base, path, PathOp::Search).await {
            Ok(path) => path,
            Err(error) => return error,
        };
        let display_path = path.to_string_lossy().into_owned();
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return ToolResult::err(format!("读取失败 {display_path}: {e}")),
        };

        let mut acc = GrepAccum {
            out: String::new(),
            matches: 0,
            bytes_out: 0,
            capped_files: 0,
            full: false,
        };
        if meta.is_file() {
            search_file(&mut acc, &re, &display_path, &display_path, max_results);
        } else if meta.is_dir() {
            for entry in WalkDir::new(&path)
                .follow_links(false)
                .max_depth(MAX_GREP_DEPTH)
                .into_iter()
            {
                if acc.full {
                    break;
                }
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue, // 权限/竞态等跳过单条目，不影响其余。
                };
                // 只搜普通文件；符号链接（is_file()==false）跳过，绝不跟随。
                if !entry.file_type().is_file() {
                    continue;
                }
                // 运行时数据库不参与全文搜索：read 会拒绝它，grep 从目录遍历进来也不能
                // 绕过（否则把二进制页里的会话明文当匹配行喂回模型）。
                if walk_entry_is_state_db(&self.base, entry.path()) {
                    continue;
                }
                let display = entry.path().to_string_lossy().replace('\\', "/");
                search_file(&mut acc, &re, &display, &display, max_results);
            }
        } else {
            return ToolResult::err(format!("无法搜索 {display_path}: 目标为特殊文件"));
        }

        finish_grep(acc, pattern)
    }
}

/// 汇总 grep 结果并附上截断提示。
fn finish_grep(acc: GrepAccum, pattern: &str) -> ToolResult {
    let mut content = if acc.matches == 0 {
        format!("未找到匹配（pattern={pattern}）")
    } else {
        acc.out
    };
    // 限流可观测化（同 list「达上限即停」）：把截断原因告诉 agent。
    let mut trailer = String::new();
    if acc.full {
        trailer.push_str("已达返回上限，仍有更多匹配未展示");
    }
    if acc.capped_files > 0 {
        if !trailer.is_empty() {
            trailer.push('；');
        }
        trailer.push_str(&format!("{} 个大文件仅扫描了前缀", acc.capped_files));
    }
    if !trailer.is_empty() {
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&format!("[{trailer}]\n"));
    }
    ToolResult::ok(content)
}

/// 超长匹配行截断，保护输出体积（与 list 条目上限同源）。
fn truncate_line(line: &str) -> String {
    let bytes = line.as_bytes();
    if bytes.len() <= MAX_GREP_LINE_BYTES {
        return line.to_string();
    }
    let cut = floor_char_boundary(bytes, MAX_GREP_LINE_BYTES);
    let mut s = String::with_capacity(cut + " …[截断]".len());
    // line 已是合法 UTF-8（调用前校验），cut 为字符边界，切片安全。
    s.push_str(std::str::from_utf8(&bytes[..cut]).unwrap_or(""));
    s.push_str(" …[截断]");
    s
}

/// 在单个文件内逐行正则搜索，命中累加进 `acc`。见 [`Grep`]。
///
/// - `read_until(b'\n')` 流式逐行读，整文件不进内存；
/// - 行内出现 NUL 视为二进制文件，停止扫描本文件（同 ripgrep 默认）；
/// - 非 UTF-8 行跳过（不中断其余行/文件）；
/// - 单文件扫描字节达 [`MAX_GREP_FILE_SCAN`] 即停（大文件只搜前缀）。
fn search_file(
    acc: &mut GrepAccum,
    re: &Regex,
    display: &str,
    real_path: &str,
    max_results: usize,
) {
    // 非阻塞打开 + 句柄类型核对：遍历到的条目在 walk 与 open 之间被换成 FIFO 也挂不住。
    let f = match open_regular_file(Path::new(real_path)) {
        Ok(f) => f,
        Err(_) => return, // 打不开（权限/竞态/非普通文件）跳过单个文件。
    };
    let mut reader = std::io::BufReader::new(f);
    let mut buf = Vec::new();
    let mut lineno = 0usize;
    let mut scanned = 0usize;
    loop {
        if acc.full || acc.matches >= max_results {
            return;
        }
        buf.clear();
        // 单行读取受扫描上限约束：无换行的超大文件否则会在按行检查上限前整段进内存。
        let remaining = MAX_GREP_FILE_SCAN.saturating_sub(scanned).max(1) as u64;
        let n = match reader.by_ref().take(remaining).read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(_) => return,
        };
        if n == 0 {
            return; // EOF
        }
        if n as u64 == remaining && buf.last() != Some(&b'\n') {
            // 行被上限截断：视为扫描触顶（大文件只搜前缀），半行不参与匹配。
            acc.capped_files += 1;
            return;
        }
        lineno += 1;
        if buf.contains(&0u8) {
            return; // 二进制文件：跳过（同 rg）。
        }
        scanned += n;
        // 扫描上限必须在【逐行 continue 之前】判定：非 UTF-8 行会 continue 跳过下方匹配
        // 逻辑，若把上限检查放在循环末尾，这类行会绕过扫描阀，让超大非文本文件被搜遍
        // 无遗（时间无界）。
        if scanned >= MAX_GREP_FILE_SCAN {
            acc.capped_files += 1;
            return;
        }
        let line = match std::str::from_utf8(&buf) {
            Ok(s) => s.trim_end_matches(['\n', '\r']),
            Err(_) => continue, // 非 UTF-8 行跳过。
        };
        if re.is_match(line) {
            let entry = format!("{}:{}: {}\n", display, lineno, truncate_line(line));
            if acc.bytes_out + entry.len() > MAX_GREP_OUTPUT_BYTES {
                acc.full = true;
                return;
            }
            acc.out.push_str(&entry);
            acc.bytes_out += entry.len();
            acc.matches += 1;
            if acc.matches >= max_results {
                acc.full = true;
                return;
            }
        }
    }
}

/// 删除目录前收集其中位于 skills 树的 .py 脚本路径（notify_skill_script_changed
/// 只认 .py 文件路径，目录本身会被忽略）。file_type 不跟随符号链接。
fn collect_skill_scripts_under(dir: &str) -> Vec<String> {
    fn walk(dir: &Path, out: &mut Vec<String>, depth: usize) {
        if depth > 6 {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                walk(&path, out, depth + 1);
            } else if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("py"))
                && path
                    .components()
                    .any(|component| component.as_os_str() == std::ffi::OsStr::new("skills"))
            {
                out.push(path.to_string_lossy().into_owned());
            }
        }
    }
    let mut out = Vec::new();
    walk(Path::new(dir), &mut out, 0);
    out
}

#[cfg(test)]
mod tests;
