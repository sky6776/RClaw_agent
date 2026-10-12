//! [`TurnState`] —— 单个 turn 循环全部共享可变状态的归属模块。
//!
//! 拆分前，这些状态袋以独立变量的形式在循环体与工具执行辅助函数的参数、
//! 出参结构体之间来回搬运，靠约定保持一致。
//! 收编后：工具批次与循环本体在 TurnState 这个内部接缝上相遇——批次直接写状态，
//! 循环直接读状态，出参结构体只剩消息本体。
//!
//! 状态分两类：
//! - **粘性状态**（整个 turn 期间累积）：动态工具集、包级 skill 表、公告去重、
//!   已验证媒体标记、seen 标记、媒体发布尝试、skill_manage 成功；
//! - **批次标志**（每个工具批次开始时复位）：skill / 媒体两个 scope 变更标志。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use rclaw_core::ToolSchema;

use crate::dynamic_tools::DynamicToolSet;

/// 单个 turn 的共享可变状态。
pub(crate) struct TurnState {
    /// 动态 Manifest 专用工具集合（C2 收编，见 [`DynamicToolSet`]）。
    pub(crate) dynamic: DynamicToolSet,
    /// 包级 skill 说明表（路径 → 内容），skill 文件变更时由调用方清空。
    pub(crate) loaded_package_skills: HashMap<PathBuf, String>,
    /// skill 公告去重集：同一 turn 内重复读取同一入口只公告一次。
    pub(crate) announced_skill_paths: HashSet<PathBuf>,
    /// 已验证的媒体 marker（完成闸门的认领依据）。
    pub(crate) validated_media_markers: Vec<String>,
    /// 见过的媒体 marker：不随 scope 变化清空，拒绝旧终态快照的重复回放。
    pub(crate) seen_media_markers: HashSet<String>,
    /// 本 turn 是否尝试过媒体发布（完成闸门的输入之一）。
    pub(crate) media_publish_attempted: bool,
    /// 粘性标志：本 turn 是否有 skill_manage 成功执行。
    pub(crate) skill_manage_succeeded: bool,
    /// 粘性标志：本 turn 是否有 memory 工具成功执行（前台自己写了记忆，
    /// 后台 memory 评审当轮跳过——与 skill_manage_succeeded 同语义）。
    pub(crate) memory_write_succeeded: bool,
    /// 本 turn 内 memory 工具连续失败次数（成功即归零）；≥3 次后转为终态拒绝，
    /// 防止记忆写失败把回复循环卡到预算耗尽（Hermes _MAX_CONSOLIDATION_FAILURES 同源）。
    pub(crate) memory_failures: u32,
    /// 批次标志：skill 定义或加载状态在本批次内发生变更，后续调用须取消。
    pub(crate) skill_scope_changed: bool,
    /// 批次标志：TaskList 带回新团队消息，旧媒体交付意图作废。
    pub(crate) media_scope_changed: bool,
    /// Layer 5 选点回滚：本迭代 write/edit/delete 捕获的文件 checkpoint（暂存，
    /// assistant 消息持久化后由 turn 循环统一落盘）。
    pub(crate) pending_file_checkpoints: Vec<super::rewind::FileCheckpoint>,
}

impl TurnState {
    /// 以静态 schema 为动态工具的冲突底线构造空状态；每 turn 新建即完成重置。
    pub(crate) fn new(static_schemas: &[ToolSchema]) -> Self {
        Self {
            dynamic: DynamicToolSet::new(static_schemas),
            loaded_package_skills: HashMap::new(),
            announced_skill_paths: HashSet::new(),
            validated_media_markers: Vec::new(),
            seen_media_markers: HashSet::new(),
            media_publish_attempted: false,
            skill_manage_succeeded: false,
            memory_write_succeeded: false,
            memory_failures: 0,
            skill_scope_changed: false,
            media_scope_changed: false,
            pending_file_checkpoints: Vec::new(),
        }
    }

    /// 工具批次开始：复位两个批次标志。粘性状态（skill_manage 成功、
    /// 已验证 marker、seen）保持不动。
    pub(crate) fn begin_batch(&mut self) {
        self.skill_scope_changed = false;
        self.media_scope_changed = false;
    }

    /// 媒体 scope 变化（TaskList 收到新团队消息）：废弃旧的已验证交付意图，
    /// 只接纳后续批次带回的新 marker。seen 保留——否则同一完成快照可把
    /// 已撤销的 marker 重新带回。
    pub(crate) fn handle_media_scope_change(&mut self) {
        self.validated_media_markers.clear();
    }

    /// 记录一个新验证的媒体 marker：经 seen 去重后才进入 validated。
    /// 同一 marker 重复到达是旧终态快照的回放，不得重复认领。
    pub(crate) fn accept_validated_marker(&mut self, marker: &str) {
        if self.seen_media_markers.insert(marker.to_string()) {
            self.validated_media_markers.push(marker.to_string());
        }
    }
}

#[cfg(test)]
mod tests;
