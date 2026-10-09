//! 原件存储层 — source_artifacts / artifact_occurrences / artifact_projections 的 CRUD
//!
//! 负责与 SQLite 交互，提供去重写入、投影注册等基础操作。

use rusqlite::{params, Connection, Row};
use tracing::{debug, info, warn};

use super::types::*;

// ============================================================================
// SourceArtifact CRUD
// ============================================================================

/// 按 sha256 查找原件（未清除的）
pub fn find_artifact_by_sha256(
    conn: &Connection,
    sha256: &str,
) -> Result<Option<SourceArtifact>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, artifact_uid, created_at, updated_at, last_seen_at,
                sha256, original_name, extension, mime_type, size_bytes,
                storage_path, canonical_slug, status, metadata_json,
                deleted_at, purged_at
         FROM source_artifacts
         WHERE sha256 = ?1 AND purged_at IS NULL
         LIMIT 1",
    )?;
    let mut rows = stmt.query(params![sha256])?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_source_artifact(row)?)),
        None => Ok(None),
    }
}

/// 按 ID 查找原件
pub fn find_artifact_by_id(
    conn: &Connection,
    id: i64,
) -> Result<Option<SourceArtifact>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, artifact_uid, created_at, updated_at, last_seen_at,
                sha256, original_name, extension, mime_type, size_bytes,
                storage_path, canonical_slug, status, metadata_json,
                deleted_at, purged_at
         FROM source_artifacts WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![id])?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_source_artifact(row)?)),
        None => Ok(None),
    }
}

/// 按 UID 查找原件
pub fn find_artifact_by_uid(
    conn: &Connection,
    uid: &str,
) -> Result<Option<SourceArtifact>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, artifact_uid, created_at, updated_at, last_seen_at,
                sha256, original_name, extension, mime_type, size_bytes,
                storage_path, canonical_slug, status, metadata_json,
                deleted_at, purged_at
         FROM source_artifacts WHERE artifact_uid = ?1",
    )?;
    let mut rows = stmt.query(params![uid])?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_source_artifact(row)?)),
        None => Ok(None),
    }
}

/// 按 canonical_slug 查找原件
///
/// P2-5 修复：过滤 purged 状态，按 updated_at DESC 排序，
/// 确保同 slug 多个 artifact 时返回最新的有效行。
pub fn find_artifact_by_slug(
    conn: &Connection,
    slug: &str,
) -> Result<Option<SourceArtifact>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, artifact_uid, created_at, updated_at, last_seen_at,
                sha256, original_name, extension, mime_type, size_bytes,
                storage_path, canonical_slug, status, metadata_json,
                deleted_at, purged_at
         FROM source_artifacts WHERE canonical_slug = ?1 AND purged_at IS NULL
         ORDER BY updated_at DESC LIMIT 1",
    )?;
    let mut rows = stmt.query(params![slug])?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_source_artifact(row)?)),
        None => Ok(None),
    }
}

/// P1-9 修复：按 canonical_slug 查找所有活跃原件
///
/// 冲突检测需要查找同 slug 下所有 artifact 的 brain_page_update 投影，
/// 而非仅依赖 find_artifact_by_slug 返回的最新 artifact。
/// 因为冲突分支的 pending artifact 没有 brain_page_update 投影，
/// 但旧稳定 artifact 仍保留该投影作为冲突检测基线。
pub fn find_artifacts_by_slug(
    conn: &Connection,
    slug: &str,
) -> Result<Vec<SourceArtifact>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, artifact_uid, created_at, updated_at, last_seen_at,
                sha256, original_name, extension, mime_type, size_bytes,
                storage_path, canonical_slug, status, metadata_json,
                deleted_at, purged_at
         FROM source_artifacts WHERE canonical_slug = ?1 AND status = 'active' AND purged_at IS NULL
         ORDER BY updated_at DESC",
    )?;
    let rows = stmt.query_map(params![slug], row_to_source_artifact)?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// 插入新原件（write-if-absent 语义）
pub fn insert_artifact(
    conn: &Connection,
    artifact: &SourceArtifact,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO source_artifacts
            (artifact_uid, sha256, original_name, extension, mime_type, size_bytes,
             storage_path, canonical_slug, status, metadata_json, created_at, updated_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            artifact.artifact_uid,
            artifact.sha256,
            artifact.original_name,
            artifact.extension,
            artifact.mime_type,
            artifact.size_bytes,
            artifact.storage_path,
            artifact.canonical_slug,
            artifact.status,
            artifact.metadata_json,
            artifact.created_at,
            artifact.updated_at,
            artifact.last_seen_at,
        ],
    )?;
    let id = conn.last_insert_rowid();
    debug!(
        "insert_artifact: id={}, uid={}, content_type={}",
        id, artifact.artifact_uid, artifact.mime_type
    );
    Ok(id)
}

/// 更新原件的 last_seen_at
pub fn touch_artifact(conn: &Connection, id: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE source_artifacts SET last_seen_at = datetime('now'), updated_at = datetime('now') WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// 重新激活已软删除的 artifact
///
/// 修复：find_artifact_by_sha256 不排除 status='deleted'，
/// 上传复用后只 touch_artifact 不恢复 status/deleted_at，
/// 导致新 occurrence/projection 挂到 deleted artifact 上，
/// 列表查不到，GC 又把它当孤儿投影处理。
/// 现在明确做 reactivation：恢复 status 为 active，清除 deleted_at。
pub fn reactivate_artifact(conn: &Connection, id: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE source_artifacts SET status = 'active', deleted_at = NULL, last_seen_at = datetime('now'), updated_at = datetime('now') WHERE id = ?1",
        params![id],
    )?;
    info!("reactivate_artifact: id={}", id);
    Ok(())
}

/// 软删除原件
pub fn soft_delete_artifact(conn: &Connection, id: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE source_artifacts SET status = 'deleted', deleted_at = datetime('now'), updated_at = datetime('now') WHERE id = ?1",
        params![id],
    )?;
    warn!("soft_delete_artifact: id={}, reason=artifact_deleted", id);
    Ok(())
}

/// 列出所有活跃原件
pub fn list_active_artifacts(
    conn: &Connection,
    limit: i64,
    offset: i64,
) -> Result<Vec<SourceArtifact>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, artifact_uid, created_at, updated_at, last_seen_at,
                sha256, original_name, extension, mime_type, size_bytes,
                storage_path, canonical_slug, status, metadata_json,
                deleted_at, purged_at
         FROM source_artifacts
         WHERE status = 'active' AND purged_at IS NULL
         ORDER BY updated_at DESC LIMIT ?1 OFFSET ?2",
    )?;
    let rows = stmt.query_map(params![limit, offset], row_to_source_artifact)?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// 统计活跃原件数
pub fn count_active_artifacts(conn: &Connection) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT COUNT(*) FROM source_artifacts WHERE status = 'active' AND purged_at IS NULL",
        [],
        |row| row.get(0),
    )
}

// ============================================================================
// ArtifactOccurrence CRUD
// ============================================================================

/// 插入事件
pub fn insert_occurrence(
    conn: &Connection,
    occ: &ArtifactOccurrence,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO artifact_occurrences
            (occurrence_uid, artifact_id, source_kind, source_uri, original_path, original_name,
             owner_ref, intent, target_slug, page_slug, library_id, folder_id,
             promotion_policy, status, stale_reason, metadata_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            occ.occurrence_uid,
            occ.artifact_id,
            occ.source_kind,
            occ.source_uri,
            occ.original_path,
            occ.original_name,
            occ.owner_ref,
            occ.intent,
            occ.target_slug,
            occ.page_slug,
            occ.library_id,
            occ.folder_id,
            occ.promotion_policy,
            occ.status,
            occ.stale_reason,
            occ.metadata_json,
            occ.created_at,
            occ.updated_at,
        ],
    )?;
    let id = conn.last_insert_rowid();
    debug!(
        "insert_occurrence: id={}, artifact_id={}, intent={}, target_slug={}",
        id, occ.artifact_id, occ.intent, occ.target_slug
    );
    Ok(id)
}

/// 按原件 ID 查找事件列表
pub fn find_occurrences_by_artifact(
    conn: &Connection,
    artifact_id: i64,
) -> Result<Vec<ArtifactOccurrence>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, occurrence_uid, created_at, updated_at,
                artifact_id, source_kind, source_uri, original_path, original_name, owner_ref,
                intent, target_slug, page_slug, library_id, folder_id, promotion_policy,
                status, stale_reason, metadata_json
         FROM artifact_occurrences WHERE artifact_id = ?1 ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map(params![artifact_id], row_to_artifact_occurrence)?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// 按 ID 查找事件
pub fn find_occurrence_by_id(
    conn: &Connection,
    id: i64,
) -> Result<Option<ArtifactOccurrence>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, occurrence_uid, created_at, updated_at,
                artifact_id, source_kind, source_uri, original_path, original_name, owner_ref,
                intent, target_slug, page_slug, library_id, folder_id, promotion_policy,
                status, stale_reason, metadata_json
         FROM artifact_occurrences WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![id])?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_artifact_occurrence(row)?)),
        None => Ok(None),
    }
}

// ============================================================================
// ArtifactProjection CRUD
// ============================================================================

/// 插入投影
///
/// 修复：移除表级 UNIQUE 约束，改用 partial unique index（仅 status='active' 唯一）。
/// 当同一 key 下已有 active 投影且 occurrence_id 不同时，先标记旧行为 superseded，
/// 再插入新 active 行，最后回填旧行 superseded_by 指向新行。
/// 同一 occurrence_id 时原地更新内容即可。
/// 旧方案先插新 active 再标旧 superseded 会撞唯一约束，导致重复上传失败。
pub fn insert_projection(
    conn: &Connection,
    proj: &ArtifactProjection,
) -> Result<i64, rusqlite::Error> {
    // 先查找同一 key 下的 active 投影
    let existing: Option<(i64, Option<i64>)> = conn
        .query_row(
            "SELECT id, occurrence_id FROM artifact_projections
             WHERE artifact_id = ?1 AND projection_type = ?2 AND projection_key = ?3
               AND status = 'active'
             LIMIT 1",
            params![proj.artifact_id, proj.projection_type, proj.projection_key],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .ok();

    if let Some((existing_id, existing_occurrence_id)) = existing {
        if existing_occurrence_id == proj.occurrence_id {
            // 同一 occurrence，原地更新即可
            conn.execute(
                "UPDATE artifact_projections
                 SET projection_ref = ?1, version_hash = ?2, metadata_json = ?3, updated_at = datetime('now')
                 WHERE id = ?4",
                params![proj.projection_ref, proj.version_hash, proj.metadata_json, existing_id],
            )?;
            return Ok(existing_id);
        }
        // 不同 occurrence：先标记旧行为 superseded（释放 active 唯一约束位置），
        // 再插入新 active 行，最后回填旧行 superseded_by 指向新行
        conn.execute(
            "UPDATE artifact_projections
             SET status = 'superseded', stale_reason = 'replaced_by_new_occurrence', updated_at = datetime('now')
             WHERE id = ?1",
            params![existing_id],
        )?;

        // 插入新 active 行（此时旧 active 已变为 superseded，不会撞唯一约束）
        conn.execute(
            "INSERT INTO artifact_projections
                (artifact_id, occurrence_id, projection_type, projection_key, projection_ref,
                 status, version_hash, stale_reason, metadata_json, superseded_by, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?11)",
            params![
                proj.artifact_id,
                proj.occurrence_id,
                proj.projection_type,
                proj.projection_key,
                proj.projection_ref,
                proj.status,
                proj.version_hash,
                proj.stale_reason,
                proj.metadata_json,
                proj.created_at,
                proj.updated_at,
            ],
        )?;
        let new_id = conn.last_insert_rowid();

        // 回填旧行 superseded_by 指向新行
        conn.execute(
            "UPDATE artifact_projections
             SET superseded_by = ?1
             WHERE id = ?2",
            params![new_id, existing_id],
        )?;

        info!(
            artifact_id = proj.artifact_id,
            projection_type = proj.projection_type,
            projection_key = proj.projection_key,
            old_id = existing_id,
            new_id,
            "投影版本链：旧投影已标记为 superseded"
        );
        return Ok(new_id);
    }

    // 无冲突，直接插入
    conn.execute(
        "INSERT INTO artifact_projections
            (artifact_id, occurrence_id, projection_type, projection_key, projection_ref,
             status, version_hash, stale_reason, metadata_json, superseded_by, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            proj.artifact_id,
            proj.occurrence_id,
            proj.projection_type,
            proj.projection_key,
            proj.projection_ref,
            proj.status,
            proj.version_hash,
            proj.stale_reason,
            proj.metadata_json,
            proj.superseded_by,
            proj.created_at,
            proj.updated_at,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// P2-11: 插入投影并返回自增 id，供 apply 时标记旧投影 superseded_by 使用
pub fn insert_projection_returning_id(
    conn: &Connection,
    proj: &ArtifactProjection,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO artifact_projections
            (artifact_id, occurrence_id, projection_type, projection_key, projection_ref,
             status, version_hash, stale_reason, metadata_json, superseded_by, created_at, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        rusqlite::params![
            proj.artifact_id, proj.occurrence_id, proj.projection_type,
            proj.projection_key, proj.projection_ref, proj.status,
            proj.version_hash, proj.stale_reason, proj.metadata_json,
            proj.superseded_by, proj.created_at, proj.updated_at
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 按原件 ID 查找投影
pub fn find_projections_by_artifact(
    conn: &Connection,
    artifact_id: i64,
) -> Result<Vec<ArtifactProjection>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, created_at, updated_at, artifact_id, occurrence_id, projection_type,
                projection_key, projection_ref, status, version_hash, stale_reason,
                metadata_json, superseded_by
         FROM artifact_projections WHERE artifact_id = ?1 ORDER BY created_at",
    )?;
    let rows = stmt.query_map(params![artifact_id], row_to_artifact_projection)?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// 按投影类型和引用查找当前活跃投影
///
/// 修复：只查 status='active' 的投影，避免返回已被 superseded/stale 的历史行。
/// 历史投影查询请用 find_projection_history_by_ref。
pub fn find_projection_by_ref(
    conn: &Connection,
    projection_type: &str,
    projection_ref: &str,
) -> Result<Option<ArtifactProjection>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, created_at, updated_at,
                artifact_id, occurrence_id, projection_type, projection_key, projection_ref,
                status, version_hash, stale_reason, metadata_json, superseded_by
         FROM artifact_projections
         WHERE projection_type = ?1 AND projection_ref = ?2 AND status = 'active'
         LIMIT 1",
    )?;
    let mut rows = stmt.query(params![projection_type, projection_ref])?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_artifact_projection(row)?)),
        None => Ok(None),
    }
}

/// P2-10 修复：按 slug 查找最新活跃 brain_page_update 投影的 version_hash
///
/// 冲突检测需要获取"最近一次成功写入稳定页面时的 page hash"作为基线。
/// 旧方案遍历同 slug 所有 artifact 再取第一个非空 version_hash，
/// 依赖 artifact 的 updated_at 间接排序，可能取到旧 hash 导致误判。
/// 新方案直接按 artifact_projections.updated_at DESC, id DESC 排序，
/// 确保取到最新已应用的 brain_page_update 投影。
pub fn find_latest_page_update_hash_by_slug(
    conn: &Connection,
    slug: &str,
) -> Result<Option<String>, rusqlite::Error> {
    // brain_page_update 投影的 projection_ref 格式为 "brain_page:{slug}"
    let projection_ref = format!("brain_page:{}", slug);
    conn.query_row(
        "SELECT p.version_hash
         FROM artifact_projections p
         JOIN source_artifacts a ON p.artifact_id = a.id
         WHERE a.canonical_slug = ?1
           AND a.status = 'active'
           AND a.purged_at IS NULL
           AND p.projection_type = 'brain_page_update'
           AND p.projection_ref = ?2
           AND p.status = 'active'
           AND p.version_hash != ''
         ORDER BY p.updated_at DESC, p.id DESC
         LIMIT 1",
        params![slug, projection_ref],
        |row| row.get(0),
    )
    .map(Some)
    .or_else(|e| {
        // 查无结果时返回 None，其它错误仍返回
        if e == rusqlite::Error::QueryReturnedNoRows {
            Ok(None)
        } else {
            Err(e)
        }
    })
}

/// 按投影类型和引用查找投影（含历史行，按 created_at DESC 排列）
///
/// 用于追溯投影版本链，返回所有状态（active/superseded/stale/orphaned）的投影。
pub fn find_projection_history_by_ref(
    conn: &Connection,
    projection_type: &str,
    projection_ref: &str,
    limit: i64,
) -> Result<Vec<ArtifactProjection>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, created_at, updated_at,
                artifact_id, occurrence_id, projection_type, projection_key, projection_ref,
                status, version_hash, stale_reason, metadata_json, superseded_by
         FROM artifact_projections
         WHERE projection_type = ?1 AND projection_ref = ?2
         ORDER BY created_at DESC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![projection_type, projection_ref, limit], |row| {
        row_to_artifact_projection(row)
    })?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// 按投影类型、key 和 artifact_id 查找当前活跃投影（用于去重检查）
///
/// 修复：只查 status='active' 的投影，避免返回已被 superseded/stale 的历史行。
/// 之前没有过滤 status，同一 key 下 active 和 superseded 行共存时可能返回旧投影，
/// 影响 KB projection 复用、EvidenceFirst 关联 artifact、shadow page 查找。
pub fn find_projection_by_key(
    conn: &Connection,
    artifact_id: i64,
    projection_type: &str,
    projection_key: &str,
) -> Result<Option<ArtifactProjection>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, created_at, updated_at,
                artifact_id, occurrence_id, projection_type, projection_key, projection_ref,
                status, version_hash, stale_reason, metadata_json, superseded_by
         FROM artifact_projections
         WHERE artifact_id = ?1 AND projection_type = ?2 AND projection_key = ?3
           AND status = 'active'
         LIMIT 1",
    )?;
    let mut rows = stmt.query(params![artifact_id, projection_type, projection_key])?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_artifact_projection(row)?)),
        None => Ok(None),
    }
}

/// M24 修复：按投影键查询版本链（§31）
///
/// 返回按 created_at DESC 排列的投影历史，最新在前。
/// 动态拼接 WHERE 条件，消除原有的 4 个重复 SQL 分支。
pub fn find_projection_history(
    conn: &Connection,
    projection_key: &str,
    artifact_id: Option<i64>,
    projection_type: Option<&str>,
    limit: i64,
) -> Result<Vec<ArtifactProjection>, rusqlite::Error> {
    const PROJECTION_HISTORY_SELECT: &str = "\
        SELECT p.id, p.created_at, p.updated_at, \
               p.artifact_id, p.occurrence_id, p.projection_type, p.projection_key, p.projection_ref, \
               p.status, p.version_hash, p.stale_reason, p.metadata_json, p.superseded_by \
        FROM artifact_projections p";

    let mut where_clauses = vec!["p.projection_key = ?1".to_string()];
    let mut param_idx = 1u32;

    if artifact_id.is_some() {
        param_idx += 1;
        where_clauses.push(format!("p.artifact_id = ?{}", param_idx));
    }
    if projection_type.is_some() {
        param_idx += 1;
        where_clauses.push(format!("p.projection_type = ?{}", param_idx));
    }
    param_idx += 1;
    let limit_param = param_idx;

    let sql = format!(
        "{} WHERE {} ORDER BY p.created_at DESC LIMIT ?{}",
        PROJECTION_HISTORY_SELECT,
        where_clauses.join(" AND "),
        limit_param,
    );

    let mut stmt = conn.prepare(&sql)?;

    // 构建动态参数列表
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    params.push(Box::new(projection_key.to_string()));
    if let Some(aid) = artifact_id {
        params.push(Box::new(aid));
    }
    if let Some(pt) = projection_type {
        params.push(Box::new(pt.to_string()));
    }
    params.push(Box::new(limit));

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), row_to_artifact_projection)?;
    rows.collect::<Result<Vec<_>, _>>()
}

/// 按 artifact_id + projection_type + projection_key 查找投影（不限状态）
///
/// 修复：reprocess 先标 stale 后调 create_kb_projection，
/// find_projection_by_key 只查 active 找不到旧投影，
/// 导致走新建 kb_documents 撞唯一索引。
/// 此函数不限状态，让 create_kb_projection 能复用 stale 的同 artifact KB projection。
pub fn find_projection_by_key_any_status(
    conn: &Connection,
    artifact_id: i64,
    projection_type: &str,
    projection_key: &str,
) -> Result<Option<ArtifactProjection>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, created_at, updated_at,
                artifact_id, occurrence_id, projection_type, projection_key, projection_ref,
                status, version_hash, stale_reason, metadata_json, superseded_by
         FROM artifact_projections
         WHERE artifact_id = ?1 AND projection_type = ?2 AND projection_key = ?3
         ORDER BY CASE WHEN status = 'active' THEN 0 ELSE 1 END, updated_at DESC
         LIMIT 1",
    )?;
    let mut rows = stmt.query(params![artifact_id, projection_type, projection_key])?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_artifact_projection(row)?)),
        None => Ok(None),
    }
}

/// 标记投影为过期
pub fn mark_projection_stale(
    conn: &Connection,
    id: i64,
    reason: &str,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE artifact_projections SET status = 'stale', stale_reason = ?1, updated_at = datetime('now') WHERE id = ?2",
        params![reason, id],
    )?;
    info!("mark_projection_stale: id={}, reason={}", id, reason);
    Ok(())
}

/// 更新投影的 version_hash（P1 修复：人工修改冲突检测）
///
/// brain_page_update 投影的 version_hash 存储的是上次 artifact 写入时的
/// page content_hash，而非 artifact 的 sha256。
/// 这样下次 artifact_put 时可以比较当前 page_hash 与上次写入时的 page_hash，
/// 判断页面是否被人工修改过。
pub fn update_projection_version_hash(
    conn: &Connection,
    id: i64,
    version_hash: &str,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE artifact_projections SET version_hash = ?1, updated_at = datetime('now') WHERE id = ?2",
        params![version_hash, id],
    )?;
    Ok(())
}

/// 标记投影为孤儿（artifact 已删除）
pub fn mark_projection_orphaned(conn: &Connection, id: i64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE artifact_projections SET status = 'orphaned', stale_reason = 'artifact_deleted', updated_at = datetime('now') WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// 查找孤立投影（artifact 已删除但投影仍 active 或 stale）
///
/// 修复：之前只查 p.status = 'active'，但 delete_artifact 先把投影标为 stale，
// stale 投影也不会被 GC 删除（只删 orphaned/superseded），导致 stale 投影永远不会被清理。
pub fn find_orphan_projections(
    conn: &Connection,
) -> Result<Vec<ArtifactProjection>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.created_at, p.updated_at,
                p.artifact_id, p.occurrence_id, p.projection_type, p.projection_key, p.projection_ref,
                p.status, p.version_hash, p.stale_reason, p.metadata_json, p.superseded_by
         FROM artifact_projections p
         LEFT JOIN source_artifacts a ON p.artifact_id = a.id
         WHERE p.status IN ('active', 'stale') AND (a.id IS NULL OR a.status != 'active')
         ORDER BY p.created_at"
    )?;
    let rows = stmt.query_map([], row_to_artifact_projection)?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// 统计过期投影数
pub fn count_stale_projections(conn: &Connection) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT COUNT(*) FROM artifact_projections WHERE status = 'stale'",
        [],
        |row| row.get(0),
    )
}

/// 标记旧投影被新投影替代（§31 版本链）
///
/// 将 old_proj_id 的 superseded_by 设为 new_proj_id，状态改为 superseded
pub fn supersede_projection(
    conn: &Connection,
    old_proj_id: i64,
    new_proj_id: i64,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE artifact_projections
         SET status = 'superseded', superseded_by = ?1, updated_at = datetime('now')
         WHERE id = ?2 AND status = 'active'",
        params![new_proj_id, old_proj_id],
    )?;
    Ok(())
}

/// 按 ID 查找投影
pub fn find_projection_by_id(
    conn: &Connection,
    id: i64,
) -> Result<Option<ArtifactProjection>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, created_at, updated_at,
                artifact_id, occurrence_id, projection_type, projection_key, projection_ref,
                status, version_hash, stale_reason, metadata_json, superseded_by
         FROM artifact_projections WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![id])?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_artifact_projection(row)?)),
        None => Ok(None),
    }
}

// ============================================================================
// artifact_events 审计（§7.6）
// ============================================================================

/// 记录 artifact 事件
pub fn record_event(
    conn: &Connection,
    artifact_id: Option<i64>,
    occurrence_id: Option<i64>,
    event_type: &str,
    actor: &str,
    payload: &str,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO artifact_events (artifact_id, occurrence_id, event_type, actor, payload_json)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![artifact_id, occurrence_id, event_type, actor, payload],
    )?;
    let id = conn.last_insert_rowid();
    debug!(
        "record_event: id={}, artifact_id={:?}, event_type={}, actor={}",
        id, artifact_id, event_type, actor
    );
    Ok(id)
}

/// 查询某 artifact 的事件历史
pub fn find_events_by_artifact(
    conn: &Connection,
    artifact_id: i64,
    limit: i64,
) -> Result<Vec<ArtifactEvent>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, created_at, artifact_id, occurrence_id, event_type, actor, payload_json
         FROM artifact_events
         WHERE artifact_id = ?1
         ORDER BY created_at DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![artifact_id, limit], |row| {
        Ok(ArtifactEvent {
            id: row.get(0)?,
            created_at: row.get(1)?,
            artifact_id: row.get(2)?,
            occurrence_id: row.get(3)?,
            event_type: row.get(4)?,
            actor: row.get(5)?,
            payload_json: row.get(6)?,
        })
    })?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// 查询某 occurrence 的事件历史
pub fn find_events_by_occurrence(
    conn: &Connection,
    occurrence_id: i64,
    limit: i64,
) -> Result<Vec<ArtifactEvent>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, created_at, artifact_id, occurrence_id, event_type, actor, payload_json
         FROM artifact_events
         WHERE occurrence_id = ?1
         ORDER BY created_at DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![occurrence_id, limit], |row| {
        Ok(ArtifactEvent {
            id: row.get(0)?,
            created_at: row.get(1)?,
            artifact_id: row.get(2)?,
            occurrence_id: row.get(3)?,
            event_type: row.get(4)?,
            actor: row.get(5)?,
            payload_json: row.get(6)?,
        })
    })?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

// ============================================================================
// Row 映射辅助
// ============================================================================

fn row_to_source_artifact(row: &Row) -> Result<SourceArtifact, rusqlite::Error> {
    Ok(SourceArtifact {
        id: row.get(0)?,
        artifact_uid: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
        last_seen_at: row.get(4)?,
        sha256: row.get(5)?,
        original_name: row.get(6)?,
        extension: row.get(7)?,
        mime_type: row.get(8)?,
        size_bytes: row.get(9)?,
        storage_path: row.get(10)?,
        canonical_slug: row.get(11)?,
        status: row.get(12)?,
        metadata_json: row.get(13)?,
        deleted_at: row.get(14)?,
        purged_at: row.get(15)?,
    })
}

fn row_to_artifact_occurrence(row: &Row) -> Result<ArtifactOccurrence, rusqlite::Error> {
    Ok(ArtifactOccurrence {
        id: row.get(0)?,
        occurrence_uid: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
        artifact_id: row.get(4)?,
        source_kind: row.get(5)?,
        source_uri: row.get(6)?,
        original_path: row.get(7)?,
        original_name: row.get(8)?,
        owner_ref: row.get(9)?,
        intent: row.get(10)?,
        target_slug: row.get(11)?,
        page_slug: row.get(12)?,
        library_id: row.get(13)?,
        folder_id: row.get(14)?,
        promotion_policy: row.get(15)?,
        status: row.get(16)?,
        stale_reason: row.get(17)?,
        metadata_json: row.get(18)?,
    })
}

fn row_to_artifact_projection(row: &Row) -> Result<ArtifactProjection, rusqlite::Error> {
    Ok(ArtifactProjection {
        id: row.get(0)?,
        created_at: row.get(1)?,
        updated_at: row.get(2)?,
        artifact_id: row.get(3)?,
        occurrence_id: row.get(4)?,
        projection_type: row.get(5)?,
        projection_key: row.get(6)?,
        projection_ref: row.get(7)?,
        status: row.get(8)?,
        version_hash: row.get(9)?,
        stale_reason: row.get(10)?,
        metadata_json: row.get(11)?,
        superseded_by: row.get(12)?,
    })
}

// ============================================================================
// detach / restore / reprocess 辅助函数（设计文档 §4.1.4）
// ============================================================================

/// 查找指定 artifact 与 target_slug 关联的活跃 occurrence
pub fn find_active_occurrences_by_artifact_and_slug(
    conn: &Connection,
    artifact_id: i64,
    target_slug: &str,
) -> Result<Vec<ArtifactOccurrence>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, occurrence_uid, created_at, updated_at,
                artifact_id, source_kind, source_uri, original_path, original_name,
                owner_ref, intent, target_slug, page_slug,
                library_id, folder_id, promotion_policy,
                status, stale_reason, metadata_json
         FROM artifact_occurrences
         WHERE artifact_id = ?1 AND target_slug = ?2 AND status = 'active'",
    )?;
    let mut rows = stmt.query(params![artifact_id, target_slug])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push(row_to_artifact_occurrence(row)?);
    }
    Ok(result)
}

/// 将 occurrence 标记为 stale（detach 操作）
///
/// P1-3 修复：接收 reason 参数，detach 时写 'detached_by_user'，
/// reprocess 时写 'reprocess_requested'，确保 restore 不会误恢复 detach 的 occurrence。
pub fn stale_occurrence(
    conn: &Connection,
    occurrence_id: i64,
    reason: &str,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE artifact_occurrences SET status = 'stale', stale_reason = ?1, updated_at = datetime('now') WHERE id = ?2",
        params![reason, occurrence_id],
    )?;
    Ok(())
}

/// 恢复指定 artifact 的 occurrence（delete/restore 操作）
///
/// P1-3 修复：只恢复因 artifact 删除而 stale 的 occurrence（stale_reason='artifact_deleted'），
/// 不恢复因用户 detach 而标记的 occurrence（stale_reason='detached_by_user'），
/// 也不恢复因 reprocess 而标记的 occurrence（stale_reason='reprocess_requested'）。
pub fn reactivate_occurrences_by_artifact(
    conn: &Connection,
    artifact_id: i64,
) -> Result<u64, rusqlite::Error> {
    let deleted = conn.execute(
        "UPDATE artifact_occurrences SET status = 'active', stale_reason = '', updated_at = datetime('now')
         WHERE artifact_id = ?1 AND status = 'deleted' AND stale_reason = 'artifact_deleted'",
        params![artifact_id],
    )? as u64;
    let stale = conn.execute(
        "UPDATE artifact_occurrences SET status = 'active', stale_reason = '', updated_at = datetime('now')
         WHERE artifact_id = ?1 AND status = 'stale' AND stale_reason = 'artifact_deleted'",
        params![artifact_id],
    )? as u64;
    Ok(deleted + stale)
}

/// 软删除指定 artifact 的所有活跃 occurrence（delete 操作调用）
///
/// 将 status 改为 'deleted'，设置 stale_reason='artifact_deleted'，设置 updated_at。
/// P1-3 修复：写 stale_reason='artifact_deleted'，确保 restore 只恢复因 delete 而 stale 的 occurrence，
/// 不恢复因 detach 而标记的 occurrence（stale_reason='detached_by_user'）。
pub fn soft_delete_occurrences_by_artifact(
    conn: &Connection,
    artifact_id: i64,
) -> Result<u64, rusqlite::Error> {
    Ok(conn.execute(
        "UPDATE artifact_occurrences SET status = 'deleted', stale_reason = 'artifact_deleted', updated_at = datetime('now')
         WHERE artifact_id = ?1 AND status = 'active'",
        params![artifact_id],
    )? as u64)
}

/// 统计所有原件总数（含 deleted，不含 purged）
pub fn count_total_artifacts(conn: &Connection) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT COUNT(*) FROM source_artifacts WHERE purged_at IS NULL",
        [],
        |row| row.get(0),
    )
}
