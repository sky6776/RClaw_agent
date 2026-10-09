//! SQLite-based job queue
//! Mirrors gbrain's src/core/jobs.ts
//!
//! Simple persistent job queue backed by the jobs table.
//! Supports priority ordering, retry with max_attempts, and status transitions.

use crate::error::{GBrainError, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Job status
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    Running,
    Completed,
    Failed,
    /// KB pause/reprocess 标记的取消状态，不同于 Failed（终态）
    Cancelled,
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl JobStatus {
    fn from_str_lossy(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Failed, // Unknown statuses default to Failed (safe: prevents re-processing)
        }
    }
}

/// A job record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: i64,
    pub job_type: String,
    pub payload: serde_json::Value,
    pub status: JobStatus,
    pub priority: i32,
    pub attempts: i32,
    pub max_attempts: i32,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

/// Input for creating a new job
#[derive(Debug, Clone)]
pub struct JobInput {
    pub job_type: String,
    pub payload: serde_json::Value,
    pub priority: Option<i32>,
    pub max_attempts: Option<i32>,
}

/// SQLite job queue
pub struct JobQueue<'a> {
    conn: &'a Connection,
}

impl<'a> JobQueue<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// jobs 表已包含在 SCHEMA_DDL 中，无需额外初始化
    pub fn init(&self) -> Result<()> {
        Ok(())
    }

    /// Enqueue a new job
    pub fn enqueue(&self, input: JobInput) -> Result<i64> {
        let payload_str = input.payload.to_string();
        self.conn.execute(
            "INSERT INTO jobs (job_type, payload, priority, max_attempts)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                input.job_type,
                payload_str,
                input.priority.unwrap_or(0),
                input.max_attempts.unwrap_or(3),
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        debug!(id, job_type = %input.job_type, "Job enqueued");
        Ok(id)
    }

    /// Dequeue the next pending job (highest priority first, then oldest)
    /// Atomic: uses WHERE status = 'pending' guard to prevent concurrent workers
    /// claiming the same job.
    /// Uses a loop with retry limit instead of recursion to prevent stack overflow
    /// under high contention.
    pub fn dequeue(&self) -> Result<Option<Job>> {
        self.dequeue_inner(None)
    }

    /// Dequeue the next pending job of a specific type.
    /// Same atomic semantics as `dequeue` but filters by job_type.
    pub fn dequeue_by_type(&self, job_type: &str) -> Result<Option<Job>> {
        self.dequeue_inner(Some(job_type))
    }

    /// M31: 任务状态机说明
    ///
    /// 状态转换图:
    ///   pending ──(dequeue 原子抢占)──→ running ──(complete)──→ completed
    ///                                      │
    ///                                      └──(fail, attempts < max_attempts)──→ pending (重试)
    ///                                      └──(fail, attempts >= max_attempts)──→ failed (终态)
    ///
    /// 关键不变量:
    /// - dequeue 使用 `WHERE status = 'pending'` 做乐观锁抢占，防止并发 worker 竞争同一任务
    /// - complete/fail 使用 `WHERE status = 'running'` 确保只有持有任务的 worker 才能转换状态
    /// - 超过 max_attempts 的任务进入 failed 终态，不再被 dequeue 捞取
    fn dequeue_inner(&self, job_type_filter: Option<&str>) -> Result<Option<Job>> {
        let max_retries = 10;
        for _ in 0..max_retries {
            // Find next pending job (optionally filtered by type)
            let job = if let Some(jt) = job_type_filter {
                let mut stmt = self.conn.prepare(
                    "SELECT id, job_type, payload, status, priority, attempts, max_attempts, error, created_at, updated_at, started_at, completed_at
                     FROM jobs
                     WHERE status = 'pending' AND job_type = ?1
                     ORDER BY priority DESC, created_at ASC
                     LIMIT 1",
                )?;
                stmt.query_row(params![jt], Self::row_to_job).ok()
            } else {
                let mut stmt = self.conn.prepare(
                    "SELECT id, job_type, payload, status, priority, attempts, max_attempts, error, created_at, updated_at, started_at, completed_at
                     FROM jobs
                     WHERE status = 'pending'
                     ORDER BY priority DESC, created_at ASC
                     LIMIT 1",
                )?;
                stmt.query_row([], Self::row_to_job).ok()
            };

            let Some(job) = job else {
                return Ok(None);
            };

            // Atomic claim: only transition if still pending (prevents concurrent dequeue races)
            let rows = self.conn.execute(
                "UPDATE jobs SET status = 'running', started_at = datetime('now'), updated_at = datetime('now'), attempts = attempts + 1
                 WHERE id = ?1 AND status = 'pending'",
                params![job.id],
            )?;

            if rows > 0 {
                let mut running_job = job;
                running_job.status = JobStatus::Running;
                running_job.attempts += 1;
                return Ok(Some(running_job));
            }
            // 另一个 worker 已抢占此 job — 短暂休眠后重试，避免忙等待浪费 CPU
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // Exceeded retry limit — no job could be claimed
        Ok(None)
    }

    /// Mark a job as completed
    /// Guarded: only transitions from 'running' state (mirrors fail() pattern)
    pub fn complete(&self, job_id: i64) -> Result<()> {
        let rows = self.conn.execute(
            "UPDATE jobs SET status = 'completed', completed_at = datetime('now'), updated_at = datetime('now') WHERE id = ?1 AND status = 'running'",
            params![job_id],
        )?;
        if rows == 0 {
            warn!(job_id, "Job complete: no running job found (may have been claimed by another worker or already completed)");
        } else {
            debug!(job_id, "Job completed");
        }
        Ok(())
    }

    /// Mark a job as failed (will be retried if attempts < max_attempts)
    /// Atomic: single UPDATE with CASE expression prevents stale-data race
    /// and guards with WHERE status = 'running' to avoid overwriting other states.
    pub fn fail(&self, job_id: i64, error: &str) -> Result<()> {
        let rows = self.conn.execute(
            "UPDATE jobs SET
                status = CASE WHEN attempts < max_attempts THEN 'pending' ELSE 'failed' END,
                error = ?2,
                updated_at = datetime('now'),
                completed_at = CASE WHEN attempts >= max_attempts THEN datetime('now') ELSE NULL END
             WHERE id = ?1 AND status = 'running'",
            params![job_id, error],
        )?;

        if rows == 0 {
            warn!(
                job_id,
                "Job fail: no running job found (may have been claimed by another worker)"
            );
        } else {
            // Read back the actual state for logging
            let (status, attempts, max_attempts): (String, i32, i32) = self
                .conn
                .query_row(
                    "SELECT status, attempts, max_attempts FROM jobs WHERE id = ?1",
                    params![job_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap_or(("unknown".to_string(), 0, 0));

            if status == "pending" {
                debug!(job_id, attempts, max_attempts, "Job failed, will retry");
            } else {
                warn!(job_id, attempts, max_attempts, "Job permanently failed");
            }
        }
        Ok(())
    }

    /// List jobs by status
    pub fn list(&self, status: Option<&JobStatus>, limit: Option<usize>) -> Result<Vec<Job>> {
        let limit = limit.unwrap_or(50);
        let status_str = status.map(|s| s.to_string());

        let jobs: Vec<Job> = if let Some(ref st) = status_str {
            let mut stmt = self.conn.prepare(
                "SELECT id, job_type, payload, status, priority, attempts, max_attempts, error, created_at, updated_at, started_at, completed_at
                 FROM jobs WHERE status = ?1 ORDER BY created_at DESC LIMIT ?2",
            )?;
            let x: Vec<Job> = stmt
                .query_map(params![st, limit], Self::row_to_job)?
                .filter_map(|r| r.ok())
                .collect();
            x
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT id, job_type, payload, status, priority, attempts, max_attempts, error, created_at, updated_at, started_at, completed_at
                 FROM jobs ORDER BY created_at DESC LIMIT ?1",
            )?;
            let x: Vec<Job> = stmt
                .query_map(params![limit], Self::row_to_job)?
                .filter_map(|r| r.ok())
                .collect();
            x
        };

        Ok(jobs)
    }

    /// Get a specific job by ID
    pub fn get(&self, job_id: i64) -> Result<Option<Job>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, job_type, payload, status, priority, attempts, max_attempts, error, created_at, updated_at, started_at, completed_at
             FROM jobs WHERE id = ?1",
        )?;
        let result = stmt.query_row(params![job_id], Self::row_to_job).ok();
        Ok(result)
    }

    /// Cancel a pending job
    /// 使用 status='cancelled' + cancel_reason='manual'，
    /// 与 KB pause/reprocess 的取消保持一致状态值，便于过滤和统计。
    pub fn cancel(&self, job_id: i64) -> Result<()> {
        let rows = self.conn.execute(
            "UPDATE jobs SET status = 'cancelled', cancel_reason = 'manual', updated_at = datetime('now')
             WHERE id = ?1 AND status = 'pending'",
            params![job_id],
        )?;
        if rows == 0 {
            return Err(GBrainError::InvalidInput(format!(
                "Job {} not found or not pending",
                job_id
            )));
        }
        debug!(job_id, "Job cancelled");
        Ok(())
    }

    /// Get count of jobs by status
    pub fn count_by_status(&self) -> Result<std::collections::HashMap<String, usize>> {
        let mut counts = std::collections::HashMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT status, COUNT(*) as cnt FROM jobs GROUP BY status")?;
        let rows: Vec<(String, usize)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        for (status, count) in rows {
            counts.insert(status, count);
        }
        Ok(counts)
    }

    /// Process all pending jobs with the given handler
    pub fn process_all<F>(&self, handler: F) -> Result<usize>
    where
        F: Fn(&Job) -> std::result::Result<(), String>,
    {
        let mut processed = 0;
        loop {
            let job = self.dequeue()?;
            let Some(job) = job else {
                break;
            };
            match handler(&job) {
                Ok(()) => {
                    self.complete(job.id)?;
                }
                Err(e) => {
                    self.fail(job.id, &e)?;
                }
            }
            processed += 1;
        }
        if processed > 0 {
            info!(processed, "Processed jobs");
        }
        Ok(processed)
    }

    fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
        let payload_str: String = row.get(2)?;
        let payload: serde_json::Value =
            serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
        Ok(Job {
            id: row.get(0)?,
            job_type: row.get(1)?,
            payload,
            status: JobStatus::from_str_lossy(&row.get::<_, String>(3)?),
            priority: row.get(4)?,
            attempts: row.get(5)?,
            max_attempts: row.get(6)?,
            error: row.get(7)?,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
            started_at: row.get(10)?,
            completed_at: row.get(11)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::schema::SCHEMA_DDL).unwrap();
        conn
    }

    #[test]
    fn test_enqueue_and_dequeue() {
        let conn = setup();
        let queue = JobQueue::new(&conn);

        let id = queue
            .enqueue(JobInput {
                job_type: "embed".to_string(),
                payload: serde_json::json!({"slug": "test"}),
                priority: Some(1),
                max_attempts: None,
            })
            .unwrap();
        assert!(id > 0);

        let job = queue.dequeue().unwrap().unwrap();
        assert_eq!(job.id, id);
        assert_eq!(job.status, JobStatus::Running);
        assert_eq!(job.job_type, "embed");
    }

    #[test]
    fn test_complete_job() {
        let conn = setup();
        let queue = JobQueue::new(&conn);

        let id = queue
            .enqueue(JobInput {
                job_type: "test".to_string(),
                payload: serde_json::json!({}),
                priority: None,
                max_attempts: None,
            })
            .unwrap();

        let job = queue.dequeue().unwrap().unwrap();
        queue.complete(job.id).unwrap();

        let job = queue.get(id).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Completed);
    }

    #[test]
    fn test_fail_and_retry() {
        let conn = setup();
        let queue = JobQueue::new(&conn);

        let id = queue
            .enqueue(JobInput {
                job_type: "test".to_string(),
                payload: serde_json::json!({}),
                priority: None,
                max_attempts: Some(2),
            })
            .unwrap();

        let job = queue.dequeue().unwrap().unwrap();
        queue.fail(job.id, "temporary error").unwrap();

        // Should be back to pending for retry
        let job = queue.get(id).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Pending);

        // Dequeue again and fail — should be permanently failed
        let job = queue.dequeue().unwrap().unwrap();
        queue.fail(job.id, "permanent error").unwrap();

        let job = queue.get(id).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Failed);
    }

    #[test]
    fn test_priority_ordering() {
        let conn = setup();
        let queue = JobQueue::new(&conn);

        queue
            .enqueue(JobInput {
                job_type: "low".to_string(),
                payload: serde_json::json!({}),
                priority: Some(0),
                max_attempts: None,
            })
            .unwrap();
        queue
            .enqueue(JobInput {
                job_type: "high".to_string(),
                payload: serde_json::json!({}),
                priority: Some(10),
                max_attempts: None,
            })
            .unwrap();

        let job = queue.dequeue().unwrap().unwrap();
        assert_eq!(job.job_type, "high");
    }
}
