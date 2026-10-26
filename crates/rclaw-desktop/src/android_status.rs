//! 安卓运行状态桥（桌面平台为 no-op 空实现）：
//! 1. turn 进行中：把当前任务（工具调用进度 / 思考 / 过程输出）推给前台
//!    服务通知，通知以绿色 accent + 不定进度动画表现「正在执行」；
//! 2. turn 结束：通知回空闲态（最近活动会话的上下文占用 + 进程内存），
//!    并在 GUI 不在前台时发一条「任务完成」提醒通知；
//! 3. 空闲期间每 30 秒刷新一次空闲态文案。
//!
//! turn 的存在与结束**不从 outbound 文本或投递动作反推**：运行时在权威点回调
//! [`rclaw_runtime::TurnObserver`]（本模块的 [`TurnStatusObserver`]）——
//! `turn_begun`（active map 登记）/ `turn_terminal`（终局回复即将写入 outbound）/
//! `turn_ended`（任务结束，正常 / 取消 / panic 一律触发，与 begun 一一配对）。
//! 用户 turn、cron、workflow 唤醒、bot 交接、`/compact` 等经 TurnManager 启动的 turn
//! 在忙碌计数上一视同仁（只排除 grp- 内部编排会话与 heartbeat 自检，见 [`tracked`]）：
//! 无需按来源分别登记，没有需要超时自愈的推断态，也不依赖 `/cancel` 回执文案；
//! 完成提醒只对顶层终局发（见 [`notifiable`]）。outbound 抽头（[`on_outbound`]）只
//! 负责过程文案（思考 / 状态行 / intermediate，与前端 App.tsx 的 bus 事件语义一致）。
//!
//! 通知推送在状态锁内完成：「检查是否空闲 → 推送」必须原子，否则异步到达的
//! 空闲文案可能顶掉刚启动 turn 的忙碌态。锁内只有常数时间计数 + 一次 JNI 调用。

// 通知状态机的调用方都在 cfg(android+apk-host) 的装配/事件桥里（agent.rs 的
// status tap、装配闭包等）；桌面与 bin target 编译时无调用方，压掉 dead_code。
#![cfg_attr(
    not(all(target_os = "android", feature = "apk-host")),
    allow(dead_code)
)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use rclaw_core::{BusMessage, MessageSource, RuntimeChannelInfo};
use rclaw_runtime::{AgentOptions, TurnObserver};
use rclaw_store::fs::Layout;
use rclaw_store::memory::MemoryStore;
use rclaw_store::session::SessionStore;
use rclaw_tools::{ToolRegistry, WebSearchControl};

/// 通知文案上限（字符）：工具进度行可能带长命令/长路径，截断保可读。
const TEXT_MAX_CHARS: usize = 100;
/// 同文案推送节流：流式思考/过程输出一秒多条，重复内容不值得重发通知。
const PUSH_THROTTLE: Duration = Duration::from_secs(2);
/// 完成提醒摘录上限（字符）。
const DONE_EXCERPT_CHARS: usize = 120;

struct StatusState {
    /// 活动 turn 计数：chat_id → 尚未结束的 turn 数。同 chat 并发的 cron job 各自
    /// 独立执行，可 >1；begun / ended 由运行时严格配对，计数无需过期清理。
    active: HashMap<String, u32>,
    /// 最近活动的桌面会话（空闲态上下文估算对象；估算按 desktop 通道查会话，
    /// 外部通道 / heartbeat 的 chat_id 在桌面侧不存在，bot 交接收件箱 `bot-<name>`
    /// 不是用户会话，都不作为对象）。
    last_chat: Option<String>,
    /// 上次推送（文案, 忙碌标志, 时刻），用于节流：同文案同状态才算重复，忙碌 ↔
    /// 空闲的翻转永远不被吞掉（不依赖「忙碌文案与空闲文案恰好不同」的隐含约定）。
    last_push: Option<(String, bool, Instant)>,
}

impl StatusState {
    fn new() -> Self {
        StatusState {
            active: HashMap::new(),
            last_chat: None,
            last_push: None,
        }
    }

    /// 登记一个已启动的 turn，返回该会话的活动计数。
    fn begin(&mut self, chat_id: &str) -> u32 {
        let n = self.active.entry(chat_id.to_string()).or_insert(0);
        *n = n.saturating_add(1);
        *n
    }

    /// 注销一个已结束的 turn。计数归零即移除；未登记的会话（防御：ended 与 begun
    /// 由运行时配对，正常不会出现）为 no-op，不会下溢。
    fn end(&mut self, chat_id: &str) {
        if let Some(n) = self.active.get_mut(chat_id) {
            if *n > 1 {
                *n -= 1;
            } else {
                self.active.remove(chat_id);
            }
        }
    }

    /// 全局是否已无任何在跑 turn。
    fn is_idle(&self) -> bool {
        self.active.is_empty()
    }

    /// 该会话是否有在跑的 turn。
    fn is_active(&self, chat_id: &str) -> bool {
        self.active.contains_key(chat_id)
    }

    /// 清空全部活动计数（装配 runtime 时调用；每进程只装配一次，这里是防御性归零）。
    fn reset(&mut self) {
        self.active.clear();
    }

    /// 推送到前台服务通知（节流 + 截断；桌面平台 no-op）。持锁调用：节流判定、
    /// 调用方的空闲 / 活动判定与实际推送在同一临界区内，推送顺序即状态变化顺序。
    fn push(&mut self, text: &str, busy: bool) {
        let truncated = if text.chars().count() > TEXT_MAX_CHARS {
            let mut t: String = text.chars().take(TEXT_MAX_CHARS).collect();
            t.push('…');
            t
        } else {
            text.to_string()
        };
        if let Some((last, last_busy, at)) = &self.last_push {
            if *last == truncated && *last_busy == busy && at.elapsed() < PUSH_THROTTLE {
                return;
            }
        }
        #[cfg(all(target_os = "android", feature = "apk-host"))]
        {
            if let Err(e) = rclaw_android::update_runtime_status(&truncated, busy) {
                tracing::debug!("通知状态推送失败: {e}");
            }
        }
        self.last_push = Some((truncated, busy, Instant::now()));
    }
}

/// 取全局状态锁。锁内只有计数与一次 JNI 调用；若持锁时 panic（毒化），里面只是
/// 可重建的计数 / 节流记录，直接沿用而不是让之后每次状态更新都跟着 panic。
fn lock_state() -> MutexGuard<'static, StatusState> {
    static STATE: OnceLock<Mutex<StatusState>> = OnceLock::new();
    STATE
        .get_or_init(|| Mutex::new(StatusState::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

// ---------------------------------------------------------------------------
// 运行时生命周期观察者
// ---------------------------------------------------------------------------

/// 运行时 turn 生命周期 → 前台通知状态机的桥（装配时经
/// `AgentLoop::set_turn_observer` 安装）。回调在运行时任务内同步触发，这里只做
/// 常数时间的计数更新 + 一次通知推送。
pub struct TurnStatusObserver;

impl TurnObserver for TurnStatusObserver {
    fn turn_begun(&self, msg: &BusMessage) {
        if !tracked(msg) {
            return;
        }
        // 空闲态估算对象只认桌面用户会话：bot 交接子轮也走 desktop 通道，但其
        // chat_id 是 bot 收件箱（bot-<name>），不是用户正在看的会话。
        let is_desktop_chat =
            msg.channel == crate::agent::CHANNEL_DESKTOP && msg.source() != MessageSource::Agent;
        turn_begun(&msg.chat_id, is_desktop_chat, &begun_text(msg));
    }

    fn turn_terminal(&self, msg: &BusMessage, reply: &str) {
        if !notifiable(msg) {
            return;
        }
        turn_terminal(&msg.chat_id, reply);
    }

    fn turn_ended(&self, msg: &BusMessage) {
        if !tracked(msg) {
            return;
        }
        turn_ended(&msg.chat_id);
    }
}

/// 该 turn 是否参与忙碌计数：grp- 内部编排会话（与 outbound_bridge / 状态抽头
/// 同过滤）与 heartbeat 自检（system 通道、用户不可见、HEARTBEAT_OK 不值得提醒）
/// 不参与；其余来源（用户 / cron / workflow / bot 交接）一律参与。
fn tracked(msg: &BusMessage) -> bool {
    !msg.chat_id.starts_with("grp-") && msg.source() != MessageSource::Heartbeat
}

/// 该 turn 的终局是否值得发「任务完成」提醒：在参与计数的基础上，还要求是顶层
/// 终局——用户消息 / 定时任务 / 工作流唤醒。bot 交接子轮（`Agent` 来源，回复目标
/// `bot-<name>`）的终局只是父 turn 的一个中间步骤，父 turn 仍在跑，此时弹「任务
/// 完成」是误报；父 turn 自己的终局会提醒。
///
/// 不按通道过滤：外部通道（微信 / QQ）turn 的完成提醒是既定产品行为（状态抽头接全部
/// 出站时即如此；决定记录在 agent.rs 安装观察者处）——无头服务模式下手机是 bot 宿主，
/// 宿主需要知道后台替人回了消息。此前只有产生过过程行（工具 / 思考）的外部 turn 才
/// 提醒，纯问答 turn 静默，那是推断实现的副作用而非设计；如需只提醒桌面会话，在这里
/// 加 `msg.channel == CHANNEL_DESKTOP` 即可。
fn notifiable(msg: &BusMessage) -> bool {
    tracked(msg)
        && matches!(
            msg.source(),
            MessageSource::User | MessageSource::Cron | MessageSource::Workflow
        )
}

/// turn 启动时的忙碌文案：cron 轮次全程无过程输出，直接显示任务名；`/compact`
/// 同样没有过程行（运行时用内容为 `/compact` 的占位消息上报它——用户输入的
/// `/compact` 会被当作命令拦截，永远不会以普通 turn 的身份到这里）。
fn begun_text(msg: &BusMessage) -> String {
    match msg.cron_job_name() {
        Some(name) if msg.is_cron_task() => format!("⏰ 定时任务：{name}"),
        _ if msg.content == "/compact" => "正在压缩会话…".to_string(),
        _ => "正在处理消息…".to_string(),
    }
}

/// turn 已启动：登记计数并进入忙碌态。`is_desktop_chat` 为真时作为空闲态估算对象。
fn turn_begun(chat_id: &str, is_desktop_chat: bool, text: &str) {
    let mut s = lock_state();
    let active = s.begin(chat_id);
    if is_desktop_chat {
        s.last_chat = Some(chat_id.to_string());
    }
    tracing::debug!("[status] turn_begun chat={chat_id} active={active}");
    s.push(text, true);
}

/// 终局回复即将交付：GUI 不在前台时发完成提醒。能走到这里的都是真实 turn 的终局
/// （命令回复、`/cancel` 回执、「当前没有正在运行的任务」等不是终局，运行时不会
/// 上报），因此无需再判断「是否忙碌过」。
fn turn_terminal(chat_id: &str, reply: &str) {
    let fg = app_foreground();
    tracing::debug!("[status] turn_terminal chat={chat_id} fg={fg}");
    emit_done_notification(reply, !fg);
}

/// turn 已结束（正常 / 取消 / panic）：注销计数；全部会话空闲时刷新空闲态文案
/// （估算异步完成，推送前会再次确认仍空闲，见 [`push_if_idle`]）。
fn turn_ended(chat_id: &str) {
    let now_idle = {
        let mut s = lock_state();
        s.end(chat_id);
        s.is_idle()
    };
    tracing::debug!("[status] turn_ended chat={chat_id} now_idle={now_idle}");
    if now_idle {
        refresh_idle();
    }
}

// ---------------------------------------------------------------------------
// outbound 抽头：过程文案
// ---------------------------------------------------------------------------

/// outbound 事件分类（与前端 App.tsx 同语义）：思考 / 状态行 / intermediate 更新
/// 任务文案；其余（终局回复、命令回复、取消回执）不改变状态——turn 的收尾由
/// [`TurnStatusObserver`] 权威上报。grp- 内部会话已在调用方过滤。
pub fn on_outbound(chat_id: &str, content: &str, intermediate: bool) {
    tracing::debug!(
        "[status] outbound chat={chat_id} inter={intermediate} content={}",
        content.chars().take(40).collect::<String>()
    );
    if content == "🟢 Agent工作中..." {
        return;
    }
    if content.starts_with('💭') {
        // 思考流是增量片段，内容不能直接展示；固定文案交给节流去重。
        task_progress(chat_id, "💭 思考中…");
    } else if is_status_line(content) || intermediate {
        task_progress(chat_id, content);
    }
}

/// 过程输出（工具进度 / 思考 / intermediate）：更新忙碌态文案。该会话没有活动 turn
/// 时忽略——过程行经异步抽头到达，可能晚于本会话的 turn_ended；此时若再推忙碌文案，
/// 要么把刚刷新的空闲态顶掉并停留到下一次空闲刷新，要么把别的会话的忙碌文案换成
/// 已结束 turn 的过程行。
pub fn task_progress(chat_id: &str, text: &str) {
    let mut s = lock_state();
    if !s.is_active(chat_id) {
        tracing::debug!("[status] 忽略无活动 turn 的过程输出 chat={chat_id}");
        return;
    }
    s.push(text, true);
}

/// 当前平台的前台状态：安卓读 GUI 生命周期标志；桌面恒前台（无通知可发）。
fn app_foreground() -> bool {
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        rclaw_android::app_foreground()
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {
        true
    }
}

/// 按门闸结果发完成提醒（桌面平台 no-op）。
fn emit_done_notification(reply: &str, notify: bool) {
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        if notify {
            let excerpt: String = reply.chars().take(DONE_EXCERPT_CHARS).collect();
            let excerpt = if reply.chars().count() > DONE_EXCERPT_CHARS {
                format!("{excerpt}…")
            } else {
                excerpt
            };
            if let Err(e) = rclaw_android::notify_turn_done(&excerpt) {
                tracing::debug!("完成通知发送失败: {e}");
            }
        }
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {
        let _ = (reply, notify);
    }
}

// ---------------------------------------------------------------------------
// 空闲态
// ---------------------------------------------------------------------------

/// 空闲态估算所需的装配期输入（GUI 与无头服务两条装配路径共用；不再依赖
/// tauri AppState——服务无头装配时没有 AppHandle，此前空闲文案因此在纯
/// 服务模式下无人刷新，是通知功能的存量缺口）。
pub(crate) struct IdleInputs {
    session: SessionStore,
    memory: MemoryStore,
    layout: Layout,
    tools: Arc<ToolRegistry>,
    opts: AgentOptions,
    channels: RuntimeChannelInfo,
    web_search: Option<Arc<WebSearchControl>>,
}

static IDLE_INPUTS: OnceLock<IdleInputs> = OnceLock::new();

/// 装配完成时安装估算输入并启动空闲刷新线程（每进程一次；重复调用 no-op）。
pub fn install_idle_inputs(
    session: SessionStore,
    memory: MemoryStore,
    layout: Layout,
    tools: Arc<ToolRegistry>,
    opts: AgentOptions,
    channels: RuntimeChannelInfo,
    web_search: Option<Arc<WebSearchControl>>,
) {
    // 新 runtime 装配 = 进程内旧 turn 不可能还活着：清空上个 runtime 遗留的活动
    // 计数，避免前台通知停在忙碌态（旧 turn 迟到的 ended 对未登记会话是 no-op）。
    lock_state().reset();
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        let _ = IDLE_INPUTS.set(IdleInputs {
            session,
            memory,
            layout,
            tools,
            opts,
            channels,
            web_search,
        });
        spawn_idle_refresher();
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {
        let _ = (session, memory, layout, tools, opts, channels, web_search);
    }
}

/// 空闲态刷新：最近活动会话的上下文占用 + 进程内存。估算要扫技能目录，
/// 放阻塞线程池；agent 未启动时跳过；无会话 / 估算失败时退化为只报内存——
/// 空闲态文案必须到位，否则通知会停在上一条忙碌文案（绿色进度动画）直到下次
/// 刷新。估算是异步的：完成时若已有新 turn 启动，放弃本次推送（否则空闲文案会
/// 顶掉忙碌态）。桌面平台直接短路（无通知可发，不付估算开销）。
pub fn refresh_idle() {
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {}
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        let chat_id = lock_state().last_chat.clone();
        let inputs = match IDLE_INPUTS.get() {
            Some(inputs) => inputs,
            None => return,
        };
        let session = inputs.session.clone();
        let memory = inputs.memory.clone();
        let layout = inputs.layout.clone();
        let tools = inputs.tools.clone();
        let opts = inputs.opts.clone();
        let channels = inputs.channels.clone();
        let web_search = inputs.web_search.clone();
        tauri::async_runtime::spawn(async move {
            // 进程刚重启（无头服务先行）还没有任何会话活动，或估算失败：只报内存，
            // 好过停在上一条忙碌文案或静态的「正在本机运行」。
            let usage = match chat_id {
                None => None,
                Some(chat_id) => match tauri::async_runtime::spawn_blocking(move || {
                    rclaw_runtime::estimate_context_usage(
                        &session,
                        &memory,
                        &layout,
                        &tools,
                        &opts,
                        &channels,
                        web_search.as_deref(),
                        crate::agent::CHANNEL_DESKTOP,
                        &chat_id,
                    )
                })
                .await
                {
                    Ok(Ok(u)) => Some(u),
                    Ok(Err(e)) => {
                        tracing::debug!("空闲态上下文估算失败: {e}");
                        None
                    }
                    Err(e) => {
                        tracing::debug!("空闲态估算任务失败: {e}");
                        None
                    }
                },
            };
            let rss_mb = crate::commands::process_rss_bytes() / 1024 / 1024;
            // request_used 已含历史（历史 + system prompt + 动态上下文 + 静态工具面），
            // 与 /context、桌面 UsageDto 同口径；再加 history_used 会把历史算两遍。
            // context_length=0 是「未知」哨兵（按 128K 假设的口径见 turn.rs）；
            // 通知里只展示可用数字，不猜。
            let text = match usage {
                Some(usage) if usage.context_length > 0 => {
                    let used = usage.request_used;
                    let pct = std::cmp::min(100, used * 100 / usage.context_length.max(1));
                    format!(
                        "上下文 {pct}%（{}k/{}k） · 内存 {rss_mb} MB",
                        used / 1000,
                        usage.context_length / 1000
                    )
                }
                _ => format!("内存 {rss_mb} MB"),
            };
            push_if_idle(&text);
        });
    }
}

/// 推送空闲态文案，但仅当此刻仍无活动 turn（异步估算期间可能已有新 turn 启动）；
/// 判定与推送在同一临界区，不会与并发的 turn_begun 交错。
fn push_if_idle(text: &str) {
    let mut s = lock_state();
    if !s.is_idle() {
        tracing::debug!("[status] 放弃过期的空闲态文案（已有新 turn）");
        return;
    }
    s.push(text, false);
}

/// 空闲态定时刷新（30s）：turn 进行中跳过（任务文案由过程输出驱动）。
/// 装配完成时经 [`install_idle_inputs`] 启动（每进程一次，幂等）。
/// 桌面平台 no-op。
pub fn spawn_idle_refresher() {
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        static REFRESHER: OnceLock<()> = OnceLock::new();
        if REFRESHER.set(()).is_err() {
            return;
        }
        const IDLE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
        std::thread::spawn(move || loop {
            std::thread::sleep(IDLE_REFRESH_INTERVAL);
            if lock_state().is_idle() {
                refresh_idle();
            }
        });
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {}
}

/// 工具/技能状态行判定（与前端 api.ts isStatusLine 同表）。
fn is_status_line(content: &str) -> bool {
    content.starts_with("🟢")
        || content.starts_with("🧩")
        || content.starts_with("🔧")
        || content.starts_with("正在调用工具")
        || content.starts_with("（已停止当前任务）")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 计数状态机：同 chat 并发 cron 各自独立，先结束的不清掉仍在跑的；全部结束
    /// 才空闲。
    #[test]
    fn begin_end_counts_balance_per_chat() {
        let mut s = StatusState::new();
        assert!(s.is_idle());
        assert_eq!(s.begin("a"), 1);
        assert_eq!(s.begin("a"), 2, "同 chat 并发 cron 计数累加");
        assert_eq!(s.begin("b"), 1);
        assert!(!s.is_idle());
        s.end("a");
        assert_eq!(
            s.active.get("a"),
            Some(&1),
            "先结束的任务不得清掉仍在跑的计数"
        );
        assert!(!s.is_idle());
        s.end("a");
        assert!(!s.active.contains_key("a"), "计数归零即移除");
        assert!(!s.is_idle(), "另一会话仍在跑");
        s.end("b");
        assert!(s.is_idle());
    }

    /// 未登记会话的收尾（运行时重装配后旧 turn 迟到的 ended）是 no-op，不下溢、
    /// 不产生幽灵计数。
    #[test]
    fn end_without_begin_is_noop() {
        let mut s = StatusState::new();
        s.end("ghost");
        assert!(s.is_idle());
        assert!(!s.active.contains_key("ghost"));
    }

    /// 重装配清空全部计数。
    #[test]
    fn reset_clears_all_counts() {
        let mut s = StatusState::new();
        s.begin("a");
        s.begin("b");
        s.reset();
        assert!(s.is_idle());
    }

    /// 参与过滤：grp- 内部编排会话与 heartbeat 自检不参与；用户 / cron / workflow
    /// 唤醒 / bot 交接一律参与计数。
    #[test]
    fn observer_tracks_user_cron_workflow_agent_but_not_group_or_heartbeat() {
        assert!(tracked(&BusMessage::new("desktop", "chat-1", "hi")));
        assert!(tracked(&BusMessage::new_cron_job(
            "desktop", "chat-1", "job-1", "daily", "run"
        )));
        assert!(tracked(&BusMessage::new_workflow(
            "desktop", "chat-1", "done"
        )));
        assert!(tracked(&BusMessage::new_agent(
            "desktop",
            "bot-helper",
            vec!["main".to_string()],
            7,
            "handoff"
        )));
        assert!(!tracked(&BusMessage::new("desktop", "grp-g1-m1", "hi")));
        assert!(!tracked(&BusMessage::new_heartbeat(
            rclaw_core::CHANNEL_SYSTEM,
            "heartbeat",
            "check"
        )));
    }

    /// 完成提醒只对顶层终局发：bot 交接子轮参与计数但不提醒（父 turn 仍在跑）。
    #[test]
    fn done_notification_only_for_top_level_sources() {
        assert!(notifiable(&BusMessage::new("desktop", "chat-1", "hi")));
        assert!(notifiable(&BusMessage::new_cron_job(
            "desktop", "chat-1", "job-1", "daily", "run"
        )));
        assert!(notifiable(&BusMessage::new_workflow(
            "desktop", "chat-1", "done"
        )));
        assert!(!notifiable(&BusMessage::new_agent(
            "desktop",
            "bot-helper",
            vec!["main".to_string()],
            7,
            "handoff"
        )));
        assert!(!notifiable(&BusMessage::new("desktop", "grp-g1-m1", "hi")));
    }

    /// cron 轮次全程无过程输出：忙碌文案直接带任务名；`/compact` 占位消息显示压缩；
    /// 普通 turn 用通用文案。
    #[test]
    fn begun_text_names_cron_job() {
        assert_eq!(
            begun_text(&BusMessage::new_cron_job(
                "desktop",
                "chat-1",
                "job-1",
                "daily-report",
                "run"
            )),
            "⏰ 定时任务：daily-report"
        );
        assert_eq!(
            begun_text(&BusMessage::new("desktop", "chat-1", "/compact")),
            "正在压缩会话…"
        );
        assert_eq!(
            begun_text(&BusMessage::new("desktop", "chat-1", "hi")),
            "正在处理消息…"
        );
    }

    /// 节流按（文案, 忙碌）去重：同文案下忙碌 ↔ 空闲的翻转不得被吞掉。
    #[test]
    fn throttle_never_swallows_busy_flip() {
        let mut s = StatusState::new();
        s.push("同一句", true);
        s.push("同一句", false);
        assert_eq!(
            s.last_push.as_ref().map(|(t, busy, _)| (t.as_str(), *busy)),
            Some(("同一句", false))
        );
        let at = s.last_push.as_ref().map(|(_, _, at)| *at).unwrap();
        s.push("同一句", false);
        assert_eq!(
            s.last_push.as_ref().map(|(_, _, at)| *at),
            Some(at),
            "同文案同状态 2s 内不重推"
        );
    }

    /// 全局状态机：该会话无活动 turn 时的过程输出被忽略（异步抽头迟到的过程行不得
    /// 把空闲态顶回忙碌，也不得顶掉别的会话的忙碌文案）；有活动 turn 时正常更新
    /// 文案；结束后再次忽略。断言只看本用例前后的相对变化，不假设全局初始态。
    #[test]
    fn progress_is_ignored_without_active_turn() {
        fn last_push_text() -> Option<String> {
            lock_state().last_push.as_ref().map(|(t, _, _)| t.clone())
        }
        let chat = "test-progress-gate";
        let other = "test-progress-gate-other";
        let before = last_push_text();
        task_progress(chat, "🔧 调用工具: late");
        assert_eq!(last_push_text(), before, "无活动 turn 的过程输出不得推送");

        turn_begun(other, true, "正在处理消息…");
        task_progress(chat, "🔧 调用工具: wrong-chat");
        assert_eq!(
            last_push_text().as_deref(),
            Some("正在处理消息…"),
            "别的会话在跑时，本会话（无 turn）的过程行不得推送"
        );

        turn_begun(chat, true, "正在处理消息…");
        task_progress(chat, "🔧 调用工具: shell");
        assert_eq!(last_push_text().as_deref(), Some("🔧 调用工具: shell"));

        turn_ended(chat);
        assert!(lock_state().is_active(other));
        task_progress(chat, "🔧 调用工具: after-end");
        assert_eq!(
            last_push_text().as_deref(),
            Some("🔧 调用工具: shell"),
            "turn 结束后迟到的过程行不得再推送"
        );
        turn_ended(other);
        let s = lock_state();
        assert!(!s.is_active(chat) && !s.is_active(other));
    }
}
