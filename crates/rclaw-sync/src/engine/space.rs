//! 同步空间与存储：按配置组装传输、配对码、创建 / 加入 / 退出 / 重置空间、「测试连接」，
//! 以及换了空间之后清掉本机的同步状态。
//!
//! 这里的函数不知道引擎的线程与轮次：由 [`super::worker`] 在两轮之间调用。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use rclaw_store::sync_db::{self, KV_SYNC_DEVICE, KV_SYNC_HLC, KV_SYNC_SCHEMA, SEAL_LOCAL};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::attach;
use crate::codec::SpaceFile;
use crate::config::{S3Section, SyncConfig, SyncPaths, WebdavSection};
use crate::crypto::{Keys, SpaceKey};
use crate::db::{
    self, scope, BlobRow, BLOB_FETCHED, BLOB_HELD, BLOB_PENDING, BLOB_REMOTE, BLOB_UPLOADED,
};
use crate::error::{Result, SyncError};
use crate::files;
use crate::identity::{self, Identity};
use crate::map;
use crate::pairing::{PairedStorage, PairingCode};
use crate::remote;
use crate::seal;
use crate::secrets::{new_space_id, Secrets};
use crate::status;
use crate::transport::file::FileTransport;
use crate::transport::s3::{S3Options, S3Transport};
use crate::transport::webdav::{WebdavOptions, WebdavTransport};
use crate::transport::{self, Scoped, SelfTestStep, SyncTransport};
use crate::trash;
use crate::wire::{self, StoredRefs};
use crate::{blocking, Ctx};

/// 一条收尾记录最多等这么久（传输层每个请求另有 60 秒上限）：要容得下一条记录正常
/// 需要的几次读写，又不能真的无限等。
const CLOSE_ENTRY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1800);

/// 一个打开的同步空间：套了空间子目录的传输，和这个空间的密钥。
pub(crate) struct Live {
    transport: Box<dyn SyncTransport>,
    pub keys: Arc<Keys>,
    pub space_id: String,
}

impl Live {
    pub fn transport(&self) -> &dyn SyncTransport {
        self.transport.as_ref()
    }
}

fn config_error(message: &str) -> SyncError {
    SyncError::Transport(crate::error::TransportError::Config(message.to_string()))
}

/// 按配置组装存储根上的传输（还没套空间子目录）。HTTPS 可以读取本机证书库。
pub(crate) fn root_transport(
    config: &SyncConfig,
    secrets: &Secrets,
) -> Result<Box<dyn SyncTransport>> {
    match config.transport.as_str() {
        "webdav" => {
            let url = config.webdav.url.trim();
            if url.is_empty() {
                return Err(config_error("还没有填写 WebDAV 地址"));
            }
            Ok(Box::new(WebdavTransport::new(&WebdavOptions {
                url: url.to_string(),
                username: config.webdav.username.clone(),
                password: secrets.storage_secret(config).to_string(),
                allow_http: config.webdav.allow_http,
                pinned_cert_sha256: config.webdav.pinned_cert_sha256.trim().to_string(),
            })?))
        }
        "file" => Ok(Box::new(FileTransport::new(file_root(config)?))),
        "s3" => {
            let s3 = &config.s3;
            if s3.endpoint.trim().is_empty() || s3.bucket.trim().is_empty() {
                return Err(config_error("还没有填写对象存储的地址和存储桶"));
            }
            Ok(Box::new(S3Transport::new(&S3Options {
                endpoint: s3.endpoint.trim().to_string(),
                region: s3.region.trim().to_string(),
                bucket: s3.bucket.trim().to_string(),
                prefix: s3.prefix.trim().to_string(),
                access_key: s3.access_key_id.trim().to_string(),
                secret_key: secrets.storage_secret(config).to_string(),
                path_style: s3.path_style,
                allow_http: s3.allow_http,
            })?))
        }
        other => Err(config_error(&format!(
            "不认识的存储类型 {other:?}（可用：webdav、file、s3）"
        ))),
    }
}

fn file_root(config: &SyncConfig) -> Result<PathBuf> {
    let dir = config.file.dir.trim();
    if dir.is_empty() {
        return Err(config_error("还没有填写同步用的文件夹"));
    }
    let dir = PathBuf::from(dir);
    if !dir.is_absolute() {
        return Err(config_error("同步用的文件夹要填绝对路径"));
    }
    Ok(dir)
}

pub(crate) async fn load_live(config: &SyncConfig, secrets: &Secrets) -> Result<Live> {
    let (config, secrets) = (config.clone(), secrets.clone());
    blocking(move || open_live(&config, &secrets)).await
}

/// 打开本机所在的同步空间：存储根下以 `space_id` 命名的子目录。
fn open_live(config: &SyncConfig, secrets: &Secrets) -> Result<Live> {
    let keys = Arc::new(secrets.keys()?);
    let transport: Box<dyn SyncTransport> = if config.transport == "file" {
        Box::new(FileTransport::new(
            file_root(config)?.join(&secrets.space_id),
        ))
    } else {
        Box::new(Scoped::new(
            root_transport(config, secrets)?,
            &secrets.space_id,
        ))
    };
    Ok(Live {
        transport,
        keys,
        space_id: secrets.space_id.clone(),
    })
}

async fn create_file_namespace(config: &SyncConfig, space_id: &str) -> Result<()> {
    if config.transport == "file" {
        FileTransport::new(file_root(config)?)
            .create_namespace(space_id)
            .await?;
    }
    Ok(())
}

/// 配对码带来的存储设置（不碰本机的密钥与凭据）：宿主先把它写进 `rclaw.toml` 再加入，
/// 避免「密钥已经存好、设置还没写回」时下次启动连到另一处存储。
pub fn pairing_config(config: &SyncConfig, code: &str) -> Result<SyncConfig> {
    let parsed = PairingCode::parse(code)?;
    let mut secrets = Secrets::default();
    Ok(apply_pairing(config, &mut secrets, &parsed))
}

/// 把配对码里的内容落到配置与 secrets 上。纯函数：不碰磁盘和网络。CLI 的 `--sync-join`
/// 没有引擎可用，直接调它。配对码里带了存储设置就以它为准，没带则沿用本机已填的。
pub fn apply_pairing(config: &SyncConfig, secrets: &mut Secrets, code: &PairingCode) -> SyncConfig {
    let mut config = config.clone();
    config.enabled = true;
    secrets.space_id = code.space_id.clone();
    secrets.key = code.key.clone();
    match &code.storage {
        Some(PairedStorage::Webdav {
            url,
            username,
            password,
            allow_http,
            pin,
        }) => {
            config.transport = "webdav".into();
            config.webdav = WebdavSection {
                url: url.clone(),
                username: username.clone(),
                allow_http: *allow_http,
                pinned_cert_sha256: pin.clone(),
            };
            // 配对码里的凭据只发往它自己指定的地址。
            secrets.webdav_account = Secrets::storage_account(&config).unwrap_or_default();
            secrets.webdav_password = password.clone();
        }
        Some(PairedStorage::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            access_key_id,
            secret_key,
            path_style,
            allow_http,
        }) => {
            config.transport = "s3".into();
            config.s3 = S3Section {
                endpoint: endpoint.clone(),
                region: region.clone(),
                bucket: bucket.clone(),
                prefix: prefix.clone(),
                access_key_id: access_key_id.clone(),
                path_style: *path_style,
                allow_http: *allow_http,
            };
            secrets.s3_account = Secrets::storage_account(&config).unwrap_or_default();
            secrets.s3_secret_key = secret_key.clone();
        }
        None => {}
    }
    config
}

/// 本机所在空间的配对码。`include_storage` 为真时带上存储地址与凭据（`file` 后端的目录
/// 是各台设备自己的本机路径，不带）。
pub(crate) fn pairing_code(
    config: &SyncConfig,
    secrets: &Secrets,
    include_storage: bool,
) -> Result<String> {
    if !secrets.is_paired() {
        return Err(SyncError::other("还没有创建或加入同步空间"));
    }
    let storage = match (include_storage, config.transport.as_str()) {
        (true, "webdav") => Some(PairedStorage::Webdav {
            url: config.webdav.url.trim().to_string(),
            username: config.webdav.username.clone(),
            password: secrets.storage_secret(config).to_string(),
            allow_http: config.webdav.allow_http,
            pin: config.webdav.pinned_cert_sha256.trim().to_string(),
        }),
        (true, "s3") => Some(PairedStorage::S3 {
            endpoint: config.s3.endpoint.trim().to_string(),
            region: config.s3.region.trim().to_string(),
            bucket: config.s3.bucket.trim().to_string(),
            prefix: config.s3.prefix.trim().to_string(),
            access_key_id: config.s3.access_key_id.trim().to_string(),
            secret_key: secrets.storage_secret(config).to_string(),
            path_style: config.s3.path_style,
            allow_http: config.s3.allow_http,
        }),
        _ => None,
    };
    PairingCode {
        space_id: secrets.space_id.clone(),
        key: secrets.key.clone(),
        storage,
    }
    .encode()
}

/// 记下「这个空间是本机建的」并保存 secrets。
async fn save_created(
    ctx: &Ctx,
    next: &Secrets,
    keepalive: Option<std::sync::Arc<std::fs::File>>,
) -> Result<()> {
    let (ctx, next) = (ctx.clone(), next.clone());
    blocking(move || {
        // 引擎锁跟着这次本机写入活到底：调用方的等待被超时丢弃时，已经开始的
        // 写入仍然攥着锁，别的进程不能在它写 secrets/创建者标记时接手。
        let _keep = keepalive;
        db::write(ctx.db(), |tx| {
            db::kv_put_json(tx, db::KV_CREATOR, &next.space_id)
        })?;
        next.save(&ctx.paths)
    })
    .await
}

// ---------------------------------------------------------------------------
// 没做完的空间切换
// ---------------------------------------------------------------------------

/// 一次没做完的空间切换（重置 / 退出）。动远端、换本机密钥之前先把它落盘（`sync/
/// pending-space.json`，与密钥同样的私有权限）：中途退出（掉电、写不了盘）后，重启或
/// 重试按记录与远端的实际状态接着做，同一个切换不会做岔、也不会留下只有远端知道的新
/// 空间。做完才删记录。记录里带着这次切换用的存储配置：恢复必须回**同一个位置**做，
/// 配置改了（换了前缀、换了桶）也不能在新位置把 404 当成「旧空间已不在」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PendingSpace {
    action: SpaceAction,
    /// 这一次切换所用的存储配置。老版本写的记录没有这个字段，恢复时沿用当时的配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    storage: Option<SyncConfig>,
    /// 动手之前本机的 secrets：关旧空间要用它的存储凭据与旧密钥。
    previous: Secrets,
    /// 重置要切到的新 secrets（退出没有）。
    replacement: Option<Secrets>,
}

/// 退出前没能收尾的关闭（`sync/pending-close.json`）。本机马上要清掉旧空间的密钥，
/// 之后只有这条记录里的凭据还能把旧空间关闭标记补齐；它只做远端关闭，不碰本机配对。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PendingClose {
    /// 这条记录的身份：收尾结果按它结算，不按目标——期间同目标可能已经被新记录替代。
    #[serde(default)]
    id: String,
    /// 这条记录（或它最近一次被失败恢复）落盘时的队列版本（队列锁内递增分配）：
    /// 成功的按目标清理只删**版本不高于尝试开始时上界**的记录，不把并发失败刚补回来
    /// 的重试依据一起删掉；也不受墙钟回拨/同毫秒影响。
    #[serde(default)]
    rev: u64,
    /// 关闭时用的存储配置（来自那条没做完的重置记录）。
    storage: SyncConfig,
    /// 旧空间的密钥与存储凭据。
    previous: Secrets,
    /// 本机在关闭现场确认过兼容标记已是关闭版、只是墓碑没补上。迟到的创建可以把
    /// 标记改回开放，但这个事实不受影响，收尾时据此直接补完。
    #[serde(default)]
    closed_seen: bool,
}

/// 规范化的存储位置标识：同一次空间切换必须在同一个位置上恢复。
fn storage_binding(config: &SyncConfig) -> String {
    match config.transport.as_str() {
        "webdav" => format!(
            "webdav|{}|{}",
            config.webdav.url.trim().trim_end_matches('/'),
            config.webdav.username.trim()
        ),
        "s3" => format!(
            "s3|{}|{}|{}|{}|{}",
            config.s3.endpoint.trim().trim_end_matches('/'),
            config.s3.bucket.trim(),
            config.s3.prefix.trim().trim_end_matches('/'),
            // 同一个桶和前缀下换一套账户，是不同的「位置」：凭据不能混着用，否则待恢复
            // 副本里的旧账户密钥会被新账户的覆盖掉，之后再也查不到能开旧空间的密钥。
            config.s3.access_key_id.trim(),
            config.s3.path_style
        ),
        "file" => format!(
            "file|{}",
            config.file.dir.trim().trim_end_matches(['/', '\\'])
        ),
        other => format!("storage|{other}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SpaceAction {
    /// 重置：立起新空间、废弃旧空间、本机切到新空间。
    Reset,
    /// 退出：本机改回未配对、清掉同步状态（远端不动）。
    Leave,
}

fn pending_path(paths: &SyncPaths) -> PathBuf {
    paths.root.join("pending-space.json")
}

fn pending_load(paths: &SyncPaths) -> Result<Option<PendingSpace>> {
    match std::fs::read(pending_path(paths)) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(pending) => Ok(Some(pending)),
            Err(error) => {
                // 记录坏了就没法接着做；把它丢掉，本机保持现在这份密钥不动。
                tracing::warn!("sync/pending-space.json 损坏，丢弃: {error}");
                let _ = std::fs::remove_file(pending_path(paths));
                Ok(None)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn pending_save(paths: &SyncPaths, pending: &PendingSpace) -> Result<()> {
    paths.ensure_root()?;
    rclaw_store::atomic_write_private(
        &pending_path(paths),
        &serde_json::to_vec_pretty(pending)?,
    )?;
    Ok(())
}

fn pending_clear(paths: &SyncPaths) -> Result<()> {
    match std::fs::remove_file(pending_path(paths)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn pending_close_path(paths: &SyncPaths) -> PathBuf {
    paths.root.join("pending-close.json")
}

fn pending_close_load(paths: &SyncPaths) -> Result<Vec<PendingClose>> {
    match std::fs::read(pending_close_path(paths)) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(close) => Ok(close),
            Err(error) => {
                // 记录坏了就没法接着做；丢掉它，远端保持现在的样子。
                tracing::warn!("sync/pending-close.json 损坏，丢弃: {error}");
                let _ = std::fs::remove_file(pending_close_path(paths));
                Ok(Vec::new())
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

fn pending_close_save(paths: &SyncPaths, closes: &[PendingClose]) -> Result<()> {
    if closes.is_empty() {
        return pending_close_clear(paths);
    }
    paths.ensure_root()?;
    rclaw_store::atomic_write_private(
        &pending_close_path(paths),
        &serde_json::to_vec_pretty(closes)?,
    )?;
    Ok(())
}

fn pending_close_clear(paths: &SyncPaths) -> Result<()> {
    match std::fs::remove_file(pending_close_path(paths)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// 同一存储位置上，连接设置（证书指纹、区域、明文 HTTP）以当前这份为准：退出之后
/// 用户可能改过它们。位置本身仍用记录里的——配对码只带空间 ID 与密钥，同一个空间
/// 可以复制到别处；不能因为「位置标识相同」就把收尾改投到另一个副本上。
fn refresh_connection_settings(target: &mut SyncConfig, current: &SyncConfig) {
    match target.transport.as_str() {
        "webdav" => {
            target.webdav.allow_http = current.webdav.allow_http;
            target.webdav.pinned_cert_sha256 = current.webdav.pinned_cert_sha256.clone();
        }
        "s3" => {
            target.s3.allow_http = current.s3.allow_http;
            target.s3.region = current.s3.region.clone();
        }
        _ => {}
    }
}

/// 两个配置是不是同一个实际存储位置。按位置字段逐字相等判（不用规范化的位置标识：
/// 规范化会把「末尾字面反斜杠」「用户名首尾空白」这类不同的实际位置当成同一处，确认
/// 事实会串到另一个副本上）；连接设置（证书指纹、区域、明文开关）不参与——同一个
/// 位置换了指纹，两条记录该合成一条。
fn same_close_location(a: &SyncConfig, b: &SyncConfig) -> bool {
    if a.transport != b.transport {
        return false;
    }
    match a.transport.as_str() {
        "webdav" => a.webdav.url == b.webdav.url && a.webdav.username == b.webdav.username,
        "s3" => {
            a.s3.endpoint == b.s3.endpoint
                && a.s3.bucket == b.s3.bucket
                && a.s3.prefix == b.s3.prefix
                && a.s3.access_key_id == b.s3.access_key_id
                && a.s3.path_style == b.s3.path_style
        }
        "file" => a.file.dir == b.file.dir,
        _ => false,
    }
}

fn rev_seq_path(paths: &SyncPaths) -> PathBuf {
    paths.root.join("pending-close.seq")
}

/// 原子写兜底路径（备份替换）的 crash 窗口里主文件会缺失、旧值还在备份里。
fn rev_seq_backup_path(paths: &SyncPaths) -> PathBuf {
    paths.root.join(".pending-close.seq.bak")
}

fn rev_seq_parse(text: &str) -> Result<u64> {
    text.trim().parse().map_err(|error| {
        SyncError::other(format!(
            "同步收尾队列的版本序号读不出来（{error}）：先确认上界再分配新版本"
        ))
    })
}

/// 已分配版本的最大值（独立于队列文件：删光记录/清空队列也不回退）。`None` = 从未
/// 分配过（首次使用）；读/解析失败要报错：当成 0 会让后续分配从更小的值重来，而在途
/// 尝试还拿着旧上界结算，失败恢复回来的记录会被当成旧记录清掉。
fn rev_seq_load(paths: &SyncPaths) -> Result<Option<u64>> {
    match std::fs::read_to_string(rev_seq_path(paths)) {
        Ok(text) => rev_seq_parse(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 备份替换的 crash 窗口：主文件已改名走、新文件还没就位。备份是上一份
            // 有效值，按它算上界；直接当首次会丢掉已分配的最大版本。
            match std::fs::read_to_string(rev_seq_backup_path(paths)) {
                Ok(text) => rev_seq_parse(&text).map(Some),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

fn rev_seq_save(paths: &SyncPaths, rev: u64) -> Result<()> {
    rclaw_store::atomic_write_private(&rev_seq_path(paths), rev.to_string().as_bytes())?;
    Ok(())
}

/// 队列锁内：给缺版本的存量记录按盘上顺序补版本（补完再取上界，避免「取样之后才补」
/// 让旧记录拿到更高版本、逃过成功清理），返回当前最大已分配版本。
///
/// 只在真的要推进已分配上界（或补写老版本）时才写序号文件：队列没有变化时（每批开头、
/// 每条尝试各取样一次）这份读改写只是读，不该给整批多出一个写失败点——它只记「已分配
/// 上界」，不是推进收尾的前置条件。
fn rev_normalize_locked(paths: &SyncPaths, closes: &mut Vec<PendingClose>) -> Result<u64> {
    let stored = rev_seq_load(paths)?.unwrap_or(0);
    let mut next = stored.max(closes.iter().map(|entry| entry.rev).max().unwrap_or(0));
    let mut changed = false;
    for entry in closes.iter_mut() {
        if entry.rev == 0 {
            next = next
                .checked_add(1)
                .ok_or_else(|| SyncError::other("同步收尾队列的版本序号已用尽"))?;
            entry.rev = next;
            changed = true;
        }
    }
    if changed || next > stored {
        rev_seq_save(paths, next)?;
    }
    if changed {
        pending_close_save(paths, closes)?;
    }
    Ok(next)
}

/// 队列锁内分配下一个版本：先把分配值写进序号文件再保存队列——队列随后保存失败或
/// 进程退出，这个值也不会被再次分配。
fn rev_allocate_locked(paths: &SyncPaths, closes: &mut Vec<PendingClose>) -> Result<u64> {
    let next = rev_normalize_locked(paths, closes)?
        .checked_add(1)
        .ok_or_else(|| SyncError::other("同步收尾队列的版本序号已用尽"))?;
    rev_seq_save(paths, next)?;
    Ok(next)
}

fn same_close_target(close: &PendingClose, storage: &SyncConfig, space_id: &str) -> bool {
    close.previous.space_id == space_id && same_close_location(&close.storage, storage)
}

/// 收尾队列的跨进程互斥：读改写之间可能还有别的进程（或旧的后台任务）在加记录。
struct CloseQueueLock(std::fs::File);

impl CloseQueueLock {
    fn acquire(paths: &SyncPaths) -> Result<CloseQueueLock> {
        paths.ensure_root()?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(paths.root.join("pending-close.lock"))?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(CloseQueueLock(file))
    }
}

impl Drop for CloseQueueLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

/// 同一个实际位置最多留这么多条收尾记录：每次没做完的重置/退出都会留一条，各自带
/// 当时的凭据与连接设置——旧的未必错（可能是后来那次填错了密码），全删掉会丢掉唯一
/// 能用的那份；但也不能无限攒。
const CLOSE_RECORDS_PER_TARGET: usize = 4;

/// 同目标的历史封顶：优先淘汰**没有**确认事实的最老一条（带着确认的留着补写）；
/// 都确认过才淘汰最老的。返回是否有改动。
fn cap_close_history(closes: &mut Vec<PendingClose>) -> bool {
    let mut changed = false;
    loop {
        let mut victim = None;
        'search: for anchor in 0..closes.len() {
            let storage = closes[anchor].storage.clone();
            let space_id = closes[anchor].previous.space_id.clone();
            let matching: Vec<usize> = closes
                .iter()
                .enumerate()
                .filter(|(_, entry)| same_close_target(entry, &storage, &space_id))
                .map(|(index, _)| index)
                .collect();
            if matching.len() > CLOSE_RECORDS_PER_TARGET {
                // 最老那条留着（它可能是唯一有效的旧凭据）：优先淘汰最老的未确认条目，
                // 都确认过就淘汰第二老的。
                victim = Some(
                    matching
                        .iter()
                        .copied()
                        .skip(1)
                        .find(|&index| !closes[index].closed_seen)
                        .unwrap_or(matching[1]),
                );
                break 'search;
            }
        }
        match victim {
            Some(index) => {
                closes.remove(index);
                changed = true;
            }
            None => break,
        }
    }
    changed
}

/// 记下一条「补完关闭」。同一目标不删旧条目（各条的凭据/连接设置都要留着试），
/// 只把已经确认过的「标记是关闭版」事实并进这一条，并给同目标的历史封顶。
async fn pending_close_add(paths: &SyncPaths, close: PendingClose) -> Result<()> {
    let paths = paths.clone();
    blocking(move || {
        let _lock = CloseQueueLock::acquire(&paths)?;
        let mut closes = pending_close_load(&paths)?;
        // 老版本写下的条目先补上各自的身份：后面按身份结算才不会串。
        for old in &mut closes {
            if old.id.is_empty() {
                old.id = crate::ids::random_hex::<8>();
            }
        }
        let mut close = close;
        close.rev = rev_allocate_locked(&paths, &mut closes)?;
        // 确认事实是目标的性质：并进新条目的同时，留在旧的里面也不算丢。
        for old in closes
            .iter()
            .filter(|old| same_close_target(old, &close.storage, &close.previous.space_id))
        {
            close.closed_seen |= old.closed_seen;
        }
        closes.push(close);
        cap_close_history(&mut closes);
        pending_close_save(&paths, &closes)
    })
    .await
}

/// 把「本机确认过标记是关闭版」落到这条记录上：就算这一批后面的补写超时、被取消、
/// 进程退出，下次也知道关闭已经开始。记录如果已经按同一目标换了身份（退出时为这次
/// 重置又加了一条），事实落到新那条上——它说的是这个空间，不是这条记录。
async fn pending_close_mark_seen(
    paths: &SyncPaths,
    id: &str,
    storage: &SyncConfig,
    space_id: &str,
) -> Result<()> {
    let paths = paths.clone();
    let (id, storage, space_id) = (id.to_string(), storage.clone(), space_id.to_string());
    blocking(move || {
        let _lock = CloseQueueLock::acquire(&paths)?;
        let mut closes = pending_close_load(&paths)?;
        let slot = closes
            .iter()
            .position(|close| close.id == id)
            .or_else(|| {
                closes
                    .iter()
                    .position(|close| same_close_target(close, &storage, &space_id))
            });
        let Some(slot) = slot else {
            return Ok(());
        };
        closes[slot].closed_seen = true;
        pending_close_save(&paths, &closes)
    })
    .await
}

/// 给老版本写下的、没有身份的条目补上身份（并落盘）：身份共用空串的话，一条的结算
/// 会落到另一条上。
async fn pending_close_ensure_ids(paths: &SyncPaths) -> Result<Vec<PendingClose>> {
    let paths = paths.clone();
    blocking(move || {
        let _lock = CloseQueueLock::acquire(&paths)?;
        let mut closes = pending_close_load(&paths)?;
        let mut changed = false;
        for close in &mut closes {
            if close.id.is_empty() {
                close.id = crate::ids::random_hex::<8>();
                changed = true;
            }
        }
        // 先按读入时的确认状态封顶，再把确认事实传播：传播会把同组全标成「已确认」，
        // 提前传播会让封顶的回退规则淘汰错人（可能丢掉唯一带有效凭据的快照）。
        let mut closes = closes;
        changed |= cap_close_history(&mut closes);
        // 老记录没有版本：按盘上顺序补上，之后的清理边界才有意义。
        rev_normalize_locked(&paths, &mut closes)?;
        // 「已确认关闭」是**目标**的事实，不是某条记录私有的：同目标（同空间 + 同位置）
        // 里只要有一条确认过，保留下来的其余条目也带着这个事实去补写——否则有效密码
        // 那条可能被当成「远端没开始关闭」删掉，只剩失效密码那条永远补不完。
        for index in 0..closes.len() {
            if closes[index].closed_seen {
                continue;
            }
            let storage = closes[index].storage.clone();
            let space_id = closes[index].previous.space_id.clone();
            if closes
                .iter()
                .any(|other| other.closed_seen && same_close_target(other, &storage, &space_id))
            {
                closes[index].closed_seen = true;
                changed = true;
            }
        }
        if changed {
            pending_close_save(&paths, &closes)?;
        }
        Ok(closes)
    })
    .await
}

/// 一条收尾完成后清同目标的记录：关闭做完（`Done`）就全删；确认远端没有关闭依据
/// （`NotStarted`）时只删**没有确认过关闭**的条目——带着「已确认关闭」事实的条目还
/// 要留着补写（迟到创建可能刚把标记改回开放）。
async fn pending_close_finish_target(
    paths: &SyncPaths,
    storage: &SyncConfig,
    space_id: &str,
    keep_confirmed: bool,
    only_batch: Option<&std::collections::HashSet<String>>,
    // 只删版本不高于它的记录（`u64::MAX` = 不设限）：这次尝试期间/之后被并发失败
    // 恢复回来的记录版本更高，要留着。
    rev_limit: u64,
) -> Result<()> {
    let paths = paths.clone();
    let (storage, space_id) = (storage.clone(), space_id.to_string());
    let batch = only_batch.cloned();
    blocking(move || {
        let _lock = CloseQueueLock::acquire(&paths)?;
        let mut closes = pending_close_load(&paths)?;
        // 盘上同目标只要有一条记着「已确认关闭」，这一批的「没开始关闭」结论就不成立
        //（可能刚被别的条目记下）：一条都别删，留下带可用凭据的那条去补写。
        let any_confirmed = keep_confirmed
            && closes
                .iter()
                .any(|close| close.closed_seen && same_close_target(close, &storage, &space_id));
        closes.retain(|close| {
            if !same_close_target(close, &storage, &space_id) {
                return true;
            }
            if close.rev > rev_limit {
                return true;
            }
            if any_confirmed {
                return true;
            }
            if keep_confirmed && close.closed_seen {
                return true;
            }
            // 「远端没开始关闭」只是本批的这一条读到的：批次开始之后才入队的记录
            //（比如前台刚失败的重置又留了一条）不归这批删。
            if let Some(batch) = &batch {
                if !batch.contains(&close.id) {
                    return true;
                }
            }
            false
        });
        pending_close_save(&paths, &closes)
    })
    .await
}

/// 一次补写失败时把这条记录重新落盘：并发的成功清理可能刚好把同目标清掉，但这次
/// 失败之后墓碑/兼容标记可能已经被中途改坏，还需要这条记录继续重试。按身份 upsert，
/// 不会造出重复条目。
async fn pending_close_restore(paths: &SyncPaths, close: &PendingClose) -> Result<()> {
    let paths = paths.clone();
    let close = close.clone();
    blocking(move || {
        let _lock = CloseQueueLock::acquire(&paths)?;
        let mut closes = pending_close_load(&paths)?;
        let rev = rev_allocate_locked(&paths, &mut closes)?;
        match closes.iter_mut().find(|entry| entry.id == close.id) {
            Some(entry) => {
                entry.closed_seen |= close.closed_seen;
                entry.rev = rev;
            }
            None => {
                let mut close = close;
                close.rev = rev;
                closes.push(close);
                cap_close_history(&mut closes);
            }
        }
        pending_close_save(&paths, &closes)
    })
    .await
}

/// 这次尝试开始时的队列版本上界（在队列锁内读）：清理只删版本不高于它的记录，
/// 期间/之后被并发失败恢复的记录（版本更高）留着。
async fn pending_close_rev_limit(paths: &SyncPaths) -> Result<u64> {
    let paths = paths.clone();
    blocking(move || {
        let _lock = CloseQueueLock::acquire(&paths)?;
        let mut closes = pending_close_load(&paths)?;
        rev_normalize_locked(&paths, &mut closes)
    })
    .await
}

/// 结算一条收尾：只按记录身份删——期间同目标可能已经被一条新记录替代，不能顺手删掉。
async fn pending_close_finish(paths: &SyncPaths, id: &str) -> Result<()> {
    let paths = paths.clone();
    let id = id.to_string();
    blocking(move || {
        let _lock = CloseQueueLock::acquire(&paths)?;
        let mut closes = pending_close_load(&paths)?;
        closes.retain(|close| close.id != id);
        pending_close_save(&paths, &closes)
    })
    .await
}

/// 把退出前没能收尾的关闭（[`PendingClose`]）补完：只在远端确实显示关闭已经开始时
/// 动手（[`remote::ensure_closed_if_started`]），做完删记录，失败留着下次再试。
///
/// 退出时本机已经清掉了旧空间的密钥，之后只有这些记录能关掉那些空间：不补完的话，
/// 迟到的创建把兼容标记覆盖回开放后，旧空间又能被当成可用。
///
/// 每一项的结算都（按记录身份）立刻落盘，不依赖整批跑完：超时、取消、进程退出都不会
/// 丢掉已经得到的结论。当前配置从宿主重读入口拿，拿不到时用记录里的那份。
pub(crate) async fn finish_pending_close(
    paths: &SyncPaths,
    refresh: Option<crate::engine::ConfigRefresh>,
) -> Result<()> {
    let closes = pending_close_ensure_ids(paths).await?;
    if closes.is_empty() {
        return Ok(());
    }
    let batch_ids: std::collections::HashSet<String> =
        closes.iter().map(|close| close.id.clone()).collect();
    let mut first_error = None;
    for mut close in closes {
        // 当前配置与凭据逐条读（同一个阻塞任务里连着读，缩小两次读取之间的窗口）：
        // 一批里可能等很久，期间设置可能又改过。配置读不出来就不拿旧设置去连（可能
        // 已经不允许明文了），留着下次再试。
        let settings = {
            let paths = paths.clone();
            let refresh = refresh.clone();
            blocking(move || {
                // 配置与 secrets 是成套写的（保存方持存储协议锁）：拿不到锁就先跳过，
                // 免得读到「新的开关 + 旧的密码」这种半新半旧的组合。
                let Some(_lock) = super::worker::StorageLock::try_acquire(&paths)? else {
                    return Ok(None);
                };
                let config = match &refresh {
                    Some(refresh) => match refresh() {
                        Ok(config) => config,
                        Err(()) => return Ok(None),
                    },
                    None => None,
                };
                Ok(Some((config, Secrets::load(&paths).ok())))
            })
            .await
            .ok()
            .flatten()
        };
        let Some((current, secrets)) = settings else {
            first_error.get_or_insert(SyncError::other(
                "同步设置正在保存或暂时读不出来，稍后再补完空间关闭",
            ));
            continue;
        };
        let rev_limit = pending_close_rev_limit(paths).await?;
        let closed = {
            // 单条也限时：卡住的一条不能把后面的健康记录一直饿着。
            match tokio::time::timeout(
                CLOSE_ENTRY_TIMEOUT,
                finish_one_close(
                    &mut close,
                    paths,
                    &batch_ids,
                    rev_limit,
                    current.as_ref(),
                    secrets.as_ref(),
                    refresh.as_ref(),
                ),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    // 超时可能正卡在远端写入上：把记录补回去，留给下一轮复核。
                    if let Err(restore) = pending_close_restore(paths, &close).await {
                        tracing::warn!("把超时的空间关闭记录重新落盘失败: {restore}");
                    }
                    Err(SyncError::other("补完这条空间关闭超时"))
                }
            }
        };
        if let Err(error) = closed {
            tracing::warn!(
                "补完空间 {} 的关闭失败（稍后再试）: {error}",
                close.previous.space_id
            );
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn finish_one_close(
    close: &mut PendingClose,
    paths: &SyncPaths,
    batch_ids: &std::collections::HashSet<String>,
    // 这次尝试开始时的队列版本上界：成功清理只删版本不高于它的记录，这次尝试期间/
    // 之后被并发失败恢复回来的记录留着。
    rev_limit: u64,
    current: Option<&SyncConfig>,
    current_secrets: Option<&Secrets>,
    refresh: Option<&crate::engine::ConfigRefresh>,
) -> Result<()> {
    if !close.previous.is_paired() {
        pending_close_finish(paths, &close.id).await?;
        return Ok(());
    }
    let mut previous = close.previous.clone();
    let mut at = close.storage.clone();
    if let Some(current) = current {
        if storage_binding(&at) == storage_binding(current) {
            // 同一个存储位置：连接设置以当前这份为准（退出之后可能换过证书指纹）；
            // 当前设置里确实有这个记录账户的凭据时凭据也换（可能按「设备丢失」流程
            // 改过密码）——本机已经退出、没有凭据时，记录里的那份才是能用的。
            refresh_connection_settings(&mut at, current);
            if let Some(secrets) = current_secrets {
                // 当前设置里确实有「记录里那个账户」的凭据才换（可能按「设备丢失」
                // 流程改过密码）；本机已经退出、没有凭据时，记录里的才是能用的。
                if !secrets.storage_secret(&at).is_empty() {
                    refresh_credentials(&mut previous, secrets);
                }
            }
        }
    }
    // 取样到建连之间设置可能又变了（保存方不与我们共用这段时间的锁）：建连前在协议锁
    // 内再读一次，比对该后端的敏感项（明文开关、证书指纹、区域），**客户端也就在这把
    // 锁里建好**——锁一放，剩下的到首个请求之间只剩一次调度和一次对象包装。
    // 只在「记录位置就是当前设置的位置」（连接项是从当前设置刷新的）时核对；记录指向
    // 别的存储是正常情况，不能因为它和当前设置不同就永远跳过。
    let refreshed = current.is_some_and(|current| storage_binding(&at) == storage_binding(current))
        && at.transport != "file";
    let root = if refreshed {
        let at_build = at.clone();
        let previous_build = previous.clone();
        let paths_for_check = paths.clone();
        let refresh_for_check = refresh.cloned();
        let built = blocking(move || -> Result<Option<Box<dyn SyncTransport>>> {
            let Some(_lock) = super::worker::StorageLock::try_acquire(&paths_for_check)? else {
                return Ok(None);
            };
            let latest = match &refresh_for_check {
                Some(refresh) => match refresh() {
                    Ok(config) => config,
                    Err(()) => return Ok(None),
                },
                None => return Ok(None),
            };
            let Some(latest) = latest else {
                return Ok(None);
            };
            if storage_binding(&at_build) != storage_binding(&latest) {
                return Ok(None);
            }
            let unchanged = match at_build.transport.as_str() {
                "webdav" => {
                    at_build.webdav.allow_http == latest.webdav.allow_http
                        && at_build.webdav.pinned_cert_sha256
                            == latest.webdav.pinned_cert_sha256
                }
                "s3" => {
                    at_build.s3.allow_http == latest.s3.allow_http
                        && at_build.s3.region == latest.s3.region
                }
                _ => true,
            };
            if !unchanged {
                return Ok(None);
            }
            Ok(Some(root_transport(&at_build, &previous_build)?))
        })
        .await?;
        let Some(root) = built else {
            return Err(SyncError::other("同步设置刚变过，稍后再补完空间关闭"));
        };
        root
    } else {
        let at_build = at.clone();
        let previous_build = previous.clone();
        blocking(move || root_transport(&at_build, &previous_build)).await?
    };
    let space_id = previous.space_id.clone();
    let keys = match previous.keys() {
        Ok(keys) => Arc::new(keys),
        // 记录里的密钥读不出来：这条永远补不了，丢掉别每 30 秒重试。
        Err(error) => {
            tracing::warn!("收尾记录里的空间密钥无法读取，丢弃这条: {error}");
            pending_close_finish(paths, &close.id).await?;
            return Ok(());
        }
    };
    let transport = Scoped::new(root, &space_id);
    // 先看一眼标记：验证过是关闭版就立刻记进记录（这一批后面超时/取消也不丢）。
    if !close.closed_seen {
        if remote::marker_closed_state(&transport, &keys, &space_id).await? == Some(true) {
            pending_close_mark_seen(paths, &close.id, &close.storage, &space_id).await?;
            close.closed_seen = true;
        }
    }
    // 补写过程中再读到「已验证关闭」时，先落盘（按 id，缺 id 按同目标）再动远端。
    let confirm = || -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<()>> + Send + '_>,
    > {
        Box::pin(pending_close_mark_seen(
            paths,
            &close.id,
            &close.storage,
            &space_id,
        ))
    };
    match remote::ensure_closed_if_started(
        &transport,
        &keys,
        &space_id,
        close.closed_seen,
        Some(&confirm),
    )
    .await
    {
        remote::CloseAttempt::Done => {
            // 关闭做完：同目标的历史快照都已完成使命。删除以这次尝试开始的时刻为界，
            // 期间/之后被并发失败恢复回来的记录（`at_ms` 更晚）留着。
            pending_close_finish_target(
                paths,
                &close.storage,
                &space_id,
                false,
                None,
                rev_limit,
            )
            .await?;
            Ok(())
        }
        remote::CloseAttempt::NotStarted => {
            // 远端没有任何关闭依据：本批的同目标未确认条目不再需要；带着「已确认关闭」
            // 事实的、以及批次之后新入队的，都留着。
            pending_close_finish_target(
                paths,
                &close.storage,
                &space_id,
                true,
                Some(batch_ids),
                u64::MAX,
            )
            .await?;
            Ok(())
        }
        // 这一遍补写里又确认了关闭（第一次读时还没写成）：事实同样要落盘。
        remote::CloseAttempt::ConfirmedButFailed(error)
        | remote::CloseAttempt::WriteFailed(error) => {
            if !close.closed_seen {
                pending_close_mark_seen(paths, &close.id, &close.storage, &space_id).await?;
                close.closed_seen = true;
            }
            if let Err(restore) = pending_close_restore(paths, close).await {
                tracing::warn!("把失败的空间关闭记录重新落盘失败: {restore}");
            }
            Err(error)
        }
        // 只读失败：这次没改远端，并发成功清理过的同目标不需要为它留重试记录。
        remote::CloseAttempt::Failed(error) => Err(error),
    }
}

/// 上一次的空间切换没做完：按记录与远端的实际状态接着做，做完删记录。没有记录就是
/// 无事可做；做完了返回这次完成的动作（调用方据此判断「重试的那次重置已经做完了」）。
/// 每一步都幂等，重复走不产生第二个空间。
///
/// 恢复用的存储是**记录里的那一份**：配置在这期间被改到别的位置（换了前缀、换了桶）
/// 时，回原位置继续做，并在状态里留一句话说明；不然新位置上的 404 会被当成「旧空间已
/// 不存在」，隔离就没有真正发生。
pub(crate) async fn resume_pending(
    ctx: &Ctx,
    config: &SyncConfig,
    secrets: &mut Secrets,
    keepalive: Option<std::sync::Arc<std::fs::File>>,
) -> Result<Option<SpaceAction>> {
    let Some(pending) = pending_load(&ctx.paths)? else {
        return Ok(None);
    };
    match pending.action {
        SpaceAction::Reset => {
            finish_reset(ctx, config, secrets, pending, keepalive.clone()).await?;
            Ok(Some(SpaceAction::Reset))
        }
        SpaceAction::Leave => {
            let mut next = pending.previous.clone();
            next.leave_space();
            let ctx_for_task = ctx.clone();
            let saved = next.clone();
            blocking(move || {
                // 引擎锁跟着写入活到底：调用方的等待被取消时，已派发的阻塞写仍然
                // 握着锁，别的进程不能在清理进行到一半时接手。
                let _keep = keepalive;
                saved.save(&ctx_for_task.paths)?;
                reset_local_state(&ctx_for_task, None, None)
            })
            .await?;
            pending_clear(&ctx.paths)?;
            *secrets = next;
            Ok(Some(SpaceAction::Leave))
        }
    }
}

/// 重置的后半程（入口见 [`reset`]）：新空间立起来（已立就核对）、旧空间废弃（已废弃或
/// 不在了就跳过）、本机切到新空间，全部走完才删记录。哪一步失败都原样上抛：记录还在，
/// 下次打开引擎或重试从这一步接着做。
///
/// 恢复用的是**记录里的存储配置**：改了配置（换了前缀/桶）时回原位置做，不然新位置上的
/// 404 会被误当成「旧空间已不在」，隔离就没发生。
async fn finish_reset(
    ctx: &Ctx,
    config: &SyncConfig,
    secrets: &mut Secrets,
    pending: PendingSpace,
    keepalive: Option<std::sync::Arc<std::fs::File>>,
) -> Result<()> {
    let Some(next) = pending.replacement.clone() else {
        // 记录不完整（老版本或写坏了）：没法接着做，丢弃记录，本机保持现状。
        tracing::warn!("待完成的重置记录里没有新空间的密钥，丢弃");
        pending_clear(&ctx.paths)?;
        return Err(SyncError::other(
            "上次的重置没有留下能接着做的新空间记录；请再重试一次「重置同步空间」",
        ));
    };
    // 老记录（没有存储字段）沿用当时的配置；位置换了就在原位置继续做，并告诉用户。
    let at: SyncConfig = pending.storage.clone().unwrap_or_else(|| config.clone());
    if storage_binding(&at) != storage_binding(config) {
        tracing::warn!(
            "重置的恢复在原来的存储位置（{}）上继续，当前配置指向的是另一处",
            storage_binding(&at)
        );
        status::notice(
            &ctx.paths,
            "reset-resumed-elsewhere",
            &next.space_id,
            "重置在原来的存储位置上继续完成",
            &format!(
                "上次没做完的「重置同步空间」是在另一个存储位置上开始的。这次配置指向了\
                 别处，恢复回到原来的位置（{}）继续，以免把那里的 404 误当成「旧空间已经\
                 关闭」。当前配置没有被改动；确认重置完成之后，可以再把存储改回你想要的\
                 地址。",
                storage_binding(&at)
            ),
        );
    }
    // 恢复用记录的存储位置，但凭据跟着当前这份 secrets 走（同一个存储账户时）：这期间
    // 用户可能已经在设置里换过密码——旧密码被撤销时，新密码同样开得了这个空间。
    let mut next = next;
    let mut old_secrets = pending.previous.clone();
    if storage_binding(&at) == storage_binding(config) {
        refresh_credentials(&mut next, secrets);
        // 旧空间的凭据只在当前设置确实有「那个账户」的凭据时换：用户名带尾随空格这类
        // 规范化差异下，换错账户会让记录里本来能用的密码变成空。
        if !secrets.storage_secret(&at).is_empty() {
            refresh_credentials(&mut old_secrets, secrets);
        }
    }
    let fresh = load_live(&at, &next).await?;
    // 1. 新空间立起来。已存在就核对密钥（幂等）；已废弃说明情况不对，停下来让人看。
    create_file_namespace(&at, &next.space_id).await?;
    remote::open_space(fresh.transport(), &fresh.keys, &next.space_id, true).await?;
    // 2. 废弃旧空间。先核对远端确实是旧空间再关；已经关了（或不在了）就跳过。位置上
    //    不是当初那个空间（被改过、别的空间）时给出具体错误，不当作「已关闭」跳过。
    //    这里不再删新空间回滚：如果关闭其实已在远端生效、只是响应丢了，新空间是唯一能
    //    接着用的。
    if pending.previous.is_paired() {
        let (open, missing) = match read_old_space(&at, &old_secrets).await {
            Ok(Some(file)) => {
                file.verify(&old_secrets.keys()?, &old_secrets.space_id)
                    .map_err(|error| {
                        SyncError::other(format!(
                            "旧空间的位置（{}）上不是当初那个空间，不能当作「已关闭」跳过；\
                             请核对存储地址后重试: {error}",
                            storage_binding(&at)
                        ))
                    })?;
                (!file.closed, false)
            }
            Ok(None) => (false, true),
            Err(error) => {
                return Err(SyncError::other(format!(
                    "恢复重置：读不到旧空间（{error}）。存储恢复可访问后重试；\
                     不想继续这次切换可以用「退出同步空间」收场"
                )));
            }
        };
        if missing {
            // 旧空间目录不在了（被清掉、第三方工具还没同步过来）：补写一个可验证的
            // 「已关闭」标记。迟到的旧副本要是又出现、带着开放状态，至少这里的结论
            // 有据可查，重试也不会一直把「目录缺失」当成不确定。
            // 注意从**外层根**加空间前缀写：旧空间子目录本身不存在，直接把它当传输根
            // 会连根都打不开（文件后端要求根目录先存在）。
            let space_id = old_secrets.space_id.clone();
            let keys = Arc::new(old_secrets.keys()?);
            let (config, secrets) = (at.clone(), old_secrets.clone());
            let root = blocking(move || root_transport(&config, &secrets)).await?;
            let transport = Scoped::new(root, &space_id);
            remote::close_space(&transport, &keys, &space_id).await?;
        } else if open {
            let old = load_live(&at, &old_secrets).await?;
            remote::close_space(old.transport(), &old.keys, &old.space_id).await?;
        } else {
            // 标记已是关闭版，但上一次关闭可能只写成了标记（墓碑那次 PUT 失败）：
            // 先把这个「已确认关闭」的事实落盘——后面 load_live、补墓碑失败或进程退出
            // 都不会丢；补墓碑成功再把这条记录撤掉。
            let close = PendingClose {
                id: crate::ids::random_hex::<8>(),
                rev: 0,
                storage: at.clone(),
                previous: old_secrets.clone(),
                closed_seen: true,
            };
            let added = pending_close_add(&ctx.paths, close.clone()).await;
            if let Err(write_error) = &added {
                tracing::warn!("记下没补完的空间关闭失败: {write_error}");
            }
            let old = load_live(&at, &old_secrets).await?;
            // 上界读不到只影响「Done 之后清同目标旧记录」，不能挡远端复核与本机提交。
            let rev_limit = match pending_close_rev_limit(&ctx.paths).await {
                Ok(limit) => Some(limit),
                Err(error) => {
                    tracing::warn!("读收尾队列版本上界失败（这次不清理旧记录）: {error}");
                    None
                }
            };
            // 两份对象都确认了才撤这条记录；撤记录只是收尾，撤不掉留给后台再验一遍
            //（幂等），不能把本机提交挡在后面。
            match remote::ensure_closed_if_started(
                old.transport(),
                &old.keys,
                &old.space_id,
                true,
                // 这条记录的「已确认关闭」在上面已经落盘了，不需要再回调。
                None,
            )
            .await
            {
                remote::CloseAttempt::Done => {
                    // 关闭已完成：清同目标的历史快照。以这次尝试开始的版本上界为界，
                    // 期间/之后被并发失败恢复回来的记录留着（下一轮复核会把它修好）。
                    if let Some(rev_limit) = rev_limit {
                        if let Err(error) = pending_close_finish_target(
                            &ctx.paths,
                            &close.storage,
                            &old.space_id,
                            false,
                            None,
                            rev_limit,
                        )
                        .await
                        {
                            tracing::warn!("撤掉已完成的收尾记录失败（留给后台重试）: {error}");
                        }
                    }
                }
                remote::CloseAttempt::NotStarted => {}
                remote::CloseAttempt::ConfirmedButFailed(error)
                | remote::CloseAttempt::Failed(error)
                | remote::CloseAttempt::WriteFailed(error) => return Err(error),
            }
        }
    }
    // 3. 本机切到新空间（记「这个空间是本机建的」+ secrets 落盘），做完才删记录。
    save_created(ctx, &next, keepalive).await?;
    pending_clear(&ctx.paths)?;
    *secrets = next;
    Ok(())
}

/// 恢复时把记录里的存储凭据换成当前 secrets 里的（只在同一个存储账户上调用）。
fn refresh_credentials(target: &mut Secrets, current: &Secrets) {
    target.webdav_account = current.webdav_account.clone();
    target.webdav_password = current.webdav_password.clone();
    target.s3_account = current.s3_account.clone();
    target.s3_secret_key = current.s3_secret_key.clone();
}

/// 读旧空间的 `space.json`。`file` 后端平常把空间子目录本身当传输根（[`open_live`]）：
/// 子目录没了会把「命名空间不在了」变成整个存储不可访问。这里改从外层根 + 空间前缀
/// 进——共享目录还在而旧空间子目录没了（被清掉、没同步过来）时，读不到 `space.json`
/// 就是「已关闭」，恢复照常收敛。
async fn read_old_space(config: &SyncConfig, secrets: &Secrets) -> Result<Option<SpaceFile>> {
    let (config, secrets, space_id) = (
        config.clone(),
        secrets.clone(),
        secrets.space_id.clone(),
    );
    let root = blocking(move || root_transport(&config, &secrets)).await?;
    let transport = Scoped::new(root, &space_id);
    remote::read_space(&transport).await
}

/// 创建一个新的同步空间，本机是它的第一台设备。存储上先写出 `space.json`，写不通就
/// 什么都不改。
pub(crate) async fn create(ctx: &Ctx, config: &SyncConfig, secrets: &mut Secrets) -> Result<()> {
    if secrets.is_paired() {
        return Err(SyncError::other(
            "本机已经在一个同步空间里了；要换一个空间，请先退出当前的",
        ));
    }
    let mut next = secrets.clone();
    next.space_id = new_space_id();
    next.key = SpaceKey::generate().to_base64();
    let live = load_live(config, &next).await?;
    create_file_namespace(config, &next.space_id).await?;
    remote::open_space(live.transport(), &live.keys, &next.space_id, true).await?;
    save_created(ctx, &next, None).await?;
    *secrets = next;
    Ok(())
}

/// 用配对码加入一个已有的同步空间。先到存储上核对这个空间确实在、密钥对得上，再保存。
/// 返回加入之后的配置（配对码里带了存储设置的话与原来不同），宿主负责把它写进
/// `rclaw.toml`。
/// 加入的校验结果：远端已经核对过，剩下的是本机提交（写凭据 + 采用配置）。
pub(crate) struct JoinPlan {
    paths: SyncPaths,
    joined: SyncConfig,
    next: Secrets,
}

impl JoinPlan {
    /// 本机提交：只写 secrets 并返回采用后的配置。调用方负责在协议锁内完成这一步
    /// 以及随后的安装/发布（远端校验不占协议锁）。
    pub(crate) async fn commit(self, secrets: &mut Secrets) -> Result<SyncConfig> {
        let (paths, joined, next) = (self.paths, self.joined, self.next);
        let saved = next.clone();
        blocking(move || saved.save(&paths)).await?;
        *secrets = next;
        Ok(joined)
    }
}

/// 加入的第一步：核对远端确实是这个空间、密钥对得上（不写本机任何东西）。本机提交
/// 由调用方在存储协议锁内做——校验期间可能有人保存/关闭设置。
pub(crate) async fn join_verify(
    paths: &SyncPaths,
    config: &SyncConfig,
    secrets: &Secrets,
    code: &str,
) -> Result<JoinPlan> {
    if secrets.is_paired() {
        return Err(SyncError::other(
            "本机已经在一个同步空间里了；要加入另一个，请先退出当前的",
        ));
    }
    let code = PairingCode::parse(code)?;
    let mut next = secrets.clone();
    let joined = apply_pairing(config, &mut next, &code);
    joined.validate()?;
    let live = load_live(&joined, &next).await?;
    remote::open_space(live.transport(), &live.keys, &next.space_id, false).await?;
    Ok(JoinPlan {
        paths: paths.clone(),
        joined,
        next,
    })
}

/// 退出同步空间：密钥与存储凭据从本机清掉，本机的同步状态回到「没同步过」。会话、记忆
/// 等数据原样留在本机；存储上的密文不动（别的设备还在用）。
///
/// 动手之前先落盘一条「退出」记录：改成未配对之后清同步状态这一步要是断了（掉电、
/// 写不了盘），重试时本机已经是未配对，靠记录把剩下的清完（见 [`resume_pending`]）。
pub(crate) async fn leave(
    ctx: &Ctx,
    config: &SyncConfig,
    secrets: &mut Secrets,
    keepalive: Option<std::sync::Arc<std::fs::File>>,
) -> Result<()> {
    // 上次没做完的切换先做完（可能是一次重置：先收敛到新空间），再决定退不退。
    // 上次没做完的切换先试着收尾；收不了也不能挡住退出——用户就是要离开这个空间。
    // 丢掉恢复记录：它的目标（新空间）本机不再使用，留着会让下次启动又把本机拉回
    // 配对状态。远端可能仍保留原空间的状态，在状态页交代清楚。
    if let Err(error) = resume_pending(ctx, config, secrets, keepalive.clone()).await {
        // 失败的是「上一次退出」的记录：清库可能只做了一半，记录必须留着（下次打开
        // 或重试接着收尾），错误交给调用方——不能丢掉记录再报成功。
        if pending_load(&ctx.paths)?.is_some_and(|pending| pending.action == SpaceAction::Leave) {
            return Err(error);
        }
        tracing::warn!("退出前收尾未完成的切换失败，按本机退出继续: {error}");
        // 没做完的可能是重置：旧空间也许已经关到一半（比如墓碑写成、兼容标记没写成）。
        // 本机马上要清掉旧空间的密钥，之后没人能再补——先留一条只做远端关闭的收尾
        // 记录，下一次打开时凭记录里的旧空间凭据把关闭补完，不碰本机配对。
        if let Some(pending) = pending_load(&ctx.paths)? {
            if pending.action == SpaceAction::Reset && pending.previous.is_paired() {
                let at = pending.storage.clone().unwrap_or_else(|| config.clone());
                let mut previous = pending.previous.clone();
                if storage_binding(&at) == storage_binding(config)
                    && !secrets.storage_secret(&at).is_empty()
                {
                    refresh_credentials(&mut previous, secrets);
                }
                pending_close_add(
                    &ctx.paths,
                    PendingClose {
                        id: crate::ids::random_hex::<8>(),
                        rev: 0,
                        storage: at,
                        previous,
                        closed_seen: false,
                    },
                )
                .await?;
            }
        }
        status::notice(
            &ctx.paths,
            "leave-after-failed-resume",
            "",
            "退出同步空间：上次未完成的切换没有收尾",
            &format!(
                "本机已经退出同步空间，不会再同步。上次没做完的空间切换（重置或退出）\
                 没能收尾：如果旧空间已经关到一半，本机会在存储恢复后把它补完（其它设备\
                 届时需要重新配对）；没有开始关闭的话，远端保持原样。具体原因：{error}"
            ),
        );
        pending_clear(&ctx.paths)?;
    }
    if !secrets.is_paired() {
        return Ok(());
    }
    let mut next = secrets.clone();
    next.leave_space();
    pending_save(
        &ctx.paths,
        &PendingSpace {
            action: SpaceAction::Leave,
            storage: Some(config.clone()),
            previous: secrets.clone(),
            replacement: None,
        },
    )?;
    let ctx_for_task = ctx.clone();
    let saved = next.clone();
    blocking(move || {
        // 同 [`resume_pending`]：锁的生命周期覆盖这次阻塞写入（调用方被超时丢弃
        // 时，别的进程仍不能进来接手）。
        let _keep = keepalive;
        saved.save(&ctx_for_task.paths)?;
        reset_local_state(&ctx_for_task, None, None)
    })
    .await?;
    pending_clear(&ctx.paths)?;
    *secrets = next;
    Ok(())
}

/// 重置同步空间（§11.2，设备丢失之后用）：新建空间和新密钥，旧空间标记为已废弃——其余
/// 设备读到这个标记就停止同步，等用户拿新的配对码重新加入。本机随后从本地数据全量重新
/// 发布（换了空间，下次打开时清掉本机的同步状态）。
///
/// 动远端之前先落盘一条「重置」记录（新旧空间、新旧密钥都在里面）：中途断掉的话，重启
/// 或重试按 [`finish_reset`] 从记录接着做，收敛到同一个新空间；不会另开一个新空间，也
/// 不会出现「旧空间已关、新密钥却没落到本机」的死角。
pub(crate) async fn reset(ctx: &Ctx, config: &SyncConfig, secrets: &mut Secrets) -> Result<()> {
    // 上次没做完的重置 / 退出：先接着做完，收敛到同一次切换的结果。刚接完的是一次
    // 重置的话，本机已经在那次重置的替代空间里——这次调用到此为止，用户再点一次才是
    // 在新空间里再重置（不会接着又开出第二个新空间）。
    if resume_pending(ctx, config, secrets, None).await? == Some(SpaceAction::Reset) {
        return Ok(());
    }
    if !secrets.is_paired() {
        return Err(SyncError::other("还没有创建或加入同步空间"));
    }
    let mut next = secrets.clone();
    next.space_id = new_space_id();
    next.key = SpaceKey::generate().to_base64();
    // 动远端之前先把这次切换记下来（连同它用的存储配置）。
    let pending = PendingSpace {
        action: SpaceAction::Reset,
        storage: Some(config.clone()),
        previous: secrets.clone(),
        replacement: Some(next.clone()),
    };
    pending_save(&ctx.paths, &pending)?;
    finish_reset(ctx, config, secrets, pending, None).await
}

/// 「测试连接」（§10）：在存储根下把用到的操作逐个走一遍。
pub(crate) async fn test_connection(config: &SyncConfig, secrets: &Secrets) -> Vec<SelfTestStep> {
    let (config, secrets) = (config.clone(), secrets.clone());
    let transport = match blocking(move || root_transport(&config, &secrets)).await {
        Ok(transport) => transport,
        Err(error) => {
            return vec![SelfTestStep {
                name: "连接设置".into(),
                ok: false,
                detail: error.to_string(),
            }]
        }
    };
    transport::self_test(transport.as_ref()).await
}

// ---------------------------------------------------------------------------
// 本机的同步状态
// ---------------------------------------------------------------------------

fn remove_files_in(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 把一条消息的附件清单换成新密钥下的对象名。清单没变返回 `None`。
fn rekey_refs(column: &str, renamed: &HashMap<String, String>) -> Option<String> {
    let before = StoredRefs::parse(Some(column))?;
    let mut refs = before.clone();
    refs.files.retain_mut(|file| match renamed.get(&file.blob) {
        Some(new) => {
            file.blob = new.clone();
            true
        }
        // 文件不在本机（还没取回，或已经删了）：新空间里拿不出这个对象。
        None => false,
    });
    for artifact in &mut refs.artifacts {
        if let Some(blob) = artifact.blob.take() {
            artifact.blob = renamed.get(&blob).cloned();
        }
    }
    (refs != before).then(|| refs.to_column())
}

/// 换了空间就换了密钥，附件对象名（内容摘要的带密钥摘要）也跟着变：本机有文件的那些
/// 重新算名字，消息的附件清单跟着改写；只在别的设备上、还没取回的那些就此作废。
///
/// 可以重复执行（中途退出后下次从头再来）：清单改写在前、登记替换在后，而且新名字也
/// 映射到它自己。
fn rekey_blobs(ctx: &Ctx, keys: &Keys) -> Result<()> {
    let rows: Vec<BlobRow> = db::read(ctx.db(), |conn| {
        let mut stmt =
            conn.prepare("SELECT blob_id, rel_path, size, state FROM sync_blob ORDER BY rowid")?;
        let rows = stmt.query_map([], |row| {
            Ok(BlobRow {
                blob_id: row.get(0)?,
                rel_path: row.get(1)?,
                size: row.get::<_, i64>(2)?.max(0) as u64,
                state: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    // 一、读文件、算新名字：在库锁之外。
    let mut renamed: HashMap<String, String> = HashMap::new();
    let mut kept: Vec<BlobRow> = Vec::new();
    for row in rows {
        if row.state == BLOB_REMOTE {
            continue;
        }
        let Ok(path) = ctx.env.checked_path(&row.rel_path) else {
            continue;
        };
        let Some((new_id, size)) = attach::hash_file_len(keys, &path) else {
            continue;
        };
        renamed.insert(row.blob_id.clone(), new_id.clone());
        renamed.insert(new_id.clone(), new_id.clone());
        kept.push(BlobRow {
            blob_id: new_id,
            rel_path: row.rel_path,
            size,
            state: BLOB_HELD,
        });
    }
    // 二、改写消息的附件清单。分批做，每个事务都短。
    let mut after = 0i64;
    loop {
        let batch: Vec<(i64, String)> = db::read(ctx.db(), |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT message_id, refs FROM sync_msg WHERE message_id > ?1 \
                 AND refs LIKE '%\"blob\"%' ORDER BY message_id LIMIT 500",
            )?;
            let rows = stmt.query_map(params![after], |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        let Some((last, _)) = batch.last() else {
            break;
        };
        after = *last;
        let changed: Vec<(i64, String)> = batch
            .iter()
            .filter_map(|(id, refs)| rekey_refs(refs, &renamed).map(|new| (*id, new)))
            .collect();
        if !changed.is_empty() {
            db::write(ctx.db(), |tx| {
                for (id, refs) in &changed {
                    tx.execute(
                        "UPDATE sync_msg SET refs = ?2 WHERE message_id = ?1",
                        params![id, refs],
                    )?;
                }
                Ok(())
            })?;
        }
    }
    // 三、换掉登记。
    db::write(ctx.db(), |tx| {
        tx.execute("DELETE FROM sync_blob", [])?;
        for row in &kept {
            db::blob_register(tx, &row.blob_id, &row.rel_path, row.size, BLOB_HELD)?;
        }
        Ok(())
    })
}

/// 清掉本机的同步状态，回到「这些会话、记忆从没同步过」：各设备的读取进度、收件箱与
/// 待发队列、本机的日志状态、文件类数据的寄存器与基线、待处理事项都清空；会话与消息的
/// 身份（`sid`、`gid`）保留，全部回到「只分配了身份」，下次发布扫描把它们当作存量。
///
/// `rekey` 给定 = 换了空间（密钥也换了）：附件对象按新密钥重新登记。不给 = 退出空间：
/// 附件登记原样留着（以后回到同一个空间还用得上），只是都算「没上传」。`space` 给定时
/// 把它记成本机现在所在的空间，与清理在同一个事务里。
pub(crate) fn reset_local_state(
    ctx: &Ctx,
    rekey: Option<&Keys>,
    space: Option<&str>,
) -> Result<()> {
    // 本机的同步状态要整片换掉了：已经开始、还没结算的附件提交阶段随后的核对都会失配
    // （它们的落点与账目属于旧状态，不能再写）。
    ctx.space.bump();
    let exists = db::read(ctx.db(), |conn| Ok(sync_db::sync_tables_exist(conn)?))?;
    if exists {
        // 交给别的设备、还没等到对方落定的定时任务先放回本机的任务表：下面一清，本机就
        // 再也不知道它们了。
        files::cron::release_parked(ctx)?;
        if let Some(keys) = rekey {
            rekey_blobs(ctx, keys)?;
        }
    }
    db::write(ctx.db(), |tx| {
        if exists {
            tx.execute("DELETE FROM sync_inbox", [])?;
            tx.execute("DELETE FROM sync_outbox", [])?;
            // 留下的：线程的开头信息（重新发布时要用，改标成「本机记下的」）、以本机会话
            // 行为键的展示标注与暂停发布的位置；没换空间时还有等着补上的附件落点。
            let alias = if rekey.is_none() {
                wire::SCOPE_BLOB_ALIAS
            } else {
                ""
            };
            tx.execute(
                "DELETE FROM sync_state WHERE scope NOT IN (?1, ?2, ?3, ?4, ?5)",
                params![
                    scope::OPEN,
                    scope::BRANCH,
                    scope::LANDED,
                    map::SCOPE_PUB_HALT,
                    alias
                ],
            )?;
            tx.execute(
                "UPDATE sync_state SET hash = ?2 WHERE scope = ?1",
                params![scope::OPEN, map::OPEN_LOCAL],
            )?;
            tx.execute(
                "UPDATE sync_session SET sealed = ?1, published = NULL",
                params![SEAL_LOCAL],
            )?;
            tx.execute(
                "UPDATE sync_msg SET sealed = ?1 WHERE sealed != ?1",
                params![SEAL_LOCAL],
            )?;
            if rekey.is_none() {
                tx.execute(
                    "UPDATE sync_blob SET state = ?1 WHERE state IN (?2, ?3, ?4)",
                    params![BLOB_HELD, BLOB_PENDING, BLOB_UPLOADED, BLOB_FETCHED],
                )?;
            }
        }
        // 设备身份、逻辑时钟、「仅本机」标记、建过的空间不属于某一个空间，留着。
        tx.execute(
            "DELETE FROM kv WHERE key LIKE 'sync/%' AND key NOT LIKE 'sync/local-only/%' \
             AND key NOT IN (?1, ?2, ?3, ?4, ?5)",
            params![
                KV_SYNC_SCHEMA,
                KV_SYNC_HLC,
                KV_SYNC_DEVICE,
                db::KV_SPACE,
                db::KV_CREATOR
            ],
        )?;
        if let Some(space) = space {
            db::kv_put_json(tx, db::KV_SPACE, &space)?;
        }
        Ok(())
    })?;
    // 旧空间的段文件、等确认的远端改动都作废了。
    remove_files_in(&ctx.paths.outgoing);
    remove_files_in(&ctx.paths.pending);
    remove_files_in(&ctx.paths.tmp);
    Ok(())
}

/// 引擎打开一个空间之前在本机做的准备（阻塞）：建目录；发现换了空间就清掉旧空间留下的
/// 同步状态；核对设备身份、登记这次启动的实例号；清理上次退出时留下的零碎。
pub(crate) fn prepare_local(
    ctx: &Ctx,
    secrets: &mut Secrets,
    keys: &Keys,
    trash_days: u64,
) -> Result<Identity> {
    ctx.paths.ensure()?;
    let space = secrets.space_id.clone();
    let known: Option<String> = db::read(ctx.db(), |conn| db::kv_json(conn, db::KV_SPACE))?;
    if known.as_deref() != Some(space.as_str()) {
        reset_local_state(ctx, Some(keys), Some(&space))?;
    }
    let identity = identity::prepare(ctx, secrets)?;
    remove_files_in(&ctx.paths.tmp);
    seal::sweep_outgoing(ctx)?;
    db::write(ctx.db(), |tx| files::retry_rejected(tx))?;
    trash::sweep(&ctx.paths, trash_days);
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::FileRef;
    use crate::wire::ArtifactRef;

    fn paired() -> Secrets {
        Secrets {
            space_id: new_space_id(),
            key: SpaceKey::generate().to_base64(),
            webdav_password: "pw".into(),
            ..Secrets::default()
        }
    }

    #[test]
    fn pairing_round_trips_storage_settings() {
        let mut config = SyncConfig::default();
        config.webdav.url = "https://dav.example.com/rclaw/".into();
        config.webdav.username = "me".into();
        let mut secrets = paired();
        secrets.set_storage_secret(&config, "pw".into()).unwrap();
        let bare = PairingCode::parse(&pairing_code(&config, &secrets, false).unwrap()).unwrap();
        assert!(bare.storage.is_none());
        let full = PairingCode::parse(&pairing_code(&config, &secrets, true).unwrap()).unwrap();

        // 另一台设备：自己原来填的是别的后端，配对码里的存储设置覆盖它。
        let mut theirs = SyncConfig::default();
        theirs.transport = "file".into();
        let mut their_secrets = Secrets::default();
        let joined = apply_pairing(&theirs, &mut their_secrets, &full);
        assert!(joined.enabled);
        assert_eq!(joined.transport, "webdav");
        assert_eq!(joined.webdav, config.webdav);
        assert_eq!(their_secrets.webdav_password, "pw");
        assert_eq!(their_secrets.storage_secret(&joined), "pw");
        let mut different = joined.clone();
        different.webdav.url = "https://elsewhere.example/rclaw/".into();
        assert!(their_secrets.storage_secret(&different).is_empty());
        assert_eq!(their_secrets.space_id, secrets.space_id);
        assert!(their_secrets.is_paired());

        // 不带存储设置的配对码：沿用本机已填的。
        let mut their_secrets = Secrets::default();
        let joined = apply_pairing(&theirs, &mut their_secrets, &bare);
        assert_eq!(joined.transport, "file");
        assert!(their_secrets.webdav_password.is_empty());

        // `file` 后端的目录是本机路径，不进配对码。
        let mut local = SyncConfig::default();
        local.transport = "file".into();
        local.file.dir = "/tmp/sync".into();
        let code = PairingCode::parse(&pairing_code(&local, &secrets, true).unwrap()).unwrap();
        assert!(code.storage.is_none());
        assert!(pairing_code(&config, &Secrets::default(), false).is_err());
    }

    #[test]
    fn transport_assembly_reports_missing_settings() {
        let secrets = paired();
        let mut config = SyncConfig::default();
        assert!(root_transport(&config, &secrets).is_err());
        config.transport = "file".into();
        assert!(root_transport(&config, &secrets).is_err());
        config.file.dir = "relative/dir".into();
        assert!(root_transport(&config, &secrets).is_err());
        let dir = tempfile::tempdir().unwrap();
        config.file.dir = dir.path().to_string_lossy().into_owned();
        assert_eq!(root_transport(&config, &secrets).unwrap().kind(), "file");
        assert_eq!(
            open_live(&config, &secrets).unwrap().space_id,
            secrets.space_id
        );
        // 公网明文地址在组装时就被拒绝。
        config.transport = "webdav".into();
        config.webdav.url = "http://8.8.8.8/dav/".into();
        config.webdav.allow_http = true;
        assert!(root_transport(&config, &secrets).is_err());
    }

    /// 退出时没能收尾的关闭：本机已经没有旧空间的密钥了，只靠这条记录补完。
    #[tokio::test]
    async fn a_close_left_over_by_a_failed_exit_is_completed_later() {
        let (_tmp, ctx) = crate::testing::ctx();
        let storage = tempfile::tempdir().unwrap();
        let mut config = SyncConfig::default();
        config.transport = "file".into();
        config.file.dir = storage.path().to_string_lossy().into_owned();
        let secrets = paired();
        let keys = secrets.keys().unwrap();
        let root = root_transport(&config, &secrets).unwrap();
        let space = Scoped::new(root, &secrets.space_id);
        // 远端：上一次关闭只写成了墓碑（兼容标记还没写成）。
        let mut file = SpaceFile::new(&keys, &secrets.space_id);
        file.closed = true;
        space
            .put(crate::codec::SPACE_CLOSED_FILE, file.encode().unwrap())
            .await
            .unwrap();
        pending_close_save(
            &ctx.paths,
            &[PendingClose {
                id: crate::ids::random_hex::<8>(),
                rev: 0,
                storage: config.clone(),
                previous: secrets.clone(),
                closed_seen: false,
            }],
        )
        .unwrap();
        finish_pending_close(&ctx.paths, None).await.unwrap();
        // 兼容标记补上了、记录删了；旧空间对新版一律拒绝。
        assert!(matches!(
            remote::open_space(&space, &keys, &secrets.space_id, false).await,
            Err(SyncError::SpaceClosed)
        ));
        assert!(pending_close_load(&ctx.paths).unwrap().is_empty());
    }

    /// 「已确认关闭」是目标的事实：同目标里只要有一条确认过，其余条目也带着它补写。
    #[tokio::test]
    async fn the_confirmed_close_fact_is_shared_across_one_target() {
        let (_tmp, ctx) = crate::testing::ctx();
        let secrets = paired();
        let mut config = SyncConfig::default();
        config.webdav.url = "https://dav.example.com/rclaw/".into();
        config.webdav.username = "me".into();
        let entry = |fingerprint: &str, closed: bool| PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev: 0,
            storage: {
                let mut storage = config.clone();
                storage.webdav.pinned_cert_sha256 = fingerprint.into();
                storage
            },
            previous: secrets.clone(),
            closed_seen: closed,
        };
        // 先写的 A 已确认关闭但密码失效；后写的 B 密码有效却没确认过。
        pending_close_save(&ctx.paths, &[entry("aa", true), entry("bb", false)]).unwrap();
        let closes = pending_close_ensure_ids(&ctx.paths).await.unwrap();
        assert_eq!(closes.len(), 2);
        assert!(closes.iter().all(|close| close.closed_seen));
        // 落盘结果一致：B 也带着事实（否则它会被 NotStarted 当成「没开始」删掉）。
        let again = pending_close_load(&ctx.paths).unwrap();
        assert!(again.iter().all(|close| close.closed_seen));
        // 另一个不同空间/位置不受影响。
        let other = PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev: 0,
            storage: config.clone(),
            previous: paired(),
            closed_seen: false,
        };
        pending_close_save(&ctx.paths, &[other.clone()]).unwrap();
        let closes = pending_close_ensure_ids(&ctx.paths).await.unwrap();
        assert_eq!(closes.len(), 1);
        assert!(!closes[0].closed_seen);
    }

    /// 读入时先按原状态封顶、再传播事实：不能因为传播把同组都标成「已确认」，
    /// 让封顶淘汰掉唯一带有效凭据的那条。
    #[tokio::test]
    async fn the_cap_runs_before_the_target_fact_is_shared() {
        let (_tmp, ctx) = crate::testing::ctx();
        let secrets = paired();
        let mut config = SyncConfig::default();
        config.webdav.url = "https://dav.example.com/rclaw/".into();
        config.webdav.username = "me".into();
        let entry = |fingerprint: &str, closed: bool| PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev: 0,
            storage: {
                let mut storage = config.clone();
                storage.webdav.pinned_cert_sha256 = fingerprint.into();
                storage
            },
            previous: secrets.clone(),
            closed_seen: closed,
        };
        let fingerprint = |close: &PendingClose| {
            close.storage.webdav.pinned_cert_sha256.clone()
        };
        // 超限旧队列：A、B 已确认（B 带唯一有效凭据），C/D/E 未确认。
        // 正确顺序：先淘汰最老的未确认 C，再把事实传播给 D/E；B 必须留着。
        pending_close_save(
            &ctx.paths,
            &[
                entry("aa", true),
                entry("bb", true),
                entry("cc", false),
                entry("dd", false),
                entry("ee", false),
            ],
        )
        .unwrap();
        let closes = pending_close_ensure_ids(&ctx.paths).await.unwrap();
        assert_eq!(closes.len(), 4);
        assert!(closes.iter().any(|close| fingerprint(close) == "bb"));
        assert!(!closes.iter().any(|close| fingerprint(close) == "cc"));
        assert!(closes
            .iter()
            .filter(|close| ["dd", "ee"].contains(&fingerprint(close).as_str()))
            .all(|close| close.closed_seen));
    }

    /// 版本不复用：清空队列/删掉最大版本后，新记录仍拿到更大的版本，不会被在途尝试的
    /// 旧上界误删。
    #[tokio::test]
    async fn revision_numbers_are_not_reused_after_the_queue_is_cleared() {
        let (_tmp, ctx) = crate::testing::ctx();
        let secrets = paired();
        let mut config = SyncConfig::default();
        config.transport = "file".into();
        config.file.dir = "C:/sync-a".into();
        let entry = |rev: u64| PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev,
            storage: config.clone(),
            previous: secrets.clone(),
            closed_seen: true,
        };
        let seeded = [entry(1), entry(2)];
        pending_close_save(&ctx.paths, &seeded).unwrap();
        // 在途尝试的上界：2。
        let in_flight_limit = pending_close_rev_limit(&ctx.paths).await.unwrap();
        assert_eq!(in_flight_limit, 2);
        // 队列被清空（成功清理删掉整组 / 清空文件）。
        pending_close_finish_target(&ctx.paths, &config, &secrets.space_id, false, None, 2)
            .await
            .unwrap();
        assert!(pending_close_load(&ctx.paths).unwrap().is_empty());
        // 清空之后恢复一条：版本必须大于 2，否则在途尝试的旧上界会把它删掉。
        pending_close_restore(&ctx.paths, &entry(1)).await.unwrap();
        let restored = pending_close_load(&ctx.paths).unwrap();
        assert_eq!(restored.len(), 1);
        assert!(restored[0].rev > in_flight_limit);
        pending_close_finish_target(
            &ctx.paths,
            &config,
            &secrets.space_id,
            false,
            None,
            in_flight_limit,
        )
        .await
        .unwrap();
        assert_eq!(pending_close_load(&ctx.paths).unwrap().len(), 1);
    }

    /// 取样时先把老记录的版本补齐：不会出现「取样之后老记录才拿到更高版本」而逃过清理。
    #[tokio::test]
    async fn legacy_records_get_their_revision_before_the_limit_is_taken() {
        let (_tmp, ctx) = crate::testing::ctx();
        let secrets = paired();
        let mut config = SyncConfig::default();
        config.transport = "file".into();
        config.file.dir = "C:/sync-a".into();
        let legacy = PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev: 0,
            storage: config.clone(),
            previous: secrets.clone(),
            closed_seen: true,
        };
        pending_close_save(&ctx.paths, &[legacy.clone()]).unwrap();
        let limit = pending_close_rev_limit(&ctx.paths).await.unwrap();
        assert!(limit > 0);
        // 老记录已经被补上版本并落盘，清理按这个上界能删掉它（它在尝试之前就存在）。
        let closes = pending_close_load(&ctx.paths).unwrap();
        assert!(closes[0].rev > 0, "取样时应把老记录的版本补上");
        assert!(closes[0].rev <= limit);
        pending_close_finish_target(&ctx.paths, &config, &secrets.space_id, false, None, limit)
            .await
            .unwrap();
        assert!(pending_close_load(&ctx.paths).unwrap().is_empty());
    }

    /// 序号文件读不出来（内容坏了 / 连读都读不出来）时按错误处理，不当成首次使用的 0：
    /// 已分配上界丢失后继续分配，可能复用把在途尝试救回来的版本。
    #[tokio::test]
    async fn an_unreadable_revision_sequence_fails_instead_of_restarting_from_zero() {
        let (_tmp, ctx) = crate::testing::ctx();
        let secrets = paired();
        let mut config = SyncConfig::default();
        config.transport = "file".into();
        config.file.dir = "C:/sync-a".into();
        let entry = PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev: 7,
            storage: config.clone(),
            previous: secrets.clone(),
            closed_seen: true,
        };
        pending_close_save(&ctx.paths, &[entry]).unwrap();
        std::fs::write(rev_seq_path(&ctx.paths), b"not-a-rev").unwrap();
        assert!(
            pending_close_rev_limit(&ctx.paths).await.is_err(),
            "序号内容解析不了时要报错，不能从 0 重新分配"
        );
        // 坏值原样留着：一次成功的写盘不能把它悄悄换成更小的上界。
        assert_eq!(
            std::fs::read_to_string(rev_seq_path(&ctx.paths)).unwrap(),
            "not-a-rev"
        );
        // 连读都读不出来（目录占位）也是错误。
        std::fs::remove_file(rev_seq_path(&ctx.paths)).unwrap();
        std::fs::create_dir(rev_seq_path(&ctx.paths)).unwrap();
        assert!(pending_close_rev_limit(&ctx.paths).await.is_err());
    }

    /// 备份替换的 crash 窗口：主序号文件缺失、旧值还在备份里时按备份算上界，不当成首次。
    #[tokio::test]
    async fn a_missing_sequence_file_falls_back_to_its_backup() {
        let (_tmp, ctx) = crate::testing::ctx();
        let secrets = paired();
        let mut config = SyncConfig::default();
        config.transport = "file".into();
        config.file.dir = "C:/sync-a".into();
        let entry = |rev: u64| PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev,
            storage: config.clone(),
            previous: secrets.clone(),
            closed_seen: true,
        };
        std::fs::write(rev_seq_backup_path(&ctx.paths), b"5").unwrap();
        pending_close_save(&ctx.paths, &[entry(1)]).unwrap();
        assert_eq!(pending_close_rev_limit(&ctx.paths).await.unwrap(), 5);
        // 分配从备份里的上界之后开始。
        let restored = entry(1);
        pending_close_restore(&ctx.paths, &restored).await.unwrap();
        let closes = pending_close_load(&ctx.paths).unwrap();
        let rev = closes
            .iter()
            .find(|close| close.id == restored.id)
            .map(|close| close.rev)
            .expect("恢复的记录还在盘上");
        assert!(rev > 5, "备份里的上界之后才该分配：{rev}");
    }

    /// 队列没变化时取样不写序号文件：那份「读改写」只是读，不该给整批多出一个写失败点。
    #[tokio::test]
    async fn a_read_only_sampling_leaves_the_sequence_file_untouched() {
        let (_tmp, ctx) = crate::testing::ctx();
        let secrets = paired();
        let mut config = SyncConfig::default();
        config.transport = "file".into();
        config.file.dir = "C:/sync-a".into();
        std::fs::write(rev_seq_path(&ctx.paths), b"5").unwrap();
        // 残留备份：真去写序号文件时，原子写会把主文件换掉并顺带删掉它。
        std::fs::write(rev_seq_backup_path(&ctx.paths), b"9").unwrap();
        let entry = PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev: 1,
            storage: config.clone(),
            previous: secrets.clone(),
            closed_seen: true,
        };
        pending_close_save(&ctx.paths, &[entry]).unwrap();
        assert_eq!(pending_close_rev_limit(&ctx.paths).await.unwrap(), 5);
        assert!(
            rev_seq_backup_path(&ctx.paths).exists(),
            "取样不该写序号文件"
        );
        assert_eq!(
            std::fs::read_to_string(rev_seq_path(&ctx.paths)).unwrap(),
            "5"
        );
    }

    /// 成功清理只删版本不高于尝试开始时上界的记录：并发失败刚补回来的（版本更高）要留着。
    #[tokio::test]
    async fn a_target_cleanup_keeps_records_restored_after_its_done() {
        let (_tmp, ctx) = crate::testing::ctx();
        let secrets = paired();
        let mut config = SyncConfig::default();
        config.transport = "file".into();
        config.file.dir = "C:/sync-a".into();
        let entry = |rev: u64| PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev,
            storage: config.clone(),
            previous: secrets.clone(),
            closed_seen: true,
        };
        // 尝试开始时队列上界是 10：版本 5 的事前记录清掉，版本 20 的并发恢复记录留着。
        let older = entry(5);
        let newer = entry(20);
        pending_close_save(&ctx.paths, &[older, newer.clone()]).unwrap();
        pending_close_finish_target(&ctx.paths, &config, &secrets.space_id, false, None, 10)
            .await
            .unwrap();
        let closes = pending_close_load(&ctx.paths).unwrap();
        assert_eq!(closes.len(), 1);
        assert_eq!(closes[0].id, newer.id);
    }

    /// `NotStarted` 时盘上同目标已有确认事实：本批「没开始」的结论不成立，一条都别删。
    #[tokio::test]
    async fn not_started_keeps_the_target_when_a_confirmed_record_exists() {
        let (_tmp, ctx) = crate::testing::ctx();
        let secrets = paired();
        let mut config = SyncConfig::default();
        config.transport = "file".into();
        config.file.dir = "C:/sync-a".into();
        let confirmed = PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev: 0,
            storage: config.clone(),
            previous: secrets.clone(),
            closed_seen: true,
        };
        let unconfirmed = PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev: 0,
            storage: config.clone(),
            previous: secrets.clone(),
            closed_seen: false,
        };
        pending_close_save(&ctx.paths, &[confirmed.clone(), unconfirmed.clone()]).unwrap();
        let batch: std::collections::HashSet<String> =
            [unconfirmed.id.clone()].into_iter().collect();
        pending_close_finish_target(&ctx.paths, &config, &secrets.space_id, true, Some(&batch), 0)
            .await
            .unwrap();
        let closes = pending_close_load(&ctx.paths).unwrap();
        assert_eq!(closes.len(), 2);
        assert!(closes.iter().any(|close| close.id == unconfirmed.id));
    }

    /// 失败时把记录重新落盘：并发的成功清理删掉了它，也要把重试依据补回来。
    #[tokio::test]
    async fn a_failed_close_restores_its_record_after_a_concurrent_cleanup() {
        let (_tmp, ctx) = crate::testing::ctx();
        let secrets = paired();
        let mut config = SyncConfig::default();
        config.transport = "file".into();
        config.file.dir = "C:/sync-a".into();
        let entry = PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev: 0,
            storage: config.clone(),
            previous: secrets.clone(),
            closed_seen: true,
        };
        pending_close_save(&ctx.paths, &[entry.clone()]).unwrap();
        // 并发的成功清理把同目标全删了。
        pending_close_finish_target(
            &ctx.paths,
            &config,
            &secrets.space_id,
            false,
            None,
            u64::MAX,
        )
            .await
            .unwrap();
        assert!(pending_close_load(&ctx.paths).unwrap().is_empty());
        // 这次失败把它按身份补回去，事实保留。
        pending_close_restore(&ctx.paths, &entry).await.unwrap();
        let closes = pending_close_load(&ctx.paths).unwrap();
        assert_eq!(closes.len(), 1);
        assert_eq!(closes[0].id, entry.id);
        assert!(closes[0].closed_seen);
        // 已经存在时只更新事实，不造重复条目。
        let mut updated = entry.clone();
        updated.closed_seen = false;
        pending_close_restore(&ctx.paths, &updated).await.unwrap();
        let closes = pending_close_load(&ctx.paths).unwrap();
        assert_eq!(closes.len(), 1);
        assert!(closes[0].closed_seen);
    }

    /// 同目标封顶：最老那条一定留着，优先淘汰最老的未确认条目，都确认过淘汰第二老。
    #[tokio::test]
    async fn the_per_target_cap_keeps_the_oldest_and_prefers_unconfirmed() {
        let (_tmp, ctx) = crate::testing::ctx();
        let secrets = paired();
        let mut config = SyncConfig::default();
        config.webdav.url = "https://dav.example.com/rclaw/".into();
        config.webdav.username = "me".into();
        let entry = |fingerprint: &str, closed: bool| PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev: 0,
            storage: {
                let mut storage = config.clone();
                storage.webdav.pinned_cert_sha256 = fingerprint.into();
                storage
            },
            previous: secrets.clone(),
            closed_seen: closed,
        };
        let fingerprint = |close: &PendingClose| {
            close.storage.webdav.pinned_cert_sha256.clone()
        };

        // 场景一（读入封顶，全未确认）：最老那条一定留着，淘汰第二老的未确认项。
        let seeded = vec![
            entry("aa", false),
            entry("bb1", false),
            entry("bb2", false),
            entry("bb3", false),
            entry("bb4", false),
        ];
        let oldest = seeded[0].id.clone();
        pending_close_save(&ctx.paths, &seeded).unwrap();
        let closes = pending_close_ensure_ids(&ctx.paths).await.unwrap();
        assert_eq!(closes.len(), 4);
        assert!(closes.iter().any(|close| close.id == oldest));
        assert!(!closes.iter().any(|close| fingerprint(close) == "bb1"));
        assert!(closes.iter().any(|close| fingerprint(close) == "bb2"));
        assert!(closes.iter().any(|close| fingerprint(close) == "bb3"));
        assert!(closes.iter().any(|close| fingerprint(close) == "bb4"));
        // 封顶结果也要落盘（不只是改内存）：回读一遍。
        let again = pending_close_load(&ctx.paths).unwrap();
        assert_eq!(again.len(), 4);
        assert!(again.iter().any(|close| close.id == oldest));

        // 场景二（6 条 → 4 条）：两个 victim 都断言——先 dd1、再 dd2。
        let seeded = vec![
            entry("cc0", true),
            entry("dd1", false),
            entry("dd2", false),
            entry("dd3", false),
            entry("dd4", false),
            entry("dd5", false),
        ];
        pending_close_save(&ctx.paths, &seeded).unwrap();
        let closes = pending_close_ensure_ids(&ctx.paths).await.unwrap();
        assert_eq!(closes.len(), 4);
        assert!(!closes.iter().any(|close| fingerprint(close) == "dd1"));
        assert!(!closes.iter().any(|close| fingerprint(close) == "dd2"));
        assert!(closes.iter().any(|close| fingerprint(close) == "dd3"));

        // 场景三（全都已确认）：最老留着，淘汰第二老的。
        let seeded: Vec<PendingClose> = (0..5)
            .map(|index| entry(&format!("ee{index}"), true))
            .collect();
        let oldest = seeded[0].id.clone();
        pending_close_save(&ctx.paths, &seeded).unwrap();
        let closes = pending_close_ensure_ids(&ctx.paths).await.unwrap();
        assert_eq!(closes.len(), 4);
        assert!(closes.iter().any(|close| close.id == oldest));
        assert!(!closes.iter().any(|close| fingerprint(close) == "ee1"));

        // 场景四（入队路径，混合组）：直接落盘 [确认, 确认, 未确认]（跳过读入时的
        // 目标级事实传播），再 add 两条；超限时优先淘汰最老的**未确认**那条（ff2），
        // 而不是第二老的（ff1）——全确认回退才会选 ff1。
        let seeded = vec![
            entry("ff0", true),
            entry("ff1", true),
            entry("ff2", false),
        ];
        let oldest = seeded[0].id.clone();
        pending_close_save(&ctx.paths, &seeded).unwrap();
        for index in 3..=4 {
            pending_close_add(&ctx.paths, entry(&format!("ff{index}"), false))
                .await
                .unwrap();
        }
        let closes = pending_close_load(&ctx.paths).unwrap();
        assert_eq!(closes.len(), 4);
        assert!(closes.iter().any(|close| close.id == oldest));
        assert!(closes.iter().any(|close| fingerprint(close) == "ff1"));
        assert!(!closes.iter().any(|close| fingerprint(close) == "ff2"));
        assert!(closes.iter().any(|close| fingerprint(close) == "ff4"));
    }

    /// 同一位置的历史条目各留各的快照（旧密码未必错），事实随目标走，成功时一起清。
    #[tokio::test]
    async fn duplicate_records_at_one_location_keep_their_snapshots_and_facts() {
        let (_tmp, ctx) = crate::testing::ctx();
        let secrets = paired();
        let mut config = SyncConfig::default();
        config.webdav.url = "https://dav.example.com/rclaw/".into();
        config.webdav.username = "me".into();
        let mut old = config.clone();
        old.webdav.pinned_cert_sha256 = "aa".repeat(32);
        let entry = |storage: &SyncConfig, closed: bool| PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev: 0,
            storage: storage.clone(),
            previous: secrets.clone(),
            closed_seen: closed,
        };
        // 升级前留下的两条：旧指纹那条不知道关闭已确认，新指纹那条知道。
        pending_close_save(&ctx.paths, &[entry(&old, false), entry(&config, true)]).unwrap();
        let closes = pending_close_ensure_ids(&ctx.paths).await.unwrap();
        // 读入不合并：两条快照都留着（旧的未必不能连）。
        assert_eq!(closes.len(), 2);
        // 新入队继承目标上已确认的事实，同时保留自己的快照。
        let newest = entry(&old, false);
        pending_close_add(&ctx.paths, newest.clone()).await.unwrap();
        let closes = pending_close_load(&ctx.paths).unwrap();
        assert_eq!(closes.len(), 3);
        let added = closes.iter().find(|close| close.id == newest.id).unwrap();
        assert!(added.closed_seen);
        assert_eq!(
            added.storage.webdav.pinned_cert_sha256,
            old.webdav.pinned_cert_sha256
        );
        // 收尾成功：同目标的全部历史一起清掉。
        pending_close_finish_target(
            &ctx.paths,
            &config,
            &secrets.space_id,
            false,
            None,
            u64::MAX,
        )
            .await
            .unwrap();
        assert!(pending_close_load(&ctx.paths).unwrap().is_empty());
    }

    #[test]
    fn close_records_at_one_location_merge_even_if_connection_settings_differ() {
        let secrets = paired();
        let mut first = SyncConfig::default();
        first.webdav.url = "https://dav.example.com/rclaw/".into();
        first.webdav.username = "me".into();
        let mut second = first.clone();
        second.webdav.pinned_cert_sha256 = "aa".repeat(32);
        let entry = |storage: &SyncConfig| PendingClose {
            id: String::new(),
            rev: 0,
            storage: storage.clone(),
            previous: secrets.clone(),
            closed_seen: false,
        };
        // 同一个位置、只换了连接设置：算同一个目标。
        assert!(same_close_target(
            &entry(&first),
            &second,
            &secrets.space_id
        ));
        // 用户名只差一个尾随空格：实际位置不同，不合并。
        let mut elsewhere = second.clone();
        elsewhere.webdav.username = "me ".into();
        assert!(!same_close_target(
            &entry(&first),
            &elsewhere,
            &secrets.space_id
        ));
    }

    /// 老版本写下的条目没有身份：读进来要先补上各自的，结算才不会串到别的空间上。
    #[tokio::test]
    async fn legacy_entries_without_identity_get_distinct_ones() {
        let (_tmp, ctx) = crate::testing::ctx();
        let mut config = SyncConfig::default();
        config.transport = "file".into();
        config.file.dir = "C:/sync-a".into();
        let first = paired();
        let second = paired();
        let entry = |space: &Secrets| PendingClose {
            id: String::new(),
            rev: 0,
            storage: config.clone(),
            previous: space.clone(),
            closed_seen: false,
        };
        pending_close_save(&ctx.paths, &[entry(&first), entry(&second)]).unwrap();
        let closes = pending_close_ensure_ids(&ctx.paths).await.unwrap();
        assert_eq!(closes.len(), 2);
        assert!(!closes[0].id.is_empty());
        assert_ne!(closes[0].id, closes[1].id);
        // 补出来的身份要落盘：下一次读到的还是这两个。
        let again = pending_close_load(&ctx.paths).unwrap();
        assert_eq!(again[0].id, closes[0].id);
        assert_eq!(again[1].id, closes[1].id);
        // 结算一条不会删掉另一条。
        pending_close_finish(&ctx.paths, &closes[0].id).await.unwrap();
        let left = pending_close_load(&ctx.paths).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, closes[1].id);
        // 置位也只落到剩下那条上。
        pending_close_mark_seen(
            &ctx.paths,
            &closes[1].id,
            &closes[1].storage,
            &closes[1].previous.space_id,
        )
        .await
        .unwrap();
        assert!(pending_close_load(&ctx.paths).unwrap()[0].closed_seen);
    }

    /// 结算按记录身份；确认事实在身份消失时落到同目标；收尾成功清掉同目标全部。
    #[tokio::test]
    async fn close_results_are_settled_by_record_id() {
        let (_tmp, ctx) = crate::testing::ctx();
        let secrets = paired();
        let other_space = paired();
        let config_at = |dir: &str| {
            let mut config = SyncConfig::default();
            config.transport = "file".into();
            config.file.dir = dir.into();
            config
        };
        let config = config_at("C:/sync-a");
        let old = PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev: 0,
            storage: config.clone(),
            previous: secrets.clone(),
            closed_seen: false,
        };
        pending_close_save(&ctx.paths, &[old.clone()]).unwrap();
        let fresh = PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev: 0,
            storage: config.clone(),
            previous: secrets.clone(),
            closed_seen: false,
        };
        pending_close_add(&ctx.paths, fresh.clone()).await.unwrap();
        // 同一目标的历史保留（各条快照都要留着试），事实在 add 时并进新条目。
        assert_eq!(pending_close_load(&ctx.paths).unwrap().len(), 2);
        // 另一条不同空间的记录。
        let other = PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev: 0,
            storage: config.clone(),
            previous: other_space.clone(),
            closed_seen: false,
        };
        pending_close_add(&ctx.paths, other.clone()).await.unwrap();
        // 旧身份的「完成」只删它自己。
        pending_close_finish(&ctx.paths, &old.id).await.unwrap();
        let closes = pending_close_load(&ctx.paths).unwrap();
        assert_eq!(closes.len(), 2);
        assert!(closes.iter().any(|close| close.id == fresh.id));
        // 「确认关闭」按身份找不到时落到同目标上，不碰别的空间。
        pending_close_mark_seen(&ctx.paths, &old.id, &config, &secrets.space_id)
            .await
            .unwrap();
        let closes = pending_close_load(&ctx.paths).unwrap();
        let fresh_now = closes.iter().find(|close| close.id == fresh.id).unwrap();
        assert!(fresh_now.closed_seen);
        let other_now = closes
            .iter()
            .find(|close| close.previous.space_id == other_space.space_id)
            .unwrap();
        assert!(!other_now.closed_seen);
        // 「本批判定没开始关闭」只清本批里的未确认条目：批次之后新入队的别动。
        // 直接落盘而不是 add，否则 add 会把 fresh 的确认事实并进来、这里就测不到批次判据。
        let later = PendingClose {
            id: crate::ids::random_hex::<8>(),
            rev: 0,
            storage: config.clone(),
            previous: secrets.clone(),
            closed_seen: false,
        };
        let mut queued = pending_close_load(&ctx.paths).unwrap();
        queued.push(later.clone());
        pending_close_save(&ctx.paths, &queued).unwrap();
        let batch: std::collections::HashSet<String> =
            [fresh.id.clone()].into_iter().collect();
        pending_close_finish_target(&ctx.paths, &config, &secrets.space_id, true, Some(&batch), 0)
            .await
            .unwrap();
        let closes = pending_close_load(&ctx.paths).unwrap();
        assert!(closes.iter().any(|close| close.id == later.id));
        // 带着确认事实的也不清（它是别的条目观察到的目标事实）。
        let closes = pending_close_load(&ctx.paths).unwrap();
        assert!(closes.iter().any(|close| close.id == fresh.id && close.closed_seen));
        // 收尾成功清掉同目标的全部历史，别的目标不动。
        pending_close_finish_target(
            &ctx.paths,
            &config,
            &secrets.space_id,
            false,
            None,
            u64::MAX,
        )
            .await
            .unwrap();
        let closes = pending_close_load(&ctx.paths).unwrap();
        assert_eq!(closes.len(), 1);
        assert_eq!(closes[0].previous.space_id, other_space.space_id);
    }

    #[tokio::test]
    async fn close_records_keep_each_storage_and_remember_a_confirmed_close() {
        let (_tmp, ctx) = crate::testing::ctx();
        let secrets = paired();
        let config_at = |dir: &str| {
            let mut config = SyncConfig::default();
            config.transport = "file".into();
            config.file.dir = dir.into();
            config
        };
        let first = config_at("C:/sync-a");
        let second = config_at("C:/sync-b");
        pending_close_add(
            &ctx.paths,
            PendingClose {
                id: crate::ids::random_hex::<8>(),
                rev: 0,
                storage: first.clone(),
                previous: secrets.clone(),
                closed_seen: true,
            },
        )
        .await
        .unwrap();
        // 同一个空间复制到另一个存储位置：两处各留一条。
        pending_close_add(
            &ctx.paths,
            PendingClose {
                id: crate::ids::random_hex::<8>(),
                rev: 0,
                storage: second.clone(),
                previous: secrets.clone(),
                closed_seen: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(pending_close_load(&ctx.paths).unwrap().len(), 2);
        // 同一目标再来一条（这次不知道关闭已确认）：旧条目留着（各带各的快照），
        // 确认过的事实并进新这条。
        let id = crate::ids::random_hex::<8>();
        pending_close_add(
            &ctx.paths,
            PendingClose {
                id: id.clone(),
                rev: 0,
                storage: first.clone(),
                previous: secrets.clone(),
                closed_seen: false,
            },
        )
        .await
        .unwrap();
        let closes = pending_close_load(&ctx.paths).unwrap();
        assert_eq!(closes.len(), 3);
        let added = closes.iter().find(|close| close.id == id).unwrap();
        assert!(added.closed_seen);
    }

    /// 重置可能压根没走到关闭那一步：退出后不能替用户关掉一个还在用的旧空间。
    #[tokio::test]
    async fn a_close_that_never_started_is_not_invented_by_the_leftover_record() {
        let (_tmp, ctx) = crate::testing::ctx();
        let storage = tempfile::tempdir().unwrap();
        let mut config = SyncConfig::default();
        config.transport = "file".into();
        config.file.dir = storage.path().to_string_lossy().into_owned();
        let secrets = paired();
        let keys = secrets.keys().unwrap();
        let root = root_transport(&config, &secrets).unwrap();
        let space = Scoped::new(root, &secrets.space_id);
        let file = SpaceFile::new(&keys, &secrets.space_id);
        space
            .put(crate::codec::SPACE_FILE, file.encode().unwrap())
            .await
            .unwrap();
        pending_close_save(
            &ctx.paths,
            &[PendingClose {
                id: crate::ids::random_hex::<8>(),
                rev: 0,
                storage: config.clone(),
                previous: secrets.clone(),
                closed_seen: false,
            }],
        )
        .unwrap();
        finish_pending_close(&ctx.paths, None).await.unwrap();
        remote::open_space(&space, &keys, &secrets.space_id, false)
            .await
            .unwrap();
        assert!(pending_close_load(&ctx.paths).unwrap().is_empty());
    }

    #[test]
    fn refs_follow_the_new_blob_names() {
        let refs = StoredRefs {
            files: vec![
                FileRef {
                    path: "uploads/a.png".into(),
                    blob: "old-a".into(),
                    size: 3,
                },
                FileRef {
                    path: "media/b.png".into(),
                    blob: "gone".into(),
                    size: 4,
                },
            ],
            artifacts: vec![
                ArtifactRef {
                    name: "t1.txt".into(),
                    size: 9,
                    blob: Some("old-t".into()),
                },
                ArtifactRef {
                    name: "t2.txt".into(),
                    size: 9,
                    blob: Some("gone".into()),
                },
            ],
        };
        let mut renamed = HashMap::new();
        renamed.insert("old-a".to_string(), "new-a".to_string());
        renamed.insert("new-a".to_string(), "new-a".to_string());
        renamed.insert("old-t".to_string(), "new-t".to_string());
        let column = rekey_refs(&refs.to_column(), &renamed).unwrap();
        let after = StoredRefs::parse(Some(&column)).unwrap();
        assert_eq!(after.files.len(), 1);
        assert_eq!(after.files[0].blob, "new-a");
        assert_eq!(after.artifacts[0].blob.as_deref(), Some("new-t"));
        assert_eq!(after.artifacts[1].blob, None);
        // 再来一遍：新名字映射到它自己，结果不变。
        renamed.insert("new-t".to_string(), "new-t".to_string());
        assert!(rekey_refs(&column, &renamed).is_none());
    }

    #[test]
    fn reset_forgets_sync_state_but_keeps_identity_and_marks() {
        let device = crate::testing::Device::new();
        let ctx = &device.ctx;
        device
            .sessions()
            .append(
                "desktop",
                "c1",
                &rclaw_core::Message::plain(rclaw_core::Role::User, "你好"),
            )
            .unwrap();
        crate::testing::seal_all(&device);
        let before = db::read(ctx.db(), |conn| {
            map::session_map_by_key(conn, "desktop", "c1")
        })
        .unwrap()
        .unwrap();
        db::write(ctx.db(), |tx| {
            sync_db::kv_put(tx, &db::local_only_key("desktop", "private"), "true")?;
            db::kv_put_json(tx, db::KV_PRIMED, &true)?;
            db::kv_put_json(tx, db::KV_CREATOR, &"aaaa")?;
            db::state_put(tx, scope::TOMB, "x/", db::HLC_ZERO, None, None)?;
            db::state_put(tx, scope::BRANCH, "7", db::HLC_ZERO, None, Some("dev"))?;
            db::blob_register(tx, "b1", "uploads/gone.bin", 1, BLOB_UPLOADED)?;
            db::blob_register(tx, "b2", "uploads/far.bin", 1, BLOB_REMOTE)?;
            Ok(())
        })
        .unwrap();
        std::fs::write(ctx.paths.pending.join("x.json"), b"{}").unwrap();

        reset_local_state(ctx, None, Some("0123456789abcdef0123")).unwrap();
        db::read(ctx.db(), |conn| {
            let after = map::session_map_by_key(conn, "desktop", "c1")?.unwrap();
            assert_eq!(after.sid, before.sid);
            assert_eq!(after.tip, before.tip);
            assert_eq!(after.sealed, SEAL_LOCAL);
            assert!(after.published.is_none());
            assert_eq!(map::outbox_len(conn)?, 0);
            assert_eq!(db::log_state(conn)?, db::LogState::default());
            assert!(db::state_get(conn, scope::TOMB, "x/")?.is_none());
            assert!(db::state_get(conn, scope::BRANCH, "7")?.is_some());
            assert!(db::kv_json::<bool>(conn, db::KV_PRIMED)?.is_none());
            assert_eq!(
                db::kv_json::<String>(conn, db::KV_CREATOR)?.as_deref(),
                Some("aaaa")
            );
            assert_eq!(
                db::kv_json::<String>(conn, db::KV_SPACE)?.as_deref(),
                Some("0123456789abcdef0123")
            );
            assert!(sync_db::kv_get(conn, &db::local_only_key("desktop", "private"))?.is_some());
            assert_eq!(
                sync_db::device_id(conn)?.as_deref(),
                Some(device.settings.device.as_str())
            );
            // 没换空间：附件登记留着，都算没上传；还在别的设备上的原样。
            assert_eq!(db::blob_get(conn, "b1")?.unwrap().state, BLOB_HELD);
            assert_eq!(db::blob_get(conn, "b2")?.unwrap().state, BLOB_REMOTE);
            Ok(())
        })
        .unwrap();
        assert!(!ctx.paths.pending.join("x.json").exists());
        assert!(std::fs::read_dir(&ctx.paths.outgoing)
            .unwrap()
            .next()
            .is_none());

        // 换了空间：本机没有文件的附件登记作废。
        reset_local_state(ctx, Some(&device.keys), None).unwrap();
        db::read(ctx.db(), |conn| {
            assert!(db::blob_get(conn, "b1")?.is_none());
            assert!(db::blob_get(conn, "b2")?.is_none());
            Ok(())
        })
        .unwrap();
        // 下一次发布扫描把这个会话当作存量重新排队。
        let queued = crate::publish::scan_sessions(ctx, &device.settings, true).unwrap();
        assert_eq!(queued.queued, 2);
    }
}
