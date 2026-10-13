//! 工具权限确认管道：工作区外访问的弹窗授权。
//!
//! rclaw-tools 的文件/shell 工具在默认权限下遇到 agent 工作区之外的路径时，
//! 经 [`agent` 模块注册的钩子](crate::agent::install_permission_hook)进入本模块：
//! 向前端发 `permission_request` 事件弹窗询问用户，工具调用挂起等待；用户允许 /
//! 拒绝后前端调 `permission_respond` 命令回填结果。120 秒无应答按超时拒绝，
//! 避免 turn 永久卡死。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::Emitter;
use tokio::sync::oneshot;

use rclaw_tools::access::{OutsideDecision, PathConfirmRequest};

/// 授权询问超时（秒）：超时按拒绝处理，防止 turn 挂死在无人应答的弹窗上。
const ASK_TIMEOUT_SECS: u64 = 120;

/// `permission_request` 事件载荷（前端弹窗展示用）。
#[derive(Debug, Clone, Serialize)]
pub struct PermissionRequestDto {
    pub id: String,
    /// 弹窗正文：操作 + 真正会被打开的物理落点，请求拼写与落点不同时一并带上
    /// （`PathConfirmRequest::display`）。工作区内一个指向外部的链接按拼写展示会像是
    /// 工作区里的路径，用户看不出这是在放行外部文件。
    pub path: String,
    /// 物理落点（正斜杠）。
    pub target: String,
    /// 工具请求时的拼写（正斜杠）。
    pub requested: String,
    /// 操作名（读取 / 写入 / 删除 / 列目录 / 搜索 / 作为 shell 工作目录）。
    pub op: String,
}

/// 等待应答的请求表（id → 回填通道）。桌面进程内全局唯一。
static PENDING: LazyLock<Mutex<HashMap<String, oneshot::Sender<bool>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 发起一次授权询问：向前端发事件并等待用户决定（由 agent 启动时装进钩子调用）。
pub async fn ask(app: tauri::AppHandle, request: PathConfirmRequest) -> OutsideDecision {
    // id = 毫秒时间戳 + 4 位十六进制随机数，够前端回填精确配对。
    let id = format!(
        "{}-{:04x}",
        chrono::Local::now().timestamp_millis(),
        rand::random::<u16>(),
    );
    let (tx, rx) = oneshot::channel();
    if PENDING
        .lock()
        .expect("PENDING 锁未被毒化")
        .insert(id.clone(), tx)
        .is_some()
    {
        tracing::warn!("权限请求 id 冲突: {id}");
    }
    let emit = app.emit(
        "permission_request",
        PermissionRequestDto {
            id: id.clone(),
            path: request.display(),
            target: request.target,
            requested: request.requested,
            op: request.op.label().to_string(),
        },
    );
    if let Err(e) = emit {
        // 事件发不出去（无窗口等）：没有人能应答，直接按超时拒绝。
        tracing::warn!("permission_request 事件发送失败: {e}");
        PENDING.lock().expect("PENDING 锁未被毒化").remove(&id);
        return OutsideDecision::Timeout;
    }
    match tokio::time::timeout(Duration::from_secs(ASK_TIMEOUT_SECS), rx).await {
        Ok(Ok(true)) => OutsideDecision::Allowed,
        Ok(Ok(false)) => OutsideDecision::UserDenied,
        // 超时 / 前端异常关闭通道：按拒绝。
        _ => {
            PENDING.lock().expect("PENDING 锁未被毒化").remove(&id);
            OutsideDecision::Timeout
        }
    }
}

/// 回填一次授权结果（`permission_respond` 命令调用）。
pub fn respond(id: &str, allowed: bool) -> Result<(), String> {
    let sender = PENDING
        .lock()
        .expect("PENDING 锁未被毒化")
        .remove(id)
        .ok_or_else(|| "授权请求不存在或已过期".to_string())?;
    let _ = sender.send(allowed);
    Ok(())
}
