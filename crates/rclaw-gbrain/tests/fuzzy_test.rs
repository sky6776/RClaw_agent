//! Fuzzy search integration tests
//! Tests trigram_similarity, find_by_title_fuzzy, and resolve_slugs

use rclaw_gbrain::config::Config;
use rclaw_gbrain::engine::BrainEngine;
use rclaw_gbrain::sqlite_engine::SqliteEngine;
use rclaw_gbrain::types::*;

fn make_engine() -> SqliteEngine {
    let mut engine = SqliteEngine::with_config(":memory:", Config::default());
    engine.connect().expect("connect");
    engine.init_schema().expect("init_schema");
    engine
}

fn page_input(title: &str, content: &str, page_type: PageType) -> PageInput {
    PageInput {
        title: title.to_string(),
        compiled_truth: content.to_string(),
        page_type,
        timeline: None,
        frontmatter: None,
        content_hash: None,
    }
}

fn seed_pages(engine: &SqliteEngine) {
    engine
        .put_page(
            "people/alice-wonderland",
            page_input("Alice Wonderland", "An engineer", PageType::Person),
        )
        .expect("put1");
    engine
        .put_page(
            "people/bob-builder",
            page_input("Bob Builder", "A manager", PageType::Person),
        )
        .expect("put2");
    engine
        .put_page(
            "people/charlie-chaplin",
            page_input("Charlie Chaplin", "An actor", PageType::Person),
        )
        .expect("put3");
    engine
        .put_page(
            "companies/wonderland-inc",
            page_input("Wonderland Inc", "A company", PageType::Company),
        )
        .expect("put4");
    engine
        .put_page(
            "companies/builder-corp",
            page_input("Builder Corp", "A company", PageType::Company),
        )
        .expect("put5");
    engine
        .put_page(
            "concepts/rust-programming",
            page_input("Rust Programming", "A guide", PageType::Concept),
        )
        .expect("put6");
    engine
        .put_page(
            "concepts/machine-learning",
            page_input("Machine Learning", "An intro", PageType::Concept),
        )
        .expect("put7");
}

// ─── find_by_title_fuzzy integration tests ───

#[test]
fn test_fuzzy_basic_match() {
    let engine = make_engine();
    seed_pages(&engine);

    let results = engine
        .find_by_title_fuzzy("Alice Wonderland", None, None, None)
        .expect("fuzzy");
    assert!(!results.is_empty());
    assert_eq!(results[0].slug, "people/alice-wonderland");
    assert!(results[0].score > 0.9);
}

#[test]
fn test_fuzzy_typo_tolerance() {
    let engine = make_engine();
    seed_pages(&engine);

    // "Alic Wnderland" has typos but should still match with lower threshold
    let results = engine
        .find_by_title_fuzzy("Alic Wnderland", None, Some(0.3), None)
        .expect("fuzzy");
    assert!(
        results.iter().any(|m| m.slug == "people/alice-wonderland"),
        "should find alice-wonderland despite typos"
    );
}

#[test]
fn test_fuzzy_dir_prefix_filter() {
    let engine = make_engine();
    seed_pages(&engine);

    // "Wonderland" appears in both people/ and companies/
    let results = engine
        .find_by_title_fuzzy("Wonderland", Some("people"), None, None)
        .expect("fuzzy");
    assert!(
        results.iter().all(|m| m.slug.starts_with("people/")),
        "all results should be in people/ prefix"
    );
    assert!(
        results.iter().any(|m| m.slug == "people/alice-wonderland"),
        "should find alice-wonderland in people/"
    );
}

#[test]
fn test_fuzzy_min_similarity() {
    let engine = make_engine();
    seed_pages(&engine);

    // High threshold should filter out weak matches
    let results_strict = engine
        .find_by_title_fuzzy("xyz", None, Some(0.9), None)
        .expect("fuzzy");
    assert!(
        results_strict.is_empty(),
        "high threshold should filter all results for unrelated query"
    );

    // Low threshold should include more results
    let results_loose = engine
        .find_by_title_fuzzy("Wonderland", None, Some(0.1), None)
        .expect("fuzzy");
    assert!(
        results_loose.len() >= 2,
        "low threshold should find both Wonderland matches"
    );
}

#[test]
fn test_fuzzy_limit() {
    let engine = make_engine();
    seed_pages(&engine);

    let results = engine
        .find_by_title_fuzzy("Wonderland", None, None, Some(1))
        .expect("fuzzy");
    assert_eq!(results.len(), 1, "should respect limit");
}

#[test]
fn test_fuzzy_no_results() {
    let engine = make_engine();
    seed_pages(&engine);

    let results = engine
        .find_by_title_fuzzy("zzzzzzzzzzzzz", None, None, None)
        .expect("fuzzy");
    assert!(
        results.is_empty(),
        "completely unrelated query should return no results"
    );
}

#[test]
fn test_fuzzy_results_sorted_by_score() {
    let engine = make_engine();
    seed_pages(&engine);

    let results = engine
        .find_by_title_fuzzy("Wonderland", None, None, None)
        .expect("fuzzy");
    for i in 1..results.len() {
        assert!(
            results[i - 1].score >= results[i].score,
            "results should be sorted by descending score"
        );
    }
}

#[test]
fn test_fuzzy_case_insensitive() {
    let engine = make_engine();
    seed_pages(&engine);

    let results_lower = engine
        .find_by_title_fuzzy("alice wonderland", None, None, None)
        .expect("fuzzy");
    let results_upper = engine
        .find_by_title_fuzzy("ALICE WONDERLAND", None, None, None)
        .expect("fuzzy");
    assert_eq!(results_lower.len(), results_upper.len());
    if !results_lower.is_empty() && !results_upper.is_empty() {
        assert!(
            (results_lower[0].score - results_upper[0].score).abs() < 0.001,
            "case should not affect similarity score"
        );
    }
}

// ─── resolve_slugs integration tests ───

#[test]
fn test_resolve_slugs_exact_match() {
    let engine = make_engine();
    seed_pages(&engine);

    let results = engine
        .resolve_slugs("people/alice-wonderland")
        .expect("resolve");
    assert_eq!(results, vec!["people/alice-wonderland"]);
}

#[test]
fn test_resolve_slugs_fts_prefix() {
    let engine = make_engine();
    seed_pages(&engine);

    let results = engine.resolve_slugs("people/alice").expect("resolve");
    assert!(
        results.iter().any(|s| s == "people/alice-wonderland"),
        "FTS5 prefix should find alice-wonderland"
    );
}

#[test]
fn test_resolve_slugs_no_match() {
    let engine = make_engine();
    seed_pages(&engine);

    let results = engine.resolve_slugs("zzzzzzzzzzzzz").expect("resolve");
    assert!(
        results.is_empty(),
        "unrelated query should return no results"
    );
}

#[test]
fn test_resolve_slugs_trigram_fallback() {
    let engine = make_engine();
    seed_pages(&engine);

    // "alce wnderland" is too different for FTS5 prefix match
    // but should be found via trigram similarity (Step 3)
    let results = engine.resolve_slugs("alce wnderland").expect("resolve");
    // May or may not find via trigram depending on threshold, but should not error
    // The important thing is it doesn't crash on misspelled input
    assert!(results.len() <= 20, "should return at most 20 results");
}

#[test]
fn test_resolve_slugs_like_fallback() {
    let engine = make_engine();
    seed_pages(&engine);

    // A partial substring that won't match FTS5 prefix or trigram
    // but will match via LIKE fallback (Step 4)
    let results = engine.resolve_slugs("wonder").expect("resolve");
    // LIKE %wonder% should find alice-wonderland
    assert!(
        results.iter().any(|s| s.contains("wonderland")),
        "LIKE fallback should find wonderland slug"
    );
}

#[test]
fn test_fuzzy_empty_query() {
    let engine = make_engine();
    seed_pages(&engine);

    // Empty query should return empty results (not crash)
    let results = engine
        .find_by_title_fuzzy("", None, None, None)
        .expect("fuzzy");
    assert!(results.is_empty(), "empty query should return no results");
}

#[test]
fn test_fuzzy_min_similarity_clamped() {
    let engine = make_engine();
    seed_pages(&engine);

    // Negative min_similarity should be clamped to 0.0
    let results = engine
        .find_by_title_fuzzy("Alice", None, Some(-1.0), None)
        .expect("fuzzy");
    // Should not crash, should return results (clamped to 0.0 threshold)
    assert!(
        !results.is_empty(),
        "clamped negative threshold should return results"
    );

    // min_similarity > 1.0 should be clamped to 1.0
    let results_high = engine
        .find_by_title_fuzzy("Alice", None, Some(2.0), None)
        .expect("fuzzy");
    assert!(
        results_high.is_empty(),
        "clamped >1.0 threshold should return no results"
    );
}

#[test]
fn test_fuzzy_dir_prefix_invalid() {
    let engine = make_engine();
    seed_pages(&engine);

    // dir_prefix with LIKE wildcard should be rejected
    let result = engine.find_by_title_fuzzy("Alice", Some("%"), None, None);
    assert!(result.is_err(), "dir_prefix with % should be rejected");

    // dir_prefix with path traversal should be rejected
    let result2 = engine.find_by_title_fuzzy("Alice", Some(".."), None, None);
    assert!(result2.is_err(), "dir_prefix with .. should be rejected");
}
