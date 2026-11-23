//! 多端同步用的技能目录入口（docs/multi-device-sync.md §8.4）。
//!
//! 同步引擎（`rclaw-sync`）不依赖本 crate，宿主把这里的函数包成它的 `SkillHost`。归属
//! （内置 / 用户 / agent）、固定、停用的判断都以 [`SkillUsageStore`] 为准，与 `skill_manage`、
//! curator 同一口径；会改技能目录的两个操作（[`install`]、[`remove`]）与它们持同一把生命
//! 周期锁。调用方另外持着运行时的静默门（没有 turn、没有后台 review 在跑）。
//!
//! 同步的单位是 `skills/<名字>/` 整个目录。被换下来的版本、被别的设备删掉的技能都移进
//! `skills/.archive/`，不直接删。被同步换下来的旧版本每个技能只留最近几份
//! （[`prune_replaced`]）——不然每同步来一个版本，归档里就多一份完整的技能。
//!
//! 换版本是两次改名（旧版本进归档、新版本换上去），所以新版本要先备在技能目录所在的卷
//! 上：[`staging_dir`]。两次改名之间技能目录里没有这个技能；进程正好在这时没了的话，
//! 留在备料目录里的记号（`<名字>.swap`）会让 [`recover`] 把旧版本放回原处——在那之前
//! [`kind`] 报「同步不该动它」，这种「不在」不会被当成用户删了技能。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::skill_manage::{archive_skill_for_curator as archive_skill, skill_path_is_redirect};
use crate::skill_usage::{has_active_skill_users, lock_lifecycle, SkillState, SkillUsageStore};

/// 带本机专有文件时最多往下找这么多层。
const CARRY_DEPTH_MAX: usize = 12;
/// 同步备料用的目录名（`skills/.sync-staging/`）。以点开头：技能扫描、同步的清单都不看它。
const STAGING_DIR: &str = ".sync-staging";
/// 换版本途中的记号：备料目录里的 `<名字>.swap`。旧版本进归档之前建，内容是它在归档里的
/// 目录名（进了归档才知道，所以先建空的、再写）；整个换完才删。
const SWAP_EXT: &str = "swap";
/// 备料目录里放了这么久没人要的东西，算进程中途退出留下的。
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);
/// 被同步换下来的旧版本在归档里的记号（文件内容是技能名）。带着它的归档每个技能只留最近
/// [`REPLACED_KEEP`] 份；用户或 curator 停用的技能、被别的设备删掉的技能没有这个记号，
/// 不在清理之列。
const REPLACED_MARK: &str = ".replaced-by-sync";
const REPLACED_KEEP: usize = 3;
/// 比这大的脚本不逐字节比，直接算作变了。
const SCRIPT_COMPARE_MAX: u64 = 8 * 1024 * 1024;

/// 同步引擎备新版本的地方：与技能目录在同一个卷上，换上去就是一次改名。
pub fn staging_dir(skills_dir: &Path) -> PathBuf {
    skills_dir.join(STAGING_DIR)
}

fn swap_mark(skills_dir: &Path, name: &str) -> PathBuf {
    staging_dir(skills_dir).join(format!("{name}.{SWAP_EXT}"))
}

/// 归档目录的名字：`<技能名>-YYYYMMDD-HHMMSS`。
fn is_archive_leaf(name: &str, leaf: &str) -> bool {
    leaf.strip_prefix(name)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|stamp| {
            stamp.len() == 15
                && stamp.bytes().enumerate().all(|(at, byte)| match at {
                    8 => byte == b'-',
                    _ => byte.is_ascii_digit(),
                })
        })
}

/// 一个名字在本机技能目录里的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncedSkill {
    /// 内置技能：随程序下发，不参与同步。
    Builtin,
    /// 同步不该动它：已停用（归档），或者这个名字被符号链接 / 普通文件占着，或者归属
    /// 读不出来。
    Parked,
    /// 正常存在的用户技能 / agent 自建技能。`pinned`：被固定或保护，远端的改动要在本机
    /// 确认。
    Live { pinned: bool },
    /// 本机没有这个技能（同名的目录如果存在，也不是一个技能）。
    Absent,
}

/// 目录是不是一个技能：有 `SKILL.md`，或者是技能包（带 `skills/` 子目录）。
fn is_skill_dir(path: &Path) -> bool {
    path.join("SKILL.md").is_file() || path.join("skills").is_dir()
}

/// 参与同步的技能名：正常存在的用户技能与 agent 自建技能。技能目录读不了时报错，不返回
/// 空表（空表会被当成「全删了」）。与 [`kind`] 同一口径：这里列出的名字 [`kind`] 都报
/// [`SyncedSkill::Live`]，反过来也一样。
pub fn syncable(skills_dir: &Path) -> Result<Vec<String>, String> {
    let bundled = SkillUsageStore::new(skills_dir.to_path_buf()).repository_names()?;
    let entries =
        std::fs::read_dir(skills_dir).map_err(|error| format!("读取技能目录失败: {error}"))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("读取技能目录失败: {error}"))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if SkillUsageStore::validate_name(&name).is_err() || bundled.contains(&name) {
            continue;
        }
        let path = entry.path();
        let real_dir = std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir())
            && !skill_path_is_redirect(&path);
        if real_dir && is_skill_dir(&path) {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

pub fn kind(skills_dir: &Path, name: &str) -> SyncedSkill {
    if SkillUsageStore::validate_name(name).is_err() {
        return SyncedSkill::Absent;
    }
    let store = SkillUsageStore::new(skills_dir.to_path_buf());
    match store.repository_names() {
        Ok(bundled) if bundled.contains(name) => return SyncedSkill::Builtin,
        Ok(_) => {}
        Err(_) => return SyncedSkill::Parked,
    }
    let record = store.record(name);
    let path = skills_dir.join(name);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            if skill_path_is_redirect(&path) || !meta.is_dir() {
                return SyncedSkill::Parked;
            }
            if is_skill_dir(&path) {
                // 固定标记读不出来时按固定算：宁可多问一次。
                let pinned = record.as_ref().map_or(true, |record| record.pinned)
                    || path.join(".pinned").exists()
                    || path.join(".protected").exists();
                return SyncedSkill::Live { pinned };
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // 换版本换到一半被打断了：旧版本在归档里，等着被放回来（[`recover`]）。这种
            // 「不在」不是谁删的。
            if std::fs::symlink_metadata(swap_mark(skills_dir, name)).is_ok() {
                return SyncedSkill::Parked;
            }
        }
        Err(_) => return SyncedSkill::Parked,
    }
    match record {
        Ok(record) if record.state == SkillState::Archived => SyncedSkill::Parked,
        Ok(_) => SyncedSkill::Absent,
        Err(_) => SyncedSkill::Parked,
    }
}

/// turn 之外还有后台任务正在用技能目录（与 curator 看同一个计数）。读不出来按「在用」算。
pub fn in_use(skills_dir: &Path) -> bool {
    has_active_skill_users(skills_dir).unwrap_or(true)
}

/// 用 `staged` 目录的内容作为技能 `name` 的新版本。原有的版本（或占着这个名字的非技能
/// 目录）移进 `skills/.archive/`；它里面本机专有的文件（`local_only` 对相对技能目录、
/// 正斜杠的路径返回真：密钥、配置、固定标记、虚拟环境等）带到新版本里。成功后 `staged`
/// 已被移走。
///
/// 换版本本身是两次改名：旧版本进归档、新版本换上去。任何时刻被打断，技能目录里的这个
/// 技能要么是完整的旧版本、要么是完整的新版本、要么（两次改名之间）不在——不会是半份。
/// 调用方持着静默门，所以这里不做和技能大小成正比的慢活：`staged` 应当备在
/// [`staging_dir`] 里；不在的话先整份搬进去（见 [`bring_in`]）。
pub fn install(
    skills_dir: &Path,
    name: &str,
    staged: &Path,
    local_only: &dyn Fn(&str) -> bool,
) -> Result<(), String> {
    SkillUsageStore::validate_name(name)?;
    let _lifecycle = lock_lifecycle()?;
    if has_active_skill_users(skills_dir)? {
        return Err("有后台任务正在使用技能".to_string());
    }
    let store = SkillUsageStore::new(skills_dir.to_path_buf());
    if store.repository_names()?.contains(name) {
        return Err(format!("「{name}」是内置技能，不接受同步来的版本"));
    }
    if skill_path_is_redirect(staged) || !staged.is_dir() {
        return Err("备好的新版本不是一个目录".to_string());
    }
    std::fs::create_dir_all(skills_dir).map_err(|error| format!("创建技能目录失败: {error}"))?;
    let live = skills_dir.join(name);
    if !rclaw_store::resolve_within_base(skills_dir, &live) {
        return Err(format!("技能「{name}」的落点不在技能目录内"));
    }
    let exists = match std::fs::symlink_metadata(&live) {
        Ok(meta) => {
            if skill_path_is_redirect(&live) || !meta.is_dir() {
                return Err(format!(
                    "技能目录里的「{name}」是符号链接或普通文件，同步不覆盖它"
                ));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(format!("检查技能「{name}」失败: {error}")),
    };
    // 新版本先到位（技能目录所在的卷上），再动旧版本。
    let incoming = bring_in(skills_dir, name, staged)?;
    let discard = |incoming: &Path| {
        if incoming != staged {
            let _ = std::fs::remove_dir_all(incoming);
        }
    };
    let mark = swap_mark(skills_dir, name);
    let old = match exists {
        true => {
            // 记号先于改名：从这里到整个换完之间被打断，[`recover`] 会接着收拾。
            if let Err(error) = std::fs::write(&mark, "") {
                discard(&incoming);
                return Err(format!("记录换版本的进度失败: {error}"));
            }
            match archive_skill(skills_dir, &live, name) {
                Ok(old) => {
                    if let Some(leaf) = old.file_name().and_then(|leaf| leaf.to_str()) {
                        let _ = std::fs::write(&mark, leaf);
                    }
                    Some(old)
                }
                Err(error) => {
                    let _ = std::fs::remove_file(&mark);
                    discard(&incoming);
                    return Err(error);
                }
            }
        }
        false => None,
    };
    if let Err(error) = std::fs::rename(&incoming, &live) {
        discard(&incoming);
        // 新版本没换上去：把旧版本放回原处。
        if let Some(old) = &old {
            if let Err(rollback) = std::fs::rename(old, &live) {
                // 记号留着：之后 [`recover`] 接着放，放回去之前同步不动这个名字。
                return Err(format!(
                    "换上新版本失败: {error}；旧版本留在 {}，放回原处也失败了: {rollback}",
                    old.display()
                ));
            }
            let _ = std::fs::remove_file(&mark);
        }
        return Err(format!("换上新版本失败: {error}"));
    }
    match &old {
        Some(old) => {
            settle_swap(old, &live, name, local_only);
            let _ = std::fs::remove_file(&mark);
        }
        None => {
            for script in changed_scripts(None, &live, local_only) {
                crate::skill_entrypoint::notify_skill_script_changed(&script);
            }
        }
    }
    // 备料目录空了就不留着（它在用户的技能目录里）。
    let _ = std::fs::remove_dir(staging_dir(skills_dir));
    Ok(())
}

/// 新版本换上去之后的收尾：把旧版本里本机专有的文件带过来，通知宿主哪些脚本变了，给旧
/// 版本做上「被同步换下来」的记号（同类的归档只留最近几份，见 [`prune_replaced`]）。
/// 记号最后做：归档里的旧版本带着它，就说明收尾做完了。每一步重做一遍都无妨。
fn settle_swap(old: &Path, live: &Path, name: &str, local_only: &dyn Fn(&str) -> bool) {
    carry_local_files(old, live, "", local_only, 0);
    for script in changed_scripts(Some(old), live, local_only) {
        crate::skill_entrypoint::notify_skill_script_changed(&script);
    }
    if let Err(error) = std::fs::write(old.join(REPLACED_MARK), name) {
        tracing::debug!(skill = name, %error, "给换下来的旧版本做记号失败");
    }
}

/// 收拾备料目录。每一轮同步开头调一次（不在静默门里）；平时这个目录不存在，什么都不做。
///
/// - 换版本换到一半被打断的技能：旧版本进了归档、新版本没换上去的，把旧版本放回原处
///   （那个远端版本之后照常再装一次）；新版本已经换上去、收尾没做完的，把收尾做完。
///   返回有没有技能被放回来——有的话技能列表要重新加载。
/// - 放了一阵没人要的东西（进程在备料途中退出留下的）：删掉。
pub fn recover(skills_dir: &Path, local_only: &dyn Fn(&str) -> bool) -> bool {
    let root = staging_dir(skills_dir);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return false;
    };
    if skill_path_is_redirect(&root) || !rclaw_store::resolve_within_base(skills_dir, &root) {
        return false;
    }
    let mut marks: Vec<(String, PathBuf)> = Vec::new();
    let mut stale: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_file() && path.extension().is_some_and(|ext| ext == SWAP_EXT) {
            if let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) {
                marks.push((name.to_string(), path));
            }
            continue;
        }
        // 修改时间读不出来的按旧的算；在将来的（改过时钟）先不动。
        let old = match meta.modified() {
            Ok(at) => at.elapsed().is_ok_and(|age| age >= STALE_AFTER),
            Err(_) => true,
        };
        if old {
            stale.push(path);
        }
    }
    let mut restored = false;
    if !marks.is_empty() {
        match lock_lifecycle() {
            Ok(_lifecycle) => {
                for (name, mark) in &marks {
                    restored |= resume_swap(skills_dir, name, mark, local_only);
                }
            }
            Err(error) => tracing::warn!(%error, "收拾换到一半的技能失败"),
        }
    }
    for path in &stale {
        remove_leftover(path);
    }
    let _ = std::fs::remove_dir(&root);
    restored
}

/// 收拾一个换到一半的技能（调用方持着生命周期锁）。返回旧版本有没有被放回原处。事情
/// 了结了才删记号：放不回去（I/O 出错）就留着下次再试，这期间同步不动这个名字。
fn resume_swap(
    skills_dir: &Path,
    name: &str,
    mark: &Path,
    local_only: &dyn Fn(&str) -> bool,
) -> bool {
    let done = |restored: bool| {
        let _ = std::fs::remove_file(mark);
        restored
    };
    if SkillUsageStore::validate_name(name).is_err() {
        return done(false);
    }
    let archive = skills_dir.join(".archive");
    if skill_path_is_redirect(&archive) {
        return false;
    }
    // 记号里写着旧版本进了归档之后的目录名；还是空的，说明进程没在「进了归档」之后活到
    // 写下它。
    let recorded = std::fs::read_to_string(mark)
        .ok()
        .map(|leaf| leaf.trim().to_string())
        .filter(|leaf| is_archive_leaf(name, leaf));
    let usable = |leaf: &str| {
        let old = archive.join(leaf);
        let safe = std::fs::symlink_metadata(&old).is_ok_and(|meta| meta.is_dir())
            && !skill_path_is_redirect(&old)
            && rclaw_store::resolve_within_base(skills_dir, &old);
        safe.then_some(old)
    };
    let live = skills_dir.join(name);
    match std::fs::symlink_metadata(&live) {
        Ok(meta) => {
            // 技能在。旧版本还没进归档就被打断了（或者已经放回来了）：没有要收拾的。新版本
            // 已经换上去：归档里那份旧版本没带「换下来」的记号，就是收尾没做完。
            if let Some(old) = recorded.as_deref().and_then(usable) {
                if meta.is_dir()
                    && !skill_path_is_redirect(&live)
                    && !old.join(REPLACED_MARK).exists()
                {
                    settle_swap(&old, &live, name, local_only);
                }
            }
            return done(false);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return false,
    }
    let old = recorded
        .or_else(|| latest_unclaimed(skills_dir, name))
        .as_deref()
        .and_then(usable);
    let Some(old) = old else {
        // 归档里找不到它的旧版本（被人清掉了）：没有可放回的。
        return done(false);
    };
    match std::fs::rename(&old, &live) {
        Ok(()) => {
            tracing::info!(skill = name, "上次换版本被打断，旧版本已放回原处");
            done(true)
        }
        Err(error) => {
            tracing::warn!(skill = name, %error, "把换到一半的技能的旧版本放回原处失败");
            false
        }
    }
}

/// 归档里这个技能最近的一份「没主」的旧版本：既不带被同步换下来的记号，也不是停用技能时
/// 记下的那一份。换版本的记号里没来得及写下目录名时用它。
fn latest_unclaimed(skills_dir: &Path, name: &str) -> Option<String> {
    let recorded = SkillUsageStore::new(skills_dir.to_path_buf())
        .record(name)
        .ok()
        .and_then(|record| record.archive_path);
    let archive = skills_dir.join(".archive");
    std::fs::read_dir(&archive)
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|leaf| {
            is_archive_leaf(name, leaf)
                && recorded.as_deref() != Some(format!(".archive/{leaf}").as_str())
                && !archive.join(leaf).join(REPLACED_MARK).exists()
        })
        .max()
}

/// 删掉备料目录里的一样东西。链接只删它自己，不跟进去。
fn remove_leftover(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    let removed = if meta.file_type().is_symlink() || skill_path_is_redirect(path) {
        std::fs::remove_file(path).or_else(|_| std::fs::remove_dir(path))
    } else if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    if let Err(error) = removed {
        tracing::debug!(path = %path.display(), %error, "清理同步备料目录失败");
    }
}

/// 把备好的新版本放到技能目录所在的卷上（[`staging_dir`] 里），返回它在那里的位置。本来
/// 就备在那里的原样返回。不在一个卷上的要整份复制过来——这一步在动旧版本之前做：复制到
/// 一半被打断，技能目录里的这个技能还是原样，留下的半份下次会被清掉。
fn bring_in(skills_dir: &Path, name: &str, staged: &Path) -> Result<PathBuf, String> {
    let root = staging_dir(skills_dir);
    let in_place = staged.parent() == Some(root.as_path());
    if !in_place {
        std::fs::create_dir_all(&root).map_err(|error| format!("创建备料目录失败: {error}"))?;
    }
    // 备料目录被换成了链接：它指到哪儿去不知道（多半不在这个卷上），不用它。
    if skill_path_is_redirect(&root) || !rclaw_store::resolve_within_base(skills_dir, &root) {
        return Err("技能目录里的备料目录是符号链接，同步不用它".to_string());
    }
    if in_place {
        return Ok(staged.to_path_buf());
    }
    let incoming = root.join(format!("{name}.incoming"));
    if std::fs::symlink_metadata(&incoming).is_ok() {
        std::fs::remove_dir_all(&incoming)
            .map_err(|error| format!("清理上次没装完的版本失败: {error}"))?;
    }
    if let Err(error) = move_dir(staged, &incoming) {
        let _ = std::fs::remove_dir_all(&incoming);
        return Err(format!("把新版本放进技能目录失败: {error}"));
    }
    Ok(incoming)
}

/// 把技能移进 `skills/.archive/`（别的设备把它删了）。返回它原来在不在。生命周期记录
/// 一并清掉：这不是本机的「停用」，同名技能以后再同步过来要能正常装上。`local_only` 同
/// [`install`]。
pub fn remove(
    skills_dir: &Path,
    name: &str,
    local_only: &dyn Fn(&str) -> bool,
) -> Result<bool, String> {
    SkillUsageStore::validate_name(name)?;
    let _lifecycle = lock_lifecycle()?;
    if has_active_skill_users(skills_dir)? {
        return Err("有后台任务正在使用技能".to_string());
    }
    let store = SkillUsageStore::new(skills_dir.to_path_buf());
    if store.repository_names()?.contains(name) {
        return Err(format!("「{name}」是内置技能，同步不移除它"));
    }
    let live = skills_dir.join(name);
    match std::fs::symlink_metadata(&live) {
        Ok(meta) => {
            if skill_path_is_redirect(&live) || !meta.is_dir() {
                return Err(format!(
                    "技能目录里的「{name}」是符号链接或普通文件，同步不移除它"
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("检查技能「{name}」失败: {error}")),
    }
    if !rclaw_store::resolve_within_base(skills_dir, &live) {
        return Err(format!("技能「{name}」的落点不在技能目录内"));
    }
    let mut scripts = Vec::new();
    collect_scripts(&live, local_only, &mut |path, _| {
        scripts.push(path.to_path_buf())
    });
    archive_skill(skills_dir, &live, name)?;
    if let Err(error) = store.forget(name) {
        tracing::warn!(skill = name, %error, "同步移除技能后清理生命周期记录失败");
    }
    for script in &scripts {
        crate::skill_entrypoint::notify_skill_script_changed(script);
    }
    Ok(true)
}

/// 清掉技能 `name` 被同步换下来的旧版本里较早的那些，只留最近 [`REPLACED_KEEP`] 份，返回
/// 清掉了几份。只动带着同步记号的归档。要删文件，不在静默门里调。
pub fn prune_replaced(skills_dir: &Path, name: &str) -> Result<usize, String> {
    SkillUsageStore::validate_name(name)?;
    // 持着生命周期锁只改名（挪进备料目录），出了锁再删：删一份完整的技能可能要一阵，而
    // 这把锁技能工具也在用。
    let doomed = {
        let _lifecycle = lock_lifecycle()?;
        set_aside_replaced(skills_dir, name)?
    };
    let mut removed = 0;
    for path in &doomed {
        match std::fs::remove_dir_all(path) {
            Ok(()) => removed += 1,
            // 留下的下次由 [`recover`] 清。
            Err(error) => tracing::warn!(skill = name, %error, "清理被同步换下来的旧版本失败"),
        }
    }
    if !doomed.is_empty() {
        let _ = std::fs::remove_dir(staging_dir(skills_dir));
    }
    Ok(removed)
}

/// 把技能 `name` 被同步换下来的旧版本里超出保留份数的挪进备料目录，返回它们现在的位置。
fn set_aside_replaced(skills_dir: &Path, name: &str) -> Result<Vec<PathBuf>, String> {
    let archive = skills_dir.join(".archive");
    if skill_path_is_redirect(&archive) {
        return Ok(Vec::new());
    }
    let entries = match std::fs::read_dir(&archive) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("读取技能归档失败: {error}")),
    };
    // 停用技能时记下的那一份（恢复要用它）不动，哪怕它带着记号——记号文件可能是用户手工
    // 把一份旧版本拷回去时带进去的。
    let recorded = SkillUsageStore::new(skills_dir.to_path_buf())
        .record(name)
        .ok()
        .and_then(|record| record.archive_path);
    let mut replaced: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|leaf| {
                recorded.as_deref() != Some(format!(".archive/{leaf}").as_str())
            })
        })
        .map(|entry| entry.path())
        .filter(|path| {
            std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
                && !skill_path_is_redirect(path)
                && std::fs::read_to_string(path.join(REPLACED_MARK))
                    .is_ok_and(|owner| owner == name)
        })
        .collect();
    // 归档名是「技能名-时间」：同一个技能的按名字排就是按时间排。
    replaced.sort();
    let excess = replaced.len().saturating_sub(REPLACED_KEEP);
    if excess == 0 {
        return Ok(Vec::new());
    }
    let root = staging_dir(skills_dir);
    std::fs::create_dir_all(&root).map_err(|error| format!("创建备料目录失败: {error}"))?;
    if skill_path_is_redirect(&root) || !rclaw_store::resolve_within_base(skills_dir, &root) {
        return Err("技能目录里的备料目录是符号链接，同步不用它".to_string());
    }
    let mut doomed = Vec::new();
    for path in replaced.into_iter().take(excess) {
        let Some(leaf) = path.file_name().and_then(|leaf| leaf.to_str()) else {
            continue;
        };
        if !rclaw_store::resolve_within_base(skills_dir, &path) {
            continue;
        }
        let aside = root.join(format!("{leaf}.old"));
        match std::fs::rename(&path, &aside) {
            Ok(()) => doomed.push(aside),
            Err(error) => {
                tracing::warn!(skill = name, %error, "清理被同步换下来的旧版本失败");
            }
        }
    }
    Ok(doomed)
}

/// 把目录整个移过去。同一个文件系统上是一次改名；跨文件系统时逐个文件复制，再删掉
/// 原来的。
fn move_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_dir(from, to, 0)?;
    let _ = std::fs::remove_dir_all(from);
    Ok(())
}

fn copy_dir(from: &Path, to: &Path, depth: usize) -> std::io::Result<()> {
    if depth > 64 {
        return Err(std::io::Error::other("目录层级过深"));
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let meta = std::fs::symlink_metadata(entry.path())?;
        let target = to.join(entry.file_name());
        if meta.file_type().is_symlink() {
            // 备好的版本里只有普通文件与目录；链接不跟、不带。
            continue;
        }
        if meta.is_dir() {
            copy_dir(&entry.path(), &target, depth + 1)?;
        } else if meta.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// 把旧版本里本机专有的文件 / 目录挪到新版本的同一位置上（新版本里已经有的不动）。
/// 尽力而为：挪不动的留在归档里，记一条日志。
fn carry_local_files(
    old: &Path,
    live: &Path,
    prefix: &str,
    local_only: &dyn Fn(&str) -> bool,
    depth: usize,
) {
    if depth > CARRY_DEPTH_MAX {
        return;
    }
    let Ok(entries) = std::fs::read_dir(old) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };
        let rel = match prefix.is_empty() {
            true => file_name.clone(),
            false => format!("{prefix}/{file_name}"),
        };
        let from = entry.path();
        let to = live.join(&file_name);
        let Ok(meta) = std::fs::symlink_metadata(&from) else {
            continue;
        };
        if local_only(&rel) {
            if std::fs::symlink_metadata(&to).is_ok() {
                continue;
            }
            let moved = match meta.is_dir() && !skill_path_is_redirect(&from) {
                true => move_dir(&from, &to),
                false => std::fs::rename(&from, &to).or_else(|_| {
                    std::fs::copy(&from, &to).map(|_| {
                        let _ = std::fs::remove_file(&from);
                    })
                }),
            };
            if let Err(error) = moved {
                tracing::warn!("技能的本机文件 {rel} 没能带到新版本里: {error}");
            }
        } else if meta.is_dir() && !skill_path_is_redirect(&from) && to.is_dir() {
            carry_local_files(&from, &to, &rel, local_only, depth + 1);
        }
    }
}

/// 目录里的 Python 脚本（宿主按路径记审批的那一类）：`found(路径, 相对 dir 的路径)`，
/// 相对路径用正斜杠。`skip` 说是本机专有的文件和目录不看：它们不随同步变，而虚拟环境
/// 里动辄几千个文件——这是在静默门里走的。
fn collect_scripts(dir: &Path, skip: &dyn Fn(&str) -> bool, found: &mut dyn FnMut(&Path, &str)) {
    fn walk(
        dir: &Path,
        prefix: &str,
        skip: &dyn Fn(&str) -> bool,
        found: &mut dyn FnMut(&Path, &str),
        depth: usize,
    ) {
        if depth > CARRY_DEPTH_MAX {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let Ok(file_name) = entry.file_name().into_string() else {
                continue;
            };
            let rel = match prefix.is_empty() {
                true => file_name,
                false => format!("{prefix}/{file_name}"),
            };
            if skip(&rel) {
                continue;
            }
            let path: PathBuf = entry.path();
            if file_type.is_dir() {
                walk(&path, &rel, skip, found, depth + 1);
            } else if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("py"))
            {
                found(&path, &rel);
            }
        }
    }
    walk(dir, "", skip, found, 0);
}

/// 两个文件的内容是不是一样。读不了、太大的都算不一样。
fn same_content(a: &Path, b: &Path) -> bool {
    let len = |path: &Path| std::fs::metadata(path).map(|meta| meta.len()).ok();
    match (len(a), len(b)) {
        (Some(x), Some(y)) if x == y && x <= SCRIPT_COMPARE_MAX => {}
        _ => return false,
    }
    matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

/// 换上新版本之后要通知宿主的脚本（路径按技能现在的位置写）：内容变了的、没有了的、新来
/// 的。两边一字不差的不通知——宿主对它们的审批是按内容记的，还作数；每来一个只改了说明
/// 文字的版本就把所有脚本的审批都作废，用户得一遍遍重新批准。
fn changed_scripts(
    old: Option<&Path>,
    live: &Path,
    local_only: &dyn Fn(&str) -> bool,
) -> Vec<PathBuf> {
    let mut before: BTreeMap<String, PathBuf> = BTreeMap::new();
    if let Some(old) = old {
        collect_scripts(old, local_only, &mut |path, rel| {
            before.insert(rel.to_string(), path.to_path_buf());
        });
    }
    let mut changed = Vec::new();
    collect_scripts(live, local_only, &mut |path, rel| {
        let same = before
            .remove(rel)
            .is_some_and(|old| same_content(&old, path));
        if !same {
            changed.push(path.to_path_buf());
        }
    });
    // 旧版本里有、新版本里没有的。
    for rel in before.keys() {
        let mut gone = live.to_path_buf();
        gone.extend(rel.split('/'));
        changed.push(gone);
    }
    changed.sort();
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill_usage::SkillOwner;
    use std::fs;

    fn skills() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("agent").join("skills");
        fs::create_dir_all(&dir).unwrap();
        (temp, dir)
    }

    fn make(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir.join(name)).unwrap();
        fs::write(dir.join(name).join("SKILL.md"), body).unwrap();
    }

    fn staged(temp: &tempfile::TempDir, body: &str) -> PathBuf {
        let dir = temp.path().join("staged");
        fs::create_dir_all(dir.join("scripts")).unwrap();
        fs::write(dir.join("SKILL.md"), body).unwrap();
        fs::write(dir.join("scripts").join("run.py"), "print('new')\n").unwrap();
        dir
    }

    fn local_only(rel: &str) -> bool {
        rel == "config.toml" || rel.split('/').any(|part| part.starts_with('.'))
    }

    fn archived(dir: &Path, name: &str) -> Vec<PathBuf> {
        fs::read_dir(dir.join(".archive"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.file_name()
                            .and_then(|leaf| leaf.to_str())
                            .is_some_and(|leaf| leaf.starts_with(&format!("{name}-")))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn classification_matches_the_listing() {
        let (_temp, dir) = skills();
        make(&dir, "mine", "# mine\n");
        make(&dir, "pinned", "# pinned\n");
        make(&dir, "guarded", "# guarded\n");
        fs::write(dir.join("guarded").join(".protected"), "").unwrap();
        // 技能包：没有顶层 SKILL.md，带 skills/ 子目录。
        fs::create_dir_all(dir.join("pack").join("skills")).unwrap();
        fs::write(dir.join("pack").join("skills").join("a.md"), "# a\n").unwrap();
        // 不算技能的东西。
        fs::create_dir_all(dir.join("stray")).unwrap();
        fs::create_dir_all(dir.join("Bad Name")).unwrap();
        fs::write(dir.join("loose.md"), "# loose\n").unwrap();
        fs::create_dir_all(dir.join(".archive")).unwrap();
        let store = SkillUsageStore::new(dir.clone());
        store.record_created("mine", SkillOwner::Agent).unwrap();
        store.set_pinned("pinned", true).unwrap();
        // 内置技能的名字取内嵌清单里的任意一个。
        let builtin = store.repository_names().unwrap().into_iter().next();
        if let Some(builtin) = &builtin {
            make(&dir, builtin, "# builtin\n");
        }

        assert_eq!(
            syncable(&dir).unwrap(),
            ["guarded", "mine", "pack", "pinned"]
        );
        assert_eq!(kind(&dir, "mine"), SyncedSkill::Live { pinned: false });
        assert_eq!(kind(&dir, "pack"), SyncedSkill::Live { pinned: false });
        assert_eq!(kind(&dir, "pinned"), SyncedSkill::Live { pinned: true });
        assert_eq!(kind(&dir, "guarded"), SyncedSkill::Live { pinned: true });
        assert_eq!(kind(&dir, "stray"), SyncedSkill::Absent);
        assert_eq!(kind(&dir, "nowhere"), SyncedSkill::Absent);
        assert_eq!(kind(&dir, "Bad Name"), SyncedSkill::Absent);
        if let Some(builtin) = &builtin {
            assert_eq!(kind(&dir, builtin), SyncedSkill::Builtin);
        }
        // 停用（归档）的技能：同步不动它。
        let moved = archive_skill(&dir, &dir.join("mine"), "mine").unwrap();
        store.mark_archived("mine", &moved).unwrap();
        assert_eq!(kind(&dir, "mine"), SyncedSkill::Parked);
        assert_eq!(syncable(&dir).unwrap(), ["guarded", "pack", "pinned"]);
        // 技能目录读不了：报错，不是空表。
        assert!(syncable(&dir.join("missing")).is_err());
    }

    #[test]
    fn install_replaces_the_skill_and_carries_local_files() {
        let (temp, dir) = skills();
        make(&dir, "tool", "# old\n");
        fs::write(dir.join("tool").join("config.toml"), "key = 1\n").unwrap();
        fs::write(dir.join("tool").join(".pinned"), "").unwrap();
        fs::create_dir_all(dir.join("tool").join("scripts").join(".cache")).unwrap();
        fs::write(
            dir.join("tool").join("scripts").join(".cache").join("x"),
            "c",
        )
        .unwrap();
        fs::write(
            dir.join("tool").join("scripts").join("old.py"),
            "print('old')\n",
        )
        .unwrap();
        SkillUsageStore::new(dir.clone())
            .record_created("tool", SkillOwner::Agent)
            .unwrap();

        let from = staged(&temp, "# new\n");
        install(&dir, "tool", &from, &local_only).unwrap();

        let live = dir.join("tool");
        assert_eq!(
            fs::read_to_string(live.join("SKILL.md")).unwrap(),
            "# new\n"
        );
        assert!(live.join("scripts").join("run.py").is_file());
        assert!(!live.join("scripts").join("old.py").exists());
        // 本机专有的文件跟着过来了。
        assert_eq!(
            fs::read_to_string(live.join("config.toml")).unwrap(),
            "key = 1\n"
        );
        assert!(live.join(".pinned").is_file());
        assert!(live.join("scripts").join(".cache").join("x").is_file());
        assert!(!from.exists());
        // 旧版本在归档里（带着「被同步换下来」的记号），归属记录没有动。
        let old = archived(&dir, "tool");
        assert_eq!(old.len(), 1);
        assert_eq!(
            fs::read_to_string(old[0].join("SKILL.md")).unwrap(),
            "# old\n"
        );
        assert!(old[0].join("scripts").join("old.py").is_file());
        assert_eq!(
            fs::read_to_string(old[0].join(REPLACED_MARK)).unwrap(),
            "tool"
        );
        let record = SkillUsageStore::new(dir.clone()).record("tool").unwrap();
        assert_eq!(record.owner, Some(SkillOwner::Agent));
        assert_eq!(record.state, SkillState::Active);
        assert_eq!(kind(&dir, "tool"), SyncedSkill::Live { pinned: true });
    }

    #[test]
    fn a_version_staged_next_to_the_skills_is_swapped_in_as_it_is() {
        let (_temp, dir) = skills();
        make(&dir, "tool", "# old\n");
        let from = staging_dir(&dir).join("tool-abcd");
        fs::create_dir_all(&from).unwrap();
        fs::write(from.join("SKILL.md"), "# new\n").unwrap();
        install(&dir, "tool", &from, &local_only).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("tool").join("SKILL.md")).unwrap(),
            "# new\n"
        );
        assert!(!from.exists());
        // 备料目录不算技能。
        assert_eq!(syncable(&dir).unwrap(), ["tool"]);
        assert_eq!(kind(&dir, STAGING_DIR), SyncedSkill::Absent);
    }

    #[test]
    fn only_scripts_whose_content_changed_are_reported() {
        let (temp, dir) = skills();
        make(&dir, "tool", "# old\n");
        let scripts = dir.join("tool").join("scripts");
        fs::create_dir_all(scripts.join(".venv")).unwrap();
        // 与新版本里的一字不差、新版本里没有了、本机专有（不看）。
        fs::write(scripts.join("run.py"), "print('new')\n").unwrap();
        fs::write(scripts.join("old.py"), "print('old')\n").unwrap();
        fs::write(scripts.join(".venv").join("site.py"), "x\n").unwrap();
        let from = staged(&temp, "# new\n");
        fs::write(from.join("scripts").join("extra.py"), "print('extra')\n").unwrap();
        install(&dir, "tool", &from, &local_only).unwrap();

        let live = dir.join("tool");
        let old = archived(&dir, "tool").remove(0);
        assert!(live.join("scripts").join(".venv").join("site.py").is_file());
        assert_eq!(
            changed_scripts(Some(&old), &live, &local_only),
            [
                live.join("scripts").join("extra.py"),
                live.join("scripts").join("old.py"),
            ]
        );
        // 内容变了的也算。
        fs::write(live.join("scripts").join("run.py"), "print('edited')\n").unwrap();
        assert_eq!(changed_scripts(Some(&old), &live, &local_only).len(), 3);
        // 没有旧版本：新版本里的都算（本机专有的还是不看）。
        assert_eq!(changed_scripts(None, &live, &local_only).len(), 2);
    }

    #[test]
    fn only_the_latest_replaced_versions_are_kept() {
        let (_temp, dir) = skills();
        let archive = dir.join(".archive");
        let put = |leaf: &str, owner: Option<&str>| {
            fs::create_dir_all(archive.join(leaf)).unwrap();
            fs::write(archive.join(leaf).join("SKILL.md"), "# old\n").unwrap();
            if let Some(owner) = owner {
                fs::write(archive.join(leaf).join(REPLACED_MARK), owner).unwrap();
            }
        };
        for second in 1..=5 {
            put(&format!("tool-20260101-00000{second}"), Some("tool"));
        }
        // 用户自己停用时归档的（没有记号）、另一个技能的：都不动。
        put("tool-20251231-235959", None);
        put("tool-x-20260101-000001", Some("tool-x"));
        assert_eq!(prune_replaced(&dir, "tool").unwrap(), 2);
        let mut left: Vec<String> = fs::read_dir(&archive)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "tool-20251231-235959",
                "tool-20260101-000003",
                "tool-20260101-000004",
                "tool-20260101-000005",
                "tool-x-20260101-000001",
            ]
        );
        assert_eq!(prune_replaced(&dir, "tool").unwrap(), 0);
        assert_eq!(prune_replaced(&dir, "missing").unwrap(), 0);
        assert!(!staging_dir(&dir).exists());
    }

    /// 模拟「旧版本进了归档、新版本还没换上去」时进程没了。`noted`：记号里写没写下旧
    /// 版本在归档里的目录名。
    fn interrupt_swap(dir: &Path, name: &str, leaf: &str, noted: bool) {
        fs::create_dir_all(staging_dir(dir)).unwrap();
        fs::create_dir_all(dir.join(".archive")).unwrap();
        fs::rename(dir.join(name), dir.join(".archive").join(leaf)).unwrap();
        fs::write(swap_mark(dir, name), if noted { leaf } else { "" }).unwrap();
    }

    #[test]
    fn an_interrupted_swap_is_undone_and_never_looks_like_a_deletion() {
        let (_temp, dir) = skills();
        for noted in [true, false] {
            make(&dir, "tool", "# old\n");
            fs::write(dir.join("tool").join("config.toml"), "key = 1\n").unwrap();
            // 归档里更早的两份：一份是被同步换下来的，一份是别的技能的。都不该被放回来。
            let archive = dir.join(".archive");
            fs::create_dir_all(archive.join("tool-20270101-000000")).unwrap();
            fs::write(
                archive.join("tool-20270101-000000").join(REPLACED_MARK),
                "tool",
            )
            .unwrap();
            fs::create_dir_all(archive.join("tool-x-20270101-000000")).unwrap();
            interrupt_swap(&dir, "tool", "tool-20260101-000000", noted);

            // 放回来之前：不在技能目录里，但同步不能把它当成「被删了」。
            assert_eq!(kind(&dir, "tool"), SyncedSkill::Parked);
            assert!(syncable(&dir).unwrap().is_empty());

            assert!(recover(&dir, &local_only));
            assert_eq!(kind(&dir, "tool"), SyncedSkill::Live { pinned: false });
            let live = dir.join("tool");
            assert_eq!(
                fs::read_to_string(live.join("SKILL.md")).unwrap(),
                "# old\n"
            );
            assert_eq!(
                fs::read_to_string(live.join("config.toml")).unwrap(),
                "key = 1\n"
            );
            assert!(!archive.join("tool-20260101-000000").exists());
            assert!(archive.join("tool-20270101-000000").is_dir());
            assert!(!staging_dir(&dir).exists());
            // 没有要收拾的了。
            assert!(!recover(&dir, &local_only));
            fs::remove_dir_all(&live).unwrap();
            fs::remove_dir_all(&archive).unwrap();
        }

        // 归档里的旧版本被人清掉了：没有可放回的，记号撤掉，这个技能就是不在了。
        make(&dir, "tool", "# old\n");
        interrupt_swap(&dir, "tool", "tool-20260101-000000", true);
        fs::remove_dir_all(dir.join(".archive").join("tool-20260101-000000")).unwrap();
        assert!(!recover(&dir, &local_only));
        assert_eq!(kind(&dir, "tool"), SyncedSkill::Absent);
    }

    #[test]
    fn a_swap_cut_short_after_the_new_version_went_live_is_finished() {
        let (_temp, dir) = skills();
        make(&dir, "tool", "# old\n");
        fs::write(dir.join("tool").join("config.toml"), "key = 1\n").unwrap();
        interrupt_swap(&dir, "tool", "tool-20260101-000000", true);
        // 新版本已经换上去了，收尾（带本机专有的文件、给旧版本做记号）还没做。
        make(&dir, "tool", "# new\n");
        assert_eq!(kind(&dir, "tool"), SyncedSkill::Live { pinned: false });

        assert!(!recover(&dir, &local_only));
        let live = dir.join("tool");
        let old = dir.join(".archive").join("tool-20260101-000000");
        assert_eq!(
            fs::read_to_string(live.join("SKILL.md")).unwrap(),
            "# new\n"
        );
        assert_eq!(
            fs::read_to_string(live.join("config.toml")).unwrap(),
            "key = 1\n"
        );
        assert_eq!(fs::read_to_string(old.join("SKILL.md")).unwrap(), "# old\n");
        assert_eq!(fs::read_to_string(old.join(REPLACED_MARK)).unwrap(), "tool");
        assert!(!staging_dir(&dir).exists());

        // 旧版本还没进归档就被打断：技能原样在，记号撤掉就行。
        fs::create_dir_all(staging_dir(&dir)).unwrap();
        fs::write(swap_mark(&dir, "tool"), "").unwrap();
        assert!(!recover(&dir, &local_only));
        assert_eq!(
            fs::read_to_string(live.join("SKILL.md")).unwrap(),
            "# new\n"
        );
        assert!(!staging_dir(&dir).exists());
    }

    #[test]
    fn only_stale_leftovers_are_swept_from_the_staging_dir() {
        let (_temp, dir) = skills();
        let root = staging_dir(&dir);
        fs::create_dir_all(root.join("fresh-0a1b2c3d")).unwrap();
        fs::write(root.join("fresh-0a1b2c3d").join("SKILL.md"), "# x\n").unwrap();
        fs::write(root.join("junk.tmp"), "x").unwrap();
        let long_ago = std::time::SystemTime::now() - STALE_AFTER * 2;
        fs::File::options()
            .write(true)
            .open(root.join("junk.tmp"))
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        assert!(!recover(&dir, &local_only));
        assert!(!root.join("junk.tmp").exists());
        // 刚备的料（也许是同一份数据上的另一个进程的）不动。
        assert!(root.join("fresh-0a1b2c3d").join("SKILL.md").is_file());
    }

    #[test]
    fn install_creates_a_missing_skill_and_refuses_what_it_must_not_touch() {
        let (temp, dir) = skills();
        let from = staged(&temp, "# fresh\n");
        install(&dir, "fresh", &from, &local_only).unwrap();
        assert_eq!(kind(&dir, "fresh"), SyncedSkill::Live { pinned: false });
        assert!(archived(&dir, "fresh").is_empty());

        // 内置技能、不合规的名字、不是目录的新版本。
        let store = SkillUsageStore::new(dir.clone());
        if let Some(builtin) = store.repository_names().unwrap().into_iter().next() {
            let from = staged(&temp, "# x\n");
            assert!(install(&dir, &builtin, &from, &local_only).is_err());
            assert!(!dir.join(&builtin).exists());
            fs::remove_dir_all(from).unwrap();
        }
        let from = staged(&temp, "# x\n");
        assert!(install(&dir, "../escape", &from, &local_only).is_err());
        assert!(install(&dir, "ok", &temp.path().join("nope"), &local_only).is_err());
        // 这个名字被一个普通文件占着：不覆盖。
        fs::write(dir.join("taken"), "file").unwrap();
        assert!(install(&dir, "taken", &from, &local_only).is_err());
        assert_eq!(fs::read_to_string(dir.join("taken")).unwrap(), "file");
        assert_eq!(kind(&dir, "taken"), SyncedSkill::Parked);
        assert!(from.is_dir(), "没装上的新版本原样留着");

        // 后台任务正在用技能：不换。
        let guard = crate::skill_usage::begin_skill_activity(&dir).unwrap();
        assert!(in_use(&dir));
        assert!(install(&dir, "fresh", &from, &local_only).is_err());
        assert!(remove(&dir, "fresh", &local_only).is_err());
        drop(guard);
        assert!(!in_use(&dir));
        install(&dir, "fresh", &from, &local_only).unwrap();
        assert_eq!(archived(&dir, "fresh").len(), 1);
    }

    #[test]
    fn remove_archives_and_forgets_the_skill() {
        let (_temp, dir) = skills();
        make(&dir, "gone", "# gone\n");
        let store = SkillUsageStore::new(dir.clone());
        store.record_created("gone", SkillOwner::Agent).unwrap();
        assert!(remove(&dir, "gone", &local_only).unwrap());
        assert!(!dir.join("gone").exists());
        assert_eq!(archived(&dir, "gone").len(), 1);
        // 被别的设备删掉的技能不带「换下来的旧版本」的记号：清理旧版本时不动它。
        assert!(!archived(&dir, "gone")[0].join(REPLACED_MARK).exists());
        assert_eq!(prune_replaced(&dir, "gone").unwrap(), 0);
        // 不算本机的停用：同名技能再同步过来能照常装上。
        assert_eq!(kind(&dir, "gone"), SyncedSkill::Absent);
        assert!(!remove(&dir, "gone", &local_only).unwrap());
    }
}
