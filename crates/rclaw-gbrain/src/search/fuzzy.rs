//! Fuzzy search using trigram similarity (pg_trgm equivalent for SQLite)
//!
//! Computes trigram similarity between query and text using Jaccard coefficient
//! on trigram sets. This provides fuzzy matching similar to PostgreSQL's pg_trgm
//! `similarity()` function.
//!
//! The core `trigram_similarity()` function is shared between:
//! - `SqliteEngine::find_by_title_fuzzy()` (title-only matching)
//! - `fuzzy_search()` (title + compiled_truth matching, returning full SearchResult)

use crate::engine::BrainEngine;
use crate::sqlite_engine::SqliteEngine;
use crate::types::*;
use std::collections::HashSet;
use tracing::{debug, info};

/// Extract character trigrams from a string.
///
/// Follows pg_trgm convention: pads the input with two spaces on each side,
/// then extracts all overlapping 3-character windows.
/// Uses char-level windows (not byte-level) for UTF-8 safety.
pub fn trigrams(s: &str) -> HashSet<String> {
    let padded = format!("  {}  ", s);
    let chars: Vec<char> = padded.chars().collect();
    chars
        .windows(3)
        .map(|w| w.iter().collect::<String>())
        .collect()
}

/// Compute trigram similarity (Jaccard coefficient) between two strings.
///
/// Returns a value between 0.0 and 1.0 where:
/// - 1.0 means identical trigram sets
/// - 0.0 means no shared trigrams (or either string is empty)
///
/// Comparison is case-insensitive (both inputs are lowercased).
pub fn trigram_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let set_a = trigrams(&a.to_lowercase());
    let set_b = trigrams(&b.to_lowercase());

    let intersection = set_a.intersection(&set_b).count() as f64;
    let union = set_a.union(&set_b).count() as f64;

    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

/// Search pages by trigram similarity to query.
///
/// Matches against both title and compiled_truth content.
/// For each page, the final score is the maximum similarity across
/// title and content matching. Results are sorted by score descending
/// and only include pages above `min_similarity`.
///
/// This is the pg_trgm equivalent of:
/// ```sql
/// SELECT slug, title, max(similarity(title, query), similarity(compiled_truth, query)) AS score
/// FROM pages
/// WHERE score >= min_similarity
/// ORDER BY score DESC
/// LIMIT limit
/// ```
pub fn fuzzy_search(
    engine: &SqliteEngine,
    query: &str,
    min_similarity: f64,
    limit: usize,
) -> crate::error::Result<Vec<SearchResult>> {
    info!(query = %query, min_similarity, limit, "Starting fuzzy search");

    let min_sim = min_similarity.clamp(0.0, 1.0);
    // H10 fix: 添加上限避免全表扫描。对于大数据库（>10000 页），
    // fuzzy search 按标题排序取最新页面作为候选集，而非全部加载。
    // 未来优化：通过 FTS5 预过滤或 SQLite 自定义 trigram 函数推入数据库层。
    let scan_limit = 5000;
    let pages = engine.list_pages(PageFilters {
        limit: Some(scan_limit),
        ..Default::default()
    })?;

    debug!(page_count = pages.len(), "Scanning pages for fuzzy matches");

    let mut results: Vec<(f64, &Page)> = Vec::new();

    for page in &pages {
        // Compute similarity against title
        let title_sim = trigram_similarity(query, &page.title);

        // Compute similarity against compiled_truth (truncated to avoid
        // excessive computation on very long pages)
        let content_preview: String = page.compiled_truth.chars().take(500).collect();
        let content_sim = trigram_similarity(query, &content_preview);

        // Use the max of title and content similarity
        let score = title_sim.max(content_sim);

        if score >= min_sim {
            results.push((score, page));
        }
    }

    // Sort by score descending
    results.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(limit);

    debug!(result_count = results.len(), "Fuzzy search complete");

    Ok(results
        .into_iter()
        .map(|(score, page)| SearchResult {
            slug: page.slug.clone(),
            title: page.title.clone(),
            chunk_text: page.compiled_truth.chars().take(200).collect(),
            score,
            page_id: Some(page.id),
            chunk_id: None,
            chunk_index: None,
            source: Some(ChunkSource::CompiledTruth),
            detail_level: DetailLevel::Low,
            page_type: Some(page.page_type.clone()),
            stale: false,
            updated_at: Some(page.updated_at.clone()),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trigrams_basic() {
        let set = trigrams("abc");
        // Padded: "  abc  " -> ["  a", " ab", "abc", "bc ", "c  "]
        assert!(set.contains("  a"));
        assert!(set.contains(" ab"));
        assert!(set.contains("abc"));
        assert!(set.contains("bc "));
        assert!(set.contains("c  "));
        assert_eq!(set.len(), 5);
    }

    #[test]
    fn test_trigrams_empty() {
        let set = trigrams("");
        // Padded: "    " -> ["  ", "   ", "  "], but as a HashSet deduped: ["   ", "  "]
        // Actually: "    " chars = [' ',' ',' ',' '], windows of 3:
        // ["  ", "   ", "  "] -> HashSet has "  " and "   "
        assert!(set.contains("   "));
    }

    #[test]
    fn test_trigrams_single_char() {
        let set = trigrams("a");
        // Padded: "  a  " -> ["  a", " a ", "a  "]
        assert_eq!(set.len(), 3);
        assert!(set.contains("  a"));
        assert!(set.contains(" a "));
        assert!(set.contains("a  "));
    }

    #[test]
    fn test_trigrams_cjk() {
        let set = trigrams("你好");
        // Padded: "  你好  "
        assert!(set.contains("  你"));
        assert!(set.contains(" 你好"));
        assert!(set.contains("你好 "));
    }

    #[test]
    fn test_trigram_similarity_identical() {
        let score = trigram_similarity("hello", "hello");
        assert!(
            (score - 1.0).abs() < 1e-6,
            "identical strings should have similarity 1.0"
        );
    }

    #[test]
    fn test_trigram_similarity_empty() {
        assert_eq!(trigram_similarity("", "hello"), 0.0);
        assert_eq!(trigram_similarity("hello", ""), 0.0);
        assert_eq!(trigram_similarity("", ""), 0.0);
    }

    #[test]
    fn test_trigram_similarity_case_insensitive() {
        let score = trigram_similarity("Hello World", "hello world");
        assert!(
            (score - 1.0).abs() < 1e-6,
            "case-insensitive match should have similarity ~1.0, got {}",
            score
        );
    }

    #[test]
    fn test_trigram_similarity_partial_match() {
        let score = trigram_similarity("Alice Wonderland", "Alice Wonder");
        assert!(
            score > 0.5,
            "partial match should have similarity > 0.5, got {}",
            score
        );
        assert!(
            score < 1.0,
            "partial match should have similarity < 1.0, got {}",
            score
        );
    }

    #[test]
    fn test_trigram_similarity_no_match() {
        let score = trigram_similarity("xyz", "abc");
        assert!(
            score < 0.2,
            "completely different strings should have very low similarity, got {}",
            score
        );
    }

    #[test]
    fn test_trigram_similarity_symmetric() {
        let score_ab = trigram_similarity("hello", "world");
        let score_ba = trigram_similarity("world", "hello");
        assert!(
            (score_ab - score_ba).abs() < 1e-10,
            "similarity should be symmetric"
        );
    }

    #[test]
    fn test_trigram_similarity_cjk() {
        let score = trigram_similarity("你好世界", "你好世界");
        assert!((score - 1.0).abs() < 1e-6);

        let partial = trigram_similarity("你好世界", "你好");
        assert!(partial > 0.2, "partial CJK match, got {}", partial);
        assert!(partial < 1.0, "partial CJK match, got {}", partial);
    }

    #[test]
    fn test_trigram_similarity_padding_effect() {
        // Short strings with padding should still produce meaningful similarity
        let score = trigram_similarity("ab", "a");
        assert!(
            score > 0.0,
            "padded short strings should have non-zero similarity"
        );
        assert!(
            score < 1.0,
            "different short strings should not be identical"
        );
    }

    #[test]
    fn test_trigram_similarity_near_match() {
        // "OpenAI" vs "Open AI" - slight difference
        let score = trigram_similarity("OpenAI", "Open AI");
        assert!(score > 0.5, "near matches should score high, got {}", score);
    }
}
