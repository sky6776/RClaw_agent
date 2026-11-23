//! 多端同步的库内结构（`docs/multi-device-sync.md` §12.2）。
//!
//! 同步表**不进 schema 迁移**：`SCHEMA_VERSION` 不变，表在首次用到时才建（开启同步，或
//! 第一次导出 / 导入会话包），它们自己的版本记在 kv 的 `sync/schema`。没用过这些功能的
//! 库里没有 `sync_*` 表，旧版本程序也照常打得开。
//!
//! 这里只放必须和会话写路径同事务的部分：建表、混合逻辑时钟（HLC）、设备身份的读取、
//! 删除会话时的队列清理与删除登记。同步引擎本身在 `rclaw-sync` crate，它用 [`Db`] 的
//! 事务直接读写这些表。
//!
//! [`Db`]: crate::Db

use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::{params, Connection, OptionalExtension};

use crate::db::now_ms;
use crate::Result;

/// 同步表自己的版本号（kv `sync/schema`）。
pub const SYNC_SCHEMA_VERSION: i64 = 1;

/// kv 键：同步表版本。存在即表已建。
pub const KV_SYNC_SCHEMA: &str = "sync/schema";
/// kv 键：HLC 上次发出的值（`<墙钟毫秒:013>-<计数:04x>`）。
pub const KV_SYNC_HLC: &str = "sync/hlc";
/// kv 键：设备身份 `{"id": "...", "past": [...], "instances": [...]}`。
pub const KV_SYNC_DEVICE: &str = "sync/device";

/// `sync_session.sealed` / `sync_msg.sealed`：已分配身份并进了待发队列，还没封段。
pub const SEAL_QUEUED: i64 = 0;
/// 已封段，或来自远端——别的设备可能见过。
pub const SEAL_SEALED: i64 = 1;
/// 只分配了身份、没进待发队列（导出 / 导入会话包时建立的映射）。下一次发布扫描会把它
/// 排进队列；没开同步的库里它一直停在这个状态。
pub const SEAL_LOCAL: i64 = 2;

/// `sync_inbox.state`：依赖的日志位置还没到，未处理。
pub const INBOX_PENDING: i64 = 0;
/// `sync_inbox.state`：所属线程正忙，按到达顺序排队。
pub const INBOX_QUEUED: i64 = 1;
/// `sync_inbox.state`：`parent` 本机没有，悬空。
pub const INBOX_ORPHAN: i64 = 2;
/// `sync_inbox.state`：校验失败，所属线程从这条起隔离。
pub const INBOX_QUARANTINED: i64 = 3;
/// `sync_inbox.state`：还没核验完的快照分片带来的 op（序号记为 0）。所属快照的分片全部
/// 对上清单之后，才与采用版本向量同一个事务转成 [`INBOX_PENDING`]；合入不看这个状态。
pub const INBOX_SNAP_STAGE: i64 = 4;

const SYNC_DDL: &str = "
CREATE TABLE IF NOT EXISTS sync_session (
  session_id INTEGER PRIMARY KEY REFERENCES session(id) ON DELETE CASCADE,
  sid        TEXT NOT NULL,
  tip_gid    TEXT,
  origin     TEXT NOT NULL,
  sealed     INTEGER NOT NULL DEFAULT 0,
  published  TEXT
);
CREATE INDEX IF NOT EXISTS sync_session_sid ON sync_session(sid);

CREATE TABLE IF NOT EXISTS sync_msg (
  message_id INTEGER PRIMARY KEY REFERENCES message(id) ON DELETE CASCADE,
  session_id INTEGER NOT NULL,
  gid        TEXT NOT NULL,
  parent_gid TEXT,
  sealed     INTEGER NOT NULL DEFAULT 0,
  refs       TEXT,
  wire_meta  TEXT,
  UNIQUE(session_id, gid)
);
CREATE INDEX IF NOT EXISTS sync_msg_gid ON sync_msg(gid);
CREATE INDEX IF NOT EXISTS sync_msg_session ON sync_msg(session_id, message_id);
CREATE INDEX IF NOT EXISTS sync_msg_unsealed ON sync_msg(message_id) WHERE sealed = 0;

CREATE TABLE IF NOT EXISTS sync_outbox (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER,
  hlc        TEXT NOT NULL,
  op         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS sync_outbox_session ON sync_outbox(session_id);

CREATE TABLE IF NOT EXISTS sync_inbox (
  id     INTEGER PRIMARY KEY AUTOINCREMENT,
  origin TEXT NOT NULL,
  seg    TEXT NOT NULL,
  seq    INTEGER NOT NULL,
  hlc    TEXT NOT NULL,
  sid    TEXT,
  op     TEXT NOT NULL,
  state  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sync_inbox_sid ON sync_inbox(sid, id);
CREATE INDEX IF NOT EXISTS sync_inbox_origin ON sync_inbox(origin, seq);

CREATE TABLE IF NOT EXISTS sync_state (
  scope TEXT NOT NULL,
  key   TEXT NOT NULL,
  hlc   TEXT NOT NULL,
  hash  TEXT,
  value TEXT,
  PRIMARY KEY(scope, key)
);

CREATE TABLE IF NOT EXISTS sync_blob (
  blob_id  TEXT PRIMARY KEY,
  rel_path TEXT NOT NULL,
  size     INTEGER NOT NULL,
  state    INTEGER NOT NULL
);
";

/// 这个进程见过建好了同步表的库。只会从假变真。
static TABLES_SEEN: AtomicBool = AtomicBool::new(false);

/// 这个进程见没见过建好了同步表的库：打开库时发现表已经在，或者在这个进程里建了表、
/// 查到了表。
///
/// 给每个 turn 都要走的路径用（[`crate::SessionStore::remote_origins`]）：从没开过多端
/// 同步、也没导入导出过会话包的进程里它一直是假，那条路径据此连库锁都不碰。它是整个
/// 进程一个记号，不分是哪个库——为真只表示「值得去库里看一眼」，看的那一眼才是准的。
///
/// 表是别的进程（和本进程共用一个数据目录）建的话，本进程要到下一次查表、或者重启之后
/// 才知道。
pub fn sync_tables_seen() -> bool {
    TABLES_SEEN.load(Ordering::Relaxed)
}

/// 同步表是否已建（一次 kv 主键查询）。
pub fn sync_tables_exist(conn: &Connection) -> Result<bool> {
    let exist = conn
        .query_row(
            "SELECT 1 FROM kv WHERE key = ?1",
            params![KV_SYNC_SCHEMA],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exist {
        TABLES_SEEN.store(true, Ordering::Relaxed);
    }
    Ok(exist)
}

/// 建同步表（幂等）。应在调用方的写事务里执行：DDL 与版本标记一起提交。
pub fn ensure_sync_tables(conn: &Connection) -> Result<()> {
    if sync_tables_exist(conn)? {
        return Ok(());
    }
    conn.execute_batch(SYNC_DDL)?;
    kv_put(conn, KV_SYNC_SCHEMA, &SYNC_SCHEMA_VERSION.to_string())?;
    // 事务还没提交就记下了：提交不成也只是让别处多查一次库。
    TABLES_SEEN.store(true, Ordering::Relaxed);
    Ok(())
}

/// 读 kv 原文（JSON 文本）。
pub fn kv_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM kv WHERE key = ?1", params![key], |row| {
            row.get(0)
        })
        .optional()?)
}

/// 写 kv 原文。`value` 必须是合法 JSON 文本（与 `JournalStore::kv_get` 的读法一致）。
pub fn kv_put(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO kv(key, value, updated_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![key, value, now_ms()],
    )?;
    Ok(())
}

pub fn kv_del(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM kv WHERE key = ?1", params![key])?;
    Ok(())
}

/// 「仅本机」标记的 kv 键（§7.9）：带这个标记的会话不参与多端同步。标记跟着会话键走，
/// 没开过同步也可以标。
pub fn local_only_key(channel: &str, chat_id: &str) -> String {
    format!("sync/local-only/{channel}/{chat_id}")
}

/// 本机当前的 `device_id`（没启用过同步为 `None`）。
pub fn device_id(conn: &Connection) -> Result<Option<String>> {
    Ok(device_ids(conn)?.into_iter().next())
}

/// 本机用过的全部 `device_id`：当前的在最前，其后是换身份之前的旧身份。
pub fn device_ids(conn: &Connection) -> Result<Vec<String>> {
    let Some(raw) = kv_get(conn, KV_SYNC_DEVICE)? else {
        return Ok(Vec::new());
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
        if !id.is_empty() {
            out.push(id.to_string());
        }
    }
    if out.is_empty() {
        return Ok(out);
    }
    if let Some(past) = value.get("past").and_then(|v| v.as_array()) {
        out.extend(past.iter().filter_map(|v| v.as_str()).map(str::to_string));
    }
    Ok(out)
}

/// 本机墙钟取值的上限（约 2255 年）：时钟错到这之后的设备也只取到它，离定宽写不下还远。
const HLC_NOW_MAX: i64 = 9_000_000_000_000;
/// 正常设备时钟可以偏快，但不能用临近格式极限的值推进整个空间的逻辑时钟。
const HLC_FUTURE_MS: i64 = 20 * 366 * 24 * 3600 * 1000;
const HLC_WALL_LIMIT: i64 = HLC_NOW_MAX + HLC_FUTURE_MS + 24 * 3600 * 1000;

fn hlc_last(conn: &Connection) -> Result<(i64, u32)> {
    let raw = kv_get(conn, KV_SYNC_HLC)?;
    Ok(raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<String>(raw).ok())
        .and_then(|text| hlc_parts(&text))
        .unwrap_or((0, 0)))
}

fn hlc_store(conn: &Connection, wall: i64, counter: u32) -> Result<()> {
    let text = format!("{wall:013}-{counter:04x}");
    kv_put(
        conn,
        KV_SYNC_HLC,
        &serde_json::Value::String(text).to_string(),
    )
}

/// 拆出 HLC 的 `(墙钟毫秒, 计数)`；设备段可有可无。格式不对返回 `None`。
pub fn hlc_parts(hlc: &str) -> Option<(i64, u32)> {
    let mut parts = hlc.splitn(3, '-');
    let wall = parts.next()?;
    let counter = parts.next()?;
    if wall.len() != 13
        || !wall.bytes().all(|b| b.is_ascii_digit())
        || counter.len() != 4
        || !counter
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let wall: i64 = wall.parse().ok()?;
    let counter = u32::from_str_radix(counter, 16).ok()?;
    (wall < HLC_WALL_LIMIT).then_some((wall, counter))
}

/// 发一个新的 HLC：`<墙钟毫秒:013>-<计数:04x>-<device_id>`。各段定宽，字典序就是先后
/// 顺序。墙钟回拨时沿用上次的毫秒值、只加计数，所以同一台设备发出的值严格递增。
/// 在调用方的写事务里执行（上次的值存在 kv 里）。
pub fn hlc_tick(conn: &Connection, device_id: &str) -> Result<String> {
    let now = now_ms().clamp(0, HLC_NOW_MAX);
    let (mut wall, mut counter) = hlc_last(conn)?;
    if now > wall {
        wall = now;
        counter = 0;
    } else if counter < 0xffff {
        counter += 1;
    } else if wall + 1 < HLC_WALL_LIMIT {
        wall += 1;
        counter = 0;
    } else {
        return Err(crate::StoreError::Other("同步逻辑时钟已耗尽".into()));
    }
    hlc_store(conn, wall, counter)?;
    Ok(format!("{wall:013}-{counter:04x}-{device_id}"))
}

/// 见到一个远端 HLC：本机的逻辑时钟不落后于它，之后本机发出的值都比它大。
///
/// 合规的远端值照常跟上，不按与本机墙钟的差距裁剪。各处的后写者胜都靠「本机的
/// 改动新于本机见过的每一条远端变更」：只跟到一半的话，一台时钟快了几天的设备改过的
/// 东西，别的设备在这几天里怎么改都盖不过它，本机刚改的值还会被它的旧值换回去。逻辑时钟
/// 只用来比先后，没有任何地方把它当真实时间用，跟着走快了没有别的影响。
pub fn hlc_observe(conn: &Connection, remote: &str) -> Result<()> {
    let Some(seen) = remote_hlc_parts(remote) else {
        return Ok(());
    };
    if seen > hlc_last(conn)? {
        hlc_store(conn, seen.0, seen.1)?;
    }
    Ok(())
}

/// 线上时钟允许正常的偏差和进位，但拒绝极端未来值。
pub fn remote_hlc_parts(hlc: &str) -> Option<(i64, u32)> {
    hlc_parts(hlc).filter(|&(wall, _)| wall <= now_ms().clamp(0, HLC_NOW_MAX) + HLC_FUTURE_MS)
}

/// 墓碑在 `sync_state` 里的键：`<sid>/<tip>`，`tip` 为空时是 `<sid>/`。
pub fn tombstone_key(sid: &str, tip: Option<&str>) -> String {
    format!("{sid}/{}", tip.unwrap_or(""))
}

/// 墓碑所带的「链」在 `sync_state` 里的 scope。键与墓碑相同（`<sid>/<tip>`），值是被删的
/// 那条分支从开头到 `tip` 各消息 `gid` 的随机尾巴（各 16 位十六进制），按先后连成一个串。
///
/// 墓碑只记着被删分支的末条。一台设备手里要是留着这条分支更早的样子（它是从旧备份恢复
/// 的，或者从一份更早的全量快照里拿到的），而中间那段续写又已经读不到了，光凭末条认不出
/// 那是同一条分支；有了链就认得出：它的末条在链上（§7.5）。
pub const SCOPE_TOMB_CHAIN: &str = "tomb-chain";
/// 链最多带这么多条消息（离 `tip` 最近的那些）：一条墓碑至多约 64 KB。
pub const TOMB_CHAIN_MAX: usize = 4096;

/// `gid`（`<device_id>.<16 位十六进制>`）的随机尾巴。形状不对返回 `None`。
pub fn gid_tail(gid: &str) -> Option<&str> {
    let (_, tail) = gid.split_once('.')?;
    let hex = tail.len() == 16
        && tail
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    hex.then_some(tail)
}

/// 一条分支到消息行 `upto` 为止的链（见 [`SCOPE_TOMB_CHAIN`]），只留离 `upto` 最近的
/// [`TOMB_CHAIN_MAX`] 条。不看这些消息封没封段：换过身份之后本机写的消息会回到「只分配了
/// 身份」，它们照样是这条分支的一部分。
fn branch_chain(conn: &Connection, session_id: i64, upto: i64) -> Result<String> {
    let mut stmt = conn.prepare_cached(
        "SELECT gid FROM sync_msg WHERE session_id = ?1 AND message_id <= ?2 \
         ORDER BY message_id DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![session_id, upto, TOMB_CHAIN_MAX as i64], |row| {
        row.get::<_, String>(0)
    })?;
    let gids = rows.collect::<rusqlite::Result<Vec<String>>>()?;
    let mut chain = String::with_capacity(gids.len() * 16);
    for gid in gids.iter().rev() {
        if let Some(tail) = gid_tail(gid) {
            chain.push_str(tail);
        }
    }
    Ok(chain)
}

/// 删除会话时的同步侧处理，必须和删除本身在同一个事务里（§7.5）：
///
/// 1. 这个会话还在待发队列里的 op 全部清掉——消息 op 引用的行马上就不存在了，这些 op
///    也没有任何设备见过；
/// 2. `tip` 取该分支最后一条「已封段或来自远端」的消息，一条都没有则为空；
/// 3. 登记 `session.delete` 并记下墓碑，连同这条分支到 `tip` 为止的链。会话连
///    `session.open` 都还没封段时，别的设备根本不知道它，不登记任何 op。
///
/// `propagate = false`：只清队列、不登记（合入远端发来的删除时用，避免来回弹）。
/// 没有同步表、这个会话没有线程映射、或本机没有同步身份时，什么都不做。
pub fn on_session_delete(
    conn: &Connection,
    session_id: i64,
    propagate: bool,
    purge: bool,
) -> Result<()> {
    if !sync_tables_exist(conn)? {
        return Ok(());
    }
    // 以本机会话行 id 为键的登记（分支标注、落地标注、暂停发布的位置）随会话一起删：
    // 行 id 以后可能被新会话重用，留着就会张冠李戴。
    conn.execute(
        "DELETE FROM sync_state WHERE scope IN ('branch', 'landed', 'pub-halt') AND key = ?1",
        params![session_id.to_string()],
    )?;
    let mapped: Option<(String, i64)> = conn
        .query_row(
            "SELECT sid, sealed FROM sync_session WHERE session_id = ?1",
            params![session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((sid, sealed)) = mapped else {
        return Ok(());
    };
    conn.execute(
        "DELETE FROM sync_outbox WHERE session_id = ?1",
        params![session_id],
    )?;
    if !propagate || sealed != SEAL_SEALED {
        return Ok(());
    }
    let Some(device) = device_id(conn)? else {
        return Ok(());
    };
    let tip: Option<(i64, String)> = conn
        .query_row(
            "SELECT message_id, gid FROM sync_msg WHERE session_id = ?1 AND sealed = ?2 \
             ORDER BY message_id DESC LIMIT 1",
            params![session_id, SEAL_SEALED],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let chain = match &tip {
        Some((upto, _)) => branch_chain(conn, session_id, *upto)?,
        None => String::new(),
    };
    let tip = tip.map(|(_, gid)| gid);
    let key = tombstone_key(&sid, tip.as_deref());
    let hlc = hlc_tick(conn, &device)?;
    let mut op = serde_json::json!({
        "t": "session.delete",
        "sid": sid,
        "tip": tip,
        "purge": purge,
    });
    if !chain.is_empty() {
        op["chain"] = serde_json::Value::String(chain.clone());
    }
    conn.execute(
        "INSERT INTO sync_outbox(session_id, hlc, op) VALUES (NULL, ?1, ?2)",
        params![hlc, op.to_string()],
    )?;
    conn.execute(
        "INSERT INTO sync_state(scope, key, hlc, hash, value) VALUES ('tomb', ?1, ?2, NULL, ?3) \
         ON CONFLICT(scope, key) DO UPDATE SET hlc = excluded.hlc, value = excluded.value \
         WHERE excluded.hlc > sync_state.hlc",
        params![key, hlc, serde_json::json!({ "purge": purge }).to_string()],
    )?;
    if !chain.is_empty() {
        // 同一个末条的链内容相同，只是可能截在不同的长度上：留长的。
        conn.execute(
            "INSERT INTO sync_state(scope, key, hlc, hash, value) VALUES (?1, ?2, ?3, NULL, ?4) \
             ON CONFLICT(scope, key) DO UPDATE SET hlc = excluded.hlc, value = excluded.value \
             WHERE sync_state.value IS NULL \
             OR length(excluded.value) > length(sync_state.value)",
            params![SCOPE_TOMB_CHAIN, key, hlc, chain],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.sqlite")).unwrap();
        (dir, db)
    }

    fn table_names(db: &Db) -> Vec<String> {
        db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'sync_%' \
                 ORDER BY name",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap()
    }

    #[test]
    fn fresh_db_has_no_sync_tables_and_keeps_schema_version() {
        let (_g, db) = db();
        assert!(table_names(&db).is_empty());
        assert!(!db.with(sync_tables_exist).unwrap());
        let version: i64 = db
            .with(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(version, crate::db::SCHEMA_VERSION);
    }

    #[test]
    fn ensure_sync_tables_is_idempotent_and_does_not_bump_schema_version() {
        let (_g, db) = db();
        db.transaction(|tx| ensure_sync_tables(tx)).unwrap();
        db.transaction(|tx| ensure_sync_tables(tx)).unwrap();
        assert_eq!(
            table_names(&db),
            [
                "sync_blob",
                "sync_inbox",
                "sync_msg",
                "sync_outbox",
                "sync_session",
                "sync_state"
            ]
        );
        assert!(db.with(sync_tables_exist).unwrap());
        // 建过表，进程里的记号就立起来了：每个 turn 都要走的路径据此才去查库。
        assert!(sync_tables_seen());
        let version: i64 = db
            .with(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(version, crate::db::SCHEMA_VERSION);
        // 旧版本程序（不认识这些表）重新打开同一个库不受影响。
        let path = db.path().to_path_buf();
        drop(db);
        Db::open(&path).unwrap();
    }

    #[test]
    fn hlc_is_strictly_increasing_and_follows_remote() {
        let (_g, db) = db();
        db.transaction(|tx| {
            let a = hlc_tick(tx, "dev1")?;
            let b = hlc_tick(tx, "dev1")?;
            assert!(b > a, "{a} {b}");
            // 远端在未来一小时：采纳后本机发出的值比它大。
            let future = format!("{:013}-0007-dev2", now_ms() + 3_600_000);
            hlc_observe(tx, &future)?;
            let c = hlc_tick(tx, "dev1")?;
            assert!(c > future, "{c} {future}");
            // 远端在一年之后：照样跟上去——本机之后的改动必须盖得过它。
            let far = format!("{:013}-0000-dev2", now_ms() + 365 * 24 * 3_600_000);
            hlc_observe(tx, &far)?;
            let d = hlc_tick(tx, "dev1")?;
            assert!(d > far, "{d} {far}");
            // 更早的远端值、格式不对的值都不影响时钟。
            hlc_observe(tx, &future)?;
            hlc_observe(tx, "garbage")?;
            let e = hlc_tick(tx, "dev1")?;
            assert!(e > d, "{e} {d}");
            // 顶到定宽上限的远端值不推进时钟。
            hlc_observe(tx, "9999999999999-ffff-dev2")?;
            let top = hlc_tick(tx, "dev1")?;
            assert!(top > e);
            assert_eq!(hlc_parts(&top).unwrap().0, hlc_parts(&far).unwrap().0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn hlc_parts_rejects_malformed_values() {
        assert_eq!(hlc_parts("0001763438400000-0001-dev"), None); // 墙钟 16 位
        assert_eq!(
            hlc_parts("1763438400000-00ff-dev"),
            Some((1_763_438_400_000, 255))
        );
        assert_eq!(
            hlc_parts("1763438400000-00ff"),
            Some((1_763_438_400_000, 255))
        );
        assert_eq!(hlc_parts("1763438400000"), None);
        assert_eq!(hlc_parts("176343840000x-0001-dev"), None);
        assert_eq!(hlc_parts("+763438400000-0001-dev"), None);
        assert_eq!(hlc_parts("1763438400000-00FF-dev"), None);
    }

    #[test]
    fn gid_tail_is_the_random_part() {
        assert_eq!(
            gid_tail("0123456789abcdef.00112233445566ff"),
            Some("00112233445566ff")
        );
        assert_eq!(gid_tail("0123456789abcdef"), None);
        assert_eq!(gid_tail("0123456789abcdef.0011"), None);
        assert_eq!(gid_tail("0123456789abcdef.00112233445566FF"), None);
    }

    /// 删除一个已经发布过的会话：登记的删除带着这条分支到 `tip` 为止的链，墓碑旁边也
    /// 存一份。链不看消息封没封段，但不包括 `tip` 之后还没发布的尾巴。
    #[test]
    fn a_propagated_delete_carries_the_chain_of_the_branch() {
        use crate::session::raw::SessionDeleteSync;
        use crate::{Layout, SessionStore};
        use rclaw_core::{Message, Role};

        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(Layout::new(dir.path().join("agent")));
        for text in ["一", "二", "三", "四"] {
            store
                .append("desktop", "c1", &Message::plain(Role::User, text))
                .unwrap();
        }
        let sid = "0123456789abcdef01234567";
        let device = "00112233445566ff";
        let gids: Vec<String> = (1..=4).map(|n| format!("{device}.{n:016x}")).collect();
        store
            .db()
            .transaction(|tx| {
                ensure_sync_tables(tx)?;
                kv_put(tx, KV_SYNC_DEVICE, &format!(r#"{{"id":"{device}"}}"#))?;
                let session_id: i64 = tx.query_row(
                    "SELECT id FROM session WHERE channel = 'desktop' AND chat_id = 'c1'",
                    [],
                    |row| row.get(0),
                )?;
                tx.execute(
                    "INSERT INTO sync_session(session_id, sid, tip_gid, origin, sealed, published) \
                     VALUES (?1, ?2, NULL, ?3, ?4, NULL)",
                    params![session_id, sid, device, SEAL_SEALED],
                )?;
                let ids: Vec<i64> = {
                    let mut stmt =
                        tx.prepare("SELECT id FROM message WHERE session_id = ?1 ORDER BY id")?;
                    let rows = stmt.query_map(params![session_id], |row| row.get(0))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                // 第一条回到了「只分配了身份」（换过身份），第二、三条别的设备可能见过，
                // 第四条还在待发队列里。
                let sealed = [SEAL_LOCAL, SEAL_SEALED, SEAL_SEALED, SEAL_QUEUED];
                for (index, id) in ids.iter().enumerate() {
                    tx.execute(
                        "INSERT INTO sync_msg(message_id, session_id, gid, parent_gid, sealed, \
                         refs, wire_meta) VALUES (?1, ?2, ?3, NULL, ?4, NULL, NULL)",
                        params![id, session_id, gids[index], sealed[index]],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        store
            .delete_session_with("desktop", "c1", SessionDeleteSync::Propagate)
            .unwrap();

        let chain = format!("{:016x}{:016x}{:016x}", 1, 2, 3);
        let (op, stored): (String, Option<String>) = store
            .db()
            .with(|c| {
                let op = c.query_row(
                    "SELECT op FROM sync_outbox ORDER BY id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )?;
                let stored = c
                    .query_row(
                        "SELECT value FROM sync_state WHERE scope = ?1 AND key = ?2",
                        params![SCOPE_TOMB_CHAIN, tombstone_key(sid, Some(&gids[2]))],
                        |r| r.get(0),
                    )
                    .optional()?;
                Ok((op, stored))
            })
            .unwrap();
        let op: serde_json::Value = serde_json::from_str(&op).unwrap();
        assert_eq!(op["t"], "session.delete");
        assert_eq!(op["tip"], gids[2].as_str());
        assert_eq!(op["chain"], chain.as_str());
        assert_eq!(stored.as_deref(), Some(chain.as_str()));
    }

    #[test]
    fn device_ids_lists_current_then_past() {
        let (_g, db) = db();
        db.transaction(|tx| {
            assert!(device_ids(tx)?.is_empty());
            kv_put(tx, KV_SYNC_DEVICE, r#"{"id":"cur","past":["old1","old2"]}"#)?;
            assert_eq!(device_ids(tx)?, ["cur", "old1", "old2"]);
            assert_eq!(device_id(tx)?.as_deref(), Some("cur"));
            Ok(())
        })
        .unwrap();
    }
}
