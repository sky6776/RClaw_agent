//! 核心组件装配：config → provider → 工具注册表 → AgentOptions → cron → slash。
//!
//! 从 `main.rs` 抽出（原 4-6.6 段），供两个二进制共用：
//! - `rclaw-cli`：console / weixin / napcat 通道 + dispatcher；
//! - `rclaw-desktop`：Tauri 桌面壳（desktop 虚拟通道 + Web 事件桥）。
//!
//! 通道接线与进程生命周期在两个二进制里各自保留——它们的通道集合、日志初始化
//! 时机与常驻方式不同；共享的是「与通道无关」的核心组件。

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::Result;

use rclaw_core::RuntimeChannelInfo;
use rclaw_llm::{Provider, SwappableProvider};
use rclaw_runtime::{
    AgentCancelTool, AgentOptions, AgentResumeTool, AgentStatusTool, AgentToolRuntime,
    AgentWaitTool, BotRegistry, ClaudeAgentTool, MessageAgentTool, PlanRunTool, SendMessageTool,
    SlashRegistry, TaskCreateTool, TaskGetTool, TaskListTool, TaskUpdateTool, TeamCreateTool,
    TeamDeleteTool,
};
use rclaw_store::fs::Layout;
use rclaw_store::memory::MemoryStore;
use rclaw_store::session::SessionStore;
use rclaw_tools::{
    CronController, DoubaoBackend, MemoryTool, ProviderSearchTool, ReaderBackend, SearchBackend,
    SessionSearchTool, SkillListTool, SkillManageTool, SkillUsageStore, TavilyClient,
    TavilyExtract, ToolRegistry, WebSearch, WebSearchControl, ZhipuBackend,
};

use crate::config::Config;

/// 存储布局准备：建目录 + 安装技能仓库 Manifest。
///
/// 必须先于日志初始化（日志默认写入 `agent/logs`）。CLI 与桌面版共用。
pub fn prepare_storage(cfg: &Config) -> Result<Layout> {
    let layout = Layout::new(&cfg.agent.base_dir);
    rclaw_store::fs::init_layout(&layout)?;
    SkillUsageStore::new(layout.skills_dir.clone())
        .install_repository_manifest()
        .map_err(anyhow::Error::msg)?;
    Ok(layout)
}

/// 与通道无关的核心组件集合。
///
/// `_mcp_manager` 必须存活到进程结束（MCP 子进程由它托管），故以字段形式持有。
pub struct CoreComponents {
    /// 热切换 provider 句柄（`/llmodel` 斜杠命令持久化目标）。
    pub hot: Arc<SwappableProvider>,
    /// agent loop 使用的 provider（即 `hot` 的 trait 对象形态）。
    pub provider: Arc<dyn Provider>,
    pub session: SessionStore,
    pub memory: MemoryStore,
    pub layout: Layout,
    pub tools: Arc<ToolRegistry>,
    pub opts: AgentOptions,
    pub cron: Arc<CronController>,
    pub slash: Arc<SlashRegistry>,
    /// 多 agent 工具运行时句柄（桌面版「Agent 状态」面板读取后台执行快照）。
    pub agent_runtime: AgentToolRuntime,
    /// Bot Mode：bot 注册表（persona / 花名册 / 每 bot 记忆）。
    pub bots: Arc<BotRegistry>,
    /// Bot Mode：BotRouter 延迟回填句柄——工具注册发生在装配期，
    /// 而 router 需要 inbound 发送端（二进制建总线后 set）。
    pub bot_router_slot: Arc<OnceLock<Arc<rclaw_runtime::BotRouter>>>,
    /// 评审专用辅助 provider（`[llm] review_provider`，缺省 None = 复用主 provider）。
    pub review_provider: Option<Arc<dyn Provider>>,
    #[allow(dead_code)]
    pub web_search_control: Option<Arc<WebSearchControl>>,
    pub _mcp_manager: Option<Arc<rclaw_tools::mcp::McpManager>>,
}

/// 装配全部核心组件。
///
/// - `layout`：[`prepare_storage`] 的返回值（调用方已完成目录初始化）；
/// - `channel_info`：由各二进制按自己的通道集合构造（桌面版只有 desktop 虚拟通道）；
/// - `slash_config_path`：`/llmodel` 持久化目标（CLI 为 cwd 的 `rclaw.toml`，
///   桌面版为用户数据目录下的 `rclaw.toml`）。
///
/// 行为保真说明：`main.rs` 原实现给 [`AgentToolRuntime`] 传的是「精简 opts」
///（stream/daemon/verify_candidate_patches，其余取默认），给 AgentLoop 传「完整 opts」
///（压缩预算派生等）。子 agent 不做历史压缩，所以压缩相关差异无影响；但 ZCode 移植的
/// 子代理选项（`background_wake_*`、`subagent_auto_background`、`subagent_request_stagger`、
/// `plan_escalation_wait_secs`）只被 AgentToolRuntime 读取——目前没有 toml 配置项、两边
/// 都是默认值；将来若接入配置，必须写进 `runtime_opts`，写进 AgentLoop 的 `opts` 不会
/// 生效。`recent_days` 两边都读（TurnManager 的推理尾部 / 摘要 / `/context`，与子代理的
/// 近期笔记注入），接入配置时必须两份 opts 都写。
pub async fn assemble_core(
    cfg: &Config,
    layout: &Layout,
    channel_info: &RuntimeChannelInfo,
    daemon: bool,
    slash_config_path: PathBuf,
) -> Result<CoreComponents> {
    assemble_core_with(cfg, layout, channel_info, daemon, slash_config_path, |_| {}).await
}

/// 与 [`assemble_core`] 相同，但允许宿主在注册表定型（`Arc` 化）前追加/覆盖工具。
///
/// `customize` 在内置工具、web_search、MCP 工具注册完毕、多 agent 工具注册之前调用，
/// 因此宿主追加的工具同样对子 agent 可见。设计给 Android APK 宿主注册进程内 gbrain
/// 工具用；CLI 与桌面版经 [`assemble_core`] 传空闭包，行为与拆分前逐项一致。
pub async fn assemble_core_with(
    cfg: &Config,
    layout: &Layout,
    channel_info: &RuntimeChannelInfo,
    daemon: bool,
    slash_config_path: PathBuf,
    customize: impl FnOnce(&mut ToolRegistry),
) -> Result<CoreComponents> {
    // 媒体沙箱目录：外部 channel 共用（绝对路径，供 shell 子进程经环境变量读取）。
    let media_dir = if layout.base.is_absolute() {
        layout.base.join("media")
    } else {
        std::env::current_dir()?.join(&layout.base).join("media")
    };
    let media_dir_str = media_dir.to_string_lossy().replace('\\', "/");
    // 子进程契约：技能脚本（publish_media.py / md2html.py 等）经 os.environ 读取。
    std::env::set_var("RCLAW_MEDIA_DIR", &media_dir_str);
    // 子进程契约：对声明了媒体能力的 channel 导出 RCLAW_<NAME>_ENABLED=1，
    // publish_media.py 据此决定能否为该 channel 发 MEDIA_MARKER。桌面/desktop
    // 渠道此前无人导出（cli main.rs 只显式处理 weixin/napcat，与其幂等重复），
    // 导致会话里 agent 发布媒体一直拿不到回执。
    for caps in &channel_info.channels {
        let flag = format!(
            "RCLAW_{}_ENABLED",
            caps.name.to_ascii_uppercase().replace('-', "_")
        );
        if caps.media {
            std::env::set_var(&flag, "1");
        } else {
            std::env::remove_var(&flag);
        }
    }

    // [skills] 技能执行参数：stdout/stderr 单流字节限额（进程级槽位，技能入口
    // 工具执行时读取；缺省 64 KiB 与历史一致——桌面/Termux 不写该键零行为变化）。
    match cfg.skills.max_output_bytes {
        Some(bytes) => rclaw_tools::set_skill_output_limit(bytes),
        None => rclaw_tools::clear_skill_output_limit(),
    }

    // Provider（DeepSeek / 智谱等 OpenAI 兼容）。SwappableProvider 支持 /llmodel 热切换。
    let pcfg = cfg.build_provider_config()?;
    let hot = Arc::new(SwappableProvider::new(pcfg)?);
    let provider: Arc<dyn Provider> = hot.clone();

    // 评审专用辅助 provider（[llm] review_provider）：自进化评审用便宜模型跑。
    // 与主 provider 同名时视为未配置；构造失败直接启动报错（配置 typo 早暴露）。
    let review_provider: Option<Arc<dyn Provider>> = match cfg.llm.review_provider.as_deref() {
        None | Some("") => None,
        Some(name) if name == cfg.llm.provider => None,
        Some(name) => {
            let pcfg = cfg.build_provider_config_for(name)?;
            Some(Arc::new(SwappableProvider::new(pcfg)?))
        }
    };

    // 存储句柄：会话状态库 agent/state.sqlite（打开失败 = 启动失败，不能带着无法落盘的
    // 会话跑）。MEMORY.md / USER.md 的字符上限由配置决定。
    let session = SessionStore::open(layout.clone())?;
    let memory = MemoryStore::new(layout.clone())
        .with_char_limits(cfg.agent.memory_char_limit, cfg.agent.user_char_limit);

    // cron 控制器 + 工具注册表（共享同一份 cron 任务表）。
    let cron = Arc::new(CronController::load(layout.clone())?);
    let mut tools = ToolRegistry::with_defaults(layout.base.clone(), cron.clone());
    let agent_tools_registry = Arc::new(OnceLock::new());

    // memory 工具：模型在 turn 中主动维护 MEMORY.md / USER.md（与后台自动 review 互补）。
    tools.register(Box::new(MemoryTool::new(memory.clone())));

    // skill_manage 工具：把「自进化」从建议落地为真实技能改动。
    tools.register(Box::new(SkillManageTool::new(layout.skills_dir.clone())));
    // skills_list 工具：只读技能清单（模型发现 + 桌面版技能面板共用）。
    tools.register(Box::new(SkillListTool::new(layout.skills_dir.clone())));

    // session_search 工具：跨会话历史检索（B1——agent 能召回自己的过往经历）。
    tools.register(Box::new(SessionSearchTool::new(session.clone())));
    // session_handoff 工具：跨会话上下文交接（ZCode 移植 · Layer 5，ReadSessionContext
    // 同源）——按 query 相关性把另一段会话压缩成有界交接素材。
    tools.register(Box::new(
        rclaw_runtime::session_handoff::SessionHandoffTool::new(session.clone()),
    ));

    // 浏览器能力统一为 CDP browser_* 工具面，由宿主装配钩子注册（桌面/APK 的
    // customize 调 rclaw_tools::cdp_browser::register）；CLI/Termux 无浏览器工具。
    // web_search 工具：router 统一负责顺序、启禁、fallback、总时间预算和熔断。
    let web_search_control = if let Some(web_cfg) = cfg.web_search_runtime_config()? {
        let control = Arc::new(
            WebSearchControl::new(
                web_cfg.order,
                &web_cfg.enabled,
                web_cfg.fallback,
                web_cfg.circuit_failure_threshold,
                std::time::Duration::from_secs(web_cfg.circuit_cooldown_secs),
            )
            .map_err(anyhow::Error::msg)?,
        );
        let mut search_backends: Vec<Arc<dyn SearchBackend>> = Vec::new();
        let mut reader_backends: Vec<Arc<dyn ReaderBackend>> = Vec::new();
        let standalone_max_results = web_cfg.tool.max_results;

        if let Some(provider_cfg) = web_cfg.doubao_custom {
            let backend = Arc::new(DoubaoBackend::new(provider_cfg).map_err(anyhow::Error::msg)?);
            tools.register(Box::new(
                ProviderSearchTool::new(backend.clone(), control.clone(), standalone_max_results)
                    .map_err(anyhow::Error::msg)?,
            ));
            search_backends.push(backend);
        }
        if let Some(provider_cfg) = web_cfg.tavily {
            let client = Arc::new(TavilyClient::new(provider_cfg).map_err(anyhow::Error::msg)?);
            tools.register(Box::new(
                ProviderSearchTool::new(client.clone(), control.clone(), standalone_max_results)
                    .map_err(anyhow::Error::msg)?,
            ));
            search_backends.push(client.clone());
            reader_backends.push(client.clone());
            tools.register(Box::new(
                TavilyExtract::from_client(client, control.clone()).map_err(anyhow::Error::msg)?,
            ));
        }
        if let Some(provider_cfg) = web_cfg.zhipu {
            let backend = Arc::new(ZhipuBackend::new(provider_cfg).map_err(anyhow::Error::msg)?);
            tools.register(Box::new(
                ProviderSearchTool::new(backend.clone(), control.clone(), standalone_max_results)
                    .map_err(anyhow::Error::msg)?,
            ));
            search_backends.push(backend.clone());
            reader_backends.push(backend);
        }
        if let Some(provider_cfg) = web_cfg.doubao_global {
            let backend = Arc::new(DoubaoBackend::new(provider_cfg).map_err(anyhow::Error::msg)?);
            tools.register(Box::new(
                ProviderSearchTool::new(backend.clone(), control.clone(), standalone_max_results)
                    .map_err(anyhow::Error::msg)?,
            ));
            search_backends.push(backend);
        }

        let tool = WebSearch::new(
            web_cfg.tool,
            control.clone(),
            search_backends,
            reader_backends,
        )
        .map_err(anyhow::Error::msg)?;
        tools.register(Box::new(tool));
        Some(control)
    } else {
        None
    };

    // MCP：解析 `[mcp]` 段，自动启动服务器并注册远端工具（限定名 `<server>__<tool>`）。
    let mcp_resolved = rclaw_tools::mcp::config::McpResolved::build(cfg.mcp.clone())
        .map_err(|e| anyhow::anyhow!("MCP 配置错误: {e}"))?;
    let _mcp_manager = if !mcp_resolved.servers.is_empty() {
        let manager = Arc::new(rclaw_tools::mcp::McpManager::load(mcp_resolved).await?);
        let before = tools.schemas().len();
        manager.extend_registry(&mut tools).await?;
        let added = tools.schemas().len() - before;
        tracing::info!("已注册 {added} 个 MCP 工具");
        Some(manager)
    } else {
        None
    };

    // 宿主定制点（APK：进程内 gbrain 工具等）；默认空操作。
    customize(&mut tools);

    // 精简 opts：与原 main.rs 一致，传给 AgentToolRuntime（见函数注释）——
    // stream/daemon 之外带上子代理也会读的行为开关：verify_candidate_patches（交叉验证
    // 在 run_task_inner 里跑）与工具结果落盘阈值（子代理工具循环同样走 spill）；不随
    // 配置传入就永远停在默认值。子代理专属选项（唤醒 / 自动转后台 / 升级等待等）
    // 当前无配置项，取默认值。
    // LLM 重试总预算按 provider 单次超时派生（两边同一个值）：预算写死 600s 时，用户把
    // `[llm] request_timeout_secs` 调到 600s 以上，每次合法长调用都会被预算截断。
    let llm_retry_budget = rclaw_runtime::options::derive_llm_retry_budget(
        std::time::Duration::from_secs(cfg.llm.request_timeout_secs),
    );
    let runtime_opts = AgentOptions {
        stream_enabled: cfg.llm.stream,
        llm_retry_budget,
        daemon,
        verify_candidate_patches: cfg.agent.verify_candidate_patches,
        tool_result_artifact_enabled: cfg.agent.tool_result_artifact_enabled,
        tool_result_artifact_bytes: cfg.agent.tool_result_artifact_bytes,
        workflow_background: cfg.agent.workflow_background,
        workflow_legacy_tool: cfg.agent.workflow_legacy_tool,
        max_active_workflow_runs: cfg.agent.max_active_workflow_runs.max(1),
        workflow_command_confirm: cfg.agent.workflow_command_confirm,
        ..AgentOptions::default()
    };

    // 按 [[llm.providers]] 名字构建独立 provider 的解析器：Bot Mode 的 BotRegistry
    // 与子代理 `model` 入参（Layer 2b）共用同一套语义，只建一份。
    let provider_factory: rclaw_runtime::agent_tools::ModelFactory = {
        let cfg = cfg.clone();
        Arc::new(move |name: &str| -> Option<Arc<dyn rclaw_llm::Provider>> {
            cfg.build_provider_config_for(name)
                .ok()
                .and_then(|pcfg| rclaw_llm::SwappableProvider::new(pcfg).ok())
                .map(|p| Arc::new(p) as Arc<dyn rclaw_llm::Provider>)
        })
    };

    // 多 agent 工具：子 agent 共享同一套普通工具；OnceLock 延迟回填避免自引用构造。
    let agent_runtime = AgentToolRuntime::new(
        provider.clone(),
        memory.clone(),
        layout.clone(),
        session.journal(),
        runtime_opts,
        agent_tools_registry.clone(),
        channel_info.clone(),
        web_search_control.clone(),
    );
    // Layer 2b（ZCode 移植）：子代理独立模型/推理级别——Agent 工具的 `model` 入参按
    // [[llm.providers]] 名字解析为独立 provider（入参校验期已拒绝未知名字）。
    // 必须在任何 clone 交给工具之前注入（factory 槽位对全部 clone 共享，这里只是
    // 让装配顺序一眼可见）。
    agent_runtime.set_model_factory(provider_factory.clone());
    tools.register(Box::new(TeamCreateTool::new(agent_runtime.clone())));
    tools.register(Box::new(TeamDeleteTool::new(agent_runtime.clone())));
    tools.register(Box::new(TaskCreateTool::new(agent_runtime.clone())));
    tools.register(Box::new(TaskUpdateTool::new(agent_runtime.clone())));
    tools.register(Box::new(TaskListTool::new(agent_runtime.clone())));
    tools.register(Box::new(TaskGetTool::new(agent_runtime.clone())));
    tools.register(Box::new(AgentStatusTool::new(agent_runtime.clone())));
    tools.register(Box::new(AgentWaitTool::new(agent_runtime.clone())));
    tools.register(Box::new(AgentCancelTool::new(agent_runtime.clone())));
    tools.register(Box::new(AgentResumeTool::new(agent_runtime.clone())));
    tools.register(Box::new(SendMessageTool::new(agent_runtime.clone())));
    // 确定性计划工作流：主循环专用（子代理被 is_subagent_blocked_tool 拒绝，计划不可嵌套）。
    tools.register(Box::new(PlanRunTool::new(agent_runtime.clone())));
    // Layer 3（ZCode 移植）：escalate 提问通道——计划步骤内提问（子代理面），
    // ResolveWorkflowQuestion 应答（主代理面；对子代理屏蔽）。
    tools.register(Box::new(rclaw_runtime::agent_tools::EscalateTool::new(
        agent_runtime.clone(),
    )));
    tools.register(Box::new(
        rclaw_runtime::agent_tools::ResolveWorkflowQuestionTool::new(agent_runtime.clone()),
    ));
    // Layer 5（ZCode 移植）：write/edit/delete 文件 checkpoint + 按 messageId 选点回滚
    //（checkpoint 与会话消息同库，按当前会话键隔离）。
    tools.register(Box::new(rclaw_runtime::turn::rewind::RewindFilesTool::new(
        session.clone(),
    )));
    // Layer 3 引擎：动态工作流工具面（主代理专属，子代理被屏蔽表拒绝）。
    // 技能文件首次安装 + 完整读取观察者（技能门 hash 登记）。
    let _ = rclaw_runtime::agent_tools::ensure_skill_file(&layout.base);
    rclaw_runtime::agent_tools::install_workflow_skill_observer(
        session.journal(),
        layout.base.clone(),
    );
    let workflow_service = rclaw_runtime::agent_tools::WorkflowService::new(
        agent_runtime.clone(),
        layout,
        rclaw_runtime::agent_tools::WorkflowOptions {
            background: cfg.agent.workflow_background,
            legacy_tool: cfg.agent.workflow_legacy_tool,
            max_active: cfg.agent.max_active_workflow_runs.max(1),
            command_confirm: cfg.agent.workflow_command_confirm,
        },
    );
    tools.register(Box::new(
        rclaw_runtime::agent_tools::CreateWorkflowTool::new(workflow_service.clone()),
    ));
    tools.register(Box::new(
        rclaw_runtime::agent_tools::AmendWorkflowTool::new(workflow_service.clone()),
    ));
    tools.register(Box::new(
        rclaw_runtime::agent_tools::ResumeWorkflowRunTool::new(workflow_service.clone()),
    ));
    tools.register(Box::new(
        rclaw_runtime::agent_tools::ListWorkflowRunsTool::new(workflow_service.clone()),
    ));
    tools.register(Box::new(
        rclaw_runtime::agent_tools::GetWorkflowRunTool::new(workflow_service.clone()),
    ));
    tools.register(Box::new(
        rclaw_runtime::agent_tools::CancelWorkflowRunTool::new(workflow_service.clone()),
    ));
    tools.register(Box::new(rclaw_runtime::agent_tools::SaveWorkflowTool::new(
        workflow_service.clone(),
    )));
    tools.register(Box::new(
        rclaw_runtime::agent_tools::ListSavedWorkflowsTool::new(workflow_service.clone()),
    ));
    tools.register(Box::new(
        rclaw_runtime::agent_tools::EvalWorkflowSnippetTool::new(workflow_service.clone()),
    ));
    tools.register(Box::new(
        rclaw_runtime::agent_tools::FillWorkflowHoleTool::new(workflow_service.clone()),
    ));
    if cfg.agent.workflow_legacy_tool {
        tools.register(Box::new(rclaw_runtime::agent_tools::WorkflowRunTool::new(
            workflow_service.clone(),
        )));
    }
    // Bot Mode（ADR-0006）：bot 注册表 + message_agent 交接工具。registry 在
    // web_search_control 之后构建（bot 提示需要搜索工具面状态）；此时 Agent/Task
    // 等多 agent 工具还没注册，装配完成后由 set_tool_schemas 回填终态注册快照并
    // 重建 persona；router 由二进制建 inbound 总线后经 bot_router_slot 回填。
    let bots = Arc::new(BotRegistry::new(
        layout,
        provider_factory,
        web_search_control.as_deref(),
        cfg.agent.memory_char_limit,
        cfg.agent.user_char_limit,
    ));
    let bot_router_slot: Arc<OnceLock<Arc<rclaw_runtime::BotRouter>>> = Arc::new(OnceLock::new());
    tools.register(Box::new(MessageAgentTool::new(
        bots.clone(),
        bot_router_slot.clone(),
    )));
    // 保留一个句柄给调用方（AgentToolRuntime 是 Clone 的 Arc 聚合）。
    let agent_runtime_handle = agent_runtime.clone();
    tools.register(Box::new(ClaudeAgentTool::new(
        agent_runtime.clone(),
        "Agent",
    )));
    tools.register(Box::new(ClaudeAgentTool::new(agent_runtime, "Task")));

    // 全部工具注册完毕：回填终态注册快照并重建 bot persona（按注册差异裁剪的
    // 提示文案从此与实际工具面一致）。
    bots.set_tool_schemas(tools.schemas().to_vec());
    bots.reload(web_search_control.as_deref());

    let tools = Arc::new(tools);
    let _ = agent_tools_registry.set(tools.clone());

    // 斜杠命令注册表：/llmodel 持久化到调用方指定的 rclaw.toml 路径。
    let slash = Arc::new({
        let reg = SlashRegistry::new(hot.clone(), slash_config_path, cfg.llm.provider.clone());
        match web_search_control.clone() {
            Some(control) => reg.with_web_search_control(control),
            None => reg,
        }
    });

    // 压缩 token 预算派生：context_length / threshold / tail 未显式配置时按模型窗口派生。
    let (derived_threshold, derived_tail) =
        rclaw_runtime::options::derive_compression_token_budgets(
            cfg.selected_context_length(),
            cfg.llm.max_tokens,
        );
    let derived_summary_ceiling = rclaw_runtime::options::derive_summary_token_ceiling(
        cfg.selected_context_length(),
        cfg.agent.compression_summary_max_tokens as usize,
    );
    let opts = AgentOptions {
        stream_enabled: cfg.llm.stream,
        llm_retry_budget,
        max_concurrent_turns: cfg.agent.max_concurrent_turns,
        context_length: cfg.selected_context_length(),
        session_compression_enabled: cfg.agent.session_compression,
        compression_protect_first: cfg.agent.compression_protect_first,
        compression_keep_recent: cfg.agent.compression_keep_recent,
        compression_threshold_tokens: if cfg.agent.compression_threshold_tokens != 0 {
            cfg.agent.compression_threshold_tokens
        } else {
            derived_threshold
        },
        compression_tail_token_budget: if cfg.agent.compression_tail_token_budget != 0 {
            cfg.agent.compression_tail_token_budget
        } else {
            derived_tail
        },
        compression_tail_min_floor: cfg.agent.compression_tail_min_floor,
        compression_summary_max_tokens: cfg.agent.compression_summary_max_tokens,
        compression_summary_context_ceiling: derived_summary_ceiling,
        auto_memory_enabled: cfg.agent.auto_memory,
        auto_evolve_enabled: cfg.agent.auto_evolve,
        auto_review_tooled: cfg
            .agent
            .auto_review_tooled
            .unwrap_or(cfg.agent.auto_evolve),
        auto_review_max_iters: cfg.agent.auto_review_max_iters,
        auto_review_max_input_tokens: cfg.agent.auto_review_max_input_tokens,
        memory_review_nudge_interval: cfg.agent.memory_review_nudge_interval,
        skill_review_nudge_interval: cfg.agent.skill_review_nudge_interval,
        skill_curator_enabled: cfg.agent.skill_curator,
        skill_curator_interval_hours: cfg.agent.skill_curator_interval_hours,
        skill_stale_after_days: cfg.agent.skill_stale_after_days,
        skill_archive_after_days: cfg.agent.skill_archive_after_days,
        verify_candidate_patches: cfg.agent.verify_candidate_patches,
        goal_audit_enabled: cfg.agent.goal_audit_enabled,
        tool_result_artifact_enabled: cfg.agent.tool_result_artifact_enabled,
        tool_result_artifact_bytes: cfg.agent.tool_result_artifact_bytes,
        daemon,
        ..AgentOptions::default()
    };

    Ok(CoreComponents {
        hot,
        provider,
        session,
        memory,
        layout: layout.clone(),
        tools,
        opts,
        cron,
        slash,
        agent_runtime: agent_runtime_handle,
        bots,
        bot_router_slot,
        review_provider,
        web_search_control,
        _mcp_manager,
    })
}
