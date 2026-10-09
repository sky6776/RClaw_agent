//! Contract-first operation definitions
//! Mirrors gbrain's src/core/operations.ts
//!
//! Operations are the single source of truth for the brain API surface.
//! CLI and MCP server both dispatch through these operations.

use crate::chunker::tree_sitter::chunk_code_tree_sitter;
use crate::chunker::{chunk_page_content, chunk_page_content_transactional};
use crate::code_index::{index_code, CodeIndex};
use crate::config::Config;
use crate::embedding::Embedder;
use crate::engine::BrainEngine;
use crate::error::Result;
use crate::link_extraction::{
    extract_entity_refs, extract_frontmatter_refs, parse_timeline_entries, refs_to_batch_input,
    EngineSlugResolver,
};
use crate::markdown::{infer_type, parse_markdown};
use crate::search::expansion::expand_query;
use crate::search::hybrid::{hybrid_search, HybridOpts};
use crate::search::intent::{classify_intent, detail_for_intent};
use crate::security::validate_contained;
use crate::security::{validate_filename, validate_page_slug, validate_upload_path};
use crate::sqlite_engine::SqliteEngine;
use crate::types::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info, trace, warn};

/// Extract mode for batch extraction (mirrors TS gbrain extract)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractMode {
    Links,
    Timeline,
    All,
}

impl ExtractMode {
    pub fn extract_links(&self) -> bool {
        matches!(self, Self::Links | Self::All)
    }
    pub fn extract_timeline(&self) -> bool {
        matches!(self, Self::Timeline | Self::All)
    }
}

/// Result of batch extraction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractResult {
    pub links_added: usize,
    pub timeline_added: usize,
    pub errors: usize,
    pub pages_scanned: usize,
}

/// Parameter type for operation definitions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    String,
    Integer,
    Boolean,
    Number,
    Array,
    Object,
}

impl ParamType {
    pub fn json_type_name(&self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Number => "number",
            Self::Array => "array",
            Self::Object => "object",
        }
    }
}

/// Parameter definition for an operation
#[derive(Debug, Clone)]
pub struct ParamDef {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
    pub param_type: ParamType,
    /// Enum values for string parameters (mirrors TS ParamDef.enum)
    pub enum_values: Option<&'static [&'static str]>,
    /// Items type for array parameters (mirrors TS ParamDef.items.type)
    pub items_type: Option<ParamType>,
}

impl ParamDef {
    /// Create a parameter definition with defaults for optional fields
    pub const fn new(
        name: &'static str,
        description: &'static str,
        required: bool,
        param_type: ParamType,
    ) -> Self {
        Self {
            name,
            description,
            required,
            param_type,
            enum_values: None,
            items_type: None,
        }
    }

    /// Set enum values (builder pattern)
    pub const fn with_enum(mut self, values: &'static [&'static str]) -> Self {
        self.enum_values = Some(values);
        self
    }

    /// Set items type for array params (builder pattern)
    pub const fn with_items(mut self, items_type: ParamType) -> Self {
        self.items_type = Some(items_type);
        self
    }
}

/// Structured operation definition — mirrors TS Operation type.
/// Single source of truth for MCP tool schema + param validation + docs.
#[derive(Debug, Clone)]
pub struct OperationDef {
    pub name: &'static str,
    pub description: &'static str,
    pub params: &'static [ParamDef],
}

impl OperationDef {
    /// Generate MCP-compatible JSON Schema from the operation definition
    pub fn to_mcp_schema(&self) -> serde_json::Value {
        let properties: serde_json::Map<String, serde_json::Value> = self
            .params
            .iter()
            .map(|p| {
                let mut prop = serde_json::json!({
                    "type": p.param_type.json_type_name(),
                    "description": p.description,
                });
                if let Some(enums) = p.enum_values {
                    prop["enum"] = serde_json::json!(enums);
                }
                if let Some(items_type) = p.items_type {
                    prop["items"] = serde_json::json!({
                        "type": items_type.json_type_name()
                    });
                }
                (p.name.to_string(), prop)
            })
            .collect();

        let required: Vec<&str> = self
            .params
            .iter()
            .filter(|p| p.required)
            .map(|p| p.name)
            .collect();

        let mut schema = serde_json::json!({
            "type": "object",
            "properties": properties,
        });

        if !required.is_empty() {
            schema["required"] = serde_json::json!(required);
        }

        schema
    }
}

/// Operation context — trust boundary
#[derive(Debug, Clone)]
pub struct OpContext {
    pub remote: bool,
    pub working_dir: std::path::PathBuf,
    /// Dry-run mode: preview before saving (mirrors TS OperationContext.dryRun)
    pub dry_run: bool,
    /// P1-7: Sub-agent ID for namespace enforcement (mirrors TS viaSubagent)
    /// When set, put_page is restricted to wiki/agents/<subagent_id>/... namespace
    pub subagent_id: Option<String>,
}

impl Default for OpContext {
    fn default() -> Self {
        Self {
            remote: false,
            working_dir: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            dry_run: false,
            subagent_id: None,
        }
    }
}

/// Operation definitions — each maps to a brain operation
pub struct Operations<'a> {
    pub engine: &'a SqliteEngine,
    pub ctx: OpContext,
    config: Config,
    /// When true, the caller has already opened a transaction; put_page must
    /// not wrap link reconciliation in `transaction_with_engine` (SQLite does
    /// not support nested `BEGIN IMMEDIATE`).
    in_transaction: bool,
    /// 统一查询缓存（§31 query_cache）
    query_cache: crate::artifact::query_cache::QueryCache,
}

impl<'a> Operations<'a> {
    /// P1-11: Maximum traversal depth (mirrors TS cap of 10)
    const TRAVERSE_DEPTH_CAP: usize = 10;
    /// P1-11: Maximum number of files returned by file_list (mirrors TS cap of 100)
    const FILE_LIST_LIMIT: usize = 100;
    /// P2-1: Maximum content size in bytes (mirrors TS 5MB limit)
    const MAX_CONTENT_SIZE: usize = 5_000_000;

    pub fn new(engine: &'a SqliteEngine, ctx: OpContext) -> Self {
        let config = Config::load().unwrap_or_else(|e| {
            panic!("gbrain 配置加载失败，必须通过环境变量显式配置必选项: {}", e);
        });
        Self::with_config(engine, ctx, config)
    }

    pub fn with_config(engine: &'a SqliteEngine, ctx: OpContext, config: Config) -> Self {
        debug!(
            remote = ctx.remote,
            auto_link = config.auto_link,
            auto_timeline = config.auto_timeline,
            "Creating Operations instance"
        );
        Self {
            engine,
            ctx,
            config,
            in_transaction: false,
            query_cache: crate::artifact::query_cache::QueryCache::new(256, 300),
        }
    }

    /// 获取 Artifact 应用服务（设计文档 §8.4）
    ///
    /// 返回 `ArtifactService` 实例，作为统一知识操作编排入口。
    /// CLI/MCP facade 层应优先通过此服务调用，而非直接访问 `Operations` 上的
    /// `upload_source`/`memory_query`/`promotion_*` 等方法。
    pub fn artifact_service(&self) -> crate::artifact::service::ArtifactService<'_> {
        crate::artifact::service::ArtifactService::new(self.engine, self.ctx.clone(), &self.config)
    }

    /// Create Operations instance that knows it's already inside a transaction.
    /// Used by `put_page_in_transaction` and `batch_put_pages` to avoid nested
    /// `BEGIN IMMEDIATE` (SQLite does not support nested transactions).
    pub(crate) fn with_config_in_transaction(
        engine: &'a SqliteEngine,
        ctx: OpContext,
        config: Config,
    ) -> Self {
        debug!(
            remote = ctx.remote,
            auto_link = config.auto_link,
            auto_timeline = config.auto_timeline,
            "Creating Operations instance (in-transaction)"
        );
        Self {
            engine,
            ctx,
            config,
            in_transaction: true,
            query_cache: crate::artifact::query_cache::QueryCache::new(256, 300),
        }
    }

    pub(crate) fn precompute_page_body_chunks(
        slug: &str,
        content: &str,
        config: &Config,
        page_type: Option<&PageType>,
    ) -> Vec<ChunkInput> {
        let pt = page_type.cloned().unwrap_or_else(|| infer_type(slug));
        chunk_page_content(content, config, &pt)
    }

    fn validate_put_page_request(&self, slug: &str, content: &str) -> Result<()> {
        validate_page_slug(slug)?;

        if let Some(ref subagent_id) = self.ctx.subagent_id {
            let required_prefix = format!("wiki/agents/{}/", subagent_id);
            if !slug.starts_with(&required_prefix) {
                return Err(crate::error::GBrainError::Security(format!(
                    "Sub-agent '{}' can only write to '{}' namespace, got '{}'",
                    subagent_id, required_prefix, slug
                )));
            }
        }

        if content.len() > Self::MAX_CONTENT_SIZE {
            return Err(crate::error::GBrainError::InvalidInput(format!(
                "content size {} exceeds maximum {} bytes",
                content.len(),
                Self::MAX_CONTENT_SIZE
            )));
        }

        Ok(())
    }

    fn dry_run_put_page(
        &self,
        slug: &str,
        title: &str,
        content: &str,
        page_type: Option<PageType>,
        content_hash: Option<&str>,
    ) -> Page {
        info!(slug = %slug, title = %title, "Dry-run: would put page");
        Page {
            id: 0,
            slug: slug.to_string(),
            page_type: page_type.unwrap_or(PageType::Note),
            title: title.to_string(),
            compiled_truth: content.to_string(),
            timeline: None,
            frontmatter: None,
            content_hash: content_hash.map(|s| s.to_string()),
            created_at: String::new(),
            updated_at: String::new(),
            deleted_at: None,
        }
    }

    /// Query the brain (high-level search with auto-escalate)
    pub fn query(&self, query: &str, opts: SearchOpts) -> Result<Vec<SearchResult>> {
        Ok(self.query_with_meta(query, opts, false)?.results)
    }

    /// Query the brain and return metadata about the retrieval path that actually ran.
    ///
    /// Mirrors the TS `HybridSearchMeta` side-channel: callers can tell whether
    /// vector search and expansion really ran instead of inferring from config.
    pub fn query_with_meta(
        &self,
        query: &str,
        mut opts: SearchOpts,
        expand: bool,
    ) -> Result<SearchResultWithMeta> {
        info!(query = %query, limit = opts.limit.unwrap_or(20), "Querying brain");

        // Determine detail level from intent if not specified
        let intent = classify_intent(query);
        let detail = opts
            .detail_level
            .or_else(|| detail_for_intent(&intent.intent))
            .unwrap_or(DetailLevel::Medium);
        opts.detail_level = Some(detail);

        let expanded_queries = if expand && opts.expanded_queries.is_none() {
            self.expand_query_sync(query)
        } else {
            None
        };
        if let Some(expanded) = expanded_queries {
            if expanded.len() > 1 {
                opts.expanded_queries = Some(expanded);
            }
        }

        let (query_embedding, expanded_embeddings) =
            self.embed_query_set(query, opts.expanded_queries.as_deref().unwrap_or(&[]));
        if opts.expanded_embeddings.is_none() {
            opts.expanded_embeddings = expanded_embeddings;
        }

        let hybrid_opts = HybridOpts::default();
        let mut result_with_meta = hybrid_search(
            self.engine,
            query,
            query_embedding.as_deref(),
            opts.clone(),
            hybrid_opts,
        )?;

        // Auto-escalate: if results are sparse, try with higher detail
        if result_with_meta.results.len() < 3 && detail != DetailLevel::High {
            info!("Auto-escalating to High detail");
            opts.detail_level = Some(DetailLevel::High);
            let escalated = hybrid_search(
                self.engine,
                query,
                query_embedding.as_deref(),
                opts,
                HybridOpts::default(),
            )?;
            if escalated.results.len() > result_with_meta.results.len() {
                result_with_meta = escalated;
            }
        }

        info!(
            result_count = result_with_meta.results.len(),
            "Query complete"
        );
        Ok(result_with_meta)
    }

    pub fn search_keyword_chunks(
        &self,
        query: &str,
        opts: SearchOpts,
    ) -> Result<Vec<CodeChunkResult>> {
        self.engine.search_keyword_chunks(query, opts)
    }

    pub fn get_callers_of(&self, slug: &str, symbol: &str) -> Result<Vec<CodeEdge>> {
        self.engine.get_callers_of(slug, symbol)
    }

    pub fn get_callees_of(&self, slug: &str, symbol: &str) -> Result<Vec<CodeEdge>> {
        self.engine.get_callees_of(slug, symbol)
    }

    pub fn get_edges_by_chunk(&self, chunk_id: i64) -> Result<Vec<CodeEdge>> {
        self.engine.get_edges_by_chunk(chunk_id)
    }

    pub fn find_code_definitions(
        &self,
        symbol: &str,
        language: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Chunk>> {
        let mut chunks = self
            .engine
            .get_chunks_by_symbol(symbol, limit.saturating_mul(4).max(20))?;
        chunks.retain(|chunk| {
            chunk.source == ChunkSource::FencedCode
                && language.is_none_or(|lang| chunk.language.as_deref() == Some(lang))
        });
        chunks.truncate(limit);
        Ok(chunks)
    }

    pub fn find_code_references(
        &self,
        symbol: &str,
        language: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodeChunkResult>> {
        self.search_keyword_chunks(
            symbol,
            SearchOpts {
                limit: Some(limit),
                page_type: Some(PageType::Code),
                language: language.map(str::to_string),
                ..Default::default()
            },
        )
    }

    pub fn reindex_code_page(&self, slug: &str) -> Result<usize> {
        let page = self
            .engine
            .get_page(slug)?
            .ok_or_else(|| crate::error::GBrainError::PageNotFound(slug.to_string()))?;
        let content = page.compiled_truth.clone();
        let language = page
            .frontmatter
            .as_deref()
            .and_then(|fm| serde_json::from_str::<serde_json::Value>(fm).ok())
            .and_then(|fm| {
                fm.get("language")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            });
        let code_index = chunk_code_tree_sitter(slug, &content, language.as_deref(), 0);
        self.engine.delete_chunks(slug)?;
        let count = self.engine.upsert_chunks(slug, &code_index.chunks)?;
        reconcile_code_edges(self.engine, slug, &code_index)?;
        Ok(count)
    }

    fn embed_query_set(
        &self,
        query: &str,
        expanded_queries: &[String],
    ) -> (Option<Vec<f32>>, Option<Vec<Vec<f32>>>) {
        let Some(api_key) = self.config.embedding_api_key.as_deref() else {
            return (None, None);
        };
        if api_key.is_empty() {
            return (None, None);
        }
        let embedder = Embedder::new(
            api_key,
            self.config
                .embedding_base_url
                .as_deref()
                .expect("GBRAIN_EMBEDDING_BASE_URL 已在启动校验"),
            &self.config.embedding_model,
            self.config.embedding_dimensions,
        );
        let mut texts = vec![query.to_string()];
        if !expanded_queries.is_empty() {
            texts.extend(expanded_queries.iter().cloned());
        }
        let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        // H4 fix: 使用全局共享运行时，避免每次嵌入查询创建新运行时
        let rt = crate::runtime::shared_runtime();
        match rt
            .block_on(embedder.embed_batch(&text_refs))
            .map_err(|e| std::io::Error::other(e.to_string()))
        {
            Ok(mut embeddings) => {
                if embeddings.is_empty() {
                    return (None, None);
                }
                let query_embedding = embeddings.remove(0);
                let expanded = if expanded_queries.is_empty() {
                    None
                } else {
                    Some(embeddings)
                };
                (Some(query_embedding), expanded)
            }
            Err(e) => {
                warn!(error = %e, "Query embedding failed; falling back to keyword-only search");
                (None, None)
            }
        }
    }

    fn expand_query_sync(&self, query: &str) -> Option<Vec<String>> {
        let api_key = self.config.expansion_api_key.as_deref()?;
        if api_key.is_empty() {
            return None;
        }
        let base_url = self
            .config
            .expansion_base_url
            .as_deref()
            .expect("GBRAIN_EXPANSION_BASE_URL 已在启动校验");
        // H4 fix: 使用全局共享运行时
        let rt = crate::runtime::shared_runtime();
        let expanded = rt.block_on(expand_query(
            query,
            api_key,
            base_url,
            &self.config.expansion_model,
        ));
        Some(expanded)
    }

    /// Search with vector embedding
    pub fn search(
        &self,
        query: &str,
        embedding: &[f32],
        opts: SearchOpts,
    ) -> Result<Vec<SearchResult>> {
        info!(query = %query, embedding_dims = embedding.len(), "Searching with embedding");
        let hybrid_opts = HybridOpts::default();
        let result_with_meta =
            hybrid_search(self.engine, query, Some(embedding), opts, hybrid_opts)?;
        info!(
            result_count = result_with_meta.results.len(),
            "Search complete"
        );
        Ok(result_with_meta.results)
    }

    /// Search with vector embedding, returning results with metadata
    pub fn search_with_meta(
        &self,
        query: &str,
        embedding: &[f32],
        opts: SearchOpts,
    ) -> Result<SearchResultWithMeta> {
        info!(query = %query, embedding_dims = embedding.len(), "Searching with embedding (with meta)");
        let hybrid_opts = HybridOpts::default();
        let result_with_meta =
            hybrid_search(self.engine, query, Some(embedding), opts, hybrid_opts)?;
        info!(
            result_count = result_with_meta.results.len(),
            "Search complete"
        );
        Ok(result_with_meta)
    }

    /// Get a page by slug
    pub fn get_page(&self, slug: &str) -> Result<Option<Page>> {
        validate_page_slug(slug)?;
        trace!(slug = %slug, "Loading page");
        self.engine.get_page(slug)
    }

    /// Put a page (create or update)
    ///
    /// Enhanced flow (P1-1 through P1-5):
    /// 1. Compute content hash; skip write if unchanged (P1-1)
    /// 2. Create version snapshot before overwrite (P1-2)
    /// 3. Write page + chunk + link extraction
    /// 4. Reconcile tags from frontmatter (P1-3)
    /// 5. Auto-extract timeline entries (P1-4)
    /// 6. Remove stale markdown links (P1-5)
    ///
    /// P2-2: Put page inside a write transaction (mirrors TS BrainWriter transaction scope).
    /// Used by BrainWriter to ensure validators and writes share the same transaction.
    pub fn put_page_in_transaction(
        &self,
        slug: &str,
        title: &str,
        content: &str,
        page_type: Option<PageType>,
        content_hash: Option<&str>,
    ) -> Result<Page> {
        self.validate_put_page_request(slug, content)?;
        if self.ctx.dry_run {
            return Ok(self.dry_run_put_page(slug, title, content, page_type, content_hash));
        }

        let slug = slug.to_string();
        let title = title.to_string();
        let content = content.to_string();
        let content_hash = content_hash.map(String::from);
        let ctx = self.ctx.clone();
        let config = self.config.clone();
        let precomputed_chunks =
            Self::precompute_page_body_chunks(&slug, &content, &config, page_type.as_ref());

        self.engine.transaction_with_engine(|engine| {
            let ops = Operations::with_config_in_transaction(engine, ctx.clone(), config.clone());
            ops.put_page_with_precomputed_chunks(
                &slug,
                &title,
                &content,
                page_type,
                content_hash.as_deref(),
                Some(precomputed_chunks),
            )
        })
    }

    /// P2-4: Batch put multiple pages in a single transaction.
    /// Mirrors TS batchPut() — each page is written with full auto-processing
    /// (chunking, link extraction, timeline extraction, etc.).
    /// Returns results for each page in order, with errors captured per-page
    /// rather than failing the entire batch.
    pub fn batch_put_pages(
        &self,
        pages: Vec<(String, String, String, Option<PageType>)>,
    ) -> Result<Vec<(String, Result<Page>)>> {
        let ctx = self.ctx.clone();
        let config = self.config.clone();
        let mut prepared = Vec::new();
        let mut results: Vec<Option<(String, Result<Page>)>> =
            (0..pages.len()).map(|_| None).collect();

        for (idx, (slug, title, content, page_type)) in pages.into_iter().enumerate() {
            if let Err(e) = self.validate_put_page_request(&slug, &content) {
                results[idx] = Some((slug, Err(e)));
                continue;
            }

            if self.ctx.dry_run {
                let page = self.dry_run_put_page(&slug, &title, &content, page_type, None);
                results[idx] = Some((slug, Ok(page)));
                continue;
            }

            let chunks =
                Self::precompute_page_body_chunks(&slug, &content, &config, page_type.as_ref());
            prepared.push((idx, slug, title, content, page_type, chunks));
        }

        if prepared.is_empty() {
            return Ok(results.into_iter().flatten().collect());
        }

        let tx_results = self.engine.transaction_with_engine(|engine| {
            let ops = Operations::with_config_in_transaction(engine, ctx.clone(), config.clone());
            Ok(prepared
                .into_iter()
                .map(|(idx, slug, title, content, page_type, chunks)| {
                    let result = ops.put_page_with_precomputed_chunks(
                        &slug,
                        &title,
                        &content,
                        page_type,
                        None,
                        Some(chunks),
                    );
                    (idx, slug, result)
                })
                .collect::<Vec<_>>())
        })?;

        for (idx, slug, result) in tx_results {
            results[idx] = Some((slug, result));
        }

        Ok(results.into_iter().flatten().collect())
    }

    /// P2-4: Batch add multiple links in a single transaction.
    /// Mirrors TS batchAddLinks() — validates slugs exist and deduplicates.
    pub fn batch_add_links(
        &self,
        links: Vec<(String, String, String, String)>, // (from_slug, to_slug, link_type, link_source)
    ) -> Result<Vec<(String, String, Result<()>)>> {
        self.engine.transaction_with_engine(|engine| {
            Ok(links
                .iter()
                .map(|(from_slug, to_slug, link_type, link_source)| {
                    let result = engine.add_link(
                        from_slug,
                        to_slug,
                        None,              // context
                        Some(link_type),   // link_type
                        Some(link_source), // source
                        None,              // confidence
                        None,              // metadata
                    );
                    (from_slug.clone(), to_slug.clone(), result)
                })
                .collect::<Vec<_>>())
        })
    }

    pub fn put_page(
        &self,
        slug: &str,
        title: &str,
        content: &str,
        page_type: Option<PageType>,
        content_hash: Option<&str>,
    ) -> Result<Page> {
        self.put_page_with_precomputed_chunks(slug, title, content, page_type, content_hash, None)
    }

    pub(crate) fn put_page_with_precomputed_chunks(
        &self,
        slug: &str,
        title: &str,
        content: &str,
        page_type: Option<PageType>,
        content_hash: Option<&str>,
        precomputed_body_chunks: Option<Vec<ChunkInput>>,
    ) -> Result<Page> {
        self.validate_put_page_request(slug, content)?;

        if self.ctx.dry_run {
            return Ok(self.dry_run_put_page(slug, title, content, page_type, content_hash));
        }

        info!(slug = %slug, title = %title, "Writing page");

        // Parse markdown content
        let parsed = parse_markdown(content);

        // Infer type from slug if not provided
        let pt = page_type.unwrap_or_else(|| infer_type(slug));

        // Extract frontmatter tags for hash computation and later reconciliation
        let fm_tags = extract_frontmatter_tags(&parsed.frontmatter);

        // ── P1-1: Compute content hash ──────────────────────────
        let computed_hash = compute_content_hash(
            title,
            &pt,
            &parsed.body,
            &parsed.timeline,
            &parsed.frontmatter,
            &fm_tags,
        );

        // If caller didn't pass a hash, use the computed one
        let hash_to_use = content_hash
            .map(|s| s.to_string())
            .unwrap_or(computed_hash.clone());

        // Check for skip-if-unchanged and capture existing page for version snapshot
        let existing_page = self.engine.get_page(slug)?;

        // ── P1-1: Skip-if-unchanged ─────────────────────────────
        if let Some(ref existing) = existing_page {
            if existing.content_hash.as_deref() == Some(hash_to_use.as_str()) {
                info!(slug = %slug, "Content unchanged (hash match), skipping re-write");
                return Ok(existing.clone());
            }
        }

        // ── P1-2: Create version snapshot before overwrite ──────
        if existing_page.is_some() {
            debug!(slug = %slug, "Creating version snapshot before overwrite");
            if let Err(e) = self.engine.create_version(slug) {
                warn!(slug = %slug, error = %e, "Failed to create version snapshot (non-critical)");
            }
        }

        // ── P2-8: DB-based auto_link/auto_timeline config with fallback ──
        // Check the DB for runtime overrides; fall back to struct config if absent.
        let auto_link = self
            .engine
            .get_config("auto_link")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(self.config.auto_link);
        let auto_timeline = self
            .engine
            .get_config("auto_timeline")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(self.config.auto_timeline);

        let input = PageInput {
            page_type: pt.clone(),
            title: title.to_string(),
            compiled_truth: parsed.body.clone(),
            timeline: if auto_timeline && !parsed.timeline.is_empty() {
                Some(serde_json::Value::String(parsed.timeline.clone()))
            } else {
                None
            },
            frontmatter: if parsed.frontmatter.is_null() {
                None
            } else {
                Some(parsed.frontmatter.clone())
            },
            content_hash: Some(hash_to_use),
        };

        let page = self.engine.put_page(slug, input)?;

        // ── Link extraction (existing flow) ─────────────────────
        // SECURITY: Skip auto-link for remote (MCP) callers. Auto-link's bare-slug regex
        // matches `people/X` etc. anywhere in page text, including code fences,
        // quoted strings, and prompt-injected content. An untrusted page can plant
        // arbitrary outbound links. Combined with the backlink boost in hybridSearch,
        // attacker-placed targets would surface higher in search. Local CLI users
        // (ctx.remote=false) opt into this behavior; MCP/remote writes do not.
        // Mirrors TS operations.ts: "skipped for remote (MCP) callers".
        //
        // P0-2: Wrap link reconciliation in a transaction to prevent concurrent
        // writes from creating duplicate or stale links.
        // P0-3: Validate link targets exist before adding (FK check via get_all_slugs).
        let _current_md_refs = if auto_link && !self.ctx.remote {
            // P0-2: Transaction-protected link reconciliation.
            // When already inside a transaction (put_page_in_transaction / batch_put_pages),
            // call reconcile_links_for_page directly to avoid nested BEGIN IMMEDIATE
            // (SQLite does not support nested transactions).
            if self.in_transaction {
                reconcile_links_for_page(
                    self.engine,
                    slug,
                    content,
                    &parsed.frontmatter,
                    &pt,
                    !self.ctx.remote,
                )
                .unwrap_or_else(|e| {
                    warn!(slug = %slug, error = %e, "Link reconciliation failed");
                    Vec::new()
                })
            } else {
                self.engine
                    .transaction_with_engine(|engine| {
                        reconcile_links_for_page(
                            engine,
                            slug,
                            content,
                            &parsed.frontmatter,
                            &pt,
                            !self.ctx.remote,
                        )
                    })
                    .unwrap_or_else(|e| {
                        warn!(slug = %slug, error = %e, "Link reconciliation transaction failed");
                        Vec::new()
                    })
            }
        } else {
            debug!(slug = %slug, "Auto-link disabled, skipping link extraction");
            Vec::new()
        };

        // ── Chunk the content ───────────────────────────────────
        // P2-2: Log early embedding skip if no API key configured
        if self
            .config
            .embedding_api_key
            .as_ref()
            .is_none_or(|k| k.is_empty())
        {
            debug!(
                "No GBRAIN_EMBEDDING_API_KEY configured, embedding will be skipped for this page"
            );
        }

        let mut chunks = precomputed_body_chunks.unwrap_or_else(|| {
            if self.in_transaction {
                chunk_page_content_transactional(content, &pt)
            } else {
                chunk_page_content(content, &self.config, &pt)
            }
        });
        let next_index = chunks.len() as i32;
        let code_index = if pt == PageType::Code {
            let language = parsed.frontmatter.get("language").and_then(|v| v.as_str());
            Some(chunk_code_tree_sitter(
                slug,
                &parsed.body,
                language,
                next_index,
            ))
        } else {
            Some(extract_fenced_code_index(slug, content, next_index))
        };
        if let Some(index) = &code_index {
            chunks.extend(index.chunks.clone());
        }
        if !chunks.is_empty() {
            debug!(slug = %slug, chunk_count = chunks.len(), "Chunking page content");
            // H19 修复: 增量更新 chunks，保留未变化 chunks 的 embeddings
            let retained_keys: Vec<(i32, ChunkSource)> = chunks
                .iter()
                .map(|c| (c.chunk_index, c.source.clone()))
                .collect();
            // 仅删除不在新集合中的过时 chunks
            if let Err(e) = self.engine.delete_stale_chunks(slug, &retained_keys) {
                warn!(slug = %slug, error = %e, "清理过时 chunks 失败（非致命）");
            }
            // 增量 upsert：内容不变则保留已有 embedding
            if let Err(e) = self.engine.upsert_chunks(slug, &chunks) {
                warn!(slug = %slug, error = %e, "Upsert chunks 失败（非致命）");
            }
            if let Some(index) = &code_index {
                if !index.edges.is_empty() {
                    if let Err(e) = reconcile_code_edges(self.engine, slug, index) {
                        warn!(slug = %slug, error = %e, "Failed to reconcile code edges (non-critical)");
                    }
                }
            }
        }

        // ── P1-3: Tag reconciliation ────────────────────────────
        if !fm_tags.is_empty() {
            reconcile_tags(self.engine, slug, &fm_tags)?;
        }

        // ── P1-4: Auto-timeline extraction ──────────────────────
        if auto_timeline && !self.ctx.remote {
            let tl_entries = parse_timeline_entries(content);
            if !tl_entries.is_empty() {
                debug!(slug = %slug, timeline_count = tl_entries.len(), "Auto-extracting timeline entries");
                let inputs: Vec<TimelineInput> = tl_entries
                    .into_iter()
                    .map(|(date, summary)| TimelineInput {
                        date,
                        source: None,
                        summary,
                        detail: None,
                    })
                    .collect();
                if let Err(e) = self.engine.add_timeline_entries_batch(slug, &inputs) {
                    warn!(slug = %slug, error = %e, "Failed to add auto-timeline entries (non-critical)");
                }
            }
        }

        // ── P2-3: Post-write lint hook ──────────────────────────
        // Run validators on the freshly-written page, log findings.
        // Feature-flagged via config.post_write_lint (mirrors TS runPostWriteLint).
        if self.config.post_write_lint {
            let lint_result = crate::validators::validate_all(self.engine, slug, content);
            if !lint_result.issues.is_empty() {
                for issue in &lint_result.issues {
                    let level = match issue.severity {
                        crate::validators::Severity::Error => "ERR",
                        crate::validators::Severity::Warning => "WRN",
                        crate::validators::Severity::Info => "INF",
                    };
                    debug!(
                        slug = %slug,
                        rule = %issue.rule,
                        severity = level,
                        message = %issue.message,
                        "Post-write lint finding"
                    );
                }
                info!(
                    slug = %slug,
                    issue_count = lint_result.issues.len(),
                    error_count = lint_result.errors().len(),
                    "Post-write lint complete"
                );
            }
        }

        info!(slug = %slug, title = %page.title, "Page saved successfully");
        Ok(page)
    }

    /// Delete a page
    pub fn delete_page(&self, slug: &str) -> Result<()> {
        validate_page_slug(slug)?;
        if self.ctx.dry_run {
            info!(slug = %slug, "Dry-run: would delete page");
            return Ok(());
        }
        info!(slug = %slug, "Deleting page");
        self.engine.delete_page(slug)
    }

    /// List pages with filters
    pub fn list_pages(&self, filters: PageFilters) -> Result<Vec<Page>> {
        trace!(limit = filters.limit.unwrap_or(50), "Listing pages");
        self.engine.list_pages(filters)
    }

    /// Get backlinks for a page
    pub fn get_backlinks(&self, slug: &str) -> Result<Vec<Link>> {
        trace!(slug = %slug, "Loading backlinks");
        self.engine.get_backlinks(slug)
    }

    /// Traverse the knowledge graph
    pub fn traverse_graph(&self, slug: &str, depth: usize) -> Result<Vec<GraphNode>> {
        // P1-11: Cap depth to prevent unbounded traversal (mirrors TS)
        let capped_depth = depth.min(Self::TRAVERSE_DEPTH_CAP);
        info!(slug = %slug, depth, capped_depth, "Traversing knowledge graph");
        self.engine.traverse_graph(slug, capped_depth)
    }

    /// Resolve partial slugs
    pub fn resolve_slugs(&self, partial: &str) -> Result<Vec<String>> {
        debug!(partial = %partial, "Resolving slugs");
        self.engine.resolve_slugs(partial)
    }

    /// Fuzzy search by title
    pub fn find_by_title_fuzzy(
        &self,
        query: &str,
        dir_prefix: Option<&str>,
        min_similarity: Option<f64>,
        limit: Option<usize>,
    ) -> Result<Vec<FuzzyMatch>> {
        debug!(query = %query, dir_prefix = ?dir_prefix, min_similarity = ?min_similarity, "Fuzzy title search");
        self.engine
            .find_by_title_fuzzy(query, dir_prefix, min_similarity, limit)
    }

    /// Fuzzy search using trigram similarity (pg_trgm equivalent).
    ///
    /// Unlike `find_by_title_fuzzy` which only matches against page titles,
    /// this method matches against both title and compiled_truth content,
    /// returning full `SearchResult` objects with the max similarity score.
    ///
    /// The `min_similarity` threshold (0.0-1.0) filters out low-quality matches.
    /// A value of 0.3 is a good default for catching near-matches.
    pub fn fuzzy_search(
        &self,
        query: &str,
        min_similarity: f64,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        info!(query = %query, min_similarity, limit, "Fuzzy search (trigram)");
        crate::search::fuzzy::fuzzy_search(self.engine, query, min_similarity, limit)
    }

    /// Get brain statistics
    pub fn get_stats(&self) -> Result<BrainStats> {
        trace!("Loading brain stats");
        self.engine.get_stats()
    }

    /// Get brain health
    pub fn get_health(&self) -> Result<BrainHealth> {
        trace!("Loading brain health");
        self.engine.get_health()
    }

    /// Get ingest log
    pub fn get_ingest_log(&self, limit: Option<usize>) -> Result<Vec<IngestLogEntry>> {
        trace!(limit = limit.unwrap_or(20), "Loading ingest log");
        self.engine.get_ingest_log(limit)
    }

    /// Upload a file
    pub fn file_upload(
        &self,
        source_path: &std::path::Path,
        slug: &str,
        opts: FileUploadOptions,
    ) -> Result<FileRecord> {
        validate_page_slug(slug)?;
        validate_upload_path(source_path, self.ctx.remote, &self.ctx.working_dir)?;
        // For remote callers: canonicalize and verify path is contained within working_dir
        if self.ctx.remote {
            validate_contained(source_path, &self.ctx.working_dir, self.ctx.remote)?;
        }
        info!(source_path = %source_path.display(), slug = %slug, "Uploading file");
        self.engine.file_upload(source_path, slug, opts)
    }

    /// List files
    pub fn file_list(&self, slug: Option<&str>, limit: Option<usize>) -> Result<Vec<FileRecord>> {
        // P1-11: Cap result count (mirrors TS)
        let capped_limit = limit
            .map(|l| l.min(Self::FILE_LIST_LIMIT))
            .or(Some(Self::FILE_LIST_LIMIT));
        trace!(slug = slug.unwrap_or("all"), limit = ?capped_limit, "Listing files");
        self.engine.file_list(slug, capped_limit)
    }

    /// Get file URL by storage path
    pub fn file_url_by_path(&self, storage_path: &str) -> Result<String> {
        trace!(storage_path = %storage_path, "Loading file URL");
        self.engine.file_url_by_storage_path(storage_path)
    }

    /// Verify all file records
    pub fn file_verify(&self) -> Result<FileVerifyResult> {
        info!("Verifying all file records");
        self.engine.file_verify()
    }

    /// Sync a directory of files to storage
    ///
    /// Walks the directory recursively, skipping hidden files and `.md` files.
    /// For each file: computes SHA256 hash, checks if already exists in DB,
    /// and if new, uploads via `file_upload`.
    pub fn file_sync(&self, dir: &std::path::Path) -> Result<FileSyncResult> {
        info!(dir = %dir.display(), "Starting file sync");
        let files = collect_files(dir);

        // Fetch existing files once before the loop (avoid N+1)
        let existing = self.engine.file_list(None, Some(10000))?;

        let mut uploaded = 0;
        let mut skipped = 0;

        for file_path in &files {
            let relative = file_path.strip_prefix(dir).unwrap_or(file_path);
            let relative_str = relative.to_string_lossy().replace('\\', "/");
            let storage_path = relative_str.clone();

            // Check if already exists by hash
            let data = std::fs::read(file_path)?;
            let hash = format!("{:x}", Sha256::digest(&data));

            let already_exists = existing.iter().any(|f| {
                f.hash.as_deref() == Some(hash.as_str()) && f.storage_path == storage_path
            });

            if already_exists {
                debug!(storage_path = %storage_path, "File already exists, skipping");
                skipped += 1;
                continue;
            }

            // Infer page slug from directory structure
            let filename = file_path
                .file_name()
                .ok_or_else(|| crate::error::GBrainError::FileError("No filename".to_string()))?
                .to_string_lossy()
                .to_string();

            // Validate filename
            if validate_filename(&filename).is_err() {
                warn!(filename = %filename, "Invalid filename, skipping");
                skipped += 1;
                continue;
            }

            // Infer slug from path: e.g., people/alice/photo.jpg -> people/alice
            let path_parts: Vec<&str> = relative_str.split('/').collect();
            let page_slug = if path_parts.len() > 1 {
                path_parts[..path_parts.len() - 1].join("/")
            } else {
                "unsorted".to_string()
            };

            let opts = FileUploadOptions {
                slug: page_slug.clone(),
                overwrite: false,
                max_size_bytes: None,
            };

            match self.engine.file_upload(file_path, &page_slug, opts) {
                Ok(_) => {
                    debug!(storage_path = %storage_path, "File uploaded");
                    uploaded += 1;
                }
                Err(e) => {
                    warn!(storage_path = %storage_path, error = %e, "File upload failed, skipping");
                    skipped += 1;
                }
            }
        }

        info!(uploaded, skipped, total = files.len(), "File sync complete");
        Ok(FileSyncResult { uploaded, skipped })
    }

    /// Check for missing backlinks: wiki-links in page content that have no
    /// corresponding entries in the links table. Returns list of (from_slug, to_slug).
    /// Mirrors TS backlinks.ts check command.
    pub fn check_backlinks(&self, slug: Option<&str>) -> Result<Vec<(String, String)>> {
        let mut missing = Vec::new();

        let filters = PageFilters {
            limit: None,
            ..Default::default()
        };
        let pages = if let Some(s) = slug {
            match self.engine.get_page(s)? {
                Some(p) => vec![p],
                None => {
                    return Err(crate::error::GBrainError::PageNotFound(s.to_string()));
                }
            }
        } else {
            self.engine.list_pages(filters)?
        };

        for page in pages {
            let refs = extract_entity_refs(&page.compiled_truth);
            let existing_links = self.engine.get_links(&page.slug)?;
            let existing_targets: std::collections::HashSet<String> =
                existing_links.iter().map(|l| l.to_slug.clone()).collect();

            for entity_ref in &refs {
                if !existing_targets.contains(&entity_ref.slug) {
                    missing.push((page.slug.clone(), entity_ref.slug.clone()));
                }
            }
        }

        Ok(missing)
    }

    /// Fix missing backlinks by adding entries to the links table.
    /// Returns the number of links added.
    /// Mirrors TS backlinks.ts fix command.
    pub fn fix_backlinks(&self, slug: Option<&str>, dry_run: bool) -> Result<usize> {
        let missing = self.check_backlinks(slug)?;

        if dry_run {
            info!(
                missing_count = missing.len(),
                "Dry run: would add missing backlinks"
            );
            for (from, to) in &missing {
                info!("  {} -> {}", from, to);
            }
            return Ok(missing.len());
        }

        let mut added = 0;
        for (from, to) in missing {
            match self.engine.add_link(
                &from,
                &to,
                None,
                Some("mentions"),
                Some("markdown"),
                None,
                None,
            ) {
                Ok(_) => {
                    debug!(from = %from, to = %to, "Added missing backlink");
                    added += 1;
                }
                Err(e) => {
                    warn!(from = %from, to = %to, error = %e, "Failed to add backlink");
                }
            }
        }

        info!(added, "Fixed missing backlinks");
        Ok(added)
    }

    /// Batch extract links and/or timeline entries from all pages in the database.
    /// Mirrors TS `gbrain extract links/timeline/all`.
    ///
    /// Returns counts of links/timeline entries extracted.
    pub fn extract(&self, mode: ExtractMode) -> Result<ExtractResult> {
        let filters = PageFilters {
            limit: None,
            offset: None,
            tag: None,
            page_type: None,
            updated_after: None,
            include_deleted: false,
            slug_prefix: None,
        };
        let pages = self.engine.list_pages(filters)?;
        let mut links_added = 0;
        let mut timeline_added = 0;
        let mut errors = 0;

        // Pre-fetch all slugs once for FK validation (avoids N+1 query per page)
        let valid_slugs: std::collections::HashSet<String> = match self.engine.get_all_slugs() {
            Ok(slugs) => slugs.into_iter().collect(),
            Err(e) => {
                warn!(error = %e, "FK validation: failed to fetch slugs");
                std::collections::HashSet::new()
            }
        };

        for page in &pages {
            if mode.extract_links() {
                let refs = extract_entity_refs(&page.compiled_truth);
                if !refs.is_empty() {
                    let mut links = refs_to_batch_input(&page.slug, &refs);
                    // FK validation using pre-fetched slugs
                    links.retain(|l| valid_slugs.contains(&l.to_slug));
                    if !links.is_empty() {
                        if let Ok(n) = self.engine.add_links_batch(&links) {
                            links_added += n;
                        } else {
                            errors += 1;
                        }
                    }
                }

                // Frontmatter links
                if !page.frontmatter.as_deref().unwrap_or("").is_empty() {
                    let fm: serde_json::Value =
                        serde_json::from_str(page.frontmatter.as_deref().unwrap_or("{}"))
                            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                    if !fm.is_null() {
                        // Remove old frontmatter links first
                        if let Err(e) = self
                            .engine
                            .remove_links_by_origin(&page.slug, "frontmatter")
                        {
                            warn!(slug = %page.slug, error = %e, "Failed to remove old frontmatter links");
                        }
                        let mut resolver = EngineSlugResolver::new(self.engine, true);
                        let fm_result = extract_frontmatter_refs(
                            &fm,
                            &page.slug,
                            Some(page.page_type.clone()),
                            Some(&mut resolver),
                        );
                        if !fm_result.candidates.is_empty() {
                            let mut fm_links =
                                refs_to_batch_input(&page.slug, &fm_result.candidates);
                            // FK validation using pre-fetched slugs
                            fm_links.retain(|l| valid_slugs.contains(&l.to_slug));
                            if !fm_links.is_empty() {
                                if let Ok(n) = self.engine.add_links_batch(&fm_links) {
                                    links_added += n;
                                } else {
                                    errors += 1;
                                }
                            }
                        }
                    }
                }
            }

            if mode.extract_timeline() {
                let tl_entries = parse_timeline_entries(&page.compiled_truth);
                if !tl_entries.is_empty() {
                    let inputs: Vec<TimelineInput> = tl_entries
                        .into_iter()
                        .map(|(date, summary)| TimelineInput {
                            date,
                            source: None,
                            summary,
                            detail: None,
                        })
                        .collect();
                    if let Ok(n) = self.engine.add_timeline_entries_batch(&page.slug, &inputs) {
                        timeline_added += n;
                    } else {
                        errors += 1;
                    }
                }
            }
        }

        info!(
            links_added,
            timeline_added,
            errors,
            page_count = pages.len(),
            "Extract complete"
        );
        Ok(ExtractResult {
            links_added,
            timeline_added,
            errors,
            pages_scanned: pages.len(),
        })
    }

    // -----------------------------------------------------------------------
    // KB query methods
    // -----------------------------------------------------------------------

    /// Query the KB subsystem (keyword-only FTS5 search).
    ///
    /// KB 关键词搜索（当配置了 API key 时自动启用向量混合搜索）。
    /// 通过 tokio 运行时计算查询向量，实现 FTS5 + 向量 RRF 混合搜索。
    pub fn kb_query(
        &self,
        input: &crate::kb::KbSearchInput,
    ) -> crate::error::Result<Vec<crate::kb::KbSearchResult>> {
        let conn = self.engine.connection()?;

        // 查询改写：在向量计算前完成，确保 FTS 和向量检索对齐到同一查询
        // 如果有 chat_history 且有 rewrite 配置，先改写查询
        let rewritten_query = if !input.chat_history.is_empty() {
            let rewrite_api_key = input
                .rewrite_api_key
                .as_deref()
                .filter(|v| !v.trim().is_empty())
                .or_else(|| self.config.expansion_api_key_resolved())
                .unwrap_or("");
            let rewrite_base_url = input
                .rewrite_base_url
                .as_deref()
                .filter(|v| !v.trim().is_empty())
                .or_else(|| self.config.expansion_base_url_resolved())
                .unwrap_or("");
            let rewrite_model = input
                .rewrite_model
                .as_deref()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or(self.config.expansion_model.as_str());

            if !rewrite_api_key.is_empty() && !rewrite_base_url.is_empty() {
                let rt = crate::runtime::shared_runtime();
                // rewrite_query_with_context 内部会做标准化，直接传入原始查询
                let rewritten = rt.block_on(crate::kb::search::rewrite_query_with_context(
                    &input.query,
                    &input.chat_history,
                    rewrite_api_key,
                    rewrite_base_url,
                    rewrite_model,
                ));
                if rewritten != input.query && !rewritten.is_empty() {
                    Some(rewritten)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        // 使用改写后的查询（如有）计算向量，确保向量和关键词检索对齐
        // 先做标准化（与 kb_search 内部 normalize_query 一致），避免向量和文本检索使用不同文本
        let query_for_vector = rewritten_query.as_deref().unwrap_or(&input.query);
        let query_for_vector = crate::kb::search::normalize_query(query_for_vector);

        // ── P1 修复: 先展开库/解析显式 index，再决定 query vector 模型 ──
        //
        // 问题: 之前用原始 input.library_ids (全库查询时为空) 解析 active index，
        // resolved_index=None 导致回退到全局 config；若库的 active index 模型/维度
        // 与全局 config 不一致，向量召回报错或精度错误。
        //
        // 修复: 先确定"向量检索要查询哪些库"（展开或解析），再从中提取共识模型/维度；
        // 显式 embedding_index_id 也要按目标 index 查 model/dims。
        let user_explicit_index_id: Option<i64> = input.embedding_index_id;

        // Step 1: 确定向量检索用的库集合及共识 embedding 模型/维度
        let (embedding_model_for_query, embedding_dims_for_query): (String, Option<usize>) =
            if let Some(explicit_id) = user_explicit_index_id {
                // 用户显式传了 embedding_index_id：按该 index 的 model/dimensions 生成向量
                match conn.query_row(
                    "SELECT model, dimensions FROM kb_embedding_indexes WHERE id = ?1",
                    rusqlite::params![explicit_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i32>(1)?)),
                ) {
                    Ok((m, d)) => (m, Some(d as usize)),
                    Err(_) => {
                        return Err(crate::error::GBrainError::InvalidInput(format!(
                            "指定的 embedding_index_id={} 不存在",
                            explicit_id
                        )));
                    }
                }
            } else {
                // 未显式指定：用所有待查库的 active index 提取共识模型
                let libs_for_resolve: Vec<i64> = if input.library_ids.is_empty() {
                    // 全库查询：展开所有有 active index 的库用于解析模型
                    crate::kb::embedding_index::all_library_ids_with_active_index(conn)?
                } else {
                    input.library_ids.clone()
                };

                if libs_for_resolve.is_empty() {
                    // 没有任何 active index：回退到全局 config
                    (
                        self.config.embedding_model.clone(),
                        Some(self.config.embedding_dimensions),
                    )
                } else {
                    // P2 修复: 多模型冲突时不直接报错，而是 warn 并跳过向量分支。
                    // 这样 title/FTS/metadata 等非向量召回仍能正常执行。
                    match crate::kb::embedding_index::resolve_active_index_for_libraries(
                        conn,
                        &libs_for_resolve,
                    ) {
                        Ok(Some((_, model, dims))) => (model, Some(dims as usize)),
                        Ok(None) => (
                            self.config.embedding_model.clone(),
                            Some(self.config.embedding_dimensions),
                        ),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                lib_count = libs_for_resolve.len(),
                                "kb_query: 无法解析共识 embedding 模型，将跳过向量检索，\
                                 仅执行 title/FTS/metadata 等非向量召回"
                            );
                            // 返回一个无法匹配模型的哨兵值，query_vector 设为 None
                            (String::new(), None)
                        }
                    }
                }
            };

        // P2 修复: 如果模型解析失败（多模型冲突等），跳过向量检索，
        // 仅执行 title/FTS/metadata 等非向量召回。
        let query_vector: Option<Vec<f32>> = if embedding_dims_for_query.is_none()
            && embedding_model_for_query.is_empty()
        {
            // 多模型冲突降级：不生成 query vector
            tracing::info!("kb_query: 向量检索已被禁用，仅执行非向量召回");
            None
        } else if let Some(api_key) = self.config.embedding_api_key.as_deref() {
            let embedder = crate::embedding::Embedder::new(
                api_key,
                self.config
                    .embedding_base_url
                    .as_deref()
                    .expect("GBRAIN_EMBEDDING_BASE_URL 已在启动校验"),
                &embedding_model_for_query,
                embedding_dims_for_query.unwrap_or(self.config.embedding_dimensions),
            );
            // H4 fix: 使用全局共享运行时
            let rt = crate::runtime::shared_runtime();
            let vec_result = rt
                .block_on(embedder.embed_batch(&[&query_for_vector]))
                .ok()
                .and_then(|v| v.into_iter().next());

            // P1 修复: 验证生成的向量维度与目标一致
            if let (Some(ref vec), Some(expected_dims)) = (&vec_result, embedding_dims_for_query) {
                let actual_dims = vec.len();
                if actual_dims != expected_dims {
                    return Err(crate::error::GBrainError::InvalidInput(format!(
                        "生成的查询向量维度 ({}) 与目标维度 ({}) 不一致，\
                             请检查 embedding 模型配置: model={}",
                        actual_dims, expected_dims, embedding_model_for_query
                    )));
                }
            }

            vec_result
        } else {
            None
        };

        // 注入 rerank 和 rewrite 配置，同时设置 max_chunks_per_doc 默认值
        let mut input_with_config = input.clone();
        // 如果已经在外部完成了改写，清空 chat_history 避免在 kb_search 中重复改写
        if let Some(query) = rewritten_query {
            // 改写后的 query 也要标准化后再传入 kb_search，确保完全对齐
            input_with_config.query = crate::kb::search::normalize_query(&query);
            input_with_config.chat_history.clear();
        }
        input_with_config.rerank_model = Some(self.config.expansion_model.clone());
        input_with_config.rerank_api_key = self
            .config
            .expansion_api_key_resolved()
            .map(|s| s.to_string());
        input_with_config.rerank_base_url = self
            .config
            .expansion_base_url_resolved()
            .map(|s| s.to_string());
        // 默认每文档最多 3 个 chunk，避免大文档垄断检索结果
        if input_with_config.max_chunks_per_doc.is_none() {
            input_with_config.max_chunks_per_doc = Some(3);
        }

        // P1 修复: 只有用户显式传了 embedding_index_id 时才转发给 kb_search；
        // 否则不注入，让 kb_vector_search 自行按 group_libraries_by_active_index
        // 展开所有 active index 并分别查询各自的 vec_kb_{id} 表，避免漏库。
        if input_with_config.embedding_index_id.is_none() {
            input_with_config.embedding_index_id = user_explicit_index_id;
        }

        // P2 修复: 不要把 expanded_library_ids 写回 input_with_config.library_ids。
        // library_ids=[] 表示"所有库"，title/FTS/summary/table 等非向量 retriever
        // 通过空数组做无过滤全库检索是正确的；只有向量检索需要展开为 active-index 库。
        // kb_vector_search 内部在 library_ids 为空时会自动查询所有 active index。
        // 这里保持原始 library_ids 不变，避免没有 active index 的库连关键词/标题
        // 召回也被排除。

        crate::kb::search::kb_search(conn, &input_with_config, query_vector.as_deref())
    }

    /// Combined query across both the brain (pages/chunks) and the KB.
    ///
    /// Returns brain results first, then KB results, both sorted by
    /// relevance. The two result sets are kept separate because they have
    /// different schemas.
    pub fn combined_query(
        &self,
        brain_query: &str,
        kb_input: &crate::kb::KbSearchInput,
    ) -> crate::error::Result<(
        Vec<crate::types::SearchResult>,
        Vec<crate::kb::KbSearchResult>,
    )> {
        let brain_results = self.query(brain_query, SearchOpts::default())?;
        let kb_results = self.kb_query(kb_input)?;
        Ok((brain_results, kb_results))
    }

    // ========================================================================
    // 单入口多投影融合架构 — upload_source / memory_query / promotion
    // ========================================================================

    /// 统一上传源文件
    ///
    /// 创建 Source Artifact、Occurrence、投影，并触发 KB 处理。
    /// CLI 和 MCP 都通过此方法调用。
    pub fn upload_source(
        &self,
        input: crate::artifact::types::UploadSourceInput,
    ) -> crate::error::Result<crate::artifact::types::UploadSourceOutput> {
        use crate::artifact::upload;

        // 写入操作失效查询缓存
        self.query_cache.invalidate_all();

        // 安全校验
        if self.ctx.remote {
            // MCP 远程调用：限制在 working_dir 内
            // 文件内容已通过 input.content 传入，无需路径校验
        }

        // 获取 artifact 目录（使用统一 resolver，与备份/恢复保持一致）
        let artifact_dir = self.config.artifact_dir();

        // 确保目录存在
        std::fs::create_dir_all(&artifact_dir).map_err(|e| {
            crate::error::GBrainError::FileError(format!("创建 artifact 目录失败: {}", e))
        })?;

        // 在事务中执行
        self.engine.transaction_with_engine(|engine| {
            let conn = engine.connection()?;
            // 修复：传入 config.default_kb_library_id 和 upload_default_promotion_policy，
            // 让配置默认值实际参与上传决策
            upload::upload_source(
                conn,
                &input,
                &artifact_dir,
                &self.engine.gbrain_dir(),
                self.config.default_kb_library_id,
                &self.config.embedding_model,
                self.config.embedding_dimensions,
                &self.config.upload_default_promotion_policy,
                self.config.artifact_auto_create_inbox_library,
            )
        })
    }

    /// 统一查询（memory_query）
    ///
    /// 根据查询意图自动选择 BrainFirst / EvidenceFirst / Provenance / TimelineFirst 策略。
    /// 支持 §31 query_cache 缓存。
    pub fn memory_query(
        &self,
        input: crate::artifact::types::UnifiedQueryInput,
    ) -> crate::error::Result<crate::artifact::types::UnifiedQueryResult> {
        use crate::artifact::query;

        // 计算缓存 key
        // 修复：加入 filter_slug，避免 Provenance 策略按不同 slug 查不同页面时串缓存
        let limit = input.limit.unwrap_or(10);
        let cache_key = crate::artifact::query_cache::QueryCache::make_cache_key(
            &input.query,
            &input.strategy.to_string(),
            limit,
            input.include_evidence,
            input.include_provenance,
            input.filter_slug.as_deref(),
        );

        // 尝试从缓存获取
        if let Some(cached) = self.query_cache.get(&cache_key) {
            debug!("查询缓存命中: key={}", cache_key);
            return Ok(cached);
        }

        let conn = self.engine.connection()?;
        let result = query::unified_query(conn, &input, self.engine, &self.config)?;

        // 写入缓存
        self.query_cache.set(cache_key, result.clone());

        Ok(result)
    }

    /// 列出候选变更
    pub fn promotion_list_candidates(
        &self,
        status: Option<&str>,
        candidate_type: Option<&str>,
        target_slug: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> crate::error::Result<Vec<crate::artifact::types::PromotionCandidate>> {
        use crate::artifact::promotion;

        let conn = self.engine.connection()?;
        promotion::list_candidates(conn, status, candidate_type, target_slug, limit, offset)
    }

    /// 获取候选变更详情
    pub fn promotion_get_candidate(
        &self,
        candidate_id: i64,
    ) -> crate::error::Result<Option<crate::artifact::types::PromotionCandidate>> {
        use crate::artifact::promotion;

        let conn = self.engine.connection()?;
        promotion::find_candidate_by_id(conn, candidate_id)
    }

    /// 审核候选变更（accept / reject）
    pub fn promotion_review_candidate(
        &self,
        input: crate::artifact::types::ReviewCandidateInput,
    ) -> crate::error::Result<crate::artifact::types::PromotionCandidate> {
        use crate::artifact::promotion;

        self.engine.transaction_with_engine(|engine| {
            let conn = engine.connection()?;
            promotion::review_candidate(conn, &input)
        })
    }

    /// 应用候选变更
    pub fn promotion_apply_candidate(
        &self,
        candidate_id: i64,
    ) -> crate::error::Result<crate::artifact::types::PromotionCandidate> {
        use crate::artifact::promotion;

        // 写入操作失效查询缓存
        self.query_cache.invalidate_all();

        self.engine.transaction_with_engine(|engine| {
            let conn = engine.connection()?;
            promotion::apply_candidate(conn, candidate_id)
        })
    }

    /// 回滚已应用的候选变更（§31 rollback_candidate）
    pub fn promotion_rollback_candidate(
        &self,
        candidate_id: i64,
    ) -> crate::error::Result<crate::artifact::types::PromotionCandidate> {
        use crate::artifact::promotion;

        // 写入操作失效查询缓存
        self.query_cache.invalidate_all();

        self.engine.transaction_with_engine(|engine| {
            let conn = engine.connection()?;
            promotion::rollback_candidate(conn, candidate_id)
        })
    }

    /// 投影垃圾回收（§31 gc_orphan_projections）
    pub fn gc_orphan_projections(
        &self,
        stale_days: u32,
        dry_run: bool,
    ) -> crate::error::Result<crate::artifact::projection::GcResult> {
        use crate::artifact::projection;

        // 写入操作失效查询缓存
        self.query_cache.invalidate_all();

        self.engine.transaction_with_engine(|engine| {
            let conn = engine.connection()?;
            projection::gc_orphan_projections(conn, stale_days, dry_run)
        })
    }

    /// 查询 artifact 事件历史（§7.6）
    pub fn artifact_event_history(
        &self,
        artifact_id: i64,
        limit: i64,
    ) -> crate::error::Result<Vec<crate::artifact::types::ArtifactEvent>> {
        use crate::artifact::store;

        let conn = self.engine.connection()?;
        store::find_events_by_artifact(conn, artifact_id, limit)
            .map_err(|e| crate::error::GBrainError::Database(format!("查询事件历史失败: {}", e)))
    }

    /// 自动应用低风险候选
    pub fn promotion_auto_apply(&self, artifact_id: i64) -> crate::error::Result<Vec<i64>> {
        use crate::artifact::promotion;

        // 写入操作失效查询缓存
        self.query_cache.invalidate_all();

        self.engine.transaction_with_engine(|engine| {
            let conn = engine.connection()?;
            promotion::auto_apply_candidates(conn, artifact_id, None, None)
        })
    }

    /// 批量应用候选（§10.5 promotion_apply_all）
    pub fn promotion_batch_apply(
        &self,
        artifact_id: Option<i64>,
        risk_filter: Option<&str>,
        dry_run: bool,
    ) -> crate::error::Result<crate::artifact::types::BatchApplyResult> {
        use crate::artifact::promotion;

        // 写入操作失效查询缓存
        self.query_cache.invalidate_all();

        self.engine.transaction_with_engine(|engine| {
            let conn = engine.connection()?;
            promotion::batch_apply_candidates(conn, artifact_id, risk_filter, dry_run)
        })
    }

    /// Artifact 健康检查
    pub fn artifact_health_check(
        &self,
    ) -> crate::error::Result<crate::artifact::types::ArtifactHealthReport> {
        use crate::artifact::query;

        let conn = self.engine.connection()?;
        query::check_artifact_health(conn)
    }

    /// 获取 Source Artifact 详情
    pub fn get_artifact(
        &self,
        artifact_id: i64,
    ) -> crate::error::Result<Option<crate::artifact::types::SourceArtifact>> {
        use crate::artifact::store;

        let conn = self.engine.connection()?;
        store::find_artifact_by_id(conn, artifact_id)
            .map_err(|e| crate::error::GBrainError::Database(e.to_string()))
    }

    /// 获取 Source Artifact 详情（按 UID）
    pub fn get_artifact_by_uid(
        &self,
        uid: &str,
    ) -> crate::error::Result<Option<crate::artifact::types::SourceArtifact>> {
        use crate::artifact::store;

        let conn = self.engine.connection()?;
        store::find_artifact_by_uid(conn, uid)
            .map_err(|e| crate::error::GBrainError::Database(e.to_string()))
    }

    /// 列出 Source Artifacts
    pub fn list_artifacts(
        &self,
        limit: i64,
        offset: i64,
    ) -> crate::error::Result<Vec<crate::artifact::types::SourceArtifact>> {
        use crate::artifact::store;

        let conn = self.engine.connection()?;
        store::list_active_artifacts(conn, limit, offset)
            .map_err(|e| crate::error::GBrainError::Database(e.to_string()))
    }

    /// 获取 Artifact 的投影列表
    pub fn get_artifact_projections(
        &self,
        artifact_id: i64,
    ) -> crate::error::Result<Vec<crate::artifact::types::ArtifactProjection>> {
        use crate::artifact::store;

        let conn = self.engine.connection()?;
        store::find_projections_by_artifact(conn, artifact_id)
            .map_err(|e| crate::error::GBrainError::Database(e.to_string()))
    }

    /// 获取 Artifact 的 Occurrence 列表
    pub fn get_artifact_occurrences(
        &self,
        artifact_id: i64,
    ) -> crate::error::Result<Vec<crate::artifact::types::ArtifactOccurrence>> {
        use crate::artifact::store;

        let conn = self.engine.connection()?;
        store::find_occurrences_by_artifact(conn, artifact_id)
            .map_err(|e| crate::error::GBrainError::Database(e.to_string()))
    }

    /// 获取 Provenance（按 brain slug）
    pub fn get_provenance(
        &self,
        brain_slug: &str,
    ) -> crate::error::Result<Vec<crate::artifact::types::ProvenanceRecord>> {
        use crate::artifact::provenance;

        let conn = self.engine.connection()?;
        provenance::find_provenance_by_brain_slug(conn, brain_slug)
    }

    /// 替代旧投影（§31 版本链 superseded_by）
    pub fn supersede_projection(
        &self,
        old_proj_id: i64,
        new_proj_id: i64,
    ) -> crate::error::Result<()> {
        use crate::artifact::projection;

        self.engine.transaction_with_engine(|engine| {
            let conn = engine.connection()?;
            projection::supersede_projection(conn, old_proj_id, new_proj_id)
        })
    }

    /// 查询投影版本链历史（§31）
    ///
    /// 修复：增加 artifact_id 和 projection_type 可选过滤，
    /// 避免同一 library 下多个 artifact 的投影混合
    pub fn get_projection_history(
        &self,
        projection_key: &str,
        artifact_id: Option<i64>,
        projection_type: Option<&str>,
        limit: i64,
    ) -> crate::error::Result<Vec<crate::artifact::types::ArtifactProjection>> {
        use crate::artifact::projection;

        let conn = self.engine.connection()?;
        projection::get_projection_history(
            conn,
            projection_key,
            artifact_id,
            projection_type,
            limit,
        )
    }
}

/// Collect non-markdown files from a directory, skipping hidden files and symlinks
fn collect_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    walk_dir(dir, dir, &mut files);
    files.sort();
    files
}

fn walk_dir(_base: &std::path::Path, dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip hidden files
        if name_str.starts_with('.') {
            continue;
        }

        let path = entry.path();

        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };

        // Skip symlinks
        if metadata.is_symlink() {
            continue;
        }

        if metadata.is_dir() {
            walk_dir(_base, &path, files);
        } else if !name_str.ends_with(".md") {
            files.push(path);
        }
    }
}

// ── P1-1: Content hash computation ──────────────────────────────

/// Serialize a serde_json::Value to a canonical JSON string with sorted keys.
/// Ensures the same logical frontmatter produces the same hash regardless of key order.
///
/// C3 fix: 使用 `serde_json::to_string()` 对 key/value 进行正确的 JSON 转义，
/// 避免含 `:`, `{`, `"` 等字符时产生歧义输出。
/// 与 src/artifact/promotion.rs `serialize_canonical_json` 保持一致。
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<_> = map.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let pairs: Vec<String> = sorted
                .iter()
                .map(|(k, v)| {
                    let key = serde_json::to_string(k).unwrap_or_else(|_| k.to_string());
                    let val = canonical_json(v);
                    format!("{}:{}", key, val)
                })
                .collect();
            format!("{{{}}}", pairs.join(","))
        }
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(canonical_json).collect();
            format!("[{}]", items.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// Compute SHA-256 content hash over the key page fields.
/// Used to detect unchanged pages and skip redundant re-writes.
fn compute_content_hash(
    title: &str,
    page_type: &PageType,
    compiled_truth: &str,
    timeline: &str,
    frontmatter: &serde_json::Value,
    tags: &[String],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(title.as_bytes());
    hasher.update(page_type.to_string().as_bytes());
    hasher.update(compiled_truth.as_bytes());
    hasher.update(timeline.as_bytes());
    // Serialize frontmatter to a canonical JSON string for stable hashing
    // Use sorted keys to ensure same logical frontmatter produces same hash
    let fm_str = canonical_json(frontmatter);
    hasher.update(fm_str.as_bytes());
    for tag in tags {
        hasher.update(tag.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// Extract tags from frontmatter's "tags" array field.
/// Returns an empty vec if frontmatter is missing or has no tags.
fn extract_frontmatter_tags(frontmatter: &serde_json::Value) -> Vec<String> {
    let Some(obj) = frontmatter.as_object() else {
        return Vec::new();
    };
    let Some(tags_val) = obj.get("tags") else {
        return Vec::new();
    };
    let Some(arr) = tags_val.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect()
}

// ── P1-3: Tag reconciliation ────────────────────────────────────

/// Reconcile frontmatter tags with existing DB tags for a page.
/// Adds missing tags and removes orphaned tags (present in DB but not in frontmatter).
fn extract_fenced_code_index(slug: &str, content: &str, start_index: i32) -> CodeIndex {
    let mut chunks = Vec::new();
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let mut in_fence = false;
    let mut lang: Option<String> = None;
    let mut buf = String::new();
    let mut start_line = 0_i32;

    for (line_idx, line) in content.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if in_fence {
                if !buf.trim().is_empty() {
                    let local_start = start_index + chunks.len() as i32;
                    let indexed = index_code(slug, buf.trim(), lang.as_deref(), local_start);
                    if indexed.symbols.is_empty() {
                        chunks.push(ChunkInput {
                            chunk_index: local_start,
                            chunk_text: buf.trim().to_string(),
                            source: ChunkSource::FencedCode,
                            token_count: crate::chunker::estimate_tokens(&buf) as i32,
                            embedding: None,
                            model: None,
                            language: lang.clone(),
                            symbol_name: None,
                            symbol_type: Some("fenced_code".to_string()),
                            start_line: Some(start_line),
                            end_line: Some(line_idx as i32 + 1),
                            parent_symbol_path: None,
                            symbol_name_qualified: None,
                            doc_comment: None,
                        });
                    } else {
                        let line_offset = start_line - 1;
                        for mut chunk in indexed.chunks {
                            if let Some(line) = chunk.start_line {
                                chunk.start_line = Some(line + line_offset);
                            }
                            if let Some(line) = chunk.end_line {
                                chunk.end_line = Some(line + line_offset);
                            }
                            chunks.push(chunk);
                        }
                        symbols.extend(indexed.symbols.into_iter().map(|mut sym| {
                            sym.start_line += line_offset;
                            sym.end_line += line_offset;
                            sym
                        }));
                        edges.extend(indexed.edges);
                    }
                }
                in_fence = false;
                lang = None;
                buf.clear();
            } else {
                in_fence = true;
                let tag = trimmed.trim_start_matches("```").trim();
                lang = if tag.is_empty() {
                    None
                } else {
                    Some(tag.split_whitespace().next().unwrap_or(tag).to_lowercase())
                };
                start_line = line_idx as i32 + 2;
            }
            continue;
        }
        if in_fence {
            buf.push_str(line);
            buf.push('\n');
        }
    }

    CodeIndex {
        chunks,
        symbols,
        edges,
    }
}

fn reconcile_code_edges(engine: &SqliteEngine, slug: &str, index: &CodeIndex) -> Result<()> {
    let chunks = engine.get_chunks(slug)?;
    let code_chunk_ids: Vec<i64> = chunks
        .iter()
        .filter(|c| c.source == ChunkSource::FencedCode)
        .map(|c| c.id)
        .collect();
    let _ = engine.delete_code_edges_for_chunks(&code_chunk_ids)?;

    let mut symbol_to_chunk: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    for chunk in chunks
        .iter()
        .filter(|c| c.source == ChunkSource::FencedCode)
    {
        if let Some(symbol) = &chunk.symbol_name {
            symbol_to_chunk.insert(symbol.clone(), chunk.id);
            if let Some(name) = symbol.rsplit('.').next() {
                symbol_to_chunk.entry(name.to_string()).or_insert(chunk.id);
            }
        }
    }

    let mut edges = index.edges.clone();
    for edge in &mut edges {
        edge.from_chunk_id = symbol_to_chunk.get(&edge.from_symbol).copied();
        edge.to_chunk_id = symbol_to_chunk.get(&edge.to_symbol).copied();
    }
    if !edges.is_empty() {
        engine.add_code_edges(&edges)?;
    }
    Ok(())
}

fn reconcile_tags(engine: &SqliteEngine, slug: &str, fm_tags: &[String]) -> Result<()> {
    let existing = engine.get_tags(slug)?;
    let existing_set: std::collections::HashSet<&str> =
        existing.iter().map(|s| s.as_str()).collect();
    let new_set: std::collections::HashSet<&str> = fm_tags.iter().map(|s| s.as_str()).collect();

    // Add tags present in frontmatter but missing from DB
    for tag in fm_tags {
        if !existing_set.contains(tag.as_str()) {
            debug!(slug = %slug, tag = %tag, "Adding missing tag");
            engine.add_tag(slug, tag)?;
        }
    }

    // Remove tags present in DB but no longer in frontmatter
    for tag in &existing {
        if !new_set.contains(tag.as_str()) {
            debug!(slug = %slug, tag = %tag, "Removing orphaned tag");
            if let Err(e) = engine.remove_tag(slug, tag) {
                warn!(slug = %slug, tag = %tag, error = %e, "Failed to remove orphaned tag");
            }
        }
    }

    Ok(())
}

// ── P1-5: Stale link reconciliation ─────────────────────────────

/// Remove stale markdown-sourced links that are no longer present in the page content.
/// P0-2: Link reconciliation for a page — extracts entity refs and frontmatter refs,
/// adds new links, removes stale links. This function does NOT wrap in a transaction;
/// callers must wrap it in `transaction_with_engine` if a transaction is needed.
/// When called from `put_page_in_transaction` or `batch_put_pages`, the outer transaction
/// already provides the necessary isolation, so we call this directly to avoid nested
/// `BEGIN IMMEDIATE` which SQLite does not support.
fn reconcile_links_for_page(
    engine: &SqliteEngine,
    slug: &str,
    content: &str,
    frontmatter: &serde_json::Value,
    page_type: &PageType,
    allow_fuzzy: bool,
) -> Result<Vec<crate::link_extraction::EntityRef>> {
    let refs = extract_entity_refs(content);

    // P0-3: FK validation — get all existing slugs to filter dead links
    let valid_slugs: std::collections::HashSet<String> = match engine.get_all_slugs() {
        Ok(slugs) => slugs.into_iter().collect(),
        Err(e) => {
            warn!(slug = %slug, error = %e, "FK validation failed: cannot verify link targets, skipping link extraction");
            return Ok(Vec::new());
        }
    };

    if !refs.is_empty() {
        debug!(slug = %slug, ref_count = refs.len(), "Extracting entity refs and adding links");
        let mut links = refs_to_batch_input(slug, &refs);
        // P0-3: Filter out links to non-existent slugs
        links.retain(|l| valid_slugs.contains(&l.to_slug));
        if !links.is_empty() {
            if let Err(e) = engine.add_links_batch(&links) {
                warn!(slug = %slug, error = %e, "Failed to add entity ref links (non-critical)");
            }
        }
    }

    // Extract frontmatter links (with reconciliation: delete old frontmatter links first)
    let mut fm_target_slugs: Vec<String> = Vec::new();
    if !frontmatter.is_null() {
        // Reconciliation: remove old frontmatter-extracted links for this slug
        if let Err(e) = engine.remove_links_by_origin(slug, "frontmatter") {
            warn!(slug = %slug, error = %e, "Failed to remove old frontmatter links (non-critical)");
        }
        // Use EngineSlugResolver to check which frontmatter refs actually exist in DB
        let mut resolver = EngineSlugResolver::new(engine, allow_fuzzy);
        let fm_result = extract_frontmatter_refs(
            frontmatter,
            slug,
            Some(page_type.clone()),
            Some(&mut resolver),
        );
        if !fm_result.candidates.is_empty() {
            debug!(slug = %slug, fm_ref_count = fm_result.candidates.len(), unresolved_count = fm_result.unresolved.len(), "Extracting frontmatter refs");
            let mut fm_links = refs_to_batch_input(slug, &fm_result.candidates);
            // P0-3: Filter out frontmatter links to non-existent slugs
            fm_links.retain(|l| valid_slugs.contains(&l.to_slug));
            if !fm_links.is_empty() {
                if let Err(e) = engine.add_links_batch(&fm_links) {
                    warn!(slug = %slug, error = %e, "Failed to add frontmatter links (non-critical)");
                }
            }
        }
        // Collect frontmatter target slugs for inbound reconciliation
        fm_target_slugs = fm_result
            .candidates
            .iter()
            .map(|r| r.slug.clone())
            .collect();
        // Log unresolved frontmatter references
        for (field, name) in &fm_result.unresolved {
            warn!(slug = %slug, field = %field, name = %name, "Unresolved frontmatter reference");
        }
    }

    // P1-5: Link reconciliation — remove stale markdown links
    reconcile_stale_links(engine, slug, &refs)?;

    // P0-4: Inbound link reconciliation — remove stale frontmatter inbound links
    reconcile_stale_inbound_links(engine, slug, &fm_target_slugs)?;

    Ok(refs)
}

/// Compares current entity refs with existing DB links; removes any DB link whose
/// link_source is Markdown and whose to_slug is not in the current refs.
fn reconcile_stale_links(
    engine: &SqliteEngine,
    slug: &str,
    current_refs: &[crate::link_extraction::EntityRef],
) -> Result<()> {
    let existing_links = engine.get_links(slug)?;
    let current_targets: std::collections::HashSet<&str> =
        current_refs.iter().map(|r| r.slug.as_str()).collect();

    for link in &existing_links {
        // Only reconcile markdown-sourced links; frontmatter/manual links are managed separately
        if link.link_source.as_ref() == Some(&LinkSource::Markdown)
            && !current_targets.contains(link.to_slug.as_str())
        {
            debug!(
                slug = %slug,
                to_slug = %link.to_slug,
                link_type = %link.link_type,
                "Removing stale markdown link"
            );
            if let Err(e) = engine.remove_link(
                &link.from_slug,
                &link.to_slug,
                Some(&link.link_type),
                link.context.as_deref(),
                Some("markdown"),
            ) {
                warn!(slug = %slug, to_slug = %link.to_slug, error = %e, "Failed to remove stale link");
            }
        }
    }

    Ok(())
}

// ── P0-4: Inbound link reconciliation ─────────────────────────────

/// Remove stale inbound (frontmatter-sourced) links where the origin page
/// no longer references this slug in its frontmatter.
///
/// In TS, operations.ts:406-410 separates candidates into outbound
/// (fromSlug === slug) and inbound (fromSlug !== slug), and reconciles
/// both. The Rust version only reconciled outbound links; this function
/// adds inbound reconciliation.
///
/// When a page's frontmatter changes (e.g., company removes `key_people: [alice]`),
/// the inbound link `people/alice -> companies/acme` should be removed.
fn reconcile_stale_inbound_links(
    engine: &SqliteEngine,
    slug: &str,
    _current_fm_targets: &[String],
) -> Result<()> {
    // Get all links where this slug is the target (backlinks = inbound links)
    let backlinks = engine.get_backlinks(slug)?;

    for link in &backlinks {
        // Only reconcile frontmatter-sourced inbound links
        // (markdown-sourced inbound links are handled by the origin page's own reconciliation)
        if link.link_source.as_ref() == Some(&LinkSource::Frontmatter) && link.from_slug != slug
        // inbound only (from_slug !== this slug)
        {
            // Check whether the origin page (from_slug) still references
            // this slug in its frontmatter. We need to read the origin
            // page's frontmatter and extract its link targets.
            let origin_page = engine.get_page(&link.from_slug)?;
            let still_referenced = match origin_page {
                Some(page) => {
                    // Extract link targets from the origin page's frontmatter
                    // (not compiled_truth — frontmatter-sourced links must be checked
                    // against frontmatter to avoid false retention)
                    let fm: serde_json::Value = page
                        .frontmatter
                        .as_deref()
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or_default();
                    let fm_refs = extract_frontmatter_refs(
                        &fm,
                        &link.from_slug,
                        Some(page.page_type.clone()),
                        None,
                    );
                    fm_refs.candidates.iter().any(|r| r.slug == slug)
                        // Also check body entity refs for non-frontmatter links
                        || {
                            let body_refs = extract_entity_refs(&page.compiled_truth);
                            body_refs.iter().any(|r| r.slug == slug)
                        }
                }
                None => {
                    // Origin page was deleted — link is stale
                    false
                }
            };

            if !still_referenced {
                debug!(
                    slug = %slug,
                    from_slug = %link.from_slug,
                    link_type = %link.link_type,
                    "Removing stale inbound frontmatter link"
                );
                if let Err(e) = engine.remove_link(
                    &link.from_slug,
                    &link.to_slug,
                    Some(&link.link_type),
                    link.context.as_deref(),
                    Some("frontmatter"),
                ) {
                    warn!(slug = %slug, from_slug = %link.from_slug, error = %e, "Failed to remove stale inbound link");
                }
            }
        }
    }

    Ok(())
}
