//! 多端同步在桌面壳 / APK 这一侧的宿主（`docs/multi-device-sync.md` §12.1、§13）。
//!
//! - 引擎的事件（状态变了、会话被同步改了、附件取回了、某类数据要重新加载）转成 Webview
//!   的 `sync` 事件。界面可能晚于引擎出现（APK 的前台服务先装配、界面后开），所以界面的
//!   句柄是后绑定的：没绑定时事件直接丢掉——界面打开时本来就会整个重新取数。
//! - 网络是否按流量计费：桌面恒为否，APK 问系统。只用于两处（后台轮询只在非计费网络下
//!   进行、计费网络下的首次同步要用户确认）；附件的上传下载不看它。
//! - APK 的前后台切换通知引擎：后台放宽轮询，回到前台立即同步。
//! - 启动时把旧版本存在 `desktop_prefs.json` 里的会话标题搬进库（标题跟着会话同步）。
//!
//! 设置面板、会话包、扫码等命令在 `commands::sync` 里。

use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use rclaw_cli::sync::{SyncProfile, SyncSeed};
use rclaw_runtime::AgentLoop;
use rclaw_store::session::SessionStore;
use rclaw_store::sync_db;
use rclaw_sync::{Reload, SyncEngine, SyncEvent, SyncHost};
use tauri::Emitter;

use crate::agent::{is_valid_chat_id, CHANNEL_DESKTOP};

/// Webview 事件名。载荷是 [`SyncEvent`]（`kind` = `status` / `sessions` / `attachment`），
/// 或 `{kind: "reload", what}`。界面把它当「该重新取数了」的提示，内容以重新查询为准。
pub const EVENT_NAME: &str = "sync";

/// 标题迁移做过的标记（`kv`）。
const KV_TITLES_MIGRATED: &str = "desktop/chat-titles-migrated";

fn app_slot() -> &'static RwLock<Option<tauri::AppHandle>> {
    static SLOT: OnceLock<RwLock<Option<tauri::AppHandle>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// 界面起来了：此后引擎的事件推给它。幂等。
pub fn bind_app(app: &tauri::AppHandle) {
    *app_slot()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(app.clone());
}

fn emit<T: serde::Serialize + Clone>(payload: T) {
    // 句柄先克隆出来再发：发事件的时候不拿着锁。
    let app = app_slot()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(app) = app {
        if let Err(error) = app.emit(EVENT_NAME, payload) {
            tracing::debug!("sync 事件推送失败: {error}");
        }
    }
}

/// 让界面刷新同步状态（命令改了引擎看不到的东西时用，比如回收站里删了一项）。
pub fn emit_status() {
    emit(SyncEvent::Status);
}

struct DesktopHost;

impl SyncHost for DesktopHost {
    fn is_metered(&self) -> bool {
        #[cfg(target_os = "android")]
        {
            rclaw_android::network_metered()
        }
        #[cfg(not(target_os = "android"))]
        {
            false
        }
    }

    fn emit(&self, event: SyncEvent) {
        emit(event);
    }

    fn reload(&self, what: Reload) {
        emit(serde_json::json!({ "kind": "reload", "what": what }));
    }
}

/// APK：引擎句柄另存一份，前后台切换的 JNI 回调（不经 Tauri 状态）用它。
#[cfg(target_os = "android")]
fn engine_slot() -> &'static RwLock<Option<SyncEngine>> {
    static SLOT: OnceLock<RwLock<Option<SyncEngine>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// 把同步引擎接到 `agent` 上（必须在 `agent.run()` 之前调用），返回句柄。桌面与 APK
/// 都有会话列表，会话参与同步。
pub fn attach(seed: SyncSeed, agent: &AgentLoop) -> SyncEngine {
    let engine = seed.attach(
        agent,
        SyncProfile {
            sessions: true,
            host: Arc::new(DesktopHost),
        },
    );
    #[cfg(target_os = "android")]
    {
        *engine_slot()
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(engine.clone());
        // 前台服务可以在界面没开的时候装配（开机自启、被系统拉起）：引擎从一开始就按
        // 后台的节奏轮询，界面回到前台时再切回来。
        if !rclaw_android::app_foreground() {
            engine.notify(rclaw_sync::engine::Trigger::Background);
        }
    }
    engine
}

/// APK：界面进入 / 离开前台（`MainActivity` 的 onResume / onPause 经 JNI 调进来）。引擎
/// 还没装配、或者同步没开时什么都不做；不阻塞。
#[cfg(target_os = "android")]
pub fn set_foreground(foreground: bool) {
    let engine = engine_slot()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(engine) = engine {
        engine.notify(if foreground {
            rclaw_sync::engine::Trigger::Foreground
        } else {
            rclaw_sync::engine::Trigger::Background
        });
    }
}

/// 旧版本把会话的自定义标题存在 `desktop_prefs.json` 的 `chat_titles` 里；标题现在存在库里
/// （`kv`），才能跟着会话同步。启动时搬一次：只搬库里还有的会话、库里还没有标题的那些，
/// 偏好文件不改写。搬过之后在库里留一个标记，此后不再看偏好文件——否则用户后来清掉的
/// 标题会在下次启动时被旧文件里的值填回去。
pub fn migrate_chat_titles(home: &Path, sessions: &SessionStore) {
    if let Err(error) = try_migrate_chat_titles(home, sessions) {
        tracing::warn!("会话标题没能从 desktop_prefs.json 搬进库（下次启动再试）: {error}");
    }
}

fn try_migrate_chat_titles(home: &Path, sessions: &SessionStore) -> Result<(), String> {
    let done = sessions
        .db()
        .with(|conn| sync_db::kv_get(conn, KV_TITLES_MIGRATED))
        .map_err(|error| error.to_string())?
        .is_some();
    if done {
        return Ok(());
    }
    let legacy = crate::agent::read_chat_titles(home);
    if !legacy.is_empty() {
        let existing = sessions
            .session_titles(CHANNEL_DESKTOP)
            .map_err(|error| error.to_string())?;
        let known: std::collections::HashSet<String> = sessions
            .list_sessions(Some(CHANNEL_DESKTOP))
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|summary| summary.chat_id)
            .collect();
        let mut moved = 0usize;
        for (chat_id, title) in &legacy {
            let title = title.trim();
            if title.is_empty()
                || !is_valid_chat_id(chat_id)
                || !known.contains(chat_id)
                || existing.contains_key(chat_id)
            {
                continue;
            }
            sessions
                .set_session_title(CHANNEL_DESKTOP, chat_id, title)
                .map_err(|error| error.to_string())?;
            moved += 1;
        }
        if moved > 0 {
            tracing::info!("已把 {moved} 个会话标题从 desktop_prefs.json 搬进库");
        }
    }
    sessions
        .db()
        .with(|conn| sync_db::kv_put(conn, KV_TITLES_MIGRATED, "1"))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rclaw_core::{Message, Role};

    fn store(dir: &Path) -> SessionStore {
        SessionStore::new(rclaw_store::fs::Layout::new(dir.join("agent")))
    }

    #[test]
    fn legacy_titles_move_into_the_database_once_and_the_prefs_file_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = store(dir.path());
        for chat_id in ["main", "c-1", "c-2"] {
            sessions
                .append(
                    CHANNEL_DESKTOP,
                    chat_id,
                    &Message::plain(Role::User, "你好"),
                )
                .unwrap();
        }
        crate::agent::write_chat_title(dir.path(), "main", "自定义标题").unwrap();
        crate::agent::write_chat_title(dir.path(), "c-1", "研究").unwrap();
        // 库里已经没有的会话：不搬。
        crate::agent::write_chat_title(dir.path(), "gone", "已删除的会话").unwrap();
        // 库里已经有标题的：以库里的为准。
        sessions
            .set_session_title(CHANNEL_DESKTOP, "c-1", "库里的名字")
            .unwrap();
        let prefs = crate::agent::prefs_path(dir.path());
        let before = std::fs::read(&prefs).unwrap();

        migrate_chat_titles(dir.path(), &sessions);
        let titles = sessions.session_titles(CHANNEL_DESKTOP).unwrap();
        assert_eq!(titles.get("main").map(String::as_str), Some("自定义标题"));
        assert_eq!(titles.get("c-1").map(String::as_str), Some("库里的名字"));
        assert!(!titles.contains_key("gone") && !titles.contains_key("c-2"));
        assert_eq!(std::fs::read(&prefs).unwrap(), before, "偏好文件不改写");

        // 重复启动结果相同；用户后来清掉的标题不会被旧文件填回去。
        sessions
            .set_session_title(CHANNEL_DESKTOP, "main", "")
            .unwrap();
        migrate_chat_titles(dir.path(), &sessions);
        let titles = sessions.session_titles(CHANNEL_DESKTOP).unwrap();
        assert!(!titles.contains_key("main"));
        assert_eq!(titles.get("c-1").map(String::as_str), Some("库里的名字"));
    }

    #[test]
    fn migration_without_a_prefs_file_only_leaves_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = store(dir.path());
        migrate_chat_titles(dir.path(), &sessions);
        assert!(sessions.session_titles(CHANNEL_DESKTOP).unwrap().is_empty());
        // 之后才出现的偏好文件（从别处拷来的）不再被读取。
        sessions
            .append(CHANNEL_DESKTOP, "main", &Message::plain(Role::User, "你好"))
            .unwrap();
        crate::agent::write_chat_title(dir.path(), "main", "晚到的标题").unwrap();
        migrate_chat_titles(dir.path(), &sessions);
        assert!(sessions.session_titles(CHANNEL_DESKTOP).unwrap().is_empty());
    }
}
