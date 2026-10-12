//! Layer 5 选点回滚（ZCode rewind/checkpoint 同源）。
//!
//! write/edit/delete 在前台路径执行时捕获文件级 checkpoint（before 内容 + 前后
//! hash，按「发起写入的 assistant 消息 id」分组落盘到 `agent/sessions/rewind/`）；
//! `rewind_files` 工具与 `/rewind` 斜杠命令按 messageId 选点恢复——恢复前逐一对比
//! 当前文件 hash 与 checkpoint 的 after_hash，任何文件被外部改动即整组拒绝
//! （all-or-nothing，绝不盲恢复）。会话历史不动，只回滚文件。
//!
//! 已知边界（v1）：只覆盖结构化文件工具（write/edit/delete）；shell/skill 脚本的
//! 副作用与子代理（Subagent 策略）写入不在 checkpoint 覆盖范围。

use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use rclaw_store::fs::Layout;
use serde_json::{json, Value};

use super::compression::content_hash;

/// 单文件 checkpoint 的内联内容上限（超过则不捕获——无法恢复的写入不做半吊子记录）。
const MAX_CHECKPOINT_BYTES: usize = 2 * 1024 * 1024;
/// 列表模式返回的最近分组数。
const LIST_LIMIT: usize = 20;

/// 一次文件写入的前后快照（尚未归属 messageId 的暂存形态）。
#[derive(Debug, Clone)]
pub(crate) struct FileCheckpoint {
    pub(crate) path: String,
    pub(crate) before: Option<String>,
    pub(crate) before_hash: Option<u64>,
    pub(crate) after_hash: u64,
    /// 回滚操作的 synthetic 标记：参与后续校验链（重锚磁盘状态），
    /// 但不进入回滚集（回滚它等于撤销回滚）。
    pub(crate) is_rewind: bool,
}

tokio::task_local! {
    /// 当前会话 (channel, chat_id)：`rewind_files` 工具定位 checkpoint 存储。
    pub(crate) static CURRENT_CHAT: Option<(String, String)>;
}

/// 当前会话键；作用域外（嵌入方直调工具）返回 None。
pub(crate) fn current_chat() -> Option<(String, String)> {
    CURRENT_CHAT.try_with(std::clone::Clone::clone).ok().flatten()
}

/// 生成持久 messageId：纳秒时间戳 + 原子单调保障（同一进程内严格递增）。
///
/// `session.append` 只在 ts==0 时盖秒级时间戳，因此 turn 循环预分配的该 id 会原样
/// 落进会话 JSONL——它就是可被 `/rewind` 与 `rewind_files` 引用的「messageId」。
pub(crate) fn next_message_id() -> i64 {
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

fn file_hash(content: Option<&str>) -> Option<u64> {
    content.map(|text| content_hash(text))
}

/// write/edit/delete → 解析目标路径并捕获 before 内容。
///
/// 路径语义与文件工具一致（`normalize_agent_tool_path`：相对路径基于 agent 根目录），
/// 保证捕获与工具实际写入指向同一文件。非文件写入工具 / 内容超限 → None。
pub(crate) fn capture_before(
    tool_name: &str,
    input: &Value,
    base: &Path,
) -> Option<FileCheckpoint> {
    if !matches!(tool_name, "write" | "edit" | "delete") {
        return None;
    }
    let raw_path = input.get("path").and_then(Value::as_str)?;
    let resolved = super::output::normalize_agent_tool_path(Path::new(raw_path), base)?;
    let before = std::fs::read_to_string(&resolved).ok();
    if let Some(text) = before.as_deref() {
        if text.len() > MAX_CHECKPOINT_BYTES {
            return None;
        }
    }
    let before_hash = file_hash(before.as_deref());
    Some(FileCheckpoint {
        path: resolved.to_string_lossy().replace('\\', "/"),
        before,
        before_hash,
        after_hash: 0,
        is_rewind: false,
    })
}

/// 执行成功后回填 after_hash；内容未变化（no-op 写入）→ false（不入账）。
pub(crate) fn finish_capture(checkpoint: &mut FileCheckpoint) -> bool {
    let current = std::fs::read_to_string(&checkpoint.path).ok();
    let after_hash = file_hash(current.as_deref());
    if after_hash == checkpoint.before_hash {
        return false;
    }
    checkpoint.after_hash = after_hash.unwrap_or_default();
    true
}

fn store_path(layout: &Layout, channel: &str, chat_id: &str) -> PathBuf {
    let fingerprint = format!("{channel}|{chat_id}");
    layout
        .session_dir
        .join("rewind")
        .join(format!("{:016x}.jsonl", content_hash(&fingerprint)))
}

/// 把本迭代捕获的文件 checkpoint 以 assistant messageId 落盘（尽力而为，失败只告警）。
pub(crate) fn flush(
    layout: &Layout,
    channel: &str,
    chat_id: &str,
    message_id: i64,
    checkpoints: Vec<FileCheckpoint>,
) {
    if checkpoints.is_empty() {
        return;
    }
    let path = store_path(layout, channel, chat_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        tracing::warn!("rewind checkpoint 存储打开失败: {}", path.display());
        return;
    };
    for checkpoint in checkpoints {
        let record = json!({
            "message_id": message_id,
            "path": checkpoint.path,
            "before": checkpoint.before,
            "before_hash": checkpoint.before_hash,
            "after_hash": checkpoint.after_hash,
            "is_rewind": checkpoint.is_rewind,
        });
        if let Err(error) = writeln!(file, "{record}") {
            tracing::warn!("rewind checkpoint 写入失败: {error}");
        }
    }
}

fn load_events(path: &Path) -> Vec<(i64, FileCheckpoint)> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut events = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let (Some(message_id), Some(checkpoint)) = (
            event.get("message_id").and_then(Value::as_i64),
            parse_checkpoint(&event),
        ) else {
            continue;
        };
        events.push((message_id, checkpoint));
    }
    events
}

fn parse_checkpoint(event: &Value) -> Option<FileCheckpoint> {
    Some(FileCheckpoint {
        path: event.get("path")?.as_str()?.to_string(),
        before: event
            .get("before")
            .map(|value| value.as_str().map(str::to_string))
            .unwrap_or(None),
        before_hash: event.get("before_hash").and_then(Value::as_u64),
        after_hash: event.get("after_hash")?.as_u64()?,
        is_rewind: event.get("is_rewind").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// `rewind_files` 工具：按 messageId 选点恢复文件（无 message_id 时列出最近分组）。
pub struct RewindFilesTool {
    pub(crate) runtime: crate::agent_tools::AgentToolRuntime,
}

impl RewindFilesTool {
    pub fn new(runtime: crate::agent_tools::AgentToolRuntime) -> Self {
        RewindFilesTool { runtime }
    }
}

#[async_trait::async_trait]
impl rclaw_tools::Tool for RewindFilesTool {
    fn name(&self) -> &str {
        "rewind_files"
    }

    fn description(&self) -> &str {
        "Rewind workspace files to the state BEFORE a specific assistant message (checkpoint granularity:          every write/edit/delete this session made is journaled with before/after hashes).          Without message_id, lists recent rewind points. Restore is all-or-nothing and          refuses when any touched file was modified after the checkpoint (hash mismatch) —          resolve that manually first. Session history is NOT touched, only files."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message_id": {"type": "integer", "description": "Assistant message id to rewind to (from the list below / /rewind output). Omit to list rewind points."}
            }
        })
    }

    async fn execute(&self, input: Value) -> rclaw_tools::ToolResult {
        let Some((channel, chat_id)) = current_chat() else {
            return rclaw_tools::ToolResult::err(
                "rewind_files 只能在会话 turn 内使用（缺少会话上下文）".to_string(),
            );
        };
        match input.get("message_id").and_then(Value::as_i64) {
            Some(message_id) => match restore_to_message(&self.runtime.layout(), &channel, &chat_id, message_id) {
                Ok(report) => rclaw_tools::ToolResult::ok(format!(
                    "{report}

注意：会话历史未回滚；如需继续在被回滚前的方向上工作，请明确说明后续计划。"
                )),
                Err(error) => rclaw_tools::ToolResult::err(error),
            },
            None => {
                let groups = list_groups(&self.runtime.layout(), &channel, &chat_id);
                if groups.is_empty() {
                    return rclaw_tools::ToolResult::ok(
                        "本会话还没有可回滚点（仅 write/edit/delete 会被 checkpoint）。".to_string(),
                    );
                }
                let mut out = format!("可回滚点（最近 {} 组，新在前）：", groups.len());
                for (message_id, files) in &groups {
                    out.push_str(&format!("
- 消息 {message_id}（{} 个文件）", files.len()));
                    for (path, _) in files {
                        out.push_str(&format!("
  - {path}"));
                    }
                }
                rclaw_tools::ToolResult::ok(out)
            }
        }
    }
}

/// 列出最近的 checkpoint 分组（message_id → 文件与写入后 hash），新在前。
pub(crate) fn list_groups(
    layout: &Layout,
    channel: &str,
    chat_id: &str,
) -> Vec<(i64, Vec<(String, u64)>)> {
    let mut groups: Vec<(i64, Vec<(String, u64)>)> = Vec::new();
    for (message_id, checkpoint) in load_events(&store_path(layout, channel, chat_id))
        .into_iter()
        .filter(|(_, checkpoint)| !checkpoint.is_rewind)
    {
        match groups.iter_mut().find(|(id, _)| *id == message_id) {
            Some((_, files)) => files.push((checkpoint.path, checkpoint.after_hash)),
            None => groups.push((
                message_id,
                vec![(checkpoint.path, checkpoint.after_hash)],
            )),
        }
    }
    groups.reverse();
    groups.truncate(LIST_LIMIT);
    groups
}

/// 按 messageId 选点恢复：回滚消息 M 及其后全部 checkpointed 写入
/// （最新优先，把工作区还原到消息 M 之前的状态）。
///
/// 安全：每个文件先与【它最新一次 checkpoint 的 after_hash】比对，不符即被外部
/// 改动 → 整组拒绝（all-or-nothing，绝不盲恢复）。会话历史不动，只回滚文件。
pub(crate) fn restore_to_message(
    layout: &Layout,
    channel: &str,
    chat_id: &str,
    message_id: i64,
) -> Result<String, String> {
    let all_events = load_events(&store_path(layout, channel, chat_id));
    let mut rollback: Vec<FileCheckpoint> = all_events
        .iter()
        .filter(|(id, checkpoint)| *id >= message_id && !checkpoint.is_rewind)
        .map(|(_, checkpoint)| checkpoint.clone())
        .collect();
    if rollback.is_empty() {
        return Err(format!(
            "消息 {message_id} 没有文件 checkpoint（仅 write/edit/delete 会被记录；回滚会同时撤销该消息之后的所有 checkpointed 写入）"
        ));
    }
    // 最新优先（load_events 保持文件序 = 时间序，反转即最新在前）。
    rollback.reverse();
    // 安全校验：每个文件与其【最新】checkpoint（含 synthetic 回滚标记）比对——
    // 回滚本身会重锚校验链，因此连环回滚不会被上一次回滚误判为外部改动。
    let mut unsafe_files: Vec<String> = Vec::new();
    let mut verified: std::collections::HashSet<String> = std::collections::HashSet::new();
    for checkpoint in all_events.iter().rev().map(|(_, c)| c) {
        if !verified.insert(checkpoint.path.clone()) {
            continue;
        }
        let current = std::fs::read_to_string(&checkpoint.path).ok();
        if file_hash(current.as_deref()) != Some(checkpoint.after_hash) {
            unsafe_files.push(checkpoint.path.clone());
        }
    }
    if !unsafe_files.is_empty() {
        return Err(format!(
            "以下文件在 checkpoint 之后被外部改动，拒绝盲恢复（请人工核对后处理）：{}",
            unsafe_files.join("、")
        ));
    }
    let mut restored: Vec<String> = Vec::new();
    let mut recreated: Vec<String> = Vec::new();
    let mut final_state: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    for checkpoint in &rollback {
        match &checkpoint.before {
            Some(content) => {
                rclaw_store::fs::atomic_write(Path::new(&checkpoint.path), content.as_bytes())
                    .map_err(|error| format!("恢复 {} 失败: {error}", checkpoint.path))?;
                if !restored.contains(&checkpoint.path) {
                    restored.push(checkpoint.path.clone());
                }
            }
            None => {
                // before 为空 = 写入时文件不存在（新建）→ 回滚即删除。
                match std::fs::remove_file(&checkpoint.path) {
                    Ok(()) => recreated.push(checkpoint.path.clone()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!("删除 {} 失败: {error}", checkpoint.path))
                    }
                }
            }
        }
        // 最新优先迭代：同文件最后一次赋值 = 回滚完成后的最终内容（供重锚校验链）。
        final_state.insert(checkpoint.path.clone(), checkpoint.before.clone());
    }
    // 回滚完成后落 synthetic checkpoint：重锚每个被回滚文件的校验链，
    // 使后续更早选点的回滚不会被本次回滚误判为外部改动。
    let store = store_path(layout, channel, chat_id);
    if let Some(parent) = store.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&store)
    {
        for (path, content) in &final_state {
            let record = json!({
                "message_id": message_id,
                "path": path,
                "before": Value::Null,
                "before_hash": Value::Null,
                "after_hash": file_hash(content.as_deref()),
                "is_rewind": true,
            });
            let _ = writeln!(file, "{record}");
        }
    }
    let mut report = format!(
        "✅ 已回滚消息 {message_id} 及其后的全部 checkpointed 写入（恢复到消息 {message_id} 之前的文件状态）：{} 个文件恢复原内容，{} 个新建文件已删除。",
        restored.len(),
        recreated.len()
    );
    for path in restored.iter().chain(recreated.iter()) {
        report.push_str(&format!("\n- {path}"));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_layout(dir: &Path) -> Layout {
        Layout::new(dir.join("agent"))
    }

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn capture_records_relative_path_against_agent_base() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("agent");
        write_file(&base.join("notes/a.md"), "v1");
        let input = json!({"path": "notes/a.md", "content": "v2"});
        let checkpoint = capture_before("write", &input, &base).expect("应捕获");
        assert!(checkpoint.path.ends_with("notes/a.md"));
        assert_eq!(checkpoint.before.as_deref(), Some("v1"));
        assert!(checkpoint.before_hash.is_some());
        // 非 write/edit/delete 不捕获。
        assert!(capture_before("shell", &json!({"command": "ls"}), &base).is_none());
    }

    #[test]
    fn restore_round_trip_and_delete_recreation() {
        let dir = tempfile::tempdir().unwrap();
        let layout = test_layout(dir.path());
        let base = layout.base.clone();
        let file_a = base.join("a.txt");
        let file_b = base.join("b.txt");
        write_file(&file_a, "v1");
        // 消息 100：改 a + 新建 b。
        let mut c1 = capture_before("write", &json!({"path": "a.txt"}), &base).unwrap();
        write_file(&file_a, "v2");
        assert!(finish_capture(&mut c1));
        let mut c2 = capture_before("write", &json!({"path": "b.txt"}), &base).unwrap();
        write_file(&file_b, "new file");
        assert!(finish_capture(&mut c2));
        flush(&layout, "cli", "console", 100, vec![c1, c2]);

        // 消息 200：又改 a（模拟后续工作）。
        let mut c3 = capture_before("write", &json!({"path": "a.txt"}), &base).unwrap();
        write_file(&file_a, "v3");
        assert!(finish_capture(&mut c3));
        flush(&layout, "cli", "console", 200, vec![c3]);

        let groups = list_groups(&layout, "cli", "console");
        assert_eq!(groups.len(), 2, "两个消息分组");
        assert_eq!(groups[0].0, 200, "新在前");

        // 选点回滚到 200：撤销 200 的写入 → a 回到 v2，b 不受影响。
        let report = restore_to_message(&layout, "cli", "console", 200).unwrap();
        assert!(report.contains("回滚消息 200"), "{report}");
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "v2");
        assert!(file_b.exists(), "200 未触碰的文件保持不变");

        // 选点回滚到 100：撤销 100 与 200 → a 回到 v1，b（100 新建）被删除。
        restore_to_message(&layout, "cli", "console", 100).unwrap();
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "v1");
        assert!(!file_b.exists(), "100 新建的文件应随回滚删除");
    }

    #[test]
    fn restore_refuses_externally_modified_files_without_touching_anything() {
        let dir = tempfile::tempdir().unwrap();
        let layout = test_layout(dir.path());
        let base = layout.base.clone();
        let file_a = base.join("a.txt");
        let file_b = base.join("b.txt");
        write_file(&file_a, "v1");
        write_file(&file_b, "keep");
        let mut c1 = capture_before("write", &json!({"path": "a.txt"}), &base).unwrap();
        write_file(&file_a, "v2");
        assert!(finish_capture(&mut c1));
        let mut c2 = capture_before("write", &json!({"path": "b.txt"}), &base).unwrap();
        write_file(&file_b, "v2b");
        assert!(finish_capture(&mut c2));
        flush(&layout, "cli", "console", 300, vec![c1, c2]);

        // 外部改动 a → 整组拒绝，b 保持原样。
        write_file(&file_a, "externally edited");
        let error = restore_to_message(&layout, "cli", "console", 300).unwrap_err();
        assert!(error.contains("外部改动"), "{error}");
        assert_eq!(std::fs::read_to_string(&file_b).unwrap(), "v2b", "all-or-nothing");
    }

    #[test]
    fn restore_errors_when_no_checkpoints_for_message() {
        let dir = tempfile::tempdir().unwrap();
        let layout = test_layout(dir.path());
        let error = restore_to_message(&layout, "cli", "console", 42).unwrap_err();
        assert!(error.contains("没有文件 checkpoint"), "{error}");
    }

    #[test]
    fn message_ids_are_unique_and_monotonic() {
        let first = next_message_id();
        let second = next_message_id();
        assert!(second > first, "messageId 必须严格递增: {first} vs {second}");
    }
}
