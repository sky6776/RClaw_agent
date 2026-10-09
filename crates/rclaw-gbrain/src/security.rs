//! Security validation — path, slug, filename
//! Mirrors gbrain's src/core/operations.ts validators

use crate::error::{GBrainError, Result};
use tracing::warn;

/// Allowed slug directory prefixes (from gbrain's DIR_PATTERN)
/// P1-9: Added meetings, deal, civic, source, media, yc, funds, area, event, industry
/// 修复：加入 documents 前缀 — shadow page 固定生成 documents/<slug>，
/// 但允许列表里没有 documents，导致 validate_page_slug 校验失败，
/// upload.rs 静默跳过 shadow page 创建，核心投影失效
const ALLOWED_SLUG_PREFIXES: &[&str] = &[
    "people",
    "companies",
    "deals",
    "topics",
    "concepts",
    "projects",
    "entities",
    "tech",
    "finance",
    "personal",
    "openclaw",
    "wiki",
    "writing",
    "meetings",
    "deal",
    "civic",
    "source",
    "media",
    "yc",
    "funds",
    "area",
    "event",
    "industry",
    "code",
    "documents",
];

/// Allowed file extensions for uploads
const ALLOWED_EXTENSIONS: &[&str] = &[
    "md", "txt", "pdf", "png", "jpg", "jpeg", "gif", "svg", "mp3", "mp4", "wav", "ogg", "m4a",
    "json", "yaml", "yml", "csv", "zip", "tar", "gz", "doc", "docx", "xls", "xlsx", "ppt", "pptx",
];

/// Validate an upload path
/// Checks for null bytes, path traversal, and (for remote callers) working directory confinement
pub fn validate_upload_path(
    path: &std::path::Path,
    remote: bool,
    working_dir: &std::path::Path,
) -> Result<()> {
    // 1. No null bytes
    if path.to_str().is_none_or(|s| s.contains('\0')) {
        return Err(GBrainError::Security("null byte in path".to_string()));
    }

    // 2. No path traversal
    if path.to_str().is_some_and(|s| s.contains("..")) {
        warn!(path = %path.display(), "Path traversal detected in upload path");
        return Err(GBrainError::Security("path traversal in path".to_string()));
    }

    // 3. Remote callers: must be under working directory
    if remote {
        // Reject symlinks BEFORE canonicalize (TOCTOU defense, mirrors validate_contained)
        reject_symlinks(path)?;
        let canonical = path
            .canonicalize()
            .map_err(|_| GBrainError::Security("path does not exist".to_string()))?;
        let cwd = working_dir
            .canonicalize()
            .map_err(|_| GBrainError::Security("cannot resolve working directory".to_string()))?;
        if !canonical.starts_with(&cwd) {
            warn!(path = %path.display(), working_dir = %working_dir.display(), "Remote upload path outside working directory");
            return Err(GBrainError::Security(
                "remote callers can only upload files within the working directory".to_string(),
            ));
        }
    }

    Ok(())
}

/// Validate a page slug
/// Checks format: prefix/name, prefix/name/sub, or just name, with allowed prefixes.
/// P1-8: Now allows multi-segment paths (e.g. people/alice/notes) to match link extraction.
pub fn validate_page_slug(slug: &str) -> Result<()> {
    // 1. No null bytes
    if slug.contains('\0') {
        return Err(GBrainError::Security("null byte in slug".to_string()));
    }

    // 2. No path traversal
    if slug.contains("..") {
        return Err(GBrainError::Security("path traversal in slug".to_string()));
    }

    // 3. No absolute paths
    if slug.starts_with('/') {
        return Err(GBrainError::Security(
            "absolute slug not allowed".to_string(),
        ));
    }

    // 4. No backslashes
    if slug.contains('\\') {
        return Err(GBrainError::Security("backslash in slug".to_string()));
    }

    // 5. No trailing slash
    if slug.ends_with('/') {
        return Err(GBrainError::Security("trailing slash in slug".to_string()));
    }

    // 6. No consecutive slashes
    if slug.contains("//") {
        return Err(GBrainError::Security(
            "consecutive slashes in slug".to_string(),
        ));
    }

    // 7. Format: prefix/name[/sub...] or just name
    // P1-8: Allow multi-segment paths; first segment must be in allowlist if present
    let parts: Vec<&str> = slug.split('/').collect();
    if parts.len() >= 2 && !ALLOWED_SLUG_PREFIXES.contains(&parts[0]) {
        return Err(GBrainError::Security(format!(
            "slug prefix '{}' not in allowlist",
            parts[0]
        )));
    }

    // 8. Each segment: lowercase, alphanumeric, hyphens only; must not be empty
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            return Err(GBrainError::Security(format!(
                "empty segment at position {} in slug",
                i
            )));
        }
        if !part
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(GBrainError::Security(
                "slug segments must be lowercase alphanumeric with hyphens".to_string(),
            ));
        }
    }

    Ok(())
}

/// Validate a filename for upload
/// Checks extension whitelist, no path separators, no traversal
pub fn validate_filename(name: &str) -> Result<()> {
    // 1. No null bytes
    if name.contains('\0') {
        return Err(GBrainError::Security("null byte in filename".to_string()));
    }

    // 2. No path separators
    if name.contains('/') || name.contains('\\') {
        return Err(GBrainError::Security(
            "path separator in filename".to_string(),
        ));
    }

    // 3. No path traversal
    if name.contains("..") {
        return Err(GBrainError::Security(
            "path traversal in filename".to_string(),
        ));
    }

    // 4. Extension must be in whitelist
    let path = std::path::Path::new(name);
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if !ALLOWED_EXTENSIONS.contains(&ext.to_lowercase().as_str()) {
            return Err(GBrainError::Security(format!(
                "file extension '.{}' not in allowlist",
                ext
            )));
        }
    } else {
        return Err(GBrainError::Security(
            "file must have an extension".to_string(),
        ));
    }

    // 5. Length limit
    if name.len() > 255 {
        return Err(GBrainError::Security(
            "filename too long (max 255 chars)".to_string(),
        ));
    }

    Ok(())
}

/// Validate that a resolved path is contained within a base directory.
/// Mirrors TS LocalStorage.contained() — prevents path traversal attacks.
/// Canonicalizes both the path and base_dir, then verifies the path starts with base_dir.
/// P1-10: Also rejects paths containing symlinks for remote callers (TOCTOU defense).
pub fn validate_contained(
    path: &std::path::Path,
    base_dir: &std::path::Path,
    remote: bool,
) -> Result<std::path::PathBuf> {
    // P1-10: Reject symlinks BEFORE canonicalize for remote callers (TOCTOU defense)
    if remote {
        reject_symlinks(path)?;
    }
    let canonical_base = base_dir.canonicalize().map_err(|e| {
        GBrainError::Security(format!(
            "Cannot resolve base directory {}: {}",
            base_dir.display(),
            e
        ))
    })?;
    let resolved = path.canonicalize().map_err(|e| {
        GBrainError::Security(format!("Cannot resolve path {}: {}", path.display(), e))
    })?;
    if !resolved.starts_with(&canonical_base) {
        return Err(GBrainError::Security(format!(
            "Path traversal detected: {} is outside {}",
            resolved.display(),
            canonical_base.display()
        )));
    }
    Ok(resolved)
}

/// Check that no component of the path is a symlink (TOCTOU defense).
/// For remote callers, we must reject paths containing symlinks because
/// the symlink target could change between validation and use.
fn reject_symlinks(path: &std::path::Path) -> Result<()> {
    let mut current = std::path::PathBuf::new();
    for component in path.components() {
        current.push(component);
        if let Ok(meta) = std::fs::symlink_metadata(&current) {
            if meta.is_symlink() || is_windows_reparse_point(&meta) {
                return Err(GBrainError::Security(format!(
                    "symlink or reparse point detected in path component: {}",
                    current.display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_windows_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_windows_reparse_point(_: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_slug_valid() {
        assert!(validate_page_slug("people/alice").is_ok());
        assert!(validate_page_slug("companies/acme-corp").is_ok());
        assert!(validate_page_slug("my-note").is_ok());
        // P1-9: new prefixes
        assert!(validate_page_slug("meetings/standup").is_ok());
        assert!(validate_page_slug("deal/series-a").is_ok());
        assert!(validate_page_slug("funds/sequoia-fund").is_ok());
        assert!(validate_page_slug("area/bay-area").is_ok());
        assert!(validate_page_slug("event/demo-day").is_ok());
        assert!(validate_page_slug("industry/fintech").is_ok());
    }

    #[test]
    fn test_validate_slug_multi_segment() {
        // P1-8: multi-segment paths should be accepted
        assert!(validate_page_slug("people/alice/notes").is_ok());
        assert!(validate_page_slug("companies/acme/projects").is_ok());
        assert!(validate_page_slug("deals/series-a/details").is_ok());
        // Each segment must still be valid
        assert!(validate_page_slug("people//notes").is_err()); // empty segment
        assert!(validate_page_slug("people/Alice/notes").is_err()); // uppercase
    }

    #[test]
    fn test_validate_slug_traversal() {
        assert!(validate_page_slug("people/../etc/passwd").is_err());
    }

    #[test]
    fn test_validate_slug_bad_prefix() {
        assert!(validate_page_slug("invalid/alice").is_err());
    }

    #[test]
    fn test_validate_slug_uppercase() {
        assert!(validate_page_slug("people/Alice").is_err());
    }

    #[test]
    fn test_validate_filename_valid() {
        assert!(validate_filename("photo.jpg").is_ok());
        assert!(validate_filename("document.pdf").is_ok());
    }

    #[test]
    fn test_validate_filename_traversal() {
        assert!(validate_filename("../../etc/passwd").is_err());
    }

    #[test]
    fn test_validate_filename_bad_extension() {
        assert!(validate_filename("evil.exe").is_err());
    }

    #[test]
    fn test_validate_filename_path_separator() {
        assert!(validate_filename("subdir/file.txt").is_err());
    }
}
