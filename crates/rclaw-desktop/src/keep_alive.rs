//! 后台保活引导命令（GUI「关于与设置 → 后台保活」卡片）。
//!
//! Android：经 `rclaw_android::keep_alive` → JNI → `KeepAliveGuide`，查电池优化
//! 白名单状态、拉起系统白名单确认弹窗、跳转厂商自启动/后台管理页。桌面：
//! `supported = false`，卡片不渲染，动作命令直接报错。

use serde::Serialize;

/// `keep_alive_status` 返回值。
#[derive(Debug, Serialize)]
pub struct KeepAliveStatusDto {
    /// 是否是 Android 宿主（桌面恒 false，其余字段取占位值）。
    pub supported: bool,
    pub battery_whitelisted: bool,
    pub manufacturer: String,
    pub oem_settings: bool,
}

#[tauri::command]
pub fn keep_alive_status() -> Result<KeepAliveStatusDto, String> {
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        match rclaw_android::keep_alive::status().map_err(|e| e.to_string())? {
            Some(status) => Ok(KeepAliveStatusDto {
                supported: true,
                battery_whitelisted: status.battery_whitelisted,
                manufacturer: status.manufacturer,
                oem_settings: status.oem_settings,
            }),
            None => Ok(unsupported()),
        }
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {
        Ok(unsupported())
    }
}

fn unsupported() -> KeepAliveStatusDto {
    KeepAliveStatusDto {
        supported: false,
        battery_whitelisted: true,
        manufacturer: String::new(),
        oem_settings: false,
    }
}

/// 拉起系统「忽略电池优化」确认弹窗（用户在系统弹窗确认；回到应用后前端重查状态）。
#[tauri::command]
pub fn keep_alive_request_battery_whitelist() -> Result<(), String> {
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        rclaw_android::keep_alive::request_battery_whitelist().map_err(|e| e.to_string())
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {
        Err("后台保活引导仅安卓 APK 内可用".into())
    }
}

/// 跳转厂商自启动/后台管理页（不可达时回退应用详情页）。
#[tauri::command]
pub fn keep_alive_open_oem_settings() -> Result<(), String> {
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        rclaw_android::keep_alive::open_oem_settings().map_err(|e| e.to_string())
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {
        Err("后台保活引导仅安卓 APK 内可用".into())
    }
}
