//! KB 文档生命周期管理
//!
//! 状态机、版本守卫、软删除、purge。

use crate::error::{GBrainError, Result};
use crate::kb::engine::KbEngine;
use rusqlite::{params, Connection};

/// 文档处理状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentStatus {
    Queued,
    Processing,
    Ready,
    Failed,
    Deleted,
}

impl DocumentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Processing => "processing",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Deleted => "deleted",
        }
    }

    /// 验证状态转换是否合法
    pub fn can_transition_to(&self, target: Self) -> bool {
        matches!(
            (self, target),
            (Self::Queued, Self::Processing)
                | (Self::Processing, Self::Ready)
                | (Self::Processing, Self::Failed)
                | (Self::Failed, Self::Queued) // 重试
                | (Self::Ready, Self::Queued) // 重新处理
                | (_, Self::Deleted) // 任何状态都可删除
        )
    }
}

/// 转换文档状态，带合法性检查。
pub fn transition_document_status(
    conn: &Connection,
    document_id: i64,
    new_status: DocumentStatus,
    run_id: Option<&str>,
    error_message: Option<&str>,
) -> Result<()> {
    // 读取当前状态
    let current_status: String = conn
        .query_row(
            "SELECT document_status FROM kb_documents WHERE id = ?1",
            params![document_id],
            |row| row.get(0),
        )
        .map_err(|e| GBrainError::Database(format!("document not found: {}", e)))?;

    let current = match current_status.as_str() {
        "queued" => DocumentStatus::Queued,
        "processing" => DocumentStatus::Processing,
        "ready" => DocumentStatus::Ready,
        "failed" => DocumentStatus::Failed,
        "deleted" => DocumentStatus::Deleted,
        _ => DocumentStatus::Queued,
    };

    if !current.can_transition_to(new_status) {
        return Err(GBrainError::InvalidInput(format!(
            "invalid status transition: {} -> {}",
            current.as_str(),
            new_status.as_str()
        )));
    }

    // 如果提供了 run_id，执行版本守卫检查
    if let Some(rid) = run_id {
        let stored_run_id: String = conn.query_row(
            "SELECT processing_run_id FROM kb_documents WHERE id = ?1",
            params![document_id],
            |row| row.get(0),
        )?;
        if !stored_run_id.is_empty() && stored_run_id != rid {
            return Err(GBrainError::InvalidInput(format!(
                "stale run: current run_id={} does not match provided run_id={}",
                stored_run_id, rid
            )));
        }
    }

    let error = error_message.unwrap_or("");
    // 仅更新 document_status，不修改 index_status（索引状态由索引流程独立管理）
    conn.execute(
        "UPDATE kb_documents SET document_status = ?1, \
         parsing_error = CASE WHEN ?2 != '' THEN ?2 ELSE parsing_error END, \
         updated_at = datetime('now') WHERE id = ?3",
        params![new_status.as_str(), error, document_id],
    )?;

    Ok(())
}

/// 将文档标记为软删除。默认搜索结果过滤软删除文档。
///
/// 修复 P2：软删除时旋转 processing_run_id，使旧 pending job 的 run_id
/// 与新 run_id 不匹配，被 worker 的 ensure_document_run_current 校验拒绝，
/// 避免已删除文档被旧 job 继续处理。
pub fn soft_delete_document(conn: &Connection, document_id: i64) -> Result<()> {
    let new_run_id = crate::kb::jobs::new_run_id();
    conn.execute(
        "UPDATE kb_documents SET deleted_at = datetime('now'), \
         document_status = 'deleted', processing_run_id = ?1, \
         updated_at = datetime('now') WHERE id = ?2",
        params![new_run_id, document_id],
    )?;
    Ok(())
}

/// 恢复已软删除的 KB 文档（restore 操作）
///
/// 清除 deleted_at，恢复 document_status 为 queued，
/// 同时生成新 processing_run_id（让旧 pending job 自动 stale）、
/// 重置索引/解析/嵌入状态、入队新 KB job 并更新 job_id。
/// 确保 KB 查询不再被 `d.deleted_at IS NULL` / `document_status != 'deleted'` 过滤掉，
/// 且文档恢复后能真正被 worker 重新处理。
pub fn restore_document(conn: &Connection, document_id: i64) -> Result<()> {
    // 读取 kb_documents 信息用于入队
    let (library_id, storage_path, extension): (i64, String, String) = conn
        .query_row(
            "SELECT library_id, storage_path, extension FROM kb_documents WHERE id = ?1",
            params![document_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| GBrainError::Database(format!("读取 KB 文档信息失败: {}", e)))?;

    // 生成新 processing_run_id，旧 pending job 的 run_id 与此不匹配会自动 stale
    let run_id = crate::kb::jobs::new_run_id();

    // 清 deleted_at，恢复 document_status，重置索引/解析/嵌入状态
    conn.execute(
        "UPDATE kb_documents SET deleted_at = NULL, \
         document_status = 'queued', index_status = 'rebuilding', \
         processing_run_id = ?1, \
         parsing_status = 0, parsing_progress = 0, parsing_error = '', \
         embedding_status = 0, embedding_progress = 0, embedding_error = '', \
         ocr_status = 'not_evaluated', ocr_text_coverage = 0.0, \
         updated_at = datetime('now') WHERE id = ?2",
        params![run_id, document_id],
    )?;

    // 入队新 KB job
    let payload = crate::kb::jobs::KbProcessPayload {
        kind: "kb_process_document".to_string(),
        document_id,
        library_id,
        processing_run_id: run_id,
        storage_path,
        extension,
    };
    crate::kb::jobs::enqueue_kb_process_job(conn, &payload)?;

    // 更新 job_id
    let job_id = conn.last_insert_rowid();
    conn.execute(
        "UPDATE kb_documents SET job_id = ?1 WHERE id = ?2",
        params![job_id.to_string(), document_id],
    )?;

    Ok(())
}

/// 彻底清理文档及其所有关联数据（purge）。
///
/// FIX9-08: 清理完所有关联数据后，直接删除 kb_documents 行，
/// 而非仅设置 purged_at，避免已 purge 行继续占用唯一哈希索引。
///
/// FIX9-09: 仅对 source_type == "upload" 且 storage_path 位于 KB 存储根目录内的文件
/// 才执行物理删除；ingest/source_sync 类型的 storage_path 是用户原始文件，不应删除。
pub fn purge_document(kb: &KbEngine, document_id: i64) -> Result<()> {
    kb.transaction(|conn| {
        // 验证文档存在且已软删除
        let deleted_at: Option<String> = conn
            .query_row(
                "SELECT deleted_at FROM kb_documents WHERE id = ?1",
                params![document_id],
                |row| row.get(0),
            )
            .map_err(|_| {
                GBrainError::InvalidInput("document not found or not deleted".to_string())
            })?;

        if deleted_at.is_none() {
            return Err(GBrainError::InvalidInput(
                "document must be soft-deleted before purge".to_string(),
            ));
        }

        // 读取 source_type 和 storage_path，用于后续判断是否删除物理文件
        let (source_type, storage_path): (String, Option<String>) = conn
            .query_row(
                "SELECT source_type, storage_path FROM kb_documents WHERE id = ?1",
                params![document_id],
                |row| {
                    let st: String = row.get(0)?;
                    let sp: String = row.get(1)?;
                    Ok((st, if sp.is_empty() { None } else { Some(sp) }))
                },
            )
            .map_err(|e| GBrainError::Database(format!("读取文档信息失败: {}", e)))?;

        // 清理表格行
        conn.execute(
            "DELETE FROM kb_table_rows WHERE table_id IN \
             (SELECT id FROM kb_tables WHERE document_id = ?1)",
            params![document_id],
        )?;
        conn.execute(
            "DELETE FROM kb_tables WHERE document_id = ?1",
            params![document_id],
        )?;

        // 清理摘要
        conn.execute(
            "DELETE FROM kb_document_summaries WHERE document_id = ?1",
            params![document_id],
        )?;

        // 清理 source_items 引用
        conn.execute(
            "UPDATE kb_source_items SET document_id = NULL WHERE document_id = ?1",
            params![document_id],
        )?;

        // 清理搜索反馈
        conn.execute(
            "DELETE FROM kb_search_feedback WHERE document_id = ?1",
            params![document_id],
        )?;

        // 清理 FTS 条目（通过删除 nodes 触发触发器）和 embedding 数据
        let node_ids: Vec<i64> = {
            let mut stmt =
                conn.prepare("SELECT id FROM kb_document_nodes WHERE document_id = ?1")?;
            let rows = stmt.query_map(params![document_id], |row| row.get(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };

        // FIX12-04: 复用 engine 的 cleanup_node_vectors，统一清理 vec_kb_nodes、
        // per-index vec_kb_{id} 虚表及 kb_node_embeddings，避免 purge 后向量行残留。
        for &node_id in &node_ids {
            crate::kb::engine::cleanup_node_vectors(conn, node_id);
        }
        conn.execute(
            "DELETE FROM kb_document_nodes WHERE document_id = ?1",
            params![document_id],
        )?;

        // 清理版本记录
        conn.execute(
            "DELETE FROM kb_document_versions WHERE document_id = ?1",
            params![document_id],
        )?;

        // FIX9-08: 删除 kb_documents 行，而非仅设置 purged_at。
        // 这样已 purge 的行不会继续占用唯一哈希索引，不会阻止新上传。
        conn.execute(
            "DELETE FROM kb_documents WHERE id = ?1",
            params![document_id],
        )?;

        // FIX9-09: 仅对 upload 类型且路径位于 KB 存储根目录内的文件执行物理删除。
        // ingest/source_sync 类型的 storage_path 是用户原始文件，不应删除。
        if let Some(path) = storage_path {
            if source_type == "upload" {
                // 计算所有可能的 KB 存储根目录，用于验证路径安全性
                let default_base = crate::config::Config::base_dir().join("kb_files");
                let mut kb_roots = vec![default_base.join("kb").join("files")];
                // 如果配置了自定义 kb_storage_dir，也加入检查范围
                if let Ok(custom_dir) = std::env::var("GBRAIN_KB_STORAGE_DIR") {
                    kb_roots.push(
                        std::path::PathBuf::from(custom_dir)
                            .join("kb")
                            .join("files"),
                    );
                }
                let canonical = std::fs::canonicalize(&path).ok();
                let safe_to_delete = kb_roots.iter().any(|root| {
                    if let Some(ref c) = canonical {
                        if let Ok(root_c) = std::fs::canonicalize(root) {
                            return c.starts_with(&root_c);
                        }
                    }
                    // 无法规范化路径时，回退到字符串前缀匹配
                    path.starts_with(root.to_string_lossy().as_ref())
                });
                if safe_to_delete {
                    if let Err(e) = std::fs::remove_file(&path) {
                        tracing::warn!("purge 删除存储文件失败 {}: {}", path, e);
                    }
                }
            }
            // ingest/source_sync 类型仅清除数据库引用（已通过 DELETE 行完成），不删物理文件
        }

        Ok(())
    })
}

/// 创建文档版本快照（在更新前调用）。
pub fn create_document_version(
    conn: &Connection,
    document_id: i64,
    version_label: &str,
    processing_run_id: &str,
    char_count: i32,
    node_count: i32,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO kb_document_versions \
         (document_id, version_label, processing_run_id, char_count, node_count, index_status) \
         VALUES (?1, ?2, ?3, ?4, ?5, 'archived')",
        params![
            document_id,
            version_label,
            processing_run_id,
            char_count,
            node_count
        ],
    )?;
    let version_id = conn.last_insert_rowid();

    // 更新 current_version_id
    conn.execute(
        "UPDATE kb_documents SET current_version_id = ?1, updated_at = datetime('now') \
         WHERE id = ?2",
        params![version_id, document_id],
    )?;

    Ok(version_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_transitions_valid() {
        assert!(DocumentStatus::Queued.can_transition_to(DocumentStatus::Processing));
        assert!(DocumentStatus::Processing.can_transition_to(DocumentStatus::Ready));
        assert!(DocumentStatus::Processing.can_transition_to(DocumentStatus::Failed));
        assert!(DocumentStatus::Failed.can_transition_to(DocumentStatus::Queued));
        assert!(DocumentStatus::Ready.can_transition_to(DocumentStatus::Deleted));
    }

    #[test]
    fn test_status_transitions_invalid() {
        assert!(!DocumentStatus::Ready.can_transition_to(DocumentStatus::Processing));
        assert!(!DocumentStatus::Ready.can_transition_to(DocumentStatus::Failed));
        assert!(!DocumentStatus::Queued.can_transition_to(DocumentStatus::Ready));
        assert!(!DocumentStatus::Deleted.can_transition_to(DocumentStatus::Queued));
    }

    #[test]
    fn test_status_as_str() {
        assert_eq!(DocumentStatus::Queued.as_str(), "queued");
        assert_eq!(DocumentStatus::Ready.as_str(), "ready");
        assert_eq!(DocumentStatus::Failed.as_str(), "failed");
    }
}
