//! Per-turn 工具文件沙箱权限（tokio task-local）。
//!
//! 每个 turn 在独立 tokio task 里跑（`run_turn` → `tokio::spawn(run_turn_inner)`），
//! 用 task-local 携带当前消息的 [`ToolAccess`]：max QQ 的 turn 设 `Unrestricted`，
//! 其它设 `Sandboxed`。task-local 天然 per-task 隔离——避免不同 QQ 的 turn 并发时
//! 「受限用户读到 max 用户标志」的串档越权。
//!
//! 读不到时（后台 review fork、subagent、任何外部 `tokio::spawn`）默认回退
//! [`ToolAccess::Sandboxed`]——**fail-safe**：丢失上下文绝不越权。
//!
//! 存储类型用 [`ToolAccess`] 本身（而非 `Cell<ToolAccess>`）：多线程 runtime 下 task 可能
//! 跨线程迁移，task-local 值需 `Send`，`Cell` 非 `Send`；`ToolAccess` 是 `Copy` 枚举且
//! 只读访问，直接存值即可。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock};

use rclaw_core::ToolAccess;

use crate::files::helpers::{is_symlink_leaf, physical_normalize};
use crate::ToolResult;

tokio::task_local! {
    /// 当前 turn 的工具沙箱权限。
    static TURN_ACCESS: ToolAccess
}

/// 读取当前 task 的工具沙箱权限；读不到（不在 turn task 内）回退 [`ToolAccess::Sandboxed`]。
pub fn current_access() -> ToolAccess {
    TURN_ACCESS.try_with(|a| *a).unwrap_or_default()
}

/// 在给定权限下执行 future（供 turn spawn 点包裹 `run_turn_inner`）。
pub fn scope_access<F>(access: ToolAccess, fut: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    TURN_ACCESS.scope(access, fut)
}

// ---------------------------------------------------------------------------
// 工作区外路径的交互式授权（桌面版弹窗确认）。
//
// 默认权限（Sandboxed）下文件/shell 工具只许操作 agent 工作区；宿主（桌面壳）
// 经 [`set_path_confirm_hook`] 注册确认钩子后，工作区之外的路径会先向前端弹窗
// 询问用户——允许才放行，拒绝/超时/无钩子一律按拒绝处理（fail-safe，保持 CLI
// 等无界面环境的既有行为不变）。每次授权以（操作, 物理落点）缓存（进程生命周期内），
// 同一路径的翻页读等重复访问不再反复弹窗；读授权不顺带放行写 / 删。
//
// 弹窗展示、授权缓存都按路径的【物理落点】（最长已存在前缀 canonicalize、尾段词法接回，
// 见 `physical_normalize`）而不是请求拼写：工作区内一个指向外部的链接
// （`agent/notes/today.md -> ~/.ssh/id_rsa`）按拼写展示会像是工作区里的路径，用户很可能
// 点允许；按拼写缓存则把链接重定向到别处还能复用这次授权。落点取「本次操作真正触及
// 的对象」（见 `affected_target`）：删除一个符号链接只删条目本身，落点就是条目而不是
// 链接目标。缓存键还带授权种类（[`GrantKind`]）：对链接【条目】的删除授权与对同一路径上
// 【真实对象】的删除授权互不相认——否则批准删掉 `/outside/link` 这个链接之后，那里换成
// 真实目录再 `delete /outside/link` 就会免询问地 `remove_dir_all`。
// ---------------------------------------------------------------------------

/// 工作区外路径请求的操作类别：授权按（操作, 物理落点）缓存。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathOp {
    /// 读文件内容。
    Read,
    /// 写入 / 编辑（会覆盖或创建）。
    Write,
    /// 删除。
    Delete,
    /// 列目录。
    List,
    /// 目录内容搜索（grep）。
    Search,
    /// 作为 shell 命令的工作目录。
    ShellCwd,
}

impl PathOp {
    /// 弹窗文案里的操作名。
    pub fn label(self) -> &'static str {
        match self {
            Self::Read => "读取",
            Self::Write => "写入",
            Self::Delete => "删除",
            Self::List => "列目录",
            Self::Search => "搜索",
            Self::ShellCwd => "作为 shell 工作目录",
        }
    }
}

/// 一次工作区外路径的授权请求（交给宿主弹窗）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathConfirmRequest {
    /// 本次操作真正会触及的物理落点（展示形式：正斜杠、不带 Windows `\\?\` 前缀）。
    pub target: String,
    /// 工具请求时的拼写（同一展示形式）；与 `target` 不同说明中途经过链接 / 别名 / `..` 折叠。
    pub requested: String,
    /// 请求的操作。
    pub op: PathOp,
    /// 附加说明（如：删的是符号链接条目本身、不碰其目标）。
    pub note: Option<String>,
}

impl PathConfirmRequest {
    /// 弹窗用的一行文案：操作 + 物理落点，拼写不同时带上请求拼写，有附加说明时附上。
    pub fn display(&self) -> String {
        let mut text = if self.requested == self.target {
            format!("{}：{}", self.op.label(), self.target)
        } else {
            format!(
                "{}：{}（请求拼写 {} 指向它）",
                self.op.label(),
                self.target,
                self.requested
            )
        };
        if let Some(note) = &self.note {
            text.push_str(&format!("（{note}）"));
        }
        text
    }
}

/// 工作区外路径的授权询问结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutsideDecision {
    /// 用户允许（或命中既往授权缓存）。
    Allowed,
    /// 用户明确拒绝。
    UserDenied,
    /// 询问超时无人应答。
    Timeout,
    /// 宿主未注册确认钩子（CLI 等无界面环境）：直接拒绝。
    NoHook,
}

/// 钩子返回的 boxed future（避免 rclaw-tools 引入 futures 依赖）。
pub type ConfirmFuture = Pin<Box<dyn Future<Output = OutsideDecision> + Send>>;
/// 确认钩子：入参为授权请求（物理落点、请求拼写、操作），返回用户决定。
pub type PathConfirmHook = Arc<dyn Fn(PathConfirmRequest) -> ConfirmFuture + Send + Sync>;

/// 一条授权针对的是路径上的什么（见 [`affected_target`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum GrantKind {
    /// 跟随链接后落点上的对象：读 / 写 / 列目录 / 搜索 / shell cwd，以及删除一个真实文件 /
    /// 目录。
    Object,
    /// 符号链接条目本身（删除时叶子是链接：只摘条目，不碰目标）。键上的路径是条目所在目录
    /// 的物理路径 + 链接名——与同一位置上真实对象的 [`Self::Object`] 键字符串可能相同，靠
    /// 种类区分。
    Entry,
}

/// 授权缓存的键：物理落点（[`granted_key`] 归一后的字符串）+ 授权种类。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GrantKey {
    path: String,
    kind: GrantKind,
}

static PATH_CONFIRM_HOOK: OnceLock<RwLock<Option<PathConfirmHook>>> = OnceLock::new();
/// 已授权的（(物理落点键, 种类) → 操作集合）。
static GRANTED_PATHS: OnceLock<StdMutex<HashMap<GrantKey, HashSet<PathOp>>>> = OnceLock::new();

fn granted_paths() -> &'static StdMutex<HashMap<GrantKey, HashSet<PathOp>>> {
    GRANTED_PATHS.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// 注册工作区外路径确认钩子（进程级，只需且只能注册一次；重复注册返回 false）。
pub fn set_path_confirm_hook(hook: PathConfirmHook) -> bool {
    let mut guard = PATH_CONFIRM_HOOK
        .get_or_init(|| RwLock::new(None))
        .write()
        .expect("PATH_CONFIRM_HOOK 锁未被毒化");
    if guard.is_some() {
        return false;
    }
    *guard = Some(hook);
    true
}

/// 授权缓存的键：物理落点、正斜杠归一；Windows 另做小写归一（大小写不敏感文件系统），
/// 其余平台保留原拼写——大小写敏感，不同拼写是不同路径，不得互认授权。
fn granted_key(physical: &Path) -> String {
    let key = physical.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

/// 展示 / 缓存用的物理落点（见模块说明）：跟随链接后的落点。
fn physical_target(resolved: &Path) -> PathBuf {
    physical_normalize(resolved)
}

/// 本次操作真正会触及的物理落点、授权种类，以及弹窗附加说明。
///
/// 一般就是 `resolved` 跟随链接后的落点：读 / 列目录 / 搜索 / shell cwd 都顺着链接走；写也
/// 按落点算——`edit` 与带 `expected_sha256` 的 `write` 会先顺着链接把目标内容读上来，按链接
/// 目标弹窗是更强也更诚实的说法。只有【删除】且叶子是符号链接时例外：删除工具只删链接
/// 条目本身、一个字节都不碰目标（`remove_symlink`），落点取条目所在目录的物理路径 + 链接名
/// ——否则弹窗会说「删除：/home/u/dir」、授权也记在 `/home/u/dir` 上：用户批准的比实际发生
/// 的更重，而且之后直接 `delete /home/u/dir` 会免询问。这种授权记为 [`GrantKind::Entry`]：
/// 只在之后的请求同样是「删链接条目」时命中；同一位置换成真实目录 / 文件后再删，要重新问。
fn affected_target(
    resolved: &Path,
    op: PathOp,
    followed: &Path,
) -> (PathBuf, GrantKind, Option<String>) {
    if op == PathOp::Delete && is_symlink_leaf(resolved) {
        if let (Some(parent), Some(name)) = (resolved.parent(), resolved.file_name()) {
            let note = format!(
                "符号链接条目本身，指向 {}；不碰目标",
                display_path(followed)
            );
            return (
                physical_normalize(parent).join(name),
                GrantKind::Entry,
                Some(note),
            );
        }
    }
    (followed.to_path_buf(), GrantKind::Object, None)
}

/// 弹窗 / 文案里的路径写法：去掉 Windows verbatim 前缀（`\\?\`、`\\?\UNC\`），统一正斜杠。
/// `physical_normalize` 在 Windows 上出自 `canonicalize`、总带 `\\?\`：不剥掉的话每个弹窗
/// 都显示 `//?/C:/…`，而且与请求拼写永远不等——「请求拼写 … 指向它」这条本该提示链接 /
/// `..` 重定向的信号就 100% 触发、形同虚设。授权缓存键（[`granted_key`]）仍用规范形式。
fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text.into_owned()
    };
    text.replace('\\', "/")
}

/// 原拼写含 `..`（词法校验一律拒绝）但折叠后的落点仍在工作区内：`agent/sub/../notes.md`
/// 不是工作区外访问，不该按「工作区以外」弹窗。折叠按物理落点做（先解析已存在前缀里的
/// 链接再折 `..`），经链接落到工作区外的照旧走授权。
fn folds_into_workspace(base: &Path, resolved: &Path, followed: &Path) -> bool {
    resolved
        .components()
        .any(|comp| matches!(comp, std::path::Component::ParentDir))
        && rclaw_store::validate_agent_path(base, &followed.to_string_lossy())
}

/// 当前 task 现在就能读 `resolved` 吗（不询问用户）：Unrestricted、工作区内、或此前已被
/// 用户放行过（任一操作）的工作区外路径——按物理落点比对，与授权缓存同一把键。给「在
/// 工具执行之前就要碰目标文件」的调用方（回滚点捕获 before 内容）用——沙箱门还没过、
/// 用户还没点允许的路径，一个字节都不该读。只认对【对象】的授权：批准过删某个链接条目
/// （[`GrantKind::Entry`]）不代表那个位置上现在的真实文件可以读。
pub fn path_readable_now(base: &Path, resolved: &Path) -> bool {
    if current_access() == ToolAccess::Unrestricted {
        return true;
    }
    if rclaw_store::validate_agent_path(base, &resolved.to_string_lossy()) {
        return true;
    }
    let followed = physical_target(resolved);
    if folds_into_workspace(base, resolved, &followed) {
        return true;
    }
    let key = GrantKey {
        path: granted_key(&followed),
        kind: GrantKind::Object,
    };
    granted_paths()
        .lock()
        .expect("GRANTED_PATHS 锁未被毒化")
        .get(&key)
        .is_some_and(|ops| !ops.is_empty())
}

/// 为 `resolved` 上的 `op` 构造授权请求（展示形式见 [`display_path`]）。
fn confirm_request(
    resolved: &Path,
    op: PathOp,
    physical: &Path,
    note: Option<String>,
) -> PathConfirmRequest {
    PathConfirmRequest {
        target: display_path(physical),
        requested: display_path(resolved),
        op,
        note,
    }
}

/// 沙箱 gate：`Unrestricted` 直放行；工作区内路径直放行（含只因拼写带 `..` 没过词法校验、
/// 折叠后仍在工作区内的路径，见 [`folds_into_workspace`]）；其余经钩子询问用户
/// （命中（操作, 物理落点）授权缓存则免询问）。`deny` 为调用方的既有拒绝文案（无钩子时
/// 原样返回，保持历史行为与测试断言不变），用户拒绝/超时在其后追加原因。
pub async fn ensure_path_allowed(
    base: &Path,
    resolved: &Path,
    op: PathOp,
    deny: &str,
) -> Result<(), ToolResult> {
    if current_access() == ToolAccess::Unrestricted {
        return Ok(());
    }
    if rclaw_store::validate_agent_path(base, &resolved.to_string_lossy()) {
        return Ok(());
    }
    let followed = physical_target(resolved);
    if folds_into_workspace(base, resolved, &followed) {
        return Ok(());
    }
    let (physical, kind, note) = affected_target(resolved, op, &followed);
    let key = GrantKey {
        path: granted_key(&physical),
        kind,
    };
    if granted_paths()
        .lock()
        .expect("GRANTED_PATHS 锁未被毒化")
        .get(&key)
        .is_some_and(|ops| ops.contains(&op))
    {
        return Ok(());
    }
    let request = confirm_request(resolved, op, &physical, note);
    // 先克隆 Arc 并释放锁，再 await——std 锁的 guard 不能跨 await（future 须 Send）。
    let hook = PATH_CONFIRM_HOOK
        .get_or_init(|| RwLock::new(None))
        .read()
        .expect("PATH_CONFIRM_HOOK 锁未被毒化")
        .as_ref()
        .cloned();
    let decision = match hook {
        Some(hook) => hook(request).await,
        None => OutsideDecision::NoHook,
    };
    match decision {
        OutsideDecision::Allowed => {
            granted_paths()
                .lock()
                .expect("GRANTED_PATHS 锁未被毒化")
                .entry(key)
                .or_default()
                .insert(op);
            Ok(())
        }
        OutsideDecision::UserDenied => Err(crate::ToolResult::err(format!(
            "{deny}（用户拒绝授权访问工作区之外的路径）"
        ))),
        OutsideDecision::Timeout => Err(crate::ToolResult::err(format!(
            "{deny}（授权请求超时未响应）"
        ))),
        OutsideDecision::NoHook => Err(crate::ToolResult::err(deny.to_string())),
    }
}

/// 测试用：直接登记一条对象授权（不经弹窗），按跟随链接后的落点记键。
#[cfg(test)]
fn grant_for_tests(resolved: &Path, op: PathOp) {
    grant_for_tests_keyed(&physical_target(resolved), GrantKind::Object, op);
}

/// 测试用：按给定的物理落点与种类直接登记（不再归一 / 跟随链接）。
#[cfg(test)]
fn grant_for_tests_keyed(physical: &Path, kind: GrantKind, op: PathOp) {
    granted_paths()
        .lock()
        .expect("GRANTED_PATHS 锁未被毒化")
        .entry(GrantKey {
            path: granted_key(physical),
            kind,
        })
        .or_default()
        .insert(op);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 授权缓存与 `path_readable_now` 按物理落点比对：`..` 折叠 / 链接拼写与直接拼写是同一把键。
    #[tokio::test]
    async fn grants_are_keyed_by_physical_target() {
        let dir = std::env::temp_dir().join(format!("rclaw-access-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("base")).unwrap();
        std::fs::create_dir_all(dir.join("outside/sub")).unwrap();
        std::fs::write(dir.join("outside/real.txt"), b"x").unwrap();
        let base = dir.join("base");
        let real = dir.join("outside/real.txt");
        let folded = dir.join("outside/sub/../real.txt");
        assert!(!path_readable_now(&base, &real));
        assert!(!path_readable_now(&base, &folded));
        grant_for_tests(&folded, PathOp::Read);
        assert!(path_readable_now(&base, &real), "按物理落点命中");
        assert!(path_readable_now(&base, &folded));
        #[cfg(unix)]
        {
            let link = dir.join("outside/link.txt");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(path_readable_now(&base, &link), "链接拼写解析到同一落点");
            let other = dir.join("outside/other.txt");
            std::fs::write(&other, b"y").unwrap();
            assert!(!path_readable_now(&base, &other));
        }
        // 没有钩子：工作区外路径仍按既有文案拒绝，已授权的路径按（操作）放行。
        let denied = ensure_path_allowed(&base, &real, PathOp::Write, "deny-text").await;
        assert_eq!(denied.unwrap_err().content, "deny-text", "读授权不放行写");
        assert!(
            ensure_path_allowed(&base, &folded, PathOp::Read, "deny-text")
                .await
                .is_ok()
        );
        // 拼写带 `..` 但折叠后仍在工作区内：不是工作区外访问，不经钩子（无钩子也放行）。
        std::fs::create_dir_all(base.join("sub")).unwrap();
        let inside = base.join("sub/../inside.md");
        assert!(
            ensure_path_allowed(&base, &inside, PathOp::Write, "deny-text")
                .await
                .is_ok()
        );
        assert!(path_readable_now(&base, &inside));
        // 折叠后越出工作区的照旧拒绝。
        let escaped = base.join("sub/../../outside/other2.txt");
        assert_eq!(
            ensure_path_allowed(&base, &escaped, PathOp::Read, "deny-text")
                .await
                .unwrap_err()
                .content,
            "deny-text"
        );
        assert!(!path_readable_now(&base, &escaped));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirm_request_display_shows_target_and_differing_spelling() {
        let same = PathConfirmRequest {
            target: "/home/u/notes.md".to_string(),
            requested: "/home/u/notes.md".to_string(),
            op: PathOp::Read,
            note: None,
        };
        assert_eq!(same.display(), "读取：/home/u/notes.md");
        let via_link = PathConfirmRequest {
            target: "/home/u/.ssh/id_rsa".to_string(),
            requested: "/work/agent/notes/today.md".to_string(),
            op: PathOp::Write,
            note: None,
        };
        assert_eq!(
            via_link.display(),
            "写入：/home/u/.ssh/id_rsa（请求拼写 /work/agent/notes/today.md 指向它）"
        );
        let link_entry = PathConfirmRequest {
            target: "/work/agent/link".to_string(),
            requested: "/work/agent/link".to_string(),
            op: PathOp::Delete,
            note: Some("符号链接条目本身，指向 /home/u/dir；不碰目标".to_string()),
        };
        assert_eq!(
            link_entry.display(),
            "删除：/work/agent/link（符号链接条目本身，指向 /home/u/dir；不碰目标）"
        );
    }

    /// 弹窗里的路径不带 Windows verbatim 前缀：`physical_normalize` 出自 `canonicalize`
    /// （`\\?\C:\…`），不剥掉的话请求拼写与落点永远不等，「请求拼写 … 指向它」就 100% 触发。
    #[test]
    fn display_path_strips_verbatim_prefix() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\u\x.txt")),
            "C:/Users/u/x.txt"
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\srv\share\f.txt")),
            "//srv/share/f.txt"
        );
        assert_eq!(display_path(Path::new("/home/u/x.txt")), "/home/u/x.txt");
        assert_eq!(display_path(Path::new(r"C:\plain\x.txt")), "C:/plain/x.txt");
    }

    /// 真实文件系统上的请求：普通绝对拼写的落点与拼写展示一致（Windows 上尤其要验证
    /// `\\?\` 已剥掉），只有经链接 / `..` 到达时才带「请求拼写」。
    #[test]
    fn confirm_request_for_plain_absolute_spelling_has_no_redirect_note() {
        let dir = std::env::temp_dir().join(format!("rclaw-confirm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("outside/sub")).unwrap();
        std::fs::write(dir.join("outside/real.txt"), b"x").unwrap();
        let real = std::fs::canonicalize(dir.join("outside/real.txt")).unwrap();
        let plain = PathBuf::from(display_path(&real).replace('/', std::path::MAIN_SEPARATOR_STR));
        let followed = physical_target(&plain);
        let (physical, kind, note) = affected_target(&plain, PathOp::Read, &followed);
        assert_eq!(kind, GrantKind::Object);
        let request = confirm_request(&plain, PathOp::Read, &physical, note);
        assert_eq!(request.target, request.requested, "{request:?}");
        assert!(!request.target.contains("//?/"), "{request:?}");
        assert_eq!(request.display(), format!("读取：{}", request.target));
        let folded = dir.join("outside/sub/../real.txt");
        let followed = physical_target(&folded);
        let (physical, _, note) = affected_target(&folded, PathOp::Read, &followed);
        let request = confirm_request(&folded, PathOp::Read, &physical, note);
        assert_ne!(request.target, request.requested);
        assert!(
            request.display().contains("（请求拼写 "),
            "{}",
            request.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 删除符号链接只删条目：落点与授权键是条目本身（在它所在目录的物理路径下，种类
    /// Entry），弹窗说明指向哪里；之后直接删链接目标不会沿用这次授权。写 / 读仍按链接目标
    /// 算。条目授权与同一位置上真实对象的删除授权互不相认（两个方向都是），条目授权也不
    /// 让那个位置上现在的真实文件变得可读。
    #[cfg(unix)]
    #[tokio::test]
    async fn deleting_a_symlink_is_scoped_to_the_link_entry() {
        let dir = std::env::temp_dir().join(format!("rclaw-linkdel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("base")).unwrap();
        std::fs::create_dir_all(dir.join("outside/dir")).unwrap();
        let base = dir.join("base");
        let target_dir = dir.join("outside/dir");
        let link = base.join("link");
        std::os::unix::fs::symlink(&target_dir, &link).unwrap();
        let followed = physical_target(&link);
        assert_eq!(followed, std::fs::canonicalize(&target_dir).unwrap());
        let entry_key = std::fs::canonicalize(&base).unwrap().join("link");
        let (physical, kind, note) = affected_target(&link, PathOp::Delete, &followed);
        assert_eq!((physical, kind), (entry_key.clone(), GrantKind::Entry));
        assert!(
            note.as_deref().unwrap().contains("符号链接条目本身"),
            "{note:?}"
        );
        let (physical, kind, note) = affected_target(&link, PathOp::Write, &followed);
        assert_eq!(
            (physical, kind, note),
            (followed.clone(), GrantKind::Object, None),
            "写按链接目标算"
        );
        // 对链接条目的删除授权不放行对目标的删除（也不放行对链接的写）。
        grant_for_tests_keyed(&entry_key, GrantKind::Entry, PathOp::Delete);
        assert!(
            ensure_path_allowed(&base, &link, PathOp::Delete, "deny-text")
                .await
                .is_ok()
        );
        assert_eq!(
            ensure_path_allowed(&base, &target_dir, PathOp::Delete, "deny-text")
                .await
                .unwrap_err()
                .content,
            "deny-text"
        );
        assert_eq!(
            ensure_path_allowed(&base, &link, PathOp::Write, "deny-text")
                .await
                .unwrap_err()
                .content,
            "deny-text"
        );
        // 工作区外的链接条目：批准删条目之后，同一位置换成真实文件再删要重新问，也不因此
        // 可读；反过来批准删真实文件之后换成链接条目，同样重新问。
        let outside_link = dir.join("outside/link2");
        std::os::unix::fs::symlink(&target_dir, &outside_link).unwrap();
        let outside_entry_key = std::fs::canonicalize(dir.join("outside"))
            .unwrap()
            .join("link2");
        grant_for_tests_keyed(&outside_entry_key, GrantKind::Entry, PathOp::Delete);
        assert!(
            ensure_path_allowed(&base, &outside_link, PathOp::Delete, "deny-text")
                .await
                .is_ok()
        );
        std::fs::remove_file(&outside_link).unwrap();
        std::fs::write(&outside_link, b"real now").unwrap();
        assert_eq!(
            ensure_path_allowed(&base, &outside_link, PathOp::Delete, "deny-text")
                .await
                .unwrap_err()
                .content,
            "deny-text",
            "条目授权不覆盖同一位置上的真实文件"
        );
        assert!(
            !path_readable_now(&base, &outside_link),
            "条目授权不让真实文件变得可读"
        );
        let outside_real = dir.join("outside/real2.txt");
        std::fs::write(&outside_real, b"x").unwrap();
        grant_for_tests(&outside_real, PathOp::Delete);
        assert!(
            ensure_path_allowed(&base, &outside_real, PathOp::Delete, "deny-text")
                .await
                .is_ok()
        );
        std::fs::remove_file(&outside_real).unwrap();
        std::os::unix::fs::symlink(&target_dir, &outside_real).unwrap();
        assert_eq!(
            ensure_path_allowed(&base, &outside_real, PathOp::Delete, "deny-text")
                .await
                .unwrap_err()
                .content,
            "deny-text",
            "对象授权不覆盖同一位置上的链接条目"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 授权缓存键区分种类：同一物理路径字符串上的条目授权与对象授权是两把键（Windows 上也
    /// 能跑：不需要真的建链接）。
    #[tokio::test]
    async fn entry_and_object_grants_are_distinct_keys() {
        let dir = std::env::temp_dir().join(format!("rclaw-grantkind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("base")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        std::fs::write(dir.join("outside/x.txt"), b"x").unwrap();
        let base = dir.join("base");
        let real = dir.join("outside/x.txt");
        let physical = physical_target(&real);
        grant_for_tests_keyed(&physical, GrantKind::Entry, PathOp::Delete);
        assert_eq!(
            ensure_path_allowed(&base, &real, PathOp::Delete, "deny-text")
                .await
                .unwrap_err()
                .content,
            "deny-text",
            "真实文件的删除不命中条目授权"
        );
        assert!(!path_readable_now(&base, &real));
        grant_for_tests_keyed(&physical, GrantKind::Object, PathOp::Delete);
        assert!(
            ensure_path_allowed(&base, &real, PathOp::Delete, "deny-text")
                .await
                .is_ok()
        );
        assert!(path_readable_now(&base, &real));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn scope_sets_access_within() {
        // scope 外默认受限。
        assert_eq!(current_access(), ToolAccess::Sandboxed);
        let inner = scope_access(ToolAccess::Unrestricted, async { current_access() }).await;
        assert_eq!(inner, ToolAccess::Unrestricted);
        // scope 结束后回到默认。
        assert_eq!(current_access(), ToolAccess::Sandboxed);
    }

    #[tokio::test]
    async fn spawned_task_does_not_inherit() {
        // spawn 的子任务不继承 task-local → fail-safe 回退 Sandboxed。
        // 这保证 subagent / 后台 review 等独立 task 绝不越权继承 max。
        scope_access(ToolAccess::Unrestricted, async {
            let child = tokio::spawn(async { current_access() }).await.unwrap();
            assert_eq!(child, ToolAccess::Sandboxed);
        })
        .await;
    }
}
