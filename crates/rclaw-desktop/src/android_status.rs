//! 安卓运行状态桥（桌面平台为 no-op 空实现）：
//! 1. turn 进行中：把当前任务（工具调用进度 / 思考 / 过程输出）推给前台
//!    服务通知，通知以绿色 accent + 不定进度动画表现「正在执行」；
//! 2. turn 结束：通知回空闲态（最近活动会话的上下文占用 + 进程内存），
//!    并在 GUI 不在前台时发一条「任务完成」提醒通知；
//! 3. 空闲期间每 30 秒刷新一次空闲态文案。
//!
//! turn 收尾判定与前端 App.tsx 的 bus 事件语义一致：思考（💭）/ 状态行
//! （🟢🧩🔧…）/ intermediate 过程消息不收尾，非 intermediate 的普通消息收尾。

// 通知状态机的调用方都在 cfg(android+apk-host) 的装配/事件桥里（agent.rs 的
// status tap、装配闭包等）；桌面与 bin target 编译时无调用方，压掉 dead_code。
#![cfg_attr(not(all(target_os = "android", feature = "apk-host")), allow(dead_code))]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rclaw_core::RuntimeChannelInfo;
use rclaw_runtime::AgentOptions;
use rclaw_store::fs::Layout;
use rclaw_store::memory::MemoryStore;
use rclaw_store::session::SessionStore;
use rclaw_tools::{ToolRegistry, WebSearchControl};

/// 通知文案上限（字符）：工具进度行可能带长命令/长路径，截断保可读。
const TEXT_MAX_CHARS: usize = 100;
/// 同文案推送节流：流式思考/过程输出一秒多条，重复内容不值得重发通知。
const PUSH_THROTTLE: Duration = Duration::from_secs(2);

struct StatusState {
    /// 忙碌 turn 集合：chat_id → 最近一次活动时刻（过程输出刷新；多会话
    /// 可并行，单槽会在交错时互相误清/漏发完成通知）。
    busy: HashMap<String, Instant>,
    /// 最近活动会话（空闲态上下文估算对象）。
    last_chat: Option<String>,
    /// 上次推送（文案, 时刻），用于节流。
    last_push: Option<(String, Instant)>,
}

fn state() -> &'static Mutex<StatusState> {
    static STATE: OnceLock<Mutex<StatusState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(StatusState {
            busy: HashMap::new(),
            last_chat: None,
            last_push: None,
        })
    })
}

/// 推送到前台服务通知（节流 + 截断；桌面平台 no-op）。
fn push(text: &str, busy: bool) {
    let truncated = if text.chars().count() > TEXT_MAX_CHARS {
        let mut t: String = text.chars().take(TEXT_MAX_CHARS).collect();
        t.push('…');
        t
    } else {
        text.to_string()
    };
    {
        let mut s = state().lock().unwrap();
        if let Some((last, at)) = &s.last_push {
            if *last == truncated && at.elapsed() < PUSH_THROTTLE {
                return;
            }
        }
        s.last_push = Some((truncated.clone(), Instant::now()));
    }
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        if let Err(e) = rclaw_android::update_runtime_status(&truncated, busy) {
            tracing::debug!("通知状态推送失败: {e}");
        }
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {
        let _ = busy;
    }
}

/// 用户发消息（agent::send 在投递【前】调用，防快回合竞速）：turn 进入忙碌态。
pub fn turn_started(chat_id: &str) {
    {
        let mut s = state().lock().unwrap();
        s.busy.insert(chat_id.to_string(), Instant::now());
        s.last_chat = Some(chat_id.to_string());
    }
    tracing::debug!("[status] turn_started chat={chat_id}");
    push("正在处理消息…", true);
}

/// 过程输出（工具进度 / 思考 / intermediate）：更新任务文案，忙碌态保持。
/// bot/定时触发的 turn 不经过 turn_started——见到过程输出即视为忙碌。
pub fn task_progress(chat_id: &str, text: &str) {
    {
        let mut s = state().lock().unwrap();
        s.last_chat = Some(chat_id.to_string());
        s.busy.insert(chat_id.to_string(), Instant::now());
    }
    push(text, true);
}

/// 消息投递失败（agent 关闭等）的回滚：撤掉 turn_started 的忙碌标记并回
/// 空闲态（turn 实际没有发生，不发完成通知）。
pub fn turn_aborted(chat_id: &str) {
    let now_idle = {
        let mut s = state().lock().unwrap();
        s.busy.remove(chat_id);
        s.busy.is_empty()
    };
    if now_idle {
        refresh_idle();
    }
}

/// outbound 事件分类（与前端 App.tsx 同语义；思考/状态/过程更新任务文案，
/// 非 intermediate 普通消息收尾）。grp- 内部会话已在调用方过滤。
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
    } else {
        turn_finished(chat_id, content);
    }
}

/// 最终回复到达：turn 收尾——全部会话空闲时刷新空闲态文案；该会话确实
/// 忙碌过且 GUI 不在前台时发完成提醒。「当前没有正在运行的任务。」这类
/// 无任务回执也是普通最终消息，没有忙碌过就不发提醒（否则空取消也会弹）。
pub fn turn_finished(chat_id: &str, reply: &str) {
    let (was_busy, now_idle) = {
        let mut s = state().lock().unwrap();
        let was_busy = s.busy.remove(chat_id).is_some();
        s.last_chat = Some(chat_id.to_string());
        (was_busy, s.busy.is_empty())
    };
    tracing::debug!("[status] turn_finished chat={chat_id} was_busy={was_busy} now_idle={now_idle}");
    if now_idle {
        refresh_idle();
    }
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        let fg = rclaw_android::app_foreground();
        tracing::debug!("[status] notify gate fg={fg} was_busy={was_busy}");
        if was_busy && !fg {
            let excerpt: String = reply.chars().take(120).collect();
            let excerpt = if reply.chars().count() > 120 {
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
        let _ = (reply, was_busy);
    }
}

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
/// 放阻塞线程池；agent 未启动 / 无会话 / 估算失败时静默跳过（下次再试）。
/// 桌面平台直接短路（无通知可发，不付估算开销）。
pub fn refresh_idle() {
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {}
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        let chat_id = state().lock().unwrap().last_chat.clone();
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
            let Some(chat_id) = chat_id else {
                // 进程刚重启（无头服务先行）还没有任何会话活动：只报内存，
                // 好过停在静态的「正在本机运行」。
                let rss_mb = crate::commands::process_rss_bytes() / 1024 / 1024;
                push(&format!("内存 {rss_mb} MB"), false);
                return;
            };
            let usage = match tauri::async_runtime::spawn_blocking(move || {
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
                Ok(Ok(u)) => u,
                Ok(Err(e)) => {
                    tracing::debug!("空闲态上下文估算失败: {e}");
                    return;
                }
                Err(e) => {
                    tracing::debug!("空闲态估算任务失败: {e}");
                    return;
                }
            };
            let used = usage.history_used + usage.request_used;
            let rss_mb = crate::commands::process_rss_bytes() / 1024 / 1024;
            // context_length=0 是「未知」哨兵（按 128K 假设的口径见 turn.rs）；
            // 通知里只展示可用数字，不猜。
            let text = if usage.context_length > 0 {
                let pct = std::cmp::min(100, used * 100 / usage.context_length.max(1));
                format!(
                    "上下文 {pct}%（{}k/{}k） · 内存 {rss_mb} MB",
                    used / 1000,
                    usage.context_length / 1000
                )
            } else {
                format!("内存 {rss_mb} MB")
            };
            push(&text, false);
        });
    }
}

/// 空闲态定时刷新（30s）：turn 进行中跳过（任务文案由过程输出驱动），但
/// 忙碌条目超时无活动（BUSY_TIMEOUT）会被清理并回落空闲态（收尾 outbound
/// 丢失的自愈）。装配完成时经 [`install_idle_inputs`] 启动（每进程一次，
/// 幂等）。桌面平台 no-op。
pub fn spawn_idle_refresher() {
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        static REFRESHER: OnceLock<()> = OnceLock::new();
        if REFRESHER.set(()).is_err() {
            return;
        }
        const IDLE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
        /// 忙碌自愈上限：收尾 outbound 丢失时（/cancel 回执后仍有零星工具
        /// 进度行把忙碌重新置位、随后 turn 静默退出，turn.rs 取消路径不再补
        /// 发收尾），超时无活动的忙碌条目自动回落空闲。正常长任务期间思考/
        /// 工具进度行会持续刷新时刻戳，不会误伤。
        const BUSY_TIMEOUT: Duration = Duration::from_secs(10 * 60);
        std::thread::spawn(move || loop {
            std::thread::sleep(IDLE_REFRESH_INTERVAL);
            let idle = {
                let now = Instant::now();
                let mut s = state().lock().unwrap();
                s.busy.retain(|_, at| now.duration_since(*at) < BUSY_TIMEOUT);
                s.busy.is_empty()
            };
            if idle {
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
