//! 端到端加密（§11.1）。
//!
//! - 每个同步空间一把 256 位随机密钥，只存在各设备本机，永不上传；
//! - 日志、附件、对象名、密钥校验各用 HKDF-SHA256 派生的子密钥；
//! - 所有远端对象做认证加密（ChaCha20-Poly1305），关联数据绑定对象路径：存储方改名、
//!   调包或截断对象都会校验失败；
//! - 附件的对象名是带密钥的摘要，存储方无法拿一个已知文件来确认「你有没有这个文件」。

use std::io::{Read, Write};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305, NONCE_LEN};
use ring::{digest, hkdf, hmac};

use crate::error::{Result, SyncError};
use crate::ids::{hex, random_bytes};

/// 小对象（段、汇总包、head）的魔数。
const MAGIC_LOG: &[u8; 4] = b"RCS1";
/// 附件对象的魔数（分块加密）。
const MAGIC_BLOB: &[u8; 4] = b"RCB1";
const TAG_LEN: usize = 16;
/// 附件分块大小（明文）。
pub const BLOB_CHUNK: usize = 1024 * 1024;

/// 空间密钥。`Debug` 不打印内容；drop 时清零。
#[derive(Clone)]
pub struct SpaceKey([u8; 32]);

impl SpaceKey {
    pub fn generate() -> Self {
        SpaceKey(random_bytes::<32>())
    }

    pub fn from_base64(text: &str) -> Result<Self> {
        let bytes = URL_SAFE_NO_PAD
            .decode(text.trim())
            .map_err(|_| SyncError::invalid("空间密钥不是合法的 base64"))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| SyncError::invalid("空间密钥长度不对"))?;
        Ok(SpaceKey(bytes))
    }

    pub fn to_base64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }
}

impl std::fmt::Debug for SpaceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpaceKey(***)")
    }
}

impl Drop for SpaceKey {
    fn drop(&mut self) {
        for byte in self.0.iter_mut() {
            // volatile 写：不让优化器把「反正要释放」的清零删掉。
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

struct OkmLen(usize);

impl hkdf::KeyType for OkmLen {
    fn len(&self) -> usize {
        self.0
    }
}

fn derive(prk: &hkdf::Prk, label: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    let info = [label.as_bytes()];
    prk.expand(&info, OkmLen(32))
        .and_then(|okm| okm.fill(&mut out))
        .expect("HKDF 输出长度合法");
    out
}

fn aead_key(bytes: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, bytes).expect("密钥长度合法"))
}

/// 由空间密钥派生出的一组子密钥。
pub struct Keys {
    log: LessSafeKey,
    blob: LessSafeKey,
    name: hmac::Key,
    check: hmac::Key,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keys(***)")
    }
}

impl Keys {
    pub fn derive(space_key: &SpaceKey) -> Keys {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, b"rclaw-sync/v1").extract(&space_key.0);
        Keys {
            log: aead_key(&derive(&prk, "rclaw-sync/v1/log")),
            blob: aead_key(&derive(&prk, "rclaw-sync/v1/blob")),
            name: hmac::Key::new(hmac::HMAC_SHA256, &derive(&prk, "rclaw-sync/v1/name")),
            check: hmac::Key::new(hmac::HMAC_SHA256, &derive(&prk, "rclaw-sync/v1/check")),
        }
    }

    /// 加密一个小对象（段、汇总包、head）。`path` 是它在远端的对象路径，作为关联数据。
    pub fn seal(&self, path: &str, plaintext: &[u8]) -> Vec<u8> {
        let nonce = random_bytes::<NONCE_LEN>();
        let mut out = Vec::with_capacity(4 + NONCE_LEN + plaintext.len() + TAG_LEN);
        out.extend_from_slice(MAGIC_LOG);
        out.extend_from_slice(&nonce);
        let mut body = plaintext.to_vec();
        self.log
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(path.as_bytes()),
                &mut body,
            )
            .expect("加密不会失败");
        out.extend_from_slice(&body);
        out
    }

    /// 解密并校验一个小对象。路径不符、内容被改、被截断都报 [`SyncError::Corrupt`]。
    pub fn open(&self, path: &str, object: &[u8]) -> Result<Vec<u8>> {
        let corrupt = || SyncError::corrupt(format!("{path} 解密失败（被改动、截断或还没写完）"));
        if object.len() < 4 + NONCE_LEN + TAG_LEN || &object[..4] != MAGIC_LOG {
            return Err(corrupt());
        }
        let nonce: [u8; NONCE_LEN] = object[4..4 + NONCE_LEN].try_into().expect("长度已检查");
        let mut body = object[4 + NONCE_LEN..].to_vec();
        let plain_len = self
            .log
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(path.as_bytes()),
                &mut body,
            )
            .map_err(|_| corrupt())?
            .len();
        body.truncate(plain_len);
        Ok(body)
    }

    /// 附件的对象名：内容摘要的带密钥摘要（十六进制）。
    pub fn blob_id(&self, content_sha256: &[u8]) -> String {
        hex(hmac::sign(&self.name, content_sha256).as_ref())
    }

    /// `space.json` 里的密钥校验值：持有同一把密钥的设备才算得出同一个值。
    pub fn check_value(&self, space_id: &str) -> String {
        hex(hmac::sign(&self.check, space_id.as_bytes()).as_ref())
    }

    /// 流式加密一个附件。对象 = 魔数 | 基准 nonce | 若干块；每块的 nonce 由基准与块序号
    /// 得出，关联数据带上路径、块序号与「是否最后一块」，所以块不能被调换、删除或截断。
    /// 返回密文长度。
    pub fn seal_blob(
        &self,
        path: &str,
        reader: &mut dyn Read,
        writer: &mut dyn Write,
    ) -> Result<u64> {
        let base = random_bytes::<NONCE_LEN>();
        writer.write_all(MAGIC_BLOB)?;
        writer.write_all(&base)?;
        let mut written = (4 + NONCE_LEN) as u64;
        let mut index: u32 = 0;
        let mut current = read_chunk(reader)?;
        loop {
            let next = if current.len() == BLOB_CHUNK {
                read_chunk(reader)?
            } else {
                Vec::new()
            };
            let last = next.is_empty();
            let mut body = current;
            self.blob
                .seal_in_place_append_tag(
                    chunk_nonce(&base, index),
                    Aad::from(chunk_aad(path, index, last)),
                    &mut body,
                )
                .expect("加密不会失败");
            writer.write_all(&body)?;
            written += body.len() as u64;
            if last {
                break;
            }
            index = index
                .checked_add(1)
                .ok_or_else(|| SyncError::other("附件过大"))?;
            current = next;
        }
        writer.flush()?;
        Ok(written)
    }

    /// 流式解密一个附件。任何一块校验不过、或缺少最后一块，都报 [`SyncError::Corrupt`]，
    /// 调用方应丢弃已写出的内容。返回明文长度。
    pub fn open_blob(
        &self,
        path: &str,
        reader: &mut dyn Read,
        writer: &mut dyn Write,
    ) -> Result<u64> {
        let corrupt = || SyncError::corrupt(format!("{path} 解密失败（被改动、截断或还没传完）"));
        let mut head = [0u8; 4 + NONCE_LEN];
        reader.read_exact(&mut head).map_err(|_| corrupt())?;
        if &head[..4] != MAGIC_BLOB {
            return Err(corrupt());
        }
        let base: [u8; NONCE_LEN] = head[4..].try_into().expect("长度固定");
        let mut written = 0u64;
        let mut index: u32 = 0;
        let mut current = read_exact_or_less(reader, BLOB_CHUNK + TAG_LEN)?;
        loop {
            if current.len() < TAG_LEN {
                return Err(corrupt());
            }
            let next = if current.len() == BLOB_CHUNK + TAG_LEN {
                read_exact_or_less(reader, BLOB_CHUNK + TAG_LEN)?
            } else {
                Vec::new()
            };
            let last = next.is_empty();
            let mut body = current;
            let plain_len = self
                .blob
                .open_in_place(
                    chunk_nonce(&base, index),
                    Aad::from(chunk_aad(path, index, last)),
                    &mut body,
                )
                .map_err(|_| corrupt())?
                .len();
            writer.write_all(&body[..plain_len])?;
            written += plain_len as u64;
            if last {
                break;
            }
            index = index.checked_add(1).ok_or_else(corrupt)?;
            current = next;
        }
        writer.flush()?;
        Ok(written)
    }
}

/// 明文 `plain` 字节的附件，加密之后的对象正好有多长（[`Keys::seal_blob`] 的输出长度）：
/// 魔数与基准 nonce，加上每块一个认证标签；空文件也有一块。
///
/// 对象名是内容的带密钥摘要，同名即同内容，所以一个附件对象的长度是定死的：远端的对象
/// 短了是没传完，长了就不是它。
pub fn sealed_blob_len(plain: u64) -> u64 {
    let chunks = plain.div_ceil(BLOB_CHUNK as u64).max(1);
    // `plain` 可能来自别的设备的登记：算术一律饱和，不溢出。
    ((4 + NONCE_LEN) as u64)
        .saturating_add(plain)
        .saturating_add(chunks.saturating_mul(TAG_LEN as u64))
}

fn read_chunk(reader: &mut dyn Read) -> Result<Vec<u8>> {
    read_exact_or_less(reader, BLOB_CHUNK)
}

/// 读满 `len` 字节，或读到流结束为止。
fn read_exact_or_less(reader: &mut dyn Read, len: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        match reader.read(&mut out[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    out.truncate(filled);
    Ok(out)
}

fn chunk_nonce(base: &[u8; NONCE_LEN], index: u32) -> Nonce {
    let mut nonce = *base;
    for (slot, byte) in nonce[NONCE_LEN - 4..].iter_mut().zip(index.to_be_bytes()) {
        *slot ^= byte;
    }
    Nonce::assume_unique_for_key(nonce)
}

fn chunk_aad(path: &str, index: u32, last: bool) -> Vec<u8> {
    let mut aad = Vec::with_capacity(path.len() + 6);
    aad.extend_from_slice(path.as_bytes());
    aad.push(0);
    aad.extend_from_slice(&index.to_be_bytes());
    aad.push(last as u8);
    aad
}

/// SHA-256，十六进制。
pub fn sha256_hex(data: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, data).as_ref())
}

/// 流式 SHA-256：返回 `(摘要字节, 长度)`。
pub fn sha256_reader(reader: &mut dyn Read) -> Result<([u8; 32], u64)> {
    let mut ctx = digest::Context::new(&digest::SHA256);
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                ctx.update(&buf[..n]);
                total += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let digest: [u8; 32] = ctx
        .finish()
        .as_ref()
        .try_into()
        .expect("SHA-256 是 32 字节");
    Ok((digest, total))
}

/// 边读边算 SHA-256 的包装读端：封装密文与核对摘要用同一次读取，密文对应的就是核对过
/// 的那一段明文（读完 [`Keys::seal_blob`] 之后调 [`HashingReader::finish`]）。
pub struct HashingReader<R: Read> {
    inner: R,
    ctx: digest::Context,
    total: u64,
}

impl<R: Read> HashingReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            ctx: digest::Context::new(&digest::SHA256),
            total: 0,
        }
    }

    /// 读到头之后的 `(摘要字节, 长度)`。
    pub fn finish(self) -> ([u8; 32], u64) {
        (
            self.ctx.finish().as_ref().try_into().expect("SHA-256 是 32 字节"),
            self.total,
        )
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.ctx.update(&buf[..n]);
        self.total += n as u64;
        Ok(n)
    }
}

/// 段明文的压缩（deflate）。
pub fn deflate(data: &[u8]) -> Vec<u8> {
    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).expect("写入内存不会失败");
    encoder.finish().expect("写入内存不会失败")
}

/// 解压，输出超过 `limit` 字节即报错（防压缩炸弹）。
pub fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut decoder = flate2::read::DeflateDecoder::new(data).take(limit as u64 + 1);
    decoder
        .read_to_end(&mut out)
        .map_err(|e| SyncError::corrupt(format!("解压失败: {e}")))?;
    if out.len() > limit {
        return Err(SyncError::corrupt("解压后的内容超过上限"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_keys() -> Keys {
        Keys::derive(&SpaceKey::generate())
    }

    #[test]
    fn seal_open_round_trip_and_binds_path() {
        let keys = new_keys();
        let object = keys.seal("heads/abc", b"hello");
        assert_eq!(keys.open("heads/abc", &object).unwrap(), b"hello");
        // 改名（换路径）校验失败。
        assert!(matches!(
            keys.open("heads/abd", &object),
            Err(SyncError::Corrupt(_))
        ));
        // 翻转一位校验失败。
        let mut tampered = object.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(keys.open("heads/abc", &tampered).is_err());
        // 截断校验失败。
        assert!(keys.open("heads/abc", &object[..object.len() - 1]).is_err());
        assert!(keys.open("heads/abc", &[]).is_err());
        // 另一把密钥解不开。
        assert!(new_keys().open("heads/abc", &object).is_err());
    }

    #[test]
    fn ciphertext_does_not_contain_plaintext_marker() {
        let keys = new_keys();
        let marker = b"PLAINTEXT-MARKER-0123456789";
        let object = keys.seal("x", marker);
        assert!(!object.windows(marker.len()).any(|window| window == marker));
    }

    #[test]
    fn space_key_base64_round_trip() {
        let key = SpaceKey::generate();
        let text = key.to_base64();
        assert_eq!(SpaceKey::from_base64(&text).unwrap().0, key.0);
        assert!(SpaceKey::from_base64("abc").is_err());
        assert_eq!(format!("{key:?}"), "SpaceKey(***)");
    }

    #[test]
    fn derived_values_are_stable_per_key() {
        let key = SpaceKey::generate();
        let (a, b) = (Keys::derive(&key), Keys::derive(&key));
        assert_eq!(a.check_value("space"), b.check_value("space"));
        assert_ne!(a.check_value("space"), a.check_value("other"));
        assert_eq!(a.blob_id(&[1; 32]), b.blob_id(&[1; 32]));
        assert_ne!(a.blob_id(&[1; 32]), new_keys().blob_id(&[1; 32]));
    }

    fn blob_round_trip(len: usize) {
        let keys = new_keys();
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let mut sealed = Vec::new();
        let written = keys
            .seal_blob("blobs/ab/x", &mut data.as_slice(), &mut sealed)
            .unwrap();
        assert_eq!(written as usize, sealed.len());
        assert_eq!(sealed_blob_len(len as u64), written, "明文 {len} 字节");
        let mut opened = Vec::new();
        let plain = keys
            .open_blob("blobs/ab/x", &mut sealed.as_slice(), &mut opened)
            .unwrap();
        assert_eq!(plain as usize, len);
        assert_eq!(opened, data);
        // 截断：任何前缀都解不开。
        if !sealed.is_empty() {
            let cut = sealed.len() - 1;
            assert!(keys
                .open_blob("blobs/ab/x", &mut &sealed[..cut], &mut Vec::new())
                .is_err());
        }
        // 换路径解不开。
        assert!(keys
            .open_blob("blobs/ab/y", &mut sealed.as_slice(), &mut Vec::new())
            .is_err());
    }

    #[test]
    fn blob_round_trips_across_chunk_boundaries() {
        for len in [
            0,
            1,
            BLOB_CHUNK - 1,
            BLOB_CHUNK,
            BLOB_CHUNK + 1,
            2 * BLOB_CHUNK,
        ] {
            blob_round_trip(len);
        }
    }

    #[test]
    fn blob_rejects_dropped_trailing_chunk() {
        let keys = new_keys();
        let data = vec![7u8; BLOB_CHUNK + 10];
        let mut sealed = Vec::new();
        keys.seal_blob("p", &mut data.as_slice(), &mut sealed)
            .unwrap();
        // 去掉整个最后一块：第一块不是「最后一块」，校验失败。
        let cut = 4 + NONCE_LEN + BLOB_CHUNK + TAG_LEN;
        assert!(keys
            .open_blob("p", &mut &sealed[..cut], &mut Vec::new())
            .is_err());
    }

    #[test]
    fn inflate_enforces_limit() {
        let data = vec![0u8; 100_000];
        let packed = deflate(&data);
        assert_eq!(inflate(&packed, 100_000).unwrap(), data);
        assert!(inflate(&packed, 99_999).is_err());
        assert!(inflate(b"not deflate \xff\xff", 1000).is_err());
    }
}
