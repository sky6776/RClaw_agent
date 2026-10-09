//! Per-turn 工具文件沙箱权限（tokio task-local）。
//!
//! 每个 turn 在独立 tokio task 里跑（`run_turn` → `tokio::spawn(run_turn_inner)`），
//! 用 task-local 携带当前消息的 [`ToolAccess`]：max QQ 的 turn 设 `Unrestricted`，
//! 其它设 `Sandboxed`。task-local 天然 per-task 隔离——避免不同 QQ 的 turn 并发时
//! 「受限用户读到 max 用户标志」的串档越权。
//!
//! 读不到时（后台 review fork、subagent、任何外部 `tokio::spawn`）默认回退
//! [`ToolAccess::Sandboxed`]——**fail-safe**：丢失上下文绝不越权。
//!
//! 存储类型用 [`ToolAccess`] 本身（而非 `Cell<ToolAccess>`）：多线程 runtime 下 task 可能
//! 跨线程迁移，task-local 值需 `Send`，`Cell` 非 `Send`；`ToolAccess` 是 `Copy` 枚举且
//! 只读访问，直接存值即可。

use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock};

use rclaw_core::ToolAccess;

use crate::ToolResult;

tokio::task_local! {
    /// 当前 turn 的工具沙箱权限。
    static TURN_ACCESS: ToolAccess
}

/// 读取当前 task 的工具沙箱权限；读不到（不在 turn task 内）回退 [`ToolAccess::Sandboxed`]。
pub fn current_access() -> ToolAccess {
    TURN_ACCESS.try_with(|a| *a).unwrap_or_default()
}

/// 在给定权限下执行 future（供 turn spawn 点包裹 `run_turn_inner`）。
pub fn scope_access<F>(access: ToolAccess, fut: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    TURN_ACCESS.scope(access, fut)
}

// ---------------------------------------------------------------------------
// 工作区外路径的交互式授权（桌面版弹窗确认）。
//
// 默认权限（Sandboxed）下文件/shell 工具只许操作 agent 工作区；宿主（桌面壳）
// 经 [`set_path_confirm_hook`] 注册确认钩子后，工作区之外的路径会先向前端弹窗
// 询问用户——允许才放行，拒绝/超时/无钩子一律按拒绝处理（fail-safe，保持 CLI
// 等无界面环境的既有行为不变）。每次授权以「精确路径」缓存（进程生命周期内），
// 同一路径的翻页读等重复访问不再反复弹窗。
// ---------------------------------------------------------------------------

/// 工作区外路径的授权询问结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutsideDecision {
    /// 用户允许（或命中既往授权缓存）。
    Allowed,
    /// 用户明确拒绝。
    UserDenied,
    /// 询问超时无人应答。
    Timeout,
    /// 宿主未注册确认钩子（CLI 等无界面环境）：直接拒绝。
    NoHook,
}

/// 钩子返回的 boxed future（避免 rclaw-tools 引入 futures 依赖）。
pub type ConfirmFuture = Pin<Box<dyn Future<Output = OutsideDecision> + Send>>;
/// 确认钩子：入参为路径的人类可读展示形式（正斜杠），返回用户决定。
pub type PathConfirmHook = Arc<dyn Fn(String) -> ConfirmFuture + Send + Sync>;

static PATH_CONFIRM_HOOK: OnceLock<RwLock<Option<PathConfirmHook>>> = OnceLock::new();
static GRANTED_PATHS: OnceLock<StdMutex<HashSet<String>>> = OnceLock::new();

/// 注册工作区外路径确认钩子（进程级，只需且只能注册一次；重复注册返回 false）。
pub fn set_path_confirm_hook(hook: PathConfirmHook) -> bool {
    let mut guard = PATH_CONFIRM_HOOK
        .get_or_init(|| RwLock::new(None))
        .write()
        .expect("PATH_CONFIRM_HOOK 锁未被毒化");
    if guard.is_some() {
        return false;
    }
    *guard = Some(hook);
    true
}

/// 授权缓存的键：正斜杠归一；Windows 另做小写归一（大小写不敏感文件系统），
/// 其余平台保留原拼写——大小写敏感，不同拼写是不同路径，不得互认授权。
#[cfg(windows)]
fn granted_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

#[cfg(not(windows))]
fn granted_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// 沙箱 gate：`Unrestricted` 直放行；工作区内路径直放行；其余经钩子询问用户
/// （命中授权缓存则免询问）。`deny` 为调用方的既有拒绝文案（无钩子时原样返回，
/// 保持历史行为与测试断言不变），用户拒绝/超时在其后追加原因。
pub async fn ensure_path_allowed(
    base: &Path,
    resolved: &Path,
    deny: &str,
) -> Result<(), ToolResult> {
    if current_access() == ToolAccess::Unrestricted {
        return Ok(());
    }
    if rclaw_store::validate_agent_path(base, &resolved.to_string_lossy()) {
        return Ok(());
    }
    let key = granted_key(resolved);
    if GRANTED_PATHS
        .get_or_init(|| StdMutex::new(HashSet::new()))
        .lock()
        .expect("GRANTED_PATHS 锁未被毒化")
        .contains(&key)
    {
        return Ok(());
    }
    let display = resolved.to_string_lossy().replace('\\', "/");
    // 先克隆 Arc 并释放锁，再 await——std 锁的 guard 不能跨 await（future 须 Send）。
    let hook = PATH_CONFIRM_HOOK
        .get_or_init(|| RwLock::new(None))
        .read()
        .expect("PATH_CONFIRM_HOOK 锁未被毒化")
        .as_ref()
        .cloned();
    let decision = match hook {
        Some(hook) => hook(display).await,
        None => OutsideDecision::NoHook,
    };
    match decision {
        OutsideDecision::Allowed => {
            GRANTED_PATHS
                .get_or_init(|| StdMutex::new(HashSet::new()))
                .lock()
                .expect("GRANTED_PATHS 锁未被毒化")
                .insert(key);
            Ok(())
        }
        OutsideDecision::UserDenied => Err(crate::ToolResult::err(format!(
            "{deny}（用户拒绝授权访问工作区之外的路径）"
        ))),
        OutsideDecision::Timeout => Err(crate::ToolResult::err(format!(
            "{deny}（授权请求超时未响应）"
        ))),
        OutsideDecision::NoHook => Err(crate::ToolResult::err(deny.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scope_sets_access_within() {
        // scope 外默认受限。
        assert_eq!(current_access(), ToolAccess::Sandboxed);
        let inner = scope_access(ToolAccess::Unrestricted, async { current_access() }).await;
        assert_eq!(inner, ToolAccess::Unrestricted);
        // scope 结束后回到默认。
        assert_eq!(current_access(), ToolAccess::Sandboxed);
    }

    #[tokio::test]
    async fn spawned_task_does_not_inherit() {
        // spawn 的子任务不继承 task-local → fail-safe 回退 Sandboxed。
        // 这保证 subagent / 后台 review 等独立 task 绝不越权继承 max。
        scope_access(ToolAccess::Unrestricted, async {
            let child = tokio::spawn(async { current_access() }).await.unwrap();
            assert_eq!(child, ToolAccess::Sandboxed);
        })
        .await;
    }
}
