//! Engine CRUD integration tests against in-memory SQLite

use rclaw_gbrain::config::Config;
use rclaw_gbrain::engine::BrainEngine;
use rclaw_gbrain::operations::{OpContext, Operations};
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

#[test]
fn test_put_and_get_page() {
    let engine = make_engine();
    let page = engine
        .put_page(
            "people/alice",
            page_input("Alice", "An engineer", PageType::Person),
        )
        .expect("put");
    assert_eq!(page.slug, "people/alice");
    assert_eq!(page.title, "Alice");
    assert_eq!(page.page_type, PageType::Person);

    let got = engine.get_page("people/alice").expect("get").expect("some");
    assert_eq!(got.title, "Alice");
    assert_eq!(got.compiled_truth, "An engineer");
}

#[test]
fn test_update_page() {
    let engine = make_engine();
    engine
        .put_page(
            "people/alice",
            page_input("Alice", "Original", PageType::Person),
        )
        .expect("put1");
    engine
        .put_page(
            "people/alice",
            page_input("Alice Updated", "Updated content", PageType::Person),
        )
        .expect("put2");

    let got = engine.get_page("people/alice").expect("get").expect("some");
    assert_eq!(got.title, "Alice Updated");
    assert_eq!(got.compiled_truth, "Updated content");
}

#[test]
fn test_delete_page() {
    let engine = make_engine();
    engine
        .put_page(
            "people/alice",
            page_input("Alice", "Content", PageType::Person),
        )
        .expect("put");
    engine.delete_page("people/alice").expect("delete");
    let got = engine.get_page("people/alice").expect("get");
    assert!(got.is_none());
}

#[test]
fn test_restore_soft_deleted_page() {
    let engine = make_engine();
    engine
        .put_page(
            "people/alice",
            page_input("Alice", "Content", PageType::Person),
        )
        .expect("put");

    engine.delete_page("people/alice").expect("delete");
    assert!(engine.get_page("people/alice").expect("get").is_none());

    let deleted = engine
        .list_pages(PageFilters {
            include_deleted: true,
            slug_prefix: Some("people/".to_string()),
            ..Default::default()
        })
        .expect("list deleted");
    assert_eq!(deleted.len(), 1);
    assert!(deleted[0].deleted_at.is_some());

    assert!(engine.restore_page("people/alice").expect("restore"));
    let restored = engine
        .get_page("people/alice")
        .expect("get restored")
        .expect("restored page");
    assert_eq!(restored.title, "Alice");
    assert!(restored.deleted_at.is_none());
}

#[test]
fn test_list_pages() {
    let engine = make_engine();
    engine
        .put_page("people/alice", page_input("Alice", "A", PageType::Person))
        .expect("put1");
    engine
        .put_page("companies/acme", page_input("Acme", "B", PageType::Company))
        .expect("put2");

    let filters = PageFilters {
        page_type: None,
        tag: None,
        limit: Some(50),
        offset: None,
        updated_after: None,
        include_deleted: false,
        slug_prefix: None,
    };
    let pages = engine.list_pages(filters).expect("list");
    assert_eq!(pages.len(), 2);
}

#[test]
fn test_list_pages_by_type() {
    let engine = make_engine();
    engine
        .put_page("people/alice", page_input("Alice", "A", PageType::Person))
        .expect("put1");
    engine
        .put_page("companies/acme", page_input("Acme", "B", PageType::Company))
        .expect("put2");

    let filters = PageFilters {
        page_type: Some(PageType::Person),
        tag: None,
        limit: Some(50),
        offset: None,
        updated_after: None,
        include_deleted: false,
        slug_prefix: None,
    };
    let pages = engine.list_pages(filters).expect("list");
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].slug, "people/alice");
}

#[test]
fn test_tags() {
    let engine = make_engine();
    engine
        .put_page("people/alice", page_input("Alice", "A", PageType::Person))
        .expect("put");

    engine
        .add_tag("people/alice", "engineer")
        .expect("add_tag1");
    engine.add_tag("people/alice", "rust").expect("add_tag2");

    let tags = engine.get_tags("people/alice").expect("get_tags");
    assert_eq!(tags.len(), 2);
    assert!(tags.contains(&"engineer".to_string()));
    assert!(tags.contains(&"rust".to_string()));

    engine
        .remove_tag("people/alice", "engineer")
        .expect("remove_tag");
    let tags = engine.get_tags("people/alice").expect("get_tags2");
    assert_eq!(tags.len(), 1);
}

#[test]
fn test_links() {
    let engine = make_engine();
    engine
        .put_page("people/alice", page_input("Alice", "A", PageType::Person))
        .expect("put1");
    engine
        .put_page("companies/acme", page_input("Acme", "B", PageType::Company))
        .expect("put2");

    engine
        .add_link(
            "people/alice",
            "companies/acme",
            None,
            Some("works_at"),
            None,
            None,
            None,
        )
        .expect("add_link");

    let links = engine.get_links("people/alice").expect("get_links");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].to_slug, "companies/acme");
    assert_eq!(links[0].link_type, "works_at");

    let backlinks = engine
        .get_backlinks("companies/acme")
        .expect("get_backlinks");
    assert_eq!(backlinks.len(), 1);
    assert_eq!(backlinks[0].from_slug, "people/alice");
}

#[test]
fn test_chunks() {
    let engine = make_engine();
    engine
        .put_page("people/alice", page_input("Alice", "A", PageType::Person))
        .expect("put");

    let chunks = vec![
        ChunkInput {
            chunk_index: 0,
            chunk_text: "First chunk".to_string(),
            source: ChunkSource::CompiledTruth,
            token_count: 10,
            embedding: None,
            model: None,
            language: None,
            symbol_name: None,
            symbol_type: None,
            start_line: None,
            end_line: None,
            parent_symbol_path: None,
            symbol_name_qualified: None,
            doc_comment: None,
        },
        ChunkInput {
            chunk_index: 1,
            chunk_text: "Second chunk".to_string(),
            source: ChunkSource::Timeline,
            token_count: 10,
            embedding: None,
            model: None,
            language: None,
            symbol_name: None,
            symbol_type: None,
            start_line: None,
            end_line: None,
            parent_symbol_path: None,
            symbol_name_qualified: None,
            doc_comment: None,
        },
    ];
    engine
        .upsert_chunks("people/alice", &chunks)
        .expect("upsert");

    let got = engine.get_chunks("people/alice").expect("get_chunks");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].chunk_text, "First chunk");
    assert_eq!(got[1].chunk_text, "Second chunk");
}

#[test]
fn test_stale_chunks_track_embedding_status() {
    let engine = make_engine();
    engine
        .put_page("people/alice", page_input("Alice", "A", PageType::Person))
        .expect("put");

    engine
        .upsert_chunks(
            "people/alice",
            &[ChunkInput {
                chunk_index: 0,
                chunk_text: "Alice writes Rust".to_string(),
                source: ChunkSource::CompiledTruth,
                token_count: 3,
                embedding: None,
                model: None,
                language: None,
                symbol_name: None,
                symbol_type: None,
                start_line: None,
                end_line: None,
                parent_symbol_path: None,
                symbol_name_qualified: None,
                doc_comment: None,
            }],
        )
        .expect("chunks");
    assert_eq!(engine.count_stale_chunks().expect("stale count"), 1);

    engine
        .upsert_chunks(
            "people/alice",
            &[ChunkInput {
                chunk_index: 0,
                chunk_text: "Alice writes Rust".to_string(),
                source: ChunkSource::CompiledTruth,
                token_count: 3,
                embedding: Some(vec![0.1, 0.2, 0.3]),
                model: Some("test-embedding".to_string()),
                language: None,
                symbol_name: None,
                symbol_type: None,
                start_line: None,
                end_line: None,
                parent_symbol_path: None,
                symbol_name_qualified: None,
                doc_comment: None,
            }],
        )
        .expect("embedded chunks");
    assert_eq!(engine.count_stale_chunks().expect("stale count"), 0);
}

#[test]
fn test_code_page_indexes_symbols_and_edges() {
    let engine = make_engine();
    let ops = Operations::with_config(&engine, OpContext::default(), Config::default());
    let content = r#"
pub fn alpha() {
    beta();
}

fn beta() {
}
"#;

    ops.put_page("code/lib", "Lib", content, Some(PageType::Code), None)
        .expect("put code page");

    let chunks = engine.get_chunks("code/lib").expect("chunks");
    assert!(chunks.iter().any(|c| {
        c.source == ChunkSource::FencedCode && c.symbol_name.as_deref() == Some("alpha")
    }));
    assert!(chunks.iter().any(|c| {
        c.source == ChunkSource::FencedCode && c.symbol_name.as_deref() == Some("beta")
    }));

    let callees = engine.get_callees_of("code/lib", "alpha").expect("callees");
    assert!(callees.iter().any(|e| e.to_symbol == "beta"));

    let results = engine
        .search_keyword_chunks(
            "alpha",
            SearchOpts {
                limit: Some(10),
                page_type: Some(PageType::Code),
                ..Default::default()
            },
        )
        .expect("search code chunks");
    assert!(results
        .iter()
        .any(|r| r.symbol_name.as_deref() == Some("alpha")));
}

#[test]
fn test_code_search_filters_language_and_symbol_kind() {
    let engine = make_engine();
    let ops = Operations::with_config(&engine, OpContext::default(), Config::default());
    let content = r#"---
language: rust
---

pub fn alpha() {
    beta();
}

fn beta() {
}
"#;

    ops.put_page("code/lib", "Lib", content, Some(PageType::Code), None)
        .expect("put code page");

    let rust_functions = engine
        .search_keyword_chunks(
            "alpha",
            SearchOpts {
                limit: Some(10),
                page_type: Some(PageType::Code),
                language: Some("rust".to_string()),
                symbol_kind: Some("function".to_string()),
                ..Default::default()
            },
        )
        .expect("search rust functions");
    assert!(
        rust_functions
            .iter()
            .any(|r| r.symbol_name.as_deref() == Some("alpha")),
        "got: {rust_functions:?}"
    );

    let python_functions = engine
        .search_keyword_chunks(
            "alpha",
            SearchOpts {
                limit: Some(10),
                page_type: Some(PageType::Code),
                language: Some("python".to_string()),
                symbol_kind: Some("function".to_string()),
                ..Default::default()
            },
        )
        .expect("search python functions");
    assert!(python_functions.is_empty());
}

#[test]
fn test_embedding_refresh_preserves_code_metadata() {
    let engine = make_engine();
    let ops = Operations::with_config(&engine, OpContext::default(), Config::default());
    let content = r#"---
language: rust
---

pub fn alpha() {
}
"#;

    ops.put_page("code/lib", "Lib", content, Some(PageType::Code), None)
        .expect("put code page");
    let alpha = engine
        .get_chunks("code/lib")
        .expect("chunks")
        .into_iter()
        .find(|c| c.symbol_name.as_deref() == Some("alpha"))
        .expect("alpha chunk");

    engine
        .upsert_chunks(
            "code/lib",
            &[ChunkInput {
                chunk_index: alpha.chunk_index,
                chunk_text: alpha.chunk_text.clone(),
                source: alpha.source.clone(),
                token_count: alpha.token_count,
                embedding: Some(vec![0.1, 0.2, 0.3]),
                model: Some("test-embedding".to_string()),
                language: alpha.language.clone(),
                symbol_name: alpha.symbol_name.clone(),
                symbol_type: alpha.symbol_type.clone(),
                start_line: alpha.start_line,
                end_line: alpha.end_line,
                parent_symbol_path: alpha.parent_symbol_path.clone(),
                symbol_name_qualified: alpha.symbol_name_qualified.clone(),
                doc_comment: alpha.doc_comment.clone(),
            }],
        )
        .expect("embed alpha");

    engine
        .upsert_chunks(
            "code/lib",
            &[ChunkInput {
                chunk_index: alpha.chunk_index,
                chunk_text: alpha.chunk_text.clone(),
                source: alpha.source.clone(),
                token_count: alpha.token_count,
                embedding: None,
                model: alpha.model.clone(),
                language: alpha.language.clone(),
                symbol_name: alpha.symbol_name.clone(),
                symbol_type: alpha.symbol_type.clone(),
                start_line: alpha.start_line,
                end_line: alpha.end_line,
                parent_symbol_path: alpha.parent_symbol_path.clone(),
                symbol_name_qualified: alpha.symbol_name_qualified.clone(),
                doc_comment: alpha.doc_comment.clone(),
            }],
        )
        .expect("metadata refresh without embedding");

    let refreshed = engine
        .get_chunks("code/lib")
        .expect("chunks")
        .into_iter()
        .find(|c| c.id == alpha.id)
        .expect("refreshed alpha");
    assert_eq!(refreshed.language.as_deref(), Some("rust"));
    assert_eq!(refreshed.symbol_name.as_deref(), Some("alpha"));
    assert_eq!(
        engine
            .get_embeddings_by_chunk_ids(&[alpha.id])
            .expect("embedding")
            .len(),
        1
    );
}

#[test]
fn test_timeline() {
    let engine = make_engine();
    engine
        .put_page("people/alice", page_input("Alice", "A", PageType::Person))
        .expect("put");

    let entry = TimelineInput {
        date: "2024-01-15".to_string(),
        source: None,
        summary: "Joined Acme Corp".to_string(),
        detail: None,
    };
    engine
        .add_timeline_entry("people/alice", entry, false)
        .expect("add_timeline");

    let entries = engine
        .get_timeline("people/alice", None)
        .expect("get_timeline");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].summary, "Joined Acme Corp");
}

#[test]
fn test_stats() {
    let engine = make_engine();
    engine
        .put_page("people/alice", page_input("Alice", "A", PageType::Person))
        .expect("put");
    engine.add_tag("people/alice", "engineer").expect("tag");

    let stats = engine.get_stats().expect("stats");
    assert_eq!(stats.page_count, 1);
    assert_eq!(stats.tag_count, 1);
}

#[test]
fn test_health() {
    let engine = make_engine();
    engine
        .put_page("people/alice", page_input("Alice", "A", PageType::Person))
        .expect("put");

    let health = engine.get_health().expect("health");
    assert_eq!(health.page_count, 1);
    assert!(health.brain_score > 0.0);
}

#[test]
fn test_resolve_slugs() {
    let engine = make_engine();
    engine
        .put_page("people/alice", page_input("Alice", "A", PageType::Person))
        .expect("put");
    engine
        .put_page("people/bob", page_input("Bob", "B", PageType::Person))
        .expect("put2");

    let slugs = engine.resolve_slugs("people").expect("resolve");
    assert_eq!(slugs.len(), 2);
}

#[test]
fn test_config() {
    let engine = make_engine();
    engine.set_config("test_key", "test_value").expect("set");
    let val = engine.get_config("test_key").expect("get");
    assert_eq!(val, Some("test_value".to_string()));
}

#[test]
fn test_raw_data() {
    let engine = make_engine();
    engine
        .put_page("people/alice", page_input("Alice", "A", PageType::Person))
        .expect("put");
    engine
        .put_raw_data("people/alice", "raw_key", serde_json::json!({"foo": "bar"}))
        .expect("put");
    let data = engine.get_raw_data("people/alice", "raw_key").expect("get");
    assert!(data.is_some());
}

#[test]
fn test_disconnect() {
    let mut engine = make_engine();
    engine.disconnect().expect("disconnect");
}
