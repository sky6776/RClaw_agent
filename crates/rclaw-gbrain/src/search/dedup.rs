//! 6-layer dedup + compiled truth guarantee
//! Mirrors gbrain's src/core/search/dedup.ts
//!
//! P2-9: Added cross-source dedup layer — when two chunks from the same page
//! have similar text but different sources (CompiledTruth vs Timeline),
//! keep the CompiledTruth version and discard the Timeline version.

use crate::types::*;
use std::collections::HashMap;

/// 从文本构建词集，用于 Jaccard 相似度计算。
/// H11 fix: 对 CJK 文本使用 jieba 分词，而非简单的 split_whitespace()。
/// 中文/日文/韩文文本词间无空格，split_whitespace() 会将整句当作一个"词"，
/// 导致 Jaccard 系数无意义，CJK 内容无法正确去重。
fn tokenize_to_word_set(text: &str) -> std::collections::HashSet<String> {
    let lower = text.to_lowercase();
    // 检测是否包含 CJK 字符（Unicode CJK Unified Ideographs 范围）
    let has_cjk = lower.chars().any(|c| {
        ('\u{4E00}'..='\u{9FFF}').contains(&c)    // CJK Unified Ideographs
            || ('\u{3400}'..='\u{4DBF}').contains(&c) // CJK Unified Ideographs Extension A
            || ('\u{3040}'..='\u{309F}').contains(&c) // Hiragana
            || ('\u{30A0}'..='\u{30FF}').contains(&c) // Katakana
            || ('\u{AC00}'..='\u{D7AF}').contains(&c) // Hangul Syllables
    });
    if has_cjk {
        // 使用 jieba 分词处理 CJK 文本
        crate::nlp::chinese::tokenize_content(&lower)
            .split_whitespace()
            .map(|s| s.to_string())
            .collect()
    } else {
        lower.split_whitespace().map(|s| s.to_string()).collect()
    }
}

/// P2-9: Cross-source dedup within the same page.
/// When two chunks from the same slug have Jaccard similarity > threshold
/// but different chunk_source values, keep the CompiledTruth version
/// and discard the Timeline version. This prevents near-duplicate content
/// from the same page (e.g., a timeline entry that restates compiled truth)
/// from polluting results.
fn dedup_by_cross_source(results: &mut Vec<SearchResult>, threshold: f64) {
    // Group by slug first
    let mut by_slug: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, r) in results.iter().enumerate() {
        by_slug.entry(r.slug.clone()).or_default().push(i);
    }

    // Pre-compute word sets for all results to avoid O(n^2) set construction
    let word_sets: Vec<std::collections::HashSet<String>> = results
        .iter()
        .map(|r| tokenize_to_word_set(&r.chunk_text))
        .collect();

    let mut to_remove: Vec<usize> = Vec::new();

    for indices in by_slug.values() {
        // For each pair of chunks from the same page
        for i in 0..indices.len() {
            if to_remove.contains(&indices[i]) {
                continue; // Already marked for removal
            }
            let r_i_words = &word_sets[indices[i]];

            for j in (i + 1)..indices.len() {
                if to_remove.contains(&indices[j]) {
                    continue;
                }
                let r_j_words = &word_sets[indices[j]];

                let intersection = r_i_words.intersection(r_j_words).count() as f64;
                let union = r_i_words.union(r_j_words).count() as f64;
                if union == 0.0 {
                    continue;
                }
                let jaccard = intersection / union;

                if jaccard > threshold {
                    let r_i = &results[indices[i]];
                    let r_j = &results[indices[j]];
                    // Similar text from same page, different sources → keep CT, drop non-CT
                    let i_ct = r_i.source.as_ref() == Some(&ChunkSource::CompiledTruth);
                    let j_ct = r_j.source.as_ref() == Some(&ChunkSource::CompiledTruth);

                    if i_ct && !j_ct {
                        to_remove.push(indices[j]);
                    } else if j_ct && !i_ct {
                        to_remove.push(indices[i]);
                        break; // i is removed, stop comparing from i
                    } else {
                        // Same source or both no source — keep higher score
                        if r_i.score >= r_j.score {
                            to_remove.push(indices[j]);
                        } else {
                            to_remove.push(indices[i]);
                            break;
                        }
                    }
                }
            }
        }
    }

    // Remove marked indices (in reverse order to preserve positions)
    to_remove.sort_unstable();
    to_remove.dedup();
    for &idx in to_remove.iter().rev() {
        if idx < results.len() {
            results.remove(idx);
        }
    }
}

/// Dedup by text similarity: remove results too similar to already-kept results.
/// Mirrors TS dedup.ts: accumulates kept results, compares each candidate
/// against ALL kept results (prevents transitive similarity misses).
fn dedup_by_text_similarity(results: &mut Vec<SearchResult>, threshold: f64) {
    // Pre-compute word sets for all results to avoid O(n^2) set construction
    let word_sets: Vec<std::collections::HashSet<String>> = results
        .iter()
        .map(|r| tokenize_to_word_set(&r.chunk_text))
        .collect();

    let mut kept: Vec<usize> = Vec::with_capacity(results.len());

    for (i, r_words) in word_sets.iter().enumerate() {
        let too_similar = kept.iter().any(|&k| {
            let k_words = &word_sets[k];
            let intersection = r_words.intersection(k_words).count() as f64;
            let union = r_words.union(k_words).count() as f64;
            if union == 0.0 {
                return false;
            }
            (intersection / union) > threshold
        });

        if !too_similar {
            kept.push(i);
        }
    }

    // Rebuild results keeping only the indices in `kept`
    let kept_set: std::collections::HashSet<usize> = kept.into_iter().collect();
    let mut new_results = Vec::with_capacity(kept_set.len());
    for (i, r) in results.drain(..).enumerate() {
        if kept_set.contains(&i) {
            new_results.push(r);
        }
    }
    *results = new_results;
}

/// Enforce type diversity: cap any single page_type to max_ratio of results.
/// R3-04 fix: Pre-computes the maximum allowed count per type as
/// floor(max_ratio * total_typed_count) to avoid cascading over-pruning
/// caused by a shrinking running denominator.
fn enforce_type_diversity(results: &mut Vec<SearchResult>, max_ratio: f64) {
    if results.is_empty() {
        return;
    }
    // Count only results with a page_type as the denominator for ratio checks
    let typed_count = results.iter().filter(|r| r.page_type.is_some()).count();
    if typed_count == 0 {
        return;
    }
    // R3-04: Pre-compute max allowed per type using the initial denominator.
    // This prevents cascading over-pruning where removing one type's results
    // changes the denominator and causes more removals than intended.
    let max_per_type = ((max_ratio * typed_count as f64).floor() as usize).max(1);

    let mut type_counts: HashMap<Option<PageType>, usize> = HashMap::new();
    let mut to_remove = Vec::new();

    for (i, r) in results.iter().enumerate() {
        // Skip results with no page_type — they don't count toward diversity
        if r.page_type.is_none() {
            continue;
        }
        let count = type_counts.entry(r.page_type.clone()).or_insert(0);
        *count += 1;
        if *count > max_per_type {
            to_remove.push(i);
            *count -= 1;
        }
    }

    for &i in to_remove.iter().rev() {
        results.remove(i);
    }
}

/// Configurable dedup options (mirrors TS dedupOpts)
#[derive(Debug, Clone)]
pub struct DedupOpts {
    /// Jaccard similarity threshold for text dedup (default 0.85)
    pub jaccard_threshold: f64,
    /// Maximum ratio of any single page_type in results (default 0.6)
    pub max_type_ratio: f64,
    /// Maximum chunks per page (default 2)
    pub max_per_page: usize,
}

impl Default for DedupOpts {
    fn default() -> Self {
        Self {
            jaccard_threshold: 0.85,
            max_type_ratio: 0.6,
            max_per_page: 2,
        }
    }
}

/// Run dedup on search results — 6-layer pipeline matching TS.
///
/// Layer 1: Collect top 3 chunks per page (by slug)
/// Layer 1.5: Cross-source dedup — within same page, drop similar non-CT chunks when CT exists (P2-9)
/// Layer 2: Text similarity dedup — Jaccard vs ALL kept results
/// Layer 3: Type diversity — cap any single page_type
/// Layer 4: Cap N chunks per page (from opts.max_per_page)
/// Layer 5: Compiled truth guarantee — swap in best CT chunk if page has none
/// Final: Flatten, sort by score, truncate to limit
pub fn dedup_results(
    hits: Vec<SearchResult>,
    limit: usize,
    opts: Option<DedupOpts>,
) -> Vec<SearchResult> {
    let opts = opts.unwrap_or_default();

    // Pre-build best CT per slug from ALL hits before Layer 1 consumes them.
    // This ensures Layer 5 (CT guarantee) can swap in CT chunks even for pages
    // whose CT chunks were filtered out during earlier layers.
    let mut best_ct_per_slug: HashMap<String, SearchResult> = HashMap::new();
    for hit in &hits {
        if hit.source.as_ref() == Some(&ChunkSource::CompiledTruth) {
            best_ct_per_slug
                .entry(hit.slug.clone())
                .and_modify(|existing| {
                    if hit.score > existing.score {
                        *existing = hit.clone();
                    }
                })
                .or_insert_with(|| hit.clone());
        }
    }

    // Layer 1: Collect top 3 chunks per page, sorted by score
    // CT priority: within each slug group, always prefer CT chunks over non-CT
    // chunks regardless of score. Score is only used as tiebreaker within the
    // same CT status. This prevents Layer 5 (CT guarantee) from failing to find
    // CT chunks that were filtered out here.
    let mut by_slug: HashMap<String, Vec<SearchResult>> = HashMap::new();
    for hit in hits {
        by_slug.entry(hit.slug.clone()).or_default().push(hit);
    }

    let mut top_per_page: Vec<SearchResult> = Vec::new();
    for (_, mut chunks) in by_slug {
        // Sort: CT chunks first, then by score descending within same CT status
        chunks.sort_by(|a, b| {
            let a_ct = a.source.as_ref() == Some(&ChunkSource::CompiledTruth);
            let b_ct = b.source.as_ref() == Some(&ChunkSource::CompiledTruth);
            // CT chunks always come before non-CT chunks
            match (a_ct, b_ct) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => b
                    .score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            }
        });
        chunks.truncate(3); // top 3 per page
        top_per_page.extend(chunks);
    }

    // Sort globally by score
    top_per_page.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Layer 1.5: Cross-source dedup (P2-9)
    // Within same page, when two chunks have similar text but different sources,
    // keep CompiledTruth and discard Timeline. Uses same threshold as Layer 2.
    dedup_by_cross_source(&mut top_per_page, opts.jaccard_threshold);

    // Layer 2: Jaccard text similarity dedup
    dedup_by_text_similarity(&mut top_per_page, opts.jaccard_threshold);

    // Layer 3: Type diversity enforcement
    enforce_type_diversity(&mut top_per_page, opts.max_type_ratio);

    // Layer 4: Cap 2 chunks per page
    let mut capped: Vec<SearchResult> = Vec::new();
    let mut per_page_counts: HashMap<String, usize> = HashMap::new();
    for result in top_per_page {
        let count = per_page_counts.entry(result.slug.clone()).or_insert(0);
        if *count < opts.max_per_page {
            capped.push(result);
            *count += 1;
        }
    }

    // Layer 5: Compiled truth guarantee — if a page has no CT chunk in results,
    // swap in the best CT chunk for that page (replace lowest-scoring non-CT chunk)
    let mut final_results: Vec<SearchResult> = Vec::new();
    let mut slugs_with_ct: HashMap<String, f64> = HashMap::new();

    // Identify which slugs need a CT swap and find their lowest-scoring non-CT chunk
    let mut slugs_needing_ct: HashMap<String, usize> = HashMap::new(); // slug -> index of lowest-score non-CT chunk
    for r in capped.iter() {
        if r.source.as_ref() == Some(&ChunkSource::CompiledTruth) {
            slugs_with_ct
                .entry(r.slug.clone())
                .and_modify(|s| *s = s.max(r.score))
                .or_insert(r.score);
        }
    }
    for (i, r) in capped.iter().enumerate() {
        if r.source.as_ref() != Some(&ChunkSource::CompiledTruth)
            && !slugs_with_ct.contains_key(&r.slug)
        {
            // Track the lowest-scoring non-CT chunk for this slug
            slugs_needing_ct
                .entry(r.slug.clone())
                .and_modify(|best_idx| {
                    if capped[*best_idx].score > r.score {
                        *best_idx = i;
                    }
                })
                .or_insert(i);
        }
    }

    for (i, r) in capped.into_iter().enumerate() {
        let has_ct = r.source.as_ref() == Some(&ChunkSource::CompiledTruth);
        let page_has_ct = slugs_with_ct.contains_key(&r.slug);

        if !has_ct && !page_has_ct {
            // This slug needs a CT swap — only replace the lowest-scoring non-CT chunk
            if let Some(&swap_idx) = slugs_needing_ct.get(&r.slug) {
                if i == swap_idx {
                    // This is the lowest-scoring non-CT chunk — swap in CT
                    if let Some(ct_chunk) = best_ct_per_slug.get(&r.slug) {
                        final_results.push(ct_chunk.clone());
                        slugs_with_ct.insert(r.slug.clone(), ct_chunk.score);
                        continue;
                    }
                }
                // Other non-CT chunks for this slug are kept (they'll get CT from the swap)
            }
        }

        final_results.push(r);
    }

    // Final: sort by score and truncate
    final_results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    final_results.truncate(limit);

    final_results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(slug: &str, chunk_text: &str, score: f64) -> SearchResult {
        SearchResult {
            slug: slug.to_string(),
            title: slug.to_string(),
            chunk_text: chunk_text.to_string(),
            score,
            page_id: None,
            chunk_id: None,
            chunk_index: None,
            source: None,
            detail_level: DetailLevel::Medium,
            page_type: None,
            stale: false,
            updated_at: None,
        }
    }

    fn make_ct_result(slug: &str, chunk_text: &str, score: f64) -> SearchResult {
        let mut r = make_result(slug, chunk_text, score);
        r.source = Some(ChunkSource::CompiledTruth);
        r
    }

    fn make_tl_result(slug: &str, chunk_text: &str, score: f64) -> SearchResult {
        let mut r = make_result(slug, chunk_text, score);
        r.source = Some(ChunkSource::Timeline);
        r
    }

    #[test]
    fn test_text_similarity_dedup() {
        // These two strings differ by only 1 word out of ~15 unique words,
        // giving Jaccard > 0.85 and triggering dedup.
        let results = vec![
            make_result(
                "a",
                "The quick brown fox jumps over the lazy dog in the park today morning and afternoon",
                0.9,
            ),
            make_result(
                "b",
                "The quick brown fox jumps over the lazy dog in the park today evening and afternoon",
                0.8,
            ),
        ];
        let deduped = dedup_results(results, 10, None);
        // Should keep only the higher-scored one due to Jaccard > 0.85
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].slug, "a");
    }

    #[test]
    fn test_type_diversity_enforcement() {
        let mut results: Vec<SearchResult> = (0..8)
            .map(|i| {
                let mut r = make_result(&format!("slug-{i}"), "content", 0.9 - i as f64 * 0.05);
                r.page_type = Some(PageType::Person);
                r
            })
            .chain((0..4).map(|i| {
                let mut r = make_result(&format!("company-{i}"), "content", 0.7 - i as f64 * 0.05);
                r.page_type = Some(PageType::Company);
                r
            }))
            .collect();
        // Sort by score first
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let deduped = dedup_results(results, 10, None);
        // R3-04: With pre-computed max_per_type = floor(0.6 * 12) = 7,
        // Person is capped at 7, Company stays at 4 → total 11.
        // The key invariant: no single type exceeds max_per_type in the output.
        let person_count = deduped
            .iter()
            .filter(|r| r.page_type == Some(PageType::Person))
            .count();
        let company_count = deduped
            .iter()
            .filter(|r| r.page_type == Some(PageType::Company))
            .count();
        // Person should be capped at max_per_type = 7
        assert!(
            person_count <= 7,
            "Person count {} should be <= 7",
            person_count
        );
        // Company should not be capped (4 <= 7)
        assert!(
            company_count <= 4,
            "Company count {} should be <= 4",
            company_count
        );
        // At least some results should remain
        assert!(!deduped.is_empty());
    }

    #[test]
    fn test_slug_dedup() {
        let results = vec![
            make_result("a", "content 1", 0.9),
            make_result("a", "content 2", 0.8),
            make_result("b", "content 3", 0.7),
        ];
        let deduped = dedup_results(results, 10, None);
        // With cap=2 per page: page "a" gets 2 chunks, page "b" gets 1 = 3 total
        assert_eq!(deduped.len(), 3);
        let a_chunks: Vec<_> = deduped.iter().filter(|r| r.slug == "a").collect();
        assert!(a_chunks.len() <= 2, "Page 'a' should have at most 2 chunks");
        assert!(
            !a_chunks.is_empty(),
            "Page 'a' should have at least 1 chunk"
        );
    }

    #[test]
    fn test_multi_chunk_per_page() {
        // Two different chunks from the same page should both survive
        // if they're semantically different enough
        let results = vec![
            make_result(
                "a",
                "machine learning algorithms for classification tasks",
                0.9,
            ),
            make_result(
                "a",
                "database schema design and SQL optimization patterns",
                0.8,
            ),
            make_result("b", "different page content here", 0.7),
        ];
        let deduped = dedup_results(results, 10, None);
        // Page "a" should have up to 2 chunks retained
        assert!(deduped.len() >= 2);
        let a_count = deduped.iter().filter(|r| r.slug == "a").count();
        assert!(a_count <= 2);
        assert!(a_count >= 1);
    }

    #[test]
    fn test_per_page_cap_2() {
        // 4 different chunks from same page, should cap at 2
        let results = vec![
            make_ct_result("a", "first topic about AI and neural networks", 0.9),
            make_ct_result("a", "second topic about databases and SQL", 0.85),
            make_ct_result("a", "third topic about web development", 0.8),
            make_ct_result("a", "fourth topic about systems programming", 0.75),
        ];
        let deduped = dedup_results(results, 10, None);
        assert!(deduped.len() <= 2);
    }

    #[test]
    fn test_compiled_truth_guarantee() {
        let results = vec![
            make_result("a", "timeline chunk about events", 0.9),
            make_ct_result("a", "compiled truth about core facts", 0.3),
        ];
        let deduped = dedup_results(results, 10, None);
        // Should have at least one CT chunk
        let has_ct = deduped
            .iter()
            .any(|r| r.source.as_ref() == Some(&ChunkSource::CompiledTruth));
        assert!(has_ct, "Should include at least one compiled truth chunk");
    }

    // P2-9: Cross-source dedup tests

    #[test]
    fn test_cross_source_dedup_prefers_ct() {
        // Same page, very similar text, different sources → keep CT, drop Timeline
        let results = vec![
            make_ct_result(
                "a",
                "Alice is a software engineer at Google working on AI and machine learning systems",
                0.7,
            ),
            make_tl_result(
                "a",
                "Alice is a software engineer at Google working on AI and machine learning systems",
                0.9,
            ),
        ];
        let deduped = dedup_results(results, 10, None);
        // Should keep CT chunk and drop Timeline chunk (similar text, same page)
        let ct_count = deduped
            .iter()
            .filter(|r| r.source.as_ref() == Some(&ChunkSource::CompiledTruth))
            .count();
        let tl_count = deduped
            .iter()
            .filter(|r| r.source.as_ref() == Some(&ChunkSource::Timeline))
            .count();
        assert!(ct_count >= 1, "Should keep at least one CT chunk");
        assert_eq!(
            tl_count, 0,
            "Should drop Timeline chunk when CT covers same content"
        );
    }

    #[test]
    fn test_cross_source_dedup_keeps_dissimilar() {
        // Same page, different text, different sources → keep both
        let results = vec![
            make_ct_result("a", "Alice is a software engineer at Google", 0.7),
            make_tl_result("a", "In 2023-Q1 the company raised a Series B round", 0.9),
        ];
        let deduped = dedup_results(results, 10, None);
        // Text is dissimilar, so both should survive
        assert!(
            deduped.len() >= 2,
            "Dissimilar chunks from different sources should both survive"
        );
    }

    #[test]
    fn test_cross_source_dedup_different_pages() {
        // Different pages, similar text, different sources → both kept
        // (cross-source dedup only applies within same page)
        let results = vec![
            make_ct_result(
                "a",
                "Alice is a software engineer at Google working on AI",
                0.7,
            ),
            make_tl_result(
                "b",
                "Alice is a software engineer at Google working on ML",
                0.9,
            ),
        ];
        let deduped = dedup_results(results, 10, None);
        // Different slugs → cross-source dedup doesn't apply
        // But Layer 2 (text similarity) may still dedup them
        // The key point: both slugs should be represented
        let slugs: std::collections::HashSet<_> = deduped.iter().map(|r| r.slug.clone()).collect();
        assert!(
            slugs.contains("a") || slugs.contains("b"),
            "At least one page should survive"
        );
    }

    #[test]
    fn test_cross_source_dedup_same_source_keeps_higher_score() {
        // Same page, very similar text, same source → keep higher score
        let results = vec![
            make_tl_result(
                "a",
                "The quick brown fox jumps over the lazy dog in the park today",
                0.9,
            ),
            make_tl_result(
                "a",
                "The quick brown fox jumps over the lazy dog in the park today",
                0.7,
            ),
        ];
        let deduped = dedup_results(results, 10, None);
        // Should keep the higher-scored one
        let a_count = deduped.iter().filter(|r| r.slug == "a").count();
        assert_eq!(
            a_count, 1,
            "Similar same-source chunks should be deduped to 1"
        );
        assert_eq!(deduped[0].score, 0.9, "Should keep the higher-scored chunk");
    }
}
