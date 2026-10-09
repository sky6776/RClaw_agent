//! KB 上传安全验证
//!
//! 复用 gbrain_rs 现有的路径/文件名验证，并添加 KB 特定检查。

use crate::error::{GBrainError, Result};
use crate::security::validate_upload_path;
use std::io::Cursor;
use std::path::Path;

/// Default max file size: 50 MB
pub const DEFAULT_MAX_FILE_SIZE_BYTES: usize = 50 * 1024 * 1024;

/// Validate a KB upload source path.
/// - Checks path traversal, null bytes
/// - For remote callers: confines to working directory
/// - Validates extension against the provided allowed_extensions list
/// - Checks file size
pub fn validate_upload_source(
    path: &Path,
    remote: bool,
    working_dir: &Path,
    max_file_bytes: usize,
    allowed_extensions: &[String],
) -> Result<std::path::PathBuf> {
    // 1. Path security (reuses existing gbrain_rs validation)
    validate_upload_path(path, remote, working_dir)?;

    // 2. File must exist
    if !path.exists() {
        return Err(GBrainError::FileError("file does not exist".to_string()));
    }

    // 3. Must be a file, not a directory
    if path.is_dir() {
        return Err(GBrainError::InvalidInput(
            "path is a directory, not a file".to_string(),
        ));
    }

    // 4. Extension validation
    validated_extension_with(path, allowed_extensions)?;

    // 5. File size check
    let metadata = std::fs::metadata(path)
        .map_err(|e| GBrainError::FileError(format!("cannot read file metadata: {}", e)))?;
    if metadata.len() as usize > max_file_bytes {
        return Err(GBrainError::InvalidInput(format!(
            "file size {} bytes exceeds maximum {} bytes",
            metadata.len(),
            max_file_bytes
        )));
    }

    Ok(path.to_path_buf())
}

/// 获取并验证文件扩展名，按配置的允许列表检查。
pub fn validated_extension_with(path: &Path, allowed_extensions: &[String]) -> Result<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| GBrainError::InvalidInput("file must have an extension".to_string()))?;

    let ext_lower = ext.to_lowercase();
    if !allowed_extensions.iter().any(|a| a == &ext_lower) {
        return Err(GBrainError::InvalidInput(format!(
            "file extension '.{}' not allowed for KB upload. Allowed: {:?}",
            ext_lower, allowed_extensions
        )));
    }

    Ok(ext_lower)
}

/// 从文件内容检测并验证 MIME 类型。
/// 使用 `infer` crate 进行基于内容的检测，回退到基于扩展名的推断。
/// 对于二进制格式（pdf, docx, xls, xlsx, png, jpg），内容检测的 MIME 与扩展名不匹配时直接拒绝。
/// 对于文本格式（txt, md, csv, html），允许回退到扩展名推断（因为 infer 对短文本检测不准）。
pub fn detect_and_validate_mime(data: &[u8], ext: &str) -> Result<String> {
    // 先尝试基于内容的检测
    if let Some(kind) = infer::get(data) {
        let detected = kind.mime_type().to_string();
        if mime_matches_extension(&detected, ext) {
            // DOCX/XLSX: 始终验证 ZIP 内部结构，防止任意 ZIP 伪装
            if matches!(ext, "docx" | "xlsx") {
                validate_zip_structure(data, ext)?;
            }
            return Ok(detected);
        }
        // 二进制格式 MIME 不匹配：直接拒绝（防止伪装文件攻击）
        if is_binary_extension(ext) {
            return Err(GBrainError::InvalidInput(format!(
                "MIME 类型不匹配: 检测到 '{}' 但扩展名 '{}' 暗示不同格式，二进制文件不允许回退",
                detected, ext
            )));
        }
        // 文本格式 MIME 不匹配：允许回退到扩展名推断
        tracing::warn!(
            "MIME 类型不匹配: 检测到 '{}' 但扩展名 '{}' 暗示不同格式，文本格式允许回退",
            detected,
            ext
        );
    } else if is_binary_extension(ext) {
        // 无法识别内容的二进制格式：直接拒绝，不允许按扩展名放行
        return Err(GBrainError::InvalidInput(format!(
            "无法识别文件内容类型，二进制格式 '.{}' 不允许按扩展名放行",
            ext
        )));
    }

    // 回退到基于扩展名的 MIME（仅文本格式）
    Ok(crate::kb::types::mime_type_for_ext(ext).to_string())
}

/// 判断扩展名是否属于二进制格式（需要严格 MIME 校验）
fn is_binary_extension(ext: &str) -> bool {
    matches!(
        ext,
        "pdf" | "docx" | "xls" | "xlsx" | "png" | "jpg" | "jpeg"
    )
}

/// 验证 ZIP 内部结构，防止任意 ZIP 伪装为 DOCX/XLSX。
/// DOCX 必须包含 `word/document.xml`，XLSX 必须包含 `xl/workbook.xml`。
fn validate_zip_structure(data: &[u8], ext: &str) -> Result<()> {
    let required_entry = match ext {
        "docx" => "word/document.xml",
        "xlsx" => "xl/workbook.xml",
        _ => return Ok(()),
    };

    let reader = Cursor::new(data);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| {
        GBrainError::InvalidInput(format!(
            "无法解析为有效的 ZIP 文件 (扩展名 '.{}'): {}",
            ext, e
        ))
    })?;

    let mut found = false;
    for i in 0..archive.len() {
        if let Ok(file) = archive.by_index(i) {
            if file.name() == required_entry {
                found = true;
                break;
            }
        }
    }

    if !found {
        return Err(GBrainError::InvalidInput(format!(
            "ZIP 内部结构不匹配扩展名 '.{}': 缺少必需文件 '{}'，可能不是有效的 {} 文件",
            ext,
            required_entry,
            ext.to_uppercase()
        )));
    }

    Ok(())
}

/// 判断检测到的 MIME 类型是否与文件扩展名匹配
fn mime_matches_extension(mime: &str, ext: &str) -> bool {
    match ext {
        "pdf" => mime == "application/pdf",
        "docx" => mime.contains("openxmlformats") || mime == "application/zip",
        "xls" => mime == "application/vnd.ms-excel",
        "xlsx" => mime.contains("openxmlformats") || mime == "application/zip",
        "csv" => mime.starts_with("text/"),
        "html" | "htm" => mime == "text/html",
        "txt" => mime.starts_with("text/"),
        "md" => mime.starts_with("text/"),
        "png" => mime == "image/png",
        "jpg" | "jpeg" => mime == "image/jpeg",
        _ => false, // 拒绝未知扩展名 (安全边界应默认拒绝)
    }
}

/// Store a KB file to controlled storage directory.
/// Copies file to $GBRAIN_DIR/kb/files/<library_id>/<hash>.<ext>
pub fn store_kb_file(
    library_id: i64,
    content_hash: &str,
    ext: &str,
    data: &[u8],
    base_dir: &Path,
) -> Result<String> {
    let kb_dir = base_dir
        .join("kb")
        .join("files")
        .join(library_id.to_string());
    std::fs::create_dir_all(&kb_dir)
        .map_err(|e| GBrainError::FileError(format!("cannot create KB storage dir: {}", e)))?;

    let filename = format!("{}.{}", content_hash, ext);
    let storage_path = kb_dir.join(&filename);

    // Only write if file doesn't already exist (dedup by hash)
    if !storage_path.exists() {
        std::fs::write(&storage_path, data)
            .map_err(|e| GBrainError::FileError(format!("cannot write KB file: {}", e)))?;
    }

    // Return the storage path as string
    Ok(storage_path.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_binary_extensions_are_detected() {
        assert!(is_binary_extension("xls"));
        assert!(is_binary_extension("png"));
        assert!(is_binary_extension("jpg"));
        assert!(is_binary_extension("jpeg"));
    }

    #[test]
    fn strict_binary_mime_must_match_extension() {
        assert!(mime_matches_extension("application/vnd.ms-excel", "xls"));
        assert!(mime_matches_extension("image/png", "png"));
        assert!(mime_matches_extension("image/jpeg", "jpg"));
        assert!(mime_matches_extension("image/jpeg", "jpeg"));
        assert!(!mime_matches_extension("image/webp", "png"));
        assert!(!mime_matches_extension("image/png", "jpg"));
    }
}
