//! Provider 身份与协议方言配置。
//!
//! 这里只描述「是谁、走哪个协议、key、模型、方言微调」，不涉及具体 wire 逻辑。
//! 加上一个新 OpenAI 兼容 provider 时，往往只需新增一段 `ProviderConfig`，
//! 复用 `OpenAiCompat` 协议实现，零协议代码。

use secrecy::SecretString;

/// 判断模型是否支持多模态输入。
///
/// 这里集中维护已知多模态模型名单，协议层和运行时都通过此接口判断，
/// 避免各处散落模型名字符串。未知模型默认按纯文本处理。
pub fn model_supports_multimodal(model: &str) -> bool {
    // glm-5v-turbo：多模态位已由 glm-5.3-flash 接替（/llmodel 旧名兼容映射），但模型仍
    // 在线且 skills/ 仍在直连；存量 rclaw.toml 配置它时多模态判定须保持为真，媒体块才
    // 不会在协议层被静默过滤。
    matches!(
        model.trim().to_ascii_lowercase().as_str(),
        "glm-5.3-flash" | "glm-5v-turbo"
    )
}

// ---------------------------------------------------------------------------
// 鉴权方式
// ---------------------------------------------------------------------------

/// 如何把 API key 放进请求头。不同 provider 的鉴权头不同：
#[derive(Debug, Clone)]
pub enum AuthScheme {
    /// `Authorization: Bearer <key>`（DeepSeek、智谱兼容端点、OpenAI、Kimi…绝大多数）。
    Bearer,
    /// `<header>: <key>`（如 Anthropic 的 `x-api-key`）。
    ApiKey { header: String },
    // 可扩展为 JWT（智谱原生）、OAuth 等。
}

// ---------------------------------------------------------------------------
// 方言：OpenAI 兼容家族里那些「差不多但又不完全一样」的差异
// ---------------------------------------------------------------------------

/// 推理/思考内容字段名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningField {
    /// DeepSeek / 智谱 GLM：`reasoning_content`。
    ReasoningContent,
    /// 部分实现：`reasoning`。
    Reasoning,
    /// 不暴露思考内容。
    None,
}

/// 限制输出 token 的请求字段名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxTokensKey {
    /// 旧 OpenAI 兼容：`max_tokens`（DeepSeek 用这个）。
    MaxTokens,
    /// 新版 OpenAI：`max_completion_tokens`。
    MaxCompletionTokens,
}

/// 方言集合：吸收各家小差异，避免为每个 provider 写一个类。
#[derive(Debug, Clone)]
pub struct Dialect {
    pub reasoning_field: ReasoningField,
    pub supports_tool_choice: bool,
    pub max_tokens_key: MaxTokensKey,
}

impl Dialect {
    /// DeepSeek 方言。
    pub fn deepseek() -> Self {
        Dialect {
            reasoning_field: ReasoningField::ReasoningContent,
            supports_tool_choice: true,
            max_tokens_key: MaxTokensKey::MaxTokens,
        }
    }

    /// 通用 OpenAI 兼容方言。
    pub fn openai_compat() -> Self {
        Dialect {
            reasoning_field: ReasoningField::ReasoningContent,
            supports_tool_choice: true,
            max_tokens_key: MaxTokensKey::MaxTokens,
        }
    }
}

// ---------------------------------------------------------------------------
// 协议种类
// ---------------------------------------------------------------------------

/// 协议形态。决定用哪个 `ChatProtocol` 实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolKind {
    /// OpenAI 兼容 `/chat/completions`：覆盖 DeepSeek / 智谱兼容 / Kimi / OpenRouter …
    OpenAiCompat,
    // 可扩展为 Anthropic（Messages API）、ZhipuNative（JWT）等。
}

// ---------------------------------------------------------------------------
// Provider 配置
// ---------------------------------------------------------------------------

/// 一个 provider 的完整身份配置。
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// 名字，如 "deepseek"；也用于在配置里被 `llm.provider` 选中。
    pub name: String,
    /// base URL（**不含** `/chat/completions`）。例：`https://api.deepseek.com/v1`。
    pub base_url: String,
    pub auth: AuthScheme,
    /// API key（密文包装，防意外打印）。
    pub api_key: SecretString,
    /// 默认模型名。
    pub default_model: String,
    pub protocol: ProtocolKind,
    pub dialect: Dialect,
    /// 单轮最大输出 token。
    pub max_tokens: u32,
    /// 单次 LLM HTTP 请求的总超时（秒）。
    pub request_timeout_secs: u64,
    /// 单轮允许的最大工具调用数（默认 256）。
    ///
    /// 解析时超出即拒绝整轮，避免静默截断工具链。该值是【本地解析端安全阀】，
    /// **不**随请求发给模型；若模型一轮内并行调用超过此数（如批量导入多文档），
    /// 可在 `rclaw.toml` 的 `[llm] max_tool_calls` 调大。
    pub max_tool_calls: usize,
}

impl ProviderConfig {
    /// 构造一份 DeepSeek 默认配置（key 由调用方填入）。
    pub fn deepseek(api_key: SecretString) -> Self {
        ProviderConfig {
            name: "deepseek".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            auth: AuthScheme::Bearer,
            api_key,
            default_model: "deepseek-v4-pro".into(),
            protocol: ProtocolKind::OpenAiCompat,
            dialect: Dialect::deepseek(),
            max_tokens: 4096,
            request_timeout_secs: 300,
            max_tool_calls: 256,
        }
    }

    /// 当前 provider 选中的默认模型是否支持多模态输入。
    pub fn supports_multimodal(&self) -> bool {
        model_supports_multimodal(&self.default_model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_known_multimodal_models() {
        assert!(model_supports_multimodal("glm-5.3-flash"));
        assert!(model_supports_multimodal(" GLM-5.3-Flash "));
        // 旧多模态模型仍在线、skills 仍直连，存量配置须保持多模态判定为真。
        assert!(model_supports_multimodal("GLM-5V-Turbo"));
        assert!(!model_supports_multimodal("glm-5.3"));
        assert!(!model_supports_multimodal("deepseek-v4-pro"));
    }

    #[test]
    fn provider_reports_current_model_multimodal_capability() {
        let mut cfg = ProviderConfig::deepseek(SecretString::from(String::from("sk-test")));
        assert!(!cfg.supports_multimodal());
        assert_eq!(cfg.request_timeout_secs, 300);
        assert_eq!(cfg.max_tool_calls, 256);

        cfg.default_model = "glm-5.3-flash".into();
        assert!(cfg.supports_multimodal());
    }
}
