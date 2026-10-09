//! 静默门（C7）：runtime 生命周期的统一互斥原语。
//!
//! 「有没有东西在跑」此前由三个机制各自补偿——active map 的空检查、review 在途计数、
//! 测试的轮询——且 `/clear` 是 check-then-act、curator 靠抱锁。静默门把它们合并：
//!
//! - 前台 turn 与后台 review 各占一个运行槽位（`turns` / `reviews`）；
//! - 生命周期操作（/clear 确认、skill 生命周期变更、curator 归档）用
//!   [`QuiescenceGate::try_acquire`] 关门：拿不到 = 仍有运行中的工作，按既有话术拒绝；
//!   拿到 = 持门期间新 turn / 新 review 无法启动，竞态按构造消失；
//! - 测试与观测用 [`QuiescenceGate::wait_quiescent`] / [`QuiescenceGate::wait_turns_idle`]
//!   等待，不再轮询。
//!
//! 等待机制用 watch 版本号而非 `Notify`：`Notify` 不存许可，「检查与注册之间」发生的
//! 释放会永久丢失唤醒（tokio `notify_waiters` 只唤醒已注册的等待者）；watch 的
//! `wait_for` 在值已变化时立即返回，没有丢失窗口。
//!
//! 所有临界区都是短同步段（无 await），`std::sync::Mutex` 即可。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

/// 生命周期操作持门守卫：Drop 时开门并推进版本号。
pub(crate) struct QuiescentGuard<'a> {
    gate: &'a QuiescenceGate,
}

impl Drop for QuiescentGuard<'_> {
    fn drop(&mut self) {
        self.gate.held.store(false, Ordering::SeqCst);
        self.gate.bump();
    }
}

/// 静默门：互斥「生命周期操作」与「新 turn / 新 review 的启动」。
pub(crate) struct QuiescenceGate {
    /// 生命周期操作是否正持门（检查 + 置位在 `door` 锁内完成）。
    held: AtomicBool,
    door: Mutex<()>,
    turns: AtomicUsize,
    reviews: AtomicUsize,
    /// 运行槽位 / 持门状态的版本号：等待者按版本变化复查。
    version: watch::Sender<u64>,
}

impl QuiescenceGate {
    pub(crate) fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
            door: Mutex::new(()),
            turns: AtomicUsize::new(0),
            reviews: AtomicUsize::new(0),
            version: watch::channel(0).0,
        }
    }

    /// 前台 turn 启动许可：门开着则占用一个 turn 槽位并返回 true。
    pub(crate) fn try_begin_turn(&self) -> bool {
        let _door = self.door.lock().expect("gate door 锁未被毒化");
        if self.held.load(Ordering::SeqCst) {
            return false;
        }
        self.turns.fetch_add(1, Ordering::SeqCst);
        self.bump();
        true
    }

    /// 后台 review 启动许可：门开着则占用一个 review 槽位并返回 true。
    pub(crate) fn try_begin_review(&self) -> bool {
        let _door = self.door.lock().expect("gate door 锁未被毒化");
        if self.held.load(Ordering::SeqCst) {
            return false;
        }
        self.reviews.fetch_add(1, Ordering::SeqCst);
        self.bump();
        true
    }

    /// turn 结束释放槽位（任何结束路径都必须调用）。
    pub(crate) fn end_turn(&self) {
        self.turns.fetch_sub(1, Ordering::SeqCst);
        self.bump();
    }

    /// review 结束释放槽位（经 ReviewSlotGuard 的 Drop，含 panic/早退）。
    pub(crate) fn end_review(&self) {
        self.reviews.fetch_sub(1, Ordering::SeqCst);
        self.bump();
    }

    /// 生命周期操作尝试关门：返回持门守卫（期间新 turn / review 无法启动）。
    pub(crate) fn try_acquire(&self) -> Option<QuiescentGuard<'_>> {
        let _door = self.door.lock().expect("gate door 锁未被毒化");
        if self.held.load(Ordering::SeqCst) {
            return None;
        }
        if self.turns.load(Ordering::SeqCst) + self.reviews.load(Ordering::SeqCst) > 0 {
            return None;
        }
        self.held.store(true, Ordering::SeqCst);
        Some(QuiescentGuard { gate: self })
    }

    /// 当前运行槽位快照：(turn 数, review 数)。
    pub(crate) fn running(&self) -> (usize, usize) {
        (
            self.turns.load(Ordering::SeqCst),
            self.reviews.load(Ordering::SeqCst),
        )
    }

    /// 是否有任何运行中的工作。
    #[cfg(test)]
    pub(crate) fn is_quiescent(&self) -> bool {
        self.running() == (0, 0)
    }

    /// 状态版本 +1，唤醒所有等待者。
    fn bump(&self) {
        self.version
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// 等待直到静默（turn 与 review 槽位全部释放）。
    pub(crate) async fn wait_quiescent(&self) {
        let mut rx = self.version.subscribe();
        loop {
            let seen = *rx.borrow();
            let (turns, reviews) = self.running();
            if turns == 0 && reviews == 0 {
                return;
            }
            // 值已在订阅后变化则立即返回，否则等待下一次变化——无丢失窗口。
            let _ = rx.wait_for(|version| *version != seen).await;
        }
    }

    /// 等待直到前台 turn 槽位全部释放（review 不阻塞下一轮 turn 的启动）。
    pub(crate) async fn wait_turns_idle(&self) {
        let mut rx = self.version.subscribe();
        loop {
            let seen = *rx.borrow();
            if self.turns.load(Ordering::SeqCst) == 0 {
                return;
            }
            let _ = rx.wait_for(|version| *version != seen).await;
        }
    }
}

/// 后台 review 槽位守卫：Drop 时把 review 槽位 -1。
pub(crate) struct ReviewSlotGuard(pub(crate) Arc<QuiescenceGate>);
impl Drop for ReviewSlotGuard {
    fn drop(&mut self) {
        self.0.end_review();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_op_waits_for_running_slots() {
        let gate = QuiescenceGate::new();
        assert!(gate.try_begin_turn());
        // 有 turn 在跑：关门失败。
        assert!(gate.try_acquire().is_none());
        gate.end_turn();
        // 静默后：关门成功，持门期间新 turn 无法启动。
        let guard = gate.try_acquire().unwrap();
        assert!(!gate.try_begin_turn());
        drop(guard);
        assert!(gate.try_begin_turn());
        gate.end_turn();
    }

    #[tokio::test]
    async fn wait_quiescent_wakes_on_release() {
        let gate = Arc::new(QuiescenceGate::new());
        assert!(gate.try_begin_turn());
        let waiter_gate = gate.clone();
        let waiter = tokio::spawn(async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                waiter_gate.wait_quiescent(),
            )
            .await
        });
        tokio::task::yield_now().await;
        gate.end_turn();
        assert!(waiter.await.unwrap().is_ok(), "释放后等待者必须被唤醒");
    }

    #[tokio::test]
    async fn wait_turns_idle_wakes_on_release() {
        let gate = Arc::new(QuiescenceGate::new());
        assert!(gate.try_begin_turn());
        let waiter_gate = gate.clone();
        let waiter = tokio::spawn(async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                waiter_gate.wait_turns_idle(),
            )
            .await
        });
        tokio::task::yield_now().await;
        gate.end_turn();
        assert!(waiter.await.unwrap().is_ok(), "释放后等待者必须被唤醒");
    }

    #[tokio::test]
    async fn review_slot_guard_releases_on_drop() {
        let gate = Arc::new(QuiescenceGate::new());
        assert!(gate.try_begin_review());
        assert!(!gate.is_quiescent());
        {
            let _guard = ReviewSlotGuard(gate.clone());
        }
        assert!(gate.is_quiescent(), "guard drop 后 review 槽位必须归零");
    }
}
