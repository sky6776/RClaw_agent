//! 同步状态与待处理事项：界面的「多端同步」面板和 `/sync status` 展示的内容。
//!
//! 待处理事项有两个来源。要用户做决定、或只发生一次的（等确认的人格改动、被拦下的记忆
//! 条目、到期丢弃的提示）各存一个小文件在 `<数据根>/sync/pending/`；能从库里的状态直接
//! 看出来的（被隔离的线程、读不下去的设备日志）每次现算，状态消失它们也跟着消失。

use std::path::PathBuf;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::attach::now_ms;
use crate::config::SyncPaths;
use crate::crypto::sha256_hex;
use crate::db;
use crate::error::{Result, SyncError};
use crate::map;
use crate::merge;

/// 读取失败持续这么久才列进待处理事项（存储偶尔抖动、对象还没到齐都会自己恢复）。
const PEER_ERROR_GRACE_MS: i64 = 3 * 60 * 1000;

/// 一条待处理事项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingItem {
    pub id: String,
    /// 类别：`persona`、`skill`、`bot`（等本机确认的远端改动）；`quarantine`（被隔离的
    /// 线程）；`upgrade`（要升级本机才能继续读的设备日志）；`peer-error`；`pub-halt`；
    /// `mem-hold`（暂停同步的记忆文件）；`mem-wipe`（记忆文件里原有的条目都不见了，问
    /// 用户是不是有意删的：`accept` = 是，把删除同步出去；`reject` = 不是，恢复到本机）；
    /// 其余是只读的提示。
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    /// 来自哪台设备（展示名）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default)]
    pub at_ms: i64,
    /// 界面上可以做的操作：`accept` / `reject` / `retry` / `discard` / `dismiss`。
    #[serde(default)]
    pub actions: Vec<String>,
    /// 类别自己的数据。
    #[serde(default)]
    pub payload: serde_json::Value,
}

fn is_item_id(id: &str) -> bool {
    (1..=96).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn item_path(paths: &SyncPaths, id: &str) -> Result<PathBuf> {
    if !is_item_id(id) {
        return Err(SyncError::invalid("待处理事项的标识不合规"));
    }
    Ok(paths.pending.join(format!("{id}.json")))
}

/// 由类别和一个任意的键得到事项标识（同一个键总是同一个标识，重复提示不会堆积）。
pub fn item_id(kind: &str, key: &str) -> String {
    format!("{kind}-{}", &sha256_hex(key.as_bytes())[..16])
}

impl PendingItem {
    /// 等确认的改动是哪一版（人格、技能、bot 的事项才有）。事项的标识由路径 / 名字算出，
    /// 不随版本变：同一个标识下，内容可能已经被更新的一版顶掉了。
    pub fn version(&self) -> Option<&str> {
        self.payload.get("hlc").and_then(|hlc| hlc.as_str())
    }
}

/// 事项的编号：没有设置面板的宿主让用户手输用的（`/sync show <编号>`）。由标识**和版本**
/// 算出——等确认的改动换了一版，编号就跟着换，用户照着旧编号 `accept` 不会接受到一份没
/// 看过的内容。取前 6 位当短编号；撞了的时候用更长的前缀（最长就是整个返回值）。
pub fn item_code_full(item: &PendingItem) -> String {
    let mut seed = item.id.clone();
    if let Some(version) = item.version() {
        seed.push('\n');
        seed.push_str(version);
    }
    sha256_hex(seed.as_bytes())[..16].to_string()
}

/// 事项的短编号（[`item_code_full`] 的前 6 位）。
pub fn item_code(item: &PendingItem) -> String {
    item_code_full(item)[..6].to_string()
}

/// 用户看过的那一版和现在等确认的不是同一版。
pub(crate) fn check_reviewed(item: &PendingItem, reviewed: Option<&str>) -> Result<()> {
    match item.version() {
        Some(current) if reviewed != Some(current) => Err(SyncError::other(
            "这项改动在你查看之后又有了更新的版本。请重新查看现在的内容，再决定接受还是拒绝",
        )),
        _ => Ok(()),
    }
}

pub fn put(paths: &SyncPaths, item: &PendingItem) -> Result<()> {
    let path = item_path(paths, &item.id)?;
    rclaw_store::atomic_write(path, &serde_json::to_vec_pretty(item)?)?;
    Ok(())
}

pub fn get(paths: &SyncPaths, id: &str) -> Option<PendingItem> {
    let bytes = std::fs::read(item_path(paths, id).ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 移除一条存成文件的事项。返回它原来在不在。
pub fn remove(paths: &SyncPaths, id: &str) -> bool {
    item_path(paths, id).is_ok_and(|path| std::fs::remove_file(path).is_ok())
}

/// 存成文件的事项，早的在前。
pub fn list(paths: &SyncPaths) -> Vec<PendingItem> {
    let Ok(entries) = std::fs::read_dir(&paths.pending) else {
        return Vec::new();
    };
    let mut items: Vec<PendingItem> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".json")?;
            let item = get(paths, id)?;
            (item.id == id).then_some(item)
        })
        .collect();
    items.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then_with(|| a.id.cmp(&b.id)));
    items
}

/// 记一条只读的提示（用户看过可以关掉）。同一个 `kind` + `key` 只记一次；写不进去只打
/// 日志——提示不值得让一轮同步失败。
pub fn notice(paths: &SyncPaths, kind: &str, key: &str, title: &str, detail: &str) {
    let id = item_id(kind, key);
    if get(paths, &id).is_some() {
        return;
    }
    let item = PendingItem {
        id,
        kind: kind.to_string(),
        title: title.to_string(),
        detail: detail.to_string(),
        origin: None,
        at_ms: now_ms(),
        actions: vec!["dismiss".into()],
        payload: serde_json::Value::Null,
    };
    if let Err(error) = put(paths, &item) {
        tracing::warn!("记录待处理事项失败: {error}");
    }
}

/// 撤掉一条提示（它说的情况已经不存在了）。
pub fn clear_notice(paths: &SyncPaths, kind: &str, key: &str) -> bool {
    remove(paths, &item_id(kind, key))
}

/// 从库里的状态现算出来的事项。
pub fn derived(conn: &Connection, now: i64) -> Result<Vec<PendingItem>> {
    let mut items = Vec::new();
    for (sid, info, held) in merge::quarantined(conn)? {
        let title = map::thread_sessions(conn, &sid)?
            .first()
            .map(|session| format!("会话「{}」有内容没能同步过来", session.chat_id))
            .unwrap_or_else(|| "有一个会话没能同步过来".to_string());
        items.push(PendingItem {
            id: format!("quarantine-{sid}"),
            kind: "quarantine".into(),
            title,
            detail: format!(
                "它收到的内容没有通过检查（{}），这个会话后续的 {held} 条同步数据先留着没有合入。\
                 如果那台设备上的 RClaw 版本更新，升级本机后点「重试」；确认不需要这些内容时点「丢弃」。",
                info.reason
            ),
            origin: Some(db::device_label(conn, &info.origin)?),
            at_ms: info.at_ms,
            actions: vec!["retry".into(), "discard".into()],
            payload: serde_json::json!({ "sid": sid }),
        });
    }
    for (session_id, _) in db::state_scan(conn, map::SCOPE_PUB_HALT, "")? {
        let Ok(id) = session_id.parse::<i64>() else {
            continue;
        };
        let Some(session) = map::session_map(conn, id)? else {
            continue;
        };
        items.push(PendingItem {
            id: format!("pub-halt-{session_id}"),
            kind: "pub-halt".into(),
            title: format!("会话「{}」暂停了向其他设备同步", session.chat_id),
            // 这个版本只有一种同步格式，没有「提升格式版本」的入口（§9.4）：能让它续发的
            // 只有把本机升级到认得那种形态的版本，所以只说这一件事。
            detail: "这个会话里有一条消息是这个版本的 RClaw 表达不了的形态（多半是更新的版本\
                     写下的），从那条消息起先不向其他设备发布，之前的内容照常同步。把这台设备\
                     升级到认得它的版本之后会自动接着发布，这条提示随之消失。"
                .into(),
            origin: None,
            at_ms: 0,
            actions: Vec::new(),
            payload: serde_json::json!({
                "channel": session.channel,
                "chat_id": session.chat_id,
            }),
        });
    }
    for (device, peer) in db::peers(conn)? {
        if peer.replaced_by.is_some() {
            continue;
        }
        let label = db::device_label(conn, &device)?;
        if let Some(reason) = &peer.halted {
            items.push(PendingItem {
                id: format!("upgrade-{device}"),
                kind: "upgrade".into(),
                title: format!("「{label}」的新内容要升级本机之后才能同步过来"),
                detail: reason.clone(),
                origin: Some(label.clone()),
                at_ms: peer.seen_ms,
                actions: Vec::new(),
                payload: serde_json::json!({ "device": device }),
            });
        }
        if let Some(error) = &peer.error {
            if peer.error_since_ms > 0 && now - peer.error_since_ms >= PEER_ERROR_GRACE_MS {
                items.push(PendingItem {
                    id: format!("peer-error-{device}"),
                    kind: "peer-error".into(),
                    title: format!("读不到「{label}」的同步数据"),
                    detail: error.clone(),
                    origin: Some(label),
                    at_ms: peer.error_since_ms,
                    actions: Vec::new(),
                    payload: serde_json::json!({ "device": device }),
                });
            }
        }
    }
    items.extend(crate::files::derived(conn)?);
    Ok(items)
}

/// 一台设备在面板里的样子。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceStatus {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub version: String,
    /// 是不是本机。
    pub me: bool,
    /// 最近一次看到它有新内容的时刻（本机时钟，Unix 毫秒；本机自己是上次写出的时刻）。
    pub seen_ms: i64,
    /// 超过 60 天没出现。
    pub stale: bool,
    /// 用户把它标成了「已丢失」（它的定时任务可以被强制接管，§8.6）。
    pub lost: bool,
    /// 它的日志里已经取回、还没处理完的 op 数。
    pub behind: u64,
}

/// 同步的整体状态。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SyncStatus {
    /// `off`（没开启）、`setup`（开启了，还没创建或加入同步空间）、`idle`、`syncing`、
    /// `paused`、`offline`（连不上存储）、`error`、`confirm`（计费网络下的首次同步在等
    /// 用户确认）、`standby`（另一个 RClaw 进程正在用同一个数据目录做同步，本进程不动）。
    pub state: String,
    /// 出错、离线、等确认时的说明。
    pub message: Option<String>,
    pub transport: String,
    /// 本机存着当前这种存储的密码（密码本身不外传，界面只需要知道填没填过）。
    pub secret_saved: bool,
    pub space_id: Option<String>,
    pub device_id: Option<String>,
    pub device_name: String,
    pub scopes: Vec<String>,
    /// 上次完整跑完一轮的时刻（Unix 毫秒）。
    pub last_sync_ms: i64,
    /// 还没传出去的 op 数。
    pub outbox: u64,
    /// 取回了还没合入的 op 数。
    pub inbox: u64,
    /// 等着上传的附件数。
    pub uploads: u64,
    /// 存量附件里还没上传的总大小（字节）：开启附件同步后由用户选上传范围。
    pub backlog_bytes: u64,
    pub devices: Vec<DeviceStatus>,
    pub pending: Vec<PendingItem>,
}

/// 设备清单：本机在最前，其余按名字排。被新身份取代的旧身份不列。
pub fn devices(conn: &Connection, me: &str, my_name: &str, now: i64) -> Result<Vec<DeviceStatus>> {
    let mut out = Vec::new();
    let log = db::log_state(conn)?;
    let own = db::device_info(conn, me)?.unwrap_or_default();
    out.push(DeviceStatus {
        id: me.to_string(),
        name: my_name.to_string(),
        platform: own.platform,
        version: own.version,
        me: true,
        seen_ms: log.head_ts,
        stale: false,
        lost: false,
        behind: 0,
    });
    let mut others = Vec::new();
    for (device, peer) in db::peers(conn)? {
        if device == me || peer.replaced_by.is_some() || peer.own_until.is_some() {
            continue;
        }
        let info = db::device_info(conn, &device)?.unwrap_or_default();
        others.push(DeviceStatus {
            name: db::device_label(conn, &device)?,
            lost: crate::files::cron::is_lost(conn, &device)?,
            id: device,
            platform: info.platform,
            version: info.version,
            me: false,
            seen_ms: peer.seen_ms,
            stale: peer.seen_ms > 0 && !peer.is_active(now),
            behind: peer.fetched.saturating_sub(peer.processed),
        });
    }
    others.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    out.extend(others);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_are_deduplicated_and_ids_are_checked() {
        let (_g, ctx) = crate::testing::ctx();
        notice(&ctx.paths, "orphans-expired", "k", "标题", "说明");
        notice(&ctx.paths, "orphans-expired", "k", "标题二", "说明二");
        let items = list(&ctx.paths);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "标题");
        assert_eq!(items[0].actions, ["dismiss"]);
        assert!(get(&ctx.paths, "../secrets").is_none());
        assert!(!remove(&ctx.paths, "../secrets"));
        assert!(clear_notice(&ctx.paths, "orphans-expired", "k"));
        assert!(list(&ctx.paths).is_empty());
    }

    #[test]
    fn derived_items_follow_database_state() {
        let (_g, ctx) = crate::testing::ctx();
        let _ = crate::testing::settings(&ctx);
        let device = "00112233445566ff";
        db::write(ctx.db(), |tx| {
            let mut peer = db::peer_state(tx, device)?;
            peer.halted = Some("格式版本更高".into());
            peer.error = Some("取不到".into());
            peer.error_since_ms = 1_000;
            db::put_peer_state(tx, device, &peer)
        })
        .unwrap();
        let soon = db::read(ctx.db(), |conn| derived(conn, 2_000)).unwrap();
        assert_eq!(
            soon.iter()
                .map(|item| item.kind.as_str())
                .collect::<Vec<_>>(),
            ["upgrade"]
        );
        let later = db::read(ctx.db(), |conn| derived(conn, 1_000 + PEER_ERROR_GRACE_MS)).unwrap();
        assert_eq!(later.len(), 2);
        assert!(later.iter().all(|item| is_item_id(&item.id)));
    }
}
