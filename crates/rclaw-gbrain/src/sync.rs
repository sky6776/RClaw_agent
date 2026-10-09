//! Git-based sync for brain pages
//! Mirrors gbrain's src/core/sync.ts
//!
//! Syncs markdown pages from a git repository into the brain.
//! Uses git CLI (not git2 crate) for simplicity.
//! Tracks sync state in a SyncManifest and logs failures as JSONL.

use crate::config::Config;
use crate::error::{GBrainError, Result};
use crate::types::PageType;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};
use unicode_normalization::UnicodeNormalization;

/// Sync manifest — tracks which files have been synced
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncManifest {
    /// Map of file path → (content_hash, last_synced_at)
    entries: std::collections::HashMap<String, SyncEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncEntry {
    content_hash: String,
    last_synced_at: String,
}

/// Sync failure record (JSONL format) with acknowledgment tracking
/// P2-2: Added acknowledged_at timestamp (mirrors TS sync.ts:162-163)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncFailure {
    pub path: String,
    pub slug: String,
    pub error: String,
    pub commit: Option<String>,
    pub line: Option<usize>,
    pub timestamp: String,
    pub acknowledged: bool,
    /// P2-2: When the failure was acknowledged (ISO 8601)
    pub acknowledged_at: Option<String>,
}

/// Record sync failures to JSONL log, deduplicating by (path, commit, error-hash)
/// P2-1: Use SHA-256 hash of error message instead of error.len() (mirrors TS _hashError)
pub fn record_sync_failures(failures: &[SyncFailure], failure_log_path: &Path) -> Result<()> {
    // Load existing failures for dedup
    let existing = load_sync_failures(failure_log_path);
    let mut existing_keys = std::collections::HashSet::new();
    for f in &existing {
        let key = format!(
            "{}:{}:{}",
            f.path,
            f.commit.as_deref().unwrap_or(""),
            hash_error(&f.error)
        );
        existing_keys.insert(key);
    }

    let new_failures: Vec<SyncFailure> = failures
        .iter()
        .filter(|f| {
            let key = format!(
                "{}:{}:{}",
                f.path,
                f.commit.as_deref().unwrap_or(""),
                hash_error(&f.error)
            );
            !existing_keys.contains(&key)
        })
        .cloned()
        .collect();

    append_failure_log(&new_failures, failure_log_path)
}

/// P2-1: Hash error message using SHA-256 (first 12 chars), mirrors TS _hashError(msg)
fn hash_error(msg: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(msg.as_bytes());
    format!("{:x}", hash).chars().take(12).collect()
}

/// Load all sync failures from JSONL log
pub fn load_sync_failures(failure_log_path: &Path) -> Vec<SyncFailure> {
    if !failure_log_path.exists() {
        return Vec::new();
    }
    let content = std::fs::read_to_string(failure_log_path).unwrap_or_default();
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Acknowledge all sync failures (mark as acknowledged)
/// Returns the number of failures acknowledged.
pub fn acknowledge_sync_failures(failure_log_path: &Path) -> Result<usize> {
    let mut failures = load_sync_failures(failure_log_path);
    let count = failures.iter().filter(|f| !f.acknowledged).count();

    for f in &mut failures {
        if !f.acknowledged {
            f.acknowledged = true;
            f.acknowledged_at = Some(chrono::Utc::now().to_rfc3339());
        }
    }

    // Rewrite the entire log atomically (write-temp-then-rename)
    if !failures.is_empty() {
        let temp_path = failure_log_path.with_extension("jsonl.tmp");
        {
            let mut file = std::fs::File::create(&temp_path).map_err(|e| {
                GBrainError::FileError(format!("Failed to create temp failure log: {}", e))
            })?;
            for failure in &failures {
                let line = serde_json::to_string(failure).map_err(|e| {
                    GBrainError::FileError(format!("Failed to serialize failure: {}", e))
                })?;
                use std::io::Write;
                file.write_all(line.as_bytes()).map_err(|e| {
                    GBrainError::FileError(format!("Failed to write failure log: {}", e))
                })?;
                file.write_all(b"\n").map_err(|e| {
                    GBrainError::FileError(format!("Failed to write newline: {}", e))
                })?;
            }
        }
        std::fs::rename(&temp_path, failure_log_path).map_err(|e| {
            GBrainError::FileError(format!("Failed to rename temp failure log: {}", e))
        })?;
    }

    Ok(count)
}

/// Get unacknowledged sync failures
pub fn unacknowledged_sync_failures(failure_log_path: &Path) -> Vec<SyncFailure> {
    load_sync_failures(failure_log_path)
        .into_iter()
        .filter(|f| !f.acknowledged)
        .collect()
}

/// Git sync result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSyncResult {
    pub synced: usize,
    pub skipped: usize,
    pub failed: usize,
    pub failures: Vec<SyncFailure>,
}

/// Sync a git repository of markdown files into the brain
///
/// Steps:
/// 1. git pull (if repo exists) or git clone
/// 2. Walk the repo for .md files
/// 3. For each file: compute content hash, compare with manifest
/// 4. If changed: parse markdown, call put_page via engine
/// 5. Update manifest with new hash
/// 6. Log any failures as JSONL
pub fn git_sync(
    repo_url: &str,
    local_path: &Path,
    manifest_path: &Path,
    failure_log_path: &Path,
    engine: &crate::sqlite_engine::SqliteEngine,
    config: &Config,
) -> Result<GitSyncResult> {
    info!(repo_url = %repo_url, local_path = %local_path.display(), "Starting git sync");

    // Step 1: Ensure repo is available
    if local_path.exists() {
        git_pull(local_path)?;
    } else {
        git_clone(repo_url, local_path)?;
    }

    // Step 2: Load manifest
    let manifest = load_manifest(manifest_path);

    // Step 3: Walk markdown and supported code files
    let import_files = collect_import_files(local_path);

    let mut synced = 0;
    let mut skipped = 0;
    let mut failed = 0;
    let mut failures: Vec<SyncFailure> = Vec::new();
    let mut updated_manifest = manifest.clone();

    for file_path in &import_files {
        let relative = file_path.strip_prefix(local_path).unwrap_or(file_path);
        let relative_str = relative.to_string_lossy().replace('\\', "/");

        // Compute content hash
        let content = std::fs::read_to_string(file_path).map_err(|e| {
            GBrainError::FileError(format!("Failed to read {}: {}", relative_str, e))
        })?;

        use sha2::{Digest, Sha256};
        let hash = format!("{:x}", Sha256::digest(content.as_bytes()));

        // Check if already synced with same hash
        if let Some(entry) = updated_manifest.entries.get(&relative_str) {
            if entry.content_hash == hash {
                debug!(path = %relative_str, "File unchanged, skipping");
                skipped += 1;
                continue;
            }
        }

        let is_code = is_code_file_path(&relative_str);
        let slug = if is_code {
            infer_code_slug_from_path(&relative_str)
        } else {
            infer_slug_from_path(&relative_str)
        };

        // Validate slug
        if crate::security::validate_page_slug(&slug).is_err() {
            warn!(path = %relative_str, slug = %slug, "Invalid slug, skipping");
            skipped += 1;
            continue;
        }

        let parsed = crate::markdown::parse_markdown(&content);
        if !is_code {
            if let Some(fm_slug) = parsed.frontmatter.get("slug").and_then(|v| v.as_str()) {
                if fm_slug != slug {
                    warn!(path = %relative_str, frontmatter_slug = %fm_slug, path_slug = %slug, "Slug mismatch, skipping");
                    skipped += 1;
                    continue;
                }
            }
        }
        let title = infer_title_from_path(&relative_str);
        let page_type = if is_code { Some(PageType::Code) } else { None };
        let import_content = if is_code {
            format!(
                "---\nfile: {}\nlanguage: {}\n---\n\n{}",
                relative_str,
                language_from_path(&relative_str).unwrap_or("text"),
                content
            )
        } else {
            content
        };

        // Connect engine if needed
        let ops = crate::operations::Operations::with_config(
            engine,
            crate::operations::OpContext {
                remote: false,
                working_dir: local_path.to_path_buf(),
                dry_run: false,
                subagent_id: None,
            },
            config.clone(),
        );

        let result = ops.put_page(&slug, &title, &import_content, page_type, Some(&hash));

        match result {
            Ok(_) => {
                debug!(slug = %slug, "Page synced");
                synced += 1;
                updated_manifest.entries.insert(
                    relative_str.clone(),
                    SyncEntry {
                        content_hash: hash,
                        last_synced_at: chrono::Utc::now().to_rfc3339(),
                    },
                );
            }
            Err(e) => {
                warn!(slug = %slug, error = %e, "Failed to sync page");
                failed += 1;
                failures.push(SyncFailure {
                    path: relative_str.clone(),
                    slug,
                    error: e.to_string(),
                    commit: None,
                    line: None,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    acknowledged: false,
                    acknowledged_at: None,
                });
            }
        }
    }

    // Step 5: Save updated manifest
    save_manifest(&updated_manifest, manifest_path)?;

    // Step 6: Record failures with dedup and bookmark gate
    if !failures.is_empty() {
        record_sync_failures(&failures, failure_log_path)?;
    }

    // Bookmark gate: don't advance sync.last_commit if unacknowledged failures exist
    let unack = unacknowledged_sync_failures(failure_log_path);
    if !unack.is_empty() {
        warn!(
            unack_count = unack.len(),
            "Unacknowledged sync failures exist — not advancing sync bookmark"
        );
    }

    info!(synced, skipped, failed, "Git sync complete");
    Ok(GitSyncResult {
        synced,
        skipped,
        failed,
        failures,
    })
}

/// P1-9: sync_brain — MCP-callable sync from a local Git repo path
/// Walks .md files, chunking/embedding new/changed pages, removing deleted ones.
/// Mirrors TS sync_brain operation.
/// `remote` should be true when called from MCP (untrusted callers), false for CLI.
pub fn sync_brain(
    engine: &crate::sqlite_engine::SqliteEngine,
    config: &Config,
    repo_path: &Path,
    force_full: bool,
    remote: bool,
) -> Result<GitSyncResult> {
    info!(path = %repo_path.display(), force_full, "Starting sync_brain");

    if !repo_path.exists() {
        return Err(GBrainError::FileError(format!(
            "Repository path does not exist: {}",
            repo_path.display()
        )));
    }

    // git pull if it's a git repo
    if repo_path.join(".git").exists() {
        git_pull(repo_path)?;
    }

    // Load manifest
    let manifest_path = repo_path.join(".gbrain-sync-manifest.json");
    let manifest = if force_full {
        SyncManifest::default()
    } else {
        load_manifest(&manifest_path)
    };

    // Walk markdown and supported code files
    let import_files = collect_import_files(repo_path);

    let mut synced = 0;
    let mut skipped = 0;
    let mut failed = 0;
    let mut failures: Vec<SyncFailure> = Vec::new();
    let mut updated_manifest = manifest.clone();

    for file_path in &import_files {
        let relative = file_path.strip_prefix(repo_path).unwrap_or(file_path);
        let relative_str = relative.to_string_lossy().replace('\\', "/");

        let content = match std::fs::read_to_string(file_path) {
            Ok(c) => c,
            Err(e) => {
                failed += 1;
                failures.push(SyncFailure {
                    path: relative_str.clone(),
                    slug: String::new(),
                    error: format!("Failed to read: {}", e),
                    commit: None,
                    line: None,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    acknowledged: false,
                    acknowledged_at: None,
                });
                continue;
            }
        };

        use sha2::{Digest, Sha256};
        let hash = format!("{:x}", Sha256::digest(content.as_bytes()));

        if !force_full {
            if let Some(entry) = updated_manifest.entries.get(&relative_str) {
                if entry.content_hash == hash {
                    debug!(path = %relative_str, "File unchanged, skipping");
                    skipped += 1;
                    continue;
                }
            }
        }

        let is_code = is_code_file_path(&relative_str);
        let slug = if is_code {
            infer_code_slug_from_path(&relative_str)
        } else {
            infer_slug_from_path(&relative_str)
        };

        if crate::security::validate_page_slug(&slug).is_err() {
            warn!(path = %relative_str, slug = %slug, "Invalid slug, skipping");
            skipped += 1;
            continue;
        }

        let parsed = crate::markdown::parse_markdown(&content);
        if !is_code {
            if let Some(fm_slug) = parsed.frontmatter.get("slug").and_then(|v| v.as_str()) {
                if fm_slug != slug {
                    warn!(path = %relative_str, frontmatter_slug = %fm_slug, path_slug = %slug, "Slug mismatch, skipping");
                    skipped += 1;
                    continue;
                }
            }
        }

        let title = infer_title_from_path(&relative_str);
        let page_type = if is_code { Some(PageType::Code) } else { None };
        let import_content = if is_code {
            format!(
                "---\nfile: {}\nlanguage: {}\n---\n\n{}",
                relative_str,
                language_from_path(&relative_str).unwrap_or("text"),
                content
            )
        } else {
            content
        };

        let ops = crate::operations::Operations::with_config(
            engine,
            crate::operations::OpContext {
                remote,
                working_dir: repo_path.to_path_buf(),
                dry_run: false,
                subagent_id: None,
            },
            config.clone(),
        );

        match ops.put_page(&slug, &title, &import_content, page_type, Some(&hash)) {
            Ok(_) => {
                debug!(slug = %slug, "Page synced");
                synced += 1;
                updated_manifest.entries.insert(
                    relative_str.clone(),
                    SyncEntry {
                        content_hash: hash,
                        last_synced_at: chrono::Utc::now().to_rfc3339(),
                    },
                );
            }
            Err(e) => {
                warn!(slug = %slug, error = %e, "Failed to sync page");
                failed += 1;
                failures.push(SyncFailure {
                    path: relative_str.clone(),
                    slug,
                    error: e.to_string(),
                    commit: None,
                    line: None,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    acknowledged: false,
                    acknowledged_at: None,
                });
            }
        }
    }

    save_manifest(&updated_manifest, &manifest_path)?;

    info!(synced, skipped, failed, "sync_brain complete");
    Ok(GitSyncResult {
        synced,
        skipped,
        failed,
        failures,
    })
}

/// P1-5: Build sync manifest from git diff (mirrors TS sync.ts:36-73)
/// Parses `git diff --name-status -M` output to identify added/modified/deleted/renamed files.
/// Returns lists of files to process and files to delete.
pub fn build_sync_manifest(repo_path: &Path) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let output = std::process::Command::new("git")
        .args(["diff", "--name-status", "-M", "HEAD"])
        .current_dir(repo_path)
        .output()
        .map_err(|e| GBrainError::FileError(format!("git diff failed: {}", e)))?;

    if !output.status.success() {
        // M34 修复：git diff 失败时记录警告，说明回退到全扫描的原因
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            stderr = %stderr,
            "git diff HEAD 失败（可能是首次提交无 HEAD），回退到全文件扫描"
        );
        let all_files = collect_import_files(repo_path);
        return Ok((all_files, Vec::new()));
    }

    let diff_output = String::from_utf8_lossy(&output.stdout);
    let mut changed_files = Vec::new();
    let mut deleted_slugs = Vec::new();

    for line in diff_output.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() < 2 {
            continue;
        }

        let status = parts[0].trim();
        let path = if status.starts_with('R') {
            // Renamed: status\told_path\tnew_path — use new path
            parts.get(2).unwrap_or(&parts[1])
        } else {
            parts[1]
        };

        if !path.ends_with(".md") && !is_code_file_path(path) {
            continue;
        }

        match status.chars().next() {
            Some('A') | Some('M') | Some('C') | Some('R') => {
                let full_path = repo_path.join(path);
                if full_path.exists() {
                    changed_files.push(full_path);
                }
            }
            Some('D') => {
                let slug = if is_code_file_path(path) {
                    infer_code_slug_from_path(path)
                } else {
                    slugify_path(path.trim_end_matches(".md"))
                };
                deleted_slugs.push(slug);
            }
            _ => {}
        }
    }

    info!(
        changed = changed_files.len(),
        deleted = deleted_slugs.len(),
        "Sync manifest built from git diff"
    );
    Ok((changed_files, deleted_slugs))
}

/// P1-6: Slugify a file path using Unicode NFD normalization
/// Mirrors TS sync.ts:102-126 — NFD decomposition + diacritic removal + lowercase + special char replacement
pub fn slugify_path(path: &str) -> String {
    let without_ext = path.trim_end_matches(".md");
    let segments: Vec<&str> = without_ext.split('/').collect();
    let slugified_segments: Vec<String> = segments
        .iter()
        .map(|seg| {
            // NFD decomposition + remove combining marks (diacritics)
            let nfd: String = seg.nfd().collect();
            let no_diacritics: String = nfd
                .chars()
                .filter(|c| {
                    // Filter out Unicode combining marks (category Mn, Mc, Me)
                    !is_combining_mark(*c)
                })
                .collect();
            // Lowercase
            let lower = no_diacritics.to_lowercase();
            // Replace non-alphanumeric with hyphens, collapse multiple hyphens
            let slugified: String = lower
                .chars()
                .map(|c: char| if c.is_alphanumeric() { c } else { '-' })
                .collect();
            // Trim leading/trailing hyphens, collapse consecutive hyphens
            let mut result = String::new();
            let mut prev_hyphen = false;
            for c in slugified.chars() {
                if c == '-' {
                    if !prev_hyphen && !result.is_empty() {
                        result.push(c);
                        prev_hyphen = true;
                    }
                } else {
                    result.push(c);
                    prev_hyphen = false;
                }
            }
            result.trim_end_matches('-').to_string()
        })
        .filter(|s| !s.is_empty())
        .collect();
    slugified_segments.join("/")
}

/// Check if a character is a Unicode combining mark (category Mn, Mc, Me)
fn is_combining_mark(c: char) -> bool {
    // Combining Diacritical Marks: U+0300..U+036F
    // Combining Diacritical Marks Extended: U+1AB0..U+1AFF
    // Combining Diacritical Marks Supplement: U+1DC0..U+1DFF
    // Combining Diacritical Marks for Symbols: U+20D0..U+20FF
    // Combining Half Marks: U+FE20..U+FE2F
    matches!(c,
        '\u{0300}'..='\u{036F}' |
        '\u{1AB0}'..='\u{1AFF}' |
        '\u{1DC0}'..='\u{1DFF}' |
        '\u{20D0}'..='\u{20FF}' |
        '\u{FE20}'..='\u{FE2F}'
    )
}

/// Infer slug from file path
/// P1-6: Now uses slugify_path for Unicode normalization
fn infer_slug_from_path(relative_path: &str) -> String {
    slugify_path(relative_path)
}

fn infer_code_slug_from_path(relative_path: &str) -> String {
    let without_ext = relative_path
        .rsplit_once('.')
        .map(|(base, _)| base)
        .unwrap_or(relative_path);
    format!("code/{}", slugify_path(without_ext))
}

fn is_code_file_path(path: &str) -> bool {
    language_from_path(path).is_some()
}

fn language_from_path(path: &str) -> Option<&'static str> {
    let ext = path.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    match ext {
        "rs" => Some("rust"),
        "ts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "py" => Some("python"),
        "go" => Some("go"),
        "java" => Some("java"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "cxx" | "hpp" => Some("cpp"),
        _ => None,
    }
}

/// Infer title from file path
/// e.g., "people/alice.md" → "Alice"
fn infer_title_from_path(relative_path: &str) -> String {
    let without_ext = relative_path.trim_end_matches(".md");
    let parts: Vec<&str> = without_ext.split('/').collect();
    // Use the last part as title, capitalized
    let name = parts.last().copied().unwrap_or("untitled");
    // Capitalize first letter
    let mut chars = name.chars();
    let first = chars
        .next()
        .unwrap_or('\0')
        .to_uppercase()
        .collect::<String>();
    let rest: String = chars.collect();
    format!("{}{}", first, rest)
}

/// Validate git URL to prevent dangerous transport protocols (ext::, fd::, file://, git::, etc.)
/// Only allows https://, http://, git@, and local paths.
/// Rejects ssh:// and git:// URLs containing `-` (could inject SSH options like -oProxyCommand=...).
fn validate_git_url(url: &str) -> Result<()> {
    // Block dangerous protocols first — these allow arbitrary command execution
    let dangerous = [
        "ext::",       // Git external transport — executes arbitrary commands
        "fd::",        // File-descriptor transport — can leak/redirect fds
        "file://",     // Local file access — can read arbitrary files via git hooks
        "git::",       // Git protocol proxy — can redirect to malicious hosts
        "git-remote-", // Custom remote helper prefix — executes arbitrary binaries
        "git-upload-pack",
        "git-receive-pack",
    ];
    if dangerous.iter().any(|d| url.contains(d)) {
        warn!(url = %url, "Dangerous git URL rejected");
        return Err(GBrainError::Security(format!(
            "Git URL contains dangerous transport: {}",
            url
        )));
    }

    // Only allow known-safe protocols
    let safe_protocols = ["https://", "http://", "git@"];
    let is_safe = safe_protocols.iter().any(|p| url.starts_with(p)) || url.starts_with('/');
    if !is_safe {
        return Err(GBrainError::Security(format!(
            "Git URL uses unsafe protocol: {}. Only https://, http://, git@, and local paths are allowed.",
            url
        )));
    }

    // For git@ SSH URLs, reject those where a hostname segment starts
    // with `-` which could inject SSH options (e.g., git@-oProxyCommand=evil).
    // Normal hyphens within hostnames like "gitlab.my-company.com" are safe.
    if url.starts_with("git@") {
        if let Some(colon_pos) = url.find(':') {
            let host_part = &url[4..colon_pos];
            if host_part.split('.').any(|segment| segment.starts_with('-')) {
                return Err(GBrainError::Security(format!(
                    "SSH git hostname segment starts with '-' (possible SSH option injection): {}",
                    url
                )));
            }
        } else {
            // git@ URL without colon is invalid — a valid SSH URL must have host:path
            // Reject to prevent SSH option injection (e.g., git@-oProxyCommand=evil)
            return Err(GBrainError::Security(format!(
                "SSH git URL missing colon separator (invalid format): {}",
                url
            )));
        }
    }

    Ok(())
}

/// git clone a repository
/// H15 fix: 使用 --depth=1 浅克隆，减少大型仓库的克隆时间和带宽消耗。
/// KB 同步场景只需要最新文件内容，不需要完整 git 历史。
/// H15 补充: 添加 120 秒超时，防止挂起的 git 进程无限期阻塞应用。
fn git_clone(url: &str, path: &Path) -> Result<()> {
    validate_git_url(url)?;
    info!(url = %url, path = %path.display(), "Cloning repository (shallow)");
    let output = git_run_with_timeout(
        &["clone", "--depth=1", url, &path.to_string_lossy()],
        None,
        120,
    )?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GBrainError::FileError(format!(
            "git clone failed: {}",
            stderr
        )));
    }
    Ok(())
}

/// git pull in an existing repository
/// H15 fix: 浅克隆仓库的 pull 使用 --depth=1 避免拉取完整历史
fn git_pull(path: &Path) -> Result<()> {
    info!(path = %path.display(), "Pulling repository");
    let output = git_run_with_timeout(&["pull", "--depth=1"], Some(path), 120)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GBrainError::FileError(format!(
            "git pull failed: {}",
            stderr
        )));
    }
    Ok(())
}

/// 带超时的 git 命令执行辅助函数
/// 在辅助线程中等待 git 进程完成，主线程设置超时。
/// 防止挂起的 git 进程（如等待凭据输入）无限期阻塞应用。
fn git_run_with_timeout(
    args: &[&str],
    current_dir: Option<&Path>,
    timeout_secs: u64,
) -> Result<std::process::Output> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // 禁用交互式凭据提示，避免 git 阻塞等待用户输入
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(dir) = current_dir {
        cmd.current_dir(dir);
    }

    let child = cmd
        .spawn()
        .map_err(|e| GBrainError::FileError(format!("git 启动失败: {}", e)))?;

    // 在辅助线程启动前提取 PID 作为超时 kill handle
    let pid = child.id();

    // 辅助线程拥有 Child 的全部所有权，用于 wait_with_output
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    let timeout = std::time::Duration::from_secs(timeout_secs);
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(GBrainError::FileError(format!("git 执行失败: {}", e))),
        Err(_) => {
            // 超时：通过 PID kill 子进程，不依赖 Child 所有权
            kill_process_by_pid(pid);
            // 等待线程回收子进程（wait_with_output 在 kill 后会很快返回）
            let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
            tracing::warn!(
                "git 命令超时（{}秒），已 kill 子进程 PID={}",
                timeout_secs,
                pid
            );
            Err(GBrainError::FileError(format!(
                "git 命令超时（{}秒），子进程已终止",
                timeout_secs
            )))
        }
    }
}

/// 通过 PID 终止进程及其子进程树（跨平台）
fn kill_process_by_pid(pid: u32) {
    #[cfg(windows)]
    {
        // Windows: taskkill /F /T /PID — /T 同时终止进程树中的子进程（如 ssh、credential helper）
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        // Unix: 先 kill 子进程（如 ssh、credential helper），再 kill 主进程
        let _ = std::process::Command::new("pkill")
            .args(["-9", "-P", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Collect markdown and supported code files from a directory, skipping hidden files.
fn collect_import_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk_import_dir(dir, &mut files);
    files.sort();
    debug!(dir = %dir.display(), total_count = files.len(), "Collected import files");
    files
}

fn walk_import_dir(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip hidden files/dirs
        if name_str.starts_with('.') {
            continue;
        }

        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() || meta.len() > 5 * 1024 * 1024 {
            // M46: 大文件（>5MB）静默跳过时记录 debug 日志，方便排查文件未同步的原因
            if meta.len() > 5 * 1024 * 1024 {
                debug!(path = %path.display(), size = meta.len(), "跳过超大文件（>5MB）");
            }
            continue;
        }
        if path.is_dir() {
            walk_import_dir(&path, files);
        } else if name_str.ends_with(".md") || is_code_file_path(&name_str) {
            files.push(path);
        }
    }
}

/// Load sync manifest from file (or return empty if not found)
fn load_manifest(path: &Path) -> SyncManifest {
    if path.exists() {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        SyncManifest::default()
    }
}

/// Save sync manifest to file
fn save_manifest(manifest: &SyncManifest, path: &Path) -> Result<()> {
    let content = serde_json::to_string_pretty(manifest)
        .map_err(|e| GBrainError::FileError(format!("Failed to serialize manifest: {}", e)))?;
    std::fs::write(path, content)
        .map_err(|e| GBrainError::FileError(format!("Failed to write manifest: {}", e)))?;
    debug!(path = %path.display(), entry_count = manifest.entries.len(), "Sync manifest saved");
    Ok(())
}

/// Append failure records to JSONL log file
fn append_failure_log(failures: &[SyncFailure], path: &Path) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| GBrainError::FileError(format!("Failed to open failure log: {}", e)))?;

    for failure in failures {
        let line = serde_json::to_string(failure)
            .map_err(|e| GBrainError::FileError(format!("Failed to serialize failure: {}", e)))?;
        use std::io::Write;
        file.write_all(line.as_bytes())
            .map_err(|e| GBrainError::FileError(format!("Failed to write failure log: {}", e)))?;
        file.write_all(b"\n")
            .map_err(|e| GBrainError::FileError(format!("Failed to write newline: {}", e)))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_slug() {
        assert_eq!(infer_slug_from_path("people/alice.md"), "people/alice");
        assert_eq!(infer_slug_from_path("notes.md"), "notes");
        assert_eq!(
            infer_slug_from_path("companies/acme-inc.md"),
            "companies/acme-inc"
        );
    }

    #[test]
    fn test_infer_title() {
        assert_eq!(infer_title_from_path("people/alice.md"), "Alice");
        assert_eq!(infer_title_from_path("notes.md"), "Notes");
    }

    #[test]
    fn test_manifest_roundtrip() {
        let mut manifest = SyncManifest::default();
        manifest.entries.insert(
            "people/alice.md".to_string(),
            SyncEntry {
                content_hash: "abc123".to_string(),
                last_synced_at: "2024-01-01T00:00:00Z".to_string(),
            },
        );

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let loaded: SyncManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries["people/alice.md"].content_hash, "abc123");
    }

    #[test]
    fn test_failure_jsonl() {
        let failure = SyncFailure {
            path: "people/alice.md".to_string(),
            slug: "people/alice".to_string(),
            error: "validation failed".to_string(),
            commit: Some("abc123".to_string()),
            line: Some(42),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            acknowledged: false,
            acknowledged_at: None,
        };

        let json = serde_json::to_string(&failure).unwrap();
        assert!(json.contains("people/alice.md"));
        assert!(json.contains("validation failed"));
        assert!(json.contains("abc123"));
    }

    #[test]
    fn test_validate_git_url_safe_protocols() {
        // https and http are allowed
        assert!(validate_git_url("https://github.com/user/repo.git").is_ok());
        assert!(validate_git_url("http://github.com/user/repo.git").is_ok());
        // git@ SSH URLs without dash are allowed
        assert!(validate_git_url("git@github.com:user/repo.git").is_ok());
        // Local paths are allowed
        assert!(validate_git_url("/home/user/repo").is_ok());
    }

    #[test]
    fn test_validate_git_url_dangerous_protocols() {
        // ext::, fd::, file://, git:: are blocked
        assert!(validate_git_url("ext::/usr/bin/git").is_err());
        assert!(validate_git_url("fd::17").is_err());
        assert!(validate_git_url("file:///etc/passwd").is_err());
        assert!(validate_git_url("git::http://evil.com/repo").is_err());
        // Custom remote helpers are blocked
        assert!(validate_git_url("git-remote-evil://host").is_err());
    }

    #[test]
    fn test_validate_git_url_ssh_option_injection() {
        // git@ URLs where a hostname segment starts with '-' could inject SSH options
        // (e.g., git@-oProxyCommand=evil). Hyphens within segments are safe
        // (e.g., git@gitlab.my-company.com).
        assert!(validate_git_url("git@-oProxyCommand=evil:user/repo.git").is_err());
        // Segment starting with dash in multi-part hostname
        assert!(validate_git_url("git@-evil.host.com:user/repo.git").is_err());
        // Normal hyphenated hostnames are now allowed
        assert!(validate_git_url("git@gitlab.my-company.com:user/repo.git").is_ok());
        assert!(validate_git_url("git@github.com:user/repo.git").is_ok());
    }

    #[test]
    fn test_validate_git_url_unsafe_protocols() {
        // git:// and ssh:// are no longer in the safe list
        assert!(validate_git_url("git://github.com/user/repo.git").is_err());
        assert!(validate_git_url("ssh://user@host/repo.git").is_err());
        // Unknown protocols are blocked
        assert!(validate_git_url("ftp://example.com/repo.git").is_err());
    }

    #[test]
    fn test_unacknowledged_failures() {
        let dir = std::env::temp_dir().join("gbrain_sync_test");
        let _ = std::fs::create_dir_all(&dir);
        let log_path = dir.join("failures.jsonl");

        let failure = SyncFailure {
            path: "test.md".to_string(),
            slug: "test".to_string(),
            error: "error".to_string(),
            commit: None,
            line: None,
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            acknowledged: false,
            acknowledged_at: None,
        };

        record_sync_failures(&[failure], &log_path).unwrap();
        let unack = unacknowledged_sync_failures(&log_path);
        assert_eq!(unack.len(), 1);

        acknowledge_sync_failures(&log_path).unwrap();
        let unack = unacknowledged_sync_failures(&log_path);
        assert_eq!(unack.len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
