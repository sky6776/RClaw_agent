//! rclaw-channels：通道抽象与具体通道。
//!
//! channel + outbound 分发。模型：
//! - 每个 [`Channel`] 自驱动：把外部输入（stdin / 微信长轮询…）投递进 `inbound`，
//!   并消费属于自己的 `outbound` 回复发回用户。
//! - [`OutboundDispatcher`] 从全局 outbound 取消息，按 `msg.channel` 名路由到
//!   各通道自己的 outbound 管道（对应 outbound 分发任务）。
//!
//! 通道与 agent loop 通过 `tokio::mpsc` 解耦（两条方向各一条 channel），
//! 用异步 channel 取代预分配 ring buffer + lease 借用模型。

pub mod console;
pub mod napcat;
pub mod weixin;

pub use console::Console;
pub use napcat::{NapCat, NapCatConfig};
pub use weixin::{Weixin, WeixinConfig};

use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::mpsc;

use rclaw_core::BusMessage;

/// inbound 方向的发送端（channel/cron/heartbeat → agent loop）。
pub type InboundSender = mpsc::Sender<BusMessage>;
/// 某通道专属的 outbound 接收端（agent loop → 该通道）。
pub type OutboundReceiver = mpsc::Receiver<BusMessage>;

/// 通道统一接口。
///
/// `run` 消费 `self`：实现内部通常再 spawn 子任务分别处理「输入→inbound」
/// 与「outbound→回发」，二者并发。
#[async_trait]
pub trait Channel: Send + 'static {
    /// 通道名（路由键，如 "cli"/"weixin"）。
    fn name(&self) -> &str;

    /// 本通道的三项能力声明（C3）：接线期由 CLI 采集，runtime 消费数据而非环境变量。
    fn capabilities(&self) -> rclaw_core::ChannelCaps {
        rclaw_core::ChannelCaps {
            name: self.name().to_string(),
            ..Default::default()
        }
    }

    /// 启动通道。返回即视为通道结束（通常永不返回，直到进程退出）。
    async fn run(self, inbound: InboundSender, outbound: OutboundReceiver);
}

/// 每个 route 转发任务【内部缓冲队列】的容量。
///
/// dispatcher 把消息 `try_send` 进这条内部队列；转发任务再从中取出、按 [`SEND_TIMEOUT`]
/// 超时发往真实通道。容量与下游通道（CLI/微信各 16）对齐：慢通道在「内部队列 + 下游通道」
/// 两级各缓冲 16，合计 ~32 条后才开始 dead-letter，给短暂拥塞留足缓冲。
const ROUTE_QUEUE_CAP: usize = 16;

/// 向真实通道单条发送的超时上限（覆盖各 route 的转发任务）。
///
/// 超时不是「丢弃后继续」的静默行为——见 [`forward_route`]：超时消息会进入 dead-letter
/// （error 级日志，含正文），让投递失败【可观测】，而非旧实现那样悄无声息地把用户回复丢掉。
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// outbound 分发器：按 `msg.channel` 把回复路由到对应通道。
///
/// **架构（修复 P2「outbound 超时会丢回复 / 跨通道阻塞」）**：
/// 旧实现是单消费者循环——对每条消息用 `timeout(5s, tx.send(msg))` 直接发往真实通道。
/// 问题有二：
/// 1. **跨通道 head-of-line blocking**：一个慢通道（队列满、网络故障）会让 dispatcher 在该
///    消息上卡最多 5s，期间所有其它通道的回复都被阻塞在 `global_rx` 后面。
/// 2. **静默丢回复**：超时后直接丢弃消息且 agent loop 无感知——用户的回复永久消失。
///
/// 现在每个 route 拥有【独立的转发任务 + 有界内部队列】：
/// - dispatcher 对每个 route 只 `try_send` 进【内部队列】，永不 await、永不阻塞——
///   慢通道最多让自己的内部队列满，不会拖累其它 route（消除跨通道阻塞）。
/// - 转发任务独占该 route 的真实发送，超时则把消息记入 dead-letter（error 级日志，含正文），
///   让投递失败【可观测】、可从日志恢复（不再静默丢弃）。
pub struct OutboundDispatcher {
    /// 每个 route 的【内部缓冲队列】发送端（dispatcher 写入，转发任务消费）。
    /// 注意：这与通往真实通道的 `Sender` 是两条不同的管道（见 [`forward_route`]）。
    routes: HashMap<String, mpsc::Sender<BusMessage>>,
    global_rx: mpsc::Receiver<BusMessage>,
}

impl OutboundDispatcher {
    pub fn new(global_rx: mpsc::Receiver<BusMessage>) -> Self {
        OutboundDispatcher {
            routes: HashMap::new(),
            global_rx,
        }
    }

    /// 注册某通道的 outbound 发送端（由 runtime 在启动通道时绑定）。
    ///
    /// 调用时会为该 route 额外 spawn 一个【独立的转发任务】（[`forward_route`]），
    /// 并把 dispatcher 写入的发送端指向该任务的内部缓冲队列，而非直接指向 `tx`。
    /// 因此：dispatcher 永远只做非阻塞的 `try_send`，慢通道的发送等待被隔离在转发任务里，
    /// 无法阻塞全局 dispatcher 或其它 route。
    pub fn route_to(&mut self, name: impl Into<String>, tx: mpsc::Sender<BusMessage>) {
        let name_str: String = name.into();
        // 为该 route 建立独立的有界内部队列 + 转发任务。
        let (queue_tx, queue_rx) = mpsc::channel::<BusMessage>(ROUTE_QUEUE_CAP);
        let route_name = name_str.clone();
        tokio::spawn(forward_route(route_name, tx, queue_rx));
        // dispatcher 之后只写 queue_tx（内部队列），不再直接持有通往真实通道的 tx。
        self.routes.insert(name_str, queue_tx);
    }

    /// 持续消费全局 outbound 并分发到各 route 的内部队列。
    ///
    /// 对每个 route 仅做 `try_send`（非阻塞）：
    /// - 成功 → 消息进入该 route 的转发任务，由它负责真实投递；
    /// - 内部队列满（`Full`）→ 该 route 消费过慢，消息进入 dead-letter（error 级日志，含正文），
    ///   【只丢这一条 / 这一路】，绝不阻塞、绝不连累其它 route；
    /// - 转发任务已退出（`Closed`）→ warn 后继续。
    pub async fn run(mut self) {
        tracing::info!("Outbound 分发器已启动");
        while let Some(msg) = self.global_rx.recv().await {
            match self.routes.get(&msg.channel) {
                Some(queue_tx) => match queue_tx.try_send(msg) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(msg)) => {
                        // 该 route 的转发任务消费过慢，内部队列已满。dead-letter：
                        // error 级日志让投递失败可观测、可恢复——不再静默丢弃。
                        //
                        // 隐私/日志体积（修复 P2「dead-letter 日志泄露完整消息正文」）：
                        // 与下方 `forward_route` 的超时分支保持一致——只记录 channel、chat_id、
                        // 字节数与【200 字符预览】，【绝不】把完整 msg.content 写入日志。outbound
                        // 可能承载用户提示、工具结果或隐私内容，完整落盘到 error 级日志会造成泄露
                        // 并放大日志压力。完整 dead-letter 若确需保留，应进入受控的持久化队列
                        // （重试需谨慎乱序），而非随日志持久化。
                        let preview: String = msg.content.chars().take(200).collect();
                        tracing::error!(
                            "向通道 {} 的转发队列已满（通道消费过慢），消息进入 dead-letter: \
                             chat_id={} ({} 字节) 正文预览={:?}",
                            msg.channel,
                            msg.chat_id,
                            msg.content.len(),
                            preview,
                        );
                    }
                    Err(mpsc::error::TrySendError::Closed(msg)) => {
                        tracing::warn!(
                            "通道 {} 的转发任务已退出（已关闭），消息丢弃: chat_id={} ({} 字节)",
                            msg.channel,
                            msg.chat_id,
                            msg.content.len()
                        );
                    }
                },
                // 无对应通道（如 system 消息）→ 仅记录，不报错。
                None => tracing::info!(
                    "无对应通道的消息 [{}:{}] ({} 字节)",
                    msg.channel,
                    msg.chat_id,
                    msg.content.len()
                ),
            }
        }
        tracing::info!("Outbound 分发器退出");
    }
}

/// 单个 route 的转发任务：独占该 route 的真实发送端。
///
/// 从内部缓冲队列取消息，用 [`SEND_TIMEOUT`] 超时发往真实通道：
/// - **成功** → 正常投递；
/// - **超时** → 消息进入 dead-letter（error 级日志，含正文），【不再静默丢弃】。
///   说明：outbound 是单向管道，无法把投递失败回报给 agent loop；但 error 级日志让失败
///   【可观测】，运维可据此发现慢通道并恢复，远好于旧实现的静默丢弃。若需自动重试，
///   可在此处接入一个持久化 dead-letter 队列（注意重试会导致乱序，需谨慎）。
/// - **通道关闭** → warn 后任务退出。
///
/// 因为每个 route 各有一个转发任务，某条慢通道的超时等待【只阻塞它自己的任务】，
/// 不再像旧实现那样卡住全局 dispatcher、连累所有通道（彻底消除跨通道 head-of-line blocking）。
async fn forward_route(
    name: String,
    tx: mpsc::Sender<BusMessage>,
    mut queue_rx: mpsc::Receiver<BusMessage>,
) {
    use tokio::time::timeout;
    tracing::info!("通道 {} 的 outbound 转发任务已启动", name);
    while let Some(msg) = queue_rx.recv().await {
        // 预先抽取用于 dead-letter 日志的信息：`tx.send(msg)` 会 move 掉 msg，
        // 超时后无法再访问其字段，故先克隆 chat_id 与正文预览（截断 200 字符避免日志膨胀）。
        let dl_chat_id = msg.chat_id.clone();
        let dl_len = msg.content.len();
        let dl_preview: String = msg.content.chars().take(200).collect();
        match timeout(SEND_TIMEOUT, tx.send(msg)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                tracing::warn!("通道 {} 已关闭，转发任务退出", name);
                break;
            }
            Err(_elapsed) => {
                // 超时 → dead-letter：error 级日志（含正文），不再静默丢弃。
                tracing::error!(
                    "向通道 {} 发送超时（{}s），消息进入 dead-letter: chat_id={} ({} 字节) 正文={:?}",
                    name,
                    SEND_TIMEOUT.as_secs(),
                    dl_chat_id,
                    dl_len,
                    dl_preview,
                );
            }
        }
    }
    tracing::info!("通道 {} 的 outbound 转发任务退出", name);
}

/// Crate 版本(关于页「版本信息」用)。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
