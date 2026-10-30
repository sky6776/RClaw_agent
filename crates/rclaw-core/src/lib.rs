//! rclaw-core：RClaw 的领域内核（无 IO 假设）。
//!
//! 本 crate 只定义规范化数据模型与通道契约，不依赖任何运行时/IO 库，
//! 使其可被测试、宿主嵌入、FFI 边界自由复用。
//!
//! 模块组成：
//! - [`model`]：消息/内容块/请求/响应/流式事件等核心类型。
//! - [`bus`]：通道间消息 `BusMessage` 与通道名常量。

pub mod bus;
pub use bus::{ChannelCaps, RuntimeChannelInfo};
pub mod model;

// 顶层 re-export，方便外部 `use rclaw_core::*` 或单符号引用。
pub use bus::{
    BusMessage, MessageSource, ToolAccess, CHANNEL_CLI, CHANNEL_NAPCAT, CHANNEL_SYSTEM,
    CHANNEL_TELEGRAM, CHANNEL_WEBSOCKET, CHANNEL_WEIXIN, CHANNEL_WXBOT, CRON_TASK_PREFIX,
};
pub use model::{
    ChatEvent, ChatOutcome, ChatRequest, ContentBlock, FinishReason, MediaKind, Message, Role,
    TokenUsage, ToolCall, ToolSchema,
};

/// Crate 版本(关于页「版本信息」用)。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
