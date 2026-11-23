//! `webdav` 后端（§10.2）：一个 WebDAV 地址加一组用户名、密码。
//!
//! 现成的 WebDAV 网盘和自建的文件服务器（包括放在公网云主机上的）用的都是这个后端，
//! 没有区别。服务端只存加密文件，不需要运行 RClaw，也不需要装任何专门的程序。
//!
//! 只用最基础的操作：`PROPFIND`（Depth 0 / 1）、`GET`、`PUT`、`DELETE`、`MKCOL`，加
//! Basic 认证。不假设 `PUT` 是原子的，不假设 ETag 可靠：写入后核对长度，读者靠解密
//! 校验发现半截对象。连接策略（公网必须 HTTPS 等）见 [`super::net`]。

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use bytes::Bytes;
use reqwest::{Method, StatusCode};

use super::net::{self, Endpoint, Upload};
use super::xml::{xml_walk, XmlEvent, XML_MAX};
use super::{
    check_key, is_object_name, too_many, ObjectMeta, SyncTransport, TransportResult, LIST_MAX,
};
use crate::error::TransportError;

/// 连接参数。
#[derive(Clone)]
pub struct WebdavOptions {
    pub url: String,
    pub username: String,
    pub password: String,
    /// 明文 HTTP：只对局域网地址有效。
    pub allow_http: bool,
    /// 固定的服务端证书指纹（SHA-256，十六进制）；空 = 信任系统与内置公共 CA。
    pub pinned_cert_sha256: String,
}

pub struct WebdavTransport {
    client: reqwest::Client,
    /// 不带账号密码的客户端：下载被服务端转到别的主机时用它去取。用到时才建。
    bare: OnceLock<reqwest::Client>,
    endpoint: Endpoint,
    username: String,
    password: String,
    /// 本进程已确认存在的目录（键以 `/` 结尾）。
    known_dirs: Mutex<HashSet<String>>,
}

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getetag/><d:getlastmodified/></d:prop></d:propfind>"#;

fn propfind() -> Method {
    Method::from_bytes(b"PROPFIND").expect("PROPFIND 是合法的方法名")
}

fn mkcol() -> Method {
    Method::from_bytes(b"MKCOL").expect("MKCOL 是合法的方法名")
}

/// multistatus 里的一项。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct DavEntry {
    /// 解码后的路径。
    path: String,
    collection: bool,
    size: Option<u64>,
    etag: Option<String>,
    modified: Option<String>,
}

/// `href` → 解码后的路径。`href` 可能是绝对地址，也可能只有路径。
fn href_path(href: &str) -> String {
    let raw = if href.starts_with("http://") || href.starts_with("https://") {
        url::Url::parse(href)
            .map(|url| url.path().to_string())
            .unwrap_or_default()
    } else {
        href.split(['?', '#'])
            .next()
            .unwrap_or_default()
            .to_string()
    };
    percent_encoding::percent_decode_str(&raw)
        .decode_utf8_lossy()
        .into_owned()
}

/// 解析 multistatus。`what` 是被列的目录（报错用）。超过 [`LIST_MAX`] 项就不再读下去。
fn parse_multistatus(xml: &str, what: &str) -> TransportResult<Vec<DavEntry>> {
    let mut entries = Vec::new();
    let mut current: Option<DavEntry> = None;
    let mut field = String::new();
    xml_walk(xml, |event| {
        match event {
            XmlEvent::Start(name) => {
                match name.as_str() {
                    "response" => current = Some(DavEntry::default()),
                    "collection" => {
                        if let Some(entry) = current.as_mut() {
                            entry.collection = true;
                        }
                    }
                    _ => {}
                }
                field = name;
            }
            XmlEvent::Text(text) => {
                if let Some(entry) = current.as_mut() {
                    match field.as_str() {
                        "href" if entry.path.is_empty() => entry.path = href_path(&text),
                        "getcontentlength" => entry.size = text.parse().ok(),
                        "getetag" => entry.etag = Some(text),
                        "getlastmodified" => entry.modified = Some(text),
                        _ => {}
                    }
                }
            }
            XmlEvent::End(name) => {
                field.clear();
                if name == "response" {
                    if let Some(entry) = current.take() {
                        if entries.len() >= LIST_MAX {
                            return Err(too_many(what));
                        }
                        entries.push(entry);
                    }
                }
            }
        }
        Ok(())
    })?;
    Ok(entries)
}

impl WebdavTransport {
    pub fn new(options: &WebdavOptions) -> TransportResult<Self> {
        let endpoint = net::check_endpoint(&options.url, options.allow_http)?;
        let pinned = match options.pinned_cert_sha256.trim() {
            "" => None,
            _ if endpoint.plaintext => None,
            text => Some(net::parse_fingerprint(text)?),
        };
        Ok(WebdavTransport {
            client: net::build_client(&endpoint, pinned)?,
            bare: OnceLock::new(),
            endpoint,
            username: options.username.clone(),
            password: options.password.clone(),
            known_dirs: Mutex::new(HashSet::new()),
        })
    }

    /// `key` 为空表示基地址本身。
    fn url(&self, key: &str) -> TransportResult<url::Url> {
        if key.is_empty() {
            return Ok(self.endpoint.url.clone());
        }
        check_key(key)?;
        self.endpoint
            .url
            .join(key)
            .map_err(|e| TransportError::Config(format!("对象地址不合法: {e}")))
    }

    fn request(&self, method: Method, url: url::Url) -> reqwest::RequestBuilder {
        let builder = self.client.request(method, url);
        if self.username.is_empty() {
            builder
        } else {
            builder.basic_auth(&self.username, Some(&self.password))
        }
    }

    fn dir_known(&self, dir: &str) -> bool {
        self.known_dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(dir)
    }

    fn remember_dir(&self, dir: &str) {
        self.known_dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(dir.to_string());
    }

    fn forget_dirs(&self) {
        self.known_dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// 建一级目录。返回 `false` 表示上级目录不存在（409）。
    async fn make_dir(&self, dir: &str) -> TransportResult<bool> {
        let response = net::send(self.request(mkcol(), self.url(dir)?)).await?;
        let status = response.status();
        // 405：目录已存在。有的服务端对已存在的目录回 2xx 或 3xx。
        if status.is_success()
            || status.is_redirection()
            || status == StatusCode::METHOD_NOT_ALLOWED
        {
            return Ok(true);
        }
        if status == StatusCode::CONFLICT {
            return Ok(false);
        }
        Err(net::status_error(
            &response,
            &format!("建目录 {dir}（MKCOL）"),
        ))
    }

    /// 逐级建出 `key` 的上级目录。建过的记在内存里，同一目录每个进程只建一次。
    async fn ensure_dirs(&self, key: &str) -> TransportResult<()> {
        let mut dir = String::new();
        let parts: Vec<&str> = key.split('/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            dir.push_str(part);
            dir.push('/');
            if self.dir_known(&dir) {
                continue;
            }
            if !self.make_dir(&dir).await? {
                // 上级不存在：第一级的上级是基地址本身，用户可能填了一个还没建的目录。
                let base_ready = dir.matches('/').count() == 1 && self.make_dir("").await?;
                if !base_ready || !self.make_dir(&dir).await? {
                    return Err(TransportError::Other(format!(
                        "建目录 {dir} 失败：上级目录不存在，请先在服务端建好存储目录"
                    )));
                }
            }
            self.remember_dir(&dir);
        }
        Ok(())
    }

    async fn put_upload(&self, key: &str, upload: Upload) -> TransportResult<()> {
        let url = self.url(key)?;
        let mut retried = false;
        loop {
            self.ensure_dirs(key).await?;
            let response =
                net::send_upload(self.request(Method::PUT, url.clone()), upload.clone()).await?;
            let status = response.status();
            if status.is_success() {
                return Ok(());
            }
            // 上级目录被别人删了：内存里的记录作废，重建后再试一次。
            let dir_missing = matches!(status, StatusCode::CONFLICT | StatusCode::NOT_FOUND);
            if dir_missing && !retried {
                retried = true;
                self.forget_dirs();
                continue;
            }
            return Err(net::status_error(&response, &format!("写入 {key}（PUT）")));
        }
    }

    fn bare_client(&self) -> TransportResult<&reqwest::Client> {
        if let Some(client) = self.bare.get() {
            return Ok(client);
        }
        let client = net::build_bare_client()?;
        Ok(self.bare.get_or_init(|| client))
    }

    async fn get_response(&self, key: &str) -> TransportResult<reqwest::Response> {
        let mut response = net::send(self.request(Method::GET, self.url(key)?)).await?;
        // 有的服务端把下载交给另一台主机（网盘的直链、CDN）。账号密码不跟过去：改用不带
        // 凭据的请求去取。只有下载这样做，别的操作被转走一律算失败。
        let elsewhere = response.status().is_redirection();
        if elsewhere {
            response = net::follow_elsewhere(self.bare_client()?, response).await?;
        }
        match response.status() {
            StatusCode::OK => Ok(response),
            StatusCode::NOT_FOUND | StatusCode::GONE => {
                Err(TransportError::NotFound(key.to_string()))
            }
            // 那台主机要的是账号密码：不是用户填错了，是地址该换成它实际所在的那个。
            status @ (StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) if elsewhere => {
                Err(TransportError::Other(format!(
                    "读取 {key}（GET）失败：服务端把下载转到了别的地址，那里不接受不带账号密码的请求（HTTP {status}）。请把存储地址改成它实际所在的地址"
                )))
            }
            _ => Err(net::status_error(&response, &format!("读取 {key}（GET）"))),
        }
    }
}

#[async_trait]
impl SyncTransport for WebdavTransport {
    async fn list(&self, prefix: &str) -> TransportResult<Vec<ObjectMeta>> {
        let is_dir = prefix.ends_with('/');
        let url = self.url(prefix)?;
        let request = self
            .request(propfind(), url.clone())
            .header("Depth", if is_dir { "1" } else { "0" })
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/xml; charset=utf-8",
            )
            .body(PROPFIND_BODY);
        let response = net::send(request).await?;
        let status = response.status();
        if matches!(status, StatusCode::NOT_FOUND | StatusCode::GONE) {
            return Ok(Vec::new());
        }
        if matches!(
            status,
            StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
        ) {
            return Err(TransportError::Unsupported(
                "这个地址不支持 WebDAV（服务端拒绝了 PROPFIND），请检查地址".into(),
            ));
        }
        if status != StatusCode::MULTI_STATUS && status != StatusCode::OK {
            return Err(net::status_error(
                &response,
                &format!("列出 {prefix}（PROPFIND）"),
            ));
        }
        let body = net::read_xml(response, XML_MAX).await?;
        let entries = parse_multistatus(&String::from_utf8_lossy(&body), prefix)?;
        drop(body);

        let own_path = percent_encoding::percent_decode_str(url.path())
            .decode_utf8_lossy()
            .trim_end_matches('/')
            .to_string();
        let wanted = (!is_dir).then(|| prefix.rsplit('/').next().unwrap_or(prefix));
        let mut out = Vec::new();
        for entry in entries {
            let path = entry.path.trim_end_matches('/');
            // 目录项、被列目录自己。
            if entry.collection || entry.path.ends_with('/') || (is_dir && path == own_path) {
                continue;
            }
            let name = path.rsplit('/').next().unwrap_or_default();
            if !is_object_name(name) || wanted.is_some_and(|wanted| wanted != name) {
                continue;
            }
            let Some(size) = entry.size else {
                return Err(TransportError::Unsupported(
                    "服务端的目录列表没有给出文件长度（getcontentlength），无法用于同步".into(),
                ));
            };
            // 强 ETag（`"opaque"`）算强标识。弱 ETag（`W/"…"`：服务端不保证两次取回
            // 逐字节一致）、不带引号的非合规值、只有修改时间的不算：等长的两次更新可以
            // 带着同一个弱标签、同一个秒级时间——同长度的新内容会被当成旧值，读者据此
            // 不能判重（[`ObjectMeta::stable_version`]），只能按有界的窗口定期实读。
            let (version, stable_version, strong_version) = match (entry.etag, entry.modified) {
                (Some(etag), _) => {
                    let trimmed = etag.trim();
                    let strong = trimmed.starts_with('"') && trimmed.ends_with('"');
                    (etag, true, strong)
                }
                (None, modified) => {
                    let present = modified.is_some();
                    (modified.unwrap_or_default(), present, false)
                }
            };
            out.push(ObjectMeta {
                name: name.to_string(),
                size,
                version,
                stable_version,
                strong_version,
            });
        }
        Ok(out)
    }

    async fn get(&self, key: &str, max: u64) -> TransportResult<Vec<u8>> {
        net::read_body(self.get_response(key).await?, key, max).await
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> TransportResult<()> {
        self.put_upload(key, Upload::Bytes(Bytes::from(body))).await
    }

    async fn put_new(&self, key: &str, body: Vec<u8>) -> TransportResult<()> {
        // 仅创建：`If-None-Match: *` 让已有的标记（尤其是另一设备刚写的关闭标记）
        // 原样保留，迟到的创建写入拿不到覆盖机会。
        let url = self.url(key)?;
        self.ensure_dirs(key).await?;
        let request = self
            .request(Method::PUT, url)
            .header("If-None-Match", "*");
        let response = net::send_upload(request, Upload::Bytes(Bytes::from(body))).await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(net::status_error(&response, &format!("创建 {key}（PUT）")))
        }
    }

    async fn delete(&self, key: &str) -> TransportResult<()> {
        let response = net::send(self.request(Method::DELETE, self.url(key)?)).await?;
        let status = response.status();
        if status.is_success() || matches!(status, StatusCode::NOT_FOUND | StatusCode::GONE) {
            Ok(())
        } else {
            Err(net::status_error(
                &response,
                &format!("删除 {key}（DELETE）"),
            ))
        }
    }

    async fn put_file(&self, key: &str, source: &Path) -> TransportResult<()> {
        self.put_upload(key, Upload::File(source.to_path_buf()))
            .await
    }

    async fn get_file(&self, key: &str, dest: &Path, max: u64) -> TransportResult<()> {
        net::read_body_to_file(self.get_response(key).await?, key, dest, max).await
    }

    fn kind(&self) -> &'static str {
        "webdav"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(url: &str, allow_http: bool) -> WebdavOptions {
        WebdavOptions {
            url: url.into(),
            username: "u".into(),
            password: "p".into(),
            allow_http,
            pinned_cert_sha256: String::new(),
        }
    }

    #[test]
    fn construction_applies_https_rule() {
        assert!(matches!(
            WebdavTransport::new(&options("http://dav.example.com/rclaw", false)),
            Err(TransportError::Insecure(_))
        ));
        assert!(matches!(
            WebdavTransport::new(&options("http://8.8.8.8/rclaw", true)),
            Err(TransportError::Insecure(_))
        ));
        assert!(WebdavTransport::new(&options("https://dav.example.com/rclaw", false)).is_ok());
        assert!(WebdavTransport::new(&options("http://192.168.1.2/rclaw", true)).is_ok());
    }

    #[test]
    fn urls_stay_under_the_base() {
        let transport =
            WebdavTransport::new(&options("https://dav.example.com/rclaw", false)).unwrap();
        assert_eq!(
            transport.url("heads/abc").unwrap().as_str(),
            "https://dav.example.com/rclaw/heads/abc"
        );
        assert_eq!(
            transport.url("heads/").unwrap().as_str(),
            "https://dav.example.com/rclaw/heads/"
        );
        assert!(transport.url("../x").is_err());
        assert!(transport.url("/etc/passwd").is_err());
        assert!(transport.url("https://evil.example/x").is_err());
    }

    #[test]
    fn parses_typical_multistatus() {
        // Apache / Nginx 风格：带前缀、目录自己也在列表里。
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/rclaw/heads/</D:href>
    <D:propstat>
      <D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/rclaw/heads/0123456789abcdef</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
        <D:getcontentlength>1234</D:getcontentlength>
        <D:getetag>"abc-1"</D:getetag>
        <D:getlastmodified>Wed, 18 Nov 2026 08:00:00 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>https://dav.example.com/rclaw/heads/fedcba9876543210%20%28copy%29</D:href>
    <D:propstat>
      <D:prop><D:getcontentlength>5</D:getcontentlength></D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        let entries = parse_multistatus(xml, "heads/").unwrap();
        assert_eq!(entries.len(), 3);
        assert!(entries[0].collection);
        assert_eq!(entries[1].path, "/rclaw/heads/0123456789abcdef");
        assert_eq!(entries[1].size, Some(1234));
        assert_eq!(entries[1].etag.as_deref(), Some("\"abc-1\""));
        assert!(!entries[1].collection);
        assert_eq!(entries[2].path, "/rclaw/heads/fedcba9876543210 (copy)");

        // 无前缀的默认命名空间写法。
        let bare = r#"<multistatus xmlns="DAV:"><response><href>/x/a</href><propstat><prop><getcontentlength>7</getcontentlength><getlastmodified>t</getlastmodified></prop></propstat></response></multistatus>"#;
        let entries = parse_multistatus(bare, "x/").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, Some(7));
        assert_eq!(entries[0].modified.as_deref(), Some("t"));

        assert!(parse_multistatus("<a><b></a>", "x/").is_err());
        assert!(parse_multistatus("", "x/").unwrap().is_empty());
    }

    #[test]
    fn a_listing_with_an_absurd_number_of_entries_is_refused() {
        let entry = "<response><href>/x/a</href></response>";
        let fits = format!("<multistatus>{}</multistatus>", entry.repeat(LIST_MAX));
        assert_eq!(parse_multistatus(&fits, "x/").unwrap().len(), LIST_MAX);
        let over = format!("<multistatus>{}</multistatus>", entry.repeat(LIST_MAX + 1));
        let refused = parse_multistatus(&over, "x/").unwrap_err();
        assert!(refused.to_string().contains("x/"), "{refused}");
    }
}
