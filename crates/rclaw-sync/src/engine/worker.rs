//! 引擎线程：一条专用的系统线程，上面是单线程的 tokio 运行时。
//!
//! 它是一个 actor：命令（宿主的通知、面板上的操作）、定时器（定时轮询、本机变更检查、
//! 约好的下一轮）依次处理，两轮同步不会同时进行，命令排在两轮之间执行。附件的上传下载
//! 是同一个运行时上的两个本地任务（[`super::transfer`]），在轮次等网络的间隙里穿插。
//!
//! 一个数据目录同一时刻只有一个进程做同步：靠 `sync/engine.lock` 上的文件锁。拿不到锁
//! 的进程处于 `standby`，隔一阵再试。

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rclaw_store::sync_db;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::round::{Outcome, Session};
use super::{local, lock, space, transfer, Shared, Trigger, PULL_FRESH_MS};
use crate::attach::{self, now_ms, Backlog};
use crate::config::{SyncConfig, SyncPaths};
use crate::db;
use crate::error::{Result, SyncError, TransportError};
use crate::files;
use crate::merge;
use crate::secrets::Secrets;
use crate::{blocking, SyncEvent};

/// 本机变更检查的间隔（§7.1）。后台放宽。
const TICK: Duration = Duration::from_secs(5);
const TICK_BACKGROUND: Duration = Duration::from_secs(30);
/// 属性变更、删除之后隔这么久发布（连着改几样时合成一轮）。
const LOCAL_CHANGE_DELAY: Duration = Duration::from_millis(300);
/// 只拉取的一轮有了收获，隔这么久补一轮完整的。
const FOLLOW_UP_DELAY: Duration = Duration::from_millis(300);
/// 出错之后的重试间隔：从 `RETRY_MIN` 起逐次翻倍，到 `RETRY_MAX` 为止。
const RETRY_MIN: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(300);
/// 换存储设置时等待在途提交的轮数上限：超过就升级成可见错误，不再静默重试。
const SWITCH_GIVE_UP_TRIES: u32 = 5;
/// 要用户动手才能好的错误（密码不对、空间被重置、要升级）：慢慢重试。
const RETRY_SLOW: Duration = Duration::from_secs(300);
/// 存储限流时最多等这么久。
const RATE_LIMIT_MAX: Duration = Duration::from_secs(900);
/// 等用户确认（计费网络下的首次同步）期间，隔这么久看一眼网络是不是换了。
const CONFIRM_RECHECK: Duration = Duration::from_secs(30);
/// 窗口重新获得焦点时，上次失败已经过了这么久就不再等退避，马上试一次。
const FOCUS_RETRY: Duration = Duration::from_secs(15);
/// 拿不到引擎锁：第一次很快再试（多半是上一个引擎线程还在收尾），之后慢慢试。
const STANDBY_FIRST: Duration = Duration::from_secs(2);
const STANDBY_RETRY: Duration = Duration::from_secs(30);
/// 本机这边没准备好（目录、数据库出错）：隔这么久再试。
const OPEN_RETRY: Duration = Duration::from_secs(30);
/// 拆任务时等已经开始的附件提交阶段结束最多这么久（换空间 / 清理之前）。等不到也往下
/// 走：代已经换了，它们的结果不会生效，落地文件的那一步也会自己收干净。
const TEARDOWN_SETTLE: Duration = Duration::from_secs(15);
/// 连着马上再来一轮最多这么多次，然后歇一会儿（大批量的首次同步照样走得完，出了
/// 「总觉得还有活」的毛病时也不至于空转）。
const RERUN_MAX: u32 = 50;
const RERUN_PAUSE: Duration = Duration::from_secs(30);
/// 打开会话、续聊之前，补齐最后这么多条消息引用的附件。
const TAIL_MESSAGES: usize = 30;

pub(crate) const STANDBY_HINT: &str = "另一个 RClaw 进程正在用这个数据目录做同步，本进程先不同步。\
     要在这里操作同步空间，请到那个进程里去，或者先退出它";

/// 没存完的存储设置（`sync/pending-storage.json`）：`rclaw.toml` 与 secrets 两个文件不
/// 能原子一起提交——保存先把 TOML 写出去，命令再把 secrets 写上。命令失败（worker
/// 待机、secrets 写失败、进程中断）时这条记录留下：下一次引擎打开先把它做完，两边才
/// 收敛到同一套配置，不会拿旧存储的凭据记录去试新目标。做完就删。与密钥同样的私有权限。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StorageIntent {
    /// 这次保存请求的标识：两次保存交错时，先完成的一方不能把后一个请求的意图删掉。
    #[serde(default)]
    id: String,
    config: SyncConfig,
    /// 要绑定的密码；`None` 或空 = 不改密码。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
}

fn storage_intent_path(paths: &SyncPaths) -> PathBuf {
    paths.root.join("pending-storage.json")
}

/// 存储保存协议的跨进程锁：意图、`rclaw.toml`、secrets 的整套提交必须串行——两次保存
/// 交错时会留下 TOML 与运行配置各一套、且没有恢复记录的状态。
pub struct StorageLock {
    file: std::fs::File,
}

impl StorageLock {
    fn lock_path(paths: &SyncPaths) -> PathBuf {
        paths.root.join("storage.lock")
    }

    /// 拿锁（阻塞到别的保存/恢复结束）。
    pub fn acquire(paths: &SyncPaths) -> Result<StorageLock> {
        paths.ensure_root()?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(Self::lock_path(paths))?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(StorageLock { file })
    }

    /// 试拿锁：别的地方正拿着就返回 `None`（不等待）。
    pub fn try_acquire(paths: &SyncPaths) -> Result<Option<StorageLock>> {
        paths.ensure_root()?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(Self::lock_path(paths))?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(StorageLock { file })),
            Err(error) if crate::lock_is_contended(&error) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for StorageLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// 落盘「存储设置更新意图」，返回它的标识。同一套配置与密码重复调用（桌面在写
/// `rclaw.toml` 之前先落一次、引擎派发命令时再落一次）沿用手上这条记录的标识，不会
/// 出现两条意图互相覆盖。
pub fn storage_intent_save(
    paths: &SyncPaths,
    config: &SyncConfig,
    secret: Option<&str>,
) -> Result<String> {
    let secret = secret.map(|secret| secret.to_string());
    if let Some(existing) = storage_intent_load(paths)? {
        if !existing.id.is_empty()
            && existing.config == *config
            && existing.secret == secret
        {
            return Ok(existing.id);
        }
    }
    let intent = StorageIntent {
        id: crate::ids::random_hex::<8>(),
        config: config.clone(),
        secret,
    };
    paths.ensure_root()?;
    rclaw_store::atomic_write_private(
        &storage_intent_path(paths),
        &serde_json::to_vec_pretty(&intent)?,
    )?;
    Ok(intent.id)
}

fn storage_intent_load(paths: &SyncPaths) -> Result<Option<StorageIntent>> {
    match std::fs::read(storage_intent_path(paths)) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(intent) => Ok(Some(intent)),
            Err(error) => {
                tracing::warn!("sync/pending-storage.json 损坏，丢弃: {error}");
                let _ = std::fs::remove_file(storage_intent_path(paths));
                Ok(None)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 只删自己那条意图：标识对不上说明它已经被后一个保存请求替换了，不能删。
pub fn storage_intent_clear_if(paths: &SyncPaths, id: &str) -> Result<()> {
    let Some(intent) = storage_intent_load(paths)? else {
        return Ok(());
    };
    if intent.id != id {
        return Ok(());
    }
    match std::fs::remove_file(storage_intent_path(paths)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// 两份配置的存储段是否一致（TOML 是否已经补写成意图里的目标）。
fn same_storage(a: &SyncConfig, b: &SyncConfig) -> bool {
    a.transport == b.transport && a.webdav == b.webdav && a.file == b.file && a.s3 == b.s3
}

/// 上一次没存完的存储设置里的目标配置（宿主启动时按它补齐 `rclaw.toml`）。不删记录：
/// secrets 由引擎收尾之后按标识删。
pub fn storage_intent_config(paths: &SyncPaths) -> Result<Option<SyncConfig>> {
    Ok(storage_intent_load(paths)?.map(|intent| intent.config))
}

pub(crate) type Reply<T> = oneshot::Sender<Result<T>>;

/// 等着这个命令结果的一方还在不在。
///
/// 动同步空间的操作（创建、加入、退出、重置）要排到两轮同步之间才办；排到的时候对方已经
/// 不等了就不办。`/clear confirm local` 等过上限时已经告诉用户「没有退出」；加入之后的
/// 配置也要靠等着的那一方写回 `rclaw.toml`——没人接着做后半截的事，前半截就不该发生。
fn wanted<T>(reply: &Reply<T>) -> bool {
    !reply.is_closed()
}

/// 交给引擎线程的命令。
pub(crate) enum Cmd {
    Trigger(Trigger),
    /// 续聊之前：把会话末尾引用的附件取回来（取不到要告诉界面）。
    Prefetch {
        channel: String,
        chat_id: String,
    },
    SyncNow(Reply<()>),
    /// 发送前的拉取：拉完（或者不需要拉）就回话。
    Presend(oneshot::Sender<()>),
    Reconfigure(SyncConfig),
    Pause(bool, Reply<()>),
    Resolve {
        id: String,
        action: String,
        /// 用户看到的那一版（等确认的改动才有）。
        reviewed: Option<String>,
        reply: Reply<()>,
    },
    Takeover {
        owner: String,
        id: String,
        force: bool,
        /// 用户看到的那份任务定义的版本。
        reviewed: String,
        reply: Reply<()>,
    },
    CreateSpace(Reply<()>),
    JoinSpace {
        code: String,
        reply: Reply<SyncConfig>,
    },
    LeaveSpace(Reply<()>),
    ResetSpace(Reply<()>),
    PairingCode {
        include_storage: bool,
        reply: Reply<String>,
    },
    SetSecret {
        secret: String,
        reply: Reply<()>,
    },
    /// 存储的设置（可带刚填的密码）一起生效：密码绑定的是这份配置的存储账户，同一条
    /// 命令内核对并写入，中间不会插进别的配置变更（引擎线程一次只办一条命令）。
    SaveStorage {
        config: SyncConfig,
        secret: Option<String>,
        reply: Reply<()>,
    },
    ConfirmMetered(Reply<()>),
    SetBacklog {
        choice: Backlog,
        reply: Reply<()>,
    },
    Stop,
    /// 测试用：让引擎线程出一个没料到的错。
    #[cfg(test)]
    Crash,
}

/// 一轮的种类。完整的一轮包含拉取，所以约了两种时只跑完整的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Pull,
    Full,
}

/// 试着拿引擎单实例锁。拿到的文件句柄要一直留着；`None` = 别的进程（或本进程里上一个
/// 还没退出的引擎线程）拿着。
pub(crate) fn take_lock(paths: &SyncPaths) -> std::io::Result<Option<std::fs::File>> {
    paths.ensure_root()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.lock)?;
    Ok(fs2::FileExt::try_lock_exclusive(&file)
        .is_ok()
        .then_some(file))
}

/// 启动引擎线程。
///
/// 线程里出了没料到的错（panic）时它就此退出，不带倒进程里别的东西：面板上说明白，句柄
/// 发现命令通道关了，会重新起一条（[`super::SyncEngine::live`]）。同步的状态都在盘上，
/// 断在哪一步都能接着来。
pub(crate) fn spawn(
    shared: Arc<Shared>,
    config: SyncConfig,
    commands: mpsc::UnboundedReceiver<Cmd>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("rclaw-sync".into())
        .spawn(move || {
            let view = shared.clone();
            let run = std::panic::AssertUnwindSafe(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        shared.set_state("error", Some(format!("同步引擎没能启动: {error}")));
                        return;
                    }
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(&runtime, Worker::new(shared, config, commands).run());
            });
            if std::panic::catch_unwind(run).is_err() {
                tracing::error!("同步引擎的线程因为程序内部的错误退出了");
                view.set_state(
                    "error",
                    Some(
                        "同步引擎意外停止了（程序内部出错）。点「重试」重新启动；一再出现的话请\
                         把日志反馈给开发者"
                            .into(),
                    ),
                );
            }
        })
        .map(|_| ())
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

/// 一轮失败之后：面板上显示成什么状态，多久之后再试。
fn classify(error: &SyncError, failures: u32) -> (&'static str, Duration) {
    let backoff = RETRY_MIN
        .saturating_mul(1u32 << failures.saturating_sub(1).min(6))
        .min(RETRY_MAX);
    match error {
        SyncError::NeedsUser(_) => ("confirm", CONFIRM_RECHECK),
        SyncError::SpaceClosed | SyncError::Upgrade(_) => ("error", RETRY_SLOW),
        SyncError::Transport(error) => match error {
            TransportError::Auth(_)
            | TransportError::Insecure(_)
            | TransportError::Config(_)
            | TransportError::Unsupported(_)
            | TransportError::TooLarge(_) => ("error", RETRY_SLOW),
            TransportError::RateLimited { retry_after, .. } => (
                "offline",
                retry_after
                    .unwrap_or(backoff)
                    .clamp(RETRY_MIN, RATE_LIMIT_MAX),
            ),
            TransportError::NotFound(_) | TransportError::Timeout(_) | TransportError::Other(_) => {
                ("offline", backoff)
            }
            // 正常到不了这里：转成 `SyncError` 时它已经算作「校验不过」了。
            TransportError::Oversize(_) => ("error", backoff),
        },
        _ => ("error", backoff),
    }
}

struct Worker {
    shared: Arc<Shared>,
    config: SyncConfig,
    commands: mpsc::UnboundedReceiver<Cmd>,
    /// 引擎单实例锁：拿着它的进程才动同步状态。
    lock: Option<std::fs::File>,
    secrets: Secrets,
    /// 密钥文件读进来了。
    loaded: bool,
    /// 连着的同步空间。
    session: Option<Session>,
    /// 附件的上传、下载任务。
    uploads: Option<JoinHandle<()>>,
    downloads: Option<JoinHandle<()>>,
    /// 约好的下一轮。
    next: Option<(Kind, Instant)>,
    /// 出错之后，在这之前不自动重试。
    hold_until: Option<Instant>,
    failures: u32,
    failed_at: Option<Instant>,
    /// 已经连着「马上再来一轮」多少次。
    reruns: u32,
    background: bool,
    paused: bool,
    next_poll: Instant,
    next_tick: Instant,
    /// 没连上同步空间时，下次再试的时刻；`None` = 等用户动手（改配置、填密码、配对）。
    reopen_at: Option<Instant>,
    standby_tries: u32,
    /// 换了存储设置但上一刻还有在途提交：连接还没重建，下一轮开头再试。
    pending_switch: bool,
    /// 上面那条重试了几次（用于给出可见状态与放弃提示）。
    switch_tries: u32,
    /// 切换已放弃（等不到在途提交）：状态要粘住，不能被之后普通轮次的 idle 覆盖。
    switch_failed: bool,
    /// 从磁盘确认同步已经关闭（或宿主停止）：worker 退出，剩下的交给宿主。
    stop: bool,
    /// 上一次打开没连上的原因（协议锁竞争/确认失败/引擎锁被占）：给「立即同步」等
    /// 等着结果的命令一个和状态面板一致的说法。
    open_note: Option<String>,
}

impl Worker {
    fn new(
        shared: Arc<Shared>,
        config: SyncConfig,
        commands: mpsc::UnboundedReceiver<Cmd>,
    ) -> Worker {
        let now = Instant::now();
        Worker {
            shared,
            config,
            commands,
            lock: None,
            secrets: Secrets::default(),
            loaded: false,
            session: None,
            uploads: None,
            downloads: None,
            next: None,
            hold_until: None,
            failures: 0,
            failed_at: None,
            reruns: 0,
            background: false,
            paused: false,
            next_poll: now,
            next_tick: now,
            reopen_at: None,
            standby_tries: 0,
            pending_switch: false,
            switch_tries: 0,
            switch_failed: false,
            stop: false,
            open_note: None,
        }
    }

    async fn run(mut self) {
        // 打开连接时会在协议锁里确认磁盘上的同步设置（见 [`Worker::open`]）。
        self.open(true).await;
        loop {
            if self.stop {
                // 退场前把已经排队的命令处理掉：宿主可能在「确认关闭」与「接收端
                // 关闭」之间刚发了重新开启/退出空间。处理到重新开启配置就清掉
                // stop，继续跑；否则关掉接收端再退出——之后再发来的配置命令会
                // 发送失败，宿主会启动继任 worker 或走离线退出。
                let mut reopened = false;
                while let Ok(cmd) = self.commands.try_recv() {
                    self.handle(cmd).await;
                    if !self.stop {
                        reopened = true;
                        break;
                    }
                }
                if reopened {
                    continue;
                }
                self.commands.close();
                break;
            }
            self.revive_transfers();
            let wake = self.deadline();
            tokio::select! {
                biased;
                cmd = self.commands.recv() => match cmd {
                    // 句柄都释放了，或者同步被关掉了。
                    None | Some(Cmd::Stop) => break,
                    Some(cmd) => self.handle(cmd).await,
                },
                _ = self.shared.kick.notified() => self.request(Kind::Full, Duration::ZERO),
                _ = sleep_until(wake) => self.on_timer().await,
            }
        }
        let _ = self.teardown().await;
    }

    // -- 定时 ---------------------------------------------------------------

    /// 下一件定了时间的事。
    fn deadline(&self) -> Option<Instant> {
        let mut wake = self.reopen_at;
        if self.session.is_some() && !self.paused {
            let mut earliest = |at: Instant| wake = Some(wake.map_or(at, |old| old.min(at)));
            earliest(self.next_poll);
            earliest(self.next_tick);
            if let Some((_, at)) = self.next {
                earliest(at);
            }
        }
        wake
    }

    async fn on_timer(&mut self) {
        let now = Instant::now();
        if self.session.is_none() {
            if self.reopen_at.is_some_and(|at| at <= now) {
                self.reopen_at = None;
                // 待机接手/重试打开前再确认一次配置：这期间另一个进程可能保存过设置。
                self.open(true).await;
            }
            return;
        }
        if self.paused {
            return;
        }
        if self.next_poll <= now {
            self.schedule_poll();
            // 后台只在不按流量计费的网络下轮询（附件的传输不受这个限制）。
            if !(self.background && self.shared.ctx.host.is_metered()) {
                self.request(Kind::Full, Duration::ZERO);
            }
        }
        if self.next_tick <= now {
            self.tick().await;
        }
        if let Some((kind, at)) = self.next {
            if at <= Instant::now() {
                let _ = self.run_round(kind).await;
            }
        }
    }

    fn tick_every(&self) -> Duration {
        if self.background {
            TICK_BACKGROUND
        } else {
            TICK
        }
    }

    fn schedule_poll(&mut self) {
        let secs = if self.background {
            self.config.background_interval_sec
        } else {
            self.config.poll_interval_sec
        };
        self.next_poll = Instant::now() + Duration::from_secs(secs.max(5));
    }

    /// 约一轮：`after` 之后跑。已经约了的话取更早的时刻、更完整的种类；出错之后的退避
    /// 期内不提前。
    fn request(&mut self, kind: Kind, after: Duration) {
        let mut at = Instant::now() + after;
        if let Some(hold) = self.hold_until {
            at = at.max(hold);
        }
        self.next = Some(match self.next {
            Some((old_kind, old_at)) => (old_kind.max(kind), old_at.min(at)),
            None => (kind, at),
        });
    }

    /// 本机变更检查：有新消息等着发布、运行提示变了，就约一轮。
    async fn tick(&mut self) {
        self.next_tick = Instant::now() + self.tick_every();
        if self.paused {
            return;
        }
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if !session.warm {
            return;
        }
        match session.tick(&self.shared, &self.config).await {
            Ok(Some(after)) => self.request(Kind::Full, after),
            Ok(None) => {}
            Err(error) => tracing::debug!("检查本机变更失败: {error}"),
        }
    }

    // -- 轮次 ---------------------------------------------------------------

    /// 跑一轮。失败时返回原因（给等着结果的调用方看）。
    async fn run_round(&mut self, kind: Kind) -> std::result::Result<(), String> {
        // 已经约好的也算在这一轮里：先消费掉。留到后面再取的话，切换失败的早退会把
        // 已到期的时刻留在 `next` 里——下一轮立刻又被唤醒，teardown 之间没有任何
        // 间隙，其他命令只能等每一轮 teardown 让出。
        let pending = self.next.take();
        // 上一刻没能重建的连接（换存储设置时有在途提交）：每轮开头再试。放弃之后
        // （switch_failed）也留在这条路上——「立即同步」/定时轮次都会再试一次切换，
        // 否则配置与连接会长期不一致而界面看起来正常。
        if (self.pending_switch || self.switch_failed) && self.session.is_some() {
            if self.teardown().await {
                self.pending_switch = false;
                self.switch_failed = false;
                self.switch_tries = 0;
                self.open(false).await;
                // open() 给新连接约了「立即一轮 Full」；这次调用接下来就会亲自跑它
                // （新 session 没预热，kind 会被抬成 Full）。不消费的话同一轮会再跑
                // 一次，残留的立即时刻还会把之后的延迟重试提前。
                self.next = None;
            } else {
                self.switch_tries = self.switch_tries.saturating_add(1);
                if self.switch_tries >= SWITCH_GIVE_UP_TRIES {
                    // 一直等不到在途提交结束：把错误粘住（配置仍是新的、连接仍是旧的），
                    // 不再自动重试；下一次「立即同步」或用户重新提交配置会再试。
                    self.switch_failed = true;
                    self.pending_switch = false;
                    let message = "存储设置还没生效：有附件提交一直没有结束。当前仍在使用旧存储；点「立即同步」可重试切换".to_string();
                    self.shared.set_state("error", Some(message.clone()));
                    return Err(message);
                }
                let message = format!(
                    "等待在途的附件提交结束（第 {} 次重试）",
                    self.switch_tries + 1
                );
                self.shared.set_state("syncing", Some(message.clone()));
                // 明确安排下一次重试：下一次是新的到期时刻，不是旧的已到期时刻。
                self.request(Kind::Full, RETRY_MIN);
                return Err(message);
            }
        }
        let Some(session) = self.session.as_mut() else {
            return Err("还没有连上同步空间".into());
        };
        // 还没完整跑过一轮（本机的存量、空间里现有的内容都没过一遍）时只跑完整的。
        let kind = if session.warm { kind } else { Kind::Full };
        if kind == Kind::Pull {
            if let Some((Kind::Full, at)) = pending {
                self.next = Some((Kind::Full, at));
            }
        }
        let shared = self.shared.clone();
        shared.round_since_ms.store(now_ms(), Ordering::Relaxed);
        let result = match kind {
            Kind::Full => {
                session
                    .full_round(&shared, &self.config, &mut self.secrets)
                    .await
            }
            Kind::Pull => session.pull_round(&shared, &self.config).await,
        };
        shared.round_since_ms.store(0, Ordering::Relaxed);
        match result {
            Ok(outcome) => {
                self.round_ok(kind, outcome);
                Ok(())
            }
            Err(error) => {
                self.round_failed(&error);
                Err(error.to_string())
            }
        }
    }

    fn round_ok(&mut self, kind: Kind, outcome: Outcome) {
        self.failures = 0;
        self.failed_at = None;
        self.hold_until = None;
        self.shared.storage_ok.store(true, Ordering::Relaxed);
        if kind == Kind::Full {
            // 完整的一轮刚跑过：定时轮询从现在重新计时。
            self.schedule_poll();
        }
        let busy = outcome.again
            || outcome
                .retry_after
                .is_some_and(|after| after < Duration::from_secs(1));
        if !self.switch_failed {
            // 切换没完成的错误要粘住：普通轮次成功不代表配置已经生效。
            self.shared
                .set_state(if busy { "syncing" } else { "idle" }, None);
        }
        if outcome.again {
            if self.reruns < RERUN_MAX {
                self.reruns += 1;
                self.request(Kind::Full, Duration::ZERO);
            } else {
                self.reruns = 0;
                self.request(Kind::Full, RERUN_PAUSE);
            }
        } else if kind == Kind::Full {
            self.reruns = 0;
        }
        if let Some(after) = outcome.retry_after {
            self.request(Kind::Full, after);
        }
        if outcome.follow_up {
            self.request(Kind::Full, FOLLOW_UP_DELAY);
        }
    }

    fn round_failed(&mut self, error: &SyncError) {
        self.failures = self.failures.saturating_add(1);
        self.reruns = 0;
        let now = Instant::now();
        self.failed_at = Some(now);
        let (state, wait) = classify(error, self.failures);
        if state == "confirm" {
            tracing::debug!("同步在等用户确认: {error}");
        } else if self.failures <= 3 {
            tracing::warn!("这一轮同步没有完成（{wait:?} 后再试）: {error}");
        } else {
            tracing::debug!("这一轮同步没有完成（{wait:?} 后再试）: {error}");
        }
        self.shared.storage_ok.store(false, Ordering::Relaxed);
        self.shared.set_state(state, Some(error.to_string()));
        self.hold_until = Some(now + wait);
        self.next = None;
        self.request(Kind::Full, Duration::ZERO);
    }

    // -- 连上 / 断开同步空间 --------------------------------------------------

    fn secret_saved(&self) -> bool {
        !self.secrets.storage_secret(&self.config).is_empty()
    }

    fn fail_open(&mut self, message: String, retry: Option<Duration>) {
        tracing::warn!("同步没有启动: {message}");
        self.shared.set_state("error", Some(message));
        self.reopen_at = retry.map(|after| Instant::now() + after);
    }

    /// 按磁盘上的配置更新「存储段 + 开关」，其余字段（范围、通道、设备名、轮询间隔
    /// 等）以宿主内存里的为准——保存协议只持久化存储与开关，不能拿一份只写了存储段
    /// 的 TOML 把别处刚改过的设置回退。`Err` 表示文件在但这次读不出来，打开应让行。
    fn adopt_disk_config(&mut self) -> std::result::Result<(), ()> {
        let Some(refresh) = self.shared.refresh_config.clone() else {
            return Ok(());
        };
        // 磁盘确认与宿主发布共享同一顺序：读盘前记下共享配置版本，提交合并时还在
        // 同一把锁里复核——版本变了说明宿主刚发布了新配置（可能正是这次回调读盘期间
        // 写下去的 TOML），手上的存储段可能更旧，重读一遍再合并。
        for _ in 0..3 {
            let version = self.shared.config_version.load(Ordering::Relaxed);
            let Some(fresh) = refresh()? else {
                return Ok(());
            };
            let mut shared = lock(&self.shared.config);
            if self.shared.config_version.load(Ordering::Relaxed) != version {
                drop(shared);
                continue;
            }
            // 只改「磁盘确认的字段」（存储段与开关），其余字段保留宿主最新的一份——
            // 用 worker 自己的旧副本整份写回会把刚发表的范围/通道/设备名回退。
            let mut merged = shared.clone();
            merged.apply_storage_from(&fresh);
            merged.enabled = fresh.enabled;
            if merged != *shared {
                tracing::info!("按磁盘上的 rclaw.toml 更新同步设置（另一进程保存过）");
                *shared = merged.clone();
            }
            drop(shared);
            if merged != self.config {
                self.config = merged;
                self.shared
                    .attachment_max
                    .store(self.config.attachment_max_bytes(), Ordering::Relaxed);
            }
            return Ok(());
        }
        // 版本一直变：这一轮不覆盖共享配置，按宿主当前配置打开，下一次打开再确认。
        self.config = lock(&self.shared.config).clone();
        self.shared
            .attachment_max
            .store(self.config.attachment_max_bytes(), Ordering::Relaxed);
        Ok(())
    }

    /// 连上本机所在的同步空间：拿引擎锁、读密钥、组装存储、准备本机的同步数据、起附件
    /// 任务，然后约第一轮。哪一步不成就停在相应的状态上。
    async fn open(&mut self, confirm: bool) {
        if self.session.is_some() {
            return;
        }
        // 确认磁盘上的同步设置（启动、待机接手、立即同步共用）：持着保存协议锁读取，
        // 并一直持到密钥与「没存完的设置」结算之后——确认完到取到引擎锁之间另一进程
        // 再保存一次的话，配置与 secrets 会各是半套。拿不到锁说明别的进程正在保存：
        // 这次让行，稍后再来（这时还没占引擎锁，不挡别人）。宿主自己持锁等
        // `SaveStorage` 回复的路径传 `false`：配置与凭据来自那条命令，由宿主结算意图。
        let mut protocol_lock: Option<StorageLock> = None;
        if confirm {
            let lock_paths = self.shared.ctx.paths.clone();
            match blocking(move || StorageLock::try_acquire(&lock_paths)).await {
                Ok(Some(lock)) => protocol_lock = Some(lock),
                Ok(None) => {
                    self.open_note = Some(
                        "另一个进程正在保存同步设置，确认后再连接（稍后自动重试）".into(),
                    );
                    self.reopen_at = Some(Instant::now() + STANDBY_RETRY);
                    return;
                }
                Err(error) => {
                    // 有宿主回调时「确认」是打开的前提：锁文件坏了说明配置和凭据都
                    // 没法确认，不能拿可能过期的缓存去连存储——进入可见的重试状态。
                    // 没有回调（测试/无配置文件的宿主）时按当前配置继续。
                    if self.shared.refresh_config.is_some() {
                        tracing::warn!("确认同步设置时拿不到协议锁（稍后重试）: {error}");
                        self.shared.set_state(
                            "syncing",
                            Some("等待确认同步存储设置（锁不可用），稍后重试".into()),
                        );
                        self.open_note = Some(
                            "确认同步存储设置失败（存储锁不可用），稍后自动重试".into(),
                        );
                        self.reopen_at = Some(Instant::now() + STANDBY_RETRY);
                        return;
                    }
                    tracing::warn!("确认同步设置时拿不到协议锁（按当前配置继续）: {error}");
                }
            }
            if protocol_lock.is_some() {
                if self.adopt_disk_config().is_err() {
                    self.reopen_at = Some(Instant::now() + STANDBY_RETRY);
                    return;
                }
                if !self.config.enabled {
                    // 另一个进程把同步关了：不接手引擎所有权，状态以磁盘为准。worker
                    // 自己也退出——留着它会让「关闭同步后只清本机」走在线命令路径，
                    // 却因为没有引擎锁而被拒绝。
                    self.lock = None;
                    self.shared.set_state("off", None);
                    self.reopen_at = None;
                    self.stop = true;
                    return;
                }
            }
        }
        // 上次切换可能把提交入口关着就失败了：只要不在切换中，入口就该开着（连不上时
        // 阶段本就无从开始，接上之后自然有效）。
        self.shared.ctx.space.reopen();
        self.reopen_at = None;
        let shared = self.shared.clone();
        let paths = shared.ctx.paths.clone();

        if self.lock.is_none() {
            let lock_paths = paths.clone();
            match blocking(move || Ok(take_lock(&lock_paths)?)).await {
                Ok(Some(file)) => {
                    self.lock = Some(file);
                    self.standby_tries = 0;
                }
                Ok(None) => {
                    self.standby_tries += 1;
                    let wait = if self.standby_tries == 1 {
                        STANDBY_FIRST
                    } else {
                        STANDBY_RETRY
                    };
                    self.reopen_at = Some(Instant::now() + wait);
                    self.open_note = Some(STANDBY_HINT.into());
                    shared.set_state("standby", Some(STANDBY_HINT.into()));
                    return;
                }
                Err(error) => {
                    return self.fail_open(format!("同步目录用不了: {error}"), Some(OPEN_RETRY));
                }
            }
        }

        match blocking(move || Secrets::load(&paths)).await {
            Ok(secrets) => {
                self.secrets = secrets;
                self.loaded = true;
            }
            Err(error) => {
                self.loaded = false;
                return self.fail_open(error.to_string(), Some(OPEN_RETRY));
            }
        }
        // 上次没存完的存储设置先收尾（secrets 按 TOML 里的目标补写），两个文件才是一套。
        // 放在空间切换恢复之前：恢复要按「这个账户」的当前凭据去开存储，先让配置与
        // secrets 收敛到同一套。
        if let Err(error) = self.resume_storage_intent(protocol_lock.is_some()).await {
            tracing::warn!("没存完的存储设置收尾失败（下次打开再试）: {error}");
        }
        // 配置、密钥与意图都已经收敛到同一套：保存协议锁到这里就可以放了。
        drop(protocol_lock);
        // 上一次重置 / 退出没做完：按落盘的记录与远端的实际状态接着做（重置可能已经把
        // 本机换到了新空间），再按正常打开往下走。
        if let Err(error) =
            space::resume_pending(&shared.ctx, &self.config, &mut self.secrets).await
        {
            return self.fail_open(
                format!("上一次的空间切换没有做完: {error}（修好后会自动接着做）"),
                Some(OPEN_RETRY),
            );
        }
        let paired = self.secrets.is_paired();
        shared.paired.store(paired, Ordering::Relaxed);
        shared.set_space(
            paired.then(|| self.secrets.space_id.clone()),
            self.secret_saved(),
        );
        if !paired {
            shared.set_state("setup", None);
            return;
        }

        // 配置或存储设置不对：等用户改（改配置、填密码都会再来一次）。
        if let Err(error) = self.config.validate() {
            return self.fail_open(error.to_string(), None);
        }
        let live = match space::load_live(&self.config, &self.secrets).await {
            Ok(live) => live,
            Err(error) => return self.fail_open(error.to_string(), None),
        };

        let prepared = {
            let (ctx, keys) = (shared.ctx.clone(), live.keys.clone());
            let trash_days = self.config.trash_days;
            let mut secrets = self.secrets.clone();
            blocking(move || {
                let identity = space::prepare_local(&ctx, &mut secrets, &keys, trash_days)?;
                let paused = db::read(ctx.db(), |conn| {
                    Ok(db::kv_json::<bool>(conn, db::KV_PAUSED)? == Some(true))
                })?;
                Ok((identity, secrets, paused))
            })
            .await
        };
        let (identity, secrets, paused) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                return self
                    .fail_open(format!("准备本机的同步数据失败: {error}"), Some(OPEN_RETRY));
            }
        };
        self.secrets = secrets;
        self.paused = paused;
        shared.paused.store(paused, Ordering::Relaxed);
        shared.storage_ok.store(true, Ordering::Relaxed);
        shared.last_pull_ms.store(0, Ordering::Relaxed);

        // 连上了：重新打开附件提交入口（换空间 / 清理期间是关着的）。
        shared.ctx.space.reopen();
        let session = Session::new(live, identity);
        self.start_uploads(session.live.clone());
        self.start_downloads(session.live.clone());
        self.session = Some(session);
        self.open_note = None;
        // 连接已经按当前配置（含新的存储设置）重建：切换标记与计数到这里就算完成。
        self.pending_switch = false;
        self.switch_failed = false;
        self.switch_tries = 0;
        self.failures = 0;
        self.failed_at = None;
        self.hold_until = None;
        self.reruns = 0;
        self.next = None;
        self.next_tick = Instant::now() + self.tick_every();
        self.schedule_poll();
        if paused {
            shared.set_state("paused", None);
        } else {
            shared.set_state("syncing", None);
            self.request(Kind::Full, Duration::ZERO);
        }
    }

    fn start_uploads(&mut self, live: Arc<space::Live>) {
        let task = transfer::upload_task(self.shared.clone(), live);
        self.uploads = Some(tokio::task::spawn_local(task));
    }

    fn start_downloads(&mut self, live: Arc<space::Live>) {
        let (fetch, requests) = mpsc::unbounded_channel();
        self.shared.set_fetch(Some(fetch));
        let task = transfer::download_task(self.shared.clone(), live, requests);
        self.downloads = Some(tokio::task::spawn_local(task));
    }

    /// 附件任务不会自己结束：结束了就是里面出了没料到的错（运行时接住了 panic，只停了那
    /// 一个任务）。把停了的那个重新起起来——上传任务等下一次被叫醒（一轮同步成功）再干活；
    /// 下载任务手上排着的请求丢了，用户再点一次就又排上。
    fn revive_transfers(&mut self) {
        let Some(live) = self.session.as_ref().map(|session| session.live.clone()) else {
            return;
        };
        if self.uploads.as_ref().is_some_and(JoinHandle::is_finished) {
            tracing::error!("附件的上传任务因为程序内部的错误退出了，重新启动");
            self.start_uploads(live.clone());
        }
        if self.downloads.as_ref().is_some_and(JoinHandle::is_finished) {
            tracing::error!("附件的下载任务因为程序内部的错误退出了，重新启动");
            self.start_downloads(live);
        }
    }

    /// 断开同步空间：停掉附件任务。写到一半的快照留给下次清理（它的名字记在库里）。
    /// 返回是否换成了新世代：在途的提交阶段没能在期限内退场时**不换代**——已经有最终
    /// 写入在进行时宣告换代完成，旧任务随后的改名会把旧字节落到可能被复用的路径上。
    /// 调用方据此中止这次切换（保持旧连接），稍后重试。
    async fn teardown(&mut self) -> bool {
        // 拆任务只拆得掉异步的那一层：已经开始的文件与记账阶段（`spawn_blocking`）拦不
        // 住。顺序是：先关提交入口（旧任务此后拿到的阶段一律作废）→ 等在途的阶段走完 →
        // 再等最终写入段退场 → 换代。任何一步超时都不换代，恢复提交入口让调用方重试
        // （下载入口也原样留着，中止的切换不影响正在用的连接）。
        self.shared.ctx.space.seal();
        let idle = self
            .shared
            .ctx
            .space
            .wait_idle(TEARDOWN_SETTLE)
            .await;
        let writers = idle
            && self
                .shared
                .ctx
                .space
                .wait_writers(TEARDOWN_SETTLE)
                .await;
        if !writers {
            tracing::warn!("还有在途的提交阶段，暂不换空间（稍后重试）");
            self.shared.ctx.space.reopen();
            return false;
        }
        self.shared.set_fetch(None);
        self.shared.ctx.space.bump();
        for task in [self.uploads.take(), self.downloads.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
            let _ = task.await;
        }
        self.session = None;
        self.next = None;
        true
    }

    // -- 命令 ---------------------------------------------------------------

    async fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Trigger(trigger) => self.on_trigger(trigger).await,
            Cmd::Prefetch { channel, chat_id } => self.prefetch(channel, chat_id, true).await,
            Cmd::SyncNow(reply) => {
                let _ = reply.send(self.sync_now().await);
            }
            Cmd::Presend(reply) => {
                self.presend().await;
                let _ = reply.send(());
            }
            Cmd::Reconfigure(config) => self.reconfigure(config).await,
            Cmd::Pause(paused, reply) => {
                let _ = reply.send(self.set_paused(paused).await);
            }
            Cmd::Resolve {
                id,
                action,
                reviewed,
                reply,
            } => {
                let _ = reply.send(self.resolve(id, action, reviewed).await);
            }
            Cmd::Takeover {
                owner,
                id,
                force,
                reviewed,
                reply,
            } => {
                let _ = reply.send(self.takeover(owner, id, force, reviewed).await);
            }
            Cmd::CreateSpace(reply) => {
                if wanted(&reply) {
                    let _ = reply.send(self.create_space().await);
                }
            }
            Cmd::JoinSpace { code, reply } => {
                if wanted(&reply) {
                    let _ = reply.send(self.join_space(&code).await);
                }
            }
            Cmd::LeaveSpace(reply) => {
                if wanted(&reply) {
                    let _ = reply.send(self.leave_space().await);
                }
            }
            Cmd::ResetSpace(reply) => {
                if wanted(&reply) {
                    let _ = reply.send(self.reset_space().await);
                }
            }
            Cmd::PairingCode {
                include_storage,
                reply,
            } => {
                let _ = reply.send(self.pairing_code(include_storage).await);
            }
            Cmd::SetSecret { secret, reply } => {
                let _ = reply.send(self.set_secret(secret).await);
            }
            Cmd::SaveStorage {
                config,
                secret,
                reply,
            } => {
                let _ = reply.send(self.save_storage(config, secret).await);
            }
            Cmd::ConfirmMetered(reply) => {
                let _ = reply.send(self.confirm_metered().await);
            }
            Cmd::SetBacklog { choice, reply } => {
                let _ = reply.send(self.set_backlog(choice).await);
            }
            Cmd::Stop => {}
            #[cfg(test)]
            Cmd::Crash => panic!("测试：引擎线程出错"),
        }
    }

    async fn on_trigger(&mut self, trigger: Trigger) {
        match trigger {
            Trigger::LocalChange => self.request(Kind::Full, LOCAL_CHANGE_DELAY),
            Trigger::TurnEnded => self.tick().await,
            Trigger::Foreground => {
                self.background = false;
                self.hold_until = None;
                if self.session.is_none() {
                    self.open(true).await;
                }
                self.next_tick = Instant::now() + self.tick_every();
                self.schedule_poll();
                self.request(Kind::Full, Duration::ZERO);
            }
            Trigger::Background => {
                self.background = true;
                self.schedule_poll();
                // 冲刷：攒着的回执也一并写出去。
                if let Some(session) = self.session.as_mut() {
                    session.force_head = true;
                }
                self.request(Kind::Full, Duration::ZERO);
            }
            Trigger::Focus => self.pull_if_stale(),
            Trigger::OpenSession { channel, chat_id } => {
                self.pull_if_stale();
                self.prefetch(channel, chat_id, false).await;
            }
            Trigger::Imported => {
                if let Some(session) = self.session.as_mut() {
                    session.full = true;
                }
                self.request(Kind::Full, Duration::ZERO);
            }
        }
    }

    /// 用户回到了窗口 / 打开了会话：距上次拉取有一阵了就拉一次。
    fn pull_if_stale(&mut self) {
        if self.failed_at.is_some_and(|at| at.elapsed() >= FOCUS_RETRY) {
            self.hold_until = None;
        }
        if now_ms() - self.shared.last_pull_ms.load(Ordering::Relaxed) >= PULL_FRESH_MS {
            self.request(Kind::Pull, Duration::ZERO);
        }
    }

    /// 把一个会话末尾引用的、还在别的设备上的附件排进下载队列（§8.7）。`user`：续聊要用，
    /// 取不到要告诉界面。
    async fn prefetch(&mut self, channel: String, chat_id: String, user: bool) {
        let attachments =
            local::scopes_of(&self.config, self.shared.sessions_supported).attachments;
        if self.session.is_none() || self.paused || !attachments {
            return;
        }
        let ctx = self.shared.ctx.clone();
        let max_size = self.config.attachment_max_bytes();
        let tail = blocking(move || {
            local::tail_remote_blobs(&ctx, &channel, &chat_id, TAIL_MESSAGES, max_size, now_ms())
        })
        .await;
        match tail {
            Ok(blobs) => {
                for blob in blobs {
                    self.shared.request_fetch(blob, user);
                }
            }
            Err(error) => tracing::debug!("查找会话引用的附件失败: {error}"),
        }
    }

    /// 没连上同步空间时，给等着结果的调用方一个说法。
    fn unavailable(&self) -> SyncError {
        if let Some(note) = &self.open_note {
            return SyncError::other(note.clone());
        }
        if self.lock.is_none() {
            return SyncError::other(STANDBY_HINT);
        }
        if self.loaded && !self.secrets.is_paired() {
            return SyncError::other("还没有创建或加入同步空间");
        }
        SyncError::other("还没有连上同步空间，请先看一下同步面板里的提示")
    }

    /// 本进程拿着引擎锁、密钥文件也读进来了：可以动同步空间。
    fn require_owner(&self) -> Result<()> {
        if self.lock.is_some() && self.loaded {
            Ok(())
        } else {
            Err(self.unavailable())
        }
    }

    async fn sync_now(&mut self) -> Result<()> {
        if self.session.is_none() {
            self.open(true).await;
        }
        let Some(session) = self.session.as_mut() else {
            return Err(self.unavailable());
        };
        if self.paused {
            return Err(SyncError::other("同步已经暂停，恢复之后才会同步"));
        }
        session.force_head = true;
        self.hold_until = None;
        self.run_round(Kind::Full).await.map_err(SyncError::other)
    }

    async fn presend(&mut self) {
        if self.session.is_none() || self.paused || self.hold_until.is_some() {
            return;
        }
        if now_ms() - self.shared.last_pull_ms.load(Ordering::Relaxed) < PULL_FRESH_MS {
            return;
        }
        let _ = self.run_round(Kind::Pull).await;
    }

    async fn reconfigure(&mut self, config: SyncConfig) {
        let old = std::mem::replace(&mut self.config, config);
        if self.config.enabled {
            // 重新开启（可能发生在退场清理期间）：不再退出。
            self.stop = false;
        }
        let new = &self.config;
        self.shared
            .attachment_max
            .store(new.attachment_max_bytes(), Ordering::Relaxed);
        let storage = old.transport != new.transport
            || old.webdav != new.webdav
            || old.file != new.file
            || old.s3 != new.s3;
        let range = old.scopes != new.scopes
            || old.channels != new.channels
            || old.device_name != new.device_name;
        if self.session.is_none() {
            // 之前没连上（设置不全、存储组装不起来、本机这边没准备好）：再试一次。配置没变
            // 也试——面板上的「重试」就是把现有的配置再交过来一遍。
            self.open(false).await;
            return;
        }
        if old == self.config {
            // 上一次切换因为等在途提交而放弃（已升级成可见错误）：用户点「重试」会带着
            // 同一份配置再来一次，这里重新排队换。
            if !self.pending_switch
                && self.switch_tries >= SWITCH_GIVE_UP_TRIES
                && self.session.is_some()
            {
                self.pending_switch = true;
                self.switch_failed = false;
                self.switch_tries = 0;
                self.shared.set_state(
                    "syncing",
                    Some("重新尝试重建同步连接".into()),
                );
                self.request(Kind::Full, Duration::ZERO);
            }
            return;
        }
        if storage {
            if !self.teardown().await {
                // 还有在途的提交阶段：配置已经是新的，连接留到下一轮开头再重建
                // （不能退回旧配置——宿主与 TOML 都已经换成新的了）。
                self.pending_switch = true;
                self.switch_failed = false;
                self.switch_tries = 0;
                self.shared.set_state(
                    "syncing",
                    Some("等待在途的附件提交结束，稍后重建同步连接".into()),
                );
                self.request(Kind::Full, RETRY_MIN);
                return;
            }
            self.open(false).await;
            return;
        }
        if range {
            // 范围、通道、设备名变了：全量扫描一遍，设备名重新公布。
            if let Some(session) = self.session.as_mut() {
                session.full = true;
            }
            self.request(Kind::Full, Duration::ZERO);
        }
        self.schedule_poll();
    }

    async fn set_paused(&mut self, paused: bool) -> Result<()> {
        if self.session.is_none() {
            return Err(self.unavailable());
        }
        let ctx = self.shared.ctx.clone();
        blocking(move || {
            db::write(ctx.db(), |tx| {
                if paused {
                    db::kv_put_json(tx, db::KV_PAUSED, &true)
                } else {
                    Ok(sync_db::kv_del(tx, db::KV_PAUSED)?)
                }
            })
        })
        .await?;
        self.paused = paused;
        self.shared.paused.store(paused, Ordering::Relaxed);
        if paused {
            self.shared.set_state("paused", None);
        } else {
            self.shared.set_state("syncing", None);
            self.hold_until = None;
            self.next_tick = Instant::now() + self.tick_every();
            self.schedule_poll();
            self.request(Kind::Full, Duration::ZERO);
            self.shared.uploads.notify_one();
        }
        Ok(())
    }

    /// 处理待办事项、接管定时任务这类操作的前提：连着同步空间，而且没有暂停（决定要落到
    /// 文件上、要发给其他设备，都得跑一轮）。
    fn require_active(&self) -> Result<&Session> {
        let Some(session) = self.session.as_ref() else {
            return Err(self.unavailable());
        };
        if self.paused {
            return Err(SyncError::other("同步已经暂停，恢复同步之后再处理"));
        }
        Ok(session)
    }

    async fn resolve(
        &mut self,
        id: String,
        action: String,
        reviewed: Option<String>,
    ) -> Result<()> {
        let s = self.require_active()?.settings(&self.shared, &self.config);
        let ctx = self.shared.ctx.clone();
        let reload = blocking(move || {
            if let Some(sid) = id.strip_prefix("quarantine-") {
                let retry = match action.as_str() {
                    "retry" => true,
                    "discard" => false,
                    _ => return Err(SyncError::invalid("这条待处理事项不支持这个操作")),
                };
                merge::resolve_quarantine(&ctx, sid, retry)?;
                return Ok(None);
            }
            files::decide(&ctx, &s, &id, &action, reviewed.as_deref())
        })
        .await?;
        if let Some(what) = reload {
            self.shared.ctx.host.reload(what);
        }
        self.shared.emit(SyncEvent::Status);
        // 决定要落到本机文件上、要告诉其他设备：马上来一轮。
        self.request(Kind::Full, Duration::ZERO);
        Ok(())
    }

    async fn takeover(
        &mut self,
        owner: String,
        id: String,
        force: bool,
        reviewed: String,
    ) -> Result<()> {
        let s = self.require_active()?.settings(&self.shared, &self.config);
        let ctx = self.shared.ctx.clone();
        blocking(move || files::cron::request_takeover(&ctx, &s, &owner, &id, force, &reviewed))
            .await?;
        self.shared.emit(SyncEvent::Status);
        self.request(Kind::Full, Duration::ZERO);
        Ok(())
    }

    async fn create_space(&mut self) -> Result<()> {
        self.require_owner()?;
        self.config.validate()?;
        space::create(&self.shared.ctx, &self.config, &mut self.secrets).await?;
        self.open(false).await;
        Ok(())
    }

    async fn join_space(&mut self, code: &str) -> Result<SyncConfig> {
        self.require_owner()?;
        // 已经在这个空间里（上次加入在把设置写回文件之前中断了）：重复加入是幂等的，
        // 只把配对码带来的存储设置对齐。别的空间的配对码仍然报错。
        if self.secrets.is_paired() {
            let parsed = crate::pairing::PairingCode::parse(code)?;
            if parsed.space_id != self.secrets.space_id || parsed.key != self.secrets.key {
                return Err(SyncError::other(
                    "本机已经在一个同步空间里了；要加入另一个，请先退出当前的",
                ));
            }
            let mut next = self.secrets.clone();
            let joined = space::apply_pairing(&self.config, &mut next, &parsed);
            if joined == self.config && next == self.secrets {
                // 完全没变化：不用换凭据；但如果当前没连着（断网后重试同一配对码），
                // 顺手重连一次。
                if self.session.is_none() {
                    self.open(false).await;
                }
                return Ok(joined);
            }
            // 配对码可能带了更新的存储地址/凭据：换凭据必须重建连接，先确认能换（在途
            // 的提交阶段会拒绝），再按正常加入的规矩核对远端确实是这个空间、密钥对得上。
            if !self.teardown().await {
                return Err(SyncError::other("还有在途的附件提交，稍后再重新加入"));
            }
            let verified = match space::load_live(&joined, &next).await {
                Ok(live) => {
                    crate::remote::open_space(
                        live.transport(),
                        &live.keys,
                        &next.space_id,
                        false,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            if let Err(error) = verified {
                self.open(false).await;
                return Err(error);
            }
            let paths = self.shared.ctx.paths.clone();
            let saved = next.clone();
            if let Err(error) = blocking(move || saved.save(&paths)).await {
                self.open(false).await;
                return Err(error);
            }
            self.secrets = next;
            self.config = joined.clone();
            self.open(false).await;
            return Ok(joined);
        }
        let joined = space::join(
            &self.shared.ctx.paths,
            &self.config,
            &mut self.secrets,
            code,
        )
        .await?;
        // 配对码里带了存储设置的话，配置也跟着换。
        self.config = joined.clone();
        self.open(false).await;
        Ok(joined)
    }

    async fn leave_space(&mut self) -> Result<()> {
        self.require_owner()?;
        if !self.teardown().await {
            return Err(SyncError::other("还有在途的附件提交，稍后再退出"));
        }
        let result = space::leave(&self.shared.ctx, &self.config, &mut self.secrets).await;
        // 退出成功就停在「还没配对」；没成功就照旧连回去。
        self.open(false).await;
        result
    }

    async fn reset_space(&mut self) -> Result<()> {
        self.require_owner()?;
        self.config.validate()?;
        if !self.teardown().await {
            return Err(SyncError::other("还有在途的附件提交，稍后再重置"));
        }
        let result = space::reset(&self.shared.ctx, &self.config, &mut self.secrets).await;
        self.open(false).await;
        result
    }

    async fn pairing_code(&mut self, include_storage: bool) -> Result<String> {
        if self.require_owner().is_ok() {
            return space::pairing_code(&self.config, &self.secrets, include_storage);
        }
        // 本进程不是做同步的那个：密钥以磁盘上的为准（只读）。
        let paths = self.shared.ctx.paths.clone();
        let secrets = blocking(move || Secrets::load(&paths)).await?;
        space::pairing_code(&self.config, &secrets, include_storage)
    }

    async fn set_secret(&mut self, secret: String) -> Result<()> {
        self.require_owner()?;
        // 换密码要重建连接：先确认能换（在途提交会拒绝），换不成时本机什么都不改。
        if self.secrets.is_paired() && !self.teardown().await {
            return Err(SyncError::other("还有在途的附件提交，稍后重试保存"));
        }
        let mut next = self.secrets.clone();
        if let Err(error) = next.set_storage_secret(&self.config, secret) {
            self.open(false).await;
            return Err(error);
        }
        let (paths, saved) = (self.shared.ctx.paths.clone(), next.clone());
        if let Err(error) = blocking(move || saved.save(&paths)).await {
            // 已经 teardown 过：把旧的连接恢复回来，别让同步停在 sealed 状态。
            self.open(false).await;
            return Err(error);
        }
        self.secrets = next;
        self.shared.set_space(
            self.secrets
                .is_paired()
                .then(|| self.secrets.space_id.clone()),
            self.secret_saved(),
        );
        if self.secrets.is_paired() {
            self.open(false).await;
        }
        Ok(())
    }

    /// 存储设置和密码一起生效：「先存配置、再单独存密码」分两条命令的话，中间可能插进
    /// 别的入口改过的配置，密码就会绑到后一份配置的存储账户上（`storage_secret` 按账户
    /// 匹配），甚至发送到那个地址去。这里在同一条命令里按新配置绑密码、落盘、重建客户端。
    ///
    /// 调用方在派发之前已把「更新意图」落盘（[`storage_intent_save`]）：本方法失败
    /// （worker 待机、secrets 写失败）时两个文件不会都换好，重启后的 [`Self::open`]
    /// 按记录把剩下的一半补上，两边收敛到同一套。
    async fn save_storage(&mut self, config: SyncConfig, secret: Option<String>) -> Result<()> {
        self.require_owner()?;
        // 换连接前先确认在途提交能退场：退不了时本机什么都不改（TOML 已经是新的，
        // 意图还在，重启时由恢复流程收敛）。
        if self.secrets.is_paired() && !self.teardown().await {
            return Err(SyncError::other("还有在途的附件提交，稍后重试保存"));
        }
        let mut next = self.secrets.clone();
        if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
            if config.transport != "file" {
                if let Err(error) = next.set_storage_secret(&config, secret) {
                    self.open(false).await;
                    return Err(error);
                }
            }
        }
        let (paths, saved) = (self.shared.ctx.paths.clone(), next.clone());
        if let Err(error) = blocking(move || saved.save(&paths)).await {
            self.open(false).await;
            return Err(error);
        }
        self.secrets = next;
        self.config = config;
        if self.config.enabled {
            self.stop = false;
        }
        self.shared.set_space(
            self.secrets
                .is_paired()
                .then(|| self.secrets.space_id.clone()),
            self.secret_saved(),
        );
        if self.secrets.is_paired() {
            self.open(false).await;
        }
        Ok(())
    }

    /// 上次没存完的存储设置：`rclaw.toml` 在保存时已经先写成新的一套，secrets 是命令
    /// 失败没写上的那一半。按记录的配置把密码绑上、落盘（配置以记录为准——密码是按它
    /// 绑的），两边收敛到同一套，然后删记录。
    async fn resume_storage_intent(&mut self, locked: bool) -> Result<()> {
        let paths = self.shared.ctx.paths.clone();
        // 调用方（打开连接时的配置确认）已经持着协议锁时不再重复拿：同一进程对同一
        // 把文件锁再拿一次会永远等自己。否则非阻塞地拿：宿主正拿着协议锁在保存时这次
        // 跳过（它会把意图结算掉，或者下次启动再来）。
        let _lock = if locked {
            None
        } else {
            match blocking(move || StorageLock::try_acquire(&paths)).await? {
                Some(lock) => Some(lock),
                None => return Ok(()),
            }
        };
        let paths = self.shared.ctx.paths.clone();
        let Some(intent) = blocking(move || storage_intent_load(&paths)).await? else {
            return Ok(());
        };
        // 宿主没能把 `rclaw.toml` 补写成意图里的目标：先别绑定 secrets、也别删意图，
        // 否则下次启动只剩一份和 TOML 不一致的凭据，恢复依据就没了。
        if !same_storage(&self.config, &intent.config) {
            tracing::warn!("存储更新意图与当前配置的存储段不一致（TOML 还没补写），先保留");
            return Ok(());
        }
        let mut next = self.secrets.clone();
        if let Some(secret) = intent.secret.as_deref().filter(|secret| !secret.is_empty()) {
            if intent.config.transport != "file" {
                next.set_storage_secret(&intent.config, secret.to_string())?;
            }
        }
        let (paths, saved) = (self.shared.ctx.paths.clone(), next.clone());
        blocking(move || saved.save(&paths)).await?;
        self.secrets = next;
        // 只把存储设置搬过来：这期间别处改过的范围/通道以当前配置为准。
        self.config.apply_storage_from(&intent.config);
        let (paths, id) = (self.shared.ctx.paths.clone(), intent.id);
        blocking(move || storage_intent_clear_if(&paths, &id)).await?;
        self.shared.set_space(
            self.secrets
                .is_paired()
                .then(|| self.secrets.space_id.clone()),
            self.secret_saved(),
        );
        Ok(())
    }

    async fn confirm_metered(&mut self) -> Result<()> {
        let Some(session) = self.session.as_ref() else {
            return Err(self.unavailable());
        };
        let (ctx, space) = (self.shared.ctx.clone(), session.live.space_id.clone());
        blocking(move || {
            db::write(ctx.db(), |tx| {
                db::kv_put_json(tx, db::KV_METERED_OK, &space)
            })
        })
        .await?;
        self.hold_until = None;
        self.request(Kind::Full, Duration::ZERO);
        Ok(())
    }

    async fn set_backlog(&mut self, choice: Backlog) -> Result<()> {
        let Some(session) = self.session.as_ref() else {
            return Err(self.unavailable());
        };
        if !local::scopes_of(&self.config, self.shared.sessions_supported).attachments {
            return Err(SyncError::invalid("附件同步没有开启"));
        }
        let (ctx, space) = (self.shared.ctx.clone(), session.live.space_id.clone());
        blocking(move || {
            let initial = db::write(ctx.db(), |tx| {
                db::kv_put_json(tx, db::KV_BACKLOG, &choice)?;
                Ok(db::kv_json::<String>(tx, db::KV_INITIAL)?.as_deref() == Some(space.as_str()))
            })?;
            // 首次同步已经做完：现在就照选择排队。还没做完的话，收尾时再排。
            if initial {
                attach::queue_backlog(&ctx, choice)?;
            }
            Ok(())
        })
        .await?;
        self.shared.uploads.notify_one();
        self.shared.emit(SyncEvent::Status);
        Ok(())
    }
}
