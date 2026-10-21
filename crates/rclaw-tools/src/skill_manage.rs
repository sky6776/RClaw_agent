//! `skill_manage` 工具：技能生命周期管理。
//!
//! 把「自进化」从 MEMORY.md 里的建议落地为真实的技能改动——支持
//! create / edit / patch / delete / write_file / remove_file，并提供三层写保护
//! （按 RClaw 精简）：
//!
//! - **路径沙箱**：所有目标必须落在 `skills_dir` 内，拒绝 `..` 穿越、绝对路径、
//!   符号链接/目录 junction 逃逸；`delete` 额外拒绝「删 skills 根自身」。
//! - **受保护技能判定（拆分两种语义）**：技能目录里的标记文件决定保护级别——
//!   - `.protected`（「神圣不可碰」）：后台 review fork
//!     不得对其做**任何写**；前台（用户在环）仅 `delete` 受限（patch/edit 仍可）。
//!   - `.pinned`：前台仅 `delete` 受限（patch/edit 仍可）；
//!     后台 review 属于无人值守的自主维护，对 pinned 技能不做任何写入。
//! - **background review 写保护**：后台 review fork（[`WriteOrigin::BackgroundReview`]）
//!   是无人值守的自动整理，其写面被严格收窄——
//!     * 不得对 `.protected` / `.pinned` 技能做任何写；pinned 的内容更新只允许前台用户在环执行。
//!     * 必须先读后写（read-before-write）：edit / patch / write_file / remove_file 要求目标文件
//!       在本轮 review 中被读过；delete consolidation 要求源与伞都已读（见 [`ReviewReadTracker`]）；
//!       `create` 豁免。
//! - **patch 唯一命中**：`old_string` 在文件中必须唯一命中（除非 `replace_all=true`），
//!   0 / >1 都拒绝并回显上下文；写前留 `.bak`，写后做大小与 Skill 根契约校验。
//!
//! 主 agent（前台）与后台 review fork（[`WriteOrigin::BackgroundReview`]）都使用本工具；
//! 它取代了早期仅 patch/create 的 `skill_patch` 工具（后者已移除，本工具是其完整超集）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::skill_ledger::SkillLedger;
use crate::skill_usage::{SkillOwner, SkillState, SkillUsageStore};
use crate::{Tool, ToolResult};

const MAX_SKILL_CONTENT_CHARS: usize = 100_000;
const MAX_SKILL_FILE_BYTES: usize = 1_048_576; // 1 MiB / supporting file
const MAX_NAME_LENGTH: usize = 64;
/// write_file / remove_file 允许的子目录。
const ALLOWED_SUBDIRS: &[&str] = &["references", "templates", "scripts", "assets"];

/// 技能文本写入前的最小安全闸（hermes-vs-rclaw 自进化分析 B2）。
///
/// 隐形/双向控制字符**拒绝**（纯恶意、无合法场景）；密钥类内容**自动脱敏**后放行
/// （威胁扫描挡注入 + 脱敏挡泄密，与记忆写入路径同一纪律）。刻意不做
/// prompt_injection 模式匹配——技能本就是写给模型读的指令文档，
/// "ignore previous instructions" 作为教学示例属合法内容，模式匹配会误杀。
fn gate_skill_text(text: &str) -> Result<String, String> {
    if let Some(reason) = rclaw_store::scan_skill_text(text) {
        return Err(format!("content rejected by security gate: {reason}"));
    }
    Ok(rclaw_store::redact_sensitive_text(text))
}

/// 写入来源：前台（用户在环）或后台 review fork（无人值守）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOrigin {
    Foreground,
    BackgroundReview,
}

/// 后台 review fork「本轮已读文件」跟踪器（read-before-write 守卫用）。
///
/// review 的读工具（[`SkillReadTool`]）记录规范路径及当时的内容摘要；写入前既检查
/// 「是否先读过」，也拒绝覆盖读取后已变化的文件。前台构造的 [`SkillManageTool`] 不带
/// tracker，不做此校验。
pub type ReviewReadTracker = Arc<Mutex<HashMap<PathBuf, [u8; 32]>>>;

/// 全功能的技能生命周期管理工具。
pub struct SkillManageTool {
    skills_dir: PathBuf,
    origin: WriteOrigin,
    read_tracker: Option<ReviewReadTracker>,
    usage: SkillUsageStore,
    /// 变更台账（before blob + jsonl），支撑 action=rollback。
    ledger: SkillLedger,
}

impl SkillManageTool {
    /// 前台（主 agent）构造：用户在环，不做 read-before-write 与受保护写收窄
    /// （`delete` 仍受 pinned 限制）。
    pub fn new(skills_dir: PathBuf) -> Self {
        SkillManageTool {
            usage: SkillUsageStore::new(skills_dir.clone()),
            ledger: SkillLedger::new(&skills_dir),
            skills_dir,
            origin: WriteOrigin::Foreground,
            read_tracker: None,
        }
    }

    /// 后台 review fork 构造：启用受保护写保护 + read-before-write 守卫。
    pub fn for_background_review(skills_dir: PathBuf, tracker: ReviewReadTracker) -> Self {
        SkillManageTool {
            usage: SkillUsageStore::new(skills_dir.clone()),
            ledger: SkillLedger::new(&skills_dir),
            skills_dir,
            origin: WriteOrigin::BackgroundReview,
            read_tracker: Some(tracker),
        }
    }

    fn is_background(&self) -> bool {
        self.origin == WriteOrigin::BackgroundReview
    }

    /// 台账 actor 标识。
    fn actor(&self) -> &'static str {
        if self.is_background() {
            "background_review"
        } else {
            "foreground"
        }
    }

    /// 回滚一次已记录的变更（前台专用；后台 review 不得擅自撤销历史变更）。
    fn do_rollback(&self, id: &str) -> ToolResult {
        if self.is_background() {
            return ToolResult::err(
                "rollback is a foreground-only operation; background review cannot undo past changes",
            );
        }
        if id.trim().is_empty() {
            return ToolResult::err("rollback requires id (a ledger entry id)".to_string());
        }
        match self.ledger.rollback(id.trim()) {
            Ok((restored, removed)) => {
                let _ = self.log_evolution(&format!("rolled back ledger entry {id}"));
                ToolResult::ok(
                    json!({
                        "success": true,
                        "id": id,
                        "restored_files": restored,
                        "removed_files": removed,
                        "message": "ledger entry rolled back; re-read the skill before further edits"
                    })
                    .to_string(),
                )
            }
            Err(e) => ToolResult::err(e),
        }
    }

    // ---- 路径解析与沙箱 ----

    /// 校验技能名并解析到 `skills_dir/<name>`（不要求已存在）。
    fn resolve_skill_dir(&self, name: &str) -> Result<PathBuf, String> {
        validate_name(name)?;
        let dir = self.skills_dir.join(name);
        self.under_skills(&dir)
    }

    /// 禁能（归档）技能：目录移入 `skills/.archive/<name>-<ts>/`，退出技能目录扫描，
    /// 不再进入系统提示与技能清单；生命周期元数据记 Archived。仅前台可用
    /// （自进化后台 reviewer 不得擅动技能面）。可经 restore 或 `/skills restore` 还原。
    fn do_archive(&self, name: &str) -> ToolResult {
        if self.is_background() {
            return ToolResult::err(
                "archive is a foreground-only enable/disable switch; background review cannot archive skills",
            );
        }
        let skill_dir = match self.resolve_skill_dir(name) {
            Ok(dir) => dir,
            Err(error) => return ToolResult::err(error),
        };
        if !skill_dir.is_dir() {
            return ToolResult::err(format!("skill '{name}' 不是目录或不存在"));
        }
        let dest = match helpers::archive_skill(&self.skills_dir, &skill_dir, name) {
            Ok(dest) => dest,
            Err(error) => return ToolResult::err(error),
        };
        if let Err(error) = self.usage.mark_archived(name, &dest) {
            return ToolResult::err(format!(
                "archived to {} but lifecycle metadata could not be saved: {error}",
                dest.display()
            ));
        }
        ToolResult::ok(format!(
            "skill '{name}' disabled (archived under skills/.archive/); restore with skill_manage action=restore"
        ))
    }

    /// 启用（恢复）已归档技能：从 `.archive/` 移回并清 Archived 标记。
    fn do_restore(&self, name: &str) -> ToolResult {
        if self.is_background() {
            return ToolResult::err(
                "restore is a foreground-only enable/disable switch; background review cannot restore skills",
            );
        }
        let curator = crate::skill_usage::SkillCurator::new(
            self.skills_dir.clone(),
            crate::skill_usage::CuratorConfig::default(),
        );
        // execute 已全程持有 lifecycle 锁：这里必须走无锁内核（再入 std Mutex 即死锁）。
        match curator.restore_locked(name) {
            Ok(dest) => ToolResult::ok(format!(
                "skill '{name}' enabled (restored to {})",
                dest.display()
            )),
            Err(error) => ToolResult::err(error),
        }
    }

    /// 解析技能内某文件（默认 SKILL.md）；根 Manifest 与支撑文件按规范校验。
    fn resolve_target(&self, skill_dir: &Path, file_path: &str) -> Result<PathBuf, String> {
        if file_path.is_empty() || file_path == "SKILL.md" {
            return self.under_skills(&skill_dir.join("SKILL.md"));
        }
        validate_file_path(file_path)?;
        self.under_skills(&skill_dir.join(file_path))
    }

    /// 把目标规范化到 skills_dir 之内：拒绝 `..` 与「跳出 skills/ 的绝对路径」；以 skills_dir
    /// 规范绝对路径为前缀重建（不要求目标已存在，create 场景需要）。允许 skills_dir 自身是
    /// 绝对路径（tempdir / 部署目录通常都是）——只要目标在其之下。
    fn under_skills(&self, target: &Path) -> Result<PathBuf, String> {
        let canon_skills = self
            .skills_dir
            .canonicalize()
            .map_err(|e| format!("skills_dir 不可读: {e}"))?;
        // 优先相对「原始 skills_dir」剥离；再尝试相对「canonical 根」剥离（resolve_skill_dir
        // 返回的就是 canonical 根拼出的路径）。
        let rel_opt = target
            .strip_prefix(&self.skills_dir)
            .or_else(|_| target.strip_prefix(&canon_skills))
            .ok();
        let rel = match rel_opt {
            Some(r) => r.to_path_buf(),
            None if target.is_absolute() => {
                return Err("target escapes skills_dir (absolute path outside skills/)".into());
            }
            None => target.to_path_buf(),
        };
        if rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("target escapes skills_dir (contains '..')".into());
        }
        let resolved = canon_skills.join(&rel);
        // 词法校验之后再做符号链接/junction 解析：canonicalize 目标的「最长已存在祖先」，
        // 确认落点仍在 skills 根下——挡住 `skills/<name> -> /etc` 这类已落地的逃逸链接
        // （词法前缀 + `..` 检查挡不住它）。复用 rclaw-store 的 resolve_within_base，
        // 与 agent 目录写入面同一套语义。
        if !rclaw_store::resolve_within_base(&canon_skills, &resolved) {
            return Err(
                "target escapes skills_dir (symlink/junction resolves outside skills/)".into(),
            );
        }
        Ok(resolved)
    }

    // ---- 写保护 ----

    /// 该技能是否被标记为 `.protected`（神圣不可碰：后台 review 不得做任何写）。
    fn is_protected(&self, skill_dir: &Path) -> bool {
        skill_dir.join(".protected").exists()
    }

    /// 该技能是否被标记为 `.pinned`（前台仅挡 delete；后台 review 不自主写入）。
    fn is_pinned(&self, skill_dir: &Path) -> bool {
        skill_dir.join(".pinned").exists()
    }

    /// 按保护级别（`.protected` / `.pinned`）与写来源（前台 / 后台 review）决定是否拒绝。
    /// 返回 `Some(错误信息)` 表示拒绝，`None` 表示放行。
    ///
    /// 保护标记分为两种语义：
    /// - `.protected`：后台 review 一切写皆拒；前台仅 `delete` 被拒（patch/edit 仍可）。
    /// - `.pinned`：后台 review 一切写皆拒；
    ///   前台仅 `delete` 被拒，patch/edit 仍可。
    /// - 既非 protected 也非 pinned：放行（delete 的 consolidation 语义另由
    ///   [`SkillManageTool::guard_background_delete`] 守卫）。
    fn guard_protected(&self, name: &str, skill_dir: &Path, action: &str) -> Option<String> {
        if skill_path_is_redirect(skill_dir) {
            return Some(format!(
                "Refusing {action} for Skill '{name}': top-level Skill symlinks/junctions are not lifecycle-managed"
            ));
        }
        let protected = self.is_protected(skill_dir);
        let marker_pinned = self.is_pinned(skill_dir);
        let lifecycle = self.usage.record(name);
        if let Err(error) = &lifecycle {
            if self.is_background() || action == "delete" {
                return Some(format!(
                    "Refusing {action} for Skill '{name}': ownership/pin metadata could not be read ({error})."
                ));
            }
        }
        let pinned = marker_pinned || lifecycle.as_ref().is_ok_and(|record| record.pinned);
        if action == "create"
            && lifecycle
                .as_ref()
                .is_ok_and(|record| record.state == SkillState::Archived)
        {
            return Some(format!(
                "Refusing create for archived Skill '{name}': use `/skills restore {name}` so its provenance and history are preserved."
            ));
        }
        if self.is_background() {
            let owner = if action == "create" {
                match self.usage.recorded_owner(name) {
                    Ok(owner) => owner,
                    Err(error) => {
                        return Some(format!(
                            "Refusing background review create for Skill '{name}': provenance metadata could not be read ({error})."
                        ));
                    }
                }
            } else {
                Some(
                    lifecycle
                        .as_ref()
                        .ok()
                        .and_then(|record| record.owner)
                        .unwrap_or(SkillOwner::User),
                )
            };
            if owner.is_some_and(|owner| owner != SkillOwner::Agent) {
                let owner = owner.unwrap_or(SkillOwner::User);
                return Some(format!(
                    "Refusing background review {action} for {owner}-owned Skill '{name}': autonomous maintenance may mutate only agent-owned Skills. Use `/skills adopt {name}` in the foreground to transfer ownership explicitly."
                ));
            }
        }
        if !protected && !pinned {
            return None;
        }
        if self.is_background() {
            // `.protected`：无人值守的自动维护不得碰——一切写皆拒。
            if protected {
                return Some(format!(
                    "Refusing background review {action} for protected skill '{name}': \
                     .protected skills are off-limits to autonomous maintenance. \
                     Ask the user to remove the .protected marker if they want it changed."
                ));
            }
            // `.pinned`：前台仅拒绝 delete；无人值守的 background review 更严格，拒绝所有写入。
            if pinned {
                return Some(format!(
                    "Refusing background review {action} for pinned skill '{name}': \
                     .pinned skills are off-limits to autonomous maintenance. \
                     Foreground/user-directed patches and edits are still allowed; \
                     ask the user to remove the .pinned marker if they want it changed."
                ));
            }
        }
        // 前台（用户在环）：protected 与 pinned 都仅挡 delete。
        if action == "delete" {
            return Some(if protected {
                format!(
                    "Skill '{name}' is protected and cannot be deleted by skill_manage. Remove the .protected marker first if you intend to delete it. Patches and edits are allowed; only deletion is blocked."
                )
            } else if marker_pinned {
                format!(
                    "Skill '{name}' is pinned by its legacy .pinned marker and cannot be deleted by skill_manage. Use `/skills unpin {name}` (or remove that marker) first. Patches and edits are allowed; only deletion is blocked."
                )
            } else {
                format!(
                    "Skill '{name}' is pinned and cannot be deleted by skill_manage. Use `/skills unpin {name}` first. Patches and edits are allowed; only deletion is blocked."
                )
            });
        }
        None
    }

    /// background review 的 fail-closed 删保护：后台无人值守的整理只能「把内容吸收进伞技能
    /// 后归档」，未声明 `absorbed_into`（= 无 verified consolidation）的裸删一律拒绝——
    /// 防止后台 review 永久删除未保护技能。前台不受此限。
    /// 返回 `Some(错误信息)` 表示拒绝。
    fn guard_background_delete(&self, name: &str, absorbed_into: &str) -> Option<String> {
        if !self.is_background() {
            return None;
        }
        if !absorbed_into.trim().is_empty() {
            return None;
        }
        Some(format!(
            "Refusing background review delete of skill '{name}': the background reviewer may \
             only remove a skill whose content it has consolidated into an umbrella. Pass \
             absorbed_into=<umbrella> (the umbrella must already exist) to record a verified \
             consolidation. A bare prune with no forwarding target is not permitted here — \
             deterministic staleness handling is a separate path. Keeping '{name}' active."
        ))
    }

    /// read-before-write 守卫（仅 background review）：edit/patch/write_file/remove_file
    /// 要求目标文件本轮被读过。`create` 与新建支撑文件豁免。
    fn require_read(&self, target: &Path, name: &str, action: &str) -> Result<(), String> {
        if !self.is_background() {
            return Ok(());
        }
        let Some(tracker) = &self.read_tracker else {
            return Ok(());
        };
        let canon = canonicalize_for_tracking(target);
        let expected = tracker
            .lock()
            .expect("read_tracker 锁未毒化")
            .get(&canon)
            .copied();
        let label = target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("the file");
        let Some(expected) = expected else {
            return Err(format!(
                "Refusing background review {action} for skill '{name}': the current content of \
                 {label} has not been loaded in this review turn. Read it first (read the skill \
                 file), then retry the write using the content just returned."
            ));
        };
        let current = std::fs::read(target).map_err(|error| {
            format!(
                "Refusing background review {action} for skill '{name}': re-read {label} before \
                 writing failed ({error})."
            )
        })?;
        let actual: [u8; 32] = Sha256::digest(&current).into();
        if actual != expected {
            return Err(format!(
                "Refusing background review {action} for skill '{name}': {label} changed after \
                 it was read. Read the current file again, observe the new content, then decide \
                 whether the mutation is still valid."
            ));
        }
        Ok(())
    }

    // ---- 动作实现 ----

    fn do_create(&self, name: &str, content: &str) -> ToolResult {
        if content.trim().is_empty() {
            return ToolResult::err(
                "create requires content (full SKILL.md: frontmatter or '# ' title + body)"
                    .to_string(),
            );
        }
        if let Err(e) = validate_name(name) {
            return ToolResult::err(e);
        }
        let skill_dir = match self.resolve_skill_dir(name) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        // 不允许 create 覆盖受保护技能（background）；前台允许覆盖既有技能目录吗——不允许，
        // create 一律要求目标不存在，用 edit/patch 改既有技能。
        if let Some(msg) = self.guard_protected(name, &skill_dir, "create") {
            return ToolResult::err(msg);
        }
        let target = match self.under_skills(&skill_dir.join("SKILL.md")) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        if skill_dir.exists() {
            return ToolResult::err(format!(
                "skill path already exists: {name} (use action=patch or edit to modify a complete Skill)"
            ));
        }
        if let Err(e) = validate_skill_md(content) {
            return ToolResult::err(e);
        }
        let content = match gate_skill_text(content) {
            Ok(c) => c,
            Err(e) => return ToolResult::err(e),
        };
        if let Err(e) = std::fs::create_dir_all(&skill_dir) {
            return ToolResult::err(format!("创建技能目录失败: {e}"));
        }
        if let Err(e) = write_with_backup(&target, content.as_str()) {
            let _ = std::fs::remove_file(&target);
            let _ = std::fs::remove_dir(&skill_dir);
            return ToolResult::err(format!("写入失败: {e}"));
        }
        // 台账：create 的 before = 文件不存在（rollback 即删除）。
        self.ledger
            .record(self.actor(), "create", name, &[(target.clone(), None)]);
        let owner = if self.is_background() {
            SkillOwner::Agent
        } else {
            SkillOwner::User
        };
        if let Err(error) = self.usage.record_created(name, owner) {
            let _ = std::fs::remove_file(&target);
            let _ = std::fs::remove_dir(&skill_dir);
            return ToolResult::err(format!(
                "Skill file was rolled back because ownership metadata could not be saved: {error}"
            ));
        }
        let _ = self.log_evolution(&format!("created skill {name}"));
        ToolResult::ok(
            json!({
                "success": true,
                "skill": name,
                "file": "SKILL.md",
                "message": "skill created"
            })
            .to_string(),
        )
    }

    fn do_edit(&self, name: &str, content: &str) -> ToolResult {
        if content.trim().is_empty() {
            return ToolResult::err("edit requires content (full updated SKILL.md)".to_string());
        }
        let (skill_dir, target) = match self.locate(name, "") {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };
        if let Some(msg) = self.guard_protected(name, &skill_dir, "edit") {
            return ToolResult::err(msg);
        }
        if let Err(e) = self.require_read(&target, name, "edit") {
            return ToolResult::err(e);
        }
        if let Err(e) = validate_skill_md(content) {
            return ToolResult::err(e);
        }
        let content = match gate_skill_text(content) {
            Ok(c) => c,
            Err(e) => return ToolResult::err(e),
        };
        let before = std::fs::read(&target).ok();
        if let Err(e) = write_with_backup(&target, content.as_str()) {
            return ToolResult::err(format!("写入失败: {e}"));
        }
        self.ledger
            .record(self.actor(), "edit", name, &[(target.clone(), before)]);
        if let Err(error) = self.usage.record_patch(name) {
            tracing::warn!(skill = name, %error, "failed to record Skill patch telemetry");
        }
        let _ = self.log_evolution(&format!("edited skill {name}"));
        ToolResult::ok(
            json!({
                "success": true,
                "skill": name,
                "file": "SKILL.md",
                "message": "skill rewritten"
            })
            .to_string(),
        )
    }

    fn do_patch(
        &self,
        name: &str,
        file_path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> ToolResult {
        if old_string.is_empty() {
            return ToolResult::err(
                "patch requires non-empty old_string (a snippet of the current content)"
                    .to_string(),
            );
        }
        let (skill_dir, target) = match self.locate(name, file_path) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };
        if let Some(msg) = self.guard_protected(name, &skill_dir, "patch") {
            return ToolResult::err(msg);
        }
        if let Err(e) = self.require_read(&target, name, "patch") {
            return ToolResult::err(e);
        }
        let original = match std::fs::read_to_string(&target) {
            Ok(s) => s,
            Err(e) => return ToolResult::err(format!("读取 {} 失败: {e}", target.display())),
        };
        let count = original.matches(old_string).count();
        if count == 0 {
            let preview: String = original.chars().take(400).collect();
            return ToolResult::err(format!(
                "old_string not found. Make sure it matches the file exactly (whitespace/indentation). Preview:\n{preview}"
            ));
        }
        if count > 1 && !replace_all {
            return ToolResult::err(format!(
                "old_string matched {count} places; provide a longer unique snippet, or set replace_all=true."
            ));
        }
        let new_string = match gate_skill_text(new_string) {
            Ok(s) => s,
            Err(e) => return ToolResult::err(e),
        };
        // 先按「命中数 × new_string」估算替换后的大小再做 replace：大小校验原本在
        // String::replace 之后，replace_all + 极短 old_string（如单个空格）+ 几十 KB 的
        // new_string 会先分配出 GB 级字符串（分配失败即 abort，Android 直接被杀）才被拒。
        let replacements = if replace_all { count } else { 1 };
        let projected = original.len().saturating_add(
            replacements.saturating_mul(new_string.len().saturating_sub(old_string.len())),
        );
        if projected > MAX_SKILL_CONTENT_CHARS.max(MAX_SKILL_FILE_BYTES) * 4 {
            return ToolResult::err(format!(
                "patch would expand the file to ~{projected} bytes ({replacements} replacement(s)); \
                 exceeds the skill content limit. Narrow old_string or shorten new_string."
            ));
        }
        let updated = if replace_all {
            original.replace(old_string, new_string.as_str())
        } else {
            original.replacen(old_string, new_string.as_str(), 1)
        };
        // SKILL.md 结构校验（支撑文件不做标题校验，仅大小）。
        if let Err(e) = validate_target_content(&target, &updated) {
            return ToolResult::err(e);
        }
        let before = std::fs::read(&target).ok();
        if let Err(e) = write_with_backup(&target, &updated) {
            return ToolResult::err(format!("写入失败: {e}"));
        }
        self.ledger
            .record(self.actor(), "patch", name, &[(target.clone(), before)]);
        if let Err(error) = self.usage.record_patch(name) {
            tracing::warn!(skill = name, %error, "failed to record Skill patch telemetry");
        }
        let _ = self.log_evolution(&format!("patched {name}/{}", file_label(&target)));
        ToolResult::ok(
            json!({
                "success": true,
                "skill": name,
                "file": target.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                "matches": count,
                "message": "skill file patched"
            })
            .to_string(),
        )
    }

    fn do_delete(&self, name: &str, absorbed_into: &str) -> ToolResult {
        let skill_dir = match self.resolve_skill_dir(name) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        if let Some(msg) = self.guard_protected(name, &skill_dir, "delete") {
            return ToolResult::err(msg);
        }
        // background review fail-closed：未声明 absorbed_into（= 无 verified consolidation）
        // 的裸删一律拒绝，防止 consolidation pass 误删整簇活跃技能。
        // 前台（用户在环）不受此限。
        if let Some(msg) = self.guard_background_delete(name, absorbed_into) {
            return ToolResult::err(msg);
        }
        let absorbed_target = absorbed_into.trim();
        let is_consolidation = !absorbed_target.is_empty();
        // 声明了 absorbed_into 时，伞技能必须已存在（不得删进一个不存在的伞）。
        if is_consolidation {
            if absorbed_target == name {
                return ToolResult::err(format!(
                    "absorbed_into='{absorbed_target}' cannot equal the skill being deleted"
                ));
            }
            let umbrella = match self.usage.live_skill_dir(absorbed_target) {
                Ok(p) => p,
                Err(e) => return ToolResult::err(e),
            };
            if self.is_background() {
                let umbrella_lifecycle = match self.usage.record(absorbed_target) {
                    Ok(record) => record,
                    Err(error) => {
                        return ToolResult::err(format!(
                            "Refusing delete consolidation into '{absorbed_target}': lifecycle metadata could not be read ({error})"
                        ));
                    }
                };
                if umbrella_lifecycle.owner != Some(SkillOwner::Agent)
                    || umbrella_lifecycle.pinned
                    || umbrella.join(".protected").exists()
                {
                    return ToolResult::err(format!(
                        "Refusing delete consolidation into '{absorbed_target}': the umbrella must be agent-owned, unpinned, and unprotected"
                    ));
                }
                let source = skill_dir.join("SKILL.md");
                if let Err(error) = self.require_read(&source, name, "delete") {
                    return ToolResult::err(error);
                }
                let umbrella_skill = umbrella.join("SKILL.md");
                if let Err(error) =
                    self.require_read(&umbrella_skill, absorbed_target, "delete consolidation")
                {
                    return ToolResult::err(error);
                }
            }
        }
        if let Err(e) = validate_delete_target(&self.skills_dir, &skill_dir) {
            return ToolResult::err(e);
        }
        if !skill_dir.is_dir() {
            return ToolResult::err(format!("skill not found (no directory): {name}"));
        }
        // background review 的删【归档可恢复】，不做永久 rmtree：curator pass
        // 经 archive_skill() 落到 skills/.archive/，可还原。前台保留既有硬删语义。
        if self.is_background() {
            let dest = match archive_skill(&self.skills_dir, &skill_dir, name) {
                Ok(p) => p,
                Err(e) => return ToolResult::err(e),
            };
            if let Err(error) = self.usage.mark_archived(name, &dest) {
                return match std::fs::rename(&dest, &skill_dir) {
                    Ok(()) => ToolResult::err(format!(
                        "Skill archive was rolled back because lifecycle metadata could not be saved: {error}"
                    )),
                    Err(rollback) => ToolResult::err(format!(
                        "Skill lifecycle metadata could not be saved ({error}), and rollback from {} also failed: {rollback}",
                        dest.display()
                    )),
                };
            }
            let _ = self.log_evolution(&format!(
                "archived skill {name} (absorbed_into={absorbed_target}) -> {}",
                dest.display()
            ));
            let mut msg = format!("skill '{name}' archived (recoverable under skills/.archive/)");
            if is_consolidation {
                msg.push_str(&format!("; content absorbed into '{absorbed_target}'"));
            }
            return ToolResult::ok(
                json!({
                    "success": true,
                    "skill": name,
                    "archived": true,
                    "message": msg
                })
                .to_string(),
            );
        }
        if let Err(e) = std::fs::remove_dir_all(&skill_dir) {
            return ToolResult::err(format!("删除技能失败: {e}"));
        }
        if let Err(error) = self.usage.forget(name) {
            tracing::warn!(skill = name, %error, "failed to remove Skill telemetry after deletion");
        }
        let _ = self.log_evolution(&format!("deleted skill {name}"));
        let mut msg = format!("skill '{name}' deleted");
        if is_consolidation {
            msg.push_str(&format!("; content absorbed into '{absorbed_target}'"));
        }
        ToolResult::ok(
            json!({
                "success": true,
                "skill": name,
                "message": msg
            })
            .to_string(),
        )
    }

    fn do_write_file(&self, name: &str, file_path: &str, file_content: &str) -> ToolResult {
        if file_path.is_empty() {
            return ToolResult::err(
                "write_file requires file_path (e.g. 'references/api-guide.md')".to_string(),
            );
        }
        if let Err(e) = validate_file_path(file_path) {
            return ToolResult::err(e);
        }
        if file_content.len() > MAX_SKILL_FILE_BYTES {
            return ToolResult::err(format!(
                "file_content exceeds {MAX_SKILL_FILE_BYTES} bytes (1 MiB) per supporting file"
            ));
        }
        let (skill_dir, target) = match self.locate(name, file_path) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };
        let file_content = match gate_skill_text(file_content) {
            Ok(c) => c,
            Err(e) => return ToolResult::err(e),
        };
        if let Err(e) = validate_target_content(&target, file_content.as_str()) {
            return ToolResult::err(e);
        }
        if let Some(msg) = self.guard_protected(name, &skill_dir, "write_file") {
            return ToolResult::err(msg);
        }
        // 覆盖既有支撑文件时（background）要求先读；新建豁免。
        if target.exists() {
            if let Err(e) = self.require_read(&target, name, "write_file") {
                return ToolResult::err(e);
            }
        }
        if let Err(e) = std::fs::create_dir_all(target.parent().unwrap_or(Path::new("."))) {
            return ToolResult::err(format!("创建支撑文件目录失败: {e}"));
        }
        let before = std::fs::read(&target).ok();
        if let Err(e) = write_with_backup(&target, file_content.as_str()) {
            return ToolResult::err(format!("写入失败: {e}"));
        }
        self.ledger.record(
            self.actor(),
            "write_file",
            name,
            &[(target.clone(), before)],
        );
        if let Err(error) = self.usage.record_patch(name) {
            tracing::warn!(skill = name, %error, "failed to record Skill patch telemetry");
        }
        let _ = self.log_evolution(&format!("wrote {name}/{file_path}"));
        ToolResult::ok(
            json!({
                "success": true,
                "skill": name,
                "file": file_path,
                "message": "support file written"
            })
            .to_string(),
        )
    }

    fn do_remove_file(&self, name: &str, file_path: &str) -> ToolResult {
        if file_path.is_empty() {
            return ToolResult::err("remove_file requires file_path".to_string());
        }
        if file_path.eq_ignore_ascii_case("SKILL.md")
            || file_path.eq_ignore_ascii_case("skill.toml")
        {
            return ToolResult::err(
                "remove_file refuses to delete SKILL.md or skill.toml; use action=delete to remove the skill"
                    .to_string(),
            );
        }
        if let Err(e) = validate_file_path(file_path) {
            return ToolResult::err(e);
        }
        let (skill_dir, target) = match self.locate(name, file_path) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };
        if let Some(msg) = self.guard_protected(name, &skill_dir, "remove_file") {
            return ToolResult::err(msg);
        }
        if let Err(e) = self.require_read(&target, name, "remove_file") {
            return ToolResult::err(e);
        }
        if !target.exists() {
            return ToolResult::err(format!("file not found: {file_path}"));
        }
        let manifest_path = skill_dir.join("skill.toml");
        match std::fs::symlink_metadata(&manifest_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return ToolResult::err(format!(
                    "检查 skill.toml 失败，已拒绝删除支撑文件: {error}"
                ));
            }
            Ok(_) => {
                let base = match skill_dir.parent().and_then(Path::parent) {
                    Some(base) => base,
                    None => return ToolResult::err("skill.toml 无法定位 agent 根目录".to_string()),
                };
                if !rclaw_store::resolve_within_base(base, &manifest_path) {
                    return ToolResult::err(
                        "skill.toml 经符号链接或 junction 解析后越出 agent 根目录，已拒绝删除支撑文件"
                            .to_string(),
                    );
                }
                let manifest_source =
                    match crate::skill_entrypoint::read_skill_manifest_source(&manifest_path) {
                        Ok(source) => source,
                        Err(error) => {
                            return ToolResult::err(format!(
                                "读取 skill.toml 失败，已拒绝删除: {error}"
                            ));
                        }
                    };
                match crate::skill_entrypoint::skill_manifest_references_script(
                    base,
                    &skill_dir,
                    &manifest_source,
                    &target,
                ) {
                    Ok(true) => {
                        return ToolResult::err(format!(
                            "refusing to remove `{file_path}`: skill.toml still references this entrypoint script"
                        ));
                    }
                    Ok(false) => {}
                    Err(error) => {
                        return ToolResult::err(format!(
                            "skill.toml 校验失败，已拒绝删除支撑文件: {error}"
                        ));
                    }
                }
            }
        }
        let before = std::fs::read(&target).ok();
        if let Err(e) = std::fs::remove_file(&target) {
            return ToolResult::err(format!("删除文件失败: {e}"));
        }
        crate::skill_entrypoint::notify_skill_script_changed(&target);
        self.ledger.record(
            self.actor(),
            "remove_file",
            name,
            &[(target.clone(), before)],
        );
        if let Err(error) = self.usage.record_patch(name) {
            tracing::warn!(skill = name, %error, "failed to record Skill patch telemetry");
        }
        let _ = self.log_evolution(&format!("removed {name}/{file_path}"));
        ToolResult::ok(
            json!({
                "success": true,
                "skill": name,
                "file": file_path,
                "message": "support file removed"
            })
            .to_string(),
        )
    }

    /// 定位技能目录 + 目标文件，要求技能目录存在（patch/edit/write_file/remove_file）。
    fn locate(&self, name: &str, file_path: &str) -> Result<(PathBuf, PathBuf), String> {
        let skill_dir = self.resolve_skill_dir(name)?;
        if !skill_dir.is_dir() {
            return Err(format!("skill not found: {name}"));
        }
        let target = self.resolve_target(&skill_dir, file_path)?;
        Ok((skill_dir, target))
    }

    fn log_evolution(&self, entry: &str) -> std::io::Result<()> {
        let log = self.skills_dir.join(".evolution.log");
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z");
        let line = format!("[{ts}] {entry}\n");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)?;
        use std::io::Write;
        f.write_all(line.as_bytes())?;
        Ok(())
    }
}

mod helpers;
pub use helpers::note_read;
use helpers::*;
pub(crate) use helpers::{
    archive_skill as archive_skill_for_curator, is_path_redirect as skill_path_is_redirect,
};
#[async_trait]
impl Tool for SkillManageTool {
    fn name(&self) -> &str {
        "skill_manage"
    }

    fn description(&self) -> &str {
        "Edit the persistent Skill library: create / edit / patch / delete / \
         write_file / remove_file. Skills capture HOW to do a class of task; memory captures \
         WHO the user is. This tool NEVER loads, invokes, activates, or marks a Skill as used; \
         read that Skill's SKILL.md in the current turn to use it. Never create/delete a dummy \
         Skill as a placeholder for activation or for skipping work. Land genuine \
         self-improvement lessons as real skill changes, not just notes. Submit exactly ONE \
         skill_manage mutation per model response; observe its result and re-read before deciding \
         another mutation. Batches containing multiple skill_manage calls are rejected before any \
         of them executes. Never combine read and skill_manage in one model response: the read \
         result must be observed before a later response decides the mutation.\n\n\
         - create: new SKILL.md (content = frontmatter or '# ' title + body). name lowercase, <=64 chars.\n\
         - patch (preferred for fixes): old_string -> new_string in SKILL.md or a supporting file. \
         old_string must be unique unless replace_all=true. Read the file first to get exact text.\n\
         - edit: full SKILL.md rewrite (major overhauls). Read first.\n\
         - delete: remove a whole skill directory.\n\
         - write_file: skill.toml, EXPERIENCE.md (hot experience layer), or supporting files under references/ | templates/ | scripts/ | assets/.\n\
         新建带 Manifest 的 Skill 时，先写入清单引用的脚本，再写 skill.toml；清单写盘前会校验脚本真实存在。\n\
         - remove_file: supporting files only; SKILL.md and skill.toml are protected root contracts.\n\n\
         Files must stay under skills/. Protected skills (with a .protected marker) cannot be \
         edited by the autonomous background reviewer at all; pinned skills (with a .pinned \
         marker) are also off-limits to autonomous background writes. Foreground/user-directed \
         patches and edits are allowed on pinned skills; deletion is blocked. Do not create a \
         duplicate skill to work around a protected or pinned target. Autonomous background \
         review may mutate only agent-owned Skills and may create only a genuinely new, \
         unclaimed name; repository- and user-owned names remain read-only.\n\n\
         delete: the background reviewer may ONLY delete a skill it consolidated into an umbrella — \
         pass absorbed_into=<umbrella> (must already exist); the skill is ARCHIVED (recoverable under \
         skills/.archive/), never permanently removed, and a bare delete (no absorbed_into) is refused. \
         Foreground deletes are permanent (hard remove).\n\n\
         Skill-writing conventions: descriptions use three parts — Use when (scenarios + concrete \
         trigger words), Do NOT use (the adjacent scenarios most likely to false-trigger), optional \
         prerequisites (network / vision model / long runtime). Delegation instructions inside skills \
         are goal-based by default (goal + completion criterion + evidence to return); reserve \
         contract-style protocols (stage barriers, message summary contracts) for orchestrations \
         that need precise inter-agent sequencing."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "patch", "edit", "delete", "write_file", "remove_file", "archive", "restore", "rollback"],
                    "description": "The action to perform. archive/restore are foreground-only enable/disable switches: archive moves the skill directory into skills/.archive/ (excluded from the skill catalog); restore brings it back. rollback undoes a previously recorded change by ledger id (foreground-only)."
                },
                "name": {
                    "type": "string",
                    "description": "Skill name (lowercase, hyphens/underscores/dots, <=64 chars). Must match an existing skill for patch/edit/delete/write_file/remove_file/archive; an archived skill for restore. Not used by rollback."
                },
                "id": {
                    "type": "string",
                    "description": "Ledger entry id (required for action=rollback). Each successful content change is recorded in skills/.ledger/ledger.jsonl with its before-snapshot."
                },
                "content": {
                    "type": "string",
                    "description": "Full SKILL.md content (frontmatter or '# ' title + body). Required for create/edit."
                },
                "old_string": {
                    "type": "string",
                    "description": "Text to find for patch. Must be unique unless replace_all=true."
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text for patch. Empty string deletes the matched text."
                },
                "replace_all": {
                    "type": "boolean",
                    "default": false,
                    "description": "patch: replace all occurrences instead of requiring a unique match."
                },
                "file_path": {
                    "type": "string",
                    "description": "skill.toml or a supporting file under references/|templates/|scripts/|assets/ for write_file; a supporting file for remove_file; or any valid file within the skill for patch (default SKILL.md)."
                },
                "file_content": {
                    "type": "string",
                    "description": "Content for write_file."
                },
                "absorbed_into": {
                    "type": "string",
                    "description": "delete only: declare the umbrella skill this skill's content was consolidated into (verified consolidation). The umbrella must already exist. Background review REFUSES a delete without this; a background delete is archived (recoverable) rather than permanently removed. Optional for foreground deletes (hard-delete semantics)."
                }
            },
            "required": ["action", "name"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let _lifecycle = match crate::skill_usage::lock_lifecycle() {
            Ok(guard) => guard,
            Err(error) => return ToolResult::err(error),
        };
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let name = input.get("name").and_then(Value::as_str).unwrap_or("");
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let old_string = input
            .get("old_string")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let new_string = input
            .get("new_string")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .unwrap_or_default();
        let replace_all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let file_path = input
            .get("file_path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let file_content = input
            .get("file_content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let absorbed_into = input
            .get("absorbed_into")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let id = input
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        match action {
            "create" => self.do_create(name, &content),
            "edit" => self.do_edit(name, &content),
            "patch" => self.do_patch(name, &file_path, &old_string, &new_string, replace_all),
            "delete" => self.do_delete(name, &absorbed_into),
            "write_file" => self.do_write_file(name, &file_path, &file_content),
            "remove_file" => self.do_remove_file(name, &file_path),
            "archive" => self.do_archive(name),
            "restore" => self.do_restore(name),
            "rollback" => self.do_rollback(&id),
            other => ToolResult::err(format!(
                "unknown action '{other}'; use create/patch/edit/delete/write_file/remove_file/archive/restore/rollback"
            )),
        }
    }
}

// ===========================================================================
// Review 读工具：限定 skills 目录，并把读过的路径记入共享 tracker（read-before-write）。
// ===========================================================================

/// 后台 review fork 专用的技能读工具：只读 `skills_dir` 下文件，并记录到 tracker。
pub struct SkillReadTool {
    skills_dir: PathBuf,
    tracker: Option<ReviewReadTracker>,
    usage: SkillUsageStore,
}

impl SkillReadTool {
    pub fn new(skills_dir: PathBuf, tracker: ReviewReadTracker) -> Self {
        SkillReadTool {
            usage: SkillUsageStore::new(skills_dir.clone()),
            skills_dir,
            tracker: Some(tracker),
        }
    }
}

#[async_trait]
impl Tool for SkillReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a skill file under the skills/ directory (SKILL.md or a supporting file) before \
         editing it. Reading a file is REQUIRED before the background reviewer can patch/edit/\
         write_file/remove_file it — the content you get back is what you must patch against."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path under skills/ to read, e.g. 'my-skill/SKILL.md' or 'my-skill/references/x.md'."
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let raw = input.get("path").and_then(Value::as_str).unwrap_or("");
        if raw.is_empty() {
            return ToolResult::err("read requires a path under skills/".to_string());
        }
        if raw.contains("..") || raw.starts_with('/') || raw.contains('\\') {
            return ToolResult::err(
                "path must be relative under skills/ (no .. / abs)".to_string(),
            );
        }
        let canon_root = match self.skills_dir.canonicalize() {
            Ok(p) => p,
            Err(e) => return ToolResult::err(format!("skills_dir 不可读: {e}")),
        };
        let Some(name) = raw.split('/').next().filter(|name| !name.is_empty()) else {
            return ToolResult::err("path must start with a top-level Skill directory".to_string());
        };
        let skill_root = canon_root.join(name);
        if !skill_root.is_dir()
            || skill_path_is_redirect(&skill_root)
            || !rclaw_store::resolve_within_base(&canon_root, &skill_root)
        {
            return ToolResult::err(format!(
                "top-level Skill directory is missing, redirected, or outside skills/: {name}"
            ));
        }
        let target = canon_root.join(raw);
        // 词法 starts_with 之外，再解析符号链接/junction，确认真实落点仍在 skills 根下
        // （对齐 rclaw-store validate_agent_path 的解析层；挡住 skills/<name> -> /etc）。
        if !target.starts_with(&canon_root)
            || !rclaw_store::resolve_within_base(&canon_root, &target)
        {
            return ToolResult::err("path escapes skills/ (symlink/junction)".to_string());
        }
        match std::fs::read_to_string(&target) {
            Ok(content) => {
                note_read(&self.tracker, &target, content.as_bytes());
                if let Err(error) = self.usage.record_view_and_use(name) {
                    tracing::warn!(skill = name, %error, "failed to record Skill read telemetry");
                }
                ToolResult::ok(
                    json!({
                        "path": raw,
                        "content": content,
                    })
                    .to_string(),
                )
            }
            Err(e) => ToolResult::err(format!("读取失败: {e}")),
        }
    }
}

// ===========================================================================
// SkillListTool：只读「技能清单」工具。
// ===========================================================================

/// 只读技能清单工具：列出 skill_manage 可按名操作的技能（`skills/<name>/SKILL.md`），
/// 返回 `name`（即 skill_manage 的 `name` 入参）+ 标题 + 描述 + read 路径。
///
/// 后台 review fork 在 transcript 未暴露具体技能路径时，也能先
/// 【发现】现有技能（例如一个可归并进去的 umbrella 技能），再 `read` + `skill_manage(patch)`，
/// 而非盲猜落点。复用 [`rclaw_store::skills::build_summaries_in`] 的扫描与解析（与系统提示里的
/// 技能摘要同源），再按「SKILL.md + 父目录在 skills 根下」筛出 skill_manage 可寻址的子集，
/// 并用 [`rclaw_store::resolve_within_base`] 排除经符号链接逃逸的条目（与读/写沙箱一致）。
pub struct SkillListTool {
    skills_dir: PathBuf,
    usage: SkillUsageStore,
}

impl SkillListTool {
    pub fn new(skills_dir: PathBuf) -> Self {
        SkillListTool {
            usage: SkillUsageStore::new(skills_dir.clone()),
            skills_dir,
        }
    }
}

#[async_trait]
impl Tool for SkillListTool {
    fn name(&self) -> &str {
        "skills_list"
    }

    fn description(&self) -> &str {
        "List skills addressable by skill_manage (each skills/<name>/SKILL.md): name, title, \
         description, read path, lifecycle owner/state, and pinned status. Use this to discover \
         existing skills before read + skill_manage(patch). Autonomous background review may \
         mutate only owner=agent skills; repository/user-owned entries are read-only there."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _input: Value) -> ToolResult {
        let canon_skills = match self.skills_dir.canonicalize() {
            Ok(c) => c,
            Err(e) => return ToolResult::err(format!("skills_dir 不可读: {e}")),
        };
        let summaries = rclaw_store::skills::build_summaries_in(&self.skills_dir);
        let mut out: Vec<Value> = Vec::new();
        for s in summaries {
            let p = Path::new(&s.path);
            // 只保留 skills/<name>/SKILL.md 形态——skill_manage 按 <name> 可操作的子集；
            // loose .md / 包内技能（<pkg>/skills/...，如嵌套子技能）按内层目录名 patch/
            // archive 不了，剔除以免给模型和面板一个操作会失败的 name。
            let Some(name) = rclaw_store::skills::top_level_skill_dir_name(&s.path) else {
                continue;
            };
            // 排除经符号链接逃逸到 skills 根外的条目（与 read/patch 沙箱一致）。
            let parent = p.parent().unwrap_or(Path::new(""));
            if !rclaw_store::resolve_within_base(&canon_skills, parent)
                || skill_path_is_redirect(&self.skills_dir.join(&name))
            {
                continue;
            }
            let lifecycle = self.usage.record(&name);
            let (owner, state, pinned, outcomes) = match lifecycle {
                Ok(record) => {
                    // 拉普拉斯平滑成功率（无样本时省略，不给评审噪声）。
                    let outcomes = record
                        .reliability()
                        .map(|(rate, total)| format!("{:.2}/{total}", rate * 100.0));
                    (
                        record
                            .owner
                            .map(|owner| owner.to_string())
                            .unwrap_or_else(|| "user".to_string()),
                        record.state.to_string(),
                        record.pinned,
                        outcomes,
                    )
                }
                Err(error) => (
                    format!("unknown ({error})"),
                    "unknown".to_string(),
                    true,
                    None,
                ),
            };
            let mut entry = json!({
                "name": name,
                "title": s.title,
                "description": s.description,
                "path": s.path,
                "owner": owner,
                "state": state,
                "pinned": pinned,
            });
            if let Some(outcomes) = outcomes {
                entry["reliability_pct"] = json!(outcomes);
            }
            out.push(entry);
        }
        out.sort_by(|a, b| {
            a.get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .cmp(b.get("name").and_then(Value::as_str).unwrap_or(""))
        });
        ToolResult::ok(json!({ "skills": out }).to_string())
    }
}

#[cfg(test)]
mod tests;
