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

/// 同一空间、同一存储位置算同一条收尾：配对码只带空间 ID 与密钥，同一个空间可以
/// 被复制到别的存储位置上，那两处要各留一条。
fn same_close_target(close: &PendingClose, storage: &SyncConfig, space_id: &str) -> bool {
    close.previous.space_id == space_id
        && storage_binding(&close.storage) == storage_binding(storage)
}

/// 记下一条「补完关闭」。同一目标只留一条：后来写的凭据更新，但已经确认过的
/// 「标记已是关闭版」这个事实不会因为后来的记录没有它而丢掉。
fn pending_close_add(paths: &SyncPaths, close: PendingClose) -> Result<()> {
    let mut closes = pending_close_load(paths)?;
    let mut close = close;
    if let Some(old) = closes
        .iter()
        .find(|old| same_close_target(old, &close.storage, &close.previous.space_id))
    {
        close.closed_seen |= old.closed_seen;
    }
    closes.retain(|old| !same_close_target(old, &close.storage, &close.previous.space_id));
    closes.push(close);
    pending_close_save(paths, &closes)
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

/// 把收尾结果并回最新的队列：成功的目标删掉；失败的目标只把这次确认过的事实
/// （`closed_seen`）并回去，别把开头的快照整批写回——期间别的进程可能追加了新条目。
fn merge_close_outcomes(
    latest: &mut Vec<PendingClose>,
    outcomes: &[(PendingClose, bool)],
) {
    for (close, done) in outcomes {
        let target = |entry: &PendingClose| {
            same_close_target(entry, &close.storage, &close.previous.space_id)
        };
        if *done {
            latest.retain(|entry| !target(entry));
        } else if let Some(entry) = latest.iter_mut().find(|entry| target(entry)) {
            entry.closed_seen |= close.closed_seen;
        }
        // 最新队列里已经没有这一条：另一进程的收尾做完了，不复活。
    }
}

/// 把退出前没能收尾的关闭（[`PendingClose`]）补完：只在远端确实显示关闭已经开始时
/// 动手（[`remote::ensure_closed_if_started`]），做完删记录，失败留着下次再试。
///
/// 退出时本机已经清掉了旧空间的密钥，之后只有这些记录能关掉那些空间：不补完的话，
/// 迟到的创建把兼容标记覆盖回开放后，旧空间又能被当成可用。
pub(crate) async fn finish_pending_close(
    paths: &SyncPaths,
    config: &SyncConfig,
    secrets: &Secrets,
) -> Result<()> {
    let mut outcomes = Vec::new();
    let mut first_error = None;
    for mut close in pending_close_load(paths)? {
        let done = match finish_one_close(&mut close, config, secrets).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    "补完空间 {} 的关闭失败（稍后再试）: {error}",
                    close.previous.space_id
                );
                first_error.get_or_insert(error);
                false
            }
        };
        outcomes.push((close, done));
    }
    let mut latest = pending_close_load(paths)?;
    merge_close_outcomes(&mut latest, &outcomes);
    pending_close_save(paths, &latest)?;
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn finish_one_close(
    close: &mut PendingClose,
    config: &SyncConfig,
    secrets: &Secrets,
) -> Result<()> {
    if !close.previous.is_paired() {
        return Ok(());
    }
    let mut previous = close.previous.clone();
    let mut at = close.storage.clone();
    if storage_binding(&at) == storage_binding(config) {
        // 同一个存储位置：当前设置里还有这个账户的凭据时凭据换成当前这份（退出之后
        // 可能按「设备丢失」流程改过密码）——本机已经退出、没有凭据时，记录里的那份
        // 才是能用的。连接设置同样以当前为准。
        refresh_connection_settings(&mut at, config);
        if !secrets.storage_secret(config).is_empty() {
            refresh_credentials(&mut previous, secrets);
        }
    }
    let space_id = previous.space_id.clone();
    let keys = match previous.keys() {
        Ok(keys) => Arc::new(keys),
        // 记录里的密钥读不出来：这条永远补不了，丢掉别每 30 秒重试。
        Err(error) => {
            tracing::warn!("收尾记录里的空间密钥无法读取，丢弃这条: {error}");
            return Ok(());
        }
    };
    let root = blocking(move || root_transport(&at, &previous)).await?;
    let transport = Scoped::new(root, &space_id);
    // 先看一眼标记：验证过是关闭版就把这个事实记进记录（本次补写再失败，下一次读到
    // 被迟到创建改回开放的标记时，仍能按关闭补完）。
    if !close.closed_seen {
        close.closed_seen =
            remote::marker_closed_state(&transport, &keys, &space_id).await? == Some(true);
    }
    remote::ensure_closed_if_started(&transport, &keys, &space_id, close.closed_seen).await?;
    Ok(())
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
        refresh_credentials(&mut old_secrets, secrets);
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
            // 提交重置之前把墓碑补上，否则迟到创建覆盖标记后旧空间又能被接受。
            let old = load_live(&at, &old_secrets).await?;
            if let Err(error) =
                remote::ensure_closed(old.transport(), &old.keys, &old.space_id).await
            {
                // 这次已经确认旧标记是关闭版、墓碑却没补上：把这个事实落盘。本机随后
                // 可能退出并清掉旧空间密钥，只有它能保证迟到创建把标记改回开放之后，
                // 收尾仍按「关闭已经开始」补完。
                let close = PendingClose {
                    storage: at.clone(),
                    previous: old_secrets.clone(),
                    closed_seen: true,
                };
                if let Err(write_error) = pending_close_add(&ctx.paths, close) {
                    tracing::warn!("记下没补完的空间关闭失败: {write_error}");
                }
                return Err(error);
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
                if storage_binding(&at) == storage_binding(config) {
                    refresh_credentials(&mut previous, secrets);
                }
                pending_close_add(
                    &ctx.paths,
                    PendingClose {
                        storage: at,
                        previous,
                        closed_seen: false,
                    },
                )?;
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
                storage: config.clone(),
                previous: secrets.clone(),
                closed_seen: false,
            }],
        )
        .unwrap();
        finish_pending_close(&ctx.paths, &config, &secrets).await.unwrap();
        // 兼容标记补上了、记录删了；旧空间对新版一律拒绝。
        assert!(matches!(
            remote::open_space(&space, &keys, &secrets.space_id, false).await,
            Err(SyncError::SpaceClosed)
        ));
        assert!(pending_close_load(&ctx.paths).unwrap().is_empty());
    }

    #[test]
    fn close_outcomes_merge_into_the_latest_queue() {
        let secrets = paired();
        let config_at = |dir: &str| {
            let mut config = SyncConfig::default();
            config.transport = "file".into();
            config.file.dir = dir.into();
            config
        };
        let first = config_at("C:/sync-a");
        let second = config_at("C:/sync-b");
        let entry = |storage: &SyncConfig, closed_seen: bool| PendingClose {
            storage: storage.clone(),
            previous: secrets.clone(),
            closed_seen,
        };
        // 开头的快照里有一条失败、一条成功；期间另一进程又追加了一条。
        let mut latest = vec![entry(&first, false), entry(&second, false)];
        latest.push(entry(&config_at("C:/sync-c"), false));
        merge_close_outcomes(&mut latest, &[(entry(&first, true), false), (entry(&second, false), true)]);
        // 失败的那条留着，并带上这次确认过的事实；成功的那条删掉；新追加的不动。
        assert_eq!(latest.len(), 2);
        let kept = latest
            .iter()
            .find(|close| same_close_target(close, &first, &secrets.space_id))
            .unwrap();
        assert!(kept.closed_seen);
        assert!(!latest
            .iter()
            .any(|close| same_close_target(close, &second, &secrets.space_id)));
        assert!(latest
            .iter()
            .any(|close| close.storage.file.dir == "C:/sync-c"));
    }

    #[test]
    fn close_records_keep_each_storage_and_remember_a_confirmed_close() {
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
                storage: first.clone(),
                previous: secrets.clone(),
                closed_seen: true,
            },
        )
        .unwrap();
        // 同一个空间复制到另一个存储位置：两处各留一条。
        pending_close_add(
            &ctx.paths,
            PendingClose {
                storage: second.clone(),
                previous: secrets.clone(),
                closed_seen: false,
            },
        )
        .unwrap();
        assert_eq!(pending_close_load(&ctx.paths).unwrap().len(), 2);
        // 同一目标再来一条（这次不知道关闭已确认）：合并成一条，但确认过的事实不丢。
        pending_close_add(
            &ctx.paths,
            PendingClose {
                storage: first.clone(),
                previous: secrets.clone(),
                closed_seen: false,
            },
        )
        .unwrap();
        let closes = pending_close_load(&ctx.paths).unwrap();
        assert_eq!(closes.len(), 2);
        assert!(closes.iter().any(|close| {
            same_close_target(close, &first, &secrets.space_id) && close.closed_seen
        }));
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
                storage: config.clone(),
                previous: secrets.clone(),
                closed_seen: false,
            }],
        )
        .unwrap();
        finish_pending_close(&ctx.paths, &config, &secrets).await.unwrap();
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
