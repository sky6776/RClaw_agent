//! 多端同步（`docs/multi-device-sync.md`）。
//!
//! 每台设备把自己的变更写成一条只追加、端到端加密的 op 日志，经一个被动的存储（网盘、
//! 自建文件服务器、对象存储）传给其他设备；其他设备取回后按规则合入本机。没有中枢、没有
//! 推送、不监听任何端口。存储一侧不运行任何 RClaw 的代码。
//!
//! 依赖方向：`rclaw-cli → {rclaw-runtime, rclaw-sync} → {rclaw-core, rclaw-store}`。本
//! crate 不依赖 `rclaw-runtime`：与运行中的 turn 互斥靠宿主注入的 [`SessionGate`]。

use std::sync::Arc;

use rclaw_store::{Db, Layout, MemoryStore};
use serde::Serialize;

pub mod attach;
pub mod bundle;
pub mod codec;
pub mod config;
pub mod crypto;
pub mod db;
pub mod engine;
pub mod error;
pub mod files;
pub mod identity;
pub mod ids;
pub mod map;
pub mod merge;
pub mod op;
pub mod pairing;
pub mod portable;
pub mod publish;
pub mod pull;
pub mod remote;
pub mod seal;
pub mod secrets;
pub mod snapshot;
pub mod status;
pub mod transport;
pub mod trash;
pub mod upload;
pub mod wire;

#[cfg(test)]
mod shape;
#[cfg(test)]
pub(crate) mod testing;

/// 把阻塞的活（数据库事务、磁盘读写、加解密）放到阻塞线程池上跑，不占着引擎自己的
/// 线程——附件传输与它共用一个运行时。
pub(crate) async fn blocking<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| SyncError::other(format!("后台任务失败: {e}")))?
}

/// 同 [`blocking`]，给只碰数据库和本机文件的活用：闭包拿到的是 `ctx` 的一份克隆（里面
/// 都是路径和共享句柄）。库锁是和会话读写共用的，等它、等磁盘都不能占着引擎的线程。
pub(crate) async fn offload<T, F>(ctx: &Ctx, f: F) -> Result<T>
where
    F: FnOnce(&Ctx) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let ctx = ctx.clone();
    blocking(move || f(&ctx)).await
}

/// 文件锁的「已被占用」判定：Windows 上 `fs2` 的竞争错误是 `ERROR_LOCK_VIOLATION`
/// （33），std 把它归到 `Uncategorized`，只按 `WouldBlock` 判断会被当成真实 I/O 错误。
pub(crate) fn lock_is_contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

pub use bundle::{export_session, import_session, ImportOptions, Imported};
pub use config::{Scopes, SyncConfig, SyncPaths};
pub use engine::{
    pairing_config, storage_intent_clear_if, storage_intent_config, storage_intent_plan,
    storage_intent_save, AttachmentBytes, AttachmentReadCheck, AttachmentStage,
    ConfigRefresh, IntentDeviceNameEdit, StorageIntentPlan, StorageLock, SyncDeps, SyncEngine,
};
pub use error::{Result, SyncError, TransportError};
pub use pairing::PairingCode;
pub use secrets::Secrets;
pub use status::{PendingItem, SyncStatus};
pub use wire::Env;

/// 与运行中的 turn 互斥（§7.3）。由宿主用运行时的 `SessionLifecycle` 实现；本 crate 只
/// 通过它占住会话、询问会话是否正忙。
pub trait SessionGate: Send + Sync {
    /// 占住一个会话做合入：这个会话上没有会改历史的活动 turn 时登记并返回守卫，守卫
    /// 释放即注销；会话正忙、或运行时正在做清理维护时返回 `None`。登记期间运行时不在
    /// 这个会话上启动会改历史的 turn。
    fn begin_merge(&self, channel: &str, chat_id: &str) -> Option<Box<dyn Send>>;

    /// 会话上是否有会改历史的活动 turn。
    fn is_busy(&self, channel: &str, chat_id: &str) -> bool;

    /// turn 已经运行了一段时间（超过 10 秒）的会话，写进 head 的运行提示（§7.4）。
    fn running_sessions(&self) -> Vec<(String, String)>;

    /// 同步改动了一个会话（合入了消息，或把它删了）：运行时清掉这个会话的运行期状态
    /// （usage 锚点、压缩防抖等）。
    fn session_changed(&self, channel: &str, chat_id: &str, deleted: bool);

    /// 在没有任何 turn、后台 review 运行的时候执行 `f`，执行期间也不会有新的启动（与
    /// 技能 curator 持同一道静默门，§8.4）。现在不空闲就不执行，返回 `false`，调用方留到
    /// 下一轮。`f` 里只做本地文件操作，要快。
    fn when_quiet(&self, f: &mut dyn FnMut()) -> bool;
}

/// 没有运行时的宿主（测试、只做文件类同步的 CLI）：没有会话会正忙。
pub struct NoGate;

impl SessionGate for NoGate {
    fn begin_merge(&self, _channel: &str, _chat_id: &str) -> Option<Box<dyn Send>> {
        Some(Box::new(()))
    }

    fn is_busy(&self, _channel: &str, _chat_id: &str) -> bool {
        false
    }

    fn running_sessions(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn session_changed(&self, _channel: &str, _chat_id: &str, _deleted: bool) {}

    fn when_quiet(&self, f: &mut dyn FnMut()) -> bool {
        f();
        true
    }
}

/// 定时任务调度器的入口（§8.6）。任务表在调度器的内存里，同步只能经它读写。
pub trait CronHost: Send + Sync {
    /// 本机任务表的快照。
    fn jobs(&self) -> Vec<rclaw_store::CronJob>;

    /// 把一个任务加进本机任务表（从别的设备接管来的）。调度器会重新分配 id、重算下次
    /// 触发时间；返回新的 id。
    fn add(&self, job: rclaw_store::CronJob) -> std::result::Result<String, String>;

    /// 把一个任务加进本机任务表，但**保持停用**（§8.6）：所有权还没有明确落到本机
    /// （原归属设备的落定回执还没发出来）时用它。原归属设备等不到本机落定会把任务收回
    /// 去，本机这一份要是已经启用了就会两边同时执行。落定之后用 [`CronHost::set_enabled`]
    /// 启用。返回新的 id。
    fn add_disabled(&self, job: rclaw_store::CronJob) -> std::result::Result<String, String>;

    /// 启用 / 停用一个已经在任务表里的任务（接管落定后启用）。
    fn set_enabled(&self, id: &str, enabled: bool) -> std::result::Result<(), String>;

    /// 从本机任务表移除一个任务。返回它原来在不在。
    fn remove(&self, id: &str) -> std::result::Result<bool, String>;
}

/// 一个名字在本机技能目录里的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillKind {
    /// 内置技能：随各端二进制下发，不参与同步。
    Builtin,
    /// 已归档（被 curator 或用户停用）：同步不动它，恢复之后再继续。
    Archived,
    /// 正常存在的用户技能 / agent 自建技能。
    Live {
        /// 被固定或保护：远端的改动要在本机确认。
        pinned: bool,
    },
    /// 本机没有。
    Absent,
}

/// 技能目录（`Layout::skills_dir`）的入口（§8.4）。归属、生命周期锁、归档都在工具层，
/// 由宿主注入。
pub trait SkillHost: Send + Sync {
    /// 参与同步的技能名：正常存在的用户技能与 agent 自建技能。技能目录读不了时必须报错，
    /// 不能返回空表——空表会被当成「全删了」。
    fn syncable(&self) -> std::result::Result<Vec<String>, String>;

    fn kind(&self, name: &str) -> SkillKind;

    /// turn 之外还有后台任务正在用技能目录（静默门管不到它们）：这时不换版本、不移走
    /// 技能，留到下一轮。
    fn in_use(&self) -> bool {
        false
    }

    /// 备新版本的地方。必须和技能目录在同一个卷上（换上去是一次改名，不是整份复制），
    /// 而且里面的东西不会被当成技能。引擎在它下面给每个要装的版本建一个子目录，用完就
    /// 删；进程中途退出留下的由 [`SkillHost::recover`] 清。
    fn staging_dir(&self) -> std::path::PathBuf;

    /// 每一轮对比开头调一次（不在静默门里）：上次换版本换到一半被打断的技能，把它恢复成
    /// 完整的旧版本；备料目录里没人要的东西清掉。返回技能目录有没有因此变化（宿主要重新
    /// 加载技能列表）。平时没有要收拾的，必须很便宜。
    fn recover(&self) -> bool {
        false
    }

    /// 用 `staged` 目录（在 [`SkillHost::staging_dir`] 下）的内容作为技能 `name` 的新
    /// 版本：原有版本移入归档，本机专有的文件（[`files::skills::is_local_only`]：密钥、
    /// 配置、固定标记等）带到新版本里。`staged` 由实现方移走或删掉都可以。调用方已持
    /// 静默门，所以只能是几次改名的事，不能把技能整份复制一遍。被打断时技能目录里的这个
    /// 技能必须是完整的旧版本、完整的新版本，或者不在——而「不在」的那一刻要能被
    /// [`SkillHost::recover`] 认出来并恢复，恢复之前 [`SkillHost::kind`] 报
    /// [`SkillKind::Archived`]（同步不动它，更不把它当成用户删了技能发布出去）。
    fn install(&self, name: &str, staged: &std::path::Path) -> std::result::Result<(), String>;

    /// 把技能移入归档（可恢复）。返回它原来在不在。调用方已持静默门。
    fn remove(&self, name: &str) -> std::result::Result<bool, String>;

    /// 一个版本装好之后调（不在静默门里）：清掉这个技能被同步换下来的旧版本里较早的
    /// 那些。不然每同步来一个版本，归档里就多留一份完整的技能。
    fn prune(&self, _name: &str) {}
}

/// 同步改了本机的哪一类数据，宿主据此重新加载。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reload {
    Memory,
    /// 人格文件被改写了（生效人格的改动在下次启动时生效）。
    Persona,
    Skills,
    Bots,
    Cron,
}

/// 引擎发给宿主的事件。界面把它当作「该重新取数了」的提示，内容以重新查询为准。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncEvent {
    /// 同步状态变了（进行中 / 空闲 / 出错 / 待处理事项）。
    Status,
    /// 这些会话的内容或属性被同步改动了；`list` 为真表示会话列表本身也变了（新增、
    /// 删除、改名、归档）。
    Sessions { chats: Vec<ChatKey>, list: bool },
    /// 一个附件的取回有了着落，界面该重新查一遍它的情况（`AttachmentInfo::state`）。
    /// `error` 为空：已经取回到本机；或者存储上没有这个文件，本机记下了它现在的情况
    /// （在等别的设备传上来、取不回来了）。不为空：用户点名要的那个这一次没取成（存储
    /// 够不着之类），内容是给用户看的原因。
    Attachment { blob: String, error: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct ChatKey {
    pub channel: String,
    pub chat_id: String,
}

/// 宿主提供的环境。
pub trait SyncHost: Send + Sync {
    /// 当前网络是否按流量计费（移动数据）。桌面端恒为否。只用于两处：后台轮询只在非
    /// 计费网络下进行、计费网络下的首次同步要用户确认。附件的上传下载不看它。
    fn is_metered(&self) -> bool {
        false
    }

    /// 事件通知。不能阻塞。
    fn emit(&self, _event: SyncEvent) {}

    /// 同步改了某类数据，请重新加载。不能阻塞。
    fn reload(&self, _what: Reload) {}
}

/// 什么都不做的宿主。
pub struct NoHost;

impl SyncHost for NoHost {}

/// 引擎各部分共用的本机环境。
#[derive(Clone)]
pub struct Ctx {
    pub env: Env,
    pub paths: SyncPaths,
    pub layout: Layout,
    pub memory: MemoryStore,
    pub gate: Arc<dyn SessionGate>,
    pub host: Arc<dyn SyncHost>,
    /// 定时任务调度器；宿主没有调度器（或没注入）时定时任务不参与同步。
    pub cron: Option<Arc<dyn CronHost>>,
    /// 技能目录；没注入时技能不参与同步。
    pub skills: Option<Arc<dyn SkillHost>>,
    /// 空间的代与已经开始的提交阶段（[`SpaceEpoch`]）：换空间、清理时拆引擎的任务，
    /// 但已经跑起来的阻塞阶段（落文件、记账）要等它结束、并且发现代变了就丢弃结果。
    pub space: Arc<SpaceEpoch>,
}

/// 空间的代（换空间 / 清理时 +1）、提交入口的开关，以及已经开始的提交阶段。
///
/// 引擎拆任务只拆得掉异步的那一层：`spawn_blocking` 已经开始的部分拦不住。它们动本机
/// 文件（落点、别名）和库里的附件登记；换空间之后旧空间的登记与落点都不该再写。
///
/// 换空间的顺序是：[`SpaceEpoch::seal`] 关掉提交入口（之后开始的阶段一律作废，旧任务
/// 只能用开始时捕获的代）→ [`SpaceEpoch::wait_idle`] 等在途的阶段走完（它们在切换**之前**
/// 正常提交，写的是旧空间的落点与账目，不算越过边界）→ 等不到才 [`SpaceEpoch::bump`]
/// 换代，把没走完的阶段与新的状态隔离开：它们随后的核对都会失配，结果丢弃，只收拾
/// 自己写下的东西。连上新空间之后 [`SpaceEpoch::reopen`]。
#[derive(Default)]
pub struct SpaceEpoch {
    epoch: std::sync::atomic::AtomicU64,
    inflight: std::sync::atomic::AtomicUsize,
    /// 正在最终写入段（改名、记账、销账）里的任务数，见 [`SpaceEpoch::enter`]。
    writers: std::sync::atomic::AtomicUsize,
    /// 提交入口关着：换空间 / 清理期间不受理新的提交阶段。
    sealed: std::sync::atomic::AtomicBool,
}

/// 入口关着时开始阶段的代：任何真实的代都到不了它。
const SEALED_EPOCH: u64 = u64::MAX;

impl SpaceEpoch {
    pub fn epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 换空间 / 清理：代 +1。已经挂号的阶段随后的核对都会失配。
    pub fn bump(&self) -> u64 {
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }

    /// 关掉提交入口：之后开始的阶段一律作废。已经在途的照常走完。
    pub fn seal(&self) {
        self.sealed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// 连上了新的空间：重新打开提交入口。
    pub fn reopen(&self) {
        self.sealed.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// 开始一个提交阶段（落文件、记账）：结算之前 `teardown` 会等它。入口关着时拿到的
    /// 阶段从一开始就作废。
    pub fn begin(self: &Arc<Self>) -> Commit {
        self.inflight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let open = !self.sealed.load(std::sync::atomic::Ordering::SeqCst);
        Commit {
            epoch: if open { self.epoch() } else { SEALED_EPOCH },
            space: self.clone(),
        }
    }

    /// 等已经开始的提交阶段都结束。超时返回假（调用方接着换代：没走完的会被隔离，
    /// 结果不会生效）。
    pub async fn wait_idle(&self, timeout: std::time::Duration) -> bool {
        let started = std::time::Instant::now();
        while self.inflight.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            if started.elapsed() >= timeout {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        true
    }

    /// 等已经进入最终写入段（改名、记账、销账）的任务退场。换代之前必须等它们：许可之内
    /// 的写入不会被换代越过。
    pub async fn wait_writers(&self, timeout: std::time::Duration) -> bool {
        let started = std::time::Instant::now();
        while self.writers.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            if started.elapsed() >= timeout {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        true
    }

    /// 进入最终写入段：与换代互斥。入口关着（或这期间关掉）时拿到的许可无效，调用方
    /// 不能写。许可活着的整段时间里 `bump` 不会发生。
    pub fn enter(&self) -> WritePermit<'_> {
        self.writers
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let open = !self.sealed.load(std::sync::atomic::Ordering::SeqCst);
        WritePermit { space: self, open }
    }
}

/// 一个最终写入段（见 [`SpaceEpoch::enter`]）。
pub struct WritePermit<'a> {
    space: &'a SpaceEpoch,
    open: bool,
}

impl WritePermit<'_> {
    /// 还能不能写：开始之后入口又被关上（换空间开始）时也不行。
    pub fn ok(&self) -> bool {
        self.open && !self.space.sealed.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for WritePermit<'_> {
    fn drop(&mut self) {
        self.space
            .writers
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 一个已经开始的提交阶段。`valid()` 为假 = 期间换了空间（或者开始时入口已经关了），
/// 结果要丢弃。
pub struct Commit {
    epoch: u64,
    space: Arc<SpaceEpoch>,
}

impl Commit {
    pub fn valid(&self) -> bool {
        self.space.epoch() == self.epoch
    }

    /// 这一刻的代的只读快照：交给阻塞闭包核对用。它不占 `inflight` 的名额（`Commit`
    /// 自身留在异步一侧守着），可以克隆。
    pub fn token(&self) -> EpochToken {
        EpochToken {
            epoch: self.epoch,
            space: self.space.clone(),
        }
    }
}

impl Drop for Commit {
    fn drop(&mut self) {
        self.space
            .inflight
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 一个提交阶段的代的只读快照（见 [`Commit::token`]）。
#[derive(Clone)]
pub struct EpochToken {
    epoch: u64,
    space: Arc<SpaceEpoch>,
}

impl EpochToken {
    /// 阶段还有效吗（期间没有换空间）。
    pub fn valid(&self) -> bool {
        self.space.epoch() == self.epoch
    }
}

impl Ctx {
    pub fn db(&self) -> &Db {
        self.env.db()
    }
}
