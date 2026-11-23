//! `s3` 后端（§10.3）：S3 兼容的对象存储。
//!
//! 请求签名（SigV4）只用到 HMAC 和 SHA-256，不引入 SDK。只用四个操作：
//! ListObjectsV2、GetObject、PutObject、DeleteObject。地址遵守与 `webdav` 相同的
//! HTTPS 规则（见 [`super::net`]）。

use std::collections::HashSet;
use std::path::Path;

use async_trait::async_trait;
use bytes::Bytes;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::{Method, StatusCode};
use ring::hmac;

use super::net::{self, Endpoint, Upload};
use super::xml::{xml_walk, XmlEvent, XML_ERROR_MAX};
use super::{
    check_key, is_object_name, too_many, ObjectMeta, SyncTransport, TransportResult, LIST_MAX,
};
use crate::crypto::{sha256_hex, sha256_reader};
use crate::error::TransportError;
use crate::ids;

/// 连接参数。
#[derive(Clone)]
pub struct S3Options {
    /// 服务地址，如 `https://s3.us-east-1.amazonaws.com`。
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// 桶内目录，可空。
    pub prefix: String,
    pub access_key: String,
    pub secret_key: String,
    /// 路径式寻址（`<地址>/<桶>/<键>`）。自建的 MinIO 等通常需要；默认用虚拟主机式。
    pub path_style: bool,
    /// 明文 HTTP：只对局域网地址有效。
    pub allow_http: bool,
}

pub struct S3Transport {
    client: reqwest::Client,
    /// 请求的源（协议 + 主机 + 端口），不带结尾的 `/`。
    origin: String,
    /// 签名用的 `host` 头。
    host: String,
    /// 桶内对象路径的前缀：路径式为 `/<桶>/`，虚拟主机式为 `/`；后面接桶内目录。
    path_root: String,
    /// 列对象时的桶路径：路径式为 `/<桶>/`，虚拟主机式为 `/`。
    bucket_path: String,
    /// 桶内目录（空，或以 `/` 结尾）。
    prefix: String,
    region: String,
    access_key: String,
    secret_key: String,
}

/// SigV4 的 URI 编码：只保留 `A-Z a-z 0-9 - _ . ~`。
const AWS_ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
        .as_ref()
        .to_vec()
}

/// 一次请求里参与签名的要素。
struct SignInput<'a> {
    method: &'a str,
    /// 规范 URI（已编码的路径）。
    path: &'a str,
    /// 规范查询串（已按键排序并编码）。
    query: &'a str,
    /// 参与签名的头：名字小写、按名字排序。
    headers: &'a [(&'a str, &'a str)],
    payload_hash: &'a str,
    /// `YYYYMMDDTHHMMSSZ`。
    amz_date: &'a str,
}

/// 计算 `Authorization` 头。
fn authorization(
    input: &SignInput<'_>,
    region: &str,
    access_key: &str,
    secret_key: &str,
) -> String {
    let date = &input.amz_date[..8.min(input.amz_date.len())];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let canonical_headers: String = input
        .headers
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect();
    let signed_headers = input
        .headers
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
        input.method, input.path, input.query, input.payload_hash
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        input.amz_date,
        sha256_hex(canonical_request.as_bytes())
    );
    let mut key = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    for part in [region, "s3", "aws4_request"] {
        key = hmac_sha256(&key, part.as_bytes());
    }
    let signature = ids::hex(&hmac_sha256(&key, string_to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    )
}

/// 规范查询串：按键排序，键值都做 URI 编码。
fn canonical_query(params: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(String, String)> = params
        .iter()
        .map(|(key, value)| {
            (
                utf8_percent_encode(key, AWS_ENCODE).to_string(),
                utf8_percent_encode(value, AWS_ENCODE).to_string(),
            )
        })
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// ListObjectsV2 的一页结果。
#[derive(Debug, Default, PartialEq, Eq)]
struct ListPage {
    /// （完整键，长度，版本标识，版本是否可信）。缺 ETag 时版本标识是修改时间，只是
    /// 提示：等长的两次更新可以带着同一个秒级时间，不能据此判重。
    objects: Vec<(String, u64, String, bool)>,
    next_token: Option<String>,
}

/// 一页列表的长度上限。请求里写明了一页最多一千个对象，到不了这个数。
const LIST_PAGE_MAX: usize = 4 * 1024 * 1024;
/// 一页要的对象数（各家服务端的上限都是一千）。
const LIST_PAGE_KEYS: &str = "1000";
/// 翻页的上限：[`LIST_MAX`] 个对象按每页一千个算是 50 页，留一倍的余量。
const LIST_PAGES_MAX: usize = 100;

/// 翻页的账：翻了几页、一共见到多少个对象、用过哪些翻页标记。
#[derive(Default)]
struct Paging {
    pages: usize,
    objects: usize,
    tokens: HashSet<String>,
}

impl Paging {
    /// 记下刚读完的一页：`objects` 是这一页的对象数（合不合规都算），`next` 是它给的
    /// 下一页的标记。对象总数或页数超了、标记重复了就报错。`what` 是被列的目录（报错用）。
    fn page(&mut self, objects: usize, next: Option<&str>, what: &str) -> TransportResult<()> {
        self.pages += 1;
        self.objects += objects;
        if self.objects > LIST_MAX {
            return Err(too_many(what));
        }
        let Some(next) = next else { return Ok(()) };
        if self.pages >= LIST_PAGES_MAX {
            return Err(TransportError::Other(format!("列出 {what} 时翻页过多")));
        }
        if !self.tokens.insert(next.to_string()) {
            return Err(TransportError::Other(format!(
                "列出 {what} 时服务端给了重复的翻页标记"
            )));
        }
        Ok(())
    }
}

/// 解析一页列表。`what` 是被列的目录（报错用）。超过 [`LIST_MAX`] 项就不再读下去。
fn parse_list(xml: &str, what: &str) -> TransportResult<ListPage> {
    let mut page = ListPage::default();
    let mut truncated = false;
    let mut token = None;
    let mut in_contents = false;
    let mut field = String::new();
    let (mut key, mut size, mut etag, mut modified) = (String::new(), None, None, None);
    xml_walk(xml, |event| {
        match event {
            XmlEvent::Start(name) => {
                if name == "contents" {
                    in_contents = true;
                    (key, size, etag, modified) = (String::new(), None, None, None);
                }
                field = name;
            }
            XmlEvent::Text(text) => match (in_contents, field.as_str()) {
                (true, "key") => key = text,
                (true, "size") => size = text.parse::<u64>().ok(),
                (true, "etag") => etag = Some(text),
                (true, "lastmodified") => modified = Some(text),
                (false, "istruncated") => truncated = text == "true",
                (false, "nextcontinuationtoken") => token = Some(text),
                _ => {}
            },
            XmlEvent::End(name) => {
                field.clear();
                if name == "contents" {
                    in_contents = false;
                    let size = size
                        .take()
                        .ok_or_else(|| TransportError::Other("对象列表里缺少对象长度".into()))?;
                    if page.objects.len() >= LIST_MAX {
                        return Err(too_many(what));
                    }
                    let strong = etag.is_some();
                    let version = etag.take().or(modified.take()).unwrap_or_default();
                    page.objects
                        .push((std::mem::take(&mut key), size, version, strong));
                }
            }
        }
        Ok(())
    })?;
    page.next_token = match (truncated, token) {
        (false, _) => None,
        (true, Some(token)) => Some(token),
        // 说还有下一页却不给标记：不能把这半份当成完整的列表。
        (true, None) => {
            return Err(TransportError::Other(format!(
                "列出 {what} 时服务端说还有下一页，却没有给翻页标记"
            )))
        }
    };
    Ok(page)
}

/// 错误响应里的 `<Code>`：第一个 `<Code>` 元素的文本。
///
/// 它是服务端给的，会进到给用户看的文字里：只收像错误码的（字母、数字和 `.`、`_`、`-`，
/// 不超过 64 个字符），别的当作没有。
fn error_code(xml: &str) -> Option<String> {
    // 0 = 还没见到 `<Code>`；1 = 刚见到，等它的文本；2 = 看完了。
    let mut state = 0u8;
    let mut code = None;
    let _ = xml_walk(xml, |event| {
        match (state, event) {
            (0, XmlEvent::Start(name)) if name == "code" => state = 1,
            (1, XmlEvent::Text(text)) => {
                code = Some(text);
                state = 2;
            }
            (1, _) => state = 2,
            _ => {}
        }
        Ok(())
    });
    code.filter(|code| {
        (1..=64).contains(&code.len())
            && code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    })
}

impl S3Transport {
    pub fn new(options: &S3Options) -> TransportResult<Self> {
        let endpoint: Endpoint = net::check_endpoint(&options.endpoint, options.allow_http)?;
        if endpoint.url.path() != "/" {
            return Err(TransportError::Config(
                "对象存储的服务地址不应带路径；桶内目录请填在「目录」里".into(),
            ));
        }
        let bucket = options.bucket.trim();
        let bucket_ok = (3..=63).contains(&bucket.len())
            && bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.'));
        if !bucket_ok {
            return Err(TransportError::Config("桶名不合法".into()));
        }
        let region = options.region.trim();
        if region.is_empty() || options.access_key.is_empty() || options.secret_key.is_empty() {
            return Err(TransportError::Config(
                "对象存储需要填写区域、访问密钥 ID 与访问密钥".into(),
            ));
        }
        let prefix = options.prefix.trim().trim_matches('/');
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            check_key(prefix)?;
            format!("{prefix}/")
        };

        let mut url = endpoint.url.clone();
        let bucket_path = if options.path_style {
            format!("/{bucket}/")
        } else {
            let host = match endpoint.url.host() {
                Some(url::Host::Domain(domain)) => format!("{bucket}.{domain}"),
                _ => {
                    return Err(TransportError::Config(
                        "服务地址是 IP 时请打开「路径式寻址」".into(),
                    ))
                }
            };
            url.set_host(Some(&host))
                .map_err(|e| TransportError::Config(format!("服务地址不合法: {e}")))?;
            "/".to_string()
        };
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
            None => url.host_str().unwrap_or_default().to_string(),
        };
        let origin = format!("{}://{host}", url.scheme());
        Ok(S3Transport {
            client: net::build_client(&endpoint, None)?,
            origin,
            host,
            path_root: format!("{bucket_path}{prefix}"),
            bucket_path,
            prefix,
            region: region.to_string(),
            access_key: options.access_key.clone(),
            secret_key: options.secret_key.clone(),
        })
    }

    fn object_path(&self, key: &str) -> TransportResult<String> {
        check_key(key)?;
        Ok(format!("{}{key}", self.path_root))
    }

    /// 签名并构造请求。
    fn request(
        &self,
        method: Method,
        path: &str,
        query: &str,
        payload_hash: &str,
    ) -> TransportResult<reqwest::RequestBuilder> {
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let headers = [
            ("host", self.host.as_str()),
            ("x-amz-content-sha256", payload_hash),
            ("x-amz-date", amz_date.as_str()),
        ];
        let auth = authorization(
            &SignInput {
                method: method.as_str(),
                path,
                query,
                headers: &headers,
                payload_hash,
                amz_date: &amz_date,
            },
            &self.region,
            &self.access_key,
            &self.secret_key,
        );
        let mut url = format!("{}{path}", self.origin);
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
        let url = url::Url::parse(&url)
            .map_err(|e| TransportError::Config(format!("对象地址不合法: {e}")))?;
        Ok(self
            .client
            .request(method, url)
            .header("x-amz-content-sha256", payload_hash)
            .header("x-amz-date", amz_date)
            .header(reqwest::header::AUTHORIZATION, auth))
    }

    /// 报错回应里的错误码（没有、读不出来、不像错误码都是 `None`）。
    async fn code_of(response: reqwest::Response) -> Option<String> {
        let body = net::read_xml(response, XML_ERROR_MAX).await.ok()?;
        error_code(&String::from_utf8_lossy(&body))
    }

    async fn fail(response: reqwest::Response, what: &str) -> TransportError {
        let error = net::status_error(&response, what);
        let Some(code) = Self::code_of(response).await else {
            return error;
        };
        // 错误码说得比状态码细（桶不存在、签名不对、区域不对……），一律带上。
        match error {
            TransportError::Auth(text) => TransportError::Auth(format!("{text}（{code}）")),
            TransportError::Config(text) => TransportError::Config(format!("{text}（{code}）")),
            TransportError::TooLarge(text) => TransportError::TooLarge(format!("{text}（{code}）")),
            TransportError::Other(text) => TransportError::Other(format!("{text}（{code}）")),
            TransportError::RateLimited {
                retry_after,
                message,
            } => TransportError::RateLimited {
                retry_after,
                message: format!("{message}（{code}）"),
            },
            other => other,
        }
    }

    async fn get_response(&self, key: &str) -> TransportResult<reqwest::Response> {
        let path = self.object_path(key)?;
        let response = net::send(self.request(Method::GET, &path, "", EMPTY_SHA256)?).await?;
        match response.status() {
            StatusCode::OK => Ok(response),
            StatusCode::NOT_FOUND | StatusCode::GONE => {
                // 桶不存在也是 404：那是配置不对，不是这个对象还没到。
                match Self::code_of(response).await.as_deref() {
                    Some("NoSuchBucket") => Err(TransportError::Config(format!(
                        "读取 {key} 失败：存储桶不存在（NoSuchBucket）。请检查存储桶的名字、区域和服务地址"
                    ))),
                    _ => Err(TransportError::NotFound(key.to_string())),
                }
            }
            _ => Err(Self::fail(response, &format!("读取 {key}")).await),
        }
    }

    async fn put_upload(
        &self,
        key: &str,
        upload: Upload,
        payload_hash: &str,
    ) -> TransportResult<()> {
        let path = self.object_path(key)?;
        let request = self.request(Method::PUT, &path, "", payload_hash)?;
        let response = net::send_upload(request, upload).await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(Self::fail(response, &format!("写入 {key}")).await)
        }
    }
}

#[async_trait]
impl SyncTransport for S3Transport {
    async fn list(&self, prefix: &str) -> TransportResult<Vec<ObjectMeta>> {
        check_key(prefix)?;
        let is_dir = prefix.ends_with('/');
        let full_prefix = format!("{}{prefix}", self.prefix);
        let wanted = (!is_dir).then(|| prefix.rsplit('/').next().unwrap_or(prefix));
        let dir_len = full_prefix.rfind('/').map(|at| at + 1).unwrap_or(0);

        let mut out = Vec::new();
        let mut token: Option<String> = None;
        // 服务端不可信（也可能只是出了毛病）：页数、对象总数都设上限，翻页标记不许重复，
        // 免得它让本机没完没了地翻页，或者把内存占满。
        let mut walk = Paging::default();
        loop {
            let mut params = vec![
                ("list-type", "2"),
                ("delimiter", "/"),
                ("max-keys", LIST_PAGE_KEYS),
                ("prefix", full_prefix.as_str()),
            ];
            if let Some(token) = token.as_deref() {
                params.push(("continuation-token", token));
            }
            let query = canonical_query(&params);
            let request = self.request(Method::GET, &self.bucket_path, &query, EMPTY_SHA256)?;
            let response = net::send(request).await?;
            if response.status() != StatusCode::OK {
                return Err(Self::fail(response, &format!("列出 {prefix}")).await);
            }
            let body = net::read_xml(response, LIST_PAGE_MAX).await?;
            let page = parse_list(&String::from_utf8_lossy(&body), prefix)?;
            drop(body);
            walk.page(page.objects.len(), page.next_token.as_deref(), prefix)?;
            for (key, size, version, stable_version) in page.objects {
                let Some(name) = key.get(dir_len..) else {
                    continue;
                };
                if name.contains('/')
                    || !is_object_name(name)
                    || !key.starts_with(&full_prefix)
                    || wanted.is_some_and(|wanted| wanted != name)
                {
                    continue;
                }
                let stable_version = stable_version && !version.is_empty();
                out.push(ObjectMeta {
                    name: name.to_string(),
                    size,
                    version,
                    stable_version,
                });
            }
            token = page.next_token;
            if token.is_none() {
                return Ok(out);
            }
        }
    }

    async fn get(&self, key: &str, max: u64) -> TransportResult<Vec<u8>> {
        net::read_body(self.get_response(key).await?, key, max).await
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> TransportResult<()> {
        let hash = sha256_hex(&body);
        self.put_upload(key, Upload::Bytes(Bytes::from(body)), &hash)
            .await
    }

    async fn delete(&self, key: &str) -> TransportResult<()> {
        let path = self.object_path(key)?;
        let response = net::send(self.request(Method::DELETE, &path, "", EMPTY_SHA256)?).await?;
        let status = response.status();
        if status.is_success() || status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(Self::fail(response, &format!("删除 {key}")).await)
        }
    }

    async fn put_file(&self, key: &str, source: &Path) -> TransportResult<()> {
        // 签名要用到内容摘要：先过一遍文件。
        let path = source.to_path_buf();
        let (hash, _) = tokio::task::spawn_blocking(move || {
            let mut file = std::fs::File::open(&path)?;
            sha256_reader(&mut file)
        })
        .await
        .map_err(|e| TransportError::Other(format!("读取文件被中断: {e}")))?
        .map_err(|e| TransportError::Other(format!("读取 {} 失败: {e}", source.display())))?;
        self.put_upload(key, Upload::File(source.to_path_buf()), &ids::hex(&hash))
            .await
    }

    async fn get_file(&self, key: &str, dest: &Path, max: u64) -> TransportResult<()> {
        net::read_body_to_file(self.get_response(key).await?, key, dest, max).await
    }

    fn kind(&self) -> &'static str {
        "s3"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // AWS 文档《Signature Calculations for the Authorization Header》里的示例。
    const ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    #[test]
    fn signature_matches_documented_get_object_example() {
        let headers = [
            ("host", "examplebucket.s3.amazonaws.com"),
            ("range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
        ];
        let auth = authorization(
            &SignInput {
                method: "GET",
                path: "/test.txt",
                query: "",
                headers: &headers,
                payload_hash: EMPTY_SHA256,
                amz_date: "20130524T000000Z",
            },
            "us-east-1",
            ACCESS_KEY,
            SECRET_KEY,
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn signature_matches_documented_list_objects_example() {
        let headers = [
            ("host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
        ];
        let query = canonical_query(&[("prefix", "J"), ("max-keys", "2")]);
        assert_eq!(query, "max-keys=2&prefix=J");
        let auth = authorization(
            &SignInput {
                method: "GET",
                path: "/",
                query: &query,
                headers: &headers,
                payload_hash: EMPTY_SHA256,
                amz_date: "20130524T000000Z",
            },
            "us-east-1",
            ACCESS_KEY,
            SECRET_KEY,
        );
        assert!(
            auth.ends_with(
                "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
            ),
            "{auth}"
        );
    }

    #[test]
    fn query_encoding_follows_sigv4_rules() {
        assert_eq!(
            canonical_query(&[
                ("prefix", "a/b c~d"),
                ("delimiter", "/"),
                ("list-type", "2")
            ]),
            "delimiter=%2F&list-type=2&prefix=a%2Fb%20c~d"
        );
    }

    fn options() -> S3Options {
        S3Options {
            endpoint: "https://s3.example.com".into(),
            region: "us-east-1".into(),
            bucket: "my-bucket".into(),
            prefix: "rclaw/sync".into(),
            access_key: "ak".into(),
            secret_key: "sk".into(),
            path_style: false,
            allow_http: false,
        }
    }

    #[test]
    fn addressing_styles() {
        let hosted = S3Transport::new(&options()).unwrap();
        assert_eq!(hosted.origin, "https://my-bucket.s3.example.com");
        assert_eq!(
            hosted.object_path("heads/a").unwrap(),
            "/rclaw/sync/heads/a"
        );
        assert_eq!(hosted.bucket_path, "/");

        let mut path_style = options();
        path_style.path_style = true;
        path_style.endpoint = "http://192.168.1.5:9000".into();
        path_style.allow_http = true;
        let transport = S3Transport::new(&path_style).unwrap();
        assert_eq!(transport.origin, "http://192.168.1.5:9000");
        assert_eq!(transport.host, "192.168.1.5:9000");
        assert_eq!(
            transport.object_path("heads/a").unwrap(),
            "/my-bucket/rclaw/sync/heads/a"
        );
        assert!(transport.object_path("../x").is_err());
    }

    #[test]
    fn construction_applies_https_rule_and_validates() {
        let mut plain = options();
        plain.endpoint = "http://s3.example.com".into();
        assert!(matches!(
            S3Transport::new(&plain),
            Err(TransportError::Insecure(_))
        ));
        let mut bad_bucket = options();
        bad_bucket.bucket = "Bad_Bucket".into();
        assert!(S3Transport::new(&bad_bucket).is_err());
        let mut bad_prefix = options();
        bad_prefix.prefix = "a/../b".into();
        assert!(S3Transport::new(&bad_prefix).is_err());
        let mut ip_hosted = options();
        ip_hosted.endpoint = "https://10.0.0.5".into();
        assert!(S3Transport::new(&ip_hosted).is_err());
    }

    #[test]
    fn parses_list_result() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>my-bucket</Name><Prefix>rclaw/sync/heads/</Prefix><KeyCount>2</KeyCount>
  <IsTruncated>true</IsTruncated><NextContinuationToken>tok==</NextContinuationToken>
  <Contents><Key>rclaw/sync/heads/0123456789abcdef</Key><LastModified>2026-11-18T08:00:00.000Z</LastModified><ETag>&quot;e1&quot;</ETag><Size>321</Size></Contents>
  <Contents><Key>rclaw/sync/heads/fedcba9876543210</Key><Size>5</Size><LastModified>t</LastModified></Contents>
  <CommonPrefixes><Prefix>rclaw/sync/heads/sub/</Prefix></CommonPrefixes>
</ListBucketResult>"#;
        let page = parse_list(xml, "heads/").unwrap();
        assert_eq!(page.next_token.as_deref(), Some("tok=="));
        assert_eq!(
            page.objects,
            vec![
                (
                    "rclaw/sync/heads/0123456789abcdef".into(),
                    321,
                    "\"e1\"".into(),
                    true
                ),
                (
                    "rclaw/sync/heads/fedcba9876543210".into(),
                    5,
                    "t".into(),
                    false
                ),
            ]
        );
        assert!(parse_list("<a><b></a>", "heads/").is_err());
        // 没截断时给了标记也不翻页；说截断了却没有标记是错误，不当成列完了。
        let done = "<ListBucketResult><IsTruncated>false</IsTruncated><NextContinuationToken>t</NextContinuationToken></ListBucketResult>";
        assert_eq!(parse_list(done, "heads/").unwrap().next_token, None);
        let cut = "<ListBucketResult><IsTruncated>true</IsTruncated></ListBucketResult>";
        assert!(parse_list(cut, "heads/").is_err());
    }

    #[test]
    fn error_codes_are_taken_only_when_they_look_like_one() {
        assert_eq!(
            error_code("<Error><Code>SignatureDoesNotMatch</Code><Message>x</Message></Error>")
                .as_deref(),
            Some("SignatureDoesNotMatch")
        );
        // 只看第一个 `<Code>`；没有文本、带别的字符、太长的都不要。
        assert_eq!(error_code("<Error><Code/><Code>Later</Code></Error>"), None);
        assert_eq!(error_code("<Error><Message>x</Message></Error>"), None);
        assert_eq!(error_code("<Error><Code>请 点击 这里</Code></Error>"), None);
        assert_eq!(
            error_code(&format!("<Error><Code>{}</Code></Error>", "A".repeat(65))),
            None
        );
        assert_eq!(error_code("不是 XML"), None);
    }

    #[test]
    fn paging_is_bounded_and_never_goes_in_circles() {
        // 正常翻到头。
        let mut walk = Paging::default();
        walk.page(1000, Some("a"), "heads/").unwrap();
        walk.page(1000, Some("b"), "heads/").unwrap();
        walk.page(7, None, "heads/").unwrap();
        // 翻页标记重复：服务端在兜圈子。
        let mut walk = Paging::default();
        walk.page(1, Some("a"), "heads/").unwrap();
        walk.page(1, Some("b"), "heads/").unwrap();
        assert!(walk.page(1, Some("a"), "heads/").is_err());
        // 对象总数超了：不合规的名字也算在里面。
        let mut walk = Paging::default();
        walk.page(LIST_MAX, Some("a"), "heads/").unwrap();
        assert!(walk.page(1, None, "heads/").is_err());
        // 一页一页都是空的，标记各不相同：页数到了上限就停。
        let mut walk = Paging::default();
        let mut refused = None;
        for index in 0..LIST_PAGES_MAX + 1 {
            if walk.page(0, Some(&index.to_string()), "heads/").is_err() {
                refused = Some(index + 1);
                break;
            }
        }
        assert_eq!(refused, Some(LIST_PAGES_MAX));
    }

    #[test]
    fn a_page_with_an_absurd_number_of_objects_is_refused() {
        let entry = "<Contents><Key>a</Key><Size>1</Size></Contents>";
        let over = format!(
            "<ListBucketResult>{}</ListBucketResult>",
            entry.repeat(LIST_MAX + 1)
        );
        assert!(parse_list(&over, "heads/").is_err());
    }
}
