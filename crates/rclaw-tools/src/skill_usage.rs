//! Skill provenance, usage telemetry, and deterministic curator lifecycle.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const USAGE_FILE: &str = ".usage.json";
const BUNDLED_MANIFEST_FILE: &str = ".bundled_manifest.json";
const CURATOR_STATE_FILE: &str = ".curator_state.json";
const SIDECAR_VERSION: u32 = 1;
const BUNDLED_MANIFEST_SOURCE: &str = include_str!("bundled_skills.json");

static SIDECAR_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static LIFECYCLE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static ACTIVE_SKILL_USERS: OnceLock<Mutex<BTreeMap<PathBuf, usize>>> = OnceLock::new();

fn sidecar_lock() -> &'static Mutex<()> {
    SIDECAR_LOCK.get_or_init(|| Mutex::new(()))
}

fn skill_activity_key(skills_dir: &Path) -> PathBuf {
    // TurnManager and AgentToolRuntime receive clones of the same configured Layout. Keeping that
    // lexical workspace identity also lets a background task register before an optional skills/
    // directory has been materialized; both the guard and Curator still use the identical key.
    skills_dir.to_path_buf()
}

pub(crate) fn lock_lifecycle() -> Result<MutexGuard<'static, ()>, String> {
    LIFECYCLE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "Skill lifecycle lock is poisoned".to_string())
}

/// Keeps deterministic lifecycle maintenance away from an asynchronous task that may read or
/// invoke Skills after its parent turn has ended.
pub struct SkillActivityGuard {
    key: PathBuf,
}

impl Drop for SkillActivityGuard {
    fn drop(&mut self) {
        let users = ACTIVE_SKILL_USERS.get_or_init(|| Mutex::new(BTreeMap::new()));
        let mut users = users
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = users.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                users.remove(&self.key);
            }
        }
    }
}

/// Register an asynchronous Skill consumer before it can outlive the foreground turn.
/// Lock ordering matches Curator operations: lifecycle, then the activity counter.
pub fn begin_skill_activity(skills_dir: &Path) -> Result<SkillActivityGuard, String> {
    let _lifecycle = lock_lifecycle()?;
    let key = skill_activity_key(skills_dir);
    let users = ACTIVE_SKILL_USERS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut users = users
        .lock()
        .map_err(|_| "Skill activity lock is poisoned".to_string())?;
    let count = users.entry(key.clone()).or_default();
    *count = count.saturating_add(1);
    Ok(SkillActivityGuard { key })
}

fn has_active_skill_users(skills_dir: &Path) -> Result<bool, String> {
    let key = skill_activity_key(skills_dir);
    let users = ACTIVE_SKILL_USERS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let users = users
        .lock()
        .map_err(|_| "Skill activity lock is poisoned".to_string())?;
    Ok(users.get(&key).copied().unwrap_or(0) > 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillOwner {
    Repository,
    User,
    Agent,
}

impl std::fmt::Display for SkillOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SkillOwner::Repository => "repository",
            SkillOwner::User => "user",
            SkillOwner::Agent => "agent",
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillState {
    #[default]
    Active,
    Stale,
    Archived,
}

impl std::fmt::Display for SkillState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SkillState::Active => "active",
            SkillState::Stale => "stale",
            SkillState::Archived => "archived",
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillLifecycleRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<SkillOwner>,
    #[serde(default)]
    pub state: SkillState,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_viewed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_patched_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_lifecycle_at: Option<String>,
    #[serde(default)]
    pub use_count: u64,
    #[serde(default)]
    pub view_count: u64,
    #[serde(default)]
    pub patch_count: u64,
    /// 技能参与的一轮执行（主 turn / 子代理任务）最终成功 / 失败的累计次数。
    /// 归因口径：该轮内读取过技能入口（read SKILL.md），turn 收尾时按整体成败记账。
    #[serde(default)]
    pub success_count: u64,
    #[serde(default)]
    pub failure_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_path: Option<String>,
}

impl SkillLifecycleRecord {
    pub fn latest_activity(&self) -> Option<DateTime<Utc>> {
        [
            self.last_used_at.as_deref(),
            self.last_viewed_at.as_deref(),
            self.last_patched_at.as_deref(),
            self.last_lifecycle_at.as_deref(),
            self.created_at.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .max()
    }

    /// 拉普拉斯平滑后的可靠性 `(rate, total)`：`(成功+1)/(成功+失败+2)`。
    ///
    /// 平滑保证小样本不极端化（0 次失败 ≠ 100%，1 次失败 ≠ 33%）；无任何结果样本时
    /// 返回 `None`（不参与索引标注与 curator 决策）。
    pub fn reliability(&self) -> Option<(f64, u64)> {
        let total = self.success_count.saturating_add(self.failure_count);
        if total == 0 {
            return None;
        }
        let rate = (self.success_count as f64 + 1.0) / (total as f64 + 2.0);
        Some((rate, total))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UsageDb {
    #[serde(default = "sidecar_version")]
    version: u32,
    #[serde(default)]
    skills: BTreeMap<String, SkillLifecycleRecord>,
}

impl Default for UsageDb {
    fn default() -> Self {
        Self {
            version: SIDECAR_VERSION,
            skills: BTreeMap::new(),
        }
    }
}

fn sidecar_version() -> u32 {
    SIDECAR_VERSION
}

#[derive(Debug, Default, Deserialize)]
struct BundledManifest {
    #[serde(default)]
    skills: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct SkillUsageStore {
    skills_dir: PathBuf,
}

impl SkillUsageStore {
    pub fn new(skills_dir: PathBuf) -> Self {
        Self { skills_dir }
    }

    pub fn skills_dir(&self) -> &Path {
        &self.skills_dir
    }

    fn usage_path(&self) -> PathBuf {
        self.skills_dir.join(USAGE_FILE)
    }

    fn validate_name(name: &str) -> Result<(), String> {
        if name.is_empty()
            || name.len() > 64
            || !name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            || !name.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.')
            })
        {
            return Err(format!("invalid Skill name: '{name}'"));
        }
        Ok(())
    }

    fn load_unlocked(&self) -> Result<UsageDb, String> {
        let path = self.usage_path();
        if !path.exists() {
            return Ok(UsageDb::default());
        }
        if !rclaw_store::resolve_within_base(&self.skills_dir, &path) {
            return Err(format!("{} resolves outside skills_dir", path.display()));
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|error| format!("read {} failed: {error}", path.display()))?;
        let db: UsageDb = serde_json::from_str(&raw)
            .map_err(|error| format!("parse {} failed: {error}", path.display()))?;
        if db.version != SIDECAR_VERSION {
            return Err(format!(
                "unsupported Skill telemetry schema version {} (expected {SIDECAR_VERSION})",
                db.version
            ));
        }
        Ok(db)
    }

    fn save_unlocked(&self, db: &UsageDb) -> Result<(), String> {
        std::fs::create_dir_all(&self.skills_dir)
            .map_err(|error| format!("create skills_dir failed: {error}"))?;
        let path = self.usage_path();
        if !rclaw_store::resolve_within_base(&self.skills_dir, &path) {
            return Err(format!("{} resolves outside skills_dir", path.display()));
        }
        let bytes = serde_json::to_vec_pretty(db)
            .map_err(|error| format!("serialize Skill telemetry failed: {error}"))?;
        rclaw_store::fs::atomic_write(&path, &bytes)
            .map_err(|error| format!("write {} failed: {error}", path.display()))
    }

    fn bundled_names_unlocked(&self) -> Result<BTreeSet<String>, String> {
        let embedded: BundledManifest = serde_json::from_str(BUNDLED_MANIFEST_SOURCE)
            .map_err(|error| format!("parse embedded repository Skill manifest failed: {error}"))?;
        // Runtime materialization is inspectable output, not an authority input. Otherwise a
        // generic file write could temporarily upgrade or downgrade ownership until restart.
        Ok(embedded.skills)
    }

    /// Materialize the compiled repository provenance manifest in the runtime Skill directory.
    /// Ownership checks also fall back to the embedded copy, so packaging that omits dotfiles
    /// remains fail-safe; this file makes the provenance policy inspectable at runtime.
    pub fn install_repository_manifest(&self) -> Result<(), String> {
        let _: BundledManifest = serde_json::from_str(BUNDLED_MANIFEST_SOURCE)
            .map_err(|error| format!("parse embedded repository Skill manifest failed: {error}"))?;
        let _guard = sidecar_lock()
            .lock()
            .map_err(|_| "Skill telemetry lock is poisoned".to_string())?;
        std::fs::create_dir_all(&self.skills_dir)
            .map_err(|error| format!("create skills_dir failed: {error}"))?;
        let path = self.skills_dir.join(BUNDLED_MANIFEST_FILE);
        if !rclaw_store::resolve_within_base(&self.skills_dir, &path) {
            return Err(format!("{} resolves outside skills_dir", path.display()));
        }
        if path.is_file()
            && std::fs::read_to_string(&path)
                .is_ok_and(|current| current == BUNDLED_MANIFEST_SOURCE)
        {
            return Ok(());
        }
        rclaw_store::fs::atomic_write(&path, BUNDLED_MANIFEST_SOURCE.as_bytes())
            .map_err(|error| format!("write {} failed: {error}", path.display()))
    }

    fn mutate<T>(
        &self,
        change: impl FnOnce(&mut UsageDb, &BTreeSet<String>) -> Result<T, String>,
    ) -> Result<T, String> {
        let _guard = sidecar_lock()
            .lock()
            .map_err(|_| "Skill telemetry lock is poisoned".to_string())?;
        let mut db = self.load_unlocked()?;
        let bundled = self.bundled_names_unlocked()?;
        let result = change(&mut db, &bundled)?;
        self.save_unlocked(&db)?;
        Ok(result)
    }

    fn read<T>(
        &self,
        query: impl FnOnce(&UsageDb, &BTreeSet<String>) -> Result<T, String>,
    ) -> Result<T, String> {
        let _guard = sidecar_lock()
            .lock()
            .map_err(|_| "Skill telemetry lock is poisoned".to_string())?;
        let db = self.load_unlocked()?;
        let bundled = self.bundled_names_unlocked()?;
        query(&db, &bundled)
    }

    fn effective_owner(
        name: &str,
        record: Option<&SkillLifecycleRecord>,
        bundled: &BTreeSet<String>,
    ) -> SkillOwner {
        if bundled.contains(name) {
            SkillOwner::Repository
        } else {
            record
                .and_then(|record| record.owner)
                .unwrap_or(SkillOwner::User)
        }
    }

    fn has_legacy_pin_marker(&self, name: &str) -> bool {
        let marker = self.skills_dir.join(name).join(".pinned");
        rclaw_store::resolve_within_base(&self.skills_dir, &marker) && marker.exists()
    }

    fn validate_archive_relative(name: &str, relative: &str) -> Result<(), String> {
        let relative_path = Path::new(relative);
        let mut components = relative_path.components();
        if !matches!(components.next(), Some(Component::Normal(part)) if part == OsStr::new(".archive"))
        {
            return Err(format!(
                "Skill '{name}' has an invalid recorded archive path: {relative}"
            ));
        }
        let Some(Component::Normal(entry)) = components.next() else {
            return Err(format!(
                "Skill '{name}' has an invalid recorded archive path: {relative}"
            ));
        };
        let Some(timestamp) = entry
            .to_str()
            .and_then(|entry| entry.strip_prefix(&format!("{name}-")))
        else {
            return Err(format!(
                "Skill '{name}' has an invalid recorded archive path: {relative}"
            ));
        };
        if components.next().is_some()
            || chrono::NaiveDateTime::parse_from_str(timestamp, "%Y%m%d-%H%M%S").is_err()
        {
            return Err(format!(
                "Skill '{name}' has an invalid recorded archive path: {relative}"
            ));
        }
        Ok(())
    }

    pub fn owner(&self, name: &str) -> Result<SkillOwner, String> {
        Self::validate_name(name)?;
        self.read(|db, bundled| Ok(Self::effective_owner(name, db.skills.get(name), bundled)))
    }

    /// Return provenance only when the name is already authoritative in the repository manifest
    /// or persisted lifecycle sidecar. An unknown missing name remains available for a new
    /// agent-owned Skill instead of being mistaken for an existing user-owned one.
    pub(crate) fn recorded_owner(&self, name: &str) -> Result<Option<SkillOwner>, String> {
        Self::validate_name(name)?;
        self.read(|db, bundled| {
            if bundled.contains(name) {
                Ok(Some(SkillOwner::Repository))
            } else {
                Ok(db
                    .skills
                    .get(name)
                    .map(|record| Self::effective_owner(name, Some(record), bundled)))
            }
        })
    }

    /// Resolve a real, addressable top-level Skill without following a directory redirect.
    pub fn live_skill_dir(&self, name: &str) -> Result<PathBuf, String> {
        Self::validate_name(name)?;
        let path = self.skills_dir.join(name);
        if crate::skill_manage::skill_path_is_redirect(&path) {
            return Err(format!(
                "Skill '{name}' is a symlink or junction and cannot be lifecycle-managed"
            ));
        }
        if !path.is_dir() || !path.join("SKILL.md").is_file() {
            return Err(format!(
                "Skill '{name}' does not exist or is not addressable"
            ));
        }
        if !rclaw_store::resolve_within_base(&self.skills_dir, &path) {
            return Err(format!("Skill '{name}' resolves outside skills_dir"));
        }
        Ok(path)
    }

    pub fn record(&self, name: &str) -> Result<SkillLifecycleRecord, String> {
        Self::validate_name(name)?;
        self.read(|db, bundled| {
            let mut record = db.skills.get(name).cloned().unwrap_or_default();
            record.owner = Some(Self::effective_owner(name, Some(&record), bundled));
            record.pinned |= self.has_legacy_pin_marker(name);
            Ok(record)
        })
    }

    pub fn records(&self) -> Result<BTreeMap<String, SkillLifecycleRecord>, String> {
        self.read(|db, bundled| {
            let mut records = db.skills.clone();
            if self.skills_dir.exists() {
                let entries = std::fs::read_dir(&self.skills_dir)
                    .map_err(|error| format!("read skills_dir failed: {error}"))?;
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir()
                        || crate::skill_manage::skill_path_is_redirect(&path)
                        || !rclaw_store::resolve_within_base(&self.skills_dir, &path)
                        || !path.join("SKILL.md").is_file()
                    {
                        continue;
                    }
                    if let Some(name) = entry.file_name().to_str() {
                        records.entry(name.to_string()).or_default();
                    }
                }
            }
            for (name, record) in &mut records {
                record.owner = Some(Self::effective_owner(name, Some(record), bundled));
                record.pinned |= self.has_legacy_pin_marker(name);
            }
            Ok(records)
        })
    }

    pub fn record_created(&self, name: &str, owner: SkillOwner) -> Result<(), String> {
        Self::validate_name(name)?;
        self.mutate(|db, bundled| {
            let record = db.skills.entry(name.to_string()).or_default();
            record.owner = Some(if bundled.contains(name) {
                SkillOwner::Repository
            } else {
                owner
            });
            record.state = SkillState::Active;
            record.last_lifecycle_at = Some(Utc::now().to_rfc3339());
            record
                .created_at
                .get_or_insert_with(|| Utc::now().to_rfc3339());
            record.archive_path = None;
            Ok(())
        })
    }

    /// A successful catalog read both views and uses the Skill.
    pub fn record_view_and_use(&self, name: &str) -> Result<(), String> {
        Self::validate_name(name)?;
        self.mutate(|db, _| {
            let now = Utc::now().to_rfc3339();
            let record = db.skills.entry(name.to_string()).or_default();
            record.view_count = record.view_count.saturating_add(1);
            record.use_count = record.use_count.saturating_add(1);
            record.last_viewed_at = Some(now.clone());
            record.last_used_at = Some(now);
            if record.state == SkillState::Stale {
                record.state = SkillState::Active;
            }
            Ok(())
        })
    }

    pub fn record_patch(&self, name: &str) -> Result<(), String> {
        Self::validate_name(name)?;
        self.mutate(|db, _| {
            let record = db.skills.entry(name.to_string()).or_default();
            record.patch_count = record.patch_count.saturating_add(1);
            record.last_patched_at = Some(Utc::now().to_rfc3339());
            if record.state == SkillState::Stale {
                record.state = SkillState::Active;
            }
            Ok(())
        })
    }

    /// 记录一次技能使用结果（整轮 turn / 子代理任务的最终成败）。
    ///
    /// 只做确定性计数，不改变 lifecycle 状态（复活仍以 read/view/use 为准——失败的
    /// 使用不应把技能标记回 Active）。
    pub fn record_outcome(&self, name: &str, success: bool) -> Result<(), String> {
        Self::validate_name(name)?;
        self.mutate(|db, _| {
            let record = db.skills.entry(name.to_string()).or_default();
            if success {
                record.success_count = record.success_count.saturating_add(1);
            } else {
                record.failure_count = record.failure_count.saturating_add(1);
            }
            record.last_outcome_at = Some(Utc::now().to_rfc3339());
            Ok(())
        })
    }

    pub fn forget(&self, name: &str) -> Result<(), String> {
        Self::validate_name(name)?;
        self.mutate(|db, _| {
            db.skills.remove(name);
            Ok(())
        })
    }

    pub fn set_pinned(&self, name: &str, pinned: bool) -> Result<(), String> {
        let _lifecycle = lock_lifecycle()?;
        self.live_skill_dir(name)?;
        let marker = self.skills_dir.join(name).join(".pinned");
        if !rclaw_store::resolve_within_base(&self.skills_dir, &marker) {
            return Err(format!("{} resolves outside skills_dir", marker.display()));
        }
        let removed_legacy_marker = if !pinned && marker.exists() {
            std::fs::remove_file(&marker)
                .map_err(|error| format!("remove legacy pin marker failed: {error}"))?;
            true
        } else {
            false
        };
        let result = self.mutate(|db, _| {
            let record = db.skills.entry(name.to_string()).or_default();
            record.pinned = pinned;
            record.last_lifecycle_at = Some(Utc::now().to_rfc3339());
            Ok(())
        });
        match result {
            Ok(()) => Ok(()),
            Err(error) if removed_legacy_marker => match std::fs::write(&marker, []) {
                Ok(()) => Err(error),
                Err(rollback) => Err(format!(
                    "{error}; restoring legacy pin marker also failed: {rollback}"
                )),
            },
            Err(error) => Err(error),
        }
    }

    pub fn adopt(&self, name: &str) -> Result<(), String> {
        let _lifecycle = lock_lifecycle()?;
        self.live_skill_dir(name)?;
        self.mutate(|db, bundled| {
            if bundled.contains(name) {
                return Err(format!(
                    "repository Skill '{name}' cannot be adopted by autonomous maintenance"
                ));
            }
            let record = db.skills.entry(name.to_string()).or_default();
            record.owner = Some(SkillOwner::Agent);
            record.state = SkillState::Active;
            record.last_lifecycle_at = Some(Utc::now().to_rfc3339());
            record
                .created_at
                .get_or_insert_with(|| Utc::now().to_rfc3339());
            Ok(())
        })
    }

    pub fn mark_archived(&self, name: &str, archive_path: &Path) -> Result<(), String> {
        Self::validate_name(name)?;
        let canonical_skills = self
            .skills_dir
            .canonicalize()
            .map_err(|error| format!("skills_dir is not readable: {error}"))?;
        let relative = archive_path
            .strip_prefix(&canonical_skills)
            .map_err(|_| "archive path is outside skills_dir".to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        Self::validate_archive_relative(name, &relative)?;
        self.mutate(|db, _| {
            let record = db.skills.entry(name.to_string()).or_default();
            record.state = SkillState::Archived;
            record.archive_path = Some(relative);
            record.last_lifecycle_at = Some(Utc::now().to_rfc3339());
            Ok(())
        })
    }

    pub fn mark_restored(&self, name: &str) -> Result<(), String> {
        Self::validate_name(name)?;
        self.mutate(|db, _| {
            let record = db.skills.entry(name.to_string()).or_default();
            record.state = SkillState::Active;
            record.archive_path = None;
            record.last_lifecycle_at = Some(Utc::now().to_rfc3339());
            Ok(())
        })
    }

    fn set_state(&self, name: &str, state: SkillState) -> Result<(), String> {
        Self::validate_name(name)?;
        self.mutate(|db, _| {
            db.skills.entry(name.to_string()).or_default().state = state;
            Ok(())
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CuratorConfig {
    pub enabled: bool,
    pub interval_hours: u64,
    pub stale_after_days: u64,
    pub archive_after_days: u64,
}

/// Curator 提前归档的可靠性下限：平滑成功率低于该值且样本充足时，视为「持续失败」。
const POOR_RELIABILITY_MAX_RATE: f64 = 0.34;
/// 触发提前归档所需的最小结果样本数（拉普拉斯平滑下 4 样本已能区分多数失败）。
const POOR_RELIABILITY_MIN_OUTCOMES: u64 = 4;

impl Default for CuratorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_hours: 168,
            stale_after_days: 30,
            archive_after_days: 90,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CuratorReport {
    pub skipped: bool,
    pub stale: Vec<String>,
    pub reactivated: Vec<String>,
    pub archived: Vec<String>,
    pub errors: Vec<String>,
}

impl CuratorReport {
    pub fn summary(&self) -> String {
        if self.skipped {
            return "Skill Curator: not due yet.".to_string();
        }
        format!(
            "Skill Curator: {} stale, {} reactivated, {} archived, {} errors.",
            self.stale.len(),
            self.reactivated.len(),
            self.archived.len(),
            self.errors.len()
        )
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CuratorState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_run_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SkillCurator {
    store: SkillUsageStore,
    config: CuratorConfig,
}

impl SkillCurator {
    pub fn new(skills_dir: PathBuf, config: CuratorConfig) -> Self {
        Self {
            store: SkillUsageStore::new(skills_dir),
            config,
        }
    }

    pub fn store(&self) -> &SkillUsageStore {
        &self.store
    }

    fn state_path(&self) -> PathBuf {
        self.store.skills_dir.join(CURATOR_STATE_FILE)
    }

    fn load_state(&self) -> Result<CuratorState, String> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(CuratorState::default());
        }
        if !rclaw_store::resolve_within_base(&self.store.skills_dir, &path) {
            return Err(format!("{} resolves outside skills_dir", path.display()));
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|error| format!("read {} failed: {error}", path.display()))?;
        serde_json::from_str(&raw)
            .map_err(|error| format!("parse {} failed: {error}", path.display()))
    }

    fn save_state(&self) -> Result<(), String> {
        let path = self.state_path();
        if !rclaw_store::resolve_within_base(&self.store.skills_dir, &path) {
            return Err(format!("{} resolves outside skills_dir", path.display()));
        }
        let state = CuratorState {
            last_run_at: Some(Utc::now().to_rfc3339()),
        };
        let bytes = serde_json::to_vec_pretty(&state)
            .map_err(|error| format!("serialize Curator state failed: {error}"))?;
        rclaw_store::fs::atomic_write(&path, &bytes)
            .map_err(|error| format!("write {} failed: {error}", path.display()))
    }

    pub fn run_if_due(&self) -> Result<CuratorReport, String> {
        let _lifecycle = lock_lifecycle()?;
        self.run_if_due_locked()
    }

    fn run_if_due_locked(&self) -> Result<CuratorReport, String> {
        if !self.config.enabled {
            return Ok(CuratorReport {
                skipped: true,
                ..CuratorReport::default()
            });
        }
        if has_active_skill_users(&self.store.skills_dir)? {
            return Ok(CuratorReport {
                skipped: true,
                ..CuratorReport::default()
            });
        }
        let state = self.load_state()?;
        if let Some(last_run) = state
            .last_run_at
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        {
            let last_run = last_run.with_timezone(&Utc);
            let now = Utc::now();
            let elapsed_hours = now.signed_duration_since(last_run).num_hours().max(0) as u64;
            // A clock rollback or corrupted future timestamp must not freeze maintenance until
            // that timestamp arrives. Large u64 intervals compare directly without constructing
            // a chrono Duration that could overflow.
            if last_run <= now && elapsed_hours < self.config.interval_hours {
                return Ok(CuratorReport {
                    skipped: true,
                    ..CuratorReport::default()
                });
            }
        }
        self.run_now_locked()
    }

    pub fn run_now(&self) -> Result<CuratorReport, String> {
        let _lifecycle = lock_lifecycle()?;
        self.run_now_locked()
    }

    fn run_now_locked(&self) -> Result<CuratorReport, String> {
        if has_active_skill_users(&self.store.skills_dir)? {
            return Err(
                "Skill Curator cannot run while an asynchronous agent is using Skills".to_string(),
            );
        }
        if self.config.archive_after_days < self.config.stale_after_days {
            return Err("archive_after_days must be >= stale_after_days".to_string());
        }
        let now = Utc::now();
        let records = self.store.records()?;
        let mut report = CuratorReport::default();
        for (name, record) in records {
            if let Err(error) = SkillUsageStore::validate_name(&name) {
                report.errors.push(error);
                continue;
            }
            if record.owner != Some(SkillOwner::Agent)
                || record.pinned
                || record.state == SkillState::Archived
            {
                continue;
            }
            let skill_dir = self.store.skills_dir.join(&name);
            if skill_dir.join(".protected").exists() || skill_dir.join(".pinned").exists() {
                continue;
            }
            let Some(last_activity) = record.latest_activity() else {
                continue;
            };
            let inactive_days = now.signed_duration_since(last_activity).num_days().max(0) as u64;
            // 可靠性提前归档：agent-owned 且已 Stale（闲置 ≥ stale_after_days）的技能，
            // 若结果样本充足且平滑成功率持续偏低（≈多数轮失败），不必等到完整
            // archive_after_days——它在真实使用中反复失败，留着只会持续产生坏结果与信号。
            let poor_reliability = record.reliability().is_some_and(|(rate, total)| {
                total >= POOR_RELIABILITY_MIN_OUTCOMES && rate < POOR_RELIABILITY_MAX_RATE
            });
            let should_archive = inactive_days >= self.config.archive_after_days
                || (poor_reliability && inactive_days >= self.config.stale_after_days);
            if should_archive {
                let skill_dir = match self.store.live_skill_dir(&name) {
                    Ok(path) => path,
                    Err(error) => {
                        report.errors.push(format!("{name}: {error}"));
                        continue;
                    }
                };
                match crate::skill_manage::archive_skill_for_curator(
                    &self.store.skills_dir,
                    &skill_dir,
                    &name,
                ) {
                    Ok(path) => match self.store.mark_archived(&name, &path) {
                        Ok(()) => report.archived.push(name),
                        Err(error) => match std::fs::rename(&path, &skill_dir) {
                            Ok(()) => report.errors.push(format!("{name}: {error}")),
                            Err(rollback) => report.errors.push(format!(
                                "{name}: {error}; rollback from {} also failed: {rollback}",
                                path.display()
                            )),
                        },
                    },
                    Err(error) => report.errors.push(format!("{name}: {error}")),
                }
            } else if inactive_days >= self.config.stale_after_days {
                if record.state != SkillState::Stale {
                    match self.store.set_state(&name, SkillState::Stale) {
                        Ok(()) => report.stale.push(name),
                        Err(error) => report.errors.push(format!("{name}: {error}")),
                    }
                }
            } else if record.state == SkillState::Stale {
                match self.store.set_state(&name, SkillState::Active) {
                    Ok(()) => report.reactivated.push(name),
                    Err(error) => report.errors.push(format!("{name}: {error}")),
                }
            }
        }
        if report.errors.is_empty() {
            self.save_state()?;
        }
        Ok(report)
    }

    pub fn restore(&self, name: &str) -> Result<PathBuf, String> {
        let _lifecycle = lock_lifecycle()?;
        self.restore_locked(name)
    }

    /// restore 的无锁内核。只能在已持有 lifecycle 锁的上下文调用
    /// （如 `skill_manage::execute` 全程持锁——std Mutex 不可重入，
    /// 再入即死锁，曾导致面板开关与 agent 的 restore 永久挂起）。
    pub(crate) fn restore_locked(&self, name: &str) -> Result<PathBuf, String> {
        if has_active_skill_users(&self.store.skills_dir)? {
            return Err(
                "Skill restore cannot run while an asynchronous agent is using Skills".to_string(),
            );
        }
        SkillUsageStore::validate_name(name)?;
        let record = self.store.record(name)?;
        if record.state != SkillState::Archived {
            return Err(format!("Skill '{name}' is not archived"));
        }
        let relative = record
            .archive_path
            .as_deref()
            .ok_or_else(|| format!("Skill '{name}' has no recorded archive path"))?;
        SkillUsageStore::validate_archive_relative(name, relative)?;
        let source = self.store.skills_dir.join(relative);
        let destination = self.store.skills_dir.join(name);
        match std::fs::symlink_metadata(&destination) {
            Ok(_) => {
                return Err(format!(
                    "Skill '{name}' already exists; refusing to overwrite it"
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "inspect restore destination {} failed: {error}",
                    destination.display()
                ));
            }
        }
        if !source.is_dir() {
            return Err(format!(
                "archive directory is missing: {}",
                source.display()
            ));
        }
        if crate::skill_manage::skill_path_is_redirect(&source) {
            return Err(format!(
                "archive directory is a symlink or junction: {}",
                source.display()
            ));
        }
        let archived_skill_md = source.join("SKILL.md");
        if !archived_skill_md.is_file()
            || !rclaw_store::resolve_within_base(&self.store.skills_dir, &archived_skill_md)
        {
            return Err(format!(
                "archive for Skill '{name}' has no safe SKILL.md file: {}",
                archived_skill_md.display()
            ));
        }
        if !rclaw_store::resolve_within_base(&self.store.skills_dir, &source)
            || !rclaw_store::resolve_within_base(&self.store.skills_dir, &destination)
        {
            return Err("restore path resolves outside skills_dir".to_string());
        }
        std::fs::rename(&source, &destination)
            .map_err(|error| format!("restore move failed: {error}"))?;
        if let Err(error) = self.store.mark_restored(name) {
            return match std::fs::rename(&destination, &source) {
                Ok(()) => Err(error),
                Err(rollback) => Err(format!(
                    "{error}; rollback to {} also failed: {rollback}",
                    source.display()
                )),
            };
        }
        Ok(destination)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use std::fs;
    use tempfile::TempDir;

    fn setup() -> (TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let skills = temp.path().join("skills");
        fs::create_dir_all(&skills).unwrap();
        (temp, skills)
    }

    #[test]
    fn embedded_repository_manifest_overrides_sidecar_ownership() {
        let (_temp, skills) = setup();
        let store = SkillUsageStore::new(skills);
        store
            .record_created("glmv-stock-analyst", SkillOwner::User)
            .unwrap();
        assert_eq!(
            store.owner("glmv-stock-analyst").unwrap(),
            SkillOwner::Repository
        );
    }

    #[test]
    fn runtime_manifest_cannot_change_embedded_repository_ownership() {
        let (_temp, skills) = setup();
        fs::write(
            skills.join(BUNDLED_MANIFEST_FILE),
            r#"{"skills":["custom-bundled"]}"#,
        )
        .unwrap();
        let store = SkillUsageStore::new(skills);
        assert_eq!(
            store.owner("glmv-stock-analyst").unwrap(),
            SkillOwner::Repository
        );
        assert_eq!(store.owner("custom-bundled").unwrap(), SkillOwner::User);
    }

    #[test]
    fn embedded_manifest_matches_repository_skill_packages() {
        let repository_skills = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills");
        if !repository_skills.is_dir() {
            return;
        }
        let expected: BTreeSet<String> = fs::read_dir(&repository_skills)
            .unwrap()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let addressable = path.join("SKILL.md").is_file()
                    || path.join("skills").read_dir().is_ok_and(|mut entries| {
                        entries.any(|item| {
                            item.ok().is_some_and(|item| {
                                item.path().extension().and_then(|ext| ext.to_str()) == Some("md")
                            })
                        })
                    });
                addressable.then(|| entry.file_name().to_string_lossy().into_owned())
            })
            .collect();
        let embedded: BundledManifest = serde_json::from_str(BUNDLED_MANIFEST_SOURCE).unwrap();
        assert_eq!(embedded.skills, expected);
    }

    #[test]
    fn unknown_skills_default_to_user_owned() {
        let (_temp, skills) = setup();
        let store = SkillUsageStore::new(skills);
        assert_eq!(store.owner("local").unwrap(), SkillOwner::User);
    }

    #[test]
    fn lifecycle_mutations_require_a_live_skill_directory() {
        let (_temp, skills) = setup();
        let store = SkillUsageStore::new(skills);
        assert!(store.adopt("missing").is_err());
        assert!(store.set_pinned("missing", true).is_err());
    }

    #[test]
    fn adopt_and_unpin_reset_curator_activity_window() {
        let (_temp, skills) = setup();
        let dir = skills.join("local");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "# Local\n").unwrap();
        let store = SkillUsageStore::new(skills.clone());
        store.record_created("local", SkillOwner::User).unwrap();
        store
            .mutate(|db, _| {
                let record = db.skills.get_mut("local").unwrap();
                record.created_at = Some((Utc::now() - Duration::days(365)).to_rfc3339());
                Ok(())
            })
            .unwrap();
        store.adopt("local").unwrap();
        store.set_pinned("local", true).unwrap();
        store.set_pinned("local", false).unwrap();
        let record = store.record("local").unwrap();
        assert!(record.last_lifecycle_at.is_some());

        let curator = SkillCurator::new(
            skills,
            CuratorConfig {
                stale_after_days: 30,
                archive_after_days: 90,
                ..CuratorConfig::default()
            },
        );
        let report = curator.run_now().unwrap();
        assert!(report.stale.is_empty());
        assert!(report.archived.is_empty());
        assert!(dir.exists());
    }

    #[test]
    fn embedded_manifest_is_used_when_runtime_manifest_is_missing() {
        let (_temp, skills) = setup();
        let store = SkillUsageStore::new(skills);
        assert_eq!(
            store.owner("glmv-stock-analyst").unwrap(),
            SkillOwner::Repository
        );
    }

    #[test]
    fn telemetry_counts_are_persisted() {
        let (_temp, skills) = setup();
        let store = SkillUsageStore::new(skills.clone());
        store.record_view_and_use("local").unwrap();
        store.record_patch("local").unwrap();
        let reloaded = SkillUsageStore::new(skills).record("local").unwrap();
        assert_eq!(reloaded.view_count, 1);
        assert_eq!(reloaded.use_count, 1);
        assert_eq!(reloaded.patch_count, 1);
    }

    #[test]
    fn outcomes_are_persisted_with_laplace_reliability() {
        let (_temp, skills) = setup();
        let store = SkillUsageStore::new(skills.clone());
        for _ in 0..3 {
            store.record_outcome("local", true).unwrap();
        }
        store.record_outcome("local", false).unwrap();
        let record = SkillUsageStore::new(skills).record("local").unwrap();
        assert_eq!(record.success_count, 3);
        assert_eq!(record.failure_count, 1);
        assert!(record.last_outcome_at.is_some());
        // 拉普拉斯平滑：(3+1)/(3+1+2) = 0.6667。
        let (rate, total) = record.reliability().unwrap();
        assert_eq!(total, 4);
        assert!((rate - 4.0 / 6.0).abs() < 1e-9);
    }

    #[test]
    fn reliability_is_none_without_outcomes() {
        let (_temp, skills) = setup();
        let store = SkillUsageStore::new(skills);
        store.record_view_and_use("local").unwrap();
        assert!(store.record("local").unwrap().reliability().is_none());
    }

    #[test]
    fn curator_archives_poor_reliability_stale_skill_early() {
        let (_temp, skills) = setup();
        let dir = skills.join("flaky");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "# Flaky\n").unwrap();
        let store = SkillUsageStore::new(skills.clone());
        store.record_created("flaky", SkillOwner::Agent).unwrap();
        // 5 轮 4 败：平滑成功率 (1+1)/(5+2) ≈ 0.286 < 0.34。
        for success in [false, false, true, false, false] {
            store.record_outcome("flaky", success).unwrap();
        }
        // 把全部活动时间拨回 stale_after_days（30 天）之后、archive_after_days（90 天）
        // 之前——record_outcome 只记结果计数，不刷新活动时间（latest_activity 不含
        // last_outcome_at），这正是「已被弃用且历史上持续失败」的形态。
        store
            .mutate(|db, _| {
                let record = db.skills.get_mut("flaky").unwrap();
                record.created_at = Some((Utc::now() - Duration::days(40)).to_rfc3339());
                record.last_lifecycle_at = None;
                Ok(())
            })
            .unwrap();
        let curator = SkillCurator::new(
            skills.clone(),
            CuratorConfig {
                stale_after_days: 30,
                archive_after_days: 90,
                ..CuratorConfig::default()
            },
        );
        let report = curator.run_now().unwrap();
        assert_eq!(report.archived, vec!["flaky"], "{:?}", report.errors);
        assert!(!dir.exists());
        // 高可靠技能即使同样闲置 40 天也只标 stale，不提前归档。
        let stable = skills.join("stable");
        fs::create_dir(&stable).unwrap();
        fs::write(stable.join("SKILL.md"), "# Stable\n").unwrap();
        store.record_created("stable", SkillOwner::Agent).unwrap();
        for _ in 0..5 {
            store.record_outcome("stable", true).unwrap();
        }
        store
            .mutate(|db, _| {
                let record = db.skills.get_mut("stable").unwrap();
                record.created_at = Some((Utc::now() - Duration::days(40)).to_rfc3339());
                record.last_lifecycle_at = None;
                Ok(())
            })
            .unwrap();
        let report = SkillCurator::new(
            skills,
            CuratorConfig {
                stale_after_days: 30,
                archive_after_days: 90,
                ..CuratorConfig::default()
            },
        )
        .run_now()
        .unwrap();
        assert!(report.archived.is_empty());
        assert_eq!(report.stale, vec!["stable"]);
    }

    #[test]
    fn curator_never_archives_user_owned_skill() {
        let (_temp, skills) = setup();
        let dir = skills.join("local");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "# Local\n").unwrap();
        let store = SkillUsageStore::new(skills.clone());
        store.record_created("local", SkillOwner::User).unwrap();
        let curator = SkillCurator::new(
            skills,
            CuratorConfig {
                stale_after_days: 0,
                archive_after_days: 0,
                ..CuratorConfig::default()
            },
        );
        let report = curator.run_now().unwrap();
        assert!(report.archived.is_empty());
        assert!(dir.exists());
    }

    #[test]
    fn curator_archives_and_restores_agent_owned_skill() {
        let (_temp, skills) = setup();
        let dir = skills.join("learned");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "# Learned\n").unwrap();
        let store = SkillUsageStore::new(skills.clone());
        store.record_created("learned", SkillOwner::Agent).unwrap();
        let curator = SkillCurator::new(
            skills.clone(),
            CuratorConfig {
                stale_after_days: 0,
                archive_after_days: 0,
                ..CuratorConfig::default()
            },
        );
        let report = curator.run_now().unwrap();
        assert_eq!(report.archived, vec!["learned"], "{:?}", report.errors);
        assert!(!dir.exists());
        curator.restore("learned").unwrap();
        assert!(dir.join("SKILL.md").exists());
        let restored = store.record("learned").unwrap();
        assert_eq!(restored.state, SkillState::Active);
        assert!(restored.last_lifecycle_at.is_some());
    }

    #[test]
    fn curator_waits_for_asynchronous_skill_users() {
        let (_temp, skills) = setup();
        let dir = skills.join("learned");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "# Learned\n").unwrap();
        let store = SkillUsageStore::new(skills.clone());
        store.record_created("learned", SkillOwner::Agent).unwrap();
        let curator = SkillCurator::new(
            skills.clone(),
            CuratorConfig {
                interval_hours: 0,
                stale_after_days: 0,
                archive_after_days: 0,
                ..CuratorConfig::default()
            },
        );

        let activity = begin_skill_activity(&skills).unwrap();
        assert!(curator.run_if_due().unwrap().skipped);
        assert!(curator
            .run_now()
            .unwrap_err()
            .contains("asynchronous agent"));
        assert!(dir.exists());

        drop(activity);
        assert_eq!(curator.run_now().unwrap().archived, vec!["learned"]);
        assert!(!dir.exists());
    }

    #[test]
    fn legacy_pinned_marker_is_effective_lifecycle_state() {
        let (_temp, skills) = setup();
        let dir = skills.join("learned");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "# Learned\n").unwrap();
        fs::write(dir.join(".pinned"), "").unwrap();
        let store = SkillUsageStore::new(skills.clone());
        store.record_created("learned", SkillOwner::Agent).unwrap();
        assert!(store.record("learned").unwrap().pinned);
        let curator = SkillCurator::new(
            skills,
            CuratorConfig {
                stale_after_days: 0,
                archive_after_days: 0,
                ..CuratorConfig::default()
            },
        );
        assert!(curator.run_now().unwrap().archived.is_empty());
        assert!(dir.exists());

        store.set_pinned("learned", false).unwrap();
        assert!(!dir.join(".pinned").exists());
        assert!(!store.record("learned").unwrap().pinned);
    }

    #[test]
    fn restore_rejects_archive_path_for_a_different_skill() {
        let (_temp, skills) = setup();
        let dir = skills.join("learned");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "# Learned\n").unwrap();
        let store = SkillUsageStore::new(skills.clone());
        store.record_created("learned", SkillOwner::Agent).unwrap();
        let curator = SkillCurator::new(
            skills.clone(),
            CuratorConfig {
                stale_after_days: 0,
                archive_after_days: 0,
                ..CuratorConfig::default()
            },
        );
        curator.run_now().unwrap();
        store
            .mutate(|db, _| {
                db.skills.get_mut("learned").unwrap().archive_path =
                    Some(".archive/different-20260101T000000Z".to_string());
                Ok(())
            })
            .unwrap();
        let error = curator.restore("learned").unwrap_err();
        assert!(error.contains("invalid recorded archive path"), "{error}");
    }

    #[test]
    fn restore_rejects_same_name_archive_path_without_curator_timestamp() {
        let (_temp, skills) = setup();
        let archive = skills.join(".archive/learned-arbitrary");
        fs::create_dir_all(&archive).unwrap();
        fs::write(archive.join("SKILL.md"), "# Untrusted\n").unwrap();
        let store = SkillUsageStore::new(skills.clone());
        store
            .mutate(|db, _| {
                db.skills.insert(
                    "learned".to_string(),
                    SkillLifecycleRecord {
                        owner: Some(SkillOwner::Agent),
                        state: SkillState::Archived,
                        archive_path: Some(".archive/learned-arbitrary".to_string()),
                        ..SkillLifecycleRecord::default()
                    },
                );
                Ok(())
            })
            .unwrap();
        let curator = SkillCurator::new(skills, CuratorConfig::default());
        assert!(curator
            .restore("learned")
            .unwrap_err()
            .contains("invalid recorded archive path"));
    }

    #[cfg(unix)]
    #[test]
    fn restore_does_not_overwrite_a_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let (temp, skills) = setup();
        let dir = skills.join("learned");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "# Learned\n").unwrap();
        let store = SkillUsageStore::new(skills.clone());
        store.record_created("learned", SkillOwner::Agent).unwrap();
        let curator = SkillCurator::new(
            skills.clone(),
            CuratorConfig {
                stale_after_days: 0,
                archive_after_days: 0,
                ..CuratorConfig::default()
            },
        );
        curator.run_now().unwrap();

        symlink(temp.path().join("missing"), &dir).unwrap();
        let error = curator.restore("learned").unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert!(dir.is_symlink());
        let archived = store.record("learned").unwrap().archive_path.unwrap();
        assert!(skills.join(archived).is_dir());
    }

    #[test]
    fn restore_rejects_an_incomplete_archive() {
        let (_temp, skills) = setup();
        let dir = skills.join("learned");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "# Learned\n").unwrap();
        let store = SkillUsageStore::new(skills.clone());
        store.record_created("learned", SkillOwner::Agent).unwrap();
        let curator = SkillCurator::new(
            skills.clone(),
            CuratorConfig {
                stale_after_days: 0,
                archive_after_days: 0,
                ..CuratorConfig::default()
            },
        );
        curator.run_now().unwrap();
        let archived = store.record("learned").unwrap().archive_path.unwrap();
        let archived_dir = skills.join(archived);
        fs::remove_file(archived_dir.join("SKILL.md")).unwrap();

        let error = curator.restore("learned").unwrap_err();
        assert!(error.contains("no safe SKILL.md file"), "{error}");
        assert!(archived_dir.is_dir());
        assert!(!dir.exists());
        assert_eq!(store.record("learned").unwrap().state, SkillState::Archived);
    }

    #[test]
    fn future_curator_timestamp_does_not_freeze_lifecycle() {
        let (_temp, skills) = setup();
        fs::write(
            skills.join(CURATOR_STATE_FILE),
            format!(
                r#"{{"last_run_at":"{}"}}"#,
                (Utc::now() + Duration::days(365)).to_rfc3339()
            ),
        )
        .unwrap();
        let curator = SkillCurator::new(skills, CuratorConfig::default());
        assert!(!curator.run_if_due().unwrap().skipped);
    }

    #[test]
    fn maximum_curator_interval_does_not_overflow() {
        let (_temp, skills) = setup();
        fs::write(
            skills.join(CURATOR_STATE_FILE),
            format!(
                r#"{{"last_run_at":"{}"}}"#,
                (Utc::now() - Duration::hours(1)).to_rfc3339()
            ),
        )
        .unwrap();
        let curator = SkillCurator::new(
            skills,
            CuratorConfig {
                interval_hours: u64::MAX,
                ..CuratorConfig::default()
            },
        );
        assert!(curator.run_if_due().unwrap().skipped);
    }
}
