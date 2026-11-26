//! Provider 身份与协议方言配置。
//!
//! 这里只描述「是谁、走哪个协议、key、模型、方言微调」，不涉及具体 wire 逻辑。
//! 加上一个新 OpenAI 兼容 provider 时，往往只需新增一段 `ProviderConfig`，
//! 复用 `OpenAiCompat` 协议实现，零协议代码。

use secrecy::{ExposeSecret, SecretString};

use crate::error::LlmError;

/// 判断模型是否支持多模态输入。
///
/// 这里集中维护已知多模态模型名单，协议层和运行时都通过此接口判断，
/// 避免各处散落模型名字符串。未知模型默认按纯文本处理。
pub fn model_supports_multimodal(model: &str) -> bool {
    // glm-5v-turbo：多模态位已由 glm-5.3-flash 接替（/llmodel 旧名兼容映射），但模型仍
    // 在线且 skills/ 仍在直连；存量 rclaw.toml 配置它时多模态判定须保持为真，媒体块才
    // 不会在协议层被静默过滤。
    // glm-5.4：当前旗舰，原生多模态（视频/图像/文本/文件）。
    // deepseek-flash（DeepSeek-V4.1-Flash-1017）：支持图像输入；deepseek-v4-pro 是纯
    // 文本模型，绝不能加入白名单（否则会把图片发给不支持的端点导致 400）。
    matches!(
        model.trim().to_ascii_lowercase().as_str(),
        "glm-5.3-flash"
            | "glm-5.4"
            | "glm-5v-turbo"
            | "deepseek-flash"
            | "deepseek-v4-flash"
            | "deepseek-v4-flash-vision-exp"
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
    /// 是否把 DeepSeek 的 `insufficient_system_resource` / `aborted` 结束原因归类为
    /// 可重试的上游繁忙。仅 DeepSeek 方言开启，避免给其它兼容后端重新分类同名结束原因。
    pub classify_upstream_busy: bool,
}

impl Dialect {
    /// DeepSeek 方言。
    pub fn deepseek() -> Self {
        Dialect {
            reasoning_field: ReasoningField::ReasoningContent,
            supports_tool_choice: true,
            max_tokens_key: MaxTokensKey::MaxTokens,
            classify_upstream_busy: true,
        }
    }

    /// 通用 OpenAI 兼容方言。
    pub fn openai_compat() -> Self {
        Dialect {
            reasoning_field: ReasoningField::ReasoningContent,
            supports_tool_choice: true,
            max_tokens_key: MaxTokensKey::MaxTokens,
            classify_upstream_busy: false,
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
// Provider 能力族 / 媒体方言 / 思考参数
// ---------------------------------------------------------------------------

/// 媒体 content part 的 wire 方言。
///
/// 智谱与 OpenAI 兼容端点接受 `image_url` / `video_url` / `file_url`；DeepSeek 只接受
/// `image_url` 与 `file`（`file_id` / `file_data`），且没有 video part。协议层必须按
/// 方言映射，不能把智谱的 part 直接透传给 DeepSeek。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaWire {
    /// OpenAI / 智谱兼容：`image_url` / `video_url` / `file_url`。
    OpenAiCompat,
    /// DeepSeek：`image_url` + `file`（无 video）。
    DeepSeek,
}

/// Provider 能力族：按 base_url host 判定，用于方言级分支（媒体 / 思考校验）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFlavor {
    Zhipu,
    DeepSeek,
    Generic,
}

impl ProviderFlavor {
    /// 按 base_url host 判定（只看官方域名；代理/自定义端点必须显式配置 flavor）。
    pub fn detect(base_url: &str) -> Self {
        let host = reqwest::Url::parse(base_url)
            .ok()
            .and_then(|url| url.host_str().map(|host| host.to_ascii_lowercase()));
        match host.as_deref() {
            Some(host) if host == "api.deepseek.com" || host.ends_with(".deepseek.com") => {
                Self::DeepSeek
            }
            Some(host) if host == "bigmodel.cn" || host.ends_with(".bigmodel.cn") => Self::Zhipu,
            _ => Self::Generic,
        }
    }

    /// 解析配置里的显式能力族（`zhipu` / `deepseek` / `generic`）。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "zhipu" => Some(Self::Zhipu),
            "deepseek" => Some(Self::DeepSeek),
            "generic" => Some(Self::Generic),
            _ => None,
        }
    }

    /// 默认媒体方言（可被 `ProviderEntry.media_wire` 覆盖）。
    pub fn media_wire(self) -> MediaWire {
        match self {
            Self::DeepSeek => MediaWire::DeepSeek,
            _ => MediaWire::OpenAiCompat,
        }
    }
}

/// provider 级显式思考参数。
///
/// `None` = 未显式配置：保持既有行为（DeepSeek 走服务端默认；智谱 glm-5.3-flash
/// 仍注入官方推荐默认）。`thinking = Some(false)` 表示关闭思考（仅 DeepSeek 允许）。
#[derive(Debug, Clone, Default)]
pub struct ReasoningConfig {
    pub thinking: Option<bool>,
    pub effort: Option<String>,
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
    /// 能力族：媒体映射与思考参数校验按它分支。
    pub flavor: ProviderFlavor,
    /// 媒体 part 方言（默认由 flavor 推导，可被配置覆盖）。
    pub media_wire: MediaWire,
    /// 显式思考参数（缺省 = 服务端默认 / 既有推荐值）。
    pub reasoning: ReasoningConfig,
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

/// 构造前的最终配置预检：保存/热切换前就拒绝「启动时也会拒绝」的配置。
///
/// 校验对象必须是**最终**生效值（profile 覆盖之后），否则会出现「保存成功、
/// 首个请求或下次重启才报错」的半套状态。错误文案只带字段名与模型名，不回显 key。
pub fn validate_provider_config(cfg: &ProviderConfig) -> Result<(), LlmError> {
    if cfg.request_timeout_secs == 0 {
        return Err(LlmError::Config(
            "request_timeout_secs 必须大于 0".to_string(),
        ));
    }
    if cfg.max_tokens == 0 {
        return Err(LlmError::Config("max_tokens 必须大于 0".to_string()));
    }
    if cfg.max_tool_calls == 0 {
        return Err(LlmError::Config("max_tool_calls 必须大于 0".to_string()));
    }
    if cfg.default_model.trim().is_empty() {
        return Err(LlmError::Config("model 为空".to_string()));
    }
    if cfg.api_key.expose_secret().trim().is_empty() {
        return Err(LlmError::Config(format!(
            "provider '{}' 的 api_key 为空",
            cfg.name
        )));
    }
    // 完整鉴权头预检：非法 header 名/值（非可见 ASCII 等）在保存前就拒绝，
    // 不能等第一个请求才报错。错误文案不回显 key。
    match &cfg.auth {
        AuthScheme::Bearer => {
            let value = format!("Bearer {}", cfg.api_key.expose_secret());
            reqwest::header::HeaderValue::from_str(&value).map_err(|_| {
                LlmError::Config(format!(
                    "provider '{}' 的 api_key 含非法 header 字节",
                    cfg.name
                ))
            })?;
        }
        AuthScheme::ApiKey { header } => {
            reqwest::header::HeaderName::from_bytes(header.as_bytes()).map_err(|_| {
                LlmError::Config(format!(
                    "provider '{}' 的鉴权 header 名非法（只允许 HTTP token 字符）",
                    cfg.name
                ))
            })?;
            reqwest::header::HeaderValue::from_str(cfg.api_key.expose_secret()).map_err(|_| {
                LlmError::Config(format!(
                    "provider '{}' 的 api_key 含非法 header 字节",
                    cfg.name
                ))
            })?;
        }
    }
    let url = reqwest::Url::parse(&cfg.base_url).map_err(|error| {
        LlmError::Config(format!(
            "provider '{}' 的 base_url 无法解析为 URL: {error}",
            cfg.name
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(LlmError::Config(format!(
            "provider '{}' 的 base_url 协议必须为 http/https",
            cfg.name
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(LlmError::Config(format!(
            "provider '{}' 的 base_url 不允许包含用户名或密码",
            cfg.name
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(LlmError::Config(format!(
            "provider '{}' 的 base_url 不允许包含 query 或 fragment",
            cfg.name
        )));
    }

    let thinking = cfg.reasoning.thinking;
    let effort = cfg
        .reasoning
        .effort
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if thinking == Some(false) && effort.is_some() {
        return Err(LlmError::Config(format!(
            "provider '{}' 关闭思考时不能同时设置 reasoning_effort",
            cfg.name
        )));
    }
    let model = cfg.default_model.trim().to_ascii_lowercase();
    if thinking == Some(false) && cfg.flavor == ProviderFlavor::Zhipu && model == "glm-5.3-flash" {
        return Err(LlmError::Config(format!(
            "provider '{}' 的 glm-5.3-flash 官方强制开启思考，不能关闭",
            cfg.name
        )));
    }
    if let Some(effort) = effort {
        let allowed = match cfg.flavor {
            ProviderFlavor::DeepSeek => matches!(effort, "none" | "low" | "high" | "max"),
            ProviderFlavor::Zhipu if model == "glm-5.3-flash" || model == "glm-5.4" => {
                matches!(effort, "low" | "high" | "max")
            }
            _ => true,
        };
        if !allowed {
            return Err(LlmError::Config(format!(
                "provider '{}' 的 reasoning_effort '{}' 超出模型 '{}' 的支持范围",
                cfg.name, effort, cfg.default_model
            )));
        }
    }
    Ok(())
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
            flavor: ProviderFlavor::DeepSeek,
            media_wire: MediaWire::DeepSeek,
            reasoning: ReasoningConfig::default(),
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
        // 5.4 是当前原生多模态旗舰。
        assert!(model_supports_multimodal("glm-5.4"));
        // DeepSeek flash 系支持图像输入（含仍被接受的旧别名）。
        assert!(model_supports_multimodal("deepseek-flash"));
        assert!(model_supports_multimodal(" DEEPSEEK-FLASH "));
        assert!(model_supports_multimodal("deepseek-v4-flash"));
        // 纯文本模型绝不能误判为多模态。
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

    #[test]
    fn validate_rejects_final_config_that_would_fail_at_first_request() {
        let base = || ProviderConfig::deepseek(SecretString::from(String::from("sk-test")));

        // 非法头字符（非可见 ASCII）在构造期就该被拒。
        let mut cfg = base();
        cfg.api_key = SecretString::from(String::from("bad\u{7f}key"));
        assert!(validate_provider_config(&cfg).is_err());

        // 空 key / 非法 endpoint / 关闭思考同时带档位 / 档位越界。
        let mut cfg = base();
        cfg.api_key = SecretString::from(String::new());
        assert!(validate_provider_config(&cfg).is_err());

        let mut cfg = base();
        cfg.base_url = "ftp://api.deepseek.com/v1".into();
        assert!(validate_provider_config(&cfg).is_err());

        let mut cfg = base();
        cfg.reasoning.thinking = Some(false);
        cfg.reasoning.effort = Some("high".into());
        assert!(validate_provider_config(&cfg).is_err());

        let mut cfg = base();
        cfg.default_model = "deepseek-flash".into();
        cfg.reasoning.effort = Some("medium".into());
        assert!(validate_provider_config(&cfg).is_err());

        // 合法组合：DeepSeek 可关闭思考；GLM-5.3-Flash 只收 low/high/max。
        let mut cfg = base();
        cfg.reasoning.thinking = Some(false);
        assert!(validate_provider_config(&cfg).is_ok());

        let mut cfg = base();
        cfg.flavor = ProviderFlavor::Zhipu;
        cfg.base_url = "https://open.bigmodel.cn/api/coding/paas/v4".into();
        cfg.default_model = "glm-5.3-flash".into();
        cfg.reasoning.thinking = Some(true);
        cfg.reasoning.effort = Some("max".into());
        assert!(validate_provider_config(&cfg).is_ok());
        cfg.reasoning.thinking = Some(false);
        assert!(validate_provider_config(&cfg).is_err());
    }
}
