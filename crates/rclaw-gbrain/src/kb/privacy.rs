//! 隐私工具模块 — 保留脱敏函数和外部模型调用审计日志
//!
//! 库级隐私开关（external_*_allowed / redaction_enabled）已移除，
//! 外部服务始终允许，脱敏始终关闭。

use regex::Regex;
use std::sync::OnceLock;

/// P5-020: 对发送给外部模型的文本进行脱敏。
/// 注意：当前脱敏功能已关闭（外部调用始终允许），此函数保留以备将来启用。
pub fn redact_content(text: &str) -> String {
    let mut result = text.to_string();

    // 邮箱
    result = email_regex().replace_all(&result, "[EMAIL]").to_string();

    // 手机号（中国大陆）
    result = phone_regex().replace_all(&result, "[PHONE]").to_string();

    // 身份证号
    result = id_card_regex()
        .replace_all(&result, "[ID_NUMBER]")
        .to_string();

    // API key 模式（sk-...、pk-... 等）
    result = api_key_regex()
        .replace_all(&result, "[API_KEY]")
        .to_string();

    // 银行卡号（16-19 位数字）
    result = bank_card_regex()
        .replace_all(&result, "[BANK_CARD]")
        .to_string();

    result
}

/// 编译静态邮箱正则表达式。
/// 正则字面量已知有效，编译不会失败，直接返回 `&'static Regex`。
fn email_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}")
            .expect("邮箱正则字面量已知有效，编译不应失败")
    })
}

/// 编译静态手机号正则表达式（中国大陆）。
fn phone_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"1[3-9]\d{9}").expect("手机号正则字面量已知有效，编译不应失败"))
}

/// 编译静态身份证号正则表达式。
fn id_card_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\d{17}[\dXx]").expect("身份证号正则字面量已知有效，编译不应失败")
    })
}

/// 编译静态 API key 正则表达式（sk-...、pk-... 等常见格式）。
fn api_key_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(sk-[a-zA-Z0-9]{20,}|pk-[a-zA-Z0-9]{20,}|[a-zA-Z0-9]{32,})\b")
            .expect("API key 正则字面量已知有效，编译不应失败")
    })
}

/// 编译静态银行卡号正则表达式（16-19 位数字）。
fn bank_card_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b\d{16,19}\b").expect("银行卡号正则字面量已知有效，编译不应失败")
    })
}

/// 记录外部模型调用（写入 kb_external_model_calls）
#[allow(clippy::too_many_arguments)]
pub fn log_external_model_call(
    conn: &rusqlite::Connection,
    library_id: Option<i64>,
    document_id: Option<i64>,
    call_type: &str,
    provider: &str,
    model: &str,
    input_tokens: i32,
    output_tokens: i32,
    latency_ms: i32,
    cost_estimate: f64,
    success: bool,
    error_message: &str,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO kb_external_model_calls \
         (library_id, document_id, call_type, provider, model, \
          input_tokens, output_tokens, latency_ms, cost_estimate, success, error_message) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            library_id,
            document_id,
            call_type,
            provider,
            model,
            input_tokens,
            output_tokens,
            latency_ms,
            cost_estimate,
            success as i32,
            error_message,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redact_email() {
        let text = "Contact alice@example.com for support";
        let redacted = redact_content(text);
        assert!(!redacted.contains("alice@example.com"));
        assert!(redacted.contains("[EMAIL]"));
    }

    #[test]
    fn test_redact_phone() {
        let text = "Call 13800138000";
        let redacted = redact_content(text);
        assert!(!redacted.contains("13800138000"));
        assert!(redacted.contains("[PHONE]"));
    }

    #[test]
    fn test_redact_no_sensitive() {
        let text = "This is normal text without PII";
        let redacted = redact_content(text);
        assert_eq!(text, redacted);
    }

    #[test]
    fn test_redact_api_key() {
        let text = "Authorization: Bearer sk-1234567890abcdefghijklmnop";
        let redacted = redact_content(text);
        assert!(redacted.contains("[API_KEY]"));
    }
}
