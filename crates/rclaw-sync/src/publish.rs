//! 发布扫描（§7.1、§9.3 第 1 步）：扫描本机变更，排进 `sync_outbox`。
//!
//! 会话写路径不知道同步的存在：扫描器事后发现还没有 `gid` 的消息行、与上次发布时不同的
//! 属性值，各自排成 op。有活动 turn 的会话跳过，等 turn 结束再扫。
//!
//! 标题、归档、目标按**分支**生效（§7.6）：`session.set{sid, at, 字段, 值}` 的 `at` 是设置
//! 时该分支末条消息的 `gid`，它对「链上含有 `at`」的本地会话生效；一个本地会话的属性取
//! 所有对它生效的设置里 HLC 最大的那个。

use rclaw_store::session::raw;
use rclaw_store::session::session_goal_key;
use rclaw_store::session_title_key;
use rclaw_store::sync_db::{self, SEAL_LOCAL};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::config::Scopes;
use crate::db::{self, scope, DeviceInfo};
use crate::error::Result;
use crate::ids;
use crate::map::{self, MapOutcome, SessionMap};
use crate::op::Op;
use crate::portable;
use crate::{ChatKey, Ctx};

/// 一轮同步用到的设置（配置在运行中可以改，每轮取一份）。
#[derive(Debug, Clone)]
pub struct Settings {
    /// 本机当前的设备身份。
    pub device: String,
    /// 本机用过的全部身份（当前的在最前）。
    pub my_ids: Vec<String>,
    pub scopes: Scopes,
    /// 参与同步的会话通道。
    pub channels: Vec<String>,
    pub device_name: String,
    pub trash_days: u64,
    /// 被引用的工作区文件大小上限（字节）。
    pub attachment_max: u64,
}

impl Settings {
    pub fn is_me(&self, device: &str) -> bool {
        self.my_ids.iter().any(|id| id == device)
    }
}

pub const KV_SCAN: &str = "sync/scan";

/// 目标（`/goal`）在线上的大小上限。
pub const GOAL_MAX: usize = 16 * 1024;

/// 这个会话是否参与会话同步：范围开着、通道在名单里、键合规、不是 bot / 群聊的内部
/// 会话、没有被标成「仅本机」。
pub fn eligible(conn: &Connection, s: &Settings, channel: &str, chat_id: &str) -> Result<bool> {
    if !s.scopes.sessions
        || !s.channels.iter().any(|c| c == channel)
        || !ids::is_valid_chat_id(chat_id)
        || chat_id.starts_with("bot-")
        || chat_id.starts_with("grp-")
    {
        return Ok(false);
    }
    Ok(sync_db::kv_get(conn, &db::local_only_key(channel, chat_id))?.is_none())
}

/// 扫描一个会话：给新行分配身份并排队，属性有变化就登记。在调用方的写事务里执行。
/// 不参与同步的会话只分配身份（合入时判断「末条」要用），不排队。
pub fn scan_session(conn: &Connection, s: &Settings, session_id: i64) -> Result<MapOutcome> {
    let Some((channel, chat_id)) = raw::session_key(conn, session_id)? else {
        return Ok(MapOutcome::default());
    };
    let queue = eligible(conn, s, &channel, &chat_id)?;
    let mut outcome = map::ensure_mapped(conn, &s.device, session_id, queue)?;
    if queue {
        if let Some(session) = map::session_map(conn, session_id)? {
            outcome.queued += publish_attrs(conn, &s.device, &session, true)?;
        }
    }
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// 属性
// ---------------------------------------------------------------------------

/// 一个会话的三个可同步属性。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Attrs {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub goal: Option<serde_json::Value>,
}

pub const FIELDS: [&str; 3] = ["title", "archived", "goal"];

impl Attrs {
    fn get(&self, field: &str) -> serde_json::Value {
        match field {
            "title" => self
                .title
                .clone()
                .map_or(serde_json::Value::Null, serde_json::Value::String),
            "archived" => serde_json::Value::Bool(self.archived),
            _ => self.goal.clone().unwrap_or(serde_json::Value::Null),
        }
    }

    fn set(&mut self, field: &str, value: &serde_json::Value) {
        match field {
            "title" => self.title = value.as_str().and_then(portable::clean_title),
            "archived" => self.archived = value.as_bool().unwrap_or(false),
            _ => self.goal = clean_goal(value),
        }
    }
}

/// 目标只接受对象（运行时写的就是 `{"objective": …, "created_at": …}`），并限制大小。
fn clean_goal(value: &serde_json::Value) -> Option<serde_json::Value> {
    (value.is_object() && value.to_string().len() <= GOAL_MAX).then(|| value.clone())
}

/// `session.set` 的值是否合规（§11.3）。
pub fn check_attr(field: &str, value: &serde_json::Value) -> bool {
    match field {
        "title" => value.is_null() || value.as_str().is_some_and(|t| t.len() <= 1024),
        "archived" => value.is_boolean(),
        "goal" => value.is_null() || clean_goal(value).is_some(),
        _ => false,
    }
}

/// 读出会话当前的属性值。
pub fn local_attrs(conn: &Connection, channel: &str, chat_id: &str) -> Result<Attrs> {
    let title = sync_db::kv_get(conn, &session_title_key(channel, chat_id))?
        .and_then(|raw| serde_json::from_str::<String>(&raw).ok())
        .and_then(|title| portable::clean_title(&title));
    let archived: bool = conn
        .query_row(
            "SELECT archived_at IS NOT NULL FROM session WHERE channel = ?1 AND chat_id = ?2",
            params![channel, chat_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false);
    let goal = sync_db::kv_get(conn, &session_goal_key(channel, chat_id))?
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|value| clean_goal(&value));
    Ok(Attrs {
        title,
        archived,
        goal,
    })
}

fn write_local_attr(
    conn: &Connection,
    channel: &str,
    chat_id: &str,
    field: &str,
    attrs: &Attrs,
) -> Result<()> {
    match field {
        "title" => {
            let key = session_title_key(channel, chat_id);
            match &attrs.title {
                Some(title) => sync_db::kv_put(
                    conn,
                    &key,
                    &serde_json::Value::String(title.clone()).to_string(),
                )?,
                None => sync_db::kv_del(conn, &key)?,
            }
        }
        "archived" => {
            // 归档时刻取现在；已经是目标状态时不动（保留原来的归档时刻）。
            conn.execute(
                "UPDATE session SET archived_at = CASE WHEN ?3 THEN COALESCE(archived_at, ?4) \
                 ELSE NULL END WHERE channel = ?1 AND chat_id = ?2",
                params![channel, chat_id, attrs.archived, raw::now_millis()],
            )?;
        }
        _ => {
            let key = session_goal_key(channel, chat_id);
            match &attrs.goal {
                Some(goal) => sync_db::kv_put(conn, &key, &goal.to_string())?,
                None => sync_db::kv_del(conn, &key)?,
            }
        }
    }
    Ok(())
}

fn published_attrs(session: &SessionMap) -> Attrs {
    session
        .published
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default()
}

fn store_published(conn: &Connection, session_id: i64, attrs: &Attrs) -> Result<()> {
    conn.execute(
        "UPDATE sync_session SET published = ?2 WHERE session_id = ?1",
        params![session_id, serde_json::to_string(attrs)?],
    )?;
    Ok(())
}

/// 分支属性在 `sync_state` 里的键：`<sid>|<at>|<字段>`（`at` 为空 = 对所有分支生效）。
pub fn attr_key(sid: &str, at: Option<&str>, field: &str) -> String {
    format!("{sid}|{}|{field}", at.unwrap_or(""))
}

/// 记下一条属性设置（后写者胜）。返回是否比已有的新。
pub fn record_attr(
    conn: &Connection,
    sid: &str,
    at: Option<&str>,
    field: &str,
    hlc: &str,
    value: &serde_json::Value,
) -> Result<bool> {
    db::state_put_lww(
        conn,
        scope::ATTR,
        &attr_key(sid, at, field),
        hlc,
        None,
        Some(&value.to_string()),
    )
}

/// 把本机改过的属性记进分支属性表；`publish` 为真时同时登记成 `session.set` 发出去。
/// 不发布的会话（通道不在同步名单里、`session.open` 还没排队）也要记：合入远端的设置时
/// 按 HLC 比较，本机刚改的值不该被更早的远端值盖掉。返回登记发出的条数。
pub fn publish_attrs(
    conn: &Connection,
    device: &str,
    session: &SessionMap,
    publish: bool,
) -> Result<usize> {
    let publish = publish && session.sealed != SEAL_LOCAL;
    let current = local_attrs(conn, &session.channel, &session.chat_id)?;
    let published = published_attrs(session);
    if current == published {
        return Ok(0);
    }
    let mut count = 0usize;
    for field in FIELDS {
        let value = current.get(field);
        if value == published.get(field) {
            continue;
        }
        let hlc = if publish {
            count += 1;
            map::enqueue(
                conn,
                device,
                Some(session.session_id),
                &Op::SessionSet {
                    sid: session.sid.clone(),
                    at: session.tip.clone(),
                    field: field.to_string(),
                    value: value.clone(),
                },
            )?
        } else {
            sync_db::hlc_tick(conn, device)?
        };
        record_attr(
            conn,
            &session.sid,
            session.tip.as_deref(),
            field,
            &hlc,
            &value,
        )?;
        if field == "title" {
            // 用户自己起了名字：继承来的「某设备的分支」之类的标注不再显示。
            let key = session.session_id.to_string();
            db::state_del(conn, scope::BRANCH, &key)?;
            db::state_del(conn, scope::LANDED, &key)?;
        }
    }
    store_published(conn, session.session_id, &current)?;
    Ok(count)
}

/// 合入远端内容之后重算一个会话的属性：先把本机没登记过的改动记下（参与同步的会话
/// 同时发出去），再按各设置的 HLC 取值写回。返回会话的属性是否有变化。
pub fn refresh_attrs(conn: &Connection, s: &Settings, session_id: i64) -> Result<bool> {
    let Some(session) = map::session_map(conn, session_id)? else {
        return Ok(false);
    };
    let publish = eligible(conn, s, &session.channel, &session.chat_id)?;
    publish_attrs(conn, &s.device, &session, publish)?;
    let Some(session) = map::session_map(conn, session_id)? else {
        return Ok(false);
    };
    apply_effective(conn, &session)
}

/// 按「对这个分支生效的设置里 HLC 最大的」重算会话的属性并写回本机。返回是否有变化。
/// 调用前应先为这个会话登记本机的改动（[`publish_attrs`]），否则它们会被当成旧值盖掉。
pub fn apply_effective(conn: &Connection, session: &SessionMap) -> Result<bool> {
    let mut best: [Option<(String, serde_json::Value)>; 3] = [None, None, None];
    let prefix = format!("{}|", session.sid);
    for (key, row) in db::state_scan(conn, scope::ATTR, &prefix)? {
        let mut parts = key[prefix.len()..].splitn(2, '|');
        let (Some(at), Some(field)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Some(slot) = FIELDS.iter().position(|f| *f == field) else {
            continue;
        };
        if !at.is_empty() && map::message_of(conn, session.session_id, at)?.is_none() {
            continue;
        }
        let Some(value) = row
            .value
            .as_deref()
            .and_then(|v| serde_json::from_str(v).ok())
        else {
            continue;
        };
        if best[slot].as_ref().is_none_or(|(hlc, _)| row.hlc > *hlc) {
            best[slot] = Some((row.hlc, value));
        }
    }
    let current = local_attrs(conn, &session.channel, &session.chat_id)?;
    let mut target = current.clone();
    for (slot, field) in FIELDS.iter().enumerate() {
        if let Some((_, value)) = &best[slot] {
            target.set(field, value);
        }
    }
    let changed = target != current;
    if changed {
        for field in FIELDS {
            if target.get(field) != current.get(field) {
                write_local_attr(conn, &session.channel, &session.chat_id, field, &target)?;
            }
        }
    }
    if published_attrs(session) != target {
        // 写回的值记成「已发布」：它来自别的设备，不再回声。
        store_published(conn, session.session_id, &target)?;
    }
    Ok(changed)
}

// ---------------------------------------------------------------------------
// 引擎一轮的发布扫描
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct ScanReport {
    /// 排进待发队列的 op 数。
    pub queued: usize,
    /// 有会话因为正忙被跳过：turn 结束后要再扫一次。
    pub skipped_busy: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ScanCursor {
    /// 消息行 id 不大于它的都扫过了。
    #[serde(default)]
    max_id: i64,
}

/// 让下一次扫描从头来（范围、通道名单、「仅本机」标记变了之后）。
pub fn reset_cursor(conn: &Connection) -> Result<()> {
    Ok(sync_db::kv_del(conn, KV_SCAN)?)
}

fn max_message_id(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT COALESCE(MAX(id), 0) FROM message", [], |row| {
            row.get(0)
        })?,
    )
}

/// 参与同步、而且有行 id 大于 `from` 的消息的会话：`(会话行 id, 通道, 会话键)`。没有消息
/// 的会话不发布。
fn candidates(conn: &Connection, s: &Settings, from: i64) -> Result<Vec<(i64, String, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.id, s.channel, s.chat_id FROM session s WHERE EXISTS \
         (SELECT 1 FROM message m WHERE m.session_id = s.id AND m.id > ?1) ORDER BY s.id",
    )?;
    let rows = stmt.query_map(params![from], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (session_id, channel, chat_id) = row?;
        if eligible(conn, s, &channel, &chat_id)? {
            out.push((session_id, channel, chat_id));
        }
    }
    Ok(out)
}

/// 两轮之间那次本机变更检查的结果。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LocalCheck {
    /// 消息表现在的最大行 id（下次检查时传回来）。
    pub max_id: i64,
    /// 有新消息行等着发布，它所在的会话现在不忙：该来一轮了。
    pub ready: bool,
    /// 有新消息行所在的会话正忙（turn 还没结束）：下次检查要再看一遍。
    pub busy: bool,
}

/// 这个会话有没有发布扫描还没碰过的消息行。因为出现了不认识的形态而暂停发布的会话
/// 不算：它的尾巴要等升级之后才发得出去。
fn has_unscanned(conn: &Connection, session_id: i64) -> Result<bool> {
    let unmapped: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM message WHERE session_id = ?1 AND id > COALESCE( \
           (SELECT MAX(message_id) FROM sync_msg WHERE session_id = ?1), 0))",
        params![session_id],
        |row| row.get(0),
    )?;
    Ok(unmapped && db::state_get(conn, map::SCOPE_PUB_HALT, &session_id.to_string())?.is_none())
}

/// 两轮之间的本机变更检查（§7.1）：有没有等着发布的新消息行。`seen` 是上次检查时消息表
/// 的最大行 id，`recheck` 是上次有没有会话因为正忙被搁下——都没变的时候，成本就是一条
/// `SELECT MAX(id)`。属性的变化不在这里看（宿主改属性时会直接通知引擎）。
pub fn check_local(ctx: &Ctx, s: &Settings, seen: i64, recheck: bool) -> Result<LocalCheck> {
    let (max_id, pending) = db::read(ctx.db(), |conn| {
        let max_now = max_message_id(conn)?;
        if !s.scopes.sessions || (max_now == seen && !recheck) {
            return Ok((max_now, Vec::new()));
        }
        // 游标之后有新行的会话里，只有真的还有行没扫过的才算：游标会因为别的会话正忙而
        // 停在原地，已经扫过的会话不该每次检查都再触发一轮。
        let cursor: ScanCursor = db::kv_json(conn, KV_SCAN)?.unwrap_or_default();
        let mut pending = Vec::new();
        for (session_id, channel, chat_id) in candidates(conn, s, cursor.max_id)? {
            if has_unscanned(conn, session_id)? {
                pending.push((session_id, channel, chat_id));
            }
        }
        Ok((max_now, pending))
    })?;
    let mut check = LocalCheck {
        max_id,
        ..LocalCheck::default()
    };
    for (_, channel, chat_id) in pending {
        if ctx.gate.is_busy(&channel, &chat_id) {
            check.busy = true;
        } else {
            check.ready = true;
        }
    }
    Ok(check)
}

/// 扫描会话：新消息行与属性变化。`full` 为真时所有会话都过一遍（启动、范围变化、导入过
/// 会话包之后）：此前只分配了身份的会话也排进队列。
pub fn scan_sessions(ctx: &Ctx, s: &Settings, full: bool) -> Result<ScanReport> {
    let mut report = ScanReport::default();
    if !s.scopes.sessions {
        return Ok(report);
    }
    let (cursor, max_now, candidates) = db::read(ctx.db(), |conn| {
        let cursor: ScanCursor = db::kv_json(conn, KV_SCAN)?.unwrap_or_default();
        let max_now = max_message_id(conn)?;
        // 增量扫描只看游标之后有新行的会话。
        let from = if full { 0 } else { cursor.max_id };
        Ok((cursor, max_now, candidates(conn, s, from)?))
    })?;
    for (session_id, channel, chat_id) in candidates {
        if ctx.gate.is_busy(&channel, &chat_id) {
            report.skipped_busy = true;
            continue;
        }
        let outcome = db::write(ctx.db(), |tx| scan_session(tx, s, session_id))?;
        report.queued += outcome.queued;
    }
    if !report.skipped_busy && (full || max_now != cursor.max_id) {
        db::write(ctx.db(), |tx| {
            db::kv_put_json(tx, KV_SCAN, &ScanCursor { max_id: max_now })
        })?;
    }
    report.queued += scan_attrs(ctx, s)?;
    Ok(report)
}

/// 属性扫描：一条查询读出所有已发布会话的当前值，只有与上次发布不同的才开事务登记。
fn scan_attrs(ctx: &Ctx, s: &Settings) -> Result<usize> {
    let changed: Vec<i64> = db::read(ctx.db(), |conn| {
        let mut stmt = conn.prepare_cached(
            "SELECT y.session_id, s.channel, s.chat_id, y.published, s.archived_at IS NOT NULL, \
               (SELECT value FROM kv WHERE key = 'session-title/' || s.channel || '/' || s.chat_id), \
               (SELECT value FROM kv WHERE key = 'session-goal/' || s.channel || '/' || s.chat_id) \
             FROM sync_session y JOIN session s ON s.id = y.session_id WHERE y.sealed != ?1",
        )?;
        let rows = stmt.query_map(params![SEAL_LOCAL], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, bool>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut changed = Vec::new();
        for row in rows {
            let (session_id, channel, chat_id, published, archived, title, goal) = row?;
            let published: Attrs = published
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or_default();
            let current = Attrs {
                title: title
                    .and_then(|raw| serde_json::from_str::<String>(&raw).ok())
                    .and_then(|title| portable::clean_title(&title)),
                archived,
                goal: goal
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                    .and_then(|value| clean_goal(&value)),
            };
            if current != published && eligible(conn, s, &channel, &chat_id)? {
                changed.push(session_id);
            }
        }
        Ok(changed)
    })?;
    let mut queued = 0usize;
    for session_id in changed {
        queued += db::write(ctx.db(), |tx| match map::session_map(tx, session_id)? {
            Some(session) => publish_attrs(tx, &s.device, &session, true),
            None => Ok(0),
        })?;
    }
    Ok(queued)
}

/// 本机的 `device.hello`：名字、平台、版本与上次发出的不同就再发一条。
pub fn publish_hello(
    ctx: &Ctx,
    s: &Settings,
    platform: &str,
    version: &str,
    replaces: Option<&str>,
) -> Result<bool> {
    db::write(ctx.db(), |tx| put_hello(tx, s, platform, version, replaces))
}

/// [`publish_hello`] 在调用方的写事务里执行的那一半。
pub fn put_hello(
    conn: &Connection,
    s: &Settings,
    platform: &str,
    version: &str,
    replaces: Option<&str>,
) -> Result<bool> {
    let wanted = DeviceInfo {
        name: s.device_name.clone(),
        platform: platform.to_string(),
        version: version.to_string(),
        replaces: replaces.map(str::to_string),
    };
    let current = db::device_info(conn, &s.device)?;
    if current.as_ref().is_some_and(|current| {
        current.name == wanted.name
            && current.platform == wanted.platform
            && current.version == wanted.version
            && (wanted.replaces.is_none() || current.replaces == wanted.replaces)
    }) {
        return Ok(false);
    }
    let replaces = wanted
        .replaces
        .clone()
        .or_else(|| current.and_then(|current| current.replaces));
    let hlc = map::enqueue(
        conn,
        &s.device,
        None,
        &Op::DeviceHello {
            name: wanted.name.clone(),
            platform: wanted.platform.clone(),
            version: wanted.version.clone(),
            replaces: replaces.clone(),
        },
    )?;
    db::state_put_json(
        conn,
        scope::DEVICE,
        &s.device,
        &hlc,
        &DeviceInfo { replaces, ..wanted },
    )?;
    Ok(true)
}

/// 会话被同步改动后通知宿主用的键。
pub fn chat_key(channel: &str, chat_id: &str) -> ChatKey {
    ChatKey {
        channel: channel.to_string(),
        chat_id: chat_id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rclaw_core::{Message, Role};

    fn setup() -> (tempfile::TempDir, Ctx, Settings) {
        let (guard, ctx) = crate::testing::ctx();
        let settings = crate::testing::settings(&ctx);
        (guard, ctx, settings)
    }

    fn ops(ctx: &Ctx) -> Vec<serde_json::Value> {
        db::read(ctx.db(), |conn| {
            let mut stmt = conn.prepare("SELECT op FROM sync_outbox ORDER BY id")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(serde_json::from_str(&row?).unwrap());
            }
            Ok(out)
        })
        .unwrap()
    }

    #[test]
    fn scans_only_eligible_sessions_and_is_idle_when_nothing_changed() {
        let (_g, ctx, s) = setup();
        let sessions = &ctx.env.sessions;
        sessions
            .append("desktop", "c1", &Message::plain(Role::User, "一"))
            .unwrap();
        sessions
            .append("desktop", "bot-x", &Message::plain(Role::User, "内部"))
            .unwrap();
        sessions
            .append("weixin", "u1", &Message::plain(Role::User, "别的通道"))
            .unwrap();
        let report = scan_sessions(&ctx, &s, false).unwrap();
        assert_eq!(report.queued, 2, "session.open + msg");
        assert!(!report.skipped_busy);
        // 没有新内容：不再排队。
        assert_eq!(scan_sessions(&ctx, &s, false).unwrap().queued, 0);
        // 新消息只扫到它所在的会话。
        sessions
            .append("desktop", "c1", &Message::plain(Role::Assistant, "二"))
            .unwrap();
        assert_eq!(scan_sessions(&ctx, &s, false).unwrap().queued, 1);
        let kinds: Vec<String> = ops(&ctx)
            .iter()
            .map(|op| op["t"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(kinds, ["session.open", "msg", "msg"]);
    }

    #[test]
    fn local_only_sessions_are_not_published() {
        let (_g, ctx, s) = setup();
        ctx.env
            .sessions
            .append("desktop", "c1", &Message::plain(Role::User, "一"))
            .unwrap();
        db::write(ctx.db(), |tx| {
            Ok(sync_db::kv_put(
                tx,
                &db::local_only_key("desktop", "c1"),
                "true",
            )?)
        })
        .unwrap();
        assert_eq!(scan_sessions(&ctx, &s, true).unwrap().queued, 0);
    }

    #[test]
    fn attribute_changes_become_session_set_once() {
        let (_g, ctx, s) = setup();
        let sessions = &ctx.env.sessions;
        sessions
            .append("desktop", "c1", &Message::plain(Role::User, "一"))
            .unwrap();
        scan_sessions(&ctx, &s, false).unwrap();
        sessions
            .set_session_title("desktop", "c1", "行程规划")
            .unwrap();
        sessions.set_archived("desktop", "c1", true).unwrap();
        assert_eq!(scan_sessions(&ctx, &s, false).unwrap().queued, 2);
        assert_eq!(scan_sessions(&ctx, &s, false).unwrap().queued, 0);
        let sets: Vec<serde_json::Value> = ops(&ctx)
            .into_iter()
            .filter(|op| op["t"] == "session.set")
            .collect();
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0]["field"], "title");
        assert_eq!(sets[0]["value"], "行程规划");
        assert!(sets[0]["at"].is_string(), "at 是设置时的分支末条");
        assert_eq!(sets[1]["field"], "archived");
        assert_eq!(sets[1]["value"], true);
    }

    #[test]
    fn effective_attributes_follow_the_branch_and_do_not_echo() {
        let (_g, ctx, s) = setup();
        let sessions = &ctx.env.sessions;
        sessions
            .append("desktop", "c1", &Message::plain(Role::User, "一"))
            .unwrap();
        scan_sessions(&ctx, &s, false).unwrap();
        let session = db::read(ctx.db(), |c| map::session_map_by_key(c, "desktop", "c1"))
            .unwrap()
            .unwrap();
        let tip = session.tip.clone().unwrap();
        let later = "9999999999999-0000-ffffffffffffffff";
        db::write(ctx.db(), |tx| {
            // 对这条分支生效的设置。
            record_attr(
                tx,
                &session.sid,
                Some(&tip),
                "title",
                later,
                &serde_json::json!("远端起的名字"),
            )?;
            // 设在本机没有的消息上：不生效。
            record_attr(
                tx,
                &session.sid,
                Some("0123456789abcdef.0123456789abcdef"),
                "archived",
                later,
                &serde_json::json!(true),
            )?;
            assert!(apply_effective(tx, &session)?);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            sessions.session_title("desktop", "c1").unwrap().as_deref(),
            Some("远端起的名字")
        );
        let list = sessions.list_sessions(Some("desktop")).unwrap();
        assert!(!list[0].archived);
        // 写回的值不会被当成本机改动再发出去。
        let before = ops(&ctx).len();
        assert_eq!(scan_sessions(&ctx, &s, false).unwrap().queued, 0);
        assert_eq!(ops(&ctx).len(), before);
    }
}
