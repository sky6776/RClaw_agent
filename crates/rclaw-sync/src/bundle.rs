//! 会话包：一个会话（一条分支）的完整可移植导出（JSONL）。
//!
//! 第一行是包头，之后每行一条消息的线上形态（与 `msg` op 相同）。用于手动导入导出、
//! 回收站与分支重发（`session.bundle`）。不依赖同步是否开启：导出时给消息分配的全局身份
//! 只记在本机，等到这个会话参与同步时原样沿用。

use std::collections::HashSet;

use rclaw_store::session::raw;
use rclaw_store::session_title_key;
use rclaw_store::sync_db::{self, SEAL_LOCAL, SEAL_SEALED};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db;
use crate::error::{Result, SyncError};
use crate::ids;
use crate::map::{self, OpenInfo, SessionMap};
use crate::op::{ForkOf, WireMsg};
use crate::portable;
use crate::wire::{self, ArtifactPolicy, Env, Landing, LocalBlob, RowWire, BUNDLE_ARTIFACT_MAX};

pub const BUNDLE_VERSION: u32 = 1;

/// 会话包的包头。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleHeader {
    pub rclaw_bundle: u32,
    /// 线程身份。
    pub sid: String,
    pub channel: String,
    pub chat_id: String,
    /// 会话创建时间（Unix 毫秒）。
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub archived: bool,
    /// 导出时间（Unix 毫秒）。
    #[serde(default)]
    pub exported_at: i64,
    /// 导出它的设备。
    #[serde(default)]
    pub device: String,
    /// 创建这个线程的设备。
    #[serde(default)]
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_of: Option<ForkOf>,
    /// 这条分支的末条消息。分支重发拆成几个包时，每个包都带着它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tip: Option<String>,
}

/// 一条分支的导出内容（数据库阶段的产物）。
#[derive(Debug, Clone)]
pub struct BranchExport {
    pub header: BundleHeader,
    pub rows: Vec<RowWire>,
    /// 本机有、却表达不成线上形态的行数（不认识的 meta 暂停发布之后的那一段）。大于 0
    /// 说明这份导出**不是完整备份**：只到暂停处为止，后面的本地对话不在里面。
    pub skipped: usize,
}

/// 读出一条分支：先补全映射（只分配身份，不排队），再逐行取线上形态。
/// 在调用方的写事务里执行。
///
/// 没有映射的行（版本表达不了的那一段）不出现在 `rows` 里，只记进 `skipped`：线上形态
/// 有格式约束，那些行不能悄悄被当成完整内容——备份、回收站都要据此决定怎么办（[`Self`]
/// 的调用方，见 `export_session`）。
pub fn collect_branch(
    conn: &Connection,
    env: &Env,
    device: &str,
    session_id: i64,
) -> Result<BranchExport> {
    map::ensure_mapped(conn, device, session_id, false)?;
    let session =
        map::session_map(conn, session_id)?.ok_or_else(|| SyncError::other("会话不存在"))?;
    let header = branch_header(conn, device, &session)?;
    let maps = map::session_msg_maps(conn, session_id)?;
    let rows = raw::raw_messages(conn, session_id)?;
    let mut out = Vec::with_capacity(maps.len());
    let mut skipped = 0usize;
    // 两边都按消息行 id 升序；没有映射的行（因不认识的 meta 而暂停处之后的）不导出。
    let mut maps = maps.into_iter().peekable();
    for row in rows {
        match maps.peek() {
            Some(mapped) if mapped.message_id == row.id => {
                let mapped = maps.next().expect("刚看过");
                out.push(wire::row_to_wire(conn, env, &session, &row, &mapped)?);
            }
            _ => skipped += 1,
        }
    }
    Ok(BranchExport {
        header,
        rows: out,
        skipped,
    })
}

/// 一条分支的包头（数据库阶段）。`tip` 取会话当前的末条。
pub fn branch_header(
    conn: &Connection,
    device: &str,
    session: &SessionMap,
) -> Result<BundleHeader> {
    let (created_at, archived): (i64, bool) = conn.query_row(
        "SELECT created_at, archived_at IS NOT NULL FROM session WHERE id = ?1",
        params![session.session_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let title = sync_db::kv_get(conn, &session_title_key(&session.channel, &session.chat_id))?
        .and_then(|raw| serde_json::from_str::<String>(&raw).ok())
        .and_then(|title| portable::clean_title(&title));
    let fork_of = map::fork_of(conn, session, &session.channel, &session.chat_id)?;
    Ok(BundleHeader {
        rclaw_bundle: BUNDLE_VERSION,
        sid: session.sid.clone(),
        channel: session.channel.clone(),
        chat_id: session.chat_id.clone(),
        created_at,
        title,
        archived,
        exported_at: raw::now_millis(),
        device: device.to_string(),
        origin: session.origin.clone(),
        fork_of,
        tip: session.tip.clone(),
    })
}

/// 会话里最后一条「已封段或来自远端」的消息：`(消息行 id, gid)`。它之前的都已经在某台
/// 设备的日志里；它之后的是本机还没发布的尾巴（附件清单也还没提取）。
pub fn published_tip(conn: &Connection, session_id: i64) -> Result<Option<(i64, String)>> {
    Ok(conn
        .query_row(
            "SELECT message_id, gid FROM sync_msg WHERE session_id = ?1 AND sealed = ?2 \
             ORDER BY message_id DESC LIMIT 1",
            params![session_id, SEAL_SEALED],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// 分页读一条分支的线上形态：消息行 id 在 `(after, upto]` 里的前 `limit` 行（正文合计约
/// `bytes` 字节，至少一行）。碰到还没有身份的行就停（它后面的也都没有）。读完了返回
/// 空表。只读，不做文件 IO。
pub fn branch_page(
    conn: &Connection,
    env: &Env,
    session: &SessionMap,
    after: i64,
    upto: i64,
    limit: usize,
    bytes: usize,
) -> Result<Vec<RowWire>> {
    let rows = raw::raw_messages_range(conn, session.session_id, after, upto, limit, bytes)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(mapped) = map::msg_map(conn, row.id)? else {
            break;
        };
        out.push(wire::row_to_wire(conn, env, session, &row, &mapped)?);
    }
    Ok(out)
}

/// 读出一条分支已经发布的部分（到 [`published_tip`] 为止），包头的 `tip` 也取它。分支
/// 重发与全量快照用：没发布的尾巴由日志照常送达，不抢在前面。一条都没发布过返回
/// `None`。只读。
pub fn collect_published(
    conn: &Connection,
    env: &Env,
    device: &str,
    session_id: i64,
) -> Result<Option<BranchExport>> {
    let Some(session) = map::session_map(conn, session_id)? else {
        return Ok(None);
    };
    let Some((upto, tip)) = published_tip(conn, session_id)? else {
        return Ok(None);
    };
    let mut header = branch_header(conn, device, &session)?;
    header.tip = Some(tip);
    let rows = branch_page(conn, env, &session, 0, upto, usize::MAX, usize::MAX)?;
    Ok(Some(BranchExport {
        header,
        rows,
        skipped: 0,
    }))
}

/// 把导出内容写成若干个会话包文本，每个不超过约 `chunk_bytes`（至少一条消息）。
/// artifact 全文在这里读进来（文件 IO）。返回要登记的附件对象。
pub fn render_chunks(
    env: &Env,
    export: BranchExport,
    policy: ArtifactPolicy<'_>,
    chunk_bytes: usize,
) -> Result<(Vec<String>, Vec<LocalBlob>)> {
    let header = serde_json::to_string(&export.header)?;
    let mut chunks = Vec::new();
    let mut blobs = Vec::new();
    let mut current = String::new();
    for mut row in export.rows {
        blobs.extend(wire::fill_artifacts(env, &mut row, policy));
        let line = serde_json::to_string(&row.msg)?;
        if !current.is_empty() && current.len() + line.len() > chunk_bytes {
            chunks.push(std::mem::take(&mut current));
        }
        if current.is_empty() {
            current.push_str(&header);
            current.push('\n');
        }
        current.push_str(&line);
        current.push('\n');
    }
    if current.is_empty() {
        current.push_str(&header);
        current.push('\n');
    }
    chunks.push(current);
    Ok((chunks, blobs))
}

/// 导出一个会话为会话包文本（手动导出、回收站）。全文都带上，不受同步的内联上限限制。
///
/// 会话里有本机版本表达不出来的行（不认识的 meta，发布停在那一段）时**明确失败**：线上
/// 形态装不下它们，导出的只能是个前缀。悄悄给出一个前缀的话，用户以为拿到了完整备份，
/// 拿它恢复、或者拿它当删除之前唯一的副本，后一段对话就无声无息地没了。
pub fn export_session(env: &Env, channel: &str, chat_id: &str) -> Result<String> {
    let export = db::write(env.db(), |tx| {
        let device = db::ensure_device(tx)?;
        let session_id = raw::find_session(tx, channel, chat_id)?
            .ok_or_else(|| SyncError::invalid(format!("会话 {channel}/{chat_id} 不存在")))?;
        collect_branch(tx, env, &device, session_id)
    })?;
    if export.skipped > 0 {
        return Err(SyncError::invalid(format!(
            "这个会话里有 {} 条消息用了本版本还不认识的内容格式，导出的会话包只会是前半段。\
             为了不让你把它当成完整备份，导出已经拒绝。要备份全部内容，请升级这台设备的 RClaw \
             之后重试；只想要前半段的话，可以先把这几条消息删掉再导出。",
            export.skipped
        )));
    }
    let (mut chunks, _) = render_chunks(env, export, ArtifactPolicy::BUNDLE, usize::MAX)?;
    Ok(chunks.pop().unwrap_or_default())
}

/// 解析并检查会话包。每条消息都按远端内容的同一套规则检查（§11.3）；消息必须连成一条
/// 链。`artifact_inline_max` 是内联全文的上限。
pub fn parse_bundle(
    text: &str,
    artifact_inline_max: usize,
) -> Result<(BundleHeader, Vec<WireMsg>)> {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let header_line = lines
        .next()
        .ok_or_else(|| SyncError::invalid("会话包是空的"))?;
    let probe: serde_json::Value =
        serde_json::from_str(header_line).map_err(|_| SyncError::invalid("这不是 RClaw 会话包"))?;
    let version = probe
        .get("rclaw_bundle")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| SyncError::invalid("这不是 RClaw 会话包"))?;
    if version > u64::from(BUNDLE_VERSION) {
        return Err(SyncError::Upgrade(format!(
            "会话包的格式版本是 {version}，本版本只认识到 {BUNDLE_VERSION}，请先升级"
        )));
    }
    let header: BundleHeader = serde_json::from_value(probe)
        .map_err(|e| SyncError::invalid(format!("会话包的包头不合规: {e}")))?;
    if !ids::is_sid(&header.sid)
        || header.channel.len() > 128
        || header.chat_id.len() > 128
        || header.tip.as_deref().is_some_and(|tip| !ids::is_gid(tip))
        || header
            .fork_of
            .as_ref()
            .is_some_and(|fork| !ids::is_sid(&fork.sid) || !ids::is_gid(&fork.gid))
    {
        return Err(SyncError::invalid("会话包的包头不合规"));
    }
    let mut messages: Vec<WireMsg> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (index, line) in lines.enumerate() {
        let msg: WireMsg = serde_json::from_str(line)
            .map_err(|e| SyncError::invalid(format!("会话包第 {} 条消息不合规: {e}", index + 1)))?;
        portable::check_wire_msg_with(&msg, artifact_inline_max)?;
        if let Some(previous) = messages.last() {
            if msg.parent.as_deref() != Some(previous.gid.as_str()) {
                return Err(SyncError::invalid(format!(
                    "会话包第 {} 条消息没有接在上一条后面",
                    index + 1
                )));
            }
        }
        if !seen.insert(msg.gid.clone()) {
            return Err(SyncError::invalid("会话包里有重复的消息"));
        }
        messages.push(msg);
    }
    Ok((header, messages))
}

#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// 导入到哪个通道；空 = 包里记的通道。
    pub channel: Option<String>,
    /// 导入成哪个会话键（被占用时自动加后缀）；空 = 包里记的会话键。宿主用它避开自己
    /// 有特殊含义的键——桌面端的 `bot-…`（bot 收件箱）、`grp-…`（群成员会话）。
    pub chat_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    pub session_id: i64,
    pub channel: String,
    pub chat_id: String,
    pub sid: String,
    pub messages: usize,
}

/// 这个会话键在本机不能给新落地的会话（别的设备的线程、导入的会话包、自动分支）用：
/// 已经有会话占着，或者用户把这个键标成了「仅本机」。
///
/// 「仅本机」的标记跟着键走，会话删掉之后也留着（同一个键下再出现的会话仍然是私有的）。
/// 别的设备的线程要是落在这样一个键上，本机就多出一个「带着仅本机标记、却对应着别的
/// 设备上那个线程」的会话：界面上说它只在本机，它在本机续写的内容确实不会发出去，可是
/// 别的设备的内容照样合进来，在本机删掉它还会把别的设备上的那一份一起删掉。所以带标记
/// 的键当成被占用，落到别的键上。
pub fn key_taken(conn: &Connection, channel: &str, chat_id: &str) -> Result<bool> {
    Ok(raw::find_session(conn, channel, chat_id)?.is_some()
        || sync_db::kv_get(conn, &db::local_only_key(channel, chat_id))?.is_some())
}

/// 挑一个没被占用的会话键：`base`，被占用时是 `base-<后缀>`，再不行在后缀后面加数字。
/// 结果满足 chat_id 白名单（`[A-Za-z0-9_-]{1,64}`），超长时截短基名。「被占用」见
/// [`key_taken`]。
pub fn free_chat_id(conn: &Connection, channel: &str, base: &str, suffix: &str) -> Result<String> {
    let base: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .take(64)
        .collect();
    let base = if base.is_empty() {
        "chat".to_string()
    } else {
        base
    };
    if suffix.is_empty() && !key_taken(conn, channel, &base)? {
        return Ok(base);
    }
    for attempt in 0..10_000u32 {
        let tail = match (suffix.is_empty(), attempt) {
            (false, 0) => format!("-{suffix}"),
            (false, n) => format!("-{suffix}{}", n + 1),
            (true, _) => format!("-{}", &ids::random_hex::<2>()),
        };
        let keep = 64usize.saturating_sub(tail.len());
        let candidate = format!("{}{tail}", &base[..base.len().min(keep)]);
        if !key_taken(conn, channel, &candidate)? {
            return Ok(candidate);
        }
    }
    Err(SyncError::other("找不到可用的会话键"))
}

/// 导入一个完整的会话包，总是新建一个本地会话（从不覆盖已有会话）。
///
/// 线程身份：导入的会话一律是**新线程**（新的 `sid`，`gid` 不变），不沿用包里的。导入得到
/// 的是一份独立的副本——沿用 `sid` 的话，它一参与同步就和原件成了同一个线程：在这边删掉
/// 副本、把它标成「仅本机」，原件所在的设备上原件也跟着被删，哪怕原件是「仅本机」的、
/// 从来没有发布过。代价是原件和副本以后都参与同步时，各台设备上会有两个内容相同的会话。
pub fn import_session(env: &Env, text: &str, options: &ImportOptions) -> Result<Imported> {
    let (header, messages) = parse_bundle(text, BUNDLE_ARTIFACT_MAX)?;
    if messages.first().is_some_and(|first| first.parent.is_some()) {
        return Err(SyncError::invalid("会话包不完整：缺少开头的消息"));
    }
    let channel = options
        .channel
        .clone()
        .unwrap_or_else(|| header.channel.clone());
    if !ids::is_valid_channel(&channel) {
        return Err(SyncError::invalid(format!("通道 {channel} 不合规")));
    }
    // 先定下会话键、把 artifact 全文写好，再开事务：事务里不做磁盘写入（库锁是会话读写
    // 共用的），提交之后消息里的句柄也一定指向存在的文件。
    let base = options.chat_id.as_deref().unwrap_or(&header.chat_id);
    let chat_id = db::read(env.db(), |conn| free_chat_id(conn, &channel, base, ""))?;
    for msg in &messages {
        wire::write_artifacts(env, &channel, &chat_id, msg)?;
    }
    let imported = db::write(env.db(), |tx| {
        let device = db::ensure_device(tx)?;
        if key_taken(tx, &channel, &chat_id)? {
            return Err(SyncError::other("会话键刚刚被占用，请再试一次"));
        }
        let sid = ids::new_sid();
        let origin = if ids::is_device_id(&header.origin) {
            header.origin.clone()
        } else {
            device.clone()
        };
        let created_at = header.created_at.clamp(0, raw::now_millis());
        let session_id = raw::create_session(tx, &channel, &chat_id, created_at)?;
        map::insert_session_map(tx, session_id, &sid, &origin, SEAL_LOCAL)?;
        map::put_local_open_info(
            tx,
            &sid,
            &OpenInfo {
                channel: channel.clone(),
                chat_id: chat_id.clone(),
                created_at,
                fork_of: header.fork_of.clone(),
                origin,
            },
        )?;
        let landing = Landing {
            session_id,
            channel: &channel,
            chat_id: &chat_id,
            sealed: SEAL_LOCAL,
        };
        for msg in &messages {
            wire::land_msg(tx, env, landing, msg)?;
        }
        if let Some(title) = header.title.as_deref().and_then(portable::clean_title) {
            sync_db::kv_put(
                tx,
                &session_title_key(&channel, &chat_id),
                &serde_json::Value::String(title).to_string(),
            )?;
        }
        apply_fork_lineage(tx, &channel, &chat_id, header.fork_of.as_ref())?;
        Ok(Imported {
            session_id,
            channel: channel.clone(),
            chat_id: chat_id.clone(),
            sid,
            messages: messages.len(),
        })
    });
    match imported {
        Ok(imported) => {
            // 包里引用的附件本机已经有的，复制到这个会话自己的位置。
            if let Err(e) = wire::settle_aliases(env, None, None) {
                tracing::warn!("导入会话包后补附件失败: {e}");
            }
            Ok(imported)
        }
        Err(e) => {
            // 没导入成：刚写的 artifact 目录不属于任何会话，清掉（键被别的会话占了就不动）。
            let taken = db::read(env.db(), |conn| {
                Ok(raw::find_session(conn, &channel, &chat_id)?.is_some())
            })
            .unwrap_or(true);
            if !taken {
                let _ = std::fs::remove_dir_all(env.sessions.artifacts_dir(&channel, &chat_id));
            }
            Err(e)
        }
    }
}

/// 分叉来源的父线程在本机（同一通道）时，写下本机形态的分叉血缘。
pub fn apply_fork_lineage(
    conn: &Connection,
    channel: &str,
    chat_id: &str,
    fork_of: Option<&ForkOf>,
) -> Result<()> {
    let Some(fork_of) = fork_of else {
        return Ok(());
    };
    for (parent_session, anchor) in map::find_gid(conn, &fork_of.sid, &fork_of.gid)? {
        if let Some((parent_channel, parent_chat)) = raw::session_key(conn, parent_session)? {
            if parent_channel == channel && parent_chat != chat_id {
                raw::set_fork_lineage(conn, channel, chat_id, &parent_chat, anchor)?;
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rclaw_core::{ContentBlock, Message, Role};
    use rclaw_store::{Layout, SessionStore, ToolCallMetaRecord};

    fn store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(Layout::new(dir.path().join("agent"))).unwrap();
        (dir, store)
    }

    fn env(store: &SessionStore) -> Env {
        Env::new(store, store.layout().base.parent().unwrap())
    }

    /// 一个有代表性的会话：普通消息、工具链（带元数据与落盘的全文）、compaction boundary。
    fn seed(store: &SessionStore, chat_id: &str) {
        let prefix = store.artifacts_dir_prefix("desktop", chat_id);
        let m1 = store
            .append(
                "desktop",
                chat_id,
                &Message::plain(Role::User, "列一下目录"),
            )
            .unwrap();
        let asst = Message {
            id: 0,
            role: Role::Assistant,
            content: vec![
                ContentBlock::reasoning("先看看"),
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "shell".into(),
                    input: serde_json::json!({ "command": "ls" }),
                },
            ],
            tool_call_id: None,
            ts: 0,
        };
        let result = Message {
            id: 0,
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: format!("预览…\n完整内容已存至: {prefix}7-shell.txt"),
                is_error: false,
            }],
            tool_call_id: None,
            ts: 0,
        };
        let meta = ToolCallMetaRecord {
            tool_use_id: "call_1".into(),
            duration_ms: Some(12),
            ..ToolCallMetaRecord::default()
        };
        store
            .append_tool_chain_full("desktop", chat_id, &asst, &result, 0, &[], &[meta])
            .unwrap();
        let dir = store.artifacts_dir("desktop", chat_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("7-shell.txt"), "a.txt\nb.txt\n").unwrap();
        let m4 = store
            .append(
                "desktop",
                chat_id,
                &Message::plain(Role::Assistant, "有两个文件"),
            )
            .unwrap();
        store
            .append_compaction(
                "desktop",
                chat_id,
                &Message::plain(Role::User, "【摘要】列过目录"),
                Some(m1),
                m4,
            )
            .unwrap();
        store
            .append("desktop", chat_id, &Message::plain(Role::User, "好的"))
            .unwrap();
        store.set_session_title("desktop", chat_id, "目录").unwrap();
    }

    fn message_lines(bundle: &str) -> Vec<&str> {
        bundle.lines().skip(1).collect()
    }

    #[test]
    fn round_trip_keeps_the_portable_transcript_byte_identical() {
        let (_ga, a) = store();
        let (_gb, b) = store();
        seed(&a, "c-1");
        let exported = export_session(&env(&a), "desktop", "c-1").unwrap();
        // 包里没有本机路径。
        let base = a.layout().base.to_string_lossy().replace('\\', "/");
        assert!(!exported.contains(&base), "{exported}");
        assert!(exported.contains("<<RCLAW_ARTIFACTS>>/7-shell.txt"));

        let imported = import_session(&env(&b), &exported, &ImportOptions::default()).unwrap();
        assert_eq!(imported.chat_id, "c-1");
        assert_eq!(imported.messages, 6);
        // 另一台设备再导出：消息行逐字节相同；线程身份是导入时新分配的（副本与原件互不
        // 相干，删掉一个不会带走另一个）。
        let again = export_session(&env(&b), "desktop", "c-1").unwrap();
        assert_eq!(message_lines(&again), message_lines(&exported));
        let (header_a, _) = parse_bundle(&exported, BUNDLE_ARTIFACT_MAX).unwrap();
        let (header_b, _) = parse_bundle(&again, BUNDLE_ARTIFACT_MAX).unwrap();
        assert_ne!(header_a.sid, header_b.sid);
        assert_eq!(imported.sid, header_b.sid);
        assert_eq!(header_b.title.as_deref(), Some("目录"));

        // 落库的是接收端自己的路径，全文也在接收端自己的 artifact 目录里。
        let all = b.load_transcript("desktop", "c-1").unwrap();
        let prefix_b = b.artifacts_dir_prefix("desktop", "c-1");
        let carried = all.iter().any(|m| {
            m.content.iter().any(|block| match block {
                ContentBlock::ToolResult { content, .. } => {
                    content.contains(&format!("{prefix_b}7-shell.txt"))
                }
                _ => false,
            })
        });
        assert!(carried);
        assert_eq!(
            std::fs::read_to_string(b.artifacts_dir("desktop", "c-1").join("7-shell.txt")).unwrap(),
            "a.txt\nb.txt\n"
        );
        // 活动视图一致：boundary 的锚点换成了接收端的行 id，仍然成立。
        let view_a: Vec<String> = a
            .load_active("desktop", "c-1")
            .unwrap()
            .iter()
            .map(|m| {
                serde_json::to_string(&m.content)
                    .unwrap()
                    .replace(&a.artifacts_dir_prefix("desktop", "c-1"), "")
            })
            .collect();
        let view_b: Vec<String> = b
            .load_active("desktop", "c-1")
            .unwrap()
            .iter()
            .map(|m| {
                serde_json::to_string(&m.content)
                    .unwrap()
                    .replace(&prefix_b, "")
            })
            .collect();
        assert_eq!(view_a, view_b);
        assert_eq!(
            view_b.len(),
            4,
            "头部 1 条 + boundary + 尾窗 1 条 + 之后 1 条"
        );
        // 工具元数据随行。
        let metas = db::read(b.db(), |c| {
            let session = raw::find_session(c, "desktop", "c-1")?.unwrap();
            let rows = raw::raw_messages(c, session)?;
            Ok(raw::tool_call_metas(c, session, rows[1].id)?)
        })
        .unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].duration_ms, Some(12));
    }

    #[test]
    fn imported_session_is_ordered_by_its_last_message_time() {
        let (_ga, a) = store();
        let (_gb, b) = store();
        // 接收端先有一个新会话。
        b.append("desktop", "recent", &Message::plain(Role::User, "现在"))
            .unwrap();
        // 来源会话是很久以前建的，最后一条消息比创建晚一小时。
        let mut old = Message::plain(Role::User, "很久以前");
        old.ts = 1_600_003_600;
        a.append("desktop", "old", &old).unwrap();
        db::write(a.db(), |tx| {
            tx.execute(
                "UPDATE session SET created_at = 1600000000000, updated_at = 1600003600000",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let exported = export_session(&env(&a), "desktop", "old").unwrap();
        import_session(&env(&b), &exported, &ImportOptions::default()).unwrap();
        let list = b.list_sessions(Some("desktop")).unwrap();
        assert_eq!(list[0].chat_id, "recent", "导入的旧会话不该排到最前");
        let imported = list.iter().find(|s| s.chat_id == "old").unwrap();
        assert_eq!(imported.created_at, 1_600_000_000_000);
        assert_eq!(
            imported.updated_at, 1_600_003_600_000,
            "取最后一条消息的时间，不是导入的时间"
        );
    }

    #[test]
    fn import_never_overwrites_and_reimport_becomes_a_new_thread() {
        let (_ga, a) = store();
        seed(&a, "c-1");
        let exported = export_session(&env(&a), "desktop", "c-1").unwrap();
        // 导回同一台设备：键被占用 → 新键；导入的总是新线程，原会话不动。
        let before = a.load_all("desktop", "c-1").unwrap();
        let imported = import_session(&env(&a), &exported, &ImportOptions::default()).unwrap();
        assert_ne!(imported.chat_id, "c-1");
        assert!(imported.chat_id.starts_with("c-1-"));
        assert_eq!(a.load_all("desktop", "c-1").unwrap(), before);
        let (original, copy) = db::read(a.db(), |c| {
            Ok((
                map::session_map_by_key(c, "desktop", "c-1")?.unwrap(),
                map::session_map_by_key(c, "desktop", &imported.chat_id)?.unwrap(),
            ))
        })
        .unwrap();
        assert_ne!(original.sid, copy.sid);
        assert_eq!(original.tip, copy.tip, "gid 不变");
    }

    #[test]
    fn rejects_broken_bundles() {
        let (_ga, a) = store();
        let (_gb, b) = store();
        seed(&a, "c-1");
        let exported = export_session(&env(&a), "desktop", "c-1").unwrap();
        let lines: Vec<&str> = exported.lines().collect();

        // 缺开头。
        let headless = [lines[0], lines[2], lines[3]].join("\n");
        assert!(import_session(&env(&b), &headless, &ImportOptions::default()).is_err());
        // 链断了。
        let gap = [lines[0], lines[1], lines[3]].join("\n");
        assert!(import_session(&env(&b), &gap, &ImportOptions::default()).is_err());
        // 不是会话包。
        assert!(import_session(&env(&b), "{\"hello\":1}\n", &ImportOptions::default()).is_err());
        assert!(import_session(&env(&b), "", &ImportOptions::default()).is_err());
        // 更高的格式版本要求升级。
        let future = exported.replacen("\"rclaw_bundle\":1", "\"rclaw_bundle\":2", 1);
        assert!(matches!(
            import_session(&env(&b), &future, &ImportOptions::default()),
            Err(SyncError::Upgrade(_))
        ));
        // 消息内容被改成不合规的形态。
        let tampered = exported.replace(
            "\\\"type\\\":\\\"reasoning\\\"",
            "\\\"type\\\":\\\"exec\\\"",
        );
        assert_ne!(tampered, exported);
        assert!(import_session(&env(&b), &tampered, &ImportOptions::default()).is_err());
        // 失败的导入不留下任何会话。
        assert!(b.list_sessions(None).unwrap().is_empty());
    }

    #[test]
    fn free_chat_id_respects_the_whitelist_and_length() {
        let (_g, store) = store();
        store
            .append("desktop", "main", &Message::plain(Role::User, "x"))
            .unwrap();
        db::read(store.db(), |c| {
            assert_eq!(free_chat_id(c, "desktop", "other", "")?, "other");
            assert_eq!(free_chat_id(c, "desktop", "main", "ab12")?, "main-ab12");
            let random = free_chat_id(c, "desktop", "main", "")?;
            assert!(random.starts_with("main-") && random.len() == 9, "{random}");
            let long = "x".repeat(80);
            let landed = free_chat_id(c, "desktop", &long, "ab12")?;
            assert_eq!(landed.len(), 64);
            assert!(landed.ends_with("-ab12"));
            assert!(ids::is_valid_chat_id(&landed));
            assert_eq!(free_chat_id(c, "desktop", "a/b c", "")?, "abc");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn chunks_share_the_header_and_cover_every_message() {
        let (_ga, a) = store();
        seed(&a, "c-1");
        let env = env(&a);
        let export = db::write(env.db(), |tx| {
            let device = db::ensure_device(tx)?;
            let session = raw::find_session(tx, "desktop", "c-1")?.unwrap();
            collect_branch(tx, &env, &device, session)
        })
        .unwrap();
        let tip = export.header.tip.clone();
        let (chunks, _) = render_chunks(&env, export, ArtifactPolicy::BUNDLE, 1).unwrap();
        assert_eq!(chunks.len(), 6);
        let mut total = 0;
        for chunk in &chunks {
            let (header, messages) = parse_bundle(chunk, BUNDLE_ARTIFACT_MAX).unwrap();
            assert_eq!(header.tip, tip);
            total += messages.len();
        }
        assert_eq!(total, 6);
    }
}
