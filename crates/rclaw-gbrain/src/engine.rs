//! BrainEngine trait — the contract for all brain operations
//! Mirrors gbrain's src/core/engine.ts BrainEngine interface
//!
//! Note: This trait is NOT dyn-compatible because some methods return
//! complex types. Use SqliteEngine directly instead of dyn BrainEngine.

use crate::error::Result;
use crate::types::*;
use std::collections::HashMap;
use std::path::Path;

/// The brain engine trait — all operations a brain must support.
///
/// This is the single source of truth for the brain API surface.
/// CLI and MCP server both dispatch through this trait.
///
/// IMPORTANT: This trait is NOT dyn-compatible. Use concrete types
/// (SqliteEngine) instead of &dyn BrainEngine.
pub trait BrainEngine {
    /// Engine discriminator
    fn kind(&self) -> &'static str;

    // ── Lifecycle ──────────────────────────────────────────────

    /// Open the database, load extensions, acquire file lock
    fn connect(&mut self) -> Result<()>;

    /// Close the database, release file lock
    fn disconnect(&mut self) -> Result<()>;

    /// Create all tables, indexes, FTS5 virtual tables, triggers. Idempotent.
    fn init_schema(&self) -> Result<()>;

    // Note: transaction is NOT part of the trait because it uses impl FnOnce
    // which is not object-safe. Use SqliteEngine::transaction() directly.

    // ── Pages CRUD ─────────────────────────────────────────────

    fn get_page(&self, slug: &str) -> Result<Option<Page>>;
    fn put_page(&self, slug: &str, input: PageInput) -> Result<Page>;
    fn delete_page(&self, slug: &str) -> Result<()>;
    fn restore_page(&self, slug: &str) -> Result<bool>;
    fn purge_deleted_pages(&self, older_than_hours: i64) -> Result<Vec<String>>;
    fn list_pages(&self, filters: PageFilters) -> Result<Vec<Page>>;
    fn resolve_slugs(&self, partial: &str) -> Result<Vec<String>>;
    fn get_all_slugs(&self) -> Result<Vec<String>>;

    // ── Search ─────────────────────────────────────────────────

    fn search_keyword(&self, query: &str, opts: SearchOpts) -> Result<Vec<SearchResult>>;
    fn search_vector(&self, embedding: &[f32], opts: SearchOpts) -> Result<Vec<SearchResult>>;
    fn get_embeddings_by_chunk_ids(&self, chunk_ids: &[i64]) -> Result<Vec<(i64, Vec<f32>)>>;
    fn search_keyword_chunks(&self, query: &str, opts: SearchOpts) -> Result<Vec<CodeChunkResult>>;

    // ── Chunks ─────────────────────────────────────────────────

    fn upsert_chunks(&self, slug: &str, chunks: &[ChunkInput]) -> Result<usize>;
    fn get_chunks(&self, slug: &str) -> Result<Vec<Chunk>>;
    fn get_chunk_by_id(&self, chunk_id: i64) -> Result<Option<Chunk>>;
    fn count_stale_chunks(&self) -> Result<usize>;
    fn list_stale_chunks(&self, limit: Option<usize>) -> Result<Vec<StaleChunk>>;
    fn delete_chunks(&self, slug: &str) -> Result<()>;
    /// H19: 增量删除过时 chunks，仅移除不在 retained 集合中的条目。
    /// 保留未变化 chunks 的 embeddings，避免全删重建导致 embedding 缓存丢失。
    fn delete_stale_chunks(&self, slug: &str, retained: &[(i32, ChunkSource)]) -> Result<()>;

    // Code graph
    fn add_code_edges(&self, edges: &[CodeEdgeInput]) -> Result<usize>;
    fn delete_code_edges_for_chunks(&self, chunk_ids: &[i64]) -> Result<usize>;
    fn get_callers_of(&self, slug: &str, symbol: &str) -> Result<Vec<CodeEdge>>;
    fn get_callees_of(&self, slug: &str, symbol: &str) -> Result<Vec<CodeEdge>>;
    fn get_edges_by_chunk(&self, chunk_id: i64) -> Result<Vec<CodeEdge>>;
    fn get_chunks_by_symbol(&self, symbol_name: &str, limit: usize) -> Result<Vec<Chunk>>;
    /// Get unresolved edges originating from a chunk (from code_edges_symbol table).
    /// Returns (to_symbol_qualified, edge_type) pairs for edges where the target
    /// chunk has not yet been imported.
    fn get_unresolved_edges_from(&self, chunk_id: i64) -> Result<Vec<(String, String)>>;

    // ── Links ──────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn add_link(
        &self,
        from_slug: &str,
        to_slug: &str,
        context: Option<&str>,
        link_type: Option<&str>,
        source: Option<&str>,
        confidence: Option<f64>,
        metadata: Option<serde_json::Value>,
    ) -> Result<()>;

    fn add_links_batch(&self, inputs: &[LinkBatchInput]) -> Result<usize>;
    fn remove_link(
        &self,
        from_slug: &str,
        to_slug: &str,
        link_type: Option<&str>,
        context: Option<&str>,
        link_source: Option<&str>,
    ) -> Result<()>;
    fn get_links(&self, slug: &str) -> Result<Vec<Link>>;
    fn get_backlinks(&self, slug: &str) -> Result<Vec<Link>>;
    fn remove_links_by_origin(&self, from_slug: &str, origin_source: &str) -> Result<()>;

    fn find_by_title_fuzzy(
        &self,
        query: &str,
        dir_prefix: Option<&str>,
        min_similarity: Option<f64>,
        limit: Option<usize>,
    ) -> Result<Vec<FuzzyMatch>>;

    fn traverse_graph(&self, slug: &str, depth: usize) -> Result<Vec<GraphNode>>;
    fn traverse_paths(&self, from: &str, to: &str, opts: TraverseOpts) -> Result<Vec<GraphPath>>;
    /// P2-10: Fetch backlink counts only for specific slugs instead of all slugs.
    /// Pass an empty slice to get an empty map (avoids full table scan).
    fn get_backlink_counts(&self, slugs: &[String]) -> Result<HashMap<String, i64>>;

    // ── Tags ───────────────────────────────────────────────────

    fn add_tag(&self, slug: &str, tag: &str) -> Result<()>;
    fn remove_tag(&self, slug: &str, tag: &str) -> Result<()>;
    fn get_tags(&self, slug: &str) -> Result<Vec<String>>;

    // ── Timeline ───────────────────────────────────────────────

    fn add_timeline_entry(
        &self,
        slug: &str,
        entry: TimelineInput,
        skip_existence_check: bool,
    ) -> Result<()>;

    fn add_timeline_entries_batch(&self, slug: &str, entries: &[TimelineInput]) -> Result<usize>;

    /// P2-8: Multi-slug timeline batch insert (each entry has its own slug)
    fn add_timeline_multi_batch(&self, batches: &[TimelineBatchInput]) -> Result<usize>;
    fn get_timeline(
        &self,
        slug: &str,
        opts: Option<TimelineQueryOpts>,
    ) -> Result<Vec<TimelineEntry>>;

    // ── Raw Data ───────────────────────────────────────────────

    fn put_raw_data(&self, slug: &str, key: &str, data: serde_json::Value) -> Result<()>;
    fn get_raw_data(&self, slug: &str, key: &str) -> Result<Option<serde_json::Value>>;

    // ── Versions ───────────────────────────────────────────────

    fn create_version(&self, slug: &str) -> Result<i64>;
    fn get_versions(&self, slug: &str, limit: Option<usize>) -> Result<Vec<PageVersion>>;
    fn revert_to_version(&self, slug: &str, version_id: i64) -> Result<()>;

    // ── Stats + Health ─────────────────────────────────────────

    fn get_stats(&self) -> Result<BrainStats>;
    fn get_health(&self) -> Result<BrainHealth>;

    // ── P2-5: Integrity + Orphan Detection ─────────────────────

    /// Detect orphan pages (pages with no incoming or outgoing links)
    fn detect_orphans(&self) -> Result<Vec<String>>;
    /// Detect dead links (links pointing to non-existent pages)
    fn detect_dead_links(&self) -> Result<Vec<(String, String)>>;

    // ── Ingest Log ─────────────────────────────────────────────

    fn log_ingest(&self, entry: IngestLogInput) -> Result<()>;
    fn get_ingest_log(&self, limit: Option<usize>) -> Result<Vec<IngestLogEntry>>;

    // ── Sync ───────────────────────────────────────────────────

    fn update_slug(&self, old_slug: &str, new_slug: &str) -> Result<()>;
    fn rewrite_links(&self, old_slug: &str, new_slug: &str) -> Result<()>;

    // ── Config ─────────────────────────────────────────────────

    fn get_config(&self, key: &str) -> Result<Option<String>>;
    fn set_config(&self, key: &str, value: &str) -> Result<()>;

    // ── Migration Support ──────────────────────────────────────

    fn run_migration(&self, version: i32, sql: &str) -> Result<()>;
    fn get_chunks_with_embeddings(&self) -> Result<Vec<(i64, String, Vec<f32>)>>;

    // ── File Storage ────────────────────────────────────────────

    fn file_upload(
        &self,
        source_path: &Path,
        slug: &str,
        opts: FileUploadOptions,
    ) -> Result<FileRecord>;
    fn file_list(&self, slug: Option<&str>, limit: Option<usize>) -> Result<Vec<FileRecord>>;
    fn file_url(&self, file_id: i64, mode: FileUrlMode) -> Result<String>;
    fn file_url_by_storage_path(&self, storage_path: &str) -> Result<String>;
    fn file_verify(&self) -> Result<FileVerifyResult>;
}
