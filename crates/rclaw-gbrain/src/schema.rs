//! SQLite schema DDL
//!
//! 完整的 SQLite schema，包含 FTS5、触发器和索引。
/// 当前 schema 版本号，新数据库会直接写入此版本以跳过历史迁移
pub const SCHEMA_VERSION: i32 = 38;

/// 完整的 schema DDL，新数据库一次性创建
pub const SCHEMA_DDL: &str = r#"
-- PRAGMAs
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA synchronous = NORMAL;
PRAGMA cache_size = -64000;
PRAGMA temp_store = MEMORY;

-- Schema version tracking
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Pages
CREATE TABLE IF NOT EXISTS pages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    slug TEXT NOT NULL UNIQUE,
    page_type TEXT NOT NULL DEFAULT 'note',
    title TEXT NOT NULL DEFAULT '',
    compiled_truth TEXT NOT NULL DEFAULT '',
    timeline TEXT NOT NULL DEFAULT '',
    frontmatter TEXT NOT NULL DEFAULT '',
    content_hash TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    deleted_at TEXT,
    title_tokens TEXT NOT NULL DEFAULT '',
    compiled_truth_tokens TEXT NOT NULL DEFAULT '',
    timeline_tokens TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_pages_slug ON pages(slug);
CREATE INDEX IF NOT EXISTS idx_pages_page_type ON pages(page_type);
CREATE INDEX IF NOT EXISTS idx_pages_updated_at ON pages(updated_at);
CREATE INDEX IF NOT EXISTS idx_pages_deleted_at ON pages(deleted_at);

-- FTS5 virtual table for full-text search (weighted: title > compiled_truth > timeline)
CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
    slug,
    title,
    title_tokens,
    compiled_truth,
    compiled_truth_tokens,
    timeline,
    timeline_tokens,
    content='pages',
    content_rowid='id',
    tokenize='unicode61'
);

-- Triggers to keep FTS5 in sync
CREATE TRIGGER IF NOT EXISTS pages_fts_insert AFTER INSERT ON pages BEGIN
    INSERT INTO pages_fts(rowid, slug, title, title_tokens, compiled_truth, compiled_truth_tokens, timeline, timeline_tokens)
    VALUES (new.id, new.slug, new.title, new.title_tokens, new.compiled_truth, new.compiled_truth_tokens, new.timeline, new.timeline_tokens);
END;

CREATE TRIGGER IF NOT EXISTS pages_fts_update AFTER UPDATE ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, slug, title, title_tokens, compiled_truth, compiled_truth_tokens, timeline, timeline_tokens)
    VALUES ('delete', old.id, old.slug, old.title, old.title_tokens, old.compiled_truth, old.compiled_truth_tokens, old.timeline, old.timeline_tokens);
    INSERT INTO pages_fts(rowid, slug, title, title_tokens, compiled_truth, compiled_truth_tokens, timeline, timeline_tokens)
    VALUES (new.id, new.slug, new.title, new.title_tokens, new.compiled_truth, new.compiled_truth_tokens, new.timeline, new.timeline_tokens);
END;

CREATE TRIGGER IF NOT EXISTS pages_fts_delete AFTER DELETE ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, slug, title, title_tokens, compiled_truth, compiled_truth_tokens, timeline, timeline_tokens)
    VALUES ('delete', old.id, old.slug, old.title, old.title_tokens, old.compiled_truth, old.compiled_truth_tokens, old.timeline, old.timeline_tokens);
END;

-- FTS5 trigram virtual table for fuzzy title matching
-- Provides indexed substring matching (approximates pg_trgm GIN index)
CREATE VIRTUAL TABLE IF NOT EXISTS pages_trgm USING fts5(
    title,
    content='pages',
    content_rowid='id',
    tokenize="trigram"
);

-- Triggers to keep trigram index in sync
CREATE TRIGGER IF NOT EXISTS pages_trgm_insert AFTER INSERT ON pages BEGIN
    INSERT INTO pages_trgm(rowid, title)
    VALUES (new.id, new.title);
END;

CREATE TRIGGER IF NOT EXISTS pages_trgm_update AFTER UPDATE ON pages BEGIN
    INSERT INTO pages_trgm(pages_trgm, rowid, title)
    VALUES ('delete', old.id, old.title);
    INSERT INTO pages_trgm(rowid, title)
    VALUES (new.id, new.title);
END;

CREATE TRIGGER IF NOT EXISTS pages_trgm_delete AFTER DELETE ON pages BEGIN
    INSERT INTO pages_trgm(pages_trgm, rowid, title)
    VALUES ('delete', old.id, old.title);
END;

-- Chunks
CREATE TABLE IF NOT EXISTS chunks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    page_id INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL DEFAULT 0,
    chunk_text TEXT NOT NULL DEFAULT '',
    chunk_source TEXT NOT NULL DEFAULT 'compiled_truth',
    token_count INTEGER NOT NULL DEFAULT 0,
    model TEXT NOT NULL DEFAULT 'text-embedding-3-large',
    embedded_at TEXT,
    language TEXT,
    symbol_name TEXT,
    symbol_type TEXT,
    start_line INTEGER,
    end_line INTEGER,
    parent_symbol_path TEXT,
    symbol_name_qualified TEXT,
    doc_comment TEXT,
    chunk_text_tokens TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(page_id, chunk_index, chunk_source)
);

CREATE INDEX IF NOT EXISTS idx_chunks_page_id ON chunks(page_id);
CREATE INDEX IF NOT EXISTS idx_chunks_language ON chunks(language);
CREATE INDEX IF NOT EXISTS idx_chunks_symbol_name ON chunks(symbol_name);
CREATE INDEX IF NOT EXISTS idx_chunks_symbol_qualified ON chunks(symbol_name_qualified);
CREATE INDEX IF NOT EXISTS idx_chunks_parent_symbol ON chunks(parent_symbol_path);

-- FTS5 virtual table for chunk/code-chunk search.
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    chunk_text,
    chunk_text_tokens,
    language,
    symbol_name,
    symbol_type,
    content='chunks',
    content_rowid='id',
    tokenize='unicode61'
);

CREATE TRIGGER IF NOT EXISTS chunks_fts_insert AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, chunk_text, chunk_text_tokens, language, symbol_name, symbol_type)
    VALUES (new.id, new.chunk_text, new.chunk_text_tokens,
            coalesce(new.language, ''), coalesce(new.symbol_name, ''), coalesce(new.symbol_type, ''));
END;

CREATE TRIGGER IF NOT EXISTS chunks_fts_update AFTER UPDATE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, chunk_text, chunk_text_tokens, language, symbol_name, symbol_type)
    VALUES ('delete', old.id, old.chunk_text, old.chunk_text_tokens,
            coalesce(old.language, ''), coalesce(old.symbol_name, ''), coalesce(old.symbol_type, ''));
    INSERT INTO chunks_fts(rowid, chunk_text, chunk_text_tokens, language, symbol_name, symbol_type)
    VALUES (new.id, new.chunk_text, new.chunk_text_tokens,
            coalesce(new.language, ''), coalesce(new.symbol_name, ''), coalesce(new.symbol_type, ''));
END;

CREATE TRIGGER IF NOT EXISTS chunks_fts_delete AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, chunk_text, chunk_text_tokens, language, symbol_name, symbol_type)
    VALUES ('delete', old.id, old.chunk_text, old.chunk_text_tokens,
            coalesce(old.language, ''), coalesce(old.symbol_name, ''), coalesce(old.symbol_type, ''));
END;

-- Portable fallback embedding store. sqlite-vec is used when available; this
-- table keeps vector search functional in zero-config builds where vec0 is not
-- loadable.
CREATE TABLE IF NOT EXISTS chunk_embeddings (
    chunk_id INTEGER PRIMARY KEY REFERENCES chunks(id) ON DELETE CASCADE,
    embedding BLOB NOT NULL,
    dimensions INTEGER NOT NULL,
    model TEXT NOT NULL DEFAULT 'text-embedding-3-large',
    embedded_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Links
CREATE TABLE IF NOT EXISTS links (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_slug TEXT NOT NULL,
    to_slug TEXT NOT NULL,
    link_type TEXT NOT NULL DEFAULT 'mentions',
    context TEXT NOT NULL DEFAULT '',
    link_source TEXT NOT NULL DEFAULT 'auto',
    origin_slug TEXT NOT NULL DEFAULT '',
    origin_field TEXT NOT NULL DEFAULT '',
    direction TEXT NOT NULL DEFAULT 'outgoing',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(from_slug, to_slug, link_type, link_source)
);

CREATE INDEX IF NOT EXISTS idx_links_from_slug ON links(from_slug);
CREATE INDEX IF NOT EXISTS idx_links_to_slug ON links(to_slug);
CREATE INDEX IF NOT EXISTS idx_links_direction ON links(direction);

-- Code edges: symbol call/reference graph extracted from code chunks.
CREATE TABLE IF NOT EXISTS code_edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_slug TEXT NOT NULL,
    from_symbol TEXT NOT NULL,
    from_symbol_qualified TEXT,
    to_slug TEXT NOT NULL,
    to_symbol TEXT NOT NULL,
    to_symbol_qualified TEXT,
    edge_type TEXT NOT NULL DEFAULT 'calls',
    confidence REAL NOT NULL DEFAULT 1.0,
    context TEXT,
    from_chunk_id INTEGER REFERENCES chunks(id) ON DELETE CASCADE,
    to_chunk_id INTEGER REFERENCES chunks(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(from_slug, from_symbol, to_slug, to_symbol, edge_type, from_chunk_id)
);

CREATE INDEX IF NOT EXISTS idx_code_edges_from ON code_edges(from_slug, from_symbol);
CREATE INDEX IF NOT EXISTS idx_code_edges_to ON code_edges(to_slug, to_symbol);
CREATE INDEX IF NOT EXISTS idx_code_edges_from_chunk ON code_edges(from_chunk_id);
CREATE INDEX IF NOT EXISTS idx_code_edges_to_chunk ON code_edges(to_chunk_id);

-- Code edges by symbol: unresolved edges where target chunk hasn't been imported yet
CREATE TABLE IF NOT EXISTS code_edges_symbol (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_chunk_id INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
    from_symbol_qualified TEXT NOT NULL,
    to_symbol_qualified TEXT NOT NULL,
    edge_type TEXT NOT NULL DEFAULT 'calls',
    edge_metadata TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(from_chunk_id, to_symbol_qualified, edge_type)
);

CREATE INDEX IF NOT EXISTS idx_code_edges_symbol_from ON code_edges_symbol(from_chunk_id);
CREATE INDEX IF NOT EXISTS idx_code_edges_symbol_to ON code_edges_symbol(to_symbol_qualified, edge_type);

-- Tags
CREATE TABLE IF NOT EXISTS tags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    page_id INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    tag TEXT NOT NULL,
    UNIQUE(page_id, tag)
);

CREATE INDEX IF NOT EXISTS idx_tags_page_id ON tags(page_id);
CREATE INDEX IF NOT EXISTS idx_tags_tag ON tags(tag);

-- Timeline
CREATE TABLE IF NOT EXISTS timeline (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    page_id INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    date TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL DEFAULT '',
    summary TEXT NOT NULL DEFAULT '',
    detail TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(page_id, date, summary)
);

CREATE INDEX IF NOT EXISTS idx_timeline_page_id ON timeline(page_id);
CREATE INDEX IF NOT EXISTS idx_timeline_date ON timeline(date);

-- Raw data
CREATE TABLE IF NOT EXISTS raw_data (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    page_id INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    source TEXT NOT NULL DEFAULT '',
    data TEXT NOT NULL DEFAULT '{}',
    fetched_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(page_id, source)
);

CREATE INDEX IF NOT EXISTS idx_raw_data_page_id ON raw_data(page_id);

-- Page versions
CREATE TABLE IF NOT EXISTS page_versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    page_id INTEGER NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    compiled_truth TEXT NOT NULL DEFAULT '',
    frontmatter TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    page_type TEXT NOT NULL DEFAULT 'note',
    snapshot_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_page_versions_page_id ON page_versions(page_id);

-- Config
CREATE TABLE IF NOT EXISTS config (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL DEFAULT ''
);

-- Ingest log
CREATE TABLE IF NOT EXISTS ingest_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source TEXT NOT NULL DEFAULT '',
    pages_updated TEXT NOT NULL DEFAULT '[]',
    status TEXT NOT NULL DEFAULT 'pending',
    error TEXT,
    source_type TEXT NOT NULL DEFAULT '',
    source_ref TEXT NOT NULL DEFAULT '',
    summary TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Files
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    page_slug TEXT NOT NULL DEFAULT '',
    filename TEXT NOT NULL DEFAULT '',
    storage_path TEXT NOT NULL DEFAULT '',
    mime_type TEXT,
    size_bytes INTEGER NOT NULL DEFAULT 0,
    checksum TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_files_page_slug ON files(page_slug);

-- 任务队列
CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_type TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'pending',
    priority INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 3,
    error TEXT,
    cancel_reason TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT,
    started_at TEXT,
    completed_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status);
CREATE INDEX IF NOT EXISTS idx_jobs_type_status ON jobs(job_type, status);

-- ═══════════════════════════════════════════════════════════════════════════════
-- KB 子系统表
-- ═══════════════════════════════════════════════════════════════════════════════

-- KB 库（知识库顶层容器）
CREATE TABLE IF NOT EXISTS kb_libraries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    name TEXT NOT NULL,
    raptor_enabled INTEGER NOT NULL DEFAULT 1,
    raptor_llm_base_url TEXT NOT NULL DEFAULT '',
    raptor_llm_secret_ref TEXT NOT NULL DEFAULT '',
    raptor_llm_model TEXT NOT NULL DEFAULT '',
    chunk_size INTEGER NOT NULL DEFAULT 512,
    chunk_overlap INTEGER NOT NULL DEFAULT 50,
    batch_max_documents INTEGER NOT NULL DEFAULT 3,
    batch_max_chunks INTEGER NOT NULL DEFAULT 10,
    sort_order INTEGER NOT NULL DEFAULT 0,
    embedding_provider TEXT NOT NULL DEFAULT '',
    embedding_model TEXT NOT NULL DEFAULT '',
    embedding_dimensions INTEGER,
    search_profile TEXT NOT NULL DEFAULT '',
    rerank_enabled INTEGER NOT NULL DEFAULT 1,
    rerank_provider TEXT NOT NULL DEFAULT '',
    title_weight REAL NOT NULL DEFAULT 0.2,
    augmentation_enabled INTEGER NOT NULL DEFAULT 1,
    deleted_at TEXT -- 软删除时间戳，NULL 表示未删除
);

CREATE INDEX IF NOT EXISTS idx_kb_libraries_name ON kb_libraries(name);

-- KB 文件夹
CREATE TABLE IF NOT EXISTS kb_folders (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    library_id INTEGER NOT NULL REFERENCES kb_libraries(id) ON DELETE CASCADE,
    parent_id INTEGER REFERENCES kb_folders(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    sort_order INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_kb_folders_library_id ON kb_folders(library_id);
CREATE INDEX IF NOT EXISTS idx_kb_folders_parent_id ON kb_folders(parent_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_kb_folders_library_parent_name
    ON kb_folders(library_id, COALESCE(parent_id, -1), name);

-- KB 文档
-- 设计说明：该表包含 40+ 列，涵盖基础信息、处理状态、解析元数据、版本控制、OCR 等多个维度。
-- 当前采用宽表设计以简化单表查询和避免 JOIN 开销。随着字段持续增长，未来可考虑：
-- 1) 将 OCR 相关字段拆分为 kb_document_ocr_meta 子表；
-- 2) 将版本/软删字段拆分为 kb_document_lifecycle 子表；
-- 3) 将解析/嵌入状态字段拆分为 kb_document_processing 子表。
CREATE TABLE IF NOT EXISTS kb_documents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    library_id INTEGER NOT NULL REFERENCES kb_libraries(id) ON DELETE CASCADE,
    folder_id INTEGER REFERENCES kb_folders(id) ON DELETE SET NULL,
    original_name TEXT NOT NULL,
    name_tokens TEXT NOT NULL DEFAULT '',
    file_size INTEGER NOT NULL DEFAULT 0,
    content_hash TEXT NOT NULL,
    extension TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    source_type TEXT NOT NULL DEFAULT 'local',
    storage_path TEXT NOT NULL DEFAULT '',
    original_path TEXT NOT NULL DEFAULT '',
    job_id TEXT NOT NULL DEFAULT '',
    processing_run_id TEXT NOT NULL DEFAULT '',
    parsing_status INTEGER NOT NULL DEFAULT 0,
    parsing_progress INTEGER NOT NULL DEFAULT 0,
    parsing_error TEXT NOT NULL DEFAULT '',
    embedding_status INTEGER NOT NULL DEFAULT 0,
    embedding_progress INTEGER NOT NULL DEFAULT 0,
    embedding_error TEXT NOT NULL DEFAULT '',
    word_total INTEGER NOT NULL DEFAULT 0,
    split_total INTEGER NOT NULL DEFAULT 0,
    title TEXT NOT NULL DEFAULT '',
    summary TEXT NOT NULL DEFAULT '',
    keywords TEXT NOT NULL DEFAULT '',
    entity_names TEXT NOT NULL DEFAULT '',
    source_uri TEXT NOT NULL DEFAULT '',
    modified_at TEXT,
    document_date TEXT,
    normalized_content_hash TEXT NOT NULL DEFAULT '',
    simhash TEXT NOT NULL DEFAULT '',
    document_family_id TEXT,
    version_label TEXT NOT NULL DEFAULT '',
    document_granularity TEXT NOT NULL DEFAULT 'micro',
    content_char_count INTEGER NOT NULL DEFAULT 0,
    content_token_count INTEGER NOT NULL DEFAULT 0,
    page_count INTEGER NOT NULL DEFAULT 0,
    section_count INTEGER NOT NULL DEFAULT 0,
    chunk_strategy TEXT NOT NULL DEFAULT 'auto',
    document_status TEXT NOT NULL DEFAULT 'queued',
    index_status TEXT NOT NULL DEFAULT 'pending',
    current_version_id INTEGER,
    deleted_at TEXT,
    purged_at TEXT,
    last_indexed_at TEXT,
    last_seen_at TEXT,
    ocr_status TEXT NOT NULL DEFAULT 'not_evaluated',
    ocr_text_coverage REAL NOT NULL DEFAULT 0.0,
    metadata_json TEXT NOT NULL DEFAULT '{}'
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_kb_docs_library_hash
    ON kb_documents(library_id, content_hash) WHERE deleted_at IS NULL AND purged_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_kb_docs_library_id ON kb_documents(library_id);
CREATE INDEX IF NOT EXISTS idx_kb_docs_library_id_id ON kb_documents(library_id, id);
CREATE INDEX IF NOT EXISTS idx_kb_docs_document_status ON kb_documents(document_status);
CREATE INDEX IF NOT EXISTS idx_kb_docs_deleted_at ON kb_documents(deleted_at);
CREATE INDEX IF NOT EXISTS idx_kb_docs_granularity ON kb_documents(document_granularity);

-- KB 文档节点
CREATE TABLE IF NOT EXISTS kb_document_nodes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    library_id INTEGER NOT NULL REFERENCES kb_libraries(id) ON DELETE CASCADE,
    document_id INTEGER NOT NULL REFERENCES kb_documents(id) ON DELETE CASCADE,
    -- P1-1: 节点所属版本。NULL 表示在 active-version 改造前已存在的遗留节点,
    -- 检索阶段会回退到"无版本约束"或通过 migration 回填到 baseline version。
    version_id INTEGER REFERENCES kb_document_versions(id) ON DELETE SET NULL,
    -- P1-1: 节点退役时间。active version 切换时旧节点标记 retired_at,
    -- 由后续 cleanup job 异步清理 vec/embedding/passage 引用。
    retired_at TEXT,
    content TEXT NOT NULL,
    content_tokens TEXT NOT NULL DEFAULT '',
    level INTEGER NOT NULL DEFAULT 0,
    parent_id INTEGER REFERENCES kb_document_nodes(id) ON DELETE SET NULL,
    chunk_order INTEGER NOT NULL DEFAULT 0,
    section_id INTEGER,
    title_path TEXT NOT NULL DEFAULT '',
    page_number INTEGER,
    source_start INTEGER,
    source_end INTEGER,
    node_metadata TEXT NOT NULL DEFAULT '{}',
    embedding_text TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_kb_nodes_library_id ON kb_document_nodes(library_id);
CREATE INDEX IF NOT EXISTS idx_kb_nodes_document_id ON kb_document_nodes(document_id);
CREATE INDEX IF NOT EXISTS idx_kb_nodes_parent_id ON kb_document_nodes(parent_id);
CREATE INDEX IF NOT EXISTS idx_kb_nodes_level ON kb_document_nodes(level);
CREATE INDEX IF NOT EXISTS idx_kb_nodes_doc_level_order
    ON kb_document_nodes(document_id, level, chunk_order);
CREATE INDEX IF NOT EXISTS idx_kb_nodes_section_id ON kb_document_nodes(section_id);
CREATE INDEX IF NOT EXISTS idx_kb_nodes_page_number ON kb_document_nodes(page_number);
-- P1-1: 检索阶段按 (document_id, version_id) 索引 active version 节点
CREATE INDEX IF NOT EXISTS idx_kb_nodes_doc_version
    ON kb_document_nodes(document_id, version_id, level, chunk_order);

-- KB Embedding 索引注册表
CREATE TABLE IF NOT EXISTS kb_embedding_indexes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    library_id INTEGER NOT NULL REFERENCES kb_libraries(id) ON DELETE CASCADE,
    provider TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL,
    dimensions INTEGER NOT NULL,
    index_type TEXT NOT NULL DEFAULT 'vec0',
    is_active INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_kb_emb_idx_library ON kb_embedding_indexes(library_id);

-- KB 节点 Embedding（复合主键，支持多索引）
CREATE TABLE IF NOT EXISTS kb_node_embeddings (
    node_id INTEGER NOT NULL REFERENCES kb_document_nodes(id) ON DELETE CASCADE,
    embedding BLOB NOT NULL,
    dimensions INTEGER NOT NULL,
    model TEXT NOT NULL DEFAULT 'text-embedding-3-large',
    embedded_at TEXT NOT NULL DEFAULT (datetime('now')),
    embedding_index_id INTEGER NOT NULL REFERENCES kb_embedding_indexes(id) ON DELETE CASCADE,
    PRIMARY KEY (node_id, embedding_index_id)
);

CREATE INDEX IF NOT EXISTS idx_kb_node_emb_index_id ON kb_node_embeddings(embedding_index_id);

-- KB 文档版本
-- P1-1: 升级为 active index version。每个版本有完整生命周期:
-- pending -> building -> ready(activated) -> retired。
-- 检索阶段只读 status=ready 且为文档 current_version_id 的版本节点。
CREATE TABLE IF NOT EXISTS kb_document_versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    document_id INTEGER NOT NULL REFERENCES kb_documents(id) ON DELETE CASCADE,
    version_label TEXT NOT NULL DEFAULT '',
    processing_run_id TEXT NOT NULL DEFAULT '',
    char_count INTEGER NOT NULL DEFAULT 0,
    node_count INTEGER NOT NULL DEFAULT 0,
    index_status TEXT NOT NULL DEFAULT 'pending',
    -- P1-1: 内容指纹,用于幂等去重(同一文档相同 hash 不重建)
    content_hash TEXT NOT NULL DEFAULT '',
    -- P1-1: 版本激活时间(状态变为 ready 时写入)
    activated_at TEXT,
    -- P1-1: 版本退役时间(被新版本替代时写入)
    retired_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_kb_doc_versions_doc ON kb_document_versions(document_id);
-- P1-1: 检索阶段按 status 索引,快速过滤 ready 版本
CREATE INDEX IF NOT EXISTS idx_kb_doc_versions_status
    ON kb_document_versions(document_id, index_status);

-- KB 文档章节
CREATE TABLE IF NOT EXISTS kb_document_sections (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    document_id INTEGER NOT NULL REFERENCES kb_documents(id) ON DELETE CASCADE,
    parent_section_id INTEGER REFERENCES kb_document_sections(id) ON DELETE SET NULL,
    title TEXT NOT NULL DEFAULT '',
    title_path TEXT NOT NULL DEFAULT '',
    heading_level INTEGER NOT NULL DEFAULT 0,
    section_order INTEGER NOT NULL DEFAULT 0,
    page_number INTEGER,
    source_start INTEGER,
    source_end INTEGER,
    content_summary TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_kb_sections_doc ON kb_document_sections(document_id);
CREATE INDEX IF NOT EXISTS idx_kb_sections_parent ON kb_document_sections(parent_section_id);

-- KB 文档摘要
CREATE TABLE IF NOT EXISTS kb_document_summaries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    document_id INTEGER NOT NULL REFERENCES kb_documents(id) ON DELETE CASCADE,
    section_id INTEGER REFERENCES kb_document_sections(id) ON DELETE CASCADE,
    summary_type TEXT NOT NULL DEFAULT 'document',
    summary_text TEXT NOT NULL DEFAULT '',
    summary_tokens TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT '',
    UNIQUE(document_id, section_id, summary_type)
);
CREATE INDEX IF NOT EXISTS idx_kb_summaries_doc ON kb_document_summaries(document_id);

-- KB 搜索评测查询集
CREATE TABLE IF NOT EXISTS kb_search_eval_queries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    library_id INTEGER REFERENCES kb_libraries(id) ON DELETE CASCADE,
    query_text TEXT NOT NULL,
    query_type TEXT NOT NULL DEFAULT 'manual',
    expected_document_ids TEXT NOT NULL DEFAULT '[]',
    notes TEXT NOT NULL DEFAULT ''
);

-- KB 索引状态表
CREATE TABLE IF NOT EXISTS kb_index_state (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    index_name TEXT NOT NULL UNIQUE,
    index_version INTEGER NOT NULL DEFAULT 1,
    index_type TEXT NOT NULL DEFAULT 'vector',
    dimensions INTEGER,
    model TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'active',
    doc_count INTEGER NOT NULL DEFAULT 0,
    last_rebuilt_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- KB 搜索日志
CREATE TABLE IF NOT EXISTS kb_search_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    query_normalized TEXT NOT NULL DEFAULT '',
    library_ids TEXT NOT NULL DEFAULT '[]',
    profile TEXT NOT NULL DEFAULT '',
    planner_type TEXT NOT NULL DEFAULT '',
    result_count INTEGER NOT NULL DEFAULT 0,
    latency_ms INTEGER NOT NULL DEFAULT 0,
    cache_hit INTEGER NOT NULL DEFAULT 0,
    debug_mode INTEGER NOT NULL DEFAULT 0,
    embedding_index_id INTEGER,
    result_document_ids TEXT NOT NULL DEFAULT '[]'
);

-- KB 搜索反馈
CREATE TABLE IF NOT EXISTS kb_search_feedback (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    search_log_id INTEGER REFERENCES kb_search_logs(id) ON DELETE SET NULL,
    document_id INTEGER REFERENCES kb_documents(id) ON DELETE SET NULL,
    node_id INTEGER REFERENCES kb_document_nodes(id) ON DELETE SET NULL,
    rating INTEGER NOT NULL DEFAULT 0,
    comment TEXT NOT NULL DEFAULT ''
);

-- KB 表格索引
-- P1 修复：增加 version_id 关联 active version，使表格索引与节点版本原子绑定
CREATE TABLE IF NOT EXISTS kb_tables (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    document_id INTEGER NOT NULL REFERENCES kb_documents(id) ON DELETE CASCADE,
    version_id INTEGER REFERENCES kb_document_versions(id) ON DELETE SET NULL,
    sheet_name TEXT NOT NULL DEFAULT '',
    headers TEXT NOT NULL DEFAULT '[]',
    column_count INTEGER NOT NULL DEFAULT 0,
    row_count INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_kb_tables_doc ON kb_tables(document_id);
CREATE INDEX IF NOT EXISTS idx_kb_tables_version ON kb_tables(version_id);

-- KB 表格行
CREATE TABLE IF NOT EXISTS kb_table_rows (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    table_id INTEGER NOT NULL REFERENCES kb_tables(id) ON DELETE CASCADE,
    row_index INTEGER NOT NULL DEFAULT 0,
    row_text TEXT NOT NULL DEFAULT '',
    row_tokens TEXT NOT NULL DEFAULT '',
    row_json TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_kb_table_rows_table ON kb_table_rows(table_id);

-- KB 表格行 FTS5 索引
CREATE VIRTUAL TABLE IF NOT EXISTS kb_table_row_fts USING fts5(
    tokens,
    table_id UNINDEXED,
    document_id UNINDEXED,
    library_id UNINDEXED,
    content='',
    tokenize='unicode61'
);

CREATE TRIGGER IF NOT EXISTS kb_table_row_fts_insert
AFTER INSERT ON kb_table_rows BEGIN
    INSERT INTO kb_table_row_fts(rowid, tokens, table_id, document_id, library_id)
    SELECT new.id, trim(new.row_tokens || ' ' || t.sheet_name || ' ' || t.headers || ' ' || new.row_text), new.table_id, t.document_id, d.library_id
    FROM kb_tables t
    JOIN kb_documents d ON d.id = t.document_id
    WHERE t.id = new.table_id;
END;

CREATE TRIGGER IF NOT EXISTS kb_table_row_fts_update
AFTER UPDATE OF row_tokens, row_text, table_id ON kb_table_rows BEGIN
    INSERT INTO kb_table_row_fts(kb_table_row_fts, rowid, tokens, table_id, document_id, library_id)
    SELECT 'delete', old.id, trim(old.row_tokens || ' ' || t.sheet_name || ' ' || t.headers || ' ' || old.row_text), old.table_id, t.document_id, d.library_id
    FROM kb_tables t
    JOIN kb_documents d ON d.id = t.document_id
    WHERE t.id = old.table_id;
    INSERT INTO kb_table_row_fts(rowid, tokens, table_id, document_id, library_id)
    SELECT new.id, trim(new.row_tokens || ' ' || t.sheet_name || ' ' || t.headers || ' ' || new.row_text), new.table_id, t.document_id, d.library_id
    FROM kb_tables t
    JOIN kb_documents d ON d.id = t.document_id
    WHERE t.id = new.table_id;
END;

CREATE TRIGGER IF NOT EXISTS kb_table_row_fts_delete
AFTER DELETE ON kb_table_rows BEGIN
    INSERT INTO kb_table_row_fts(kb_table_row_fts, rowid, tokens, table_id, document_id, library_id)
    SELECT 'delete', old.id, trim(old.row_tokens || ' ' || t.sheet_name || ' ' || t.headers || ' ' || old.row_text), old.table_id, t.document_id, d.library_id
    FROM kb_tables t
    JOIN kb_documents d ON d.id = t.document_id
    WHERE t.id = old.table_id;
END;

-- KB 外部模型调用审计
CREATE TABLE IF NOT EXISTS kb_external_model_calls (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    library_id INTEGER REFERENCES kb_libraries(id) ON DELETE SET NULL,
    document_id INTEGER REFERENCES kb_documents(id) ON DELETE SET NULL,
    call_type TEXT NOT NULL DEFAULT '',
    provider TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT '',
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    latency_ms INTEGER NOT NULL DEFAULT 0,
    cost_estimate REAL NOT NULL DEFAULT 0.0,
    success INTEGER NOT NULL DEFAULT 1,
    error_message TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_kb_ext_calls_library ON kb_external_model_calls(library_id);

-- KB 导入源
CREATE TABLE IF NOT EXISTS kb_sources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    library_id INTEGER NOT NULL REFERENCES kb_libraries(id) ON DELETE CASCADE,
    source_type TEXT NOT NULL DEFAULT 'local',
    source_uri TEXT NOT NULL DEFAULT '',
    display_name TEXT NOT NULL DEFAULT '',
    connector_config TEXT NOT NULL DEFAULT '{}',
    delete_policy TEXT NOT NULL DEFAULT 'mark_only',
    sync_status TEXT NOT NULL DEFAULT 'idle',
    last_synced_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_kb_sources_library ON kb_sources(library_id);

-- KB 导入源条目
CREATE TABLE IF NOT EXISTS kb_source_items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    source_id INTEGER NOT NULL REFERENCES kb_sources(id) ON DELETE CASCADE,
    document_id INTEGER REFERENCES kb_documents(id) ON DELETE SET NULL,
    external_id TEXT NOT NULL DEFAULT '',
    item_path TEXT NOT NULL DEFAULT '',
    content_hash TEXT NOT NULL DEFAULT '',
    file_size INTEGER NOT NULL DEFAULT 0,
    last_seen_at TEXT,
    sync_status TEXT NOT NULL DEFAULT 'pending',
    sync_error TEXT NOT NULL DEFAULT '',
    UNIQUE(source_id, item_path)
);
CREATE INDEX IF NOT EXISTS idx_kb_source_items_source ON kb_source_items(source_id);

-- P2-1: KB 媒体资产表
-- 用于 OCR 文本/caption/图片引用保真,后续可在此基础上接入 CLIP 跨模态向量。
-- P1-2: 增加 version_id 关联 active version,支持版本切换时同步清理;
--       sort_order 保留原文中的出现顺序;mime_type/byte_size 为预留字段。
CREATE TABLE IF NOT EXISTS kb_media_assets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    library_id INTEGER NOT NULL REFERENCES kb_libraries(id) ON DELETE CASCADE,
    document_id INTEGER NOT NULL REFERENCES kb_documents(id) ON DELETE CASCADE,
    version_id INTEGER REFERENCES kb_document_versions(id) ON DELETE SET NULL,
    node_id INTEGER REFERENCES kb_document_nodes(id) ON DELETE SET NULL,
    page_number INTEGER,
    media_type TEXT NOT NULL,
    storage_path TEXT NOT NULL,
    alt_text TEXT,
    ocr_text TEXT,
    caption TEXT,
    bbox_json TEXT,
    mime_type TEXT NOT NULL DEFAULT '',
    byte_size INTEGER NOT NULL DEFAULT 0,
    sort_order INTEGER NOT NULL DEFAULT 0,
    metadata_json TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_kb_media_doc ON kb_media_assets(document_id);
CREATE INDEX IF NOT EXISTS idx_kb_media_doc_version ON kb_media_assets(document_id, version_id);
CREATE INDEX IF NOT EXISTS idx_kb_media_node ON kb_media_assets(node_id);
CREATE INDEX IF NOT EXISTS idx_kb_media_lib_type ON kb_media_assets(library_id, media_type);

-- KB 节点 FTS5 全文索引
CREATE VIRTUAL TABLE IF NOT EXISTS kb_doc_fts USING fts5(
    tokens,
    library_id,
    document_id UNINDEXED,
    level UNINDEXED,
    content='',
    tokenize='unicode61'
);

CREATE TRIGGER IF NOT EXISTS kb_nodes_fts_insert
AFTER INSERT ON kb_document_nodes BEGIN
    INSERT INTO kb_doc_fts(rowid, tokens, library_id, document_id, level)
    VALUES (new.id, new.content_tokens, new.library_id, new.document_id, new.level);
END;

CREATE TRIGGER IF NOT EXISTS kb_nodes_fts_update
AFTER UPDATE OF content_tokens, library_id, document_id, level ON kb_document_nodes BEGIN
    INSERT INTO kb_doc_fts(kb_doc_fts, rowid, tokens, library_id, document_id, level)
    VALUES('delete', old.id, old.content_tokens, old.library_id, old.document_id, old.level);
    INSERT INTO kb_doc_fts(rowid, tokens, library_id, document_id, level)
    VALUES (new.id, new.content_tokens, new.library_id, new.document_id, new.level);
END;

CREATE TRIGGER IF NOT EXISTS kb_nodes_fts_delete
AFTER DELETE ON kb_document_nodes BEGIN
    INSERT INTO kb_doc_fts(kb_doc_fts, rowid, tokens, library_id, document_id, level)
    VALUES('delete', old.id, old.content_tokens, old.library_id, old.document_id, old.level);
END;

-- KB passage 多视图索引
-- 这是面向检索可靠性的兜底索引：不依赖标题或文档结构，
-- 从每个 KB node 生成固定滑窗、清洗文本片段和原子段落片段。
CREATE TABLE IF NOT EXISTS kb_passage_spans (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    library_id INTEGER NOT NULL REFERENCES kb_libraries(id) ON DELETE CASCADE,
    document_id INTEGER NOT NULL REFERENCES kb_documents(id) ON DELETE CASCADE,
    node_id INTEGER NOT NULL REFERENCES kb_document_nodes(id) ON DELETE CASCADE,
    view_type TEXT NOT NULL DEFAULT 'window',
    passage_order INTEGER NOT NULL DEFAULT 0,
    source_start INTEGER,
    source_end INTEGER,
    content TEXT NOT NULL,
    content_tokens TEXT NOT NULL DEFAULT '',
    quality_score REAL NOT NULL DEFAULT 1.0,
    metadata_json TEXT NOT NULL DEFAULT '{}',
    UNIQUE(node_id, view_type, passage_order)
);

CREATE INDEX IF NOT EXISTS idx_kb_passage_library_id ON kb_passage_spans(library_id);
CREATE INDEX IF NOT EXISTS idx_kb_passage_document_id ON kb_passage_spans(document_id);
CREATE INDEX IF NOT EXISTS idx_kb_passage_node_id ON kb_passage_spans(node_id);
CREATE INDEX IF NOT EXISTS idx_kb_passage_view_type ON kb_passage_spans(view_type);
CREATE INDEX IF NOT EXISTS idx_kb_passage_doc_order
    ON kb_passage_spans(document_id, node_id, view_type, passage_order);

CREATE VIRTUAL TABLE IF NOT EXISTS kb_passage_fts USING fts5(
    tokens,
    library_id,
    document_id UNINDEXED,
    node_id UNINDEXED,
    view_type UNINDEXED,
    content='',
    tokenize='unicode61'
);

CREATE TRIGGER IF NOT EXISTS kb_passage_fts_insert
AFTER INSERT ON kb_passage_spans BEGIN
    INSERT INTO kb_passage_fts(rowid, tokens, library_id, document_id, node_id, view_type)
    VALUES (new.id, new.content_tokens, new.library_id, new.document_id, new.node_id, new.view_type);
END;

CREATE TRIGGER IF NOT EXISTS kb_passage_fts_update
AFTER UPDATE OF content_tokens, library_id, document_id, node_id, view_type ON kb_passage_spans BEGIN
    INSERT INTO kb_passage_fts(kb_passage_fts, rowid, tokens, library_id, document_id, node_id, view_type)
    VALUES('delete', old.id, old.content_tokens, old.library_id, old.document_id, old.node_id, old.view_type);
    INSERT INTO kb_passage_fts(rowid, tokens, library_id, document_id, node_id, view_type)
    VALUES (new.id, new.content_tokens, new.library_id, new.document_id, new.node_id, new.view_type);
END;

CREATE TRIGGER IF NOT EXISTS kb_passage_fts_delete
AFTER DELETE ON kb_passage_spans BEGIN
    INSERT INTO kb_passage_fts(kb_passage_fts, rowid, tokens, library_id, document_id, node_id, view_type)
    VALUES('delete', old.id, old.content_tokens, old.library_id, old.document_id, old.node_id, old.view_type);
END;

-- KB 文档名称 FTS5 索引
CREATE VIRTUAL TABLE IF NOT EXISTS kb_doc_name_fts USING fts5(
    name_tokens,
    library_id,
    document_id UNINDEXED,
    content='',
    tokenize='unicode61'
);

CREATE TRIGGER IF NOT EXISTS kb_docs_fts_insert
AFTER INSERT ON kb_documents BEGIN
    INSERT INTO kb_doc_name_fts(rowid, name_tokens, library_id, document_id)
    VALUES (new.id, new.name_tokens, new.library_id, new.id);
END;

CREATE TRIGGER IF NOT EXISTS kb_docs_fts_delete
AFTER DELETE ON kb_documents BEGIN
    INSERT INTO kb_doc_name_fts(kb_doc_name_fts, rowid, name_tokens, library_id, document_id)
    VALUES('delete', old.id, old.name_tokens, old.library_id, old.id);
END;

CREATE TRIGGER IF NOT EXISTS kb_docs_fts_update
AFTER UPDATE OF name_tokens, library_id ON kb_documents BEGIN
    INSERT INTO kb_doc_name_fts(kb_doc_name_fts, rowid, name_tokens, library_id, document_id)
    VALUES('delete', old.id, old.name_tokens, old.library_id, old.id);
    INSERT INTO kb_doc_name_fts(rowid, name_tokens, library_id, document_id)
    VALUES (new.id, new.name_tokens, new.library_id, new.id);
END;

-- KB 文档 OCR 页级结果表
CREATE TABLE IF NOT EXISTS kb_document_ocr_pages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    document_id INTEGER NOT NULL REFERENCES kb_documents(id) ON DELETE CASCADE,
    page_number INTEGER NOT NULL,
    processing_run_id TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending',
    provider TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT '',
    text TEXT NOT NULL DEFAULT '',
    markdown TEXT NOT NULL DEFAULT '',
    layout_json TEXT NOT NULL DEFAULT '[]',
    layout_visualization_url TEXT NOT NULL DEFAULT '',
    raw_response_json TEXT NOT NULL DEFAULT '{}',
    request_id TEXT NOT NULL DEFAULT '',
    confidence REAL,
    error TEXT NOT NULL DEFAULT '',
    ocr_page_width INTEGER,
    ocr_page_height INTEGER,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(document_id, page_number, processing_run_id)
);

CREATE INDEX IF NOT EXISTS idx_kb_ocr_pages_document
    ON kb_document_ocr_pages(document_id);

CREATE INDEX IF NOT EXISTS idx_kb_ocr_pages_status
    ON kb_document_ocr_pages(status);

-- KB 文档 OCR 版面块表
CREATE TABLE IF NOT EXISTS kb_document_ocr_blocks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    document_id INTEGER NOT NULL REFERENCES kb_documents(id) ON DELETE CASCADE,
    page_number INTEGER NOT NULL,
    processing_run_id TEXT NOT NULL DEFAULT '',
    block_index INTEGER NOT NULL DEFAULT 0,
    label TEXT NOT NULL DEFAULT '',
    bbox_json TEXT NOT NULL DEFAULT '',
    content TEXT NOT NULL DEFAULT '',
    plain_text TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL DEFAULT 'glm_ocr',
    raw_json TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(document_id, page_number, block_index, source, processing_run_id)
);

CREATE INDEX IF NOT EXISTS idx_kb_ocr_blocks_document_page
    ON kb_document_ocr_blocks(document_id, page_number);

CREATE INDEX IF NOT EXISTS idx_kb_ocr_blocks_label
    ON kb_document_ocr_blocks(label);

-- ═══════════════════════════════════════════════════════════════════════════════
-- Artifact 子系统表（单入口多投影融合架构）
-- ═══════════════════════════════════════════════════════════════════════════════

-- 原件内容对象（按 sha256 去重）
CREATE TABLE IF NOT EXISTS source_artifacts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    artifact_uid TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_seen_at TEXT,

    sha256 TEXT NOT NULL,
    original_name TEXT NOT NULL DEFAULT '',
    extension TEXT NOT NULL DEFAULT '',
    mime_type TEXT NOT NULL DEFAULT '',
    size_bytes INTEGER NOT NULL DEFAULT 0,

    storage_path TEXT NOT NULL DEFAULT '',
    canonical_slug TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'active',
    metadata_json TEXT NOT NULL DEFAULT '{}',

    deleted_at TEXT,
    purged_at TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_source_artifacts_sha256
    ON source_artifacts(sha256) WHERE purged_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_source_artifacts_slug ON source_artifacts(canonical_slug);
CREATE INDEX IF NOT EXISTS idx_source_artifacts_status ON source_artifacts(status);

-- 上传/同步/关联事件
CREATE TABLE IF NOT EXISTS artifact_occurrences (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    occurrence_uid TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),

    artifact_id INTEGER NOT NULL REFERENCES source_artifacts(id) ON DELETE CASCADE,
    source_kind TEXT NOT NULL DEFAULT 'upload',
    source_uri TEXT NOT NULL DEFAULT '',
    original_path TEXT NOT NULL DEFAULT '',
    original_name TEXT NOT NULL DEFAULT '',
    owner_ref TEXT NOT NULL DEFAULT '',

    intent TEXT NOT NULL DEFAULT 'auto',
    target_slug TEXT NOT NULL DEFAULT '',
    page_slug TEXT NOT NULL DEFAULT '',
    library_id INTEGER,
    folder_id INTEGER,
    promotion_policy TEXT NOT NULL DEFAULT 'auto_apply',

    status TEXT NOT NULL DEFAULT 'active',
    stale_reason TEXT NOT NULL DEFAULT '',
    metadata_json TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_artifact_occ_artifact ON artifact_occurrences(artifact_id);
CREATE INDEX IF NOT EXISTS idx_artifact_occ_source ON artifact_occurrences(source_kind, source_uri);
CREATE INDEX IF NOT EXISTS idx_artifact_occ_target ON artifact_occurrences(target_slug);
CREATE INDEX IF NOT EXISTS idx_artifact_occ_page ON artifact_occurrences(page_slug);

-- 投影映射
CREATE TABLE IF NOT EXISTS artifact_projections (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),

    artifact_id INTEGER NOT NULL REFERENCES source_artifacts(id) ON DELETE CASCADE,
    occurrence_id INTEGER REFERENCES artifact_occurrences(id) ON DELETE SET NULL,
    projection_type TEXT NOT NULL,
    projection_key TEXT NOT NULL DEFAULT '',
    projection_ref TEXT NOT NULL DEFAULT '',

    status TEXT NOT NULL DEFAULT 'active',
    version_hash TEXT NOT NULL DEFAULT '',
    stale_reason TEXT NOT NULL DEFAULT '',
    metadata_json TEXT NOT NULL DEFAULT '{}',
    superseded_by INTEGER REFERENCES artifact_projections(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_artifact_proj_artifact ON artifact_projections(artifact_id);
CREATE INDEX IF NOT EXISTS idx_artifact_proj_occurrence ON artifact_projections(occurrence_id);
CREATE INDEX IF NOT EXISTS idx_artifact_proj_type ON artifact_projections(projection_type);
CREATE INDEX IF NOT EXISTS idx_artifact_proj_ref ON artifact_projections(projection_ref);
CREATE INDEX IF NOT EXISTS idx_artifact_proj_superseded ON artifact_projections(superseded_by);
CREATE UNIQUE INDEX IF NOT EXISTS idx_artifact_proj_active_unique
    ON artifact_projections(artifact_id, projection_type, projection_key)
    WHERE status = 'active';

-- 候选变更
CREATE TABLE IF NOT EXISTS promotion_candidates (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),

    artifact_id INTEGER NOT NULL REFERENCES source_artifacts(id) ON DELETE CASCADE,
    occurrence_id INTEGER REFERENCES artifact_occurrences(id) ON DELETE SET NULL,
    kb_document_id INTEGER REFERENCES kb_documents(id) ON DELETE SET NULL,
    kb_node_id INTEGER REFERENCES kb_document_nodes(id) ON DELETE SET NULL,

    candidate_type TEXT NOT NULL,
    target_slug TEXT NOT NULL DEFAULT '',
    target_field TEXT NOT NULL DEFAULT '',

    title TEXT NOT NULL DEFAULT '',
    proposed_payload TEXT NOT NULL DEFAULT '{}',
    evidence_json TEXT NOT NULL DEFAULT '{}',

    confidence REAL NOT NULL DEFAULT 0.0,
    risk_level TEXT NOT NULL DEFAULT 'medium',
    status TEXT NOT NULL DEFAULT 'pending',
    reviewer TEXT NOT NULL DEFAULT '',
    review_notes TEXT NOT NULL DEFAULT '',
    applied_at TEXT,
    candidate_fingerprint TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_promo_candidates_artifact ON promotion_candidates(artifact_id);
CREATE INDEX IF NOT EXISTS idx_promo_candidates_occurrence ON promotion_candidates(occurrence_id);
CREATE INDEX IF NOT EXISTS idx_promo_candidates_doc ON promotion_candidates(kb_document_id);
CREATE INDEX IF NOT EXISTS idx_promo_candidates_status ON promotion_candidates(status);
CREATE INDEX IF NOT EXISTS idx_promo_candidates_target ON promotion_candidates(target_slug);
CREATE UNIQUE INDEX IF NOT EXISTS idx_promo_candidates_fingerprint
    ON promotion_candidates(candidate_fingerprint)
    WHERE candidate_fingerprint != '' AND status IN ('pending', 'accepted', 'applied');

-- 来源追溯
CREATE TABLE IF NOT EXISTS provenance_ledger (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),

    artifact_id INTEGER REFERENCES source_artifacts(id) ON DELETE SET NULL,
    occurrence_id INTEGER REFERENCES artifact_occurrences(id) ON DELETE SET NULL,
    kb_document_id INTEGER REFERENCES kb_documents(id) ON DELETE SET NULL,
    kb_node_id INTEGER REFERENCES kb_document_nodes(id) ON DELETE SET NULL,
    promotion_candidate_id INTEGER REFERENCES promotion_candidates(id) ON DELETE SET NULL,

    brain_slug TEXT NOT NULL DEFAULT '',
    brain_field TEXT NOT NULL DEFAULT '',
    fact_hash TEXT NOT NULL DEFAULT '',

    quote_text TEXT NOT NULL DEFAULT '',
    quote_start INTEGER,
    quote_end INTEGER,
    page_number INTEGER,

    confidence REAL NOT NULL DEFAULT 0.0,
    status TEXT NOT NULL DEFAULT 'active',
    stale_reason TEXT NOT NULL DEFAULT '',
    metadata_json TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_provenance_artifact ON provenance_ledger(artifact_id);
CREATE INDEX IF NOT EXISTS idx_provenance_occurrence ON provenance_ledger(occurrence_id);
CREATE INDEX IF NOT EXISTS idx_provenance_kb_doc ON provenance_ledger(kb_document_id);
CREATE INDEX IF NOT EXISTS idx_provenance_kb_node ON provenance_ledger(kb_node_id);
CREATE INDEX IF NOT EXISTS idx_provenance_brain_slug ON provenance_ledger(brain_slug);
CREATE INDEX IF NOT EXISTS idx_provenance_fact_hash ON provenance_ledger(fact_hash);

-- Artifact 审计事件
CREATE TABLE IF NOT EXISTS artifact_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    artifact_id INTEGER REFERENCES source_artifacts(id) ON DELETE SET NULL,
    occurrence_id INTEGER REFERENCES artifact_occurrences(id) ON DELETE SET NULL,
    event_type TEXT NOT NULL,
    actor TEXT NOT NULL DEFAULT '',
    payload_json TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_artifact_events_artifact ON artifact_events(artifact_id);
CREATE INDEX IF NOT EXISTS idx_artifact_events_occurrence ON artifact_events(occurrence_id);
CREATE INDEX IF NOT EXISTS idx_artifact_events_type ON artifact_events(event_type);

-- Token 语义向量（离线挖掘用）
CREATE TABLE IF NOT EXISTS kb_token_embeddings (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    token               TEXT    NOT NULL,
    embedding_index_id  INTEGER NOT NULL,
    embedding           BLOB    NOT NULL,
    doc_freq            INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT    NOT NULL DEFAULT (datetime('now')),
    UNIQUE(token, embedding_index_id),
    FOREIGN KEY (embedding_index_id)
        REFERENCES kb_embedding_indexes(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_token_emb_idx
    ON kb_token_embeddings(embedding_index_id);

-- Token 同义词（运行时查询路径唯一被查的表）
CREATE TABLE IF NOT EXISTS kb_token_synonyms (
    token               TEXT    NOT NULL,
    synonym             TEXT    NOT NULL,
    score               REAL    NOT NULL,
    embedding_index_id  INTEGER NOT NULL,
    PRIMARY KEY (token, synonym, embedding_index_id),
    FOREIGN KEY (embedding_index_id)
        REFERENCES kb_embedding_indexes(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_token_syn_lookup
    ON kb_token_synonyms(embedding_index_id, token, score DESC);
"#;

/// Generate sqlite-vec virtual table DDL
/// Returns DDL for the vec_chunks virtual table
///
/// 声明 `distance_metric=cosine`，确保 distance = 1 - cosine_similarity，
/// 使检索侧 `1.0 - distance` 的语义正确。
pub fn vec_chunks_ddl(dimensions: usize) -> String {
    format!(
        r#"CREATE VIRTUAL TABLE IF NOT EXISTS vec_chunks USING vec0(
    chunk_id INTEGER PRIMARY KEY,
    embedding float[{}] distance_metric=cosine
);"#,
        dimensions
    )
}

/// 生成 sqlite-vec KB 节点虚拟表 DDL。
///
/// 显式声明 `distance_metric=cosine`，确保 vec0 返回的距离为
/// cosine distance（即 distance = 1 - cosine_similarity）。
/// sqlite-vec float 类型的默认距离度量是 L2，必须显式声明 cosine
/// 才能使检索侧 `similarity = 1.0 - distance` 的语义正确。
/// 参见: https://alexgarcia.xyz/sqlite-vec/api-reference/vec0.html
pub fn vec_kb_nodes_ddl(dimensions: usize) -> String {
    format!(
        r#"CREATE VIRTUAL TABLE IF NOT EXISTS vec_kb_nodes USING vec0(
    node_id INTEGER PRIMARY KEY,
    embedding float[{}] distance_metric=cosine
);"#,
        dimensions
    )
}
