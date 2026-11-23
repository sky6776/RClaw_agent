//! 本机 id ↔ 全局身份的映射（`sync_session`、`sync_msg`，§6）与发布扫描的单会话部分（§7.1）。
//!
//! 这里的函数都在调用方的事务里执行。

use rclaw_store::session::raw;
use rclaw_store::sync_db::{self, SEAL_LOCAL, SEAL_QUEUED};
use rusqlite::{params, Connection, OptionalExtension};

use crate::db::{self, scope};
use crate::error::Result;
use crate::ids;
use crate::op::{ForkOf, Op};

/// 一个本地会话与它所属线程的对应关系。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMap {
    pub session_id: i64,
    pub channel: String,
    pub chat_id: String,
    pub sid: String,
    /// 该分支末条消息的 `gid`。
    pub tip: Option<String>,
    /// 创建这个线程的设备。
    pub origin: String,
    pub sealed: i64,
    /// 上次发布的属性值（JSON）。
    pub published: Option<String>,
}

const SESSION_MAP_SQL: &str = "SELECT y.session_id, s.channel, s.chat_id, y.sid, y.tip_gid, \
     y.origin, y.sealed, y.published FROM sync_session y JOIN session s ON s.id = y.session_id";

fn row_to_session_map(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMap> {
    Ok(SessionMap {
        session_id: row.get(0)?,
        channel: row.get(1)?,
        chat_id: row.get(2)?,
        sid: row.get(3)?,
        tip: row.get(4)?,
        origin: row.get(5)?,
        sealed: row.get(6)?,
        published: row.get(7)?,
    })
}

pub fn session_map(conn: &Connection, session_id: i64) -> Result<Option<SessionMap>> {
    Ok(conn
        .query_row(
            &format!("{SESSION_MAP_SQL} WHERE y.session_id = ?1"),
            params![session_id],
            row_to_session_map,
        )
        .optional()?)
}

pub fn session_map_by_key(
    conn: &Connection,
    channel: &str,
    chat_id: &str,
) -> Result<Option<SessionMap>> {
    Ok(conn
        .query_row(
            &format!("{SESSION_MAP_SQL} WHERE s.channel = ?1 AND s.chat_id = ?2"),
            params![channel, chat_id],
            row_to_session_map,
        )
        .optional()?)
}

/// 一个线程在本机的全部分支（按会话行 id 升序）。
pub fn thread_sessions(conn: &Connection, sid: &str) -> Result<Vec<SessionMap>> {
    let mut stmt = conn.prepare_cached(&format!(
        "{SESSION_MAP_SQL} WHERE y.sid = ?1 ORDER BY y.session_id"
    ))?;
    let rows = stmt.query_map(params![sid], row_to_session_map)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 全部有映射的会话。
pub fn all_sessions(conn: &Connection) -> Result<Vec<SessionMap>> {
    let mut stmt = conn.prepare_cached(&format!("{SESSION_MAP_SQL} ORDER BY y.session_id"))?;
    let rows = stmt.query_map([], row_to_session_map)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 一条消息的映射。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsgMap {
    pub message_id: i64,
    pub gid: String,
    pub parent: Option<String>,
    pub sealed: i64,
    pub refs: Option<String>,
    pub wire_meta: Option<String>,
}

fn row_to_msg_map(row: &rusqlite::Row<'_>) -> rusqlite::Result<MsgMap> {
    Ok(MsgMap {
        message_id: row.get(0)?,
        gid: row.get(1)?,
        parent: row.get(2)?,
        sealed: row.get(3)?,
        refs: row.get(4)?,
        wire_meta: row.get(5)?,
    })
}

const MSG_MAP_COLUMNS: &str = "message_id, gid, parent_gid, sealed, refs, wire_meta";

pub fn msg_map(conn: &Connection, message_id: i64) -> Result<Option<MsgMap>> {
    Ok(conn
        .query_row(
            &format!("SELECT {MSG_MAP_COLUMNS} FROM sync_msg WHERE message_id = ?1"),
            params![message_id],
            row_to_msg_map,
        )
        .optional()?)
}

/// 一个会话全部消息的映射，按消息行 id 升序（= 链的顺序）。
pub fn session_msg_maps(conn: &Connection, session_id: i64) -> Result<Vec<MsgMap>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {MSG_MAP_COLUMNS} FROM sync_msg WHERE session_id = ?1 ORDER BY message_id"
    ))?;
    let rows = stmt.query_map(params![session_id], row_to_msg_map)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 某个会话里 `gid` 对应的本机消息行 id。
pub fn message_of(conn: &Connection, session_id: i64, gid: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT message_id FROM sync_msg WHERE session_id = ?1 AND gid = ?2",
            params![session_id, gid],
            |row| row.get(0),
        )
        .optional()?)
}

/// 某个会话里一条消息的 `gid`。
pub fn gid_of(conn: &Connection, session_id: i64, message_id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT gid FROM sync_msg WHERE session_id = ?1 AND message_id = ?2",
            params![session_id, message_id],
            |row| row.get(0),
        )
        .optional()?)
}

/// `gid` 在线程 `sid` 的哪些本地会话里：`(会话行 id, 消息行 id)`，按会话行 id 升序。
/// `gid` 只在线程内唯一（恢复出的会话是新线程、`gid` 不变），所以一律按线程查。
pub fn find_gid(conn: &Connection, sid: &str, gid: &str) -> Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT m.session_id, m.message_id FROM sync_msg m \
         JOIN sync_session y ON y.session_id = m.session_id \
         WHERE m.gid = ?2 AND y.sid = ?1 ORDER BY m.session_id",
    )?;
    let rows = stmt.query_map(params![sid, gid], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 会话最后一条消息的行 id（没有消息为 `None`）。
pub fn last_message_id(conn: &Connection, session_id: i64) -> Result<Option<i64>> {
    Ok(conn.query_row(
        "SELECT MAX(id) FROM message WHERE session_id = ?1",
        params![session_id],
        |row| row.get(0),
    )?)
}

pub fn set_tip(conn: &Connection, session_id: i64, tip: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE sync_session SET tip_gid = ?2 WHERE session_id = ?1",
        params![session_id, tip],
    )?;
    Ok(())
}

pub fn insert_session_map(
    conn: &Connection,
    session_id: i64,
    sid: &str,
    origin: &str,
    sealed: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_session(session_id, sid, tip_gid, origin, sealed, published) \
         VALUES (?1, ?2, NULL, ?3, ?4, NULL)",
        params![session_id, sid, origin, sealed],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn insert_msg_map(
    conn: &Connection,
    message_id: i64,
    session_id: i64,
    gid: &str,
    parent: Option<&str>,
    sealed: i64,
    refs: Option<&str>,
    wire_meta: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_msg(message_id, session_id, gid, parent_gid, sealed, refs, wire_meta) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![message_id, session_id, gid, parent, sealed, refs, wire_meta],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 待发队列
// ---------------------------------------------------------------------------

/// 把一条 op 排进待发队列，返回它的 HLC。`session_id` 只给「随会话删除一起作废」的 op
/// 填（消息、`session.open`、属性）。
pub fn enqueue(
    conn: &Connection,
    device: &str,
    session_id: Option<i64>,
    op: &Op,
) -> Result<String> {
    enqueue_text(conn, device, session_id, &serde_json::to_string(op)?)
}

/// 同 [`enqueue`]，op 已经序列化好了（`serde_json::to_string(&Op)` 的结果）。大的 op 在
/// 库锁之外序列化，事务里只是把文本写进去。
pub fn enqueue_text(
    conn: &Connection,
    device: &str,
    session_id: Option<i64>,
    op: &str,
) -> Result<String> {
    let hlc = sync_db::hlc_tick(conn, device)?;
    conn.execute(
        "INSERT INTO sync_outbox(session_id, hlc, op) VALUES (?1, ?2, ?3)",
        params![session_id, hlc, op],
    )?;
    Ok(hlc)
}

/// 把一条消息的行引用排进待发队列。消息体到封段时才读出来。
fn enqueue_row_ref(
    conn: &Connection,
    device: &str,
    session_id: i64,
    message_id: i64,
) -> Result<()> {
    let hlc = sync_db::hlc_tick(conn, device)?;
    conn.execute(
        "INSERT INTO sync_outbox(session_id, hlc, op) VALUES (?1, ?2, ?3)",
        params![
            session_id,
            hlc,
            serde_json::json!({ "t": "msg", "ref": message_id }).to_string()
        ],
    )?;
    Ok(())
}

pub fn outbox_len(conn: &Connection) -> Result<u64> {
    Ok(
        conn.query_row("SELECT count(*) FROM sync_outbox", [], |row| {
            row.get::<_, i64>(0)
        })? as u64,
    )
}

// ---------------------------------------------------------------------------
// 线程的 `session.open` 信息
// ---------------------------------------------------------------------------

/// 线程的开头信息（`sync_state` 的 `open`）：远端线程落地、导入的会话包再发布时用。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OpenInfo {
    pub channel: String,
    pub chat_id: String,
    /// Unix 毫秒。
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_of: Option<ForkOf>,
    /// 创建线程的设备。
    pub origin: String,
}

pub fn open_info(conn: &Connection, sid: &str) -> Result<Option<OpenInfo>> {
    Ok(db::state_get(conn, scope::OPEN, sid)?.and_then(|row| row.json()))
}

/// 开头信息行上的记号（`hash` 列）：这份信息是本机导入会话包时记下的，不是从别的设备的
/// 日志里读到的。
pub const OPEN_LOCAL: &str = "local";

/// 线程有没有来自别的设备的开头信息（本机导入会话包时记下的不算）。有 = 这个线程在
/// 别的设备的日志里出现过。
pub fn has_remote_open(conn: &Connection, sid: &str) -> Result<bool> {
    Ok(db::state_get(conn, scope::OPEN, sid)?
        .is_some_and(|row| row.hash.as_deref() != Some(OPEN_LOCAL)))
}

/// 记下从别的设备的日志里读到的开头信息。已经有了就保留先到的那一份；但那一份是本机
/// 导入会话包时记的，就换成远端的（记号也随之去掉）——各设备对同一个线程记着同样的
/// 开头信息，线程落在哪个通道才一致。
pub fn put_open_info(conn: &Connection, sid: &str, info: &OpenInfo) -> Result<()> {
    if !has_remote_open(conn, sid)? {
        db::state_put_json(conn, scope::OPEN, sid, db::HLC_ZERO, info)?;
    }
    Ok(())
}

/// 同 [`put_open_info`]，但标明它来自本机导入的会话包。导入的会话在发布之前只属于本机，
/// 它的开头信息不随全量快照出去（§9.5）。
pub fn put_local_open_info(conn: &Connection, sid: &str, info: &OpenInfo) -> Result<()> {
    if db::state_get(conn, scope::OPEN, sid)?.is_none() {
        db::state_put(
            conn,
            scope::OPEN,
            sid,
            db::HLC_ZERO,
            Some(OPEN_LOCAL),
            Some(&serde_json::to_string(info)?),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 发布扫描：单个会话
// ---------------------------------------------------------------------------

/// `meta` 是否是两种已知形态之一（§7.2）。
pub fn is_known_meta(meta: &str) -> bool {
    raw::compaction_anchors(meta).is_some() || raw::fork_notice_anchor(meta).is_some()
}

/// 这个会话因为出现了不认识的 `meta` 形态而暂停发布的位置（`sync_state` 的 `pub-halt`）。
pub const SCOPE_PUB_HALT: &str = "pub-halt";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MapOutcome {
    /// 新分配了身份的消息数。
    pub mapped: usize,
    /// 排进待发队列的 op 数。
    pub queued: usize,
}

/// 给一个会话建立（或补全）线程映射：没有映射就分配 `sid`，再给还没有 `gid` 的行逐行分配，
/// `parent` 取上一行的 `gid`。
///
/// `queue = true` 是发布扫描：新分配的和此前「只分配了身份」的都排进待发队列。
/// `queue = false` 只分配身份（导出会话包时用），留给以后的发布扫描去排队。
pub fn ensure_mapped(
    conn: &Connection,
    device: &str,
    session_id: i64,
    queue: bool,
) -> Result<MapOutcome> {
    let mut outcome = MapOutcome::default();
    let Some((channel, chat_id)) = raw::session_key(conn, session_id)? else {
        return Ok(outcome);
    };
    let fresh = if queue { SEAL_QUEUED } else { SEAL_LOCAL };
    let (current, created) = match session_map(conn, session_id)? {
        Some(current) => (current, false),
        None => {
            insert_session_map(conn, session_id, &ids::new_sid(), device, SEAL_LOCAL)?;
            (session_map(conn, session_id)?.expect("刚插入的映射"), true)
        }
    };
    if queue && (created || current.sealed == SEAL_LOCAL) {
        // `session.open` 还没排过队（刚建的映射，或导出 / 导入会话包时建的）。
        let created_at: i64 = conn.query_row(
            "SELECT created_at FROM session WHERE id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        let fork_of = fork_of(conn, &current, &channel, &chat_id)?;
        enqueue(
            conn,
            device,
            Some(session_id),
            &Op::SessionOpen {
                sid: current.sid.clone(),
                channel: channel.clone(),
                chat_id: chat_id.clone(),
                created_at,
                fork_of,
            },
        )?;
        outcome.queued += 1;
        conn.execute(
            "UPDATE sync_session SET sealed = ?2 WHERE session_id = ?1",
            params![session_id, SEAL_QUEUED],
        )?;
    }
    if queue {
        // 此前只分配了身份的行先排队：它们在链上排在还没映射的行之前。
        let local_rows: Vec<i64> = {
            let mut stmt = conn.prepare_cached(
                "SELECT message_id FROM sync_msg WHERE session_id = ?1 AND sealed = ?2 \
                 ORDER BY message_id",
            )?;
            let rows = stmt.query_map(params![session_id, SEAL_LOCAL], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for message_id in &local_rows {
            enqueue_row_ref(conn, device, session_id, *message_id)?;
        }
        if !local_rows.is_empty() {
            conn.execute(
                "UPDATE sync_msg SET sealed = ?3 WHERE session_id = ?1 AND sealed = ?2",
                params![session_id, SEAL_LOCAL, SEAL_QUEUED],
            )?;
            outcome.queued += local_rows.len();
        }
    }
    // 还没有映射的行一定是会话的一段尾巴：行只追加，合入远端消息前也先补做这一步。
    let unmapped: Vec<(i64, Option<String>)> = {
        let mut stmt = conn.prepare_cached(
            "SELECT id, meta FROM message WHERE session_id = ?1 AND id > COALESCE( \
               (SELECT MAX(message_id) FROM sync_msg WHERE session_id = ?1), 0) ORDER BY id",
        )?;
        let rows = stmt.query_map(params![session_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut tip = current.tip.clone();
    let mut halted = false;
    for (message_id, meta) in unmapped {
        if meta.as_deref().is_some_and(|meta| !is_known_meta(meta)) {
            // 这个版本表达不了的行：这个会话从这一行起暂停发布（§9.4）。
            db::state_put(
                conn,
                SCOPE_PUB_HALT,
                &session_id.to_string(),
                db::HLC_ZERO,
                None,
                Some(&message_id.to_string()),
            )?;
            halted = true;
            break;
        }
        let gid = ids::new_gid(device);
        insert_msg_map(
            conn,
            message_id,
            session_id,
            &gid,
            tip.as_deref(),
            fresh,
            None,
            None,
        )?;
        if queue {
            enqueue_row_ref(conn, device, session_id, message_id)?;
            outcome.queued += 1;
        }
        outcome.mapped += 1;
        tip = Some(gid);
    }
    if !halted {
        // 尾巴上没有（或者不再有）表达不了的行——升级到认得那种形态的版本之后，上面已经
        // 把停在那里的行接着分配了身份：暂停的标记撤掉，面板上的那条事项跟着消失，这个
        // 会话此后照常发布。标记留着的话，两轮之间的检查会一直把它当成「发不出去」。
        db::state_del(conn, SCOPE_PUB_HALT, &session_id.to_string())?;
    }
    if tip != current.tip {
        set_tip(conn, session_id, tip.as_deref())?;
    }
    Ok(outcome)
}

/// 会话的分叉来源，用线程身份表达：`/fork` 的血缘（父会话在本机且锚点已有 `gid`），
/// 否则沿用这个线程开头信息里记着的那一份。
pub(crate) fn fork_of(
    conn: &Connection,
    current: &SessionMap,
    channel: &str,
    chat_id: &str,
) -> Result<Option<ForkOf>> {
    if let Some((parent_chat, anchor)) = raw::fork_lineage(conn, channel, chat_id)? {
        if let Some(parent) = session_map_by_key(conn, channel, &parent_chat)? {
            if let Some(gid) = gid_of(conn, parent.session_id, anchor)? {
                return Ok(Some(ForkOf {
                    sid: parent.sid,
                    gid,
                }));
            }
        }
    }
    Ok(open_info(conn, &current.sid)?.and_then(|info| info.fork_of))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rclaw_core::{Message, Role};
    use rclaw_store::{Layout, SessionStore};

    fn store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(Layout::new(dir.path())).unwrap();
        (dir, store)
    }

    fn session_row(store: &SessionStore, chat_id: &str) -> i64 {
        db::read(store.db(), |c| {
            Ok(raw::find_session(c, "desktop", chat_id)?.expect("会话存在"))
        })
        .unwrap()
    }

    fn outbox(store: &SessionStore) -> Vec<serde_json::Value> {
        db::read(store.db(), |c| {
            let mut stmt = c.prepare("SELECT op FROM sync_outbox ORDER BY id")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            Ok(rows
                .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
                .collect())
        })
        .unwrap()
    }

    #[test]
    fn export_mapping_is_promoted_by_the_publish_scan_in_chain_order() {
        let (_g, store) = store();
        let a = store
            .append("desktop", "c1", &Message::plain(Role::User, "一"))
            .unwrap();
        let b = store
            .append("desktop", "c1", &Message::plain(Role::Assistant, "二"))
            .unwrap();
        let session = session_row(&store, "c1");

        // 只分配身份：不进队列。
        let device = db::write(store.db(), |tx| {
            let device = db::ensure_device(tx)?;
            let outcome = ensure_mapped(tx, &device, session, false)?;
            assert_eq!(
                outcome,
                MapOutcome {
                    mapped: 2,
                    queued: 0
                }
            );
            Ok(device)
        })
        .unwrap();
        assert!(outbox(&store).is_empty());
        let maps = db::read(store.db(), |c| session_msg_maps(c, session)).unwrap();
        assert_eq!(maps.len(), 2);
        assert_eq!(maps[0].parent, None);
        assert_eq!(maps[1].parent.as_deref(), Some(maps[0].gid.as_str()));
        assert!(maps.iter().all(|m| m.sealed == SEAL_LOCAL));

        // 之后又来了一条，发布扫描：open、前两条、新的一条，按链的顺序排队，身份不变。
        let c = store
            .append("desktop", "c1", &Message::plain(Role::User, "三"))
            .unwrap();
        db::write(store.db(), |tx| {
            let outcome = ensure_mapped(tx, &device, session, true)?;
            assert_eq!(
                outcome,
                MapOutcome {
                    mapped: 1,
                    queued: 4
                }
            );
            // 再扫一次什么都不做。
            assert_eq!(
                ensure_mapped(tx, &device, session, true)?,
                MapOutcome::default()
            );
            Ok(())
        })
        .unwrap();
        let ops = outbox(&store);
        assert_eq!(ops[0]["t"], "session.open");
        let refs: Vec<i64> = ops[1..]
            .iter()
            .map(|op| op["ref"].as_i64().unwrap())
            .collect();
        assert_eq!(refs, [a, b, c]);
        let after = db::read(store.db(), |c| session_msg_maps(c, session)).unwrap();
        assert_eq!(after[0].gid, maps[0].gid);
        assert_eq!(after[2].parent.as_deref(), Some(maps[1].gid.as_str()));
        assert!(after.iter().all(|m| m.sealed == SEAL_QUEUED));
        let map = db::read(store.db(), |c| session_map(c, session))
            .unwrap()
            .unwrap();
        assert_eq!(map.tip.as_deref(), Some(after[2].gid.as_str()));
        assert_eq!(map.sealed, SEAL_QUEUED);
        assert_eq!(
            db::read(store.db(), |c| find_gid(c, &map.sid, &after[1].gid)).unwrap(),
            [(session, b)]
        );
    }

    #[test]
    fn unknown_meta_halts_publishing_at_that_row_until_it_can_be_expressed() {
        let (_g, store) = store();
        store
            .append("desktop", "c1", &Message::plain(Role::User, "一"))
            .unwrap();
        let session = session_row(&store, "c1");
        db::write(store.db(), |tx| {
            let device = db::ensure_device(tx)?;
            raw::insert_raw_message(
                tx,
                session,
                "user",
                1,
                r#"{"role":"user","content":[]}"#,
                Some(r#"{"future":{}}"#),
                &[],
            )?;
            raw::insert_raw_message(
                tx,
                session,
                "user",
                2,
                r#"{"role":"user","content":[]}"#,
                None,
                &[],
            )?;
            let outcome = ensure_mapped(tx, &device, session, true)?;
            assert_eq!(outcome.mapped, 1);
            assert!(db::state_get(tx, SCOPE_PUB_HALT, &session.to_string())?.is_some());
            // 再扫还是停在那一行：标记留着。
            assert_eq!(
                ensure_mapped(tx, &device, session, true)?,
                MapOutcome::default()
            );
            assert!(db::state_get(tx, SCOPE_PUB_HALT, &session.to_string())?.is_some());

            // 那一行变得表达得了了（现实里是升级到了认得这种形态的版本）：剩下的两行接着
            // 发布，暂停的标记撤掉。
            tx.execute(
                "UPDATE message SET meta = NULL WHERE session_id = ?1 AND meta IS NOT NULL",
                params![session],
            )?;
            let outcome = ensure_mapped(tx, &device, session, true)?;
            assert_eq!((outcome.mapped, outcome.queued), (2, 2));
            assert!(db::state_get(tx, SCOPE_PUB_HALT, &session.to_string())?.is_none());
            Ok(())
        })
        .unwrap();
    }
}
