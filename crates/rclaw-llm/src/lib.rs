//! rclaw-llm：LLM Provider 抽象层（RClaw 可扩展性的核心）。
//!
//! 两层抽象：
//! - 协议层 [`ChatProtocol`]：wire format 的构造与解析（当前实现 [`OpenAiCompat`]）。
//! - 身份层 [`ProviderConfig`] / [`Dialect`]：是谁、key、模型、方言。
//!
//! 高层入口 [`Provider`]（由 [`ProtocolProvider`] 通用实现）屏蔽所有 wire 细节，
//! agent loop 只调用 `chat` / `chat_stream`。
//!
//! DeepSeek、智谱、Kimi 等 provider 均可通过配置接入。

pub mod config;
pub mod error;
pub mod hotswap;
pub mod openai_compat;
pub mod protocol;
pub mod provider;

// 顶层 re-export。
pub use config::{
    model_supports_multimodal, AuthScheme, Dialect, MaxTokensKey, ProtocolKind, ProviderConfig,
    ReasoningField,
};
pub use error::{LlmError, LlmErrorClass, Result};
pub use hotswap::SwappableProvider;
pub use openai_compat::OpenAiCompat;
pub use protocol::{ChatProtocol, StreamDelta, StreamParser};
pub use provider::{build_provider, OnReasoning, OnText, ProtocolProvider, Provider};

/// Crate 版本(关于页「版本信息」用)。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
