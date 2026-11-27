//! 斜杠命令模块。
//!
//! 在 agent loop 处理每条【用户】消息的最开头拦截：若正文以 `/` 开头则派发给对应命令，
//! 命中即返回回复文本并跳过 LLM；未命中（非斜杠）则原样放行进入正常对话。
//!
//! 设计目标：
//! - **通道无关**：CLI、微信等任意通道发的 `/cmd` 都会被处理，回复按原 `channel`/`chat_id`
//!   经 outbound 回送，agent_loop 中已据此提前 return（不走 LLM、不污染会话历史）。
//! - **可扩展**：不依赖 turn 状态的命令在 [`SlashRegistry::try_handle`] 增加分支；需要
//!   workspace、session 或活动 turn 状态的命令（如 `/skills`、`/tools`、`/context`）由
//!   turn 的命令分发器在进入 LLM 前处理。
//!
//! 当前命令：
//! - `/search [provider]`：把指定 web_search 方式移到优先级首位。
//! - `/search dis|en <provider>`：运行期禁用/启用方式并同步写回 rclaw.toml。
//! - `/searchfallback [on|off]`：开关 `web_search` 的主→辅 fallback（关闭后只用主搜索源，
//!   失败也不回退），同步写回 rclaw.toml。
//! - `/skills`：由 turn 命令分发器列出当前 workspace 可发现的 skill 名称、所属技能包与功能。
//! - `/tools`：由 turn 命令分发器列出当前注册的全部工具名称与说明。
//! - `/log`：由 turn 命令分发器查看 / 发送 / 删除 agent/logs 下的日志文件。
//! - `/help`：列出可用命令。
//!
//! 模型与思考强度切换已迁移到会话页输入框底部的选择器（`SlashRegistry::set_model` /
//! `set_thinking`）；旧 `/llmodel` 只保留一条引导文案，不再修改任何配置。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rclaw_llm::{ProviderConfig, ProviderFlavor, SwappableProvider};
use rclaw_tools::{BackendId, WebSearchControl};

/// 斜杠命令处理结果。
pub enum CommandOutcome {
    /// 命中命令；附带要回送给用户的文本（agent_loop 据此直接 outbound 回复）。
    Handled(String),
    /// 非斜杠命令，放行进入正常 LLM 对话。
    NotACommand,
}

/// 已知模型规格：会话页模型选择器的唯一事实源。
///
/// 切换时按这张表 **整组覆盖** provider / base_url / context_length / max_tokens /
/// thinking / reasoning_effort / media_wire / 字段名，并全部写回 `rclaw.toml`；
/// 不再只改 model/base_url（旧实现会留下与模型不匹配的输出上限与思考参数）。
struct ModelProfile {
    model: &'static str,
    label: &'static str,
    provider: &'static str,
    base_url: &'static str,
    context_length: usize,
    max_tokens: u32,
    thinking: &'static str,
    reasoning_effort: &'static str,
    media_wire: &'static str,
    reasoning_field: &'static str,
    max_tokens_key: &'static str,
}

/// 模型选择器开放切换的白名单（只认这三个）。
///
/// - `glm-5.3-flash`：智谱原生多模态，强制思考，effort 只收 max/high/low；
/// - `glm-5.4`：智谱当前旗舰（多模态，1M/128K），effort low/high/max；
/// - `deepseek-flash`：DeepSeek-V4.1-Flash-1017（1M/384K，支持图像），
///   effort none/low/high/max，可关闭思考。
///
/// 其它模型（`glm-5.3` / `glm-5.2` / `glm-5v-turbo` / `deepseek-v4-pro` 等）不再映射；
/// 需要时手改 `rclaw.toml` 后重启。
const MODEL_PROFILES: &[ModelProfile] = &[
    ModelProfile {
        model: "glm-5.3-flash",
        label: "GLM-5.3-Flash",
        provider: "zhipu",
        base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
        // token 计数用十进制（行业惯例），不是 1024 进制的字节数。
        context_length: 1_000_000,
        max_tokens: 128_000,
        thinking: "enabled",
        reasoning_effort: "max",
        media_wire: "openai",
        reasoning_field: "reasoning_content",
        max_tokens_key: "max_tokens",
    },
    ModelProfile {
        model: "glm-5.4",
        label: "GLM-5.4",
        provider: "zhipu",
        base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
        context_length: 1_000_000,
        max_tokens: 128_000,
        thinking: "enabled",
        reasoning_effort: "max",
        media_wire: "openai",
        reasoning_field: "reasoning_content",
        max_tokens_key: "max_tokens",
    },
    ModelProfile {
        model: "deepseek-flash",
        label: "DeepSeek Flash",
        provider: "deepseek",
        base_url: "https://api.deepseek.com/v1",
        // DeepSeek 官方 1M = 1,048,576。
        context_length: 1_048_576,
        max_tokens: 128_000,
        thinking: "enabled",
        reasoning_effort: "high",
        media_wire: "deepseek",
        reasoning_field: "reasoning_content",
        max_tokens_key: "max_tokens",
    },
];

fn resolve_model_spec(input: &str) -> Option<&'static ModelProfile> {
    let target = input.trim().to_ascii_lowercase();
    MODEL_PROFILES
        .iter()
        .find(|profile| profile.model.eq_ignore_ascii_case(&target))
}

fn profile_flavor(profile: &ModelProfile) -> ProviderFlavor {
    match profile.provider {
        "zhipu" => ProviderFlavor::Zhipu,
        "deepseek" => ProviderFlavor::DeepSeek,
        _ => ProviderFlavor::Generic,
    }
}

/// scheme + host + 有效端口相同（路径可不同；显式 `:443`/`:80` 与省略等价）。
///
/// 与配置校验/请求构建共用 `url` crate 的解析结果：手工切字符串会把某些合法
/// URL 的路径内容误判成 authority，导致「同源判断通过、key 实际发往别处」。
fn same_origin(left: &str, right: &str) -> bool {
    fn origin(value: &str) -> Option<(String, String, u16)> {
        let url = url::Url::parse(value.trim()).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        Some((
            url.scheme().to_ascii_lowercase(),
            url.host_str()?.to_ascii_lowercase(),
            url.port_or_known_default()?,
        ))
    }
    match (origin(left), origin(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// 会话模型覆盖的 cache/kv 键（channel 与 chat_id 组合）。
fn session_model_key(channel: &str, chat_id: &str) -> String {
    format!("{channel}\u{1f}{chat_id}")
}

fn check_expected_identity(
    config: &ProviderConfig,
    expected_provider: Option<&str>,
    expected_model: Option<&str>,
) -> Result<(), String> {
    if let Some(expected) = expected_provider {
        if !expected.is_empty() && expected != config.name {
            return Err("模型已切换，请刷新后重试".into());
        }
    }
    if let Some(expected) = expected_model {
        if !expected.is_empty() && expected != config.default_model {
            return Err("模型已切换，请刷新后重试".into());
        }
    }
    Ok(())
}

/// 按 provider 名构造 ProviderConfig 的工厂（跨 provider 切换用）。
///
/// 由宿主（bootstrap）用 `Config::build_provider_config_for` 注入；返回 Err 表示该
/// provider 条目不存在或缺少 api_key。这里用闭包而不是直接依赖 rclaw-cli，避免
/// runtime → cli 的反向依赖。
/// 模型选择器 profile 要覆盖到目标条目上的整组参数。
///
/// 工厂先把这些值覆盖到条目再校验/构造，保证「保存前验证的就是最终生效配置」；
/// 旧条目的模型/档位检查不会阻断本会被整组替换的切换。
#[derive(Clone, Copy, Debug)]
pub struct ProviderProfileOverride<'a> {
    pub model: &'a str,
    pub media_wire: &'a str,
    pub thinking: &'a str,
    pub reasoning_effort: &'a str,
    pub reasoning_field: &'a str,
    pub max_tokens_key: &'a str,
    pub max_tokens: u32,
}

pub type ProviderFactory = Arc<
    dyn Fn(&str, &str, Option<&ProviderProfileOverride<'_>>) -> Result<ProviderConfig, String>
        + Send
        + Sync,
>;

/// 单个会话的模型覆盖（选择器写入；不修改全局 rclaw.toml）。
#[derive(Clone)]
struct SessionModelEntry {
    config: ProviderConfig,
    provider: Arc<dyn rclaw_llm::Provider>,
    /// true = 来自显式模型选择（套 profile 参数）；false = 仅在默认配置上改档位。
    profile_params: bool,
}

/// 斜杠命令注册表。持有一份 provider 热切换句柄、rclaw.toml 路径与当前选中的 provider 名。
pub struct SlashRegistry {
    provider: Arc<SwappableProvider>,
    /// rclaw.toml 路径（与 `Config::load` 读取的相同）。
    config_path: PathBuf,
    /// 当前选中的 `[[llm.providers]]` 名（`[llm].provider`）——持久化时定位要改的 provider 段。
    /// 跨 provider 切换后需要更新；Mutex 让模型切换与思考强度切换共享同一份状态。
    selected_provider: Mutex<String>,
    /// 跨 provider 切换的工厂；None 时只能在当前 provider 内切换。
    provider_factory: Option<ProviderFactory>,
    /// web_search 顺序、手动启禁、健康状态及 fallback 开关的共享控制面。
    web_search_control: Option<Arc<WebSearchControl>>,
    /// 串行化运行时切换与 TOML 写回，避免多个会话的斜杠命令相互覆盖。
    config_mutation: Mutex<()>,
    /// 模型/思考状态的单调版本：GET、SET 回执与广播统一用它排序。
    state_revision: AtomicU64,
    /// 会话级模型覆盖（channel + chat_id 组合）；不写全局配置。
    session_models: Mutex<HashMap<String, SessionModelEntry>>,
    /// 会话模型持久化（state.sqlite kv）；None = 纯内存（测试/精简装配）。
    session_store: Option<rclaw_store::SessionStore>,
    /// 每个 chat 独立的状态版本：A 会话切换不会使 B 的菜单提交过期。
    session_revisions: Mutex<HashMap<String, u64>>,
    /// 全清/删除后的版本地板：未登记过 revision 的会话默认从这里开始，保证清理前
    /// 取得的旧版本（含 0）在清理后一定过期。
    session_epoch: AtomicU64,
    /// 启动期压缩预算（threshold, tail）：切到更小窗口的会话模型前必须放得下，
    /// 否则拒绝切换，避免自动压缩永远不触发。
    session_budget: Option<(usize, usize)>,
}

impl SlashRegistry {
    pub fn new(
        provider: Arc<SwappableProvider>,
        config_path: PathBuf,
        selected_provider: String,
    ) -> Self {
        SlashRegistry {
            provider,
            config_path,
            selected_provider: Mutex::new(selected_provider),
            provider_factory: None,
            web_search_control: None,
            config_mutation: Mutex::new(()),
            state_revision: AtomicU64::new(1),
            session_models: Mutex::new(HashMap::new()),
            session_store: None,
            session_revisions: Mutex::new(HashMap::new()),
            session_epoch: AtomicU64::new(0),
            session_budget: None,
        }
    }

    /// 当前 live provider 配置的克隆（只读）。
    pub fn current_config(&self) -> Result<ProviderConfig, String> {
        self.provider
            .current_config()
            .map_err(|error| format!("读取当前 provider 失败：{error}"))
    }

    /// 取得会话模型变更锁（`/clear` 用它把「主事务提交 + 缓存/epoch 失效」
    /// 与模型 setter 串起来；调用方必须在同一线程内持有到全清结束）。
    pub fn session_mutation_guard(&self) -> Result<std::sync::MutexGuard<'_, ()>, String> {
        match self.config_mutation.lock() {
            Ok(guard) => Ok(guard),
            Err(poisoned) => {
                let guard = poisoned.into_inner();
                self.config_mutation.clear_poison();
                Ok(guard)
            }
        }
    }

    /// 注入启动期压缩预算：会话模型窗口必须容得下 threshold/tail，否则拒绝切换。
    pub fn with_session_budget(mut self, threshold: usize, tail: usize) -> Self {
        self.session_budget = Some((threshold, tail));
        self
    }

    /// 注入会话存储：会话模型选择按 chat 持久化到 state.sqlite kv。
    pub fn with_session_store(mut self, session: rclaw_store::SessionStore) -> Self {
        self.session_store = Some(session);
        self
    }

    /// 注入跨 provider 构造工厂（模型选择器跨厂切换需要）。
    pub fn with_provider_factory(mut self, factory: ProviderFactory) -> Self {
        self.provider_factory = Some(factory);
        self
    }

    /// 当前选中的 provider 名（含跨 provider 切换后的最新值）。
    pub fn selected_provider(&self) -> String {
        self.selected_provider
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// live provider 身份 `(name, model)`：状态条 / UI 不读启动快照。
    pub fn live_provider_summary(&self) -> Option<(String, String)> {
        self.provider
            .current_config()
            .ok()
            .map(|config| (config.name, config.default_model))
    }

    /// 当前模型的思考强度状态（选项按 live provider 能力族 + 模型判定）。
    pub fn thinking_state(&self) -> crate::thinking::ThinkingState {
        crate::thinking::state_from(&self.provider)
    }

    fn model_state_of(&self, config: &ProviderConfig) -> crate::thinking::ModelState {
        let models = MODEL_PROFILES
            .iter()
            .map(|profile| crate::thinking::ModelOption {
                model: profile.model,
                label: profile.label,
                provider: profile.provider,
                efforts: crate::thinking::options_for_model(profile.model),
            })
            .collect::<Vec<_>>();
        crate::thinking::ModelState {
            supported: !models.is_empty(),
            provider: config.name.clone(),
            model: config.default_model.clone(),
            models,
        }
    }

    /// 「模型 + 思考强度」的完整状态：**同一份 live 配置快照**生成，避免两次读之间
    /// 被另一个窗口切换，返回新旧混合身份。
    pub fn llm_state(&self) -> crate::thinking::LlmState {
        // 与 SET 共用同一把配置锁：GET 不会在“配置已换、版本未递增”之间读到混合状态。
        let _guard = self.config_mutation.lock();
        let revision = self.state_revision.load(Ordering::SeqCst);
        match self.provider.current_config() {
            Ok(config) => crate::thinking::LlmState {
                revision,
                model: self.model_state_of(&config),
                thinking: crate::thinking::state_from_config(&config),
            },
            Err(_) => crate::thinking::LlmState {
                revision,
                model: crate::thinking::ModelState {
                    supported: false,
                    provider: String::new(),
                    model: String::new(),
                    models: Vec::new(),
                },
                thinking: crate::thinking::ThinkingState::unsupported(String::new(), String::new()),
            },
        }
    }

    /// 会话级模型状态：优先该 chat 的独立覆盖，否则用调用方给的默认（bot 固定
    /// provider / 主 provider）配置。只读，不会修改全局 provider 或 rclaw.toml。
    pub fn session_llm_state(
        &self,
        channel: &str,
        chat_id: &str,
        fallback: Option<&ProviderConfig>,
    ) -> crate::thinking::LlmState {
        let _guard = self.config_mutation.lock();
        let key = session_model_key(channel, chat_id);
        let revision = self.session_revision(&key);
        let config = self.effective_session_config(channel, chat_id, fallback);
        match config {
            Some(config) => crate::thinking::LlmState {
                revision,
                model: self.model_state_of(&config),
                thinking: crate::thinking::state_from_config(&config),
            },
            None => crate::thinking::LlmState {
                revision,
                model: crate::thinking::ModelState {
                    supported: false,
                    provider: String::new(),
                    model: String::new(),
                    models: Vec::new(),
                },
                thinking: crate::thinking::ThinkingState::unsupported(String::new(), String::new()),
            },
        }
    }

    /// 当前 chat 的独立 provider（没有覆盖时 None，调用方回落到 bot/主 provider）。
    ///
    /// 只读缓存/持久化；构建失败（例如旧模型已从配置删除）记录告警并返回 None，
    /// 不让一轮对话因为过期的会话选择而失败。
    pub fn session_provider(
        &self,
        channel: &str,
        chat_id: &str,
    ) -> Option<Arc<dyn rclaw_llm::Provider>> {
        let key = session_model_key(channel, chat_id);
        if let Ok(cache) = self.session_models.lock() {
            if let Some(entry) = cache.get(&key) {
                return Some(entry.provider.clone());
            }
        }
        let _guard = match self.config_mutation.lock() {
            Ok(guard) => guard,
            Err(_) => return None,
        };
        match self.load_session_entry(channel, chat_id) {
            Ok(Some(entry)) => {
                let provider = entry.provider.clone();
                if let Ok(mut cache) = self.session_models.lock() {
                    cache.insert(key, entry);
                }
                Some(provider)
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(
                    channel,
                    chat_id,
                    %error,
                    "会话模型覆盖不可用，回退到默认 provider"
                );
                None
            }
        }
    }

    /// 设置某会话的模型（不写 rclaw.toml，不影响其它会话）。
    #[allow(clippy::too_many_arguments)]
    pub fn set_session_model_at(
        &self,
        channel: &str,
        chat_id: &str,
        model: &str,
        expected_provider: Option<&str>,
        expected_model: Option<&str>,
        expected_revision: Option<u64>,
        fallback: Option<&ProviderConfig>,
    ) -> Result<crate::thinking::LlmState, String> {
        let _mutation = self
            .config_mutation
            .lock()
            .map_err(|_| "配置修改锁已损坏，未切换模型。".to_string())?;
        let key = session_model_key(channel, chat_id);
        let current_revision = self.session_revision(&key);
        if let Some(expected) = expected_revision {
            if expected != current_revision {
                return Err("状态已更新，请刷新后重试".into());
            }
        }
        let current = self
            .effective_session_config(channel, chat_id, fallback)
            .ok_or_else(|| "读取当前模型失败".to_string())?;
        check_expected_identity(&current, expected_provider, expected_model)?;
        let (config, provider) = self.build_session_provider(model, None, Some(&current))?;
        self.persist_session_selection(
            channel,
            chat_id,
            &config.name,
            &config.base_url,
            model,
            None,
            true,
        )?;
        if let Ok(mut cache) = self.session_models.lock() {
            cache.insert(
                key.clone(),
                SessionModelEntry {
                    config: config.clone(),
                    provider,
                    profile_params: true,
                },
            );
        }
        let revision = self.set_session_revision(&key, current_revision + 1);
        Ok(crate::thinking::LlmState {
            revision,
            model: self.model_state_of(&config),
            thinking: crate::thinking::state_from_config(&config),
        })
    }

    /// 设置某会话的思考强度（不写 rclaw.toml，不影响其它会话）。
    #[allow(clippy::too_many_arguments)]
    pub fn set_session_thinking_at(
        &self,
        channel: &str,
        chat_id: &str,
        value: &str,
        expected_provider: Option<&str>,
        expected_model: Option<&str>,
        expected_revision: Option<u64>,
        fallback: Option<&ProviderConfig>,
    ) -> Result<crate::thinking::LlmState, String> {
        let _mutation = self
            .config_mutation
            .lock()
            .map_err(|_| "配置修改锁已损坏，未修改思考强度。".to_string())?;
        let key = session_model_key(channel, chat_id);
        let current_revision = self.session_revision(&key);
        if let Some(expected) = expected_revision {
            if expected != current_revision {
                return Err("状态已更新，请刷新后重试".into());
            }
        }
        let current = self
            .effective_session_config(channel, chat_id, fallback)
            .ok_or_else(|| "读取当前模型失败".to_string())?;
        check_expected_identity(&current, expected_provider, expected_model)?;
        let options = crate::thinking::options_for(&current);
        if !options.iter().any(|option| option.value == value) {
            return Err(format!(
                "当前模型「{}」不支持思考强度「{value}」",
                current.default_model
            ));
        }
        // 调档只保留**当前 live** 的 provider 配置、凭据与端点，仅覆盖思考字段；
        // 不再从磁盘重读条目（否则配置迁移后会用新凭据请求旧地址）。
        let profile_params = self
            .session_models
            .lock()
            .ok()
            .and_then(|cache| cache.get(&key).map(|entry| entry.profile_params))
            .unwrap_or(false);
        let model = current.default_model.clone();
        let config = self.apply_session_reasoning(current.clone(), value)?;
        let provider: Arc<dyn rclaw_llm::Provider> = Arc::from(
            rclaw_llm::build_provider(config.clone())
                .map_err(|error| format!("切换思考强度失败（未改动）：{error}"))?,
        );
        self.persist_session_selection(
            channel,
            chat_id,
            &config.name,
            &config.base_url,
            &model,
            Some(value),
            profile_params,
        )?;
        if let Ok(mut cache) = self.session_models.lock() {
            cache.insert(
                key.clone(),
                SessionModelEntry {
                    config: config.clone(),
                    provider,
                    profile_params,
                },
            );
        }
        let revision = self.set_session_revision(&key, current_revision + 1);
        Ok(crate::thinking::LlmState {
            revision,
            model: self.model_state_of(&config),
            thinking: crate::thinking::state_from_config(&config),
        })
    }

    /// 会话删除/清空后丢弃进程内缓存；持久化 kv 由 SessionStore 一并清理。
    pub fn forget_session_model(&self, channel: &str, chat_id: &str) {
        let _guard = self.config_mutation.lock();
        let key = session_model_key(channel, chat_id);
        if let Ok(mut cache) = self.session_models.lock() {
            cache.remove(&key);
        }
        // 删除后保留一个更高的 revision：已打开的窗口下一次 GET 能应用默认状态，
        // 而不是被旧的更大 revision 拦住。
        let current = self.session_revision(&key);
        if let Ok(mut revisions) = self.session_revisions.lock() {
            revisions.insert(key.clone(), current + 1);
        }
        if let Some(store) = self.session_store.as_ref() {
            if let Err(error) = store.journal().kv_delete(&key) {
                tracing::warn!(%error, "删除会话模型 kv 失败");
            }
        }
    }

    pub fn forget_all_session_models(&self) {
        let _guard = self.config_mutation.lock();
        self.forget_all_session_models_locked();
    }

    /// 全清路径：调用方已持 `config_mutation`（见 [`Self::session_mutation_guard`]）。
    pub fn forget_all_session_models_locked(&self) {
        self.invalidate_all_session_models_locked();
        if let Some(store) = self.session_store.as_ref() {
            if let Err(error) = store.delete_all_session_models() {
                tracing::warn!(%error, "清空会话模型 kv 失败");
            }
        }
    }

    /// 全清路径的「提交状态未知」分支：只丢弃进程内缓存并推进 epoch，让下一次
    /// 冷读按数据库实际状态恢复；**不删**持久化 KV——主事务可能没有提交，删了
    /// 会丢掉仍然有效的模型设置。调用方已持 `config_mutation`。
    pub fn invalidate_all_session_models_locked(&self) {
        if let Ok(mut cache) = self.session_models.lock() {
            cache.clear();
        }
        let next_epoch = self
            .session_revisions
            .lock()
            .map(|revisions| {
                revisions
                    .values()
                    .copied()
                    .max()
                    .unwrap_or(0)
                    .max(self.session_epoch.load(Ordering::SeqCst))
                    + 1
            })
            .unwrap_or_else(|_| self.session_epoch.load(Ordering::SeqCst) + 1);
        self.session_epoch.store(next_epoch, Ordering::SeqCst);
        if let Ok(mut revisions) = self.session_revisions.lock() {
            revisions.clear();
        }
    }

    /// 读取某个 provider 条目的配置（不写盘、不构造 live provider）；供会话页
    /// 在 bot 固定 provider 上显示默认模型/档位。
    pub fn provider_config_for(&self, name: &str) -> Result<ProviderConfig, String> {
        let snapshot = std::fs::read_to_string(&self.config_path).map_err(|error| {
            format!(
                "读取 rclaw.toml 失败（{}）：{error}",
                self.config_path.display()
            )
        })?;
        let factory = self
            .provider_factory
            .as_ref()
            .ok_or_else(|| "配置事务未启用，无法读取 provider".to_string())?;
        factory(name, &snapshot, None)
    }

    fn session_revision(&self, key: &str) -> u64 {
        self.session_revisions
            .lock()
            .ok()
            .and_then(|revisions| revisions.get(key).copied())
            .unwrap_or_else(|| self.session_epoch.load(Ordering::SeqCst))
    }

    fn set_session_revision(&self, key: &str, revision: u64) -> u64 {
        if let Ok(mut revisions) = self.session_revisions.lock() {
            revisions.insert(key.to_string(), revision);
        }
        revision
    }

    fn effective_session_config(
        &self,
        channel: &str,
        chat_id: &str,
        fallback: Option<&ProviderConfig>,
    ) -> Option<ProviderConfig> {
        let key = session_model_key(channel, chat_id);
        if let Ok(cache) = self.session_models.lock() {
            if let Some(entry) = cache.get(&key) {
                return Some(entry.config.clone());
            }
        }
        match self.load_session_entry(channel, chat_id) {
            Ok(Some(entry)) => {
                let config = entry.config.clone();
                if let Ok(mut cache) = self.session_models.lock() {
                    cache.insert(key, entry);
                }
                Some(config)
            }
            Ok(None) => fallback
                .cloned()
                .or_else(|| self.provider.current_config().ok()),
            Err(error) => {
                tracing::warn!(
                    channel,
                    chat_id,
                    %error,
                    "会话模型覆盖读取失败，回退到默认 provider"
                );
                fallback
                    .cloned()
                    .or_else(|| self.provider.current_config().ok())
            }
        }
    }

    fn load_session_entry(
        &self,
        channel: &str,
        chat_id: &str,
    ) -> Result<Option<SessionModelEntry>, String> {
        let Some(store) = self.session_store.as_ref() else {
            return Ok(None);
        };
        let Some(value) = store
            .journal()
            .kv_get(&rclaw_store::session::session_model_key(channel, chat_id))
            .map_err(|error| format!("读取会话模型失败: {error}"))?
        else {
            return Ok(None);
        };
        let model = value
            .get("model")
            .and_then(|model| model.as_str())
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .ok_or_else(|| "会话模型记录缺少 model".to_string())?;
        let effort = value
            .get("effort")
            .and_then(|effort| effort.as_str())
            .map(str::trim)
            .filter(|effort| !effort.is_empty())
            .map(str::to_string);
        let provider_name = value
            .get("provider")
            .and_then(|provider| provider.as_str())
            .map(str::trim)
            .filter(|provider| !provider.is_empty())
            .ok_or_else(|| "会话模型记录缺少 provider".to_string())?;
        let base_url = value
            .get("base_url")
            .and_then(|url| url.as_str())
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| "会话模型记录缺少 base_url".to_string())?;
        let profile_params = value
            .get("profile")
            .and_then(|flag| flag.as_bool())
            .ok_or_else(|| "会话模型记录缺少 profile".to_string())?;
        let (config, provider) = self.build_session_from_saved_provider(
            provider_name,
            Some(base_url),
            model,
            effort.as_deref(),
            profile_params,
        )?;
        Ok(Some(SessionModelEntry {
            config,
            provider,
            profile_params,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn persist_session_selection(
        &self,
        channel: &str,
        chat_id: &str,
        provider: &str,
        base_url: &str,
        model: &str,
        effort: Option<&str>,
        profile_params: bool,
    ) -> Result<(), String> {
        let Some(store) = self.session_store.as_ref() else {
            return Ok(());
        };
        store
            .journal()
            .kv_set(
                &rclaw_store::session::session_model_key(channel, chat_id),
                &serde_json::json!({
                    "provider": provider,
                    "base_url": base_url,
                    "model": model,
                    "effort": effort,
                    "profile": profile_params
                }),
            )
            .map_err(|error| format!("保存会话模型失败: {error}"))
    }

    fn check_session_window(&self, profile: &ModelProfile) -> Result<(), String> {
        let Some((threshold, tail)) = self.session_budget else {
            return Ok(());
        };
        let usable = profile
            .context_length
            .saturating_sub(profile.max_tokens as usize);
        if threshold > usable || tail > usable {
            return Err(format!(
                "无法切换到「{}」：启动期压缩预算（阈值 {threshold} / 尾窗 {tail}）                 超过该模型可用输入窗口 {usable}；请调低 [agent] compression_threshold_tokens /                  compression_tail_token_budget 或主 context_length",
                profile.model
            ));
        }
        Ok(())
    }

    fn apply_session_reasoning(
        &self,
        mut base: ProviderConfig,
        value: &str,
    ) -> Result<ProviderConfig, String> {
        if !crate::thinking::options_for(&base)
            .iter()
            .any(|option| option.value == value)
        {
            return Err(format!(
                "当前模型「{}」不支持思考强度「{value}」",
                base.default_model
            ));
        }
        base.reasoning =
            crate::thinking::reasoning_for(value).ok_or_else(|| "未知思考强度".to_string())?;
        Ok(base)
    }

    /// 从**已保存/已生效的 provider 条目**重建会话 provider：保留该条目的端点与
    /// 账户，只覆盖模型（profile 模型同时覆盖 profile 参数）与思考字段。
    #[allow(clippy::too_many_arguments)]
    fn build_session_from_saved_provider(
        &self,
        provider_name: &str,
        base_url: Option<&str>,
        model: &str,
        effort: Option<&str>,
        profile_params: bool,
    ) -> Result<(ProviderConfig, Arc<dyn rclaw_llm::Provider>), String> {
        let snapshot = std::fs::read_to_string(&self.config_path).map_err(|error| {
            format!(
                "读取 rclaw.toml 失败（{}）：{error}",
                self.config_path.display()
            )
        })?;
        let factory = self
            .provider_factory
            .as_ref()
            .ok_or_else(|| "配置事务未启用，无法重建会话模型".to_string())?;
        let mut config = if profile_params {
            let Some(profile) = resolve_model_spec(model) else {
                return Err(format!("不支持的模型「{model}」"));
            };
            self.check_session_window(profile)?;
            if let Some(effort) = effort {
                if !crate::thinking::options_for_model(profile.model)
                    .iter()
                    .any(|option| option.value == effort)
                {
                    return Err(format!(
                        "模型「{}」不支持思考强度「{effort}」",
                        profile.model
                    ));
                }
            }
            let (thinking, reasoning_effort) = match effort {
                Some("disabled") => ("disabled", ""),
                Some(effort) => ("enabled", effort),
                None => (profile.thinking, profile.reasoning_effort),
            };
            let over = ProviderProfileOverride {
                model: profile.model,
                media_wire: profile.media_wire,
                thinking,
                reasoning_effort,
                reasoning_field: profile.reasoning_field,
                max_tokens_key: profile.max_tokens_key,
                max_tokens: profile.max_tokens,
            };
            // 保留条目自己的 base_url（不再套官方 URL）；初始选模型时已经校验过同源。
            factory(provider_name, &snapshot, Some(&over))?
        } else {
            let mut base = factory(provider_name, &snapshot, None)?;
            base.default_model = model.to_string();
            if let Some(effort) = effort {
                base = self.apply_session_reasoning(base, effort)?;
            }
            base
        };
        // 恢复时以当前条目为凭据来源，保存的端点必须与当前条目同源，否则拒绝
        // （避免把迁移后的新凭据发到旧地址）。旧版 KV 没有端点时，profile 模型
        // 仍要求官方同源 + Bearer，并回到 profile 官方 URL。
        if let Some(base_url) = base_url {
            if !same_origin(&config.base_url, base_url) {
                return Err(format!(
                    "会话模型已失效：保存的端点与 provider「{provider_name}」当前条目不同源"
                ));
            }
            config.base_url = base_url.to_string();
        } else if let Some(profile) = resolve_model_spec(model) {
            if !same_origin(&config.base_url, profile.base_url)
                || !matches!(config.auth, rclaw_llm::AuthScheme::Bearer)
            {
                return Err(format!(
                    "会话模型已失效：provider「{provider_name}」当前条目不再满足官方 profile 约束"
                ));
            }
            config.base_url = profile.base_url.to_string();
        }
        let built: Arc<dyn rclaw_llm::Provider> = Arc::from(
            rclaw_llm::build_provider(config.clone())
                .map_err(|error| format!("会话 provider 不可用：{error}"))?,
        );
        Ok((config, built))
    }

    /// 按白名单 profile 构造**独立** provider（不写配置、不动全局 live）。
    fn build_session_provider(
        &self,
        model: &str,
        effort: Option<&str>,
        preferred: Option<&ProviderConfig>,
    ) -> Result<(ProviderConfig, Arc<dyn rclaw_llm::Provider>), String> {
        let profile = resolve_model_spec(model).ok_or_else(|| {
            let supported = MODEL_PROFILES
                .iter()
                .map(|profile| profile.model)
                .collect::<Vec<_>>()
                .join(" / ");
            format!("不支持的模型「{model}」。可选：{supported}")
        })?;
        let options = crate::thinking::options_for_model(profile.model);
        if let Some(effort) = effort {
            if !options.iter().any(|option| option.value == effort) {
                return Err(format!(
                    "模型「{}」不支持思考强度「{effort}」",
                    profile.model
                ));
            }
        }
        self.check_session_window(profile)?;
        let current = self
            .provider
            .current_config()
            .map_err(|error| format!("读取当前 provider 失败：{error}"))?;
        let target_flavor = profile_flavor(profile);
        // 优先沿用该会话当前/已保存的条目（同源、同能力族）；否则沿用主 provider；
        // 最后才用 profile 的 canonical 条目。这样同源自定义名称条目不会被换账户。
        let matches = |cfg: &ProviderConfig| {
            target_flavor != ProviderFlavor::Generic
                && cfg.flavor == target_flavor
                && same_origin(&cfg.base_url, profile.base_url)
        };
        let target_provider = preferred
            .filter(|cfg| matches(cfg))
            .map(|cfg| cfg.name.clone())
            .or_else(|| {
                if matches(&current) {
                    Some(current.name.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| profile.provider.to_string());
        let snapshot = std::fs::read_to_string(&self.config_path).map_err(|error| {
            format!(
                "读取 rclaw.toml 失败（{}）：{error}",
                self.config_path.display()
            )
        })?;
        let factory = self
            .provider_factory
            .as_ref()
            .ok_or_else(|| "配置事务未启用，无法切换会话模型".to_string())?;
        let (thinking, reasoning_effort) = match effort {
            Some("disabled") => ("disabled", ""),
            Some(effort) => ("enabled", effort),
            None => (profile.thinking, profile.reasoning_effort),
        };
        let over = ProviderProfileOverride {
            model: profile.model,
            media_wire: profile.media_wire,
            thinking,
            reasoning_effort,
            reasoning_field: profile.reasoning_field,
            max_tokens_key: profile.max_tokens_key,
            max_tokens: profile.max_tokens,
        };
        let mut pcfg = factory(&target_provider, &snapshot, Some(&over))
            .map_err(|error| format!("会话模型不可用（provider「{target_provider}」）：{error}"))?;
        if !same_origin(&pcfg.base_url, profile.base_url) {
            return Err(format!(
                "会话模型不可用：provider「{target_provider}」端点与官方端点不同源"
            ));
        }
        if !matches!(pcfg.auth, rclaw_llm::AuthScheme::Bearer) {
            return Err(format!(
                "会话模型不可用：provider「{target_provider}」使用自定义鉴权头"
            ));
        }
        pcfg.base_url = profile.base_url.to_string();
        let built: Arc<dyn rclaw_llm::Provider> =
            Arc::from(rclaw_llm::build_provider(pcfg.clone()).map_err(|error| error.to_string())?);
        Ok((pcfg, built))
    }

    /// 切换当前模型的思考强度。
    ///
    /// 顺序：取 live 配置 → 校验预期身份/磁盘模型一致 → 校验档位 → **先写 TOML** →
    /// 再发布到 live provider。写盘失败时 live 保持原样（不会出现"运行时已切、重启
    /// 回退"的半套状态）；与模型切换、`/search` 共用 `config_mutation` 锁。
    pub fn set_thinking(
        &self,
        value: &str,
        expected_provider: Option<&str>,
        expected_model: Option<&str>,
    ) -> Result<crate::thinking::LlmState, String> {
        self.set_thinking_at(value, expected_provider, expected_model, None)
    }

    /// 同 [`Self::set_thinking`]，额外校验前端看到的 revision（拒绝过期操作）。
    pub fn set_thinking_at(
        &self,
        value: &str,
        expected_provider: Option<&str>,
        expected_model: Option<&str>,
        expected_revision: Option<u64>,
    ) -> Result<crate::thinking::LlmState, String> {
        let _mutation = self
            .config_mutation
            .lock()
            .map_err(|_| "配置修改锁已损坏，未修改思考强度。".to_string())?;
        if let Some(expected) = expected_revision {
            if expected != self.state_revision.load(Ordering::SeqCst) {
                return Err("状态已更新，请刷新后重试".into());
            }
        }
        let mut config = self
            .provider
            .current_config()
            .map_err(|error| format!("读取当前模型失败：{error}"))?;
        // 乐观并发保护：菜单打开后模型可能已被另一窗口/会话切走，旧档位不能盲写。
        if let Some(expected) = expected_provider {
            if !expected.is_empty() && expected != config.name {
                return Err("模型已切换，请刷新后重试".into());
            }
        }
        if let Some(expected) = expected_model {
            if !expected.is_empty() && expected != config.default_model {
                return Err("模型已切换，请刷新后重试".into());
            }
        }
        // 配置面板可能已经改了磁盘条目；写到"磁盘上的另一个模型"会产生错误语义
        // （例如把 disabled 写给强制思考的 glm-5.3-flash）。先对齐磁盘身份。
        let snapshot = std::fs::read_to_string(&self.config_path).map_err(|error| {
            format!(
                "读取 rclaw.toml 失败（{}）：{error}",
                self.config_path.display()
            )
        })?;
        if let Some(selected) = rclaw_store::toml_file::RclawToml::new(&self.config_path)
            .selected_provider_in_text(&snapshot)
            .map_err(|error| format!("切换思考强度失败（未修改）：{error}"))?
        {
            if selected != config.name {
                return Err(
                    "配置已被其他窗口修改（选中的 provider 已变化），请重启 RClaw 后再切换".into(),
                );
            }
        }
        if let Some(factory) = self.provider_factory.as_ref() {
            let disk = factory(&config.name, &snapshot, None)
                .map_err(|error| format!("读取磁盘 provider 失败（未修改）：{error}"))?;
            if disk.default_model != config.default_model || disk.base_url != config.base_url {
                return Err("配置已被其他窗口修改，请重启 RClaw 后再切换思考强度".into());
            }
            // 用磁盘条目的最新凭据（可能刚轮换过 key）；只覆盖思考参数，其余保持磁盘值。
            config = disk;
        }
        let options = crate::thinking::options_for(&config);
        if options.is_empty() {
            return Err(format!(
                "当前模型「{}」不支持思考强度设置",
                config.default_model
            ));
        }
        if !options.iter().any(|option| option.value == value) {
            return Err(format!(
                "当前模型「{}」不支持思考强度「{value}」",
                config.default_model
            ));
        }
        let Some(reasoning) = crate::thinking::reasoning_for(value) else {
            return Err(format!("未知思考强度「{value}」"));
        };
        config.reasoning = reasoning;
        // 先验证新配置可构造（base_url / 鉴权等），避免写盘后 live 发布失败。
        let built: Arc<dyn rclaw_llm::Provider> = Arc::from(
            rclaw_llm::build_provider(config.clone())
                .map_err(|error| format!("切换思考强度失败（未改动）：{error}"))?,
        );
        let (thinking, effort) = if value == "disabled" {
            ("disabled", None)
        } else {
            ("enabled", Some(value))
        };
        rclaw_store::toml_file::RclawToml::new(&self.config_path)
            .set_reasoning_fields(&config.name, thinking, effort, Some(&snapshot))
            .map_err(|error| format!("切换思考强度失败（未修改）：{error}"))?;
        self.provider.switch_provider_built(config.clone(), built);
        let revision = self.state_revision.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(crate::thinking::LlmState {
            revision,
            model: self.model_state_of(&config),
            thinking: crate::thinking::state_from_config(&config),
        })
    }

    /// 注入 web_search 控制面，启用顺序、启禁与 fallback 的热切换。
    pub fn with_web_search_control(mut self, control: Arc<WebSearchControl>) -> Self {
        self.web_search_control = Some(control);
        self
    }

    /// web_search 控制面句柄（turn 组装工具面 / 系统提示时做禁能联动用）。
    pub fn web_search_control(&self) -> Option<Arc<WebSearchControl>> {
        self.web_search_control.clone()
    }

    /// 尝试把一条消息正文当作斜杠命令处理。
    ///
    /// - 非 `/` 开头 → [`CommandOutcome::NotACommand`]，放行进 LLM；
    /// - `/` 后是已知命令名（`search` / `searchfallback` / `help`；`llmodel` 只回引导）→
    ///   执行命令并返回回复；
    /// - `/` 后是未知名（如 `/etc/hosts`、`/typo`）→ 也放行进 LLM。
    ///   RClaw 常处理文件路径 / shell 片段，未知名若一律报错会误伤用户想发给模型的内容；
    ///   因此只有明确命中的命令才拦截，用 `/help` 发现可用命令。
    pub fn try_handle(&self, content: &str) -> CommandOutcome {
        let trimmed = content.trim();
        let Some(rest) = trimmed.strip_prefix('/') else {
            return CommandOutcome::NotACommand;
        };
        let rest = rest.trim_start();
        // 拆出命令名（首个空白前）与剩余参数。
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (rest, ""),
        };
        match name.to_ascii_lowercase().as_str() {
            // 旧命令已移除：不做任何配置修改，只引导到 UI 的模型选择器。
            "llmodel" => CommandOutcome::Handled(
                "`/llmodel` 已移除：请用输入框底部的「模型」按钮切换模型与思考强度。".into(),
            ),
            "search" => CommandOutcome::Handled(self.handle_search(args)),
            "searchfallback" => CommandOutcome::Handled(self.handle_searchfallback(args)),
            "help" => CommandOutcome::Handled(self.help_text()),
            _ => CommandOutcome::NotACommand,
        }
    }

    /// 模型选择器状态（单独读取；UI 一般用 `llm_state` 拿同一份快照）。
    pub fn model_state(&self) -> crate::thinking::ModelState {
        match self.provider.current_config() {
            Ok(config) => self.model_state_of(&config),
            Err(_) => crate::thinking::ModelState {
                supported: false,
                provider: String::new(),
                model: String::new(),
                models: Vec::new(),
            },
        }
    }

    /// 切换模型（会话页模型选择器）：整组参数联动 + 凭据同源校验 + 先写 TOML 后发布。
    ///
    /// 与 `set_thinking` 共用 `config_mutation` 锁；`expected_*` 用于拒绝"菜单打开后
    /// 模型已被另一窗口/会话切走"的过期提交。
    pub fn set_model(
        &self,
        model: &str,
        expected_provider: Option<&str>,
        expected_model: Option<&str>,
    ) -> Result<crate::thinking::LlmState, String> {
        self.set_model_at(model, expected_provider, expected_model, None)
    }

    /// 同 [`Self::set_model`]，额外校验前端看到的 revision（拒绝过期操作）。
    pub fn set_model_at(
        &self,
        model: &str,
        expected_provider: Option<&str>,
        expected_model: Option<&str>,
        expected_revision: Option<u64>,
    ) -> Result<crate::thinking::LlmState, String> {
        let _mutation = self
            .config_mutation
            .lock()
            .map_err(|_| "配置修改锁已损坏，未切换模型。".to_string())?;
        if let Some(expected) = expected_revision {
            if expected != self.state_revision.load(Ordering::SeqCst) {
                return Err("状态已更新，请刷新后重试".into());
            }
        }
        let Some(profile) = resolve_model_spec(model) else {
            let supported = MODEL_PROFILES
                .iter()
                .map(|profile| profile.model)
                .collect::<Vec<_>>()
                .join(" / ");
            return Err(format!(
                "不支持的模型「{model}」。可选：{supported}；其它模型请改 rclaw.toml 后重启。"
            ));
        };
        let current_cfg = self
            .provider
            .current_config()
            .map_err(|e| format!("切换模型失败（未改动）：读取当前 provider 失败：{e}"))?;
        if let Some(expected) = expected_provider {
            if !expected.is_empty() && expected != current_cfg.name {
                return Err("模型已切换，请刷新后重试".into());
            }
        }
        if let Some(expected) = expected_model {
            if !expected.is_empty() && expected != current_cfg.default_model {
                return Err("模型已切换，请刷新后重试".into());
            }
        }

        // 同源/凭据决策必须以「将要落盘并生效的磁盘条目」为准，而不是 live 快照：
        // 配置面板可能刚把条目改成代理端点/换了 key。先取 TOML 文本做 CAS 快照，
        // 再从磁盘构造目标 provider；写盘时文本不一致就整笔拒绝。
        let snapshot = std::fs::read_to_string(&self.config_path).map_err(|error| {
            format!(
                "读取 rclaw.toml 失败（{}）：{error}",
                self.config_path.display()
            )
        })?;
        if let Some(selected) = rclaw_store::toml_file::RclawToml::new(&self.config_path)
            .selected_provider_in_text(&snapshot)
            .map_err(|error| format!("切换模型失败（未改动）：{error}"))?
        {
            if selected != current_cfg.name {
                return Err(
                    "配置已被其他窗口修改（选中的 provider 已变化），请重启后再切换".into(),
                );
            }
        }
        let target_flavor = profile_flavor(profile);
        let target_provider = if target_flavor != ProviderFlavor::Generic
            && current_cfg.flavor == target_flavor
            && same_origin(&current_cfg.base_url, profile.base_url)
        {
            current_cfg.name.clone()
        } else {
            profile.provider.to_string()
        };
        let Some(factory) = self.provider_factory.as_ref() else {
            return Err(format!(
                "切换模型失败（未改动）：配置事务未启用，无法切到「{target_provider}」"
            ));
        };
        let profile_override = ProviderProfileOverride {
            model: profile.model,
            media_wire: profile.media_wire,
            thinking: profile.thinking,
            reasoning_effort: profile.reasoning_effort,
            reasoning_field: profile.reasoning_field,
            max_tokens_key: profile.max_tokens_key,
            max_tokens: profile.max_tokens,
        };
        let mut pcfg =
            factory(&target_provider, &snapshot, Some(&profile_override)).map_err(|e| {
                format!("切换模型失败（未改动）：provider「{target_provider}」不可用：{e}")
            })?;
        // 目标条目自己的端点必须与 profile 官方端点同源；否则它的 key 会被发到
        // 另一个来源。代理用户应新建指向官方端点的条目，或手改 rclaw.toml。
        if !same_origin(&pcfg.base_url, profile.base_url) {
            return Err(format!(
                "切换模型失败（未改动）：provider「{target_provider}」的端点与官方端点不同源，                 拒绝把该条目的 key 发往官方端点。请新建一个指向官方端点的 provider 条目。"
            ));
        }
        // 官方 profile 只接受 Bearer；自定义头可能泄露给官方端点。
        if !matches!(pcfg.auth, rclaw_llm::AuthScheme::Bearer) {
            return Err(format!(
                "切换模型失败（未改动）：provider「{target_provider}」使用自定义鉴权头，\
                 官方 profile 只接受 bearer 鉴权"
            ));
        }
        let select_provider = target_provider != current_cfg.name;

        // 工厂已按 profile 覆盖 model/media_wire/thinking/effort；这里补齐只影响
        // live 请求与写盘的其余字段。flavor 保留工厂按显式覆盖/域名解析的结果：
        // 不能按 host 重算，否则「官方域名 + 显式 generic/zhipu」会在热切换后与
        // 重启结果不一致。
        pcfg.base_url = profile.base_url.to_string();

        // 先验证新配置可构造，再写盘；写盘成功后才发布 live。任何一步失败都保持原样，
        // 不出现「运行时已切换、重启却回退」的半套状态。
        let built: Arc<dyn rclaw_llm::Provider> = Arc::from(
            rclaw_llm::build_provider(pcfg.clone())
                .map_err(|e| format!("切换模型失败（未改动）：{e}"))?,
        );
        let params = rclaw_store::toml_file::ProviderParamUpdate {
            model: profile.model,
            base_url: profile.base_url,
            context_length: profile.context_length,
            max_tokens: profile.max_tokens,
            thinking: profile.thinking,
            reasoning_effort: profile.reasoning_effort,
            media_wire: profile.media_wire,
            reasoning_field: profile.reasoning_field,
            max_tokens_key: profile.max_tokens_key,
        };
        rclaw_store::toml_file::RclawToml::new(&self.config_path)
            .apply_model_switch(&target_provider, &params, select_provider, Some(&snapshot))
            .map_err(|e| format!("切换模型失败（未改动）：{e}"))?;
        if select_provider {
            if let Ok(mut guard) = self.selected_provider.lock() {
                *guard = target_provider.clone();
            }
        }
        // 预构建成功且写盘成功后才发布；发布不可失败（锁中毒也整体替换）。
        self.provider.switch_provider_built(pcfg.clone(), built);
        let revision = self.state_revision.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(crate::thinking::LlmState {
            revision,
            model: self.model_state_of(&pcfg),
            thinking: crate::thinking::state_from_config(&pcfg),
        })
    }

    /// `/search` 显示状态；`/search <provider>` 把方式移到首位；
    /// `/search dis|en <provider>` 持久禁用/启用。禁用不改变顺序，重新启用后回到原位。
    fn handle_search(&self, args: &str) -> String {
        let Some(control) = self.web_search_control.as_ref() else {
            return "web_search 未启用，无法调整搜索优先级。".to_string();
        };
        let tokens = args.split_whitespace().collect::<Vec<_>>();
        if tokens.is_empty() || tokens[0].eq_ignore_ascii_case("status") {
            return self.search_status(control);
        }

        if matches!(tokens[0].to_ascii_lowercase().as_str(), "dis" | "en") {
            if tokens.len() != 2 {
                return "用法：/search dis|en <zhipu|tavily|deepseek|doubao-custom|doubao-global>"
                    .to_string();
            }
            let Some(id) = BackendId::parse(tokens[1]) else {
                return search_unknown_provider(tokens[1]);
            };
            if !control.is_configured(id) {
                return format!("搜索方式 {id} 未配置");
            }
            let enabled = tokens[0].eq_ignore_ascii_case("en");
            let _mutation = match self.config_mutation.lock() {
                Ok(guard) => guard,
                Err(_) => return "配置修改锁已损坏，未修改搜索方式。".into(),
            };
            let id_table = match id {
                BackendId::Zhipu => "zhipu",
                BackendId::Tavily => "tavily",
                BackendId::DeepSeek => "deepseek",
                BackendId::DoubaoCustom => "doubao_custom",
                BackendId::DoubaoGlobal => "doubao_global",
            };
            if let Err(error) = rclaw_store::toml_file::RclawToml::new(&self.config_path)
                .set_search_provider_enabled(id_table, enabled)
            {
                return format!("写入 rclaw.toml 失败，运行时状态未改动：{error}");
            }
            // 已在锁内确认 provider 配置存在，此处不会失败。
            control
                .set_enabled(id, enabled)
                .expect("已配置的 web_search provider 必须可修改启用状态");
            tracing::info!(
                provider = id.as_str(),
                enabled,
                "用户通过 /search 修改搜索方式启用状态"
            );
            return format!(
                "✅ {} 已{}。\n\n{}",
                id.label(),
                if enabled {
                    "启用并清除阻塞状态"
                } else {
                    "禁用"
                },
                self.search_status(control)
            );
        }

        if tokens.len() != 1 {
            return "用法：/search <zhipu|tavily|deepseek|doubao-custom|doubao-global>".into();
        }
        let Some(id) = BackendId::parse(tokens[0]) else {
            return search_unknown_provider(tokens[0]);
        };
        if !control.is_configured(id) {
            return format!("搜索方式 {id} 未配置");
        }
        let _mutation = match self.config_mutation.lock() {
            Ok(guard) => guard,
            Err(_) => return "配置修改锁已损坏，未调整搜索顺序。".into(),
        };
        let mut order = control.order();
        order.retain(|item| *item != id);
        order.insert(0, id);
        let values = order.iter().map(|id| id.as_str()).collect::<Vec<_>>();
        if let Err(error) =
            rclaw_store::toml_file::RclawToml::new(&self.config_path).set_search_order(&values)
        {
            return format!("写入 rclaw.toml 失败，运行时顺序未改动：{error}");
        }
        control
            .move_to_front(id)
            .expect("已配置的 web_search provider 必须可调整顺序");
        tracing::info!(
            provider = id.as_str(),
            order = ?order.iter().map(|item| item.as_str()).collect::<Vec<_>>(),
            "用户通过 /search 切换主搜索方式"
        );
        format!(
            "✅ 已将 {} 设为首选搜索方式。\n\n{}",
            id.label(),
            self.search_status(control)
        )
    }

    /// `/searchfallback <on|off>`：开关 web_search 的主→辅 fallback。无参回显当前状态。
    /// 关闭后 web_search 只用当前主搜索源（`/search` 设定的那个），主路径失败/不足时
    /// 也不会回退到另一个 provider。web_search 未启用时提示不可用。
    fn handle_searchfallback(&self, args: &str) -> String {
        let Some(control) = self.web_search_control.as_ref() else {
            return "web_search 未启用，无法调整 fallback。".to_string();
        };
        let input = args.split_whitespace().next().unwrap_or("");
        if input.is_empty() {
            return format!(
                "web_search fallback 当前：{}\n\n用法：/searchfallback <on|off>",
                self.search_fallback_status(control)
            );
        }
        let enabled = match input.to_ascii_lowercase().as_str() {
            "on" | "true" | "1" | "开" | "开启" => true,
            "off" | "false" | "0" | "关" | "关闭" => false,
            _ => {
                return format!(
                    "未知参数「{input}」。支持：on / off\n\n用法：/searchfallback <on|off>"
                );
            }
        };
        let _mutation = match self.config_mutation.lock() {
            Ok(guard) => guard,
            Err(_) => return "配置修改锁已损坏，未修改 fallback。".into(),
        };
        if let Err(error) =
            rclaw_store::toml_file::RclawToml::new(&self.config_path).set_search_fallback(enabled)
        {
            return format!("写入 rclaw.toml 失败，运行时 fallback 未改动：{error}");
        }
        control.set_fallback_enabled(enabled);
        tracing::info!(enabled, "用户通过 /searchfallback 修改 fallback 状态");
        format!(
            "✅ web_search fallback 已{}",
            self.search_fallback_status(control)
        )
    }

    /// `/help`：列出可用命令。`/search` 行附带当前首选搜索方式与 fallback 开关状态，
    /// 让用户一眼看出当前主搜索方式及自动降级是否开启。
    ///
    /// 聊天窗按 Markdown 渲染（桌面 / APK 的 `.md-body`），段落内的单换行会被折叠成空格，
    /// 所以每条命令都写成列表项：Markdown 下逐行成条，纯文本通道（微信 / QQ）也照旧逐行可读。
    fn help_text(&self) -> String {
        let search_status = match self.web_search_control.as_ref() {
            None => "（web_search 未启用）".to_string(),
            Some(control) => {
                let fallback = if control.fallback_enabled() {
                    "开启"
                } else {
                    "关闭"
                };
                match control.statuses().into_iter().find(|status| status.primary) {
                    Some(status) => {
                        format!("（当前首选 {}，fallback {fallback}）", status.id.as_str())
                    }
                    None => format!("（当前没有可用搜索源，fallback {fallback}）"),
                }
            }
        };
        format!(
            "可用斜杠命令：\n\
             \n\
             - /search [方式] — 显示状态或把指定方式移到首位{search_status}\n\
             - /search dis|en <方式> — 禁用/启用 zhipu、tavily、deepseek、doubao-custom、doubao-global\n\
             - /searchfallback <on|off> — 开关 web_search fallback（关闭后只用主搜索源，失败也不回退）；状态见上方 /search 行\n\
             - /skills — 显示当前支持的所有 skill 名称、所属技能包及功能\n\
             - /tools — 显示当前支持的所有工具名称及说明\n\
             - /context — 显示当前会话上下文用量（窗口/已用/剩余/使用率）\n\
             - /compact — 立即压缩当前会话历史（忽略阈值；有任务运行时拒绝）\n\
             - /rewind [回滚点 id] — 列出可回滚的文件 checkpoint，或按列表给出的回滚点 id 回滚文件（hash 校验，外部改动整组拒绝）\n\
             - /fork [消息 id] — 列出可作锚点的用户消息，或从该消息分叉出新会话（并自动把锚点之后的文件改动回滚到该时刻；full / files 旗标已移除）\n\
             - /goal [目标文本|clear] — 设定并立即开始执行 / 查看 / 清除本会话的长期目标（设定后直接作为本轮任务开工；每轮注入提醒，成功收尾后小审计确认达成即自动清除）\n\
             - /export [all] — 把当前会话（或全部会话）的完整记录导出为 JSONL（agent/exports/，含已压缩的消息）\n\
             - /clear [confirm] — 清空全部会话状态（agent/state.sqlite）、会话 artifact 落盘目录并删除 agent/memory 全部文件（需 /clear confirm；设备在同步空间中时，即使同步已关闭，也要用 /clear confirm local 或 /clear confirm all）\n\
             - /sync [status|now|pause|resume|show|accept|reject] — 查看多端同步状态、立即同步、暂停 / 恢复、处理待确认的改动\n\
             - /log [子命令] — 查看 / 发送 / 删除 agent/logs 日志文件（list / get / del）\n\
             - /workflow <请求> — 设计并运行一个动态工作流（展开为 prompt，历史保留原文）\n\
             - /cancel — 停止当前会话正在运行的任务（路由层即时生效，不进入 LLM）\n\
             - /help — 显示本帮助\n\
             \n已知模型：glm-5.3、glm-5.3-flash"
        )
    }

    fn search_fallback_status(&self, control: &WebSearchControl) -> String {
        if !control.fallback_enabled() {
            return "关闭（只用主搜索源）".to_string();
        }
        "开启".to_string()
    }

    fn search_status(&self, control: &WebSearchControl) -> String {
        let statuses = control.statuses();
        let mut output = format!(
            "web_search 顺序与状态（fallback {}）：",
            self.search_fallback_status(control)
        );
        for (index, status) in statuses.iter().enumerate() {
            let enabled = if status.enabled {
                "enabled"
            } else {
                "disabled"
            };
            let primary = if status.primary { " / primary" } else { "" };
            output.push_str(&format!(
                "\n{}. {}  {} / {}{}",
                index + 1,
                status.id.as_str(),
                enabled,
                status.health.label(),
                primary
            ));
        }
        output.push_str("\n\n用法：/search <方式>；/search dis|en <方式>；/searchfallback on|off");
        output
    }
}

fn search_unknown_provider(input: &str) -> String {
    format!(
        "未知搜索方式「{input}」。支持：zhipu / tavily / deepseek / doubao-custom / doubao-global"
    )
}

#[cfg(test)]
mod tests;
