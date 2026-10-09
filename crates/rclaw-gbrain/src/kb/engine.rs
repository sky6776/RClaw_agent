//! KB Engine — database CRUD operations for the KB subsystem
//!
//! Shares the same SQLite connection as SqliteEngine.

use crate::error::{GBrainError, Result};
use crate::kb::types::*;
use rusqlite::{params, Connection, Row, ToSql, Transaction};

struct SqlUpdateBuilder {
    sets: Vec<String>,
    values: Vec<Box<dyn ToSql>>,
}

impl SqlUpdateBuilder {
    fn new() -> Self {
        Self {
            sets: Vec::new(),
            values: Vec::new(),
        }
    }

    fn push_set<V>(&mut self, column: &str, value: V)
    where
        V: ToSql + 'static,
    {
        let placeholder = self.push_param(value);
        self.sets.push(format!("{} = {}", column, placeholder));
    }

    fn push_param<V>(&mut self, value: V) -> String
    where
        V: ToSql + 'static,
    {
        self.values.push(Box::new(value));
        format!("?{}", self.values.len())
    }

    fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }

    fn set_clause(&self) -> String {
        self.sets.join(", ")
    }

    fn param_refs(&self) -> Vec<&dyn ToSql> {
        self.values.iter().map(|p| p.as_ref()).collect()
    }
}

fn row_to_document(row: &Row) -> std::result::Result<Document, rusqlite::Error> {
    Ok(Document {
        id: row.get(0)?,
        created_at: row.get(1)?,
        updated_at: row.get(2)?,
        library_id: row.get(3)?,
        folder_id: row.get(4)?,
        original_name: row.get(5)?,
        name_tokens: row.get(6)?,
        file_size: row.get(7)?,
        content_hash: row.get(8)?,
        extension: row.get(9)?,
        mime_type: row.get(10)?,
        source_type: row.get(11)?,
        storage_path: row.get(12)?,
        original_path: row.get(13)?,
        job_id: row.get(14)?,
        processing_run_id: row.get(15)?,
        parsing_status: row.get(16)?,
        parsing_progress: row.get(17)?,
        parsing_error: row.get(18)?,
        embedding_status: row.get(19)?,
        embedding_progress: row.get(20)?,
        embedding_error: row.get(21)?,
        word_total: row.get(22)?,
        split_total: row.get(23)?,
        title: row.get(24)?,
        summary: row.get(25)?,
        keywords: row.get(26)?,
        entity_names: row.get(27)?,
        source_uri: row.get(28)?,
        modified_at: row.get(29)?,
        document_date: row.get(30)?,
        normalized_content_hash: row.get(31)?,
        simhash: row.get(32)?,
        document_family_id: row.get(33)?,
        version_label: row.get(34)?,
        document_granularity: row.get(35)?,
        content_char_count: row.get(36)?,
        content_token_count: row.get(37)?,
        page_count: row.get(38)?,
        section_count: row.get(39)?,
        chunk_strategy: row.get(40)?,
        document_status: row.get(41)?,
        index_status: row.get(42)?,
        current_version_id: row.get(43)?,
        deleted_at: row.get(44)?,
        purged_at: row.get(45)?,
        last_indexed_at: row.get(46)?,
        last_seen_at: row.get(47)?,
        ocr_status: row.get(48)?,
        ocr_text_coverage: row.get(49)?,
    })
}

/// FIX11-02: 清理节点的向量数据（包括 per-index vec 表）
/// delete_library/delete_document/delete_document_nodes/purge_document 均需调用此函数，
/// 否则 vec_kb_nodes 和 vec_kb_{index_id} 虚表中的向量数据会残留，随时间累积影响搜索结果和磁盘空间。
///
/// L8: 当前逐节点单条 DELETE，批量删除场景（如 delete_library）会产生大量单条 SQL。
/// 未来可优化为批量 DELETE ... WHERE node_id IN (...) 或一次性清理整个 index 的 vec 表。
pub(crate) fn cleanup_node_vectors(conn: &Connection, node_id: i64) {
    // 清理 per-index vec 表（通过 kb_node_embeddings 反查 index_id）
    if let Ok(mut stmt) = conn
        .prepare("SELECT DISTINCT embedding_index_id FROM kb_node_embeddings WHERE node_id = ?1")
    {
        let index_ids: Vec<i64> = stmt
            .query_map(rusqlite::params![node_id], |row| row.get(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();
        for idx_id in index_ids {
            let vec_table = crate::kb::embedding_index::vec_table_name_for_index(idx_id);
            // FIX11-03: 验证表名格式防止 SQL 注入 — 表名必须匹配 vec_kb_{数字} 模式
            if vec_table.starts_with("vec_kb_")
                && vec_table[7..].chars().all(|c| c.is_ascii_digit())
            {
                if let Err(e) = conn.execute(
                    &format!("DELETE FROM {} WHERE node_id = ?1", vec_table),
                    rusqlite::params![node_id],
                ) {
                    tracing::warn!("删除 {} node_id={} 失败: {}", vec_table, node_id, e);
                }
            }
        }
    }
    // 清理 legacy vec 表
    if let Err(e) = conn.execute(
        "DELETE FROM vec_kb_nodes WHERE node_id = ?1",
        params![node_id],
    ) {
        tracing::warn!("删除 vec_kb_nodes node_id={} 失败: {}", node_id, e);
    }
    // 清理 kb_node_embeddings
    if let Err(e) = conn.execute(
        "DELETE FROM kb_node_embeddings WHERE node_id = ?1",
        params![node_id],
    ) {
        tracing::warn!("删除 kb_node_embeddings node_id={} 失败: {}", node_id, e);
    }
}

pub struct KbEngine<'a> {
    conn: &'a Connection,
}

impl<'a> KbEngine<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Execute in a transaction (RAII: auto-rollback on Drop if not committed)
    ///
    /// H6 fix: 移除手动 `tx.rollback()`，依赖 Drop guard 自动回滚，
    /// 避免手动回滚 + Drop 回滚的双重回滚问题。
    ///
    pub fn transaction<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Transaction<'_>) -> Result<T>,
    {
        let tx = self.conn.unchecked_transaction()?;
        let result = f(&tx);
        if result.is_ok() {
            tx.commit()?;
        }
        // tx Drop: if not committed, auto-rollback via Drop guard
        result
    }

    /// Read-only query
    pub fn query<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        f(self.conn)
    }

    // --- Library CRUD ---

    pub fn list_libraries(&self) -> Result<Vec<Library>> {
        self.query(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, created_at, updated_at, name, \
                        raptor_enabled, \
                        raptor_llm_base_url, raptor_llm_secret_ref, raptor_llm_model, \
                        chunk_size, chunk_overlap, batch_max_documents, batch_max_chunks, \
                        sort_order, \
                        embedding_provider, embedding_model, embedding_dimensions, \
                        search_profile, rerank_enabled, rerank_provider, \
                        title_weight, augmentation_enabled \
                 FROM kb_libraries ORDER BY sort_order DESC, id DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(Library {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    updated_at: row.get(2)?,
                    name: row.get(3)?,
                    raptor_enabled: row.get::<_, i32>(4)? != 0,
                    raptor_llm_base_url: row.get(5)?,
                    raptor_llm_secret_ref: row.get(6)?,
                    raptor_llm_model: row.get(7)?,
                    chunk_size: row.get::<_, i32>(8)? as usize,
                    chunk_overlap: row.get::<_, i32>(9)? as usize,
                    batch_max_documents: row.get::<_, i32>(10)? as usize,
                    batch_max_chunks: row.get::<_, i32>(11)? as usize,
                    sort_order: row.get(12)?,
                    embedding_provider: row.get(13)?,
                    embedding_model: row.get(14)?,
                    embedding_dimensions: row.get(15)?,
                    search_profile: row.get(16)?,
                    rerank_enabled: row.get::<_, i32>(17)? != 0,
                    rerank_provider: row.get(18)?,
                    title_weight: row.get::<_, f32>(19)?,
                    augmentation_enabled: row.get::<_, i32>(20)? != 0,
                })
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| GBrainError::Database(e.to_string()))
        })
    }

    /// List libraries with document_count and chunk_count stats.
    pub fn list_libraries_with_stats(&self) -> Result<Vec<LibraryListItem>> {
        self.query(|conn| {
            let mut stmt = conn.prepare(
                "SELECT l.id, l.name, l.sort_order, l.raptor_enabled, \
                        l.raptor_llm_secret_ref, \
                        COALESCE(d.doc_count, 0), COALESCE(n.chunk_count, 0) \
                 FROM kb_libraries l \
                 LEFT JOIN (SELECT library_id, COUNT(*) as doc_count FROM kb_documents GROUP BY library_id) d \
                    ON l.id = d.library_id \
                 LEFT JOIN (SELECT library_id, COUNT(*) as chunk_count FROM kb_document_nodes GROUP BY library_id) n \
                    ON l.id = n.library_id \
                 ORDER BY l.sort_order DESC, l.id DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(LibraryListItem {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    document_count: row.get(5)?,
                    chunk_count: row.get(6)?,
                    sort_order: row.get(2)?,
                    raptor_enabled: row.get::<_, i32>(3)? != 0,
                    has_raptor_secret: !row.get::<_, String>(4)?.is_empty(),
                })
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| GBrainError::Database(e.to_string()))
        })
    }

    pub fn create_library(&self, input: &CreateLibraryInput) -> Result<i64> {
        self.transaction(|conn| {
            let chunk_size = input.chunk_size.unwrap_or(512).clamp(200, 5000) as i32;
            let chunk_overlap = input.chunk_overlap.unwrap_or(50).clamp(0, 1000) as i32;
            let batch_max_docs = input.batch_max_documents.unwrap_or(3).clamp(1, 5) as i32;
            let batch_max_chunks = input.batch_max_chunks.unwrap_or(10).clamp(1, 20) as i32;
            let raptor_enabled = input.raptor_enabled.unwrap_or(true) as i32;

            // Get next sort_order from MAX+1
            let max_sort: i32 = conn
                .query_row(
                    "SELECT COALESCE(MAX(sort_order), 0) FROM kb_libraries",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);

            conn.execute(
                "INSERT INTO kb_libraries \
                 (name, raptor_enabled, \
                  raptor_llm_base_url, raptor_llm_secret_ref, raptor_llm_model, \
                  chunk_size, chunk_overlap, batch_max_documents, batch_max_chunks, sort_order, \
                  embedding_provider, embedding_model, embedding_dimensions, \
                  search_profile, rerank_enabled, rerank_provider, \
                  title_weight, augmentation_enabled) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                params![
                    input.name,
                    raptor_enabled,
                    input.raptor_llm_base_url.as_deref().unwrap_or(""),
                    input.raptor_llm_secret_ref.as_deref().unwrap_or(""),
                    input.raptor_llm_model.as_deref().unwrap_or(""),
                    chunk_size,
                    chunk_overlap,
                    batch_max_docs,
                    batch_max_chunks,
                    max_sort + 1,
                    input.embedding_provider.as_deref().unwrap_or("openai"),
                    input.embedding_model.as_deref().unwrap_or("text-embedding-3-large"),
                    input.embedding_dimensions.unwrap_or(1536),
                    input.search_profile.as_deref().unwrap_or("balanced"),
                    input.rerank_enabled.unwrap_or(true) as i32,
                    input.rerank_provider.as_deref().unwrap_or(""),
                    input.title_weight.unwrap_or(0.2).clamp(0.0, 1.0),
                    input.augmentation_enabled.unwrap_or(true) as i32,
                ],
            )?;
            let lib_id = conn.last_insert_rowid();

            // 自动创建默认 embedding index 并设为 active
            let dims = input.embedding_dimensions.unwrap_or(1536);
            let provider = input.embedding_provider.as_deref().unwrap_or("openai");
            let model = input
                .embedding_model
                .as_deref()
                .unwrap_or("text-embedding-3-large");
            let index_id = crate::kb::embedding_index::create_embedding_index(
                conn, lib_id, provider, model, dims, "vec0",
            )?;
            crate::kb::embedding_index::activate_index(conn, index_id)?;

            Ok(lib_id)
        })
    }

    pub fn get_library(&self, id: i64) -> Result<Library> {
        self.query(|conn| {
            conn.query_row(
                "SELECT id, created_at, updated_at, name, \
                        raptor_enabled, \
                        raptor_llm_base_url, raptor_llm_secret_ref, raptor_llm_model, \
                        chunk_size, chunk_overlap, batch_max_documents, batch_max_chunks, \
                        sort_order, \
                        embedding_provider, embedding_model, embedding_dimensions, \
                        search_profile, rerank_enabled, rerank_provider, \
                        title_weight, augmentation_enabled \
                 FROM kb_libraries WHERE id = ?1",
                [id],
                |row| {
                    Ok(Library {
                        id: row.get(0)?,
                        created_at: row.get(1)?,
                        updated_at: row.get(2)?,
                        name: row.get(3)?,
                        raptor_enabled: row.get::<_, i32>(4)? != 0,
                        raptor_llm_base_url: row.get(5)?,
                        raptor_llm_secret_ref: row.get(6)?,
                        raptor_llm_model: row.get(7)?,
                        chunk_size: row.get::<_, i32>(8)? as usize,
                        chunk_overlap: row.get::<_, i32>(9)? as usize,
                        batch_max_documents: row.get::<_, i32>(10)? as usize,
                        batch_max_chunks: row.get::<_, i32>(11)? as usize,
                        sort_order: row.get(12)?,
                        embedding_provider: row.get(13)?,
                        embedding_model: row.get(14)?,
                        embedding_dimensions: row.get(15)?,
                        search_profile: row.get(16)?,
                        rerank_enabled: row.get::<_, i32>(17)? != 0,
                        rerank_provider: row.get(18)?,
                        title_weight: row.get::<_, f32>(19)?,
                        augmentation_enabled: row.get::<_, i32>(20)? != 0,
                    })
                },
            )
            .map_err(|e| GBrainError::Database(format!("Library not found: {}", e)))
        })
    }

    pub fn update_library(&self, id: i64, input: &UpdateLibraryInput) -> Result<()> {
        self.transaction(|conn| {
            let mut update = SqlUpdateBuilder::new();

            if let Some(ref name) = input.name {
                update.push_set("name", name.clone());
            }
            if let Some(raptor) = input.raptor_enabled {
                update.push_set("raptor_enabled", raptor as i32);
            }
            if let Some(ref url) = input.raptor_llm_base_url {
                update.push_set("raptor_llm_base_url", url.clone());
            }
            if let Some(ref secret) = input.raptor_llm_secret_ref {
                update.push_set("raptor_llm_secret_ref", secret.clone());
            }
            if let Some(ref model) = input.raptor_llm_model {
                update.push_set("raptor_llm_model", model.clone());
            }
            if let Some(chunk_size) = input.chunk_size {
                update.push_set("chunk_size", chunk_size.clamp(200, 5000) as i32);
            }
            if let Some(chunk_overlap) = input.chunk_overlap {
                update.push_set("chunk_overlap", chunk_overlap.clamp(0, 1000) as i32);
            }
            // P0-016: 库级治理字段更新
            if let Some(ref v) = input.embedding_provider {
                update.push_set("embedding_provider", v.clone());
            }
            if let Some(ref v) = input.embedding_model {
                update.push_set("embedding_model", v.clone());
            }
            if let Some(v) = input.embedding_dimensions {
                update.push_set("embedding_dimensions", v);
            }
            if let Some(ref v) = input.search_profile {
                update.push_set("search_profile", v.clone());
            }
            if let Some(v) = input.rerank_enabled {
                update.push_set("rerank_enabled", v as i32);
            }
            if let Some(ref v) = input.rerank_provider {
                update.push_set("rerank_provider", v.clone());
            }
            if let Some(v) = input.title_weight {
                update.push_set("title_weight", v.clamp(0.0, 1.0));
            }
            if let Some(v) = input.augmentation_enabled {
                update.push_set("augmentation_enabled", v as i32);
            }

            if update.is_empty() {
                return Ok(());
            }

            update.sets.push("updated_at = datetime('now')".to_string());
            let id_placeholder = update.push_param(id);

            let sql = format!(
                "UPDATE kb_libraries SET {} WHERE id = {}",
                update.set_clause(),
                id_placeholder
            );
            let param_refs = update.param_refs();

            conn.execute(&sql, param_refs.as_slice())?;
            Ok(())
        })
    }

    pub fn delete_library(&self, id: i64) -> Result<()> {
        self.transaction(|conn| {
            // FIX11-02: 获取节点 ID 用于向量清理（包括 per-index vec 表）
            let node_ids: Vec<i64> = {
                let mut stmt =
                    conn.prepare("SELECT id FROM kb_document_nodes WHERE library_id = ?1")?;
                let rows = stmt.query_map([id], |row| row.get(0))?;
                rows.filter_map(|r| r.ok()).collect()
            };

            // FIX12-01: 先清理节点向量数据（DELETE 行），再 drop 整个 vec 虚表。
            // 如果先 drop 再 cleanup，cleanup 会尝试 DELETE FROM 已 drop 的表，产生 "no such table" warn。
            for &node_id in &node_ids {
                cleanup_node_vectors(conn, node_id);
            }

            // 收集该 library 的 embedding index ids 并 drop 对应的 vec 虚表。
            // FK cascade 删除 kb_embedding_indexes 后就失去 index_id 信息，
            // vec_kb_{index_id} 虚表会成为孤立表，所以必须在 cascade 前先 drop。
            let index_ids: Vec<i64> = {
                let mut stmt =
                    conn.prepare("SELECT id FROM kb_embedding_indexes WHERE library_id = ?1")?;
                let rows = stmt.query_map([id], |row| row.get(0))?;
                rows.filter_map(|r| r.ok()).collect()
            };
            for &idx_id in &index_ids {
                if let Err(e) = crate::kb::embedding_index::drop_vec_table_for_index(conn, idx_id) {
                    tracing::warn!("drop vec_kb_{} 虚表失败: {}", idx_id, e);
                }
            }

            // Delete library (CASCADE handles folders, documents, document_nodes, embedding_indexes)
            conn.execute("DELETE FROM kb_libraries WHERE id = ?1", [id])?;
            Ok(())
        })
    }

    // --- Document CRUD ---

    pub fn create_document(&self, doc: &Document) -> Result<i64> {
        self.transaction(|conn| {
            // 新文档刚创建时只代表“已进入 KB 队列”，还没有经过 PDF 文本层检测、
            // 图片 OCR 能力判断或普通文档解析流程。这里必须显式写入 not_evaluated，
            // 避免用户在 worker 尚未处理前查询状态时，把默认值误解为“已判定不需要 OCR”。
            conn.execute(
                "INSERT INTO kb_documents \
                 (library_id, folder_id, original_name, name_tokens, \
                  file_size, content_hash, extension, mime_type, \
                  source_type, storage_path, original_path, job_id, processing_run_id, ocr_status) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    doc.library_id,
                    doc.folder_id,
                    doc.original_name,
                    doc.name_tokens,
                    doc.file_size,
                    doc.content_hash,
                    doc.extension,
                    doc.mime_type,
                    doc.source_type,
                    doc.storage_path,
                    doc.original_path,
                    doc.job_id,
                    doc.processing_run_id,
                    crate::kb::ocr::OcrStatus::NotEvaluated.as_str(),
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn get_document(&self, id: i64) -> Result<Document> {
        self.query(|conn| {
            conn.query_row(
                "SELECT id, created_at, updated_at, library_id, folder_id, \
                        original_name, name_tokens, file_size, content_hash, \
                        extension, mime_type, source_type, storage_path, original_path, \
                        job_id, processing_run_id, \
                        parsing_status, parsing_progress, parsing_error, \
                        embedding_status, embedding_progress, embedding_error, \
                        word_total, split_total, \
                        title, summary, keywords, entity_names, source_uri, \
                        modified_at, document_date, normalized_content_hash, simhash, \
                        document_family_id, version_label, document_granularity, \
                        content_char_count, content_token_count, page_count, section_count, \
                        chunk_strategy, document_status, index_status, current_version_id, \
                        deleted_at, purged_at, last_indexed_at, last_seen_at, \
                        ocr_status, ocr_text_coverage \
                 FROM kb_documents WHERE id = ?1",
                [id],
                row_to_document,
            )
            .map_err(|e| GBrainError::Database(format!("Document not found: {}", e)))
        })
    }

    pub fn list_documents(
        &self,
        library_id: i64,
        folder_id: Option<i64>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<DocumentListItem>> {
        self.query(|conn| {
            // 修复：排除已软删除的文档，与搜索路径保持一致
            let sql = if folder_id.is_some() {
                "SELECT id, original_name, extension, file_size, \
                        parsing_status, parsing_progress, embedding_status, embedding_progress, \
                        job_id, folder_id, updated_at, \
                        title, document_granularity \
                 FROM kb_documents WHERE library_id = ?1 AND folder_id = ?2 \
                 AND deleted_at IS NULL AND document_status != 'deleted' \
                 ORDER BY updated_at DESC LIMIT ?3 OFFSET ?4"
            } else {
                "SELECT id, original_name, extension, file_size, \
                        parsing_status, parsing_progress, embedding_status, embedding_progress, \
                        job_id, folder_id, updated_at, \
                        title, document_granularity \
                 FROM kb_documents WHERE library_id = ?1 \
                 AND deleted_at IS NULL AND document_status != 'deleted' \
                 ORDER BY updated_at DESC LIMIT ?3 OFFSET ?4"
            };

            let mut stmt = conn.prepare(sql)?;

            // Use a single closure to map rows, avoiding the if/else closure-type mismatch
            let map_row =
                |row: &rusqlite::Row| -> std::result::Result<DocumentListItem, rusqlite::Error> {
                    Ok(DocumentListItem {
                        id: row.get(0)?,
                        original_name: row.get(1)?,
                        extension: row.get(2)?,
                        file_size: row.get(3)?,
                        parsing_status: row.get(4)?,
                        parsing_progress: row.get(5)?,
                        embedding_status: row.get(6)?,
                        embedding_progress: row.get(7)?,
                        job_id: row.get(8)?,
                        folder_id: row.get(9)?,
                        updated_at: row.get(10)?,
                        title: row.get(11)?,
                        document_granularity: row.get(12)?,
                    })
                };

            let rows: Vec<DocumentListItem> = if let Some(fid) = folder_id {
                stmt.query_map(
                    params![library_id, fid, limit as i64, offset as i64],
                    map_row,
                )?
                .filter_map(|r| r.ok())
                .collect()
            } else {
                stmt.query_map(params![library_id, limit as i64, offset as i64], map_row)?
                    .filter_map(|r| r.ok())
                    .collect()
            };

            Ok(rows)
        })
    }

    pub fn find_document_by_hash(
        &self,
        library_id: i64,
        content_hash: &str,
    ) -> Result<Option<Document>> {
        self.query(|conn| {
            let result = conn.query_row(
                "SELECT id, created_at, updated_at, library_id, folder_id, \
                        original_name, name_tokens, file_size, content_hash, \
                        extension, mime_type, source_type, storage_path, original_path, \
                        job_id, processing_run_id, \
                        parsing_status, parsing_progress, parsing_error, \
                        embedding_status, embedding_progress, embedding_error, \
                        word_total, split_total, \
                        title, summary, keywords, entity_names, source_uri, \
                        modified_at, document_date, normalized_content_hash, simhash, \
                        document_family_id, version_label, document_granularity, \
                        content_char_count, content_token_count, page_count, section_count, \
                        chunk_strategy, document_status, index_status, current_version_id, \
                        deleted_at, purged_at, last_indexed_at, last_seen_at, \
                        ocr_status, ocr_text_coverage \
                 FROM kb_documents WHERE library_id = ?1 AND content_hash = ?2 \
                 AND deleted_at IS NULL AND purged_at IS NULL",
                params![library_id, content_hash],
                row_to_document,
            );
            match result {
                Ok(doc) => Ok(Some(doc)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(GBrainError::Database(e.to_string())),
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_document_status(
        &self,
        id: i64,
        parsing_status: Option<i32>,
        parsing_progress: Option<i32>,
        parsing_error: Option<&str>,
        embedding_status: Option<i32>,
        embedding_progress: Option<i32>,
        embedding_error: Option<&str>,
    ) -> Result<()> {
        self.update_document_status_with_run_guard(
            id,
            parsing_status,
            parsing_progress,
            parsing_error,
            embedding_status,
            embedding_progress,
            embedding_error,
            None,
        )
    }

    /// 修复：带 run_id 守卫的 update_document_status。
    /// 如果提供了 run_id，UPDATE 语句增加 `AND processing_run_id = ?` 条件，
    /// 确保只有当前 run 的文档状态才会被更新。如果 run_id 不匹配（0 行受影响），
    /// 静默忽略（中间状态更新失败不应中断管道），仅记录警告。
    /// 中间状态（进度/错误）丢失不影响最终一致性，最终 stats 有严格守卫。
    #[allow(clippy::too_many_arguments)]
    pub fn update_document_status_with_run_guard(
        &self,
        id: i64,
        parsing_status: Option<i32>,
        parsing_progress: Option<i32>,
        parsing_error: Option<&str>,
        embedding_status: Option<i32>,
        embedding_progress: Option<i32>,
        embedding_error: Option<&str>,
        run_id: Option<&str>,
    ) -> Result<()> {
        self.transaction(|conn| {
            let mut update = SqlUpdateBuilder::new();

            if let Some(s) = parsing_status {
                update.push_set("parsing_status", s);
            }
            if let Some(p) = parsing_progress {
                update.push_set("parsing_progress", p);
            }
            if let Some(e) = parsing_error {
                update.push_set("parsing_error", e.to_string());
            }
            if let Some(s) = embedding_status {
                update.push_set("embedding_status", s);
            }
            if let Some(p) = embedding_progress {
                update.push_set("embedding_progress", p);
            }
            if let Some(e) = embedding_error {
                update.push_set("embedding_error", e.to_string());
            }

            if update.is_empty() {
                return Ok(());
            }

            update.sets.push("updated_at = datetime('now')".to_string());
            let id_placeholder = update.push_param(id);

            let sql = if let Some(rid) = run_id {
                // 修复：增加 processing_run_id WHERE 条件，防止旧 run 污染中间状态
                let run_id_placeholder = update.push_param(rid.to_string());
                format!(
                    "UPDATE kb_documents SET {} WHERE id = {} AND processing_run_id = {}",
                    update.set_clause(),
                    id_placeholder,
                    run_id_placeholder
                )
            } else {
                format!(
                    "UPDATE kb_documents SET {} WHERE id = {}",
                    update.set_clause(),
                    id_placeholder
                )
            };

            let param_refs = update.param_refs();

            let rows = conn.execute(&sql, param_refs.as_slice())?;
            if run_id.is_some() && rows == 0 {
                tracing::warn!(
                    id,
                    "update_document_status: run_id 不匹配，跳过中间状态更新（stale job）"
                );
            }
            Ok(())
        })
    }

    /// 更新文档的 job_id
    pub fn update_document_job_id(&self, id: i64, job_id: &str) -> Result<()> {
        self.transaction(|conn| {
            conn.execute(
                "UPDATE kb_documents SET job_id = ?1, updated_at = datetime('now') WHERE id = ?2",
                params![job_id, id],
            )?;
            Ok(())
        })
    }

    /// 更新文档的 processing_run_id
    pub fn update_document_run_id(&self, id: i64, processing_run_id: &str) -> Result<()> {
        self.transaction(|conn| {
            conn.execute(
                "UPDATE kb_documents SET processing_run_id = ?1, updated_at = datetime('now') WHERE id = ?2",
                params![processing_run_id, id],
            )?;
            Ok(())
        })
    }

    /// 更新文档的 content_hash/file_size/storage_path，用于 Changed 场景同步源文件元数据
    pub fn update_document_source_metadata(
        &self,
        id: i64,
        content_hash: &str,
        file_size: i64,
        storage_path: &str,
    ) -> Result<()> {
        self.transaction(|conn| {
            conn.execute(
                "UPDATE kb_documents SET content_hash = ?1, file_size = ?2, storage_path = ?3, \
                 updated_at = datetime('now') WHERE id = ?4",
                params![content_hash, file_size, storage_path, id],
            )?;
            Ok(())
        })
    }

    /// 重置文档处理状态为 queued/pending，用于 Changed 场景重新处理
    pub fn reset_document_processing(&self, id: i64) -> Result<()> {
        self.transaction(|conn| {
            // 内容变更后会重新进入完整解析流程。旧 OCR 结论已经不再可信，
            // 因此同时清空覆盖率并回到 not_evaluated，等待新的 worker run 重新判定。
            conn.execute(
                "UPDATE kb_documents SET document_status = 'queued', index_status = 'pending', \
                 parsing_status = 0, parsing_progress = 0, parsing_error = '', \
                 embedding_status = 0, embedding_progress = 0, embedding_error = '', \
                 ocr_status = 'not_evaluated', ocr_text_coverage = 0.0, \
                 updated_at = datetime('now') WHERE id = ?1",
                params![id],
            )?;
            Ok(())
        })
    }

    pub fn update_document_stats(
        &self,
        id: i64,
        word_total: i32,
        split_total: i32,
        embedding_status: Option<i32>,
    ) -> Result<()> {
        self.update_document_stats_with_run_guard(
            id,
            word_total,
            split_total,
            embedding_status,
            None,
        )
    }

    /// 内部实现：在调用者已有事务的情况下直接执行条件 UPDATE，不再开事务。
    /// 修复：SQLite 不支持嵌套 BEGIN，pipeline.rs 外层已 unchecked_transaction，
    /// 内层不能再通过 self.transaction() 开新事务，否则收尾阶段会失败。
    /// 此函数供外层事务内直接调用，避免嵌套事务。
    pub fn update_document_stats_with_run_guard_inner(
        &self,
        id: i64,
        word_total: i32,
        split_total: i32,
        embedding_status: Option<i32>,
        run_id: Option<&str>,
    ) -> Result<()> {
        let emb_st = embedding_status.unwrap_or(STATUS_COMPLETED);
        let (doc_status, idx_status, set_last_indexed) = if emb_st == STATUS_COMPLETED {
            ("ready", "ready", true)
        } else if emb_st == STATUS_FAILED {
            ("failed", "failed", false)
        } else if emb_st == STATUS_SKIPPED {
            ("ready", "keyword_only", true)
        } else {
            ("processing", "pending", false)
        };
        // 修复：直接在已有事务的 conn 上执行 UPDATE，不再开新事务
        // 到达统计更新阶段，说明文档已经完成了解析流程的关键判断。
        // 如果 OCR 状态仍停留在初始的 not_evaluated，表示该文档没有进入 PDF/图片 OCR 分支，
        // 可以安全推进为 not_needed；若 OCR 分支已写入 queued/done/needed/failed 等真实状态，
        // CASE 表达式会保留这些更精确的结果，避免被普通统计更新覆盖。
        let conn = self.conn;
        let run_guard = run_id.is_some();
        if set_last_indexed {
            let rows = if let Some(rid) = run_id {
                conn.execute(
                    "UPDATE kb_documents SET word_total = ?1, split_total = ?2, \
                     parsing_status = ?3, embedding_status = ?4, \
                     document_status = ?5, index_status = ?6, \
                     ocr_status = CASE WHEN ocr_status = 'not_evaluated' THEN 'not_needed' ELSE ocr_status END, \
                     last_indexed_at = datetime('now'), updated_at = datetime('now') \
                     WHERE id = ?7 AND processing_run_id = ?8",
                    params![
                        word_total,
                        split_total,
                        STATUS_COMPLETED,
                        emb_st,
                        doc_status,
                        idx_status,
                        id,
                        rid
                    ],
                )?
            } else {
                conn.execute(
                    "UPDATE kb_documents SET word_total = ?1, split_total = ?2, \
                     parsing_status = ?3, embedding_status = ?4, \
                     document_status = ?5, index_status = ?6, \
                     ocr_status = CASE WHEN ocr_status = 'not_evaluated' THEN 'not_needed' ELSE ocr_status END, \
                     last_indexed_at = datetime('now'), updated_at = datetime('now') \
                     WHERE id = ?7",
                    params![
                        word_total,
                        split_total,
                        STATUS_COMPLETED,
                        emb_st,
                        doc_status,
                        idx_status,
                        id
                    ],
                )?
            };
            if run_guard && rows == 0 {
                return Err(GBrainError::InvalidInput(
                    "stale KB processing job; document has a newer run (update_document_stats)"
                        .to_string(),
                ));
            }
        } else {
            let rows = if let Some(rid) = run_id {
                conn.execute(
                    "UPDATE kb_documents SET word_total = ?1, split_total = ?2, \
                     parsing_status = ?3, embedding_status = ?4, \
                     document_status = ?5, index_status = ?6, \
                     ocr_status = CASE WHEN ocr_status = 'not_evaluated' THEN 'not_needed' ELSE ocr_status END, \
                     updated_at = datetime('now') \
                     WHERE id = ?7 AND processing_run_id = ?8",
                    params![
                        word_total,
                        split_total,
                        STATUS_COMPLETED,
                        emb_st,
                        doc_status,
                        idx_status,
                        id,
                        rid
                    ],
                )?
            } else {
                conn.execute(
                    "UPDATE kb_documents SET word_total = ?1, split_total = ?2, \
                     parsing_status = ?3, embedding_status = ?4, \
                     document_status = ?5, index_status = ?6, \
                     ocr_status = CASE WHEN ocr_status = 'not_evaluated' THEN 'not_needed' ELSE ocr_status END, \
                     updated_at = datetime('now') \
                     WHERE id = ?7",
                    params![
                        word_total,
                        split_total,
                        STATUS_COMPLETED,
                        emb_st,
                        doc_status,
                        idx_status,
                        id
                    ],
                )?
            };
            if run_guard && rows == 0 {
                return Err(GBrainError::InvalidInput(
                    "stale KB processing job; document has a newer run (update_document_stats)"
                        .to_string(),
                ));
            }
        }
        Ok(())
    }

    /// 带事务的 update_document_stats_with_run_guard（自带事务）。
    /// 若调用方已持有事务，请改用 `update_document_stats_with_run_guard_inner` 以避免嵌套事务。
    pub fn update_document_stats_with_run_guard(
        &self,
        id: i64,
        word_total: i32,
        split_total: i32,
        embedding_status: Option<i32>,
        run_id: Option<&str>,
    ) -> Result<()> {
        self.transaction(|_| {
            self.update_document_stats_with_run_guard_inner(
                id,
                word_total,
                split_total,
                embedding_status,
                run_id,
            )
        })
    }

    pub fn delete_document(&self, id: i64) -> Result<()> {
        let doc = self.get_document(id)?;
        let storage_path = doc.storage_path;
        let source_type = doc.source_type;

        self.transaction(|conn| {
            // FIX11-02: 获取节点 ID 用于向量清理（包括 per-index vec 表）
            let node_ids: Vec<i64> = {
                let mut stmt =
                    conn.prepare("SELECT id FROM kb_document_nodes WHERE document_id = ?1")?;
                let rows = stmt.query_map([id], |row| row.get(0))?;
                rows.filter_map(|r| r.ok()).collect()
            };

            // 清理所有节点的向量数据（vec_kb_nodes + per-index vec 表 + kb_node_embeddings）
            for &node_id in &node_ids {
                cleanup_node_vectors(conn, node_id);
            }

            // Delete document (CASCADE handles document_nodes)
            conn.execute("DELETE FROM kb_documents WHERE id = ?1", [id])?;

            Ok(())
        })?;

        // 仅对 upload 类型删除物理文件；ingest 类型的 storage_path 是用户原始文件，不应删除
        if !storage_path.is_empty() && source_type == "upload" {
            let path = std::path::Path::new(&storage_path);
            if path.exists() {
                if let Err(e) = std::fs::remove_file(path) {
                    tracing::warn!("删除存储文件失败 {}: {}", storage_path, e);
                }
            }
        }

        Ok(())
    }

    // --- DocumentNode operations ---

    pub fn get_document_nodes(&self, document_id: i64) -> Result<Vec<DocumentNode>> {
        self.query(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, created_at, updated_at, library_id, document_id, \
                        content, content_tokens, level, parent_id, chunk_order \
                 FROM kb_document_nodes \
                 WHERE document_id = ?1 \
                 ORDER BY level ASC, chunk_order ASC",
            )?;
            let rows = stmt.query_map([document_id], |row| {
                Ok(DocumentNode {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    updated_at: row.get(2)?,
                    library_id: row.get(3)?,
                    document_id: row.get(4)?,
                    content: row.get(5)?,
                    content_tokens: row.get(6)?,
                    level: row.get(7)?,
                    parent_id: row.get(8)?,
                    chunk_order: row.get(9)?,
                    // P0-011: new node metadata fields
                    section_id: None,
                    title_path: String::new(),
                    page_number: None,
                    source_start: None,
                    source_end: None,
                    node_metadata: String::new(),
                    embedding_text: String::new(),
                })
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| GBrainError::Database(e.to_string()))
        })
    }

    pub fn delete_document_nodes(&self, document_id: i64) -> Result<()> {
        self.transaction(|conn| {
            // FIX11-02: 获取节点 ID 用于向量清理（包括 per-index vec 表）
            let node_ids: Vec<i64> = {
                let mut stmt =
                    conn.prepare("SELECT id FROM kb_document_nodes WHERE document_id = ?1")?;
                let rows = stmt.query_map([document_id], |row| row.get(0))?;
                rows.filter_map(|r| r.ok()).collect()
            };

            // 清理所有节点的向量数据（vec_kb_nodes + per-index vec 表 + kb_node_embeddings）
            for &node_id in &node_ids {
                cleanup_node_vectors(conn, node_id);
            }

            // Delete nodes
            conn.execute(
                "DELETE FROM kb_document_nodes WHERE document_id = ?1",
                [document_id],
            )?;
            Ok(())
        })
    }

    pub fn count_document_nodes(&self, library_id: i64) -> Result<i64> {
        self.query(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM kb_document_nodes WHERE library_id = ?1",
                [library_id],
                |row| row.get(0),
            )
            .map_err(|e| GBrainError::Database(e.to_string()))
        })
    }

    pub fn count_documents(&self, library_id: i64) -> Result<i64> {
        self.query(|conn| {
            // 修复：排除已软删除的文档，与搜索路径保持一致
            conn.query_row(
                "SELECT COUNT(*) FROM kb_documents WHERE library_id = ?1 AND deleted_at IS NULL AND document_status != 'deleted'",
                [library_id],
                |row| row.get(0),
            )
            .map_err(|e| GBrainError::Database(e.to_string()))
        })
    }

    // --- Lifecycle helpers ---

    /// 软删除文档：设置 deleted_at 并将 document_status 设为 deleted
    pub fn soft_delete_document(&self, id: i64) -> Result<()> {
        self.transaction(|conn| crate::kb::lifecycle::soft_delete_document(conn, id))
    }

    /// 彻底清理已软删除的文档及其所有关联数据
    pub fn purge_document(&self, id: i64) -> Result<()> {
        crate::kb::lifecycle::purge_document(self, id)
    }

    /// 转换文档处理状态，带状态机合法性检查和可选的 run_id 守卫
    /// FIX11-01: 写操作必须用 transaction，不能用 query（query 是只读语义）
    pub fn transition_document_status(
        &self,
        id: i64,
        new_status: crate::kb::lifecycle::DocumentStatus,
        run_id: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<()> {
        self.transaction(|conn| {
            crate::kb::lifecycle::transition_document_status(
                conn,
                id,
                new_status,
                run_id,
                error_message,
            )
        })
    }

    /// 创建文档版本快照
    pub fn create_document_version(
        &self,
        document_id: i64,
        version_label: &str,
        processing_run_id: &str,
        char_count: i32,
        node_count: i32,
    ) -> Result<i64> {
        self.transaction(|conn| {
            crate::kb::lifecycle::create_document_version(
                conn,
                document_id,
                version_label,
                processing_run_id,
                char_count,
                node_count,
            )
        })
    }

    /// 写入文档元数据（title/author/keywords/entities/source/dates）
    #[allow(clippy::too_many_arguments)]
    pub fn update_document_metadata(
        &self,
        id: i64,
        title: &str,
        _author: &str,
        keywords: &str,
        entity_names: &str,
        source_uri: &str,
        document_date: Option<&str>,
        modified_at: Option<&str>,
    ) -> Result<()> {
        self.update_document_metadata_with_run_guard(
            id,
            title,
            _author,
            keywords,
            entity_names,
            source_uri,
            document_date,
            modified_at,
            None,
        )
    }

    /// 修复：带 run_id 守卫的 update_document_metadata。
    /// 如果提供了 run_id，UPDATE 语句增加 `AND processing_run_id = ?` 条件，
    /// 防止旧 job 在过期后继续写元数据，污染新 run 的文档。
    #[allow(clippy::too_many_arguments)]
    pub fn update_document_metadata_with_run_guard(
        &self,
        id: i64,
        title: &str,
        _author: &str,
        keywords: &str,
        entity_names: &str,
        source_uri: &str,
        document_date: Option<&str>,
        modified_at: Option<&str>,
        run_id: Option<&str>,
    ) -> Result<()> {
        self.transaction(|conn| {
            let title_val = if !title.is_empty() { title } else { "" };
            if let Some(rid) = run_id {
                // 修复：增加 processing_run_id 条件，防止旧 run 污染元数据
                let rows = conn.execute(
                    "UPDATE kb_documents SET title = COALESCE(NULLIF(?1, ''), title), \
                     keywords = ?2, entity_names = ?3, source_uri = COALESCE(NULLIF(?4, ''), source_uri), \
                     document_date = COALESCE(?5, document_date), \
                     modified_at = COALESCE(?6, modified_at), \
                     updated_at = datetime('now') \
                     WHERE id = ?7 AND processing_run_id = ?8",
                    rusqlite::params![
                        title_val,
                        keywords,
                        entity_names,
                        source_uri,
                        document_date,
                        modified_at,
                        id,
                        rid,
                    ],
                )?;
                if rows == 0 {
                    tracing::warn!(
                        id,
                        "update_document_metadata: run_id 不匹配，跳过元数据更新（stale job）"
                    );
                }
            } else {
                conn.execute(
                    "UPDATE kb_documents SET title = COALESCE(NULLIF(?1, ''), title), \
                     keywords = ?2, entity_names = ?3, source_uri = COALESCE(NULLIF(?4, ''), source_uri), \
                     document_date = COALESCE(?5, document_date), \
                     modified_at = COALESCE(?6, modified_at), \
                     updated_at = datetime('now') \
                     WHERE id = ?7",
                    rusqlite::params![
                        title_val,
                        keywords,
                        entity_names,
                        source_uri,
                        document_date,
                        modified_at,
                        id,
                    ],
                )?;
            }
            Ok(())
        })
    }

    /// 更新文档的 granularity 和 chunk 策略
    pub fn update_document_granularity(
        &self,
        id: i64,
        granularity: &str,
        chunk_strategy: &str,
        char_count: i32,
        page_count: i32,
        token_count: Option<i32>,
    ) -> Result<()> {
        self.update_document_granularity_with_run_guard(
            id,
            granularity,
            chunk_strategy,
            char_count,
            page_count,
            token_count,
            None,
        )
    }

    /// 修复：带 run_id 守卫的 update_document_granularity。
    /// 如果提供了 run_id，UPDATE 语句增加 `AND processing_run_id = ?` 条件，
    /// 防止旧 job 在过期后继续写 granularity，污染新 run 的文档。
    #[allow(clippy::too_many_arguments)]
    pub fn update_document_granularity_with_run_guard(
        &self,
        id: i64,
        granularity: &str,
        chunk_strategy: &str,
        char_count: i32,
        page_count: i32,
        token_count: Option<i32>,
        run_id: Option<&str>,
    ) -> Result<()> {
        // 优先使用调用方通过 token_counter::count_tokens_heuristic 计算的精确值；
        // 未提供时回退到 char_count * 0.6 的粗略估算（向后兼容）。
        let approx_token_count =
            token_count.unwrap_or_else(|| ((char_count as f64) * 0.6).round() as i32);
        self.transaction(|conn| {
            if let Some(rid) = run_id {
                // 修复：增加 processing_run_id 条件，防止旧 run 污染 granularity
                let rows = conn.execute(
                    "UPDATE kb_documents SET document_granularity = ?1, chunk_strategy = ?2, \
                     content_char_count = ?3, content_token_count = ?4, page_count = ?5, updated_at = datetime('now') \
                     WHERE id = ?6 AND processing_run_id = ?7",
                    params![granularity, chunk_strategy, char_count, approx_token_count, page_count, id, rid],
                )?;
                if rows == 0 {
                    tracing::warn!(
                        id,
                        "update_document_granularity: run_id 不匹配，跳过 granularity 更新（stale job）"
                    );
                }
            } else {
                conn.execute(
                    "UPDATE kb_documents SET document_granularity = ?1, chunk_strategy = ?2, \
                     content_char_count = ?3, content_token_count = ?4, page_count = ?5, updated_at = datetime('now') \
                     WHERE id = ?6",
                    params![granularity, chunk_strategy, char_count, approx_token_count, page_count, id],
                )?;
            }
            Ok(())
        })
    }

    /// P2-019: 更新文档 OCR 状态和文本覆盖率
    pub fn update_document_ocr(
        &self,
        id: i64,
        ocr_status: &str,
        ocr_text_coverage: f64,
    ) -> Result<()> {
        self.transaction(|conn| {
            conn.execute(
                "UPDATE kb_documents SET ocr_status = ?1, ocr_text_coverage = ?2, \
                 updated_at = datetime('now') WHERE id = ?3",
                params![ocr_status, ocr_text_coverage, id],
            )?;
            Ok(())
        })
    }

    /// 内部实现：在调用者已有事务的情况下直接执行 UPDATE，不再开事务。
    /// 避免 SQLite 嵌套 BEGIN 导致写回失败。
    pub fn update_document_ocr_with_run_guard_inner(
        &self,
        id: i64,
        ocr_status: &str,
        ocr_text_coverage: f64,
        run_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn;
        if let Some(rid) = run_id {
            let rows = conn.execute(
                "UPDATE kb_documents SET ocr_status = ?1, ocr_text_coverage = ?2, \
                 updated_at = datetime('now') WHERE id = ?3 AND processing_run_id = ?4",
                params![ocr_status, ocr_text_coverage, id, rid],
            )?;
            if rows == 0 {
                tracing::warn!(
                    id,
                    "update_document_ocr_with_run_guard_inner: run_id 不匹配，跳过 OCR 状态更新（stale job）"
                );
            }
        } else {
            conn.execute(
                "UPDATE kb_documents SET ocr_status = ?1, ocr_text_coverage = ?2, \
                 updated_at = datetime('now') WHERE id = ?3",
                params![ocr_status, ocr_text_coverage, id],
            )?;
        }
        Ok(())
    }

    /// 带 run guard 的 OCR 状态更新：仅当 processing_run_id 匹配时才写入。
    /// 防止 stale OCR job 修改新 run 的 ocr_status。
    pub fn update_document_ocr_with_run_guard(
        &self,
        id: i64,
        ocr_status: &str,
        ocr_text_coverage: f64,
        run_id: Option<&str>,
    ) -> Result<()> {
        self.transaction(|conn| {
            if let Some(rid) = run_id {
                let rows = conn.execute(
                    "UPDATE kb_documents SET ocr_status = ?1, ocr_text_coverage = ?2, \
                     updated_at = datetime('now') WHERE id = ?3 AND processing_run_id = ?4",
                    params![ocr_status, ocr_text_coverage, id, rid],
                )?;
                if rows == 0 {
                    tracing::warn!(
                        id,
                        "update_document_ocr_with_run_guard: run_id 不匹配，跳过 OCR 状态更新（stale job）"
                    );
                }
            } else {
                conn.execute(
                    "UPDATE kb_documents SET ocr_status = ?1, ocr_text_coverage = ?2, \
                     updated_at = datetime('now') WHERE id = ?3",
                    params![ocr_status, ocr_text_coverage, id],
                )?;
            }
            Ok(())
        })
    }

    // --- Run guard ---

    pub fn ensure_document_run_current(
        &self,
        document_id: i64,
        processing_run_id: &str,
    ) -> Result<()> {
        self.query(|conn| {
            let current: String = conn
                .query_row(
                    "SELECT processing_run_id FROM kb_documents WHERE id = ?1",
                    [document_id],
                    |row| row.get(0),
                )
                .map_err(|e| GBrainError::Database(format!("Document not found: {}", e)))?;

            if current != processing_run_id {
                return Err(GBrainError::InvalidInput(
                    "stale KB processing job; document has a newer run".to_string(),
                ));
            }
            Ok(())
        })
    }

    // --- Folder CRUD ---

    pub fn list_folders(&self, library_id: i64) -> Result<Vec<Folder>> {
        self.query(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, created_at, updated_at, library_id, parent_id, name, sort_order \
                 FROM kb_folders WHERE library_id = ?1 \
                 ORDER BY sort_order ASC, id ASC",
            )?;
            let rows = stmt.query_map([library_id], |row| {
                Ok(Folder {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    updated_at: row.get(2)?,
                    library_id: row.get(3)?,
                    parent_id: row.get(4)?,
                    name: row.get(5)?,
                    sort_order: row.get(6)?,
                    children: Vec::new(),
                })
            })?;

            let flat: Vec<Folder> = rows.filter_map(|r| r.ok()).collect();
            Ok(build_folder_tree(flat))
        })
    }

    pub fn create_folder(&self, input: &CreateFolderInput) -> Result<i64> {
        self.transaction(|conn| {
            // Verify library exists
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM kb_libraries WHERE id = ?1",
                    [input.library_id],
                    |row| row.get(0),
                )
                .unwrap_or(false);

            if !exists {
                return Err(GBrainError::InvalidInput("Library not found".to_string()));
            }

            // Verify parent folder exists and belongs to same library
            if let Some(pid) = input.parent_id {
                let parent_lib: i64 = conn
                    .query_row(
                        "SELECT library_id FROM kb_folders WHERE id = ?1",
                        [pid],
                        |row| row.get(0),
                    )
                    .map_err(|_| {
                        GBrainError::InvalidInput("Parent folder not found".to_string())
                    })?;

                if parent_lib != input.library_id {
                    return Err(GBrainError::InvalidInput(
                        "Parent folder belongs to different library".to_string(),
                    ));
                }
            }

            let max_sort: i32 = conn
                .query_row(
                    "SELECT COALESCE(MAX(sort_order), 0) FROM kb_folders WHERE library_id = ?1",
                    [input.library_id],
                    |row| row.get(0),
                )
                .unwrap_or(0);

            conn.execute(
                "INSERT INTO kb_folders (library_id, parent_id, name, sort_order) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![input.library_id, input.parent_id, input.name, max_sort + 1],
            )?;

            Ok(conn.last_insert_rowid())
        })
    }

    pub fn delete_folder(&self, folder_id: i64) -> Result<()> {
        self.transaction(|conn| {
            // Move all documents in this folder and its descendants to ungrouped
            // Use recursive CTE to find all descendant folder IDs
            let descendant_ids: Vec<i64> = {
                let mut stmt = conn.prepare(
                    "WITH RECURSIVE descendants(id) AS (\
                     SELECT ?1 UNION ALL \
                     SELECT f.id FROM kb_folders f INNER JOIN descendants d ON f.parent_id = d.id\
                     ) SELECT id FROM descendants",
                )?;
                let rows = stmt.query_map([folder_id], |row| row.get(0))?;
                rows.filter_map(|r| r.ok()).collect()
            };

            for &fid in &descendant_ids {
                conn.execute(
                    "UPDATE kb_documents SET folder_id = NULL WHERE folder_id = ?1",
                    [fid],
                )?;
            }

            // Delete all descendant folders and the target folder itself
            // (documents already moved to ungrouped above)
            for &fid in &descendant_ids {
                conn.execute("DELETE FROM kb_folders WHERE id = ?1", [fid])?;
            }

            Ok(())
        })
    }

    pub fn move_document_to_folder(&self, document_id: i64, folder_id: Option<i64>) -> Result<()> {
        self.transaction(|conn| {
            if let Some(fid) = folder_id {
                // Verify folder belongs to same library as document
                let lib_id: i64 = conn
                    .query_row(
                        "SELECT library_id FROM kb_folders WHERE id = ?1",
                        [fid],
                        |row| row.get(0),
                    )
                    .map_err(|_| GBrainError::InvalidInput("Folder not found".to_string()))?;

                let doc_lib: i64 = conn
                    .query_row(
                        "SELECT library_id FROM kb_documents WHERE id = ?1",
                        [document_id],
                        |row| row.get(0),
                    )
                    .map_err(|_| GBrainError::InvalidInput("Document not found".to_string()))?;

                if lib_id != doc_lib {
                    return Err(GBrainError::InvalidInput(
                        "Folder belongs to different library".to_string(),
                    ));
                }
            }

            conn.execute(
                "UPDATE kb_documents SET folder_id = ?1 WHERE id = ?2",
                params![folder_id, document_id],
            )?;
            Ok(())
        })
    }

    // --- Section CRUD (P1-008) ---

    /// 写入文档章节
    #[allow(clippy::too_many_arguments)]
    pub fn insert_section(
        &self,
        document_id: i64,
        parent_section_id: Option<i64>,
        title: &str,
        title_path: &str,
        heading_level: i32,
        section_order: i32,
        page_number: Option<i32>,
        source_start: Option<i32>,
        source_end: Option<i32>,
    ) -> Result<i64> {
        self.transaction(|conn| {
            conn.execute(
                "INSERT INTO kb_document_sections (document_id, parent_section_id, title, \
                 title_path, heading_level, section_order, page_number, source_start, source_end) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    document_id,
                    parent_section_id,
                    title,
                    title_path,
                    heading_level,
                    section_order,
                    page_number,
                    source_start,
                    source_end
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// 获取文档的所有章节
    pub fn get_sections_for_document(
        &self,
        document_id: i64,
    ) -> Result<Vec<(i64, String, String, i32)>> {
        self.query(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, title, title_path, heading_level FROM kb_document_sections \
                 WHERE document_id = ?1 ORDER BY section_order",
            )?;
            let rows = stmt.query_map(params![document_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?;
            let results: Vec<(i64, String, String, i32)> = rows.filter_map(|r| r.ok()).collect();
            Ok(results)
        })
    }

    // --- Summary CRUD (P4-011) ---

    /// 写入文档/章节/表格摘要
    pub fn insert_summary(
        &self,
        document_id: i64,
        section_id: Option<i64>,
        summary_type: &str,
        summary_text: &str,
        model: &str,
    ) -> Result<i64> {
        self.transaction(|conn| {
            let tokens = crate::nlp::chinese::tokenize_content(summary_text);
            conn.execute(
                "INSERT INTO kb_document_summaries (document_id, section_id, summary_type, \
                 summary_text, summary_tokens, model) VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    document_id,
                    section_id,
                    summary_type,
                    summary_text,
                    tokens,
                    model
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// 获取文档摘要
    pub fn get_summaries_for_document(
        &self,
        document_id: i64,
    ) -> Result<Vec<(i64, String, String)>> {
        self.query(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, summary_type, summary_text FROM kb_document_summaries \
                 WHERE document_id = ?1",
            )?;
            let rows = stmt.query_map(params![document_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
            let results: Vec<(i64, String, String)> = rows.filter_map(|r| r.ok()).collect();
            Ok(results)
        })
    }

    // --- Source CRUD (P6-001/002) ---

    /// 创建导入源
    pub fn create_source(
        &self,
        library_id: i64,
        source_type: &str,
        source_uri: &str,
        display_name: &str,
        delete_policy: &str,
    ) -> Result<i64> {
        self.transaction(|conn| {
            conn.execute(
                "INSERT INTO kb_sources (library_id, source_type, source_uri, display_name, \
                 delete_policy, sync_status) VALUES (?1,?2,?3,?4,?5,'idle')",
                params![
                    library_id,
                    source_type,
                    source_uri,
                    display_name,
                    delete_policy
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// 获取单个导入源
    #[allow(clippy::type_complexity)]
    pub fn get_source(
        &self,
        source_id: i64,
    ) -> Result<Option<(i64, i64, String, String, String, String, String)>> {
        self.query(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, library_id, source_type, source_uri, display_name, delete_policy, sync_status \
                 FROM kb_sources WHERE id=?1"
            )?;
            let mut rows = stmt.query_map(params![source_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?))
            })?;
            match rows.next() {
                Some(Ok(r)) => Ok(Some(r)),
                _ => Ok(None),
            }
        })
    }

    /// 列出库的导入源
    pub fn list_sources(&self, library_id: i64) -> Result<Vec<(i64, String, String, String)>> {
        self.query(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, source_type, source_uri, display_name FROM kb_sources WHERE library_id=?1"
            )?;
            let rows = stmt.query_map(params![library_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?;
            let results: Vec<(i64, String, String, String)> = rows.filter_map(|r| r.ok()).collect();
            Ok(results)
        })
    }

    /// 删除导入源
    pub fn delete_source(&self, source_id: i64) -> Result<()> {
        self.transaction(|conn| {
            conn.execute(
                "DELETE FROM kb_source_items WHERE source_id=?1",
                params![source_id],
            )?;
            conn.execute("DELETE FROM kb_sources WHERE id=?1", params![source_id])?;
            Ok(())
        })
    }

    // --- Source Items CRUD (P6-002) ---

    /// 插入 source item（扫描后的文件条目）
    pub fn insert_source_item(
        &self,
        source_id: i64,
        item_path: &str,
        content_hash: &str,
        file_size: i64,
        last_seen_at: &str,
    ) -> Result<i64> {
        self.transaction(|conn| {
            conn.execute(
                "INSERT INTO kb_source_items (source_id, item_path, content_hash, file_size, \
                 last_seen_at, sync_status) VALUES (?1,?2,?3,?4,?5,'pending')",
                params![source_id, item_path, content_hash, file_size, last_seen_at],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// 更新 source item（hash 变化或同步状态更新）
    #[allow(clippy::too_many_arguments)]
    pub fn update_source_item(
        &self,
        source_id: i64,
        item_path: &str,
        content_hash: Option<&str>,
        sync_status: Option<&str>,
        sync_error: Option<&str>,
        document_id: Option<i64>,
        last_seen_at: Option<&str>,
    ) -> Result<()> {
        self.transaction(|conn| {
            if let Some(hash) = content_hash {
                conn.execute(
                    "UPDATE kb_source_items SET content_hash=?1 WHERE source_id=?2 AND item_path=?3",
                    params![hash, source_id, item_path],
                )?;
            }
            if let Some(status) = sync_status {
                conn.execute(
                    "UPDATE kb_source_items SET sync_status=?1 WHERE source_id=?2 AND item_path=?3",
                    params![status, source_id, item_path],
                )?;
            }
            if let Some(error) = sync_error {
                conn.execute(
                    "UPDATE kb_source_items SET sync_error=?1 WHERE source_id=?2 AND item_path=?3",
                    params![error, source_id, item_path],
                )?;
            }
            if let Some(doc_id) = document_id {
                conn.execute(
                    "UPDATE kb_source_items SET document_id=?1 WHERE source_id=?2 AND item_path=?3",
                    params![doc_id, source_id, item_path],
                )?;
            }
            if let Some(seen) = last_seen_at {
                conn.execute(
                    "UPDATE kb_source_items SET last_seen_at=?1 WHERE source_id=?2 AND item_path=?3",
                    params![seen, source_id, item_path],
                )?;
            }
            Ok(())
        })
    }

    /// 列出 source 的所有 items
    #[allow(clippy::type_complexity)]
    pub fn list_source_items(
        &self,
        source_id: i64,
    ) -> Result<Vec<(i64, String, String, Option<i64>, String)>> {
        self.query(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, item_path, content_hash, document_id, sync_status \
                 FROM kb_source_items WHERE source_id=?1 ORDER BY item_path",
            )?;
            let rows = stmt.query_map(params![source_id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?;
            let results: Vec<(i64, String, String, Option<i64>, String)> =
                rows.filter_map(|r| r.ok()).collect();
            Ok(results)
        })
    }

    /// 按 source_id 批量删除 source items
    pub fn delete_source_items_by_source(&self, source_id: i64) -> Result<i64> {
        self.transaction(|conn| {
            let count = conn.execute(
                "DELETE FROM kb_source_items WHERE source_id=?1",
                params![source_id],
            )?;
            Ok(count as i64)
        })
    }
}

/// Build a tree from a flat list of folders.
///
/// Uses a two-pass approach:
/// 1. Index all folders by id
/// 2. Attach children to their parents, extract roots
fn build_folder_tree(flat: Vec<Folder>) -> Vec<Folder> {
    if flat.is_empty() {
        return flat;
    }

    // Collect parent_id -> child indices mapping
    let mut children_map: std::collections::HashMap<i64, Vec<usize>> =
        std::collections::HashMap::new();
    let mut root_indices: Vec<usize> = Vec::new();

    for (i, folder) in flat.iter().enumerate() {
        match folder.parent_id {
            Some(pid) => {
                children_map.entry(pid).or_default().push(i);
            }
            None => {
                root_indices.push(i);
            }
        }
    }

    // Recursively build tree starting from roots
    let mut result = Vec::new();
    for &root_idx in &root_indices {
        let folder = build_subtree(&flat, root_idx, &children_map);
        result.push(folder);
    }
    result
}

fn build_subtree(
    flat: &[Folder],
    idx: usize,
    children_map: &std::collections::HashMap<i64, Vec<usize>>,
) -> Folder {
    let folder = &flat[idx];
    let child_indices = children_map.get(&folder.id).cloned().unwrap_or_default();

    let children: Vec<Folder> = child_indices
        .into_iter()
        .map(|ci| build_subtree(flat, ci, children_map))
        .collect();

    Folder {
        id: folder.id,
        created_at: folder.created_at.clone(),
        updated_at: folder.updated_at.clone(),
        library_id: folder.library_id,
        parent_id: folder.parent_id,
        name: folder.name.clone(),
        sort_order: folder.sort_order,
        children,
    }
}
