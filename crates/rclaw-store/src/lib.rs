//! rclaw-store：文件系统持久化层。
//!
//! 负责：
//! - 初始化 agent 目录布局（`Layout`）
//! - 原子写（tmp + rename）与确保目录/文件存在
//! - session JSONL 读写（含历史裁剪、孤儿 tool_result 跳过、原子工具链追加）
//! - 长期记忆 / daily notes / skills 摘要
//! - cron.json 持久化（`CronJob` 定义 + load/save）
//! - 百分号编码的 session 键与路径校验
//!
//! 设计要点：
//! - 用 tmp+rename 替代 `ftruncate` 回滚做原子性；
//! - session「两条记录原子追加」改为「拼接成单次 write_all」，消除半条工具链窗口；
//! - 用 `std::fs` + `chrono`，不再依赖 TLSF / cJSON / SPIFFS。

pub mod bots;
pub mod config_doc;
pub mod cron_store;
pub mod fs;
pub mod memory;
pub mod path_util;
pub mod redact;
pub mod session;
pub mod skills;
pub mod toml_file;

pub use bots::{
    is_valid_bot_name, is_valid_group_id, BotDef, BotStore, GroupActivity, GroupLine, GroupRoom,
    GroupStore, GROUP_ACTIVITY_MAX, GROUP_LOG_MAX_LINES, GROUP_MAX_MEMBERS, GROUP_MIN_MEMBERS,
};
pub use cron_store::{CronJob, CronKind};
pub use fs::{atomic_write, ensure_dir, ensure_file, ensure_parent_dir, init_layout, Layout};
pub use memory::{scan_skill_text, MemOpResult, MemoryBatchOp, MemoryStore, MemoryTarget};
pub use path_util::{percent_encode_key, resolve_within_base, validate_agent_path};
pub use redact::{redact_sensitive_shell_command, redact_sensitive_text};
pub use session::{
    append_message, append_tool_chain, clear_session, load_history,
    synthesize_aborted_tool_results, SessionStore, ABORTED_TOOL_RESULT_PLACEHOLDER,
};
pub use skills::SkillSummary;
pub use toml_file::{EditValue, RclawToml};

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

    #[error("消息过大（{size} > {max}）")]
    MessageTooLarge { size: usize, max: usize },

    #[error("其它: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Crate 版本(关于页「版本信息」用)。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
