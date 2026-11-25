//! 同步引擎读写状态库的小工具。
//!
//! 约定（见 `rclaw_store::Db`）：库是一条连接加一把不可重入的互斥锁，闭包里只能调
//! 连接级函数，**不能**再调 `SessionStore` / `JournalStore` 的方法；事务要短。

use std::collections::BTreeMap;

use rclaw_store::sync_db::{self, KV_SYNC_DEVICE};
use rclaw_store::{Db, StoreError};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::codec::{HeadSeg, RollupRef, SegRef, SnapRef};
use crate::error::{Result, SyncError};
use crate::ids;

fn run<T>(
    f: impl FnOnce(&Transaction<'_>) -> Result<T>,
    transact: impl FnOnce(
        &mut dyn FnMut(&Transaction<'_>) -> rclaw_store::Result<()>,
    ) -> rclaw_store::Result<()>,
) -> Result<T> {
    // 同步层的错误跨不过 store 的事务闭包（它只认 `StoreError`）：先存下来，让事务回滚，
    // 出来以后再还原。
    let mut f = Some(f);
    let mut output: Option<T> = None;
    let mut failure: Option<SyncError> = None;
    let outcome = transact(&mut |tx| {
        let f = f.take().expect("事务闭包只会被调用一次");
        match f(tx) {
            Ok(value) => {
                output = Some(value);
                Ok(())
            }
            Err(e) => {
                failure = Some(e);
                Err(StoreError::Other("同步事务回滚".into()))
            }
        }
    });
    match (outcome, failure) {
        (_, Some(e)) => Err(e),
        (Err(e), None) => Err(e.into()),
        (Ok(()), None) => output.ok_or_else(|| SyncError::other("同步事务没有产出结果")),
    }
}

/// 一个 `BEGIN IMMEDIATE` 写事务；闭包返回 `Err` 时回滚。
pub fn write<T>(db: &Db, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
    run(f, |body| db.transaction(|tx| body(tx)))
}

/// 「落盘点」（§9.3 第 3 步）：把这之前已经提交的事务真正落到盘上。在库锁之外 fsync，
/// 这期间别的线程照常用库。阻塞（一次 fsync），async 上下文里要放进 `blocking`。
///
/// 往存储上写日志（段、head）之前必须先过这一关——[`crate::upload::upload_round`] 每次
/// 都做。别处在提交之后调它只是为了缩短「提交了、还没落盘」的窗口：失败了事务也已经
/// 提交，调用方不能把它当成「没发生」。
pub fn flush(db: &Db) -> Result<()> {
    Ok(db.flush()?)
}

/// 只读访问。
pub fn read<T>(db: &Db, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let mut f = Some(f);
    let mut output: Option<Result<T>> = None;
    db.with(|conn| {
        let f = f.take().expect("闭包只会被调用一次");
        output = Some(f(conn));
        Ok(())
    })?;
    output.unwrap_or_else(|| Err(SyncError::other("读取没有产出结果")))
}

// ---------------------------------------------------------------------------
// kv（小状态，键前缀 `sync/`）
// ---------------------------------------------------------------------------

pub const KV_LOG: &str = "sync/log";
pub const KV_PAUSED: &str = "sync/paused";
/// 上一次用到的同步空间。换了空间（重置、离开后加入另一个）时，本机的同步状态要清掉。
pub const KV_SPACE: &str = "sync/space";
/// 「已经把空间里现有的内容拉取并合入过一遍」。在那之前本机不发布存量（§8.8）。
pub const KV_PRIMED: &str = "sync/primed";
/// 「本机的读取位置被整段挪过（吃了全量快照），或者本机的库可能是旧的（换过身份）」：
/// 各设备的日志读到头之后，要把所有带链的墓碑对着本机的分支清点一遍（§7.5、§9.5），
/// 清点之前不发布本机存量——手里那些「别处已经删掉的分支的旧样子」不能再发出去。
pub const KV_STALE_CHECK: &str = "sync/stale-check";
/// 上一轮用的会话同步范围：范围扩大时要把各设备的日志从头再读一遍。
pub const KV_RANGE: &str = "sync/range";

/// 生效的同步范围（kv [`KV_RANGE`]，引擎每轮写；封段与附件上传在动手前读它——范围
/// 收窄之后，排着队的会话 op、文件类 op 与自动排队的附件不再发出去，范围重新变宽时
/// 照常恢复）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    #[serde(default)]
    pub sessions: bool,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub attachments: bool,
    #[serde(default)]
    pub memory: bool,
    #[serde(default)]
    pub persona: bool,
    #[serde(default)]
    pub skills: bool,
    #[serde(default)]
    pub bots: bool,
    /// 定时任务的定义同步（`cron.put` / `cron.del`）。老库里的记录没有这个字段，读出来
    /// 按关算——引擎下一轮写 [`KV_RANGE`] 时就会带上当前值。
    #[serde(default)]
    pub cron: bool,
}

impl Range {
    /// 本机当前生效的范围（引擎每轮把它写进 kv [`KV_RANGE`]）。
    pub(crate) fn of(s: &crate::publish::Settings) -> Range {
        let mut channels = if s.scopes.sessions {
            s.channels.clone()
        } else {
            Vec::new()
        };
        channels.sort();
        channels.dedup();
        Range {
            sessions: s.scopes.sessions,
            channels,
            attachments: s.scopes.attachments,
            memory: s.scopes.memory,
            persona: s.scopes.persona,
            skills: s.scopes.skills,
            bots: s.scopes.bots,
            cron: s.scopes.cron,
        }
    }
}
/// 存量附件的上传范围（§8.8）：`"all"`、天数、`"none"`。
pub const KV_BACKLOG: &str = "sync/backlog";
/// 快照与回收的记录。
pub const KV_MAINT: &str = "sync/maint";
/// 「本机要换身份」（§9.6）：发现的时候先记下，换完再清掉——中途退出也不会忘。
pub const KV_ROTATE: &str = "sync/rotate";
/// 本机创建的同步空间（创建者不必等「空间里现有的内容」，也负责写 `space.json`）。
pub const KV_CREATOR: &str = "sync/creator";
/// 计费网络下的首次同步已经得到用户确认的空间。
pub const KV_METERED_OK: &str = "sync/metered-ok";
/// 首次同步（拉取并合入空间里现有的内容、发布完本机存量）已经做完的空间。在那之前新
/// 登记的附件不直接排队上传，等用户选过存量附件的上传范围再说（§8.8）。
pub const KV_INITIAL: &str = "sync/initial";

pub fn local_only_key(channel: &str, chat_id: &str) -> String {
    sync_db::local_only_key(channel, chat_id)
}

pub fn kv_json<T: DeserializeOwned>(conn: &Connection, key: &str) -> Result<Option<T>> {
    match sync_db::kv_get(conn, key)? {
        Some(raw) => Ok(serde_json::from_str(&raw).ok()),
        None => Ok(None),
    }
}

pub fn kv_put_json<T: Serialize>(conn: &Connection, key: &str, value: &T) -> Result<()> {
    Ok(sync_db::kv_put(conn, key, &serde_json::to_string(value)?)?)
}

/// 本机的设备身份记录（kv `sync/device`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub id: String,
    /// 换身份之前用过的身份，新的在前。
    #[serde(default)]
    pub past: Vec<String>,
    /// 本库里的引擎用过的实例号，新的在后（只留最近若干个）。
    #[serde(default)]
    pub instances: Vec<String>,
    /// 还没有用 `device.hello` 声明「已被当前身份取代」的旧身份。
    #[serde(default)]
    pub announce: Vec<String>,
}

pub fn device_record(conn: &Connection) -> Result<Option<DeviceRecord>> {
    kv_json(conn, KV_SYNC_DEVICE)
}

pub fn put_device_record(conn: &Connection, record: &DeviceRecord) -> Result<()> {
    kv_put_json(conn, KV_SYNC_DEVICE, record)
}

/// 取本机的设备身份；没有就建一个（第一次导出 / 导入会话包、或第一次开启同步时）。
/// 同步表也在这里按需建出。
pub fn ensure_device(conn: &Connection) -> Result<String> {
    sync_db::ensure_sync_tables(conn)?;
    if let Some(record) = device_record(conn)? {
        if ids::is_device_id(&record.id) {
            return Ok(record.id);
        }
    }
    let record = DeviceRecord {
        id: ids::new_device_id(),
        ..DeviceRecord::default()
    };
    put_device_record(conn, &record)?;
    Ok(record.id)
}

/// 本机日志的状态（kv `sync/log`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogState {
    /// 最后一条已封段 op 的序号。
    #[serde(default)]
    pub seq: u64,
    /// 最后一个已封的段（下一个段的 `prev`）。
    #[serde(default)]
    pub last: Option<SegRef>,
    /// 「已封未传」的段。任何时刻至多一个。
    #[serde(default)]
    pub pending: Option<HeadSeg>,
    /// 最近一个汇总包。
    #[serde(default)]
    pub rollup: Option<RollupRef>,
    /// 序号不大于它的日志已被回收。
    #[serde(default)]
    pub floor: u64,
    /// 上次写 head 的时刻（Unix 毫秒）。
    #[serde(default)]
    pub head_ts: i64,
    /// 回执等只影响 head 的内容有变化，还没写出去。
    #[serde(default)]
    pub head_dirty: bool,
    /// 上次写出的 head 里记的日志末尾（序号、末段）、实例号、回收位置与快照。身份自检
    /// 拿存储上的 head 与它们核对（§9.6）；回收对象要等带着新位置的 head 写出之后。
    #[serde(default)]
    pub up_seq: u64,
    #[serde(default)]
    pub up_seg: Option<SegRef>,
    #[serde(default)]
    pub head_instance: String,
    #[serde(default)]
    pub head_floor: u64,
    #[serde(default)]
    pub head_snap: Option<String>,
    /// 上次写出的 head 里汇总包覆盖到的序号：不大于它的零散段对象读者已经用不上了。
    #[serde(default)]
    pub head_packed: u64,
    /// 上次写出的 head 里的运行提示。
    #[serde(default)]
    pub head_running: Vec<String>,
    /// 本机写出的最近一个全量快照（§9.5）。
    #[serde(default)]
    pub snap: Option<SnapRef>,
    /// 打汇总包要用的段在本机和存储上都拿不到一份对的：这个时刻（Unix 毫秒）之前不再去
    /// 取（`upload.rs`）。
    #[serde(default)]
    pub pack_hold_ms: i64,
}

pub fn log_state(conn: &Connection) -> Result<LogState> {
    Ok(kv_json(conn, KV_LOG)?.unwrap_or_default())
}

pub fn put_log_state(conn: &Connection, state: &LogState) -> Result<()> {
    kv_put_json(conn, KV_LOG, state)
}

/// head 里只跟回执有关的内容变了：下一次写 head 时带上（没有新段的话最多攒 10 分钟）。
pub fn mark_head_dirty(conn: &Connection) -> Result<()> {
    let mut log = log_state(conn)?;
    if !log.head_dirty {
        log.head_dirty = true;
        put_log_state(conn, &log)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// sync_state（后写者胜寄存器、分支属性、墓碑、各类登记）
// ---------------------------------------------------------------------------

pub mod scope {
    /// 分支属性：键 `<sid>|<at>|<field>`。
    pub const ATTR: &str = "attr";
    /// 墓碑：键 `<sid>/<tip>`。
    pub const TOMB: &str = "tomb";
    /// 墓碑所带的链（被删分支到 `tip` 为止各消息 `gid` 的随机尾巴）：键与墓碑相同。
    /// 单独放一个 scope：链可以有几十 KB，扫墓碑的地方不必把它们读出来。
    pub const TOMB_CHAIN: &str = rclaw_store::sync_db::SCOPE_TOMB_CHAIN;
    /// 线程的 `session.open` 信息：键 `<sid>`。
    pub const OPEN: &str = "open";
    /// 不在本机落地的线程（通道不参与同步等）：键 `<sid>`。
    pub const SKIP: &str = "skip";
    /// 被隔离的线程：键 `<sid>`，值是原因。
    pub const QUARANTINE: &str = "quarantine";
    /// 本机已封的段：键 `<止:012>`。
    pub const SEG: &str = "seg";
    /// 读到的别的设备的段：键 `<设备>/<止:012>`。
    pub const RSEG: &str = "rseg";
    /// 收件箱里各段的依赖与取回时刻：键 `<设备>/<段名>`。
    pub const INSEG: &str = "inseg";
    /// 各来源设备的读取进度：键 `<设备>`。
    pub const PEER: &str = "peer";
    /// 各设备的 `device.hello`：键 `<设备>`。
    pub const DEVICE: &str = "device";
    /// 已发出的 `session.want`：键 `<sid>/<gid>`。
    pub const WANT: &str = "want";
    /// 已应答的 `session.bundle`：键 `<sid>/<tip>`。
    pub const BUNDLE_SENT: &str = "bundle-sent";
    /// 本机分支的展示用标注：键是本机会话行 id。
    pub const BRANCH: &str = "branch";
    /// 远端线程落地时换了键的展示用标注：键是本机会话行 id。
    pub const LANDED: &str = "landed";
    /// 记忆条目：键 `<target>/<id>`。
    pub const MEM: &str = "mem";
    /// 已合入的每日笔记块：键 `<日期>/<id>`。
    pub const DAILY: &str = "daily";
    /// 每日笔记的发布基线：键 `<日期>`。
    pub const DAILY_BASE: &str = "daily-base";
    /// 文件类数据（人格）：键是相对路径。
    pub const FILE: &str = "file";
    pub const SKILL: &str = "skill";
    pub const BOT: &str = "bot";
    /// 定时任务：键 `<归属设备>/<任务 id>`。
    pub const CRON: &str = "cron";
    /// 向别的设备要过、还在等的附件：键是 blob id，值见 `attach::Want`。
    pub const BLOB_WANT: &str = "blob-want";
    /// 不再自动去取的附件（等了很久没有设备传上来，或者持有它的设备说拿不出来了）：
    /// 键是 blob id，值见 `attach::Lost`。
    pub const BLOB_LOST: &str = "blob-lost";
    /// 本机登记过、后来拿不出来了的附件（文件没了，或内容变了）：键是 blob id，值是原来
    /// 登记的路径与大小。别的设备再来要时照着它重新核对一遍。
    pub const BLOB_GONE: &str = "blob-gone";
    /// 本机写出的汇总包：键 `<止:012>`。
    pub const PACK: &str = "pack";
    /// 等着从存储上删掉的对象：键是对象路径，值是可以动手的条件。
    pub const GC: &str = "gc";
    /// 本机传上去、最近一个快照里已经没人引用的附件：键是 blob id，值是那个快照的时刻。
    pub const BLOB_GC: &str = "blob-gc";
    /// 存储是通的、却没传上去的附件：键是 blob id，值是连着失败的次数。
    pub const BLOB_FAIL: &str = "blob-fail";
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRow {
    pub hlc: String,
    pub hash: Option<String>,
    pub value: Option<String>,
}

impl StateRow {
    pub fn json<T: DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_str(self.value.as_deref()?).ok()
    }
}

pub fn state_get(conn: &Connection, scope: &str, key: &str) -> Result<Option<StateRow>> {
    Ok(conn
        .query_row(
            "SELECT hlc, hash, value FROM sync_state WHERE scope = ?1 AND key = ?2",
            params![scope, key],
            |row| {
                Ok(StateRow {
                    hlc: row.get(0)?,
                    hash: row.get(1)?,
                    value: row.get(2)?,
                })
            },
        )
        .optional()?)
}

/// 无条件写入。
pub fn state_put(
    conn: &Connection,
    scope: &str,
    key: &str,
    hlc: &str,
    hash: Option<&str>,
    value: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_state(scope, key, hlc, hash, value) VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(scope, key) DO UPDATE SET hlc = excluded.hlc, hash = excluded.hash, \
         value = excluded.value",
        params![scope, key, hlc, hash, value],
    )?;
    Ok(())
}

/// 后写者胜：只有 `hlc` 比现有的大才写入。返回是否写入。
pub fn state_put_lww(
    conn: &Connection,
    scope: &str,
    key: &str,
    hlc: &str,
    hash: Option<&str>,
    value: Option<&str>,
) -> Result<bool> {
    let changed = conn.execute(
        "INSERT INTO sync_state(scope, key, hlc, hash, value) VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(scope, key) DO UPDATE SET hlc = excluded.hlc, hash = excluded.hash, \
         value = excluded.value WHERE excluded.hlc > sync_state.hlc",
        params![scope, key, hlc, hash, value],
    )?;
    Ok(changed > 0)
}

pub fn state_put_json<T: Serialize>(
    conn: &Connection,
    scope: &str,
    key: &str,
    hlc: &str,
    value: &T,
) -> Result<()> {
    state_put(
        conn,
        scope,
        key,
        hlc,
        None,
        Some(&serde_json::to_string(value)?),
    )
}

pub fn state_del(conn: &Connection, scope: &str, key: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM sync_state WHERE scope = ?1 AND key = ?2",
        params![scope, key],
    )?;
    Ok(())
}

/// 一个 scope 下键以 `prefix` 开头的全部行，按键排序。
pub fn state_scan(conn: &Connection, scope: &str, prefix: &str) -> Result<Vec<(String, StateRow)>> {
    // 键只含 ASCII：`prefix` ≤ key < `prefix` + U+10FFFF 就是前缀范围。
    let upper = format!("{prefix}\u{10ffff}");
    let mut stmt = conn.prepare_cached(
        "SELECT key, hlc, hash, value FROM sync_state \
         WHERE scope = ?1 AND key >= ?2 AND key < ?3 ORDER BY key",
    )?;
    let rows = stmt.query_map(params![scope, prefix, upper], |row| {
        Ok((
            row.get::<_, String>(0)?,
            StateRow {
                hlc: row.get(1)?,
                hash: row.get(2)?,
                value: row.get(3)?,
            },
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 一个 scope 下键以 `prefix` 开头的全部键，按键排序。只取键，不把值读出来。
pub fn state_keys(conn: &Connection, scope: &str, prefix: &str) -> Result<Vec<String>> {
    let upper = format!("{prefix}\u{10ffff}");
    let mut stmt = conn.prepare_cached(
        "SELECT key FROM sync_state WHERE scope = ?1 AND key >= ?2 AND key < ?3 ORDER BY key",
    )?;
    let rows = stmt.query_map(params![scope, prefix, upper], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
}

/// 没有时间意义的登记项用的占位 HLC（比任何真实值都小）。
pub const HLC_ZERO: &str = "0000000000000-0000-";

// ---------------------------------------------------------------------------
// 来源设备的读取进度
// ---------------------------------------------------------------------------

/// 本机对某台设备日志的读取进度与它的状态（`sync_state` 的 `peer`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerState {
    /// 「已取回」：序号不大于它的 op 已经在收件箱里或已处理。
    #[serde(default)]
    pub fetched: u64,
    /// 已取回的最后一个段。接着读的时候用它核对哈希链。
    #[serde(default)]
    pub fetched_seg: Option<SegRef>,
    /// `fetched` 是吃快照时从它的版本向量里得来的，本机还没在这台设备的日志上核对过
    /// （§9.5）。从这个位置接得上——它正好是日志的末尾，或者接着取到了下一个段——才算
    /// 核对过，在那之前不当回执写进 head；接不上的（超过了日志末尾、链对不上），等过对象
    /// 到齐的宽限就不作数，从日志现存的部分重新读（见 `pull.rs`）。
    #[serde(default)]
    pub from_snap: bool,
    /// 上次拉取提前停了，还没有读到这份 head 的末尾。
    #[serde(default)]
    pub pull_incomplete: bool,
    /// 「已处理」：序号不大于它的 op 都已经看过并有了去处（合入、排队、悬空、隔离或跳过）。
    #[serde(default)]
    pub processed: u64,
    /// 上次成功读到并处理完的 head 的版本标识。
    #[serde(default)]
    pub head_tag: String,
    /// head 里记的写出时刻（那台设备的时钟，Unix 毫秒）。
    #[serde(default)]
    pub head_ts: i64,
    /// 本机最近一次看到它的 head 有变化的时刻（本机时钟，Unix 毫秒）。
    #[serde(default)]
    pub seen_ms: i64,
    #[serde(default)]
    pub instance: String,
    /// 它对各设备日志的「已处理」位置（回执）。
    #[serde(default)]
    pub acks: BTreeMap<String, u64>,
    /// 它上面正在运行的会话（线程 id）。
    #[serde(default)]
    pub running: Vec<String>,
    /// 它日志的回收位置。
    #[serde(default)]
    pub floor: u64,
    /// 日志停在某个位置不再处理的原因（不认识的 op 类型、格式版本更高）。
    #[serde(default)]
    pub halted: Option<String>,
    /// 最近一次读取失败的原因与连续失败的轮数。
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_rounds: u32,
    /// 第一次读取失败的时刻（本机时钟），用来判断「对象还没到齐」是否等得够久了。
    #[serde(default)]
    pub error_since_ms: i64,
    /// 这是本机用过的旧身份：序号不大于它的 op 是本机自己发的，不必取回。
    #[serde(default)]
    pub own_until: Option<u64>,
    /// 它已经被这个新身份取代。
    #[serde(default)]
    pub replaced_by: Option<String>,
    /// 它写出的最近一个全量快照（head 里带着）。
    #[serde(default)]
    pub snap: Option<SnapRef>,
}

/// 超过这么久没见到一台设备的 head 有变化，就当它已经不用了（§9.5）。
pub const PEER_STALE_MS: i64 = 60 * 24 * 3600 * 1000;

impl PeerState {
    /// 这台设备是否还在用：没有被新身份取代，而且最近见过它的 head 有变化。
    pub fn is_active(&self, now_ms: i64) -> bool {
        self.replaced_by.is_none() && self.seen_ms > 0 && now_ms - self.seen_ms <= PEER_STALE_MS
    }
}

pub fn peer_state(conn: &Connection, device: &str) -> Result<PeerState> {
    Ok(known_peer(conn, device)?.unwrap_or_default())
}

/// 本机认过的设备才有登记：它的 head 用这个空间的密钥解开过，或者别的设备（同样拿着
/// 密钥）在日志、快照里提到过它。`heads/` 里光有一个名字不算——存储端不可信，名字可以
/// 随便放（见 [`crate::pull::pull`]）。
pub fn known_peer(conn: &Connection, device: &str) -> Result<Option<PeerState>> {
    Ok(state_get(conn, scope::PEER, device)?.and_then(|row| row.json()))
}

pub fn put_peer_state(conn: &Connection, device: &str, state: &PeerState) -> Result<()> {
    state_put_json(conn, scope::PEER, device, HLC_ZERO, state)
}

pub fn peers(conn: &Connection) -> Result<Vec<(String, PeerState)>> {
    Ok(state_scan(conn, scope::PEER, "")?
        .into_iter()
        .filter_map(|(device, row)| Some((device, row.json()?)))
        .collect())
}

/// 各设备的 `device.hello` 信息（`sync_state` 的 `device`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub replaces: Option<String>,
}

pub fn device_info(conn: &Connection, device: &str) -> Result<Option<DeviceInfo>> {
    Ok(state_get(conn, scope::DEVICE, device)?.and_then(|row| row.json()))
}

/// 设备的展示名：`device.hello` 里的名字；还没到时用 id 的前 4 位。
pub fn device_label(conn: &Connection, device: &str) -> Result<String> {
    let name = device_info(conn, device)?
        .map(|info| info.name)
        .unwrap_or_default();
    Ok(if name.trim().is_empty() {
        format!("设备 {}", &device[..4.min(device.len())])
    } else {
        name
    })
}

// ---------------------------------------------------------------------------
// 附件对象（`sync_blob`）
// ---------------------------------------------------------------------------

/// 本机有这个文件，没有上传（附件同步没开，或属于还没选择上传的存量）。
pub const BLOB_HELD: i64 = 0;
/// 本机有这个文件，等待上传。
pub const BLOB_PENDING: i64 = 1;
/// 本机有这个文件，远端也有。
pub const BLOB_UPLOADED: i64 = 2;
/// 本机没有，等待从远端取回（自动或按需）。
pub const BLOB_REMOTE: i64 = 3;
/// 已取回到 `rel_path`。
pub const BLOB_FETCHED: i64 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRow {
    pub blob_id: String,
    /// 相对 agent 根的路径，正斜杠：本机文件所在处，或取回后的落点。
    pub rel_path: String,
    pub size: u64,
    pub state: i64,
}

fn row_to_blob(row: &rusqlite::Row<'_>) -> rusqlite::Result<BlobRow> {
    Ok(BlobRow {
        blob_id: row.get(0)?,
        rel_path: row.get(1)?,
        size: row.get::<_, i64>(2)?.max(0) as u64,
        state: row.get(3)?,
    })
}

pub fn blob_get(conn: &Connection, blob_id: &str) -> Result<Option<BlobRow>> {
    Ok(conn
        .query_row(
            "SELECT blob_id, rel_path, size, state FROM sync_blob WHERE blob_id = ?1",
            params![blob_id],
            row_to_blob,
        )
        .optional()?)
}

/// 登记一个附件对象；已经登记过就保持原样。返回它现在的状态。
pub fn blob_register(
    conn: &Connection,
    blob_id: &str,
    rel_path: &str,
    size: u64,
    state: i64,
) -> Result<i64> {
    conn.execute(
        "INSERT OR IGNORE INTO sync_blob(blob_id, rel_path, size, state) VALUES (?1, ?2, ?3, ?4)",
        params![blob_id, rel_path, size as i64, state],
    )?;
    Ok(conn.query_row(
        "SELECT state FROM sync_blob WHERE blob_id = ?1",
        params![blob_id],
        |row| row.get(0),
    )?)
}

/// 删掉一个对象的登记（`gone` 记录另存，不在这里）。
pub fn blob_delete(conn: &Connection, blob_id: &str) -> Result<()> {
    conn.execute("DELETE FROM sync_blob WHERE blob_id = ?1", params![blob_id])?;
    Ok(())
}

pub fn blob_set_state(conn: &Connection, blob_id: &str, state: i64) -> Result<()> {
    conn.execute(
        "UPDATE sync_blob SET state = ?2 WHERE blob_id = ?1",
        params![blob_id, state],
    )?;
    Ok(())
}

pub fn blob_set(conn: &Connection, blob_id: &str, rel_path: &str, state: i64) -> Result<()> {
    conn.execute(
        "UPDATE sync_blob SET rel_path = ?2, state = ?3 WHERE blob_id = ?1",
        params![blob_id, rel_path, state],
    )?;
    Ok(())
}

/// 落在同一个落点上的登记：`(对象名, 长度)`。旧消息的附件索引可能缺项，占位文件与这里
/// 的长度对上时才能准确认出是哪个对象。
pub fn blob_objects_at(conn: &Connection, rel_path: &str) -> Result<Vec<(String, u64)>> {
    let mut stmt =
        conn.prepare_cached("SELECT blob_id, size FROM sync_blob WHERE rel_path = ?1")?;
    let rows = stmt.query_map(params![rel_path], |row| {
        Ok((row.get(0)?, row.get::<_, i64>(1)?.max(0) as u64))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 登记一个要随消息/快照发出的本地对象：`state` 是要收敛到的目标状态——本机缓存
/// （`BLOB_FETCHED`）在范围关着时转 `HELD`、范围开时升 `PENDING`；已 `HELD` 的对象
/// 在新引用且范围允许时升 `PENDING`。远端已经确认过的（`UPLOADED`/`REMOTE`）不回退。
pub fn blob_register_or_upload(
    conn: &Connection,
    blob_id: &str,
    rel_path: &str,
    size: u64,
    state: i64,
) -> Result<i64> {
    let current: Option<i64> = conn
        .query_row(
            "SELECT state FROM sync_blob WHERE blob_id = ?1",
            params![blob_id],
            |row| row.get(0),
        )
        .optional()?;
    // 「目标状态」是要收敛到的状态，不只是新插入行的初值：
    // - 本机缓存（FETCHED）遇到范围关着 → HELD（重新开启后「上传已有附件」才找得到）；
    // - 本机缓存或 HELD 在新消息引用且范围允许时 → PENDING（引用要能取到字节）；
    // - UPLOADED / REMOTE 是远端已有的结论，不回退。
    let next = match (current, state) {
        (None, _) => Some(state),
        (Some(BLOB_FETCHED), BLOB_HELD) | (Some(BLOB_FETCHED), BLOB_PENDING) => Some(state),
        (Some(BLOB_HELD), BLOB_PENDING) => Some(state),
        (Some(_), _) => None,
    };
    match next {
        Some(next) => {
            if current.is_none() {
                conn.execute(
                    "INSERT INTO sync_blob(blob_id, rel_path, size, state) VALUES (?1, ?2, ?3, ?4)",
                    params![blob_id, rel_path, size as i64, next],
                )?;
            } else {
                conn.execute(
                    "UPDATE sync_blob SET state = ?2, rel_path = ?3 WHERE blob_id = ?1",
                    params![blob_id, next, rel_path],
                )?;
            }
            Ok(next)
        }
        None => Ok(current.unwrap_or(state)),
    }
}

/// 待上传的附件（按入队顺序取 `limit` 个）。`skip_skill` 为真时在 SQL 里排除「技能依赖
/// 但没被点名」的对象：先取前 N 个再在内存里过滤，会让队首的一堆暂停项把后面的普通
/// 附件永远挡住。
pub fn blobs_for_upload(
    conn: &Connection,
    skip_skill: bool,
    skill_scope: &str,
    named_scope: &str,
    limit: usize,
) -> Result<Vec<BlobRow>> {
    if !skip_skill {
        return blobs_in_state(conn, BLOB_PENDING, limit);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT b.blob_id, b.rel_path, b.size, b.state FROM sync_blob b          WHERE b.state = ?1 AND (              NOT EXISTS (SELECT 1 FROM sync_state s WHERE s.scope = ?2 AND s.key = b.blob_id)              OR EXISTS (SELECT 1 FROM sync_state s WHERE s.scope = ?3 AND s.key = b.blob_id)          ) ORDER BY b.rowid LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        params![BLOB_PENDING, skill_scope, named_scope, limit as i64],
        row_to_blob,
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 把一个等着上传的附件挪到队尾。队列按 rowid 排（见 [`blobs_in_state`]）：删掉再插入，
/// 它就排到了最后。登记已经不是「等待上传」时不动它，返回 `false`。
pub fn blob_requeue(conn: &Connection, blob_id: &str) -> Result<bool> {
    let Some(row) = blob_get(conn, blob_id)?.filter(|row| row.state == BLOB_PENDING) else {
        return Ok(false);
    };
    conn.execute("DELETE FROM sync_blob WHERE blob_id = ?1", params![blob_id])?;
    conn.execute(
        "INSERT INTO sync_blob(blob_id, rel_path, size, state) VALUES (?1, ?2, ?3, ?4)",
        params![row.blob_id, row.rel_path, row.size as i64, row.state],
    )?;
    Ok(true)
}

pub fn blobs_in_state(conn: &Connection, state: i64, limit: usize) -> Result<Vec<BlobRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT blob_id, rel_path, size, state FROM sync_blob WHERE state = ?1 \
         ORDER BY rowid LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![state, limit as i64], row_to_blob)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.sqlite")).unwrap();
        (dir, db)
    }

    #[test]
    fn write_commits_and_rolls_back_with_the_original_error() {
        let (_g, db) = db();
        let device = write(&db, |tx| ensure_device(tx)).unwrap();
        assert!(ids::is_device_id(&device));
        // 再取一次是同一个身份。
        assert_eq!(write(&db, |tx| ensure_device(tx)).unwrap(), device);

        let failed: Result<()> = write(&db, |tx| {
            state_put(tx, scope::SKIP, "x", HLC_ZERO, None, None)?;
            Err(SyncError::invalid("boom"))
        });
        assert!(matches!(failed, Err(SyncError::Invalid(ref m)) if m == "boom"));
        // 回滚了。
        assert!(read(&db, |c| state_get(c, scope::SKIP, "x"))
            .unwrap()
            .is_none());

        write(&db, |tx| {
            state_put(tx, scope::SKIP, "x", HLC_ZERO, None, Some("v"))
        })
        .unwrap();
        // 落盘点不占库锁、不动库里的内容；连着做也没事。
        flush(&db).unwrap();
        flush(&db).unwrap();
        let row = read(&db, |c| state_get(c, scope::SKIP, "x"))
            .unwrap()
            .unwrap();
        assert_eq!(row.value.as_deref(), Some("v"));
    }

    #[test]
    fn lww_keeps_the_larger_hlc() {
        let (_g, db) = db();
        write(&db, |tx| {
            ensure_device(tx)?;
            let (old, new) = ("1763438400000-0000-a", "1763438400000-0001-a");
            assert!(state_put_lww(tx, scope::ATTR, "k", new, None, Some("new"))?);
            assert!(!state_put_lww(
                tx,
                scope::ATTR,
                "k",
                old,
                None,
                Some("old")
            )?);
            assert!(!state_put_lww(
                tx,
                scope::ATTR,
                "k",
                new,
                None,
                Some("same")
            )?);
            assert_eq!(
                state_get(tx, scope::ATTR, "k")?.unwrap().value.as_deref(),
                Some("new")
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn scan_returns_only_the_prefix() {
        let (_g, db) = db();
        write(&db, |tx| {
            ensure_device(tx)?;
            for key in ["a/1", "a/2", "ab/1", "b/1"] {
                state_put(tx, scope::RSEG, key, HLC_ZERO, None, None)?;
            }
            state_put(tx, scope::SEG, "a/9", HLC_ZERO, None, None)?;
            let keys: Vec<String> = state_scan(tx, scope::RSEG, "a/")?
                .into_iter()
                .map(|(key, _)| key)
                .collect();
            assert_eq!(keys, ["a/1", "a/2"]);
            assert_eq!(state_scan(tx, scope::RSEG, "")?.len(), 4);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn peer_and_log_state_round_trip() {
        let (_g, db) = db();
        write(&db, |tx| {
            ensure_device(tx)?;
            assert_eq!(peer_state(tx, "0123456789abcdef")?, PeerState::default());
            let mut peer = PeerState::default();
            peer.fetched = 7;
            peer.processed = 5;
            put_peer_state(tx, "0123456789abcdef", &peer)?;
            assert_eq!(peer_state(tx, "0123456789abcdef")?, peer);
            assert_eq!(peers(tx)?.len(), 1);

            assert_eq!(log_state(tx)?, LogState::default());
            let mut log = LogState::default();
            log.seq = 3;
            put_log_state(tx, &log)?;
            assert_eq!(log_state(tx)?.seq, 3);
            Ok(())
        })
        .unwrap();
    }
}
