//! 桌面 / 特权 UI 的工作流管理接口（模型工具面不存在这些入口）。
//!
//! 所有函数走同一服务端鉴权：项目根、`ToolAccess` 快照、`exec_generation`；调用方是
//! 桌面命令层，权限恒为 `Unrestricted`。

use serde_json::Value;

use rclaw_core::ToolAccess;

fn service() -> Result<super::WorkflowService, String> {
    super::workflow_service::global()
        .ok_or_else(|| "工作流服务尚未初始化（agent 未启动）".to_string())
}

fn error_to_string(error: super::workflow_service::WorkflowError) -> String {
    error.to_json().to_string()
}

/// 运行列表（桌面卡片 / workflows hub）。
pub fn status() -> Result<Value, String> {
    let service = service()?;
    service
        .list(
            50,
            Vec::new(),
            true,
            "desktop",
            "console",
            ToolAccess::Unrestricted,
        )
        .map_err(error_to_string)
}

/// 单 run 详情（卡片展开 / 产物区）。
pub fn detail(run_id: &str, include_effects: bool) -> Result<Value, String> {
    let service = service()?;
    service
        .get(
            run_id,
            super::workflow_service::GetOptions {
                include_legacy: true,
                include_effects,
                ..Default::default()
            },
            "desktop",
            "console",
            ToolAccess::Unrestricted,
        )
        .map_err(error_to_string)
}

/// 桌面取消（`user` 来源）。
pub async fn cancel(run_id: &str) -> Result<Value, String> {
    let service = service()?;
    service
        .cancel(
            run_id,
            "user",
            "desktop".to_string(),
            "console".to_string(),
            ToolAccess::Unrestricted,
        )
        .await
        .map_err(error_to_string)
}

/// 未知 effect 逐项裁决。
pub fn confirm_effects(
    run_id: &str,
    exec_generation: u32,
    decisions: &[Value],
) -> Result<Value, String> {
    let service = service()?;
    service
        .confirm_effects(run_id, exec_generation, decisions, "desktop")
        .map_err(error_to_string)
}

/// 进程树人工确认。
pub fn confirm_process_gone(
    run_id: &str,
    exec_generation: u32,
    process_ids: &[String],
) -> Result<Value, String> {
    let service = service()?;
    service
        .confirm_process_gone(run_id, exec_generation, process_ids)
        .map_err(error_to_string)
}

/// dead 通知重投。
pub fn resend_notification(notification_id: &str) -> Result<Value, String> {
    let service = service()?;
    service
        .resend_notification(notification_id)
        .map_err(error_to_string)
}

/// 确认 amend 可重建（前驱有写 effect 时）。
pub fn confirm_amend(run_id: &str) -> Result<Value, String> {
    let service = service()?;
    service.confirm_amend(run_id).map_err(error_to_string)
}

/// 填充等待中的 hole（桌面弹窗 / 人工补全）。
pub async fn fill_hole(
    run_id: &str,
    hole_id: &str,
    script_generation: u32,
    body: &str,
) -> Result<Value, String> {
    let service = service()?;
    service
        .fill_hole(
            run_id,
            hole_id,
            script_generation,
            body.to_string(),
            "desktop".to_string(),
            "console".to_string(),
            ToolAccess::Unrestricted,
            false,
        )
        .await
        .map_err(error_to_string)
}

/// 桌面续跑（用户来源）：`stopped(user)` 的 run 要求新鲜授权，UI 点击本身就是用户的明确
/// 请求——这里代签一次性 auth_token 再调 resume；模型工具面拿不到这个入口。
pub async fn resume(run_id: &str) -> Result<Value, String> {
    let service = service()?;
    let auth_token = service
        .issue_auth_token_for_ui(run_id)
        .map_err(error_to_string)?;
    service
        .resume(
            run_id,
            None,
            auth_token,
            "desktop".to_string(),
            "console".to_string(),
            ToolAccess::Unrestricted,
        )
        .await
        .map_err(error_to_string)
}

/// 回答运行中 actor 的问题（桌面弹窗）。
pub fn resolve_question(question_id: &str, answer: &str) -> Result<Value, String> {
    let service = service()?;
    service
        .resolve_question(
            question_id,
            answer,
            "desktop",
            "console",
            ToolAccess::Unrestricted,
        )
        .map_err(error_to_string)
}
