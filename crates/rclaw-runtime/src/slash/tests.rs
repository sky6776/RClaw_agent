use super::*;
use rclaw_llm::Provider as _;

/// 测试工厂按生产实现的方式应用 profile 覆盖（覆盖后再返回，供 set_model 直接发布）。
fn apply_profile(
    mut cfg: rclaw_llm::ProviderConfig,
    over: Option<&ProviderProfileOverride<'_>>,
) -> rclaw_llm::ProviderConfig {
    if let Some(over) = over {
        cfg.default_model = over.model.to_string();
        cfg.media_wire = if over.media_wire == "deepseek" {
            rclaw_llm::MediaWire::DeepSeek
        } else {
            rclaw_llm::MediaWire::OpenAiCompat
        };
        cfg.reasoning = rclaw_llm::ReasoningConfig {
            thinking: Some(over.thinking == "enabled"),
            effort: Some(over.reasoning_effort.to_string()),
        };
        cfg.max_tokens = over.max_tokens;
        cfg.dialect.reasoning_field = if over.reasoning_field == "none" {
            rclaw_llm::ReasoningField::None
        } else {
            rclaw_llm::ReasoningField::ReasoningContent
        };
        cfg.dialect.max_tokens_key = rclaw_llm::MaxTokensKey::MaxTokens;
    }
    cfg
}

#[test]
fn resolves_only_supported_models_case_insensitively() {
    let flash = resolve_model_spec(" glm-5.3-flash ").expect("glm-5.3-flash");
    assert_eq!(flash.model, "glm-5.3-flash");
    assert_eq!(flash.provider, "zhipu");
    assert_eq!(
        flash.base_url,
        "https://open.bigmodel.cn/api/coding/paas/v4"
    );
    // context_length：token 计数用十进制（1M=1000000，非 1024 进制）。
    assert_eq!(flash.context_length, 1_000_000);
    assert_eq!(flash.max_tokens, 128_000);
    assert_eq!(flash.reasoning_effort, "max");

    let flagship = resolve_model_spec("GLM-5.4").expect("glm-5.4");
    assert_eq!(flagship.model, "glm-5.4");
    assert_eq!(flagship.provider, "zhipu");
    assert_eq!(flagship.context_length, 1_000_000);

    let deepseek = resolve_model_spec("DeepSeek-Flash").expect("deepseek-flash");
    assert_eq!(deepseek.model, "deepseek-flash");
    assert_eq!(deepseek.provider, "deepseek");
    assert_eq!(deepseek.base_url, "https://api.deepseek.com/v1");
    assert_eq!(deepseek.context_length, 1_048_576);
    assert_eq!(deepseek.media_wire, "deepseek");

    // 不在白名单：模型选择器 / set_model 明确拒绝（如需使用请手改 rclaw.toml）。
    assert!(resolve_model_spec("glm-5.3").is_none());
    assert!(resolve_model_spec("glm-5.2").is_none());
    assert!(resolve_model_spec("glm-5v-turbo").is_none());
    assert!(resolve_model_spec("deepseek-v4-pro").is_none());
    assert!(resolve_model_spec("gpt-4").is_none());
}

#[test]
fn unknown_slash_passes_through_to_llm() {
    // 未知名（含路径、typo）原样放行，避免误伤发给模型的内容。
    let reg = make_registry();
    assert!(matches!(
        reg.try_handle("/nope"),
        CommandOutcome::NotACommand
    ));
    assert!(matches!(
        reg.try_handle("/etc/hosts"),
        CommandOutcome::NotACommand
    ));
}

#[test]
fn non_slash_passes_through() {
    let reg = make_registry();
    assert!(matches!(
        reg.try_handle("你好"),
        CommandOutcome::NotACommand
    ));
}

#[test]
fn help_command_lists_available_commands() {
    let reg = make_registry();
    let CommandOutcome::Handled(resp) = reg.try_handle("/help") else {
        panic!("应为 Handled");
    };
    assert!(!resp.contains("/llmodel"));
    assert!(resp.contains("/skills"));
    assert!(resp.contains("/help"));
    // 排版回归：每条命令独占一个列表项。聊天窗按 Markdown 渲染，段落内的单换行会被
    // 折成空格——写成「每行一条命令」的纯文本会在手机上糊成一整段。
    let bullets: Vec<&str> = resp.lines().filter(|line| line.starts_with("- ")).collect();
    for command in [
        "/search",
        "/searchfallback",
        "/skills",
        "/tools",
        "/context",
        "/compact",
        "/rewind",
        "/fork",
        "/goal",
        "/export",
        "/clear",
        "/log",
        "/workflow",
        "/cancel",
        "/help",
    ] {
        assert!(
            bullets.iter().any(|line| line.contains(command)),
            "{command} 必须独占一个列表项：\n{resp}"
        );
    }
    // 15 条命令 + `/search dis|en` 一行 = 16 个列表项；合并成一行会让总数变少。
    assert!(bullets.len() >= 16, "命令清单应一条一行：\n{resp}");
}

#[test]
fn llmodel_command_is_removed_and_points_to_picker() {
    let reg = make_registry();
    let CommandOutcome::Handled(resp) = reg.try_handle("/llmodel gpt-4") else {
        panic!("应为 Handled");
    };
    assert!(resp.contains("已移除"), "{resp}");
    assert!(resp.contains("模型"), "{resp}");
}

#[test]
fn model_state_lists_three_models_and_current() {
    let reg = make_registry();
    let state = reg.model_state();
    assert!(state.supported);
    assert_eq!(state.model, "glm-5.3");
    assert_eq!(state.provider, "zhipu");
    assert_eq!(state.models.len(), 3);
    assert!(state
        .models
        .iter()
        .any(|model| model.model == "glm-5.3-flash"
            && !model
                .efforts
                .iter()
                .any(|effort| effort.value == "disabled")));
    assert!(state.models.iter().any(|model| model.model == "glm-5.4"
        && !model
            .efforts
            .iter()
            .any(|effort| effort.value == "disabled")));
    assert!(state
        .models
        .iter()
        .any(|model| model.model == "deepseek-flash"
            && model
                .efforts
                .iter()
                .any(|effort| effort.value == "disabled")));
}

#[test]
fn set_model_switches_runtime_and_persists_all_params() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rclaw.toml");
    std::fs::write(
        &path,
        r#"[llm]
provider = "zhipu"

[[llm.providers]]
name = "zhipu"
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
model = "glm-5.3"
api_key = "secret"
"#,
    )
    .unwrap();

    let provider = Arc::new(SwappableProvider::new(zhipu_fixture()).expect("build swappable"));
    let factory: ProviderFactory = Arc::new(
        |name: &str, _text: &str, over: Option<&ProviderProfileOverride<'_>>| match name {
            "zhipu" => Ok(apply_profile(zhipu_fixture(), over)),
            "deepseek" => Ok(apply_profile(deepseek_fixture(), over)),
            other => Err(format!("unknown provider {other}")),
        },
    );
    let reg = SlashRegistry::new(provider.clone(), path.clone(), "zhipu".into())
        .with_provider_factory(factory);

    let state = reg.set_model("glm-5.3-flash", None, None).unwrap();
    assert_eq!(state.model.model, "glm-5.3-flash");
    assert!(state.thinking.supported);

    let (model, base_url) = provider.current_model().unwrap();
    assert_eq!(model, "glm-5.3-flash");
    assert_eq!(base_url, "https://open.bigmodel.cn/api/coding/paas/v4");
    assert!(provider.supports_multimodal());

    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains("model = \"glm-5.3-flash\""));
    assert!(text.contains("base_url = \"https://open.bigmodel.cn/api/coding/paas/v4\""));
    assert!(text.contains("api_key = \"secret\""));
    // 原 toml 无 context_length 行 → 切换时补写为 1000000。
    assert!(text.contains("context_length = 1000000"));
    // 整组参数联动：max_tokens / thinking / reasoning_effort / media_wire 一并写回。
    assert!(text.contains("max_tokens = 128000"), "{text}");
    assert!(text.contains("thinking = \"enabled\""), "{text}");
    assert!(text.contains("reasoning_effort = \"max\""), "{text}");
    assert!(text.contains("media_wire = \"openai\""), "{text}");
}

// 纯函数行编辑器（replace_provider_fields / replace_web_search_* / parse_toml_key）的
// 行为已迁至 rclaw-store 的 RclawToml（toml_edit 注释保真，C6），测试见
// rclaw-store/src/toml_file.rs；此处只保留经 SlashRegistry 的持久化集成测试。

fn deepseek_fixture() -> rclaw_llm::ProviderConfig {
    use rclaw_llm::{
        AuthScheme, Dialect, MaxTokensKey, MediaWire, ProtocolKind, ProviderFlavor,
        ReasoningConfig, ReasoningField,
    };
    use secrecy::SecretString;

    rclaw_llm::ProviderConfig {
        name: "deepseek".into(),
        base_url: "https://api.deepseek.com/v1".into(),
        auth: AuthScheme::Bearer,
        api_key: SecretString::from(String::from("deepseek-secret")),
        default_model: "deepseek-v4-pro".into(),
        protocol: ProtocolKind::OpenAiCompat,
        dialect: Dialect {
            reasoning_field: ReasoningField::ReasoningContent,
            supports_tool_choice: true,
            max_tokens_key: MaxTokensKey::MaxTokens,
            classify_upstream_busy: true,
        },
        flavor: ProviderFlavor::DeepSeek,
        media_wire: MediaWire::DeepSeek,
        reasoning: ReasoningConfig::default(),
        max_tokens: 4096,
        request_timeout_secs: 300,
        max_tool_calls: 128,
    }
}

#[test]
fn llmodel_switches_across_provider_with_all_params() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rclaw.toml");
    std::fs::write(
        &path,
        r#"[llm]
provider = "zhipu"

[[llm.providers]]
name = "zhipu"
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
model = "glm-5.3-flash"
api_key = "zhipu-secret"

[[llm.providers]]
name = "deepseek"
base_url = "https://api.deepseek.com/v1"
model = "deepseek-v4-pro"
api_key = "deepseek-secret"
context_length = 131072
"#,
    )
    .unwrap();

    let provider = Arc::new(SwappableProvider::new(zhipu_fixture()).expect("build swappable"));
    let factory: ProviderFactory = Arc::new(
        |name: &str, _text: &str, over: Option<&ProviderProfileOverride<'_>>| match name {
            "zhipu" => Ok(apply_profile(zhipu_fixture(), over)),
            "deepseek" => Ok(apply_profile(deepseek_fixture(), over)),
            other => Err(format!("unknown provider {other}")),
        },
    );
    let reg = SlashRegistry::new(provider.clone(), path.clone(), "zhipu".into())
        .with_provider_factory(factory);

    let state = reg.set_model("deepseek-flash", None, None).unwrap();
    assert_eq!(state.model.provider, "deepseek");
    assert_eq!(state.model.model, "deepseek-flash");
    assert_eq!(state.thinking.current.as_deref(), Some("high"));

    // 运行时：整组参数都换（provider / model / base_url / max_tokens / media / effort）。
    let cfg = provider.current_config().unwrap();
    assert_eq!(cfg.name, "deepseek");
    assert_eq!(cfg.default_model, "deepseek-flash");
    assert_eq!(cfg.base_url, "https://api.deepseek.com/v1");
    assert_eq!(cfg.max_tokens, 128_000);
    assert_eq!(cfg.media_wire, rclaw_llm::MediaWire::DeepSeek);
    assert_eq!(cfg.reasoning.thinking, Some(true));
    assert_eq!(cfg.reasoning.effort.as_deref(), Some("high"));
    assert_eq!(reg.selected_provider(), "deepseek");

    // 持久化：一次写盘，[llm].provider 与目标条目全字段都更新，且不碰另一个 provider。
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains("provider = \"deepseek\""), "{text}");
    assert!(text.contains("model = \"deepseek-flash\""), "{text}");
    assert!(text.contains("context_length = 1048576"), "{text}");
    assert!(text.contains("max_tokens = 128000"), "{text}");
    assert!(text.contains("media_wire = \"deepseek\""), "{text}");
    assert!(text.contains("reasoning_effort = \"high\""), "{text}");
    assert!(text.contains("api_key = \"deepseek-secret\""), "{text}");
    assert!(text.contains("api_key = \"zhipu-secret\""), "{text}");
}

#[test]
fn llmodel_refuses_when_global_model_override_present() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rclaw.toml");
    std::fs::write(
        &path,
        r#"[llm]
provider = "zhipu"
model = "glm-5.3-flash"

[[llm.providers]]
name = "zhipu"
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
model = "glm-5.3-flash"
api_key = "secret"
"#,
    )
    .unwrap();
    let provider = Arc::new(SwappableProvider::new(zhipu_fixture()).expect("build swappable"));
    let factory: ProviderFactory = Arc::new(
        |name: &str, _text: &str, over: Option<&ProviderProfileOverride<'_>>| match name {
            "zhipu" => Ok(apply_profile(zhipu_fixture(), over)),
            "deepseek" => Ok(apply_profile(deepseek_fixture(), over)),
            other => Err(format!("unknown provider {other}")),
        },
    );
    let reg = SlashRegistry::new(provider.clone(), path.clone(), "zhipu".into())
        .with_provider_factory(factory);

    let error = reg.set_model("glm-5.4", None, None).unwrap_err();
    assert!(error.contains("全局覆盖"), "{error}");
    // 写盘失败发生在发布之前：运行时与磁盘都保持原样。
    assert!(error.contains("未改动"), "{error}");
    let (model, _) = provider.current_model().unwrap();
    assert_eq!(model, "glm-5.3");
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains("model = \"glm-5.3-flash\""), "{text}");
}

#[test]
fn llmodel_refuses_to_move_proxy_credentials_to_official_origin() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rclaw.toml");
    std::fs::write(
        &path,
        r#"[llm]
provider = "zhipu"

[[llm.providers]]
name = "zhipu"
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
model = "glm-5.3-flash"
api_key = "zhipu-secret"

[[llm.providers]]
name = "deepseek"
base_url = "https://deepseek-proxy.example.com/v1"
model = "deepseek-flash"
api_key = "proxy-secret"
"#,
    )
    .unwrap();
    let provider = Arc::new(SwappableProvider::new(zhipu_fixture()).expect("build swappable"));
    let factory: ProviderFactory = Arc::new(
        |name: &str, _text: &str, over: Option<&ProviderProfileOverride<'_>>| match name {
            "zhipu" => Ok(apply_profile(zhipu_fixture(), over)),
            "deepseek" => {
                let mut cfg = deepseek_fixture();
                cfg.base_url = "https://deepseek-proxy.example.com/v1".into();
                Ok(apply_profile(cfg, over))
            }
            other => Err(format!("unknown provider {other}")),
        },
    );
    let reg = SlashRegistry::new(provider.clone(), path.clone(), "zhipu".into())
        .with_provider_factory(factory);

    let error = reg.set_model("deepseek-flash", None, None).unwrap_err();
    assert!(error.contains("不同源"), "{error}");
    assert!(error.contains("未改动"), "{error}");
    let cfg = provider.current_config().unwrap();
    assert_eq!(cfg.name, "zhipu");
    assert_eq!(cfg.default_model, "glm-5.3");
    assert_eq!(reg.selected_provider(), "zhipu");
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains("provider = \"zhipu\""), "{text}");
}

#[test]
fn search_without_control_reports_unavailable() {
    let reg = make_registry();
    let CommandOutcome::Handled(resp) = reg.try_handle("/search") else {
        panic!("应为 Handled");
    };
    assert!(resp.contains("web_search 未启用"), "{resp}");
}

#[test]
fn search_command_changes_order_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rclaw.toml");
    std::fs::write(&path, web_search_config_text()).unwrap();
    let control = search_control();
    let provider = Arc::new(SwappableProvider::new(zhipu_fixture()).expect("build swappable"));
    let reg = SlashRegistry::new(provider, path.clone(), "zhipu".into())
        .with_web_search_control(control.clone());

    let CommandOutcome::Handled(resp) = reg.try_handle("/search") else {
        panic!("应为 Handled");
    };
    assert!(resp.contains("doubao-custom"), "{resp}");
    assert!(resp.contains("primary"), "{resp}");

    let CommandOutcome::Handled(resp) = reg.try_handle("/search tavily") else {
        panic!("应为 Handled");
    };
    assert!(resp.contains("设为首选"), "{resp}");
    assert_eq!(control.order()[0], BackendId::Tavily);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("order = [\"tavily\", \"doubao-custom\", \"zhipu\", \"doubao-global\"]"));
}

#[test]
fn search_command_disables_and_reenables_provider() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rclaw.toml");
    std::fs::write(&path, web_search_config_text()).unwrap();
    let control = search_control();
    let provider = Arc::new(SwappableProvider::new(zhipu_fixture()).expect("build swappable"));
    let reg = SlashRegistry::new(provider, path.clone(), "zhipu".into())
        .with_web_search_control(control.clone());

    let CommandOutcome::Handled(resp) = reg.try_handle("/search dis doubao-custom") else {
        panic!("应为 Handled");
    };
    assert!(resp.contains("已禁用"), "{resp}");
    assert!(!control.is_enabled(BackendId::DoubaoCustom));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("[web_search.doubao_custom]\nenabled = false"));

    let CommandOutcome::Handled(resp) = reg.try_handle("/search en doubao-custom") else {
        panic!("应为 Handled");
    };
    assert!(resp.contains("已启用并清除阻塞状态"), "{resp}");
    assert!(control.is_enabled(BackendId::DoubaoCustom));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("[web_search.doubao_custom]\nenabled = true"));
}

#[test]
fn searchfallback_command_toggles_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rclaw.toml");
    std::fs::write(&path, web_search_config_text()).unwrap();
    let control = search_control();
    let provider = Arc::new(SwappableProvider::new(zhipu_fixture()).expect("build swappable"));
    let reg = SlashRegistry::new(provider, path.clone(), "zhipu".into())
        .with_web_search_control(control.clone());

    let CommandOutcome::Handled(resp) = reg.try_handle("/searchfallback") else {
        panic!("应为 Handled");
    };
    assert!(resp.contains("开启"), "{resp}");

    let CommandOutcome::Handled(resp) = reg.try_handle("/searchfallback off") else {
        panic!("应为 Handled");
    };
    assert!(resp.contains("已关闭"), "{resp}");
    assert!(!control.fallback_enabled());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("fallback = false"));

    let CommandOutcome::Handled(resp) = reg.try_handle("/searchfallback on") else {
        panic!("应为 Handled");
    };
    assert!(resp.contains("已开启"), "{resp}");
    assert!(control.fallback_enabled());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("fallback = true"));
}

#[test]
fn search_commands_do_not_change_runtime_when_persistence_fails() {
    let dir = tempfile::tempdir().unwrap();
    let missing_path = dir.path().join("missing").join("rclaw.toml");
    let control = search_control();
    let provider = Arc::new(SwappableProvider::new(zhipu_fixture()).expect("build swappable"));
    let reg = SlashRegistry::new(provider, missing_path, "zhipu".into())
        .with_web_search_control(control.clone());

    let CommandOutcome::Handled(disable) = reg.try_handle("/search dis doubao-custom") else {
        panic!("应为 Handled");
    };
    assert!(disable.contains("运行时状态未改动"), "{disable}");
    assert!(control.is_enabled(BackendId::DoubaoCustom));

    let CommandOutcome::Handled(reorder) = reg.try_handle("/search tavily") else {
        panic!("应为 Handled");
    };
    assert!(reorder.contains("运行时顺序未改动"), "{reorder}");
    assert_eq!(control.order()[0], BackendId::DoubaoCustom);

    let CommandOutcome::Handled(fallback) = reg.try_handle("/searchfallback off") else {
        panic!("应为 Handled");
    };
    assert!(fallback.contains("运行时 fallback 未改动"), "{fallback}");
    assert!(control.fallback_enabled());
}

fn search_control() -> Arc<WebSearchControl> {
    Arc::new(
        WebSearchControl::new(
            vec![
                BackendId::DoubaoCustom,
                BackendId::Tavily,
                BackendId::Zhipu,
                BackendId::DoubaoGlobal,
            ],
            &[
                (BackendId::DoubaoCustom, true),
                (BackendId::Tavily, true),
                (BackendId::Zhipu, true),
                (BackendId::DoubaoGlobal, true),
            ],
            true,
            3,
            std::time::Duration::from_secs(60),
        )
        .unwrap(),
    )
}

fn web_search_config_text() -> &'static str {
    "[web_search]\n\
enabled = true\n\
order = [\"doubao-custom\", \"tavily\", \"zhipu\", \"doubao-global\"]\n\
fallback = true\n\
\n\
[web_search.doubao_custom]\n\
enabled = true\n\
api_key = \"custom-test\"\n\
\n\
[web_search.tavily]\n\
enabled = true\n\
api_keys = [\"tavily-test\"]\n\
\n\
[web_search.zhipu]\n\
enabled = true\n\
api_key = \"zhipu-test\"\n\
\n\
[web_search.doubao_global]\n\
enabled = true\n\
api_key = \"global-test\"\n"
}

// 构造一个 registry 用于命令派发测试（provider 句柄用真实 SwappableProvider，
// 但这些用例不真正触发网络调用——只测派发与文本生成逻辑）。
fn make_registry() -> SlashRegistry {
    let provider = Arc::new(SwappableProvider::new(zhipu_fixture()).expect("build swappable"));
    SlashRegistry::new(provider, PathBuf::from("rclaw.toml"), "zhipu".into())
}

// 测试夹具：直接构造一份合法的 ProviderConfig（避免依赖 rclaw-cli 的 Config）。
fn zhipu_fixture() -> rclaw_llm::ProviderConfig {
    use rclaw_llm::{
        AuthScheme, Dialect, MaxTokensKey, MediaWire, ProtocolKind, ProviderFlavor,
        ReasoningConfig, ReasoningField,
    };
    use secrecy::SecretString;

    rclaw_llm::ProviderConfig {
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

// ---------------------------------------------------------------------------
// 思考强度（与 /llmodel 同锁的配置事务）
// ---------------------------------------------------------------------------

fn thinking_fixture(model: &str) -> rclaw_llm::ProviderConfig {
    let mut cfg = zhipu_fixture();
    cfg.default_model = model.into();
    cfg
}

fn thinking_config(path: &std::path::Path, provider: &str, model: &str) {
    std::fs::write(
        path,
        format!(
            r#"[llm]
provider = "{provider}"

[[llm.providers]]
name = "{provider}"
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
model = "{model}"
api_key = "secret"
"#
        ),
    )
    .unwrap();
}

#[test]
fn set_thinking_persists_level_and_switches_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rclaw.toml");
    thinking_config(&path, "zhipu", "glm-5.3-flash");
    let provider = Arc::new(
        SwappableProvider::new(thinking_fixture("glm-5.3-flash")).expect("build swappable"),
    );
    let reg = SlashRegistry::new(provider.clone(), path.clone(), "zhipu".into());

    let state = reg
        .set_thinking("max", Some("zhipu"), Some("glm-5.3-flash"))
        .unwrap();
    assert_eq!(state.thinking.current.as_deref(), Some("max"));
    assert_eq!(state.model.model, "glm-5.3-flash");
    let cfg = provider.current_config().unwrap();
    assert_eq!(cfg.reasoning.thinking, Some(true));
    assert_eq!(cfg.reasoning.effort.as_deref(), Some("max"));
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains("thinking = \"enabled\""), "{text}");
    assert!(text.contains("reasoning_effort = \"max\""), "{text}");
}

#[test]
fn set_thinking_disabled_is_deepseek_only_and_clears_effort() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rclaw.toml");
    std::fs::write(
        &path,
        r#"[llm]
provider = "deepseek"

[[llm.providers]]
name = "deepseek"
base_url = "https://api.deepseek.com/v1"
model = "deepseek-flash"
api_key = "secret"
"#,
    )
    .unwrap();
    let provider = Arc::new(SwappableProvider::new(deepseek_fixture()).expect("build swappable"));
    let reg = SlashRegistry::new(provider.clone(), path.clone(), "deepseek".into());

    // 先写入 effort=max，再关闭：必须证明已有 effort 被删除（而不是原本就没有）。
    let state = reg
        .set_thinking("max", Some("deepseek"), Some("deepseek-v4-pro"))
        .unwrap();
    assert_eq!(state.thinking.current.as_deref(), Some("max"));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("reasoning_effort = \"max\""), "{text}");

    let state = reg
        .set_thinking("disabled", Some("deepseek"), Some("deepseek-v4-pro"))
        .unwrap();
    assert_eq!(state.thinking.current.as_deref(), Some("disabled"));
    let cfg = provider.current_config().unwrap();
    assert_eq!(cfg.reasoning.thinking, Some(false));
    assert!(cfg.reasoning.effort.is_none());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("thinking = \"disabled\""), "{text}");
    assert!(!text.contains("reasoning_effort"), "{text}");

    // 再开回 high：effort 重新写入。
    let state = reg
        .set_thinking("high", Some("deepseek"), Some("deepseek-v4-pro"))
        .unwrap();
    assert_eq!(state.thinking.current.as_deref(), Some("high"));
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains("thinking = \"enabled\""), "{text}");
    assert!(text.contains("reasoning_effort = \"high\""), "{text}");
}

#[test]
fn set_thinking_rejects_stale_expected_model() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rclaw.toml");
    thinking_config(&path, "zhipu", "glm-5.3-flash");
    let provider = Arc::new(
        SwappableProvider::new(thinking_fixture("glm-5.3-flash")).expect("build swappable"),
    );
    let reg = SlashRegistry::new(provider.clone(), path.clone(), "zhipu".into());

    let error = reg
        .set_thinking("max", Some("zhipu"), Some("glm-5.4"))
        .unwrap_err();
    assert!(error.contains("模型已切换"), "{error}");
    assert_eq!(
        provider.current_config().unwrap().reasoning.effort,
        None,
        "过期提交不得改动运行时"
    );
}

#[test]
fn same_origin_uses_parsed_url_and_effective_port() {
    assert!(same_origin(
        "https://api.deepseek.com/v1",
        "https://api.deepseek.com:443/other"
    ));
    assert!(same_origin(
        "http://api.example.com/a",
        "http://api.example.com:80/b"
    ));
    assert!(!same_origin(
        "https://api.deepseek.com/v1",
        "https://api.deepseek.com.evil/v1"
    ));
    assert!(!same_origin(
        "http://api.deepseek.com/v1",
        "https://api.deepseek.com/v1"
    ));
    assert!(!same_origin("not a url", "https://api.deepseek.com/v1"));
}

#[test]
fn stale_revision_is_rejected_under_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rclaw.toml");
    thinking_config(&path, "zhipu", "glm-5.3-flash");
    let provider = Arc::new(
        SwappableProvider::new(thinking_fixture("glm-5.3-flash")).expect("build swappable"),
    );
    let reg = SlashRegistry::new(provider, path, "zhipu".into());

    let first = reg
        .set_thinking_at("high", None, None, Some(1))
        .expect("初始 revision 应通过");
    assert!(first.revision > 1);
    let error = reg
        .set_thinking_at("max", None, None, Some(1))
        .expect_err("过期 revision 必须拒绝");
    assert!(error.contains("状态已更新"), "{error}");
}

#[test]
fn set_thinking_rejects_disabled_for_glm_flash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rclaw.toml");
    thinking_config(&path, "zhipu", "glm-5.3-flash");
    let provider = Arc::new(
        SwappableProvider::new(thinking_fixture("glm-5.3-flash")).expect("build swappable"),
    );
    let reg = SlashRegistry::new(provider, path, "zhipu".into());
    let error = reg.set_thinking("disabled", None, None).unwrap_err();
    assert!(error.contains("不支持"), "{error}");
}
