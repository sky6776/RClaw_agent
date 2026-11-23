//! 网络后端（`webdav`、`s3`）共用的连接策略（§10.2）。
//!
//! - **公网地址必须用 HTTPS**。明文 HTTP 只允许局域网地址，并且要在配置里显式允许。
//!   判定看实际连接的 IP 而不是主机名：主机名在每次连接前解析，解析结果不在私有地址
//!   范围内就拒绝，凭据不会发出去。
//! - 凭据只发给配置的那个地址：带凭据的请求只跟留在同一个源上的重定向。下载被转到别的
//!   主机时，用不带凭据的请求去取，并且只走 HTTPS；跳到明文 HTTP 的重定向一律拒绝。
//! - 自签证书靠固定证书指纹信任，不提供「忽略证书错误」。
//! - 大对象不设总时长上限，只看进度：进度停滞超过 [`IDLE_TIMEOUT`] 才算超时。

use std::error::Error as StdError;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::StreamExt;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::io::AsyncReadExt;

use super::{oversize, TransportResult};
use crate::error::TransportError;
use crate::ids;

/// 进度停滞多久算超时。
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const UPLOAD_CHUNK: usize = 64 * 1024;
/// 请求体流完之后等回应的时限：每这么多字节多等一秒，最多等 [`SETTLE_MAX`]。
const SETTLE_BYTES_PER_SEC: u64 = 16 * 1024;
const SETTLE_MAX: Duration = Duration::from_secs(15 * 60);

/// 是否局域网（私有）地址：10/8、172.16/12、192.168/16、fc00::/7、链路本地、回环。
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ip(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

/// 校验过的服务端地址。
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// 以 `/` 结尾的基地址。
    pub url: url::Url,
    /// 明文 HTTP（只可能是局域网地址）。
    pub plaintext: bool,
}

const HTTPS_REQUIRED: &str =
    "公网地址必须使用 HTTPS。明文 HTTP 只能用于局域网地址，并且要在配置里显式允许（allow_http = true）";

/// 校验服务端地址并套用 HTTPS 规则。
pub fn check_endpoint(raw: &str, allow_http: bool) -> TransportResult<Endpoint> {
    let mut url = url::Url::parse(raw.trim())
        .map_err(|e| TransportError::Config(format!("存储地址不合法: {e}")))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(TransportError::Config(
            "存储地址里不要带用户名和密码，请分别填写".into(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(TransportError::Config(
            "存储地址里不能带 ? 或 # 部分".into(),
        ));
    }
    let host = url
        .host()
        .ok_or_else(|| TransportError::Config("存储地址缺少主机名".into()))?;
    let plaintext = match url.scheme() {
        "https" => false,
        "http" => {
            if !allow_http {
                return Err(TransportError::Insecure(HTTPS_REQUIRED.into()));
            }
            let literal = match host {
                url::Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
                url::Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
                url::Host::Domain(_) => None,
            };
            // 主机名留到每次连接前解析后再判（见 `LanOnlyResolver`）。
            if let Some(ip) = literal {
                if !is_private_ip(ip) {
                    return Err(TransportError::Insecure(format!(
                        "{ip} 不是局域网地址。{HTTPS_REQUIRED}"
                    )));
                }
            }
            true
        }
        other => {
            return Err(TransportError::Config(format!(
                "存储地址只支持 https://（局域网可用 http://），不支持 {other}://"
            )))
        }
    };
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(Endpoint { url, plaintext })
}

/// 解析结果落在局域网之外。
#[derive(Debug)]
struct NotLan(String);

impl std::fmt::Display for NotLan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl StdError for NotLan {}

/// 明文模式下的解析器：解析结果只要有一个不在私有地址范围内就拒绝，连接不会建立，
/// 凭据也就不会发出去。
struct LanOnlyResolver;

impl reqwest::dns::Resolve for LanOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if addrs.is_empty() {
                return Err(format!("{host} 解析不到地址").into());
            }
            if let Some(public) = addrs.iter().find(|addr| !is_private_ip(addr.ip())) {
                return Err(Box::new(NotLan(format!(
                    "{host} 解析到 {}，不是局域网地址。{HTTPS_REQUIRED}",
                    public.ip()
                ))) as Box<dyn StdError + Send + Sync>);
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// 只信任指纹相符的那一张证书。
#[derive(Debug)]
struct PinnedCert {
    sha256: [u8; 32],
    provider: Arc<CryptoProvider>,
}

/// 探测用：记下服务端证书的指纹。只在「查看证书指纹」时使用，那个请求不带凭据。
#[derive(Debug)]
struct CaptureCert {
    seen: Arc<Mutex<Option<[u8; 32]>>>,
    provider: Arc<CryptoProvider>,
}

fn cert_sha256(cert: &CertificateDer<'_>) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(ring::digest::digest(&ring::digest::SHA256, cert.as_ref()).as_ref());
    out
}

macro_rules! signature_checks {
    () => {
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    };
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // 指纹不是秘密，普通比较即可。
        if cert_sha256(end_entity) == self.sha256 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    signature_checks!();
}

impl ServerCertVerifier for CaptureCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(cert_sha256(end_entity));
        Ok(ServerCertVerified::assertion())
    }

    signature_checks!();
}

fn tls_config(
    verifier: Arc<dyn ServerCertVerifier>,
    provider: Arc<CryptoProvider>,
) -> TransportResult<rustls::ClientConfig> {
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| TransportError::Other(format!("TLS 初始化失败: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// 解析证书指纹：64 位十六进制，容许 `:` 与空格分隔、大小写不限。
pub fn parse_fingerprint(text: &str) -> TransportResult<[u8; 32]> {
    let cleaned: String = text
        .chars()
        .filter(|c| !matches!(c, ':' | ' ' | '-'))
        .collect::<String>()
        .to_ascii_lowercase();
    let bytes = ids::unhex(&cleaned)
        .filter(|b| b.len() == 32)
        .ok_or_else(|| TransportError::Config("证书指纹应是 64 位十六进制（SHA-256）".into()))?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// 指纹的展示形式：`AB:CD:…`。
pub fn format_fingerprint(sha256: &[u8; 32]) -> String {
    sha256
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

const REDIRECT_MAX: usize = 5;
const PLAINTEXT_REDIRECT: &str = "服务端把请求重定向到了明文 HTTP 地址，已拒绝";

/// 带凭据的请求遇到重定向时怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hop {
    /// 还在最初请求的那个源（协议 + 主机 + 端口）上：照常跟。典型的是目录补斜杠。
    Follow,
    /// 跳到了别的源：不跟，把这个回应原样交还调用方。
    Stop,
    /// 跳到了别的源，而且不是 HTTPS。
    Refuse,
}

/// 凭据只发给配置的那个地址。跳到别的主机、别的端口或别的协议的重定向一律不带着凭据
/// 跟过去。
///
/// 这不能交给 HTTP 库去「跨主机时剥掉凭据」：它只拿目标和上一跳比，而每一跳的请求头
/// 都是从最初那个请求恢复出来的——先跳到别的主机、再在那台主机上跳一次，凭据就又带上
/// 了。所以判据是「最初请求的源」，而且不是同源就干脆不跟。
fn judge_redirect(first: &url::Url, target: &url::Url) -> Hop {
    if target.origin() == first.origin() {
        Hop::Follow
    } else if target.scheme() == "https" {
        Hop::Stop
    } else {
        Hop::Refuse
    }
}

fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= REDIRECT_MAX {
            return attempt.error("重定向次数过多");
        }
        let hop = match attempt.previous().first() {
            Some(first) => judge_redirect(first, attempt.url()),
            None => Hop::Stop,
        };
        match hop {
            Hop::Follow => attempt.follow(),
            Hop::Stop => attempt.stop(),
            Hop::Refuse => attempt.error(PLAINTEXT_REDIRECT),
        }
    })
}

/// 不带任何凭据的客户端：只用来取「下载被服务端转到了别的主机」的对象（有的网盘把下载
/// 交给另一台主机或 CDN，见 [`follow_elsewhere`]）。只走 HTTPS。
pub fn build_bare_client() -> TransportResult<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= REDIRECT_MAX {
                attempt.error("重定向次数过多")
            } else if attempt.url().scheme() == "https" {
                attempt.follow()
            } else {
                attempt.error(PLAINTEXT_REDIRECT)
            }
        }))
        .user_agent(concat!("rclaw-sync/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| TransportError::Other(format!("HTTP 客户端初始化失败: {}", describe(&e))))
}

/// 一个没有跟随的重定向指向哪里。不是重定向、或者没给出能用的地址时返回 `None`；目标
/// 不是 HTTPS 的拒绝。
fn elsewhere_target(
    status: reqwest::StatusCode,
    location: Option<&str>,
    from: &url::Url,
) -> TransportResult<Option<url::Url>> {
    if !status.is_redirection() {
        return Ok(None);
    }
    let Some(target) = location.and_then(|location| from.join(location).ok()) else {
        return Ok(None);
    };
    if target.scheme() != "https" {
        return Err(TransportError::Insecure(PLAINTEXT_REDIRECT.into()));
    }
    Ok(Some(target))
}

/// 下载请求的回应是一个没有跟随的重定向（目标在别的源上）时，用不带凭据的客户端 `bare`
/// 去取。对象都是加密并带认证的，从哪台主机取回来不影响内容的校验。别的回应原样返回。
pub async fn follow_elsewhere(
    bare: &reqwest::Client,
    response: reqwest::Response,
) -> TransportResult<reqwest::Response> {
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok());
    match elsewhere_target(response.status(), location, response.url())? {
        Some(target) => send(bare.get(target)).await,
        None => Ok(response),
    }
}

/// 按连接策略建 HTTP 客户端。
pub fn build_client(
    endpoint: &Endpoint,
    pinned: Option<[u8; 32]>,
) -> TransportResult<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(redirect_policy())
        .user_agent(concat!("rclaw-sync/", env!("CARGO_PKG_VERSION")));
    if endpoint.plaintext {
        // 经代理转发时看不到实际连接的 IP，局域网判定就失效了。
        builder = builder.no_proxy().dns_resolver(Arc::new(LanOnlyResolver));
    }
    if let Some(sha256) = pinned {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(PinnedCert {
            sha256,
            provider: provider.clone(),
        });
        builder = builder.use_preconfigured_tls(tls_config(verifier, provider)?);
    } else if !endpoint.plaintext {
        for cert in rustls_native_certs::load_native_certs().certs {
            if let Ok(cert) = reqwest::Certificate::from_der(cert.as_ref()) {
                builder = builder.add_root_certificate(cert);
            }
        }
    }
    builder
        .build()
        .map_err(|e| TransportError::Other(format!("HTTP 客户端初始化失败: {}", describe(&e))))
}

/// 取服务端证书的 SHA-256 指纹，给用户核对后固定。请求不带凭据、不带数据。
pub async fn probe_certificate(raw_url: &str) -> TransportResult<[u8; 32]> {
    let endpoint = check_endpoint(raw_url, false)?;
    let seen = Arc::new(Mutex::new(None));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(CaptureCert {
        seen: seen.clone(),
        provider: provider.clone(),
    });
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .use_preconfigured_tls(tls_config(verifier, provider)?)
        .build()
        .map_err(|e| TransportError::Other(format!("HTTP 客户端初始化失败: {}", describe(&e))))?;
    let mut root = endpoint.url.clone();
    root.set_path("/");
    let outcome = tokio::time::timeout(
        IDLE_TIMEOUT,
        client.request(reqwest::Method::OPTIONS, root).send(),
    )
    .await;
    let fingerprint = *seen.lock().unwrap_or_else(|e| e.into_inner());
    match (fingerprint, outcome) {
        (Some(fingerprint), _) => Ok(fingerprint),
        (None, Ok(Err(e))) => Err(map_error(e)),
        (None, Ok(Ok(_))) => Err(TransportError::Other("没有取到服务端证书".into())),
        (None, Err(_)) => Err(TransportError::Timeout("连接服务端超时".into())),
    }
}

/// 错误链连成一句话（reqwest 的 Display 只有最外层，证书错误之类藏在里面）。
fn describe(error: &dyn StdError) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        let inner_text = inner.to_string();
        if !text.contains(&inner_text) {
            text.push_str(": ");
            text.push_str(&inner_text);
        }
        source = inner.source();
    }
    text
}

/// reqwest 的错误 → 传输错误。地址不进错误文本。
pub fn map_error(error: reqwest::Error) -> TransportError {
    let error = error.without_url();
    let mut source: Option<&(dyn StdError + 'static)> = Some(&error);
    while let Some(inner) = source {
        if let Some(not_lan) = inner.downcast_ref::<NotLan>() {
            return TransportError::Insecure(not_lan.0.clone());
        }
        source = inner.source();
    }
    let text = describe(&error);
    if text.contains("明文 HTTP") {
        TransportError::Insecure(text)
    } else if error.is_timeout() {
        TransportError::Timeout(text)
    } else if text.contains("ApplicationVerificationFailure") {
        TransportError::Insecure(
            "服务端证书与已固定的指纹不符：可能是证书换过了，也可能连到了别的服务器。请核对后重新固定指纹".into(),
        )
    } else if text.contains("certificate") || text.contains("UnknownIssuer") {
        TransportError::Insecure(format!(
            "服务端证书校验失败（{text}）。自签证书请在设置里核对并固定证书指纹"
        ))
    } else {
        TransportError::Other(text)
    }
}

/// `Retry-After`：秒数或 HTTP 日期。上限 1 小时。
fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    let secs = match value.parse::<u64>() {
        Ok(secs) => secs,
        Err(_) => {
            let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
            (at.timestamp() - chrono::Utc::now().timestamp()).max(0) as u64
        }
    };
    Some(Duration::from_secs(secs.min(3600)))
}

/// 非成功状态码 → 传输错误。`what` 是「做什么失败了」，会原样进入给用户看的文字。
///
/// 「对象不存在」不在这里判：读一个对象、问一个对象在不在的时候 404 是正常的回答，由
/// 调用方自己先接住。到了这里的 404 是写入、建目录、列目录得到的——地址、目录或者存储桶
/// 本身不对，不是哪个对象还没到。
pub fn status_error(response: &reqwest::Response, what: &str) -> TransportError {
    let status = response.status();
    match status.as_u16() {
        // 没有跟随的重定向：目标在别的主机（或别的端口、协议）上，凭据不跟过去。
        300..=399 => TransportError::Other(format!(
            "{what}失败：服务端把请求转到了别的地址（HTTP {status}），没有跟过去。请把存储地址改成它实际所在的地址"
        )),
        401 | 403 => TransportError::Auth(format!("{what}被拒绝（HTTP {status}）：请检查用户名、密码与目录权限")),
        404 | 410 => TransportError::Config(format!(
            "{what}失败：服务端说这个地址不存在（HTTP {status}）。请检查存储地址、目录或存储桶的名字"
        )),
        413 => TransportError::TooLarge(format!(
            "{what}失败：对象超过了服务端的上传大小上限（HTTP 413），请调大服务端的限制"
        )),
        429 | 503 => TransportError::RateLimited {
            retry_after: retry_after(response),
            message: format!("{what}被服务端限流（HTTP {status}）"),
        },
        507 => TransportError::Other(format!("{what}失败：存储空间不足（HTTP 507）")),
        _ => TransportError::Other(format!("{what}失败（HTTP {status}）")),
    }
}

/// 请求体的来源。
#[derive(Clone)]
pub enum Upload {
    Bytes(Bytes),
    File(PathBuf),
}

impl Upload {
    pub async fn size(&self) -> TransportResult<u64> {
        match self {
            Upload::Bytes(bytes) => Ok(bytes.len() as u64),
            Upload::File(path) => file_len(path).await,
        }
    }
}

async fn file_len(path: &Path) -> TransportResult<u64> {
    tokio::fs::metadata(path)
        .await
        .map(|meta| meta.len())
        .map_err(|e| TransportError::Other(format!("读取 {} 失败: {e}", path.display())))
}

/// 发一个不带大请求体的请求，等响应头最多 [`IDLE_TIMEOUT`]。
pub async fn send(builder: reqwest::RequestBuilder) -> TransportResult<reqwest::Response> {
    match tokio::time::timeout(IDLE_TIMEOUT, builder.send()).await {
        Ok(result) => result.map_err(map_error),
        Err(_) => Err(TransportError::Timeout("服务端长时间没有响应".into())),
    }
}

/// 请求体全部交出去之后，等服务端回应最多这么久。
///
/// 「交出去」只是交给了发送缓冲：网络慢的时候最后几兆还在路上。服务端（或者它前面的
/// 反向代理）收完之后还要落盘、转存，花的时间也随对象的大小涨。所以在 [`IDLE_TIMEOUT`]
/// 之上按大小加一段，封顶 [`SETTLE_MAX`]。
fn settle_wait(len: u64) -> Duration {
    (IDLE_TIMEOUT + Duration::from_secs(len / SETTLE_BYTES_PER_SEC)).min(SETTLE_MAX)
}

/// 发一个带请求体的请求。请求体分块流出，每流出一块记一次进度；进度停滞超过
/// [`IDLE_TIMEOUT`] 才算超时，所以大文件、慢网络不受总时长限制。请求体流完之后等的是
/// 服务端的回应，时限见 [`settle_wait`]。
pub async fn send_upload(
    builder: reqwest::RequestBuilder,
    upload: Upload,
) -> TransportResult<reqwest::Response> {
    let len = upload.size().await?;
    let started = Instant::now();
    // 最近一次流出一块的时刻（距 `started` 的毫秒数），和已经流出的字节数。
    let progress = Arc::new(AtomicU64::new(0));
    let handed = Arc::new(AtomicU64::new(0));
    let touch = {
        let (progress, handed) = (progress.clone(), handed.clone());
        move |bytes: usize| {
            progress.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            handed.fetch_add(bytes as u64, Ordering::Relaxed);
        }
    };

    let body = match upload {
        Upload::Bytes(bytes) => {
            let chunks = (0..bytes.len()).step_by(UPLOAD_CHUNK).map(move |start| {
                let end = (start + UPLOAD_CHUNK).min(bytes.len());
                bytes.slice(start..end)
            });
            let stream = futures::stream::iter(chunks).map(move |chunk| {
                touch(chunk.len());
                Ok::<Bytes, std::io::Error>(chunk)
            });
            reqwest::Body::wrap_stream(stream)
        }
        Upload::File(path) => {
            let file = tokio::fs::File::open(&path)
                .await
                .map_err(|e| TransportError::Other(format!("读取 {} 失败: {e}", path.display())))?;
            let stream = futures::stream::try_unfold(file, move |mut file| {
                let touch = touch.clone();
                async move {
                    let mut buf = vec![0u8; UPLOAD_CHUNK];
                    let n = file.read(&mut buf).await?;
                    touch(n);
                    if n == 0 {
                        return Ok::<_, std::io::Error>(None);
                    }
                    buf.truncate(n);
                    Ok(Some((Bytes::from(buf), file)))
                }
            });
            reqwest::Body::wrap_stream(stream)
        }
    };

    // 显式给出长度：不少 WebDAV 服务端不接受分块传输编码的 PUT。
    let request = builder
        .header(reqwest::header::CONTENT_LENGTH, len)
        .body(body)
        .send();
    tokio::pin!(request);
    let idle = IDLE_TIMEOUT.as_millis() as u64;
    let settle = settle_wait(len).as_millis() as u64;
    loop {
        tokio::select! {
            result = &mut request => return result.map_err(map_error),
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                let quiet = (started.elapsed().as_millis() as u64)
                    .saturating_sub(progress.load(Ordering::Relaxed));
                // 请求体还没流完：看的是多久没再流出下一块。流完了：剩下的是缓冲里的数据
                // 上路、服务端处理，按对象的大小多等一阵。
                if handed.load(Ordering::Relaxed) < len {
                    if quiet > idle {
                        return Err(TransportError::Timeout(
                            "上传长时间没有进展，已中断".into(),
                        ));
                    }
                } else if quiet > settle {
                    return Err(TransportError::Timeout(
                        "上传的内容发出去之后，服务端长时间没有回应".into(),
                    ));
                }
            }
        }
    }
}

async fn next_chunk(response: &mut reqwest::Response) -> TransportResult<Option<Bytes>> {
    match tokio::time::timeout(IDLE_TIMEOUT, response.chunk()).await {
        Ok(result) => result.map_err(map_error),
        Err(_) => Err(TransportError::Timeout("下载长时间没有进展，已中断".into())),
    }
}

/// 响应头里声明的长度已经超过上限：响应体一个字节都不读。
fn check_declared(response: &reqwest::Response, key: &str, max: u64) -> TransportResult<()> {
    match response.content_length() {
        Some(len) if len > max => Err(oversize(key, max)),
        _ => Ok(()),
    }
}

/// 把响应体读进内存，最多 `max` 字节（`key` 是对象名，报错用）。服务端不可信：声明的
/// 长度超过上限时不读；没声明长度（或声明得不对）的，读到超过上限的那一块就停。
pub async fn read_body(
    mut response: reqwest::Response,
    key: &str,
    max: u64,
) -> TransportResult<Vec<u8>> {
    check_declared(&response, key, max)?;
    let mut out = Vec::new();
    while let Some(chunk) = next_chunk(&mut response).await? {
        if out.len() as u64 + chunk.len() as u64 > max {
            return Err(oversize(key, max));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// 读服务端自己的 XML 响应（目录列表、报错说明），最多 `max` 字节。它不是同步对象，过大
/// 时按普通的访问失败报。
pub async fn read_xml(response: reqwest::Response, max: usize) -> TransportResult<Vec<u8>> {
    match read_body(response, "服务端的 XML 响应", max as u64).await {
        Err(TransportError::Oversize(_)) => {
            Err(TransportError::Other("服务端返回的 XML 过大".into()))
        }
        other => other,
    }
}

/// 把响应体流式写到文件，最多 `max` 字节。超过上限时中断并把写了一半的文件删掉。
pub async fn read_body_to_file(
    mut response: reqwest::Response,
    key: &str,
    dest: &Path,
    max: u64,
) -> TransportResult<()> {
    use tokio::io::AsyncWriteExt;
    check_declared(&response, key, max)?;
    let write_error =
        |e: std::io::Error| TransportError::Other(format!("写入 {} 失败: {e}", dest.display()));
    let mut file = tokio::fs::File::create(dest).await.map_err(write_error)?;
    let mut written = 0u64;
    while let Some(chunk) = next_chunk(&mut response).await? {
        written += chunk.len() as u64;
        if written > max {
            drop(file);
            let _ = tokio::fs::remove_file(dest).await;
            return Err(oversize(key, max));
        }
        file.write_all(&chunk).await.map_err(write_error)?;
    }
    file.flush().await.map_err(write_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges() {
        for ip in [
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.1",
            "127.0.0.1",
            "169.254.10.10",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "::ffff:192.168.1.1",
        ] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "172.32.0.1",
            "172.15.255.255",
            "192.169.0.1",
            "100.64.0.1",
            "2001:db8::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn public_addresses_must_use_https() {
        // 公网 IP 走明文：无论是否显式允许都拒绝。
        for allow_http in [false, true] {
            assert!(matches!(
                check_endpoint("http://8.8.8.8/dav", allow_http),
                Err(TransportError::Insecure(_))
            ));
        }
        // 局域网地址走明文：要显式允许。
        assert!(matches!(
            check_endpoint("http://192.168.1.10/dav", false),
            Err(TransportError::Insecure(_))
        ));
        let lan = check_endpoint("http://192.168.1.10:5005/dav", true).unwrap();
        assert!(lan.plaintext);
        assert_eq!(lan.url.as_str(), "http://192.168.1.10:5005/dav/");
        // 主机名走明文：留到连接时按解析结果判，这里只要求显式允许。
        assert!(check_endpoint("http://nas.local/dav", false).is_err());
        assert!(
            check_endpoint("http://nas.local/dav", true)
                .unwrap()
                .plaintext
        );
        // HTTPS 总是可以。
        let https = check_endpoint("https://dav.example.com/rclaw", false).unwrap();
        assert!(!https.plaintext);
        assert_eq!(https.url.as_str(), "https://dav.example.com/rclaw/");
    }

    #[test]
    fn credentials_only_follow_redirects_that_stay_on_the_first_origin() {
        let url = |text: &str| url::Url::parse(text).unwrap();
        let first = url("https://dav.example.com/rclaw/heads");
        // 目录补斜杠之类：还在原来的源上。
        assert_eq!(
            judge_redirect(&first, &url("https://dav.example.com/rclaw/heads/")),
            Hop::Follow
        );
        // 别的主机、同一台主机的别的端口：不带着凭据跟过去。
        assert_eq!(
            judge_redirect(&first, &url("https://cdn.other.example/a")),
            Hop::Stop
        );
        assert_eq!(
            judge_redirect(&first, &url("https://dav.example.com:8443/rclaw/")),
            Hop::Stop
        );
        // 判据是最初请求的源，不是上一跳：先跳到别的主机、再在那台主机上跳，也不是同源。
        assert_eq!(
            judge_redirect(&first, &url("https://cdn.other.example/b")),
            Hop::Stop
        );
        // 跳到明文 HTTP：拒绝，同一台主机也一样。
        assert_eq!(
            judge_redirect(&first, &url("http://dav.example.com/rclaw/")),
            Hop::Refuse
        );
        assert_eq!(
            judge_redirect(&first, &url("http://cdn.other.example/a")),
            Hop::Refuse
        );
        // 局域网明文模式：同源照常跟，别处的 HTTPS 交还调用方，别处的明文拒绝。
        let lan = url("http://192.168.1.10:5005/dav/heads");
        assert_eq!(
            judge_redirect(&lan, &url("http://192.168.1.10:5005/dav/heads/")),
            Hop::Follow
        );
        assert_eq!(
            judge_redirect(&lan, &url("https://192.168.1.10/dav/")),
            Hop::Stop
        );
        assert_eq!(
            judge_redirect(&lan, &url("http://192.168.1.11:5005/dav/")),
            Hop::Refuse
        );
    }

    #[test]
    fn a_download_sent_elsewhere_is_fetched_only_over_https() {
        use reqwest::StatusCode;
        let from = url::Url::parse("https://dav.example.com/rclaw/heads/abc").unwrap();
        // 不是重定向、没有地址：原样交还。
        assert_eq!(
            elsewhere_target(StatusCode::OK, Some("https://x.example/"), &from).unwrap(),
            None
        );
        assert_eq!(
            elsewhere_target(StatusCode::FOUND, None, &from).unwrap(),
            None
        );
        // 绝对地址与相对地址。
        assert_eq!(
            elsewhere_target(
                StatusCode::FOUND,
                Some("https://cdn.other.example/a?sig=1"),
                &from
            )
            .unwrap()
            .map(|target| target.to_string()),
            Some("https://cdn.other.example/a?sig=1".to_string())
        );
        assert_eq!(
            elsewhere_target(
                StatusCode::TEMPORARY_REDIRECT,
                Some("//cdn.other.example/a"),
                &from
            )
            .unwrap()
            .map(|target| target.to_string()),
            Some("https://cdn.other.example/a".to_string())
        );
        // 明文 HTTP 的目标拒绝。
        assert!(matches!(
            elsewhere_target(StatusCode::FOUND, Some("http://cdn.other.example/a"), &from),
            Err(TransportError::Insecure(_))
        ));
    }

    #[test]
    fn rejects_malformed_endpoints() {
        for bad in [
            "",
            "dav.example.com/x",
            "ftp://example.com/x",
            "https://user:pass@example.com/x",
            "https://example.com/x?token=1",
            "https://example.com/x#frag",
        ] {
            assert!(
                matches!(check_endpoint(bad, false), Err(TransportError::Config(_))),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn lan_only_resolver_rejects_public_results() {
        use reqwest::dns::Resolve;
        use std::str::FromStr;
        // localhost 解析到回环地址：放行。
        let name = reqwest::dns::Name::from_str("localhost").unwrap();
        let addrs: Vec<SocketAddr> = LanOnlyResolver.resolve(name).await.unwrap().collect();
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|addr| is_private_ip(addr.ip())));
    }

    #[test]
    fn the_wait_after_the_body_grows_with_its_size_up_to_a_cap() {
        assert_eq!(settle_wait(0), IDLE_TIMEOUT);
        assert_eq!(
            settle_wait(2 * 1024 * 1024),
            IDLE_TIMEOUT + Duration::from_secs(128)
        );
        assert_eq!(settle_wait(1 << 30), SETTLE_MAX);
        assert_eq!(settle_wait(u64::MAX), SETTLE_MAX);
    }

    #[test]
    fn fingerprint_round_trip() {
        let bytes = [0xabu8; 32];
        let shown = format_fingerprint(&bytes);
        assert_eq!(shown.len(), 32 * 3 - 1);
        assert_eq!(parse_fingerprint(&shown).unwrap(), bytes);
        assert_eq!(parse_fingerprint(&"ab".repeat(32)).unwrap(), bytes);
        assert!(parse_fingerprint("abcd").is_err());
        assert!(parse_fingerprint(&"zz".repeat(32)).is_err());
    }
}
