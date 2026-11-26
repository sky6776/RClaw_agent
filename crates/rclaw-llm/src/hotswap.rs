//! 热切换 Provider：运行期替换底层 provider 的 model / base_url，无需重启进程。
//!
//! 动机：`/llmodel` 这类斜杠命令需要在对话进行中切换模型。agent loop 持有
//! `Arc<dyn Provider>`，正常 provider 构造后不可变；这里用一个包装把它变成可替换的。
//!
//! 结构：`SwappableProvider` 同时持有
//! - 构造当前 provider 所用的 `ProviderConfig`（切换时基于它克隆、只改 model/base_url，
//!   其余 api_key / auth / dialect 等原样保留）；
//! - 当前活跃的底层 provider（`Arc<dyn Provider>`）。
//!
//! 二者作为一份整体状态放在 `std::sync::RwLock` 后面，自身实现 [`Provider`]。
//!
//! 切换语义（[`SwappableProvider::switch_model`]）：先在【读锁】下克隆配置、构造新 provider，
//! 构造成功才在【写锁】下原子替换配置与 provider；失败则什么都不改。
//! [`Provider::chat`] / [`chat_stream`](Provider::chat_stream) 调用前先克隆出底层 `Arc` 再释放读锁，
//! 故【正在进行的】一轮对话用旧 provider 跑完，切换从下一次调用生效——绝不会在一轮 SSE 流中途换模型。

use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use rclaw_core::{ChatOutcome, ChatRequest};

use crate::config::ProviderConfig;
use crate::error::{LlmError, Result};
use crate::provider::{build_provider, OnReasoning, OnText, Provider};

/// 把 `PoisonError` 统一翻成 `LlmError::Config`，避免在每个调用点重复写闭包。
fn lock_err() -> LlmError {
    LlmError::Config("provider 内部锁被 poisoned".into())
}

/// 可热切换的 Provider 包装。
///
/// 对外是一个普通 `Provider`，agent loop / 业务代码无感知；内部可在运行期整体替换。
pub struct SwappableProvider {
    /// provider 名（不随模型切换变化），用于实现 `Provider::name` 返回 `&str`。
    name: String,
    /// 当前 provider 配置与底层实现作为整体替换，避免切换瞬间读到新旧混合状态。
    state: RwLock<ProviderState>,
}

struct ProviderState {
    /// 当前 provider 配置（含 model / base_url / api_key / dialect…）。
    config: ProviderConfig,
    /// 当前活跃的底层 provider。
    inner: Arc<dyn Provider>,
}

impl SwappableProvider {
    /// 由初始配置构造：立即 build 出 provider 并装入。
    pub fn new(cfg: ProviderConfig) -> Result<Self> {
        let name = cfg.name.clone();
        let provider: Arc<dyn Provider> = Arc::from(build_provider(cfg.clone())?);
        Ok(SwappableProvider {
            name,
            state: RwLock::new(ProviderState {
                config: cfg,
                inner: provider,
            }),
        })
    }

    /// 热切换 model / base_url：保留 api_key / auth / dialect 等其余配置不变，
    /// 仅改这两个字段后重建 provider 并替换。下一次 `chat` 即用新模型。
    ///
    /// 构造失败时【什么都不改】（配置与 provider 都保持原样），调用方据 `Err` 反馈。
    pub fn switch_model(&self, model: String, base_url: String) -> Result<()> {
        let model = normalize_model(model)?;
        let base_url = normalize_base_url(base_url)?;
        // 读锁内：克隆配置、改两字段、构造新 provider。构造可能失败（如非法 base_url），
        // 失败则原配置/原 provider 均未被触碰。
        let (new_cfg, new_provider): (ProviderConfig, Arc<dyn Provider>) = {
            let state = self.state.read().map_err(|_| lock_err())?;
            let mut new_cfg = state.config.clone();
            new_cfg.default_model = model;
            new_cfg.base_url = base_url;
            let provider: Arc<dyn Provider> = Arc::from(build_provider(new_cfg.clone())?);
            (new_cfg, provider)
        };
        // 构造成功：写锁内原子提交配置 + provider。
        let mut state = self.state.write().map_err(|_| lock_err())?;
        state.config = new_cfg;
        state.inner = new_provider;
        Ok(())
    }

    /// 当前生效的 `(model, base_url)`，供 `/llmodel` 无参时回显状态。
    pub fn current_model(&self) -> Result<(String, String)> {
        let state = self.state.read().map_err(|_| lock_err())?;
        Ok((
            state.config.default_model.clone(),
            state.config.base_url.clone(),
        ))
    }

    /// 当前 live 配置的克隆（含 name / model / base_url / reasoning / media_wire）。
    ///
    /// 供思考强度选择器与 `/llmodel` 在"整组参数切换"时读取现状：跨 provider 切换后
    /// `config.name` 已是目标条目名，持久化不会写错段。
    pub fn current_config(&self) -> Result<ProviderConfig> {
        Ok(self.state.read().map_err(|_| lock_err())?.config.clone())
    }

    /// 整体替换 provider 配置（含 api_key / auth / dialect / flavor / media_wire /
    /// reasoning / max_tokens），用于跨 provider 或整组参数切换。
    ///
    /// 与 [`Self::switch_model`] 相同：先构造新 provider，成功后才在写锁内原子替换；
    /// 构造失败时配置与 provider 都保持原样。
    pub fn switch_provider(&self, cfg: ProviderConfig) -> Result<()> {
        let provider: Arc<dyn Provider> = Arc::from(build_provider(cfg.clone())?);
        self.switch_provider_built(cfg, provider);
        Ok(())
    }

    /// 用**预先构建好**的 provider 原子替换配置。
    ///
    /// 供「先保存 TOML、再发布 live」的事务使用：构造已在保存前完成，这一步只剩
    /// 写锁替换。锁中毒（持锁线程 panic）时也继续替换：state 是两个字段的整体赋值，
    /// 不存在半更新；这样「磁盘已改、发布失败」的窗口被彻底关掉。
    pub fn switch_provider_built(&self, cfg: ProviderConfig, inner: Arc<dyn Provider>) {
        let mut state = match self.state.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.config = cfg;
        state.inner = inner;
        // 在写守卫内清除本次观察到的中毒：读端被阻塞到替换完成，不会看到
        // 「已替换但读端仍报不可用」的间隙；持写锁期间也不可能有别的线程
        // 制造新的中毒，因此不会误清后续的真实中毒。
        if self.state.is_poisoned() {
            self.state.clear_poison();
        }
    }
}

fn normalize_model(model: String) -> Result<String> {
    let model = model.trim().to_string();
    if model.is_empty() {
        return Err(LlmError::Config("model 不能为空".into()));
    }
    Ok(model)
}

fn normalize_base_url(base_url: String) -> Result<String> {
    let base_url = base_url.trim().trim_end_matches('/').to_string();
    let parsed = reqwest::Url::parse(&base_url)
        .map_err(|e| LlmError::Config(format!("base_url 不是合法 URL: {e}: {base_url}")))?;
    if !(parsed.scheme() == "http" || parsed.scheme() == "https") || parsed.host_str().is_none() {
        return Err(LlmError::Config(format!(
            "base_url 必须是 http(s) URL: {base_url}"
        )));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(LlmError::Config(
            "base_url 不能包含 query 或 fragment".into(),
        ));
    }
    Ok(base_url)
}

#[async_trait]
impl Provider for SwappableProvider {
    fn name(&self) -> &str {
        // name 不随模型切换变化，直接返回存储的 &str（无需取锁）。
        &self.name
    }

    fn max_tokens(&self) -> u32 {
        self.state
            .read()
            .map(|s| s.config.max_tokens)
            .unwrap_or(4096)
    }

    fn supports_multimodal(&self) -> bool {
        // 多模态能力随 model 变化（如 glm-5.3-flash），故实时读配置。
        self.state
            .read()
            .map(|s| s.config.supports_multimodal())
            .unwrap_or(false)
    }

    fn current_identity(&self) -> Option<(String, String)> {
        self.state.read().ok().map(|state| {
            (
                state.config.name.clone(),
                state.config.default_model.clone(),
            )
        })
    }

    fn snapshot(&self) -> Option<Arc<dyn Provider>> {
        let config = self.state.read().ok()?.config.clone();
        build_provider(config).ok().map(Arc::from)
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatOutcome> {
        // 克隆出底层 Arc 后立即释放读锁：不持锁跨 await，正在进行的对话不受切换影响。
        let provider = self.state.read().map_err(|_| lock_err())?.inner.clone();
        provider.chat(req).await
    }

    async fn chat_stream(&self, req: ChatRequest, on_text: OnText) -> Result<ChatOutcome> {
        let provider = self.state.read().map_err(|_| lock_err())?.inner.clone();
        provider.chat_stream(req, on_text).await
    }

    async fn chat_stream_reasoning(
        &self,
        req: ChatRequest,
        on_text: OnText,
        on_reasoning: OnReasoning,
    ) -> Result<ChatOutcome> {
        let provider = self.state.read().map_err(|_| lock_err())?.inner.clone();
        provider
            .chat_stream_reasoning(req, on_text, on_reasoning)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AuthScheme, Dialect, MaxTokensKey, MediaWire, ProtocolKind, ProviderFlavor,
        ReasoningConfig, ReasoningField,
    };
    use secrecy::SecretString;

    #[test]
    fn switch_model_updates_model_and_base_url() {
        let provider = SwappableProvider::new(fixture()).unwrap();

        provider
            .switch_model(
                " glm-5.3-flash ".into(),
                "https://open.bigmodel.cn/api/coding/paas/v4/".into(),
            )
            .unwrap();

        let (model, base_url) = provider.current_model().unwrap();
        assert_eq!(model, "glm-5.3-flash");
        assert_eq!(base_url, "https://open.bigmodel.cn/api/coding/paas/v4");
        assert!(provider.supports_multimodal());
    }

    #[test]
    fn switch_model_rejects_invalid_values_without_changing_current() {
        let provider = SwappableProvider::new(fixture()).unwrap();
        let before = provider.current_model().unwrap();

        assert!(provider
            .switch_model("".into(), "https://open.bigmodel.cn/api/paas/v4".into())
            .is_err());
        assert_eq!(provider.current_model().unwrap(), before);

        assert!(provider
            .switch_model("glm-5.3-flash".into(), "not a url".into())
            .is_err());
        assert_eq!(provider.current_model().unwrap(), before);
    }

    fn fixture() -> ProviderConfig {
        ProviderConfig {
            name: "zhipu".into(),
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4".into(),
            auth: AuthScheme::Bearer,
            api_key: SecretString::from(String::from("test-api-key")),
            default_model: "glm-5.3".into(),
            protocol: ProtocolKind::OpenAiCompat,
            dialect: Dialect {
                reasoning_field: ReasoningField::ReasoningContent,
                supports_tool_choice: true,
                max_tokens_key: MaxTokensKey::MaxTokens,
                classify_upstream_busy: false,
            },
            flavor: ProviderFlavor::Zhipu,
            media_wire: MediaWire::OpenAiCompat,
            reasoning: ReasoningConfig::default(),
            max_tokens: 4096,
            request_timeout_secs: 300,
            max_tool_calls: 128,
        }
    }
}
