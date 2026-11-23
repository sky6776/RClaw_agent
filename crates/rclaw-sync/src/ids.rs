//! 标识（§6）：`device_id`、`sid`、`gid`、实例号、段与汇总包的对象名。
//!
//! 全部是随机串，不带计数、不从本机行 id 推出：库被旧备份覆盖或数据根被复制之后，
//! 新分配的标识不会与已经发出去的重复。远端对象名只含 ASCII 字母、数字、`-`、`_`、`.`。

use ring::rand::{SecureRandom, SystemRandom};

const HEX: &[u8; 16] = b"0123456789abcdef";

pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    text.as_bytes()
        .chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

/// 系统随机源的随机字节。随机源不可用时 panic：没有随机数就没有任何安全保证，
/// 继续运行比停下更糟。
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    SystemRandom::new()
        .fill(&mut out)
        .expect("系统随机源不可用");
    out
}

pub fn random_hex<const N: usize>() -> String {
    hex(&random_bytes::<N>())
}

/// 设备身份：16 位十六进制。
pub fn new_device_id() -> String {
    random_hex::<8>()
}

/// 线程身份：24 位十六进制。
pub fn new_sid() -> String {
    random_hex::<12>()
}

/// 消息的全局身份：`<device_id>.<16 位十六进制>`。前缀标明这条消息产生于哪台设备。
pub fn new_gid(device_id: &str) -> String {
    format!("{device_id}.{}", random_hex::<8>())
}

/// 引擎每次启动随机生成的实例号。
pub fn new_instance_id() -> String {
    random_hex::<8>()
}

pub(crate) fn is_lower_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn is_device_id(text: &str) -> bool {
    is_lower_hex(text, 16)
}

pub fn is_sid(text: &str) -> bool {
    is_lower_hex(text, 24)
}

pub fn is_gid(text: &str) -> bool {
    text.split_once('.')
        .is_some_and(|(device, tail)| is_device_id(device) && is_lower_hex(tail, 16))
}

/// 十六进制摘要（SHA-256 = 64 位）。
pub fn is_digest(text: &str) -> bool {
    is_lower_hex(text, 64)
}

/// `gid` 的设备前缀。
pub fn gid_device(gid: &str) -> Option<&str> {
    gid.split_once('.').map(|(device, _)| device)
}

/// `gid` 的随机尾巴（`.` 后面的 16 位十六进制）。形状不对返回 `None`。
pub fn gid_tail(gid: &str) -> Option<&str> {
    rclaw_store::sync_db::gid_tail(gid)
}

/// 墓碑所带的链（§7.5）里一条消息占的长度：`gid` 的随机尾巴。
const CHAIN_ITEM: usize = 16;
/// 收到的链最多接受这么多条。写的一方只带 `rclaw_store::sync_db::TOMB_CHAIN_MAX` 条，
/// 多出来的是给以后的版本留的余量。
pub const CHAIN_ACCEPT_MAX: usize = 16384;

/// 墓碑所带的链形状对不对：若干个 16 位小写十六进制连成一个串（空串也算，= 没带链）。
pub fn is_chain(text: &str) -> bool {
    text.len() % CHAIN_ITEM == 0
        && text.len() <= CHAIN_ACCEPT_MAX * CHAIN_ITEM
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `gid` 这条消息在不在链上。链只记随机尾巴：`gid` 在线程内唯一，尾巴是 64 位随机数，
/// 同一个线程里不会撞上。
pub fn chain_contains(chain: &str, gid: &str) -> bool {
    let Some(tail) = gid_tail(gid) else {
        return false;
    };
    chain
        .as_bytes()
        .chunks_exact(CHAIN_ITEM)
        .any(|item| item == tail.as_bytes())
}

/// 段对象名：`<起:012>-<止:012>-<8 位随机>.seg`。随机后缀让两个写者（数据根被复制）
/// 不会写出同名对象、互相覆盖。
pub fn segment_name(from: u64, to: u64) -> String {
    format!("{from:012}-{to:012}-{}.seg", random_hex::<4>())
}

/// 汇总包对象名：`<起:012>-<止:012>-<8 位随机>.pack`。
pub fn rollup_name(from: u64, to: u64) -> String {
    format!("{from:012}-{to:012}-{}.pack", random_hex::<4>())
}

fn parse_range_name(name: &str, extension: &str) -> Option<(u64, u64)> {
    let stem = name.strip_suffix(extension)?;
    let mut parts = stem.split('-');
    let (from, to, random) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some()
        || from.len() != 12
        || to.len() != 12
        || !is_lower_hex(random, 8)
        || !from.bytes().all(|b| b.is_ascii_digit())
        || !to.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let (from, to) = (from.parse().ok()?, to.parse().ok()?);
    (from >= 1 && from <= to).then_some((from, to))
}

/// 解析段对象名里的序号区间。名字来自远端，不合规一律拒绝（防路径穿越）。
pub fn parse_segment_name(name: &str) -> Option<(u64, u64)> {
    parse_range_name(name, ".seg")
}

pub fn parse_rollup_name(name: &str) -> Option<(u64, u64)> {
    parse_range_name(name, ".pack")
}

/// 全量快照的名字：`<写出时刻毫秒:013>-<8 位随机>`。它的各个对象名都由这个名字加后缀
/// 得到。
pub fn snapshot_name(ts_ms: i64) -> String {
    format!("{:013}-{}", ts_ms.max(0), random_hex::<4>())
}

/// 解析快照名里的写出时刻。名字来自远端，不合规一律拒绝。
pub fn parse_snapshot_name(name: &str) -> Option<i64> {
    let (ts, random) = name.split_once('-')?;
    if ts.len() != 13 || !ts.bytes().all(|b| b.is_ascii_digit()) || !is_lower_hex(random, 8) {
        return None;
    }
    ts.parse().ok()
}

/// chat_id 白名单：`[A-Za-z0-9_-]{1,64}`（与桌面端 `is_valid_chat_id` 同一规则）。
pub fn is_valid_chat_id(chat_id: &str) -> bool {
    (1..=64).contains(&chat_id.len())
        && chat_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// 通道名：小写字母、数字、`_`、`-`，1–32 位。
pub fn is_valid_channel(channel: &str) -> bool {
    (1..=32).contains(&channel.len())
        && channel
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_expected_shapes() {
        let device = new_device_id();
        assert!(is_device_id(&device));
        assert!(is_sid(&new_sid()));
        let gid = new_gid(&device);
        assert!(is_gid(&gid));
        assert_eq!(gid_device(&gid), Some(device.as_str()));
        assert!(!is_gid("abc.def"));
        assert!(!is_device_id("ABCDEF0123456789"));
    }

    #[test]
    fn chains_are_checked_and_searched_by_gid_tail() {
        let a = "0123456789abcdef.00000000000000a1";
        let b = "fedcba9876543210.00000000000000b2";
        let chain = format!("{}{}", gid_tail(a).unwrap(), gid_tail(b).unwrap());
        assert!(is_chain(&chain));
        assert!(is_chain(""));
        assert!(chain_contains(&chain, a));
        assert!(chain_contains(&chain, b));
        // 只认整条的尾巴：跨着两条的 16 个字符不算。
        assert!(!chain_contains(&chain, "0123456789abcdef.000000a100000000"));
        assert!(!chain_contains(&chain, "0123456789abcdef.00000000000000c3"));
        assert!(!chain_contains(&chain, "not-a-gid"));
        assert!(!chain_contains("", a));
        // 长度不是 16 的倍数、有大写或别的字符、太长：都不合规。
        assert!(!is_chain("00000000000000a"));
        assert!(!is_chain("00000000000000A1"));
        assert!(!is_chain("0000000000000.a1"));
        assert!(is_chain(&"0".repeat(CHAIN_ACCEPT_MAX * 16)));
        assert!(!is_chain(&"0".repeat((CHAIN_ACCEPT_MAX + 1) * 16)));
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 0xab, 0xff];
        assert_eq!(hex(&bytes), "0001abff");
        assert_eq!(unhex("0001abff").unwrap(), bytes);
        assert!(unhex("0").is_none());
        assert!(unhex("zz").is_none());
    }

    #[test]
    fn segment_names_parse_and_reject_garbage() {
        let name = segment_name(7, 12);
        assert_eq!(parse_segment_name(&name), Some((7, 12)));
        assert_eq!(parse_rollup_name(&rollup_name(1, 100)), Some((1, 100)));
        for bad in [
            "../../x.seg",
            "000000000002-000000000001-abcdef01.seg",
            "000000000000-000000000001-abcdef01.seg",
            "1-2-abcdef01.seg",
            "000000000001-000000000002-abcdef01.pack",
            "000000000001-000000000002-ABCDEF01.seg",
            "000000000001-000000000002-abcdef01-x.seg",
        ] {
            assert_eq!(parse_segment_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn snapshot_names_parse_and_reject_garbage() {
        let name = snapshot_name(1_763_438_400_000);
        assert_eq!(parse_snapshot_name(&name), Some(1_763_438_400_000));
        for bad in [
            "",
            "x",
            "1763438400000",
            "1763438400000-ABCDEF01",
            "../1763438400000-abcdef01",
        ] {
            assert_eq!(parse_snapshot_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn chat_id_whitelist() {
        assert!(is_valid_chat_id("c-1763438400000abc"));
        assert!(is_valid_chat_id("main"));
        assert!(!is_valid_chat_id(""));
        assert!(!is_valid_chat_id("a/b"));
        assert!(!is_valid_chat_id(&"x".repeat(65)));
    }
}
