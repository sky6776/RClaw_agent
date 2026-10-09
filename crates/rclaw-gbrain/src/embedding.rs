//! OpenAI embedding API client
//! Mirrors gbrain's src/core/embedding.ts
//!
//! Supports batch embedding (internally split into provider-safe batches),
//! retry with exponential backoff, Retry-After header parsing,
//! input truncation, and batch completion callbacks.

use crate::backoff::{backoff_delay_ms, BackoffOpts};
use crate::error::{GBrainError, Result};
use serde::Deserialize;
use tracing::{debug, error, info, warn};

/// Batch embedding result type alias.
type EmbeddingBatchResult = Result<(usize, Vec<Vec<f32>>)>;
/// JoinHandle for batch embedding tasks.
type EmbeddingBatchHandle = tokio::task::JoinHandle<EmbeddingBatchResult>;

/// Default embedding model
pub const DEFAULT_MODEL: &str = "text-embedding-3-large";

/// Default embedding dimensions
pub const DEFAULT_DIMENSIONS: usize = 1536;

/// Maximum batch size for a single embedding API request.
///
/// Some OpenAI-compatible providers, including BigModel embedding-3, reject
/// batches larger than 64 even though other providers allow more. Keep the
/// transport batch conservative and let `embed_batch` split larger inputs.
pub const MAX_BATCH_SIZE: usize = 64;

/// Maximum input text length for embedding (characters)
const MAX_EMBEDDING_INPUT_CHARS: usize = 8000;

/// Embedding API response
#[derive(Debug, Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
    usage: EmbeddingUsage,
}

#[derive(Debug, Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

#[derive(Debug, Deserialize)]
struct EmbeddingUsage {
    total_tokens: i64,
}

/// Embedding client
pub struct Embedder {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    dimensions: usize,
}

impl Embedder {
    /// Create a new embedding client
    pub fn new(api_key: &str, base_url: &str, model: &str, dimensions: usize) -> Self {
        debug!(
            base_url = base_url,
            model = model,
            dimensions = dimensions,
            "Creating embedding client"
        );
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.to_string(),
            base_url: base_url.to_string(),
            model: model.to_string(),
            dimensions,
        }
    }

    /// Check if the client is configured (has API key)
    pub fn is_available(&self) -> bool {
        !self.api_key.is_empty()
    }

    /// Embed a single text string
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        debug!(text_len = text.len(), "Embedding single text");
        let results = self.embed_batch(&[text]).await?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| GBrainError::Embedding("No embedding returned".to_string()))
    }

    /// Embed a batch of text strings.
    ///
    /// The caller may pass any number of texts; this method splits them into
    /// provider-safe request batches and preserves the original order.
    ///
    /// P0-3: 当批次数量超过 1 且环境变量 `GBRAIN_EMBEDDING_CONCURRENCY` > 1 时,
    /// 并发发送多个 MAX_BATCH_SIZE 子批次,保持原始顺序。并发度上限 8,
    /// 避免对外部 embedding 服务造成 burst 压力。失败重试 / 退避行为与单线程版本一致。
    pub async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let concurrency = std::env::var("GBRAIN_EMBEDDING_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, 8);

        // 单子批次或并发度 = 1 时直接走串行路径,避免无谓的 task spawn 开销
        let chunk_count = texts.chunks(MAX_BATCH_SIZE).count();
        if concurrency == 1 || chunk_count <= 1 {
            return self.embed_batch_serial(texts).await;
        }

        self.embed_batch_concurrent(texts, concurrency).await
    }

    /// 串行处理批次:逐 MAX_BATCH_SIZE 子批发送请求,保持顺序。
    async fn embed_batch_serial(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut all_embeddings = Vec::with_capacity(texts.len());
        for batch in texts.chunks(MAX_BATCH_SIZE) {
            let mut embeddings = self.embed_batch_request(batch).await?;
            all_embeddings.append(&mut embeddings);
        }
        Ok(all_embeddings)
    }

    /// 并发处理批次:按 (index, MAX_BATCH_SIZE) 切分子批,使用 bounded semaphore
    /// 限制并发度,完成后按原始 index 拼回顺序结果。
    ///
    /// 失败语义:任一子批失败立即返回错误(其他在飞子批继续完成但结果丢弃),
    /// 与串行版本"失败即中断"行为一致。
    async fn embed_batch_concurrent(
        &self,
        texts: &[&str],
        concurrency: usize,
    ) -> Result<Vec<Vec<f32>>> {
        use std::sync::Arc;
        use tokio::sync::Semaphore;

        let semaphore = Arc::new(Semaphore::new(concurrency));
        // 收集 (start_index, batch_texts) 子任务。注意:tokio::spawn 要求 'static,
        // 因此 batch 必须拥有所有权(String)而不是借用(&str)。
        let mut sub_tasks: Vec<(usize, Vec<String>)> = Vec::new();
        let mut start = 0usize;
        for batch in texts.chunks(MAX_BATCH_SIZE) {
            let owned: Vec<String> = batch.iter().map(|s| s.to_string()).collect();
            sub_tasks.push((start, owned));
            start += batch.len();
        }

        let mut handles: Vec<EmbeddingBatchHandle> = Vec::with_capacity(sub_tasks.len());

        for (idx, batch_owned) in sub_tasks {
            let permit = semaphore.clone().acquire_owned().await.map_err(|e| {
                GBrainError::Embedding(format!("embedding semaphore closed: {}", e))
            })?;
            // 将 self 字段克隆到 owned,使 task 满足 'static
            let client = self.client.clone();
            let api_key = self.api_key.clone();
            let base_url = self.base_url.clone();
            let model = self.model.clone();
            let dimensions = self.dimensions;
            // 复用 self 通过 Arc,避免拷贝大量 batch
            let this = Arc::new(Self {
                client,
                api_key,
                base_url,
                model,
                dimensions,
            });
            handles.push(tokio::spawn(async move {
                let _permit = permit; // 持有 permit 直到请求完成
                let refs: Vec<&str> = batch_owned.iter().map(|s| s.as_str()).collect();
                let embeddings = this.embed_batch_request(&refs).await?;
                Ok::<_, GBrainError>((idx, embeddings))
            }));
        }

        // 收集结果并按子批起始索引排序,以保持原始顺序
        let mut indexed_results: Vec<(usize, Vec<Vec<f32>>)> = Vec::with_capacity(handles.len());
        for handle in handles {
            let outcome = handle
                .await
                .map_err(|e| GBrainError::Embedding(format!("embedding task panicked: {}", e)))??;
            indexed_results.push(outcome);
        }
        indexed_results.sort_by_key(|(idx, _)| *idx);

        let mut all_embeddings = Vec::with_capacity(texts.len());
        for (_, embeddings) in indexed_results {
            all_embeddings.extend(embeddings);
        }
        Ok(all_embeddings)
    }

    async fn embed_batch_request(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        // Truncate input texts to MAX_EMBEDDING_INPUT_CHARS at word boundaries
        let truncated_texts: Vec<String> = texts
            .iter()
            .map(|t| {
                if t.len() > MAX_EMBEDDING_INPUT_CHARS {
                    let truncated = truncate_at_word_boundary(t, MAX_EMBEDDING_INPUT_CHARS);
                    warn!(
                        original_len = t.len(),
                        truncated_len = truncated.len(),
                        "Truncating embedding input at word boundary"
                    );
                    truncated.to_string()
                } else {
                    t.to_string()
                }
            })
            .collect();

        info!(
            model = %self.model,
            text_count = texts.len(),
            dimensions = self.dimensions,
            "Starting embedding batch request"
        );

        let url = format!("{}/embeddings", self.base_url);

        let body = serde_json::json!({
            "model": self.model,
            "input": truncated_texts,
            "dimensions": self.dimensions,
        });

        // Retry with exponential backoff (using shared BackoffOpts + backoff_delay_ms)
        let backoff_opts = BackoffOpts {
            max_retries: 5,
            base_ms: 4000,
            max_ms: 120000,
            jitter: true,
        };

        let mut attempt = 0u32;

        loop {
            attempt += 1;
            debug!(
                attempt,
                max_retries = backoff_opts.max_retries,
                "Sending embedding request"
            );

            let response = self
                .client
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await;

            match response {
                Ok(resp) => {
                    if resp.status().is_success() {
                        let embedding_resp: EmbeddingResponse = resp.json().await.map_err(|e| {
                            GBrainError::Embedding(format!("Failed to parse response: {}", e))
                        })?;

                        let token_count = embedding_resp.usage.total_tokens;
                        let result_count = embedding_resp.data.len();

                        // Validate embedding dimensions match expected configuration
                        for d in &embedding_resp.data {
                            if d.embedding.len() != self.dimensions {
                                warn!(
                                    expected = self.dimensions,
                                    actual = d.embedding.len(),
                                    index = d.index,
                                    "Embedding dimension mismatch — skipping batch"
                                );
                                return Err(GBrainError::Embedding(format!(
                                    "Embedding dimension mismatch: expected {}, got {} at index {}",
                                    self.dimensions,
                                    d.embedding.len(),
                                    d.index
                                )));
                            }
                            // Validate all values are finite (no NaN or Infinity)
                            if d.embedding.iter().any(|v| !v.is_finite()) {
                                warn!(
                                    index = d.index,
                                    "Embedding contains non-finite values (NaN/Inf) — rejecting batch"
                                );
                                return Err(GBrainError::Embedding(format!(
                                    "Embedding contains non-finite values at index {}",
                                    d.index
                                )));
                            }
                        }

                        // Validate response count matches input count
                        if embedding_resp.data.len() != texts.len() {
                            warn!(
                                expected = texts.len(),
                                actual = embedding_resp.data.len(),
                                "Embedding response count mismatch"
                            );
                            return Err(GBrainError::Embedding(format!(
                                "Response count mismatch: expected {} embeddings, got {}",
                                texts.len(),
                                embedding_resp.data.len()
                            )));
                        }

                        // Sort by index to maintain order
                        let mut data = embedding_resp.data;
                        data.sort_by_key(|d| d.index);

                        info!(token_count, result_count, "Embedding batch complete");

                        return Ok(data.into_iter().map(|d| d.embedding).collect());
                    }

                    let status = resp.status();
                    let headers = resp.headers().clone();
                    let error_text = resp.text().await.unwrap_or_default();

                    // Retry on rate limit (429) or server error (5xx)
                    if (status.as_u16() == 429 || status.as_u16() >= 500)
                        && attempt <= backoff_opts.max_retries
                    {
                        // Parse Retry-After header
                        let retry_after_ms = headers
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse::<u64>().ok())
                            .map(|s| s * 1000)
                            .unwrap_or(0);

                        let calculated_ms = backoff_delay_ms(attempt, &backoff_opts);
                        let wait_ms = retry_after_ms.max(calculated_ms);

                        warn!(
                            status = status.as_u16(),
                            attempt,
                            wait_ms,
                            retry_after_ms,
                            "Rate limited or server error, retrying with backoff"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                        continue;
                    }

                    warn!(status = status.as_u16(), error = %error_text, "Embedding API error");
                    return Err(GBrainError::Embedding(format!(
                        "API error {}: {}",
                        status, error_text
                    )));
                }
                Err(e) => {
                    warn!(attempt, error = %e, "Embedding request failed");
                    if attempt <= backoff_opts.max_retries {
                        let delay_ms = backoff_delay_ms(attempt, &backoff_opts);
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    error!(attempt, max_retries = backoff_opts.max_retries, error = %e, "Embedding request failed after all retries");
                    return Err(GBrainError::Embedding(format!(
                        "Request failed after {} attempts: {}",
                        backoff_opts.max_retries + 1,
                        e
                    )));
                }
            }
        }
    }

    /// Get the model name
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Get the dimensions
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }
}

/// Truncate text to at most `max_chars` characters, breaking at a word boundary
/// when possible. Looks for the last space before `max_chars` and truncates there.
/// Falls back to hard truncation at `max_chars` if no space is found.
pub fn truncate_at_word_boundary(text: &str, max_chars: usize) -> &str {
    if text.len() <= max_chars {
        return text;
    }
    let limit = text.floor_char_boundary(max_chars);
    if let Some(space_pos) = text[..limit].rfind(' ') {
        &text[..space_pos]
    } else {
        &text[..limit]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_at_word_boundary_short_text() {
        // Text shorter than limit is returned as-is
        assert_eq!(truncate_at_word_boundary("hello world", 100), "hello world");
    }

    #[test]
    fn test_truncate_at_word_boundary_exact_limit() {
        // Text exactly at limit is returned as-is
        let text = "a".repeat(8000);
        assert_eq!(truncate_at_word_boundary(&text, 8000).len(), 8000);
    }

    #[test]
    fn test_truncate_at_word_boundary_breaks_at_space() {
        // Should break at the last space before the limit, not mid-word
        let word = "abcdefghij";
        let text = format!("{} {} {}", word, word, word); // "abcdefghij abcdefghij abcdefghij"
                                                          // Limit in the middle of the third word — should truncate after the second word
        let truncated = truncate_at_word_boundary(&text, word.len() * 2 + 2 + 5);
        // The truncation should end at a space boundary (may include trailing space)
        // The key invariant: no partial word at the end
        assert!(
            truncated == "abcdefghij abcdefghij " || truncated == "abcdefghij abcdefghij",
            "Expected truncation to end at a space boundary, got: {:?}",
            truncated
        );
        assert!(!truncated.is_empty());
    }

    #[test]
    fn test_truncate_at_word_boundary_no_space_fallback() {
        // Long text with no spaces falls back to hard truncation
        let text = "abcdefghij".repeat(1000); // 10000 chars, no spaces
        let truncated = truncate_at_word_boundary(&text, 8000);
        assert_eq!(truncated.len(), 8000);
    }

    #[test]
    fn test_truncate_at_word_boundary_utf8_safe() {
        // Multi-byte UTF-8 characters should not be split
        let text = "Hello ".to_string() + &"\u{1F600}".repeat(2000); // lots of emoji
        let truncated = truncate_at_word_boundary(&text, 8000);
        // Must be a valid UTF-8 string (no panic = success)
        assert!(truncated.len() <= 8000 || text.len() <= 8000);
    }
}
