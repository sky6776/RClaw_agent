use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use async_trait::async_trait;

pub const MAX_RESULTS: usize = 50;
pub const MAX_READ_TOP: usize = 5;
pub const MAX_QUERY_BYTES: usize = 1024;
pub const MAX_OUTPUT_CHARS: usize = 48_000;

/// Stable identifiers used by configuration, routing, logs, and `/search`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackendId {
    DoubaoCustom,
    Tavily,
    Zhipu,
    DoubaoGlobal,
}

impl BackendId {
    pub const ALL: [Self; 4] = [
        Self::DoubaoCustom,
        Self::Tavily,
        Self::Zhipu,
        Self::DoubaoGlobal,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DoubaoCustom => "doubao-custom",
            Self::Tavily => "tavily",
            Self::Zhipu => "zhipu",
            Self::DoubaoGlobal => "doubao-global",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::DoubaoCustom => "豆包 Custom",
            Self::Tavily => "Tavily",
            Self::Zhipu => "智谱",
            Self::DoubaoGlobal => "豆包 Global",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "doubao-custom" => Some(Self::DoubaoCustom),
            "tavily" => Some(Self::Tavily),
            "zhipu" => Some(Self::Zhipu),
            "doubao-global" => Some(Self::DoubaoGlobal),
            _ => None,
        }
    }
}

impl fmt::Display for BackendId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BackendId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or_else(|| {
            format!(
                "未知搜索方式「{value}」，支持：{}",
                Self::ALL
                    .iter()
                    .map(|id| id.as_str())
                    .collect::<Vec<_>>()
                    .join(" / ")
            )
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendErrorKind {
    InvalidRequest,
    Authentication,
    QuotaExhausted,
    RateLimited,
    Transient,
    EmptyOrInsufficient,
    Permanent,
}

#[derive(Clone, Debug)]
pub struct BackendError {
    pub kind: BackendErrorKind,
    pub message: String,
}

impl BackendError {
    pub fn new(kind: BackendErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(BackendErrorKind::InvalidRequest, message)
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(BackendErrorKind::Authentication, message)
    }

    pub fn quota(message: impl Into<String>) -> Self {
        Self::new(BackendErrorKind::QuotaExhausted, message)
    }

    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self::new(BackendErrorKind::RateLimited, message)
    }

    pub fn transient(message: impl Into<String>) -> Self {
        Self::new(BackendErrorKind::Transient, message)
    }

    pub fn empty(message: impl Into<String>) -> Self {
        Self::new(BackendErrorKind::EmptyOrInsufficient, message)
    }

    pub fn permanent(message: impl Into<String>) -> Self {
        Self::new(BackendErrorKind::Permanent, message)
    }

    pub fn concise(&self) -> String {
        truncate_chars(&self.message.replace(['\r', '\n'], " "), 300)
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BackendError {}

#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub query: String,
    pub max_results: usize,
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub title: String,
    pub url: Option<String>,
    pub snippet: Option<String>,
    pub content: Option<String>,
    pub provider: BackendId,
    pub rank: usize,
}

#[derive(Clone, Debug)]
pub struct SearchResponse {
    pub provider: BackendId,
    pub hits: Vec<SearchHit>,
    /// Some providers (notably Zhipu MCP) return useful provider-rendered text
    /// in addition to, or instead of, structured hits.
    pub summary: Option<String>,
    pub request_id: Option<String>,
}

impl SearchResponse {
    pub fn is_usable(&self) -> bool {
        !self.hits.is_empty()
            || self
                .summary
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
    }
}

#[derive(Clone, Debug)]
pub struct ReadResponse {
    pub provider: BackendId,
    pub content: String,
}

#[async_trait]
pub trait SearchBackend: Send + Sync {
    fn id(&self) -> BackendId;

    async fn search(&self, request: &SearchRequest) -> Result<SearchResponse, BackendError>;
}

#[async_trait]
pub trait ReaderBackend: Send + Sync {
    fn id(&self) -> BackendId;

    async fn read(&self, url: &str) -> Result<ReadResponse, BackendError>;
}

pub fn normalize_api_key(provider: &str, value: String) -> Result<String, String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(format!("{provider} api_key 不能为空"));
    }
    if value.contains(['\r', '\n']) {
        return Err(format!("{provider} api_key 不能包含换行"));
    }
    Ok(value)
}

pub fn normalize_base_url(provider: &str, value: String, default: &str) -> Result<String, String> {
    let value = if value.trim().is_empty() {
        default.to_string()
    } else {
        value.trim().trim_end_matches('/').to_string()
    };
    let parsed =
        reqwest::Url::parse(&value).map_err(|error| format!("{provider} URL 非法: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        return Err(format!("{provider} URL 必须是 HTTP(S) 绝对地址"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!("{provider} URL 不允许包含用户名或密码"));
    }
    Ok(value)
}

pub fn validate_public_http_url(value: &str) -> Result<String, String> {
    let mut parsed =
        reqwest::Url::parse(value.trim()).map_err(|error| format!("URL 非法: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("URL 只支持 http/https".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("URL 不允许包含用户名或密码".into());
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL 缺少 host".to_string())?;
    if host.eq_ignore_ascii_case("localhost") {
        return Err("URL 不允许访问 localhost".into());
    }
    // `url` 对 IPv6 的 host 字符串可能保留方括号，解析 IP 前显式去除。
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        if !is_public_ip(ip) {
            return Err("URL 不允许访问私有、环回或保留 IP".into());
        }
    }
    parsed.set_fragment(None);
    Ok(parsed.to_string())
}

pub fn canonical_url_key(value: &str) -> String {
    reqwest::Url::parse(value)
        .map(|mut parsed| {
            parsed.set_fragment(None);
            parsed.to_string()
        })
        .unwrap_or_else(|_| value.to_string())
}

/// 生成适合日志的端点标签，去掉凭据、查询参数和 fragment。
pub(super) fn safe_endpoint_label(value: &str) -> String {
    reqwest::Url::parse(value)
        .map(|mut parsed| {
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.set_query(None);
            parsed.set_fragment(None);
            parsed.to_string()
        })
        .unwrap_or_else(|_| "<invalid-url>".to_string())
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, d] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224
        || (a == 255 && b == 255 && c == 255 && d == 255))
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(ipv4) = ip.to_ipv4() {
        return is_public_ipv4(ipv4);
    }
    let segments = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] == 0x0100 && segments[1..].iter().all(|segment| *segment == 0))
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

pub fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut output = value.chars().take(max).collect::<String>();
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_url_validation_rejects_special_use_addresses() {
        for url in [
            "http://100.64.0.1/",
            "http://198.18.0.1/",
            "http://240.0.0.1/",
            "http://[::ffff:127.0.0.1]/",
            "http://[fec0::1]/",
        ] {
            assert!(validate_public_http_url(url).is_err(), "应拒绝 {url}");
        }
        assert!(validate_public_http_url("https://8.8.8.8/").is_ok());
    }

    #[test]
    fn provider_base_url_rejects_embedded_credentials() {
        assert!(normalize_base_url(
            "test",
            "https://user:password@example.com/api".into(),
            "https://example.com"
        )
        .is_err());
    }
}
