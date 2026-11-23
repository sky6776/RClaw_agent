//! 封段（§9.3 第 2 步）：把待发队列里的一批 op 分配日志序号，序列化、压缩、加密成一个
//! 段文件，写到 `<数据根>/sync/outgoing/`，然后在一个事务里登记这个段、把这批 op 移出
//! 队列、把涉及的消息标成「已封段」。这个提交在上传之前落盘（第 3 步，见
//! [`crate::upload`]）。
//!
//! 封段之后这些 op 不再依赖本机的消息行，会话被删也照样能传出去。任何时刻最多只有一个
//! 「已封未传」的段。

use std::collections::{BTreeMap, HashSet};

use rclaw_store::session::raw;
use rclaw_store::sync_db::SEAL_SEALED;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::attach;
use crate::codec::{self, HeadSeg, SegRef, SegmentHeader};
use crate::crypto::{sha256_hex, Keys};
use crate::db::{self, scope, BLOB_HELD, BLOB_PENDING};
use crate::error::Result;
use crate::ids;
use crate::map;
use crate::op::{MsgOp, Op, ARTIFACT_INLINE_MAX, FORMAT_VERSION};
use crate::wire::{self, ArtifactPolicy, LocalBlob, RowWire, StoredRefs, OP_ARTIFACTS_MAX};
use crate::Ctx;

/// 本机已封的一个段（`sync_state` 的 `seg`，键是 12 位补零的止序号）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegEntry {
    pub name: String,
    pub digest: String,
    pub from: u64,
    pub to: u64,
    /// 已上传（head 也已写出）。
    #[serde(default)]
    pub up: bool,
    /// 已打进汇总包。
    #[serde(default)]
    pub packed: bool,
    /// 它在存储上的零散对象已经排进回收队列（内容还在汇总包里）。
    #[serde(default)]
    pub gone: bool,
}

impl SegEntry {
    pub fn head_seg(&self) -> HeadSeg {
        HeadSeg {
            name: self.name.clone(),
            digest: self.digest.clone(),
            from: self.from,
            to: self.to,
        }
    }

    pub fn seg_ref(&self) -> SegRef {
        SegRef {
            name: self.name.clone(),
            digest: self.digest.clone(),
        }
    }
}

fn seg_key(to: u64) -> String {
    format!("{to:012}")
}

pub fn seg_entries(conn: &Connection) -> Result<Vec<SegEntry>> {
    Ok(db::state_scan(conn, scope::SEG, "")?
        .into_iter()
        .filter_map(|(_, row)| row.json())
        .collect())
}

pub fn put_seg_entry(conn: &Connection, entry: &SegEntry) -> Result<()> {
    db::state_put_json(conn, scope::SEG, &seg_key(entry.to), db::HLC_ZERO, entry)
}

pub fn del_seg_entry(conn: &Connection, to: u64) -> Result<()> {
    db::state_del(conn, scope::SEG, &seg_key(to))
}

pub struct SealOptions<'a> {
    pub keys: &'a Keys,
    pub device: &'a str,
    /// 一个段的明文预算（字节）。至少装一条 op。
    pub budget: usize,
    /// 被引用的工作区文件大小上限；超过的不登记。
    pub attachment_max: u64,
    /// 附件同步是否开启：开启时新登记的附件对象直接排队上传。
    pub upload_attachments: bool,
}

/// 队列里的一条 op，已读出但还没有序号。
struct Pending {
    outbox_id: i64,
    hlc: String,
    body: Body,
}

enum Body {
    /// 完整的 op（JSON 对象，不含 `hlc`）。`open_session` 是 `session.open` 所属的本地会话。
    Full {
        value: serde_json::Value,
        open_session: Option<i64>,
    },
    /// 消息行引用，已取出线上形态。
    Row { sid: String, row: Box<RowWire> },
    /// 引用的行已经不在了：这条 op 直接丢弃。
    Gone,
}

/// 准备好写进段里的一行。
struct Ready {
    outbox_id: i64,
    line: Option<String>,
    open_session: Option<i64>,
    /// 消息行：`(消息行 id, 新提取的附件清单, 要登记的附件对象)`。
    row: Option<(i64, Option<String>, Vec<LocalBlob>)>,
}

const BATCH_ROWS: usize = 2000;

fn read_pending(conn: &Connection, ctx: &Ctx, budget: usize) -> Result<Vec<Pending>> {
    let mut stmt = conn
        .prepare_cached("SELECT id, session_id, hlc, op FROM sync_outbox ORDER BY id LIMIT ?1")?;
    let mut rows = stmt.query(params![BATCH_ROWS as i64])?;
    let mut out = Vec::new();
    let mut size = 0usize;
    while out.is_empty() || size <= budget {
        let Some(row) = rows.next()? else {
            break;
        };
        let (outbox_id, session_id, hlc): (i64, Option<i64>, String) =
            (row.get(0)?, row.get(1)?, row.get(2)?);
        let op: String = row.get(3)?;
        size += op.len();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&op) else {
            out.push(Pending {
                outbox_id,
                hlc,
                body: Body::Gone,
            });
            continue;
        };
        let kind = value.get("t").and_then(|v| v.as_str()).unwrap_or_default();
        let body = match (kind, value.get("ref").and_then(|v| v.as_i64())) {
            ("msg", Some(message_id)) => match load_row(conn, ctx, message_id)? {
                Some((sid, row)) => {
                    size += row.msg.data.len();
                    Body::Row {
                        sid,
                        row: Box::new(row),
                    }
                }
                None => Body::Gone,
            },
            ("session.open", _) => Body::Full {
                value,
                open_session: session_id,
            },
            _ => Body::Full {
                value,
                open_session: None,
            },
        };
        out.push(Pending {
            outbox_id,
            hlc,
            body,
        });
    }
    Ok(out)
}

fn load_row(conn: &Connection, ctx: &Ctx, message_id: i64) -> Result<Option<(String, RowWire)>> {
    let Some(row) = raw::raw_message(conn, message_id)? else {
        return Ok(None);
    };
    let (Some(mapped), Some(session)) = (
        map::msg_map(conn, message_id)?,
        map::session_map(conn, row.session_id)?,
    ) else {
        return Ok(None);
    };
    let wire = wire::row_to_wire(conn, &ctx.env, &session, &row, &mapped)?;
    Ok(Some((session.sid, wire)))
}

/// 文件阶段：读 artifact 全文、提取并登记被引用的工作区文件，生成段里的行。超出预算的
/// 留在队列里等下一个段。
fn prepare(ctx: &Ctx, opts: &SealOptions<'_>, pending: Vec<Pending>) -> Result<Vec<Ready>> {
    let policy = ArtifactPolicy {
        inline_max: ARTIFACT_INLINE_MAX,
        message_max: OP_ARTIFACTS_MAX,
        keys: Some(opts.keys),
    };
    let mut out = Vec::with_capacity(pending.len());
    let mut size = 0usize;
    for item in pending {
        if !out.is_empty() && size > opts.budget {
            break;
        }
        let ready = match item.body {
            Body::Gone => Ready {
                outbox_id: item.outbox_id,
                line: None,
                open_session: None,
                row: None,
            },
            Body::Full {
                mut value,
                open_session,
            } => {
                if let Some(object) = value.as_object_mut() {
                    object.insert("hlc".into(), serde_json::Value::String(item.hlc.clone()));
                }
                let line = value.is_object().then(|| value.to_string());
                Ready {
                    outbox_id: item.outbox_id,
                    line,
                    open_session,
                    row: None,
                }
            }
            Body::Row { sid, mut row } => {
                let mut blobs = wire::fill_artifacts(&ctx.env, &mut row, policy);
                let mut new_refs = None;
                if row.stored.is_none() {
                    let found = attach::extract_refs(
                        &ctx.env,
                        opts.keys,
                        &row.msg.data,
                        opts.attachment_max,
                    );
                    let mut stored = StoredRefs::default();
                    for (file, blob) in found {
                        stored.files.push(file);
                        blobs.push(blob);
                    }
                    row.msg.refs = stored.files.clone();
                    new_refs = Some(stored.to_column());
                }
                let message_id = row.message_id;
                let line = Op::Msg(MsgOp { sid, msg: row.msg }).to_line(&item.hlc)?;
                Ready {
                    outbox_id: item.outbox_id,
                    line: Some(line),
                    open_session: None,
                    row: Some((message_id, new_refs, blobs)),
                }
            }
        };
        size += ready.line.as_ref().map_or(0, String::len);
        out.push(ready);
    }
    Ok(out)
}

/// 封段的一次尝试。
enum Attempt {
    Sealed(HeadSeg),
    /// 没有可封的：队列是空的，或上一个段还没传完。
    Idle,
    /// 准备期间队列变了（会话被删、只剩作废的 op）：这次不算，重新读队列。
    Retry,
}

/// 队列里还在的那些（`ready` 的 id 范围内）。
fn alive_ids(conn: &Connection, first: i64, last: i64) -> Result<HashSet<i64>> {
    let mut stmt = conn.prepare_cached("SELECT id FROM sync_outbox WHERE id >= ?1 AND id <= ?2")?;
    let rows = stmt.query_map(params![first, last], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<HashSet<i64>>>()?)
}

/// 封下一个段。上一个段还没传完、或队列是空的，返回 `None`。
pub fn seal_next(ctx: &Ctx, opts: &SealOptions<'_>) -> Result<Option<HeadSeg>> {
    // 准备期间队列被改动的情况很少见（用户正好在删会话）；重试几次仍不成就留给下一轮。
    for _ in 0..4 {
        match try_seal(ctx, opts)? {
            Attempt::Sealed(seg) => return Ok(Some(seg)),
            Attempt::Idle => return Ok(None),
            Attempt::Retry => continue,
        }
    }
    Ok(None)
}

/// 库锁只在三处短暂持有：读队列、定序号、提交登记。读 artifact 全文、压缩、加密、把段
/// 文件写到本机（fsync）都在库锁之外——库锁是会话读写共用的。
fn try_seal(ctx: &Ctx, opts: &SealOptions<'_>) -> Result<Attempt> {
    let pending = db::read(ctx.db(), |conn| {
        if db::log_state(conn)?.pending.is_some() {
            return Ok(Vec::new());
        }
        read_pending(conn, ctx, opts.budget)
    })?;
    if pending.is_empty() {
        return Ok(Attempt::Idle);
    }
    let ready = prepare(ctx, opts, pending)?;
    let (Some(first), Some(last)) = (
        ready.first().map(|item| item.outbox_id),
        ready.last().map(|item| item.outbox_id),
    ) else {
        return Ok(Attempt::Idle);
    };
    // 定下序号与依赖。文件阶段期间会话可能被删，它在队列里的 op 随之清掉了：只封还在
    // 队列里的。
    let (log, alive, deps) = db::read(ctx.db(), |conn| {
        let deps: BTreeMap<String, u64> = db::peers(conn)?
            .into_iter()
            .filter(|(_, peer)| peer.processed > 0)
            .map(|(device, peer)| (device, peer.processed))
            .collect();
        Ok((db::log_state(conn)?, alive_ids(conn, first, last)?, deps))
    })?;
    if log.pending.is_some() {
        return Ok(Attempt::Idle);
    }
    let ready: Vec<&Ready> = ready
        .iter()
        .filter(|item| alive.contains(&item.outbox_id))
        .collect();
    let lines: Vec<String> = ready.iter().filter_map(|item| item.line.clone()).collect();
    if lines.is_empty() {
        // 只剩作废的 op（引用的行已经不在了）：移出队列，再读下一批。
        db::write(ctx.db(), |tx| {
            for item in &ready {
                tx.execute(
                    "DELETE FROM sync_outbox WHERE id = ?1",
                    params![item.outbox_id],
                )?;
            }
            Ok(())
        })?;
        return Ok(Attempt::Retry);
    }
    let from = log.seq + 1;
    let to = log.seq + lines.len() as u64;
    let header = SegmentHeader {
        v: FORMAT_VERSION,
        dev: opts.device.to_string(),
        from,
        to,
        deps,
        prev: log.last.clone(),
    };
    let plaintext = codec::segment_plaintext(&header, &lines)?;
    let entry = SegEntry {
        name: ids::segment_name(from, to),
        digest: sha256_hex(&plaintext),
        from,
        to,
        up: false,
        packed: false,
        gone: false,
    };
    let object = codec::seal_log_object(
        opts.keys,
        &codec::segment_path(opts.device, &entry.name),
        &plaintext,
    );
    // 先把密文写到本机并 fsync，再提交登记：崩在两者之间只留下一个没人引用的文件
    // （[`sweep_outgoing`] 会清掉）。
    let file = ctx.paths.outgoing.join(&entry.name);
    rclaw_store::atomic_write(&file, &object)?;

    let fresh_blob = if opts.upload_attachments {
        BLOB_PENDING
    } else {
        BLOB_HELD
    };
    // 这个提交不在这里落盘：上传之前 `upload_round` 先过落盘点（在库锁之外 fsync）。
    let committed = db::write(ctx.db(), |tx| {
        // 序号是在库锁之外定的：日志状态或这批 op 在此期间变了，这个段就不能用。
        let mut now = db::log_state(tx)?;
        if now.pending.is_some() || now.seq != log.seq || now.last != log.last {
            return Ok(false);
        }
        let still = alive_ids(tx, first, last)?;
        if ready.iter().any(|item| !still.contains(&item.outbox_id)) {
            return Ok(false);
        }
        for item in &ready {
            tx.execute(
                "DELETE FROM sync_outbox WHERE id = ?1",
                params![item.outbox_id],
            )?;
        }
        put_seg_entry(tx, &entry)?;
        now.seq = to;
        now.last = Some(entry.seg_ref());
        now.pending = Some(entry.head_seg());
        db::put_log_state(tx, &now)?;
        for item in &ready {
            if let Some(session_id) = item.open_session {
                tx.execute(
                    "UPDATE sync_session SET sealed = ?2 WHERE session_id = ?1",
                    params![session_id, SEAL_SEALED],
                )?;
            }
            let Some((message_id, new_refs, blobs)) = &item.row else {
                continue;
            };
            tx.execute(
                "UPDATE sync_msg SET sealed = ?2 WHERE message_id = ?1",
                params![message_id, SEAL_SEALED],
            )?;
            if let Some(refs) = new_refs {
                tx.execute(
                    "UPDATE sync_msg SET refs = ?2 WHERE message_id = ?1",
                    params![message_id, refs],
                )?;
            }
            for blob in blobs {
                db::blob_register(tx, &blob.blob_id, &blob.rel_path, blob.size, fresh_blob)?;
            }
        }
        Ok(true)
    });
    match committed {
        Ok(true) => Ok(Attempt::Sealed(entry.head_seg())),
        Ok(false) => {
            let _ = std::fs::remove_file(&file);
            Ok(Attempt::Retry)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&file);
            Err(e)
        }
    }
}

/// 清掉 `outgoing/` 里没有登记的文件（封段写了文件、没来得及提交就退出时留下的）。
pub fn sweep_outgoing(ctx: &Ctx) -> Result<()> {
    let known: HashSet<String> = db::read(ctx.db(), |conn| {
        Ok(seg_entries(conn)?
            .into_iter()
            .map(|entry| entry.name)
            .collect())
    })?;
    let Ok(entries) = std::fs::read_dir(&ctx.paths.outgoing) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !known.contains(&name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::SpaceKey;
    use crate::op::OpLine;
    use rclaw_core::{Message, Role};

    fn seal(ctx: &Ctx, keys: &Keys, device: &str, budget: usize) -> Option<HeadSeg> {
        seal_next(
            ctx,
            &SealOptions {
                keys,
                device,
                budget,
                attachment_max: 1 << 20,
                upload_attachments: false,
            },
        )
        .unwrap()
    }

    fn open_segment(ctx: &Ctx, keys: &Keys, device: &str, seg: &HeadSeg) -> codec::Segment {
        let object = std::fs::read(ctx.paths.outgoing.join(&seg.name)).unwrap();
        let plain = codec::open_log_object(
            keys,
            &codec::segment_path(device, &seg.name),
            &object,
            codec::SEGMENT_PLAIN_MAX,
        )
        .unwrap();
        assert_eq!(sha256_hex(&plain), seg.digest);
        codec::parse_segment(&plain, device).unwrap()
    }

    #[test]
    fn seals_queued_ops_into_a_chained_segment_and_marks_them() {
        let (_g, ctx) = crate::testing::ctx();
        let keys = Keys::derive(&SpaceKey::generate());
        let sessions = &ctx.env.sessions;
        sessions
            .append("desktop", "c1", &Message::plain(Role::User, "一"))
            .unwrap();
        sessions
            .append("desktop", "c1", &Message::plain(Role::Assistant, "二"))
            .unwrap();
        let (device, session) = db::write(ctx.db(), |tx| {
            let device = db::ensure_device(tx)?;
            let session = raw::find_session(tx, "desktop", "c1")?.unwrap();
            map::ensure_mapped(tx, &device, session, true)?;
            Ok((device, session))
        })
        .unwrap();

        let first = seal(&ctx, &keys, &device, 1 << 20).expect("封出一个段");
        assert_eq!((first.from, first.to), (1, 3));
        let segment = open_segment(&ctx, &keys, &device, &first);
        assert_eq!(segment.header.prev, None);
        let kinds: Vec<String> = segment
            .lines
            .iter()
            .map(|line| OpLine::parse(line).unwrap().kind)
            .collect();
        assert_eq!(kinds, ["session.open", "msg", "msg"]);
        // 消息 op 是完整的，能通过合入检查。
        let Op::Msg(op) = OpLine::parse(&segment.lines[1]).unwrap().decode().unwrap() else {
            panic!("应为 msg");
        };
        crate::portable::check_wire_msg(&op.msg).unwrap();

        db::read(ctx.db(), |conn| {
            assert_eq!(map::outbox_len(conn)?, 0);
            let maps = map::session_msg_maps(conn, session)?;
            assert!(maps
                .iter()
                .all(|m| m.sealed == SEAL_SEALED && m.refs.is_some()));
            assert_eq!(
                map::session_map(conn, session)?.unwrap().sealed,
                SEAL_SEALED
            );
            let log = db::log_state(conn)?;
            assert_eq!(log.seq, 3);
            assert_eq!(log.pending.as_ref(), Some(&first));
            Ok(())
        })
        .unwrap();

        // 上一个段没传完：不封下一个。
        sessions
            .append("desktop", "c1", &Message::plain(Role::User, "三"))
            .unwrap();
        db::write(ctx.db(), |tx| {
            map::ensure_mapped(tx, &device, session, true)?;
            Ok(())
        })
        .unwrap();
        assert!(seal(&ctx, &keys, &device, 1 << 20).is_none());

        // 记账之后，下一个段接在上一个后面。
        db::write(ctx.db(), |tx| {
            let mut log = db::log_state(tx)?;
            log.pending = None;
            db::put_log_state(tx, &log)
        })
        .unwrap();
        let second = seal(&ctx, &keys, &device, 1 << 20).unwrap();
        assert_eq!((second.from, second.to), (4, 4));
        let segment = open_segment(&ctx, &keys, &device, &second);
        assert_eq!(segment.header.prev, Some(first.seg_ref()));
    }

    #[test]
    fn budget_splits_the_queue_but_always_takes_one_op() {
        let (_g, ctx) = crate::testing::ctx();
        let keys = Keys::derive(&SpaceKey::generate());
        for text in ["一", "二", "三"] {
            ctx.env
                .sessions
                .append("desktop", "c1", &Message::plain(Role::User, text))
                .unwrap();
        }
        let device = db::write(ctx.db(), |tx| {
            let device = db::ensure_device(tx)?;
            let session = raw::find_session(tx, "desktop", "c1")?.unwrap();
            map::ensure_mapped(tx, &device, session, true)?;
            Ok(device)
        })
        .unwrap();
        let first = seal(&ctx, &keys, &device, 1).unwrap();
        assert_eq!((first.from, first.to), (1, 1), "预算再小也装一条");
        assert_eq!(db::read(ctx.db(), map::outbox_len).unwrap(), 3);
    }

    #[test]
    fn ops_of_a_deleted_session_are_not_sealed() {
        let (_g, ctx) = crate::testing::ctx();
        let keys = Keys::derive(&SpaceKey::generate());
        ctx.env
            .sessions
            .append("desktop", "c1", &Message::plain(Role::User, "一"))
            .unwrap();
        let device = db::write(ctx.db(), |tx| {
            let device = db::ensure_device(tx)?;
            let session = raw::find_session(tx, "desktop", "c1")?.unwrap();
            map::ensure_mapped(tx, &device, session, true)?;
            Ok(device)
        })
        .unwrap();
        ctx.env.sessions.delete_session("desktop", "c1").unwrap();
        assert!(seal(&ctx, &keys, &device, 1 << 20).is_none());
        // 连 session.open 都没封过：别的设备不知道它，不登记删除。
        assert_eq!(db::read(ctx.db(), map::outbox_len).unwrap(), 0);
    }

    #[test]
    fn sweep_removes_unregistered_outgoing_files() {
        let (_g, ctx) = crate::testing::ctx();
        db::write(ctx.db(), |tx| db::ensure_device(tx).map(|_| ())).unwrap();
        std::fs::write(ctx.paths.outgoing.join("stray.seg"), b"x").unwrap();
        sweep_outgoing(&ctx).unwrap();
        assert!(!ctx.paths.outgoing.join("stray.seg").exists());
    }
}
