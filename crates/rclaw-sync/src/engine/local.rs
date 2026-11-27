//! 不经引擎线程、直接在调用线程上做的本机操作：同步状态、删除会话、「仅本机」标记、
//! 会话列表的角标、附件清单。它们只碰数据库和本机文件，不碰网络，也不等轮次。
//!
//! 都是阻塞调用：宿主从阻塞线程上调（桌面端的命令都包在 `spawn_blocking` 里）。

use std::collections::{BTreeMap, HashMap};
use std::io::Read as _;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rclaw_store::session::raw::SessionDeleteSync;
use rclaw_store::sync_db::{self, INBOX_PENDING, INBOX_QUEUED, SEAL_LOCAL, SEAL_SEALED};
use rclaw_store::MemoryTarget;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::attach::{self, now_ms, Backlog};
use crate::config::{Scopes, SyncConfig, SCOPE_NAMES};
use crate::crypto::{sha256_reader, Keys};
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
    delete_session_checked(ctx, channel, chat_id, purge, keep_trash, None)
}

/// 同 [`delete_session`]，可带会话行主键前置条件（事务内核对，防止同键重建误删）。
pub(crate) fn delete_session_checked(
    ctx: &Ctx,
    channel: &str,
    chat_id: &str,
    purge: bool,
    keep_trash: bool,
    expected_row_id: Option<i64>,
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
            .delete_session_with_checked(channel, chat_id, mode, expected_row_id)?);
    }
    let guard = hold_session(ctx, channel, chat_id)?;
    let existed = delete_held_checked(
        ctx,
        channel,
        chat_id,
        mode,
        keep_trash && !purge,
        expected_row_id,
    )?;
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
    delete_held_checked(ctx, channel, chat_id, mode, stash, None)
}

fn delete_held_checked(
    ctx: &Ctx,
    channel: &str,
    chat_id: &str,
    mode: SessionDeleteSync,
    stash: bool,
    expected_row_id: Option<i64>,
) -> Result<bool> {
    // 过期确认不产生回收站副作用：先核对记录，再导出/删除（存储事务里还会再核对一次）。
    if let Some(expected) = expected_row_id {
        let current = ctx
            .env
            .sessions
            .session_row_id(channel, chat_id)
            .map_err(|error| SyncError::other(format!("校验会话记录失败: {error}")))?;
        if current != Some(expected) {
            return Err(SyncError::other("会话记录已变化，请刷新列表后再试"));
        }
    }
    // 先导出到暂存（不发布）；删除事务成功后才发布到回收站，失败则丢弃。
    // 记录身份已核对；按原有语义先导出回收站，再走带行校验的删除事务。
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
        .delete_session_with_checked(channel, chat_id, mode, expected_row_id)
        .map_err(SyncError::from)?)
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
/// 设备（`/clear` 会报失败、让用户重试，这次清理并没有完成）。返回条目标识与这次登记的
/// 操作标识。这一步本身没有任何可发布的副作用——意图先耐久落库，调用方才删本机文件；
/// 把清空登记成发给所有设备的删除（[`commit_memory_clear`]）留给调用方在本地清空成功
/// 之后做。
pub(crate) fn stash_memory_clear(
    ctx: &Ctx,
    keep_trash: bool,
    in_space: bool,
) -> Result<(Vec<String>, String)> {
    let tables = db::read(ctx.db(), |conn| Ok(sync_db::sync_tables_exist(conn)?))?;
    // 已配对、但同步表还没建好（首次装配、重建库）：意图登记不进去，本地删除就没有
    // 任何可结算的凭据——另一进程初始化后拉到旧条目，会在文件删掉之后写回、被当成
    // 新的再发布。让用户等同步连上再清，不能在删文件前跳过意图。
    if in_space && !tables {
        return Err(SyncError::other(
            "同步状态正在初始化，请等同步连上后再执行 /clear confirm all",
        ));
    }
    // 与后台对比的落地共用一把跨进程锁（见 [`memory::ClearLock`]）：读条目与登记意图
    // 必须原子，否则另一进程可能在这两步之间把清空前读到的远端条目写回文件——它们
    // 不在这次登记的删除集合里，随后会被当成「本机新写的」再发出去。没开过同步的
    // 库里没有同步表，也没有对比要串行：不建同步目录、不拿锁。
    let _clear_lock = if tables {
        match memory::ClearLock::try_acquire(ctx)? {
            Some(lock) => Some(lock),
            None => {
                return Err(SyncError::other(
                    "另一个进程正在对比或清理记忆文件，请稍后再试 /clear confirm all",
                ))
            }
        }
    } else {
        None
    };
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
    let op = db::write(ctx.db(), |tx| {
        if !sync_db::sync_tables_exist(tx)? {
            return Ok(String::new());
        }
        memory::mark_clearing(tx, "memory", &ids)
    })?;
    // 意图事务提交后、调用方删本机文件之前，必须越过落盘点：库平时是 synchronous=NORMAL，
    // 提交只写进 WAL、不 fsync，掉电 / 崩溃时这条意图会回滚，而已经删掉的 MEMORY.md 却
    // 留了下来——重启后的普通对比会把缺掉的一两条当成正常删除发给别的设备。落盘失败就
    // 返回错误，调用方不会继续删文件；重试走的也是这条路，同样要先落盘。
    db::flush(ctx.db()).map_err(|error| {
        SyncError::other(format!(
            "「正在清理记忆」的意图没能落盘（{error}），没有删本机文件"
        ))
    })?;
    Ok((ids, op))
}

/// `/clear confirm all`（§7.5）的第二半：本地清空已经成功，把清空登记成发给所有设备的
/// 删除（[`memory::publish_clear`]）——用户已经明说了范围，不等下一轮对比自己发现「文件
/// 空了」再来问一遍。返回登记的删除数。`op` 是 [`stash_memory_clear`] 返回的操作标识：
/// 复核与发布在同一个事务里，这次意图已经被另一次登记取代时不发布。
///
/// 没在同步记忆（范围关着、还没有同步身份）时不登记，但要明确结束这次意图：否则标记
/// 会一直挡着这个目标的对比。基线留着不动——以后打开这个范围，对比发现原有的条目都
/// 不在了，会照常问用户。
pub(crate) fn commit_memory_clear(
    ctx: &Ctx,
    config: &SyncConfig,
    sessions_supported: bool,
    ids: &[String],
    op: &str,
) -> Result<usize> {
    db::write(ctx.db(), |tx| {
        if !sync_db::sync_tables_exist(tx)? {
            return Ok(0);
        }
        let s = local_settings(tx, config, sessions_supported)?;
        if !s.scopes.memory || s.device.is_empty() {
            memory::abort_clearing(tx, "memory", op)?;
            return Ok(0);
        }
        memory::publish_clear(tx, &s, "memory", ids, op)
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
    set_local_only_checked(ctx, channel, chat_id, on, None, true)
}

/// 同 [`set_local_only`]，可在写事务内核对会话行主键：确认期间同键重建时，
/// 「设为仅本机」不能作用到新记录上。
pub(crate) fn set_local_only_checked(
    ctx: &Ctx,
    channel: &str,
    chat_id: &str,
    on: bool,
    expected_row_id: Option<i64>,
    confirmed: bool,
) -> Result<()> {
    if !ids::is_valid_channel(channel) || !ids::is_valid_chat_id(chat_id) {
        return Err(SyncError::invalid("会话标识不合规"));
    }
    let key = db::local_only_key(channel, chat_id);
    if !on {
        return db::write(ctx.db(), |tx| {
            if let Some(expected) = expected_row_id {
                let current: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM session WHERE channel = ?1 AND chat_id = ?2",
                        params![channel, chat_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if current != Some(expected) {
                    return Err(SyncError::other("会话记录已变化，请刷新列表后再试"));
                }
            }
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
        if let Some(expected) = expected_row_id {
            let current: Option<i64> = tx
                .query_row(
                    "SELECT id FROM session WHERE channel = ?1 AND chat_id = ?2",
                    params![channel, chat_id],
                    |row| row.get(0),
                )
                .optional()?;
            if current != Some(expected) {
                return Err(SyncError::other("会话记录已变化，请刷新列表后再试"));
            }
        }
        sync_db::kv_put(tx, &key, "true")?;
        if !sync_db::sync_tables_exist(tx)? {
            return Ok(());
        }
        if !confirmed {
            if let Some(session) = map::session_map_by_key(tx, channel, chat_id)? {
                if session.sealed == SEAL_SEALED {
                    let row_id: Option<i64> = tx
                        .query_row(
                            "SELECT id FROM session WHERE channel = ?1 AND chat_id = ?2",
                            params![channel, chat_id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    // #<行ID> 供前端确认后按同一记录重试；0 表示没读到（前端应终止）。
                    return Err(SyncError::other(format!(
                        "需要确认#{}：这个会话已经同步到其他设备，设为仅本机等于在那边删除",
                        row_id.unwrap_or(0)
                    )));
                }
            }
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
    /// `local`：本机有，就在原来的位置（落点暂时读不了、`Landing::Unknown` 时也可能是
    /// `local` 但 `local_path=None`）；`cached`：从别的设备取回、放在同步缓存里的一份
    /// 快照；`remote`：还在别的设备上，没有下载；`missing`：本机登记过，但文件已经不
    /// 在了。
    ///
    /// 去取过、同步空间里却没有这个文件的（§8.7）：`asked`：向别的设备要了，在等它们
    /// 传上来；`gone`：保存着它的设备说已经拿不出来了；`lost`：等了很久也没有设备传
    /// 上来。后两种本机不再自动去取，用户点「重试」时才再去看一次。
    ///
    /// `changed`：登记的那个落点上有个同名文件，内容已经不是这个对象了（被就地改写过）。
    /// 不能把它当「本机有」打开——点「重新下载」会取回登记的那份（落到同步缓存，不覆盖
    /// 现有文件）。
    pub state: &'static str,
    /// 本机上这个文件的绝对路径（有的话）。只给需要经引擎核对登记的后端入口用
    /// （预览、保存快照）；界面打开文件要走 [`Self::rel_path`]。
    pub local_path: Option<String>,
    /// `local` 状态下这个文件在 agent 根下的相对路径（正斜杠）：界面按它打开文件
    /// （`open_media` 只收工作区相对路径）。`cached` 的副本在同步缓存里，只能经
    /// 「存进工作区」取用。
    pub rel_path: Option<String>,
    /// 产生这条消息的设备的名字；本机产生的为空。
    pub origin: Option<String>,
    /// 消息里写的那个路径（[`Self::path`]）在本机工作区里是否真的原样存在。同内容的
    /// 多个落点里，本机可能只有其中一个：别名补到消息路径上之后这里就变成 true，
    /// 界面按工作区路径出卡（图片/视频照常内联），不必再让用户「存进工作区」。
    pub path_present: bool,
    /// 这次清单里 `path_present` / `changed` 是不是核对出来的。false 表示这一条没轮到
    /// 内容核对（大文件、老消息，见 [`VERIFY_BUDGET`]）：界面别把它当命中，点开之前
    /// 另有一次按需核对（`sync_open_attachment`）。
    pub checked: bool,
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
    /// 本机登记被回收过、留下 `BLOB_GONE` 记录的：原来在这台设备上，现在拿不出来
    /// （文件被删/改过；放回原处后别的设备来要时会重新上传）。
    was_mine: bool,
    origin: Option<String>,
}

pub(crate) fn attachments(
    ctx: &Ctx,
    keys: Option<&Keys>,
    channel: &str,
    chat_id: &str,
    blob_filter: Option<&str>,
) -> Result<Vec<AttachmentInfo>> {
    // 旧消息的附件索引可能只有一半（那时还没有 artifact 记录）：打开会话时把能准确
    // 重建的补上，否则清单里会缺掉那些工具结果。补不成不算错，清单照常给。
    if let Err(error) = crate::wire::backfill_artifact_refs(&ctx.env, channel, chat_id) {
        tracing::debug!("补附件索引失败（清单照常）: {error}");
    }
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
        let lookup = |blob: &str| -> Result<(Option<BlobRow>, &'static str, bool)> {
            let row = db::blob_get(conn, blob)?;
            let remote = match &row {
                Some(row) if row.state == BLOB_REMOTE => attach::remote_state(conn, blob, now)?,
                _ => "remote",
            };
            // 没有登记、却留着「原本在这台设备上、后来拿不出来」的记录：照实说，
            // 别给一个必然失败的「重新下载」。
            let was_mine = row.is_none() && db::state_get(conn, scope::BLOB_GONE, blob)?.is_some();
            Ok((row, remote, was_mine))
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
                if blob_filter.is_some_and(|want| file.blob != want) {
                    continue;
                }
                let (row, remote, was_mine) = lookup(&file.blob)?;
                out.push(FoundRef {
                    message_id,
                    kind: "file",
                    row,
                    remote,
                    was_mine,
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
                if blob_filter.is_some_and(|want| blob != want) {
                    continue;
                }
                let (row, remote, was_mine) = lookup(&blob)?;
                out.push(FoundRef {
                    message_id,
                    kind: "artifact",
                    row,
                    remote,
                    was_mine,
                    path: artifact.name,
                    blob,
                    size: artifact.size,
                    origin: origin.clone(),
                });
            }
        }
        Ok(out)
    })?;
    // 内容核对要整读文件，可能很贵：从最新的一条消息往前排，整个会话一次最多读这么
    // 多字节。落在预算外面的（都在界面上看不到的老消息里）只按存在性说——打开会话、
    // 媒体卡可用不该等一场整库大小的顺序读。
    const VERIFY_BUDGET: u64 = 128 * 1024 * 1024;
    let mut budget = VERIFY_BUDGET;
    let mut spent = 0u64;
    let mut verify = vec![false; found.len()];
    for (index, item) in found.iter().enumerate().rev() {
        if item.kind != "file" {
            continue;
        }
        // 成本按「这一条要读多少」算：摘要缓存里已经有一份新鲜的就不要钱（同一次会话
        // 里反复重取清单时，后面几轮会把预算让给还没核对过的文件）。
        let cost = verification_cost(ctx, item);
        // 这一轮还没真读过东西时，单个再大的文件也核一次（不会被预算永远挡在外面）；
        // 已经读过别的文件了，就等下一轮——缓存命中的零成本条目不算读过，不会把大文件
        // 一直饿死。
        if cost > budget && spent > 0 {
            continue;
        }
        budget = budget.saturating_sub(cost);
        spent = spent.saturating_add(cost);
        verify[index] = true;
    }
    // 文件在不在、还是不是登记的那份内容：库锁之外看。路径先过 `check_path`（工作区里
    // 的链接不能把读取带到别处），内容核对按（路径、长度、mtime）记忆摘要。
    let list: Vec<AttachmentInfo> = found
        .into_iter()
        .enumerate()
        .map(|(index, found)| {
            let deep = verify[index];
            // 只认原样存在的文件：名字要改写才拼得上的路径（别的系统上的文件名），本机
            // 工作区里不会有它；中间的目录是链接的也不算。
            //
            // 落点（登记、同步缓存）按本机的改写规则解析（`checked_path`），写下的就是
            // 那个名字；消息里写的路径必须原样存在（`resolve_exact`）——界面打开时用的
            // 是消息里那一串，改写过名字的文件在界面上打不开（「存进工作区」会给出存好
            // 的那个路径）。
            // 落点的三种情况：文件在、确认不在、暂时读不了（权限/IO）——最后一种
            // 不能当「不在」（会误撤保存记录、预览拿不到可重试错误）。
            let landing = |rel_path: &str| -> Landing {
                match ctx.env.checked_path_typed(rel_path) {
                    Err(crate::wire::LinkCheckError::Policy) => Landing::Absent,
                    Err(crate::wire::LinkCheckError::Io(_)) => Landing::Unknown,
                    Ok(path) => match std::fs::symlink_metadata(&path) {
                        Ok(meta) if meta.is_file() => Landing::File(path),
                        Ok(_) => Landing::Absent,
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                            ) =>
                        {
                            Landing::Absent
                        }
                        Err(_) => Landing::Unknown,
                    },
                }
            };

            // 这个落点上的文件是不是就是这条消息登记的那份内容。没有密钥（不在同步空间
            // 里、密钥读不出来）时验不了：按登记说它在（老行为）。
            let verified = |path: &Path| -> BlobMatch {
                match keys {
                    Some(keys) => same_blob_status(keys, path, &found.blob, found.size),
                    None => BlobMatch::Yes,
                }
            };
            // 消息里写的那个路径自身的三态结论 + 路径（不能复用落点结论：落点可能
            // 是同步缓存；消息路径上的文件读不了时也要按 Unknown 处理）。
            let exact_status = |rel: &str, deep: bool| -> (BlobMatch, Option<std::path::PathBuf>) {
                let Some(path) = ctx.env.resolve_exact(rel) else {
                    return (BlobMatch::No, None);
                };
                match ctx.env.check_path_typed(rel, &path) {
                    Err(crate::wire::LinkCheckError::Policy) => return (BlobMatch::No, None),
                    Err(crate::wire::LinkCheckError::Io(_)) => {
                        return (BlobMatch::Unknown, Some(path))
                    }
                    Ok(()) => {}
                }
                match std::fs::symlink_metadata(&path) {
                    // 预算外（!deep）只探测存在性：不整读旧附件。
                    Ok(meta) if meta.is_file() => {
                        let match_state = if deep {
                            verified(&path)
                        } else {
                            BlobMatch::Unknown
                        };
                        (match_state, Some(path))
                    }
                    Ok(_) => (BlobMatch::No, None),
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        (BlobMatch::No, None)
                    }
                    Err(_) => (BlobMatch::Unknown, Some(path)),
                }
            };
            let (state, local_path, rel_path) = match &found.row {
                Some(row) if row.state == BLOB_REMOTE => (found.remote, None, None),
                Some(row) => match landing(&row.rel_path) {
                    // 落点上的文件已经不是这个对象（被就地改写过）：不能当它还在，
                    // 界面上给「重新下载」（`attach::lookup` 也按内容核对，取回时不会
                    // 覆盖这份内容不同的文件）。只有这一条应用了核对才算——预算没轮到的
                    // 只是「没核对过」，不是「内容对不上」。
                    Landing::File(path)
                        if deep
                            && found.kind == "file"
                            && matches!(verified(&path), BlobMatch::No) =>
                    {
                        ("changed", None, None)
                    }
                    Landing::File(path) if row.rel_path.starts_with(SYNC_DIR_PREFIX) => {
                        ("cached", Some(path), None)
                    }
                    Landing::File(path) => ("local", Some(path), Some(row.rel_path.clone())),
                    Landing::Absent => ("missing", None, None),
                    // 落点暂时读不了：登记状态与落点都保留——卡片按登记的路径重试
                    // （打开时会再核对一次），预览/保存拿不到 local_path 时给「暂时
                    // 不可访问」的可重试错误，而不是「没下载/不在缓存」。
                    Landing::Unknown => {
                        let state = if row.rel_path.starts_with(SYNC_DIR_PREFIX) {
                            "cached"
                        } else {
                            "local"
                        };
                        (state, None, Some(row.rel_path.clone()))
                    }
                },
                // 没有登记（登记被回收了）：工作区里原来的位置上还有、内容也对得上就算
                // 本机有——文件被放回原处之后不该还按「取不回来」说。
                None if found.kind == "file" => match exact_status(&found.path, deep) {
                    // 核对过、内容对不上：按「已改写」处理。
                    (BlobMatch::No, Some(_)) if deep => ("changed", None, None),
                    // 在（或暂时读不了/核不了）：按本机有处理；读不了的 `checked=false`
                    // 让界面别拿它撤保存记录。
                    (_, Some(path)) => ("local", Some(path), Some(found.path.clone())),
                    // 文件没了、又留着「原本在这台设备上」的记录：照实说取不回来。
                    (_, None) if found.was_mine => ("gone", None, None),
                    (_, None) => ("missing", None, None),
                },
                // 工具结果全文：登记被回收过、留下记录的，照实说取不回来。
                None if found.was_mine => ("gone", None, None),
                None => ("missing", None, None),
            };
            // 这一轮的核对结论（`Yes`/`No`/`Unknown`）：落点文件读不了时是 Unknown，
            // 不能当成「内容对不上」。只对预算内的条目算（`deep`）：预算外的不整读。
            let outcome = if deep {
                local_path.as_deref().map(&verified)
            } else {
                None
            };

            let message_status = if found.kind == "file" && deep {
                Some(
                    if state == "local" && rel_path.as_deref() == Some(found.path.as_str()) {
                        outcome.unwrap_or(BlobMatch::Unknown)
                    } else if keys.is_none() {
                        // 没有密钥（不在同步空间里）验不了内容：文件不在是 No，文件在
                        // 但核不了是 Unknown（不能当成「没了」）。
                        match ctx.env.resolve_exact(&found.path) {
                            Some(path) => match std::fs::symlink_metadata(&path) {
                                Ok(meta) if meta.is_file() => BlobMatch::Unknown,
                                Ok(_) => BlobMatch::No,
                                Err(e)
                                    if matches!(
                                        e.kind(),
                                        std::io::ErrorKind::NotFound
                                            | std::io::ErrorKind::NotADirectory
                                    ) =>
                                {
                                    BlobMatch::No
                                }
                                Err(_) => BlobMatch::Unknown,
                            },
                            None => BlobMatch::No,
                        }
                    } else {
                        exact_status(&found.path, true).0
                    },
                )
            } else {
                None
            };
            let local_path = local_path.map(|path| path.to_string_lossy().into_owned());
            // 消息里写的那个路径上现在是不是真有这份内容：登记说它就是落点、内容上面
            // 已经核对过的（`local`），或者那个路径上的文件核对下来就是这个对象。
            // 工具结果（artifact）在消息里只有一个文件名，按状态算。
            let path_present = if found.kind == "file" {
                matches!(message_status, Some(BlobMatch::Yes))
            } else {
                state == "local"
            };
            AttachmentInfo {
                message_id: found.message_id,
                kind: found.kind,
                path: found.path,
                blob: found.blob,
                size: found.size,
                state,
                local_path,
                rel_path,
                origin: found.origin,
                path_present,
                // 读不了/核不了不是「核对结论」：checked=false 让界面按「还没核对」
                // 处理（别拿它撤有效的保存记录）。
                // 核对结论只看消息路径：同步缓存暂时读不了不影响「消息路径上有没有
                // 这份内容」这个判断（缓存状态仍由 state 表达）。
                checked: found.kind != "file"
                    || (deep && !matches!(message_status, Some(BlobMatch::Unknown))),
            }
        })
        .collect();
    Ok(list)
}

/// 核对「某个路径上的文件就是登记的那个对象」时算过的摘要：按文件版本（路径、长度、
/// 修改时间、创建时间、类 Unix 上的 ctime）记住。打开会话、每次同步动静之后
/// [`attachments`] 都会被调用一遍，同一个版本不重复整读。
///
/// 容量按「一个会话里可能引用的文件数」留足，命中挪到队尾（LRU）：按 128 条留的时候，
/// 一个引用过上百个文件的会话每轮都会把彼此挤掉、每次清单查询都把这些文件重读一遍。
struct DigestEntry {
    path: String,
    version: FileVersion,
    digest: [u8; 32],
    /// 读完的时刻：「就是它」这个结论放久了要重新确认。
    at: Instant,
}

/// 文件版本：任何一项变了都重新整读。修改时间读不出来时判断不了它有没有变，不记。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileVersion {
    size: u64,
    mtime: i128,
    created: Option<i128>,
    /// 类 Unix 上的 ctime：用户态改不了，写入、改名、utime 都会让它变——备份还原
    /// 保留 mtime 的那类替换靠它发现。（Windows 上没有对应的稳定接口，只能靠
    /// [`DIGEST_TRUST`] 的重新确认。）
    changed: Option<i128>,
}

static DIGEST_CACHE: Mutex<Vec<DigestEntry>> = Mutex::new(Vec::new());
const DIGEST_CACHE_MAX: usize = 8192;
/// 「内容就是它」的结论最多信这么久。时间戳被还原过的替换（还原工具会保留长度、
/// mtime、创建时间）只能靠重新整读发现；元数据没变的版本每这么久重新确认一次。
pub(crate) const DIGEST_TRUST: Duration = Duration::from_secs(60);

fn timestamp(time: std::io::Result<SystemTime>) -> Option<i128> {
    time.ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_nanos() as i128)
}

/// 文件「变更时间」：用户态改不了、每次写入都会变。
///
/// - 类 Unix：ctime（写入、改名、utime 都会更新）。
/// - Windows：ChangeTime（`FILE_BASIC_INFO`）。这里不能用 mtime/创建时间：实测同一秒
///   内的同长度快速覆盖，两者的 100ns 值经常一个字节都不变（Windows 对目录项时间戳
///   是懒更新的），摘要缓存会把新内容当成旧内容。
#[cfg(unix)]
fn changed_stamp(file: Option<&std::fs::File>, meta: &std::fs::Metadata) -> Option<i128> {
    let _ = file;
    use std::os::unix::fs::MetadataExt;
    Some(meta.ctime() as i128 * 1_000_000_000 + meta.ctime_nsec() as i128)
}

#[cfg(windows)]
fn changed_stamp(file: Option<&std::fs::File>, _meta: &std::fs::Metadata) -> Option<i128> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileBasicInfo, GetFileInformationByHandleEx, FILE_BASIC_INFO,
    };
    let file = file?;
    let mut info = std::mem::MaybeUninit::<FILE_BASIC_INFO>::zeroed();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as _,
            FileBasicInfo,
            info.as_mut_ptr() as _,
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    let info = unsafe { info.assume_init() };
    Some(info.ChangeTime as i128)
}

#[cfg(not(any(unix, windows)))]
fn changed_stamp(_file: Option<&std::fs::File>, _meta: &std::fs::Metadata) -> Option<i128> {
    None
}

fn version_with(file: Option<&std::fs::File>, meta: &std::fs::Metadata) -> Option<FileVersion> {
    Some(FileVersion {
        size: meta.len(),
        mtime: timestamp(meta.modified())?,
        created: timestamp(meta.created()),
        changed: changed_stamp(file, meta),
    })
}

/// 这个文件的版本标识；修改时间读不出来（判不了有没有变）时返回 `None`。
///
/// 能打开就把字段全部取自这个句柄（Windows 的变更时间本来就只能从句柄取）：两次 stat
/// 之间文件被原子替换时，也不会拼出一个「长度来自旧文件、变更时间来自新文件」的混合
/// 版本。打不开（权限之类）时退回调用方给的那一份元数据。
pub(crate) fn version_of(path: &Path, meta: &std::fs::Metadata) -> Option<FileVersion> {
    if let Ok(file) = std::fs::File::open(path) {
        if let Ok(own) = file.metadata() {
            return version_with(Some(&file), &own);
        }
    }
    version_with(None, meta)
}

/// 同一个文件的版本标识，但全部字段取自这个已经打开的句柄（流式发送时用：校验和
/// 发送必须是同一个文件）。
pub(crate) fn version_of_open(file: &std::fs::File) -> Option<FileVersion> {
    let meta = file.metadata().ok()?;
    version_with(Some(file), &meta)
}

/// 文件版本针：长度 + 修改时间 + 变更时间（拿得到的话）。流式入口用它钉住「核对过
/// 的那一份」——同长度、时间戳被保留的替换也会让针变。
pub(crate) fn file_version_pin(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    pin_of_version(&version_of_open(&file)?)
}

/// 同一个文件的版本针，字段全部取自这个已打开的句柄。
pub(crate) fn file_version_pin_of(file: &std::fs::File) -> Option<String> {
    pin_of_version(&version_of_open(file)?)
}

pub(crate) fn pin_of_version(version: &FileVersion) -> Option<String> {
    Some(format!(
        "{}-{}-{}",
        version.size,
        version.mtime,
        version
            .changed
            .map(|changed| changed.to_string())
            .unwrap_or_else(|| "x".to_string())
    ))
}

/// 登记长度允许的误差：长度是别的设备报来的，下载本身宽出约一个分块（见
/// `attach::download_max`）。超出这么多的文件不可能是这个对象，连读都不读。
pub(crate) const SIZE_SLACK: u64 = 1024 * 1024;

/// 读文件算摘要，最多读 `limit` 字节：路径上被换成一个超大文件时不能整读（清单在
/// 打开会话的关键路径上）。读满了上限还没到文件尾就返回 `None`——判不了它是哪个对象。
/// 读这个已经打开的句柄算摘要（版本与摘要出自同一份内容），最多读 `limit` 字节。
fn hash_file_digest_of(file: &mut std::fs::File, limit: u64) -> Option<[u8; 32]> {
    let mut reader = file.by_ref().take(limit);
    let (digest, read) = sha256_reader(&mut reader).ok()?;
    (read < limit || file.metadata().is_ok_and(|meta| meta.len() == limit)).then_some(digest)
}

/// 打开一个普通文件（`symlink_metadata` 先确认最后一段不是链接）。路径中间的链接由
/// 调用方的路径守卫（`check_path`）保证不存在。
fn open_regular(path: &Path) -> Option<std::fs::File> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    std::fs::File::open(path).ok()
}

/// 这个版本的摘要与「读完的时刻」。缓存命中不算读（不刷新时刻）。
pub(crate) fn cached_digest(path: &str, version: &FileVersion) -> Option<([u8; 32], Instant)> {
    let mut cache = DIGEST_CACHE.lock().ok()?;
    let at = cache
        .iter()
        .position(|entry| entry.path == path && entry.version == *version)?;
    let entry = cache.remove(at);
    let found = (entry.digest, entry.at);
    cache.push(entry);
    Some(found)
}

/// 整读一遍并记下这个版本的摘要（同一路径的旧版本被顶掉）。
///
/// 读的就是 `file` 这个已经打开、`version` 也是从它身上取的那一份：读完再对一次
/// 版本，变了就不记结论（读到的是哪一版说不清，下一次重新读）。
fn read_digest(
    path: &Path,
    file: &mut std::fs::File,
    version: &FileVersion,
    limit: u64,
) -> Option<[u8; 32]> {
    let digest = hash_file_digest_of(file, limit)?;
    let meta = file.metadata().ok()?;
    if version_with(Some(file), &meta)? != *version {
        return None;
    }
    if let Ok(mut cache) = DIGEST_CACHE.lock() {
        let key = path.to_string_lossy().into_owned();
        cache.retain(|entry| entry.path != key);
        if cache.len() >= DIGEST_CACHE_MAX {
            cache.remove(0);
        }
        cache.push(DigestEntry {
            path: key,
            version: *version,
            digest,
            at: Instant::now(),
        });
    }
    Some(digest)
}

/// 核对一条文件引用大概要读多少字节：落点和消息路径上存在的文件里，摘要缓存里没有
/// 这一版的新鲜结论的，按长度累计。已经核对过（缓存新鲜）的成本是 0——同一次会话里
/// 反复重取清单时，预算会一路让给还没核对过的文件。
fn verification_cost(ctx: &Ctx, found: &FoundRef) -> u64 {
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    if let Some(row) = &found.row {
        if let Ok(path) = ctx.env.checked_path(&row.rel_path) {
            if path.is_file() {
                paths.push(path);
            }
        }
    }
    if let Some(path) = ctx.env.resolve_exact(&found.path) {
        if path.is_file() && !paths.iter().any(|known| known == &path) {
            paths.push(path);
        }
    }
    let mut cost = 0u64;
    for path in paths {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        // 长度与登记差太多：`same_blob` 一看长度就否，不用读，也不该占预算（否则一个
        // 被换成超大文件的旧引用每轮都把预算吃光）。
        if meta.len().abs_diff(found.size) > SIZE_SLACK {
            continue;
        }
        let Some(version) = version_of(&path, &meta) else {
            cost = cost.saturating_add(meta.len());
            continue;
        };
        let key = path.to_string_lossy().into_owned();
        let fresh =
            cached_digest(&key, &version).is_some_and(|(_, at)| at.elapsed() < DIGEST_TRUST);
        if !fresh {
            cost = cost.saturating_add(meta.len());
        }
    }
    cost
}

/// 登记落点的三种情况（见 `attachments()` 的 `landing`）。
enum Landing {
    /// 文件就在这个落点上。
    File(std::path::PathBuf),
    /// 确认不在（不存在、不是文件、路径策略拒绝）。
    Absent,
    /// 暂时读不了（权限/IO）：不能当成「不在」。
    Unknown,
}

/// 点开前的一次明确核对：不用摘要缓存里「刚算过」的结论，直接读一遍当前内容。
/// （时间戳被还原过的替换在缓存的 60 秒窗口里看不出来；这是用户点开的那一下，读一次
/// 文件是值得的。）
pub(crate) fn same_blob_now(keys: &Keys, path: &Path, blob: &str, size: u64) -> bool {
    matches!(same_blob_now_status(keys, path, blob, size), BlobMatch::Yes)
}

/// [`same_blob_now`] 的三态版本：读不了/判不了是 `Unknown`，不是「内容对不上」。
pub(crate) fn same_blob_now_status(keys: &Keys, path: &Path, blob: &str, size: u64) -> BlobMatch {
    let Some(mut file) = open_regular(path) else {
        return BlobMatch::Unknown;
    };
    let Ok(meta) = file.metadata() else {
        return BlobMatch::Unknown;
    };
    if meta.len().abs_diff(size) > SIZE_SLACK {
        return BlobMatch::No;
    }
    let limit = size.saturating_add(SIZE_SLACK);
    let digest = match version_with(Some(&file), &meta) {
        Some(version) => read_digest(path, &mut file, &version, limit),
        None => hash_file_digest_of(&mut file, limit),
    };
    match digest {
        Some(digest) if keys.blob_id(&digest) == blob => BlobMatch::Yes,
        Some(_) => BlobMatch::No,
        None => BlobMatch::Unknown,
    }
}

/// `path` 上的文件是不是 `blob` 这个对象。
///
/// 以内容摘要为准（长度是别的设备报来的，可能差一点；下载本身也只认摘要）。缓存里
/// 的结论（对得上或对不上都算）只在 [`DIGEST_TRUST`] 之内直接用：同一份清单在一次
/// 同步动静之后会被反复查询，每次都整读一遍上百个文件不可接受；而时间戳被还原过的
/// 替换只能靠过期后的重新整读发现，所以两个方向都设了期限——还原成正确内容之后，
/// 卡上最多一分钟就会自己变回可用。
pub(crate) fn same_blob(keys: &Keys, path: &Path, blob: &str, size: u64) -> bool {
    matches!(same_blob_status(keys, path, blob, size), BlobMatch::Yes)
}

/// 一次内容核对的三态结论：`Unknown` 表示「这一下读不了/判不了」（权限、I/O、
/// 判不了版本），不是「内容对不上」——调用方不能拿它当缺失。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlobMatch {
    Yes,
    No,
    Unknown,
}

pub(crate) fn same_blob_status(keys: &Keys, path: &Path, blob: &str, size: u64) -> BlobMatch {
    // 版本字段与摘要都取自同一个句柄：先 stat 路径、再按路径打开读取时，两次之间
    // 文件被原子替换会把「旧版本 + 新内容」配成一条缓存结论。
    let Some(mut file) = open_regular(path) else {
        return BlobMatch::Unknown;
    };
    let Ok(meta) = file.metadata() else {
        return BlobMatch::Unknown;
    };
    // 长度差太多的（路径上换成了一个更大的文件之类）不可能是这个对象：连读都不读，
    // 不能让一个超大文件拖住整份清单。
    if meta.len().abs_diff(size) > SIZE_SLACK {
        return BlobMatch::No;
    }
    let limit = size.saturating_add(SIZE_SLACK);
    let Some(version) = version_with(Some(&file), &meta) else {
        // 判不了有没有变：直接读一遍，不记结论。
        return match hash_file_digest_of(&mut file, limit) {
            Some(digest) if keys.blob_id(&digest) == blob => BlobMatch::Yes,
            Some(_) => BlobMatch::No,
            None => BlobMatch::Unknown,
        };
    };
    let key = path.to_string_lossy().into_owned();
    if let Some((digest, at)) = cached_digest(&key, &version) {
        if at.elapsed() < DIGEST_TRUST {
            return if keys.blob_id(&digest) == blob {
                BlobMatch::Yes
            } else {
                BlobMatch::No
            };
        }
    }
    match read_digest(path, &mut file, &version, limit) {
        Some(digest) if keys.blob_id(&digest) == blob => BlobMatch::Yes,
        Some(_) => BlobMatch::No,
        None => BlobMatch::Unknown,
    }
}

/// 一条消息引用里这个对象的路径与长度（`path` 是消息里写的那一串；artifact 是文件名）
/// 和它是不是工具结果全文。登记被回收之后（上传失败留下的 `BLOB_GONE` 记录那种），
/// 「点开前核对」和「重新下载」都拿它当依据。
pub(crate) fn referenced_attachment(
    ctx: &Ctx,
    channel: &str,
    chat_id: &str,
    blob: &str,
) -> Option<(String, u64, bool)> {
    db::read(ctx.db(), |conn| {
        if !sync_db::sync_tables_exist(conn)? {
            return Ok(None);
        }
        let Some(session) = map::session_map_by_key(conn, channel, chat_id)? else {
            return Ok(None);
        };
        let rows: Vec<String> = {
            let mut stmt = conn.prepare_cached(
                "SELECT refs FROM sync_msg WHERE session_id = ?1                  AND refs IS NOT NULL AND refs != '{}'",
            )?;
            let rows = stmt.query_map(params![session.session_id], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for column in rows {
            let Some(refs) = StoredRefs::parse(Some(&column)) else {
                continue;
            };
            if let Some(file) = refs.files.into_iter().find(|file| file.blob == blob) {
                return Ok(Some((file.path, file.size, false)));
            }
            if let Some(artifact) = refs
                .artifacts
                .into_iter()
                .find(|artifact| artifact.blob.as_deref() == Some(blob))
            {
                return Ok(Some((artifact.name, artifact.size, true)));
            }
        }
        Ok(None)
    })
    .ok()
    .flatten()
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
        // 默认范围之外再加附件（不是换掉默认范围）。
        let mut scopes = SyncConfig::default().scopes;
        scopes.push("attachments".into());
        let config = SyncConfig {
            enabled: true,
            scopes,
            ..Default::default()
        };
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
        let got = attachments(ctx, None, "desktop", "c1", None).unwrap();
        let states: Vec<&str> = got.iter().map(|item| item.state).collect();
        assert_eq!(states, ["local", "cached", "remote"]);
        // 消息路径上真有这份内容的只有第一个：cached 的那份在同步缓存里，消息写的
        // 原位置（output/b.txt）本机没有；remote 的哪个位置都没有。
        let at_path: Vec<bool> = got.iter().map(|item| item.path_present).collect();
        assert_eq!(at_path, [true, false, false]);
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
        assert!(attachments(ctx, None, "desktop", "nope", None)
            .unwrap()
            .is_empty());

        // 去取过、同步空间里没有的：照它现在的情况说（§8.7）。
        let (hour, day) = (3600 * 1000, 24 * 3600 * 1000);
        let remote = || attachments(ctx, None, "desktop", "c1", None).unwrap()[2].state;
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
