use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Tool, ToolResult};

use super::control::WebSearchControl;
use super::render::render_search_results;
use super::types::{
    BackendError, BackendId, SearchBackend, SearchRequest, MAX_QUERY_BYTES, MAX_RESULTS,
};

/// 单一搜索方式的直连工具。
///
/// 与聚合入口 `web_search` 共用同一后端实例和控制面，因此 `/search dis|en`
/// 以及额度阻塞、鉴权阻塞、临时熔断对两类入口同时生效。
pub struct ProviderSearchTool {
    backend: Arc<dyn SearchBackend>,
    control: Arc<WebSearchControl>,
    default_max_results: usize,
}

impl ProviderSearchTool {
    pub fn new(
        backend: Arc<dyn SearchBackend>,
        control: Arc<WebSearchControl>,
        default_max_results: usize,
    ) -> Result<Self, String> {
        let id = backend.id();
        if !control.is_configured(id) {
            return Err(format!("独立搜索工具对应的方式 {id} 未配置"));
        }
        if default_max_results == 0 || default_max_results > MAX_RESULTS {
            return Err(format!(
                "独立搜索工具 default_max_results 必须在 1..={MAX_RESULTS} 之间"
            ));
        }
        Ok(Self {
            backend,
            control,
            default_max_results,
        })
    }

    fn id(&self) -> BackendId {
        self.backend.id()
    }
}

#[async_trait]
impl Tool for ProviderSearchTool {
    fn name(&self) -> &str {
        match self.id() {
            BackendId::Zhipu => "zhipu_search",
            BackendId::Tavily => "tavily_search",
            BackendId::DoubaoCustom => "doubao_custom_search",
            BackendId::DoubaoGlobal => "doubao_global_search",
        }
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        match self.id() {
            BackendId::Zhipu => "Search the public web directly through Zhipu webSearchPrime.",
            BackendId::Tavily => {
                "Search the public web directly through Tavily with API-key rotation."
            }
            BackendId::DoubaoCustom => {
                "Search the public web directly through Doubao Custom Search."
            }
            BackendId::DoubaoGlobal => {
                "Search the public web directly through Doubao Global Search."
            }
        }
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query."
                },
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_RESULTS,
                    "description": format!(
                        "Maximum results requested. Provider limits still apply. Default {}.",
                        self.default_max_results
                    )
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(query) = input
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|query| !query.is_empty())
        else {
            return ToolResult::err(format!("{} 需要非空 query", self.name()));
        };
        if query.len() > MAX_QUERY_BYTES {
            return ToolResult::err(format!(
                "query 过长（{} 字节 > {MAX_QUERY_BYTES}）",
                query.len()
            ));
        }
        let max_results = match input.get("max_results") {
            None => self.default_max_results,
            Some(value) => match value.as_u64().and_then(|value| usize::try_from(value).ok()) {
                Some(value) if (1..=MAX_RESULTS).contains(&value) => value,
                _ => {
                    return ToolResult::err(format!("max_results 必须在 1..={MAX_RESULTS} 之间"));
                }
            },
        };
        if let Some(reason) = self.control.unavailable_reason(self.id()) {
            tracing::warn!(
                provider = self.id().as_str(),
                tool = self.name(),
                "独立搜索工具因控制面状态拒绝调用"
            );
            return ToolResult::err(reason);
        }

        let id = self.id();
        tracing::info!(
            provider = id.as_str(),
            tool = self.name(),
            query_bytes = query.len(),
            max_results,
            "独立搜索工具开始调用"
        );
        let request = SearchRequest {
            query: query.to_string(),
            max_results,
        };
        match self.backend.search(&request).await {
            Ok(mut response) if response.is_usable() => {
                self.control.record_success(id);
                response.hits.truncate(max_results);
                tracing::info!(
                    provider = id.as_str(),
                    tool = self.name(),
                    result_count = response.hits.len(),
                    request_id = response.request_id.as_deref().unwrap_or(""),
                    "独立搜索工具调用成功"
                );
                let summaries = response
                    .summary
                    .map(|summary| vec![(id, summary)])
                    .unwrap_or_default();
                ToolResult::ok(render_search_results(
                    query,
                    &response.hits,
                    &summaries,
                    &[],
                ))
            }
            Ok(_) => {
                let error = BackendError::empty(format!("{} 未返回可用搜索结果", id.label()));
                self.control.record_error(id, &error);
                tracing::warn!(
                    provider = id.as_str(),
                    tool = self.name(),
                    "独立搜索工具返回空结果"
                );
                ToolResult::err(error.concise())
            }
            Err(error) => {
                self.control.record_error(id, &error);
                let safe_error = rclaw_store::redact_sensitive_text(&error.concise());
                tracing::warn!(
                    provider = id.as_str(),
                    tool = self.name(),
                    error_kind = ?error.kind,
                    error = %safe_error,
                    "独立搜索工具调用失败"
                );
                ToolResult::err(safe_error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::types::{SearchHit, SearchResponse};
    use super::*;

    struct FakeBackend(BackendId);

    #[async_trait]
    impl SearchBackend for FakeBackend {
        fn id(&self) -> BackendId {
            self.0
        }

        async fn search(&self, request: &SearchRequest) -> Result<SearchResponse, BackendError> {
            Ok(SearchResponse {
                provider: self.id(),
                hits: vec![SearchHit {
                    title: request.query.clone(),
                    url: Some("https://example.com/".into()),
                    snippet: Some("ok".into()),
                    content: None,
                    provider: self.id(),
                    rank: 0,
                }],
                summary: None,
                request_id: None,
            })
        }
    }

    fn tool(id: BackendId) -> (ProviderSearchTool, Arc<WebSearchControl>) {
        let control = Arc::new(
            WebSearchControl::new(vec![id], &[(id, true)], true, 3, Duration::from_secs(60))
                .unwrap(),
        );
        (
            ProviderSearchTool::new(Arc::new(FakeBackend(id)), control.clone(), 5).unwrap(),
            control,
        )
    }

    #[tokio::test]
    async fn direct_tool_uses_provider_name_and_renders_results() {
        let (tool, _) = tool(BackendId::DoubaoCustom);
        assert_eq!(tool.name(), "doubao_custom_search");
        let result = tool.execute(json!({"query": "rust"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("provider: doubao-custom"));
    }

    #[test]
    fn all_provider_tool_names_are_stable_and_unique() {
        let names = BackendId::ALL
            .into_iter()
            .map(|id| tool(id).0.name().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "doubao_custom_search",
                "tavily_search",
                "zhipu_search",
                "doubao_global_search",
            ]
        );
        assert_eq!(
            names.iter().collect::<std::collections::HashSet<_>>().len(),
            names.len()
        );
    }

    #[tokio::test]
    async fn direct_tool_respects_manual_disable() {
        let (tool, control) = tool(BackendId::DoubaoCustom);
        control.set_enabled(BackendId::DoubaoCustom, false).unwrap();
        let result = tool.execute(json!({"query": "rust"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("/search en doubao-custom"));
    }
}
