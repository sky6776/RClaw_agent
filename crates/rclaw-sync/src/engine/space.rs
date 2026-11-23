//! 同步空间与存储：按配置组装传输、配对码、创建 / 加入 / 退出 / 重置空间、「测试连接」，
//! 以及换了空间之后清掉本机的同步状态。
//!
//! 这里的函数不知道引擎的线程与轮次：由 [`super::worker`] 在两轮之间调用。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use rclaw_store::sync_db::{self, KV_SYNC_DEVICE, KV_SYNC_HLC, KV_SYNC_SCHEMA, SEAL_LOCAL};
use rusqlite::params;

use crate::attach;
use crate::codec;
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
async fn save_created(ctx: &Ctx, next: &Secrets) -> Result<()> {
    let (ctx, next) = (ctx.clone(), next.clone());
    blocking(move || {
        db::write(ctx.db(), |tx| {
            db::kv_put_json(tx, db::KV_CREATOR, &next.space_id)
        })?;
        next.save(&ctx.paths)
    })
    .await
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
    save_created(ctx, &next).await?;
    *secrets = next;
    Ok(())
}

/// 用配对码加入一个已有的同步空间。先到存储上核对这个空间确实在、密钥对得上，再保存。
/// 返回加入之后的配置（配对码里带了存储设置的话与原来不同），宿主负责把它写进
/// `rclaw.toml`。
pub(crate) async fn join(
    paths: &SyncPaths,
    config: &SyncConfig,
    secrets: &mut Secrets,
    code: &str,
) -> Result<SyncConfig> {
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
    let (paths, saved) = (paths.clone(), next.clone());
    blocking(move || saved.save(&paths)).await?;
    *secrets = next;
    Ok(joined)
}

/// 退出同步空间：密钥与存储凭据从本机清掉，本机的同步状态回到「没同步过」。会话、记忆
/// 等数据原样留在本机；存储上的密文不动（别的设备还在用）。
pub(crate) async fn leave(ctx: &Ctx, secrets: &mut Secrets) -> Result<()> {
    if !secrets.is_paired() {
        return Ok(());
    }
    let mut next = secrets.clone();
    next.leave_space();
    let (ctx, saved) = (ctx.clone(), next.clone());
    blocking(move || {
        saved.save(&ctx.paths)?;
        reset_local_state(&ctx, None, None)
    })
    .await?;
    *secrets = next;
    Ok(())
}

/// 重置同步空间（§11.2，设备丢失之后用）：新建空间和新密钥，旧空间标记为已废弃——其余
/// 设备读到这个标记就停止同步，等用户拿新的配对码重新加入。本机随后从本地数据全量重新
/// 发布（换了空间，下次打开时清掉本机的同步状态）。
pub(crate) async fn reset(ctx: &Ctx, config: &SyncConfig, secrets: &mut Secrets) -> Result<()> {
    if !secrets.is_paired() {
        return Err(SyncError::other("还没有创建或加入同步空间"));
    }
    let old = load_live(config, secrets).await?;
    let mut next = secrets.clone();
    next.space_id = new_space_id();
    next.key = SpaceKey::generate().to_base64();
    let fresh = load_live(config, &next).await?;
    // 先把新空间建好，再废弃旧空间：哪一步不通，本机都还没有任何改动。
    create_file_namespace(config, &next.space_id).await?;
    remote::open_space(fresh.transport(), &fresh.keys, &next.space_id, true).await?;
    let closed = async {
        create_file_namespace(config, &old.space_id).await?;
        remote::close_space(old.transport(), &old.keys, &old.space_id).await
    }
    .await;
    if let Err(error) = closed {
        let _ = fresh.transport().delete(codec::SPACE_FILE).await;
        return Err(error);
    }
    save_created(ctx, &next).await?;
    *secrets = next;
    Ok(())
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
