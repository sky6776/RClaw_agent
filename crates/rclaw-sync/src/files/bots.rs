//! bot 定义（§8.5）：`bots/<name>.toml`，整份定义后写者胜。bot 自己的记忆走记忆条目那
//! 一套（`memory.rs`），bot 会话与群聊房间不同步。
//!
//! 远端的定义经 `BotDef` 解析（不认识的字段即拒绝，升级后重试）后直接落到本机，只有
//! 一种情况要用户在本机确认：它比本机现有的定义**放宽了工具限制**（`denied_tools` 少了
//! 东西）。「本机现有的定义」也包括刚被别的设备删掉的那一份：远端的删除落地时记下它禁用
//! 的工具（[`SCOPE_GONE`]），之后同名的定义再来就拿它来比——不然「先删掉、再建一个不禁
//! 工具的」就绕过了确认。
//!
//! 远端的定义换掉本机一份不一样的定义时（包括刚加入同步空间的设备上与空间里同名的
//! bot：以空间里的为准），本机原来的那份留一个副本在 `<数据根>/sync/conflicts/bots/`：
//! 两台设备各自改了同一个 bot 时后同步的那一份胜出，输掉的那份在这里找得回来。

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rclaw_store::{is_valid_bot_name, BotDef, BotStore};
use rusqlite::Connection;

use super::{
    keep_copy, mark, memory, mtime_ms, publish, put_remote, rows, stamp_now, Cache, FilesReport,
    Row, Stamp, AWAITING, HANDLED, REJECTED,
};
use crate::crypto::sha256_hex;
use crate::db::{self, scope};
use crate::error::{Result, SyncError};
use crate::op::Op;
use crate::publish::Settings;
use crate::status::{self, PendingItem};
use crate::{Ctx, Reload};

pub(crate) const KIND: &str = "bot";
/// 上次同步后本机定义的摘要：键是 bot 名。
const SCOPE_BASE: &str = "bot-base";
/// 用户拒绝过的远端版本：键是 bot 名，值是那一版的摘要。
const SCOPE_REJ: &str = "bot-rej";
/// 被别的设备删掉的 bot 在本机原来禁用的工具：键是 bot 名，值是工具名的 JSON 数组。这个
/// 名字在本机又有了定义（远端的落地了，或者本机自己建了）之后清掉。
const SCOPE_GONE: &str = "bot-gone";
/// 定义文件的大小上限。
const DEF_MAX: usize = 256 * 1024;
const CACHE_KEY: &str = "bots";

/// 合入时记一条 `bot.put` / `bot.del`。这里只查外壳；定义本身到落地时才解析——现在解析
/// 不了的（别的设备版本更新）留在寄存器里，本机升级之后还能用上。
pub(crate) fn record(conn: &Connection, hlc: &str, op: &Op, stored: &str) -> Result<bool> {
    let name = match op {
        Op::BotPut { name, def, .. } => {
            if def.len() > DEF_MAX {
                return Err(SyncError::invalid("bot 定义过大"));
            }
            name
        }
        Op::BotDel { name, .. } => name,
        _ => return Ok(false),
    };
    if !is_valid_bot_name(name) {
        return Err(SyncError::invalid("bot 名不合规"));
    }
    put_remote(conn, scope::BOT, name, hlc, stored)
}

/// 与 `BotStore::save` 相同的规整，让同一份定义在各端算出同一个摘要。
fn normalize(mut def: BotDef) -> BotDef {
    def.title = def.title.trim().to_string();
    def.description = def.description.trim().to_string();
    def.persona = def.persona.trim().to_string();
    def.denied_tools.sort();
    def.denied_tools.dedup();
    if def
        .provider
        .as_deref()
        .is_some_and(|name| name.trim().is_empty())
    {
        def.provider = None;
    }
    def
}

/// 定义的摘要：规整后的 JSON（键有序、不含空值）。不拿 TOML 文本算——那取决于排版。
fn def_hash(def: &BotDef) -> String {
    let mut value = serde_json::to_value(def).unwrap_or_default();
    if let Some(fields) = value.as_object_mut() {
        fields.retain(|_, field| !field.is_null());
    }
    sha256_hex(value.to_string().as_bytes())
}

fn def_text(def: &BotDef) -> Result<String> {
    toml::to_string_pretty(def).map_err(|e| SyncError::other(format!("bot 定义序列化失败: {e}")))
}

/// 解析远端来的定义。失败的原因给用户看。
fn parse(text: &str) -> std::result::Result<BotDef, String> {
    let def = normalize(toml::from_str::<BotDef>(text).map_err(|e| e.to_string())?);
    if def.title.is_empty() || def.persona.is_empty() {
        return Err("显示名或人格为空".into());
    }
    Ok(def)
}

fn bots_dir(ctx: &Ctx) -> PathBuf {
    ctx.layout.base.join("bots")
}

/// `bots/` 下名字合规的定义文件：`(bot 名, 路径, 长度, 修改时间)`。
///
/// 只有「目录不存在」算一个 bot 都没有。别的错（目录一时列不出来、某一项的属性看不了）
/// 都往上报，这一轮不对比 bot：把「看不见」当成「删掉了」的话，`bot.del` 和这个 bot 每一条
/// 记忆的删除就会发给所有设备。
fn def_files(ctx: &Ctx) -> Result<Vec<(String, PathBuf, u64, i64)>> {
    let unreadable =
        |error: std::io::Error| SyncError::other(format!("读取 bot 定义目录失败: {error}"));
    let entries = match std::fs::read_dir(bots_dir(ctx)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(unreadable(error)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(unreadable)?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if !is_valid_bot_name(name) {
            continue;
        }
        let is_link = entry.file_type().map_err(unreadable)?.is_symlink();
        // 跟着符号链接看：`BotStore` 也是照着路径直接读的，链接指向的定义在程序里是个
        // 正常的 bot。
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {
                out.push((name.to_string(), path, meta.len(), mtime_ms(&meta)));
            }
            Ok(_) => {}
            // 链接指向的东西现在看不到（那个盘没挂上等）：算「在、但读不了」，和手工改坏
            // 的定义一样这一轮不碰（`read_def` 读不出来）。
            Err(_) if is_link => out.push((name.to_string(), path, 0, 0)),
            // 刚列出来就被删了。
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(unreadable(error)),
        }
    }
    out.sort();
    Ok(out)
}

/// 本机的一份定义。`None` = 文件在、但读不懂（多半是手工改坏了）：这一轮不碰它，既不
/// 发布也不覆盖——不能把「读不懂」当成「删掉了」传给别的设备。
fn read_def(path: &std::path::Path) -> Option<BotDef> {
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str::<BotDef>(&text).ok().map(normalize)
}

/// 远端的定义比 `denied`（本机的定义禁用的工具）少禁了哪些。
fn relaxed(denied: &[String], remote: &BotDef) -> Vec<String> {
    denied
        .iter()
        .filter(|tool| !remote.denied_tools.contains(tool))
        .cloned()
        .collect()
}

/// 一个 bot 这一轮要做的事。
enum Step<'a> {
    /// 远端版本与本机相同，或是用户拒绝过的那一版：记为已处理。`base` 是要记下的基线
    /// （`None` = 不动基线）。
    Settle {
        row: &'a Row,
        base: Option<Option<String>>,
    },
    /// 本机的版本（或本机的删除）发布出去。
    Publish { op: Op, base: Option<String> },
    /// 远端的版本落到本机；`None` = 删除。本机原来的那份留一个副本。
    Apply {
        row: &'a Row,
        remote: Option<(BotDef, String)>,
    },
    /// 远端的版本放宽了工具限制：等用户确认。`recreated` = 比的是这个 bot 被别的设备删掉
    /// 之前的限制（本机现在没有它）。
    Ask {
        row: &'a Row,
        hash: String,
        relaxed: Vec<String>,
        recreated: bool,
    },
    /// 远端的版本本机读不懂。
    Reject { row: &'a Row, reason: String },
}

fn set_base(conn: &Connection, name: &str, base: &Option<String>) -> Result<()> {
    match base {
        Some(hash) => db::state_put(conn, SCOPE_BASE, name, db::HLC_ZERO, None, Some(hash)),
        None => db::state_del(conn, SCOPE_BASE, name),
    }
}

/// 把本机的一份定义留个副本。
fn keep_def(ctx: &Ctx, name: &str, def: &BotDef) {
    if let Ok(text) = def_text(def) {
        keep_copy(
            ctx,
            "bots",
            &format!("{name}.{}.toml", stamp_now()),
            text.as_bytes(),
        );
    }
}

/// 把远端的版本落到本机，本机原来的那份（`local`）留一个副本。失败的原因给用户看。
fn apply(
    ctx: &Ctx,
    name: &str,
    local: Option<&BotDef>,
    remote: &Option<(BotDef, String)>,
) -> std::result::Result<(), String> {
    let store = BotStore::new(&ctx.layout.base);
    match remote {
        Some((def, _)) => {
            // 被换掉的那份留个副本：两台设备各自改了同一个 bot 时，后同步的那一份胜出，
            // 输掉的那份不能就这么没了。
            if let Some(local) = local.filter(|local| def_hash(local) != def_hash(def)) {
                keep_def(ctx, name, local);
            }
            store.save(name, def.clone()).map_err(|e| e.to_string())
        }
        None => {
            // 别的设备删了这个 bot：定义和它的数据目录（记忆）挪进冲突副本，后悔了还能
            // 取回。挪不动（不在同一个盘上等）就随定义一起删。
            if let Some(local) = local {
                keep_def(ctx, name, local);
            }
            let data = bots_dir(ctx).join(name);
            if data.is_dir() {
                let kept = ctx.paths.conflicts.join("bots");
                let moved = std::fs::create_dir_all(&kept).and_then(|()| {
                    std::fs::rename(&data, kept.join(format!("{name}.{}", stamp_now())))
                });
                if let Err(error) = moved {
                    tracing::warn!("保留 bot「{name}」的数据目录失败: {error}");
                }
            }
            store.delete(name).map(|_| ()).map_err(|e| e.to_string())
        }
    }
}

pub(crate) fn reconcile(
    ctx: &Ctx,
    s: &Settings,
    cache: &mut Cache,
    report: &mut FilesReport,
) -> Result<()> {
    let files = def_files(ctx)?;
    let stamp: Stamp = files
        .iter()
        .map(|(name, _, len, mtime)| (name.clone(), *len, *mtime))
        .collect();
    let (register, bases, rejected, gone) = db::read(ctx.db(), |conn| {
        let values = |scope: &str| -> Result<BTreeMap<String, String>> {
            Ok(db::state_scan(conn, scope, "")?
                .into_iter()
                .filter_map(|(key, row)| Some((key, row.value?)))
                .collect())
        };
        let gone: BTreeMap<String, Vec<String>> = db::state_scan(conn, SCOPE_GONE, "")?
            .into_iter()
            .filter_map(|(key, row)| Some((key, row.json()?)))
            .collect();
        Ok((
            rows(conn, scope::BOT, "")?,
            values(SCOPE_BASE)?,
            values(SCOPE_REJ)?,
            gone,
        ))
    })?;
    if !register.iter().any(Row::is_new) && cache.is_current(CACHE_KEY, &stamp) {
        return Ok(());
    }
    let local: BTreeMap<&str, Option<BotDef>> = files
        .iter()
        .map(|(name, path, _, _)| (name.as_str(), read_def(path)))
        .collect();
    let register: BTreeMap<&str, &Row> =
        register.iter().map(|row| (row.key.as_str(), row)).collect();
    let names: BTreeSet<&str> = local
        .keys()
        .copied()
        .chain(register.keys().copied())
        .chain(bases.keys().map(String::as_str))
        .collect();

    let mut steps: Vec<(&str, Step)> = Vec::new();
    for name in names {
        let def = match local.get(name) {
            Some(None) => continue,
            Some(Some(def)) => Some(def),
            None => None,
        };
        let local_hash = def.map(def_hash);
        let base = bases.get(name);
        let changed = local_hash.as_ref() != base;
        // 本机的版本：有定义就发定义；没有了，而且以前同步过，发删除。
        let local_op = || -> Result<Option<Op>> {
            Ok(match (def, base) {
                (Some(def), _) => Some(Op::BotPut {
                    name: name.to_string(),
                    hash: local_hash.clone().unwrap_or_default(),
                    def: def_text(def)?,
                }),
                (None, Some(base)) => Some(Op::BotDel {
                    name: name.to_string(),
                    hash: base.clone(),
                }),
                (None, None) => None,
            })
        };
        let Some(row) = register.get(name).copied().filter(|row| row.is_unapplied()) else {
            if changed {
                if let Some(op) = local_op()? {
                    steps.push((
                        name,
                        Step::Publish {
                            op,
                            base: local_hash,
                        },
                    ));
                }
            }
            continue;
        };
        let remote = match &row.op {
            Op::BotPut { def, .. } => match parse(def) {
                Ok(def) => {
                    let hash = def_hash(&def);
                    Some((def, hash))
                }
                Err(reason) => {
                    if row.is_new() {
                        steps.push((name, Step::Reject { row, reason }));
                    }
                    continue;
                }
            },
            _ => None,
        };
        let remote_hash = remote.as_ref().map(|(_, hash)| hash);
        let local_newer = match base {
            // 上次同步之后本机也改过：本机的更新。
            Some(_) => changed,
            // 从没同步过的本机 bot 遇上一条早先的删除记录：那条删除说的不是它。
            None => remote.is_none() && def.is_some(),
        };
        if remote_hash == local_hash.as_ref() {
            steps.push((
                name,
                Step::Settle {
                    row,
                    base: Some(local_hash),
                },
            ));
        } else if local_newer {
            if let Some(op) = local_op()? {
                steps.push((
                    name,
                    Step::Publish {
                        op,
                        base: local_hash,
                    },
                ));
            }
        } else if remote_hash.is_some() && rejected.get(name) == remote_hash {
            let base = base.is_none().then_some(local_hash);
            steps.push((name, Step::Settle { row, base }));
        } else {
            let (relaxed, recreated) = match (def, &remote) {
                (Some(local), Some((remote, _))) => (relaxed(&local.denied_tools, remote), false),
                // 本机没有这个 bot、而它是被别的设备删掉的：拿它被删之前禁用的工具来比。
                (None, Some((remote, _))) => match gone.get(name) {
                    Some(denied) => (relaxed(denied, remote), true),
                    None => (Vec::new(), false),
                },
                _ => (Vec::new(), false),
            };
            match remote_hash.filter(|_| !relaxed.is_empty()) {
                Some(hash) => {
                    if row.is_new()
                        || status::get(&ctx.paths, &status::item_id(KIND, name)).is_none()
                    {
                        let hash = hash.clone();
                        steps.push((
                            name,
                            Step::Ask {
                                row,
                                hash,
                                relaxed,
                                recreated,
                            },
                        ));
                    }
                }
                // 本机有一份不一样的定义（刚加入的设备上同名的 bot，或者别的设备后改的
                // 胜出）：以远端的为准，本机的留副本。
                None => steps.push((name, Step::Apply { row, remote })),
            }
        }
    }
    if steps.is_empty() {
        cache.put(CACHE_KEY, stamp);
        return Ok(());
    }

    // 文件操作在库锁之外做完，再一次记账。
    let mut touched = false;
    let mut failures: Vec<Option<String>> = Vec::new();
    for (name, step) in &steps {
        failures.push(match step {
            Step::Apply { remote, .. } => {
                let def = local.get(name).and_then(Option::as_ref);
                touched = true;
                apply(ctx, name, def, remote).err()
            }
            Step::Reject { reason, .. } => Some(reason.clone()),
            _ => None,
        });
    }
    let labels = db::write(ctx.db(), |tx| {
        let mut labels: Vec<String> = Vec::new();
        // 本机又有了定义的名字：之后拿这份定义来比，删除时记下的那份用不着了。
        for name in gone.keys() {
            if matches!(local.get(name.as_str()), Some(Some(_))) {
                db::state_del(tx, SCOPE_GONE, name)?;
            }
        }
        for ((name, step), failure) in steps.iter().zip(&failures) {
            match step {
                Step::Settle { row, base } => {
                    mark(tx, scope::BOT, &row.key, &row.hlc, HANDLED)?;
                    if let Some(base) = base {
                        set_base(tx, name, base)?;
                    }
                }
                Step::Publish { op, base } => {
                    publish(tx, s, scope::BOT, name, op)?;
                    set_base(tx, name, base)?;
                    if matches!(op, Op::BotDel { .. }) {
                        memory::retire_bot(tx, s, name)?;
                    }
                }
                Step::Apply { row, remote, .. } if failure.is_none() => {
                    mark(tx, scope::BOT, &row.key, &row.hlc, HANDLED)?;
                    set_base(tx, name, &remote.as_ref().map(|(_, hash)| hash.clone()))?;
                    db::state_del(tx, SCOPE_REJ, name)?;
                    match remote {
                        // 这个名字在本机又有定义了。
                        Some(_) => db::state_del(tx, SCOPE_GONE, name)?,
                        None => {
                            memory::forget_bot(tx, name)?;
                            // 记下被删掉的这份禁用了哪些工具：之后同名的定义再来，不能
                            // 因为「本机没有它」就不问。本机本来就没有它的（已经删过一
                            // 次），上次记下的留着。
                            let denied = local
                                .get(name)
                                .and_then(Option::as_ref)
                                .map(|def| &def.denied_tools)
                                .filter(|denied| !denied.is_empty());
                            if let Some(denied) = denied {
                                db::state_put_json(tx, SCOPE_GONE, name, db::HLC_ZERO, denied)?;
                            }
                        }
                    }
                }
                Step::Apply { row, .. } | Step::Reject { row, .. } => {
                    mark(tx, scope::BOT, &row.key, &row.hlc, REJECTED)?;
                    labels.push(db::device_label(tx, &row.origin)?);
                }
                Step::Ask { row, .. } => {
                    mark(tx, scope::BOT, &row.key, &row.hlc, AWAITING)?;
                    labels.push(db::device_label(tx, &row.origin)?);
                }
            }
        }
        Ok(labels)
    })?;

    let mut labels = labels.into_iter();
    for ((name, step), failure) in steps.iter().zip(&failures) {
        let id = status::item_id(KIND, name);
        match (step, failure) {
            (
                Step::Ask {
                    row,
                    hash,
                    relaxed,
                    recreated,
                },
                _,
            ) => {
                let label = labels.next().unwrap_or_default();
                let tools = relaxed.join("、");
                let (title, detail) = if *recreated {
                    (
                        format!("「{label}」重新建了 bot「{name}」，工具限制比原来宽"),
                        format!(
                            "这个 bot 之前被别的设备删掉了，现在又建了出来；和它在这台设备上被删\
                             之前相比，新的定义不再禁用这些工具：{tools}。放宽工具限制的改动要你在\
                             这台设备上确认之后才会生效；拒绝则这台设备上不会有这个 bot。"
                        ),
                    )
                } else {
                    (
                        format!("「{label}」放宽了 bot「{name}」的工具限制"),
                        format!(
                            "这份定义不再禁用这些工具：{tools}。放宽工具限制的改动要你在这台设备上\
                             确认之后才会生效；拒绝则这台设备继续用现在的定义。"
                        ),
                    )
                };
                let item = PendingItem {
                    id,
                    kind: KIND.into(),
                    title,
                    detail,
                    origin: Some(label),
                    at_ms: crate::attach::now_ms(),
                    actions: vec!["accept".into(), "reject".into()],
                    payload: serde_json::json!({
                        "name": name,
                        "hash": hash,
                        "hlc": row.hlc,
                        "relaxed": relaxed,
                        "recreated": recreated,
                    }),
                };
                status::put(&ctx.paths, &item)?;
                report.status_changed = true;
            }
            (Step::Apply { row, .. } | Step::Reject { row, .. }, Some(reason)) => {
                let label = labels.next().unwrap_or_default();
                status::notice(
                    &ctx.paths,
                    "bot-rejected",
                    &format!("{name}/{}", row.hlc),
                    &format!("来自「{label}」的 bot「{name}」没有同步到本机"),
                    &format!(
                        "这份定义本机用不了（{reason}）。如果那台设备上的 RClaw 版本更新，\
                         升级本机并重启之后会再试一次。"
                    ),
                );
                report.status_changed = true;
            }
            (Step::Apply { .. }, None) => {
                report.applied += 1;
                report.reload.insert(Reload::Bots);
                report.status_changed |= status::remove(&ctx.paths, &id);
            }
            (Step::Publish { op, .. }, _) => {
                report.published += 1;
                report.status_changed |= status::remove(&ctx.paths, &id);
                if matches!(op, Op::BotDel { .. }) {
                    // 这个 bot 的记忆文件已经不在了，记忆那边的指纹跟着作废。
                    cache.forget(&format!("mem/bot:{name}:memory"));
                    cache.forget(&format!("mem/bot:{name}:user"));
                }
            }
            (Step::Settle { .. }, _) => {
                report.status_changed |= status::remove(&ctx.paths, &id);
            }
            (Step::Reject { .. }, None) => {}
        }
    }
    if touched {
        // 文件刚被这里改过：这一轮的指纹不留，下一轮再对比一次。
        cache.forget(CACHE_KEY);
    } else {
        cache.put(CACHE_KEY, stamp);
    }
    Ok(())
}

/// 待处理事项对应的寄存器行和它里面的定义；事项已经过期（来了更新的远端版本，或已经
/// 处理过）时把它清掉并报错，界面刷新后会看到最新的。
fn pending_row(ctx: &Ctx, item: &PendingItem) -> Result<(String, Row, BotDef, String)> {
    let name = item.payload["name"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let found = db::read(ctx.db(), |conn| super::row(conn, scope::BOT, &name))?;
    if let Some(row) = found {
        if row.is_unapplied() && Some(row.hlc.as_str()) == item.payload["hlc"].as_str() {
            if let Op::BotPut { def, .. } = &row.op {
                if let Ok(def) = parse(def) {
                    let hash = def_hash(&def);
                    return Ok((name, row, def, hash));
                }
            }
        }
    }
    status::remove(&ctx.paths, &item.id);
    Err(SyncError::invalid("这项改动已经有了更新的版本，请重新查看"))
}

fn local_def(ctx: &Ctx, name: &str) -> Option<BotDef> {
    read_def(&bots_dir(ctx).join(format!("{name}.toml")))
}

/// 差异对比用的两份定义（TOML 文本）。
pub(crate) fn detail(ctx: &Ctx, item: &PendingItem) -> Result<serde_json::Value> {
    let (name, _, remote, _) = pending_row(ctx, item)?;
    let local = match local_def(ctx, &name) {
        Some(def) => Some(def_text(&def)?),
        None => None,
    };
    Ok(serde_json::json!({
        "name": name,
        "local": local,
        "remote": def_text(&remote)?,
        "relaxed": item.payload["relaxed"],
        "recreated": item.payload["recreated"],
    }))
}

/// 用户在本机接受或拒绝了一份放宽工具限制的定义。
pub(crate) fn decide(
    ctx: &Ctx,
    _s: &Settings,
    item: &PendingItem,
    accept: bool,
) -> Result<Option<Reload>> {
    let (name, row, remote, hash) = pending_row(ctx, item)?;
    let local = local_def(ctx, &name);
    if accept {
        apply(ctx, &name, local.as_ref(), &Some((remote, hash.clone())))
            .map_err(SyncError::other)?;
    }
    db::write(ctx.db(), |tx| {
        mark(tx, scope::BOT, &row.key, &row.hlc, HANDLED)?;
        if accept {
            set_base(tx, &name, &Some(hash.clone()))?;
            db::state_del(tx, SCOPE_GONE, &name)?;
            db::state_del(tx, SCOPE_REJ, &name)
        } else {
            db::state_put(tx, SCOPE_REJ, &name, db::HLC_ZERO, None, Some(&hash))?;
            // 还没有基线（刚加入的设备）：记成本机当前的定义，免得反过来把本机的版本推给
            // 别的设备。之后本机再改，照常发布。
            if db::state_get(tx, SCOPE_BASE, &name)?.is_none() {
                set_base(tx, &name, &local.as_ref().map(def_hash))?;
            }
            Ok(())
        }
    })?;
    status::remove(&ctx.paths, &item.id);
    Ok(accept.then_some(Reload::Bots))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::{self, Cache};
    use crate::testing::{self, Device};
    use rclaw_store::MemoryTarget;

    fn sync_files(device: &Device) -> files::FilesReport {
        files::reconcile(
            &device.ctx,
            &device.settings,
            &device.keys,
            &mut Cache::default(),
            true,
        )
    }

    fn exchange(from: &Device, to: &Device) -> files::FilesReport {
        sync_files(from);
        testing::seal_all(from);
        testing::ship(from, to);
        to.merge();
        sync_files(to)
    }

    fn store(device: &Device) -> BotStore {
        BotStore::new(&device.ctx.layout.base)
    }

    fn bot(persona: &str, denied: &[&str]) -> BotDef {
        BotDef {
            title: "研究员".into(),
            description: "负责检索".into(),
            persona: persona.into(),
            provider: None,
            denied_tools: denied.iter().map(|tool| tool.to_string()).collect(),
        }
    }

    fn pending(device: &Device) -> Vec<PendingItem> {
        status::list(&device.ctx.paths)
            .into_iter()
            .filter(|item| item.kind == KIND)
            .collect()
    }

    fn conflicts(device: &Device) -> usize {
        std::fs::read_dir(device.ctx.paths.conflicts.join("bots")).map_or(0, |dir| dir.count())
    }

    #[test]
    fn the_hash_ignores_formatting_but_not_content() {
        let a = bot("你是研究员。", &["shell", "write"]);
        let mut b = bot("  你是研究员。\n", &["write", "shell", "write"]);
        b.provider = Some("  ".into());
        assert_eq!(def_hash(&normalize(a.clone())), def_hash(&normalize(b)));
        let c = bot("你是研究员。", &["shell"]);
        assert_ne!(
            def_hash(&normalize(a.clone())),
            def_hash(&normalize(c.clone()))
        );
        assert_eq!(relaxed(&a.denied_tools, &c), ["write"]);
        assert!(relaxed(&c.denied_tools, &a).is_empty());
    }

    #[test]
    fn definitions_travel_and_later_edits_follow() {
        let a = Device::new();
        let b = Device::new();
        store(&a)
            .save("researcher", bot("你是研究员。", &["shell"]))
            .unwrap();
        let report = exchange(&a, &b);
        assert!(report.reload.contains(&Reload::Bots));
        assert_eq!(store(&b).get("researcher").unwrap().persona, "你是研究员。");
        // 落下来的版本不会被当成本机的改动再发出去。
        sync_files(&b);
        assert!(testing::outbox(&b).iter().all(|op| op["t"] != "bot.put"));
        // 收紧工具限制的改动直接生效。
        store(&b)
            .save("researcher", bot("你是严谨的研究员。", &["shell", "write"]))
            .unwrap();
        exchange(&b, &a);
        let def = store(&a).get("researcher").unwrap();
        assert_eq!(def.persona, "你是严谨的研究员。");
        assert_eq!(def.denied_tools, ["shell", "write"]);
        assert!(pending(&a).is_empty());
    }

    #[test]
    fn relaxing_denied_tools_needs_local_confirmation() {
        let a = Device::new();
        let b = Device::new();
        store(&a)
            .save("researcher", bot("你是研究员。", &["shell", "write"]))
            .unwrap();
        exchange(&a, &b);
        store(&a)
            .save("researcher", bot("你是研究员。", &["write"]))
            .unwrap();
        exchange(&a, &b);
        assert_eq!(
            store(&b).get("researcher").unwrap().denied_tools,
            ["shell", "write"]
        );
        let items = pending(&b);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].payload["relaxed"], serde_json::json!(["shell"]));
        let detail = files::detail(&b.ctx, &items[0].id).unwrap();
        assert!(detail["remote"].as_str().unwrap().contains("write"));
        // 拒绝：本机保持原样，也不把本机的版本推回去。
        files::decide(
            &b.ctx,
            &b.settings,
            &items[0].id,
            "reject",
            items[0].version(),
        )
        .unwrap();
        sync_files(&b);
        assert!(pending(&b).is_empty());
        assert_eq!(
            store(&b).get("researcher").unwrap().denied_tools,
            ["shell", "write"]
        );
        assert!(testing::outbox(&b).iter().all(|op| op["t"] != "bot.put"));
        // 再放宽一次（新的版本）：再问一次，这次接受。
        store(&a)
            .save("researcher", bot("你是研究员。", &[]))
            .unwrap();
        exchange(&a, &b);
        let items = pending(&b);
        assert_eq!(items.len(), 1);
        let reload = files::decide(
            &b.ctx,
            &b.settings,
            &items[0].id,
            "accept",
            items[0].version(),
        )
        .unwrap();
        assert_eq!(reload, Some(Reload::Bots));
        assert!(store(&b).get("researcher").unwrap().denied_tools.is_empty());
        assert_eq!(conflicts(&b), 1);
    }

    #[test]
    fn a_deleted_bot_cannot_come_back_with_fewer_restrictions_unasked() {
        let a = Device::new();
        let b = Device::new();
        store(&a)
            .save("researcher", bot("你是研究员。", &["shell", "write"]))
            .unwrap();
        exchange(&a, &b);
        // A 删掉它，B 照做；随后 A 又建了一个同名的、少禁一个工具的。
        store(&a).delete("researcher").unwrap();
        exchange(&a, &b);
        assert!(store(&b).get("researcher").is_err());
        store(&a)
            .save("researcher", bot("你是研究员。", &["write"]))
            .unwrap();
        exchange(&a, &b);
        assert!(store(&b).get("researcher").is_err(), "先问，不直接落地");
        let items = pending(&b);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].payload["relaxed"], serde_json::json!(["shell"]));
        assert_eq!(items[0].payload["recreated"], serde_json::json!(true));
        // 拒绝之后，限制没放宽的版本照常落地；之后再放宽，还是要问。
        files::decide(
            &b.ctx,
            &b.settings,
            &items[0].id,
            "reject",
            items[0].version(),
        )
        .unwrap();
        store(&a)
            .save("researcher", bot("你是严谨的研究员。", &["shell", "write"]))
            .unwrap();
        exchange(&a, &b);
        assert_eq!(
            store(&b).get("researcher").unwrap().denied_tools,
            ["shell", "write"]
        );
        assert!(pending(&b).is_empty());
        store(&a)
            .save("researcher", bot("你是严谨的研究员。", &[]))
            .unwrap();
        exchange(&a, &b);
        assert_eq!(pending(&b).len(), 1);
        assert_eq!(
            store(&b).get("researcher").unwrap().denied_tools,
            ["shell", "write"]
        );
    }

    #[test]
    fn a_replaced_definition_keeps_a_copy() {
        let a = Device::new();
        let b = Device::new();
        store(&a).save("researcher", bot("第一版。", &[])).unwrap();
        exchange(&a, &b);
        assert_eq!(conflicts(&b), 0);
        store(&a).save("researcher", bot("第二版。", &[])).unwrap();
        exchange(&a, &b);
        assert_eq!(store(&b).get("researcher").unwrap().persona, "第二版。");
        assert_eq!(conflicts(&b), 1, "被换掉的第一版留了副本");
    }

    #[test]
    fn a_joining_device_takes_the_space_version_and_keeps_a_copy() {
        let a = Device::new();
        let b = Device::new();
        store(&a)
            .save("researcher", bot("空间里的版本。", &["shell"]))
            .unwrap();
        store(&b)
            .save("researcher", bot("本机原来的版本。", &["shell"]))
            .unwrap();
        store(&b)
            .save("writer", bot("只在本机的 bot。", &[]))
            .unwrap();
        exchange(&a, &b);
        assert_eq!(
            store(&b).get("researcher").unwrap().persona,
            "空间里的版本。"
        );
        assert_eq!(conflicts(&b), 1);
        // 本机独有的 bot 作为存量发布出去。
        exchange(&b, &a);
        assert_eq!(store(&a).get("writer").unwrap().persona, "只在本机的 bot。");
    }

    #[test]
    fn deleting_a_bot_removes_it_and_its_memory_everywhere() {
        let a = Device::new();
        let b = Device::new();
        store(&a)
            .save("researcher", bot("你是研究员。", &[]))
            .unwrap();
        exchange(&a, &b);
        let memory = |device: &Device| {
            rclaw_store::MemoryStore::new(rclaw_store::Layout::new(
                device.ctx.layout.base.join("bots").join("researcher"),
            ))
        };
        memory(&a)
            .add_entry(MemoryTarget::Memory, "用户偏好引用原始出处")
            .unwrap();
        exchange(&a, &b);
        assert_eq!(
            memory(&b).list_entries(MemoryTarget::Memory).unwrap().len(),
            1
        );

        store(&a).delete("researcher").unwrap();
        exchange(&a, &b);
        assert!(store(&b).get("researcher").is_err());
        assert_eq!(conflicts(&b), 2, "定义和数据目录各留一份");
        // 新建一个同名的 bot：旧的记忆不回来。
        store(&a)
            .save("researcher", bot("新的研究员。", &[]))
            .unwrap();
        exchange(&a, &b);
        assert_eq!(store(&b).get("researcher").unwrap().persona, "新的研究员。");
        assert!(memory(&b)
            .list_entries(MemoryTarget::Memory)
            .unwrap()
            .is_empty());
        exchange(&b, &a);
        assert!(memory(&a)
            .list_entries(MemoryTarget::Memory)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_directory_that_cannot_be_listed_is_not_taken_for_deleted_bots() {
        let a = Device::new();
        let b = Device::new();
        store(&a)
            .save("researcher", bot("你是研究员。", &[]))
            .unwrap();
        exchange(&a, &b);
        let sent =
            |device: &Device, kind: &str| testing::outbox(device).iter().any(|op| op["t"] == kind);
        // `bots/` 列不出来（拿一个同名的文件顶替目录来模拟）：不是「bot 都删了」。
        let dir = b.ctx.layout.base.join("bots");
        let away = b.ctx.layout.base.join("bots.away");
        std::fs::rename(&dir, &away).unwrap();
        std::fs::write(&dir, "").unwrap();
        assert!(def_files(&b.ctx).is_err());
        sync_files(&b);
        assert!(!sent(&b, "bot.del"));
        // 目录回来之后一切照旧。
        std::fs::remove_file(&dir).unwrap();
        std::fs::rename(&away, &dir).unwrap();
        sync_files(&b);
        assert!(!sent(&b, "bot.del") && !sent(&b, "bot.put"));
        // 定义真的不在了才是删除。
        store(&b).delete("researcher").unwrap();
        sync_files(&b);
        assert!(sent(&b, "bot.del"));
    }

    #[test]
    fn an_unreadable_local_file_is_left_alone_and_unknown_fields_wait_for_an_upgrade() {
        let a = Device::new();
        let b = Device::new();
        store(&a)
            .save("researcher", bot("你是研究员。", &[]))
            .unwrap();
        exchange(&a, &b);
        // 本机的文件被手工改坏：不当成删除发布。
        let path = b.ctx.layout.base.join("bots").join("researcher.toml");
        std::fs::write(&path, "title = ").unwrap();
        sync_files(&b);
        assert!(testing::outbox(&b).iter().all(|op| op["t"] != "bot.del"));

        // 带着本机不认识的字段的定义：不落地，提示升级；重启后再试。
        let c = Device::new();
        sync_files(&a);
        testing::seal_all(&a);
        testing::ship_tampered(&a, &c, |line| {
            line.replace("title = ", "future = 1\\ntitle = ")
        });
        c.merge();
        sync_files(&c);
        assert!(store(&c).get("researcher").is_err());
        let notices = status::list(&c.ctx.paths);
        assert!(notices.iter().any(|item| item.kind == "bot-rejected"));
        db::write(c.ctx.db(), |tx| files::retry_rejected(tx)).unwrap();
        let again = db::read(c.ctx.db(), |conn| {
            files::row(conn, scope::BOT, "researcher")
        })
        .unwrap()
        .unwrap();
        assert!(again.is_new());
    }
}
