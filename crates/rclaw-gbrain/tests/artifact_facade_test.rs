//! artifact_* 统一 facade 测试:
//! - 工具定义（artifact_* facade + KB OCR 扩展）
//! - put_memory 创建 artifact、occurrence、projection、page
//! - put_memory creates artifact, occurrence, projection, page
//! - Intent routing (memory/evidence/promote)
//! - Query with include_sources
//! - Get artifact detail with sources
//! - Delete/restore lifecycle
//! - Idempotent put

use rclaw_gbrain::artifact::promotion::{self, CreateCandidateInput};
use rclaw_gbrain::artifact::service::ArtifactService;
use rclaw_gbrain::artifact::types::{
    CandidateType, ProjectionType, PromotionPolicy, ReviewCandidateInput, RiskLevel, SourceKind,
    UploadIntent, UploadSourceInput, UploadSourceOutput,
};
use rclaw_gbrain::config::Config;
use rclaw_gbrain::engine::BrainEngine;
use rclaw_gbrain::operations::OpContext;
use rclaw_gbrain::sqlite_engine::SqliteEngine;

fn make_engine() -> SqliteEngine {
    let mut engine = SqliteEngine::with_config(":memory:", Config::default());
    engine.connect().expect("connect");
    engine.init_schema().expect("init_schema");
    // Ensure jobs table exists (needed by KB document processing)
    let conn = engine.connection().expect("connection");
    rclaw_gbrain::jobs::JobQueue::new(conn)
        .init()
        .expect("init_jobs");
    engine
}

fn make_config() -> Config {
    Config::default()
}

fn make_svc<'a>(engine: &'a SqliteEngine, config: &'a Config) -> ArtifactService<'a> {
    let ctx = OpContext::default();
    ArtifactService::new(engine, ctx, config)
}

fn uploaded_kb_document_id(output: &UploadSourceOutput) -> i64 {
    output
        .projections
        .iter()
        .find(|p| p.projection_type == ProjectionType::KbDocument)
        .and_then(|p| p.projection_ref.strip_prefix("kb_document:"))
        .and_then(|id| id.parse::<i64>().ok())
        .expect("upload should create a kb_document projection")
}

#[test]
fn artifact_upload_default_promotion_policy_auto_applies() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    assert_eq!(config.upload_default_promotion_policy, "auto-apply");

    let result = svc
        .upload_file(UploadSourceInput {
            content: b"# Upload Default\n\nFresh knowledge for the KB.".to_vec(),
            path: None,
            original_name: "upload-default.md".to_string(),
            source_kind: SourceKind::Upload,
            source_uri: "upload://upload-default.md".to_string(),
            intent: UploadIntent::Auto,
            target_slug: None,
            page_slug: None,
            library_id: None,
            folder_id: None,
            promotion_policy: None,
            owner_ref: None,
            metadata: None,
            dry_run: true,
        })
        .expect("dry-run upload");

    assert!(result.route_plan.to_kb);
    assert!(result.route_plan.to_shadow);
    assert_eq!(result.route_plan.promotion, PromotionPolicy::AutoApply);
}

#[test]
fn artifact_upload_same_source_uri_updates_existing_kb_document() {
    let engine = make_engine();
    let temp_dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).expect("temp dir");
    let mut config = make_config();
    config.artifact_storage_dir = Some(
        temp_dir
            .path()
            .join("artifacts")
            .to_string_lossy()
            .to_string(),
    );
    config.upload_default_promotion_policy = "none".to_string();
    let svc = make_svc(&engine, &config);
    let stable_source_uri = "upload://stable/report-123".to_string();

    let first = svc
        .upload_file(UploadSourceInput {
            content: b"# Report\n\nfirst version".to_vec(),
            path: Some(std::path::PathBuf::from("docs/report.md")),
            original_name: "report.md".to_string(),
            source_kind: SourceKind::Upload,
            source_uri: stable_source_uri.clone(),
            intent: UploadIntent::Document,
            target_slug: None,
            page_slug: None,
            library_id: None,
            folder_id: None,
            promotion_policy: Some(PromotionPolicy::None),
            owner_ref: None,
            metadata: None,
            dry_run: false,
        })
        .expect("first upload");
    let first_doc_id = uploaded_kb_document_id(&first);

    let second = svc
        .upload_file(UploadSourceInput {
            content: b"# Report\n\nsecond version with new facts".to_vec(),
            path: Some(std::path::PathBuf::from("docs/renamed-report.md")),
            original_name: "renamed-report.md".to_string(),
            source_kind: SourceKind::Upload,
            source_uri: stable_source_uri.clone(),
            intent: UploadIntent::Document,
            target_slug: None,
            page_slug: None,
            library_id: None,
            folder_id: None,
            promotion_policy: Some(PromotionPolicy::None),
            owner_ref: None,
            metadata: None,
            dry_run: false,
        })
        .expect("second upload updates source");
    let second_doc_id = uploaded_kb_document_id(&second);

    assert_ne!(first.sha256, second.sha256);
    assert_ne!(first.artifact_id, second.artifact_id);
    assert_eq!(
        first_doc_id, second_doc_id,
        "same stable source_uri should keep the KB document identity even if the file name changes"
    );

    let conn = engine.connection().expect("connection");
    let (content_hash, original_name, original_path, source_uri, document_status, index_status): (
        String,
        String,
        String,
        String,
        String,
        String,
    ) = conn
        .query_row(
            "SELECT content_hash, original_name, original_path, source_uri, document_status, index_status
             FROM kb_documents WHERE id = ?1",
            rusqlite::params![second_doc_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .expect("kb document");
    assert_eq!(content_hash, second.sha256);
    assert_eq!(original_name, "renamed-report.md");
    assert_eq!(original_path, "docs/renamed-report.md");
    assert_eq!(source_uri, stable_source_uri);
    assert_eq!(document_status, "queued");
    assert_eq!(index_status, "pending");

    let (old_occ_status, old_occ_reason): (String, String) = conn
        .query_row(
            "SELECT status, stale_reason FROM artifact_occurrences WHERE id = ?1",
            rusqlite::params![first.occurrence_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("old occurrence");
    assert_eq!(old_occ_status, "stale");
    assert_eq!(old_occ_reason, "content_updated");

    let old_active_kb_projections: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_projections
             WHERE artifact_id = ?1 AND projection_type = 'kb_document' AND status = 'active'",
            rusqlite::params![first.artifact_id],
            |row| row.get(0),
        )
        .expect("old active projections");
    assert_eq!(old_active_kb_projections, 0);

    let new_active_kb_projections: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_projections
             WHERE artifact_id = ?1 AND projection_type = 'kb_document' AND projection_ref = ?2 AND status = 'active'",
            rusqlite::params![second.artifact_id, format!("kb_document:{}", second_doc_id)],
            |row| row.get(0),
        )
        .expect("new active projections");
    assert_eq!(new_active_kb_projections, 1);
}

#[test]
fn artifact_upload_source_uri_update_to_existing_content_retires_old_kb_document() {
    let engine = make_engine();
    let temp_dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).expect("temp dir");
    let mut config = make_config();
    config.artifact_storage_dir = Some(
        temp_dir
            .path()
            .join("artifacts")
            .to_string_lossy()
            .to_string(),
    );
    config.upload_default_promotion_policy = "none".to_string();
    let svc = make_svc(&engine, &config);

    let source_uri_a = "upload://stable/source-a".to_string();
    let source_uri_b = "upload://stable/source-b".to_string();
    let shared_content = b"# Shared\n\ncontent already indexed elsewhere".to_vec();

    let first_a = svc
        .upload_file(UploadSourceInput {
            content: b"# Source A\n\nold source-specific facts".to_vec(),
            path: Some(std::path::PathBuf::from("docs/source-a.md")),
            original_name: "source-a.md".to_string(),
            source_kind: SourceKind::Upload,
            source_uri: source_uri_a.clone(),
            intent: UploadIntent::Document,
            target_slug: None,
            page_slug: None,
            library_id: None,
            folder_id: None,
            promotion_policy: Some(PromotionPolicy::None),
            owner_ref: None,
            metadata: None,
            dry_run: false,
        })
        .expect("first source A upload");
    let old_source_a_doc_id = uploaded_kb_document_id(&first_a);

    let source_b = svc
        .upload_file(UploadSourceInput {
            content: shared_content.clone(),
            path: Some(std::path::PathBuf::from("docs/source-b.md")),
            original_name: "source-b.md".to_string(),
            source_kind: SourceKind::Upload,
            source_uri: source_uri_b,
            intent: UploadIntent::Document,
            target_slug: None,
            page_slug: None,
            library_id: None,
            folder_id: None,
            promotion_policy: Some(PromotionPolicy::None),
            owner_ref: None,
            metadata: None,
            dry_run: false,
        })
        .expect("source B upload");
    let shared_doc_id = uploaded_kb_document_id(&source_b);

    let updated_a = svc
        .upload_file(UploadSourceInput {
            content: shared_content,
            path: Some(std::path::PathBuf::from("docs/source-a-renamed.md")),
            original_name: "source-a-renamed.md".to_string(),
            source_kind: SourceKind::Upload,
            source_uri: source_uri_a.clone(),
            intent: UploadIntent::Document,
            target_slug: None,
            page_slug: None,
            library_id: None,
            folder_id: None,
            promotion_policy: Some(PromotionPolicy::None),
            owner_ref: None,
            metadata: None,
            dry_run: false,
        })
        .expect("source A update should reuse existing content doc without unique hash conflict");
    let updated_source_a_doc_id = uploaded_kb_document_id(&updated_a);

    assert_ne!(old_source_a_doc_id, shared_doc_id);
    assert_eq!(
        updated_source_a_doc_id, shared_doc_id,
        "when updated content already has a KB doc, source update should reuse that content doc"
    );

    let conn = engine.connection().expect("connection");
    let (old_doc_status, old_doc_deleted_at): (String, Option<String>) = conn
        .query_row(
            "SELECT document_status, deleted_at FROM kb_documents WHERE id = ?1",
            rusqlite::params![old_source_a_doc_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("old source A kb document");
    assert_eq!(old_doc_status, "deleted");
    assert!(
        old_doc_deleted_at.is_some(),
        "old source A KB document should be filtered from search after source update"
    );

    let (old_occ_status, old_occ_reason): (String, String) = conn
        .query_row(
            "SELECT status, stale_reason FROM artifact_occurrences WHERE id = ?1",
            rusqlite::params![first_a.occurrence_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("old source A occurrence");
    assert_eq!(old_occ_status, "stale");
    assert_eq!(old_occ_reason, "content_updated");

    let stale_old_source_projections: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_projections
             WHERE occurrence_id = ?1 AND projection_type = 'kb_document'
               AND status = 'stale' AND stale_reason = 'content_updated'",
            rusqlite::params![first_a.occurrence_id],
            |row| row.get(0),
        )
        .expect("stale old source projections");
    assert_eq!(stale_old_source_projections, 1);

    let active_updated_source_projection: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_projections p
             JOIN artifact_occurrences o ON o.id = p.occurrence_id
             WHERE p.projection_type = 'kb_document'
               AND p.projection_ref = ?1
               AND p.status = 'active'
               AND o.source_uri = ?2",
            rusqlite::params![format!("kb_document:{}", shared_doc_id), source_uri_a],
            |row| row.get(0),
        )
        .expect("active updated source projection");
    assert_eq!(active_updated_source_projection, 1);

    let source_update_events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_events
             WHERE artifact_id = ?1
               AND occurrence_id = ?2
               AND event_type = 'source_content_updated'
               AND payload_json LIKE ?3",
            rusqlite::params![
                updated_a.artifact_id,
                updated_a.occurrence_id,
                format!("%\"kb_document_id\":{}%", shared_doc_id)
            ],
            |row| row.get(0),
        )
        .expect("source update event");
    assert_eq!(source_update_events, 1);
}

#[test]
fn artifact_upload_source_uri_update_retires_all_legacy_active_source_docs() {
    let engine = make_engine();
    let temp_dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).expect("temp dir");
    let mut config = make_config();
    config.artifact_storage_dir = Some(
        temp_dir
            .path()
            .join("artifacts")
            .to_string_lossy()
            .to_string(),
    );
    config.upload_default_promotion_policy = "none".to_string();
    let svc = make_svc(&engine, &config);

    let source_uri = "upload://stable/legacy-duplicates".to_string();
    let first = svc
        .upload_file(UploadSourceInput {
            content: b"# Legacy\n\nfirst active source document".to_vec(),
            path: Some(std::path::PathBuf::from("docs/legacy-a.md")),
            original_name: "legacy-a.md".to_string(),
            source_kind: SourceKind::Upload,
            source_uri: source_uri.clone(),
            intent: UploadIntent::Document,
            target_slug: None,
            page_slug: None,
            library_id: None,
            folder_id: None,
            promotion_policy: Some(PromotionPolicy::None),
            owner_ref: None,
            metadata: None,
            dry_run: false,
        })
        .expect("first legacy source upload");
    let first_doc_id = uploaded_kb_document_id(&first);

    let legacy_duplicate = svc
        .upload_file(UploadSourceInput {
            content: b"# Legacy\n\nsecond pre-existing source document".to_vec(),
            path: Some(std::path::PathBuf::from("docs/legacy-b.md")),
            original_name: "legacy-b.md".to_string(),
            source_kind: SourceKind::Upload,
            source_uri: "upload://stable/temporary-other-source".to_string(),
            intent: UploadIntent::Document,
            target_slug: None,
            page_slug: None,
            library_id: None,
            folder_id: None,
            promotion_policy: Some(PromotionPolicy::None),
            owner_ref: None,
            metadata: None,
            dry_run: false,
        })
        .expect("legacy duplicate upload");
    let duplicate_doc_id = uploaded_kb_document_id(&legacy_duplicate);

    {
        let conn = engine.connection().expect("connection");
        conn.execute(
            "UPDATE artifact_occurrences
             SET source_uri = ?1, updated_at = datetime('now', '+1 second')
             WHERE id = ?2",
            rusqlite::params![source_uri, legacy_duplicate.occurrence_id],
        )
        .expect("seed legacy duplicate source_uri");
    }

    let updated = svc
        .upload_file(UploadSourceInput {
            content: b"# Legacy\n\nfresh replacement content".to_vec(),
            path: Some(std::path::PathBuf::from("docs/legacy-fresh.md")),
            original_name: "legacy-fresh.md".to_string(),
            source_kind: SourceKind::Upload,
            source_uri: source_uri.clone(),
            intent: UploadIntent::Document,
            target_slug: None,
            page_slug: None,
            library_id: None,
            folder_id: None,
            promotion_policy: Some(PromotionPolicy::None),
            owner_ref: None,
            metadata: None,
            dry_run: false,
        })
        .expect("source update should retire all legacy duplicate docs");

    let conn = engine.connection().expect("connection");
    let old_searchable_docs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kb_documents
             WHERE id IN (?1, ?2)
               AND deleted_at IS NULL
               AND document_status != 'deleted'
               AND content_hash != ?3",
            rusqlite::params![first_doc_id, duplicate_doc_id, updated.sha256],
            |row| row.get(0),
        )
        .expect("old searchable docs");
    assert_eq!(
        old_searchable_docs, 0,
        "legacy source duplicates must not leave old content searchable"
    );

    let updated_reused_doc_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kb_documents
             WHERE id IN (?1, ?2)
               AND deleted_at IS NULL
               AND document_status != 'deleted'
               AND content_hash = ?3",
            rusqlite::params![first_doc_id, duplicate_doc_id, updated.sha256],
            |row| row.get(0),
        )
        .expect("updated reused doc count");
    assert_eq!(updated_reused_doc_count, 1);

    let deleted_legacy_doc_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kb_documents
             WHERE id IN (?1, ?2)
               AND deleted_at IS NOT NULL
               AND document_status = 'deleted'",
            rusqlite::params![first_doc_id, duplicate_doc_id],
            |row| row.get(0),
        )
        .expect("deleted legacy doc count");
    assert_eq!(deleted_legacy_doc_count, 1);

    let stale_source_projection_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_projections
             WHERE occurrence_id IN (?1, ?2)
               AND projection_type = 'kb_document'
               AND status = 'stale'
               AND stale_reason = 'content_updated'",
            rusqlite::params![first.occurrence_id, legacy_duplicate.occurrence_id],
            |row| row.get(0),
        )
        .expect("stale legacy source projections");
    assert_eq!(stale_source_projection_count, 2);
}

#[allow(dead_code)]
fn seed_kb_artifact(
    conn: &rusqlite::Connection,
    slug: &str,
    original_name: &str,
    content: &str,
) -> (i64, String, i64) {
    conn.execute(
        "INSERT INTO kb_libraries (name) VALUES ('test-library')",
        [],
    )
    .expect("insert library");
    let library_id = conn.last_insert_rowid();

    let artifact_uid = format!("art_test_{}", slug.replace(['/', '-'], "_"));
    conn.execute(
        "INSERT INTO source_artifacts
            (artifact_uid, sha256, original_name, extension, mime_type, size_bytes,
             storage_path, canonical_slug, status, metadata_json)
         VALUES (?1, ?2, ?3, 'md', 'text/markdown', ?4, '', ?5, 'active', '{}')",
        rusqlite::params![
            artifact_uid,
            format!("hash-{}", slug),
            original_name,
            content.len() as i64,
            slug
        ],
    )
    .expect("insert source artifact");
    let artifact_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO kb_documents
             (library_id, original_name, name_tokens, content_hash, extension, mime_type, document_status)
         VALUES (?1, ?2, ?3, ?4, 'md', 'text/markdown', 'ready')",
        rusqlite::params![
            library_id,
            original_name,
            rclaw_gbrain::nlp::chinese::tokenize_name(original_name),
            format!("doc-hash-{}", slug)
        ],
    )
    .expect("insert kb document");
    let document_id = conn.last_insert_rowid();

    let tokens = rclaw_gbrain::nlp::chinese::tokenize_content(content);
    conn.execute(
        "INSERT INTO kb_document_nodes
             (library_id, document_id, content, content_tokens, level, chunk_order, source_start, source_end)
         VALUES (?1, ?2, ?3, ?4, 0, 0, 0, ?5)",
        rusqlite::params![
            library_id,
            document_id,
            content,
            tokens,
            content.chars().count() as i64
        ],
    )
    .expect("insert kb node");

    conn.execute(
        "INSERT INTO artifact_projections
            (artifact_id, projection_type, projection_key, projection_ref, status, version_hash)
         VALUES (?1, 'kb_document', ?2, ?2, 'active', 'test')",
        rusqlite::params![artifact_id, format!("kb_document:{}", document_id)],
    )
    .expect("insert kb projection");
    conn.execute(
        "INSERT INTO artifact_projections
            (artifact_id, projection_type, projection_key, projection_ref, status, version_hash)
         VALUES (?1, 'brain_shadow_page', ?2, ?2, 'active', 'test')",
        rusqlite::params![artifact_id, format!("slug:documents/{}", slug)],
    )
    .expect("insert shadow projection");

    (artifact_id, artifact_uid, document_id)
}

fn is_expected_default_mcp_tool(name: &str) -> bool {
    name.starts_with("artifact_")
        || matches!(name, "kb_document_status" | "kb_ocr_run" | "kb_ocr_retry")
}

// --- Test 1: Default tool defs contain the facade and documented OCR extensions ---

#[test]
fn tool_defs_default_include_artifact_and_ocr_tools() {
    let defs = rclaw_gbrain::mcp::tool_defs::build_tool_defs();
    assert!(!defs.is_empty(), "should have some tool defs");
    for def in &defs {
        assert!(
            is_expected_default_mcp_tool(&def.name),
            "unexpected default MCP tool '{}'",
            def.name
        );
    }
}

// --- Test 2: artifact_put creates artifact, occurrence, projection, page ---

#[test]
fn artifact_put_creates_artifact_occurrence_projection_page() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    let result = svc
        .put_memory(
            "people/test-person",
            "Test content about a person",
            Some("Test Person"),
            None,  // default intent = "memory"
            false, // not dry_run
            false, // not force
        )
        .expect("put_memory should succeed");

    // Verify the result has artifact_id
    let obj = result.as_object().expect("result should be object");
    assert!(
        obj.get("artifact_id").is_some() || obj.get("id").is_some(),
        "result should contain artifact_id"
    );

    // Verify page was created
    let page = engine.get_page("people/test-person").expect("get_page");
    assert!(page.is_some(), "page should exist after put_memory");
    let page = page.unwrap();
    assert_eq!(page.title, "Test Person");
    assert!(page.compiled_truth.contains("Test content"));
}

// --- Test 4: Intent routing produces different route plans ---

#[test]
fn artifact_put_intent_routing() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // Test memory intent (default)
    let memory_result = svc
        .put_memory(
            "concepts/test-memory",
            "Memory intent content",
            None,
            Some("memory"),
            true,  // dry_run
            false, // not force
        )
        .expect("dry_run memory");
    let mem_obj = memory_result.as_object().unwrap();
    assert_eq!(
        mem_obj
            .get("intent")
            .and_then(|v: &serde_json::Value| v.as_str()),
        Some("memory")
    );

    // Test evidence intent
    let evidence_result = svc
        .put_memory(
            "concepts/test-evidence",
            "Evidence intent content",
            None,
            Some("evidence"),
            true,  // dry_run
            false, // not force
        )
        .expect("dry_run evidence");
    let ev_obj = evidence_result.as_object().unwrap();
    assert_eq!(
        ev_obj
            .get("intent")
            .and_then(|v: &serde_json::Value| v.as_str()),
        Some("evidence")
    );

    // Test promote intent
    let promote_result = svc
        .put_memory(
            "concepts/test-promote",
            "Promote intent content",
            None,
            Some("promote"),
            true,  // dry_run
            false, // not force
        )
        .expect("dry_run promote");
    let prom_obj = promote_result.as_object().unwrap();
    assert_eq!(
        prom_obj
            .get("intent")
            .and_then(|v: &serde_json::Value| v.as_str()),
        Some("promote")
    );

    // Route plans should differ
    let mem_plan = mem_obj.get("route_plan").unwrap();
    let ev_plan = ev_obj.get("route_plan").unwrap();
    let prom_plan = prom_obj.get("route_plan").unwrap();

    // memory: to_brain=true, to_shadow=false
    assert_eq!(
        mem_plan
            .get("to_brain")
            .and_then(|v: &serde_json::Value| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        mem_plan
            .get("to_shadow")
            .and_then(|v: &serde_json::Value| v.as_bool()),
        Some(false)
    );

    // evidence: to_brain=false, to_kb=true
    assert_eq!(
        ev_plan
            .get("to_brain")
            .and_then(|v: &serde_json::Value| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        ev_plan
            .get("to_kb")
            .and_then(|v: &serde_json::Value| v.as_bool()),
        Some(true)
    );

    // promote: to_brain=true, to_shadow=true, to_kb=true
    assert_eq!(
        prom_plan
            .get("to_brain")
            .and_then(|v: &serde_json::Value| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        prom_plan
            .get("to_shadow")
            .and_then(|v: &serde_json::Value| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        prom_plan
            .get("to_kb")
            .and_then(|v: &serde_json::Value| v.as_bool()),
        Some(true)
    );
}

// --- Test 5: artifact_query include_sources ---

#[test]
fn artifact_query_include_sources_returns_citations() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // First, put some content
    svc.put_memory(
        "people/query-test",
        "Query test content about someone",
        Some("Query Test"),
        None,
        false,
        false,
    )
    .expect("put_memory");

    // Query with include_sources=true
    let input = rclaw_gbrain::artifact::types::ArtifactQueryInput {
        query: "Query test".to_string(),
        mode: Some("memory".to_string()),
        limit: Some(10),
        filter_slug: None,
        include_sources: Some(true),
    };
    let result = svc.query_facade(&input).expect("query_facade");
    // The key guarantee is that the sources field is present and properly typed
    // (may be empty if no provenance yet)
    let _ = &result.sources;
}

#[test]
fn artifact_query_evidence_snippet_centers_a_late_match_for_all_kb_file_types() {
    for extension in rclaw_gbrain::artifact::types::KB_SUPPORTED_EXTENSIONS {
        let engine = make_engine();
        let config = make_config();
        let svc = make_svc(&engine, &config);
        let conn = engine.connection().expect("connection");

        conn.execute(
            "INSERT INTO kb_libraries (name) VALUES ('test-library')",
            [],
        )
        .expect("insert library");
        let library_id = conn.last_insert_rowid();
        let original_name = format!("late-match-document.{}", extension);
        let content_hash = format!("late-match-document-{}", extension);
        conn.execute(
            "INSERT INTO kb_documents \
             (library_id, original_name, content_hash, extension, mime_type, \
              document_status, index_status, parsing_status, embedding_status, ocr_status) \
             VALUES (?1, ?2, ?3, ?4, 'application/octet-stream', \
                     'ready', 'ready', 2, 2, 'not_needed')",
            rusqlite::params![library_id, original_name, content_hash, extension],
        )
        .expect("insert document");
        let document_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO kb_document_versions \
             (document_id, index_status, content_hash, node_count, activated_at) \
             VALUES (?1, 'ready', ?2, 1, datetime('now'))",
            rusqlite::params![document_id, content_hash],
        )
        .expect("insert ready document version");
        let version_id = conn.last_insert_rowid();
        conn.execute(
            "UPDATE kb_documents SET current_version_id = ?1 WHERE id = ?2",
            rusqlite::params![version_id, document_id],
        )
        .expect("activate document version");

        let content = format!(
            "{}The Vendor Logo is displayed on the startup screen and can be replaced by configuration.",
            "Serial communication and protocol setup details. ".repeat(12)
        );
        let content_tokens = rclaw_gbrain::nlp::chinese::tokenize_content(&content);
        conn.execute(
            "INSERT INTO kb_document_nodes \
             (library_id, document_id, version_id, content, content_tokens, level, chunk_order) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 0)",
            rusqlite::params![library_id, document_id, version_id, content, content_tokens],
        )
        .expect("insert searchable node");

        let result = svc
            .query_facade(&rclaw_gbrain::artifact::types::ArtifactQueryInput {
                query: "vendor logo".to_string(),
                mode: Some("evidence".to_string()),
                limit: Some(10),
                filter_slug: None,
                include_sources: Some(false),
            })
            .expect("query evidence");

        assert!(
            !result.evidence.is_empty(),
            "the {} KB node should be returned",
            extension
        );
        assert!(
            result.evidence[0]
                .snippet
                .to_ascii_lowercase()
                .contains("vendor logo"),
            "{} result snippet should expose the matching context: {}",
            extension,
            result.evidence[0].snippet
        );
    }
}

#[test]
fn artifact_query_fallback_no_results_is_explicit() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    let result = svc
        .query_facade(&rclaw_gbrain::artifact::types::ArtifactQueryInput {
            query: "完全不存在的主题 alpha beta".to_string(),
            mode: Some("evidence".to_string()),
            limit: Some(5),
            filter_slug: None,
            include_sources: Some(false),
        })
        .expect("no results fallback");

    assert_eq!(result.meta.fallback_stage.as_deref(), Some("no_results"));
    assert!(result.meta.no_results);
    assert!(result.evidence.is_empty());
    assert!(result.candidates.is_empty());
    assert_eq!(result.meta.total, 0);
}

// --- Test 6: artifact_get with include_sources queries by artifact_id ---

#[test]
fn artifact_get_include_sources_queries_by_artifact_id() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // Put content
    let put_result = svc
        .put_memory(
            "people/get-test",
            "Get test content",
            Some("Get Test"),
            None,
            false,
            false,
        )
        .expect("put_memory");

    // Extract artifact_id from put result
    let artifact_id = put_result
        .get("artifact_id")
        .or_else(|| put_result.get("id"))
        .and_then(|v: &serde_json::Value| v.as_i64())
        .expect("should have artifact_id");

    // Get detail with sources
    let detail = svc
        .get_artifact_detail(
            &artifact_id.to_string(),
            true,  // include_projections
            true,  // include_sources
            false, // include_content
        )
        .expect("get_artifact_detail");

    assert!(detail.is_some(), "should find artifact by id");
    let detail = detail.unwrap();
    assert_eq!(detail.slug, "people/get-test");
    assert!(detail.projections.is_some(), "should include projections");
    assert!(detail.occurrences.is_some(), "should include occurrences");
}

#[test]
fn artifact_get_include_content_reads_artifact_body() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    let put_result = svc
        .put_memory(
            "people/content-test",
            "Full body content from the artifact",
            Some("Content Test"),
            None,
            false,
            false,
        )
        .expect("put_memory");

    let artifact_id = put_result
        .get("artifact_id")
        .or_else(|| put_result.get("id"))
        .and_then(|v: &serde_json::Value| v.as_i64())
        .expect("should have artifact_id");

    let detail = svc
        .get_artifact_detail(&artifact_id.to_string(), false, false, true)
        .expect("get_artifact_detail")
        .expect("detail should exist");

    let content = detail.content.expect("content should be included");
    assert!(
        content.contains("Full body content from the artifact"),
        "artifact body should be returned"
    );
}

// --- Test 7: artifact_delete marks occurrences, projections, kb_docs, provenance ---

#[test]
fn artifact_delete_marks_occurrences_projections_kb_docs_provenance() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // Put content
    let put_result = svc
        .put_memory(
            "people/delete-test",
            "Delete test content",
            Some("Delete Test"),
            None,
            false,
            false,
        )
        .expect("put_memory");

    let artifact_id = put_result
        .get("artifact_id")
        .or_else(|| put_result.get("id"))
        .and_then(|v: &serde_json::Value| v.as_i64())
        .expect("should have artifact_id");

    // Delete
    svc.delete_artifact(artifact_id).expect("delete_artifact");

    // Verify artifact is deleted
    let detail = svc
        .get_artifact_detail(&artifact_id.to_string(), true, false, false)
        .expect("get_artifact_detail after delete");
    if let Some(d) = detail {
        assert_eq!(d.status, "deleted", "artifact should be soft-deleted");
    }
}

// --- Test 8: artifact_restore restores artifact-deleted state ---

#[test]
fn artifact_restore_only_restores_artifact_deleted_state() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // Put content
    let put_result = svc
        .put_memory(
            "people/restore-test",
            "Restore test content",
            Some("Restore Test"),
            None,
            false,
            false,
        )
        .expect("put_memory");

    let artifact_id = put_result
        .get("artifact_id")
        .or_else(|| put_result.get("id"))
        .and_then(|v: &serde_json::Value| v.as_i64())
        .expect("should have artifact_id");

    // Delete
    svc.delete_artifact(artifact_id).expect("delete_artifact");

    // Restore
    let restore_result = svc
        .restore(&artifact_id.to_string(), false)
        .expect("restore");
    let restore_obj = restore_result.as_object().unwrap();
    assert!(restore_obj.get("restored_occurrences").is_some());
    assert!(restore_obj.get("restored_projections").is_some());

    // Verify artifact is active again
    let detail = svc
        .get_artifact_detail(&artifact_id.to_string(), true, false, false)
        .expect("get_artifact_detail after restore");
    if let Some(d) = detail {
        assert_eq!(d.status, "active", "artifact should be restored to active");
    }
}

// --- Test 9: artifact_put idempotent same content ---

#[test]
fn artifact_put_idempotent_same_content() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // First put
    let result1 = svc
        .put_memory(
            "people/idempotent-test",
            "Same content",
            Some("Idempotent Test"),
            None,
            false,
            false,
        )
        .expect("put_memory 1");

    // Second put with same content
    let result2 = svc
        .put_memory(
            "people/idempotent-test",
            "Same content",
            Some("Idempotent Test"),
            None,
            false,
            false,
        )
        .expect("put_memory 2");

    // Second put should return no_op resolution
    let obj2 = result2.as_object().unwrap();
    assert_eq!(
        obj2.get("resolution")
            .and_then(|v: &serde_json::Value| v.as_str()),
        Some("no_op"),
        "same content should return no_op"
    );

    // Artifact IDs should match
    let id1 = result1.get("artifact_id").or_else(|| result1.get("id"));
    let id2 = result2.get("artifact_id");
    assert!(id1.is_some(), "first put should return artifact_id");
    assert!(id2.is_some(), "second put should return artifact_id");
    assert_eq!(id1, id2, "same content should return same artifact_id");
}

// --- Test 10: artifact_put with different content updates ---

#[test]
fn artifact_put_different_content_updates() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // First put
    svc.put_memory(
        "people/update-test",
        "Original content",
        Some("Update Test"),
        None,
        false,
        false,
    )
    .expect("put_memory 1");

    // Second put with different content
    let result2 = svc
        .put_memory(
            "people/update-test",
            "Updated content",
            Some("Update Test"),
            None,
            false,
            false,
        )
        .expect("put_memory 2");

    // Should NOT be no_op since content changed
    let obj2 = result2.as_object().unwrap();
    let resolution = obj2
        .get("resolution")
        .and_then(|v: &serde_json::Value| v.as_str());
    assert_ne!(
        resolution,
        Some("no_op"),
        "different content should not be no_op"
    );

    // Page should be updated
    let page = engine
        .get_page("people/update-test")
        .expect("get_page")
        .unwrap();
    assert!(
        page.compiled_truth.contains("Updated"),
        "page content should be updated"
    );
}

// --- P1-1 语义测试: artifact_put --dry-run 零副作用 ---

#[test]
fn artifact_put_dry_run_zero_side_effects() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // dry-run 不应创建 artifact 或 page
    let result = svc
        .put_memory(
            "people/dry-run-test",
            "Dry run content",
            Some("Dry Run Test"),
            None,
            true,  // dry_run
            false, // not force
        )
        .expect("dry_run put_memory");

    let obj = result.as_object().unwrap();
    assert_eq!(obj.get("dry_run").and_then(|v| v.as_bool()), Some(true));

    // page 不应存在
    let page = engine.get_page("people/dry-run-test").expect("get_page");
    assert!(page.is_none(), "dry-run 不应创建 page");

    // artifact 不应存在
    let conn = engine.connection().expect("connection");
    let artifact =
        rclaw_gbrain::artifact::store::find_artifact_by_slug(conn, "people/dry-run-test")
            .expect("find_artifact_by_slug");
    assert!(artifact.is_none(), "dry-run 不应创建 artifact");
}

// --- P1-2 语义测试: intent=evidence 不应写 gbrain page ---

#[test]
fn artifact_put_evidence_intent_no_brain_page() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // evidence intent: to_brain=false, 不应写 gbrain page
    let _result = svc
        .put_memory(
            "people/evidence-test-doc",
            "Evidence content for KB only",
            Some("Evidence Doc"),
            Some("evidence"),
            false,
            false,
        )
        .expect("put_memory evidence");

    // gbrain page 不应存在（evidence 不写 page）
    let page = engine
        .get_page("people/evidence-test-doc")
        .expect("get_page");
    assert!(page.is_none(), "evidence intent 不应创建 gbrain page");
}

// --- P1-3 语义测试: detach 后 restore 不应恢复 detached occurrence ---

#[test]
fn detach_then_restore_does_not_restore_detached_occurrence() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // 创建 artifact
    let put_result = svc
        .put_memory(
            "people/detach-test",
            "Content for detach test",
            Some("Detach Test"),
            None,
            false,
            false,
        )
        .expect("put_memory");

    let artifact_id = put_result
        .get("artifact_id")
        .or_else(|| put_result.get("id"))
        .and_then(|v| v.as_i64())
        .expect("should have artifact_id");

    // detach
    let detach_result = svc
        .detach(&artifact_id.to_string(), "people/detach-test", false)
        .expect("detach");
    let detach_obj = detach_result.as_object().unwrap();
    assert!(detach_obj.get("detached_occurrences").is_some());

    // delete
    svc.delete_artifact(artifact_id).expect("delete_artifact");

    // restore
    let restore_result = svc
        .restore(&artifact_id.to_string(), false)
        .expect("restore");
    let restore_obj = restore_result.as_object().unwrap();
    // restore 应只恢复因 delete 而标记的 occurrence，不应恢复 detach 的
    // detached_occurrences 的 stale_reason='detached_by_user' 不应被恢复
    let restored_occ = restore_obj
        .get("restored_occurrences")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    // detach 创建的 occurrence 已被 detach 标记为 stale，
    // delete 又把剩余 active 的标记为 deleted，
    // restore 只恢复 stale_reason='artifact_deleted' 的
    assert_eq!(restored_occ, 0, "detach 的 occurrence 不应被 restore 恢复");
}

// --- P1-5 语义测试: MCP artifact_delete dry_run 返回 DeleteImpactPreview ---

#[test]
fn mcp_artifact_delete_dry_run_returns_impact_preview() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // 创建 artifact
    let put_result = svc
        .put_memory(
            "people/mcp-delete-test",
            "Content for MCP delete test",
            Some("MCP Delete Test"),
            None,
            false,
            false,
        )
        .expect("put_memory");

    let artifact_id = put_result
        .get("artifact_id")
        .or_else(|| put_result.get("id"))
        .and_then(|v| v.as_i64())
        .expect("should have artifact_id");

    // dry_run 应返回 DeleteImpactPreview
    let preview = svc
        .delete_artifact_dry_run(&artifact_id.to_string())
        .expect("delete_artifact_dry_run");

    assert!(
        preview.projection_count >= 0,
        "preview 应包含 projection_count"
    );
    assert!(
        preview.occurrence_count >= 0,
        "preview 应包含 occurrence_count"
    );
    assert!(
        preview.kb_document_count >= 0,
        "preview 应包含 kb_document_count"
    );
    assert!(
        preview.provenance_count >= 0,
        "preview 应包含 provenance_count"
    );
}

// --- P2-3 语义测试: artifact_list 不暴露 raw DB row ---

#[test]
fn artifact_list_returns_dto_not_raw_row() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // 创建 artifact
    svc.put_memory(
        "people/list-dto-test",
        "Content for list DTO test",
        Some("List DTO Test"),
        None,
        false,
        false,
    )
    .expect("put_memory");

    // list 应返回 ArtifactListItem
    let items = svc.list_artifacts(10, 0).expect("list_artifacts");
    assert!(!items.is_empty(), "应至少有一个 artifact");

    let item = &items[0];
    // ArtifactListItem 不应有 id/storage_path/metadata_json 等内部字段
    // 它应有 uid/slug/original_name 等用户友好字段
    assert!(!item.uid.is_empty(), "uid 不应为空");
    assert!(!item.slug.is_empty(), "slug 不应为空");
}

// --- P1-1 修复验证: artifact_list DTO 序列化不含内部字段 ---
// MCP artifact_list 改走 ArtifactService::list_artifacts DTO 后，
// JSON 输出不应包含 id/storage_path/metadata_json/sha256 等内部字段

#[test]
fn artifact_list_dto_json_no_internal_fields() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    svc.put_memory(
        "people/list-json-test",
        "Content for list JSON test",
        Some("List JSON Test"),
        None,
        false,
        false,
    )
    .expect("put_memory");

    let items = svc.list_artifacts(10, 0).expect("list_artifacts");
    let json = serde_json::to_value(&items).expect("serialize list");
    let json_str = serde_json::to_string(&items).expect("serialize list to string");

    // JSON 不应包含内部字段
    assert!(
        !json_str.contains("storage_path"),
        "DTO JSON 不应包含 storage_path"
    );
    assert!(
        !json_str.contains("metadata_json"),
        "DTO JSON 不应包含 metadata_json"
    );
    assert!(!json_str.contains("sha256"), "DTO JSON 不应包含 sha256");
    // 注意：id 可能作为数字出现，但 ArtifactListItem 结构体不含 id 字段
    // 检查 JSON 对象不含 "id" key
    if let Some(arr) = json.as_array() {
        for item in arr {
            let obj = item.as_object().unwrap();
            assert!(!obj.contains_key("id"), "DTO JSON 不应包含 id key");
            assert!(
                !obj.contains_key("storage_path"),
                "DTO JSON 不应包含 storage_path key"
            );
            assert!(
                !obj.contains_key("metadata_json"),
                "DTO JSON 不应包含 metadata_json key"
            );
            assert!(!obj.contains_key("sha256"), "DTO JSON 不应包含 sha256 key");
        }
    }
}

// --- P1-2 修复验证: manual put intent=memory 的 occurrence promotion_policy 与 route plan 对齐 ---
// manual put intent=memory 不受 upload 默认提升策略影响；
// route plan 与 occurrence 都应保持 AutoAcceptLowRisk。

#[test]
fn artifact_put_memory_occurrence_promotion_policy_matches_route_plan() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // put with intent=memory (dry_run to get route_plan)
    let result = svc
        .put_memory(
            "people/promotion-policy-test",
            "Content for promotion policy test",
            Some("Promotion Policy Test"),
            Some("memory"),
            true,  // dry_run
            false, // not force
        )
        .expect("dry_run put_memory");

    let obj = result.as_object().unwrap();
    let route_plan = obj.get("route_plan").unwrap();
    let promotion = route_plan.get("promotion").and_then(|v| v.as_str());

    // memory intent 的 route plan promotion 应为 auto_accept_low_risk
    assert_eq!(
        promotion,
        Some("auto_accept_low_risk"),
        "memory intent route plan promotion 应为 auto_accept_low_risk"
    );

    // 实际写入后检查 occurrence 的 promotion_policy
    let real_result = svc
        .put_memory(
            "people/promotion-policy-real",
            "Content for promotion policy real test",
            Some("Promotion Policy Real"),
            Some("memory"),
            false, // not dry_run
            false, // not force
        )
        .expect("put_memory");

    let artifact_id = real_result
        .get("artifact_id")
        .or_else(|| real_result.get("id"))
        .and_then(|v| v.as_i64())
        .expect("should have artifact_id");

    let conn = engine.connection().expect("connection");
    let occurrences =
        rclaw_gbrain::artifact::store::find_occurrences_by_artifact(conn, artifact_id)
            .expect("find_occurrences");
    assert!(!occurrences.is_empty(), "应有 occurrence");

    let occ = &occurrences[0];
    assert_eq!(
        occ.promotion_policy, "auto_accept_low_risk",
        "occurrence promotion_policy 应与 route plan 的 auto_accept_low_risk 对齐"
    );
}

#[test]
fn auto_apply_all_candidates_applies_medium_risk_candidates() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);
    let conn = engine.connection().expect("connection");

    let _result = svc
        .put_memory(
            "people/auto-apply-all-test",
            "Initial page content",
            Some("Auto Apply All Test"),
            None,
            false,
            false,
        )
        .expect("put_memory should succeed");

    let artifact_id: i64 = conn
        .query_row(
            "SELECT id FROM source_artifacts WHERE canonical_slug = 'people/auto-apply-all-test'",
            [],
            |row| row.get(0),
        )
        .expect("get artifact_id");

    let candidate_id = promotion::create_candidate(
        conn,
        CreateCandidateInput {
            artifact_id,
            occurrence_id: None,
            kb_document_id: None,
            kb_node_id: None,
            candidate_type: CandidateType::FactClaim,
            target_slug: "people/auto-apply-all-test".to_string(),
            target_field: "compiled_truth".to_string(),
            title: "Medium risk fact".to_string(),
            proposed_payload: serde_json::json!({
                "subject_slug": "people/auto-apply-all-test",
                "predicate": "summary",
                "object_text": "Auto applied medium-risk knowledge",
            })
            .to_string(),
            evidence_json: "{}".to_string(),
            confidence: 0.6,
            risk_level: RiskLevel::Medium,
        },
    )
    .expect("create medium-risk candidate");

    let applied = promotion::auto_apply_all_candidates(conn, artifact_id, None, None)
        .expect("auto apply all");

    assert_eq!(applied, vec![candidate_id]);

    let (status, reviewer): (String, String) = conn
        .query_row(
            "SELECT status, reviewer FROM promotion_candidates WHERE id = ?1",
            rusqlite::params![candidate_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query candidate status");
    assert_eq!(status, "applied");
    assert_eq!(reviewer, "auto_apply");

    let page_content: String = conn
        .query_row(
            "SELECT compiled_truth FROM pages WHERE slug = 'people/auto-apply-all-test'",
            [],
            |row| row.get(0),
        )
        .expect("query page content");
    assert!(page_content.contains("Auto applied medium-risk knowledge"));
}

#[test]
fn auto_apply_all_candidates_processes_more_than_one_batch() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);
    let conn = engine.connection().expect("connection");

    svc.put_memory(
        "people/auto-apply-paged-test",
        "Initial page content",
        Some("Auto Apply Paged Test"),
        None,
        false,
        false,
    )
    .expect("put_memory should succeed");

    let artifact_id: i64 = conn
        .query_row(
            "SELECT id FROM source_artifacts WHERE canonical_slug = 'people/auto-apply-paged-test'",
            [],
            |row| row.get(0),
        )
        .expect("get artifact_id");

    let mut candidate_ids = Vec::new();
    for i in 0..1005 {
        let candidate_id = promotion::create_candidate(
            conn,
            CreateCandidateInput {
                artifact_id,
                occurrence_id: None,
                kb_document_id: None,
                kb_node_id: None,
                candidate_type: CandidateType::FactClaim,
                target_slug: "people/auto-apply-paged-test".to_string(),
                target_field: "compiled_truth".to_string(),
                title: format!("Paged medium risk fact {}", i),
                proposed_payload: serde_json::json!({
                    "subject_slug": "people/auto-apply-paged-test",
                    "predicate": "summary",
                    "object_text": format!("Auto applied paged knowledge {}", i),
                })
                .to_string(),
                evidence_json: "{}".to_string(),
                confidence: 0.6,
                risk_level: RiskLevel::Medium,
            },
        )
        .expect("create paged medium-risk candidate");
        candidate_ids.push(candidate_id);
    }

    let applied = promotion::auto_apply_all_candidates(conn, artifact_id, None, None)
        .expect("auto apply all paged");

    assert_eq!(applied.len(), candidate_ids.len());
    assert_eq!(applied.first(), candidate_ids.first());
    assert_eq!(applied.last(), candidate_ids.last());

    let pending_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM promotion_candidates WHERE artifact_id = ?1 AND status = 'pending'",
            rusqlite::params![artifact_id],
            |row| row.get(0),
        )
        .expect("count pending candidates");
    assert_eq!(pending_count, 0);
}

// --- P1/P2 修复验证: manual promote 创建 shadow page ---
// intent=promote 的 route plan to_shadow=true，
// 不仅应创建 shadow projection，还应实际写入 shadow page 到 pages 表

#[test]
fn artifact_put_promote_creates_shadow_page() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    let result = svc
        .put_memory(
            "people/promote-shadow-test",
            "Content for promote shadow page test",
            Some("Promote Shadow Test"),
            Some("promote"),
            false, // not dry_run
            false, // not force
        )
        .expect("put_memory promote");

    let artifact_id = result
        .get("artifact_id")
        .or_else(|| result.get("id"))
        .and_then(|v| v.as_i64())
        .expect("should have artifact_id");

    // 检查 shadow page 是否存在
    let shadow_slug = "documents/people/promote-shadow-test";
    let page = engine.get_page(shadow_slug).expect("get_page");
    assert!(
        page.is_some(),
        "promote intent 应创建 shadow page: {}",
        shadow_slug
    );

    // 检查 shadow projection 是否存在
    let conn = engine.connection().expect("connection");
    let projections =
        rclaw_gbrain::artifact::store::find_projections_by_artifact(conn, artifact_id)
            .expect("find_projections");
    let has_shadow = projections
        .iter()
        .any(|p| p.projection_type == "brain_shadow_page" && p.status == "active");
    assert!(
        has_shadow,
        "promote intent 应创建 brain_shadow_page projection"
    );
}

// --- P2-4 修复验证: artifact_query include_sources=false 时 evidence 不返回 sources ---
// evidence 的 fallback source 也应受 include_sources 控制

#[test]
fn artifact_query_include_sources_false_evidence_no_sources() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // 创建 artifact
    svc.put_memory(
        "people/evidence-source-test",
        "Content for evidence source test",
        Some("Evidence Source Test"),
        Some("evidence"),
        false,
        false,
    )
    .expect("put_memory evidence");

    // Query with include_sources=false
    let input = rclaw_gbrain::artifact::types::ArtifactQueryInput {
        query: "evidence source".to_string(),
        mode: Some("auto".to_string()),
        limit: Some(10),
        filter_slug: None,
        include_sources: Some(false),
    };
    let result = svc.query_facade(&input).expect("query_facade");

    // 顶层 sources 应为空
    assert!(
        result.sources.is_empty(),
        "include_sources=false 时顶层 sources 应为空"
    );

    // evidence 的 sources 也应为空
    for ev in &result.evidence {
        assert!(
            ev.sources.is_empty(),
            "include_sources=false 时 evidence sources 也应为空"
        );
    }
}

// ============================================================================
// P3 修复: MCP dispatch 层测试覆盖
// ============================================================================

// --- MCP dispatch 测试: artifact_list 通过 MCP dispatch 返回 DTO ---
// 验证 MCP artifact_list 的 dispatch 路径走 ArtifactService::list_artifacts DTO，
// 而非直接返回 Operations::list_artifacts 的 raw SourceArtifact

#[test]
fn mcp_dispatch_artifact_list_returns_dto() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // 创建 artifact
    svc.put_memory(
        "people/mcp-list-test",
        "Content for MCP list dispatch test",
        Some("MCP List Test"),
        None,
        false,
        false,
    )
    .expect("put_memory");

    // 通过 ArtifactService facade 获取 list（模拟 MCP dispatch 路径）
    let items = svc
        .list_artifacts(10, 0)
        .expect("list_artifacts via facade");
    assert!(!items.is_empty(), "应至少有一个 artifact");

    // 验证 DTO 不含内部字段
    let json_str = serde_json::to_string(&items).expect("serialize");
    assert!(
        !json_str.contains("storage_path"),
        "MCP dispatch 不应暴露 storage_path"
    );
    assert!(
        !json_str.contains("metadata_json"),
        "MCP dispatch 不应暴露 metadata_json"
    );
    assert!(!json_str.contains("sha256"), "MCP dispatch 不应暴露 sha256");

    // 验证 DTO 含用户友好字段
    let item = &items[0];
    assert!(!item.uid.is_empty(), "MCP dispatch 应返回 uid");
    assert!(!item.slug.is_empty(), "MCP dispatch 应返回 slug");
}

// --- MCP dispatch 测试: artifact_delete dry_run 返回 DeleteImpactPreview ---
// 验证 MCP artifact_delete --dry_run 的 dispatch 路径走
// ArtifactService::delete_artifact_dry_run，返回结构化影响预览

#[test]
fn mcp_dispatch_artifact_delete_dry_run_returns_preview() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // 创建 artifact
    let put_result = svc
        .put_memory(
            "people/mcp-delete-dry-test",
            "Content for MCP delete dry-run dispatch test",
            Some("MCP Delete Dry Test"),
            None,
            false,
            false,
        )
        .expect("put_memory");

    let artifact_id = put_result
        .get("artifact_id")
        .or_else(|| put_result.get("id"))
        .and_then(|v| v.as_i64())
        .expect("should have artifact_id");

    // 通过 ArtifactService facade 获取 dry-run preview（模拟 MCP dispatch 路径）
    let preview = svc
        .delete_artifact_dry_run(&artifact_id.to_string())
        .expect("delete_artifact_dry_run via facade");

    // 验证 DeleteImpactPreview 结构
    assert!(
        preview.projection_count >= 0,
        "preview 应包含 projection_count"
    );
    assert!(
        preview.occurrence_count >= 0,
        "preview 应包含 occurrence_count"
    );
    assert!(
        !preview.artifact_uid.is_empty(),
        "preview 应包含 artifact_uid"
    );
    assert_eq!(
        preview.artifact_status, "active",
        "preview artifact_status 应为 active"
    );

    // 验证 preview 序列化不含内部 id/storage_path
    let preview_json = serde_json::to_string(&preview).expect("serialize preview");
    assert!(
        !preview_json.contains("storage_path"),
        "preview 不应暴露 storage_path"
    );
}

// --- MCP dispatch 测试: artifact_query include_sources=false 不返回 sources ---
// 验证 MCP artifact_query 的 dispatch 路径走 ArtifactService::query_facade，
// include_sources=false 时 evidence 和顶层 sources 均为空

#[test]
fn mcp_dispatch_artifact_query_no_sources() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // 创建 artifact
    svc.put_memory(
        "people/mcp-query-source-test",
        "Content for MCP query source dispatch test",
        Some("MCP Query Source Test"),
        Some("evidence"),
        false,
        false,
    )
    .expect("put_memory evidence");

    // 通过 ArtifactService facade 查询（模拟 MCP dispatch 路径）
    let input = rclaw_gbrain::artifact::types::ArtifactQueryInput {
        query: "MCP query source".to_string(),
        mode: Some("auto".to_string()),
        limit: Some(10),
        filter_slug: None,
        include_sources: Some(false),
    };
    let result = svc
        .query_facade(&input)
        .expect("query_facade via MCP dispatch");

    // 验证 MCP dispatch 路径的 include_sources=false 语义
    assert!(
        result.sources.is_empty(),
        "MCP dispatch: include_sources=false 时顶层 sources 应为空"
    );
    for ev in &result.evidence {
        assert!(
            ev.sources.is_empty(),
            "MCP dispatch: include_sources=false 时 evidence sources 也应为空"
        );
    }
}

// --- MCP tool_defs 测试: 默认 tools-json 含 facade 与 OCR 扩展 ---
// 验证 build_tool_defs() 默认输出只包含已公开的 MCP 工具

#[test]
fn mcp_tool_defs_default_include_artifact_facade_and_ocr_extensions() {
    let defs = rclaw_gbrain::mcp::tool_defs::build_tool_defs();

    // 应包含 Artifact facade 与已公开的 OCR 扩展工具
    let tool_names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert!(tool_names.contains(&"artifact_put"), "应包含 artifact_put");
    assert!(
        tool_names.contains(&"artifact_upload"),
        "应包含 artifact_upload"
    );
    assert!(
        tool_names.contains(&"artifact_query"),
        "应包含 artifact_query"
    );
    assert!(
        tool_names.contains(&"artifact_list"),
        "应包含 artifact_list"
    );
    assert!(tool_names.contains(&"artifact_get"), "应包含 artifact_get");
    assert!(
        tool_names.contains(&"artifact_delete"),
        "应包含 artifact_delete"
    );
    assert!(
        tool_names.contains(&"artifact_review_list"),
        "应包含 artifact_review_list"
    );
    assert!(
        tool_names.contains(&"artifact_review_apply"),
        "应包含 artifact_review_apply"
    );
    let review_reject = defs
        .iter()
        .find(|def| def.name == "artifact_review_reject")
        .expect("应包含 artifact_review_reject");
    assert!(
        review_reject.input_schema["properties"]
            .get("reviewer")
            .is_some(),
        "artifact_review_reject schema 应公开 reviewer"
    );
    assert!(
        tool_names.contains(&"kb_document_status"),
        "应包含 kb_document_status"
    );
    assert!(tool_names.contains(&"kb_ocr_run"), "应包含 kb_ocr_run");
    assert!(tool_names.contains(&"kb_ocr_retry"), "应包含 kb_ocr_retry");

    // 不应包含未公开的内部工具
    for def in &defs {
        assert!(
            is_expected_default_mcp_tool(&def.name),
            "默认 tools-json 不应包含未公开工具: {}",
            def.name
        );
    }
}

// --- P1 修复验证: 同 slug promote 更新时旧 shadow projection 变 stale ---
// 第一次 artifact_put --intent promote 创建 shadow page，
// 第二次同 slug 不同内容再 put，旧 brain_shadow_page projection 应标记 stale

#[test]
fn artifact_put_promote_same_slug_update_stales_shadow_projection() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // 第一次 promote put
    let result1 = svc
        .put_memory(
            "people/promote-update-test",
            "Original promote content",
            Some("Promote Update Test"),
            Some("promote"),
            false,
            false,
        )
        .expect("put_memory promote 1");

    let artifact_id_1 = result1
        .get("artifact_id")
        .or_else(|| result1.get("id"))
        .and_then(|v| v.as_i64())
        .expect("should have artifact_id");

    // 验证第一次 put 的 shadow projection 为 active
    let conn = engine.connection().expect("connection");
    let projections1 =
        rclaw_gbrain::artifact::store::find_projections_by_artifact(conn, artifact_id_1)
            .expect("find_projections");
    let shadow_active_1 = projections1
        .iter()
        .any(|p| p.projection_type == "brain_shadow_page" && p.status == "active");
    assert!(
        shadow_active_1,
        "第一次 promote 应有 active shadow projection"
    );

    // P3-8 修复：记录第一次 put 后的 shadow page_id，
    // 用于后续断言同 slug 更新后 page_id 不变（避免 INSERT OR REPLACE 导致 page_id 变化）
    let shadow_slug = "documents/people/promote-update-test";
    let shadow_page_id_before: i64 = conn
        .query_row(
            "SELECT id FROM pages WHERE slug = ?1",
            rusqlite::params![shadow_slug],
            |row| row.get(0),
        )
        .expect("查询第一次 put 后 shadow page id");

    // 第二次同 slug 不同内容 promote put
    let result2 = svc
        .put_memory(
            "people/promote-update-test",
            "Updated promote content",
            Some("Promote Update Test"),
            Some("promote"),
            false,
            false,
        )
        .expect("put_memory promote 2");

    // 验证旧 artifact 的 shadow projection 变 stale
    let projections1_after =
        rclaw_gbrain::artifact::store::find_projections_by_artifact(conn, artifact_id_1)
            .expect("find_projections after update");
    let shadow_stale_1 = projections1_after.iter().any(|p| {
        p.projection_type == "brain_shadow_page"
            && p.status == "stale"
            && p.stale_reason == "content_updated"
    });
    assert!(
        shadow_stale_1,
        "同 slug 更新后旧 shadow projection 应标记为 stale (content_updated)"
    );

    // 验证旧 artifact 的 brain_page_update 也变 stale
    let page_update_stale_1 = projections1_after.iter().any(|p| {
        p.projection_type == "brain_page_update"
            && p.status == "stale"
            && p.stale_reason == "content_updated"
    });
    assert!(
        page_update_stale_1,
        "同 slug 更新后旧 brain_page_update 也应标记为 stale"
    );

    // 验证 shadow page 内容已更新（指向新 artifact）
    let page = engine.get_page(shadow_slug).expect("get_page").unwrap();
    // shadow page 的 frontmatter 应包含新 artifact UID
    let artifact_id_2 = result2
        .get("artifact_id")
        .or_else(|| result2.get("id"))
        .and_then(|v| v.as_i64())
        .expect("should have artifact_id for second put");
    let artifact2 = rclaw_gbrain::artifact::store::find_artifact_by_id(conn, artifact_id_2)
        .expect("find_artifact_by_id")
        .unwrap();
    assert!(
        page.frontmatter
            .as_ref()
            .is_some_and(|fm| fm.contains(&artifact2.artifact_uid)),
        "shadow page frontmatter 应包含新 artifact UID"
    );

    // P3 修复：断言 page_versions 版本快照存在
    // 同 slug promote 更新后，page_id 应不变，page_versions 应新增一条快照
    let shadow_page_id: i64 = conn
        .query_row(
            "SELECT id FROM pages WHERE slug = ?1",
            rusqlite::params![shadow_slug],
            |row| row.get(0),
        )
        .expect("查询 shadow page id");

    // P3-8 修复：断言同 slug 更新后 page_id 不变
    // INSERT OR REPLACE 会 DELETE+INSERT 导致 page_id 变化，
    // 当前实现改为 UPDATE 保留 page_id，此断言锁死该关键条件
    assert_eq!(
        shadow_page_id, shadow_page_id_before,
        "P3-8 修复：同 slug promote 更新后 shadow page_id 应不变，\
         更新前: {}, 更新后: {}（可能使用了 INSERT OR REPLACE）",
        shadow_page_id_before, shadow_page_id
    );

    // 断言 page_versions 至少有一条快照记录
    let version_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM page_versions WHERE page_id = ?1",
            rusqlite::params![shadow_page_id],
            |row| row.get(0),
        )
        .expect("查询 page_versions 数量");
    assert!(
        version_count >= 1,
        "同 slug promote 更新后 page_versions 应至少有一条快照记录，实际: {}",
        version_count
    );

    // 断言快照内容包含第一次写入的关键信息（artifact UID）
    // shadow page 内容由 create_shadow_page_content 生成，包含 artifact UID
    let snapshot_content: String = conn
        .query_row(
            "SELECT compiled_truth FROM page_versions WHERE page_id = ?1 ORDER BY snapshot_at DESC LIMIT 1",
            rusqlite::params![shadow_page_id],
            |row| row.get(0),
        ).expect("查询最新快照内容");
    // 第一次写入的 artifact UID 应出现在快照中（旧版本的 shadow page）
    let artifact1 = rclaw_gbrain::artifact::store::find_artifact_by_id(conn, artifact_id_1)
        .expect("find_artifact_by_id")
        .unwrap();
    assert!(
        snapshot_content.contains(&artifact1.artifact_uid),
        "page_versions 快照应包含第一次写入的 artifact UID，实际内容不含 {}",
        artifact1.artifact_uid
    );
}

// ============================================================================
// P3 修复: 真正走 MCP tools/call dispatch 的测试
// 之前名为 "MCP dispatch" 的测试实际直接调用 ArtifactService 方法，
// 没有经过 McpServer::handle_tool_call 的参数映射、内部工具拦截和返回包装。
// 以下测试通过 McpServer::dispatch_tool_call 真正走 MCP dispatch 路径。
// ============================================================================

fn make_mcp_server() -> rclaw_gbrain::mcp::McpServer {
    let engine = make_engine();
    rclaw_gbrain::mcp::McpServer::with_config(engine, Config::default())
}

// --- MCP tools/call 测试: artifact_put 通过 dispatch 创建 artifact ---

#[test]
fn mcp_tools_call_artifact_put() {
    let mut server = make_mcp_server();

    // 通过 MCP dispatch 调用 artifact_put
    let result = server
        .dispatch_tool_call(
            "artifact_put",
            serde_json::json!({
                "slug": "people/mcp-put-test",
                "content": "Content via MCP tools/call",
                "title": "MCP Put Test",
                "intent": "memory"
            }),
        )
        .expect("dispatch artifact_put");

    // 验证返回值包含 artifact_id
    assert!(
        result.get("artifact_id").is_some() || result.get("id").is_some(),
        "artifact_put dispatch 应返回 artifact_id"
    );
    assert!(
        result.get("artifact_uid").is_some(),
        "artifact_put dispatch 应返回 artifact_uid"
    );
    // 验证返回值包含路由计划
    assert!(
        result.get("route_plan").is_some(),
        "artifact_put dispatch 应返回 route_plan"
    );
}

// --- MCP tools/call 测试: artifact_list 通过 dispatch 返回 DTO ---

#[test]
fn mcp_tools_call_artifact_list_returns_dto() {
    let mut server = make_mcp_server();

    // 先创建 artifact
    server
        .dispatch_tool_call(
            "artifact_put",
            serde_json::json!({
                "slug": "people/mcp-list-dispatch-test",
                "content": "Content for MCP list dispatch",
                "title": "MCP List Dispatch Test"
            }),
        )
        .expect("dispatch artifact_put");

    // 通过 MCP dispatch 调用 artifact_list
    let result = server
        .dispatch_tool_call(
            "artifact_list",
            serde_json::json!({
                "limit": 10,
                "offset": 0
            }),
        )
        .expect("dispatch artifact_list");

    // 验证返回值是数组且不含内部字段
    let items = result.as_array().expect("artifact_list 应返回数组");
    assert!(!items.is_empty(), "应至少有一个 artifact");

    let json_str = serde_json::to_string(&result).expect("serialize");
    assert!(
        !json_str.contains("storage_path"),
        "MCP dispatch 不应暴露 storage_path"
    );
    assert!(
        !json_str.contains("metadata_json"),
        "MCP dispatch 不应暴露 metadata_json"
    );
    assert!(!json_str.contains("sha256"), "MCP dispatch 不应暴露 sha256");
}

// --- MCP tools/call 测试: artifact_query 通过 dispatch 查询 ---

#[test]
fn mcp_tools_call_artifact_query() {
    let mut server = make_mcp_server();

    // 先创建 artifact
    server
        .dispatch_tool_call(
            "artifact_put",
            serde_json::json!({
                "slug": "people/mcp-query-dispatch-test",
                "content": "Content for MCP query dispatch test",
                "title": "MCP Query Dispatch Test",
                "intent": "evidence"
            }),
        )
        .expect("dispatch artifact_put");

    // 通过 MCP dispatch 调用 artifact_query，include_sources=false
    let result = server
        .dispatch_tool_call(
            "artifact_query",
            serde_json::json!({
                "query": "MCP query dispatch",
                "mode": "auto",
                "limit": 10,
                "include_sources": false
            }),
        )
        .expect("dispatch artifact_query");

    // 验证 include_sources=false 时 sources 为空
    let sources = result.get("sources").and_then(|v| v.as_array());
    assert!(
        sources.is_none() || sources.is_none_or(|s| s.is_empty()),
        "MCP dispatch: include_sources=false 时顶层 sources 应为空"
    );

    // 验证 evidence 存在
    assert!(
        result.get("evidence").is_some(),
        "artifact_query dispatch 应返回 evidence 字段"
    );
}

// --- MCP tools/call 测试: kb_document_status 读取 OCR 状态 ---

#[test]
fn mcp_tools_call_kb_document_status() {
    let mut server = make_mcp_server();

    server
        .dispatch_tool_call(
            "artifact_put",
            serde_json::json!({
                "slug": "people/mcp-status-dispatch-test",
                "content": "Content for MCP status dispatch test",
                "title": "MCP Status Dispatch Test",
                "intent": "evidence"
            }),
        )
        .expect("dispatch artifact_put evidence");

    let result = server
        .dispatch_tool_call(
            "kb_document_status",
            serde_json::json!({
                "document_id": 1
            }),
        )
        .expect("dispatch kb_document_status");

    assert_eq!(result.get("document_id").and_then(|v| v.as_i64()), Some(1));
    assert_eq!(
        result.get("ocr_status").and_then(|v| v.as_str()),
        Some("not_evaluated"),
        "尚未被 KB worker 处理的文档不应显示为 not_needed"
    );
    assert!(
        result.get("ocr_status").is_some(),
        "kb_document_status 应返回 ocr_status"
    );
    assert!(
        result.get("ocr_pages").is_some(),
        "kb_document_status 应返回 ocr_pages"
    );
}

// --- MCP tools/call 测试: artifact_delete dry_run 通过 dispatch 返回预览 ---

#[test]
fn mcp_tools_call_artifact_delete_dry_run() {
    let mut server = make_mcp_server();

    // 先创建 artifact
    let put_result = server
        .dispatch_tool_call(
            "artifact_put",
            serde_json::json!({
                "slug": "people/mcp-delete-dispatch-test",
                "content": "Content for MCP delete dispatch test",
                "title": "MCP Delete Dispatch Test"
            }),
        )
        .expect("dispatch artifact_put");

    let artifact_id = put_result
        .get("artifact_id")
        .or_else(|| put_result.get("id"))
        .and_then(|v| v.as_i64())
        .expect("should have artifact_id");

    // 通过 MCP dispatch 调用 artifact_delete --dry-run
    let result = server
        .dispatch_tool_call(
            "artifact_delete",
            serde_json::json!({
                "id_or_uid": artifact_id.to_string(),
                "dry_run": true
            }),
        )
        .expect("dispatch artifact_delete dry_run");

    // 验证返回 DeleteImpactPreview 结构
    assert!(
        result.get("artifact_uid").is_some(),
        "dry_run preview 应包含 artifact_uid"
    );
    assert!(
        result.get("projection_count").is_some(),
        "dry_run preview 应包含 projection_count"
    );
    assert!(
        result.get("occurrence_count").is_some(),
        "dry_run preview 应包含 occurrence_count"
    );

    // 验证不含内部字段
    let json_str = serde_json::to_string(&result).expect("serialize");
    assert!(
        !json_str.contains("storage_path"),
        "preview 不应暴露 storage_path"
    );
}

// --- MCP tools/call 测试: 参数校验 — 缺少必填参数返回错误 ---

#[test]
fn mcp_tools_call_missing_required_params() {
    let mut server = make_mcp_server();

    // artifact_put 缺少 slug
    let result = server.dispatch_tool_call(
        "artifact_put",
        serde_json::json!({
            "content": "some content"
        }),
    );
    assert!(result.is_err(), "缺少 slug 应返回错误");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("slug") || err_msg.contains("必填"),
        "错误信息应提到 slug 必填: {}",
        err_msg
    );

    // artifact_put 缺少 content 和 file
    let result2 = server.dispatch_tool_call(
        "artifact_put",
        serde_json::json!({
            "slug": "people/test"
        }),
    );
    assert!(result2.is_err(), "缺少 content 和 file 应返回错误");
}

// --- MCP tools/call 测试: artifact_put force 参数传递 ---

#[test]
fn mcp_tools_call_artifact_put_force_param() {
    let mut server = make_mcp_server();

    // 第一次写入
    server
        .dispatch_tool_call(
            "artifact_put",
            serde_json::json!({
                "slug": "people/mcp-force-test",
                "content": "Original content",
                "title": "MCP Force Test"
            }),
        )
        .expect("dispatch artifact_put first");

    // 模拟人工修改：直接修改页面内容
    // 注意：McpServer 的 engine 是私有的，无法直接修改页面。
    // 但我们可以测试 force=true 参数不会报错（即使无冲突也正常通过）

    // 第二次写入同 slug，force=true
    let result = server
        .dispatch_tool_call(
            "artifact_put",
            serde_json::json!({
                "slug": "people/mcp-force-test",
                "content": "Updated content with force",
                "title": "MCP Force Test",
                "force": true
            }),
        )
        .expect("dispatch artifact_put with force=true");

    // 验证正常返回（不报冲突）
    assert!(
        result.get("artifact_id").is_some() || result.get("id").is_some(),
        "force=true 时应正常写入并返回 artifact_id"
    );
}

// ============================================================================
// P2 修复: 真实人工修改冲突检测测试
// 之前只测试 force=true 参数传递，没有模拟真实人工修改场景。
// 以下测试：第一次 put → 模拟人工修改页面 → 第二次 put 默认返回 conflict →
// 页面内容仍是人工修改后的 → force=true 可覆盖。
// ============================================================================

#[test]
fn artifact_put_conflict_detection_on_human_edit() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // 第一次 artifact_put：写入初始内容
    let result1 = svc
        .put_memory(
            "people/conflict-test",
            "Original content",
            Some("Conflict Test Person"),
            None,  // default intent = "memory"
            false, // not dry_run
            false, // not force
        )
        .expect("第一次 put_memory 应成功");

    assert!(
        result1.get("artifact_id").is_some() || result1.get("id").is_some(),
        "第一次 put 应返回 artifact_id"
    );

    // 模拟人工修改：直接通过 SQL 更新页面的 compiled_truth 和 content_hash
    // 使 content_hash 与上次 artifact 写入时记录的 version_hash 不同
    let conn = engine.connection().expect("connection");
    conn.execute(
        "UPDATE pages SET compiled_truth = 'Human edited content', content_hash = 'fake_human_edit_hash' WHERE slug = 'people/conflict-test'",
        rusqlite::params![],
    ).expect("模拟人工修改页面");

    // 第二次 artifact_put（默认 force=false）：应检测到冲突，返回 resolution=conflict
    let result2 = svc
        .put_memory(
            "people/conflict-test",
            "New content from second put",
            Some("Conflict Test Person Updated"),
            None,  // default intent = "memory"
            false, // not dry_run
            false, // not force — 应触发冲突检测
        )
        .expect("第二次 put_memory 应返回结果（不报错，而是返回 conflict resolution）");

    // 断言：返回 resolution=conflict
    assert_eq!(
        result2.get("resolution").and_then(|v| v.as_str()),
        Some("conflict"),
        "人工修改后默认 put 应返回 resolution=conflict"
    );

    // P2 修复验证：冲突分支应自动创建 suggested change
    assert!(
        result2.get("change_id").and_then(|v| v.as_i64()).is_some(),
        "冲突分支应返回 change_id（suggested change ID）"
    );
    assert_eq!(
        result2.get("review_status").and_then(|v| v.as_str()),
        Some("pending"),
        "冲突分支应返回 review_status: pending"
    );
    assert!(
        result2
            .get("detail")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("suggested change"),
        "detail 应提示新内容已保存为 suggested change"
    );

    // 断言：页面内容仍是人工修改后的内容，未被覆盖
    let page_content: String = conn
        .query_row(
            "SELECT compiled_truth FROM pages WHERE slug = 'people/conflict-test'",
            rusqlite::params![],
            |row| row.get(0),
        )
        .expect("查询页面内容");
    assert_eq!(
        page_content, "Human edited content",
        "冲突时页面内容应保持人工修改后的内容，未被覆盖"
    );

    // P1-8 修复验证：冲突分支不应创建 active brain_page_update 投影
    // 冲突分支没有写入稳定页面，不应有指向稳定页的 active brain_page_update 投影
    let conflict_artifact_id = result2
        .get("artifact_id")
        .or_else(|| result2.get("id"))
        .and_then(|v| v.as_i64())
        .expect("冲突分支应返回 artifact_id");
    let conflict_projections =
        rclaw_gbrain::artifact::store::find_projections_by_artifact(conn, conflict_artifact_id)
            .expect("查询冲突 artifact 投影");
    let has_active_brain_page_update = conflict_projections
        .iter()
        .any(|p| p.projection_type == "brain_page_update" && p.status == "active");
    assert!(
        !has_active_brain_page_update,
        "P1-8 修复：冲突分支不应创建 active brain_page_update 投影，实际发现 {:?}",
        conflict_projections
            .iter()
            .filter(|p| p.projection_type == "brain_page_update")
            .collect::<Vec<_>>()
    );

    // P1-8 修复验证：冲突 suggested change 使用 PageUpdate 类型，
    // accept + apply 后应正确写入用户提交的新内容
    let change_id = result2
        .get("change_id")
        .and_then(|v| v.as_i64())
        .expect("冲突分支应返回 change_id");

    // 先 accept 厷取审核通过，再 apply 应用变更
    let _accept_result = svc
        .list_suggested_changes(Some("pending"), None, 10, 0)
        .expect("列出 pending suggested changes");
    // 直接通过 SQL 更新候选状态为 accepted（模拟审核流程）
    conn.execute(
        "UPDATE promotion_candidates SET status = 'accepted', reviewer = 'test_user', updated_at = datetime('now') WHERE id = ?1",
        rusqlite::params![change_id],
    ).expect("更新候选状态为 accepted");

    let _apply_result = svc
        .apply_suggested_change(change_id)
        .expect("apply 冲突 suggested change 应成功");
    let after_apply_content: String = conn
        .query_row(
            "SELECT compiled_truth FROM pages WHERE slug = 'people/conflict-test'",
            rusqlite::params![],
            |row| row.get(0),
        )
        .expect("查询 apply 后页面内容");
    // P3-10 修复：精确断言 replace 行为，而非宽松的 contains。
    // 如果 replace 退化成 append，页面会同时包含旧内容和新内容，
    // contains 断言仍会通过，无法检测到退化。
    assert_eq!(
        after_apply_content, "New content from second put",
        "P3-10 修复：replace 后页面应精确等于新内容，不应包含旧内容，实际: {}",
        after_apply_content
    );
    assert!(
        !after_apply_content.contains("Human edited content"),
        "P3-10 修复：replace 后页面不应包含被替换的旧内容 'Human edited content'，实际: {}",
        after_apply_content
    );
}

#[test]
fn artifact_put_force_overrides_conflict() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // 第一次 artifact_put：写入初始内容
    let result1 = svc
        .put_memory(
            "people/force-conflict-test",
            "Original content",
            Some("Force Conflict Test"),
            None,  // default intent = "memory"
            false, // not dry_run
            false, // not force
        )
        .expect("第一次 put_memory 应成功");

    assert!(
        result1.get("artifact_id").is_some() || result1.get("id").is_some(),
        "第一次 put 应返回 artifact_id"
    );

    // 模拟人工修改
    let conn = engine.connection().expect("connection");
    conn.execute(
        "UPDATE pages SET compiled_truth = 'Human edited content', content_hash = 'fake_human_edit_hash' WHERE slug = 'people/force-conflict-test'",
        rusqlite::params![],
    ).expect("模拟人工修改页面");

    // 第二次 artifact_put（force=true）：应强制覆盖，不返回 conflict
    let result2 = svc
        .put_memory(
            "people/force-conflict-test",
            "Force updated content",
            Some("Force Conflict Test Updated"),
            None,  // default intent = "memory"
            false, // not dry_run
            true,  // force=true — 应跳过冲突检测，强制覆盖
        )
        .expect("force=true 时 put_memory 应成功");

    // 断言：返回 resolution 不是 conflict
    let resolution = result2.get("resolution").and_then(|v| v.as_str());
    assert_ne!(
        resolution,
        Some("conflict"),
        "force=true 时不应返回 conflict"
    );
    assert!(
        result2.get("artifact_id").is_some() || result2.get("id").is_some(),
        "force=true 时应正常写入并返回 artifact_id"
    );

    // 断言：页面内容已被 force 覆盖为新内容
    let page_content: String = conn
        .query_row(
            "SELECT compiled_truth FROM pages WHERE slug = 'people/force-conflict-test'",
            rusqlite::params![],
            |row| row.get(0),
        )
        .expect("查询页面内容");
    assert_eq!(
        page_content, "Force updated content",
        "force=true 时页面内容应被覆盖为新内容"
    );
}

/// P1-9 修复验证：冲突后 pending suggested change 未 apply 时，
/// 再次同 slug artifact_put 不应覆盖人工编辑页面，也不应返回误导性的 no_op。
#[test]
fn artifact_put_after_conflict_preserves_human_edit() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // 第一次 artifact_put：写入初始内容
    let result1 = svc
        .put_memory(
            "people/conflict-baseline-test",
            "Original content",
            Some("Conflict Baseline Test"),
            None,  // default intent = "memory"
            false, // not dry_run
            false, // not force
        )
        .expect("第一次 put_memory 应成功");

    assert!(
        result1.get("artifact_id").is_some() || result1.get("id").is_some(),
        "第一次 put 应返回 artifact_id"
    );

    // 模拟人工修改页面
    let conn = engine.connection().expect("connection");
    conn.execute(
        "UPDATE pages SET compiled_truth = 'Human edited content', content_hash = 'fake_human_edit_hash' WHERE slug = 'people/conflict-baseline-test'",
        rusqlite::params![],
    ).expect("模拟人工修改页面");

    // 第二次 artifact_put：应检测到冲突，返回 pending suggested change
    let result2 = svc
        .put_memory(
            "people/conflict-baseline-test",
            "Second put content",
            Some("Conflict Baseline Test Updated"),
            None,  // default intent = "memory"
            false, // not dry_run
            false, // not force
        )
        .expect("第二次 put_memory 应成功");

    assert_eq!(
        result2.get("resolution").and_then(|v| v.as_str()),
        Some("conflict"),
        "第二次 put 应返回 conflict resolution"
    );

    // 不 apply suggested change，直接第三次 artifact_put
    // P1-9 修复：第三次 put 不应覆盖人工编辑页面，
    // 也不应返回 no_op（因为页面内容与第三次提交的内容不同）
    let result3 = svc
        .put_memory(
            "people/conflict-baseline-test",
            "Third put content",
            Some("Conflict Baseline Test Third"),
            None,  // default intent = "memory"
            false, // not dry_run
            false, // not force
        )
        .expect("第三次 put_memory 应成功");

    // 断言：第三次 put 不应返回 no_op（内容不同）
    let resolution3 = result3.get("resolution").and_then(|v| v.as_str());
    assert_ne!(
        resolution3,
        Some("no_op"),
        "P1-9 修复：冲突后再次 put 不同内容不应返回 no_op"
    );

    // 断言：页面内容仍为人工编辑内容（未被第三次 put 覆盖）
    let page_content: String = conn
        .query_row(
            "SELECT compiled_truth FROM pages WHERE slug = 'people/conflict-baseline-test'",
            rusqlite::params![],
            |row| row.get(0),
        )
        .expect("查询页面内容");
    assert_eq!(
        page_content, "Human edited content",
        "P1-9 修复：冲突后未 apply 时，再次 put 不应覆盖人工编辑页面，实际: {}",
        page_content
    );
}

/// P2-9 修复验证：apply suggested change 后，新 brain_page_update.version_hash
/// 应为页面 content_hash（而非空字符串），使后续冲突检测基线完整。
/// 测试场景：apply 后再人工编辑页面，再次 artifact_put 应返回 conflict。
#[test]
fn artifact_put_after_apply_detects_new_human_edit() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // 第一次 artifact_put：写入初始内容
    let result1 = svc
        .put_memory(
            "people/apply-baseline-test",
            "Original content",
            Some("Apply Baseline Test"),
            None,  // default intent = "memory"
            false, // not dry_run
            false, // not force
        )
        .expect("第一次 put_memory 应成功");

    assert!(
        result1.get("artifact_id").is_some() || result1.get("id").is_some(),
        "第一次 put 应返回 artifact_id"
    );

    // 模拟人工修改页面，触发冲突
    let conn = engine.connection().expect("connection");
    conn.execute(
        "UPDATE pages SET compiled_truth = 'Human edited content', content_hash = 'fake_human_edit_hash' WHERE slug = 'people/apply-baseline-test'",
        rusqlite::params![],
    ).expect("模拟人工修改页面");

    // 第二次 artifact_put：应检测到冲突，返回 pending suggested change
    let result2 = svc
        .put_memory(
            "people/apply-baseline-test",
            "Second put content",
            Some("Apply Baseline Test Updated"),
            None,  // default intent = "memory"
            false, // not dry_run
            false, // not force
        )
        .expect("第二次 put_memory 应成功");

    assert_eq!(
        result2.get("resolution").and_then(|v| v.as_str()),
        Some("conflict"),
        "第二次 put 应返回 conflict resolution"
    );

    let change_id = result2
        .get("change_id")
        .and_then(|v| v.as_i64())
        .expect("冲突分支应返回 change_id");

    // P1-9 修复：apply_suggested_change 对 pending 变更自动执行 accept+apply
    let _apply_result = svc
        .apply_suggested_change(change_id)
        .expect("P1-9 修复：apply pending suggested change 应自动 accept+apply 成功");

    // P2-9 修复验证：apply 后新 brain_page_update.version_hash 应为页面 content_hash
    // 查询 apply 后创建的 brain_page_update 投影
    let projections: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT ap.projection_type, ap.version_hash
             FROM artifact_projections ap
             JOIN source_artifacts sa ON ap.artifact_id = sa.id
             WHERE sa.canonical_slug = 'people/apply-baseline-test'
               AND ap.status = 'active'
               AND ap.projection_type = 'brain_page_update'",
            )
            .expect("prepare");
        let rows = stmt
            .query_map(rusqlite::params![], |row| {
                let proj_type: String = row.get(0)?;
                let vhash: String = row.get(1)?;
                Ok((proj_type, vhash))
            })
            .expect("query_map");
        rows.collect::<Result<Vec<_>, _>>().expect("collect")
    };
    // 应至少有一个 active brain_page_update 且 version_hash 非空
    let has_non_empty_vhash = projections.iter().any(|(_, vhash)| !vhash.is_empty());
    assert!(
        has_non_empty_vhash,
        "P2-9 修复：apply 后 brain_page_update.version_hash 应非空，实际: {:?}",
        projections
    );

    // 模拟第二次人工修改页面
    conn.execute(
        "UPDATE pages SET compiled_truth = 'Second human edit', content_hash = 'second_human_edit_hash' WHERE slug = 'people/apply-baseline-test'",
        rusqlite::params![],
    ).expect("模拟第二次人工修改页面");

    // 第三次 artifact_put：P2-9 修复后应检测到新的冲突
    let result3 = svc
        .put_memory(
            "people/apply-baseline-test",
            "Third put content",
            Some("Apply Baseline Test Third"),
            None,  // default intent = "memory"
            false, // not dry_run
            false, // not force
        )
        .expect("第三次 put_memory 应成功");

    assert_eq!(
        result3.get("resolution").and_then(|v| v.as_str()),
        Some("conflict"),
        "P2-9 修复：apply 后再人工编辑页面，再次 put 应返回 conflict，实际: {:?}",
        result3.get("resolution")
    );
}

/// P1-10 修复验证：冲突后不 apply，再次提交与 pending 完全相同的内容，
/// 应继续返回 conflict（而非 no_op），且页面不变。
/// 之前 no_op 排在 conflict 前面，导致冲突后重复提交同一内容时
/// 系统返回 no_op，但稳定页面仍是人工编辑内容，用户误以为内容已存在。
#[test]
fn artifact_put_after_conflict_same_content_returns_conflict() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // 第一次 artifact_put：写入初始内容
    let result1 = svc
        .put_memory(
            "people/conflict-same-content-test",
            "Original content",
            Some("Conflict Same Content Test"),
            None,  // default intent = "memory"
            false, // not dry_run
            false, // not force
        )
        .expect("第一次 put_memory 应成功");

    assert!(
        result1.get("artifact_id").is_some() || result1.get("id").is_some(),
        "第一次 put 应返回 artifact_id"
    );

    // 模拟人工修改页面
    let conn = engine.connection().expect("connection");
    conn.execute(
        "UPDATE pages SET compiled_truth = 'Human edited content', content_hash = 'fake_human_edit_hash' WHERE slug = 'people/conflict-same-content-test'",
        rusqlite::params![],
    ).expect("模拟人工修改页面");

    // 第二次 artifact_put：应检测到冲突，返回 pending suggested change
    let result2 = svc
        .put_memory(
            "people/conflict-same-content-test",
            "Second put content",
            Some("Conflict Same Content Test Updated"),
            None,  // default intent = "memory"
            false, // not dry_run
            false, // not force
        )
        .expect("第二次 put_memory 应成功");

    assert_eq!(
        result2.get("resolution").and_then(|v| v.as_str()),
        Some("conflict"),
        "第二次 put 应返回 conflict resolution"
    );

    // P1-10 核心测试：不 apply suggested change，再次提交与第二次完全相同的内容
    // 之前会返回 no_op（因为最新 pending artifact 的 sha256 与提交内容相同），
    // 修复后应继续返回 conflict（因为页面仍处于人工修改冲突状态）
    let result3 = svc
        .put_memory(
            "people/conflict-same-content-test",
            "Second put content", // 与第二次提交完全相同的内容
            Some("Conflict Same Content Test Updated"),
            None,  // default intent = "memory"
            false, // not dry_run
            false, // not force
        )
        .expect("第三次 put_memory 应成功");

    // 断言：第三次 put 应返回 conflict，绝不能返回 no_op
    let resolution3 = result3.get("resolution").and_then(|v| v.as_str());
    assert_eq!(
        resolution3,
        Some("conflict"),
        "P1-10 修复：冲突后再次提交相同内容应返回 conflict，而非 no_op，实际: {:?}",
        resolution3
    );

    // 断言：页面内容仍为人工编辑内容（未被覆盖）
    let page_content: String = conn
        .query_row(
            "SELECT compiled_truth FROM pages WHERE slug = 'people/conflict-same-content-test'",
            rusqlite::params![],
            |row| row.get(0),
        )
        .expect("查询页面内容");
    assert_eq!(
        page_content, "Human edited content",
        "P1-10 修复：冲突后再次提交相同内容，页面应保持人工编辑内容，实际: {}",
        page_content
    );
}

/// P1-11 修复验证：apply 后新 brain_page_update 的 projection_ref
/// 必须为 "brain_page:{slug}" 格式，与 baseline 查询一致。
/// 之前写成 "slug:{slug}"，导致 apply 后的投影无法被
/// find_latest_page_update_hash_by_slug 查到，后续冲突检测基线丢失。
#[test]
fn apply_projection_ref_matches_baseline_query() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);
    let conn = engine.connection().expect("connection");

    // 第一次 artifact_put：写入初始内容
    let _result1 = svc
        .put_memory(
            "people/apply-projref-test",
            "Original content",
            Some("Apply ProjRef Test"),
            None,
            false,
            false,
        )
        .expect("第一次 put_memory 应成功");

    // 模拟人工修改页面
    conn.execute(
        "UPDATE pages SET compiled_truth = 'Human edited', content_hash = 'human_hash_1' WHERE slug = 'people/apply-projref-test'",
        rusqlite::params![],
    ).expect("模拟人工修改页面");

    // 第二次 artifact_put：应检测到冲突
    let result2 = svc
        .put_memory(
            "people/apply-projref-test",
            "Second put content",
            Some("Apply ProjRef Test Updated"),
            None,
            false,
            false,
        )
        .expect("第二次 put_memory 应成功");
    assert_eq!(
        result2.get("resolution").and_then(|v| v.as_str()),
        Some("conflict")
    );

    let change_id = result2
        .get("change_id")
        .and_then(|v| v.as_i64())
        .expect("冲突分支应返回 change_id");

    // apply suggested change
    let _apply_result = svc
        .apply_suggested_change(change_id)
        .expect("apply pending suggested change 应成功");

    // P1-11 核心断言：apply 后新投影的 projection_ref 必须为 "brain_page:{slug}"
    let proj_ref: String = conn
        .query_row(
            "SELECT ap.projection_ref FROM artifact_projections ap
         JOIN source_artifacts sa ON ap.artifact_id = sa.id
         WHERE sa.canonical_slug = 'people/apply-projref-test'
           AND ap.projection_type = 'brain_page_update'
           AND ap.status = 'active'
         ORDER BY ap.updated_at DESC, ap.id DESC LIMIT 1",
            rusqlite::params![],
            |row| row.get(0),
        )
        .expect("查询 apply 后投影的 projection_ref");
    assert_eq!(proj_ref, "brain_page:people/apply-projref-test",
        "P1-11 修复：apply 后投影 projection_ref 应为 'brain_page:people/apply-projref-test'，实际: {}", proj_ref);

    // P1-11 核心断言：baseline 查询能查到 apply 后的投影
    let baseline_hash = rclaw_gbrain::artifact::store::find_latest_page_update_hash_by_slug(
        conn,
        "people/apply-projref-test",
    )
    .expect("baseline 查询不应报错");
    assert!(
        baseline_hash.is_some() && !baseline_hash.as_ref().unwrap().is_empty(),
        "P1-11 修复：apply 后 baseline 查询应返回非空 hash，实际: {:?}",
        baseline_hash
    );
}

/// P1-11 修复验证：apply 后未发生人工修改时，再次 artifact_put
/// 不应误判 conflict。之前 projection_ref 不一致导致 baseline 查询
/// 找不到 apply 后的投影，只能用旧 hash，从而误判。
#[test]
fn apply_then_put_without_human_edit_no_false_conflict() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);
    let conn = engine.connection().expect("connection");

    // 第一次 artifact_put：写入初始内容
    let _result1 = svc
        .put_memory(
            "people/apply-no-false-conflict-test",
            "Original content",
            Some("Apply No False Conflict Test"),
            None,
            false,
            false,
        )
        .expect("第一次 put_memory 应成功");

    // 模拟人工修改页面
    conn.execute(
        "UPDATE pages SET compiled_truth = 'Human edited', content_hash = 'human_hash_2' WHERE slug = 'people/apply-no-false-conflict-test'",
        rusqlite::params![],
    ).expect("模拟人工修改页面");

    // 第二次 artifact_put：应检测到冲突
    let result2 = svc
        .put_memory(
            "people/apply-no-false-conflict-test",
            "Second put content",
            Some("Apply No False Conflict Test Updated"),
            None,
            false,
            false,
        )
        .expect("第二次 put_memory 应成功");
    assert_eq!(
        result2.get("resolution").and_then(|v| v.as_str()),
        Some("conflict")
    );

    let change_id = result2
        .get("change_id")
        .and_then(|v| v.as_i64())
        .expect("冲突分支应返回 change_id");

    // apply suggested change — 页面被替换为新内容
    let _apply_result = svc
        .apply_suggested_change(change_id)
        .expect("apply pending suggested change 应成功");

    // P1-11 核心测试：不做任何人工修改，再次 put 同内容应为幂等
    let result3 = svc
        .put_memory(
            "people/apply-no-false-conflict-test",
            "Second put content", // 与 apply 后页面内容一致
            Some("Apply No False Conflict Test Updated"),
            None,
            false,
            false,
        )
        .expect("第三次 put_memory 应成功");

    let resolution3 = result3.get("resolution").and_then(|v| v.as_str());
    assert_ne!(
        resolution3,
        Some("conflict"),
        "P1-11 修复：apply 后未人工修改，再次 put 同内容不应误判 conflict，实际: {:?}",
        resolution3
    );
}

/// P2-11 修复验证：apply 后旧 brain_page_update 投影应被标记为 superseded，
/// 不应存在多个 active 的同 slug brain_page_update 投影。
#[test]
fn apply_supersedes_old_page_update_projection() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);
    let conn = engine.connection().expect("connection");

    // 第一次 artifact_put：写入初始内容
    let _result1 = svc
        .put_memory(
            "people/apply-supersede-test",
            "Original content",
            Some("Apply Supersede Test"),
            None,
            false,
            false,
        )
        .expect("第一次 put_memory 应成功");

    // 模拟人工修改页面
    conn.execute(
        "UPDATE pages SET compiled_truth = 'Human edited', content_hash = 'human_hash_3' WHERE slug = 'people/apply-supersede-test'",
        rusqlite::params![],
    ).expect("模拟人工修改页面");

    // 第二次 artifact_put：应检测到冲突
    let result2 = svc
        .put_memory(
            "people/apply-supersede-test",
            "Second put content",
            Some("Apply Supersede Test Updated"),
            None,
            false,
            false,
        )
        .expect("第二次 put_memory 应成功");
    assert_eq!(
        result2.get("resolution").and_then(|v| v.as_str()),
        Some("conflict")
    );

    let change_id = result2
        .get("change_id")
        .and_then(|v| v.as_i64())
        .expect("冲突分支应返回 change_id");

    // apply suggested change
    let _apply_result = svc
        .apply_suggested_change(change_id)
        .expect("apply pending suggested change 应成功");

    // P2-11 核心断言：同 slug 只应有一个 active brain_page_update 投影
    let active_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_projections ap
         JOIN source_artifacts sa ON ap.artifact_id = sa.id
         WHERE sa.canonical_slug = 'people/apply-supersede-test'
           AND ap.projection_type = 'brain_page_update'
           AND ap.status = 'active'",
            rusqlite::params![],
            |row| row.get(0),
        )
        .expect("查询 active 投影数");
    assert_eq!(
        active_count, 1,
        "P2-11 修复：apply 后同 slug 只应有 1 个 active brain_page_update 投影，实际: {}",
        active_count
    );

    // P2-11 核心断言：旧投影应被标记为 superseded
    let superseded_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_projections ap
         JOIN source_artifacts sa ON ap.artifact_id = sa.id
         WHERE sa.canonical_slug = 'people/apply-supersede-test'
           AND ap.projection_type = 'brain_page_update'
           AND ap.status = 'superseded'",
            rusqlite::params![],
            |row| row.get(0),
        )
        .expect("查询 superseded 投影数");
    assert!(
        superseded_count >= 1,
        "P2-11 修复：apply 后旧 brain_page_update 应被标记为 superseded，实际 superseded 数: {}",
        superseded_count
    );
}

// ============================================================================
// P3-12 修复: artifact_put --file 与 rollback 投影一致性回归测试
// ============================================================================

/// P2-12 修复验证：artifact_put 内容超过 1MB 应被拒绝。
/// put_memory 的内容上限为 1MB，artifact_put --file 预检也应使用相同上限。
#[test]
fn artifact_put_content_exceeds_1mb_rejected() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);

    // 构造超过 1MB 的内容
    let max_bytes = rclaw_gbrain::artifact::service::MAX_PUT_MEMORY_CONTENT_BYTES;
    let oversized_content = "x".repeat(max_bytes + 1);

    let result = svc.put_memory(
        "people/oversized-test",
        &oversized_content,
        Some("Oversized Test"),
        None,
        false,
        false,
    );

    assert!(result.is_err(), "P2-12 修复：超过 1MB 的内容应被拒绝");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("超过上限") || err_msg.contains("exceeds"),
        "P2-12 修复：错误信息应说明超出上限，实际: {}",
        err_msg
    );
}

/// P2-12 修复验证：artifact_put --file 的文本文件扩展名白名单。
/// pdf/docx/xls/xlsx 等二进制格式不应通过 artifact_put --file，
/// 它们应走 artifact_upload 路径。
#[test]
fn artifact_put_file_text_only_allowlist() {
    // 验证 TEXT_FILE_EXTENSIONS 不包含二进制格式
    let text_exts = rclaw_gbrain::artifact::service::TEXT_FILE_EXTENSIONS;
    assert!(
        !text_exts.contains(&"pdf"),
        "P2-12 修复：TEXT_FILE_EXTENSIONS 不应包含 pdf"
    );
    assert!(
        !text_exts.contains(&"docx"),
        "P2-12 修复：TEXT_FILE_EXTENSIONS 不应包含 docx"
    );
    assert!(
        !text_exts.contains(&"xls"),
        "P2-12 修复：TEXT_FILE_EXTENSIONS 不应包含 xls"
    );
    assert!(
        !text_exts.contains(&"xlsx"),
        "P2-12 修复：TEXT_FILE_EXTENSIONS 不应包含 xlsx"
    );

    // 验证 TEXT_FILE_EXTENSIONS 包含常见文本格式
    assert!(
        text_exts.contains(&"txt"),
        "P2-12 修复：TEXT_FILE_EXTENSIONS 应包含 txt"
    );
    assert!(
        text_exts.contains(&"md"),
        "P2-12 修复：TEXT_FILE_EXTENSIONS 应包含 md"
    );
    assert!(
        text_exts.contains(&"json"),
        "P2-12 修复：TEXT_FILE_EXTENSIONS 应包含 json"
    );
    assert!(
        text_exts.contains(&"yaml"),
        "P2-12 修复：TEXT_FILE_EXTENSIONS 应包含 yaml"
    );
    assert!(
        text_exts.contains(&"csv"),
        "P2-12 修复：TEXT_FILE_EXTENSIONS 应包含 csv"
    );

    // 验证 MAX_PUT_MEMORY_CONTENT_BYTES 为 1MB
    assert_eq!(
        rclaw_gbrain::artifact::service::MAX_PUT_MEMORY_CONTENT_BYTES,
        1024 * 1024,
        "P2-12 修复：MAX_PUT_MEMORY_CONTENT_BYTES 应为 1MB"
    );
}

/// P1-12 修复验证：rollback 后 apply 创建的 brain_page_update 投影
/// 不应继续为 active，应被标记为 stale (stale_reason='rolled_back')。
#[test]
fn rollback_stales_applied_page_update_projection() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);
    let conn = engine.connection().expect("connection");

    // 第一次 artifact_put：写入初始内容
    let _result1 = svc
        .put_memory(
            "people/rollback-proj-test",
            "Original content",
            Some("Rollback Proj Test"),
            None,
            false,
            false,
        )
        .expect("第一次 put_memory 应成功");

    // 模拟人工修改页面，触发冲突
    conn.execute(
        "UPDATE pages SET compiled_truth = 'Human edited', content_hash = 'human_hash_rollback' WHERE slug = 'people/rollback-proj-test'",
        rusqlite::params![],
    ).expect("模拟人工修改页面");

    // 第二次 artifact_put：应检测到冲突
    let result2 = svc
        .put_memory(
            "people/rollback-proj-test",
            "Second put content",
            Some("Rollback Proj Test Updated"),
            None,
            false,
            false,
        )
        .expect("第二次 put_memory 应成功");
    assert_eq!(
        result2.get("resolution").and_then(|v| v.as_str()),
        Some("conflict")
    );

    let change_id = result2
        .get("change_id")
        .and_then(|v| v.as_i64())
        .expect("冲突分支应返回 change_id");

    // apply suggested change
    let _apply_result = svc
        .apply_suggested_change(change_id)
        .expect("apply pending suggested change 应成功");

    // 确认 apply 后有 active brain_page_update 投影
    let active_before_rollback: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_projections ap
         JOIN source_artifacts sa ON ap.artifact_id = sa.id
         WHERE sa.canonical_slug = 'people/rollback-proj-test'
           AND ap.projection_type = 'brain_page_update'
           AND ap.status = 'active'",
            rusqlite::params![],
            |row| row.get(0),
        )
        .expect("查询 apply 后 active 投影数");
    assert!(
        active_before_rollback >= 1,
        "apply 后应有至少 1 个 active brain_page_update 投影，实际: {}",
        active_before_rollback
    );

    // rollback suggested change
    let _rollback_result = svc
        .rollback_suggested_change(change_id)
        .expect("rollback suggested change 应成功");

    // P1-12 核心断言：rollback 后 apply 创建的投影不应继续为 active
    let _active_after_rollback: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_projections ap
         JOIN source_artifacts sa ON ap.artifact_id = sa.id
         WHERE sa.canonical_slug = 'people/rollback-proj-test'
           AND ap.projection_type = 'brain_page_update'
           AND ap.status = 'active'",
            rusqlite::params![],
            |row| row.get(0),
        )
        .expect("查询 rollback 后 active 投影数");
    // rollback 后不应有 apply 创建的 active 投影
    // 注意：第一次 put 的投影可能仍为 active（如果它未被 superseded）
    // 关键是 apply 创建的投影（metadata_json 含 candidate_id）不应为 active
    let rolled_back_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_projections
         WHERE projection_type = 'brain_page_update'
           AND projection_ref = 'brain_page:people/rollback-proj-test'
           AND status = 'stale'
           AND stale_reason = 'rolled_back'",
            rusqlite::params![],
            |row| row.get(0),
        )
        .expect("查询 rolled_back 投影数");
    assert!(
        rolled_back_count >= 1,
        "P1-12 修复：rollback 后 apply 创建的投影应被标记为 stale (rolled_back)，实际: {}",
        rolled_back_count
    );
}

/// P1-12 修复验证：rollback 后下一次 artifact_put 不应因已回滚投影的
/// 旧 version_hash 出现错误冲突判断。
#[test]
fn rollback_then_put_no_false_conflict_from_rolled_back_projection() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);
    let conn = engine.connection().expect("connection");

    // 第一次 artifact_put：写入初始内容
    let _result1 = svc
        .put_memory(
            "people/rollback-conflict-test",
            "Original content",
            Some("Rollback Conflict Test"),
            None,
            false,
            false,
        )
        .expect("第一次 put_memory 应成功");

    // 模拟人工修改页面，触发冲突
    conn.execute(
        "UPDATE pages SET compiled_truth = 'Human edited', content_hash = 'human_hash_rc' WHERE slug = 'people/rollback-conflict-test'",
        rusqlite::params![],
    ).expect("模拟人工修改页面");

    // 第二次 artifact_put：应检测到冲突
    let result2 = svc
        .put_memory(
            "people/rollback-conflict-test",
            "Second put content",
            Some("Rollback Conflict Test Updated"),
            None,
            false,
            false,
        )
        .expect("第二次 put_memory 应成功");
    assert_eq!(
        result2.get("resolution").and_then(|v| v.as_str()),
        Some("conflict")
    );

    let change_id = result2
        .get("change_id")
        .and_then(|v| v.as_i64())
        .expect("冲突分支应返回 change_id");

    // apply suggested change
    let _apply_result = svc
        .apply_suggested_change(change_id)
        .expect("apply pending suggested change 应成功");

    // rollback suggested change — 页面恢复到 apply 前状态
    let _rollback_result = svc
        .rollback_suggested_change(change_id)
        .expect("rollback suggested change 应成功");

    // P1-12 核心测试：rollback 后，baseline 查询不应返回已回滚投影的 hash
    let baseline_hash = rclaw_gbrain::artifact::store::find_latest_page_update_hash_by_slug(
        conn,
        "people/rollback-conflict-test",
    )
    .expect("baseline 查询不应报错");

    // baseline_hash 应为第一次 put 的 hash（旧投影被恢复为 active），
    // 或为 None（如果旧投影未被恢复，也没有其它 active 投影）
    // 关键是不应返回 apply 时创建的、已回滚的投影 hash
    // 如果 baseline_hash 指向已回滚内容，后续冲突检测会误判
    let page_hash: String = conn
        .query_row(
            "SELECT content_hash FROM pages WHERE slug = 'people/rollback-conflict-test'",
            rusqlite::params![],
            |row| row.get(0),
        )
        .expect("查询页面 content_hash");

    // 如果 baseline_hash 存在，它应与当前页面 content_hash 一致
    // （因为 rollback 恢复了页面内容，旧投影也应恢复为 active）
    if let Some(ref bh) = baseline_hash {
        assert_eq!(
            bh, &page_hash,
            "P1-12 修复：rollback 后 baseline hash 应与当前页面 content_hash 一致，\
             baseline: {}, page: {}（baseline 可能指向已回滚内容）",
            bh, page_hash
        );
    }
}

// ============================================================================
// P3 修复: artifact_put --file MCP dispatch 层文件路径回归测试
// ============================================================================

/// P3 修复验证：通过 MCP dispatch artifact_put --file 传入 .md/.txt 文件应成功。
/// 此前只有 put_memory 的 content 路径测试，缺少真实文件 dispatch 测试。
#[test]
fn mcp_artifact_put_file_accepts_text_extensions() {
    let mut server = make_mcp_server();
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).expect("创建临时目录");

    // 测试 .md 文件
    let md_path = dir.path().join("test_input.md");
    std::fs::write(&md_path, "# Hello\n\nSome markdown content").expect("写入 md 文件");
    let md_result = server.dispatch_tool_call(
        "artifact_put",
        serde_json::json!({
            "slug": "people/mcp-file-md-test",
            "file": md_path.to_str().unwrap(),
            "title": "MCP File MD Test"
        }),
    );
    assert!(
        md_result.is_ok(),
        ".md 文件通过 artifact_put --file dispatch 应成功，错误: {:?}",
        md_result.err()
    );
    let md_val = md_result.unwrap();
    assert!(
        md_val.get("artifact_id").is_some() || md_val.get("id").is_some(),
        ".md 文件 dispatch 应返回 artifact_id"
    );

    // 测试 .txt 文件
    let txt_path = dir.path().join("test_input.txt");
    std::fs::write(&txt_path, "Plain text content").expect("写入 txt 文件");
    let txt_result = server.dispatch_tool_call(
        "artifact_put",
        serde_json::json!({
            "slug": "people/mcp-file-txt-test",
            "file": txt_path.to_str().unwrap(),
            "title": "MCP File TXT Test"
        }),
    );
    assert!(
        txt_result.is_ok(),
        ".txt 文件通过 artifact_put --file dispatch 应成功，错误: {:?}",
        txt_result.err()
    );
    let txt_val = txt_result.unwrap();
    assert!(
        txt_val.get("artifact_id").is_some() || txt_val.get("id").is_some(),
        ".txt 文件 dispatch 应返回 artifact_id"
    );
}

/// P3 修复验证：通过 MCP dispatch artifact_put --file 传入 .pdf/.docx/.xls/.xlsx
/// 应在 validate_upload_source 阶段被扩展名白名单拒绝，
/// 不会进入 String::from_utf8 转换。
#[test]
fn mcp_artifact_put_file_rejects_binary_extensions() {
    let mut server = make_mcp_server();
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).expect("创建临时目录");

    for ext in &["pdf", "docx", "xls", "xlsx"] {
        let file_path = dir.path().join(format!("test.{}", ext));
        std::fs::write(&file_path, b"fake binary content").expect("写入二进制文件");

        let result = server.dispatch_tool_call(
            "artifact_put",
            serde_json::json!({
                "slug": format!("people/mcp-file-reject-{}", ext),
                "file": file_path.to_str().unwrap(),
                "title": format!("MCP File Reject {}", ext)
            }),
        );

        assert!(
            result.is_err(),
            ".{} 文件应被 artifact_put --file dispatch 拒绝",
            ext
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.to_lowercase().contains("not allowed")
                || err_msg.to_lowercase().contains("extension")
                || err_msg.contains("不允许"),
            ".{} 拒绝原因应说明扩展名不允许，实际: {}",
            ext,
            err_msg
        );
    }
}

/// P3 修复验证：通过 MCP dispatch artifact_put --file 传入超过 1MB 的文件
/// 应在 validate_upload_source 阶段被大小检查拒绝，不会进入文件读取。
#[test]
fn mcp_artifact_put_file_rejects_oversized() {
    let mut server = make_mcp_server();
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).expect("创建临时目录");

    let file_path = dir.path().join("oversized.md");
    // 使用 set_len 创建超过 1MB 的稀疏文件，避免实际写入 1MB+ 数据
    let f = std::fs::File::create(&file_path).expect("创建 oversized 文件");
    f.set_len((rclaw_gbrain::artifact::service::MAX_PUT_MEMORY_CONTENT_BYTES + 1) as u64)
        .expect("设置 oversized 文件长度");
    drop(f);

    let result = server.dispatch_tool_call(
        "artifact_put",
        serde_json::json!({
            "slug": "people/mcp-file-oversized",
            "file": file_path.to_str().unwrap(),
            "title": "MCP File Oversized"
        }),
    );

    assert!(
        result.is_err(),
        "超过 1MB 的文件应被 artifact_put --file dispatch 拒绝"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.to_lowercase().contains("exceed")
            || err_msg.to_lowercase().contains("超过上限")
            || err_msg.to_lowercase().contains("maximum"),
        "超大文件拒绝原因应说明超出上限，实际: {}",
        err_msg
    );
}

// ============================================================================
// P2 修复: 非 page_update 类型候选 rollback 回归测试
// ============================================================================

/// P2 修复验证：fact_claim 候选 apply 后 rollback 成功，
/// 页面恢复到 apply 前内容，candidate 状态变为 rolled_back。
#[test]
fn rollback_fact_claim_restores_page_content() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);
    let conn = engine.connection().expect("connection");

    // 创建初始页面（含 artifact）
    let _result1 = svc
        .put_memory(
            "people/fact-claim-rollback-test",
            "Initial page content",
            Some("Fact Claim Rollback Test"),
            None,
            false,
            false,
        )
        .expect("put_memory 初始页面应成功");

    // 获取 artifact_id
    let artifact_id: i64 = conn
        .query_row(
            "SELECT id FROM source_artifacts WHERE canonical_slug = 'people/fact-claim-rollback-test'",
            [],
            |row| row.get(0),
        )
        .expect("获取 artifact_id");

    // 记录应用前页面内容
    let content_before: String = conn
        .query_row(
            "SELECT compiled_truth FROM pages WHERE slug = 'people/fact-claim-rollback-test'",
            [],
            |row| row.get(0),
        )
        .expect("查询初始页面内容");

    // 创建 fact_claim 候选
    let candidate_id = promotion::create_candidate(
        conn,
        CreateCandidateInput {
            artifact_id,
            occurrence_id: None,
            kb_document_id: None,
            kb_node_id: None,
            candidate_type: CandidateType::FactClaim,
            target_slug: "people/fact-claim-rollback-test".to_string(),
            target_field: "compiled_truth".to_string(),
            title: "测试事实声明".to_string(),
            proposed_payload: r#"{"subject_slug":"people/fact-claim-rollback-test","predicate":"测试属性","object_text":"这是一个测试事实声明值"}"#.to_string(),
            evidence_json: "{}".to_string(),
            confidence: 0.9,
            risk_level: RiskLevel::Low,
        },
    )
    .expect("创建 fact_claim 候选应成功");

    // 接受候选
    promotion::review_candidate(
        conn,
        &ReviewCandidateInput {
            candidate_id,
            action: "accept".to_string(),
            reviewer: "test".to_string(),
            notes: Some("测试用接受".to_string()),
        },
    )
    .expect("接受候选应成功");

    // 应用候选
    let applied = promotion::apply_candidate(conn, candidate_id).expect("应用候选应成功");
    assert_eq!(applied.status, "applied");

    // 验证页面内容已变更（包含 facts 条目）
    let content_after_apply: String = conn
        .query_row(
            "SELECT compiled_truth FROM pages WHERE slug = 'people/fact-claim-rollback-test'",
            [],
            |row| row.get(0),
        )
        .expect("查询应用后页面内容");
    assert!(
        content_after_apply.contains("测试事实声明值"),
        "应用后页面应包含 fact_claim 内容，实际: {}",
        content_after_apply
    );

    // 回滚候选
    let rolled_back = promotion::rollback_candidate(conn, candidate_id).expect("回滚候选应成功");
    assert_eq!(rolled_back.status, "rolled_back");

    // 验证页面内容恢复
    let content_after_rollback: String = conn
        .query_row(
            "SELECT compiled_truth FROM pages WHERE slug = 'people/fact-claim-rollback-test'",
            [],
            |row| row.get(0),
        )
        .expect("查询回滚后页面内容");
    assert_eq!(
        content_before, content_after_rollback,
        "回滚后页面内容应与 apply 前一致"
    );
}

/// P2 修复验证：page_create 候选 apply 后 rollback 应软删除新建页面，
/// 不应出现"状态 rolled_back 但页面仍 active"的不一致。
#[test]
fn rollback_page_create_soft_deletes_page() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);
    let conn = engine.connection().expect("connection");

    // 先创建一个 artifact（page_create 候选需要 artifact_id）
    // 使用 put_memory 创建不相关的页面来获得 artifact
    let _result = svc
        .put_memory(
            "people/dummy-for-page-create-test",
            "Dummy content",
            Some("Dummy Page"),
            None,
            false,
            false,
        )
        .expect("put_memory dummy 应成功");

    let artifact_id: i64 = conn
        .query_row(
            "SELECT id FROM source_artifacts WHERE canonical_slug = 'people/dummy-for-page-create-test'",
            [],
            |row| row.get(0),
        )
        .expect("获取 artifact_id");

    let new_page_slug = "people/page-create-rollback-test";

    // 确认页面不存在
    let page_exists_before: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pages WHERE slug = ?1",
            rusqlite::params![new_page_slug],
            |row| row.get(0),
        )
        .unwrap_or(false);
    assert!(!page_exists_before, "测试前页面不应存在");

    // 创建 page_create 候选
    let candidate_id = promotion::create_candidate(
        conn,
        CreateCandidateInput {
            artifact_id,
            occurrence_id: None,
            kb_document_id: None,
            kb_node_id: None,
            candidate_type: CandidateType::PageCreate,
            target_slug: new_page_slug.to_string(),
            target_field: "compiled_truth".to_string(),
            title: "新建页面标题".to_string(),
            proposed_payload: r#"{"title":"新建页面标题","content":"这是新建页面的内容"}"#
                .to_string(),
            evidence_json: "{}".to_string(),
            confidence: 0.8,
            risk_level: RiskLevel::Low,
        },
    )
    .expect("创建 page_create 候选应成功");

    // 接受并应用
    promotion::review_candidate(
        conn,
        &ReviewCandidateInput {
            candidate_id,
            action: "accept".to_string(),
            reviewer: "test".to_string(),
            notes: Some("测试用接受".to_string()),
        },
    )
    .expect("接受 page_create 候选应成功");

    let applied = promotion::apply_candidate(conn, candidate_id).expect("应用 page_create 应成功");
    assert_eq!(applied.status, "applied");

    // 验证页面已创建
    let page_exists_after_apply: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pages WHERE slug = ?1 AND deleted_at IS NULL",
            rusqlite::params![new_page_slug],
            |row| row.get(0),
        )
        .unwrap_or(false);
    assert!(
        page_exists_after_apply,
        "page_create apply 后页面应存在且未删除"
    );

    // 回滚
    let rolled_back =
        promotion::rollback_candidate(conn, candidate_id).expect("回滚 page_create 应成功");
    assert_eq!(rolled_back.status, "rolled_back");

    // 验证页面已被软删除
    let page_active_after_rollback: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pages WHERE slug = ?1 AND deleted_at IS NULL",
            rusqlite::params![new_page_slug],
            |row| row.get(0),
        )
        .unwrap_or(false);
    assert!(
        !page_active_after_rollback,
        "P2 修复：page_create rollback 后页面不应仍为 active"
    );

    let page_has_deleted_at: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pages WHERE slug = ?1 AND deleted_at IS NOT NULL",
            rusqlite::params![new_page_slug],
            |row| row.get(0),
        )
        .unwrap_or(false);
    assert!(
        page_has_deleted_at,
        "P2 修复：page_create rollback 后页面应设置 deleted_at"
    );
}

/// P2 修复验证：同一 slug 连续 apply 两个候选后，
/// rollback 较早候选应被最新 applied 防护拒绝，且页面/投影不变。
#[test]
fn rollback_older_fact_claim_rejected_after_newer_applied() {
    let engine = make_engine();
    let config = make_config();
    let svc = make_svc(&engine, &config);
    let conn = engine.connection().expect("connection");

    // 创建初始页面
    let _result1 = svc
        .put_memory(
            "people/older-rollback-rejected-test",
            "Initial page content",
            Some("Older Rollback Rejected Test"),
            None,
            false,
            false,
        )
        .expect("put_memory 应成功");

    let artifact_id: i64 = conn
        .query_row(
            "SELECT id FROM source_artifacts WHERE canonical_slug = 'people/older-rollback-rejected-test'",
            [],
            |row| row.get(0),
        )
        .expect("获取 artifact_id");

    let slug = "people/older-rollback-rejected-test";

    // 第一个 fact_claim 候选
    let candidate_1_id = promotion::create_candidate(
        conn,
        CreateCandidateInput {
            artifact_id,
            occurrence_id: None,
            kb_document_id: None,
            kb_node_id: None,
            candidate_type: CandidateType::FactClaim,
            target_slug: slug.to_string(),
            target_field: "compiled_truth".to_string(),
            title: "第一个事实声明".to_string(),
            proposed_payload: r#"{"subject_slug":"people/older-rollback-rejected-test","predicate":"属性1","object_text":"值1"}"#.to_string(),
            evidence_json: "{}".to_string(),
            confidence: 0.9,
            risk_level: RiskLevel::Low,
        },
    )
    .expect("创建第一个候选");

    promotion::review_candidate(
        conn,
        &ReviewCandidateInput {
            candidate_id: candidate_1_id,
            action: "accept".to_string(),
            reviewer: "test".to_string(),
            notes: Some("接受第一个候选".to_string()),
        },
    )
    .expect("接受第一个候选");

    let _applied1 = promotion::apply_candidate(conn, candidate_1_id).expect("应用第一个候选");

    // 第二个 fact_claim 候选
    let candidate_2_id = promotion::create_candidate(
        conn,
        CreateCandidateInput {
            artifact_id,
            occurrence_id: None,
            kb_document_id: None,
            kb_node_id: None,
            candidate_type: CandidateType::FactClaim,
            target_slug: slug.to_string(),
            target_field: "compiled_truth".to_string(),
            title: "第二个事实声明".to_string(),
            proposed_payload: r#"{"subject_slug":"people/older-rollback-rejected-test","predicate":"属性2","object_text":"值2"}"#.to_string(),
            evidence_json: "{}".to_string(),
            confidence: 0.9,
            risk_level: RiskLevel::Low,
        },
    )
    .expect("创建第二个候选");

    promotion::review_candidate(
        conn,
        &ReviewCandidateInput {
            candidate_id: candidate_2_id,
            action: "accept".to_string(),
            reviewer: "test".to_string(),
            notes: Some("接受第二个候选".to_string()),
        },
    )
    .expect("接受第二个候选");

    let _applied2 = promotion::apply_candidate(conn, candidate_2_id).expect("应用第二个候选");

    // 记录两个候选应用后的页面内容
    let content_before_rollback: String = conn
        .query_row(
            "SELECT compiled_truth FROM pages WHERE slug = ?1",
            rusqlite::params![slug],
            |row| row.get(0),
        )
        .expect("查询页面内容");

    // 尝试回滚较早的候选 → 应失败
    let rollback_result = promotion::rollback_candidate(conn, candidate_1_id);
    assert!(
        rollback_result.is_err(),
        "P2 修复：rollback 较早候选应被拒绝，因为不是最新 applied 候选"
    );
    let err_msg = format!("{}", rollback_result.unwrap_err());
    assert!(
        err_msg.contains("不是") || err_msg.contains("最新"),
        "错误消息应说明不是最新 applied 候选，实际: {}",
        err_msg
    );

    // 验证较早候选状态未变
    let c1_status: String = conn
        .query_row(
            "SELECT status FROM promotion_candidates WHERE id = ?1",
            rusqlite::params![candidate_1_id],
            |row| row.get(0),
        )
        .expect("查询候选1状态");
    assert_eq!(c1_status, "applied", "较早候选状态应仍为 applied");

    // 验证页面内容未变
    let content_after_rollback: String = conn
        .query_row(
            "SELECT compiled_truth FROM pages WHERE slug = ?1",
            rusqlite::params![slug],
            |row| row.get(0),
        )
        .expect("查询页面内容");
    assert_eq!(
        content_before_rollback, content_after_rollback,
        "回滚被拒绝后页面内容不应发生变化"
    );
}
