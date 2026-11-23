//! 传输后端（§10）。
//!
//! 传输层只有四个基本操作：列、读、写、删。存储都是被动的，没有推送；同步也不需要
//! 任何一台设备常开着当中转。协议照着限制最紧的那类存储定：
//!
//! 1. 一台设备只写自己的 `heads/<自己>` 和 `devices/<自己>/`，以及内容寻址的 `blobs/`，
//!    所以远端没有写冲突，不需要锁、条件写或事务；
//! 2. 除了 `heads/`，不靠列目录发现内容：段和汇总包都从 head 出发按名字读取。

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::TransportError;

pub mod file;
pub mod net;
pub mod s3;
pub mod webdav;
mod xml;

pub type TransportResult<T> = std::result::Result<T, TransportError>;

/// 远端对象的元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    /// 对象名（不含目录）。
    pub name: String,
    pub size: u64,
    /// 版本标识：有 ETag 用 ETag，没有就用修改时间。只用来判断「变没变」，不假定它可靠：
    /// 读者只在读取并校验成功之后才记下它。
    pub version: String,
    /// 这个版本标识是不是存储端真正给出的（ETag 或修改时间）。两者都没有时它就是空串，
    /// [`ObjectMeta::tag`] 只剩长度——同长度的新内容会被当成旧值，读者不能拿它判重，
    /// 要按内容（读取并校验之后的 head）来认。
    pub stable_version: bool,
    /// 这个版本标识是不是**强**标识（内容变了标识一定跟着变）。只有强标识能当回收
    /// 依据的「这份对象核对过、之后没被换过」；修改时间、弱 ETag、不合规标签都只是
    /// 提示，判重时要么立刻读取（标识变了），要么按窗口定期实读（标识没变也读）。
    pub strong_version: bool,
}

impl ObjectMeta {
    /// 「变没变」的判据：版本标识加长度。
    pub fn tag(&self) -> String {
        format!("{}:{}", self.version, self.size)
    }
}

/// 对象键：相对空间根的路径，正斜杠分隔，只含 ASCII 字母、数字、`-`、`_`、`.`。
/// 不合规的键一律拒绝（防路径穿越；也保证在所有后端上表现一致）。
pub fn check_key(key: &str) -> TransportResult<()> {
    let ok = !key.is_empty()
        && key.len() <= 512
        && key.trim_end_matches('/').split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        });
    if ok {
        Ok(())
    } else {
        Err(TransportError::Config(format!("对象键不合法: {key:?}")))
    }
}

/// 列一次目录最多接受这么多项，再多就按访问失败报，不再读下去。
///
/// 存储端不可信：它可以把一个目录列成任意长，让本机的内存跟着涨。协议里会被列的目录只有
/// `heads/`（每台设备一个对象）、某台设备自己的快照目录（一个快照最多一万个分片，新旧两个
/// 可以并存）和自检目录，都远到不了这个数。
pub(crate) const LIST_MAX: usize = 50_000;

/// 目录里的东西多过了 [`LIST_MAX`]。`what` 是被列的目录。
pub(crate) fn too_many(what: &str) -> TransportError {
    TransportError::Other(format!(
        "{what} 里的对象多得不正常（超过 {LIST_MAX} 个），没有再读下去"
    ))
}

/// 对象名是否合规。同步工具留下的冲突副本、临时文件、其他不认识的名字都不算对象，
/// 列目录时一律忽略。
pub(crate) fn is_object_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains(".sync-conflict-")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[async_trait]
pub trait SyncTransport: Send + Sync {
    /// `prefix` 以 `/` 结尾时列出该目录下的直接子对象（不含子目录；目录不存在返回空）；
    /// 否则它是一个对象的完整键，返回这个对象自己的元数据（不存在返回空）。
    /// 写后核对长度用后一种取法，不增加基本操作。
    async fn list(&self, prefix: &str) -> TransportResult<Vec<ObjectMeta>>;

    /// 把一个对象读进内存。`max` 是调用方按这类对象可能的最大长度给的上限（字节）。
    ///
    /// 存储端不可信：它（或者任何能往那个目录里放文件的人）可以放一个几个 GB 的对象在
    /// 任何名字下面。所以上限在读之前（看得到长度的话）和读的过程中就拦，对象不会先被
    /// 整个放进内存再检查。超过上限报 [`TransportError::Oversize`]。
    async fn get(&self, key: &str, max: u64) -> TransportResult<Vec<u8>>;

    async fn put(&self, key: &str, body: Vec<u8>) -> TransportResult<()>;

    /// 仅创建：对象已经存在就失败（不覆盖）。用于「新建空间标记」这类一旦存在就不能
    /// 被迟到写入改回开放的场合；后端不支持原子创建时退化为 [`Self::put`]。
    async fn put_new(&self, key: &str, body: Vec<u8>) -> TransportResult<()> {
        self.put(key, body).await
    }

    /// 删除对象；对象本来就不存在视为成功。
    async fn delete(&self, key: &str) -> TransportResult<()>;

    /// 附件的流式上传：从本机文件读，不把整个文件放进内存。
    async fn put_file(&self, key: &str, source: &Path) -> TransportResult<()> {
        let body = tokio::fs::read(source)
            .await
            .map_err(|e| TransportError::Other(format!("读取 {} 失败: {e}", source.display())))?;
        self.put(key, body).await
    }

    /// 附件的流式下载：写到本机文件（调用方给的是临时路径）。`max` 同 [`Self::get`]：
    /// 写出的字节数超过它就中断，不会把磁盘写满。中断时 `dest` 里可能留着写了一半的
    /// 内容，由调用方删掉。
    async fn get_file(&self, key: &str, dest: &Path, max: u64) -> TransportResult<()> {
        let body = self.get(key, max).await?;
        tokio::fs::write(dest, body)
            .await
            .map_err(|e| TransportError::Other(format!("写入 {} 失败: {e}", dest.display())))
    }

    /// 「对象还没到齐」可以容忍多久。目录交给第三方工具同步时，文件到达的先后没有保证，
    /// head 可能先于它引用的段出现；读者在这段时间内只等待，不报错。
    fn arrival_grace(&self) -> Duration {
        Duration::from_secs(120)
    }

    /// 后端名（状态显示用）。
    fn kind(&self) -> &'static str;
}

/// 单个对象的元数据（不存在为 `None`）。
pub async fn stat(transport: &dyn SyncTransport, key: &str) -> TransportResult<Option<ObjectMeta>> {
    let name = key.rsplit('/').next().unwrap_or(key);
    Ok(transport
        .list(key)
        .await?
        .into_iter()
        .find(|meta| meta.name == name))
}

/// 对象比它这一类可能的最大长度还大。
pub(crate) fn oversize(key: &str, max: u64) -> TransportError {
    TransportError::Oversize(format!(
        "{key} 比这类对象可能的最大长度（{max} 字节）还大，没有读取"
    ))
}

/// 读对象；不存在返回 `None`。`max` 见 [`SyncTransport::get`]。
pub async fn get_opt(
    transport: &dyn SyncTransport,
    key: &str,
    max: u64,
) -> TransportResult<Option<Vec<u8>>> {
    match transport.get(key, max).await {
        Ok(body) => Ok(Some(body)),
        Err(TransportError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// 写对象并核对远端长度：连接中断时有的服务端会留下截断的对象。
pub async fn put_checked(
    transport: &dyn SyncTransport,
    key: &str,
    body: Vec<u8>,
) -> TransportResult<()> {
    let len = body.len() as u64;
    transport.put(key, body).await?;
    check_len(transport, key, len).await
}

/// 同 [`put_checked`]，但对象已存在时失败、不覆盖。
pub async fn put_new_checked(
    transport: &dyn SyncTransport,
    key: &str,
    body: Vec<u8>,
) -> TransportResult<()> {
    let len = body.len() as u64;
    transport.put_new(key, body).await?;
    check_len(transport, key, len).await
}

pub async fn check_len(transport: &dyn SyncTransport, key: &str, len: u64) -> TransportResult<()> {
    match stat(transport, key).await? {
        Some(meta) if meta.size == len => Ok(()),
        Some(meta) => Err(TransportError::Other(format!(
            "{key} 写入后长度不符（本机 {len} 字节，远端 {} 字节）",
            meta.size
        ))),
        None => Err(TransportError::Other(format!("{key} 写入后在远端找不到"))),
    }
}

/// 「测试连接」里的一步。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SelfTestStep {
    pub name: String,
    pub ok: bool,
    /// 没通过的原因；通过时为空。
    pub detail: String,
}

/// 测试对象所在的目录（存储根下，不属于任何同步空间）。
const SELF_TEST_DIR: &str = "selftest";
/// 读回小测试对象时的长度上限（它只有一两百字节）。
const SELF_TEST_SMALL_MAX: u64 = 64 * 1024;
/// 试传的大对象：反向代理默认的请求体上限常常只有 1 MB，汇总包和附件会被它拒绝。
const SELF_TEST_LARGE: usize = 2 * 1024 * 1024;

/// 「测试连接」（§10）：在存储根下的 `selftest/` 里把用到的操作逐个走一遍——建目录并
/// 写入、列目录、读回比对、覆盖写、删除，再试传一个 2 MB 的对象。哪一步不通就报哪一步，
/// 后面的不再做。用过的对象不论成败都会删掉（目录本身留着：传输层没有删目录的操作）。
pub async fn self_test(transport: &dyn SyncTransport) -> Vec<SelfTestStep> {
    let tag = crate::ids::random_hex::<4>();
    let small = format!("{SELF_TEST_DIR}/probe-{tag}.bin");
    let large = format!("{SELF_TEST_DIR}/large-{tag}.bin");
    let mut steps = Vec::new();
    self_test_steps(transport, &small, &large, &mut steps).await;
    let _ = transport.delete(&small).await;
    let _ = transport.delete(&large).await;
    steps
}

fn self_test_record(
    steps: &mut Vec<SelfTestStep>,
    name: &str,
    result: TransportResult<()>,
) -> bool {
    let detail = match &result {
        Ok(()) => String::new(),
        Err(TransportError::TooLarge(_)) => {
            "服务端的上传上限太小：汇总包和附件会被拒绝，请调大服务端（或它前面的反向代理）的请求体上限"
                .to_string()
        }
        Err(error) => error.to_string(),
    };
    steps.push(SelfTestStep {
        name: name.to_string(),
        ok: result.is_ok(),
        detail,
    });
    result.is_ok()
}

async fn self_test_steps(
    transport: &dyn SyncTransport,
    small: &str,
    large: &str,
    steps: &mut Vec<SelfTestStep>,
) {
    let mismatch = |what: &str| Err(TransportError::Other(what.to_string()));
    let name = small.rsplit('/').next().unwrap_or(small).to_string();
    let first = format!("rclaw-sync 连接测试 {name}").into_bytes();
    let second = format!("rclaw-sync 连接测试（覆盖写）{name} {}", "·".repeat(64)).into_bytes();

    if !self_test_record(
        steps,
        "建目录并写入",
        transport.put(small, first.clone()).await,
    ) {
        return;
    }
    let listed = match transport.list(&format!("{SELF_TEST_DIR}/")).await {
        Ok(objects) => match objects.iter().find(|meta| meta.name == name) {
            Some(meta) if meta.size == first.len() as u64 => Ok(()),
            Some(_) => mismatch("列目录给出的对象长度与写入的不符"),
            None => mismatch("列目录的结果里没有刚写入的对象"),
        },
        Err(error) => Err(error),
    };
    if !self_test_record(steps, "列目录", listed) {
        return;
    }
    let read_back = match transport.get(small, SELF_TEST_SMALL_MAX).await {
        Ok(body) if body == first => Ok(()),
        Ok(_) => mismatch("读回的内容与写入的不同"),
        Err(error) => Err(error),
    };
    if !self_test_record(steps, "读回比对", read_back) {
        return;
    }
    let overwritten = match put_checked(transport, small, second.clone()).await {
        Ok(()) => match transport.get(small, SELF_TEST_SMALL_MAX).await {
            Ok(body) if body == second => Ok(()),
            Ok(_) => mismatch("覆盖写之后读到的还是旧内容"),
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };
    if !self_test_record(steps, "覆盖写", overwritten) {
        return;
    }
    let deleted = match transport.delete(small).await {
        Ok(()) => match get_opt(transport, small, SELF_TEST_SMALL_MAX).await {
            Ok(None) => Ok(()),
            Ok(Some(_)) => mismatch("删除之后对象还读得到"),
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };
    if !self_test_record(steps, "删除", deleted) {
        return;
    }
    // 不可压缩的内容：有的服务端或代理会透明压缩，全零的对象测不出上限。
    let mut state = u64::from_le_bytes(crate::ids::random_bytes::<8>()) | 1;
    let mut body = Vec::with_capacity(SELF_TEST_LARGE);
    while body.len() < SELF_TEST_LARGE {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        body.extend_from_slice(&state.to_le_bytes());
    }
    body.truncate(SELF_TEST_LARGE);
    let uploaded = put_checked(transport, large, body).await;
    self_test_record(steps, "上传 2 MB 的对象", uploaded);
}

/// 套一层子目录的传输：每个同步空间住在存储根下以 `space_id` 命名的子目录里，
/// 重置同步空间就是换一个子目录，新旧空间的对象不会混在一起。
pub struct Scoped<T> {
    inner: T,
    prefix: String,
}

impl<T: SyncTransport> Scoped<T> {
    pub fn new(inner: T, dir: &str) -> Self {
        Scoped {
            inner,
            prefix: format!("{}/", dir.trim_matches('/')),
        }
    }

    fn key(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }
}

#[async_trait]
impl<T: SyncTransport> SyncTransport for Scoped<T> {
    async fn list(&self, prefix: &str) -> TransportResult<Vec<ObjectMeta>> {
        self.inner.list(&self.key(prefix)).await
    }

    async fn get(&self, key: &str, max: u64) -> TransportResult<Vec<u8>> {
        self.inner.get(&self.key(key), max).await
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> TransportResult<()> {
        self.inner.put(&self.key(key), body).await
    }

    async fn delete(&self, key: &str) -> TransportResult<()> {
        self.inner.delete(&self.key(key)).await
    }

    async fn put_file(&self, key: &str, source: &Path) -> TransportResult<()> {
        self.inner.put_file(&self.key(key), source).await
    }

    async fn get_file(&self, key: &str, dest: &Path, max: u64) -> TransportResult<()> {
        self.inner.get_file(&self.key(key), dest, max).await
    }

    fn arrival_grace(&self) -> Duration {
        self.inner.arrival_grace()
    }

    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
}

#[async_trait]
impl SyncTransport for Box<dyn SyncTransport> {
    async fn list(&self, prefix: &str) -> TransportResult<Vec<ObjectMeta>> {
        (**self).list(prefix).await
    }

    async fn get(&self, key: &str, max: u64) -> TransportResult<Vec<u8>> {
        (**self).get(key, max).await
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> TransportResult<()> {
        (**self).put(key, body).await
    }

    async fn delete(&self, key: &str) -> TransportResult<()> {
        (**self).delete(key).await
    }

    async fn put_file(&self, key: &str, source: &Path) -> TransportResult<()> {
        (**self).put_file(key, source).await
    }

    async fn get_file(&self, key: &str, dest: &Path, max: u64) -> TransportResult<()> {
        (**self).get_file(key, dest, max).await
    }

    fn arrival_grace(&self) -> Duration {
        (**self).arrival_grace()
    }

    fn kind(&self) -> &'static str {
        (**self).kind()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_key_rejects_traversal_and_odd_characters() {
        for good in [
            "heads/",
            "heads/0123456789abcdef",
            "devices/a/ops/x.seg",
            "space.json",
        ] {
            check_key(good).unwrap();
        }
        for bad in [
            "", "/abs", "a//b", "../x", "a/../b", "a/./b", "a b", "a\\b", "中文", "a?b",
        ] {
            assert!(check_key(bad).is_err(), "{bad}");
        }
    }
}
