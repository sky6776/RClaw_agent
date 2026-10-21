//! 日志装配：根据 `[log]` 段初始化 tracing，控制台 / 文件输出独立开关。
//!
//! - 控制台等级由 `[log].console_level`、文件等级由 `[log].file_level` 各自控制（互不影响）；
//!   `RUST_LOG` 若设置则【同时覆盖】两者（运维逃生通道）。
//! - `console` / `file` 两个开关互相独立：可只控制台、只文件、都开、都关。
//! - 文件输出按本地日期轮转（`dir/<file_name>.YYYY-MM-DD`），用 std + chrono 实现，
//!   不依赖 tracing-appender（其 `time` crate 要求 Rust 1.88，会顶破本仓库 MSRV 1.80）。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tracing_subscriber::{
    filter::EnvFilter, fmt::MakeWriter, layer::SubscriberExt, util::SubscriberInitExt, Layer,
};

use crate::config::LogSection;

/// 根据 `[log]` 段装配 tracing：控制台与文件输出【独立】开关，且各自独立 level。
///
/// - `console_level` / `file_level`：经 [`parse_log_level`] 校验的等级（error/warn/info/debug/trace），
///   控制台与文件【各自独立】（如控制台 debug、文件 info）。`RUST_LOG` 若已设置则【同时覆盖】两者
///   （运维逃生通道）。
/// - `console=true` → stdout；`file=true` → `dir/file_name`（按日轮转，避免单文件无限增长）。
///
/// 实现上每个输出目标包成 `Option<Layer>`：`Option<L: Layer>` 自身实现 `Layer`，`None` 即空操作，
/// 故 `console` 与 `file` 两个开关天然独立、互不影响。两者都关闭则丢弃所有日志（合法「静默」模式）。
pub fn init_tracing(log: &LogSection, default_dir: &Path) -> Result<()> {
    // 始终校验配置中的两个 level（fail-fast）：即便日志全关，拼错的 level（如 "imfo"）也应在
    // 启动期暴露，而非被 EnvFilter 静默当作 target 指令吞掉（那样只会表现为「日志凭空消失」）。
    let console_level = parse_log_level(&log.console_level)?;
    let file_level = parse_log_level(&log.file_level)?;

    // 控制台与文件都关闭：不装配任何 layer，丢弃所有日志。
    if !(log.console || log.file) {
        // 用 try_init 而非 init：全局订阅者每进程只能设置一次，`init()` 第二次调用会
        // panic（桌面/APK 装配失败后「改配置再来」会再次走到这里，panic 发生在 tauri
        // 任务里 → 命令永不返回、前端永久卡在「正在启动」）。已装配过视为成功。
        let _ = tracing_subscriber::registry().try_init();
        return Ok(());
    }

    // RUST_LOG 优先；否则用各自配置 level。控制台与文件【各自独立】 filter：
    // RUST_LOG 是全局「运维逃生通道」——设置后同时覆盖两者。
    let console_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(console_level));
    let file_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(file_level));

    // 控制台 layer：stdout。
    let console_layer = log.console.then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stdout)
            .with_filter(console_filter)
    });

    // 文件 layer：按日轮转，写入 dir/file_name.YYYY-MM-DD。目录不存在则创建。
    let file_layer = if log.file {
        // 留空（默认）则落入 agent/logs（与 config/skills 平级）；否则尊重用户配置。
        let dir = if log.dir.trim().is_empty() {
            default_dir.to_string_lossy().into_owned()
        } else {
            log.dir.clone()
        };
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("创建日志目录 '{dir}' 失败（[log].dir）"))?;
        let writer = DailyFileWriter::open(Path::new(&dir), &log.file_name).with_context(|| {
            format!("打开日志文件失败（dir={dir}, file_name={}）", log.file_name)
        })?;
        Some(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                // 文件日志关闭 ANSI 着色：避免转义码（\x1b[..m）污染日志文件、干扰 grep / 阅读。
                // 控制台层保留默认 ANSI（彩色输出，与历史行为一致）。
                .with_ansi(false)
                .with_filter(file_filter),
        )
    } else {
        None
    };

    // 同上：已存在全局订阅者时 try_init 返回 Err，沿用既有订阅者即可，不能 panic。
    if tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .try_init()
        .is_err()
    {
        tracing::debug!("tracing 全局订阅者已存在，沿用既有配置");
    }
    Ok(())
}

/// 校验并规范化日志等级字符串。
///
/// 仅接受 error/warn/info/debug/trace（大小写不敏感、自动 trim），返回归一化的小写常量。
/// 用于 `[log].level` 的启动期 fail-fast：拼错（如 `"imfo"`）会在启动期直接报错，而非被
/// `EnvFilter` 静默当作 target 指令忽略——后者只会表现为「所有日志凭空消失」，极难排查。
pub fn parse_log_level(level: &str) -> Result<&'static str> {
    match level.trim().to_ascii_lowercase().as_str() {
        "error" => Ok("error"),
        "warn" => Ok("warn"),
        "info" => Ok("info"),
        "debug" => Ok("debug"),
        "trace" => Ok("trace"),
        other => anyhow::bail!(
            "[log] level 值 '{other}' 不识别（支持 error / warn / info / debug / trace）"
        ),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 文件日志 writer
// ────────────────────────────────────────────────────────────────────────────
// 替代 tracing-appender：后者经 `time` crate 拉入需 Rust 1.88 的依赖，会顶破本仓库声明
// 的 MSRV 1.80。本实现只用 std + 已在依赖树中的 chrono，按本地日期轮转：
// 写入 `dir/<file_name>.YYYY-MM-DD`，跨日自动切换到新文件。写入经 Mutex 串行化，
// 多 tokio task 共享安全。
// ────────────────────────────────────────────────────────────────────────────

/// 以追加模式打开日志文件（不存在则创建）。Windows 上额外允许文件在被持有期间被删除
/// （`FILE_SHARE_DELETE`），使 `/log del` 能删除当日正在写入的日志；删除后由
/// [`DailyFileWriter::reopen_if_missing`] 在下一次写入前重建文件名。
fn open_log_file(path: &Path) -> std::io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Win32 文件共享位：默认 READ|WRITE 之上叠加 DELETE，允许其它句柄删除本文件。
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_SHARE_DELETE: u32 = 0x4;
        OpenOptions::new()
            .create(true)
            .append(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        OpenOptions::new().create(true).append(true).open(path)
    }
}

#[derive(Clone)]
struct DailyFileWriter {
    inner: Arc<Mutex<DailyFileInner>>,
}

struct DailyFileInner {
    dir: PathBuf,
    file_name: String,
    /// 当前文件对应的本地日期 "YYYY-MM-DD"，用于跨日检测。
    date: String,
    file: File,
}

impl DailyFileWriter {
    /// 打开（或创建）当日日志文件 `dir/<file_name>.YYYY-MM-DD`（追加模式）。目录须已存在。
    fn open(dir: &Path, file_name: &str) -> std::io::Result<Self> {
        let date = today_local();
        let path = dir.join(format!("{file_name}.{date}"));
        let file = open_log_file(&path)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(DailyFileInner {
                dir: dir.to_path_buf(),
                file_name: file_name.to_string(),
                date,
                file,
            })),
        })
    }

    /// 跨日（本地时区）则切换到新文件。best-effort：新文件打开失败时沿用旧文件，
    /// 优先保证日志不丢，而非严格轮转。
    fn rotate_if_new_day(&self) {
        let today = today_local();
        let Ok(mut g) = self.inner.lock() else {
            return; // mutex 中毒：跳过轮转
        };
        if g.date == today {
            return;
        }
        let path = g.dir.join(format!("{}.{today}", g.file_name));
        if let Ok(new_file) = open_log_file(&path) {
            g.date = today;
            g.file = new_file;
        }
    }

    /// 当日志文件在运行期被删除（如 `/log del` 或外部删除）时，下一次日志事件会重建它。
    ///
    /// 写入端持有的句柄不会自动发现底层文件已被删除：删除后旧句柄仍可写（数据落到已解链的
    /// inode；Windows 上因 `FILE_SHARE_DELETE` 允许删除而同样失联），但不会重新创建文件名——
    /// 表现为「删了日志后再也不出新文件」。这里在每次取 writer 时检测当日文件是否存在，
    /// 缺失则用 [`open_log_file`]（OPEN_ALWAYS）重新创建。
    fn reopen_if_missing(&self) {
        let Ok(mut g) = self.inner.lock() else {
            return; // mutex 中毒：跳过重建
        };
        let path = g.dir.join(format!("{}.{}", g.file_name, today_local()));
        if path.exists() {
            return;
        }
        if let Ok(new_file) = open_log_file(&path) {
            g.file = new_file;
        }
    }
}

impl<'a> MakeWriter<'a> for DailyFileWriter {
    type Writer = DailyFileHandle;
    fn make_writer(&'a self) -> Self::Writer {
        self.rotate_if_new_day();
        self.reopen_if_missing();
        DailyFileHandle {
            inner: self.inner.clone(),
        }
    }
}

struct DailyFileHandle {
    inner: Arc<Mutex<DailyFileInner>>,
}

impl Write for DailyFileHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.inner.lock() {
            Ok(mut g) => g.file.write(buf),
            // mutex 中毒：丢弃本次写入并假装成功，避免 panic 经 tracing 拖垮整个进程。
            Err(_) => Ok(buf.len()),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self.inner.lock() {
            Ok(mut g) => g.file.flush(),
            Err(_) => Ok(()),
        }
    }
}

/// 当前本地日期，格式 "YYYY-MM-DD"。
fn today_local() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_log_level_accepts_known_levels() {
        // 大小写不敏感 + 自动 trim。
        for lvl in ["error", "WARN", " Info ", "debug", "trace"] {
            assert!(parse_log_level(lvl).is_ok(), "{lvl:?} 应为合法等级");
        }
        // 归一化输出。
        assert_eq!(parse_log_level(" WARN ").unwrap(), "warn");
    }

    #[test]
    fn parse_log_level_rejects_unknown() {
        // fail-fast：拼错的 level 不应被当作 target 指令静默吞掉。
        assert!(parse_log_level("imfo").is_err());
        assert!(parse_log_level("").is_err());
        assert!(parse_log_level("verbose").is_err());
    }

    #[test]
    fn daily_file_writer_writes_dated_file_and_appends() {
        // 直接验证 DailyFileWriter 的核心写路径（不经全局 subscriber）：
        //  - 文件名含当日日期 `test.log.YYYY-MM-DD`；
        //  - 经 handle 写入的内容正确落盘；
        //  - 再次写入为追加（append 语义），不覆盖。
        use std::io::Write as IoWrite;
        use tracing_subscriber::fmt::MakeWriter as _;

        let dir = std::env::temp_dir().join("rclaw_daily_writer_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let today = today_local();
        let writer = DailyFileWriter::open(&dir, "test.log").unwrap();

        let mut h = writer.make_writer();
        h.write_all(b"hello\n").unwrap();
        h.flush().unwrap();
        drop(h);

        let path = dir.join(format!("test.log.{today}"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");

        // 同一 writer 再写一次 → 追加（append 模式打开，不截断）。
        let mut h2 = writer.make_writer();
        h2.write_all(b"world\n").unwrap();
        h2.flush().unwrap();
        drop(h2);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nworld\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daily_file_writer_recreates_after_deletion() {
        // 修复「运行期删除日志后无法重新生成」：删除当日文件后，下一次 make_writer
        // 应通过 reopen_if_missing 重建文件。
        use std::io::Write as IoWrite;
        use tracing_subscriber::fmt::MakeWriter as _;

        let dir = std::env::temp_dir().join("rclaw_daily_writer_regen_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let today = today_local();
        let writer = DailyFileWriter::open(&dir, "test.log").unwrap();
        let path = dir.join(format!("test.log.{today}"));

        let mut h = writer.make_writer();
        h.write_all(b"first\n").unwrap();
        h.flush().unwrap();
        drop(h);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");

        // 模拟运行期删除日志（writer 仍持有旧句柄）。
        std::fs::remove_file(&path).unwrap();
        assert!(!path.exists());

        // 下一次写入应触发 reopen_if_missing 重建文件（旧句柄写入已解链文件被丢弃）。
        let mut h2 = writer.make_writer();
        h2.write_all(b"second\n").unwrap();
        h2.flush().unwrap();
        drop(h2);
        assert!(path.exists(), "删除后日志应被重新生成");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second\n");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
