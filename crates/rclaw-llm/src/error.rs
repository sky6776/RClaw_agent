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

    /// 传输层失败（连接/读取/超时）：只保留受控类别，**不带 reqwest 的完整 URL**。
    #[error("网络请求失败（{kind}）")]
    Transport { kind: &'static str, retryable: bool },

    #[error("JSON 处理失败: {0}")]
    Json(#[from] serde_json::Error),

    /// 服务端返回非 2xx，附状态码与受控错误码/分类（正文不进入错误文案）。
    #[error("LLM API 返回状态 {status}: {body}")]
    BadStatus { status: u16, body: String },

    /// 响应格式损坏、缺关键字段、工具调用不完整等「拒绝整轮」的情形。
    /// 对应 parse 阶段的各种红线（格式损坏 / 工具链不完整等）。
    #[error("响应不可用: {0}")]
    BadResponse(String),

    /// 流式被异常终止（无 [DONE]/finish_reason 的 EOF、流内 error 等）。
    #[error("流式异常终止: {0}")]
    StreamAborted(String),

    /// 上游资源不足/主动中止（如 DeepSeek 的 `insufficient_system_resource`、
    /// `aborted` 结束原因）。请求本身没有问题，保持原样重试可能成功；与协议错误
    /// （`BadResponse`）区分开，避免把一次服务端抖动判成整轮失败。
    #[error("上游暂不可用: {0}")]
    UpstreamBusy(String),

    #[error("缺少 API key")]
    MissingApiKey,

    #[error("缺少 API URL/endpoint")]
    MissingEndpoint,

    /// Provider 配置错误：如自定义鉴权头（`auth = apikey:...`）的 header 名/值非法。
    ///
    /// 用于让配置阶段【直接失败】而不是像旧实现那样在 `build_header_map` 里静默
    /// 丢弃非法头——后者会让请求以【无鉴权】发出，只在远端返回 401 时才暴露。
    /// 文案只说明字段与格式，不回显 header 名/值（可能被误填凭据）。
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
            LlmError::Transport { retryable, .. } => {
                if *retryable {
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
            LlmError::UpstreamBusy(_) => LlmErrorClass::Retryable,
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
    // 结构化错误码常带下划线（invalid_api_key）：归一成空格后再匹配文本表。
    let message = message.to_ascii_lowercase().replace('_', " ");
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

/// 上游错误码/类型的本地白名单：未知值一律不透出（避免把任意远端文本当诊断展示）。
pub(crate) fn safe_error_code(value: &str) -> Option<String> {
    const KNOWN: &[&str] = &[
        "invalid_api_key",
        "invalid_key",
        "invalid_request_error",
        "invalid_request",
        "authentication_error",
        "permission_error",
        "insufficient_quota",
        "rate_limit_exceeded",
        "context_length_exceeded",
        "model_not_found",
        "content_filter",
        "server_error",
        "overloaded_error",
        "billing_not_active",
        "account_deactivated",
    ];
    let lower = value.trim().to_ascii_lowercase();
    if KNOWN.contains(&lower.as_str()) {
        return Some(lower);
    }
    // 数字码也可能是上游回显的 PIN/验证码：不保留，未知值统一省略。
    None
}

/// 上游瞬时错误码：SSE 中此类结构化码优先于 message 的自由文本分类。
pub(crate) fn error_code_is_transient(code: &str) -> bool {
    matches!(
        code,
        "rate_limit_exceeded"
            | "server_error"
            | "overloaded_error"
            | "service_unavailable"
            | "timeout"
    )
}

/// 从已知错误码映射分类标记（语言无关；message 只有自由文本时才依赖文本扫描）。
pub(crate) fn error_code_tags(code: &str) -> Vec<&'static str> {
    match code {
        "invalid_api_key"
        | "invalid_key"
        | "authentication_error"
        | "permission_error"
        | "account_deactivated"
        | "billing_not_active" => vec!["authentication_error"],
        "context_length_exceeded" => vec!["context_length_exceeded"],
        _ => Vec::new(),
    }
}

/// 从上游错误正文/流内 message 提取有限分类标记（只在边界内部使用，正文不外泄）。
pub(crate) fn upstream_error_tags(text: &str) -> Vec<&'static str> {
    let lower = text.to_ascii_lowercase().replace('_', " ");
    let mut tags = Vec::new();
    let api_key_bad = lower.contains("api key")
        && (lower.contains("invalid")
            || lower.contains("blocked")
            || lower.contains("expired")
            || lower.contains("revoked"));
    if api_key_bad
        || [
            "invalid_api_key",
            "invalid api key",
            "invalid x-api-key",
            "unauthorized",
            "authentication",
            "permission denied",
            "permission error",
            "forbidden",
            "auth failure",
            "鉴权失败",
            "认证失败",
            "未授权",
            "无效的api key",
            "api key无效",
            "密钥无效",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        tags.push("authentication_error");
    }
    // 与下游分类词表保持超集：payload too large / 413 正文同样按超限处理。
    if [
        "context length",
        "maximum context",
        "context window",
        "prompt is too long",
        "too many tokens",
        "input length exceeds",
        "context length exceeded",
        "request too large",
        "payload too large",
        "上下文长度",
        "上下文窗口",
        "超过模型token上限",
        "token上限",
        "请求的tokens超过",
        "输入过长",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        tags.push("context_length_exceeded");
    }
    // SSE/压缩分类器依赖的确定性错误短语：统一产出标签，避免摘要丢信号。
    for (needle, tag) in [
        ("invalid request", "invalid_request"),
        ("unsupported", "unsupported"),
        ("content policy", "content_policy"),
        ("insufficient quota", "insufficient_quota"),
    ] {
        if lower.contains(needle) {
            tags.push(tag);
        }
    }
    tags
}

/// 把 reqwest 错误收敛为不含 URL 的受控类别（端点路径可能带代理路由凭据）。
pub fn transport_error(error: &reqwest::Error) -> LlmError {
    // 连接建立失败可能同时满足 is_timeout（底层 TCP TimedOut）：连接阶段优先，
    // 这样压缩摘要仍按「建连失败」保留原历史；纯读取超时才是 timeout。
    let (kind, retryable) = if error.is_connect() {
        ("connect", true)
    } else if error.is_timeout() {
        ("timeout", true)
    } else if error.is_body() {
        ("body", true)
    } else if error.is_decode() {
        ("decode", false)
    } else if error.is_request() {
        ("request", false)
    } else if error.is_redirect() {
        ("redirect", false)
    } else {
        ("other", false)
    };
    LlmError::Transport { kind, retryable }
}

pub type Result<T> = std::result::Result<T, LlmError>;

/// 从任意文本中移除常见凭据形态（`sk-...`、`Bearer ...`），供错误体/回显使用。
///
/// 只做保守的模式替换：即使 key 被截断或被包在 JSON 字符串里，也会把可见的
/// 前缀和后续 token 一起替换掉；无法识别的自定义格式仍需调用方避免输出原文。
pub fn redact_secrets(text: &str) -> String {
    fn redact_token(rest: &str) -> (&str, &str) {
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
            .unwrap_or(rest.len());
        (&rest[end..], &rest[..end])
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let sk = rest.find("sk-");
        let bearer = rest.find("Bearer ").or_else(|| rest.find("bearer "));
        let next = match (sk, bearer) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        let Some(index) = next else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..index]);
        let after = &rest[index..];
        if let Some(token) = after.strip_prefix("sk-") {
            let (tail, _) = redact_token(token);
            out.push_str("<redacted>");
            rest = tail;
        } else {
            // "Bearer " / "bearer " 前缀：保留前缀，替换后面的 token。
            let prefix_len = 7;
            out.push_str(&after[..prefix_len]);
            let (tail, _) = redact_token(&after[prefix_len..]);
            out.push_str("<redacted>");
            rest = tail;
        }
    }
    out
}

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
        assert_eq!(
            LlmError::UpstreamBusy("finish_reason=insufficient_system_resource".into()).class(),
            LlmErrorClass::Retryable
        );
    }
}
