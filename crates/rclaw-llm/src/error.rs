//! llm 层错误类型。

use thiserror::Error;

/// Harness 可据此决定是否原样重试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmErrorClass {
    /// 网络抖动、限流、服务端过载等，保持请求不变重试可能成功。
    Retryable,
    /// 鉴权、配置、参数等确定性错误，原样重试没有意义。
    NonRetryable,
    /// Provider 返回了不完整或不合法协议数据；应中止当前路径并保留诊断。
    Protocol,
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP 请求失败: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON 处理失败: {0}")]
    Json(#[from] serde_json::Error),

    /// 服务端返回非 2xx，附状态码与（截断后的）响应体，便于排查。
    #[error("LLM API 返回状态 {status}: {body}")]
    BadStatus { status: u16, body: String },

    /// 响应格式损坏、缺关键字段、工具调用不完整等「拒绝整轮」的情形。
    /// 对应 parse 阶段的各种红线（格式损坏 / 工具链不完整等）。
    #[error("响应不可用: {0}")]
    BadResponse(String),

    /// 流式被异常终止（无 [DONE]/finish_reason 的 EOF、流内 error 等）。
    #[error("流式异常终止: {0}")]
    StreamAborted(String),

    #[error("缺少 API key")]
    MissingApiKey,

    #[error("缺少 API URL/endpoint")]
    MissingEndpoint,

    /// Provider 配置错误：如自定义鉴权头（`auth = apikey:...`）的 header 名/值非法。
    ///
    /// 用于让配置阶段【直接失败并指出是哪个 header 非法】，而不是像旧实现那样在
    /// `build_header_map` 里静默丢弃非法头——后者会让请求以【无鉴权】发出，
    /// 只在远端返回 401 时才暴露，排查困难。
    #[error("Provider 配置错误: {0}")]
    Config(String),
}

impl LlmError {
    pub fn class(&self) -> LlmErrorClass {
        match self {
            LlmError::Http(error) => {
                if error.is_timeout() || error.is_connect() || error.is_body() {
                    LlmErrorClass::Retryable
                } else {
                    LlmErrorClass::NonRetryable
                }
            }
            LlmError::BadStatus { status, .. } => {
                if matches!(*status, 408 | 425 | 429) || (500..=599).contains(status) {
                    LlmErrorClass::Retryable
                } else {
                    LlmErrorClass::NonRetryable
                }
            }
            LlmError::StreamAborted(message) => {
                if stream_error_is_deterministic(message) {
                    LlmErrorClass::NonRetryable
                } else {
                    LlmErrorClass::Retryable
                }
            }
            LlmError::BadResponse(_) | LlmError::Json(_) => LlmErrorClass::Protocol,
            LlmError::MissingApiKey | LlmError::MissingEndpoint | LlmError::Config(_) => {
                LlmErrorClass::NonRetryable
            }
        }
    }

    pub fn is_retryable(&self) -> bool {
        self.class() == LlmErrorClass::Retryable
    }
}

fn stream_error_is_deterministic(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "invalid api key",
        "invalid x-api-key",
        "unauthorized",
        "authentication",
        "forbidden",
        "permission denied",
        "invalid request",
        "invalid_request",
        "context length",
        "maximum context",
        "unsupported",
        "content policy",
        "insufficient quota",
        "insufficient_quota",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

pub type Result<T> = std::result::Result<T, LlmError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_errors_are_classified_for_retry() {
        assert!(LlmError::BadStatus {
            status: 429,
            body: String::new()
        }
        .is_retryable());
        assert!(LlmError::BadStatus {
            status: 503,
            body: String::new()
        }
        .is_retryable());
        assert!(!LlmError::BadStatus {
            status: 401,
            body: String::new()
        }
        .is_retryable());
        assert!(!LlmError::BadStatus {
            status: 409,
            body: String::new()
        }
        .is_retryable());
    }

    #[test]
    fn deterministic_and_protocol_errors_are_not_retried() {
        assert_eq!(LlmError::MissingApiKey.class(), LlmErrorClass::NonRetryable);
        assert_eq!(
            LlmError::BadResponse("broken".into()).class(),
            LlmErrorClass::Protocol
        );
        assert_eq!(
            LlmError::StreamAborted("invalid api key".into()).class(),
            LlmErrorClass::NonRetryable
        );
        assert_eq!(
            LlmError::StreamAborted("maximum context length exceeded".into()).class(),
            LlmErrorClass::NonRetryable
        );
        assert_eq!(
            LlmError::StreamAborted("SSE EOF 未收到终止事件".into()).class(),
            LlmErrorClass::Retryable
        );
    }
}
