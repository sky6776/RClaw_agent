//! rclaw-cli 库目标：把「配置加载」与「运行时装配」暴露给其它二进制复用。
//!
//! - [`config`]：`rclaw.toml` 解析、provider 构建、通道配置派生。
//!   桌面版（rclaw-desktop）经 [`config::Config::load_from_dir`] 从用户数据目录
//!   加载配置，与 CWD 解耦。
//! - [`logging`]：tracing 初始化（控制台 + 按日轮转文件）。
//! - [`bootstrap`]：核心组件装配（provider / 工具注册表 / AgentOptions / cron /
//!   slash 注册表）。`main.rs` 与桌面版共用同一装配路径，避免两份代码漂移。
//! - [`sync`]：多端同步的装配（同步引擎接到运行时、技能目录、定时任务调度器上）、
//!   `[sync]` 段的写回、命令行的 `--sync-join`。

pub mod bootstrap;
pub mod config;
pub mod logging;
pub mod sync;

/// Crate 版本(关于页「版本信息」用)。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
