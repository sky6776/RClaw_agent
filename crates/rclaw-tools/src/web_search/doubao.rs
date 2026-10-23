use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};

use super::types::{
    normalize_api_key, normalize_base_url, read_response_capped, safe_endpoint_label,
    validate_public_http_url, BackendError, BackendErrorKind, BackendId, SearchBackend, SearchHit,
    SearchRequest, SearchResponse,
};

const DEFAULT_CUSTOM_URL: &str = "https://open.feedcoopapi.com/search_api/web_search";
const DEFAULT_GLOBAL_URL: &str = "https://open.feedcoopapi.com/search_api/global_search";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoubaoVariant {
    Custom,
    Global,
}

impl DoubaoVariant {
    fn id(self) -> BackendId {
        match self {
            Self::Custom => BackendId::DoubaoCustom,
            Self::Global => BackendId::DoubaoGlobal,
        }
    }

    fn default_url(self) -> &'static str {
        match self {
            Self::Custom => DEFAULT_CUSTOM_URL,
            Self::Global => DEFAULT_GLOBAL_URL,
        }
    }

    fn max_results(self) -> usize {
        match self {
            Self::Custom => 50,
            Self::Global => 20,
        }
    }
}

#[derive(Clone)]
pub struct DoubaoSearchConfig {
    pub variant: DoubaoVariant,
    pub api_key: String,
    pub endpoint: String,
    pub timeout_secs: u64,
    pub max_results: usize,
    pub max_snippet_tokens: usize,
}

impl DoubaoSearchConfig {
    pub fn normalized(mut self) -> Result<Self, String> {
        self.api_key = normalize_api_key(self.variant.id().label(), self.api_key)?;
        self.endpoint = normalize_base_url(
            self.variant.id().label(),
            self.endpoint,
            self.variant.default_url(),
        )?;
        if !(1..=120).contains(&self.timeout_secs) {
            return Err(format!(
                "{} timeout_secs 必须在 1..=120 之间",
                self.variant.id()
            ));
        }
        if self.max_results == 0 || self.max_results > self.variant.max_results() {
            return Err(format!(
                "{} max_results 必须在 1..={} 之间",
                self.variant.id(),
                self.variant.max_results()
            ));
        }
        if self.max_snippet_tokens == 0 || self.max_snippet_tokens > 3_000 {
            return Err(format!(
                "{} max_snippet_tokens 必须在 1..=3000 之间",
                self.variant.id()
            ));
        }
        Ok(self)
    }
}

pub struct DoubaoBackend {
    cfg: DoubaoSearchConfig,
    client: reqwest::Client,
}

impl DoubaoBackend {
    pub fn new(cfg: DoubaoSearchConfig) -> Result<Self, String> {
        let cfg = cfg.normalized()?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .build()
            .map_err(|error| format!("创建 {} HTTP client 失败: {error}", cfg.variant.id()))?;
        Ok(Self { cfg, client })
    }

    async fn request(&self, body: Value) -> Result<Value, BackendError> {
        tracing::debug!(
            provider = self.cfg.variant.id().as_str(),
            endpoint = %safe_endpoint_label(&self.cfg.endpoint),
            "豆包搜索开始发送请求"
        );
        let response = self
            .client
            .post(&self.cfg.endpoint)
            .headers(auth_headers(&self.cfg.api_key)?)
            .json(&body)
            .send()
            .await
            .map_err(|error| BackendError::transient(format!("请求失败: {error}")))?;
        let status = response.status();
        let bytes = match read_response_capped(response, MAX_RESPONSE_BYTES).await {
            Ok(bytes) => bytes,
            Err(None) => {
                return Err(BackendError::permanent(format!(
                    "响应超过 {MAX_RESPONSE_BYTES} 字节"
                )))
            }
            Err(Some(error)) => {
                return Err(BackendError::transient(format!("读取响应失败: {error}")))
            }
        };
        if !status.is_success() {
            // 仅在出错时才把响应体转成文本：成功路径直接 from_slice 解析 JSON，
            // 避免每次成功调用都为（可达 8MB 的）响应体分配一份 lossy String。
            let text = String::from_utf8_lossy(&bytes);
            let message = format!("HTTP {}: {}", status.as_u16(), truncate_body(&text));
            let error = match status.as_u16() {
                401 | 403 => BackendError::auth(message),
                429 => BackendError::rate_limited(message),
                500..=599 => BackendError::transient(message),
                _ => BackendError::permanent(message),
            };
            tracing::warn!(
                provider = self.cfg.variant.id().as_str(),
                status = status.as_u16(),
                error_kind = ?error.kind,
                "豆包搜索返回非成功 HTTP 状态"
            );
            return Err(error);
        }
        let root: Value = serde_json::from_slice(&bytes).map_err(|error| {
            let text = String::from_utf8_lossy(&bytes);
            BackendError::permanent(format!(
                "响应不是合法 JSON: {error}: {}",
                truncate_body(&text)
            ))
        })?;
        if let Some((code, message)) = response_error(&root) {
            let error = classify_doubao_error(&code, &message);
            tracing::warn!(
                provider = self.cfg.variant.id().as_str(),
                error_code = %code,
                error_kind = ?error.kind,
                "豆包搜索返回业务错误"
            );
            return Err(error);
        }
        Ok(root)
    }

    fn custom_body(&self, request: &SearchRequest) -> Value {
        json!({
            "Query": request.query,
            "SearchType": "web",
            "Count": request.max_results.min(self.cfg.max_results),
            // NeedContent 的含义是“仅保留有正文的结果”，不是“返回正文”。
            // 为避免把只有高质量摘要的结果过滤掉，这里不强制正文；NeedUrl 则保证结果可引用。
            "Filter": {
                "NeedContent": false,
                "NeedUrl": true
            },
            "ContentFormats": "markdown"
        })
    }

    fn global_body(&self, request: &SearchRequest) -> Value {
        json!({
            "Query": request.query,
            "DocCount": request.max_results.min(self.cfg.max_results),
            "MaxSnippetLength": self.cfg.max_snippet_tokens
        })
    }
}

#[async_trait]
impl SearchBackend for DoubaoBackend {
    fn id(&self) -> BackendId {
        self.cfg.variant.id()
    }

    async fn search(&self, request: &SearchRequest) -> Result<SearchResponse, BackendError> {
        let body = match self.cfg.variant {
            DoubaoVariant::Custom => self.custom_body(request),
            DoubaoVariant::Global => self.global_body(request),
        };
        let root = self.request(body).await?;
        let request_id = get_path_string(&root, &["ResponseMetadata", "RequestId"]);
        let hits = match self.cfg.variant {
            DoubaoVariant::Custom => parse_custom_hits(&root),
            DoubaoVariant::Global => parse_global_hits(&root),
        };
        if hits.is_empty() {
            return Err(BackendError::empty(format!(
                "{} 未返回可用结果",
                self.id().label()
            )));
        }
        tracing::info!(
            provider = self.id().as_str(),
            result_count = hits.len(),
            request_id = request_id.as_deref().unwrap_or(""),
            "豆包搜索调用成功"
        );
        Ok(SearchResponse {
            provider: self.id(),
            hits,
            summary: None,
            request_id,
        })
    }
}

fn auth_headers(api_key: &str) -> Result<HeaderMap, BackendError> {
    let mut headers = HeaderMap::new();
    let auth = HeaderValue::from_str(&format!("Bearer {api_key}"))
        .map_err(|error| BackendError::invalid(format!("API Key 非法: {error}")))?;
    headers.insert(AUTHORIZATION, auth);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

fn parse_custom_hits(root: &Value) -> Vec<SearchHit> {
    get_path(root, &["Result", "WebResults"])
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(rank, item)| {
            let url = get_string(item, "Url")
                .or_else(|| get_string(item, "url"))
                .and_then(|url| validate_public_http_url(&url).ok());
            let title = get_string(item, "Title")
                .or_else(|| get_string(item, "title"))
                .unwrap_or_else(|| url.clone().unwrap_or_else(|| "(untitled)".into()));
            let snippet = get_string(item, "Summary")
                .or_else(|| get_string(item, "summary"))
                .or_else(|| get_string(item, "Snippet"))
                .or_else(|| get_string(item, "snippet"));
            let content = get_string(item, "Content").or_else(|| get_string(item, "content"));
            if url.is_none() && snippet.is_none() && content.is_none() {
                return None;
            }
            Some(SearchHit {
                title,
                url,
                snippet,
                content,
                provider: BackendId::DoubaoCustom,
                rank,
            })
        })
        .collect()
}

fn parse_global_hits(root: &Value) -> Vec<SearchHit> {
    get_path(root, &["Result", "Documents"])
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(rank, item)| {
            let url = get_string(item, "Url")
                .or_else(|| get_string(item, "url"))
                .and_then(|url| validate_public_http_url(&url).ok());
            let title = get_string(item, "Title")
                .or_else(|| get_string(item, "title"))
                .unwrap_or_else(|| url.clone().unwrap_or_else(|| "(untitled)".into()));
            let snippet = item
                .get("Snippet")
                .or_else(|| item.get("snippet"))
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter(|entry| {
                            get_string(entry, "Type")
                                .or_else(|| get_string(entry, "type"))
                                .is_none_or(|kind| kind.eq_ignore_ascii_case("text"))
                        })
                        .filter_map(|entry| {
                            get_string(entry, "Text").or_else(|| get_string(entry, "text"))
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|value| !value.trim().is_empty());
            if url.is_none() && snippet.is_none() {
                return None;
            }
            Some(SearchHit {
                title,
                url,
                snippet,
                content: None,
                provider: BackendId::DoubaoGlobal,
                rank,
            })
        })
        .collect()
}

fn response_error(root: &Value) -> Option<(String, String)> {
    if let Some(error) =
        get_path(root, &["ResponseMetadata", "Error"]).filter(|error| !error.is_null())
    {
        let code = error
            .get("Code")
            .or_else(|| error.get("CodeN"))
            .or_else(|| error.get("ErrorCode"))
            .and_then(value_as_string)
            .unwrap_or_else(|| "unknown".into());
        if code != "0" && !code.is_empty() {
            let message = error
                .get("Message")
                .or_else(|| error.get("ErrorMsg"))
                .and_then(value_as_string)
                .unwrap_or_else(|| "未知错误".into());
            return Some((code, message));
        }
    }

    // Global 版除统一 ResponseMetadata 外，还会在 Result 内返回业务错误码。
    let result = root.get("Result")?;
    let code = result.get("ErrorCode").and_then(value_as_string)?;
    if code == "0" || code.is_empty() {
        return None;
    }
    let message = result
        .get("ErrorMsg")
        .and_then(value_as_string)
        .unwrap_or_else(|| "未知错误".into());
    Some((code, message))
}

fn classify_doubao_error(code: &str, message: &str) -> BackendError {
    let rendered = format!("豆包错误 {code}: {message}");
    match code.trim() {
        "10400" | "10402" => BackendError::invalid(rendered),
        "10406" | "10412" => BackendError::quota(rendered),
        "10401" | "10403" | "10408" | "10409" | "10410" | "700901" => BackendError::auth(rendered),
        "700429" => BackendError::rate_limited(rendered),
        "10500" | "10501" => BackendError::transient(rendered),
        _ => BackendError::new(BackendErrorKind::Permanent, rendered),
    }
}

fn get_path<'a>(root: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(root, |value, key| value.get(*key))
}

fn get_path_string(root: &Value, path: &[&str]) -> Option<String> {
    get_path(root, path).and_then(value_as_string)
}

fn get_string(root: &Value, key: &str) -> Option<String> {
    root.get(key).and_then(value_as_string)
}

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn truncate_body(value: &str) -> String {
    super::types::truncate_chars(value, 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_request_uses_documented_filter_shape() {
        let backend = DoubaoBackend::new(DoubaoSearchConfig {
            variant: DoubaoVariant::Custom,
            api_key: "test-key".into(),
            endpoint: String::new(),
            timeout_secs: 8,
            max_results: 50,
            max_snippet_tokens: 1_000,
        })
        .unwrap();
        let body = backend.custom_body(&SearchRequest {
            query: "test".into(),
            max_results: 10,
        });
        assert!(body.get("NeedContent").is_none());
        assert_eq!(body["Filter"]["NeedContent"], false);
        assert_eq!(body["Filter"]["NeedUrl"], true);
        assert_eq!(body["ContentFormats"], "markdown");
    }

    #[test]
    fn parses_custom_content_and_summary() {
        let root = json!({
            "Result": {
                "WebResults": [{
                    "Title": "Example",
                    "Url": "https://example.com/a",
                    "Snippet": "short",
                    "Summary": "long",
                    "Content": "# body"
                }]
            }
        });
        let hits = parse_custom_hits(&root);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet.as_deref(), Some("long"));
        assert_eq!(hits[0].content.as_deref(), Some("# body"));
    }

    #[test]
    fn parses_global_text_snippets_only() {
        let root = json!({
            "Result": {
                "Documents": [{
                    "Title": "Example",
                    "Url": "https://example.com/a",
                    "Snippet": [
                        {"Type": "text", "Text": "first"},
                        {"Type": "image", "Image": {"ImageUrl": "https://example.com/a.jpg"}},
                        {"Type": "text", "Text": "second"}
                    ]
                }]
            }
        });
        let hits = parse_global_hits(&root);
        assert_eq!(hits[0].snippet.as_deref(), Some("first\nsecond"));
    }

    #[test]
    fn classifies_quota_and_rate_limit_errors() {
        assert_eq!(
            classify_doubao_error("10412", "quota").kind,
            BackendErrorKind::QuotaExhausted
        );
        assert_eq!(
            classify_doubao_error("700429", "limit").kind,
            BackendErrorKind::RateLimited
        );
    }

    #[test]
    fn detects_global_result_business_error() {
        let root = json!({
            "ResponseMetadata": {"RequestId": "request-id"},
            "Result": {
                "Documents": [],
                "ErrorCode": 10412,
                "ErrorMsg": "quota exhausted"
            }
        });
        assert_eq!(
            response_error(&root),
            Some(("10412".into(), "quota exhausted".into()))
        );
    }

    #[test]
    fn malformed_metadata_error_is_not_ignored() {
        let root = json!({
            "ResponseMetadata": {
                "Error": {"Message": "upstream rejected request"}
            },
            "Result": {"WebResults": []}
        });
        assert_eq!(
            response_error(&root),
            Some(("unknown".into(), "upstream rejected request".into()))
        );
    }
}
