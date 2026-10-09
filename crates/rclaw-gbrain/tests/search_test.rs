//! Search quality integration tests

use rclaw_gbrain::config::Config;
use rclaw_gbrain::engine::BrainEngine;
use rclaw_gbrain::sqlite_engine::SqliteEngine;
use rclaw_gbrain::types::*;

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

fn make_brain() -> SqliteEngine {
    let mut engine = SqliteEngine::with_config(":memory:", Config::default());
    engine.connect().expect("connect");
    engine.init_schema().expect("init_schema");

    // Seed some pages
    engine.put_page("people/alice", page_input("Alice Example", "Alice is a software engineer at Acme Corp. She specializes in Rust and distributed systems.", PageType::Person)).expect("put");
    engine.put_page("people/bob", page_input("Bob Builder", "Bob is a product manager at Widget Co. He focuses on user experience and mobile apps.", PageType::Person)).expect("put");
    engine
        .put_page(
            "companies/acme",
            page_input(
                "Acme Corp",
                "Acme Corp is a technology company building distributed systems tools.",
                PageType::Company,
            ),
        )
        .expect("put");
    engine.put_page("concepts/distributed-systems", page_input("Distributed Systems", "Distributed systems are computing systems where components on networked computers communicate and coordinate.", PageType::Concept)).expect("put");

    engine
}

#[test]
fn test_keyword_search() {
    let engine = make_brain();
    let opts = SearchOpts {
        limit: Some(10),
        page_type: None,
        detail_level: None,
        expanded_queries: None,
        ..Default::default()
    };
    let results = engine.search_keyword("engineer", opts).expect("search");
    assert!(!results.is_empty());
    // Alice should appear since she's an engineer
    assert!(results.iter().any(|r| r.slug == "people/alice"));
}

#[test]
fn test_keyword_search_no_results() {
    let engine = make_brain();
    let opts = SearchOpts {
        limit: Some(10),
        page_type: None,
        detail_level: None,
        expanded_queries: None,
        ..Default::default()
    };
    let results = engine
        .search_keyword("quantum_physics_nonexistent", opts)
        .expect("search");
    assert!(results.is_empty());
}

#[test]
fn test_search_by_type() {
    let engine = make_brain();
    let opts = SearchOpts {
        limit: Some(10),
        page_type: Some(PageType::Person),
        detail_level: None,
        expanded_queries: None,
        ..Default::default()
    };
    let results = engine.search_keyword("engineer", opts).expect("search");
    assert!(results.iter().all(|r| r.slug.starts_with("people/")));
}
