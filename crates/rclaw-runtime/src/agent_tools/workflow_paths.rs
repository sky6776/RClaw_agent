//! 工作流路径与项目模型（RClaw 特有）。
//!
//! `project_root = canonicalize(Layout.base)`：`read/list/grep/shell` 的沙箱根就是它。
//! RClaw 没有 per-session cwd（`TurnKey` 只有 channel/chat_id），同一进程的所有会话
//! 共享同一个项目根；`workflow_run.cwd` 列保存规范化后的 project root，`session_key`
//! 只表达归属。
//!
//! 路径规则：
//! - 草稿 / 快照：`<project_root>/workflow-drafts/<run_id>/g<N>.dwf.js`
//! - 项目存档：`<project_root>/workflows/<name>.dwf.js`
//! - 全局存档：`workflow_home()/workflows/<name>.dwf.js`
//! - 运行表：`<project_root>/state.sqlite`（Layout.db_file）
//! - 单实例锁：`<project_root>/workflow.lock`

use std::path::{Component, Path, PathBuf};

use rclaw_store::fs::Layout;

/// 工作流路径集合。
#[derive(Debug, Clone)]
pub struct WorkflowPaths {
    /// 文件系统路径（canonicalize 的原生形态；用于 resolve/IO）。
    pub project_root: PathBuf,
    /// 数据库键（去 verbatim 前缀 + Windows 小写）；只用于 `workflow_run.cwd` 比较。
    pub project_key: String,
    pub drafts_dir: PathBuf,
    pub saved_dir: PathBuf,
    pub global_saved_dir: PathBuf,
    pub artifacts_dir: PathBuf,
    pub lock_file: PathBuf,
}

impl WorkflowPaths {
    pub fn new(layout: &Layout) -> Self {
        let project_root = canonical_project_root(&layout.base);
        let project_key = normalize_project_key(&project_root);
        WorkflowPaths {
            drafts_dir: project_root.join("workflow-drafts"),
            saved_dir: project_root.join("workflows"),
            global_saved_dir: rclaw_store::workflow_home(Some(&project_root)).join("workflows"),
            artifacts_dir: project_root.join("workflow-artifacts"),
            lock_file: project_root.join("workflow.lock"),
            project_key,
            project_root,
        }
    }

    pub fn snapshot_path(&self, run_id: &str, script_generation: u32) -> PathBuf {
        self.drafts_dir
            .join(run_id)
            .join(format!("g{script_generation}.dwf.js"))
    }

    /// 创建运行所需目录（草稿 / 项目存档；全局存档按需创建）。
    pub fn ensure_dirs(&self) -> Result<(), String> {
        for dir in [&self.drafts_dir, &self.artifacts_dir, &self.project_root] {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("创建工作流目录 {} 失败: {error}", dir.display()))?;
        }
        Ok(())
    }
}

/// 项目根：canonicalize；路径不存在时以当前目录拼绝对路径（`Layout.base` 启动期已建）。
pub fn canonical_project_root(base: &Path) -> PathBuf {
    let absolute = if base.is_absolute() {
        base.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(base)
    };
    std::fs::canonicalize(&absolute).unwrap_or(absolute)
}

/// 项目键的字符串形态：Windows 去掉 `\\?\` 前缀并小写化（大小写不敏感文件系统）；
/// 其它平台保持原样。
pub fn normalize_project_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.strip_prefix("//?/").unwrap_or(&text).to_string();
    if cfg!(windows) {
        text.to_ascii_lowercase()
    } else {
        text
    }
}

/// 会话键：`channel/chat_id`（只表归属）。
pub fn session_key(channel: &str, chat_id: &str) -> String {
    format!("{channel}/{chat_id}")
}

/// 去掉 Windows verbatim 前缀（`\\?\C:\x` → `C:\x`、`\\?\UNC\srv\share` → `\\srv\share`）；
/// 其它形态原样返回。只用于词法比较。
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

/// 把用户提供的相对 / 绝对路径解析到项目根下，拒绝 symlink/junction 逃逸。
///
/// - `must_exist=true`：目标必须存在，canonicalize 后必须仍在根下；
/// - `must_exist=false`：允许目标不存在，但最长已存在父目录 canonicalize 后必须仍在根下
///   （剩余尾段必须全是普通名字，不能有 `..`）。
pub fn resolve_under(root: &Path, requested: &str, must_exist: bool) -> Result<PathBuf, String> {
    if requested.trim().is_empty() {
        return Err("path 为空".to_string());
    }
    let raw = Path::new(requested);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    // 先做词法规范化：拒绝越过根的 `..`。
    let mut lexical = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                if !lexical.pop() {
                    return Err("path 不能越出工作区".to_string());
                }
            }
            Component::CurDir => {}
            other => lexical.push(other.as_os_str()),
        }
    }
    // Windows 下 root 是 canonicalize 出来的 verbatim 形态（`\\?\C:\...`），用户给的绝对路径
    // 通常不是：词法比较两边都去掉 verbatim 前缀，否则项目内的绝对路径会被误拒。
    if !strip_verbatim(&lexical).starts_with(strip_verbatim(root)) {
        return Err(format!(
            "path 必须位于 {} 之下",
            root.to_string_lossy().replace('\\', "/")
        ));
    }
    if must_exist {
        let canonical =
            std::fs::canonicalize(&lexical).map_err(|error| format!("path 不可访问: {error}"))?;
        if !canonical.starts_with(root) {
            return Err("path 经符号链接指向工作区之外".to_string());
        }
        return Ok(canonical);
    }
    // 目标可能不存在：对最长已存在前缀 canonicalize，再拼回尾段。
    let mut prefix = lexical.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !prefix.exists() {
        let Some(name) = prefix.file_name().map(|name| name.to_os_string()) else {
            return Err("path 不可访问".to_string());
        };
        tail.push(name);
        if !prefix.pop() {
            return Err("path 不可访问".to_string());
        }
    }
    let canonical_prefix =
        std::fs::canonicalize(&prefix).map_err(|error| format!("path 不可访问: {error}"))?;
    if !canonical_prefix.starts_with(root) {
        return Err("path 经符号链接指向工作区之外".to_string());
    }
    let mut resolved = canonical_prefix;
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

/// 原子写文件：同目录临时文件 → fsync → rename；Unix 上对父目录 fsync。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| format!("路径没有父目录: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("创建目录 {} 失败: {error}", parent.display()))?;
    let stamp: u64 = rand::random();
    let tmp = parent.join(format!(
        ".{}.{stamp:016x}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workflow")
    ));
    let result = (|| -> Result<(), String> {
        let mut file = std::fs::File::create(&tmp)
            .map_err(|error| format!("创建临时文件 {} 失败: {error}", tmp.display()))?;
        file.write_all(bytes)
            .map_err(|error| format!("写入临时文件失败: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("fsync 临时文件失败: {error}"))?;
        drop(file);
        #[cfg(windows)]
        {
            // Windows rename 不覆盖：先备份旧文件，成功后再删除；失败则恢复。
            let backup = parent.join(format!(
                ".{}.{stamp:016x}.bak",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("workflow")
            ));
            if path.exists() {
                std::fs::rename(path, &backup)
                    .map_err(|error| format!("备份 {} 失败: {error}", path.display()))?;
            }
            match std::fs::rename(&tmp, path) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&backup);
                }
                Err(error) => {
                    if backup.exists() {
                        let _ = std::fs::rename(&backup, path);
                    }
                    return Err(format!("rename 到 {} 失败: {error}", path.display()));
                }
            }
        }
        #[cfg(not(windows))]
        {
            std::fs::rename(&tmp, path)
                .map_err(|error| format!("rename 到 {} 失败: {error}", path.display()))?;
        }
        #[cfg(unix)]
        {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rejects_parent_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        assert!(resolve_under(&root, "../escape.txt", false).is_err());
        assert!(resolve_under(&root, "ok.txt", false).is_ok());
    }

    #[test]
    fn project_key_strips_verbatim_prefix() {
        let key = normalize_project_key(Path::new(r"\\?\C:\Work\Agent"));
        if cfg!(windows) {
            assert_eq!(key, "c:/work/agent");
        } else {
            assert_eq!(key, "//?/C:/Work/Agent");
        }
    }
}
