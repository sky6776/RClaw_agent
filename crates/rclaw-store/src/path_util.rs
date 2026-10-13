//! 路径工具：session 键的百分号编码、agent 存储路径校验。
//!
//! - [`percent_encode_key`]：把 channel / chat_id 一一映射成合法文件名片段（避免串档）。
//!   会话正文本身在 `state.sqlite` 里、不再按会话落文件，现在只有会话摘要
//!   `memory/session_summaries/<ch>_<id>.md` 与 `/export` 的导出文件名还用它；
//! - [`validate_agent_path`] / [`resolve_within_base`]：工具读写文件时的沙箱校验。

use std::ffi::OsString;
use std::path::{Component, Path};

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

/// 「安全字符」集合：字母、数字、连字符、`.`、`~` 原样保留，其余 %XX 编码。
/// **一一映射**（旧实现用 '_' 替换是有损多对一，会串档）。
///
/// `_` 也要编码：调用方把两个键拼成 `<ch>_<id>`，分隔符若能出现在编码结果里，
/// `("a_b", "c")` 与 `("a", "b_c")` 就会撞成同一个文件名——编码掉它，`_` 在拼接结果里
/// 只可能是分隔符，整个文件名对 `(channel, chat_id)` 才是单射。
const SAFE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'/')
    .add(b'\\')
    .add(b'?')
    .add(b'#')
    .add(b'%')
    .add(b':')
    .add(b'*')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'|')
    .add(b'_');

/// 对 session 键（channel 或 chat_id）做百分号编码，作为会话摘要 / 导出文件的文件名片段。
///
/// 不同输入必定产生不同输出；结果里不含 `_`，所以用 `_` 拼接两个编码结果仍是单射，
/// 杜绝跨 channel/跨用户串档。
pub fn percent_encode_key(s: &str) -> String {
    utf8_percent_encode(s, SAFE).to_string()
}

/// 校验工具访问的路径：解析符号链接后必须落在 `base`（agent/）之内。
///
/// 路径校验，并覆盖【符号链接逃逸】攻击面。
///
/// 旧实现只做「字符串前缀 + 禁止 `..`」两项词法检查，挡得住词法穿越，却挡不住
/// `agent/link -> /etc` 这类指向 base 之外的符号链接：`agent/link/passwd` 能通过校验，
/// 实际却读到 `/etc/passwd`。
///
/// 本实现分两层：
/// 1. **词法层**：相对路径必须以 `base` 为前缀；绝对路径交给解析层判断是否位于
///    `base` 内。两者都不得含 `..` 路径分量（挡住尚未存在的词法穿越）；
/// 2. **解析层**（[`resolve_within_base`]）：规范化 `base`，并对 target 的「最长已存在祖先」
///    做 canonicalize，解析掉已落地路径里的符号链接；不存在的尾部分量按词法拼回，
///    最终判断落点是否仍在 canonical base 之内。
///
/// 返回 `true` 表示安全可用。
pub fn validate_agent_path(base: &Path, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let p = Path::new(path);
    // 相对路径必须显式带 base 前缀。base 本身可能来自默认配置 `agent`，此时位于
    // agent 内的绝对路径不可能在词法上 starts_with("agent")；绝对路径改由下面的
    // canonical 解析层判断，兼容工具 schema 所承诺的 agent 内绝对路径。
    if !p.is_absolute() && !p.starts_with(base) {
        return false;
    }
    // 禁止任何 `..` 路径分量（词法穿越，能挡住尚未创建的逃逸路径）。
    for comp in p.components() {
        if matches!(comp, Component::ParentDir) {
            return false;
        }
    }
    // 解析符号链接，确保最终落点仍在 base 之内。
    resolve_within_base(base, p)
}

/// 把 `base` 与 `target` 解析到「真实路径」后，判断 target 是否仍落在 base 之下。
///
/// - `base` 必须**存在**并 `canonicalize`（agent 目录由初始化保证存在）；
/// - 对 `target` 取其【最长已存在祖先】做 `canonicalize`——这一步会展开已落地路径里的
///   符号链接（攻击面），如 `agent/link -> /etc`；尚未创建的尾部分量（如待写入的新文件）
///   按词法拼回，由于上层已禁止 `..`，词法部分无法逃逸；
/// - 最终判断解析后的落点是否为 canonical base 的前缀。
///
/// 公开供 `skill_manage` 等需要「词法校验后再做符号链接解析」的写入面复用：仅做词法前缀 +
/// `..` 检查挡不住已落地的逃逸符号链接/junction，必须对最长已存在祖先 canonicalize。
///
/// 注：`validate` 与实际 `open` 之间存在 TOCTOU 窗口（校验后、打开前有人在路径上建了
/// 链接）；要彻底消除需 `O_NOFOLLOW` 逐段打开，超出本次修复范围。当前实现对「已存在的
/// 逃逸链接」这一主要威胁提供可靠拦截。
pub fn resolve_within_base(base: &Path, target: &Path) -> bool {
    // 1) 规范化 base。
    let cbase = match base.canonicalize() {
        Ok(c) => c,
        Err(_) => return false, // base 不存在 → 拒绝（agent 目录理应已初始化）。
    };
    // 2) 找 target 最长【已存在】祖先；不存在尾部按词法记录。
    let mut existing = target.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) if parent != existing => {
                tail.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => break, // 走到根仍未命中，交给下面 canonicalize 兜底。
        }
    }
    // 3) 规范化已存在祖先：解析其中可能藏着的符号链接。
    let mut resolved = match existing.canonicalize() {
        Ok(c) => c,
        Err(_) => return false,
    };
    // 4) 把不存在尾部正序拼回（tail 是逆序压栈的，需反转）。
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    // 5) 最终落点必须在 canonical base 之下。
    resolved.starts_with(&cbase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_is_injective() {
        // 三个在旧实现（'_' 替换）下会撞名的输入，编码后必须互不相同。
        let a = percent_encode_key("a/b");
        let b = percent_encode_key("a?b");
        let c = percent_encode_key("a_b");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
        // 下划线是拼接分隔符，必须编码掉，编码结果里不得再出现 `_`。
        assert_eq!(c, "a%5Fb");
        assert!(!percent_encode_key("x_y_z%_").contains('_'));
        assert_eq!(percent_encode_key("chat-1.2~x"), "chat-1.2~x");
    }

    /// `<ch>_<id>` 拼接对 `(channel, chat_id)` 必须是单射：`("a_b","c")` 与 `("a","b_c")`
    /// 不能撞成同一个会话摘要 / 导出文件名。
    #[test]
    fn joined_keys_are_injective() {
        let join = |ch: &str, id: &str| format!("{}_{}", percent_encode_key(ch), percent_encode_key(id));
        assert_ne!(join("a_b", "c"), join("a", "b_c"));
        assert_ne!(join("a", "b_c"), join("a_b", "c"));
        assert_ne!(join("", "a_b"), join("a", "b"));
        assert_eq!(join("a_b", "c").matches('_').count(), 1);
    }

    /// 基本校验：用真实（绝对、已存在）的 base，避免相对路径让 canonicalize 失败。
    #[test]
    fn validate_path_basic() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("agent");
        std::fs::create_dir_all(base.join("memory")).unwrap();

        // base 内的合法路径。
        let inside = base.join("memory").join("MEMORY.md");
        assert!(validate_agent_path(&base, inside.to_str().unwrap()));
        // 尚不存在的子路径也允许（待写入文件）。
        let future = base.join("skills").join("new.md");
        assert!(validate_agent_path(&base, future.to_str().unwrap()));
        // 绝对路径在 base 之外 → 拒绝。
        assert!(!validate_agent_path(&base, "/etc/passwd"));
        // 词法穿越 → 拒绝。
        let traversal = format!("{}/../etc", base.to_string_lossy());
        assert!(!validate_agent_path(&base, &traversal));
        // 空路径 → 拒绝。
        assert!(!validate_agent_path(&base, ""));
    }

    #[test]
    fn relative_base_accepts_absolute_path_inside_agent() {
        let cwd = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&cwd).unwrap();
        let base_absolute = dir.path().join("agent");
        std::fs::create_dir_all(base_absolute.join("skills")).unwrap();
        let base_relative = base_absolute.strip_prefix(&cwd).unwrap();

        let inside = base_absolute.join("skills").join("report.md");
        assert!(validate_agent_path(base_relative, inside.to_str().unwrap()));

        let outside = dir.path().join("outside.md");
        assert!(!validate_agent_path(
            base_relative,
            outside.to_str().unwrap()
        ));
    }

    /// 符号链接逃逸防御：base 内的链接指向 base 外，经它访问必须被拒。
    /// 仅在能创建符号链接的平台运行（Unix；Windows 需要权限，跳过）。
    #[cfg(unix)]
    #[test]
    fn validate_path_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("agent");
        std::fs::create_dir_all(&base).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        // agent/link -> <outside>（指向 base 之外）。
        symlink(&outside, base.join("link")).unwrap();

        let via_link = base.join("link").join("secret");
        assert!(
            !validate_agent_path(&base, via_link.to_str().unwrap()),
            "经逃逸符号链接的路径必须被拒绝"
        );
    }
}
