//! rclaw-store：持久化层。
//!
//! 负责：
//! - 初始化 agent 目录布局（`Layout`）
//! - 原子写（tmp + rename）与确保目录/文件存在
//! - `agent/state.sqlite` 机器状态库（`db`）：会话消息（含非破坏性 compaction boundary、
//!   历史裁剪、孤儿 tool_result 跳过、事务级工具链追加）、rewind 文件 checkpoint、
//!   usage 锚点（`session`）；PlanRun / WorkflowRun / 团队 journal 与 kv（`journal`）
//! - 长期记忆 / daily notes / skills 摘要（文件）
//! - cron.json 持久化（`CronJob` 定义 + load/save）
//! - 百分号编码的键与路径校验
//!
//! 设计要点：
//! - 给人读改的内容留在文件（tmp+rename 原子写）；机器状态进 SQLite（WAL + 事务）；
//! - 消息有稳定身份 `Message::id`，压缩只追加 boundary、不重写历史；
//! - 用 `std::fs` + `chrono`，不再依赖 TLSF / cJSON / SPIFFS。

pub mod bots;
pub mod config_doc;
pub mod cron_store;
pub mod db;
pub mod fs;
pub mod journal;
pub mod memory;
pub mod path_util;
pub mod redact;
pub mod session;
pub mod skills;
pub mod toml_file;
pub mod workflow_home;

pub use bots::{
    is_valid_bot_name, is_valid_group_id, BotDef, BotStore, GroupActivity, GroupLine, GroupRoom,
    GroupStore, GROUP_ACTIVITY_MAX, GROUP_LOG_MAX_LINES, GROUP_MAX_MEMBERS, GROUP_MIN_MEMBERS,
};
pub use cron_store::{CronJob, CronKind};
pub use db::Db;
pub use fs::{atomic_write, ensure_dir, ensure_file, ensure_parent_dir, init_layout, Layout};
pub use journal::{
    JournalStore, JOURNAL_EXEC_LOG, JOURNAL_PLAN, JOURNAL_TEAM_STATE, JOURNAL_WORKFLOW,
    KV_TEAM_INBOX,
};
pub use memory::{scan_skill_text, MemOpResult, MemoryBatchOp, MemoryStore, MemoryTarget};
pub use path_util::{percent_encode_key, resolve_within_base, validate_agent_path};
pub use redact::{redact_sensitive_shell_command, redact_sensitive_text};
pub use session::{
    append_message, append_tool_chain, clear_session, load_history, repair_tool_chains,
    synthesize_aborted_tool_results, tool_use_counts, FileCheckpointRecord, SearchRow,
    SessionStats, SessionStore, SessionSummary, UsageAnchorRecord, ABORTED_TOOL_RESULT_PLACEHOLDER,
};
pub use skills::SkillSummary;
pub use toml_file::{EditValue, RclawToml};
pub use workflow_home::{workflow_home, workflows_dir};

use thiserror::Error;

/// store 层错误。
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("路径非法: {0}")]
    InvalidPath(String),

    #[error("SQLite 错误: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("其它: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Crate 版本(关于页「版本信息」用)。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
