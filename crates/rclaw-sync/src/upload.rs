//! 上传（§9.3 第 3–7 步）：落盘点、身份自检、传段、打汇总包、写 head、记账。
//!
//! 顺序不能换。写远端之前本机状态必须已经真正落盘（第 3 步）：否则一次断电就可能让
//! 本机忘掉自己上传过的段和发出去的回执，远端反而比本机「新」。先写段或汇总包，再写
//! head；读者只信 head，所以任何时刻崩溃，最多留下一个没人引用的对象。
//!
//! 一轮的请求数：读 head、写段、核对段、写 head、读回 head，共 5 个。没有东西要写的
//! 时候这里一个请求也不发。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::attach::now_ms;
use crate::codec::{self, Head, HeadSeg, RollupLink, RollupRef, SegRef};
use crate::crypto::{sha256_hex, Keys};
use crate::db::{self, scope, LogState};
use crate::error::{Result, SyncError};
use crate::ids;
use crate::op::FORMAT_VERSION;
use crate::remote;
use crate::seal::{self, SegEntry};
use crate::transport::{self, SyncTransport};
use crate::{blocking, offload, Ctx};

/// 只有回执变了而没有新段时，head 的更新最多推迟这么久，攒成一次写入。
pub const ACK_DEFER_MS: i64 = 10 * 60 * 1000;
/// 打汇总包要用的段，本机没有、存储上的那一份也不对：隔这么久再去取一次。每一轮都取
/// 只是白费流量。
const PACK_HOLD_MS: i64 = 60 * 60 * 1000;
/// 同一台设备两次写 head 至少间隔这么久：有的存储修改时间只精确到秒，间隔太短别的
/// 设备看不出 head 变了。
const HEAD_SPACING_MS: i64 = 2000;
/// 已上传的段文件始终保留最近这么多个（换身份时重新发布要用，§9.6）。
const KEEP_OUTGOING: usize = 8;
/// 一轮里最多打这么多个汇总包。
const PACKS_PER_ROUND: usize = 8;
/// head 里的运行提示最多带这么多个会话。
pub const RUNNING_MAX: usize = 64;

/// 本机写出的一个汇总包（`sync_state` 的 `pack`，键是 12 位补零的止序号）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackEntry {
    pub name: String,
    /// 汇总包明文的摘要。
    pub digest: String,
    pub from: u64,
    pub to: u64,
    /// 包里最后一个段。
    pub last: SegRef,
}

impl PackEntry {
    pub fn rollup_ref(&self) -> RollupRef {
        RollupRef {
            name: self.name.clone(),
            digest: self.digest.clone(),
            to: self.to,
            last: self.last.clone(),
        }
    }
}

fn pack_key(to: u64) -> String {
    format!("{to:012}")
}

pub fn pack_entries(conn: &Connection) -> Result<Vec<PackEntry>> {
    Ok(db::state_scan(conn, scope::PACK, "")?
        .into_iter()
        .filter_map(|(_, row)| row.json())
        .collect())
}

pub fn put_pack_entry(conn: &Connection, entry: &PackEntry) -> Result<()> {
    db::state_put_json(conn, scope::PACK, &pack_key(entry.to), db::HLC_ZERO, entry)
}

pub fn del_pack_entry(conn: &Connection, to: u64) -> Result<()> {
    db::state_del(conn, scope::PACK, &pack_key(to))
}

/// 身份自检的结论（§9.6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfCheck {
    /// 存储上的 head 与本机记录一致。
    Ok,
    /// 比本机记录多出的正好是那个「已封未传」的段：上次 head 已经写出，本机没来得及
    /// 记账就退出了。补记账，继续。
    Adopt,
    /// 停在本机更早上传过的某个段上，或者 head 不见了而本机上传过：存储被回退或清空。
    Regressed(String),
    /// 末尾是本机不认识的段，或实例号不是本机用过的：本机的库被回退过，或另有一份
    /// 复制品在写。
    Rotate(String),
}

/// 拿存储上的 `heads/<自己>` 与本机记录比三样：日志末尾的序号、最后一个段、实例号。
/// `instances` 是这个库里的引擎用过的实例号。
pub fn self_check(
    log: &LogState,
    entries: &[SegEntry],
    instances: &[String],
    head: Option<&Head>,
) -> SelfCheck {
    let Some(head) = head else {
        return if log.head_ts == 0 && log.up_seq == 0 {
            SelfCheck::Ok
        } else {
            SelfCheck::Regressed("存储上找不到本机写过的 head".into())
        };
    };
    let known = instances.contains(&head.instance)
        || (!log.head_instance.is_empty() && log.head_instance == head.instance);
    // 实例号不认识有两种来历：另有一份数据在写（要换身份），或者存储被回退到了本机很久以前
    // 写的 head——本机只记着最近的一批实例号，那时的已经不在了。后一种的 head 一定比本机
    // 最后一次写的旧（同一台设备的时钟），这时接着往下比内容：对得上本机的日志就按「存储
    // 被回退」报，不自动换身份；对不上照样换。另一份数据所在的设备时钟偏慢时也可能先走到
    // 这里，它再写一次 head、或者封出一个段，就会被认出来。
    if !known && head.ts >= log.head_ts {
        return SelfCheck::Rotate("存储上本机名下的 head 不是这份数据写的（实例号对不上）".into());
    }
    if head.last_seq == log.up_seq && head.last_seg == log.up_seg {
        return SelfCheck::Ok;
    }
    if let Some(pending) = &log.pending {
        if head.last_seq == pending.to && head.last_seg.as_ref() == Some(&pending.seg_ref()) {
            return SelfCheck::Adopt;
        }
    }
    if head.last_seq < log.up_seq {
        let regressed = || {
            SelfCheck::Regressed(format!(
                "存储上本机的日志停在序号 {}，而本机已经上传到 {}",
                head.last_seq, log.up_seq
            ))
        };
        return match entries.iter().find(|entry| entry.to == head.last_seq) {
            Some(entry) if head.last_seg.as_ref() == Some(&entry.seg_ref()) => regressed(),
            Some(_) => SelfCheck::Rotate("存储上本机名下的日志与本机上传过的不是同一条".into()),
            // 那一段本机已经回收、核对不了：按存储被回退处理（不自动修复）。
            None if head.last_seq <= log.floor => regressed(),
            None => SelfCheck::Rotate("存储上本机名下的日志停在一个本机不认识的段上".into()),
        };
    }
    SelfCheck::Rotate("存储上本机名下的日志末尾不是这份数据写的".into())
}

pub struct UploadOptions<'a> {
    pub keys: &'a Arc<Keys>,
    pub device: &'a str,
    /// 引擎这次启动的实例号（已经在本机登记过）。
    pub instance: &'a str,
    /// 这个库里的引擎用过的实例号。
    pub instances: &'a [String],
    /// 运行提示：正在本机运行的会话（线程 id），已排序去重。
    pub running: Vec<String>,
    /// 一个汇总包的明文上限（字节）。
    pub pack_plain_max: usize,
    /// 零散段攒到多少个就打汇总包。
    pub rollup_every: usize,
    /// 不等回执攒够时间，这一轮就写 head。
    pub force_head: bool,
}

#[derive(Debug, Default)]
pub struct UploadReport {
    /// 这一轮传上去的段。
    pub uploaded: Option<HeadSeg>,
    pub head_written: bool,
    /// 身份自检的结论是要换身份（原因）。这一轮什么都没传。
    pub rotate: Option<String>,
    /// 存储被回退或清空（原因）。这一轮什么都没传。
    pub regressed: Option<String>,
}

/// 这一轮有没有东西要写到存储上。
fn head_due(log: &LogState, opts: &UploadOptions<'_>, now: i64) -> bool {
    log.pending.is_some()
        // 还没写过 head：先报个到。别的设备回收日志时要等本机的回执，得让它们知道有本机。
        || log.head_ts == 0
        || opts.force_head
        || log.floor != log.head_floor
        || log.snap.as_ref().map(|snap| &snap.name) != log.head_snap.as_ref()
        || opts.running != log.head_running
        || (log.head_dirty && (now - log.head_ts >= ACK_DEFER_MS || now < log.head_ts))
}

/// 存储上本机名下的 head。
enum Remote {
    Missing,
    /// 读到了但解不开：多半是上次写到一半断了（有的服务端不是原子替换）。
    Broken,
    Head(Box<Head>),
}

/// 第 3–7 步。没有东西要写时直接返回，不发任何请求。
///
/// 读写库、删文件都放到阻塞线程池上（[`offload`]）：库锁是和会话读写共用的，引擎的线程
/// 不在它上面等。
pub async fn upload_round(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    opts: &UploadOptions<'_>,
) -> Result<UploadReport> {
    let mut report = UploadReport::default();
    let log = offload(ctx, |ctx| db::read(ctx.db(), db::log_state)).await?;
    if !head_due(&log, opts, now_ms()) {
        return Ok(report);
    }

    // 3. 落盘点。WAL 是顺序日志，fsync 一次，此前的所有提交（刚封的段、要写进 head 的
    // 状态）就都落盘了。在库锁之外做，失败就什么都不往存储上写。
    offload(ctx, |ctx| db::flush(ctx.db())).await?;

    // 4. 身份自检。
    let remote_head = match remote::read_head(transport, opts.keys, opts.device).await {
        Ok(Some(head)) => Remote::Head(Box::new(head)),
        Ok(None) => Remote::Missing,
        Err(SyncError::Corrupt(reason)) => {
            // 自己的 head 解不开：重写一份。真有另一个写者的话，它的 head 会带着本机
            // 不认识的实例号出现，下一轮照样查得出来。
            tracing::warn!("存储上本机的 head 校验不过（{reason}），重新写一份");
            Remote::Broken
        }
        Err(error) => return Err(error),
    };
    let (mut log, entries) = offload(ctx, |ctx| {
        db::read(ctx.db(), |conn| {
            Ok((db::log_state(conn)?, seal::seg_entries(conn)?))
        })
    })
    .await?;
    let verdict = match &remote_head {
        Remote::Broken => SelfCheck::Ok,
        Remote::Missing => self_check(&log, &entries, opts.instances, None),
        Remote::Head(head) => self_check(&log, &entries, opts.instances, Some(head.as_ref())),
    };
    match verdict {
        SelfCheck::Ok => {}
        SelfCheck::Adopt => {
            if let Remote::Head(head) = &remote_head {
                let head = head.clone();
                log = offload(ctx, move |ctx| adopt(ctx, &head)).await?;
                if !head_due(&log, opts, now_ms()) {
                    return Ok(report);
                }
            }
        }
        SelfCheck::Regressed(reason) => {
            report.regressed = Some(reason);
            return Ok(report);
        }
        SelfCheck::Rotate(reason) => {
            report.rotate = Some(reason);
            return Ok(report);
        }
    }

    // 5. 上传段，然后核对远端对象的长度。
    if let Some(pending) = &log.pending {
        let file = ctx.paths.outgoing.join(&pending.name);
        let len = match tokio::fs::metadata(&file).await {
            Ok(meta) => meta.len(),
            Err(_) => {
                // 登记过的段文件不见了：这批 op 已经出了队列，只能在新身份下从库里重新
                // 发布（§9.6 的做法，重复的靠幂等消化）。
                report.rotate = Some("本机已封好的段文件丢失".into());
                return Ok(report);
            }
        };
        let path = codec::segment_path(opts.device, &pending.name);
        transport.put_file(&path, &file).await?;
        transport::check_len(transport, &path, len).await?;
        report.uploaded = Some(pending.clone());
    }

    // 零散段攒够了：先写汇总包，再写 head。
    let packed = make_rollups(ctx, transport, opts).await?;

    // 6. 上传 head，然后读回来核对。
    let (device, instance, running) = (
        opts.device.to_string(),
        opts.instance.to_string(),
        opts.running.clone(),
    );
    let head = offload(ctx, move |ctx| {
        db::read(ctx.db(), |conn| {
            build_head(conn, &device, &instance, &running)
        })
    })
    .await?;
    head.check(opts.device)?;
    let since = now_ms() - log.head_ts;
    if (0..HEAD_SPACING_MS).contains(&since) {
        tokio::time::sleep(Duration::from_millis((HEAD_SPACING_MS - since) as u64)).await;
    }
    let head = Head {
        ts: now_ms(),
        ..head
    };
    remote::write_head(transport, opts.keys, &head).await?;
    report.head_written = true;

    // 7. 记账。
    offload(ctx, move |ctx| {
        db::write(ctx.db(), |tx| {
            let mut log = db::log_state(tx)?;
            log.pending = None;
            mark_uploaded(tx, head.last_seq)?;
            log.up_seq = head.last_seq;
            log.up_seg = head.last_seg.clone();
            log.head_ts = head.ts;
            log.head_dirty = false;
            log.head_instance = head.instance.clone();
            log.head_floor = head.floor;
            log.head_snap = head.snap.as_ref().map(|snap| snap.name.clone());
            log.head_packed = head.rollup.as_ref().map_or(0, |rollup| rollup.to);
            log.head_running = head.running.clone();
            db::put_log_state(tx, &log)
        })?;
        if packed {
            prune_outgoing(ctx)?;
        }
        Ok(())
    })
    .await?;
    Ok(report)
}

/// 把序号不大于 `seq` 的段都标成「已上传」。
fn mark_uploaded(conn: &Connection, seq: u64) -> Result<()> {
    for mut entry in seal::seg_entries(conn)? {
        if !entry.up && entry.to <= seq {
            entry.up = true;
            seal::put_seg_entry(conn, &entry)?;
        }
    }
    Ok(())
}

/// 补记账：上次 head 已经写出（带着那个「已封未传」的段），本机没来得及记下。
fn adopt(ctx: &Ctx, head: &Head) -> Result<LogState> {
    db::write(ctx.db(), |tx| {
        let mut log = db::log_state(tx)?;
        log.pending = None;
        mark_uploaded(tx, head.last_seq)?;
        log.up_seq = head.last_seq;
        log.up_seg = head.last_seg.clone();
        log.head_instance = head.instance.clone();
        log.head_floor = head.floor;
        log.head_snap = head.snap.as_ref().map(|snap| snap.name.clone());
        log.head_packed = head.rollup.as_ref().map_or(0, |rollup| rollup.to);
        log.head_running = head.running.clone();
        // 那份 head 是什么时候写的不知道：当作刚写过（保证间隔），其余内容（回执等）
        // 过一阵按本机的记录重写一份。
        log.head_ts = now_ms();
        log.head_dirty = true;
        db::put_log_state(tx, &log)?;
        Ok(log)
    })
}

/// 按本机的记录拼出 head（`ts` 由调用方在写出前填）。`running` 是运行提示。
fn build_head(conn: &Connection, device: &str, instance: &str, running: &[String]) -> Result<Head> {
    let log = db::log_state(conn)?;
    let rollup = log.rollup.clone().filter(|rollup| rollup.to > log.floor);
    let packed_to = rollup.as_ref().map_or(log.floor, |rollup| rollup.to);
    let segs: Vec<HeadSeg> = seal::seg_entries(conn)?
        .iter()
        .filter(|entry| !entry.packed && entry.to > packed_to)
        .map(SegEntry::head_seg)
        .collect();
    // 快照的版本向量给的、还没在那台设备的日志上核对过的位置不当回执发：它可能根本不在
    // 那条日志上，对方照着它回收日志的话，本机没读过的那一段就没了（§9.5）。
    let acks: BTreeMap<String, u64> = db::peers(conn)?
        .into_iter()
        .filter(|(_, peer)| peer.processed > 0 && !peer.from_snap)
        .map(|(device, peer)| (device, peer.processed))
        .collect();
    let mut running = running.to_vec();
    running.truncate(RUNNING_MAX);
    Ok(Head {
        v: FORMAT_VERSION,
        dev: device.to_string(),
        instance: instance.to_string(),
        last_seq: log.seq,
        last_seg: log.last.clone(),
        segs,
        rollup,
        floor: log.floor,
        acks,
        running,
        snap: log.snap.clone(),
        ts: 0,
    })
}

struct Built {
    pack: PackEntry,
    object: Vec<u8>,
    /// 打进包里的段（止序号）。
    packed: Vec<u64>,
}

/// 用本机留着的段文件拼一个汇总包：从 `loose` 的开头取，明文总量不超过 `plain_max`
/// （至少取一个）。
fn build_pack(
    keys: &Keys,
    outgoing: &Path,
    device: &str,
    loose: &[SegEntry],
    prev: Option<RollupLink>,
    plain_max: usize,
) -> Result<Built> {
    let mut segments: Vec<(HeadSeg, Vec<u8>)> = Vec::new();
    let mut total = 0usize;
    for entry in loose {
        let file = outgoing.join(&entry.name);
        let object = std::fs::read(&file)?;
        let opened = codec::open_log_object(
            keys,
            &codec::segment_path(device, &entry.name),
            &object,
            codec::SEGMENT_PLAIN_MAX,
        )
        .ok()
        .filter(|plain| sha256_hex(plain) == entry.digest);
        let Some(plain) = opened else {
            // 本机这一份坏了。它已经传上去过，删掉，下一轮从存储上取一份对的回来；留着
            // 的话以后每一轮都卡在它上面。
            let _ = std::fs::remove_file(&file);
            return Err(SyncError::other(format!(
                "本机的段文件 {} 与登记的不符，下一轮从存储上重新取回",
                entry.name
            )));
        };
        if !segments.is_empty() && total + plain.len() > plain_max {
            break;
        }
        total += plain.len();
        segments.push((entry.head_seg(), plain));
    }
    let (Some((first, _)), Some((last, _))) = (segments.first(), segments.last()) else {
        return Err(SyncError::other("没有可以打包的段"));
    };
    let (from, to, last_ref) = (first.from, last.to, last.seg_ref());
    let packed: Vec<u64> = segments.iter().map(|(seg, _)| seg.to).collect();
    let plain = codec::rollup_plaintext(device, &segments, prev)?;
    drop(segments);
    let pack = PackEntry {
        name: ids::rollup_name(from, to),
        digest: sha256_hex(&plain),
        from,
        to,
        last: last_ref,
    };
    let object = codec::seal_log_object(keys, &codec::rollup_path(device, &pack.name), &plain);
    Ok(Built {
        pack,
        object,
        packed,
    })
}

/// 零散段攒到汇总粒度就打包（§9.1）。返回这一轮有没有打出汇总包。
///
/// 打包失败不拦着这一轮写 head——head 的零散段清单还装得下；一直失败，清单长到上限时
/// 写 head 的自检会报错。
async fn make_rollups(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    opts: &UploadOptions<'_>,
) -> Result<bool> {
    let mut made = false;
    for _ in 0..PACKS_PER_ROUND {
        let (log, loose) = offload(ctx, |ctx| {
            db::read(ctx.db(), |conn| {
                let loose: Vec<SegEntry> = seal::seg_entries(conn)?
                    .into_iter()
                    .filter(|entry| !entry.packed)
                    .collect();
                Ok((db::log_state(conn)?, loose))
            })
        })
        .await?;
        if loose.len() < opts.rollup_every.max(1) || pack_held(&log, now_ms()) {
            break;
        }
        match restore_missing(ctx, transport, opts, &loose).await {
            Ok(None) => {}
            Ok(Some(reason)) => {
                tracing::warn!("生成汇总包要用的段取不回一份对的（{reason}），过一小时再试");
                offload(ctx, |ctx| {
                    db::write(ctx.db(), |tx| {
                        let mut log = db::log_state(tx)?;
                        log.pack_hold_ms = now_ms() + PACK_HOLD_MS;
                        db::put_log_state(tx, &log)
                    })
                })
                .await?;
                break;
            }
            Err(error) => {
                tracing::warn!("生成汇总包失败（下一轮再试）: {error}");
                break;
            }
        }
        match make_rollup(ctx, transport, opts, &log, loose).await {
            Ok(()) => made = true,
            Err(error) => {
                tracing::warn!("生成汇总包失败（下一轮再试）: {error}");
                break;
            }
        }
    }
    Ok(made)
}

/// 上次从存储上取回来的段不对，还没到再试的时候。记下的时刻比「现在 + 时限」还晚的不
/// 算（时钟被拨回去过）。
fn pack_held(log: &LogState, now: i64) -> bool {
    (1..=PACK_HOLD_MS).contains(&(log.pack_hold_ms - now))
}

/// 打包要用的段文件，本机没有了的（目录被清过、上一轮发现坏了删掉的）从存储上取回来。
/// 核对过是登记的那个段才留下：存下一份不对的，以后每一轮打包都卡在它上面，也不会再去
/// 取。返回 `Some(原因)` = 存储上的那一份不在了或者不对。
async fn restore_missing(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    opts: &UploadOptions<'_>,
    loose: &[SegEntry],
) -> Result<Option<String>> {
    for entry in loose {
        let file = ctx.paths.outgoing.join(&entry.name);
        if tokio::fs::metadata(&file).await.is_ok() {
            continue;
        }
        let path = codec::segment_path(opts.device, &entry.name);
        let object = match transport::get_opt(transport, &path, codec::SEGMENT_OBJECT_MAX)
            .await
            .map_err(SyncError::from)
        {
            Ok(Some(object)) => object,
            Ok(None) => return Ok(Some(format!("存储上找不到 {path}"))),
            // 比一个段该有的还大。
            Err(SyncError::Corrupt(reason)) => return Ok(Some(reason)),
            Err(error) => return Err(error),
        };
        let (keys, digest) = (opts.keys.clone(), entry.digest.clone());
        let wrong = blocking(move || {
            match codec::open_log_object(&keys, &path, &object, codec::SEGMENT_PLAIN_MAX) {
                Ok(plain) if sha256_hex(&plain) == digest => {}
                Ok(_) => return Ok(Some(format!("{path} 与本机登记的摘要不符"))),
                Err(error) => return Ok(Some(error.to_string())),
            }
            rclaw_store::atomic_write(&file, &object)?;
            Ok(None)
        })
        .await?;
        if wrong.is_some() {
            return Ok(wrong);
        }
    }
    Ok(None)
}

/// 打一个汇总包。段文件都在本机（[`restore_missing`] 补齐过）。
async fn make_rollup(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    opts: &UploadOptions<'_>,
    log: &LogState,
    loose: Vec<SegEntry>,
) -> Result<()> {
    let Some(first) = loose.first() else {
        return Ok(());
    };
    // 上一个汇总包必须正好接在前面；已经被回收掉的不再指向。
    let prev = match &log.rollup {
        Some(rollup) if rollup.to + 1 == first.from => Some(RollupLink {
            name: rollup.name.clone(),
            digest: rollup.digest.clone(),
            to: rollup.to,
        }),
        Some(rollup) if rollup.to > log.floor => {
            return Err(SyncError::other("零散段没有接在上一个汇总包后面"));
        }
        _ => None,
    };
    let (keys, outgoing, device) = (
        opts.keys.clone(),
        ctx.paths.outgoing.clone(),
        opts.device.to_string(),
    );
    let plain_max = opts.pack_plain_max;
    let Built {
        pack,
        object,
        packed,
    } = blocking(move || build_pack(&keys, &outgoing, &device, &loose, prev, plain_max)).await?;
    let path = codec::rollup_path(opts.device, &pack.name);
    transport::put_checked(transport, &path, object).await?;
    // 回读核对：只确认远端长度不够——同长度的坏对象一旦进 head，零散段就会被回收，
    // 包里是这段日志唯一的副本。解密、核对明文摘要、解析包头与逐段摘要都通过才算传好。
    let object = transport
        .get(&path, codec::ROLLUP_OBJECT_MAX)
        .await
        .map_err(SyncError::from)?;
    let (keys, device, digest, read_path) = (
        opts.keys.clone(),
        opts.device.to_string(),
        pack.digest.clone(),
        path.clone(),
    );
    let verified = blocking(move || -> Result<bool> {
        let plain = codec::open_log_object(&keys, &read_path, &object, codec::ROLLUP_PLAIN_MAX)?;
        if sha256_hex(&plain) != digest {
            return Ok(false);
        }
        Ok(codec::parse_rollup(&plain, &device).is_ok())
    })
    .await?;
    if !verified {
        return Err(SyncError::corrupt(format!(
            "汇总包 {path} 回读核对不通过，不能进 head"
        )));
    }
    // 汇总包已经在存储上了：登记它，把这些段标成已打包。head 这一轮稍后就写。
    offload(ctx, move |ctx| {
        db::write(ctx.db(), |tx| {
            put_pack_entry(tx, &pack)?;
            for mut entry in seal::seg_entries(tx)? {
                if !entry.packed && packed.contains(&entry.to) {
                    entry.packed = true;
                    seal::put_seg_entry(tx, &entry)?;
                }
            }
            let mut log = db::log_state(tx)?;
            log.rollup = Some(pack.rollup_ref());
            db::put_log_state(tx, &log)
        })
    })
    .await
}

/// 已经打进汇总包的段，本机的密文文件可以删了；始终保留最近 [`KEEP_OUTGOING`] 个。
pub fn prune_outgoing(ctx: &Ctx) -> Result<()> {
    let entries = db::read(ctx.db(), seal::seg_entries)?;
    let keep_from = entries.len().saturating_sub(KEEP_OUTGOING);
    for entry in &entries[..keep_from] {
        if entry.packed {
            let _ = std::fs::remove_file(ctx.paths.outgoing.join(&entry.name));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map;
    use crate::op::Op;
    use crate::seal::SealOptions;
    use crate::transport::file::FileTransport;

    struct Rig {
        _dir: tempfile::TempDir,
        _store: tempfile::TempDir,
        ctx: Ctx,
        transport: FileTransport,
        keys: Arc<Keys>,
        device: String,
        instance: String,
    }

    fn rig() -> Rig {
        let (dir, ctx) = crate::testing::ctx();
        let device = db::write(ctx.db(), |tx| db::ensure_device(tx)).unwrap();
        let store = tempfile::tempdir().unwrap();
        Rig {
            transport: FileTransport::new(store.path()),
            _dir: dir,
            _store: store,
            ctx,
            keys: Arc::new(crate::testing::keys()),
            device,
            instance: ids::new_instance_id(),
        }
    }

    impl Rig {
        fn options(&self, rollup_every: usize) -> UploadOptions<'_> {
            UploadOptions {
                keys: &self.keys,
                device: &self.device,
                instance: &self.instance,
                instances: std::slice::from_ref(&self.instance),
                running: Vec::new(),
                pack_plain_max: 1 << 20,
                rollup_every,
                force_head: false,
            }
        }

        /// 排一条 op、封段。
        fn seal_one(&self) -> HeadSeg {
            db::write(self.ctx.db(), |tx| {
                map::enqueue(
                    tx,
                    &self.device,
                    None,
                    &Op::BlobWant {
                        blob: "ab".repeat(32),
                    },
                )
            })
            .unwrap();
            seal::seal_next(
                &self.ctx,
                &SealOptions {
                    keys: &self.keys,
                    device: &self.device,
                    budget: 1 << 20,
                    attachment_max: 1 << 20,
                    upload_attachments: false,
                },
            )
            .unwrap()
            .expect("封出一个段")
        }

        async fn round(&self, rollup_every: usize) -> UploadReport {
            upload_round(&self.ctx, &self.transport, &self.options(rollup_every))
                .await
                .unwrap()
        }

        async fn remote_head(&self) -> Head {
            remote::read_head(&self.transport, &self.keys, &self.device)
                .await
                .unwrap()
                .expect("存储上有 head")
        }

        fn log(&self) -> LogState {
            db::read(self.ctx.db(), db::log_state).unwrap()
        }
    }

    #[tokio::test]
    async fn the_first_round_announces_the_device_and_idle_rounds_write_nothing() {
        let rig = rig();
        let report = rig.round(100).await;
        assert!(report.head_written && report.uploaded.is_none());
        let head = rig.remote_head().await;
        assert_eq!((head.last_seq, head.segs.len()), (0, 0));
        assert_eq!(head.instance, rig.instance);
        // 没有新东西：不再写。
        let report = rig.round(100).await;
        assert!(!report.head_written);
    }

    #[test]
    fn a_position_taken_from_a_snapshot_is_not_acked_until_it_is_checked() {
        let rig = rig();
        let (checked, told) = ("00000000000000b1", "00000000000000b2");
        db::write(rig.ctx.db(), |tx| {
            for (device, from_snap) in [(checked, false), (told, true)] {
                let peer = db::PeerState {
                    fetched: 7,
                    processed: 7,
                    from_snap,
                    ..db::PeerState::default()
                };
                db::put_peer_state(tx, device, &peer)?;
            }
            Ok(())
        })
        .unwrap();
        let head = db::read(rig.ctx.db(), |conn| {
            build_head(conn, &rig.device, &rig.instance, &[])
        })
        .unwrap();
        assert_eq!(head.acks, BTreeMap::from([(checked.to_string(), 7)]));
    }

    #[tokio::test]
    async fn a_segment_file_that_is_missing_or_damaged_is_fetched_back_and_checked() {
        let rig = rig();
        let first = rig.seal_one();
        rig.round(100).await;
        let local = rig.ctx.paths.outgoing.join(&first.name);
        let remote_path = codec::segment_path(&rig.device, &first.name);
        let good = rig
            .transport
            .get(&remote_path, codec::SEGMENT_OBJECT_MAX)
            .await
            .unwrap();

        // 本机的没了，存储那一次答的又不对：不留下，过一阵再取。head 照写。
        std::fs::remove_file(&local).unwrap();
        rig.transport
            .put(&remote_path, b"<html>".to_vec())
            .await
            .unwrap();
        rig.seal_one();
        let report = rig.round(2).await;
        assert!(report.head_written);
        assert!(!local.exists());
        assert!(rig.remote_head().await.rollup.is_none());
        assert!(pack_held(&rig.log(), now_ms()));
        // 时限之内不去取。
        rig.transport.put(&remote_path, good).await.unwrap();
        rig.seal_one();
        rig.round(2).await;
        assert!(!local.exists());
        // 到时候了：取回来的对得上，汇总包打出来了。
        db::write(rig.ctx.db(), |tx| {
            let mut log = db::log_state(tx)?;
            log.pack_hold_ms = now_ms() - 1;
            db::put_log_state(tx, &log)
        })
        .unwrap();
        rig.seal_one();
        rig.round(2).await;
        assert!(local.exists());
        let rolled = |head: Head| head.rollup.map(|rollup| rollup.to);
        assert_eq!(rolled(rig.remote_head().await), Some(4));

        // 本机的一份坏了：删掉，下一轮从存储上取回来再打包。
        let fifth = rig.seal_one();
        rig.round(100).await;
        let local = rig.ctx.paths.outgoing.join(&fifth.name);
        std::fs::write(&local, b"junk").unwrap();
        rig.seal_one();
        rig.round(2).await;
        assert!(!local.exists());
        assert_eq!(rolled(rig.remote_head().await), Some(4));
        rig.seal_one();
        rig.round(2).await;
        assert_eq!(rolled(rig.remote_head().await), Some(7));
    }

    #[tokio::test]
    async fn a_sealed_segment_is_uploaded_listed_and_accounted() {
        let rig = rig();
        let seg = rig.seal_one();
        let report = rig.round(100).await;
        assert_eq!(report.uploaded.as_ref(), Some(&seg));
        let head = rig.remote_head().await;
        assert_eq!(head.segs, vec![seg.clone()]);
        assert_eq!(head.last_seq, 1);
        let log = rig.log();
        assert!(log.pending.is_none());
        assert_eq!((log.up_seq, log.up_seg.clone()), (1, Some(seg.seg_ref())));
        // 存储上的段读得出来、摘要对得上。
        let segment = remote::fetch_segment(
            &rig.transport,
            &rig.keys,
            &rig.device,
            &seg.name,
            &seg.digest,
        )
        .await
        .unwrap();
        assert_eq!(segment.lines.len(), 1);
        let entries = db::read(rig.ctx.db(), seal::seg_entries).unwrap();
        assert!(entries.iter().all(|entry| entry.up));
    }

    #[tokio::test]
    async fn a_head_written_but_not_accounted_is_adopted() {
        let rig = rig();
        rig.round(100).await;
        let seg = rig.seal_one();
        let before = rig.log();
        rig.round(100).await;
        // 模拟写完 head、没来得及记账就退出：把本机的记录拨回去。
        db::write(rig.ctx.db(), |tx| {
            db::put_log_state(tx, &before)?;
            let mut entry = seal::seg_entries(tx)?.pop().unwrap();
            entry.up = false;
            seal::put_seg_entry(tx, &entry)
        })
        .unwrap();
        let report = rig.round(100).await;
        assert!(report.rotate.is_none() && report.regressed.is_none());
        let log = rig.log();
        assert!(log.pending.is_none());
        assert_eq!(log.up_seq, seg.to);
        assert!(db::read(rig.ctx.db(), seal::seg_entries).unwrap()[0].up);
    }

    #[tokio::test]
    async fn loose_segments_are_rolled_up_before_the_head_is_written() {
        let rig = rig();
        let mut segs = Vec::new();
        for _ in 0..4 {
            segs.push(rig.seal_one());
            rig.round(4).await;
        }
        let head = rig.remote_head().await;
        let rollup = head.rollup.clone().expect("打出了汇总包");
        assert_eq!(rollup.to, 4);
        assert!(head.segs.is_empty());
        assert_eq!(head.last_seg, Some(segs[3].seg_ref()));
        assert_eq!(rollup.last, segs[3].seg_ref());
        let pack = remote::fetch_rollup(
            &rig.transport,
            &rig.keys,
            &rig.device,
            &rollup.name,
            &rollup.digest,
        )
        .await
        .unwrap();
        assert_eq!(pack.segments.len(), 4);
        assert!(pack.header.prev.is_none());
        // 下一个汇总包指向它。
        for _ in 0..4 {
            rig.seal_one();
            rig.round(4).await;
        }
        let head = rig.remote_head().await;
        let second = head.rollup.clone().unwrap();
        assert_eq!(second.to, 8);
        let pack = remote::fetch_rollup(
            &rig.transport,
            &rig.keys,
            &rig.device,
            &second.name,
            &second.digest,
        )
        .await
        .unwrap();
        assert_eq!(pack.header.prev.map(|prev| prev.name), Some(rollup.name));
        assert_eq!(db::read(rig.ctx.db(), pack_entries).unwrap().len(), 2);
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

    fn head_at(entry: Option<&SegEntry>, instance: &str) -> Head {
        Head {
            v: FORMAT_VERSION,
            dev: "0123456789abcdef".into(),
            instance: instance.into(),
            last_seq: entry.map_or(0, |entry| entry.to),
            last_seg: entry.map(SegEntry::seg_ref),
            segs: Vec::new(),
            rollup: None,
            floor: 0,
            acks: BTreeMap::new(),
            running: Vec::new(),
            snap: None,
            ts: 1,
        }
    }

    #[test]
    fn self_check_follows_the_table() {
        let mine = "00000000000000aa".to_string();
        let instances = vec![mine.clone()];
        let (a, b, c) = (
            entry(1, 2, "aaaaaaaa"),
            entry(3, 3, "bbbbbbbb"),
            entry(4, 6, "cccccccc"),
        );
        let entries = vec![a.clone(), b.clone(), c.clone()];
        let at_b = LogState {
            seq: 3,
            last: Some(b.seg_ref()),
            up_seq: 3,
            up_seg: Some(b.seg_ref()),
            head_ts: 1,
            ..LogState::default()
        };
        let check =
            |log: &LogState, head: Option<&Head>| self_check(log, &entries, &instances, head);

        // 一致。
        assert_eq!(check(&at_b, Some(&head_at(Some(&b), &mine))), SelfCheck::Ok);
        // 从没写过 head，存储上也没有。
        assert_eq!(check(&LogState::default(), None), SelfCheck::Ok);
        // 多出的正好是「已封未传」的那个段。
        let with_pending = LogState {
            seq: 6,
            last: Some(c.seg_ref()),
            pending: Some(c.head_seg()),
            ..at_b.clone()
        };
        assert_eq!(
            check(&with_pending, Some(&head_at(Some(&c), &mine))),
            SelfCheck::Adopt
        );
        // 停在更早上传过的段上，或 head 不见了。
        assert!(matches!(
            check(&at_b, Some(&head_at(Some(&a), &mine))),
            SelfCheck::Regressed(_)
        ));
        assert!(matches!(check(&at_b, None), SelfCheck::Regressed(_)));
        // 末尾是本机不认识的段。
        assert!(matches!(
            check(&at_b, Some(&head_at(Some(&c), &mine))),
            SelfCheck::Rotate(_)
        ));
        let other = entry(3, 3, "dddddddd");
        assert!(matches!(
            check(&at_b, Some(&head_at(Some(&other), &mine))),
            SelfCheck::Rotate(_)
        ));
        // 同样的序号、更早的位置，却不是本机传过的那个段。
        let other_early = entry(1, 2, "eeeeeeee");
        assert!(matches!(
            check(&at_b, Some(&head_at(Some(&other_early), &mine))),
            SelfCheck::Rotate(_)
        ));
        // 实例号不是本机用过的。
        let foreign = "00000000000000bb";
        assert!(matches!(
            check(&at_b, Some(&head_at(Some(&b), foreign))),
            SelfCheck::Rotate(_)
        ));
        assert!(matches!(
            check(&at_b, Some(&head_at(Some(&a), foreign))),
            SelfCheck::Rotate(_)
        ));
        // 实例号不认识、但比本机最后一次写的 head 还旧：是本机很久以前写的那份回来了。
        // 内容对得上本机的日志就是存储被回退；对不上还是换身份。
        let later = LogState {
            head_ts: 10,
            ..at_b.clone()
        };
        assert!(matches!(
            check(&later, Some(&head_at(Some(&a), foreign))),
            SelfCheck::Regressed(_)
        ));
        assert_eq!(
            check(&later, Some(&head_at(Some(&b), foreign))),
            SelfCheck::Ok
        );
        assert!(matches!(
            check(&later, Some(&head_at(Some(&other_early), foreign))),
            SelfCheck::Rotate(_)
        ));
        assert!(matches!(
            check(&later, Some(&head_at(Some(&c), foreign))),
            SelfCheck::Rotate(_)
        ));
    }
}
