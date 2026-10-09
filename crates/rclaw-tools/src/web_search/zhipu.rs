use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::json;
use tokio::sync::Mutex;

use crate::mcp::client::McpToolResult;
use crate::mcp::provider::render_tool_result;
use crate::mcp::McpClient;

use super::types::{
    normalize_api_key, normalize_base_url, safe_endpoint_label, validate_public_http_url,
    BackendError, BackendErrorKind, BackendId, ReadResponse, ReaderBackend, SearchBackend,
    SearchHit, SearchRequest, SearchResponse,
};

const DEFAULT_SEARCH_URL: &str = "https://open.bigmodel.cn/api/mcp/web_search_prime/mcp";
const DEFAULT_READER_URL: &str = "https://open.bigmodel.cn/api/mcp/web_reader/mcp";
const SEARCH_TOOL: &str = "web_search_prime";
const READER_TOOL: &str = "webReader";
const MAX_ATTEMPTS: usize = 2;

type ClientSlot = Arc<Mutex<Option<Arc<McpClient>>>>;

static REQUEST_GATE: OnceLock<Mutex<Option<tokio::time::Instant>>> = OnceLock::new();

#[derive(Clone)]
pub struct ZhipuSearchConfig {
    pub api_key: String,
    pub search_url: String,
    pub reader_url: String,
    pub startup_timeout_secs: u64,
    pub call_timeout_secs: u64,
}

impl ZhipuSearchConfig {
    pub fn normalized(mut self) -> Result<Self, String> {
        self.api_key = normalize_api_key("智谱", self.api_key)?;
        self.search_url =
            normalize_base_url("智谱 Search MCP", self.search_url, DEFAULT_SEARCH_URL)?;
        self.reader_url =
            normalize_base_url("智谱 Reader MCP", self.reader_url, DEFAULT_READER_URL)?;
        if !(1..=120).contains(&self.startup_timeout_secs) {
            return Err("zhipu startup_timeout_secs 必须在 1..=120 之间".into());
        }
        if !(1..=120).contains(&self.call_timeout_secs) {
            return Err("zhipu call_timeout_secs 必须在 1..=120 之间".into());
        }
        Ok(self)
    }
}

pub struct ZhipuBackend {
    cfg: ZhipuSearchConfig,
    search_slot: ClientSlot,
    reader_slot: ClientSlot,
}

impl ZhipuBackend {
    pub fn new(cfg: ZhipuSearchConfig) -> Result<Self, String> {
        Ok(Self {
            cfg: cfg.normalized()?,
            search_slot: Arc::new(Mutex::new(None)),
            reader_slot: Arc::new(Mutex::new(None)),
        })
    }

    fn auth_headers(&self) -> Option<HashMap<String, String>> {
        Some(HashMap::from([(
            "Authorization".into(),
            format!("Bearer {}", self.cfg.api_key),
        )]))
    }

    async fn client(
        &self,
        slot: &ClientSlot,
        name: &str,
        url: &str,
    ) -> Result<Arc<McpClient>, BackendError> {
        let mut guard = slot.lock().await;
        if let Some(client) = guard.as_ref() {
            if client.is_alive() {
                tracing::debug!(server = name, "复用已建立的智谱 MCP 连接");
                return Ok(Arc::clone(client));
            }
            tracing::warn!(server = name, "智谱 MCP 连接已失效，准备重建");
            *guard = None;
        }
        wait_for_slot().await;
        tracing::info!(
            server = name,
            endpoint = %safe_endpoint_label(url),
            "开始建立智谱 MCP 连接"
        );
        let client = McpClient::start_http(
            name,
            url,
            &self.auth_headers(),
            Duration::from_secs(self.cfg.startup_timeout_secs),
            Duration::from_secs(self.cfg.call_timeout_secs),
        )
        .await
        .map(Arc::new)
        .map_err(|error| classify_error(&format!("智谱 MCP({name}) 连接失败: {error}")))?;
        tracing::info!(server = name, "智谱 MCP 连接建立成功");
        *guard = Some(Arc::clone(&client));
        Ok(client)
    }

    async fn call_mcp(
        &self,
        slot: &ClientSlot,
        name: &str,
        url: &str,
        tool: &str,
        arguments: serde_json::Value,
    ) -> Result<McpToolResult, BackendError> {
        let mut last_error = None;
        for attempt in 0..MAX_ATTEMPTS {
            let client = match self.client(slot, name, url).await {
                Ok(client) => client,
                Err(error) => {
                    let retry = retryable(&error) && attempt + 1 < MAX_ATTEMPTS;
                    last_error = Some(error);
                    if retry {
                        tracing::warn!(
                            server = name,
                            attempt = attempt + 1,
                            "智谱 MCP 建连失败，短暂退避后重试"
                        );
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                    break;
                }
            };
            wait_for_slot().await;
            tracing::debug!(
                server = name,
                tool,
                attempt = attempt + 1,
                "开始调用智谱 MCP 工具"
            );
            match client.call_tool(tool, arguments.clone()).await {
                Ok(result) if !result.is_error => {
                    tracing::info!(server = name, tool, "智谱 MCP 工具调用成功");
                    return Ok(result);
                }
                Ok(result) => {
                    let error = classify_error(&render_tool_result(&result));
                    let retry = retryable(&error) && attempt + 1 < MAX_ATTEMPTS;
                    last_error = Some(error);
                    if retry {
                        tracing::warn!(
                            server = name,
                            tool,
                            attempt = attempt + 1,
                            "智谱 MCP 返回瞬态错误，短暂退避后重试"
                        );
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                    break;
                }
                Err(error) => {
                    if !client.is_alive() {
                        let mut guard = slot.lock().await;
                        if guard
                            .as_ref()
                            .is_some_and(|current| Arc::ptr_eq(current, &client))
                        {
                            *guard = None;
                        }
                    }
                    let error =
                        classify_error(&format!("智谱 MCP({name}) 调用 {tool} 失败: {error}"));
                    let retry = retryable(&error) && attempt + 1 < MAX_ATTEMPTS;
                    last_error = Some(error);
                    if retry {
                        tracing::warn!(
                            server = name,
                            tool,
                            attempt = attempt + 1,
                            "智谱 MCP 调用失败，短暂退避后重试"
                        );
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| BackendError::permanent("智谱 MCP 调用失败")))
    }
}

#[async_trait]
impl SearchBackend for ZhipuBackend {
    fn id(&self) -> BackendId {
        BackendId::Zhipu
    }

    async fn search(&self, request: &SearchRequest) -> Result<SearchResponse, BackendError> {
        let result = self
            .call_mcp(
                &self.search_slot,
                "zhipu-search",
                &self.cfg.search_url,
                SEARCH_TOOL,
                json!({"search_query": request.query}),
            )
            .await?;
        let rendered = render_tool_result(&result);
        if rendered.trim().is_empty() {
            return Err(BackendError::empty("智谱搜索返回空结果"));
        }
        let hits = extract_urls(&rendered)
            .into_iter()
            .take(request.max_results)
            .enumerate()
            .map(|(rank, url)| SearchHit {
                title: url.clone(),
                url: Some(url),
                snippet: None,
                content: None,
                provider: BackendId::Zhipu,
                rank,
            })
            .collect();
        Ok(SearchResponse {
            provider: BackendId::Zhipu,
            hits,
            summary: Some(rendered),
            request_id: None,
        })
    }
}

#[async_trait]
impl ReaderBackend for ZhipuBackend {
    fn id(&self) -> BackendId {
        BackendId::Zhipu
    }

    async fn read(&self, url: &str) -> Result<ReadResponse, BackendError> {
        let url = validate_public_http_url(url).map_err(BackendError::invalid)?;
        let result = self
            .call_mcp(
                &self.reader_slot,
                "zhipu-reader",
                &self.cfg.reader_url,
                READER_TOOL,
                json!({"url": url}),
            )
            .await?;
        let content = render_tool_result(&result);
        if content.trim().is_empty() {
            return Err(BackendError::empty("智谱 webReader 返回空正文"));
        }
        Ok(ReadResponse {
            provider: BackendId::Zhipu,
            content,
        })
    }
}

async fn wait_for_slot() {
    let gate = REQUEST_GATE.get_or_init(|| Mutex::new(None));
    let mut previous = gate.lock().await;
    if let Some(started) = *previous {
        let next = started + Duration::from_millis(250);
        let now = tokio::time::Instant::now();
        if next > now {
            tokio::time::sleep_until(next).await;
        }
    }
    *previous = Some(tokio::time::Instant::now());
}

fn extract_urls(text: &str) -> Vec<String> {
    static URL_RE: OnceLock<Regex> = OnceLock::new();
    let regex = URL_RE.get_or_init(|| Regex::new(r#"https?://[^\s<>"'`\]\[(){}]+"#).unwrap());
    let mut seen = std::collections::HashSet::new();
    regex
        .find_iter(text)
        .filter_map(|matched| {
            let value = matched
                .as_str()
                .trim_end_matches(['.', ',', ';', ':', '!', '?']);
            validate_public_http_url(value).ok()
        })
        .filter(|url| seen.insert(super::types::canonical_url_key(url)))
        .collect()
}

fn classify_error(message: &str) -> BackendError {
    let lower = message.to_ascii_lowercase();
    if [
        "401",
        "403",
        "unauthorized",
        "forbidden",
        "api key",
        "api_key",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return BackendError::auth(message);
    }
    if ["quota", "额度", "余额不足"]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return BackendError::quota(message);
    }
    if ["429", "1302", "rate limit", "请求频率"]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return BackendError::rate_limited(message);
    }
    if [
        "1234",
        "500",
        "502",
        "503",
        "504",
        "timeout",
        "timed out",
        "connection",
        "network",
        "超时",
        "连接",
        "网络",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return BackendError::transient(message);
    }
    BackendError::permanent(message)
}

fn retryable(error: &BackendError) -> bool {
    matches!(
        error.kind,
        BackendErrorKind::Transient | BackendErrorKind::RateLimited
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_deduplicates_public_urls() {
        let urls = extract_urls(
            "a https://example.com/a#x b https://example.com/a#y c http://127.0.0.1/x",
        );
        assert_eq!(urls, vec!["https://example.com/a"]);
    }

    #[test]
    fn classifies_rate_limit_before_generic_transient() {
        assert_eq!(
            classify_error("1302 rate limit").kind,
            super::super::types::BackendErrorKind::RateLimited
        );
    }

    #[test]
    fn only_transient_and_rate_limit_errors_are_retried() {
        assert!(retryable(&BackendError::transient("timeout")));
        assert!(retryable(&BackendError::rate_limited("429")));
        assert!(!retryable(&BackendError::auth("401")));
        assert!(!retryable(&BackendError::quota("quota")));
        assert!(!retryable(&BackendError::permanent("bad request")));
    }
}
