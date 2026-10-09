//! Logging initialization
//!
//! Configurable via env vars or config.json:
//! - `GBRAIN_RUST_LOG` - full tracing filter rules, overrides GBRAIN_LOG_LEVEL
//! - `GBRAIN_LOG_LEVEL` — trace|debug|info|warn|error (default: info)
//! - `GBRAIN_LOG_TO_FILE` — true|false (default: true)
//! - `GBRAIN_LOG_FILE_PATH` — custom log file path (default: $GBRAIN_DIR/logs/gbrain.log)
//! - `GBRAIN_LOG_TO_CONSOLE` — true|false (default: true)

use crate::config::Config;
use crate::error::{GBrainError, Result};
use std::path::PathBuf;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Initialize the logging system based on config.
///
/// Call this once at program startup (CLI or MCP server).
/// If called multiple times, subsequent calls are no-ops (tracing guards this).
pub fn init(config: &Config) -> Result<()> {
    if tracing::dispatcher::has_been_set() {
        return Ok(());
    }

    // Build env filter: GBRAIN_RUST_LOG takes priority, then config.log_level.
    let gbrain_rust_log = std::env::var("GBRAIN_RUST_LOG").ok();
    let filter = build_env_filter(&config.log_level, gbrain_rust_log.as_deref())?;

    if config.log_to_file {
        let log_path = config
            .log_file_path
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| Config::base_dir().join("logs").join("gbrain.log"));

        // Ensure parent directory exists
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let file_name = log_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "gbrain.log".to_string());
        let log_dir = log_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| Config::base_dir().join("logs"));

        let file_appender = tracing_appender::rolling::daily(&log_dir, &file_name);
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(file_appender)
            .with_ansi(false);

        if config.log_to_console {
            // 显式输出到 stderr，避免与 MCP stdio 的 stdout JSON-RPC 协议交错
            let console_layer = tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(false);
            tracing_subscriber::registry()
                .with(filter)
                .with(file_layer)
                .with(console_layer)
                .try_init()
                .ok();
        } else {
            tracing_subscriber::registry()
                .with(filter)
                .with(file_layer)
                .try_init()
                .ok();
        }
    } else if config.log_to_console {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(filter)
            .with_target(false)
            .try_init()
            .ok();
    }
    // If both file and console are disabled, no logging output (silent mode)
    Ok(())
}

fn build_env_filter(config_log_level: &str, gbrain_rust_log: Option<&str>) -> Result<EnvFilter> {
    if let Some(value) = gbrain_rust_log {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(GBrainError::Config(
                "GBRAIN_RUST_LOG 已配置但值为空".to_string(),
            ));
        }
        return EnvFilter::try_new(trimmed).map_err(|e| {
            GBrainError::Config(format!("GBRAIN_RUST_LOG 无效值 '{}': {}", trimmed, e))
        });
    }

    let trimmed = config_log_level.trim();
    if trimmed.is_empty() {
        return Err(GBrainError::Config(
            "GBRAIN_LOG_LEVEL 已配置但值为空".to_string(),
        ));
    }
    if !matches!(trimmed, "trace" | "debug" | "info" | "warn" | "error") {
        return Err(GBrainError::Config(format!(
            "GBRAIN_LOG_LEVEL 无效值: {}，有效值: trace/debug/info/warn/error",
            trimmed
        )));
    }
    EnvFilter::try_new(trimmed)
        .map_err(|e| GBrainError::Config(format!("GBRAIN_LOG_LEVEL 无效值 '{}': {}", trimmed, e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_env_filter_uses_config_log_level_when_gbrain_rust_log_is_absent() {
        assert!(build_env_filter("info", None).is_ok());
    }

    #[test]
    fn build_env_filter_accepts_gbrain_rust_log_module_rules() {
        assert!(build_env_filter(
            "warn",
            Some("info,gbrain_core::kb::pipeline=debug,reqwest=warn"),
        )
        .is_ok());
    }

    #[test]
    fn build_env_filter_gbrain_rust_log_overrides_invalid_config_log_level() {
        assert!(build_env_filter("not-a-level", Some("gbrain_core=debug")).is_ok());
    }

    #[test]
    fn build_env_filter_rejects_invalid_config_log_level_when_gbrain_rust_log_is_absent() {
        let err = build_env_filter("not-a-level", None).unwrap_err();
        assert!(err.to_string().contains("GBRAIN_LOG_LEVEL"));
    }

    #[test]
    fn build_env_filter_rejects_empty_gbrain_rust_log() {
        let err = build_env_filter("info", Some("  ")).unwrap_err();
        assert!(err.to_string().contains("GBRAIN_RUST_LOG"));
    }

    #[test]
    fn build_env_filter_rejects_invalid_gbrain_rust_log() {
        let err = build_env_filter("info", Some("gbrain_core::kb::pipeline=verbose")).unwrap_err();
        assert!(err.to_string().contains("GBRAIN_RUST_LOG"));
    }
}
