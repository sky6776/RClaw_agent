//! 文件类数据的同步（§8）：记忆、每日笔记、人格、技能、bot、定时任务。
//!
//! 分两半：
//!
//! - **记录**（[`record_op`]）：合入远端 op 时，在合入事务里只更新 `sync_state` 里的
//!   寄存器（后写者胜），不碰文件。
//! - **对比**（[`reconcile`]）：在库锁之外读写文件。把文件的当前内容与上次同步后的基线
//!   比较，差异就是本机变更，发布出去；寄存器里还没落到文件上的远端变更，落下去，或者
//!   挂成等本机确认的事项。本机变更在对比时才拿到 HLC，所以它总是新于同一轮里还没
//!   落地的远端变更——两边都改过时，本机的留下并发布，对方那一版不落地。
//!
//! 寄存器行的 `hash` 列在这里当处理标记：空 = 远端变更刚到，还没看过；[`AWAITING`] =
//! 看过了，在等用户到本机确认；[`HANDLED`] = 已处理；[`REJECTED`] = 这个版本的程序没让它
//! 通过检查（程序启动时会再试一次，升级之后也许就认识了）。

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::crypto::Keys;
use crate::db::{self, StateRow};
use crate::error::{Result, SyncError};
use crate::map;
use crate::op::Op;
use crate::publish::Settings;
use crate::status::{self, PendingItem};
use crate::{Ctx, Reload};

pub mod bots;
pub mod cron;
pub mod daily;
pub mod memory;
pub mod persona;
pub mod skills;

pub(crate) const HANDLED: &str = "1";
pub(crate) const REJECTED: &str = "x";
pub(crate) const AWAITING: &str = "?";

/// 参与 [`HANDLED`] / [`REJECTED`] 标记的寄存器。全量快照、换身份后的重新发布都按这张表
/// 把各寄存器当前生效的 op 取出来。
pub(crate) const REGISTERS: [&str; 7] = [
    db::scope::MEM,
    db::scope::DAILY,
    db::scope::FILE,
    db::scope::SKILL,
    db::scope::BOT,
    db::scope::CRON,
    cron::SCOPE_XFER,
];

/// 合入时哪几类文件数据的寄存器有了新内容。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dirty {
    pub memory: bool,
    pub daily: bool,
    pub persona: bool,
    pub skills: bool,
    pub bots: bool,
    pub cron: bool,
}

impl Dirty {
    pub fn all() -> Dirty {
        Dirty {
            memory: true,
            daily: true,
            persona: true,
            skills: true,
            bots: true,
            cron: true,
        }
    }

    pub fn any(&self) -> bool {
        self.memory || self.daily || self.persona || self.skills || self.bots || self.cron
    }

    pub fn merge(&mut self, other: Dirty) {
        self.memory |= other.memory;
        self.daily |= other.daily;
        self.persona |= other.persona;
        self.skills |= other.skills;
        self.bots |= other.bots;
        self.cron |= other.cron;
    }
}

/// 把一条文件类 op 记进寄存器。在合入事务里执行，不碰文件。内容不合规返回
/// [`SyncError::Invalid`]，调用方丢弃这条 op。本机的范围设置不影响记录：以后打开某个
/// 范围时，之前收到的内容还在。
///
/// `stored` 是这条 op 写进寄存器的值（[`stored_text`]），调用方在库锁之外备好：技能的
/// 清单可能有几兆字节，事务里只是把文本写进去。
pub fn record_op(
    conn: &Connection,
    origin: &str,
    hlc: &str,
    op: &Op,
    stored: &str,
) -> Result<Dirty> {
    let mut dirty = Dirty::default();
    match op {
        Op::MemAdd { .. } | Op::MemDel { .. } => {
            dirty.memory = memory::record(conn, hlc, op, stored)?;
        }
        Op::DailyAppend { .. } => dirty.daily = daily::record(conn, hlc, op, stored)?,
        Op::FilePut { .. } | Op::FileDel { .. } => {
            dirty.persona = persona::record(conn, hlc, op, stored)?;
        }
        Op::SkillPut { .. } | Op::SkillDel { .. } => {
            dirty.skills = skills::record(conn, hlc, op, stored)?;
        }
        Op::BotPut { .. } | Op::BotDel { .. } => dirty.bots = bots::record(conn, hlc, op, stored)?,
        Op::CronPut { .. } | Op::CronDel { .. } | Op::CronTransfer { .. } => {
            dirty.cron = cron::record(conn, origin, hlc, op, stored)?;
        }
        _ => {}
    }
    Ok(dirty)
}

/// 寄存器一行的值：最近一条生效的 op 原样留着。落文件、等确认时用它的内容；写全量快照
/// 时也靠它重新生成 op（§9.5）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Stored {
    /// 写出这条 op 的设备。
    pub origin: String,
    pub op: Op,
}

/// 寄存器里的一行。
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub key: String,
    pub hlc: String,
    /// 处理标记：空 = 远端变更还没落到本机。
    pub mark: Option<String>,
    pub origin: String,
    pub op: Op,
}

impl Row {
    /// 刚到、还没看过。
    pub(crate) fn is_new(&self) -> bool {
        self.mark.is_none()
    }

    /// 远端的变更还没落到本机：刚到，或在等用户确认。
    pub(crate) fn is_unapplied(&self) -> bool {
        self.mark.is_none() || self.mark.as_deref() == Some(AWAITING)
    }
}

/// 把一份要被同步替换掉的本机内容留一个副本：`<数据根>/sync/conflicts/<kind>/<file>`。
/// 留不下只记日志——副本是给用户的后悔药，不值得让同步停下来。
pub(crate) fn keep_copy(ctx: &Ctx, kind: &str, file: &str, content: &[u8]) {
    let path = ctx.paths.conflicts.join(kind).join(file);
    if let Err(error) = rclaw_store::atomic_write(&path, content) {
        tracing::warn!("保留冲突副本 {} 失败: {error}", path.display());
    }
}

/// 解析寄存器里的一行。技能的清单可能有几兆字节，调用方可以先在库锁里只取出原文
/// （`db::state_get`），出了锁再调这个。
pub(crate) fn to_row(key: String, row: StateRow) -> Option<Row> {
    let stored: Stored = row.json()?;
    Some(Row {
        key,
        hlc: row.hlc,
        mark: row.hash,
        origin: stored.origin,
        op: stored.op,
    })
}

/// [`Stored`] 的借用形态：写寄存器时不必把 op 再复制一份。
#[derive(Serialize)]
struct StoredRef<'a> {
    origin: &'a str,
    op: &'a Op,
}

/// 一条 op 写进寄存器的值（[`Stored`] 的 JSON）。大的 op 在库锁之外调。
pub fn stored_text(origin: &str, op: &Op) -> Result<String> {
    Ok(serde_json::to_string(&StoredRef { origin, op })?)
}

/// 把一条远端 op 记进寄存器（后写者胜，标记为「还没落到本机」）。返回是否写入。`stored`
/// 是 [`stored_text`] 备好的值。
pub(crate) fn put_remote(
    conn: &Connection,
    scope: &str,
    key: &str,
    hlc: &str,
    stored: &str,
) -> Result<bool> {
    db::state_put_lww(conn, scope, key, hlc, None, Some(stored))
}

/// 序列化好的一条本机变更：待发队列里的 op，和寄存器的值。
pub(crate) struct Prepared {
    op: String,
    stored: String,
}

/// 把一条本机变更序列化好，留给 [`publish_prepared`] 在事务里写。大的 op（技能的清单
/// 可能有几兆字节）在库锁之外调。
pub(crate) fn prepare(s: &Settings, op: &Op) -> Result<Prepared> {
    let text = serde_json::to_string(op)?;
    // 寄存器的值是 `{"origin":…,"op":…}`（[`Stored`]）：op 的文本原样嵌进去，不再序列化
    // 一遍。
    let stored = format!(
        "{{\"origin\":{},\"op\":{text}}}",
        serde_json::to_string(&s.device)?
    );
    Ok(Prepared { op: text, stored })
}

/// 本机的变更：排进待发队列，寄存器同时更新成它（已处理）。它的 HLC 是现取的，新于本机
/// 见过的任何一条远端变更。
pub(crate) fn publish_prepared(
    conn: &Connection,
    s: &Settings,
    scope: &str,
    key: &str,
    prepared: &Prepared,
) -> Result<()> {
    let hlc = map::enqueue_text(conn, &s.device, None, &prepared.op)?;
    db::state_put(
        conn,
        scope,
        key,
        &hlc,
        Some(HANDLED),
        Some(&prepared.stored),
    )
}

/// 同 [`publish_prepared`]，序列化也在这里做（小的 op）。
pub(crate) fn publish(
    conn: &Connection,
    s: &Settings,
    scope: &str,
    key: &str,
    op: &Op,
) -> Result<()> {
    publish_prepared(conn, s, scope, key, &prepare(s, op)?)
}

pub(crate) fn row(conn: &Connection, scope: &str, key: &str) -> Result<Option<Row>> {
    Ok(db::state_get(conn, scope, key)?.and_then(|row| to_row(key.to_string(), row)))
}

/// 一个寄存器里键以 `prefix` 开头的全部行，按键排序。值读不懂的行跳过。
pub(crate) fn rows(conn: &Connection, scope: &str, prefix: &str) -> Result<Vec<Row>> {
    Ok(db::state_scan(conn, scope, prefix)?
        .into_iter()
        .filter_map(|(key, row)| to_row(key, row))
        .collect())
}

/// 同 [`rows`]，只要还没落到本机的远端变更，按 HLC 先后排。
pub(crate) fn unhandled(conn: &Connection, scope: &str, prefix: &str) -> Result<Vec<Row>> {
    let upper = format!("{prefix}\u{10ffff}");
    let found: Vec<(String, StateRow)> = {
        let mut stmt = conn.prepare_cached(
            "SELECT key, hlc, value FROM sync_state \
             WHERE scope = ?1 AND key >= ?2 AND key < ?3 AND hash IS NULL ORDER BY hlc, key",
        )?;
        let found = stmt.query_map(params![scope, prefix, upper], |row| {
            Ok((
                row.get::<_, String>(0)?,
                StateRow {
                    hlc: row.get(1)?,
                    hash: None,
                    value: row.get(2)?,
                },
            ))
        })?;
        found.collect::<rusqlite::Result<_>>()?
    };
    Ok(found
        .into_iter()
        .filter_map(|(key, row)| {
            let parsed = to_row(key.clone(), row);
            if parsed.is_none() {
                // 不该出现（写入时都是本模块自己序列化的）。留着不动，只记日志。
                tracing::warn!("同步寄存器 {scope}/{key} 的内容读不懂，已跳过");
            }
            parsed
        })
        .collect())
}

/// 给一行寄存器打处理标记。带上当时看到的 HLC：这期间来了更新的远端变更就不打。
pub(crate) fn mark(conn: &Connection, scope: &str, key: &str, hlc: &str, mark: &str) -> Result<()> {
    conn.execute(
        "UPDATE sync_state SET hash = ?4 WHERE scope = ?1 AND key = ?2 AND hlc = ?3",
        params![scope, key, hlc, mark],
    )?;
    Ok(())
}

/// 各寄存器当前生效的 op 的原始行：`(HLC, 值的 JSON 原文)`。全量快照（§9.5）与换身份后
/// 的重新发布（§9.6）用：新设备把它们当远端 op 记一遍（[`record_op`]），就得到同样的
/// 寄存器。这里只取原文——技能的清单可能有几兆字节，解析（[`parse_register`]）留到库锁
/// 之外。
pub(crate) fn register_rows(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for scope in REGISTERS {
        for (_, row) in db::state_scan(conn, scope, "")? {
            if let Some(value) = row.value {
                out.push((row.hlc, value));
            }
        }
    }
    Ok(out)
}

/// 解析 [`register_rows`] 的一行：`(写出这条 op 的设备, op)`。
pub(crate) fn parse_register(value: &str) -> Option<(String, Op)> {
    let stored: Stored = serde_json::from_str(value).ok()?;
    Some((stored.origin, stored.op))
}

/// 程序启动时调用：之前被拒的远端变更再试一次。
pub fn retry_rejected(conn: &Connection) -> Result<()> {
    for scope in REGISTERS {
        conn.execute(
            "UPDATE sync_state SET hash = NULL WHERE scope = ?1 AND hash = ?2",
            params![scope, REJECTED],
        )?;
    }
    Ok(())
}

/// 文件的指纹：`(名字, 长度, 修改时间毫秒)` 的列表。
pub(crate) type Stamp = Vec<(String, u64, i64)>;

/// 一个文件的长度与修改时间。文件不存在时长度记为 `u64::MAX`。
pub(crate) fn file_stamp(path: &Path) -> (u64, i64) {
    match std::fs::metadata(path) {
        Ok(meta) => (meta.len(), mtime_ms(&meta)),
        Err(_) => (u64::MAX, 0),
    }
}

pub(crate) fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_millis() as i64)
}

/// 对比用的进程内缓存：各份数据上次对比完时的文件指纹。指纹没变、也没有待落地的远端
/// 变更，就不用再读文件。（指纹只看长度和修改时间，同一时刻内等长的改动会漏掉，所以
/// 引擎每隔一阵做一次不看缓存的完整对比。）
#[derive(Debug, Default)]
pub struct Cache {
    stamps: HashMap<String, Stamp>,
    /// 技能文件的摘要：键 `<技能名>/<路径>`，值 `(长度, 修改时间, sha256, 是否可执行)`。
    /// 长度和修改时间都没变的文件不重新读。
    hashes: HashMap<String, (u64, i64, String, bool)>,
    /// 没做成、要隔一阵再试的事：键由各类数据自己定，值 `(连着失败的次数, 下次可以再试
    /// 的时刻)`。不随 [`Cache::clear`] 清掉——完整对比不该把「刚失败过」忘了。
    retries: HashMap<String, (u32, i64)>,
}

/// 重试的间隔：第一次失败后等 30 秒，之后每次翻倍，最长 30 分钟。
const RETRY_FIRST_MS: i64 = 30_000;
const RETRY_MAX_MS: i64 = 30 * 60_000;

impl Cache {
    pub(crate) fn is_current(&self, key: &str, stamp: &Stamp) -> bool {
        self.stamps.get(key) == Some(stamp)
    }

    pub(crate) fn put(&mut self, key: &str, stamp: Stamp) {
        self.stamps.insert(key.to_string(), stamp);
    }

    pub(crate) fn forget(&mut self, key: &str) {
        self.stamps.remove(key);
    }

    pub(crate) fn hash_of(&self, key: &str, len: u64, mtime: i64) -> Option<(String, bool)> {
        self.hashes
            .get(key)
            .filter(|(known_len, known_mtime, _, _)| *known_len == len && *known_mtime == mtime)
            .map(|(_, _, sha256, exec)| (sha256.clone(), *exec))
    }

    pub(crate) fn put_hash(
        &mut self,
        key: String,
        len: u64,
        mtime: i64,
        sha256: String,
        exec: bool,
    ) {
        self.hashes.insert(key, (len, mtime, sha256, exec));
    }

    pub(crate) fn retain_hashes(&mut self, keep: impl Fn(&str) -> bool) {
        self.hashes.retain(|key, _| keep(key));
    }

    /// `key` 这件事刚失败过、还没到可以再试的时候。
    pub(crate) fn backing_off(&self, key: &str, now_ms: i64) -> bool {
        self.retries
            .get(key)
            .is_some_and(|(_, until)| now_ms < *until)
    }

    /// 记一次失败，返回连着失败了几次。
    pub(crate) fn failed(&mut self, key: &str, now_ms: i64) -> u32 {
        let entry = self.retries.entry(key.to_string()).or_insert((0, 0));
        entry.0 = entry.0.saturating_add(1);
        let wait = RETRY_FIRST_MS
            .saturating_mul(1i64 << (entry.0 - 1).min(16))
            .min(RETRY_MAX_MS);
        entry.1 = now_ms.saturating_add(wait);
        entry.0
    }

    /// 做成了（或者这件事已经不用做了）：不再记着失败。
    pub(crate) fn settled(&mut self, key: &str) {
        self.retries.remove(key);
    }

    /// 测试用：当作等待的时间已经过去了（失败的次数还记着）。
    #[cfg(test)]
    pub(crate) fn retry_now(&mut self, key: &str) {
        if let Some(entry) = self.retries.get_mut(key) {
            entry.1 = 0;
        }
    }

    pub fn clear(&mut self) {
        self.stamps.clear();
        self.hashes.clear();
    }
}

/// 一轮对比的结果。
#[derive(Debug, Default)]
pub struct FilesReport {
    /// 排进待发队列的 op 数。
    pub published: usize,
    /// 落到本机文件上的远端变更数。
    pub applied: usize,
    /// 待处理事项有变化。
    pub status_changed: bool,
    /// 宿主要重新加载的数据。
    pub reload: BTreeSet<Reload>,
    /// 要先取回才能继续的附件对象（技能里的大文件）。
    pub want_blobs: Vec<String>,
    /// 有事情这一轮没做成，要过一会儿再来一轮：文件在对比期间又被改了，或者替换技能
    /// 目录要等到没有 turn 在跑。
    pub retry: bool,
}

/// 做一轮对比。各类数据互不牵连：一类失败只记日志，其余照常。`force` 为真时不看指纹
/// 缓存，并把每日笔记的全部日期都看一遍。
pub fn reconcile(
    ctx: &Ctx,
    s: &Settings,
    keys: &Keys,
    cache: &mut Cache,
    force: bool,
) -> FilesReport {
    let mut report = FilesReport::default();
    if force {
        cache.clear();
    }
    let failed = |what: &str, result: Result<()>| {
        if let Err(error) = result {
            tracing::warn!("同步{what}失败: {error}");
        }
    };
    if s.scopes.bots {
        // bot 定义先于 bot 记忆：新来的 bot 落地之后，它的记忆才有地方放。
        failed("bot 定义", bots::reconcile(ctx, s, cache, &mut report));
    }
    if s.scopes.memory {
        failed("记忆", memory::reconcile(ctx, s, cache, &mut report));
        failed("每日笔记", daily::reconcile(ctx, s, force, &mut report));
    }
    if s.scopes.persona {
        failed("人格", persona::reconcile(ctx, s, cache, &mut report));
    }
    if s.scopes.skills {
        failed("技能", skills::reconcile(ctx, s, keys, cache, &mut report));
    }
    if s.scopes.cron {
        failed("定时任务", cron::reconcile(ctx, s, &mut report));
    } else {
        // 范围关着也有收尾要做：发布过的任务撤回，已经开始的交接走完。
        failed("定时任务", cron::wind_down(ctx, s, &mut report));
    }
    report
}

/// 从库里的状态看得出来的文件类待处理事项（暂停同步的记忆文件等）。
pub(crate) fn derived(conn: &Connection) -> Result<Vec<PendingItem>> {
    memory::derived(conn)
}

/// 待处理事项的详情（人格、bot、技能的差异对比）。等确认的改动带着 `hlc`：这份详情说的
/// 是哪一版，决定的时候要原样交回来（见 [`decide`]）。
pub fn detail(ctx: &Ctx, id: &str) -> Result<serde_json::Value> {
    let Some(item) = status::get(&ctx.paths, id) else {
        // 不存成文件、从库里的状态现算的事项（被隔离的会话、暂停同步的记忆文件等）：
        // 没有更多可看的，详情就是它的说明。
        let now = crate::attach::now_ms();
        let derived = db::read(ctx.db(), |conn| {
            if !rclaw_store::sync_db::sync_tables_exist(conn)? {
                return Ok(Vec::new());
            }
            status::derived(conn, now)
        })?;
        return derived
            .into_iter()
            .find(|item| item.id == id)
            .map(|item| serde_json::json!({ "detail": item.detail }))
            .ok_or_else(|| SyncError::invalid("这条待处理事项已经不在了"));
    };
    let mut detail = match item.kind.as_str() {
        persona::KIND => persona::detail(ctx, &item)?,
        bots::KIND => bots::detail(ctx, &item)?,
        skills::KIND => skills::detail(ctx, &item)?,
        _ => serde_json::json!({ "detail": item.detail }),
    };
    if let (Some(version), Some(object)) = (item.version(), detail.as_object_mut()) {
        object.insert("hlc".into(), serde_json::Value::String(version.to_string()));
    }
    Ok(detail)
}

/// 用户对一条存成文件的待处理事项做了决定。返回宿主要重新加载的数据。
///
/// `reviewed` 是用户看到的那一版（事项 `payload` 或详情里的 `hlc`）。等确认的改动必须带
/// 着它，而且得和现在等确认的是同一版：事项的标识不随版本变，用户看着旧内容点下「接受」
/// 的时候，等着的可能已经是另一份没看过的内容了。只读的提示没有版本，不用带。
pub fn decide(
    ctx: &Ctx,
    s: &Settings,
    id: &str,
    action: &str,
    reviewed: Option<&str>,
) -> Result<Option<Reload>> {
    // 「记忆文件里原有的条目都不见了」不存成文件，是从库里的状态现算的。
    if memory::decide_wipe(ctx, id, action, reviewed)? {
        return Ok(None);
    }
    let item = status::get(&ctx.paths, id)
        .ok_or_else(|| SyncError::invalid("这条待处理事项已经不在了"))?;
    if !item.actions.iter().any(|allowed| allowed == action) {
        return Err(SyncError::invalid("这条待处理事项不支持这个操作"));
    }
    status::check_reviewed(&item, reviewed)?;
    let accept = action == "accept";
    match item.kind.as_str() {
        persona::KIND => {
            persona::decide(ctx, s, &item, accept).map(|()| accept.then_some(Reload::Persona))
        }
        bots::KIND => bots::decide(ctx, s, &item, accept),
        skills::KIND => skills::decide(ctx, s, &item, accept),
        _ => {
            // 只读的提示：关掉。
            status::remove(&ctx.paths, id);
            Ok(None)
        }
    }
}

/// 小工具：`sha256` 十六进制摘要的前 `n` 位。
pub(crate) fn short_hash(bytes: &[u8], n: usize) -> String {
    crate::crypto::sha256_hex(bytes)[..n].to_string()
}

/// 冲突副本用的时间戳。
pub(crate) fn stamp_now() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}
