//! stdio 传输：spawn 子进程，stdin/stdout + 阻塞读线程。迁自原 client.rs。
//!
//! - [`StdioTransport::spawn`] 起子进程与读线程；读线程逐行读 stdout，调
//!   [`Core::route_inbound`](super::super::jsonrpc::Core::route_inbound) 路由，server 主动请求的
//!   回写经 stdin 回送。
//! - [`StdioTransport::send_raw`] 写子进程 stdin（`spawn_blocking`，避免阻塞 runtime）。
//! - `close_sync` 关 stdin + 宽限后 kill（`McpClient::drop` 调用）。
//!
//! 每个 server 一条独立 OS 读线程，阻塞读自己的 stdout 管道，不持任何跨 server 锁——
//! 多个 stdio server 完全并发互不阻塞。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tracing::{debug, error, warn};

use super::super::jsonrpc::{summarize_text, Core};
use super::super::{McpError, Result as McpResult};
use super::McpTransport;

/// Drop 时给 server 自行退出的宽限时间：关闭 stdin 后最多等这么久，再强杀。
const SHUTDOWN_GRACE: Duration = Duration::from_millis(2000);

/// 自身进程是否附着了控制台（CLI 为 true；GUI 子系统桌面版为 false）。
#[cfg(windows)]
fn has_attached_console() -> bool {
    // SAFETY: 纯查询，无副作用；返回的伪句柄不需关闭。
    unsafe { !windows_sys::Win32::System::Console::GetConsoleWindow().is_null() }
}

pub struct StdioTransport {
    name: String,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    alive: Arc<AtomicBool>,
    child: Arc<Mutex<Child>>,
}

impl StdioTransport {
    /// spawn 子进程 + 读线程。
    ///
    /// - `core`：协议核心的弱引用，读线程 upgrade 后回放消息 / 标记失效。
    /// - `alive`：与 [`Core`] 共享的存活标志（同一把 `Arc<AtomicBool>`），读线程在 stdout
    ///   关闭时置 false。
    pub fn spawn(
        core: Weak<Core>,
        alive: Arc<AtomicBool>,
        name: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: Option<&str>,
    ) -> McpResult<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        // GUI 子系统宿主（桌面版）没有控制台，直接 spawn 控制台子进程（如
        // gbrain.exe serve）会让 Windows 新弹一个控制台窗口；CREATE_NO_WINDOW
        // 让 server 静默后台运行。CLI（自带控制台）不加：子进程继承控制台，
        // stderr 日志仍直接可见。
        #[cfg(windows)]
        if !has_attached_console() {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        for (k, v) in env {
            cmd.env(k, v);
        }
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::Tool(format!("MCP[{name}]: 启动 '{command}' 失败: {e}")))?;

        let child_stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(McpError::Tool(format!("MCP[{name}]: 无 stdin")));
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(McpError::Tool(format!("MCP[{name}]: 无 stdout")));
            }
        };

        let stdin: Arc<Mutex<Option<ChildStdin>>> = Arc::new(Mutex::new(Some(child_stdin)));
        let child = Arc::new(Mutex::new(child));

        let stdin_clone = Arc::clone(&stdin);
        let alive_clone = Arc::clone(&alive);
        let name_owned = name.to_string();
        if let Err(e) = std::thread::Builder::new()
            .name(format!("mcp-reader-{name}"))
            .spawn(move || Self::reader_thread(stdout, core, stdin_clone, alive_clone, name_owned))
        {
            if let Ok(mut g) = stdin.lock() {
                *g = None;
            }
            if let Ok(mut child) = child.lock() {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Err(McpError::Tool(format!("MCP[{name}]: 启动读线程失败: {e}")));
        }

        Ok(Self {
            name: name.to_string(),
            stdin,
            alive,
            child,
        })
    }

    /// 读线程主体：逐行读 stdout，按 JSON-RPC 语义分类并分发。
    fn reader_thread(
        stdout: ChildStdout,
        core: Weak<Core>,
        stdin: Arc<Mutex<Option<ChildStdin>>>,
        alive: Arc<AtomicBool>,
        server_name: String,
    ) {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();

        // 回写闭包：锁 stdin、若仍打开则写入一行。失败静默（best-effort）。
        let write_back = |line: &str| {
            let Ok(mut g) = stdin.lock() else {
                return;
            };
            if let Some(s) = g.as_mut() {
                let _ = s.write_all(line.as_bytes());
                let _ = s.write_all(b"\n");
                let _ = s.flush();
            }
        };

        // 单行上限：server 往 stdout 写一条极长的无换行数据（超大结果、调试输出、二进制噪声）
        // 时 `read_until` 会把整行无上限地攒进内存，Android 上一次 OOM 即整进程被杀。
        // 用 `take(MAX+1)` 限定单次读取量；超限按协议违规处理：跳出循环 → 置失效 +
        // 唤醒全部等待方（与 stdout 关闭同一收尾路径）。
        const MAX_LINE_BYTES: u64 = 16 * 1024 * 1024;
        loop {
            buf.clear();
            match (&mut reader)
                .take(MAX_LINE_BYTES + 1)
                .read_until(b'\n', &mut buf)
            {
                Ok(0) => break,
                Ok(_) if buf.len() as u64 > MAX_LINE_BYTES => {
                    error!(
                        server = %server_name,
                        "MCP: stdout 单行超过 {MAX_LINE_BYTES} 字节上限，连接标记为失效"
                    );
                    break;
                }
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf).trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    debug!(
                        server = %server_name,
                        "MCP ← {}",
                        summarize_text(&line, 200)
                    );
                    if let Some(core) = core.upgrade() {
                        let mut replies = Vec::new();
                        core.route_inbound(&line, &mut replies);
                        for r in replies {
                            write_back(&r);
                        }
                    }
                }
                Err(e) => {
                    error!(server = %server_name, "MCP: 读取错误: {e}");
                    break;
                }
            }
        }

        // stdout 关闭：置失效并唤醒所有等待中的调用。
        alive.store(false, Ordering::SeqCst);
        error!(server = %server_name, "MCP: stdout 已关闭，连接标记为失效");
        if let Some(core) = core.upgrade() {
            core.fail_all_pending("MCP server stdout closed");
        }
    }

    /// 同步关闭：关 stdin（发 EOF）→ 宽限等待 → 强杀。供 `McpClient::drop` 调用。
    pub fn close_sync(&self) {
        // 用 try_lock：若 send_raw 的阻塞写线程正卡在写满的管道上（server 不读 stdin），
        // 它会一直持有这把锁，这里 lock() 会把关闭方（含 runtime 线程上的 drop）一起挂死。
        // 拿不到锁就跳过优雅 EOF，直接走下面的 kill——子进程一死管道断开，卡住的写线程
        // 会以 broken pipe 出错返回并释放锁。
        match self.stdin.try_lock() {
            Ok(mut g) => {
                *g = None; // 丢弃 ChildStdin → 关闭管道，server 读到 EOF。
            }
            Err(std::sync::TryLockError::Poisoned(p)) => {
                *p.into_inner() = None;
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                warn!(
                    server = %self.name,
                    "MCP stdin 仍被阻塞写入占用，跳过优雅关闭直接终止子进程"
                );
            }
        }
        if let Ok(mut child) = self.child.lock() {
            let deadline = Instant::now() + SHUTDOWN_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
            warn!(
                server = %self.name,
                grace_ms = SHUTDOWN_GRACE.as_millis() as u64,
                "MCP server 未在宽限期内退出，强制终止"
            );
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.close_sync();
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn send_raw(&self, line: &str) -> McpResult<()> {
        let stdin = Arc::clone(&self.stdin);
        let server_name = self.name.clone();
        let line = line.to_string();

        tokio::task::spawn_blocking(move || -> McpResult<()> {
            let mut guard = stdin
                .lock()
                .map_err(|_| McpError::Tool(format!("MCP[{server_name}]: stdin 锁中毒")))?;
            let stdin = guard.as_mut().ok_or_else(|| {
                McpError::Tool(format!("MCP[{server_name}]: stdin 已关闭，无法写入"))
            })?;
            stdin
                .write_all(line.as_bytes())
                .map_err(|e| McpError::Tool(format!("MCP[{server_name}]: 写入错误: {e}")))?;
            stdin
                .write_all(b"\n")
                .map_err(|e| McpError::Tool(format!("MCP[{server_name}]: 写入错误: {e}")))?;
            stdin
                .flush()
                .map_err(|e| McpError::Tool(format!("MCP[{server_name}]: flush 错误: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| McpError::Tool(format!("MCP[{}]: 写入任务失败: {e}", self.name)))?
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    async fn shutdown(&self) {
        self.close_sync();
    }

    fn shutdown_blocking(&self) {
        self.close_sync();
    }
}
