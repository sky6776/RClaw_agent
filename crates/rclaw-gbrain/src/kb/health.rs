//! 索引健康检查与修复 (P5-001~P5-009)
//!
//! 检查 orphan nodes/embeddings/summaries、missing FTS rows、
//! split 一致性，支持 repair 和 rebuild。

use crate::error::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// 单个检查项结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckItem {
    pub check_name: String,
    pub status: String, // "ok", "warning", "error"
    pub detail: String,
    pub affected_count: i64,
}

/// 健康检查汇总
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSummary {
    pub overall_status: String,
    pub checks: Vec<HealthCheckItem>,
    pub issues_count: usize,
}

/// 运行全部索引健康检查
pub fn check_index_health(conn: &Connection) -> Result<HealthSummary> {
    let mut checks = Vec::new();
    let mut issues = 0usize;

    // P5-002: 检查 orphan nodes（document 已删除但 node 仍在）
    let orphan_nodes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kb_document_nodes n \
         LEFT JOIN kb_documents d ON n.document_id = d.id \
         WHERE d.id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let status = if orphan_nodes > 0 { "error" } else { "ok" };
    if orphan_nodes > 0 {
        issues += 1;
    }
    checks.push(HealthCheckItem {
        check_name: "orphan_nodes".into(),
        status: status.into(),
        detail: format!("{} orphan document nodes found", orphan_nodes),
        affected_count: orphan_nodes,
    });

    // P5-002: 检查 orphan embeddings
    let orphan_embeddings: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kb_node_embeddings e \
         LEFT JOIN kb_document_nodes n ON e.node_id = n.id \
         WHERE n.id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if orphan_embeddings > 0 {
        issues += 1;
    }
    checks.push(HealthCheckItem {
        check_name: "orphan_embeddings".into(),
        status: if orphan_embeddings > 0 { "error" } else { "ok" }.into(),
        detail: format!("{} orphan embeddings found", orphan_embeddings),
        affected_count: orphan_embeddings,
    });

    // P5-002: 检查 orphan summaries
    let orphan_summaries: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kb_document_summaries s \
         LEFT JOIN kb_documents d ON s.document_id = d.id \
         WHERE d.id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if orphan_summaries > 0 {
        issues += 1;
    }
    checks.push(HealthCheckItem {
        check_name: "orphan_summaries".into(),
        status: if orphan_summaries > 0 { "error" } else { "ok" }.into(),
        detail: format!("{} orphan summaries found", orphan_summaries),
        affected_count: orphan_summaries,
    });

    // P5-003: 检查 FTS 缺失
    let missing_fts: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kb_document_nodes n \
         WHERE n.id NOT IN (SELECT rowid FROM kb_doc_fts)",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if missing_fts > 0 {
        issues += 1;
    }
    checks.push(HealthCheckItem {
        check_name: "missing_fts".into(),
        status: if missing_fts > 0 { "error" } else { "ok" }.into(),
        detail: format!("{} nodes missing FTS entries", missing_fts),
        affected_count: missing_fts,
    });

    // P5-002: 检查 orphan table rows
    let orphan_table_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kb_table_rows r \
         LEFT JOIN kb_tables t ON r.table_id = t.id \
         WHERE t.id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if orphan_table_rows > 0 {
        issues += 1;
    }
    checks.push(HealthCheckItem {
        check_name: "orphan_table_rows".into(),
        status: if orphan_table_rows > 0 { "error" } else { "ok" }.into(),
        detail: format!("{} orphan table rows found", orphan_table_rows),
        affected_count: orphan_table_rows,
    });

    // P5-004: 检查 split_total 不一致
    // Only compare against level=0 nodes, since split_total counts original
    // splits and excludes RAPTOR parent nodes (level > 0).
    let split_mismatch: i64 = conn.query_row(
        "SELECT COUNT(*) FROM kb_documents d \
         WHERE d.split_total != (SELECT COUNT(*) FROM kb_document_nodes n WHERE n.document_id = d.id AND n.level = 0)",
        [],
        |row| row.get(0),
    ).unwrap_or(0);
    if split_mismatch > 0 {
        issues += 1;
    }
    checks.push(HealthCheckItem {
        check_name: "split_total_mismatch".into(),
        status: if split_mismatch > 0 { "warning" } else { "ok" }.into(),
        detail: format!("{} documents with split_total mismatch", split_mismatch),
        affected_count: split_mismatch,
    });

    let summary = HealthSummary {
        overall_status: if issues == 0 {
            "healthy".into()
        } else {
            "issues_found".into()
        },
        checks,
        issues_count: issues,
    };
    info!(
        "health check: status={}, issues={}, orphan_nodes={}, orphan_embeddings={}, missing_fts={}, split_mismatches={}",
        summary.overall_status,
        summary.issues_count,
        summary.checks.iter().find(|c| c.check_name == "orphan_nodes").map(|c| c.affected_count).unwrap_or(0),
        summary.checks.iter().find(|c| c.check_name == "orphan_embeddings").map(|c| c.affected_count).unwrap_or(0),
        summary.checks.iter().find(|c| c.check_name == "missing_fts").map(|c| c.affected_count).unwrap_or(0),
        summary.checks.iter().find(|c| c.check_name == "split_total_mismatch").map(|c| c.affected_count).unwrap_or(0),
    );
    Ok(summary)
}

/// P5-005: 修复缺失的 FTS 条目
pub fn repair_fts(conn: &Connection) -> Result<i64> {
    let mut repaired = 0i64;
    info!("repairing missing FTS entries");
    let mut stmt = conn.prepare(
        "SELECT n.id, n.content_tokens, n.library_id, n.document_id, n.level \
         FROM kb_document_nodes n WHERE n.id NOT IN (SELECT rowid FROM kb_doc_fts)",
    )?;
    let rows: Vec<(i64, String, i64, i64, i32)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    for (id, tokens, lib_id, doc_id, level) in &rows {
        conn.execute(
            "INSERT INTO kb_doc_fts(rowid, tokens, library_id, document_id, level) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, tokens, lib_id, doc_id, level],
        )?;
        repaired += 1;
    }
    info!("FTS repair complete: repaired={}", repaired);
    Ok(repaired)
}

/// P5-006: 修复缺失的 embedding（缺失的 vector 重新生成）
///
/// 扫描 kb_document_nodes 中存在但 kb_node_embeddings 中缺失的节点，
/// 为每个缺失节点创建一个 re-embed job（job_type = "kb_reembed_node"）。
/// 实际重新嵌入由 worker 异步处理。
///
/// Returns the count of nodes marked for re-embedding.
pub fn repair_embeddings(conn: &Connection) -> Result<i64> {
    // Find nodes that exist but have no embedding in the fallback BLOB table.
    // We check kb_node_embeddings (portable fallback) rather than vec_kb_nodes
    // (sqlite-vec) because vec_kb_nodes may not be available in all builds.
    let missing_ids: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT n.id FROM kb_document_nodes n \
             WHERE n.id NOT IN (SELECT node_id FROM kb_node_embeddings)",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let count = missing_ids.len() as i64;
    if count == 0 {
        return Ok(0);
    }
    info!(
        "repairing embeddings: {} nodes marked for re-embedding",
        count
    );

    // For each missing embedding node, insert a re-embed job into the jobs table.
    // The worker will pick these up and call the embedder to generate vectors.
    for node_id in &missing_ids {
        let payload = serde_json::json!({
            "node_id": node_id,
        });
        let payload_str = payload.to_string();
        conn.execute(
            "INSERT INTO jobs (job_type, payload, status, priority, max_attempts) \
             VALUES ('kb_reembed_node', ?1, 'pending', 0, 3)",
            rusqlite::params![payload_str],
        )?;
    }

    Ok(count)
}

/// P5-007: 重建单文档索引
///
/// Marks the document for re-processing by changing index_status = 'rebuilding'
/// and document_status = 'queued', assigns a new processing_run_id, and enqueues
/// a kb_process_document job. The actual pipeline execution happens asynchronously
/// via the worker.
///
/// The old index data (nodes, FTS, embeddings, summaries, table_rows) is NOT
/// deleted upfront. The pipeline's persist_nodes_and_vectors function handles
/// deletion of old data inside a transaction, so if rebuild fails, the old
/// index remains intact.
pub fn rebuild_document_index(conn: &Connection, document_id: i64) -> Result<()> {
    // Verify the document exists and is not soft-deleted
    let (library_id, storage_path, extension, deleted_at): (i64, String, String, Option<String>) =
        conn.query_row(
            "SELECT library_id, storage_path, extension, deleted_at \
             FROM kb_documents WHERE id = ?1",
            rusqlite::params![document_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|e| {
            crate::error::GBrainError::Database(format!("document not found for rebuild: {}", e))
        })?;

    if deleted_at.is_some() {
        return Err(crate::error::GBrainError::InvalidInput(
            "cannot rebuild index for a soft-deleted document".to_string(),
        ));
    }

    // Assign a new processing_run_id so stale jobs are rejected
    let run_id = crate::kb::jobs::new_run_id();

    // Mark document for re-processing: index_status = 'rebuilding', document_status = 'queued'
    conn.execute(
        "UPDATE kb_documents SET \
         document_status = 'queued', \
         index_status = 'rebuilding', \
         processing_run_id = ?1, \
         parsing_status = 0, \
         parsing_progress = 0, \
         parsing_error = '', \
         embedding_status = 0, \
         embedding_progress = 0, \
         embedding_error = '', \
         ocr_status = 'not_evaluated', \
         ocr_text_coverage = 0.0, \
         updated_at = datetime('now') \
         WHERE id = ?2",
        rusqlite::params![run_id, document_id],
    )?;

    // Enqueue a kb_process_document job for the worker to pick up
    let payload = crate::kb::jobs::KbProcessPayload {
        kind: "kb_process_document".to_string(),
        document_id,
        library_id,
        processing_run_id: run_id,
        storage_path,
        extension,
    };
    crate::kb::jobs::enqueue_kb_process_job(conn, &payload)?;

    Ok(())
}

/// P5-008: 重建库级索引
///
/// Iterate all non-deleted documents in the library and call
/// rebuild_document_index for each. Track completed/failed counts
/// and return a summary.
pub fn rebuild_library_index(conn: &Connection, library_id: i64) -> Result<RebuildLibrarySummary> {
    // Collect all non-deleted document IDs in the library
    let doc_ids: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM kb_documents \
             WHERE library_id = ?1 AND deleted_at IS NULL",
        )?;
        let rows = stmt.query_map(rusqlite::params![library_id], |row| row.get::<_, i64>(0))?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let mut completed = 0i64;
    let mut failed = 0i64;
    let mut errors: Vec<String> = Vec::new();

    for doc_id in &doc_ids {
        match rebuild_document_index(conn, *doc_id) {
            Ok(_) => completed += 1,
            Err(e) => {
                failed += 1;
                let msg = format!("document_id={}: {}", doc_id, e);
                errors.push(msg);
            }
        }
    }

    Ok(RebuildLibrarySummary {
        library_id,
        total: doc_ids.len() as i64,
        completed,
        failed,
        errors,
    })
}

/// Summary result for rebuild_library_index
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebuildLibrarySummary {
    pub library_id: i64,
    pub total: i64,
    pub completed: i64,
    pub failed: i64,
    pub errors: Vec<String>,
}

/// P5-009: 清理已软删除且超过保留期的文档
pub fn purge_deleted(conn: &Connection, older_than_days: i32) -> Result<i64> {
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM kb_documents WHERE deleted_at IS NOT NULL \
             AND deleted_at < datetime('now', ?1)",
        )?;
        let rows = stmt.query_map(params![format!("-{} days", older_than_days)], |row| {
            row.get::<_, i64>(0)
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let count = ids.len() as i64;
    warn!(
        "purging {} deleted documents older than {} days",
        count, older_than_days
    );
    let kb = crate::kb::engine::KbEngine::new(conn);
    for id in &ids {
        crate::kb::lifecycle::purge_document(&kb, *id)?;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_summary_empty() {
        let summary = HealthSummary {
            overall_status: "healthy".into(),
            checks: vec![],
            issues_count: 0,
        };
        assert_eq!(summary.overall_status, "healthy");
        assert_eq!(summary.issues_count, 0);
    }
}
