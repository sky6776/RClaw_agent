//! Per-turn tool state isolated with Tokio task-local storage.

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};

use rclaw_store::memory::MemoryStore;
use tokio_util::sync::CancellationToken;

tokio::task_local! {
    static MEMORY_FAILURES: AtomicU32;
    static PERSONA_MEMORY: Option<MemoryStore>;
    /// 当前顶层 turn 的取消令牌（ZCode 移植 · Layer 2a）：前台子代理自动转后台时
    /// 用于「父取消 → 子取消」的联动（detach 后解除）。子代理 scope 内重置为 None。
    static TURN_CANCEL: Option<CancellationToken>;
}

/// Run one top-level or sub-agent turn with isolated tool state.
///
/// 无 persona 记忆（主 persona / 子代理）：`PERSONA_MEMORY` 以 `None` 进入作用域，
/// memory 工具回落到自己的默认 store。
pub fn scope<F>(fut: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    scope_inner(None, None, fut)
}

/// 顶层 turn 作用域：persona 记忆（Bot Mode：bot 会话的 memory 工具写入 bot 自己的
/// MemoryStore，而非主 persona 的）+ 当前 turn 取消令牌（Layer 2a detachParent 联动）。
pub fn scope_top_level<F>(
    memory: Option<MemoryStore>,
    cancel: Option<CancellationToken>,
    fut: F,
) -> impl Future<Output = F::Output>
where
    F: Future,
{
    scope_inner(memory, cancel, fut)
}

fn scope_inner<F>(
    persona: Option<MemoryStore>,
    cancel: Option<CancellationToken>,
    fut: F,
) -> impl Future<Output = F::Output>
where
    F: Future,
{
    PERSONA_MEMORY.scope(
        persona,
        MEMORY_FAILURES.scope(AtomicU32::new(0), TURN_CANCEL.scope(cancel, fut)),
    )
}

/// Increment and return the current turn's memory-consolidation failure count.
///
/// Returns `None` outside a scoped turn so standalone tool users can retain
/// their existing instance-local fallback.
pub(crate) fn record_memory_failure() -> Option<u32> {
    MEMORY_FAILURES
        .try_with(|failures| failures.fetch_add(1, Ordering::SeqCst) + 1)
        .ok()
}

/// 当前 turn 的 persona 记忆（Bot Mode bot 会话）；主 persona / 子代理 / 作用域外
/// 返回 None，调用方回落默认 store。
pub fn persona_memory() -> Option<MemoryStore> {
    PERSONA_MEMORY.try_with(Clone::clone).ok().flatten()
}

/// 当前顶层 turn 的取消令牌（仅顶层 turn 作用域内非 None；子代理 scope 重置为 None）。
/// 前台子代理自动转后台（Layer 2a）用它建立「父取消 → 子取消」联动。
pub fn turn_cancel() -> Option<CancellationToken> {
    TURN_CANCEL.try_with(Clone::clone).ok().flatten()
}
