//! 多端同步 / 会话包用的原始行原语（`docs/multi-device-sync.md` §7.2、§12.1）。
//!
//! `SessionStore` 的常规写路径给消息盖当前时间、把 `updated_at` 刷到当前时间、只接受
//! `Message`。导入另一台设备上的会话要按原样写行，并且和同步引擎自己的映射、位置更新
//! 在同一个事务里提交——所以这里的自由函数都接受调用方的事务连接，不自己开事务。
//!
//! 现有写路径一行不改；这些函数只被 `rclaw-sync` 调用。

use std::collections::HashMap;
use std::path::PathBuf;

use rusqlite::{params, Connection, OptionalExtension};

use super::{
    artifact_chat_key, fork_lineage_key, insert_tool_call_metas, parse_tool_call_meta,
    remove_artifact_dir, session_goal_key, session_id, session_model_key,
    validate_compaction_anchors, CompactionMeta, SessionStore, ToolCallMetaRecord,
};
use crate::db::now_ms;
use crate::{sync_db, Result, StoreError};

/// 会话标题的 kv 键。标题在库里（不在 `desktop_prefs.json`），才能随会话同步；与
/// `session-goal/…`、`session-fork/…` 同一模式，随会话删除 / 全清一并清理。
pub fn session_title_key(channel: &str, chat_id: &str) -> String {
    format!("session-title/{channel}/{chat_id}")
}

/// `message` 表的一行，原样读出（不解析 `data`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawMessageRow {
    pub id: i64,
    pub session_id: i64,
    pub role: String,
    /// Unix 秒。
    pub ts: i64,
    pub data: String,
    pub meta: Option<String>,
}

/// 删除会话时同步侧怎么处理（§7.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDeleteSync {
    /// 登记 `session.delete`：其他设备上末条相同的分支随之删除，并进它们的回收站。
    Propagate,
    /// 登记「彻底删除」：其他设备收到后不进回收站。
    Purge,
    /// 不登记：这次删除本身就是在合入远端发来的删除。
    Local,
}

/// 会话里产生于其他设备的消息来源（跨设备续聊说明用，§7.8）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageOrigin {
    pub device_id: String,
    /// 设备名；那台设备的 `device.hello` 还没到时为空。
    pub name: String,
    /// 平台（`windows` / `android` / …）；未知为空。
    pub platform: String,
    pub messages: u32,
}

const FORK_NOTICE_INHERITED: &str = "上面的历史继承自父会话。";
const FORK_NOTICE_TAIL: &str = "\n请从本分叉继续：不要假设分叉点之后的消息在本会话发生过。";

/// 分叉告知行的正文。`parent = Some((父会话 chat_id, 锚点消息 id))` 是本机形态；`None` 是
/// 不含任何本机 id 的规范文本——线上用它，父会话不在本机时落库的也是它。
pub fn fork_notice_text(parent: Option<(&str, i64)>, files_note: &str) -> String {
    match parent {
        Some((parent_chat_id, anchor_message_id)) => format!(
            "【会话分叉】本会话自父会话 `{parent_chat_id}` 的消息 #{anchor_message_id} 分叉而来，\
             {FORK_NOTICE_INHERITED}{files_note}{FORK_NOTICE_TAIL}"
        ),
        None => format!(
            "【会话分叉】本会话自另一个会话分叉而来（父会话不在本机），\
             {FORK_NOTICE_INHERITED}{files_note}{FORK_NOTICE_TAIL}"
        ),
    }
}

/// 从分叉告知行的正文里取回 `files_note`（两种形态的正文都认；认不出返回空串）。
pub fn fork_notice_files_note(text: &str) -> &str {
    text.split_once(FORK_NOTICE_INHERITED)
        .and_then(|(_, rest)| rest.strip_suffix(FORK_NOTICE_TAIL))
        .unwrap_or("")
}

/// 分叉告知行的本机形态 `meta`。父会话不在本机时两个值为 `null`——`meta` 本身必须保留，
/// 前端靠它把这一行排除在回放之外。
pub fn fork_notice_meta(parent: Option<(&str, i64)>) -> String {
    match parent {
        Some((parent_chat_id, anchor_message_id)) => serde_json::json!({
            "fork_notice": { "parent": parent_chat_id, "anchor": anchor_message_id }
        }),
        None => serde_json::json!({ "fork_notice": { "parent": null, "anchor": null } }),
    }
    .to_string()
}

/// 解析本机形态的分叉告知 `meta`：`(父会话 chat_id, 锚点消息 id)`，都可能为空。
/// 不是分叉告知形态返回 `None`。
pub fn fork_notice_anchor(meta: &str) -> Option<(Option<String>, Option<i64>)> {
    let value: serde_json::Value = serde_json::from_str(meta).ok()?;
    let notice = value.get("fork_notice")?;
    Some((
        notice
            .get("parent")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        notice
            .get("anchor")
            .and_then(|v| v.as_i64())
            .filter(|id| *id > 0),
    ))
}

/// 解析本机形态的 compaction `meta`：`(head_until, tail_from)`。
pub fn compaction_anchors(meta: &str) -> Option<(Option<i64>, i64)> {
    CompactionMeta::parse(meta).map(|m| (m.head_until, m.tail_from))
}

/// 用已经换成本机行 id 的锚点生成本机形态的 compaction `meta`。按 `append_compaction`
/// 同一套规则校验；不通过返回 `None`，调用方把这一行降级为普通行（`fork_session` 对
/// boundary 也是这样处理的）。必须在 boundary 行自己插入之前调用。
pub fn local_compaction_meta(
    conn: &Connection,
    session_id: i64,
    head_until: Option<i64>,
    tail_from: i64,
) -> Result<Option<String>> {
    if head_until.is_some_and(|h| h >= tail_from) {
        return Ok(None);
    }
    match validate_compaction_anchors(conn, session_id, head_until, tail_from) {
        Ok(()) => Ok(Some(
            CompactionMeta {
                head_until,
                tail_from,
            }
            .to_json(),
        )),
        Err(StoreError::Other(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// 按会话键查本机会话行 id。
pub fn find_session(conn: &Connection, channel: &str, chat_id: &str) -> Result<Option<i64>> {
    session_id(conn, channel, chat_id)
}

/// 会话行 id → `(channel, chat_id)`。
pub fn session_key(conn: &Connection, session_id: i64) -> Result<Option<(String, String)>> {
    Ok(conn
        .query_row(
            "SELECT channel, chat_id FROM session WHERE id = ?1",
            params![session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// 别的设备报来的时间（毫秒）拿来排会话列表之前先卡在「现在」：那台设备的时钟快（或者
/// 时间是编出来的）时，这个会话不能凭一个未来的时间一直排在列表最前面——本机之后再怎么
/// 续写别的会话也压不过它。
fn not_in_future(at_ms: i64) -> i64 {
    at_ms.min(now_ms())
}

/// 新建一个会话行，`created_at` 与 `updated_at` 都取 `created_at_ms`（来源设备上的创建
/// 时间；比现在还晚的按现在算）。键已被占用时报错——导入从不覆盖已有会话。
pub fn create_session(
    conn: &Connection,
    channel: &str,
    chat_id: &str,
    created_at_ms: i64,
) -> Result<i64> {
    if session_id(conn, channel, chat_id)?.is_some() {
        return Err(StoreError::Other(format!(
            "会话 {channel}/{chat_id} 已存在，导入拒绝覆盖"
        )));
    }
    conn.execute(
        "INSERT INTO session(channel, chat_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
        params![channel, chat_id, not_in_future(created_at_ms)],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 按原始行导入一条消息，返回新行 id。
///
/// - 行格式与 `insert_message` 相同，`role` / `ts` / `data` / `meta` 原样写入（调用方已校验）；
/// - `tool_call_meta` 按新行 id 写入；
/// - `session.updated_at` 取「现值」与「这条消息自己的时间」的较大者，**不用当前时间**：
///   否则首次同步后所有导入的会话都排到列表最前，以后每导入一条旧消息又把会话顶上去。
///   消息自己的时间比现在还晚时按现在算（见 [`not_in_future`]）；`message.ts` 原样保留；
/// - 这一行是 compaction boundary 时作废该会话的 usage 锚点（活动视图变了）。
pub fn insert_raw_message(
    conn: &Connection,
    session_id: i64,
    role: &str,
    ts: i64,
    data: &str,
    meta: Option<&str>,
    tool_metas: &[ToolCallMetaRecord],
) -> Result<i64> {
    conn.execute(
        "INSERT INTO message(session_id, role, ts, data, meta) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![session_id, role, ts, data, meta],
    )?;
    let id = conn.last_insert_rowid();
    insert_tool_call_metas(conn, session_id, id, tool_metas)?;
    conn.execute(
        "UPDATE session SET updated_at = max(updated_at, ?2) WHERE id = ?1",
        params![session_id, not_in_future(ts.saturating_mul(1000))],
    )?;
    if meta.is_some_and(|raw| CompactionMeta::parse(raw).is_some()) {
        conn.execute(
            "DELETE FROM usage_anchor WHERE (channel, chat_id) = \
             (SELECT channel, chat_id FROM session WHERE id = ?1)",
            params![session_id],
        )?;
    }
    Ok(id)
}

fn row_to_raw(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawMessageRow> {
    Ok(RawMessageRow {
        id: row.get(0)?,
        session_id: row.get(1)?,
        role: row.get(2)?,
        ts: row.get(3)?,
        data: row.get(4)?,
        meta: row.get(5)?,
    })
}

/// 读一行原始消息。
pub fn raw_message(conn: &Connection, message_id: i64) -> Result<Option<RawMessageRow>> {
    Ok(conn
        .query_row(
            "SELECT id, session_id, role, ts, data, meta FROM message WHERE id = ?1",
            params![message_id],
            row_to_raw,
        )
        .optional()?)
}

/// 一个会话的全部原始行（id 升序 = 链的顺序），含被摘要的中段与所有 boundary。
pub fn raw_messages(conn: &Connection, session_id: i64) -> Result<Vec<RawMessageRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, session_id, role, ts, data, meta FROM message WHERE session_id = ?1 \
         ORDER BY id",
    )?;
    let rows = stmt.query_map(params![session_id], row_to_raw)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 一个会话里 id 在 `(after, upto]` 之间的原始行，id 升序，最多 `limit` 行、`data` 合计
/// 约 `bytes` 字节（至少一行）。长会话分页读：每页一次短的读取，不长时间占着库锁，也
/// 不把整个会话一次读进内存。
pub fn raw_messages_range(
    conn: &Connection,
    session_id: i64,
    after: i64,
    upto: i64,
    limit: usize,
    bytes: usize,
) -> Result<Vec<RawMessageRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, session_id, role, ts, data, meta FROM message \
         WHERE session_id = ?1 AND id > ?2 AND id <= ?3 ORDER BY id LIMIT ?4",
    )?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut rows = stmt.query(params![session_id, after, upto, limit])?;
    let mut out = Vec::new();
    let mut total = 0usize;
    while let Some(row) = rows.next()? {
        let row = row_to_raw(row)?;
        total = total.saturating_add(row.data.len());
        out.push(row);
        if total >= bytes {
            break;
        }
    }
    Ok(out)
}

/// 一条 assistant(tool_calls) 行的工具调用侧表记录（坏行跳过）。
pub fn tool_call_metas(
    conn: &Connection,
    session_id: i64,
    message_id: i64,
) -> Result<Vec<ToolCallMetaRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT meta FROM tool_call_meta WHERE session_id = ?1 AND message_id = ?2 ORDER BY rowid",
    )?;
    let rows = stmt.query_map(params![session_id, message_id], |row| {
        row.get::<_, String>(0)
    })?;
    let mut out = Vec::new();
    for row in rows {
        if let Some(meta) = parse_tool_call_meta(message_id, &row?) {
            out.push(meta);
        }
    }
    Ok(out)
}

/// 原始前缀复制：把 `src_session` 里 id ≤ `upto_message_id` 的行原样复制进 `dst_session`，
/// 返回 `(源行 id, 新行 id)`（按源行 id 升序）。并发续写自动分支时用（§7.4）。
///
/// 与 `fork_session` 不同，这里复制的是完整转录而不是活动视图：boundary 保持为 boundary，
/// 锚点重映射到新行 id（映射不到则降级为普通行）；其余 `meta` 原样保留。消息体里指向源
/// 会话 artifact 目录的路径改写到目标会话的目录（实体文件由调用方在提交后复制）；
/// `tool_call_meta` 随行复制，文件 checkpoint 不复制。
pub fn copy_raw_prefix(
    conn: &Connection,
    src_session: i64,
    upto_message_id: i64,
    dst_session: i64,
    src_artifacts_prefix: &str,
    dst_artifacts_prefix: &str,
) -> Result<Vec<(i64, i64)>> {
    let rows: Vec<RawMessageRow> = {
        let mut stmt = conn.prepare_cached(
            "SELECT id, session_id, role, ts, data, meta FROM message \
             WHERE session_id = ?1 AND id <= ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![src_session, upto_message_id], row_to_raw)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut map: Vec<(i64, i64)> = Vec::with_capacity(rows.len());
    let remap = |map: &[(i64, i64)], old: i64| {
        map.binary_search_by_key(&old, |(src, _)| *src)
            .ok()
            .map(|pos| map[pos].1)
    };
    let mut last_ts = 0i64;
    for row in rows {
        let data = if !src_artifacts_prefix.is_empty() && row.data.contains(src_artifacts_prefix) {
            row.data.replace(src_artifacts_prefix, dst_artifacts_prefix)
        } else {
            row.data
        };
        let meta = match row.meta {
            Some(raw) => match CompactionMeta::parse(&raw) {
                Some(compaction) => {
                    let head_until = match compaction.head_until {
                        Some(old) => remap(&map, old).map(Some),
                        None => Some(None),
                    };
                    match (head_until, remap(&map, compaction.tail_from)) {
                        (Some(head_until), Some(tail_from)) => Some(
                            CompactionMeta {
                                head_until,
                                tail_from,
                            }
                            .to_json(),
                        ),
                        _ => None,
                    }
                }
                None => Some(raw),
            },
            None => None,
        };
        conn.execute(
            "INSERT INTO message(session_id, role, ts, data, meta) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![dst_session, row.role, row.ts, data, meta],
        )?;
        let new_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT OR REPLACE INTO tool_call_meta(session_id, message_id, tool_use_id, meta) \
             SELECT ?1, ?2, tool_use_id, meta FROM tool_call_meta \
             WHERE session_id = ?3 AND message_id = ?4",
            params![dst_session, new_id, src_session, row.id],
        )?;
        last_ts = last_ts.max(row.ts);
        map.push((row.id, new_id));
    }
    conn.execute(
        "UPDATE session SET updated_at = max(updated_at, ?2) WHERE id = ?1",
        params![dst_session, not_in_future(last_ts.saturating_mul(1000))],
    )?;
    Ok(map)
}

/// 写分叉血缘 kv（与 `fork_session` 同一形态）：导入一个带 `fork_of` 的线程、且父线程在
/// 本机时用。
pub fn set_fork_lineage(
    conn: &Connection,
    channel: &str,
    chat_id: &str,
    parent_chat_id: &str,
    anchor_message_id: i64,
) -> Result<()> {
    let lineage = serde_json::json!({
        "parent": parent_chat_id,
        "anchor": anchor_message_id,
    })
    .to_string();
    sync_db::kv_put(conn, &fork_lineage_key(channel, chat_id), &lineage)
}

/// 读分叉血缘：`(父会话 chat_id, 锚点消息 id)`。
pub fn fork_lineage(
    conn: &Connection,
    channel: &str,
    chat_id: &str,
) -> Result<Option<(String, i64)>> {
    let Some(raw) = sync_db::kv_get(conn, &fork_lineage_key(channel, chat_id))? else {
        return Ok(None);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Ok(None);
    };
    Ok(value
        .get("parent")
        .and_then(|v| v.as_str())
        .zip(value.get("anchor").and_then(|v| v.as_i64()))
        .map(|(parent, anchor)| (parent.to_string(), anchor)))
}

impl SessionStore {
    /// 自定义会话标题（没有则 `None`）。
    pub fn session_title(&self, channel: &str, chat_id: &str) -> Result<Option<String>> {
        let raw = self
            .db
            .with(|conn| sync_db::kv_get(conn, &session_title_key(channel, chat_id)))?;
        Ok(raw
            .and_then(|raw| serde_json::from_str::<String>(&raw).ok())
            .filter(|title| !title.is_empty()))
    }

    /// 设置自定义会话标题；空串 = 清除（界面回到按首条消息派生的标题）。
    pub fn set_session_title(&self, channel: &str, chat_id: &str, title: &str) -> Result<()> {
        let key = session_title_key(channel, chat_id);
        self.db.with(|conn| {
            if title.is_empty() {
                sync_db::kv_del(conn, &key)
            } else {
                sync_db::kv_put(
                    conn,
                    &key,
                    &serde_json::Value::String(title.to_string()).to_string(),
                )
            }
        })
    }

    /// 一个通道下全部自定义标题：`chat_id → 标题`（会话列表用，一次范围查询）。
    pub fn session_titles(&self, channel: &str) -> Result<HashMap<String, String>> {
        let prefix = session_title_key(channel, "");
        // 前缀范围：`<prefix>` ≤ key < `<prefix 末字符 +1>`（`/` 的下一个字符是 `0`）。
        let mut upper = prefix.clone();
        upper.pop();
        upper.push('0');
        self.db.with(|conn| {
            let mut stmt =
                conn.prepare_cached("SELECT key, value FROM kv WHERE key >= ?1 AND key < ?2")?;
            let rows = stmt.query_map(params![prefix, upper], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut out = HashMap::new();
            for row in rows {
                let (key, value) = row?;
                let Some(title) = serde_json::from_str::<String>(&value)
                    .ok()
                    .filter(|title| !title.is_empty())
                else {
                    continue;
                };
                out.insert(key[prefix.len()..].to_string(), title);
            }
            Ok(out)
        })
    }

    /// 删除会话，并指定同步侧的处理方式（[`SessionStore::delete_session`] 等价于
    /// `SessionDeleteSync::Propagate`）。没开同步、或该会话不参与同步时三种方式没有区别。
    ///
    /// 级联范围同 `delete_session`：消息 / checkpoint / 工具侧表随外键删除；usage 锚点、
    /// 会话目标、fork 血缘与标题 kv 一并删除；artifact 目录提交后清理。同步侧的队列清理
    /// 与删除登记在同一个事务里（§7.5）。返回会话是否存在。
    pub fn delete_session_with(
        &self,
        channel: &str,
        chat_id: &str,
        sync: SessionDeleteSync,
    ) -> Result<bool> {
        let existed = self.db.transaction(|tx| {
            tx.execute(
                "DELETE FROM usage_anchor WHERE channel = ?1 AND chat_id = ?2",
                params![channel, chat_id],
            )?;
            // 会话级 kv（/goal 目标、fork 血缘、标题）随会话删除：目标活不过它的会话；血缘
            // 与标题留着会让 chat_id 复用时「新会话被当成旧分叉 / 顶着旧标题」。
            tx.execute(
                "DELETE FROM kv WHERE key IN (?1, ?2, ?3, ?4)",
                params![
                    session_goal_key(channel, chat_id),
                    fork_lineage_key(channel, chat_id),
                    session_title_key(channel, chat_id),
                    session_model_key(channel, chat_id)
                ],
            )?;
            if let Some(sid) = session_id(tx, channel, chat_id)? {
                sync_db::on_session_delete(
                    tx,
                    sid,
                    sync != SessionDeleteSync::Local,
                    sync == SessionDeleteSync::Purge,
                )?;
            }
            let n = tx.execute(
                "DELETE FROM session WHERE channel = ?1 AND chat_id = ?2",
                params![channel, chat_id],
            )?;
            Ok(n > 0)
        })?;
        if existed {
            remove_artifact_dir(&self.layout, channel, chat_id);
        }
        Ok(existed)
    }

    /// 某会话的 artifact 目录（工具结果全文落盘处）。
    pub fn artifacts_dir(&self, channel: &str, chat_id: &str) -> PathBuf {
        self.layout
            .base
            .join("artifacts")
            .join(artifact_chat_key(channel, chat_id))
    }

    /// 会话里产生于**其他设备**的消息来源，按消息数降序。`gid` 的前缀就是产生这条消息的
    /// 设备；本机换过身份的话，旧身份也算本机。设备名与平台来自各设备的 `device.hello`。
    /// 没有同步表、或会话全部产生于本机时返回空。
    ///
    /// 这个进程从没见过同步表（[`sync_db::sync_tables_seen`]）时直接返回，不拿库锁、不查库：
    /// 每个 turn 开头都会问一次，没开过多端同步的用户不该为它付任何代价。见过的话要查几
    /// 次库（其中一次按这个会话的消息数走），调用方应当放到阻塞线程上做。
    pub fn remote_origins(&self, channel: &str, chat_id: &str) -> Result<Vec<MessageOrigin>> {
        if !sync_db::sync_tables_seen() {
            return Ok(Vec::new());
        }
        self.db.with(|conn| {
            if !sync_db::sync_tables_exist(conn)? {
                return Ok(Vec::new());
            }
            let Some(sid) = session_id(conn, channel, chat_id)? else {
                return Ok(Vec::new());
            };
            let own = sync_db::device_ids(conn)?;
            let mut stmt = conn.prepare_cached(
                "SELECT substr(gid, 1, instr(gid, '.') - 1) AS device, count(*) \
                 FROM sync_msg WHERE session_id = ?1 GROUP BY device ORDER BY count(*) DESC, device",
            )?;
            let rows = stmt.query_map(params![sid], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (device_id, messages) = row?;
                if device_id.is_empty() || own.contains(&device_id) {
                    continue;
                }
                let hello: Option<String> = conn
                    .query_row(
                        "SELECT value FROM sync_state WHERE scope = 'device' AND key = ?1",
                        params![device_id],
                        |row| row.get(0),
                    )
                    .optional()?
                    .flatten();
                let hello = hello
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                    .unwrap_or_default();
                let field = |name: &str| {
                    hello
                        .get(name)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string()
                };
                out.push(MessageOrigin {
                    name: field("name"),
                    platform: field("platform"),
                    device_id,
                    messages: messages.max(0) as u32,
                });
            }
            Ok(out)
        })
    }
}

/// `clear_all_sessions` 的同步侧清理：会话全清后，待发队列里引用会话行的 op 都成了悬空
/// 引用。不带 `session_id` 的 op（已登记的删除、属性等）保留，照常发出。
pub(super) fn on_clear_all_sessions(conn: &Connection) -> Result<()> {
    if sync_db::sync_tables_exist(conn)? {
        conn.execute("DELETE FROM sync_outbox WHERE session_id IS NOT NULL", [])?;
        // 以会话行 id 为键的登记随会话一起删（行 id 会被新会话重用）。
        conn.execute(
            "DELETE FROM sync_state WHERE scope IN ('branch', 'landed', 'pub-halt')",
            [],
        )?;
    }
    Ok(())
}

/// 当前 Unix 毫秒（给调用方在导入时补「现在」用，与库内时间同源）。
pub fn now_millis() -> i64 {
    now_ms()
}
