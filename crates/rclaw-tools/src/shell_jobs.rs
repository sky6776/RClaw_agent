//! 后台 shell 任务表与 `shell_poll` 工具（借鉴 Codex unified_exec 的
//! 「yield + id + poll」语义）。
//!
//! - `shell(background=true)`：立即返回 `job_id`，进程脱离等待、输出落盘；
//! - `shell_poll(job_id)`：读取任务状态与输出的头尾（HeadTail 语义，中段省略
//!   但文件完整保留），终态轮询会把任务移出进程表（输出文件仍在磁盘上，
//!   之后仍可继续 `shell_poll` 查看）。
//!
//! 进程表上限 [`MAX_BACKGROUND_JOBS`]，插入时按创建顺序驱逐【已结束】的任务；
//! 全部未结束则拒绝新的后台请求——绝不驱逐仍在产出的进程。

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::truncate::{advance_to_boundary, floor_char_boundary, omission_marker, HeadTailOutput};
use crate::{Tool, ToolResult};

/// 进程表上限（LRU 驱逐仅针对已结束任务）。
pub(crate) const MAX_BACKGROUND_JOBS: usize = 64;
/// 单个后台任务输出文件的落盘上限；达到后停止写入并留标记（读取端可见）。
pub(crate) const MAX_JOB_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
/// `shell_poll` 默认的头/尾保留字节数（每侧）。
const DEFAULT_POLL_BYTES: u64 = 16_384;
/// `shell_poll` 每侧头/尾保留的最大字节数。
const MAX_POLL_BYTES: u64 = 65_536;

/// 后台任务终态。由 watcher 在进程退出后写入；`Running` = 仍在运行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobExit {
    Running,
    Code(i32),
    Signal,
}

type ExitCell = Arc<StdMutex<JobExit>>;

pub(crate) struct ShellJob {
    pub(crate) command: String,
    pub(crate) output_path: PathBuf,
    /// stdout/stderr 读取任务；终态轮询时短暂等待它们以冲刷最后几字节。
    pub(crate) readers: Vec<tokio::task::JoinHandle<()>>,
    pub(crate) exit: ExitCell,
}

impl ShellJob {
    fn view(&self) -> ShellJob {
        ShellJob {
            command: self.command.clone(),
            output_path: self.output_path.clone(),
            readers: Vec::new(), // JoinHandle 不可克隆；只读视图不携带 readers。
            exit: self.exit.clone(),
        }
    }

    fn is_finished(&self) -> bool {
        *self.exit.lock().expect("exit cell 未被毒化") != JobExit::Running
    }
}

#[derive(Default)]
struct TableState {
    map: HashMap<String, ShellJob>,
    order: VecDeque<String>,
}

/// 进程内后台任务表。跨 `Shell` / `ShellPoll` 工具实例共享。
#[derive(Clone, Default)]
pub struct ShellJobTable {
    state: Arc<StdMutex<TableState>>,
    counter: Arc<AtomicU64>,
}

impl ShellJobTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 运行中任务数（供测试与诊断）。
    pub fn running_jobs(&self) -> usize {
        self.state
            .lock()
            .expect("shell job table 未被毒化")
            .map
            .values()
            .filter(|job| !job.is_finished())
            .count()
    }

    /// 插入前保证有空位：按创建顺序驱逐【第一个已结束】的任务；表满且全部
    /// 未结束 → Err（绝不驱逐仍在产出的进程）。
    fn ensure_capacity(&self) -> Result<(), String> {
        let mut state = self.state.lock().expect("shell job table 未被毒化");
        while state.map.len() >= MAX_BACKGROUND_JOBS {
            let Some(victim) = state
                .order
                .iter()
                .find(|id| state.map.get(*id).is_some_and(ShellJob::is_finished))
                .cloned()
            else {
                return Err(format!(
                    "后台任务数已达上限 {MAX_BACKGROUND_JOBS} 且全部仍在运行；请等部分任务结束后（shell_poll 查看状态）再试"
                ));
            };
            state.map.remove(&victim);
            state.order.retain(|id| id != &victim);
        }
        Ok(())
    }

    fn insert(&self, id: String, job: ShellJob) {
        let mut state = self.state.lock().expect("shell job table 未被毒化");
        state.map.insert(id.clone(), job);
        state.order.push_back(id);
    }

    /// 只读视图（command / output_path / exit），不取走 readers。
    fn peek(&self, id: &str) -> Option<ShellJob> {
        self.state
            .lock()
            .expect("shell job table 未被毒化")
            .map
            .get(id)
            .map(ShellJob::view)
    }

    /// 取走整个任务（终态轮询用）；之后 `peek` 返回 None，输出文件保留在磁盘。
    fn take(&self, id: &str) -> Option<ShellJob> {
        let mut state = self.state.lock().expect("shell job table 未被毒化");
        let job = state.map.remove(id)?;
        state.order.retain(|existing| existing != id);
        Some(job)
    }

    /// 测试可见性包装：暴露容量检查（驱逐 + 满表拒绝）。
    #[cfg(test)]
    fn ensure_capacity_for_test(&self) -> Result<(), String> {
        self.ensure_capacity()
    }
}

/// 后台任务输出落盘器：两个流读取任务共享，总量达 [`MAX_JOB_OUTPUT_BYTES`]
/// 后停止写入并一次性写标记（读取端保持排空管道，避免子进程被写满的管道卡住）。
pub(crate) struct OutputWriter {
    file: StdMutex<std::fs::File>,
    written: AtomicU64,
    cap_noted: AtomicBool,
}

impl OutputWriter {
    fn open(path: &Path) -> std::io::Result<Arc<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(path)?;
        Ok(Arc::new(OutputWriter {
            file: StdMutex::new(file),
            written: AtomicU64::new(0),
            cap_noted: AtomicBool::new(false),
        }))
    }

    fn write_all(&self, bytes: &[u8]) {
        let mut file = self.file.lock().expect("output writer 未被毒化");
        let pos = self.written.load(Ordering::Relaxed);
        if pos >= MAX_JOB_OUTPUT_BYTES {
            self.note_cap(&mut file);
            return;
        }
        let remaining = (MAX_JOB_OUTPUT_BYTES - pos) as usize;
        let take = bytes.len().min(remaining);
        if file.write_all(&bytes[..take]).is_ok() {
            self.written.store(pos + take as u64, Ordering::Relaxed);
        }
        if take < bytes.len() {
            self.note_cap(&mut file);
        }
    }

    fn note_cap(&self, file: &mut std::fs::File) {
        if !self.cap_noted.swap(true, Ordering::Relaxed) {
            let _ = writeln!(
                file,
                "\n[output capped at {MAX_JOB_OUTPUT_BYTES} bytes; further output discarded]"
            );
        }
    }
}

/// `shell(background=true)` 的产物：任务号与输出文件路径。
pub(crate) struct BackgroundSpawn {
    pub(crate) job_id: String,
    pub(crate) output_path: PathBuf,
}

/// 启动一个后台 shell 任务：保证进程表容量、落盘输出、启动读取/监视任务并登记。
///
/// 调用方负责构造好工作目录与环境变量的 `cmd`（与前台同一条 `shell_command` 路径）。
/// 返回 `Err` 时进程【未启动】。
pub(crate) fn spawn_background_job(
    table: &ShellJobTable,
    jobs_dir: &Path,
    command: String,
    mut cmd: tokio::process::Command,
) -> Result<BackgroundSpawn, String> {
    table.ensure_capacity()?;
    let seq = table.counter.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let job_id = format!("job-{millis}-{seq}");
    let output_path = jobs_dir.join(format!("{job_id}.log"));
    let writer =
        OutputWriter::open(&output_path).map_err(|e| format!("创建后台任务输出文件失败: {e}"))?;

    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("命令启动失败: {e}"))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut readers = Vec::new();
    if let Some(out) = stdout {
        readers.push(spawn_stream_reader(out, writer.clone()));
    }
    if let Some(err) = stderr {
        readers.push(spawn_stream_reader(err, writer));
    }
    let exit: ExitCell = Arc::new(StdMutex::new(JobExit::Running));
    let exit_for_watcher = exit.clone();
    tokio::spawn(async move {
        let status = child.wait().await.ok();
        let exit_state = match status.and_then(|s| s.code()) {
            Some(code) => JobExit::Code(code),
            None => JobExit::Signal,
        };
        *exit_cell(&exit_for_watcher) = exit_state;
    });
    table.insert(
        job_id.clone(),
        ShellJob {
            command,
            output_path: output_path.clone(),
            readers,
            exit,
        },
    );
    Ok(BackgroundSpawn {
        job_id,
        output_path,
    })
}

/// 帮助函数：拿 exit cell 的锁（集中 `expect` 文案）。
fn exit_cell(cell: &ExitCell) -> std::sync::MutexGuard<'_, JobExit> {
    cell.lock().expect("exit cell 未被毒化")
}

fn spawn_stream_reader<R>(mut stream: R, writer: Arc<OutputWriter>) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => writer.write_all(&buf[..n]),
            }
        }
    })
}

/// `shell_poll` 入参 job_id 合法性：与 [`spawn_background_job`] 生成的
/// `job-{millis}-{seq}` 同构（`job-` 前缀 + ASCII 字母数字/连字符）。
/// 先校验再拼输出文件路径，杜绝 `../`、绝对路径等把轮询变成任意文件读取。
fn is_valid_job_id(job_id: &str) -> bool {
    job_id.len() <= 64
        && job_id.strip_prefix("job-").is_some_and(|rest| {
            !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// `shell_poll`：轮询后台 shell 任务的状态与输出头尾。
pub struct ShellPoll {
    jobs: ShellJobTable,
    jobs_dir: PathBuf,
}

impl ShellPoll {
    /// 与 [`crate::Shell`] 共享同一张任务表与输出目录。
    pub fn with_table(jobs: ShellJobTable, jobs_dir: PathBuf) -> Self {
        ShellPoll { jobs, jobs_dir }
    }
}

#[async_trait]
impl Tool for ShellPoll {
    fn name(&self) -> &str {
        "shell_poll"
    }

    fn description(&self) -> &str {
        "Poll a background job started with shell(background=true): returns its status and the head/tail of its output (stdout+stderr merged; the full log stays on disk). A finished job is removed from the process table by its final poll, but remains pollable."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "job_id": {"type": "string", "description": "The job_id returned by shell(background=true)."},
                "max_bytes": {
                    "type": "integer",
                    "minimum": 256,
                    "maximum": MAX_POLL_BYTES,
                    "description": format!("Head/tail bytes to keep per side. Default {DEFAULT_POLL_BYTES}, max {MAX_POLL_BYTES}.")
                }
            },
            "required": ["job_id"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(job_id) = input
            .get("job_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return ToolResult::err("缺少非空 'job_id' 字段");
        };
        if !is_valid_job_id(job_id) {
            return ToolResult::err(format!("非法 job_id: {job_id}"));
        }
        let max_bytes = match input.get("max_bytes") {
            None => DEFAULT_POLL_BYTES,
            Some(value) => match value.as_u64() {
                Some(value) if (256..=MAX_POLL_BYTES).contains(&value) => value,
                _ => {
                    return ToolResult::err(format!(
                        "max_bytes 必须是 256..={MAX_POLL_BYTES} 的整数"
                    ))
                }
            },
        };
        let output_path = self.jobs_dir.join(format!("{job_id}.log"));

        let Some(job) = self.jobs.peek(job_id) else {
            // 不在进程表：来自先前进程、或已被终态轮询收回。文件仍在则照常读。
            return match read_output_head_tail(&output_path, max_bytes) {
                Ok(output) => ToolResult::ok(render_poll(
                    "unknown_untracked",
                    job_id,
                    &output_path,
                    &output,
                )),
                Err(_) => ToolResult::err(format!("未知后台任务: {job_id}")),
            };
        };

        let exit = *exit_cell(&job.exit);
        if exit != JobExit::Running {
            // 终态：取走任务，短暂等待读取任务冲刷最后几字节，再读文件。
            if let Some(finished) = self.jobs.take(job_id) {
                for reader in finished.readers {
                    let _ = tokio::time::timeout(Duration::from_secs(2), reader).await;
                }
            }
            let status_text = match exit {
                JobExit::Code(code) => code.to_string(),
                _ => "terminated_by_signal".to_string(),
            };
            let output = match read_output_head_tail(&job.output_path, max_bytes) {
                Ok(output) => output,
                Err(e) => return ToolResult::err(format!("读取输出文件失败: {e}")),
            };
            let content = render_poll(&status_text, job_id, &job.output_path, &output);
            return if exit == JobExit::Code(0) {
                ToolResult::ok(content)
            } else {
                ToolResult::err(content)
            };
        }

        // 仍在运行。
        let output = match read_output_head_tail(&job.output_path, max_bytes) {
            Ok(output) => output,
            Err(e) => return ToolResult::err(format!("读取输出文件失败: {e}")),
        };
        ToolResult::ok(render_poll("running", job_id, &job.output_path, &output))
    }
}

/// 从输出文件读头尾各 `max_per_side` 字节（HeadTail 语义 + 原始字节数标注）。
pub(crate) fn read_output_head_tail(
    path: &Path,
    max_per_side: u64,
) -> std::io::Result<HeadTailOutput> {
    let max_per_side = max_per_side.max(1) as usize;
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len == 0 {
        return Ok(HeadTailOutput {
            text: String::new(),
            total_bytes: 0,
            truncated: false,
        });
    }
    if len <= max_per_side * 2 {
        let mut buf = Vec::with_capacity(len);
        file.read_to_end(&mut buf)?;
        return Ok(HeadTailOutput {
            text: String::from_utf8_lossy(&buf).into_owned(),
            total_bytes: len,
            truncated: false,
        });
    }
    // 头侧：从 0 读 max_per_side，末端回退到字符边界。
    let mut head = vec![0u8; max_per_side];
    file.read_exact(&mut head)?;
    let head_end = floor_char_boundary(&head, head.len());
    // 尾侧：从 len - max_per_side 读到 EOF，起点在缓冲内推进到字符边界。
    let tail_start = len.saturating_sub(max_per_side);
    file.seek(SeekFrom::Start(tail_start as u64))?;
    let mut tail = Vec::with_capacity(len - tail_start);
    file.read_to_end(&mut tail)?;
    let tail_begin = advance_to_boundary(&tail);
    let head_text = String::from_utf8_lossy(&head[..head_end]);
    let tail_text = String::from_utf8_lossy(&tail[tail_begin..]);
    let kept = head_end + (tail.len() - tail_begin);
    let omitted = len - kept;
    Ok(HeadTailOutput {
        text: format!("{head_text}{}{tail_text}", omission_marker(omitted, len)),
        total_bytes: len,
        truncated: true,
    })
}

fn render_poll(status: &str, job_id: &str, output_path: &Path, output: &HeadTailOutput) -> String {
    let mut out = format!("status: {status}\njob_id: {job_id}\n");
    out.push_str(&format!(
        "output file: {}\n",
        output_path.to_string_lossy().replace('\\', "/")
    ));
    out.push_str("\n[output]\n");
    out.push_str(&output.text);
    if output.truncated {
        out.push_str(&format!(
            "\n[output truncated: original {} bytes, first/last halves kept]\n",
            output.total_bytes
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job_finished() -> ShellJob {
        ShellJob {
            command: "echo".into(),
            output_path: PathBuf::from("/tmp/x.log"),
            readers: Vec::new(),
            exit: Arc::new(StdMutex::new(JobExit::Code(0))),
        }
    }

    fn job_running() -> ShellJob {
        ShellJob {
            command: "echo".into(),
            output_path: PathBuf::from("/tmp/x.log"),
            readers: Vec::new(),
            exit: Arc::new(StdMutex::new(JobExit::Running)),
        }
    }

    #[test]
    fn insert_evicts_only_finished_jobs_and_refuses_when_all_running() {
        let t = ShellJobTable::new();
        // 全部运行中且表满 → 拒绝（绝不驱逐仍在产出的进程）。
        for i in 0..MAX_BACKGROUND_JOBS {
            t.insert(format!("run-{i}"), job_running());
        }
        assert_eq!(t.running_jobs(), MAX_BACKGROUND_JOBS);
        assert!(
            t.ensure_capacity_for_test().is_err(),
            "全为运行中任务时应拒绝新的后台请求"
        );

        // 表满、唯一的已结束任务位于创建序【最老】位置 → 驱逐它腾出空位。
        let t = ShellJobTable::new();
        t.insert("done-old".into(), job_finished());
        for i in 0..(MAX_BACKGROUND_JOBS - 1) {
            t.insert(format!("run-{i}"), job_running());
        }
        assert!(t.ensure_capacity_for_test().is_ok());
        assert!(t.peek("done-old").is_none(), "已结束的最老任务应被驱逐");
        assert!(t.peek("run-0").is_some(), "运行中的任务不得被驱逐");

        // 表满、唯一的已结束任务位于创建序【末尾】（前面全是运行中）
        // → 跳过运行中、驱逐它。
        let t = ShellJobTable::new();
        for i in 0..(MAX_BACKGROUND_JOBS - 1) {
            t.insert(format!("run-{i}"), job_running());
        }
        t.insert("done-new".into(), job_finished());
        assert!(t.ensure_capacity_for_test().is_ok());
        assert!(
            t.peek("done-new").is_none(),
            "应跳过运行中的任务、驱逐已结束者"
        );
    }

    #[test]
    fn take_removes_job_from_table() {
        let t = ShellJobTable::new();
        t.insert("j1".into(), job_finished());
        assert!(t.peek("j1").is_some());
        let job = t.take("j1").expect("终态任务应可取走");
        assert_eq!(job.command, "echo");
        assert!(t.peek("j1").is_none(), "取走后不应再出现在进程表");
    }

    #[test]
    fn output_writer_caps_and_notes_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.log");
        let writer = OutputWriter::open(&path).unwrap();
        let chunk = vec![b'x'; 4096];
        let writes = (MAX_JOB_OUTPUT_BYTES / 4096) + 5;
        for _ in 0..writes {
            writer.write_all(&chunk);
        }
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("output capped at"));
        assert_eq!(content.matches("output capped at").count(), 1);
        assert!(
            (content.len() as u64) <= MAX_JOB_OUTPUT_BYTES + 128,
            "{}",
            content.len()
        );
    }

    #[test]
    fn read_output_head_tail_keeps_head_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.log");
        let head = "HEAD";
        let middle = "m".repeat(5_000);
        let tail = "Traceback: boom";
        std::fs::write(&path, format!("{head}{middle}\n{tail}")).unwrap();

        let out = read_output_head_tail(&path, 256).unwrap();
        assert!(out.truncated);
        assert_eq!(out.total_bytes, head.len() + middle.len() + 1 + tail.len());
        assert!(out.text.starts_with(head), "{}", out.text);
        assert!(out.text.ends_with(tail), "{}", out.text);
        assert!(out.text.contains("bytes omitted"), "{}", out.text);
    }

    #[test]
    fn read_output_head_tail_handles_small_and_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("small.log");
        std::fs::write(&small, "tiny").unwrap();
        let out = read_output_head_tail(&small, 256).unwrap();
        assert!(!out.truncated);
        assert_eq!(out.text, "tiny");

        let empty = dir.path().join("empty.log");
        std::fs::write(&empty, "").unwrap();
        let out = read_output_head_tail(&empty, 256).unwrap();
        assert!(!out.truncated);
        assert_eq!(out.text, "");
    }
}
