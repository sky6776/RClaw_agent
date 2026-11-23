//! 全量快照与回收（§9.5）。
//!
//! **快照**是某台设备手里全部已发布内容的一份完整副本，加一个版本向量（它读到各设备
//! 日志的哪里）。新设备从最新的快照起步，再接着读它之后的日志；日志被回收之后才加入、
//! 或落后太多的设备也靠它补齐。
//!
//! 快照里的内容不是另一种格式，就是一行行 op。接收方把它们写进收件箱（序号记为 0），
//! 由合入照常检查、照常处理：它和日志里的 op 一样是不可信输入（§11.3）；而所有 op 都能
//! 重复重放，所以「先吃快照再接日志」「快照比向量说的多一点」都不出错。
//!
//! 存储上的对象（都压缩并加密）：
//!
//! ```text
//! snapshots/<设备>/<名字>.snap          清单：版本向量、各分片的名字与摘要
//! snapshots/<设备>/<名字>-0000.part     分片：每行 `<来源设备>\t<op>`
//! snapshots/<设备>/<名字>-blobs.part    在用附件清单：每行一个附件对象名
//! ```
//!
//! **谁能写**：手里的数据是全的设备——没有排队、悬空、被隔离的 op，没有因为范围而没
//! 落地的线程，没丢过 op，自己的日志也都传上去了。
//!
//! **回收**：一个段或汇总包被所有活跃设备回执、且已被某个快照覆盖之后，作者可以删掉
//! 它；不在最新快照的在用附件清单里的附件对象，隔一天、再出一个快照仍然没人引用，也
//! 可以删。删除都分两步：先在本机登记，等带着新回收位置（或新快照指针）的 head 写出去、
//! 再过一段宽限期，才真的去存储上删——读者只信 head，head 里不再提的对象才是没人要的。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rclaw_store::sync_db::{self, INBOX_PENDING, INBOX_SNAP_STAGE, SEAL_LOCAL};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::attach::now_ms;
use crate::bundle;
use crate::codec::{self, SegRef, SnapRef};
use crate::crypto::{sha256_hex, Keys};
use crate::db::{
    self, scope, DeviceInfo, LogState, StateRow, BLOB_HELD, BLOB_PENDING, BLOB_UPLOADED,
};
use crate::error::{Result, SyncError, TransportError};
use crate::files;
use crate::ids;
use crate::map::{self, OpenInfo, SessionMap};
use crate::merge;
use crate::op::{Op, OpLine, ARTIFACT_INLINE_MAX, FORMAT_VERSION};
use crate::publish::{self, Settings};
use crate::pull::{self, InSeg};
use crate::remote;
use crate::seal::{self, SegEntry};
use crate::transport::{self, SyncTransport};
use crate::upload;
use crate::wire::{self, ArtifactPolicy, LocalBlob, StoredRefs, OP_ARTIFACTS_MAX};
use crate::{blocking, offload, Ctx};

/// 读某台设备的日志时发现需要的部分已被回收：记在它的读取进度里的原因。
pub const NEED_SNAPSHOT: &str = "它较早的同步数据已被回收，要先从全量快照补齐";

/// 一个快照最多写这么久；超过就放弃（下次再从头写）。
pub const WRITER_MAX_MS: i64 = 3600 * 1000;

/// 一个分片的明文大小（字节）。写满就封口；单行比它大时分片也跟着大。
const PART_TARGET: usize = 2 * 1024 * 1024;
/// 导出会话时一页读多少行、多少字节的正文。
const PAGE_ROWS: usize = 200;
const PAGE_BYTES: usize = 4 * 1024 * 1024;
/// 导出删除记录时一次去库里取多少条的链、合计多少字节（至少一条）。
const TOMB_BATCH: usize = 32;
const TOMB_BATCH_BYTES: usize = 1024 * 1024;
/// 分片个数的上限（分片名是 4 位序号）。
const PARTS_MAX: usize = 10_000;
/// 在用附件清单的分片名。
const BLOBS_PART: &str = "blobs";
/// 清单明文的上限。
const MANIFEST_PLAIN_MAX: usize = 16 * 1024 * 1024;
/// 版本向量里设备数的上限（换过的身份也各占一项）。
const VECTOR_MAX: usize = codec::DEVICES_MAX;

/// 空间里还没有快照时，op 总数到这么多就写第一个。
const SNAP_FIRST_OPS: u64 = 5000;
/// 之后：比上一个快照多出这么多 op、而且它已经有一天了，就再写一个；
const SNAP_OPS: u64 = 5000;
const SNAP_AGE_MS: i64 = 24 * 3600 * 1000;
/// 或者多出的不多、但上一个快照已经有一个月了。
const SNAP_OPS_SLOW: u64 = 1000;
const SNAP_AGE_SLOW_MS: i64 = 30 * 24 * 3600 * 1000;
/// 几台设备同时满足条件时错开：设备 id 排在前面的先写。
const SNAP_STAGGER_OPS: u64 = 500;
const SNAP_STAGGER_MS: i64 = 2 * 3600 * 1000;
/// 写快照的条件不满足（或写失败）之后，隔这么久再试。
const SNAP_RETRY_MS: i64 = 3600 * 1000;

/// 日志对象从 head 里消失之后，隔这么久才从存储上删：照着旧 head 读到一半的设备来得及读完。
const LOG_GRACE_MS: i64 = 10 * 60 * 1000;
/// 旧快照在新快照的指针写出去之后保留这么久。
const SNAP_GRACE_MS: i64 = 60 * 60 * 1000;
/// 附件对象连续这么久没人引用才删。
const BLOB_GRACE_MS: i64 = 24 * 3600 * 1000;
/// 最近这么多个段始终不回收（换身份时重新发布要用，§9.6）。
const KEEP_SEGMENTS: usize = 8;

/// 两次「删到期对象」至少隔这么久；一次最多删这么多个、这么久。
const SWEEP_SPACING_MS: i64 = 60 * 1000;
const SWEEP_BATCH: usize = 100;
const SWEEP_BUDGET: Duration = Duration::from_secs(8);
/// 依据这轮核验不过（不是预算不够）后的退避：排在后面的对象才轮得到。
const GC_RETRY_MS: i64 = 5 * 60 * 1000;
/// 升级前留下的、推不出恢复依据的日志对象：退到很久以后再看（对象不删）。不退避的话
/// 它们每轮都占满批次，后面的正常删除记录永远轮不到。
const GC_PARK_MS: i64 = 30 * 24 * 3600 * 1000;
/// 晋升（候选快照 / 候选回收位置）前的完整复核一次走完的时限：这一步每小时最多一次，
/// 给得比每轮的回收扫描宽，让大一点的快照也有机会一次核完（跨轮前缀不能当证据）。
const PROMOTE_BUDGET: Duration = Duration::from_secs(30);
/// 一份依据连续这么多次没能在一次判定里核完，就不再重试（保留旧边界与对象）。
const PROMOTE_TRIES: u32 = 3;
/// 回收检查（回收位置、附件）隔这么久做一次。
const GC_SCAN_MS: i64 = 3600 * 1000;
/// 一轮里最多新下载这么多个快照清单、最多吃这么多个快照。
const MANIFESTS_PER_ROUND: usize = 4;
const APPLIES_PER_ROUND: usize = 2;
/// 一轮里最多核验这么多个快照分片：推进回收位置的依据要完整核验过（分批做，见
/// [`refresh_covers`]）。
const VERIFY_PARTS_PER_ROUND: usize = 8;
/// 清单刚读不过的快照，这么久以内不再试它（[`Memo`] 里退避）：把预算让给排在后面的
/// 候选，别让最前面的几个堵住所有轮次。
const MANIFEST_BACKOFF_MS: i64 = 10 * 60 * 1000;
/// 内存里留着的快照清单个数。
const MEMO_MANIFESTS: usize = 16;
/// 吃快照、登记附件时一个事务最多写这么多行。
const APPLY_BATCH: usize = 500;

// ---------------------------------------------------------------------------
// 清单
// ---------------------------------------------------------------------------

/// 版本向量里的一项：快照覆盖了这台设备的日志到 `seq`（它那里的最后一个段是 `seg`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorEntry {
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seg: Option<SegRef>,
}

/// 清单里的一个分片。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartRef {
    pub name: String,
    /// 分片明文的摘要。
    pub digest: String,
    /// 分片里的行数。
    pub lines: u64,
}

/// 快照清单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub v: u32,
    pub dev: String,
    /// 写出时刻（写者的时钟，Unix 毫秒）；快照名里也是它。
    pub ts: i64,
    pub vector: BTreeMap<String, VectorEntry>,
    pub parts: Vec<PartRef>,
    /// 在用附件清单。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blobs: Option<PartRef>,
}

fn part_name(index: usize) -> String {
    format!("{index:04}")
}

impl Manifest {
    /// 解析并核对清单。清单来自远端：不合规一律拒绝。
    fn parse(plain: &[u8], dev: &str, name: &str) -> Result<Manifest> {
        let manifest: Manifest = serde_json::from_slice(plain)
            .map_err(|e| SyncError::corrupt(format!("快照清单无法解析: {e}")))?;
        if manifest.v > FORMAT_VERSION {
            return Err(SyncError::Upgrade(format!(
                "设备 {dev} 的快照格式版本 {} 高于本程序支持的 {FORMAT_VERSION}",
                manifest.v
            )));
        }
        let bad = |what: &str| SyncError::corrupt(format!("快照清单不自洽: {what}"));
        if manifest.v == 0
            || manifest.dev != dev
            || ids::parse_snapshot_name(name) != Some(manifest.ts)
        {
            return Err(bad("作者、版本或时刻"));
        }
        let seg_ok = |seg: &SegRef| {
            ids::parse_segment_name(&seg.name).is_some() && ids::is_digest(&seg.digest)
        };
        // 向量里的位置之后会直接成为本机的读取位置：序号与 head 里的同一个上限（再大就
        // 不可能是真的，还会让后面的加减溢出），设备数也有限。位置对不对，由读者读到那台
        // 设备的 head、接着读它的日志时把关：接不上的不作数，从日志现存的开头重新读，在
        // 那之前也不当回执发出去（[`db::PeerState::from_snap`]）。
        if manifest.vector.len() > VECTOR_MAX
            || !manifest.vector.iter().all(|(device, entry)| {
                ids::is_device_id(device)
                    && entry.seq <= codec::SEQ_MAX
                    && entry.seg.as_ref().is_none_or(seg_ok)
            })
        {
            return Err(bad("版本向量"));
        }
        if manifest.parts.len() > PARTS_MAX
            || !manifest
                .parts
                .iter()
                .enumerate()
                .all(|(index, part)| part.name == part_name(index) && ids::is_digest(&part.digest))
        {
            return Err(bad("分片清单"));
        }
        if manifest
            .blobs
            .as_ref()
            .is_some_and(|part| part.name != BLOBS_PART || !ids::is_digest(&part.digest))
        {
            return Err(bad("附件清单"));
        }
        Ok(manifest)
    }

    /// 快照覆盖的 op 总数。
    fn ops(&self) -> u64 {
        self.vector
            .values()
            .fold(0u64, |sum, entry| sum.saturating_add(entry.seq))
    }
}

fn snapshot_dir(device: &str) -> String {
    format!("snapshots/{device}/")
}

/// 快照目录里的一个对象属于哪个快照（名字不合规返回 `None`）。
fn snapshot_of(object: &str) -> Option<&str> {
    // 快照名：13 位时刻、`-`、8 位随机。
    let name = object.get(..22)?;
    ids::parse_snapshot_name(name)?;
    let rest = &object[22..];
    (rest == ".snap" || (rest.starts_with('-') && rest.ends_with(".part"))).then_some(name)
}

fn snap_key(device: &str, name: &str) -> String {
    format!("{device}/{name}")
}

/// 两个快照里 `a` 是不是比 `b` 新：覆盖的 op 多的算新，一样多时看写出时刻。
///
/// 不直接比时刻：那是写者的时钟，时钟快的设备写的快照会一直排在最前面。覆盖的 op 数
/// 只增不减（各设备的日志位置之和），后写的快照不会比写者当时知道的那些少。
fn newer(a: &SnapRef, b: &SnapRef) -> bool {
    (a.ops, a.ts) > (b.ops, b.ts)
}

fn hlc_device(hlc: &str) -> Option<&str> {
    hlc.splitn(3, '-').nth(2)
}

/// 可以放过、留到以后再试的错误：对象不在、校验不过、要升级才读得懂。其余的（连不上、
/// 被拒绝、本机库出错）照常往上报。
fn skippable(error: &SyncError) -> bool {
    matches!(
        error,
        SyncError::Corrupt(_)
            | SyncError::Upgrade(_)
            | SyncError::Transport(TransportError::NotFound(_))
    )
}

// ---------------------------------------------------------------------------
// 本机记录
// ---------------------------------------------------------------------------

/// 快照与回收的本机记录（kv `sync/maint`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Maint {
    /// 正在写的快照。写到一半退出的话，下次按它清掉已经传上去的对象。
    #[serde(default)]
    writing: Option<String>,
    /// 各快照覆盖本机日志到哪里：键 `<设备>/<快照名>`。
    #[serde(default)]
    covers: BTreeMap<String, u64>,
    /// 上次做附件回收时依据的快照（`<设备>/<快照名>`）。
    #[serde(default)]
    blob_snap: Option<String>,
    /// 上次尝试写快照的时刻。
    #[serde(default)]
    snap_try_ms: i64,
    /// 上次做回收检查的时刻。
    #[serde(default)]
    gc_ms: i64,
    /// 正在吃的快照：它的 op 已经开始进收件箱，各设备日志的读取位置还没挪。没吃完就
    /// 改吃另一个快照的话，先把它留在收件箱里的 op 清掉。
    #[serde(default)]
    applying: Option<String>,
    /// 各快照分片的核验进度：键 `<设备>/<快照名>`。清单摘要钉住内容；`next` 之前的前缀
    /// 都已下载并核对过摘要。只有 `next` 追平 `parts` 的（完整核验过）才能推进回收位置。
    #[serde(default)]
    covers_checked: BTreeMap<String, VerifiedCover>,
    /// 待公布的回收位置（[`PendingFloor`]）：删除已排队，head 与日志索引还没动。公布前
    /// 要把依据的快照在当前存储上重新完整复核一遍。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    floor_pending: Option<PendingFloor>,
    /// 写好、还没核验完的候选快照：分片完整核验、并确认覆盖到回收边界之后，才在 head
    /// 里取代上一份快照（[`promote_snap`]）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    snap_pending: Option<SnapRef>,
    /// 候选快照完整复核失败的次数。超过上限后不再每小时重试（候选留着，等下一份候选
    /// 取代它）；旧指针与旧快照都保留——这是弱版本存储上无法一次核完时的保守落点。
    #[serde(default)]
    snap_attempts: u32,
}

/// 候选回收位置（[`Maint::floor_pending`]）：对象删除已经按宽限期排队，但本机日志的
/// 索引、`log.floor` 与 head 都还没动——新设备仍然按旧边界读得到这些对象。只有在当前
/// 存储上把依据的快照完整复核通过之后，才推进并公布这个位置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PendingFloor {
    floor: u64,
    /// 回收依据：`<设备>/<快照名>`、清单摘要。
    basis: (String, String),
    /// 已经尝试完整复核的次数。一次判定必须连续走完（不能跨轮复用前缀），大快照在弱
    /// 版本存储上可能永远走不完：超过上限就撤掉候选、保留对象与旧边界，别每小时重读。
    #[serde(default)]
    tries: u32,
}

/// 一份快照的分片核验进度（[`Maint::covers_checked`] 的值）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct VerifiedCover {
    /// 清单的摘要：换清单（换了内容）就从头核验。
    digest: String,
    /// 它覆盖本机日志到哪里。
    seq: u64,
    /// 清单里声明的分片数。
    parts: usize,
    /// 已经核验过摘要的前缀长度。
    next: usize,
    /// 核验时存储端给出的对象版本（与清单分片一一对应；给不出可信版本的记空串）。
    /// 晋升候选快照前按它做一遍版本复核，省掉重复下载，也让「核对过」有据可查。
    #[serde(default)]
    versions: Vec<String>,
}

impl VerifiedCover {
    fn complete(&self) -> bool {
        self.next >= self.parts
    }
}

fn maint(conn: &Connection) -> Result<Maint> {
    Ok(db::kv_json(conn, db::KV_MAINT)?.unwrap_or_default())
}

fn put_maint(conn: &Connection, maint: &Maint) -> Result<()> {
    db::kv_put_json(conn, db::KV_MAINT, maint)
}

/// 本机换了身份：旧身份名下的快照、它们对旧日志的覆盖都与新身份无关了。旧身份的对象
/// 留在存储上不动——别的设备也许还要靠它们补齐。
pub(crate) fn forget_own(conn: &Connection) -> Result<()> {
    let mut record = maint(conn)?;
    record.writing = None;
    record.covers.clear();
    record.covers_checked.clear();
    record.blob_snap = None;
    record.floor_pending = None;
    record.snap_pending = None;
    put_maint(conn, &record)
}

/// 等着从存储上删的对象（`sync_state` 的 `gc`，键是对象路径）可以动手的条件。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct GcWhen {
    /// 不早于这个时刻（本机时钟）。
    after_ms: i64,
    /// 而且本机写出的 head 里的回收位置已经不小于它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    floor: Option<u64>,
    /// 而且本机写出的 head 里的快照指针已经是它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    snap: Option<String>,
    /// 而且这份回收依据（`<设备>/<快照名>`，清单摘要）那时还能完整读到。删日志对象前
    /// 再确认一次：作为依据的快照要是没了（第三方目录只同步了一半、被改过），删除就得
    /// 等着——新设备可能正靠它补齐这段已被回收的日志。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    basis: Option<(String, String)>,
    /// 而且这份汇总包（[`PackGuard`]）那时还能完整读到：零散段删掉之后，包里是这段
    /// 日志唯一的副本。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pack: Option<PackGuard>,
    /// 依据这轮核验不过后的重试时刻（本机时钟）。不早于它之前不再重复核验，免得排在
    /// 前面的坏依据每轮都占满批次，后面的对象永远轮不到。
    #[serde(default, skip_serializing_if = "is_zero")]
    retry_ms: i64,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

/// 删除零散段前要能读回的汇总包。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PackGuard {
    /// 包所属设备。
    device: String,
    name: String,
    /// 包明文的摘要。
    digest: String,
}

fn queue_delete(conn: &Connection, path: &str, when: &GcWhen) -> Result<()> {
    db::state_put_json(conn, scope::GC, path, db::HLC_ZERO, when)
}

/// 引擎跨轮留着的一点记忆（不落库）。
#[derive(Default)]
pub struct Memo {
    manifests: HashMap<String, Arc<Manifest>>,
    order: VecDeque<String>,
    /// 清单读不过的快照（键 `<设备>/<快照名>`）：不早于这个时刻（本机时钟）再试它。
    backoff: HashMap<String, i64>,
    /// 这次运行里已经试过「新设备直接从快照起步」。
    pub tried_new: bool,
    /// 上次删到期对象的时刻。
    swept_ms: i64,
}

impl Memo {
    fn get(&self, key: &str) -> Option<Arc<Manifest>> {
        self.manifests.get(key).cloned()
    }

    fn put(&mut self, key: String, manifest: Arc<Manifest>) {
        if self.manifests.insert(key.clone(), manifest).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > MEMO_MANIFESTS {
            if let Some(oldest) = self.order.pop_front() {
                self.manifests.remove(&oldest);
            }
        }
    }

    /// 清单上次读不过、还没到重试时刻。
    fn backed_off(&mut self, key: &str, now: i64) -> bool {
        match self.backoff.get(key) {
            Some(&until) if now < until => true,
            Some(_) => {
                self.backoff.remove(key);
                false
            }
            None => false,
        }
    }

    /// 清单这次读不过：退避一段时间，不每轮都堵在同几个候选上。
    fn note_backoff(&mut self, key: &str, now: i64) {
        if self.backoff.len() > MEMO_MANIFESTS * 4 {
            self.backoff.clear();
        }
        self.backoff.insert(key.to_string(), now + MANIFEST_BACKOFF_MS);
    }
}

/// 直接读一份快照清单（不经过内存缓存）：替换/晋升比较旧快照的向量用。`digest` 给了就
/// 核对内容摘要；没给时只解析（旧快照可能已经被清理，读不到就报错，调用方保留对象）。
async fn open_manifest(
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    dev: &str,
    name: &str,
    digest: Option<&str>,
) -> Result<Manifest> {
    let path = codec::snapshot_path(dev, name);
    let object = transport
        .get(&path, codec::sealed_max(MANIFEST_PLAIN_MAX))
        .await
        .map_err(SyncError::from)?;
    let (keys, dev, name, digest) = (
        keys.clone(),
        dev.to_string(),
        name.to_string(),
        digest.map(str::to_string),
    );
    blocking(move || {
        let plain = codec::open_log_object(&keys, &path, &object, MANIFEST_PLAIN_MAX)?;
        if let Some(digest) = &digest {
            if sha256_hex(&plain) != *digest {
                return Err(SyncError::corrupt(format!("{path} 的摘要与登记不符")));
            }
        }
        Manifest::parse(&plain, &dev, &name)
    })
    .await
}

/// 取一个快照的清单：下载、解密、核对摘要、解析。读过的留在内存里；读不过的（对象不在、
/// 校验不过、要升级）退避一会儿，避免每轮都把预算耗在最前面的几个候选上。
async fn fetch_manifest(
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    memo: &mut Memo,
    dev: &str,
    snap: &SnapRef,
) -> Result<Arc<Manifest>> {
    let key = snap_key(dev, &snap.name);
    if let Some(found) = memo.get(&key) {
        return Ok(found);
    }
    let path = codec::snapshot_path(dev, &snap.name);
    // 清单不在（404：被回收的旧快照、第三方目录只同步了一半）与解密/解析失败一样，
    // 都是这个候选的问题——同样退避，别让最前面的几个缺清单的候选每轮耗光预算。
    let object = match transport
        .get(&path, codec::sealed_max(MANIFEST_PLAIN_MAX))
        .await
    {
        Ok(object) => object,
        Err(error) if skippable(&SyncError::from(error.clone())) => {
            memo.note_backoff(&key, now_ms());
            return Err(SyncError::from(error));
        }
        Err(error) => return Err(SyncError::from(error)),
    };
    let (keys, dev, name, digest, ops) = (
        keys.clone(),
        dev.to_string(),
        snap.name.clone(),
        snap.digest.clone(),
        snap.ops,
    );
    let manifest = match blocking(move || {
        let plain = codec::open_log_object(&keys, &path, &object, MANIFEST_PLAIN_MAX)?;
        if sha256_hex(&plain) != digest {
            return Err(SyncError::corrupt(format!(
                "{path} 的摘要与 head 里记的不符"
            )));
        }
        let manifest = Manifest::parse(&plain, &dev, &name)?;
        // head 里报的「覆盖了多少 op」各处拿来比新旧、算「又攒了多少」，得和清单对得上。
        if manifest.ops() != ops {
            return Err(SyncError::corrupt(format!(
                "{path} 覆盖的 op 数与 head 里记的不符"
            )));
        }
        Ok(manifest)
    })
    .await
    {
        Ok(manifest) => manifest,
        Err(error) if skippable(&error) => {
            memo.note_backoff(&key, now_ms());
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let manifest = Arc::new(manifest);
    memo.put(key, manifest.clone());
    Ok(manifest)
}

/// 下载一个快照分片，解密后核对它与清单里记的摘要（推进回收位置前核验依据用）。
async fn verify_snapshot_part(
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    dev: &str,
    snap_name: &str,
    part: &PartRef,
) -> Result<()> {
    let path = codec::snapshot_part_path(dev, snap_name, &part.name);
    let object = transport.get(&path, codec::SEGMENT_OBJECT_MAX).await?;
    let (keys, digest) = (keys.clone(), part.digest.clone());
    blocking(move || {
        let plain = codec::open_log_object(&keys, &path, &object, codec::SEGMENT_PLAIN_MAX)?;
        drop(object);
        if sha256_hex(&plain) != digest {
            return Err(SyncError::corrupt(format!("{path} 的摘要与清单里记的不符")));
        }
        Ok(())
    })
    .await
}

/// 汇总包还能不能完整读回：下载、解密、核对明文摘要、解析包头并逐段核对。零散段删掉
/// 之后包里是这段日志唯一的副本，读不回来就不能删。
async fn pack_intact(
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    pack: &PackGuard,
    deadline: Instant,
) -> bool {
    // 单个 GET 没法中途取消；至少在开始前守住这一轮分给它的份额。
    if Instant::now() >= deadline {
        return false;
    }
    let path = codec::rollup_path(&pack.device, &pack.name);
    let object = match transport.get(&path, codec::ROLLUP_OBJECT_MAX).await {
        Ok(object) => object,
        Err(_) => return false,
    };
    let (keys, digest, device, parse_path) =
        (keys.clone(), pack.digest.clone(), pack.device.clone(), path);
    blocking(move || -> Result<bool> {
        let plain = codec::open_log_object(&keys, &parse_path, &object, codec::ROLLUP_PLAIN_MAX)?;
        Ok(sha256_hex(&plain) == digest && codec::parse_rollup(&plain, &device).is_ok())
    })
    .await
    .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 写快照：条件与内容
// ---------------------------------------------------------------------------

/// 写快照的前提里便宜的那几项：会话范围开着；没有排队、悬空、被隔离、还没处理的 op
/// （因范围收窄暂缓的除外——它们留着自己等范围重开，不该挡住别的范围写快照）；
/// 本机的变更都封了段、传上去了；没有跳过的线程、没丢过 op；各设备的日志都处理到了取回
/// 的位置，而且读得下去。
fn settled(conn: &Connection, s: &Settings, now: i64) -> Result<bool> {
    if !s.scopes.sessions {
        return Ok(false);
    }
    let busy: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sync_inbox)",
        [],
        |row| row.get(0),
    )?;
    if busy || seal::has_sendable(conn, s)? {
        return Ok(false);
    }
    // 队列里还有因范围收窄暂缓的行：文件寄存器里是它们从未发布的最新内容，快照会把
    // 这些内容绕过范围带给其他设备。等它们封出去（范围重开）或被清掉再写。
    if seal::has_deferred(conn, s)? {
        return Ok(false);
    }
    let log = db::log_state(conn)?;
    if log.pending.is_some() || log.up_seq != log.seq {
        return Ok(false);
    }
    if !db::state_scan(conn, scope::SKIP, "")?.is_empty() {
        return Ok(false);
    }
    // 还没清点过的：本机手里可能留着别处已经删掉的分支更早的样子，不能当成现有内容写
    // 进快照。
    if merge::stale_pending(conn)? {
        return Ok(false);
    }
    for (_, peer) in db::peers(conn)? {
        // 活跃设备上还没核对过的位置（别的快照给的，[`PeerState::from_snap`]）不再往下
        // 传：读到它的 head 就有结论，等得起。head 已经不在存储上的设备没处核对，照样写。
        if peer.processed != peer.fetched
            || peer.halted.is_some()
            || ((peer.error.is_some() || peer.from_snap || peer.pull_incomplete)
                && peer.is_active(now))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 写快照要用的内容里，在库里一次读出来的那部分（原样取出，出了库锁再整理）。
struct Plan {
    vector: BTreeMap<String, VectorEntry>,
    devices: Vec<(String, StateRow)>,
    opens: Vec<(String, StateRow)>,
    tombs: Vec<(String, StateRow)>,
    attrs: Vec<(String, StateRow)>,
    sessions: Vec<SessionMap>,
    /// 文件类寄存器的键。值按分片读取。
    registers: Vec<(&'static str, String)>,
}

/// 核对前提并读出内容。前提不满足返回 `None`。
///
/// 版本向量与这些索引在同一次读取里取出；会话消息和寄存器值稍后读取，可能比向量更新
/// （多出来的是向量之后的 op，接收方接着读日志时会再处理一遍，幂等）。
fn read_plan(conn: &Connection, s: &Settings, now: i64) -> Result<Option<Plan>> {
    if !settled(conn, s, now)? {
        return Ok(None);
    }
    // 参与同步的会话都发布了、不参与的都没发布：否则本机手里的「已发布内容」与该有的不
    // 一致（通道名单缩小过、导入的会话还没扫到……），这份快照不能写。
    let sessions = map::all_sessions(conn)?;
    for session in &sessions {
        let eligible = publish::eligible(conn, s, &session.channel, &session.chat_id)?;
        if (session.sealed != SEAL_LOCAL) != eligible {
            return Ok(None);
        }
    }
    let log = db::log_state(conn)?;
    let mut vector = BTreeMap::new();
    if log.seq > 0 {
        vector.insert(
            s.device.clone(),
            VectorEntry {
                seq: log.seq,
                seg: log.last.clone(),
            },
        );
    }
    for (device, peer) in db::peers(conn)? {
        if peer.fetched > 0 && device != s.device {
            vector.insert(
                device,
                VectorEntry {
                    seq: peer.fetched,
                    seg: peer.fetched_seg,
                },
            );
        }
    }
    let mut registers = Vec::new();
    for scope in files::REGISTERS {
        registers.extend(
            db::state_keys(conn, scope, "")?
                .into_iter()
                .map(|key| (scope, key)),
        );
    }
    Ok(Some(Plan {
        vector,
        devices: db::state_scan(conn, scope::DEVICE, "")?,
        opens: db::state_scan(conn, scope::OPEN, "")?,
        tombs: db::state_scan(conn, scope::TOMB, "")?,
        attrs: db::state_scan(conn, scope::ATTR, "")?,
        sessions,
        registers,
    }))
}

/// 快照开头的小 op：设备、线程的开头信息、分支属性。每项是 `(来源设备, op 行)`。
/// 删除记录跟在它们后面、会话包前面，由 [`Producer::next_tomb`] 分批给出——它们带着
/// 链，不算小。
///
/// 只属于本机的东西不出门：导入后还没发布的会话的开头信息、本机给没发布的线程设过的
/// 属性，都不写进去。
fn small_lines(plan: &Plan, s: &Settings) -> Result<VecDeque<(String, String)>> {
    let published: HashSet<&str> = plan
        .sessions
        .iter()
        .filter(|session| session.sealed != SEAL_LOCAL)
        .map(|session| session.sid.as_str())
        .collect();
    let buried: HashSet<&str> = plan
        .tombs
        .iter()
        .filter_map(|(key, _)| key.split_once('/').map(|(sid, _)| sid))
        .collect();
    let public = |sid: &str| published.contains(sid) || buried.contains(sid);
    let mut out = VecDeque::new();
    for (device, row) in &plan.devices {
        let Some(info) = row.json::<DeviceInfo>() else {
            continue;
        };
        if !ids::is_device_id(device) {
            continue;
        }
        let op = Op::DeviceHello {
            name: info.name,
            platform: info.platform,
            version: info.version,
            replaces: info.replaces,
        };
        out.push_back((device.clone(), op.to_line(&row.hlc)?));
    }
    for (sid, row) in &plan.opens {
        let Some(info) = row.json::<OpenInfo>() else {
            continue;
        };
        if row.hash.as_deref() == Some(map::OPEN_LOCAL) && !public(sid) {
            continue;
        }
        let origin = if ids::is_device_id(&info.origin) {
            info.origin.clone()
        } else {
            s.device.clone()
        };
        let op = Op::SessionOpen {
            sid: sid.clone(),
            channel: info.channel,
            chat_id: info.chat_id,
            created_at: info.created_at,
            fork_of: info.fork_of,
        };
        out.push_back((origin, op.to_line(db::HLC_ZERO)?));
    }
    for (key, row) in &plan.attrs {
        let mut parts = key.splitn(3, '|');
        let (Some(sid), Some(at), Some(field)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Some(value) = row
            .value
            .as_deref()
            .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
        else {
            continue;
        };
        let mine = hlc_device(&row.hlc).is_some_and(|device| s.is_me(device));
        if mine && !public(sid) {
            continue;
        }
        let op = Op::SessionSet {
            sid: sid.to_string(),
            at: (!at.is_empty()).then(|| at.to_string()),
            field: field.to_string(),
            value,
        };
        out.push_back((s.device.clone(), op.to_line(&row.hlc)?));
    }
    Ok(out)
}

/// 正在导出的一个会话。
struct Cursor {
    session_id: i64,
    sid: String,
    /// 包头（JSON 一行）。这个会话拆出的每个会话包都以它开头。
    header: String,
    /// 导出到这条消息为止（已发布部分的末条）。
    upto: i64,
    after: i64,
    /// 正在攒的会话包。
    chunk: String,
    emitted: bool,
    done: bool,
}

/// 快照内容的生产者：一次给出一个分片。库只在短读取里用，文件 IO 与序列化都在库锁之外。
struct Producer {
    ctx: Ctx,
    s: Settings,
    keys: Arc<Keys>,
    small: VecDeque<(String, String)>,
    /// 还没出的删除记录：`(墓碑的键, 墓碑)`。
    tombs: VecDeque<(String, StateRow)>,
    /// 取好了链、还没写进分片的删除记录（op 行）。
    tomb_lines: VecDeque<String>,
    sessions: VecDeque<i64>,
    cursor: Option<Cursor>,
    /// 攒好了、还没写进分片的会话包：`(线程, 会话包文本)`。
    ready: VecDeque<(String, String)>,
    registers: VecDeque<(&'static str, String)>,
    /// 快照内容引用到的附件对象。
    blobs: BTreeSet<String>,
    /// 导出时新算出来、本机还没登记的附件对象。
    fresh: Vec<LocalBlob>,
}

impl Producer {
    /// 下一个分片的明文与行数；内容都出完了返回 `None`。
    fn next_part(&mut self) -> Result<Option<(Vec<u8>, u64)>> {
        // 每出一片都把资格重新核对一遍：快照写着写着，队列里可能多了没发布的行（其中
        // 暂缓行的内容就在寄存器里）、范围也可能变了。不再合格就中止——调用方按写失败
        // 清掉已经传上去的对象，下一轮重新来过。
        let now = now_ms();
        let ok = db::read(self.ctx.db(), |conn| settled(conn, &self.s, now))?;
        if !ok {
            return Err(SyncError::other(
                "写快照期间队列或范围变了，这份快照作废",
            ));
        }
        fn push(plain: &mut String, origin: &str, line: &str) {
            plain.push_str(origin);
            plain.push('\t');
            plain.push_str(line);
            plain.push('\n');
        }
        let mut plain = String::new();
        let mut lines = 0u64;
        while plain.len() < PART_TARGET {
            if let Some((origin, line)) = self.small.pop_front() {
                push(&mut plain, &origin, &line);
                lines += 1;
                continue;
            }
            // 删除记录排在会话包前面：接收方先知道哪些分支已经删了，重发来的同一条分支
            // 就不会先落地、再删一遍。
            if let Some(line) = self.next_tomb()? {
                push(&mut plain, &self.s.device, &line);
                lines += 1;
                continue;
            }
            if let Some((sid, bundle)) = self.next_bundle()? {
                let line = Op::SessionBundle { sid, bundle }.to_line(db::HLC_ZERO)?;
                push(&mut plain, &self.s.device, &line);
                lines += 1;
                continue;
            }
            let Some((scope, key)) = self.registers.pop_front() else {
                break;
            };
            let row = db::read(self.ctx.db(), |conn| db::state_get(conn, scope, &key))?;
            let Some(StateRow {
                hlc,
                value: Some(value),
                ..
            }) = row
            else {
                continue;
            };
            let (origin, op) = files::parse_register(&value)
                .ok_or_else(|| SyncError::corrupt(format!("快照寄存器 {scope}/{key} 读不出来")))?;
            if !ids::is_device_id(&origin) {
                return Err(SyncError::corrupt(format!(
                    "快照寄存器 {scope}/{key} 的来源设备不合规"
                )));
            }
            if let Op::SkillPut { files, .. } = &op {
                self.blobs
                    .extend(files.iter().filter_map(|file| file.blob.clone()));
            }
            push(&mut plain, &origin, &op.to_line(&hlc)?);
            lines += 1;
        }
        Ok((lines > 0).then(|| (plain.into_bytes(), lines)))
    }

    /// 下一条删除记录，连同它带的链（§7.5）；出完了返回 `None`。一条链可以有几十 KB，
    /// 所以不和开头那些小 op 一起先读进内存，出到这里才分批去库里取。
    fn next_tomb(&mut self) -> Result<Option<String>> {
        while self.tomb_lines.is_empty() && !self.tombs.is_empty() {
            let tombs = &self.tombs;
            let chains: Vec<String> = db::read(self.ctx.db(), |conn| {
                let mut chains = Vec::new();
                let mut bytes = 0usize;
                for (key, _) in tombs.iter().take(TOMB_BATCH) {
                    let chain = db::state_get(conn, scope::TOMB_CHAIN, key)?
                        .and_then(|row| row.value)
                        .unwrap_or_default();
                    bytes += chain.len();
                    chains.push(chain);
                    if bytes >= TOMB_BATCH_BYTES {
                        break;
                    }
                }
                Ok(chains)
            })?;
            for chain in chains {
                let Some((key, row)) = self.tombs.pop_front() else {
                    break;
                };
                let Some((sid, tip)) = key.split_once('/') else {
                    continue;
                };
                let op = Op::SessionDelete {
                    sid: sid.to_string(),
                    tip: (!tip.is_empty()).then(|| tip.to_string()),
                    purge: merge::tomb_purge(&row),
                    chain,
                };
                self.tomb_lines.push_back(op.to_line(&row.hlc)?);
            }
        }
        Ok(self.tomb_lines.pop_front())
    }

    /// 下一个会话包；会话都导出完了返回 `None`。
    fn next_bundle(&mut self) -> Result<Option<(String, String)>> {
        loop {
            if let Some(found) = self.ready.pop_front() {
                return Ok(Some(found));
            }
            if self.cursor.as_ref().is_some_and(|cursor| cursor.done) {
                self.cursor = None;
            }
            if self.cursor.is_none() {
                let Some(session_id) = self.sessions.pop_front() else {
                    return Ok(None);
                };
                // 开始导出之前就被删掉、或不再参与同步的会话直接跳过：那次删除在本机
                // 日志里排在这个快照的向量之后，接收方接着读日志就知道了。
                self.cursor = self.open_cursor(session_id)?;
                continue;
            }
            self.read_page()?;
        }
    }

    fn open_cursor(&self, session_id: i64) -> Result<Option<Cursor>> {
        let s = &self.s;
        let found = db::read(self.ctx.db(), |conn| {
            let Some(session) = map::session_map(conn, session_id)? else {
                return Ok(None);
            };
            if session.sealed == SEAL_LOCAL
                || !publish::eligible(conn, s, &session.channel, &session.chat_id)?
            {
                return Ok(None);
            }
            // 只导出已经发布的部分：没发布的尾巴由日志照常送达，不抢在前面。
            let tip = bundle::published_tip(conn, session_id)?;
            let mut header = bundle::branch_header(conn, &s.device, &session)?;
            header.tip = tip.as_ref().map(|(_, gid)| gid.clone());
            Ok(Some((session.sid, header, tip.map_or(0, |(id, _)| id))))
        })?;
        let Some((sid, header, upto)) = found else {
            return Ok(None);
        };
        Ok(Some(Cursor {
            session_id,
            sid,
            header: serde_json::to_string(&header)?,
            upto,
            after: 0,
            chunk: String::new(),
            emitted: false,
            done: false,
        }))
    }

    /// 读当前会话的下一页，把攒满的会话包放进 `ready`。
    fn read_page(&mut self) -> Result<()> {
        let Some(cursor) = self.cursor.as_mut() else {
            return Ok(());
        };
        let rows = if cursor.after >= cursor.upto {
            Vec::new()
        } else {
            let (session_id, sid, after, upto) = (
                cursor.session_id,
                cursor.sid.clone(),
                cursor.after,
                cursor.upto,
            );
            let env = &self.ctx.env;
            let page = db::read(self.ctx.db(), |conn| {
                match map::session_map(conn, session_id)? {
                    Some(session) if session.sid == sid => Ok(Some(bundle::branch_page(
                        conn, env, &session, after, upto, PAGE_ROWS, PAGE_BYTES,
                    )?)),
                    _ => Ok(None),
                }
            })?;
            // 导出到一半会话没了：已经写出去的那半截会在接收方留下一条残缺的分支，
            // 整个快照作废。
            page.ok_or_else(|| {
                SyncError::other("有会话在写快照的中途被删除或移出了同步，这个快照作废")
            })?
        };
        if rows.is_empty() {
            cursor.done = true;
        }
        let policy = ArtifactPolicy {
            inline_max: ARTIFACT_INLINE_MAX,
            message_max: OP_ARTIFACTS_MAX,
            keys: Some(self.keys.as_ref()),
        };
        for mut row in rows {
            cursor.after = row.message_id;
            self.fresh
                .extend(wire::fill_artifacts(&self.ctx.env, &mut row, policy));
            self.blobs
                .extend(row.msg.refs.iter().map(|file| file.blob.clone()));
            self.blobs.extend(
                row.msg
                    .artifacts
                    .iter()
                    .filter_map(|artifact| artifact.blob.clone()),
            );
            let line = serde_json::to_string(&row.msg)?;
            if !cursor.chunk.is_empty() && cursor.chunk.len() + line.len() > merge::BUNDLE_CHUNK {
                self.ready
                    .push_back((cursor.sid.clone(), std::mem::take(&mut cursor.chunk)));
                cursor.emitted = true;
            }
            if cursor.chunk.is_empty() {
                cursor.chunk.push_str(&cursor.header);
                cursor.chunk.push('\n');
            }
            cursor.chunk.push_str(&line);
            cursor.chunk.push('\n');
        }
        if cursor.after >= cursor.upto {
            cursor.done = true;
        }
        if cursor.done && (!cursor.chunk.is_empty() || !cursor.emitted) {
            if cursor.chunk.is_empty() {
                // 一条已发布的消息都没有：只带包头，线程的开头信息靠它。
                cursor.chunk.push_str(&cursor.header);
                cursor.chunk.push('\n');
            }
            self.ready
                .push_back((cursor.sid.clone(), std::mem::take(&mut cursor.chunk)));
            cursor.emitted = true;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 写快照
// ---------------------------------------------------------------------------

/// 现在该不该写一个新的全量快照。只看 op 的数量与上一个快照的年纪；本机够不够格由
/// [`Writer::begin`] 判断。
pub fn due(conn: &Connection, s: &Settings, now: i64) -> Result<bool> {
    if !s.scopes.sessions {
        return Ok(false);
    }
    if (0..SNAP_RETRY_MS).contains(&(now - maint(conn)?.snap_try_ms)) {
        return Ok(false);
    }
    let log = db::log_state(conn)?;
    let mut total = log.seq;
    let mut snaps: Vec<SnapRef> = log.snap.into_iter().collect();
    // 排在本机前面的活跃设备数：它们先写。
    let mut rank = 0u64;
    for (device, peer) in db::peers(conn)? {
        total = total.saturating_add(peer.fetched);
        if peer.is_active(now) && peer.own_until.is_none() && device.as_str() < s.device.as_str() {
            rank += 1;
        }
        snaps.extend(peer.snap);
    }
    if snaps.is_empty() {
        return Ok(total >= SNAP_FIRST_OPS.saturating_add(rank.saturating_mul(SNAP_STAGGER_OPS)));
    }
    let stagger = i64::try_from(rank)
        .unwrap_or(i64::MAX)
        .saturating_mul(SNAP_STAGGER_MS);
    // 现有的快照（本机的、各设备 head 里带着的）只要有一个还够新，就不写：不只看「最新」
    // 的那一个，哪个快照的时刻或覆盖数不对劲，都不会让本机一个接一个地写。
    Ok(snaps.iter().all(|snap| {
        let since = total.saturating_sub(snap.ops);
        // 别的设备的快照，时刻在取回时已经压到「不晚于本机第一次见到它」（见
        // `pull`）。还比现在晚，是本机的时钟往回拨过：它有多旧说不清，按够旧算。
        let age = if snap.ts > now {
            i64::MAX
        } else {
            now - snap.ts
        };
        let age = age.saturating_sub(stagger);
        (since >= SNAP_OPS && age >= SNAP_AGE_MS)
            || (since >= SNAP_OPS_SLOW && age >= SNAP_AGE_SLOW_MS)
    }))
}

/// 一个正在写的快照。引擎每轮让它走一段（[`Writer::step`]），不长时间占着引擎。
pub struct Writer {
    name: String,
    dev: String,
    ts: i64,
    keys: Arc<Keys>,
    upload_attachments: bool,
    vector: BTreeMap<String, VectorEntry>,
    producer: Option<Producer>,
    parts: Vec<PartRef>,
    /// 已经（或正要）传上去的对象。
    objects: Vec<String>,
}

impl Writer {
    /// 开始写一个快照。本机现在不够格（见模块说明）返回 `None`。
    pub async fn begin(
        ctx: &Ctx,
        transport: &dyn SyncTransport,
        s: &Settings,
        keys: &Arc<Keys>,
        upload_attachments: bool,
    ) -> Result<Option<Writer>> {
        let now = now_ms();
        let ready = {
            let s = s.clone();
            offload(ctx, move |ctx| {
                db::read(ctx.db(), |conn| settled(conn, &s, now))
            })
            .await?
        };
        if !ready {
            return Ok(None);
        }
        // 上次写到一半留下的对象先清掉。
        cleanup(ctx, transport, &s.device).await?;
        let name = ids::snapshot_name(now);
        let planned = {
            let (s, name) = (s.clone(), name.clone());
            offload(ctx, move |ctx| {
                // 接下来那次读取要把所有会话过一遍：记下试过的时刻，不够格也不必每轮都查。
                db::write(ctx.db(), |tx| {
                    let mut record = maint(tx)?;
                    record.snap_try_ms = now;
                    put_maint(tx, &record)
                })?;
                let Some(plan) = db::read(ctx.db(), |conn| read_plan(conn, &s, now))? else {
                    return Ok(None);
                };
                let small = small_lines(&plan, &s)?;
                db::write(ctx.db(), |tx| {
                    let mut record = maint(tx)?;
                    record.writing = Some(name);
                    put_maint(tx, &record)
                })?;
                Ok(Some((plan, small)))
            })
            .await?
        };
        let Some((plan, small)) = planned else {
            return Ok(None);
        };
        let sessions = plan
            .sessions
            .iter()
            .filter(|session| session.sealed != SEAL_LOCAL)
            .map(|session| session.session_id)
            .collect();
        Ok(Some(Writer {
            name,
            dev: s.device.clone(),
            ts: now,
            keys: keys.clone(),
            upload_attachments,
            vector: plan.vector,
            producer: Some(Producer {
                ctx: ctx.clone(),
                s: s.clone(),
                keys: keys.clone(),
                small,
                tombs: plan.tombs.into(),
                tomb_lines: VecDeque::new(),
                sessions,
                cursor: None,
                ready: VecDeque::new(),
                registers: plan.registers.into(),
                blobs: BTreeSet::new(),
                fresh: Vec::new(),
            }),
            parts: Vec::new(),
            objects: Vec::new(),
        }))
    }

    /// 开始写的时刻（本机时钟，Unix 毫秒）。
    pub fn started_ms(&self) -> i64 {
        self.ts
    }

    /// 加密并上传一个分片。
    async fn put_part(
        &mut self,
        transport: &dyn SyncTransport,
        part: &str,
        plain: Vec<u8>,
        lines: u64,
    ) -> Result<PartRef> {
        let path = codec::snapshot_part_path(&self.dev, &self.name, part);
        let (keys, sealed_path) = (self.keys.clone(), path.clone());
        let (object, digest) = blocking(move || {
            Ok((
                codec::seal_log_object(&keys, &sealed_path, &plain),
                sha256_hex(&plain),
            ))
        })
        .await?;
        // 先记下再传：传到一半失败，放弃时也会把它删掉。
        self.objects.push(path.clone());
        transport::put_checked(transport, &path, object).await?;
        Ok(PartRef {
            name: part.to_string(),
            digest,
            lines,
        })
    }

    /// 往前写，最多 `budget` 这么久。写完了返回快照指针（本机记录也已经更新：下一次写
    /// head 会带上它）；没写完返回 `None`，下一轮接着来。出错时调用方应当
    /// [`Writer::abort`]。
    pub async fn step(
        &mut self,
        ctx: &Ctx,
        transport: &dyn SyncTransport,
        s: &Settings,
        budget: Duration,
    ) -> Result<Option<SnapRef>> {
        // 快照的内容按开始时的范围与计划生成。范围（会话/记忆/技能/附件/通道）在这期间
        // 变了，已经写出去的分片就是旧范围的内容——整份作废，不能收尾发布。
        if !self.settings_match(s) {
            return Err(SyncError::other(
                "写快照期间同步范围变了，这份快照作废",
            ));
        }
        let started = Instant::now();
        loop {
            let mut producer = self
                .producer
                .take()
                .ok_or_else(|| SyncError::other("这个快照已经中止"))?;
            let (producer, part) = blocking(move || {
                let part = producer.next_part()?;
                Ok((producer, part))
            })
            .await?;
            self.producer = Some(producer);
            let Some((plain, lines)) = part else {
                return self.finish(ctx, transport, s).await.map(Some);
            };
            if self.parts.len() >= PARTS_MAX {
                return Err(SyncError::other("快照太大，写不下"));
            }
            let name = part_name(self.parts.len());
            let part = self.put_part(transport, &name, plain, lines).await?;
            self.parts.push(part);
            if started.elapsed() >= budget {
                return Ok(None);
            }
        }
    }

    /// 范围相关的设置是否还和开始写时一样（设备身份也一样：换了身份这份快照就不属于
    /// 本机了）。
    fn settings_match(&self, s: &Settings) -> bool {
        let Some(producer) = &self.producer else {
            return true;
        };
        producer.s.device == s.device
            && producer.s.scopes == s.scopes
            && producer.s.channels == s.channels
            && producer.s.device_name == s.device_name
            && producer.s.attachment_max == s.attachment_max
    }

    /// 收尾：在用附件清单、快照清单，然后一个事务里把本机记录换成新快照。
    async fn finish(
        &mut self,
        ctx: &Ctx,
        transport: &dyn SyncTransport,
        s: &Settings,
    ) -> Result<SnapRef> {
        let mut producer = self
            .producer
            .take()
            .ok_or_else(|| SyncError::other("这个快照已经中止"))?;
        // 导出时新算出来的附件对象（没内联的工具结果全文）：登记下来，别的设备才要得到。
        let fresh_state = if self.upload_attachments {
            BLOB_PENDING
        } else {
            BLOB_HELD
        };
        let fresh = std::mem::take(&mut producer.fresh);
        if !fresh.is_empty() {
            offload(ctx, move |ctx| {
                for batch in fresh.chunks(APPLY_BATCH) {
                    db::write(ctx.db(), |tx| {
                        for blob in batch {
                            // 同封段：本机缓存（FETCHED）也要升成待上传，引用才可取。
                            db::blob_register_or_upload(
                                tx,
                                &blob.blob_id,
                                &blob.rel_path,
                                blob.size,
                                fresh_state,
                            )?;
                        }
                        Ok(())
                    })?;
                }
                Ok(())
            })
            .await?;
        }
        let mut list = String::new();
        for blob in &producer.blobs {
            list.push_str(blob);
            list.push('\n');
        }
        let blobs = self
            .put_part(
                transport,
                BLOBS_PART,
                list.into_bytes(),
                producer.blobs.len() as u64,
            )
            .await?;
        drop(producer);

        let manifest = Manifest {
            v: FORMAT_VERSION,
            dev: self.dev.clone(),
            ts: self.ts,
            vector: self.vector.clone(),
            parts: self.parts.clone(),
            blobs: Some(blobs),
        };
        // 收尾时还要拿自己的向量和旧快照比覆盖关系（下面的阻塞闭包会拿走一份）。
        let manifest_for_check = manifest.clone();
        let path = codec::snapshot_path(&self.dev, &self.name);
        let (object, snap) = {
            let (keys, path, name) = (self.keys.clone(), path.clone(), self.name.clone());
            blocking(move || {
                let plain = serde_json::to_vec(&manifest)?;
                let snap = SnapRef {
                    name,
                    digest: sha256_hex(&plain),
                    ts: manifest.ts,
                    ops: manifest.ops(),
                };
                Ok((codec::seal_log_object(&keys, &path, &plain), snap))
            })
            .await?
        };
        self.objects.push(path.clone());
        transport::put_checked(transport, &path, object).await?;

        // 本机名下更早的快照对象。已公布的那一份（head 里的指针）不在这里退位：新快照
        // 只是**候选**，要等它的分片完整核验、确认覆盖（[`promote_snap`]）之后才接管
        // head——一份长度对得上但解不开的新快照，挤不掉唯一可用的旧快照。其余的（写到
        // 一半没清干净的、被更新的候选换掉的）没人引用，过宽限期直接删。
        let dir = snapshot_dir(&self.dev);
        let stale: Vec<(String, String)> = transport
            .list(&dir)
            .await?
            .into_iter()
            .filter_map(|meta| {
                snapshot_of(&meta.name)
                    .filter(|owner| *owner != self.name)
                    .map(|owner| (owner.to_string(), format!("{dir}{}", meta.name)))
            })
            .collect();
        let now = now_ms();
        let covered = self.vector.get(&self.dev).map_or(0, |entry| entry.seq);
        let parts = self.parts.len();
        // 旧对象的排删要能证明「新快照覆盖不少于旧快照的每一台设备」：只看本机那一格
        // 不够——别的设备的历史可能只有旧快照覆盖着。读不出旧清单或向量不支配时保留。
        let mut replaceable: Vec<(String, String)> = Vec::new();
        for (owner, path) in &stale {
            match open_manifest(transport, &self.keys, &self.dev, owner, None).await {
                Ok(old) if covers_dominates(&manifest_for_check, &old) => {
                    replaceable.push((owner.clone(), path.clone()));
                }
                Ok(_) => tracing::debug!("旧快照 {owner} 覆盖了更多历史，保留"),
                Err(error) => tracing::debug!("读旧快照 {owner} 的清单失败，保留: {error}"),
            }
        }
        {
            let (dev, name, snap) = (self.dev.clone(), self.name.clone(), snap.clone());
            let s = s.clone();
            offload(ctx, move |ctx| {
                db::write(ctx.db(), |tx| {
                    let current = db::device_record(tx)?.map(|record| record.id);
                    let mut record = maint(tx)?;
                    if current.as_deref() != Some(dev.as_str())
                        || record.writing.as_deref() != Some(name.as_str())
                    {
                        return Err(SyncError::other(
                            "写快照期间本机的同步身份变了，这个快照作废",
                        ));
                    }
                    // 收尾前在同一个写事务里再确认一遍：这期间队列或范围不再合格（新排
                    // 队的行、因范围暂缓的行、范围被收窄）时整份作废。寄存器在写分片时
                    // 就读过了，只有事务里的复核能把「检查与读取之间新登记的行」挡住。
                    if !settled(tx, &s, now_ms())? {
                        return Err(SyncError::other(
                            "写快照期间队列或范围变了，这份快照作废",
                        ));
                    }
                    record.writing = None;
                    record.covers.insert(snap_key(&dev, &name), covered);
                    // 本机刚写出的快照也不直接算「完整核验」：put_checked 只确认了对象
                    // 长度，还没核对过摘要。核验进度记 0，由 [`refresh_covers`] 像别的
                    // 快照一样分批下载核对；对上了才能接管指针、推进回收位置。
                    record.covers_checked.insert(
                        snap_key(&dev, &name),
                        VerifiedCover {
                            digest: snap.digest.clone(),
                            seq: covered,
                            parts,
                            next: 0,
                            versions: Vec::new(),
                        },
                    );
                    let mut log = db::log_state(tx)?;
                    let published = log.snap.as_ref().map(|snap| snap.name.as_str());
                    // 新快照对这些旧对象的替代证据：删除押在「新的这份能完整读回」上；
                    // 覆盖不到旧对象那份范围的，不排删除（旧对象还能恢复更多历史）。
                    let new_key = snap_key(&dev, &name);
                    let new_digest = snap.digest.clone();
                    if published.is_some() && published != Some(name.as_str()) {
                        // 候选保留独立指针：head 与上一份快照都先不动。
                        record.snap_attempts = 0;
                        record.snap_pending = Some(snap);
                        let when = GcWhen {
                            after_ms: now + SNAP_GRACE_MS,
                            floor: None,
                            snap: None,
                            basis: Some((new_key, new_digest)),
                            pack: None,
                            retry_ms: 0,
                        };
                        for (owner, path) in &replaceable {
                            if Some(owner.as_str()) != published {
                                queue_delete(tx, path, &when)?;
                            }
                        }
                        return put_maint(tx, &record);
                    }
                    // 没有上一份快照（首次发布，或同名重写）：直接接管，无人引用的旧
                    // 对象照旧排队，新指针下一次写 head 带出去。
                    record.snap_pending = None;
                    record.snap_attempts = 0;
                    let when = GcWhen {
                        after_ms: now + SNAP_GRACE_MS,
                        floor: None,
                        snap: Some(name.clone()),
                        basis: Some((new_key, new_digest)),
                        pack: None,
                        retry_ms: 0,
                    };
                    for (_, path) in &replaceable {
                        queue_delete(tx, path, &when)?;
                    }
                    log.snap = Some(snap);
                    log.head_dirty = true;
                    db::put_log_state(tx, &log)?;
                    put_maint(tx, &record)
                })
            })
            .await?;
        }
        self.objects.clear();
        Ok(snap)
    }

    /// 放弃这个快照：删掉已经传上去的对象。删不掉（离线）就留着记录，下次
    /// [`cleanup`] 再清。
    pub async fn abort(self, ctx: &Ctx, transport: &dyn SyncTransport) {
        let mut clean = true;
        for path in &self.objects {
            if let Err(error) = transport.delete(path).await {
                tracing::debug!("清理没写完的快照对象 {path} 失败: {error}");
                clean = false;
            }
        }
        if !clean {
            return;
        }
        if let Err(error) = clear_writing(ctx, self.name).await {
            tracing::debug!("清理快照记录失败: {error}");
        }
    }
}

/// 「正在写的快照」还是 `name` 的话，把这条记录去掉。
async fn clear_writing(ctx: &Ctx, name: String) -> Result<()> {
    offload(ctx, move |ctx| {
        db::write(ctx.db(), |tx| {
            let mut record = maint(tx)?;
            if record.writing.as_deref() == Some(name.as_str()) {
                record.writing = None;
                put_maint(tx, &record)?;
            }
            Ok(())
        })
    })
    .await
}

/// 上次写到一半就退出的快照：把它已经传上去的对象删掉。没有这种快照时不发任何请求。
/// 不能在有 [`Writer`] 活着的时候调用。
pub async fn cleanup(ctx: &Ctx, transport: &dyn SyncTransport, device: &str) -> Result<()> {
    let writing = offload(ctx, |ctx| {
        db::read(ctx.db(), |conn| Ok(maint(conn)?.writing))
    })
    .await?;
    let Some(name) = writing else {
        return Ok(());
    };
    let dir = snapshot_dir(device);
    for meta in transport.list(&dir).await? {
        if snapshot_of(&meta.name) == Some(name.as_str()) {
            transport.delete(&format!("{dir}{}", meta.name)).await?;
        }
    }
    clear_writing(ctx, name).await
}

// ---------------------------------------------------------------------------
// 吃快照
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BootReport {
    /// 吃了几个快照。
    pub applied: usize,
    /// 写进收件箱的 op 数。
    pub ops: usize,
}

/// 分片里的一行。
struct PartLine<'a> {
    origin: &'a str,
    raw: &'a str,
    kind: String,
    hlc: String,
    sid: Option<String>,
}

fn parse_part(text: &str, expect: u64) -> Result<Vec<PartLine<'_>>> {
    let mut out = Vec::new();
    for line in text.split('\n').filter(|line| !line.is_empty()) {
        let (origin, raw) = line
            .split_once('\t')
            .filter(|(origin, _)| ids::is_device_id(origin))
            .ok_or_else(|| SyncError::corrupt("快照分片里有不合规的行"))?;
        let parsed = OpLine::parse(raw)?;
        out.push(PartLine {
            origin,
            raw,
            kind: parsed.kind,
            hlc: parsed.hlc,
            sid: parsed.sid,
        });
    }
    if out.len() as u64 != expect {
        return Err(SyncError::corrupt("快照分片的行数与清单不符"));
    }
    Ok(out)
}

/// 把一个分片的 op 写进收件箱：序号记为 0，段名是 `snap-<快照名>-<分片名>`，先停在
/// 暂存状态（[`INBOX_SNAP_STAGE`]）——这份快照的分片还没全部通过校验，合入不看它们。
/// 悬空的到期清理按「段取回的时刻」算，这里一并记下；快照携带的 HLC 等转正时才观察，
/// 不让没提交的批次推动逻辑时钟。
fn insert_part(ctx: &Ctx, seg: &str, lines: &[PartLine<'_>], sessions_on: bool) -> Result<usize> {
    let now = now_ms();
    let mut inserted = 0usize;
    for batch in lines.chunks(APPLY_BATCH) {
        inserted += db::write(ctx.db(), |tx| {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO sync_inbox(origin, seg, seq, hlc, sid, op, state) \
                 VALUES (?1, ?2, 0, ?3, ?4, ?5, ?6)",
            )?;
            let mut count = 0usize;
            let mut origins: BTreeSet<&str> = BTreeSet::new();
            for line in batch {
                // 会话范围没开的设备不收会话类的 op（与合入时的处理一致）。
                if !sessions_on && merge::SESSION_OPS.contains(&line.kind.as_str()) {
                    continue;
                }
                stmt.execute(params![
                    line.origin,
                    seg,
                    line.hlc,
                    line.sid,
                    line.raw,
                    INBOX_SNAP_STAGE
                ])?;
                origins.insert(line.origin);
                count += 1;
            }
            // 悬空的 op 到期清理时按「段取回的时刻」算。
            for origin in origins {
                db::state_put_json(
                    tx,
                    scope::INSEG,
                    &pull::inseg_key(origin, seg),
                    db::HLC_ZERO,
                    &InSeg {
                        deps: BTreeMap::new(),
                        ts: now,
                        to: 0,
                    },
                )?;
            }
            Ok(count)
        })?;
    }
    Ok(inserted)
}

/// 没有提交完的快照批：应用快照中断后它们留在收件箱里等下一次 [`apply`] 开头清理；
/// 引擎启动后的第一轮合入先把它们清掉，免得一直压着「本机够格写快照」（收件箱非空就
/// 不写）。只在登记了「正在吃快照」的时候查一下，平时零开销。
///
/// 两个来源都清：
/// - 新版本的暂存行（[`INBOX_SNAP_STAGE`]，登记的前缀之内与之外都算——暂存行本来就
///   只在应用期间有效）；
/// - 升级前中断的半批：旧版本把行直接写成可合入（[`INBOX_PENDING`]）而且中途停住的话，
///   标记还指着它。按标记指向的合法前缀清掉这批历史行，标记一并撤销；别的快照已经
///   提交的行不在前缀里，照常合入。
pub(crate) fn clear_uncommitted(ctx: &Ctx) -> Result<()> {
    let applying = db::read(ctx.db(), |conn| Ok(maint(conn)?.applying.clone()))?;
    let Some(name) = applying else {
        // 没有标记也把残留的暂存行清掉（理论上传不下来，防旧库搬家之类的意外）。
        db::write(ctx.db(), |tx| {
            tx.execute(
                "DELETE FROM sync_inbox WHERE seq = 0 AND state = ?1",
                params![INBOX_SNAP_STAGE],
            )?;
            Ok(())
        })?;
        return Ok(());
    };
    let legal = ids::parse_snapshot_name(&name).is_some();
    db::write(ctx.db(), |tx| {
        // 新版本的暂存批：登记之内与之外都清。
        tx.execute(
            "DELETE FROM sync_inbox WHERE seq = 0 AND state = ?1",
            params![INBOX_SNAP_STAGE],
        )?;
        // 升级前中断的半批：按标记指向的合法前缀清历史行；标记不合法（库被动过）就不
        // 碰可合入的行，只把标记清掉——乱删会丢内容，留着最多白合入一遍（幂等）。
        if legal {
            tx.execute(
                "DELETE FROM sync_inbox WHERE seq = 0 AND state = ?1 AND seg LIKE ?2",
                params![INBOX_PENDING, format!("snap-{name}-%")],
            )?;
        }
        let mut record = maint(tx)?;
        record.applying = None;
        put_maint(tx, &record)
    })?;
    Ok(())
}

/// 吃快照的最后一步（与采用向量同一个事务）：这份快照的分片已全部通过校验，把它带进
/// 收件箱的暂存行转成可合入（[`INBOX_PENDING`]），并观察快照携带的 HLC。位置也在这一个
/// 事务里挪：要么整个快照生效，要么什么都不生效。
fn promote_snapshot_rows(tx: &Connection, prefix: &str) -> Result<()> {
    let upper = format!("{prefix}\u{10ffff}");
    tx.execute(
        "UPDATE sync_inbox SET state = ?1 WHERE seq = 0 AND state = ?2 AND seg >= ?3 AND seg < ?4",
        params![INBOX_PENDING, INBOX_SNAP_STAGE, prefix, upper],
    )?;
    let newest: Option<String> = tx.query_row(
        "SELECT max(hlc) FROM sync_inbox WHERE seq = 0 AND seg >= ?1 AND seg < ?2",
        params![prefix, upper],
        |row| row.get(0),
    )?;
    if let Some(hlc) = newest {
        sync_db::hlc_observe(tx, &hlc)?;
    }
    Ok(())
}

/// 吃一个快照：各分片的 op 先以暂存状态进收件箱，全部分片都通过校验后，在采用各设备
/// 日志读取位置的同一个事务里转成可合入。中途失败的话位置没动、暂存行也不可合入，下次
/// 从头再来（重复的 op 靠幂等消化）；启动、放弃、改吃别的快照都先清掉暂存批。
///
/// 位置挪过之后，快照的向量之前的日志本机不再去读，那里面的续写与删除只能从快照里的
/// 结果得知：本机手里要是留着某条已被删除的分支更早的样子，光凭墓碑记的末条认不出来。
/// 所以吃完要记下「得清点一遍」（[`db::KV_STALE_CHECK`]），由引擎在各设备的日志读到头
/// 之后拿墓碑带的链去对（[`merge::sweep_stale`]）。
async fn apply(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    s: &Settings,
    dev: &str,
    snap: &SnapRef,
    manifest: &Manifest,
) -> Result<usize> {
    let prefix = format!("snap-{}-", snap.name);
    {
        let name = snap.name.clone();
        offload(ctx, move |ctx| {
            db::write(ctx.db(), |tx| {
                let mut record = maint(tx)?;
                // 上次吃到一半留下的：这个快照自己的；以及没吃完就放下了的另一个快照
                // 的——它的位置没挪过，那些 op 本机之后会从日志或这个快照里再读到，不必
                // 留着和这个快照的混在一起处理。
                let mut leftovers = vec![name.clone()];
                if let Some(other) = record.applying.take() {
                    if other != name && ids::parse_snapshot_name(&other).is_some() {
                        leftovers.push(other);
                    }
                }
                for name in &leftovers {
                    tx.execute(
                        "DELETE FROM sync_inbox WHERE seq = 0 AND seg LIKE ?1",
                        params![format!("snap-{name}-%")],
                    )?;
                }
                // 别的暂存批也一律不留：它们要么属于上面的 leftovers，要么是更早中断的
                // 半批（它的快照已经不在这台设备的 head 里了）。
                tx.execute(
                    "DELETE FROM sync_inbox WHERE seq = 0 AND state = ?1",
                    params![INBOX_SNAP_STAGE],
                )?;
                record.applying = Some(name);
                put_maint(tx, &record)
            })
        })
        .await?;
    }
    let mut inserted = 0usize;
    for part in &manifest.parts {
        let path = codec::snapshot_part_path(dev, &snap.name, &part.name);
        let object = transport.get(&path, codec::SEGMENT_OBJECT_MAX).await?;
        let seg = format!("{prefix}{}", part.name);
        let (ctx, keys, digest, lines, sessions_on) = (
            ctx.clone(),
            keys.clone(),
            part.digest.clone(),
            part.lines,
            s.scopes.sessions,
        );
        inserted += blocking(move || {
            let plain = codec::open_log_object(&keys, &path, &object, codec::SEGMENT_PLAIN_MAX)?;
            drop(object);
            if sha256_hex(&plain) != digest {
                return Err(SyncError::corrupt(format!("{path} 的摘要与清单里记的不符")));
            }
            let text = std::str::from_utf8(&plain)
                .map_err(|_| SyncError::corrupt("快照分片不是合法的 UTF-8"))?;
            insert_part(&ctx, &seg, &parse_part(text, lines)?, sessions_on)
        })
        .await?;
    }
    let (vector, me, prefix) = (manifest.vector.clone(), s.device.clone(), prefix);
    offload(ctx, move |ctx| {
        db::write(ctx.db(), |tx| {
            promote_snapshot_rows(tx, &prefix)?;
            adopt_vector(tx, &vector, &me)
        })
    })
    .await?;
    Ok(inserted)
}

/// 吃快照的最后一步（与转正暂存行同一个事务，见 [`promote_snapshot_rows`]）：把各设备
/// 日志的读取位置挪到快照的版本向量。在调用方的写事务里。
fn adopt_vector(tx: &Connection, vector: &BTreeMap<String, VectorEntry>, me: &str) -> Result<()> {
    for (device, entry) in vector {
        if device == me || entry.seq == 0 {
            continue;
        }
        let mut peer = db::peer_state(tx, device)?;
        if entry.seq > peer.fetched {
            peer.fetched = entry.seq;
            peer.fetched_seg = entry.seg.clone();
            peer.processed = peer.processed.max(entry.seq);
            // 这个位置是快照说的，不是本机读出来的：下次读它的 head 时核对。
            peer.from_snap = true;
            // 链上更早的记录用不上了；留下快照停住的那一个，接着读时核对。
            tx.execute(
                "DELETE FROM sync_state WHERE scope = ?1 AND key >= ?2 AND key < ?3",
                params![
                    scope::RSEG,
                    format!("{device}/"),
                    format!("{device}/\u{10ffff}")
                ],
            )?;
            if let Some(seg) = &entry.seg {
                db::state_put_json(
                    tx,
                    scope::RSEG,
                    &pull::rseg_key(device, entry.seq),
                    db::HLC_ZERO,
                    seg,
                )?;
            }
        }
        if peer.error.as_deref() == Some(NEED_SNAPSHOT) && peer.fetched >= peer.floor {
            peer.error = None;
            peer.error_rounds = 0;
            peer.error_since_ms = 0;
        }
        // 下一次拉取重新读它的 head。
        peer.head_tag.clear();
        db::put_peer_state(tx, device, &peer)?;
    }
    let mut record = maint(tx)?;
    record.applying = None;
    put_maint(tx, &record)?;
    db::kv_put_json(tx, db::KV_STALE_CHECK, &true)?;
    // 回执变了。
    let mut log = db::log_state(tx)?;
    log.head_dirty = true;
    db::put_log_state(tx, &log)
}

/// 有没有设备的日志要靠快照才能接着读。
pub fn needs_bootstrap(conn: &Connection) -> Result<bool> {
    Ok(db::peers(conn)?
        .iter()
        .any(|(_, peer)| peer.error.as_deref() == Some(NEED_SNAPSHOT)))
}

/// 给「需要的日志已被回收」的设备找快照补齐（§9.5）。吃完之后调用方应当再拉取一轮。
/// 找不到合用的快照（还没同步过来、校验不过）就留到下一轮。
pub async fn bootstrap(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    s: &Settings,
    memo: &mut Memo,
) -> Result<BootReport> {
    let mut report = BootReport::default();
    let mut fetches = 0usize;
    let mut tried: HashSet<String> = HashSet::new();
    while report.applied < APPLIES_PER_ROUND {
        let (peers, own) = offload(ctx, |ctx| {
            db::read(ctx.db(), |conn| {
                Ok((db::peers(conn)?, db::log_state(conn)?.snap))
            })
        })
        .await?;
        let needy: Vec<(&String, u64, u64)> = peers
            .iter()
            .filter(|(_, peer)| peer.error.as_deref() == Some(NEED_SNAPSHOT))
            .map(|(device, peer)| (device, peer.floor, peer.fetched))
            .collect();
        if needy.is_empty() {
            break;
        }
        // 各设备 head 里带着的快照，新的在前（见 [`newer`]）。本机自己写过的也算：重新读
        // 日志的时候它一样能用。
        let mut candidates: Vec<(String, SnapRef)> = peers
            .iter()
            .filter_map(|(device, peer)| peer.snap.clone().map(|snap| (device.clone(), snap)))
            .collect();
        candidates.extend(own.map(|snap| (s.device.clone(), snap)));
        candidates.sort_by(|a, b| (b.1.ops, b.1.ts).cmp(&(a.1.ops, a.1.ts)));
        let mut applied = false;
        for (dev, snap) in candidates {
            let key = snap_key(&dev, &snap.name);
            // 刚读不过的候选先退避：别让最前面的几个把每一轮的预算都堵光。
            if memo.backed_off(&key, now_ms()) {
                continue;
            }
            if !tried.insert(key.clone()) {
                continue;
            }
            if memo.get(&key).is_none() {
                if fetches >= MANIFESTS_PER_ROUND {
                    continue;
                }
                fetches += 1;
            }
            let manifest = match fetch_manifest(transport, keys, memo, &dev, &snap).await {
                Ok(manifest) => manifest,
                Err(error) if skippable(&error) => {
                    tracing::debug!("读设备 {dev} 的快照清单失败: {error}");
                    continue;
                }
                Err(error) => return Err(error),
            };
            // 合用：它覆盖到了某台设备的回收位置之后，而且比本机现在的位置靠前。
            let helps = needy.iter().any(|(device, floor, fetched)| {
                manifest
                    .vector
                    .get(*device)
                    .is_some_and(|entry| entry.seq >= *floor && entry.seq > *fetched)
            });
            if !helps {
                continue;
            }
            match apply(ctx, transport, keys, s, &dev, &snap, &manifest).await {
                Ok(ops) => {
                    report.applied += 1;
                    report.ops += ops;
                    applied = true;
                    break;
                }
                Err(error) if skippable(&error) => {
                    tracing::warn!("设备 {dev} 的快照 {} 用不了: {error}", snap.name);
                }
                Err(error) => return Err(error),
            }
        }
        if !applied {
            break;
        }
    }
    Ok(report)
}

/// 新设备直接从最新的快照起步（§9.5），不必把各设备的日志从头读一遍。只在本机还没读过
/// 任何设备的日志时做，一次运行里只试一次；没有快照、或快照用不了，就照常从头读日志。
pub async fn bootstrap_new(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    s: &Settings,
    memo: &mut Memo,
) -> Result<BootReport> {
    memo.tried_new = true;
    let none = BootReport::default();
    let fresh = offload(ctx, |ctx| {
        db::read(ctx.db(), |conn| {
            Ok(db::peers(conn)?.iter().all(|(_, peer)| peer.fetched == 0))
        })
    })
    .await?;
    if !fresh {
        return Ok(none);
    }
    let mut best: Option<(String, SnapRef)> = None;
    for meta in remote::list_heads(transport).await? {
        if meta.name == s.device {
            continue;
        }
        let head = match remote::read_head(transport, keys, &meta.name).await {
            Ok(Some(head)) => head,
            Ok(None) => continue,
            Err(error) if skippable(&error) => continue,
            Err(error) => return Err(error),
        };
        if let Some(snap) = head.snap {
            if best.as_ref().is_none_or(|(_, newest)| newer(&snap, newest)) {
                best = Some((meta.name, snap));
            }
        }
    }
    let Some((dev, snap)) = best else {
        return Ok(none);
    };
    // 刚读不过的清单等退避结束；这一轮照常从头读日志。
    if memo.backed_off(&snap_key(&dev, &snap.name), now_ms()) {
        return Ok(none);
    }
    let manifest = match fetch_manifest(transport, keys, memo, &dev, &snap).await {
        Ok(manifest) => manifest,
        Err(error) if skippable(&error) => {
            tracing::debug!("读设备 {dev} 的快照清单失败: {error}");
            return Ok(none);
        }
        Err(error) => return Err(error),
    };
    match apply(ctx, transport, keys, s, &dev, &snap, &manifest).await {
        Ok(ops) => Ok(BootReport { applied: 1, ops }),
        Err(error) if skippable(&error) => {
            tracing::warn!(
                "设备 {dev} 的快照 {} 用不了，改为从头读日志: {error}",
                snap.name
            );
            Ok(none)
        }
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// 回收
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GcReport {
    /// 这次从存储上删掉的对象数。
    pub deleted: usize,
    /// 回收位置前移到了哪里（没动为 `None`）：下一次要写 head。
    pub floor: Option<u64>,
}

/// 回收一轮。`now` 是本机时钟（Unix 毫秒）。到期的对象随时删（有间隔）；回收位置与附件
/// 每小时查一次。内存紧张时（`low_memory`）不做附件回收——它要把在用清单读进内存。
pub async fn collect_garbage(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    s: &Settings,
    memo: &mut Memo,
    now: i64,
    low_memory: bool,
) -> Result<GcReport> {
    let mut report = GcReport::default();
    if !(0..SWEEP_SPACING_MS).contains(&(now - memo.swept_ms)) {
        memo.swept_ms = now;
        report.deleted = sweep_due(ctx, transport, keys, now).await?;
    }
    let scan = offload(ctx, move |ctx| {
        db::read(ctx.db(), |conn| {
            Ok(!(0..GC_SCAN_MS).contains(&(now - maint(conn)?.gc_ms)))
        })
    })
    .await?;
    if !scan {
        return Ok(report);
    }
    refresh_covers(ctx, transport, keys, s, memo).await?;
    // 候选快照核验完成就接管 head 的快照指针（旧指针退位）；候选回收位置在依据于当前
    // 存储上重新完整复核通过之后公布。这两个都以「先不动，证据齐了再动」为顺序。
    promote_snap(ctx, transport, keys, s, memo, now).await?;
    // 先登记候选回收位置（只动本机记录），随即尝试公布：证据齐全时这一轮就能推进，
    // 大快照这一轮核不完就跨轮继续（已登记的候选下一轮接着核）。
    {
        let s = s.clone();
        offload(ctx, move |ctx| advance_floor(ctx, &s, now)).await?;
    }
    report.floor = promote_floor(ctx, transport, keys, &s.device).await?;
    if !low_memory {
        collect_blobs(ctx, transport, keys, s, memo, now).await?;
    }
    offload(ctx, move |ctx| {
        db::write(ctx.db(), |tx| {
            let mut record = maint(tx)?;
            record.gc_ms = now;
            put_maint(tx, &record)
        })
    })
    .await?;
    Ok(report)
}

/// 删掉登记过、条件已经满足的对象。带回收依据的先确认依据还能完整读到——作为依据的
/// 快照要是在删除前消失了（第三方目录只同步了一半、被人动过），这一批留着等下一轮。
/// 押在汇总包上的同理：零散段删掉之后，包里是这段日志唯一的副本。
///
/// 日志对象（段、汇总包、快照）没有恢复依据的一律不删：升级前留下的旧记录没有 `pack`
/// 字段，不能因此获得直接删除资格。附件对象（`blobs/`）的回收本来就不依赖快照依据。
async fn sweep_due(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    now: i64,
) -> Result<usize> {
    type Due = (String, Option<(String, String)>, Option<PackGuard>);
    let (due, seeds): (Vec<Due>, HashMap<String, Vec<String>>) = offload(ctx, move |ctx| {
        db::read(ctx.db(), |conn| {
            let rows = db::state_scan(conn, scope::GC, "")?;
            if rows.is_empty() {
                return Ok((Vec::new(), HashMap::new()));
            }
            let log = db::log_state(conn)?;
            let seeds: HashMap<String, Vec<String>> = maint(conn)?
                .covers_checked
                .into_iter()
                .map(|(key, entry)| (key, entry.versions))
                .collect();
            let due: Vec<Due> = rows
                .into_iter()
                .filter_map(|(path, row)| {
                    let when = row.json::<GcWhen>()?;
                    (now >= when.after_ms
                        && now >= when.retry_ms
                        && when.floor.is_none_or(|floor| log.head_floor >= floor)
                        && when
                            .snap
                            .is_none_or(|snap| log.head_snap.as_ref() == Some(&snap)))
                    .then(|| (path, when.basis, when.pack))
                })
                .take(SWEEP_BATCH)
                .collect();
            Ok((due, seeds))
        })
    })
    .await?;
    if due.is_empty() {
        return Ok(0);
    }
    let started = Instant::now();
    // 复核（下载、核对摘要）与删除共享这一轮的时间预算：总预算再按依据份数均分，排在
    // 后面的依据也分得到一份；每份在自己的份额里核对。删除阶段固定留四分之一，复核把
    // 预算吃光时至少还能删掉已经确认过的那些（且每批至少尝试删一个）。
    let verify_end = started + SWEEP_BUDGET.mul_f64(0.75);
    let mut checked: HashMap<(String, String), BasisCheck> = HashMap::new();
    let mut packs_ok: HashMap<(String, String), bool> = HashMap::new();
    let distinct: Vec<(String, String)> = {
        let mut seen = HashSet::new();
        due.iter()
            .filter_map(|(_, basis, _)| basis.clone())
            .filter(|basis| seen.insert(basis.clone()))
            .collect()
    };
    let mut share = SWEEP_BUDGET.mul_f64(0.75) / distinct.len().max(1) as u32;
    share = share.max(Duration::from_millis(500));
    let mut deletable: Vec<String> = Vec::new();
    let mut due_left: Vec<Due> = Vec::new();
    // 这轮核验不过（不是预算不够）的行：退避一会儿，别让排在前面的坏依据每轮占满批次。
    let mut retry: Vec<String> = Vec::new();
    let mut parked: Vec<String> = Vec::new();
    for (path, basis, pack) in due {
        if basis.is_none() && pack.is_none() && !path.starts_with("blobs/") {
            // 升级前留下的旧记录：没有恢复依据的日志对象不能删。退到很久以后，
            // 别让它一直占着批次把后面的正常记录挡住。
            tracing::debug!("{path} 没有恢复依据（旧格式的回收记录），先不删");
            parked.push(path);
            continue;
        }
        let Some(basis) = basis else {
            let Some(pack) = pack else {
                deletable.push(path);
                continue;
            };
            let key = (pack.name.clone(), pack.digest.clone());
            let ok = match packs_ok.get(&key) {
                Some(&ok) => ok,
                None => {
                    let slice = share.min(verify_end.saturating_duration_since(Instant::now()));
                    if slice.is_zero() {
                        due_left.push((path, None, Some(pack)));
                        continue;
                    }
                    let ok = pack_intact(transport, keys, &pack, Instant::now() + slice).await;
                    packs_ok.insert(key, ok);
                    ok
                }
            };
            if ok {
                deletable.push(path);
            } else {
                tracing::debug!("汇总包 {} 这轮读不回来，押着它的零散段先不删", pack.name);
                retry.push(path.clone());
                due_left.push((path, None, Some(pack)));
            }
            continue;
        };
        let verdict = match checked.get(&basis) {
            Some(&verdict) => verdict,
            None => {
                let slice = share.min(verify_end.saturating_duration_since(Instant::now()));
                if slice.is_zero() {
                    due_left.push((path, Some(basis), pack));
                    continue;
                }
                let seed = seeds.get(&basis.0).cloned().unwrap_or_default();
                let verdict = basis_intact(
                    transport,
                    keys,
                    &basis,
                    &seed,
                    Instant::now() + slice,
                )
                .await;
                checked.insert(basis.clone(), verdict);
                verdict
            }
        };
        match verdict {
            BasisCheck::Intact => deletable.push(path),
            BasisCheck::Deferred => {
                tracing::debug!("回收依据 {} 这轮没核验完，押着它的对象先不删", basis.0);
                // 和核验不过一样退避：否则排在批头的慢依据每 60 秒重读一遍，后面的
                // 行永远轮不到（一次判定必须连续走完，不能跨轮续用前缀）。
                retry.push(path.clone());
                due_left.push((path, Some(basis), pack));
            }
            BasisCheck::Broken => {
                tracing::debug!("回收依据 {} 这轮核验不过，押着它的对象先不删", basis.0);
                retry.push(path.clone());
                due_left.push((path, Some(basis), pack));
            }
        }
    }
    let mut done: Vec<String> = Vec::new();
    for (index, path) in deletable.into_iter().enumerate() {
        // 总预算用完也要给这一批留一次删除机会：一次慢复核不该让回收永远零进展。
        if index > 0 && started.elapsed() > SWEEP_BUDGET {
            break;
        }
        match transport.delete(&path).await {
            Ok(()) | Err(TransportError::NotFound(_)) => done.push(path),
            Err(error) => {
                tracing::debug!("删除 {path} 失败（下次再试）: {error}");
                break;
            }
        }
    }
    let deleted = done.len();
    offload(ctx, move |ctx| {
        db::write(ctx.db(), |tx| {
            for path in &done {
                db::state_del(tx, scope::GC, path)?;
            }
            for (paths, retry_ms) in [(&retry, now + GC_RETRY_MS), (&parked, now + GC_PARK_MS)] {
                for path in paths {
                    let Some(mut when) =
                        db::state_get(tx, scope::GC, path)?.and_then(|row| row.json::<GcWhen>())
                    else {
                        continue;
                    };
                    when.retry_ms = retry_ms;
                    db::state_put_json(tx, scope::GC, path, db::HLC_ZERO, &when)?;
                }
            }
            Ok(())
        })
    })
    .await?;
    Ok(deleted)
}

/// 一次依据复核的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BasisCheck {
    /// 清单与全部数据片都对上了。
    Intact,
    /// 这一轮的预算 / 时间用完，还没核完。下一次从头再来（版本一致的分片只花一次 stat）。
    Deferred,
    /// 核验对不上：清单读不到或摘要不符、某个数据片对不上。这轮不删，下一轮再试。
    Broken,
}

/// 不可逆删除或公布候选回收位置前，回收依据（`<设备>/<快照名>`、清单摘要）是否仍可
/// 恢复：下载清单、解密、核对摘要；再把**全部分片在一个调用里**过一遍——强版本与登记
/// 时一致（`seed` 里同一数据片的版本）就跳过下载，否则下载核对摘要。
///
/// 必须一次走完：跨轮保留的「前缀已经核过」不能当作「现在还完好」的证据（两次之间
/// 分片可能被换掉）。超时返回 [`BasisCheck::Deferred`]，下一次从头再来——版本一致的
/// 分片仍然只花一次 stat。`seed` 是登记时核验过的版本（[`VerifiedCover::versions`]）。
async fn basis_intact(
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    basis: &(String, String),
    seed: &[String],
    deadline: Instant,
) -> BasisCheck {
    let Some((dev, name)) = basis.0.split_once('/') else {
        return BasisCheck::Broken;
    };
    let path = codec::snapshot_path(dev, name);
    let digest = basis.1.clone();
    let object = match transport
        .get(&path, codec::sealed_max(MANIFEST_PLAIN_MAX))
        .await
    {
        Ok(object) => object,
        Err(_) => return BasisCheck::Broken,
    };
    let keys_for_parse = keys.clone();
    let (dev_for_parse, name_for_parse) = (dev.to_string(), name.to_string());
    let manifest = match blocking(move || {
        let plain = codec::open_log_object(&keys_for_parse, &path, &object, MANIFEST_PLAIN_MAX)?;
        if sha256_hex(&plain) != digest {
            return Err(SyncError::corrupt("回收依据的摘要不符"));
        }
        Manifest::parse(&plain, &dev_for_parse, &name_for_parse)
    })
    .await
    {
        Ok(manifest) => manifest,
        Err(_) => return BasisCheck::Broken,
    };
    for (index, part) in manifest.parts.iter().enumerate() {
        if Instant::now() >= deadline {
            return BasisCheck::Deferred;
        }
        let part_path = codec::snapshot_part_path(dev, name, &part.name);
        let meta = transport::stat(transport, &part_path).await.ok().flatten();
        let trusted = match (&meta, seed.get(index)) {
            (Some(meta), Some(saved)) if meta.strong_version && !saved.is_empty() => {
                meta.tag() == *saved
            }
            _ => false,
        };
        if !trusted && verify_snapshot_part(transport, keys, dev, name, part).await.is_err() {
            return BasisCheck::Broken;
        }
    }
    BasisCheck::Intact
}

/// 核对完一个分片后记下它当时在存储端的版本：核前核后两次元数据一致（且都是强版本）
/// 才算这个版本对应刚核对过的内容；拿不到就记空串。这个版本只当后续复核的**种子**
/// （省掉没变过的分片重复下载），不是「现在还完好」的证据。
async fn verified_part_version(
    transport: &dyn SyncTransport,
    path: &str,
    before: Option<&transport::ObjectMeta>,
) -> String {
    let Some(before) = before.filter(|meta| meta.strong_version) else {
        return String::new();
    };
    match transport::stat(transport, path).await.ok().flatten() {
        Some(after) if after.strong_version && after.tag() == before.tag() => after.tag(),
        _ => String::new(),
    }
}

/// 登记时记下的各分片版本（[`Maint::covers_checked`]）。回收/晋升复核用它省掉没变过的
/// 分片的重复下载；拿不到就是空表（全部分片重新下载核对）。
async fn basis_seed(ctx: &Ctx, key: &str, digest: &str) -> Result<Vec<String>> {
    let key = key.to_string();
    let digest = digest.to_string();
    offload(ctx, move |ctx| {
        db::read(ctx.db(), |conn| {
            Ok(maint(conn)?
                .covers_checked
                .get(&key)
                .filter(|entry| entry.digest == digest)
                .map(|entry| entry.versions.clone())
                .unwrap_or_default())
        })
    })
    .await
}

/// 新快照的向量是否覆盖了旧快照的每一段：每个设备记的位置都不比旧的少。只有这样才能
/// 说「旧快照能恢复的，新快照也恢复得了」，删旧对象不会减少可达的历史。
fn covers_dominates(new: &Manifest, old: &Manifest) -> bool {
    old.vector
        .iter()
        .all(|(device, entry)| new.vector.get(device).is_some_and(|mine| mine.seq >= entry.seq))
}

/// 一个快照覆盖了本机日志到哪里。它的向量里记的必须确实是本机这条日志上的位置。
fn coverage(manifest: &Manifest, me: &str, log: &LogState, entries: &[SegEntry]) -> u64 {
    let Some(entry) = manifest.vector.get(me) else {
        return 0;
    };
    if entry.seq == 0 || entry.seq > log.seq {
        return 0;
    }
    if entry.seq <= log.floor {
        // 已经回收的部分核对不了，也不需要了。
        return entry.seq;
    }
    match entries.iter().find(|mine| mine.to == entry.seq) {
        Some(mine) if entry.seg.as_ref() == Some(&mine.seg_ref()) => entry.seq,
        _ => 0,
    }
}

/// 各设备 head 里带着的快照覆盖了本机日志到哪里：没算过的读它的清单算出来记下，已经
/// 没人提的快照从记录里去掉。算出来的只是「清单说覆盖到哪里」；推进回收位置之前，还要
/// 把分片分批下载下来核对摘要（[`Maint::covers_checked`]），全对上了才算数——清单先到、
/// 分片迟到或漏掉（第三方同步文件夹会这样）的快照不能当成日志的备份。
async fn refresh_covers(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    s: &Settings,
    memo: &mut Memo,
) -> Result<()> {
    let me = s.device.clone();
    let (refs, known, known_checked, log, entries) = offload(ctx, move |ctx| {
        db::read(ctx.db(), |conn| {
            let record = maint(conn)?;
            let log = db::log_state(conn)?;
            let mut refs: Vec<(String, SnapRef)> = db::peers(conn)?
                .into_iter()
                .filter_map(|(device, peer)| peer.snap.map(|snap| (device, snap)))
                .collect();
            refs.extend(log.snap.clone().map(|snap| (me.clone(), snap)));
            // 候选快照还没接管 head：把它也放进名单，分批核验才有预算轮到它。
            if let Some(pending) = &record.snap_pending {
                if log.snap.as_ref().map(|snap| snap.name.as_str()) != Some(pending.name.as_str())
                {
                    refs.push((me.clone(), pending.clone()));
                }
            }
            Ok((refs, record.covers, record.covers_checked, log, seal::seg_entries(conn)?))
        })
    })
    .await?;
    let mut covers: BTreeMap<String, u64> = BTreeMap::new();
    // 没人提的快照的核验进度也去掉；有人提的留着——预算轮不到它时依据不丢。
    let mut checked: BTreeMap<String, VerifiedCover> = known_checked
        .iter()
        .filter(|(key, _)| {
            refs.iter()
                .any(|(dev, snap)| &snap_key(dev, &snap.name) == *key)
        })
        .map(|(key, entry)| (key.clone(), entry.clone()))
        .collect();
    let mut fetches = 0usize;
    let mut verify_budget = VERIFY_PARTS_PER_ROUND;
    for (dev, snap) in &refs {
        let key = snap_key(dev, &snap.name);
        // 覆盖算过没有：算过就用记下的；没算过要读清单。
        let need_cover = known.get(&key).is_none();
        // 核验还没做完的也要清单（分片名单在清单里）：只读内存里有的，不额外占预算。
        let label = checked.get(&key);
        let need_verify = label.is_none_or(|entry| !entry.complete() || entry.digest != snap.digest);
        if !need_cover && !need_verify {
            covers.insert(key.clone(), known[&key]);
            continue;
        }
        // 清单刚读不过的先退避，下一轮再试；已有的核验进度照旧留着。
        if memo.backed_off(&key, now_ms()) {
            if let Some(seq) = known.get(&key) {
                covers.insert(key, *seq);
            }
            continue;
        }
        if memo.get(&key).is_none() {
            if fetches >= MANIFESTS_PER_ROUND {
                if let Some(seq) = known.get(&key) {
                    covers.insert(key, *seq);
                }
                continue;
            }
            fetches += 1;
        }
        let manifest = match fetch_manifest(transport, keys, memo, dev, snap).await {
            Ok(manifest) => manifest,
            Err(error) if skippable(&error) => {
                tracing::debug!("读设备 {dev} 的快照清单失败: {error}");
                if let Some(seq) = known.get(&key) {
                    covers.insert(key, *seq);
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        if need_cover {
            covers.insert(key.clone(), coverage(&manifest, &s.device, &log, &entries));
        } else {
            covers.insert(key.clone(), known[&key]);
        }
        // 分批核验这份快照的分片：换清单（换了内容）就从头核，并记下核对当时存储端的
        // 对象版本（晋升候选前按它复核，省掉重复下载）。
        let mut entry = checked.remove(&key).unwrap_or_default();
        if entry.digest != snap.digest {
            entry.next = 0;
            entry.versions.clear();
        }
        entry.digest = snap.digest.clone();
        entry.seq = covers[&key];
        entry.parts = manifest.parts.len();
        entry.versions.truncate(entry.next.min(entry.parts));
        while entry.next < entry.parts && verify_budget > 0 {
            let index = entry.next;
            let part = &manifest.parts[index];
            let part_path = codec::snapshot_part_path(dev, &snap.name, &part.name);
            let meta = transport::stat(transport, &part_path).await.ok().flatten();
            match verify_snapshot_part(transport, keys, dev, &snap.name, part).await {
                Ok(()) => {
                    let version = verified_part_version(transport, &part_path, meta.as_ref()).await;
                    if entry.versions.len() <= index {
                        entry.versions.resize(index + 1, String::new());
                    }
                    entry.versions[index] = version;
                    entry.next += 1;
                    verify_budget -= 1;
                }
                Err(error) if skippable(&error) => {
                    tracing::debug!(
                        "设备 {dev} 的快照 {} 分片 {} 核验不过（下一轮再试）: {error}",
                        snap.name,
                        part.name
                    );
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        checked.insert(key, entry);
    }
    if covers != known || checked != known_checked {
        offload(ctx, move |ctx| {
            db::write(ctx.db(), |tx| {
                let mut record = maint(tx)?;
                record.covers = covers;
                record.covers_checked = checked;
                put_maint(tx, &record)
            })
        })
        .await?;
    }
    Ok(())
}

/// 回收本机的日志，分两步（都只在本机登记）：
/// 这一步登记**候选**回收位置：把要删的段与汇总包按宽限期排队、把位置记进
/// [`Maint::floor_pending`]，但日志索引、`log.floor` 与 head 都不动——新设备仍然按旧
/// 边界读得到这些对象。公布（推进位置、改写 head）由 [`promote_floor`] 在依据的快照于
/// 当前存储上完整复核通过之后进行。
/// 另一件独立的事照旧直接做：已经打进汇总包、head 也改用汇总包的零散段对象，排队删除。
fn advance_floor(ctx: &Ctx, s: &Settings, now: i64) -> Result<()> {
    db::write(ctx.db(), |tx| {
        let log = db::log_state(tx)?;
        let mut entries = seal::seg_entries(tx)?;
        let packs = upload::pack_entries(tx)?;
        let mut record = maint(tx)?;
        // 零散段删掉之后，汇总包是这段日志唯一的副本：删除必须押在「包还能完整读回」上，
        // 找不到能证明的包（旧记录、包被回收）就不删。
        for entry in &mut entries {
            if entry.packed && entry.up && !entry.gone && entry.to <= log.head_packed {
                let Some(guard) = packs
                    .iter()
                    .find(|pack| pack.from <= entry.to && entry.to <= pack.to)
                    .map(|pack| PackGuard {
                        device: s.device.clone(),
                        name: pack.name.clone(),
                        digest: pack.digest.clone(),
                    })
                else {
                    continue;
                };
                let when = GcWhen {
                    after_ms: now + LOG_GRACE_MS,
                    floor: None,
                    snap: None,
                    basis: None,
                    pack: Some(guard),
                    retry_ms: 0,
                };
                queue_delete(tx, &codec::segment_path(&s.device, &entry.name), &when)?;
                entry.gone = true;
                seal::put_seg_entry(tx, entry)?;
            }
        }
        // 上一个候选还没公布：不叠加新的，先把旧的等出结果。
        if record.floor_pending.is_some() {
            return Ok(());
        }
        // 最近的几个段始终留着。
        let Some(keep) = entries
            .len()
            .checked_sub(KEEP_SEGMENTS + 1)
            .map(|index| entries[index].to)
        else {
            return Ok(());
        };
        // 回收依据只能是完整核验过的快照：清单和它声明的每个分片都对上了（分批核验，
        // 没核验完的不算数）。挑覆盖得最远的那一份，把它钉在候选记录里，公布与删除之前
        // 都会在当前存储上再复核。
        let basis = record
            .covers_checked
            .iter()
            .filter(|(_, entry)| entry.complete())
            .max_by_key(|(_, entry)| entry.seq)
            .map(|(key, entry)| (key.clone(), entry.digest.clone(), entry.seq));
        let covered = basis.as_ref().map_or(0, |(_, _, seq)| *seq);
        let basis = basis.map(|(key, digest, _)| (key, digest));
        // 没有活跃的其他设备时不用等回执：以后加入的设备从快照起步。
        let mut acked = u64::MAX;
        for (_, peer) in db::peers(tx)? {
            if peer.is_active(now) && peer.own_until.is_none() {
                acked = acked.min(peer.acks.get(&s.device).copied().unwrap_or(0));
            }
        }
        let limit = covered.min(acked).min(log.up_seq).min(keep);
        // 回收位置落在汇总包的末尾；还没打过汇总包的，落在段的末尾。
        let target = match packs
            .iter()
            .map(|pack| pack.to)
            .filter(|to| *to <= limit)
            .max()
        {
            Some(to) => Some(to),
            None if packs.is_empty() && log.rollup.is_none() => entries
                .iter()
                .filter(|entry| entry.up && entry.to <= limit)
                .map(|entry| entry.to)
                .max(),
            None => None,
        };
        let Some(floor) = target.filter(|floor| *floor > log.floor) else {
            return Ok(());
        };
        let Some(basis) = basis else {
            return Ok(());
        };
        // 删除要等带着新回收位置的 head 写出去之后才动手（`floor` 守卫）；删之前与公布
        // 之前都还会按 basis 复核一次。
        let when = GcWhen {
            after_ms: now + LOG_GRACE_MS,
            floor: Some(floor),
            snap: None,
            basis: Some(basis.clone()),
            pack: None,
            retry_ms: 0,
        };
        for entry in entries.iter().filter(|entry| entry.to <= floor) {
            if !entry.gone {
                queue_delete(tx, &codec::segment_path(&s.device, &entry.name), &when)?;
            }
        }
        for pack in packs.iter().filter(|pack| pack.to <= floor) {
            queue_delete(tx, &codec::rollup_path(&s.device, &pack.name), &when)?;
        }
        record.floor_pending = Some(PendingFloor {
            floor,
            basis,
            tries: 0,
        });
        put_maint(tx, &record)
    })
}

/// 公布候选回收位置（[`Maint::floor_pending`]）：把依据的快照在当前存储上重新完整复核
/// （跨轮分批，进度按数据片的对象版本绑定，见 [`BasisProgress`]），全部对上才推进日志
/// 索引、把新回收位置记进 head（下一次写 head 带出去）。在那之前新设备仍按旧边界读得
/// 到这些对象，引导不会被没有依据的位置卡住。依据的快照不再被任何设备提着（它的设备
/// 已经换掉并删了它）时，撤销候选并释放押在它上面的删除——这份依据已经不能恢复什么，
/// 死等只会堵死往后的回收。返回公布的回收位置（没动为 `None`）。
async fn promote_floor(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    device: &str,
) -> Result<Option<u64>> {
    let pending = offload(ctx, |ctx| {
        db::read(ctx.db(), |conn| Ok(maint(conn)?.floor_pending))
    })
    .await?;
    let Some(pending) = pending else {
        return Ok(None);
    };
    // 依据的快照还有没有设备提着（谁的 head 里还有它）？
    let key = pending.basis.0.clone();
    let device = device.to_string();
    let referenced = offload(ctx, move |ctx| {
        db::read(ctx.db(), |conn| {
            let own = db::log_state(conn)?
                .snap
                .map(|snap| snap_key(&device, &snap.name));
            let peers = db::peers(conn)?
                .into_iter()
                .filter_map(|(dev, peer)| peer.snap.map(|snap| snap_key(&dev, &snap.name)));
            Ok(peers.chain(own).any(|other| other == key))
        })
    })
    .await?;
    if !referenced {
        let basis = pending.basis.0.clone();
        offload(ctx, move |ctx| {
            db::write(ctx.db(), |tx| {
                let mut record = maint(tx)?;
                if record.floor_pending.as_ref().map(|p| p.basis.0.as_str()) != Some(basis.as_str())
                {
                    return Ok(());
                }
                record.floor_pending = None;
                put_maint(tx, &record)?;
                for (path, row) in db::state_scan(tx, scope::GC, "")? {
                    if row
                        .json::<GcWhen>()
                        .is_some_and(|when| when.basis.is_some_and(|b| b.0 == basis))
                    {
                        db::state_del(tx, scope::GC, &path)?;
                    }
                }
                Ok(())
            })
        })
        .await?;
        return Ok(None);
    }
    let seed = basis_seed(ctx, &pending.basis.0, &pending.basis.1).await?;
    let verdict = basis_intact(
        transport,
        keys,
        &pending.basis,
        &seed,
        Instant::now() + PROMOTE_BUDGET,
    )
    .await;
    if verdict != BasisCheck::Intact {
        // 暂缓或失效都不公布：等下一轮。连续失败达到上限说明这份依据在本机（弱版本
        // 存储 + 大快照）上无法在一次判定里核完：撤掉候选、释放押着的删除记录，保留
        // 对象与旧边界（不回收比删错好），也给后面的候选让路。
        let basis = pending.basis.0.clone();
        let gave_up = offload(ctx, move |ctx| {
            db::write(ctx.db(), |tx| {
                let mut record = maint(tx)?;
                let Some(mut pending) = record.floor_pending.clone() else {
                    return Ok(false);
                };
                if pending.basis.0 != basis {
                    return Ok(false);
                }
                pending.tries = pending.tries.saturating_add(1);
                if pending.tries < PROMOTE_TRIES {
                    record.floor_pending = Some(pending);
                    put_maint(tx, &record)?;
                    return Ok(false);
                }
                record.floor_pending = None;
                put_maint(tx, &record)?;
                for (path, row) in db::state_scan(tx, scope::GC, "")? {
                    if row
                        .json::<GcWhen>()
                        .is_some_and(|when| when.basis.is_some_and(|b| b.0 == basis))
                    {
                        db::state_del(tx, scope::GC, &path)?;
                    }
                }
                Ok(true)
            })
        })
        .await?;
        if gave_up {
            tracing::warn!(
                "回收依据 {} 连续 {PROMOTE_TRIES} 次没能在一次判定里核完，保留旧边界与对象",
                pending.basis.0
            );
        }
        return Ok(None);
    }
    let floor = pending.floor;
    let promoted = offload(ctx, move |ctx| {
        db::write(ctx.db(), |tx| {
            let mut record = maint(tx)?;
            // 候选还是这一个才动手；被并发改掉就等下一轮。
            if record.floor_pending.as_ref() != Some(&pending) {
                return Ok(None);
            }
            let mut log = db::log_state(tx)?;
            for entry in seal::seg_entries(tx)?.iter().filter(|entry| entry.to <= floor) {
                seal::del_seg_entry(tx, entry.to)?;
            }
            let packs = upload::pack_entries(tx)?;
            for pack in packs.iter().filter(|pack| pack.to <= floor) {
                upload::del_pack_entry(tx, pack.to)?;
            }
            // 押在即将删掉的汇总包上的零散段：快照已经覆盖这段日志，改押这份快照依据。
            // 不改的话包一删，它们的核验永远对不上，删除记录会一直留在队列里。
            let gone: HashSet<(String, String)> = packs
                .iter()
                .filter(|pack| pack.to <= floor)
                .map(|pack| (pack.name.clone(), pack.digest.clone()))
                .collect();
            if !gone.is_empty() {
                for (path, row) in db::state_scan(tx, scope::GC, "")? {
                    let Some(mut when) = row.json::<GcWhen>() else {
                        continue;
                    };
                    let Some(guard) = when.pack.as_ref() else {
                        continue;
                    };
                    if gone.contains(&(guard.name.clone(), guard.digest.clone())) {
                        when.pack = None;
                        when.basis = Some(pending.basis.clone());
                        db::state_put_json(tx, scope::GC, &path, db::HLC_ZERO, &when)?;
                    }
                }
            }
            if log.rollup.as_ref().is_some_and(|rollup| rollup.to <= floor) {
                log.rollup = None;
            }
            log.floor = floor;
            log.head_dirty = true;
            db::put_log_state(tx, &log)?;
            record.floor_pending = None;
            put_maint(tx, &record)?;
            Ok(Some(floor))
        })
    })
    .await?;
    if promoted.is_some() {
        // 这些段在本机留着的密文也用不上了。
        seal::sweep_outgoing(ctx)?;
    }
    Ok(promoted)
}

/// 候选快照（[`Maint::snap_pending`]）接管 head 的快照指针：分片完整核验（清单摘要与
/// 写它时一致、每个分片都对上了）、向量也覆盖到本机当前的回收边界，才取代上一份快照。
/// 上一份快照的删除押在「新指针已公布」上（宽限期 + head 守卫），它还是候选回收位置
/// 的依据时再多留一阵——公布之后就没人在乎它了，下一轮写快照时按旧对象清走。
async fn promote_snap(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    s: &Settings,
    memo: &mut Memo,
    now: i64,
) -> Result<()> {
    let pending = offload(ctx, |ctx| {
        db::read(ctx.db(), |conn| Ok(maint(conn)?.snap_pending))
    })
    .await?;
    let Some(pending) = pending else {
        return Ok(());
    };
    let key = snap_key(&s.device, &pending.name);
    // 候选必须**现在**完整可读：把这台设备的登记核验进度当种子，把全部分片在一个调用
    // 里重新确认一遍（强版本没变的只花一次 stat，其余重新下载核对）。登记时的结论
    // 不能直接当晋升依据——两份结论之间分片可能被换掉。
    let basis = (key.clone(), pending.digest.clone());
    let attempts = offload(ctx, |ctx| {
        db::read(ctx.db(), |conn| Ok(maint(conn)?.snap_attempts))
    })
    .await?;
    if attempts >= PROMOTE_TRIES {
        // 连续几次都没能在一次判定里核完（弱版本存储 + 大快照）：不再每小时重试。
        // 候选留着、旧指针不动；下一份候选会把它换掉。
        return Ok(());
    }
    let seed = basis_seed(ctx, &basis.0, &basis.1).await?;
    let verdict = basis_intact(
        transport,
        keys,
        &basis,
        &seed,
        Instant::now() + PROMOTE_BUDGET,
    )
    .await;
    if verdict != BasisCheck::Intact {
        // 没核完或对不上：记一次，候选先留着，下一轮再试（超过上限就不再试）。
        offload(ctx, move |ctx| {
            db::write(ctx.db(), |tx| {
                let mut record = maint(tx)?;
                if record.snap_pending.as_ref() == Some(&pending) {
                    record.snap_attempts = record.snap_attempts.saturating_add(1);
                    put_maint(tx, &record)?;
                }
                Ok(())
            })
        })
        .await?;
        return Ok(());
    }
    // 向量覆盖到当前的回收边界（`coverage` 会对照日志核对清单里记的位置）。
    let manifest = match fetch_manifest(transport, keys, memo, &s.device, &pending).await {
        Ok(manifest) => manifest,
        Err(error) if skippable(&error) => {
            tracing::debug!("读候选快照 {} 的清单失败（下一轮再试）: {error}", pending.name);
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let (covers_floor, previous, dir) = offload(ctx, {
        let manifest = manifest.clone();
        let device = s.device.clone();
        move |ctx| {
            db::read(ctx.db(), |conn| {
                let log = db::log_state(conn)?;
                let entries = seal::seg_entries(conn)?;
                let covered = coverage(&manifest, &device, &log, &entries);
                Ok((covered >= log.floor, log.snap.clone(), snapshot_dir(&device)))
            })
        }
    })
    .await?;
    if !covers_floor {
        // 覆盖不到当前的回收边界：它替代不了上一份快照，等下一份候选。
        return Ok(());
    }
    // 上一份快照的全部对象（清单加数据片）——接管之后它们退位，按宽限期排队。
    let old_paths: Vec<String> = match previous.as_ref().map(|previous| previous.name.clone()) {
        Some(name) if name != pending.name => transport
            .list(&dir)
            .await?
            .into_iter()
            .filter(|meta| snapshot_of(&meta.name) == Some(name.as_str()))
            .map(|meta| format!("{dir}{}", meta.name))
            .collect(),
        _ => Vec::new(),
    };
    // 上一份快照的覆盖不能比候选多：逐个设备比清单里的向量，候选都覆盖到了才允许
    // 退位。读不出上一份的清单时无法证明替代关系——不晋升：撤走旧指针会让新设备
    // 找不到那段历史（候选留在原地，下一轮再试）。
    let dominates = match previous.as_ref().filter(|previous| previous.name != pending.name) {
        Some(previous) => match fetch_manifest(transport, keys, memo, &s.device, previous).await {
            Ok(old) => covers_dominates(&manifest, &old),
            Err(error) if skippable(&error) => {
                tracing::debug!(
                    "读上一份快照 {} 的清单失败，先不晋升（下一轮再试）: {error}",
                    previous.name
                );
                return Ok(());
            }
            Err(error) => return Err(error),
        },
        None => true,
    };
    if !dominates {
        // 候选覆盖不到上一份的某些设备：保留旧指针（head 仍指向能恢复更多历史的那份），
        // 候选留在 snap_pending，等后续同步补齐或下一份候选。
        tracing::debug!(
            "候选快照 {} 的向量不支配上一份，保留旧指针",
            pending.name
        );
        return Ok(());
    }
    let device = s.device.clone();
    offload(ctx, move |ctx| {
        db::write(ctx.db(), |tx| {
            let mut record = maint(tx)?;
            // 候选还是这一个才动手；被并发改掉就等下一轮。
            if record.snap_pending.as_ref() != Some(&pending) {
                return Ok(());
            }
            let mut log = db::log_state(tx)?;
            record.snap_pending = None;
            log.snap = Some(pending.clone());
            log.head_dirty = true;
            db::put_log_state(tx, &log)?;
            put_maint(tx, &record)?;
            let Some(previous) = previous else {
                return Ok(());
            };
            if previous.name == pending.name {
                return Ok(());
            }
            let old_key = snap_key(&device, &previous.name);
            // 上一份快照还是候选回收位置的依据：先留着它（公布回收位置之前它还是唯一
            // 的恢复路径），不排删除。
            let still_basis = record
                .floor_pending
                .as_ref()
                .is_some_and(|pending| pending.basis.0 == old_key);
            // 其余押在上一份快照上的日志对象改押刚核验过的这一份，不然它们永远删不掉。
            let new_key = snap_key(&device, &pending.name);
            for (path, row) in db::state_scan(tx, scope::GC, "")? {
                let Some(mut when) = row.json::<GcWhen>() else {
                    continue;
                };
                if let Some((who, digest)) = &mut when.basis {
                    if *who == old_key {
                        *who = new_key.clone();
                        *digest = pending.digest.clone();
                        db::state_put_json(tx, scope::GC, &path, db::HLC_ZERO, &when)?;
                    }
                }
            }
            if !still_basis {
                // 旧快照的删除押在「新快照现在仍可完整读回」上：到期回收时还要再复核
                // 一遍，候选在这期间坏掉就保不住旧对象。
                let when = GcWhen {
                    after_ms: now + SNAP_GRACE_MS,
                    floor: None,
                    snap: Some(pending.name.clone()),
                    basis: Some((new_key.clone(), pending.digest.clone())),
                    pack: None,
                    retry_ms: 0,
                };
                for path in old_paths {
                    queue_delete(tx, &path, &when)?;
                }
            }
            Ok(())
        })
    })
    .await?;
    Ok(())
}

/// 本机的消息与技能还引用着的附件对象。分页读，不长时间占着库锁。
fn local_blob_refs(ctx: &Ctx) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    let mut after = 0i64;
    loop {
        let page: Vec<(i64, String)> = db::read(ctx.db(), |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT message_id, refs FROM sync_msg WHERE refs IS NOT NULL \
                 AND length(refs) > 2 AND message_id > ?1 ORDER BY message_id LIMIT 2000",
            )?;
            let rows = stmt.query_map(params![after], |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        let Some((last, _)) = page.last() else {
            break;
        };
        after = *last;
        for (_, refs) in &page {
            let Some(stored) = StoredRefs::parse(Some(refs)) else {
                continue;
            };
            out.extend(stored.files.into_iter().map(|file| file.blob));
            out.extend(
                stored
                    .artifacts
                    .into_iter()
                    .filter_map(|artifact| artifact.blob),
            );
        }
    }
    let skills: Vec<String> = db::read(ctx.db(), |conn| {
        Ok(db::state_scan(conn, scope::SKILL, "")?
            .into_iter()
            .filter_map(|(_, row)| row.value)
            .collect())
    })?;
    for value in &skills {
        if let Some((_, Op::SkillPut { files, .. })) = files::parse_register(value) {
            out.extend(files.into_iter().filter_map(|file| file.blob));
        }
    }
    Ok(out)
}

/// 回收本机传上去、已经没人引用的附件对象。依据最新的那个快照的在用清单；每个快照只看
/// 一次。第一次发现没人引用只记下时刻，过了一天、在更新的快照里仍然没人引用才删。
async fn collect_blobs(
    ctx: &Ctx,
    transport: &dyn SyncTransport,
    keys: &Arc<Keys>,
    s: &Settings,
    memo: &mut Memo,
    now: i64,
) -> Result<()> {
    let me = s.device.clone();
    let (newest, last, uploaded) = offload(ctx, move |ctx| {
        db::read(ctx.db(), |conn| {
            let mut newest: Option<(String, SnapRef)> =
                db::log_state(conn)?.snap.map(|snap| (me, snap));
            for (device, peer) in db::peers(conn)? {
                if let Some(snap) = peer.snap {
                    if newest.as_ref().is_none_or(|(_, best)| newer(&snap, best)) {
                        newest = Some((device, snap));
                    }
                }
            }
            let mut stmt = conn.prepare_cached("SELECT blob_id FROM sync_blob WHERE state = ?1")?;
            let rows = stmt.query_map(params![BLOB_UPLOADED], |row| row.get(0))?;
            let uploaded = rows.collect::<rusqlite::Result<Vec<String>>>()?;
            Ok((newest, maint(conn)?.blob_snap, uploaded))
        })
    })
    .await?;
    let Some((dev, snap)) = newest else {
        return Ok(());
    };
    let key = snap_key(&dev, &snap.name);
    if last.as_deref() == Some(key.as_str()) {
        return Ok(());
    }
    let mut unreferenced: HashSet<String> = HashSet::new();
    if !uploaded.is_empty() {
        let manifest = match fetch_manifest(transport, keys, memo, &dev, &snap).await {
            Ok(manifest) => manifest,
            Err(error) if skippable(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        // 没带在用清单的快照不能当依据。
        let Some(part) = manifest.blobs.clone() else {
            return Ok(());
        };
        let path = codec::snapshot_part_path(&dev, &snap.name, &part.name);
        let object = match transport.get(&path, codec::SEGMENT_OBJECT_MAX).await {
            Ok(object) => object,
            // 不在、或者大得不像话：这个快照当不了依据，这一次不回收附件。
            Err(TransportError::NotFound(_) | TransportError::Oversize(_)) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let keys = keys.clone();
        let in_use = blocking(move || {
            let plain = codec::open_log_object(&keys, &path, &object, codec::SEGMENT_PLAIN_MAX)?;
            if sha256_hex(&plain) != part.digest {
                return Err(SyncError::corrupt(format!("{path} 的摘要与清单里记的不符")));
            }
            let text = String::from_utf8(plain)
                .map_err(|_| SyncError::corrupt("在用附件清单不是合法的 UTF-8"))?;
            Ok(text
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect::<HashSet<String>>())
        })
        .await;
        let in_use = match in_use {
            Ok(in_use) => in_use,
            Err(error) if skippable(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        let candidates: Vec<String> = uploaded
            .into_iter()
            .filter(|blob| !in_use.contains(blob))
            .collect();
        if !candidates.is_empty() {
            // 快照之后本机又引用了的不算（它们的引用还没进任何快照）。
            let local = {
                let ctx = ctx.clone();
                blocking(move || local_blob_refs(&ctx)).await?
            };
            unreferenced = candidates
                .into_iter()
                .filter(|blob| !local.contains(blob))
                .collect();
        }
    }
    offload(ctx, move |ctx| {
        db::write(ctx.db(), |tx| {
            note_unreferenced(tx, &unreferenced, &key, now)
        })
    })
    .await
}

/// 附件回收的记账（一个事务）：`unreferenced` 是这一次看下来没人引用的附件对象，`key`
/// 是依据的那个快照。
fn note_unreferenced(
    tx: &Connection,
    unreferenced: &HashSet<String>,
    key: &str,
    now: i64,
) -> Result<()> {
    for (blob, row) in db::state_scan(tx, scope::BLOB_GC, "")? {
        if !unreferenced.contains(&blob) {
            // 又有人引用了。
            db::state_del(tx, scope::BLOB_GC, &blob)?;
            continue;
        }
        let first = row
            .value
            .as_deref()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(now);
        if now - first < BLOB_GRACE_MS {
            continue;
        }
        if db::blob_get(tx, &blob)?.is_some_and(|row| row.state == BLOB_UPLOADED) {
            queue_delete(
                tx,
                &codec::blob_path(&blob),
                &GcWhen {
                    after_ms: now,
                    floor: None,
                    snap: None,
                    basis: None,
                    pack: None,
                    retry_ms: 0,
                },
            )?;
            // 文件还在本机：以后又有人要的话（`blob.want`）重新上传。
            db::blob_set_state(tx, &blob, BLOB_HELD)?;
        }
        db::state_del(tx, scope::BLOB_GC, &blob)?;
    }
    for blob in unreferenced {
        if db::state_get(tx, scope::BLOB_GC, blob)?.is_none()
            && db::blob_get(tx, blob)?.is_some_and(|row| row.state == BLOB_UPLOADED)
        {
            db::state_put(
                tx,
                scope::BLOB_GC,
                blob,
                db::HLC_ZERO,
                None,
                Some(&now.to_string()),
            )?;
        }
    }
    let mut record = maint(tx)?;
    record.blob_snap = Some(key.to_string());
    put_maint(tx, &record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::SealOptions;
    use crate::testing::{self, Device};
    use crate::transport::file::FileTransport;
    use crate::upload::UploadOptions;
    use rclaw_core::{Message, Role};
    use rclaw_store::session::raw::SessionDeleteSync;

    /// 一台接在同一个存储上的测试设备。
    struct Node {
        device: Device,
        keys: Arc<Keys>,
        instance: String,
        memo: Memo,
    }

    fn node() -> Node {
        Node {
            device: Device::new(),
            keys: Arc::new(testing::keys()),
            instance: ids::new_instance_id(),
            memo: Memo::default(),
        }
    }

    impl Node {
        fn me(&self) -> &str {
            &self.device.settings.device
        }

        fn ctx(&self) -> &Ctx {
            &self.device.ctx
        }

        fn say(&self, chat_id: &str, role: Role, text: &str) {
            self.device
                .sessions()
                .append("desktop", chat_id, &Message::plain(role, text))
                .unwrap();
        }

        fn texts(&self, chat_id: &str) -> Vec<String> {
            self.device
                .sessions()
                .load_all("desktop", chat_id)
                .unwrap()
                .iter()
                .map(|message| {
                    message
                        .content
                        .iter()
                        .filter_map(|block| block.as_text())
                        .collect::<String>()
                })
                .collect()
        }

        fn chats(&self) -> Vec<String> {
            let mut chats: Vec<String> = self
                .device
                .sessions()
                .list_sessions(Some("desktop"))
                .unwrap()
                .into_iter()
                .map(|session| session.chat_id)
                .collect();
            chats.sort();
            chats
        }

        fn log(&self) -> LogState {
            db::read(self.ctx().db(), db::log_state).unwrap()
        }

        /// 写一次 head（不管有没有新段）。
        async fn upload(&self, transport: &FileTransport, rollup_every: usize) {
            let report = upload::upload_round(
                self.ctx(),
                transport,
                &UploadOptions {
                    keys: &self.keys,
                    device: self.me(),
                    instance: &self.instance,
                    instances: std::slice::from_ref(&self.instance),
                    running: Vec::new(),
                    pack_plain_max: 1 << 20,
                    rollup_every,
                    force_head: true,
                },
            )
            .await
            .unwrap();
            assert!(report.rotate.is_none() && report.regressed.is_none());
        }

        /// 发布：扫描、封段、上传，直到待发队列空了。
        async fn publish(&self, transport: &FileTransport, rollup_every: usize) {
            publish::scan_sessions(self.ctx(), &self.device.settings, true).unwrap();
            loop {
                let sealed = seal::seal_next(
                    self.ctx(),
                    &SealOptions {
                        keys: &self.keys,
                        device: self.me(),
                        budget: 1 << 20,
                        attachment_max: 1 << 20,
                        upload_attachments: false,
                    },
                )
                .unwrap();
                self.upload(transport, rollup_every).await;
                if sealed.is_none() {
                    break;
                }
            }
        }

        /// 拉取并合入，直到没有新东西。
        async fn sync(&mut self, transport: &FileTransport) {
            for _ in 0..8 {
                let pulled = pull::pull(
                    self.ctx(),
                    transport,
                    &self.keys,
                    self.me(),
                    pull::PULL_BUDGET,
                    &mut pull::Memo::default(),
                )
                .await
                .unwrap();
                let needs = db::read(self.ctx().db(), needs_bootstrap).unwrap();
                if needs {
                    let boot = bootstrap(
                        &self.device.ctx,
                        transport,
                        &self.keys,
                        &self.device.settings,
                        &mut self.memo,
                    )
                    .await
                    .unwrap();
                    if boot.applied > 0 {
                        continue;
                    }
                }
                let merged = self.device.merge_full();
                if pulled.ops == 0 && !pulled.more && merged.applied == 0 {
                    break;
                }
            }
        }

        /// 写一个快照并把指针写进 head。返回快照指针；本机不够格返回 `None`。
        async fn snapshot(&self, transport: &FileTransport) -> Option<SnapRef> {
            let mut writer = Writer::begin(
                self.ctx(),
                transport,
                &self.device.settings,
                &self.keys,
                false,
            )
            .await
            .unwrap()?;
            let snap = loop {
                if let Some(snap) = writer
                    .step(self.ctx(), transport, &self.device.settings, Duration::from_secs(60))
                    .await
                    .unwrap()
                {
                    break snap;
                }
            };
            self.upload(transport, 100).await;
            Some(snap)
        }

        async fn gc(&mut self, transport: &FileTransport, now: i64) -> GcReport {
            collect_garbage(
                &self.device.ctx,
                transport,
                &self.keys,
                &self.device.settings,
                &mut self.memo,
                now,
                false,
            )
            .await
            .unwrap()
        }
    }

    fn storage() -> (tempfile::TempDir, FileTransport) {
        let dir = tempfile::tempdir().unwrap();
        let transport = FileTransport::new(dir.path());
        (dir, transport)
    }

    async fn exists(transport: &FileTransport, key: &str) -> bool {
        transport::stat(transport, key).await.unwrap().is_some()
    }

    /// 在 `a` 上攒出一些内容：两个会话、一个标题、一个被删掉的会话。
    fn seed(a: &Node) {
        a.say("c1", Role::User, "一");
        a.say("c1", Role::Assistant, "二");
        a.say("c2", Role::User, "甲");
        a.say("gone", Role::User, "要删的");
        a.device
            .sessions()
            .set_session_title("desktop", "c1", "行程")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_device_started_from_a_snapshot_matches_one_that_replayed_the_log() {
        let (_g, transport) = storage();
        let a = node();
        seed(&a);
        a.publish(&transport, 100).await;
        a.device
            .sessions()
            .delete_session_with("desktop", "gone", SessionDeleteSync::Propagate)
            .unwrap();
        a.say("c2", Role::Assistant, "乙");
        a.publish(&transport, 100).await;
        let snap = a.snapshot(&transport).await.expect("写出快照");
        assert_eq!(snap.ops, a.log().seq);
        // 快照之后又有新内容：只在日志里。
        a.say("c1", Role::User, "三");
        a.publish(&transport, 100).await;

        // B 从快照起步，再接着读日志。
        let mut b = node();
        let boot = bootstrap_new(
            &b.device.ctx,
            &transport,
            &b.keys,
            &b.device.settings,
            &mut b.memo,
        )
        .await
        .unwrap();
        assert_eq!(boot.applied, 1);
        let peer = db::read(b.ctx().db(), |conn| db::peer_state(conn, a.me())).unwrap();
        assert_eq!(peer.fetched, snap.ops, "位置挪到了快照的版本向量");
        b.sync(&transport).await;

        // C 把日志从头读一遍。
        let mut c = node();
        c.memo.tried_new = true;
        c.sync(&transport).await;

        for reader in [&b, &c] {
            assert_eq!(reader.chats(), ["c1", "c2"]);
            assert_eq!(reader.texts("c1"), ["一", "二", "三"]);
            assert_eq!(reader.texts("c2"), ["甲", "乙"]);
            assert_eq!(
                reader
                    .device
                    .sessions()
                    .session_title("desktop", "c1")
                    .unwrap()
                    .as_deref(),
                Some("行程")
            );
            let (peer, inbox) = db::read(reader.ctx().db(), |conn| {
                let inbox: i64 =
                    conn.query_row("SELECT count(*) FROM sync_inbox", [], |row| row.get(0))?;
                Ok((db::peer_state(conn, a.me())?, inbox))
            })
            .unwrap();
            assert_eq!(peer.fetched, a.log().seq);
            assert_eq!(peer.processed, peer.fetched);
            assert_eq!(inbox, 0);
        }
        // 两台设备对线程的认识也一样：同一个线程、同样的消息身份。
        let threads = |reader: &Node| {
            db::read(reader.ctx().db(), |conn| {
                let mut out = Vec::new();
                for session in map::all_sessions(conn)? {
                    let gids: Vec<String> = map::session_msg_maps(conn, session.session_id)?
                        .into_iter()
                        .map(|mapped| mapped.gid)
                        .collect();
                    out.push((session.chat_id, session.sid, gids));
                }
                out.sort();
                Ok(out)
            })
            .unwrap()
        };
        assert_eq!(threads(&b), threads(&c));
    }

    #[tokio::test(start_paused = true)]
    async fn a_snapshot_lets_a_lagging_device_recognise_an_older_view_of_a_deleted_branch() {
        let (_g, transport) = storage();
        let a = node();
        a.say("c1", Role::User, "一");
        a.say("c2", Role::User, "甲");
        a.publish(&transport, 100).await;
        // B 读到这里之后很久没上线。
        let mut b = node();
        b.memo.tried_new = true;
        b.sync(&transport).await;
        assert_eq!(b.chats(), ["c1", "c2"]);
        // 这期间 A 在 c1 上接着写了一条，然后把它删了，又写了全量快照。
        a.say("c1", Role::Assistant, "二");
        a.publish(&transport, 100).await;
        a.device
            .sessions()
            .delete_session_with("desktop", "c1", SessionDeleteSync::Propagate)
            .unwrap();
        a.publish(&transport, 100).await;
        let snap = a.snapshot(&transport).await.expect("写出快照");
        // B 回来时那段日志已经被回收了：只能从快照补齐。
        db::write(b.ctx().db(), |tx| {
            let mut peer = db::peer_state(tx, a.me())?;
            peer.snap = Some(snap.clone());
            peer.floor = snap.ops;
            peer.error = Some(NEED_SNAPSHOT.to_string());
            db::put_peer_state(tx, a.me(), &peer)
        })
        .unwrap();
        let boot = bootstrap(
            &b.device.ctx,
            &transport,
            &b.keys,
            &b.device.settings,
            &mut b.memo,
        )
        .await
        .unwrap();
        assert_eq!(boot.applied, 1);
        b.device.merge_full();
        // 快照里只有「删除，末条是二」。B 手里的 c1 停在「一」，末条对不上：合入时不删，
        // 但吃过快照就记下了要清点一遍。
        assert_eq!(b.chats(), ["c1", "c2"]);
        let check = db::read(b.ctx().db(), |conn| {
            db::kv_json::<bool>(conn, db::KV_STALE_CHECK)
        })
        .unwrap();
        assert_eq!(check, Some(true));
        // 清点靠删除记录带的链认出它：这是那条已删除的分支更早的样子。
        let sweep = merge::sweep_stale(b.ctx(), &b.device.settings, true).unwrap();
        assert!(!sweep.pending && sweep.report.list_changed);
        assert_eq!(b.chats(), ["c2"]);
        assert_eq!(b.texts("c2"), ["甲"]);
    }

    #[tokio::test(start_paused = true)]
    async fn leftovers_of_a_snapshot_that_was_given_up_are_dropped_when_another_is_applied() {
        let (_g, transport) = storage();
        let a = node();
        seed(&a);
        a.publish(&transport, 100).await;
        a.snapshot(&transport).await.expect("写出快照");
        // B 上次吃另一个快照吃到一半：它的 op 进了收件箱，位置没挪。
        let mut b = node();
        let other = ids::snapshot_name(1_763_438_400_000);
        db::write(b.ctx().db(), |tx| {
            tx.execute(
                "INSERT INTO sync_inbox(origin, seg, seq, hlc, sid, op, state) \
                 VALUES ('00112233445566ff', ?1, 0, ?2, NULL, '{}', 2)",
                params![format!("snap-{other}-0000"), db::HLC_ZERO],
            )?;
            let mut record = maint(tx)?;
            record.applying = Some(other.clone());
            put_maint(tx, &record)
        })
        .unwrap();
        let boot = bootstrap_new(
            &b.device.ctx,
            &transport,
            &b.keys,
            &b.device.settings,
            &mut b.memo,
        )
        .await
        .unwrap();
        assert_eq!(boot.applied, 1);
        let (left, record) = db::read(b.ctx().db(), |conn| {
            let left: i64 = conn.query_row(
                "SELECT count(*) FROM sync_inbox WHERE seg LIKE ?1",
                params![format!("snap-{other}-%")],
                |row| row.get(0),
            )?;
            Ok((left, maint(conn)?))
        })
        .unwrap();
        assert_eq!(left, 0);
        assert_eq!(record.applying, None);
        b.sync(&transport).await;
        assert_eq!(b.chats(), ["c1", "c2", "gone"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_device_whose_data_may_be_incomplete_does_not_write_snapshots() {
        let (_g, transport) = storage();
        let a = node();
        seed(&a);
        // 还有没发布的变更。
        assert!(a.snapshot(&transport).await.is_none());
        a.publish(&transport, 100).await;

        let mark = |key: &str| {
            db::write(a.ctx().db(), |tx| {
                db::state_put(tx, scope::SKIP, key, db::HLC_ZERO, None, None)
            })
            .unwrap();
        };
        let unmark = |key: &str| {
            db::write(a.ctx().db(), |tx| db::state_del(tx, scope::SKIP, key)).unwrap();
        };
        // 丢过 op。
        mark(merge::LOSSY_KEY);
        assert!(a.snapshot(&transport).await.is_none());
        unmark(merge::LOSSY_KEY);
        // 有线程因为通道不在名单里没落地。
        mark("0123456789abcdef01234567");
        assert!(a.snapshot(&transport).await.is_none());
        unmark("0123456789abcdef01234567");
        // 收件箱里还有没处理完的 op（悬空、被隔离的也算）。
        db::write(a.ctx().db(), |tx| {
            tx.execute(
                "INSERT INTO sync_inbox(origin, seg, seq, hlc, sid, op, state) \
                 VALUES ('00112233445566ff', 'x', 1, ?1, NULL, '{}', 2)",
                params![db::HLC_ZERO],
            )?;
            Ok(())
        })
        .unwrap();
        assert!(a.snapshot(&transport).await.is_none());
        db::write(a.ctx().db(), |tx| {
            tx.execute("DELETE FROM sync_inbox", [])?;
            Ok(())
        })
        .unwrap();
        // 通道名单缩小过：已经发布的会话现在不参与同步了。
        let mut narrowed = a.device.settings.clone();
        narrowed.channels = vec!["telegram".into()];
        let refused = Writer::begin(a.ctx(), &transport, &narrowed, &a.keys, false)
            .await
            .unwrap();
        assert!(refused.is_none());

        // 都没有了：可以写。没写成的那几次没有在存储上留下东西。
        let snap = a.snapshot(&transport).await.expect("写出快照");
        let objects = transport.list(&snapshot_dir(a.me())).await.unwrap();
        assert!(objects
            .iter()
            .all(|meta| snapshot_of(&meta.name) == Some(snap.name.as_str())));
        assert_eq!(a.log().snap, Some(snap));
    }

    /// `a` 发布出十几个段（每条消息一个），打成汇总包。
    async fn long_log(a: &Node, transport: &FileTransport) {
        for index in 0..12 {
            a.say("c1", Role::User, &format!("第 {index} 条"));
            a.publish(transport, 4).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_log_is_reclaimed_only_when_acked_and_covered_and_late_readers_use_the_snapshot() {
        let (_g, transport) = storage();
        let mut a = node();
        let mut b = node();
        long_log(&a, &transport).await;
        let now = now_ms();
        let hour = GC_SCAN_MS + 1;

        // 没有快照：不回收。
        assert_eq!(a.gc(&transport, now).await.floor, None);
        // 有了快照，但 B 还没有回执。
        b.sync(&transport).await;
        b.publish(&transport, 100).await;
        a.sync(&transport).await;
        a.publish(&transport, 100).await;
        a.snapshot(&transport).await.expect("写出快照");
        let before = a.log();
        // B 的回执是在 A 之前的位置：把它改小，模拟「还没处理到」。
        db::write(a.ctx().db(), |tx| {
            let mut peer = db::peer_state(tx, b.me())?;
            peer.acks.insert(a.me().to_string(), 0);
            db::put_peer_state(tx, b.me(), &peer)
        })
        .unwrap();
        assert_eq!(a.gc(&transport, now + hour).await.floor, None);

        // B 处理完并回执之后：回收位置前移到一个汇总包的末尾，最近的几个段留着。
        b.sync(&transport).await;
        b.publish(&transport, 100).await;
        db::write(a.ctx().db(), |tx| {
            let mut peer = db::peer_state(tx, b.me())?;
            peer.head_tag.clear();
            db::put_peer_state(tx, b.me(), &peer)
        })
        .unwrap();
        a.sync(&transport).await;
        let packs = db::read(a.ctx().db(), upload::pack_entries).unwrap();
        let report = a.gc(&transport, now + 2 * hour).await;
        let floor = report.floor.expect("回收位置前移");
        assert!(floor > before.floor && floor < before.seq);
        assert!(packs.iter().any(|pack| pack.to == floor));
        let doomed: Vec<String> = packs
            .iter()
            .filter(|pack| pack.to <= floor)
            .map(|pack| codec::rollup_path(a.me(), &pack.name))
            .collect();
        assert!(!doomed.is_empty());

        // 带着新回收位置的 head 还没写出去：对象一个都不删，过了宽限期也不删。
        let later = now + 2 * hour + LOG_GRACE_MS + SWEEP_SPACING_MS;
        a.gc(&transport, later).await;
        for path in &doomed {
            assert!(exists(&transport, path).await, "{path}");
        }
        // head 写出去之后才删。
        a.upload(&transport, 100).await;
        assert_eq!(a.log().head_floor, floor);
        a.gc(&transport, later + SWEEP_SPACING_MS).await;
        for path in &doomed {
            assert!(!exists(&transport, path).await, "{path}");
        }

        // 回收之后才加入的设备：日志的前一段读不到了，靠快照补齐，结果与 B 一样。
        a.say("c1", Role::User, "最后一条");
        a.publish(&transport, 100).await;
        let mut c = node();
        c.memo.tried_new = true;
        c.sync(&transport).await;
        b.sync(&transport).await;
        assert_eq!(c.texts("c1"), b.texts("c1"));
        assert_eq!(c.texts("c1").len(), 13);
        assert_eq!(c.texts("c1"), a.texts("c1"));
        let peer = db::read(c.ctx().db(), |conn| db::peer_state(conn, a.me())).unwrap();
        assert_eq!(peer.error, None);
        assert_eq!(peer.fetched, a.log().seq);
    }

    #[tokio::test(start_paused = true)]
    async fn unreferenced_attachments_are_deleted_a_day_and_a_snapshot_later() {
        let (_g, transport) = storage();
        let mut a = node();
        seed(&a);
        a.publish(&transport, 100).await;
        // 一个传上去了、但没有任何消息引用的附件对象。
        let blob = "ab".repeat(32);
        let path = codec::blob_path(&blob);
        transport.put(&path, b"x".to_vec()).await.unwrap();
        db::write(a.ctx().db(), |tx| {
            db::blob_register(tx, &blob, "uploads/x.bin", 1, BLOB_UPLOADED)?;
            Ok(())
        })
        .unwrap();
        let now = now_ms();
        let hour = GC_SCAN_MS + 1;
        let state = |a: &Node| {
            db::read(a.ctx().db(), |conn| {
                Ok(db::blob_get(conn, &blob)?.map(|row| row.state))
            })
            .unwrap()
        };

        // 第一个快照里没有它：只记下。
        a.snapshot(&transport).await.expect("写出快照");
        a.gc(&transport, now).await;
        assert!(exists(&transport, &path).await);
        assert_eq!(state(&a), Some(BLOB_UPLOADED));
        // 同一个快照不看第二遍；不到一天、换了快照也不删。
        a.gc(&transport, now + 30 * hour).await;
        assert!(exists(&transport, &path).await);
        db::write(a.ctx().db(), |tx| {
            let mut record = maint(tx)?;
            record.snap_try_ms = 0;
            put_maint(tx, &record)
        })
        .unwrap();
        a.say("c2", Role::User, "乙");
        a.publish(&transport, 100).await;
        a.snapshot(&transport).await.expect("写出第二个快照");
        a.gc(&transport, now + 2 * hour).await;
        assert!(exists(&transport, &path).await);
        assert_eq!(state(&a), Some(BLOB_UPLOADED));

        // 过了一天，又一个快照里仍然没有它：登记删除，本机改回「没上传」。
        a.say("c2", Role::User, "丙");
        a.publish(&transport, 100).await;
        a.snapshot(&transport).await.expect("写出第三个快照");
        a.gc(&transport, now + 26 * hour).await;
        assert_eq!(state(&a), Some(BLOB_HELD));
        a.gc(&transport, now + 26 * hour + SWEEP_SPACING_MS).await;
        assert!(!exists(&transport, &path).await);
    }

    /// 该不该写新快照：现有的快照里有一个够新就不写；时刻不对劲的快照不会让本机一直不写，
    /// 也不会让本机一个接一个地写。
    #[test]
    fn a_new_snapshot_is_due_only_when_every_known_one_is_stale() {
        let device = Device::new();
        let peer_id = "00112233445566ff";
        let now = 1_763_438_400_000i64;
        let snap = |ts: i64, ops: u64| SnapRef {
            name: ids::snapshot_name(ts),
            digest: "ab".repeat(32),
            ts,
            ops,
        };
        // 本机日志到 `seq`；`own` 是本机的快照，`theirs` 是另一台（不活跃的）设备 head 里的。
        let due_with = |seq: u64, own: Option<SnapRef>, theirs: Option<SnapRef>| {
            db::write(device.ctx.db(), |tx| {
                let mut log = db::log_state(tx)?;
                log.seq = seq;
                log.snap = own;
                db::put_log_state(tx, &log)?;
                let mut peer = db::peer_state(tx, peer_id)?;
                peer.snap = theirs;
                db::put_peer_state(tx, peer_id, &peer)
            })
            .unwrap();
            db::read(device.ctx.db(), |conn| due(conn, &device.settings, now)).unwrap()
        };
        let day = SNAP_AGE_MS;

        // 还没有快照：攒够了才写第一个。
        assert!(!due_with(SNAP_FIRST_OPS - 1, None, None));
        assert!(due_with(SNAP_FIRST_OPS, None, None));
        // 上一个快照有两天了、又攒了 5000 条：写。
        let old = snap(now - 2 * day, 1000);
        assert!(due_with(6000, Some(old.clone()), None));
        // 不到一天、或者攒得不够：不写。
        assert!(!due_with(6000, Some(snap(now - day / 2, 1000)), None));
        assert!(!due_with(5999, Some(old.clone()), None));
        // 攒得不多，但已经一个月了：写。
        assert!(due_with(2000, Some(snap(now - 31 * day, 1000)), None));
        assert!(!due_with(2000, Some(snap(now - 29 * day, 1000)), None));
        // 别的设备刚写过一个覆盖得差不多的：不写。
        assert!(!due_with(
            6000,
            Some(old.clone()),
            Some(snap(now - 3_600_000, 5500))
        ));
        // 时刻比现在还晚的快照（本机的时钟往回拨过）按够旧算：不会让本机一直不写……
        assert!(due_with(6000, None, Some(snap(now + 10 * day, 1000))));
        assert!(due_with(6000, Some(snap(now + 365 * day, 1000)), None));
        // ……本机写过新的之后也不会接着再写。
        assert!(!due_with(
            6000,
            Some(snap(now - 1000, 6000)),
            Some(snap(now + 10 * day, 1000))
        ));
    }

    #[test]
    fn snapshots_are_ordered_by_coverage_then_time() {
        let snap = |ts: i64, ops: u64| SnapRef {
            name: ids::snapshot_name(ts),
            digest: "ab".repeat(32),
            ts,
            ops,
        };
        // 覆盖得多的算新，哪怕时刻早（写者的时钟不可信）。
        assert!(newer(&snap(100, 9), &snap(9_000, 8)));
        assert!(!newer(&snap(9_000, 8), &snap(100, 9)));
        // 一样多时看时刻；完全一样不算新。
        assert!(newer(&snap(200, 9), &snap(100, 9)));
        assert!(!newer(&snap(100, 9), &snap(100, 9)));
    }

    #[test]
    fn manifests_are_checked_and_object_names_are_recognised() {
        let dev = "0123456789abcdef";
        let name = ids::snapshot_name(1_763_438_400_000);
        let manifest = Manifest {
            v: FORMAT_VERSION,
            dev: dev.to_string(),
            ts: 1_763_438_400_000,
            vector: BTreeMap::from([(dev.to_string(), VectorEntry { seq: 3, seg: None })]),
            parts: vec![PartRef {
                name: part_name(0),
                digest: sha256_hex(b"x"),
                lines: 1,
            }],
            blobs: None,
        };
        let plain = serde_json::to_vec(&manifest).unwrap();
        assert_eq!(Manifest::parse(&plain, dev, &name).unwrap(), manifest);
        assert_eq!(manifest.ops(), 3);
        // 别的设备名下、别的名字、分片名不对、版本更高。
        assert!(Manifest::parse(&plain, "00112233445566ff", &name).is_err());
        assert!(Manifest::parse(&plain, dev, &ids::snapshot_name(1)).is_err());
        let mut bad = manifest.clone();
        bad.parts[0].name = "../x".into();
        assert!(Manifest::parse(&serde_json::to_vec(&bad).unwrap(), dev, &name).is_err());
        let mut newer = manifest.clone();
        newer.v = FORMAT_VERSION + 1;
        assert!(matches!(
            Manifest::parse(&serde_json::to_vec(&newer).unwrap(), dev, &name),
            Err(SyncError::Upgrade(_))
        ));
        assert!(Manifest::parse(b"not json", dev, &name).is_err());
        // 向量里的位置大得不可能是真的。
        let mut huge = manifest.clone();
        huge.vector.insert(
            dev.to_string(),
            VectorEntry {
                seq: codec::SEQ_MAX + 1,
                seg: None,
            },
        );
        assert!(Manifest::parse(&serde_json::to_vec(&huge).unwrap(), dev, &name).is_err());

        assert_eq!(snapshot_of(&format!("{name}.snap")), Some(name.as_str()));
        assert_eq!(
            snapshot_of(&format!("{name}-0003.part")),
            Some(name.as_str())
        );
        assert_eq!(
            snapshot_of(&format!("{name}-blobs.part")),
            Some(name.as_str())
        );
        assert_eq!(snapshot_of(&format!("{name}.tmp")), None);
        assert_eq!(snapshot_of("x.snap"), None);

        // 分片里的行：来源设备、制表符、op。
        let line = Op::BlobWant {
            blob: "ab".repeat(32),
        }
        .to_line("1763438400000-0000-0123456789abcdef")
        .unwrap();
        let text = format!("{dev}\t{line}\n");
        let parsed = parse_part(&text, 1).unwrap();
        assert_eq!(
            (parsed[0].origin, parsed[0].kind.as_str()),
            (dev, "blob.want")
        );
        assert!(parse_part(&text, 2).is_err());
        assert!(parse_part(&format!("nobody\t{line}\n"), 1).is_err());
        assert!(parse_part(&format!("{line}\n"), 1).is_err());
    }
}
