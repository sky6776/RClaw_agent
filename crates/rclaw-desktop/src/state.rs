//! 应用共享状态：数据目录 + 运行中 agent 的句柄。
//!
//! 前端经 command 只能拿到句柄的克隆（sender / cron 快照接口），永远拿不到
//! 核心组件本身——「前端只经 command/event 说话」的边界由类型保证。

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use rclaw_core::{BusMessage, RuntimeChannelInfo};
use rclaw_runtime::{AgentOptions, AgentToolRuntime, BotRegistry, BotRouter, SessionLifecycle};
use rclaw_store::fs::Layout;
use rclaw_store::memory::MemoryStore;
use rclaw_store::session::SessionStore;
use rclaw_tools::{CronController, ToolRegistry, WebSearchControl};
use tokio::sync::mpsc;

/// agent 启动后可用的进程句柄。
pub struct RunningAgent {
    /// inbound 总线发送端（对话消息 / 斜杠命令都从这里进 agent loop）。
    pub inbound_tx: mpsc::Sender<BusMessage>,
    /// cron 任务表（定时任务面板直接读它，不经模型）。
    pub cron: Arc<CronController>,
    /// 会话存储（多会话列表 / 历史 / 归档直接读它）。
    pub session: SessionStore,
    /// 会话生命周期句柄：删会话时同步清掉 agent loop 里该会话的运行期状态
    /// （usage 锚点、压缩防抖 / 冷却），库与内存一起归零。
    pub session_lifecycle: SessionLifecycle,
    /// 统一分叉入口句柄：桌面「分叉」按钮直连 runtime 的 fork_chat（复制历史 +
    /// 自动文件回滚 + 子会话告知行），与 `/fork` 斜杠命令同一实现。
    pub fork: rclaw_runtime::ForkHandle,
    /// 多 agent 工具运行时（Agent 状态面板读后台执行快照）。
    pub agent_runtime: AgentToolRuntime,
    /// 工具注册表（工具面板的列表与禁能开关）。
    pub tools: Arc<ToolRegistry>,
    /// 存储布局（自进化面板读记忆文件 / 技能目录）。
    pub layout: Layout,
    /// 记忆存储克隆（状态面板的上下文占用估算读 MEMORY.md / USER.md）。
    pub memory: MemoryStore,
    /// agent 选项克隆（上下文窗口 / 历史裁剪参数与运行期压缩同源）。
    pub opts: AgentOptions,
    /// 通道能力克隆（动态上下文注入需要，与 agent loop 一致）。
    pub channels: RuntimeChannelInfo,
    /// web_search 控制克隆（system prompt 重建需要；未配置为 None）。
    pub web_search: Option<Arc<WebSearchControl>>,
    /// 选中的 provider 名。
    pub provider_name: String,
    /// 选中的模型名。
    pub model: Option<String>,
    /// Bot Mode：bot 注册表（花名册 / persona / 编辑后热重载）。
    pub bots: Arc<BotRegistry>,
    /// Bot Mode：交接路由（群聊编排投成员轮）。
    pub bot_router: Arc<BotRouter>,
    /// 可选 provider 名列表（bot 编辑器固定模型用）。
    pub llm_providers: Vec<String>,
}

/// Tauri managed state。
pub struct AppState {
    /// 用户数据目录（rclaw.toml + agent/ 都在这里）。
    pub home: PathBuf,
    pub agent: RwLock<Option<RunningAgent>>,
    /// 最近一次 agent 启动失败的错误文案（get_status 暴露给前端，作为事件
    /// 丢失时的兜底——启动失败的 error 事件若被前端错过，界面会永远停在
    /// 「启动中」；轮询状态查询时据此转入错误态）。
    pub start_error: RwLock<Option<String>>,
    /// Bot Mode：进行中的群聊编排登记（group_id → 取消令牌）。
    pub group_runs: crate::groups::GroupRuns,
}

impl AppState {
    pub fn new(home: PathBuf) -> Self {
        AppState {
            home,
            agent: RwLock::new(None),
            start_error: RwLock::new(None),
            group_runs: crate::groups::GroupRuns::new(),
        }
    }

    /// 记录启动失败文案（成功启动时清空）。
    pub fn set_start_error(&self, error: Option<String>) {
        *self
            .start_error
            .write()
            .expect("AppState.start_error 锁未被毒化") = error;
    }

    /// 取 inbound 发送端克隆；agent 未启动时返回错误文案。
    pub fn inbound(&self) -> Result<mpsc::Sender<BusMessage>, String> {
        self.agent
            .read()
            .expect("AppState.agent 锁未被毒化")
            .as_ref()
            .map(|a| a.inbound_tx.clone())
            .ok_or_else(|| "agent 尚未启动（首运行请先完成配置向导）".to_string())
    }

    /// cron 句柄克隆。
    pub fn cron(&self) -> Option<Arc<CronController>> {
        self.agent
            .read()
            .expect("AppState.agent 锁未被毒化")
            .as_ref()
            .map(|a| a.cron.clone())
    }

    /// 会话存储克隆。
    pub fn session(&self) -> Option<SessionStore> {
        self.agent
            .read()
            .expect("AppState.agent 锁未被毒化")
            .as_ref()
            .map(|a| a.session.clone())
    }

    /// 会话生命周期句柄克隆。
    pub fn session_lifecycle(&self) -> Option<SessionLifecycle> {
        self.agent
            .read()
            .expect("AppState.agent 锁未被毒化")
            .as_ref()
            .map(|a| a.session_lifecycle.clone())
    }

    /// 统一分叉入口句柄克隆。
    pub fn fork(&self) -> Option<rclaw_runtime::ForkHandle> {
        self.agent
            .read()
            .expect("AppState.agent 锁未被毒化")
            .as_ref()
            .map(|a| a.fork.clone())
    }

    /// 多 agent 运行时句柄克隆。
    pub fn agent_runtime(&self) -> Option<AgentToolRuntime> {
        self.agent
            .read()
            .expect("AppState.agent 锁未被毒化")
            .as_ref()
            .map(|a| a.agent_runtime.clone())
    }

    /// 工具注册表克隆。
    pub fn tools(&self) -> Option<Arc<ToolRegistry>> {
        self.agent
            .read()
            .expect("AppState.agent 锁未被毒化")
            .as_ref()
            .map(|a| a.tools.clone())
    }

    /// 存储布局克隆。
    pub fn layout(&self) -> Option<Layout> {
        self.agent
            .read()
            .expect("AppState.agent 锁未被毒化")
            .as_ref()
            .map(|a| a.layout.clone())
    }

    /// 上下文占用估算所需的输入克隆（memory / opts / channels / web_search）。
    pub fn usage_inputs(
        &self,
    ) -> Option<(
        MemoryStore,
        AgentOptions,
        RuntimeChannelInfo,
        Option<Arc<WebSearchControl>>,
    )> {
        self.agent
            .read()
            .expect("AppState.agent 锁未被毒化")
            .as_ref()
            .map(|a| {
                (
                    a.memory.clone(),
                    a.opts.clone(),
                    a.channels.clone(),
                    a.web_search.clone(),
                )
            })
    }

    /// (provider 名, 模型名)。
    pub fn provider_summary(&self) -> Option<(String, Option<String>)> {
        self.agent
            .read()
            .expect("AppState.agent 锁未被毒化")
            .as_ref()
            .map(|a| (a.provider_name.clone(), a.model.clone()))
    }

    /// Bot Mode：bot 注册表 + 交接路由 + provider 名列表。
    pub fn bot_components(&self) -> Option<(Arc<BotRegistry>, Arc<BotRouter>, Vec<String>)> {
        self.agent
            .read()
            .expect("AppState.agent 锁未被毒化")
            .as_ref()
            .map(|a| {
                (
                    a.bots.clone(),
                    a.bot_router.clone(),
                    a.llm_providers.clone(),
                )
            })
    }

    /// web_search 控制克隆（bot 注册表热重载重建提示用）。
    pub fn web_search(&self) -> Option<Arc<WebSearchControl>> {
        self.agent
            .read()
            .expect("AppState.agent 锁未被毒化")
            .as_ref()
            .and_then(|a| a.web_search.clone())
    }
}
