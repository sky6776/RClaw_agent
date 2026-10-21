//! rclaw-tools：工具抽象、注册表与内置工具。
//!
//! 工具注册表 + 文件工具 + 命令工具 + Web 工具 + cron 工具。
//! - [`Tool`] trait：统一工具接口（名字、描述、JSON Schema、async execute）。
//! - [`ToolRegistry`]：注册表，产出工具描述（供 LLM）、按名执行。
//! - 内置工具：read/write/edit/list/grep/shell + health-aware web_search /
//!   provider-specific search tools / tavily_extract + 拟人化 CDP 浏览器
//!   browser_* 工具面（cdp_browser）+ cron_add/list/remove。
//!
//! 用 `Box<dyn Tool>` 而非静态数组 + 函数指针，便于扩展。
//! 工具描述内部沿用 Anthropic 风格 `input_schema`，由 LLM 协议层翻译为具体 wire。

pub mod access;
pub mod cdp_browser;
pub mod cron_controller;
pub mod cron_tools;
pub mod download_media;
pub mod files;
pub mod mcp;
pub mod memory_tool;
pub mod session_search;
pub mod shell;
pub mod shell_jobs;
pub mod skill_entrypoint;
pub mod skill_ledger;
pub mod skill_manage;
pub mod skill_signals;
pub mod skill_usage;
pub mod truncate;
pub mod turn_state;
pub mod web_search;

pub use cron_controller::CronController;
pub use files::{DeleteFile, EditFile, Grep, ListDir, Read, WriteFile};
pub use mcp::{
    mcp_access_allows, McpClient, McpConfig, McpError, McpManager, McpResolved, McpTool,
    McpToolTarget, McpToolWrapper,
};
pub use memory_tool::MemoryTool;
pub use session_search::SessionSearchTool;
pub use shell::Shell;
pub use shell_jobs::ShellPoll;
pub use skill_entrypoint::{
    clear_python_script_executor, clear_skill_output_limit, clear_skill_script_change_hook,
    load_skill_entrypoints, set_python_script_executor, set_skill_output_limit,
    set_skill_script_change_hook, PythonResolver, PythonScriptExecutor, PythonScriptRequest,
    ScriptFailure, ScriptOutcome, SkillEntrypointTool, SkillScriptChangeHook,
};
pub use skill_ledger::SkillLedger;
pub use skill_manage::{SkillListTool, SkillManageTool, SkillReadTool, WriteOrigin};
pub use skill_signals::{
    classify_failure_signal, looks_like_user_correction, recent_signals, record_signal,
    signal_digest_for_review, SkillSignal,
};
pub use skill_usage::{
    begin_skill_activity, CuratorConfig, CuratorReport, SkillActivityGuard, SkillCurator,
    SkillLifecycleRecord, SkillOwner, SkillState, SkillUsageStore,
};
pub use truncate::{head_tail_truncate, HeadTailBuffer, HeadTailOutput};
pub use web_search::{
    BackendError, BackendErrorKind, BackendId, BackendStatus, DoubaoBackend, DoubaoSearchConfig,
    DoubaoVariant, HealthStatus, ProviderSearchTool, ReadResponse, ReaderBackend, SearchBackend,
    SearchHit, SearchRequest, SearchResponse, TavilyClient, TavilyExtract, TavilySearchConfig,
    WebSearch, WebSearchConfig, WebSearchControl, ZhipuBackend, ZhipuSearchConfig,
};

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use rclaw_core::ToolSchema;

/// 工具执行结果。
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    pub attachments: Vec<rclaw_core::ContentBlock>,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        ToolResult {
            content: content.into(),
            is_error: false,
            attachments: Vec::new(),
        }
    }

    pub fn ok_with_attachments(
        content: impl Into<String>,
        attachments: Vec<rclaw_core::ContentBlock>,
    ) -> Self {
        ToolResult {
            content: content.into(),
            is_error: false,
            attachments,
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        ToolResult {
            content: content.into(),
            is_error: true,
            attachments: Vec::new(),
        }
    }
}

/// 工具统一接口。
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema（Anthropic 风格 `input_schema`）。
    fn input_schema(&self) -> serde_json::Value;

    /// 执行工具，入参为已解析的 JSON 对象。
    async fn execute(&self, input: serde_json::Value) -> ToolResult;

    /// 在每个 turn 开始前调用，重置工具的 per-turn 状态。
    ///
    /// 默认无操作；持有 per-turn 状态的工具（如 `memory` 的同 turn 失败熔断计数）
    /// 自行覆盖。由 [`ToolRegistry::reset_turn_state`] 在 ReAct 循环顶部统一调用。
    fn reset_turn_state(&self) {}

    /// 本工具与同批其它工具并发执行是否安全（无共享可变状态、无副作用顺序要求）。
    ///
    /// 默认 `false`（保守串行）：写文件、shell、cron、多 agent 等有副作用或持有
    /// 可变状态的工具不得覆盖。只读类工具（read / list / grep / web_search 家族）
    /// 覆盖为 `true`。执行层规则：一批工具调用【混入任一】非 parallel_safe 工具
    /// → 整批串行；全部 parallel_safe → 并发执行、按序回填槽位。
    fn parallel_safe(&self) -> bool {
        false
    }

    /// 组装成规范化工具描述（供注册表批量生成 tools JSON）。
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}

/// 工具注册表。
pub struct ToolRegistry {
    tools: std::collections::HashMap<String, Box<dyn Tool>>,
    schemas: Vec<ToolSchema>,
    /// 用户禁用的工具（运行期开关；进入 LLM 工具面与执行都被拦截）。
    disabled: std::sync::RwLock<std::collections::HashSet<String>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        ToolRegistry {
            tools: std::collections::HashMap::new(),
            schemas: Vec::new(),
            disabled: std::sync::RwLock::new(std::collections::HashSet::new()),
        }
    }

    /// 注册一个工具
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            tracing::warn!("工具 {name} 已注册，覆盖旧实现与旧 schema（保持两张表一致）");
            self.schemas.retain(|s| s.name != name);
        }
        self.schemas.push(tool.schema());
        tracing::info!("注册工具: {name}");
        self.tools.insert(name, tool);
    }

    /// 物理移除一个已注册工具（schema 与执行面同步消失）。
    ///
    /// 供嵌入宿主在装配期摘掉平台不适用的内置工具（如 APK 摘 `shell`）；
    /// 桌面 / Termux 不调用，行为不变。未注册的名字是 no-op。
    pub fn remove(&mut self, name: &str) {
        if self.tools.remove(name).is_some() {
            self.schemas.retain(|s| s.name != name);
            if let Ok(mut disabled) = self.disabled.write() {
                disabled.remove(name);
            }
            tracing::info!("移除工具: {name}");
        }
    }

    /// 全部工具的 schema（供 LLM 请求的 tools 字段）。
    pub fn schemas(&self) -> &[ToolSchema] {
        &self.schemas
    }

    /// 发送给 LLM 的工具面：`schemas()` 滤除用户禁用的工具。
    ///
    /// 每次 turn 开始时调用方重新取用，因此运行期 [`Self::set_enabled`]
    /// 的变更从下一个 turn 起生效。
    pub fn request_schemas(&self) -> Vec<ToolSchema> {
        let disabled = self.disabled.read().expect("registry.disabled 锁未被毒化");
        self.schemas
            .iter()
            .filter(|s| !disabled.contains(&s.name))
            .cloned()
            .collect()
    }

    /// 启用 / 禁用工具；返回该工具是否存在。
    ///
    /// 禁用立即拦截执行层，并从下一个 turn 起移出 LLM 工具面。
    pub fn set_enabled(&self, name: &str, enabled: bool) -> bool {
        if !self.tools.contains_key(name) {
            return false;
        }
        let mut disabled = self.disabled.write().expect("registry.disabled 锁未被毒化");
        if enabled {
            disabled.remove(name);
        } else {
            disabled.insert(name.to_string());
        }
        true
    }

    /// 工具当前是否启用（未知工具返回 false）。
    pub fn is_enabled(&self, name: &str) -> bool {
        self.tools.contains_key(name)
            && !self
                .disabled
                .read()
                .expect("registry.disabled 锁未被毒化")
                .contains(name)
    }

    /// 按名查询工具是否可并发执行；未知工具一律 `false`（保守串行）。
    pub fn is_parallel_safe(&self, name: &str) -> bool {
        self.tools.get(name).is_some_and(|t| t.parallel_safe())
    }

    /// 按名执行工具；未知工具或已被用户禁用的工具返回错误结果。
    pub async fn execute(&self, name: &str, input: serde_json::Value) -> ToolResult {
        if !self.is_enabled(name) {
            return ToolResult::err(format!(
                "工具 {name} 不存在或已被用户禁用；请改用其它方式完成任务"
            ));
        }
        match self.tools.get(name) {
            Some(t) => t.execute(input).await,
            None => ToolResult::err(format!("未知工具: {name}")),
        }
    }

    /// 在每个 turn 开始前重置所有工具的 per-turn 状态（如 memory 工具的同 turn 失败熔断计数）。
    ///
    /// 由 ReAct 循环顶部调用，
    /// 保证 per-turn 熔断计数不会跨 turn 累积。默认实现为空操作，仅持有 per-turn 状态
    /// 的工具会覆盖 [`Tool::reset_turn_state`]。
    pub fn reset_turn_state(&self) {
        for t in self.tools.values() {
            t.reset_turn_state();
        }
    }

    /// 用默认内置工具装配一个注册表。
    ///
    /// - `base`：agent 存储根目录（路径校验基准）。
    /// - `cron`：共享的 cron 控制器（cron 工具与运行时调度共用同一份任务表）。
    pub fn with_defaults(base: PathBuf, cron: Arc<CronController>) -> Self {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(Read::new(base.clone())));
        reg.register(Box::new(WriteFile::new(base.clone())));
        reg.register(Box::new(EditFile::new(base.clone())));
        reg.register(Box::new(DeleteFile::new(base.clone())));
        reg.register(Box::new(ListDir::new(base.clone())));
        reg.register(Box::new(Grep::new(base.clone())));
        // 远程媒体下载进 media/ 沙箱（聊天卡片渲染 / open_media / publish_media 的素材入口）。
        reg.register(Box::new(download_media::DownloadMedia::new(base.clone())));
        // shell 与 shell_poll 共享同一张后台任务表（T1：yield + id + poll）。
        let jobs = shell_jobs::ShellJobTable::new();
        reg.register(Box::new(Shell::with_job_table(base.clone(), jobs.clone())));
        reg.register(Box::new(shell_jobs::ShellPoll::with_table(
            jobs,
            base.join(shell::JOBS_DIR_NAME),
        )));
        reg.register(Box::new(cron_tools::CronAdd::new(cron.clone())));
        reg.register(Box::new(cron_tools::CronList::new(cron.clone())));
        reg.register(Box::new(cron_tools::CronRemove::new(cron)));
        reg
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Crate 版本(关于页「版本信息」用)。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
