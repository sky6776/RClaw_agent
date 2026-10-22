//! 后台完成通知的批量唤醒器（ZCode 长程能力移植 · Layer 4）。
//!
//! 移植前 RClaw 的后台子 Agent 完成通知只入团队邮箱（未读计数），主循环在下一个
//! 自然迭代从尾部提示得知——但父 turn 已收尾时，没有任何机制驱动新轮次去收集结果，
//! 「干等 or 漏看」二选一（大规模并行下父 turn 往往先于子任务结束）。
//!
//! 本模块在完成入箱后追加一条【合批唤醒】：防抖窗口内的多个完成合并为一条
//! Heartbeat 来源的系统消息投给发起任务的会话——驱动 [`crate::turn::TurnManager`]
//! 启动收集轮次（若有活动 turn 则按既有语义延后为下一轮），窗口内 N 个完成只产生
//! 一次模型请求，不按完成数线性放大。火化时未读为 0（父已在活动 turn 里用 TaskList
//! 消费）则降级为一条轻量提醒，避免多余轮次。
//!
//! 与「不唤醒」旧设计的关系：尾部 `[System hint]` 提示保留（活动 turn 的实时可见性），
//! 唤醒补的是「turn 结束后无人驱动」的空窗。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::mpsc;

use rclaw_core::BusMessage;
use rclaw_store::journal::JournalStore;

use crate::agent_tools::read_team_inbox_unread;

type WakeTx = mpsc::Sender<BusMessage>;

/// 批量唤醒器。Clone 廉价（内部全是 Arc 共享）；`attach` 一次注入 inbound 发送端。
///
/// 生命周期：随 [`crate::agent_tools::AgentToolRuntime`] 构造，二进制建好 inbound
/// 总线后 `attach(inbound_tx.clone())`（与 bot_router_slot 的回填模式一致）。
#[derive(Clone)]
pub struct BackgroundWaker {
    inner: Arc<Inner>,
}

struct Inner {
    tx: OnceLock<WakeTx>,
    /// state.sqlite journal/kv 句柄：火化时读团队邮箱未读计数（与尾部提示同源）。
    journal: JournalStore,
    /// 待唤醒目标（去重集合）：防抖窗口内同一会话的多个完成只留一条。
    pending: Mutex<HashSet<(String, String)>>,
    /// 是否已有防抖定时器在跑（避免每个完成各起一个 task）。
    timer_running: AtomicBool,
    enabled: bool,
    debounce: Duration,
}

impl BackgroundWaker {
    pub fn new(journal: JournalStore, enabled: bool, debounce: Duration) -> Self {
        BackgroundWaker {
            inner: Arc::new(Inner {
                tx: OnceLock::new(),
                journal,
                pending: Mutex::new(HashSet::new()),
                timer_running: AtomicBool::new(false),
                enabled,
                debounce,
            }),
        }
    }

    /// 注入 inbound 发送端（二进制装配期调用一次；重复调用以首次为准）。
    pub fn attach(&self, tx: WakeTx) {
        let _ = self.inner.tx.set(tx);
    }

    /// 直接投递一条系统消息（工作流 outbox 用；不做防抖合批 / 未读计数）。
    pub async fn send_message(&self, message: BusMessage) -> bool {
        let inner = &*self.inner;
        // 工作流通知不走防抖合批：enabled 只控制后台子 Agent 的唤醒，不抑制通知投递。
        let Some(tx) = inner.tx.get() else {
            return false;
        };
        tx.send(message).await.is_ok()
    }

    /// 一个后台子 Agent 终态落盘后调用（幂等：同会话窗口内去重）。
    ///
    /// 未启用 / 未 attach / 缺会话路由时零开销返回。
    pub fn notify(&self, channel: &str, chat_id: &str) {
        let inner = &*self.inner;
        if !inner.enabled || inner.tx.get().is_none() {
            return;
        }
        if channel.is_empty() || chat_id.is_empty() {
            return;
        }
        {
            let mut pending = inner
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if !pending.insert((channel.to_string(), chat_id.to_string())) {
                return; // 已在窗口内，合并
            }
        }
        // 起一个防抖定时器（只有窗口内第一个完成会起；火化后清标志）。
        if inner
            .timer_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let waker = self.clone();
        let debounce = inner.debounce;
        tokio::spawn(async move {
            // 定时器任务若在到期前被丢弃（所在 runtime 关闭），复位标志：否则 timer_running
            // 永远为 true，之后每次 notify 的 CAS 都失败，pending 只进不出、再也不会唤醒。
            // 正常到期由 fire() 在锁内清标志，这里解除守卫即可。
            let mut reset = TimerReset(Some(waker.clone()));
            tokio::time::sleep(debounce).await;
            reset.0.take();
            waker.fire();
        });
    }

    /// 防抖窗口到期：合并待唤醒目标，逐会话投一条系统消息。
    fn fire(&self) {
        let inner = &*self.inner;
        let targets: Vec<(String, String)> = {
            let mut pending = inner
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            inner.timer_running.store(false, Ordering::Release);
            std::mem::take(&mut *pending).into_iter().collect()
        };
        let Some(tx) = inner.tx.get() else {
            return;
        };
        let unread = read_team_inbox_unread(&inner.journal);
        for (channel, chat_id) in targets {
            let content = if unread > 0 {
                format!(
                    "[系统通知] 后台子 Agent 完成通知已就绪（当前未读 {unread} 条）。\
                     请用 TaskList 查看各执行的结果摘要并决定后续动作：需要跟进的继续派发，\
                     已完成的汇总收尾。若全部工作已收尾，请直接给出最终汇总。"
                )
            } else {
                // 未读已被 TaskList 清零（父在活动 turn 里消费过）或任务无团队邮箱：
                // 降级为轻量核对提醒，不驱动一次专门的收集轮次。
                "[系统通知] 有后台子 Agent 任务已结束。请用 AgentStatus/TaskList 核对结果状态；\
                 若无需跟进请继续或收尾当前工作。"
                    .to_string()
            };
            let message = BusMessage::new_heartbeat(channel, chat_id, content);
            if let Err(e) = tx.try_send(message) {
                tracing::warn!("后台完成唤醒消息投递失败（inbound 满或关闭）: {e}");
            }
        }
    }
}

/// 防抖定时器任务的复位守卫：持有 `Some` 时 Drop 复位 `timer_running`（任务被丢弃路径）；
/// 正常到期前 `take()` 解除，交由 `fire()` 在锁内清标志。
struct TimerReset(Option<BackgroundWaker>);

impl Drop for TimerReset {
    fn drop(&mut self) {
        if let Some(waker) = self.0.take() {
            waker.inner.timer_running.store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_journal(name: &str) -> JournalStore {
        let dir = std::env::temp_dir().join(format!(
            "rclaw-wake-{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        JournalStore::open(&rclaw_store::fs::Layout::new(dir.join("agent"))).unwrap()
    }

    fn waker_for_test() -> BackgroundWaker {
        BackgroundWaker::new(test_journal("test"), true, Duration::from_millis(20))
    }

    #[tokio::test]
    async fn notify_dedupes_within_window_and_fires_once() {
        let (tx, mut rx) = mpsc::channel::<BusMessage>(8);
        let waker = waker_for_test();
        waker.attach(tx);
        // 同一会话 3 次完成 → 窗口内合并，只投 1 条。
        for _ in 0..3 {
            waker.notify("cli", "console");
        }
        let first = rx.recv().await.expect("应有唤醒消息");
        assert_eq!(first.channel, "cli");
        assert_eq!(first.chat_id, "console");
        assert!(first.content.contains("后台子 Agent"));
        // 不同会话各自一条。
        waker.notify("weixin", "user-1");
        let second = rx.recv().await.expect("第二会话应有唤醒消息");
        assert_eq!(second.chat_id, "user-1");
        assert!(rx.try_recv().is_err(), "窗口内不应有第三条");
    }

    #[tokio::test]
    async fn disabled_or_unattached_waker_is_noop() {
        let journal = test_journal("off");
        let (tx, mut rx) = mpsc::channel::<BusMessage>(8);
        let disabled = BackgroundWaker::new(journal.clone(), false, Duration::from_millis(10));
        disabled.attach(tx.clone());
        disabled.notify("cli", "console");
        let unattached = BackgroundWaker::new(journal, true, Duration::from_millis(10));
        unattached.notify("cli", "console");
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(rx.try_recv().is_err(), "禁用/未 attach 时不应投递");
    }
}
