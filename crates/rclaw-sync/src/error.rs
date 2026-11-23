//! 同步层错误。

use std::time::Duration;

use thiserror::Error;

/// 传输后端的错误。引擎按种类决定：等一等、退避、报给用户还是停下。
#[derive(Debug, Clone, Error)]
pub enum TransportError {
    /// 对象不存在。
    #[error("对象不存在: {0}")]
    NotFound(String),
    /// 账号或密码不对，或没有访问权限。
    #[error("存储拒绝访问（账号、密码或权限不对）: {0}")]
    Auth(String),
    /// 服务端的上传上限太小（413）。
    #[error("服务端的上传上限太小: {0}")]
    TooLarge(String),
    /// 存储上的对象比它这一类可能的最大长度还大：没有读它（存储端不可信，不能让它拿一个
    /// 巨大的对象占满本机的内存或磁盘）。转成 [`SyncError`] 时算作「校验不过」。
    #[error("远端对象过大: {0}")]
    Oversize(String),
    /// 被限流（429 / 503）。`retry_after` 为服务端给的等待时间。
    #[error("存储限流: {message}")]
    RateLimited {
        retry_after: Option<Duration>,
        message: String,
    },
    /// 违反「公网地址必须 HTTPS」或证书指纹不符：连接被本机拒绝，账号密码没有发出去。
    #[error("连接被拒绝: {0}")]
    Insecure(String),
    /// 一段时间没有进展。
    #[error("存储没有响应: {0}")]
    Timeout(String),
    /// 服务端不支持某个操作。
    #[error("服务端不支持 {0}")]
    Unsupported(String),
    /// 配置不对（地址、目录等）。
    #[error("存储配置有误: {0}")]
    Config(String),
    #[error("存储访问失败: {0}")]
    Other(String),
}

impl TransportError {
    pub fn is_not_found(&self) -> bool {
        matches!(self, TransportError::NotFound(_))
    }
}

#[derive(Debug, Error)]
pub enum SyncError {
    #[error(transparent)]
    Store(#[from] rclaw_store::StoreError),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Transport(TransportError),
    /// 远端对象解密或校验不过：被改名、调包、截断，或者还没写完。
    #[error("远端对象校验失败: {0}")]
    Corrupt(String),
    /// 本程序的版本处理不了：格式版本更高，或不认识的 op 类型。
    #[error("需要升级 RClaw: {0}")]
    Upgrade(String),
    /// 远端内容没通过合入检查（§11.3）。
    #[error("内容不合规: {0}")]
    Invalid(String),
    /// 同步空间已被「重置同步空间」废弃：本机应停止同步，重新配对。
    #[error("同步空间已被重置，请在一台已经加入新空间的设备上重新生成配对码")]
    SpaceClosed,
    /// 需要用户先做决定才能继续（例如计费网络下的首次同步）。
    #[error("{0}")]
    NeedsUser(String),
    #[error("{0}")]
    Other(String),
}

impl From<rusqlite::Error> for SyncError {
    fn from(e: rusqlite::Error) -> Self {
        SyncError::Store(rclaw_store::StoreError::Sqlite(e))
    }
}

impl From<TransportError> for SyncError {
    fn from(e: TransportError) -> Self {
        match e {
            // 过大的对象和解密不过的对象是一回事：这个对象不能用。上层对「校验不过」已经
            // 有的处理（等一等、跳过这台设备、换一条路）对它同样适用。
            TransportError::Oversize(what) => SyncError::Corrupt(what),
            other => SyncError::Transport(other),
        }
    }
}

impl SyncError {
    pub fn other(message: impl Into<String>) -> Self {
        SyncError::Other(message.into())
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        SyncError::Invalid(message.into())
    }

    pub fn corrupt(message: impl Into<String>) -> Self {
        SyncError::Corrupt(message.into())
    }

    /// 远端对象不存在。
    pub fn is_not_found(&self) -> bool {
        matches!(self, SyncError::Transport(e) if e.is_not_found())
    }
}

pub type Result<T> = std::result::Result<T, SyncError>;
