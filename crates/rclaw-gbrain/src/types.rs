//! Core types for gbrain
//! Mirrors gbrain's src/core/types.ts

use serde::{Deserialize, Serialize};

/// Page types — mirrors the TypeScript PageType enum
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PageType {
    Person,
    Company,
    Deal,
    Yc,
    Civic,
    Project,
    Concept,
    Source,
    Media,
    Writing,
    Analysis,
    Guide,
    Hardware,
    Architecture,
    Meeting,
    Email,
    Slack,
    CalendarEvent,
    Code,
    Note,
}

impl PageType {
    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "person" | "people" => Self::Person,
            "company" | "companies" => Self::Company,
            "deal" | "deals" => Self::Deal,
            "yc" => Self::Yc,
            "civic" => Self::Civic,
            "project" | "projects" => Self::Project,
            "concept" | "concepts" => Self::Concept,
            "source" => Self::Source,
            "media" => Self::Media,
            "writing" => Self::Writing,
            "analysis" => Self::Analysis,
            "guide" | "guides" => Self::Guide,
            "hardware" => Self::Hardware,
            "architecture" => Self::Architecture,
            "meeting" | "meetings" => Self::Meeting,
            "email" | "emails" => Self::Email,
            "slack" => Self::Slack,
            "calendar-event" | "calendar_event" | "calendar" => Self::CalendarEvent,
            "code" => Self::Code,
            _ => Self::Note,
        }
    }
}

impl std::fmt::Display for PageType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Person => write!(f, "person"),
            Self::Company => write!(f, "company"),
            Self::Deal => write!(f, "deal"),
            Self::Yc => write!(f, "yc"),
            Self::Civic => write!(f, "civic"),
            Self::Project => write!(f, "project"),
            Self::Concept => write!(f, "concept"),
            Self::Source => write!(f, "source"),
            Self::Media => write!(f, "media"),
            Self::Writing => write!(f, "writing"),
            Self::Analysis => write!(f, "analysis"),
            Self::Guide => write!(f, "guide"),
            Self::Hardware => write!(f, "hardware"),
            Self::Architecture => write!(f, "architecture"),
            Self::Meeting => write!(f, "meeting"),
            Self::Email => write!(f, "email"),
            Self::Slack => write!(f, "slack"),
            Self::CalendarEvent => write!(f, "calendar-event"),
            Self::Code => write!(f, "code"),
            Self::Note => write!(f, "note"),
        }
    }
}

/// A brain page
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub id: i64,
    pub slug: String,
    pub page_type: PageType,
    pub title: String,
    pub compiled_truth: String,
    pub timeline: Option<String>,
    pub frontmatter: Option<String>,
    pub content_hash: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

/// Input for creating/updating a page
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageInput {
    pub page_type: PageType,
    pub title: String,
    pub compiled_truth: String,
    pub timeline: Option<serde_json::Value>,
    pub frontmatter: Option<serde_json::Value>,
    pub content_hash: Option<String>,
}

/// Filters for listing pages
#[derive(Debug, Clone, Default)]
pub struct PageFilters {
    pub page_type: Option<PageType>,
    pub tag: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// Filter to pages updated after this ISO date string (mirrors TS updated_after)
    pub updated_after: Option<String>,
    pub include_deleted: bool,
    pub slug_prefix: Option<String>,
}

/// Chunk source type
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkSource {
    CompiledTruth,
    Timeline,
    FencedCode,
}

impl std::fmt::Display for ChunkSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CompiledTruth => write!(f, "compiled_truth"),
            Self::Timeline => write!(f, "timeline"),
            Self::FencedCode => write!(f, "fenced_code"),
        }
    }
}

/// A text chunk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: i64,
    pub page_id: i64,
    pub slug: String,
    pub chunk_index: i32,
    pub chunk_text: String,
    pub source: ChunkSource,
    pub token_count: i32,
    pub model: Option<String>,
    pub embedded_at: Option<String>,
    pub language: Option<String>,
    pub symbol_name: Option<String>,
    pub symbol_type: Option<String>,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
    /// Comma-separated parent scope path (e.g. "BrainEngine,searchKeyword")
    pub parent_symbol_path: Option<String>,
    /// Language-aware qualified name (e.g. "BrainEngine.searchKeyword")
    pub symbol_name_qualified: Option<String>,
    /// Extracted doc comment above symbol
    pub doc_comment: Option<String>,
    pub created_at: String,
}

/// Input for creating a chunk
#[derive(Debug, Clone)]
pub struct ChunkInput {
    pub chunk_index: i32,
    pub chunk_text: String,
    pub source: ChunkSource,
    pub token_count: i32,
    pub embedding: Option<Vec<f32>>,
    pub model: Option<String>,
    pub language: Option<String>,
    pub symbol_name: Option<String>,
    pub symbol_type: Option<String>,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
    /// Comma-separated parent scope path (e.g. "BrainEngine,searchKeyword")
    pub parent_symbol_path: Option<String>,
    /// Language-aware qualified name (e.g. "BrainEngine.searchKeyword")
    pub symbol_name_qualified: Option<String>,
    /// Extracted doc comment above symbol
    pub doc_comment: Option<String>,
}

impl ChunkInput {
    pub fn text(
        chunk_index: i32,
        chunk_text: String,
        source: ChunkSource,
        token_count: i32,
    ) -> Self {
        Self {
            chunk_index,
            chunk_text,
            source,
            token_count,
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
        }
    }
}

/// Detail level for search results
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DetailLevel {
    Low,
    #[default]
    Medium,
    High,
}

/// Search options
#[derive(Debug, Clone, Default)]
pub struct SearchOpts {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub page_type: Option<PageType>,
    pub detail_level: Option<DetailLevel>,
    /// Slugs to exclude from search results (mirrors TS exclude_slugs)
    pub exclude_slugs: Option<Vec<String>>,
    pub exclude_slug_prefixes: Option<Vec<String>>,
    pub include_slug_prefixes: Option<Vec<String>>,
    /// 精确 slug 白名单：搜索结果只返回匹配这些 slug 的页面。
    /// 用于 filter_slug 下推，避免扩大全局候选后后置过滤导致假阴性。
    pub include_slugs: Option<Vec<String>>,
    /// Pre-computed expanded queries for multi-list RRF fusion.
    /// Caller should run expand_query() asynchronously and pass results here.
    pub expanded_queries: Option<Vec<String>>,
    /// Pre-computed embeddings for each expanded query (1:1 with expanded_queries).
    /// Index 0 = expanded_queries[0], etc. If None, falls back to reusing the
    /// original query embedding for all (legacy behavior).
    pub expanded_embeddings: Option<Vec<Vec<f32>>>,
    /// P2-6: Dedup options for customizing dedup behavior (mirrors TS dedupOpts)
    pub dedup_opts: Option<crate::search::dedup::DedupOpts>,
    /// Language filter for code chunk search (mirrors TS opts.language)
    pub language: Option<String>,
    /// Symbol kind filter for code chunk search (mirrors TS opts.symbolKind)
    pub symbol_kind: Option<String>,
    /// Anchor at a qualified symbol name for two-pass expansion (mirrors TS opts.nearSymbol)
    pub near_symbol: Option<String>,
    /// Walk depth for two-pass code graph expansion (0=off, 1-2=expand N hops)
    pub walk_depth: Option<usize>,
}

/// Search result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub slug: String,
    pub title: String,
    pub chunk_text: String,
    pub score: f64,
    pub page_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub chunk_index: Option<i32>,
    pub source: Option<ChunkSource>,
    pub detail_level: DetailLevel,
    pub page_type: Option<PageType>,
    /// Whether the embedding for this chunk is stale (content updated since embed)
    pub stale: bool,
    /// P1-1: Page updated_at timestamp for recency boost calculation
    pub updated_at: Option<String>,
}

/// Metadata about what hybrid search actually did (mirrors TS HybridSearchMeta)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMeta {
    /// Whether vector search was actually enabled (has API key + embedding succeeded)
    pub vector_enabled: bool,
    /// The detail level that was actually resolved (may differ from requested)
    pub detail_resolved: Option<DetailLevel>,
    /// Whether query expansion was applied
    pub expansion_applied: bool,
}

/// Search results with metadata about what the search pipeline actually did
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResultWithMeta {
    pub results: Vec<SearchResult>,
    pub meta: SearchMeta,
}

/// Code symbol extracted from a code page or fenced code block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeSymbol {
    pub name: String,
    pub qualified_name: String,
    pub symbol_type: String,
    pub language: String,
    pub start_line: i32,
    pub end_line: i32,
    pub parent_symbol: Option<String>,
}

/// Input for a code edge between symbols.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeEdgeInput {
    pub from_slug: String,
    pub from_symbol: String,
    pub to_slug: String,
    pub to_symbol: String,
    pub edge_type: String,
    pub confidence: f64,
    pub context: Option<String>,
    pub from_chunk_id: Option<i64>,
    pub to_chunk_id: Option<i64>,
    /// Qualified name of the source symbol (e.g. "BrainEngine.searchKeyword")
    pub from_symbol_qualified: Option<String>,
    /// Qualified name of the target symbol (e.g. "SqliteEngine.searchKeyword")
    pub to_symbol_qualified: Option<String>,
}

/// A stored code edge between symbols/chunks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeEdge {
    pub id: i64,
    pub from_slug: String,
    pub from_symbol: String,
    pub to_slug: String,
    pub to_symbol: String,
    pub edge_type: String,
    pub confidence: f64,
    pub context: Option<String>,
    pub from_chunk_id: Option<i64>,
    pub to_chunk_id: Option<i64>,
    pub from_symbol_qualified: Option<String>,
    pub to_symbol_qualified: Option<String>,
    pub created_at: String,
}

/// Code-specific keyword search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeChunkResult {
    pub slug: String,
    pub title: String,
    pub chunk_id: i64,
    pub chunk_index: i32,
    pub chunk_text: String,
    pub score: f64,
    pub language: Option<String>,
    pub symbol_name: Option<String>,
    pub symbol_type: Option<String>,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
}

/// A link between pages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub id: i64,
    pub from_slug: String,
    pub to_slug: String,
    pub link_type: String,
    pub context: Option<String>,
    pub link_source: Option<LinkSource>,
    pub origin_slug: Option<String>,
    pub origin_field: Option<String>,
    pub direction: Option<LinkDirection>,
    pub created_at: String,
}

/// Link direction — distinguishes outgoing vs incoming semantic direction
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum LinkDirection {
    Outgoing,
    Incoming,
}

impl LinkDirection {
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "incoming" => Self::Incoming,
            _ => Self::Outgoing,
        }
    }
}

/// Link source — where the link was extracted from
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum LinkSource {
    Markdown,
    Frontmatter,
    Manual,
}

impl LinkSource {
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "frontmatter" => Self::Frontmatter,
            "manual" => Self::Manual,
            _ => Self::Markdown,
        }
    }
}

/// Batch link input
#[derive(Debug, Clone)]
pub struct LinkBatchInput {
    pub from_slug: String,
    pub to_slug: String,
    pub link_type: Option<String>,
    pub context: Option<String>,
    pub link_source: Option<LinkSource>,
    pub origin_slug: Option<String>,
    pub origin_field: Option<String>,
    pub direction: Option<LinkDirection>,
}

/// A node in the knowledge graph
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub slug: String,
    pub page_type: String,
    pub title: String,
    pub depth: usize,
    pub links: Vec<NodeLink>,
}

/// A link within a graph node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeLink {
    pub to_slug: String,
    pub link_type: String,
}

/// A path edge through the knowledge graph (mirrors TS edge-based GraphPath)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphPath {
    pub from_slug: String,
    pub to_slug: String,
    pub link_type: String,
    pub context: String,
    /// Distance of to_slug from the root node
    pub depth: usize,
}

/// Traversal options
#[derive(Debug, Clone, Default)]
pub struct TraverseOpts {
    pub depth: usize,
    pub direction: Direction,
    pub link_type: Option<String>,
}

/// Direction for graph traversal
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Both,
    In,
    Out,
}

/// Timeline entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub id: i64,
    pub slug: String,
    pub date: String,
    pub source: Option<String>,
    pub summary: String,
    pub detail: Option<String>,
    pub created_at: String,
}

/// Timeline input
#[derive(Debug, Clone)]
pub struct TimelineInput {
    pub date: String,
    pub source: Option<String>,
    pub summary: String,
    pub detail: Option<String>,
}

/// Batch timeline input
#[derive(Debug, Clone)]
pub struct TimelineBatchInput {
    pub slug: String,
    pub entries: Vec<TimelineInput>,
}

/// Timeline query options
#[derive(Debug, Clone, Default)]
pub struct TimelineQueryOpts {
    pub limit: Option<usize>,
    /// Filter entries after this ISO date (e.g. "2024-01-01")
    pub after: Option<String>,
    /// Filter entries before this ISO date (e.g. "2024-12-31")
    pub before: Option<String>,
}

/// Raw data record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawData {
    pub id: i64,
    pub slug: String,
    pub key: String,
    pub data: serde_json::Value,
    pub created_at: String,
}

/// Page version
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageVersion {
    pub id: i64,
    pub slug: String,
    pub page_type: String,
    pub title: String,
    pub compiled_truth: String,
    pub frontmatter: Option<String>,
    pub created_at: String,
}

/// Brain statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainStats {
    pub page_count: i64,
    pub chunk_count: i64,
    pub embedded_count: i64,
    pub link_count: i64,
    pub tag_count: i64,
    pub timeline_entry_count: i64,
    /// Count of pages grouped by page_type (mirrors TS pages_by_type)
    pub pages_by_type: std::collections::HashMap<String, i64>,
}

/// Brain health assessment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainHealth {
    pub brain_score: f64,
    pub page_count: i64,
    pub embed_coverage: f64,
    pub stale_pages: i64,
    pub orphan_pages: i64,
    pub dead_links: i64,
    pub link_coverage: f64,
    pub timeline_coverage: f64,
    pub embed_coverage_score: f64,
    pub link_density_score: f64,
    pub timeline_coverage_score: f64,
    pub no_orphans_score: f64,
    pub no_dead_links_score: f64,
    /// Chunks without embeddings (mirrors TS missing_embeddings)
    pub missing_embeddings: i64,
    /// Top 5 most connected pages by link count (mirrors TS most_connected)
    pub most_connected: Vec<MostConnectedPage>,
}

/// Top-connected page for health dashboard (mirrors TS most_connected)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MostConnectedPage {
    pub slug: String,
    pub link_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleChunk {
    pub slug: String,
    pub chunk_id: i64,
    pub chunk_index: i32,
    pub chunk_text: String,
    pub source: ChunkSource,
    pub token_count: i32,
    pub model: Option<String>,
}

/// Ingest log entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestLogEntry {
    pub id: i64,
    /// Legacy combined source field (kept for backward compat)
    pub source: String,
    /// Type of ingestion source (mirrors TS source_type: "git" | "import" | "api" etc.)
    pub source_type: String,
    /// Reference for the ingestion source (mirrors TS source_ref)
    pub source_ref: String,
    /// Human-readable summary of the ingestion (mirrors TS summary)
    pub summary: String,
    pub pages_updated: Vec<String>,
    pub status: String,
    pub error: Option<String>,
    pub created_at: String,
}

/// Ingest log input
#[derive(Debug, Clone)]
pub struct IngestLogInput {
    /// Type of ingestion source (mirrors TS source_type: "git" | "import" | "api" etc.)
    pub source_type: String,
    /// Reference for the ingestion source (mirrors TS source_ref)
    pub source_ref: String,
    /// Human-readable summary of the ingestion
    pub summary: String,
    pub pages_updated: Vec<String>,
    pub status: String,
    pub error: Option<String>,
}

/// Engine configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    pub database_path: String,
    pub wal_mode: bool,
    pub pool_size: usize,
}

/// File record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub id: i64,
    pub slug: String,
    pub filename: String,
    pub storage_path: String,
    pub mime_type: Option<String>,
    pub size_bytes: i64,
    pub hash: Option<String>,
    pub created_at: String,
}

/// File upload options
#[derive(Debug, Clone)]
pub struct FileUploadOptions {
    pub slug: String,
    pub overwrite: bool,
    pub max_size_bytes: Option<usize>,
}

/// File URL mode
#[derive(Debug, Clone)]
pub enum FileUrlMode {
    LocalPath,
    Http { port: u16 },
}

/// File verify result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileVerifyResult {
    pub verified: usize,
    pub mismatches: usize,
    pub missing: usize,
}

/// File sync result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSyncResult {
    pub uploaded: usize,
    pub skipped: usize,
}

/// Fuzzy match result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuzzyMatch {
    pub slug: String,
    pub title: String,
    pub score: f64,
}

/// Maximum search limit
pub const MAX_SEARCH_LIMIT: usize = 200;

/// Clamp search limit to a maximum
pub fn clamp_search_limit(limit: Option<usize>, default: usize, cap: usize) -> usize {
    let l = limit.unwrap_or(default);
    l.min(cap).min(MAX_SEARCH_LIMIT)
}

/// P2-9: PutPageResult — enriched response from put_page (mirrors TS)
/// TS returns { slug, status, chunks, auto_links, auto_timeline, writer_lint }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutPageResult {
    pub slug: String,
    pub status: String,
    pub chunk_count: usize,
    pub auto_links_added: usize,
    pub auto_links_removed: usize,
    pub auto_timeline_added: usize,
}

/// P2-10: ListPageEntry — lightweight page listing (mirrors TS)
/// TS only returns { slug, type, title, updated_at } for list_pages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListPageEntry {
    pub slug: String,
    pub page_type: Option<PageType>,
    pub title: String,
    pub updated_at: String,
}
