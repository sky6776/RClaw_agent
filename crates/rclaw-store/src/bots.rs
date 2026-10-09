//! Bot Mode 的持久化：bot 定义（`agent/bots/<name>.toml`）与群聊房间
//! （`agent/bots/groups/<id>.json`）。
//!
//! 本模块只负责「数据形状 + 读写」，编排逻辑（交接、轮转）在 runtime / 桌面层。
//! 每 bot 的独立数据目录 `agent/bots/<name>/memory/` 也在创建时一并建立。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fs::{atomic_write, ensure_dir};
use crate::{Result, StoreError};

/// 保留名：与群聊 @user 升级语义、主 persona 身份及全量指称冲突，不得用作 bot 名
/// （对齐 Hermes 的 reserved-token 防劫持设计）。
pub const RESERVED_BOT_NAMES: [&str; 7] = [
    "user", "rclaw", "main", "all", "everyone", "system", "groups",
];

/// bot 名字约束：小写字母/数字开头，可含 `-`/`_`，长度 1..=32，且不得是保留名。
/// 同时是 chat_id 前缀（`bot-<name>`）与目录名，字符集与 session 文件名安全集合兼容。
pub fn is_valid_bot_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    let rest: usize = chars
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_')
        .count();
    rest == name.len() - 1 && name.len() <= 32 && !RESERVED_BOT_NAMES.contains(&name)
}

/// 群聊 id 约束：8 位十六进制（与 cron id 同风格）。
pub fn is_valid_group_id(id: &str) -> bool {
    id.len() == 8
        && id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// 单个 bot 的定义（`agent/bots/<name>.toml`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotDef {
    /// 显示名（花名册与归属前缀用），1..=32 字符。
    pub title: String,
    /// 职责一句话（花名册行），0..=120 字符。
    #[serde(default)]
    pub description: String,
    /// 角色系统提示正文（人格 + 职责边界），非空。
    pub persona: String,
    /// 固定使用的 provider 名（`[[llm.providers]]` 之一）；None = 跟随主 provider。
    #[serde(default)]
    pub provider: Option<String>,
    /// 收窄的工具面（这些工具对 bot 不可见），默认空。
    #[serde(default)]
    pub denied_tools: Vec<String>,
}

/// bot 定义存储：`agent/bots/` 目录式 CRUD。
#[derive(Debug, Clone)]
pub struct BotStore {
    bots_dir: PathBuf,
}

impl BotStore {
    /// `base` 为 agent 存储根（`Layout::base`）。
    pub fn new(base: impl AsRef<Path>) -> Self {
        BotStore {
            bots_dir: base.as_ref().join("bots"),
        }
    }

    fn def_path(&self, name: &str) -> Result<PathBuf> {
        // 保留名单独报错：对用户而言 "user" 之类看起来完全合法，
        // 笼统的「非法名」会误导排查方向。
        if RESERVED_BOT_NAMES.contains(&name) {
            return Err(StoreError::InvalidPath(format!(
                "「{name}」是保留名，不可用作 bot 名"
            )));
        }
        if !is_valid_bot_name(name) {
            return Err(StoreError::InvalidPath(format!("非法 bot 名: {name}")));
        }
        Ok(self.bots_dir.join(format!("{name}.toml")))
    }

    /// 列出全部 bot（按名排序）。单个文件损坏时跳过并告警，不影响其余。
    pub fn list(&self) -> Vec<(String, BotDef)> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.bots_dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // 文件名即 bot 名（chat_id 前缀与数据目录名）：非法名字的散落文件
            // 直接跳过——例如 "..toml" 会让 persona 解析到 agent 根目录。
            if !is_valid_bot_name(stem) {
                tracing::warn!("跳过非法名字的 bot 定义文件 {}: {stem}", path.display());
                continue;
            }
            match std::fs::read_to_string(&path)
                .map_err(StoreError::Io)
                .and_then(|s| {
                    toml::from_str::<BotDef>(&s).map_err(|e| StoreError::Other(e.to_string()))
                }) {
                Ok(def) => out.push((stem.to_string(), def)),
                Err(e) => tracing::warn!("跳过无法解析的 bot 定义 {}: {e}", path.display()),
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// 读取单个 bot。
    pub fn get(&self, name: &str) -> Result<BotDef> {
        let path = self.def_path(name)?;
        let raw = std::fs::read_to_string(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StoreError::Other(format!("bot 不存在: {name}")),
            _ => StoreError::Io(e),
        })?;
        toml::from_str(&raw).map_err(|e| StoreError::Other(format!("bot 定义解析失败: {e}")))
    }

    /// 创建 / 覆盖保存 bot 定义，并确保其数据目录存在。
    /// `title` 与 `persona` 去空白后必须非空；`denied_tools` 去重。
    pub fn save(&self, name: &str, mut def: BotDef) -> Result<()> {
        def.title = def.title.trim().to_string();
        def.description = def.description.trim().to_string();
        def.persona = def.persona.trim().to_string();
        if def.title.is_empty() {
            return Err(StoreError::Other("bot 显示名不能为空".into()));
        }
        if def.title.chars().count() > 32 {
            return Err(StoreError::Other("bot 显示名超过 32 字符".into()));
        }
        if def.description.chars().count() > 120 {
            return Err(StoreError::Other("bot 职责描述超过 120 字符".into()));
        }
        if def.persona.is_empty() {
            return Err(StoreError::Other("bot persona 不能为空".into()));
        }
        def.denied_tools.sort();
        def.denied_tools.dedup();

        let path = self.def_path(name)?;
        ensure_dir(&self.bots_dir)?;
        ensure_dir(self.bots_dir.join(name).join("memory"))?;
        // 校验 provider 名非空字符串（存在性由装配层对配置校验）。
        if let Some(p) = def.provider.as_deref() {
            if p.trim().is_empty() {
                def.provider = None;
            }
        }
        let body = toml::to_string_pretty(&def)
            .map_err(|e| StoreError::Other(format!("bot 定义序列化失败: {e}")))?;
        atomic_write(&path, body.as_bytes())
    }

    /// 删除 bot 定义与其数据目录。返回是否真的删除了（不存在 = false）。
    pub fn delete(&self, name: &str) -> Result<bool> {
        let path = self.def_path(name)?;
        if !path.exists() {
            return Ok(false);
        }
        std::fs::remove_file(&path).map_err(StoreError::Io)?;
        let data_dir = self.bots_dir.join(name);
        if data_dir.is_dir() {
            if let Err(e) = std::fs::remove_dir_all(&data_dir) {
                tracing::warn!("删除 bot 数据目录失败（定义已删除）: {e}");
            }
        }
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// 群聊房间
// ---------------------------------------------------------------------------

/// 房间日志上限（超出丢最旧，seq 保持单调不减）。
pub const GROUP_LOG_MAX_LINES: usize = 500;
/// 活动流上限。
pub const GROUP_ACTIVITY_MAX: usize = 200;

/// 群聊上限（对齐 Hermes Bot Mode 的有界协调面板）。
pub const GROUP_MIN_MEMBERS: usize = 2;
pub const GROUP_MAX_MEMBERS: usize = 6;

/// 一条房间消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupLine {
    /// 单调递增序号（水位线锚点）。
    pub seq: u64,
    /// 发言者：`"user"` 或 `"bot:<name>"`。
    pub who: String,
    pub text: String,
    /// unix 秒。
    pub ts: i64,
}

/// 一条活动流记录（非发言类事件：排队/工作/弃权/收敛/超时等）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupActivity {
    pub ts: i64,
    /// 事件类型：queued / working / passed / settled / capped / failed / timeout。
    pub kind: String,
    /// 相关成员（bot 名；群级事件为空）。
    #[serde(default)]
    pub who: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 群聊房间（`agent/bots/groups/<id>.json`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupRoom {
    pub id: String,
    pub name: String,
    /// 成员 bot 名（2..=6，创建时校验）。
    pub members: Vec<String>,
    pub created_ts: i64,
    /// 下一条消息的 seq。
    #[serde(default)]
    pub next_seq: u64,
    /// 是否有编排正在进行（用户发送后、收敛/封顶前为 true）。
    #[serde(default)]
    pub running: bool,
    /// 房间日志（有界）。
    #[serde(default)]
    pub lines: Vec<GroupLine>,
    /// 每成员已读水位线（seq；增量 = 水位线之后的消息）。
    #[serde(default)]
    pub watermarks: BTreeMap<String, u64>,
    /// 活动流（有界）。
    #[serde(default)]
    pub activities: Vec<GroupActivity>,
}

impl GroupRoom {
    /// 创建新房间（校验成员数与成员名）。
    pub fn new(id: &str, name: &str, members: Vec<String>, created_ts: i64) -> Result<Self> {
        if !is_valid_group_id(id) {
            return Err(StoreError::InvalidPath(format!("非法群聊 id: {id}")));
        }
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 40 {
            return Err(StoreError::Other("群聊名须为 1..=40 字符".into()));
        }
        let members: Vec<String> = members.into_iter().map(|m| m.trim().to_string()).collect();
        if members.iter().any(|m| !is_valid_bot_name(m)) {
            return Err(StoreError::Other("群成员名非法".into()));
        }
        if members.len() < GROUP_MIN_MEMBERS || members.len() > GROUP_MAX_MEMBERS {
            return Err(StoreError::Other(format!(
                "群成员数须在 {GROUP_MIN_MEMBERS}..={GROUP_MAX_MEMBERS}"
            )));
        }
        if members
            .iter()
            .any(|m| members.iter().filter(|x| x == &m).count() > 1)
        {
            return Err(StoreError::Other("群成员不得重复".into()));
        }
        Ok(GroupRoom {
            id: id.to_string(),
            name: name.to_string(),
            members,
            created_ts,
            next_seq: 1,
            running: false,
            lines: Vec::new(),
            watermarks: BTreeMap::new(),
            activities: Vec::new(),
        })
    }

    /// 追加一条发言并按需裁剪；返回其 seq。
    pub fn push_line(&mut self, who: &str, text: &str, ts: i64) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.lines.push(GroupLine {
            seq,
            who: who.to_string(),
            text: text.to_string(),
            ts,
        });
        if self.lines.len() > GROUP_LOG_MAX_LINES {
            let drop = self.lines.len() - GROUP_LOG_MAX_LINES;
            self.lines.drain(..drop);
        }
        seq
    }

    /// 追加活动流并按需裁剪。
    pub fn push_activity(&mut self, kind: &str, who: &str, detail: Option<String>, ts: i64) {
        self.activities.push(GroupActivity {
            ts,
            kind: kind.to_string(),
            who: who.to_string(),
            detail,
        });
        if self.activities.len() > GROUP_ACTIVITY_MAX {
            let drop = self.activities.len() - GROUP_ACTIVITY_MAX;
            self.activities.drain(..drop);
        }
    }

    /// 读取某成员水位线之后的增量日志。
    pub fn lines_after(&self, member: &str) -> &[GroupLine] {
        let mark = self.watermarks.get(member).copied().unwrap_or(0);
        let idx = self.lines.partition_point(|l| l.seq <= mark);
        &self.lines[idx..]
    }

    /// 推进成员水位线到当前最新 seq。
    pub fn advance_watermark(&mut self, member: &str) {
        let latest = self.lines.last().map(|l| l.seq).unwrap_or(0);
        self.watermarks.insert(member.to_string(), latest);
    }
}

/// 群聊房间存储。
#[derive(Debug, Clone)]
pub struct GroupStore {
    groups_dir: PathBuf,
}

impl GroupStore {
    /// `base` 为 agent 存储根。
    pub fn new(base: impl AsRef<Path>) -> Self {
        GroupStore {
            groups_dir: base.as_ref().join("bots").join("groups"),
        }
    }

    fn room_path(&self, id: &str) -> Result<PathBuf> {
        if !is_valid_group_id(id) {
            return Err(StoreError::InvalidPath(format!("非法群聊 id: {id}")));
        }
        Ok(self.groups_dir.join(format!("{id}.json")))
    }

    /// 列出全部房间（轻量摘要：不含日志与活动流全文）。
    pub fn list(&self) -> Vec<GroupRoom> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.groups_dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read(&path)
                .map_err(StoreError::Io)
                .and_then(|b| serde_json::from_slice::<GroupRoom>(&b).map_err(StoreError::Json))
            {
                Ok(mut room) => {
                    room.lines.clear();
                    room.activities.clear();
                    out.push(room);
                }
                Err(e) => tracing::warn!("跳过无法解析的群聊房间 {}: {e}", path.display()),
            }
        }
        out.sort_by_key(|r| r.created_ts);
        out
    }

    /// 读取完整房间。
    pub fn get(&self, id: &str) -> Result<GroupRoom> {
        let path = self.room_path(id)?;
        let raw = std::fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StoreError::Other(format!("群聊不存在: {id}")),
            _ => StoreError::Io(e),
        })?;
        serde_json::from_slice(&raw).map_err(StoreError::Json)
    }

    /// 保存房间（读-改-写在调用方完成）。
    pub fn save(&self, room: &GroupRoom) -> Result<()> {
        let path = self.room_path(&room.id)?;
        ensure_dir(&self.groups_dir)?;
        let body = serde_json::to_vec(room).map_err(StoreError::Json)?;
        atomic_write(&path, &body)
    }

    /// 删除房间。
    pub fn delete(&self, id: &str) -> Result<bool> {
        let path = self.room_path(id)?;
        if !path.exists() {
            return Ok(false);
        }
        std::fs::remove_file(&path).map_err(StoreError::Io)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def() -> BotDef {
        BotDef {
            title: "研究员".into(),
            description: "负责检索与取证".into(),
            persona: "你是一名严谨的研究员。".into(),
            provider: None,
            denied_tools: vec![],
        }
    }

    #[test]
    fn bot_name_validation() {
        assert!(is_valid_bot_name("a"));
        assert!(is_valid_bot_name("research-2_x"));
        assert!(!is_valid_bot_name("A")); // 大写
        assert!(!is_valid_bot_name("-x")); // 非法开头
        assert!(!is_valid_bot_name("x!")); // 非法字符
        assert!(!is_valid_bot_name("")); // 空
        assert!(!is_valid_bot_name(&"x".repeat(33))); // 超长
                                                      // 保留名拒绝（@user 升级 / 主 persona 身份 / 全量指称不得被劫持）。
        for reserved in RESERVED_BOT_NAMES {
            assert!(!is_valid_bot_name(reserved), "{reserved} 是保留名");
        }
        assert!(is_valid_bot_name("userbot")); // 前缀撞保留名但整体不同 → 允许
    }

    #[test]
    fn bot_store_roundtrip_and_validation() {
        let dir = tempfile::tempdir().unwrap();
        let store = BotStore::new(dir.path());

        store.save("researcher", def()).unwrap();
        assert_eq!(store.get("researcher").unwrap().title, "研究员");
        assert!(dir
            .path()
            .join("bots")
            .join("researcher")
            .join("memory")
            .is_dir());

        // 空标题 / 空 persona 拒绝。
        let mut bad = def();
        bad.title = "  ".into();
        assert!(store.save("x", bad.clone()).is_err());
        let mut bad2 = def();
        bad2.persona = "".into();
        assert!(store.save("x", bad2).is_err());

        // 非法名字直接拒绝，不落盘。
        assert!(store.save("Bad Name", def()).is_err());
        assert!(store.get("Bad Name").is_err());

        // list 按名排序；覆盖保存后读取到新值。
        store.save("alpha", def()).unwrap();
        let names: Vec<String> = store.list().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["alpha".to_string(), "researcher".to_string()]);

        let mut updated = def();
        updated.description = "更新后的职责".into();
        store.save("researcher", updated).unwrap();
        assert_eq!(store.get("researcher").unwrap().description, "更新后的职责");

        // 删除：定义与数据目录一起消失。
        assert!(store.delete("researcher").unwrap());
        assert!(store.get("researcher").is_err());
        assert!(!dir.path().join("bots").join("researcher").exists());
        assert!(!store.delete("researcher").unwrap());
    }

    #[test]
    fn group_room_construction_and_watermarks() {
        // 成员数与重复校验。
        assert!(GroupRoom::new("abcd1234", "投研组", vec!["a".into()], 0).is_err());
        assert!(GroupRoom::new(
            "abcd1234",
            "投研组",
            (0..7).map(|i| format!("b{i}")).collect(),
            0
        )
        .is_err());
        assert!(GroupRoom::new("abcd1234", "投研组", vec!["a".into(), "a".into()], 0).is_err());
        assert!(GroupRoom::new("ZZZZ9999", "x", vec!["a".into(), "b".into()], 0).is_err());

        let mut room = GroupRoom::new(
            "abcd1234",
            " 投研组 ",
            vec!["researcher".into(), "critic".into()],
            42,
        )
        .unwrap();
        assert_eq!(room.name, "投研组");

        // 增量：水位线之后的消息；push 后推进。
        room.push_line("user", "任务", 1);
        room.push_line("bot:researcher", "结论", 2);
        assert_eq!(room.lines_after("researcher").len(), 2);
        room.advance_watermark("researcher");
        room.push_line("bot:critic", "质疑", 3);
        assert_eq!(room.lines_after("researcher").len(), 1);
        assert_eq!(room.lines_after("researcher")[0].who, "bot:critic");
        assert_eq!(room.lines_after("critic").len(), 3);
    }

    #[test]
    fn group_room_bounds_log_and_activity() {
        let mut room = GroupRoom::new("abcd1234", "g", vec!["a".into(), "b".into()], 0).unwrap();
        for i in 0..(GROUP_LOG_MAX_LINES + 50) as u64 {
            room.push_line("user", &format!("m{i}"), i as i64);
        }
        assert_eq!(room.lines.len(), GROUP_LOG_MAX_LINES);
        // seq 单调不减（裁剪只删最旧）。
        assert!(room.lines.windows(2).all(|w| w[0].seq < w[1].seq));

        for i in 0..(GROUP_ACTIVITY_MAX + 10) as i64 {
            room.push_activity("passed", "a", None, i);
        }
        assert_eq!(room.activities.len(), GROUP_ACTIVITY_MAX);
    }

    #[test]
    fn group_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = GroupStore::new(dir.path());

        let mut room = GroupRoom::new("abcd1234", "组", vec!["a".into(), "b".into()], 7).unwrap();
        room.push_line("user", "hi", 1);
        room.push_activity("settled", "", None, 2);
        store.save(&room).unwrap();

        let loaded = store.get("abcd1234").unwrap();
        assert_eq!(loaded.name, "组");
        assert_eq!(loaded.lines.len(), 1);
        assert_eq!(loaded.activities.len(), 1);

        // list 摘要不带日志全文，但保留成员与 running。
        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].members, vec!["a".to_string(), "b".to_string()]);
        assert!(listed[0].lines.is_empty());

        assert!(store.delete("abcd1234").unwrap());
        assert!(!store.delete("abcd1234").unwrap());
    }
}
