//! 配置加载：`rclaw.toml`。
//!
//! 优先级：`rclaw.toml` > 编译期默认。
//! DeepSeek、智谱等 OpenAI 兼容 provider 只需在 `[[llm.providers]]`
//! 加一段配置即可接入，无需改协议代码。

use anyhow::{Context, Result};
use secrecy::Secret;
use serde::Deserialize;

use rclaw_channels::{NapCatConfig, WeixinConfig, WxbotConfig};
use rclaw_llm::{AuthScheme, Dialect, MaxTokensKey, ProtocolKind, ProviderConfig, ReasoningField};
pub use rclaw_runtime::options::WorkflowActivation;

// ---------------------------------------------------------------------------
// 文件配置结构
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, Default)]
// P3：拒绝未知字段——rclaw.toml 里出现任何未声明的顶层键（如把 [weixin] 误写成 [weixn]）
// 将直接解析失败，而非被静默忽略。各子节（AgentSection / LlmSection / ProviderEntry /
// CronSection / HeartbeatSection / WeixinSection）同样启用 deny_unknown_fields，保证
// 「拼错字段 → 配置项失效且无提示」这类隐患在启动期就暴露。
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub agent: AgentSection,
    #[serde(default)]
    pub llm: LlmSection,
    #[serde(default)]
    pub weixin: WeixinSection,
    #[serde(default)]
    pub napcat: NapCatSection,
    #[serde(default)]
    pub wxbot: WxbotSection,
    #[serde(default)]
    pub cron: CronSection,
    #[serde(default)]
    pub heartbeat: HeartbeatSection,
    /// 运行期日志控制：控制台/文件各自等级 + 输出独立开关。详见 [`LogSection`]。
    /// 缺省（无 `[log]` 段）= 写文件 info、不输出到控制台（适合常驻 / 守护进程）。
    #[serde(default)]
    pub log: LogSection,
    /// MCP（Model Context Protocol）服务器配置：标量默认值 + `[mcp.servers.<name>]` 表。
    /// 缺省（无 `[mcp]` 段）时不接入任何 MCP 服务器。详见 `rclaw-tools::mcp::config`。
    #[serde(default)]
    pub mcp: rclaw_tools::mcp::config::McpConfig,
    /// 可扩展的 web_search provider pool。缺省关闭。
    #[serde(default)]
    pub web_search: WebSearchSection,
    /// `[skills]` 段：技能入口执行参数。缺省（无该段）时全部取默认值，
    /// 行为与历史版本一致。
    #[serde(default)]
    pub skills: SkillsSection,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSection {
    #[serde(default = "default_base_dir")]
    pub base_dir: String,
    /// 可同时执行的顶层 turn 数量；同一普通会话仍保持串行。缺省 8，0 在运行时按 1 处理。
    #[serde(default = "default_max_concurrent_turns")]
    pub max_concurrent_turns: usize,
    #[serde(default = "default_true")]
    pub session_compression: bool,
    #[serde(default = "default_compression_protect_first")]
    pub compression_protect_first: usize,
    #[serde(default = "default_compression_keep_recent")]
    pub compression_keep_recent: usize,
    #[serde(default = "default_compression_threshold_tokens")]
    pub compression_threshold_tokens: usize,
    #[serde(default = "default_compression_tail_token_budget")]
    pub compression_tail_token_budget: usize,
    #[serde(default = "default_compression_tail_min_floor")]
    pub compression_tail_min_floor: usize,
    #[serde(default = "default_compression_summary_max_tokens")]
    pub compression_summary_max_tokens: u32,
    #[serde(default = "default_false")]
    pub auto_memory: bool,
    #[serde(default = "default_false")]
    pub auto_evolve: bool,
    /// 用带工具的 fork agent 做后台 review。
    ///
    /// 三态语义：缺省 `None` = 由 [`AgentSection::auto_evolve`] 派生——
    /// `auto_evolve=true` 时**默认强制走 tooled review**（`skills_list`/`read`/`skill_manage`
    /// fork，把自进化建议落地为真实技能改动，而非只写进 MEMORY.md）；`auto_evolve=false`
    /// 时沿用单次无工具 JSON 提取。显式写 `true`/`false` 则覆盖该派生——旧的无工具 JSON
    /// 路径仅作为**显式 legacy 模式**保留（`auto_review_tooled = false`）。
    #[serde(default)]
    pub auto_review_tooled: Option<bool>,
    /// 带工具 review 的最大工具迭代数。缺省 32。
    #[serde(default = "default_auto_review_max_iters")]
    pub auto_review_max_iters: usize,
    /// 带工具 review 的聚合输入 token 预算（跨迭代累计）。缺省 600_000；0 = 不限。
    #[serde(default = "default_auto_review_max_input_tokens")]
    pub auto_review_max_input_tokens: u64,
    /// 自动记忆 review 的 nudge 间隔：按成功用户 turn 计数。缺省 10。
    #[serde(default = "default_memory_review_nudge_interval")]
    pub memory_review_nudge_interval: u32,
    /// 自动自进化 review 的 nudge 间隔：按 ReAct/API 迭代计数。缺省 10。
    #[serde(default = "default_skill_review_nudge_interval")]
    pub skill_review_nudge_interval: u32,
    /// Deterministic Skill Curator (usage-based active/stale/archive lifecycle).
    #[serde(default = "default_true")]
    pub skill_curator: bool,
    #[serde(default = "default_skill_curator_interval_hours")]
    pub skill_curator_interval_hours: u64,
    #[serde(default = "default_skill_stale_after_days")]
    pub skill_stale_after_days: u64,
    #[serde(default = "default_skill_archive_after_days")]
    pub skill_archive_after_days: u64,
    /// candidate_patch 编码子代理完成后跑一次只读交叉验证（read/list/grep mini 循环
    /// 核实结构化交接，结论附在 handoff 后）。默认 false——每个编码子代理最多多出
    /// 4 次小推理，追求交付质量时开启。
    #[serde(default = "default_false")]
    pub verify_candidate_patches: bool,
    /// MEMORY.md 总字符上限（含 legacy / 高层工作方式的 `[自进化]` 条目）。超限写入被拒，
    /// 迫使模型 replace/remove 旧条目。
    /// 缺省 2200（≈800 token）。
    #[serde(default = "default_memory_char_limit")]
    pub memory_char_limit: usize,
    /// USER.md 总字符上限。缺省 1375（≈500 token）。
    #[serde(default = "default_user_char_limit")]
    pub user_char_limit: usize,
    /// `/goal` 目标的完成审计：有活动目标的会话成功收尾后，用一次无工具的小推理核对
    /// 汇报证据，确认达成即自动清除目标。缺省开启；关闭后只剩提醒 + 手动 `/goal clear`。
    #[serde(default = "default_true")]
    pub goal_audit_enabled: bool,
    /// 超长工具结果落盘为 artifact（`agent/artifacts/<会话键>/`），模型只看首尾预览 +
    /// 文件路径。缺省开启；关闭后工具结果按原有截断口径进上下文。
    #[serde(default = "default_true")]
    pub tool_result_artifact_enabled: bool,
    /// 工具结果落盘阈值（字节）：超过即落盘。缺省 32768（32 KiB）。
    #[serde(default = "default_tool_result_artifact_bytes")]
    pub tool_result_artifact_bytes: usize,
    /// 工作流后台执行（默认 true）。false = 同步契约（Create 等调用返回时已终态），
    /// 但仍写 v3 投影表与快照。
    #[serde(default = "default_true")]
    pub workflow_background: bool,
    /// 是否注册 deprecated 的 `WorkflowRun` 旧工具（默认 false；仅工具面回滚用）。
    #[serde(default = "default_false")]
    pub workflow_legacy_tool: bool,
    /// 同时持有执行许可的工作流 run 上限（桌面默认 4、Android 默认 2）。
    #[serde(default = "default_max_active_workflow_runs")]
    pub max_active_workflow_runs: usize,
    /// `world.run` 是否逐命令确认（P5；默认 false = 仅 ToolAccess，与 shell 同权）。
    #[serde(default = "default_false")]
    pub workflow_command_confirm: bool,
    /// 动态工作流的启用方式：`"command"` = 只能由 `/workflow` 命令触发；`"always"`（默认）=
    /// 命令与对话里的明确要求都启用，模型也可按用户意图主动启动。非法取值在解析期拒绝。
    #[serde(default)]
    pub workflow_activation: WorkflowActivation,
}
impl Default for AgentSection {
    fn default() -> Self {
        AgentSection {
            base_dir: default_base_dir(),
            max_concurrent_turns: default_max_concurrent_turns(),
            session_compression: true,
            compression_protect_first: default_compression_protect_first(),
            compression_keep_recent: default_compression_keep_recent(),
            compression_threshold_tokens: default_compression_threshold_tokens(),
            compression_tail_token_budget: default_compression_tail_token_budget(),
            compression_tail_min_floor: default_compression_tail_min_floor(),
            compression_summary_max_tokens: default_compression_summary_max_tokens(),
            auto_memory: false,
            auto_evolve: false,
            auto_review_tooled: None,
            auto_review_max_iters: default_auto_review_max_iters(),
            auto_review_max_input_tokens: default_auto_review_max_input_tokens(),
            memory_review_nudge_interval: default_memory_review_nudge_interval(),
            skill_review_nudge_interval: default_skill_review_nudge_interval(),
            skill_curator: true,
            skill_curator_interval_hours: default_skill_curator_interval_hours(),
            skill_stale_after_days: default_skill_stale_after_days(),
            skill_archive_after_days: default_skill_archive_after_days(),
            verify_candidate_patches: false,
            memory_char_limit: default_memory_char_limit(),
            user_char_limit: default_user_char_limit(),
            goal_audit_enabled: true,
            tool_result_artifact_enabled: true,
            tool_result_artifact_bytes: default_tool_result_artifact_bytes(),
            workflow_background: true,
            workflow_legacy_tool: false,
            max_active_workflow_runs: default_max_active_workflow_runs(),
            workflow_command_confirm: false,
            workflow_activation: WorkflowActivation::default(),
        }
    }
}
fn default_base_dir() -> String {
    "agent".into()
}
fn default_max_concurrent_turns() -> usize {
    8
}
fn default_tool_result_artifact_bytes() -> usize {
    32 * 1024
}
fn default_max_active_workflow_runs() -> usize {
    if cfg!(target_os = "android") {
        2
    } else {
        4
    }
}
fn default_compression_protect_first() -> usize {
    3
}
fn default_compression_keep_recent() -> usize {
    80
}
/// 0 = 未显式配置 → 由 `context_length` 派生（见 [`Config::selected_context_length`] 与
/// `rclaw_runtime::options::derive_compression_token_budgets`），使 token 触发条件默认生效。
fn default_compression_threshold_tokens() -> usize {
    0
}
/// 0 = 未显式配置 → 与 token threshold 一起由 `context_length` 派生。
fn default_compression_tail_token_budget() -> usize {
    0
}
fn default_compression_tail_min_floor() -> usize {
    8
}
/// 压缩摘要输出 token 的【绝对上限】默认值：12_000。
///
/// 实际摘要 max_tokens 由 runtime 的 `rclaw_runtime::summary_max_tokens` 按「被压缩内容 × 20%」
/// 动态计算，夹到 `[2000, 本上限]`（且受上下文相关上限与 provider 上限约束）后再 ×1.3——大上下文
/// 压缩时给摘要更多空间、尽量保真。旧默认 2048 会过早截断大上下文摘要并丢失细节。
/// 被 `AgentSection` 的 serde default 与 manual `Default` impl 共用，改这一处即同时覆盖缺省路径。
fn default_compression_summary_max_tokens() -> u32 {
    12_000
}
fn default_auto_review_max_input_tokens() -> u64 {
    600_000
}

fn default_auto_review_max_iters() -> usize {
    32
}
fn default_memory_review_nudge_interval() -> u32 {
    10
}
fn default_skill_review_nudge_interval() -> u32 {
    10
}
fn default_skill_curator_interval_hours() -> u64 {
    168
}
fn default_skill_stale_after_days() -> u64 {
    14
}
fn default_skill_archive_after_days() -> u64 {
    30
}
fn default_memory_char_limit() -> usize {
    2_200
}
fn default_user_char_limit() -> usize {
    1_375
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmSection {
    /// 选中的 provider 名（对应 [[llm.providers]].name）。
    #[serde(default = "default_provider")]
    pub provider: String,
    /// 可覆盖 provider 的默认模型。
    pub model: Option<String>,
    #[serde(default = "default_true")]
    pub stream: bool,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// 单次 LLM HTTP 请求总超时（秒）。深度推理和大工具上下文可能超过两分钟。
    #[serde(default = "default_llm_request_timeout_secs")]
    pub request_timeout_secs: u64,
    /// 单轮允许的最大工具调用数（默认 256）。
    ///
    /// 这是【解析端安全阀】：模型一轮内并行发起的工具调用超过此数即拒绝整轮，
    /// 避免静默截断工具链（截断会让模型预期 N 个结果却只收到 <N 个，破坏配对）。
    /// 批量任务（如一次导入多个文档）若超过此值，可在 `[llm] max_tool_calls` 调大。
    /// 注意：该值不随请求发给模型，仅本地约束解析。
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: usize,
    #[serde(default)]
    pub providers: Vec<ProviderEntry>,
    /// 评审专用辅助 provider 名（对应 [[llm.providers]].name，缺省用主 provider）。
    ///
    /// 后台自进化评审是高频低难度任务（摘要 / 对比 / 小补丁），可指向更便宜的
    /// 模型以省成本；路由到异构模型时评审历史自动改为 digest 重放（无前缀缓存
    /// 收益，全量重放纯浪费）。填未定义的名字会在启动期报错。
    pub review_provider: Option<String>,
}

impl Default for LlmSection {
    fn default() -> Self {
        LlmSection {
            provider: default_provider(),
            model: None,
            stream: default_true(),
            max_tokens: default_max_tokens(),
            request_timeout_secs: default_llm_request_timeout_secs(),
            max_tool_calls: default_max_tool_calls(),
            providers: Vec::new(),
            review_provider: None,
        }
    }
}
fn default_provider() -> String {
    "deepseek".into()
}
fn default_true() -> bool {
    true
}
fn default_false() -> bool {
    false
}
fn default_max_tokens() -> u32 {
    4096
}
fn default_llm_request_timeout_secs() -> u64 {
    300
}
/// 单轮最大工具调用数默认值。
///
/// 批量多 Agent 任务可能在同一响应中发起数十个调用；默认与 Agent 迭代上限统一为
/// 256，避免解析端先于执行器拒绝合法工具链。该值仍只是本地解析安全阀，可在
/// rclaw.toml 的 `[llm] max_tool_calls` 显式覆盖。
fn default_max_tool_calls() -> usize {
    256
}

#[derive(Debug, Deserialize, Clone)]
// P3：拒绝未知字段——例如把 supports_tool_choice 误写成 supports_tool_choices 会被
// 静默忽略并回退默认 true，导致「关闭 tool_choice」的配置失效（这正是上一轮修复的能力）。
// 启用后拼错字段会在启动期直接报错。
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub name: String,
    pub base_url: String,
    pub model: Option<String>,
    /// 该 provider 的 API key。直接从配置文件读取（不再从环境变量获取）。
    #[serde(default)]
    pub api_key: Option<String>,
    /// "bearer" 或 "apikey:<header>"。
    #[serde(default = "default_auth")]
    pub auth: String,
    #[serde(default = "default_reasoning")]
    pub reasoning_field: String,
    #[serde(default = "default_max_tokens_key")]
    pub max_tokens_key: String,
    /// 该 provider 是否支持 `tool_choice` 字段（默认 true）。
    ///
    /// **修复 P2「Provider 配置无法关闭 tool_choice」**：`Dialect` 早已支持 `supports_tool_choice`，
    /// 但旧实现对配置文件 provider 一律硬编码为 `true`；而 `openai_compat` 在 tools 存在时会
    /// 发送 `tool_choice`——部分 OpenAI 兼容服务会拒绝该字段，用户却无法通过 rclaw.toml 关闭。
    /// 现在把它做成可配置字段（默认 true 保持兼容），并在 `build_provider_config` 透传到 Dialect。
    #[serde(default = "default_true")]
    pub supports_tool_choice: bool,
    /// 该 provider/模型的上下文窗口大小（token 数）。
    ///
    /// 用于按 `context_length` 派生会话压缩的 token 触发阈值与尾窗预算。缺省 `0` = 未声明，
    /// 按内置假设窗口（128K）派生——这会让 token 触发条件「默认生效」。在
    /// `[[llm.providers]]` 里按模型实际窗口填写（如 DeepSeek 128K = 131072）可得最准确的预算。
    #[serde(default)]
    pub context_length: usize,
}
fn default_auth() -> String {
    "bearer".into()
}
fn default_reasoning() -> String {
    "reasoning_content".into()
}
fn default_max_tokens_key() -> String {
    "max_tokens".into()
}

/// `[web_search]` 及其 provider 子表。provider 表存在即代表已配置；`enabled=false`
/// 只在运行时跳过，仍会构建客户端，以便 `/search en <provider>` 无需重启即可恢复。
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchSection {
    #[serde(default = "default_false")]
    pub enabled: bool,
    /// 有序搜索链。空数组时按 doubao-custom → tavily → zhipu → doubao-global
    /// 过滤出已配置的 provider。
    #[serde(default)]
    pub order: Vec<String>,
    #[serde(default = "default_true")]
    pub fallback: bool,
    #[serde(default = "default_web_search_read_top")]
    pub read_top: usize,
    #[serde(default = "default_web_search_max_results")]
    pub max_results: usize,
    #[serde(default = "default_web_search_total_timeout")]
    pub total_timeout_secs: u64,
    #[serde(default = "default_web_search_circuit_threshold")]
    pub circuit_failure_threshold: usize,
    #[serde(default = "default_web_search_circuit_cooldown")]
    pub circuit_cooldown_secs: u64,
    #[serde(default)]
    pub zhipu: Option<ZhipuSearchSection>,
    #[serde(default)]
    pub tavily: Option<TavilySearchSection>,
    #[serde(default)]
    pub doubao_custom: Option<DoubaoSearchSection>,
    #[serde(default)]
    pub doubao_global: Option<DoubaoSearchSection>,
}

impl std::fmt::Debug for WebSearchSection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebSearchSection")
            .field("enabled", &self.enabled)
            .field("order", &self.order)
            .field("fallback", &self.fallback)
            .field("read_top", &self.read_top)
            .field("max_results", &self.max_results)
            .field("total_timeout_secs", &self.total_timeout_secs)
            .field("circuit_failure_threshold", &self.circuit_failure_threshold)
            .field("circuit_cooldown_secs", &self.circuit_cooldown_secs)
            .field("zhipu_configured", &self.zhipu.is_some())
            .field("tavily_configured", &self.tavily.is_some())
            .field("doubao_custom_configured", &self.doubao_custom.is_some())
            .field("doubao_global_configured", &self.doubao_global.is_some())
            .finish()
    }
}

impl Default for WebSearchSection {
    fn default() -> Self {
        WebSearchSection {
            enabled: false,
            order: Vec::new(),
            fallback: true,
            read_top: default_web_search_read_top(),
            max_results: default_web_search_max_results(),
            total_timeout_secs: default_web_search_total_timeout(),
            circuit_failure_threshold: default_web_search_circuit_threshold(),
            circuit_cooldown_secs: default_web_search_circuit_cooldown(),
            zhipu: None,
            tavily: None,
            doubao_custom: None,
            doubao_global: None,
        }
    }
}

fn default_web_search_read_top() -> usize {
    1
}
fn default_web_search_max_results() -> usize {
    10
}
fn default_web_search_total_timeout() -> u64 {
    25
}
fn default_web_search_circuit_threshold() -> usize {
    3
}
fn default_web_search_circuit_cooldown() -> u64 {
    60
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZhipuSearchSection {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub api_key: String,
    #[serde(default = "default_zhipu_search_url")]
    pub search_url: String,
    #[serde(default = "default_zhipu_reader_url")]
    pub reader_url: String,
    #[serde(default = "default_zhipu_startup_timeout")]
    pub startup_timeout_secs: u64,
    #[serde(default = "default_zhipu_call_timeout")]
    pub call_timeout_secs: u64,
}

fn default_zhipu_search_url() -> String {
    "https://open.bigmodel.cn/api/mcp/web_search_prime/mcp".into()
}
fn default_zhipu_reader_url() -> String {
    "https://open.bigmodel.cn/api/mcp/web_reader/mcp".into()
}
fn default_zhipu_startup_timeout() -> u64 {
    10
}
fn default_zhipu_call_timeout() -> u64 {
    10
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TavilySearchSection {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub api_keys: Vec<String>,
    #[serde(default = "default_tavily_base_url")]
    pub base_url: String,
    #[serde(default = "default_tavily_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_tavily_max_results")]
    pub max_results: usize,
    #[serde(default = "default_tavily_depth")]
    pub search_depth: String,
    #[serde(default)]
    pub include_answer: bool,
}

impl Default for TavilySearchSection {
    fn default() -> Self {
        TavilySearchSection {
            enabled: true,
            api_keys: Vec::new(),
            base_url: default_tavily_base_url(),
            timeout_secs: default_tavily_timeout(),
            max_results: default_tavily_max_results(),
            search_depth: default_tavily_depth(),
            include_answer: false,
        }
    }
}

fn default_tavily_base_url() -> String {
    "https://api.tavily.com".into()
}
fn default_tavily_timeout() -> u64 {
    20
}
fn default_tavily_max_results() -> usize {
    5
}
fn default_tavily_depth() -> String {
    "basic".into()
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoubaoSearchSection {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub api_key: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default = "default_doubao_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub max_results: Option<usize>,
    /// 仅豆包 Global 支持；Custom 子表显式配置会在启动期报错。
    #[serde(default)]
    pub max_snippet_tokens: Option<usize>,
}

fn default_doubao_timeout() -> u64 {
    8
}

/// 启动期完成校验后的 web_search 配置。API Key 仅向各后端构造函数传递，
/// 不实现 `Debug`，避免被启动日志意外展开。
pub struct WebSearchRuntimeConfig {
    pub tool: rclaw_tools::WebSearchConfig,
    pub order: Vec<rclaw_tools::BackendId>,
    pub enabled: Vec<(rclaw_tools::BackendId, bool)>,
    pub fallback: bool,
    pub circuit_failure_threshold: usize,
    pub circuit_cooldown_secs: u64,
    pub zhipu: Option<rclaw_tools::ZhipuSearchConfig>,
    pub tavily: Option<rclaw_tools::TavilySearchConfig>,
    pub doubao_custom: Option<rclaw_tools::DoubaoSearchConfig>,
    pub doubao_global: Option<rclaw_tools::DoubaoSearchConfig>,
}

#[derive(Clone, Debug, Deserialize)]
// deny_unknown_fields：修复 P3「Weixin 重试相关字段不可从 TOML 配置」的配套防御。
// 现已把 WeixinConfig 的全部运行时调优项都暴露到本结构（见下方字段），任何【未识别】的
// TOML 键（如把 retry_sec 误写成 retry_secs）将直接解析失败，而非被静默忽略。
#[serde(deny_unknown_fields)]
pub struct WeixinSection {
    #[serde(default)]
    pub enabled: Option<bool>,
    pub token: Option<String>,
    pub allow_from: Option<String>,
    #[serde(default = "default_weixin_base")]
    pub api_base: String,
    /// 媒体 CDN 基座（图片/文件/视频上传下载）。默认官方 novac2c CDN。
    #[serde(default = "default_weixin_cdn_base")]
    pub cdn_base: String,
    /// 单分片最大字节数（UTF-8 安全分段）。下限保护在 [`Config::weixin_channel_config`] 中做（>=1）。
    #[serde(default = "default_chunk")]
    pub chunk_bytes: usize,
    /// 长轮询单次超时（秒）。对应 [`WeixinConfig::poll_timeout_sec`]。
    #[serde(default = "default_weixin_poll_timeout")]
    pub poll_timeout_sec: u64,
    /// 两次重试之间的基础间隔（秒）。对应 [`WeixinConfig::retry_sec`]。
    #[serde(default = "default_weixin_retry")]
    pub retry_sec: u64,
    /// 连续失败达阈值后的退避时长（秒）。对应 [`WeixinConfig::backoff_sec`]。
    #[serde(default = "default_weixin_backoff")]
    pub backoff_sec: u64,
    /// 单分片最大重试次数 / 连续失败阈值。对应 [`WeixinConfig::max_failures`]。
    /// 下限保护在 [`Config::weixin_channel_config`] 中做（>=1）。
    #[serde(default = "default_weixin_max_failures")]
    pub max_failures: u32,
}
impl Default for WeixinSection {
    fn default() -> Self {
        WeixinSection {
            enabled: None,
            token: None,
            allow_from: None,
            api_base: default_weixin_base(),
            cdn_base: default_weixin_cdn_base(),
            chunk_bytes: default_chunk(),
            poll_timeout_sec: default_weixin_poll_timeout(),
            retry_sec: default_weixin_retry(),
            backoff_sec: default_weixin_backoff(),
            max_failures: default_weixin_max_failures(),
        }
    }
}
fn default_weixin_base() -> String {
    "https://ilinkai.weixin.qq.com".into()
}
fn default_weixin_cdn_base() -> String {
    "https://novac2c.cdn.weixin.qq.com/c2c".into()
}
fn default_chunk() -> usize {
    2000
}
// 微信调优项默认值——与 [`WeixinConfig::default`] 保持一致，保证「未在 TOML 显式配置」时
// 行为与旧实现（恒用 Default）完全相同，不引入回归。
fn default_weixin_poll_timeout() -> u64 {
    35
}
fn default_weixin_retry() -> u64 {
    2
}
fn default_weixin_backoff() -> u64 {
    30
}
fn default_weixin_max_failures() -> u32 {
    3
}

// ---------------------------------------------------------------------------
// wxbot（VA + WCX 容器内个人微信）
// ---------------------------------------------------------------------------

/// `[wxbot]` 段：个人微信通道配置（VA + WCX 容器内微信，经本机 REST/webhook 交互）。
/// 与 [`WeixinSection`] 一样启用 deny_unknown_fields，拼错字段（如把 api_token 误写成
/// apitoken）将在启动期直接解析失败，而非静默忽略。设计契约见 docs/wxbot-channel.md。
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WxbotSection {
    #[serde(default)]
    pub enabled: Option<bool>,
    /// wcx ApiServer 基址（必须 loopback：127.0.0.1 / [::1] / localhost）。
    #[serde(default = "default_wxbot_api_base")]
    pub api_base: String,
    /// wcx「API + MCP 服务器」的认证令牌；启用时必须非空。
    pub api_token: Option<String>,
    /// webhook 监听地址（必须 loopback）。
    #[serde(default = "default_wxbot_webhook_listen")]
    pub webhook_listen: String,
    #[serde(default = "default_wxbot_webhook_path")]
    pub webhook_path: String,
    /// webhook secret（与 wcx WebhookPush 一致）；启用时必须非空。
    pub webhook_secret: Option<String>,
    /// 逗号分隔的允许私聊 wxid；空 = 放行全部。
    pub allow_from: Option<String>,
    /// 逗号分隔的允许群 id（xxx@chatroom）；空 = 所有群不响应。
    pub allow_groups: Option<String>,
    /// 群消息触发策略：mention(仅精确 @ 本账号) / all。
    #[serde(default = "default_wxbot_group_reply_policy")]
    pub group_reply_policy: String,
    /// 兜底长轮询开关（webhook 主路之外的 fallback）。
    #[serde(default = "default_wxbot_poll_fallback")]
    pub poll_fallback_enabled: bool,
    /// 长轮询单次超时（秒）。
    #[serde(default = "default_wxbot_poll_timeout")]
    pub poll_timeout_sec: u64,
    /// 两次重试之间的基础间隔（秒）。
    #[serde(default = "default_wxbot_retry")]
    pub retry_sec: u64,
    /// 连续失败达阈值后的退避时长（秒）。
    #[serde(default = "default_wxbot_backoff")]
    pub backoff_sec: u64,
    /// 单分片最大重试次数 / 连续失败阈值。下限保护在 [`Config::wxbot_channel_config`] 中做（>=1）。
    #[serde(default = "default_wxbot_max_failures")]
    pub max_failures: u32,
    /// 单分片最大字节数（UTF-8 安全分段）。下限保护在 [`Config::wxbot_channel_config`] 中做（>=1）。
    #[serde(default = "default_chunk")]
    pub chunk_bytes: usize,
    /// 出站相邻发送基础间隔（秒）。
    #[serde(default = "default_wxbot_send_min_gap")]
    pub send_min_gap_sec: u64,
    /// 出站发送随机抖动上限（秒）。
    #[serde(default = "default_wxbot_send_jitter")]
    pub send_jitter_sec: u64,
    /// 60s 滑动窗口内最大发送条数；0 = 关闭窗口限制。
    #[serde(default = "default_wxbot_send_window_max")]
    pub send_window_max: usize,
    /// 连续发送失败熔断阈值；0 = 关闭熔断。
    #[serde(default = "default_wxbot_send_failure_fuse")]
    pub send_failure_fuse: u32,
}

impl Default for WxbotSection {
    fn default() -> Self {
        WxbotSection {
            enabled: None,
            api_base: default_wxbot_api_base(),
            api_token: None,
            webhook_listen: default_wxbot_webhook_listen(),
            webhook_path: default_wxbot_webhook_path(),
            webhook_secret: None,
            allow_from: None,
            allow_groups: None,
            group_reply_policy: default_wxbot_group_reply_policy(),
            poll_fallback_enabled: default_wxbot_poll_fallback(),
            poll_timeout_sec: default_wxbot_poll_timeout(),
            retry_sec: default_wxbot_retry(),
            backoff_sec: default_wxbot_backoff(),
            max_failures: default_wxbot_max_failures(),
            chunk_bytes: default_chunk(),
            send_min_gap_sec: default_wxbot_send_min_gap(),
            send_jitter_sec: default_wxbot_send_jitter(),
            send_window_max: default_wxbot_send_window_max(),
            send_failure_fuse: default_wxbot_send_failure_fuse(),
        }
    }
}
fn default_wxbot_api_base() -> String {
    "http://127.0.0.1:3001".into()
}
fn default_wxbot_webhook_listen() -> String {
    "127.0.0.1:38081".into()
}
fn default_wxbot_webhook_path() -> String {
    "/wxbot/webhook".into()
}
fn default_wxbot_group_reply_policy() -> String {
    "mention".into()
}
fn default_wxbot_poll_fallback() -> bool {
    true
}
// wxbot 调优项默认值——与 [`WxbotConfig::default`] 保持一致。
fn default_wxbot_poll_timeout() -> u64 {
    60
}
fn default_wxbot_retry() -> u64 {
    2
}
fn default_wxbot_backoff() -> u64 {
    30
}
fn default_wxbot_max_failures() -> u32 {
    3
}
fn default_wxbot_send_min_gap() -> u64 {
    2
}
fn default_wxbot_send_jitter() -> u64 {
    3
}
fn default_wxbot_send_window_max() -> usize {
    10
}
fn default_wxbot_send_failure_fuse() -> u32 {
    5
}

// ---------------------------------------------------------------------------
// NapCatQQ（OneBot 11 over WebSocket）
// ---------------------------------------------------------------------------

/// `[napcat]` 段：NapCatQQ 通道配置。与 [`WeixinSection`] 一样启用 deny_unknown_fields，
/// 拼错字段（如把 ws_url 误写成 wsurl）将在启动期直接解析失败，而非静默忽略。
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NapCatSection {
    #[serde(default)]
    pub enabled: Option<bool>,
    /// NapCat WebSocket 地址，如 `ws://127.0.0.1:3001`。
    pub ws_url: Option<String>,
    /// access_token（与 NapCat 配置一致）；留空表示不鉴权。
    pub access_token: Option<String>,
    /// 逗号分隔的允许用户 QQ，空=放行全部，支持 `*`。
    pub allow_from: Option<String>,
    /// 逗号分隔的允许群号，空=放行全部群，支持 `*`。
    pub allow_groups: Option<String>,
    /// 逗号分隔的拉黑用户 QQ，优先级高于 allow_from，支持 `*`。
    pub block_from: Option<String>,
    /// 逗号分隔的「max 权限」用户 QQ：这些 QQ 的私聊消息可让 read/write/edit/list/grep/shell
    /// 操作 agent 目录外任意路径；其它消息仍被限制在 agent 目录内。空=无人拥有 max 权限，支持 `*`。
    pub max_from: Option<String>,
    /// 群消息响应模式：`none` / `at_only` / `all`。对应 [`NapCatConfig::group_response_mode`]。
    #[serde(default = "default_napcat_group_response_mode")]
    pub group_response_mode: String,
    /// 心跳间隔（秒）。对应 [`NapCatConfig::heartbeat_interval_sec`]。
    #[serde(default = "default_napcat_heartbeat_interval")]
    pub heartbeat_interval_sec: u64,
    /// 重连基础退避（秒，指数翻倍上限 60）。对应 [`NapCatConfig::reconnect_delay_sec`]。
    #[serde(default = "default_napcat_reconnect_delay")]
    pub reconnect_delay_sec: u64,
    /// 单条出站文本的最大字节数（按 UTF-8 边界安全分段）。超过该值的回复改走合并转发卡片。
    /// 对应 [`rclaw_channels::NapCatConfig::send_chunk_bytes`]。
    #[serde(default = "default_napcat_send_chunk_bytes")]
    pub send_chunk_bytes: usize,
}
impl Default for NapCatSection {
    fn default() -> Self {
        NapCatSection {
            enabled: None,
            ws_url: None,
            access_token: None,
            allow_from: None,
            allow_groups: None,
            block_from: None,
            max_from: None,
            group_response_mode: default_napcat_group_response_mode(),
            heartbeat_interval_sec: default_napcat_heartbeat_interval(),
            reconnect_delay_sec: default_napcat_reconnect_delay(),
            send_chunk_bytes: default_napcat_send_chunk_bytes(),
        }
    }
}
fn default_napcat_group_response_mode() -> String {
    "all".into()
}
fn default_napcat_heartbeat_interval() -> u64 {
    30
}
fn default_napcat_reconnect_delay() -> u64 {
    5
}
fn default_napcat_send_chunk_bytes() -> usize {
    NapCatConfig::default().send_chunk_bytes
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CronSection {
    #[serde(default = "default_cron_interval")]
    pub check_interval_sec: u64,
}
impl Default for CronSection {
    fn default() -> Self {
        CronSection {
            check_interval_sec: default_cron_interval(),
        }
    }
}
fn default_cron_interval() -> u64 {
    60
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatSection {
    #[serde(default = "default_heartbeat_interval")]
    pub interval_sec: u64,
}
impl Default for HeartbeatSection {
    fn default() -> Self {
        HeartbeatSection {
            interval_sec: default_heartbeat_interval(),
        }
    }
}
fn default_heartbeat_interval() -> u64 {
    1800
}

// ---------------------------------------------------------------------------
// 技能执行
// ---------------------------------------------------------------------------

/// `[skills]` 段：skill 入口（skill.toml 声明式动态工具）执行参数。
///
/// 缺省（无 `[skills]` 段）时 `max_output_bytes = None`，运行期取
/// `rclaw-tools` 内置默认 64 KiB，与历史行为逐字节一致（Android 移植 D7）。
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillsSection {
    /// 技能 stdout/stderr 单流字节截断上限（两流各自限额）。可选；不写即默认。
    /// 注意真正的硬上限是 LLM 上下文窗口——64 KiB 中文已折合约 3–6 万 token，
    /// 调大只会把截断转嫁给会话压缩并增加成本。
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
}

// ---------------------------------------------------------------------------
// 日志
// ---------------------------------------------------------------------------

/// `[log]` 段：运行期日志控制。
///
/// - `console_level`：控制台日志等级（error/warn/info/debug/trace）。
/// - `file_level`：文件日志等级（error/warn/info/debug/trace）。两者【各自独立】——
///   例如可控制台 debug、文件 info。
/// - `console`：是否输出到控制台（stdout）。
/// - `file`：是否输出到文件（按日轮转，写入 `dir/file_name.YYYY-MM-DD`）。
///
/// `console` 与 `file` 互相独立——四种组合（仅控制台 / 仅文件 / 两者都开 / 两者都关）
/// 均合法，开关其一不影响另一个。`RUST_LOG` 环境变量若设置则【同时覆盖】两者（运维逃生通道）。
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogSection {
    /// 控制台日志等级字符串。启动期经 [`parse_log_level`] 校验，拼错会直接 fail-fast。
    #[serde(default = "default_log_level")]
    pub console_level: String,
    /// 文件日志等级字符串。启动期经 [`parse_log_level`] 校验，拼错会直接 fail-fast。
    #[serde(default = "default_log_level")]
    pub file_level: String,
    /// 是否输出到控制台（stdout）。缺省 `false`——默认仅写文件，避免常驻进程刷屏控制台。
    #[serde(default = "default_false")]
    pub console: bool,
    /// 是否输出到文件（按日轮转，写入 `dir/file_name.YYYY-MM-DD`）。缺省 `true`。
    #[serde(default = "default_true")]
    pub file: bool,
    /// 日志文件目录（相对 CWD 或绝对路径）。留空（默认）则写入 agent/logs（与 config/skills 平级）；
    /// 仅 `file = true` 时使用，不存在则自动创建。
    #[serde(default = "default_log_dir")]
    pub dir: String,
    /// 日志文件名前缀；实际按日轮转为 `<file_name>.YYYY-MM-DD`。
    #[serde(default = "default_log_file_name")]
    pub file_name: String,
}

impl Default for LogSection {
    fn default() -> Self {
        LogSection {
            console_level: default_log_level(),
            file_level: default_log_level(),
            // 默认：写文件 info、不输出到控制台。适合常驻 / 守护进程——日志落盘可追溯，
            // 控制台不被刷屏。需要控制台输出时在 [log] 设 console = true。
            console: false,
            file: true,
            dir: default_log_dir(),
            file_name: default_log_file_name(),
        }
    }
}

fn default_log_level() -> String {
    "info".into()
}
fn default_log_dir() -> String {
    // 留空 → init_tracing 回退到 agent/logs（与 config/skills 平级）。
    String::new()
}
fn default_log_file_name() -> String {
    "rclaw.log".into()
}

// ---------------------------------------------------------------------------
// 加载
// ---------------------------------------------------------------------------

/// 已从 schema 整节删除、但旧版本可能写进 `rclaw.toml` 的顶层配置键。
/// 顶层 [`Config`] 启用 `deny_unknown_fields`：这些键若不剥离，旧配置在升级后
/// 会被当「拼错的键」拒绝，应用启动即「解析 rclaw.toml 失败」（实例：v2.8.0
/// 删除 `[web_browser]`，v2.7.x 的配置在其上无法启动）。只收录确认退役的键，
/// 现存键的拼写错误防护不受影响。
pub const REMOVED_LEGACY_TOP_LEVEL_KEYS: &[&str] = &["web_browser"];

/// 剥离 [`REMOVED_LEGACY_TOP_LEVEL_KEYS`] 中的遗留顶层键，返回 (清理后文本,
/// 被移除的键)。经 toml_edit 保真往返：其余内容的注释与格式逐字节不动，
/// 后续严格解析的报错行号仍指向原文。文本本身有语法错误时原样返回——
/// 让严格解析报出原始错误。
pub fn strip_removed_legacy_sections(text: &str) -> (String, Vec<&'static str>) {
    let mut doc = match text.parse::<toml_edit::DocumentMut>() {
        Ok(doc) => doc,
        Err(_) => return (text.to_string(), Vec::new()),
    };
    let mut removed = Vec::new();
    for key in REMOVED_LEGACY_TOP_LEVEL_KEYS.iter().copied() {
        if doc.remove(key).is_some() {
            removed.push(key);
        }
    }
    if removed.is_empty() {
        return (text.to_string(), removed);
    }
    (doc.to_string(), removed)
}

fn parse_config_text(text: &str) -> std::result::Result<Config, toml::de::Error> {
    let normalized = normalize_windows_path_basic_strings(text);
    let (stripped, removed) = strip_removed_legacy_sections(&normalized);
    if !removed.is_empty() {
        tracing::warn!(
            keys = %removed.join(", "),
            "rclaw.toml 含已退役的配置节，解析时忽略（现存键的拼写错误防护不受影响）"
        );
    }
    toml::from_str(&stripped)
}

fn normalize_windows_path_basic_strings(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;

    while i < text.len() {
        let rest = &text[i..];
        if let Some(stripped) = rest.strip_prefix("'''") {
            let end = stripped.find("'''").map(|pos| i + 3 + pos + 3);
            let next = end.unwrap_or(text.len());
            out.push_str(&text[i..next]);
            i = next;
            continue;
        }
        if rest.starts_with('"') {
            if let Some(stripped) = rest.strip_prefix("\"\"\"") {
                let end = stripped.find("\"\"\"").map(|pos| i + 3 + pos + 3);
                let next = end.unwrap_or(text.len());
                out.push_str(&text[i..next]);
                i = next;
                continue;
            }

            let content_start = i + 1;
            let mut j = content_start;
            while j < text.len() {
                let ch = text[j..].chars().next().expect("valid char boundary");
                if ch == '"' {
                    let slashes = preceding_backslashes(text, j);
                    let raw_windows_path_end = slashes % 2 == 1
                        && looks_like_windows_path_literal(&text[content_start..j])
                        && is_value_terminator_after_quote(text, j + 1);
                    if slashes.is_multiple_of(2) || raw_windows_path_end {
                        break;
                    }
                }
                j += ch.len_utf8();
            }

            if j >= text.len() {
                out.push_str(&text[i..]);
                break;
            }

            let content = &text[content_start..j];
            out.push('"');
            if looks_like_windows_path_literal(content) {
                out.push_str(&escape_windows_path_backslashes(content));
            } else {
                out.push_str(content);
            }
            out.push('"');
            i = j + 1;
            continue;
        }
        if let Some(stripped) = rest.strip_prefix('\'') {
            let end = stripped.find('\'').map(|pos| i + 1 + pos + 1);
            let next = end.unwrap_or(text.len());
            out.push_str(&text[i..next]);
            i = next;
            continue;
        }
        if rest.starts_with('#') {
            let end = rest.find('\n').map(|pos| i + pos + 1).unwrap_or(text.len());
            out.push_str(&text[i..end]);
            i = end;
            continue;
        }

        let ch = rest.chars().next().expect("valid char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

fn looks_like_windows_path_literal(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes
        .windows(3)
        .any(|w| w[0].is_ascii_alphabetic() && w[1] == b':' && w[2] == b'\\')
        || bytes.iter().take_while(|&&b| b == b'\\').count() >= 2
}

fn escape_windows_path_backslashes(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'\\' {
            let ch = s[i..].chars().next().expect("valid char boundary");
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }

        let start = i;
        while i < bytes.len() && bytes[i] == b'\\' {
            i += 1;
        }
        let run = i - start;
        let next = bytes.get(i).copied();
        let leading_raw_unc = start == 0 && run == 2 && next.is_some();
        let escaped_quote = next == Some(b'"') && run % 2 == 1;
        let target_run = if escaped_quote {
            run
        } else if leading_raw_unc || run % 2 == 1 {
            run + 1
        } else {
            run
        };
        out.extend(std::iter::repeat_n('\\', target_run));
    }

    out
}

fn preceding_backslashes(s: &str, quote_idx: usize) -> usize {
    s.as_bytes()[..quote_idx]
        .iter()
        .rev()
        .take_while(|&&b| b == b'\\')
        .count()
}

fn is_value_terminator_after_quote(s: &str, quote_end: usize) -> bool {
    let rest = &s[quote_end..];
    let rest = rest.trim_start_matches([' ', '\t']);
    rest.is_empty()
        || rest.starts_with('\n')
        || rest.starts_with('\r')
        || rest.starts_with('#')
        || rest.starts_with(',')
        || rest.starts_with(']')
        || rest.starts_with('}')
}

impl Config {
    fn validate_skill_curator(&self) -> Result<()> {
        if self.agent.skill_archive_after_days < self.agent.skill_stale_after_days {
            anyhow::bail!(
                "[agent] skill_archive_after_days ({}) must be >= skill_stale_after_days ({})",
                self.agent.skill_archive_after_days,
                self.agent.skill_stale_after_days
            );
        }
        Ok(())
    }

    /// 从 `rclaw.toml`（若存在）加载配置。
    pub fn load() -> Result<Self> {
        Self::load_from_dir(std::path::Path::new("."))
    }

    /// 解析一段 TOML 文本为配置（桌面配置面板保存前的校验用；不做缺省 provider
    /// 注入与文件 IO，只做与加载一致的语法/结构校验）。
    pub fn parse_str(text: &str) -> Result<Self> {
        parse_config_text(text).context("配置解析失败")
    }

    /// 从指定目录的 `rclaw.toml` 加载配置。
    ///
    /// 桌面版（rclaw-desktop）从用户数据目录（Windows `%APPDATA%\rclaw`）读取，
    /// 与进程 CWD 解耦——双击 exe 启动时工作目录不可依赖。文件缺失时与
    /// [`Self::load`] 一致：返回编译期默认（含默认 DeepSeek provider 占位）。
    pub fn load_from_dir(dir: &std::path::Path) -> Result<Self> {
        let path = dir.join("rclaw.toml");
        let cfg = if let Ok(text) = std::fs::read_to_string(&path) {
            // 与 parse_config_text 同序：先归一化 Windows 路径字面量（`\U` 等
            // 反斜杠序列会让 toml_edit 解析失败，导致遗留节剥不掉、带病解析），
            // 再物理删除遗留节并写回原文件（toml_edit 保真，注释与格式不动）；
            // 写回失败仅告警——本次仍按剥离结果加载，下次启动再试。
            let normalized = normalize_windows_path_basic_strings(&text);
            let (stripped, removed) = strip_removed_legacy_sections(&normalized);
            if !removed.is_empty() {
                // 原子写：截断窗口内被杀会让整份 rclaw.toml（含 API key）归零，
                // 下次启动按 Config::default() 加载。Android 路径（lib.rs）已同样处理。
                match rclaw_store::atomic_write(&path, stripped.as_bytes()) {
                    Ok(()) => tracing::info!(
                        keys = %removed.join(", "),
                        path = %path.display(),
                        "已从 rclaw.toml 物理移除已退役配置节"
                    ),
                    Err(error) => tracing::warn!(
                        keys = %removed.join(", "),
                        %error,
                        "rclaw.toml 遗留节写回失败（本次按剥离结果加载）"
                    ),
                }
            }
            toml::from_str(&stripped).with_context(|| format!("解析 {} 失败", path.display()))?
        } else {
            Config::default()
        };
        Self::finish_load(cfg)
    }

    /// 解析一段完整的 `rclaw.toml` 文本为配置，不触碰文件系统。
    ///
    /// 与 [`Self::load_from_dir`] 走同一条收尾路径（缺省 provider 注入 + 校验），
    /// 供 Android APK 等嵌入宿主使用：宿主自行决定配置文本从哪里来（app 私有
    /// 目录 / 向导写入），但接受的键与校验规则与 CLI / 桌面版完全一致。
    pub fn from_toml(text: &str) -> Result<Self> {
        let cfg = parse_config_text(text).context("解析 rclaw.toml 失败")?;
        Self::finish_load(cfg)
    }

    /// 加载收尾：缺省 provider 注入 + 依赖交叉校验（`load_from_dir` / `from_toml` 共用）。
    fn finish_load(mut cfg: Self) -> Result<Self> {
        // 若配置里没有 providers，注入默认 DeepSeek（保证开箱可用；api_key 留空，
        // 由用户在 rclaw.toml 中填入）。
        if cfg.llm.providers.is_empty() {
            cfg.llm.providers.push(ProviderEntry {
                name: "deepseek".into(),
                base_url: "https://api.deepseek.com/v1".into(),
                model: Some("deepseek-v4-pro".into()),
                api_key: None,
                auth: "bearer".into(),
                reasoning_field: "reasoning_content".into(),
                max_tokens_key: "max_tokens".into(),
                supports_tool_choice: true,
                context_length: 0,
            });
        }

        cfg.validate_skill_curator()?;

        Ok(cfg)
    }

    /// 当前选中 provider 的上下文窗口大小（token）。0 = 该 provider 未声明 `context_length`。
    ///
    /// 供 `rclaw_runtime::options::derive_compression_token_budgets` 派生压缩 token 阈值 /
    /// 尾窗预算。选中名为 [`LlmSection::provider`] 的条目。
    pub fn selected_context_length(&self) -> usize {
        self.llm
            .providers
            .iter()
            .find(|p| p.name == self.llm.provider)
            .map(|p| p.context_length)
            .unwrap_or(0)
    }

    /// 由选中的 provider 条目 + env key 构造 `ProviderConfig`。
    pub fn build_provider_config(&self) -> Result<ProviderConfig> {
        self.build_provider_config_for(&self.llm.provider)
    }

    /// 按名字构造 provider 配置（评审辅助 provider 复用同一校验路径）。
    pub fn build_provider_config_for(&self, provider_name: &str) -> Result<ProviderConfig> {
        let entry = self
            .llm
            .providers
            .iter()
            .find(|p| p.name == provider_name)
            .with_context(|| format!("未找到 provider '{provider_name}'"))?;

        // P2：统一用 trim 后的规范化字符串做【校验 / api_key / ProviderConfig】。
        // 旧实现用 trim 校验 name/base_url/model，却把【原始（可能带前后空白）】字符串存进
        // ProviderConfig：配置里不小心带空白会通过启动期校验，却在请求阶段拼出非法 URL 或
        // 把带空白的 model 发给 provider。另外旧实现在 name 校验之前就取 api_key，空 name
        // 会先报「缺少 api_key」而非真正的「name 为空」。现在先 trim 三项，再按 trim 后的
        // name 构造报错信息，并把 trim 后的值写进 ProviderConfig。
        let name = entry.name.trim().to_string();
        let base_url = entry.base_url.trim().to_string();
        let default_model = self
            .llm
            .model
            .as_deref()
            // entry.model.as_deref() 是廉价的 &str 借用（无副作用），用 eager 的 .or 即可，
            // 无需 .or_else 闭包（否则 clippy unnecessary_lazy_evaluations 告警）。
            .or(entry.model.as_deref())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "deepseek-v4-pro".to_string());

        // name 非空校验必须在构造 api_key 报错之前：否则空 name 会误报「缺少 api_key」，
        // 掩盖真正的「name 配置为空」问题。
        if name.is_empty() {
            anyhow::bail!("provider 配置非法：name 为空");
        }

        // api key 直接从配置文件的 `[[llm.providers]].api_key` 读取（trim 后非空校验）。
        let api_key = entry
            .api_key
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .with_context(|| {
                format!("provider '{name}' 缺少 api_key（请在 rclaw.toml 的 [[llm.providers]] 设置 api_key）")
            })?;
        let api_key = Secret::new(api_key);

        // auth 字段必须显式匹配已知方案，避免配置 typo（如 "apikey" vs "apikey:X-API-Key"）
        // 被静默降级为 Bearer，最终只表现为远端 401/400 难以排查。
        let auth = match entry.auth.as_str() {
            "bearer" | "" => AuthScheme::Bearer,
            s if s.starts_with("apikey:") => {
                let header = &s["apikey:".len()..];
                if header.is_empty() {
                    anyhow::bail!(
                        "provider '{}' auth 为 apikey: 但 header 名为空（应为 apikey:<Header-Name>）",
                        entry.name
                    );
                }
                AuthScheme::ApiKey {
                    header: header.to_string(),
                }
            }
            other => {
                anyhow::bail!(
                    "provider '{}' auth 值 '{}' 不识别（支持 bearer 或 apikey:<Header-Name>）",
                    entry.name,
                    other,
                );
            }
        };

        let reasoning_field = match entry.reasoning_field.as_str() {
            "reasoning_content" | "" => ReasoningField::ReasoningContent,
            "reasoning" => ReasoningField::Reasoning,
            "none" => ReasoningField::None,
            other => {
                anyhow::bail!(
                    "provider '{}' reasoning_field 值 '{}' 不识别（支持 reasoning_content / reasoning / none）",
                    entry.name,
                    other,
                );
            }
        };
        let max_tokens_key = match entry.max_tokens_key.as_str() {
            "max_tokens" | "" => MaxTokensKey::MaxTokens,
            "max_completion_tokens" => MaxTokensKey::MaxCompletionTokens,
            other => {
                anyhow::bail!(
                    "provider '{}' max_tokens_key 值 '{}' 不识别（支持 max_tokens / max_completion_tokens）",
                    entry.name,
                    other,
                );
            }
        };

        // P2：启动期标量校验（修复「Provider 标量配置缺少启动期校验」），全部使用上方 trim
        // 后的 `name` / `base_url` / `default_model`。
        // 旧实现直接把 name/base_url/model/max_tokens 写入 ProviderConfig：
        // - max_tokens = 0 会被请求层原样发送（max_tokens:0），provider 可能返回异常或空响应；
        // - 空 model / 空 base_url 会变成难排查的运行时 HTTP 错误（连接失败 / 404）。
        // 现在在装配前做 fail-fast 校验，失败时返回【带 provider 名】的 anyhow 错误，让误配在
        // 启动期就暴露，而不是等远端返回莫名其妙的 HTTP 错误。
        // （name 非空校验已在上文 env_key 生成前完成，此处不再重复。）
        if self.llm.max_tokens == 0 {
            anyhow::bail!("provider '{}' 配置非法：[llm] max_tokens 必须大于 0", name);
        }
        if self.llm.max_tool_calls == 0 {
            anyhow::bail!(
                "provider '{}' 配置非法：[llm] max_tool_calls 必须大于 0",
                name
            );
        }
        if self.llm.request_timeout_secs == 0 {
            anyhow::bail!(
                "provider '{}' 配置非法：[llm] request_timeout_secs 必须大于 0",
                name
            );
        }
        if base_url.is_empty() {
            anyhow::bail!("provider '{}' 配置非法：base_url 为空", name);
        }
        // base_url 必须可解析为绝对 http(s) URL：否则运行时拼 endpoint 会得到非法地址，
        // 表现为难以排查的连接失败 / 404。用 url crate 做真实解析（而非仅前缀判断），
        // 可捕获诸如非法字符、缺 host 等问题。
        match url::Url::parse(&base_url) {
            Ok(u) if matches!(u.scheme(), "http" | "https") => {}
            Ok(u) => {
                anyhow::bail!(
                    "provider '{}' 配置非法：base_url 协议必须为 http/https（实际为 '{}'): {}",
                    name,
                    u.scheme(),
                    base_url
                );
            }
            Err(e) => {
                anyhow::bail!(
                    "provider '{}' 配置非法：base_url 无法解析为 URL: {e}: {}",
                    name,
                    base_url
                );
            }
        }
        if default_model.is_empty() {
            anyhow::bail!(
                "provider '{}' 配置非法：model 为空（请在 [llm] model 或该 provider 的 model 设置有效模型名）",
                name
            );
        }

        Ok(ProviderConfig {
            // 全部使用 trim 后的规范化字符串，杜绝「校验过、却把带空白原值发出去」的隐患。
            name,
            base_url,
            auth,
            api_key,
            default_model,
            protocol: ProtocolKind::OpenAiCompat,
            dialect: Dialect {
                reasoning_field,
                // 透传 provider 配置的 supports_tool_choice（默认 true），
                // 让「不支持 tool_choice 的兼容服务」能在 rclaw.toml 里关掉（修复 P2）。
                supports_tool_choice: entry.supports_tool_choice,
                max_tokens_key,
            },
            max_tokens: self.llm.max_tokens,
            request_timeout_secs: self.llm.request_timeout_secs,
            max_tool_calls: self.llm.max_tool_calls,
        })
    }

    /// 构造微信通道配置（融合文件 + env）。
    pub fn weixin_channel_config(&self) -> WeixinConfig {
        // 用结构体字面量显式初始化【全部】字段（修复 P3「Weixin 重试相关字段不可从 TOML 配置」）：
        // 旧实现只透传 chunk_bytes，其余字段恒用 WeixinConfig::default()，用户在 TOML 里写
        // poll_timeout_sec / retry_sec / backoff_sec / max_failures 会被静默忽略。现在全部从
        // WeixinSection 透传；并对两个有「下限语义」的字段做保护：
        //  - send_chunk_bytes：chunk_bytes=0 会让 utf8 分段死循环（utf8_chunks 另有兜底），强制 >=1；
        //  - max_failures：=0 会让 send 的重试循环 `1..=0` 一次都不执行，任何瞬时失败都被当成
        //    最终失败，强制 >=1 以保证至少尝试一次。
        WeixinConfig {
            enabled: self.weixin.enabled.unwrap_or(false),
            token: self.weixin.token.clone().unwrap_or_default(),
            allow_from: self.weixin.allow_from.clone().unwrap_or_default(),
            api_base: self.weixin.api_base.clone(),
            cdn_base: self.weixin.cdn_base.clone(),
            // media_dir 为运行期路径，默认留空；由 main.rs 用 agent 存储根注入（agent/media）。
            media_dir: std::path::PathBuf::new(),
            send_chunk_bytes: self.weixin.chunk_bytes.max(1),
            max_failures: self.weixin.max_failures.max(1),
            poll_timeout_sec: self.weixin.poll_timeout_sec,
            retry_sec: self.weixin.retry_sec,
            backoff_sec: self.weixin.backoff_sec,
        }
    }

    /// 构造 wxbot 通道配置（融合文件配置）。
    ///
    /// 与 [`Self::weixin_channel_config`] 同款下限保护：`send_chunk_bytes`/`max_failures`
    /// 为 0 会导致分段死循环 / 重试循环一次都不执行，强制 >=1。
    pub fn wxbot_channel_config(&self) -> WxbotConfig {
        WxbotConfig {
            enabled: self.wxbot.enabled.unwrap_or(false),
            api_base: self.wxbot.api_base.clone(),
            api_token: self.wxbot.api_token.clone().unwrap_or_default(),
            webhook_listen: self.wxbot.webhook_listen.clone(),
            webhook_path: self.wxbot.webhook_path.clone(),
            webhook_secret: self.wxbot.webhook_secret.clone().unwrap_or_default(),
            allow_from: self.wxbot.allow_from.clone().unwrap_or_default(),
            allow_groups: self.wxbot.allow_groups.clone().unwrap_or_default(),
            group_reply_policy: self.wxbot.group_reply_policy.clone(),
            poll_fallback_enabled: self.wxbot.poll_fallback_enabled,
            poll_timeout_sec: self.wxbot.poll_timeout_sec,
            retry_sec: self.wxbot.retry_sec.max(1),
            backoff_sec: self.wxbot.backoff_sec.max(1),
            max_failures: self.wxbot.max_failures.max(1),
            send_chunk_bytes: self.wxbot.chunk_bytes.max(1),
            send_min_gap_sec: self.wxbot.send_min_gap_sec,
            send_jitter_sec: self.wxbot.send_jitter_sec,
            send_window_max: self.wxbot.send_window_max,
            send_failure_fuse: self.wxbot.send_failure_fuse,
            // media_dir 为运行期路径，默认留空；由 main.rs / host_glue 注入（agent/media）。
            media_dir: std::path::PathBuf::new(),
        }
    }

    /// 构造 NapCatQQ 通道配置（融合文件配置）。
    pub fn napcat_channel_config(&self) -> NapCatConfig {
        NapCatConfig {
            enabled: self.napcat.enabled.unwrap_or(false),
            ws_url: self.napcat.ws_url.clone().unwrap_or_default(),
            access_token: self.napcat.access_token.clone().unwrap_or_default(),
            allow_from: self.napcat.allow_from.clone().unwrap_or_default(),
            allow_groups: self.napcat.allow_groups.clone().unwrap_or_default(),
            block_from: self.napcat.block_from.clone().unwrap_or_default(),
            max_from: self.napcat.max_from.clone().unwrap_or_default(),
            group_response_mode: self.napcat.group_response_mode.clone(),
            heartbeat_interval_sec: self.napcat.heartbeat_interval_sec,
            reconnect_delay_sec: self.napcat.reconnect_delay_sec,
            send_chunk_bytes: if self.napcat.send_chunk_bytes == 0 {
                default_napcat_send_chunk_bytes()
            } else {
                self.napcat.send_chunk_bytes
            },
            // media_dir 为运行期路径，默认留空；由 main.rs 用 agent 存储根注入（agent/media）。
            media_dir: std::path::PathBuf::new(),
        }
    }

    /// 构造完整的 provider pool。没有 provider 子表时直接报错；即使某 provider
    /// `enabled=false` 也构建其客户端，使 `/search en` 能在当前进程内立即恢复。
    pub fn web_search_runtime_config(&self) -> Result<Option<WebSearchRuntimeConfig>> {
        if !self.web_search.enabled {
            return Ok(None);
        }
        let tool = rclaw_tools::WebSearchConfig {
            read_top: self.web_search.read_top,
            max_results: self.web_search.max_results,
            total_timeout_secs: self.web_search.total_timeout_secs,
        }
        .normalized()
        .map_err(|error| anyhow::anyhow!("web_search 配置非法: {error}"))?;
        if self.web_search.circuit_failure_threshold == 0
            || self.web_search.circuit_failure_threshold > 20
        {
            anyhow::bail!("web_search circuit_failure_threshold 必须在 1..=20 之间");
        }
        if self.web_search.circuit_cooldown_secs == 0
            || self.web_search.circuit_cooldown_secs > 3_600
        {
            anyhow::bail!("web_search circuit_cooldown_secs 必须在 1..=3600 之间");
        }

        let zhipu = self
            .web_search
            .zhipu
            .as_ref()
            .map(|section| {
                rclaw_tools::ZhipuSearchConfig {
                    api_key: section.api_key.clone(),
                    search_url: section.search_url.clone(),
                    reader_url: section.reader_url.clone(),
                    startup_timeout_secs: section.startup_timeout_secs,
                    call_timeout_secs: section.call_timeout_secs,
                }
                .normalized()
                .map_err(anyhow::Error::msg)
            })
            .transpose()?;
        let tavily = self
            .web_search
            .tavily
            .as_ref()
            .map(|section| {
                rclaw_tools::TavilySearchConfig {
                    api_keys: section.api_keys.clone(),
                    base_url: section.base_url.clone(),
                    timeout_secs: section.timeout_secs,
                    max_results: section.max_results,
                    search_depth: section.search_depth.clone(),
                    include_answer: section.include_answer,
                }
                .normalized()
                .map_err(anyhow::Error::msg)
            })
            .transpose()?;
        let doubao_custom = self
            .web_search
            .doubao_custom
            .as_ref()
            .map(|section| {
                if section.max_snippet_tokens.is_some() {
                    anyhow::bail!(
                        "web_search.doubao_custom 不支持 max_snippet_tokens；该字段仅用于 doubao_global"
                    );
                }
                rclaw_tools::DoubaoSearchConfig {
                    variant: rclaw_tools::DoubaoVariant::Custom,
                    api_key: section.api_key.clone(),
                    endpoint: section.endpoint.clone().unwrap_or_default(),
                    timeout_secs: section.timeout_secs,
                    max_results: section.max_results.unwrap_or(50),
                    // Custom 请求不会发送该字段；后端公共配置结构仍要求一个合法占位值。
                    max_snippet_tokens: 1_000,
                }
                .normalized()
                .map_err(anyhow::Error::msg)
            })
            .transpose()?;
        let doubao_global = self
            .web_search
            .doubao_global
            .as_ref()
            .map(|section| {
                rclaw_tools::DoubaoSearchConfig {
                    variant: rclaw_tools::DoubaoVariant::Global,
                    api_key: section.api_key.clone(),
                    endpoint: section.endpoint.clone().unwrap_or_default(),
                    timeout_secs: section.timeout_secs,
                    max_results: section.max_results.unwrap_or(20),
                    max_snippet_tokens: section.max_snippet_tokens.unwrap_or(1_000),
                }
                .normalized()
                .map_err(anyhow::Error::msg)
            })
            .transpose()?;

        let configured = [
            (
                rclaw_tools::BackendId::DoubaoCustom,
                doubao_custom.is_some(),
            ),
            (rclaw_tools::BackendId::Tavily, tavily.is_some()),
            (rclaw_tools::BackendId::Zhipu, zhipu.is_some()),
            (
                rclaw_tools::BackendId::DoubaoGlobal,
                doubao_global.is_some(),
            ),
        ];
        if !configured.iter().any(|(_, present)| *present) {
            anyhow::bail!("web_search 已启用，但没有配置任何 provider 子表");
        }
        let order = if self.web_search.order.is_empty() {
            configured
                .iter()
                .filter_map(|(id, present)| present.then_some(*id))
                .collect::<Vec<_>>()
        } else {
            self.web_search
                .order
                .iter()
                .map(|raw| {
                    raw.parse::<rclaw_tools::BackendId>()
                        .map_err(anyhow::Error::msg)
                })
                .collect::<Result<Vec<_>>>()?
        };
        let enabled = [
            self.web_search
                .doubao_custom
                .as_ref()
                .map(|section| (rclaw_tools::BackendId::DoubaoCustom, section.enabled)),
            self.web_search
                .tavily
                .as_ref()
                .map(|section| (rclaw_tools::BackendId::Tavily, section.enabled)),
            self.web_search
                .zhipu
                .as_ref()
                .map(|section| (rclaw_tools::BackendId::Zhipu, section.enabled)),
            self.web_search
                .doubao_global
                .as_ref()
                .map(|section| (rclaw_tools::BackendId::DoubaoGlobal, section.enabled)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

        // 复用控制面的严格校验，启动期就拒绝重复、遗漏或未配置的 order 项。
        rclaw_tools::WebSearchControl::new(
            order.clone(),
            &enabled,
            self.web_search.fallback,
            self.web_search.circuit_failure_threshold,
            std::time::Duration::from_secs(self.web_search.circuit_cooldown_secs),
        )
        .map_err(anyhow::Error::msg)?;

        Ok(Some(WebSearchRuntimeConfig {
            tool,
            order,
            enabled,
            fallback: self.web_search.fallback,
            circuit_failure_threshold: self.web_search.circuit_failure_threshold,
            circuit_cooldown_secs: self.web_search.circuit_cooldown_secs,
            zhipu,
            tavily,
            doubao_custom,
            doubao_global,
        }))
    }
}

// ===========================================================================
// 单元测试：聚焦「`[mcp]` 段经 TOML → Config → McpResolved」这条集成路径。
// 验证 TOML 表/HashMap 映射、默认回退、缺省空配置、deny_unknown_fields。
// ===========================================================================
#[cfg(test)]
mod tests;

#[cfg(test)]
mod device_probe {
    // 真机取证辅助：从设备拉来的 rclaw.toml 解析探针（本地临时）。
    #[test]
    fn device_toml_parse_probe() {
        let path = std::env::var("RCLAW_PROBE_TOML").unwrap_or_default();
        if path.is_empty() {
            return;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        match crate::config::Config::from_toml(&text) {
            Ok(_) => println!("PARSE OK"),
            Err(e) => println!("PARSE ERR: {e:?}"),
        }
    }
}
