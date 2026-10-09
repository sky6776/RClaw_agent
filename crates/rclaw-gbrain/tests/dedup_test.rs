//! Dedup pipeline tests

use rclaw_gbrain::search::dedup::dedup_results;
use rclaw_gbrain::types::*;

fn make_result(slug: &str, score: f64, source: Option<ChunkSource>) -> SearchResult {
    make_result_with_text(slug, &format!("content for {}", slug), score, source)
}

fn make_result_with_text(
    slug: &str,
    chunk_text: &str,
    score: f64,
    source: Option<ChunkSource>,
) -> SearchResult {
    SearchResult {
        slug: slug.to_string(),
        title: slug.to_string(),
        chunk_text: chunk_text.to_string(),
        score,
        page_id: None,
        chunk_id: None,
        chunk_index: None,
        source,
        detail_level: DetailLevel::Medium,
        page_type: None,
        stale: false,
        updated_at: None,
    }
}

#[test]
fn test_dedup_slug() {
    // Two different chunks from same page with different text (to avoid Jaccard dedup)
    let hits = vec![
        make_result_with_text(
            "people/alice",
            "Alice's background and early career in technology",
            0.9,
            None,
        ),
        make_result_with_text(
            "people/alice",
            "Alice's recent projects and portfolio companies",
            0.5,
            None,
        ),
    ];
    let results = dedup_results(hits, 10, None);
    // With cap=2 per page, both chunks from same page can survive
    assert_eq!(results.len(), 2);
    assert!((results[0].score - 0.9).abs() < 0.001);
}

#[test]
fn test_dedup_compiled_truth_preferred() {
    let hits = vec![
        make_result_with_text(
            "people/alice",
            "Alice timeline of events through the years",
            0.5,
            Some(ChunkSource::Timeline),
        ),
        make_result_with_text(
            "people/alice",
            "Alice core facts and compiled knowledge base",
            0.5,
            Some(ChunkSource::CompiledTruth),
        ),
    ];
    let results = dedup_results(hits, 10, None);
    // With cap=2 per page, both chunks survive (CT + Timeline)
    assert_eq!(results.len(), 2);
    // At least one result should be compiled_truth
    let has_ct = results
        .iter()
        .any(|r| r.source == Some(ChunkSource::CompiledTruth));
    assert!(has_ct, "Should include compiled truth chunk");
}

#[test]
fn test_dedup_limit() {
    let hits = vec![
        make_result("a", 0.9, None),
        make_result("b", 0.8, None),
        make_result("c", 0.7, None),
    ];
    let results = dedup_results(hits, 2, None);
    assert_eq!(results.len(), 2);
}

#[test]
fn test_dedup_sorted_by_score() {
    let hits = vec![
        make_result("a", 0.5, None),
        make_result("b", 0.9, None),
        make_result("c", 0.7, None),
    ];
    let results = dedup_results(hits, 10, None);
    assert_eq!(results.len(), 3);
    assert!(results[0].score >= results[1].score);
    assert!(results[1].score >= results[2].score);
}
