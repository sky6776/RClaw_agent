fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    capture_build_info();

    // 目标平台必须读 CARGO_CFG_TARGET_OS：build script 里的 cfg!/#[cfg] 评估的是
    // 构建宿主（Windows 上交叉编译 aarch64-linux-android 时两者相反）。
    let target_is_android = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android");
    let apk_host = std::env::var_os("CARGO_FEATURE_APK_HOST").is_some();
    if target_is_android && !apk_host {
        // Termux：目标为 android 且未开 apk-host，不跑 tauri-build。
        return;
    }
    if target_is_android && apk_host {
        // tauri CLI 在 android 构建时设置该路径：tauri-build 据此把 tauri-android
        // 与各插件的 Android 工程挂进 gen/android（缺了会 ClassNotFound 插件类）。
        let project_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("gen/android");
        std::env::set_var("TAURI_ANDROID_PROJECT_PATH", &project_path);
    }
    tauri_build::build();
}

/// 捕获「关于」页版本信息（git 提交 / rustc / 构建时间 / 应用版本）。
/// 应用版本以 gen/android/app/tauri.properties 的 tauri.androidName 为准（安卓
/// versionName 的同一来源，改一处即可），缺失时回退 crate 版本。
fn capture_build_info() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // git 短提交：HEAD 变化（新提交/切分支）触发本 crate 重编译刷新。
    println!(
        "cargo:rerun-if-changed={}/../../.git/HEAD",
        manifest.display()
    );
    let git = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(&manifest)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=RCLAW_GIT={git}");

    let rustc = std::process::Command::new(std::env::var("RUSTC").as_deref().unwrap_or("rustc"))
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=RCLAW_RUSTC={rustc}");

    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=RCLAW_BUILD_TIME={secs}");

    // 应用版本：tauri.properties 里 tauri.androidName（key=value 行解析）。
    // 同时接受 tauri.android.versionName——tauri CLI 驱动构建时会重新生成该
    // 文件且只写后一种拼写，只认一种会在 CLI 构建后悄悄回退到 crate 版本。
    let props = manifest.join("gen/android/app/tauri.properties");
    println!("cargo:rerun-if-changed={}", props.display());
    let text = std::fs::read_to_string(&props).unwrap_or_default();
    let read_prop = |key: &str| -> Option<String> {
        text.lines().find_map(|line| {
            line.strip_prefix(key)
                .map(|v| v.strip_prefix('=').unwrap_or_default().trim().to_string())
        })
    };
    let app_version = read_prop("tauri.androidName")
        .filter(|s| !s.is_empty())
        .or_else(|| read_prop("tauri.android.versionName"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=RCLAW_APP_VERSION={app_version}");
}
