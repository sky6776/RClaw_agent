//! 同步引擎：宿主拿在手里的句柄。
//!
//! 同步开着的时候，引擎有一条自己的线程（[`worker`]）：单线程的 tokio 运行时，一轮一轮地
//! 同步（[`round`]），附件的传输穿插在轮次之间（[`transfer`]）。宿主的线程——界面、turn
//! ——不等它，例外只有两类，而且都有上限或者是用户自己点的：发送前的那次拉取
//! （[`SyncEngine::presend`]，最多等 `presend_pull_wait_ms`），和面板上的操作（立即同步、
//! 处理待办事项、创建 / 加入 / 退出空间）。
//!
//! 同步关着的时候没有线程、不建同步表、不碰磁盘。用户自己点的本机操作（看回收站、从
//! 回收站恢复、导出导入会话包）不在此列：它们不需要同步开着，做的时候才读写对应的文件。
//!
//! 句柄上的方法分两种：`async` 的要经过引擎线程（排在两轮之间执行）；其余的直接在调用
//! 线程上做（[`local`]），只碰数据库和本机文件——它们是阻塞调用，宿主要从阻塞线程上调。

mod local;
mod round;
mod space;
mod transfer;
mod worker;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rclaw_store::{sync_db, MemoryStore, SessionStore};
use tokio::sync::{mpsc, oneshot, Notify};

use crate::attach::now_ms;
use crate::bundle::{self, ImportOptions, Imported};
use crate::config::{SyncConfig, SyncPaths};
use crate::db;
use crate::error::{Result, SyncError};
use crate::files;
use crate::pairing::PairingCode;
use crate::publish;
use crate::secrets::Secrets;
use crate::status::SyncStatus;
use crate::transport::net;
use crate::trash;
use crate::wire::Env;
use crate::{blocking, CronHost, Ctx, Reload, SessionGate, SkillHost, SyncEvent, SyncHost};

use transfer::FetchReq;
use worker::{Cmd, Reply};

pub use crate::attach::Backlog;
pub use crate::files::cron::RemoteJob;
pub use crate::files::memory::MemoryRestored;
pub use crate::transport::SelfTestStep;
pub use crate::trash::{MemoryTrashItem, TrashItem};
pub use local::{AttachmentInfo, SessionMark};
pub use space::{apply_pairing, pairing_config};
pub use worker::{
    storage_intent_clear_if, storage_intent_config, storage_intent_plan, storage_intent_save,
    IntentDeviceNameEdit, StorageIntentPlan, StorageLock,
};

/// 距上次拉取不到这么久就不再拉（发送前、窗口获得焦点、打开会话时）。
pub(crate) const PULL_FRESH_MS: i64 = 5_000;
/// 当前这一轮已经跑了这么久的话，发送前不等它（等也多半等不到）。
const ROUND_BUSY_MS: i64 = 3_000;
/// 引擎线程自己退出之后，距它启动满这么久才自动重新起一条（见 [`SyncEngine::live`]）。
const REVIVE_AFTER: Duration = Duration::from_secs(60);

/// 从宿主的配置文件重读 `[sync]`（见 [`SyncDeps::refresh_config`]）。`Err` = 文件在但
/// 这次读不出来（稍后重试）；`Ok(None)` = 还没有这个文件（按当前配置，与首次运行一致）。
pub type ConfigRefresh =
    Arc<dyn Fn() -> std::result::Result<Option<SyncConfig>, ()> + Send + Sync>;

/// 宿主注入的依赖。
pub struct SyncDeps {
    pub sessions: SessionStore,
    /// `rclaw.toml` 所在的目录（同步目录是它下面的 `sync/`）。
    pub data_root: PathBuf,
    pub memory: MemoryStore,
    pub gate: Arc<dyn SessionGate>,
    pub host: Arc<dyn SyncHost>,
    pub cron: Option<Arc<dyn CronHost>>,
    pub skills: Option<Arc<dyn SkillHost>>,
    /// 写进设备清单的平台名（`windows`、`android`……）与程序版本。
    pub platform: String,
    pub version: String,
    /// 宿主有没有会话列表、能不能打开同步来的会话。CLI / Termux 没有：只同步文件类数据。
    pub sessions_supported: bool,
    /// 内存紧张的设备（手机）：一次读写的批量小一些。
    pub low_memory: bool,
    /// 宿主侧的重读入口（有的话）：启动/待机接手前引擎会按磁盘上的 `rclaw.toml`
    /// 重读 `[sync]`，确认没有被同一数据目录里的另一个进程改掉。
    pub refresh_config: Option<ConfigRefresh>,
}

/// 宿主告诉引擎「发生了什么」，引擎自己决定要不要同步、什么时候同步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// 本机有东西要发布：会话改了名、归档了、设了目标，或者删了会话。
    LocalChange,
    /// 一个 turn 结束了（它的消息去抖之后发布）。不通知也行，引擎每几秒自己看一次。
    TurnEnded,
    /// 应用回到前台。
    Foreground,
    /// 应用转入后台：把手头的冲刷出去，此后按后台的间隔轮询。
    Background,
    /// 窗口获得焦点。
    Focus,
    /// 用户打开了一个会话：拉取一次，并把它末尾引用的附件取回来。
    OpenSession { channel: String, chat_id: String },
    /// 导入了会话包、从回收站恢复了会话：所有会话重新扫描一遍。
    Imported,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 引擎线程与句柄共用的状态。每次启动引擎线程新建一份：上一个线程收尾时写的东西不会
/// 串到新线程上。
pub(crate) struct Shared {
    pub ctx: Arc<Ctx>,
    pub platform: String,
    pub version: String,
    pub sessions_supported: bool,
    pub low_memory: bool,
    /// 宿主侧的重读入口（有的话）：启动/待机接手前按磁盘上的 `rclaw.toml` 重新取一份
    /// `[sync]`，确认配置没有被同一数据目录里的另一个进程改掉。`Err` = 文件在但这次
    /// 读不出来（稍后重试），`Ok(None)` = 还没有这个文件（按当前配置，与首次运行一致）。
    pub refresh_config: Option<ConfigRefresh>,
    /// 宿主与引擎线程共用的一份配置（桌面 + CLI 共用一个数据目录时，启动那一刻的
    /// 配置可能已被另一个进程更新；引擎线程从磁盘确认后会写回这里，宿主读到的就是
    /// 同一份）。
    pub config: Arc<Mutex<SyncConfig>>,
    /// 共享配置的版本（见 [`Inner::config_version`]）：引擎线程提交磁盘确认前复核。
    pub config_version: Arc<std::sync::atomic::AtomicU64>,
    /// 引擎线程实际安装过的配置序号：保存/加入的回执带着它，宿主只在「没有更新的
    /// 安装」时才发布自己那一份——发布顺序跟随 worker 的提交顺序，而不是回执到达顺序。
    pub config_seq: Arc<std::sync::atomic::AtomicU64>,
    view: Mutex<local::View>,
    /// 上次拉取成功的时刻（Unix 毫秒）。
    pub last_pull_ms: AtomicI64,
    /// 上一轮访问存储是成功的。
    pub storage_ok: AtomicBool,
    /// 正在跑的这一轮是什么时候开始的（Unix 毫秒）；没有在跑为 0。
    pub round_since_ms: AtomicI64,
    /// 已经创建或加入了同步空间。
    pub paired: AtomicBool,
    pub paused: AtomicBool,
    /// 正等着引擎线程回话的调用数。一轮同步看到有人在等，就把还能往后放的活留给下一轮。
    waiting: AtomicU32,
    /// 下载任务的入口；没有连上同步空间时为空。
    fetch: Mutex<Option<mpsc::UnboundedSender<FetchReq>>>,
    /// 叫醒上传任务。
    pub uploads: Notify,
    /// 附件任务请引擎马上跑一轮（有 `blob.want` 要发）。
    pub kick: Notify,
    /// 有点名要过的附件：以后的轮次隔一阵去取一次。
    pub wants: AtomicBool,
    /// 本机设置的附件大小上限（字节）。比它大的附件不取回（§11.3）；下载任务不经引擎
    /// 线程的命令循环，所以放在这里，配置改了由引擎线程更新。
    pub attachment_max: AtomicU64,
}

impl Shared {
    fn new(inner: &Inner, config: &SyncConfig) -> Shared {
        Shared {
            attachment_max: AtomicU64::new(config.attachment_max_bytes()),
            ctx: inner.ctx.clone(),
            platform: inner.platform.clone(),
            version: inner.version.clone(),
            sessions_supported: inner.sessions_supported,
            low_memory: inner.low_memory,
            refresh_config: inner.refresh_config.clone(),
            config: inner.config.clone(),
            config_version: inner.config_version.clone(),
            config_seq: inner.config_seq.clone(),
            view: Mutex::new(local::View {
                state: "syncing",
                ..local::View::default()
            }),
            last_pull_ms: AtomicI64::new(0),
            storage_ok: AtomicBool::new(true),
            round_since_ms: AtomicI64::new(0),
            paired: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            waiting: AtomicU32::new(0),
            fetch: Mutex::new(None),
            uploads: Notify::new(),
            kick: Notify::new(),
            wants: AtomicBool::new(false),
        }
    }

    pub(crate) fn emit(&self, event: SyncEvent) {
        self.ctx.host.emit(event);
    }

    /// 改面板上的状态；有变化才通知界面。
    pub(crate) fn set_state(&self, state: &'static str, message: Option<String>) {
        let changed = {
            let mut view = lock(&self.view);
            let changed = view.state != state || view.message != message;
            view.state = state;
            view.message = message;
            changed
        };
        if changed {
            self.emit(SyncEvent::Status);
        }
    }

    /// 本机所在的空间、存没存着存储的密码。
    pub(crate) fn set_space(&self, space_id: Option<String>, secret_saved: bool) {
        let changed = {
            let mut view = lock(&self.view);
            let changed = view.space_id != space_id || view.secret_saved != secret_saved;
            view.space_id = space_id;
            view.secret_saved = secret_saved;
            changed
        };
        if changed {
            self.emit(SyncEvent::Status);
        }
    }

    /// 完整地跑完了一轮。
    pub(crate) fn mark_synced(&self) {
        lock(&self.view).last_sync_ms = now_ms();
    }

    fn view(&self) -> local::View {
        lock(&self.view).clone()
    }

    /// 有调用在等引擎线程回话。
    pub(crate) fn interrupted(&self) -> bool {
        self.waiting.load(Ordering::Relaxed) > 0
    }

    pub(crate) fn set_fetch(&self, sender: Option<mpsc::UnboundedSender<FetchReq>>) {
        *lock(&self.fetch) = sender;
    }

    /// 排一个取回附件的请求。没有连上同步空间时返回 `false`。
    pub(crate) fn request_fetch(&self, blob: String, user: bool) -> bool {
        lock(&self.fetch)
            .as_ref()
            .is_some_and(|sender| sender.send(FetchReq { blob, user }).is_ok())
    }
}

/// 一次等引擎线程回话的调用。
struct Waiting(Arc<Shared>);

impl Waiting {
    fn enter(shared: &Arc<Shared>) -> Waiting {
        shared.waiting.fetch_add(1, Ordering::Relaxed);
        Waiting(shared.clone())
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 一条正在运行的引擎线程。命令通道的发送端只放在这里：句柄全部释放时通道关闭，线程
/// 随之退出。
struct Running {
    commands: mpsc::UnboundedSender<Cmd>,
    shared: Arc<Shared>,
    started: Instant,
}

impl Running {
    /// 引擎线程自己退出了：没能启动，或者中途出了没料到的错。正常停下（关掉同步、
    /// [`SyncEngine::stop`]）之前这一项已经先拿掉了，走不到这里。
    fn dead(&self) -> bool {
        self.commands.is_closed()
    }
}

struct Inner {
    ctx: Arc<Ctx>,
    platform: String,
    version: String,
    sessions_supported: bool,
    low_memory: bool,
    /// 宿主侧的重读入口：引擎线程在打开连接前按磁盘确认 `[sync]`。
    refresh_config: Option<ConfigRefresh>,
    config: Arc<Mutex<SyncConfig>>,
    /// 配置版本：宿主每次整份发布配置（`apply_config`、保存存储、加入空间）都 +1。
    /// 引擎线程从磁盘确认存储段前记下版本，提交合并时发现变了就重读，避免把较早读到
    /// 的存储段写回已经换了设置的那一份。
    config_version: Arc<std::sync::atomic::AtomicU64>,
    config_seq: Arc<std::sync::atomic::AtomicU64>,
    running: Mutex<Option<Running>>,
}

/// 引擎句柄。克隆出来的句柄指向同一个引擎。
#[derive(Clone)]
pub struct SyncEngine {
    inner: Arc<Inner>,
}

fn not_enabled() -> SyncError {
    SyncError::other("多端同步没有开启")
}

impl SyncEngine {
    /// 只组装，不碰磁盘、不起线程；[`SyncEngine::start`] 之后才开始同步。
    pub fn new(deps: SyncDeps, config: SyncConfig) -> SyncEngine {
        let ctx = Ctx {
            env: Env::new(&deps.sessions, &deps.data_root),
            paths: SyncPaths::new(&deps.data_root),
            layout: deps.sessions.layout().clone(),
            memory: deps.memory,
            gate: deps.gate,
            host: deps.host,
            cron: deps.cron,
            skills: deps.skills,
            space: Arc::new(crate::SpaceEpoch::default()),
        };
        SyncEngine {
            inner: Arc::new(Inner {
                ctx: Arc::new(ctx),
                platform: deps.platform,
                version: deps.version,
                sessions_supported: deps.sessions_supported,
                low_memory: deps.low_memory,
                refresh_config: deps.refresh_config,
                config: Arc::new(Mutex::new(config)),
                config_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                config_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                running: Mutex::new(None),
            }),
        }
    }

    /// 配置里开着同步就启动引擎线程。
    pub fn start(&self) {
        let config = self.config();
        let mut running = lock(&self.inner.running);
        if config.enabled && running.as_ref().is_none_or(Running::dead) {
            *running = self.spawn(config);
        }
    }

    /// 停掉引擎线程（它做完手头这一轮就退出，不等它）。
    pub fn stop(&self) {
        if let Some(running) = lock(&self.inner.running).take() {
            let _ = running.commands.send(Cmd::Stop);
        }
    }

    /// 配置交给引擎（用户在设置里改的，或者重新读了 `rclaw.toml`）。开关变了就启停引擎
    /// 线程，其余的交给它自己对比。
    ///
    /// 没变的配置也交过去：面板上的「重试」就是把现有的配置再存一遍，没连上同步空间的
    /// 引擎线程收到之后会再试一次。线程自己退出了的话重新起一条。
    pub fn apply_config(&self, config: SyncConfig) {
        // 兼容入口（测试/无文件协议）：直接发布。宿主侧应调用
        // [`Self::apply_config_locked`]——由它在协议锁内读盘得到权威配置再发布。
        self.apply_config_locked(config);
    }

    /// 在**已持有存储协议锁**时发布配置：写盘、读盘与发布在同一临界区里，较早读到的
    /// 快照不会盖过之后完成的保存。发布存储段变化会推进提交代次（旧意图失效）。
    pub fn apply_config_locked(&self, config: SyncConfig) {
        let paths = self.inner.ctx.paths.clone();
        let _ = worker::storage_commit_advance_if_changed(&paths, &config);
        let mut running = lock(&self.inner.running);
        let host_seq = {
            // 值与版本、安装序号在同一把锁里提交：引擎线程只会在锁内看到成对的
            // 「新值 + 新版本 + 新序号」。更早派发的保存/加入回执（序号更小）不能
            // 把这份发布覆盖回去；旧的重配命令带着小序号也不会再发布。
            let mut guard = lock(&self.inner.config);
            *guard = config.clone();
            self.inner.config_version.fetch_add(1, Ordering::Relaxed);
            self.inner.config_seq.fetch_add(1, Ordering::Relaxed) + 1
        };
        // 这次发布是一次显式改名：取消仍在等待的旧设备名编辑（凭据部分保留）。
        let _ = worker::storage_intent_cancel_name_edit(&paths, &config.device_name);
        if !config.enabled {
            if let Some(running) = running.take() {
                let _ = running.commands.send(Cmd::Stop);
            }
            return;
        }
        let delivered = running
            .as_ref()
            .is_some_and(|live| live.commands.send(Cmd::Reconfigure(config.clone(), host_seq)).is_ok());
        if !delivered {
            *running = self.spawn(config);
        }
    }

    pub fn config(&self) -> SyncConfig {
        lock(&self.inner.config).clone()
    }

    /// 引擎线程最近一次安装配置的序号（见 [`Inner::config_seq`]）。宿主用它判断
    /// 「我这次加入/保存是不是最后一次安装」，免得迟到的回写盖掉更新的设置。
    pub fn config_seq(&self) -> u64 {
        self.inner.config_seq.load(Ordering::Relaxed)
    }

    fn spawn(&self, config: SyncConfig) -> Option<Running> {
        let shared = Arc::new(Shared::new(&self.inner, &config));
        let (commands, receiver) = mpsc::unbounded_channel();
        match worker::spawn(shared.clone(), config, receiver) {
            Ok(()) => Some(Running {
                commands,
                shared,
                started: Instant::now(),
            }),
            Err(error) => {
                tracing::error!("同步引擎的线程没能启动: {error}");
                None
            }
        }
    }

    /// 在运行的引擎线程。
    ///
    /// 线程自己退出了的话（[`Running::dead`]）在这里重新起一条。`force`（用户点名要做的
    /// 操作）时马上起；否则要等到距它启动满 [`REVIVE_AFTER`]——一起来就出错的线程不会被
    /// 界面上的各种动静带着连轴转。
    fn live(&self, force: bool) -> Option<(mpsc::UnboundedSender<Cmd>, Arc<Shared>)> {
        let mut running = lock(&self.inner.running);
        let revive = running
            .as_ref()
            .is_some_and(|old| old.dead() && (force || old.started.elapsed() >= REVIVE_AFTER));
        if revive {
            let config = self.config();
            if config.enabled {
                tracing::warn!("同步引擎的线程已经退出，重新启动");
                *running = self.spawn(config);
            }
        }
        running
            .as_ref()
            .filter(|running| !running.dead())
            .map(|running| (running.commands.clone(), running.shared.clone()))
    }

    /// 引擎线程不在时，给调用方的说法。
    fn not_running(&self) -> SyncError {
        if self.config().enabled {
            SyncError::other("同步引擎没有在运行，请到「多端同步」页面点「重试」")
        } else {
            not_enabled()
        }
    }

    /// 通知引擎发生了什么。不阻塞；同步没开时什么都不做。
    pub fn notify(&self, trigger: Trigger) {
        if let Some((commands, _)) = self.live(false) {
            let _ = commands.send(Cmd::Trigger(trigger));
        }
    }

    /// 把一个命令交给引擎线程，等它在两轮之间执行完。
    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Cmd) -> Result<T> {
        let Some((commands, shared)) = self.live(true) else {
            return Err(self.not_running());
        };
        let (reply, answer) = oneshot::channel();
        let _waiting = Waiting::enter(&shared);
        commands.send(make(reply)).map_err(|_| self.not_running())?;
        answer
            .await
            .map_err(|_| SyncError::other("同步引擎已经停止"))?
    }

    // -- 经引擎线程的操作 -----------------------------------------------------

    /// 「立即同步」：马上完整地跑一轮，返回这一轮成没成。
    pub async fn sync_now(&self) -> Result<()> {
        self.call(Cmd::SyncNow).await
    }

    /// 发送消息之前调（§7.4）：如果这个会话参与同步、距上次拉取已经有一阵，先拉取一次，
    /// 让别的设备刚写的内容先落到本机，尽量不分叉。从进来算起最多等
    /// `presend_pull_wait_ms`（查这个会话参不参与同步的那次读库也算在里面），到点就返回，
    /// 已经开始的拉取在后台接着做。斜杠命令、不参与同步的会话、同步暂停或关着、上一次
    /// 访问存储失败时都不等。
    pub async fn presend(&self, channel: &str, chat_id: &str, text: &str) {
        if text.trim_start().starts_with('/') {
            return;
        }
        let (enabled, wait_ms) = {
            let config = lock(&self.inner.config);
            (config.enabled, config.presend_pull_wait_ms)
        };
        let Some((commands, shared)) = self.live(false) else {
            return;
        };
        if !enabled
            || !shared.paired.load(Ordering::Relaxed)
            || shared.paused.load(Ordering::Relaxed)
        {
            return;
        }
        // 续聊要用到的附件（会话末尾引用的、还在别的设备上的）顺带取回来。
        let _ = commands.send(Cmd::Prefetch {
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
        });
        let now = now_ms();
        let round_since = shared.round_since_ms.load(Ordering::Relaxed);
        if wait_ms == 0
            || !shared.storage_ok.load(Ordering::Relaxed)
            || now - shared.last_pull_ms.load(Ordering::Relaxed) < PULL_FRESH_MS
            || (round_since > 0 && now - round_since > ROUND_BUSY_MS)
        {
            return;
        }
        let pull = async {
            let eligible = {
                let (ctx, config) = (self.inner.ctx.clone(), self.config());
                let supported = self.inner.sessions_supported;
                let (channel, chat_id) = (channel.to_string(), chat_id.to_string());
                blocking(move || {
                    db::read(ctx.db(), |conn| {
                        if !sync_db::sync_tables_exist(conn)? {
                            return Ok(false);
                        }
                        let s = local::local_settings(conn, &config, supported)?;
                        publish::eligible(conn, &s, &channel, &chat_id)
                    })
                })
                .await
            };
            if !matches!(eligible, Ok(true)) {
                return;
            }
            let (reply, answer) = oneshot::channel();
            let _waiting = Waiting::enter(&shared);
            if commands.send(Cmd::Presend(reply)).is_ok() {
                let _ = answer.await;
            }
        };
        // 一个期限管两段：读库排在别的事务后面等久了，也不会让发送多等。
        let _ = tokio::time::timeout(Duration::from_millis(wait_ms), pull).await;
    }

    /// 暂停 / 恢复同步。暂停期间不跑轮次、不自动上传；用户点名要的附件照样取。
    pub async fn set_paused(&self, paused: bool) -> Result<()> {
        self.call(|reply| Cmd::Pause(paused, reply)).await
    }

    /// 处理一条待办事项：`accept` / `reject`（等确认的人格、技能、bot 改动）、`retry` /
    /// `discard`（被隔离的会话）、`dismiss`（提示）。
    ///
    /// `reviewed` 是用户看到的那一版（[`PendingItem::version`]，详情里的 `hlc`）。等确认的
    /// 改动必须带着它：事项的标识不随版本变，用户看着旧内容点「接受」的时候，等着的可能
    /// 已经是另一份没看过的内容了——对不上就拒绝，让用户重新看。
    pub async fn resolve_pending(
        &self,
        id: &str,
        action: &str,
        reviewed: Option<&str>,
    ) -> Result<()> {
        let (id, action) = (id.to_string(), action.to_string());
        let reviewed = reviewed.map(str::to_string);
        self.call(|reply| Cmd::Resolve {
            id,
            action,
            reviewed,
            reply,
        })
        .await
    }

    /// 把别的设备上的一个定时任务接管到本机（§8.6）。`force`：那台设备已经标成「已丢失」，
    /// 不等它确认。`reviewed` 是用户看到的那份定义的版本（[`RemoteJob::version`]）：那台
    /// 设备在这之后改过任务的话不办，让用户先看新的。
    pub async fn cron_takeover(
        &self,
        owner: &str,
        id: &str,
        force: bool,
        reviewed: &str,
    ) -> Result<()> {
        let (owner, id, reviewed) = (owner.to_string(), id.to_string(), reviewed.to_string());
        self.call(|reply| Cmd::Takeover {
            owner,
            id,
            force,
            reviewed,
            reply,
        })
        .await
    }

    /// 创建一个新的同步空间（本机是第一台设备）。
    pub async fn create_space(&self) -> Result<()> {
        self.call(Cmd::CreateSpace).await
    }

    /// 用配对码加入一个同步空间。返回加入之后的配置——配对码里带了存储设置的话与原来
    /// 不同，宿主负责把它写回 `rclaw.toml`（不必再调 [`SyncEngine::apply_config`]）。
    pub async fn join_space(&self, code: &str) -> Result<SyncConfig> {
        self.join_space_with_seq(code)
            .await
            .map(|(config, _)| config)
    }

    /// 同 [`Self::join_space`]，另外返回这次加入在引擎线程里的安装序号：宿主写回
    /// `rclaw.toml` 前用它确认「这仍是最近一次安装」，迟到的回写不会盖掉更新的设置。
    pub async fn join_space_with_seq(&self, code: &str) -> Result<(SyncConfig, u64)> {
        let code = code.trim().to_string();
        let host_seq = self.inner.config_seq.load(Ordering::Relaxed);
        let (joined, seq) = self
            .call(|reply| Cmd::JoinSpace {
                code,
                host_seq,
                reply,
            })
            .await?;
        {
            let mut guard = lock(&self.inner.config);
            if seq == self.inner.config_seq.load(Ordering::Relaxed) {
                *guard = joined.clone();
                self.inner.config_version.fetch_add(1, Ordering::Relaxed);
            } else {
                tracing::info!("加入空间的回执不是最新安装，跳过发布（由更新的那次提交发布）");
            }
        }
        Ok((joined, seq))
    }

    /// 退出同步空间：本机的密钥与存储凭据清掉，会话、记忆等数据原样留着。
    ///
    /// 同步关着的时候（没有引擎线程）也能退：本机可能还配着对，`/clear confirm local` 要
    /// 靠它把配对解掉。那时直接在这里做，先拿引擎锁——用同一个数据目录的另一个进程正在
    /// 同步的话，不能在它脚下把密钥和同步状态清掉。
    pub async fn leave_space(&self) -> Result<()> {
        if self.live(true).is_some() {
            // 在线路径把结果（成功或业务错误）原样交回：已经执行过的退出不能再走
            // 离线路径重复清理。关闭态的 worker 会在命令里自己走离线退出。
            return self.call(Cmd::LeaveSpace).await;
        }
        // 离线（同步从启动起就关着，没有引擎线程）：整个退出放到独立任务里执行到
        // 结算。先取引擎锁、再读 secrets 与待恢复记录——另一个进程正在退出/重建的
        // 中途，不能在它脚下清理本机状态。调用方对这次等待有 5 秒上限：future 被
        // 丢弃时任务仍在跑、仍持锁，远端标记（文件后端的 space.json）与本机写入都
        // 不会在锁释放之后继续——否则迟到的「创建空间」写入会把后来标成关闭的空间
        // 重新打开。
        let ctx = self.inner.ctx.clone();
        let config = self.config();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = async {
                let paths = ctx.paths.clone();
                let (lock, mut secrets) = blocking(move || {
                    let lock = worker::take_lock(&paths)?
                        .ok_or_else(|| SyncError::other(worker::STANDBY_HINT))?;
                    let secrets = Secrets::load(&paths)?;
                    Ok((std::sync::Arc::new(lock), secrets))
                })
                .await?;
                let result = space::leave(&ctx, &config, &mut secrets, Some(lock.clone())).await;
                drop(lock);
                result
            }
            .await;
            let _ = tx.send(result);
        });
        rx.await
            .unwrap_or_else(|_| Err(SyncError::other("退出同步空间的任务没有回话（稍后重试）")))
    }

    /// 重置同步空间（§11.2，设备丢失之后用）：换新空间、新密钥，其余设备要拿新的配对码
    /// 重新加入。
    pub async fn reset_space(&self) -> Result<()> {
        self.call(Cmd::ResetSpace).await
    }

    /// 本机所在空间的配对码。`include_storage`：带上存储的地址与凭据，新设备扫一下就能
    /// 用。配对码等同于密钥，只能显示给用户，不能写进日志、会话或记忆。
    pub async fn pairing_code(&self, include_storage: bool) -> Result<String> {
        self.call(|reply| Cmd::PairingCode {
            include_storage,
            reply,
        })
        .await
    }

    /// 保存存储的密码（WebDAV 的密码 / 对象存储的 Secret Key）。它只存在本机的
    /// `sync/secrets.json` 里，不进 `rclaw.toml`。
    pub async fn set_storage_secret(&self, secret: &str) -> Result<()> {
        let secret = secret.to_string();
        self.call(|reply| Cmd::SetSecret { secret, reply }).await
    }

    /// 存储的设置与密码一起保存并生效（面板「保存」用这个）：密码按这条命令里的配置
    /// 绑定存储账户，中间不会有别的配置变更插进来。`secret` 为空 = 不动已经存着的。
    ///
    /// 派发之前先把「更新意图」落盘：命令失败（worker 待机、secrets 写失败）时，
    /// `rclaw.toml` 已经是新的一套而 secrets 还是旧的，重启后的引擎会按记录把剩下的
    /// 一半补上。命令成功才把新配置公布给宿主、清掉意图——失败时宿主内存里仍是旧配置，
    /// 界面上看到的与实际生效的一致。
    pub async fn save_storage(&self, config: SyncConfig, secret: Option<String>) -> Result<()> {
        self.save_storage_with(config, secret, None, |config| Ok(config.clone()), |_| Ok(()))
            .await
    }

    /// 保存存储设置与密码的完整协议：`persist` 在这一段里把设置写回宿主的配置文件
    /// （`rclaw.toml`）。整段在一把跨进程锁里串行：先落「更新意图」→ 写配置文件 →
    /// 引擎按同一条请求绑定 secrets 并生效 → 按意图标识结算。中途失败留下意图，下次
    /// 启动由宿主与引擎收敛到同一套；两次保存不会交错出 TOML 与运行配置各一套的状态。
    pub async fn save_storage_with<M, W>(
        &self,
        config: SyncConfig,
        secret: Option<String>,
        // 这次请求**明确修改**的设备名目标（向导里编辑过）：原值取锁内合并时磁盘上
        // 的当前名——向导打开时的基线可能早已过期。`None` = 不负责设备名。
        explicit_device_name: Option<String>,
        merge: M,
        write: W,
    ) -> Result<()>
    where
        M: FnOnce(&SyncConfig) -> std::result::Result<SyncConfig, String> + Send + 'static,
        W: FnOnce(&SyncConfig) -> std::result::Result<(), String> + Send + 'static,
    {
        let paths = self.inner.ctx.paths.clone();
        let lock_paths = paths.clone();
        let _lock = blocking(move || worker::StorageLock::acquire(&lock_paths)).await?;
        let secret = secret.filter(|secret| !secret.is_empty());
        // 协议顺序：锁内先算出要提交的配置（宿主把它和磁盘上的最新一份合并），**先**
        // 耐久登记「更新意图」（含这次要绑的密码），再写 TOML、派发命令。意图没写上
        // 就停下：不能出现「TOML 已经是新存储、凭据却还绑在旧账户」而重启后无从恢复
        // 的状态。
        let mut merged = blocking(move || merge(&config).map_err(SyncError::other)).await?;
        // 明确改名：原值取锁内读到的磁盘名（不是向导打开时的旧基线），再把目标写进
        // 要落盘的配置。
        let device_edit = explicit_device_name.and_then(|to| {
            if merged.device_name == to {
                None
            } else {
                let edit = (merged.device_name.clone(), to.clone());
                merged.device_name = to;
                Some(edit)
            }
        });
        // 最后一份 TOML 已提交的记录先放进备份：新请求在这之后任何一步失败，启动
        // 都能按磁盘存储段找回它；备份都写不成就别动任何东西。
        if let Some(prev) = worker::storage_intent_last_written(&paths, &merged)? {
            worker::storage_intent_backup_save(&paths, &prev)?;
        }
        let id =
            worker::storage_intent_save(&paths, &merged, secret.as_deref(), device_edit)?;
        let written = {
            let target = merged.clone();
            blocking(move || write(&target).map_err(SyncError::other)).await
        };
        if let Err(error) = written {
            // 确定 TOML 没换成：删掉这次的主记录（它从没生效）；备份里的上一份仍
            // 可按磁盘存储段恢复。
            let _ = worker::storage_intent_clear_if(&paths, &id);
            return Err(error);
        }
        // TOML 已写成：存储段若变了，这里才推进提交代次并淘汰其它存储段的旧记录
        // （登记意图时没有动过它们）。
        let _ = worker::storage_commit_advance_if_changed(&paths, &merged);
        // 这一条从此有资格在启动时被采用（要等密码补上才结算）。
        if let Err(error) = worker::storage_intent_mark_written(&paths, &id) {
            tracing::warn!("记录存储设置的提交状态失败: {error}");
        }
        let config = merged;
        let seq = self
            .call(|reply| Cmd::SaveStorage {
                config: config.clone(),
                secret,
                reply,
            })
            .await?;
        // 密码已经绑定、配置已经生效：先删落选的备份，再删主记录（中途中断时
        // 胜出记录还在，下次启动接着结算同一笔）。
        worker::storage_intent_settle(&paths, false)?;
        {
            // 发布顺序跟随 worker 的安装顺序：回执号不是当前最新的安装（期间又装了
            // 别的配置）就不发布这一份，让最后安装的那条路径去发布。
            let mut guard = lock(&self.inner.config);
            if seq == self.inner.config_seq.load(Ordering::Relaxed) {
                *guard = config;
                self.inner.config_version.fetch_add(1, Ordering::Relaxed);
            } else {
                tracing::info!("保存存储设置的回执不是最新安装，跳过发布（由更新的那次提交发布）");
            }
        }
        Ok(())
    }

    /// 用户同意在按流量计费的网络下做首次同步（§8.8）。
    pub async fn confirm_metered(&self) -> Result<()> {
        self.call(Cmd::ConfirmMetered).await
    }

    /// 用户选了「同步已有附件」的范围（§8.8）。
    pub async fn set_backlog(&self, choice: Backlog) -> Result<()> {
        self.call(|reply| Cmd::SetBacklog { choice, reply }).await
    }

    /// 「测试连接」（§10）：用给定的配置——界面上正在填的那一份，还没保存也可以——把存储
    /// 要用到的操作逐个走一遍。`secret` 是界面上刚填的密码；不给就用本机存着的。跑在
    /// 调用方的运行时上，不占引擎线程。
    pub async fn test_connection(
        &self,
        config: &SyncConfig,
        secret: Option<&str>,
    ) -> Vec<SelfTestStep> {
        let paths = self.inner.ctx.paths.clone();
        let mut secrets = blocking(move || Secrets::load(&paths))
            .await
            .unwrap_or_default();
        if let Some(secret) = secret {
            let _ = secrets.set_storage_secret(config, secret.to_string());
        }
        space::test_connection(config, &secrets).await
    }

    /// 取一个 HTTPS 地址出示的证书指纹（自签证书的「信任这张证书」用，§10）。
    pub async fn probe_certificate(url: &str) -> Result<String> {
        let digest = net::probe_certificate(url).await?;
        Ok(net::format_fingerprint(&digest))
    }

    // -- 直接在调用线程上做的本机操作（阻塞） ---------------------------------

    /// 面板上的同步状态。同步没开时不碰数据库。
    pub fn status(&self) -> SyncStatus {
        let config = self.config();
        // 引擎线程不在：面板上显示「同步没有启动」和「重试」。线程是自己退出的话，把它
        // 最后留下的说法带上。
        let stopped = |last: Option<local::View>| local::View {
            state: "error",
            message: last
                .filter(|last| last.state == "error")
                .and_then(|last| last.message)
                .or_else(|| Some("同步引擎没有在运行".into())),
            ..local::View::default()
        };
        let view = match lock(&self.inner.running).as_ref() {
            Some(running) if running.dead() => stopped(Some(running.shared.view())),
            Some(running) => running.shared.view(),
            None if config.enabled => stopped(None),
            None => local::View::default(),
        };
        local::status(
            &self.inner.ctx,
            &config,
            self.inner.sessions_supported,
            view,
        )
    }

    /// 这台设备（这个数据目录）是否在一个同步空间里：创建或加入过，还没有退出。**阻塞**——
    /// 引擎没在这个进程里连着空间时，要读一次 `sync/secrets.json`。
    ///
    /// 同步关着、或者负责同步的是用同一个数据目录的另一个进程时，也算在：运行时的 `/clear`
    /// 据此要用户指明范围。那时候不指明就清空的话，记忆文件空了而合并基线还在，同步一
    /// 恢复（或者那个进程的下一轮）就会把每一条记忆都当成「本机删掉了」发给所有设备。
    pub fn in_space(&self) -> Result<bool> {
        let live = self
            .live(false)
            .is_some_and(|(_, shared)| shared.paired.load(Ordering::Relaxed));
        if live {
            return Ok(true);
        }
        let secrets = Secrets::load(&self.inner.ctx.paths)?;
        if secrets.space_id.is_empty() && secrets.key.is_empty() {
            return Ok(false);
        }
        secrets.space_key()?;
        Ok(true)
    }

    /// 一条待办事项的详情（等确认的改动的新旧内容，给界面做对比）。
    pub fn pending_detail(&self, id: &str) -> Result<serde_json::Value> {
        files::detail(&self.inner.ctx, id)
    }

    /// 删除一个会话（§7.5）。已经发布过的会话：删除会传播到其他设备，本机先留一份在
    /// 回收站；`purge`（「彻底删除」）不留，其他设备也不进回收站。返回会话原来在不在。
    /// 会话正忙时最多等两秒，然后报错。
    pub fn delete_session(&self, channel: &str, chat_id: &str, purge: bool) -> Result<bool> {
        // 回收站留不留只看保留天数，不看同步此刻开没开：发布过的会话，删除登记在库里，
        // 同步一恢复就传出去，所以关着的时候删也要留得回来。
        let keep_trash = self.config().trash_days > 0;
        let existed = local::delete_session(&self.inner.ctx, channel, chat_id, purge, keep_trash)?;
        self.notify(Trigger::LocalChange);
        Ok(existed)
    }

    /// `/clear confirm all`（§7.5）第一半：把参与同步、已经发布的会话逐个删除并传播出去，
    /// 本机各留一份在回收站。返回登记删除的会话数。其余会话由调用方接着清。
    ///
    /// 记忆的清空拆在 [`Self::stash_memory_for_clear`]（清空之前留副本）与
    /// [`Self::commit_memory_clear`]（本地清空成功后才登记为可发送的删除）两步：本地清空
    /// 中途失败的话，记忆的删除不能已经发出去。
    ///
    /// 调用方必须保证这期间没有 turn 在跑、合入也进不来（运行时持着静默门调用）：这里不
    /// 逐个去占会话。
    pub fn delete_published_sessions(&self) -> Result<usize> {
        let config = self.config();
        let keep_trash = config.trash_days > 0;
        let deleted = local::delete_published(&self.inner.ctx, &config, self.inner.sessions_supported, keep_trash)?;
        self.notify(Trigger::LocalChange);
        Ok(deleted)
    }

    /// `/clear confirm all`（§7.5）第二半的前一步：把本机 `MEMORY.md` 现有的条目在回收站
    /// 里留一份，并登记「正在清理」的意图，返回条目标识与这次登记的操作标识。没有可发布
    /// 的副作用：意图先耐久落库，调用方才删本机文件。
    pub fn stash_memory_for_clear(&self) -> Result<(Vec<String>, String)> {
        let config = self.config();
        let keep_trash = config.trash_days > 0;
        // 同步关着时「等同步连上再清」永远等不到：按没加入空间处理（本机删除不传播）。
        let in_space = config.enabled && self.in_space()?;
        local::stash_memory_clear(&self.inner.ctx, keep_trash, in_space)
    }

    /// `/clear confirm all`（§7.5）第二半的后一步：本地清空已成功，把清空登记成发给所有
    /// 设备的删除。`ids` / `op` 是 [`Self::stash_memory_for_clear`] 返回的那些。记忆范围
    /// 关着时没有可发布的删除，只明确结束这次清理的意图（不留下挡住对比的标记）。
    pub fn commit_memory_clear(&self, ids: &[String], op: &str) -> Result<usize> {
        let config = self.config();
        let committed = local::commit_memory_clear(
            &self.inner.ctx,
            &config,
            self.inner.sessions_supported,
            ids,
            op,
        )?;
        self.notify(Trigger::LocalChange);
        Ok(committed)
    }

    /// 标记 / 取消「仅本机」（§7.9）。
    pub fn set_local_only(&self, channel: &str, chat_id: &str, on: bool) -> Result<()> {
        local::set_local_only(&self.inner.ctx, channel, chat_id, on)?;
        self.notify(Trigger::LocalChange);
        Ok(())
    }

    pub fn is_local_only(&self, channel: &str, chat_id: &str) -> Result<bool> {
        local::is_local_only(&self.inner.ctx, channel, chat_id)
    }

    /// 会话列表的角标：仅本机、已同步、自动分出的分支、正在别的设备上运行。
    pub fn session_marks(&self) -> Result<Vec<SessionMark>> {
        local::session_marks(
            &self.inner.ctx,
            &self.config(),
            self.inner.sessions_supported,
        )
    }

    /// 一个会话里各条消息引用的附件在本机的情况（§8.7）。
    pub fn attachments(&self, channel: &str, chat_id: &str) -> Result<Vec<AttachmentInfo>> {
        local::attachments(&self.inner.ctx, channel, chat_id)
    }

    /// 本机缓存的一份附件现在还是不是它登记的那个对象。打开缓存副本的入口用它拦下被
    /// 原地改写的文件：内容不符就不当「有」，用户重新下载会走缺附件的路径。旧版本按
    /// 明文摘要登记的内联缓存按本机文件内容归一（换成线上对象名），不判坏。验不了
    /// （没配对、读不出密钥、没有登记）时不算坏——那有别的说法。
    pub fn cached_attachment_intact(&self, blob: &str) -> bool {
        let secrets = match Secrets::load(&self.inner.ctx.paths) {
            Ok(secrets) => secrets,
            Err(_) => return true,
        };
        let Ok(keys) = secrets.keys() else {
            return true;
        };
        matches!(
            crate::attach::check_local_object(&self.inner.ctx, &keys, blob),
            Ok(crate::attach::LocalObject::Intact(_)) | Ok(crate::attach::LocalObject::Unknown)
        )
    }

    /// 用户点了一个还没下载的附件：排到下载队列的最前面。结果经
    /// [`SyncEvent::Attachment`] 通知。
    pub fn fetch_attachment(&self, blob: &str) -> Result<()> {
        match self.live(true) {
            Some((_, shared)) if shared.request_fetch(blob.to_string(), true) => Ok(()),
            _ => Err(SyncError::other("还没有连上同步空间，取不了这个附件")),
        }
    }

    // 回收站（§7.5）不看同步此刻开没开、在不在同步空间里：退出同步空间之后，里面的东西
    // 照样列得出来、恢复得了。

    /// 回收站里的会话。列之前先把过了保留期的清掉：同步关着、或者已经退出同步空间的时候
    /// 没有引擎线程替它清，用户打开回收站的这一刻就是清理的时机。
    pub fn trash_list(&self) -> Vec<TrashItem> {
        let paths = &self.inner.ctx.paths;
        trash::sweep(paths, self.config().trash_days);
        trash::list(paths)
    }

    /// 从回收站恢复一个会话：作为新线程回到本机，并发布给其他设备。
    pub fn trash_restore(&self, id: &str) -> Result<Imported> {
        let imported = trash::restore(&self.inner.ctx, id)?;
        self.notify(Trigger::Imported);
        Ok(imported)
    }

    pub fn trash_remove(&self, id: &str) -> Result<()> {
        trash::remove(&self.inner.ctx.paths, id)
    }

    /// 回收站里的记忆条目：因为同步而从本机的记忆文件里删掉的那些（别的设备删的，或者
    /// `/clear confirm all` 清空的）。过了保留期的由 [`Self::trash_list`] 清。
    pub fn trash_memory_list(&self) -> Vec<MemoryTrashItem> {
        trash::list_memory(&self.inner.ctx.paths)
    }

    /// 把回收站里的一份记忆条目放回它原来所在的记忆文件。同步开着的话，放回去的条目随后
    /// 作为本机新写的内容发给其他设备。
    pub fn trash_memory_restore(&self, id: &str) -> Result<MemoryRestored> {
        let restored = trash::restore_memory(&self.inner.ctx, id)?;
        let host = &self.inner.ctx.host;
        if restored.restored > 0 {
            host.reload(Reload::Memory);
            self.notify(Trigger::LocalChange);
        }
        // 指着这份副本的提示可能撤掉了。
        host.emit(SyncEvent::Status);
        Ok(restored)
    }

    pub fn trash_memory_remove(&self, id: &str) -> Result<()> {
        trash::remove_memory(&self.inner.ctx.paths, id)?;
        self.inner.ctx.host.emit(SyncEvent::Status);
        Ok(())
    }

    /// 把一台设备标成「已丢失」（它的定时任务可以被强制接管，§8.6），或取消。
    pub fn mark_lost(&self, device: &str, lost: bool) -> Result<()> {
        local::mark_lost(&self.inner.ctx, device, lost)?;
        if let Some((_, shared)) = self.live(false) {
            shared.emit(SyncEvent::Status);
        }
        Ok(())
    }

    /// 别的设备上的定时任务（只读镜像，§8.6）。
    pub fn remote_cron(&self) -> Result<Vec<RemoteJob>> {
        local::remote_cron(
            &self.inner.ctx,
            &self.config(),
            self.inner.sessions_supported,
        )
    }

    /// 从别的设备同步来、现在装在本机的技能：`(技能名, 来源设备的展示名)`（§8.4）。界面在
    /// 技能列表里标出来源——依赖 shell 的技能到了手机上不一定能跑，同步不替用户过滤。
    pub fn skill_origins(&self) -> Result<Vec<(String, String)>> {
        local::skill_origins(&self.inner.ctx)
    }

    /// 把一个会话导出成会话包（§6）。不需要开启同步。
    pub fn export_session(&self, channel: &str, chat_id: &str) -> Result<String> {
        let text = bundle::export_session(&self.inner.ctx.env, channel, chat_id)?;
        // 导出给还没发布的行分配了身份：让下一次发布扫描把它们捡起来。
        self.notify(Trigger::LocalChange);
        Ok(text)
    }

    /// 导入一个会话包（§6）。不需要开启同步；开着的话导入的会话随后发布给其他设备。
    pub fn import_session(&self, text: &str, options: &ImportOptions) -> Result<Imported> {
        let imported = bundle::import_session(&self.inner.ctx.env, text, options)?;
        self.notify(Trigger::Imported);
        Ok(imported)
    }
}

/// 用这个配对码加入之前，要不要先向用户要存储的密码：配对码里没带存储设置、配置的存储
/// 又需要密码、而本机没存着。配对码格式不对在这里就报出来。
pub fn join_needs_secret(data_root: &Path, config: &SyncConfig, code: &str) -> Result<bool> {
    if PairingCode::parse(code.trim())?.storage.is_some() {
        return Ok(false);
    }
    let secrets = Secrets::load(&SyncPaths::new(data_root))?;
    Ok(match config.transport.as_str() {
        "webdav" => {
            !config.webdav.username.trim().is_empty() && secrets.storage_secret(config).is_empty()
        }
        "s3" => secrets.storage_secret(config).is_empty(),
        _ => false,
    })
}

/// 没有引擎在跑的时候加入同步空间（CLI 的 `--sync-join`）：核对配对码、把密钥存到本机，
/// 返回加入之后的配置，调用方写回 `rclaw.toml`。`secret` 是用户刚输入的存储密码（见
/// [`join_needs_secret`]）；配对码里带了存储设置时以配对码为准。同一个数据目录上有 RClaw
/// 正在同步时拒绝，让用户到那个进程里去加入。
pub async fn join_space_offline(
    data_root: &Path,
    config: &SyncConfig,
    code: &str,
    secret: Option<&str>,
) -> Result<SyncConfig> {
    let paths = SyncPaths::new(data_root);
    let held = {
        let paths = paths.clone();
        blocking(move || Ok(worker::take_lock(&paths)?)).await?
    };
    let Some(_held) = held else {
        return Err(SyncError::other(
            "这个数据目录上有 RClaw 正在运行并同步：请在运行着的应用里加入同步空间，或者先退出它",
        ));
    };
    let mut secrets = {
        let paths = paths.clone();
        blocking(move || Secrets::load(&paths)).await?
    };
    if let Some(secret) = secret {
        secrets.set_storage_secret(config, secret.to_string())?;
    }
    // 重复执行（上次已经把密钥存下了，但调用方没能把配置写回去）：还是同一个空间就再走
    // 一遍，不算「已经在别的空间里」。
    if secrets.is_paired() {
        let parsed = PairingCode::parse(code.trim())?;
        if parsed.space_id == secrets.space_id && parsed.key == secrets.key {
            secrets.space_id.clear();
            secrets.key.clear();
        }
    }
    let plan = space::join_verify(&paths, config, &secrets, code.trim()).await?;
    let lock_paths = paths.clone();
    let _lock = blocking(move || worker::StorageLock::acquire(&lock_paths)).await?;
    plan.commit(&mut secrets).await
}

#[cfg(test)]
mod tests;
