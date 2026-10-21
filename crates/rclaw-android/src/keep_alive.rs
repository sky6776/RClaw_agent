//! 保活引导桥：电池优化白名单状态/申请、厂商自启动设置页跳转。
//!
//! 前台服务 + 看门狗挡不住 Doze（非白名单应用的 wake lock 被忽略、网络被切）
//! 和国产 ROM 的自启动管理，两者都只能由用户点一次系统设置。这里把 Java 侧
//! `KeepAliveGuide`（经 `NativeBridge.keepAlive*`）暴露给桌面 command 层，GUI
//! 「关于与设置 → 后台保活」卡片据此显示状态并提供跳转按钮。桌面平台返回
//! `supported = false`，命令层直接透传。

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// 保活状态快照（Java `KeepAliveGuide.statusJson` 的反序列化）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeepAliveStatus {
    /// 是否已加入电池优化白名单（`PowerManager.isIgnoringBatteryOptimizations`）。
    pub battery_whitelisted: bool,
    /// `Build.MANUFACTURER` 原值（GUI 文案用）。
    pub manufacturer: String,
    /// 本机是否有可达的厂商自启动/后台管理页（否则按钮退化为「应用详情」）。
    pub oem_settings: bool,
}

/// 查询保活状态；非 Android 宿主返回 `None`。
pub fn status() -> Result<Option<KeepAliveStatus>> {
    #[cfg(target_os = "android")]
    {
        let json = crate::jni_bridge::keep_alive_status()?;
        let status: KeepAliveStatus = serde_json::from_str(&json)
            .map_err(|e| anyhow::anyhow!("解析保活状态 JSON 失败: {e}; raw={json}"))?;
        Ok(Some(status))
    }
    #[cfg(not(target_os = "android"))]
    {
        Ok(None)
    }
}

/// 拉起系统「忽略电池优化」确认弹窗（个别 ROM 退到电池优化列表页）。
pub fn request_battery_whitelist() -> Result<()> {
    #[cfg(target_os = "android")]
    {
        if !crate::jni_bridge::keep_alive_request_battery_whitelist()? {
            return Err(anyhow::anyhow!(
                "系统未受理电池白名单请求（该 ROM 可能禁用了此入口，请到系统设置 → 电池手动添加）"
            ));
        }
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(anyhow::anyhow!("保活引导仅 Android 宿主支持"))
    }
}

/// 跳转厂商自启动/后台管理页（不可达时 Java 侧回退应用详情页）。
pub fn open_oem_settings() -> Result<()> {
    #[cfg(target_os = "android")]
    {
        if !crate::jni_bridge::keep_alive_open_oem_settings()? {
            return Err(anyhow::anyhow!("无法打开系统设置页"));
        }
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(anyhow::anyhow!("保活引导仅 Android 宿主支持"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_json_round_trips_java_shape() {
        let raw = r#"{"battery_whitelisted":false,"manufacturer":"vivo","oem_settings":true}"#;
        let parsed: KeepAliveStatus = serde_json::from_str(raw).unwrap();
        assert_eq!(
            parsed,
            KeepAliveStatus {
                battery_whitelisted: false,
                manufacturer: "vivo".into(),
                oem_settings: true,
            }
        );
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn desktop_host_reports_unsupported() {
        assert_eq!(status().unwrap(), None);
        assert!(request_battery_whitelist().is_err());
        assert!(open_oem_settings().is_err());
    }
}
