//! APK 桥执行器与技能脚本审批表（Android 移植阶段 2.2，crate 边界 ②）。
//!
//! - [`BridgePythonExecutor`]：实现 `rclaw_tools::PythonScriptExecutor`，经 JNI →
//!   `PythonRuntime.runScript` → `rclaw_runtime_bridge.run_script` 在内嵌 CPython 中
//!   执行 skill 脚本。四层挂死处置：严格串行化 → 协作中断（桥内 watchdog +
//!   Rust 侧 `interrupt_python` 双路线，0-E spike 2 结论待真机核验）→ poisoned
//!   标记（宁拒后不阻塞）→ 如实声明无硬 kill。
//! - [`SkillApprovalStore`]：脚本相对路径 → 内容 SHA-256 的审批表，落
//!   `noBackupFilesDir/rclaw-skill-approvals.json`；`baseline` 条目随 APK 内置
//!   副本自动刷新，`user` 条目升级保留。执行闸门保证「改码必须过人」。

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rclaw_tools::{PythonScriptExecutor, PythonScriptRequest, ScriptFailure, ScriptOutcome};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{call_python_bridge, interrupt_python_bridge};

pub(crate) const APPROVALS_FILE_NAME: &str = "rclaw-skill-approvals.json";
const APPROVALS_MAX_ENTRIES: usize = 512;
const APPROVED_SCRIPT_MAX_BYTES: u64 = 1024 * 1024;
/// Rust 侧放弃等待前，给协作中断留出的额外宽限（含桥内 watchdog 的重试周期）。
const COOPERATIVE_CANCEL_GRACE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// 审批表
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ApprovalEntry {
    /// APK 内置副本哈希；随 `refresh_baseline` 在每次启动时自动刷新。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    baseline: Option<String>,
    /// 用户经 GUI 人工批准的当前内容哈希；升级时保留。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ApprovalsFile {
    version: u32,
    entries: HashMap<String, ApprovalEntry>,
}

/// 技能脚本审批表：内存权威 + JSON 落盘（`noBackupFilesDir`）。
pub struct SkillApprovalStore {
    path: PathBuf,
    entries: Mutex<HashMap<String, ApprovalEntry>>,
}

impl SkillApprovalStore {
    /// 从磁盘加载（缺失/损坏 → 空表并告警；运行中目录由宿主保证存在）。
    pub fn load(path: PathBuf) -> Self {
        let entries = match std::fs::read_to_string(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "审批表读取失败，按空表处理");
                HashMap::new()
            }
            Ok(raw) => match serde_json::from_str::<ApprovalsFile>(&raw) {
                Err(error) => {
                    // 损坏（半截写入等）按空表重建 baseline；用户批准记录会丢，
                    // 必须留痕而不是静默。
                    tracing::warn!(%error, path = %path.display(), "审批表解析失败，按空表处理");
                    HashMap::new()
                }
                Ok(file) => {
                    if file.version != 1 {
                        tracing::warn!(version = file.version, "审批表版本未知，按空表处理");
                        HashMap::new()
                    } else {
                        file.entries
                    }
                }
            },
        };
        Self {
            path,
            entries: Mutex::new(entries),
        }
    }

    /// 以 APK 内置 skill 副本刷新 `baseline` 条目（升级安装后自动更新，
    /// 不会把内置 skill 误判失配）。刷新后落盘。
    pub fn refresh_baseline(&self, bundled_skills_dir: &Path) {
        let mut updates = Vec::new();
        collect_python_scripts(bundled_skills_dir, &mut updates);
        let mut entries = self.lock_entries();
        let mut changed = false;
        for script in updates {
            let Some(relative) = script.strip_prefix(bundled_skills_dir).ok() else {
                continue;
            };
            let Some(key) = relative_slash(relative) else {
                continue;
            };
            let sha = match script_sha256(&script) {
                Ok(sha) => sha,
                Err(error) => {
                    tracing::warn!(script = %key, %error, "跳过无法读取的内置脚本");
                    continue;
                }
            };
            let entry = entries.entry(key).or_default();
            if entry.baseline.as_deref() != Some(sha.as_str()) {
                entry.baseline = Some(sha);
                changed = true;
            }
        }
        if changed || entries.len() > APPROVALS_MAX_ENTRIES {
            entries.truncate_approval_entries();
            drop(entries);
            self.persist();
        }
    }

    /// 旧宿主（v0.2.0 Java 文件编辑器）迁移来的用户批准：仅在无 `user` 条目时
    /// 播种，不覆盖新表里已有的批准。
    pub fn seed_user_approvals(&self, legacy: &HashMap<String, String>) {
        if legacy.is_empty() {
            return;
        }
        let mut entries = self.lock_entries();
        let mut changed = false;
        for (key, sha) in legacy {
            let entry = entries.entry(key.clone()).or_default();
            if entry.user.is_none() {
                entry.user = Some(sha.clone());
                changed = true;
            }
        }
        if changed {
            entries.truncate_approval_entries();
            drop(entries);
            self.persist();
        }
    }

    /// 执行闸门：当前哈希 == `user` 哈希，或无 `user` 条目时 == `baseline` 哈希。
    /// 拒绝文案自描述（脚本路径 + 原因 + 批准入口），供模型向用户转述。
    pub fn check(&self, skills_dir: &Path, script: &Path) -> Result<(), String> {
        let relative = Self::relative_to_skills(skills_dir, script)?;
        let key = relative_slash(&relative)
            .ok_or_else(|| format!("skill 脚本相对路径无效: {}", script.display()))?;
        let actual = script_sha256(script)
            .map_err(|error| format!("读取 skill 脚本 {key} 失败: {error}"))?;
        let entries = self.lock_entries();
        match entries.get(&key) {
            Some(entry) => {
                if entry.user.as_deref() == Some(actual.as_str()) {
                    return Ok(());
                }
                if entry.user.is_none() && entry.baseline.as_deref() == Some(actual.as_str()) {
                    return Ok(());
                }
                if entry.user.is_some() {
                    Err(format!(
                        "skill 脚本 {key} 已被修改，与最近批准版本不一致；\
                         恢复执行需在 GUI「权限」页重新批准当前版本，或回滚到 APK 内置基线"
                    ))
                } else {
                    Err(format!(
                        "skill 脚本 {key} 不是 APK 内置版本，也没有用户批准记录；\
                         首次执行需在 GUI「权限」页批准当前版本"
                    ))
                }
            }
            None => Err(format!(
                "skill 脚本 {key} 不在审批表中（无 APK 内置基线，也无用户批准）；\
                 如确需执行，请在 GUI「权限」页人工批准"
            )),
        }
    }

    /// 脚本相对 skills/ 的路径。Android 上 `/data/user/0/<pkg>` 与 `/data/data/<pkg>`
    /// 互为别名（bind-mount/symlink 方向因 ROM 而异）：调用链一侧经 canonicalize
    /// 归一后前缀字符串必然失配——前缀不中时把双方都归一再试一次。
    fn relative_to_skills(skills_dir: &Path, script: &Path) -> Result<PathBuf, String> {
        if let Ok(relative) = script.strip_prefix(skills_dir) {
            return Ok(relative.to_path_buf());
        }
        let script_canon = script
            .canonicalize()
            .map_err(|_| format!("skill 脚本必须位于 skills/ 内: {}", script.display()))?;
        let skills_canon = skills_dir
            .canonicalize()
            .map_err(|_| format!("skill 脚本必须位于 skills/ 内: {}", script.display()))?;
        script_canon
            .strip_prefix(&skills_canon)
            .map(Path::to_path_buf)
            .map_err(|_| {
                format!(
                    "skill 脚本必须位于 skills/ 内: {}（解析为 {}）",
                    script.display(),
                    script_canon.display()
                )
            })
    }

    /// 写路径钩子入口：脚本被 write/edit/delete/patch 修改后摘除其 `user` 批准，
    /// 强制下次执行前重新过人。删除/回滚到基线内容的场景同样安全（baseline 判定
    /// 不受影响）。
    pub fn revoke_user_approval(&self, script_path: &Path) {
        let components_ok = script_path
            .components()
            .any(|component| component.as_os_str() == std::ffi::OsStr::new("skills"));
        if !components_ok {
            return;
        }
        let file_name = script_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let relative = script_path
            .components()
            .skip_while(|component| component.as_os_str() != std::ffi::OsStr::new("skills"))
            .skip(1)
            .collect::<PathBuf>();
        let Some(key) = relative_slash(&relative) else {
            return;
        };
        let mut entries = self.lock_entries();
        if let Some(entry) = entries.get_mut(&key) {
            if entry.user.take().is_some() {
                tracing::info!(script = %key, file = %file_name, "技能脚本已变更，摘除用户批准");
                drop(entries);
                self.persist();
            }
        }
    }

    /// GUI 权限页批准入口：写入/更新 `user` 哈希（crate 边界 ② 的对外审批 API，
    /// 阶段 4.6 的 tauri command 调这里）。
    pub fn approve_user_version(&self, skills_dir: &Path, script: &Path) -> Result<(), String> {
        let relative = script
            .strip_prefix(skills_dir)
            .map_err(|_| format!("skill 脚本必须位于 skills/ 内: {}", script.display()))?;
        let key = relative_slash(relative)
            .ok_or_else(|| format!("skill 脚本相对路径无效: {}", script.display()))?;
        let sha = script_sha256(script)
            .map_err(|error| format!("读取 skill 脚本 {key} 失败: {error}"))?;
        let mut entries = self.lock_entries();
        entries.entry(key).or_default().user = Some(sha);
        entries.truncate_approval_entries();
        drop(entries);
        self.persist();
        Ok(())
    }

    /// 待批准清单（GUI 权限页展示）：返回 skills 下内容与批准状态不符的脚本。
    pub fn pending_approvals(&self, skills_dir: &Path) -> Vec<PendingApproval> {
        let mut scripts = Vec::new();
        collect_python_scripts(skills_dir, &mut scripts);
        let entries = self.lock_entries();
        let mut pending = Vec::new();
        for script in scripts {
            let Ok(relative) = script.strip_prefix(skills_dir) else {
                continue;
            };
            let Some(key) = relative_slash(relative) else {
                continue;
            };
            let Ok(sha) = script_sha256(&script) else {
                continue;
            };
            let entry = entries.get(&key);
            let approved = match entry {
                Some(entry) => {
                    entry.user.as_deref() == Some(sha.as_str())
                        || (entry.user.is_none() && entry.baseline.as_deref() == Some(sha.as_str()))
                }
                None => false,
            };
            if !approved {
                pending.push(PendingApproval {
                    script: key,
                    has_baseline: entry.is_some_and(|entry| entry.baseline.is_some()),
                });
            }
        }
        pending.sort_by(|a, b| a.script.cmp(&b.script));
        pending.truncate(APPROVALS_MAX_ENTRIES);
        pending
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, ApprovalEntry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn persist(&self) {
        let entries = self.lock_entries().clone();
        let file = ApprovalsFile {
            version: 1,
            entries,
        };
        match serde_json::to_string_pretty(&file) {
            Ok(raw) => {
                if let Err(error) = std::fs::write(&self.path, raw) {
                    tracing::warn!(
                        path = %self.path.display(),
                        %error,
                        "技能审批表落盘失败（本会话内审批状态仍生效）"
                    );
                }
            }
            Err(error) => tracing::warn!(%error, "技能审批表序列化失败"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingApproval {
    /// 相对 skills/ 的路径（`/` 分隔）。
    pub script: String,
    /// 是否存在 APK 内置基线（决定 GUI 展示「批准」还是「批准/回滚」二选一）。
    pub has_baseline: bool,
}

trait TruncateApprovalEntries {
    fn truncate_approval_entries(&mut self);
}

impl TruncateApprovalEntries for HashMap<String, ApprovalEntry> {
    fn truncate_approval_entries(&mut self) {
        if self.len() > APPROVALS_MAX_ENTRIES {
            // 超限时优先丢无 user 批准的条目（baseline 可随启动重建）。
            let mut keys: Vec<String> = self
                .iter()
                .filter(|(_, entry)| entry.user.is_none())
                .map(|(key, _)| key.clone())
                .collect();
            keys.sort();
            for key in keys {
                if self.len() <= APPROVALS_MAX_ENTRIES {
                    break;
                }
                self.remove(&key);
            }
        }
    }
}

fn collect_python_scripts(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_python_scripts(&path, out);
        } else if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("py"))
        {
            out.push(path);
        }
    }
}

fn relative_slash(relative: &Path) -> Option<String> {
    relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => value.to_str().map(str::to_string),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

fn script_sha256(path: &Path) -> Result<String, String> {
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > APPROVED_SCRIPT_MAX_BYTES {
        return Err(format!(
            "技能脚本必须是不超过 {} 字节的普通文件",
            APPROVED_SCRIPT_MAX_BYTES
        ));
    }
    let digest = Sha256::digest(std::fs::read(path).map_err(|error| error.to_string())?);
    Ok(format!("{digest:x}"))
}

// ---------------------------------------------------------------------------
// 桥执行器
// ---------------------------------------------------------------------------

/// 经 JNI → Chaquopy 桥执行 skill Python 脚本的执行器。
pub struct BridgePythonExecutor {
    base: PathBuf,
    skills_dir: PathBuf,
    approvals: Arc<SkillApprovalStore>,
    /// 严格串行化：内嵌解释器进程内共享，同一时刻只允许一个 skill 脚本运行。
    execute_lock: tokio::sync::Mutex<()>,
    poisoned: AtomicBool,
}

impl BridgePythonExecutor {
    pub fn new(base: PathBuf, skills_dir: PathBuf, approvals: Arc<SkillApprovalStore>) -> Self {
        Self {
            base,
            skills_dir,
            approvals,
            execute_lock: tokio::sync::Mutex::new(()),
            poisoned: AtomicBool::new(false),
        }
    }

    /// 运行时是否已 poisoned（GUI 状态展示用）。
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Relaxed)
    }

    fn mark_poisoned(&self, reason: &str) {
        if !self.poisoned.swap(true, Ordering::Relaxed) {
            tracing::error!(
                %reason,
                "APK Python 运行时已标记为不可用（poisoned）；重启 RClaw 服务前技能执行将全部拒绝"
            );
        }
    }

    /// 测试专用：直接置 poisoned 标记。
    #[cfg(test)]
    pub(crate) fn mark_poisoned_for_test(&self) {
        self.mark_poisoned("test");
    }
}

#[derive(Debug, Deserialize)]
struct BridgeExecution {
    exit_code: i32,
    stdout: String,
    stderr: String,
    #[serde(default)]
    stdout_truncated: bool,
    #[serde(default)]
    stderr_truncated: bool,
}

#[async_trait]
impl PythonScriptExecutor for BridgePythonExecutor {
    async fn execute(
        &self,
        request: PythonScriptRequest<'_>,
    ) -> Result<ScriptOutcome, ScriptFailure> {
        if self.is_poisoned() {
            return Err(ScriptFailure::Unavailable(
                "Python 运行时已标记为不可用（此前一次执行超时未返回）；请重启 RClaw 服务恢复技能执行"
                    .to_string(),
            ));
        }
        // 执行闸门：审批通过才进解释器（改码必须过人）。
        if let Err(message) = self.approvals.check(&self.skills_dir, request.script) {
            return Err(ScriptFailure::Rejected(message));
        }

        // 层 1：严格串行化（进程内共享解释器的必然约束）。
        let _serial = self.execute_lock.lock().await;

        let args: Vec<String> = request
            .argv
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        let args_json = serde_json::to_string(&args)
            .map_err(|error| ScriptFailure::Spawn(format!("序列化 Python 参数失败: {error}")))?;
        let env_json = serde_json::to_string(&request.env)
            .map_err(|error| ScriptFailure::Spawn(format!("序列化执行环境失败: {error}")))?;
        let script = request.script.to_path_buf();
        let cwd = if request.cwd.as_os_str().is_empty() {
            self.base.clone()
        } else {
            request.cwd.to_path_buf()
        };
        let limit_bytes = request.max_output_bytes.min(u32::MAX as usize) as u64;
        // 桥内 watchdog 的中断时刻 ≈ Rust 侧超时；Rust 侧再留宽限等协作中断生效。
        let watchdog_ms = request.timeout.as_millis().min(u64::MAX as u128) as u64;
        let total_budget = request.timeout + COOPERATIVE_CANCEL_GRACE;

        // 层 2：JNI 调用放阻塞线程 + oneshot，超时后协作中断（双注入路线）。
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            let result = call_python_bridge(
                &script,
                &args_json,
                &cwd,
                &env_json,
                watchdog_ms,
                limit_bytes,
            );
            let _ = done_tx.send(result);
        });
        let mut done_rx = done_rx;
        match tokio::time::timeout(total_budget, &mut done_rx).await {
            Ok(Ok(result)) => {
                let raw = result.map_err(|error| ScriptFailure::Spawn(error.to_string()))?;
                let execution: BridgeExecution = serde_json::from_str(&raw).map_err(|error| {
                    ScriptFailure::Wait(format!("解析 Python 运行结果失败: {error}"))
                })?;
                Ok(execution.into_outcome())
            }
            Ok(Err(_dropped)) => {
                // spawn_blocking 结束前发送端已被丢弃：视为桥任务异常终止。
                self.mark_poisoned("Python 运行任务异常结束");
                let _ = task.await;
                Err(ScriptFailure::Wait(
                    "Python 运行任务异常结束；运行时已标记为不可用，需重启 RClaw 服务".to_string(),
                ))
            }
            Err(_elapsed) => {
                // 层 2（Rust 侧注入路线 B）：桥内 watchdog（路线 A）之外再补一次
                // 经 Java/Chaquopy 的中断；随后只等宽限期。
                interrupt_python_bridge();
                match tokio::time::timeout(COOPERATIVE_CANCEL_GRACE, &mut done_rx).await {
                    Ok(Ok(result)) => {
                        let raw =
                            result.map_err(|error| ScriptFailure::Spawn(error.to_string()))?;
                        let execution: BridgeExecution =
                            serde_json::from_str(&raw).map_err(|error| {
                                ScriptFailure::Wait(format!("解析 Python 运行结果失败: {error}"))
                            })?;
                        tracing::warn!("skill 脚本经协作中断终止（超时 {} ms）", watchdog_ms);
                        Ok(execution.into_outcome())
                    }
                    _ => {
                        // 层 3：放弃等待 + poisoned 标记（宁拒后不阻塞；线程泄漏与
                        // GIL 占用是已知代价，poisoned 阻断其向后续 skill 扩散）。
                        self.mark_poisoned("skill 执行超时且协作中断未生效");
                        Err(ScriptFailure::Unavailable(format!(
                            "skill 执行超过 {} ms 且无法中断；Python 运行时已标记为不可用，\
                             需重启 RClaw 服务恢复技能执行",
                            watchdog_ms
                        )))
                    }
                }
            }
        }
    }
}

impl BridgeExecution {
    fn into_outcome(self) -> ScriptOutcome {
        ScriptOutcome {
            exit_code: Some(self.exit_code),
            stdout: self.stdout,
            stderr: self.stderr,
            stdout_truncated: self.stdout_truncated,
            stderr_truncated: self.stderr_truncated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn store_dir(_tag: &str) -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn approvals_roundtrip_baseline_refresh_and_user_seed() {
        let dir = store_dir("approvals");
        let bundled = dir.path().join("bundled");
        let skills = dir.path().join("skills");
        write(&bundled.join("demo/scripts/run.py"), "print('v1')\n");
        write(&skills.join("demo/scripts/run.py"), "print('v1')\n");
        let store = SkillApprovalStore::load(dir.path().join(APPROVALS_FILE_NAME));

        // baseline 刷新：内置副本哈希入表，基线内容可直接执行。
        store.refresh_baseline(&bundled);
        assert!(store
            .check(&skills, &skills.join("demo/scripts/run.py"))
            .is_ok());

        // user 播种不覆盖既有；落盘后重载仍生效。
        let mut legacy = HashMap::new();
        legacy.insert(
            "demo/scripts/run.py".to_string(),
            script_sha256(&skills.join("demo/scripts/run.py")).unwrap(),
        );
        store.seed_user_approvals(&legacy);
        store.seed_user_approvals(&legacy);
        let reloaded = SkillApprovalStore::load(dir.path().join(APPROVALS_FILE_NAME));
        assert!(reloaded
            .check(&skills, &skills.join("demo/scripts/run.py"))
            .is_ok());
    }

    #[test]
    fn modified_script_is_rejected_until_reapproved() {
        let dir = store_dir("modified");
        let bundled = dir.path().join("bundled");
        let skills = dir.path().join("skills");
        write(&bundled.join("demo/scripts/run.py"), "print('v1')\n");
        write(&skills.join("demo/scripts/run.py"), "print('v1')\n");
        let store = SkillApprovalStore::load(dir.path().join(APPROVALS_FILE_NAME));
        store.refresh_baseline(&bundled);

        // 模型改写脚本（write/edit/patch 后钩子摘除批准；这里直接改内容）。
        write(&skills.join("demo/scripts/run.py"), "print('tampered')\n");
        let err = store
            .check(&skills, &skills.join("demo/scripts/run.py"))
            .unwrap_err();
        assert!(err.contains("demo/scripts/run.py"), "{err}");
        assert!(err.contains("批准") || err.contains("回滚"), "{err}");

        // 回滚到基线内容 → 恢复可执行。
        write(&skills.join("demo/scripts/run.py"), "print('v1')\n");
        assert!(store
            .check(&skills, &skills.join("demo/scripts/run.py"))
            .is_ok());

        // 用户批准自定义版本 → user 哈希生效。
        write(&skills.join("demo/scripts/run.py"), "print('custom')\n");
        store
            .approve_user_version(&skills, &skills.join("demo/scripts/run.py"))
            .unwrap();
        assert!(store
            .check(&skills, &skills.join("demo/scripts/run.py"))
            .is_ok());

        // 再被修改（钩子摘除 user）→ 拒绝（自定义内容失配基线，按无批准处理）。
        store.revoke_user_approval(&skills.join("demo/scripts/run.py"));
        let err = store
            .check(&skills, &skills.join("demo/scripts/run.py"))
            .unwrap_err();
        assert!(err.contains("demo/scripts/run.py"), "{err}");
        assert!(err.contains("批准") || err.contains("内置"), "{err}");
    }

    #[test]
    fn pending_approvals_list_unapproved_scripts() {
        let dir = store_dir("pending");
        let bundled = dir.path().join("bundled");
        let skills = dir.path().join("skills");
        write(&bundled.join("a/x.py"), "print('a')\n");
        write(&bundled.join("b/y.py"), "print('b')\n");
        write(&skills.join("a/x.py"), "print('a')\n");
        write(&skills.join("b/y.py"), "print('changed')\n");
        let store = SkillApprovalStore::load(dir.path().join(APPROVALS_FILE_NAME));
        store.refresh_baseline(&bundled);
        let pending = store.pending_approvals(&skills);
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!(pending[0].script, "b/y.py");
        assert!(pending[0].has_baseline);
    }

    #[tokio::test]
    async fn executor_rejects_unapproved_and_reports_poisoned() {
        let dir = store_dir("executor");
        let bundled = dir.path().join("bundled");
        let skills = dir.path().join("skills");
        write(&bundled.join("demo/scripts/run.py"), "print('v1')\n");
        write(&skills.join("demo/scripts/run.py"), "print('v1')\n");
        let store = Arc::new(SkillApprovalStore::load(
            dir.path().join(APPROVALS_FILE_NAME),
        ));
        store.refresh_baseline(&bundled);
        let executor = BridgePythonExecutor::new(dir.path().to_path_buf(), skills.clone(), store);

        let script = skills.join("demo/scripts/run.py");
        let argv = [std::ffi::OsString::from("--flag")];
        let env = [("RCLAW_SOURCE_CHANNEL".to_string(), "napcat".to_string())];
        let request = PythonScriptRequest {
            script: &script,
            argv: &argv,
            cwd: dir.path(),
            env: &env,
            timeout: Duration::from_secs(1),
            max_output_bytes: 64 * 1024,
        };

        // host（非 android）上桥不可用：审批通过后落到 Spawn 失败，文案原样保留。
        let failure = executor.execute(request).await.unwrap_err();
        match failure {
            ScriptFailure::Spawn(message) => {
                assert!(message.contains("Android"), "{message}")
            }
            other => panic!("期望 Spawn 失败，实际 {other:?}"),
        }

        // 审批拒绝：改写后未批准。
        write(&script, "print('tampered')\n");
        let request = PythonScriptRequest {
            script: &script,
            argv: &argv,
            cwd: dir.path(),
            env: &env,
            timeout: Duration::from_secs(1),
            max_output_bytes: 64 * 1024,
        };
        match executor.execute(request).await.unwrap_err() {
            ScriptFailure::Rejected(message) => assert!(message.contains("run.py"), "{message}"),
            other => panic!("期望 Rejected，实际 {other:?}"),
        }

        // poisoned：后续调用立即 Unavailable。
        executor.mark_poisoned_for_test();
        let request = PythonScriptRequest {
            script: &script,
            argv: &argv,
            cwd: dir.path(),
            env: &env,
            timeout: Duration::from_secs(1),
            max_output_bytes: 64 * 1024,
        };
        match executor.execute(request).await.unwrap_err() {
            ScriptFailure::Unavailable(message) => {
                assert!(message.contains("不可用"), "{message}")
            }
            other => panic!("期望 Unavailable，实际 {other:?}"),
        }
    }
}
