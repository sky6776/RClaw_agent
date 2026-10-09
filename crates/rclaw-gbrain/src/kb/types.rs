//! KB data model types

use crate::error::GBrainError;
use serde::{Deserialize, Serialize};

// --- Status constants ---
pub const STATUS_PENDING: i32 = 0;
pub const STATUS_PROCESSING: i32 = 1;
pub const STATUS_COMPLETED: i32 = 2;
pub const STATUS_FAILED: i32 = 3;
pub const STATUS_SKIPPED: i32 = 4;

/// 扩展名对应的 MIME 类型映射（格式映射，非安全校验）
/// 安全校验的允许扩展名列表由 Config.kb_allowed_extensions 控制。
///
/// L6: ext.to_lowercase() 每次调用分配新 String。
/// 返回值为 &'static str 无法避免输入分配（除非改为接受 &str 要求调用方预转小写），
/// 调用频率低，暂不优化。
pub fn mime_type_for_ext(ext: &str) -> &'static str {
    // M44 修复：统一 MIME 映射，覆盖所有 KB 和 artifact 支持的格式
    match ext.to_lowercase().as_str() {
        // 文档
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        // 文本
        "txt" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "rst" => "text/x-rst",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "html" | "htm" => "text/html",
        // 数据/配置
        "json" => "application/json",
        "xml" => "application/xml",
        // 非 IANA 标准类型，前端/客户端依赖此类型做格式判断
        "yaml" | "yml" => "application/x-yaml",
        "toml" => "application/toml",
        // 图片
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

// --- Library model ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Library {
    pub id: i64,
    pub created_at: String,
    pub updated_at: String,
    pub name: String,
    pub raptor_enabled: bool,
    pub raptor_llm_base_url: String,
    #[serde(skip_serializing)]
    pub raptor_llm_secret_ref: String,
    pub raptor_llm_model: String,
    /// 切片最大长度（单位：字符数）。默认 512，范围 [200, 5000]。
    pub chunk_size: usize,
    /// 切片重叠长度（单位：字符数）。默认 50，范围 [0, 1000]。
    pub chunk_overlap: usize,
    pub batch_max_documents: usize,
    pub batch_max_chunks: usize,
    pub sort_order: i32,
    // P0-016: 库级治理和模型配置
    pub embedding_provider: String,
    pub embedding_model: String,
    pub embedding_dimensions: Option<i32>,
    pub search_profile: String,
    pub rerank_enabled: bool,
    pub rerank_provider: String,
    /// 标题/文件名在 embedding 文本中的权重 (0.0-1.0)。默认 0.2。
    /// 权重越高，标题重复次数越多，文档级检索越准确。
    pub title_weight: f32,
    /// 是否启用自动关键词和问题生成（分块后 LLM 增强）。
    pub augmentation_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateLibraryInput {
    pub name: String,
    #[serde(default)]
    pub raptor_enabled: Option<bool>,
    #[serde(default)]
    pub raptor_llm_base_url: Option<String>,
    #[serde(default)]
    pub raptor_llm_secret_ref: Option<String>,
    #[serde(default)]
    pub raptor_llm_model: Option<String>,
    #[serde(default)]
    pub chunk_size: Option<usize>,
    #[serde(default)]
    pub chunk_overlap: Option<usize>,
    #[serde(default)]
    pub batch_max_documents: Option<usize>,
    #[serde(default)]
    pub batch_max_chunks: Option<usize>,
    // P0-016: 库级治理和模型配置
    #[serde(default)]
    pub embedding_provider: Option<String>,
    #[serde(default)]
    pub embedding_model: Option<String>,
    #[serde(default)]
    pub embedding_dimensions: Option<i32>,
    #[serde(default)]
    pub search_profile: Option<String>,
    #[serde(default)]
    pub rerank_enabled: Option<bool>,
    #[serde(default)]
    pub rerank_provider: Option<String>,
    #[serde(default)]
    pub title_weight: Option<f32>,
    #[serde(default)]
    pub augmentation_enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateLibraryInput {
    pub name: Option<String>,
    pub raptor_enabled: Option<bool>,
    pub raptor_llm_base_url: Option<String>,
    pub raptor_llm_secret_ref: Option<String>,
    pub raptor_llm_model: Option<String>,
    pub chunk_size: Option<usize>,
    pub chunk_overlap: Option<usize>,
    // P0-016: 库级治理和模型配置
    pub embedding_provider: Option<String>,
    pub embedding_model: Option<String>,
    pub embedding_dimensions: Option<i32>,
    pub search_profile: Option<String>,
    pub rerank_enabled: Option<bool>,
    pub rerank_provider: Option<String>,
    pub title_weight: Option<f32>,
    pub augmentation_enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryListItem {
    pub id: i64,
    pub name: String,
    pub document_count: i64,
    pub chunk_count: i64,
    pub sort_order: i32,
    pub raptor_enabled: bool,
    pub has_raptor_secret: bool,
}

// --- Folder model ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    pub id: i64,
    pub created_at: String,
    pub updated_at: String,
    pub library_id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub sort_order: i32,
    #[serde(default)]
    pub children: Vec<Folder>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFolderInput {
    pub library_id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderStats {
    pub folder_id: i64,
    pub doc_count: i64,
    pub latest_doc_updated_at: Option<String>,
}

// --- Document model ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: i64,
    pub created_at: String,
    pub updated_at: String,
    pub library_id: i64,
    pub folder_id: Option<i64>,
    pub original_name: String,
    pub name_tokens: String,
    pub file_size: i64,
    pub content_hash: String,
    pub extension: String,
    pub mime_type: String,
    pub source_type: String,
    pub storage_path: String,
    pub original_path: String,
    pub job_id: String,
    pub processing_run_id: String,
    pub parsing_status: i32,
    pub parsing_progress: i32,
    pub parsing_error: String,
    pub embedding_status: i32,
    pub embedding_progress: i32,
    pub embedding_error: String,
    pub word_total: i32,
    pub split_total: i32,
    // P0-010: 扩展文档元数据字段
    pub title: String,
    pub summary: String,
    pub keywords: String,
    pub entity_names: String,
    pub source_uri: String,
    pub modified_at: Option<String>,
    pub document_date: Option<String>,
    pub normalized_content_hash: String,
    pub simhash: String,
    pub document_family_id: Option<String>,
    pub version_label: String,
    pub document_granularity: String,
    pub content_char_count: i32,
    pub content_token_count: i32,
    pub page_count: i32,
    pub section_count: i32,
    pub chunk_strategy: String,
    pub document_status: String,
    pub index_status: String,
    pub current_version_id: Option<i64>,
    pub deleted_at: Option<String>,
    pub purged_at: Option<String>,
    pub last_indexed_at: Option<String>,
    pub last_seen_at: Option<String>,
    // P2-019: OCR 回写状态
    pub ocr_status: String,
    pub ocr_text_coverage: f64,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            id: 0,
            created_at: String::new(),
            updated_at: String::new(),
            library_id: 0,
            folder_id: None,
            original_name: String::new(),
            name_tokens: String::new(),
            file_size: 0,
            content_hash: String::new(),
            extension: String::new(),
            mime_type: String::new(),
            source_type: "local".to_string(),
            storage_path: String::new(),
            original_path: String::new(),
            job_id: String::new(),
            processing_run_id: String::new(),
            parsing_status: STATUS_PENDING,
            parsing_progress: 0,
            parsing_error: String::new(),
            embedding_status: STATUS_PENDING,
            embedding_progress: 0,
            embedding_error: String::new(),
            word_total: 0,
            split_total: 0,
            // P0-010: 新字段默认值
            title: String::new(),
            summary: String::new(),
            keywords: String::new(),
            entity_names: String::new(),
            source_uri: String::new(),
            modified_at: None,
            document_date: None,
            normalized_content_hash: String::new(),
            simhash: String::new(),
            document_family_id: None,
            version_label: String::new(),
            document_granularity: "micro".to_string(),
            content_char_count: 0,
            content_token_count: 0,
            page_count: 0,
            section_count: 0,
            chunk_strategy: "auto".to_string(),
            document_status: "queued".to_string(),
            index_status: "pending".to_string(),
            current_version_id: None,
            deleted_at: None,
            purged_at: None,
            last_indexed_at: None,
            last_seen_at: None,
            ocr_status: "not_evaluated".to_string(),
            ocr_text_coverage: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentListItem {
    pub id: i64,
    pub original_name: String,
    pub extension: String,
    pub file_size: i64,
    pub parsing_status: i32,
    pub parsing_progress: i32,
    pub embedding_status: i32,
    pub embedding_progress: i32,
    pub job_id: String,
    pub folder_id: Option<i64>,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_granularity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadDocumentResponse {
    pub document_id: i64,
    pub job_id: String,
    pub processing_run_id: String,
    pub status: String,
    pub duplicate_of: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentStatus {
    pub document_id: i64,
    pub job_id: String,
    pub processing_run_id: String,
    pub parsing_status: i32,
    pub parsing_progress: i32,
    pub parsing_error: String,
    pub embedding_status: i32,
    pub embedding_progress: i32,
    pub embedding_error: String,
    pub word_total: i32,
    pub split_total: i32,
}

// --- DocumentNode model ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentNode {
    pub id: i64,
    pub created_at: String,
    pub updated_at: String,
    pub library_id: i64,
    pub document_id: i64,
    pub content: String,
    pub content_tokens: String,
    pub level: i32,
    pub parent_id: Option<i64>,
    pub chunk_order: i32,
    // P0-011: 扩展节点元数据字段
    pub section_id: Option<i64>,
    pub title_path: String,
    pub page_number: Option<i32>,
    pub source_start: Option<i32>,
    pub source_end: Option<i32>,
    pub node_metadata: String,
    pub embedding_text: String,
}

/// In-memory RAPTOR node (with vector, not stored in DB directly)
#[derive(Debug, Clone)]
pub struct RaptorNode {
    pub id: i64,
    pub library_id: i64,
    pub document_id: i64,
    pub content: String,
    pub level: i32,
    pub parent_id: Option<i64>,
    pub chunk_order: i32,
    pub vector: Option<Vec<f32>>,
    // P0-011: 扩展节点元数据字段
    pub title_path: String,
    pub page_number: Option<i32>,
    pub source_start: Option<i32>,
    pub source_end: Option<i32>,
    pub node_metadata: String,
    pub embedding_text: String,
}

// --- Search model ---

/// 对话消息，用于查询改写时传递多轮对话历史
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String, // "user" 或 "assistant"
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct KbSearchInput {
    pub library_ids: Vec<i64>,
    /// Optional hard filter for specific KB documents. Empty means all documents
    /// within the selected libraries are eligible.
    #[serde(default)]
    pub document_ids: Vec<i64>,
    pub query: String,
    pub level: Option<i32>,
    pub top_k: usize,
    // P3-028: 扩展搜索参数
    pub profile: Option<String>,
    pub planner_override: Option<String>,
    pub debug: bool,
    pub include_context: bool,
    pub context_before: usize,
    pub context_after: usize,
    pub include_highlights: bool,
    pub group_by_document: bool,
    pub folder_id: Option<i64>,
    // P5-011: filter by specific embedding index
    pub embedding_index_id: Option<i64>,
    pub rerank_model: Option<String>,
    /// 限制每个文档在检索结果中的最大 chunk 数，避免大文档垄断结果。
    /// None 表示不限制（由 top_k 自然截断）。推荐值 3。
    pub max_chunks_per_doc: Option<usize>,
    /// 多轮对话历史，用于查询改写。为空时跳过改写。
    pub chat_history: Vec<ChatMessage>,
    /// 查询改写使用的 LLM API key（空则跳过改写）
    #[serde(skip_serializing)]
    pub rewrite_api_key: Option<String>,
    /// 查询改写使用的 LLM base URL
    pub rewrite_base_url: Option<String>,
    /// 查询改写使用的 LLM 模型名
    pub rewrite_model: Option<String>,
    // FIX11-07: API key 不应序列化到 JSON 日志/响应中，防止泄露
    #[serde(skip_serializing)]
    pub rerank_api_key: Option<String>,
    pub rerank_base_url: Option<String>,
    // ACL 已移除 — 个人使用全库可见，不需要访问控制
    /// 可选质量门控配置。None 表示使用 profile 派生的默认 gate。
    #[serde(default)]
    pub quality_gate: Option<SearchQualityGate>,
}

impl Default for KbSearchInput {
    fn default() -> Self {
        Self {
            library_ids: vec![],
            document_ids: vec![],
            query: String::new(),
            level: None,
            top_k: 10,
            profile: None,
            planner_override: None,
            debug: false,
            include_context: false,
            context_before: 200,
            context_after: 200,
            include_highlights: false,
            group_by_document: false,
            folder_id: None,
            embedding_index_id: None,
            rerank_model: None,
            rerank_api_key: None,
            rerank_base_url: None,
            max_chunks_per_doc: None,
            chat_history: Vec::new(),
            rewrite_api_key: None,
            rewrite_base_url: None,
            rewrite_model: None,
            quality_gate: None,
        }
    }
}

/// 质量门控配置。RRF 融合后,rerank 前应用,以避免低质量噪声进入 rerank。
///
/// 关键修正:PandaWiki 直接做 `score >= 0.2` 的相似度阈值过滤不适合 gbrain_rs,
/// 因为这里的 score 可能是 RRF 分、rerank 分或本地融合分,不是统一的语义相似度。
/// 本结构使用原始信号(vector_similarity / fts_rank_score / exact_match / 多检索器命中)
/// 进行门控判断。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQualityGate {
    /// 向量相似度最小阈值(如 0.55)。命中即视为通过。
    pub min_vector_similarity: Option<f64>,
    /// FTS rank 派生分数(`1/rank`)最小阈值。命中即视为通过。
    pub min_fts_rank_score: Option<f64>,
    /// 摘要检索器 rank 分数(`1/rank`)最小阈值。
    pub min_summary_score: Option<f64>,
    /// 表格检索器 rank 分数(`1/rank`)最小阈值。
    pub min_table_score: Option<f64>,
    /// 元数据检索器 rank 分数(`1/rank`)最小阈值。
    pub min_metadata_score: Option<f64>,
    /// 是否允许精确标题匹配豁免(命中即通过,无视其他阈值)
    pub allow_exact_title_match: bool,
    /// 是否允许多检索器命中豁免(>=2 retrievers 即通过)
    pub allow_multi_retriever_match: bool,
}

impl Default for SearchQualityGate {
    fn default() -> Self {
        // balanced 默认:任一信号足够好即通过
        Self {
            min_vector_similarity: Some(0.55),
            min_fts_rank_score: Some(0.18),
            min_summary_score: Some(0.2),
            min_table_score: Some(0.2),
            min_metadata_score: Some(0.2),
            allow_exact_title_match: true,
            allow_multi_retriever_match: true,
        }
    }
}

impl SearchQualityGate {
    /// precise profile:更严格,要求至少一个信号明显高于阈值。
    fn precise() -> Self {
        Self {
            min_vector_similarity: Some(0.7),
            min_fts_rank_score: Some(0.3),
            min_summary_score: Some(0.5),
            min_table_score: Some(0.5),
            min_metadata_score: Some(0.5),
            allow_exact_title_match: true,
            allow_multi_retriever_match: true,
        }
    }

    /// recall profile:更宽松,容忍单检索器低分命中(用于召回优先场景)
    fn recall() -> Self {
        Self {
            min_vector_similarity: Some(0.4),
            min_fts_rank_score: Some(0.1),
            min_summary_score: Some(0.1),
            min_table_score: Some(0.1),
            min_metadata_score: Some(0.1),
            allow_exact_title_match: true,
            allow_multi_retriever_match: false,
        }
    }

    /// 按 search profile 名派生默认质量门控。
    pub fn from_profile(profile: &str) -> Self {
        match profile {
            "precise" => Self::precise(),
            "recall" => Self::recall(),
            _ => Self::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KbSearchResult {
    pub node_id: i64,
    pub document_id: i64,
    pub document_name: String,
    pub content: String,
    pub level: i32,
    pub score: f64,
    pub library_id: i64,
    pub library_name: String,
    // P3-024~027: 扩展返回字段
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_number: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight_ranges: Option<Vec<(usize, usize)>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_signals: Option<serde_json::Value>,
    // P3-025: group_by_document metadata
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_hits: Option<Vec<KbSearchResult>>,
    /// P1-2: 命中文档关联的媒体引用（图片/附件），用于在 prompt 中保留富文本上下文
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_refs: Option<Vec<MediaRef>>,
}

/// P3-025: Document group for group_by_document search.
/// Represents a cluster of search results from the same document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentGroup {
    pub document_id: i64,
    pub document_title: String,
    pub best_score: f64,
    pub hits: Vec<KbSearchResult>,
}

/// 检索器类型枚举,标识命中的来源检索通道。
/// 用于 signal-preserving fusion,在 RRF 合并后保留每条结果来自哪些检索器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetrieverKind {
    /// 标题/文件名检索器(kb_doc_name_fts)
    TitleName,
    /// 节点内容 FTS 检索器(kb_doc_fts)
    NodeFts,
    /// Passage 检索器(kb_passage_fts,目前未独立调用,预留)
    PassageFts,
    /// 向量检索器(sqlite-vec / fallback cosine)
    Vector,
    /// 摘要检索器(kb_document_summaries)
    Summary,
    /// 表格检索器(kb_table_rows)
    Table,
    /// 元数据检索器(kb_documents title/keywords/entity_names)
    Metadata,
}

/// 单条候选结果的检索信号集合。
/// RRF 融合时不会丢弃任何来源信号,后续 rerank 阶段可使用完整信号加权。
#[derive(Debug, Clone, Default)]
pub struct RankSignals {
    /// 累加的 RRF 分数(各检索器贡献之和)
    pub rrf_score: f64,
    /// 来源检索器的原始分数(取最高者),语义不一定统一
    pub source_score: f64,
    /// FTS5 BM25 原始分数(lower is better,仅 FTS 检索器写入)
    pub fts_bm25_raw: Option<f64>,
    /// 由 FTS rank 派生的分数(`1/(rank+1)`),用于跨检索器归一
    pub fts_rank_score: Option<f64>,
    /// 向量相似度(余弦相似度,通常 0.0~1.0)
    pub vector_similarity: Option<f64>,
    /// 向量原始距离(sqlite-vec 返回的 distance 值，仅向量检索器写入)
    /// cosine 距离下 similarity = 1.0 - distance
    pub vector_distance: Option<f64>,
    /// 标题匹配分数
    pub title_score: Option<f64>,
    /// 摘要匹配分数
    pub summary_score: Option<f64>,
    /// 表格匹配分数
    pub table_score: Option<f64>,
    /// 元数据匹配分数
    pub metadata_score: Option<f64>,
    /// 是否触发精确标题匹配(质量门控可豁免)
    pub exact_match: bool,
    /// 命中此节点的检索器集合
    pub retrievers: Vec<RetrieverKind>,
}

impl RankSignals {
    /// 合并另一组信号(用于 RRF 合并阶段)。rrf_score 累加,可选字段取已存在或新值,
    /// retrievers 取并集,exact_match 取或。
    pub fn merge_from(&mut self, other: &RankSignals) {
        self.rrf_score += other.rrf_score;
        if self.source_score < other.source_score {
            self.source_score = other.source_score;
        }
        if self.fts_bm25_raw.is_none() && other.fts_bm25_raw.is_some() {
            self.fts_bm25_raw = other.fts_bm25_raw;
        }
        if self.fts_rank_score.is_none() && other.fts_rank_score.is_some() {
            self.fts_rank_score = other.fts_rank_score;
        }
        if self.vector_similarity.is_none() && other.vector_similarity.is_some() {
            self.vector_similarity = other.vector_similarity;
        }
        if self.vector_distance.is_none() && other.vector_distance.is_some() {
            self.vector_distance = other.vector_distance;
        }
        if self.title_score.is_none() && other.title_score.is_some() {
            self.title_score = other.title_score;
        }
        if self.summary_score.is_none() && other.summary_score.is_some() {
            self.summary_score = other.summary_score;
        }
        if self.table_score.is_none() && other.table_score.is_some() {
            self.table_score = other.table_score;
        }
        if self.metadata_score.is_none() && other.metadata_score.is_some() {
            self.metadata_score = other.metadata_score;
        }
        if other.exact_match {
            self.exact_match = true;
        }
        for k in &other.retrievers {
            if !self.retrievers.contains(k) {
                self.retrievers.push(*k);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct RankedResult {
    pub node_id: i64,
    pub rank: usize,
    pub score: f64,
    /// 保留信号:RRF 累加前后的所有来源分数。向后兼容字段,
    /// 单检索器路径默认为空 signals。
    pub signals: RankSignals,
}

impl RankedResult {
    /// 创建仅含 node_id 与默认 signals 的占位结果(各 retriever 会回填信号)
    pub fn placeholder(node_id: i64) -> Self {
        Self {
            node_id,
            rank: 0,
            score: 0.0,
            signals: RankSignals::default(),
        }
    }
}

// --- Pipeline model ---

#[derive(Debug, Clone)]
pub struct ProcessResult {
    pub word_total: i32,
    pub split_total: i32,
    /// P2 修复：显式标记异步 OCR 已延后，替代 word_total==0 && split_total==0 的隐式判断。
    /// 合法的空文档也会产生 0/0，导致误判。此字段精确表达"文档因异步 OCR 而延后"语义。
    pub deferred_ocr: bool,
}

// --- Parser block abstraction (P1-010) ---

/// 所有 parser 输出的统一 block 结构，屏蔽格式差异。
#[derive(Debug, Clone)]
pub struct ParsedBlock {
    /// 文本内容（原文）
    pub text: String,
    /// 标题路径，如 "第一章 / 1.1 概述"
    pub title_path: String,
    /// 页码（PDF 等，非页面格式为 None）
    pub page_number: Option<i32>,
    /// 在原文件中的起始字符偏移
    pub source_start: Option<i32>,
    /// 在原文件中的结束字符偏移
    pub source_end: Option<i32>,
    /// block 类型：paragraph, heading, list_item, table, code, whole_document
    pub block_type: String,
    /// 扩展元数据 JSON
    pub metadata: String,
}

// --- P1-2: 富文本标准化与媒体引用 ---

/// 媒体引用：从富文本（HTML/PDF）中抽取的图片/附件引用。
/// 写入 `kb_media_assets`，并在 `node_metadata.media_refs` 中以 JSON 形式保留。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaRef {
    /// 媒体类型：image / attachment / audio / video
    pub media_type: String,
    /// 存储路径或远程 URL
    pub storage_path: String,
    /// MIME 类型（内嵌媒体或已知远程资源）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// 媒体字节大小
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_size: Option<i64>,
    /// 仅用于处理阶段的内嵌媒体数据；不会写入 node metadata 或 API 响应。
    #[serde(skip)]
    pub embedded_data_base64: Option<String>,
    /// alt 文本（图片）或链接文本（附件）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt_text: Option<String>,
    /// OCR 提取的文本（图片）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr_text: Option<String>,
    /// 图注 / 标题
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// 来源页码
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_number: Option<i32>,
}

impl ParsedBlock {
    /// 创建一个简单段落 block
    pub fn paragraph(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            title_path: String::new(),
            page_number: None,
            source_start: None,
            source_end: None,
            block_type: "paragraph".to_string(),
            metadata: String::new(),
        }
    }

    /// 创建一个全文 block（用于 micro/small 文档）
    pub fn whole_document(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            title_path: String::new(),
            page_number: None,
            source_start: None,
            source_end: None,
            block_type: "whole_document".to_string(),
            metadata: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Parsing,
    Chunking,
    Embedding,
    Raptor,
    Persist,
}

#[derive(Debug)]
pub struct PhaseError {
    pub phase: Phase,
    pub source: Box<dyn std::error::Error + Send + Sync>,
}

impl PhaseError {
    pub fn new<E>(phase: Phase, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self {
            phase,
            source: Box::new(source),
        }
    }
}

impl std::fmt::Display for PhaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "phase {:?}: {}", self.phase, self.source)
    }
}

impl std::error::Error for PhaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // L7: 返回内部错误作为 source，使 error chain 可通过 .source() 遍历。
        // Some(self.source.as_ref()) 是包装器模式的标准实现：
        // 调用方可通过 iter::successors(err.source(), |e| e.source()) 遍历完整错误链。
        Some(self.source.as_ref())
    }
}

impl From<PhaseError> for GBrainError {
    fn from(err: PhaseError) -> Self {
        GBrainError::InvalidInput(err.to_string())
    }
}
