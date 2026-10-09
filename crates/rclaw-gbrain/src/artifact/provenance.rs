//! 来源追溯 — provenance_ledger 的 CRUD 和生命周期管理
//!
//! 负责：
//! 1. 记录 gbrain 事实与 KB 证据的来源关系
//! 2. 文档变化时标记旧 provenance 为 stale
//! 3. 查询某条 brain 事实的完整来源链

use rusqlite::{params, Connection, Row};
use tracing::{debug, info};

use crate::error::{GBrainError, Result};

use super::store;
use super::types::*;

/// 从候选变更记录 provenance
pub fn record_provenance_from_candidate(
    conn: &Connection,
    candidate: &PromotionCandidate,
) -> Result<i64> {
    // 查找 artifact_uid
    let artifact_uid = store::find_artifact_by_id(conn, candidate.artifact_id)
        .map_err(|e| GBrainError::Database(format!("查找 artifact 失败: {}", e)))?
        .map(|a| a.artifact_uid)
        .unwrap_or_default();

    let fact_hash = make_fact_hash(
        &candidate.target_slug,
        &candidate.target_field,
        &candidate.proposed_payload,
        &artifact_uid,
        &candidate
            .kb_node_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
    );

    let now = now_str();
    conn.execute(
        "INSERT INTO provenance_ledger
            (artifact_id, occurrence_id, kb_document_id, kb_node_id,
             promotion_candidate_id, brain_slug, brain_field, fact_hash,
             quote_text, confidence, status, metadata_json,
             created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            candidate.artifact_id,
            candidate.occurrence_id,
            candidate.kb_document_id,
            candidate.kb_node_id,
            candidate.id,
            candidate.target_slug,
            candidate.target_field,
            fact_hash,
            candidate.evidence_json,
            candidate.confidence,
            "active",
            format!("{{\"candidate_type\": \"{}\"}}", candidate.candidate_type),
            now,
            now,
        ],
    )
    .map_err(|e| GBrainError::Database(format!("插入 provenance 失败: {}", e)))?;

    let id = conn.last_insert_rowid();
    debug!(
        "记录 provenance: id={}, slug={}, field={}",
        id, candidate.target_slug, candidate.target_field
    );

    Ok(id)
}

/// 直接记录 provenance（不通过候选）
#[allow(clippy::too_many_arguments)]
pub fn record_provenance(
    conn: &Connection,
    artifact_id: Option<i64>,
    occurrence_id: Option<i64>,
    kb_document_id: Option<i64>,
    kb_node_id: Option<i64>,
    brain_slug: &str,
    brain_field: &str,
    quote_text: &str,
    quote_start: Option<i64>,
    quote_end: Option<i64>,
    page_number: Option<i64>,
    confidence: f64,
    metadata: Option<serde_json::Value>,
    artifact_uid: &str,
) -> Result<i64> {
    let kb_node_id_str = kb_node_id.map(|id| id.to_string()).unwrap_or_default();
    let fact_hash = make_fact_hash(
        brain_slug,
        brain_field,
        quote_text,
        artifact_uid,
        &kb_node_id_str,
    );

    let now = now_str();
    conn.execute(
        "INSERT INTO provenance_ledger
            (artifact_id, occurrence_id, kb_document_id, kb_node_id,
             brain_slug, brain_field, fact_hash,
             quote_text, quote_start, quote_end, page_number,
             confidence, status, metadata_json,
             created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            artifact_id,
            occurrence_id,
            kb_document_id,
            kb_node_id,
            brain_slug,
            brain_field,
            fact_hash,
            quote_text,
            quote_start,
            quote_end,
            page_number,
            confidence,
            "active",
            metadata
                .map(|v| v.to_string())
                .unwrap_or_else(|| "{}".to_string()),
            now,
            now,
        ],
    )
    .map_err(|e| GBrainError::Database(format!("插入 provenance 失败: {}", e)))?;

    let id = conn.last_insert_rowid();
    debug!(
        "record_provenance: id={}, artifact_id={:?}, slug={}, field={}",
        id, artifact_id, brain_slug, brain_field
    );
    Ok(id)
}

/// 查找某条 brain 事实的来源链
pub fn find_provenance_by_brain_slug(
    conn: &Connection,
    brain_slug: &str,
) -> Result<Vec<ProvenanceRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, created_at, updated_at,
                artifact_id, occurrence_id, kb_document_id, kb_node_id,
                promotion_candidate_id, brain_slug, brain_field, fact_hash,
                quote_text, quote_start, quote_end, page_number,
                confidence, status, stale_reason, metadata_json
         FROM provenance_ledger
         WHERE brain_slug = ?1 AND status = 'active'
         ORDER BY created_at DESC",
        )
        .map_err(|e| GBrainError::Database(format!("准备查询 provenance 失败: {}", e)))?;

    let rows = stmt.query_map(params![brain_slug], row_to_provenance_record)?;

    let mut result = Vec::new();
    for row in rows {
        result.push(
            row.map_err(|e| GBrainError::Database(format!("映射 provenance 行失败: {}", e)))?,
        );
    }
    debug!(
        "find_provenance_by_brain_slug: slug={}, count={}",
        brain_slug,
        result.len()
    );
    Ok(result)
}

/// 查找某条 brain 事实 + 字段的来源
pub fn find_provenance_by_brain_field(
    conn: &Connection,
    brain_slug: &str,
    brain_field: &str,
) -> Result<Vec<ProvenanceRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, created_at, updated_at,
                artifact_id, occurrence_id, kb_document_id, kb_node_id,
                promotion_candidate_id, brain_slug, brain_field, fact_hash,
                quote_text, quote_start, quote_end, page_number,
                confidence, status, stale_reason, metadata_json
         FROM provenance_ledger
         WHERE brain_slug = ?1 AND brain_field = ?2 AND status = 'active'
         ORDER BY created_at DESC",
        )
        .map_err(|e| GBrainError::Database(format!("准备查询 provenance 失败: {}", e)))?;

    let rows = stmt.query_map(params![brain_slug, brain_field], |row| {
        row_to_provenance_record(row)
    })?;

    let mut result = Vec::new();
    for row in rows {
        result.push(
            row.map_err(|e| GBrainError::Database(format!("映射 provenance 行失败: {}", e)))?,
        );
    }
    Ok(result)
}

/// 查找某 artifact 的所有 provenance
pub fn find_provenance_by_artifact(
    conn: &Connection,
    artifact_id: i64,
) -> Result<Vec<ProvenanceRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, created_at, updated_at,
                artifact_id, occurrence_id, kb_document_id, kb_node_id,
                promotion_candidate_id, brain_slug, brain_field, fact_hash,
                quote_text, quote_start, quote_end, page_number,
                confidence, status, stale_reason, metadata_json
         FROM provenance_ledger
         WHERE artifact_id = ?1
         ORDER BY created_at DESC",
        )
        .map_err(|e| GBrainError::Database(format!("准备查询 provenance 失败: {}", e)))?;

    let rows = stmt.query_map(params![artifact_id], row_to_provenance_record)?;

    let mut result = Vec::new();
    for row in rows {
        result.push(
            row.map_err(|e| GBrainError::Database(format!("映射 provenance 行失败: {}", e)))?,
        );
    }
    Ok(result)
}

/// 查找某 KB 文档的所有 provenance
pub fn find_provenance_by_kb_document(
    conn: &Connection,
    kb_document_id: i64,
) -> Result<Vec<ProvenanceRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, created_at, updated_at,
                artifact_id, occurrence_id, kb_document_id, kb_node_id,
                promotion_candidate_id, brain_slug, brain_field, fact_hash,
                quote_text, quote_start, quote_end, page_number,
                confidence, status, stale_reason, metadata_json
         FROM provenance_ledger
         WHERE kb_document_id = ?1
         ORDER BY created_at DESC",
        )
        .map_err(|e| GBrainError::Database(format!("准备查询 provenance 失败: {}", e)))?;

    let rows = stmt.query_map(params![kb_document_id], row_to_provenance_record)?;

    let mut result = Vec::new();
    for row in rows {
        result.push(
            row.map_err(|e| GBrainError::Database(format!("映射 provenance 行失败: {}", e)))?,
        );
    }
    Ok(result)
}

/// 文档变化时标记旧 provenance 为 stale
///
/// 当 KB 文档被重新处理或 gbrain page 被编辑时调用。
pub fn mark_provenance_stale(
    conn: &Connection,
    brain_slug: &str,
    brain_field: &str,
    reason: &str,
) -> Result<i64> {
    let now = now_str();
    let count = conn
        .execute(
            "UPDATE provenance_ledger
         SET status = 'stale', stale_reason = ?1, updated_at = ?2
         WHERE brain_slug = ?3 AND brain_field = ?4 AND status = 'active'",
            params![reason, now, brain_slug, brain_field],
        )
        .map_err(|e| GBrainError::Database(format!("标记 provenance stale 失败: {}", e)))?;

    if count > 0 {
        info!(
            "标记 {} 条 provenance 为 stale: slug={}, field={}, reason={}",
            count, brain_slug, brain_field, reason
        );
    }

    Ok(count as i64)
}

/// 按候选 ID 精确标记 provenance 为 stale
///
/// 修复：rollback 时按 promotion_candidate_id 精确 stale，
/// 避免误伤同一 slug+field 下其它候选来源的 provenance。
/// 之前按 brain_slug + brain_field 标记，会把该字段所有 active provenance 都标 stale。
pub fn mark_provenance_stale_by_candidate(
    conn: &Connection,
    candidate_id: i64,
    reason: &str,
) -> Result<i64> {
    let now = now_str();
    let count = conn
        .execute(
            "UPDATE provenance_ledger
         SET status = 'stale', stale_reason = ?1, updated_at = ?2
         WHERE promotion_candidate_id = ?3 AND status = 'active'",
            params![reason, now, candidate_id],
        )
        .map_err(|e| {
            GBrainError::Database(format!("按候选 ID 标记 provenance stale 失败: {}", e))
        })?;

    if count > 0 {
        info!(
            "标记 {} 条 provenance 为 stale: candidate_id={}, reason={}",
            count, candidate_id, reason
        );
    }

    Ok(count as i64)
}

/// KB 文档重新处理时，标记相关 provenance 为 stale
pub fn mark_provenance_stale_by_kb_document(
    conn: &Connection,
    kb_document_id: i64,
    reason: &str,
) -> Result<i64> {
    let now = now_str();
    let count = conn
        .execute(
            "UPDATE provenance_ledger
         SET status = 'stale', stale_reason = ?1, updated_at = ?2
         WHERE kb_document_id = ?3 AND status = 'active'",
            params![reason, now, kb_document_id],
        )
        .map_err(|e| GBrainError::Database(format!("标记 provenance stale 失败: {}", e)))?;

    if count > 0 {
        info!(
            "标记 {} 条 provenance 为 stale: kb_document_id={}, reason={}",
            count, kb_document_id, reason
        );
    }

    Ok(count as i64)
}

/// 恢复因 KB 文档删除而变 stale 的 provenance（restore 操作）
///
/// 只恢复 stale_reason='kb_document_deleted' 的 provenance，
/// 不恢复 reprocess/detach 主动标记的 stale provenance。
pub fn reactivate_provenance_by_kb_document(conn: &Connection, kb_document_id: i64) -> Result<u64> {
    let count =
        conn.execute(
            "UPDATE provenance_ledger
         SET status = 'active', stale_reason = '', updated_at = datetime('now')
         WHERE kb_document_id = ?1 AND status = 'stale' AND stale_reason = 'kb_document_deleted'",
            params![kb_document_id],
        )
        .map_err(|e| GBrainError::Database(format!("恢复 provenance 失败: {}", e)))? as u64;
    info!(
        "reactivate_provenance_by_kb_document: kb_document_id={}, count={}",
        kb_document_id, count
    );
    Ok(count)
}

/// 统计活跃 provenance 数
pub fn count_active_provenance(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM provenance_ledger WHERE status = 'active'",
        [],
        |row| row.get(0),
    )
    .map_err(|e| GBrainError::Database(format!("统计 provenance 失败: {}", e)))
}

/// 统计 stale provenance 数
pub fn count_stale_provenance(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM provenance_ledger WHERE status = 'stale'",
        [],
        |row| row.get(0),
    )
    .map_err(|e| GBrainError::Database(format!("统计 provenance 失败: {}", e)))
}

fn row_to_provenance_record(row: &Row) -> std::result::Result<ProvenanceRecord, rusqlite::Error> {
    Ok(ProvenanceRecord {
        id: row.get(0)?,
        created_at: row.get(1)?,
        updated_at: row.get(2)?,
        artifact_id: row.get(3)?,
        occurrence_id: row.get(4)?,
        kb_document_id: row.get(5)?,
        kb_node_id: row.get(6)?,
        promotion_candidate_id: row.get(7)?,
        brain_slug: row.get(8)?,
        brain_field: row.get(9)?,
        fact_hash: row.get(10)?,
        quote_text: row.get(11)?,
        quote_start: row.get(12)?,
        quote_end: row.get(13)?,
        page_number: row.get(14)?,
        confidence: row.get(15)?,
        status: row.get(16)?,
        stale_reason: row.get(17)?,
        metadata_json: row.get(18)?,
    })
}

fn now_str() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}
