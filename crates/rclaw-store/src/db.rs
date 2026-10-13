//! `agent/state.sqlite`：机器状态单库（连接、PRAGMA、schema 迁移、事务封装）。
//!
//! 设计参考 ZCode 的 `sqlite-session-store`：
//! - 单文件 WAL 库，`busy_timeout` 兜底同进程多连接；
//! - 写事务一律 `BEGIN IMMEDIATE`（拿写锁后再读，避免升级死锁）；
//! - schema 以 `PRAGMA user_version` 版本化；RClaw 没有旧数据，v1 一次建全。
//!
//! 库里只放「机器写入的运行状态」：会话消息、rewind 文件 checkpoint、usage 锚点、
//! PlanRun / WorkflowRun / 团队 journal、团队收件箱。给人读改的文件
//! （MEMORY.md、daily、SOUL/USER、cron.json、skills）仍留在文件系统。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::Result;

/// 当前 schema 版本（`PRAGMA user_version`）。
pub const SCHEMA_VERSION: i64 = 2;

/// v1 → v2：`file_checkpoint` 加 `anchor_message_id` 列（checkpoint 与同事务写入的
/// tool_result 消息行的关联；「fork + 文件回滚到锚点」按它切回滚集。旧行 NULL =
/// 迁移前的记录，归属未知，按消息锚点回滚时不参与）。
const SCHEMA_V2_MIGRATION: &str = "ALTER TABLE file_checkpoint ADD COLUMN anchor_message_id INTEGER;";

/// v1 schema。字段语义见 `docs/superpowers/specs/2026-10-12-sqlite-state-store-design.md`。
const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS session (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  channel     TEXT    NOT NULL,
  chat_id     TEXT    NOT NULL,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL,
  archived_at INTEGER,
  UNIQUE(channel, chat_id)
);

CREATE TABLE IF NOT EXISTS message (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER NOT NULL REFERENCES session(id) ON DELETE CASCADE,
  role       TEXT    NOT NULL,
  ts         INTEGER NOT NULL,
  data       TEXT    NOT NULL,
  meta       TEXT
);
CREATE INDEX IF NOT EXISTS message_session_idx ON message(session_id, id);
CREATE INDEX IF NOT EXISTS message_boundary_idx ON message(session_id, id) WHERE meta IS NOT NULL;

CREATE TABLE IF NOT EXISTS file_checkpoint (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id  INTEGER NOT NULL REFERENCES session(id) ON DELETE CASCADE,
  group_id    INTEGER NOT NULL,
  path        TEXT    NOT NULL,
  before      TEXT,
  before_hash TEXT,
  after_hash  TEXT,
  is_rewind   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS file_checkpoint_session_idx ON file_checkpoint(session_id, id);

CREATE TABLE IF NOT EXISTS usage_anchor (
  channel           TEXT    NOT NULL,
  chat_id           TEXT    NOT NULL,
  cron_job_id       TEXT    NOT NULL DEFAULT '',
  total_tokens      INTEGER NOT NULL,
  anchor_message_id INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL,
  PRIMARY KEY(channel, chat_id, cron_job_id)
);

CREATE TABLE IF NOT EXISTS journal_event (
  id      INTEGER PRIMARY KEY AUTOINCREMENT,
  kind    TEXT    NOT NULL,
  stream  TEXT    NOT NULL,
  seq     INTEGER NOT NULL,
  ts      INTEGER NOT NULL,
  payload TEXT    NOT NULL,
  UNIQUE(kind, stream, seq)
);

CREATE TABLE IF NOT EXISTS kv (
  key        TEXT PRIMARY KEY,
  value      TEXT    NOT NULL,
  updated_at INTEGER NOT NULL
);
"#;

/// 一个打开的状态库连接（可 `Clone` 共享；内部 `Mutex` 串行化同一连接上的操作）。
///
/// rusqlite 的 `Connection` 不是 `Sync`，而 RClaw 的 store 需要在 tokio 任务间共享，
/// 所以用 `Arc<Mutex<_>>`。追加 / 加载单会话等热路径是毫秒级同步调用，不跨 `await`
/// 持锁；全库扫描类操作（`search_messages` / `scan_messages` / `VACUUM`）耗时与库体积
/// 成正比，调用方应放到 `spawn_blocking`，并且不要在持锁回调里再调 store 方法
/// （`std::sync::Mutex` 不可重入，会死锁）。
#[derive(Clone)]
pub struct Db {
    inner: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").field("path", &self.path).finish()
    }
}

impl Db {
    /// 打开（必要时创建）状态库并把 schema 迁移到当前版本。
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)?;
        configure(&conn)?;
        migrate(&conn)?;
        Ok(Db {
            inner: Arc::new(Mutex::new(conn)),
            path: path.to_path_buf(),
        })
    }

    /// 库文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // 持锁期间 panic 只会发生在调用方闭包里（SQLite 本身不 panic）；连接仍然可用，
        // 直接接管而不是让整个 store 永久不可用。
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 在连接上执行只读 / 单语句操作。
    pub(crate) fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.lock();
        f(&conn)
    }

    /// 在一个 `BEGIN DEFERRED` 读事务里执行【多条】只读语句：WAL 下从第一条 SELECT 起就固定
    /// 在同一个快照，直到事务结束。进程内的写者由连接互斥锁串行、本就不会交错，但桌面 +
    /// CLI 共用一个 `agent/` 时另一进程可以在两条语句之间提交——拼接活动视图的几段 SELECT
    /// 若各看各的快照，刚提交的 boundary 会以普通 user 行混进 `after` 段（一个视图里两份
    /// 摘要）。只读，结束时回滚。
    pub(crate) fn read_snapshot<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let out = f(&tx)?;
        tx.finish()?;
        Ok(out)
    }

    /// 在 `BEGIN IMMEDIATE` 写事务里执行一组操作；闭包返回 `Err` 自动回滚。
    pub(crate) fn transaction<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// `VACUUM` + `wal_checkpoint(TRUNCATE)`：`/clear` 等大批删除后回收文件体积。
    ///
    /// `secure_delete=ON` 只把【主库文件】里的删除页清零；WAL 模式下这些写入先落在
    /// `state.sqlite-wal`，被覆盖前的旧页面（含删掉的消息明文）仍留在 WAL 文件里。VACUUM
    /// 把空闲页交还文件系统后，再做一次 TRUNCATE checkpoint 把 WAL 回写并截成 0 字节，
    /// 磁盘上才真的不剩残留。有别的连接持读事务时 checkpoint 会 BUSY / 不完整——只告警，
    /// 下次 checkpoint 自然追平。必须在事务外执行。
    pub(crate) fn vacuum(&self) -> Result<()> {
        let conn = self.lock();
        conn.execute_batch("VACUUM")?;
        if let Err(e) = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)") {
            tracing::warn!("state.sqlite WAL checkpoint 失败（主库已回收，WAL 残留待下次 checkpoint）: {e}");
        }
        Ok(())
    }
}

/// 每个连接打开时的 PRAGMA。
fn configure(conn: &Connection) -> Result<()> {
    // busy_timeout 必须最先设：切换 journal_mode 需要独占锁，另一进程 / 连接（桌面 + CLI
    // 共用同一 agent/、测试里的第二连接）正持锁时，没有 busy handler 会立刻 SQLITE_BUSY
    // 导致打开失败（ZCode 的 migration-runner 同样先设 timeout 再跑 PRAGMA）。
    conn.pragma_update(None, "busy_timeout", 5_000)?;
    // WAL：跨连接 / 跨进程的读不被写阻塞（进程内本就共用一条连接、按互斥锁串行），
    // 追加路径只写 WAL 顺序页；崩溃一致性由 SQLite 保证。
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!("state.sqlite 未能启用 WAL（journal_mode={mode}），继续使用当前模式");
    }
    // NORMAL + WAL：提交只保证写入 WAL，掉电最多丢最后几笔已提交事务但不会损坏库；
    // 对聊天历史而言是合适的折中（与旧 JSONL append+fsync 的保证相当）。
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // `/clear` 承诺「永久删除」：删除页内容清零，而不是仅标记空闲。
    conn.pragma_update(None, "secure_delete", "ON")?;
    register_functions(conn)?;
    Ok(())
}

/// 连接级 Rust 标量函数。
///
/// `rclaw_lower(text)`：Unicode 小写（SQLite 内建 `lower()` 只折 ASCII，不带 ICU 的
/// bundled 构建对西里尔 / 希腊 / 带重音拉丁 / 全角字母无能为力）。`search_messages` 用它
/// 做大小写不敏感的 AND 粗筛，与调用方 Rust 侧 `str::to_lowercase` 计分口径一致。
fn register_functions(conn: &Connection) -> Result<()> {
    use rusqlite::functions::FunctionFlags;
    conn.create_scalar_function(
        "rclaw_lower",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text = ctx.get_raw(0).as_str_or_null()?;
            Ok(text.map(str::to_lowercase))
        },
    )?;
    Ok(())
}

/// 读取 `user_version`，高于本程序支持的版本则拒绝。
fn supported_schema_version(conn: &Connection) -> Result<i64> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        // 新版本程序写过的库不能由旧程序继续写：列 / 语义都可能变了，部分写入比拒绝更糟。
        return Err(crate::StoreError::Other(format!(
            "state.sqlite schema 版本 v{version} 高于本程序支持的 v{SCHEMA_VERSION}，请升级 RClaw"
        )));
    }
    Ok(version)
}

/// 按 `user_version` 执行迁移。v1 之后的版本在此追加 `if version < N { ... }` 分支。
fn migrate(conn: &Connection) -> Result<()> {
    if supported_schema_version(conn)? == SCHEMA_VERSION {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let applied = (|| -> Result<()> {
        // 拿到写锁后重读：另一进程（桌面 + CLI 共用同一 agent/）可能在我们排队等锁期间
        // 已经完成迁移，非幂等的迁移步骤（如 ALTER TABLE ADD COLUMN）不能再跑一遍。
        let version = supported_schema_version(conn)?;
        if version == SCHEMA_VERSION {
            return Ok(());
        }
        if version < 1 {
            conn.execute_batch(SCHEMA_V1)?;
        }
        if version < 2 {
            conn.execute_batch(SCHEMA_V2_MIGRATION)?;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    })();
    match applied {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// 当前 Unix 毫秒时间戳（`created_at` / `updated_at` / journal `ts`）。
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_is_idempotent_and_sets_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("state.sqlite");
        let db = Db::open(&path).unwrap();
        let version: i64 = db
            .with(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let mode: String = db
            .with(|c| Ok(c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        let fk: i64 = db
            .with(|c| Ok(c.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(fk, 1);

        // 第二次打开同一文件：不重建、不报错。
        let again = Db::open(&path).unwrap();
        let tables: i64 = again
            .with(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN \
                     ('session','message','file_checkpoint','usage_anchor','journal_event','kv')",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(tables, 6);
    }

    #[test]
    fn transaction_rolls_back_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.sqlite")).unwrap();
        let res: Result<()> = db.transaction(|tx| {
            tx.execute(
                "INSERT INTO kv(key, value, updated_at) VALUES ('a', '1', 0)",
                [],
            )?;
            Err(crate::StoreError::Other("boom".into()))
        });
        assert!(res.is_err());
        let n: i64 = db
            .with(|c| Ok(c.query_row("SELECT count(*) FROM kv", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 0);
    }

    /// 读快照结束后（闭包 Ok / Err 两条路径）连接都回到自动提交：没有残留的打开事务，
    /// 随后的写事务照常开得起来；快照里的多条 SELECT 读到同一份数据。
    #[test]
    fn read_snapshot_leaves_no_open_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.sqlite")).unwrap();
        let (a, b): (i64, i64) = db
            .read_snapshot(|c| {
                assert!(!c.is_autocommit(), "快照内处于事务中");
                let a = c.query_row("SELECT count(*) FROM kv", [], |r| r.get(0))?;
                let b = c.query_row("SELECT count(*) FROM kv", [], |r| r.get(0))?;
                Ok((a, b))
            })
            .unwrap();
        assert_eq!((a, b), (0, 0));
        assert!(db.with(|c| Ok(c.is_autocommit())).unwrap());

        let res: Result<()> = db.read_snapshot(|_| Err(crate::StoreError::Other("boom".into())));
        assert!(res.is_err());
        assert!(db.with(|c| Ok(c.is_autocommit())).unwrap(), "Err 路径也不留事务");

        db.transaction(|tx| {
            tx.execute(
                "INSERT INTO kv(key, value, updated_at) VALUES ('a', '1', 0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let n: i64 = db
            .read_snapshot(|c| Ok(c.query_row("SELECT count(*) FROM kv", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn newer_schema_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        {
            let db = Db::open(&path).unwrap();
            db.with(|c| {
                c.pragma_update(None, "user_version", SCHEMA_VERSION + 1)?;
                Ok(())
            })
            .unwrap();
        }
        let err = Db::open(&path).unwrap_err();
        assert!(err.to_string().contains("高于本程序支持"), "{err}");
    }

    #[test]
    fn rclaw_lower_folds_unicode_and_passes_null_through() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.sqlite")).unwrap();
        let lowered: String = db
            .with(|c| {
                Ok(c.query_row(
                    "SELECT rclaw_lower('Ärger ПРИВЕТ Ｎｇｉｎｘ')",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(lowered, "ärger привет ｎｇｉｎｘ");
        let builtin: String = db
            .with(|c| Ok(c.query_row("SELECT lower('ПРИВЕТ')", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(builtin, "ПРИВЕТ", "内建 lower() 只折 ASCII，正是需要自定义函数的原因");
        let null: Option<String> = db
            .with(|c| Ok(c.query_row("SELECT rclaw_lower(NULL)", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(null, None);
    }
}

#[cfg(test)]
mod v2_migration_tests {
    use super::*;

    #[test]
    fn v1_database_migrates_to_v2_with_checkpoint_anchor_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        // 手造 v1 库：建 v1 schema、插一行 checkpoint、盖 user_version=1。
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute(
                "INSERT INTO session(channel, chat_id, created_at, updated_at) VALUES ('cli','c',1,1)",
                [],
            )
            .unwrap();
            let sid: i64 = conn.query_row("SELECT id FROM session", [], |r| r.get(0)).unwrap();
            conn.execute(
                "INSERT INTO file_checkpoint(session_id, group_id, path, before, before_hash, \
                 after_hash, is_rewind) VALUES (?1, 100, 'a.txt', 'v1', 'h1', 'h2', 0)",
                [sid],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1_i64).unwrap();
        }
        // 正常打开：迁移到 v2，旧行在、anchor 列为 NULL。
        let db = Db::open(&path).unwrap();
        db.with(|conn| {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            assert_eq!(version, SCHEMA_VERSION);
            let (count, anchor): (i64, Option<i64>) = conn.query_row(
                "SELECT count(*), min(anchor_message_id) FROM file_checkpoint",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!(count, 1, "迁移不丢数据");
            assert_eq!(anchor, None, "旧行 anchor 为 NULL（归属未知，不参与按消息回滚）");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn fresh_database_creates_v2_directly() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.sqlite")).unwrap();
        db.with(|conn| {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            assert_eq!(version, SCHEMA_VERSION);
            // v2 列可直接写入（先建会话行满足外键）。
            conn.execute(
                "INSERT INTO session(channel, chat_id, created_at, updated_at) VALUES ('c','k',1,1)",
                [],
            )?;
            conn.execute(
                "INSERT INTO file_checkpoint(session_id, group_id, path, is_rewind, \
                 anchor_message_id) VALUES ((SELECT id FROM session), 1, 'p', 0, 42)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    }
}
