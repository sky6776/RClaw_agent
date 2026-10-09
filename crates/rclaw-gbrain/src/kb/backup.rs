//! 备份与恢复 (P5-015~P5-018) + 库级导出导入 (P5-017)
//!
//! 支持 DB + storage 备份，manifest 记录版本信息。
//! 支持单个 library 导出/导入，用于跨实例迁移。

use crate::error::{GBrainError, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 备份 archive manifest
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub schema_version: i32,
    pub created_at: String,
    pub library_ids: Vec<i64>,
    pub embedding_indexes: Vec<EmbeddingIndexInfo>,
    pub file_count: usize,
    pub db_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingIndexInfo {
    pub id: i64,
    pub library_id: i64,
    pub model: String,
    pub dimensions: i32,
}

/// 生成备份 manifest
pub fn create_manifest(
    schema_version: i32,
    library_ids: Vec<i64>,
    embedding_indexes: Vec<EmbeddingIndexInfo>,
    file_count: usize,
    db_size_bytes: u64,
) -> BackupManifest {
    BackupManifest {
        schema_version,
        created_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        library_ids,
        embedding_indexes,
        file_count,
        db_size_bytes,
    }
}

/// 备份 DB 文件 — 先执行 WAL checkpoint 确保数据完整，再复制 DB/WAL/SHM
pub fn backup_database(db_path: &Path, output_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(output_dir)
        .map_err(|e| GBrainError::FileError(format!("cannot create backup dir: {}", e)))?;

    // WAL checkpoint: 将 WAL 内容合并到主 DB 文件
    if let Ok(conn) = Connection::open(db_path) {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
    }

    let dest = output_dir.join("gbrain.db");
    std::fs::copy(db_path, &dest)
        .map_err(|e| GBrainError::FileError(format!("cannot copy DB: {}", e)))?;

    // 同时复制 WAL 和 SHM 文件（如果存在）
    // 注意：使用字符串拼接而非 with_extension，因为 with_extension 会替换扩展名，
    // 当 DB 文件名为 gbrain.sqlite 或无扩展名时会产生错误路径
    // 复制失败时报错，而非静默忽略，避免备份不完整
    for ext in &["-wal", "-shm"] {
        let src = PathBuf::from(format!("{}{}", db_path.display(), ext));
        if src.exists() {
            let dst = output_dir.join(format!("gbrain.db{}", ext));
            std::fs::copy(&src, &dst).map_err(|e| {
                GBrainError::FileError(format!(
                    "无法复制备份 sidecar {} 到 {}: {}",
                    src.display(),
                    dst.display(),
                    e
                ))
            })?;
        }
    }

    Ok(dest)
}

/// 备份 storage 目录（kb/files/）
pub fn backup_storage(storage_dir: &Path, output_dir: &Path) -> Result<usize> {
    let dest = output_dir.join("storage");
    std::fs::create_dir_all(&dest)
        .map_err(|e| GBrainError::FileError(format!("cannot create storage backup dir: {}", e)))?;
    copy_dir_recursive(storage_dir, &dest)
}

/// 备份 artifact store 目录
/// 将 artifact 文件（按 hash 去重存储的原始文件）复制到备份目录
pub fn backup_artifact_store(artifact_dir: &Path, output_dir: &Path) -> Result<usize> {
    let dest = output_dir.join("artifacts");
    std::fs::create_dir_all(&dest)
        .map_err(|e| GBrainError::FileError(format!("cannot create artifact backup dir: {}", e)))?;
    copy_dir_recursive(artifact_dir, &dest)
}

/// 递归复制目录
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<usize> {
    let mut count = 0usize;
    if !src.exists() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(src)
        .map_err(|e| GBrainError::FileError(format!("cannot read dir {}: {}", src.display(), e)))?
    {
        let entry = entry.map_err(|e| GBrainError::FileError(format!("dir entry error: {}", e)))?;
        let path = entry.path();
        let dest_path = dst.join(entry.file_name());
        if path.is_dir() {
            std::fs::create_dir_all(&dest_path).map_err(|e| {
                GBrainError::FileError(format!("无法创建备份目录 {}: {}", dest_path.display(), e))
            })?;
            count += copy_dir_recursive(&path, &dest_path)?;
        } else {
            std::fs::copy(&path, &dest_path).map_err(|e| {
                GBrainError::FileError(format!(
                    "无法复制文件 {} 到 {}: {}",
                    path.display(),
                    dest_path.display(),
                    e
                ))
            })?;
            count += 1;
        }
    }
    Ok(count)
}

/// 清理临时路径三件套（main + -wal + -shm），用于函数入口预清理和失败回滚
/// 返回删除失败的文件路径和错误信息，调用方决定是否要中止操作
fn cleanup_tmp_trio(tmp_path: &Path) -> Result<()> {
    if tmp_path.exists() {
        std::fs::remove_file(tmp_path).map_err(|e| {
            GBrainError::FileError(format!("无法删除临时文件 {}: {}", tmp_path.display(), e))
        })?;
    }
    for ext in &["-wal", "-shm"] {
        let p = PathBuf::from(format!("{}{}", tmp_path.display(), ext));
        if p.exists() {
            std::fs::remove_file(&p).map_err(|e| {
                GBrainError::FileError(format!("无法删除临时 sidecar 文件 {}: {}", p.display(), e))
            })?;
        }
    }
    Ok(())
}

/// 从备份恢复 DB。
///
/// FIX10-06: Windows 安全恢复流程：
/// 1. 清理残留的临时文件，复制备份到临时路径
/// 2. 对临时文件执行 PRAGMA integrity_check 校验完整性
/// 3. 三阶段替换：现有DB→.bak, tmp→目标, 成功删.bak/失败回滚
/// 4. 同步处理 -wal/-shm 侧车文件
pub fn restore_database(backup_path: &Path, target_db_path: &Path) -> Result<()> {
    // 步骤0：清理上次残留的临时文件（含 sidecar），防止 stale WAL/SHM 被误用
    // 删除失败时返回错误，而非 best-effort 忽略，避免锁定/无权限场景下留下脏 sidecar
    let tmp_path = target_db_path.with_extension("db.restoring");
    cleanup_tmp_trio(&tmp_path)?;

    // 步骤1：复制到临时路径
    std::fs::copy(backup_path, &tmp_path)
        .map_err(|e| GBrainError::FileError(format!("无法复制备份到临时路径: {}", e)))?;

    // 同时复制 WAL/SHM 侧车文件（如果存在）
    // 复制失败时报错，而非静默忽略，避免恢复后主 DB 与 stale WAL/SHM 搭配导致数据不一致
    for ext in &["-wal", "-shm"] {
        let src = PathBuf::from(format!("{}{}", backup_path.display(), ext));
        if src.exists() {
            let dst = PathBuf::from(format!("{}{}", tmp_path.display(), ext));
            std::fs::copy(&src, &dst).map_err(|e| {
                cleanup_tmp_trio(&tmp_path).ok();
                GBrainError::FileError(format!(
                    "无法复制备份 sidecar {} 到 {}: {}",
                    src.display(),
                    dst.display(),
                    e
                ))
            })?;
        }
    }

    // 步骤2：校验临时文件的完整性
    // 临时 DB 必须能打开，否则视为 restore 失败，避免跳过完整性校验
    // 校验放入独立 block，确保 conn 在 rename/cleanup 前释放，避免 Windows 下文件被占用
    {
        let conn = Connection::open(&tmp_path).map_err(|e| {
            let _ = cleanup_tmp_trio(&tmp_path);
            GBrainError::FileError(format!("无法打开临时数据库进行完整性校验: {}", e))
        })?;
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap_or_else(|_| "query_failed".to_string());
        if integrity != "ok" {
            // conn 在 drop 前清理文件会失败，先让 block 结束释放连接
            let integrity_err =
                GBrainError::FileError(format!("备份文件完整性校验失败: {}", integrity));
            drop(conn);
            let _ = cleanup_tmp_trio(&tmp_path);
            return Err(integrity_err);
        }
    }

    // 步骤3：三阶段替换流程（兼容 Windows）
    // 3a: 将现有 DB 及其 WAL/SHM sidecar 一起重命名为 .bak
    // FIX10-R4: 不删除 sidecar，而是和 main DB 一起移到 .bak 侧，确保回滚完整
    let bak_path = target_db_path.with_extension("db.bak");
    let existing_db_exists = target_db_path.exists();

    // 如果主 DB 不存在但目标路径残留 sidecar，必须先清理，
    // 否则新 DB 会和旧 WAL/SHM 搭配导致数据不一致
    // 删除失败时返回错误，而非 best-effort 忽略，防止锁定/无权限场景下留下脏 sidecar
    if !existing_db_exists {
        for ext in &["-wal", "-shm"] {
            let sidecar = PathBuf::from(format!("{}{}", target_db_path.display(), ext));
            if sidecar.exists() {
                std::fs::remove_file(&sidecar).map_err(|e| {
                    let _ = cleanup_tmp_trio(&tmp_path);
                    GBrainError::FileError(format!(
                        "无法删除残留 sidecar 文件 {}: {}",
                        sidecar.display(),
                        e
                    ))
                })?;
            }
        }
    }

    if existing_db_exists {
        // 先删除旧的 .bak 文件（如果存在）
        let _ = std::fs::remove_file(&bak_path);
        for ext in &["-wal", "-shm"] {
            let old_bak_sidecar = PathBuf::from(format!("{}{}", bak_path.display(), ext));
            let _ = std::fs::remove_file(&old_bak_sidecar);
        }
        // 将现有 DB 重命名为 .bak
        std::fs::rename(target_db_path, &bak_path).map_err(|e| {
            // 重命名失败，清理临时文件
            let _ = cleanup_tmp_trio(&tmp_path);
            GBrainError::FileError(format!("无法将现有数据库重命名为备份: {}", e))
        })?;
        // FIX10-R4: 将 WAL/SHM sidecar 也移到 .bak 侧
        // 记录已成功移动的 sidecar，任一失败时全部回滚（包括已移走的 sidecar）
        let mut moved_sidecars: Vec<(PathBuf, PathBuf)> = Vec::new(); // (原始路径, .bak路径)
        for ext in &["-wal", "-shm"] {
            let sidecar = PathBuf::from(format!("{}{}", target_db_path.display(), ext));
            if sidecar.exists() {
                let bak_sidecar = PathBuf::from(format!("{}{}", bak_path.display(), ext));
                if let Err(e) = std::fs::rename(&sidecar, &bak_sidecar) {
                    // FIX10-R4: sidecar rename 失败，回滚所有已移动的文件
                    // 先把 main DB 移回去
                    let _ = std::fs::rename(&bak_path, target_db_path);
                    // 把已成功移动的 sidecar 也移回原路径
                    for (orig, bak) in &moved_sidecars {
                        let _ = std::fs::rename(bak, orig);
                    }
                    // 清理临时文件三件套
                    let _ = cleanup_tmp_trio(&tmp_path);
                    return Err(GBrainError::FileError(format!(
                        "无法重命名 sidecar 文件 {}: {}",
                        sidecar.display(),
                        e
                    )));
                }
                moved_sidecars.push((sidecar, bak_sidecar));
            }
        }
    }

    // 3b: 将临时文件重命名为目标 DB
    let rename_result = std::fs::rename(&tmp_path, target_db_path);
    match rename_result {
        Ok(()) => {
            // FIX10-R5: 重命名侧车文件，记录已移动的新 sidecar，失败时先清理再回滚
            let mut moved_tmp_sidecars: Vec<(PathBuf, PathBuf)> = Vec::new(); // (tmp路径, 目标路径)
            for ext in &["-wal", "-shm"] {
                let src = PathBuf::from(format!("{}{}", tmp_path.display(), ext));
                let dst = PathBuf::from(format!("{}{}", target_db_path.display(), ext));
                if src.exists() {
                    if let Err(e) = std::fs::rename(&src, &dst) {
                        // FIX10-R5: 先清理已成功移动到目标路径的新 sidecar
                        for (tmp_sc, dst_sc) in &moved_tmp_sidecars {
                            // 尝试移回 tmp；如果目标路径文件已不在则忽略
                            if dst_sc.exists() {
                                let _ = std::fs::rename(dst_sc, tmp_sc);
                            }
                        }
                        // 将目标 DB 移回 tmp
                        let _ = std::fs::rename(target_db_path, &tmp_path);
                        // 恢复 .bak 及其 sidecar
                        if existing_db_exists {
                            let _ = std::fs::rename(&bak_path, target_db_path);
                            for ext2 in &["-wal", "-shm"] {
                                let bak_sc =
                                    PathBuf::from(format!("{}{}", bak_path.display(), ext2));
                                let orig_sc =
                                    PathBuf::from(format!("{}{}", target_db_path.display(), ext2));
                                if bak_sc.exists() {
                                    let _ = std::fs::rename(&bak_sc, &orig_sc);
                                }
                            }
                        }
                        // 清理残留的临时文件（main 已移回 tmp，但 sidecar 可能仍在目标路径）
                        let _ = cleanup_tmp_trio(&tmp_path);
                        return Err(GBrainError::FileError(format!(
                            "无法重命名临时 sidecar 文件 {}: {}",
                            src.display(),
                            e
                        )));
                    }
                    moved_tmp_sidecars.push((src, dst));
                }
            }
            // 3c: 成功，删除 .bak 文件及其 sidecar
            if existing_db_exists {
                let _ = std::fs::remove_file(&bak_path);
                for ext in &["-wal", "-shm"] {
                    let bak_sidecar = PathBuf::from(format!("{}{}", bak_path.display(), ext));
                    let _ = std::fs::remove_file(&bak_sidecar);
                }
            }
        }
        Err(e) => {
            // 3b 失败：回滚 — 将 .bak 及其 sidecar 全部恢复为原始 DB
            if existing_db_exists {
                let _ = std::fs::rename(&bak_path, target_db_path);
                for ext in &["-wal", "-shm"] {
                    let bak_sidecar = PathBuf::from(format!("{}{}", bak_path.display(), ext));
                    let orig_sidecar =
                        PathBuf::from(format!("{}{}", target_db_path.display(), ext));
                    if bak_sidecar.exists() {
                        let _ = std::fs::rename(&bak_sidecar, &orig_sidecar);
                    }
                }
            }
            // 清理临时文件
            let _ = cleanup_tmp_trio(&tmp_path);
            return Err(GBrainError::FileError(format!("无法替换目标数据库: {}", e)));
        }
    }

    Ok(())
}

/// 从备份恢复 storage
pub fn restore_storage(backup_dir: &Path, target_dir: &Path) -> Result<usize> {
    let source = backup_dir.join("storage");
    std::fs::create_dir_all(target_dir)
        .map_err(|e| GBrainError::FileError(format!("cannot create target storage dir: {}", e)))?;
    copy_dir_recursive(&source, target_dir)
}

/// 从备份恢复 artifact store
pub fn restore_artifact_store(backup_dir: &Path, target_dir: &Path) -> Result<usize> {
    let source = backup_dir.join("artifacts");
    if !source.exists() {
        // 旧版备份可能没有 artifacts 目录，跳过
        return Ok(0);
    }
    std::fs::create_dir_all(target_dir)
        .map_err(|e| GBrainError::FileError(format!("cannot create target artifact dir: {}", e)))?;
    copy_dir_recursive(&source, target_dir)
}

// ---------------------------------------------------------------------------
// P5-017: Library export/import — 单库导出导入，支持跨实例迁移
// ---------------------------------------------------------------------------

/// Library export manifest — 记录导出的库元数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryExportManifest {
    pub export_version: i32,
    pub exported_at: String,
    pub source_library_id: i64,
    pub source_library_name: String,
    pub document_count: i32,
    pub node_count: i32,
    pub embedding_indexes: Vec<EmbeddingIndexInfo>,
}

/// Export a single library to a directory archive.
///
/// Extracts all data belonging to the library into a portable format:
/// - library metadata (JSON manifest)
/// - document rows (JSON)
/// - document nodes (JSON)
/// - node embeddings (JSON)
/// - document summaries (JSON)
/// - embedding indexes (JSON)
/// - source files (copied from storage)
pub fn export_library(
    conn: &Connection,
    library_id: i64,
    output_dir: &Path,
    storage_dir: Option<&Path>,
) -> Result<LibraryExportManifest> {
    std::fs::create_dir_all(output_dir)
        .map_err(|e| GBrainError::FileError(format!("cannot create export dir: {}", e)))?;

    // Read library metadata
    let (lib_name, _): (String, i32) = conn
        .query_row(
            "SELECT name, sort_order FROM kb_libraries WHERE id=?1",
            params![library_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| GBrainError::Database(format!("library {} not found: {}", library_id, e)))?;

    // Export documents
    let doc_count = export_table_to_json(
        conn,
        output_dir,
        "documents.json",
        "SELECT id, created_at, updated_at, library_id, folder_id, original_name, \
                name_tokens, file_size, content_hash, extension, mime_type, source_type, \
                storage_path, original_path, job_id, processing_run_id, \
                parsing_status, parsing_progress, parsing_error, \
                embedding_status, embedding_progress, embedding_error, \
                word_total, split_total, title, summary, keywords, entity_names, \
                source_uri, modified_at, document_date, normalized_content_hash, simhash, \
                document_family_id, version_label, document_granularity, \
                content_char_count, content_token_count, page_count, section_count, \
                chunk_strategy, document_status, index_status, current_version_id, \
                deleted_at, purged_at, last_indexed_at, last_seen_at \
         FROM kb_documents WHERE library_id=?1 AND deleted_at IS NULL",
        params![library_id],
    )?;

    // 导出文档节点（使用当前 schema 的实际列）
    let node_count = export_table_to_json(
        conn,
        output_dir,
        "nodes.json",
        "SELECT id, created_at, updated_at, library_id, document_id, content, \
                content_tokens, level, parent_id, chunk_order, \
                section_id, title_path, page_number, source_start, source_end, \
                node_metadata, embedding_text \
         FROM kb_document_nodes WHERE library_id=?1",
        params![library_id],
    )?;

    // 导出节点 embedding（BLOB 列通过 hex() 编码，避免 JSON null 丢失数据）
    export_table_to_json(conn, output_dir, "embeddings.json",
        "SELECT node_id, hex(embedding) as embedding_hex, dimensions, model, embedded_at, embedding_index_id \
         FROM kb_node_embeddings WHERE node_id IN \
         (SELECT id FROM kb_document_nodes WHERE library_id=?1)",
        params![library_id],
    )?;

    // 导出摘要（使用当前 schema 的实际列）
    export_table_to_json(
        conn,
        output_dir,
        "summaries.json",
        "SELECT id, created_at, document_id, section_id, summary_type, \
                summary_text, summary_tokens, model \
         FROM kb_document_summaries WHERE document_id IN \
         (SELECT id FROM kb_documents WHERE library_id=?1)",
        params![library_id],
    )?;

    // Export embedding indexes
    let indexes = export_embedding_indexes(conn, library_id, output_dir)?;

    // Export folders
    export_table_to_json(
        conn,
        output_dir,
        "folders.json",
        "SELECT id, created_at, updated_at, library_id, parent_id, name, sort_order \
         FROM kb_folders WHERE library_id=?1",
        params![library_id],
    )?;

    // Export sources
    export_table_to_json(
        conn,
        output_dir,
        "sources.json",
        "SELECT id, library_id, source_type, source_uri, display_name, \
                delete_policy, sync_status, last_synced_at \
         FROM kb_sources WHERE library_id=?1",
        params![library_id],
    )?;

    // FIX10-07: 复制受控 storage 中的源文件到 archive 的 files/ 目录，
    // 并在 documents.json 中注入 archive_file_path 字段，记录文件在 archive 内的相对路径。
    // 导入时根据 archive_file_path 定位文件，而非从旧绝对路径推断。
    let mut archive_file_paths: std::collections::HashMap<i64, String> =
        std::collections::HashMap::new();
    if let Some(storage_root) = storage_dir {
        let files_dir = output_dir.join("files");
        if storage_root.exists() {
            // 查询此库所有文档的 id 和 storage_path
            let mut stmt = conn.prepare(
                "SELECT id, storage_path FROM kb_documents WHERE library_id=?1 AND deleted_at IS NULL",
            )?;
            let rows: Vec<(i64, String)> = stmt
                .query_map(params![library_id], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .filter_map(|r| r.ok())
                .filter(|(_, s)| !s.is_empty())
                .collect();
            for (doc_id, sp) in &rows {
                let src_path = std::path::Path::new(sp);
                // 只复制位于 KB storage root 内的文件
                if src_path.starts_with(storage_root) && src_path.exists() {
                    if let Ok(relative) = src_path.strip_prefix(storage_root) {
                        let dest = files_dir.join(relative);
                        if let Some(parent) = dest.parent() {
                            std::fs::create_dir_all(parent).ok();
                        }
                        std::fs::copy(src_path, &dest).ok();
                        // 记录文件在 archive 内的相对路径（使用正斜杠，跨平台兼容）
                        let archive_relative = relative.to_string_lossy().replace('\\', "/");
                        archive_file_paths.insert(*doc_id, archive_relative);
                    }
                }
            }
        }
    }

    // FIX10-07: 将 archive_file_path 注入到 documents.json 中
    if !archive_file_paths.is_empty() {
        let docs_path = output_dir.join("documents.json");
        if docs_path.exists() {
            let docs_data = std::fs::read_to_string(&docs_path)?;
            let docs: Vec<serde_json::Map<String, serde_json::Value>> =
                serde_json::from_str(&docs_data)?;
            let updated_docs: Vec<serde_json::Map<String, serde_json::Value>> = docs
                .into_iter()
                .map(|mut doc| {
                    let doc_id = doc.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
                    if let Some(afp) = archive_file_paths.get(&doc_id) {
                        doc.insert(
                            "archive_file_path".to_string(),
                            serde_json::Value::String(afp.clone()),
                        );
                    }
                    doc
                })
                .collect();
            let json = serde_json::to_string_pretty(&updated_docs)?;
            std::fs::write(&docs_path, json)?;
        }
    }

    // FIX9-15: 导出遗漏的关键表 — tables, table_rows, sections, source_items
    export_table_to_json(
        conn,
        output_dir,
        "tables.json",
        "SELECT id, created_at, document_id, sheet_name, headers, column_count, row_count \
         FROM kb_tables WHERE document_id IN \
         (SELECT id FROM kb_documents WHERE library_id=?1)",
        params![library_id],
    )?;

    export_table_to_json(
        conn,
        output_dir,
        "table_rows.json",
        "SELECT id, created_at, table_id, row_index, row_text, row_tokens, row_json \
         FROM kb_table_rows WHERE table_id IN \
         (SELECT id FROM kb_tables WHERE document_id IN \
         (SELECT id FROM kb_documents WHERE library_id=?1))",
        params![library_id],
    )?;

    export_table_to_json(
        conn,
        output_dir,
        "sections.json",
        "SELECT id, created_at, updated_at, document_id, parent_section_id, title, \
                title_path, heading_level, section_order, page_number, \
                source_start, source_end, content_summary \
         FROM kb_document_sections WHERE document_id IN \
         (SELECT id FROM kb_documents WHERE library_id=?1)",
        params![library_id],
    )?;

    export_table_to_json(
        conn,
        output_dir,
        "source_items.json",
        "SELECT id, created_at, source_id, document_id, external_id, item_path, \
                content_hash, file_size, last_seen_at, sync_status, sync_error \
         FROM kb_source_items WHERE source_id IN \
         (SELECT id FROM kb_sources WHERE library_id=?1)",
        params![library_id],
    )?;

    // Build manifest
    let manifest = LibraryExportManifest {
        export_version: 1,
        exported_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        source_library_id: library_id,
        source_library_name: lib_name,
        document_count: doc_count as i32,
        node_count: node_count as i32,
        embedding_indexes: indexes,
    };

    // Write manifest
    let manifest_json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| GBrainError::FileError(format!("cannot serialize manifest: {}", e)))?;
    std::fs::write(output_dir.join("manifest.json"), manifest_json)
        .map_err(|e| GBrainError::FileError(format!("cannot write manifest: {}", e)))?;

    Ok(manifest)
}

/// Import a library from an export archive into the target database.
///
/// Creates a new library (with a new ID), restores all documents, nodes,
/// embeddings, summaries, and embedding indexes. Handles name conflicts
/// by appending a suffix.
pub fn import_library(
    conn: &Connection,
    archive_dir: &Path,
    new_name: Option<&str>,
    target_storage_dir: Option<&Path>,
) -> Result<i64> {
    // Read manifest
    let manifest_data = std::fs::read_to_string(archive_dir.join("manifest.json"))
        .map_err(|e| GBrainError::FileError(format!("cannot read manifest: {}", e)))?;
    let manifest: LibraryExportManifest = serde_json::from_str(&manifest_data)
        .map_err(|e| GBrainError::FileError(format!("cannot parse manifest: {}", e)))?;

    // Determine library name (resolve conflicts)
    let lib_name = new_name.unwrap_or(&manifest.source_library_name);
    let final_name = resolve_library_name_conflict(conn, lib_name)?;

    // Create new library
    conn.execute(
        "INSERT INTO kb_libraries (name, sort_order, raptor_enabled, \
         chunk_size, chunk_overlap, batch_max_documents, batch_max_chunks) \
         VALUES (?1, 0, 1, 512, 50, 3, 10)",
        params![final_name],
    )?;
    let new_lib_id = conn.last_insert_rowid();

    // Import embedding indexes FIRST — 后面的 embedding 导入依赖 index_id 映射
    let (mut index_id_map, mut active_index_id) =
        import_embedding_indexes(conn, archive_dir, new_lib_id)?;

    // 若备份中没有 embedding index，为导入库创建默认 index 并激活
    // 避免之后新增文档/re-embed 时报 "没有 active embedding index"
    // 同时将 0 → default_idx 加入映射，让旧 embedding（无 index_id）能正确归属
    if index_id_map.is_empty() {
        let dims = infer_embedding_dimensions(archive_dir).unwrap_or(1536);
        let default_idx = crate::kb::embedding_index::create_embedding_index(
            conn,
            new_lib_id,
            "openai",
            "text-embedding-3-large",
            dims,
            "vec0",
        )?;
        crate::kb::embedding_index::activate_index(conn, default_idx)?;
        index_id_map.insert(0, default_idx);
        active_index_id = Some(default_idx);
    }

    // Import folders FIRST — 建立 old_id → new_id 映射，后续导入依赖此映射
    let folder_id_map = import_folders(conn, archive_dir, new_lib_id)?;

    // Import documents — assign new IDs, remap library_id and folder_id
    let doc_id_map = import_documents(
        conn,
        archive_dir,
        new_lib_id,
        &folder_id_map,
        target_storage_dir,
    )?;

    // 导入节点 — 重映射 document_id 和 library_id，返回 old→new node_id 映射
    // FIX9-14: import_nodes 同时返回旧 parent_id/section_id 以便回填
    let (node_id_map, old_parent_ids, old_section_ids) =
        import_nodes(conn, archive_dir, new_lib_id, &doc_id_map)?;

    // 回填节点的 parent_id（RAPTOR 层级关系）
    backfill_node_refs(conn, &node_id_map, &old_parent_ids)?;

    // 导入 sections — 必须在 import_nodes 之后，以便回填 section_id
    let section_id_map = import_sections(conn, archive_dir, new_lib_id, &doc_id_map)?;

    // 回填节点的 section_id（用 section_id_map + old_section_ids）
    backfill_section_refs(conn, &old_section_ids, &section_id_map)?;

    // 导入 embedding — 使用 node_id_map + index_id_map 重映射
    import_embeddings(
        conn,
        archive_dir,
        &node_id_map,
        &index_id_map,
        active_index_id,
    )?;

    // Import summaries — remap document_id and section_id (using section_id_map)
    import_summaries(conn, archive_dir, &doc_id_map, &section_id_map)?;

    // FIX9-15: 导入遗漏的关键表 — tables, table_rows, source_items
    let table_id_map = import_tables(conn, archive_dir, new_lib_id, &doc_id_map)?;
    import_table_rows(conn, archive_dir, &table_id_map)?;
    let source_id_map = import_sources(conn, archive_dir, new_lib_id)?;
    import_source_items(conn, archive_dir, &source_id_map, &doc_id_map)?;

    Ok(new_lib_id)
}

// --- Helper functions for export/import ---

/// Export a SQL query result to a JSON file
fn export_table_to_json<P: rusqlite::Params>(
    conn: &Connection,
    output_dir: &Path,
    filename: &str,
    sql: &str,
    params: P,
) -> Result<usize> {
    let mut stmt = conn.prepare(sql)?;
    let column_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let col_count = column_names.len();
    let rows: Vec<serde_json::Map<String, serde_json::Value>> = stmt
        .query_map(params, |row| {
            let mut map = serde_json::Map::new();
            for (i, _col_name) in column_names.iter().enumerate().take(col_count) {
                let val: serde_json::Value = match row.get_ref(i) {
                    Ok(rusqlite::types::ValueRef::Null) => serde_json::Value::Null,
                    Ok(rusqlite::types::ValueRef::Integer(n)) => serde_json::json!(n),
                    Ok(rusqlite::types::ValueRef::Real(f)) => serde_json::json!(f),
                    Ok(rusqlite::types::ValueRef::Text(s)) => {
                        // rusqlite 的 ValueRef::Text 返回 &[u8]，必须转为 UTF-8 字符串
                        // 否则 serde_json 会将其序列化为字节数组
                        std::str::from_utf8(s)
                            .map(|s| serde_json::json!(s))
                            .unwrap_or(serde_json::Value::Null)
                    }
                    Ok(rusqlite::types::ValueRef::Blob(_)) => serde_json::Value::Null,
                    Err(_) => serde_json::Value::Null,
                };
                map.insert(column_names[i].clone(), val);
            }
            Ok(map)
        })?
        .filter_map(|r| r.ok())
        .collect();

    let json = serde_json::to_string_pretty(&rows)
        .map_err(|e| GBrainError::FileError(format!("cannot serialize {}: {}", filename, e)))?;
    std::fs::write(output_dir.join(filename), json)
        .map_err(|e| GBrainError::FileError(format!("cannot write {}: {}", filename, e)))?;
    Ok(rows.len())
}

fn export_embedding_indexes(
    conn: &Connection,
    library_id: i64,
    output_dir: &Path,
) -> Result<Vec<EmbeddingIndexInfo>> {
    let indexes = crate::kb::embedding_index::list_embedding_indexes(conn, library_id)?;
    let infos: Vec<EmbeddingIndexInfo> = indexes
        .iter()
        .map(|idx| EmbeddingIndexInfo {
            id: idx.id,
            library_id: idx.library_id,
            model: idx.model.clone(),
            dimensions: idx.dimensions,
        })
        .collect();
    let json = serde_json::to_string_pretty(&indexes)
        .map_err(|e| GBrainError::FileError(format!("cannot serialize indexes: {}", e)))?;
    std::fs::write(output_dir.join("embedding_indexes.json"), json)
        .map_err(|e| GBrainError::FileError(format!("cannot write indexes: {}", e)))?;
    Ok(infos)
}

fn resolve_library_name_conflict(conn: &Connection, desired_name: &str) -> Result<String> {
    let existing: Vec<String> = conn
        .prepare("SELECT name FROM kb_libraries")?
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();

    if !existing.contains(&desired_name.to_string()) {
        return Ok(desired_name.to_string());
    }

    // Append suffix to resolve conflict
    for i in 1..100 {
        let candidate = format!("{} (import-{})", desired_name, i);
        if !existing.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Ok(format!("{} (import)", desired_name))
}

fn import_documents(
    conn: &Connection,
    archive_dir: &Path,
    new_lib_id: i64,
    folder_id_map: &std::collections::HashMap<i64, i64>,
    target_storage_dir: Option<&Path>,
) -> Result<std::collections::HashMap<i64, i64>> {
    let data = std::fs::read_to_string(archive_dir.join("documents.json"))
        .map_err(|e| GBrainError::FileError(format!("cannot read documents.json: {}", e)))?;
    let docs: Vec<serde_json::Map<String, serde_json::Value>> = serde_json::from_str(&data)
        .map_err(|e| GBrainError::FileError(format!("cannot parse documents.json: {}", e)))?;

    let mut id_map = std::collections::HashMap::new();
    for doc in &docs {
        let old_id = doc.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let old_folder_id = json_null_or_int(doc, "folder_id");
        let new_folder_id = old_folder_id.and_then(|fid| folder_id_map.get(&fid).copied());

        // FIX10-07: 重写 storage_path：优先使用 archive_file_path 定位 archive 内的文件，
        // 复制到目标 storage 并改写为新路径。archive_file_path 由导出阶段注入，
        // 记录文件在 archive 内的相对路径，避免从旧绝对路径推断（Windows 路径无法 strip_prefix("/")）。
        // 若无 archive_file_path，回退到旧逻辑（兼容旧版导出）。
        // 源文件不在受控 storage root 内时，保留 original_path，但 storage_path 不指向旧机器路径。
        let old_storage_path = json_str(doc, "storage_path");
        let archive_file_path = json_str(doc, "archive_file_path");
        let new_storage_path = if !old_storage_path.is_empty() {
            if let Some(storage_root) = target_storage_dir {
                // 优先使用 archive_file_path（跨平台兼容）
                if !archive_file_path.is_empty() {
                    let archive_file = archive_dir.join("files").join(&archive_file_path);
                    if archive_file.exists() {
                        let dest = storage_root.join(&archive_file_path);
                        if let Some(parent) = dest.parent() {
                            std::fs::create_dir_all(parent).ok();
                        }
                        if std::fs::copy(&archive_file, &dest).is_ok() {
                            dest.to_string_lossy().to_string()
                        } else {
                            // 复制失败，保留旧路径
                            old_storage_path.clone()
                        }
                    } else {
                        // archive 中没有此文件，保留旧路径
                        old_storage_path.clone()
                    }
                } else {
                    // 回退：兼容旧版导出（无 archive_file_path），尝试从旧路径推断
                    let old_path = std::path::Path::new(&old_storage_path);
                    if let Ok(relative) = old_path.strip_prefix("/") {
                        let archive_file = archive_dir.join("files").join(relative);
                        if archive_file.exists() {
                            let dest = storage_root.join(relative);
                            if let Some(parent) = dest.parent() {
                                std::fs::create_dir_all(parent).ok();
                            }
                            if std::fs::copy(&archive_file, &dest).is_ok() {
                                dest.to_string_lossy().to_string()
                            } else {
                                old_storage_path.clone()
                            }
                        } else {
                            old_storage_path.clone()
                        }
                    } else {
                        // 无法提取相对路径，保留旧路径
                        old_storage_path.clone()
                    }
                }
            } else {
                old_storage_path.clone()
            }
        } else {
            old_storage_path.clone()
        };

        conn.execute(
            "INSERT INTO kb_documents (library_id, folder_id, original_name, name_tokens, file_size, \
             content_hash, extension, mime_type, source_type, storage_path, original_path, \
             job_id, processing_run_id, parsing_status, parsing_progress, \
             embedding_status, embedding_progress, word_total, split_total, \
             title, summary, keywords, entity_names, source_uri, \
             document_granularity, content_char_count, content_token_count, \
             page_count, section_count, chunk_strategy, document_status, index_status) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32)",
            params![
                new_lib_id,
                new_folder_id,
                json_str(doc, "original_name"),
                json_str(doc, "name_tokens"),
                json_int(doc, "file_size"),
                json_str(doc, "content_hash"),
                json_str(doc, "extension"),
                json_str(doc, "mime_type"),
                json_str(doc, "source_type"),
                new_storage_path,
                json_str(doc, "original_path"),
                json_str(doc, "job_id"),
                json_str(doc, "processing_run_id"),
                json_int(doc, "parsing_status"),
                json_int(doc, "parsing_progress"),
                json_int(doc, "embedding_status"),
                json_int(doc, "embedding_progress"),
                json_int(doc, "word_total"),
                json_int(doc, "split_total"),
                json_str(doc, "title"),
                json_str(doc, "summary"),
                json_str(doc, "keywords"),
                json_str(doc, "entity_names"),
                json_str(doc, "source_uri"),
                json_str(doc, "document_granularity"),
                json_int(doc, "content_char_count"),
                json_int(doc, "content_token_count"),
                json_int(doc, "page_count"),
                json_int(doc, "section_count"),
                json_str(doc, "chunk_strategy"),
                json_str(doc, "document_status"),
                json_str(doc, "index_status"),
            ],
        )?;
        id_map.insert(old_id, conn.last_insert_rowid());
    }
    Ok(id_map)
}

#[allow(clippy::type_complexity)]
fn import_nodes(
    conn: &Connection,
    archive_dir: &Path,
    new_lib_id: i64,
    doc_id_map: &std::collections::HashMap<i64, i64>,
) -> Result<(
    std::collections::HashMap<i64, i64>,
    std::collections::HashMap<i64, i64>,
    std::collections::HashMap<i64, i64>,
)> {
    let data = std::fs::read_to_string(archive_dir.join("nodes.json"))
        .map_err(|e| GBrainError::FileError(format!("cannot read nodes.json: {}", e)))?;
    let nodes: Vec<serde_json::Map<String, serde_json::Value>> = serde_json::from_str(&data)
        .map_err(|e| GBrainError::FileError(format!("cannot parse nodes.json: {}", e)))?;

    let mut node_id_map = std::collections::HashMap::new();
    // FIX9-14: 记录每个新节点对应的旧 parent_id 和旧 section_id，
    // 以便 backfill_node_refs 用 node_id_map 映射回填
    let mut old_parent_ids = std::collections::HashMap::new();
    let mut old_section_ids = std::collections::HashMap::new();
    for node in &nodes {
        let old_node_id = node.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let old_doc_id = node
            .get("document_id")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let new_doc_id = doc_id_map.get(&old_doc_id).copied().unwrap_or(0);
        // FIX9-14: 第一阶段插入时 parent_id 和 section_id 设为 NULL，
        // 后续由 backfill_node_refs 用 old→new id 映射回填
        conn.execute(
            "INSERT INTO kb_document_nodes (library_id, document_id, content, content_tokens, \
             level, parent_id, chunk_order, section_id, title_path, page_number, \
             source_start, source_end, node_metadata, embedding_text) \
             VALUES (?, ?, ?, ?, ?, NULL, ?, NULL, ?, ?, ?, ?, ?, ?)",
            params![
                new_lib_id,
                new_doc_id,
                json_str(node, "content"),
                json_str(node, "content_tokens"),
                json_int(node, "level"),
                json_int(node, "chunk_order"),
                json_str(node, "title_path"),
                json_null_or_int(node, "page_number"),
                json_null_or_int(node, "source_start"),
                json_null_or_int(node, "source_end"),
                json_str(node, "node_metadata"),
                json_str(node, "embedding_text"),
            ],
        )?;
        let new_id = conn.last_insert_rowid();
        node_id_map.insert(old_node_id, new_id);
        // 记录旧的 parent_id 和 section_id
        if let Some(old_pid) = json_null_or_int(node, "parent_id") {
            old_parent_ids.insert(new_id, old_pid);
        }
        if let Some(old_sid) = json_null_or_int(node, "section_id") {
            old_section_ids.insert(new_id, old_sid);
        }
    }
    Ok((node_id_map, old_parent_ids, old_section_ids))
}

/// FIX9-14: 回填节点的 parent_id。
/// import_nodes 第一阶段插入时 parent_id 和 section_id 设为 NULL，
/// 此函数用 old→new node_id 映射回填 parent_id。
/// section_id 由 backfill_section_refs 用 section_id_map 单独回填。
fn backfill_node_refs(
    conn: &Connection,
    node_id_map: &std::collections::HashMap<i64, i64>,
    old_parent_ids: &std::collections::HashMap<i64, i64>,
) -> Result<()> {
    // 回填 parent_id：用 node_id_map 将旧 parent_id 映射为新 parent_id
    for (&new_node_id, &old_parent_id) in old_parent_ids {
        if let Some(&new_parent_id) = node_id_map.get(&old_parent_id) {
            conn.execute(
                "UPDATE kb_document_nodes SET parent_id = ?1 WHERE id = ?2",
                params![new_parent_id, new_node_id],
            )?;
        }
    }
    Ok(())
}

fn import_embeddings(
    conn: &Connection,
    archive_dir: &Path,
    node_id_map: &std::collections::HashMap<i64, i64>,
    index_id_map: &std::collections::HashMap<i64, i64>,
    active_index_id: Option<i64>,
) -> Result<()> {
    let path = archive_dir.join("embeddings.json");
    if !path.exists() {
        return Ok(());
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| GBrainError::FileError(format!("cannot read embeddings.json: {}", e)))?;
    let embeddings: Vec<serde_json::Map<String, serde_json::Value>> =
        serde_json::from_str(&data)
            .map_err(|e| GBrainError::FileError(format!("cannot parse embeddings.json: {}", e)))?;

    for emb in &embeddings {
        let old_node_id = emb.get("node_id").and_then(|v| v.as_i64()).unwrap_or(0);
        let new_node_id = match node_id_map.get(&old_node_id).copied() {
            Some(id) => id,
            None => {
                return Err(GBrainError::FileError(format!(
                    "embedding 引用的 node_id={} 在导入的节点中不存在",
                    old_node_id
                )))
            }
        };
        let old_index_id = emb
            .get("embedding_index_id")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let new_index_id = if old_index_id > 0 {
            index_id_map.get(&old_index_id).copied().ok_or_else(|| {
                GBrainError::FileError(format!(
                    "embedding 引用的 embedding_index_id={} 在导入的索引中不存在",
                    old_index_id
                ))
            })?
        } else {
            // 旧数据没有 index_id，归入 active/default index（确定性选择）
            active_index_id.ok_or_else(|| {
                GBrainError::FileError(
                    "无法解析 embedding 的 index 归属：没有可用的 active index".into(),
                )
            })?
        };

        let hex_str = emb
            .get("embedding_hex")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if hex_str.is_empty() {
            return Err(GBrainError::FileError("embedding_hex 字段为空".into()));
        }
        let blob = hex::decode(hex_str)
            .map_err(|e| GBrainError::FileError(format!("embedding hex 解码失败: {}", e)))?;

        // 校验 blob 长度必须是 4 的倍数（每个 f32 占 4 字节）
        if blob.len() % 4 != 0 {
            return Err(GBrainError::FileError(format!(
                "embedding blob 长度 {} 不是 4 的倍数，数据可能损坏",
                blob.len()
            )));
        }

        // blob → f32 向量（用于统一写入函数）
        let embedding_vec: Vec<f32> = blob
            .chunks_exact(4)
            .map(|chunk| {
                let bytes: [u8; 4] = chunk.try_into().expect("chunks_exact(4) 保证 4 字节");
                f32::from_le_bytes(bytes)
            })
            .collect();
        if embedding_vec.is_empty() {
            return Err(GBrainError::FileError("embedding blob 解码后为空".into()));
        }

        // 校验备份中的 dimensions 与实际解码长度一致
        let backup_dims = emb.get("dimensions").and_then(|v| v.as_i64()).unwrap_or(0) as usize;
        if backup_dims > 0 && backup_dims != embedding_vec.len() {
            return Err(GBrainError::FileError(format!(
                "embedding 维度不一致：备份声明 dimensions={}，实际解码长度={}",
                backup_dims,
                embedding_vec.len()
            )));
        }

        // 校验目标 index 的维度与向量维度一致
        let index_dims: i32 = conn
            .query_row(
                "SELECT dimensions FROM kb_embedding_indexes WHERE id = ?1",
                rusqlite::params![new_index_id],
                |row| row.get(0),
            )
            .map_err(|_| {
                GBrainError::FileError(format!("目标 embedding index {} 不存在", new_index_id))
            })?;
        if index_dims as usize != embedding_vec.len() {
            return Err(GBrainError::FileError(format!(
                "embedding 维度与目标 index 不匹配：向量={}，index {} dimensions={}",
                embedding_vec.len(),
                new_index_id,
                index_dims
            )));
        }

        let dimensions = embedding_vec.len() as i32;
        let model = emb
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("text-embedding-3-large");

        // 使用统一函数写入：BLOB 表 + per-index vec 表同步更新
        crate::kb::embedding_index::upsert_node_embedding_for_index(
            conn,
            new_node_id,
            new_index_id,
            &embedding_vec,
            dimensions,
            model,
        )?;
    }
    Ok(())
}

fn import_summaries(
    conn: &Connection,
    archive_dir: &Path,
    doc_id_map: &std::collections::HashMap<i64, i64>,
    section_id_map: &std::collections::HashMap<i64, i64>,
) -> Result<()> {
    let path = archive_dir.join("summaries.json");
    if !path.exists() {
        return Ok(());
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| GBrainError::FileError(format!("cannot read summaries.json: {}", e)))?;
    let summaries: Vec<serde_json::Map<String, serde_json::Value>> = serde_json::from_str(&data)
        .map_err(|e| GBrainError::FileError(format!("cannot parse summaries.json: {}", e)))?;

    for summary in &summaries {
        let old_doc_id = summary
            .get("document_id")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let new_doc_id = doc_id_map.get(&old_doc_id).copied().unwrap_or(0);
        let old_section_id = json_null_or_int(summary, "section_id");
        let new_section_id = old_section_id.and_then(|sid| section_id_map.get(&sid).copied());
        conn.execute(
            "INSERT INTO kb_document_summaries (document_id, section_id, summary_type, \
             summary_text, summary_tokens, model) \
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                new_doc_id,
                new_section_id,
                json_str(summary, "summary_type"),
                json_str(summary, "summary_text"),
                json_str(summary, "summary_tokens"),
                json_str(summary, "model"),
            ],
        )?;
    }
    Ok(())
}

// FIX9-15: 导入遗漏的关键表

/// 导入 kb_tables，返回 old→new table_id 映射
fn import_tables(
    conn: &Connection,
    archive_dir: &Path,
    _new_lib_id: i64,
    doc_id_map: &std::collections::HashMap<i64, i64>,
) -> Result<std::collections::HashMap<i64, i64>> {
    let path = archive_dir.join("tables.json");
    if !path.exists() {
        return Ok(std::collections::HashMap::new());
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| GBrainError::FileError(format!("cannot read tables.json: {}", e)))?;
    let tables: Vec<serde_json::Map<String, serde_json::Value>> = serde_json::from_str(&data)
        .map_err(|e| GBrainError::FileError(format!("cannot parse tables.json: {}", e)))?;

    let mut id_map = std::collections::HashMap::new();
    for table in &tables {
        let old_id = table.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let old_doc_id = table
            .get("document_id")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let new_doc_id = doc_id_map.get(&old_doc_id).copied().unwrap_or(0);
        conn.execute(
            "INSERT INTO kb_tables (document_id, sheet_name, headers, column_count, row_count) \
             VALUES (?1,?2,?3,?4,?5)",
            params![
                new_doc_id,
                json_str(table, "sheet_name"),
                json_str(table, "headers"),
                json_int(table, "column_count"),
                json_int(table, "row_count"),
            ],
        )?;
        id_map.insert(old_id, conn.last_insert_rowid());
    }
    Ok(id_map)
}

/// 导入 kb_table_rows，重映射 table_id
fn import_table_rows(
    conn: &Connection,
    archive_dir: &Path,
    table_id_map: &std::collections::HashMap<i64, i64>,
) -> Result<()> {
    let path = archive_dir.join("table_rows.json");
    if !path.exists() {
        return Ok(());
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| GBrainError::FileError(format!("cannot read table_rows.json: {}", e)))?;
    let rows: Vec<serde_json::Map<String, serde_json::Value>> = serde_json::from_str(&data)
        .map_err(|e| GBrainError::FileError(format!("cannot parse table_rows.json: {}", e)))?;

    for row in &rows {
        let old_table_id = row.get("table_id").and_then(|v| v.as_i64()).unwrap_or(0);
        let new_table_id = table_id_map.get(&old_table_id).copied().unwrap_or(0);
        conn.execute(
            "INSERT INTO kb_table_rows (table_id, row_index, row_text, row_tokens, row_json) VALUES (?1,?2,?3,?4,?5)",
            params![
                new_table_id,
                json_int(row, "row_index"),
                json_str(row, "row_text"),
                json_str(row, "row_tokens"),
                json_str(row, "row_json"),
            ],
        )?;
    }
    Ok(())
}

/// 导入 kb_document_sections，返回 old→new section_id 映射
fn import_sections(
    conn: &Connection,
    archive_dir: &Path,
    _new_lib_id: i64,
    doc_id_map: &std::collections::HashMap<i64, i64>,
) -> Result<std::collections::HashMap<i64, i64>> {
    let path = archive_dir.join("sections.json");
    if !path.exists() {
        return Ok(std::collections::HashMap::new());
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| GBrainError::FileError(format!("cannot read sections.json: {}", e)))?;
    let sections: Vec<serde_json::Map<String, serde_json::Value>> = serde_json::from_str(&data)
        .map_err(|e| GBrainError::FileError(format!("cannot parse sections.json: {}", e)))?;

    // 两阶段导入：先插入（parent_section_id=NULL），再回填
    let mut id_map = std::collections::HashMap::new();
    for section in &sections {
        let old_id = section.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let old_doc_id = section
            .get("document_id")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let new_doc_id = doc_id_map.get(&old_doc_id).copied().unwrap_or(0);
        conn.execute(
            "INSERT INTO kb_document_sections \
             (document_id, parent_section_id, title, title_path, heading_level, \
              section_order, page_number, source_start, source_end, content_summary) \
             VALUES (?, NULL, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                new_doc_id,
                json_str(section, "title"),
                json_str(section, "title_path"),
                json_int(section, "heading_level"),
                json_int(section, "section_order"),
                json_null_or_int(section, "page_number"),
                json_null_or_int(section, "source_start"),
                json_null_or_int(section, "source_end"),
                json_str(section, "content_summary"),
            ],
        )?;
        id_map.insert(old_id, conn.last_insert_rowid());
    }

    // 回填 parent_section_id
    for section in &sections {
        let old_id = section.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        if let Some(&new_id) = id_map.get(&old_id) {
            if let Some(old_parent) = json_null_or_int(section, "parent_section_id") {
                if let Some(&new_parent) = id_map.get(&old_parent) {
                    conn.execute(
                        "UPDATE kb_document_sections SET parent_section_id = ?1 WHERE id = ?2",
                        params![new_parent, new_id],
                    )?;
                }
            }
        }
    }
    Ok(id_map)
}

/// 用 section_id_map 回填节点的 section_id
/// old_section_ids 记录了每个新 node_id 对应的旧 section_id，
/// 用 section_id_map 将旧 section_id 映射为新 section_id
fn backfill_section_refs(
    conn: &Connection,
    old_section_ids: &std::collections::HashMap<i64, i64>,
    section_id_map: &std::collections::HashMap<i64, i64>,
) -> Result<()> {
    for (&new_node_id, &old_section_id) in old_section_ids {
        if let Some(&new_section_id) = section_id_map.get(&old_section_id) {
            conn.execute(
                "UPDATE kb_document_nodes SET section_id = ?1 WHERE id = ?2",
                params![new_section_id, new_node_id],
            )?;
        }
    }
    Ok(())
}

/// 导入 kb_sources（已有 sources.json），返回 old→new source_id 映射
fn import_sources(
    conn: &Connection,
    archive_dir: &Path,
    new_lib_id: i64,
) -> Result<std::collections::HashMap<i64, i64>> {
    let path = archive_dir.join("sources.json");
    if !path.exists() {
        return Ok(std::collections::HashMap::new());
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| GBrainError::FileError(format!("cannot read sources.json: {}", e)))?;
    let sources: Vec<serde_json::Map<String, serde_json::Value>> = serde_json::from_str(&data)
        .map_err(|e| GBrainError::FileError(format!("cannot parse sources.json: {}", e)))?;

    let mut id_map = std::collections::HashMap::new();
    for source in &sources {
        let old_id = source.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        conn.execute(
            "INSERT INTO kb_sources (library_id, source_type, source_uri, display_name, \
             delete_policy, sync_status, last_synced_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                new_lib_id,
                json_str(source, "source_type"),
                json_str(source, "source_uri"),
                json_str(source, "display_name"),
                json_str(source, "delete_policy"),
                json_str(source, "sync_status"),
                json_str(source, "last_synced_at"),
            ],
        )?;
        id_map.insert(old_id, conn.last_insert_rowid());
    }
    Ok(id_map)
}

/// 导入 kb_source_items，重映射 source_id 和 document_id
fn import_source_items(
    conn: &Connection,
    archive_dir: &Path,
    source_id_map: &std::collections::HashMap<i64, i64>,
    doc_id_map: &std::collections::HashMap<i64, i64>,
) -> Result<()> {
    let path = archive_dir.join("source_items.json");
    if !path.exists() {
        return Ok(());
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| GBrainError::FileError(format!("cannot read source_items.json: {}", e)))?;
    let items: Vec<serde_json::Map<String, serde_json::Value>> = serde_json::from_str(&data)
        .map_err(|e| GBrainError::FileError(format!("cannot parse source_items.json: {}", e)))?;

    for item in &items {
        let old_source_id = item.get("source_id").and_then(|v| v.as_i64()).unwrap_or(0);
        let new_source_id = source_id_map.get(&old_source_id).copied().unwrap_or(0);
        // 重映射 document_id：旧 ID → 新 ID，无法映射时写 NULL
        let new_document_id = item
            .get("document_id")
            .and_then(|v| v.as_i64())
            .and_then(|old_id| doc_id_map.get(&old_id).copied());
        conn.execute(
            "INSERT INTO kb_source_items (source_id, document_id, external_id, item_path, \
             content_hash, file_size, last_seen_at, sync_status, sync_error) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                new_source_id,
                new_document_id,
                json_str(item, "external_id"),
                json_str(item, "item_path"),
                json_str(item, "content_hash"),
                json_int(item, "file_size"),
                json_opt_str(item, "last_seen_at"),
                json_str(item, "sync_status"),
                json_str(item, "sync_error"),
            ],
        )?;
    }
    Ok(())
}

fn import_embedding_indexes(
    conn: &Connection,
    archive_dir: &Path,
    new_lib_id: i64,
) -> Result<(std::collections::HashMap<i64, i64>, Option<i64>)> {
    let path = archive_dir.join("embedding_indexes.json");
    if !path.exists() {
        return Ok((std::collections::HashMap::new(), None));
    }
    let data = std::fs::read_to_string(&path).map_err(|e| {
        GBrainError::FileError(format!("cannot read embedding_indexes.json: {}", e))
    })?;
    let indexes: Vec<crate::kb::embedding_index::EmbeddingIndex> = serde_json::from_str(&data)
        .map_err(|e| {
            GBrainError::FileError(format!("cannot parse embedding_indexes.json: {}", e))
        })?;

    // 记住备份中哪个 index 是 active
    let old_active_id = indexes.iter().find(|i| i.is_active).map(|i| i.id);

    let mut id_map = std::collections::HashMap::new();
    let mut new_active_id: Option<i64> = None;
    for idx in &indexes {
        let old_id = idx.id;
        let new_id = crate::kb::embedding_index::create_embedding_index(
            conn,
            new_lib_id,
            &idx.provider,
            &idx.model,
            idx.dimensions,
            &idx.index_type,
        )?;
        if Some(old_id) == old_active_id {
            new_active_id = Some(new_id);
        }
        id_map.insert(old_id, new_id);
    }

    // 恢复 active 状态：优先使用备份中的 active index，否则激活第一个
    if let Some(new_id) = new_active_id {
        crate::kb::embedding_index::activate_index(conn, new_id)?;
    } else if let Some(&first_id) = id_map.values().next() {
        crate::kb::embedding_index::activate_index(conn, first_id)?;
        new_active_id = Some(first_id);
    }

    Ok((id_map, new_active_id))
}

fn import_folders(
    conn: &Connection,
    archive_dir: &Path,
    new_lib_id: i64,
) -> Result<std::collections::HashMap<i64, i64>> {
    let path = archive_dir.join("folders.json");
    if !path.exists() {
        return Ok(std::collections::HashMap::new());
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| GBrainError::FileError(format!("cannot read folders.json: {}", e)))?;
    let folders: Vec<serde_json::Map<String, serde_json::Value>> = serde_json::from_str(&data)
        .map_err(|e| GBrainError::FileError(format!("cannot parse folders.json: {}", e)))?;

    // 两阶段导入：先插入所有 folder（parent_id=NULL），再回填 parent_id
    let mut id_map = std::collections::HashMap::new();
    for folder in &folders {
        let old_id = folder.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        conn.execute(
            "INSERT INTO kb_folders (library_id, parent_id, name, sort_order) \
             VALUES (?1, NULL, ?2, ?3)",
            params![
                new_lib_id,
                json_str(folder, "name"),
                json_int(folder, "sort_order"),
            ],
        )?;
        id_map.insert(old_id, conn.last_insert_rowid());
    }

    // 回填 parent_id：用 id_map 重映射
    for folder in &folders {
        let old_id = folder.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        if let Some(&new_id) = id_map.get(&old_id) {
            if let Some(old_parent) = json_null_or_int(folder, "parent_id") {
                if let Some(&new_parent) = id_map.get(&old_parent) {
                    conn.execute(
                        "UPDATE kb_folders SET parent_id = ?1 WHERE id = ?2",
                        params![new_parent, new_id],
                    )?;
                }
            }
        }
    }
    Ok(id_map)
}

/// Helper: extract string from JSON map, default empty
fn json_str(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> String {
    map.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Helper: extract i64 from JSON map, default 0
fn json_int(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> i64 {
    map.get(key).and_then(|v| v.as_i64()).unwrap_or(0)
}

/// Helper: extract Option<i64> from JSON map (null → None)
fn json_null_or_int(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<i64> {
    map.get(key)
        .and_then(|v| if v.is_null() { None } else { v.as_i64() })
}

/// Helper: extract Option<String> from JSON map (null 或不存在 → None)
fn json_opt_str(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    map.get(key).and_then(|v| {
        if v.is_null() {
            None
        } else {
            v.as_str().map(|s| s.to_string())
        }
    })
}

/// 从 embeddings.json 推断向量维度：优先取 `dimensions` 字段，否则从第一条 embedding blob 计算
fn infer_embedding_dimensions(archive_dir: &Path) -> Option<i32> {
    let path = archive_dir.join("embeddings.json");
    if !path.exists() {
        return None;
    }
    let data = std::fs::read_to_string(&path).ok()?;
    let embeddings: Vec<serde_json::Map<String, serde_json::Value>> =
        serde_json::from_str(&data).ok()?;

    // 优先取第一条 embedding 的 dimensions 字段
    if let Some(first) = embeddings.first() {
        if let Some(dims) = first.get("dimensions").and_then(|v| v.as_i64()) {
            if dims > 0 {
                return Some(dims as i32);
            }
        }
        // 回退：从 embedding_hex blob 长度推算
        let hex_str = first
            .get("embedding_hex")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !hex_str.is_empty() {
            if let Ok(blob) = hex::decode(hex_str) {
                if blob.len() % 4 == 0 && !blob.is_empty() {
                    return Some((blob.len() / 4) as i32);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_serialization() {
        let m = create_manifest(17, vec![1, 2], vec![], 10, 1024000);
        let json = serde_json::to_string(&m).unwrap();
        let restored: BackupManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.schema_version, 17);
        assert_eq!(restored.file_count, 10);
    }
}
