//! 定时任务（§8.6）。
//!
//! 每个任务有一个归属设备，只在它上面执行：本机的 `cron.json` 只含归属本机的任务，调度器
//! 不用改。别的设备的任务只进寄存器，并写一份只读镜像 `cron.remote.json`，调度器不加载
//! 它——所以同一个任务在结构上不可能在两台设备上各触发一次。
//!
//! 换归属要用户在接手的那台设备上发起，一共四种记录，都是 `cron.transfer`（寄存器
//! `cron-xfer`，按任务、版本、阶段和发送者存放，每个任务只有一条生效）：
//!
//! | 记录 | 谁发 | 意思 |
//! |---|---|---|
//! | 请求 | 接手的设备 | 想把这个任务移过来 |
//! | 强制接管 | 接手的设备 | 原设备陈旧、或被用户标为「已丢失」：不等它，先执行起来 |
//! | 回执 | 原归属设备 | 已经把任务从自己的任务表里移除 |
//! | 落定 | 任务最后落在谁那里，就由谁发 | 任务已经在 `to` 的任务表里，这次交接到此为止 |
//!
//! 接手的设备看到指向自己的回执才把任务加进任务表，然后发落定。原归属设备交出任务之后
//! 把它的定义留着（`cron-parked`）：回执传出去一天还没等到对方落定，就把任务收回来、
//! 自己发落定——任务不会因为接手的那台再也不出现而没有设备执行。两边都发了落定时，先发
//! 的算数，另一边把自己那份撤掉。
//!
//! 每条记录都带着任务的版本（`rev`）：任务在原设备上改过、删过之后，针对旧版的请求和回执
//! 都作废——落到接手设备上的只能是用户点下去时看到的那一份。往本机任务表里加东西一律要
//! 有本机用户发起过的请求（`cron-claim`）：别的设备伪造一条回执，什么也加不进来。

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rclaw_store::sync_db;
use rclaw_store::CronJob;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use super::{keep_copy, publish, put_remote, rows, stamp_now, FilesReport, HANDLED};
use crate::attach::now_ms;
use crate::crypto::sha256_hex;
use crate::db::{self, scope, PeerState};
use crate::error::{Result, SyncError};
use crate::ids;
use crate::map;
use crate::op::Op;
use crate::publish::Settings;
use crate::status;
use crate::{CronHost, Ctx, Reload};

/// 归属变更。回执与落定分开保留，新设备恢复快照时也要用回执核对落定。
pub(crate) const SCOPE_XFER: &str = "cron-xfer";
/// 本机任务上次发布的版本：键是任务 id，值是摘要。
const SCOPE_BASE: &str = "cron-base";
/// 本机用户发起过、还没了结的接手请求：键 `<原归属设备>/<任务 id>`，值是 [`Claim`]。
const SCOPE_CLAIM: &str = "cron-claim";
/// 本机交出去、还没等到对方落定的任务：键 `<交出时本机的身份>/<任务 id>`，值是
/// [`Parked`]。
const SCOPE_PARKED: &str = "cron-parked";
/// 用户标为「已丢失」的设备：键是设备 id。只在本机生效。
const SCOPE_LOST: &str = "lost";
/// 一个任务定义的大小上限。
const JOB_MAX: usize = 64 * 1024;
const MIRROR_FILE: &str = "cron.remote.json";
/// 回执传出去这么久还没等到对方落定，原归属设备就把任务收回来。
const RECLAIM_AFTER_MS: i64 = 24 * 3600 * 1000;

/// `cron.transfer` 的四种记录。数值就是同一版里的先后：大的压过小的。
const REQUEST: u8 = 1;
const FORCE: u8 = 2;
const ACK: u8 = 3;
const DONE: u8 = 4;

fn is_job_id(id: &str) -> bool {
    ids::is_lower_hex(id, 8)
}

/// `rev` 是不是 `owner` 发出的一个 HLC。
fn is_rev_of(rev: &str, owner: &str) -> bool {
    sync_db::hlc_parts(rev).is_some() && rev.splitn(3, '-').nth(2) == Some(owner)
}

fn transfer(from: &str, id: &str, to: &str, rev: &str, rank: u8) -> Op {
    Op::CronTransfer {
        id: id.to_string(),
        from: from.to_string(),
        to: to.to_string(),
        rev: rev.to_string(),
        force: rank == FORCE,
        ack: rank == ACK,
        done: rank == DONE,
    }
}

/// 一条归属变更（`cron.transfer`）连同它的 HLC。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Xfer {
    id: String,
    to: String,
    /// 说的是任务的哪一版（原归属设备发布那份定义时的 HLC）。
    rev: String,
    rank: u8,
    hlc: String,
}

impl Xfer {
    /// `force`、`ack`、`done` 至多一个为真，否则不是一条合规的记录。
    fn of(op: &Op, hlc: &str) -> Option<Xfer> {
        let Op::CronTransfer {
            id,
            to,
            rev,
            force,
            ack,
            done,
            ..
        } = op
        else {
            return None;
        };
        let rank = match (*force, *ack, *done) {
            (false, false, false) => REQUEST,
            (true, false, false) => FORCE,
            (false, true, false) => ACK,
            (false, false, true) => DONE,
            _ => return None,
        };
        Some(Xfer {
            id: id.clone(),
            to: to.clone(),
            rev: rev.clone(),
            rank,
            hlc: hlc.to_string(),
        })
    }

    /// 同一个任务的两条记录谁生效。这是一个全序，结果与到达的先后无关：版本新的压过
    /// 旧的；同一版里落定 > 回执 > 强制接管 > 请求；同级的后发的压过先发的——只有落定
    /// 反过来，先落定的算数（接手的设备和收回任务的原设备各发了一条时，晚的那一边把
    /// 自己那份撤掉）。
    fn outranks(&self, current: &Xfer) -> bool {
        use std::cmp::Ordering::{Equal, Greater, Less};
        match (self.rev.cmp(&current.rev), self.rank.cmp(&current.rank)) {
            (Greater, _) | (Equal, Greater) => true,
            (Less, _) | (Equal, Less) => false,
            (Equal, Equal) if self.rank == DONE => self.hlc < current.hlc,
            (Equal, Equal) => self.hlc > current.hlc,
        }
    }
}

/// 一个任务当前生效的归属变更。任务定义、回执和落定来自不同日志，任何一个都可能晚到。
fn effective(conn: &Connection, key: &str) -> Result<Option<Xfer>> {
    let Some(rev) = put_rev(conn, key)? else {
        return Ok(None);
    };
    let Some((owner, _)) = key.split_once('/') else {
        return Ok(None);
    };
    let mut leading: Option<Xfer> = None;
    let mut done = Vec::new();
    for row in rows(conn, SCOPE_XFER, &format!("{key}/{rev}/"))? {
        let Some(xfer) = Xfer::of(&row.op, &row.hlc) else {
            continue;
        };
        let author = row.hlc.rsplit('-').next().unwrap_or_default();
        if !follows(conn, author, &row.origin)? {
            continue;
        }
        if xfer.rank == DONE {
            done.push(xfer);
        } else if leading
            .as_ref()
            .is_none_or(|current| xfer.outranks(current))
        {
            leading = Some(xfer);
        }
    }
    let receiver = leading.as_ref().filter(|xfer| xfer.rank == ACK);
    let mut winner: Option<Xfer> = None;
    for xfer in done {
        let authorized = follows(conn, owner, &xfer.to)?
            || match receiver {
                Some(ack) => follows(conn, &ack.to, &xfer.to)?,
                None => false,
            };
        if authorized && winner.as_ref().is_none_or(|current| xfer.outranks(current)) {
            winner = Some(xfer);
        }
    }
    Ok(winner.or(leading))
}

fn follows(conn: &Connection, original: &str, current: &str) -> Result<bool> {
    Ok(original == current
        || lineage(conn, original)?
            .iter()
            .any(|(device, _)| device == current))
}

/// 一个任务现在登记着的定义是哪一版；已经删除、或者没见过的为空。
fn put_rev(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(super::row(conn, scope::CRON, key)?
        .filter(|row| matches!(row.op, Op::CronPut { .. }))
        .map(|row| row.hlc))
}

/// 本机发一条归属变更，返回它的 HLC。压不过现在生效的那一条就不发，返回错误。**只能在
/// 调用方的写事务里用，出错时事务要整个回滚**——这时 op 已经排进了待发队列。
fn publish_xfer(conn: &Connection, s: &Settings, key: &str, op: &Op) -> Result<String> {
    let hlc = map::enqueue(conn, &s.device, None, op)?;
    let new = Xfer::of(op, &hlc).ok_or_else(|| SyncError::invalid("定时任务的归属变更不合规"))?;
    // 即使 HLC 比现有记录新，也可能因阶段较早而不能生效。
    if effective(conn, key)?.is_some_and(|current| !new.outranks(&current)) {
        return Err(SyncError::other(
            "这个任务的归属正在变动，本机还没有同步到最新的情况。请稍后再试",
        ));
    }
    let value = super::stored_text(&s.device, op)?;
    store_xfer(conn, key, &s.device, &new, Some(HANDLED), &value)?;
    Ok(hlc)
}

fn store_xfer(
    conn: &Connection,
    key: &str,
    origin: &str,
    xfer: &Xfer,
    mark: Option<&str>,
    stored: &str,
) -> Result<bool> {
    let key = format!("{key}/{}/{}/{origin}", xfer.rev, xfer.rank);
    if let Some(current) = db::state_get(conn, SCOPE_XFER, &key)? {
        let wins = if xfer.rank == DONE {
            xfer.hlc < current.hlc
        } else {
            xfer.hlc > current.hlc
        };
        if !wins {
            return Ok(false);
        }
    }
    db::state_put(conn, SCOPE_XFER, &key, &xfer.hlc, mark, Some(stored))?;
    Ok(true)
}

fn prune_transfers(conn: &Connection, key: &str, rev: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM sync_state WHERE scope = ?1 AND key >= ?2 AND key < ?3",
        params![SCOPE_XFER, format!("{key}/"), format!("{key}/{rev}/")],
    )?;
    Ok(())
}

/// 合入时记一条定时任务 op。任务定义只查外壳，不在这里解析：别的设备用了本机版本不认识
/// 的调度方式时，照样能在镜像里看到，只是不能接手。
pub(crate) fn record(
    conn: &Connection,
    origin: &str,
    hlc: &str,
    op: &Op,
    stored: &str,
) -> Result<bool> {
    match op {
        Op::CronPut {
            owner,
            id,
            job,
            target,
        } => {
            // 任务只能由归属设备自己发布。
            if owner != origin || !is_rev_of(hlc, origin) || !is_job_id(id) {
                return Err(SyncError::invalid("定时任务的归属或标识不合规"));
            }
            if target.as_deref().is_some_and(|sid| !ids::is_sid(sid)) {
                return Err(SyncError::invalid("定时任务的投递目标不合规"));
            }
            if job.get("id").and_then(|v| v.as_str()) != Some(id.as_str())
                || job.to_string().len() > JOB_MAX
            {
                return Err(SyncError::invalid("定时任务的定义不合规"));
            }
            let key = format!("{owner}/{id}");
            let changed = put_remote(conn, scope::CRON, &key, hlc, stored)?;
            if changed {
                prune_transfers(conn, &key, hlc)?;
            }
            Ok(changed)
        }
        Op::CronDel { owner, id } => {
            if owner != origin || !is_rev_of(hlc, origin) || !is_job_id(id) {
                return Err(SyncError::invalid("定时任务的归属或标识不合规"));
            }
            let key = format!("{owner}/{id}");
            let changed = put_remote(conn, scope::CRON, &key, hlc, stored)?;
            if changed {
                prune_transfers(conn, &key, hlc)?;
            }
            Ok(changed)
        }
        Op::CronTransfer {
            id, from, to, rev, ..
        } => {
            let new = Xfer::of(op, hlc)
                .filter(|new| {
                    is_job_id(id)
                        && ids::is_device_id(from)
                        && ids::is_device_id(to)
                        && is_rev_of(rev, from)
                        // 指向原归属设备自己的只能是落定：它把任务收了回去。
                        && (from != to || new.rank == DONE)
                })
                .ok_or_else(|| SyncError::invalid("定时任务的归属变更不合规"))?;
            // 回执只能由原归属设备发；请求、强制接管、落定只能由 `to` 自己发。
            let sender = if new.rank == ACK { from } else { to };
            if origin != sender.as_str() {
                return Err(SyncError::invalid("定时任务的归属变更不是当事设备发的"));
            }
            let author = hlc
                .rsplit('-')
                .next()
                .filter(|author| is_rev_of(hlc, author));
            let Some(author) = author else {
                return Err(SyncError::invalid("定时任务的归属变更时钟不合规"));
            };
            // 回执不能换身份重发。其他记录的替换声明可能晚到，由 effective 核对。
            if (new.rank == ACK && author != origin) || hlc <= rev.as_str() {
                return Err(SyncError::invalid("定时任务的归属变更时钟不合规"));
            }
            let key = format!("{from}/{id}");
            if db::state_get(conn, scope::CRON, &key)?.is_some_and(|row| row.hlc > *rev) {
                return Ok(false);
            }
            store_xfer(conn, &key, origin, &new, None, stored)
        }
        _ => Ok(false),
    }
}

/// 任务定义的线上形态：去掉本机的触发状态。
fn canonical(job: &CronJob) -> serde_json::Value {
    let mut value = serde_json::to_value(job).unwrap_or_default();
    if let Some(fields) = value.as_object_mut() {
        fields.remove("last_run");
        fields.remove("next_run");
    }
    value
}

/// 判断「是不是同一个任务」用的形态：再去掉各端不同的 id 和启用状态。
fn essence(job: &CronJob) -> serde_json::Value {
    let mut value = canonical(job);
    if let Some(fields) = value.as_object_mut() {
        fields.remove("id");
        fields.remove("enabled");
    }
    value
}

/// 一个本机任务发布出去的样子：`(定义, 投递目标的线程, 摘要)`。投递目标换成线程 id——
/// 会话键各端可能不同（§6）；目标会话不参与同步时为空。
fn wire_form(
    conn: &Connection,
    job: &CronJob,
) -> Result<(serde_json::Value, Option<String>, String)> {
    let target =
        map::session_map_by_key(conn, &job.channel, &job.chat_id)?.map(|session| session.sid);
    let value = canonical(job);
    let hash = sha256_hex(
        serde_json::json!({ "job": value, "target": target })
            .to_string()
            .as_bytes(),
    );
    Ok((value, target, hash))
}

/// 任务表里和 `job` 是同一个任务的那些。
fn twins(jobs: &[CronJob], job: &CronJob) -> Vec<String> {
    let wanted = essence(job);
    jobs.iter()
        .filter(|local| essence(local) == wanted)
        .map(|local| local.id.clone())
        .collect()
}

/// 上一次往任务表里加 `job`、加进去了却没来得及记账（进程中断）的那一个：和 `job` 是同
/// 一个任务，又不在动手之前就有的那些（`before`）里面。同步只认自己加进去的任务——用户
/// 本来就有一个一模一样的，不会被当成这一份，以后也就不会被同步移除。
fn orphan(jobs: &[CronJob], job: &CronJob, before: &[String]) -> Option<String> {
    twins(jobs, job).into_iter().find(|id| !before.contains(id))
}

/// 提示里怎么称呼一个任务。
fn title_of(job: &serde_json::Value) -> String {
    match job.get("name").and_then(|name| name.as_str()) {
        Some(name) if !name.trim().is_empty() => format!("定时任务「{name}」"),
        _ => "有一个定时任务".to_string(),
    }
}

/// 一条接手请求。`job` + `target` 是用户点「移到本机执行」时看到的那份任务定义，落到本机
/// 任务表里的只能是它——用户没看过、没同意过的东西不能在本机无人值守地跑起来。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Claim {
    job: serde_json::Value,
    #[serde(default)]
    target: Option<String>,
    /// 那份定义的版本。
    rev: String,
    /// 已经动手往任务表里加了。
    #[serde(default)]
    attempt: Option<Attempt>,
    /// 落地之后它在本机任务表里的 id。
    #[serde(default)]
    local: Option<String>,
    /// 落过地、又撤回了（被另一台设备的强制接管压过）。
    #[serde(default)]
    withdrawn: bool,
}

/// 往任务表里加之前先记下的东西：中途中断之后重来，靠它认出上次加进去的那一个
/// （[`orphan`]），不加第二遍。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Attempt {
    /// 要加的那一份（投递目标已经换成本机上的会话）。
    job: CronJob,
    /// 动手之前任务表里已有的同样的任务。
    before: Vec<String>,
}

/// 本机交出去的一个任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Parked {
    /// 它在任务表里的完整定义。
    job: serde_json::Value,
    /// 交给了谁。
    to: String,
    /// 交出去的是哪一版。
    rev: String,
    /// 本机那条回执的 HLC；空 = 还没发（已经从任务表里移除，还没来得及发）。
    #[serde(default)]
    ack: String,
    /// 看到回执已经传到存储上的时刻；0 = 还没传出去。收回之前的等待从这时算起：本机写下
    /// 回执就断了网的话，对方还没有机会看到它。
    #[serde(default)]
    sent_ms: i64,
    /// 不用再等一天，能确认对方没有落定就收回：本机换了身份（[`on_rotate`]），那条回执
    /// 对方可能永远等不到。
    #[serde(default)]
    urgent: bool,
    /// 已经告诉过用户「没法确认对方有没有接手」。
    #[serde(default)]
    warned: bool,
    /// 动手放回任务表之前，任务表里已有的同样的任务（见 [`Attempt`]）。
    #[serde(default)]
    before: Option<Vec<String>>,
    /// 收回之后它在任务表里的 id。
    #[serde(default)]
    restored: Option<String>,
}

/// 别的设备上的一个定时任务（界面与镜像文件用）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RemoteJob {
    /// 归属设备。
    pub owner: String,
    pub owner_name: String,
    pub id: String,
    /// 任务定义（`CronJob` 的 JSON，不含触发状态）。
    pub job: serde_json::Value,
    /// 这份定义的版本（归属设备发布它时的 HLC）。请求接手时原样交回来：用户看到的和要
    /// 接手的得是同一版。
    pub version: String,
    /// 投递目标的线程（界面用不上）。
    #[serde(skip)]
    target: Option<String>,
    /// 不能移到本机执行的原因；空 = 可以。
    pub blocked: Option<String>,
    /// 本机已经请求接手，在等那台设备确认。
    pub pending: bool,
    /// 那台设备很久没出现、或被标为已丢失：可以不等它确认，强制接管。
    pub can_force: bool,
}

/// 一个远端任务为什么不能移到本机；`None` = 可以。
fn blocked(
    conn: &Connection,
    job: &serde_json::Value,
    target: Option<&str>,
) -> Result<Option<String>> {
    let Ok(parsed) = serde_json::from_value::<CronJob>(job.clone()) else {
        return Ok(Some(
            "这个任务用了本机版本还不认识的调度方式，升级之后才能移到本机".into(),
        ));
    };
    if !parsed.enabled {
        return Ok(Some("这个任务已经停用".into()));
    }
    Ok(match target {
        Some(sid) if map::thread_sessions(conn, sid)?.is_empty() => {
            Some("它的投递目标会话还没有同步到这台设备".into())
        }
        None if parsed.channel != rclaw_core::CHANNEL_SYSTEM => {
            Some("它的投递目标不在同步范围内，只能在这里查看".into())
        }
        _ => None,
    })
}

/// 用户把一台设备标为「已丢失」，或取消标记。
pub fn mark_lost(conn: &Connection, device: &str, lost: bool) -> Result<()> {
    if !ids::is_device_id(device) {
        return Err(SyncError::invalid("设备标识不合规"));
    }
    if lost {
        db::state_put(conn, SCOPE_LOST, device, db::HLC_ZERO, None, Some("1"))
    } else {
        db::state_del(conn, SCOPE_LOST, device)
    }
}

pub fn is_lost(conn: &Connection, device: &str) -> Result<bool> {
    Ok(db::state_get(conn, SCOPE_LOST, device)?.is_some())
}

/// 用户标为「已丢失」的设备。
pub fn lost_devices(conn: &Connection) -> Result<Vec<String>> {
    Ok(db::state_scan(conn, SCOPE_LOST, "")?
        .into_iter()
        .map(|(device, _)| device)
        .collect())
}

/// 本机换了身份（§9.6），在换身份的事务里调用。`past` 是本机用过的全部旧身份（包括刚换
/// 掉的那个），`new_id` 是新身份。定时任务的 op 只认当事设备自己发的，换了发送者就不能
/// 照搬到新身份的日志里，各自这样处理：
///
/// - 本机的任务：忘掉「上次发布的版本」，下一轮对比时以新身份为归属重新发布。
/// - 待发队列里还没封段的定时任务 op：作废（别的设备不会认）。
/// - 本机发过、现在仍然生效的请求、强制接管、落定（这三种由 `to` 自己发）：改成指向新
///   身份，带着原来的 HLC 再发一遍——旧身份名下的那一条可能没传出去。收到过原件的设备
///   分不出两者的先后，不受影响；没收到过的补上这一条。各端算出的归属仍然一致。
/// - 本机发过的回执没法这样重发（回执只认原归属设备，也就是旧身份）：对方可能永远等不
///   到它。交出去还没落定的任务不再等一天，下一轮对比就收回（留底里记上 `urgent`）。
pub(crate) fn on_rotate(conn: &Connection, past: &[String], new_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM sync_state WHERE scope = ?1",
        params![SCOPE_BASE],
    )?;
    // 待发队列里的 op 都是本机序列化的，类型标签 `t` 总是第一个字段：看开头就知道是不是
    // 定时任务的 op，不用把每一条（技能的清单可能有几兆字节）读出来解析。
    conn.execute(
        "DELETE FROM sync_outbox WHERE session_id IS NULL AND substr(op, 1, 11) = '{\"t\":\"cron.'",
        [],
    )?;
    for row in rows(conn, scope::CRON, "")? {
        let Op::CronPut { owner, .. } = row.op else {
            continue;
        };
        let Some(xfer) = effective(conn, &row.key)? else {
            continue;
        };
        if xfer.rank == ACK || !past.contains(&xfer.to) {
            continue;
        }
        let op = transfer(&owner, &xfer.id, new_id, &xfer.rev, xfer.rank);
        conn.execute(
            "INSERT INTO sync_outbox(session_id, hlc, op) VALUES (NULL, ?1, ?2)",
            params![xfer.hlc, serde_json::to_string(&op)?],
        )?;
    }
    for (key, row) in db::state_scan(conn, SCOPE_PARKED, "")? {
        let Some(mut parked) = row.json::<Parked>() else {
            continue;
        };
        if parked.restored.is_none() && !parked.urgent {
            parked.urgent = true;
            db::state_put_json(conn, SCOPE_PARKED, &key, db::HLC_ZERO, &parked)?;
        }
    }
    Ok(())
}

/// 一台设备，连同它后来换成的身份（§9.6），依次排开。
fn lineage(conn: &Connection, device: &str) -> Result<Vec<(String, PeerState)>> {
    let mut out: Vec<(String, PeerState)> = Vec::new();
    let mut next = Some(device.to_string());
    while let Some(device) = next {
        if out.len() >= 8 || out.iter().any(|(seen, _)| *seen == device) {
            break;
        }
        let peer = db::peer_state(conn, &device)?;
        next = peer.replaced_by.clone();
        out.push((device, peer));
    }
    Ok(out)
}

/// 本机对一台设备（连同它换成的新身份）的日志是不是读到了头。读不下去、或者取回来的
/// 还没处理完的时候，它的落定记录可能已经发出来了，只是本机还没看到。
fn caught_up(conn: &Connection, device: &str) -> Result<bool> {
    Ok(lineage(conn, device)?.iter().all(|(_, peer)| {
        !peer.pull_incomplete
            && !peer.from_snap
            && peer.error_since_ms == 0
            && peer.halted.is_none()
            && peer.processed >= peer.fetched
    }))
}

/// 一台设备（或者它后来换成的身份）是不是被用户标为了「已丢失」。
fn gone(conn: &Connection, device: &str) -> Result<bool> {
    for (device, _) in lineage(conn, device)? {
        if is_lost(conn, &device)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 强制接管后可以启用任务了吗：用户标了「已丢失」，或者归属设备已经陈旧（§9.5，很久
/// 没再出现）。界面对这两种设备都提供「强制接管」；这条准入与调度器的启用用同一个判定
/// ——接管之后就该启用，不然用户看到的操作与结果不一致，任务没人执行。
fn takeover_ready(conn: &Connection, owner: &str, now: i64) -> Result<bool> {
    if gone(conn, owner)? {
        return Ok(true);
    }
    let peer = db::peer_state(conn, owner)?;
    Ok(peer.seen_ms > 0 && !peer.is_active(now) && peer.replaced_by.is_none())
}

/// 本机发的一条 op 是不是已经传到存储上了：不在待发队列里，也没有封好还没传的段。
fn is_shipped(conn: &Connection, hlc: &str) -> Result<bool> {
    let queued: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sync_outbox WHERE hlc = ?1)",
        params![hlc],
        |row| row.get(0),
    )?;
    Ok(!queued && db::log_state(conn)?.pending.is_none())
}

fn claim_of(conn: &Connection, key: &str) -> Result<Option<Claim>> {
    Ok(db::state_get(conn, SCOPE_CLAIM, key)?.and_then(|row| row.json::<Claim>()))
}

fn save_claim(ctx: &Ctx, key: &str, claim: &Claim) -> Result<()> {
    db::write(ctx.db(), |tx| {
        db::state_put_json(tx, SCOPE_CLAIM, key, db::HLC_ZERO, claim)
    })
}

fn drop_claim(ctx: &Ctx, key: &str) -> Result<()> {
    db::write(ctx.db(), |tx| db::state_del(tx, SCOPE_CLAIM, key))
}

fn save_parked(ctx: &Ctx, key: &str, parked: &Parked) -> Result<()> {
    db::write(ctx.db(), |tx| {
        db::state_put_json(tx, SCOPE_PARKED, key, db::HLC_ZERO, parked)
    })
}

fn drop_parked(ctx: &Ctx, key: &str) -> Result<()> {
    db::write(ctx.db(), |tx| db::state_del(tx, SCOPE_PARKED, key))
}

/// 外部副作用（改任务表 `cron.json`）之前，把刚提交的交接记录推到盘上。SQLite 用
/// `synchronous=NORMAL`，掉电可能回滚最近的提交：任务已经从任务表里移除（或加进去）而
/// 记录丢了的话，任务的定义就两头都没有（或者同一交接加出两份）。先耐久、再动手，
/// 中途断了才总有一份记录认得回已经动过的任务表。
fn durable(ctx: &Ctx) -> Result<()> {
    ctx.db().flush()?;
    Ok(())
}

/// 别的设备上的定时任务：归属本机的、归属已经变了的、归属设备已换了新身份的都不算。
pub fn remote_jobs(conn: &Connection, s: &Settings, now: i64) -> Result<Vec<RemoteJob>> {
    let mut out = Vec::new();
    // 每台归属设备查一次：`(展示名, 能否强制接管)`；已换了新身份的记 `None`。
    let mut owners: BTreeMap<String, Option<(String, bool)>> = BTreeMap::new();
    for row in rows(conn, scope::CRON, "")? {
        let Op::CronPut {
            owner,
            id,
            job,
            target,
        } = row.op
        else {
            continue;
        };
        if s.is_me(&owner) {
            continue;
        }
        let key = format!("{owner}/{id}");
        let claim = claim_of(conn, &key)?;
        if let Some(xfer) = effective(conn, &key)? {
            // 归属已经变了（被强制接管、有了回执或落定）。只算针对现在这一版的记录：任务
            // 改过之后，旧版的记录都作废了。
            let moved = xfer.rev == row.hlc && xfer.rank >= FORCE;
            // 例外：归属正要变给本机，本机却没有在接手（上次没能加进任务表）——照样列
            // 着，用户可以再试一次。
            let retry = s.is_me(&xfer.to) && xfer.rank <= ACK && claim.is_none();
            if moved && !retry {
                continue;
            }
        }
        if !owners.contains_key(&owner) {
            let peer = db::peer_state(conn, &owner)?;
            let info = match peer.replaced_by {
                Some(_) => None,
                None => {
                    // 陈旧：见过它，但已经很久没再出现（§9.5）。
                    let stale = peer.seen_ms > 0 && !peer.is_active(now);
                    Some((
                        db::device_label(conn, &owner)?,
                        stale || is_lost(conn, &owner)?,
                    ))
                }
            };
            owners.insert(owner.clone(), info);
        }
        let Some(Some((owner_name, can_force))) = owners.get(&owner).cloned() else {
            continue;
        };
        out.push(RemoteJob {
            blocked: blocked(conn, &job, target.as_deref())?,
            pending: claim.is_some_and(|claim| claim.rev == row.hlc),
            version: row.hlc,
            owner,
            owner_name,
            id,
            job,
            target,
            can_force,
        });
    }
    out.sort_by(|a, b| (&a.owner_name, &a.owner, &a.id).cmp(&(&b.owner_name, &b.owner, &b.id)));
    Ok(out)
}

/// 用户在本机发起：把别的设备上的一个任务移到本机执行。`force` = 不等那台设备确认（只在
/// 它陈旧或被标为已丢失时允许）。`reviewed` 是用户看到的那份定义的版本
/// （[`RemoteJob::version`]）：和现在登记着的不是同一版就不办，让用户先看新的。之后由下
/// 一轮对比完成。
pub fn request_takeover(
    ctx: &Ctx,
    s: &Settings,
    owner: &str,
    id: &str,
    force: bool,
    reviewed: &str,
) -> Result<()> {
    if ctx.cron.is_none() {
        return Err(SyncError::invalid("这台设备上没有运行定时任务调度器"));
    }
    // 新的请求只在这个范围开着时受理（关着的时候镜像不更新，用户看到的是过时的列表）。
    if !s.scopes.cron {
        return Err(SyncError::invalid(
            "这台设备没有在同步定时任务；先在「多端同步」的同步范围里打开它",
        ));
    }
    let now = now_ms();
    db::write(ctx.db(), |tx| {
        let jobs = remote_jobs(tx, s, now)?;
        let Some(job) = jobs.iter().find(|job| job.owner == owner && job.id == id) else {
            return Err(SyncError::invalid("这个任务已经不在那台设备上了"));
        };
        if job.version != reviewed {
            return Err(SyncError::other(
                "这个任务刚刚在那台设备上改过。请看一下它现在的内容，再决定要不要移到本机",
            ));
        }
        if let Some(reason) = &job.blocked {
            return Err(SyncError::invalid(reason.clone()));
        }
        let key = format!("{owner}/{id}");
        // 上一次接手留下的东西还在本机任务表里（等着下一轮对比撤掉）。
        if claim_of(tx, &key)?.is_some_and(|claim| claim.local.is_some() || claim.attempt.is_some())
        {
            return Err(SyncError::other(
                "本机上一次接手它的事还没有处理完，请稍后再试",
            ));
        }
        // 这一版已经定给本机了（上次没能加进任务表）：不用再发请求，下一轮对比接着落地。
        let granted = effective(tx, &key)?
            .is_some_and(|xfer| xfer.rev == job.version && s.is_me(&xfer.to) && xfer.rank >= FORCE);
        if !granted {
            if force && !job.can_force {
                return Err(SyncError::invalid(
                    "那台设备最近还在线，不能强制接管；请用「移到本机执行」，等它确认",
                ));
            }
            let rank = if force { FORCE } else { REQUEST };
            publish_xfer(
                tx,
                s,
                &key,
                &transfer(owner, id, &s.device, &job.version, rank),
            )?;
        }
        let claim = Claim {
            job: job.job.clone(),
            target: job.target.clone(),
            rev: job.version.clone(),
            attempt: None,
            local: None,
            withdrawn: false,
        };
        db::state_put_json(tx, SCOPE_CLAIM, &key, db::HLC_ZERO, &claim)
    })
}

// ---------------------------------------------------------------------------
// 原归属设备这一边
// ---------------------------------------------------------------------------

/// 归属本机的任务被别的设备要走（或强制接管）了：从本机任务表里移除，发回执。任务的定义
/// 留一份（[`Parked`]），对方迟迟不落定时靠它收回（[`settle_parked`]）。
fn hand_over(ctx: &Ctx, s: &Settings, host: &dyn CronHost, report: &mut FilesReport) -> Result<()> {
    let prefix = format!("{}/", s.device);
    let jobs = host.jobs();
    // `(请求, 任务表里的那个任务)`。任务为空 = 上次已经把它从任务表里移除，没来得及发回执。
    let requests: Vec<(Xfer, Option<CronJob>)> = db::read(ctx.db(), |conn| {
        let mut out = Vec::new();
        for row in rows(conn, scope::CRON, &prefix)? {
            let Some(xfer) = effective(conn, &row.key)? else {
                continue;
            };
            if xfer.rank > FORCE || s.is_me(&xfer.to) {
                continue;
            }
            // 只交出本机发布过、没有发布过删除的任务，而且对方看到的得是现在登记着的这一版。
            let Some(base) = db::state_get(conn, SCOPE_BASE, &xfer.id)?.and_then(|row| row.value)
            else {
                continue;
            };
            match jobs.iter().find(|job| job.id == xfer.id) {
                // 停用的不交。本机有还没发布的改动也不交：这一轮稍后会把它发布出去，对方
                // 看到新内容之后再决定。
                Some(job) => {
                    if job.enabled && wire_form(conn, job)?.2 == base {
                        out.push((xfer, Some(job.clone())));
                    }
                }
                // 任务表里没有、也没留底：用户自己删的，[`publish_local`] 会发删除。
                None => {
                    if db::state_get(conn, SCOPE_PARKED, &row.key)?.is_some() {
                        out.push((xfer, None));
                    }
                }
            }
        }
        Ok(out)
    })?;
    for (xfer, job) in requests {
        let key = format!("{prefix}{}", xfer.id);
        let mut parked = match job {
            Some(job) => {
                let parked = Parked {
                    job: serde_json::to_value(&job)?,
                    to: xfer.to.clone(),
                    rev: xfer.rev.clone(),
                    ack: String::new(),
                    sent_ms: 0,
                    urgent: false,
                    warned: false,
                    before: None,
                    restored: None,
                };
                // 先留底（并推到盘上），再从任务表里移除：中途中断的话，任务的定义不会
                // 两头都没有。
                save_parked(ctx, &key, &parked)?;
                durable(ctx)?;
                match host.remove(&xfer.id) {
                    Ok(true) => parked,
                    // 没移除成：任务还在任务表里（或者刚被用户删了），留底用不着。
                    kept => {
                        drop_parked(ctx, &key)?;
                        if let Err(reason) = kept {
                            tracing::warn!("移交定时任务 {} 失败: {reason}", xfer.id);
                            report.retry = true;
                        }
                        continue;
                    }
                }
            }
            None => {
                let Some(parked) = db::read(ctx.db(), |conn| {
                    Ok(db::state_get(conn, SCOPE_PARKED, &key)?
                        .and_then(|row| row.json::<Parked>()))
                })?
                else {
                    continue;
                };
                Parked {
                    to: xfer.to.clone(),
                    rev: xfer.rev.clone(),
                    ..parked
                }
            }
        };
        db::write(ctx.db(), |tx| {
            // 不发 `cron.del`：任务的定义要留在寄存器里，这一版归了谁靠它对得上。
            db::state_del(tx, SCOPE_BASE, &xfer.id)?;
            let ack = transfer(&s.device, &xfer.id, &xfer.to, &xfer.rev, ACK);
            parked.ack = publish_xfer(tx, s, &key, &ack)?;
            parked.sent_ms = 0;
            db::state_put_json(tx, SCOPE_PARKED, &key, db::HLC_ZERO, &parked)
        })?;
        report.published += 1;
        report.reload.insert(Reload::Cron);
    }
    Ok(())
}

/// 一个交出去的任务这一轮要怎么办。
enum Fate {
    /// 接着等。
    Keep,
    /// 回执已经传出去了：等待的时间从现在算起。
    Sent,
    /// 没有人要接手了（请求作废了、记录没了）：悄悄放回任务表。
    Unpark,
    /// 等不到对方落定：收回，本机发落定。
    Reclaim,
    /// 该收回了，但对方的日志本机读不到头：它也许已经落定了，只是本机看不到。不收回
    /// （收回了就可能两边各跑一遍），告诉用户。
    Stuck,
    /// 对方落定了：任务归它。
    Taken,
    /// 本机的收回算数。
    Mine,
}

fn fate(conn: &Connection, s: &Settings, key: &str, parked: &Parked, now: i64) -> Result<Fate> {
    let Some(xfer) = effective(conn, key)?.filter(|xfer| xfer.rev == parked.rev) else {
        return Ok(Fate::Unpark);
    };
    Ok(match xfer.rank {
        DONE if s.is_me(&xfer.to) => Fate::Mine,
        DONE => Fate::Taken,
        // 用户说那台设备已经不在了：不用等，也不用管它的日志读没读完。
        ACK if gone(conn, &xfer.to)? => Fate::Reclaim,
        ACK if !parked.urgent && parked.sent_ms == 0 => {
            if is_shipped(conn, &parked.ack)? {
                Fate::Sent
            } else {
                Fate::Keep
            }
        }
        ACK if !parked.urgent && now - parked.sent_ms < RECLAIM_AFTER_MS => Fate::Keep,
        // 本机换过身份：那条回执对方可能永远读不到，等不到它接手，只能按日志读到头推断
        // （对方的接手在没有落定回执前是停用的，收回了也不会两边同时执行）。
        ACK if parked.urgent => {
            if caught_up(conn, &xfer.to)? {
                Fate::Reclaim
            } else {
                Fate::Stuck
            }
        }
        // 回执传出去一天还等不到落定：不再自动收回——「读到头」只能证明读完了它已发布
        // 的日志，它的落定可能写好了还没传上来；收回了就可能两边各跑一遍。提醒用户去
        // 处理：把那台设备标为「已丢失」，或者到对方上把接手办完（办不成了它会归还）。
        ACK => Fate::Stuck,
        _ => Fate::Unpark,
    })
}

/// 本机放回任务表的那一份（还在的话）：记过账的那个，或者加进去了、没来得及记账的那个。
fn restored(jobs: &[CronJob], parked: &Parked) -> Option<String> {
    match &parked.restored {
        Some(local) => jobs
            .iter()
            .any(|job| job.id == *local)
            .then(|| local.clone()),
        None => {
            let job: CronJob = serde_json::from_value(parked.job.clone()).ok()?;
            orphan(jobs, &job, parked.before.as_deref()?)
        }
    }
}

/// 把交出去的任务放回本机任务表，返回它在任务表里的 id。`disabled` = 交接还没有得到
/// 对方确认（退出 / 重置时看不到它落没落定），放回来也要停用——对方其实已经接手的话，
/// 启用的两份会同时跑。外层的错误是库出了问题，内层的是任务表不收（原因给用户看）。
fn put_back(
    ctx: &Ctx,
    host: &dyn CronHost,
    jobs: &[CronJob],
    key: &str,
    parked: &mut Parked,
    disabled: bool,
) -> Result<std::result::Result<String, String>> {
    // 它压根没离开过任务表（留了底、还没移除就中断了）。
    let original = parked
        .job
        .get("id")
        .and_then(|id| id.as_str())
        .unwrap_or_default();
    if jobs.iter().any(|job| job.id == original) {
        return Ok(Ok(original.to_string()));
    }
    let Ok(job) = serde_json::from_value::<CronJob>(parked.job.clone()) else {
        return Ok(Err("任务的定义读不出来了".into()));
    };
    if parked.before.is_none() {
        parked.before = Some(twins(jobs, &job));
        save_parked(ctx, key, parked)?;
        // 加进任务表之前把「已有同样的任务」的底推到盘上：掉电丢了它，认不出上次加过的
        // 那一份，就会加出第二份。
        durable(ctx)?;
    }
    let before = parked.before.as_deref().unwrap_or_default();
    Ok(match orphan(jobs, &job, before) {
        Some(local) => Ok(local),
        None => {
            // 动任务表之前再推一次落盘点：上次可能提交成功、flush 却失败了（库还能读出
            // 这条记录，但掉电会丢）。已有意图也不能豁免——只有确认这份记录真的耐久了，
            // 才允许加进任务表；认回已存在的任务（上面的 orphan 分支）不新增，不用重推。
            durable(ctx)?;
            if disabled {
                host.add_disabled(job)
            } else {
                host.add(job)
            }
        }
    })
}

/// 把一个任务的定义另存到 `<数据根>/sync/conflicts/cron/`，返回路径。
fn save_definition(ctx: &Ctx, id: &str, text: &str) -> PathBuf {
    let file = format!("{id}.{}.json", stamp_now());
    keep_copy(ctx, "cron", &file, text.as_bytes());
    ctx.paths.conflicts.join("cron").join(file)
}

/// 交出去的任务放不回本机的任务表：定义另存一份，告诉用户。
fn keep_parked(ctx: &Ctx, key: &str, id: &str, parked: &Parked, reason: &str) {
    let text = serde_json::to_string_pretty(&parked.job).unwrap_or_else(|_| parked.job.to_string());
    let path = save_definition(ctx, id, &text);
    status::notice(
        &ctx.paths,
        "cron-reclaim-failed",
        key,
        &format!("{}没能放回本机的任务表", title_of(&parked.job)),
        &format!(
            "{reason}。这个任务之前从本机交给了另一台设备，但没有被接手。它的定义保存在 {}，\
             需要的话请照着重新创建。",
            path.display()
        ),
    );
}

/// 交出去的任务后来怎么样了：对方落定了就了结；请求作废了就放回任务表；回执传出去一天
/// 还等不到落定（或者对方被标为已丢失）就收回。
fn settle_parked(
    ctx: &Ctx,
    s: &Settings,
    host: &dyn CronHost,
    report: &mut FilesReport,
) -> Result<()> {
    let now = now_ms();
    let found = db::read(ctx.db(), |conn| {
        let mut out = Vec::new();
        for (key, row) in db::state_scan(conn, SCOPE_PARKED, "")? {
            let parked: Option<Parked> = row.json();
            let fate = match &parked {
                Some(parked) => fate(conn, s, &key, parked, now)?,
                None => Fate::Keep,
            };
            out.push((key, row.value, parked, fate));
        }
        Ok(out)
    })?;
    for (key, raw, parked, fate) in found {
        let (owner, id) = key.split_once('/').unwrap_or(("", key.as_str()));
        let Some(mut parked) = parked else {
            // 读不懂（不该出现）：原文另存一份，别让任务的定义无声无息地没了。
            let path = save_definition(ctx, id, raw.as_deref().unwrap_or_default());
            tracing::warn!(
                "交出去的定时任务 {key} 的记录读不懂，原文另存在 {}",
                path.display()
            );
            drop_parked(ctx, &key)?;
            continue;
        };
        let jobs = host.jobs();
        let back = restored(&jobs, &parked);
        // 有了结果：「没法确认」的提示用不着了。
        if parked.warned && !matches!(fate, Fate::Keep | Fate::Sent | Fate::Stuck) {
            report.status_changed |=
                status::clear_notice(&ctx.paths, "cron-handover-unconfirmed", &key);
            parked.warned = false;
        }
        match fate {
            Fate::Keep => {}
            Fate::Sent => {
                parked.sent_ms = now;
                save_parked(ctx, &key, &parked)?;
            }
            Fate::Stuck => {
                // 只说一次：用户关掉提示之后不再弹出来。
                if !parked.warned {
                    let taker = db::read(ctx.db(), |conn| db::device_label(conn, &parked.to))?;
                    status::notice(
                        &ctx.paths,
                        "cron-handover-unconfirmed",
                        &key,
                        &format!(
                            "{}交给了「{taker}」，但一直没能确认它有没有接手",
                            title_of(&parked.job)
                        ),
                        &format!(
                            "本机已经按「{taker}」的请求把这个任务交了出去，一直没有等到它确认\
                             接手。为了不冒「两边各跑一遍」的险，本机不再按时限自动收回：它的\
                             接手在没有落定回执之前是停用的，这个任务现在没有任何设备在执行。\
                             请二选一：到那台设备上看一眼，任务不在那里的话让它重新发起接手，\
                             或者就在本机把那台设备标为「已丢失」，本机会马上把任务收回来。"
                        ),
                    );
                    parked.warned = true;
                    save_parked(ctx, &key, &parked)?;
                    report.status_changed = true;
                }
            }
            Fate::Mine => {
                // 收回的那份已经不在任务表里了（用户删了，或者又交给了别的设备）：了结。
                if back.is_none() {
                    drop_parked(ctx, &key)?;
                }
            }
            Fate::Taken => {
                // 本机收回过，但对方落定得更早：收回的那份撤掉。
                if let Some(local) = &back {
                    if let Err(reason) = host.remove(local) {
                        tracing::warn!("撤掉收回的定时任务 {local} 失败: {reason}");
                        report.retry = true;
                        continue;
                    }
                    status::notice(
                        &ctx.paths,
                        "cron-reclaim-undone",
                        &key,
                        &format!(
                            "{}已经由另一台设备接手，本机收回的那一份已经移除",
                            title_of(&parked.job)
                        ),
                        "本机之前没有等到对方确认接手，把这个任务收了回来；现在看到对方其实先一步\
                         接手了。为了不让同一个任务在两台设备上各跑一遍，本机这一份撤掉了。",
                    );
                    report.status_changed = true;
                    report.reload.insert(Reload::Cron);
                } else {
                    let taker = db::read(ctx.db(), |conn| db::device_label(conn, &parked.to))?;
                    status::notice(
                        &ctx.paths,
                        "cron-handover-complete",
                        &key,
                        &format!("{}已移交给「{taker}」", title_of(&parked.job)),
                        "接手设备已经确认接手，这个任务在那台设备上执行。",
                    );
                    report.status_changed = true;
                }
                drop_parked(ctx, &key)?;
            }
            Fate::Unpark => {
                if back.is_none() {
                    // 没人要接手了（请求作废、记录没了）：对方不可能落定，放回即启用。
                    match put_back(ctx, host, &jobs, &key, &mut parked, false)? {
                        Ok(_) => {
                            report.reload.insert(Reload::Cron);
                        }
                        Err(reason) => {
                            keep_parked(ctx, &key, id, &parked, &reason);
                            report.status_changed = true;
                        }
                    }
                }
                drop_parked(ctx, &key)?;
            }
            Fate::Reclaim => match put_back(ctx, host, &jobs, &key, &mut parked, false)? {
                Ok(local) => {
                    parked.restored = Some(local);
                    // 换过身份的话，落定以本机现在的身份发。
                    let done = transfer(owner, id, &s.device, &parked.rev, DONE);
                    let taker = db::write(ctx.db(), |tx| {
                        db::state_put_json(tx, SCOPE_PARKED, &key, db::HLC_ZERO, &parked)?;
                        publish_xfer(tx, s, &key, &done)?;
                        db::device_label(tx, &parked.to)
                    })?;
                    status::notice(
                        &ctx.paths,
                        "cron-reclaimed",
                        &key,
                        &format!("{}没有被接手，已经收回本机继续执行", title_of(&parked.job)),
                        &format!(
                            "「{taker}」请求把它移过去，本机已经把它交了出去，但一直没有等到它确认\
                             接手（等了一天，或者它已经被标为丢失）。为了不让这个任务没有设备执行，\
                             本机把它收了回来。"
                        ),
                    );
                    report.published += 1;
                    report.status_changed = true;
                    report.reload.insert(Reload::Cron);
                }
                Err(reason) => {
                    keep_parked(ctx, &key, id, &parked, &reason);
                    drop_parked(ctx, &key)?;
                    report.status_changed = true;
                }
            },
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 接手的设备这一边
// ---------------------------------------------------------------------------

/// 一条接手请求这一轮要怎么办。
enum Step {
    /// 接着等。
    Wait,
    /// 把任务加进本机任务表。`settle` = 原归属设备已经回执，加进去之后发落定。
    /// 任务先停用着进表（所有权还没有明确落到本机），落定发出后才启用。
    Land { settle: bool },
    /// 早就加进任务表了（强制接管），现在等到了回执：补发落定。
    Settle,
    /// 落定已经发过了，任务在表里还是停用的（中途断过）：补上启用。
    Enable,
    /// 另一台设备的强制接管压过了本机的：本机那份撤掉，请求留着——原归属设备的回执还
    /// 可能指向本机。
    Withdraw,
    /// 接不成了。
    GiveUp(Lost),
    /// 用不着再记了。
    Forget,
}

/// 接不成的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lost {
    /// 原归属设备在用户看过之后改了任务。
    Changed,
    /// 原归属设备删了任务，或者不再同步定时任务。
    Deleted,
    /// 原归属设备换了身份：它的任务以新身份重新发布，旧的请求没有人回执了。
    Replaced,
    /// 归属定给了另一台设备。
    Taken,
    /// 原归属设备没等到本机落定，把任务收了回去。
    Reclaimed,
}

impl Lost {
    /// 还没加进任务表的请求为什么接不成（给用户看）。
    fn why_failed(self) -> &'static str {
        match self {
            Lost::Changed => {
                "它在原来的设备上又改过了，你当时看到的已经不是最新的内容。请在「其他设备上的\
                 定时任务」里看一下它现在的内容，再决定要不要移到本机。"
            }
            Lost::Deleted => "它已经在原来的设备上被删除，或者那台设备不再同步定时任务了。",
            Lost::Replaced => {
                "原来的设备换了同步身份，这次请求作废了。它的任务会重新出现在「其他设备上的\
                 定时任务」里，需要的话请再操作一次。"
            }
            Lost::Taken => "另一台设备先一步把它接走了。",
            Lost::Reclaimed => {
                "本机没有及时接手，原来的设备已经把它收了回去，继续由它执行。需要的话请再操作\
                 一次。"
            }
        }
    }

    /// 已经在本机执行的那一份为什么撤掉（给用户看）。
    fn why_withdrawn(self) -> &'static str {
        match self {
            Lost::Taken => {
                "本机是在原来那台设备联系不上时强制接管它的；之后它归了另一台设备（原来的设备\
                 把它交给了那台设备，或者那台设备在本机之后也强制接管了它）。为了不让同一个任务\
                 在两台设备上各跑一遍，本机这一份撤掉了。"
            }
            Lost::Reclaimed => {
                "原来的设备没有等到本机确认接手，已经把它收了回去，继续由它执行。为了不让同\
                 一个任务在两台设备上各跑一遍，本机这一份撤掉了。需要的话请再操作一次。"
            }
            Lost::Changed | Lost::Replaced => {
                "本机是在原来那台设备联系不上时强制接管它的；那台设备其实还在，之后又改过这个\
                 任务（或者换了同步身份），任务仍然归它。为了不让同一个任务在两台设备上各跑\
                 一遍，本机这一份撤掉了。"
            }
            Lost::Deleted => {
                "本机是在原来那台设备联系不上时强制接管它的；那台设备其实还在，之后删除了这个\
                 任务，或者不再同步定时任务（任务留在它那里执行）。本机这一份撤掉了；还需要的话\
                 请在本机重新创建。"
            }
        }
    }
}

fn next_step(
    conn: &Connection,
    s: &Settings,
    owner: &str,
    key: &str,
    claim: &Claim,
    held: Option<bool>,
) -> Result<Step> {
    // 任务已撤回或出现新版本，旧的请求和临时接管都应结束。
    let Some(xfer) = effective(conn, key)? else {
        return Ok(match put_rev(conn, key)? {
            Some(rev) if rev == claim.rev => Step::Forget,
            Some(_) => Step::GiveUp(Lost::Changed),
            None => Step::GiveUp(Lost::Deleted),
        });
    };
    // 生效的记录说的是更新的一版：用户看到的那份已经过时了。
    if xfer.rev != claim.rev {
        return Ok(Step::GiveUp(Lost::Changed));
    }
    let mine = s.is_me(&xfer.to);
    if xfer.rank >= ACK {
        return Ok(match (mine, xfer.rank) {
            // 定了。请求留到任务不在任务表里为止：原归属设备更早的一条收回记录可能晚到，
            // 那时要靠它把本机这份撤掉。在表里还是停用的（落定前后中断过）就补上启用。
            (true, DONE) => match held {
                Some(false) => Step::Enable,
                Some(true) => Step::Wait,
                None => Step::Forget,
            },
            (true, _) => {
                if held.is_some() {
                    Step::Settle
                } else {
                    Step::Land { settle: true }
                }
            }
            (false, _) => {
                let back = lineage(conn, owner)?
                    .iter()
                    .any(|(device, _)| *device == xfer.to);
                Step::GiveUp(if back { Lost::Reclaimed } else { Lost::Taken })
            }
        });
    }
    // 还没定。原归属设备那边变了，就等不到回执了。
    if db::peer_state(conn, owner)?.replaced_by.is_some() {
        return Ok(Step::GiveUp(Lost::Replaced));
    }
    match put_rev(conn, key)? {
        Some(rev) if rev == claim.rev => {}
        Some(_) => return Ok(Step::GiveUp(Lost::Changed)),
        None => return Ok(Step::GiveUp(Lost::Deleted)),
    }
    Ok(if !mine {
        if held.is_some() {
            Step::Withdraw
        } else {
            Step::Wait
        }
    } else if xfer.rank == FORCE {
        match held {
            None => Step::Land { settle: false },
            // 用户后来才把那台设备标成「已丢失」，或者它已经陈旧（很久没出现）：它的
            // 回执不会来了，停用的这一份该启用了——不然任务谁都不执行。这条准入与
            // 界面给「强制接管」的条件一致。
            Some(false) if takeover_ready(conn, owner, now_ms())? => Step::Enable,
            Some(_) => Step::Wait,
        }
    } else {
        Step::Wait
    })
}

/// 这条请求加进本机任务表的那一份（还在的话）：记过账的那个，或者加进去了、没来得及
/// 记账的那个。
fn landed(jobs: &[CronJob], claim: &Claim) -> Option<String> {
    match (&claim.local, &claim.attempt) {
        (Some(local), _) => jobs
            .iter()
            .any(|job| job.id == *local)
            .then(|| local.clone()),
        (None, Some(attempt)) => orphan(jobs, &attempt.job, &attempt.before),
        (None, None) => None,
    }
}

/// 一条请求里的任务落到本机是什么样：投递目标换成本机上的那个会话。落不了地时给出原因。
fn landing_job(conn: &Connection, claim: &Claim) -> Result<std::result::Result<CronJob, String>> {
    // 请求之后本机的情况可能变了（投递目标的会话被删了）：能不能落地再看一遍。
    if let Some(reason) = blocked(conn, &claim.job, claim.target.as_deref())? {
        return Ok(Err(reason));
    }
    let Ok(mut job) = serde_json::from_value::<CronJob>(claim.job.clone()) else {
        return Ok(Err("这次接手请求的记录读不出来了，请重新操作".into()));
    };
    if let Some(sid) = &claim.target {
        let Some(session) = map::thread_sessions(conn, sid)?.into_iter().next() else {
            return Ok(Err("它的投递目标会话已经不在这台设备上了".into()));
        };
        job.channel = session.channel;
        job.chat_id = session.chat_id;
    }
    Ok(Ok(job))
}

/// 把一条请求里的任务加进本机任务表（先停用；落定之后才启用），返回它在任务表里的 id。
/// 外层的错误是库出了问题，内层的是落不了地的原因（给用户看）。
fn land(
    ctx: &Ctx,
    host: &dyn CronHost,
    jobs: &[CronJob],
    key: &str,
    claim: &mut Claim,
) -> Result<std::result::Result<String, String>> {
    let attempt = match claim.attempt.clone() {
        Some(attempt) => attempt,
        None => {
            let job = match db::read(ctx.db(), |conn| landing_job(conn, claim))? {
                Ok(job) => job,
                Err(reason) => return Ok(Err(reason)),
            };
            // 先记下要加的是哪一份、任务表里已有哪些同样的任务（推到盘上），再动手：
            // 掉电丢了这份记录的话，加进去的那一份会被当成新的再加一遍。
            let attempt = Attempt {
                before: twins(jobs, &job),
                job,
            };
            claim.attempt = Some(attempt.clone());
            save_claim(ctx, key, claim)?;
            durable(ctx)?;
            attempt
        }
    };
    Ok(match orphan(jobs, &attempt.job, &attempt.before) {
        Some(local) => Ok(local),
        None => {
            // 动任务表之前再推一次落盘点：上次可能提交成功、flush 却失败了（库还能读出
            // 这条记录，但掉电会丢）。已有意图也不能豁免——只有确认这份记录真的耐久了，
            // 才允许加进任务表；认回已存在的任务（上面的 orphan 分支）不新增，不用重推。
            durable(ctx)?;
            host.add_disabled(attempt.job)
        }
    })
}

/// 把接手来的、现在不该再由本机执行的那一份从任务表里撤掉，并告诉用户为什么。撤不掉
/// 返回 `false`：下一轮再来。
fn withdraw(
    ctx: &Ctx,
    host: &dyn CronHost,
    key: &str,
    claim: &Claim,
    local: &str,
    cause: Lost,
    report: &mut FilesReport,
) -> bool {
    if let Err(reason) = host.remove(local) {
        tracing::warn!("撤掉接手来的定时任务 {local} 失败: {reason}");
        report.retry = true;
        return false;
    }
    status::notice(
        &ctx.paths,
        "cron-takeover-lost",
        key,
        &format!("{}不再由本机执行，本机这一份已经移除", title_of(&claim.job)),
        cause.why_withdrawn(),
    );
    report.status_changed = true;
    report.reload.insert(Reload::Cron);
    true
}

/// 本机用户发起的接手请求：归属定给本机了（或者是强制接管）就把任务加进本机任务表
/// （先停用，落定之后才启用），接不成了就了结并归还，本机那份不该再执行了就撤掉。
fn settle_claims(
    ctx: &Ctx,
    s: &Settings,
    host: &dyn CronHost,
    report: &mut FilesReport,
) -> Result<()> {
    let claims = db::read(ctx.db(), |conn| db::state_scan(conn, SCOPE_CLAIM, ""))?;
    for (key, row) in claims {
        let (Some(mut claim), Some((owner, id))) = (row.json::<Claim>(), key.split_once('/'))
        else {
            drop_claim(ctx, &key)?;
            continue;
        };
        let jobs = host.jobs();
        let local = landed(&jobs, &claim);
        // 它在表里的话，现在是启用还是停用。
        let held = local.as_deref().and_then(|local| {
            jobs.iter()
                .find(|job| job.id == local)
                .map(|job| job.enabled)
        });
        let step = db::read(ctx.db(), |conn| {
            next_step(conn, s, owner, &key, &claim, held)
        })?;
        let done = transfer(owner, id, &s.device, &claim.rev, DONE);
        match step {
            Step::Wait => {}
            Step::Forget => drop_claim(ctx, &key)?,
            Step::Settle => {
                // 落定发出之后才启用：启用顺序反了的话，原归属设备等不到落定而收回时，
                // 本机这一份已经跑起来了。
                db::write(ctx.db(), |tx| publish_xfer(tx, s, &key, &done).map(drop))?;
                report.published += 1;
                if let Some(local) = &local {
                    if let Err(reason) = host.set_enabled(local, true) {
                        tracing::warn!("启用接手来的定时任务 {local} 失败: {reason}");
                        report.retry = true;
                    } else {
                        report.reload.insert(Reload::Cron);
                    }
                }
            }
            Step::Enable => {
                if let Some(local) = &local {
                    if let Err(reason) = host.set_enabled(local, true) {
                        tracing::warn!("启用接手来的定时任务 {local} 失败: {reason}");
                        report.retry = true;
                    } else {
                        report.reload.insert(Reload::Cron);
                    }
                }
            }
            Step::Land { settle } => match land(ctx, host, &jobs, &key, &mut claim)? {
                Ok(added) => {
                    claim.local = Some(added);
                    claim.withdrawn = false;
                    db::write(ctx.db(), |tx| {
                        db::state_put_json(tx, SCOPE_CLAIM, &key, db::HLC_ZERO, &claim)?;
                        if settle {
                            publish_xfer(tx, s, &key, &done)?;
                        }
                        Ok(())
                    })?;
                    report.published += usize::from(settle);
                    // 什么时候可以启用：落定已经发出去，或者原归属设备已被用户标为丢失
                    // （强制接管，它的回执永远不会来——任务没人执行才是真问题）。其余情况
                    // 保持停用：原归属设备还没确认把任务交出来，收回了也不会两边同时跑。
                    let owner_gone = db::read(ctx.db(), |conn| gone(conn, owner))?;
                    if (settle || owner_gone) && claim.local.is_some() {
                        if let Some(local) = &claim.local {
                            if let Err(reason) = host.set_enabled(local, true) {
                                tracing::warn!("启用接手来的定时任务 {local} 失败: {reason}");
                                report.retry = true;
                            } else {
                                report.reload.insert(Reload::Cron);
                            }
                        }
                    }
                    // 上一次没接住的提示用不着了。
                    report.status_changed |=
                        status::clear_notice(&ctx.paths, "cron-takeover", &key);
                    report.reload.insert(Reload::Cron);
                }
                Err(reason) => {
                    // 接不成：不发明文以外的「放弃」记录（现成的几种记录都比回执低，
                    // 盖不过它），原归属设备会一直等本机的落定。把话说清楚，让用户决定：
                    // 在这台设备上再试一次，或者到原归属设备上把它标为「已丢失」。
                    let hint = if settle {
                        "原来的设备已经把它交了出来，现在没有设备在执行它。可以在\
                         「其他设备上的定时任务」里再试一次；不想再移的话，去原来那台设备上\
                         把本机标为「已丢失」，它会把任务收回去继续执行。"
                    } else {
                        "可以在「其他设备上的定时任务」里再试一次。"
                    };
                    status::notice(
                        &ctx.paths,
                        "cron-takeover",
                        &key,
                        &format!("{}没能移到本机", title_of(&claim.job)),
                        &format!("{reason}。{hint}"),
                    );
                    drop_claim(ctx, &key)?;
                    report.status_changed = true;
                }
            },
            Step::Withdraw => {
                if let Some(local) = &local {
                    if !withdraw(ctx, host, &key, &claim, local, Lost::Taken, report) {
                        continue;
                    }
                }
                claim.local = None;
                claim.attempt = None;
                claim.withdrawn = true;
                save_claim(ctx, &key, &claim)?;
            }
            Step::GiveUp(cause) => {
                match &local {
                    Some(local) => {
                        if !withdraw(ctx, host, &key, &claim, local, cause, report) {
                            continue;
                        }
                    }
                    // 从没加进过任务表：用户还在等，告诉他接不成了。
                    None if claim.local.is_none() && !claim.withdrawn => {
                        status::notice(
                            &ctx.paths,
                            "cron-takeover",
                            &key,
                            &format!("{}没能移到本机", title_of(&claim.job)),
                            cause.why_failed(),
                        );
                        report.status_changed = true;
                    }
                    None => {}
                }
                drop_claim(ctx, &key)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 发布、镜像与每一轮的对比
// ---------------------------------------------------------------------------

/// 这台设备不再同步定时任务（范围关了）：发布过的任务向别的设备撤回——它们的镜像里不该
/// 再列着，也不该有人等着本机确认移交。任务本身留在本机照常执行；范围再打开时，它们由
/// [`publish_local`] 重新发布。
fn retire_local(ctx: &Ctx, s: &Settings, report: &mut FilesReport) -> Result<()> {
    let published: Vec<String> = db::read(ctx.db(), |conn| {
        Ok(db::state_scan(conn, SCOPE_BASE, "")?
            .into_iter()
            .map(|(id, _)| id)
            .collect())
    })?;
    if published.is_empty() {
        return Ok(());
    }
    report.published += published.len();
    db::write(ctx.db(), |tx| {
        for id in &published {
            let op = Op::CronDel {
                owner: s.device.clone(),
                id: id.clone(),
            };
            publish_definition(tx, s, id, &op)?;
            db::state_del(tx, SCOPE_BASE, id)?;
        }
        Ok(())
    })
}

/// 本机任务表与上次发布的版本对比，差异发布出去。
fn publish_local(
    ctx: &Ctx,
    s: &Settings,
    host: &dyn CronHost,
    report: &mut FilesReport,
) -> Result<()> {
    let jobs = host.jobs();
    let (puts, dels) = db::read(ctx.db(), |conn| {
        let bases: BTreeMap<String, String> = db::state_scan(conn, SCOPE_BASE, "")?
            .into_iter()
            .filter_map(|(id, row)| Some((id, row.value?)))
            .collect();
        let mut puts: Vec<(Op, String)> = Vec::new();
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for job in &jobs {
            if !is_job_id(&job.id) || !seen.insert(&job.id) {
                continue;
            }
            let (value, target, hash) = wire_form(conn, job)?;
            if bases.get(&job.id) == Some(&hash) {
                continue;
            }
            if value.to_string().len() > JOB_MAX {
                tracing::warn!("定时任务 {} 的定义过大，不同步", job.id);
                continue;
            }
            let op = Op::CronPut {
                owner: s.device.clone(),
                id: job.id.clone(),
                job: value,
                target,
            };
            puts.push((op, hash));
        }
        let dels: Vec<String> = bases
            .into_keys()
            .filter(|id| !seen.contains(id.as_str()))
            .collect();
        Ok((puts, dels))
    })?;
    if puts.is_empty() && dels.is_empty() {
        return Ok(());
    }
    report.published += puts.len() + dels.len();
    db::write(ctx.db(), |tx| {
        for (op, hash) in &puts {
            let Op::CronPut { id, .. } = op else {
                continue;
            };
            publish_definition(tx, s, id, op)?;
            db::state_put(tx, SCOPE_BASE, id, db::HLC_ZERO, None, Some(hash))?;
        }
        for id in &dels {
            let op = Op::CronDel {
                owner: s.device.clone(),
                id: id.clone(),
            };
            publish_definition(tx, s, id, &op)?;
            db::state_del(tx, SCOPE_BASE, id)?;
        }
        Ok(())
    })
}

fn publish_definition(conn: &Connection, s: &Settings, id: &str, op: &Op) -> Result<()> {
    let key = format!("{}/{id}", s.device);
    publish(conn, s, scope::CRON, &key, op)?;
    if let Some(row) = db::state_get(conn, scope::CRON, &key)? {
        prune_transfers(conn, &key, &row.hlc)?;
    }
    Ok(())
}

/// 重写只读镜像 `cron.remote.json`（内容没变就不写）。
fn write_mirror(ctx: &Ctx, jobs: &[RemoteJob], report: &mut FilesReport) -> Result<()> {
    let entries: Vec<serde_json::Value> = jobs
        .iter()
        .map(|job| {
            serde_json::json!({
                "owner": job.owner,
                "owner_name": job.owner_name,
                "id": job.id,
                "job": job.job,
            })
        })
        .collect();
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(&serde_json::json!({ "jobs": entries }))?
    );
    let path = ctx.layout.base.join(MIRROR_FILE);
    let current = std::fs::read_to_string(&path).ok();
    if current.as_deref() == Some(text.as_str()) || (current.is_none() && entries.is_empty()) {
        return Ok(());
    }
    rclaw_store::atomic_write(&path, text.as_bytes())?;
    report.reload.insert(Reload::Cron);
    Ok(())
}

pub(crate) fn reconcile(ctx: &Ctx, s: &Settings, report: &mut FilesReport) -> Result<()> {
    if let Some(host) = &ctx.cron {
        let host = host.as_ref();
        hand_over(ctx, s, host, report)?;
        settle_parked(ctx, s, host, report)?;
        settle_claims(ctx, s, host, report)?;
        publish_local(ctx, s, host, report)?;
    }
    let jobs = db::read(ctx.db(), |conn| remote_jobs(conn, s, now_ms()))?;
    write_mirror(ctx, &jobs, report)
}

/// 这台设备不再同步定时任务（范围关着）时每一轮做的收尾：发布过的任务向别的设备撤回，
/// 镜像清空，不再受理新的移交。已经开始的交接照常走完——本机请求过、对方已经回执的照样
/// 加进任务表；交出去、还没等到落定的留着等（不按时限自动收回，见 [`fate`]），用户把那台
/// 设备标为「已丢失」时收回。不然任务会卡在半路，哪边都不执行。
pub(crate) fn wind_down(ctx: &Ctx, s: &Settings, report: &mut FilesReport) -> Result<()> {
    retire_local(ctx, s, report)?;
    if let Some(host) = &ctx.cron {
        let host = host.as_ref();
        settle_parked(ctx, s, host, report)?;
        settle_claims(ctx, s, host, report)?;
    }
    write_mirror(ctx, &[], report)
}

/// 本机要清掉同步状态了（退出同步空间，或者换了一个）：交出去、还没等到对方落定的任务
/// 放回本机的任务表。清掉之后就再也看不到对方有没有接手——对方其实已经接手的话会多出
/// 一份，但不会两边都没有。放不回去的（没有调度器、任务表不收）把定义另存一份。
pub(crate) fn release_parked(ctx: &Ctx) -> Result<()> {
    let (found, mine) = db::read(ctx.db(), |conn| {
        let mut out = Vec::new();
        for (key, row) in db::state_scan(conn, SCOPE_PARKED, "")? {
            let xfer = effective(conn, &key)?;
            out.push((key, row, xfer));
        }
        Ok((out, sync_db::device_ids(conn)?))
    })?;
    for (key, row, xfer) in found {
        let id = key.split_once('/').map_or(key.as_str(), |(_, id)| id);
        let keep = |text: &str, why: &str| {
            let path = save_definition(ctx, id, text);
            tracing::warn!(
                "交出去的定时任务 {key} 没能放回本机（{why}），定义另存在 {}",
                path.display()
            );
        };
        match row.json::<Parked>() {
            None => keep(row.value.as_deref().unwrap_or_default(), "记录读不懂"),
            Some(mut parked) => {
                let definition = parked.job.to_string();
                // 对方已经落定：任务在它那里，本机收回过的那份（有的话）撤掉。
                let taken = xfer.as_ref().is_some_and(|xfer| {
                    xfer.rev == parked.rev && xfer.rank == DONE && !mine.contains(&xfer.to)
                });
                // 交接悬着（回执已发、对方没落定）：清掉之后就再也看不到它落没落定，
                // 放回来必须停用——对方其实已经接手的话，启用的两份会同时跑。定义作为
                // 停用备份保留，用户核对另一台设备后再启用。
                let unconfirmed = xfer.as_ref().is_some_and(|xfer| {
                    xfer.rev == parked.rev
                        && xfer.rank >= REQUEST
                        && xfer.rank <= ACK
                        && !mine.contains(&xfer.to)
                });
                match &ctx.cron {
                    Some(host) => {
                        let jobs = host.jobs();
                        match (taken, restored(&jobs, &parked)) {
                            (true, Some(local)) => {
                                if let Err(reason) = host.remove(&local) {
                                    tracing::warn!("撤掉收回的定时任务 {local} 失败: {reason}");
                                }
                            }
                            (false, None) => {
                                if let Err(reason) = put_back(
                                    ctx,
                                    host.as_ref(),
                                    &jobs,
                                    &key,
                                    &mut parked,
                                    unconfirmed,
                                )? {
                                    keep(&definition, &reason);
                                } else if unconfirmed {
                                    status::notice(
                                        &ctx.paths,
                                        "cron-handover-unknown",
                                        &key,
                                        &format!("{}放回了本机，但保持停用", title_of(&parked.job)),
                                        "本机曾把这个任务交给另一台设备，退出 / 重置时没能确认\
                                         它有没有接手。为了不出现两台设备同时执行，放回的这份\
                                         是停用的；到那台设备上核对过之后，在本机的任务表里\
                                         手动启用或删掉其一。",
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                    None if !taken && parked.restored.is_none() => keep(&definition, "没有调度器"),
                    None => {}
                }
            }
        }
        drop_parked(ctx, &key)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::files::{self, Cache};
    use crate::testing::{self, Device};

    /// 记一条远端来的 op：寄存器里的值照合入时那样现算。
    fn record(conn: &Connection, origin: &str, hlc: &str, op: &Op) -> Result<bool> {
        super::record(conn, origin, hlc, op, &files::stored_text(origin, op)?)
    }

    /// 测试用的任务表：与调度器一样，新增时重新分配 id。
    #[derive(Default)]
    struct Table {
        jobs: Mutex<Vec<CronJob>>,
        next: Mutex<u32>,
        /// 模拟任务表不收（写不了盘）。
        refuse: Mutex<bool>,
    }

    impl CronHost for Table {
        fn jobs(&self) -> Vec<CronJob> {
            self.jobs.lock().unwrap().clone()
        }

        fn add(&self, mut job: CronJob) -> std::result::Result<String, String> {
            if *self.refuse.lock().unwrap() {
                return Err("任务表写不进去".into());
            }
            let mut next = self.next.lock().unwrap();
            *next += 1;
            job.id = format!("{:08x}", *next);
            job.enabled = true;
            self.jobs.lock().unwrap().push(job.clone());
            Ok(job.id)
        }

        fn add_disabled(&self, mut job: CronJob) -> std::result::Result<String, String> {
            if *self.refuse.lock().unwrap() {
                return Err("任务表写不进去".into());
            }
            let mut next = self.next.lock().unwrap();
            *next += 1;
            job.id = format!("{:08x}", *next);
            job.enabled = false;
            self.jobs.lock().unwrap().push(job.clone());
            Ok(job.id)
        }

        fn set_enabled(&self, id: &str, enabled: bool) -> std::result::Result<(), String> {
            let mut jobs = self.jobs.lock().unwrap();
            match jobs.iter_mut().find(|job| job.id == id) {
                Some(job) => {
                    job.enabled = enabled;
                    Ok(())
                }
                None => Err(format!("任务 {id} 不在任务表里")),
            }
        }

        fn remove(&self, id: &str) -> std::result::Result<bool, String> {
            let mut jobs = self.jobs.lock().unwrap();
            let before = jobs.len();
            jobs.retain(|job| job.id != id);
            Ok(jobs.len() != before)
        }
    }

    fn device() -> (Device, Arc<Table>) {
        let mut device = Device::new();
        let table = Arc::new(Table::default());
        device.ctx.cron = Some(table.clone());
        (device, table)
    }

    fn job(name: &str) -> CronJob {
        serde_json::from_value(serde_json::json!({
            "id": "", "name": name, "kind": "every", "interval_s": 3600,
            "message": "检查一下邮箱", "channel": "system", "chat_id": "cron",
        }))
        .unwrap()
    }

    fn sync_files(device: &Device) -> files::FilesReport {
        files::reconcile(
            &device.ctx,
            &device.settings,
            &device.keys,
            &mut Cache::default(),
            true,
        )
    }

    fn exchange(from: &Device, to: &Device) {
        sync_files(from);
        testing::seal_all(from);
        testing::ship(from, to);
        to.merge();
        sync_files(to);
    }

    fn remote(device: &Device) -> Vec<RemoteJob> {
        db::read(device.ctx.db(), |conn| {
            remote_jobs(conn, &device.settings, now_ms())
        })
        .unwrap()
    }

    fn names(table: &Table) -> Vec<String> {
        table.jobs().into_iter().map(|job| job.name).collect()
    }

    /// 用户在 `on` 上对列表里的第一个任务点「移到本机执行」（或强制接管）。
    fn take(on: &Device, force: bool) -> Result<()> {
        let job = remote(on).into_iter().next().expect("列表里有任务");
        request_takeover(
            &on.ctx,
            &on.settings,
            &job.owner,
            &job.id,
            force,
            &job.version,
        )
    }

    /// 用户在 `on` 上把 `device` 标为「已丢失」。
    fn mark_lost_on(on: &Device, device: &Device) {
        db::write(on.ctx.db(), |tx| {
            mark_lost(tx, &device.settings.device, true)
        })
        .unwrap();
    }

    fn parked(device: &Device) -> Vec<Parked> {
        db::read(device.ctx.db(), |conn| {
            Ok(db::state_scan(conn, SCOPE_PARKED, "")?
                .into_iter()
                .filter_map(|(_, row)| row.json())
                .collect())
        })
        .unwrap()
    }

    fn claims(device: &Device) -> Vec<Claim> {
        db::read(device.ctx.db(), |conn| {
            Ok(db::state_scan(conn, SCOPE_CLAIM, "")?
                .into_iter()
                .filter_map(|(_, row)| row.json())
                .collect())
        })
        .unwrap()
    }

    fn notices(device: &Device, kind: &str) -> usize {
        status::list(&device.ctx.paths)
            .iter()
            .filter(|item| item.kind == kind)
            .count()
    }

    /// 把「回执已经传出去」的时刻往前拨，模拟等了一天多。
    fn age_parked(device: &Device) {
        db::write(device.ctx.db(), |tx| {
            for (key, row) in db::state_scan(tx, SCOPE_PARKED, "")? {
                let mut parked: Parked = row.json().expect("留底");
                assert!(parked.sent_ms > 0, "回执传出去之后才开始计时");
                parked.sent_ms -= RECLAIM_AFTER_MS + 1;
                db::state_put_json(tx, SCOPE_PARKED, &key, db::HLC_ZERO, &parked)?;
            }
            Ok(())
        })
        .unwrap();
    }

    /// 让接下来发的 HLC 一定晚于之前发的（两台测试设备共用一个时钟）。
    fn later() {
        std::thread::sleep(Duration::from_millis(5));
    }

    #[test]
    fn each_job_lives_in_exactly_one_task_table() {
        let (a, table_a) = device();
        let (b, table_b) = device();
        table_a.add(job("甲的任务")).unwrap();
        table_b.add(job("乙的任务")).unwrap();
        exchange(&a, &b);
        exchange(&b, &a);
        // 两台设备都看得到两个任务，但各自的任务表里只有自己的那个。
        assert_eq!(names(&table_a), ["甲的任务"]);
        assert_eq!(names(&table_b), ["乙的任务"]);
        assert_eq!(remote(&a).len(), 1);
        assert_eq!(remote(&a)[0].job["name"], "乙的任务");
        assert_eq!(remote(&b)[0].job["name"], "甲的任务");
        let mirror = std::fs::read_to_string(b.ctx.layout.base.join(MIRROR_FILE)).unwrap();
        assert!(mirror.contains("甲的任务"));
        // 触发状态的变化不重新发布。
        table_a.jobs.lock().unwrap()[0].next_run += 3600;
        let before = testing::outbox(&a).len();
        sync_files(&a);
        assert_eq!(testing::outbox(&a).len(), before);
        // 删除跟着传过去。
        let id = table_a.jobs()[0].id.clone();
        table_a.remove(&id).unwrap();
        exchange(&a, &b);
        assert!(remote(&b).is_empty());
    }

    #[test]
    fn a_transfer_waits_for_the_owner_and_moves_the_job_once() {
        let (a, table_a) = device();
        let (b, table_b) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        let listed = remote(&b)[0].clone();
        let request = |force: bool, reviewed: &str| {
            request_takeover(
                &b.ctx,
                &b.settings,
                &listed.owner,
                &listed.id,
                force,
                reviewed,
            )
        };
        // 最近还在线的设备不能被强制接管。
        assert!(request(true, &listed.version).is_err());
        // 用户看到的不是现在登记着的这一版：不办。
        assert!(request(false, "1763438400000-0000-00000000000000aa").is_err());
        request(false, &listed.version).unwrap();
        sync_files(&b);
        // 回执到达之前，任务还只在原设备上。
        assert!(table_b.jobs().is_empty());
        assert!(remote(&b)[0].pending);
        exchange(&b, &a);
        assert!(table_a.jobs().is_empty(), "原设备合入请求后移除任务");
        assert_eq!(parked(&a).len(), 1, "任务的定义留着，等对方落定");
        exchange(&a, &b);
        assert_eq!(names(&table_b), ["巡检"]);
        assert!(remote(&b).is_empty());
        // 再对比几轮也不会加第二遍；原设备的镜像里它现在归乙端。
        sync_files(&b);
        assert_eq!(table_b.jobs().len(), 1);
        exchange(&b, &a);
        assert_eq!(remote(&a).len(), 1);
        assert_eq!(remote(&a)[0].owner, b.settings.device);
        assert!(table_a.jobs().is_empty());
        assert!(parked(&a).is_empty(), "对方落定了：不用再留着任务的定义");
    }

    #[test]
    fn a_lost_device_can_be_taken_over_without_its_consent() {
        let (a, table_a) = device();
        let (b, table_b) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        mark_lost_on(&b, &a);
        assert!(remote(&b)[0].can_force);
        take(&b, true).unwrap();
        sync_files(&b);
        assert_eq!(names(&table_b), ["巡检"], "强制接管不等回执");
        assert!(remote(&b).is_empty());
        // 那台设备其实还在：它重新联网、看到接管记录后停止执行。
        exchange(&b, &a);
        assert!(table_a.jobs().is_empty());
        assert_eq!(remote(&a)[0].owner, b.settings.device);
        // 它的回执一到，这次接管就落定了：任务还是那一份，原设备不用再留着定义。
        exchange(&a, &b);
        exchange(&b, &a);
        assert_eq!(names(&table_b), ["巡检"]);
        assert!(parked(&a).is_empty());
    }

    #[test]
    fn forged_or_misattributed_ops_are_refused() {
        let (a, table_a) = device();
        let (b, table_b) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        let listed = remote(&b)[0].clone();
        let (id, rev) = (listed.id.clone(), listed.version.clone());
        let me = b.settings.device.clone();
        let other = a.settings.device.clone();
        let stranger = "0123456789abcdef";
        let hlc = "9763438400000-0001-0123456789abcdef";
        db::write(b.ctx.db(), |tx| {
            // 冒充归属设备发布、删除它的任务。
            let put = Op::CronPut {
                owner: other.clone(),
                id: id.clone(),
                job: serde_json::json!({ "id": id }),
                target: None,
            };
            assert!(record(tx, stranger, hlc, &put).is_err());
            let del = Op::CronDel {
                owner: other.clone(),
                id: id.clone(),
            };
            assert!(record(tx, stranger, hlc, &del).is_err());
            // 不是原归属设备发的回执；不是接手的设备自己发的请求、落定。
            let ack = transfer(&other, &id, &me, &rev, ACK);
            assert!(record(tx, stranger, hlc, &ack).is_err());
            assert!(record(tx, &me, hlc, &ack).is_err());
            assert!(record(tx, &other, hlc, &transfer(&other, &id, &me, &rev, REQUEST)).is_err());
            assert!(record(tx, &other, hlc, &transfer(&other, &id, &me, &rev, DONE)).is_err());
            // 版本不是原归属设备发出的 HLC；指向原归属设备自己的不是落定；标志不止一个。
            let foreign = transfer(&other, &id, &me, "1763438400000-0000-0123456789abcdef", ACK);
            assert!(record(tx, &other, hlc, &foreign).is_err());
            assert!(record(tx, &other, hlc, &transfer(&other, &id, &me, "x", ACK)).is_err());
            assert!(record(tx, &other, hlc, &transfer(&other, &id, &other, &rev, ACK)).is_err());
            let both = Op::CronTransfer {
                id: id.clone(),
                from: other.clone(),
                to: me.clone(),
                rev: rev.clone(),
                force: false,
                ack: true,
                done: true,
            };
            assert!(record(tx, &other, hlc, &both).is_err());
            // 原归属设备发来一条回执，但本机用户没有请求过：记下，不建任务。
            let clock = format!("8000000000000-0001-{other}");
            assert!(record(tx, &other, &clock, &ack).unwrap());
            Ok(())
        })
        .unwrap();
        sync_files(&b);
        assert!(table_b.jobs().is_empty());
    }

    #[test]
    fn transfer_precedence_does_not_depend_on_arrival_order() {
        let (_g, ctx) = testing::ctx();
        let _ = testing::settings(&ctx);
        let (from, to, late) = ("00000000000000aa", "00000000000000bb", "00000000000000cc");
        let old = "1763438400000-0000-00000000000000aa";
        let new = "1763438400100-0000-00000000000000aa";
        let ops = [
            (
                to,
                "1763438400001-0001-00000000000000bb",
                transfer(from, "0000beef", to, old, REQUEST),
            ),
            (
                late,
                "1763438400009-0001-00000000000000cc",
                transfer(from, "0000beef", late, old, REQUEST),
            ),
            (
                to,
                "1763438400005-0001-00000000000000bb",
                transfer(from, "0000beef", to, old, FORCE),
            ),
            (
                from,
                "1763438400003-0001-00000000000000aa",
                transfer(from, "0000beef", to, old, ACK),
            ),
            (
                to,
                "1763438400020-0001-00000000000000bb",
                transfer(from, "0000beef", to, old, DONE),
            ),
            (
                from,
                "1763438400030-0001-00000000000000aa",
                transfer(from, "0000beef", from, old, DONE),
            ),
            (
                late,
                "1763438400200-0001-00000000000000cc",
                transfer(from, "0000beef", late, new, REQUEST),
            ),
        ];
        let key = format!("{from}/0000beef");
        let definition = Op::CronPut {
            owner: from.into(),
            id: "0000beef".into(),
            job: serde_json::json!({ "id": "0000beef" }),
            target: None,
        };
        let winner = |order: &[usize]| {
            db::write(ctx.db(), |tx| {
                tx.execute(
                    "DELETE FROM sync_state WHERE scope = ?1",
                    params![SCOPE_XFER],
                )?;
                db::state_del(tx, scope::CRON, &key)?;
                for index in order {
                    let (origin, hlc, op) = &ops[*index];
                    record(tx, origin, hlc, op)?;
                }
                record(tx, from, old, &definition)?;
                if order.contains(&6) {
                    record(tx, from, new, &definition)?;
                }
                Ok(effective(tx, &key)?.unwrap())
            })
            .unwrap()
        };
        for order in [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1], [1, 3, 0, 2]] {
            assert_eq!(
                winner(&order),
                Xfer::of(&ops[3].2, ops[3].1).unwrap(),
                "{order:?}"
            );
        }
        for order in [[0, 3, 4, 5], [5, 4, 3, 0], [4, 5, 0, 3], [5, 0, 4, 3]] {
            assert_eq!(
                winner(&order),
                Xfer::of(&ops[4].2, ops[4].1).unwrap(),
                "{order:?}"
            );
        }
        for order in [[6, 2, 3, 4, 5], [5, 4, 3, 2, 6], [4, 6, 5, 2, 3]] {
            assert_eq!(
                winner(&order),
                Xfer::of(&ops[6].2, ops[6].1).unwrap(),
                "{order:?}"
            );
        }
        db::write(ctx.db(), |tx| {
            let bogus = "8000000000000-0000-00000000000000aa";
            let future = transfer(from, "0000beef", late, bogus, REQUEST);
            let premature = transfer(from, "0000beef", late, new, DONE);
            assert!(record(
                tx,
                late,
                "8000000000000-0001-00000000000000cc",
                &future
            )?);
            assert!(record(
                tx,
                late,
                "1763438400150-0001-00000000000000cc",
                &premature
            )?);
            assert_eq!(effective(tx, &key)?, Xfer::of(&ops[6].2, ops[6].1));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_job_added_before_an_interruption_is_recognised_not_added_twice() {
        let wanted = job("巡检");
        let mut own = job("巡检");
        own.id = "0000000a".into();
        let before = twins(&[own.clone()], &wanted);
        assert_eq!(before, ["0000000a"]);
        // 用户本来就有的一模一样的任务，不算这次加进去的。
        assert_eq!(orphan(&[own.clone()], &wanted, &before), None);
        let mut added = job("巡检");
        added.id = "0000000b".into();
        added.next_run = 99;
        assert_eq!(
            orphan(&[own, added], &wanted, &before).as_deref(),
            Some("0000000b")
        );
    }

    #[test]
    fn the_owner_keeps_waiting_and_reports_when_the_taker_never_confirms() {
        let (a, table_a) = device();
        let (b, table_b) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        take(&b, false).unwrap();
        exchange(&b, &a);
        assert!(table_a.jobs().is_empty());
        // 回执还在待发队列里：不计时。
        sync_files(&a);
        assert_eq!(parked(&a)[0].sent_ms, 0);
        // 回执传出去了，接手的那台却迟迟不出现。
        testing::seal_all(&a);
        sync_files(&a);
        assert!(table_a.jobs().is_empty());
        age_parked(&a);
        sync_files(&a);
        // 等了一天也没等到落定：不再按时限自动收回——「读到头」证明不了它没有落定，
        // 收回了就可能两边各跑一遍。提醒用户来处理；对方的接手在落定之前是停用的，
        // 这个任务此刻没有任何设备在执行。
        assert!(table_a.jobs().is_empty(), "超时不自动收回");
        assert_eq!(notices(&a, "cron-handover-unconfirmed"), 1);
        // 用户确认那台设备已经不在了：这才收回。
        mark_lost_on(&a, &b);
        sync_files(&a);
        assert_eq!(names(&table_a), ["巡检"], "用户确认失联：收回");
        assert_eq!(notices(&a, "cron-reclaimed"), 1);
        sync_files(&a);
        assert_eq!(table_a.jobs().len(), 1, "不会收回两遍");
        // 接手的那台这时才出现：归属已经定回原设备，它不再把任务加进来。
        exchange(&a, &b);
        assert!(table_b.jobs().is_empty());
        assert_eq!(notices(&b, "cron-takeover"), 1);
        assert!(claims(&b).is_empty());
        // 收回的任务照常列在别的设备的镜像里（它在原设备上是一个新任务）。
        assert_eq!(remote(&b).len(), 1);
        assert_eq!(remote(&b)[0].job["name"], "巡检");
        assert!(!remote(&b)[0].pending);
    }

    #[test]
    fn a_taker_marked_lost_gives_the_job_back_at_once() {
        let (a, table_a) = device();
        let (b, _table_b) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        take(&b, false).unwrap();
        exchange(&b, &a);
        assert!(table_a.jobs().is_empty());
        mark_lost_on(&a, &b);
        sync_files(&a);
        assert_eq!(names(&table_a), ["巡检"]);
    }

    #[test]
    fn the_owner_waits_when_it_has_not_seen_the_settle() {
        let (a, table_a) = device();
        let (b, table_b) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        take(&b, false).unwrap();
        exchange(&b, &a);
        // 接手的设备看到回执，把任务加进任务表（先停用）、发了落定、然后启用——
        // 但落定还没传到原设备。
        exchange(&a, &b);
        assert_eq!(names(&table_b), ["巡检"]);
        assert!(table_b.jobs()[0].enabled, "落定已发出：已启用");
        later();
        sync_files(&a);
        age_parked(&a);
        sync_files(&a);
        // 原设备没看到落定也不收回：收回了就可能两边各跑一遍；提醒用户来处理。
        assert!(table_a.jobs().is_empty(), "超时不自动收回");
        assert_eq!(notices(&a, "cron-handover-unconfirmed"), 1);
        // 落定到达：任务归接手的设备，本机的留底了结。
        exchange(&b, &a);
        assert!(table_a.jobs().is_empty());
        assert_eq!(notices(&a, "cron-reclaim-undone"), 0);
        assert!(parked(&a).is_empty());
        assert_eq!(names(&table_b), ["巡检"]);
    }

    #[test]
    fn a_reclaim_that_races_a_late_settle_loses_to_the_taker() {
        let (a, table_a) = device();
        let (b, table_b) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        take(&b, false).unwrap();
        exchange(&b, &a);
        // 回执到了接手的设备的收件箱，它还没来得及处理……
        testing::ship(&a, &b);
        sync_files(&a);
        // ……原设备把它标为「已丢失」，把任务收了回来（收回记录还没传出去）。
        mark_lost_on(&a, &b);
        sync_files(&a);
        assert_eq!(names(&table_a), ["巡检"]);
        later();
        // 接手的设备这才看到回执（还没看到对方的收回）：加进任务表（先停用），发落定。
        b.merge();
        sync_files(&b);
        assert_eq!(names(&table_b), ["巡检"]);
        assert!(table_b.jobs()[0].enabled, "落定已发出：已启用");
        // 两边的落定互相到达：先发的（原设备）算数，接手的设备把自己那份撤掉。
        exchange(&a, &b);
        assert!(table_b.jobs().is_empty());
        assert_eq!(notices(&b, "cron-takeover-lost"), 1);
        assert!(claims(&b).is_empty());
        exchange(&b, &a);
        assert_eq!(names(&table_a), ["巡检"]);
        assert_eq!(notices(&a, "cron-reclaim-undone"), 0);
    }

    #[test]
    fn a_forced_copy_is_withdrawn_when_another_device_ends_up_with_the_job() {
        let (a, table_a) = device();
        let (b, table_b) = device();
        let (c, table_c) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        exchange(&a, &c);
        mark_lost_on(&b, &a);
        mark_lost_on(&c, &a);
        take(&b, true).unwrap();
        sync_files(&b);
        assert_eq!(names(&table_b), ["巡检"]);
        // 原设备已被用户标为「已丢失」：强制接管直接算数，任务立刻启用（它的回执不会来）。
        assert!(table_b.jobs()[0].enabled, "原设备已标失联：直接启用");
        later();
        take(&c, true).unwrap();
        sync_files(&c);
        assert_eq!(names(&table_c), ["巡检"]);
        // 后一条强制接管压过前一条：先接管的那台把自己那份撤掉。
        exchange(&c, &b);
        assert!(table_b.jobs().is_empty());
        assert_eq!(notices(&b, "cron-takeover-lost"), 1);
        exchange(&b, &c);
        assert_eq!(names(&table_c), ["巡检"]);
        // 原来的设备其实还在：它把任务交给生效的那条记录指的设备，那台设备落定。
        exchange(&c, &a);
        assert!(table_a.jobs().is_empty());
        exchange(&a, &c);
        exchange(&a, &b);
        // 接手方的落定依赖它已经读过的另一位申请者的日志。
        testing::ship(&b, &a);
        a.merge();
        exchange(&c, &a);
        assert_eq!(names(&table_c), ["巡检"]);
        assert!(table_b.jobs().is_empty());
        assert!(claims(&b).is_empty(), "归属定给了别的设备：请求了结");
        assert!(parked(&a).is_empty());
    }

    #[test]
    fn changing_the_job_voids_requests_for_the_old_version() {
        let (a, table_a) = device();
        let (b, table_b) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        take(&b, false).unwrap();
        // 请求到达之前，任务在原设备上改了内容。
        table_a.jobs.lock().unwrap()[0].message = "改成检查日历".into();
        exchange(&b, &a);
        assert_eq!(names(&table_a), ["巡检"], "有还没发布的改动：不交出去");
        exchange(&a, &b);
        assert!(
            table_b.jobs().is_empty(),
            "用户看到的那一版已经过时：不加进任务表"
        );
        assert_eq!(notices(&b, "cron-takeover"), 1);
        // 列表里是新内容，可以重新请求；落到本机的是用户这次看到的这一版。
        assert_eq!(remote(&b)[0].job["message"], "改成检查日历");
        assert!(!remote(&b)[0].pending);
        take(&b, false).unwrap();
        exchange(&b, &a);
        assert!(table_a.jobs().is_empty());
        exchange(&a, &b);
        assert_eq!(table_b.jobs()[0].message, "改成检查日历");
    }

    #[test]
    fn a_forced_copy_is_withdrawn_when_the_owner_is_alive_and_changes_the_job() {
        let (a, table_a) = device();
        let (b, table_b) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        mark_lost_on(&b, &a);
        take(&b, true).unwrap();
        sync_files(&b);
        assert_eq!(names(&table_b), ["巡检"]);
        // 那台设备其实还在，没看到接管记录就改了任务：任务仍然归它，本机那份撤掉。
        table_a.jobs.lock().unwrap()[0].message = "改成检查日历".into();
        exchange(&a, &b);
        assert!(table_b.jobs().is_empty());
        assert_eq!(notices(&b, "cron-takeover-lost"), 1);
        assert_eq!(remote(&b).len(), 1);
        // 原设备之后看到那条针对旧版的接管记录：不理会。
        exchange(&b, &a);
        assert_eq!(names(&table_a), ["巡检"]);
        assert!(parked(&a).is_empty());
    }

    #[test]
    fn turning_the_scope_off_retires_published_jobs() {
        let (a, table_a) = device();
        let (b, table_b) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        take(&b, false).unwrap();
        sync_files(&b);
        testing::ship(&b, &a);
        a.merge();
        // 原设备不再同步定时任务：发布过的任务撤回，任务本身留在它那里照常执行。
        let mut off = a.settings.clone();
        off.scopes.cron = false;
        files::reconcile(&a.ctx, &off, &a.keys, &mut Cache::default(), true);
        assert_eq!(names(&table_a), ["巡检"], "范围关着：不交出任务");
        testing::ship(&a, &b);
        b.merge();
        sync_files(&b);
        assert!(remote(&b).is_empty());
        assert!(table_b.jobs().is_empty());
        assert_eq!(notices(&b, "cron-takeover"), 1, "等不到回执了：请求了结");
        // 范围再打开：任务重新发布，别的设备又看得到了；之前那条请求不再算数。
        exchange(&a, &b);
        assert_eq!(remote(&b).len(), 1);
        assert_eq!(names(&table_a), ["巡检"]);
    }

    #[test]
    fn leaving_the_space_puts_parked_jobs_back() {
        let (a, table_a) = device();
        let (b, _table_b) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        take(&b, false).unwrap();
        exchange(&b, &a);
        assert!(table_a.jobs().is_empty());
        release_parked(&a.ctx).unwrap();
        assert_eq!(names(&table_a), ["巡检"], "交出去、还没落定的任务放回本机");
        assert!(parked(&a).is_empty());
        release_parked(&a.ctx).unwrap();
        assert_eq!(table_a.jobs().len(), 1);
    }

    #[test]
    fn a_failed_landing_can_be_retried() {
        let (a, table_a) = device();
        let (b, table_b) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        take(&b, false).unwrap();
        exchange(&b, &a);
        *table_b.refuse.lock().unwrap() = true;
        exchange(&a, &b);
        assert!(table_b.jobs().is_empty());
        assert_eq!(notices(&b, "cron-takeover"), 1);
        assert!(claims(&b).is_empty());
        // 归属已经定给本机、本机却没接住：任务还列着，再点一次就接着落地，不用再等原设备。
        *table_b.refuse.lock().unwrap() = false;
        assert_eq!(remote(&b).len(), 1);
        take(&b, false).unwrap();
        sync_files(&b);
        assert_eq!(names(&table_b), ["巡检"]);
        assert!(remote(&b).is_empty());
        assert_eq!(
            notices(&b, "cron-takeover"),
            0,
            "接住了：之前的失败提示撤掉"
        );
        // 落定传到原设备：了结。
        exchange(&b, &a);
        assert!(parked(&a).is_empty());
        assert!(table_a.jobs().is_empty());
    }

    #[test]
    fn an_unreadable_taker_is_reported_instead_of_reclaimed_blindly() {
        let (a, table_a) = device();
        let (b, _table_b) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        take(&b, false).unwrap();
        exchange(&b, &a);
        testing::seal_all(&a);
        sync_files(&a);
        age_parked(&a);
        // 对方的日志读不下去：它也许已经落定了，只是本机看不到。
        let taker = b.settings.device.clone();
        let halt = |reason: Option<&str>| {
            db::write(a.ctx.db(), |tx| {
                let mut peer = db::peer_state(tx, &taker)?;
                peer.halted = reason.map(str::to_string);
                db::put_peer_state(tx, &taker, &peer)
            })
            .unwrap();
        };
        halt(Some("读不下去"));
        sync_files(&a);
        sync_files(&a);
        assert!(table_a.jobs().is_empty(), "不盲目收回");
        assert_eq!(notices(&a, "cron-handover-unconfirmed"), 1);
        // 读得到了也不够：它可能是落定写好了还没传上来。继续等，提示保留。
        halt(None);
        sync_files(&a);
        assert!(table_a.jobs().is_empty(), "读到头也不自动收回");
        assert_eq!(notices(&a, "cron-handover-unconfirmed"), 1);
        // 用户确认那台设备不在了：收回，提示撤掉。
        mark_lost_on(&a, &b);
        sync_files(&a);
        assert_eq!(names(&table_a), ["巡检"]);
        assert_eq!(notices(&a, "cron-handover-unconfirmed"), 0);
    }

    #[test]
    fn marking_the_new_identity_of_a_taker_lost_counts_too() {
        let (a, table_a) = device();
        let (b, _table_b) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        take(&b, false).unwrap();
        exchange(&b, &a);
        assert!(table_a.jobs().is_empty());
        // 接手的设备后来换了身份，用户标记的是它的新身份。
        let (old, new) = (b.settings.device.clone(), "00000000000000ee");
        db::write(a.ctx.db(), |tx| {
            let mut peer = db::peer_state(tx, &old)?;
            peer.replaced_by = Some(new.to_string());
            db::put_peer_state(tx, &old, &peer)?;
            mark_lost(tx, new, true)
        })
        .unwrap();
        sync_files(&a);
        assert_eq!(names(&table_a), ["巡检"]);
    }

    #[test]
    fn a_new_identity_resends_the_records_only_it_may_send() {
        let (a, table_a) = device();
        let (b, table_b) = device();
        let (c, _table_c) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        let listed = remote(&b)[0].clone();
        let key = format!("{}/{}", listed.owner, listed.id);
        take(&b, false).unwrap();
        exchange(&b, &a);
        testing::ship(&b, &c);
        exchange(&a, &c);
        exchange(&a, &b);
        assert_eq!(names(&table_b), ["巡检"]);
        // 接手的设备发了落定，还没传出去就换了身份。
        let (old, new) = (b.settings.device.clone(), "00000000000000ee");
        let original = db::read(b.ctx.db(), |conn| Ok(effective(conn, &key)?.unwrap())).unwrap();
        let queued: Vec<(String, String)> = db::write(b.ctx.db(), |tx| {
            on_rotate(tx, std::slice::from_ref(&old), new)?;
            let mut stmt = tx.prepare("SELECT hlc, op FROM sync_outbox ORDER BY id")?;
            let found = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok(found.collect::<rusqlite::Result<_>>()?)
        })
        .unwrap();
        // 旧身份名下的定时任务 op 作废；落定改成指向新身份，带着原来的 HLC 再发一遍。
        assert_eq!(queued.len(), 1);
        let (hlc, op) = &queued[0];
        let op: Op = serde_json::from_str(op).unwrap();
        assert_eq!(*hlc, original.hlc);
        assert_eq!(
            op,
            transfer(&listed.owner, &listed.id, new, &listed.version, DONE)
        );
        // 没收到过原件的设备补上这一条，交接照常了结。
        assert!(db::write(a.ctx.db(), |tx| {
            let mut peer = db::peer_state(tx, &old)?;
            peer.replaced_by = Some(new.to_string());
            db::put_peer_state(tx, &old, &peer)?;
            record(tx, new, hlc, &op)
        })
        .unwrap());
        sync_files(&a);
        assert!(parked(&a).is_empty());
        assert!(table_a.jobs().is_empty());
        // 新身份的记录可以先到；旧身份的原件与替换声明晚到也能完成核对。
        db::write(c.ctx.db(), |tx| {
            assert!(record(tx, new, hlc, &op)?);
            assert!(effective(tx, &key)?.is_none_or(|record| record.rank != DONE));
            let old_op = transfer(&listed.owner, &listed.id, &old, &listed.version, DONE);
            assert!(record(tx, &old, &original.hlc, &old_op)?);
            assert_eq!(effective(tx, &key)?, Some(original));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_new_identity_takes_back_the_jobs_it_handed_over() {
        let (a, table_a) = device();
        let (b, _table_b) = device();
        table_a.add(job("巡检")).unwrap();
        exchange(&a, &b);
        take(&b, false).unwrap();
        exchange(&b, &a);
        assert!(table_a.jobs().is_empty());
        // 原设备换了身份：回执没法以新身份重发，对方可能永远等不到——不等一天，这就收回。
        let old = a.settings.device.clone();
        db::write(a.ctx.db(), |tx| {
            on_rotate(tx, std::slice::from_ref(&old), "00000000000000ee")
        })
        .unwrap();
        assert!(testing::outbox(&a)
            .iter()
            .all(|op| !op["t"].as_str().unwrap_or_default().starts_with("cron.")));
        sync_files(&a);
        assert_eq!(names(&table_a), ["巡检"]);
        assert_eq!(notices(&a, "cron-reclaimed"), 1);
    }
}
