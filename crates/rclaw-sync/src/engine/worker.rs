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

/// 同步在磁盘上关着时给命令与面板的统一说法。
const CLOSED_NOTE: &str = "同步已经关闭，请在「多端同步」设置里重新开启";
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
pub(crate) struct StorageIntent {
    /// 这次保存请求的标识：两次保存交错时，先完成的一方不能把后一个请求的意图删掉。
    #[serde(default)]
    pub(crate) id: String,
    pub(crate) config: SyncConfig,
    /// 要绑定的密码；`None` 或空 = 不改密码。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) secret: Option<String>,
    /// 这次请求**明确修改**的设备名 `(原值, 目标值)`（向导里编辑过）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) device_name_edit: Option<DeviceNameEdit>,
    /// 这份记录属于哪一次「存储段提交」（见 [`StorageCommitFile`]）。磁盘存储段被
    /// 任何入口换过一次，代次就 +1，旧代次的记录失去恢复资格。
    #[serde(default)]
    pub(crate) commit: u64,
    /// 同一存储段内的请求序号（协议锁内单调分配）：密码修改这类不换存储的请求靠它
    /// 定先后，不依赖墙钟（同毫秒/时钟回拨都不会选错）。
    #[serde(default)]
    pub(crate) req: u64,
    /// 这份记录的配置是否已经写进 `rclaw.toml`。旧版本没有这个字段（`false`），
    /// 恢复时按「同代次里最大的 req」判定，不单看这个标记。
    #[serde(default)]
    pub(crate) toml_written: bool,
    /// 旧版本（round19/20）的「明确设备名」字段：读进来避免解析失败。归属有歧义时
    /// 不应用（保留磁盘现值），凭据照常结算。
    #[serde(default, alias = "device_name", skip_serializing_if = "Option::is_none")]
    pub(crate) legacy_device_name: Option<String>,
}

/// 一次请求明确提交的设备名修改（见 [`StorageIntent::device_name_edit`]）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DeviceNameEdit {
    pub(crate) from: String,
    pub(crate) to: String,
}

/// 意图里与设备名有关的那一项（给宿主回补用）。
pub struct IntentDeviceNameEdit {
    pub from: String,
    pub to: String,
}

fn storage_intent_path(paths: &SyncPaths) -> PathBuf {
    paths.root.join("pending-storage.json")
}

/// 备份 = 最后一份可恢复的记录。替换主记录之前先把当前有效的记录放进备份；任何
/// 时刻都至少有一条「与磁盘存储段一致」的记录可恢复。
fn storage_intent_backup_path(paths: &SyncPaths) -> PathBuf {
    paths.root.join("pending-storage.prev.json")
}

/// 持久化的存储提交状态：`commit` 标识当前磁盘存储段是哪一次提交；`req` 是协议锁内
/// 单调的请求序号（同一存储段内的先后）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StorageCommitFile {
    #[serde(default)]
    commit: u64,
    #[serde(default)]
    req: u64,
    #[serde(default)]
    storage: SyncConfig,
}

fn storage_commit_path(paths: &SyncPaths) -> PathBuf {
    paths.root.join("storage-commit.json")
}

fn storage_commit_read(paths: &SyncPaths) -> Result<StorageCommitFile> {
    match std::fs::read(storage_commit_path(paths)) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(file) => Ok(file),
            Err(error) => {
                tracing::warn!("sync/storage-commit.json 损坏，按空状态处理: {error}");
                Ok(StorageCommitFile::default())
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StorageCommitFile::default()),
        Err(e) => Err(e.into()),
    }
}

fn storage_commit_write(paths: &SyncPaths, file: &StorageCommitFile) -> Result<()> {
    paths.ensure_root()?;
    rclaw_store::atomic_write_private(
        &storage_commit_path(paths),
        &serde_json::to_vec_pretty(file)?,
    )?;
    Ok(())
}

/// 存储段**确已**换成 `storage`（新 TOML 写成、或权威发布完成）之后推进提交代次，
/// 并让属于其它存储段的旧意图失去恢复资格（切回旧存储不会复活旧密码）。必须持有
/// 存储协议锁调用，且只能在新状态真正生效之后调用——登记意图/读盘/断线重连都只读。
pub(crate) fn storage_commit_advance(paths: &SyncPaths, storage: &SyncConfig) -> Result<u64> {
    let mut file = storage_commit_read(paths)?;
    if file.commit == 0 {
        // 第一次建立提交状态：旧版本的记录没有代次，不能被顺手清掉（它们仍按
        // 「存储段与磁盘一致」兜底恢复）。
        file.commit = 1;
        file.storage = storage.clone();
        storage_commit_write(paths, &file)?;
        return Ok(file.commit);
    }
    if !same_storage_cfg(&file.storage, storage) {
        file.commit = file.commit.saturating_add(1);
        file.storage = storage.clone();
        storage_commit_write(paths, &file)?;
        // 只淘汰「存储段不同」的记录；同一存储段的待补密码保留。
        for path in [storage_intent_path(paths), storage_intent_backup_path(paths)] {
            if let Ok(Some(intent)) = storage_intent_read(path.clone()) {
                if !same_storage_cfg(&intent.config, storage) {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
    Ok(file.commit)
}

/// 存储段已变时才推进（返回新的代次）；未变返回 `None`。调用方负责在协议锁内、
/// 新状态真正生效之后调用。
pub(crate) fn storage_commit_advance_if_changed(
    paths: &SyncPaths,
    storage: &SyncConfig,
) -> Result<Option<u64>> {
    let file = storage_commit_read(paths)?;
    if file.commit == 0 || !same_storage_cfg(&file.storage, storage) {
        return Ok(Some(storage_commit_advance(paths, storage)?));
    }
    Ok(None)
}

fn storage_intent_store(paths: &SyncPaths, intent: &StorageIntent) -> Result<()> {
    paths.ensure_root()?;
    rclaw_store::atomic_write_private(
        &storage_intent_path(paths),
        &serde_json::to_vec_pretty(intent)?,
    )?;
    Ok(())
}

pub(crate) fn storage_intent_backup_save(
    paths: &SyncPaths,
    intent: &StorageIntent,
) -> Result<()> {
    paths.ensure_root()?;
    rclaw_store::atomic_write_private(
        &storage_intent_backup_path(paths),
        &serde_json::to_vec_pretty(intent)?,
    )?;
    Ok(())
}

pub(crate) fn storage_intent_backup_clear(paths: &SyncPaths) -> Result<()> {
    match std::fs::remove_file(storage_intent_backup_path(paths)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn storage_intent_read(path: PathBuf) -> Result<Option<StorageIntent>> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(intent) => Ok(Some(intent)),
            Err(error) => {
                tracing::warn!("同步设置更新意图损坏，丢弃: {error}");
                Ok(None)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn storage_intent_load(paths: &SyncPaths) -> Result<Option<StorageIntent>> {
    storage_intent_read(storage_intent_path(paths))
}

fn storage_intent_backup_load(paths: &SyncPaths) -> Result<Option<StorageIntent>> {
    storage_intent_read(storage_intent_backup_path(paths))
}

/// 落盘「存储设置更新意图」，返回它的标识。同一套配置、密码与名称编辑重复调用
/// （同一请求重试）沿用手上这条记录的标识与提交状态；否则在协议锁内分配新的请求
/// 序号（同一存储段内的先后由此决定，不看墙钟）。
pub fn storage_intent_save(
    paths: &SyncPaths,
    config: &SyncConfig,
    secret: Option<&str>,
    device_name_edit: Option<(String, String)>,
) -> Result<String> {
    let secret = secret.map(|secret| secret.to_string());
    let device_name_edit = device_name_edit.map(|(from, to)| DeviceNameEdit { from, to });
    // 只读当前代次：登记意图不是一次存储提交，不能在这里推进/清理。
    let commit = storage_commit_read(paths)?.commit;
    if let Some(existing) = storage_intent_load(paths)? {
        if !existing.id.is_empty()
            && existing.config == *config
            && existing.secret == secret
            && existing.device_name_edit == device_name_edit
            && existing.commit == commit
        {
            return Ok(existing.id);
        }
    }
    let mut file = storage_commit_read(paths)?;
    // 提交文件缺失/损坏时不能从 0 重新发号：既有记录的序号可能更大，否则新请求
    // 会被排到旧记录之前。以现有记录的最大序号为下限。
    let floor = [
        storage_intent_load(paths)?.map(|intent| intent.req),
        storage_intent_backup_load(paths)?.map(|intent| intent.req),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(0);
    file.req = file.req.max(floor).saturating_add(1);
    storage_commit_write(paths, &file)?;
    let intent = StorageIntent {
        id: crate::ids::random_hex::<8>(),
        config: config.clone(),
        secret,
        device_name_edit,
        commit,
        req: file.req,
        toml_written: false,
        legacy_device_name: None,
    };
    storage_intent_store(paths, &intent)?;
    Ok(intent.id)
}

/// 把这条记录的配置标记成「TOML 已写成」。标记失败不会阻止恢复：同一代次里请求
/// 序号最大的记录即使没有标记，只要存储段与磁盘一致也算可恢复（覆盖「TOML 写完、
/// 标记未写就崩溃」这一窗口）。
pub(crate) fn storage_intent_mark_written(paths: &SyncPaths, id: &str) -> Result<()> {
    let Some(mut intent) = storage_intent_load(paths)? else {
        return Ok(());
    };
    if intent.id != id {
        return Ok(());
    }
    intent.toml_written = true;
    storage_intent_store(paths, &intent)
}

/// 最后一份可恢复的记录（给下一次保存做备份用）。
pub(crate) fn storage_intent_last_written(
    paths: &SyncPaths,
    disk: &SyncConfig,
) -> Result<Option<StorageIntent>> {
    Ok(storage_intent_effective(paths, disk)?.map(|effective| effective.intent))
}

/// 两套配置的存储段是否指向同一处。
fn same_storage_cfg(a: &SyncConfig, b: &SyncConfig) -> bool {
    a.transport == b.transport
        && a.webdav == b.webdav
        && a.file == b.file
        && a.s3 == b.s3
}

pub(crate) struct EffectiveIntent {
    pub(crate) intent: StorageIntent,
    pub(crate) from_backup: bool,
}

/// 启动/收尾要执行哪条意图：
/// - 非旧格式记录必须属于当前存储提交代次且存储段与磁盘一致；
/// - 同代次里请求序号最大的那笔胜出（即使 `toml_written` 还没写上——TOML 写完与
///   标记之间崩溃时，磁盘存储段已经一致，这是唯一能恢复新密码的依据）；
/// - 旧格式（没有序号）只在没有新格式候选时按主/备顺序兜底；
/// - 都不匹配磁盘（已被第三笔写入替代）时返回 `None`，保留磁盘现值。
pub(crate) fn storage_intent_effective(
    paths: &SyncPaths,
    disk: &SyncConfig,
) -> Result<Option<EffectiveIntent>> {
    let main = storage_intent_load(paths)?;
    let backup = storage_intent_backup_load(paths)?;
    let mut best: Option<EffectiveIntent> = None;
    for (intent, from_backup) in [(main, false), (backup, true)] {
        let Some(intent) = intent else {
            continue;
        };
        if !same_storage_cfg(disk, &intent.config) {
            continue;
        }
        // 只按「存储段与磁盘一致」判定归属：提交代次只用于先后排序与淘汰
        // （旧代次的记录在存储段真正换掉时已被删除）。这样提交文件缺失/损坏、
        // 或「TOML 已写、意图还没盖上代次」都不会把唯一可恢复的记录排除掉。
        let replace = match &best {
            None => true,
            // 请求序号是协议锁内全局单调的（跨存储代次也递增），先按它排；代次只做
            // 平局时的次序。这样提交文件缺失（代次读成 0）时新请求不会输给旧备份。
            Some(current) => (intent.req, intent.commit) > (current.intent.req, current.intent.commit),
        };
        if replace {
            best = Some(EffectiveIntent {
                intent,
                from_backup,
            });
        }
    }
    Ok(best)
}

/// 结算：先删落选的那一份（它从此不能被恢复），再删胜出的那一份。中途中断时胜出
/// 记录还在，下次启动会接着结算同一笔。
pub(crate) fn storage_intent_settle(paths: &SyncPaths, from_backup: bool) -> Result<()> {
    if from_backup {
        let _ = std::fs::remove_file(storage_intent_path(paths));
        storage_intent_backup_clear(paths)
    } else {
        let _ = storage_intent_backup_clear(paths);
        match std::fs::remove_file(storage_intent_path(paths)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// 后续一次显式改名（通用配置保存/导入）发布到 `current_name` 时，取消仍在等待的
/// 设备名编辑。凭据部分保留。
pub(crate) fn storage_intent_cancel_name_edit(
    paths: &SyncPaths,
    current_name: &str,
) -> Result<()> {
    for (path, is_backup) in [
        (storage_intent_path(paths), false),
        (storage_intent_backup_path(paths), true),
    ] {
        if let Some(mut intent) = storage_intent_read(path)? {
            let stale = intent
                .device_name_edit
                .as_ref()
                .is_some_and(|edit| edit.to != current_name);
            if !stale {
                continue;
            }
            intent.device_name_edit = None;
            if is_backup {
                storage_intent_backup_save(paths, &intent)?;
            } else {
                storage_intent_store(paths, &intent)?;
            }
        }
    }
    Ok(())
}

/// 给宿主启动回补用的计划。
pub struct StorageIntentPlan {
    pub config: SyncConfig,
    pub device_name_edit: Option<IntentDeviceNameEdit>,
    /// 选中的是备份记录（结算时按来源先删另一份）。
    pub from_backup: bool,
}

pub fn storage_intent_plan(
    paths: &SyncPaths,
    disk: &SyncConfig,
) -> Result<Option<StorageIntentPlan>> {
    Ok(storage_intent_effective(paths, disk)?.map(|effective| StorageIntentPlan {
        config: effective.intent.config,
        device_name_edit: effective
            .intent
            .device_name_edit
            .map(|edit| IntentDeviceNameEdit {
                from: edit.from,
                to: edit.to,
            }),
        from_backup: effective.from_backup,
    }))
}

/// 存储保存协议的跨进程锁：意图、`rclaw.toml`、secrets 与提交状态的整套提交必须
/// 串行——两次保存交错时会留下 TOML 与运行配置各一套、且没有恢复记录的状态。
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
    /// 重配：配置 + 宿主发布时分配的序号（比共享序号旧的命令直接丢掉）。
    Reconfigure(SyncConfig, u64),
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
        /// 宿主派发这次加入时的配置代次：远端校验完成、提交凭据之前复核，
        /// 期间若有更新的发布（例如关闭同步）就作废这次加入。
        host_seq: u64,
        reply: Reply<(SyncConfig, u64)>,
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
        reply: Reply<u64>,
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
    /// 上一次打开没连上的原因（协议锁竞争/确认失败/引擎锁被占）：给「立即同步」等
    /// 等着结果的命令一个和状态面板一致的说法。
    open_note: Option<String>,
    /// 当前 `self.config` 是哪一次安装的（`publish_local_config` 分配的序号）：
    /// 幂等加入要按「实际采用的那份配置所属的提交」给宿主写盘资格。
    installed_seq: u64,
    /// 上一次安装前的共享配置：读盘确认时如果磁盘还是这一份，说明宿主的写回还没
    /// 发生（不能把它当更新的安装，把刚完成/正在等待写回的加入回执作废）。
    published_before: Option<SyncConfig>,
    /// 只有「加入」安装需要等宿主写回 TOML：这段时间内磁盘等于安装前那一份是正常
    /// 的。普通保存先写 TOML 再安装，不需要这个保护（否则真实回滚会被永久跳过）。
    protect_before: bool,
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
            open_note: None,
            installed_seq: 0,
            published_before: None,
            protect_before: false,
        }
    }

    async fn run(mut self) {
        // 打开连接时会在协议锁里确认磁盘上的同步设置（见 [`Worker::open`]）。
        self.open(true, false).await;
        loop {
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
                self.open(true, false).await;
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
                self.open(false, true).await;
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
        self.open_note = Some(message.clone());
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
            // 磁盘上还是「上一次安装前」的那一份：宿主的写回还没发生（典型是本进程
            // 刚加入/保存、回执还没结算）。这时不能把它当更新的安装——否则等待写回
            // 的加入回执会被自己的旧 TOML 作废。
            let stale = self.protect_before
                && self.published_before.as_ref().is_some_and(|before| {
                    before.transport == fresh.transport
                        && before.webdav == fresh.webdav
                        && before.file == fresh.file
                        && before.s3 == fresh.s3
                        && before.enabled == fresh.enabled
                });
            if stale {
                return Ok(());
            }
            // 看到任何别的磁盘值（或安装值已写回）就不再保留这个保护。
            self.protect_before = false;
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
                // 读盘采用新值也是一次安装：推进序号，更早派发的加入/保存回执与
                // 重配不能再按旧序号通过检查。
                let seq = self.shared.config_seq.fetch_add(1, Ordering::Relaxed) + 1;
                self.installed_seq = seq;
            } else {
                // 同一份配置：安装归属沿用共享值当前对应的那次提交。
                self.installed_seq = self.shared.config_seq.load(Ordering::Relaxed);
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
    async fn open(&mut self, confirm: bool, resume_intent: bool) {
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
                    if !self.config.enabled {
                        // 已知关闭只是上一次读盘的结果：锁暂时被占时保留一次确认
                        // 重试，等另一进程放开后再看看磁盘是否已经重新开启。
                        self.open_note = Some(CLOSED_NOTE.into());
                        self.shared.set_state("off", None);
                        self.reopen_at = Some(Instant::now() + STANDBY_RETRY);
                        return;
                    }
                    let note = "另一个进程正在保存同步设置，确认后再连接（稍后自动重试）";
                    self.open_note = Some(note.into());
                    self.shared.set_state("syncing", Some(note.into()));
                    self.reopen_at = Some(Instant::now() + STANDBY_RETRY);
                    return;
                }
                Err(error) => {
                    // 有宿主回调时「确认」是打开的前提：锁文件坏了说明配置和凭据都
                    // 没法确认，不能拿可能过期的缓存去连存储——进入可见的重试状态。
                    // 没有回调（测试/无配置文件的宿主）时按当前配置继续。
                    if !self.config.enabled {
                        self.open_note = Some(CLOSED_NOTE.into());
                        self.shared.set_state("off", None);
                        self.reopen_at = Some(Instant::now() + STANDBY_RETRY);
                        return;
                    }
                    if self.shared.refresh_config.is_some() {
                        tracing::warn!("确认同步设置时拿不到协议锁（稍后重试）: {error}");
                        self.shared.set_state(
                            "syncing",
                            Some("等待确认同步存储设置（锁不可用），稍后重试".into()),
                        );
                        let note = "确认同步存储设置失败（存储锁不可用），稍后自动重试";
                        self.open_note = Some(note.into());
                        self.shared.set_state("syncing", Some(note.into()));
                        self.reopen_at = Some(Instant::now() + STANDBY_RETRY);
                        return;
                    }
                    tracing::warn!("确认同步设置时拿不到协议锁（按当前配置继续）: {error}");
                }
            }
            if protocol_lock.is_some() {
                if self.adopt_disk_config().is_err() {
                    let note = "读取同步设置失败（稍后自动重试）";
                    self.open_note = Some(note.into());
                    self.shared.set_state("syncing", Some(note.into()));
                    self.reopen_at = Some(Instant::now() + STANDBY_RETRY);
                    return;
                }
                if !self.config.enabled {
                    // 另一个进程把同步关了：不接手引擎所有权，状态以磁盘为准。worker
                    // 留着（不退出）：重新开启的配置命令与「关闭后只清本机」的退出
                    // 请求都还要由它接住——退场与宿主派发之间的交接窗口反而会丢命令。
                    self.lock = None;
                    self.open_note = Some(CLOSED_NOTE.into());
                    self.shared.set_state("off", None);
                    self.reopen_at = None;
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
        // 收尾存储意图：协议锁内按磁盘权威配置打开时一定结算；宿主配置发布触发的
        // 重连（`resume_intent`）也允许结算——但拿到锁失败就整次让行、安排下一次
        // 权威打开，别拿缓存配置去连一个凭据还没补上的存储。
        if protocol_lock.is_some() {
            if let Err(error) = self.resume_storage_intent(true, true).await {
                tracing::warn!("没存完的存储设置收尾失败（下次打开再试）: {error}");
            }
        } else if resume_intent {
            match self.resume_storage_intent(false, true).await {
                Ok(true) => {}
                Ok(false) => {
                    self.reopen_at = Some(Instant::now() + STANDBY_RETRY);
                    return;
                }
                Err(error) => {
                    tracing::warn!("没存完的存储设置收尾失败（下次打开再试）: {error}");
                }
            }
        }
        // 配置、密钥与意图都已经收敛到同一套：保存协议锁到这里就可以放了。
        drop(protocol_lock);
        // 上一次重置 / 退出没做完：按落盘的记录与远端的实际状态接着做（重置可能已经把
        // 本机换到了新空间），再按正常打开往下走。
        if let Err(error) =
            space::resume_pending(&shared.ctx, &self.config, &mut self.secrets, None).await
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
            self.open_note = None;
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
        // 没有连接、没有附件任务：没有需要封口的写入阶段，这一步无事可做。关闭态
        // worker 被宿主显式停止时不能去碰共用的提交入口——继任者可能已经连上并
        // 打开了它，旧收尾再 seal/bump 会把新连接一直关在门外。
        if self.session.is_none() && self.uploads.is_none() && self.downloads.is_none() {
            return true;
        }
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
            Cmd::Reconfigure(config, host_seq) => self.reconfigure(config, host_seq).await,
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
            Cmd::JoinSpace {
                code,
                host_seq,
                reply,
            } => {
                if wanted(&reply) {
                    let _ = reply.send(self.join_space(&code, host_seq).await);
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
                    self.open(true, false).await;
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
            self.open(true, false).await;
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

    /// 把 worker 刚安装的配置发布到共享的那一份，并推进安装序号：保存/加入的回执
    /// 带着这个序号，宿主按它决定谁该发布（发布顺序跟随安装顺序）。
    fn publish_local_config(&mut self) -> u64 {
        // 值与序号在同一把锁里提交：回执方在锁内比较序号，不会看到「新值 + 旧序号」。
        let mut guard = lock(&self.shared.config);
        self.published_before = Some(guard.clone());
        *guard = self.config.clone();
        let seq = self.shared.config_seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.installed_seq = seq;
        seq
    }

    async fn reconfigure(&mut self, config: SyncConfig, host_seq: u64) {
        // 检查与发布在同一把配置锁里：宿主在两次动作之间发布新配置（包括关闭同步）
        // 时，旧重配不能再把开启状态写回去；同时把这次安装记成共享配置的当前值。
        let old = {
            let mut guard = lock(&self.shared.config);
            let latest = self.shared.config_seq.load(Ordering::Relaxed);
            if host_seq < latest {
                tracing::info!("收到过期的配置重配（#{host_seq} < #{latest}），忽略");
                return;
            }
            let old = std::mem::replace(&mut self.config, config);
            if self.config == old {
                // 同一份配置（面板「重试」、同代次排队的重配）：不重复安装、
                // 不推进序号——否则会把同一代次派发的加入/保存误判为过期。
                self.installed_seq = latest;
            } else {
                *guard = self.config.clone();
                let seq = self.shared.config_seq.fetch_add(1, Ordering::Relaxed) + 1;
                self.installed_seq = seq;
                self.protect_before = false;
            }
            old
        };
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
            // 也试——面板上的「重试」就是把现有的配置再交过来一遍。宿主发布来的配置
            // 是从磁盘读的，允许顺带结算存储意图。
            self.open(false, true).await;
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
            self.open(false, false).await;
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
        self.open(false, false).await;
        Ok(())
    }

    async fn join_space(&mut self, code: &str, host_seq: u64) -> Result<(SyncConfig, u64)> {
        self.require_owner()?;
        let ready = {
            let shared = self.shared.clone();
            move || host_seq >= shared.config_seq.load(Ordering::Relaxed)
        };
        if !ready() {
            return Err(SyncError::other("同步设置在加入过程中被改动，请重试加入"));
        }
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
                    self.open(false, false).await;
                }
                // 幂等：配置没变，写盘资格属于当初安装这份配置的那次提交。
                return Ok((joined, self.installed_seq));
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
                self.open(false, false).await;
                return Err(error);
            }
            // 和首次加入一样：凭据提交与代次推进在存储协议锁内，不能挤出另一笔
            // 正在持锁的保存（那笔的恢复依据会被这里推进代次时误删）。
            let lock_paths = self.shared.ctx.paths.clone();
            let acquired = blocking(move || StorageLock::try_acquire(&lock_paths)).await;
            let _storage_lock = match acquired {
                Ok(Some(lock)) => lock,
                Ok(None) => {
                    self.open(false, false).await;
                    return Err(SyncError::other("存储设置正在保存，请稍后重试加入"));
                }
                Err(error) => {
                    // 锁文件本身出错（I/O）：旧连接已经拆掉，恢复它并把这个错误
                    // 如实交给加入方，别留下一个没有 session 的 worker。
                    self.open(false, false).await;
                    return Err(error);
                }
            };
            if !ready() {
                self.open(false, false).await;
                return Err(SyncError::other("同步设置在加入过程中被改动，请重试加入"));
            }
            let paths = self.shared.ctx.paths.clone();
            let saved = next.clone();
            if let Err(error) = blocking(move || saved.save(&paths)).await {
                self.open(false, false).await;
                return Err(error);
            }
            self.secrets = next;
            self.config = joined.clone();
            self.protect_before = true;
            // 凭据与配置已提交：这时才推进存储代次（淘汰其它存储段的旧意图）。
            let _ = storage_commit_advance(&self.shared.ctx.paths, &joined);
            let seq = self.publish_local_config();
            drop(_storage_lock);
            self.open(false, false).await;
            return Ok((joined, seq));
        }
        let plan =
            space::join_verify(&self.shared.ctx.paths, &self.config, &self.secrets, code).await?;
        let lock_paths = self.shared.ctx.paths.clone();
        let Some(_storage_lock) =
            blocking(move || StorageLock::try_acquire(&lock_paths)).await?
        else {
            return Err(SyncError::other("存储设置正在保存，请稍后重试加入"));
        };
        if !ready() {
            return Err(SyncError::other("同步设置在加入过程中被改动，请重试加入"));
        }
        let joined = plan.commit(&mut self.secrets).await?;
        // 配对码里带了存储设置的话，配置也跟着换：和同空间重加一样走同一次
        // 「安装 + 发布 + 推进序号」，回执带的就是这次安装的序号。
        self.config = joined.clone();
        self.protect_before = true;
        let _ = storage_commit_advance(&self.shared.ctx.paths, &joined);
        let seq = self.publish_local_config();
        drop(_storage_lock);
        self.open(false, false).await;
        Ok((joined, seq))
    }

    async fn leave_space(&mut self) -> Result<()> {
        if !self.config.enabled {
            // 同步已在磁盘上关闭（worker 不持引擎锁）：按离线退出。先取引擎锁、再重读
            // secrets——另一个进程可能刚做完退出的一半，不能凭本机旧副本清任何东西。
            let paths = self.shared.ctx.paths.clone();
            let (lock, mut secrets) = blocking(move || {
                let lock =
                    take_lock(&paths)?.ok_or_else(|| SyncError::other(STANDBY_HINT))?;
                let secrets = Secrets::load(&paths)?;
                Ok((lock, secrets))
            })
            .await?;
            let result = space::leave(&self.shared.ctx, &self.config, &mut secrets, None).await;
            drop(lock);
            result?;
            self.secrets = secrets;
            self.loaded = true;
            self.shared.paired.store(false, Ordering::Relaxed);
            self.shared.set_space(None, false);
            // 同步仍是关闭状态：面板按 off 显示，命令也报关闭（不报别的进程占锁）。
            self.open_note = Some(CLOSED_NOTE.into());
            self.shared.set_state("off", None);
            return Ok(());
        }
        self.require_owner()?;
        if !self.teardown().await {
            return Err(SyncError::other("还有在途的附件提交，稍后再退出"));
        }
        let result = space::leave(&self.shared.ctx, &self.config, &mut self.secrets, None).await;
        // 退出成功就停在「还没配对」；没成功就照旧连回去。
        self.open(false, false).await;
        result
    }

    async fn reset_space(&mut self) -> Result<()> {
        self.require_owner()?;
        self.config.validate()?;
        if !self.teardown().await {
            return Err(SyncError::other("还有在途的附件提交，稍后再重置"));
        }
        let result = space::reset(&self.shared.ctx, &self.config, &mut self.secrets).await;
        self.open(false, false).await;
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
            self.open(false, false).await;
            return Err(error);
        }
        let (paths, saved) = (self.shared.ctx.paths.clone(), next.clone());
        if let Err(error) = blocking(move || saved.save(&paths)).await {
            // 已经 teardown 过：把旧的连接恢复回来，别让同步停在 sealed 状态。
            self.open(false, false).await;
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
            self.open(false, false).await;
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
    async fn save_storage(
        &mut self,
        config: SyncConfig,
        secret: Option<String>,
    ) -> Result<u64> {
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
                    self.open(false, false).await;
                    return Err(error);
                }
            }
        }
        let (paths, saved) = (self.shared.ctx.paths.clone(), next.clone());
        if let Err(error) = blocking(move || saved.save(&paths)).await {
            self.open(false, false).await;
            return Err(error);
        }
        self.secrets = next;
        self.config = config;
        self.protect_before = false;
        let seq = self.publish_local_config();
        self.shared.set_space(
            self.secrets
                .is_paired()
                .then(|| self.secrets.space_id.clone()),
            self.secret_saved(),
        );
        if self.secrets.is_paired() {
            // 本次保存的 TOML/配置已装上：顺带结算可能存在的其它意图。
            self.open(false, true).await;
        }
        Ok(seq)
    }

    /// 上次没存完的存储设置：`rclaw.toml` 在保存时已经先写成新的一套，secrets 是命令
    /// 失败没写上的那一半。按记录的配置把密码绑上、落盘（配置以记录为准——密码是按它
    /// 绑的），两边收敛到同一套，然后删记录。
    /// 返回 `Ok(false)` 表示「需要权威配置但我们拿不到协议锁」：调用方应让行。
    async fn resume_storage_intent(&mut self, locked: bool, required: bool) -> Result<bool> {
        let paths = self.shared.ctx.paths.clone();
        // 调用方（打开连接时的配置确认）已经持着协议锁时不再重复拿：同一进程对同一
        // 把文件锁再拿一次会永远等自己。否则非阻塞地拿：宿主正拿着协议锁在保存时这次
        // 跳过（它会把意图结算掉，或者下次启动再来）。
        let _lock = if locked {
            None
        } else {
            match blocking(move || StorageLock::try_acquire(&paths)).await? {
                Some(lock) => Some(lock),
                None if required => return Ok(false),
                None => return Ok(true),
            }
        };
        let paths = self.shared.ctx.paths.clone();
        let disk = self.config.clone();
        let Some(intent) = blocking(move || storage_intent_effective(&paths, &disk)).await? else {
            return Ok(true);
        };
        // 宿主没能把 `rclaw.toml` 补写成意图里的目标：先别绑定 secrets、也别删意图，
        // 否则下次启动只剩一份和 TOML 不一致的凭据，恢复依据就没了。
        if !same_storage(&self.config, &intent.intent.config) {
            tracing::warn!("存储更新意图与当前配置的存储段不一致（TOML 还没补写），先保留");
            return Ok(true);
        }
        let mut next = self.secrets.clone();
        if let Some(secret) = intent.intent.secret.as_deref().filter(|secret| !secret.is_empty()) {
            if intent.intent.config.transport != "file" {
                next.set_storage_secret(&intent.intent.config, secret.to_string())?;
            }
        }
        let (paths, saved) = (self.shared.ctx.paths.clone(), next.clone());
        blocking(move || saved.save(&paths)).await?;
        self.secrets = next;
        // 只把存储设置（以及这次请求明确改过的设备名）搬过来：这期间别处改过的
        // 范围/通道以当前配置为准。
        self.config.apply_storage_from(&intent.intent.config);
        if let Some(edit) = &intent.intent.device_name_edit {
            if self.config.device_name == edit.from || self.config.device_name == edit.to {
                self.config.device_name = edit.to.clone();
            }
        }
        let (paths, from_backup) = (self.shared.ctx.paths.clone(), intent.from_backup);
        blocking(move || storage_intent_settle(&paths, from_backup)).await?;
        self.shared.set_space(
            self.secrets
                .is_paired()
                .then(|| self.secrets.space_id.clone()),
            self.secret_saved(),
        );
        Ok(true)
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
