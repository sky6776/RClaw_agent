//! 长期记忆与 daily notes。
//!
//! 长期记忆存储：
//! - `MEMORY.md`：长期记忆（整体读写）。legacy / 高层工作方式改进可用 `[自进化]` 前缀标记；
//!   具体任务流程改进优先通过 skill_manage 落地到技能文件。
//! - `memory/daily/<YYYY-MM-DD>.md`：每日笔记（追加）。
//! - `memory/session_summaries/<channel>_<chat_id>.md`：会话压缩摘要（整体写）。
//! - `memory_read_recent`：读最近 N 天 daily，拼成一段。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Local;

use crate::fs::Layout;
use crate::{ensure_parent_dir, Result, StoreError};

const AUTO_MEMORY_CHAR_LIMIT: usize = 2_200;
const AUTO_USER_CHAR_LIMIT: usize = 1_375;
const AUTO_ENTRY_CHAR_LIMIT: usize = 1_200;
const AUTO_DAILY_CHAR_LIMIT: usize = 12_000;

/// 记忆写入目标。
///
/// - [`MemoryTarget::Memory`]：MEMORY.md，agent 自身的长期笔记（约定、工具特性、环境事实）。
/// - [`MemoryTarget::User`]：USER.md，用户档案（偏好、风格、工作流）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTarget {
    Memory,
    User,
}

impl MemoryTarget {
    /// 该目标文件重建时使用的标题行（文件可能为空/被外部改写，重建需稳定头部）。
    fn header(self) -> &'static str {
        match self {
            MemoryTarget::Memory => "# Memory\n",
            MemoryTarget::User => "# User\n",
        }
    }
}

/// 记忆 add/replace/remove 操作的结果（供工具层回显给模型）。
#[derive(Debug, Clone)]
pub struct MemOpResult {
    /// 本次实际改动条数（add=1 / replace=1 / remove=1；未命中=0）。
    pub changed: usize,
    /// 操作后该目标文件的条目总数。
    pub entry_count: usize,
    /// 操作后该目标文件占用 / 上限（便于模型自纠超额）。
    pub usage: String,
    /// 人类可读的一句话说明。
    pub message: String,
}

/// 一条 batch 操作：`{action, content?, old_text?}`。
///
/// 由 [`MemoryStore::apply_batch`] 在单次调用里原子地应用到同一 target，便于模型在
/// 记忆接近上限时「删旧 + 改写 + 新增」一次性整理，而非多轮 consolidate-then-retry。
#[derive(Debug, Clone)]
pub struct MemoryBatchOp {
    /// `"add"` | `"replace"` | `"remove"`。
    pub action: String,
    /// add/replace 的新内容（写入前过威胁扫描 + 脱敏）。
    pub content: Option<String>,
    /// replace/remove 的定位子串。
    pub old_text: Option<String>,
}

/// 记忆存储。
///
/// 除原有的 append-only 自动写入外，新增结构化 add/replace/remove：以 bullet（`- `）
/// 为条目粒度，replace/remove 用子串定位，
/// 写前做漂移检测、威胁扫描、脱敏与总字符上限校验。`lock` 保证前台 `memory`
/// 工具与后台 review fork 并发写时的读-改-写原子性（RClaw 单进程，进程内互斥足矣；
/// 跨进程仍由 [`crate::fs::atomic_write`] 的 tmp+rename 兜底）。
#[derive(Debug, Clone)]
pub struct MemoryStore {
    layout: Layout,
    memory_char_limit: usize,
    user_char_limit: usize,
    lock: Arc<Mutex<()>>,
}

impl MemoryStore {
    pub fn new(layout: Layout) -> Self {
        MemoryStore {
            layout,
            memory_char_limit: AUTO_MEMORY_CHAR_LIMIT,
            user_char_limit: AUTO_USER_CHAR_LIMIT,
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// 自定义 MEMORY.md / USER.md 的总字符上限。
    pub fn with_char_limits(mut self, memory: usize, user: usize) -> Self {
        self.memory_char_limit = memory.max(256);
        self.user_char_limit = user.max(256);
        self
    }

    fn limit_for(&self, target: MemoryTarget) -> usize {
        match target {
            MemoryTarget::Memory => self.memory_char_limit,
            MemoryTarget::User => self.user_char_limit,
        }
    }

    fn file_for(&self, target: MemoryTarget) -> &Path {
        match target {
            MemoryTarget::Memory => &self.layout.memory_file,
            MemoryTarget::User => &self.layout.user_file,
        }
    }

    /// 读取长期记忆全文。
    ///
    /// - 文件不存在 → 返回 `Ok("")`（正常情况，尚无长期记忆）。
    /// - IO 错误（权限、磁盘故障等）或 UTF-8 非法 → 返回 `Err`，
    ///   由上层（`build_system_prompt`）记录 warning，避免静默丢失用户记忆。
    pub fn read_long_term(&self) -> Result<String> {
        match fs::read_to_string(&self.layout.memory_file) {
            Ok(content) => Ok(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    /// 读取某目标（MEMORY.md / USER.md）的「注入安全」视图：逐条复扫威胁模式，
    /// 命中者以 `[BLOCKED]` 占位符替换（磁盘原文不动，用户可手工修复）。
    ///
    /// 写入路径已有扫描，但文件是用户可手工编辑的纯文本——外部改入的内容不经
    /// 写入扫描直接进系统提示。加载期复扫是最后一条防线（Hermes memory_tool
    /// load_from_disk 同款设计）。NotFound 返回空串，与 [`Self::read_long_term`] 一致。
    pub fn read_target_sanitized(&self, target: MemoryTarget) -> Result<String> {
        let raw = read_memory_raw(self.file_for(target))?;
        if raw.is_empty() {
            return Ok(raw);
        }
        let (header, entries) = parse_entries(&raw);
        let entries = entries
            .into_iter()
            .map(|e| match first_memory_threat(&e) {
                Some(reason) => format!("[BLOCKED: threat scanner: {reason}]"),
                None => e,
            })
            .collect::<Vec<_>>();
        Ok(serialize_entries(&header, &entries))
    }

    /// 某目标的当前占用 / 上限（chars），供系统提示渲染用量指示。
    pub fn usage_report(&self, target: MemoryTarget) -> (usize, usize) {
        let raw = read_memory_raw(self.file_for(target)).unwrap_or_default();
        let (header, entries) = parse_entries(&raw);
        // 与写入路径一致：以序列化后的全文（含 header）为占用口径。
        let header = normalize_header(header, target);
        let total = serialize_entries(&header, &entries).chars().count();
        (total, self.limit_for(target))
    }

    /// 整体覆盖写长期记忆（工具 edit 写回后调用）。使用原子写。
    pub fn write_long_term(&self, content: &str) -> Result<()> {
        crate::fs::atomic_write(&self.layout.memory_file, content.as_bytes())
    }

    /// 向长期记忆追加一条去重 bullet。
    ///
    /// 走配置后的 `self.memory_char_limit`（而非常量 `AUTO_MEMORY_CHAR_LIMIT`）：旧 post-turn
    /// review 路径（显式 `auto_review_tooled=false` 的 legacy JSON 模式）由此和带工具 review 的
    /// `add_entry`/`apply_batch`（已用 `limit_for(Memory)`）保持同一字符预算——
    /// 否则用户配置的 `memory_char_limit` 在该 review 路径下静默失效。
    pub fn append_long_term_entry(&self, note: &str) -> Result<()> {
        append_unique_markdown_bullet(
            &self.layout.memory_file,
            "# Memory\n",
            note,
            self.memory_char_limit,
        )
    }

    // ---- 结构化 add / replace / remove / list ----
    //
    // 以 bullet（`- `）为条目粒度，replace/remove 用子串定位单条命中；多义/未命中/漂移
    // 均拒绝并把当前条目回显给模型自纠。所有写入都过威胁扫描 + 脱敏 + 总字符上限。

    /// 列出某目标的全部条目（供模型 read-before-write，避免盲改）。
    pub fn list_entries(&self, target: MemoryTarget) -> Result<Vec<String>> {
        let raw = read_memory_raw(self.file_for(target))?;
        Ok(parse_entries(&raw).1)
    }

    /// 追加一条 bullet（add）。命中等价条目则幂等跳过。
    pub fn add_entry(&self, target: MemoryTarget, content: &str) -> Result<MemOpResult> {
        let _g = self.lock.lock().expect("memory lock not poisoned");
        let path = self.file_for(target);
        let limit = self.limit_for(target);
        let raw = read_memory_raw(path)?;
        if detect_drift(&raw) {
            snapshot_drift_backup(path, &raw)?;
            return Err(StoreError::Other(format!(
                "drift detected: {} was modified outside the memory tool (snapshot saved); \
                 refusing to overwrite. Re-read with list and retry.",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("(file)")
            )));
        }
        let (header, mut entries) = parse_entries(&raw);
        let header = normalize_header(header, target);
        let content = validate_auto_note(content)?;
        if entries.iter().any(|e| e == &content) {
            return Ok(MemOpResult {
                changed: 0,
                entry_count: entries.len(),
                usage: usage_str(entries.join("\n").chars().count(), limit),
                message: "entry already exists; skipped".into(),
            });
        }
        entries.push(content);
        let serialized = serialize_entries(&header, &entries);
        check_size(&serialized, limit, &entries)?;
        crate::fs::atomic_write(path, serialized.as_bytes())?;
        Ok(MemOpResult {
            changed: 1,
            entry_count: entries.len(),
            usage: usage_str(serialized.chars().count(), limit),
            message: "entry added".into(),
        })
    }

    /// 替换首个含 `old_substr` 的条目为 `new_content`。
    pub fn replace_entry(
        &self,
        target: MemoryTarget,
        old_substr: &str,
        new_content: &str,
    ) -> Result<MemOpResult> {
        let new_content = validate_auto_note(new_content)?; // 提前校验，闭包内只赋值
        self.mutate_entry(target, old_substr, |entries, idx| {
            entries[idx] = new_content.clone();
            Ok(())
        })
    }

    /// 移除首个含 `old_substr` 的条目。
    pub fn remove_entry(&self, target: MemoryTarget, old_substr: &str) -> Result<MemOpResult> {
        self.mutate_entry(target, old_substr, |entries, idx| {
            entries.remove(idx);
            Ok(())
        })
    }

    /// 原子批量应用 add/replace/remove。
    ///
    /// all-or-nothing：先把每个 add/replace 的 content 过威胁扫描 + 脱敏 + 非空校验
    /// （任一中毒/非法即整体拒绝，不碰盘）；再在一份 working copy 上顺序应用所有 op
    /// （add 幂等跳过重复，replace/remove 用 `old_text` 子串在 working copy 上唯一定位）；
    /// 最后只对【最终状态】做一次 char 上限校验。任一 op 未命中/多义，或最终超限，
    /// 【什么都不写】并把当前 live entries 回显给模型自纠。
    ///
    /// 这让模型能在记忆接近上限时「删旧 + 改写 + 新增」一次性完成，而非多轮
    /// consolidate-then-retry（每轮重发整个上下文，昂贵且易把用户回复挤掉）。
    pub fn apply_batch(&self, target: MemoryTarget, ops: &[MemoryBatchOp]) -> Result<MemOpResult> {
        if ops.is_empty() {
            return Err(StoreError::Other("operations list is empty".into()));
        }
        // Pass 1：对所有 add/replace 的 content 做威胁扫描 + 脱敏 + 非空校验。任一非法即整体拒绝。
        let mut plan: Vec<MemoryBatchOp> = Vec::with_capacity(ops.len());
        for (i, op) in ops.iter().enumerate() {
            let act = op.action.trim().to_string();
            let content = match act.as_str() {
                "add" | "replace" => Some(
                    validate_auto_note(op.content.as_deref().unwrap_or(""))
                        .map_err(|e| StoreError::Other(format!("Operation {}: {e}", i + 1)))?,
                ),
                _ => None,
            };
            plan.push(MemoryBatchOp {
                action: act,
                content,
                old_text: op.old_text.as_deref().map(|s| s.trim().to_string()),
            });
        }

        // Pass 2：持锁读-改-写。漂移检测 → 顺序应用 → 最终预算校验 → 原子落盘。
        let _g = self.lock.lock().expect("memory lock not poisoned");
        let path = self.file_for(target);
        let limit = self.limit_for(target);
        let raw = read_memory_raw(path)?;
        if detect_drift(&raw) {
            snapshot_drift_backup(path, &raw)?;
            return Err(StoreError::Other(format!(
                "drift detected: {} was modified outside the memory tool (snapshot saved); \
                 refusing to overwrite. Re-read with list and retry.",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("(file)")
            )));
        }
        let (header, mut working) = parse_entries(&raw);
        let header = normalize_header(header, target);

        for (i, op) in plan.iter().enumerate() {
            let pos = format!("Operation {} ({})", i + 1, op.action);
            match op.action.as_str() {
                "add" => {
                    let content = op.content.clone().unwrap_or_default();
                    if working.iter().any(|e| e == &content) {
                        continue; // 幂等：跳过已存在的等价条目，不让重复条目弄垮整批。
                    }
                    working.push(content);
                }
                "replace" => {
                    let old_text = op.old_text.as_deref().unwrap_or("");
                    let content = op.content.clone().unwrap_or_default();
                    if old_text.is_empty() {
                        return Err(batch_abort(
                            &working,
                            &format!("{pos}: old_text is required"),
                        ));
                    }
                    if content.is_empty() {
                        return Err(batch_abort(
                            &working,
                            &format!("{pos}: content is required (use action=remove to delete)"),
                        ));
                    }
                    let hits = match_indices(&working, old_text);
                    if hits.is_empty() {
                        return Err(batch_abort(
                            &working,
                            &format!("{pos}: no entry matched {old_text:?}"),
                        ));
                    }
                    if distinct_count(&working, &hits) > 1 {
                        return Err(batch_abort(
                            &working,
                            &format!("{pos}: {old_text:?} matched multiple distinct entries; be more specific"),
                        ));
                    }
                    working[hits[0]] = content;
                }
                "remove" => {
                    let old_text = op.old_text.as_deref().unwrap_or("");
                    if old_text.is_empty() {
                        return Err(batch_abort(
                            &working,
                            &format!("{pos}: old_text is required"),
                        ));
                    }
                    let hits = match_indices(&working, old_text);
                    if hits.is_empty() {
                        return Err(batch_abort(
                            &working,
                            &format!("{pos}: no entry matched {old_text:?}"),
                        ));
                    }
                    if distinct_count(&working, &hits) > 1 {
                        return Err(batch_abort(
                            &working,
                            &format!("{pos}: {old_text:?} matched multiple distinct entries; be more specific"),
                        ));
                    }
                    working.remove(hits[0]);
                }
                other => {
                    return Err(batch_abort(
                        &working,
                        &format!("{pos}: unknown action {other:?}; use add/replace/remove"),
                    ));
                }
            }
        }

        // 只对最终状态做一次预算校验（中间过程的临时膨胀无关）。
        let serialized = serialize_entries(&header, &working);
        let total = serialized.chars().count();
        if total > limit {
            return Err(batch_abort_over_budget(&working, ops.len(), total, limit));
        }
        crate::fs::atomic_write(path, serialized.as_bytes())?;
        Ok(MemOpResult {
            changed: plan.len(),
            entry_count: working.len(),
            usage: usage_str(total, limit),
            message: format!("applied {} operation(s)", plan.len()),
        })
    }

    /// replace/remove 共享的读-改-写骨架：漂移检测 → 子串定位 → 单命中 → 闭包改动 → 落盘。
    fn mutate_entry<F>(&self, target: MemoryTarget, old_substr: &str, f: F) -> Result<MemOpResult>
    where
        F: FnOnce(&mut Vec<String>, usize) -> Result<()>,
    {
        let _g = self.lock.lock().expect("memory lock not poisoned");
        let path = self.file_for(target);
        let limit = self.limit_for(target);
        let raw = read_memory_raw(path)?;
        if detect_drift(&raw) {
            snapshot_drift_backup(path, &raw)?;
            return Err(StoreError::Other(format!(
                "drift detected: {} was modified outside the memory tool (snapshot saved); \
                 refusing to overwrite. Re-read with list and retry.",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("(file)")
            )));
        }
        let (header, mut entries) = parse_entries(&raw);
        let header = normalize_header(header, target);
        let hits: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.contains(old_substr))
            .map(|(i, _)| i)
            .collect();
        let distinct_count = {
            let mut s: Vec<String> = hits.iter().map(|&i| entries[i].clone()).collect();
            s.sort();
            s.dedup();
            s.len()
        };
        if hits.is_empty() {
            return Err(StoreError::Other(format!(
                "no entry matched {:?}. current entries:\n{}",
                old_substr,
                entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| format!("{}. {e}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n")
            )));
        }
        if distinct_count > 1 {
            let previews: Vec<String> = hits
                .iter()
                .map(|&i| format!("- {}", preview(&entries[i], 80)))
                .collect();
            return Err(StoreError::Other(format!(
                "multiple distinct entries matched {:?}; provide a longer unique substring:\n{}",
                old_substr,
                previews.join("\n")
            )));
        }
        let idx = hits[0];
        f(&mut entries, idx)?;
        let serialized = serialize_entries(&header, &entries);
        check_size(&serialized, limit, &entries)?;
        crate::fs::atomic_write(path, serialized.as_bytes())?;
        Ok(MemOpResult {
            changed: 1,
            entry_count: entries.len(),
            usage: usage_str(serialized.chars().count(), limit),
            message: "entry updated".into(),
        })
    }

    /// 保存某个会话的最新压缩摘要。
    pub fn write_session_summary(&self, channel: &str, chat_id: &str, summary: &str) -> Result<()> {
        let path = self.session_summary_path(channel, chat_id);
        let content = format!(
            "# Session Summary\n\n- channel: `{}`\n- chat_id: `{}`\n- updated_at: `{}`\n\n{}\n",
            channel,
            chat_id,
            Local::now().format("%Y-%m-%d %H:%M:%S %:z"),
            summary.trim()
        );
        crate::fs::atomic_write(&path, content.as_bytes())
    }

    /// 读取某个会话的最新压缩摘要。
    pub fn read_session_summary(&self, channel: &str, chat_id: &str) -> Result<String> {
        let path = self.session_summary_path(channel, chat_id);
        match fs::read_to_string(path) {
            Ok(content) => Ok(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    /// 向今天的 daily 文件追加一段（无则创建并写日期标题）。
    pub fn append_today(&self, note: &str) -> Result<()> {
        let note = validate_auto_note(note)?;
        let today = today_str();
        let path = self.daily_path(&today);
        ensure_parent_dir(&path)?;
        ensure_file_with_room(&path, &note, AUTO_DAILY_CHAR_LIMIT, false)?;

        let need_header = !path.exists();
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        if need_header {
            writeln!(f, "# {today}\n")?;
        }
        writeln!(f, "{note}")?;
        Ok(())
    }

    /// 读取最近 `days` 天的 daily，用 `\n---\n` 分隔拼成一段。
    ///
    /// - 文件不存在（`NotFound`）→ 静默跳过（当天尚无笔记，正常情况）。
    /// - IO 错误（权限、磁盘故障等）→ 打 warn 日志后继续读后续天数，
    ///   不因一个文件损坏就丢弃所有 recent notes。
    ///   这与 `read_long_term` 不同：长期记忆是单文件，错误应传播；
    ///   recent notes 是多文件拼接，部分失败不应阻止其余内容进入上下文。
    pub fn read_recent(&self, days: u32) -> Result<String> {
        let mut out = String::new();
        // i=0 → today, i=1 → yesterday, ... (负偏移 = 回溯)
        for i in 0..days {
            let date = offset_date_str(-(i as i64));
            let path = self.daily_path(&date);
            match fs::read_to_string(&path) {
                Ok(content) => {
                    if !out.is_empty() {
                        out.push_str("\n---\n");
                    }
                    out.push_str(&content);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // 当天尚无笔记，正常跳过。
                }
                Err(e) => {
                    // 非 NotFound 的 IO 错误（权限、磁盘故障等）或非法 UTF-8：
                    // 记录 warning 便于排查，继续读取后续天数，不因单日文件异常
                    // 就丢弃整个 recent notes 上下文。
                    tracing::warn!("读取 daily note 失败，已跳过 {}: {e}", path.display());
                }
            }
        }
        Ok(out)
    }

    fn daily_path(&self, date: &str) -> PathBuf {
        self.layout.memory_daily_dir.join(format!("{date}.md"))
    }

    fn session_summary_path(&self, channel: &str, chat_id: &str) -> PathBuf {
        let ch = crate::percent_encode_key(channel);
        let id = crate::percent_encode_key(chat_id);
        self.layout
            .session_summary_dir
            .join(format!("{ch}_{id}.md"))
    }
}

fn append_markdown_bullet(path: &PathBuf, header: &str, note: &str) -> Result<()> {
    ensure_parent_dir(path)?;
    let need_header = !path.exists();
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if need_header {
        writeln!(f, "{header}")?;
    }
    writeln!(f, "- {}", note.trim())?;
    Ok(())
}

fn append_unique_markdown_bullet(
    path: &PathBuf,
    header: &str,
    note: &str,
    max_total_chars: usize,
) -> Result<()> {
    let note = validate_auto_note(note)?;
    if existing_bullet_contains(path, &note)? {
        return Ok(());
    }
    ensure_file_with_room(path, &note, max_total_chars, true)?;
    append_markdown_bullet(path, header, &note)
}

fn validate_auto_note(note: &str) -> Result<String> {
    let note = note.trim();
    if note.is_empty() {
        return Err(StoreError::Other("memory entry cannot be empty".into()));
    }
    if note.chars().count() > AUTO_ENTRY_CHAR_LIMIT {
        return Err(StoreError::Other(format!(
            "memory entry too large (>{AUTO_ENTRY_CHAR_LIMIT} chars)"
        )));
    }
    if let Some(reason) = first_memory_threat(note) {
        return Err(StoreError::Other(format!(
            "memory entry rejected by threat scanner: {reason}"
        )));
    }
    // 写入前脱敏：剥离密钥/令牌（与威胁扫描并列——威胁扫描挡注入，脱敏挡泄密）。
    Ok(crate::redact_sensitive_text(note))
}

// ---- 结构化 add/replace/remove 的纯辅助函数 ----

fn read_memory_raw(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(StoreError::Io(e)),
    }
}

/// 把记忆文件拆成 (header, entries)。
///
/// 首个 `- ` bullet 之前的行（含 `# Memory`、空行）算 header；每个 bullet 行去掉
/// `- ` 前缀（trim）即一条 entry。bullet 之后的非 bullet 行（空行等）忽略——写出时
/// 由 [`serialize_entries`] 统一重建，保证 round-trip 稳定。
fn parse_entries(raw: &str) -> (String, Vec<String>) {
    let mut header = String::new();
    let mut entries = Vec::new();
    let mut saw_bullet = false;
    for line in raw.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("- ") {
            saw_bullet = true;
            entries.push(rest.trim().to_string());
        } else if !saw_bullet {
            header.push_str(line);
            header.push('\n');
        }
    }
    (header, entries)
}

fn serialize_entries(header: &str, entries: &[String]) -> String {
    let header = header.trim_end_matches('\n');
    let mut out = String::new();
    if !header.is_empty() {
        out.push_str(header);
        out.push('\n');
    }
    for e in entries {
        out.push_str("- ");
        out.push_str(e);
        out.push('\n');
    }
    out
}

/// header 缺失（新建文件）时用目标的默认标题，保持与既有 append 写入一致的格式。
fn normalize_header(header: String, target: MemoryTarget) -> String {
    if header.trim().is_empty() {
        target.header().to_string()
    } else {
        header
    }
}

/// 漂移检测：bullet 出现之后，若存在「非空、非 bullet」的行，说明被外部编辑过，
/// 工具不应盲目覆盖外部写入的内容。
fn detect_drift(raw: &str) -> bool {
    let mut saw_bullet = false;
    for line in raw.lines() {
        let t = line.trim_start();
        if t.strip_prefix("- ").is_some() {
            saw_bullet = true;
            continue;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if saw_bullet {
            return true;
        }
    }
    false
}

/// 漂移时把当前原始内容快照到 sidecar 备份（确定性命名，保留最近一份）。
fn snapshot_drift_backup(path: &Path, raw: &str) -> Result<()> {
    let fname = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("memory");
    let bak = path.with_file_name(format!(".{fname}.drift.bak"));
    crate::fs::atomic_write(&bak, raw.as_bytes())
}

fn check_size(serialized: &str, limit: usize, entries: &[String]) -> Result<()> {
    let total = serialized.chars().count();
    if total > limit {
        // 与 batch 超预算错误同构：附当前全部条目，引导模型在同一轮用
        // replace/remove 整合后重试（而非多轮 consolidate-then-retry）。
        Err(StoreError::Other(format!(
            "memory file would exceed {limit} chars ({total}); consolidate before adding — \
             use a single `operations` batch to remove/shorten stale entries AND add together. \
             current_entries:\n{}",
            live_entries_preview(entries)
        )))
    } else {
        Ok(())
    }
}

fn usage_str(current: usize, limit: usize) -> String {
    let pct = if limit == 0 {
        100
    } else {
        ((current as f64 / limit as f64) * 100.0).min(100.0) as usize
    };
    format!("{pct}% — {current}/{limit} chars")
}

fn preview(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---- apply_batch 的纯辅助函数 ----

/// 返回所有包含 `needle` 子串的条目下标（保持顺序）。
fn match_indices(entries: &[String], needle: &str) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.contains(needle))
        .map(|(i, _)| i)
        .collect()
}

/// `hits` 命中条目里【去重后】的不同条目数：>1 表示 `needle` 多义（命中了多条不同内容）。
fn distinct_count(entries: &[String], hits: &[usize]) -> usize {
    let mut s: Vec<&String> = hits.iter().map(|&i| &entries[i]).collect();
    s.sort();
    s.dedup();
    s.len()
}

/// 构造 batch 中止错误：附上 live entries 供模型自纠，并强调 all-or-nothing（未写任何内容）。
fn batch_abort(working: &[String], msg: &str) -> StoreError {
    let live = live_entries_preview(working);
    StoreError::Other(format!(
        "{msg}. No operations were applied (batch is all-or-nothing). current_entries:\n{live}"
    ))
}

/// 构造 batch 最终超预算错误，并包含实时条目。
fn batch_abort_over_budget(
    working: &[String],
    op_count: usize,
    total: usize,
    limit: usize,
) -> StoreError {
    let live = live_entries_preview(working);
    StoreError::Other(format!(
        "after applying all {op_count} operations, memory would be {total}/{limit} chars — \
         over the limit. Remove or shorten more entries in the same batch then retry. \
         current_entries:\n{live}"
    ))
}

fn live_entries_preview(working: &[String]) -> String {
    working
        .iter()
        .enumerate()
        .map(|(i, e)| format!("{}. {e}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

fn ensure_file_with_room(
    path: &PathBuf,
    note: &str,
    max_total_chars: usize,
    bullet: bool,
) -> Result<()> {
    let existing = match fs::read_to_string(path) {
        Ok(existing) => existing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(StoreError::Io(e)),
    };
    let added = if bullet {
        format!("- {note}\n")
    } else {
        format!("{note}\n")
    };
    let next_total = existing.chars().count() + added.chars().count();
    if next_total > max_total_chars {
        return Err(StoreError::Other(format!(
            "memory file would exceed {max_total_chars} chars; consolidate before adding"
        )));
    }
    Ok(())
}

fn existing_bullet_contains(path: &PathBuf, note: &str) -> Result<bool> {
    let existing = match fs::read_to_string(path) {
        Ok(existing) => existing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(StoreError::Io(e)),
    };
    Ok(existing.lines().any(|line| {
        line.trim_start()
            .strip_prefix("- ")
            .is_some_and(|entry| entry.trim() == note)
    }))
}

fn first_memory_threat(note: &str) -> Option<&'static str> {
    if note.chars().any(is_invisible_or_bidi_control) {
        return Some("invisible unicode control character");
    }
    let lower = note.to_ascii_lowercase();
    let contains_any = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    if lower.contains("ignore") && lower.contains("instructions") {
        return Some("prompt_injection");
    }
    if lower.contains("system prompt override") {
        return Some("sys_prompt_override");
    }
    if lower.contains("disregard") && contains_any(&["instructions", "rules", "guidelines"]) {
        return Some("disregard_rules");
    }
    if lower.contains("do not tell the user") {
        return Some("deception_hide");
    }
    if (lower.contains("output") || lower.contains("print") || lower.contains("share"))
        && lower.contains("system prompt")
    {
        return Some("leak_system_prompt");
    }
    if contains_any(&["authorized_keys", "$home/.ssh", "~/.ssh"]) {
        return Some("ssh_backdoor");
    }
    if contains_any(&["curl ", "wget "])
        && contains_any(&["key", "token", "secret", "password", "credential", "api"])
    {
        return Some("exfil_command");
    }
    if lower.contains("cat ") && contains_any(&[".env", "credentials", ".netrc", ".npmrc"]) {
        return Some("read_secrets");
    }
    if contains_any(&["send ", "post ", "upload ", "transmit "])
        && contains_any(&["http://", "https://"])
    {
        return Some("send_to_url");
    }
    if contains_any(&["api_key", "api key", "token", "secret", "password"])
        && note.chars().filter(|c| c.is_ascii_alphanumeric()).count() >= 40
    {
        return Some("hardcoded_secret");
    }
    None
}

fn is_invisible_or_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{200b}'
            | '\u{200c}'
            | '\u{200d}'
            | '\u{202a}'
            | '\u{202b}'
            | '\u{202c}'
            | '\u{202d}'
            | '\u{202e}'
            | '\u{2060}'
            | '\u{2062}'
            | '\u{2063}'
            | '\u{2064}'
            | '\u{2066}'
            | '\u{2067}'
            | '\u{2068}'
            | '\u{2069}'
            | '\u{feff}'
    )
}

/// 技能/其它持久化文本写入前的最小安全闸（B2）。
///
/// 与 [`first_memory_threat`]（记忆条目全量模式）刻意不同：只拦**隐形/双向控制
/// 字符**（纯恶意，无合法场景）并交由调用方对密钥做**脱敏**；不做 prompt_injection
/// 等模式匹配——技能本就是写给模型读的指令文档，"ignore previous instructions"
/// 作为教学示例属合法内容，模式匹配会误杀。
///
/// 返回 `Some(reason)` 时调用方应拒绝写入。
pub fn scan_skill_text(text: &str) -> Option<&'static str> {
    if text.chars().any(is_invisible_or_bidi_control) {
        return Some("invisible unicode control character");
    }
    None
}

/// 当地时区今天的 `YYYY-MM-DD`。
fn today_str() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// 相对今天偏移 `days`（可负）的【当地时区】日期串。
///
/// 必须与 [`today_str`] 用【同一时区】：daily 文件名是按 `today_str`（本地日期）写的，
/// 读取时若用 UTC 去算偏移，在本地 00:00–07:59（东八区等正时区）会读不到「今天刚写」
/// 的笔记——写入是本地今天、读取的「今天」却还是 UTC 昨天。这里统一用 Local，保证
/// 「当天写入 → 当天能读到」。
fn offset_date_str(days: i64) -> String {
    let now = Local::now();
    let t = now + chrono::Duration::days(days);
    t.format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests;
