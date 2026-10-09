//! Cathedral II two-pass structural retrieval
//! Mirrors gbrain's src/core/search/two-pass.ts
//!
//! Expands an anchor set of search results through code_edges,
//! collecting structural neighbors with hop-distance score decay.
//! Best-effort: errors in expansion must not break base hybrid retrieval.

use crate::engine::BrainEngine;
use crate::error::GBrainError;
use crate::sqlite_engine::SqliteEngine;
use crate::types::*;
use std::collections::HashMap;
use tracing::{debug, trace, warn};

/// Maximum neighborhood blast radius cap (hops).
pub const MAX_WALK_DEPTH: usize = 2;

/// High-fan-out protection: cap on neighbors collected per hop level.
pub const NEIGHBOR_CAP_PER_HOP: usize = 50;

/// Options controlling two-pass code graph expansion.
#[derive(Debug, Clone)]
pub struct TwoPassOpts {
    /// Walk depth for BFS expansion: 0=off, 1-2=expand N hops.
    /// Capped at MAX_WALK_DEPTH (2).
    pub walk_depth: usize,
    /// Qualified symbol name to anchor at (e.g., "module::function").
    /// If set, finds chunks matching this symbol and adds them as
    /// additional anchor seeds before BFS expansion.
    pub near_symbol: Option<String>,
}

/// A chunk discovered during two-pass expansion, with metadata
/// about how it was found and its decayed score.
#[derive(Debug, Clone)]
pub struct ExpandedChunk {
    pub chunk_id: i64,
    pub score: f64,
    /// Hop distance from the nearest anchor (0 = anchor itself)
    pub hop: usize,
    /// Origin: "anchor" for seed results, "neighbor" for graph-walk results
    pub source: String,
}

/// Internal tracking entry for BFS frontier.
#[derive(Debug, Clone)]
struct FrontierEntry {
    chunk_id: i64,
    score: f64,
}

/// Expand anchor search results through the code_edges graph.
///
/// Algorithm:
/// 1. If walk_depth=0 and no near_symbol, return anchors as-is.
/// 2. Seed `seen` map from anchor SearchResult[] (hop=0, source='anchor').
/// 3. --near-symbol expansion: query chunks WHERE symbol_name = $1 LIMIT 50
///    to find additional anchor chunks.
/// 4. Walk N hops (frontier BFS):
///    - For each hop from 1..depth:
///      - Compute decay = 1 / (1 + hop)
///      - For each chunk in frontier: get edges, resolve neighbor chunk IDs
///      - For unresolved edges (to_symbol set but to_chunk_id null):
///        query chunks WHERE symbol_name matches to find chunk IDs
///      - For each resolved neighbor chunk ID not yet seen:
///        - Score = current.score * decay
///        - Add to seen and next_frontier
/// 5. Return all chunks from seen map.
pub fn expand_anchors(
    engine: &SqliteEngine,
    anchors: &[SearchResult],
    opts: &TwoPassOpts,
) -> Result<Vec<ExpandedChunk>, GBrainError> {
    let walk_depth = opts.walk_depth.min(MAX_WALK_DEPTH);

    // Step 1: If no expansion needed, return anchors as-is
    if walk_depth == 0 && opts.near_symbol.is_none() {
        return Ok(anchors
            .iter()
            .filter_map(|r| {
                r.chunk_id.map(|chunk_id| ExpandedChunk {
                    chunk_id,
                    score: r.score,
                    hop: 0,
                    source: "anchor".to_string(),
                })
            })
            .collect());
    }

    // Determine the default score for near-symbol seeds:
    // Use the top anchor score, or 1.0 if no anchors exist
    let default_score = anchors.first().map(|r| r.score).unwrap_or(1.0);

    // Step 2: Seed `seen` map from anchor SearchResult[]
    let mut seen: HashMap<i64, ExpandedChunk> = HashMap::new();
    let mut frontier: Vec<FrontierEntry> = Vec::new();

    for anchor in anchors {
        if let Some(chunk_id) = anchor.chunk_id {
            if seen.contains_key(&chunk_id) {
                // Keep the higher score if seen twice
                let entry = seen.get_mut(&chunk_id).unwrap();
                if anchor.score > entry.score {
                    entry.score = anchor.score;
                }
                continue;
            }
            seen.insert(
                chunk_id,
                ExpandedChunk {
                    chunk_id,
                    score: anchor.score,
                    hop: 0,
                    source: "anchor".to_string(),
                },
            );
            frontier.push(FrontierEntry {
                chunk_id,
                score: anchor.score,
            });
        }
    }

    // Step 3: --near-symbol expansion
    if let Some(ref symbol) = opts.near_symbol {
        match engine.get_chunks_by_symbol(symbol, NEIGHBOR_CAP_PER_HOP) {
            Ok(chunks) => {
                let added_count = chunks.len();
                for chunk in &chunks {
                    if seen.contains_key(&chunk.id) {
                        continue;
                    }
                    seen.insert(
                        chunk.id,
                        ExpandedChunk {
                            chunk_id: chunk.id,
                            score: default_score,
                            hop: 0,
                            source: "anchor".to_string(),
                        },
                    );
                    frontier.push(FrontierEntry {
                        chunk_id: chunk.id,
                        score: default_score,
                    });
                }
                debug!(
                    symbol = %symbol,
                    added = added_count,
                    "Near-symbol expansion seeded additional anchors"
                );
            }
            Err(e) => {
                // Best-effort: log but don't fail
                warn!(
                    symbol = %symbol,
                    error = %e,
                    "Near-symbol expansion failed, skipping"
                );
            }
        }
    }

    // Step 4: Walk N hops (frontier BFS)
    for hop in 1..=walk_depth {
        let decay = 1.0 / (1.0 + hop as f64);
        let mut next_frontier: Vec<FrontierEntry> = Vec::new();
        let mut neighbors_this_hop = 0;

        for entry in &frontier {
            if neighbors_this_hop >= NEIGHBOR_CAP_PER_HOP {
                break;
            }

            // Get edges for this chunk
            let edges = match engine.get_edges_by_chunk(entry.chunk_id) {
                Ok(edges) => edges,
                Err(e) => {
                    trace!(
                        chunk_id = entry.chunk_id,
                        error = %e,
                        "get_edges_by_chunk failed, skipping"
                    );
                    continue;
                }
            };

            for edge in &edges {
                if neighbors_this_hop >= NEIGHBOR_CAP_PER_HOP {
                    break;
                }

                // Resolve neighbor chunk IDs from the edge.
                let neighbor_chunk_ids = resolve_neighbor_ids(engine, entry.chunk_id, edge);

                for neighbor_id in neighbor_chunk_ids {
                    if neighbors_this_hop >= NEIGHBOR_CAP_PER_HOP {
                        break;
                    }
                    if seen.contains_key(&neighbor_id) {
                        continue;
                    }

                    let neighbor_score = entry.score * decay;
                    seen.insert(
                        neighbor_id,
                        ExpandedChunk {
                            chunk_id: neighbor_id,
                            score: neighbor_score,
                            hop,
                            source: "neighbor".to_string(),
                        },
                    );
                    next_frontier.push(FrontierEntry {
                        chunk_id: neighbor_id,
                        score: neighbor_score,
                    });
                    neighbors_this_hop += 1;
                }
            }

            // Also check unresolved edges from code_edges_symbol table.
            // These are forward-declaration edges where the target chunk
            // was not yet imported when the source was indexed.
            if neighbors_this_hop < NEIGHBOR_CAP_PER_HOP {
                let unresolved_ids = resolve_unresolved_symbol_edges(engine, entry.chunk_id);
                for neighbor_id in unresolved_ids {
                    if neighbors_this_hop >= NEIGHBOR_CAP_PER_HOP {
                        break;
                    }
                    if seen.contains_key(&neighbor_id) {
                        continue;
                    }

                    let neighbor_score = entry.score * decay;
                    seen.insert(
                        neighbor_id,
                        ExpandedChunk {
                            chunk_id: neighbor_id,
                            score: neighbor_score,
                            hop,
                            source: "neighbor".to_string(),
                        },
                    );
                    next_frontier.push(FrontierEntry {
                        chunk_id: neighbor_id,
                        score: neighbor_score,
                    });
                    neighbors_this_hop += 1;
                }
            }
        }

        debug!(
            hop,
            neighbors_found = neighbors_this_hop,
            frontier_size = next_frontier.len(),
            "BFS hop complete"
        );

        frontier = next_frontier;
        if frontier.is_empty() {
            break; // No more neighbors to explore
        }
    }

    let result: Vec<ExpandedChunk> = seen.into_values().collect();
    debug!(
        total_expanded = result.len(),
        walk_depth, "Two-pass expansion complete"
    );
    Ok(result)
}

/// Resolve neighbor chunk IDs from a code edge.
///
/// Given the current chunk_id and an edge, determine which chunk(s) are
/// on the other side. Handles both direct chunk_id links and unresolved
/// edges where only the symbol name is known.
fn resolve_neighbor_ids(engine: &SqliteEngine, current_chunk_id: i64, edge: &CodeEdge) -> Vec<i64> {
    let mut neighbors = Vec::new();

    // Case 1: Current chunk is the from_chunk -> neighbor is to_chunk_id
    if edge.from_chunk_id == Some(current_chunk_id) {
        if let Some(to_id) = edge.to_chunk_id {
            // Direct link: neighbor resolved
            neighbors.push(to_id);
        } else if !edge.to_symbol.is_empty() {
            // Unresolved edge: try to find chunk by symbol name.
            // The to_symbol field contains the qualified symbol name.
            if let Ok(chunks) = engine.get_chunks_by_symbol(&edge.to_symbol, 5) {
                for chunk in chunks {
                    neighbors.push(chunk.id);
                }
            }
        }
    }

    // Case 2: Current chunk is the to_chunk -> neighbor is from_chunk_id
    if edge.to_chunk_id == Some(current_chunk_id) {
        if let Some(from_id) = edge.from_chunk_id {
            // Direct link: neighbor resolved
            if !neighbors.contains(&from_id) {
                neighbors.push(from_id);
            }
        }
        // from_chunk_id is always set in the current schema (NOT NULL),
        // so no symbol-resolution fallback needed for the from side.
    }

    neighbors
}

/// Resolve unresolved edges from the code_edges_symbol table.
///
/// When a code page is imported before its dependencies, edges are stored
/// in code_edges_symbol with only to_symbol_qualified (no to_chunk_id).
/// This function queries that table and attempts to resolve the symbols
/// to actual chunk IDs.
fn resolve_unresolved_symbol_edges(engine: &SqliteEngine, chunk_id: i64) -> Vec<i64> {
    let mut neighbors = Vec::new();

    match engine.get_unresolved_edges_from(chunk_id) {
        Ok(edges) => {
            for (to_symbol, _edge_type) in &edges {
                if let Ok(chunks) = engine.get_chunks_by_symbol(to_symbol, 5) {
                    for chunk in chunks {
                        if !neighbors.contains(&chunk.id) {
                            neighbors.push(chunk.id);
                        }
                    }
                }
            }
        }
        Err(e) => {
            trace!(
                chunk_id,
                error = %e,
                "get_unresolved_edges_from failed, skipping"
            );
        }
    }

    neighbors
}

/// Convert expanded chunk IDs into full SearchResult rows.
///
/// This hydrates new neighbors that weren't in the original hybrid search
/// results by joining chunks with pages to get slug, title, page_type, etc.
///
/// Returns only the NEW results that are not already in existing_results.
pub fn hydrate_chunks(
    engine: &SqliteEngine,
    expanded: &[ExpandedChunk],
    existing_results: &[SearchResult],
) -> Result<Vec<SearchResult>, GBrainError> {
    // Step 1: Collect existing chunk_ids to skip
    let existing_chunk_ids: std::collections::HashSet<i64> =
        existing_results.iter().filter_map(|r| r.chunk_id).collect();

    // Step 2: Find chunk IDs that need hydration (not in existing results)
    let needs_hydration: Vec<i64> = expanded
        .iter()
        .filter(|ec| !existing_chunk_ids.contains(&ec.chunk_id))
        .map(|ec| ec.chunk_id)
        .collect();

    if needs_hydration.is_empty() {
        return Ok(Vec::new());
    }

    // Build a score map from expanded chunks (keyed by chunk_id)
    let score_map: HashMap<i64, f64> = expanded.iter().map(|ec| (ec.chunk_id, ec.score)).collect();

    // Step 3: Query each chunk by ID and join with page data
    let mut new_results: Vec<SearchResult> = Vec::new();

    for chunk_id in &needs_hydration {
        // Use get_chunk_by_id to retrieve the chunk, then get_page for parent data
        match engine.get_chunk_by_id(*chunk_id) {
            Ok(Some(chunk)) => match engine.get_page(&chunk.slug) {
                Ok(Some(page)) => {
                    let score = score_map.get(chunk_id).copied().unwrap_or(0.1);
                    let chunk_source = chunk.source.clone();
                    let page_type = page.page_type.clone();

                    new_results.push(SearchResult {
                        slug: chunk.slug.clone(),
                        title: page.title,
                        chunk_text: chunk.chunk_text,
                        score,
                        page_id: Some(chunk.page_id),
                        chunk_id: Some(chunk.id),
                        chunk_index: Some(chunk.chunk_index),
                        source: Some(chunk_source),
                        detail_level: DetailLevel::Medium,
                        page_type: Some(page_type),
                        stale: false,
                        updated_at: Some(page.updated_at),
                    });
                }
                Ok(None) => {
                    trace!(chunk_id, "Page not found during hydration, skipping");
                }
                Err(e) => {
                    trace!(chunk_id, error = %e, "Page lookup failed during hydration");
                }
            },
            Ok(None) => {
                trace!(chunk_id, "Chunk not found during hydration, skipping");
            }
            Err(e) => {
                trace!(chunk_id, error = %e, "Chunk hydration failed, skipping");
            }
        }
    }

    debug!(
        hydrated_count = new_results.len(),
        total_expanded = expanded.len(),
        existing_count = existing_results.len(),
        "Hydrated new neighbor chunks"
    );

    Ok(new_results)
}
