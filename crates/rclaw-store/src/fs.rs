//! 文件系统布局、目录/文件确保、原子写。
//!
//! agent 数据目录与文件布局，但：
//! - 用一个 `Layout` 结构集中描述所有路径（取代散落的路径宏）；
//! - `atomic_write` 用 tmp + rename 取代「直接覆盖」，提升崩溃安全性；
//! - 不再处理 SPIFFS，直接用 `std::fs`。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use rand::Rng;

use crate::Result;

/// agent 数据目录布局。所有路径由 `base` 派生：
/// ```text
/// agent/
///   config/  { SOUL.md  USER.md }
///   memory/  { MEMORY.md  daily/  session_summaries/ }
///   state.sqlite   （会话 / rewind checkpoint / usage 锚点 / journal / kv，WAL 模式，
///                     旁有 state.sqlite-wal / -shm）
///   exports/       （/export 产出的 JSONL，按需创建）
///   skills/
///   logs/  { rclaw.log.YYYY-MM-DD }
///   cron.json
///   HEARTBEAT.md
/// ```
#[derive(Debug, Clone)]
pub struct Layout {
    pub base: PathBuf,
    pub config_dir: PathBuf,
    pub memory_dir: PathBuf,
    pub memory_daily_dir: PathBuf,
    pub session_summary_dir: PathBuf,
    pub skills_dir: PathBuf,
    pub logs_dir: PathBuf,
    /// 机器状态单库（会话 / rewind checkpoint / usage 锚点 / journal / kv）。
    pub db_file: PathBuf,
    /// `/export` 的 JSONL 输出目录（按需创建）。
    pub exports_dir: PathBuf,
    pub memory_file: PathBuf,
    pub soul_file: PathBuf,
    pub user_file: PathBuf,
    pub cron_file: PathBuf,
    pub heartbeat_file: PathBuf,
}

impl Layout {
    /// 以给定 base 目录派生完整布局。
    pub fn new(base: impl Into<PathBuf>) -> Self {
        let base = base.into();
        let config_dir = base.join("config");
        let memory_dir = base.join("memory");
        Layout {
            memory_daily_dir: memory_dir.join("daily"),
            session_summary_dir: memory_dir.join("session_summaries"),
            skills_dir: base.join("skills"),
            logs_dir: base.join("logs"),
            db_file: base.join("state.sqlite"),
            exports_dir: base.join("exports"),
            memory_file: memory_dir.join("MEMORY.md"),
            soul_file: config_dir.join("SOUL.md"),
            user_file: config_dir.join("USER.md"),
            cron_file: base.join("cron.json"),
            heartbeat_file: base.join("HEARTBEAT.md"),
            base,
            config_dir,
            memory_dir,
        }
    }

    /// `/export` 输出文件路径：`exports/<percent(channel)>_<percent(chat_id)>_<stamp>.jsonl`。
    pub fn export_path(&self, channel: &str, chat_id: &str, stamp: &str) -> PathBuf {
        let ch = crate::percent_encode_key(channel);
        let id = crate::percent_encode_key(chat_id);
        self.exports_dir.join(format!("{ch}_{id}_{stamp}.jsonl"))
    }
}

/// 递归确保目录存在（已存在则跳过）。
pub fn ensure_dir(path: impl AsRef<Path>) -> Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

/// 确保某文件路径的父目录存在。
pub fn ensure_parent_dir(path: impl AsRef<Path>) -> Result<()> {
    if let Some(parent) = path.as_ref().parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// 清空目录下的所有内容（文件与子目录），但保留目录本身。
///
/// 返回移除的【顶层条目】数。目录不存在视为空（返回 0），不报错——供 `/clear` 这类
/// 「目录可能尚未创建」的清理命令安全调用。
///
/// **安全闸（P2）**：用 [`fs::symlink_metadata`]（不 follow）检查根路径本身——若是符号链接
/// 或非目录，直接返回 [`StoreError::InvalidPath`]。否则 `exists()`/`read_dir` 会跟随根路径的
/// 符号链接，把外部目录（agent/memory 或 agent/skills 被替换成 symlink/junction 时）的内容
/// 删掉。注意：Windows junction 是 reparse mount point，`FileType::is_symlink()` 检测不到，
/// 需调用方再用 canonicalize 兜底（见 turn.rs `ensure_dir_is_real`）。
///
/// 条目仍用 [`std::fs::DirEntry::file_type`]（不 follow）判定类型：符号链接按条目自身删除
/// （`remove_file`），不会顺着链接误删目标。遇到第一个无法删除的条目即返回错误（调用方可
/// 提示重试）；此前已删的条目保持已删状态。
pub fn clear_dir_contents(dir: impl AsRef<Path>) -> Result<usize> {
    let dir = dir.as_ref();
    // 用 symlink_metadata（不 follow 根路径）：根若是 symlink/reparse 点 → 拒绝。
    let meta = match fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(crate::StoreError::Io(e)),
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        return Err(crate::StoreError::InvalidPath(format!(
            "拒绝清空符号链接根路径 {dir:?}（可能指向 agent 之外，详见 /clear 安全策略）"
        )));
    }
    if !ft.is_dir() {
        return Err(crate::StoreError::InvalidPath(format!(
            "{dir:?} 不是目录，无法清空其内容"
        )));
    }
    let mut count = 0usize;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
        count += 1;
    }
    Ok(count)
}

/// 若文件不存在，则以默认内容创建。
///
/// **启动期崩溃自愈（修复「stale .bak 被 ensure_file 绕过」）**：在判断「目标是否
/// 缺失」之前，先调用 [`recover_stale_backup`]。原因：[`atomic_write`] 的兜底「备份
/// 替换」路径存在一个崩溃窗口——目标已被改名为 sidecar 备份、新文件尚未就位时进程
/// 崩溃，目标路径会缺失。若此时进程重启先跑 [`init_layout`]（它对 soul/user/memory/
/// cron/heartbeat 都调 `ensure_file`），旧实现会直接用【默认内容】重建缺失的目标，
/// 把崩溃前的真实数据（cron 任务、长期记忆等）永久覆盖；而遗留的 sidecar 备份再也没
/// 有机会恢复。把自愈放在 `ensure_file` 入口，所有经它初始化的引导文件都能在重建前
/// 先还原崩溃前的内容，数据不会因启动顺序而丢失。
pub fn ensure_file(path: impl AsRef<Path>, default_content: &str) -> Result<()> {
    let path = path.as_ref();
    ensure_parent_dir(path)?;
    // 先尝试还原崩溃遗留的 sidecar 备份（若存在且目标缺失）。
    recover_stale_backup(path);
    if !path.exists() {
        let mut f = fs::File::create(path)?;
        if !default_content.is_empty() {
            f.write_all(default_content.as_bytes())?;
        }
    }
    Ok(())
}

/// 初始化完整目录布局与几个引导文件。
///
/// 任何单项失败仅记录在返回的错误里，不阻断其余项（尽力而为）。
pub fn init_layout(layout: &Layout) -> Result<()> {
    ensure_dir(&layout.base)?;
    ensure_dir(&layout.config_dir)?;
    ensure_dir(&layout.memory_dir)?;
    ensure_dir(&layout.memory_daily_dir)?;
    ensure_dir(&layout.session_summary_dir)?;
    ensure_dir(&layout.skills_dir)?;
    ensure_dir(&layout.logs_dir)?;

    ensure_file(&layout.soul_file, "# Personality\n\n")?;
    ensure_file(&layout.user_file, "# User Info\n\n")?;
    ensure_file(&layout.memory_file, "# Memory\n\n")?;
    ensure_file(&layout.cron_file, "{\"jobs\":[]}\n")?;
    ensure_file(&layout.heartbeat_file, "# Heartbeat\n\n")?;
    Ok(())
}

/// 原子写：先写唯一 sidecar 临时文件，`sync_all` 落盘后再把 tmp 替换为目标。
///
/// 相比「直接 fopen(w) 覆盖」，这里能避免：
/// - 写到一半崩溃留下半截文件；
/// - 读者读到写了一半的内容。
///
/// 适用于整文件替换（cron.json、MEMORY.md 编辑后写回）。session 的「追加」语义
/// 见 [`crate::session`]，不使用本函数。
///
/// # 临时 / 备份文件命名（修复「foo.tmp/foo.bak 与真实用户文件冲突」P1）
///
/// 旧实现用 `set_extension("tmp"/"bak")`：写 `foo.md` 会生成 `foo.tmp` / `foo.bak`，
/// 于是：
/// - 同目录下用户真实存在的 `foo.tmp` / `foo.bak` 会被占用甚至删除；
/// - 不同扩展名同 stem 的文件（`foo.md` 与 `foo.txt`）会抢同一个 `foo.tmp`。
///
/// 现在改用「以完整文件名为基础 + 随机 nonce」的 sidecar 命名：
/// - 临时文件：`.<完整文件名>.<16-hex nonce>.tmp`（如 `.cron.json.1f4a….tmp`）。
///   nonce 使两次并发写也几乎不可能撞名，配合 [`OpenOptions::create_new`] 进一步保证
///   【绝不覆盖已存在文件】；且这个名字永远不会等于用户可命名的 `foo.tmp`。
/// - 备份文件：`.<完整文件名>.bak`（如 `.cron.json.bak`）。前导点表示「内部产物」，
///   并保留完整文件名，既不会与用户的 `foo.bak` 冲突，又【确定性可发现】，供崩溃自愈
///   （见下方）按目标路径稳定定位。
///
/// # 替换的崩溃安全性
///
/// 1. **主路径——真正的原子替换**：`std::fs::rename` 在 Unix 上是 `rename(2)`，对已
///    存在目标是内核级原子替换；在 **Windows 上，Rust std（含本仓库的 1.95）的
///    `rename` 调用的是 `MoveFileExW(..., MOVEFILE_REPLACE_EXISTING)`**——由内核单次
///    完成覆盖，不存在「目标先消失、新文件再就位」的中间窗口。因此主路径在两个平台上
///    都是崩溃安全的。
///
/// 2. **兜底路径——备份替换 + sidecar 备份自愈**：若主路径 rename 失败（跨卷、特殊
///    文件系统、权限等），退化为「目标改名为 sidecar 备份 → tmp 改名到目标」。这个
///    两步序列**理论上**存在一个崩溃窗口：目标已改名走、新文件尚未就位时进程崩溃，
///    目标路径会缺失。为堵住这个窗口，本函数（以及 [`ensure_file`]）在入口先做
///    [`recover_stale_backup`]——若发现某路径存在遗留 sidecar 备份、且目标缺失，说明
///    上次写崩在上述窗口，先把备份还原为目标再继续。即便进程在兜底路径中途崩溃，下次
///    对该路径的写入（或启动期的 `ensure_file`）也会自动修复，目标不会长期缺失。
///
/// 3. **绝不「先删目标再 rename tmp」**：那是旧实现的隐患——第二步 rename 失败时原
///    文件凭空消失，`atomic_write` 名不副实，可能让 MEMORY.md / cron.json 归零。
///
/// # 关于 `sync_all` 错误传播
///
/// 落盘失败（`ENOSPC` / `EIO`）必须让调用方感知，否则会把「未真正落盘的写入」当成功，
/// 与本函数的持久化保证相矛盾。因此这里用 `f.sync_all()?` 传播错误（而不是 `let _ =`）。
/// 会话 / journal 数据不走本函数：它们在 `agent/state.sqlite` 里由 SQLite 事务保证落盘。
pub fn atomic_write(path: impl AsRef<Path>, data: &[u8]) -> Result<()> {
    let path = path.as_ref();
    ensure_parent_dir(path)?;

    let backup = backup_path_for(path);

    // 目标是指向目录的符号链接（含 Windows 目录链接 / junction）时拒绝：主路径的 rename 对
    // 链接条目是「原子替换条目」——会把 `agent -> /data/real` 这样的工作区入口换成一个普通
    // 文件，目标目录完好但再也找不到。真实目录由主路径的 rename 自己失败、再被兜底路径
    // 拒绝；链接到目录的情况 rename 不会失败，只能在动手之前看条目（`symlink_metadata`）
    // 与落点（`metadata`）各一眼。
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
        && fs::metadata(path).is_ok_and(|meta| meta.is_dir())
    {
        return Err(crate::StoreError::Other(format!(
            "atomic_write 拒绝覆盖指向目录的符号链接: {}",
            path.display()
        )));
    }

    // 0) 崩溃自愈：若存在上次崩溃遗留的 sidecar 备份、且目标缺失，先把备份还原为目标
    //    （见上文「兜底路径的崩溃窗口」）。
    recover_stale_backup(path);

    // 1) 写【唯一】sidecar 临时文件：create_new 保证【绝不覆盖】已存在文件。
    //    每次 nonce 不同，并发写互不抢名；即便上次崩溃残留了某个 .tmp sidecar，
    //    本次也不会去碰它（残留无害，仅占少量磁盘）。
    let tmp = tmp_path_for(path);
    // 任一错误路径离开本函数时删除 tmp：否则 ENOSPC 下每次失败都留下一个带部分数据的
    // 孤儿 sidecar，cron / memory 等周期性写入会反复失败、反复残留，磁盘越写越满，
    // 永远恢复不了。两处 rename 成功后解除守卫（tmp 已不在）。守卫在 create_new 成功之后
    // 才武装：open 以 AlreadyExists 失败时该文件属于别的写入方，不能替它删。
    let mut tmp_guard = TmpGuard { path: &tmp, armed: false };
    {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        tmp_guard.armed = true;
        f.write_all(data)?;
        // 传播 fsync 错误：ENOSPC/EIO 时让整轮失败，而非谎报成功。
        f.sync_all()?;
    }

    // 2) 主路径：直接 rename。Unix 与现代 Windows（MOVEFILE_REPLACE_EXISTING）都是
    //    崩溃安全的原子替换；目标不存在时同样成功。
    if fs::rename(&tmp, path).is_ok() {
        tmp_guard.armed = false;
        // 主路径成功：顺带清理可能残留的旧 sidecar 备份（无残留时 remove_file 返回 Err，忽略）。
        let _ = fs::remove_file(&backup);
        // P3：rename 与备份 remove 改的是父目录项，sync 临时文件内容并不保证它们落盘——
        // 同步父目录收口，避免断电后 rename 看似成功实则未持久化。
        sync_parent_dir(path);
        return Ok(());
    }

    // 3) 兜底路径：rename 失败（跨卷 / 权限等罕见情况），用「备份替换」安全覆盖。
    //    关键：绝不能「先删目标再 rename tmp」——那样 rename 失败会让原文件消失。
    //    先清理可能残留的旧 sidecar 备份。
    let _ = fs::remove_file(&backup);
    // P1 修复：进入兜底路径前，先用 symlink_metadata 检查目标是否为目录。
    // 若 write 工具错误地以目录路径（如 agent/memory）为目标，旧代码会在下面的
    // rename(path, backup) 里把整个目录挪成 .bak 隐藏备份，然后用普通文件占掉路径，
    // 破坏 agent 目录结构。这里提前拒绝目录（tmp sidecar 由守卫清理；指向目录的链接已在写
    // tmp 之前拒绝；指向文件的链接按条目覆盖——链接本身被换成普通文件，目标不动）。
    let had_target = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            // 目标是个目录 —— 这是工具层写入了错误路径（如写到目录名而非目录内文件）。
            // 拒绝覆盖，保留目录结构完整。
            return Err(crate::StoreError::Other(format!(
                "atomic_write 拒绝覆盖目录（目标为目录，请检查路径是否为文件）: {}",
                path.display()
            )));
        }
        Ok(_meta) => true, // 普通文件/硬链接 → 可以安全覆盖。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false, // 目标不存在 → 新文件。
        Err(e) => return Err(crate::StoreError::Io(e)),
    };
    if had_target {
        // 目标 → sidecar 备份。
        rename_strict(path, &backup, "备份原文件")?;
    }
    // tmp → 目标。失败时必须把备份恢复回去，保证原文件不丢。
    if let Err(e) = fs::rename(&tmp, path) {
        if had_target {
            let _ = fs::rename(&backup, path);
        }
        // 临时 sidecar 未能就位，由守卫删除。
        return Err(crate::StoreError::Io(e));
    }
    tmp_guard.armed = false;
    // 4) 新文件已落地，删除 sidecar 备份（失败不影响最终结果）。
    if had_target {
        let _ = fs::remove_file(&backup);
    }
    // P3：兜底路径多次改写父目录项（备份 rename + tmp rename + 备份删除），同样同步父目录收口。
    sync_parent_dir(path);
    Ok(())
}

/// [`atomic_write`] 的 tmp sidecar 守卫：`armed` 时离开作用域即删除（错误路径兜底）。
struct TmpGuard<'a> {
    path: &'a Path,
    armed: bool,
}

impl Drop for TmpGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(self.path);
        }
    }
}

/// 计算目标的 sidecar 备份路径：`<dir>/.<完整文件名>.bak`。
///
/// 带前导点 + 完整文件名，既区别于用户可命名的 `foo.bak`，又能按目标路径稳定定位
/// （崩溃自愈需要确定性地找到它）。
fn backup_path_for(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let fname = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    parent.join(format!(".{fname}.bak"))
}

/// 计算目标的【唯一】sidecar 临时文件路径：`<dir>/.<完整文件名>.<16-hex nonce>.tmp`。
///
/// nonce 使并发写互不抢名，且永不等于用户可命名的 `foo.tmp`。配合 [`OpenOptions::create_new`]
/// 进一步保证不覆盖任何已存在文件。
fn tmp_path_for(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let fname = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nonce: u64 = rand::thread_rng().gen();
    parent.join(format!(".{fname}.{nonce:016x}.tmp"))
}

/// 崩溃自愈：若目标缺失、但存在遗留的 sidecar 备份，说明上次写崩在「备份替换」窗口
/// （目标已改名走、新文件未就位）。先把备份还原为目标，保证后续读写看到的是崩溃前
/// 的旧版本，而不是「凭空缺失」。
///
/// 在 [`atomic_write`] 与 [`ensure_file`] 入口都调用，覆盖「正常运行中再次写」与
/// 「进程重启先 init_layout」两条路径，避免启动期用默认内容覆盖真实数据。
fn recover_stale_backup(path: &Path) {
    let backup = backup_path_for(path);
    if backup.exists() && !path.exists() {
        // 还原失败仅记日志：本次写入仍会继续（用新内容覆盖），不会因此中断；
        // 但若还原失败，目标会缺失，写完后由 rename/tmp 就位——最坏情况下相当于一次
        // 正常覆盖，不会丢更多数据。
        if let Err(e) = fs::rename(&backup, path) {
            tracing::warn!(
                "atomic_write：还原崩溃遗留的 sidecar 备份失败 ({} -> {}): {e}",
                backup.display(),
                path.display()
            );
        }
    }
}

/// 同步重命名：把 `from` 改名为 `to`，失败时返回带上下文的 store 错误。
///
/// 与 `fs::rename` 不同点仅在于错误包装，便于「备份替换」序列在上层给出清晰诊断。
fn rename_strict(from: &Path, to: &Path, ctx: &str) -> Result<()> {
    fs::rename(from, to).map_err(|e| {
        tracing::warn!(
            "atomic_write：{ctx}失败 ({} -> {}): {e}",
            from.display(),
            to.display()
        );
        crate::StoreError::Io(e)
    })
}

/// 同步【父目录】，确保 `rename` / 备份删除产生的【目录项】变更也落盘（修复 P3）。
///
/// **背景**：[`atomic_write`] 已对临时文件【内容】做 `sync_all`，但 `rename`（tmp→目标）
/// 与备份 `remove_file` 改的是【父目录的目录项】，这些目录项的更新并未 fsync。断电后
/// 即便内容已落盘、目录项仍可能丢失，使 `rename` 看似返回 `Ok`、实际未持久化——削弱了
/// atomic_write 宣称的崩溃安全保证。本函数在 rename / 备份删除之后收口，把父目录也刷下去。
///
/// **平台差异**：
/// - Unix：可以只读打开目录 fd 并对它 `sync_all`（Linux 上这正是持久化目录项的标准做法）；
///   打不开或 sync 失败仅记 warn，不阻断写入（降级为「内容已落盘、目录项可能未落盘」）。
/// - Windows：std 没有可靠的「打开目录 fd 并 fsync」途径（`File::open` 目录通常失败），
///   故此处【不尝试】、不每次打噪声日志，改以本文档「明确降级说明」呈现——内容已 fsync，
///   目录项未 fsync。需要更强保证时须调用平台原生 API（经目录 handle 的 FlushFileBuffers）。
fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    {
        let Some(parent) = path.parent() else {
            return;
        };
        if parent.as_os_str().is_empty() {
            return;
        }
        // 只读打开目录 fd（Unix 允许），对它 fsync 以持久化目录项。
        match fs::File::open(parent) {
            Ok(dir) => {
                if let Err(e) = dir.sync_all() {
                    tracing::warn!(
                        "atomic_write：同步父目录失败（{}），崩溃安全降级（目录项可能未落盘）: {e}",
                        parent.display()
                    );
                }
            }
            Err(e) => tracing::warn!(
                "atomic_write：无法打开父目录做同步（{}），崩溃安全降级: {e}",
                parent.display()
            ),
        }
    }
    // Windows：std 无法可靠 fsync 目录，按文档降级（见上文「平台差异」）。保持静默避免噪声。
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 覆盖「目标已存在」的替换路径：写入新内容后，文件被整体替换、内容正确，
    /// 且过程中不会丢文件（备份替换序列的核心保证）。
    #[test]
    fn atomic_write_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cron.json");
        // 先有一份旧内容。
        atomic_write(&target, b"{\"jobs\":[\"old\"]}").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "{\"jobs\":[\"old\"]}");
        // 再覆盖新内容。
        atomic_write(&target, b"{\"jobs\":[\"new\"]}").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "{\"jobs\":[\"new\"]}");
        // 结束后无残留临时/备份文件。
        assert!(!target.with_extension("tmp").exists());
        assert!(!target.with_extension("bak").exists());
    }

    /// 写入全新文件（目标不存在）走直接 rename 分支。
    #[test]
    fn atomic_write_creates_new() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("MEMORY.md");
        atomic_write(&target, b"# Memory\n").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "# Memory\n");
    }

    /// 崩溃自愈：模拟「上次写崩在备份替换窗口」——目标已被改名走（缺失）、仅剩 sidecar 备份。
    /// 再次调用 atomic_write 时，入口应先把 sidecar 备份还原为目标，再正常写入新内容，
    /// 结束后目标为新内容、无残留 sidecar 备份，也不产生旧的 `cron.tmp`/`cron.bak`。
    #[test]
    fn atomic_write_recovers_stale_bak() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cron.json");
        // 先有一份旧内容并正常写入（建立 target）。
        atomic_write(&target, b"{\"jobs\":[\"old\"]}").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "{\"jobs\":[\"old\"]}");

        // 模拟崩溃在备份替换窗口：把 target 改名为 sidecar 备份名，target 缺失。
        let bak = backup_path_for(&target);
        fs::rename(&target, &bak).unwrap();
        assert!(!target.exists());
        assert!(bak.exists());

        // 再次写入：入口先自愈还原 sidecar 备份，再写新内容。
        atomic_write(&target, b"{\"jobs\":[\"new\"]}").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "{\"jobs\":[\"new\"]}");
        // 自愈 + 主路径成功后，sidecar 备份应被清理，且不会产生旧的 cron.tmp/cron.bak。
        assert!(!bak.exists());
        assert!(!target.with_extension("tmp").exists());
        assert!(!target.with_extension("bak").exists());
    }

    /// P1：atomic_write 不得破坏用户真实存在的 `foo.tmp` / `foo.bak`。
    /// 旧实现用 `set_extension` 会让写 `cron.json` 时占用/删除同目录真实的 `cron.tmp`/`cron.bak`；
    /// 新方案用带完整文件名 + 随机 nonce 的 sidecar，与用户可命名的固定 .tmp/.bak 不冲突。
    #[test]
    fn atomic_write_preserves_user_tmp_and_bak() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cron.json");
        // 用户真实的 cron.tmp / cron.bak（与目标同 stem、不同扩展名）。
        let user_tmp = dir.path().join("cron.tmp");
        let user_bak = dir.path().join("cron.bak");
        fs::write(&user_tmp, "user-tmp-sentinel").unwrap();
        fs::write(&user_bak, "user-bak-sentinel").unwrap();

        atomic_write(&target, b"{\"jobs\":[]}").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "{\"jobs\":[]}");
        // 用户文件原样保留，绝未被占用或删除。
        assert_eq!(fs::read_to_string(&user_tmp).unwrap(), "user-tmp-sentinel");
        assert_eq!(fs::read_to_string(&user_bak).unwrap(), "user-bak-sentinel");
    }

    /// P2：ensure_file 必须在用默认内容重建目标前，先还原崩溃遗留的 sidecar 备份——
    /// 否则启动期 init_layout 会用默认内容覆盖真实数据（cron/memory 可能丢失）。
    #[test]
    fn ensure_file_recovers_stale_bak_before_default() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cron.json");
        // 先有真实内容。
        atomic_write(&target, b"REAL_DATA").unwrap();
        // 模拟崩溃在备份替换窗口：target 改名为 sidecar 备份，target 缺失。
        let bak = backup_path_for(&target);
        fs::rename(&target, &bak).unwrap();
        assert!(!target.exists());

        // ensure_file 入口应先自愈还原 REAL_DATA，而不是用 default 重建覆盖。
        ensure_file(&target, "DEFAULT").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "REAL_DATA");
        assert!(!bak.exists(), "自愈后 sidecar 备份应被移走");
    }

    /// clear_dir_contents：删除文件 + 子目录（含嵌套），保留目录本身，返回顶层条目数。
    #[test]
    fn clear_dir_contents_removes_files_and_subdirs_keeps_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // 布局：root 下 1 个文件 + 2 个子目录（各含嵌套文件），模拟 memory/ 结构。
        fs::write(root.join("MEMORY.md"), "# Memory\n").unwrap();
        fs::create_dir_all(root.join("daily")).unwrap();
        fs::write(root.join("daily").join("2024-01-01.md"), "note").unwrap();
        fs::create_dir_all(root.join("session_summaries")).unwrap();
        fs::write(root.join("session_summaries").join("a.json"), "{}").unwrap();

        let count = clear_dir_contents(root).unwrap();
        // 顶层条目：MEMORY.md + daily/ + session_summaries/ = 3（嵌套文件不算顶层）。
        assert_eq!(count, 3);
        // 内容已清空，但目录本身保留。
        assert!(root.exists());
        assert!(root.read_dir().unwrap().next().is_none());
    }

    /// clear_dir_contents：目录不存在视为空，返回 0、不报错。
    #[test]
    fn clear_dir_contents_missing_dir_is_ok_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert_eq!(clear_dir_contents(&missing).unwrap(), 0);
    }

    /// clear_dir_contents：根路径是普通文件（非目录）→ 拒绝，不报 IO 假成功。
    #[test]
    fn clear_dir_contents_rejects_non_directory_root() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not_a_dir.txt");
        fs::write(&file, "x").unwrap();
        assert!(clear_dir_contents(&file).is_err());
    }

    /// clear_dir_contents：根路径本身是符号链接 → 拒绝，且【不会】顺着链接删到外部目标。
    #[cfg(unix)]
    #[test]
    fn clear_dir_contents_rejects_symlink_root() {
        use std::os::unix::fs::symlink;
        let target = tempfile::tempdir().unwrap();
        // 外部目标里放敏感内容，验证它绝不被删。
        fs::write(target.path().join("secret.txt"), "external").unwrap();
        let parent = tempfile::tempdir().unwrap();
        let link = parent.path().join("agent_memory");
        symlink(target.path(), &link).unwrap();

        assert!(clear_dir_contents(&link).is_err());
        assert!(
            target.path().join("secret.txt").exists(),
            "符号链接指向的外部内容不应被删除"
        );
    }
}
