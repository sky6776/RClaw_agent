//! rclaw-runtime：编排层。
//!
//! 把 store / llm / tools 组装成可运行的 agent：
//! - [`context`]：系统提示构建；
//! - [`AgentLoop`]：ReAct 主循环；
//! - [`CronService`] / [`Heartbeat`]：作为 inbound 生产者的后台服务。
//!
//! 顶层二进制（rclaw-cli）负责装配这些组件并 spawn 任务。

pub mod agent_loop;
pub mod agent_tools;
pub mod background_wake;
pub mod bots;
pub mod context;
pub mod cron;
pub(crate) mod dynamic_tools;
pub mod heartbeat;
pub mod options;
pub mod session_handoff;
pub mod slash;
pub mod turn;

pub use agent_loop::{AgentLoop, SessionLifecycle};
pub use agent_tools::{
    AgentCancelTool, AgentResumeTool, AgentStatusTool, AgentToolRuntime, AgentWaitTool,
    ClaudeAgentTool, PlanRunTool, SendMessageTool, TaskCreateTool, TaskGetTool, TaskListTool,
    TaskUpdateTool, TeamCreateTool, TeamDeleteTool,
};
pub use bots::{
    bot_inbox_chat_id, chat_key, group_member_chat_id, sanitize_control_frames, BotRegistry,
    BotRouter, BotTurnCtx, HandoffError, HandoffOutcome, MessageAgentTool, BOT_CHANNEL,
    HANDOFF_MAX_CHAIN, HANDOFF_MESSAGE_MAX_CHARS, HANDOFF_REPLY_TIMEOUT_SECS,
};
pub use context::{
    build_bot_system_prompt, build_bot_system_prompt_with, build_dynamic_context_snapshot,
    build_system_prompt, build_system_prompt_for_tools, build_system_prompt_with,
    turn_context_suffix, DynamicContextSnapshot, HostKind,
};
pub use cron::CronService;
pub use heartbeat::Heartbeat;
pub use options::AgentOptions;
pub use slash::{CommandOutcome, SlashRegistry};
pub use turn::{estimate_context_usage, ContextUsageSnapshot, TurnKey, TurnManager};

pub use background_wake::BackgroundWaker;

/// inbound 方向发送端类型（channel/cron/heartbeat → agent loop）。
pub type InboundSender = tokio::sync::mpsc::Sender<rclaw_core::BusMessage>;

/// Crate 版本(关于页「版本信息」用)。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
