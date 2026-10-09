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
use crate::turn::TurnManager;

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
