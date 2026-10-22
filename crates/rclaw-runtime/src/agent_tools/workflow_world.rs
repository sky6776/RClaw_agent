//! 工作流 facade 的真实世界访问：files / git / world.run。
//!
//! 安全边界与 `read` 工具一致：所有路径先解析到 `project_root` 下（symlink/junction
//! 逃逸拒绝）；`world.run` 仅 `Unrestricted`，cwd 必须在项目根内，没有逐命令审批
//! （与 `shell` 同权，见文档 §3.7 / D10；P5 的 `set_command_confirm_hook` 走同一入口）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use rclaw_core::ToolAccess;

use super::workflow_paths::resolve_under;

const FILES_READ_LIMIT: usize = 2 * 1024 * 1024;
const FILES_LIST_LIMIT: usize = 2000;
/// glob / grep 遍历的目录项上限：结果条数有上限挡不住“没几个命中但目录树极大”的遍历。
const FILES_WALK_LIMIT: usize = 200_000;
const GREP_TOTAL_LIMIT: usize = 256 * 1024;
const GIT_DIFF_LIMIT: usize = 1024 * 1024;
const WORLD_OUTPUT_LIMIT: usize = 1024 * 1024;

static COMMAND_CONFIRM_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// P5：启用 `world.run` 逐命令确认（无确认钩子时拒绝执行）。
pub fn set_command_confirm_enabled(enabled: bool) {
    COMMAND_CONFIRM_ENABLED.store(enabled, std::sync::atomic::Ordering::SeqCst);
}

/// `files.glob(pattern)`。
pub fn files_glob(root: &Path, pattern: &str) -> Result<Value, String> {
    if pattern.trim().is_empty() {
        return Err("files.glob 需要 pattern".to_string());
    }
    let matcher = globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .map_err(|error| format!("glob pattern 非法: {error}"))?
        .compile_matcher();
    let mut matches = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".git" && entry.file_name() != "node_modules")
        .take(FILES_WALK_LIMIT)
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if matcher.is_match(&relative) {
            matches.push(relative);
            if matches.len() >= FILES_LIST_LIMIT {
                break;
            }
        }
    }
    matches.sort();
    Ok(json!(matches))
}

/// `files.read(path)`：UTF-8，≤2MB；拒绝目录与越界。
pub fn files_read(root: &Path, path: &str) -> Result<Value, String> {
    let resolved = resolve_under(root, path, true)?;
    let metadata = std::fs::metadata(&resolved)
        .map_err(|error| format!("files.read 无法访问 {}: {error}", resolved.display()))?;
    if !metadata.is_file() {
        return Err(format!("files.read 目标不是文件: {path}"));
    }
    if metadata.len() > FILES_READ_LIMIT as u64 {
        return Err(format!(
            "files.read 超过 {} 字节上限: {path}",
            FILES_READ_LIMIT
        ));
    }
    let bytes =
        std::fs::read(&resolved).map_err(|error| format!("files.read 读取失败: {error}"))?;
    let text =
        String::from_utf8(bytes).map_err(|_| format!("files.read 目标不是 UTF-8: {path}"))?;
    Ok(json!(text))
}

/// `files.grep(pattern, glob?)`：项目根内正则搜索，≤2000 条 / 256KB。
pub fn files_grep(root: &Path, pattern: &str, glob: Option<&str>) -> Result<Value, String> {
    let regex = regex::Regex::new(pattern).map_err(|error| format!("grep 正则非法: {error}"))?;
    let matcher = match glob.filter(|value| !value.trim().is_empty()) {
        Some(pattern) => Some(
            globset::GlobBuilder::new(pattern)
                .literal_separator(false)
                .build()
                .map_err(|error| format!("grep glob 非法: {error}"))?
                .compile_matcher(),
        ),
        None => None,
    };
    let mut out = Vec::new();
    let mut bytes = 0usize;
    'outer: for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".git" && entry.file_name() != "node_modules")
        .take(FILES_WALK_LIMIT)
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let relative_text = relative.to_string_lossy().replace('\\', "/");
        if let Some(matcher) = &matcher {
            if !matcher.is_match(&relative_text) {
                continue;
            }
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.len() > FILES_READ_LIMIT as u64 {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for (line_index, line) in content.lines().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            let text = if line.chars().count() > 500 {
                line.chars().take(500).collect::<String>()
            } else {
                line.to_string()
            };
            bytes += text.len() + relative_text.len() + 32;
            out.push(json!({
                "path": relative_text,
                "line": line_index + 1,
                "text": text,
            }));
            if out.len() >= FILES_LIST_LIMIT || bytes >= GREP_TOTAL_LIMIT {
                break 'outer;
            }
        }
    }
    Ok(json!(out))
}

/// 只读 git 命令的墙钟上限：挂住的凭据助手 / 锁住的 index 不能把引擎线程无限期占住。
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

fn run_git(root: &Path, args: &[&str], limit: usize) -> Result<String, String> {
    let mut command = std::process::Command::new("git");
    command.args(args).current_dir(root);
    // 关掉一切可能停下来等输入的路径：终端提示、交互式凭据、分页器。
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "echo")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat");
    let output = super::workflow::run_with_timeout(command, GIT_TIMEOUT).ok_or_else(|| {
        format!(
            "git {} 超过 {}s 未结束或无法启动，已终止",
            args.join(" "),
            GIT_TIMEOUT.as_secs()
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {} 失败: {}", args.join(" "), stderr.trim()));
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(if text.len() > limit {
        text.chars().take(limit).collect()
    } else {
        text
    })
}

/// `git.status()`：只读。
pub fn git_status(root: &Path) -> Result<Value, String> {
    let text = run_git(
        root,
        &["status", "--porcelain=v1", "-b", "--", "."],
        256 * 1024,
    )?;
    let mut branch = None;
    let mut staged = Vec::new();
    let mut modified = Vec::new();
    let mut untracked = Vec::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("## ") {
            branch = Some(name.split("...").next().unwrap_or(name).trim().to_string());
            continue;
        }
        if line.len() < 3 {
            continue;
        }
        let index = line.as_bytes()[0] as char;
        let worktree = line.as_bytes()[1] as char;
        let path = line[3..].to_string();
        if index != ' ' && index != '?' {
            staged.push(path.clone());
        }
        if worktree == '?' {
            untracked.push(path.clone());
        } else if worktree != ' ' {
            modified.push(path);
        }
    }
    Ok(json!({
        "branch": branch,
        "clean": staged.is_empty() && modified.is_empty() && untracked.is_empty(),
        "staged": staged,
        "modified": modified,
        "untracked": untracked,
    }))
}

/// `git.diff(path?)`：只读，≤1MB。
pub fn git_diff(root: &Path, path: Option<&str>) -> Result<Value, String> {
    let mut args = vec!["diff", "--no-color", "--"];
    let resolved = match path.filter(|value| !value.trim().is_empty()) {
        Some(path) => {
            let resolved = resolve_under(root, path, true)?;
            let relative = resolved
                .strip_prefix(root)
                .map(|value| value.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.to_string());
            Some(relative)
        }
        None => None,
    };
    if let Some(relative) = &resolved {
        args.push(relative);
    } else {
        // 只 diff 项目子树，避免 Layout.base 位于更大仓库时泄露兄弟目录。
        args.push(".");
    }
    run_git(root, &args, GIT_DIFF_LIMIT).map(|text| json!(text))
}

/// `world.run` 的结果（只有确认进程树已退出才会构造）。
pub struct WorldOutcome {
    pub value: Value,
    pub process_id: String,
}

/// 执行命令并持有整个进程树；取消 / 超时先 TERM 后 KILL，只有确认退出才返回 Ok。
///
/// `on_start` 在子进程启动后立即调用（写 `workflow_process` 行）；
/// 失败路径返回的错误以 `world_run_unknown:` 开头表示无法确认进程树已退出。
pub async fn world_run(
    root: &Path,
    tool_access: ToolAccess,
    command: &str,
    opts: &Value,
    cancel: &CancellationToken,
    timeout: Duration,
    on_start: impl FnOnce(&str, u32),
) -> Result<WorldOutcome, String> {
    if tool_access != ToolAccess::Unrestricted {
        return Err("forbidden_world: world.run 仅 Unrestricted 会话可用".to_string());
    }
    let cwd = match opts.get("cwd").and_then(Value::as_str) {
        Some(cwd) if !cwd.trim().is_empty() => resolve_under(root, cwd, true)?,
        _ => root.to_path_buf(),
    };
    if !cwd.is_dir() {
        return Err("world.run 的 cwd 不是目录".to_string());
    }
    if COMMAND_CONFIRM_ENABLED.load(std::sync::atomic::Ordering::SeqCst)
        && !rclaw_tools::access::confirm_command(command, &cwd).await
    {
        return Err("command_not_confirmed: world.run 需要用户逐命令确认".to_string());
    }
    let mut command_builder = shell_command(command);
    command_builder
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command_builder.as_std_mut().process_group(0);
    }

    let mut child = command_builder
        .spawn()
        .map_err(|error| format!("world.run 启动失败: {error}"))?;
    let root_pid = child.id().unwrap_or(0);
    let process_id = format!("pgid:{root_pid}");
    #[cfg(windows)]
    let job = windows_job::assign(&child)?;
    #[cfg(not(windows))]
    let job = ();
    on_start(&process_id, root_pid);

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut stdout_task = tokio::spawn(read_limited(stdout, WORLD_OUTPUT_LIMIT));
    let mut stderr_task = tokio::spawn(read_limited(stderr, WORLD_OUTPUT_LIMIT));

    let deadline = tokio::time::Instant::now() + timeout;
    let mut timed_out = false;
    let mut cancelled = false;
    let status = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break Some(status);
        }
        if cancel.is_cancelled() {
            cancelled = true;
            kill_tree(&mut child, root_pid, &job);
            break wait_for_exit(&mut child, Duration::from_secs(5)).await;
        }
        if tokio::time::Instant::now() >= deadline {
            timed_out = true;
            kill_tree(&mut child, root_pid, &job);
            break wait_for_exit(&mut child, Duration::from_secs(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    // 根进程已退出（或已被杀），但留下的后台孙进程若仍持有管道写端，reader 永远读不到
    // EOF：这里只等有限时间，超时就杀整棵树并放弃剩余输出，不能让 effect 永久挂起。
    let outputs = tokio::time::timeout(Duration::from_secs(3), async {
        (
            (&mut stdout_task).await.unwrap_or_default(),
            (&mut stderr_task).await.unwrap_or_default(),
        )
    })
    .await;
    let (stdout, stderr) = match outputs {
        Ok(pair) => pair,
        Err(_elapsed) => {
            stdout_task.abort();
            stderr_task.abort();
            kill_tree(&mut child, root_pid, &job);
            (
                String::new(),
                "[world.run] 命令已退出但仍有子进程占用输出管道，已终止整棵进程树并放弃其输出"
                    .to_string(),
            )
        }
    };
    // Job 句柄关闭 = 整棵树由 OS 回收（KILL_ON_JOB_CLOSE）；Unix 已在 kill_tree 里整组信号。
    #[cfg(windows)]
    drop(job);
    let Some(status) = status else {
        return Err("world_run_unknown: 无法确认命令进程树退出".to_string());
    };
    if cancelled {
        return Err("world_run_unknown: world.run 已取消（无法证明命令没有写入）".to_string());
    }
    let exit_code = status.code().unwrap_or(-1);
    Ok(WorldOutcome {
        value: json!({
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
            "timed_out": timed_out,
        }),
        process_id,
    })
}

async fn wait_for_exit(
    child: &mut tokio::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn read_limited<R: tokio::io::AsyncRead + Unpin>(reader: Option<R>, limit: usize) -> String {
    let Some(mut reader) = reader else {
        return String::new();
    };
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if buffer.len() >= limit {
                    continue; // 继续读空管道，避免子进程阻塞
                }
                let take = read.min(limit - buffer.len());
                buffer.extend_from_slice(&chunk[..take]);
            }
        }
    }
    String::from_utf8_lossy(&buffer).to_string()
}

#[cfg(unix)]
fn kill_tree(child: &mut tokio::process::Child, root_pid: u32, _job: &()) {
    if root_pid == 0 {
        let _ = child.start_kill();
        return;
    }
    unsafe {
        libc::kill(-(root_pid as i32), libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(300));
    unsafe {
        libc::kill(-(root_pid as i32), libc::SIGKILL);
    }
}

#[cfg(windows)]
fn kill_tree(child: &mut tokio::process::Child, _root_pid: u32, job: &windows_job::Job) {
    // 显式终止整棵树；KILL_ON_JOB_CLOSE 仍作为宿主崩溃兜底。
    windows_job::terminate(job);
    let _ = child.start_kill();
}

#[cfg(windows)]
mod windows_job {
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct Job(windows_sys::Win32::Foundation::HANDLE);

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub fn assign(child: &tokio::process::Child) -> Result<Job, String> {
        let handle = child
            .raw_handle()
            .ok_or_else(|| "world.run 缺少子进程句柄".to_string())?;
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err("world.run 创建 Job Object 失败".to_string());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                CloseHandle(job);
                return Err("world.run 配置 Job Object 失败".to_string());
            }
            if AssignProcessToJobObject(job, handle as windows_sys::Win32::Foundation::HANDLE) == 0
            {
                CloseHandle(job);
                return Err("world.run 把进程加入 Job Object 失败".to_string());
            }
            Ok(Job(job))
        }
    }

    pub fn terminate(job: &Job) {
        unsafe {
            TerminateJobObject(job.0, 1);
        }
    }
}

#[cfg(windows)]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.arg("/D")
        .arg("/S")
        .arg("/C")
        .raw_arg(format!("\"{command}\""));
    cmd.env("PYTHONIOENCODING", "utf-8").env("PYTHONUTF8", "1");
    cmd
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> tokio::process::Command {
    let apk_host = std::env::var("RCLAW_ANDROID").is_ok_and(|value| value.trim() == "1");
    if apk_host {
        let mut cmd = tokio::process::Command::new("/system/bin/sh");
        cmd.arg("-c").arg(command);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-lc").arg(command);
        cmd
    }
}

/// `artifact` 的落盘目标（受管理根，不接受模型拼接的绝对路径）。
pub fn artifact_target(root: &Path, run_id: &str, id: &str) -> Result<PathBuf, String> {
    let safe_id: String = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    // 全点号的 id（`.` / `..`）会把 `v<N>.<ext>` 写到 run 目录之外的共享目录里。
    if safe_id.is_empty() || safe_id.len() > 64 || safe_id.chars().all(|ch| ch == '.') {
        return Err("artifact id 非法（1-64 字符，[A-Za-z0-9._-]，不能全是点号）".to_string());
    }
    Ok(root.join("workflow-artifacts").join(run_id).join(safe_id))
}
