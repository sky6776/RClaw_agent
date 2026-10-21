use std::collections::HashSet;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};

use crate::{Tool, ToolResult};

use super::control::WebSearchControl;
use super::render::limit_output;
use super::types::{
    normalize_api_key, normalize_base_url, read_response_capped, validate_public_http_url,
    BackendError, BackendId, ReadResponse, ReaderBackend, SearchBackend, SearchHit,
    SearchRequest, SearchResponse,
};

const DEFAULT_BASE_URL: &str = "https://api.tavily.com";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct TavilySearchConfig {
    pub api_keys: Vec<String>,
    pub base_url: String,
    pub timeout_secs: u64,
    pub max_results: usize,
    pub search_depth: String,
    pub include_answer: bool,
}

impl TavilySearchConfig {
    pub fn normalized(mut self) -> Result<Self, String> {
        let mut keys = Vec::new();
        let mut seen = HashSet::new();
        for key in self.api_keys {
            let key = normalize_api_key("Tavily", key)?;
            if seen.insert(key.clone()) {
                keys.push(key);
            }
        }
        if keys.is_empty() {
            return Err("Tavily api_keys 不能为空".into());
        }
        self.api_keys = keys;
        self.base_url = normalize_base_url("Tavily", self.base_url, DEFAULT_BASE_URL)?;
        if !(1..=120).contains(&self.timeout_secs) {
            return Err("Tavily timeout_secs 必须在 1..=120 之间".into());
        }
        if !(1..=20).contains(&self.max_results) {
            return Err("Tavily max_results 必须在 1..=20 之间".into());
        }
        self.search_depth = canonical_depth(&self.search_depth)
            .ok_or_else(|| "Tavily search_depth 支持 basic/advanced/fast/ultra-fast".to_string())?
            .to_string();
        Ok(self)
    }
}

pub struct TavilyClient {
    cfg: TavilySearchConfig,
    client: reqwest::Client,
    current: AtomicUsize,
}

impl TavilyClient {
    pub fn new(cfg: TavilySearchConfig) -> Result<Self, String> {
        let cfg = cfg.normalized()?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .build()
            .map_err(|error| format!("创建 Tavily HTTP client 失败: {error}"))?;
        Ok(Self {
            cfg,
            client,
            current: AtomicUsize::new(0),
        })
    }

    async fn post_json(&self, endpoint: &str, body: &Value) -> Result<Value, BackendError> {
        let total = self.cfg.api_keys.len();
        let start = self.current.load(Ordering::Relaxed) % total;
        let mut last_error = None;
        let mut attempted = 0usize;
        for offset in 0..total {
            let index = (start + offset) % total;
            attempted += 1;
            tracing::debug!(key_index = index, endpoint, "Tavily 开始调用 API");
            match self
                .post_json_with_key(endpoint, body, &self.cfg.api_keys[index])
                .await
            {
                Ok(root) => {
                    self.current.store(index, Ordering::Relaxed);
                    tracing::info!(
                        key_index = index,
                        endpoint,
                        "Tavily API 调用成功，当前 Key 已固定到健康索引"
                    );
                    return Ok(root);
                }
                Err(error) => {
                    let rotate = matches!(
                        error.kind,
                        super::types::BackendErrorKind::Authentication
                            | super::types::BackendErrorKind::QuotaExhausted
                            | super::types::BackendErrorKind::RateLimited
                    );
                    last_error = Some(error);
                    if !rotate {
                        break;
                    }
                    tracing::warn!(
                        failed_key_index = index,
                        endpoint,
                        "Tavily 当前 Key 不可用，尝试下一个 Key"
                    );
                }
            }
        }
        debug_assert!(attempted > 0, "normalized 配置保证至少有一个 Tavily Key");
        Err(last_error.unwrap_or_else(|| BackendError::permanent("Tavily 请求失败")))
    }

    async fn post_json_with_key(
        &self,
        endpoint: &str,
        body: &Value,
        api_key: &str,
    ) -> Result<Value, BackendError> {
        let response = self
            .client
            .post(format!("{}{endpoint}", self.cfg.base_url))
            .headers(headers(api_key)?)
            .json(body)
            .send()
            .await
            .map_err(|error| BackendError::transient(format!("Tavily 请求失败: {error}")))?;
        let status = response.status();
        let bytes = match read_response_capped(response, MAX_RESPONSE_BYTES).await {
            Ok(bytes) => bytes,
            Err(None) => return Err(BackendError::permanent("Tavily 响应过大")),
            Err(Some(error)) => {
                return Err(BackendError::transient(format!(
                    "Tavily 响应读取失败: {error}"
                )))
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        if !status.is_success() {
            let message = format!(
                "Tavily HTTP {}: {}",
                status.as_u16(),
                super::types::truncate_chars(&text, 1_000)
            );
            return Err(classify_http_error(status.as_u16(), &message));
        }
        serde_json::from_slice(&bytes).map_err(|error| {
            BackendError::permanent(format!(
                "Tavily 响应不是合法 JSON: {error}: {}",
                super::types::truncate_chars(&text, 1_000)
            ))
        })
    }

    async fn search_value(&self, body: &Value) -> Result<Value, BackendError> {
        self.post_json("/search", body).await
    }

    async fn extract_value(&self, urls: &[String]) -> Result<Value, BackendError> {
        self.post_json(
            "/extract",
            &json!({
                "urls": urls,
                "extract_depth": "advanced",
                "format": "markdown",
                "timeout": self.cfg.timeout_secs.min(60)
            }),
        )
        .await
    }
}

#[async_trait]
impl SearchBackend for TavilyClient {
    fn id(&self) -> BackendId {
        BackendId::Tavily
    }

    async fn search(&self, request: &SearchRequest) -> Result<SearchResponse, BackendError> {
        let root = self
            .search_value(&json!({
                "query": request.query,
                "search_depth": self.cfg.search_depth,
                "max_results": request.max_results.min(self.cfg.max_results),
                "include_answer": self.cfg.include_answer,
                "include_raw_content": false
            }))
            .await?;
        let hits = parse_search_hits(&root);
        let summary = root
            .get("answer")
            .and_then(Value::as_str)
            .filter(|answer| !answer.trim().is_empty())
            .map(str::to_string);
        if hits.is_empty() && summary.is_none() {
            return Err(BackendError::empty("Tavily 未返回可用搜索结果"));
        }
        Ok(SearchResponse {
            provider: BackendId::Tavily,
            hits,
            summary,
            request_id: root
                .get("request_id")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
}

#[async_trait]
impl ReaderBackend for TavilyClient {
    fn id(&self) -> BackendId {
        BackendId::Tavily
    }

    async fn read(&self, url: &str) -> Result<ReadResponse, BackendError> {
        let url = validate_public_http_url(url).map_err(BackendError::invalid)?;
        let root = self.extract_value(std::slice::from_ref(&url)).await?;
        let content = root
            .get("results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find_map(|item| {
                item.get("raw_content")
                    .or_else(|| item.get("content"))
                    .and_then(Value::as_str)
                    .filter(|content| !content.trim().is_empty())
                    .map(str::to_string)
            })
            .ok_or_else(|| extract_failure(&root))?;
        Ok(ReadResponse {
            provider: BackendId::Tavily,
            content,
        })
    }
}

pub struct TavilyExtract {
    client: Arc<TavilyClient>,
    control: Arc<WebSearchControl>,
}

impl TavilyExtract {
    pub fn from_client(
        client: Arc<TavilyClient>,
        control: Arc<WebSearchControl>,
    ) -> Result<Self, String> {
        if !control.is_configured(BackendId::Tavily) {
            return Err("tavily_extract 对应的 Tavily 搜索方式未配置".into());
        }
        Ok(Self { client, control })
    }
}

#[async_trait]
impl Tool for TavilyExtract {
    fn name(&self) -> &str {
        "tavily_extract"
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Extract readable Markdown from one or more public web URLs through Tavily."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "urls": {
                    "oneOf": [
                        {"type": "string"},
                        {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 5}
                    ]
                }
            },
            "required": ["urls"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        if let Some(reason) = self.control.unavailable_reason(BackendId::Tavily) {
            tracing::warn!("tavily_extract 因控制面状态拒绝调用");
            return ToolResult::err(reason);
        }
        let urls = match parse_urls(&input) {
            Ok(urls) => urls,
            Err(error) => return ToolResult::err(error),
        };
        match self.client.extract_value(&urls).await {
            Ok(root) => match render_extract(&root) {
                Ok(output) => {
                    self.control.record_success(BackendId::Tavily);
                    tracing::info!(url_count = urls.len(), "tavily_extract 网页正文提取成功");
                    ToolResult::ok(limit_output(output))
                }
                Err(error) => {
                    // HTTP 已 2xx，这里是逐 URL 的提取失败（failed_results 文案来自目标网页），
                    // 不能据此把 Tavily 置为 QuotaBlocked。
                    let error = error.demote_for_reader();
                    self.control.record_error(BackendId::Tavily, &error);
                    ToolResult::err(rclaw_store::redact_sensitive_text(&error.concise()))
                }
            },
            Err(error) => {
                self.control.record_error(BackendId::Tavily, &error);
                ToolResult::err(rclaw_store::redact_sensitive_text(&error.concise()))
            }
        }
    }
}

fn parse_search_hits(root: &Value) -> Vec<SearchHit> {
    root.get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(rank, item)| {
            let url = item
                .get("url")
                .and_then(Value::as_str)
                .and_then(|url| validate_public_http_url(url).ok());
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_else(|| url.as_deref().unwrap_or("(untitled)"))
                .to_string();
            let snippet = item
                .get("content")
                .and_then(Value::as_str)
                .filter(|content| !content.trim().is_empty())
                .map(str::to_string);
            if url.is_none() && snippet.is_none() {
                return None;
            }
            Some(SearchHit {
                title,
                url,
                snippet,
                content: None,
                provider: BackendId::Tavily,
                rank,
            })
        })
        .collect()
}

fn parse_urls(input: &Value) -> Result<Vec<String>, String> {
    let value = input
        .get("urls")
        .ok_or_else(|| "tavily_extract 需要 urls".to_string())?;
    let raw = match value {
        Value::String(url) => vec![url.clone()],
        Value::Array(urls) if !urls.is_empty() && urls.len() <= 5 => urls
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "urls 数组必须全为字符串".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err("urls 必须是字符串或包含 1..=5 个字符串的数组".into()),
    };
    raw.into_iter()
        .map(|url| validate_public_http_url(&url))
        .collect()
}

fn extract_failure(root: &Value) -> BackendError {
    let message = root
        .get("failed_results")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("error"))
        .and_then(Value::as_str)
        .unwrap_or("Tavily Extract 未返回正文");
    if message.to_ascii_lowercase().contains("limit") {
        BackendError::quota(message)
    } else {
        BackendError::empty(message)
    }
}

fn classify_http_error(status: u16, message: &str) -> BackendError {
    let lower = message.to_ascii_lowercase();
    match status {
        401 | 403 => BackendError::auth(message),
        429 | 432 | 433 if lower.contains("limit") || lower.contains("plan") => {
            BackendError::quota(message)
        }
        429 | 432 | 433 => BackendError::rate_limited(message),
        500..=599 => BackendError::transient(message),
        400..=499 => BackendError::permanent(message),
        _ => BackendError::transient(message),
    }
}

fn headers(api_key: &str) -> Result<HeaderMap, BackendError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|error| BackendError::invalid(format!("Tavily API Key 非法: {error}")))?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

fn canonical_depth(value: &str) -> Option<&str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "basic" => Some("basic"),
        "advanced" => Some("advanced"),
        "fast" => Some("fast"),
        "ultra-fast" | "ultra_fast" => Some("ultra-fast"),
        _ => None,
    }
}

fn render_extract(root: &Value) -> Result<String, BackendError> {
    let mut output = String::new();
    for item in root
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let url = item.get("url").and_then(Value::as_str).unwrap_or("unknown");
        let content = item
            .get("raw_content")
            .or_else(|| item.get("content"))
            .and_then(Value::as_str)
            .filter(|content| !content.trim().is_empty());
        let Some(content) = content else {
            continue;
        };
        output.push_str("url: ");
        output.push_str(url);
        output.push_str("\n\n");
        output.push_str(content);
        output.push_str("\n\n");
    }
    if output.is_empty() {
        return Err(extract_failure(root));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config(server: &MockServer) -> TavilySearchConfig {
        TavilySearchConfig {
            api_keys: vec!["bad".into(), "good".into()],
            base_url: server.uri(),
            timeout_secs: 2,
            max_results: 5,
            search_depth: "basic".into(),
            include_answer: false,
        }
    }

    #[tokio::test]
    async fn rotates_key_on_auth_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(header("authorization", "Bearer bad"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(header("authorization", "Bearer good"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [{"title": "ok", "url": "https://example.com", "content": "body"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = TavilyClient::new(config(&server)).unwrap();
        let response = client
            .search(&SearchRequest {
                query: "q".into(),
                max_results: 5,
            })
            .await
            .unwrap();
        assert_eq!(response.hits.len(), 1);
    }

    #[test]
    fn extract_failure_is_not_rendered_as_success() {
        let root = json!({
            "results": [],
            "failed_results": [{"url": "https://example.com", "error": "quota limit reached"}]
        });
        assert_eq!(
            render_extract(&root).unwrap_err().kind,
            super::super::types::BackendErrorKind::QuotaExhausted
        );
    }
}
