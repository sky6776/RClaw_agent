//! 全局工作流存档目录解析（跨项目共享）。
//!
//! 解析顺序与桌面端 `desktop/home.rs` 一致：`RCLAW_HOME` > Windows
//! `%USERPROFILE%\.rclaw` > `$XDG_CONFIG_HOME/rclaw` > `~/.config/rclaw`。
//! Android APK 没有 `%USERPROFILE%`：`RCLAW_ANDROID=1` 时优先使用 `Layout.base`
//! 同级的 app-private `.global` 目录，避免把全局存档写进不可写路径。
//!
//! 本模块只做路径解析，不创建目录（调用方按需 `create_dir_all`）。

use std::path::{Path, PathBuf};

/// Windows 下挂在用户主目录下的数据目录名（与桌面端一致）。
pub const WIN_DIR_NAME: &str = ".rclaw";

/// 数据目录名（挂在 XDG 配置根下）。
pub const DIR_NAME: &str = "rclaw";

/// 解析全局工作流根目录。
///
/// `layout_base` 是当前 agent 工作区（`Layout.base`），只在 Android 与完全无环境变量
/// 的兜底路径上使用。
pub fn workflow_home(layout_base: Option<&Path>) -> PathBuf {
    workflow_home_from_env(
        std::env::var("RCLAW_HOME").ok(),
        std::env::var("USERPROFILE").ok(),
        std::env::var("XDG_CONFIG_HOME").ok(),
        std::env::var("HOME").ok(),
        std::env::var("RCLAW_ANDROID")
            .ok()
            .is_some_and(|value| value.trim() == "1"),
        layout_base,
    )
}

/// 纯函数解析（供单测注入环境变量与 Android 标志）。
pub fn workflow_home_from_env(
    rclaw_home: Option<String>,
    userprofile: Option<String>,
    xdg_config_home: Option<String>,
    home: Option<String>,
    android: bool,
    layout_base: Option<&Path>,
) -> PathBuf {
    if let Some(path) = rclaw_home.filter(|path| !path.trim().is_empty()) {
        return PathBuf::from(path);
    }
    if android {
        // Android 没有用户主目录语义；app-private 的 Layout.base 是唯一稳定可写的位置。
        if let Some(base) = layout_base {
            return base.join(".global");
        }
    }
    if let Some(path) = userprofile.filter(|path| !path.trim().is_empty()) {
        return PathBuf::from(path).join(WIN_DIR_NAME);
    }
    if let Some(path) = xdg_config_home.filter(|path| !path.trim().is_empty()) {
        return PathBuf::from(path).join(DIR_NAME);
    }
    if let Some(path) = home.filter(|path| !path.trim().is_empty()) {
        return PathBuf::from(path).join(".config").join(DIR_NAME);
    }
    if let Some(base) = layout_base {
        return base.join(".global");
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(WIN_DIR_NAME)
}

/// 全局工作流存档目录（`<home>/workflows`）。
pub fn workflows_dir(layout_base: Option<&Path>) -> PathBuf {
    workflow_home(layout_base).join("workflows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rclaw_home_wins() {
        let home = workflow_home_from_env(
            Some("/tmp/custom".into()),
            Some("/home/u".into()),
            None,
            None,
            false,
            Some(Path::new("/work")),
        );
        assert_eq!(home, PathBuf::from("/tmp/custom"));
    }

    #[test]
    fn windows_profile_uses_dot_rclaw() {
        let home = workflow_home_from_env(
            None,
            Some("C:\\Users\\u".into()),
            None,
            None,
            false,
            Some(Path::new("C:\\work\\agent")),
        );
        assert_eq!(home, PathBuf::from("C:\\Users\\u").join(WIN_DIR_NAME));
    }

    #[test]
    fn android_falls_back_to_layout_base() {
        let home = workflow_home_from_env(
            None,
            Some("C:\\Users\\u".into()),
            None,
            None,
            true,
            Some(Path::new("/data/user/0/app/files/agent")),
        );
        assert_eq!(
            home,
            PathBuf::from("/data/user/0/app/files/agent").join(".global")
        );
    }

    #[test]
    fn xdg_then_home() {
        let xdg = workflow_home_from_env(
            None,
            None,
            Some("/xdg".into()),
            Some("/home/u".into()),
            false,
            None,
        );
        assert_eq!(xdg, PathBuf::from("/xdg").join(DIR_NAME));
        let home = workflow_home_from_env(None, None, None, Some("/home/u".into()), false, None);
        assert_eq!(home, PathBuf::from("/home/u/.config").join(DIR_NAME));
    }
}
