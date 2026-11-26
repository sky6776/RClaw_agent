//! DeepSeek Search / Fetch 后端（客户端直连）。
//!
//! 官方接口（Bearer 鉴权，与对话 API 同一把 key）：
//! - `POST {base}/v1/tools/search`：`{query, count, need_content}` → `{results:[...]}`
//! - `POST {base}/v1/tools/fetch`：`{url}` → `{title, content}`
//!
//! 计费按成功请求（失败 / 空结果 / 超时不计费）；`need_content=true` 时一次搜索即带
//! 正文，router 会跳过二次 fetch，省一次计费请求。

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};

use super::render::limit_output;
use super::types::{
    normalize_api_key, normalize_base_url, read_response_capped, validate_public_http_url,
    BackendError, BackendId, ReadResponse, ReaderBackend, SearchBackend, SearchHit, SearchRequest,
    SearchResponse, MAX_RESULTS,
};

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct DeepSeekSearchConfig {
    pub api_key: String,
    /// 服务根（不含 `/v1/tools/...`），默认 `https://api.deepseek.com`。
    pub base_url: String,
    pub timeout_secs: u64,
    pub max_results: usize,
    /// 搜索时是否直接返回正文（少一次 fetch 计费请求；响应体更大）。
    pub need_content: bool,
}

impl DeepSeekSearchConfig {
    pub fn normalized(mut self) -> Result<Self, String> {
        self.api_key = normalize_api_key("DeepSeek", self.api_key)?;
        self.base_url = normalize_base_url("DeepSeek", self.base_url, DEFAULT_BASE_URL)?;
        // endpoint 用字符串拼接 `/v1/tools/...`；带 query/fragment 的 base 会把路径拼进
        // 这些段里（请求打到错误位置，甚至把参数带给第三方）。启动期直接拒绝。
        let parsed = reqwest::Url::parse(&self.base_url)
            .map_err(|error| format!("DeepSeek URL 非法: {error}"))?;
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err("DeepSeek base_url 不允许包含 query 或 fragment".into());
        }
        if !(1..=120).contains(&self.timeout_secs) {
            return Err("DeepSeek timeout_secs 必须在 1..=120 之间".into());
        }
        if self.max_results == 0 || self.max_results > MAX_RESULTS {
            return Err(format!(
                "DeepSeek max_results 必须在 1..={MAX_RESULTS} 之间"
            ));
        }
        Ok(self)
    }
}

pub struct DeepSeekBackend {
    cfg: DeepSeekSearchConfig,
    client: reqwest::Client,
}

impl DeepSeekBackend {
    pub fn new(cfg: DeepSeekSearchConfig) -> Result<Self, String> {
        let cfg = cfg.normalized()?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            // API 请求不应依赖重定向；禁止跟随，避免把 Bearer 头带到跨源目标。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| format!("创建 DeepSeek HTTP client 失败: {error}"))?;
        Ok(Self { cfg, client })
    }

    fn headers(&self) -> Result<HeaderMap, BackendError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let auth = HeaderValue::from_str(&format!("Bearer {}", self.cfg.api_key))
            .map_err(|_| BackendError::auth("DeepSeek API key 含非法 header 字符"))?;
        let mut auth = auth;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        Ok(headers)
    }

    async fn post_json(&self, path: &str, body: Value) -> Result<Value, BackendError> {
        let url = format!("{}{path}", self.cfg.base_url.trim_end_matches('/'));
        let response = self
            .client
            .post(url)
            .headers(self.headers()?)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                // reqwest 的错误可能带完整端点路径；只保留受控类别。
                BackendError::transient(if error.is_timeout() {
                    "DeepSeek 请求超时"
                } else if error.is_connect() {
                    "DeepSeek 连接失败"
                } else if error.is_body() {
                    "DeepSeek 响应读取失败"
                } else {
                    "DeepSeek 请求失败"
                })
            })?;
        let status = response.status().as_u16();
        if !(200..=299).contains(&status) {
            // 不读取/不透出上游正文：其中可能含搜索词、页面内容或任意格式凭据。
            return Err(classify_status(status));
        }
        let body = read_response_capped(response, MAX_RESPONSE_BYTES)
            .await
            .map_err(|error| match error {
                // 不拼接 reqwest 错误（可能含完整端点 URL）。
                Some(_) => BackendError::transient("DeepSeek 响应读取失败（网络中断）"),
                None => BackendError::permanent("DeepSeek 响应体超过 8 MiB 上限"),
            })?;
        serde_json::from_slice(&body).map_err(|error| {
            BackendError::permanent(format!("DeepSeek 响应不是合法 JSON: {error}"))
        })
    }
}

/// 把 HTTP 状态映射为统一的搜索后端错误分类。
///
/// 两家文档都没有列错误码，按 HTTP 语义归类：鉴权、额度、限流、408/5xx/网络超时
/// 分别归类；**远端 400/422 归 Permanent 而不是 InvalidRequest**——`InvalidRequest`
/// 会让 router 直接中止整条 fallback 链，而远端的拒绝往往只影响这一个来源。
/// 本地入参问题（query 超长、URL 非法）在发请求前已由本地校验拒绝。
fn classify_status(status: u16) -> BackendError {
    let message = format!("DeepSeek 接口返回 {status}");
    match status {
        400 | 422 => BackendError::permanent(message),
        401 | 403 => BackendError::auth(message),
        402 => BackendError::quota(message),
        429 => BackendError::rate_limited(message),
        408 | 500..=599 => BackendError::transient(message),
        _ => BackendError::permanent(message),
    }
}

fn parse_search_response(root: &Value, limit: usize) -> Result<SearchResponse, BackendError> {
    let results = root
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| BackendError::permanent("DeepSeek 搜索响应缺少 results 数组"))?;
    let mut hits = Vec::new();
    for item in results {
        let Some(object) = item.as_object() else {
            continue;
        };
        // 没有可用公网 URL 的命中无法补读/引用；带凭据、内网或非 HTTP 的一律丢弃，
        // 绝不把原始 URL 交给渲染或其它 reader 后端。
        let Some(url) = object
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .and_then(|url| validate_public_http_url(url).ok())
        else {
            continue;
        };
        let title = object
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // snippet 是 100~200 字短摘要；缺失时退回 summary（600~800 字长摘要）。
        let snippet = object
            .get("snippet")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .or_else(|| {
                object
                    .get("summary")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
            })
            .map(str::to_string);
        let content = object
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string);
        hits.push(SearchHit {
            title,
            url: Some(url),
            snippet,
            content,
            provider: BackendId::DeepSeek,
            rank: hits.len() + 1,
        });
        if hits.len() >= limit {
            break;
        }
    }
    if hits.is_empty() {
        return Err(BackendError::empty("DeepSeek 搜索无有效结果"));
    }
    Ok(SearchResponse {
        provider: BackendId::DeepSeek,
        hits,
        summary: None,
        request_id: None,
    })
}

#[async_trait]
impl SearchBackend for DeepSeekBackend {
    fn id(&self) -> BackendId {
        BackendId::DeepSeek
    }

    async fn search(&self, request: &SearchRequest) -> Result<SearchResponse, BackendError> {
        let count = request
            .max_results
            .min(self.cfg.max_results)
            .clamp(1, MAX_RESULTS);
        let body = json!({
            "query": request.query,
            "count": count,
            "need_content": request.need_content && self.cfg.need_content,
        });
        let root = self.post_json("/v1/tools/search", body).await?;
        parse_search_response(&root, count)
    }
}

#[async_trait]
impl ReaderBackend for DeepSeekBackend {
    fn id(&self) -> BackendId {
        BackendId::DeepSeek
    }

    async fn read(&self, url: &str) -> Result<ReadResponse, BackendError> {
        let url = validate_public_http_url(url).map_err(BackendError::invalid)?;
        let root = self
            .post_json("/v1/tools/fetch", json!({ "url": url }))
            .await?;
        let content = root
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|content| !content.is_empty())
            .map(|content| limit_output(content.to_string()))
            .ok_or_else(|| BackendError::empty("DeepSeek 网页读取返回空正文"))?;
        Ok(ReadResponse {
            provider: BackendId::DeepSeek,
            content,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config(server: &MockServer) -> DeepSeekSearchConfig {
        DeepSeekSearchConfig {
            api_key: "sk-test".into(),
            base_url: server.uri(),
            timeout_secs: 2,
            max_results: 10,
            need_content: true,
        }
    }

    fn request() -> SearchRequest {
        SearchRequest {
            query: "rust async".into(),
            max_results: 5,
            need_content: true,
        }
    }

    #[tokio::test]
    async fn search_maps_full_schema_and_sends_need_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .and(header("authorization", "Bearer sk-test"))
            .and(wiremock::matchers::body_partial_json(json!({
                "query": "rust async",
                "count": 5,
                "need_content": true
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [{
                    "rerank_score": 1.8,
                    "title": "Rust async",
                    "url": "https://example.com/rust",
                    "snippet": "short",
                    "summary": "long summary",
                    "content": "# body",
                    "site_name": null,
                    "site_icon": null,
                    "date": null,
                    "authority": 2
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let backend = DeepSeekBackend::new(config(&server)).unwrap();
        let response = backend.search(&request()).await.unwrap();
        assert_eq!(response.provider, BackendId::DeepSeek);
        assert_eq!(response.hits.len(), 1);
        let hit = &response.hits[0];
        assert_eq!(hit.title, "Rust async");
        assert_eq!(hit.url.as_deref(), Some("https://example.com/rust"));
        assert_eq!(hit.snippet.as_deref(), Some("short"));
        assert_eq!(hit.content.as_deref(), Some("# body"));
        assert_eq!(hit.rank, 1);
    }

    #[tokio::test]
    async fn search_falls_back_to_summary_and_empty_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [{
                    "rerank_score": 0.0,
                    "title": "T",
                    "url": "https://example.com",
                    "snippet": "",
                    "summary": "long summary",
                    "content": "",
                    "site_name": null,
                    "site_icon": null,
                    "date": null,
                    "authority": -1
                }]
            })))
            .mount(&server)
            .await;
        let backend = DeepSeekBackend::new(config(&server)).unwrap();
        let response = backend.search(&request()).await.unwrap();
        assert_eq!(response.hits[0].snippet.as_deref(), Some("long summary"));
        assert!(response.hits[0].content.is_none());
    }

    #[tokio::test]
    async fn search_empty_results_is_empty_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": [] })))
            .mount(&server)
            .await;
        let backend = DeepSeekBackend::new(config(&server)).unwrap();
        let error = backend.search(&request()).await.unwrap_err();
        assert_eq!(
            error.kind,
            crate::web_search::types::BackendErrorKind::EmptyOrInsufficient
        );
    }

    #[tokio::test]
    async fn classifies_auth_quota_rate_and_transient() {
        for (status, expected) in [
            (
                401,
                crate::web_search::types::BackendErrorKind::Authentication,
            ),
            (
                402,
                crate::web_search::types::BackendErrorKind::QuotaExhausted,
            ),
            (429, crate::web_search::types::BackendErrorKind::RateLimited),
            (500, crate::web_search::types::BackendErrorKind::Transient),
            (400, crate::web_search::types::BackendErrorKind::Permanent),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/tools/search"))
                .respond_with(ResponseTemplate::new(status).set_body_string("boom"))
                .mount(&server)
                .await;
            let backend = DeepSeekBackend::new(config(&server)).unwrap();
            let error = backend.search(&request()).await.unwrap_err();
            assert_eq!(error.kind, expected, "status={status}");
        }
    }

    #[tokio::test]
    async fn read_returns_markdown_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/fetch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "title": "Example",
                "content": "# Example\n\nbody"
            })))
            .mount(&server)
            .await;
        let backend = DeepSeekBackend::new(config(&server)).unwrap();
        let response = backend.read("https://example.com/a").await.unwrap();
        assert_eq!(response.provider, BackendId::DeepSeek);
        assert!(response.content.contains("# Example"));
    }

    #[tokio::test]
    async fn read_empty_content_is_empty_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/fetch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "title": "",
                "content": "   "
            })))
            .mount(&server)
            .await;
        let backend = DeepSeekBackend::new(config(&server)).unwrap();
        let error = backend.read("https://example.com/a").await.unwrap_err();
        assert_eq!(
            error.kind,
            crate::web_search::types::BackendErrorKind::EmptyOrInsufficient
        );
    }

    #[test]
    fn config_rejects_empty_key_and_bad_timeout() {
        let mut cfg = DeepSeekSearchConfig {
            api_key: "  ".into(),
            base_url: DEFAULT_BASE_URL.into(),
            timeout_secs: 20,
            max_results: 10,
            need_content: true,
        };
        assert!(cfg.clone().normalized().is_err());
        cfg.api_key = "sk-test".into();
        cfg.timeout_secs = 0;
        assert!(cfg.normalized().is_err());
    }
}

#[cfg(test)]
mod extra_tests {
    use super::*;
    use crate::web_search::types::BackendErrorKind;

    fn hit(url: &str, title: &str) -> Value {
        json!({
            "rerank_score": 1.0, "title": title, "url": url,
            "snippet": "s", "summary": "sum", "content": "",
            "site_name": null, "site_icon": null, "date": null, "authority": 1
        })
    }

    #[test]
    fn drops_bad_items_and_caps_valid_hits() {
        let root = json!({"results": [
            42,
            {"title": "no url"},
            hit("http://127.0.0.1/private", "private"),
            hit("https://example.com/a", "a"),
            hit("https://example.com/b", "b"),
            hit("https://example.com/c", "c")
        ]});
        let response = parse_search_response(&root, 2).expect("至少两个有效命中");
        assert_eq!(response.hits.len(), 2);
        assert_eq!(
            response.hits[0].url.as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(response.hits[1].rank, 2);
    }

    #[test]
    fn all_invalid_results_is_empty_error() {
        let root = json!({"results": [null, {"title": "x"}]});
        let error = parse_search_response(&root, 5).unwrap_err();
        assert_eq!(error.kind, BackendErrorKind::EmptyOrInsufficient);
    }

    #[test]
    fn config_rejects_query_in_base_url() {
        let error = DeepSeekSearchConfig {
            api_key: "sk-test".into(),
            base_url: "https://api.deepseek.com/?redirect=evil".into(),
            timeout_secs: 20,
            max_results: 10,
            need_content: true,
        }
        .normalized()
        .err()
        .expect("带 query 的 base_url 必须被拒绝");
        assert!(error.contains("query"), "{error}");
    }
}
