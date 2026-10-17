//! Agent 主循环（路由器）。
//!
//! 旧模型是「单 turn 串行」：`pop inbound → handle_turn(msg).await → 下一条`，长 turn 期间
//! 无法处理新输入、无法中断正在执行的 skill / tool。
//!
//! 现在改成路由器：`pop inbound → TurnManager::route(message)`。真正的 LLM / tool 执行、
//! `/cancel` 中断、插队注入全部下沉到 [`crate::turn::TurnManager`]（每会话一个活动 turn 任务）。
//! 全局 loop 永远不被长 turn 卡住，可以持续处理 CLI / 微信 / cron / heartbeat 的新消息。
//!
//! 详见 [`crate::turn`] 模块注释。
//!
//! 流程要点（落在 [`crate::turn`] 模块）：
//! pop inbound → `TurnManager::route(msg)`：
//!   - `/cancel`：取消当前会话 running turn，或回送「无任务」提示；
//!   - 已有活动 turn：把新输入作为插队事件投递（下一个协议安全点处理）；
//!   - 无活动 turn：为该会话启动新 turn 任务（ReAct：调 LLM → 必要时执行工具 → 最终回复）。

use std::sync::Arc;

use tokio::sync::mpsc;

use rclaw_core::BusMessage;
use rclaw_llm::Provider;
use rclaw_store::fs::Layout;
use rclaw_store::memory::MemoryStore;
use rclaw_store::session::SessionStore;
use rclaw_tools::ToolRegistry;

use crate::options::AgentOptions;
use crate::slash::SlashRegistry;
use crate::turn::{ForkChatOutcome, TurnManager};

/// agent 主循环：持有 [`TurnManager`]，循环把 inbound 消息路由出去。
pub struct AgentLoop {
    turn_mgr: Arc<TurnManager>,
    inbound_rx: mpsc::Receiver<BusMessage>,
}

impl AgentLoop {
    /// 构造主循环。参数集合与改造前完全一致，便于 `main.rs` 无感替换；
    /// 内部把它们组装成一个共享的 [`TurnManager`]。
    /// `bots` / `bot_router` 为 Bot Mode 组件（ADR-0006），由 bootstrap 装配后传入。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn Provider>,
        review_provider: Option<Arc<dyn Provider>>,
        session: SessionStore,
        memory: MemoryStore,
        layout: Layout,
        tools: Arc<ToolRegistry>,
        opts: AgentOptions,
        inbound_rx: mpsc::Receiver<BusMessage>,
        outbound_tx: mpsc::Sender<BusMessage>,
        slash: Option<Arc<SlashRegistry>>,
        channels: rclaw_core::RuntimeChannelInfo,
        bots: Arc<crate::bots::BotRegistry>,
        bot_router: Option<Arc<crate::bots::BotRouter>>,
    ) -> Self {
        let turn_mgr = Arc::new(TurnManager::new(
            provider,
            review_provider,
            session,
            memory,
            layout,
            tools,
            opts,
            outbound_tx,
            slash,
            channels,
            bots,
            bot_router,
        ));
        AgentLoop {
            turn_mgr,
            inbound_rx,
        }
    }

    /// 会话生命周期句柄（在 `run()` 之前取走、可跨任务持有）：宿主在 agent loop 之外
    /// 直接删库里的会话后，用它把该会话的运行期状态一并清掉。
    pub fn session_lifecycle(&self) -> SessionLifecycle {
        SessionLifecycle {
            turn_mgr: self.turn_mgr.clone(),
        }
    }

    /// 分叉句柄（在 `run()` 之前取走、可跨任务持有）：宿主桌面命令直连统一的分叉
    /// 入口（复制历史 + 自动文件回滚 + 子会话告知行），与 `/fork` 斜杠命令同一实现。
    pub fn fork_handle(&self) -> ForkHandle {
        ForkHandle {
            turn_mgr: self.turn_mgr.clone(),
        }
    }

    /// 运行主循环：收到一条 inbound 就交给 [`TurnManager::route`]，直到 inbound 关闭。
    ///
    /// `route` 自身不阻塞在长 turn 上（turn 在独立任务里跑），因此 loop 能持续路由新消息。
    pub async fn run(mut self) {
        tracing::info!("Agent loop 已启动（路由模式）");
        while let Some(msg) = self.inbound_rx.recv().await {
            self.turn_mgr.route(msg).await;
        }
        tracing::info!("Agent loop 退出");
    }
}

/// 会话生命周期句柄：对 [`TurnManager`] 的薄封装，只暴露宿主删会话时需要的那两个入口，
/// 不把核心组件本身交给前端 / 宿主层。
#[derive(Clone)]
pub struct SessionLifecycle {
    turn_mgr: Arc<TurnManager>,
}

/// 分叉句柄：对 [`TurnManager`] 的薄封装，只暴露宿主桌面命令需要的统一分叉入口，
/// 不把核心组件本身交给前端 / 宿主层（与 [`SessionLifecycle`] 同一模式）。
#[derive(Clone)]
pub struct ForkHandle {
    turn_mgr: Arc<TurnManager>,
}

impl ForkHandle {
    /// 统一分叉入口：复制可见历史前缀到新会话 + 自动回滚锚点之后的 checkpointed
    /// 文件写入 + 往子会话写「分叉告知」行。持静默门：任务运行中 / 后台 review 时拒绝。
    pub async fn fork_chat(
        &self,
        channel: &str,
        chat_id: &str,
        anchor_message_id: i64,
    ) -> Result<ForkChatOutcome, String> {
        self.turn_mgr
            .fork_chat(channel, chat_id, anchor_message_id)
            .await
    }
}

impl SessionLifecycle {
    #[cfg(test)]
    pub(crate) fn for_tests(turn_mgr: Arc<TurnManager>) -> Self {
        Self { turn_mgr }
    }

    /// 某会话此刻是否还有活动 turn（见 [`TurnManager::is_session_busy`]）：宿主删会话前
    /// 先问一句，正在跑的会话拒绝删除，让用户先 `/cancel` 或等它结束。
    pub fn is_session_busy(&self, channel: &str, chat_id: &str) -> bool {
        self.turn_mgr.is_session_busy(channel, chat_id)
    }

    /// 清掉某会话的运行期状态（见 [`TurnManager::forget_session`]）。
    pub fn forget_session(&self, channel: &str, chat_id: &str) {
        self.turn_mgr.forget_session(channel, chat_id);
    }
}
