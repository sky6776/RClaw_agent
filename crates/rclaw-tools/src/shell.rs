//! 命令执行工具：在 agent 存储目录内运行 shell 命令。
//!
//! 安全约束：
//! - 工作目录必须落在 agent 存储根目录内；
//! - 命令有长度、超时和输出大小上限；
//! - stdout/stderr 分别截断，避免把无限输出灌进上下文。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command as TokioCommand;
use tokio::time::timeout;

use crate::shell_jobs::{spawn_background_job, ShellJobTable};
use crate::{truncate::HeadTailBuffer, Tool, ToolResult};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
// Long-running skills may perform several bounded network retries. Callers
// still have to opt in explicitly; the default remains 30 seconds.
const MAX_TIMEOUT_MS: u64 = 1_500_000;
const MAX_COMMAND_BYTES: usize = 8192;
const MAX_STREAM_BYTES: usize = 64 * 1024;
/// 后台任务输出目录（相对 agent 存储根）。
pub(crate) const JOBS_DIR_NAME: &str = ".shell-jobs";

pub struct Shell {
    base: PathBuf,
    jobs: ShellJobTable,
}

impl Shell {
    pub fn new(base: PathBuf) -> Self {
        Shell {
            base,
            jobs: ShellJobTable::new(),
        }
    }

    /// 与一个 [`ShellPoll`] 共享后台任务表（`ToolRegistry::with_defaults` 用）。
    pub fn with_job_table(base: PathBuf, jobs: ShellJobTable) -> Self {
        Shell { base, jobs }
    }

    /// 后台任务表的共享句柄（嵌入方/测试用它把 [`crate::ShellPoll`] 接到同一张表）。
    pub fn job_table(&self) -> ShellJobTable {
        self.jobs.clone()
    }

    fn jobs_dir(&self) -> PathBuf {
        self.base.join(JOBS_DIR_NAME)
    }
}

#[async_trait]
impl Tool for Shell {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute an ordinary project or system shell command. 已读取的 Skill 包脚本必须改用 Manifest 动态专用工具，不得在 shell 中手写其脚本路径或参数。cwd outside agent storage requires unrestricted access."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to run. Keep it short and explicit."
                },
                "cwd": {
                    "type": "string",
                    "description": format!("Working directory (outside {}/ requires unrestricted access). Defaults to the agent storage root.", base_str(&self.base))
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TIMEOUT_MS,
                    "description": format!("Timeout in milliseconds. Default {DEFAULT_TIMEOUT_MS}, max {MAX_TIMEOUT_MS}. Ignored when background=true.")
                },
                "background": {
                    "type": "boolean",
                    "description": "Run detached in background: returns a job_id immediately (timeout_ms is ignored). Poll output and exit status with shell_poll."
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let command = match get_str(&input, "command") {
            Some(s) if !s.trim().is_empty() => s,
            _ => return ToolResult::err("缺少非空 'command' 字段"),
        };
        if command.len() > MAX_COMMAND_BYTES {
            return ToolResult::err(format!(
                "command 过长（{} 字节 > 上限 {MAX_COMMAND_BYTES}）",
                command.len()
            ));
        }

        let timeout_ms = match parse_timeout_ms(&input) {
            Ok(ms) => ms,
            Err(e) => return e,
        };

        let cwd = match resolve_cwd(&self.base, get_str(&input, "cwd")).await {
            Ok(p) => p,
            Err(e) => return e,
        };

        let mut cmd = shell_command(command);
        cmd.current_dir(&cwd);
        apply_runtime_env(&mut cmd, &input);

        // T1 后台命令：立即返回 job_id，进程脱离等待、stdout+stderr 合并落盘；
        // 轮询走 shell_poll(job_id)。timeout_ms 不适用于后台任务。
        if get_bool(&input, "background") {
            return match spawn_background_job(
                &self.jobs,
                &self.jobs_dir(),
                command.to_string(),
                cmd,
            ) {
                Ok(spawn) => ToolResult::ok(format!(
                    "status: started\ncommand: {command}\ncwd: {}\njob_id: {}\noutput file: {}\n\n\
                     后台任务已启动（stdout+stderr 合并落盘）。用 shell_poll(job_id=\"{}\") \
                     查看输出头尾与最终退出码；终态轮询会自动移出进程表。",
                    cwd.to_string_lossy().replace('\\', "/"),
                    spawn.job_id,
                    spawn.output_path.to_string_lossy().replace('\\', "/"),
                    spawn.job_id
                )),
                Err(e) => ToolResult::err(e),
            };
        }

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                return ToolResult::err(format!("命令启动失败: {e}"));
            }
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_task = tokio::spawn(async move {
            match stdout {
                Some(out) => read_limited(out, MAX_STREAM_BYTES).await,
                None => Ok((String::new(), 0, false)),
            }
        });
        let stderr_task = tokio::spawn(async move {
            match stderr {
                Some(err) => read_limited(err, MAX_STREAM_BYTES).await,
                None => Ok((String::new(), 0, false)),
            }
        });

        let mut timed_out = false;
        let status = match timeout(Duration::from_millis(timeout_ms), child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(e)) => {
                return ToolResult::err(format!("等待命令结束失败: {e}"));
            }
            Err(_) => {
                timed_out = true;
                let _ = child.kill().await;
                None
            }
        };

        let (stdout, stdout_total, stdout_truncated) = join_reader(stdout_task).await;
        let (stderr, stderr_total, stderr_truncated) = join_reader(stderr_task).await;
        let content = render_result(ShellRenderResult {
            command,
            cwd: &cwd,
            exit_code: status.map(|s| s.code()),
            timed_out,
            stdout: &stdout,
            stdout_total,
            stdout_truncated,
            stderr: &stderr,
            stderr_total,
            stderr_truncated,
        });

        if timed_out || status.is_some_and(|s| !s.success()) {
            ToolResult::err(content)
        } else {
            ToolResult::ok(content)
        }
    }
}

fn get_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(|v| v.as_str())
}

fn get_bool(input: &Value, key: &str) -> bool {
    input.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// 子进程运行时环境：RCLAW_SOURCE_CHANNEL 透传。
///
/// 所有外部 channel 共用媒体沙箱，但 marker 前缀不同；来源必须来自本轮
/// 运行时保留字段。先清除进程级旧值，避免并发/后续 turn 把文件回执给错误 channel；
/// 有界 ASCII 校验与 marker/能力变量语法保持一致，避免把任意文本带入子进程环境。
/// 前台与后台两条路径共用。
fn apply_runtime_env(cmd: &mut TokioCommand, input: &Value) {
    cmd.env_remove("RCLAW_SOURCE_CHANNEL");
    if let Some(source_channel) = get_str(input, "_rclaw_source_channel")
        .map(str::trim)
        .filter(|value| {
            let mut bytes = value.bytes();
            !value.is_empty()
                && value.len() <= 64
                && bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
                && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    {
        cmd.env("RCLAW_SOURCE_CHANNEL", source_channel);
    }
}

fn parse_timeout_ms(input: &Value) -> Result<u64, ToolResult> {
    let Some(v) = input.get("timeout_ms") else {
        return Ok(DEFAULT_TIMEOUT_MS);
    };
    let Some(ms) = v.as_u64() else {
        return Err(ToolResult::err("timeout_ms 必须是正整数"));
    };
    if ms == 0 || ms > MAX_TIMEOUT_MS {
        return Err(ToolResult::err(format!(
            "timeout_ms 必须在 1..={MAX_TIMEOUT_MS} 之间"
        )));
    }
    Ok(ms)
}

async fn resolve_cwd(base: &Path, cwd: Option<&str>) -> Result<PathBuf, ToolResult> {
    let path = match cwd {
        Some(s) if !s.trim().is_empty() => PathBuf::from(s),
        _ => base.to_path_buf(),
    };
    // Unrestricted（max 权限）直放任意目录；工作区内直放；工作区外在宿主注册了
    // 确认钩子时（桌面版）弹窗询问用户，允许才放行——见 [`ensure_path_allowed`]。
    crate::access::ensure_path_allowed(
        base,
        &path,
        &format!("cwd 必须位于 {}/ 之下且不能包含 '..'", base_str(base)),
    )
    .await?;
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_dir() => Ok(path),
        Ok(_) => Err(ToolResult::err("cwd 不是目录")),
        Err(e) => Err(ToolResult::err(format!("cwd 不可访问: {e}"))),
    }
}

fn base_str(base: &Path) -> String {
    base.to_string_lossy().replace('\\', "/")
}

#[cfg(windows)]
fn shell_command(command: &str) -> TokioCommand {
    use std::os::windows::process::CommandExt;

    let mut cmd = TokioCommand::new("cmd");
    cmd.arg("/D").arg("/S").arg("/C");
    // std::process::Command 的常规 arg 转义遵循 CommandLineToArgvW，而 cmd.exe
    // 使用自己的引号规则。直接 `.arg(command)` 会把含双引号的整条命令再次转义，
    // 使 Python 等子进程收到字面量 `"`，例如把 `"reports/中文 文件.md"`
    // 当成文件名的一部分。/S /C 配合 raw_arg 的一对外层引号交给 cmd 自己解析。
    cmd.as_std_mut().raw_arg(format!("\"{command}\""));
    cmd
        // `read_limited` 按 UTF-8 解码工具输出；Windows Python 默认常用 GBK，
        // 中文或 emoji 既可能乱码，也可能在写 stdout 时直接抛 UnicodeEncodeError。
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1");
    cmd
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> TokioCommand {
    // APK 宿主（进程环境带 RCLAW_ANDROID=1，与 rclaw_runtime::HostKind 同约定）
    // 没有 login 环境：toybox sh 固定在 /system/bin，PATH 视 ROM 可能缺系统路径
    // ——绝对路径启动并兜底补 PATH，保证脚本内 tar/unzip 等可解析；且不用 -l
    // （避免 source 系统rc的噪音）。Termux 与桌面维持 `sh -lc` 原语义。
    let apk_host = std::env::var("RCLAW_ANDROID").is_ok_and(|value| value.trim() == "1");
    let mut cmd = if apk_host {
        let mut cmd = TokioCommand::new("/system/bin/sh");
        cmd.arg("-c");
        let needs_path = match std::env::var_os("PATH") {
            Some(path) => !path
                .to_string_lossy()
                .split(':')
                .any(|entry| entry == "/system/bin"),
            None => true,
        };
        if needs_path {
            let fallback: &str = "/system/bin:/system/xbin:/vendor/bin:/product/bin";
            let path = match std::env::var_os("PATH") {
                Some(existing) => {
                    let mut joined = std::ffi::OsString::from("/system/bin:");
                    joined.push(&existing);
                    joined
                }
                None => std::ffi::OsString::from(fallback),
            };
            cmd.env("PATH", path);
        }
        cmd
    } else {
        let mut cmd = TokioCommand::new("sh");
        cmd.arg("-lc");
        cmd
    };
    cmd.arg(command);
    cmd
}

/// 流式读取子进程输出：头尾各半保留（HeadTailBuffer），内存有界，持续排空管道。
///
/// 返回（截断后文本，原始总字节数，是否截断）。中段省略标记已内嵌在文本里。
async fn read_limited<R>(mut reader: R, max: usize) -> std::io::Result<(String, usize, bool)>
where
    R: AsyncRead + Unpin,
{
    let mut buf = HeadTailBuffer::new(max);
    let mut chunk = [0u8; 8192];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.push(&chunk[..n]);
    }
    let out = buf.finish();
    Ok((out.text, out.total_bytes, out.truncated))
}

async fn join_reader(
    task: tokio::task::JoinHandle<std::io::Result<(String, usize, bool)>>,
) -> (String, usize, bool) {
    match task.await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => (format!("[读取输出失败: {e}]"), 0, false),
        Err(e) => (format!("[读取输出任务失败: {e}]"), 0, false),
    }
}

struct ShellRenderResult<'a> {
    command: &'a str,
    cwd: &'a Path,
    exit_code: Option<Option<i32>>,
    timed_out: bool,
    stdout: &'a str,
    stdout_total: usize,
    stdout_truncated: bool,
    stderr: &'a str,
    stderr_total: usize,
    stderr_truncated: bool,
}

fn render_result(result: ShellRenderResult<'_>) -> String {
    let status = if result.timed_out {
        "timeout".to_string()
    } else {
        match result.exit_code {
            Some(Some(code)) => code.to_string(),
            Some(None) => "terminated_by_signal".to_string(),
            None => "unknown".to_string(),
        }
    };
    let mut out = format!(
        "status: {status}\ncommand: {command}\ncwd: {cwd}\n",
        command = result.command,
        cwd = result.cwd.to_string_lossy().replace('\\', "/")
    );
    // 截断采用头尾各半：诊断信息（traceback/错误汇总）集中在输出尾部，保头截断
    // 正好把它切掉；中段省略标记与原始字节数内嵌在正文里，模型可按需取回中段。
    out.push_str("\n[stdout]\n");
    out.push_str(result.stdout);
    if result.stdout_truncated {
        out.push_str(&format!(
            "\n[stdout truncated: original {} bytes, first/last halves kept]\n",
            result.stdout_total
        ));
    }
    out.push_str("\n[stderr]\n");
    out.push_str(result.stderr);
    if result.stderr_truncated {
        out.push_str(&format!(
            "\n[stderr truncated: original {} bytes, first/last halves kept]\n",
            result.stderr_total
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rclaw_core::ToolAccess;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    fn base() -> PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("agent");
        std::fs::create_dir_all(&p).unwrap();
        std::mem::forget(dir);
        p
    }

    #[tokio::test]
    async fn shell_executes_command_in_base() {
        let b = base();
        let shell = Shell::new(b);
        #[cfg(windows)]
        let command = "echo hello";
        #[cfg(not(windows))]
        let command = "printf hello";

        let r = shell.execute(json!({ "command": command })).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("hello"), "{}", r.content);
        assert!(r.content.contains("status: 0"), "{}", r.content);
    }

    #[tokio::test]
    async fn shell_exposes_runtime_source_channel_to_child_process() {
        let b = base();
        let shell = Shell::new(b);
        #[cfg(windows)]
        let command = "echo %RCLAW_SOURCE_CHANNEL%";
        #[cfg(not(windows))]
        let command = "printf %s \"$RCLAW_SOURCE_CHANNEL\"";

        let r = shell
            .execute(json!({
                "command": command,
                "_rclaw_source_channel": "future-channel"
            }))
            .await;

        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("future-channel"), "{}", r.content);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_configures_python_for_utf8_output_on_windows() {
        let b = base();
        let shell = Shell::new(b);

        let r = shell
            .execute(json!({
                "command": "echo %PYTHONIOENCODING% %PYTHONUTF8%"
            }))
            .await;

        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("utf-8 1"), "{}", r.content);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_preserves_cmd_quoting_for_arguments_with_spaces() {
        let b = base();
        let shell = Shell::new(b);

        let r = shell
            .execute(json!({
                "command": "for %I in (\"quoted value\") do @echo %~I"
            }))
            .await;

        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.content.contains("[stdout]\nquoted value\r\n"),
            "{}",
            r.content
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_reads_a_quoted_unicode_path() {
        let b = base();
        let dir = b.join("中文 目录");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("报告 文件.txt"), "path-ok").unwrap();
        let shell = Shell::new(b);

        let r = shell
            .execute(json!({
                "command": "type \"中文 目录\\报告 文件.txt\""
            }))
            .await;

        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("[stdout]\npath-ok"), "{}", r.content);
    }

    #[tokio::test]
    async fn shell_rejects_cwd_outside_base() {
        let b = base();
        let shell = Shell::new(b);
        let outside = std::env::temp_dir().to_string_lossy().to_string();
        let r = shell
            .execute(json!({ "command": "echo no", "cwd": outside }))
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("cwd 必须位于"), "{}", r.content);
    }

    #[tokio::test]
    async fn shell_allows_cwd_outside_base_with_unrestricted_access() {
        let b = base();
        let shell = Shell::new(b);
        let outside = tempfile::tempdir().unwrap();
        let cwd = outside.path().to_string_lossy().to_string();
        let r = crate::access::scope_access(ToolAccess::Unrestricted, async move {
            shell
                .execute(json!({ "command": "echo max", "cwd": cwd }))
                .await
        })
        .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("max"), "{}", r.content);
    }

    #[tokio::test]
    async fn shell_reports_nonzero_exit_as_error() {
        let b = base();
        let shell = Shell::new(b);
        #[cfg(windows)]
        let command = "exit /b 7";
        #[cfg(not(windows))]
        let command = "exit 7";

        let r = shell.execute(json!({ "command": command })).await;
        assert!(r.is_error);
        assert!(r.content.contains("status: 7"), "{}", r.content);
    }

    #[tokio::test]
    async fn read_limited_drains_after_truncating() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let payload = vec![b'x'; MAX_STREAM_BYTES * 2];
        let writer_task = tokio::spawn(async move {
            writer.write_all(&payload).await.unwrap();
        });

        let (out, total, truncated) = read_limited(reader, MAX_STREAM_BYTES).await.unwrap();

        assert!(truncated);
        assert_eq!(total, MAX_STREAM_BYTES * 2);
        // 头尾各半保留 + 中段省略标记：总长度必须落在预算附近，且不能吞掉尾部。
        assert!(out.len() < MAX_STREAM_BYTES + 200, "{}", out.len());
        assert!(out.contains("bytes omitted"), "{}", out);
        tokio::time::timeout(Duration::from_secs(1), writer_task)
            .await
            .expect("reader should keep draining so writer can finish")
            .unwrap();
    }

    #[tokio::test]
    async fn read_limited_keeps_tail_for_tracebacks() {
        // 保头截断会切掉尾部 traceback；头尾截断必须保住它。
        let (mut writer, reader) = tokio::io::duplex(1024);
        let mut payload = vec![b'.'; MAX_STREAM_BYTES * 2];
        payload.extend_from_slice(b"\nTraceback (most recent call last): boom");
        let writer_task = tokio::spawn(async move {
            writer.write_all(&payload).await.unwrap();
        });

        let (out, _total, truncated) = read_limited(reader, MAX_STREAM_BYTES).await.unwrap();
        assert!(truncated);
        assert!(
            out.ends_with("Traceback (most recent call last): boom"),
            "{}",
            &out[out.len().saturating_sub(200)..]
        );
        tokio::time::timeout(Duration::from_secs(1), writer_task)
            .await
            .expect("reader should keep draining so writer can finish")
            .unwrap();
    }

    #[tokio::test]
    async fn background_shell_returns_job_id_and_poll_reports_completion() {
        let b = base();
        let shell = Shell::new(b.clone());
        let poll =
            crate::shell_jobs::ShellPoll::with_table(shell.job_table(), b.join(".shell-jobs"));
        let command = "echo bg-output";

        let started = shell
            .execute(serde_json::json!({ "command": command, "background": true }))
            .await;
        assert!(!started.is_error, "{}", started.content);
        assert!(
            started.content.contains("status: started"),
            "{}",
            started.content
        );
        let job_id = started
            .content
            .lines()
            .find_map(|line| line.strip_prefix("job_id: "))
            .expect("启动结果应包含 job_id 行")
            .trim()
            .to_string();
        assert!(job_id.starts_with("job-"), "{}", job_id);

        // 轮询直到终态（最多 ~10s）。
        let mut final_poll: Option<ToolResult> = None;
        for _ in 0..100 {
            let p = poll.execute(serde_json::json!({ "job_id": job_id })).await;
            if !p.content.contains("status: running") {
                final_poll = Some(p);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let p = final_poll.expect("后台任务应在超时前结束");
        assert!(!p.is_error, "{}", p.content);
        assert!(p.content.contains("status: 0"), "{}", p.content);
        assert!(p.content.contains("bg-output"), "{}", p.content);

        // 终态轮询已把任务移出进程表；文件仍在，可继续查询（untracked）。
        let again = poll.execute(serde_json::json!({ "job_id": job_id })).await;
        assert!(!again.is_error, "{}", again.content);
        assert!(
            again.content.contains("unknown_untracked"),
            "{}",
            again.content
        );

        // 未知 job_id → 报错。
        let missing = poll
            .execute(serde_json::json!({ "job_id": "job-does-not-exist" }))
            .await;
        assert!(missing.is_error, "{}", missing.content);
    }
}
