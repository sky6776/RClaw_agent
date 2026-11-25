//! 拉取（§9.3 第 8 步）：列 `heads/`，读有变化的 head，按清单取新段（落后较多时取汇总
//! 包），校验并解密，写入 `sync_inbox`，然后推进「已取回」位置。
//!
//! 读者只信 head。每台设备的日志是一条哈希链：读者按「名字 + 摘要」记住自己取到的最后
//! 一个段，接着读时逐段核对 `prev`。链对不上有两种情况（§9.6 读者一侧）：head 停在
//! 记住的段之前（存储被回退）——位置不动，等它恢复；在某个已知的段之后分了叉（这个
//! 身份出现过两个写者）——从分叉点起重新取回，重复的 op 靠幂等消化。
//!
//! 位置也可能不是读者自己读出来的，而是全量快照的版本向量给的（§9.5，
//! [`PeerState::from_snap`]）。这样的位置要在这条日志上接得上才算数，在那之前不当回执
//! 发出去；接不上就不作数，从日志现存的开头重新读（[`drop_told`]）。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use rclaw_store::sync_db::{self, INBOX_PENDING};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::attach::now_ms;
use crate::codec::{self, Head, HeadSeg, Rollup, RollupLink, SegRef, Segment, SnapRef};
use crate::crypto::Keys;
use crate::db::{self, scope, PeerState};
use crate::error::{Result, SyncError, TransportError};
use crate::op::OpLine;
use crate::remote;
use crate::transport::{ObjectMeta, SyncTransport};
use crate::{offload, Ctx};

/// 一轮拉取里每台设备最多取这么多明文（字节）。取不完的下一轮接着取。
pub const PULL_BUDGET: usize = 32 * 1024 * 1024;
pub const PULL_BUDGET_LOW_MEMORY: usize = 8 * 1024 * 1024;

/// 收件箱里一个段的依赖与取回时刻（`sync_state` 的 `inseg`，键 `<设备>/<段名>`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InSeg {
    #[serde(default)]
    pub deps: BTreeMap<String, u64>,
    /// 取回的时刻（本机时钟，Unix 毫秒）。
    #[serde(default)]
    pub ts: i64,
    #[serde(default)]
    pub to: u64,
}

pub fn inseg_key(device: &str, name: &str) -> String {
    format!("{device}/{name}")
}

pub(crate) fn rseg_key(device: &str, to: u64) -> String {
    format!("{device}/{to:012}")
}

#[derive(Debug, Default)]
pub struct PullReport {
    /// 写进收件箱的 op 数。
    pub ops: usize,
    /// 还有没取完的（预算用尽）：应当马上再来一轮。
    pub more: bool,
    /// 这一轮里 head 有变化的设备数。
    pub changed: usize,
}

/// 本机在某台设备的日志上该从哪里接着读。
#[derive(Debug, PartialEq, Eq)]
enum Position {
    UpToDate,
    /// head 停在记住的位置之前。
    Behind,
    /// 接着读 head 清单里这些段。
    Loose(Vec<HeadSeg>),
    /// 位置比最近的汇总包还早：先读汇总包。
    Packs,
    /// 链对不上：从 `at`（它之前的都还一致）重新读。
    Fork {
        at: u64,
        seg: Option<SegRef>,
    },
    /// 需要的日志已被回收，只能从全量快照起步。
    NeedSnapshot,
}

fn known_segment(conn: &Connection, device: &str, to: u64) -> Result<Option<SegRef>> {
    Ok(db::state_get(conn, scope::RSEG, &rseg_key(device, to))?.and_then(|row| row.json()))
}

/// 在 head 的零散段清单里找分叉点：从新到旧，第一个与本机记录一致的段。
fn fork_point(conn: &Connection, device: &str, head: &Head) -> Result<(u64, Option<SegRef>)> {
    for seg in head.segs.iter().rev() {
        if known_segment(conn, device, seg.to)?.as_ref() == Some(&seg.seg_ref()) {
            return Ok((seg.to, Some(seg.seg_ref())));
        }
    }
    if let Some(rollup) = &head.rollup {
        if known_segment(conn, device, rollup.to)?.as_ref() == Some(&rollup.last) {
            return Ok((rollup.to, Some(rollup.last.clone())));
        }
    }
    // 清单里找不到共同的段：从这条日志现存的开头重新读（重复的 op 靠幂等消化）。
    Ok((head.floor, None))
}

fn locate(conn: &Connection, device: &str, peer: &PeerState, head: &Head) -> Result<Position> {
    if head.floor > peer.fetched {
        return Ok(Position::NeedSnapshot);
    }
    if head.last_seq < peer.fetched {
        return Ok(Position::Behind);
    }
    let fork = |conn: &Connection| -> Result<Position> {
        let (at, seg) = fork_point(conn, device, head)?;
        Ok(Position::Fork { at, seg })
    };
    if head.last_seq == peer.fetched {
        // 日志被回收到末尾时 head 只记着末段，照样能比。
        return if head.last_seg == peer.fetched_seg || peer.fetched_seg.is_none() {
            Ok(Position::UpToDate)
        } else {
            fork(conn)
        };
    }
    let packed_to = head.rollup.as_ref().map_or(head.floor, |rollup| rollup.to);
    if peer.fetched < packed_to {
        return Ok(Position::Packs);
    }
    if peer.fetched == packed_to {
        let consistent = match (&head.rollup, &peer.fetched_seg) {
            (Some(rollup), Some(seg)) => &rollup.last == seg,
            _ => true,
        };
        return if consistent {
            Ok(Position::Loose(head.segs.clone()))
        } else {
            fork(conn)
        };
    }
    // 位置在零散段中间：必须正好停在某个段的末尾，而且就是记住的那个段。
    match head.segs.iter().position(|seg| seg.to == peer.fetched) {
        Some(index)
            if peer
                .fetched_seg
                .as_ref()
                .is_none_or(|seg| seg == &head.segs[index].seg_ref()) =>
        {
            Ok(Position::Loose(head.segs[index + 1..].to_vec()))
        }
        _ => fork(conn),
    }
}

/// 把读取位置挪回 `at`（它之前的都还一致）：它之后的链记录作废，下一轮从那里接着读，
/// 重复的 op 靠幂等消化。在调用方的写事务里执行。
fn rewind(
    conn: &Connection,
    device: &str,
    peer: &mut PeerState,
    at: u64,
    seg: Option<SegRef>,
) -> Result<()> {
    peer.fetched = at;
    peer.fetched_seg = seg;
    peer.processed = peer.processed.min(at);
    peer.pull_incomplete = true;
    conn.execute(
        "DELETE FROM sync_state WHERE scope = ?1 AND key >= ?2 AND key < ?3",
        params![
            scope::RSEG,
            rseg_key(device, at + 1),
            format!("{device}/\u{10ffff}")
        ],
    )?;
    db::put_peer_state(conn, device, peer)
}

/// 一个段里各行 op 的 `(hlc, 所属线程)`，[`ingest`] 写收件箱时要用。段可能有几兆字节，
/// 逐行解析要一阵：在库锁之外调。有一行不是合规的 op 就是段损坏。
pub(crate) fn outline(segment: &Segment) -> Result<Vec<(String, Option<String>)>> {
    segment
        .lines
        .iter()
        .map(|line| OpLine::parse(line).map(|line| (line.hlc, line.sid)))
        .collect()
}

/// 把一个段写进收件箱，并推进「已取回」。一个事务。`outline` 是 [`outline`] 对这个段
/// 算出来的。
pub(crate) fn ingest(
    conn: &Connection,
    device: &str,
    peer: &mut PeerState,
    name: &str,
    digest: &str,
    segment: &Segment,
    outline: &[(String, Option<String>)],
) -> Result<usize> {
    let header = &segment.header;
    if header.from != peer.fetched + 1 {
        return Err(SyncError::corrupt(format!(
            "设备 {device} 的段 {name} 没有接在已取回的位置后面"
        )));
    }
    if let Some(expected) = &peer.fetched_seg {
        if header.prev.as_ref() != Some(expected) {
            return Err(SyncError::corrupt(format!(
                "设备 {device} 的段 {name} 的 prev 与上一段不符"
            )));
        }
    }
    if outline.len() != segment.lines.len() {
        return Err(SyncError::other("段的行数与解析结果对不上"));
    }
    let own_until = peer.own_until.unwrap_or(0);
    let mut stmt = conn.prepare_cached(
        "INSERT INTO sync_inbox(origin, seg, seq, hlc, sid, op, state) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    let mut inserted = 0usize;
    for (index, ((hlc, sid), raw)) in outline.iter().zip(&segment.lines).enumerate() {
        let seq = header.from + index as u64;
        sync_db::hlc_observe(conn, hlc)?;
        if seq <= own_until {
            continue;
        }
        stmt.execute(params![
            device,
            name,
            seq as i64,
            hlc,
            sid,
            raw,
            INBOX_PENDING
        ])?;
        inserted += 1;
    }
    let seg_ref = SegRef {
        name: name.to_string(),
        digest: digest.to_string(),
    };
    db::state_put_json(
        conn,
        scope::INSEG,
        &inseg_key(device, name),
        db::HLC_ZERO,
        &InSeg {
            deps: header.deps.clone(),
            ts: now_ms(),
            to: header.to,
        },
    )?;
    db::state_put_json(
        conn,
        scope::RSEG,
        &rseg_key(device, header.to),
        db::HLC_ZERO,
        &seg_ref,
    )?;
    peer.fetched = header.to;
    peer.fetched_seg = Some(seg_ref);
    // 从记着的位置接上了：快照给的位置到这里算核对过，回执从这时起写进 head。
    if std::mem::take(&mut peer.from_snap) {
        db::mark_head_dirty(conn)?;
    }
    db::put_peer_state(conn, device, peer)?;
    Ok(inserted)
}

/// 从第一次读不下去算起，是不是已经等过了「对象还没到齐」的宽限。记着的时刻在将来
/// （时钟往回拨过）也算等够了。
fn waited_out(peer: &PeerState, now: i64, grace_ms: i64) -> bool {
    peer.error_since_ms != 0 && !(0..grace_ms).contains(&(now - peer.error_since_ms))
}

/// 快照的版本向量给的位置（§9.5）接不上这条日志：那个位置不作数，从日志现存的开头重新
/// 读，重复的 op 靠幂等消化。
///
/// 位置不是本机在这条日志上读出来的。它落在日志之内、链却对不上，就是写快照的设备读的
/// 是这个身份的另一条链，或者向量本身不可信，调用方当场就退。它比日志的末尾还靠后，
/// 可能只是日志还没到齐（目录由别的工具同步时快照会先到），调用方先等过到齐的宽限
/// （[`waited_out`]）再退——照它一直等下去，这条日志长过那个位置之前本机一条也读不到。
async fn drop_told(
    ctx: &Ctx,
    device: &str,
    mut peer: PeerState,
    floor: u64,
    why: &str,
) -> Result<()> {
    tracing::warn!(
        "设备 {device} 的日志接不上快照里记的位置（序号 {}，{why}），从序号 {floor} 之后重新取回",
        peer.fetched
    );
    peer.from_snap = false;
    peer.error = None;
    peer.error_rounds = 0;
    peer.error_since_ms = 0;
    let device = device.to_string();
    offload(ctx, move |ctx| {
        db::write(ctx.db(), |tx| {
            db::mark_head_dirty(tx)?;
            rewind(tx, &device, &mut peer, floor, None)
        })
    })
    .await
}

/// 把这台设备的登记写回库里。
async fn save_peer(ctx: &Ctx, device: &str, peer: &PeerState) -> Result<()> {
    let (device, peer) = (device.to_string(), peer.clone());
    offload(ctx, move |ctx| {
        db::write(ctx.db(), |tx| db::put_peer_state(tx, &device, &peer))
    })
    .await
}

fn pack_cache(ctx: &Ctx, device: &str, name: &str) -> PathBuf {
    ctx.paths.tmp.join(format!("pack-{device}-{name}"))
}

/// 取一个汇总包。沿链往回找的时候会先后读到好几个，密文先落到本机的临时目录：往前
/// 处理时不必再下载一遍，内存里也始终只有一个包。返回包、它是不是这次从存储上取的
/// （缓存命中不占网络预算）、以及这次实际下载的字节数（预算按字节算）。
async fn load_pack(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    device: &str,
    name: &str,
    digest: &str,
) -> Result<(Rollup, bool, usize)> {
    let cache = pack_cache(ctx, device, name);
    let path = codec::rollup_path(device, name);
    let (object, downloaded) = match tokio::fs::read(&cache).await {
        Ok(bytes) => (bytes, false),
        Err(_) => {
            let bytes = transport.get(&path, codec::ROLLUP_OBJECT_MAX).await?;
            // 缓存只是加速：写不上（数据目录临时区没空间、被并发清了）下一轮要重下，
            // 但读取游标的推进不依赖它，说一声就行。
            if let Err(error) = tokio::fs::write(&cache, &bytes).await {
                tracing::debug!("汇总包缓存写不上（下一轮可能要重下）: {error}");
            }
            trim_pack_cache(ctx, &cache);
            (bytes, true)
        }
    };
    let network = downloaded.then(|| object.len()).unwrap_or(0);
    let (keys, device, digest) = (keys.clone(), device.to_string(), digest.to_string());
    let parsed = tokio::task::spawn_blocking(move || -> Result<Rollup> {
        let plain = codec::open_log_object(&keys, &path, &object, codec::ROLLUP_PLAIN_MAX)?;
        if crate::crypto::sha256_hex(&plain) != digest {
            return Err(SyncError::corrupt(format!("{path} 的摘要与指针里记的不符")));
        }
        codec::parse_rollup(&plain, &device)
    })
    .await
    .map_err(|e| SyncError::other(format!("解包任务失败: {e}")))?;
    if parsed.is_err() {
        let _ = tokio::fs::remove_file(&cache).await;
    }
    Ok((parsed?, downloaded, network))
}

/// 汇总包的缓存（`<数据根>/sync/tmp/pack-…`）最多留这么多字节：超了按修改时间从最旧
/// 的开始删，刚写的那个不动。它只是「往前处理时不必再下载」的加速，删掉的下一轮重取。
const PACK_CACHE_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// 缓存总量超了上限就删最旧的缓存文件（`keep` 是刚写的那个）。
fn trim_pack_cache(ctx: &Ctx, keep: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(&ctx.paths.tmp) else {
        return;
    };
    let mut files: Vec<(std::path::PathBuf, u64, std::time::SystemTime)> = entries
        .flatten()
        .filter(|entry| {
            entry.file_type().map(|t| t.is_file()).unwrap_or(false)
                && entry.file_name().to_string_lossy().starts_with("pack-")
        })
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            Some((entry.path(), meta.len(), meta.modified().ok()?))
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, size, _)| *size).sum();
    if total <= PACK_CACHE_MAX_BYTES {
        return;
    }
    files.sort_by_key(|(_, _, modified)| *modified);
    for (path, size, _) in files {
        if total <= PACK_CACHE_MAX_BYTES || path == keep {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

/// 一条汇总包链往回找的进度。跨轮记在 [`Memo`] 里：预算用完停在中途时，下一轮从这里
/// 接着往回找，不把已经确认过的路径再走一遍。
#[derive(Debug, Clone, Default)]
struct PackChain {
    /// 这条链属于哪台设备：缓存文件名按它算（淘汰别的设备的链时不能拿当前设备拼）。
    device: String,
    /// 已知的链：新的在前。
    links: Vec<RollupLink>,
    /// 已经找到「包住本机位置」的那一环（或链的开头）。
    complete: bool,
}

/// 跨轮的汇总包链最多记这么多台设备。
const PACK_CHAINS_MAX: usize = 8;

/// 一轮里回溯最多为新下载这么多汇总包（缓存命中不算）。回溯阶段一分段也交不出去——
/// 本机位置还在更早的包里没找到——字节预算卡不死它（包比预算大、缓存又留不住时，
/// 每轮都会重下同一个包停在原地），改用条数封顶：链的进度跨轮保留，再长的链也终会
/// 走到头。
const PACK_LINKS_PER_ROUND: usize = 16;

struct DevicePull<'a> {
    ctx: &'a Ctx,
    transport: &'a dyn SyncTransport,
    keys: &'a Arc<Keys>,
    device: &'a str,
    budget: usize,
    spent: usize,
    ops: usize,
    /// 汇总包链往回找的进度：跨轮留着（见 [`PackChain`]）。
    pack_chains: &'a mut HashMap<String, PackChain>,
    /// 这一趟读不下去，是因为日志接不上本机记着的位置（位置落在某个段的中间，或者链对
    /// 不上），不是对象坏了、没到齐。
    misfit: bool,
}

impl DevicePull<'_> {
    fn exhausted(&self) -> bool {
        self.spent >= self.budget
    }

    /// 记下一条汇总包链的进度；超过上限时淘汰最旧的一条，它缓存的密文一并清掉。
    fn remember_chain(&mut self, key: String, chain: PackChain) {
        if self.pack_chains.len() >= PACK_CHAINS_MAX && !self.pack_chains.contains_key(&key) {
            if let Some(victim) = self.pack_chains.keys().next().cloned() {
                if let Some(old) = self.pack_chains.remove(&victim) {
                    self.clean_caches(&old);
                }
            }
        }
        self.pack_chains.insert(key, chain);
    }

    /// 清掉一条链各个包在本机临时目录里的密文。缓存文件名按链所属的设备算。
    fn clean_caches(&self, chain: &PackChain) {
        for link in &chain.links {
            let _ = std::fs::remove_file(pack_cache(self.ctx, &chain.device, &link.name));
        }
    }

    async fn ingest_segment(
        &mut self,
        peer: &mut PeerState,
        name: &str,
        digest: &str,
        segment: Segment,
    ) -> Result<()> {
        // 接不接得上由 [`ingest`] 在事务里把关、报错；这里只记下错的种类。
        let header = &segment.header;
        if header.from != peer.fetched + 1
            || peer
                .fetched_seg
                .as_ref()
                .is_some_and(|known| header.prev.as_ref() != Some(known))
        {
            self.misfit = true;
        }
        self.spent += segment.lines.iter().map(String::len).sum::<usize>();
        let ctx = self.ctx.clone();
        let (device, name, digest) = (
            self.device.to_string(),
            name.to_string(),
            digest.to_string(),
        );
        let mut state = peer.clone();
        let (state, inserted) =
            tokio::task::spawn_blocking(move || -> Result<(PeerState, usize)> {
                // 逐行解析在事务之外做：事务里只剩写库。
                let outline = outline(&segment)?;
                let inserted = db::write(ctx.db(), |tx| {
                    ingest(tx, &device, &mut state, &name, &digest, &segment, &outline)
                })?;
                Ok((state, inserted))
            })
            .await
            .map_err(|e| SyncError::other(format!("写收件箱的任务失败: {e}")))??;
        *peer = state;
        self.ops += inserted;
        Ok(())
    }

    /// 接着读零散段。返回是否全部读完。
    async fn pull_loose(&mut self, peer: &mut PeerState, segs: &[HeadSeg]) -> Result<bool> {
        for seg in segs {
            if seg.to <= peer.fetched {
                continue;
            }
            if self.exhausted() {
                return Ok(false);
            }
            let segment = remote::fetch_segment(
                self.transport,
                self.keys,
                self.device,
                &seg.name,
                &seg.digest,
            )
            .await?;
            if (segment.header.from, segment.header.to) != (seg.from, seg.to) {
                return Err(SyncError::corrupt(format!(
                    "设备 {} 的段 {} 与 head 清单不符",
                    self.device, seg.name
                )));
            }
            self.ingest_segment(peer, &seg.name, &seg.digest, segment)
                .await?;
        }
        Ok(true)
    }

    /// 位置比最近的汇总包还早：沿汇总包链往回找到含有本机位置的那一个，再往前逐段处理。
    /// 返回是否把汇总包覆盖的部分全部读完。
    ///
    /// 往回找与往前处理共用这一轮的预算（下载、解包、临时文件）：预算用完就停在中途，
    /// 进度记在 [`Memo`] 里，下一轮接着往回找，不重走已经确认过的链。
    async fn pull_packs(&mut self, peer: &mut PeerState, head: &Head) -> Result<bool> {
        let Some(newest) = &head.rollup else {
            return Ok(true);
        };
        // 链的进度按设备记：来源端打了新包时，新包头指向的环往往已经在走过的链里，接着
        // 用那一段，不必从新包头把整条链再走一遍（每轮都从新包头重走的设备永远到不了
        // 本机位置）。
        let key = self.device.to_string();
        let mut chain = self.pack_chains.remove(&key).unwrap_or_default();
        let newest_link = RollupLink {
            name: newest.name.clone(),
            digest: newest.digest.clone(),
            to: newest.to,
        };
        chain.device = self.device.to_string();
        // 上一次记下的链（新的在前）。新包头指向的环要是就在里面，接着用那一段。
        let mut previous = Some(chain.links.clone());
        let mut same_head = false;
        match chain
            .links
            .iter()
            .position(|link| link.name == newest_link.name && link.digest == newest_link.digest)
        {
            // 这一环已经走过：去掉它前面那些过时的头，保留它和它之后的历史。head 没变
            // 时（index == 0）整条链和完成状态都还有效，不必重新回溯。
            Some(index) => {
                same_head = index == 0;
                chain.links.drain(0..index);
                previous = None;
            }
            None => {
                chain.links.clear();
                chain.links.push(newest_link);
            }
        }
        if !same_head {
            chain.complete = false;
        }
        // 往回找：只记下各包的名字与摘要（密文落在临时目录里，往前处理时不必再下载）。
        // 这一阶段的网络预算与别的部分共享，但每轮至少允许下载一个新包：卡住的位置在
        // 任何可交的段之外，一点字节都不让走的话，「包比预算大、缓存又留不住」的设备
        // 永远动不了。新下载的环数另有条数封顶，链的进度跨轮保留。
        let mut walked = 0usize;
        // 已经在手的定位包：链走到「包住本机位置」那一环时它就在内存里。这一轮即使网络
        // 预算已经用完，也可以直接拿它提交至少一段（缓存写不上时不用下一轮重下）。
        let mut cover: Option<(String, crate::codec::Rollup)> = None;
        while !chain.complete {
            let link = chain.links.last().expect("至少一个").clone();
            let loaded = load_pack(
                self.ctx,
                self.transport,
                self.keys,
                self.device,
                &link.name,
                &link.digest,
            )
            .await;
            let (pack, downloaded, network) = match loaded {
                Ok(loaded) => loaded,
                Err(error) => {
                    // 已经找到的链留着：下一轮从这里接着走，不重走已经下载过的包。
                    self.remember_chain(key, chain);
                    return Err(error);
                }
            };
            if downloaded {
                self.spent += network;
                walked += 1;
            }
            if pack.header.from <= peer.fetched + 1 {
                chain.complete = true;
                cover = Some((link.name.clone(), pack));
                break;
            }
            // 新包头指向的环在上一轮的链里：把那一整段接在后面，不必重新下载。第一次
            // 没接上时保留旧链——往回再走一两个包就可能接上。
            let mut seeded = false;
            if let Some(prev) = pack.header.prev.as_ref() {
                if let Some(old) = previous.as_ref() {
                    if let Some(at) = old
                        .iter()
                        .position(|link| link.name == prev.name && link.digest == prev.digest)
                    {
                        chain.links.extend(old[at..].iter().cloned());
                        seeded = true;
                        previous = None;
                    }
                }
            } else {
                previous = None;
            }
            if seeded {
                // 已经接上了旧链，下一次循环直接处理那一段。
            } else {
                match pack.header.prev {
                    Some(prev) => chain.links.push(prev),
                    // 更早的日志已经被回收：只能从这里读起（`locate` 已确认位置不早于回收点）。
                    None => {
                        chain.complete = true;
                    }
                }
            }
            if walked >= PACK_LINKS_PER_ROUND || (walked > 0 && self.exhausted()) {
                // 这一轮往回找的深度或网络预算到顶：链的进度留着，下一轮接着往回找
                // （当前这一环的回指已经处理过，不会重复下载它）。
                self.remember_chain(key, chain);
                return Ok(false);
            }
        }
        for link in chain.links.iter().rev() {
            // 整包都在本机位置之前的不再下载也不解包：把它的缓存顺手清掉。
            if link.to <= peer.fetched {
                let _ =
                    tokio::fs::remove_file(pack_cache(self.ctx, &chain.device, &link.name)).await;
                continue;
            }
            let preloaded = cover
                .as_ref()
                .filter(|(name, _)| *name == link.name)
                .map(|(_, pack)| pack.clone());
            if preloaded.is_none()
                && self.exhausted()
                && !self.pack_in_cache(&chain.device, &link.name)
            {
                // 停在中途：链已经完整找到了，下一轮直接从链上接着往前处理（已处理过的
                // 段按位置跳过），不必重走回溯。缓存里还有这个包就接着读：读取游标的
                // 推进不依赖网络预算，读缓存不再花网络。
                self.remember_chain(key.clone(), chain.clone());
                return Ok(false);
            }
            let loaded = match preloaded {
                Some(pack) => Ok((pack, false, 0)),
                None => {
                    load_pack(
                        self.ctx,
                        self.transport,
                        self.keys,
                        self.device,
                        &link.name,
                        &link.digest,
                    )
                    .await
                }
            };
            let (pack, downloaded, network) = match loaded {
                Ok(loaded) => loaded,
                Err(error) => {
                    // 链已经完整找到了：留着它，下一轮接着往前处理。
                    self.remember_chain(key.clone(), chain.clone());
                    return Err(error);
                }
            };
            if downloaded {
                self.spent += network;
            }
            let mut ingested_here = 0usize;
            for (seg, segment) in pack.segments {
                if seg.to <= peer.fetched {
                    // 正好停在这个段末尾时核对链：对不上说明这个身份有过两个写者。
                    if seg.to == peer.fetched
                        && peer.fetched_seg.as_ref().is_some_and(|known| {
                            known.name != seg.name || known.digest != seg.digest
                        })
                    {
                        self.misfit = true;
                        // 这条链读不下去：缓存的密文不用留着了。
                        self.clean_caches(&chain);
                        return Err(SyncError::corrupt(format!(
                            "设备 {} 的日志在序号 {} 处与本机取到的不是同一条链",
                            self.device, seg.to
                        )));
                    }
                    continue;
                }
                // 已经下载且校验成功的包至少提交一段：它自己花的网络预算不拦读取游标，
                // 缓存留不留得住也一样。之后到预算用尽就停在这里，链的进度留着，下一轮
                // 从这条链接着往前（已经处理过的段按位置跳过）。
                if ingested_here > 0 && self.exhausted() {
                    self.remember_chain(key.clone(), chain.clone());
                    return Ok(false);
                }
                self.ingest_segment(peer, &seg.name, &seg.digest, segment)
                    .await?;
                ingested_here += 1;
            }
            let _ = tokio::fs::remove_file(pack_cache(self.ctx, self.device, &link.name)).await;
        }
        Ok(true)
    }

    /// 这个包的密文缓存还在不在本机（读取它不花网络预算）。
    fn pack_in_cache(&self, device: &str, name: &str) -> bool {
        std::fs::metadata(pack_cache(self.ctx, device, name)).is_ok()
    }
}

/// 把 head 里的快照指针记进本机：写出时刻不晚于本机第一次见到这个快照的时刻。
///
/// 那个时刻是写者的时钟。时钟快的设备写的快照，照原样记的话在本机眼里会一直是「刚写
/// 的」，谁都不再写新的（§9.5）。`known` 是本机上次记下的指针。
fn seen_snap(known: Option<&SnapRef>, mut snap: SnapRef, now: i64) -> SnapRef {
    let first_seen = match known {
        Some(known) if known.name == snap.name => known.ts,
        _ => now,
    };
    snap.ts = snap.ts.min(first_seen);
    snap
}

/// 「对象还没到齐」之类可以等一等的错误。
fn is_transient(error: &SyncError) -> bool {
    matches!(
        error,
        SyncError::Corrupt(_)
            | SyncError::Transport(
                TransportError::NotFound(_)
                    | TransportError::Timeout(_)
                    | TransportError::RateLimited { .. }
                    | TransportError::Other(_)
            )
    )
}

/// 读一台设备的 head，接着取它的日志。`known` 是本机认不认得这台设备（有没有它的登记）；
/// `grace_ms` 是对象没到齐时可以等多久（[`SyncTransport::arrival_grace`]）。
/// 返回写进收件箱的 op 数，以及是不是还有没取完的。
#[allow(clippy::too_many_arguments)]
async fn pull_device(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    device: &str,
    tag: &str,
    budget: usize,
    known: bool,
    grace_ms: i64,
    chains: &mut HashMap<String, PackChain>,
) -> Result<(usize, bool)> {
    let mut peer = {
        let device = device.to_string();
        offload(ctx, move |ctx| {
            db::read(ctx.db(), |conn| db::peer_state(conn, &device))
        })
        .await?
    };
    let head = match remote::read_head(transport, keys, device).await {
        Ok(Some(head)) => head,
        Ok(None) => return Ok((0, false)),
        Err(SyncError::Upgrade(reason)) => {
            peer.halted = Some(reason);
            peer.head_tag = tag.to_string();
            save_peer(ctx, device, &peer).await?;
            return Ok((0, false));
        }
        Err(e) => return Err(e),
    };
    let now = now_ms();
    peer.instance = head.instance.clone();
    peer.acks = head.acks.clone();
    peer.running = head.running.clone();
    peer.floor = head.floor;
    peer.snap = head
        .snap
        .clone()
        .map(|snap| seen_snap(peer.snap.as_ref(), snap, now));
    peer.head_ts = head.ts;
    // 第一次见到这台设备时，取它上次写 head 的时刻：早就不用了的设备，不该在刚加入的设备
    // 眼里又活跃 60 天（回收日志要等活跃设备的回执，§9.5）。
    peer.seen_ms = if peer.seen_ms == 0 {
        head.ts.min(now).max(1)
    } else {
        now
    };
    if peer
        .halted
        .as_deref()
        .is_some_and(|reason| reason.contains("格式版本"))
    {
        // 这次读得懂了（本机升级过）。
        peer.halted = None;
    }
    let position = {
        let (device, peer, head) = (device.to_string(), peer.clone(), head.clone());
        offload(ctx, move |ctx| {
            if !known {
                // 这个 head 是拿空间的密钥写的：从这一刻起本机认这台设备，之后读不到它的
                // 日志才记账、才报给用户（见 [`pull`]）。「见到它」的时刻留到这一趟读完
                // 再记——没读完就再来的话，仍然按它上次写 head 的时刻算。
                let first = PeerState {
                    seen_ms: 0,
                    ..peer.clone()
                };
                db::write(ctx.db(), |tx| db::put_peer_state(tx, &device, &first))?;
            }
            db::read(ctx.db(), |conn| locate(conn, &device, &peer, &head))
        })
        .await?
    };
    let mut run = DevicePull {
        ctx,
        transport,
        keys,
        device,
        budget,
        spent: 0,
        ops: 0,
        pack_chains: chains,
        misfit: false,
    };
    // 位置正好是日志的末尾：快照给的位置到这里算核对过（见最后记账的那个事务）。
    let at_end = position == Position::UpToDate;
    let read = match position {
        Position::UpToDate => Ok(true),
        Position::Behind => {
            if peer.error_since_ms == 0 {
                peer.error_since_ms = now;
            }
            // 快照给的位置比日志的末尾还靠后：多半只是这份 head 还没到齐，先等。
            if peer.from_snap && waited_out(&peer, now, grace_ms) {
                drop_told(ctx, device, peer, head.floor, "日志比它短").await?;
                return Ok((0, true));
            }
            peer.error_rounds = peer.error_rounds.saturating_add(1);
            if peer.from_snap {
                // 不记版本标识：下一轮照样来看，等够了好往回退。
                peer.error = Some(
                    "全量快照里记的位置比它在存储上现有的日志靠后：多半是日志还没同步完，稍后再读"
                        .into(),
                );
            } else {
                // 本机自己读出来的位置：位置不动，等这份 head 变了再看（§9.6 读者一侧）。
                peer.error = Some(
                    "它在存储上的日志比本机取到过的还短：存储可能被回退了，或者还没同步完".into(),
                );
                peer.head_tag = tag.to_string();
            }
            save_peer(ctx, device, &peer).await?;
            return Ok((0, false));
        }
        Position::NeedSnapshot => {
            peer.error = Some(crate::snapshot::NEED_SNAPSHOT.into());
            if peer.error_since_ms == 0 {
                peer.error_since_ms = now;
            }
            save_peer(ctx, device, &peer).await?;
            return Ok((0, false));
        }
        Position::Fork { at, seg } => {
            tracing::warn!("设备 {device} 的日志在序号 {at} 之后分了叉，从那里重新取回");
            // 分叉点是本机的链记录对得上的位置（没有就是日志现存的开头），不再是快照说的。
            let told = std::mem::take(&mut peer.from_snap);
            let device = device.to_string();
            offload(ctx, move |ctx| {
                db::write(ctx.db(), |tx| {
                    if told {
                        db::mark_head_dirty(tx)?;
                    }
                    rewind(tx, &device, &mut peer, at, seg)
                })
            })
            .await?;
            // 位置已经挪回分叉点，下一轮照常接着读。
            return Ok((0, true));
        }
        Position::Packs => match run.pull_packs(&mut peer, &head).await {
            Ok(true) => run.pull_loose(&mut peer, &head.segs).await,
            other => other,
        },
        Position::Loose(segs) => run.pull_loose(&mut peer, &segs).await,
    };
    let done = match read {
        Ok(done) => done,
        // 快照给的位置落在日志之内，却接不上它后面的那个段（接上过的话记号已经去掉了）：
        // 不是对象坏了、没到齐，是位置本身对不上这条链，没什么可等的。
        Err(SyncError::Corrupt(reason)) if peer.from_snap && run.misfit => {
            drop_told(ctx, device, peer, head.floor, &reason).await?;
            return Ok((0, true));
        }
        Err(error) => return Err(error),
    };
    let ops = run.ops;
    if done {
        peer.head_tag = tag.to_string();
    }
    // 汇总包之前的链记录用不上了（找分叉点只看零散段与包尾）。
    let prune_before = head
        .rollup
        .as_ref()
        .map(|rollup| rseg_key(device, rollup.to));
    let device = device.to_string();
    offload(ctx, move |ctx| {
        db::write(ctx.db(), |tx| {
            if let Some(upper) = &prune_before {
                tx.execute(
                    "DELETE FROM sync_state WHERE scope = ?1 AND key >= ?2 AND key < ?3",
                    params![scope::RSEG, format!("{device}/"), upper],
                )?;
            }
            // 取段的事务里已经写过进度；这里补上 head 自己带的信息。
            let mut latest = db::peer_state(tx, &device)?;
            latest.instance = peer.instance;
            latest.acks = peer.acks;
            latest.running = peer.running;
            latest.floor = peer.floor;
            latest.snap = peer.snap;
            latest.head_ts = peer.head_ts;
            latest.seen_ms = peer.seen_ms;
            latest.halted = peer.halted;
            latest.error = None;
            latest.error_rounds = 0;
            latest.error_since_ms = 0;
            latest.pull_incomplete = !done;
            // 快照给的位置核对过了（接着取到了后面的段的，取段的事务里已经去掉记号）：
            // 回执从这时起写进 head。
            if at_end && std::mem::take(&mut latest.from_snap) {
                db::mark_head_dirty(tx)?;
            }
            latest.head_tag = peer.head_tag;
            db::put_peer_state(tx, &device, &latest)
        })
    })
    .await?;
    Ok((ops, !done))
}

/// 不是哪一台设备的事，是整个存储的事：被拒绝、连接不安全、配置不对、被限流。整轮停下，
/// 由引擎按种类处置（报给用户，或者照服务端给的时间退避）；接着去读别的设备只会再碰一次
/// 壁——被限流的时候还会被罚得更久。
fn whole_storage(error: &SyncError) -> bool {
    matches!(
        error,
        SyncError::Transport(
            TransportError::Auth(_)
                | TransportError::Insecure(_)
                | TransportError::Config(_)
                | TransportError::RateLimited { .. }
        )
    )
}

/// 没认出来的 head 第一次隔多久再读；连着认不出来逐次加倍，到 [`STRANGER_WAIT_MAX_MS`] 为止。
const STRANGER_WAIT_MS: i64 = 15_000;
const STRANGER_WAIT_MAX_MS: i64 = 3_600_000;

/// `heads/` 里一个名字像设备 id、却没被认出来的文件：读不到，或者不是拿这个空间的密钥写
/// 的（也可能只是还没写完）。
#[derive(Debug, Clone)]
struct Stranger {
    /// 没认出来的那一份的版本标识。
    tag: String,
    /// 记下的时刻（本机时钟，Unix 毫秒）与从那时起等多久。
    noted_ms: i64,
    wait_ms: i64,
    /// 同一份连着没认出来的次数。
    tries: u32,
}

/// 拉取在两轮之间记在内存里的东西。
///
/// 存储端不可信：它（或者任何能往那个目录里放文件的人）可以在 `heads/` 里放任意多个名字
/// 像设备 id 的文件。没认出来的 head 因此不进本机的库——不然每个名字永久占一行，面板上
/// 还各有一条「读不到它的同步数据」——只在这里记着：版本标识没变就隔一阵再读，不是每
/// 一轮都读。个数不超过 `heads/` 里列出来的名字数（见 [`remote::list_heads`] 的上限）。
#[derive(Debug, Default)]
pub struct Memo {
    strangers: HashMap<String, Stranger>,
    /// 版本标识不可靠的设备（存储端既没给 ETag 也没给修改时间）上次重读 head 的时刻。
    /// 这些设备的 head 不能靠「版本 + 长度」判重：同长度的新回执会被当成旧值跳过，长期
    /// 漏读会一直挡着日志回收。改为有界地定期 GET，读到之后按解开并校验过的内容判重
    /// （[`pull_device`] 只取还没取过的段，内容没变就是一次空跑）。
    unstable_ms: HashMap<String, i64>,
    /// 汇总包链往回找的进度（见 [`PackChain`]）：预算用完时下一轮接着走，不重走验证过
    /// 的路径。
    pack_chains: HashMap<String, PackChain>,
}

/// 版本标识不可靠的 head 每隔这么久重读一次。
const UNSTABLE_HEAD_RECHECK_MS: i64 = 60 * 1000;

impl Memo {
    /// 不在这次列表里的名字不必再记着。
    fn keep_listed(&mut self, heads: &[ObjectMeta]) {
        if !self.strangers.is_empty() {
            let listed: HashSet<&str> = heads.iter().map(|meta| meta.name.as_str()).collect();
            self.strangers
                .retain(|device, _| listed.contains(device.as_str()));
        }
        if !self.unstable_ms.is_empty() {
            let listed: HashSet<&str> = heads.iter().map(|meta| meta.name.as_str()).collect();
            self.unstable_ms
                .retain(|device, _| listed.contains(device.as_str()));
        }
    }

    /// 版本不可靠的设备现在还不必再读（上次重读还不到间隔）。
    fn unstable_waits(&self, device: &str, now: i64) -> bool {
        self.unstable_ms
            .get(device)
            .is_some_and(|checked| (0..UNSTABLE_HEAD_RECHECK_MS).contains(&(now - checked)))
    }

    fn unstable_read(&mut self, device: &str, now: i64) {
        self.unstable_ms.insert(device.to_string(), now);
    }

    /// 这个没认出来的 head 现在还不必再读：还是那一份，也还没到再试的时刻。
    fn waits(&self, device: &str, tag: &str, now: i64) -> bool {
        self.strangers.get(device).is_some_and(|stranger| {
            stranger.tag == tag && (0..stranger.wait_ms).contains(&(now - stranger.noted_ms))
        })
    }

    /// 记下一个没认出来的 head。
    fn note(&mut self, device: &str, tag: &str, now: i64) {
        let tries = match self.strangers.get(device) {
            Some(stranger) if stranger.tag == tag => stranger.tries.saturating_add(1),
            _ => 1,
        };
        let wait_ms = (STRANGER_WAIT_MS << (tries - 1).min(16)).min(STRANGER_WAIT_MAX_MS);
        self.strangers.insert(
            device.to_string(),
            Stranger {
                tag: tag.to_string(),
                noted_ms: now,
                wait_ms,
                tries,
            },
        );
    }
}

/// 拉取一轮。`me` 是本机当前的设备身份（自己的日志不读）。单台设备出错不影响其他设备；
/// 整个存储的事（[`whole_storage`]）整轮停下。
///
/// 读取失败只给本机认得的设备记账——它的 head 拿空间的密钥解开过，或者别的设备提到过
/// 它（[`db::known_peer`]）。没认出来的只记在 `memo` 里。
pub async fn pull(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    me: &str,
    budget: usize,
    memo: &mut Memo,
) -> Result<PullReport> {
    let heads = remote::list_heads(transport).await?;
    let grace_ms = transport.arrival_grace().as_millis() as i64;
    let mut report = PullReport::default();
    memo.keep_listed(&heads);
    // 本机认得的设备，各自上次读完的那份 head 的版本标识。一次读出来：这一轮里别处不动
    // 它们（[`pull_device`] 只写自己那台设备的登记）。
    let seen: HashMap<String, String> = offload(ctx, |ctx| {
        db::read(ctx.db(), |conn| {
            Ok(db::peers(conn)?
                .into_iter()
                .map(|(device, peer)| (device, peer.head_tag))
                .collect())
        })
    })
    .await?;
    // 一轮的总预算是所有设备共享的：每台只拿自己那一份，免得设备一多就整轮超支。
    let devices = heads.iter().filter(|meta| meta.name != me).count().max(1);
    let per_device = (budget / devices).max(1);
    for meta in heads {
        let device = meta.name.as_str();
        if device == me {
            continue;
        }
        let tag = meta.tag();
        let known = seen.get(device);
        let now = now_ms();
        // 强标识：标识没变就跳过。弱标识（修改时间、弱 ETag、不合规标签）与完全没有
        // 标识的：标识变了立刻读，没变也要按有界窗口定期实读——等长同标识的更新只能
        // 靠重读发现。上一轮没读成（登记里的版本是空的）不算「没变」。
        if meta.stable_version && meta.strong_version {
            match known {
                Some(last) if *last == tag => continue,
                None if memo.waits(device, &tag, now) => continue,
                _ => {}
            }
        } else {
            // 不认识的名字：按陌生设备的退避，别每轮重读。
            if known.is_none() && memo.waits(device, &tag, now) {
                continue;
            }
            let retry = known.is_some_and(|last| last.is_empty());
            let same = known.is_some_and(|last| *last == tag);
            if same && !retry && memo.unstable_waits(device, now) {
                continue;
            }
            memo.unstable_read(device, now);
        }
        report.changed += 1;
        let known = known.is_some();
        let pulled = pull_device(
            ctx,
            transport,
            keys,
            device,
            &tag,
            per_device,
            known,
            grace_ms,
            &mut memo.pack_chains,
        )
        .await;
        let failed = match pulled {
            Ok((ops, more)) => {
                report.ops += ops;
                report.more |= more;
                None
            }
            Err(error) if whole_storage(&error) => return Err(error),
            Err(error) => Some(error),
        };
        // 这一趟下来认出它了没有：head 一解开就登记了（见 [`pull_device`]）。
        let known = known || {
            let device = device.to_string();
            offload(ctx, move |ctx| {
                db::read(
                    ctx.db(),
                    |conn| Ok(db::known_peer(conn, &device)?.is_some()),
                )
            })
            .await?
        };
        if !known {
            if let Some(error) = &failed {
                tracing::debug!("heads/{device} 没认出来: {error}");
            }
            memo.note(device, &tag, now_ms());
            continue;
        }
        memo.strangers.remove(device);
        let Some(error) = failed else { continue };
        let now = now_ms();
        {
            // 目录由第三方工具同步时，head 可能先于它引用的段到达：宽限期内只等。
            let (device, transient, text) =
                (device.to_string(), is_transient(&error), error.to_string());
            offload(ctx, move |ctx| {
                db::write(ctx.db(), |tx| {
                    let mut peer = db::peer_state(tx, &device)?;
                    if peer.error_since_ms == 0 {
                        peer.error_since_ms = now;
                    }
                    peer.error_rounds = peer.error_rounds.saturating_add(1);
                    let waiting = transient && now - peer.error_since_ms < grace_ms;
                    peer.error = (!waiting).then_some(text);
                    db::put_peer_state(tx, &device, &peer)
                })
            })
            .await?;
        }
        tracing::debug!("读取设备 {device} 的日志失败: {error}");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(from: u64, to: u64, tag: &str) -> HeadSeg {
        HeadSeg {
            name: format!("{from:012}-{to:012}-{tag}.seg"),
            digest: "ab".repeat(32),
            from,
            to,
        }
    }

    fn head(device: &str, segs: Vec<HeadSeg>) -> Head {
        Head {
            v: 1,
            dev: device.to_string(),
            instance: "0123456789abcdef".into(),
            last_seq: segs.last().map_or(0, |seg| seg.to),
            last_seg: segs.last().map(HeadSeg::seg_ref),
            segs,
            rollup: None,
            floor: 0,
            acks: BTreeMap::new(),
            running: Vec::new(),
            snap: None,
            ts: 1,
        }
    }

    #[test]
    fn locate_follows_extends_and_detects_rollback_and_fork() {
        let (_g, ctx) = crate::testing::ctx();
        let device = "00112233445566ff";
        let (a, b, c) = (
            seg(1, 2, "aaaaaaaa"),
            seg(3, 3, "bbbbbbbb"),
            seg(4, 6, "cccccccc"),
        );
        db::write(ctx.db(), |tx| {
            db::ensure_device(tx)?;
            for known in [&a, &b] {
                db::state_put_json(
                    tx,
                    scope::RSEG,
                    &rseg_key(device, known.to),
                    db::HLC_ZERO,
                    &known.seg_ref(),
                )?;
            }
            Ok(())
        })
        .unwrap();
        let at_b = PeerState {
            fetched: 3,
            fetched_seg: Some(b.seg_ref()),
            ..PeerState::default()
        };
        db::read(ctx.db(), |conn| {
            // 一致。
            let same = head(device, vec![a.clone(), b.clone()]);
            assert_eq!(locate(conn, device, &at_b, &same)?, Position::UpToDate);
            // 往后延伸。
            let longer = head(device, vec![a.clone(), b.clone(), c.clone()]);
            assert_eq!(
                locate(conn, device, &at_b, &longer)?,
                Position::Loose(vec![c.clone()])
            );
            // 停在记住的段之前。
            let shorter = head(device, vec![a.clone()]);
            assert_eq!(locate(conn, device, &at_b, &shorter)?, Position::Behind);
            // 在 a 之后分了叉。
            let other = seg(3, 5, "dddddddd");
            let forked = head(device, vec![a.clone(), other]);
            assert_eq!(
                locate(conn, device, &at_b, &forked)?,
                Position::Fork {
                    at: 2,
                    seg: Some(a.seg_ref())
                }
            );
            // 从头读。
            assert_eq!(
                locate(conn, device, &PeerState::default(), &longer)?,
                Position::Loose(vec![a.clone(), b.clone(), c.clone()])
            );
            // 需要的日志已被回收。
            let mut reclaimed = head(device, vec![c.clone()]);
            reclaimed.floor = 3;
            assert_eq!(
                locate(conn, device, &PeerState::default(), &reclaimed)?,
                Position::NeedSnapshot
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_snapshot_is_never_recorded_as_written_after_it_was_first_seen() {
        let snap = |name: &str, ts: i64| SnapRef {
            name: name.to_string(),
            digest: "ab".repeat(32),
            ts,
            ops: 7,
        };
        let (one, two) = ("1763438400000-0123abcd", "1763438400001-0123abcd");
        // 写者的时钟不比本机快：照原样记。
        assert_eq!(seen_snap(None, snap(one, 100), 500).ts, 100);
        // 写者的时钟快：记成本机第一次见到它的时刻；以后再读到同一个快照，不跟着往后挪。
        let first = seen_snap(None, snap(one, 9_000), 500);
        assert_eq!(first.ts, 500);
        assert_eq!(seen_snap(Some(&first), snap(one, 9_000), 800).ts, 500);
        // 换了一个快照：重新算。
        assert_eq!(seen_snap(Some(&first), snap(two, 9_000), 800).ts, 800);
        // 别的字段不动。
        assert_eq!((first.name.as_str(), first.ops), (one, 7));
    }

    #[test]
    fn an_unrecognised_head_is_reread_only_now_and_then() {
        let mut memo = Memo::default();
        let (device, tag) = ("00112233445566ff", "v1:10");
        assert!(!memo.waits(device, tag, 1_000));
        memo.note(device, tag, 1_000);
        // 还是那一份：隔一阵再读。
        assert!(memo.waits(device, tag, 1_000 + STRANGER_WAIT_MS - 1));
        assert!(!memo.waits(device, tag, 1_000 + STRANGER_WAIT_MS));
        // 换了一份：马上读。
        assert!(!memo.waits(device, "v2:10", 1_001));
        // 连着认不出来：隔的时间逐次加倍，有封顶。
        memo.note(device, tag, 20_000);
        assert!(memo.waits(device, tag, 20_000 + 2 * STRANGER_WAIT_MS - 1));
        assert!(!memo.waits(device, tag, 20_000 + 2 * STRANGER_WAIT_MS));
        for _ in 0..40 {
            memo.note(device, tag, 50_000);
        }
        assert!(memo.waits(device, tag, 50_000 + STRANGER_WAIT_MAX_MS - 1));
        assert!(!memo.waits(device, tag, 50_000 + STRANGER_WAIT_MAX_MS));
        // 本机的钟往回拨了：不等。
        assert!(!memo.waits(device, tag, 49_999));
        // 存储上已经没有这个名字：忘掉。
        memo.keep_listed(&[]);
        assert!(!memo.waits(device, tag, 50_001));
    }

    #[tokio::test]
    async fn names_in_heads_that_no_device_of_this_space_wrote_leave_no_trace() {
        use crate::crypto::SpaceKey;
        use crate::transport::file::FileTransport;

        let (_g, ctx) = crate::testing::ctx();
        let me = crate::testing::settings(&ctx).device;
        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path());
        let keys = Arc::new(crate::testing::keys());
        let peers = |ctx: &Ctx| db::read(ctx.db(), db::peers).unwrap();

        // 存储上被放进两个名字像设备 id 的文件：一个是垃圾，一个是拿别的密钥写的 head。
        let (junk, foreign) = ("00000000000000a1", "00000000000000a2");
        transport
            .put(&codec::head_path(junk), b"x".to_vec())
            .await
            .unwrap();
        let other = Keys::derive(&SpaceKey::generate());
        let forged = head(foreign, Vec::new()).encode(&other).unwrap();
        transport
            .put(&codec::head_path(foreign), forged)
            .await
            .unwrap();

        let mut memo = Memo::default();
        let report = pull(&ctx, &transport, &keys, &me, PULL_BUDGET, &mut memo)
            .await
            .unwrap();
        assert_eq!((report.ops, report.changed), (0, 2));
        // 库里没有它们的登记，也就没有「读不到它的同步数据」。
        assert!(peers(&ctx).is_empty());
        // 还是那两份：下一轮不再去读。
        let report = pull(&ctx, &transport, &keys, &me, PULL_BUDGET, &mut memo)
            .await
            .unwrap();
        assert_eq!(report.changed, 0);
        assert!(peers(&ctx).is_empty());

        // 拿这个空间的密钥写的 head：认下这台设备。
        let real = "00000000000000b1";
        let written = head(real, Vec::new()).encode(&keys).unwrap();
        transport
            .put(&codec::head_path(real), written)
            .await
            .unwrap();
        let report = pull(&ctx, &transport, &keys, &me, PULL_BUDGET, &mut memo)
            .await
            .unwrap();
        assert_eq!(report.changed, 1);
        let known = peers(&ctx);
        assert_eq!(known.len(), 1);
        assert_eq!(known[0].0, real);
        assert!(known[0].1.error.is_none() && !known[0].1.head_tag.is_empty());
        assert!(known[0].1.seen_ms > 0);
    }

    #[tokio::test]
    async fn a_device_is_known_from_the_moment_its_head_opens() {
        use crate::transport::file::FileTransport;

        let (_g, ctx) = crate::testing::ctx();
        let me = crate::testing::settings(&ctx).device;
        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path());
        let keys = Arc::new(crate::testing::keys());

        // head 到了，它引用的段还没到（目录交给别的工具同步时常有的事）。
        let device = "00000000000000b1";
        let written = head(device, vec![seg(1, 2, "aaaaaaaa")])
            .encode(&keys)
            .unwrap();
        transport
            .put(&codec::head_path(device), written)
            .await
            .unwrap();
        let mut memo = Memo::default();
        pull(&ctx, &transport, &keys, &me, PULL_BUDGET, &mut memo)
            .await
            .unwrap();
        // 这台设备认下了，读不到它的日志这件事记在它头上（宽限期内先不报）。
        let peer = db::read(ctx.db(), |conn| db::known_peer(conn, device))
            .unwrap()
            .unwrap();
        assert_eq!((peer.fetched, peer.error_rounds), (0, 1));
        assert!(peer.error_since_ms > 0 && peer.error.is_none());
        assert!(peer.head_tag.is_empty() && peer.seen_ms == 0);
        // 不算「没认出来的」：下一轮照常再读。
        let report = pull(&ctx, &transport, &keys, &me, PULL_BUDGET, &mut memo)
            .await
            .unwrap();
        assert_eq!(report.changed, 1);
        let peer = db::read(ctx.db(), |conn| db::known_peer(conn, device))
            .unwrap()
            .unwrap();
        assert_eq!(peer.error_rounds, 2);
    }

    /// 快照的版本向量给的位置超过了那台设备日志的末尾：先按「还没到齐」等，等够了就不
    /// 作数，从日志现存的部分重新读。本机自己读出来的位置不这样——位置不动，报出来
    /// （§9.6 读者一侧）。位置落在日志之内的，读到 head 就算核对过。
    #[tokio::test]
    async fn a_position_from_a_snapshot_beyond_the_log_is_dropped_after_the_grace() {
        use crate::transport::file::FileTransport;

        let (_g, ctx) = crate::testing::ctx();
        let me = crate::testing::settings(&ctx).device;
        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path());
        let keys = Arc::new(crate::testing::keys());
        let grace_ms = transport.arrival_grace().as_millis() as i64;
        let peer_of = |device: &str| {
            db::read(ctx.db(), |conn| db::known_peer(conn, device))
                .unwrap()
                .unwrap()
        };
        let put_peer = |device: &str, peer: &PeerState| {
            db::write(ctx.db(), |tx| db::put_peer_state(tx, device, peer)).unwrap();
        };

        // 三台设备的日志都只到序号 2。本机记着的位置：两台是 9（一个是快照给的，一个是
        // 本机自己读出来的），一台是快照给的 2。
        let (told, read, fine) = ("00000000000000b1", "00000000000000b2", "00000000000000b3");
        let only = seg(1, 2, "aaaaaaaa");
        let ahead = seg(8, 9, "eeeeeeee").seg_ref();
        for device in [told, read, fine] {
            let written = head(device, vec![only.clone()]).encode(&keys).unwrap();
            transport
                .put(&codec::head_path(device), written)
                .await
                .unwrap();
            let at = if device == fine {
                (2, only.seg_ref())
            } else {
                (9, ahead.clone())
            };
            let peer = PeerState {
                fetched: at.0,
                fetched_seg: Some(at.1.clone()),
                processed: at.0,
                from_snap: device != read,
                ..PeerState::default()
            };
            db::write(ctx.db(), |tx| {
                db::state_put_json(
                    tx,
                    scope::RSEG,
                    &rseg_key(device, at.0),
                    db::HLC_ZERO,
                    &at.1,
                )?;
                db::put_peer_state(tx, device, &peer)
            })
            .unwrap();
        }
        let head_dirty = || db::read(ctx.db(), db::log_state).unwrap().head_dirty;
        assert!(!head_dirty());
        let mut memo = Memo::default();
        let report = pull(&ctx, &transport, &keys, &me, PULL_BUDGET, &mut memo)
            .await
            .unwrap();
        assert_eq!((report.ops, report.more, report.changed), (0, false, 3));
        // 位置正好是日志末尾、段也对得上的：核对过了，回执从这时起才写进 head。
        let peer = peer_of(fine);
        assert!(!peer.from_snap && peer.error.is_none() && !peer.head_tag.is_empty());
        assert_eq!(peer.fetched, 2);
        assert!(head_dirty());
        // 超过末尾的两台都先等着：位置不动。
        for device in [told, read] {
            let peer = peer_of(device);
            assert_eq!((peer.fetched, peer.processed), (9, 9));
            assert!(peer.error.is_some() && peer.error_since_ms > 0);
        }
        // 本机读出来的：这份 head 不变就不再读。快照给的：下一轮照样来看。
        assert!(!peer_of(read).head_tag.is_empty() && peer_of(told).head_tag.is_empty());
        let report = pull(&ctx, &transport, &keys, &me, PULL_BUDGET, &mut memo)
            .await
            .unwrap();
        assert_eq!(report.changed, 1);
        assert!(peer_of(told).from_snap && peer_of(told).fetched == 9);

        // 等够了。
        for device in [told, read] {
            let mut peer = peer_of(device);
            peer.error_since_ms -= grace_ms;
            peer.head_tag.clear();
            put_peer(device, &peer);
        }
        let report = pull(&ctx, &transport, &keys, &me, PULL_BUDGET, &mut memo)
            .await
            .unwrap();
        assert!(report.more);
        // 快照给的位置不作数：从日志现存的开头重新读，回执跟着退回去，记着的那个段也清掉。
        let peer = peer_of(told);
        assert_eq!(
            (peer.fetched, peer.fetched_seg.clone(), peer.processed),
            (0, None, 0)
        );
        assert!(!peer.from_snap && peer.error.is_none() && peer.error_since_ms == 0);
        let kept = db::read(ctx.db(), |conn| known_segment(conn, told, 9)).unwrap();
        assert_eq!(kept, None);
        // 本机自己读出来的位置不动。
        let peer = peer_of(read);
        assert_eq!((peer.fetched, peer.processed), (9, 9));
        assert!(peer.error.is_some());
    }

    /// 快照给的位置落在日志之内，却不在这条链上（写快照的设备读的是这个身份的另一条
    /// 链，或者向量不对）：没什么可等的，那个位置不作数，从日志现存的开头重新读。本机
    /// 自己读出来的位置碰上同样的事，照旧报错、位置不动。
    #[tokio::test]
    async fn a_position_from_a_snapshot_that_is_not_on_the_chain_is_dropped_at_once() {
        use crate::op::Op;
        use crate::seal::{self, SealOptions};
        use crate::transport::file::FileTransport;
        use crate::upload::{self, UploadOptions};

        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path());
        let keys = Arc::new(crate::testing::keys());

        // 写者：四个段，每两个打成一个汇总包。
        let (_w, writer) = crate::testing::ctx();
        let author = db::write(writer.db(), |tx| db::ensure_device(tx)).unwrap();
        let instance = crate::ids::new_instance_id();
        for _ in 0..4 {
            db::write(writer.db(), |tx| {
                crate::map::enqueue(
                    tx,
                    &author,
                    None,
                    &Op::BlobWant {
                        blob: "ab".repeat(32),
                    },
                )
            })
            .unwrap();
            seal::seal_next(
                &writer,
                &SealOptions {
                    keys: &keys,
                    device: &author,
                    budget: 1 << 20,
                    attachment_max: 1 << 20,
                    upload_attachments: false,
                },
            )
            .unwrap()
            .expect("封出一个段");
            upload::upload_round(
                &writer,
                &transport,
                &UploadOptions {
                    keys: &keys,
                    device: &author,
                    instance: &instance,
                    instances: std::slice::from_ref(&instance),
                    running: Vec::new(),
                    pack_plain_max: 1 << 20,
                    rollup_every: 2,
                    force_head: false,
                },
            )
            .await
            .unwrap();
        }
        let written = remote::read_head(&transport, &keys, &author)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                written.last_seq,
                written.rollup.as_ref().map(|rollup| rollup.to)
            ),
            (4, Some(4))
        );

        // 读者记着的位置是序号 1，段却不是这条日志上的那一个。
        let (_r, ctx) = crate::testing::ctx();
        let me = crate::testing::settings(&ctx).device;
        let elsewhere = seg(1, 1, "eeeeeeee").seg_ref();
        let place = |from_snap: bool| {
            let peer = PeerState {
                fetched: 1,
                fetched_seg: Some(elsewhere.clone()),
                processed: 1,
                from_snap,
                ..PeerState::default()
            };
            db::write(ctx.db(), |tx| {
                db::state_put_json(
                    tx,
                    scope::RSEG,
                    &rseg_key(&author, 1),
                    db::HLC_ZERO,
                    &elsewhere,
                )?;
                db::put_peer_state(tx, &author, &peer)
            })
            .unwrap();
        };
        let peer_of = || {
            db::read(ctx.db(), |conn| db::known_peer(conn, &author))
                .unwrap()
                .unwrap()
        };
        let mut memo = Memo::default();

        // 本机自己读出来的：读不下去，位置不动。
        place(false);
        let report = pull(&ctx, &transport, &keys, &me, PULL_BUDGET, &mut memo)
            .await
            .unwrap();
        assert_eq!((report.ops, report.more), (0, false));
        let peer = peer_of();
        assert_eq!((peer.fetched, peer.processed), (1, 1));
        assert!(peer.error_since_ms > 0);

        // 快照给的：不作数，从头读。
        place(true);
        let report = pull(&ctx, &transport, &keys, &me, PULL_BUDGET, &mut memo)
            .await
            .unwrap();
        assert_eq!((report.ops, report.more), (0, true));
        let peer = peer_of();
        assert_eq!(
            (peer.fetched, peer.fetched_seg.clone(), peer.processed),
            (0, None, 0)
        );
        assert!(!peer.from_snap && peer.error.is_none() && peer.error_since_ms == 0);
        assert!(db::read(ctx.db(), db::log_state).unwrap().head_dirty);
        let report = pull(&ctx, &transport, &keys, &me, PULL_BUDGET, &mut memo)
            .await
            .unwrap();
        assert_eq!((report.ops, report.more), (4, false));
        let peer = peer_of();
        assert_eq!(
            (peer.fetched, peer.fetched_seg, peer.processed),
            (4, written.last_seg, 0)
        );
    }
}
