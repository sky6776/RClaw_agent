//! `[sync]` 配置段（§12.3）与本机的同步目录布局。
//!
//! 密码、空间密钥、`device_id` 不在配置里，存于 `<数据根>/sync/secrets.json`
//! （见 [`crate::secrets`]）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, SyncError};
use crate::ids;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SyncConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 设备名；留空用主机名。
    #[serde(default)]
    pub device_name: String,
    /// `webdav` | `file` | `s3`。
    #[serde(default = "default_transport")]
    pub transport: String,
    /// 同步范围：`sessions`、`memory`、`persona`，可追加 `attachments`、`skills`、
    /// `bots`、`cron`。
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// 参与同步的会话通道。
    #[serde(default = "default_channels")]
    pub channels: Vec<String>,
    /// 前台轮询间隔（秒）。
    #[serde(default = "default_poll_interval")]
    pub poll_interval_sec: u64,
    /// 后台轮询间隔（秒）。
    #[serde(default = "default_background_interval")]
    pub background_interval_sec: u64,
    /// 发送前拉取最多等多久（毫秒），0 = 不拉取。
    #[serde(default = "default_presend_wait")]
    pub presend_pull_wait_ms: u64,
    /// 被消息引用的工作区文件大小上限（MB）。附件的上传下载不区分网络类型。
    #[serde(default = "default_attachment_max")]
    pub attachment_max_mb: u64,
    #[serde(default = "default_trash_days")]
    pub trash_days: u64,
    #[serde(default)]
    pub webdav: WebdavSection,
    #[serde(default)]
    pub file: FileSection,
    #[serde(default)]
    pub s3: S3Section,
}

/// `[sync.webdav]`：第三方网盘和自建文件服务器写法相同。
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebdavSection {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub username: String,
    /// 明文 HTTP：只对局域网地址有效。公网地址必须 HTTPS。
    #[serde(default)]
    pub allow_http: bool,
    /// 自签证书的指纹；留空信任系统与内置公共 CA。
    #[serde(default)]
    pub pinned_cert_sha256: String,
}

/// `[sync.file]`：一个本机目录，由别的工具负责同步。
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileSection {
    #[serde(default)]
    pub dir: String,
}

/// `[sync.s3]`：S3 兼容的对象存储。
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct S3Section {
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub access_key_id: String,
    #[serde(default)]
    pub path_style: bool,
    /// 阿里云 OSS：`PutObject` 拒绝条件头（`If-None-Match` 返回 400），改用 OSS 专有的
    /// `x-oss-forbid-overwrite: true` 做「仅创建」（已存在时 409 `FileAlreadyExists`）。
    /// 这个头在桶开启或暂停过版本控制后失效；OSS 也只支持虚拟主机式寻址。
    ///
    /// 默认 `false` 时不写进 TOML / 恢复记录：`S3Section` 是 `deny_unknown_fields`，
    /// 老版本读到不认识的键会整份拒绝（恢复记录还会被当成损坏删掉）。
    #[serde(default, skip_serializing_if = "is_false")]
    pub oss_forbid_overwrite: bool,
    /// 明文 HTTP：只对局域网地址有效。公网地址必须 HTTPS。
    #[serde(default)]
    pub allow_http: bool,
}

/// serde 用：`false` 不落盘（见 `S3Section::oss_forbid_overwrite`）。
pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

fn default_transport() -> String {
    "webdav".into()
}
fn default_scopes() -> Vec<String> {
    vec!["sessions".into(), "memory".into(), "persona".into()]
}
fn default_channels() -> Vec<String> {
    vec!["desktop".into()]
}
fn default_poll_interval() -> u64 {
    60
}
fn default_background_interval() -> u64 {
    900
}
fn default_presend_wait() -> u64 {
    1500
}
fn default_attachment_max() -> u64 {
    50
}
fn default_trash_days() -> u64 {
    30
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            enabled: false,
            device_name: String::new(),
            transport: default_transport(),
            scopes: default_scopes(),
            channels: default_channels(),
            poll_interval_sec: default_poll_interval(),
            background_interval_sec: default_background_interval(),
            presend_pull_wait_ms: default_presend_wait(),
            attachment_max_mb: default_attachment_max(),
            trash_days: default_trash_days(),
            webdav: WebdavSection::default(),
            file: FileSection::default(),
            s3: S3Section::default(),
        }
    }
}

/// 同步范围。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scopes {
    pub sessions: bool,
    pub memory: bool,
    pub persona: bool,
    pub attachments: bool,
    pub skills: bool,
    pub bots: bool,
    pub cron: bool,
}

pub const SCOPE_NAMES: &[&str] = &[
    "sessions",
    "memory",
    "persona",
    "attachments",
    "skills",
    "bots",
    "cron",
];

impl Scopes {
    pub fn parse(names: &[String]) -> Result<Scopes> {
        let mut scopes = Scopes::default();
        for name in names {
            match name.as_str() {
                "sessions" => scopes.sessions = true,
                "memory" => scopes.memory = true,
                "persona" => scopes.persona = true,
                "attachments" => scopes.attachments = true,
                "skills" => scopes.skills = true,
                "bots" => scopes.bots = true,
                "cron" => scopes.cron = true,
                other => {
                    return Err(SyncError::other(format!(
                        "[sync] scopes 里有不认识的项 {other:?}（可用：{}）",
                        SCOPE_NAMES.join("、")
                    )))
                }
            }
        }
        // 附件跟着会话走：没有会话就没有引用它们的消息。
        scopes.attachments &= scopes.sessions;
        Ok(scopes)
    }

    /// 宿主不支持会话同步时（CLI / Termux：没有会话列表，下载的会话没处打开）去掉
    /// 会话与附件。
    pub fn without_sessions(mut self) -> Scopes {
        self.sessions = false;
        self.attachments = false;
        self
    }

    pub fn any(&self) -> bool {
        self.sessions
            || self.memory
            || self.persona
            || self.attachments
            || self.skills
            || self.bots
            || self.cron
    }
}

impl SyncConfig {
    /// 把另一份配置里的存储设置搬过来（范围、通道、设备名等不动）。存储更新意图只应该
    /// 影响存储目标与凭据绑定：这期间别处改过的范围不能被一份旧意图回退。
    pub fn apply_storage_from(&mut self, other: &SyncConfig) {
        self.transport = other.transport.clone();
        self.webdav = other.webdav.clone();
        self.file = other.file.clone();
        self.s3 = other.s3.clone();
    }

    /// 结构校验：拼错的传输名、范围名、通道名在启动期就报出来。
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.transport.as_str(), "webdav" | "file" | "s3") {
            return Err(SyncError::other(format!(
                "[sync] transport 只能是 webdav / file / s3，当前为 {:?}",
                self.transport
            )));
        }
        Scopes::parse(&self.scopes)?;
        if let Some(bad) = self.channels.iter().find(|c| !ids::is_valid_channel(c)) {
            return Err(SyncError::other(format!(
                "[sync] channels 里的通道名不合法: {bad:?}"
            )));
        }
        if self.poll_interval_sec < 5 {
            return Err(SyncError::other("[sync] poll_interval_sec 不能小于 5"));
        }
        if self.background_interval_sec < 60 {
            return Err(SyncError::other(
                "[sync] background_interval_sec 不能小于 60",
            ));
        }
        if self.transport == "s3" && self.s3.oss_forbid_overwrite && self.s3.path_style {
            return Err(SyncError::other(
                "[sync.s3] 阿里云 OSS 只支持虚拟主机式：oss_forbid_overwrite 与 path_style 不能同时开启",
            ));
        }
        // 存储密码只放在 `<数据根>/sync/secrets.json`，不进 rclaw.toml（§11.2）。地址里夹带
        // 的 `用户名:密码@` 在这里就拦下：设置界面存不进去，手改的文件在启动时报出来。
        // 地址的其余毛病（协议、主机名）留给连接时报，不拦启动。
        for (key, raw) in [
            ("[sync.webdav] url", &self.webdav.url),
            ("[sync.s3] endpoint", &self.s3.endpoint),
        ] {
            let embedded = url::Url::parse(raw.trim())
                .is_ok_and(|url| !url.username().is_empty() || url.password().is_some());
            if embedded {
                return Err(SyncError::other(format!(
                    "{key} 里不要带用户名和密码：用户名单独填写，密码在同步设置里输入（它不保存在 rclaw.toml 里）"
                )));
            }
        }
        Ok(())
    }

    pub fn scope_set(&self) -> Scopes {
        Scopes::parse(&self.scopes).unwrap_or_default()
    }

    /// 展示用的设备名：配置里的名字，留空时取主机名。
    pub fn effective_device_name(&self) -> String {
        let name = self.device_name.trim();
        if !name.is_empty() {
            return name.chars().take(40).collect();
        }
        std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .ok()
            .map(|host| host.trim().to_string())
            .filter(|host| !host.is_empty())
            .unwrap_or_else(|| "未命名设备".into())
    }

    pub fn attachment_max_bytes(&self) -> u64 {
        self.attachment_max_mb.saturating_mul(1024 * 1024)
    }
}

/// 本机的同步目录：`<数据根>/sync/`。文件工具把整个目录当禁区。
#[derive(Debug, Clone)]
pub struct SyncPaths {
    pub root: PathBuf,
    /// 空间密钥、设备身份、存储凭据。
    pub secrets: PathBuf,
    /// 已封段的密文：上传前的落盘点，也是换身份后重发的来源。
    pub outgoing: PathBuf,
    /// 回收站：被别的设备删除的会话，明文会话包，保留若干天。
    pub trash: PathBuf,
    /// 等本机确认的远端改动（人格、带脚本的技能、放宽权限的 bot）。
    pub pending: PathBuf,
    /// 只进缓存的附件（`media/`、`output/`，以及与本机同名文件内容不同的）。
    pub files: PathBuf,
    /// 首次加入时被空间版本顶掉的本机同名技能 / bot。
    pub conflicts: PathBuf,
    /// 下载中的临时文件。
    pub tmp: PathBuf,
    /// 引擎单实例锁。
    pub lock: PathBuf,
}

impl SyncPaths {
    /// `data_root` 是 `rclaw.toml` 所在目录。
    pub fn new(data_root: &Path) -> SyncPaths {
        let root = data_root.join("sync");
        SyncPaths {
            secrets: root.join("secrets.json"),
            outgoing: root.join("outgoing"),
            trash: root.join("trash"),
            pending: root.join("pending"),
            files: root.join("files"),
            conflicts: root.join("conflicts"),
            tmp: root.join("tmp"),
            lock: root.join("engine.lock"),
            root,
        }
    }

    /// 建 `sync/` 本身，并收紧到只有本用户进得去（Unix 0700）：里面是密钥、凭据、回收站
    /// 里的明文会话包。目录进不去，里面的文件各自是什么权限都读不到。
    pub fn ensure_root(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&self.root)?.permissions().mode();
            if mode & 0o077 != 0 {
                std::fs::set_permissions(
                    &self.root,
                    std::fs::Permissions::from_mode(mode & 0o7700),
                )?;
            }
        }
        Ok(())
    }

    pub fn ensure(&self) -> Result<()> {
        self.ensure_root()?;
        for dir in [
            &self.outgoing,
            &self.trash,
            &self.pending,
            &self.files,
            &self.conflicts,
            &self.tmp,
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_section() {
        let config = SyncConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.transport, "webdav");
        assert_eq!(config.scopes, ["sessions", "memory", "persona"]);
        assert_eq!(config.channels, ["desktop"]);
        assert_eq!(config.poll_interval_sec, 60);
        assert_eq!(config.background_interval_sec, 900);
        assert_eq!(config.presend_pull_wait_ms, 1500);
        assert_eq!(config.attachment_max_mb, 50);
        assert_eq!(config.trash_days, 30);
        assert!(!config.webdav.allow_http);
        config.validate().unwrap();
    }

    #[test]
    fn parses_from_toml_and_rejects_unknown_keys() {
        let config: SyncConfig = toml::from_str(
            r#"
enabled = true
device_name = "我的电脑"
scopes = ["sessions", "memory", "attachments"]

[webdav]
url = "https://dav.example.com/rclaw-sync/"
username = "me@example.com"
"#,
        )
        .unwrap();
        assert!(config.enabled);
        assert_eq!(config.webdav.username, "me@example.com");
        let scopes = config.scope_set();
        assert!(scopes.sessions && scopes.memory && scopes.attachments && !scopes.persona);
        assert!(toml::from_str::<SyncConfig>("enabeld = true").is_err());
        assert!(toml::from_str::<SyncConfig>("[webdav]\npassword = \"x\"").is_err());
    }

    #[test]
    fn s3_parses_the_oss_switch() {
        let config: SyncConfig = toml::from_str(
            r#"
transport = "s3"

[s3]
endpoint = "https://s3.oss-cn-hangzhou.aliyuncs.com"
bucket = "rclaw-test"
oss_forbid_overwrite = true
"#,
        )
        .unwrap();
        assert!(config.s3.oss_forbid_overwrite);
        // 老配置没有这个键：默认关。
        let old: SyncConfig = toml::from_str(
            "[s3]
endpoint = \"https://s3.example.com\"
",
        )
        .unwrap();
        assert!(!old.s3.oss_forbid_overwrite);
    }

    #[test]
    fn oss_mode_cannot_be_combined_with_path_style() {
        let mut config = SyncConfig {
            transport: "s3".into(),
            ..Default::default()
        };
        config.s3.endpoint = "https://s3.oss-cn-hangzhou.aliyuncs.com".into();
        config.s3.bucket = "rclaw-test".into();
        config.s3.oss_forbid_overwrite = true;
        config.s3.path_style = true;
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("虚拟主机式"), "{error}");
        config.s3.path_style = false;
        config.validate().unwrap();
    }

    #[test]
    fn validation_catches_typos() {
        let config = SyncConfig {
            transport: "ftp".into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
        let config = SyncConfig {
            scopes: vec!["everything".into()],
            ..Default::default()
        };
        assert!(config.validate().is_err());
        let config = SyncConfig {
            channels: vec!["Desktop".into()],
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn credentials_in_the_storage_address_are_refused() {
        let mut config = SyncConfig::default();
        config.webdav.url = "https://me:hunter2@dav.example.com/rclaw/".into();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("[sync.webdav] url"), "{error}");
        assert!(!error.contains("hunter2"), "报错里不能带出密码: {error}");
        config.webdav.url = "https://me@dav.example.com/rclaw/".into();
        assert!(config.validate().is_err());
        config.webdav.url = "https://dav.example.com/rclaw/".into();
        config.validate().unwrap();
        config.s3.endpoint = "https://key:secret@oss.example.com".into();
        assert!(config.validate().is_err());
        // 写不成地址的字符串不在这里拦（连接时再报）。
        config.s3.endpoint = "not a url".into();
        config.validate().unwrap();
    }

    #[test]
    fn attachments_need_sessions() {
        let scopes = Scopes::parse(&["attachments".into(), "memory".into()]).unwrap();
        assert!(!scopes.attachments && scopes.memory);
        let scopes = Scopes::parse(&["sessions".into(), "attachments".into()]).unwrap();
        assert!(scopes.without_sessions() == Scopes::default());
    }
}
