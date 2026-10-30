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
pub mod wxbot;

pub use console::Console;
pub use napcat::{NapCat, NapCatConfig};
pub use weixin::{Weixin, WeixinConfig};
pub use wxbot::{Wxbot, WxbotConfig};

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

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
/// 超时发往真实通道。慢通道在「内部队列 + 下游通道」两级缓冲后才开始 dead-letter：
/// desktop 下游缓冲 64（合计 ~192 条），napcat/weixin 下游 16（合计 ~144 条）。
/// 2026-10-21 取值 16→128（灭屏实测：💭 突发在旧值下积压 ~80 条丢 2 条增量）。
/// 【有意不取无限/动态】：无界积压会让最终回复排在一长串过期增量后面，交付延迟
/// 无上界（`forward_route` 的超时从出队后才起算，排队时间不计入，积压只会拖慢
/// 交付、不会帮它超时）；且消费端长停摆时内存无界。有界 + 满载后增量立即
/// dead-letter、最终回复进滞留车道等待，才是对两者都最优的形态。
const ROUTE_QUEUE_CAP: usize = 128;

/// 向真实通道单条发送的超时上限（覆盖各 route 的转发任务）。
///
/// 超时不是「丢弃后继续」的静默行为——见 [`forward_route`]：超时消息会进入 dead-letter
/// （error 级日志，含正文），让投递失败【可观测】，而非旧实现那样悄无声息地把用户回复丢掉。
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 最终回复（非 intermediate）专用的加长超时。
///
/// 进度增量（💭 流式噪音）丢了无所谓，最终回复承载交付内容，不能因瞬时拥塞被丢：
/// 2026-10-21 真机实测（京东采集 turn，手机灭屏期 desktop 路由），两级缓冲打满后
/// 两条进度增量 dead-letter——最终回复当时只是恰好赶上队列腾空才幸免。慢消费端
/// （Android WebView 挂后台/灭屏暂停渲染）恢复前台后毫秒级排空；分发层的滞留车道
/// （见 [`OutboundDispatcher`]）负责等队列腾位，这里的加长窗口负责最后一级真实
/// 发送：消费端停摆超过它才允许丢最终回复（保全局 outbound 流量不被一个死消费端
/// 永久卡死），错误日志保证可观测。
const SEND_TIMEOUT_FINAL: std::time::Duration = std::time::Duration::from_secs(30);

/// 每 route 滞留车道的积压上限（条数）。车道只在短暂拥塞（消费端恢复为毫秒级）
/// 时存活；超限后增量可丢、最终回复始终保留。
const OVERFLOW_LANE_CAP: usize = 64;

/// 每 route 的滞留车道：满载期间按序待发的消息。
#[derive(Default)]
struct OverflowLane {
    pending: VecDeque<BusMessage>,
    /// 排空任务在岗。期间该 route 的新消息一律追加本队列，【不得 try_send 抢位】——
    /// 后来者若从空位直接入队，会越过先滞留的消息，破坏交付顺序。
    active: bool,
}

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
///
/// **满载滞留车道（2026-10-21）**：内部队列也满时，进度增量照旧 dead-letter（噪音
/// 可丢），最终回复则进入该 route 的 [`OverflowLane`] 按序滞留，由独立排空任务
/// [`drain_overflow_lane`] 等队列腾位后逐条投递。车道激活期间该 route 的所有新消息
/// 一律入道——先滞留者先交付，排空任务的单消费者顺序保证不乱序；排空在 spawn 的
/// 任务里进行，全局分发循环【永不被一条消息卡住】（跨 route 阻塞是本架构明确消灭
/// 的属性，不能为保最终回复而复活）。
pub struct OutboundDispatcher {
    /// 每个 route 的【内部缓冲队列】发送端（dispatcher 写入，转发任务消费）。
    /// 注意：这与通往真实通道的 `Sender` 是两条不同的管道（见 [`forward_route`]）。
    routes: HashMap<String, mpsc::Sender<BusMessage>>,
    /// 每 route 的满载滞留车道（dispatcher 写入，排空任务消费）。
    overflow: Arc<tokio::sync::Mutex<HashMap<String, OverflowLane>>>,
    global_rx: mpsc::Receiver<BusMessage>,
}

impl OutboundDispatcher {
    pub fn new(global_rx: mpsc::Receiver<BusMessage>) -> Self {
        OutboundDispatcher {
            routes: HashMap::new(),
            overflow: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
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

    /// 尝试把消息交给 route 的滞留车道。
    ///
    /// 返回 `None` = 消息已被车道接管（入队保序或积压超限丢弃并记日志）；返回
    /// `Some(msg)` = 车道不在岗，调用方走 `try_send` 快路径。
    async fn lane_enqueue(
        overflow: &tokio::sync::Mutex<HashMap<String, OverflowLane>>,
        msg: BusMessage,
    ) -> Option<BusMessage> {
        let mut lanes = overflow.lock().await;
        // 车道不存在/不在岗 = 快路径：把消息还给调用方走 try_send。
        // （此处不得用 `?`——get_mut 的 None 是「无车道」，不是「已接管」。）
        let Some(lane) = lanes.get_mut(&msg.channel) else {
            return Some(msg);
        };
        if !lane.active {
            return Some(msg);
        }
        if msg.is_intermediate() && lane.pending.len() >= OVERFLOW_LANE_CAP {
            // 积压超限：增量可丢（最终回复始终保留），debug 日志可观测。
            tracing::debug!(
                "通道 {} 滞留车道积压超限，丢弃进度增量: chat_id={} ({} 字节)",
                msg.channel,
                msg.chat_id,
                msg.content.len()
            );
            return None;
        }
        lane.pending.push_back(msg);
        None
    }

    /// 持续消费全局 outbound 并分发到各 route 的内部队列。
    ///
    /// 对每个 route 仅做 `try_send`（非阻塞）：
    /// - 成功 → 消息进入该 route 的转发任务，由它负责真实投递；
    /// - 滞留车道在岗 → 本 route 新消息一律入道保序（见 [`OverflowLane`]）；
    /// - 内部队列满（`Full`）→ 按消息类型分流：进度增量（intermediate）进入
    ///   dead-letter（error 级日志，含正文），【只丢这一条 / 这一路】；**最终回复**
    ///   激活该 route 的滞留车道按序滞留，等队列腾位后照常投递，不因瞬时拥塞静默
    ///   消失；
    /// - 转发任务已退出（`Closed`）→ warn 后继续。
    pub async fn run(mut self) {
        tracing::info!("Outbound 分发器已启动");
        while let Some(msg) = self.global_rx.recv().await {
            // 先克隆发送端再进入分支：Full 的最终回复分支要把 Sender 移交进
            // 排空任务，不能借用 self.routes 内的句柄。
            let Some(queue_tx) = self.routes.get(&msg.channel).cloned() else {
                // 无对应通道（如 system 消息）→ 仅记录，不报错。
                tracing::info!(
                    "无对应通道的消息 [{}:{}] ({} 字节)",
                    msg.channel,
                    msg.chat_id,
                    msg.content.len()
                );
                continue;
            };
            // 滞留车道在岗：本 route 新消息一律入道（含增量——它们若被丢弃或
            // try_send 抢空位，都会越过先滞留的最终回复，破坏交付顺序）。
            let msg = match Self::lane_enqueue(&self.overflow, msg).await {
                Some(msg) => msg,
                None => continue,
            };
            match queue_tx.try_send(msg) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(msg)) => {
                    if msg.is_intermediate() {
                        // 该 route 的转发任务消费过慢，内部队列已满。进度增量
                        // 可丢：dead-letter，error 级日志让投递失败可观测。
                        //
                        // 隐私/日志体积（修复 P2「dead-letter 日志泄露完整消息正文」）：
                        // 与下方 `forward_route` 的超时分支保持一致——只记录 channel、
                        // chat_id、字节数与【200 字符预览】，【绝不】把完整 msg.content
                        // 写入日志。outbound 可能承载用户提示、工具结果或隐私内容，
                        // 完整落盘到 error 级日志会造成泄露并放大日志压力。
                        let preview: String = msg.content.chars().take(200).collect();
                        tracing::error!(
                            "向通道 {} 的转发队列已满（通道消费过慢），消息进入 dead-letter: \
                             chat_id={} ({} 字节) 正文预览={:?}",
                            msg.channel,
                            msg.chat_id,
                            msg.content.len(),
                            preview,
                        );
                    } else {
                        // 最终回复不许丢：入滞留车道按序等待。车道空闲则同时 spawn
                        // 排空任务；在岗则只入队（已有排空任务在消费）。锁内只做
                        // 队列操作，全局循环不被阻塞。
                        let (channel, should_spawn) = {
                            let mut lanes = self.overflow.lock().await;
                            let channel = msg.channel.clone();
                            let lane = lanes.entry(channel.clone()).or_default();
                            lane.pending.push_back(msg);
                            if lane.active {
                                (channel, false)
                            } else {
                                lane.active = true;
                                (channel, true)
                            }
                        };
                        if should_spawn {
                            tokio::spawn(drain_overflow_lane(
                                channel,
                                queue_tx,
                                Arc::clone(&self.overflow),
                            ));
                        }
                    }
                }
                Err(mpsc::error::TrySendError::Closed(msg)) => {
                    tracing::warn!(
                        "通道 {} 的转发任务已退出（已关闭），消息丢弃: chat_id={} ({} 字节)",
                        msg.channel,
                        msg.chat_id,
                        msg.content.len()
                    );
                }
            }
        }
        tracing::info!("Outbound 分发器退出");
    }
}

/// 单个 route 滞留车道的排空任务：按滞留顺序把消息送回内部队列。
///
/// 每条消息 `send().await` 等队列腾位——本任务独立运行，等位只阻塞自己，
/// 不阻塞全局分发循环与其它 route。顺序保证：车道激活后该 route 的一切新消息
/// 都从车道尾部入队（dispatcher 保证），本任务是唯一消费者，先滞留者先送出。
/// 退出条件：车道取空（复位 active，后续消息回到 try_send 快路径）或转发任务
/// 已退出（清残留车道后退出）。
async fn drain_overflow_lane(
    channel: String,
    queue_tx: mpsc::Sender<BusMessage>,
    overflow: Arc<tokio::sync::Mutex<HashMap<String, OverflowLane>>>,
) {
    loop {
        // 锁内只做取批判断（持锁尽量短）；整批取走后逐条等位投递。
        let batch: VecDeque<BusMessage> = {
            let mut lanes = overflow.lock().await;
            let Some(lane) = lanes.get_mut(&channel) else {
                return;
            };
            if lane.pending.is_empty() {
                lane.active = false;
                return;
            }
            std::mem::take(&mut lane.pending)
        };
        for msg in batch {
            let chat_id = msg.chat_id.clone();
            let bytes = msg.content.len();
            if queue_tx.send(msg).await.is_err() {
                tracing::warn!(
                    "通道 {channel} 的转发任务已退出（已关闭），滞留消息丢弃: \
                     chat_id={chat_id} ({bytes} 字节)"
                );
                let mut lanes = overflow.lock().await;
                if let Some(lane) = lanes.get_mut(&channel) {
                    lane.pending.clear();
                    lane.active = false;
                }
                return;
            }
        }
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
        // 超时按消息类型分流：进度增量 5s（可丢），最终回复加长宽限（慢消费端
        // 恢复前台即毫秒级排空，见 [`SEND_TIMEOUT_FINAL`]）。
        let per_msg_timeout = if msg.is_intermediate() {
            SEND_TIMEOUT
        } else {
            SEND_TIMEOUT_FINAL
        };
        match timeout(per_msg_timeout, tx.send(msg)).await {
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
                    per_msg_timeout.as_secs(),
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

#[cfg(test)]
mod dispatcher_tests {
    use super::*;
    use rclaw_core::BusMessage;

    fn final_msg(tag: usize) -> BusMessage {
        BusMessage::new(
            "desktop".to_string(),
            "c-test".to_string(),
            format!("最终回复{tag:03}"),
        )
    }

    fn delta_msg(tag: usize) -> BusMessage {
        BusMessage::new(
            "desktop".to_string(),
            "c-test".to_string(),
            format!("💭 {tag:03}"),
        )
        .with_intermediate()
    }

    /// 从消息尾部解析 3 位定宽标签。
    fn tag_of(msg: &BusMessage) -> usize {
        let content = &msg.content;
        msg.content[content.len() - 3..].parse().unwrap_or(0)
    }

    /// 把 desktop 路由推到【内部队列满载】的确定状态（codex r1 P2：满载前提不能靠
    /// sleep 碰运气——旧实现的直接丢弃必须无法通过本测试）。容量无关：改
    /// [`ROUTE_QUEUE_CAP`] 不破坏本配方。
    ///
    /// current_thread runtime 下调度可控：下游 rx 容量 1 且不被消费 → 稳态下转发
    /// 任务恰好送出 1 条（入 rx 缓冲）、卡死在第 2 条上，即至多腾出 2 个空位。
    /// 首批 `ROUTE_QUEUE_CAP + 10` 条增量把队列灌到满（溢出部分被 dead-letter）；
    /// 再补 5 条（> 2 个可能空位）：前 2 条填满队列，其后必撞 Full 被 dead-letter
    /// （金丝雀效应，同时证明队列此刻确实满载）。此后任何消息的 try_send 都必然
    /// 走 Full 分支。
    async fn saturate_desktop_route(gtx: &mpsc::Sender<BusMessage>) {
        for i in 0..ROUTE_QUEUE_CAP + 10 {
            gtx.send(delta_msg(i)).await.unwrap();
        }
        // 单次 sleep 内到达稳态：分发器清空首批，转发任务推进到卡死点。稳态
        // （队列满、转发卡死、空位 ≤ 2）与调度次序无关。
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        // 补位标签取 burst（0..CAP+10）之外、车道增量 202 之下，保证递增断言有效。
        for i in 150..155 {
            gtx.send(delta_msg(i)).await.unwrap();
        }
    }

    /// 满载回归（2026-10-21 真机实测复刻）：满载时到达的最终回复经滞留车道保序
    /// 投递，绝不随进度增量一起进 dead-letter（用户侧表现为「采集完成了却看不到
    /// 结果」）；后续消息在车道在岗期间一律追加，交付顺序 = 到达顺序。
    #[tokio::test]
    async fn final_reply_survives_full_route_queue_in_arrival_order() {
        let (tx, mut rx) = mpsc::channel::<BusMessage>(1);
        let (gtx, grx) = mpsc::channel::<BusMessage>(64);
        let mut dispatcher = OutboundDispatcher::new(grx);
        dispatcher.route_to("desktop", tx);
        tokio::spawn(dispatcher.run());

        saturate_desktop_route(&gtx).await;
        // 满载已由金丝雀证明。最终回复 A 必走滞留车道；随后到达的增量 B、最终
        // 回复 C 也必须入道排在 A 后（不得 try_send 抢位越权）。
        gtx.send(final_msg(201)).await.unwrap();
        gtx.send(delta_msg(202)).await.unwrap();
        gtx.send(final_msg(203)).await.unwrap();
        // 消费下游：全部可投递消息按序流出。
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut seq: Vec<(bool, usize)> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv()).await {
                Ok(Some(msg)) => {
                    seq.push((msg.is_intermediate(), tag_of(&msg)));
                }
                _ => break,
            }
        }
        // 吸收账本（结构不变量，容量无关）：最终回复不丢且保序；车道增量 202
        // 必达；全部增量交付严格递增（= 到达序，金丝雀与其余被丢增量不在其中）。
        let deltas: Vec<usize> = seq.iter().filter(|(i, _)| *i).map(|(_, t)| *t).collect();
        let finals: Vec<usize> = seq.iter().filter(|(i, _)| !*i).map(|(_, t)| *t).collect();
        assert_eq!(finals, vec![201, 203], "最终回复不得被丢且必须保序");
        assert!(deltas.contains(&202), "车道增量必须投递");
        // 交付顺序 = 到达顺序：A(201) < B(202) < C(203)，且先于 A 的消息不乱序。
        let pos = |t: usize| seq.iter().position(|(_, x)| *x == t).unwrap();
        assert!(
            pos(201) < pos(202) && pos(202) < pos(203),
            "车道内乱序: {seq:?}"
        );
        assert!(
            deltas.windows(2).all(|w| w[0] < w[1]),
            "增量交付乱序: {deltas:?}"
        );
        assert_eq!(
            deltas.len(),
            seq.len() - 2,
            "seq 中非增量必须是两条最终回复"
        );
    }

    /// 满载滞留不得拖累全局分发：desktop 满载且最终回复在车道等位时，其它 route
    /// 的消息照常路由（跨 route 阻塞是本架构明确消灭的属性）。
    #[tokio::test]
    async fn full_route_lane_does_not_block_other_routes() {
        let (tx, _rx) = mpsc::channel::<BusMessage>(1);
        let (nap_tx, mut nap_rx) = mpsc::channel::<BusMessage>(16);
        let (gtx, grx) = mpsc::channel::<BusMessage>(64);
        let mut dispatcher = OutboundDispatcher::new(grx);
        dispatcher.route_to("desktop", tx);
        dispatcher.route_to("napcat", nap_tx);
        tokio::spawn(dispatcher.run());

        saturate_desktop_route(&gtx).await;
        gtx.send(final_msg(201)).await.unwrap();
        // 不消费 desktop 接收端（最终回复滞留在车道），napcat 消息必须照常到达。
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        gtx.send(BusMessage::new(
            "napcat".to_string(),
            "qq-1".to_string(),
            "跨通道消息".to_string(),
        ))
        .await
        .unwrap();
        let nap = tokio::time::timeout(std::time::Duration::from_secs(2), nap_rx.recv()).await;
        assert!(
            matches!(nap, Ok(Some(_))),
            "desktop 满载滞留期间 napcat 路由不得被阻塞"
        );
    }
}
