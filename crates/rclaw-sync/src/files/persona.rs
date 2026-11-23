//! 人格（§8.3）：生效人格 `config/SOUL.md`，和人格库 `config/souls/` 里用户自建的文件。
//!
//! 一个文件是一个单位，后写者胜。但远端的版本**不直接落盘**：它留在寄存器里，界面上列
//! 一条待处理事项，用户在本机看过差异、点了接受才写进文件（生效人格的改动下次启动
//! 生效）。拒绝则保留本机的版本——两端的人格可以不同，直到任一端再改。
//!
//! 基线（`file-base`）是上次同步后本机那份内容的摘要：本机内容与它不同就是本机改过，
//! 发布出去；本机在远端版本到达之后才改的，本机的更新，那条远端版本作废。

use std::collections::{BTreeMap, BTreeSet};

use rclaw_store::soul::{self, SYNC_ACTIVE_PATH};
use rusqlite::Connection;

use super::{
    file_stamp, keep_copy, mark, mtime_ms, publish, put_remote, rows, stamp_now, Cache,
    FilesReport, Row, Stamp, AWAITING, HANDLED,
};
use crate::crypto::sha256_hex;
use crate::db::{self, scope};
use crate::error::{Result, SyncError};
use crate::op::Op;
use crate::publish::Settings;
use crate::status::{self, PendingItem};
use crate::Ctx;

pub(crate) const KIND: &str = "persona";
/// 上次同步后本机内容的摘要：键是相对路径。
const SCOPE_BASE: &str = "file-base";
/// 用户拒绝过的远端版本：键是相对路径，值是那一版的摘要（删除记作 [`REJ_DEL`]）。同一版
/// 再到达时不再问。
const SCOPE_REJ: &str = "file-rej";
const REJ_DEL: &str = "del";
const CACHE_KEY: &str = "persona";

/// 合入时记一条 `file.put` / `file.del`。路径必须在白名单里：生效人格，或人格库里用户
/// 自建的一级文件（内置人格随各端程序下发，不同步）。
pub(crate) fn record(conn: &Connection, hlc: &str, op: &Op, stored: &str) -> Result<bool> {
    match op {
        Op::FilePut {
            path,
            hash,
            content,
        } => {
            if !soul::is_sync_path(path) {
                return Err(SyncError::invalid("不是可同步的人格文件"));
            }
            if content.trim().is_empty()
                || content.len() > rclaw_store::MAX_IMPORT_BYTES
                || content.contains('\0')
                || sha256_hex(content.as_bytes()) != *hash
            {
                return Err(SyncError::invalid("人格文件的内容不合规"));
            }
            put_remote(conn, scope::FILE, path, hlc, stored)
        }
        Op::FileDel { path, .. } => {
            if path == SYNC_ACTIVE_PATH || !soul::is_sync_path(path) {
                return Err(SyncError::invalid("不是可以删除的人格文件"));
            }
            put_remote(conn, scope::FILE, path, hlc, stored)
        }
        _ => Ok(false),
    }
}

/// 远端版本的摘要；删除为 `None`。
fn remote_hash(op: &Op) -> Option<&str> {
    match op {
        Op::FilePut { hash, .. } => Some(hash),
        _ => None,
    }
}

fn stamp(ctx: &Ctx) -> Stamp {
    let (len, mtime) = file_stamp(&ctx.layout.soul_file);
    let mut out = vec![(SYNC_ACTIVE_PATH.to_string(), len, mtime)];
    if let Ok(entries) = std::fs::read_dir(rclaw_store::souls_dir(&ctx.layout)) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    out.push((name, meta.len(), mtime_ms(&meta)));
                }
            }
        }
    }
    out.sort();
    out
}

/// 界面上怎么称呼一个人格文件。
fn describe(path: &str) -> String {
    match path.rsplit_once('/') {
        _ if path == SYNC_ACTIVE_PATH => "当前人格".to_string(),
        Some((_, name)) => format!("人格「{name}」"),
        None => format!("人格「{path}」"),
    }
}

/// 一个路径这一轮要做的事。
enum Step<'a> {
    /// 远端版本与本机相同，或是用户拒绝过的那一版：记为已处理。`base` 是要记下的基线
    /// （`None` = 不动基线）。
    Settle {
        row: &'a Row,
        base: Option<Option<String>>,
        /// 盘上实际命中的文件：只差大小写时和寄存器里的写法不同。
        local_path: String,
    },
    /// 本机的版本发布出去。
    Publish { op: Op, base: Option<String> },
    /// 远端版本等用户确认。
    Ask {
        row: &'a Row,
        /// 盘上实际命中的文件：确认页要显示它，接受/拒绝也按它来。
        local_path: String,
    },
    /// 没有可发布的东西了（生效人格被清空）：只把基线清掉。
    Forget,
}

fn set_base(conn: &Connection, path: &str, base: &Option<String>) -> Result<()> {
    match base {
        Some(hash) => db::state_put(conn, SCOPE_BASE, path, db::HLC_ZERO, None, Some(hash)),
        None => db::state_del(conn, SCOPE_BASE, path),
    }
}

/// 记基线。远端那个写法和盘上的写法指向同一个文件时两边都记：下一轮对比是从盘上的
/// 名字出发的，只记一边会让它看起来像本机的新改动，再发一遍。
fn set_base_for(
    conn: &Connection,
    path: &str,
    local_path: &str,
    base: &Option<String>,
) -> Result<()> {
    set_base(conn, path, base)?;
    if local_path != path {
        set_base(conn, local_path, base)?;
    }
    Ok(())
}

pub(crate) fn reconcile(
    ctx: &Ctx,
    s: &Settings,
    cache: &mut Cache,
    report: &mut FilesReport,
) -> Result<()> {
    let stamp = stamp(ctx);
    let (register, bases, rejected) = db::read(ctx.db(), |conn| {
        let values = |scope: &str| -> Result<BTreeMap<String, String>> {
            Ok(db::state_scan(conn, scope, "")?
                .into_iter()
                .filter_map(|(key, row)| Some((key, row.value?)))
                .collect())
        };
        Ok((
            rows(conn, scope::FILE, "")?,
            values(SCOPE_BASE)?,
            values(SCOPE_REJ)?,
        ))
    })?;
    if !register.iter().any(Row::is_new) && cache.is_current(CACHE_KEY, &stamp) {
        return Ok(());
    }
    let local: BTreeMap<String, String> = soul::sync_files(&ctx.layout)?.into_iter().collect();
    let register: BTreeMap<&str, &Row> =
        register.iter().map(|row| (row.key.as_str(), row)).collect();
    let paths: BTreeSet<&str> = local
        .keys()
        .map(String::as_str)
        .chain(register.keys().copied())
        .chain(bases.keys().map(String::as_str))
        .collect();
    // 基线可能记在远端那个写法上（旧版本只按原字符串记）：按本机落点建一份索引，
    // 盘上的名字也能找回同一份文件的基线，免得把它当成新改动再发一遍。
    let on_disk_bases: BTreeMap<String, &String> = bases
        .iter()
        .map(|(key, base)| (soul::sync_local_rel(&ctx.layout, key), base))
        .collect();

    let mut steps: Vec<(&str, Step)> = Vec::new();
    for path in paths {
        // 只差大小写的名字在本机不分大小写的卷上就是同一个文件（远端的 `notes.md` 落到
        // 盘上可能是已有的 `Notes.md`）。本机有没有原件、往哪写、删哪个，都按盘上那个
        // 文件判断；按原字符串判断会把本机的原件看成不存在，接受时静默覆盖。
        let local_path = soul::sync_local_rel(&ctx.layout, path);
        let aliased = local_path != path;
        let content = local.get(&local_path);
        let local_hash = content.map(|content| sha256_hex(content.as_bytes()));
        let base = bases
            .get(path)
            .or_else(|| bases.get(&local_path))
            .or_else(|| on_disk_bases.get(&local_path).copied());
        // 本机的版本能不能发布：有内容就能；删除只对人格库文件有意义。盘上的名字和远端
        // 给的不一样时不从这里发：本机那个名字自己会发，同一个改动不发两遍。
        let local_op = match content {
            Some(content) if !aliased => Some(Op::FilePut {
                path: path.to_string(),
                hash: local_hash.clone().unwrap_or_default(),
                content: content.clone(),
            }),
            None if !aliased && path != SYNC_ACTIVE_PATH => Some(Op::FileDel {
                path: path.to_string(),
                hash: base.cloned().unwrap_or_default(),
            }),
            _ => None,
        };
        let changed = local_hash.as_ref() != base;
        match register.get(path).copied().filter(|row| row.is_unapplied()) {
            Some(row) => {
                let remote = remote_hash(&row.op);
                let local_newer = match base {
                    // 上次同步之后本机也改过：本机的更新。
                    Some(_) => changed,
                    // 从没同步过的本机文件遇上一条早先的删除记录：那条删除说的不是它。
                    None => remote.is_none() && content.is_some(),
                };
                if remote == local_hash.as_deref() {
                    steps.push((
                        path,
                        Step::Settle {
                            row,
                            base: Some(local_hash),
                            local_path,
                        },
                    ));
                } else if let (true, Some(op)) = (local_newer, local_op) {
                    steps.push((
                        path,
                        Step::Publish {
                            op,
                            base: local_hash,
                        },
                    ));
                } else if aliased && local_newer {
                    // 本机这个文件的改动由盘上那个名字自己发布；远端这一版作废，把寄存器
                    // 收掉，免得每一轮都再问。
                    steps.push((
                        path,
                        Step::Settle {
                            row,
                            base: Some(local_hash),
                            local_path,
                        },
                    ));
                } else if rejected.get(path).map(String::as_str) == Some(remote.unwrap_or(REJ_DEL))
                {
                    // 没有基线的设备（刚加入）拒绝之后把基线记成本机当前的内容，免得反过来
                    // 把本机的版本推给别的设备。
                    let base = base.is_none().then_some(local_hash);
                    steps.push((
                        path,
                        Step::Settle {
                            row,
                            base,
                            local_path,
                        },
                    ));
                } else if row.is_new()
                    || status::get(&ctx.paths, &status::item_id(KIND, path)).is_none()
                {
                    // 刚到的版本；或者在等确认、而那条待处理事项不见了（补一条）。
                    steps.push((path, Step::Ask { row, local_path }));
                }
            }
            None if changed && !aliased => match local_op {
                Some(op) => steps.push((
                    path,
                    Step::Publish {
                        op,
                        base: local_hash,
                    },
                )),
                None => steps.push((path, Step::Forget)),
            },
            None => {}
        }
    }

    if !steps.is_empty() {
        let labels = db::write(ctx.db(), |tx| {
            let mut labels: Vec<String> = Vec::new();
            for (path, step) in &steps {
                match step {
                    Step::Settle {
                        row,
                        base,
                        local_path,
                    } => {
                        mark(tx, scope::FILE, &row.key, &row.hlc, HANDLED)?;
                        if let Some(base) = base {
                            set_base_for(tx, path, local_path, base)?;
                        }
                    }
                    Step::Publish { op, base } => {
                        publish(tx, s, scope::FILE, path, op)?;
                        set_base(tx, path, base)?;
                    }
                    Step::Ask { row, .. } => {
                        mark(tx, scope::FILE, &row.key, &row.hlc, AWAITING)?;
                        labels.push(db::device_label(tx, &row.origin)?);
                    }
                    Step::Forget => db::state_del(tx, SCOPE_BASE, path)?,
                }
            }
            Ok(labels)
        })?;
        let mut labels = labels.into_iter();
        for (path, step) in &steps {
            let id = status::item_id(KIND, path);
            match step {
                Step::Ask { row, local_path } => {
                    let label = labels.next().unwrap_or_default();
                    let what = describe(path);
                    let (title, op) = match &row.op {
                        Op::FilePut { .. } if local.contains_key(local_path.as_str()) => {
                            (format!("「{label}」修改了{what}"), "put")
                        }
                        Op::FilePut { .. } => (format!("「{label}」新增了{what}"), "put"),
                        _ => (format!("「{label}」删除了{what}"), "del"),
                    };
                    let mut detail = if *path == SYNC_ACTIVE_PATH {
                        "人格决定 agent 的行为方式，所以别的设备上的改动要你在这台设备上看过、\
                         确认之后才会写入；接受后在下次启动 RClaw 时生效。拒绝则这台设备继续用现在的人格。"
                            .to_string()
                    } else {
                        "人格库里的文件，确认之后才会写入这台设备。拒绝则这台设备保持现状。"
                            .to_string()
                    };
                    // 本机命中的文件和远端给的名字只差大小写：写明接受后真正被动到的是哪个。
                    if local_path.as_str() != *path {
                        if let Some((_, name)) = local_path.rsplit_once('/') {
                            let action = if op == "put" {
                                "被对方的内容覆盖"
                            } else {
                                "被删除"
                            };
                            detail.push_str(&format!(
                                "\n这台设备上命中的是已有的「{name}」：接受后它会{action}，\
                                 原内容会留一份副本。"
                            ));
                        }
                    }
                    let item = PendingItem {
                        id,
                        kind: KIND.into(),
                        title,
                        detail,
                        origin: Some(label),
                        at_ms: crate::attach::now_ms(),
                        actions: vec!["accept".into(), "reject".into()],
                        payload: serde_json::json!({ "path": path, "op": op, "hlc": row.hlc }),
                    };
                    status::put(&ctx.paths, &item)?;
                    report.status_changed = true;
                }
                Step::Publish { .. } => {
                    report.published += 1;
                    report.status_changed |= status::remove(&ctx.paths, &id);
                }
                Step::Settle { .. } | Step::Forget => {
                    report.status_changed |= status::remove(&ctx.paths, &id);
                }
            }
        }
    }
    cache.put(CACHE_KEY, stamp);
    Ok(())
}

/// 待处理事项对应的寄存器行；事项已经过期（来了更新的远端版本，或已经处理过）时把它
/// 清掉并报错，界面刷新后会看到最新的。
fn pending_row(ctx: &Ctx, item: &PendingItem) -> Result<(String, Row)> {
    let path = item.payload["path"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let found = db::read(ctx.db(), |conn| super::row(conn, scope::FILE, &path))?;
    match found {
        Some(row)
            if row.is_unapplied() && Some(row.hlc.as_str()) == item.payload["hlc"].as_str() =>
        {
            Ok((path, row))
        }
        _ => {
            status::remove(&ctx.paths, &item.id);
            Err(SyncError::invalid("这项改动已经有了更新的版本，请重新查看"))
        }
    }
}

/// 差异对比用的两份内容。
pub(crate) fn detail(ctx: &Ctx, item: &PendingItem) -> Result<serde_json::Value> {
    let (path, row) = pending_row(ctx, item)?;
    // 本机落点可能和远端给的名字只差大小写：确认页要显示真正命中的那个文件。
    let local_path = soul::sync_local_rel(&ctx.layout, &path);
    let local = soul::sync_files(&ctx.layout)?
        .into_iter()
        .find(|(rel, _)| *rel == local_path)
        .map(|(_, content)| content);
    let remote = match row.op {
        Op::FilePut { content, .. } => Some(content),
        _ => None,
    };
    Ok(serde_json::json!({ "path": path, "local_path": local_path, "local": local, "remote": remote }))
}

/// 用户在本机接受或拒绝了一个远端版本。
pub(crate) fn decide(ctx: &Ctx, _s: &Settings, item: &PendingItem, accept: bool) -> Result<()> {
    let (path, row) = pending_row(ctx, item)?;
    // 只差大小写的名字在不分大小写的卷上就是同一个文件：备份、写入、删除都按盘上那个
    // 文件来，不能按远端给的写法另起一份（或者当成本机没有）。
    let local_path = soul::sync_local_rel(&ctx.layout, &path);
    let local = soul::sync_files(&ctx.layout)?
        .into_iter()
        .find(|(rel, _)| *rel == local_path)
        .map(|(_, content)| content);
    let local_hash = local.as_ref().map(|content| sha256_hex(content.as_bytes()));
    let remote = remote_hash(&row.op).map(str::to_string);
    let aliased = local_path != path;

    if accept {
        // 被替换、被删除的人格库文件留一个副本；生效人格由 `sync_write` 自己备份。
        if let (Some(content), Some((_, name))) = (&local, local_path.rsplit_once('/')) {
            if local_path != SYNC_ACTIVE_PATH {
                keep_copy(
                    ctx,
                    "souls",
                    &format!("{name}.{}", stamp_now()),
                    content.as_bytes(),
                );
            }
        }
        match &row.op {
            Op::FilePut { content, .. } => {
                soul::sync_write(&ctx.layout, &local_path, content)?;
            }
            _ => {
                soul::sync_remove(&ctx.layout, &local_path)?;
            }
        }
    }
    db::write(ctx.db(), |tx| {
        mark(tx, scope::FILE, &row.key, &row.hlc, HANDLED)?;
        if accept {
            set_base_for(tx, &path, &local_path, &remote)?;
            db::state_del(tx, SCOPE_REJ, &path)?;
            if aliased {
                db::state_del(tx, SCOPE_REJ, &local_path)?;
            }
            Ok(())
        } else {
            let rejected = remote.as_deref().unwrap_or(REJ_DEL);
            db::state_put(tx, SCOPE_REJ, &path, db::HLC_ZERO, None, Some(rejected))?;
            if aliased {
                db::state_put(tx, SCOPE_REJ, &local_path, db::HLC_ZERO, None, Some(rejected))?;
            }
            // 还没有基线（刚加入的设备）：记成本机当前的内容，免得反过来把本机的版本推给
            // 别的设备。之后本机再改，照常发布。
            if db::state_get(tx, SCOPE_BASE, &path)?.is_none() {
                set_base(tx, &path, &local_hash)?;
            }
            // 盘上那个写法的基线也要记：下一轮对比是从它出发的。
            if aliased && db::state_get(tx, SCOPE_BASE, &local_path)?.is_none() {
                set_base(tx, &local_path, &local_hash)?;
            }
            Ok(())
        }
    })?;
    status::remove(&ctx.paths, &item.id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::{self, Cache};
    use crate::testing::{self, Device};

    /// 记一条远端来的 op：寄存器里的值照合入时那样现算。
    fn record(conn: &Connection, origin: &str, hlc: &str, op: &Op) -> Result<bool> {
        super::record(conn, hlc, op, &files::stored_text(origin, op)?)
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

    fn soul(device: &Device) -> String {
        std::fs::read_to_string(&device.ctx.layout.soul_file).unwrap_or_default()
    }

    fn set_soul(device: &Device, content: &str) {
        std::fs::create_dir_all(&device.ctx.layout.config_dir).unwrap();
        std::fs::write(&device.ctx.layout.soul_file, content).unwrap();
    }

    fn library(device: &Device, name: &str) -> Option<String> {
        std::fs::read_to_string(rclaw_store::souls_dir(&device.ctx.layout).join(name)).ok()
    }

    fn set_library(device: &Device, name: &str, content: &str) {
        let dir = rclaw_store::souls_dir(&device.ctx.layout);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), content).unwrap();
    }

    fn pending(device: &Device) -> Vec<PendingItem> {
        status::list(&device.ctx.paths)
            .into_iter()
            .filter(|item| item.kind == KIND)
            .collect()
    }

    fn decide_first(device: &Device, action: &str) {
        let item = pending(device).remove(0);
        files::decide(
            &device.ctx,
            &device.settings,
            &item.id,
            action,
            item.version(),
        )
        .unwrap();
    }

    fn puts(device: &Device) -> usize {
        testing::outbox(device)
            .iter()
            .filter(|op| op["t"] == "file.put")
            .count()
    }

    #[test]
    fn a_remote_persona_waits_for_local_confirmation() {
        let a = Device::new();
        let b = Device::new();
        set_soul(&a, "你是甲端调教出来的助手。\n");
        set_soul(&b, "你是乙端原来的助手。\n");
        // 乙端先同步过一轮（有了基线）。
        sync_files(&b);
        testing::seal_all(&b);
        exchange(&a, &b);
        // 确认之前不落盘。
        assert_eq!(soul(&b), "你是乙端原来的助手。\n");
        let items = pending(&b);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].actions, ["accept", "reject"]);
        let detail = files::detail(&b.ctx, &items[0].id).unwrap();
        assert_eq!(detail["local"], "你是乙端原来的助手。\n");
        assert_eq!(detail["remote"], "你是甲端调教出来的助手。\n");
        // 再对比多少轮都还是这一条，不重复提示。
        sync_files(&b);
        assert_eq!(pending(&b).len(), 1);

        decide_first(&b, "accept");
        assert_eq!(soul(&b), "你是甲端调教出来的助手。\n");
        assert!(pending(&b).is_empty());
        // 旧的生效人格有备份。
        let backups = std::fs::read_dir(&b.ctx.layout.config_dir)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("SOUL.md.bak.")
            })
            .count();
        assert_eq!(backups, 1);
        // 接受下来的版本不会被当成本机的改动再发出去。
        let before = puts(&b);
        sync_files(&b);
        assert_eq!(puts(&b), before);
    }

    #[test]
    fn rejecting_keeps_the_local_persona_until_someone_edits_again() {
        let a = Device::new();
        let b = Device::new();
        set_soul(&a, "甲端的人格。\n");
        set_soul(&b, "乙端的人格。\n");
        exchange(&a, &b);
        // 乙端刚加入、没有基线：也要确认，而且本机的版本不会抢先发出去。
        assert_eq!(pending(&b).len(), 1);
        assert_eq!(puts(&b), 0);
        decide_first(&b, "reject");
        assert_eq!(soul(&b), "乙端的人格。\n");
        sync_files(&b);
        assert!(pending(&b).is_empty());
        assert_eq!(puts(&b), 0);
        // 同一版再到达（重放）不再问。
        testing::replay(&a, &b);
        b.merge();
        sync_files(&b);
        assert!(pending(&b).is_empty());
        // 乙端自己再改：发布出去，到甲端等确认。
        set_soul(&b, "乙端改过的人格。\n");
        exchange(&b, &a);
        assert_eq!(soul(&a), "甲端的人格。\n");
        assert_eq!(pending(&a).len(), 1);
    }

    #[test]
    fn a_local_edit_after_the_remote_one_wins() {
        let a = Device::new();
        let b = Device::new();
        set_soul(&a, "第一版。\n");
        exchange(&a, &b);
        decide_first(&b, "accept");
        // 甲端改了第二版；乙端在它到达之后、确认之前自己也改了。
        set_soul(&a, "甲端的第二版。\n");
        exchange(&a, &b);
        assert_eq!(pending(&b).len(), 1);
        set_soul(&b, "乙端的第二版。\n");
        sync_files(&b);
        assert!(pending(&b).is_empty());
        assert_eq!(soul(&b), "乙端的第二版。\n");
        exchange(&b, &a);
        assert_eq!(pending(&a).len(), 1);
    }

    #[test]
    fn library_files_sync_but_builtins_and_markers_do_not() {
        let a = Device::new();
        let b = Device::new();
        set_library(&a, "我的人格.md", "自建的人格。\n");
        let builtin = rclaw_store::BUILTIN_SOULS[0].file_name;
        set_library(&a, builtin, "改过的内置人格。\n");
        set_library(&a, ".active", "我的人格.md");
        sync_files(&a);
        let sent: Vec<String> = testing::outbox(&a)
            .iter()
            .filter(|op| op["t"] == "file.put")
            .map(|op| op["path"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(sent, ["config/souls/我的人格.md"]);
        exchange(&a, &b);
        let items = pending(&b);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].payload["path"], "config/souls/我的人格.md");
        decide_first(&b, "accept");
        assert_eq!(library(&b, "我的人格.md").unwrap(), "自建的人格。\n");
        assert!(library(&b, builtin).is_none());
        assert!(library(&b, ".active").is_none());

        // 甲端删掉这个文件：乙端确认之后才删，并留下副本。
        std::fs::remove_file(rclaw_store::souls_dir(&a.ctx.layout).join("我的人格.md")).unwrap();
        exchange(&a, &b);
        assert!(library(&b, "我的人格.md").is_some());
        decide_first(&b, "accept");
        assert!(library(&b, "我的人格.md").is_none());
        let copies = std::fs::read_dir(b.ctx.paths.conflicts.join("souls"))
            .unwrap()
            .count();
        assert_eq!(copies, 1);
    }

    #[test]
    fn paths_outside_the_whitelist_are_refused() {
        let (_g, ctx) = testing::ctx();
        let _ = testing::settings(&ctx);
        let hlc = "1763438400000-0001-0123456789abcdef";
        let put = |path: &str, content: &str| Op::FilePut {
            path: path.into(),
            hash: sha256_hex(content.as_bytes()),
            content: content.into(),
        };
        db::write(ctx.db(), |tx| {
            for path in [
                "rclaw.toml",
                "config/souls/../SOUL.md",
                "config/souls/.active",
                "skills/x/SKILL.md",
            ] {
                assert!(matches!(
                    record(tx, "0123456789abcdef", hlc, &put(path, "内容")),
                    Err(SyncError::Invalid(_))
                ));
            }
            // 摘要对不上、删除生效人格同样拒绝。
            let mut wrong = put("config/SOUL.md", "内容");
            if let Op::FilePut { hash, .. } = &mut wrong {
                *hash = sha256_hex(b"other");
            }
            assert!(record(tx, "0123456789abcdef", hlc, &wrong).is_err());
            let del = Op::FileDel {
                path: "config/SOUL.md".into(),
                hash: String::new(),
            };
            assert!(record(tx, "0123456789abcdef", hlc, &del).is_err());
            assert!(record(tx, "0123456789abcdef", hlc, &put("config/SOUL.md", "内容")).unwrap());
            Ok(())
        })
        .unwrap();
    }
}
