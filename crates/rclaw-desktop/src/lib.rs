//! RClaw 桌面版壳（Tauri 2）——Windows 桌面与 Android APK（apk-host）共用。
//!
//! 启动流程：解析用户数据目录 → manage state → setup：
//! - `rclaw.toml` 已存在 → 后台装配并启动 agent loop；
//! - 不存在 → 前端经 `get_status` 检测到向导模式，提交后由 `wizard_save` 启动。
//!
//! 桌面窗口语义：点关闭 = 隐藏到托盘（无人值守常驻），托盘菜单「退出」才真正结束；
//! Android APK：无托盘（`not(android)` 门），关闭即退后台由前台服务接管。
//!
//! Termux（android target 且未开 apk-host feature）：模块全部不编译，
//! main.rs 保留提示入口（D7 零回归）。

// 桌面与 APK 宿主共用模块面；Termux（android 且无 feature）整体不编译。
#[cfg(any(not(target_os = "android"), feature = "apk-host"))]
mod agent;
#[cfg(any(not(target_os = "android"), feature = "apk-host"))]
mod android_status;
#[cfg(any(not(target_os = "android"), feature = "apk-host"))]
mod commands;
#[cfg(any(not(target_os = "android"), feature = "apk-host"))]
mod export;
#[cfg(any(not(target_os = "android"), feature = "apk-host"))]
mod groups;
#[cfg(any(not(target_os = "android"), feature = "apk-host"))]
mod home;
#[cfg(any(not(target_os = "android"), feature = "apk-host"))]
mod insights;
#[cfg(any(not(target_os = "android"), feature = "apk-host"))]
mod keep_alive;
#[cfg(any(not(target_os = "android"), feature = "apk-host"))]
mod media_stream;
#[cfg(any(not(target_os = "android"), feature = "apk-host"))]
mod permissions;
#[cfg(any(not(target_os = "android"), feature = "apk-host"))]
mod state;
#[cfg(any(not(target_os = "android"), feature = "apk-host"))]
mod tool_events;
#[cfg(any(not(target_os = "android"), feature = "apk-host"))]
mod wizard;

// APK 宿主面：Java_ai_rclaw_app_NativeBridge_* JNI 导出（单 .so）。
#[cfg(all(target_os = "android", feature = "apk-host"))]
mod android_host;

// 托盘仅桌面（手机无托盘；tauri android 表不带 tray-icon feature）。
#[cfg(not(target_os = "android"))]
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager,
};

#[cfg(any(not(target_os = "android"), feature = "apk-host"))]
use crate::state::AppState;

/// Android 启动取证日志路径（filesDir 相对由调用方传入 cwd 不可靠，直接用
/// 宿主上下文的 files_dir）。
/// 启动取证日志：Android 写 filesDir/boot_debug.log；其他平台 no-op。
#[cfg(all(target_os = "android", feature = "apk-host"))]
pub fn android_boot_log(step: &str) {
    rclaw_android::log_boot(step);
}

#[cfg(not(all(target_os = "android", feature = "apk-host")))]
pub fn android_boot_log(_step: &str) {}

/// 桌面 / APK 共用入口：Tauri Builder 装配（command 面 + agent 启动 + 窗口语义）。
/// Android 上由 tauri 的 mobile entry point（gen/android MainActivity）触发。
#[cfg_attr(
    all(target_os = "android", feature = "apk-host"),
    tauri::mobile_entry_point
)]
#[cfg(any(not(target_os = "android"), feature = "apk-host"))]
pub fn run() {
    // 真机取证：vivo 裁剪 logcat，panic/abort 无从可读；把启动阶段写进应用私有
    // 文件（run-as 可读）。panics 跨 JNI = abort（tauri 宏 stop_unwind），必须先落盘。
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        std::panic::set_hook(Box::new(|info| {
            let msg = format!(
                "PANIC at {}: {}\n",
                std::thread::current().name().unwrap_or("?"),
                info
            );
            let path = rclaw_android::host_context()
                .map(|c| c.files_dir.join("boot_debug.log"))
                .unwrap_or_default();
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                use std::io::Write;
                let _ = f.write_all(msg.as_bytes());
            }
            eprintln!("RClaw PANIC: {info}");
        }));
    }
    let home = home::resolve_home();
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        // 旧数据迁移是 Windows 语义；APK 无需（也避免动 filesDir 之外的路径）。
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    home::migrate_legacy_appdata(&home);
    std::fs::create_dir_all(&home).expect("创建用户数据目录失败");

    tauri::Builder::default()
        .manage(AppState::new(home.clone()))
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::update_check,
            commands::update_download_install,
            commands::open_url,
            commands::browser_open_url,
            commands::media_base64,
            commands::media_preview,
            commands::media_stream_url,
            commands::open_media,
            commands::browser_toggle_visible,
            commands::save_media,
            commands::share_media,
            commands::share_text,
            commands::export_messages,
            commands::app_versions,
            commands::workspace_list,
            commands::workspace_read,
            commands::workspace_write,
            commands::workspace_copy_file,
            commands::workspace_delete,
            commands::workspace_rename,
            commands::workspace_new_folder,
            commands::workspace_paste_from_clipboard,
            commands::workspace_copy_to_clipboard,
            commands::consume_pending_share,
            commands::skill_approvals_pending,
            commands::skill_approvals_approve,
            commands::skill_approvals_revoke,
            commands::skill_config_list,
            commands::skill_config_read,
            commands::skill_config_write,
            commands::copy_home_path,
            commands::open_home_dir,
            commands::app_exit,
            keep_alive::keep_alive_status,
            keep_alive::keep_alive_request_battery_whitelist,
            keep_alive::keep_alive_open_oem_settings,
            commands::wizard_save,
            commands::send_message,
            commands::cancel,
            commands::chat_list,
            commands::chat_history,
            commands::chat_fork,
            commands::chat_history_since,
            commands::chat_tool_detail,
            commands::plan_status,
            commands::plan_pause,
            commands::workflow_status,
            commands::workflow_answer_command_confirm,
            commands::workflow_detail,
            commands::workflow_cancel,
            commands::workflow_resume,
            commands::workflow_confirm_effects,
            commands::workflow_confirm_process_gone,
            commands::workflow_resend_notification,
            commands::workflow_confirm_amend,
            commands::workflow_resolve_question,
            commands::workflow_fill_hole,
            commands::chat_archive,
            commands::chat_restore,
            commands::chat_delete,
            commands::chat_rename,
            commands::chat_export,
            commands::agents_list,
            commands::agents_clear_history,
            commands::usage_snapshot,
            commands::tools_list,
            commands::set_tool_enabled,
            commands::skills_list,
            commands::skill_set_enabled,
            commands::evolution_info,
            commands::insights,
            commands::config_state,
            commands::config_save,
            commands::config_import_raw,
            commands::slogans_get,
            commands::slogans_set,
            commands::set_window_title,
            commands::window_minimize,
            commands::window_toggle_maximize,
            commands::window_maximized,
            commands::window_close,
            commands::window_start_drag,
            commands::theme_get,
            commands::theme_set,
            commands::permission_mode_get,
            commands::permission_mode_set,
            commands::permission_respond,
            commands::upload_file,
            commands::cron_list,
            commands::cron_remove,
            commands::bots_list,
            commands::bot_providers,
            commands::bot_save,
            commands::bot_delete,
            commands::groups_list,
            commands::group_create,
            commands::group_delete,
            commands::group_history,
            commands::group_send,
            commands::group_cancel,
            commands::capture_start,
            commands::consume_capture_result,
            commands::record_start,
            commands::record_stop,
            commands::record_active,
            commands::record_cancel,
            commands::save_to_gallery,
            commands::discard_capture,
            commands::save_edited_photo,
            commands::whisper_model_status,
            commands::whisper_model_download,
            commands::whisper_transcribe,
        ])
        .setup(move |app| {
            // 托盘仅桌面（手机无托盘）。
            #[cfg(not(target_os = "android"))]
            {
                setup_tray(app)?;
            }
            if home::has_config(&home) {
                // 配置已存在：后台装配并启动 agent（向导模式则等 wizard_save 触发）。
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = agent::start_agent(handle, home).await {
                        tracing::error!("agent 启动失败: {e}");
                    }
                });
            } else {
                tracing::info!("检测到首次运行，等待配置向导完成");
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭窗口 = 隐藏到托盘，保持无人值守常驻。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("RClaw 桌面版运行失败");
}

/// 托盘：显示窗口 / 退出。
#[cfg(not(target_os = "android"))]
fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示 RClaw", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    let mut tray = TrayIconBuilder::with_id("rclaw-tray")
        .tooltip("RClaw")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| handle_tray_event(app, event.id.as_ref()));
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

/// 托盘菜单事件：显示窗口 / 退出进程。
#[cfg(not(target_os = "android"))]
fn handle_tray_event(app: &tauri::AppHandle, id: &str) {
    match id {
        "show" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }
        // 与 app_exit 命令同一清理：无头 Chromium 不随父进程退出，先回收。
        "quit" => {
            rclaw_tools::cdp_browser::kill_headless_processes();
            app.exit(0)
        }
        _ => {}
    }
}
