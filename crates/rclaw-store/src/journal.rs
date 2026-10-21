//! 追加式 journal 与 kv（`agent/state.sqlite` 的 `journal_event` / `kv` 表）。
//!
//! 取代旧的 `agent/plans/<plan_id>.jsonl`、`agent/workflows/<run_id>.jsonl`、
//! `agent/task_executions/{team-state,<agent>-<hash>}.jsonl` 与团队收件箱 JSON：
//! 每条事件 = `(kind, stream, seq, payload)`，`seq` 在 `(kind, stream)` 内从 1 连续递增
//! （对应 ZCode `workflow_event(run_id, sequence)` 的唯一约束），追加与读取天然有序，
//! 不再依赖文件追加的原子性与目录扫描。

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::db::{now_ms, Db};
use crate::fs::Layout;
use crate::Result;

/// PlanRun 事件流（stream = plan_id）。
pub const JOURNAL_PLAN: &str = "plan";
/// WorkflowRun 事件流（stream = run_id）。
pub const JOURNAL_WORKFLOW: &str = "workflow";
/// 团队运行态快照流（stream 固定为空串）。
pub const JOURNAL_TEAM_STATE: &str = "team_state";
/// 子 agent 执行事件流（stream = agent_id）。
pub const JOURNAL_EXEC_LOG: &str = "exec_log";
/// 团队收件箱（kv）。
pub const KV_TEAM_INBOX: &str = "team_inbox";

/// journal / kv 门面；`Clone` 共享同一连接。
#[derive(Debug, Clone)]
pub struct JournalStore {
    db: Db,
}

impl JournalStore {
    /// 独立打开 `layout.db_file`（与 [`crate::SessionStore`] 共享连接时用 `session.journal()`）。
    pub fn open(layout: &Layout) -> Result<Self> {
        Ok(JournalStore {
            db: Db::open(&layout.db_file)?,
        })
    }

    pub fn from_db(db: Db) -> Self {
        JournalStore { db }
    }

    /// 追加一条事件，返回其在 `(kind, stream)` 内的 `seq`（从 1 起）。
    pub fn append(&self, kind: &str, stream: &str, payload: &Value) -> Result<u64> {
        self.db
            .transaction(|tx| insert_event(tx, kind, stream, payload))
    }

    /// 追加一条事件，并在同一事务里只保留该流最新的 `keep_last` 条（含本条）。
    ///
    /// 给「每次变更整份落一条快照、读取只看最新一条」的流（团队运行态）用：不裁剪就是
    /// O(变更次数 × 快照大小) 的只增不减。`seq` 按 `max(seq) + 1` 生成，删旧行不影响单调性。
    pub fn append_keep_last(
        &self,
        kind: &str,
        stream: &str,
        payload: &Value,
        keep_last: usize,
    ) -> Result<u64> {
        self.db.transaction(|tx| {
            let seq = insert_event(tx, kind, stream, payload)?;
            let cutoff = (seq as i64).saturating_sub(keep_last.max(1) as i64);
            tx.execute(
                "DELETE FROM journal_event WHERE kind = ?1 AND stream = ?2 AND seq <= ?3",
                params![kind, stream, cutoff],
            )?;
            Ok(seq)
        })
    }

    /// 删除整条事件流（保留策略清理已终态的历史流用）。返回删除的事件数。
    pub fn delete_stream(&self, kind: &str, stream: &str) -> Result<usize> {
        self.db.transaction(|tx| {
            Ok(tx.execute(
                "DELETE FROM journal_event WHERE kind = ?1 AND stream = ?2",
                params![kind, stream],
            )?)
        })
    }

    /// 「先读整条流、再决定是否追加」，两步在同一个 `BEGIN IMMEDIATE` 写事务里完成。
    ///
    /// `decide` 拿到当前全部事件（`seq` 升序），返回 `(Some(payload), verdict)` 则追加并把
    /// 新事件的 `seq` 一起返回，返回 `(None, verdict)` 则什么都不写。写锁在读之前就已持有：
    /// 同进程另一次调用（连接互斥）与另一进程（桌面与 CLI 共用一个库）的写入都只能排在
    /// 本事务之后、并看到本次结果——用于「没应答过才落应答」这类判定与写入必须原子的
    /// 场景。闭包内不得再调用本 store 的其它方法（连接锁已被持有，会死锁）。
    pub fn append_guarded<T>(
        &self,
        kind: &str,
        stream: &str,
        decide: impl FnOnce(&[(u64, Value)]) -> (Option<Value>, T),
    ) -> Result<(Option<u64>, T)> {
        self.db.transaction(|tx| {
            let events = load_events(tx, kind, stream)?;
            let (payload, verdict) = decide(&events);
            let seq = match payload {
                Some(payload) => Some(insert_event(tx, kind, stream, &payload)?),
                None => None,
            };
            Ok((seq, verdict))
        })
    }

    /// 读取整个事件流（`seq` 升序）。不存在的流返回空 Vec。
    pub fn load(&self, kind: &str, stream: &str) -> Result<Vec<(u64, Value)>> {
        self.db.with(|conn| load_events(conn, kind, stream))
    }

    /// 读取事件流的最后 `limit` 条（`seq` 升序）。只关心尾部的调用方（启动恢复、状态面板）
    /// 用它代替 [`Self::load`]，避免把长流整条读进内存。不可解析的事件跳过（同 `load`）。
    pub fn load_tail(&self, kind: &str, stream: &str, limit: usize) -> Result<Vec<(u64, Value)>> {
        self.db.with(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT seq, payload FROM journal_event WHERE kind = ?1 AND stream = ?2 \
                 ORDER BY seq DESC LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![kind, stream, limit as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (seq, text) = row?;
                match serde_json::from_str::<Value>(&text) {
                    Ok(v) => out.push((seq.max(0) as u64, v)),
                    Err(e) => tracing::warn!(
                        "journal 事件解析失败，已跳过: kind={kind} stream={stream} seq={seq} err={e}"
                    ),
                }
            }
            out.reverse();
            Ok(out)
        })
    }

    /// 最后一条事件（payload 不可解析时告警并视为不存在，与 [`Self::load`] 的跳过策略一致）。
    pub fn latest(&self, kind: &str, stream: &str) -> Result<Option<Value>> {
        self.db.with(|conn| {
            let text: Option<String> = conn
                .query_row(
                    "SELECT payload FROM journal_event WHERE kind = ?1 AND stream = ?2 \
                     ORDER BY seq DESC LIMIT 1",
                    params![kind, stream],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(text.and_then(|t| match serde_json::from_str(&t) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(
                        "journal 最新事件解析失败，按不存在处理: kind={kind} stream={stream} err={e}"
                    );
                    None
                }
            }))
        })
    }

    /// 事件流是否存在（至少一条事件）。
    pub fn exists(&self, kind: &str, stream: &str) -> Result<bool> {
        self.db.with(|conn| {
            let n: i64 = conn.query_row(
                "SELECT count(*) FROM journal_event WHERE kind = ?1 AND stream = ?2",
                params![kind, stream],
                |row| row.get(0),
            )?;
            Ok(n > 0)
        })
    }

    /// 某类 journal 的全部 stream 名（去重，按首次出现顺序）。
    pub fn streams(&self, kind: &str) -> Result<Vec<String>> {
        self.db.with(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT stream FROM journal_event WHERE kind = ?1 GROUP BY stream ORDER BY min(id)",
            )?;
            let rows = stmt.query_map(params![kind], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    /// 读 kv（值不可解析时告警并视为不存在：调用方会按「空」重建，而不是静默丢数据不留痕）。
    pub fn kv_get(&self, key: &str) -> Result<Option<Value>> {
        self.db.with(|conn| {
            let text: Option<String> = conn
                .query_row(
                    "SELECT value FROM kv WHERE key = ?1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(text.and_then(|t| match serde_json::from_str(&t) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("kv 值解析失败，按不存在处理: key={key} err={e}");
                    None
                }
            }))
        })
    }

    pub fn kv_set(&self, key: &str, value: &Value) -> Result<()> {
        let text = serde_json::to_string(value)?;
        self.db.with(|conn| {
            conn.execute(
                "INSERT INTO kv(key, value, updated_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![key, text, now_ms()],
            )?;
            Ok(())
        })
    }

    pub fn kv_delete(&self, key: &str) -> Result<()> {
        self.db.with(|conn| {
            conn.execute("DELETE FROM kv WHERE key = ?1", params![key])?;
            Ok(())
        })
    }

    /// 条件删除（原子）：仅当现值与 `expected` 完全一致时删除，返回是否真的删了。
    ///
    /// 「读到 → 比对 → 删」三步各自进出数据库互斥锁，并发写可以插在中间把值换掉，
    /// 最后一步的无条件删就会端掉别人的数据——比对与删除必须在同一条 SQL 里。
    /// 序列化形态确定（serde_json BTreeMap 键序），`kv_set` 写入的值与 `kv_get` 读回
    /// 再序列化的结果逐字节一致，字符串比较可靠。
    pub fn kv_delete_if(&self, key: &str, expected: &Value) -> Result<bool> {
        let text = serde_json::to_string(expected)?;
        self.db.with(|conn| {
            let deleted = conn.execute(
                "DELETE FROM kv WHERE key = ?1 AND value = ?2",
                params![key, text],
            )?;
            Ok(deleted > 0)
        })
    }
}

/// 在给定连接 / 事务上追加一条事件，返回其 `seq`。`seq = max(seq) + 1` 必须与 INSERT 同在
/// 一个写事务里（调用方保证），否则两个写者会算出同一个 `seq`。
fn insert_event(conn: &Connection, kind: &str, stream: &str, payload: &Value) -> Result<u64> {
    let text = serde_json::to_string(payload)?;
    let next: i64 = conn.query_row(
        "SELECT coalesce(max(seq), 0) + 1 FROM journal_event WHERE kind = ?1 AND stream = ?2",
        params![kind, stream],
        |row| row.get(0),
    )?;
    conn.execute(
        "INSERT INTO journal_event(kind, stream, seq, ts, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![kind, stream, next, now_ms(), text],
    )?;
    Ok(next.max(0) as u64)
}

/// 在给定连接 / 事务上读取整条事件流（`seq` 升序）；不可解析的事件告警后跳过。
fn load_events(conn: &Connection, kind: &str, stream: &str) -> Result<Vec<(u64, Value)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT seq, payload FROM journal_event WHERE kind = ?1 AND stream = ?2 ORDER BY seq",
    )?;
    let rows = stmt.query_map(params![kind, stream], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, text) = row?;
        match serde_json::from_str::<Value>(&text) {
            Ok(v) => out.push((seq.max(0) as u64, v)),
            Err(e) => tracing::warn!(
                "journal 事件解析失败，已跳过: kind={kind} stream={stream} seq={seq} err={e}"
            ),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> (tempfile::TempDir, JournalStore) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("agent"));
        let journal = JournalStore::open(&layout).unwrap();
        (dir, journal)
    }

    #[test]
    fn seq_is_per_stream_and_ordered() {
        let (_dir, j) = store();
        assert_eq!(j.append(JOURNAL_PLAN, "p1", &json!({"e": 1})).unwrap(), 1);
        assert_eq!(j.append(JOURNAL_PLAN, "p1", &json!({"e": 2})).unwrap(), 2);
        assert_eq!(j.append(JOURNAL_PLAN, "p2", &json!({"e": 1})).unwrap(), 1);
        assert_eq!(j.append(JOURNAL_WORKFLOW, "p1", &json!({"w": 1})).unwrap(), 1);

        let p1 = j.load(JOURNAL_PLAN, "p1").unwrap();
        assert_eq!(p1.len(), 2);
        assert_eq!(p1[0], (1, json!({"e": 1})));
        assert_eq!(p1[1], (2, json!({"e": 2})));
        assert_eq!(j.latest(JOURNAL_PLAN, "p1").unwrap(), Some(json!({"e": 2})));
        assert_eq!(j.latest(JOURNAL_PLAN, "nope").unwrap(), None);
        assert!(j.load(JOURNAL_PLAN, "nope").unwrap().is_empty());
        assert!(j.exists(JOURNAL_PLAN, "p2").unwrap());
        assert!(!j.exists(JOURNAL_EXEC_LOG, "p2").unwrap());
        assert_eq!(j.streams(JOURNAL_PLAN).unwrap(), vec!["p1", "p2"]);
        assert_eq!(j.streams(JOURNAL_WORKFLOW).unwrap(), vec!["p1"]);
        // load_tail：只取尾部、仍按 seq 升序；limit 超过长度等价于 load；空流为空。
        j.append(JOURNAL_PLAN, "p1", &json!({"e": 3})).unwrap();
        assert_eq!(
            j.load_tail(JOURNAL_PLAN, "p1", 2).unwrap(),
            vec![(2, json!({"e": 2})), (3, json!({"e": 3}))]
        );
        assert_eq!(j.load_tail(JOURNAL_PLAN, "p1", 10).unwrap(), j.load(JOURNAL_PLAN, "p1").unwrap());
        assert!(j.load_tail(JOURNAL_PLAN, "nope", 5).unwrap().is_empty());
    }

    #[test]
    fn kv_roundtrip_and_overwrite() {
        let (_dir, j) = store();
        assert_eq!(j.kv_get(KV_TEAM_INBOX).unwrap(), None);
        j.kv_set(KV_TEAM_INBOX, &json!({"unread": 1})).unwrap();
        assert_eq!(j.kv_get(KV_TEAM_INBOX).unwrap(), Some(json!({"unread": 1})));
        j.kv_set(KV_TEAM_INBOX, &json!({"unread": 0})).unwrap();
        assert_eq!(j.kv_get(KV_TEAM_INBOX).unwrap(), Some(json!({"unread": 0})));
        j.kv_delete(KV_TEAM_INBOX).unwrap();
        assert_eq!(j.kv_get(KV_TEAM_INBOX).unwrap(), None);
    }

    #[test]
    fn kv_delete_if_only_removes_the_expected_value() {
        let (_dir, j) = store();
        j.kv_set("k", &json!({"created_at": 1, "objective": "A"})).unwrap();
        // 值不符（目标已被 /goal 换掉）：不删，返回 false。
        assert!(!j.kv_delete_if("k", &json!({"created_at": 0, "objective": "A"})).unwrap());
        assert!(j.kv_get("k").unwrap().is_some());
        // 值一致（读回的 Value 再序列化与库内文本逐字节一致）：原子删除。
        let current = j.kv_get("k").unwrap().unwrap();
        assert!(j.kv_delete_if("k", &current).unwrap());
        assert_eq!(j.kv_get("k").unwrap(), None);
        // 键不存在：false 而非错误（幂等）。
        assert!(!j.kv_delete_if("k", &current).unwrap());
    }

    #[test]
    fn session_store_journal_writes_are_visible_to_separately_opened_journal() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("agent"));
        let session = crate::SessionStore::open(layout).unwrap();
        let j = session.journal();
        j.append(JOURNAL_TEAM_STATE, "", &json!({"teams": {}})).unwrap();
        // 第二个连接打开同一 state.sqlite：事务已提交，立即可见。
        let again = JournalStore::open(session.layout()).unwrap();
        assert_eq!(again.load(JOURNAL_TEAM_STATE, "").unwrap().len(), 1);
    }

    #[test]
    fn corrupt_payloads_are_skipped_with_warning_not_error() {
        let (_dir, j) = store();
        j.append(JOURNAL_PLAN, "p", &json!({"ok": 1})).unwrap();
        j.kv_set("k", &json!({"ok": 1})).unwrap();
        // 直接篡改落库内容，模拟损坏。
        j.db
            .with(|conn| {
                conn.execute("UPDATE journal_event SET payload = '{not json'", [])?;
                conn.execute("UPDATE kv SET value = '{not json'", [])?;
                Ok(())
            })
            .unwrap();
        assert!(j.load(JOURNAL_PLAN, "p").unwrap().is_empty());
        assert_eq!(j.latest(JOURNAL_PLAN, "p").unwrap(), None);
        assert!(j.exists(JOURNAL_PLAN, "p").unwrap(), "行仍在，只是不可解析");
        assert_eq!(j.kv_get("k").unwrap(), None);
    }

    #[test]
    fn append_guarded_decides_on_current_stream_and_is_atomic_across_connections() {
        let (_dir, j) = store();
        j.append(JOURNAL_PLAN, "p", &json!({"event": "asked", "q": "q1"}))
            .unwrap();
        let resolve = |events: &[(u64, Value)]| {
            let resolved = events
                .iter()
                .any(|(_, event)| event["event"] == "resolved" && event["q"] == "q1");
            if resolved {
                (None, "already_resolved")
            } else {
                (Some(json!({"event": "resolved", "q": "q1"})), "resolved")
            }
        };
        // 首次：流里没有应答 → 追加，seq 紧随其后。
        assert_eq!(
            j.append_guarded(JOURNAL_PLAN, "p", resolve).unwrap(),
            (Some(2), "resolved")
        );
        // 另一条连接（另一进程的等价物）再试：读到的是已提交的应答 → 不写。
        let other = JournalStore::open(&Layout::new(j.db.path().parent().unwrap().to_path_buf()))
            .unwrap();
        assert_eq!(
            other.append_guarded(JOURNAL_PLAN, "p", resolve).unwrap(),
            (None, "already_resolved")
        );
        assert_eq!(j.load(JOURNAL_PLAN, "p").unwrap().len(), 2, "只有一条应答");
        // 不写的分支不会占用 seq。
        assert_eq!(j.append(JOURNAL_PLAN, "p", &json!({"event": "done"})).unwrap(), 3);
    }
}
