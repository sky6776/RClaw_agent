//! Layer 5 选点回滚（ZCode rewind/checkpoint 同源）。
//!
//! write/edit/delete 在前台路径执行时捕获文件级 checkpoint（before 内容 + 前后
//! SHA-256，按「发起写入的迭代分组 id」归组写入 `state.sqlite` 的 `file_checkpoint` 表，
//! 与同一迭代的工具链两条在同一事务提交）；`rewind_files` 工具与 `/rewind` 斜杠命令按
//! 分组 id 选点恢复——恢复前逐一对比回滚集内每个文件的当前 hash 与它【最新】checkpoint
//! 的 after_hash，任何文件被外部改动即整组拒绝；写入阶段若中途失败，按逆序撤回已应用的
//! 文件（all-or-nothing，绝不盲恢复、不留半截工作区）。会话历史不动，只回滚文件。
//!
//! 分组 id（用户可见名称：**回滚点 id**）是纳秒单调时间戳——`Message.id` 是数据库行 id，
//! 另一套身份，不混用，用户界面与工具参数也不叫它 message id；`/rewind` 列表同时给出
//! id 对应的时间，便于定位。
//!
//! 列表 / 校验只读元数据（[`SessionStore::load_file_checkpoint_meta`]，不含 `before` 正文），
//! 回滚集的正文按行 id 范围单独读取：checkpoint 永不清理，`before` 单条可达 2MB。
//! 校验分两层：回滚集内每个文件的当前 hash 必须等于它【最新】checkpoint 的 after_hash
//! （选点之后没有外部改动）；同一文件在回滚集内相邻 checkpoint 必须首尾相接
//! （`cur.before_hash == prev.after_hash`，回滚标记重锚），否则两次 checkpoint 之间存在
//! 未记录的改动（shell / 外部编辑），还原到最早的 `before` 会把它悄悄抹掉。
//!
//! 已知边界（v1）：只覆盖结构化文件工具（write/edit/delete）对【UTF-8 文本、≤ 2MB】
//! 文件的写入；非 UTF-8 / 超限 / 目录操作 / 符号链接条目不做 checkpoint（无法恢复的写入不做半吊子
//! 记录）；shell/skill 脚本的副作用与子代理（Subagent 策略）写入不在覆盖范围。沙箱模式下
//! 对某个工作区外文件的【首次】放行若是写入 / 删除，这一次同样没有 checkpoint：before 捕获
//! 跑在工具自己的授权门之前（见 [`capture_before`]），用户还没点允许时一个字节都不读，
//! 所以只有该文件放行之后的写入 / 删除才有回滚点。授权弹窗里有同样的提示。

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use rclaw_store::session::{FileCheckpointMeta, FileCheckpointRecord, SessionStore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// 单文件 checkpoint 的内联内容上限（超过则不捕获——无法恢复的写入不做半吊子记录）。
const MAX_CHECKPOINT_BYTES: u64 = 2 * 1024 * 1024;
/// 列表模式返回的最近分组数。
const LIST_LIMIT: usize = 20;

/// 一次文件写入的前后快照（尚未归属分组 id 的暂存形态）。
#[derive(Debug, Clone)]
pub(crate) struct FileCheckpoint {
    pub(crate) path: String,
    /// 写入前内容；None = 写入前文件不存在（回滚即删除）。
    pub(crate) before: Option<String>,
    pub(crate) before_hash: Option<String>,
    /// 写入后内容 hash；None = 写入后文件不存在（delete）。
    pub(crate) after_hash: Option<String>,
    /// 回滚操作的 synthetic 标记：参与后续校验链（重锚磁盘状态），
    /// 但不进入回滚集（回滚它等于撤销回滚）。
    pub(crate) is_rewind: bool,
}

impl FileCheckpoint {
    /// 转成 store 记录（`group_id = 0` → 由写入时的分组参数决定）。
    pub(crate) fn into_record(self) -> FileCheckpointRecord {
        FileCheckpointRecord {
            id: 0,
            group_id: 0,
            path: self.path,
            before: self.before,
            before_hash: self.before_hash,
            after_hash: self.after_hash,
            is_rewind: self.is_rewind,
        }
    }
}

/// 把本迭代捕获的 checkpoint 批量转成 store 记录。
pub(crate) fn into_records(checkpoints: Vec<FileCheckpoint>) -> Vec<FileCheckpointRecord> {
    checkpoints
        .into_iter()
        .map(FileCheckpoint::into_record)
        .collect()
}

tokio::task_local! {
    /// 当前会话 (channel, chat_id)：`rewind_files` 工具定位 checkpoint 存储。
    pub(crate) static CURRENT_CHAT: Option<(String, String)>;
}

/// 当前会话键；作用域外（嵌入方直调工具）返回 None。
pub(crate) fn current_chat() -> Option<(String, String)> {
    CURRENT_CHAT.try_with(std::clone::Clone::clone).ok().flatten()
}

/// 生成 rewind 分组 id（回滚点 id）：纳秒时间戳 + 原子单调保障（同一进程内严格递增）。
///
/// 该 id 只用于 checkpoint 分组（与 `Message.id` 无关）；回滚集的选取按记录的行序
/// （= 时间序），不依赖 id 跨进程 / 跨时钟回拨的单调性。
pub(crate) fn next_rewind_id() -> i64 {
    static LAST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    use std::sync::atomic::Ordering;
    let mut now = chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_default();
    loop {
        let last = LAST.load(Ordering::Relaxed);
        if now <= last {
            now = last + 1;
        }
        if LAST
            .compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return now;
        }
    }
}

/// 回滚点 id（纳秒）对应的本地时间，供列表定位。
pub(crate) fn format_rewind_time(rewind_id: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_nanos(rewind_id)
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// 条目是符号链接时的错误：checkpoint 只记普通文件（见 [`capture_before`]），路径上现在是
/// 链接条目就是外部改动——跟随链接比对的是目标的内容，比对通过后 `atomic_write` 又会把
/// 链接条目换成普通文件。
fn symlink_entry_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, "目标是符号链接条目")
}

/// 读取当前文件内容：Ok(None) = 文件不存在；Err = 无法读取（权限 / 目录 / 符号链接条目 /
/// 非普通文件等）。按条目看（`symlink_metadata`，不跟随链接）。打开走非阻塞 + 句柄复核
/// （[`rclaw_tools::files::open_regular_file`]）：checkpoint 之后被换成 FIFO 的路径按「无法
/// 读取」拒绝回滚，而不是把恢复线程挂在 `open` 上。
fn read_file_bytes(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read as _;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => return Err(symlink_entry_error()),
        Ok(meta) if meta.is_dir() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::IsADirectory,
                "目标是目录",
            ))
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let mut file = match rclaw_tools::files::open_regular_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// 当前文件内容 hash（流式，不整文件读入内存）：Ok(None) = 文件不存在。
///
/// 与 `capture_before` 同一口径：先按条目元数据（`symlink_metadata`）确认是普通文件再
/// `open`——工具返回到这里之间路径若被换成 FIFO / 设备，先打开再判类型会把 runtime 线程挂在
/// `open` 上；条目被换成符号链接则按「无法读取」处理（见 [`symlink_entry_error`]）；open 本身
/// 也走非阻塞 + 句柄复核（[`rclaw_tools::files::open_regular_file`]），metadata 与 open 之间
/// 再被换掉也挂不住。
fn current_file_hash(path: &Path) -> std::io::Result<Option<String>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if meta.file_type().is_symlink() {
        return Err(symlink_entry_error());
    }
    if meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::IsADirectory,
            "目标是目录",
        ));
    }
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "目标不是普通文件",
        ));
    }
    let mut file = match rclaw_tools::files::open_regular_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(Some(format!("{:x}", hasher.finalize())))
}

/// 与文件工具 `resolve_path` 同一套路径语义（绝对路径原义；相对路径基于 agent 根；
/// 显式带 agent 根目录名的旧式相对路径基于其父目录），再做纯词法规范化（`.` / `..`
/// 折叠，不访问文件系统）。工具在 Unrestricted 权限下放行的 `..` 路径同样被折叠记录，
/// 沙箱模式下工具会拒绝它们（执行失败 → 不入账），两边都不会记错文件。
fn resolve_like_file_tool(raw: &Path, base: &Path) -> Option<PathBuf> {
    let joined = if raw.is_absolute() || raw.starts_with(base) {
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
    let absolute = if joined.is_absolute() {
        joined
    } else {
        std::env::current_dir().ok()?.join(joined)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Some(normalized)
}

/// 记录里的路径写法：去掉 Windows verbatim 前缀（`\\?\`），统一正斜杠。
fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text.into_owned()
    };
    text.replace('\\', "/")
}

/// 同一文件的比较键：Windows 路径大小写不敏感，按小写折叠，避免 `A.md` / `a.md`
/// 被当成两个文件而误判「外部改动」。
#[cfg(windows)]
fn path_key(path: &str) -> String {
    path.to_lowercase()
}

#[cfg(not(windows))]
fn path_key(path: &str) -> String {
    path.to_string()
}

/// write/edit/delete → 解析目标路径并捕获 before 内容。
///
/// 非文件写入工具 / 非普通文件 / 内容超限 / 非 UTF-8 / 无法读取 → None（不做 checkpoint）。
/// 「文件不存在」与「无法读取」严格区分：前者记为 before=None（回滚即删除），
/// 后者放弃记录——否则一次权限错误会让回滚把用户的原文件当成「新建文件」删掉。
///
/// 路径按【条目】看（`symlink_metadata`，不跟随链接）：叶子是符号链接就不做 checkpoint。
/// `delete` 对链接只摘条目、`write` 的原子写会把链接条目换成普通文件，目标文件都没变；
/// 若跟随链接把【目标】的内容记成 before，回滚会把条目「还原」成一个装着目标内容的普通
/// 文件——既没恢复链接，目标也没动过，不是任何一方曾有过的状态。与目录操作同列为不覆盖。
///
/// 这一步跑在工具自己的路径门【之前】，所以自己也得守门：沙箱下工作区外、用户尚未放行的
/// 路径一个字节都不读（否则被工具拒掉的 `write /etc/shadow` 也会先把原文抄进回滚点）。
/// 代价是刻意接受的：用户在弹窗里放行的那一次写入 / 删除没有回滚点（放行发生在这一步之后，
/// 工具成功时 before 已经来不及捕获），该文件之后的操作才有——模块文档与授权弹窗都写明了。
/// 只读普通文件（FIFO 的 open 会一直等写端、`/dev/zero` 读不到头），且按上限 `take`，不信
/// metadata 报的长度。
pub(crate) fn capture_before(
    tool_name: &str,
    input: &Value,
    base: &Path,
) -> Option<FileCheckpoint> {
    if !matches!(tool_name, "write" | "edit" | "delete") {
        return None;
    }
    let raw_path = input.get("path").and_then(Value::as_str)?;
    if raw_path.is_empty() {
        return None;
    }
    let resolved = resolve_like_file_tool(Path::new(raw_path), base)?;
    if !rclaw_tools::access::path_readable_now(base, &resolved) {
        tracing::debug!(
            "rewind：{} 在工作区外且未放行，不记录 checkpoint",
            resolved.display()
        );
        return None;
    }
    let before = match std::fs::symlink_metadata(&resolved) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                tracing::debug!(
                    "rewind：{} 是符号链接条目，不记录 checkpoint",
                    resolved.display()
                );
                return None;
            }
            if !meta.is_file() {
                return None;
            }
            if meta.len() > MAX_CHECKPOINT_BYTES {
                tracing::debug!(
                    "rewind：{} 超过 checkpoint 上限，不记录",
                    resolved.display()
                );
                return None;
            }
            match read_bounded(&resolved) {
                Ok(Some(bytes)) => match String::from_utf8(bytes) {
                    Ok(text) => Some(text),
                    Err(_) => {
                        tracing::debug!(
                            "rewind：{} 不是 UTF-8 文本，不记录",
                            resolved.display()
                        );
                        return None;
                    }
                },
                Ok(None) => {
                    tracing::debug!(
                        "rewind：{} 超过 checkpoint 上限，不记录",
                        resolved.display()
                    );
                    return None;
                }
                Err(error) => {
                    tracing::warn!(
                        "rewind：读取 {} 失败，不记录 checkpoint: {error}",
                        resolved.display()
                    );
                    return None;
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(
                "rewind：无法访问 {}，不记录 checkpoint: {error}",
                resolved.display()
            );
            return None;
        }
    };
    let before_hash = before.as_deref().map(|text| hash_bytes(text.as_bytes()));
    Some(FileCheckpoint {
        path: display_path(&resolved),
        before,
        before_hash,
        after_hash: None,
        is_rewind: false,
    })
}

/// 按 [`MAX_CHECKPOINT_BYTES`] 上限读取：读到上限 +1 字节即 `Ok(None)`（文件在 stat 之后
/// 长大了也不会把内存吃满）。打开走非阻塞 + 句柄复核：stat 之后被换成 FIFO 也挂不住。
fn read_bounded(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read as _;
    let file = rclaw_tools::files::open_regular_file(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_CHECKPOINT_BYTES + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= MAX_CHECKPOINT_BYTES).then_some(bytes))
}

/// 执行成功后回填 after_hash；内容未变化（no-op 写入）或写入后无法读取 → false（不入账）。
pub(crate) fn finish_capture(checkpoint: &mut FileCheckpoint) -> bool {
    let after_hash = match current_file_hash(Path::new(&checkpoint.path)) {
        Ok(hash) => hash,
        Err(error) => {
            tracing::warn!(
                "rewind：读取 {} 写入后内容失败，放弃 checkpoint: {error}",
                checkpoint.path
            );
            return false;
        }
    };
    if after_hash == checkpoint.before_hash {
        return false;
    }
    checkpoint.after_hash = after_hash;
    true
}

/// 把本迭代捕获的文件 checkpoint 以分组 id 单独落库（cron turn / 取消清理等不与工具链
/// 同事务的路径；尽力而为，失败只告警）。前台路径用
/// [`SessionStore::append_tool_chain_with_checkpoints`] 与工具链同事务提交。
pub(crate) fn flush(
    session: &SessionStore,
    channel: &str,
    chat_id: &str,
    group_id: i64,
    checkpoints: Vec<FileCheckpoint>,
) {
    flush_records(session, channel, chat_id, group_id, &into_records(checkpoints));
}

/// [`flush`] 的记录形态：工具链同事务写入失败后的补落库路径复用（记录已转换）。
pub(crate) fn flush_records(
    session: &SessionStore,
    channel: &str,
    chat_id: &str,
    group_id: i64,
    checkpoints: &[FileCheckpointRecord],
) {
    if checkpoints.is_empty() {
        return;
    }
    if let Err(error) = session.append_file_checkpoints(channel, chat_id, group_id, checkpoints) {
        tracing::warn!("rewind checkpoint 写入失败: {error}");
    }
}

/// `rewind_files` 工具：按回滚点 id 选点恢复文件（无 `rewind_id` 时列出最近分组）。
pub struct RewindFilesTool {
    session: SessionStore,
}

impl RewindFilesTool {
    pub fn new(session: SessionStore) -> Self {
        RewindFilesTool { session }
    }
}

/// 工具入参里的回滚点 id：规范名 `rewind_id`；`message_id` 作为旧名继续接受（模型可能
/// 沿用早期提示里的叫法），但它从来不是 `Message.id`。
fn rewind_id_arg(input: &Value) -> Option<&Value> {
    input
        .get("rewind_id")
        .or_else(|| input.get("message_id"))
        .filter(|value| !value.is_null())
}

#[async_trait::async_trait]
impl rclaw_tools::Tool for RewindFilesTool {
    fn name(&self) -> &str {
        "rewind_files"
    }

    fn description(&self) -> &str {
        "Rewind workspace files to the state BEFORE a specific rewind point (checkpoint \
         granularity: every write/edit/delete of a UTF-8 text file (<= 2MB) this session made \
         is journaled with before content and after hash, grouped per tool iteration). Without \
         rewind_id, lists recent rewind points. Restore is all-or-nothing and refuses when any \
         file in the rollback set was modified outside the checkpoints (hash mismatch) — \
         resolve that manually first. Not checkpointed: shell side effects, subagent writes, \
         paths whose final component is a symlink (the link entry is never read or restored), \
         and — under sandboxed access — the FIRST user-approved write/delete of a file outside \
         the workspace (its content is not read before approval). Session history is NOT \
         touched, only files."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "rewind_id": {"type": "integer", "description": "Rewind point id to restore to (exactly as shown in the list / /rewind output; it is NOT a message id). Omit to list rewind points."}
            }
        })
    }

    async fn execute(&self, input: Value) -> rclaw_tools::ToolResult {
        let Some((channel, chat_id)) = current_chat() else {
            return rclaw_tools::ToolResult::err(
                "rewind_files 只能在会话 turn 内使用（缺少会话上下文）".to_string(),
            );
        };
        let session = self.session.clone();
        match rewind_id_arg(&input) {
            Some(value) => {
                let Some(rewind_id) = value.as_i64() else {
                    return rclaw_tools::ToolResult::err(
                        "rewind_id 必须是整数（原样使用列表给出的回滚点 id）".to_string(),
                    );
                };
                let restored = tokio::task::spawn_blocking(move || {
                    restore_to_point(&session, &channel, &chat_id, rewind_id)
                })
                .await;
                match restored {
                    Ok(Ok(report)) => rclaw_tools::ToolResult::ok(format!(
                        "{report}\n\n注意：会话历史未回滚；如需继续在被回滚前的方向上工作，请明确说明后续计划。"
                    )),
                    Ok(Err(error)) => rclaw_tools::ToolResult::err(error),
                    Err(error) => {
                        rclaw_tools::ToolResult::err(format!("rewind 任务执行异常: {error}"))
                    }
                }
            }
            _ => {
                let groups = tokio::task::spawn_blocking(move || {
                    list_groups(&session, &channel, &chat_id)
                })
                .await
                .unwrap_or_default();
                if groups.is_empty() {
                    return rclaw_tools::ToolResult::ok(
                        "本会话还没有可回滚点（仅 write/edit/delete 会被 checkpoint）。".to_string(),
                    );
                }
                let mut out = format!("可回滚点（最近 {} 组，新在前）：", groups.len());
                for (rewind_id, files) in &groups {
                    out.push_str(&format!(
                        "\n- 回滚点 {rewind_id}（{}，{} 个文件）",
                        format_rewind_time(*rewind_id),
                        files.len()
                    ));
                    for path in files {
                        out.push_str(&format!("\n  - {path}"));
                    }
                }
                rclaw_tools::ToolResult::ok(out)
            }
        }
    }
}

/// 该会话全部 checkpoint 的元数据（不含 `before`），行序 = 时间序；读库失败按空处理（告警）。
fn load_meta(session: &SessionStore, channel: &str, chat_id: &str) -> Vec<FileCheckpointMeta> {
    match session.load_file_checkpoint_meta(channel, chat_id) {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!("读取 rewind checkpoint 失败: {error}");
            Vec::new()
        }
    }
}

/// 列出最近的 checkpoint 分组（回滚点 id → 文件列表），新在前。
pub(crate) fn list_groups(
    session: &SessionStore,
    channel: &str,
    chat_id: &str,
) -> Vec<(i64, Vec<String>)> {
    let mut groups: Vec<(i64, Vec<String>)> = Vec::new();
    for record in load_meta(session, channel, chat_id)
        .into_iter()
        .filter(|record| !record.is_rewind)
    {
        match groups.iter_mut().find(|(id, _)| *id == record.group_id) {
            Some((_, files)) => files.push(record.path),
            None => groups.push((record.group_id, vec![record.path])),
        }
    }
    groups.reverse();
    groups.truncate(LIST_LIMIT);
    groups
}

/// 校验链：回滚集内同一文件的相邻 checkpoint 必须首尾相接。
///
/// 返回链条断裂的文件（去重、按首次出现顺序）。`records` 为选点行起的全部元数据（含回滚
/// 标记，行序 = 时间序）；回滚标记没有 `before_hash`，自身不参与「接上前一条」的比对，
/// 但它的 `after_hash` 是其后写入的新起点。
fn broken_chains(records: &[FileCheckpointMeta]) -> Vec<String> {
    let mut last_after: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    let mut broken: Vec<String> = Vec::new();
    for record in records {
        let key = path_key(&record.path);
        if let Some(previous_after) = last_after.get(&key) {
            if !record.is_rewind
                && record.before_hash != *previous_after
                && !broken.iter().any(|path| path_key(path) == key)
            {
                broken.push(record.path.clone());
            }
        }
        last_after.insert(key, record.after_hash.clone());
    }
    broken
}

/// 把文件置为目标状态：Some = 写入内容；None = 删除（已不存在视为成功）。
fn apply_state(path: &Path, desired: Option<&[u8]>) -> Result<(), String> {
    match desired {
        Some(bytes) => rclaw_store::fs::atomic_write(path, bytes).map_err(|error| error.to_string()),
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        },
    }
}

/// 按回滚点 id 选点恢复：回滚分组 M 及其后全部 checkpointed 写入
/// （把工作区还原到分组 M 之前的状态）。
///
/// - `rewind_id` 必须精确命中某个分组（模糊的 `>=` 匹配会让错写一位的 id 回滚整个会话）；
///   回滚集按记录行序（= 时间序）选取，不依赖 id 的跨进程单调性。
/// - 安全：只校验回滚集涉及的文件——每个与它【最新】checkpoint（含 synthetic 回滚标记）
///   的 after_hash 比对，且回滚集内相邻 checkpoint 首尾相接（见 [`broken_chains`]）；
///   不符即存在未记录的改动 → 整组拒绝（不相干文件的外部改动不阻塞）。
/// - 写入阶段中途失败 → 按逆序撤回已应用的文件，保持回滚前状态。
/// - 读库分两步：元数据定位 / 校验，再按行 id 范围读回滚集的 `before` 正文。
pub(crate) fn restore_to_point(
    session: &SessionStore,
    channel: &str,
    chat_id: &str,
    rewind_id: i64,
) -> Result<String, String> {
    let all_meta = load_meta(session, channel, chat_id);
    let Some(start) = all_meta
        .iter()
        .position(|record| record.group_id == rewind_id && !record.is_rewind)
    else {
        return Err(format!(
            "回滚点 {rewind_id} 没有文件 checkpoint（请原样使用 /rewind 列表给出的回滚点 id；仅 write/edit/delete 会被记录；回滚会同时撤销该回滚点之后的所有 checkpointed 写入）"
        ));
    };
    let window = &all_meta[start..];
    let broken = broken_chains(window);
    if !broken.is_empty() {
        return Err(format!(
            "以下文件在两次 checkpoint 之间存在未记录的改动（shell / 外部编辑），还原到最早的 \
             checkpoint 会把它抹掉，拒绝盲恢复（请人工核对后处理）：{}",
            broken.join("、")
        ));
    }

    // 回滚集的正文：只读选点行起的记录（行 id >= 选点行）。
    let rollback_rows = session
        .load_file_checkpoints_from(channel, chat_id, window[0].id)
        .map_err(|error| format!("读取 rewind checkpoint 正文失败: {error}"))?;
    let rollback: Vec<&FileCheckpointRecord> = rollback_rows
        .iter()
        .filter(|record| !record.is_rewind)
        .collect();
    // 每个文件的目标状态 = 回滚集中它【最早】一次 checkpoint 的 before（行序 = 时间序）。
    let mut targets: Vec<(String, Option<String>)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for record in &rollback {
        if seen.insert(path_key(&record.path)) {
            targets.push((record.path.clone(), record.before.clone()));
        }
    }

    // 安全校验（只看回滚集内的文件）+ 为补偿撤回留存当前内容。
    let mut unsafe_files: Vec<String> = Vec::new();
    let mut snapshots: Vec<Option<Vec<u8>>> = Vec::with_capacity(targets.len());
    for (path, _) in &targets {
        let key = path_key(path);
        let latest_after_hash = all_meta
            .iter()
            .rev()
            .find(|record| path_key(&record.path) == key)
            .and_then(|record| record.after_hash.clone());
        match read_file_bytes(Path::new(path)) {
            Ok(current) => {
                if current.as_deref().map(hash_bytes) != latest_after_hash {
                    unsafe_files.push(path.clone());
                }
                snapshots.push(current);
            }
            Err(error) => unsafe_files.push(format!("{path}（无法读取：{error}）")),
        }
    }
    if !unsafe_files.is_empty() {
        return Err(format!(
            "以下文件在 checkpoint 之后被外部改动，拒绝盲恢复（请人工核对后处理）：{}",
            unsafe_files.join("、")
        ));
    }

    // 应用；中途失败按逆序撤回已应用的文件（尽量保持 all-or-nothing）。
    let mut applied: Vec<(&str, Option<Vec<u8>>)> = Vec::new();
    for ((path, desired), snapshot) in targets.iter().zip(snapshots) {
        if let Err(error) = apply_state(Path::new(path), desired.as_deref().map(str::as_bytes)) {
            let mut undo_failures: Vec<String> = Vec::new();
            for (done_path, previous) in applied.iter().rev() {
                if apply_state(Path::new(done_path), previous.as_deref()).is_err() {
                    undo_failures.push((*done_path).to_string());
                }
            }
            return Err(if undo_failures.is_empty() {
                format!("恢复 {path} 失败: {error}；已应用的文件均已撤回，工作区保持回滚前状态")
            } else {
                format!(
                    "恢复 {path} 失败: {error}；且以下文件撤回失败，请人工核对：{}",
                    undo_failures.join("、")
                )
            });
        }
        applied.push((path.as_str(), snapshot));
    }

    // 回滚完成后落 synthetic checkpoint：重锚每个被回滚文件的校验链，
    // 使后续更早选点的回滚不会被本次回滚误判为外部改动（删除的文件以 NULL 重锚）。
    let markers: Vec<FileCheckpointRecord> = targets
        .iter()
        .map(|(path, desired)| FileCheckpointRecord {
            id: 0,
            group_id: rewind_id,
            path: path.clone(),
            before: None,
            before_hash: None,
            after_hash: desired.as_deref().map(|text| hash_bytes(text.as_bytes())),
            is_rewind: true,
        })
        .collect();
    if let Err(error) = session.append_file_checkpoints(channel, chat_id, rewind_id, &markers) {
        tracing::warn!("rewind 重锚标记写入失败（后续更早选点的回滚可能被判为外部改动）: {error}");
    }

    let restored: Vec<&String> = targets
        .iter()
        .filter(|(_, desired)| desired.is_some())
        .map(|(path, _)| path)
        .collect();
    let deleted: Vec<&String> = targets
        .iter()
        .filter(|(_, desired)| desired.is_none())
        .map(|(path, _)| path)
        .collect();
    let mut report = format!(
        "✅ 已回滚回滚点 {rewind_id} 及其后的全部 checkpointed 写入（恢复到回滚点 {rewind_id} 之前的文件状态）：{} 个文件恢复原内容，{} 个新建文件已删除。",
        restored.len(),
        deleted.len()
    );
    for path in restored {
        report.push_str(&format!("\n- {path}"));
    }
    for path in deleted {
        report.push_str(&format!("\n- {path}（已删除）"));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rclaw_store::fs::Layout;

    fn test_store(dir: &Path) -> SessionStore {
        SessionStore::new(Layout::new(dir.join("agent")))
    }

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// 模拟一次工具写入：capture → 实际写 / 删 → finish。
    fn checkpointed_write(base: &Path, rel: &str, content: Option<&str>) -> FileCheckpoint {
        let tool = if content.is_some() { "write" } else { "delete" };
        let mut checkpoint = capture_before(tool, &json!({"path": rel}), base).expect("应捕获");
        match content {
            Some(text) => write_file(&base.join(rel), text),
            None => std::fs::remove_file(base.join(rel)).unwrap(),
        }
        assert!(finish_capture(&mut checkpoint), "内容有变化必须入账");
        checkpoint
    }

    #[test]
    fn capture_records_relative_path_against_agent_base() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("agent");
        write_file(&base.join("notes/a.md"), "v1");
        let input = json!({"path": "notes/a.md", "content": "v2"});
        let checkpoint = capture_before("write", &input, &base).expect("应捕获");
        assert_eq!(
            checkpoint.path,
            display_path(&base.join("notes").join("a.md")),
            "相对路径必须基于 agent 根解析"
        );
        assert_eq!(checkpoint.before.as_deref(), Some("v1"));
        assert_eq!(
            checkpoint.before_hash.as_deref(),
            Some(hash_bytes(b"v1").as_str())
        );
        // 非 write/edit/delete 不捕获。
        assert!(capture_before("shell", &json!({"command": "ls"}), &base).is_none());
        // `..` 按词法折叠（Unrestricted 权限下工具会放行这类路径）。
        let dotted = capture_before("write", &json!({"path": "notes/../notes/a.md"}), &base)
            .expect("应捕获");
        assert_eq!(dotted.path, checkpoint.path);
    }

    #[test]
    fn capture_skips_non_utf8_and_directories_instead_of_fabricating_absence() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("agent");
        std::fs::create_dir_all(base.join("bin")).unwrap();
        std::fs::write(base.join("bin/blob"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        // 非 UTF-8 文件：不能当成「不存在」记录，否则回滚会删掉用户原文件。
        assert!(capture_before("write", &json!({"path": "bin/blob"}), &base).is_none());
        // 目录：不在覆盖范围。
        assert!(capture_before("delete", &json!({"path": "bin"}), &base).is_none());
        // 真正不存在的文件：before=None（回滚即删除）。
        let fresh = capture_before("write", &json!({"path": "bin/new.txt"}), &base).unwrap();
        assert!(fresh.before.is_none() && fresh.before_hash.is_none());
    }

    #[test]
    fn capture_never_reads_unapproved_outside_paths_or_special_files() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("agent");
        std::fs::create_dir_all(&base).unwrap();
        // 测试线程没有 turn 权限上下文 → Sandboxed：工作区外的文件工具会拒绝，回滚点捕获跑在
        // 工具之前，也不能先把原文抄进 state.sqlite。
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        assert!(
            capture_before("write", &json!({"path": outside.to_string_lossy()}), &base).is_none(),
            "沙箱下未放行的工作区外路径不读"
        );
        // Unrestricted 下同一路径照常捕获。
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let captured = runtime.block_on(rclaw_tools::access::scope_access(
            rclaw_core::ToolAccess::Unrestricted,
            async { capture_before("write", &json!({"path": outside.to_string_lossy()}), &base) },
        ));
        assert_eq!(captured.and_then(|c| c.before).as_deref(), Some("secret"));
        // 超过上限的文件：metadata 就拦下，不读。
        let big = base.join("big.txt");
        std::fs::write(&big, "x".repeat(MAX_CHECKPOINT_BYTES as usize + 1)).unwrap();
        assert!(capture_before("write", &json!({"path": "big.txt"}), &base).is_none());
        #[cfg(unix)]
        {
            // FIFO：`open` 会一直等到有写端出现；只认普通文件，根本不打开（否则这里就挂死）。
            let fifo = base.join("pipe");
            if std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .is_ok_and(|status| status.success())
            {
                assert!(capture_before("delete", &json!({"path": "pipe"}), &base).is_none());
            }
            // 字符设备（Unrestricted 下工具会放行 `/dev/zero`）：读不到头，不是普通文件就不读。
            let device = runtime.block_on(rclaw_tools::access::scope_access(
                rclaw_core::ToolAccess::Unrestricted,
                async { capture_before("write", &json!({"path": "/dev/zero"}), &base) },
            ));
            assert!(device.is_none());
        }
    }

    /// 符号链接条目不做 checkpoint：`delete` 只摘条目、`write` 把条目换成普通文件，目标都没
    /// 变；跟随链接记下目标内容再回滚，会把条目「还原」成装着目标内容的普通文件。恢复侧同一
    /// 口径：checkpoint 过的普通文件被换成链接条目算外部改动，整组拒绝、不跟随链接比对目标。
    #[cfg(unix)]
    #[test]
    fn capture_skips_symlink_entries_and_restore_refuses_them() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_store(dir.path());
        let base = session.layout().base.clone();
        std::fs::create_dir_all(&base).unwrap();
        let target = dir.path().join("outside/target.txt");
        write_file(&target, "secret");
        let link = base.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // Unrestricted：沙箱门不会先拦下指向工作区外的链接，走到条目判断这一步。
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let captured = runtime.block_on(rclaw_tools::access::scope_access(
            rclaw_core::ToolAccess::Unrestricted,
            async {
                (
                    capture_before("delete", &json!({"path": "link"}), &base),
                    capture_before("write", &json!({"path": "link"}), &base),
                )
            },
        ));
        assert!(captured.0.is_none() && captured.1.is_none(), "{captured:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "secret");
        // 普通文件照常 checkpoint；之后被换成链接条目 → 恢复按「无法读取」拒绝，链接与目标原样。
        write_file(&base.join("a.txt"), "v1");
        let c1 = checkpointed_write(&base, "a.txt", Some("v2"));
        flush(&session, "cli", "console", 100, vec![c1]);
        std::fs::remove_file(base.join("a.txt")).unwrap();
        std::os::unix::fs::symlink(&target, base.join("a.txt")).unwrap();
        let error = current_file_hash(&base.join("a.txt")).unwrap_err();
        assert!(error.to_string().contains("符号链接"), "{error}");
        let error = restore_to_point(&session, "cli", "console", 100).unwrap_err();
        assert!(error.contains("无法读取") && error.contains("符号链接"), "{error}");
        assert!(std::fs::symlink_metadata(base.join("a.txt")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "secret");
    }

    #[test]
    fn restore_round_trip_and_delete_recreation() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_store(dir.path());
        let base = session.layout().base.clone();
        let file_a = base.join("a.txt");
        let file_b = base.join("b.txt");
        write_file(&file_a, "v1");
        // 消息 100：改 a + 新建 b。
        let c1 = checkpointed_write(&base, "a.txt", Some("v2"));
        let c2 = checkpointed_write(&base, "b.txt", Some("new file"));
        flush(&session, "cli", "console", 100, vec![c1, c2]);

        // 消息 200：又改 a（模拟后续工作）。
        let c3 = checkpointed_write(&base, "a.txt", Some("v3"));
        flush(&session, "cli", "console", 200, vec![c3]);

        let groups = list_groups(&session, "cli", "console");
        assert_eq!(groups.len(), 2, "两个消息分组");
        assert_eq!(groups[0].0, 200, "新在前");

        // 选点回滚到 200：撤销 200 的写入 → a 回到 v2，b 不受影响。
        let report = restore_to_point(&session, "cli", "console", 200).unwrap();
        assert!(report.contains("回滚回滚点 200"), "{report}");
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "v2");
        assert!(file_b.exists(), "200 未触碰的文件保持不变");

        // 选点回滚到 100：撤销 100 与 200 → a 回到 v1，b（100 新建）被删除。
        restore_to_point(&session, "cli", "console", 100).unwrap();
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "v1");
        assert!(!file_b.exists(), "100 新建的文件应随回滚删除");

        // 连环回滚第三次：b 已被回滚删除并以 NULL 重锚，再次回滚到 100 不得误判外部改动。
        write_file(&file_a, "v1");
        let report = restore_to_point(&session, "cli", "console", 100).unwrap();
        assert!(report.contains("回滚回滚点 100"), "{report}");
        assert!(!file_b.exists());
        // 重锚标记不出现在列表里。
        assert_eq!(list_groups(&session, "cli", "console").len(), 2);
    }

    #[test]
    fn delete_tool_checkpoints_restore_and_do_not_poison_later_rewinds() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_store(dir.path());
        let base = session.layout().base.clone();
        let file_a = base.join("a.txt");
        let file_b = base.join("b.txt");
        write_file(&file_a, "keep me");
        // 消息 100：删除 a。
        let c1 = checkpointed_write(&base, "a.txt", None);
        assert!(c1.after_hash.is_none(), "删除后 after_hash 必须是 None 而非 0");
        flush(&session, "cli", "console", 100, vec![c1]);
        // 消息 200：新建 b。
        let c2 = checkpointed_write(&base, "b.txt", Some("later"));
        flush(&session, "cli", "console", 200, vec![c2]);

        // 回滚到 200：a 的删除记录不在回滚集，也不得阻塞。
        restore_to_point(&session, "cli", "console", 200).unwrap();
        assert!(!file_b.exists());
        assert!(!file_a.exists(), "200 的回滚不触碰 a");

        // 回滚到 100：恢复被删除的 a。
        restore_to_point(&session, "cli", "console", 100).unwrap();
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "keep me");
    }

    #[test]
    fn restore_requires_exact_rewind_id() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_store(dir.path());
        let base = session.layout().base.clone();
        write_file(&base.join("a.txt"), "v1");
        let c1 = checkpointed_write(&base, "a.txt", Some("v2"));
        flush(&session, "cli", "console", 100, vec![c1]);
        // 比所有分组都小的 id（如手误）不得回滚整个会话。
        let error = restore_to_point(&session, "cli", "console", 1).unwrap_err();
        assert!(error.contains("没有文件 checkpoint"), "{error}");
        assert_eq!(std::fs::read_to_string(base.join("a.txt")).unwrap(), "v2");
    }

    #[test]
    fn restore_refuses_externally_modified_files_without_touching_anything() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_store(dir.path());
        let base = session.layout().base.clone();
        let file_a = base.join("a.txt");
        let file_b = base.join("b.txt");
        write_file(&file_a, "v1");
        write_file(&file_b, "keep");
        let c1 = checkpointed_write(&base, "a.txt", Some("v2"));
        let c2 = checkpointed_write(&base, "b.txt", Some("v2b"));
        flush(&session, "cli", "console", 300, vec![c1, c2]);

        // 外部改动 a → 整组拒绝，b 保持原样。
        write_file(&file_a, "externally edited");
        let error = restore_to_point(&session, "cli", "console", 300).unwrap_err();
        assert!(error.contains("外部改动"), "{error}");
        assert_eq!(std::fs::read_to_string(&file_b).unwrap(), "v2b", "all-or-nothing");
    }

    #[test]
    fn unrecorded_edit_between_checkpoints_breaks_the_chain_and_refuses_restore() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_store(dir.path());
        let base = session.layout().base.clone();
        let file_a = base.join("a.txt");
        write_file(&file_a, "v1");
        let c1 = checkpointed_write(&base, "a.txt", Some("v2"));
        flush(&session, "cli", "console", 100, vec![c1]);
        // 两次 checkpoint 之间的未记录改动（shell / 外部编辑）：后一次的 before 接不上前一次的 after。
        write_file(&file_a, "shell edit");
        let c2 = checkpointed_write(&base, "a.txt", Some("v3"));
        flush(&session, "cli", "console", 200, vec![c2]);

        // 回滚到 200 只涉及 c2 一条，链条完整：成功，a 回到 shell edit。
        restore_to_point(&session, "cli", "console", 200).unwrap();
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "shell edit");
        // 回滚到 100：回滚集 = c1, c2（+ 刚才的回滚标记），c2.before != c1.after → 拒绝。
        let error = restore_to_point(&session, "cli", "console", 100).unwrap_err();
        assert!(error.contains("未记录的改动"), "{error}");
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "shell edit", "拒绝时不动文件");

        // 回滚标记重锚：标记之后的新写入以标记的 after_hash 为起点，不算断链——
        // 再次回滚到 200 的窗口 = [c2, 标记, c3, 标记]，c3.before 接上标记的 after。
        let c3 = checkpointed_write(&base, "a.txt", Some("v4"));
        flush(&session, "cli", "console", 300, vec![c3]);
        restore_to_point(&session, "cli", "console", 300).unwrap();
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "shell edit");
        restore_to_point(&session, "cli", "console", 200).unwrap();
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "shell edit");
    }

    #[test]
    fn tool_accepts_legacy_message_id_argument_name() {
        assert_eq!(
            rewind_id_arg(&json!({"rewind_id": 7})).and_then(Value::as_i64),
            Some(7)
        );
        assert_eq!(
            rewind_id_arg(&json!({"message_id": 9})).and_then(Value::as_i64),
            Some(9)
        );
        assert!(rewind_id_arg(&json!({"rewind_id": null})).is_none());
        assert!(rewind_id_arg(&json!({})).is_none());
    }

    #[test]
    fn external_edits_outside_rollback_set_do_not_block_restore() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_store(dir.path());
        let base = session.layout().base.clone();
        let file_old = base.join("old.txt");
        let file_new = base.join("new.txt");
        write_file(&file_old, "v1");
        let c1 = checkpointed_write(&base, "old.txt", Some("v2"));
        flush(&session, "cli", "console", 100, vec![c1]);
        let c2 = checkpointed_write(&base, "new.txt", Some("fresh"));
        flush(&session, "cli", "console", 200, vec![c2]);
        // 用户随后手工改了 old.txt——与回滚 200 无关，不得阻塞。
        write_file(&file_old, "user edit");
        restore_to_point(&session, "cli", "console", 200).unwrap();
        assert!(!file_new.exists());
        assert_eq!(std::fs::read_to_string(&file_old).unwrap(), "user edit");
    }

    #[test]
    fn restore_errors_when_no_checkpoints_for_message() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_store(dir.path());
        let error = restore_to_point(&session, "cli", "console", 42).unwrap_err();
        assert!(error.contains("没有文件 checkpoint"), "{error}");
    }

    #[test]
    fn checkpoints_are_scoped_per_session() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_store(dir.path());
        let base = session.layout().base.clone();
        let c1 = checkpointed_write(&base, "a.txt", Some("v1"));
        flush(&session, "cli", "console", 100, vec![c1]);
        assert!(list_groups(&session, "desktop", "other").is_empty());
        assert_eq!(list_groups(&session, "cli", "console").len(), 1);
    }

    #[test]
    fn rewind_ids_are_unique_and_monotonic() {
        let first = next_rewind_id();
        let second = next_rewind_id();
        assert!(second > first, "回滚点 id 必须严格递增: {first} vs {second}");
        let ids: Vec<i64> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| (0..64).map(|_| next_rewind_id()).collect::<Vec<_>>()))
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| handle.join().unwrap())
                .collect()
        });
        let unique: HashSet<i64> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "并发分配不得重复");
    }
}
