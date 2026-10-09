//! Per-turn tool state isolated with Tokio task-local storage.

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};

use rclaw_store::memory::MemoryStore;

tokio::task_local! {
    static MEMORY_FAILURES: AtomicU32;
    static PERSONA_MEMORY: Option<MemoryStore>;
}

/// Run one top-level or sub-agent turn with isolated tool state.
///
/// 无 persona 记忆（主 persona / 子代理）：`PERSONA_MEMORY` 以 `None` 进入作用域，
/// memory 工具回落到自己的默认 store。
pub fn scope<F>(fut: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    scope_inner(None, fut)
}

/// 带 bot persona 记忆运行一个 turn（Bot Mode：bot 会话的 memory 工具写入
/// bot 自己的 MemoryStore，而非主 persona 的）。
pub fn scope_with_persona<F>(memory: Option<MemoryStore>, fut: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    scope_inner(memory, fut)
}

fn scope_inner<F>(persona: Option<MemoryStore>, fut: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    PERSONA_MEMORY.scope(persona, MEMORY_FAILURES.scope(AtomicU32::new(0), fut))
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
