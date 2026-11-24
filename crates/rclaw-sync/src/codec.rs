//! 远端对象的编解码（§9.1、§9.2）：段、汇总包、head、`space.json`。
//!
//! ```text
//! <空间根>/
//!   space.json                                  明文：格式版本、space_id、密钥校验值
//!   heads/<device_id>                           每设备一个小文件（加密）
//!   devices/<device_id>/ops/<名字>.seg          段：一批 op，压缩后加密，写入后不改
//!   devices/<device_id>/rollups/<名字>.pack     汇总包：连续若干个段，写入后不改
//!   blobs/<前两位>/<blob_id>                    附件：加密，内容寻址
//!   snapshots/…                                 全量快照
//! ```
//!
//! 这里的解码函数面对的是不可信输入：任意字节都不能让它们 panic，校验不过一律报
//! [`SyncError::Corrupt`]（格式版本更高报 [`SyncError::Upgrade`]）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::crypto::{deflate, inflate, sha256_hex, Keys};
use crate::error::{Result, SyncError};
use crate::ids;
use crate::op::FORMAT_VERSION;

/// 一个段解压后的明文上限。
pub const SEGMENT_PLAIN_MAX: usize = 128 * 1024 * 1024;
/// 一个汇总包解压后的明文上限。
pub const ROLLUP_PLAIN_MAX: usize = 256 * 1024 * 1024;
/// head 密文的上限。head 的大小不随日志增长，超过这个数一定不对。
pub const HEAD_OBJECT_MAX: usize = 256 * 1024;
/// `space.json` 的长度上限。
pub const SPACE_FILE_MAX: usize = 4096;
/// 日志序号的上限：本机库里按有符号 64 位存。远端对象里比它还大的序号一律当作损坏，
/// 核对时的加减也就不会溢出。
pub const SEQ_MAX: u64 = i64::MAX as u64;

/// 明文不超过 `plain_max` 的对象，压缩、加密之后最长能有多长。压不动的内容 deflate 之后
/// 会略长一点（不到万分之四），再加上魔数、nonce 和认证标签；这里给得宽一些。
///
/// 读远端对象时把它当作长度上限交给传输层：存储端不可信，比这还长的对象不必读进来就
/// 知道它不对。
pub const fn sealed_max(plain_max: usize) -> u64 {
    (plain_max + plain_max / 1000 + 4096) as u64
}

/// 段对象（密文）的长度上限。
pub const SEGMENT_OBJECT_MAX: u64 = sealed_max(SEGMENT_PLAIN_MAX);
/// 汇总包对象（密文）的长度上限。
pub const ROLLUP_OBJECT_MAX: u64 = sealed_max(ROLLUP_PLAIN_MAX);
/// head 里零散段清单的条数上限（汇总粒度）。
pub const ROLLUP_EVERY: usize = 100;
/// 一个同步空间里设备身份（含换下来的旧身份）的上限。`heads/` 里的名字多过它，一定不是
/// 这个空间的设备自己写出来的；全量快照的位置表（§9.5）也按它封顶。
pub const DEVICES_MAX: usize = 4096;

pub fn head_path(device_id: &str) -> String {
    format!("heads/{device_id}")
}

pub fn segment_path(device_id: &str, name: &str) -> String {
    format!("devices/{device_id}/ops/{name}")
}

pub fn rollup_path(device_id: &str, name: &str) -> String {
    format!("devices/{device_id}/rollups/{name}")
}

pub fn blob_path(blob_id: &str) -> String {
    format!("blobs/{}/{blob_id}", &blob_id[..2.min(blob_id.len())])
}

/// 全量快照的清单对象。
pub fn snapshot_path(device_id: &str, name: &str) -> String {
    format!("snapshots/{device_id}/{name}.snap")
}

/// 全量快照的一个分片：`part` 是清单里记的分片名（序号，或在用附件清单的 `blobs`）。
pub fn snapshot_part_path(device_id: &str, name: &str, part: &str) -> String {
    format!("snapshots/{device_id}/{name}-{part}.part")
}

pub const SPACE_FILE: &str = "space.json";
/// 空间已废弃的墓碑，关闭空间的设备最后写它。部分 WebDAV 服务端
/// （文档里列出的 `rclone serve webdav`）不实现 `If-None-Match`，迟到的创建会把
/// `space.json` 里的关闭标记无条件改回开放；墓碑只有关闭路径会写、创建路径从不碰，
/// 写下去就不会被迟到创建抹掉。
pub const SPACE_CLOSED_FILE: &str = "space-closed.json";
pub const HEADS_DIR: &str = "heads/";

/// 段（或汇总包里最后一个段）的引用：名字 + 明文摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegRef {
    pub name: String,
    pub digest: String,
}

/// 段头。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentHeader {
    pub v: u32,
    pub dev: String,
    /// 本段覆盖的日志序号区间（闭区间）。序号在封段时分配。
    pub from: u64,
    pub to: u64,
    /// 封段时作者对其他各设备日志的「已处理」位置。接收方在本机的对应位置达到它
    /// 之前不处理该段，由此保证因果顺序。
    #[serde(default)]
    pub deps: BTreeMap<String, u64>,
    /// 上一段。日志因此成为哈希链：缺段、被回退、出现第二个写者都能发现。
    #[serde(default)]
    pub prev: Option<SegRef>,
}

/// 解码后的段：段头 + 每行一条 op（原文）。
#[derive(Debug, Clone)]
pub struct Segment {
    pub header: SegmentHeader,
    pub lines: Vec<String>,
}

/// 段的明文：一行头，加若干行 op。
pub fn segment_plaintext(header: &SegmentHeader, lines: &[String]) -> Result<Vec<u8>> {
    let mut out = serde_json::to_vec(header)?;
    out.push(b'\n');
    for line in lines {
        debug_assert!(!line.contains('\n'));
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    Ok(out)
}

/// 解析段的明文并核对：格式版本、作者、序号区间与 op 行数一致。
pub fn parse_segment(plaintext: &[u8], device_id: &str) -> Result<Segment> {
    let text =
        std::str::from_utf8(plaintext).map_err(|_| SyncError::corrupt("段不是合法的 UTF-8"))?;
    let mut lines = text.split('\n');
    let header: SegmentHeader = serde_json::from_str(lines.next().unwrap_or_default())
        .map_err(|e| SyncError::corrupt(format!("段头无法解析: {e}")))?;
    if header.v > FORMAT_VERSION {
        return Err(SyncError::Upgrade(format!(
            "段的格式版本 {} 高于本程序支持的 {FORMAT_VERSION}",
            header.v
        )));
    }
    if header.v == 0 || header.dev != device_id {
        return Err(SyncError::corrupt("段头的作者或版本不对"));
    }
    if header.from == 0 || header.to < header.from || header.to > SEQ_MAX {
        return Err(SyncError::corrupt("段头的序号区间不合法"));
    }
    if !header
        .deps
        .iter()
        .all(|(dev, seq)| ids::is_device_id(dev) && *seq <= SEQ_MAX)
    {
        return Err(SyncError::corrupt("段头的 deps 不合法"));
    }
    if let Some(prev) = &header.prev {
        check_seg_ref(prev)?;
    }
    let lines: Vec<String> = lines
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    if lines.len() as u64 != header.to - header.from + 1 {
        return Err(SyncError::corrupt("段的 op 行数与序号区间不符"));
    }
    Ok(Segment { header, lines })
}

fn check_seg_ref(seg: &SegRef) -> Result<()> {
    if ids::parse_segment_name(&seg.name).is_none() || !ids::is_digest(&seg.digest) {
        return Err(SyncError::corrupt("段引用不合法"));
    }
    Ok(())
}

/// 压缩并加密一个段（或汇总包）的明文。
pub fn seal_log_object(keys: &Keys, path: &str, plaintext: &[u8]) -> Vec<u8> {
    keys.seal(path, &deflate(plaintext))
}

/// 解密并解压一个段（或汇总包）。
pub fn open_log_object(keys: &Keys, path: &str, object: &[u8], limit: usize) -> Result<Vec<u8>> {
    inflate(&keys.open(path, object)?, limit)
}

/// head 清单里的一个零散段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadSeg {
    pub name: String,
    pub digest: String,
    pub from: u64,
    pub to: u64,
}

impl HeadSeg {
    pub fn seg_ref(&self) -> SegRef {
        SegRef {
            name: self.name.clone(),
            digest: self.digest.clone(),
        }
    }
}

/// 汇总包指针。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollupRef {
    pub name: String,
    /// 汇总包明文的摘要。
    pub digest: String,
    /// 覆盖到的序号。
    pub to: u64,
    /// 包里最后一个段。位置正好停在包尾的读者据此核对链，不必下载整个包。
    pub last: SegRef,
}

/// 全量快照指针：名字、清单明文的摘要、写出时刻。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapRef {
    pub name: String,
    pub digest: String,
    /// 写出时刻（写者的时钟，Unix 毫秒）。
    pub ts: i64,
    /// 快照覆盖的 op 总数（各设备日志位置之和）。别的设备据此判断「自上一个快照以来又
    /// 攒了多少」，不必下载清单。
    #[serde(default)]
    pub ops: u64,
}

/// head：每台设备一个小文件，每轮重写。大小不随日志增长。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub v: u32,
    pub dev: String,
    /// 引擎每次启动随机生成，用于发现数据根被复制（§9.6）。
    pub instance: String,
    /// 最后一条 op 的序号。
    pub last_seq: u64,
    /// 最后一个段。
    #[serde(default)]
    pub last_seg: Option<SegRef>,
    /// 最近一个汇总包之后的各段。
    #[serde(default)]
    pub segs: Vec<HeadSeg>,
    #[serde(default)]
    pub rollup: Option<RollupRef>,
    /// 序号不大于它的日志已被回收，只能从全量快照取得（P3）。
    #[serde(default)]
    pub floor: u64,
    /// 回执：本机对其他各设备日志的「已处理」位置。
    #[serde(default)]
    pub acks: BTreeMap<String, u64>,
    /// 运行提示：正在本机运行的会话（线程 id）。
    #[serde(default)]
    pub running: Vec<String>,
    /// 这台设备写出的最近一个全量快照（§9.5）。别的设备只从 head 得知快照在哪里。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snap: Option<SnapRef>,
    /// 写出时刻（Unix 毫秒）。
    pub ts: i64,
}

impl Head {
    pub fn encode(&self, keys: &Keys) -> Result<Vec<u8>> {
        Ok(keys.seal(&head_path(&self.dev), &serde_json::to_vec(self)?))
    }

    /// 解密并核对一个 head。读者只信 head，所以这里把它自洽性查全：
    /// 作者、版本、零散段首尾相接、日志末尾与清单一致。
    pub fn decode(keys: &Keys, device_id: &str, object: &[u8]) -> Result<Head> {
        if object.len() > HEAD_OBJECT_MAX {
            return Err(SyncError::corrupt("head 过大"));
        }
        let plain = keys.open(&head_path(device_id), object)?;
        let head: Head = serde_json::from_slice(&plain)
            .map_err(|e| SyncError::corrupt(format!("head 无法解析: {e}")))?;
        if head.v > FORMAT_VERSION {
            return Err(SyncError::Upgrade(format!(
                "设备 {device_id} 的格式版本 {} 高于本程序支持的 {FORMAT_VERSION}",
                head.v
            )));
        }
        head.check(device_id)?;
        Ok(head)
    }

    /// 核对 head 是否自洽。写者在写出之前也查一遍：不自洽的 head 不该出门。
    pub(crate) fn check(&self, device_id: &str) -> Result<()> {
        let bad = |what: &str| SyncError::corrupt(format!("head 不自洽: {what}"));
        if self.v == 0 || self.dev != device_id || !ids::is_device_id(&self.instance) {
            return Err(bad("作者、版本或实例号"));
        }
        // 序号在本机库里按有符号 64 位存。比这还大的值不可能是真的，而且会让下面的加减
        // 溢出；回执是别的设备对各条日志的位置，同样的上限。
        if self.last_seq > SEQ_MAX
            || self.floor > SEQ_MAX
            || self.acks.values().any(|seq| *seq > SEQ_MAX)
        {
            return Err(bad("序号过大"));
        }
        if self.segs.len() > ROLLUP_EVERY * 2 {
            return Err(bad("零散段清单过长"));
        }
        if !self.acks.keys().all(|dev| ids::is_device_id(dev))
            || !self.running.iter().all(|sid| ids::is_sid(sid))
            || self.running.len() > 64
        {
            return Err(bad("回执或运行提示"));
        }
        // 快照名里带着写出时刻；指针里单独报的那个得和它是同一个数。
        if self.snap.as_ref().is_some_and(|snap| {
            ids::parse_snapshot_name(&snap.name) != Some(snap.ts) || !ids::is_digest(&snap.digest)
        }) {
            return Err(bad("快照指针"));
        }
        let mut next = match &self.rollup {
            Some(rollup) => {
                if ids::parse_rollup_name(&rollup.name).is_none_or(|(_, to)| to != rollup.to)
                    || !ids::is_digest(&rollup.digest)
                {
                    return Err(bad("汇总包指针"));
                }
                check_seg_ref(&rollup.last).map_err(|_| bad("汇总包指针"))?;
                rollup.to.checked_add(1).ok_or_else(|| bad("汇总包指针"))?
            }
            None => self.floor + 1,
        };
        if self
            .rollup
            .as_ref()
            .is_some_and(|rollup| rollup.to < self.floor)
        {
            return Err(bad("回收位置"));
        }
        let mut last: Option<SegRef> = self.rollup.as_ref().map(|rollup| rollup.last.clone());
        for seg in &self.segs {
            if ids::parse_segment_name(&seg.name) != Some((seg.from, seg.to))
                || !ids::is_digest(&seg.digest)
                || seg.from != next
            {
                return Err(bad("零散段清单"));
            }
            next = seg.to.checked_add(1).ok_or_else(|| bad("零散段清单"))?;
            last = Some(seg.seg_ref());
        }
        if self.last_seq != next - 1 {
            return Err(bad("日志末尾的序号"));
        }
        // 日志被回收到末尾（没有汇总包也没有零散段）时，末段只剩 head 自己记着。
        let tail_known = self.rollup.is_some() || !self.segs.is_empty();
        if tail_known && self.last_seg != last {
            return Err(bad("最后一个段"));
        }
        if let Some(seg) = &self.last_seg {
            check_seg_ref(seg).map_err(|_| bad("最后一个段"))?;
        } else if self.last_seq != 0 {
            return Err(bad("最后一个段"));
        }
        Ok(())
    }
}

/// 汇总包里的一个段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollupSeg {
    pub name: String,
    pub digest: String,
    pub from: u64,
    pub to: u64,
    /// 该段明文在包体里的字节数。
    pub len: u64,
}

/// 上一个汇总包。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollupLink {
    pub name: String,
    pub digest: String,
    pub to: u64,
}

/// 汇总包头。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollupHeader {
    pub v: u32,
    pub dev: String,
    pub from: u64,
    pub to: u64,
    pub segs: Vec<RollupSeg>,
    #[serde(default)]
    pub prev: Option<RollupLink>,
}

/// 解码后的汇总包：每个段保留自己的段头，接收方仍然逐段按各自的 `deps` 处理。
#[derive(Debug, Clone)]
pub struct Rollup {
    pub header: RollupHeader,
    pub segments: Vec<(RollupSeg, Segment)>,
}

/// 汇总包的明文：一行包头，后面是各段的明文原样相接。摘要按段的明文计算，同一个段
/// 在零散对象里和在汇总包里摘要相同。
pub fn rollup_plaintext(
    device_id: &str,
    segments: &[(HeadSeg, Vec<u8>)],
    prev: Option<RollupLink>,
) -> Result<Vec<u8>> {
    let (Some(first), Some(last)) = (segments.first(), segments.last()) else {
        return Err(SyncError::other("汇总包不能为空"));
    };
    let header = RollupHeader {
        v: FORMAT_VERSION,
        dev: device_id.to_string(),
        from: first.0.from,
        to: last.0.to,
        segs: segments
            .iter()
            .map(|(seg, plain)| RollupSeg {
                name: seg.name.clone(),
                digest: seg.digest.clone(),
                from: seg.from,
                to: seg.to,
                len: plain.len() as u64,
            })
            .collect(),
        prev,
    };
    let mut out = serde_json::to_vec(&header)?;
    out.push(b'\n');
    for (_, plain) in segments {
        out.extend_from_slice(plain);
    }
    Ok(out)
}

/// 解析汇总包的明文并逐段核对摘要、序号首尾相接。
pub fn parse_rollup(plaintext: &[u8], device_id: &str) -> Result<Rollup> {
    let bad = |what: &str| SyncError::corrupt(format!("汇总包不合法: {what}"));
    let split = plaintext
        .iter()
        .position(|b| *b == b'\n')
        .ok_or_else(|| bad("缺少包头"))?;
    let header: RollupHeader = serde_json::from_slice(&plaintext[..split])
        .map_err(|e| SyncError::corrupt(format!("汇总包头无法解析: {e}")))?;
    if header.v > FORMAT_VERSION {
        return Err(SyncError::Upgrade(format!(
            "汇总包的格式版本 {} 高于本程序支持的 {FORMAT_VERSION}",
            header.v
        )));
    }
    if header.v == 0 || header.dev != device_id || header.segs.is_empty() {
        return Err(bad("作者、版本或段清单"));
    }
    if let Some(prev) = &header.prev {
        if ids::parse_rollup_name(&prev.name).is_none_or(|(_, to)| to != prev.to)
            || !ids::is_digest(&prev.digest)
            || prev.to.checked_add(1) != Some(header.from)
        {
            return Err(bad("上一个汇总包"));
        }
    }
    let mut body = &plaintext[split + 1..];
    let mut next = header.from;
    let mut segments = Vec::with_capacity(header.segs.len());
    for seg in &header.segs {
        if ids::parse_segment_name(&seg.name) != Some((seg.from, seg.to)) || seg.from != next {
            return Err(bad("段清单"));
        }
        let len = usize::try_from(seg.len).map_err(|_| bad("段长度"))?;
        if len > body.len() {
            return Err(bad("包体比清单短"));
        }
        let (plain, rest) = body.split_at(len);
        body = rest;
        if sha256_hex(plain) != seg.digest {
            return Err(bad("段摘要不符"));
        }
        let segment = parse_segment(plain, device_id)?;
        if (segment.header.from, segment.header.to) != (seg.from, seg.to) {
            return Err(bad("段头与清单不符"));
        }
        next = seg.to.checked_add(1).ok_or_else(|| bad("段清单"))?;
        segments.push((seg.clone(), segment));
    }
    // 段清单不为空，所以 `next` 至少是 1。
    if !body.is_empty() || next - 1 != header.to {
        return Err(bad("包体与清单不符"));
    }
    Ok(Rollup { header, segments })
}

/// `space.json`：明文。只含格式版本、`space_id` 与密钥校验值——持有同一把空间密钥的
/// 设备才算得出同一个校验值，所以填错目录或密钥时能在读写任何数据之前发现。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceFile {
    pub format: u32,
    pub space_id: String,
    pub check: String,
    /// 空间已被「重置同步空间」废弃：仍连着它的设备应停止同步并重新配对。
    #[serde(default)]
    pub closed: bool,
}

impl SpaceFile {
    pub fn new(keys: &Keys, space_id: &str) -> SpaceFile {
        SpaceFile {
            format: FORMAT_VERSION,
            space_id: space_id.to_string(),
            check: keys.check_value(space_id),
            closed: false,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn decode(bytes: &[u8]) -> Result<SpaceFile> {
        if bytes.len() > SPACE_FILE_MAX {
            return Err(SyncError::corrupt("space.json 过大"));
        }
        serde_json::from_slice(bytes)
            .map_err(|e| SyncError::corrupt(format!("space.json 无法解析: {e}")))
    }

    /// 核对它确实是本机所在的空间。
    pub fn verify(&self, keys: &Keys, space_id: &str) -> Result<()> {
        if self.format > FORMAT_VERSION {
            return Err(SyncError::Upgrade(format!(
                "同步空间的格式版本 {} 高于本程序支持的 {FORMAT_VERSION}",
                self.format
            )));
        }
        if self.space_id != space_id || self.check != keys.check_value(space_id) {
            return Err(SyncError::other(
                "存储目录里是另一个同步空间（space_id 或密钥不符），请检查存储地址与配对码",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::SpaceKey;

    const DEV: &str = "0123456789abcdef";
    const HLC: &str = "1763438400000-0000-0123456789abcdef";

    fn keys() -> Keys {
        Keys::derive(&SpaceKey::generate())
    }

    fn line(n: u64) -> String {
        format!(r#"{{"blob":"b{n}","hlc":"{HLC}","t":"blob.want"}}"#)
    }

    fn segment(from: u64, to: u64, prev: Option<SegRef>) -> (HeadSeg, Vec<u8>) {
        let header = SegmentHeader {
            v: FORMAT_VERSION,
            dev: DEV.into(),
            from,
            to,
            deps: BTreeMap::new(),
            prev,
        };
        let lines: Vec<String> = (from..=to).map(line).collect();
        let plain = segment_plaintext(&header, &lines).unwrap();
        let seg = HeadSeg {
            name: ids::segment_name(from, to),
            digest: sha256_hex(&plain),
            from,
            to,
        };
        (seg, plain)
    }

    #[test]
    fn segment_round_trips_and_is_sealed() {
        let keys = keys();
        let (seg, plain) = segment(1, 3, None);
        let path = segment_path(DEV, &seg.name);
        let object = seal_log_object(&keys, &path, &plain);
        let opened = open_log_object(&keys, &path, &object, SEGMENT_PLAIN_MAX).unwrap();
        assert_eq!(opened, plain);
        let parsed = parse_segment(&opened, DEV).unwrap();
        assert_eq!((parsed.header.from, parsed.header.to), (1, 3));
        assert_eq!(parsed.lines.len(), 3);
        // 互换两个段的名字：路径是关联数据，解密失败。
        let other = segment_path(DEV, &ids::segment_name(1, 3));
        assert!(open_log_object(&keys, &other, &object, SEGMENT_PLAIN_MAX).is_err());
        // 作者不符。
        assert!(parse_segment(&opened, "fedcba9876543210").is_err());
    }

    #[test]
    fn segment_rejects_inconsistent_content() {
        let (_, plain) = segment(1, 3, None);
        // 少一行。
        let text = String::from_utf8(plain.clone()).unwrap();
        let cut: String = text.lines().take(3).map(|l| format!("{l}\n")).collect();
        assert!(parse_segment(cut.as_bytes(), DEV).is_err());
        // 更高的格式版本要求升级，而不是当成损坏。
        let future = text.replacen(r#""v":1"#, r#""v":99"#, 1);
        assert!(matches!(
            parse_segment(future.as_bytes(), DEV),
            Err(SyncError::Upgrade(_))
        ));
        for garbage in [&b""[..], b"\n", b"{}", b"\xff\xfe", b"{\"v\":1}\n"] {
            assert!(parse_segment(garbage, DEV).is_err());
        }
    }

    fn head(segs: Vec<HeadSeg>) -> Head {
        Head {
            v: FORMAT_VERSION,
            dev: DEV.into(),
            instance: ids::new_instance_id(),
            last_seq: segs.last().map(|s| s.to).unwrap_or(0),
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
    fn head_round_trips_and_checks_consistency() {
        let keys = keys();
        let (a, _) = segment(1, 3, None);
        let (b, _) = segment(4, 4, Some(a.seg_ref()));
        let good = head(vec![a.clone(), b.clone()]);
        let object = good.encode(&keys).unwrap();
        assert_eq!(Head::decode(&keys, DEV, &object).unwrap(), good);
        // 换到别的设备名下：路径是关联数据，解密失败。
        assert!(Head::decode(&keys, "fedcba9876543210", &object).is_err());
        // 空 head 合法。
        let empty = head(Vec::new());
        Head::decode(&keys, DEV, &empty.encode(&keys).unwrap()).unwrap();

        let reject = |head: Head| {
            assert!(Head::decode(&keys, DEV, &head.encode(&keys).unwrap()).is_err());
        };
        // 清单有缺口。
        reject(head(vec![b.clone()]));
        // 末尾序号与清单不符。
        let mut wrong_seq = good.clone();
        wrong_seq.last_seq = 9;
        reject(wrong_seq);
        // 最后一个段与清单不符。
        let mut wrong_last = good.clone();
        wrong_last.last_seg = Some(a.seg_ref());
        reject(wrong_last);
    }

    /// 持有密钥的设备（出了毛病，或者不怀好意）写出序号大到离谱的 head：报损坏，核对时
    /// 的加减不溢出。
    #[test]
    fn head_with_absurd_sequence_numbers_is_rejected_without_overflow() {
        let keys = keys();
        let reject = |head: Head| {
            assert!(Head::decode(&keys, DEV, &head.encode(&keys).unwrap()).is_err());
        };
        for huge in [u64::MAX, u64::MAX - 1, SEQ_MAX + 1] {
            // 回收位置。
            let mut floor = head(Vec::new());
            floor.floor = huge;
            floor.last_seq = huge;
            reject(floor);
            // 汇总包指针。
            let mut rollup = head(Vec::new());
            let last = SegRef {
                name: ids::segment_name(1, 1),
                digest: "0".repeat(64),
            };
            rollup.rollup = Some(RollupRef {
                name: ids::rollup_name(1, huge),
                digest: "0".repeat(64),
                to: huge,
                last: last.clone(),
            });
            rollup.last_seq = huge;
            rollup.last_seg = Some(last);
            reject(rollup);
            // 回执。
            let mut acks = head(Vec::new());
            acks.acks.insert("fedcba9876543210".into(), huge);
            reject(acks);
        }
        // 上限本身是合法的序号。
        let mut edge = head(Vec::new());
        edge.acks.insert("fedcba9876543210".into(), SEQ_MAX);
        Head::decode(&keys, DEV, &edge.encode(&keys).unwrap()).unwrap();
    }

    #[test]
    fn head_decode_never_panics_on_garbage() {
        let keys = keys();
        for garbage in [&b""[..], b"RCS1", &[0u8; 64], b"{\"v\":1}"] {
            assert!(Head::decode(&keys, DEV, garbage).is_err());
        }
        // 密钥正确、内容是任意 JSON。
        for plain in ["null", "[]", "{}", r#"{"v":1,"dev":"x"}"#] {
            let object = keys.seal(&head_path(DEV), plain.as_bytes());
            assert!(Head::decode(&keys, DEV, &object).is_err());
        }
    }

    #[test]
    fn rollup_round_trips_and_verifies_each_segment() {
        let (a, plain_a) = segment(1, 3, None);
        let (b, plain_b) = segment(4, 5, Some(a.seg_ref()));
        let pack = rollup_plaintext(
            DEV,
            &[(a.clone(), plain_a.clone()), (b.clone(), plain_b.clone())],
            None,
        )
        .unwrap();
        let parsed = parse_rollup(&pack, DEV).unwrap();
        assert_eq!((parsed.header.from, parsed.header.to), (1, 5));
        assert_eq!(parsed.segments.len(), 2);
        assert_eq!(parsed.segments[1].1.header.prev, Some(a.seg_ref()));
        // 段摘要与零散对象相同。
        assert_eq!(parsed.segments[0].0.digest, sha256_hex(&plain_a));

        // 包体被改动一个字节。
        let mut tampered = pack.clone();
        let last = tampered.len() - 2;
        tampered[last] ^= 1;
        assert!(parse_rollup(&tampered, DEV).is_err());
        // 截断。
        assert!(parse_rollup(&pack[..pack.len() - 1], DEV).is_err());
        // 多出尾巴。
        let mut extended = pack.clone();
        extended.push(b'x');
        assert!(parse_rollup(&extended, DEV).is_err());
        for garbage in [&b""[..], b"\n", b"{}\n", b"no newline"] {
            assert!(parse_rollup(garbage, DEV).is_err());
        }
    }

    #[test]
    fn space_file_detects_wrong_key_or_space() {
        let keys = keys();
        let space = SpaceFile::new(&keys, "space-a");
        let decoded = SpaceFile::decode(&space.encode().unwrap()).unwrap();
        decoded.verify(&keys, "space-a").unwrap();
        assert!(decoded.verify(&keys, "space-b").is_err());
        assert!(decoded.verify(&super::tests::keys(), "space-a").is_err());
        assert!(SpaceFile::decode(b"not json").is_err());
    }
}
