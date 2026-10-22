//! 不可变脚本快照与代次。
//!
//! 所有来源（内联 / path / saved / amend / hole fill）在启动前都复制到
//! `<project_root>/workflow-drafts/<run_id>/g<script_generation>.dwf.js`；
//! 执行、续跑、补洞只用快照 + hash，`source_path` 仅展示。
//!
//! 文件与 DB 的提交顺序（SQLite 事务不能原子提交文件）：
//! 1. 写临时文件并 fsync；
//! 2. 原子 rename 到最终路径；
//! 3. 提交 DB 事务（script_version + run.snapshot_*）。
//!
//! 2 之后 3 之前崩溃 → 文件存在但 DB 未引用，启动清理；3 之后两者一致。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::workflow_paths::{write_atomic, WorkflowPaths};

/// 快照 hash：FNV-1a 64（与引擎 script_hash 同算法，16 位十六进制）。
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 规范化脚本文本（CRLF → LF），保证跨平台重提同一脚本 hash 一致。
pub fn normalize_script(script: &str) -> String {
    script.replace("\r\n", "\n")
}

/// 写入一代不可变快照；返回（绝对路径，hash）。
pub fn write_snapshot(
    paths: &WorkflowPaths,
    run_id: &str,
    script_generation: u32,
    script: &str,
) -> Result<(PathBuf, String), String> {
    let normalized = normalize_script(script);
    let bytes = normalized.as_bytes();
    let hash = hash_bytes(bytes);
    let path = paths.snapshot_path(run_id, script_generation);
    write_atomic(&path, bytes)?;
    Ok((path, hash))
}

/// `read_snapshot` 的 hash 不一致错误前缀：调用方据此区分 §5.3 的 `script_mismatch`
/// （文件在但内容被改）与 `snapshot_missing`（缺失 / 不可读）。
pub const HASH_MISMATCH: &str = "快照 hash 不匹配";

/// 读取快照并校验 hash；缺失 / 不匹配返回 Err。
pub fn read_snapshot(path: &Path, expected_hash: &str) -> Result<String, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("快照 {} 不可读: {error}", path.display()))?;
    let hash = hash_bytes(&bytes);
    if !expected_hash.is_empty() && hash != expected_hash {
        return Err(format!(
            "{HASH_MISMATCH}：{}（期望 {expected_hash}，实际 {hash}）",
            path.display()
        ));
    }
    String::from_utf8(bytes).map_err(|error| format!("快照不是 UTF-8: {error}"))
}

/// 展示用相对路径（`workflow-drafts/<run>/gN.dwf.js`）。
pub fn display_path(paths: &WorkflowPaths, path: &Path) -> String {
    path.strip_prefix(&paths.project_root)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

/// 清理孤儿快照：无 DB 引用、且 mtime 超过 24h 的 `*.tmp-*` 与 `g*.dwf.js`。
///
/// `referenced` 是所有 run 行的 `snapshot_path`（绝对路径）。删除失败只告警——
/// 清理是尽力而为，不影响启动。
pub fn cleanup_orphans(paths: &WorkflowPaths, referenced: &HashSet<PathBuf>) {
    let cutoff = SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(24 * 60 * 60))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let drafts = paths.drafts_dir.clone();
    let Ok(entries) = std::fs::read_dir(&drafts) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            cleanup_dir(&path, referenced, cutoff);
        }
    }
}

fn cleanup_dir(dir: &Path, referenced: &HashSet<PathBuf>, cutoff: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut remaining = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let is_temp = name.contains(".tmp-") || name.ends_with(".tmp");
        let is_snapshot = name.starts_with('g') && name.ends_with(".dwf.js");
        if !(is_temp || is_snapshot) {
            remaining += 1;
            continue;
        }
        if referenced.contains(&path) {
            remaining += 1;
            continue;
        }
        let old_enough = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|modified| modified < cutoff)
            .unwrap_or(false);
        if !old_enough {
            remaining += 1;
            continue;
        }
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!("清理孤儿快照 {} 失败: {error}", path.display());
            remaining += 1;
        }
    }
    if remaining == 0 {
        let _ = std::fs::remove_dir(dir);
    }
}
