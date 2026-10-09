//! 导入源和增量同步 (P6-001~P6-008)
//!
//! 支持本地目录 connector、增量扫描（new/unchanged/changed/missing）。

use crate::error::{GBrainError, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// 同步操作类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncAction {
    New,
    Unchanged,
    Changed,
    Missing,
}

/// 增量扫描结果摘要
#[derive(Debug, Clone, Serialize)]
pub struct SyncSummary {
    pub new_count: usize,
    pub changed_count: usize,
    pub missing_count: usize,
    pub unchanged_count: usize,
}

/// 扫描本地目录，返回文件路径列表
pub fn scan_directory(dir: &Path, allowed_extensions: &[&str]) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return Ok(files);
    }
    for entry in std::fs::read_dir(dir)
        .map_err(|e| GBrainError::FileError(format!("cannot read dir: {}", e)))?
    {
        let entry = entry.map_err(|e| GBrainError::FileError(format!("entry error: {}", e)))?;
        let path = entry.path();
        if path.is_dir() {
            if should_skip_dir(&path) {
                continue;
            }
            files.extend(scan_directory(&path, allowed_extensions)?);
        } else if path.is_file() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if allowed_extensions.contains(&ext.as_str()) {
                files.push(path);
            }
        }
    }
    Ok(files)
}

fn should_skip_dir(path: &Path) -> bool {
    // L11: 跳过隐藏目录（.开头）、node_modules、__pycache__，
    // 与常见 .gitignore 规则一致，避免扫描依赖和缓存目录。
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.') || n == "node_modules" || n == "__pycache__")
        .unwrap_or(false)
}

/// 计算文件内容哈希
pub fn compute_file_hash(path: &Path) -> Result<String> {
    let data = std::fs::read(path)
        .map_err(|e| GBrainError::FileError(format!("cannot read {}: {}", path.display(), e)))?;
    Ok(hex::encode(Sha256::digest(&data)))
}

/// 增量扫描：对比当前文件列表和上次记录，返回每项的操作类型
pub fn incremental_scan(
    conn: &Connection,
    source_id: i64,
    current_files: &[std::path::PathBuf],
) -> Result<Vec<(std::path::PathBuf, SyncAction, Option<String>)>> {
    let mut results = Vec::new();

    // 加载已有的 source items
    let mut existing: std::collections::HashMap<String, (String, Option<i64>)> =
        std::collections::HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT item_path, content_hash, document_id FROM kb_source_items WHERE source_id = ?1",
    )?;
    let rows = stmt.query_map(params![source_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<i64>>(2)?,
        ))
    })?;
    for (path, hash, doc_id) in rows.flatten() {
        existing.insert(path, (hash, doc_id));
    }

    // 对比当前文件
    let mut current_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    for file in current_files {
        let item_path = file.to_string_lossy().to_string();
        current_paths.insert(item_path.clone());

        if let Some((old_hash, _)) = existing.get(&item_path) {
            let new_hash = compute_file_hash(file)?;
            if &new_hash == old_hash {
                results.push((file.clone(), SyncAction::Unchanged, None));
            } else {
                results.push((file.clone(), SyncAction::Changed, Some(new_hash)));
            }
        } else {
            let new_hash = compute_file_hash(file)?;
            results.push((file.clone(), SyncAction::New, Some(new_hash)));
        }
    }

    // 检测 missing
    for (path, (_, _doc_id)) in &existing {
        if !current_paths.contains(path) {
            results.push((Path::new(path).to_path_buf(), SyncAction::Missing, None));
        }
    }

    Ok(results)
}

/// P6-005: 删除策略执行 — 对 missing 的文件执行对应操作
pub fn apply_delete_policy(
    conn: &Connection,
    item_path: &str,
    delete_policy: &str,
) -> Result<String> {
    match delete_policy {
        "soft_delete" => {
            // 根据 item_path 查找 document_id 并软删除
            if let Ok(Some(id)) = conn.query_row(
                "SELECT document_id FROM kb_source_items WHERE item_path=?1 AND document_id IS NOT NULL",
                params![item_path], |row| row.get::<_, Option<i64>>(0),
            ) {
                conn.execute(
                    "UPDATE kb_documents SET deleted_at=datetime('now'), document_status='deleted' WHERE id=?1",
                    params![id],
                )?;
                return Ok(format!("soft_deleted doc {}", id));
            }
            Ok("no_doc_found".into())
        }
        "mark_only" => Ok("marked_missing".into()),
        "ignore" => Ok("ignored".into()),
        _ => Ok("unknown_policy".into()),
    }
}

/// P6-006: 记录同步失败项
pub fn record_sync_failure(
    conn: &Connection,
    source_id: i64,
    item_path: &str,
    error: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE kb_source_items SET sync_status='failed', sync_error=?1 \
         WHERE source_id=?2 AND item_path=?3",
        params![error, source_id, item_path],
    )?;
    Ok(())
}

/// 汇总增量扫描结果
pub fn summarize_scan(results: &[(std::path::PathBuf, SyncAction, Option<String>)]) -> SyncSummary {
    let mut new_count = 0;
    let mut changed_count = 0;
    let mut missing_count = 0;
    let mut unchanged_count = 0;
    for (_, action, _) in results {
        match action {
            SyncAction::New => new_count += 1,
            SyncAction::Changed => changed_count += 1,
            SyncAction::Missing => missing_count += 1,
            SyncAction::Unchanged => unchanged_count += 1,
        }
    }
    SyncSummary {
        new_count,
        changed_count,
        missing_count,
        unchanged_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_hash() {
        let tmp = std::env::temp_dir().join("test_sync_hash.txt");
        std::fs::write(&tmp, "hello world").unwrap();
        let hash = compute_file_hash(&tmp).unwrap();
        assert_eq!(hash.len(), 64); // SHA-256 hex is 64 chars
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_summarize() {
        let results = vec![
            (Path::new("a.txt").to_path_buf(), SyncAction::New, None),
            (
                Path::new("b.txt").to_path_buf(),
                SyncAction::Unchanged,
                None,
            ),
            (Path::new("c.txt").to_path_buf(), SyncAction::Changed, None),
            (Path::new("d.txt").to_path_buf(), SyncAction::Missing, None),
        ];
        let summary = summarize_scan(&results);
        assert_eq!(summary.new_count, 1);
        assert_eq!(summary.unchanged_count, 1);
        assert_eq!(summary.changed_count, 1);
        assert_eq!(summary.missing_count, 1);
    }
}
