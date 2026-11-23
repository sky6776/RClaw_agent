//! 本机回收站（§7.5）：会话因为同步被删除之前（合入别的设备的删除，或在本机删除一个
//! 会传播出去的会话），先把它导出成会话包放进 `<数据根>/sync/trash/`，保留若干天。
//! 明文、只在本机，不上传；恢复出来的是一个新线程。
//!
//! 因为同步而从记忆文件里删掉的条目也在这里留一份（`memory-*.md`）：给人看的一个小
//! Markdown 文件，外加一行注释记着条目原来在哪份记忆里，界面凭它把条目放回去。
//!
//! 回收站不依赖同步开没开、在不在同步空间里：列表、恢复、清除都只碰这个目录（和会话库）。

use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use rclaw_store::session::raw;
use serde::Serialize;

use crate::attach::now_ms;
use crate::bundle::{self, BundleHeader, ImportOptions, Imported};
use crate::config::SyncPaths;
use crate::db;
use crate::error::{Result, SyncError};
use crate::files::memory::{self, MemoryRestored};
use crate::ids;
use crate::status;
use crate::Ctx;

/// 回收站里的一个会话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrashItem {
    pub id: String,
    pub channel: String,
    pub chat_id: String,
    pub title: Option<String>,
    /// 进回收站的时刻（Unix 毫秒）。
    pub deleted_at_ms: i64,
    pub messages: u64,
    /// 会话包的大小（字节）。
    pub size: u64,
}

/// 文件名：`<进回收站的时刻，13 位毫秒>-<消息条数>-<4 位随机>.jsonl`。列表时不必读完
/// 整个文件。
fn parse_id(id: &str) -> Option<(i64, u64)> {
    let mut parts = id.split('-');
    let (at, count, tail) = (parts.next()?, parts.next()?, parts.next()?);
    let plain = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let ok = parts.next().is_none()
        && at.len() == 13
        && plain(at)
        && plain(count)
        && count.len() <= 9
        && tail.len() == 4
        && tail.bytes().all(|b| b.is_ascii_hexdigit());
    if !ok {
        return None;
    }
    Some((at.parse().ok()?, count.parse().ok()?))
}

fn item_path(paths: &SyncPaths, id: &str) -> Result<PathBuf> {
    if parse_id(id).is_none() {
        return Err(SyncError::invalid("回收站条目的标识不合规"));
    }
    Ok(paths.trash.join(format!("{id}.jsonl")))
}

/// 把一个会话导出到回收站。会话不存在或是空的返回 `None`。在库锁之外读写文件。
pub fn stash(ctx: &Ctx, channel: &str, chat_id: &str) -> Result<Option<String>> {
    let exists = db::read(ctx.db(), |conn| {
        Ok(raw::find_session(conn, channel, chat_id)?.is_some())
    })?;
    if !exists {
        return Ok(None);
    }
    let text = bundle::export_session(&ctx.env, channel, chat_id)?;
    let messages = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
        .saturating_sub(1);
    if messages == 0 {
        return Ok(None);
    }
    let id = format!(
        "{:013}-{messages}-{}",
        now_ms().max(0),
        ids::random_hex::<2>()
    );
    rclaw_store::atomic_write(item_path(&ctx.paths, &id)?, text.as_bytes())?;
    Ok(Some(id))
}

/// 回收站里的会话，新的在前。读不懂的文件跳过。
pub fn list(paths: &SyncPaths) -> Vec<TrashItem> {
    let Ok(entries) = std::fs::read_dir(&paths.trash) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let Some((deleted_at_ms, messages)) = parse_id(id) else {
            continue;
        };
        // 只读第一行（会话包的头）。
        let mut head = String::new();
        let Ok(file) = std::fs::File::open(entry.path()) else {
            continue;
        };
        let size = file.metadata().map_or(0, |meta| meta.len());
        if BufReader::new(file).read_line(&mut head).is_err() {
            continue;
        }
        let Ok(header) = serde_json::from_str::<BundleHeader>(&head) else {
            continue;
        };
        items.push(TrashItem {
            id: id.to_string(),
            channel: header.channel,
            chat_id: header.chat_id,
            title: header.title,
            deleted_at_ms,
            messages,
            size,
        });
    }
    items.sort_by(|a, b| {
        b.deleted_at_ms
            .cmp(&a.deleted_at_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    items
}

/// 从回收站恢复：作为一个新线程导入（导入的总是新线程；原来的线程在各设备上都已经
/// 删除），然后移出回收站。调用方随后要做一次全量扫描，把它发布出去。
pub fn restore(ctx: &Ctx, id: &str) -> Result<Imported> {
    let path = item_path(&ctx.paths, id)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|_| SyncError::other("回收站里没有这个会话（可能已经过期清理）"))?;
    let imported = bundle::import_session(&ctx.env, &text, &ImportOptions::default())?;
    let _ = std::fs::remove_file(&path);
    Ok(imported)
}

/// 彻底删除回收站里的一个会话。
pub fn remove(paths: &SyncPaths, id: &str) -> Result<()> {
    match std::fs::remove_file(item_path(paths, id)?) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// 记忆条目副本的文件名带有 13 位毫秒和内容摘要。四位尾缀用于读回已有副本。
fn parse_memory_name(name: &str) -> Option<i64> {
    let (at, tail) = name
        .strip_prefix("memory-")?
        .strip_suffix(".md")?
        .split_once('-')?;
    let ok = at.len() == 13
        && at.bytes().all(|b| b.is_ascii_digit())
        && matches!(tail.len(), 4 | 64)
        && tail.bytes().all(|b| b.is_ascii_hexdigit());
    if !ok {
        return None;
    }
    at.parse().ok()
}

/// 别的设备一次删掉本机不少条记忆时那条提示的类别；提示的键是副本的文件名。
pub(crate) const MEMORY_NOTICE_KIND: &str = "mem-removed";
/// 记忆副本里记着「条目原来在哪份记忆里」的那一行：`<!-- target: memory -->`。
const MEMORY_TARGET_OPEN: &str = "<!-- target: ";
const MEMORY_TARGET_CLOSE: &str = " -->";
/// 比这大的记忆副本不读（正常的副本远小于它：单条条目、整个记忆文件都有字数上限）。
const MEMORY_COPY_MAX: u64 = 4 * 1024 * 1024;

/// 回收站里的一份记忆条目副本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryTrashItem {
    /// 文件名去掉 `.md`。
    pub id: String,
    /// 给人看的一行说明：哪个文件、因为什么。
    pub what: String,
    /// 这些条目原来在哪份记忆里（`memory`、`user`、`bot:<名字>:memory`……）。副本里没记
    /// 的时候为空：只能查看，不能一键放回。
    pub target: Option<String>,
    /// 进回收站的时刻（Unix 毫秒）。
    pub deleted_at_ms: i64,
    pub entries: Vec<String>,
}

/// 把因为同步而从记忆文件里删掉的条目留一份在回收站（§7.5：记忆的清空会传到所有设备，
/// 所以各端都要留得回来）。`target` 是这些条目原来所在的那份记忆（同步里的目标名），
/// `what` 是给人看的一行说明：哪个文件、因为什么。明文、只在本机，和回收站里的会话一样
/// 到期清理；要找回，在界面的回收站里放回去，或者把里面的条目抄回记忆文件。没有条目时
/// 什么都不写。返回写下的文件名。
pub fn stash_memory(
    paths: &SyncPaths,
    target: &str,
    what: &str,
    entries: &[String],
) -> Result<Option<String>> {
    if entries.is_empty() {
        return Ok(None);
    }
    let one_line = |text: &str| text.replace(['\r', '\n'], " ");
    let digest = crate::crypto::sha256_hex(&serde_json::to_vec(&(target, entries))?);
    let suffix = format!("-{digest}.md");
    match std::fs::read_dir(&paths.trash) {
        Ok(files) => {
            for file in files {
                let file = file?;
                let name = file.file_name().to_string_lossy().into_owned();
                if name.ends_with(&suffix)
                    && parse_memory_name(&name).is_some()
                    && file.file_type()?.is_file()
                {
                    return Ok(Some(name));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let name = format!("memory-{:013}{suffix}", now_ms().max(0));
    let mut text = format!(
        "# {}\n{MEMORY_TARGET_OPEN}{}{MEMORY_TARGET_CLOSE}\n\n\
         要找回：在「多端同步」页的回收站里放回，或者把下面的条目抄回那个文件。\n\n",
        one_line(what),
        one_line(target)
    );
    for entry in entries {
        text.push_str("- ");
        text.push_str(&one_line(entry));
        text.push('\n');
    }
    rclaw_store::atomic_write(paths.trash.join(&name), text.as_bytes())?;
    Ok(Some(name))
}

fn memory_path(paths: &SyncPaths, id: &str) -> Result<PathBuf> {
    let name = format!("{id}.md");
    if parse_memory_name(&name).is_none() {
        return Err(SyncError::invalid("回收站条目的标识不合规"));
    }
    Ok(paths.trash.join(name))
}

/// 解析一份记忆副本。条目是 `- ` 开头的行（写的时候每条折成了一行）；第一条条目之前的
/// 是标题和目标那两行。
fn parse_memory_copy(id: &str, deleted_at_ms: i64, text: &str) -> MemoryTrashItem {
    let mut item = MemoryTrashItem {
        id: id.to_string(),
        what: String::new(),
        target: None,
        deleted_at_ms,
        entries: Vec::new(),
    };
    for line in text.lines() {
        if let Some(entry) = line.strip_prefix("- ") {
            if !entry.trim().is_empty() {
                item.entries.push(entry.to_string());
            }
        } else if item.entries.is_empty() {
            let line = line.trim();
            if let Some(what) = line.strip_prefix("# ") {
                if item.what.is_empty() {
                    item.what = what.trim().to_string();
                }
            } else if let Some(target) = line
                .strip_prefix(MEMORY_TARGET_OPEN)
                .and_then(|rest| rest.strip_suffix(MEMORY_TARGET_CLOSE))
            {
                item.target = Some(target.trim().to_string()).filter(|target| !target.is_empty());
            }
        }
    }
    item
}

fn read_memory_copy(
    path: &std::path::Path,
    id: &str,
    deleted_at_ms: i64,
) -> Option<MemoryTrashItem> {
    if std::fs::metadata(path).ok()?.len() > MEMORY_COPY_MAX {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    Some(parse_memory_copy(id, deleted_at_ms, &text))
}

/// 回收站里的记忆条目副本，新的在前。读不懂的、空的跳过。
pub fn list_memory(paths: &SyncPaths) -> Vec<MemoryTrashItem> {
    let Ok(entries) = std::fs::read_dir(&paths.trash) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(deleted_at_ms) = parse_memory_name(&name) else {
            continue;
        };
        let id = name.strip_suffix(".md").unwrap_or(&name);
        if let Some(item) = read_memory_copy(&entry.path(), id, deleted_at_ms) {
            if !item.entries.is_empty() {
                items.push(item);
            }
        }
    }
    items.sort_by(|a, b| {
        b.deleted_at_ms
            .cmp(&a.deleted_at_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    items
}

/// 读回收站里的一份记忆副本。
pub fn get_memory(paths: &SyncPaths, id: &str) -> Result<MemoryTrashItem> {
    let path = memory_path(paths, id)?;
    let deleted_at_ms = parse_memory_name(&format!("{id}.md")).unwrap_or(0);
    read_memory_copy(&path, id, deleted_at_ms)
        .ok_or_else(|| SyncError::other("回收站里没有这份记忆（可能已经过期清理）"))
}

/// 把回收站里的一份记忆副本放回它原来所在的记忆文件。全部放回了，副本移出回收站；有
/// 条目没通过本机的检查，副本留着（还能查看），结果里说明是哪几条、为什么。
pub fn restore_memory(ctx: &Ctx, id: &str) -> Result<MemoryRestored> {
    let item = get_memory(&ctx.paths, id)?;
    if item.entries.is_empty() {
        return Err(SyncError::other("这份副本里没有条目"));
    }
    let restored = memory::restore_entries(
        ctx,
        item.target.as_deref().unwrap_or_default(),
        &item.entries,
    )?;
    if restored.rejected.is_empty() {
        remove_memory(&ctx.paths, id)?;
    }
    Ok(restored)
}

/// 彻底删除回收站里的一份记忆副本。当初「删掉了本机的若干条记忆」的那条提示一起撤掉：
/// 它指着的副本不在了。
pub fn remove_memory(paths: &SyncPaths, id: &str) -> Result<()> {
    let path = memory_path(paths, id)?;
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    status::clear_notice(paths, MEMORY_NOTICE_KIND, &format!("{id}.md"));
    Ok(())
}

/// 清掉超过保留期的条目。返回清掉的个数。
///
/// 谁来调：同步空间打开时一次，同步运行期间每天一次，以及每次列回收站之前（界面打开
/// 回收站时）。同步关着、或者已经退出同步空间的时候没有后台线程，过期的条目就留到下一次
/// 打开回收站、或者同步再开起来的时候清——它们在界面上不会多显示一天。
pub fn sweep(paths: &SyncPaths, keep_days: u64) -> usize {
    let Ok(entries) = std::fs::read_dir(&paths.trash) else {
        return 0;
    };
    let keep_ms = (keep_days.min(3650) as i64) * 24 * 3600 * 1000;
    let now = now_ms();
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let memory_copy = parse_memory_name(&name);
        let stashed_at = name
            .strip_suffix(".jsonl")
            .and_then(parse_id)
            .map(|(at, _)| at)
            .or(memory_copy);
        let expired = match stashed_at {
            Some(at) => now - at > keep_ms,
            // 写到一半留下的临时文件之类：过一天再清。
            None => entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age.as_secs() > 24 * 3600),
        };
        if expired && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
            if memory_copy.is_some() {
                // 指着这份副本的那条提示一起撤掉。
                status::clear_notice(paths, MEMORY_NOTICE_KIND, &name);
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use rclaw_core::{Message, Role};

    #[test]
    fn stash_list_restore_roundtrip() {
        let (_g, ctx) = crate::testing::ctx();
        let _ = crate::testing::settings(&ctx);
        let sessions = &ctx.env.sessions;
        sessions
            .append("desktop", "c1", &Message::plain(Role::User, "一"))
            .unwrap();
        sessions
            .append("desktop", "c1", &Message::plain(Role::Assistant, "二"))
            .unwrap();
        sessions.set_session_title("desktop", "c1", "行程").unwrap();
        assert_eq!(stash(&ctx, "desktop", "missing").unwrap(), None);

        let id = stash(&ctx, "desktop", "c1").unwrap().expect("进回收站");
        let items = list(&ctx.paths);
        assert_eq!(items.len(), 1);
        assert_eq!(
            (
                items[0].id.as_str(),
                items[0].chat_id.as_str(),
                items[0].messages
            ),
            (id.as_str(), "c1", 2)
        );
        assert_eq!(items[0].title.as_deref(), Some("行程"));

        // 原会话还在：恢复出来的是另一个会话键下的新线程。
        let restored = restore(&ctx, &id).unwrap();
        assert_ne!(restored.chat_id, "c1");
        assert_eq!(restored.messages, 2);
        assert!(list(&ctx.paths).is_empty());
        assert!(restore(&ctx, &id).is_err());
    }

    #[test]
    fn ids_are_validated_and_old_items_are_swept() {
        let (_g, ctx) = crate::testing::ctx();
        assert!(remove(&ctx.paths, "../secrets").is_err());
        assert!(parse_id("1700000000000-12-ab12").is_some());
        assert!(parse_id("1700000000000-12-ab12-x").is_none());
        let old = format!("{:013}-1-abcd", now_ms() - 40 * 24 * 3600 * 1000);
        let fresh = format!("{:013}-1-abcd", now_ms());
        for id in [&old, &fresh] {
            std::fs::write(ctx.paths.trash.join(format!("{id}.jsonl")), b"{}\n").unwrap();
        }
        assert_eq!(sweep(&ctx.paths, 30), 1);
        assert!(!ctx.paths.trash.join(format!("{old}.jsonl")).exists());
        assert!(ctx.paths.trash.join(format!("{fresh}.jsonl")).exists());
        remove(&ctx.paths, &fresh).unwrap();
        remove(&ctx.paths, &fresh).unwrap();
    }

    #[test]
    fn memory_copies_are_listed_and_put_back() {
        use rclaw_store::MemoryTarget;

        let (_g, ctx) = crate::testing::ctx();
        // 没有条目什么都不写。
        assert_eq!(
            stash_memory(&ctx.paths, "memory", "说明", &[]).unwrap(),
            None
        );
        let what = "记忆文件 MEMORY.md 里被「笔记本」的同步删掉的条目";
        let entries = vec![
            "用户喜欢简洁的回答".to_string(),
            "项目用 Rust\n写".to_string(),
        ];
        let name = stash_memory(&ctx.paths, "memory", what, &entries)
            .unwrap()
            .expect("写下副本");
        assert_eq!(
            stash_memory(&ctx.paths, "memory", what, &entries).unwrap(),
            Some(name.clone())
        );
        let items = list_memory(&ctx.paths);
        assert_eq!(items.len(), 1);
        let item = items[0].clone();
        assert_eq!(format!("{}.md", item.id), name);
        assert_eq!(
            (item.what.as_str(), item.target.as_deref()),
            (what, Some("memory"))
        );
        // 说明那几行不算条目；条目里的换行折成了一行。
        assert_eq!(item.entries, ["用户喜欢简洁的回答", "项目用 Rust 写"]);
        // 会话的列表里没有它。
        assert!(list(&ctx.paths).is_empty());

        // 文件里已经有其中一条：放回之后不重复。副本移出回收站，指着它的提示撤掉。
        ctx.memory
            .add_entry(MemoryTarget::Memory, "用户喜欢简洁的回答")
            .unwrap();
        status::notice(
            &ctx.paths,
            MEMORY_NOTICE_KIND,
            &name,
            "删掉了本机的记忆",
            "详情",
        );
        let restored = restore_memory(&ctx, &item.id).unwrap();
        assert_eq!((restored.restored, restored.rejected.len()), (2, 0));
        assert_eq!(restored.into, "记忆文件 MEMORY.md");
        assert_eq!(
            ctx.memory.list_entries(MemoryTarget::Memory).unwrap(),
            ["用户喜欢简洁的回答", "项目用 Rust 写"]
        );
        assert!(list_memory(&ctx.paths).is_empty());
        assert!(status::list(&ctx.paths).is_empty());
        assert!(restore_memory(&ctx, &item.id).is_err());
    }

    #[test]
    fn memory_copies_that_cannot_go_back_stay_in_the_trash() {
        use rclaw_store::MemoryTarget;

        let (_g, ctx) = crate::testing::ctx();
        // 没记目标的副本（手工放进来的、别的版本写的）：列得出来，不能直接放回。
        let bare = format!("memory-{:013}-ab12", now_ms());
        std::fs::write(
            ctx.paths.trash.join(format!("{bare}.md")),
            "# 以前的副本\n\n- 一条旧记忆\n",
        )
        .unwrap();
        // bot 已经不在本机了：它的记忆没有地方放。
        let orphan = stash_memory(
            &ctx.paths,
            "bot:researcher:memory",
            "bot 的记忆",
            &["研究员的笔记".into()],
        )
        .unwrap()
        .expect("写下副本");
        let orphan = orphan.trim_end_matches(".md").to_string();
        assert_eq!(list_memory(&ctx.paths).len(), 2);
        assert_eq!(get_memory(&ctx.paths, &bare).unwrap().target, None);
        for id in [&bare, &orphan] {
            assert!(restore_memory(&ctx, id).is_err());
        }
        assert_eq!(list_memory(&ctx.paths).len(), 2);
        assert!(ctx
            .memory
            .list_entries(MemoryTarget::Memory)
            .unwrap()
            .is_empty());

        // 标识只认回收站自己起的名字。
        assert!(remove_memory(&ctx.paths, "../secrets").is_err());
        assert!(get_memory(&ctx.paths, "memory-1700000000000-zzzz").is_err());
        remove_memory(&ctx.paths, &bare).unwrap();
        remove_memory(&ctx.paths, &bare).unwrap();
        assert_eq!(list_memory(&ctx.paths).len(), 1);

        // 过了保留期的副本和会话一样被清掉，指着它的提示一起撤。
        let old = format!("memory-{:013}-cd34.md", now_ms() - 40 * 24 * 3600 * 1000);
        std::fs::write(ctx.paths.trash.join(&old), "# 很久以前\n\n- 旧的一条\n").unwrap();
        status::notice(
            &ctx.paths,
            MEMORY_NOTICE_KIND,
            &old,
            "删掉了本机的记忆",
            "详情",
        );
        assert_eq!(sweep(&ctx.paths, 30), 1);
        assert!(!ctx.paths.trash.join(&old).exists());
        assert!(status::list(&ctx.paths).is_empty());
        assert_eq!(list_memory(&ctx.paths).len(), 1);
    }
}
