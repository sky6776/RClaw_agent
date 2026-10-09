//! Embedding 模型升级/多索引管理 (P5-010~P5-014)
//!
//! 支持多 embedding index 并存、按维度分表、灰度评测。

use crate::error::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// Grouped library index: (model, dimensions, [(index_id, [library_ids])])
pub type LibraryIndexGroup = Vec<(String, i32, Vec<(i64, Vec<i64>)>)>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingIndex {
    pub id: i64,
    pub library_id: i64,
    pub provider: String,
    pub model: String,
    pub dimensions: i32,
    pub index_type: String,
    pub is_active: bool,
}

/// 创建 embedding index 记录，同时创建对应的 sqlite-vec 虚表
pub fn create_embedding_index(
    conn: &Connection,
    library_id: i64,
    provider: &str,
    model: &str,
    dimensions: i32,
    index_type: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO kb_embedding_indexes (library_id, provider, model, dimensions, index_type, is_active) \
         VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        params![library_id, provider, model, dimensions, index_type],
    )?;
    let index_id = conn.last_insert_rowid();

    // 创建专用 sqlite-vec 虚拟表（如果失败，后续写入会 fallback 到 BLOB 表）
    if let Err(e) = create_vec_table_for_index(conn, index_id, dimensions) {
        tracing::warn!(
            index_id,
            dimensions,
            error = %e,
            "创建 embedding index vec 虚拟表失败，将回退到 BLOB 存储；请确认 sqlite-vec 扩展已加载"
        );
    }

    Ok(index_id)
}

/// 列出 library 的所有 embedding index
pub fn list_embedding_indexes(conn: &Connection, library_id: i64) -> Result<Vec<EmbeddingIndex>> {
    let mut stmt = conn.prepare(
        "SELECT id, library_id, provider, model, dimensions, index_type, is_active \
         FROM kb_embedding_indexes WHERE library_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map(params![library_id], |row| {
        Ok(EmbeddingIndex {
            id: row.get(0)?,
            library_id: row.get(1)?,
            provider: row.get(2)?,
            model: row.get(3)?,
            dimensions: row.get(4)?,
            index_type: row.get(5)?,
            is_active: row.get::<_, i32>(6)? != 0,
        })
    })?;
    let results: Vec<EmbeddingIndex> = rows.filter_map(|r| r.ok()).collect();
    Ok(results)
}

/// 激活某个 embedding index（只影响同一 library 下的其他 index）
///
/// P3 修复: 切换 active index 后递增 index_version，使 retrieval cache 立即失效。
/// 缓存 key 依赖 MAX(index_version)，不递增会导致 30 秒内复用旧 active index 的候选集。
pub fn activate_index(conn: &Connection, index_id: i64) -> Result<()> {
    // 先查目标 index 所属的 library_id，限定 UPDATE 作用域
    let library_id: i64 = conn
        .query_row(
            "SELECT library_id FROM kb_embedding_indexes WHERE id = ?1",
            params![index_id],
            |row| row.get(0),
        )
        .map_err(|_| GBrainError::InvalidInput(format!("embedding index {} 不存在", index_id)))?;
    conn.execute(
        "UPDATE kb_embedding_indexes \
         SET is_active = CASE WHEN id = ?1 THEN 1 ELSE 0 END \
         WHERE library_id = ?2",
        params![index_id, library_id],
    )?;
    // P3 修复: 递增版本号，立即失效 retrieval cache
    if let Err(e) = increment_index_version(conn, "retrieval_cache") {
        tracing::warn!(
            index_id,
            error = %e,
            "激活 embedding index 后递增 index_version 失败，缓存可能返回过期结果"
        );
    }
    Ok(())
}

/// 删除 embedding index，同时删除对应的 sqlite-vec 虚表和关联的 kb_node_embeddings 行
///
/// P3 修复: 删除后递增 index_version，使 retrieval cache 立即失效。
pub fn delete_embedding_index(conn: &Connection, index_id: i64) -> Result<()> {
    // P5-012: Drop the per-index vec table
    let _ = drop_vec_table_for_index(conn, index_id);

    // Delete embeddings associated with this index from the BLOB fallback table
    conn.execute(
        "DELETE FROM kb_node_embeddings WHERE embedding_index_id = ?1",
        params![index_id],
    )?;

    // Delete the index record itself
    conn.execute(
        "DELETE FROM kb_embedding_indexes WHERE id = ?1",
        params![index_id],
    )?;

    // P3 修复: 递增版本号，立即失效 retrieval cache
    if let Err(e) = increment_index_version(conn, "retrieval_cache") {
        tracing::warn!(
            index_id,
            error = %e,
            "删除 embedding index 后递增 index_version 失败，缓存可能返回过期结果"
        );
    }
    Ok(())
}

/// 递增 index_state 中的版本号
pub fn increment_index_version(conn: &Connection, index_name: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO kb_index_state (index_name, index_version, index_type, state) \
         VALUES (?1, 1, 'vector', 'active') \
         ON CONFLICT(index_name) DO UPDATE SET index_version = index_version + 1, \
         last_rebuilt_at = datetime('now')",
        params![index_name],
    )?;
    let version: i64 = conn.query_row(
        "SELECT index_version FROM kb_index_state WHERE index_name = ?1",
        params![index_name],
        |row| row.get(0),
    )?;
    Ok(version)
}

// ---------------------------------------------------------------------------
// P5-013: Reembed job support
// ---------------------------------------------------------------------------

use crate::error::GBrainError;

/// Queue re-embed jobs for all documents in a library.
///
/// Creates a "kb_reembed" job for each document that has nodes but
/// no embeddings in the target index. If `target_index_id` is 0,
/// resolves to the library's active index.
pub fn queue_reembed_jobs(
    conn: &Connection,
    library_id: i64,
    target_index_id: i64,
) -> Result<usize> {
    // 解析 target_index_id：0 → active index
    let resolved_index_id = if target_index_id > 0 {
        target_index_id
    } else {
        get_active_index_for_library(conn, library_id)?
            .ok_or_else(|| {
                GBrainError::InvalidInput(format!(
                    "library {} 没有 active embedding index",
                    library_id
                ))
            })?
            .id
    };

    // 查找需要重新嵌入的文档：节点在目标 index 中没有 embedding
    let sql = "SELECT DISTINCT d.id FROM kb_documents d \
               JOIN kb_document_nodes n ON n.document_id = d.id \
               WHERE d.library_id = ?1 AND d.deleted_at IS NULL \
               AND n.id NOT IN (SELECT node_id FROM kb_node_embeddings WHERE embedding_index_id = ?2)";
    let doc_ids: Vec<i64> = {
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query(params![library_id, resolved_index_id])?;
        let mut ids = Vec::new();
        while let Ok(Some(row)) = rows.next() {
            ids.push(row.get::<_, i64>(0).unwrap_or(0));
        }
        ids
    };

    let mut queued = 0;
    for doc_id in &doc_ids {
        let payload = serde_json::json!({
            "kind": "kb_reembed",
            "document_id": doc_id,
            "library_id": library_id,
            "target_embedding_index_id": resolved_index_id,
        });
        conn.execute(
            "INSERT INTO jobs (job_type, payload, status, priority, max_attempts) \
             VALUES ('kb_reembed', ?1, 'pending', 0, 3)",
            params![payload.to_string()],
        )
        .map_err(|e| GBrainError::Database(e.to_string()))?;
        queued += 1;
    }
    Ok(queued)
}

// ---------------------------------------------------------------------------
// P5-012: 统一 embedding 写入 — 同时更新 kb_node_embeddings 和 vec_kb_{index}
// ---------------------------------------------------------------------------

/// 统一写入/替换节点的 embedding：同时更新 BLOB 表和 per-index sqlite-vec 虚表。
///
/// 调用者无需关心 vec 表是否存在或 sqlite-vec 是否加载 — 此函数自动处理。
/// 写入失败时返回错误，不再静默吞下。
pub fn upsert_node_embedding_for_index(
    conn: &Connection,
    node_id: i64,
    embedding_index_id: i64,
    embedding: &[f32],
    dimensions: i32,
    model: &str,
) -> Result<()> {
    let blob: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();

    // 1. 写入 BLOB fallback 表
    conn.execute(
        "INSERT OR REPLACE INTO kb_node_embeddings \
         (node_id, embedding_index_id, embedding, dimensions, model, embedded_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
        rusqlite::params![node_id, embedding_index_id, blob, dimensions, model],
    )?;

    // 2. 确保 per-index vec 虚表存在（失败时 fallback 到 BLOB 表）
    if let Err(e) = create_vec_table_for_index(conn, embedding_index_id, dimensions) {
        tracing::warn!(
            embedding_index_id,
            dimensions,
            error = %e,
            "写入嵌入时 vec 虚表创建失败，该索引将仅使用 BLOB 存储"
        );
    }

    // 3. 写入 per-index vec 虚表：先 DELETE 旧行再 INSERT
    //    vec0 虚表无主键/唯一约束，INSERT OR REPLACE 不会替换重复行，必须先删后插
    let vec_table = vec_table_name_for_index(embedding_index_id);
    // L1: 清理 vec 虚表旧行，失败时 warn 而非静默吞错
    if let Err(e) = conn.execute(
        &format!("DELETE FROM {} WHERE node_id = ?1", vec_table),
        rusqlite::params![node_id],
    ) {
        tracing::warn!(node_id, error = %e, "清理 vec 虚表旧行失败");
    }
    if let Err(e) = conn.execute(
        &format!(
            "INSERT INTO {} (node_id, embedding) VALUES (?1, ?2)",
            vec_table
        ),
        rusqlite::params![node_id, blob],
    ) {
        tracing::warn!(node_id, error = %e, "写入 vec 虚表失败");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// P5-012: sqlite-vec per-dimension/index table management
// ---------------------------------------------------------------------------

/// Generate the sqlite-vec virtual table name for an embedding index.
pub fn vec_table_name_for_index(index_id: i64) -> String {
    format!("vec_kb_{}", index_id)
}

/// Create a dedicated sqlite-vec virtual table for an embedding index.
///
/// 显式声明 `distance_metric=cosine`，确保 vec0 返回的距离为
/// cosine distance (= 1 - cosine_similarity)。sqlite-vec 默认使用 L2，
/// 不声明会导致 `1.0 - distance` 的 similarity 转换语义错误。
pub fn create_vec_table_for_index(conn: &Connection, index_id: i64, dimensions: i32) -> Result<()> {
    let table_name = vec_table_name_for_index(index_id);
    let sql = format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS {} USING vec0(\
         embedding float[{}] distance_metric=cosine, \
         node_id integer)",
        table_name, dimensions,
    );
    conn.execute_batch(&sql)
        .map_err(|e| GBrainError::Database(e.to_string()))?;
    Ok(())
}

/// Drop the sqlite-vec virtual table for an embedding index.
pub fn drop_vec_table_for_index(conn: &Connection, index_id: i64) -> Result<()> {
    let table_name = vec_table_name_for_index(index_id);
    let sql = format!("DROP TABLE IF EXISTS {}", table_name);
    conn.execute_batch(&sql)
        .map_err(|e| GBrainError::Database(e.to_string()))?;
    Ok(())
}

/// Get the active embedding index for a library.
/// Returns None if no active index exists.
pub fn get_active_index_for_library(
    conn: &Connection,
    library_id: i64,
) -> Result<Option<EmbeddingIndex>> {
    let result = conn.query_row(
        "SELECT id, library_id, provider, model, dimensions, index_type, is_active \
         FROM kb_embedding_indexes WHERE library_id = ?1 AND is_active = 1 LIMIT 1",
        params![library_id],
        |row| {
            Ok(EmbeddingIndex {
                id: row.get(0)?,
                library_id: row.get(1)?,
                provider: row.get(2)?,
                model: row.get(3)?,
                dimensions: row.get(4)?,
                index_type: row.get(5)?,
                is_active: row.get::<_, i32>(6)? != 0,
            })
        },
    );
    match result {
        Ok(idx) => Ok(Some(idx)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// P1 修复: 按 (model, dimensions) 对多个 library 的 active embedding index 分组。
///
/// 每个分组代表一组可以用同一模型/维度生成查询向量的库。不同 model 的向量不可互换，
/// 需要各自生成查询向量并分别检索各自的 vec 表后合并结果。
///
/// 返回 Vec<(model, dimensions, [(index_id, [使用此 index 的 library_ids])])>，
/// 无 library 或所有库都无 active index 时返回空 Vec。
pub fn group_libraries_by_active_index(
    conn: &Connection,
    library_ids: &[i64],
) -> Result<LibraryIndexGroup> {
    if library_ids.is_empty() {
        return Ok(Vec::new());
    }

    // 收集所有 (library_id, index_id, model, dimensions)
    let mut lib_indexes: Vec<(i64, i64, String, i32)> = Vec::new();
    for &lib_id in library_ids {
        match get_active_index_for_library(conn, lib_id)? {
            Some(idx) => {
                lib_indexes.push((lib_id, idx.id, idx.model, idx.dimensions));
            }
            None => {
                tracing::warn!(
                    library_id = lib_id,
                    "库没有 active embedding index，跳过该库的向量检索"
                );
            }
        }
    }

    if lib_indexes.is_empty() {
        return Ok(Vec::new());
    }

    // 验证所有 index 的维度一致性（不同维度无法合并结果）
    let first_dims = lib_indexes[0].3;
    for &(lib_id, _, _, dims) in &lib_indexes {
        if dims != first_dims {
            return Err(GBrainError::InvalidInput(format!(
                "库 {} 的 active embedding index 维度 ({}) 与其他库 ({}) 不一致，\
                 请确保所有查询库使用相同维度的 embedding index",
                lib_id, dims, first_dims
            )));
        }
    }

    // 按 (model, dimensions) 分组
    // 不同 model 即使同维度也分到不同组——向量不可互换
    let mut groups: LibraryIndexGroup = Vec::new();
    for (lib_id, idx_id, model, dims) in lib_indexes {
        if let Some(group) = groups
            .iter_mut()
            .find(|(m, d, _)| *m == model && *d == dims)
        {
            // 同一 model+dim 组内，按 index_id 聚合 library_ids
            if let Some(entry) = group.2.iter_mut().find(|(id, _)| *id == idx_id) {
                entry.1.push(lib_id);
            } else {
                group.2.push((idx_id, vec![lib_id]));
            }
        } else {
            groups.push((model, dims, vec![(idx_id, vec![lib_id])]));
        }
    }

    Ok(groups)
}

/// P1 修复: 从多个 library ID 解析共识 active embedding index。
///
/// 调用 group_libraries_by_active_index，若所有库的 active index 模型和维度一致，
/// 返回共识的 (id, model, dimensions)；否则返回错误。
/// 无 library 或任一库无 active index 时返回 None。
///
/// 注意：此函数用于需要单一 index 的旧调用路径。新代码应优先使用
/// group_libraries_by_active_index 以正确处理多模型/多 index 场景。
pub fn resolve_active_index_for_libraries(
    conn: &Connection,
    library_ids: &[i64],
) -> Result<Option<(i64, String, i32)>> {
    let groups = group_libraries_by_active_index(conn, library_ids)?;
    if groups.is_empty() {
        return Ok(None);
    }
    if groups.len() > 1 {
        return Err(GBrainError::InvalidInput(format!(
            "查询的多个库使用了不同的 embedding 模型 ({}、{})，\
             向量不可互换。请确保所有库使用相同的 embedding 模型，\
             或使用支持多模型分组检索的查询接口",
            groups[0].0, groups[1].0
        )));
    }
    // 单组：返回该组的 model/dims，以及第一个 index_id 作为代表
    let (model, dims, entries) = groups.into_iter().next().unwrap();
    let first_id = entries.first().map(|(id, _)| *id).unwrap_or(0);
    Ok(Some((first_id, model, dims)))
}

/// P1 修复: 从 kb_document_nodes 所属库解析 active embedding index。
///
/// 适用于已有 node_id 但尚未确定 library 的防御路径。
pub fn resolve_active_index_for_node(
    conn: &Connection,
    node_id: i64,
) -> Result<Option<(i64, String, i32)>> {
    let result = conn.query_row(
        "SELECT ei.id, ei.model, ei.dimensions \
         FROM kb_embedding_indexes ei \
         INNER JOIN kb_document_nodes dn ON dn.library_id = ei.library_id \
         WHERE dn.id = ?1 AND ei.is_active = 1 LIMIT 1",
        params![node_id],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i32>(2)?,
            ))
        },
    );
    match result {
        Ok(t) => Ok(Some(t)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// P1 修复: 获取所有拥有 active embedding index 的库 ID 列表。
///
/// 用于全库查询（library_ids=[]）时展开为所有有效库，避免走 legacy 路径
/// 扫描历史/非 active 向量。
pub fn all_library_ids_with_active_index(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt =
        conn.prepare("SELECT DISTINCT library_id FROM kb_embedding_indexes WHERE is_active = 1")?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
    let results: Vec<i64> = rows.filter_map(|r| r.ok()).collect();
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedding_index_struct() {
        let idx = EmbeddingIndex {
            id: 1,
            library_id: 1,
            provider: "openai".into(),
            model: "text-embedding-3-large".into(),
            dimensions: 1536,
            index_type: "vec0".into(),
            is_active: false,
        };
        assert_eq!(idx.dimensions, 1536);
        assert!(!idx.is_active);
    }

    #[test]
    fn test_vec_table_name_for_index() {
        assert_eq!(vec_table_name_for_index(1), "vec_kb_1");
        assert_eq!(vec_table_name_for_index(42), "vec_kb_42");
    }
}
