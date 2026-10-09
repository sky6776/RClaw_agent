//! 投影管理 — KB 投影、影子页面投影、文件存储投影的创建和管理
//!
//! 负责投影的创建、更新、过期标记、一致性检查等。

use rusqlite::{params, Connection};
use tracing::{debug, info};

use crate::error::{GBrainError, Result};
use crate::kb::jobs::{new_run_id, KbProcessPayload};

use super::store;
use super::types::*;

/// 复用已有 kb_document 并重新入队 KB job
///
/// 当 active 投影的 occurrence_id 不同时调用此函数：
/// 复用旧 kb_document（同 hash = 同内容），只创建新 projection 行和 KB job，
/// 避免撞 kb_documents 唯一索引 (library_id, content_hash)。
fn reuse_kb_doc_and_enqueue(
    conn: &Connection,
    artifact_id: i64,
    occurrence_id: i64,
    library_id: i64,
    kb_doc_id: i64,
    proj_key: &str,
) -> Result<String> {
    let run_id = new_run_id();

    // 修复 P2：入队新 job 前先取消该文档的旧 pending job，
    // 防止旧 job 被认领后因 run_id 不匹配被判定为 stale。
    // 不取消 processing 状态的 job（worker 正在执行，无法中断）。
    if let Err(e) = crate::kb::jobs::cancel_pending_kb_jobs_by_document_id(conn, kb_doc_id) {
        tracing::warn!(
            kb_doc_id,
            error = %e,
            "active 投影复用路径取消旧 KB job 失败，继续"
        );
    }

    // 修复 P1：复用已删除的 kb_document 时必须同时清 deleted_at，
    // 否则后续查询用 d.deleted_at IS NULL 过滤时搜不到。
    conn.execute(
        "UPDATE kb_documents SET processing_run_id = ?1, document_status = 'queued', \
         ocr_status = 'not_evaluated', ocr_text_coverage = 0.0, \
         deleted_at = NULL, updated_at = datetime('now') WHERE id = ?2",
        params![run_id, kb_doc_id],
    )
    .map_err(|e| GBrainError::Database(format!("更新 KB document run_id 失败: {}", e)))?;

    let artifact = store::find_artifact_by_id(conn, artifact_id)
        .map_err(|e| GBrainError::Database(format!("查找 artifact 失败: {}", e)))?
        .ok_or_else(|| GBrainError::PageNotFound(format!("artifact {} 不存在", artifact_id)))?;

    let payload = KbProcessPayload {
        kind: "kb_process_document".to_string(),
        document_id: kb_doc_id,
        library_id,
        processing_run_id: run_id,
        storage_path: artifact.storage_path.clone(),
        extension: artifact.extension.clone(),
    };
    let payload_json = serde_json::to_string(&payload)
        .map_err(|e| GBrainError::Serialization(format!("序列化 KB job payload 失败: {}", e)))?;
    conn.execute(
        "INSERT INTO jobs (job_type, payload, status, priority, created_at)
         VALUES ('kb_process_document', ?1, 'pending', 0, datetime('now'))",
        params![payload_json],
    )
    .map_err(|e| GBrainError::Database(format!("重新入队 KB job 失败: {}", e)))?;
    let job_id = conn.last_insert_rowid();

    conn.execute(
        "UPDATE kb_documents SET job_id = ?1 WHERE id = ?2",
        params![job_id.to_string(), kb_doc_id],
    )
    .map_err(|e| GBrainError::Database(format!("更新 KB document job_id 失败: {}", e)))?;

    let proj_ref = format!("kb_document:{}", kb_doc_id);
    let now = now_str();
    let proj = ArtifactProjection {
        id: 0,
        created_at: now.clone(),
        updated_at: now.clone(),
        artifact_id,
        occurrence_id: Some(occurrence_id),
        projection_type: ProjectionType::KbDocument.to_string(),
        projection_key: proj_key.to_string(),
        projection_ref: proj_ref.clone(),
        status: "active".to_string(),
        version_hash: String::new(),
        stale_reason: String::new(),
        metadata_json: format!(
            "{{\"kb_document_id\": {}, \"job_id\": {}}}",
            kb_doc_id, job_id
        ),
        superseded_by: None,
    };

    let _proj_id = store::insert_projection(conn, &proj)
        .map_err(|e| GBrainError::Database(format!("插入 KB 投影失败: {}", e)))?;

    info!(
        "复用 kb_document={}, 创建新投影，重新入队 KB job: job_id={}",
        kb_doc_id, job_id
    );

    Ok(proj_ref)
}

#[derive(Debug)]
struct SourceKbProjectionMatch {
    artifact_id: i64,
    occurrence_id: i64,
    kb_doc_id: i64,
}

fn parse_kb_document_ref(projection_ref: &str) -> Option<i64> {
    projection_ref
        .strip_prefix("kb_document:")
        .and_then(|s| s.parse::<i64>().ok())
}

fn find_active_source_kb_projections(
    conn: &Connection,
    source_kind: &str,
    source_uri: &str,
    current_artifact_id: i64,
    current_occurrence_id: i64,
    proj_key: &str,
) -> Result<Vec<SourceKbProjectionMatch>> {
    if source_kind.trim().is_empty() || source_uri.trim().is_empty() {
        return Ok(Vec::new());
    }

    let mut stmt = conn
        .prepare(
            "SELECT o.id, o.artifact_id, p.projection_ref
             FROM artifact_occurrences o
             JOIN artifact_projections p ON p.occurrence_id = o.id
             JOIN source_artifacts a ON a.id = o.artifact_id
             WHERE o.source_kind = ?1
               AND o.source_uri = ?2
               AND o.status = 'active'
               AND o.id <> ?3
               AND o.artifact_id <> ?4
               AND p.projection_type = 'kb_document'
               AND p.projection_key = ?5
               AND p.status = 'active'
               AND p.projection_ref LIKE 'kb_document:%'
               AND a.status = 'active'
               AND a.purged_at IS NULL
             ORDER BY o.updated_at DESC, o.id DESC",
        )
        .map_err(|e| {
            GBrainError::Database(format!("prepare source KB projection lookup failed: {}", e))
        })?;

    let rows = stmt
        .query_map(
            params![
                source_kind,
                source_uri,
                current_occurrence_id,
                current_artifact_id,
                proj_key
            ],
            |row| {
                let projection_ref: String = row.get(2)?;
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, projection_ref))
            },
        )
        .map_err(|e| GBrainError::Database(format!("query source KB projection failed: {}", e)))?;

    let mut matches = Vec::new();
    for row in rows {
        let (occurrence_id, artifact_id, projection_ref) = row.map_err(|e| {
            GBrainError::Database(format!("map source KB projection failed: {}", e))
        })?;
        if let Some(kb_doc_id) = parse_kb_document_ref(&projection_ref) {
            matches.push(SourceKbProjectionMatch {
                artifact_id,
                occurrence_id,
                kb_doc_id,
            });
        }
    }

    Ok(matches)
}

fn enqueue_kb_process_job_for_artifact(
    conn: &Connection,
    artifact: &SourceArtifact,
    kb_doc_id: i64,
    library_id: i64,
    run_id: &str,
) -> Result<i64> {
    let payload = KbProcessPayload {
        kind: "kb_process_document".to_string(),
        document_id: kb_doc_id,
        library_id,
        processing_run_id: run_id.to_string(),
        storage_path: artifact.storage_path.clone(),
        extension: artifact.extension.clone(),
    };
    let payload_json = serde_json::to_string(&payload).map_err(|e| {
        GBrainError::Serialization(format!("serialize KB job payload failed: {}", e))
    })?;
    conn.execute(
        "INSERT INTO jobs (job_type, payload, status, priority, created_at)
         VALUES ('kb_process_document', ?1, 'pending', 0, datetime('now'))",
        params![payload_json],
    )
    .map_err(|e| GBrainError::Database(format!("enqueue KB job failed: {}", e)))?;
    Ok(conn.last_insert_rowid())
}

fn find_active_kb_document_by_hash(
    conn: &Connection,
    library_id: i64,
    content_hash: &str,
) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM kb_documents
         WHERE library_id = ?1 AND content_hash = ?2
           AND deleted_at IS NULL AND purged_at IS NULL
         ORDER BY id DESC LIMIT 1",
        params![library_id, content_hash],
        |row| row.get(0),
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        _ => Err(GBrainError::Database(format!(
            "lookup KB document by content hash failed: {}",
            e
        ))),
    })
}

fn mark_source_matches_stale(
    conn: &Connection,
    previous_matches: &[SourceKbProjectionMatch],
) -> Result<()> {
    for matched in previous_matches {
        store::stale_occurrence(conn, matched.occurrence_id, "content_updated").map_err(|e| {
            GBrainError::Database(format!("mark old occurrence stale failed: {}", e))
        })?;
        mark_projections_stale_by_occurrence(conn, matched.occurrence_id, "content_updated")?;
    }
    Ok(())
}

fn source_match_document_ids(previous_matches: &[SourceKbProjectionMatch]) -> Vec<i64> {
    let mut doc_ids = Vec::new();
    for matched in previous_matches {
        if !doc_ids.contains(&matched.kb_doc_id) {
            doc_ids.push(matched.kb_doc_id);
        }
    }
    doc_ids
}

fn mark_source_match_document_provenance_stale(
    conn: &Connection,
    previous_matches: &[SourceKbProjectionMatch],
) -> Result<()> {
    for doc_id in source_match_document_ids(previous_matches) {
        crate::artifact::provenance::mark_provenance_stale_by_kb_document(
            conn,
            doc_id,
            "content_updated",
        )?;
    }
    Ok(())
}

fn soft_delete_source_match_documents_except(
    conn: &Connection,
    previous_matches: &[SourceKbProjectionMatch],
    keep_doc_id: Option<i64>,
) -> Result<()> {
    for doc_id in source_match_document_ids(previous_matches) {
        if Some(doc_id) == keep_doc_id {
            continue;
        }
        if let Err(e) = crate::kb::jobs::cancel_pending_kb_jobs_by_document_id(conn, doc_id) {
            tracing::warn!(
                kb_doc_id = doc_id,
                error = %e,
                "source update failed to cancel old KB jobs before retiring stale source doc; continuing"
            );
        }
        crate::kb::lifecycle::soft_delete_document(conn, doc_id)?;
    }
    Ok(())
}

fn record_source_content_updated_event(
    conn: &Connection,
    artifact: &SourceArtifact,
    occurrence: &ArtifactOccurrence,
    kb_doc_id: i64,
    previous: &SourceKbProjectionMatch,
    previous_matches: &[SourceKbProjectionMatch],
) {
    let _ = store::record_event(
        conn,
        Some(artifact.id),
        Some(occurrence.id),
        "source_content_updated",
        "create_kb_projection",
        &serde_json::json!({
            "kb_document_id": kb_doc_id,
            "previous_artifact_id": previous.artifact_id,
            "previous_occurrence_id": previous.occurrence_id,
            "previous_kb_document_ids": source_match_document_ids(previous_matches),
            "sha256": artifact.sha256
        })
        .to_string(),
    );
}

fn update_kb_doc_for_source_update(
    conn: &Connection,
    artifact: &SourceArtifact,
    occurrence: &ArtifactOccurrence,
    library_id: i64,
    previous_matches: &[SourceKbProjectionMatch],
    proj_key: &str,
) -> Result<String> {
    let previous = previous_matches.first().ok_or_else(|| {
        GBrainError::InvalidInput("missing previous source KB projection".to_string())
    })?;
    let kb_doc_id = previous.kb_doc_id;
    let run_id = new_run_id();

    if let Some(existing_doc_id) =
        find_active_kb_document_by_hash(conn, library_id, &artifact.sha256)?
    {
        if existing_doc_id != kb_doc_id {
            mark_source_match_document_provenance_stale(conn, previous_matches)?;
            mark_source_matches_stale(conn, previous_matches)?;
            soft_delete_source_match_documents_except(
                conn,
                previous_matches,
                Some(existing_doc_id),
            )?;
            let proj_ref = reuse_kb_doc_and_enqueue(
                conn,
                artifact.id,
                occurrence.id,
                library_id,
                existing_doc_id,
                proj_key,
            )?;
            record_source_content_updated_event(
                conn,
                artifact,
                occurrence,
                existing_doc_id,
                previous,
                previous_matches,
            );
            return Ok(proj_ref);
        }
    }

    if let Err(e) = crate::kb::jobs::cancel_pending_kb_jobs_by_document_id(conn, kb_doc_id) {
        tracing::warn!(
            kb_doc_id,
            error = %e,
            "source update failed to cancel old pending KB jobs; continuing"
        );
    }

    mark_source_match_document_provenance_stale(conn, previous_matches)?;

    mark_source_matches_stale(conn, previous_matches)?;
    soft_delete_source_match_documents_except(conn, previous_matches, Some(kb_doc_id))?;

    let updated = conn
        .execute(
            "UPDATE kb_documents SET
                folder_id = ?1,
                original_name = ?2,
                name_tokens = '',
                file_size = ?3,
                content_hash = ?4,
                extension = ?5,
                mime_type = ?6,
                source_type = 'artifact',
                storage_path = ?7,
                original_path = ?8,
                source_uri = ?9,
                processing_run_id = ?10,
                document_status = 'queued',
                index_status = 'pending',
                parsing_status = 0,
                parsing_progress = 0,
                parsing_error = '',
                embedding_status = 0,
                embedding_progress = 0,
                embedding_error = '',
                word_total = 0,
                split_total = 0,
                normalized_content_hash = '',
                simhash = '',
                last_indexed_at = NULL,
                last_seen_at = datetime('now'),
                ocr_status = 'not_evaluated',
                ocr_text_coverage = 0.0,
                deleted_at = NULL,
                purged_at = NULL,
                updated_at = datetime('now')
             WHERE id = ?11 AND library_id = ?12",
            params![
                occurrence.folder_id,
                &artifact.original_name,
                artifact.size_bytes,
                &artifact.sha256,
                &artifact.extension,
                &artifact.mime_type,
                &artifact.storage_path,
                &occurrence.original_path,
                &occurrence.source_uri,
                &run_id,
                kb_doc_id,
                library_id
            ],
        )
        .map_err(|e| {
            GBrainError::Database(format!(
                "update KB document for source update failed: {}",
                e
            ))
        })?;
    if updated == 0 {
        return Err(GBrainError::PageNotFound(format!(
            "kb_document {} not found for source update",
            kb_doc_id
        )));
    }

    let job_id =
        enqueue_kb_process_job_for_artifact(conn, artifact, kb_doc_id, library_id, &run_id)?;
    conn.execute(
        "UPDATE kb_documents SET job_id = ?1 WHERE id = ?2",
        params![job_id.to_string(), kb_doc_id],
    )
    .map_err(|e| GBrainError::Database(format!("update KB document job_id failed: {}", e)))?;

    let proj_ref = format!("kb_document:{}", kb_doc_id);
    let now = now_str();
    let proj = ArtifactProjection {
        id: 0,
        created_at: now.clone(),
        updated_at: now.clone(),
        artifact_id: artifact.id,
        occurrence_id: Some(occurrence.id),
        projection_type: ProjectionType::KbDocument.to_string(),
        projection_key: proj_key.to_string(),
        projection_ref: proj_ref.clone(),
        status: "active".to_string(),
        version_hash: String::new(),
        stale_reason: String::new(),
        metadata_json: serde_json::json!({
            "kb_document_id": kb_doc_id,
            "job_id": job_id,
            "source_update_of_artifact_id": previous.artifact_id,
            "source_update_of_occurrence_id": previous.occurrence_id,
        })
        .to_string(),
        superseded_by: None,
    };

    store::insert_projection(conn, &proj).map_err(|e| {
        GBrainError::Database(format!(
            "insert KB projection for source update failed: {}",
            e
        ))
    })?;

    record_source_content_updated_event(
        conn,
        artifact,
        occurrence,
        kb_doc_id,
        previous,
        previous_matches,
    );

    info!(
        "updated source KB document: source_uri={}, kb_doc_id={}, old_artifact_id={}, new_artifact_id={}, job_id={}",
        occurrence.source_uri, kb_doc_id, previous.artifact_id, artifact.id, job_id
    );

    Ok(proj_ref)
}

/// 创建 KB 投影
///
/// 在 artifact_projections 表中记录 artifact -> kb_document 的映射，
/// 同时创建 kb_documents 记录（source_type='artifact'）并入队 KB 处理 job。
/// 返回 projection_ref（格式: kb_document:{id}）。
///
/// 修复：reprocess 先标 stale 后调此函数，旧投影 stale 后 find_projection_by_key
/// 只查 active 找不到，走新建 kb_documents 撞唯一索引。现在先查 active，
/// 再查不限状态（含 stale），复用 stale 投影的 kb_document 并重新激活。
pub fn create_kb_projection(
    conn: &Connection,
    artifact_id: i64,
    occurrence_id: i64,
    library_id: i64,
) -> Result<String> {
    let proj_key = format!("library:{}", library_id);

    // 查找 artifact 信息用于创建/复用 kb_document。
    let artifact = store::find_artifact_by_id(conn, artifact_id)
        .map_err(|e| GBrainError::Database(format!("查找 artifact 失败: {}", e)))?
        .ok_or_else(|| GBrainError::PageNotFound(format!("artifact {} 不存在", artifact_id)))?;

    // 查找 occurrence 信息。source_uri 更新必须优先于同 artifact 复用判断，
    // 否则更新到已存在内容时会绕过旧 source 的失效流程。
    let occurrence = store::find_occurrence_by_id(conn, occurrence_id)
        .map_err(|e| GBrainError::Database(format!("查找 occurrence 失败: {}", e)))?
        .ok_or_else(|| GBrainError::PageNotFound(format!("occurrence {} 不存在", occurrence_id)))?;

    let previous_source_matches = find_active_source_kb_projections(
        conn,
        &occurrence.source_kind,
        &occurrence.source_uri,
        artifact_id,
        occurrence_id,
        &proj_key,
    )?;
    if !previous_source_matches.is_empty() {
        return update_kb_doc_for_source_update(
            conn,
            &artifact,
            &occurrence,
            library_id,
            &previous_source_matches,
            &proj_key,
        );
    }

    // 1. 先查 active 投影（快速路径）
    let existing_active =
        store::find_projection_by_key(conn, artifact_id, "kb_document", &proj_key)
            .map_err(|e| GBrainError::Database(format!("查找 KB 投影失败: {}", e)))?;

    if let Some(existing) = existing_active {
        if existing.occurrence_id == Some(occurrence_id) {
            debug!(
                "KB 投影已存在且 occurrence_id 未变，跳过创建: artifact_id={}, key={}",
                artifact_id, proj_key
            );
            return Ok(existing.projection_ref.clone());
        }
        // occurrence_id 不同时，复用旧 kb_document（同 hash = 同内容），
        // 只创建新 projection 行和 KB job，避免撞 kb_documents 唯一索引
        if let Some(kb_doc_id) = existing
            .projection_ref
            .strip_prefix("kb_document:")
            .and_then(|s| s.parse::<i64>().ok())
        {
            return reuse_kb_doc_and_enqueue(
                conn,
                artifact_id,
                occurrence_id,
                library_id,
                kb_doc_id,
                &proj_key,
            );
        }
    }

    // 2. 查 stale 状态的投影（修复 reprocess 场景：旧投影已 stale）
    // 复用 stale 投影的 kb_document，避免撞 kb_documents 唯一索引
    // 修复 P3：限制只查 status='stale'，不查 superseded/orphaned，
    // 防止把历史行重新激活。superseded/orphaned 的投影不应被复活。
    let existing_stale =
        store::find_projection_by_key_any_status(conn, artifact_id, "kb_document", &proj_key)
            .map_err(|e| GBrainError::Database(format!("查找 KB 投影(不限状态)失败: {}", e)))?
            .filter(|p| p.status == "stale");

    if let Some(stale_proj) = existing_stale {
        if let Some(doc_id) = stale_proj
            .projection_ref
            .strip_prefix("kb_document:")
            .and_then(|s| s.parse::<i64>().ok())
        {
            // 修复 P3：先确认 kb_documents 是否存在，再决定是否激活旧 projection。
            // 如果 KB doc 已被 purge 或直接删除，激活旧 projection 会产生
            // 指向不存在 kb_document:{id} 的 active projection。
            // 此时不应激活旧 projection，改为走新建 kb_documents 路径。
            let doc_exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM kb_documents WHERE id = ?1",
                    rusqlite::params![doc_id],
                    |row| row.get(0),
                )
                .unwrap_or(false);

            if doc_exists {
                // KB doc 存在：重新激活 stale 投影，更新 occurrence_id，清空 superseded_by
                // 修复 P3：复活时同步清 superseded_by=NULL，避免残留旧 supersede 链
                conn.execute(
                    "UPDATE artifact_projections
                     SET status = 'active', stale_reason = '', superseded_by = NULL,
                         updated_at = datetime('now'), occurrence_id = ?1
                     WHERE id = ?2",
                    rusqlite::params![occurrence_id, stale_proj.id],
                )
                .map_err(|e| GBrainError::Database(format!("重新激活 KB 投影失败: {}", e)))?;

                let run_id = new_run_id();
                // 修复 P1：复用已删除的 kb_document 时必须同时清 deleted_at，
                // 否则后续查询用 d.deleted_at IS NULL 过滤时搜不到。
                conn.execute(
                    "UPDATE kb_documents SET processing_run_id = ?1, document_status = 'queued', \
                     ocr_status = 'not_evaluated', ocr_text_coverage = 0.0, \
                     deleted_at = NULL, updated_at = datetime('now') WHERE id = ?2",
                    rusqlite::params![run_id, doc_id],
                )
                .map_err(|e| {
                    GBrainError::Database(format!("更新 KB document run_id 失败: {}", e))
                })?;

                let artifact = store::find_artifact_by_id(conn, artifact_id)
                    .map_err(|e| GBrainError::Database(format!("查找 artifact 失败: {}", e)))?
                    .ok_or_else(|| {
                        GBrainError::PageNotFound(format!("artifact {} 不存在", artifact_id))
                    })?;

                let payload = KbProcessPayload {
                    kind: "kb_process_document".to_string(),
                    document_id: doc_id,
                    library_id,
                    processing_run_id: run_id,
                    storage_path: artifact.storage_path.clone(),
                    extension: artifact.extension.clone(),
                };

                // 修复：入队新 job 前先取消该文档的旧 pending job，
                // 防止旧 job 被认领后因 run_id 不匹配被判定为 stale。
                // 不取消 processing 状态的 job（worker 正在执行，无法中断）。
                if let Err(e) = crate::kb::jobs::cancel_pending_kb_jobs_by_document_id(conn, doc_id)
                {
                    tracing::warn!(
                        doc_id,
                        error = %e,
                        "reprocess 取消旧 KB job 失败，继续入队新 job"
                    );
                }

                let payload_json = serde_json::to_string(&payload).map_err(|e| {
                    GBrainError::Serialization(format!("序列化 KB job payload 失败: {}", e))
                })?;
                conn.execute(
                    "INSERT INTO jobs (job_type, payload, status, priority, created_at)
                     VALUES ('kb_process_document', ?1, 'pending', 0, datetime('now'))",
                    rusqlite::params![payload_json],
                )
                .map_err(|e| GBrainError::Database(format!("重新入队 KB job 失败: {}", e)))?;
                let job_id = conn.last_insert_rowid();

                conn.execute(
                    "UPDATE kb_documents SET job_id = ?1 WHERE id = ?2",
                    rusqlite::params![job_id.to_string(), doc_id],
                )
                .map_err(|e| {
                    GBrainError::Database(format!("更新 KB document job_id 失败: {}", e))
                })?;

                info!(
                    "reprocess 复用 stale KB 投影: artifact_id={}, kb_doc_id={}, job_id={}",
                    artifact_id, doc_id, job_id
                );

                return Ok(stale_proj.projection_ref);
            }
            // KB doc 不存在（已被 purge）：不激活旧 projection，
            // 走新建 kb_documents 路径，让下面的代码创建新的 KB doc 和投影。
            info!(
                "stale KB 投影的 kb_document={} 已不存在(purge)，走新建路径: artifact_id={}",
                doc_id, artifact_id
            );
        }
    }

    // 先生成 processing_run_id，确保 kb_documents 和 job payload 使用同一个 run id
    // 修复：之前 job payload 用 new_run_id() 但 kb_documents 没写入 processing_run_id，
    // 导致 worker 校验 DB 里的 run id 为空串，判定为 stale 作业
    let run_id = new_run_id();

    // 创建 kb_documents 记录 — 写入 processing_run_id 以匹配 job payload
    conn.execute(
        "INSERT INTO kb_documents
            (library_id, folder_id, original_name, name_tokens, file_size,
             content_hash, extension, mime_type, source_type, storage_path,
             original_path, source_uri, document_status, processing_run_id, created_at, updated_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            library_id,
            occurrence.folder_id,
            artifact.original_name,
            "", // name_tokens 后续由 KB worker 填充
            artifact.size_bytes,
            artifact.sha256,
            artifact.extension,
            artifact.mime_type,
            "artifact", // source_type 标记为 artifact
            artifact.storage_path,
            occurrence.original_path,
            occurrence.source_uri,
            "queued",
            run_id,
            artifact.created_at,
            artifact.updated_at,
        ],
    )
    .map_err(|e| GBrainError::Database(format!("创建 KB document 失败: {}", e)))?;
    let kb_doc_id = conn.last_insert_rowid();

    // 创建 KB 处理 job — 使用与 kb_documents 相同的 run_id
    let payload = KbProcessPayload {
        kind: "kb_process_document".to_string(),
        document_id: kb_doc_id,
        library_id,
        processing_run_id: run_id,
        storage_path: artifact.storage_path.clone(),
        extension: artifact.extension.clone(),
    };
    let payload_json = serde_json::to_string(&payload)
        .map_err(|e| GBrainError::Serialization(format!("序列化 KB job payload 失败: {}", e)))?;
    conn.execute(
        "INSERT INTO jobs (job_type, payload, status, priority, created_at)
         VALUES ('kb_process_document', ?1, 'pending', 0, datetime('now'))",
        params![payload_json],
    )
    .map_err(|e| GBrainError::Database(format!("创建 KB job 失败: {}", e)))?;
    let job_id = conn.last_insert_rowid();

    // 更新 kb_documents 的 job_id
    conn.execute(
        "UPDATE kb_documents SET job_id = ?1 WHERE id = ?2",
        params![job_id.to_string(), kb_doc_id],
    )
    .map_err(|e| GBrainError::Database(format!("更新 KB document job_id 失败: {}", e)))?;

    // 创建投影记录 — projection_ref 使用实际的 kb_document:{id}
    let proj_ref = format!("kb_document:{}", kb_doc_id);
    let now = now_str();
    let proj = ArtifactProjection {
        id: 0,
        created_at: now.clone(),
        updated_at: now.clone(),
        artifact_id,
        occurrence_id: Some(occurrence_id),
        projection_type: ProjectionType::KbDocument.to_string(),
        projection_key: proj_key.clone(),
        projection_ref: proj_ref.clone(),
        status: "active".to_string(),
        version_hash: String::new(),
        stale_reason: String::new(),
        metadata_json: format!(
            "{{\"kb_document_id\": {}, \"job_id\": {}}}",
            kb_doc_id, job_id
        ),
        superseded_by: None,
    };

    let _proj_id = store::insert_projection(conn, &proj)
        .map_err(|e| GBrainError::Database(format!("插入 KB 投影失败: {}", e)))?;

    info!(
        "创建 KB 投影: artifact_id={}, kb_doc_id={}, job_id={}",
        artifact_id, kb_doc_id, job_id
    );

    Ok(proj_ref)
}

/// 创建影子页面投影
///
/// 在 artifact_projections 表中记录 artifact -> shadow_page 的映射。
pub fn create_shadow_page_projection(
    conn: &Connection,
    artifact_id: i64,
    occurrence_id: i64,
    brain_slug: &str,
) -> Result<ArtifactProjection> {
    let proj_key = format!("slug:{}", brain_slug);
    let proj_ref = format!("slug:{}", brain_slug);

    // 检查是否已存在
    let existing = store::find_projection_by_ref(conn, "brain_shadow_page", &proj_key)
        .map_err(|e| GBrainError::Database(format!("查找影子页面投影失败: {}", e)))?;

    if let Some(existing) = existing {
        if existing.status == "active" {
            debug!(
                "影子页面投影已存在: artifact_id={}, slug={}",
                artifact_id, brain_slug
            );
            return Ok(existing);
        }
    }

    let now = now_str();
    let proj = ArtifactProjection {
        id: 0,
        created_at: now.clone(),
        updated_at: now.clone(),
        artifact_id,
        occurrence_id: Some(occurrence_id),
        projection_type: ProjectionType::BrainShadowPage.to_string(),
        projection_key: proj_key.clone(),
        projection_ref: proj_ref.clone(),
        status: "active".to_string(),
        version_hash: String::new(),
        stale_reason: String::new(),
        metadata_json: "{}".to_string(),
        superseded_by: None,
    };

    let _proj_id = store::insert_projection(conn, &proj)
        .map_err(|e| GBrainError::Database(format!("插入影子页面投影失败: {}", e)))?;

    info!(
        "创建影子页面投影: artifact_id={}, slug={}",
        artifact_id, brain_slug
    );

    Ok(proj)
}

/// 创建文件存储投影
///
/// 在 artifact_projections 表中记录 artifact -> file 的映射。
pub fn create_file_projection(
    conn: &Connection,
    artifact_id: i64,
    occurrence_id: i64,
    page_slug: &str,
    filename: &str,
    file_id: i64,
) -> Result<ArtifactProjection> {
    let proj_key = format!("page:{}:file:{}", page_slug, filename);
    let proj_ref = format!("file:{}", file_id);

    let now = now_str();
    let proj = ArtifactProjection {
        id: 0,
        created_at: now.clone(),
        updated_at: now.clone(),
        artifact_id,
        occurrence_id: Some(occurrence_id),
        projection_type: ProjectionType::FileAttachment.to_string(),
        projection_key: proj_key.clone(),
        projection_ref: proj_ref.clone(),
        status: "active".to_string(),
        version_hash: String::new(),
        stale_reason: String::new(),
        metadata_json: "{}".to_string(),
        superseded_by: None,
    };

    store::insert_projection(conn, &proj)
        .map_err(|e| GBrainError::Database(format!("插入文件投影失败: {}", e)))?;

    Ok(proj)
}

/// 标记 KB 投影为过期（KB document 被删除时调用）
pub fn mark_kb_projection_stale(
    conn: &Connection,
    artifact_id: i64,
    library_id: i64,
    reason: &str,
) -> Result<()> {
    let proj_key = format!("library:{}", library_id);
    // 查找投影
    let projections = store::find_projections_by_artifact(conn, artifact_id)
        .map_err(|e| GBrainError::Database(format!("查找投影失败: {}", e)))?;

    for proj in projections {
        if proj.projection_type == "kb_document" && proj.projection_key == proj_key {
            store::mark_projection_stale(conn, proj.id, reason)
                .map_err(|e| GBrainError::Database(format!("标记投影过期失败: {}", e)))?;
        }
    }

    Ok(())
}

/// 标记影子页面投影为过期（shadow page 被删除时调用）
pub fn mark_shadow_projection_stale(
    conn: &Connection,
    artifact_id: i64,
    brain_slug: &str,
    reason: &str,
) -> Result<()> {
    let proj_key = format!("slug:{}", brain_slug);
    let projections = store::find_projections_by_artifact(conn, artifact_id)
        .map_err(|e| GBrainError::Database(format!("查找投影失败: {}", e)))?;

    for proj in projections {
        if proj.projection_type == "brain_shadow_page" && proj.projection_key == proj_key {
            store::mark_projection_stale(conn, proj.id, reason)
                .map_err(|e| GBrainError::Database(format!("标记投影过期失败: {}", e)))?;
        }
    }

    Ok(())
}

/// 标记所有投影为过期（artifact 被删除时调用）
///
/// 返回受影响的投影数量
pub fn mark_all_projections_stale(
    conn: &Connection,
    artifact_id: i64,
    reason: &str,
) -> Result<u64> {
    let projections = store::find_projections_by_artifact(conn, artifact_id)
        .map_err(|e| GBrainError::Database(format!("查找投影失败: {}", e)))?;

    let mut count = 0u64;
    for proj in projections {
        if proj.status == "active" {
            store::mark_projection_stale(conn, proj.id, reason)
                .map_err(|e| GBrainError::Database(format!("标记投影过期失败: {}", e)))?;
            count += 1;
        }
    }

    Ok(count)
}

/// 查找 artifact 的影子页面 slug
pub fn find_shadow_page_slug(conn: &Connection, artifact_id: i64) -> Result<Option<String>> {
    let projections = store::find_projections_by_artifact(conn, artifact_id)
        .map_err(|e| GBrainError::Database(format!("查找投影失败: {}", e)))?;

    for proj in projections {
        if proj.projection_type == "brain_shadow_page" && proj.status == "active" {
            // 从 projection_ref 中提取 slug
            if let Some(slug) = proj.projection_ref.strip_prefix("slug:") {
                return Ok(Some(slug.to_string()));
            }
        }
    }

    Ok(None)
}

/// 查找 artifact 的 KB document ID
pub fn find_kb_document_id(conn: &Connection, artifact_id: i64) -> Result<Option<i64>> {
    let projections = store::find_projections_by_artifact(conn, artifact_id)
        .map_err(|e| GBrainError::Database(format!("查找投影失败: {}", e)))?;

    for proj in projections {
        if proj.projection_type == "kb_document" && proj.status == "active" {
            if let Some(id_str) = proj.projection_ref.strip_prefix("kb_document:") {
                if let Ok(id) = id_str.parse::<i64>() {
                    return Ok(Some(id));
                }
            }
        }
    }

    Ok(None)
}

/// 查找 artifact 的所有 KB document ID（不限状态）
///
/// 用于 delete_artifact 时软删除所有关联 kb_documents，
/// 包括已 stale/orphaned 的投影，确保 deleted_at 被设置。
pub fn find_all_kb_document_ids(conn: &Connection, artifact_id: i64) -> Result<Vec<i64>> {
    let projections = store::find_projections_by_artifact(conn, artifact_id)
        .map_err(|e| GBrainError::Database(format!("查找投影失败: {}", e)))?;

    let mut ids = Vec::new();
    for proj in projections {
        if proj.projection_type == "kb_document" {
            if let Some(id_str) = proj.projection_ref.strip_prefix("kb_document:") {
                if let Ok(id) = id_str.parse::<i64>() {
                    ids.push(id);
                }
            }
        }
    }

    Ok(ids)
}

/// 查找 restore 操作应恢复的 KB document ID（dedup）
///
/// 修复 P2：restore 只恢复 stale_reason='artifact_deleted' 的 projection，
/// 不恢复 detach/reprocess/superseded 主动标记的 stale 投影对应的 KB doc。
/// 与 reactivate_projections_by_artifact 的恢复范围对齐，避免误恢复。
/// 返回的 ID 列表已 dedup（同一 kb_document 可能被多个 projection 引用）。
pub fn find_kb_document_ids_for_restore(conn: &Connection, artifact_id: i64) -> Result<Vec<i64>> {
    let projections = store::find_projections_by_artifact(conn, artifact_id)
        .map_err(|e| GBrainError::Database(format!("查找投影失败: {}", e)))?;

    // 只取 stale_reason='artifact_deleted' 的 KB projection，与 reactivate 范围对齐
    let mut ids = std::collections::HashSet::new();
    for proj in projections {
        if proj.projection_type == "kb_document"
            && proj.status == "stale"
            && proj.stale_reason == "artifact_deleted"
        {
            if let Some(id_str) = proj.projection_ref.strip_prefix("kb_document:") {
                if let Ok(id) = id_str.parse::<i64>() {
                    ids.insert(id);
                }
            }
        }
    }

    Ok(ids.into_iter().collect())
}

/// 投影垃圾回收结果
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct GcResult {
    /// 清理的孤儿投影数
    pub orphaned_count: usize,
    /// 删除的过期投影数
    pub deleted_count: usize,
    /// 清理的 KB 向量数
    pub kb_vector_cleaned: usize,
    /// 清理的影子页面数
    pub shadow_page_cleaned: usize,
    /// 错误列表
    pub errors: Vec<String>,
}

/// 投影垃圾回收（§31 gc_orphan_projections）
///
/// 定期清理孤儿投影和过期投影：
/// 1. 查找所有孤儿投影（artifact 已删除但投影仍为 active）
/// 2. 将孤儿投影标记为 orphaned
/// 3. 清理关联的 KB 向量
/// 4. 清理关联的影子页面
/// 5. 删除已过期超过指定天数的投影记录
pub fn gc_orphan_projections(
    conn: &Connection,
    stale_days: u32,
    dry_run: bool,
) -> Result<GcResult> {
    let mut result = GcResult::default();

    // 1. 查找孤儿投影 — artifact 已删除但投影仍为 active/stale
    let orphan_projections = store::find_orphan_projections(conn)
        .map_err(|e| GBrainError::Database(format!("查找孤儿投影失败: {}", e)))?;

    for proj in &orphan_projections {
        if dry_run {
            result.orphaned_count += 1;
            continue;
        }

        // 修复：先清理关联数据，成功后再标 orphaned。
        // 之前先标 orphaned 再清理，清理失败后投影丢出重试集合
        // （find_orphan_projections 只查 active/stale）。
        let mut cleanup_ok = true;

        // 清理关联的 KB 向量
        if proj.projection_type == "kb_document" {
            if let Some(kb_doc_id) = proj
                .projection_ref
                .strip_prefix("kb_document:")
                .and_then(|s| s.parse::<i64>().ok())
            {
                if let Err(e) = cleanup_kb_vectors(conn, kb_doc_id) {
                    result.errors.push(format!(
                        "清理 KB 向量 kb_document:{} 失败: {}",
                        kb_doc_id, e
                    ));
                    cleanup_ok = false;
                } else {
                    result.kb_vector_cleaned += 1;
                }
            }
        }

        // 清理关联的影子页面
        if proj.projection_type == "brain_shadow_page" {
            if let Some(slug) = proj.projection_ref.strip_prefix("slug:") {
                if let Err(e) = cleanup_shadow_page(conn, slug) {
                    result
                        .errors
                        .push(format!("清理影子页面 {} 失败: {}", slug, e));
                    cleanup_ok = false;
                } else {
                    result.shadow_page_cleaned += 1;
                }
            }
        }

        // 清理成功后才标 orphaned；失败则保持 active/stale，下次 GC 可重试
        if cleanup_ok {
            if let Err(e) = store::mark_projection_orphaned(conn, proj.id) {
                result
                    .errors
                    .push(format!("标记投影 {} 为 orphaned 失败: {}", proj.id, e));
            } else {
                result.orphaned_count += 1;
            }
        }
    }

    // 2. 删除已过期超过指定天数的投影记录
    // 修复：保留期 sweep 只删除 orphaned/superseded，不删除 stale。
    // stale projection 可能是清理失败后保留的重试句柄（清理失败时不标 orphaned），
    // 如果这里也删 stale，清理失败的 projection 会丢失，KB/shadow 残留无法重试。
    let cutoff = format!("datetime('now', '-{} days')", stale_days);
    let deleted: usize = if dry_run {
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM artifact_projections
                 WHERE status IN ('orphaned', 'superseded')
                 AND updated_at < {}",
                cutoff
            ),
            [],
            |row| row.get(0),
        )
        .unwrap_or(0)
    } else {
        conn.execute(
            &format!(
                "DELETE FROM artifact_projections
                 WHERE status IN ('orphaned', 'superseded')
                 AND updated_at < {}",
                cutoff
            ),
            [],
        )
        .map_err(|e| GBrainError::Database(format!("删除过期投影失败: {}", e)))? as usize
    };
    result.deleted_count = deleted;

    info!(
        "投影 GC 完成: 孤儿={}, 删除={}, KB清理={}, 影子页面清理={}, dry_run={}",
        result.orphaned_count,
        result.deleted_count,
        result.kb_vector_cleaned,
        result.shadow_page_cleaned,
        dry_run
    );

    Ok(result)
}

/// 清理 KB 向量 — 软删除 kb_document 并清理关联数据
///
/// 修复：设置 deleted_at 而非仅 document_status='deleted'，
/// 否则唯一索引 idx_kb_docs_library_hash（WHERE deleted_at IS NULL AND purged_at IS NULL）
/// 仍会排除已删除行，导致同 hash 重传撞唯一约束。
fn cleanup_kb_vectors(conn: &Connection, kb_doc_id: i64) -> Result<()> {
    // 软删除 kb_document：同时设置 deleted_at 和 document_status
    crate::kb::lifecycle::soft_delete_document(conn, kb_doc_id)?;

    // 修复：同步标记关联 provenance 为 stale，失败则返回错误
    // 让外层 cleanup_ok=false，保留 projection 以便后续重试。
    // 之前只 warn 后继续，外层会认为 KB 清理成功，
    // 可能把 projection 标为 orphaned 后删除，留下仍为 active 的 provenance。
    crate::artifact::provenance::mark_provenance_stale_by_kb_document(
        conn,
        kb_doc_id,
        "kb_document_gc_cleanup",
    )?;

    // 修复：先收集所有 node_id，逐个调用 cleanup_node_vectors 清理
    // vec_kb_nodes、per-index vec_kb_{id} 虚表和 kb_node_embeddings，
    // 再删 kb_document_nodes。之前只删 kb_node_embeddings + kb_document_nodes，
    // 但 vec_kb_nodes 和 vec_kb_{index_id} 中的向量数据会残留，
    // 随时间累积影响搜索结果和磁盘空间。
    let node_ids: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT id FROM kb_document_nodes WHERE document_id = ?1")
            .map_err(|e| GBrainError::Database(format!("查询 KB 文档节点失败: {}", e)))?;
        stmt.query_map(rusqlite::params![kb_doc_id], |row| row.get(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };

    for node_id in node_ids {
        crate::kb::engine::cleanup_node_vectors(conn, node_id);
    }

    // 删除 kb_document_nodes，触发 FTS 清理。
    // 删除 nodes 后，kb_doc_fts 的触发器会自动清理 FTS 索引
    conn.execute(
        "DELETE FROM kb_document_nodes WHERE document_id = ?1",
        rusqlite::params![kb_doc_id],
    )
    .map_err(|e| GBrainError::Database(format!("清理 KB 文档节点失败: {}", e)))?;

    debug!("KB 向量清理完成: kb_document_id={}", kb_doc_id);
    Ok(())
}

/// 清理影子页面 — 删除 pages 表中对应的记录
fn cleanup_shadow_page(conn: &Connection, slug: &str) -> Result<()> {
    // 先保存版本历史到 page_versions（旧代码引用不存在的 pages_version_history 表）
    let now = now_str();
    conn.execute(
        "INSERT OR IGNORE INTO page_versions (page_id, compiled_truth, frontmatter, title, page_type, snapshot_at)
         SELECT id, compiled_truth, frontmatter, title, page_type, ?2 FROM pages WHERE slug = ?1",
        rusqlite::params![slug, now],
    )
    .map_err(|e| GBrainError::Database(format!("保存影子页面版本历史失败: {}", e)))?;

    // 删除页面
    conn.execute(
        "DELETE FROM pages WHERE slug = ?1 AND page_type = 'source'",
        rusqlite::params![slug],
    )
    .map_err(|e| GBrainError::Database(format!("删除影子页面失败: {}", e)))?;

    debug!("影子页面清理完成: slug={}", slug);
    Ok(())
}

fn now_str() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 替代旧投影（§31 版本链 superseded_by）
///
/// 当同一 projection_key 下创建新投影时，将旧投影标记为 superseded，
/// 并设置 superseded_by 指向新投影 ID。
pub fn supersede_projection(
    conn: &Connection,
    old_proj_id: i64,
    new_proj_id: i64,
) -> crate::error::Result<()> {
    store::supersede_projection(conn, old_proj_id, new_proj_id)
        .map_err(|e| GBrainError::Database(format!("替代投影失败: {}", e)))?;

    // 记录事件
    let _ = store::record_event(
        conn,
        None,
        None,
        "projection_superseded",
        "supersede_projection",
        &serde_json::json!({"old_proj_id": old_proj_id, "new_proj_id": new_proj_id}).to_string(),
    );

    info!(
        "投影替代: old_proj_id={} -> new_proj_id={}",
        old_proj_id, new_proj_id
    );
    Ok(())
}

/// 查询投影版本链（§31）
///
/// 按 projection_key 查询历史投影记录，最新在前。
/// 修复：增加 artifact_id 和 projection_type 可选过滤，避免同一 library 下
/// 多个 artifact 的投影混合
pub fn get_projection_history(
    conn: &Connection,
    projection_key: &str,
    artifact_id: Option<i64>,
    projection_type: Option<&str>,
    limit: i64,
) -> crate::error::Result<Vec<ArtifactProjection>> {
    store::find_projection_history(conn, projection_key, artifact_id, projection_type, limit)
        .map_err(|e| GBrainError::Database(format!("查询投影历史失败: {}", e)))
}

/// 按 ID 查找投影
pub fn find_projection_by_id(
    conn: &Connection,
    id: i64,
) -> crate::error::Result<Option<ArtifactProjection>> {
    store::find_projection_by_id(conn, id)
        .map_err(|e| GBrainError::Database(format!("查找投影失败: {}", e)))
}

/// 按 occurrence_id 标记关联投影为 stale（detach 操作）
///
/// 将指定 occurrence 下所有活跃投影标记为 stale
pub fn mark_projections_stale_by_occurrence(
    conn: &Connection,
    occurrence_id: i64,
    reason: &str,
) -> Result<u64> {
    // 查找该 occurrence 关联的所有活跃投影
    let count = conn
        .execute(
            "UPDATE artifact_projections
             SET status = 'stale', stale_reason = ?1, updated_at = datetime('now')
             WHERE occurrence_id = ?2 AND status = 'active'",
            params![reason, occurrence_id],
        )
        .map_err(|e| GBrainError::Database(format!("按 occurrence 标记投影过期失败: {}", e)))?
        as u64;

    Ok(count)
}

/// 恢复指定 artifact 的因 delete 而变 stale 的投影（restore 操作）
///
/// 修复：只恢复 stale_reason='artifact_deleted' 的投影，
/// 不恢复 detach/reprocess 主动标记的 stale 投影，
/// 避免误恢复用户主动解除的关联。
pub fn reactivate_projections_by_artifact(conn: &Connection, artifact_id: i64) -> Result<u64> {
    let count = conn
        .execute(
            "UPDATE artifact_projections
             SET status = 'active', stale_reason = '', updated_at = datetime('now')
             WHERE artifact_id = ?1 AND status = 'stale' AND stale_reason = 'artifact_deleted'",
            params![artifact_id],
        )
        .map_err(|e| GBrainError::Database(format!("恢复投影失败: {}", e)))? as u64;

    Ok(count)
}
