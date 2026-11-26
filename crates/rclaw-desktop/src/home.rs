//! 桌面版数据目录解析。
//!
//! 双击 exe 启动时 CWD 不可依赖，全部持久化数据落在用户主目录：
//! 优先级 `RCLAW_HOME` 环境变量 > `%USERPROFILE%\.rclaw`（Windows）>
//! `$XDG_CONFIG_HOME/rclaw` > `~/.config/rclaw`（非 Windows 兜底）。

use std::path::{Path, PathBuf};

/// Windows 下挂在用户主目录下的数据目录名（`%USERPROFILE%\.rclaw`）。
#[cfg_attr(all(target_os = "android", feature = "apk-host"), allow(dead_code))]
pub const WIN_DIR_NAME: &str = ".rclaw";

/// 数据目录名（挂在 XDG 配置根下）。
pub const DIR_NAME: &str = "rclaw";

/// APK 宿主：数据目录 = Android `Context.filesDir`（nativeStart 记录）；
/// `RCLAW_HOME` 环境变量兜底（调试通道）。
#[cfg(all(target_os = "android", feature = "apk-host"))]
pub fn resolve_home() -> PathBuf {
    if let Some(home) = std::env::var("RCLAW_HOME")
        .ok()
        .filter(|p| !p.trim().is_empty())
    {
        return PathBuf::from(home);
    }
    // MainActivity 在 super.onCreate 前已 nativeInit 记录上下文；短自旋兜底
    // （理论上不等待，防御服务/装配线程的极端竞态）。
    for _ in 0..100 {
        if let Some(context) = rclaw_android::host_context() {
            return context.files_dir;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("Android 宿主上下文未初始化：nativeInit 必须先于 GUI 装配调用")
}

/// 解析用户数据目录；全部环境变量缺失时 panic（桌面环境不可能发生）。
#[cfg(not(all(target_os = "android", feature = "apk-host")))]
pub fn resolve_home() -> PathBuf {
    home_from_env(
        std::env::var("RCLAW_HOME").ok(),
        std::env::var("USERPROFILE").ok(),
        std::env::var("XDG_CONFIG_HOME").ok(),
        std::env::var("HOME").ok(),
    )
    .expect("无法定位用户数据目录（RCLAW_HOME / USERPROFILE / XDG_CONFIG_HOME / HOME 均缺失）")
}

/// 纯函数解析（供单测注入环境变量）。
#[cfg_attr(all(target_os = "android", feature = "apk-host"), allow(dead_code))]
pub fn home_from_env(
    rclaw_home: Option<String>,
    userprofile: Option<String>,
    xdg_config_home: Option<String>,
    home: Option<String>,
) -> Option<PathBuf> {
    if let Some(p) = rclaw_home.filter(|p| !p.trim().is_empty()) {
        return Some(PathBuf::from(p));
    }
    if let Some(p) = userprofile.filter(|p| !p.trim().is_empty()) {
        return Some(PathBuf::from(p).join(WIN_DIR_NAME));
    }
    if let Some(p) = xdg_config_home.filter(|p| !p.trim().is_empty()) {
        return Some(PathBuf::from(p).join(DIR_NAME));
    }
    home.map(|h| PathBuf::from(h).join(".config").join(DIR_NAME))
}

/// `rclaw.toml` 的完整路径。
pub fn config_path(home: &Path) -> PathBuf {
    home.join("rclaw.toml")
}

/// 旧版布局（`%APPDATA%\rclaw`）一次性迁移到新 home（`%USERPROFILE%\.rclaw`）。
///
/// 仅当新 home 尚无 `rclaw.toml` 而旧目录有（老版本用户升级）时执行；否则不动
/// 任何东西。须在 `create_dir_all(home)` 之前调用（rename 要求目标不存在）。
/// 失败只告警不阻断启动（走首次运行向导重新配置，旧目录原样保留）——
/// 不能用 eprintln!：GUI 子系统的 stderr 句柄无效，写入失败会 panic。
#[cfg_attr(all(target_os = "android", feature = "apk-host"), allow(dead_code))]
pub fn migrate_legacy_appdata(home: &Path) {
    if config_path(home).exists() {
        return;
    }
    let Some(appdata) = std::env::var("APPDATA")
        .ok()
        .filter(|p| !p.trim().is_empty())
    else {
        return; // 非 Windows：无旧布局可迁。
    };
    let legacy = PathBuf::from(appdata).join(DIR_NAME);
    if let Err(e) = migrate_legacy_dir(&legacy, home) {
        tracing::warn!("旧数据目录迁移失败（已保留 {}）: {e}", legacy.display());
    }
}

/// 迁移的实现体（legacy 目录 → home，供单测注入路径）。
/// rename 失败（home 非空或跨卷）退化为递归复制合并，旧目录保留由用户处置。
#[cfg_attr(all(target_os = "android", feature = "apk-host"), allow(dead_code))]
fn migrate_legacy_dir(legacy: &Path, home: &Path) -> std::io::Result<()> {
    if !config_path(legacy).exists() || config_path(home).exists() {
        return Ok(());
    }
    // home 若已被建出（至多为空目录）先移掉，rename 才能落位；
    // 移不掉（非空但无配置）或跨卷 rename 失败 → 复制合并兜底。
    if home.exists() && std::fs::remove_dir(home).is_err() {
        return copy_dir(legacy, home, std::path::Path::new(""));
    }
    match std::fs::rename(legacy, home) {
        Ok(()) => Ok(()),
        Err(_) => copy_dir(legacy, home, std::path::Path::new("")),
    }
}

/// 递归复制目录（rename 跨卷失败时的迁移兜底）。
#[cfg_attr(all(target_os = "android", feature = "apk-host"), allow(dead_code))]
fn copy_dir(src: &Path, dst: &Path, rel: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    let mut entries: Vec<(std::ffi::OsString, std::path::PathBuf, bool)> = Vec::new();
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let is_dir = entry.file_type()?.is_dir();
        if !is_dir && is_internal_lock(rel, &name) {
            continue;
        }
        entries.push((name, entry.path(), is_dir));
    }
    // 配置必须最后发布：它存在即代表「已迁移」，先发布会让中途失败的迁移被
    // 下次启动整体跳过，留下缺失的数据文件。其它文件先补齐（重复迁移会跳过
    // 已完整发布的同名文件），配置落位即完成。
    entries.sort_by_key(|(name, _, _)| name == "rclaw.toml");
    for (_, path, is_dir) in entries {
        let target = dst.join(path.file_name().unwrap_or_default());
        if is_dir {
            copy_dir(
                &path,
                &target,
                &rel.join(path.file_name().unwrap_or_default()),
            )?;
        } else {
            copy_new(&path, &target)?;
        }
    }
    Ok(())
}

/// 只排除 rclaw 自己的运行时锁文件，且按相对旧数据根的真实路径判断：
/// 用户工作区里同名的业务锁文件不受影响。
fn is_internal_lock(rel: &Path, name: &std::ffi::OsStr) -> bool {
    match (rel.as_os_str().is_empty(), name.to_str()) {
        (true, Some("rclaw.toml.lock")) => true,
        (false, Some("storage.lock" | "engine.lock")) => rel == Path::new("sync"),
        _ => false,
    }
}

/// 复制单个文件：先写同目录临时文件，再以无覆盖方式原子发布。
///
/// 目标已存在（另一实例/更早一次迁移已发布）时跳过，不覆盖；进程中断最多留下
/// 一个临时文件，不会留下半份正式文件。
fn copy_new(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    let parent = dst.parent().unwrap_or_else(|| std::path::Path::new("."));
    let mut input = std::fs::File::open(src)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".rclaw-migrate-")
        .tempfile_in(parent)?;
    std::io::copy(&mut input, tmp.as_file_mut())?;
    tmp.as_file().sync_all()?;
    match tmp.persist_noclobber(dst) {
        Ok(_file) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error),
    }
}

/// 是否已完成首运行配置。/// 是否已完成首运行配置。
pub fn has_config(home: &Path) -> bool {
    config_path(home).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rclaw_home_env_wins() {
        let got = home_from_env(
            Some("D:\\data\\rclaw".into()),
            Some("C:\\AppData".into()),
            None,
            None,
        );
        assert_eq!(got, Some(PathBuf::from("D:\\data\\rclaw")));
    }

    #[test]
    fn userprofile_used_on_windows_layout() {
        let got = home_from_env(None, Some("C:\\Users\\u".into()), None, None);
        assert_eq!(got, Some(PathBuf::from("C:\\Users\\u").join(WIN_DIR_NAME)));
    }

    #[test]
    fn xdg_then_home_fallback() {
        let xdg = home_from_env(None, None, Some("/cfg".into()), Some("/home/u".into()));
        assert_eq!(xdg, Some(PathBuf::from("/cfg").join(DIR_NAME)));
        let home_only = home_from_env(None, None, None, Some("/home/u".into()));
        assert_eq!(
            home_only,
            Some(PathBuf::from("/home/u").join(".config").join(DIR_NAME))
        );
    }

    #[test]
    fn blank_values_are_skipped() {
        let got = home_from_env(Some("  ".into()), Some("C:\\AppData".into()), None, None);
        assert_eq!(
            got,
            Some(PathBuf::from("C:\\AppData").join(WIN_DIR_NAME)),
            "空白环境变量应跳过"
        );
        assert_eq!(home_from_env(None, None, None, None), None);
    }

    #[test]
    fn legacy_dir_migrates_when_new_home_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("legacy");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(legacy.join("agent")).unwrap();
        std::fs::write(legacy.join("rclaw.toml"), "x = 1").unwrap();
        std::fs::write(legacy.join("agent").join("desktop_prefs.json"), "{}").unwrap();

        migrate_legacy_dir(&legacy, &home).unwrap();
        assert!(config_path(&home).exists());
        assert!(home.join("agent").join("desktop_prefs.json").exists());
        assert!(!legacy.exists(), "rename 成功后旧目录应整体移走");
    }

    #[test]
    fn legacy_dir_kept_when_home_already_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("legacy");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("rclaw.toml"), "old").unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(config_path(&home), "new").unwrap();

        migrate_legacy_dir(&legacy, &home).unwrap();
        assert_eq!(std::fs::read_to_string(config_path(&home)).unwrap(), "new");
        assert!(legacy.exists(), "新 home 已有配置时不得动旧目录");
    }

    #[test]
    fn legacy_copy_never_overwrites_files_created_concurrently() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("legacy");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("rclaw.toml"), "old").unwrap();
        std::fs::create_dir_all(&home).unwrap();
        // 模拟检查之后、复制之前另一进程写出的新配置/新文件。
        std::fs::write(config_path(&home), "new").unwrap();

        copy_dir(&legacy, &home, std::path::Path::new("")).unwrap();
        assert_eq!(std::fs::read_to_string(config_path(&home)).unwrap(), "new");
    }

    #[test]
    fn internal_locks_are_matched_by_relative_path_only() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("legacy");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(legacy.join("agent").join("project")).unwrap();
        std::fs::create_dir_all(legacy.join("sync")).unwrap();
        std::fs::write(legacy.join("rclaw.toml.lock"), "x").unwrap();
        std::fs::write(
            legacy.join("agent").join("project").join("engine.lock"),
            "business",
        )
        .unwrap();
        std::fs::write(legacy.join("sync").join("engine.lock"), "internal").unwrap();

        copy_dir(&legacy, &home, std::path::Path::new("")).unwrap();
        assert!(!home.join("rclaw.toml.lock").exists(), "根锁文件应跳过");
        assert!(
            !home.join("sync").join("engine.lock").exists(),
            "sync 内部锁应跳过"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("agent").join("project").join("engine.lock"))
                .unwrap(),
            "business",
            "用户工作区同名业务锁必须复制"
        );
    }

    #[test]
    fn legacy_copy_fallback_when_rename_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("legacy");
        let home = tmp.path().join("home");
        // 目标已存在一个文件（remove_dir 失败 → rename 不可行）→ 退化为复制。
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("rclaw.toml"), "old").unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("blocker"), "占位").unwrap();

        migrate_legacy_dir(&legacy, &home).unwrap();
        assert!(config_path(&home).exists(), "复制兜底应把配置带过去");
        assert!(legacy.exists(), "复制兜底保留旧目录");
    }
}
