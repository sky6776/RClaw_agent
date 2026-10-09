//! Hybrid search: RRF fusion of keyword + vector results
//! Mirrors gbrain's src/core/search/hybrid.ts
//!
//! P1 improvements integrated:
//! - Max normalization (instead of min-max)
//! - Compiled truth boost conditional on detail level
//! - Auto-detail detection from query intent
//! - Auto-escalate (retry with detail=high when detail=low returns empty)
//! - Cosine rescore with internal max normalization of RRF scores
//! - Multi-list RRF fusion for query expansion support

use crate::engine::BrainEngine;
use crate::search::dedup::dedup_results;
use crate::search::intent::{classify_intent, detail_for_intent, Intent, QueryIntent};
use crate::search::keyword::build_fts_query;
use crate::search::two_pass::{expand_anchors, hydrate_chunks, TwoPassOpts};
use crate::search::vector::cosine_similarity;
use crate::sqlite_engine::SqliteEngine;
use crate::types::*;
use chrono::{NaiveDateTime, Utc};
use std::collections::HashMap;
use std::sync::OnceLock;
use tracing::debug;

/// Check if search debug mode is enabled via GBRAIN_SEARCH_DEBUG env var.
/// Lazily evaluated once per process (mirrors TS process.env.GBRAIN_SEARCH_DEBUG).
fn search_debug_enabled() -> bool {
    static DEBUG: OnceLock<bool> = OnceLock::new();
    *DEBUG.get_or_init(|| {
        std::env::var("GBRAIN_SEARCH_DEBUG")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false)
    })
}

/// Hybrid search options
#[derive(Debug, Clone)]
pub struct HybridOpts {
    pub rrf_k: usize,
    pub compiled_truth_boost: f64,
    pub backlink_boost: f64,
    pub dedup: bool,
    pub rrf_weight: f64,
    pub cos_weight: f64,
    /// P1-1: Recency boost half-life in days (default: 365.0).
    /// Recent pages get higher scores via time-decay: 1 / (1 + days_since_update / half_life)
    pub recency_half_life_days: f64,
    /// P1-2: Intent-type boost magnitude (default: 0.10).
    /// Entity intent boosts entity pages; time/event intent boosts pages with timeline.
    pub intent_type_boost: f64,
}

impl Default for HybridOpts {
    fn default() -> Self {
        Self {
            rrf_k: 60,
            compiled_truth_boost: 2.0,
            backlink_boost: 0.05,
            dedup: true,
            rrf_weight: 0.7,
            cos_weight: 0.3,
            recency_half_life_days: 365.0,
            intent_type_boost: 0.10,
        }
    }
}

/// P2-5: Maximum internal search limit (use types::MAX_SEARCH_LIMIT for consistency)
const MAX_SEARCH_LIMIT: usize = crate::types::MAX_SEARCH_LIMIT;

/// Perform hybrid search combining keyword and vector results with RRF fusion.
/// Returns results with metadata about what the search pipeline actually did
/// (mirrors TS HybridSearchMeta via onMeta callback).
pub fn hybrid_search(
    engine: &SqliteEngine,
    query: &str,
    embedding: Option<&[f32]>,
    mut opts: SearchOpts,
    hybrid_opts: HybridOpts,
) -> Result<SearchResultWithMeta, crate::error::GBrainError> {
    let limit = opts.limit.unwrap_or(20);
    // R3-01 fix: Account for offset when computing internal limit and dedup pool size.
    // Without this, pagination (offset > 0) returns fewer results than requested
    // because dedup truncates to `limit` before offset is applied, discarding
    // the first `offset` results that should be skipped.
    let offset = opts.offset.unwrap_or(0);
    let effective_limit = limit + offset;

    // Guard: empty/whitespace query produces empty results with keyword-only meta
    if query.trim().is_empty() {
        debug!("Empty query, returning empty results");
        return Ok(SearchResultWithMeta {
            results: Vec::new(),
            meta: SearchMeta {
                vector_enabled: false,
                detail_resolved: opts.detail_level,
                expansion_applied: false,
            },
        });
    }

    // P1-3: Auto-detail detection — when detail_level not specified,
    // classify query intent and derive detail level (mirrors TS)
    let intent = classify_intent(query);
    let detail = opts
        .detail_level
        .or_else(|| detail_for_intent(&intent.intent));

    debug!(query = %query, has_embedding = embedding.is_some(), limit, offset, detail = ?detail, "Starting hybrid search");

    // Track metadata about what the search pipeline actually did (mirrors TS HybridSearchMeta)
    let expansion_applied = opts
        .expanded_queries
        .as_ref()
        .map(|eq| eq.len() > 1)
        .unwrap_or(false);
    let vector_enabled = embedding.is_some();

    // P2-5: Cap internal limit to MAX_SEARCH_LIMIT (mirrors TS Math.min(limit * 2, MAX_SEARCH_LIMIT))
    // R3-01 fix: Use effective_limit (limit + offset) so the pool is large enough
    // to skip offset results and still return limit results.
    let internal_limit = (effective_limit * 2).min(MAX_SEARCH_LIMIT);

    // 1. Keyword search via FTS5
    let fts_query = build_fts_query(query);
    let keyword_results = engine.search_keyword(
        &fts_query,
        SearchOpts {
            limit: Some(internal_limit),
            detail_level: detail,
            ..opts.clone()
        },
    )?;
    debug!(
        keyword_count = keyword_results.len(),
        "FTS5 keyword search complete"
    );

    // 2. Vector search (if embedding provided)
    let vector_results = if let Some(emb) = embedding {
        engine.search_vector(
            emb,
            SearchOpts {
                limit: Some(internal_limit),
                detail_level: detail,
                ..opts.clone()
            },
        )?
    } else {
        Vec::new()
    };
    debug!(
        vector_count = vector_results.len(),
        "Vector search complete"
    );

    // P2-1: Search fallback strategy — when vector returns <3 results,
    // broaden the keyword query by expanding into OR terms and run a
    // second FTS5 pass. The fallback results are fused with half RRF weight
    // to avoid overwhelming the primary keyword results.
    let fallback_keyword_results = if vector_results.len() < 3 {
        let broadened = broaden_fts_query(query);
        if broadened != fts_query {
            let fb_results = engine.search_keyword(
                &broadened,
                SearchOpts {
                    limit: Some(internal_limit),
                    detail_level: detail,
                    ..opts.clone()
                },
            )?;
            debug!(
                fallback_count = fb_results.len(),
                "P2-1 fallback keyword search (broadened OR query)"
            );
            Some(fb_results)
        } else {
            None
        }
    } else {
        None
    };

    // 3. RRF fusion
    // When expanded queries provided, run vector search for each
    // expanded query with its own embedding (mirrors TS: independent embeddings).
    // Falls back to reusing the original query embedding if no expanded_embeddings.
    let mut fused = match (&opts.expanded_queries, embedding) {
        (Some(expanded), Some(query_emb)) => {
            // Build result lists: original keyword + vector + expanded results
            let mut all_lists: Vec<Vec<SearchResult>> = vec![keyword_results, vector_results];

            let expanded_embs: Vec<Option<&[f32]>> = match &opts.expanded_embeddings {
                Some(embs) => embs.iter().map(|e| Some(e.as_slice())).collect(),
                None => vec![None; expanded.len()], // fallback: reuse query_emb
            };

            for (i, exp_query) in expanded.iter().enumerate() {
                if exp_query == query {
                    continue; // Skip original query (already searched)
                }
                // Use per-query embedding when available, fall back to original
                let exp_emb = expanded_embs.get(i).and_then(|e| *e).unwrap_or(query_emb);
                let exp_vector = engine.search_vector(
                    exp_emb,
                    SearchOpts {
                        limit: Some(internal_limit),
                        detail_level: detail,
                        ..opts.clone()
                    },
                )?;

                if search_debug_enabled() {
                    debug!(
                        "exp_query=\"{}\" exp_emb_idx={} has_own_emb={}",
                        exp_query,
                        i,
                        expanded_embs.get(i).and_then(|e| *e).is_some()
                    );
                }

                // P0 fix: only push vector list per expanded query (matches TS: [...vectorLists, keywordResults])
                all_lists.push(exp_vector);
            }

            // P2-1: Append fallback keyword results with half-weight marker
            // We add them as a separate list so RRF naturally gives them lower weight
            // (they appear once vs primary keyword results which also appear once,
            //  but the fallback terms are broader so they contribute less per-hit).
            if let Some(ref fb_results) = fallback_keyword_results {
                all_lists.push(fb_results.clone());
            }

            rrf_fuse_multi(
                &all_lists,
                hybrid_opts.rrf_k,
                detail != Some(DetailLevel::High),
                hybrid_opts.compiled_truth_boost,
            )
        }
        _ => {
            // P2-1: If we have fallback keyword results, use multi-list RRF
            // with half-weight by including fallback as a separate list
            if let Some(ref fb_results) = fallback_keyword_results {
                let all_lists: Vec<Vec<SearchResult>> =
                    vec![keyword_results, vector_results, fb_results.clone()];
                rrf_fuse_multi(
                    &all_lists,
                    hybrid_opts.rrf_k,
                    detail != Some(DetailLevel::High),
                    hybrid_opts.compiled_truth_boost,
                )
            } else {
                rrf_fuse(&keyword_results, &vector_results, hybrid_opts.rrf_k)
            }
        }
    };

    // 4. Normalize RRF scores to [0, 1] — max normalization (mirrors TS)
    normalize_scores(&mut fused);

    // Debug: after RRF fusion + normalization
    if search_debug_enabled() {
        for r in &fused {
            debug!(
                "rrf {}:{} rrf_norm={:.6} source={:?}",
                r.slug,
                r.chunk_id.map_or("?".to_string(), |id| id.to_string()),
                r.score,
                r.source
            );
        }
    }

    // 5. Compiled truth boost (multiplicative) — skip when detail=high
    // (mirrors TS: temporal/event queries want natural ranking)
    apply_compiled_truth_boost(&mut fused, hybrid_opts.compiled_truth_boost, detail);

    // Debug: after compiled truth boost
    if search_debug_enabled() {
        for r in &fused {
            debug!(
                "boost {}:{} boosted={:.6} source={:?}",
                r.slug,
                r.chunk_id.map_or("?".to_string(), |id| id.to_string()),
                r.score,
                r.source
            );
        }
    }

    // 6. Cosine re-score: blend normalized RRF with cosine similarity
    if let Some(query_emb) = embedding {
        let chunk_ids: Vec<i64> = fused.iter().filter_map(|r| r.chunk_id).collect();
        if !chunk_ids.is_empty() {
            let embeddings = engine.get_embeddings_by_chunk_ids(&chunk_ids)?;
            cosine_rescore(
                &mut fused,
                &embeddings,
                query_emb,
                hybrid_opts.rrf_weight,
                hybrid_opts.cos_weight,
            );

            // Debug: after cosine rescore
            if search_debug_enabled() {
                for r in &fused {
                    debug!(
                        "cosine {}:{} blended={:.6}",
                        r.slug,
                        r.chunk_id.map_or("?".to_string(), |id| id.to_string()),
                        r.score
                    );
                }
            }
        }
    }

    // 7. Backlink boost (multiplicative)
    // P2-10: Only fetch backlink counts for result slugs, not all slugs
    let result_slugs: Vec<String> = fused.iter().map(|r| r.slug.clone()).collect();
    let backlink_counts = engine.get_backlink_counts(&result_slugs)?;
    apply_backlink_boost(&mut fused, &backlink_counts, hybrid_opts.backlink_boost);

    // 8. P1-1: Recency boost — time-decay factor based on updated_at
    // Recent pages get higher scores: 1 / (1 + days_since_update / half_life)
    // Scaled to ~0.05 max additive boost to avoid overwhelming other signals.
    apply_recency_boost(&mut fused, hybrid_opts.recency_half_life_days);

    // Debug: after recency boost
    if search_debug_enabled() {
        for r in &fused {
            debug!(
                "recency {}:{} score={:.6} updated_at={:?}",
                r.slug,
                r.chunk_id.map_or("?".to_string(), |id| id.to_string()),
                r.score,
                r.updated_at
            );
        }
    }

    // 9. P1-2: Intent-type boost — boost results matching query intent type
    // Entity intent → boost Person/Company pages; Time/Event intent → boost timeline pages
    apply_intent_type_boost(&mut fused, &intent, hybrid_opts.intent_type_boost);

    // Debug: after intent-type boost
    if search_debug_enabled() {
        for r in &fused {
            debug!(
                "intent_boost {}:{} score={:.6} page_type={:?}",
                r.slug,
                r.chunk_id.map_or("?".to_string(), |id| id.to_string()),
                r.score,
                r.page_type
            );
        }
    }

    // Debug: final scores after all boosts
    if search_debug_enabled() {
        for r in &fused {
            debug!(
                "final {}:{} score={:.6} source={:?} page_type={:?}",
                r.slug,
                r.chunk_id.map_or("?".to_string(), |id| id.to_string()),
                r.score,
                r.source,
                r.page_type
            );
        }
    }

    // 10. Two-pass code graph expansion (mirrors TS Cathedral II)
    // Expand anchor set through code_edges, collecting structural neighbors
    // with hop-distance score decay. Best-effort: errors must not break base retrieval.
    let walk_depth = opts
        .walk_depth
        .unwrap_or(0)
        .min(crate::search::two_pass::MAX_WALK_DEPTH);
    let needs_expansion = walk_depth > 0 || opts.near_symbol.is_some();
    if needs_expansion {
        let anchor_set: Vec<SearchResult> = fused
            .iter()
            .take(std::cmp::max(10, limit))
            .cloned()
            .collect();
        let two_pass_opts = TwoPassOpts {
            walk_depth,
            near_symbol: opts.near_symbol.clone(),
        };
        // Best-effort: errors in expansion should not break base retrieval
        if let Ok(expanded) = expand_anchors(engine, &anchor_set, &two_pass_opts) {
            if let Ok(new_results) = hydrate_chunks(engine, &expanded, &fused) {
                if !new_results.is_empty() {
                    debug!(
                        new_neighbor_count = new_results.len(),
                        "Two-pass expansion added structural neighbors"
                    );
                    fused.extend(new_results);
                    fused.sort_by(|a, b| {
                        b.score
                            .partial_cmp(&a.score)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    // Widen dedup cap for structural neighbors
                    // max_per_page = min(10, max(walk_depth * 5, 5))
                    if let Some(ref mut dedup_opts) = opts.dedup_opts {
                        let expanded_cap = (walk_depth * 5).clamp(5, 10);
                        if expanded_cap > dedup_opts.max_per_page {
                            dedup_opts.max_per_page = expanded_cap;
                        }
                    }
                }
            }
        }
    }

    // 11. Dedup
    // P2-6: Pass dedup_opts from SearchOpts (mirrors TS opts?.dedupOpts)
    // R3-01 fix: Use effective_limit (limit + offset) so dedup retains enough
    // candidates for the offset to skip over before returning `limit` results.
    if hybrid_opts.dedup {
        fused = dedup_results(fused, effective_limit, opts.dedup_opts.clone());
    } else {
        fused.truncate(effective_limit);
    }

    // P1-12: Apply offset for pagination (mirrors TS)
    // R3-01 fix: offset is already computed above from opts.offset
    if offset > 0 && offset < fused.len() {
        fused = fused.split_off(offset);
    } else if offset > 0 && offset >= fused.len() {
        fused.clear();
    }
    // Ensure we don't return more than the requested limit
    fused.truncate(limit);

    // P1-3: Auto-escalate — if results empty and detail=low, retry with detail=high
    // Reset offset on retry since the first attempt already applied it and returned empty
    if fused.is_empty() && detail == Some(DetailLevel::Low) {
        debug!("Auto-escalating: detail=low returned 0 results, retrying with detail=high");
        let escalated_opts = SearchOpts {
            detail_level: Some(DetailLevel::High),
            offset: None,
            ..opts
        };
        return hybrid_search(engine, query, embedding, escalated_opts, hybrid_opts);
    }

    debug!(result_count = fused.len(), "Hybrid search complete");
    Ok(SearchResultWithMeta {
        results: fused,
        meta: SearchMeta {
            vector_enabled,
            detail_resolved: detail,
            expansion_applied,
        },
    })
}

/// Build a chunk-level RRF key: `slug:chunk_id` or `slug:chunk_text_prefix`
/// This preserves chunk granularity in RRF fusion (mirrors TS behavior).
fn rrf_key(result: &SearchResult) -> String {
    match result.chunk_id {
        Some(cid) => format!("{}:{}", result.slug, cid),
        None => {
            // Char-boundary-safe prefix to avoid panic on multi-byte UTF-8
            let prefix: String = result.chunk_text.chars().take(50).collect();
            format!("{}:{}", result.slug, prefix)
        }
    }
}

/// Reciprocal Rank Fusion of keyword and vector results.
/// Uses `1.0 / (k + rank)` — matches TS and rrf_fuse_multi (P0 fix).
fn rrf_fuse(
    keyword_results: &[SearchResult],
    vector_results: &[SearchResult],
    k: usize,
) -> Vec<SearchResult> {
    let mut scores: HashMap<String, (f64, SearchResult)> = HashMap::new();

    // Score keyword results
    for (rank, result) in keyword_results.iter().enumerate() {
        let rrf_score = 1.0 / (k as f64 + rank as f64);
        let key = rrf_key(result);
        scores
            .entry(key)
            .and_modify(|(s, r)| {
                *s += rrf_score;
                if result.score > r.score {
                    *r = result.clone();
                }
            })
            .or_insert_with(|| (rrf_score, result.clone()));
    }

    // Score vector results
    for (rank, result) in vector_results.iter().enumerate() {
        let rrf_score = 1.0 / (k as f64 + rank as f64);
        let key = rrf_key(result);
        scores
            .entry(key)
            .and_modify(|(s, r)| {
                *s += rrf_score;
                if result.score > r.score {
                    *r = result.clone();
                }
            })
            .or_insert_with(|| (rrf_score, result.clone()));
    }

    // Sort by fused score
    let mut results: Vec<SearchResult> = scores
        .into_values()
        .map(|(score, mut result)| {
            result.score = score;
            result
        })
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

/// P1-4: Multi-list RRF fusion for query expansion support
/// Accepts multiple result lists and fuses them all.
/// Each list gets weight 1.0 (equal contribution), matching TS rrfFusion behavior.
///
/// Note: compiled_truth boost is NOT applied here — it is applied once after
/// normalization by `apply_compiled_truth_boost()` to avoid double-boosting.
pub fn rrf_fuse_multi(
    lists: &[Vec<SearchResult>],
    k: usize,
    _apply_boost: bool,
    _boost: f64,
) -> Vec<SearchResult> {
    let mut scores: HashMap<String, (f64, SearchResult)> = HashMap::new();

    for list in lists {
        for (rank, result) in list.iter().enumerate() {
            let rrf_score = 1.0 / (k as f64 + rank as f64);
            let key = rrf_key(result);
            scores
                .entry(key)
                .and_modify(|(s, r)| {
                    *s += rrf_score;
                    if result.score > r.score {
                        *r = result.clone();
                    }
                })
                .or_insert_with(|| (rrf_score, result.clone()));
        }
    }

    let mut results: Vec<SearchResult> = scores
        .into_values()
        .map(|(score, mut result)| {
            result.score = score;
            result
        })
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

/// P1-2: Boost results from compiled_truth source (multiplicative)
/// Skip boost when detail == High (mirrors TS: temporal/event queries want natural ranking)
fn apply_compiled_truth_boost(
    results: &mut [SearchResult],
    boost: f64,
    detail: Option<DetailLevel>,
) {
    if detail == Some(DetailLevel::High) {
        return; // Skip boost for detail=high queries
    }
    for result in results.iter_mut() {
        if result.source.as_ref() == Some(&ChunkSource::CompiledTruth) {
            result.score *= boost;
        }
    }
}

/// P1-1: Normalize scores to [0, 1] range using max normalization
/// (mirrors TS: score / maxScore, preserves relative spacing)
/// Handles edge cases: all-zero scores, all-negative scores
fn normalize_scores(results: &mut [SearchResult]) {
    if results.is_empty() {
        return;
    }
    let max_score = results
        .iter()
        .map(|r| r.score)
        .fold(f64::NEG_INFINITY, f64::max);
    if max_score > 0.0 {
        for r in results.iter_mut() {
            r.score /= max_score;
        }
    } else if max_score == 0.0 {
        // M42 修复：所有 RRF 分数为零时，说明候选完全无意义，
        // 均匀分布会产生极小正值（如 1/N），干扰下游排序。
        // 直接将所有分数置零，让调用方按零分处理（通常被过滤掉）。
        for r in results.iter_mut() {
            r.score = 0.0;
        }
    } else {
        // All scores are negative — shift to [0, 1] preserving order
        // (less negative = better, so min_score maps to 0, max_score maps to 1)
        let min_score = results
            .iter()
            .map(|r| r.score)
            .fold(f64::INFINITY, f64::min);
        let range = max_score - min_score; // both negative, max > min, so range > 0
        if range > 0.0 {
            for r in results.iter_mut() {
                r.score = ((r.score - min_score) / range).clamp(0.0, 1.0);
            }
        } else {
            // All scores are the same negative value — assign uniform
            let uniform = 1.0 / results.len() as f64;
            for r in results.iter_mut() {
                r.score = uniform;
            }
        }
    }
}

/// P1-extra: Cosine re-score: blend normalized RRF with cosine similarity
/// Mirrors TS: first normalize RRF scores by max, then blend with cosine
/// final_score = rrf_weight * (rrf / maxRrf) + cos_weight * cosine_sim
fn cosine_rescore(
    results: &mut [SearchResult],
    embeddings: &[(i64, Vec<f32>)],
    query_embedding: &[f32],
    rrf_weight: f64,
    cos_weight: f64,
) {
    let emb_map: std::collections::HashMap<i64, &Vec<f32>> =
        embeddings.iter().map(|(id, emb)| (*id, emb)).collect();

    // Scores are already normalized to [0,1] by normalize_scores() at step 4.
    // No re-normalization here — that would negate the compiled truth boost
    // applied at step 5 (the boosted max would deflate non-CT results).
    for result in results.iter_mut() {
        if let Some(chunk_id) = result.chunk_id {
            if let Some(emb) = emb_map.get(&chunk_id) {
                let cos = cosine_similarity(query_embedding, emb).clamp(0.0, 1.0) as f64;
                result.score = rrf_weight * result.score + cos_weight * cos;
            } else {
                // No embedding for this chunk — scale to match blend range
                result.score *= rrf_weight;
            }
        } else {
            // No chunk_id — scale to match blend range
            result.score *= rrf_weight;
        }
    }
}

/// Boost results based on backlink count (multiplicative)
/// Mirrors TS: score *= (1 + coef * ln(1 + count))
/// Multiplicative boost preserves relative ordering better than additive.
fn apply_backlink_boost(results: &mut [SearchResult], counts: &HashMap<String, i64>, coef: f64) {
    for result in results.iter_mut() {
        if let Some(&count) = counts.get(&result.slug) {
            let factor = 1.0 + coef * (1.0 + count as f64).ln();
            result.score *= factor;
        }
    }
}

/// P1-1: Recency boost — additive time-decay factor based on updated_at.
/// Mirrors TS: recencyBoost = 1 / (1 + daysSinceUpdate / halfLife)
/// The raw recency factor is scaled to ~0.05 max additive contribution
/// so it doesn't overwhelm other ranking signals.
fn apply_recency_boost(results: &mut [SearchResult], half_life_days: f64) {
    if half_life_days <= 0.0 {
        return;
    }
    let max_boost = 0.05; // cap additive contribution
    for result in results.iter_mut() {
        if let Some(ref updated_at_str) = result.updated_at {
            // SQLite datetime('now') produces "YYYY-MM-DD HH:MM:SS" (no T/Z),
            // so we parse with NaiveDateTime and assume UTC.
            let updated = NaiveDateTime::parse_from_str(updated_at_str, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|naive| naive.and_utc());
            if let Some(updated) = updated {
                let days_since = (Utc::now() - updated).num_days().max(0) as f64;
                let recency = 1.0 / (1.0 + days_since / half_life_days);
                result.score += max_boost * recency;
            }
        }
    }
}

/// P1-2: Intent-type boost — boost results whose page_type matches query intent.
/// Mirrors TS: entity intent boosts Person/Company pages; time/event intent
/// boosts pages with timeline source chunks.
/// Additive boost of `magnitude` (default 0.10) preserves score ordering.
fn apply_intent_type_boost(results: &mut [SearchResult], intent: &QueryIntent, magnitude: f64) {
    if magnitude <= 0.0 {
        return;
    }
    match &intent.intent {
        Intent::Entity => {
            // Boost Person and Company pages
            for result in results.iter_mut() {
                if matches!(
                    result.page_type,
                    Some(PageType::Person) | Some(PageType::Company)
                ) {
                    result.score += magnitude;
                }
            }
        }
        Intent::Temporal | Intent::Event => {
            // Boost timeline source chunks (they contain temporal data)
            for result in results.iter_mut() {
                if result.source == Some(ChunkSource::Timeline) {
                    result.score += magnitude;
                }
            }
        }
        Intent::General => {
            // No type-specific boost for general intent
        }
    }
}

/// P2-1: 放宽 FTS 查询，将词项以 OR 连接。
/// 当向量搜索返回结果较少时，将关键词查询从 AND（所有词项必须匹配）
/// 放宽为 OR（任一词项匹配即可）。
/// 对应 TS: fallbackStrategy = "broaden_or"
fn broaden_fts_query(query: &str) -> String {
    // 使用 jieba 分词构建 FTS5 查询，支持中文分词
    crate::nlp::chinese::build_fts_match_query(query)
}
