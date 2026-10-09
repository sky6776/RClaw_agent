use super::*;
use rclaw_llm::Provider as _;

#[test]
fn resolves_known_models_case_insensitively() {
    assert_eq!(
        resolve_model_spec("glm-5.3-flash").map(|s| s.model),
        Some("glm-5.3-flash")
    );
    assert_eq!(
        resolve_model_spec(" glm-5.3-flash ").map(|s| s.base_url),
        Some("https://open.bigmodel.cn/api/coding/paas/v4")
    );
    assert_eq!(
        resolve_model_spec("glm-5.3").map(|s| s.model),
        Some("glm-5.3")
    );
    assert_eq!(
        resolve_model_spec("glm-5.3").map(|s| s.base_url),
        Some("https://open.bigmodel.cn/api/coding/paas/v4")
    );
    // context_length：token 计数用十进制（1M=1000000，非 1024 进制）。
    assert_eq!(
        resolve_model_spec("glm-5.3-flash").map(|s| s.context_length),
        Some(1_000_000)
    );
    assert_eq!(
        resolve_model_spec("glm-5.3").map(|s| s.context_length),
        Some(1_000_000)
    );
    // 大小写与首尾空白容错。
    assert_eq!(
        resolve_model_spec("  GLM-5.2  ").map(|s| s.model),
        Some("glm-5.3")
    );
    // 旧名兼容：GLM-5V-Turbo 映射到多模态接替者 glm-5.3-flash（coding 端点、1M）。
    assert_eq!(
        resolve_model_spec("GLM-5V-Turbo").map(|s| s.model),
        Some("glm-5.3-flash")
    );
    assert_eq!(
        resolve_model_spec(" glm-5v-turbo ").map(|s| s.base_url),
        Some("https://open.bigmodel.cn/api/coding/paas/v4")
    );
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
    assert!(resp.contains("/llmodel"));
    assert!(resp.contains("/skills"));
    assert!(resp.contains("/help"));
}

#[test]
fn llmodel_unknown_model_reports_available() {
    let reg = make_registry();
    let CommandOutcome::Handled(resp) = reg.try_handle("/llmodel gpt-4") else {
        panic!("应为 Handled");
    };
    assert!(resp.contains("未知模型"));
    assert!(resp.contains("glm-5.3"));
}

#[test]
fn llmodel_no_args_shows_current() {
    let reg = make_registry();
    let CommandOutcome::Handled(resp) = reg.try_handle("/llmodel") else {
        panic!("应为 Handled");
    };
    assert!(resp.contains("当前模型"));
    assert!(resp.contains("glm-5.3"));
}

#[test]
fn llmodel_known_model_switches_runtime_and_persists_config() {
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
    let reg = SlashRegistry::new(provider.clone(), path.clone(), "zhipu".into());

    let CommandOutcome::Handled(resp) = reg.try_handle("/llmodel glm-5.3-flash") else {
        panic!("应为 Handled");
    };
    assert!(resp.contains("已切换模型"));
    // 回显里带上 context_length（glm-5.3-flash = 1M）。
    assert!(resp.contains("context_length：1000000"));

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
}

// 纯函数行编辑器（replace_provider_fields / replace_web_search_* / parse_toml_key）的
// 行为已迁至 rclaw-store 的 RclawToml（toml_edit 注释保真，C6），测试见
// rclaw-store/src/toml_file.rs；此处只保留经 SlashRegistry 的持久化集成测试。

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
    use rclaw_llm::{AuthScheme, Dialect, MaxTokensKey, ProtocolKind, ReasoningField};
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
        },
        max_tokens: 4096,
        request_timeout_secs: 300,
        max_tool_calls: 128,
    }
}
