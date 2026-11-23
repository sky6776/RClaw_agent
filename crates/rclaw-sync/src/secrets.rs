//! `<数据根>/sync/secrets.json`：空间密钥、设备身份、存储凭据（§11.1）。
//!
//! 这些东西只存在本机，不进 `rclaw.toml`、不进库、不上传。Android 上它在应用私有目录里；
//! 桌面端用文件权限保护（Unix 0600）。整个 `sync/` 目录是文件工具的禁区。

use serde::{Deserialize, Serialize};

use crate::config::{SyncConfig, SyncPaths};
use crate::crypto::{Keys, SpaceKey};
use crate::error::{Result, SyncError};
use crate::ids;

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Secrets {
    /// 所在的同步空间；空 = 还没创建或加入。
    #[serde(default)]
    pub space_id: String,
    /// 空间密钥（base64url）。
    #[serde(default)]
    pub key: String,
    /// 本机的设备身份。
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub webdav_password: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) webdav_account: String,
    #[serde(default)]
    pub s3_secret_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) s3_account: String,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets")
            .field("space_id", &self.space_id)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// `space_id`：20 位十六进制。它也是空间在存储根下的子目录名。
pub fn new_space_id() -> String {
    ids::random_hex::<10>()
}

pub fn is_space_id(text: &str) -> bool {
    text.len() == 20
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Secrets {
    pub(crate) fn storage_account(config: &SyncConfig) -> Option<String> {
        let origin = |value: &str| {
            url::Url::parse(value.trim())
                .ok()
                .map(|url| url.origin().ascii_serialization())
        };
        match config.transport.as_str() {
            "webdav" => Some(format!(
                "webdav\n{}\n{}",
                origin(&config.webdav.url)?,
                config.webdav.username
            )),
            "s3" => Some(format!(
                "s3\n{}\n{}\n{}\n{}",
                origin(&config.s3.endpoint)?,
                config.s3.bucket.trim(),
                config.s3.access_key_id.trim(),
                config.s3.path_style
            )),
            _ => None,
        }
    }

    pub(crate) fn storage_secret(&self, config: &SyncConfig) -> &str {
        let (account, secret) = match config.transport.as_str() {
            "webdav" => (&self.webdav_account, &self.webdav_password),
            "s3" => (&self.s3_account, &self.s3_secret_key),
            _ => return "",
        };
        if Self::storage_account(config).as_deref() == Some(account.as_str()) {
            secret
        } else {
            ""
        }
    }

    pub(crate) fn set_storage_secret(&mut self, config: &SyncConfig, secret: String) -> Result<()> {
        let account =
            Self::storage_account(config).ok_or_else(|| SyncError::invalid("请先填写存储地址"))?;
        match config.transport.as_str() {
            "webdav" => {
                self.webdav_account = account;
                self.webdav_password = secret;
            }
            "s3" => {
                self.s3_account = account;
                self.s3_secret_key = secret;
            }
            _ => return Err(SyncError::invalid("共享文件夹不需要密码")),
        }
        Ok(())
    }

    /// 读取；文件不存在返回默认值（未配对）。
    pub fn load(paths: &SyncPaths) -> Result<Secrets> {
        match std::fs::read(&paths.secrets) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| SyncError::other(format!("sync/secrets.json 损坏: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Secrets::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, paths: &SyncPaths) -> Result<()> {
        paths.ensure_root()?;
        rclaw_store::atomic_write_private(&paths.secrets, &serde_json::to_vec_pretty(self)?)?;
        remove_stale_copies(paths);
        Ok(())
    }

    /// 已创建或加入了同步空间。
    pub fn is_paired(&self) -> bool {
        is_space_id(&self.space_id) && !self.key.is_empty()
    }

    pub fn space_key(&self) -> Result<SpaceKey> {
        if !self.is_paired() {
            return Err(SyncError::other("还没有创建或加入同步空间"));
        }
        SpaceKey::from_base64(&self.key)
    }

    pub fn keys(&self) -> Result<Keys> {
        Ok(Keys::derive(&self.space_key()?))
    }

    /// 离开同步空间：密钥与空间身份清掉，存储凭据也一并清掉。设备身份保留——本机已有
    /// 消息的 `gid` 前缀是它。
    pub fn leave_space(&mut self) {
        self.space_id.clear();
        self.key.clear();
        self.webdav_password.clear();
        self.webdav_account.clear();
        self.s3_secret_key.clear();
        self.s3_account.clear();
    }
}

/// 一次没写完的保存（进程在写临时文件和改名之间退出）留下的临时文件是密钥的另一份
/// 拷贝：离开空间、换密钥之后它还带着旧的。每次保存成功后顺手清掉。
///
/// 只清放了一阵子的：同一个数据根上的另一个进程可能正写到一半，它的临时文件不能动。
fn remove_stale_copies(paths: &SyncPaths) {
    const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(60);
    let Some(name) = paths.secrets.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let prefix = format!(".{name}.");
    let Ok(entries) = std::fs::read_dir(&paths.root) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !(file_name.starts_with(&prefix) && file_name.ends_with(".tmp")) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= STALE_AFTER);
        if stale {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                tracing::warn!("清理 sync/ 下没写完的密钥临时文件失败: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_defaults_to_unpaired() {
        let dir = tempfile::tempdir().unwrap();
        let paths = SyncPaths::new(dir.path());
        let empty = Secrets::load(&paths).unwrap();
        assert!(!empty.is_paired());
        assert!(empty.space_key().is_err());

        let secrets = Secrets {
            space_id: new_space_id(),
            key: SpaceKey::generate().to_base64(),
            device_id: ids::new_device_id(),
            webdav_password: "hunter2".into(),
            ..Secrets::default()
        };
        secrets.save(&paths).unwrap();
        let loaded = Secrets::load(&paths).unwrap();
        assert_eq!(loaded, secrets);
        assert!(loaded.is_paired());
        loaded.keys().unwrap();
        // Debug 输出不带密钥与密码。
        let shown = format!("{loaded:?}");
        assert!(!shown.contains("hunter2") && !shown.contains(&secrets.key));
    }

    /// 密钥文件和它所在的目录只有本用户能读；上次没写完留下的临时文件在下次保存时清掉，
    /// 别的进程正在写的那个不动。
    #[test]
    fn saving_keeps_secrets_private_and_clears_stale_copies() {
        let dir = tempfile::tempdir().unwrap();
        let paths = SyncPaths::new(dir.path());
        std::fs::create_dir_all(&paths.root).unwrap();
        let stale = paths.root.join(".secrets.json.0123456789abcdef.tmp");
        let fresh = paths.root.join(".secrets.json.fedcba9876543210.tmp");
        let other = paths.root.join(".engine.lock.0123456789abcdef.tmp");
        for path in [&stale, &fresh, &other] {
            std::fs::write(path, b"{}").unwrap();
        }
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for path in [&stale, &other] {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            file.set_modified(old).unwrap();
        }

        let secrets = Secrets {
            space_id: new_space_id(),
            key: SpaceKey::generate().to_base64(),
            device_id: ids::new_device_id(),
            ..Secrets::default()
        };
        secrets.save(&paths).unwrap();
        assert!(!stale.exists(), "放了很久的临时文件要清掉");
        assert!(fresh.exists(), "刚写的可能属于另一个进程");
        assert!(other.exists(), "别的文件的临时文件不归这里管");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |path: &std::path::Path| {
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777
            };
            assert_eq!(mode(&paths.secrets), 0o600);
            assert_eq!(mode(&paths.root) & 0o077, 0);
        }
    }

    #[test]
    fn leaving_keeps_device_identity() {
        let mut secrets = Secrets {
            space_id: new_space_id(),
            key: SpaceKey::generate().to_base64(),
            device_id: "0123456789abcdef".into(),
            webdav_password: "p".into(),
            s3_secret_key: "s".into(),
            ..Secrets::default()
        };
        secrets.leave_space();
        assert!(!secrets.is_paired());
        assert_eq!(secrets.device_id, "0123456789abcdef");
        assert!(secrets.webdav_password.is_empty() && secrets.s3_secret_key.is_empty());
    }

    #[test]
    fn corrupt_file_is_an_error_not_a_reset() {
        let dir = tempfile::tempdir().unwrap();
        let paths = SyncPaths::new(dir.path());
        std::fs::create_dir_all(&paths.root).unwrap();
        std::fs::write(&paths.secrets, b"{ not json").unwrap();
        assert!(Secrets::load(&paths).is_err());
    }
}
