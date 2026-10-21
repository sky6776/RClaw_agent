//! KB job payload/status helpers
//!
//! Reuses the global `jobs` table for KB document processing tasks.

use crate::error::{GBrainError, Result};
use crate::jobs::{JobInput, JobQueue, JobStatus};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// L2 修复：作业优先级集中配置，便于调整调度策略
// 数值越高越优先处理，默认 0 为标准优先级

/// KB 文档处理作业优先级
const PRIORITY_KB_PROCESS: i32 = 0;
/// OCR 作业优先级（供 pipeline/mcp 等外部入队点引用）
#[allow(dead_code)]
const PRIORITY_KB_OCR: i32 = 0;
/// 重嵌入作业优先级（低于标准，避免占用前台资源）
#[allow(dead_code)]
const PRIORITY_KB_REEMBED: i32 = -1;
/// 同义词挖掘作业优先级（最低，后台静默执行）
const PRIORITY_KB_SYNONYMS: i32 = -2;

/// KB document processing job payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KbProcessPayload {
    pub kind: String, // "kb_process_document"
    pub document_id: i64,
    pub library_id: i64,
    pub processing_run_id: String,
    pub storage_path: String,
    pub extension: String,
}

/// P1-4: KB 索引命令模型。
///
/// 把索引生命周期拆分为独立的命令，每种命令对应一种状态变更，
/// 不再依赖单一巨型 `kb_process_document` payload。便于：
/// - 删除/清理与重建解耦
/// - 各命令可以独立调度和重试
///
/// 调度路径以 `KbIndexCommand` 为主，worker 逐条 claim 并执行。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command")]
pub enum KbIndexCommand {
    /// 创建/更新文档版本（触发 parse → chunk → embed → raptor → activate）
    UpsertDocumentVersion {
        document_id: i64,
        library_id: i64,
        processing_run_id: String,
    },
    /// 删除文档索引（保留 file，仅清理节点/向量/版本）
    DeleteDocumentIndex { document_id: i64 },
    /// 生成/刷新文档摘要
    SummarizeDocument {
        document_id: i64,
        processing_run_id: String,
    },
    /// 重新计算 library 下所有文档的 index_status（巡检/修复）
    ReconcileIndexStatus { library_id: i64 },
    /// P0 修复: 清理退役版本（retired version）的向量/节点/版本数据
    CleanupRetiredVersion { version_id: i64 },
    /// P1 修复: 阶段 2 — 仅对已解析的文档节点执行嵌入（不重新解析）
    EmbedNodes {
        document_id: i64,
        library_id: i64,
        processing_run_id: String,
    },
    /// P1 修复: 阶段 3 — 对已嵌入的文档执行版本激活与最终持久化
    FinalizeIndex {
        document_id: i64,
        library_id: i64,
        processing_run_id: String,
    },
}

impl KbIndexCommand {
    /// 序列化为 job payload JSON（写入 jobs.payload）。
    pub fn to_payload_json(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|e| GBrainError::InvalidInput(format!("序列化 KbIndexCommand 失败: {}", e)))
    }

    /// 反序列化 job payload JSON。
    pub fn from_payload_json(s: &str) -> Result<Self> {
        serde_json::from_str(s)
            .map_err(|e| GBrainError::InvalidInput(format!("反序列化 KbIndexCommand 失败: {}", e)))
    }

    /// 推荐的 job_type 字符串（用于 jobs 表）。
    pub fn job_type(&self) -> &'static str {
        match self {
            Self::UpsertDocumentVersion { .. } => "kb_cmd_upsert_version",
            Self::DeleteDocumentIndex { .. } => "kb_cmd_delete_index",
            Self::SummarizeDocument { .. } => "kb_cmd_summarize",
            Self::ReconcileIndexStatus { .. } => "kb_cmd_reconcile_status",
            Self::CleanupRetiredVersion { .. } => "kb_cmd_cleanup_retired",
            Self::EmbedNodes { .. } => "kb_cmd_embed_nodes",
            Self::FinalizeIndex { .. } => "kb_cmd_finalize_index",
        }
    }

    /// 入队：把命令包装成 JobInput。
    pub fn to_job_input(&self) -> Result<JobInput> {
        let value = serde_json::to_value(self)
            .map_err(|e| GBrainError::InvalidInput(format!("序列化 KbIndexCommand 失败: {}", e)))?;
        Ok(JobInput {
            job_type: self.job_type().to_string(),
            payload: value,
            priority: Some(0),
            max_attempts: Some(3),
        })
    }
}

/// Phase 3: OCR 文档处理 job payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KbOcrPayload {
    pub kind: String, // "kb_ocr_document"
    pub document_id: i64,
    pub library_id: i64,
    pub processing_run_id: String,
    pub storage_path: String,
    /// 需要 OCR 的页码列表（1-based）
    pub pages: Vec<i32>,
    pub submit_mode: String,
    pub provider: String,
    pub model: String,
    /// 是否返回裁剪图片
    pub return_crop_images: bool,
    /// 是否需要版面可视化
    pub need_layout_visualization: bool,
}

/// P5-013: Reembed job payload — re-embed documents into a new embedding index
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KbReembedPayload {
    pub kind: String, // "kb_reembed"
    /// Scope: "document" or "library"
    pub scope: String,
    /// Document ID (when scope is "document")
    pub document_id: Option<i64>,
    /// Library ID (required for both scopes)
    pub library_id: i64,
    /// Target embedding index ID to write embeddings into
    pub target_embedding_index_id: i64,
}

/// Generate a new processing run ID
pub fn new_run_id() -> String {
    let mut hasher = Sha256::new();
    hasher.update(
        chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or(0)
            .to_le_bytes(),
    );
    hasher.update(rand::random::<[u8; 16]>());
    let hash = hasher.finalize();
    format!("run_{}", hex::encode(&hash[..8]))
}

/// Generate a new job ID
pub fn new_job_id() -> String {
    let mut hasher = Sha256::new();
    hasher.update(
        chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or(0)
            .to_le_bytes(),
    );
    hasher.update(rand::random::<[u8; 16]>());
    let hash = hasher.finalize();
    format!("job_{}", hex::encode(&hash[..8]))
}

/// Enqueue a KB document processing job.
/// Returns the database row ID of the new job.
pub fn enqueue_kb_process_job(conn: &Connection, payload: &KbProcessPayload) -> Result<i64> {
    let queue = JobQueue::new(conn);

    let input = JobInput {
        job_type: "kb_process_document".to_string(),
        payload: serde_json::to_value(payload)
            .map_err(|e| GBrainError::Serialization(e.to_string()))?,
        priority: Some(PRIORITY_KB_PROCESS),
        max_attempts: Some(3),
    };
    queue.enqueue(input)
}

/// Get the status of a KB document's processing job by database row ID
pub fn get_kb_job_status(conn: &Connection, job_db_id: i64) -> Result<Option<JobStatus>> {
    let queue = JobQueue::new(conn);
    match queue.get(job_db_id) {
        Ok(Some(job)) => Ok(Some(job.status)),
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Cancel a KB document's processing job by database row ID
pub fn cancel_kb_job(conn: &Connection, job_db_id: i64) -> Result<()> {
    let queue = JobQueue::new(conn);
    queue.cancel(job_db_id)
}

/// Claim and get the next pending KB job.
/// Dequeues from the global jobs table filtering by job_type.
/// Returns (job_db_id, payload) so the caller can complete/fail the job.
pub fn claim_next_kb_job(conn: &Connection) -> Result<Option<(i64, KbProcessPayload)>> {
    let queue = JobQueue::new(conn);
    // Dequeue the next pending job filtering by KB type
    match queue.dequeue_by_type("kb_process_document") {
        Ok(Some(job)) => {
            let payload: KbProcessPayload = serde_json::from_value(job.payload).map_err(|e| {
                GBrainError::Serialization(format!("invalid KB job payload: {}", e))
            })?;
            Ok(Some((job.id, payload)))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    }
}

/// 回收孤儿 running 作业（worker 启动时调用）。
///
/// 进程崩溃/重启或 worker 线程 panic 会把 job 留在 running 终态——
/// `dequeue_inner` 只捞 pending，这些 job 永远不会被重新认领，文档随之
/// 永久卡在 processing。把 running 状态持续超过 `stale_after_minutes`
/// 分钟的作业重置回 pending，让正常的状态机接管。
///
/// 阈值兜底防止误伤其他活跃进程正在处理的作业。
///
/// 回收时同样套用 `max_attempts`：会把进程直接打死（OOM/LMK/栈溢出）的作业永远走不到
/// `fail()`，若无条件复位为 pending 就会"认领 → 崩溃 → 2 小时后再认领"无限循环，
/// attempts 一路递增却永不进入终态。达到上限的直接置 failed，并把对应文档标为 failed，
/// 否则文档会永久停在 processing。
pub fn recover_stale_running_jobs(conn: &Connection, stale_after_minutes: i64) -> Result<usize> {
    const STALE_EXHAUSTED_ERROR: &str = "stale running job exceeded max_attempts";
    let stale_before = format!("-{} minutes", stale_after_minutes);
    // 先把耗尽重试次数的文档作业对应的文档标为 failed（带 run_id 守卫，不污染新 run）
    let failed_docs = conn.execute(
        "UPDATE kb_documents
         SET document_status = 'failed', index_status = 'failed',
             parsing_error = ?2, updated_at = datetime('now')
         WHERE EXISTS (
             SELECT 1 FROM jobs j
             WHERE j.status = 'running'
               AND COALESCE(j.started_at, j.updated_at) < datetime('now', ?1)
               AND j.attempts >= j.max_attempts
               AND j.job_type IN ('kb_process_document', 'kb_ocr_document')
               AND CAST(json_extract(j.payload, '$.document_id') AS INTEGER) = kb_documents.id
               AND COALESCE(json_extract(j.payload, '$.processing_run_id'), '') = kb_documents.processing_run_id
         )
         AND document_status NOT IN ('ready', 'failed', 'deleted')",
        rusqlite::params![stale_before, STALE_EXHAUSTED_ERROR],
    )?;
    let updated = conn.execute(
        "UPDATE jobs
         SET status = CASE WHEN attempts >= max_attempts THEN 'failed' ELSE 'pending' END,
             error = CASE WHEN attempts >= max_attempts THEN ?2 ELSE error END,
             completed_at = CASE WHEN attempts >= max_attempts THEN datetime('now') ELSE NULL END,
             updated_at = datetime('now')
         WHERE status = 'running'
           AND COALESCE(started_at, updated_at) < datetime('now', ?1)",
        rusqlite::params![stale_before, STALE_EXHAUSTED_ERROR],
    )?;
    if updated > 0 {
        tracing::warn!(
            recovered = updated,
            failed_docs,
            stale_after_minutes,
            "jobs: 回收孤儿 running 作业（未耗尽重试的复位为 pending，耗尽的置 failed）"
        );
    }
    Ok(updated)
}

/// P0 修复: 入队 KbIndexCommand 作业。
pub fn enqueue_kb_cmd_job(conn: &Connection, cmd: &KbIndexCommand) -> Result<i64> {
    let queue = JobQueue::new(conn);
    let input = cmd.to_job_input()?;
    queue.enqueue(input)
}

/// P0 修复: 认领下一个 KbIndexCommand 作业。
/// 遍历所有 kb_cmd_* 类型的 pending job，反序列化为 KbIndexCommand。
pub fn claim_next_kb_cmd_job(conn: &Connection) -> Result<Option<(i64, KbIndexCommand)>> {
    let queue = JobQueue::new(conn);
    // 按优先级尝试认领各类型命令作业
    let cmd_types = [
        "kb_cmd_upsert_version",
        "kb_cmd_summarize",
        "kb_cmd_embed_nodes",
        "kb_cmd_finalize_index",
        "kb_cmd_delete_index",
        "kb_cmd_cleanup_retired",
        "kb_cmd_reconcile_status",
    ];
    for cmd_type in &cmd_types {
        match queue.dequeue_by_type(cmd_type) {
            Ok(Some(job)) => {
                let cmd: KbIndexCommand = KbIndexCommand::from_payload_json(
                    &serde_json::to_string(&job.payload).unwrap_or_default(),
                )?;
                return Ok(Some((job.id, cmd)));
            }
            Ok(None) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// P0 修复: 扫描所有 retired 版本并入队清理作业（幂等）。
///
/// 遍历 kb_document_versions 中 index_status='retired' 的记录，
/// 为每个 retired 版本入队一个 CleanupRetiredVersion 命令。
///
/// P2 修复: 入队前检查是否已存在同 version_id 的 pending 清理作业，
/// 避免重复入队导致第二次执行时因版本已清理而失败重试。
pub fn enqueue_cleanup_retired_jobs(conn: &Connection) -> Result<usize> {
    let mut stmt =
        conn.prepare("SELECT id FROM kb_document_versions WHERE index_status = 'retired'")?;
    let retired_ids: Vec<i64> = stmt
        .query_map([], |row| row.get::<_, i64>(0))?
        .filter_map(|r| r.ok())
        .collect();

    let mut count = 0;
    for version_id in retired_ids {
        // 检查是否已存在 pending 或 running 清理作业（幂等去重）
        // running 状态也需要检查：job claim 后从 pending → running，
        // 如果只查 pending 会导致已认领但未完成的作业被重复入队。
        let already_exists: bool = conn
            .query_row(
                "SELECT 1 FROM jobs \
                 WHERE job_type = 'kb_cmd_cleanup_retired' \
                 AND (status = 'pending' OR status = 'running') \
                 AND json_extract(payload, '$.version_id') = ?1 \
                 LIMIT 1",
                rusqlite::params![version_id],
                |_row| Ok(()),
            )
            .is_ok();
        if already_exists {
            continue;
        }
        let cmd = KbIndexCommand::CleanupRetiredVersion { version_id };
        if enqueue_kb_cmd_job(conn, &cmd).is_ok() {
            count += 1;
        }
    }
    Ok(count)
}

/// 认领下一个 re-embed 作业（kb_reembed 或 kb_reembed_node）。
///
/// 优先认领 kb_reembed_node（单节点修复），再认领 kb_reembed（文档级重嵌入）。
/// 返回 (job_db_id, job_type, payload_json) 供 worker 处理。
pub fn claim_next_reembed_job(
    conn: &Connection,
) -> Result<Option<(i64, String, serde_json::Value)>> {
    let queue = JobQueue::new(conn);
    // 优先处理单节点修复
    if let Ok(Some(job)) = queue.dequeue_by_type("kb_reembed_node") {
        return Ok(Some((job.id, "kb_reembed_node".into(), job.payload)));
    }
    if let Ok(Some(job)) = queue.dequeue_by_type("kb_reembed") {
        return Ok(Some((job.id, "kb_reembed".into(), job.payload)));
    }
    Ok(None)
}

/// Mark a KB job as completed by database row ID
pub fn complete_kb_job(conn: &Connection, job_db_id: i64) -> Result<()> {
    let queue = JobQueue::new(conn);
    queue.complete(job_db_id)
}

/// Mark a KB job as failed by database row ID
pub fn fail_kb_job(conn: &Connection, job_db_id: i64, error: &str) -> Result<()> {
    let queue = JobQueue::new(conn);
    queue.fail(job_db_id, error)
}

// ---------------------------------------------------------------------------
// E6: Token synonym mining job
// ---------------------------------------------------------------------------

/// E6: Synonym mining job payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KbMineSynonymsPayload {
    pub kind: String, // "kb_mine_synonyms"
    /// Library ID to mine (None = all libraries)
    pub library_id: Option<i64>,
    /// Full rebuild (ignore existing embeddings)
    pub full: bool,
}

/// Enqueue a synonym mining job. Only enqueues if one isn't already pending.
pub fn enqueue_mine_synonyms_job(
    conn: &Connection,
    library_id: Option<i64>,
    full: bool,
) -> Result<i64> {
    // 使用 IMMEDIATE 事务：立即获取写锁，防止并发调用者同时读到 COUNT=0 后重复入队
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| GBrainError::Database(format!("开启 IMMEDIATE 事务失败: {}", e)))?;
    let result = enqueue_mine_synonyms_job_inner(conn, library_id, full);
    // 统一在外层管理事务关闭：Ok → COMMIT, Err → ROLLBACK
    match &result {
        Ok(_) => {
            if let Err(e) = conn.execute_batch("COMMIT") {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(GBrainError::Database(format!("提交事务失败: {}", e)));
            }
        }
        Err(_) => {
            let _ = conn.execute_batch("ROLLBACK");
        }
    }
    result
}

fn enqueue_mine_synonyms_job_inner(
    conn: &Connection,
    library_id: Option<i64>,
    full: bool,
) -> Result<i64> {
    let existing: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM jobs WHERE job_type = 'kb_mine_synonyms' AND status = 'pending'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| GBrainError::Database(format!("查询待处理同义词任务失败: {}", e)))?;
    if existing > 0 {
        return Ok(0);
    }
    let payload = KbMineSynonymsPayload {
        kind: "kb_mine_synonyms".to_string(),
        library_id,
        full,
    };
    let queue = JobQueue::new(conn);
    let job_id = queue.enqueue(JobInput {
        job_type: "kb_mine_synonyms".to_string(),
        payload: serde_json::to_value(&payload)
            .map_err(|e| GBrainError::Serialization(e.to_string()))?,
        priority: Some(PRIORITY_KB_SYNONYMS),
        max_attempts: Some(2),
    })?;
    // 注意: COMMIT 由外层 enqueue_mine_synonyms_job 统一管理
    Ok(job_id)
}

/// Claim the next pending synonym mining job.
pub fn claim_next_mine_synonyms_job(
    conn: &Connection,
) -> Result<Option<(i64, KbMineSynonymsPayload)>> {
    let queue = JobQueue::new(conn);
    match queue.dequeue_by_type("kb_mine_synonyms") {
        Ok(Some(job)) => {
            let payload: KbMineSynonymsPayload =
                serde_json::from_value(job.payload).map_err(|e| {
                    GBrainError::Serialization(format!("invalid mine_synonyms payload: {}", e))
                })?;
            Ok(Some((job.id, payload)))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// P5-026/P6-010: Job management helpers (list/pause/resume)
// ---------------------------------------------------------------------------

use rusqlite::params;

/// 列出 KB 作业，可选按 library 过滤。
/// 返回 (job_id, status, document_id) 元组列表。
pub fn list_kb_jobs(conn: &Connection, library_id: Option<i64>) -> Result<Vec<(i64, String, i64)>> {
    let sql = if library_id.is_some() {
        "SELECT j.id, j.status, j.payload FROM jobs j \
         WHERE j.job_type='kb_process_document' \
         AND j.payload->>'$.library_id' = ?1 \
         ORDER BY j.id DESC LIMIT 100"
    } else {
        "SELECT j.id, j.status, j.payload FROM jobs j \
         WHERE j.job_type='kb_process_document' \
         ORDER BY j.id DESC LIMIT 100"
    };
    let mut stmt = conn.prepare(sql)?;

    // 统一闭包：解析 payload 中的 document_id
    let parse_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<(i64, String, i64)> {
        let id: i64 = row.get(0)?;
        let status: String = row.get(1)?;
        let payload_str: String = row.get(2)?;
        let payload: serde_json::Value =
            serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
        let doc_id: i64 = payload
            .get("document_id")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        Ok((id, status, doc_id))
    };

    let results: Vec<(i64, String, i64)> = if let Some(lib_id) = library_id {
        stmt.query_map(params![lib_id], parse_row)?
            .filter_map(|r| r.ok())
            .collect()
    } else {
        stmt.query_map([], parse_row)?
            .filter_map(|r| r.ok())
            .collect()
    };
    Ok(results)
}

/// 取消指定文档的待处理 KB job（仅 pending 状态，不影响正在处理的 job）。
///
/// 用于 reprocess 场景：在入队新 job 前，先取消排队的旧 pending job，
/// 防止旧 job 被认领后因 processing_run_id 不匹配被判定为 stale。
/// 注意：不取消 processing 状态的 job，因为 worker 线程正在同步执行中，
/// 改 DB 状态无法中断正在运行的处理，应让 run guard 自然处理。
///
/// 设置 cancel_reason='reprocess' 以区分 pause 取消，
/// 避免 resume_library_jobs 误复活这些已过时的 reprocess 取消。
pub fn cancel_pending_kb_jobs_by_document_id(conn: &Connection, document_id: i64) -> Result<usize> {
    let changed = conn.execute(
        "UPDATE jobs SET status = 'cancelled', cancel_reason = 'reprocess', updated_at = datetime('now') \
         WHERE job_type = 'kb_process_document' \
         AND status = 'pending' \
         AND payload->>'$.document_id' = ?1",
        rusqlite::params![document_id],
    )?;
    Ok(changed)
}

/// Pause KB job processing for a library by cancelling pending jobs.
///
/// 设置 cancel_reason='pause' 以区别于 reprocess 取消，
/// 使 resume_library_jobs 能精确恢复仅由暂停产生的取消。
pub fn pause_library_jobs(conn: &Connection, library_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE jobs SET status='cancelled', cancel_reason='pause', updated_at = datetime('now') \
         WHERE job_type='kb_process_document' AND status='pending' \
         AND payload->>'$.library_id' = ?1",
        params![library_id],
    )?;
    Ok(())
}

/// Resume KB job processing for a library by re-queuing cancelled jobs.
///
/// 仅恢复 cancel_reason='pause' 的取消（由 pause_library_jobs 产生），
/// 不恢复 cancel_reason='reprocess' 的取消（由 reprocess/reupload 产生），
/// 避免复活已过时的 stale job。
/// 兼容旧数据：cancel_reason IS NULL 的也恢复（升级前暂停的作业）。
pub fn resume_library_jobs(conn: &Connection, library_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE jobs SET status='pending', updated_at = datetime('now') \
         WHERE job_type='kb_process_document' AND status='cancelled' \
         AND (cancel_reason = 'pause' OR cancel_reason IS NULL) \
         AND payload->>'$.library_id' = ?1",
        params![library_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 3: OCR 作业入队与认领
// ---------------------------------------------------------------------------

/// 入队 OCR 文档处理作业。
/// 返回新作业的数据库行 ID。
pub fn enqueue_kb_ocr_job(conn: &Connection, payload: &KbOcrPayload) -> Result<i64> {
    let queue = JobQueue::new(conn);
    let input = JobInput {
        job_type: "kb_ocr_document".to_string(),
        payload: serde_json::to_value(payload)
            .map_err(|e| GBrainError::Serialization(e.to_string()))?,
        priority: Some(PRIORITY_KB_OCR),
        max_attempts: Some(3),
    };
    queue.enqueue(input)
}

/// 认领下一个待处理的 OCR 作业。
/// 返回 (job_db_id, payload) 供 worker 处理。
pub fn claim_next_ocr_job(conn: &Connection) -> Result<Option<(i64, KbOcrPayload)>> {
    let queue = JobQueue::new(conn);
    match queue.dequeue_by_type("kb_ocr_document") {
        Ok(Some(job)) => {
            let payload: KbOcrPayload = serde_json::from_value(job.payload).map_err(|e| {
                GBrainError::Serialization(format!("无效的 OCR 作业 payload: {}", e))
            })?;
            Ok(Some((job.id, payload)))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    }
}
