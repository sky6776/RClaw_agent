//! Rerank 模块 (P3-016~P3-021, P4-003~P4-005)
//!
//! 模型 rerank 优先策略 + 本地 fallback。
//! P3-018: chat/completions rerank fallback adapter — 当无专用 rerank API 时，
//! 使用通用 LLM chat/completions 接口对候选文档进行相关性评分。

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// P3-018: Module-level HTTP client reuse (same pattern as expansion.rs)
// ---------------------------------------------------------------------------

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn get_http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|e| {
                tracing::warn!("rerank HTTP client 构建失败，使用默认配置: {}", e);
                reqwest::Client::new()
            })
    })
}

/// Rerank 配置
#[derive(Debug, Clone)]
pub struct RerankConfig {
    pub model_rerank_enabled: bool,
    pub rerank_provider: String,
    pub rerank_model: String,
    pub rerank_timeout_ms: u64,
    pub rerank_max_candidates: usize,
}

impl Default for RerankConfig {
    fn default() -> Self {
        Self {
            model_rerank_enabled: true,
            rerank_provider: String::new(),
            rerank_model: "gpt-4o-mini".into(),
            rerank_timeout_ms: 5000,
            rerank_max_candidates: 50,
        }
    }
}

/// Fallback 原因
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    NotConfigured,
    Timeout,
    ApiError,
    BudgetExceeded,
    PrivacyBlocked,
}

impl FallbackReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Timeout => "timeout",
            Self::ApiError => "api_error",
            Self::BudgetExceeded => "budget_exceeded",
            Self::PrivacyBlocked => "privacy_blocked",
        }
    }
}

/// Rerank 结果
#[derive(Debug, Clone)]
pub struct RerankResult {
    pub model_rerank_attempted: bool,
    pub model_rerank_succeeded: bool,
    pub fallback_used: bool,
    pub fallback_reason: Option<FallbackReason>,
    pub provider: String,
    pub candidates_reranked: usize,
}

/// 本地 rerank 信号
#[derive(Debug, Clone)]
pub struct LocalRankSignals {
    pub fts_score: f64,
    pub vector_score: f64,
    pub title_score: f64,
    pub summary_score: f64,
    pub table_score: f64,
    pub metadata_score: f64,
    pub freshness_score: f64,
    pub granularity_score: f64,
    pub exact_match_score: f64,
}

impl Default for LocalRankSignals {
    fn default() -> Self {
        Self {
            fts_score: 0.0,
            vector_score: 0.0,
            title_score: 0.0,
            summary_score: 0.0,
            table_score: 0.0,
            metadata_score: 0.0,
            freshness_score: 0.0,
            granularity_score: 0.0,
            exact_match_score: 0.0,
        }
    }
}

/// 使用本地信号加权排序
pub fn local_rerank(candidates: &[(i64, LocalRankSignals)], weights: &[f64]) -> Vec<(i64, f64)> {
    let mut scored: Vec<(i64, f64)> = candidates
        .iter()
        .map(|(doc_id, signals)| {
            let score = signals.fts_score * weights.first().copied().unwrap_or(0.25)
                + signals.vector_score * weights.get(1).copied().unwrap_or(0.25)
                + signals.title_score * weights.get(2).copied().unwrap_or(0.15)
                + signals.exact_match_score * weights.get(3).copied().unwrap_or(0.05)
                + signals.metadata_score * weights.get(4).copied().unwrap_or(0.05)
                + signals.summary_score * weights.get(5).copied().unwrap_or(0.05)
                + signals.table_score * weights.get(6).copied().unwrap_or(0.05)
                + signals.freshness_score * weights.get(7).copied().unwrap_or(0.1);
            (*doc_id, score)
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

/// Reciprocal Rank Fusion (k=60)
pub fn rrf_merge(results_lists: &[Vec<(i64, f64)>], k: f64) -> Vec<(i64, f64)> {
    let mut scores: std::collections::HashMap<i64, f64> = std::collections::HashMap::new();
    for results in results_lists {
        for (rank, (doc_id, _)) in results.iter().enumerate() {
            *scores.entry(*doc_id).or_insert(0.0) += 1.0 / (k + (rank + 1) as f64);
        }
    }
    let mut merged: Vec<(i64, f64)> = scores.into_iter().collect();
    merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    merged
}

// ---------------------------------------------------------------------------
// P3-018: Chat/Completions Rerank Adapter
// ---------------------------------------------------------------------------

/// Chat/completions based reranker configuration.
///
/// Uses a generic LLM chat/completions API to score document relevance
/// when no dedicated rerank API is available.
#[derive(Debug, Clone)]
pub struct ChatCompletionsReranker {
    /// Base URL for the chat/completions endpoint (e.g. "https://api.openai.com/v1")
    pub base_url: String,
    /// API key for authentication
    pub api_key: String,
    /// Model name (e.g. "gpt-4o-mini")
    pub model: String,
    /// Request timeout in milliseconds
    pub timeout_ms: u64,
    /// Maximum number of candidates to send for reranking
    pub max_candidates: usize,
}

impl ChatCompletionsReranker {
    /// Build from RerankConfig, falling back to the provided API credentials.
    pub fn from_config(config: &RerankConfig, base_url: &str, api_key: &str) -> Self {
        Self {
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            model: config.rerank_model.clone(),
            timeout_ms: config.rerank_timeout_ms,
            max_candidates: config.rerank_max_candidates,
        }
    }
}

/// A single candidate document for chat/completions reranking.
#[derive(Debug, Clone)]
pub struct RerankCandidate {
    pub doc_id: i64,
    pub text: String,
}

/// Parse a relevance score from the model's text response.
///
/// The model is asked to return a number 0-100. This function extracts the
/// first numeric value found in the response. Returns `None` if no valid
/// number can be parsed.
fn parse_score(raw: &str) -> Option<f64> {
    // Try to find the first number in the response
    for token in raw.split_whitespace() {
        // Strip trailing punctuation that LLMs sometimes add (e.g. "85.", "90,")
        let cleaned = token.trim_end_matches(['.', ',', ';', ':']);
        if let Ok(score) = cleaned.parse::<f64>() {
            // Clamp to 0-100 range
            if (0.0..=100.0).contains(&score) {
                return Some(score);
            }
            // If out of range but positive, clamp it
            if score > 100.0 {
                return Some(100.0);
            }
            if score < 0.0 {
                return Some(0.0);
            }
        }
    }
    None
}

/// Relevance scoring prompt for a single document.
#[allow(dead_code)]
fn build_rerank_prompt(query: &str, doc_text: &str) -> String {
    format!(
        "Evaluate the relevance of the following document to the query. \
         Score 0-100.\n\nQuery: {}\n\nDocument: {}\n\nRelevance score (0-100):",
        query, doc_text
    )
}

/// Use a chat/completions API to score candidate documents for relevance.
///
/// Sends candidates in batches (respecting `max_candidates` limit).
/// Returns scored candidates with scores normalized to [0.0, 1.0].
/// On any failure (timeout, API error, unparseable response), returns `None`
/// so the caller can fall back to local_rerank.
pub async fn chat_completions_rerank(
    reranker: &ChatCompletionsReranker,
    query: &str,
    candidates: &[RerankCandidate],
    budget: Option<&crate::kb::cost::TokenBudget>,
) -> Option<Vec<(i64, f64)>> {
    if reranker.api_key.is_empty() || reranker.base_url.is_empty() {
        debug!("chat_completions_rerank: no API key or base URL configured");
        return None;
    }

    if candidates.is_empty() {
        // 无候选文本不应记为成功，返回 None 触发 fallback
        return None;
    }

    // Budget guard: estimate ~100 tokens per candidate for the prompt
    let estimated_tokens = (candidates.len() as u64) * 100;
    if let Some(b) = budget {
        if !b.try_consume(estimated_tokens) {
            warn!(
                "chat_completions_rerank: budget exceeded (estimated {} tokens, remaining {})",
                estimated_tokens,
                b.remaining()
            );
            return None;
        }
    }

    // Limit candidates to max_candidates
    let limited: Vec<&RerankCandidate> = candidates.iter().take(reranker.max_candidates).collect();

    let client = get_http_client();
    let url = format!(
        "{}/chat/completions",
        reranker.base_url.trim_end_matches('/')
    );

    let mut scored: Vec<(i64, f64)> = Vec::with_capacity(limited.len());

    // Process in batches — each candidate gets its own scoring call so we can
    // parse individual scores reliably. For efficiency we batch up to 10
    // candidates per single API call using a structured prompt.
    let batch_size = 10;
    for chunk in limited.chunks(batch_size) {
        // Build a single prompt that asks the model to score all documents
        let mut doc_section = String::new();
        for (i, c) in chunk.iter().enumerate() {
            // Truncate document text to avoid excessive token usage
            let truncated = if c.text.len() > 800 {
                let mut end = 800;
                while !c.text.is_char_boundary(end) {
                    end -= 1;
                }
                &c.text[..end]
            } else {
                &c.text
            };
            doc_section.push_str(&format!("[{}] {}\n", i + 1, truncated));
        }

        let user_content = format!(
            "Evaluate the relevance of each document to the query. \
             For each document, provide a relevance score from 0 to 100.\n\n\
             Query: {}\n\nDocuments:\n{}\n\
             Respond with ONLY the scores in this exact format: [score1, score2, ...]\n\
             Example: [85, 42, 91]",
            query, doc_section
        );

        let mut body = serde_json::json!({
            "model": reranker.model,
            "max_tokens": 128,
            "temperature": 0.0,
            "stream": false,
            "messages": [
                { "role": "system", "content": "You are a relevance scoring assistant. Score documents 0-100 based on their relevance to a query. Respond with only the score array." },
                { "role": "user", "content": user_content }
            ]
        });
        crate::llm::apply_deepseek_chat_options(&mut body, &reranker.base_url, &reranker.model);

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(reranker.timeout_ms),
            client
                .post(&url)
                .header("Authorization", format!("Bearer {}", reranker.api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send(),
        )
        .await;

        match result {
            Ok(Ok(resp)) => {
                if !resp.status().is_success() {
                    warn!(
                        status = %resp.status(),
                        "chat_completions_rerank: API returned non-success status"
                    );
                    // 任意批次失败即整体失败，避免部分结果被审计为成功
                    return None;
                }

                match resp.json::<serde_json::Value>().await {
                    Ok(data) => {
                        if let Some(reason) = crate::llm::terminal_finish_reason(&data) {
                            warn!(
                                finish_reason = reason,
                                "chat_completions_rerank: generation ended before a usable score array"
                            );
                            return None;
                        }
                        let batch_scores = extract_scores_from_response(&data, chunk.len());
                        match batch_scores {
                            Some(scores) => {
                                for (c, score) in chunk.iter().zip(scores.iter()) {
                                    // Normalize 0-100 to 0.0-1.0
                                    scored.push((c.doc_id, *score / 100.0));
                                }
                            }
                            None => {
                                warn!("chat_completions_rerank: failed to parse scores from response, falling back to per-doc parsing");
                                // Try per-document fallback parsing from the raw content
                                if let Some(raw) = extract_raw_content(&data) {
                                    let parsed = parse_score(&raw);
                                    if parsed.is_none() {
                                        warn!("chat_completions_rerank: unparseable response, aborting");
                                        return None;
                                    }
                                }
                                return None;
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "chat_completions_rerank: failed to parse response JSON");
                        return None;
                    }
                }
            }
            Ok(Err(e)) => {
                warn!(error = %e, "chat_completions_rerank: HTTP request failed");
                return None;
            }
            Err(_) => {
                warn!("chat_completions_rerank: request timed out");
                return None;
            }
        }
    }

    if scored.is_empty() {
        return None;
    }

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Some(scored)
}

/// Extract scores from a chat/completions response.
///
/// Expects the model to return a JSON array like [85, 42, 91] or
/// a bracketed list in plain text.
fn extract_scores_from_response(data: &serde_json::Value, expected: usize) -> Option<Vec<f64>> {
    let content = extract_raw_content(data)?;
    let content = content.trim();

    // Try parsing as a JSON array first
    if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(content) {
        let scores: Vec<f64> = arr
            .iter()
            .filter_map(|v| v.as_f64())
            .map(|s| s.clamp(0.0, 100.0))
            .collect();
        if scores.len() == expected {
            return Some(scores);
        }
        // 分数数量不匹配 — 视为解析失败，避免部分分数被记为成功
    }

    // Fallback: try to extract numbers from bracketed text like "[85, 42, 91]"
    if content.starts_with('[') && content.contains(']') {
        let inner = &content[1..content.find(']').unwrap_or(content.len())];
        let scores: Vec<f64> = inner
            .split(',')
            .filter_map(|s| parse_score(s.trim()))
            .collect();
        if scores.len() == expected {
            return Some(scores);
        }
    }

    // Last resort: extract all numbers from the content
    let scores: Vec<f64> = content
        .split(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<f64>().ok())
        .map(|s| s.clamp(0.0, 100.0))
        .collect();
    if scores.len() == expected {
        return Some(scores);
    }

    None
}

/// Extract the text content from a chat/completions response.
fn extract_raw_content(data: &serde_json::Value) -> Option<String> {
    let choices = data.get("choices")?.as_array()?;
    let message = choices.first()?.get("message")?;
    let content = message.get("content")?.as_str()?;
    Some(content.to_string())
}

// ---------------------------------------------------------------------------
// P3-018: Unified rerank orchestrator
// ---------------------------------------------------------------------------

/// Orchestrate reranking with a two-tier fallback strategy:
/// 1. Dedicated rerank API (if configured and available)
/// 2. Local rerank (weighted signal combination)
///
/// Note: Tier 2 (chat/completions adapter) is intentionally skipped here.
/// Callers that want chat/completions fallback should use `try_model_rerank_simple`
/// which handles the chat/completions → local two-tier chain.
///
/// Returns a `RerankResult` describing which tier was used and the scored
/// candidate list.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub async fn try_model_rerank(
    config: &RerankConfig,
    _query: &str,
    candidates: &[(i64, LocalRankSignals)],
    _candidate_texts: &[RerankCandidate],
    weights: &[f64],
    budget: Option<&crate::kb::cost::TokenBudget>,
    // Dedicated rerank API caller — returns None if not configured or failed
    dedicated_rerank_fn: Option<
        Box<
            dyn FnOnce() -> std::pin::Pin<
                    Box<dyn std::future::Future<Output = Option<Vec<(i64, f64)>>> + Send>,
                > + Send,
        >,
    >,
) -> (Vec<(i64, f64)>, RerankResult) {
    // Tier 0: Not enabled or no candidates — skip everything
    if !config.model_rerank_enabled || candidates.is_empty() {
        let local = local_rerank(candidates, weights);
        return (
            local,
            RerankResult {
                model_rerank_attempted: false,
                model_rerank_succeeded: false,
                fallback_used: false,
                fallback_reason: None,
                provider: String::new(),
                candidates_reranked: 0,
            },
        );
    }

    // 外部 rerank 始终允许，跳过隐私检查

    if let Some(b) = budget {
        if b.remaining() == 0 {
            let local = local_rerank(candidates, weights);
            return (
                local,
                RerankResult {
                    model_rerank_attempted: false,
                    model_rerank_succeeded: false,
                    fallback_used: true,
                    fallback_reason: Some(FallbackReason::BudgetExceeded),
                    provider: "local".into(),
                    candidates_reranked: candidates.len(),
                },
            );
        }
    }

    // Tier 1: Dedicated rerank API
    if let Some(dedicated_fn) = dedicated_rerank_fn {
        debug!("try_model_rerank: attempting dedicated rerank API");
        match dedicated_fn().await {
            Some(scored) => {
                return (
                    scored,
                    RerankResult {
                        model_rerank_attempted: true,
                        model_rerank_succeeded: true,
                        fallback_used: false,
                        fallback_reason: None,
                        provider: config.rerank_provider.clone(),
                        candidates_reranked: candidates.len(),
                    },
                );
            }
            None => {
                debug!(
                    "try_model_rerank: dedicated rerank API failed, falling back to local rerank"
                );
            }
        }
    }

    // Tier 2: Chat/completions adapter
    // FIX11-09: Tier 2 之前传入空 API key 导致永远跳过。
    // try_model_rerank 的调用方（search.rs）已通过 try_model_rerank_simple 传入 api_key，
    // 因此 try_model_rerank 本身不再需要 Tier 2 — 调用方应直接使用 try_model_rerank_simple。
    // 保留此分支但移除空 API key 硬编码，改为注释说明此 Tier 已由 try_model_rerank_simple 覆盖。
    debug!("try_model_rerank: Tier 2 (chat/completions) is handled by try_model_rerank_simple; skipping here");

    // Tier 3: Local rerank fallback
    let local = local_rerank(candidates, weights);
    (
        local,
        RerankResult {
            model_rerank_attempted: true,
            model_rerank_succeeded: false,
            fallback_used: true,
            fallback_reason: Some(FallbackReason::ApiError),
            provider: "local".into(),
            candidates_reranked: candidates.len(),
        },
    )
}

/// Convenience overload: try_model_rerank without a dedicated rerank API.
/// Only uses chat/completions adapter and local_rerank as fallbacks.
#[allow(clippy::too_many_arguments)]
pub async fn try_model_rerank_simple(
    config: &RerankConfig,
    query: &str,
    candidates: &[(i64, LocalRankSignals)],
    candidate_texts: &[RerankCandidate],
    weights: &[f64],
    budget: Option<&crate::kb::cost::TokenBudget>,
    chat_base_url: &str,
    chat_api_key: &str,
) -> (Vec<(i64, f64)>, RerankResult) {
    let reranker = ChatCompletionsReranker::from_config(config, chat_base_url, chat_api_key);

    // Pre-checks
    if !config.model_rerank_enabled || candidates.is_empty() {
        let local = local_rerank(candidates, weights);
        return (
            local,
            RerankResult {
                model_rerank_attempted: false,
                model_rerank_succeeded: false,
                fallback_used: false,
                fallback_reason: None,
                provider: String::new(),
                candidates_reranked: 0,
            },
        );
    }

    // 外部 rerank 始终允许

    if let Some(b) = budget {
        if b.remaining() == 0 {
            let local = local_rerank(candidates, weights);
            return (
                local,
                RerankResult {
                    model_rerank_attempted: false,
                    model_rerank_succeeded: false,
                    fallback_used: true,
                    fallback_reason: Some(FallbackReason::BudgetExceeded),
                    provider: "local".into(),
                    candidates_reranked: candidates.len(),
                },
            );
        }
    }

    // Try chat/completions adapter
    if !reranker.api_key.is_empty() && !reranker.base_url.is_empty() {
        // 候选文本为空或少于实际会发给模型的候选数时跳过，避免部分/空请求被记为成功
        // 调用方可能只截取前 rerank_max_candidates 个文本，用 min 对齐
        let expected_texts = candidates.len().min(config.rerank_max_candidates);
        if candidate_texts.is_empty() || candidate_texts.len() < expected_texts {
            debug!(
                "try_model_rerank_simple: candidate_texts({}) < expected({}), skipping model rerank",
                candidate_texts.len(),
                expected_texts
            );
            // 未实际发起外部请求，直接返回本地 rerank，标记 attempted=false 避免误写审计
            let local = local_rerank(candidates, weights);
            return (
                local,
                RerankResult {
                    model_rerank_attempted: false,
                    model_rerank_succeeded: false,
                    fallback_used: false,
                    fallback_reason: None,
                    provider: "local".into(),
                    candidates_reranked: candidates.len(),
                },
            );
        }
        match chat_completions_rerank(&reranker, query, candidate_texts, budget).await {
            Some(scored) => {
                return (
                    scored,
                    RerankResult {
                        model_rerank_attempted: true,
                        model_rerank_succeeded: true,
                        fallback_used: false,
                        fallback_reason: None,
                        provider: format!("chat_completions/{}", config.rerank_model),
                        candidates_reranked: candidates.len(),
                    },
                );
            }
            None => {
                debug!("try_model_rerank_simple: chat/completions adapter failed, falling back to local_rerank");
            }
        }
    }

    // Fallback to local rerank
    let local = local_rerank(candidates, weights);
    (
        local,
        RerankResult {
            model_rerank_attempted: true,
            model_rerank_succeeded: false,
            fallback_used: true,
            fallback_reason: Some(FallbackReason::ApiError),
            provider: "local".into(),
            candidates_reranked: candidates.len(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rrf_merge() {
        let list1 = vec![(1, 0.9), (2, 0.8)];
        let list2 = vec![(2, 0.7), (3, 0.6)];
        let merged = rrf_merge(&[list1, list2], 60.0);
        assert!(!merged.is_empty());
        // doc 2 appears in both lists, should rank high
        assert_eq!(merged[0].0, 2);
    }

    #[test]
    fn test_local_rerank() {
        let candidates = vec![
            (
                1,
                LocalRankSignals {
                    fts_score: 0.9,
                    ..Default::default()
                },
            ),
            (
                2,
                LocalRankSignals {
                    title_score: 0.8,
                    vector_score: 0.3,
                    ..Default::default()
                },
            ),
        ];
        let weights = vec![0.4, 0.3, 0.2, 0.1, 0.0, 0.0];
        let ranked = local_rerank(&candidates, &weights);
        assert_eq!(ranked.len(), 2);
    }

    #[test]
    fn test_fallback_reason_as_str() {
        assert_eq!(FallbackReason::Timeout.as_str(), "timeout");
        assert_eq!(FallbackReason::PrivacyBlocked.as_str(), "privacy_blocked");
    }

    // --- P3-018: Chat/completions adapter tests ---

    #[test]
    fn test_parse_score_numeric() {
        assert_eq!(parse_score("85"), Some(85.0));
        assert_eq!(parse_score("42.5"), Some(42.5));
        assert_eq!(parse_score("0"), Some(0.0));
        assert_eq!(parse_score("100"), Some(100.0));
    }

    #[test]
    fn test_parse_score_with_punctuation() {
        assert_eq!(parse_score("85."), Some(85.0));
        assert_eq!(parse_score("90,"), Some(90.0));
        assert_eq!(parse_score("75;"), Some(75.0));
    }

    #[test]
    fn test_parse_score_clamps_range() {
        assert_eq!(parse_score("150"), Some(100.0));
        assert_eq!(parse_score("-10"), Some(0.0));
    }

    #[test]
    fn test_parse_score_non_numeric() {
        assert_eq!(parse_score("not a number"), None);
        assert_eq!(parse_score(""), None);
    }

    #[test]
    fn test_parse_score_from_sentence() {
        // Model might return "The relevance score is 85" — extract 85
        assert_eq!(parse_score("The relevance score is 85"), Some(85.0));
    }

    #[test]
    fn test_extract_scores_from_json_array() {
        let data = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "[85, 42, 91]"
                }
            }]
        });
        let scores = extract_scores_from_response(&data, 3);
        assert_eq!(scores, Some(vec![85.0, 42.0, 91.0]));
    }

    #[test]
    fn test_extract_scores_from_bracketed_text() {
        let data = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Scores: [85, 42, 91]"
                }
            }]
        });
        let scores = extract_scores_from_response(&data, 3);
        assert_eq!(scores, Some(vec![85.0, 42.0, 91.0]));
    }

    #[test]
    fn test_extract_scores_partial_match() {
        let data = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "[85, 42]"
                }
            }]
        });
        // Expected 3 but got 2 — 分数不足时返回 None，避免部分结果被记为成功
        let scores = extract_scores_from_response(&data, 3);
        assert_eq!(scores, None);
    }

    #[test]
    fn test_extract_scores_empty_response() {
        let data = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "I cannot determine relevance."
                }
            }]
        });
        let scores = extract_scores_from_response(&data, 3);
        assert_eq!(scores, None);
    }

    #[test]
    fn test_chat_completions_reranker_from_config() {
        let config = RerankConfig {
            rerank_model: "gpt-4o-mini".into(),
            rerank_timeout_ms: 3000,
            rerank_max_candidates: 20,
            ..Default::default()
        };
        let reranker = ChatCompletionsReranker::from_config(
            &config,
            "https://api.openai.com/v1",
            "sk-test-key",
        );
        assert_eq!(reranker.model, "gpt-4o-mini");
        assert_eq!(reranker.timeout_ms, 3000);
        assert_eq!(reranker.max_candidates, 20);
        assert_eq!(reranker.base_url, "https://api.openai.com/v1");
        assert_eq!(reranker.api_key, "sk-test-key");
    }

    #[test]
    fn test_chat_completions_rerank_no_api_key() {
        let reranker = ChatCompletionsReranker {
            base_url: String::new(),
            api_key: String::new(),
            model: "gpt-4o-mini".into(),
            timeout_ms: 5000,
            max_candidates: 50,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(chat_completions_rerank(&reranker, "test query", &[], None));
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_try_model_rerank_simple_not_enabled() {
        let config = RerankConfig {
            model_rerank_enabled: false,
            ..Default::default()
        };
        let candidates = vec![(
            1,
            LocalRankSignals {
                fts_score: 0.9,
                ..Default::default()
            },
        )];
        let candidate_texts = vec![RerankCandidate {
            doc_id: 1,
            text: "test".into(),
        }];
        let (scored, result) = try_model_rerank_simple(
            &config,
            "query",
            &candidates,
            &candidate_texts,
            &[0.4, 0.3, 0.2, 0.1, 0.0, 0.0],
            None,
            "",
            "",
        )
        .await;
        assert!(!result.model_rerank_attempted);
        assert_eq!(scored.len(), 1);
    }

    #[tokio::test]
    async fn test_try_model_rerank_simple_budget_exceeded() {
        let config = RerankConfig {
            model_rerank_enabled: true,
            ..Default::default()
        };
        let budget = crate::kb::cost::TokenBudget::new(0); // zero budget
        let candidates = vec![(
            1,
            LocalRankSignals {
                fts_score: 0.9,
                ..Default::default()
            },
        )];
        let candidate_texts = vec![RerankCandidate {
            doc_id: 1,
            text: "test".into(),
        }];
        let (_, result) = try_model_rerank_simple(
            &config,
            "query",
            &candidates,
            &candidate_texts,
            &[0.4, 0.3, 0.2, 0.1, 0.0, 0.0],
            Some(&budget),
            "https://api.openai.com/v1",
            "sk-key",
        )
        .await;
        assert!(result.fallback_used);
        assert_eq!(result.fallback_reason, Some(FallbackReason::BudgetExceeded));
    }

    #[test]
    fn test_build_rerank_prompt() {
        let prompt =
            build_rerank_prompt("distributed systems", "A paper about consensus algorithms");
        assert!(prompt.contains("distributed systems"));
        assert!(prompt.contains("consensus algorithms"));
        assert!(prompt.contains("0-100"));
    }
}
