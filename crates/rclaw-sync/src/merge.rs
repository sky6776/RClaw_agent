//! 合入（§7.2–§7.5、§9.3 第 9 步）：把收件箱里的 op 按规则落到本机。
//!
//! 每条 op 一个短事务：它的效果、它在收件箱里的去处、这台设备日志的「已处理」位置一起
//! 提交。事务里只做数据库读写——artifact 全文在事务之前写好，要复制的附件在提交之后补
//! （库锁是会话读写共用的，不能在里面做磁盘写入）。
//!
//! 一条远端消息落到哪里（§7.2）：
//!
//! 1. 本机这个线程里已经有它 → 跳过；
//! 2. 没有 `parent`（线程的第一条）→ 按 §6 新建一个本地会话；
//! 3. 某个本地会话的末条正是 `parent` → 追加；
//! 4. `parent` 在某个本地会话里但不是末条 → 自动分支：原始前缀复制到新会话，再追加；
//! 5. 本机没有 `parent` → 悬空，留在收件箱，不挡住这个线程的其他 op。
//!
//! 线程在本机的任何一个会话正忙（有会改历史的活动 turn）时，这个线程新到的 op 按到达
//! 顺序排队，等它空下来再处理。校验不通过的 op 不落库，所属线程从这一条起隔离。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use rclaw_store::session::raw::{self, SessionDeleteSync};
use rclaw_store::sync_db::{
    self, INBOX_ORPHAN, INBOX_PENDING, INBOX_QUARANTINED, INBOX_QUEUED, SEAL_LOCAL, SEAL_SEALED,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::attach::{self, now_ms};
use crate::bundle;
use crate::crypto::Keys;
use crate::db::{self, scope, DeviceInfo, BLOB_HELD, BLOB_PENDING};
use crate::error::{Result, SyncError};
use crate::files::{self, Dirty};
use crate::ids;
use crate::map::{self, OpenInfo, SessionMap};
use crate::op::{ForkOf, Op, OpLine, WireMsg, ARTIFACT_INLINE_MAX};
use crate::portable;
use crate::publish::{self, Settings};
use crate::pull::{self, InSeg};
use crate::snapshot;
use crate::status;
use crate::trash;
use crate::wire::{self, ArtifactPolicy, Landing, StoredRefs, OP_ARTIFACTS_MAX};
use crate::{ChatKey, Ctx};

/// 段头里声明的依赖最多等这么久（依赖只用来优化顺序：等不到就照常处理）。
const DEPS_WAIT_MS: i64 = 10 * 60 * 1000;
/// 同一条悬空链的 `session.want` 在这段时间内不重发。
const WANT_REPEAT_MS: i64 = 24 * 3600 * 1000;
/// 悬空的 op 保留这么久，到期仍没有接上就丢弃。
const ORPHAN_KEEP_MS: i64 = 30 * 24 * 3600 * 1000;
/// 对一个线程的占用最多连续持有这么久；到点就放开再重新占，等着的 turn 得以开始。
const HOLD_MAX: Duration = Duration::from_millis(200);
/// 一条消息的「看去向 → 准备文件 → 提交」最多试几次（去向在准备期间变了就重来）。
const MSG_ATTEMPTS: usize = 3;
/// 一条 op 连续这么多轮合入失败，就不再让它堵着那台设备的日志。
const ROW_FAIL_MAX: u32 = 5;
/// 分支重发（以及全量快照）里每个会话包的大小（字节）。
pub(crate) const BUNDLE_CHUNK: usize = 2 * 1024 * 1024;

/// 日志因为不认识的 op 类型停住时，`PeerState::halted` 的开头。
pub const HALT_UNKNOWN_OP: &str = "不认识的操作类型";
/// 收到的 `session.want`，等这一轮合入结束后应答：键 `<sid>|<gid>`。
pub const SCOPE_WANT_TODO: &str = "want-todo";
/// 末条命中了墓碑、但当时正忙没删成的线程：键 `<sid>`。
const SCOPE_TOMB_TODO: &str = "tomb-todo";
/// 有分支的末条落在某条墓碑的链上（却不是墓碑自己的末条）的线程，等各设备的日志读到头
/// 再清点（[`sweep_stale`]）：键 `<sid>`。
const SCOPE_STALE_TODO: &str = "stale-todo";
/// 清点一个线程时最多看它的这么多条链。一个线程被删掉的分支通常只有一两条；封个顶是
/// 为了不让一个线程把库锁占住很久。
const CHAINS_PER_THREAD: usize = 64;

/// 一轮合入的结果。
#[derive(Debug, Default)]
pub struct MergeReport {
    /// 这一轮里有了去处的 op 数（合入、跳过、排队、悬空、隔离）。
    pub applied: usize,
    /// 内容或属性被改动（或被删除）的会话。
    pub chats: BTreeSet<ChatKey>,
    /// 会话列表本身变了（新增、删除、改名、归档）。
    pub list_changed: bool,
    /// 文件类数据（记忆、人格、技能……）有新的远端变更要落到本机文件。
    pub dirty: Dirty,
    /// 有设备声明它取代了本机当前的身份：本机也要换身份（§9.6）。
    pub rotate: bool,
    /// 设备清单或待处理事项可能变了。
    pub status_changed: bool,
    /// 还有 op 因为线程正忙在排队：turn 结束后要再来一轮。
    pub queued: bool,
}

/// 跨轮保留的一点记忆（不落库）。
#[derive(Debug, Default)]
pub struct Memo {
    /// 收件箱行 id → 连续合入失败的轮数。
    failures: HashMap<i64, u32>,
}

/// 被隔离的线程（`sync_state` 的 `quarantine`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quarantine {
    pub reason: String,
    /// 那条不合规的 op 来自哪台设备的日志。
    pub origin: String,
    pub at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WantTodo {
    /// 被点名应答的设备。
    to: String,
    /// 发出请求的设备。
    from: String,
}

// ---------------------------------------------------------------------------
// 收件箱
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct InboxRow {
    id: i64,
    origin: String,
    seq: u64,
    hlc: String,
    op: String,
    state: i64,
}

fn load_row(conn: &Connection, id: i64) -> Result<Option<InboxRow>> {
    Ok(conn
        .query_row(
            "SELECT id, origin, seq, hlc, op, state FROM sync_inbox WHERE id = ?1",
            params![id],
            |row| {
                Ok(InboxRow {
                    id: row.get(0)?,
                    origin: row.get(1)?,
                    seq: row.get::<_, i64>(2)?.max(0) as u64,
                    hlc: row.get(3)?,
                    op: row.get(4)?,
                    state: row.get(5)?,
                })
            },
        )
        .optional()?)
}

/// 一个线程在某个状态下的 op，按到达顺序。
fn thread_rows(conn: &Connection, sid: &str, state: i64) -> Result<Vec<i64>> {
    let mut stmt =
        conn.prepare_cached("SELECT id FROM sync_inbox WHERE sid = ?1 AND state = ?2 ORDER BY id")?;
    let rows = stmt.query_map(params![sid, state], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<i64>>>()?)
}

/// 收件箱里处于某个状态的 op 涉及哪些线程。
fn threads_in_state(conn: &Connection, state: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT sid FROM sync_inbox WHERE state = ?1 AND sid IS NOT NULL ORDER BY sid",
    )?;
    let rows = stmt.query_map(params![state], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
}

/// 推进一台设备日志的「已处理」位置；它的日志此前因为不认识的 op 停住的话，一并解除。
fn advance(conn: &Connection, origin: &str, seq: u64) -> Result<()> {
    let mut peer = db::peer_state(conn, origin)?;
    let forward = seq > peer.processed && seq <= peer.fetched;
    let unhalt = peer
        .halted
        .as_deref()
        .is_some_and(|reason| reason.starts_with(HALT_UNKNOWN_OP));
    if !forward && !unhalt {
        return Ok(());
    }
    if forward {
        peer.processed = seq;
    }
    if unhalt {
        peer.halted = None;
    }
    db::put_peer_state(conn, origin, &peer)
}

/// 一条 op 有了去处：移出收件箱（`state` 为空）或改成新状态，同时推进「已处理」。
fn settle(conn: &Connection, row: &InboxRow, state: Option<i64>) -> Result<()> {
    match state {
        None => conn.execute("DELETE FROM sync_inbox WHERE id = ?1", params![row.id])?,
        Some(state) => conn.execute(
            "UPDATE sync_inbox SET state = ?2 WHERE id = ?1",
            params![row.id, state],
        )?,
    };
    advance(conn, &row.origin, row.seq)
}

/// 隔离线程：记下原因（已经隔离的保留最早的原因），这条 op 留在收件箱。
fn quarantine(conn: &Connection, row: &InboxRow, sid: &str, reason: &str) -> Result<()> {
    if db::state_get(conn, scope::QUARANTINE, sid)?.is_none() {
        db::state_put_json(
            conn,
            scope::QUARANTINE,
            sid,
            db::HLC_ZERO,
            &Quarantine {
                reason: reason.to_string(),
                origin: row.origin.clone(),
                at_ms: now_ms(),
            },
        )?;
    }
    settle(conn, row, Some(INBOX_QUARANTINED))
}

/// 线程层面的拦截：被隔离的线程后续的 op 一律留在收件箱；线程还有更早到达、正在排队的
/// op 时，新的跟在它们后面。返回这条 op 应当停在的状态。
fn thread_hold(conn: &Connection, sid: &str, row: &InboxRow) -> Result<Option<i64>> {
    if db::state_get(conn, scope::QUARANTINE, sid)?.is_some() {
        return Ok(Some(INBOX_QUARANTINED));
    }
    let queued_before: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sync_inbox WHERE sid = ?1 AND state = ?2 AND id < ?3)",
        params![sid, INBOX_QUEUED, row.id],
        |row| row.get(0),
    )?;
    Ok(queued_before.then_some(INBOX_QUEUED))
}

/// 会话类的 op：它们只对「会话」这个同步范围有意义。
pub(crate) const SESSION_OPS: [&str; 6] = [
    "msg",
    "session.open",
    "session.set",
    "session.delete",
    "session.want",
    "session.bundle",
];

/// 线程 `sid` 是否不在本机的会话同步范围里：会话范围没开；或者它所在的通道（记在线程的
/// 开头信息里）不在本机参与同步的通道名单里。开头信息还没到时不算——照常等着，它到了
/// 再定。
///
/// 不在范围里的线程，它的 op 看过就算：不落地、不悬空、也不请别的设备重发。以后范围
/// 扩大时由引擎把各设备的日志从头再读一遍。
pub(crate) fn thread_out_of_scope(conn: &Connection, s: &Settings, sid: &str) -> Result<bool> {
    if !s.scopes.sessions {
        return Ok(true);
    }
    Ok(map::open_info(conn, sid)?
        .is_some_and(|open| !s.channels.iter().any(|channel| *channel == open.channel)))
}

/// 记下「这个线程因为通道不在名单里没有在本机落地」。有这种线程的设备手里的会话不全，
/// 不能写全量快照（§9.5）。范围扩大、重新读日志时引擎会把这些记录清掉。
fn note_skipped(conn: &Connection, sid: &str) -> Result<()> {
    if db::state_get(conn, scope::SKIP, sid)?.is_none() {
        db::state_put(conn, scope::SKIP, sid, db::HLC_ZERO, None, None)?;
    }
    Ok(())
}

/// 「本机丢过 op」的记号：`skip` 里一个不是线程 id 的键。
pub(crate) const LOSSY_KEY: &str = "!lossy";

/// 记下「本机丢弃过别的设备的 op」（内容不合规、悬空到期、用户放弃了被隔离的线程……）。
/// 丢过 op 的设备手里的数据可能不全——换一个更新的版本也许就认得那条 op——所以它和有
/// 跳过线程的设备一样不写全量快照（§9.5）。重新读日志时引擎把记号连同跳过记录一起清掉。
pub(crate) fn note_lossy(conn: &Connection) -> Result<()> {
    if db::state_get(conn, scope::SKIP, LOSSY_KEY)?.is_none() {
        db::state_put(conn, scope::SKIP, LOSSY_KEY, db::HLC_ZERO, None, None)?;
    }
    Ok(())
}

/// 这个段声明的依赖（「写这些 op 时我已经处理到别的设备日志的哪里」）是否都满足了。
fn deps_ready(conn: &Connection, s: &Settings, origin: &str, seg: &str, now: i64) -> Result<bool> {
    let Some(inseg) = db::state_get(conn, scope::INSEG, &pull::inseg_key(origin, seg))?
        .and_then(|row| row.json::<InSeg>())
    else {
        return Ok(true);
    };
    if (now - inseg.ts).abs() > DEPS_WAIT_MS {
        return Ok(true);
    }
    for (device, position) in &inseg.deps {
        if *position == 0 || device == origin || s.is_me(device) {
            continue;
        }
        let peer = db::peer_state(conn, device)?;
        if peer.processed >= *position {
            continue;
        }
        // 那台设备已经换了身份、它的日志本机读不下去、或者很久没出现过：不等。
        let unreachable = peer.replaced_by.is_some()
            || peer.halted.is_some()
            || (peer.seen_ms > 0 && !peer.is_active(now));
        if !unreachable {
            return Ok(false);
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// 一条消息的去向
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    /// 规则 1：本机这个线程里已经有这条消息。
    Skip,
    /// 规则 3：追加到末条正是 `parent` 的会话。
    Append {
        session_id: i64,
        channel: String,
        chat_id: String,
    },
    /// 规则 2：线程在本机还没有会话，新建一个。`landed` = 原来的键被别的线程占了，换了键。
    New {
        channel: String,
        chat_id: String,
        landed: bool,
    },
    /// 规则 4：`parent` 在会话 `from` 里（行 `upto`）但不是末条，从那里分出一条分支。
    Branch {
        from: i64,
        upto: i64,
        channel: String,
        source_chat: String,
        chat_id: String,
    },
    /// 规则 5：本机没有 `parent`（或线程的开头信息还没到）。
    Orphan,
    /// 不合规：所属线程从这条起隔离。
    Quarantine(String),
}

impl Plan {
    /// 是否会往本机会话里写消息。
    fn lands(&self) -> bool {
        matches!(
            self,
            Plan::Append { .. } | Plan::New { .. } | Plan::Branch { .. }
        )
    }
}

fn is_internal_chat(chat_id: &str) -> bool {
    chat_id.starts_with("bot-") || chat_id.starts_with("grp-")
}

/// 自动分支的会话键：`<原 chat_id>-b<4 位>`（§7.4），超长时截短原来的键。
fn branch_chat_id(conn: &Connection, channel: &str, source_chat: &str) -> Result<String> {
    for _ in 0..64 {
        let tail = format!("-b{}", ids::random_hex::<2>());
        let keep = 64usize.saturating_sub(tail.len());
        let base: String = source_chat.chars().take(keep).collect();
        let candidate = format!("{base}{tail}");
        if !bundle::key_taken(conn, channel, &candidate)? {
            return Ok(candidate);
        }
    }
    Err(SyncError::other("找不到可用的分支会话键"))
}

/// 按 §7.2 的规则看一条（已通过检查的）消息该去哪里。只读。`hint` 是上一次为分支挑好
/// 的会话键：还空着就沿用，已经为它准备好的文件才对得上。
fn plan_msg(conn: &Connection, sid: &str, msg: &WireMsg, hint: Option<&str>) -> Result<Plan> {
    if !map::find_gid(conn, sid, &msg.gid)?.is_empty() {
        return Ok(Plan::Skip);
    }
    let Some(parent) = msg.parent.as_deref() else {
        if !map::thread_sessions(conn, sid)?.is_empty() {
            return Ok(Plan::Quarantine("线程里出现了第二条开头的消息".into()));
        }
        // 开头信息（通道、会话键）还没到：先等着，它到了再落地。
        let Some(open) = map::open_info(conn, sid)? else {
            return Ok(Plan::Orphan);
        };
        if !bundle::key_taken(conn, &open.channel, &open.chat_id)? {
            return Ok(Plan::New {
                channel: open.channel,
                chat_id: open.chat_id,
                landed: false,
            });
        }
        // 键被别的线程占了（或者是本机标成「仅本机」的键）：落到
        // `<chat_id>-<来源设备 id 前 4 位>`（§6）。
        let suffix = open.origin.get(..4).unwrap_or("sync");
        let chat_id = bundle::free_chat_id(conn, &open.channel, &open.chat_id, suffix)?;
        return Ok(Plan::New {
            channel: open.channel,
            chat_id,
            landed: true,
        });
    };
    let holders = map::find_gid(conn, sid, parent)?;
    let Some((from, upto)) = holders.first().copied() else {
        return Ok(Plan::Orphan);
    };
    for (session_id, message_id) in &holders {
        if map::last_message_id(conn, *session_id)? == Some(*message_id) {
            let (channel, chat_id) = raw::session_key(conn, *session_id)?
                .ok_or_else(|| SyncError::other("会话不见了"))?;
            return Ok(Plan::Append {
                session_id: *session_id,
                channel,
                chat_id,
            });
        }
    }
    let (channel, source_chat) =
        raw::session_key(conn, from)?.ok_or_else(|| SyncError::other("会话不见了"))?;
    let chat_id = match hint {
        Some(hint) if !bundle::key_taken(conn, &channel, hint)? => hint.to_string(),
        _ => branch_chat_id(conn, &channel, &source_chat)?,
    };
    Ok(Plan::Branch {
        from,
        upto,
        channel,
        source_chat,
        chat_id,
    })
}

/// 落地前要占住哪些会话：线程在本机的全部会话，加上即将新建的会话键（占住它，本机就
/// 不会在合入的同时用这个键新开一个会话）。
fn gate_keys(conn: &Connection, sid: &str, plan: &Plan) -> Result<Vec<(String, String)>> {
    let mut keys: Vec<(String, String)> = map::thread_sessions(conn, sid)?
        .into_iter()
        .map(|session| (session.channel, session.chat_id))
        .collect();
    if let Plan::New {
        channel, chat_id, ..
    }
    | Plan::Branch {
        channel, chat_id, ..
    } = plan
    {
        let key = (channel.clone(), chat_id.clone());
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Ok(keys)
}

fn needs_files(plan: &Plan, msg: &WireMsg) -> bool {
    match plan {
        Plan::Branch { .. } => true,
        Plan::Append { .. } | Plan::New { .. } => !msg.artifacts.is_empty(),
        _ => false,
    }
}

/// 落地要用的文件：artifact 全文写进目标会话的目录；分支先把源会话的 artifact 目录复制
/// 过来。在事务之前做，重复执行结果相同。
fn prepare_files(ctx: &Ctx, plan: &Plan, msg: &WireMsg, keys: Option<&Keys>) -> Result<()> {
    match plan {
        Plan::Append {
            channel, chat_id, ..
        }
        | Plan::New {
            channel, chat_id, ..
        } => wire::write_artifacts(&ctx.env, channel, chat_id, msg, keys),
        Plan::Branch {
            channel,
            source_chat,
            chat_id,
            ..
        } => {
            ctx.env
                .sessions
                .copy_artifact_dir(channel, source_chat, chat_id);
            wire::write_artifacts(&ctx.env, channel, chat_id, msg, keys)
        }
        _ => Ok(()),
    }
}

/// 准备好的文件没用上（去向变了）：为新会话键建的 artifact 目录不属于任何会话，清掉。
/// 键已经被某个会话占了就不动。
fn discard_files(ctx: &Ctx, plan: &Plan) {
    let (channel, chat_id) = match plan {
        Plan::New {
            channel, chat_id, ..
        }
        | Plan::Branch {
            channel, chat_id, ..
        } => (channel, chat_id),
        _ => return,
    };
    let taken = db::read(ctx.db(), |conn| {
        Ok(raw::find_session(conn, channel, chat_id)?.is_some())
    })
    .unwrap_or(true);
    if !taken {
        let _ = std::fs::remove_dir_all(ctx.env.sessions.artifacts_dir(channel, chat_id));
    }
}

/// 落地之后的情况。
#[derive(Debug, Default)]
struct Effect {
    /// 消息落进了哪个会话。
    session: Option<(String, String)>,
    /// 新建了会话。
    created: bool,
    /// 会话的标题、归档或目标变了。
    attrs_changed: bool,
}

/// 有没有设在这条消息上的属性（`session.set` 先到、消息后到的情况）。
fn attrs_at(conn: &Connection, sid: &str, gid: &str) -> Result<bool> {
    Ok(!db::state_scan(conn, scope::ATTR, &format!("{sid}|{gid}|"))?.is_empty())
}

/// 自动分支：把前缀各行的身份映射也复制过来（同样的 `gid`）。这些行别的设备要么已经
/// 见过、要么会从源会话那边发出去，分支这边不再发布。
fn copy_msg_maps(
    conn: &Connection,
    ctx: &Ctx,
    dst: i64,
    channel: &str,
    chat_id: &str,
    pairs: &[(i64, i64)],
) -> Result<()> {
    let rel_dir = wire::artifact_rel_dir(channel, chat_id);
    for (old, new) in pairs {
        let Some(mapped) = map::msg_map(conn, *old)? else {
            continue;
        };
        map::insert_msg_map(
            conn,
            *new,
            dst,
            &mapped.gid,
            mapped.parent.as_deref(),
            SEAL_SEALED,
            mapped.refs.as_deref(),
            mapped.wire_meta.as_deref(),
        )?;
        // 全文还在别的设备上的 artifact：取回之后这条分支自己的目录里也要有一份。
        if let Some(stored) = StoredRefs::parse(mapped.refs.as_deref()) {
            for artifact in &stored.artifacts {
                if let Some(blob) = &artifact.blob {
                    let rel_path = format!("{rel_dir}{}", artifact.name);
                    wire::expect_blob(conn, &ctx.env, blob, &rel_path, artifact.size)?;
                }
            }
        }
    }
    Ok(())
}

/// 本机这个线程里有没有「只分配了身份、还没发布」的这条消息。
fn has_unpublished_copy(conn: &Connection, sid: &str, gid: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sync_msg m JOIN sync_session y ON y.session_id = m.session_id \
         WHERE m.gid = ?1 AND m.sealed = ?2 AND y.sid = ?3)",
        params![gid, SEAL_LOCAL, sid],
        |row| row.get(0),
    )?)
}

/// 别的设备已经发布过这条消息：本机那一行如果还只是「只分配了身份」，就算作已发布，不用
/// 再发一遍。重置同步空间、退出后重新加入时各设备手里是同一批消息，少了这一步每台设备
/// 都会把全部历史重新上传一遍。
fn adopt_published(conn: &Connection, sid: &str, gid: &str) -> Result<()> {
    conn.execute(
        "UPDATE sync_msg SET sealed = ?1 WHERE gid = ?2 AND sealed = ?3 AND session_id IN \
         (SELECT session_id FROM sync_session WHERE sid = ?4)",
        params![SEAL_SEALED, gid, SEAL_LOCAL, sid],
    )?;
    Ok(())
}

/// 在调用方的事务里执行一条消息的去向。
fn apply_plan(
    conn: &Connection,
    ctx: &Ctx,
    s: &Settings,
    sid: &str,
    msg: &WireMsg,
    plan: &Plan,
    keys: Option<&Keys>,
) -> Result<Effect> {
    let env = &ctx.env;
    match plan {
        Plan::Skip => {
            adopt_published(conn, sid, &msg.gid)?;
            Ok(Effect::default())
        }
        Plan::Orphan | Plan::Quarantine(_) => Ok(Effect::default()),
        Plan::Append {
            session_id,
            channel,
            chat_id,
        } => {
            let landing = Landing {
                session_id: *session_id,
                channel,
                chat_id,
                sealed: SEAL_SEALED,
            };
            wire::land_msg(conn, env, landing, msg, keys)?;
            let attrs_changed =
                attrs_at(conn, sid, &msg.gid)? && publish::refresh_attrs(conn, s, *session_id)?;
            Ok(Effect {
                session: Some((channel.clone(), chat_id.clone())),
                created: false,
                attrs_changed,
            })
        }
        Plan::New {
            channel,
            chat_id,
            landed,
        } => {
            let open = map::open_info(conn, sid)?
                .ok_or_else(|| SyncError::other("线程的开头信息不见了"))?;
            let created_at = open.created_at.clamp(0, raw::now_millis());
            let session_id = raw::create_session(conn, channel, chat_id, created_at)?;
            map::insert_session_map(conn, session_id, sid, &open.origin, SEAL_SEALED)?;
            let landing = Landing {
                session_id,
                channel,
                chat_id,
                sealed: SEAL_SEALED,
            };
            wire::land_msg(conn, env, landing, msg, keys)?;
            if *landed {
                // 展示用：「<标题> · <来源设备名>」。
                db::state_put(
                    conn,
                    scope::LANDED,
                    &session_id.to_string(),
                    db::HLC_ZERO,
                    None,
                    Some(&open.origin),
                )?;
            }
            bundle::apply_fork_lineage(conn, channel, chat_id, open.fork_of.as_ref())?;
            publish::refresh_attrs(conn, s, session_id)?;
            Ok(Effect {
                session: Some((channel.clone(), chat_id.clone())),
                created: true,
                attrs_changed: false,
            })
        }
        Plan::Branch {
            from,
            upto,
            channel,
            source_chat,
            chat_id,
        } => {
            let source = map::session_map(conn, *from)?
                .ok_or_else(|| SyncError::other("分支的源会话不见了"))?;
            let dst = raw::create_session(conn, channel, chat_id, raw::now_millis())?;
            let pairs = raw::copy_raw_prefix(
                conn,
                *from,
                *upto,
                dst,
                &env.sessions.artifacts_dir_prefix(channel, source_chat),
                &env.sessions.artifacts_dir_prefix(channel, chat_id),
            )?;
            copy_msg_maps(conn, ctx, dst, channel, chat_id, &pairs)?;
            map::insert_session_map(conn, dst, sid, &source.origin, SEAL_SEALED)?;
            map::set_tip(conn, dst, msg.parent.as_deref())?;
            let landing = Landing {
                session_id: dst,
                channel,
                chat_id,
                sealed: SEAL_SEALED,
            };
            wire::land_msg(conn, env, landing, msg, keys)?;
            // 展示用：「<标题>（<设备名>的分支）」，直到用户给它改名（§7.6）。
            let author = ids::gid_device(&msg.gid).unwrap_or_default();
            db::state_put(
                conn,
                scope::BRANCH,
                &dst.to_string(),
                db::HLC_ZERO,
                None,
                Some(author),
            )?;
            if let Some(open) = map::open_info(conn, sid)? {
                bundle::apply_fork_lineage(conn, channel, chat_id, open.fork_of.as_ref())?;
            }
            publish::refresh_attrs(conn, s, dst)?;
            Ok(Effect {
                session: Some((channel.clone(), chat_id.clone())),
                created: true,
                attrs_changed: false,
            })
        }
    }
}

/// 一条消息合入的结果。
#[derive(Debug)]
enum MsgOutcome {
    Landed,
    Skipped,
    Orphan,
    /// 线程正忙，排队。
    Queued,
    /// 不合规（附原因）。
    Quarantine(String),
    /// 这一轮没定下来，下一轮再试。
    Defer,
}

/// 会话真正的末条消息的 `gid`：`Some(None)` = 会话没有消息；`None` = 末条还没有身份
/// （本机刚写下、还没扫描到的行）。
fn true_tip(conn: &Connection, session_id: i64) -> Result<Option<Option<String>>> {
    match map::last_message_id(conn, session_id)? {
        None => Ok(Some(None)),
        Some(last) => Ok(map::gid_of(conn, session_id, last)?.map(Some)),
    }
}

/// 墓碑记的是不是「彻底删除」。
pub(crate) fn tomb_purge(row: &db::StateRow) -> bool {
    row.json::<serde_json::Value>()
        .and_then(|value| value.get("purge").and_then(|v| v.as_bool()))
        .unwrap_or(false)
}

fn tombstone(conn: &Connection, sid: &str, tip: Option<&str>) -> Result<Option<bool>> {
    Ok(
        db::state_get(conn, scope::TOMB, &sync_db::tombstone_key(sid, tip))?
            .map(|row| tomb_purge(&row)),
    )
}

/// 占住会话之后复核一次：占到之前这条分支可能又往下写了，或者已经不在这个线程里了。
/// 会话还在 `sid` 这个线程里、真实的末条上有生效的墓碑，才给出（重新读出的会话行，墓碑
/// 是不是彻底删除，这时真实的末条）。
fn recheck_tombstone(
    conn: &Connection,
    sid: &str,
    session_id: i64,
) -> Result<Option<(SessionMap, bool, Option<String>)>> {
    let session = map::session_map(conn, session_id)?;
    let Some(session) = session.filter(|session| session.sid == sid) else {
        return Ok(None);
    };
    let Some(tip) = true_tip(conn, session_id)? else {
        return Ok(None);
    };
    Ok(tombstone(conn, sid, tip.as_deref())?.map(|purge| (session, purge, tip)))
}

/// 线程 `sid` 的墓碑所带的链：`(墓碑的键, 链)`，至多 [`CHAINS_PER_THREAD`] 条。
fn thread_chains(conn: &Connection, sid: &str) -> Result<Vec<(String, String)>> {
    let lower = format!("{sid}/");
    let upper = format!("{sid}/\u{10ffff}");
    let mut stmt = conn.prepare_cached(
        "SELECT key, value FROM sync_state WHERE scope = ?1 AND key >= ?2 AND key < ?3 \
         AND value IS NOT NULL ORDER BY key LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        params![scope::TOMB_CHAIN, lower, upper, CHAINS_PER_THREAD as i64],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<(String, String)>>>()?)
}

/// `gid` 这条消息落在 `chains` 里哪条墓碑的链上：给出那条墓碑是不是彻底删除（落在几条
/// 上时有一条是就算）。一条都没落上返回 `None`。
fn chain_hit(conn: &Connection, chains: &[(String, String)], gid: &str) -> Result<Option<bool>> {
    let mut hit = None;
    for (key, chain) in chains {
        if !ids::chain_contains(chain, gid) {
            continue;
        }
        // 链是跟着墓碑走的；墓碑不在了它也不算数。
        let Some(row) = db::state_get(conn, scope::TOMB, key)? else {
            continue;
        };
        hit = Some(hit.unwrap_or(false) || tomb_purge(&row));
    }
    Ok(hit)
}

/// 线程 `sid` 在本机的分支里，哪些是「别处已经删掉的分支更早的样子」：末条落在某条墓碑
/// 的链上，却不是墓碑自己的末条（那种随到随删，见 `sweep_tombstones`）。
///
/// 链是被删的那条分支从开头到末条的各条消息。一个本地分支的末条在链上，说明它没有往下
/// 写过任何别的东西——它就是那条分支在某个更早时刻的样子，只是中间那段续写本机没有
/// （还没到，或者再也到不了：本机是从旧备份恢复的，或那段日志已经被回收）。在末条之后
/// 接着写过的分支，末条不在链上，不受影响（续写优先于删除）。还没有消息、或末条还没有
/// 身份（本机刚写下）的分支不算。
fn stale_sessions(conn: &Connection, sid: &str) -> Result<Vec<(SessionMap, bool)>> {
    let mut tips = Vec::new();
    for session in map::thread_sessions(conn, sid)? {
        if let Some(Some(tip)) = true_tip(conn, session.session_id)? {
            if tombstone(conn, sid, Some(&tip))?.is_none() {
                tips.push((session, tip));
            }
        }
    }
    if tips.is_empty() {
        return Ok(Vec::new());
    }
    let chains = thread_chains(conn, sid)?;
    let mut stale = Vec::new();
    for (session, tip) in tips {
        if let Some(purge) = chain_hit(conn, &chains, &tip)? {
            stale.push((session, purge));
        }
    }
    Ok(stale)
}

// ---------------------------------------------------------------------------
// 一轮合入
// ---------------------------------------------------------------------------

/// 一条 op 处理之后的去处。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// 合入或跳过，已移出收件箱。
    Done,
    Queued,
    Orphan,
    Quarantined,
    /// 不认识的 op 类型：这台设备的日志停在这里。
    Halt,
    /// 这一轮没处理成，留着下一轮再试。
    Defer,
}

struct Held {
    sid: String,
    keys: Vec<(String, String)>,
    _guards: Vec<Box<dyn Send>>,
    since: Instant,
}

struct Round<'a> {
    ctx: &'a Ctx,
    s: &'a Settings,
    /// 空间密钥：落地内联工具结果撞名时要按它算对象名。清理类轮次没有消息落地，可以为空。
    keys: Option<&'a Keys>,
    memo: &'a mut Memo,
    report: MergeReport,
    held: Option<Held>,
    /// 被改动、还没通知运行时的会话：键 → 是否被删除。
    changed: BTreeMap<(String, String), bool>,
    /// 这一轮里有消息落地、有删除或开头信息到达的线程：结束时复查悬空区与墓碑。
    touched: BTreeSet<String>,
    /// 这一轮里不再看的设备（收件箱已处理完，或它的日志停住了）。
    finished: HashSet<String>,
}

/// 合入一轮。阻塞（数据库与文件 IO），由引擎放在阻塞线程上跑；同一时刻只能有一轮。
/// `full` 为真时把所有悬空区、墓碑都复查一遍，并做到期清理（启动时、导入会话包之后、
/// 以及每隔一段时间）。`keys` 是当前空间密钥：内联工具结果撞名时按它登记对象名。
pub fn merge_round(
    ctx: &Ctx,
    s: &Settings,
    keys: &Keys,
    memo: &mut Memo,
    full: bool,
) -> Result<MergeReport> {
    let mut round = Round {
        ctx,
        s,
        keys: Some(keys),
        memo,
        report: MergeReport::default(),
        held: None,
        changed: BTreeMap::new(),
        touched: BTreeSet::new(),
        finished: HashSet::new(),
    };
    let outcome = round.run(full);
    // 不论成败：先通知运行时这些会话变了，再放开占用。
    round.release();
    let mut report = round.report;
    outcome?;
    report.queued = db::read(ctx.db(), |conn| {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_inbox WHERE state = ?1)",
            params![INBOX_QUEUED],
            |row| row.get(0),
        )?)
    })?;
    Ok(report)
}

/// 有没有要清点的（[`sweep_stale`]）：本机吃过全量快照或换过身份、还没清点过
/// （[`db::KV_STALE_CHECK`]），或者合入时记下了线程。有的话，本机手里可能留着已被删除的
/// 分支更早的样子——这时候不写全量快照。
pub(crate) fn stale_pending(conn: &Connection) -> Result<bool> {
    if db::kv_json::<bool>(conn, db::KV_STALE_CHECK)? == Some(true) {
        return Ok(true);
    }
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sync_state WHERE scope = ?1)",
        params![SCOPE_STALE_TODO],
        |row| row.get(0),
    )?)
}

/// [`sweep_stale`] 的结果。
#[derive(Debug, Default)]
pub struct StaleSweep {
    /// 删掉了哪些会话（`chats`、`list_changed`）。
    pub report: MergeReport,
    /// 还有线程没清点完（它的会话正忙，或它还有排着队的 op）：之后再来。
    pub pending: bool,
}

/// 清点「别处已经删掉的分支更早的样子」（§7.5）：末条落在某条墓碑的链上的本地分支按删除
/// 处理（不是彻底删除时进本机的回收站）。
///
/// 只在各设备的日志都读到头、收件箱也处理过一遍之后调用：末条落在链上的分支也可能只是
/// 后面的消息还没到——到了之后它要么长到墓碑的末条、照常删除，要么接上别的续写、留下。
/// 判断早了也不丢东西：那段续写到的时候接不上，会请持有它的设备把整条分支重发。
///
/// `all` 为假只看合入时记下的线程；为真把所有带链的墓碑对着本机的分支看一遍（本机吃了
/// 全量快照、或换过身份之后）。阻塞，和合入一样放在阻塞线程上跑，不与合入同时进行。
pub fn sweep_stale(ctx: &Ctx, s: &Settings, all: bool) -> Result<StaleSweep> {
    let (todo, threads) = db::read(ctx.db(), |conn| {
        let todo: BTreeSet<String> = db::state_keys(conn, SCOPE_STALE_TODO, "")?
            .into_iter()
            .collect();
        let mut threads = todo.clone();
        if all {
            for key in db::state_keys(conn, scope::TOMB_CHAIN, "")? {
                if let Some((sid, _)) = key.split_once('/') {
                    threads.insert(sid.to_string());
                }
            }
        }
        Ok((todo, threads))
    })?;
    if threads.is_empty() {
        return Ok(StaleSweep::default());
    }
    let mut memo = Memo::default();
    let mut round = Round {
        ctx,
        s,
        // 这一步只删会话、不发消息：不需要空间密钥。
        keys: None,
        memo: &mut memo,
        report: MergeReport::default(),
        held: None,
        changed: BTreeMap::new(),
        touched: BTreeSet::new(),
        finished: HashSet::new(),
    };
    let outcome = round.sweep_stale(&todo, &threads);
    // 不论成败：先通知运行时这些会话没了，再放开占用。
    round.release();
    let report = round.report;
    Ok(StaleSweep {
        report,
        pending: outcome?,
    })
}

impl Round<'_> {
    fn run(&mut self, full: bool) -> Result<()> {
        // 上次应用快照中断留下的暂存批先清掉：它们不可合入，只会一直压着「本机够格写
        // 快照」。平时没有登记中的快照，这一下是零开销。
        snapshot::clear_uncommitted(self.ctx)?;
        self.retry_tombstones()?;
        self.drain_queued()?;
        self.main_pass()?;
        if full {
            let (orphaned, tombstoned) = db::read(self.ctx.db(), |conn| {
                let orphaned = threads_in_state(conn, INBOX_ORPHAN)?;
                let mut tombstoned = BTreeSet::new();
                for (key, _) in db::state_scan(conn, scope::TOMB, "")? {
                    if let Some((sid, _)) = key.split_once('/') {
                        tombstoned.insert(sid.to_string());
                    }
                }
                Ok((orphaned, tombstoned))
            })?;
            self.touched.extend(orphaned);
            self.touched.extend(tombstoned);
        }
        let touched: Vec<String> = self.touched.iter().cloned().collect();
        for sid in &touched {
            self.recheck_orphans(sid)?;
        }
        for sid in &touched {
            self.sweep_tombstones(sid)?;
        }
        self.note_stale(&touched)?;
        for sid in &touched {
            self.prune_and_want(sid)?;
        }
        if full {
            self.expire()?;
        }
        if full || self.report.applied > 0 {
            self.sweep_insegs()?;
        }
        Ok(())
    }

    /// 这一轮动过的线程里，哪些有分支成了「别处已经删掉的分支更早的样子」：记下来，等
    /// 各设备的日志读到头再清点（[`sweep_stale`]）。这里不删——后面的续写可能只是还没到。
    fn note_stale(&mut self, touched: &[String]) -> Result<()> {
        let ctx = self.ctx;
        for chunk in touched.chunks(256) {
            let stale: Vec<&String> = db::read(ctx.db(), |conn| {
                let mut stale = Vec::new();
                for sid in chunk {
                    if !stale_sessions(conn, sid)?.is_empty() {
                        stale.push(sid);
                    }
                }
                Ok(stale)
            })?;
            if stale.is_empty() {
                continue;
            }
            db::write(ctx.db(), |tx| {
                for sid in &stale {
                    db::state_put(tx, SCOPE_STALE_TODO, sid, db::HLC_ZERO, None, None)?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    /// [`sweep_stale`] 的主体。`todo` 是合入时记下的线程，`threads` 是这一次要看的全部
    /// 线程。返回还有没有没清点完的。
    fn sweep_stale(&mut self, todo: &BTreeSet<String>, threads: &BTreeSet<String>) -> Result<bool> {
        enum Check {
            Clear,
            Busy,
            Stale(Vec<(SessionMap, bool)>),
        }
        let ctx = self.ctx;
        let mut pending = false;
        for sid in threads {
            let check = db::read(ctx.db(), |conn| {
                // 被隔离的线程留给用户处理，不在这里等它。
                if db::state_get(conn, scope::QUARANTINE, sid)?.is_some() {
                    return Ok(Check::Clear);
                }
                // 还有排着队的 op（线程正忙）：它们落地之后末条会变，到时候再看。
                let queued: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sync_inbox WHERE sid = ?1 AND state = ?2)",
                    params![sid, INBOX_QUEUED],
                    |row| row.get(0),
                )?;
                if queued {
                    return Ok(Check::Busy);
                }
                let stale = stale_sessions(conn, sid)?;
                Ok(if stale.is_empty() {
                    Check::Clear
                } else {
                    Check::Stale(stale)
                })
            })?;
            let busy = match check {
                Check::Clear => false,
                Check::Busy => true,
                Check::Stale(stale) => {
                    let mut busy = false;
                    for (session, _) in stale {
                        let key = (session.channel.clone(), session.chat_id.clone());
                        if !self.hold(sid, vec![key]) {
                            busy = true;
                            continue;
                        }
                        // 占住之后再确认一次：占到之前它可能又往下写了。
                        let still = db::read(ctx.db(), |conn| {
                            Ok(stale_sessions(conn, sid)?
                                .into_iter()
                                .find(|(now, _)| now.session_id == session.session_id)
                                .map(|(_, purge)| purge))
                        })?;
                        if let Some(purge) = still {
                            // 一个会话删不成（磁盘上的问题）不连累别的：留着下次再试。
                            if let Err(error) = self.remove_session(&session, purge) {
                                tracing::warn!(
                                    "线程 {sid} 留在本机的旧分支没能清理（下次再试）: {error}"
                                );
                                busy = true;
                            }
                        }
                    }
                    busy
                }
            };
            pending |= busy;
            if busy != todo.contains(sid) {
                db::write(ctx.db(), |tx| {
                    if busy {
                        db::state_put(tx, SCOPE_STALE_TODO, sid, db::HLC_ZERO, None, None)
                    } else {
                        db::state_del(tx, SCOPE_STALE_TODO, sid)
                    }
                })?;
            }
        }
        Ok(pending)
    }

    // -- 占用 ---------------------------------------------------------------

    /// 占住 `keys` 这些会话。已经占着同一组而且没占多久就沿用；占不到（有会话正忙）
    /// 返回假。
    fn hold(&mut self, sid: &str, keys: Vec<(String, String)>) -> bool {
        if let Some(held) = &self.held {
            if held.sid == sid && held.keys == keys && held.since.elapsed() < HOLD_MAX {
                return true;
            }
        }
        self.release();
        let mut guards = Vec::with_capacity(keys.len());
        for (channel, chat_id) in &keys {
            match self.ctx.gate.begin_merge(channel, chat_id) {
                Some(guard) => guards.push(guard),
                None => return false,
            }
        }
        self.held = Some(Held {
            sid: sid.to_string(),
            keys,
            _guards: guards,
            since: Instant::now(),
        });
        true
    }

    /// 放开占用。放开之前先把改动过的会话通知运行时：放开之后 turn 随时可能开始。
    fn release(&mut self) {
        for ((channel, chat_id), deleted) in std::mem::take(&mut self.changed) {
            self.ctx.gate.session_changed(&channel, &chat_id, deleted);
        }
        self.held = None;
    }

    fn mark_changed(&mut self, channel: &str, chat_id: &str, deleted: bool) {
        let entry = self
            .changed
            .entry((channel.to_string(), chat_id.to_string()))
            .or_insert(false);
        *entry |= deleted;
        self.report
            .chats
            .insert(publish::chat_key(channel, chat_id));
    }

    // -- 收件箱里一条 op 的处置 ---------------------------------------------

    /// 把一条 op 停在某个状态（排队、悬空、隔离）。状态没变就不写库。
    fn park(&mut self, row: &InboxRow, state: i64) -> Result<Outcome> {
        if row.state != state {
            db::write(self.ctx.db(), |tx| settle(tx, row, Some(state)))?;
        }
        Ok(match state {
            INBOX_QUEUED => Outcome::Queued,
            INBOX_ORPHAN => Outcome::Orphan,
            _ => Outcome::Quarantined,
        })
    }

    /// 丢弃一条不合规、又不属于任何线程的 op。
    fn drop_row(&mut self, row: &InboxRow, reason: &str) -> Result<Outcome> {
        tracing::warn!(
            "丢弃来自设备 {} 的第 {} 条 op：{reason}",
            row.origin,
            row.seq
        );
        db::write(self.ctx.db(), |tx| {
            note_lossy(tx)?;
            settle(tx, row, None)
        })?;
        Ok(Outcome::Done)
    }

    /// 不合规的会话包。日志里重发的包只是补救手段：它本身不合规就不用它，不牵连线程。
    /// 全量快照里的包（收件箱里序号记为 0）却是这个线程在本机的全部来源，丢了就补不回来：
    /// 隔离线程，留给用户处理（升级之后重试也许就认得了）。
    fn reject_bundle(&mut self, row: &InboxRow, sid: &str, reason: &str) -> Result<Outcome> {
        if row.seq == 0 {
            self.quarantine_row(row, sid, reason)
        } else {
            self.drop_row(row, reason)
        }
    }

    fn quarantine_row(&mut self, row: &InboxRow, sid: &str, reason: &str) -> Result<Outcome> {
        tracing::warn!(
            "线程 {sid} 被隔离：来自设备 {} 的第 {} 条 op 不合规（{reason}）",
            row.origin,
            row.seq
        );
        db::write(self.ctx.db(), |tx| quarantine(tx, row, sid, reason))?;
        self.report.status_changed = true;
        Ok(Outcome::Quarantined)
    }

    /// 处理一条 op；出错时记一次失败，连续多轮都失败就不再让它堵着。
    fn attempt(&mut self, row: &InboxRow) -> Result<Outcome> {
        let failure = match self.process_row(row) {
            Ok(Outcome::Defer) => "去向一直定不下来".to_string(),
            Ok(outcome) => {
                self.memo.failures.remove(&row.id);
                return Ok(outcome);
            }
            Err(error) => error.to_string(),
        };
        let count = self.memo.failures.entry(row.id).or_insert(0);
        *count += 1;
        tracing::warn!(
            "合入来自设备 {} 的第 {} 条 op 没有成功（第 {count} 次）：{failure}",
            row.origin,
            row.seq,
        );
        if *count < ROW_FAIL_MAX {
            return Ok(Outcome::Defer);
        }
        self.memo.failures.remove(&row.id);
        let sid = OpLine::parse(&row.op)
            .ok()
            .and_then(|line| line.sid)
            .filter(|sid| ids::is_sid(sid));
        match sid {
            Some(sid) => self.quarantine_row(row, &sid, &format!("合入失败：{failure}")),
            None => {
                status::notice(
                    &self.ctx.paths,
                    "op-dropped",
                    &format!("{}/{}", row.origin, row.seq),
                    "有一条同步数据无法处理，已跳过",
                    &failure,
                );
                self.report.status_changed = true;
                self.drop_row(row, &failure)
            }
        }
    }

    fn process_row(&mut self, row: &InboxRow) -> Result<Outcome> {
        let line = match OpLine::parse(&row.op) {
            Ok(line) => line,
            // 取回时已经解析过一遍；走到这里说明库里这一行坏了。
            Err(error) => return self.drop_row(row, &error.to_string()),
        };
        if !line.is_known() {
            let reason = format!(
                "{HALT_UNKNOWN_OP}（{}）：那台设备上的 RClaw 版本更新，请升级本机",
                line.kind
            );
            db::write(self.ctx.db(), |tx| {
                let mut peer = db::peer_state(tx, &row.origin)?;
                if peer.halted.as_deref() != Some(reason.as_str()) {
                    peer.halted = Some(reason.clone());
                    db::put_peer_state(tx, &row.origin, &peer)?;
                }
                Ok(())
            })?;
            self.report.status_changed = true;
            return Ok(Outcome::Halt);
        }
        // 不在本机会话同步范围里的线程（会话范围没开，或线程的通道不在名单里）：它的 op
        // 看过就算。`session.open` 在范围开着时照常记下——线程在哪个通道就写在它里面。
        if SESSION_OPS.contains(&line.kind.as_str()) {
            let s = self.s;
            let skip = if !s.scopes.sessions {
                true
            } else if line.kind == "session.open" {
                false
            } else {
                match line.sid.as_deref().filter(|sid| ids::is_sid(sid)) {
                    Some(sid) => db::read(self.ctx.db(), |conn| thread_out_of_scope(conn, s, sid))?,
                    None => false,
                }
            };
            if skip {
                // 范围开着却跳过：是通道不在名单里。
                let skipped = line
                    .sid
                    .as_deref()
                    .filter(|sid| s.scopes.sessions && ids::is_sid(sid));
                db::write(self.ctx.db(), |tx| {
                    if let Some(sid) = skipped {
                        note_skipped(tx, sid)?;
                    }
                    settle(tx, row, None)
                })?;
                return Ok(Outcome::Done);
            }
        }
        let op = match line.decode() {
            Ok(op) => op,
            Err(error) => {
                // 内容不合规：消息与开头信息坏了，线程接不下去，隔离；其余的丢弃。
                let chain = matches!(line.kind.as_str(), "msg" | "session.open");
                return match line.sid.as_deref().filter(|sid| chain && ids::is_sid(sid)) {
                    Some(sid) => self.quarantine_row(row, sid, &error.to_string()),
                    None => self.drop_row(row, &error.to_string()),
                };
            }
        };
        match op {
            Op::Msg(op) => self.op_msg(row, &op.sid, &op.msg),
            Op::SessionBundle { sid, bundle } => self.op_bundle(row, &sid, &bundle),
            Op::SessionDelete {
                sid,
                tip,
                purge,
                chain,
            } => self.op_delete(row, &sid, tip.as_deref(), purge, &chain),
            Op::SessionOpen {
                sid,
                channel,
                chat_id,
                created_at,
                fork_of,
            } => self.op_open(row, &sid, channel, chat_id, created_at, fork_of),
            Op::SessionSet {
                sid,
                at,
                field,
                value,
            } => self.op_set(row, &sid, at.as_deref(), &field, &value),
            Op::SessionWant { sid, gid, to } => self.op_want(row, &sid, &gid, &to),
            Op::DeviceHello {
                name,
                platform,
                version,
                replaces,
            } => self.op_hello(row, &name, &platform, &version, replaces),
            Op::BlobWant { blob } => {
                db::write(self.ctx.db(), |tx| {
                    if ids::is_digest(&blob) {
                        {
                            let keys = crate::secrets::Secrets::load(&self.ctx.paths)
                                .ok()
                                .and_then(|secrets| secrets.keys().ok());
                            attach::on_blob_want_at(
                                tx,
                                Some(&self.ctx.env),
                                keys.as_ref(),
                                &blob,
                            )?
                        }
                    }
                    settle(tx, row, None)
                })?;
                Ok(Outcome::Done)
            }
            Op::BlobGone { blob } => {
                let now = now_ms();
                db::write(self.ctx.db(), |tx| {
                    if ids::is_digest(&blob) {
                        attach::on_blob_gone(tx, &blob, now)?;
                    }
                    settle(tx, row, None)
                })?;
                Ok(Outcome::Done)
            }
            other => self.op_file(row, &other),
        }
    }

    // -- 各类 op -------------------------------------------------------------

    fn op_msg(&mut self, row: &InboxRow, sid: &str, msg: &WireMsg) -> Result<Outcome> {
        if !ids::is_sid(sid) {
            return self.drop_row(row, "线程标识不合规");
        }
        if let Some(state) = db::read(self.ctx.db(), |conn| thread_hold(conn, sid, row))? {
            return self.park(row, state);
        }
        if let Err(error) = portable::check_wire_msg(msg) {
            return self.quarantine_row(row, sid, &error.to_string());
        }
        Ok(match self.merge_msg(sid, msg, Some(row))? {
            MsgOutcome::Landed | MsgOutcome::Skipped => Outcome::Done,
            MsgOutcome::Orphan => Outcome::Orphan,
            MsgOutcome::Queued => Outcome::Queued,
            MsgOutcome::Quarantine(_) => {
                self.report.status_changed = true;
                Outcome::Quarantined
            }
            MsgOutcome::Defer => Outcome::Defer,
        })
    }

    /// 合入一条已通过检查的消息。`row` 给定时（它是收件箱里的一条 `msg` op），它在收件箱
    /// 里的去处与消息的效果在同一个事务里提交；会话包里的消息不带 `row`。
    fn merge_msg(
        &mut self,
        sid: &str,
        msg: &WireMsg,
        row: Option<&InboxRow>,
    ) -> Result<MsgOutcome> {
        let (ctx, s) = (self.ctx, self.s);
        // 空间密钥：落地时内联工具结果撞名要按它登记对象名（`keys` 下面还有同名局部量，
        // 是会话占用的键）。
        let space_keys = self.keys;
        let mut hint: Option<String> = None;
        let mut prepared: Option<Plan> = None;
        for _ in 0..MSG_ATTEMPTS {
            // 1. 先看去向（只读），据此占住会话、准备文件。
            let (plan, keys, adopt) = db::read(ctx.db(), |conn| {
                let plan = plan_msg(conn, sid, msg, hint.as_deref())?;
                let keys = if plan.lands() {
                    gate_keys(conn, sid, &plan)?
                } else {
                    Vec::new()
                };
                // 会话包里本机已有的消息：本机那一行还没发布过的话，要写库把它记成已发布。
                let adopt = row.is_none()
                    && plan == Plan::Skip
                    && has_unpublished_copy(conn, sid, &msg.gid)?;
                Ok((plan, keys, adopt))
            })?;
            if plan.lands() {
                if !self.hold(sid, keys) {
                    if let Some(old) = prepared.take() {
                        discard_files(ctx, &old);
                    }
                    if let Some(row) = row {
                        self.park(row, INBOX_QUEUED)?;
                    }
                    return Ok(MsgOutcome::Queued);
                }
                if needs_files(&plan, msg) && prepared.as_ref() != Some(&plan) {
                    if let Some(old) = prepared.take() {
                        discard_files(ctx, &old);
                    }
                    prepare_files(ctx, &plan, msg, space_keys)?;
                    prepared = Some(plan.clone());
                }
                if let Plan::Branch { chat_id, .. } = &plan {
                    hint = Some(chat_id.clone());
                }
            } else {
                // 不落地、也没有收件箱里的去处要改（会话包里的消息，或者复查悬空区时仍然
                // 悬空）：什么都没变，不必写库。
                let unchanged = match row {
                    None => !adopt,
                    Some(row) => matches!(plan, Plan::Orphan) && row.state == INBOX_ORPHAN,
                };
                if unchanged {
                    return Ok(match plan {
                        Plan::Orphan => MsgOutcome::Orphan,
                        Plan::Quarantine(reason) => MsgOutcome::Quarantine(reason),
                        _ => MsgOutcome::Skipped,
                    });
                }
            }
            // 2. 在写事务里重新看一遍去向：与刚才一致才执行，不一致就重来。
            let effect = db::write(ctx.db(), |tx| {
                if plan_msg(tx, sid, msg, hint.as_deref())? != plan {
                    return Ok(None);
                }
                let effect = apply_plan(tx, ctx, s, sid, msg, &plan, space_keys)?;
                if let Some(row) = row {
                    match &plan {
                        Plan::Orphan => settle(tx, row, Some(INBOX_ORPHAN))?,
                        Plan::Quarantine(reason) => quarantine(tx, row, sid, reason)?,
                        _ => settle(tx, row, None)?,
                    }
                }
                Ok(Some(effect))
            })?;
            let Some(effect) = effect else {
                continue;
            };
            if let Some(old) = prepared.take() {
                if old != plan {
                    discard_files(ctx, &old);
                }
            }
            if let Some((channel, chat_id)) = &effect.session {
                self.mark_changed(channel, chat_id, false);
                self.report.list_changed |= effect.created || effect.attrs_changed;
            }
            if effect.session.is_some() || matches!(plan, Plan::Orphan) {
                // 落地了新消息：悬空区里等它的可以接上了。新悬空的：结束时发请求。
                self.touched.insert(sid.to_string());
            }
            return Ok(match plan {
                Plan::Skip => MsgOutcome::Skipped,
                Plan::Orphan => MsgOutcome::Orphan,
                Plan::Quarantine(reason) => MsgOutcome::Quarantine(reason),
                _ => MsgOutcome::Landed,
            });
        }
        if let Some(old) = prepared.take() {
            discard_files(ctx, &old);
        }
        Ok(MsgOutcome::Defer)
    }

    /// `session.bundle`：别的设备重发的一条分支（§7.5）。已有的消息跳过，缺的补上。
    fn op_bundle(&mut self, row: &InboxRow, sid: &str, text: &str) -> Result<Outcome> {
        if !ids::is_sid(sid) {
            return self.drop_row(row, "线程标识不合规");
        }
        if let Some(state) = db::read(self.ctx.db(), |conn| thread_hold(conn, sid, row))? {
            return self.park(row, state);
        }
        let (header, messages) = match bundle::parse_bundle(text, ARTIFACT_INLINE_MAX) {
            Ok(parsed) if parsed.0.sid == sid => parsed,
            Ok(_) => return self.reject_bundle(row, sid, "会话包与所属线程不符"),
            Err(error) => return self.reject_bundle(row, sid, &error.to_string()),
        };
        let (ctx, s) = (self.ctx, self.s);
        let tip_key = sync_db::tombstone_key(sid, header.tip.as_deref());
        // 末条命中墓碑的分支整条跳过：已删除的分支不因为重发而复活。
        let buried = db::write(ctx.db(), |tx| {
            if db::state_get(tx, scope::TOMB, &tip_key)?.is_some() {
                settle(tx, row, None)?;
                return Ok(true);
            }
            // 这个末条的分支已经有设备重发过了：本机不必再应答同一个请求。
            db::state_put(tx, scope::BUNDLE_SENT, &tip_key, db::HLC_ZERO, None, None)?;
            let usable = ids::is_valid_channel(&header.channel)
                && ids::is_valid_chat_id(&header.chat_id)
                && !is_internal_chat(&header.chat_id);
            if usable && !map::has_remote_open(tx, sid)? {
                let origin = if ids::is_device_id(&header.origin) {
                    header.origin.clone()
                } else {
                    row.origin.clone()
                };
                map::put_open_info(
                    tx,
                    sid,
                    &OpenInfo {
                        channel: header.channel.clone(),
                        chat_id: header.chat_id.clone(),
                        created_at: header.created_at,
                        fork_of: header.fork_of.clone(),
                        origin,
                    },
                )?;
            }
            // 线程的通道到这里才知道（开头信息是这个包带来的）：不在本机范围里就不落地。
            if thread_out_of_scope(tx, s, sid)? {
                if s.scopes.sessions {
                    note_skipped(tx, sid)?;
                }
                settle(tx, row, None)?;
                return Ok(true);
            }
            Ok(false)
        })?;
        if buried {
            // 悬空区里等着这个线程开头信息的 op 可以有个了结了。
            self.touched.insert(sid.to_string());
            return Ok(Outcome::Done);
        }
        for msg in &messages {
            match self.merge_msg(sid, msg, None)? {
                MsgOutcome::Landed | MsgOutcome::Skipped => {}
                MsgOutcome::Queued => return self.park(row, INBOX_QUEUED),
                MsgOutcome::Orphan => return self.park(row, INBOX_ORPHAN),
                MsgOutcome::Quarantine(reason) => return self.quarantine_row(row, sid, &reason),
                MsgOutcome::Defer => return Ok(Outcome::Defer),
            }
        }
        db::write(ctx.db(), |tx| settle(tx, row, None))?;
        Ok(Outcome::Done)
    }

    /// `session.delete{sid, tip, chain}`：记下墓碑和它带的链，删掉本机末条正是 `tip` 的
    /// 分支（§7.5）。末条落在链上、却不是 `tip` 的分支（这条分支更早的样子）不在这里删：
    /// 后面的续写可能只是还没到，等各设备的日志读到头再清点（[`sweep_stale`]）。
    fn op_delete(
        &mut self,
        row: &InboxRow,
        sid: &str,
        tip: Option<&str>,
        purge: bool,
        chain: &str,
    ) -> Result<Outcome> {
        if !ids::is_sid(sid) || tip.is_some_and(|tip| !ids::is_gid(tip)) {
            return self.drop_row(row, "删除记录不合规");
        }
        if let Some(state) = db::read(self.ctx.db(), |conn| thread_hold(conn, sid, row))? {
            return self.park(row, state);
        }
        // 链的最后一条就是 `tip`。对不上、或形状不对的链不用：删除本身照常生效，只是本机
        // 认不出这条分支更早的样子——和丢过 op 一样，本机不再写全量快照。
        let chain_ok = chain.is_empty()
            || (ids::is_chain(chain)
                && tip
                    .and_then(ids::gid_tail)
                    .is_some_and(|tail| chain.ends_with(tail)));
        if !chain_ok {
            tracing::warn!(
                "来自设备 {} 的第 {} 条 op（删除记录）带的链不合规，没有采用",
                row.origin,
                row.seq
            );
        }
        let ctx = self.ctx;
        let matching: Vec<SessionMap> = db::write(ctx.db(), |tx| {
            let key = sync_db::tombstone_key(sid, tip);
            db::state_put_lww(
                tx,
                scope::TOMB,
                &key,
                &row.hlc,
                None,
                Some(&serde_json::json!({ "purge": purge }).to_string()),
            )?;
            if !chain_ok {
                note_lossy(tx)?;
            } else if !chain.is_empty() {
                // 同一个末条的链内容相同，只是可能截在不同的长度上：留长的。
                tx.execute(
                    "INSERT INTO sync_state(scope, key, hlc, hash, value) \
                     VALUES (?1, ?2, ?3, NULL, ?4) \
                     ON CONFLICT(scope, key) DO UPDATE SET hlc = excluded.hlc, \
                     value = excluded.value \
                     WHERE sync_state.value IS NULL \
                     OR length(excluded.value) > length(sync_state.value)",
                    params![scope::TOMB_CHAIN, key, row.hlc, chain],
                )?;
            }
            let mut matching = Vec::new();
            for session in map::thread_sessions(tx, sid)? {
                if true_tip(tx, session.session_id)? == Some(tip.map(str::to_string)) {
                    matching.push(session);
                }
            }
            if matching.is_empty() {
                settle(tx, row, None)?;
            }
            Ok(matching)
        })?;
        // 悬空区里末条命中这个墓碑的链要丢弃。
        self.touched.insert(sid.to_string());
        if matching.is_empty() {
            return Ok(Outcome::Done);
        }
        let keys = matching
            .iter()
            .map(|session| (session.channel.clone(), session.chat_id.clone()))
            .collect();
        if !self.hold(sid, keys) {
            return self.park(row, INBOX_QUEUED);
        }
        for session in &matching {
            // 占住之后再确认一次：占到之前这条分支可能又往下写了，远端的删除没覆盖到续写
            // 的部分。会话还在这个线程里、末条没变才删，用的是这时墓碑里的彻底删除标记。
            let current = db::read(ctx.db(), |conn| recheck_tombstone(conn, sid, session.session_id))?;
            match current {
                Some((session, purge, now_tip)) if now_tip == tip.map(str::to_string) => {
                    self.remove_session(&session, purge)?;
                }
                _ => {}
            }
        }
        db::write(ctx.db(), |tx| settle(tx, row, None))?;
        Ok(Outcome::Done)
    }

    /// 删掉一个本地会话（合入远端的删除）。不是彻底删除时先放进本机的回收站。
    fn remove_session(&mut self, session: &SessionMap, purge: bool) -> Result<()> {
        let ctx = self.ctx;
        if !purge && self.s.trash_days > 0 {
            trash::stash(ctx, &session.channel, &session.chat_id)?;
        }
        ctx.env.sessions.delete_session_with(
            &session.channel,
            &session.chat_id,
            SessionDeleteSync::Local,
        )?;
        self.mark_changed(&session.channel, &session.chat_id, true);
        self.report.list_changed = true;
        Ok(())
    }

    fn op_open(
        &mut self,
        row: &InboxRow,
        sid: &str,
        channel: String,
        chat_id: String,
        created_at: i64,
        fork_of: Option<ForkOf>,
    ) -> Result<Outcome> {
        if !ids::is_sid(sid) {
            return self.drop_row(row, "线程标识不合规");
        }
        let valid = ids::is_valid_channel(&channel)
            && ids::is_valid_chat_id(&chat_id)
            && !is_internal_chat(&chat_id)
            && fork_of
                .as_ref()
                .is_none_or(|fork| ids::is_sid(&fork.sid) && ids::is_gid(&fork.gid));
        if !valid {
            return self.quarantine_row(row, sid, "线程的开头信息不合规");
        }
        db::write(self.ctx.db(), |tx| {
            map::put_open_info(
                tx,
                sid,
                &OpenInfo {
                    channel,
                    chat_id,
                    created_at,
                    fork_of,
                    origin: row.origin.clone(),
                },
            )?;
            // 这个线程别的设备已经发布过：本机这个线程里「只分配了身份」的会话不必再发
            // 一遍 `session.open`（重置同步空间、重新加入之后）。它们还没发布过的消息照常
            // 由下一次全量发布扫描排队。
            tx.execute(
                "UPDATE sync_session SET sealed = ?1 WHERE sid = ?2 AND sealed = ?3",
                params![SEAL_SEALED, sid, SEAL_LOCAL],
            )?;
            settle(tx, row, None)
        })?;
        // 开头的消息可能先到、正等着这份信息。
        self.touched.insert(sid.to_string());
        Ok(Outcome::Done)
    }

    /// `session.set`：属性不动历史，所以不排队也不占会话。
    fn op_set(
        &mut self,
        row: &InboxRow,
        sid: &str,
        at: Option<&str>,
        field: &str,
        value: &serde_json::Value,
    ) -> Result<Outcome> {
        let known = publish::FIELDS.contains(&field);
        // 不认识的字段（更新的版本加的）也记下，升级之后就能用上；只是现在不生效。
        let valid = ids::is_sid(sid)
            && at.is_none_or(ids::is_gid)
            && if known {
                publish::check_attr(field, value)
            } else {
                (1..=32).contains(&field.len())
                    && field
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                    && value.to_string().len() <= publish::GOAL_MAX
            };
        if !valid {
            return self.drop_row(row, "属性设置不合规");
        }
        let s = self.s;
        let changed: Vec<(String, String)> = db::write(self.ctx.db(), |tx| {
            let mut changed = Vec::new();
            if publish::record_attr(tx, sid, at, field, &row.hlc, value)? && known {
                for session in map::thread_sessions(tx, sid)? {
                    if publish::refresh_attrs(tx, s, session.session_id)? {
                        changed.push((session.channel, session.chat_id));
                    }
                }
            }
            settle(tx, row, None)?;
            Ok(changed)
        })?;
        for (channel, chat_id) in changed {
            self.report
                .chats
                .insert(publish::chat_key(&channel, &chat_id));
            self.report.list_changed = true;
        }
        Ok(Outcome::Done)
    }

    /// `session.want`：先记下，等这一轮合入结束再应答（[`answer_wants`]）。
    fn op_want(&mut self, row: &InboxRow, sid: &str, gid: &str, to: &str) -> Result<Outcome> {
        if !ids::is_sid(sid) || !ids::is_gid(gid) || !ids::is_device_id(to) {
            return self.drop_row(row, "分支重发请求不合规");
        }
        db::write(self.ctx.db(), |tx| {
            db::state_put_json(
                tx,
                SCOPE_WANT_TODO,
                &format!("{sid}|{gid}"),
                &row.hlc,
                &WantTodo {
                    to: to.to_string(),
                    from: row.origin.clone(),
                },
            )?;
            settle(tx, row, None)
        })?;
        Ok(Outcome::Done)
    }

    fn op_hello(
        &mut self,
        row: &InboxRow,
        name: &str,
        platform: &str,
        version: &str,
        replaces: Option<String>,
    ) -> Result<Outcome> {
        let token = |text: &str, max: usize| -> String {
            text.chars()
                .filter(|c| c.is_ascii_graphic())
                .take(max)
                .collect()
        };
        let info = DeviceInfo {
            name: portable::clean_title(name)
                .map(|name| name.chars().take(64).collect())
                .unwrap_or_default(),
            platform: token(platform, 32),
            version: token(version, 64),
            replaces: replaces.filter(|old| ids::is_device_id(old) && *old != row.origin),
        };
        let s = self.s;
        let rotate = db::write(self.ctx.db(), |tx| {
            db::state_put_lww(
                tx,
                scope::DEVICE,
                &row.origin,
                &row.hlc,
                None,
                Some(&serde_json::to_string(&info)?),
            )?;
            let mut rotate = false;
            if let Some(old) = &info.replaces {
                let mut peer = db::peer_state(tx, old)?;
                if peer.replaced_by.is_none() {
                    peer.replaced_by = Some(row.origin.clone());
                    db::put_peer_state(tx, old, &peer)?;
                }
                // 别的设备声明它取代了本机**现在**用的身份：这个身份有过两个写者，本机
                // 也换一个（§9.6）。和这条 op 的去处一起记下，中途退出也不会忘。
                rotate = *old == s.device && !s.is_me(&row.origin);
                if rotate {
                    crate::identity::request_rotation(
                        tx,
                        "另一份数据在用同一个同步身份，并且已经换用了新身份",
                        &[],
                    )?;
                }
            }
            settle(tx, row, None)?;
            Ok(rotate)
        })?;
        self.report.rotate |= rotate;
        self.report.status_changed = true;
        Ok(Outcome::Done)
    }

    /// 文件类数据的 op（记忆、每日笔记、人格、技能、bot、定时任务）：这里只更新寄存器，
    /// 落到本机文件由引擎在库锁之外做（[`files`]）。
    fn op_file(&mut self, row: &InboxRow, op: &Op) -> Result<Outcome> {
        // 寄存器的值在库锁之外序列化：技能的清单可能有几兆字节。
        let stored = files::stored_text(&row.origin, op)?;
        let (dirty, rejected) = db::write(self.ctx.db(), |tx| {
            let recorded = match files::record_op(tx, &row.origin, &row.hlc, op, &stored) {
                Ok(dirty) => (dirty, None),
                Err(SyncError::Invalid(reason)) => {
                    note_lossy(tx)?;
                    (Dirty::default(), Some(reason))
                }
                Err(error) => return Err(error),
            };
            settle(tx, row, None)?;
            Ok(recorded)
        })?;
        if let Some(reason) = rejected {
            tracing::warn!(
                "来自设备 {} 的第 {} 条 op 内容不合规，已跳过：{reason}",
                row.origin,
                row.seq
            );
        }
        self.report.dirty.merge(dirty);
        Ok(Outcome::Done)
    }

    // -- 各阶段 --------------------------------------------------------------

    /// 排队的 op：线程空下来了就按到达顺序处理。
    fn drain_queued(&mut self) -> Result<()> {
        let ctx = self.ctx;
        for sid in db::read(ctx.db(), |conn| threads_in_state(conn, INBOX_QUEUED))? {
            for id in db::read(ctx.db(), |conn| thread_rows(conn, &sid, INBOX_QUEUED))? {
                let Some(row) = db::read(ctx.db(), |conn| load_row(conn, id))? else {
                    continue;
                };
                if row.state != INBOX_QUEUED {
                    continue;
                }
                match self.attempt(&row)? {
                    Outcome::Queued | Outcome::Defer | Outcome::Halt => break,
                    _ => self.report.applied += 1,
                }
            }
        }
        Ok(())
    }

    /// 新取回的 op：每台设备的日志按序号处理；段声明的依赖没满足时先停下，等别的设备的
    /// 日志处理到位再继续。各设备轮流，直到一整圈都没有进展。
    fn main_pass(&mut self) -> Result<()> {
        let origins: Vec<String> = db::read(self.ctx.db(), |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT DISTINCT origin FROM sync_inbox WHERE state = ?1 ORDER BY origin",
            )?;
            let rows = stmt.query_map(params![INBOX_PENDING], |row| row.get(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
        })?;
        // 全量快照带来的 op（序号记为 0）先全部处理：各设备日志里接在快照之后的 op 大多
        // 接在它们后面，先处理日志只会白白悬空一遍。
        self.run_snapshot_rows()?;
        loop {
            let mut progressed = false;
            for origin in &origins {
                if !self.finished.contains(origin) {
                    progressed |= self.run_origin(origin)?;
                }
            }
            if !progressed {
                return Ok(());
            }
        }
    }

    /// 全量快照带来的 op（收件箱里序号记为 0）：按写进收件箱的先后处理，不分来源设备——
    /// 快照里的先后是写者排好的（设备、线程的开头信息、属性、删除、会话包、文件类数据）。
    /// 停住或一时定不下去向的留在原处，不拦着后面的：它们之后还会在所属设备的日志那
    /// 一遍里再被看到，该停的在那里停。
    fn run_snapshot_rows(&mut self) -> Result<()> {
        let ctx = self.ctx;
        let mut after = 0i64;
        loop {
            let batch: Vec<i64> = db::read(ctx.db(), |conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT id FROM sync_inbox WHERE state = ?1 AND seq = 0 AND id > ?2 \
                     ORDER BY id LIMIT 256",
                )?;
                let rows = stmt.query_map(params![INBOX_PENDING, after], |row| row.get(0))?;
                Ok(rows.collect::<rusqlite::Result<Vec<i64>>>()?)
            })?;
            if batch.is_empty() {
                return Ok(());
            }
            for id in batch {
                after = id;
                let Some(row) = db::read(ctx.db(), |conn| load_row(conn, id))? else {
                    continue;
                };
                if row.state != INBOX_PENDING {
                    continue;
                }
                match self.attempt(&row)? {
                    Outcome::Halt | Outcome::Defer => {}
                    _ => self.report.applied += 1,
                }
            }
        }
    }

    /// 处理一台设备日志里待处理的 op，直到处理完、被依赖挡住或停住。返回有没有进展。
    fn run_origin(&mut self, origin: &str) -> Result<bool> {
        let (ctx, s) = (self.ctx, self.s);
        let mut progressed = false;
        let mut after: (i64, i64) = (-1, 0);
        let mut ready_seg: Option<String> = None;
        loop {
            let batch: Vec<(i64, i64, String)> = db::read(ctx.db(), |conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT id, seq, seg FROM sync_inbox WHERE origin = ?1 AND state = ?2 \
                     AND (seq > ?3 OR (seq = ?3 AND id > ?4)) ORDER BY seq, id LIMIT 256",
                )?;
                let rows = stmt
                    .query_map(params![origin, INBOX_PENDING, after.0, after.1], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })?;
            if batch.is_empty() {
                self.finished.insert(origin.to_string());
                return Ok(progressed);
            }
            for (id, seq, seg) in batch {
                after = (seq, id);
                if ready_seg.as_deref() != Some(seg.as_str()) {
                    let ready =
                        db::read(ctx.db(), |conn| deps_ready(conn, s, origin, &seg, now_ms()))?;
                    if !ready {
                        return Ok(progressed);
                    }
                    ready_seg = Some(seg);
                }
                let Some(row) = db::read(ctx.db(), |conn| load_row(conn, id))? else {
                    continue;
                };
                if row.state != INBOX_PENDING {
                    continue;
                }
                match self.attempt(&row)? {
                    Outcome::Halt | Outcome::Defer => {
                        self.finished.insert(origin.to_string());
                        return Ok(progressed);
                    }
                    _ => {
                        progressed = true;
                        self.report.applied += 1;
                    }
                }
            }
        }
    }

    /// 线程有新消息落地之后复查它的悬空区：`parent` 到了的接上，直到一整遍都没有进展。
    fn recheck_orphans(&mut self, sid: &str) -> Result<()> {
        let ctx = self.ctx;
        loop {
            let mut progressed = false;
            for id in db::read(ctx.db(), |conn| thread_rows(conn, sid, INBOX_ORPHAN))? {
                let Some(row) = db::read(ctx.db(), |conn| load_row(conn, id))? else {
                    continue;
                };
                if row.state != INBOX_ORPHAN {
                    continue;
                }
                match self.attempt(&row)? {
                    Outcome::Done => {
                        progressed = true;
                        self.report.applied += 1;
                    }
                    // 线程正忙（这条已经改成排队）：等它空下来。
                    Outcome::Queued => return Ok(()),
                    _ => {}
                }
            }
            if !progressed {
                return Ok(());
            }
        }
    }

    /// 上一轮因为会话正忙没删成的：再试一次。
    fn retry_tombstones(&mut self) -> Result<()> {
        let ctx = self.ctx;
        let todo: Vec<String> = db::read(ctx.db(), |conn| {
            Ok(db::state_scan(conn, SCOPE_TOMB_TODO, "")?
                .into_iter()
                .map(|(sid, _)| sid)
                .collect())
        })?;
        if todo.is_empty() {
            return Ok(());
        }
        db::write(ctx.db(), |tx| {
            for sid in &todo {
                db::state_del(tx, SCOPE_TOMB_TODO, sid)?;
            }
            Ok(())
        })?;
        for sid in &todo {
            self.sweep_tombstones(sid)?;
        }
        Ok(())
    }

    /// 末条命中墓碑的本地分支按删除处理（§7.5）：重放、重发让一条已删除的分支又长回原来
    /// 的末条时，它不会复活。
    fn sweep_tombstones(&mut self, sid: &str) -> Result<()> {
        let ctx = self.ctx;
        let hits: Vec<SessionMap> = db::read(ctx.db(), |conn| {
            let mut hits = Vec::new();
            for session in map::thread_sessions(conn, sid)? {
                let Some(tip) = true_tip(conn, session.session_id)? else {
                    continue;
                };
                if tombstone(conn, sid, tip.as_deref())?.is_some() {
                    hits.push(session);
                }
            }
            Ok(hits)
        })?;
        for session in hits {
            let key = (session.channel.clone(), session.chat_id.clone());
            if !self.hold(sid, vec![key]) {
                // 正忙：记下来，下一轮再删。
                db::write(ctx.db(), |tx| {
                    db::state_put(tx, SCOPE_TOMB_TODO, sid, db::HLC_ZERO, None, None)
                })?;
                continue;
            }
            // 占住之后再确认一次：占到之前它可能又往下写了。是不是彻底删除也按这时生效的
            // 墓碑算。
            let current = db::read(ctx.db(), |conn| recheck_tombstone(conn, sid, session.session_id))?;
            if let Some((session, purge, _)) = current {
                self.remove_session(&session, purge)?;
            }
        }
        Ok(())
    }

    /// 悬空区的收尾：末条命中墓碑的链整条丢弃（它的作者后来自己删了这一支）；剩下的
    /// 链各发一条 `session.want`，请把它发来的那台设备重发整条分支（§7.5）。
    fn prune_and_want(&mut self, sid: &str) -> Result<()> {
        let (ctx, s) = (self.ctx, self.s);
        let now = now_ms();
        db::write(ctx.db(), |tx| {
            let mut nodes = orphan_nodes(tx, sid)?;
            if nodes.is_empty() || db::state_get(tx, scope::QUARANTINE, sid)?.is_some() {
                return Ok(());
            }
            let mut children: HashMap<String, Vec<String>> = HashMap::new();
            for (gid, node) in &nodes {
                if let Some(parent) = node
                    .parent
                    .as_ref()
                    .filter(|parent| nodes.contains_key(*parent))
                {
                    children
                        .entry(parent.clone())
                        .or_default()
                        .push(gid.clone());
                }
            }
            // 从命中墓碑的末条往上：一个节点的所有后续都要丢弃时，它也丢弃。末条落在某条
            // 墓碑的链上也算命中：那是被删的分支更早的一段，后面没有接着别的续写——留着
            // 只会去请别的设备把一条已经删掉的分支重发过来。
            let mut doomed: HashSet<String> = HashSet::new();
            let mut stack: Vec<String> = Vec::new();
            let mut chains: Option<Vec<(String, String)>> = None;
            for gid in nodes.keys() {
                if children.contains_key(gid) {
                    continue;
                }
                let mut hit = tombstone(tx, sid, Some(gid.as_str()))?.is_some();
                if !hit {
                    if chains.is_none() {
                        chains = Some(thread_chains(tx, sid)?);
                    }
                    let chains = chains.as_deref().unwrap_or_default();
                    hit = chain_hit(tx, chains, gid)?.is_some();
                }
                if hit {
                    stack.push(gid.clone());
                }
            }
            while let Some(gid) = stack.pop() {
                if !doomed.insert(gid.clone()) {
                    continue;
                }
                let Some(parent) = nodes.get(&gid).and_then(|node| node.parent.clone()) else {
                    continue;
                };
                let all_doomed = children
                    .get(&parent)
                    .is_some_and(|kids| kids.iter().all(|kid| doomed.contains(kid)));
                if all_doomed {
                    stack.push(parent);
                }
            }
            for gid in &doomed {
                if let Some(node) = nodes.remove(gid) {
                    for id in node.rows {
                        tx.execute("DELETE FROM sync_inbox WHERE id = ?1", params![id])?;
                    }
                }
            }
            // 每条链的第一条：它的 `parent` 既不在本机也不在悬空区里。
            for (gid, node) in &nodes {
                if node
                    .parent
                    .as_ref()
                    .is_some_and(|parent| nodes.contains_key(parent))
                {
                    continue;
                }
                let key = format!("{sid}/{gid}");
                let asked: i64 = db::state_get(tx, scope::WANT, &key)?
                    .and_then(|row| row.value.as_deref().and_then(|value| value.parse().ok()))
                    .unwrap_or(0);
                if now - asked < WANT_REPEAT_MS {
                    continue;
                }
                // 请把这条消息发来的那台设备应答；它是本机自己的旧身份时，改请缺的那
                // 条消息的作者。
                let to = if s.is_me(&node.origin) {
                    node.parent
                        .as_deref()
                        .and_then(ids::gid_device)
                        .filter(|device| !s.is_me(device))
                        .map(str::to_string)
                } else {
                    Some(node.origin.clone())
                };
                let Some(to) = to else {
                    continue;
                };
                map::enqueue(
                    tx,
                    &s.device,
                    None,
                    &Op::SessionWant {
                        sid: sid.to_string(),
                        gid: gid.clone(),
                        to,
                    },
                )?;
                db::state_put(
                    tx,
                    scope::WANT,
                    &key,
                    db::HLC_ZERO,
                    None,
                    Some(&now.to_string()),
                )?;
            }
            Ok(())
        })
    }

    /// 到期清理：悬空超过 30 天的 op 丢弃并提示；很久以前的请求记录清掉。
    fn expire(&mut self) -> Result<()> {
        let ctx = self.ctx;
        let now = now_ms();
        let dropped = db::write(ctx.db(), |tx| {
            let rows: Vec<(i64, String, String)> = {
                let mut stmt = tx.prepare_cached(
                    "SELECT id, origin, seg FROM sync_inbox WHERE state = ?1 ORDER BY id",
                )?;
                let rows = stmt.query_map(params![INBOX_ORPHAN], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut arrived: HashMap<String, i64> = HashMap::new();
            let mut dropped = 0usize;
            for (id, origin, seg) in rows {
                let key = pull::inseg_key(&origin, &seg);
                let at = match arrived.get(&key) {
                    Some(at) => *at,
                    None => {
                        let at = db::state_get(tx, scope::INSEG, &key)?
                            .and_then(|row| row.json::<InSeg>())
                            .map_or(now, |inseg| inseg.ts);
                        arrived.insert(key, at);
                        at
                    }
                };
                if now - at > ORPHAN_KEEP_MS {
                    tx.execute("DELETE FROM sync_inbox WHERE id = ?1", params![id])?;
                    dropped += 1;
                }
            }
            tx.execute(
                "DELETE FROM sync_state WHERE scope = ?1 AND CAST(value AS INTEGER) < ?2",
                params![scope::WANT, now - ORPHAN_KEEP_MS],
            )?;
            if dropped > 0 {
                note_lossy(tx)?;
            }
            Ok(dropped)
        })?;
        if dropped > 0 {
            status::notice(
                &ctx.paths,
                "orphans-expired",
                "orphans",
                "有消息因为一直缺少前文被丢弃",
                &format!(
                    "{dropped} 条来自其他设备的消息等了 30 天仍然接不上它们前面的内容，已经丢弃。\
                     如果那台设备上还有完整的会话，可以在那边导出会话包再导入到本机。"
                ),
            );
            self.report.status_changed = true;
        }
        Ok(())
    }

    /// 收件箱里已经没有 op 引用的段，它的依赖记录不用留了。
    fn sweep_insegs(&mut self) -> Result<()> {
        let ctx = self.ctx;
        let (used, known) = db::read(ctx.db(), |conn| {
            let mut stmt = conn.prepare_cached("SELECT DISTINCT origin, seg FROM sync_inbox")?;
            let rows = stmt.query_map([], |row| {
                Ok(pull::inseg_key(
                    &row.get::<_, String>(0)?,
                    &row.get::<_, String>(1)?,
                ))
            })?;
            let used = rows.collect::<rusqlite::Result<HashSet<String>>>()?;
            let known: Vec<String> = db::state_scan(conn, scope::INSEG, "")?
                .into_iter()
                .map(|(key, _)| key)
                .collect();
            Ok((used, known))
        })?;
        let stale: Vec<&String> = known.iter().filter(|key| !used.contains(*key)).collect();
        if stale.is_empty() {
            return Ok(());
        }
        db::write(ctx.db(), |tx| {
            for key in &stale {
                db::state_del(tx, scope::INSEG, key)?;
            }
            Ok(())
        })
    }
}

/// 悬空区里的一条消息（同一条消息可能从几台设备的日志各到一次）。
struct OrphanNode {
    rows: Vec<i64>,
    /// 把它发来的设备（最早到的那一行）。
    origin: String,
    parent: Option<String>,
}

#[derive(Deserialize)]
struct Peek {
    t: String,
    #[serde(default)]
    gid: Option<String>,
    #[serde(default)]
    parent: Option<String>,
}

/// 一个线程悬空区里的消息，按 `gid` 归并。
fn orphan_nodes(conn: &Connection, sid: &str) -> Result<BTreeMap<String, OrphanNode>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, origin, op FROM sync_inbox WHERE sid = ?1 AND state = ?2 ORDER BY id",
    )?;
    let mut rows = stmt.query(params![sid, INBOX_ORPHAN])?;
    let mut nodes: BTreeMap<String, OrphanNode> = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let origin: String = row.get(1)?;
        // 只取类型、`gid`、`parent` 三个字段，不把整条消息解析成树。
        let Ok(text) = row.get_ref(2)?.as_str() else {
            continue;
        };
        let Ok(peek) = serde_json::from_str::<Peek>(text) else {
            continue;
        };
        let (true, Some(gid)) = (peek.t == "msg", peek.gid) else {
            continue;
        };
        nodes
            .entry(gid)
            .or_insert_with(|| OrphanNode {
                rows: Vec::new(),
                origin,
                parent: peek.parent,
            })
            .rows
            .push(id);
    }
    Ok(nodes)
}

// ---------------------------------------------------------------------------
// 应答 `session.want`
// ---------------------------------------------------------------------------

/// 应答这一轮收到的分支重发请求（§7.5）：把含有被请求消息的本地分支写成
/// `session.bundle` 排进待发队列。被点名的设备应答；被点名的设备已经不在了（换了身份、
/// 很久没出现）时，其他持有这条分支的设备代为应答。同一条分支的同一个末条只重发一次。
/// 返回排进队列的 op 数。
///
/// 只重发分支已经发布的部分（[`bundle::published_tip`] 为止）：没发布的尾巴由日志照常
/// 送达，附件清单也要到封段时才有。所以不必等正在写的会话空下来。
pub fn answer_wants(
    ctx: &Ctx,
    s: &Settings,
    keys: &Keys,
    upload_attachments: bool,
) -> Result<usize> {
    let todos = db::read(ctx.db(), |conn| db::state_scan(conn, SCOPE_WANT_TODO, ""))?;
    let mut queued = 0usize;
    let now = now_ms();
    for (key, row) in todos {
        let (Some((sid, gid)), Some(todo)) = (key.split_once('|'), row.json::<WantTodo>()) else {
            db::write(ctx.db(), |tx| db::state_del(tx, SCOPE_WANT_TODO, &key))?;
            continue;
        };
        // 该不该由本机应答，以及哪些分支要重发。
        let branches: Vec<i64> = db::read(ctx.db(), |conn| {
            let mine = s.is_me(&todo.to) || {
                let peer = db::peer_state(conn, &todo.to)?;
                !peer.is_active(now)
            };
            if !mine || s.is_me(&todo.from) {
                return Ok(Vec::new());
            }
            let mut branches = Vec::new();
            for (session_id, message_id) in map::find_gid(conn, sid, gid)? {
                let Some(session) = map::session_map(conn, session_id)? else {
                    continue;
                };
                // 不参与同步的会话（通道不在名单里、仅本机）不往外发。
                if !publish::eligible(conn, s, &session.channel, &session.chat_id)? {
                    continue;
                }
                // 被请求的那条消息得在已经发布的部分里。
                let Some((upto, tip)) = bundle::published_tip(conn, session_id)? else {
                    continue;
                };
                if message_id > upto {
                    continue;
                }
                let tip_key = sync_db::tombstone_key(sid, Some(tip.as_str()));
                if db::state_get(conn, scope::BUNDLE_SENT, &tip_key)?.is_none() {
                    branches.push(session_id);
                }
            }
            Ok(branches)
        })?;
        for session_id in branches {
            let export = db::read(ctx.db(), |conn| {
                bundle::collect_published(conn, &ctx.env, &s.device, session_id)
            })?;
            // 这期间会话被删了：没有可发的。
            let Some(export) = export else {
                continue;
            };
            let tip_key = sync_db::tombstone_key(sid, export.header.tip.as_deref());
            let policy = ArtifactPolicy {
                inline_max: ARTIFACT_INLINE_MAX,
                message_max: OP_ARTIFACTS_MAX,
                keys: Some(keys),
            };
            let (chunks, blobs) = bundle::render_chunks(&ctx.env, export, policy, BUNDLE_CHUNK)?;
            let fresh_blob = if upload_attachments {
                BLOB_PENDING
            } else {
                BLOB_HELD
            };
            db::write(ctx.db(), |tx| {
                for chunk in &chunks {
                    map::enqueue(
                        tx,
                        &s.device,
                        None,
                        &Op::SessionBundle {
                            sid: sid.to_string(),
                            bundle: chunk.clone(),
                        },
                    )?;
                }
                for blob in &blobs {
                    db::blob_register(tx, &blob.blob_id, &blob.rel_path, blob.size, fresh_blob)?;
                }
                db::state_put(tx, scope::BUNDLE_SENT, &tip_key, db::HLC_ZERO, None, None)
            })?;
            queued += chunks.len();
        }
        db::write(ctx.db(), |tx| db::state_del(tx, SCOPE_WANT_TODO, &key))?;
    }
    Ok(queued)
}

// ---------------------------------------------------------------------------
// 隔离的线程（待处理事项）
// ---------------------------------------------------------------------------

/// 被隔离的线程：`(sid, 原因, 留在收件箱里的 op 数)`。
pub fn quarantined(conn: &Connection) -> Result<Vec<(String, Quarantine, u64)>> {
    let mut out = Vec::new();
    for (sid, row) in db::state_scan(conn, scope::QUARANTINE, "")? {
        let Some(info) = row.json::<Quarantine>() else {
            continue;
        };
        let held: i64 = conn.query_row(
            "SELECT count(*) FROM sync_inbox WHERE sid = ?1 AND state = ?2",
            params![sid, INBOX_QUARANTINED],
            |row| row.get(0),
        )?;
        out.push((sid, info, held.max(0) as u64));
    }
    Ok(out)
}

/// 处理一个被隔离的线程。`retry` 为真：解除隔离，留着的 op 重新处理一遍（升级之后也许
/// 就认得了）；为假：丢弃留着的 op 并解除隔离，这个线程以后的 op 照常处理。
pub fn resolve_quarantine(ctx: &Ctx, sid: &str, retry: bool) -> Result<()> {
    db::write(ctx.db(), |tx| {
        if retry {
            tx.execute(
                "UPDATE sync_inbox SET state = ?3 WHERE sid = ?1 AND state = ?2",
                params![sid, INBOX_QUARANTINED, INBOX_PENDING],
            )?;
        } else {
            let dropped = tx.execute(
                "DELETE FROM sync_inbox WHERE sid = ?1 AND state = ?2",
                params![sid, INBOX_QUARANTINED],
            )?;
            if dropped > 0 {
                note_lossy(tx)?;
            }
        }
        db::state_del(tx, scope::QUARANTINE, sid)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{self, Device};
    use rclaw_core::{Message, Role};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn texts(device: &Device, chat_id: &str) -> Vec<String> {
        device
            .sessions()
            .load_all("desktop", chat_id)
            .unwrap()
            .iter()
            .map(|message| {
                message
                    .content
                    .iter()
                    .filter_map(|block| block.as_text())
                    .collect::<String>()
            })
            .collect()
    }

    fn chats(device: &Device) -> Vec<String> {
        let mut chats: Vec<String> = device
            .sessions()
            .list_sessions(Some("desktop"))
            .unwrap()
            .into_iter()
            .map(|session| session.chat_id)
            .collect();
        chats.sort();
        chats
    }

    fn say(device: &Device, chat_id: &str, role: Role, text: &str) {
        device
            .sessions()
            .append("desktop", chat_id, &Message::plain(role, text))
            .unwrap();
    }

    #[test]
    fn a_session_travels_and_continues_on_the_other_device() {
        let a = Device::new();
        let b = Device::new();
        say(&a, "c1", Role::User, "一");
        say(&a, "c1", Role::Assistant, "二");
        a.sessions()
            .set_session_title("desktop", "c1", "行程")
            .unwrap();
        testing::ship(&a, &b);
        let report = b.merge();
        assert_eq!(texts(&b, "c1"), ["一", "二"]);
        assert!(report.list_changed);
        assert_eq!(
            b.sessions()
                .session_title("desktop", "c1")
                .unwrap()
                .as_deref(),
            Some("行程")
        );
        // 合入的内容不会被当成本机的改动再发回去。
        testing::ship(&b, &a);
        assert_eq!(a.merge().applied, 1, "只有 B 的 device.hello");

        // 在 B 上接着聊，回到 A：追加到同一个会话。
        say(&b, "c1", Role::User, "三");
        testing::ship(&b, &a);
        a.merge();
        assert_eq!(texts(&a, "c1"), ["一", "二", "三"]);
        assert_eq!(chats(&a), ["c1"]);
        // 重复送达同一批 op：结果不变。
        testing::replay(&b, &a);
        a.merge();
        assert_eq!(texts(&a, "c1"), ["一", "二", "三"]);
    }

    #[test]
    fn concurrent_continuations_become_a_branch_without_touching_the_local_one() {
        let a = Device::new();
        let b = Device::new();
        say(&a, "c1", Role::User, "一");
        testing::ship(&a, &b);
        b.merge();
        // 两边各自接着写。
        say(&a, "c1", Role::User, "A 的续写");
        say(&b, "c1", Role::User, "B 的续写");
        testing::ship(&a, &b);
        testing::ship(&b, &a);
        a.merge();
        b.merge();
        for device in [&a, &b] {
            let all = chats(device);
            assert_eq!(all.len(), 2, "{all:?}");
            let branch = all.iter().find(|chat| chat.starts_with("c1-b")).unwrap();
            let (mine, theirs) = if std::ptr::eq(device, &a) {
                ("A 的续写", "B 的续写")
            } else {
                ("B 的续写", "A 的续写")
            };
            assert_eq!(texts(device, "c1"), ["一", mine]);
            assert_eq!(texts(device, branch), ["一", theirs]);
        }
        // 分支上再来的消息追加到分支，不再分叉。
        say(&b, "c1", Role::Assistant, "B 的回复");
        testing::ship(&b, &a);
        a.merge();
        let branch = chats(&a)
            .into_iter()
            .find(|chat| chat.starts_with("c1-b"))
            .unwrap();
        assert_eq!(texts(&a, &branch), ["一", "B 的续写", "B 的回复"]);
        assert_eq!(chats(&a).len(), 2);
    }

    #[test]
    fn a_taken_key_lands_under_a_suffixed_key() {
        let a = Device::new();
        let b = Device::new();
        say(&a, "main", Role::User, "A 的主会话");
        say(&b, "main", Role::User, "B 的主会话");
        testing::ship(&a, &b);
        b.merge();
        let landed = format!("main-{}", &a.settings.device[..4]);
        assert_eq!(chats(&b), ["main".to_string(), landed.clone()]);
        assert_eq!(texts(&b, "main"), ["B 的主会话"]);
        assert_eq!(texts(&b, &landed), ["A 的主会话"]);
    }

    #[test]
    fn a_key_marked_local_only_is_never_reused_for_a_remote_thread() {
        let a = Device::new();
        let b = Device::new();
        // B 上这个键标过「仅本机」，会话后来删了：标记还留在键上。
        db::write(b.ctx.db(), |tx| {
            Ok(sync_db::kv_put(
                tx,
                &db::local_only_key("desktop", "c1"),
                "true",
            )?)
        })
        .unwrap();
        say(&a, "c1", Role::User, "A 的会话");
        testing::ship(&a, &b);
        b.merge();
        // 落到别的键上：不然它在 B 上成了「带着仅本机标记、却对应着 A 那个线程」的会话。
        let landed = format!("c1-{}", &a.settings.device[..4]);
        assert_eq!(chats(&b), [landed.clone()]);
        assert_eq!(texts(&b, &landed), ["A 的会话"]);
        db::read(b.ctx.db(), |conn| {
            assert!(publish::eligible(conn, &b.settings, "desktop", &landed)?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn delete_removes_only_branches_at_that_tip_and_keeps_a_trash_copy() {
        let a = Device::new();
        let b = Device::new();
        say(&a, "c1", Role::User, "一");
        say(&a, "c2", Role::User, "甲");
        testing::ship(&a, &b);
        b.merge();
        // B 在 c2 上接着写了一条，A 同时把两个会话都删了。
        say(&b, "c2", Role::User, "乙");
        a.sessions().delete_session("desktop", "c1").unwrap();
        a.sessions().delete_session("desktop", "c2").unwrap();
        testing::ship(&a, &b);
        let report = b.merge();
        assert!(report.list_changed);
        // c1 末条相同：删除并进回收站。c2 在 B 上已经往下写了：留着。
        assert_eq!(chats(&b), ["c2"]);
        assert_eq!(texts(&b, "c2"), ["甲", "乙"]);
        let trashed = trash::list(&b.ctx.paths);
        assert_eq!(trashed.len(), 1);
        assert_eq!(trashed[0].chat_id, "c1");
        // 重放 A 的日志（比如它换了身份重发）：已删除的分支不会复活。
        testing::replay(&a, &b);
        b.merge();
        assert_eq!(chats(&b), ["c2"]);
    }

    /// 会话在本机对应的线程，和它末条的 `gid`。
    fn thread_of(device: &Device, chat_id: &str) -> (String, String) {
        db::read(device.ctx.db(), |conn| {
            let session = map::session_map_by_key(conn, "desktop", chat_id)?.expect("会话的映射");
            let tip = true_tip(conn, session.session_id)?
                .flatten()
                .expect("末条的身份");
            Ok((session.sid, tip))
        })
        .unwrap()
    }

    fn stale_todo(device: &Device) -> Vec<String> {
        db::read(device.ctx.db(), |conn| {
            db::state_keys(conn, SCOPE_STALE_TODO, "")
        })
        .unwrap()
    }

    #[test]
    fn an_older_view_of_a_deleted_branch_is_removed_once_the_logs_are_read() {
        let a = Device::new();
        // 一台留着这条分支更早的样子，另一台在更早的样子上接着写了。
        let old = Device::new();
        let cont = Device::new();
        say(&a, "c1", Role::User, "一");
        say(&a, "c1", Role::Assistant, "二");
        say(&a, "c2", Role::User, "甲");
        testing::ship(&a, &old);
        testing::ship(&a, &cont);
        old.merge();
        cont.merge();
        // A 接着写了一条，然后把会话删了。另外两台没拿到那条续写（那段日志已经被回收，
        // 它们是从全量快照里知道这次删除的；或者它们是从旧备份恢复的），只拿到删除。
        say(&a, "c1", Role::User, "三");
        let lost = testing::seal_all(&a);
        a.sessions().delete_session("desktop", "c1").unwrap();
        say(&cont, "c1", Role::User, "四");
        testing::ship_after(&a, &old, lost);
        testing::ship_after(&a, &cont, lost);
        old.merge();
        cont.merge();
        // 末条对不上：合入时不删（后面的续写可能只是还没到），记下来等清点。
        assert_eq!(chats(&old), ["c1", "c2"]);
        assert_eq!(stale_todo(&old).len(), 1);
        let chains = db::read(old.ctx.db(), |conn| {
            db::state_scan(conn, scope::TOMB_CHAIN, "")
        })
        .unwrap();
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].1.value.as_deref().map(str::len), Some(3 * 16));
        // 各设备的日志读到头之后清点：更早的样子按删除处理，进回收站；别的线程不受影响。
        let sweep = sweep_stale(&old.ctx, &old.settings, false).unwrap();
        assert!(!sweep.pending && sweep.report.list_changed);
        assert_eq!(chats(&old), ["c2"]);
        assert_eq!(texts(&old, "c2"), ["甲"]);
        let trashed = trash::list(&old.ctx.paths);
        assert_eq!(trashed.len(), 1);
        assert_eq!(trashed[0].chat_id, "c1");
        assert!(stale_todo(&old).is_empty());
        // 把所有带链的墓碑再对一遍：没有可做的。
        let sweep = sweep_stale(&old.ctx, &old.settings, true).unwrap();
        assert!(!sweep.pending && !sweep.report.list_changed);

        // 接着写过的分支留下（续写优先于删除）：刚写下、还没有身份的末条不算在链上，
        // 发布之后它是本机自己的消息，也不在链上。
        assert!(stale_todo(&cont).is_empty());
        let sweep = sweep_stale(&cont.ctx, &cont.settings, true).unwrap();
        assert!(!sweep.pending && !sweep.report.list_changed);
        testing::seal_all(&cont);
        let sweep = sweep_stale(&cont.ctx, &cont.settings, true).unwrap();
        assert!(!sweep.pending && !sweep.report.list_changed);
        assert_eq!(texts(&cont, "c1"), ["一", "二", "四"]);
        assert!(trash::list(&cont.ctx.paths).is_empty());
    }

    #[test]
    fn an_older_view_that_arrives_after_the_deletion_is_removed_too() {
        let a = Device::new();
        let b = Device::new();
        let c = Device::new();
        say(&a, "c1", Role::User, "一");
        say(&a, "c1", Role::Assistant, "二");
        testing::ship(&a, &b);
        b.merge();
        let (sid, tip) = thread_of(&b, "c1");
        // A 接着写了一条，然后删了会话。C 只知道这次删除。
        say(&a, "c1", Role::User, "三");
        let lost = testing::seal_all(&a);
        a.sessions().delete_session("desktop", "c1").unwrap();
        testing::ship_after(&a, &c, lost);
        c.merge();
        assert!(chats(&c).is_empty());
        // B 还停在更早的样子上（续写和删除它都没读到），把这条分支重发了一遍——应答一个
        // 重发请求；它写的全量快照里的会话包到了 C 也是同样的情形。
        db::write(b.ctx.db(), |tx| {
            db::state_put_json(
                tx,
                SCOPE_WANT_TODO,
                &format!("{sid}|{tip}"),
                db::HLC_ZERO,
                &WantTodo {
                    to: b.settings.device.clone(),
                    from: c.settings.device.clone(),
                },
            )
        })
        .unwrap();
        assert_eq!(
            answer_wants(&b.ctx, &b.settings, &b.keys, false).unwrap(),
            1
        );
        testing::ship(&b, &c);
        c.merge();
        // 会话包照常落地（它也可能正是某条续写所缺的前半段），清点时再按删除处理。
        assert_eq!(texts(&c, "c1"), ["一", "二"]);
        let sweep = sweep_stale(&c.ctx, &c.settings, false).unwrap();
        assert!(!sweep.pending && sweep.report.list_changed);
        assert!(chats(&c).is_empty());
        assert_eq!(trash::list(&c.ctx.paths).len(), 1);
    }

    #[test]
    fn orphans_on_the_chain_of_a_deleted_branch_are_dropped_without_asking_for_it() {
        let a = Device::new();
        let b = Device::new();
        let c = Device::new();
        // B 开的会话；A 接着写了一条，然后删了。
        say(&b, "c1", Role::User, "一");
        let lost_b = testing::seal_all(&b);
        say(&b, "c1", Role::Assistant, "二");
        testing::ship(&b, &a);
        a.merge();
        say(&a, "c1", Role::User, "三");
        let lost_a = testing::seal_all(&a);
        a.sessions().delete_session("desktop", "c1").unwrap();
        // C 只拿到 B 的后一半（开头那段日志已经被回收）和 A 的删除。
        testing::ship_after(&b, &c, lost_b);
        testing::ship_after(&a, &c, lost_a);
        c.merge();
        // 「二」接不上，但它在被删分支的链上：丢弃，不去请谁把一条删掉的分支重发过来。
        assert!(chats(&c).is_empty());
        assert_eq!(inbox_len(&c), 0);
        assert!(testing::outbox(&c)
            .iter()
            .all(|op| op["t"] != "session.want"));
    }

    #[test]
    fn a_delete_with_a_malformed_chain_still_deletes_but_the_chain_is_not_used() {
        let a = Device::new();
        let b = Device::new();
        say(&a, "c1", Role::User, "一");
        testing::ship(&a, &b);
        b.merge();
        let (sid, tip) = thread_of(&b, "c1");
        a.sessions().delete_session("desktop", "c1").unwrap();
        // 链的最后一条不是末条。
        let tail = ids::gid_tail(&tip).unwrap().to_string();
        testing::ship_tampered(&a, &b, |line| {
            line.replace(
                &format!("\"chain\":\"{tail}\""),
                "\"chain\":\"00000000000000aa\"",
            )
        });
        let report = b.merge();
        assert!(report.list_changed);
        assert!(chats(&b).is_empty());
        db::read(b.ctx.db(), |conn| {
            let key = sync_db::tombstone_key(&sid, Some(&tip));
            assert!(db::state_get(conn, scope::TOMB, &key)?.is_some());
            assert!(db::state_get(conn, scope::TOMB_CHAIN, &key)?.is_none());
            // 和丢过 op 一样：本机不再写全量快照。
            assert!(db::state_get(conn, scope::SKIP, LOSSY_KEY)?.is_some());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_missing_parent_is_requested_and_healed_by_a_bundle() {
        let a = Device::new();
        let b = Device::new();
        say(&a, "c1", Role::User, "一");
        say(&a, "c1", Role::Assistant, "二");
        // 前面的段（开头信息与前两条）B 没有拿到，只收到后面的。
        let lost = testing::seal_all(&a);
        say(&a, "c1", Role::User, "三");
        testing::ship_after(&a, &b, lost);
        b.merge();
        assert!(chats(&b).is_empty());
        // B 发出请求。
        let wants: Vec<serde_json::Value> = testing::outbox(&b)
            .into_iter()
            .filter(|op| op["t"] == "session.want")
            .collect();
        assert_eq!(wants.len(), 1);
        assert_eq!(wants[0]["to"], a.settings.device.as_str());
        // 同一条链不重复请求。
        b.merge_full();
        assert_eq!(
            testing::outbox(&b)
                .iter()
                .filter(|op| op["t"] == "session.want")
                .count(),
            1
        );
        // A 应答：整条分支作为会话包重发。
        testing::ship(&b, &a);
        a.merge();
        assert_eq!(
            answer_wants(&a.ctx, &a.settings, &a.keys, false).unwrap(),
            1
        );
        assert_eq!(
            answer_wants(&a.ctx, &a.settings, &a.keys, false).unwrap(),
            0
        );
        testing::ship(&a, &b);
        b.merge();
        assert_eq!(texts(&b, "c1"), ["一", "二", "三"]);
        let orphans: i64 = db::read(b.ctx.db(), |conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM sync_inbox WHERE state = ?1",
                params![INBOX_ORPHAN],
                |row| row.get(0),
            )?)
        })
        .unwrap();
        assert_eq!(orphans, 0);
    }

    struct BusyGate(AtomicBool);

    impl crate::SessionGate for BusyGate {
        fn begin_merge(&self, _channel: &str, _chat_id: &str) -> Option<Box<dyn Send>> {
            (!self.0.load(Ordering::SeqCst)).then(|| Box::new(()) as Box<dyn Send>)
        }
        fn is_busy(&self, _channel: &str, _chat_id: &str) -> bool {
            self.0.load(Ordering::SeqCst)
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

    #[test]
    fn ops_for_a_busy_thread_queue_in_order_and_land_when_it_is_free() {
        let gate = Arc::new(BusyGate(AtomicBool::new(false)));
        let a = Device::new();
        let b = Device::with_gate(gate.clone());
        say(&a, "c1", Role::User, "一");
        testing::ship(&a, &b);
        b.merge();
        // B 上这个会话正在跑 turn。
        gate.0.store(true, Ordering::SeqCst);
        say(&a, "c1", Role::Assistant, "二");
        say(&a, "c1", Role::User, "三");
        testing::ship(&a, &b);
        let report = b.merge();
        assert!(report.queued);
        assert_eq!(texts(&b, "c1"), ["一"]);
        // turn 结束：按到达顺序接上。
        gate.0.store(false, Ordering::SeqCst);
        let report = b.merge();
        assert!(!report.queued);
        assert_eq!(texts(&b, "c1"), ["一", "二", "三"]);
    }

    #[test]
    fn the_stale_sweep_waits_for_a_busy_session() {
        let gate = Arc::new(BusyGate(AtomicBool::new(false)));
        let a = Device::new();
        let b = Device::with_gate(gate.clone());
        say(&a, "c1", Role::User, "一");
        testing::ship(&a, &b);
        b.merge();
        say(&a, "c1", Role::User, "二");
        let lost = testing::seal_all(&a);
        a.sessions().delete_session("desktop", "c1").unwrap();
        testing::ship_after(&a, &b, lost);
        b.merge();
        // B 上这个会话正在跑 turn：不动它，留到下一次。
        gate.0.store(true, Ordering::SeqCst);
        let sweep = sweep_stale(&b.ctx, &b.settings, true).unwrap();
        assert!(sweep.pending && !sweep.report.list_changed);
        assert_eq!(chats(&b), ["c1"]);
        gate.0.store(false, Ordering::SeqCst);
        let sweep = sweep_stale(&b.ctx, &b.settings, false).unwrap();
        assert!(!sweep.pending && sweep.report.list_changed);
        assert!(chats(&b).is_empty());
    }

    #[test]
    fn an_invalid_message_quarantines_its_thread_only() {
        let a = Device::new();
        let b = Device::new();
        say(&a, "bad", Role::User, "一");
        say(&a, "good", Role::User, "甲");
        // 把 bad 的消息改成通不过检查的形态（外层的 role 被改掉，与正文对不上）。
        testing::ship_tampered(&a, &b, |line| {
            if line.contains("\\\"text\\\":\\\"一\\\"") {
                line.replace("\"role\":\"user\"", "\"role\":\"system\"")
            } else {
                line.to_string()
            }
        });
        let report = b.merge();
        assert!(report.status_changed);
        assert_eq!(chats(&b), ["good"]);
        let held = db::read(b.ctx.db(), quarantined).unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].2, 1);
        // 丢弃之后线程解除隔离。
        resolve_quarantine(&b.ctx, &held[0].0, false).unwrap();
        assert!(db::read(b.ctx.db(), quarantined).unwrap().is_empty());
    }

    #[test]
    fn an_unknown_op_type_halts_that_log_until_it_is_understood() {
        let a = Device::new();
        let b = Device::new();
        say(&a, "c1", Role::User, "一");
        testing::ship_tampered(&a, &b, |line| {
            line.replace("\"t\":\"msg\"", "\"t\":\"msg.v9\"")
        });
        b.merge();
        assert!(chats(&b).is_empty());
        let peer = db::read(b.ctx.db(), |conn| db::peer_state(conn, &a.settings.device)).unwrap();
        assert!(peer
            .halted
            .as_deref()
            .is_some_and(|reason| reason.starts_with(HALT_UNKNOWN_OP)));
        assert!(peer.processed < peer.fetched);
    }

    fn inbox_len(device: &Device) -> i64 {
        db::read(device.ctx.db(), |conn| {
            Ok(conn.query_row("SELECT count(*) FROM sync_inbox", [], |row| row.get(0))?)
        })
        .unwrap()
    }

    #[test]
    fn a_device_without_the_sessions_scope_imports_no_sessions() {
        let a = Device::new();
        let mut b = Device::new();
        // CLI / Termux 这样的宿主：只收发文件类数据。
        b.settings.scopes = b.settings.scopes.without_sessions();
        say(&a, "c1", Role::User, "一");
        say(&a, "c1", Role::Assistant, "二");
        a.sessions()
            .set_session_title("desktop", "c1", "行程")
            .unwrap();
        // 已经发布过的会话再删除：日志里有 `session.delete`。
        testing::seal_all(&a);
        a.sessions().delete_session("desktop", "c1").unwrap();
        say(&a, "c2", Role::User, "甲");
        // 开头的段没拿到的线程：本来会悬空并请对方重发。
        say(&a, "c3", Role::User, "子");
        let lost = testing::seal_all(&a);
        say(&a, "c3", Role::Assistant, "丑");
        testing::ship_after(&a, &b, lost);
        let report = b.merge_full();
        assert!(report.chats.is_empty() && !report.list_changed);
        assert_eq!(inbox_len(&b), 0);
        assert!(testing::outbox(&b)
            .iter()
            .all(|op| op["t"] != "session.want"));
        // 整条日志（开头信息、消息、属性、删除）都到了也一样。
        testing::replay(&a, &b);
        let report = b.merge_full();
        assert!(report.chats.is_empty() && !report.list_changed);
        assert!(chats(&b).is_empty());
        // 看过就算：收件箱清空，回执照常推进，不请人重发。
        assert_eq!(inbox_len(&b), 0);
        let peer = db::read(b.ctx.db(), |conn| db::peer_state(conn, &a.settings.device)).unwrap();
        assert_eq!(peer.processed, peer.fetched);
        assert!(testing::outbox(&b)
            .iter()
            .all(|op| op["t"] != "session.want"));
        assert!(trash::list(&b.ctx.paths).is_empty());
    }

    #[test]
    fn threads_of_channels_outside_the_list_do_not_land() {
        let a = Device::new();
        let mut b = Device::new();
        b.settings.channels = vec!["telegram".into()];
        say(&a, "c1", Role::User, "一");
        say(&a, "c1", Role::Assistant, "二");
        testing::ship(&a, &b);
        b.merge_full();
        assert!(chats(&b).is_empty());
        assert_eq!(inbox_len(&b), 0);
        assert!(testing::outbox(&b)
            .iter()
            .all(|op| op["t"] != "session.want"));
        // 名单扩大之后把日志重读一遍（引擎的做法），线程照常落地。
        b.settings.channels = vec!["desktop".into()];
        testing::replay(&a, &b);
        b.merge_full();
        assert_eq!(texts(&b, "c1"), ["一", "二"]);
    }

    #[test]
    fn orphans_of_a_thread_are_dropped_once_it_turns_out_to_be_out_of_scope() {
        let a = Device::new();
        let mut b = Device::new();
        b.settings.channels = vec!["telegram".into()];
        say(&a, "c1", Role::User, "一");
        let lost = testing::seal_all(&a);
        say(&a, "c1", Role::Assistant, "二");
        // 先只到后面的消息：线程的通道还不知道，照常悬空。
        testing::ship_after(&a, &b, lost);
        b.merge();
        assert_eq!(inbox_len(&b), 1);
        // 开头信息到了：这个线程不在本机的范围里，悬空的那条随之丢弃。
        testing::replay(&a, &b);
        b.merge();
        assert_eq!(inbox_len(&b), 0);
        assert!(chats(&b).is_empty());
    }
}
