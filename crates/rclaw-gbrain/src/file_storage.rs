//! File storage operations
//! Mirrors gbrain's src/core/storage.ts

use crate::engine::BrainEngine;
use crate::error::{GBrainError, Result};
use crate::security::{
    validate_contained, validate_filename, validate_page_slug, validate_upload_path,
};
use crate::types::*;
use std::path::Path;

/// Upload a file to brain storage
pub fn upload_file<E: BrainEngine>(
    engine: &E,
    source_path: &Path,
    slug: &str,
    opts: FileUploadOptions,
    remote: bool,
    working_dir: &Path,
) -> Result<FileRecord> {
    validate_page_slug(slug)?;
    validate_upload_path(source_path, remote, working_dir)?;

    // L23: 纵深防御 — 远程调用方额外使用 validate_contained 做路径遍历防护。
    // validate_upload_path 已做基本校验，validate_contained 通过 canonicalize 两条路径
    // 并验证包含关系，防止符号链接或路径编码绕过。多层校验确保即使单层被绕过也不会泄露文件。
    if remote {
        validate_contained(source_path, working_dir, remote)?;
    }

    let filename = source_path
        .file_name()
        .ok_or_else(|| GBrainError::FileError("No filename".to_string()))?
        .to_string_lossy();

    validate_filename(&filename)?;

    engine.file_upload(source_path, slug, opts)
}

/// Read a file from brain storage, validating that the path is contained within the base directory.
/// Mirrors TS LocalStorage.contained() — prevents path traversal on file reads.
/// Note: Symlink rejection is not applied here since this is a read operation (no TOCTOU risk).
pub fn read_file(path: &Path, base_dir: &Path) -> Result<std::path::PathBuf> {
    validate_contained(path, base_dir, false)
}

/// List files in brain storage
pub fn list_files<E: BrainEngine>(
    engine: &E,
    slug: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<FileRecord>> {
    engine.file_list(slug, limit)
}

/// Get a file URL
pub fn get_file_url<E: BrainEngine>(engine: &E, file_id: i64, mode: FileUrlMode) -> Result<String> {
    engine.file_url(file_id, mode)
}
