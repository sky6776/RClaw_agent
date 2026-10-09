use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::time::Instant;

use crate::{Tool, ToolResult};

use super::control::WebSearchControl;
use super::render::{merge_response, render_read_result, render_search_results};
use super::types::{
    safe_endpoint_label, validate_public_http_url, BackendError, BackendErrorKind, BackendId,
    ReaderBackend, SearchBackend, SearchRequest, MAX_QUERY_BYTES, MAX_READ_TOP, MAX_RESULTS,
};

#[derive(Clone, Debug)]
pub struct WebSearchConfig {
    pub read_top: usize,
    pub max_results: usize,
    pub total_timeout_secs: u64,
}

impl WebSearchConfig {
    pub fn normalized(mut self) -> Result<Self, String> {
        if self.read_top > MAX_READ_TOP {
            return Err(format!("web_search read_top 最大为 {MAX_READ_TOP}"));
        }
        if self.max_results == 0 || self.max_results > MAX_RESULTS {
            return Err(format!(
                "web_search max_results 必须在 1..={MAX_RESULTS} 之间"
            ));
        }
        if !(1..=120).contains(&self.total_timeout_secs) {
            return Err("web_search total_timeout_secs 必须在 1..=120 之间".into());
        }
        self.read_top = self.read_top.min(self.max_results);
        Ok(self)
    }
}

pub struct WebSearch {
    cfg: WebSearchConfig,
    control: Arc<WebSearchControl>,
    search_backends: HashMap<BackendId, Arc<dyn SearchBackend>>,
    reader_backends: HashMap<BackendId, Arc<dyn ReaderBackend>>,
}

impl WebSearch {
    pub fn new(
        cfg: WebSearchConfig,
        control: Arc<WebSearchControl>,
        search_backends: Vec<Arc<dyn SearchBackend>>,
        reader_backends: Vec<Arc<dyn ReaderBackend>>,
    ) -> Result<Self, String> {
        let cfg = cfg.normalized()?;
        let mut search_map = HashMap::new();
        for backend in search_backends {
            let id = backend.id();
            if search_map.insert(id, backend).is_some() {
                return Err(format!("重复注册搜索后端：{id}"));
            }
        }
        let mut reader_map = HashMap::new();
        for backend in reader_backends {
            let id = backend.id();
            if reader_map.insert(id, backend).is_some() {
                return Err(format!("重复注册网页读取后端：{id}"));
            }
        }
        if search_map.is_empty() {
            return Err("web_search 至少需要一个搜索后端".into());
        }
        for id in control.order() {
            if !search_map.contains_key(&id) {
                return Err(format!("已配置方式 {id} 没有对应搜索后端"));
            }
        }
        for id in search_map.keys().chain(reader_map.keys()) {
            if !control.is_configured(*id) {
                return Err(format!("后端 {id} 未在 web_search 控制面中配置"));
            }
        }
        Ok(Self {
            cfg,
            control,
            search_backends: search_map,
            reader_backends: reader_map,
        })
    }

    async fn execute_search(&self, input: &Value) -> ToolResult {
        let Some(query) = input
            .get("search_query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|query| !query.is_empty())
        else {
            return ToolResult::err("web_search 需要非空 search_query 或 url");
        };
        if query.len() > MAX_QUERY_BYTES {
            return ToolResult::err(format!(
                "search_query 过长（{} 字节 > {MAX_QUERY_BYTES}）",
                query.len()
            ));
        }
        let read_top = match parse_usize(input, "read_top", self.cfg.read_top, 0, MAX_READ_TOP) {
            Ok(value) => value,
            Err(error) => return ToolResult::err(error),
        };
        let max_results =
            match parse_usize(input, "max_results", self.cfg.max_results, 1, MAX_RESULTS) {
                Ok(value) => value,
                Err(error) => return ToolResult::err(error),
            };

        let deadline = Instant::now() + Duration::from_secs(self.cfg.total_timeout_secs);
        let mut ids = self.control.eligible_order();
        if ids.is_empty() {
            return ToolResult::err(
                "web_search 没有已启用且健康的搜索方式；请使用 /search en <方式>",
            );
        }
        if !self.control.fallback_enabled() {
            ids.truncate(1);
        }
        tracing::info!(
            query_bytes = query.len(),
            read_top,
            max_results,
            providers = ?ids.iter().map(|id| id.as_str()).collect::<Vec<_>>(),
            fallback = self.control.fallback_enabled(),
            total_timeout_secs = self.cfg.total_timeout_secs,
            "web_search 开始执行搜索路由"
        );

        let request = SearchRequest {
            query: query.to_string(),
            max_results,
        };
        let desired_hits = read_top.max(1).min(max_results);
        let mut hits = Vec::new();
        let mut seen_urls = HashSet::new();
        let mut summaries = Vec::new();
        let mut notices = Vec::new();
        let mut index = 0usize;
        let mut attempts = 0usize;

        while index < ids.len() {
            let id = ids[index];
            let Some(backend) = self.search_backends.get(&id) else {
                notices.push(format!("{id} 未注册，已跳过"));
                tracing::error!(provider = id.as_str(), "web_search 控制面存在未注册后端");
                index += 1;
                continue;
            };
            let provider_started = Instant::now();
            tracing::info!(
                provider = id.as_str(),
                route_index = index,
                "web_search 开始调用搜索方式"
            );
            attempts += 1;
            let result = self
                .call_search_with_retry(backend.as_ref(), &request, deadline)
                .await;
            match result {
                Ok(response) => {
                    let returned_hits = response.hits.len();
                    self.control.record_success(id);
                    let summary = merge_response(&mut hits, &mut seen_urls, response);
                    hits.truncate(max_results);
                    if let Some(summary) = summary.filter(|value| !value.trim().is_empty()) {
                        summaries.push((id, summary));
                    }
                    tracing::info!(
                        provider = id.as_str(),
                        returned_hits,
                        merged_hits = hits.len(),
                        elapsed_ms = provider_started.elapsed().as_millis(),
                        "web_search 搜索方式调用成功"
                    );
                    if hits.len() >= desired_hits
                        || (read_top == 0 && (!hits.is_empty() || !summaries.is_empty()))
                    {
                        break;
                    }
                    notices.push(format!(
                        "{} 结果不足，继续使用下一个搜索方式补足",
                        id.label()
                    ));
                    tracing::info!(
                        provider = id.as_str(),
                        merged_hits = hits.len(),
                        desired_hits,
                        "web_search 当前结果不足，触发 fallback 补足"
                    );
                }
                Err(error) => {
                    self.control.record_error(id, &error);
                    notices.push(format!("{} 搜索失败：{}", id.label(), safe_error(&error)));
                    if error.kind == BackendErrorKind::InvalidRequest {
                        if hits.is_empty() && summaries.is_empty() {
                            return ToolResult::err(notices.join("\n"));
                        }
                        // 后续 provider 的入参限制可能比主源更严格；已经取得的部分结果
                        // 不应因 fallback 的 InvalidRequest 被整体丢弃。
                        break;
                    }
                    if id == BackendId::DoubaoCustom
                        && matches!(
                            error.kind,
                            BackendErrorKind::RateLimited | BackendErrorKind::QuotaExhausted
                        )
                    {
                        prioritize_remaining(&mut ids, index + 1, BackendId::DoubaoGlobal);
                        tracing::info!(
                            provider = "doubao-global",
                            reason = ?error.kind,
                            "豆包 Custom 额度或限流异常，优先切到独立 QPS 的 Global 路线"
                        );
                    }
                }
            }
            if Instant::now() >= deadline {
                notices.push("web_search 已用完总时间预算".into());
                break;
            }
            index += 1;
        }

        if hits.is_empty() && summaries.is_empty() {
            tracing::error!(attempts, "web_search 所有可用搜索方式均未返回结果");
            return ToolResult::err(if notices.is_empty() {
                "所有搜索方式均未返回可用结果".into()
            } else {
                notices.join("\n")
            });
        }

        if read_top > 0 {
            let mut content_successes = hits
                .iter()
                .take(read_top)
                .filter(|hit| {
                    hit.content
                        .as_deref()
                        .is_some_and(|content| !content.trim().is_empty())
                })
                .count();
            for hit in hits.iter_mut().take(read_top) {
                if hit
                    .content
                    .as_deref()
                    .is_some_and(|content| !content.trim().is_empty())
                {
                    continue;
                }
                let Some(url) = hit.url.clone() else {
                    continue;
                };
                match self.read_url(&url, deadline).await {
                    Ok((provider, content, read_notices)) => {
                        hit.content = Some(content);
                        content_successes += 1;
                        notices.extend(read_notices);
                        tracing::debug!(
                            endpoint = %safe_endpoint_label(&url),
                            provider = %provider,
                            "web_search 正文补读成功"
                        );
                    }
                    Err(read_notices) => notices.extend(read_notices),
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
            if content_successes < read_top {
                notices.push(format!(
                    "正文读取不完整：期望 {read_top} 个，实际成功 {content_successes} 个；保留搜索摘要作为部分成功结果"
                ));
            }
        }

        tracing::info!(
            result_count = hits.len(),
            summary_count = summaries.len(),
            notice_count = notices.len(),
            "web_search 搜索路由完成"
        );
        ToolResult::ok(render_search_results(query, &hits, &summaries, &notices))
    }

    async fn execute_read(&self, raw_url: &str) -> ToolResult {
        let url = match validate_public_http_url(raw_url) {
            Ok(url) => url,
            Err(error) => return ToolResult::err(error),
        };
        let deadline = Instant::now() + Duration::from_secs(self.cfg.total_timeout_secs);
        tracing::info!(
            host = reqwest::Url::parse(&url)
                .ok()
                .and_then(|parsed| parsed.host_str().map(str::to_string))
                .unwrap_or_default(),
            "web_search 开始直接读取 URL"
        );
        match self.read_url(&url, deadline).await {
            Ok((provider, content, notices)) => {
                ToolResult::ok(render_read_result(&url, provider, &content, &notices))
            }
            Err(notices) => ToolResult::err(if notices.is_empty() {
                "没有已启用且健康的网页读取方式".into()
            } else {
                notices.join("\n")
            }),
        }
    }

    async fn call_search_with_retry(
        &self,
        backend: &dyn SearchBackend,
        request: &SearchRequest,
        deadline: Instant,
    ) -> Result<super::types::SearchResponse, BackendError> {
        let mut attempt = 0usize;
        loop {
            let remaining = remaining(deadline)?;
            let result = tokio::time::timeout(remaining, backend.search(request))
                .await
                .map_err(|_| BackendError::transient("搜索超过 web_search 总时间预算"))?;
            match result {
                Err(error)
                    if error.kind == BackendErrorKind::Transient
                        && attempt == 0
                        && Instant::now() < deadline =>
                {
                    attempt += 1;
                    tracing::warn!(
                        provider = backend.id().as_str(),
                        error = %safe_error(&error),
                        "web_search 搜索方式出现瞬态错误，执行一次快速重试"
                    );
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                result => return result,
            }
        }
    }

    async fn read_url(
        &self,
        url: &str,
        deadline: Instant,
    ) -> Result<(BackendId, String, Vec<String>), Vec<String>> {
        let mut ids = self.control.eligible_order();
        if !self.control.fallback_enabled() {
            ids.truncate(1);
        }
        ids.retain(|id| self.reader_backends.contains_key(id));
        let mut notices = Vec::new();
        for id in ids {
            let Some(backend) = self.reader_backends.get(&id) else {
                continue;
            };
            let reader_started = Instant::now();
            tracing::info!(provider = id.as_str(), "web_search 开始调用网页读取方式");
            let remaining = match remaining(deadline) {
                Ok(value) => value,
                Err(error) => {
                    notices.push(error.concise());
                    break;
                }
            };
            let result = tokio::time::timeout(remaining, backend.read(url)).await;
            match result {
                Ok(Ok(response)) if !response.content.trim().is_empty() => {
                    self.control.record_success(id);
                    tracing::info!(
                        provider = id.as_str(),
                        content_bytes = response.content.len(),
                        elapsed_ms = reader_started.elapsed().as_millis(),
                        "web_search 网页读取方式调用成功"
                    );
                    return Ok((id, response.content, notices));
                }
                Ok(Ok(_)) => {
                    let error = BackendError::empty("网页读取返回空正文");
                    self.control.record_error(id, &error);
                    notices.push(format!("{}：{}", id.label(), error.concise()));
                }
                Ok(Err(error)) => {
                    self.control.record_error(id, &error);
                    notices.push(format!("{}：{}", id.label(), safe_error(&error)));
                }
                Err(_) => {
                    let error = BackendError::transient("网页读取超过 web_search 总时间预算");
                    self.control.record_error(id, &error);
                    notices.push(format!("{}：{}", id.label(), error.concise()));
                    break;
                }
            }
        }
        Err(notices)
    }
}

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Search and read the public web through an ordered, health-aware provider pool. Supports Zhipu, Tavily multi-key, Doubao Custom, and Doubao Global with automatic fallback."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "search_query": {
                    "type": "string",
                    "description": "Search query. Ignored when url is provided."
                },
                "url": {
                    "type": "string",
                    "description": "A public HTTP(S) URL to read directly."
                },
                "read_top": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_READ_TOP,
                    "description": format!("Read/enrich the first N results. Default {}.", self.cfg.read_top)
                },
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_RESULTS,
                    "description": format!("Maximum search results requested. Default {}.", self.cfg.max_results)
                }
            }
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        if let Some(url) = input
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            self.execute_read(url).await
        } else {
            self.execute_search(&input).await
        }
    }
}

fn remaining(deadline: Instant) -> Result<Duration, BackendError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| BackendError::transient("web_search 已用完总时间预算"))
}

fn parse_usize(
    input: &Value,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, String> {
    let Some(value) = input.get(key) else {
        return Ok(default);
    };
    let Some(value) = value.as_u64().and_then(|value| usize::try_from(value).ok()) else {
        return Err(format!("{key} 必须是整数"));
    };
    if value < min || value > max {
        return Err(format!("{key} 必须在 {min}..={max} 之间"));
    }
    Ok(value)
}

fn prioritize_remaining(ids: &mut Vec<BackendId>, start: usize, wanted: BackendId) {
    if let Some(index) = ids
        .iter()
        .enumerate()
        .skip(start)
        .find_map(|(index, id)| (*id == wanted).then_some(index))
    {
        let id = ids.remove(index);
        ids.insert(start, id);
    }
}

fn safe_error(error: &BackendError) -> String {
    rclaw_store::redact_sensitive_text(&error.concise())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::web_search::types::{ReadResponse, SearchHit, SearchResponse};

    struct FakeSearch {
        id: BackendId,
        result: Mutex<Result<SearchResponse, BackendError>>,
        calls: Arc<Mutex<Vec<BackendId>>>,
    }

    #[async_trait]
    impl SearchBackend for FakeSearch {
        fn id(&self) -> BackendId {
            self.id
        }

        async fn search(&self, _: &SearchRequest) -> Result<SearchResponse, BackendError> {
            self.calls.lock().unwrap().push(self.id);
            self.result.lock().unwrap().clone()
        }
    }

    struct FakeReader {
        id: BackendId,
        content: String,
    }

    #[async_trait]
    impl ReaderBackend for FakeReader {
        fn id(&self) -> BackendId {
            self.id
        }

        async fn read(&self, _: &str) -> Result<ReadResponse, BackendError> {
            Ok(ReadResponse {
                provider: self.id,
                content: self.content.clone(),
            })
        }
    }

    fn success(id: BackendId, text: &str) -> SearchResponse {
        SearchResponse {
            provider: id,
            hits: vec![SearchHit {
                title: text.into(),
                url: Some(format!("https://example.com/{}", id.as_str())),
                snippet: Some(text.into()),
                content: None,
                provider: id,
                rank: 0,
            }],
            summary: None,
            request_id: None,
        }
    }

    fn make_tool(
        order: Vec<BackendId>,
        enabled: Vec<(BackendId, bool)>,
        search_backends: Vec<Arc<dyn SearchBackend>>,
        readers: Vec<Arc<dyn ReaderBackend>>,
    ) -> WebSearch {
        let control = Arc::new(
            WebSearchControl::new(order, &enabled, true, 3, Duration::from_secs(60)).unwrap(),
        );
        WebSearch::new(
            WebSearchConfig {
                read_top: 0,
                max_results: 5,
                total_timeout_secs: 2,
            },
            control,
            search_backends,
            readers,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn disabled_backend_is_skipped_without_reordering() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let custom = Arc::new(FakeSearch {
            id: BackendId::DoubaoCustom,
            result: Mutex::new(Ok(success(BackendId::DoubaoCustom, "custom"))),
            calls: calls.clone(),
        });
        let tavily = Arc::new(FakeSearch {
            id: BackendId::Tavily,
            result: Mutex::new(Ok(success(BackendId::Tavily, "tavily"))),
            calls: calls.clone(),
        });
        let tool = make_tool(
            vec![BackendId::DoubaoCustom, BackendId::Tavily],
            vec![(BackendId::DoubaoCustom, false), (BackendId::Tavily, true)],
            vec![custom, tavily],
            vec![],
        );
        let result = tool.execute(json!({"search_query": "q"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(*calls.lock().unwrap(), vec![BackendId::Tavily]);
    }

    #[tokio::test]
    async fn custom_rate_limit_prefers_global_independent_qps_lane() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fake = |id, result| {
            Arc::new(FakeSearch {
                id,
                result: Mutex::new(result),
                calls: calls.clone(),
            }) as Arc<dyn SearchBackend>
        };
        let tool = make_tool(
            vec![
                BackendId::DoubaoCustom,
                BackendId::Tavily,
                BackendId::DoubaoGlobal,
            ],
            vec![
                (BackendId::DoubaoCustom, true),
                (BackendId::Tavily, true),
                (BackendId::DoubaoGlobal, true),
            ],
            vec![
                fake(
                    BackendId::DoubaoCustom,
                    Err(BackendError::rate_limited("700429")),
                ),
                fake(BackendId::Tavily, Ok(success(BackendId::Tavily, "tavily"))),
                fake(
                    BackendId::DoubaoGlobal,
                    Ok(success(BackendId::DoubaoGlobal, "global")),
                ),
            ],
            vec![],
        );
        let result = tool.execute(json!({"search_query": "q"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![BackendId::DoubaoCustom, BackendId::DoubaoGlobal]
        );
    }

    #[tokio::test]
    async fn snippet_only_search_is_partial_success_when_reader_missing() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let global = Arc::new(FakeSearch {
            id: BackendId::DoubaoGlobal,
            result: Mutex::new(Ok(success(BackendId::DoubaoGlobal, "snippet"))),
            calls,
        });
        let mut tool = make_tool(
            vec![BackendId::DoubaoGlobal],
            vec![(BackendId::DoubaoGlobal, true)],
            vec![global],
            vec![],
        );
        tool.cfg.read_top = 1;
        let result = tool.execute(json!({"search_query": "q"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("部分成功结果"));
    }

    #[tokio::test]
    async fn direct_url_uses_reader_capability_only() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let tavily_search = Arc::new(FakeSearch {
            id: BackendId::Tavily,
            result: Mutex::new(Ok(success(BackendId::Tavily, "unused"))),
            calls,
        });
        let reader = Arc::new(FakeReader {
            id: BackendId::Tavily,
            content: "full text".into(),
        });
        let tool = make_tool(
            vec![BackendId::Tavily],
            vec![(BackendId::Tavily, true)],
            vec![tavily_search],
            vec![reader],
        );
        let result = tool.execute(json!({"url": "https://example.com/a"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("full text"));
    }

    #[tokio::test]
    async fn fallback_off_does_not_skip_primary_to_find_secondary_reader() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let custom = Arc::new(FakeSearch {
            id: BackendId::DoubaoCustom,
            result: Mutex::new(Ok(success(BackendId::DoubaoCustom, "unused"))),
            calls: calls.clone(),
        });
        let tavily = Arc::new(FakeSearch {
            id: BackendId::Tavily,
            result: Mutex::new(Ok(success(BackendId::Tavily, "unused"))),
            calls,
        });
        let reader = Arc::new(FakeReader {
            id: BackendId::Tavily,
            content: "secondary text".into(),
        });
        let tool = make_tool(
            vec![BackendId::DoubaoCustom, BackendId::Tavily],
            vec![(BackendId::DoubaoCustom, true), (BackendId::Tavily, true)],
            vec![custom, tavily],
            vec![reader],
        );
        tool.control.set_fallback_enabled(false);

        let result = tool.execute(json!({"url": "https://example.com/a"})).await;
        assert!(
            result.is_error,
            "关闭 fallback 后不应绕过主源调用次级 reader"
        );
        assert!(result.content.contains("没有已启用且健康的网页读取方式"));
    }

    #[tokio::test]
    async fn fallback_invalid_request_does_not_discard_existing_partial_result() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fake = |id, result| {
            Arc::new(FakeSearch {
                id,
                result: Mutex::new(result),
                calls: calls.clone(),
            }) as Arc<dyn SearchBackend>
        };
        let mut tool = make_tool(
            vec![BackendId::DoubaoCustom, BackendId::Tavily],
            vec![(BackendId::DoubaoCustom, true), (BackendId::Tavily, true)],
            vec![
                fake(
                    BackendId::DoubaoCustom,
                    Ok(success(BackendId::DoubaoCustom, "partial")),
                ),
                fake(
                    BackendId::Tavily,
                    Err(BackendError::invalid("provider-specific query limit")),
                ),
            ],
            vec![],
        );
        tool.cfg.read_top = 2;

        let result = tool.execute(json!({"search_query": "q"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("partial"));
        assert!(result.content.contains("provider-specific query limit"));
    }
}
