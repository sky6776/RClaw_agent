//! RClaw 桌面版 bin 入口。
//!
//! Windows/Linux/macOS 桌面：直接进 `rclaw_desktop::run()`（Tauri 装配）。
//! Termux（android target 且未开 apk-host feature）：桌面壳不参与构建/运行，
//! 指引用 cli——APK 构建产出的是 .so（无 bin 入口），不经过本文件。

// release 构建按 GUI 子系统链接：双击运行不弹控制台框；debug 构建保留
// console 子系统，`cargo run` 时日志仍直接可见。
#![cfg_attr(all(not(debug_assertions), not(target_os = "android")), windows_subsystem = "windows")]

#[cfg(not(target_os = "android"))]
fn main() {
    rclaw_desktop::run()
}

/// Termux：`cargo build --profile release-termux` 会编译本 crate（default-members
/// 含 rclaw-desktop），保留提示入口；APK 构建开 `apk-host` 后入口是 JNI 导出。
#[cfg(all(target_os = "android", not(feature = "apk-host")))]
fn main() {
    eprintln!("rclaw-desktop 是桌面（Tauri）壳，在 Termux 下不构建；请使用 rclaw-cli。");
    std::process::exit(1);
}

/// APK 构建：真正的入口是 lib 的 JNI 导出与 mobile_entry_point；bin 仅为满足
/// default-members 的整树构建存在（不会被运行）。
#[cfg(all(target_os = "android", feature = "apk-host"))]
fn main() {
    eprintln!("rclaw-desktop APK 构建入口在 librclaw_desktop.so（JNI/mobile entry）；本 bin 不会被打包运行。");
}
