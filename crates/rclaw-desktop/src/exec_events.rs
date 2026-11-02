//! 子 agent 执行登记表直播 → Webview `agent_exec` 事件桥（侧边栏「Agent / 任务概览」
//! 的实时进度、详情页的实时活动记录；桌面与 APK 共用）。
//!
//! 运行时在登记表每次变更（排队 / 开跑 / 阶段切换 / token 记账 / 终态 / 清除）、工具批
//! 即将执行、transcript 新行落库时广播 [`ExecutionUpdate`]；这里逐条转成事件推给前端。
//! 直播只是加速：事实源是登记表（`agents_list` / `agent_execution_detail`）与会话库里的
//! transcript（`agent_execution_history*`）。广播队列滞后（前端卡住、事件被丢）时发一条
//! `{kind: "resync"}`，前端据此重拉全量对齐，不在这里攒无界队列。
//!
//! 每进程只接一次（`start_agent` 成功后）：APK 的 GUI 采用共享运行时时同样经 `start_agent`
//! 进来，而它对已启动的 agent 直接返回，不会重复起桥。

use tauri::Emitter;

use rclaw_runtime::{AgentToolRuntime, ExecutionUpdate};

/// Webview 事件名。
pub const EVENT_NAME: &str = "agent_exec";

/// 订阅运行时的执行直播并转发到 Webview，直到运行时被丢弃（广播端关闭）。
pub async fn run_bridge(app: tauri::AppHandle, runtime: AgentToolRuntime) {
    let mut rx = runtime.subscribe_execution_updates();
    loop {
        match rx.recv().await {
            Ok(update) => emit(&app, &update),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!("agent_exec 直播滞后，丢弃 {skipped} 条事件，通知前端重拉全量");
                emit_resync(&app);
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::debug!("agent_exec 广播端已关闭，事件桥退出");
                return;
            }
        }
    }
}

fn emit(app: &tauri::AppHandle, update: &ExecutionUpdate) {
    if let Err(e) = app.emit(EVENT_NAME, update) {
        tracing::debug!("agent_exec 事件推送失败: {e}");
    }
}

fn emit_resync(app: &tauri::AppHandle) {
    if let Err(e) = app.emit(EVENT_NAME, serde_json::json!({ "kind": "resync" })) {
        tracing::debug!("agent_exec resync 事件推送失败: {e}");
    }
}
