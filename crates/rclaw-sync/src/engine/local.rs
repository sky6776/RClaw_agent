//! 不经引擎线程、直接在调用线程上做的本机操作：同步状态、删除会话、「仅本机」标记、
//! 会话列表的角标、附件清单。它们只碰数据库和本机文件，不碰网络，也不等轮次。
//!
//! 都是阻塞调用：宿主从阻塞线程上调（桌面端的命令都包在 `spawn_blocking` 里）。

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use rclaw_store::session::raw::SessionDeleteSync;
use rclaw_store::sync_db::{self, INBOX_PENDING, INBOX_QUEUED, SEAL_LOCAL, SEAL_SEALED};
use rclaw_store::MemoryTarget;
use rusqlite::params;
use serde::Serialize;

use crate::attach::{self, now_ms, Backlog};
use crate::config::{Scopes, SyncConfig, SCOPE_NAMES};
use crate::db::{self, scope, BlobRow, BLOB_HELD, BLOB_PENDING, BLOB_REMOTE};
use crate::error::{Result, SyncError};
use crate::files::cron::{self, RemoteJob};
use crate::files::{memory, skills};
use crate::ids;
use crate::map::{self, SessionMap};
use crate::publish::{self, Settings};
use crate::status::{self, SyncStatus};
use crate::trash;
use crate::wire::{StoredRefs, SYNC_DIR_PREFIX};
use crate::Ctx;

/// 占住一个会话时每隔这么久试一次、最多等这么久（§7.3）。
const HOLD_STEP: Duration = Duration::from_millis(50);
const HOLD_WAIT: Duration = Duration::from_secs(2);
/// 别的设备的运行提示超过这么久没有更新就不再显示（那台设备多半是中途退出了）。
const RUNNING_HINT_MAX_MS: i64 = 2 * 3600 * 1000;

/// 这台设备实际参与的同步范围。
pub(crate) fn scopes_of(config: &SyncConfig, sessions_supported: bool) -> Scopes {
    let scopes = config.scope_set();
    if sessions_supported {
        scopes
    } else {
        scopes.without_sessions()
    }
}

/// 一轮同步用的设置。
pub(crate) fn settings_of(
    config: &SyncConfig,
    sessions_supported: bool,
    device: String,
    my_ids: Vec<String>,
) -> Settings {
    Settings {
        device,
        my_ids,
        scopes: scopes_of(config, sessions_supported),
        channels: config.channels.clone(),
        device_name: config.effective_device_name(),
        trash_days: config.trash_days,
        attachment_max: config.attachment_max_bytes(),
    }
}

/// 同 [`settings_of`]，设备身份从库里读（引擎没在跑的时候用）。还没有身份时为空。
pub(crate) fn local_settings(
    conn: &rusqlite::Connection,
    config: &SyncConfig,
    sessions_supported: bool,
) -> Result<Settings> {
    let my_ids = sync_db::device_ids(conn)?;
    let device = my_ids.first().cloned().unwrap_or_default();
    Ok(settings_of(config, sessions_supported, device, my_ids))
}

fn tables_exist(ctx: &Ctx) -> Result<bool> {
    db::read(ctx.db(), |conn| Ok(sync_db::sync_tables_exist(conn)?))
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

/// 引擎线程维护的那一半状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct View {
    pub state: &'static str,
    pub message: Option<String>,
    pub last_sync_ms: i64,
    pub space_id: Option<String>,
    /// 本机存着当前这种存储的密码。
    pub secret_saved: bool,
}

impl Default for View {
    fn default() -> Self {
        View {
            state: "off",
            message: None,
            last_sync_ms: 0,
            space_id: None,
            secret_saved: false,
        }
    }
}

/// 面板上的同步状态。没开启同步时不碰数据库。
pub(crate) fn status(
    ctx: &Ctx,
    config: &SyncConfig,
    sessions_supported: bool,
    view: View,
) -> SyncStatus {
    let scopes = scopes_of(config, sessions_supported);
    let enabled = [
        scopes.sessions,
        scopes.memory,
        scopes.persona,
        scopes.attachments,
        scopes.skills,
        scopes.bots,
        scopes.cron,
    ];
    let mut status = SyncStatus {
        state: view.state.to_string(),
        message: view.message,
        transport: config.transport.clone(),
        secret_saved: view.secret_saved,
        space_id: view.space_id,
        device_name: config.effective_device_name(),
        scopes: SCOPE_NAMES
            .iter()
            .zip(enabled)
            .filter(|(_, on)| *on)
            .map(|(name, _)| name.to_string())
            .collect(),
        last_sync_ms: view.last_sync_ms,
        ..SyncStatus::default()
    };
    if !config.enabled {
        status.state = "off".into();
        status.message = None;
        return status;
    }
    let now = now_ms();
    let name = status.device_name.clone();
    let read = db::read(ctx.db(), |conn| {
        if !sync_db::sync_tables_exist(conn)? {
            return Ok(None);
        }
        let device = sync_db::device_id(conn)?;
        let outbox = map::outbox_len(conn)?;
        let inbox: i64 = conn.query_row(
            "SELECT count(*) FROM sync_inbox WHERE state IN (?1, ?2)",
            params![INBOX_PENDING, INBOX_QUEUED],
            |row| row.get(0),
        )?;
        let uploads: i64 = conn.query_row(
            "SELECT count(*) FROM sync_blob WHERE state = ?1",
            params![BLOB_PENDING],
            |row| row.get(0),
        )?;
        // 存量附件：范围开着、用户还没选过上传范围时才提示。
        let asked = db::kv_json::<Backlog>(conn, db::KV_BACKLOG)?.is_some();
        let backlog: i64 = if scopes.attachments && !asked {
            conn.query_row(
                "SELECT COALESCE(sum(size), 0) FROM sync_blob WHERE state = ?1",
                params![BLOB_HELD],
                |row| row.get(0),
            )?
        } else {
            0
        };
        let devices = match &device {
            Some(device) => status::devices(conn, device, &name, now)?,
            None => Vec::new(),
        };
        let pending = status::derived(conn, now)?;
        Ok(Some((
            device, outbox, inbox, uploads, backlog, devices, pending,
        )))
    });
    match read {
        Ok(Some((device, outbox, inbox, uploads, backlog, devices, pending))) => {
            status.device_id = device;
            status.outbox = outbox;
            status.inbox = inbox.max(0) as u64;
            status.uploads = uploads.max(0) as u64;
            status.backlog_bytes = backlog.max(0) as u64;
            status.devices = devices;
            status.pending = pending;
        }
        Ok(None) => {}
        Err(error) => tracing::warn!("读取同步状态失败: {error}"),
    }
    status.pending.extend(status::list(&ctx.paths));
    status
}

/// 把别的设备标成「已丢失」（它的定时任务可以被强制接管，§8.6），或取消这个标记。
pub(crate) fn mark_lost(ctx: &Ctx, device: &str, lost: bool) -> Result<()> {
    if !tables_exist(ctx)? {
        return Err(SyncError::invalid("多端同步还没有开启过"));
    }
    db::write(ctx.db(), |tx| cron::mark_lost(tx, device, lost))
}

/// 别的设备上的定时任务（只读镜像）。
pub(crate) fn remote_cron(
    ctx: &Ctx,
    config: &SyncConfig,
    sessions_supported: bool,
) -> Result<Vec<RemoteJob>> {
    // 没开同步、或者没在同步定时任务：镜像不再更新，接手请求也没有人处理（对比只在
    // 这个范围开着时做），不列——列出来的会是过时的任务，点「移到本机执行」会一直等下去。
    if !config.enabled || !config.scope_set().cron {
        return Ok(Vec::new());
    }
    let now = now_ms();
    db::read(ctx.db(), |conn| {
        if !sync_db::sync_tables_exist(conn)? {
            return Ok(Vec::new());
        }
        let s = local_settings(conn, config, sessions_supported)?;
        cron::remote_jobs(conn, &s, now)
    })
}

/// 从别的设备同步来、现在装在本机的技能：`(技能名, 来源设备的展示名)`（§8.4）。同步
/// 此刻开没开都照实说（装上的东西不会因为关了同步就变成本机写的）；没开过同步时为空，
/// 不建同步表。
pub(crate) fn skill_origins(ctx: &Ctx) -> Result<Vec<(String, String)>> {
    db::read(ctx.db(), |conn| {
        if !sync_db::sync_tables_exist(conn)? {
            return Ok(Vec::new());
        }
        skills::origins(conn)
    })
}

// ---------------------------------------------------------------------------
// 删除会话与「仅本机」
// ---------------------------------------------------------------------------

/// 占住一个会话：合入不会在这期间往里追加，运行时也不会在它上面启动 turn。会话正忙就
/// 每隔一小会儿再试，最多等两秒。
fn hold_session(ctx: &Ctx, channel: &str, chat_id: &str) -> Result<Box<dyn Send>> {
    let deadline = Instant::now() + HOLD_WAIT;
    loop {
        if let Some(guard) = ctx.gate.begin_merge(channel, chat_id) {
            return Ok(guard);
        }
        if Instant::now() >= deadline {
            return Err(SyncError::other(
                "这个会话正忙（有回复在进行，或者同步正在往里合入内容），请稍后再试",
            ));
        }
        std::thread::sleep(HOLD_STEP);
    }
}

fn session_map_of(ctx: &Ctx, channel: &str, chat_id: &str) -> Result<Option<SessionMap>> {
    db::read(ctx.db(), |conn| {
        if !sync_db::sync_tables_exist(conn)? {
            return Ok(None);
        }
        map::session_map_by_key(conn, channel, chat_id)
    })
}

/// 删除一个会话（§7.5）。已经发布过的会话：删除会传播到其他设备，所以先占住它、把它
/// 导出到本机回收站（`keep_trash`，「彻底删除」不留），再删。没发布过的就是普通的本机
/// 删除。返回会话原来在不在。
pub(crate) fn delete_session(
    ctx: &Ctx,
    channel: &str,
    chat_id: &str,
    purge: bool,
    keep_trash: bool,
) -> Result<bool> {
    let mode = if purge {
        SessionDeleteSync::Purge
    } else {
        SessionDeleteSync::Propagate
    };
    let published =
        session_map_of(ctx, channel, chat_id)?.is_some_and(|session| session.sealed == SEAL_SEALED);
    if !published {
        return Ok(ctx
            .env
            .sessions
            .delete_session_with(channel, chat_id, mode)?);
    }
    let guard = hold_session(ctx, channel, chat_id)?;
    let existed = delete_held(ctx, channel, chat_id, mode, keep_trash && !purge)?;
    drop(guard);
    Ok(existed)
}

/// 删除一个已经发布的会话。调用方保证这期间没有 turn、没有合入在动它。
fn delete_held(
    ctx: &Ctx,
    channel: &str,
    chat_id: &str,
    mode: SessionDeleteSync,
    stash: bool,
) -> Result<bool> {
    if stash {
        trash::stash(ctx, channel, chat_id).map_err(|error| {
            SyncError::other(format!(
                "删除前把会话放进回收站失败（{error}），没有删除。不需要保留的话可以用「彻底删除」"
            ))
        })?;
    }
    Ok(ctx
        .env
        .sessions
        .delete_session_with(channel, chat_id, mode)?)
}

/// `/clear confirm all`（§7.5）：把参与同步、已经发布的会话逐个按「删除」处理——本机留
/// 一份在回收站（`keep_trash`），删除传播到其他设备。返回删掉的个数。没发布的、不参与
/// 同步的会话不在这里动，由调用方接着清。
///
/// 调用方（运行时的 `/clear`）持着静默门：没有 turn 在跑，合入也登记不上，所以这里不再
/// 逐个去占会话——门关着的时候也占不到。
pub(crate) fn delete_published(
    ctx: &Ctx,
    config: &SyncConfig,
    sessions_supported: bool,
    keep_trash: bool,
) -> Result<usize> {
    let targets: Vec<(String, String)> = db::read(ctx.db(), |conn| {
        if !sync_db::sync_tables_exist(conn)? {
            return Ok(Vec::new());
        }
        let s = local_settings(conn, config, sessions_supported)?;
        let mut targets = Vec::new();
        for session in map::all_sessions(conn)? {
            if session.sealed == SEAL_SEALED
                && publish::eligible(conn, &s, &session.channel, &session.chat_id)?
            {
                targets.push((session.channel, session.chat_id));
            }
        }
        Ok(targets)
    })?;
    let mut deleted = 0;
    for (channel, chat_id) in targets {
        if delete_held(
            ctx,
            &channel,
            &chat_id,
            SessionDeleteSync::Propagate,
            keep_trash,
        )? {
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// `/clear confirm all`（§7.5）的第一半：调用方马上要清空本机的 `MEMORY.md`，先把现有的
/// 条目在回收站里留一份（`keep_trash`），并登记「正在清理」的意图（[`memory::mark_clearing`]）
/// ——之后的文件删除要是只成功了一半，后台对比不能把缺掉的条目当成正常删除发给别的
/// 设备（`/clear` 会报失败、让用户重试，这次清理并没有完成）。返回条目标识。这一步本身
/// 没有任何可发布的副作用——把清空登记成发给所有设备的删除（[`commit_memory_clear`]）
/// 留给调用方在本地清空成功之后做。
pub(crate) fn stash_memory_clear(ctx: &Ctx, keep_trash: bool) -> Result<Vec<String>> {
    let entries = match ctx.memory.list_entries(MemoryTarget::Memory) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!("清空之前读不出记忆文件，回收站里没有留副本: {error}");
            Vec::new()
        }
    };
    if keep_trash {
        trash::stash_memory(
            &ctx.paths,
            "memory",
            "/clear confirm all 清空之前，本机记忆文件 MEMORY.md 里的条目",
            &entries,
        )?;
    }
    let ids: Vec<String> = entries.iter().map(|text| memory::mem_id(text)).collect();
    // 清理意图：只在文件删除之前登记，让后台对比跳到这次清理有了着落为止。没开过同步
    // 的库里没有同步表，也不需要这个标记。
    db::write(ctx.db(), |tx| {
        if sync_db::sync_tables_exist(tx)? {
            memory::mark_clearing(tx, "memory", &ids)?;
        }
        Ok(())
    })?;
    Ok(ids)
}

/// `/clear confirm all`（§7.5）的第二半：本地清空已经成功，把清空登记成发给所有设备的
/// 删除（[`memory::publish_clear`]）——用户已经明说了范围，不等下一轮对比自己发现「文件
/// 空了」再来问一遍。返回登记的删除数。
///
/// 没在同步记忆（范围关着、还没有同步数据）时不登记：以后打开这个范围，对比发现原有的
/// 条目都不在了，会问用户。
pub(crate) fn commit_memory_clear(
    ctx: &Ctx,
    config: &SyncConfig,
    sessions_supported: bool,
    ids: &[String],
) -> Result<usize> {
    if !scopes_of(config, sessions_supported).memory {
        return Ok(0);
    }
    db::write(ctx.db(), |tx| {
        if !sync_db::sync_tables_exist(tx)? {
            return Ok(0);
        }
        let s = local_settings(tx, config, sessions_supported)?;
        if s.device.is_empty() {
            return Ok(0);
        }
        memory::publish_clear(tx, &s, "memory", ids)
    })
}

pub(crate) fn is_local_only(ctx: &Ctx, channel: &str, chat_id: &str) -> Result<bool> {
    let key = db::local_only_key(channel, chat_id);
    db::read(ctx.db(), |conn| Ok(sync_db::kv_get(conn, &key)?.is_some()))
}

/// 标记 / 取消「仅本机」（§7.9）。没开过同步也可以标（新建会话时就能选）。
///
/// 标记一个已经发布的会话：向其他设备发「彻底删除」，本机这一份解除与线程的对应关系
/// ——以后取消标记时，它作为一个新线程重新发布。
pub(crate) fn set_local_only(ctx: &Ctx, channel: &str, chat_id: &str, on: bool) -> Result<()> {
    if !ids::is_valid_channel(channel) || !ids::is_valid_chat_id(chat_id) {
        return Err(SyncError::invalid("会话标识不合规"));
    }
    let key = db::local_only_key(channel, chat_id);
    if !on {
        return db::write(ctx.db(), |tx| {
            sync_db::kv_del(tx, &key)?;
            // 下一次发布扫描从头来，把它捡起来。
            publish::reset_cursor(tx)
        });
    }
    let mapped = session_map_of(ctx, channel, chat_id)?.is_some();
    let _guard = match mapped {
        true => Some(hold_session(ctx, channel, chat_id)?),
        false => None,
    };
    db::write(ctx.db(), |tx| {
        sync_db::kv_put(tx, &key, "true")?;
        if !sync_db::sync_tables_exist(tx)? {
            return Ok(());
        }
        let Some(session) = map::session_map_by_key(tx, channel, chat_id)? else {
            return Ok(());
        };
        // 它引用的附件还在等上传的不传了（别的会话也引用、别的设备真要的话会来点名要）。
        for row in map::session_msg_maps(tx, session.session_id)? {
            let Some(refs) = StoredRefs::parse(row.refs.as_deref()) else {
                continue;
            };
            let blobs = refs
                .files
                .iter()
                .map(|file| file.blob.as_str())
                .chain(refs.artifacts.iter().filter_map(|a| a.blob.as_deref()));
            for blob in blobs {
                tx.execute(
                    "UPDATE sync_blob SET state = ?2 WHERE blob_id = ?1 AND state = ?3",
                    params![blob, BLOB_HELD, BLOB_PENDING],
                )?;
            }
        }
        // 待发队列里它的 op 清掉；发布过的话登记一条「彻底删除」。
        sync_db::on_session_delete(tx, session.session_id, true, true)?;
        tx.execute(
            "DELETE FROM sync_msg WHERE session_id = ?1",
            params![session.session_id],
        )?;
        tx.execute(
            "DELETE FROM sync_session WHERE session_id = ?1",
            params![session.session_id],
        )?;
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// 会话列表的角标
// ---------------------------------------------------------------------------

/// 一个会话在同步上的情况。只列出至少有一项不是默认值的会话。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SessionMark {
    pub channel: String,
    pub chat_id: String,
    /// 用户把它标成了「仅本机」。
    pub local_only: bool,
    /// 它参与同步：别的设备上有它（或马上会有）。
    pub synced: bool,
    /// 别的设备已经（或可能已经）有它了：在本机删掉它，删除会传到其他设备，本机先在
    /// 回收站里留一份。还在排队、一次都没传出去过的会话不算——删它只是本机的事，也不进
    /// 回收站。同步此刻关着也照样标：删除登记在库里，同步一恢复就传出去。
    pub published: bool,
    /// 它是并发续写时自动分出来的分支：造成分叉的那台设备的名字。
    pub branch_of: Option<String>,
    /// 它的会话键在本机被别的会话占了，换了一个键落地：来源设备的名字。
    pub landed_from: Option<String>,
    /// 它正在这些设备上运行（§7.4 的提示）。
    pub running_on: Vec<String>,
}

const LOCAL_ONLY_PREFIX: &str = "sync/local-only/";

pub(crate) fn session_marks(
    ctx: &Ctx,
    config: &SyncConfig,
    sessions_supported: bool,
) -> Result<Vec<SessionMark>> {
    let now = now_ms();
    db::read(ctx.db(), |conn| {
        let mut marks: BTreeMap<(String, String), SessionMark> = BTreeMap::new();
        fn entry<'a>(
            marks: &'a mut BTreeMap<(String, String), SessionMark>,
            channel: &str,
            chat_id: &str,
        ) -> &'a mut SessionMark {
            marks
                .entry((channel.to_string(), chat_id.to_string()))
                .or_insert_with(|| SessionMark {
                    channel: channel.to_string(),
                    chat_id: chat_id.to_string(),
                    ..SessionMark::default()
                })
        }
        // 「仅本机」标记：kv 里的一段前缀（`/` 的下一个字符是 `0`）。
        {
            let mut stmt = conn.prepare_cached(
                "SELECT key FROM kv WHERE key >= 'sync/local-only/' AND key < 'sync/local-only0'",
            )?;
            let keys = stmt.query_map([], |row| row.get::<_, String>(0))?;
            for key in keys {
                let key = key?;
                if let Some((channel, chat_id)) = key[LOCAL_ONLY_PREFIX.len()..].split_once('/') {
                    entry(&mut marks, channel, chat_id).local_only = true;
                }
            }
        }
        if !sync_db::sync_tables_exist(conn)? {
            return Ok(marks.into_values().collect());
        }
        let sessions = map::all_sessions(conn)?;
        // 与删除时登记不登记 `session.delete` 是同一个条件（`sync_db::on_session_delete`）。
        for session in sessions
            .iter()
            .filter(|session| session.sealed == SEAL_SEALED)
        {
            entry(&mut marks, &session.channel, &session.chat_id).published = true;
        }
        if !config.enabled {
            return Ok(marks.into_values().collect());
        }
        let s = local_settings(conn, config, sessions_supported)?;
        for session in &sessions {
            if session.sealed != SEAL_LOCAL
                && publish::eligible(conn, &s, &session.channel, &session.chat_id)?
            {
                entry(&mut marks, &session.channel, &session.chat_id).synced = true;
            }
        }
        let by_row = |key: &str| -> Option<&SessionMap> {
            let id: i64 = key.parse().ok()?;
            sessions.iter().find(|session| session.session_id == id)
        };
        for (key, row) in db::state_scan(conn, scope::BRANCH, "")? {
            if let (Some(session), Some(device)) = (by_row(&key), row.value.as_deref()) {
                entry(&mut marks, &session.channel, &session.chat_id).branch_of =
                    Some(db::device_label(conn, device)?);
            }
        }
        for (key, row) in db::state_scan(conn, scope::LANDED, "")? {
            if let (Some(session), Some(device)) = (by_row(&key), row.value.as_deref()) {
                entry(&mut marks, &session.channel, &session.chat_id).landed_from =
                    Some(db::device_label(conn, device)?);
            }
        }
        for (device, peer) in db::peers(conn)? {
            let fresh = peer.seen_ms > 0 && now - peer.seen_ms <= RUNNING_HINT_MAX_MS;
            if peer.running.is_empty() || peer.replaced_by.is_some() || !fresh || s.is_me(&device) {
                continue;
            }
            let label = db::device_label(conn, &device)?;
            for session in sessions
                .iter()
                .filter(|session| peer.running.contains(&session.sid))
            {
                entry(&mut marks, &session.channel, &session.chat_id)
                    .running_on
                    .push(label.clone());
            }
        }
        Ok(marks.into_values().collect())
    })
}

// ---------------------------------------------------------------------------
// 附件
// ---------------------------------------------------------------------------

/// 一条消息引用的一个附件在本机的情况（§8.7）。界面先看这份清单，清单里没有的再按
/// 工作区路径找。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AttachmentInfo {
    pub message_id: i64,
    /// `file`：被消息引用的工作区文件；`artifact`：工具结果的全文。
    pub kind: &'static str,
    /// 消息里写的相对路径（`uploads/…`、`media/…`、`output/…`）；artifact 是文件名。
    pub path: String,
    pub blob: String,
    pub size: u64,
    /// `local`：本机有，就在原来的位置；`cached`：从别的设备取回、放在同步缓存里的
    /// 一份快照；`remote`：还在别的设备上，没有下载；`missing`：本机登记过，但文件已经
    /// 不在了。
    ///
    /// 去取过、同步空间里却没有这个文件的（§8.7）：`asked`：向别的设备要了，在等它们
    /// 传上来；`gone`：保存着它的设备说已经拿不出来了；`lost`：等了很久也没有设备传
    /// 上来。后两种本机不再自动去取，用户点「重试」时才再去看一次。
    pub state: &'static str,
    /// 本机上这个文件的绝对路径（有的话）。
    pub local_path: Option<String>,
    /// 产生这条消息的设备的名字；本机产生的为空。
    pub origin: Option<String>,
}

struct FoundRef {
    message_id: i64,
    kind: &'static str,
    path: String,
    blob: String,
    size: u64,
    row: Option<BlobRow>,
    /// 登记是「还在别的设备上」的：`remote` / `asked` / `gone` / `lost`。
    remote: &'static str,
    origin: Option<String>,
}

pub(crate) fn attachments(ctx: &Ctx, channel: &str, chat_id: &str) -> Result<Vec<AttachmentInfo>> {
    let found: Vec<FoundRef> = db::read(ctx.db(), |conn| {
        if !sync_db::sync_tables_exist(conn)? {
            return Ok(Vec::new());
        }
        let Some(session) = map::session_map_by_key(conn, channel, chat_id)? else {
            return Ok(Vec::new());
        };
        let mine = sync_db::device_ids(conn)?;
        let rows: Vec<(i64, String, String)> = {
            let mut stmt = conn.prepare_cached(
                "SELECT message_id, gid, refs FROM sync_msg WHERE session_id = ?1 \
                 AND refs IS NOT NULL AND refs != '{}' ORDER BY message_id",
            )?;
            let rows = stmt.query_map(params![session.session_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut labels: HashMap<String, String> = HashMap::new();
        let mut out = Vec::new();
        let now = now_ms();
        // 登记连同「还在别的设备上」的那几种情况一起查出来。
        let lookup = |blob: &str| -> Result<(Option<BlobRow>, &'static str)> {
            let row = db::blob_get(conn, blob)?;
            let remote = match &row {
                Some(row) if row.state == BLOB_REMOTE => attach::remote_state(conn, blob, now)?,
                _ => "remote",
            };
            Ok((row, remote))
        };
        for (message_id, gid, refs) in rows {
            let Some(refs) = StoredRefs::parse(Some(&refs)) else {
                continue;
            };
            let origin = match ids::gid_device(&gid) {
                Some(device) if !mine.iter().any(|id| id == device) => {
                    if !labels.contains_key(device) {
                        labels.insert(device.to_string(), db::device_label(conn, device)?);
                    }
                    labels.get(device).cloned()
                }
                _ => None,
            };
            for file in refs.files {
                let (row, remote) = lookup(&file.blob)?;
                out.push(FoundRef {
                    message_id,
                    kind: "file",
                    row,
                    remote,
                    path: file.path,
                    blob: file.blob,
                    size: file.size,
                    origin: origin.clone(),
                });
            }
            for artifact in refs.artifacts {
                let Some(blob) = artifact.blob else {
                    continue;
                };
                let (row, remote) = lookup(&blob)?;
                out.push(FoundRef {
                    message_id,
                    kind: "artifact",
                    row,
                    remote,
                    path: artifact.name,
                    blob,
                    size: artifact.size,
                    origin: origin.clone(),
                });
            }
        }
        Ok(out)
    })?;
    // 文件在不在：库锁之外看。
    Ok(found
        .into_iter()
        .map(|found| {
            // 只认原样存在的文件：名字要改写才拼得上的路径（别的系统上的文件名），本机
            // 工作区里不会有它。
            let present = |rel_path: &str| {
                let path = ctx.env.resolve_exact(rel_path)?;
                path.is_file().then(|| path.to_string_lossy().into_owned())
            };
            let (state, local_path) = match &found.row {
                Some(row) if row.state == BLOB_REMOTE => (found.remote, None),
                Some(row) => match present(&row.rel_path) {
                    Some(path) if row.rel_path.starts_with(SYNC_DIR_PREFIX) => {
                        ("cached", Some(path))
                    }
                    Some(path) => ("local", Some(path)),
                    None => ("missing", None),
                },
                // 没有登记（登记被回收了）：工作区里原来的位置上还有就算本机有。
                None if found.kind == "file" => match present(&found.path) {
                    Some(path) => ("local", Some(path)),
                    None => ("missing", None),
                },
                None => ("missing", None),
            };
            AttachmentInfo {
                message_id: found.message_id,
                kind: found.kind,
                path: found.path,
                blob: found.blob,
                size: found.size,
                state,
                local_path,
                origin: found.origin,
            }
        })
        .collect())
}

/// 一个会话最后 `tail` 条消息引用的、还在别的设备上的附件（打开会话、续聊之前补齐用）。
/// 登记的大小超过 `max_size`（本机设置的附件大小上限）的不算：反正取不回来，不必每次
/// 打开会话都去碰一次。向别的设备要过、还在等的照样算；已经收了场的隔一天才算一次
/// （§8.7，[`attach::worth_fetching`]）。`now`：现在的时刻（Unix 毫秒）。
pub(crate) fn tail_remote_blobs(
    ctx: &Ctx,
    channel: &str,
    chat_id: &str,
    tail: usize,
    max_size: u64,
    now: i64,
) -> Result<Vec<String>> {
    db::read(ctx.db(), |conn| {
        if !sync_db::sync_tables_exist(conn)? {
            return Ok(Vec::new());
        }
        let Some(session) = map::session_map_by_key(conn, channel, chat_id)? else {
            return Ok(Vec::new());
        };
        let refs: Vec<Option<String>> = {
            let mut stmt = conn.prepare_cached(
                "SELECT refs FROM sync_msg WHERE session_id = ?1 ORDER BY message_id DESC LIMIT ?2",
            )?;
            let rows =
                stmt.query_map(params![session.session_id, tail as i64], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut out: Vec<String> = Vec::new();
        for refs in refs {
            let Some(refs) = StoredRefs::parse(refs.as_deref()) else {
                continue;
            };
            let blobs = refs.files.into_iter().map(|file| file.blob).chain(
                refs.artifacts
                    .into_iter()
                    .filter_map(|artifact| artifact.blob),
            );
            for blob in blobs {
                if !out.contains(&blob)
                    && db::blob_get(conn, &blob)?
                        .is_some_and(|row| row.state == BLOB_REMOTE && row.size <= max_size)
                    && attach::worth_fetching(conn, &blob, now)?
                {
                    out.push(blob);
                }
            }
        }
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::BLOB_FETCHED;
    use crate::op::FileRef;
    use crate::testing::{self, Device};
    use rclaw_core::{Message, Role};

    fn say(device: &Device, chat_id: &str, text: &str) -> i64 {
        device
            .sessions()
            .append("desktop", chat_id, &Message::plain(Role::User, text))
            .unwrap()
    }

    #[test]
    fn status_is_off_without_touching_sync_tables() {
        let (_g, ctx) = testing::ctx();
        let config = SyncConfig::default();
        let got = status(&ctx, &config, true, View::default());
        assert_eq!(got.state, "off");
        assert_eq!(got.scopes, ["sessions", "memory", "persona"]);
        assert!(!tables_exist(&ctx).unwrap());
        // CLI：没有会话列表，会话与附件不参与。
        let mut config = SyncConfig::default();
        config.scopes.push("attachments".into());
        assert_eq!(
            status(&ctx, &config, false, View::default()).scopes,
            ["memory", "persona"]
        );
    }

    #[test]
    fn status_counts_queues_and_lists_devices() {
        let device = Device::new();
        say(&device, "c1", "你好");
        let mut config = SyncConfig::default();
        config.enabled = true;
        config.scopes.push("attachments".into());
        crate::publish::scan_sessions(&device.ctx, &device.settings, true).unwrap();
        db::write(device.ctx.db(), |tx| {
            db::blob_register(tx, "b1", "uploads/a.bin", 7, BLOB_HELD)?;
            db::blob_register(tx, "b2", "uploads/b.bin", 5, BLOB_PENDING)?;
            Ok(())
        })
        .unwrap();
        let view = View {
            state: "idle",
            message: None,
            last_sync_ms: 5,
            space_id: Some("s".into()),
            secret_saved: true,
        };
        let got = status(&device.ctx, &config, true, view.clone());
        assert_eq!(got.state, "idle");
        assert!(got.secret_saved);
        assert_eq!(
            got.device_id.as_deref(),
            Some(device.settings.device.as_str())
        );
        // device.hello、session.open、一条消息。
        assert_eq!(got.outbox, 3);
        assert_eq!(got.uploads, 1);
        assert_eq!(got.backlog_bytes, 7);
        assert_eq!(got.devices.len(), 1);
        assert!(got.devices[0].me);
        // 选过上传范围之后不再提示。
        db::write(device.ctx.db(), |tx| {
            db::kv_put_json(tx, db::KV_BACKLOG, &Backlog::Days(30))
        })
        .unwrap();
        assert_eq!(status(&device.ctx, &config, true, view).backlog_bytes, 0);
    }

    #[test]
    fn deleting_a_published_session_keeps_a_copy_in_the_trash() {
        let device = Device::new();
        say(&device, "c1", "你好");
        say(&device, "c2", "也发布了");
        testing::seal_all(&device);
        say(&device, "late", "封段之后才有的，还没发布");

        // 没发布过的：普通删除，不进回收站，也不登记删除。
        assert!(delete_session(&device.ctx, "desktop", "late", false, true).unwrap());
        assert!(trash::list(&device.ctx.paths).is_empty());
        assert!(testing::outbox(&device).is_empty());

        assert!(delete_session(&device.ctx, "desktop", "c1", false, true).unwrap());
        let trashed = trash::list(&device.ctx.paths);
        assert_eq!(trashed.len(), 1);
        assert_eq!(trashed[0].chat_id, "c1");
        let ops = testing::outbox(&device);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0]["t"], "session.delete");
        assert_eq!(ops[0]["purge"], false);

        // 彻底删除：不留回收站。
        assert!(delete_session(&device.ctx, "desktop", "c2", true, true).unwrap());
        assert_eq!(trash::list(&device.ctx.paths).len(), 1);
        assert_eq!(testing::outbox(&device)[1]["purge"], true);
        assert!(!delete_session(&device.ctx, "desktop", "c2", true, true).unwrap());
    }

    #[test]
    fn local_only_mark_unpublishes_and_unmarking_republishes_as_a_new_thread() {
        let device = Device::new();
        say(&device, "c1", "你好");
        testing::seal_all(&device);
        let before = session_map_of(&device.ctx, "desktop", "c1")
            .unwrap()
            .unwrap();

        // 没开过同步的库上也能标。
        let (_g, bare) = testing::ctx();
        set_local_only(&bare, "desktop", "x", true).unwrap();
        assert!(is_local_only(&bare, "desktop", "x").unwrap());
        assert!(!tables_exist(&bare).unwrap());
        assert!(set_local_only(&bare, "desktop", "../x", true).is_err());

        set_local_only(&device.ctx, "desktop", "c1", true).unwrap();
        assert!(is_local_only(&device.ctx, "desktop", "c1").unwrap());
        assert!(session_map_of(&device.ctx, "desktop", "c1")
            .unwrap()
            .is_none());
        let ops = testing::outbox(&device);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0]["t"], "session.delete");
        assert_eq!(ops[0]["sid"], before.sid.as_str());
        assert_eq!(ops[0]["purge"], true);
        // 标着的时候发布扫描不碰它。
        say(&device, "c1", "只在本机");
        testing::seal_all(&device);
        assert!(session_map_of(&device.ctx, "desktop", "c1")
            .unwrap()
            .is_none());

        set_local_only(&device.ctx, "desktop", "c1", false).unwrap();
        testing::seal_all(&device);
        let after = session_map_of(&device.ctx, "desktop", "c1")
            .unwrap()
            .unwrap();
        assert_ne!(after.sid, before.sid);
        assert_eq!(after.sealed, SEAL_SEALED);
    }

    #[test]
    fn marks_cover_local_only_synced_and_running_sessions() {
        let device = Device::new();
        say(&device, "c1", "你好");
        say(&device, "c2", "也发布了");
        testing::seal_all(&device);
        say(&device, "private", "仅本机");
        set_local_only(&device.ctx, "desktop", "private", true).unwrap();
        let sid = session_map_of(&device.ctx, "desktop", "c1")
            .unwrap()
            .unwrap()
            .sid;
        let row = session_map_of(&device.ctx, "desktop", "c2")
            .unwrap()
            .unwrap()
            .session_id;
        let other = "00112233445566ff";
        db::write(device.ctx.db(), |tx| {
            let mut peer = db::peer_state(tx, other)?;
            peer.running = vec![sid.clone()];
            peer.seen_ms = now_ms();
            db::put_peer_state(tx, other, &peer)?;
            db::state_put(
                tx,
                scope::BRANCH,
                &row.to_string(),
                db::HLC_ZERO,
                None,
                Some(other),
            )
        })
        .unwrap();

        let mut config = SyncConfig::default();
        // 同步关闭时保留「仅本机」和用于删除确认的已发布状态。
        let marks = session_marks(&device.ctx, &config, true).unwrap();
        assert_eq!(marks.len(), 3);
        assert!(marks
            .iter()
            .any(|mark| mark.local_only && mark.chat_id == "private"));
        assert_eq!(marks.iter().filter(|mark| mark.published).count(), 2);
        assert!(marks
            .iter()
            .all(|mark| !mark.synced && mark.running_on.is_empty()));

        config.enabled = true;
        let marks = session_marks(&device.ctx, &config, true).unwrap();
        let of = |chat_id: &str| marks.iter().find(|mark| mark.chat_id == chat_id).unwrap();
        assert!(of("c1").synced && !of("c1").local_only);
        assert_eq!(of("c1").running_on.len(), 1);
        assert!(of("c2").branch_of.is_some());
        assert!(of("private").local_only && !of("private").synced);
    }

    #[test]
    fn attachments_report_where_each_file_is() {
        let device = Device::new();
        let message = say(&device, "c1", "看这两个文件");
        testing::seal_all(&device);
        let ctx = &device.ctx;
        let here = ctx.env.resolve("uploads/a.txt");
        std::fs::create_dir_all(here.parent().unwrap()).unwrap();
        std::fs::write(&here, b"abc").unwrap();
        let cached_rel = crate::wire::cache_path("output/b.txt", "bb");
        let cached = ctx.env.resolve(&cached_rel);
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, b"defg").unwrap();
        let refs = StoredRefs {
            files: vec![
                FileRef {
                    path: "uploads/a.txt".into(),
                    blob: "aa".into(),
                    size: 3,
                },
                FileRef {
                    path: "output/b.txt".into(),
                    blob: "bb".into(),
                    size: 4,
                },
                FileRef {
                    path: "media/c.png".into(),
                    blob: "cc".into(),
                    size: 5,
                },
            ],
            artifacts: Vec::new(),
        };
        db::write(ctx.db(), |tx| {
            tx.execute(
                "UPDATE sync_msg SET refs = ?2 WHERE message_id = ?1",
                params![message, refs.to_column()],
            )?;
            db::blob_register(tx, "aa", "uploads/a.txt", 3, BLOB_HELD)?;
            db::blob_register(tx, "bb", &cached_rel, 4, BLOB_FETCHED)?;
            db::blob_register(tx, "cc", "sync/files/cc/cc/c.png", 5, BLOB_REMOTE)?;
            Ok(())
        })
        .unwrap();
        let got = attachments(ctx, "desktop", "c1").unwrap();
        let states: Vec<&str> = got.iter().map(|item| item.state).collect();
        assert_eq!(states, ["local", "cached", "remote"]);
        assert!(got[0].local_path.is_some() && got[2].local_path.is_none());
        assert!(got
            .iter()
            .all(|item| item.message_id == message && item.origin.is_none()));
        let start = now_ms();
        assert_eq!(
            tail_remote_blobs(ctx, "desktop", "c1", 20, 5, start).unwrap(),
            ["cc"]
        );
        // 比本机的附件大小上限还大的不预取。
        assert!(tail_remote_blobs(ctx, "desktop", "c1", 20, 4, start)
            .unwrap()
            .is_empty());
        assert!(attachments(ctx, "desktop", "nope").unwrap().is_empty());

        // 去取过、同步空间里没有的：照它现在的情况说（§8.7）。
        let (hour, day) = (3600 * 1000, 24 * 3600 * 1000);
        let remote = || attachments(ctx, "desktop", "c1").unwrap()[2].state;
        let tail = |at: i64| tail_remote_blobs(ctx, "desktop", "c1", 20, 5, at).unwrap();
        let me = device.settings.device.clone();
        let missing = |at: i64| {
            db::write(ctx.db(), |tx| attach::on_missing(tx, &me, "cc", false, at)).unwrap()
        };
        // 向别的设备要了，在等：打开会话时照样去看一眼。
        let asked = start - day;
        assert_eq!(
            missing(asked),
            attach::Missing::Waiting {
                asked: true,
                gone: false
            }
        );
        assert_eq!(remote(), "asked");
        assert_eq!(tail(start), ["cc"]);
        // 等了很久没有设备传上来：收场。打开会话时隔一天才再去存储上看一次。
        let ended = asked + 30 * day;
        assert_eq!(missing(ended), attach::Missing::Lost { gone: false });
        assert_eq!(remote(), "lost");
        assert!(tail(ended + hour).is_empty());
        assert_eq!(tail(ended + day), ["cc"]);
        assert_eq!(missing(ended + day), attach::Missing::Lost { gone: false });
        assert!(tail(ended + day + hour).is_empty());
        // 又有消息引用它：从头再来。
        db::write(ctx.db(), |tx| attach::on_new_ref(tx, "cc")).unwrap();
        assert_eq!(remote(), "remote");
        assert_eq!(tail(start), ["cc"]);
        // 保存着它的设备说拿不出来了：本机还没去取过，先照「取不回来」说。打开会话时照样
        // 去看一次、要一次（只等宽限那么久），那之后才隔一天看一次。
        db::write(ctx.db(), |tx| attach::on_blob_gone(tx, "cc", start)).unwrap();
        assert_eq!(remote(), "gone");
        assert_eq!(tail(start), ["cc"]);
        assert_eq!(
            missing(start),
            attach::Missing::Waiting {
                asked: true,
                gone: true
            }
        );
        assert_eq!(remote(), "asked");
        assert_eq!(tail(start), ["cc"]);
        assert_eq!(missing(start + hour), attach::Missing::Lost { gone: true });
        assert_eq!(remote(), "gone");
        assert!(tail(start + 2 * hour).is_empty());
        assert_eq!(tail(start + hour + day), ["cc"]);
    }
}
