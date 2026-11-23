//! 设备身份（§6、§9.6）：启动时的核对、实例号、换身份。
//!
//! 协议的前提是「一个 `device_id` 只有一个写者，而且它记得自己写过什么」。库被旧备份
//! 覆盖、数据根被复制到另一台机器上继续用，都会打破它。发现之后（[`crate::upload`] 的
//! 身份自检，或别的设备声明取代了本机正在用的身份）本机换一个新身份：
//!
//! 1. 新的 `device_id`，用 `device.hello` 声明它取代旧身份；
//! 2. 分叉点（本机上传过、同时也在存储上那条链上的最后一个段）之后本机的全部 op 在新
//!    身份下重新发布，`gid` 不变；
//! 3. 旧身份在存储上的那条日志，从分叉点起当作别的设备的日志取回、合入。
//!
//! 重新发布的 op 别的设备可能已经合入过，靠幂等消化（§9.2）。

use std::sync::Arc;

use rclaw_store::sync_db::{self, SEAL_LOCAL};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::attach::now_ms;
use crate::codec::{self, Head, SegRef};
use crate::crypto::{sha256_hex, Keys};
use crate::db::{self, scope, DeviceRecord, LogState, PeerState};
use crate::error::{Result, SyncError};
use crate::files;
use crate::ids;
use crate::map;
use crate::merge;
use crate::op::{Op, OpLine};
use crate::publish::{self, Settings};
use crate::pull;
use crate::remote;
use crate::seal::{self, SegEntry};
use crate::secrets::Secrets;
use crate::transport::SyncTransport;
use crate::upload::{self, PackEntry};
use crate::{blocking, offload, Ctx};

/// 库里记着的实例号只留最近这么多个。
const INSTANCES_KEPT: usize = 32;

/// 引擎这次运行用的身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub device: String,
    /// 本机用过的全部身份（当前的在最前）。
    pub my_ids: Vec<String>,
    /// 这次启动的实例号（已经在本机落盘）。
    pub instance: String,
    /// 这个库里的引擎用过的实例号。
    pub instances: Vec<String>,
}

/// 记在库里的「要换身份」（kv `sync/rotate`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotateTodo {
    pub reason: String,
    /// 除了当前身份之外，还要声明取代的身份。
    #[serde(default)]
    pub also: Vec<String>,
}

/// 记下「本机要换身份」。在调用方的写事务里执行；已经记着的原因保留最早的那个。
pub fn request_rotation(conn: &Connection, reason: &str, also: &[String]) -> Result<()> {
    let mut todo: RotateTodo = db::kv_json(conn, db::KV_ROTATE)?.unwrap_or_default();
    if todo.reason.is_empty() {
        todo.reason = reason.to_string();
    }
    for id in also {
        if !todo.also.contains(id) {
            todo.also.push(id.clone());
        }
    }
    db::kv_put_json(conn, db::KV_ROTATE, &todo)
}

pub fn rotation_todo(conn: &Connection) -> Result<Option<RotateTodo>> {
    db::kv_json(conn, db::KV_ROTATE)
}

/// 引擎启动时调用：把 `sync/secrets.json` 里的设备身份与库里的记录对齐，并登记这次启动
/// 的实例号（先落盘，之后才会写进 head）。
///
/// | 库里 | secrets 里 | 处理 |
/// |---|---|---|
/// | 有 | 相同 | 正常 |
/// | 有 | 空 | 沿用库里的 |
/// | 有 | 是库里记着的旧身份 | 上次换身份换到一半退出了：以库为准 |
/// | 有 | 库不认识的身份 | 库或 secrets 被换过：记下「要换身份」，由引擎的第一轮去做 |
/// | 没有 | 有 | 库被清空过：新建身份，声明它取代旧的；旧身份的日志当作别的设备的取回 |
/// | 没有 | 空 | 新建 |
pub fn prepare(ctx: &Ctx, secrets: &mut Secrets) -> Result<Identity> {
    let instance = ids::new_instance_id();
    let known = Some(secrets.device_id.clone()).filter(|id| ids::is_device_id(id));
    let (record, pending) = db::write(ctx.db(), |tx| {
        sync_db::ensure_sync_tables(tx)?;
        let existing = db::device_record(tx)?.filter(|record| ids::is_device_id(&record.id));
        let mut record = match existing {
            Some(record) => {
                if let Some(known) = &known {
                    if *known != record.id && !record.past.contains(known) {
                        request_rotation(
                            tx,
                            "本机的同步身份与数据库里记着的不是同一个（数据库或 sync/secrets.json 被替换过）",
                            std::slice::from_ref(known),
                        )?;
                    }
                }
                record
            }
            None => {
                let mut record = DeviceRecord {
                    id: ids::new_device_id(),
                    ..DeviceRecord::default()
                };
                if let Some(known) = &known {
                    record.past.push(known.clone());
                    record.announce.push(known.clone());
                }
                record
            }
        };
        record.instances.push(instance.clone());
        let excess = record.instances.len().saturating_sub(INSTANCES_KEPT);
        record.instances.drain(..excess);
        db::put_device_record(tx, &record)?;
        Ok((record, rotation_todo(tx)?.is_some()))
    })?;
    // 先落盘，再往 secrets 里写：断电让这个提交回滚的话，库里没有身份而 secrets 里有，
    // 下次启动会当成「库被清空过」再建一个身份。这时还没有任何内存里的状态，失败就整个
    // 重来。
    db::flush(ctx.db())?;
    // 要换身份的时候 secrets 先不动：换完才写新身份，中途退出下次还能发现。
    if secrets.device_id != record.id && !pending {
        secrets.device_id = record.id.clone();
        secrets.save(&ctx.paths)?;
    }
    Ok(identity_of(&record, instance))
}

fn identity_of(record: &DeviceRecord, instance: String) -> Identity {
    let mut my_ids = vec![record.id.clone()];
    my_ids.extend(record.past.iter().cloned());
    Identity {
        device: record.id.clone(),
        my_ids,
        instance,
        instances: record.instances.clone(),
    }
}

/// 发出本机的 `device.hello`。有还没声明过的「被当前身份取代的旧身份」就各发一条；否则
/// 只在名字、平台、版本与上次发出的不同时才发。返回有没有排进待发队列。
pub fn announce(ctx: &Ctx, s: &Settings, platform: &str, version: &str) -> Result<bool> {
    db::write(ctx.db(), |tx| {
        let Some(mut record) = db::device_record(tx)? else {
            return Ok(false);
        };
        if record.announce.is_empty() {
            return publish::put_hello(tx, s, platform, version, None);
        }
        let mut published = false;
        for old in std::mem::take(&mut record.announce) {
            published |= publish::put_hello(tx, s, platform, version, Some(&old))?;
        }
        db::put_device_record(tx, &record)?;
        Ok(published)
    })
}

// ---------------------------------------------------------------------------
// 分叉点
// ---------------------------------------------------------------------------

/// 分叉点：序号不大于 `seq` 的日志两边一致。`seg` 是那个位置上的段（不知道时为空）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fork {
    pub seq: u64,
    pub seg: Option<SegRef>,
}

/// 找分叉点：本机封过、同时也在存储上那条链上的最后一个段（§9.6 第 2 步）。段名带随机
/// 后缀，名字与摘要都相同就是同一个段；日志是哈希链，同一个段之前的部分必然一致。
/// 找不到共同的段（或存储上没有 head）时退到本机日志现存的开头。
pub fn fork_point(
    log: &LogState,
    entries: &[SegEntry],
    packs: &[PackEntry],
    head: Option<&Head>,
) -> Fork {
    let mut best = Fork {
        seq: log.floor,
        seg: None,
    };
    let Some(head) = head else {
        return best;
    };
    let mut consider = |seq: u64, seg: SegRef| {
        if seq > best.seq || (seq == best.seq && best.seg.is_none()) {
            best = Fork {
                seq,
                seg: Some(seg),
            };
        }
    };
    let mine = |to: u64, seg: &SegRef| {
        entries
            .iter()
            .any(|entry| entry.to == to && entry.name == seg.name && entry.digest == seg.digest)
    };
    // head 的零散段清单里与本机登记一致的。
    for seg in &head.segs {
        let seg_ref = seg.seg_ref();
        if mine(seg.to, &seg_ref) {
            consider(seg.to, seg_ref);
        }
    }
    if let Some(rollup) = &head.rollup {
        // head 指着的汇总包就是本机写的某一个：它覆盖的部分都一致。
        if let Some(pack) = packs
            .iter()
            .find(|pack| pack.name == rollup.name && pack.digest == rollup.digest)
        {
            consider(pack.to, pack.last.clone());
        }
        // 汇总包不是本机写的，但包里最后一个段是。
        if mine(rollup.to, &rollup.last) {
            consider(rollup.to, rollup.last.clone());
        }
    }
    // 日志被回收到末尾时 head 只记着末段。
    if head.rollup.is_none() && head.segs.is_empty() {
        if let Some(last) = &head.last_seg {
            if mine(head.last_seq, last) {
                consider(head.last_seq, last.clone());
            }
        }
    }
    best
}

// ---------------------------------------------------------------------------
// 换身份
// ---------------------------------------------------------------------------

/// 换身份的结果。
#[derive(Debug, Clone)]
pub struct Rotated {
    /// 换掉的旧身份。
    pub old: String,
    pub identity: Identity,
    /// 分叉点之后的段文件没能读全，改成了从库里重新发布本机产生的全部内容。
    pub from_db: bool,
}

/// HLC 里的设备段。
fn hlc_device(hlc: &str) -> Option<&str> {
    hlc.splitn(3, '-').nth(2)
}

/// 把本机封过的这些段里的 op 行取回来：密文在 `outgoing/`，不在了就从存储上取。
async fn read_back(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    device: &str,
    entries: &[SegEntry],
) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for entry in entries {
        let path = codec::segment_path(device, &entry.name);
        let object = match tokio::fs::read(ctx.paths.outgoing.join(&entry.name)).await {
            Ok(bytes) => bytes,
            Err(_) => transport.get(&path, codec::SEGMENT_OBJECT_MAX).await?,
        };
        let (keys, device, digest) = (keys.clone(), device.to_string(), entry.digest.clone());
        let segment = blocking(move || {
            let plain = codec::open_log_object(&keys, &path, &object, codec::SEGMENT_PLAIN_MAX)?;
            if sha256_hex(&plain) != digest {
                return Err(SyncError::corrupt(format!("{path} 与本机登记的摘要不符")));
            }
            codec::parse_segment(&plain, &device)
        })
        .await?;
        lines.extend(segment.lines);
    }
    Ok(lines)
}

/// 身份相关的 op 不能照搬到新身份的日志里：`device.hello` 由新身份自己发；定时任务的 op
/// 只认当事设备自己发的——本机的任务以新身份为归属重新发布，正在交接的由
/// [`files::cron::on_rotate`] 换到新身份名下。
fn is_identity_bound(kind: &str) -> bool {
    kind == "device.hello" || kind.starts_with("cron.")
}

/// 段里的 op 行 → 待发队列的行：`hlc` 从 JSON 里拿出来放回它自己那一列，值不变。
fn requeue_lines(lines: &[String]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let mut parsed = OpLine::parse(line)?;
        if is_identity_bound(&parsed.kind) {
            continue;
        }
        if let Some(object) = parsed.value.as_object_mut() {
            object.remove("hlc");
        }
        out.push((parsed.hlc, parsed.value.to_string()));
    }
    Ok(out)
}

/// 段文件读不全时的退路：从库里把本机写出过的非消息内容找出来（消息由发布扫描重新
/// 排队）。属性和删除记录看 HLC 的设备段判断是不是本机写的，文件类数据看寄存器里记的
/// 来源设备；都带着原来的 HLC，别的设备已经有的不会被盖掉。
fn requeue_from_db(ctx: &Ctx, s: &Settings, my_ids: &[String]) -> Result<Vec<(String, String)>> {
    let mine = |device: &str| my_ids.iter().any(|id| id == device);
    let (mut ops, tombs, registers) = db::read(ctx.db(), |conn| {
        let mut ops: Vec<(String, String)> = Vec::new();
        for (key, row) in db::state_scan(conn, scope::ATTR, "")? {
            if !hlc_device(&row.hlc).is_some_and(&mine) {
                continue;
            }
            let mut parts = key.splitn(3, '|');
            let (Some(sid), Some(at), Some(field)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let Some(value) = row
                .value
                .as_deref()
                .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
            else {
                continue;
            };
            // 不参与同步的会话（通道不在名单里、仅本机）上设置的属性也记在这张表里
            // （合入时比较先后要用），它们不能发出去。
            let mut published = false;
            for session in map::thread_sessions(conn, sid)? {
                if publish::eligible(conn, s, &session.channel, &session.chat_id)?
                    && (at.is_empty() || map::message_of(conn, session.session_id, at)?.is_some())
                {
                    published = true;
                    break;
                }
            }
            if !published {
                continue;
            }
            let op = Op::SessionSet {
                sid: sid.to_string(),
                at: (!at.is_empty()).then(|| at.to_string()),
                field: field.to_string(),
                value,
            };
            ops.push((row.hlc, serde_json::to_string(&op)?));
        }
        // 墓碑只在删除一个已经发布过的会话时才写，都可以再发一遍。
        let tombs: Vec<(String, db::StateRow)> = db::state_scan(conn, scope::TOMB, "")?
            .into_iter()
            .filter(|(_, row)| hlc_device(&row.hlc).is_some_and(&mine))
            .collect();
        Ok((ops, tombs, files::register_rows(conn)?))
    })?;
    // 删除记录带着链（§7.5），一条可以有几十 KB：出了上面那次读取，再分批去取。
    for batch in tombs.chunks(32) {
        let chains: Vec<String> = db::read(ctx.db(), |conn| {
            let mut chains = Vec::with_capacity(batch.len());
            for (key, _) in batch {
                chains.push(
                    db::state_get(conn, scope::TOMB_CHAIN, key)?
                        .and_then(|row| row.value)
                        .unwrap_or_default(),
                );
            }
            Ok(chains)
        })?;
        for ((key, row), chain) in batch.iter().zip(chains) {
            let Some((sid, tip)) = key.split_once('/') else {
                continue;
            };
            let op = Op::SessionDelete {
                sid: sid.to_string(),
                tip: (!tip.is_empty()).then(|| tip.to_string()),
                purge: merge::tomb_purge(row),
                chain,
            };
            ops.push((row.hlc.clone(), serde_json::to_string(&op)?));
        }
    }
    // 寄存器的值可能很大（技能清单）：解析放在库锁之外。
    for (hlc, value) in registers {
        let Ok(stored) = serde_json::from_str::<serde_json::Value>(&value) else {
            continue;
        };
        let origin = stored
            .get("origin")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let Some(op) = stored.get("op").filter(|op| op.is_object()) else {
            continue;
        };
        let kind = op.get("t").and_then(|v| v.as_str()).unwrap_or_default();
        if mine(origin) && !is_identity_bound(kind) {
            ops.push((hlc, op.to_string()));
        }
    }
    ops.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(ops)
}

/// 换身份（§9.6）。`current` 是引擎正在用的身份；`reason` 只进日志。
///
/// 本机状态的改动在一个事务里完成：新身份、旧身份的读取进度、重新发布的 op、清空的日志
/// 状态。提交并落盘之后才删旧的段文件、改写 secrets——中途退出的话，下次启动时
/// [`prepare`] 会发现 secrets 里还是旧身份，以库为准。
pub async fn rotate(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    s: &Settings,
    secrets: &mut Secrets,
    current: &Identity,
    reason: &str,
) -> Result<Rotated> {
    let old = current.device.clone();
    tracing::warn!("本机换用新的同步身份（{reason}）");

    // 一、不持库锁：读存储上旧身份的 head，定分叉点，把分叉点之后本机封过的 op 读回来。
    let head = match remote::read_head(transport, keys, &old).await {
        Ok(head) => head,
        // 解不开、或者是更新的版本写的：当作没有共同的段。
        Err(SyncError::Corrupt(_) | SyncError::Upgrade(_)) => None,
        Err(error) => return Err(error),
    };
    let (log, entries, packs) = offload(ctx, |ctx| {
        db::read(ctx.db(), |conn| {
            Ok((
                db::log_state(conn)?,
                seal::seg_entries(conn)?,
                upload::pack_entries(conn)?,
            ))
        })
    })
    .await?;
    let fork = fork_point(&log, &entries, &packs, head.as_ref());
    let after: Vec<SegEntry> = entries
        .iter()
        .filter(|entry| entry.to > fork.seq)
        .cloned()
        .collect();
    // 分叉点之后的段必须从分叉点起一个不缺，才能原样重发。
    let contiguous = after.first().is_none_or(|first| first.from == fork.seq + 1);
    let requeue = if contiguous {
        match read_back(ctx, transport, keys, &old, &after).await {
            Ok(lines) => blocking(move || requeue_lines(&lines).map(Some)).await,
            Err(error) => Err(error),
        }
    } else {
        Ok(None)
    };
    let requeue = match requeue {
        Ok(requeue) => requeue,
        Err(error)
            if error.is_not_found()
                || matches!(error, SyncError::Corrupt(_) | SyncError::Io(_)) =>
        {
            tracing::warn!("分叉点之后的段没能读全（{error}），改为从库里重新发布");
            None
        }
        Err(error) => return Err(error),
    };
    let from_db = requeue.is_none();
    let requeue = match requeue {
        Some(requeue) => requeue,
        None => {
            let (ctx, s, my_ids) = (ctx.clone(), s.clone(), current.my_ids.clone());
            blocking(move || requeue_from_db(&ctx, &s, &my_ids)).await?
        }
    };

    // 二、一个事务，提交之后落盘。
    let new_id = ids::new_device_id();
    let instance = current.instance.clone();
    let record = {
        let (ctx, old, new_id, instance) =
            (ctx.clone(), old.clone(), new_id.clone(), instance.clone());
        let my_ids = current.my_ids.clone();
        blocking(move || {
            let record = db::write(ctx.db(), |tx| {
                let mut record = db::device_record(tx)?
                    .ok_or_else(|| SyncError::other("本机没有同步身份"))?;
                if record.id != old || db::log_state(tx)?.seq != log.seq {
                    return Err(SyncError::other("换身份期间本机的同步状态变了，下一轮再试"));
                }
                // 1. 新身份。
                let todo = rotation_todo(tx)?.unwrap_or_default();
                record.past.insert(0, old.clone());
                for id in &todo.also {
                    if ids::is_device_id(id) && *id != new_id && !record.past.contains(id) {
                        record.past.push(id.clone());
                    }
                    if ids::is_device_id(id) && !record.announce.contains(id) {
                        record.announce.push(id.clone());
                    }
                }
                record.announce.retain(|id| *id != old);
                record.announce.push(old.clone());
                record.id = new_id.clone();
                record.instances = vec![instance.clone()];
                db::put_device_record(tx, &record)?;
                sync_db::kv_del(tx, db::KV_ROTATE)?;

                // 2. 旧身份在存储上的日志，从分叉点起当作别的设备的日志读。
                let peer = PeerState {
                    fetched: fork.seq,
                    fetched_seg: fork.seg.clone(),
                    processed: fork.seq,
                    own_until: Some(fork.seq),
                    replaced_by: Some(new_id.clone()),
                    seen_ms: now_ms(),
                    ..PeerState::default()
                };
                db::put_peer_state(tx, &old, &peer)?;
                tx.execute(
                    "DELETE FROM sync_state WHERE scope = ?1 AND key >= ?2 AND key < ?3",
                    params![scope::RSEG, format!("{old}/"), format!("{old}/\u{10ffff}")],
                )?;
                if let Some(seg) = &fork.seg {
                    db::state_put_json(
                        tx,
                        scope::RSEG,
                        &pull::rseg_key(&old, fork.seq),
                        db::HLC_ZERO,
                        seg,
                    )?;
                }

                // 3. 重新发布。
                if from_db {
                    // 本机产生的消息回到「只分配了身份」，会话的 `session.open` 也重新
                    // 排队：下一次全量扫描把它们按链的顺序排进队列。队列里原有的会话类
                    // op 作废（消息、开头信息由扫描重排，属性在下面带着原来的 HLC 重发）。
                    tx.execute("DELETE FROM sync_outbox WHERE session_id IS NOT NULL", [])?;
                    tx.execute("UPDATE sync_session SET sealed = ?1", params![SEAL_LOCAL])?;
                    for id in &my_ids {
                        tx.execute(
                            "UPDATE sync_msg SET sealed = ?1 WHERE gid LIKE ?2",
                            params![SEAL_LOCAL, format!("{id}.%")],
                        )?;
                    }
                    publish::reset_cursor(tx)?;
                }
                // 排在队列里已有的 op 之前（它们本来就先于队列里的）。
                let first: i64 = tx.query_row(
                    "SELECT COALESCE(MIN(id), 1) FROM sync_outbox",
                    [],
                    |row| row.get(0),
                )?;
                let mut id = first - requeue.len() as i64;
                for (hlc, op) in &requeue {
                    tx.execute(
                        "INSERT INTO sync_outbox(id, session_id, hlc, op) VALUES (?1, NULL, ?2, ?3)",
                        params![id, hlc, op],
                    )?;
                    id += 1;
                }

                // 4. 新身份的日志从头开始。旧身份名下的段、汇总包、快照留在存储上，别的
                // 设备可能还没读完；回收它们的登记也不再有意义。
                tx.execute(
                    "DELETE FROM sync_state WHERE scope IN (?1, ?2, ?3)",
                    params![scope::SEG, scope::PACK, scope::GC],
                )?;
                db::put_log_state(tx, &LogState::default())?;
                crate::snapshot::forget_own(tx)?;
                // 先把空间里的内容（包括旧身份名下本机不知道的那部分）拉取合入，再发布
                // 本机的文件类数据；定时任务以新身份为归属重新发布，正在交接的也跟着
                // 换到新身份名下。
                sync_db::kv_del(tx, db::KV_PRIMED)?;
                // 换身份多半是因为本机的库被还原成了旧的：手里的会话可能是别处已经续写
                // 又删掉的分支更早的样子。各设备的日志读到头之后清点一遍（§7.5），在那
                // 之前不发布本机存量——本机建的空间也一样。
                db::kv_put_json(tx, db::KV_STALE_CHECK, &true)?;
                files::cron::on_rotate(tx, &record.past, &record.id)?;
                Ok(record)
            })?;
            // 身份已经换了（提交过了），落盘失败不能当成「没换」返回：照常往下走。往存储上
            // 写任何东西之前上传那一步还会再过一次落盘点；真在这之间断电，库回到旧身份而
            // secrets 里是新的，下次启动按「库不认识的身份」再换一次。
            if let Err(error) = db::flush(ctx.db()) {
                tracing::warn!("换身份之后落盘失败: {error}");
            }
            Ok(record)
        })
        .await?
    };

    // 三、提交之后：旧身份的段文件用不上了；secrets 换成新身份。
    //
    // 到这里身份已经换了，后面哪一步不成都不能当成「没换」报回去——调用方会接着拿旧
    // 身份去写一个已经是新身份的库。secrets 没写成的，下次启动 `prepare` 见到里面是库里
    // 记着的旧身份，以库为准再写一次。
    secrets.device_id = new_id;
    let saved = {
        let (paths, secrets) = (ctx.paths.clone(), secrets.clone());
        blocking(move || {
            for entry in &entries {
                let _ = std::fs::remove_file(paths.outgoing.join(&entry.name));
            }
            secrets.save(&paths)
        })
        .await
    };
    if let Err(error) = saved {
        tracing::warn!("新的同步身份没能写进 sync/secrets.json（下次启动时补上）: {error}");
    }
    Ok(Rotated {
        old,
        identity: identity_of(&record, instance),
        from_db,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::HeadSeg;
    use crate::op::FORMAT_VERSION;
    use crate::seal::SealOptions;
    use crate::testing;
    use crate::transport::file::FileTransport;
    use crate::upload::{upload_round, UploadOptions};
    use rclaw_core::{Message, Role};
    use std::collections::BTreeMap;

    #[test]
    fn prepare_reconciles_secrets_with_the_database() {
        let (_g, ctx) = testing::ctx();
        let mut secrets = Secrets::default();
        // 第一次：建身份，写进 secrets。
        let first = prepare(&ctx, &mut secrets).unwrap();
        assert!(ids::is_device_id(&first.device));
        assert_eq!(secrets.device_id, first.device);
        assert_eq!(first.instances, vec![first.instance.clone()]);
        assert_eq!(Secrets::load(&ctx.paths).unwrap().device_id, first.device);
        // 再启动：同一个身份，新的实例号，旧的也还记着。
        let second = prepare(&ctx, &mut secrets).unwrap();
        assert_eq!(second.device, first.device);
        assert_ne!(second.instance, first.instance);
        assert_eq!(
            second.instances,
            vec![first.instance.clone(), second.instance.clone()]
        );
        // secrets 丢了：沿用库里的。
        let mut blank = Secrets::default();
        assert_eq!(prepare(&ctx, &mut blank).unwrap().device, first.device);
        assert_eq!(blank.device_id, first.device);
        // secrets 里是换身份之前的旧身份（换到一半退出）：以库为准。
        db::write(ctx.db(), |tx| {
            let mut record = db::device_record(tx)?.unwrap();
            record.past = vec!["00000000000000aa".into()];
            db::put_device_record(tx, &record)
        })
        .unwrap();
        let mut stale = Secrets {
            device_id: "00000000000000aa".into(),
            ..Secrets::default()
        };
        prepare(&ctx, &mut stale).unwrap();
        assert_eq!(stale.device_id, first.device);
        assert!(db::read(ctx.db(), rotation_todo).unwrap().is_none());
        // secrets 里是库不认识的身份：记下要换身份，并且要声明取代它；secrets 先不动。
        let mut foreign = Secrets {
            device_id: "00000000000000bb".into(),
            ..Secrets::default()
        };
        let identity = prepare(&ctx, &mut foreign).unwrap();
        assert_eq!(identity.device, first.device);
        let todo = db::read(ctx.db(), rotation_todo).unwrap().unwrap();
        assert_eq!(todo.also, ["00000000000000bb"]);
        assert_eq!(foreign.device_id, "00000000000000bb");
    }

    #[test]
    fn a_fresh_database_with_old_secrets_gets_an_identity_that_replaces_the_old_one() {
        let (_g, ctx) = testing::ctx();
        let mut secrets = Secrets {
            device_id: "00000000000000cc".into(),
            ..Secrets::default()
        };
        let identity = prepare(&ctx, &mut secrets).unwrap();
        assert_ne!(identity.device, "00000000000000cc");
        assert_eq!(
            identity.my_ids,
            [identity.device.clone(), "00000000000000cc".to_string()]
        );
        assert_eq!(secrets.device_id, identity.device);
        let record = db::read(ctx.db(), db::device_record).unwrap().unwrap();
        assert_eq!(record.announce, ["00000000000000cc"]);
        // 声明发出去之后不再重复。
        let s = testing::settings(&ctx);
        assert!(announce(&ctx, &s, "test", "0.0.0").unwrap());
        let ops = db::read(ctx.db(), |conn| {
            let mut stmt = conn.prepare("SELECT op FROM sync_outbox ORDER BY id")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap();
        assert_eq!(ops.len(), 1);
        assert!(ops[0].contains("\"replaces\":\"00000000000000cc\""));
        assert!(!announce(&ctx, &s, "test", "0.0.0").unwrap());
    }

    fn entry(from: u64, to: u64, tag: &str) -> SegEntry {
        SegEntry {
            name: format!("{from:012}-{to:012}-{tag}.seg"),
            digest: "cd".repeat(32),
            from,
            to,
            up: true,
            packed: false,
            gone: false,
        }
    }

    fn head_with(segs: &[&SegEntry]) -> Head {
        Head {
            v: FORMAT_VERSION,
            dev: "0123456789abcdef".into(),
            instance: "00000000000000aa".into(),
            last_seq: segs.last().map_or(0, |seg| seg.to),
            last_seg: segs.last().map(|seg| seg.seg_ref()),
            segs: segs.iter().map(|seg| seg.head_seg()).collect(),
            rollup: None,
            floor: 0,
            acks: BTreeMap::new(),
            running: Vec::new(),
            snap: None,
            ts: 1,
        }
    }

    #[test]
    fn fork_point_is_the_last_segment_both_sides_have() {
        let (a, b, c) = (
            entry(1, 2, "aaaaaaaa"),
            entry(3, 3, "bbbbbbbb"),
            entry(4, 6, "cccccccc"),
        );
        let entries = vec![a.clone(), b.clone(), c.clone()];
        let log = LogState::default();
        let at = |entry: &SegEntry| Fork {
            seq: entry.to,
            seg: Some(entry.seg_ref()),
        };
        let none = Fork { seq: 0, seg: None };
        // 完全一致。
        assert_eq!(
            fork_point(&log, &entries, &[], Some(&head_with(&[&a, &b, &c]))),
            at(&c)
        );
        // b 之后是别人写的段。
        let other = entry(4, 5, "dddddddd");
        assert_eq!(
            fork_point(&log, &entries, &[], Some(&head_with(&[&a, &b, &other]))),
            at(&b)
        );
        // 没有共同的段，或者存储上没有 head。
        let stranger = entry(1, 9, "eeeeeeee");
        assert_eq!(
            fork_point(&log, &entries, &[], Some(&head_with(&[&stranger]))),
            none
        );
        assert_eq!(fork_point(&log, &entries, &[], None), none);
        // head 指着本机写的汇总包。
        let pack = PackEntry {
            name: "000000000001-000000000003-ffffffff.pack".into(),
            digest: "ab".repeat(32),
            from: 1,
            to: 3,
            last: b.seg_ref(),
        };
        let mut packed = head_with(&[&other]);
        packed.rollup = Some(pack.rollup_ref());
        assert_eq!(
            fork_point(&log, &[c.clone()], &[pack.clone()], Some(&packed)),
            at(&b)
        );
        // 汇总包是另一个写者打的，但包尾是本机的段。
        let mut foreign = packed.clone();
        foreign.rollup.as_mut().unwrap().digest = "ef".repeat(32);
        assert_eq!(fork_point(&log, &entries, &[pack], Some(&foreign)), at(&b));
        // 日志被回收到末尾：head 只记着末段。
        let mut reclaimed = head_with(&[]);
        reclaimed.floor = 6;
        reclaimed.last_seq = 6;
        reclaimed.last_seg = Some(c.seg_ref());
        assert_eq!(fork_point(&log, &entries, &[], Some(&reclaimed)), at(&c));
    }

    struct Rig {
        _dir: tempfile::TempDir,
        _store: tempfile::TempDir,
        ctx: Ctx,
        transport: FileTransport,
        keys: Arc<Keys>,
        secrets: Secrets,
        identity: Identity,
        settings: Settings,
    }

    fn rig() -> Rig {
        let (dir, ctx) = testing::ctx();
        let mut secrets = Secrets::default();
        let identity = prepare(&ctx, &mut secrets).unwrap();
        let settings = testing::settings(&ctx);
        let store = tempfile::tempdir().unwrap();
        Rig {
            transport: FileTransport::new(store.path()),
            _dir: dir,
            _store: store,
            ctx,
            keys: Arc::new(testing::keys()),
            secrets,
            identity,
            settings,
        }
    }

    impl Rig {
        fn seal(&self) -> Option<HeadSeg> {
            seal::seal_next(
                &self.ctx,
                &SealOptions {
                    keys: &self.keys,
                    device: &self.identity.device,
                    budget: 1 << 20,
                    attachment_max: 1 << 20,
                    upload_attachments: false,
                },
            )
            .unwrap()
        }

        /// 排一条 op、封段。
        fn seal_one(&self, blob: &str) -> HeadSeg {
            db::write(self.ctx.db(), |tx| {
                map::enqueue(
                    tx,
                    &self.identity.device,
                    None,
                    &Op::BlobWant {
                        blob: blob.repeat(32),
                    },
                )
            })
            .unwrap();
            self.seal().expect("封出一个段")
        }

        async fn upload(&self) -> upload::UploadReport {
            upload_round(
                &self.ctx,
                &self.transport,
                &UploadOptions {
                    keys: &self.keys,
                    device: &self.identity.device,
                    instance: &self.identity.instance,
                    instances: &self.identity.instances,
                    running: Vec::new(),
                    pack_plain_max: 1 << 20,
                    rollup_every: 100,
                    force_head: true,
                },
            )
            .await
            .unwrap()
        }

        /// 另一份数据（同一个身份、另一个实例）把 head 改写成 `segs`。
        async fn overwrite_head(&self, segs: Vec<HeadSeg>) {
            let head = Head {
                v: FORMAT_VERSION,
                dev: self.identity.device.clone(),
                instance: ids::new_instance_id(),
                last_seq: segs.last().map_or(0, |seg| seg.to),
                last_seg: segs.last().map(HeadSeg::seg_ref),
                segs,
                rollup: None,
                floor: 0,
                acks: BTreeMap::new(),
                running: Vec::new(),
                snap: None,
                ts: now_ms(),
            };
            remote::write_head(&self.transport, &self.keys, &head)
                .await
                .unwrap();
        }

        async fn rotate(&mut self) -> Rotated {
            let rotated = rotate(
                &self.ctx,
                &self.transport,
                &self.keys,
                &self.settings,
                &mut self.secrets,
                &self.identity,
                "测试",
            )
            .await
            .unwrap();
            self.identity = rotated.identity.clone();
            self.settings.device = rotated.identity.device.clone();
            self.settings.my_ids = rotated.identity.my_ids.clone();
            rotated
        }

        fn outbox(&self) -> Vec<(i64, String, serde_json::Value)> {
            db::read(self.ctx.db(), |conn| {
                let mut stmt = conn.prepare("SELECT id, hlc, op FROM sync_outbox ORDER BY id")?;
                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?;
                let mut out = Vec::new();
                for row in rows {
                    let (id, hlc, op) = row?;
                    out.push((id, hlc, serde_json::from_str(&op)?));
                }
                Ok(out)
            })
            .unwrap()
        }
    }

    #[tokio::test]
    async fn a_second_writer_makes_this_device_rotate_and_republish_after_the_fork() {
        let mut rig = rig();
        let old = rig.identity.device.clone();
        let first = rig.seal_one("aa");
        rig.upload().await;
        let second = rig.seal_one("bb");
        rig.upload().await;
        // 另一份数据在第一个段之后写了自己的段。
        let foreign = HeadSeg {
            name: ids::segment_name(2, 3),
            digest: "ef".repeat(32),
            from: 2,
            to: 3,
        };
        rig.overwrite_head(vec![first.clone(), foreign]).await;
        // 队列里还有一条没封的。
        db::write(rig.ctx.db(), |tx| {
            map::enqueue(
                tx,
                &old,
                None,
                &Op::BlobWant {
                    blob: "cc".repeat(32),
                },
            )
        })
        .unwrap();
        rig.seal().expect("封出第三个段");
        let report = rig.upload().await;
        assert!(report.rotate.is_some() && report.uploaded.is_none());

        let rotated = rig.rotate().await;
        assert_eq!(rotated.old, old);
        assert!(!rotated.from_db);
        assert_ne!(rotated.identity.device, old);
        assert_eq!(
            rotated.identity.my_ids,
            [rotated.identity.device.clone(), old.clone()]
        );
        assert_eq!(rig.secrets.device_id, rotated.identity.device);
        assert_eq!(
            Secrets::load(&rig.ctx.paths).unwrap().device_id,
            rotated.identity.device
        );

        // 分叉点之后的两个段（第二个已经上传过、第三个还没传）里的 op 回到了队列里。
        let outbox = rig.outbox();
        let blobs: Vec<String> = outbox
            .iter()
            .map(|(_, _, op)| op["blob"].as_str().unwrap()[..2].to_string())
            .collect();
        assert_eq!(blobs, ["bb", "cc"]);
        assert!(outbox
            .iter()
            .all(|(_, hlc, op)| hlc.ends_with(&old) && op.get("hlc").is_none()));

        db::read(rig.ctx.db(), |conn| {
            assert_eq!(db::log_state(conn)?, LogState::default());
            assert!(seal::seg_entries(conn)?.is_empty());
            // 旧身份的日志从分叉点起当作别的设备的读。
            let peer = db::peer_state(conn, &old)?;
            assert_eq!((peer.fetched, peer.processed), (first.to, first.to));
            assert_eq!(peer.fetched_seg, Some(first.seg_ref()));
            assert_eq!(peer.own_until, Some(first.to));
            assert_eq!(
                peer.replaced_by.as_deref(),
                Some(rotated.identity.device.as_str())
            );
            let record = db::device_record(conn)?.unwrap();
            assert_eq!(record.past, [old.clone()]);
            assert_eq!(record.announce, [old.clone()]);
            Ok(())
        })
        .unwrap();
        assert!(!rig.ctx.paths.outgoing.join(&second.name).exists());

        // 新身份声明它取代旧身份，日志从 1 开始。
        assert!(announce(&rig.ctx, &rig.settings, "test", "0.0.0").unwrap());
        let seg = rig.seal().expect("新身份的第一个段");
        assert_eq!((seg.from, seg.to), (1, 3));
        let report = rig.upload().await;
        assert!(report.head_written && report.rotate.is_none());
    }

    #[tokio::test]
    async fn rotation_republishes_from_the_database_when_segment_files_are_gone() {
        let mut rig = rig();
        let old = rig.identity.device.clone();
        let sessions = rig.ctx.env.sessions.clone();
        sessions
            .append("desktop", "c1", &Message::plain(Role::User, "一"))
            .unwrap();
        sessions
            .append("desktop", "c1", &Message::plain(Role::Assistant, "二"))
            .unwrap();
        sessions.set_session_title("desktop", "c1", "行程").unwrap();
        publish::scan_sessions(&rig.ctx, &rig.settings, true).unwrap();
        let seg = rig.seal().expect("封出一个段");
        // 段文件丢了，也没传上去过。
        std::fs::remove_file(rig.ctx.paths.outgoing.join(&seg.name)).unwrap();

        let rotated = rig.rotate().await;
        assert!(rotated.from_db);
        // 属性带着原来的 HLC 回到队列；消息由下一次全量扫描重新排队。
        let outbox = rig.outbox();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].2["t"], "session.set");
        assert!(outbox[0].1.ends_with(&old));
        publish::scan_sessions(&rig.ctx, &rig.settings, true).unwrap();
        let kinds: Vec<String> = rig
            .outbox()
            .iter()
            .map(|(_, _, op)| op["t"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(kinds, ["session.set", "session.open", "msg", "msg"]);
    }
}
