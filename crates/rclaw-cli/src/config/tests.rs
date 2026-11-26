use super::*;

/// 回归保护：仓库根的 `rclaw.toml.example` 必须始终能被 [`Config`] 解析，
/// 且各通道段（如新增的 napcat）字段名 / 类型与结构体一致。这能避免「示例配置与代码漂移」。
#[test]
fn example_config_parses() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("rclaw.toml.example");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读取 {} 失败: {e}", path.display()));
    let cfg: Config = toml::from_str(&text).expect("rclaw.toml.example 必须可被 Config 解析");
    // napcat 段存在且示例默认未启用，ws_url 为示例值。
    assert_eq!(cfg.napcat.enabled, Some(false));
    assert_eq!(cfg.napcat.ws_url.as_deref(), Some("ws://127.0.0.1:3001"));
    // 示例显式配置的群响应模式是 at_only（群内仅@机器人时响应）。
    assert_eq!(cfg.napcat.group_response_mode, "at_only");
    assert_eq!(
        cfg.napcat.send_chunk_bytes,
        NapCatConfig::default().send_chunk_bytes
    );
    // 示例显式给出的压缩摘要上限必须是 12_000（不是旧的 2048）。
    assert_eq!(cfg.agent.compression_summary_max_tokens, 12_000);
    // 示例显式给出动态工作流启用方式默认值 always。
    assert_eq!(cfg.agent.workflow_activation, WorkflowActivation::Always);
}

/// `[agent] workflow_activation`：缺省 always；`"command"` 可解析；非法取值在解析期拒绝
/// （配置面板保存前走 `Config::parse_str`，错误信息要能说明合法取值）。
#[test]
fn workflow_activation_parses_defaults_and_rejects_unknown() {
    let default_cfg: Config = toml::from_str("").unwrap();
    assert_eq!(
        default_cfg.agent.workflow_activation,
        WorkflowActivation::Always
    );
    assert_eq!(
        AgentSection::default().workflow_activation,
        WorkflowActivation::Always
    );

    let command: Config = toml::from_str("[agent]\nworkflow_activation = \"command\"\n").unwrap();
    assert_eq!(
        command.agent.workflow_activation,
        WorkflowActivation::Command
    );
    assert_eq!(command.agent.workflow_activation.as_str(), "command");

    // `{:#}` 展示 anyhow 链：外层「配置解析失败」+ 里层 toml 的 `unknown variant … expected …`。
    let error = Config::parse_str("[agent]\nworkflow_activation = \"sometimes\"\n").unwrap_err();
    let error = format!("{error:#}");
    assert!(
        error.contains("command") && error.contains("always"),
        "错误信息应列出合法取值: {error}"
    );
}

/// 配置面板的下拉选项表（`rclaw_store::config_doc::options_of`，store 不能依赖 runtime 所以
/// 手工维护）必须与 `WorkflowActivation` 的变体一一对应，且每个选项都能解析回同一枚举值。
#[test]
fn config_panel_options_track_workflow_activation_variants() {
    let doc = rclaw_store::config_doc::parse_example(rclaw_store::config_doc::EXAMPLE);
    let item = doc
        .sections
        .iter()
        .find(|s| s.path == "agent")
        .and_then(|s| s.items.iter().find(|i| i.key == "workflow_activation"))
        .expect("面板应收录 agent.workflow_activation");
    let mut options = item.options.clone();
    options.sort();
    let mut variants: Vec<String> = WorkflowActivation::ALL
        .iter()
        .map(|v| v.as_str().to_string())
        .collect();
    variants.sort();
    assert_eq!(options, variants, "面板选项与枚举变体漂移");
    for variant in WorkflowActivation::ALL {
        let cfg: Config = toml::from_str(&format!(
            "[agent]\nworkflow_activation = \"{}\"\n",
            variant.as_str()
        ))
        .unwrap();
        assert_eq!(cfg.agent.workflow_activation, *variant);
    }
}

#[test]
fn llm_request_timeout_defaults_to_300_and_is_configurable() {
    let default_cfg: Config = toml::from_str("").unwrap();
    assert_eq!(default_cfg.llm.request_timeout_secs, 300);
    assert_eq!(default_cfg.llm.max_tool_calls, 256);
    // 评审 fork 迭代上限收敛为 32（旧 256 过大，Hermes 同款成本收敛）。
    assert_eq!(default_cfg.agent.auto_review_max_iters, 32);
    assert_eq!(default_cfg.agent.auto_review_max_input_tokens, 600_000);

    let configured: Config = toml::from_str("[llm]\nrequest_timeout_secs = 420\n").unwrap();
    assert_eq!(configured.llm.request_timeout_secs, 420);
}

#[test]
fn skill_curator_archive_threshold_must_not_precede_stale_threshold() {
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.agent.skill_stale_after_days = 30;
    cfg.agent.skill_archive_after_days = 29;
    let error = cfg.validate_skill_curator().unwrap_err().to_string();
    assert!(error.contains("skill_archive_after_days"), "{error}");
}

#[test]
fn napcat_max_from_is_parsed_and_forwarded() {
    let cfg: Config = toml::from_str("[napcat]\nmax_from = \"123, *\"\n").unwrap();
    assert_eq!(cfg.napcat.max_from.as_deref(), Some("123, *"));
    assert_eq!(cfg.napcat_channel_config().max_from, "123, *");
}

#[test]
fn napcat_send_chunk_bytes_is_forwarded_and_normalized() {
    let configured: Config = toml::from_str("[napcat]\nsend_chunk_bytes = 8192\n").unwrap();
    assert_eq!(configured.napcat_channel_config().send_chunk_bytes, 8192);

    let zero: Config = toml::from_str("[napcat]\nsend_chunk_bytes = 0\n").unwrap();
    assert_eq!(
        zero.napcat_channel_config().send_chunk_bytes,
        default_napcat_send_chunk_bytes()
    );
}

/// 回归保护：压缩摘要上限默认必须是 12_000，
/// 不能回退到旧的 2048。CLI 默认路径下 `summary_max_tokens` 把它作为绝对 ceiling，2048 会
/// 过早截断大上下文摘要并丢失细节。
#[test]
fn compression_summary_max_tokens_default_is_12000() {
    // manual Default impl 路径。
    assert_eq!(
        AgentSection::default().compression_summary_max_tokens,
        12_000,
    );
    // serde default 路径（TOML 未写该键）。
    let section: AgentSection =
        toml::from_str("").expect("空 [agent] 应可解析（全 serde default）");
    assert_eq!(section.compression_summary_max_tokens, 12_000);
}

#[test]
fn max_concurrent_turns_defaults_to_eight_and_parses_override() {
    let default_cfg: Config = toml::from_str("").unwrap();
    assert_eq!(default_cfg.agent.max_concurrent_turns, 8);

    let configured: Config = toml::from_str("[agent]\nmax_concurrent_turns = 7\n").unwrap();
    assert_eq!(configured.agent.max_concurrent_turns, 7);
}

#[test]
fn goal_audit_and_artifact_keys_default_and_parse() {
    // 第十二轮新增的运行时开关必须能从配置改（AgentOptions 有字段但配置没键 = 永远默认）。
    let default_cfg: Config = toml::from_str("").unwrap();
    assert!(default_cfg.agent.goal_audit_enabled);
    assert!(default_cfg.agent.tool_result_artifact_enabled);
    assert_eq!(default_cfg.agent.tool_result_artifact_bytes, 32 * 1024);

    let configured: Config = toml::from_str(
        "[agent]\ngoal_audit_enabled = false\ntool_result_artifact_enabled = false\ntool_result_artifact_bytes = 4096\n",
    )
    .unwrap();
    assert!(!configured.agent.goal_audit_enabled);
    assert!(!configured.agent.tool_result_artifact_enabled);
    assert_eq!(configured.agent.tool_result_artifact_bytes, 4096);
}

#[test]
fn compression_token_budgets_default_to_derived_sentinels() {
    // 0 在 CLI 配置层表示「未显式配置」，main.rs 会用 provider context_length
    // 派生 threshold/tail 预算。
    let cfg: Config = toml::from_str("").unwrap();
    assert_eq!(cfg.agent.compression_threshold_tokens, 0);
    assert_eq!(cfg.agent.compression_tail_token_budget, 0);
}

#[test]
fn mcp_section_parses_from_toml() {
    let toml = r#"
[mcp]
startup_timeout_secs = 15
call_timeout_secs = 45

[mcp.servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.servers.filesystem.env]
FOO = "bar"
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.mcp.startup_timeout_secs, 15);
    assert_eq!(cfg.mcp.call_timeout_secs, 45);
    let fs = cfg
        .mcp
        .servers
        .get("filesystem")
        .expect("filesystem server");
    assert_eq!(fs.command.as_deref(), Some("npx"));
    assert_eq!(
        fs.args,
        vec!["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
    );
    assert_eq!(fs.env.get("FOO").unwrap(), "bar");
    assert!(fs.enabled, "未显式 enabled 应默认 true");
}

#[test]
fn missing_mcp_section_defaults_to_empty() {
    // 完全没有 [mcp] 段：解析成功且 servers 为空（向后兼容已有 rclaw.toml）。
    let cfg: Config = toml::from_str("").unwrap();
    assert!(cfg.mcp.servers.is_empty());
    assert_eq!(cfg.mcp.startup_timeout_secs, 20); // 全局默认
}

#[test]
fn mcp_unknown_field_is_rejected() {
    // deny_unknown_fields：拼错键名必须在启动期暴露，而非被静默忽略。
    let toml = r#"
[mcp]
startup_timeout_secs = 20
serveers = {}
"#;
    let res: Result<Config, _> = toml::from_str(toml);
    assert!(res.is_err(), "deny_unknown_fields 应拒绝未声明的 mcp 字段");
}

#[test]
fn mcp_server_unknown_field_is_rejected() {
    let toml = r#"
[mcp.servers.x]
command = "npx"
comand = "typo"
"#;
    let res: Result<Config, _> = toml::from_str(toml);
    assert!(res.is_err(), "server 定义内拼错字段应被拒绝");
}

#[test]
fn mcp_env_inline_table_parses() {
    // 对应 rclaw.toml.example「示例 2 (a)」：env 内联表写法。
    let toml = r#"
[mcp.servers.fetch]
command = "uvx"
args = ["mcp-server-fetch"]
env = { HTTP_PROXY = "http://127.0.0.1:7890", HTTPS_PROXY = "http://127.0.0.1:7890" }
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    let fetch = cfg.mcp.servers.get("fetch").unwrap();
    assert_eq!(
        fetch.env.get("HTTP_PROXY").unwrap(),
        "http://127.0.0.1:7890"
    );
    assert_eq!(
        fetch.env.get("HTTPS_PROXY").unwrap(),
        "http://127.0.0.1:7890"
    );
}

#[test]
fn mcp_env_subtable_parses() {
    // 对应 rclaw.toml.example「示例 2 (b)」：[mcp.servers.<name>.env] 子表写法。
    let toml = r#"
[mcp.servers.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
[mcp.servers.github.env]
GITHUB_PERSONAL_ACCESS_TOKEN = "ghp_xxx"
GITHUB_API_URL = "https://api.github.com"
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    let gh = cfg.mcp.servers.get("github").unwrap();
    assert_eq!(
        gh.env.get("GITHUB_PERSONAL_ACCESS_TOKEN").unwrap(),
        "ghp_xxx"
    );
    assert_eq!(
        gh.env.get("GITHUB_API_URL").unwrap(),
        "https://api.github.com"
    );
}

#[test]
fn mcp_toml_flows_into_resolved() {
    // 端到端：TOML → Config.mcp → McpResolved::build（含默认回退）。
    let toml = r#"
[mcp]
auto_start = false
[mcp.servers.git]
command = "uvx"
args = ["mcp-server-git"]
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    let resolved = rclaw_tools::mcp::config::McpResolved::build(cfg.mcp).unwrap();
    let git = resolved.servers.get("git").unwrap();
    assert_eq!(git.command.as_deref(), Some("uvx"));
    assert!(!git.auto_start, "应回退到 [mcp] 的 auto_start=false");
    assert!(git.enabled);
    assert_eq!(git.startup_timeout_secs, 20); // 全局默认
}

#[test]
fn mcp_http_section_parses_from_toml() {
    let toml = r#"
[mcp.servers.zhipu]
url = "https://open.bigmodel.cn/api/mcp/web_search_prime/mcp"
[mcp.servers.zhipu.headers]
Authorization = "Bearer xxx"
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    let z = cfg.mcp.servers.get("zhipu").unwrap();
    assert_eq!(
        z.url.as_deref(),
        Some("https://open.bigmodel.cn/api/mcp/web_search_prime/mcp")
    );
    assert_eq!(
        z.headers.as_ref().unwrap().get("Authorization").unwrap(),
        "Bearer xxx"
    );
    let resolved = rclaw_tools::mcp::config::McpResolved::build(cfg.mcp).unwrap();
    assert_eq!(
        resolved.servers.get("zhipu").unwrap().transport,
        rclaw_tools::mcp::config::TransportKind::Http
    );
}

#[test]
fn mcp_http_without_url_is_rejected() {
    let toml = r#"
[mcp.servers.bad]
transport = "http"
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert!(rclaw_tools::mcp::config::McpResolved::build(cfg.mcp).is_err());
}

// ── [log] 段 ───────────────────────────────────────────────────────────

#[test]
fn log_section_defaults_when_absent() {
    // 无 [log] 段：回退默认——写文件 info、不输出到控制台；dir 留空 → init_tracing 回退到 agent/logs。
    let cfg: Config = toml::from_str("").unwrap();
    assert_eq!(cfg.log.console_level, "info");
    assert_eq!(cfg.log.file_level, "info");
    assert!(!cfg.log.console);
    assert!(cfg.log.file);
    assert_eq!(cfg.log.dir, "");
    assert_eq!(cfg.log.file_name, "rclaw.log");
}

#[test]
fn log_section_overrides_from_toml() {
    // console 与 file 各自独立 level + 独立开关：控制台 debug、文件 trace、只控制台不文件。
    let toml = r#"
[log]
console_level = "debug"
file_level = "trace"
console = true
file = false
dir = "/var/log/rclaw"
file_name = "agent.log"
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.log.console_level, "debug");
    assert_eq!(cfg.log.file_level, "trace");
    assert!(cfg.log.console);
    assert!(!cfg.log.file);
    assert_eq!(cfg.log.dir, "/var/log/rclaw");
    assert_eq!(cfg.log.file_name, "agent.log");
}

#[test]
fn log_section_rejects_unknown_field() {
    // deny_unknown_fields：未声明键必须在启动期暴露，而非被静默忽略。
    // 旧版的共享 `level` 已被 console_level/file_level 取代——写入 `level` 同样会被拒。
    let toml = r#"
[log]
level = "info"
"#;
    let res: Result<Config, _> = toml::from_str(toml);
    assert!(
        res.is_err(),
        "deny_unknown_fields 应拒绝未声明的 [log] 字段"
    );
}

// ── [web_search] provider pool ───────────────────────────────────────

#[test]
fn web_search_defaults_to_disabled() {
    let cfg: Config = toml::from_str("").unwrap();
    assert!(!cfg.web_search.enabled);
    assert!(cfg.web_search_runtime_config().unwrap().is_none());
}

#[test]
fn web_search_provider_pool_parses() {
    let toml = r#"
[web_search]
enabled = true
order = ["doubao-custom", "tavily", "zhipu", "doubao-global"]
fallback = false
read_top = 2
max_results = 8

[web_search.doubao_custom]
api_key = "custom-test"

[web_search.tavily]
api_keys = ["tavily-a", "tavily-b"]

[web_search.zhipu]
enabled = false
api_key = "zhipu-test"

[web_search.doubao_global]
api_key = "global-test"
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    let runtime = cfg.web_search_runtime_config().unwrap().unwrap();
    assert_eq!(
        runtime.order,
        vec![
            rclaw_tools::BackendId::DoubaoCustom,
            rclaw_tools::BackendId::Tavily,
            rclaw_tools::BackendId::Zhipu,
            rclaw_tools::BackendId::DoubaoGlobal,
        ]
    );
    assert!(!runtime.fallback);
    assert_eq!(runtime.tool.read_top, 2);
    assert_eq!(runtime.tool.max_results, 8);
    assert_eq!(
        runtime.tavily.unwrap().api_keys,
        vec!["tavily-a".to_string(), "tavily-b".to_string()]
    );
    assert_eq!(
        runtime.doubao_custom.unwrap().variant,
        rclaw_tools::DoubaoVariant::Custom
    );
    assert_eq!(
        runtime.doubao_global.unwrap().variant,
        rclaw_tools::DoubaoVariant::Global
    );
    assert!(
        !runtime
            .enabled
            .iter()
            .find(|(id, _)| *id == rclaw_tools::BackendId::Zhipu)
            .unwrap()
            .1
    );
}

#[test]
fn web_search_empty_order_uses_default_configured_order() {
    let toml = r#"
[web_search]
enabled = true

[web_search.tavily]
api_keys = ["tavily-test"]

[web_search.doubao_global]
api_key = "global-test"
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    let runtime = cfg.web_search_runtime_config().unwrap().unwrap();
    assert_eq!(
        runtime.order,
        vec![
            rclaw_tools::BackendId::Tavily,
            rclaw_tools::BackendId::DoubaoGlobal,
        ]
    );
}

#[test]
fn web_search_rejects_missing_provider_and_invalid_order() {
    let cfg: Config = toml::from_str("[web_search]\nenabled = true\n").unwrap();
    assert!(cfg.web_search_runtime_config().is_err());

    let toml = r#"
[web_search]
enabled = true
order = ["doubao-custom"]

[web_search.doubao_custom]
api_key = "custom-test"

[web_search.tavily]
api_keys = ["tavily-test"]
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert!(cfg.web_search_runtime_config().is_err());

    let custom_only_field = r#"
[web_search]
enabled = true

[web_search.doubao_custom]
api_key = "custom-test"
max_snippet_tokens = 1000
"#;
    let cfg: Config = toml::from_str(custom_only_field).unwrap();
    assert!(cfg.web_search_runtime_config().is_err());
}

#[test]
fn web_search_debug_output_redacts_provider_keys() {
    let cfg: Config = toml::from_str(
        r#"
[web_search]
enabled = true

[web_search.doubao_custom]
api_key = "custom-sensitive-value"

[web_search.tavily]
api_keys = ["tavily-sensitive-value"]
"#,
    )
    .unwrap();
    let rendered = format!("{cfg:?}");
    assert!(!rendered.contains("custom-sensitive-value"));
    assert!(!rendered.contains("tavily-sensitive-value"));
    assert!(rendered.contains("doubao_custom_configured: true"));
    assert!(rendered.contains("tavily_configured: true"));
}

#[test]
fn example_toml_parses() {
    let cfg = parse_config_text(include_str!("../../../../rclaw.toml.example")).unwrap();
    // 示例里 web_search 池是开着的（豆包 Custom / 智谱 / 豆包 Global 配了示例键）。
    assert!(cfg.web_search.enabled);
}

#[test]
fn skills_section_is_optional_and_parses() {
    // 缺省（无 [skills] 段）：None = 运行期取 64 KiB 默认，零行为变化（D7）。
    let cfg = parse_config_text("").unwrap();
    assert_eq!(cfg.skills.max_output_bytes, None);

    let cfg = parse_config_text("[skills]\nmax_output_bytes = 131072\n").unwrap();
    assert_eq!(cfg.skills.max_output_bytes, Some(131_072));

    // deny_unknown_fields：拼错键名必须在启动期暴露。
    let res = parse_config_text("[skills]\nmax_output = 131072\n");
    assert!(
        res.is_err(),
        "deny_unknown_fields 应拒绝未声明的 [skills] 字段"
    );
}

#[test]
fn removed_legacy_web_browser_section_is_ignored() {
    // v2.8.0 删除了 [web_browser] 节（web_browser 工具退役）。旧版本写出的
    // 配置（如 /webbrowser 热切换回写的 engine）升级后必须仍可加载，
    // 否则应用启动即「解析 rclaw.toml 失败」。
    let cfg = parse_config_text(
        "[web_browser]\nengine = \"baidu\"\n\n[llm]\nrequest_timeout_secs = 60\n",
    )
    .unwrap();
    assert_eq!(cfg.llm.request_timeout_secs, 60);
}

#[test]
fn legacy_section_with_windows_path_still_loads() {
    // 遗留节 + 含 `\U` 反斜杠序列的 Windows 路径（TOML 里非法转义）同时出现：
    // 归一化必须先行——`\U` 会让 toml_edit 解析失败、遗留节剥不掉，带病解析
    // 仍然失败。回归用例（codex review 第 3 轮）：load_from_dir 曾先剥离后
    // 归一化，这种配置启动即「解析 rclaw.toml 失败」。
    let text =
        "[web_browser]\nengine = \"baidu\"\n\n[agent]\nbase_dir = \"C:\\Users\\alice\\rclaw\"\n";
    assert!(text.contains("\\U"), "用例应含非法转义 \\U");
    let cfg = parse_config_text(text).unwrap_or_else(|e| panic!("parse failed: {e:#}"));
    // 遗留节已剥离、其余配置正常生效（agent.base_dir 归一化为合法 TOML 转义）。
    assert!(cfg.agent.base_dir.starts_with("C:\\Users\\alice\\rclaw"));
}

#[test]
fn unknown_top_level_keys_are_still_rejected() {
    // 遗留节剥离不得放宽 deny_unknown_fields：拼错的现存/未知键仍要在启动期暴露。
    let res = parse_config_text("[no_such_section]\nkey = 1\n");
    assert!(res.is_err(), "deny_unknown_fields 应拒绝未声明的顶层键");
}

#[test]
fn wxbot_send_pacer_config_defaults_and_zero_clamps() {
    let cfg = Config::default();
    let w = cfg.wxbot_channel_config();
    assert_eq!(w.send_min_gap_sec, 2);
    assert_eq!(w.send_jitter_sec, 3);
    assert_eq!(w.send_window_max, 10);
    assert_eq!(w.send_failure_fuse, 5);

    let mut cfg = Config::default();
    cfg.wxbot.retry_sec = 0;
    cfg.wxbot.backoff_sec = 0;
    let w = cfg.wxbot_channel_config();
    assert_eq!(w.retry_sec, 1, "retry_sec 0 会热循环，必须钳到 1");
    assert_eq!(w.backoff_sec, 1, "backoff_sec 0 会热循环，必须钳到 1");
}

#[test]
fn wxbot_humanize_defaults_and_clamps() {
    let cfg = Config::default();
    let w = cfg.wxbot_channel_config();
    assert!(!w.humanize_enabled, "拟人化默认关闭");
    assert_eq!(w.humanize_think_ms, 600);
    assert_eq!(w.humanize_segment_chars, 60);
    assert_eq!(w.humanize_segment_max, 3);
    assert_eq!(w.humanize_segment_interval_min_ms, 700);
    assert_eq!(w.humanize_segment_interval_max_ms, 1500);

    let mut cfg = Config::default();
    cfg.wxbot.humanize_segment_chars = 0;
    cfg.wxbot.humanize_segment_max = 0;
    cfg.wxbot.humanize_segment_interval_min_ms = 2000;
    cfg.wxbot.humanize_segment_interval_max_ms = 100;
    cfg.wxbot.humanize_think_ms = u64::MAX;
    let w = cfg.wxbot_channel_config();
    assert!(
        w.humanize_segment_chars >= 8,
        "目标字符数过小会切碎任意 UTF-8 字符"
    );
    assert!(w.humanize_segment_max >= 1, "段数下限 1");
    assert!(
        w.humanize_segment_interval_max_ms >= w.humanize_segment_interval_min_ms,
        "段间停顿上限不得小于下限"
    );
    assert!(
        w.humanize_think_ms <= 10_000,
        "think 必须有硬上限，避免长期挂起"
    );
    assert!(
        w.humanize_segment_interval_max_ms <= 30_000,
        "interval 必须有硬上限，避免长期挂起"
    );
    let mut cfg = Config::default();
    cfg.wxbot.chunk_bytes = 1;
    assert!(
        cfg.wxbot_channel_config().send_chunk_bytes >= 4,
        "UTF-8 最小 4 字节，chunk 不得小于 4"
    );
}

#[test]
fn strip_removed_legacy_sections_reports_and_preserves_rest() {
    let text = "# 顶部注释\n[llm]\nprovider = \"deepseek\"\n\n[web_browser]\nengine = \"baidu\"\n";
    let (stripped, removed) = strip_removed_legacy_sections(text);
    assert_eq!(removed, vec!["web_browser"]);
    // 其余内容（含注释与格式）逐字节保留。
    assert_eq!(stripped, "# 顶部注释\n[llm]\nprovider = \"deepseek\"\n");
    // 清理后的文本必须可解析。
    parse_config_text(&stripped).unwrap();
    // 无遗留节时原样返回。
    let (same, none) = strip_removed_legacy_sections("[llm]\nprovider = \"x\"\n");
    assert!(none.is_empty());
    assert_eq!(same, "[llm]\nprovider = \"x\"\n");
}

#[test]
fn soul_enabled_defaults_true_and_example_round_trips() {
    // 缺省开启（既有行为不变）。
    assert!(Config::default().agent.soul_enabled);
    // 显式关闭可解析。
    let cfg: Config = toml::from_str("[agent]\nsoul_enabled = false\n").unwrap();
    assert!(!cfg.agent.soul_enabled);
    // example 里写了这个键且为 true（配置页据此渲染勾选框）。
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("rclaw.toml.example");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("soul_enabled = true"));
}

// ── [sync] 多端同步 ───────────────────────────────────────────────────

#[test]
fn sync_section_defaults_when_absent() {
    // 无 [sync] 段：同步关闭，其余取默认值（与 docs/multi-device-sync.md §12.3 一致）。
    let cfg = parse_config_text("").unwrap();
    assert!(!cfg.sync.enabled);
    assert_eq!(cfg.sync.transport, "webdav");
    assert_eq!(cfg.sync.scopes, ["sessions", "memory", "persona"]);
    assert_eq!(cfg.sync.channels, ["desktop"]);
    assert_eq!(cfg.sync.poll_interval_sec, 60);
    assert_eq!(cfg.sync.background_interval_sec, 900);
    assert_eq!(cfg.sync.presend_pull_wait_ms, 1500);
    assert_eq!(cfg.sync.attachment_max_mb, 50);
    assert_eq!(cfg.sync.trash_days, 30);
    assert_eq!(cfg.sync, rclaw_sync::SyncConfig::default());
    // 示例文件里的 [sync] 段是关着的，没注释掉的键与默认值一致。
    let example = parse_config_text(include_str!("../../../../rclaw.toml.example")).unwrap();
    assert_eq!(example.sync, rclaw_sync::SyncConfig::default());
}

#[test]
fn sync_section_overrides_from_toml() {
    let toml = r#"
[sync]
enabled = true
device_name = "我的电脑"
transport = "s3"
scopes = ["memory", "persona", "skills"]
channels = ["desktop", "weixin"]
poll_interval_sec = 30
background_interval_sec = 600
presend_pull_wait_ms = 0
attachment_max_mb = 20
trash_days = 0

[sync.webdav]
url = "https://dav.example.com/rclaw-sync/"
username = "me@example.com"
allow_http = false
pinned_cert_sha256 = "AB:CD"

[sync.file]
dir = "/srv/sync"

[sync.s3]
endpoint = "https://oss-cn-hangzhou.aliyuncs.com"
region = "cn-hangzhou"
bucket = "my-bucket"
prefix = "rclaw"
access_key_id = "AKID"
path_style = true
allow_http = false
"#;
    let cfg = Config::from_toml(toml).unwrap();
    assert!(cfg.sync.enabled);
    assert_eq!(cfg.sync.device_name, "我的电脑");
    assert_eq!(cfg.sync.transport, "s3");
    assert_eq!(cfg.sync.scopes, ["memory", "persona", "skills"]);
    assert_eq!(cfg.sync.channels, ["desktop", "weixin"]);
    assert_eq!(cfg.sync.poll_interval_sec, 30);
    assert_eq!(cfg.sync.background_interval_sec, 600);
    assert_eq!(cfg.sync.presend_pull_wait_ms, 0);
    assert_eq!(cfg.sync.attachment_max_mb, 20);
    assert_eq!(cfg.sync.trash_days, 0);
    assert_eq!(cfg.sync.webdav.url, "https://dav.example.com/rclaw-sync/");
    assert_eq!(cfg.sync.webdav.username, "me@example.com");
    assert_eq!(cfg.sync.webdav.pinned_cert_sha256, "AB:CD");
    assert_eq!(cfg.sync.file.dir, "/srv/sync");
    assert_eq!(cfg.sync.s3.bucket, "my-bucket");
    assert!(cfg.sync.s3.path_style);
}

#[test]
fn sync_section_rejects_unknown_fields_and_bad_values() {
    // deny_unknown_fields：拼错的键、不该写在这里的密码都在启动期暴露。
    for toml in [
        "[sync]\nenabeld = true\n",
        "[sync]\nattachments_wifi_only = true\n",
        "[sync.webdav]\npassword = \"hunter2\"\n",
        "[sync.s3]\nsecret_key = \"x\"\n",
    ] {
        assert!(parse_config_text(toml).is_err(), "应拒绝: {toml}");
    }
    // 取值校验（存储类型、同步范围、通道名、轮询间隔）在加载收尾与面板保存前都做。
    for toml in [
        "[sync]\ntransport = \"ftp\"\n",
        "[sync]\nscopes = [\"everything\"]\n",
        "[sync]\nchannels = [\"Desktop\"]\n",
        "[sync]\npoll_interval_sec = 1\n",
        "[sync]\nbackground_interval_sec = 5\n",
    ] {
        assert!(Config::from_toml(toml).is_err(), "加载应拒绝: {toml}");
        assert!(Config::parse_str(toml).is_err(), "面板保存前应拒绝: {toml}");
    }
    // 配置的 Debug 输出里没有任何凭据可言：密码与密钥根本不在这个结构里。
    let cfg =
        Config::from_toml("[sync.webdav]\nurl = \"https://dav.example.com/\"\nusername = \"me\"\n")
            .unwrap();
    let shown = format!("{:?}", cfg.sync);
    assert!(
        !shown.contains("password") && !shown.contains("secret"),
        "{shown}"
    );
}

// ---------------------------------------------------------------------------
// DeepSeek / GLM：推理参数、媒体方言、per-provider max_tokens、搜索子表
// ---------------------------------------------------------------------------

fn build_provider(text: &str, name: &str) -> Result<rclaw_llm::ProviderConfig, String> {
    let cfg = Config::parse_str(text).map_err(|error| format!("{error:#}"))?;
    cfg.build_provider_config_for(name)
        .map_err(|error| format!("{error:#}"))
}

#[test]
fn toml_diagnostics_redact_quoted_field_values() {
    let redacted = crate::config::redact_toml_diagnostic(
        "invalid type: string \"sk-secret\", expected a sequence",
    );
    assert!(!redacted.contains("sk-secret"), "{redacted}");
    assert!(redacted.contains("\"…\""), "{redacted}");
    // 漏写引号的数字凭据：Serde 用反引号包住原值。
    let numeric = crate::config::redact_toml_diagnostic(
        "invalid type: integer `123456789`, expected a string",
    );
    assert!(!numeric.contains("123456789"), "{numeric}");
    assert!(numeric.contains("`…`"), "{numeric}");
    // 字段名/类型名仍可读。
    let named = crate::config::redact_toml_diagnostic("missing field `api_key`");
    assert!(named.contains("api_key"), "{named}");
}

#[test]
fn omitted_main_provider_uses_default_and_must_exist() {
    // 缺省 [llm].provider = "deepseek"；只有 zhipu 条目时启动会找不到目标。
    let text = r#"[llm]
[[llm.providers]]
name = "zhipu"
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
model = "glm-5.4"
api_key = "sk-test"
"#;
    let error = match Config::parse_str(text) {
        Ok(_) => panic!("缺省主 provider 不存在时必须拒绝"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(message.contains("deepseek"), "{message}");

    // 显式空白与缺省同义。
    let blank = text.replace(
        "[llm]",
        "[llm]
provider = \"  \"",
    );
    assert!(Config::parse_str(&blank).is_err());

    // 补上 deepseek 条目后通过，且有效名归一为 deepseek。
    let ok = format!(
        "{text}
[[llm.providers]]
name = \"deepseek\"
base_url = \"https://api.deepseek.com/v1\"
model = \"deepseek-flash\"
api_key = \"sk-test\"
"
    );
    let cfg = Config::parse_str(&ok).unwrap();
    assert_eq!(cfg.effective_provider_name(), "deepseek");

    // 空 provider 列表 + 显式非默认主引用：启动只会注入 deepseek，必须拒绝。
    let empty_other = "[llm]
provider = \"zhipu\"
";
    assert!(Config::parse_str(empty_other).is_err());
    // 只有默认引用/无引用时合法（供「仅含同步设置」的配置使用）。
    assert!(Config::parse_str(
        "[llm]
provider = \"deepseek\"
"
    )
    .is_ok());
    assert!(Config::parse_str(
        "[sync]
enabled = false
"
    )
    .is_ok());
}

#[test]
fn profile_override_wins_before_final_validation() {
    // 旧条目本身合法（glm-5.3 + none），但切换目标是 glm-5.3-flash：覆盖必须在
    // 校验之前发生，否则旧档位会按新模型的支持范围误拒/误判。
    let cfg = Config::parse_str(
        r#"[llm]
provider = "zhipu"

[[llm.providers]]
name = "zhipu"
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
model = "glm-5.3"
api_key = "sk-test"
reasoning_effort = "none"
"#,
    )
    .unwrap();
    let built = cfg
        .build_provider_config_for_profile(
            "zhipu",
            &crate::config::ProviderProfileParams {
                model: "glm-5.3-flash",
                media_wire: "openai",
                thinking: "enabled",
                reasoning_effort: "max",
                reasoning_field: "reasoning_content",
                max_tokens_key: "max_tokens",
                max_tokens: 128_000,
            },
        )
        .unwrap();
    assert_eq!(built.default_model, "glm-5.3-flash");
    assert_eq!(built.reasoning.thinking, Some(true));
    assert_eq!(built.reasoning.effort.as_deref(), Some("max"));
    assert_eq!(built.media_wire, rclaw_llm::MediaWire::OpenAiCompat);
}

#[test]
fn deepseek_provider_parses_reasoning_and_media_fields() {
    let cfg = build_provider(
        r#"[llm]
provider = "deepseek"

[[llm.providers]]
name = "deepseek"
base_url = "https://api.deepseek.com/v1"
model = "deepseek-flash"
api_key = "sk-test"
context_length = 1048576
max_tokens = 128000
thinking = "disabled"
media_wire = "deepseek"
"#,
        "deepseek",
    )
    .unwrap();
    assert_eq!(cfg.max_tokens, 128_000);
    assert_eq!(cfg.flavor, rclaw_llm::ProviderFlavor::DeepSeek);
    assert_eq!(cfg.media_wire, rclaw_llm::MediaWire::DeepSeek);
    assert_eq!(cfg.reasoning.thinking, Some(false));
    assert!(cfg.reasoning.effort.is_none());
}

#[test]
fn provider_rejects_unsupported_reasoning_values() {
    // DeepSeek 只认 none/low/high/max；minimal 不是官方值。
    let error = build_provider(
        r#"[llm]
provider = "deepseek"

[[llm.providers]]
name = "deepseek"
base_url = "https://api.deepseek.com/v1"
model = "deepseek-flash"
api_key = "sk-test"
reasoning_effort = "minimal"
"#,
        "deepseek",
    )
    .unwrap_err();
    assert!(error.contains("reasoning_effort"), "{error}");

    // glm-5.3-flash 强制思考，禁止 disabled。
    let error = build_provider(
        r#"[llm]
provider = "zhipu"

[[llm.providers]]
name = "zhipu"
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
model = "glm-5.3-flash"
api_key = "sk-test"
thinking = "disabled"
"#,
        "zhipu",
    )
    .unwrap_err();
    assert!(error.contains("强制开启思考"), "{error}");

    // 关闭思考与 effort 互斥。
    let error = build_provider(
        r#"[llm]
provider = "deepseek"

[[llm.providers]]
name = "deepseek"
base_url = "https://api.deepseek.com/v1"
model = "deepseek-flash"
api_key = "sk-test"
thinking = "disabled"
reasoning_effort = "low"
"#,
        "deepseek",
    )
    .unwrap_err();
    assert!(error.contains("不能同时设置"), "{error}");
}

#[test]
fn provider_rejects_credentials_in_base_url() {
    let error = build_provider(
        r#"[llm]
provider = "custom"

[[llm.providers]]
name = "custom"
base_url = "https://user:pass@example.com/v1"
model = "x"
api_key = "sk-test"
"#,
        "custom",
    )
    .unwrap_err();
    assert!(error.contains("用户名或密码"), "{error}");
}

#[test]
fn web_search_deepseek_section_parses() {
    let cfg = Config::parse_str(
        r#"[web_search]
enabled = true
order = ["deepseek"]

[web_search.deepseek]
api_key = "sk-test"
need_content = true
"#,
    )
    .unwrap();
    let runtime = cfg.web_search_runtime_config().unwrap().unwrap();
    let deepseek = runtime.deepseek.expect("deepseek 子表应被构建");
    assert!(deepseek.need_content);
    assert_eq!(runtime.order, vec![rclaw_tools::BackendId::DeepSeek]);
}
