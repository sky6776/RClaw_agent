//! 工作流运行态投影表（`state.sqlite` schema v3）。
//!
//! `journal_event` 是唯一事实源；这里的 8 张表是投影 / 索引。启动时按 journal 全局 id
//! 严格升序折叠（`workflow_run.last_event_id` 只前进不后退），表与 journal 冲突以 journal
//! 为准。终态事件、run 行更新、notification 插入必须落在同一个事务里。

use std::collections::{HashMap, HashSet};

use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::{json, Value};

use rclaw_store::db::{now_ms, Db};
use rclaw_store::journal::{JournalStore, JOURNAL_WORKFLOW};

pub const STATUS_QUEUED: &str = "queued";
pub const STATUS_RUNNING: &str = "running";
pub const STATUS_STOPPING: &str = "stopping";
pub const STATUS_COMPLETED: &str = "completed";
pub const STATUS_ERRORED: &str = "errored";
pub const STATUS_STOPPED: &str = "stopped";

pub const STOP_USER: &str = "user";
pub const STOP_MODEL: &str = "model";
pub const STOP_INTERRUPTED: &str = "interrupted";
pub const STOP_SUPERSEDED: &str = "superseded";
pub const STOP_PERMISSION_REVOKED: &str = "permission_revoked";

const KV_PROJECTION_CURSOR: &str = "workflow-projection-cursor";

type Result<T> = std::result::Result<T, String>;

fn db_err(error: impl std::fmt::Display) -> String {
    format!("工作流状态库操作失败: {error}")
}

/// 一行 run 投影。
#[derive(Debug, Clone)]
pub struct RunRow {
    pub run_id: String,
    pub cwd: String,
    pub session_key: String,
    pub label: String,
    pub source_path: Option<String>,
    pub snapshot_path: String,
    pub snapshot_hash: String,
    pub script_generation: u32,
    pub exec_generation: u32,
    pub legacy: bool,
    pub resumable: bool,
    pub status: String,
    pub queued_reason: Option<String>,
    pub stop_reason: Option<String>,
    pub stop_origin: Option<String>,
    pub requires_fresh_auth: bool,
    pub capacity_owner: Option<String>,
    pub capacity_claim_for: Option<String>,
    pub resumed_from: Option<String>,
    pub superseded_by: Option<String>,
    pub started_by: String,
    pub execution_config: String,
    pub owner_instance: Option<String>,
    pub lease_until: Option<i64>,
    pub last_event_id: i64,
    pub elapsed_runtime_ms: i64,
    pub cache_closed: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub heartbeat_at: i64,
    pub spent_tokens: i64,
    pub result_json: Option<String>,
    pub result_handle: Option<String>,
    pub error: Option<String>,
}

impl RunRow {
    pub fn execution_config(&self) -> Value {
        serde_json::from_str(&self.execution_config).unwrap_or_else(|_| json!({}))
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            STATUS_COMPLETED | STATUS_ERRORED | STATUS_STOPPED
        )
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self.status.as_str(),
            STATUS_QUEUED | STATUS_RUNNING | STATUS_STOPPING
        )
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(RunRow {
            run_id: row.get("run_id")?,
            cwd: row.get("cwd")?,
            session_key: row.get("session_key")?,
            label: row.get("label")?,
            source_path: row.get("source_path")?,
            snapshot_path: row.get("snapshot_path")?,
            snapshot_hash: row.get("snapshot_hash")?,
            script_generation: row.get::<_, i64>("script_generation")?.max(1) as u32,
            exec_generation: row.get::<_, i64>("exec_generation")?.max(0) as u32,
            legacy: row.get::<_, i64>("legacy")? != 0,
            resumable: row.get::<_, i64>("resumable")? != 0,
            status: row.get("status")?,
            queued_reason: row.get("queued_reason")?,
            stop_reason: row.get("stop_reason")?,
            stop_origin: row.get("stop_origin")?,
            requires_fresh_auth: row.get::<_, i64>("requires_fresh_auth")? != 0,
            capacity_owner: row.get("capacity_owner")?,
            capacity_claim_for: row.get("capacity_claim_for")?,
            resumed_from: row.get("resumed_from")?,
            superseded_by: row.get("superseded_by")?,
            started_by: row.get("started_by")?,
            execution_config: row.get("execution_config")?,
            owner_instance: row.get("owner_instance")?,
            lease_until: row.get("lease_until")?,
            last_event_id: row.get("last_event_id")?,
            elapsed_runtime_ms: row.get("elapsed_runtime_ms")?,
            cache_closed: row.get::<_, i64>("cache_closed")? != 0,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
            heartbeat_at: row.get("heartbeat_at")?,
            spent_tokens: row.get("spent_tokens")?,
            result_json: row.get("result_json")?,
            result_handle: row.get("result_handle")?,
            error: row.get("error")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct EffectRow {
    pub effect_key: String,
    pub op: String,
    pub is_write: bool,
    pub args_hash: String,
    pub args_preview: Option<String>,
    pub status: String,
    pub result: Option<String>,
    pub decision: Option<String>,
    pub decision_exec_generation: Option<u32>,
    pub substitute_result: Option<String>,
}

impl EffectRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(EffectRow {
            effect_key: row.get("effect_key")?,
            op: row.get("op")?,
            is_write: row.get::<_, i64>("is_write")? != 0,
            args_hash: row.get("args_hash")?,
            args_preview: row.get("args_preview")?,
            status: row.get("status")?,
            decision: row.get("decision")?,
            decision_exec_generation: row
                .get::<_, Option<i64>>("decision_exec_generation")?
                .map(|value| value.max(0) as u32),
            substitute_result: row.get("substitute_result")?,
            result: row.get("result")?,
        })
    }

    /// 该项是否阻止在 `next_exec_generation` 启动执行。
    ///
    /// 只读 effect（files.* / git.*）失败或半途中断不可能留下写入，不进阻断集合——否则脚本里
    /// 一次 `try { await files.read(..) } catch {}` 探测就会让 Resume / Fill 永远被
    /// `resume_blocked_effect` 挡住；续跑时直接重做即可（见 `EffectTracker::begin`）。
    pub fn blocks(&self, next_exec_generation: u32) -> bool {
        if !matches!(self.status.as_str(), "failed" | "started" | "unknown") {
            return false;
        }
        if !self.is_write {
            return false;
        }
        match self.decision.as_deref() {
            Some("skip") => self.substitute_result.is_none(),
            Some("rerun") => self.decision_exec_generation != Some(next_exec_generation),
            _ => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuestionRow {
    pub question_id: String,
    pub actor: String,
    pub question: String,
    pub state: String,
    pub answer: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HoleRow {
    pub hole_id: String,
    pub script_generation: u32,
    pub name: String,
    pub prompt: Option<String>,
    pub state: String,
    pub last_rejected_hash: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NotificationRow {
    pub id: String,
    pub run_id: String,
    pub channel: String,
    pub chat_id: String,
    pub payload: String,
    pub state: String,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct RequestRow {
    pub digest: String,
    pub state: String,
    pub run_id: Option<String>,
    pub response: Option<String>,
}

/// 工作流投影表门面；与 journal 共享同一条 SQLite 连接。
#[derive(Clone)]
pub struct WorkflowStore {
    journal: JournalStore,
    db: Db,
}

impl WorkflowStore {
    pub fn new(journal: &JournalStore) -> Self {
        WorkflowStore {
            journal: journal.clone(),
            db: journal.db(),
        }
    }

    pub fn journal(&self) -> &JournalStore {
        &self.journal
    }

    pub fn transaction<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        self.db
            .transaction(|tx| f(tx).map_err(rclaw_store::StoreError::Other))
            .map_err(db_err)
    }

    // -----------------------------------------------------------------------
    // run
    // -----------------------------------------------------------------------

    pub fn get_run(&self, run_id: &str) -> Result<Option<RunRow>> {
        self.db
            .with(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT * FROM workflow_run WHERE run_id = ?1",
                        params![run_id],
                        RunRow::from_row,
                    )
                    .optional()?)
            })
            .map_err(db_err)
    }

    /// 按项目根列 run；`include_legacy` 只在特权调用方为 true。
    pub fn list_runs(
        &self,
        cwd: &str,
        limit: usize,
        statuses: &[String],
        include_legacy: bool,
    ) -> Result<Vec<RunRow>> {
        self.db
            .with(|conn| {
                let mut sql = String::from("SELECT * FROM workflow_run WHERE ");
                let mut clauses: Vec<String> = Vec::new();
                if include_legacy {
                    clauses.push("(cwd = ?1 OR (legacy = 1 AND cwd = ''))".to_string());
                } else {
                    clauses.push("cwd = ?1".to_string());
                }
                if !statuses.is_empty() {
                    let placeholders: Vec<String> = statuses
                        .iter()
                        .enumerate()
                        .map(|(index, _)| format!("?{}", index + 2))
                        .collect();
                    clauses.push(format!("status IN ({})", placeholders.join(",")));
                }
                sql.push_str(&clauses.join(" AND "));
                sql.push_str(&format!(
                    " ORDER BY updated_at DESC LIMIT {}",
                    limit.clamp(1, 200)
                ));
                let mut stmt = conn.prepare(&sql)?;
                let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(cwd.to_string())];
                for status in statuses {
                    values.push(Box::new(status.clone()));
                }
                let refs: Vec<&dyn rusqlite::ToSql> =
                    values.iter().map(|value| value.as_ref()).collect();
                let rows = stmt.query_map(refs.as_slice(), RunRow::from_row)?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .map_err(db_err)
    }

    pub fn list_unfinished(&self) -> Result<Vec<RunRow>> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT * FROM workflow_run WHERE status IN ('queued','running','stopping') \
                     ORDER BY updated_at ASC",
                )?;
                let rows = stmt.query_map([], RunRow::from_row)?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .map_err(db_err)
    }

    /// 租约扫描（§3.6）：本实例 owner、仍在 running/stopping、心跳早于 `before_ms` 的 run。
    pub fn stalled_runs(&self, instance: &str, before_ms: i64) -> Result<Vec<RunRow>> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT * FROM workflow_run WHERE owner_instance = ?1 \
                     AND status IN ('running','stopping') AND heartbeat_at < ?2 \
                     ORDER BY heartbeat_at ASC",
                )?;
                let rows = stmt.query_map(params![instance, before_ms], RunRow::from_row)?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .map_err(db_err)
    }

    /// `running → stopping` 的 CAS 与 `run_stopping` 事件同事务（Cancel / 租约扫描 / 撤权
    /// 共用）。行已不在 running（刚终态或已在 stopping）返回 Ok(false)，什么都不写。
    /// `stop_origin == user` 时立下 fresh-auth 要求（只升不降）。
    pub fn begin_stopping(
        &self,
        run_id: &str,
        exec_generation: u32,
        reason: &str,
        origin: &str,
    ) -> Result<bool> {
        let event = json!({
            "event": "run_stopping",
            "exec_generation": exec_generation,
            "reason": reason,
            "stop_origin": origin,
        });
        let journal = self.journal.clone();
        self.transaction(|tx| {
            let changed = tx
                .execute(
                    "UPDATE workflow_run SET status = 'stopping', stop_reason = ?2, \
                     stop_origin = ?3, requires_fresh_auth = max(?4, requires_fresh_auth), \
                     updated_at = ?5 WHERE run_id = ?1 AND status = 'running'",
                    params![
                        run_id,
                        reason,
                        origin,
                        (origin == STOP_USER) as i64,
                        now_ms()
                    ],
                )
                .map_err(db_err)?;
            if changed == 0 {
                return Ok(false);
            }
            journal
                .append_tx(tx, JOURNAL_WORKFLOW, run_id, &event)
                .map_err(db_err)?;
            Ok(true)
        })
    }

    #[allow(clippy::too_many_arguments)] // 参数即 workflow_run 状态列，拆结构体无增益
    pub fn set_status(
        &self,
        run_id: &str,
        status: &str,
        queued_reason: Option<&str>,
        stop_reason: Option<&str>,
        stop_origin: Option<&str>,
        requires_fresh_auth: bool,
        resumable: bool,
    ) -> Result<()> {
        let now = now_ms();
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_run SET status = ?2, queued_reason = ?3, stop_reason = ?4, \
                     stop_origin = ?5, requires_fresh_auth = ?6, resumable = ?7, updated_at = ?8 \
                     WHERE run_id = ?1",
                    params![
                        run_id,
                        status,
                        queued_reason,
                        stop_reason,
                        stop_origin,
                        requires_fresh_auth as i64,
                        resumable as i64,
                        now
                    ],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    pub fn heartbeat(&self, run_id: &str, elapsed_ms: i64) -> Result<()> {
        let now = now_ms();
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_run SET heartbeat_at = ?2, lease_until = ?3, \
                     elapsed_runtime_ms = elapsed_runtime_ms + ?4, updated_at = ?2 \
                     WHERE run_id = ?1 AND status IN ('running','stopping')",
                    params![run_id, now, now + 60_000, elapsed_ms.max(0)],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    pub fn add_elapsed(&self, run_id: &str, elapsed_ms: i64) -> Result<()> {
        if elapsed_ms <= 0 {
            return Ok(());
        }
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_run SET elapsed_runtime_ms = elapsed_runtime_ms + ?2, \
                     updated_at = ?3 WHERE run_id = ?1",
                    params![run_id, elapsed_ms, now_ms()],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    pub fn set_result(
        &self,
        run_id: &str,
        result_json: Option<&str>,
        result_handle: Option<&str>,
    ) -> Result<()> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_run SET result_json = ?2, result_handle = ?3, updated_at = ?4 \
                     WHERE run_id = ?1",
                    params![run_id, result_json, result_handle, now_ms()],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    pub fn set_execution_config(&self, run_id: &str, config: &Value) -> Result<()> {
        let text = serde_json::to_string(config).map_err(db_err)?;
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_run SET execution_config = ?2, updated_at = ?3 \
                     WHERE run_id = ?1",
                    params![run_id, text, now_ms()],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    pub fn snapshot_paths(&self) -> Result<Vec<String>> {
        self.db
            .with(|conn| {
                let mut stmt = conn
                    .prepare("SELECT snapshot_path FROM workflow_run WHERE snapshot_path <> ''")?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .map_err(db_err)
    }

    // -----------------------------------------------------------------------
    // 配额所有权
    // -----------------------------------------------------------------------

    pub fn set_capacity_owner(&self, run_id: &str, owner: Option<&str>) -> Result<()> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_run SET capacity_owner = ?2, updated_at = ?3 WHERE run_id = ?1",
                    params![run_id, owner, now_ms()],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    pub fn set_capacity_claim(&self, run_id: &str, claim_for: Option<&str>) -> Result<()> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_run SET capacity_claim_for = ?2, updated_at = ?3 \
                     WHERE run_id = ?1",
                    params![run_id, claim_for, now_ms()],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    /// 启动期：终态行不该再持有许可（旧版本终态事务没清 owner 时留下的残留），先清掉，
    /// 否则每次重启都把它们重新计入 `capacity`，几次之后所有 Create 都报 capacity_exceeded。
    pub fn clear_stale_capacity_owners(&self) -> Result<()> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_run SET capacity_owner = NULL, updated_at = ?1 \
                     WHERE capacity_owner IS NOT NULL \
                     AND status IN ('completed', 'errored', 'stopped')",
                    params![now_ms()],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    /// 仍持有许可的活动 run（queued 后继持 claim 转来的 owner 也算）。
    pub fn capacity_owners(&self) -> Result<Vec<String>> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT DISTINCT capacity_owner FROM workflow_run \
                     WHERE capacity_owner IS NOT NULL AND capacity_owner <> '' \
                     AND status IN ('queued', 'running', 'stopping')",
                )?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .map_err(db_err)
    }

    /// 许可归属无法唯一推导的活动行（§3.6 `capacity_unrecoverable`）：许可所有者只能是行
    /// 自己（acquire / 交接都写 `capacity_owner = run_id`），活动行的 owner 指向别的 run
    /// （两行互指、交接写了一半）就说明许可计数不可信。返回 `(run_id, capacity_owner)`。
    pub fn inconsistent_capacity_owners(&self) -> Result<Vec<(String, String)>> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT run_id, capacity_owner FROM workflow_run \
                     WHERE capacity_owner IS NOT NULL AND capacity_owner <> '' \
                     AND capacity_owner <> run_id \
                     AND status IN ('queued', 'running', 'stopping')",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .map_err(db_err)
    }

    /// 等待从前驱交接许可的后继（queued 且 claim 指向它）。
    pub fn successor_claimed_by(&self, predecessor: &str) -> Result<Option<String>> {
        self.db
            .with(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT run_id FROM workflow_run WHERE resumed_from = ?1                          AND status = 'queued' AND capacity_claim_for IS NOT NULL                          ORDER BY created_at ASC LIMIT 1",
                        params![predecessor],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?)
            })
            .map_err(db_err)
    }

    pub fn transfer_capacity(&self, from: &str, to: &str) -> Result<()> {
        self.transaction(|tx| {
            tx.execute(
                "UPDATE workflow_run SET capacity_owner = NULL WHERE run_id = ?1",
                params![from],
            )
            .map_err(db_err)?;
            tx.execute(
                "UPDATE workflow_run SET capacity_owner = ?2, capacity_claim_for = NULL \
                 WHERE run_id = ?1",
                params![to, to],
            )
            .map_err(db_err)?;
            tx.execute(
                "UPDATE workflow_run SET capacity_claim_for = NULL, updated_at = ?2 \
                 WHERE capacity_claim_for = ?1",
                params![to, now_ms()],
            )
            .map_err(db_err)?;
            Ok(())
        })
    }

    // -----------------------------------------------------------------------
    // effect（登记 / 终态由 EffectTracker 在事务里直接写）
    // -----------------------------------------------------------------------

    pub fn list_effects(&self, run_id: &str) -> Result<Vec<EffectRow>> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT * FROM workflow_effect WHERE run_id = ?1 ORDER BY call_index ASC",
                )?;
                let rows = stmt.query_map(params![run_id], EffectRow::from_row)?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .map_err(db_err)
    }

    pub fn get_effect(&self, run_id: &str, effect_key: &str) -> Result<Option<EffectRow>> {
        self.db
            .with(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT * FROM workflow_effect WHERE run_id = ?1 AND effect_key = ?2",
                        params![run_id, effect_key],
                        EffectRow::from_row,
                    )
                    .optional()?)
            })
            .map_err(db_err)
    }

    /// 统一准入谓词：存在 failed/started/unknown 且无有效裁决即阻挡。
    pub fn blocking_effects(
        &self,
        run_id: &str,
        next_exec_generation: u32,
    ) -> Result<Vec<EffectRow>> {
        Ok(self
            .list_effects(run_id)?
            .into_iter()
            .filter(|effect| effect.blocks(next_exec_generation))
            .collect())
    }

    pub fn has_write_effects(&self, run_id: &str) -> Result<bool> {
        self.db
            .with(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM workflow_effect WHERE run_id = ?1 \
                         AND is_write = 1 AND status IN ('started','completed','failed','unknown'))",
                        params![run_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .map(|value| value != 0)
                    .unwrap_or(false))
            })
            .map_err(db_err)
    }

    pub fn decide_effect(
        &self,
        run_id: &str,
        effect_key: &str,
        decision: &str,
        exec_generation: u32,
        substitute: Option<&str>,
        decided_by: &str,
    ) -> Result<()> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_effect SET decision = ?3, decision_exec_generation = ?4, \
                     substitute_result = ?5, decided_by = ?6, updated_at = ?7 \
                     WHERE run_id = ?1 AND effect_key = ?2",
                    params![
                        run_id,
                        effect_key,
                        decision,
                        exec_generation as i64,
                        substitute,
                        decided_by,
                        now_ms()
                    ],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // process（world.run 进程树）
    // -----------------------------------------------------------------------

    pub fn record_process(
        &self,
        run_id: &str,
        exec_generation: u32,
        process_id: &str,
        root_pid: u32,
    ) -> Result<()> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "INSERT OR REPLACE INTO workflow_process (run_id, exec_generation, process_id, \
                     root_pid, state, created_at) VALUES (?1, ?2, ?3, ?4, 'running', ?5)",
                    params![
                        run_id,
                        exec_generation as i64,
                        process_id,
                        root_pid as i64,
                        now_ms()
                    ],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    pub fn finish_process(
        &self,
        run_id: &str,
        exec_generation: u32,
        process_id: &str,
        state: &str,
    ) -> Result<()> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_process SET state = ?4, exited_at = ?5 \
                     WHERE run_id = ?1 AND exec_generation = ?2 AND process_id = ?3",
                    params![run_id, exec_generation as i64, process_id, state, now_ms()],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    pub fn running_processes(&self, run_id: &str, exec_generation: u32) -> Result<Vec<String>> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT process_id FROM workflow_process WHERE run_id = ?1 \
                     AND exec_generation = ?2 AND state IN ('running','unknown')",
                )?;
                let rows = stmt.query_map(params![run_id, exec_generation as i64], |row| {
                    row.get::<_, String>(0)
                })?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .map_err(db_err)
    }

    /// 当前 exec 的全部进程记录（UI 对 `stopping(interrupted)` 做人工确认时需要知道有哪些
    /// `running/unknown` 进程树）：`(process_id, root_pid, state)`。
    pub fn list_processes(
        &self,
        run_id: &str,
        exec_generation: u32,
    ) -> Result<Vec<(String, u32, String)>> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT process_id, root_pid, state FROM workflow_process WHERE run_id = ?1 \
                     AND exec_generation = ?2 ORDER BY created_at ASC",
                )?;
                let rows = stmt.query_map(params![run_id, exec_generation as i64], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?.max(0) as u32,
                        row.get::<_, String>(2)?,
                    ))
                })?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .map_err(db_err)
    }

    pub fn mark_process_exited(
        &self,
        run_id: &str,
        exec_generation: u32,
        process_id: &str,
    ) -> Result<()> {
        self.finish_process(run_id, exec_generation, process_id, "exited")
    }

    // -----------------------------------------------------------------------
    // question
    // -----------------------------------------------------------------------

    /// 答案裁决 CAS：只有第一个并发回答成功。
    pub fn answer_question_cas(&self, question_id: &str, answer: &str) -> Result<bool> {
        self.db
            .with(|conn| {
                let changed = conn.execute(
                    "UPDATE workflow_question SET state = 'answered', answer = ?2, answered_at = ?3 \
                     WHERE question_id = ?1 AND state = 'pending'",
                    params![question_id, answer, now_ms()],
                )?;
                Ok(changed > 0)
            })
            .map_err(db_err)
    }

    pub fn get_question(&self, question_id: &str) -> Result<Option<QuestionRow>> {
        self.db
            .with(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT * FROM workflow_question WHERE question_id = ?1",
                        params![question_id],
                        |row| {
                            Ok(QuestionRow {
                                question_id: row.get("question_id")?,
                                actor: row.get("actor")?,
                                question: row.get("question")?,
                                state: row.get("state")?,
                                answer: row.get("answer")?,
                            })
                        },
                    )
                    .optional()?)
            })
            .map_err(db_err)
    }

    pub fn pending_questions(&self, run_id: &str, limit: usize) -> Result<Vec<QuestionRow>> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT * FROM workflow_question WHERE run_id = ?1 AND state = 'pending' \
                     ORDER BY created_at ASC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![run_id, limit as i64], |row| {
                    Ok(QuestionRow {
                        question_id: row.get("question_id")?,
                        actor: row.get("actor")?,
                        question: row.get("question")?,
                        state: row.get("state")?,
                        answer: row.get("answer")?,
                    })
                })?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .map_err(db_err)
    }

    /// 跨代次问题复用：同一 `question_key` 已回答的答案（Resume 不再问、不重复计费）。
    pub fn find_answered_by_key(&self, run_id: &str, question_key: &str) -> Result<Option<String>> {
        self.db
            .with(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT answer FROM workflow_question WHERE run_id = ?1 AND question_key = ?2                          AND state = 'answered' ORDER BY answered_at DESC LIMIT 1",
                        params![run_id, question_key],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()?
                    .flatten())
            })
            .map_err(db_err)
    }

    // -----------------------------------------------------------------------
    // hole（登记 / 作废由各自的事务直接写）
    // -----------------------------------------------------------------------

    pub fn cas_hole_filling(&self, run_id: &str, hole_id: &str, generation: u32) -> Result<bool> {
        self.db
            .with(|conn| {
                let changed = conn.execute(
                    "UPDATE workflow_hole SET state = 'filling', updated_at = ?4 \
                     WHERE run_id = ?1 AND hole_id = ?2 AND script_generation = ?3 AND state = 'waiting'",
                    params![run_id, hole_id, generation as i64, now_ms()],
                )?;
                Ok(changed > 0)
            })
            .map_err(db_err)
    }

    /// `filling → waiting` CAS：Fill 未提交就退出（回退 / future 被 drop）时放回等待态；
    /// 期间已被 Cancel 置成 aborted 的不动。
    pub fn revert_hole_filling(&self, run_id: &str, hole_id: &str) -> Result<bool> {
        self.db
            .with(|conn| {
                let changed = conn.execute(
                    "UPDATE workflow_hole SET state = 'waiting', updated_at = ?3 \
                     WHERE run_id = ?1 AND hole_id = ?2 AND state = 'filling'",
                    params![run_id, hole_id, now_ms()],
                )?;
                Ok(changed > 0)
            })
            .map_err(db_err)
    }

    pub fn set_hole_state(&self, run_id: &str, hole_id: &str, state: &str) -> Result<()> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_hole SET state = ?3, updated_at = ?4 \
                     WHERE run_id = ?1 AND hole_id = ?2",
                    params![run_id, hole_id, state, now_ms()],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    pub fn set_hole_rejected(&self, run_id: &str, hole_id: &str, hash: &str) -> Result<()> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_hole SET last_rejected_hash = ?3, updated_at = ?4 \
                     WHERE run_id = ?1 AND hole_id = ?2",
                    params![run_id, hole_id, hash, now_ms()],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    pub fn get_hole(&self, run_id: &str, hole_id: &str) -> Result<Option<HoleRow>> {
        self.db
            .with(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT * FROM workflow_hole WHERE run_id = ?1 AND hole_id = ?2",
                        params![run_id, hole_id],
                        hole_row,
                    )
                    .optional()?)
            })
            .map_err(db_err)
    }

    pub fn list_holes(&self, run_id: &str) -> Result<Vec<HoleRow>> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT * FROM workflow_hole WHERE run_id = ?1 ORDER BY created_at ASC",
                )?;
                let rows = stmt.query_map(params![run_id], hole_row)?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .map_err(db_err)
    }

    // -----------------------------------------------------------------------
    // notification（入队由各自的事务经 enqueue_notification_tx 直接写）
    // -----------------------------------------------------------------------

    pub fn claim_notifications(&self, limit: usize, now: i64) -> Result<Vec<NotificationRow>> {
        self.transaction(|tx| {
            let rows = {
                let mut stmt = tx
                    .prepare(
                        "SELECT * FROM workflow_notification WHERE state = 'pending' AND next_at <= ?1 \
                         ORDER BY next_at ASC LIMIT ?2",
                    )
                    .map_err(db_err)?;
                let mapped = stmt
                    .query_map(params![now, limit as i64], notification_row)
                    .map_err(db_err)?;
                let mut out = Vec::new();
                for row in mapped {
                    out.push(row.map_err(db_err)?);
                }
                out
            };
            for row in &rows {
                tx.execute(
                    "UPDATE workflow_notification SET state = 'scheduled', scheduled_at = ?2, \
                     attempts = attempts + 1 WHERE id = ?1 AND state = 'pending'",
                    params![row.id, now],
                )
                .map_err(db_err)?;
            }
            Ok(rows)
        })
    }

    /// 接收端认领（CAS）：只有还没 consumed 的通知才能被消费一次。`scheduled` 60s 未确认会回
    /// pending 重发，同一条通知可能两次到达接收端——第二次这里返回 false，接收端直接丢弃，
    /// 模型不会收到重复的「run 已完成」。`dead` 的也允许认领（UI 重投后的到达）。
    pub fn mark_notification_consumed(&self, id: &str) -> Result<bool> {
        let changed = self
            .db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_notification SET state = 'consumed', consumed_at = ?2 \
                     WHERE id = ?1 AND state <> 'consumed'",
                    params![id, now_ms()],
                )?)
            })
            .map_err(db_err)?;
        Ok(changed > 0)
    }

    /// 认领后落库失败：退回 pending 立刻重发（否则这条通知就在 consumed 态里静默消失）。
    pub fn release_notification(&self, id: &str, error: &str) -> Result<()> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_notification SET state = 'pending', consumed_at = NULL, \
                     next_at = ?2, last_error = ?3 WHERE id = ?1 AND state = 'consumed'",
                    params![id, now_ms(), error],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    pub fn reschedule_notification(&self, id: &str, next_at: i64, error: &str) -> Result<()> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_notification SET state = 'pending', next_at = ?2, \
                     last_error = ?3 WHERE id = ?1 AND state = 'scheduled'",
                    params![id, next_at, error],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    /// `scheduled` 超时未确认（默认 60s）→ 回 pending 重发。
    pub fn requeue_stale_notifications(&self, stale_before: i64) -> Result<usize> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_notification SET state = 'pending', next_at = ?1 \
                     WHERE state = 'scheduled' AND scheduled_at IS NOT NULL AND scheduled_at < ?2",
                    params![now_ms(), stale_before],
                )?)
            })
            .map_err(db_err)
    }

    pub fn dead_notification(&self, id: &str, error: &str) -> Result<()> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_notification SET state = 'dead', last_error = ?2 WHERE id = ?1",
                    params![id, error],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    pub fn resend_notification(&self, id: &str) -> Result<bool> {
        self.db
            .with(|conn| {
                let changed = conn.execute(
                    "UPDATE workflow_notification SET state = 'pending', attempts = 0, next_at = ?2, \
                     last_error = NULL WHERE id = ?1 AND state = 'dead'",
                    params![id, now_ms()],
                )?;
                Ok(changed > 0)
            })
            .map_err(db_err)
    }

    pub fn list_notifications(&self, run_id: &str, limit: usize) -> Result<Vec<NotificationRow>> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT * FROM workflow_notification WHERE run_id = ?1 \
                     ORDER BY created_at DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![run_id, limit as i64], notification_row)?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .map_err(db_err)
    }

    // -----------------------------------------------------------------------
    // request（幂等）
    // -----------------------------------------------------------------------

    pub fn get_request(&self, scope: &str, request_id: &str) -> Result<Option<RequestRow>> {
        self.db
            .with(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT digest, state, run_id, response \
                         FROM workflow_request WHERE scope = ?1 AND request_id = ?2",
                        params![scope, request_id],
                        |row| {
                            Ok(RequestRow {
                                digest: row.get(0)?,
                                state: row.get(1)?,
                                run_id: row.get(2)?,
                                response: row.get(3)?,
                            })
                        },
                    )
                    .optional()?)
            })
            .map_err(db_err)
    }

    pub fn mark_request_failed(&self, scope: &str, request_id: &str) -> Result<()> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_request SET state = 'failed', updated_at = ?3 \
                     WHERE scope = ?1 AND request_id = ?2 AND state = 'in_progress'",
                    params![scope, request_id, now_ms()],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    pub fn reset_request(&self, scope: &str, request_id: &str, digest: &str) -> Result<()> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_request SET state = 'in_progress', digest = ?3, run_id = NULL, \
                     response = NULL, updated_at = ?4 WHERE scope = ?1 AND request_id = ?2",
                    params![scope, request_id, digest, now_ms()],
                )?)
            })
            .map_err(db_err)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 投影 / 回填
    // -----------------------------------------------------------------------

    /// 启动投影：从 kv 游标之后按 journal 全局 id 严格升序折叠。
    /// 游标缺失 = 首次升级，全量扫描。
    pub fn project_from_cursor(&self) -> Result<()> {
        let cursor = self
            .journal
            .kv_get(KV_PROJECTION_CURSOR)
            .map_err(db_err)?
            .and_then(|value| value.get("last_event_id").and_then(Value::as_i64))
            .unwrap_or(0);
        let events = self.journal.all_events(JOURNAL_WORKFLOW).map_err(db_err)?;
        let mut applied = 0usize;
        // 同一 stream 的 seq 必须连续（§3.4：缺口视为损坏，停止重建并报 projection_corrupt）。
        // 游标之前的事件也要过一遍计数器，否则本轮第一条事件没有可比的前驱。
        let mut last_seq: HashMap<String, u64> = HashMap::new();
        for (event_id, stream, seq, payload) in events {
            let expected = last_seq.get(&stream).map_or(1, |last| last + 1);
            if seq != expected {
                return Err(format!(
                    "projection_corrupt: journal 流 {stream} 的 seq 不连续（期望 {expected}，实际 {seq}，事件 id {event_id}）"
                ));
            }
            last_seq.insert(stream.clone(), seq);
            if event_id <= cursor {
                continue;
            }
            // 折叠与游标推进同一事务：崩在中间不会留下「表已更新、游标未动」而重放两次。
            let journal = self.journal.clone();
            self.transaction(|tx| {
                apply_event_tx(tx, &stream, &payload, event_id)?;
                journal
                    .kv_set_tx(
                        tx,
                        KV_PROJECTION_CURSOR,
                        &json!({"last_event_id": event_id}),
                    )
                    .map_err(db_err)
            })?;
            applied += 1;
        }
        if applied > 0 {
            tracing::info!(applied, "workflow 投影折叠完成");
        }
        Ok(())
    }

    /// 旧数据回填：没有 cwd/session/脚本的行标 legacy（只读、不可 resume）。
    pub fn mark_legacy_rows(&self) -> Result<usize> {
        self.db
            .with(|conn| {
                Ok(conn.execute(
                    "UPDATE workflow_run SET legacy = 1, resumable = 0 \
                     WHERE (cwd IS NULL OR cwd = '') AND legacy = 0",
                    [],
                )?)
            })
            .map_err(db_err)
    }

    pub fn referenced_snapshots(&self) -> Result<HashSet<std::path::PathBuf>> {
        Ok(self
            .snapshot_paths()?
            .into_iter()
            .filter(|path| !path.is_empty())
            .map(std::path::PathBuf::from)
            .collect())
    }
}

// ---------------------------------------------------------------------------
// 事务内 helper
// ---------------------------------------------------------------------------

pub fn insert_run_tx(tx: &Transaction<'_>, run: &RunRow) -> Result<()> {
    tx.execute(
        "INSERT INTO workflow_run (run_id, cwd, session_key, label, source_path, snapshot_path, \
         snapshot_hash, script_generation, exec_generation, legacy, resumable, status, queued_reason, \
         stop_reason, stop_origin, requires_fresh_auth, capacity_owner, capacity_claim_for, \
         resumed_from, superseded_by, started_by, execution_config, owner_instance, lease_until, \
         last_event_id, elapsed_runtime_ms, cache_closed, created_at, updated_at, heartbeat_at, \
         spent_tokens, result_json, result_handle, error) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, \
         ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34) \
         ON CONFLICT(run_id) DO UPDATE SET updated_at = excluded.updated_at \
         WHERE excluded.last_event_id > workflow_run.last_event_id",
        params![
            run.run_id,
            run.cwd,
            run.session_key,
            run.label,
            run.source_path,
            run.snapshot_path,
            run.snapshot_hash,
            run.script_generation as i64,
            run.exec_generation as i64,
            run.legacy as i64,
            run.resumable as i64,
            run.status,
            run.queued_reason,
            run.stop_reason,
            run.stop_origin,
            run.requires_fresh_auth as i64,
            run.capacity_owner,
            run.capacity_claim_for,
            run.resumed_from,
            run.superseded_by,
            run.started_by,
            run.execution_config,
            run.owner_instance,
            run.lease_until,
            run.last_event_id,
            run.elapsed_runtime_ms,
            run.cache_closed as i64,
            run.created_at,
            run.updated_at,
            run.heartbeat_at,
            run.spent_tokens,
            run.result_json,
            run.result_handle,
            run.error
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)] // 参数即 workflow_script_version 列
pub fn insert_script_version_tx(
    tx: &Transaction<'_>,
    run_id: &str,
    generation: u32,
    path: &str,
    hash: &str,
    parent_hash: Option<&str>,
    reason: &str,
    replay_prefix: bool,
) -> Result<()> {
    tx.execute(
        "INSERT INTO workflow_script_version (run_id, script_generation, path, hash, parent_hash, \
         reason, replay_prefix, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT(run_id, script_generation) DO UPDATE SET path = excluded.path, hash = excluded.hash",
        params![
            run_id,
            generation as i64,
            path,
            hash,
            parent_hash,
            reason,
            replay_prefix as i64,
            now_ms()
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

pub fn enqueue_notification_tx(
    tx: &Transaction<'_>,
    id: &str,
    run_id: &str,
    session_key: &str,
    channel: &str,
    chat_id: &str,
    payload: &Value,
) -> Result<()> {
    let now = now_ms();
    tx.execute(
        "INSERT OR IGNORE INTO workflow_notification (id, run_id, session_key, channel, chat_id, \
         payload, state, attempts, next_at, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 0, ?7, ?7)",
        params![
            id,
            run_id,
            session_key,
            channel,
            chat_id,
            serde_json::to_string(payload).map_err(db_err)?,
            now
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

pub fn set_request_done_tx(
    tx: &Transaction<'_>,
    scope: &str,
    request_id: &str,
    response: &str,
) -> Result<()> {
    tx.execute(
        "UPDATE workflow_request SET state = 'done', response = ?3, updated_at = ?4 \
         WHERE scope = ?1 AND request_id = ?2",
        params![scope, request_id, response, now_ms()],
    )
    .map_err(db_err)?;
    Ok(())
}

fn hole_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HoleRow> {
    Ok(HoleRow {
        hole_id: row.get("hole_id")?,
        script_generation: row.get::<_, i64>("script_generation")?.max(0) as u32,
        name: row.get("name")?,
        prompt: row.get("prompt")?,
        state: row.get("state")?,
        last_rejected_hash: row.get("last_rejected_hash")?,
    })
}

fn notification_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NotificationRow> {
    Ok(NotificationRow {
        id: row.get("id")?,
        run_id: row.get("run_id")?,
        channel: row.get("channel")?,
        chat_id: row.get("chat_id")?,
        payload: row.get("payload")?,
        state: row.get("state")?,
        attempts: row.get("attempts")?,
        last_error: row.get("last_error")?,
        created_at: row.get::<_, Option<i64>>("created_at")?.unwrap_or(0),
    })
}

/// 把一条 journal 事件折叠进投影表（在调用方事务内执行）。
pub fn apply_event_tx(
    tx: &Transaction<'_>,
    run_id: &str,
    event: &Value,
    event_id: i64,
) -> Result<()> {
    let now = now_ms();
    let kind = event
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let text = |key: &str| event.get(key).and_then(Value::as_str).map(str::to_string);
    let num = |key: &str| event.get(key).and_then(Value::as_i64);
    match kind {
        "run_started" | "run_queued" => {
            let generation = num("script_generation").unwrap_or(1).max(1) as u32;
            let exec_generation = num("exec_generation").unwrap_or(0).max(0) as u32;
            let config = event
                .get("execution_config")
                .cloned()
                .unwrap_or_else(|| json!({}));
            tx.execute(
                "INSERT INTO workflow_run (run_id, cwd, session_key, label, snapshot_path, \
                 snapshot_hash, script_generation, exec_generation, status, queued_reason, \
                 superseded_by, started_by, execution_config, last_event_id, created_at, updated_at, \
                 heartbeat_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15, ?15) \
                 ON CONFLICT(run_id) DO UPDATE SET last_event_id = excluded.last_event_id, \
                 updated_at = excluded.updated_at, status = excluded.status, \
                 snapshot_path = excluded.snapshot_path, snapshot_hash = excluded.snapshot_hash, \
                 script_generation = excluded.script_generation, exec_generation = excluded.exec_generation, \
                 queued_reason = excluded.queued_reason, requires_fresh_auth = 0, \
                 stop_reason = NULL, stop_origin = NULL \
                 WHERE excluded.last_event_id > workflow_run.last_event_id",
                params![
                    run_id,
                    text("cwd").unwrap_or_default(),
                    text("session_key").unwrap_or_default(),
                    text("label").unwrap_or_else(|| run_id.to_string()),
                    text("snapshot_path").unwrap_or_default(),
                    text("script_hash").unwrap_or_default(),
                    generation as i64,
                    exec_generation as i64,
                    if kind == "run_queued" { STATUS_QUEUED } else { STATUS_RUNNING },
                    if kind == "run_queued" { text("queued_reason") } else { None },
                    text("superseded_by"),
                    text("started_by").unwrap_or_else(|| "main".to_string()),
                    serde_json::to_string(&config).map_err(db_err)?,
                    event_id,
                    now
                ],
            )
            .map_err(db_err)?;
        }
        "run_stopping" => {
            // 与 live 的 `begin_stopping` 同口径：用户发起的停止立下 fresh-auth 要求（只升不降），
            // 否则崩在 stopping 窗口、重启重放投影的 run 会丢掉这个要求，模型无 token 即可 Resume。
            tx.execute(
                "UPDATE workflow_run SET status = 'stopping', stop_reason = ?2, stop_origin = ?3, \
                 requires_fresh_auth = max(requires_fresh_auth, CASE WHEN ?3 = 'user' THEN 1 ELSE 0 END), \
                 updated_at = ?4, last_event_id = ?5 WHERE run_id = ?1",
                params![
                    run_id,
                    text("reason"),
                    text("stop_origin"),
                    now,
                    event_id
                ],
            )
            .map_err(db_err)?;
        }
        "run_resumed" => {
            // 与 driver 的生命周期 UPDATE 同口径：新一代凭新授权启动，fresh-auth 与上次停止的
            // reason/origin 清零，否则重放后的投影会把陈旧的 user 停止带进下一次收口。
            tx.execute(
                "UPDATE workflow_run SET status = 'running', exec_generation = ?2, \
                 snapshot_path = coalesce(?3, snapshot_path), snapshot_hash = coalesce(?4, snapshot_hash), \
                 requires_fresh_auth = 0, stop_reason = NULL, stop_origin = NULL, queued_reason = NULL, \
                 updated_at = ?5, last_event_id = ?6 WHERE run_id = ?1",
                params![
                    run_id,
                    num("exec_generation").unwrap_or(1).max(0),
                    text("snapshot_path"),
                    text("script_hash"),
                    now,
                    event_id
                ],
            )
            .map_err(db_err)?;
        }
        "run_retuned" => {
            let config = event
                .get("execution_config")
                .cloned()
                .unwrap_or_else(|| json!({}));
            tx.execute(
                "UPDATE workflow_run SET execution_config = ?2, updated_at = ?3, last_event_id = ?4 \
                 WHERE run_id = ?1",
                params![
                    run_id,
                    serde_json::to_string(&config).map_err(db_err)?,
                    now,
                    event_id
                ],
            )
            .map_err(db_err)?;
        }
        "run_cache_closed" => {
            tx.execute(
                "UPDATE workflow_run SET cache_closed = 1, updated_at = ?2, last_event_id = ?3 \
                 WHERE run_id = ?1",
                params![run_id, now, event_id],
            )
            .map_err(db_err)?;
        }
        "run_completed" => {
            let result = event.get("result_json").map(|value| match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            });
            tx.execute(
                "UPDATE workflow_run SET status = 'completed', result_json = ?2, updated_at = ?3, \
                 last_event_id = ?4, lease_until = NULL WHERE run_id = ?1",
                params![run_id, result, now, event_id],
            )
            .map_err(db_err)?;
        }
        "run_failed" => {
            let error = event.get("error").cloned().unwrap_or(Value::Null);
            tx.execute(
                "UPDATE workflow_run SET status = 'errored', error = ?2, updated_at = ?3, \
                 last_event_id = ?4, lease_until = NULL WHERE run_id = ?1",
                params![
                    run_id,
                    serde_json::to_string(&error).map_err(db_err)?,
                    now,
                    event_id
                ],
            )
            .map_err(db_err)?;
        }
        "run_stopped" => {
            tx.execute(
                "UPDATE workflow_run SET status = 'stopped', stop_reason = ?2, stop_origin = ?3, \
                 requires_fresh_auth = ?4, updated_at = ?5, last_event_id = ?6, lease_until = NULL \
                 WHERE run_id = ?1",
                params![
                    run_id,
                    text("reason"),
                    text("stop_origin"),
                    num("requires_fresh_auth").unwrap_or(0),
                    now,
                    event_id
                ],
            )
            .map_err(db_err)?;
        }
        "run_superseded" => {
            tx.execute(
                "UPDATE workflow_run SET superseded_by = ?2, updated_at = ?3, last_event_id = ?4 \
                 WHERE run_id = ?1",
                params![run_id, text("by"), now, event_id],
            )
            .map_err(db_err)?;
        }
        "run_progress" => {
            // 事件里是跨代累计值；投影按 max 折叠，重放旧事件不会把已记账的耗时往回拨。
            tx.execute(
                "UPDATE workflow_run SET heartbeat_at = ?2, \
                 elapsed_runtime_ms = max(elapsed_runtime_ms, coalesce(?3, 0)), \
                 updated_at = ?2, last_event_id = ?4 WHERE run_id = ?1",
                params![run_id, now, num("elapsed_runtime_ms"), event_id],
            )
            .map_err(db_err)?;
        }
        "effect" => {
            let key = text("effect_key").unwrap_or_default();
            if key.is_empty() {
                return Ok(());
            }
            let status = text("status").unwrap_or_else(|| "started".to_string());
            tx.execute(
                "INSERT INTO workflow_effect (run_id, effect_key, first_script_generation, \
                 last_exec_generation, call_index, op, is_write, args_hash, args_preview, status, \
                 result, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?10, ?11, ?11) \
                 ON CONFLICT(run_id, effect_key) DO UPDATE SET status = excluded.status, \
                 result = excluded.result, last_exec_generation = excluded.last_exec_generation, \
                 updated_at = excluded.updated_at",
                params![
                    run_id,
                    key,
                    num("script_generation").unwrap_or(1),
                    num("exec_generation").unwrap_or(0),
                    num("call_index").unwrap_or(0),
                    text("op").unwrap_or_default(),
                    // 事件里是 JSON bool（见 EffectTracker::begin）；按整数读恒为 0 会让重建的
                    // 投影把写 effect 当成读 effect。
                    event
                        .get("is_write")
                        .and_then(|value| value
                            .as_bool()
                            .or_else(|| value.as_i64().map(|n| n != 0)))
                        .unwrap_or(false) as i64,
                    text("args_hash").unwrap_or_default(),
                    status,
                    event.get("result").map(|value| match value {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    }),
                    now
                ],
            )
            .map_err(db_err)?;
        }
        "effect_decided" => {
            let key = text("effect_key").unwrap_or_default();
            if !key.is_empty() {
                tx.execute(
                    "UPDATE workflow_effect SET decision = ?3, decision_exec_generation = ?4,                      substitute_result = ?5, decided_by = ?6, updated_at = ?7                      WHERE run_id = ?1 AND effect_key = ?2",
                    params![
                        run_id,
                        key,
                        text("decision"),
                        num("decision_exec_generation"),
                        event.get("substitute_result").map(|value| value.to_string()),
                        text("decided_by"),
                        now
                    ],
                )
                .map_err(db_err)?;
            }
        }
        "question_opened" => {
            let question_id = text("question_id").unwrap_or_default();
            if !question_id.is_empty() {
                tx.execute(
                    "INSERT OR REPLACE INTO workflow_question (question_id, run_id, exec_generation, \
                     actor, question, state, created_at, question_key) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7)",
                    params![
                        question_id,
                        run_id,
                        num("exec_generation").unwrap_or(0),
                        text("actor").unwrap_or_default(),
                        text("question").unwrap_or_default(),
                        now,
                        text("question_key")
                    ],
                )
                .map_err(db_err)?;
            }
        }
        "question_answered" => {
            let question_id = text("question_id").unwrap_or_default();
            if !question_id.is_empty() {
                tx.execute(
                    "UPDATE workflow_question SET state = 'answered', answer = ?2, answered_at = ?3 \
                     WHERE question_id = ?1 AND state = 'pending'",
                    params![question_id, text("answer"), now],
                )
                .map_err(db_err)?;
            }
        }
        "question_aborted" => {
            tx.execute(
                "UPDATE workflow_question SET state = 'aborted' WHERE question_id = ?1 AND state = 'pending'",
                params![text("question_id").unwrap_or_default()],
            )
            .map_err(db_err)?;
        }
        "hole_opened" => {
            tx.execute(
                "INSERT OR REPLACE INTO workflow_hole (run_id, hole_id, script_generation, name, \
                 prompt, state, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'waiting', ?6, ?6)",
                params![
                    run_id,
                    text("hole_id").unwrap_or_default(),
                    num("script_generation").unwrap_or(1),
                    text("name").unwrap_or_default(),
                    text("prompt"),
                    now
                ],
            )
            .map_err(db_err)?;
        }
        "hole_filled" => {
            tx.execute(
                "UPDATE workflow_hole SET state = 'filled', updated_at = ?3 WHERE run_id = ?1 AND hole_id = ?2",
                params![run_id, text("hole_id").unwrap_or_default(), now],
            )
            .map_err(db_err)?;
            tx.execute(
                "UPDATE workflow_run SET script_generation = coalesce(?2, script_generation),                  snapshot_path = coalesce(?3, snapshot_path),                  snapshot_hash = coalesce(?4, snapshot_hash), updated_at = ?5, last_event_id = ?6                  WHERE run_id = ?1",
                params![
                    run_id,
                    num("script_generation"),
                    text("snapshot_path"),
                    text("snapshot_hash"),
                    now,
                    event_id
                ],
            )
            .map_err(db_err)?;
        }
        "hole_aborted" => {
            tx.execute(
                "UPDATE workflow_hole SET state = 'aborted', updated_at = ?3 \
                 WHERE run_id = ?1 AND hole_id = ?2",
                params![run_id, text("hole_id").unwrap_or_default(), now],
            )
            .map_err(db_err)?;
        }
        _ => {}
    }
    Ok(())
}
