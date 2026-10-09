//! 技能变更台账（hermes-vs-rclaw 自进化分析 M2）。
//!
//! 每次 skill_manage 内容变更成功后，把「变更前」的受影响文件内容以内容寻址 blob
//! 落盘，并在 jsonl 台账追加一条记录；`skill_manage(action=rollback, id=…)` 按
//! 条目恢复 before 状态。Hermes `skill_ledger`（sha256 blob + jsonl 台账）的本地化
//! 简版：blob 按 `条目id-序号-文件名` 存放（不做跨条目去重，删除语义简单正确），
//! 台账环形保留最近 [`MAX_LEDGER_ENTRIES`] 条。
//!
//! 存放位置：`skills_dir/.ledger/`——技能目录扫描会跳过它（无 SKILL.md、无
//! `skills/` 子目录），不会污染技能清单；curator 归档同理不受影响。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use rclaw_store::atomic_write;

/// 台账环形保留的条目数上限。
const MAX_LEDGER_ENTRIES: usize = 50;

/// 一条变更记录（jsonl 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// 条目 id（时间戳派生，单调）。
    pub id: String,
    /// 记录时间（本地时区，人读格式）。
    pub ts: String,
    /// foreground | background_review。
    pub actor: String,
    /// skill_manage 动作名（create/edit/patch/write_file/remove_file/rollback）。
    pub action: String,
    /// 受影响技能名（rollback 条目为空）。
    pub skill: String,
    /// rollback 条目指向的被撤销条目 id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_of: Option<String>,
    /// 变更前文件快照。
    pub files: Vec<LedgerFile>,
}

/// 单个文件的 before 快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerFile {
    /// 相对 skills_dir 的路径（正斜杠显示）。
    pub path: String,
    /// 变更前是否存在。
    pub existed_before: bool,
    /// before 内容的 sha256（blob 键；不存在时为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// 技能变更台账。
pub struct SkillLedger {
    skills_dir: PathBuf,
    root: PathBuf,
    blobs: PathBuf,
}

impl SkillLedger {
    pub fn new(skills_dir: &Path) -> Self {
        let root = skills_dir.join(".ledger");
        SkillLedger {
            skills_dir: skills_dir.to_path_buf(),
            blobs: root.join("blobs"),
            root,
        }
    }

    /// 记录一次成功变更：`files` 为受影响的绝对路径，调用方须在【写入前】读出
    /// 它们的旧内容传入（None = 变更前不存在）。失败仅告警不阻断——台账是遥测
    /// 与回滚便利，不是正确性依赖。
    pub fn record(
        &self,
        actor: &str,
        action: &str,
        skill: &str,
        before: &[(PathBuf, Option<Vec<u8>>)],
    ) {
        if let Err(e) = self.record_inner(actor, action, skill, None, before) {
            tracing::warn!("skill ledger 记录失败（不影响变更本身）: {e}");
        }
    }

    fn record_inner(
        &self,
        actor: &str,
        action: &str,
        skill: &str,
        undo_of: Option<String>,
        before: &[(PathBuf, Option<Vec<u8>>)],
    ) -> Result<(), String> {
        let id = format!("{}-{:x}", chrono::Local::now().format("%Y%m%d-%H%M%S"), {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        });
        let mut files = Vec::new();
        // 目标路径可能来自 canonicalize 后的技能目录（Windows 下带 \\?\ 前缀），
        // 与非规范化的 skills_dir 前缀不匹配——两种前缀都尝试剥离。
        let canon_base = self
            .skills_dir
            .canonicalize()
            .unwrap_or_else(|_| self.skills_dir.clone());
        for (idx, (path, content)) in before.iter().enumerate() {
            let rel = path
                .strip_prefix(&self.skills_dir)
                .or_else(|_| path.strip_prefix(&canon_base))
                .map_err(|_| format!("{} 不在 skills_dir 内", path.display()))?
                .to_string_lossy()
                .replace('\\', "/");
            let mut f = LedgerFile {
                path: rel,
                existed_before: content.is_some(),
                sha256: None,
            };
            if let Some(bytes) = content {
                let hex = format!("{:x}", Sha256::digest(bytes));
                let blob = self.blobs.join(format!("{id}-{idx}.bin"));
                fs::create_dir_all(&self.blobs).map_err(|e| e.to_string())?;
                atomic_write(&blob, bytes).map_err(|e| e.to_string())?;
                f.sha256 = Some(hex);
            }
            files.push(f);
        }
        let entry = LedgerEntry {
            id,
            ts: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            actor: actor.to_string(),
            action: action.to_string(),
            skill: skill.to_string(),
            undo_of,
            files,
        };
        let mut entries = self.read_entries()?;
        entries.push(entry);
        // 环形保留：裁掉最旧条目及其 blob（blob 名以条目 id 为前缀，无跨条目共享）。
        while entries.len() > MAX_LEDGER_ENTRIES {
            let oldest = entries.remove(0);
            self.prune_blobs_of(&oldest.id);
        }
        self.write_entries(&entries)
    }

    /// 按条目 id 回滚：把每个 before blob 写回原路径；变更前不存在的文件删除。
    /// 返回（恢复的文件数, 删除的文件数）。
    pub fn rollback(&self, id: &str) -> Result<(usize, usize), String> {
        let entries = self.read_entries()?;
        let entry = entries
            .iter()
            .find(|e| e.id == id && e.undo_of.is_none())
            .ok_or_else(|| format!("ledger entry not found: {id}"))?;
        let mut restored = 0usize;
        let mut removed = 0usize;
        for (idx, f) in entry.files.iter().enumerate() {
            let abs = self.skills_dir.join(&f.path);
            // 路径沙箱：台账可能被外部改写，恢复前复核每个路径仍在 skills 根内。
            if !rclaw_store::resolve_within_base(&self.skills_dir, &abs) {
                return Err(format!("ledger path escapes skills_dir: {}", f.path));
            }
            match f.sha256.as_deref() {
                Some(hex) => {
                    let blob = self.blobs.join(format!("{}-{idx}.bin", entry.id));
                    let bytes = fs::read(&blob)
                        .map_err(|e| format!("blob 读取失败（{}）: {e}", blob.display()))?;
                    let actual = format!("{:x}", Sha256::digest(&bytes));
                    if actual != hex {
                        return Err(format!("blob 校验和不匹配（{}）", blob.display()));
                    }
                    if let Some(parent) = abs.parent() {
                        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    atomic_write(&abs, &bytes).map_err(|e| e.to_string())?;
                    restored += 1;
                }
                None => {
                    if abs.exists() {
                        fs::remove_file(&abs).map_err(|e| e.to_string())?;
                        removed += 1;
                    }
                }
            }
        }
        // 回滚本身也留痕，且标记 undo_of 防止对回滚条目再套娃回滚。
        self.record_inner(
            "rollback",
            "rollback",
            &entry.skill,
            Some(entry.id.clone()),
            &[],
        )?;
        Ok((restored, removed))
    }

    fn ledger_path(&self) -> PathBuf {
        self.root.join("ledger.jsonl")
    }

    fn read_entries(&self) -> Result<Vec<LedgerEntry>, String> {
        let raw = match fs::read_to_string(self.ledger_path()) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.to_string()),
        };
        raw.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(|e| format!("台账行解析失败: {e}")))
            .collect()
    }

    fn write_entries(&self, entries: &[LedgerEntry]) -> Result<(), String> {
        let mut buf = String::new();
        for e in entries {
            let line = serde_json::to_string(e).map_err(|e| e.to_string())?;
            buf.push_str(&line);
            buf.push('\n');
        }
        fs::create_dir_all(&self.root).map_err(|e| e.to_string())?;
        atomic_write(self.ledger_path(), buf.as_bytes()).map_err(|e| e.to_string())
    }

    fn prune_blobs_of(&self, entry_id: &str) {
        if let Ok(blobs) = fs::read_dir(&self.blobs) {
            for b in blobs.flatten() {
                let name = b.file_name();
                if name.to_string_lossy().starts_with(entry_id) {
                    let _ = fs::remove_file(b.path());
                }
            }
        }
    }
}
