use super::*;

// ---------------------------------------------------------------------------
// 通用辅助
// ---------------------------------------------------------------------------

/// 共享的 base 目录（路径校验基准 + 相对解析）。
pub(super) fn base_str(base: &Path) -> String {
    base.to_string_lossy().replace('\\', "/")
}

/// 解析文件工具路径并执行沙箱校验。`op` 是本次要对路径做的操作：工作区外路径的授权
/// 弹窗按它措辞、授权缓存按它分开（见 [`crate::access::ensure_path_allowed`]）。
///
/// 绝对路径保持原义；相对路径统一相对于 agent 存储根解析。为兼容旧调用，
/// 已显式带 `agent/` 前缀的相对路径不会再重复拼接一层。
pub(super) async fn resolve_path(
    base: &Path,
    path: &str,
    op: PathOp,
) -> Result<PathBuf, ToolResult> {
    resolve_path_with(base, path, op, ReservedDirPolicy::Refuse).await
}

/// 根下保留名【目录】的处理策略（见 [`resolve_path_with`]）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ReservedDirPolicy {
    /// 一律拒绝（读 / 写 / 列目录 / grep）。
    Refuse,
    /// 允许【真实目录】（非符号链接；保留名目录本身及其下的子目录都算）：只给 delete 用
    /// ——一个叫 `state.sqlite-journal/` 的目录不可能是库文件本身，却会让 SQLite 下次打开
    /// 失败；没有这个口子，模型和桌面文件面板都删不掉它，用户只剩清空应用数据一条路。
    /// 目录里的【文件】仍按库文件口径拒绝（见 `DeleteFile::execute` 的二次判定）。
    AllowRealDirectory,
}

/// [`resolve_path`] 的带策略版本。顺序：先过沙箱门（工作区外的路径在那里就被拒 / 弹窗，
/// 不会走到下面带 `exists()` 探测的归一化——否则沙箱内的调用者能借「数据库」与
/// 「越区」两种不同错误文案探测工作区外目录是否存在），再做运行时数据库守卫。
pub(super) async fn resolve_path_with(
    base: &Path,
    path: &str,
    op: PathOp,
    policy: ReservedDirPolicy,
) -> Result<PathBuf, ToolResult> {
    if path.is_empty() {
        return Err(ToolResult::err("路径不能为空"));
    }
    let resolved = join_workspace_path(base, path)?;
    // Unrestricted（max 权限）直放任意路径；工作区内直放；工作区外在宿主注册了
    // 确认钩子时（桌面版）弹窗询问用户，允许才放行——见 [`ensure_path_allowed`]。
    crate::access::ensure_path_allowed(
        base,
        &resolved,
        op,
        &format!("路径必须位于 {}/ 内且不能包含 '..'", base_str(base)),
    )
    .await?;
    // 运行时数据库在工作区根内，任何权限模式下都不经文件工具读写。根下叫伴生保留名的
    // 符号链接只对读 / 写类工具拒绝（原子写会把链接换成真的保留名文件）；delete 删的是
    // 链接条目本身，放行（见 is_reserved_name_link）。叫 `state.sqlite` 本名的链接对 delete
    // 也拒绝：那可能就是库的入口（`agent/state.sqlite -> ../real.sqlite`），删了它下次打开
    // 会新建一个空库。
    let link_names: &[&str] = if policy == ReservedDirPolicy::Refuse {
        STATE_DB_NAMES
    } else {
        &STATE_DB_NAMES[..1]
    };
    if (is_state_db_file(base, &resolved) && !reserved_real_directory(&resolved, policy))
        || is_reserved_name_link(base, &resolved, link_names)
    {
        return Err(ToolResult::err(format!(
            "{} 是 RClaw 运行时数据库（state.sqlite 及 -wal/-shm 伴生文件），文件工具禁止直接读写；\
             会话内容请用 session_search / session_handoff，导出请用 /export",
            resolved.display()
        )));
    }
    Ok(resolved)
}

/// 把工具给的路径拼成待校验的绝对路径（文件工具与 shell 的 `cwd` 同一规则）：绝对路径保持
/// 原义；相对路径相对 agent 根；已显式带根目录名前缀的旧式相对路径（`agent/sub`）不重复
/// 拼接。Windows 盘符相对路径（`C:`、`C:foo`）拒绝：有前缀没有根，落点是该盘的「当前目录」
/// ——进程级、随 cd 漂移，`base.join` 又会被它整个顶掉，任何权限 / 策略下都不表示一个
/// 确定的文件。类 Unix 下没有 Prefix 分量，这一条恒为假。
pub(crate) fn join_workspace_path(base: &Path, path: &str) -> Result<PathBuf, ToolResult> {
    let raw = Path::new(path);
    if matches!(
        raw.components().next(),
        Some(std::path::Component::Prefix(_))
    ) && !raw.has_root()
    {
        return Err(ToolResult::err(format!(
            "拒绝盘符相对路径 {path}：请写成带根的绝对路径（如 C:\\dir\\file）或相对 {}/ 的路径",
            base_str(base)
        )));
    }
    // Windows 的 DOS 设备名分量（`con`、`nul.md`、`COM¹`）打开的是设备不是文件：`read con`
    // 在控制台 CLI 里会卡在键盘输入上，`write con.txt` 凭空消失。按拼写拦（纯词法，不探测
    // 文件系统，放在沙箱门之前不泄露存在性）；桌面面板同一判定。
    if let Some(device) = raw.components().find_map(|comp| match comp {
        std::path::Component::Normal(name) => {
            let name = name.to_string_lossy();
            is_dos_device_name(&name).then(|| name.into_owned())
        }
        _ => None,
    }) {
        return Err(ToolResult::err(format!(
            "拒绝路径 {path}：{device} 是 Windows 设备名（CON / PRN / AUX / NUL / COM1-9 / LPT1-9），不是文件"
        )));
    }
    Ok(if raw.is_absolute() || raw.starts_with(base) {
        raw.to_path_buf()
    } else if base
        .file_name()
        .is_some_and(|base_name| raw.starts_with(base_name))
    {
        match base.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.join(raw),
            _ => raw.to_path_buf(),
        }
    } else {
        base.join(raw)
    })
}

/// Windows 的 DOS 设备名：`CON` / `PRN` / `AUX` / `NUL` / `COM1`–`COM9` / `LPT1`–`LPT9`
/// （第 4 个字符还包括上标数字 `¹²³`——Win32 同样把 `COM¹` 当设备，Python 的
/// `ntpath.isreserved` 也这么列），且只看第一个点之前的主干（`con.txt`、`NUL.md` 同样命中）。
/// Win32 路径里这样的分量不是文件而是设备——`agent/con.txt` 打开的是控制台，写入会卡住或
/// 凭空消失，`read_dir` 里又永远看不到它。只在 Windows 上拦：类 Unix 下它们就是普通文件名。
/// 按字符而不是字节计长：`COM¹` 是 5 个字节、4 个字符。文件工具与桌面文件面板共用。
#[cfg(windows)]
pub fn is_dos_device_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment).trim_end();
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let mut chars = upper.chars();
    let prefix: String = chars.by_ref().take(3).collect();
    (prefix == "COM" || prefix == "LPT")
        && matches!(chars.next(), Some('1'..='9' | '¹' | '²' | '³'))
        && chars.next().is_none()
}

/// 见 Windows 版：类 Unix 下 DOS 设备名就是普通文件名。
#[cfg(not(windows))]
pub fn is_dos_device_name(_segment: &str) -> bool {
    false
}

/// 策略允许时，`resolved` 实际落点是否是一个【真实目录】（`symlink_metadata` 不跟随末段
/// 链接：指向库文件的链接、库文件本身都不是目录）。中间段的 `..` 由内核解析，与真正
/// 被删的对象一致。
fn reserved_real_directory(resolved: &Path, policy: ReservedDirPolicy) -> bool {
    policy == ReservedDirPolicy::AllowRealDirectory
        && fs::symlink_metadata(resolved).is_ok_and(|meta| meta.is_dir())
}

/// 运行时数据库文件名（`agent/state.sqlite` 及 SQLite 伴生文件）。
const STATE_DB_NAMES: &[&str] = &[
    "state.sqlite",
    "state.sqlite-wal",
    "state.sqlite-shm",
    "state.sqlite-journal",
];

/// `resolved` 是否触及工作区根下的运行时数据库文件。
///
/// 旁路 SQLite 事务写库会直接损坏会话 / 回滚点 / journal；读它对模型也没有意义。
/// 只拦 base 根目录下的保留名，用户工作区别处的同名文件不受影响。判定按「实际会被
/// 打开的是哪个路径」而不是字面拼写（见 [`physical_normalize`]）：目标相对 canonical
/// base 的【第一段】是保留名即拦——既包括文件本身（`state.sqlite-wal`、`state.sqlite.`、
/// `STATE~1.SQL`、指向库文件的符号链接、`agent/<不存在>/../state.sqlite`），也包括
/// 以保留名作目录的路径（`state.sqlite-journal/x.txt`）：write 会 `create_dir_all` 出一个
/// 与热 journal / WAL 同名的目录，SQLite 下次打开时 `hasHotJournal` 对目录读失败，整个
/// 库就打不开了，而模型又删不掉它（同一守卫）。Windows 另剥掉 Win32 会静默丢弃的
/// 尾随点/空格与 `:stream` 后缀。
///
/// 还要看一眼【未折叠的原拼写】：`state.sqlite-journal/sub/../../x.txt` 物理落点是
/// `x.txt`，但 write 的 `create_dir_all` 拿到的是原拼写，POSIX 会逐段 mkdir、先把
/// `state.sqlite-journal/` 真的建出来再回退——所以原拼写里以保留名作【目录】用、且落在
/// base 根下的那一段，不管后面跟什么都拦。
pub(super) fn is_state_db_file(base: &Path, resolved: &Path) -> bool {
    let base = physical_normalize(base);
    let target = physical_normalize(resolved);
    if let Ok(relative) = target.strip_prefix(&base) {
        if first_component_is_reserved(relative) {
            return true;
        }
    }
    reserved_dir_in_raw_spelling(&base, resolved)
}

/// 根下叫保留名（`names` 之一）的【符号链接】（`agent/state.sqlite-journal -> note.md`）。
/// 物理落点不是库文件、末段又不参与原拼写规则，[`is_state_db_file`] 的两条规则都看不见它；
/// 但 write / edit 的原子写是「临时文件 rename 覆盖」——会把链接换成一个真的叫保留名的
/// 文件（假 hot journal），读它对模型也没有意义，所以 [`ReservedDirPolicy::Refuse`] 下四个
/// 保留名都拦。delete 删的是链接条目本身，正是清理伴生名残留的办法，只拦 `state.sqlite`
/// 本名：SQLite 自己从不创建链接，叫本名的链接只可能是用户把库挪走后留下的入口，删了它
/// 下次启动就是一个空库（伴生文件按打开时的路径派生在链接旁边、不会是链接）。指向库文件
/// 的链接由物理落点规则照旧拦住。
fn is_reserved_name_link(base: &Path, resolved: &Path, names: &[&str]) -> bool {
    if !is_symlink_leaf(resolved) {
        return false;
    }
    let reserved_leaf = resolved
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name_in(platform_plain_name(name), names));
    reserved_leaf
        && resolved
            .parent()
            .is_some_and(|parent| physical_normalize(parent) == physical_normalize(base))
}

/// 叶子本身是否是符号链接（Windows 上也包括 junction / 卷挂载点：`is_symlink` 对名字
/// 代理类 reparse point 都为真）。不跟随链接。
pub(crate) fn is_symlink_leaf(resolved: &Path) -> bool {
    fs::symlink_metadata(resolved).is_ok_and(|meta| meta.file_type().is_symlink())
}

/// 删除符号链接本身（不碰目标）。Windows 的目录链接 / junction 要用 `remove_dir`
/// （`remove_file` 对它报拒绝访问），文件链接用 `remove_file`——`remove_dir` 对链接条目只摘
/// 条目、不会进到目标里。按条目自己的类型选删法，两种都试、报与类型相符的那个错误（文件
/// 链接因共享冲突删不掉时，不能用 `remove_dir` 的「目录名无效」盖掉真因）。与桌面端
/// `remove_symlink` 同口径。
pub(super) fn remove_symlink(entry: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        type RemoveFn = fn(&Path) -> std::io::Result<()>;
        let is_dir_link =
            fs::symlink_metadata(entry).is_ok_and(|meta| meta.file_type().is_symlink_dir());
        let (first, second): (RemoveFn, RemoveFn) = if is_dir_link {
            (|p| fs::remove_dir(p), |p| fs::remove_file(p))
        } else {
            (|p| fs::remove_file(p), |p| fs::remove_dir(p))
        };
        match first(entry) {
            Ok(()) => Ok(()),
            Err(e) => second(entry).map_err(|_| e),
        }
    }
    #[cfg(not(windows))]
    {
        fs::remove_file(entry)
    }
}

/// `resolved` 是否就是工作区根（或其祖先）：删它等于连 `state.sqlite` 一起删。
///
/// 叶子本身是符号链接时不算：删除工具对链接只删链接本身（`symlink_metadata` 分支），
/// `agent/selflink -> agent` 删的是 `selflink` 这个条目，不是工作区；`physical_normalize`
/// 会跟到链接目标、把它当成根而误拒。POSIX 上 `selflink/` / `selflink/.` 这类拼写解析后
/// 已是目录本身，仍按根拒绝（Windows 的 Win32 层在规范化阶段就把尾随 `\.` 折掉，`selflink\.`
/// 与 `selflink` 是同一个 junction 条目，删的仍只是条目）。唯一的例外是 base 【自己的条目】
/// （`agent` / `./agent` / 其绝对路径）：
/// 工作区根本身就是一个链接（`agent -> /data/real`）时，删这个条目等于把工作区入口拆掉
/// （下次启动在原处新建一个空工作区），不能走「链接只删条目」的口子——先按条目身份拦。
pub(super) fn contains_workspace_root(base: &Path, resolved: &Path) -> bool {
    if is_workspace_root_entry(base, resolved) {
        return true;
    }
    if is_symlink_leaf(resolved) {
        return false;
    }
    let target = physical_normalize(resolved);
    if physical_normalize(base).starts_with(&target) {
        return true;
    }
    // 工作区根本身是链接（`/home/u/agent -> /data/real`）时，链接条目所在的目录及其祖先
    // （`/home/u`、`/home`）也不能删：递归删除会把入口条目一起带走，而上面按落点比
    // （`/data/real` 不在 `/home/u` 下）看不见这一层。
    entry_identity(base).is_some_and(|(entry_parent, _)| entry_parent.starts_with(&target))
}

/// `resolved` 在词法上就是 base 这个【目录项】：父目录按物理落点比（`./agent`、绝对路径、
/// `x/../agent` 都认），叶子名不跟随链接、按平台大小写规则比。与 [`contains_workspace_root`]
/// 的物理比对互补：后者在叶子是链接时故意不看落点，这里看的是条目本身。
///
/// 名字对不上但条目可能是同一个：8.3 短名（`WORKSP~1` 对 `workspace`）、大小写不敏感文件
/// 系统（APFS / sdcardfs）上的大小写变体。这时若叶子是链接 / reparse point 且跟随后的落点
/// 与 base 的落点相同，也按根条目算——同一父目录下指向同一目标的另一条链接被顺带拦下，
/// 可以接受（删不掉一条链接，比删掉工作区入口好）。
pub(super) fn is_workspace_root_entry(base: &Path, resolved: &Path) -> bool {
    let (Some((base_parent, base_name)), Some((parent, name))) =
        (entry_identity(base), entry_identity(resolved))
    else {
        return false;
    };
    if base_parent != parent {
        return false;
    }
    if same_entry_name(&base_name, &name) {
        return true;
    }
    is_symlink_leaf(resolved)
        && matches!(
            (fs::canonicalize(resolved), fs::canonicalize(base)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// 目录项身份：(父目录物理落点, 叶子名)。叶子不是普通分量（`..` 结尾、根）→ None。
fn entry_identity(path: &Path) -> Option<(PathBuf, std::ffi::OsString)> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut components = absolute.components();
    let std::path::Component::Normal(name) = components.next_back()? else {
        return None;
    };
    let parent: PathBuf = components.collect();
    Some((physical_normalize(&parent), name.to_os_string()))
}

#[cfg(windows)]
fn same_entry_name(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
    let plain =
        |name: &std::ffi::OsStr| platform_plain_name(&name.to_string_lossy()).to_lowercase();
    plain(a) == plain(b)
}

#[cfg(not(windows))]
fn same_entry_name(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
    a == b
}

/// `resolved` 的物理落点是否是【路径形态上的】文件系统根 / 盘符根（`/`、`C:\`、`\\?\C:\`、
/// UNC 共享根、以及 `C:/../..` 这类折叠后停在根上的拼写），Unix 上另加挂载点（`/mnt/usb`、
/// Android 的 `/storage/emulated/0`：与父目录不在同一设备上）。与工作区是什么关系都不能删：
/// Unrestricted 下别的盘的根不是工作区祖先，[`contains_workspace_root`] 拦不住，而
/// `remove_dir_all` 会真的从根开始删。Windows 的 subst 盘根 / 卷挂载目录在路径形态上有
/// 父目录（或本身是 reparse point），不在此列，Unrestricted 下按普通目录处理。
///
/// 叶子是符号链接时不算（`agent/rootlink -> /`）：与 [`contains_workspace_root`] 同一口径，
/// 删除工具对链接只删链接条目本身、不会顺着链接递归。
pub(super) fn is_filesystem_root(resolved: &Path) -> bool {
    if is_symlink_leaf(resolved) {
        return false;
    }
    let physical = physical_normalize(resolved);
    physical.parent().is_none() || is_mount_point(&physical)
}

/// Unix：目录与其父目录不在同一设备上即挂载点（bind mount 同一文件系统的子目录不算）。
/// 目录本身能 stat、父目录却不能（Android 的 `/storage/emulated/0`：父目录对应用不可见）
/// 时按挂载点处理——拿不到证据证明它不是，宁可多拦（删不掉一个目录比删光一张 SD 卡好）。
#[cfg(unix)]
fn is_mount_point(physical: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = physical.parent() else {
        return false;
    };
    match (fs::metadata(physical), fs::metadata(parent)) {
        (Ok(this), Ok(parent)) => this.is_dir() && this.dev() != parent.dev(),
        (Ok(this), Err(_)) => this.is_dir(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn is_mount_point(_physical: &Path) -> bool {
    false
}

/// 除 ASCII 大小写外还按 Unicode 大小写折叠比一次：NTFS / APFS 这类大小写不敏感文件系统
/// 把 `ſtate.sqlite`（长 s，大写折叠成 `S`）这类同形名当同一条目，`eq_ignore_ascii_case`
/// 看不见。
fn is_state_db_name(name: &str) -> bool {
    name_in(name, STATE_DB_NAMES)
}

fn name_in(name: &str, names: &[&str]) -> bool {
    names.iter().any(|db| {
        name.eq_ignore_ascii_case(db)
            || name.to_lowercase() == *db
            || name.to_uppercase() == db.to_uppercase()
    })
}

/// 目录遍历（grep）用的廉价预筛：先只比文件名，命中保留名的条目才去做带 canonicalize 的
/// `is_state_db_file`，避免对每个被遍历的文件都触盘规范化。
pub(super) fn walk_entry_is_state_db(base: &Path, entry: &Path) -> bool {
    entry
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| is_state_db_name(platform_plain_name(name)))
        && is_state_db_file(base, entry)
}

fn first_component_is_reserved(relative: &Path) -> bool {
    match relative.components().next() {
        Some(std::path::Component::Normal(first)) => first
            .to_str()
            .is_some_and(|name| is_state_db_name(platform_plain_name(name))),
        _ => false,
    }
}

/// 原拼写里以保留名作【中间目录】、且其前缀物理上就是 base 根的分量：不管后面跟的是
/// 普通段还是 `..`，`create_dir_all` 按原拼写逐段 mkdir 时都会先把它建出来（词法折叠只
/// 看最终落点，看不见这一步；Win32 打开前先词法消解，不受影响，一并拦不吃亏）。
/// 末段保留名由物理落点那条规则负责。
fn reserved_dir_in_raw_spelling(base: &Path, resolved: &Path) -> bool {
    let components: Vec<std::path::Component<'_>> = resolved.components().collect();
    let last = components.len().saturating_sub(1);
    components.iter().enumerate().any(|(index, comp)| {
        let std::path::Component::Normal(name) = comp else {
            return false;
        };
        if index >= last
            || !name
                .to_str()
                .is_some_and(|name| is_state_db_name(platform_plain_name(name)))
        {
            return false;
        }
        let prefix: PathBuf = components[..index]
            .iter()
            .map(|comp| comp.as_os_str())
            .collect();
        physical_normalize(&prefix) == base
    })
}

/// 按操作系统实际会打开的落点规范化路径：取最长【已存在】前缀 `canonicalize`（符号链接、
/// Win32 尾随点/空格归一、8.3 短名、`\\?\` / `\\.\` 前缀与大小写都在这一步归一），尚不存在
/// 的尾巴再词法折叠 `.` / `..` 接上（Win32 打开前就是这么消解的；POSIX 对不存在的中间段
/// 本来就会 ENOENT，按词法折叠只会偏向多拦不会漏拦）。先物理后词法的顺序与
/// `resolve_within_base` 一致——反过来先词法折叠会把 `link/../x` 折成 `x` 的兄弟，而内核
/// 是从 link 的目标往上走的。
///
/// 已存在前缀从【最长】往短找，而不是从前缀一段段往下拼：单独的卷前缀（`\\?\C:`、`\\.\C:`）
/// `exists()` 为假，从前往后会一段都接不上、整条路径退化成词法比较，`\\?\C:\WORK\AGENT`
/// 这类拼写就和 canonical base 对不上了。
///
/// 也是工作区外路径授权（`crate::access`）展示与缓存用的落点。
pub(crate) fn physical_normalize(path: &Path) -> PathBuf {
    let components: Vec<std::path::Component<'_>> = path.components().collect();
    let mut split = 0usize;
    let mut out = PathBuf::new();
    for len in (1..=components.len()).rev() {
        let candidate: PathBuf = components[..len]
            .iter()
            .map(|comp| comp.as_os_str())
            .collect();
        if candidate.exists() {
            out = fs::canonicalize(&candidate).unwrap_or(candidate);
            split = len;
            break;
        }
    }
    for comp in &components[split..] {
        push_lexical(&mut out, *comp);
    }
    out
}

/// 词法折叠一段：`.` 丢弃；`..` 回退一段普通分量，已到根 / 盘符就像内核一样原地不动
/// （`/..` 就是 `/`），只有相对路径开头的 `..` 原样保留（交给后面的比对去判假）。
fn push_lexical(out: &mut PathBuf, comp: std::path::Component<'_>) {
    match comp {
        std::path::Component::CurDir => {}
        std::path::Component::ParentDir => match out.components().next_back() {
            Some(std::path::Component::Normal(_)) => {
                out.pop();
            }
            Some(std::path::Component::RootDir | std::path::Component::Prefix(_)) => {}
            _ => out.push(comp.as_os_str()),
        },
        other => out.push(other.as_os_str()),
    }
}

/// Win32 打开文件时会静默丢弃文件名尾随的点与空格，`name:stream` 打开的是同一文件的
/// 备用数据流——比对保留名前先剥掉这些，拿到真正会被打开的主名。
#[cfg(windows)]
fn platform_plain_name(name: &str) -> &str {
    name.split(':')
        .next()
        .unwrap_or(name)
        .trim_end_matches(['.', ' '])
}

#[cfg(not(windows))]
fn platform_plain_name(name: &str) -> &str {
    name
}

/// 从 JSON 入参取字符串字段。
pub(super) fn get_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(|v| v.as_str())
}

pub(super) fn parse_optional_bool(input: &Value, key: &str) -> Result<bool, ToolResult> {
    match input.get(key) {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| ToolResult::err(format!("{key} 必须是布尔值"))),
    }
}

pub(super) type FileMutationLock = tokio::sync::Mutex<()>;

pub(super) fn file_mutation_lock(path: &str) -> Arc<FileMutationLock> {
    static LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Weak<FileMutationLock>>>> = OnceLock::new();
    let key = normalize_lock_path(path);
    let mut locks = LOCKS
        .get_or_init(|| StdMutex::new(HashMap::new()))
        .lock()
        .expect("file mutation lock map not poisoned");
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = Arc::new(FileMutationLock::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

pub(super) fn normalize_lock_path(path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    let normalized = if let Ok(canonical) = fs::canonicalize(&path) {
        canonical
    } else {
        let absolute = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };
        if let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) {
            if let Ok(canonical_parent) = fs::canonicalize(parent) {
                canonical_parent.join(name)
            } else {
                absolute
            }
        } else {
            absolute
        }
    };
    platform_lock_key(normalized)
}

#[cfg(windows)]
pub(super) fn platform_lock_key(path: PathBuf) -> PathBuf {
    // Windows paths are case-insensitive by default. Case-fold missing paths too:
    // `canonicalize` cannot normalize their spelling before create-only checks.
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

#[cfg(not(windows))]
pub(super) fn platform_lock_key(path: PathBuf) -> PathBuf {
    path
}

pub(super) fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn file_sha256(path: &str) -> Result<(String, u64), ToolResult> {
    // 先按 metadata 确认是普通文件再打开：FIFO 的 `open` 会一直等到有写端出现，字符设备
    // 则读不到头——打开之后再看类型已经晚了。打开本身非阻塞并按句柄再核对一次（两次之间
    // 被换掉），见 `open_regular_file`。
    if !fs::metadata(path)
        .map_err(|e| ToolResult::err(format!("读取版本失败 {path}: {e}")))?
        .is_file()
    {
        return Err(ToolResult::err(format!(
            "读取版本失败 {path}: 目标不是普通文件"
        )));
    }
    let mut file = open_regular_file(Path::new(path))
        .map_err(|e| ToolResult::err(format!("读取版本失败 {path}: {e}")))?;
    let metadata = file
        .metadata()
        .map_err(|e| ToolResult::err(format!("读取版本失败 {path}: {e}")))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| ToolResult::err(format!("读取版本失败 {path}: {e}")))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok((format!("{:x}", hasher.finalize()), metadata.len()))
}

pub(super) fn verify_expected_sha256(path: &str, expected: Option<&str>) -> Result<(), ToolResult> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if expected == "missing" {
        return match fs::metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ToolResult::err(format!("版本校验失败 {path}: {error}"))),
            Ok(_) => Err(ToolResult::err(format!(
                "VERSION_CONFLICT: expected_sha256=missing，但 {path} 已存在"
            ))),
        };
    }
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ToolResult::err(
            "expected_sha256 必须是 64 位十六进制 SHA-256，或字符串 \"missing\"",
        ));
    }
    let (actual, _) = match file_sha256(path) {
        Ok(value) => value,
        Err(_error) if !Path::new(path).exists() => {
            return Err(ToolResult::err(format!(
                "VERSION_CONFLICT: expected_sha256={expected}，但 {path} 不存在"
            )))
        }
        Err(error) => return Err(error),
    };
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(ToolResult::err(format!(
            "VERSION_CONFLICT: expected_sha256={expected}，actual_sha256={actual}；文件未修改"
        )))
    }
}

/// 判断本地文件/URL 的媒体类型和 MIME。
pub(super) fn media_type_from_name(name: &str) -> Option<(MediaKind, &'static str)> {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some((MediaKind::Image, "image/png")),
        "jpg" | "jpeg" => Some((MediaKind::Image, "image/jpeg")),
        "webp" => Some((MediaKind::Image, "image/webp")),
        "gif" => Some((MediaKind::Image, "image/gif")),
        "bmp" => Some((MediaKind::Image, "image/bmp")),
        "mp4" => Some((MediaKind::Video, "video/mp4")),
        "mov" => Some((MediaKind::Video, "video/quicktime")),
        "webm" => Some((MediaKind::Video, "video/webm")),
        "mpeg" | "mpg" => Some((MediaKind::Video, "video/mpeg")),
        "avi" => Some((MediaKind::Video, "video/x-msvideo")),
        "pdf" => Some((MediaKind::File, "application/pdf")),
        "doc" => Some((MediaKind::File, "application/msword")),
        "docx" => Some((
            MediaKind::File,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        )),
        "xls" => Some((MediaKind::File, "application/vnd.ms-excel")),
        "xlsx" => Some((
            MediaKind::File,
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        )),
        "ppt" => Some((MediaKind::File, "application/vnd.ms-powerpoint")),
        "pptx" => Some((
            MediaKind::File,
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        )),
        _ => None,
    }
}

pub(super) fn read_media_as_data_uri(path: &str, kind: MediaKind, mime: &str) -> ToolResult {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return ToolResult::err(format!("读取失败 {path}: {e}")),
    };
    if !meta.is_file() {
        return ToolResult::err(format!("无法读取 {path}: 目标不是普通文件"));
    }
    if meta.len() > MAX_MEDIA_FILE_SIZE {
        return ToolResult::err(format!(
            "媒体文件过大 ({} 字节 > 上限 {MAX_MEDIA_FILE_SIZE})。请改用 HTTPS URL 附加大文件/视频。",
            meta.len()
        ));
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => return ToolResult::err(format!("读取失败 {path}: {e}")),
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let url = format!("data:{mime};base64,{encoded}");
    let block = ContentBlock::media_url(kind, url);
    ToolResult::ok_with_attachments(
        format!("OK: 已读取媒体文件 {path}，类型 {mime}，将作为多模态输入附加给模型。"),
        vec![block],
    )
}

pub(super) fn attach_remote_media(url: &str) -> ToolResult {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return ToolResult::err("url 必须是 http(s) 地址");
    }
    let name = url.split('?').next().unwrap_or(url);
    let media = media_type_from_name(name).or_else(|| {
        let ext = Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match ext.as_str() {
            "txt" => Some((MediaKind::File, "text/plain")),
            "md" => Some((MediaKind::File, "text/markdown")),
            "csv" => Some((MediaKind::File, "text/csv")),
            "json" => Some((MediaKind::File, "application/json")),
            "html" | "htm" => Some((MediaKind::File, "text/html")),
            _ => None,
        }
    });
    let Some((kind, mime)) = media else {
        return ToolResult::err("无法从 url 扩展名判断媒体类型");
    };
    ToolResult::ok_with_attachments(
        format!("OK: 已附加远程媒体 {url}，推断类型 {mime}。"),
        vec![ContentBlock::media_url(kind, url.to_string())],
    )
}

/// 从 JSON 入参取可选的非负整数字段（offset 等）。
pub(super) fn parse_nonnegative_usize(
    input: &Value,
    key: &str,
) -> Result<Option<usize>, ToolResult> {
    let Some(v) = input.get(key) else {
        return Ok(None);
    };
    let Some(n) = v.as_u64() else {
        return Err(ToolResult::err(format!("{key} 必须是非负整数")));
    };
    usize::try_from(n)
        .map(Some)
        .map_err(|_| ToolResult::err(format!("{key} 过大")))
}

/// 从 JSON 入参取可选的正整数字段（limit / max_results 等）。
///
/// 调用方如果显式传了字段但值不是正整数，应得到清晰报错；否则 `limit=0` 会让
/// `read` 返回“offset 超出范围”，`max_results=0` 会让 `grep` 在未扫描任何内容时
/// 误报“未找到匹配”。
pub(super) fn parse_positive_usize(input: &Value, key: &str) -> Result<Option<usize>, ToolResult> {
    let Some(v) = input.get(key) else {
        return Ok(None);
    };
    let Some(n) = v.as_u64() else {
        return Err(ToolResult::err(format!("{key} 必须是正整数")));
    };
    if n == 0 {
        return Err(ToolResult::err(format!("{key} 必须大于 0")));
    }
    usize::try_from(n)
        .map(Some)
        .map_err(|_| ToolResult::err(format!("{key} 过大")))
}

/// 取目标元信息并确保是【普通文件】：拒绝目录 / FIFO / 字符设备等。
///
/// 目录、FIFO、设备文件在 `File::open` / `read_to_end` 上可能阻塞或读取特殊流，导致
/// 工具调用挂起。用 `metadata`（跟随符号链接）判断最终目标类型——指向普通文件的合法
/// 符号链接不会被误拒。[`read_text_bounded`]、[`read_lines_window`] 共用。
pub(super) fn regular_file_meta(path: &str) -> Result<fs::Metadata, ToolResult> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return Err(ToolResult::err(format!("读取失败 {path}: {e}"))),
    };
    if !meta.is_file() {
        let kind = if meta.is_dir() {
            "目录"
        } else {
            "特殊文件"
        };
        return Err(ToolResult::err(format!(
            "无法读取 {path}: 目标为{kind}，仅支持普通文本文件"
        )));
    }
    Ok(meta)
}

/// 只读打开一个【普通文件】，打开后按句柄再核对一次类型。
///
/// 调用方多半已经按 `metadata` 确认过是普通文件（[`regular_file_meta`]），但 `metadata` 与
/// `open` 之间路径可能被换成 FIFO / 设备：FIFO 的阻塞发生在 `open` 本身（一直等写端出现），
/// 事后核对拦不住。Unix 上带 `O_NONBLOCK` 打开：FIFO 立即返回、字符设备不等载波，再由句柄
/// 类型核对拒绝；普通文件的读取不受 `O_NONBLOCK` 影响。Windows 没有这一层阻塞（设备名在
/// 路径解析时已被 [`is_dos_device_name`] 拒掉），只做句柄核对。
pub fn open_regular_file(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "目标不是普通文件",
        ));
    }
    Ok(file)
}

/// 返回 ≤ `idx` 的最大 UTF-8 字符边界下标，用于「按字节截断但不劈开多字节字符」。
pub(super) fn floor_char_boundary(bytes: &[u8], mut idx: usize) -> usize {
    if idx > bytes.len() {
        idx = bytes.len();
    }
    // `idx` 是字符边界 ⟺ `bytes[..idx]` 是合法 UTF-8 前缀。UTF-8 最长 4 字节，
    // 故最多回退 3 次，整体开销极小。
    while idx > 0 && std::str::from_utf8(&bytes[..idx]).is_err() {
        idx -= 1;
    }
    idx
}

/// 整篇限流读取文本文件：先 metadata 拦截超大文件，再 `take(MAX+1)` 兜底读取。
///
/// 当前仅供 [`EditFile`] 使用——edit 必须拿到【完整原文】才能安全替换，故对超过
/// [`MAX_FILE_SIZE`] 的文件直接拒绝（无法整篇载入即无法可靠编辑）。[`Read`] 改用
/// [`read_lines_window`] 分页读取，不再受 32KB 整篇上限约束。
///
/// 体积拦截：超过 [`MAX_FILE_SIZE`] 的文件在 metadata 阶段即拒绝（不读不分配）；
/// 限额内文件才会整篇 `read_to_end` 载入（edit 需要完整原文）。`take(MAX+1)` 兜底防
/// metadata 对管道/特殊文件失真——读到 `MAX_FILE_SIZE + 1` 字节即判超限。
pub(super) fn read_text_bounded(path: &str) -> Result<String, ToolResult> {
    let meta = regular_file_meta(path)?;
    if meta.len() > MAX_FILE_SIZE as u64 {
        return Err(ToolResult::err(format!(
            "文件过大 ({} 字节 > 上限 {MAX_FILE_SIZE})",
            meta.len()
        )));
    }
    let f = match open_regular_file(Path::new(path)) {
        Ok(f) => f,
        Err(e) => return Err(ToolResult::err(format!("读取失败 {path}: {e}"))),
    };
    let mut bytes = Vec::new();
    if let Err(e) = f.take((MAX_FILE_SIZE + 1) as u64).read_to_end(&mut bytes) {
        return Err(ToolResult::err(format!("读取失败 {path}: {e}")));
    }
    if bytes.len() > MAX_FILE_SIZE {
        return Err(ToolResult::err(format!(
            "文件过大 ({} 字节 > 上限 {MAX_FILE_SIZE})",
            bytes.len()
        )));
    }
    match String::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(_) => Err(ToolResult::err("文件不是合法 UTF-8 文本")),
    }
}

/// `read` 分页读取的返回窗口。
pub(super) struct ReadWindow {
    /// 选中的行内容（保留原始换行）。
    pub(super) content: String,
    /// 本次扫描是否已读到 EOF。
    pub(super) reached_end: bool,
    /// 窗口起始行号（0 基）。
    pub(super) start_line: usize,
    /// 实际返回的行数。
    pub(super) lines_returned: usize,
    /// 是否因扫描字节数触顶而提前停止（offset 过大 / 文件病态冗长）。
    pub(super) scan_truncated: bool,
}

/// 流式分页读取：跳过 `offset` 行，返回至多 `limit` 行。
///
/// 与 [`read_text_bounded`] 不同，本函数【不把整篇读进内存再判超限】，而是用
/// `BufReader` 逐行扫描——只缓存当前行与已选中窗口，因而可安全处理远超 32KB 的大文件
/// （这正是旧实现 在 Termux 上 38KB 即报错的根因）。
///
/// 三层界限（与 list 的「深度+条目」双限流同源，保护 LLM 上下文预算）：
/// 1. `limit`：本次返回的最大行数（默认 [`READ_DEFAULT_LIMIT`]）；
/// 2. [`READ_MAX_OUTPUT_BYTES`]：本次输出的最大字节数——等价于旧的 32KB 上下文预算，
///    仅改为按「每次调用」而非「整个文件」计量；
/// 3. [`READ_MAX_SCAN_BYTES`]：本次扫描（含跳过的 offset 行）的最大字节数——兜底
///    「亿万空行」这类让 offset 跳过阶段空转的病态文件，避免工具调用挂起。
pub(super) fn read_lines_window(
    path: &str,
    offset: usize,
    limit: usize,
) -> Result<ReadWindow, ToolResult> {
    regular_file_meta(path)?;
    let f = match open_regular_file(Path::new(path)) {
        Ok(f) => f,
        Err(e) => return Err(ToolResult::err(format!("读取失败 {path}: {e}"))),
    };
    let mut reader = std::io::BufReader::new(f);

    let mut out: Vec<u8> = Vec::new();
    let mut line: Vec<u8> = Vec::new();
    let mut idx = 0usize; // 当前行号（0 基）
    let mut kept = 0usize; // 已纳入窗口的行数
    let mut bytes_out = 0usize; // 窗口输出字节数
    let mut scanned = 0usize; // 累计扫描字节数（含跳过行）
    let mut reached_end = false;
    let mut scan_truncated = false;

    loop {
        line.clear();
        let n = match reader.read_until(b'\n', &mut line) {
            Ok(n) => n,
            Err(e) => return Err(ToolResult::err(format!("读取失败 {path}: {e}"))),
        };
        if n == 0 {
            reached_end = true; // EOF
            break;
        }
        scanned += n;
        if idx >= offset {
            // 已进入请求窗口。
            if kept >= limit {
                break; // 行数预算用尽：其后是否还有更多未知 → 视为「可能未完」
            }
            if bytes_out + n > READ_MAX_OUTPUT_BYTES {
                // 输出字节触顶。单行即超预算时至少截断返回一段，避免空响应。
                if kept == 0 {
                    let s = match std::str::from_utf8(&line) {
                        Ok(s) => s,
                        Err(_) => return Err(ToolResult::err("文件不是合法 UTF-8 文本")),
                    };
                    let cut = floor_char_boundary(s.as_bytes(), READ_MAX_OUTPUT_BYTES.min(s.len()));
                    out.extend_from_slice(&s.as_bytes()[..cut]);
                    out.extend_from_slice("\n[单行过长，已截断]\n".as_bytes());
                    kept = 1;
                }
                break;
            }
            // 逐行 UTF-8 校验：`\n` 必为 UTF-8 边界，逐行校验等价于整篇校验。
            if std::str::from_utf8(&line).is_err() {
                return Err(ToolResult::err("文件不是合法 UTF-8 文本"));
            }
            out.extend_from_slice(&line);
            bytes_out += n;
            kept += 1;
        }
        idx += 1;
        if scanned >= READ_MAX_SCAN_BYTES {
            scan_truncated = true;
            break;
        }
    }

    let content = String::from_utf8(out).map_err(|_| ToolResult::err("文件不是合法 UTF-8 文本"))?;
    Ok(ReadWindow {
        content,
        reached_end,
        start_line: offset,
        lines_returned: kept,
        scan_truncated,
    })
}

/// 为 `read` 的分页结果拼「翻页提示」。
pub(super) fn read_window_note(w: &ReadWindow) -> String {
    let start = w.start_line;
    let end = start + w.lines_returned; // 半开区间末尾 = 下一段的 offset
    if w.scan_truncated {
        return format!(
            "[read: 已扫描上限 {READ_MAX_SCAN_BYTES} 字节仍未取得足够内容；\
             文件过大或 offset={start} 过大，请减小 offset 或改用 grep 搜索]"
        );
    }
    if w.lines_returned == 0 {
        return format!("[read: offset={start} 超出范围，无内容返回（请减小 offset）]");
    }
    if w.reached_end {
        format!("[read: 返回第 {}–{} 行，已到文件末尾]", start + 1, end)
    } else {
        format!(
            "[read: 返回第 {}–{} 行，仍有后续内容；继续读取请传 offset={end}]",
            start + 1,
            end
        )
    }
}
