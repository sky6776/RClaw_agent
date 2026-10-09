//! Configuration loading
//! Mirrors gbrain's src/core/config.ts
//!
//! 外部模型服务配置必须由各用途自己的环境变量显式提供。
//! gbrain 不再把 GBRAIN_OPENAI_* 当作查询扩展、分块、RAPTOR 或转录的共享回退。

use crate::error::GBrainError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{debug, info, trace};

/// Brain configuration
/// R3-03 fix: API keys are marked with `#[serde(skip_serializing)]` to prevent
/// accidental leakage when `Config::save()` writes config.json. Keys are still
/// deserialized from config files (for backward compatibility) but are never
/// written back out — they should only come from environment variables.
// 修复：添加 #[serde(default)]，旧 config.json 缺少新增字段时仍能正常加载
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    // --- Database ---
    pub database_path: Option<String>,
    pub wal_mode: bool,
    pub pool_size: usize,

    // --- Embedding (vector search) ---
    #[serde(skip_serializing)]
    pub embedding_api_key: Option<String>,
    pub embedding_base_url: Option<String>,
    pub embedding_model: String,
    pub embedding_dimensions: usize,

    // --- Chunking ---
    // M45 说明：chunk_size 和 chunk_overlap 的单位均为字符数（chars），而非字节数。
    // 即 chunk_size=500 表示每块最多 500 个 Unicode 字符。这与 TextSplitter 的行为一致。
    pub chunk_size: usize,
    pub chunk_overlap: usize,

    // --- Query Expansion (LLM) ---
    #[serde(skip_serializing)]
    pub expansion_api_key: Option<String>,
    pub expansion_base_url: Option<String>,
    pub expansion_model: String,

    // --- LLM Chunking (semantic) ---
    #[serde(skip_serializing)]
    pub chunker_api_key: Option<String>,
    pub chunker_base_url: Option<String>,
    pub chunker_model: String,
    /// 普通脑页分块模式：auto / recursive / llm。
    ///
    /// auto 为默认值：非代码页面优先尝试 LLM 引导分块，不适合或失败时回退 recursive。
    pub page_chunker_mode: String,

    // --- Transcription (speech-to-text) ---
    pub transcription_provider: String, // "groq" | "openai"
    #[serde(skip_serializing)]
    pub transcription_groq_api_key: Option<String>,
    pub transcription_groq_base_url: Option<String>,
    #[serde(skip_serializing)]
    pub transcription_openai_api_key: Option<String>,
    pub transcription_openai_base_url: Option<String>,

    // --- Logging ---
    pub log_level: String,             // "trace"|"debug"|"info"|"warn"|"error"
    pub log_to_file: bool,             // default true
    pub log_file_path: Option<String>, // None → $GBRAIN_DIR/logs/gbrain.log
    pub log_to_console: bool,          // default true

    // --- P2-6: Auto-link / Auto-timeline (mirrors TS) ---
    pub auto_link: bool,     // default true, GBRAIN_AUTO_LINK
    pub auto_timeline: bool, // default true, GBRAIN_AUTO_TIMELINE

    // --- P2-3: Post-write lint (mirrors TS runPostWriteLint) ---
    pub post_write_lint: bool, // default false, GBRAIN_POST_WRITE_LINT

    // --- KB subsystem config ---
    pub kb_enabled: bool,
    pub kb_raptor_secret_ref: Option<String>,
    pub kb_raptor_base_url: Option<String>,
    pub kb_raptor_model: String,
    pub kb_max_file_size_mb: usize,
    pub kb_allowed_extensions: Vec<String>,
    pub kb_storage_dir: Option<String>,
    pub kb_worker_enabled: bool,
    pub kb_worker_poll_interval_secs: u64,
    pub autopilot_enabled: bool,
    pub autopilot_interval_secs: u64,
    /// P3-003: 同义词文件路径
    pub kb_synonyms_file: Option<String>,
    /// P3-004: 别名映射文件路径
    pub kb_aliases_file: Option<String>,

    // --- 单入口多投影融合架构 ---
    /// Artifact 存储目录（默认 $GBRAIN_DIR/artifacts）
    pub artifact_storage_dir: Option<String>,
    /// 默认 KB library ID（upload_source 使用）
    pub default_kb_library_id: Option<i64>,
    /// 上传默认提升策略
    pub upload_default_promotion_policy: String,
    /// artifact 默认意图。默认 "memory"。可选值: memory, evidence, promote
    pub artifact_default_intent: String,
    /// 当 artifact_put 需要写入 KB 但没有 Inbox 库时，是否自动创建。默认 true。
    pub artifact_auto_create_inbox_library: bool,
    /// artifact_put 的 memory 意图是否写入 KB。默认 true。设为 false 则仅写入 gbrain 页面。
    pub artifact_manual_memory_to_kb: bool,

    // --- OCR 子系统配置 ---
    /// OCR 是否启用（默认 true）
    pub ocr_enabled: bool,
    /// OCR API key（仅从环境变量 GBRAIN_OCR_API_KEY 读取）
    /// L13: 使用 skip_serializing 允许从配置文件读取（向后兼容），但写入时不持久化密钥。
    /// 环境变量优先级高于配置文件值（见 apply_env_overrides）。
    #[serde(skip_serializing)]
    pub ocr_api_key: Option<String>,
    /// OCR base URL（默认智谱 GLM-OCR endpoint）
    pub ocr_base_url: String,
    /// 是否允许自定义 base URL（默认 false，固定官方 endpoint）
    pub ocr_allow_custom_base_url: bool,
    /// OCR 模型名（默认 glm-ocr）
    pub ocr_model: String,
    /// OCR profile: auto/general/table/formula/handwriting（只影响后处理，不发送给 API）
    pub ocr_profile: String,
    /// 是否启用 layout details（默认 true，用于块级合并）
    pub ocr_enable_layout: bool,
    /// OCR 模式: auto/all_pages
    pub ocr_mode: String,
    /// OCR 提交模式: pdf_first/pdf_range
    pub ocr_submit_mode: String,
    /// OCR 文本密度阈值（字符数低于此值视为低密度页）
    pub ocr_text_density_threshold: usize,
    /// OCR 最低低密度页比例（触发 OCR 的阈值）
    /// L29: 此字段已废弃 — detect_ocr_pages 不再使用 ratio 参数，
    /// 保留字段以兼容现有配置文件反序列化。
    #[deprecated(note = "不再使用，低密度页比例不作为 OCR 触发条件")]
    pub ocr_min_low_density_ratio: f64,
    /// OCR 图片面积覆盖率阈值
    pub ocr_image_area_threshold: f64,
    /// OCR 嵌入图片数量阈值
    pub ocr_image_count_threshold: usize,
    /// OCR 每页超时秒数
    pub ocr_timeout_seconds_per_page: u64,
    /// OCR 单次请求最大页数（默认 5）
    pub ocr_max_pages_per_request: usize,
    /// OCR 单次请求最大 PDF 字节数（默认 50MB）
    pub ocr_max_pdf_bytes_per_request: usize,
    /// OCR 单文档最大页数（默认 300）
    pub ocr_max_pages_per_document: usize,
    /// OCR 全局最大并发请求数（默认 2）
    pub ocr_max_concurrency: usize,
    /// OCR 临时目录总字节预算（默认 512MB）
    pub ocr_temp_dir_max_bytes: u64,
    /// OCR 是否返回裁剪图片（默认 false）
    pub ocr_return_crop_images: bool,
    /// OCR 是否返回版面可视化（默认 false）
    pub ocr_need_layout_visualization: bool,
    /// OCR 是否同步内联执行（默认 false，生产用异步 job）
    pub ocr_sync_inline: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Database
            database_path: None,
            wal_mode: true,
            pool_size: 10,

            // Embedding
            embedding_api_key: None,
            embedding_base_url: None,
            embedding_model: "text-embedding-3-large".to_string(),
            embedding_dimensions: 1536,

            // Chunking
            chunk_size: 500,
            chunk_overlap: 50,

            // Query Expansion
            expansion_api_key: None,
            expansion_base_url: None,
            expansion_model: "gpt-4o-mini".to_string(),

            // LLM Chunker
            chunker_api_key: None,
            chunker_base_url: None,
            chunker_model: "gpt-4o-mini".to_string(),
            page_chunker_mode: "auto".to_string(),

            // Transcription
            transcription_provider: "groq".to_string(),
            transcription_groq_api_key: None,
            transcription_groq_base_url: None,
            transcription_openai_api_key: None,
            transcription_openai_base_url: None,

            // Logging
            log_level: "info".to_string(),
            log_to_file: true,
            log_file_path: None,
            log_to_console: true,

            // P2-6: Auto-link / Auto-timeline
            auto_link: true,
            auto_timeline: true,

            // P2-3: Post-write lint
            post_write_lint: false,

            kb_enabled: true,
            kb_raptor_secret_ref: Some("GBRAIN_KB_RAPTOR_API_KEY".to_string()),
            kb_raptor_base_url: None,
            kb_raptor_model: String::new(),
            kb_max_file_size_mb: 50,
            kb_allowed_extensions: vec![
                // 修复：默认 KB 允许列表与 KB_SUPPORTED_EXTENSIONS 保持一致，
                // 否则 route planner 认为可处理的文件会被 MCP 安全检查拒绝
                "pdf".into(),
                "docx".into(),
                "xls".into(),
                "xlsx".into(),
                "csv".into(),
                "tsv".into(),
                "html".into(),
                "htm".into(),
                "txt".into(),
                "md".into(),
                "markdown".into(),
                "rst".into(),
                "json".into(),
                "xml".into(),
                "yaml".into(),
                "yml".into(),
                "toml".into(),
                // GLM-OCR 可直接解析的单图格式。
                "png".into(),
                "jpg".into(),
                "jpeg".into(),
            ],
            kb_storage_dir: None,
            kb_worker_enabled: true,
            kb_worker_poll_interval_secs: 30,
            autopilot_enabled: true,
            autopilot_interval_secs: 3600,
            kb_synonyms_file: None,
            kb_aliases_file: None,

            // 单入口多投影融合架构
            artifact_storage_dir: None,
            default_kb_library_id: None,
            upload_default_promotion_policy: "auto-apply".to_string(),
            // artifact 默认意图为 memory（写入 gbrain 页面 + KB）
            artifact_default_intent: "memory".to_string(),
            // 当 artifact_put 需要写入 KB 但没有 Inbox 库时，自动创建
            artifact_auto_create_inbox_library: true,
            // artifact_put 的 memory 意图默认写入 KB
            artifact_manual_memory_to_kb: true,

            // OCR 子系统默认配置
            ocr_enabled: true,
            ocr_api_key: None,
            ocr_base_url: "https://open.bigmodel.cn/api/paas/v4/layout_parsing".to_string(),
            ocr_allow_custom_base_url: false,
            ocr_model: "glm-ocr".to_string(),
            ocr_profile: "auto".to_string(),
            ocr_enable_layout: true,
            ocr_mode: "auto".to_string(),
            ocr_submit_mode: "pdf_range".to_string(),
            ocr_text_density_threshold: 50,
            #[allow(deprecated)]
            ocr_min_low_density_ratio: 0.5,
            ocr_image_area_threshold: 0.08,
            ocr_image_count_threshold: 1,
            ocr_timeout_seconds_per_page: 60,
            ocr_max_pages_per_request: 5,
            ocr_max_pdf_bytes_per_request: 52_428_800, // 50MB
            ocr_max_pages_per_document: 300,
            ocr_max_concurrency: 2,
            ocr_temp_dir_max_bytes: 536_870_912, // 512MB
            ocr_return_crop_images: false,
            ocr_need_layout_visualization: false,
            ocr_sync_inline: false,
        }
    }
}

/// Resolved RAPTOR LLM configuration (avoids cross-module dependency).
#[derive(Debug, Clone)]
pub struct ResolvedRaptorConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
}

impl Config {
    /// 轻量配置加载：只读 config.json 与可选环境变量，不执行严格运行时校验。
    ///
    /// P1 修复：供 CLI 的 dry-run / config / --db / init 等“不需要外部模型”的快速路径使用，
    /// 避免在 fresh 或 partial 环境（缺少 GBRAIN_EMBEDDING_* 等必填 env）下提前失败。
    /// 本函数只读取 config.json 与可选 env 并填充字段，绝不调用
    /// `apply_required_external_model_env()`，因此不会因缺失外部模型 env 而返回 Err。
    pub fn load_partial() -> Result<Self, Box<dyn std::error::Error>> {
        let mut config = Self::default();

        // Try loading from config file
        let config_path = Self::base_dir().join("config.json");
        if config_path.exists() {
            debug!(path = %config_path.display(), "Loading config.json");
            let content = std::fs::read_to_string(&config_path)?;
            let file_config: Config = serde_json::from_str(&content)?;
            config.merge(file_config);
            info!(path = %config_path.display(), "Merged config.json");
        }

        // --- Database env vars ---
        if let Ok(path) = std::env::var("GBRAIN_DB_PATH") {
            config.database_path = Some(path);
        }

        // --- Embedding env vars ---
        if let Ok(key) = std::env::var("GBRAIN_EMBEDDING_API_KEY") {
            config.embedding_api_key = Some(key);
        }
        if let Ok(url) = std::env::var("GBRAIN_EMBEDDING_BASE_URL") {
            config.embedding_base_url = Some(url);
        }
        if let Ok(model) = std::env::var("GBRAIN_EMBEDDING_MODEL") {
            config.embedding_model = model;
        }
        if let Ok(dims) = std::env::var("GBRAIN_EMBEDDING_DIMENSIONS") {
            if let Ok(d) = dims.parse() {
                config.embedding_dimensions = d;
            }
        }

        // --- Query Expansion env vars ---
        if let Ok(key) = std::env::var("GBRAIN_EXPANSION_API_KEY") {
            config.expansion_api_key = Some(key);
        }
        if let Ok(url) = std::env::var("GBRAIN_EXPANSION_BASE_URL") {
            config.expansion_base_url = Some(url);
        }
        if let Ok(model) = std::env::var("GBRAIN_EXPANSION_MODEL") {
            config.expansion_model = model;
        }

        // --- LLM Chunker env vars ---
        if let Ok(key) = std::env::var("GBRAIN_CHUNKER_API_KEY") {
            config.chunker_api_key = Some(key);
        }
        if let Ok(url) = std::env::var("GBRAIN_CHUNKER_BASE_URL") {
            config.chunker_base_url = Some(url);
        }
        if let Ok(model) = std::env::var("GBRAIN_CHUNKER_MODEL") {
            config.chunker_model = model;
        }
        if let Ok(mode) = std::env::var("GBRAIN_PAGE_CHUNKER_MODE") {
            config.page_chunker_mode = mode.to_lowercase();
        }

        // --- Transcription env vars ---
        if let Ok(provider) = std::env::var("GBRAIN_TRANSCRIPTION_PROVIDER") {
            config.transcription_provider = provider;
        }
        if let Ok(key) = std::env::var("GBRAIN_TRANSCRIPTION_GROQ_API_KEY") {
            config.transcription_groq_api_key = Some(key);
        }
        if let Ok(url) = std::env::var("GBRAIN_TRANSCRIPTION_GROQ_BASE_URL") {
            config.transcription_groq_base_url = Some(url);
        }
        if let Ok(key) = std::env::var("GBRAIN_TRANSCRIPTION_OPENAI_API_KEY") {
            config.transcription_openai_api_key = Some(key);
        }
        if let Ok(url) = std::env::var("GBRAIN_TRANSCRIPTION_OPENAI_BASE_URL") {
            config.transcription_openai_base_url = Some(url);
        }

        // --- Logging env vars ---
        if let Ok(level) = std::env::var("GBRAIN_LOG_LEVEL") {
            config.log_level = level;
        }
        if let Ok(to_file) = std::env::var("GBRAIN_LOG_TO_FILE") {
            config.log_to_file = to_file.parse().unwrap_or(true);
        }
        if let Ok(path) = std::env::var("GBRAIN_LOG_FILE_PATH") {
            config.log_file_path = Some(path);
        }
        if let Ok(to_console) = std::env::var("GBRAIN_LOG_TO_CONSOLE") {
            config.log_to_console = to_console.parse().unwrap_or(true);
        }

        // --- P2-6: Auto-link / Auto-timeline env vars ---
        if let Ok(auto_link) = std::env::var("GBRAIN_AUTO_LINK") {
            config.auto_link = auto_link != "false" && auto_link != "0";
        }
        if let Ok(auto_timeline) = std::env::var("GBRAIN_AUTO_TIMELINE") {
            config.auto_timeline = auto_timeline != "false" && auto_timeline != "0";
        }
        // --- P2-3: Post-write lint env var ---
        if let Ok(post_write_lint) = std::env::var("GBRAIN_POST_WRITE_LINT") {
            config.post_write_lint = post_write_lint == "true" || post_write_lint == "1";
        }

        // KB config
        config.kb_enabled = std::env::var("GBRAIN_KB_ENABLED")
            .map(|v| v == "true")
            .unwrap_or(config.kb_enabled);
        if let Ok(url) = std::env::var("GBRAIN_KB_RAPTOR_BASE_URL") {
            config.kb_raptor_base_url = Some(url);
        }
        if let Ok(model) = std::env::var("GBRAIN_KB_RAPTOR_MODEL") {
            config.kb_raptor_model = model;
        }
        if let Ok(size) = std::env::var("GBRAIN_KB_MAX_FILE_SIZE_MB") {
            if let Ok(s) = size.parse() {
                config.kb_max_file_size_mb = s;
            }
        }
        if let Ok(ext) = std::env::var("GBRAIN_KB_ALLOWED_EXTENSIONS") {
            config.kb_allowed_extensions = ext.split(',').map(|s| s.trim().to_string()).collect();
        }
        if let Ok(dir) = std::env::var("GBRAIN_KB_STORAGE_DIR") {
            config.kb_storage_dir = Some(dir);
        }
        config.kb_worker_enabled =
            parse_env_bool("GBRAIN_KB_WORKER_ENABLED").unwrap_or(config.kb_worker_enabled);
        if let Ok(secs) = std::env::var("GBRAIN_KB_WORKER_POLL_INTERVAL") {
            if let Ok(s) = secs.parse() {
                config.kb_worker_poll_interval_secs = s;
            }
        }
        config.autopilot_enabled =
            parse_env_bool("GBRAIN_AUTOPILOT_ENABLED").unwrap_or(config.autopilot_enabled);
        if let Ok(secs) = std::env::var("GBRAIN_AUTOPILOT_INTERVAL") {
            let s: u64 = match secs.parse() {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        "GBRAIN_AUTOPILOT_INTERVAL 无效值 '{}': {}，使用默认 {}s",
                        secs,
                        e,
                        config.autopilot_interval_secs
                    );
                    // 不覆盖，保留 config 默认值
                    config.autopilot_interval_secs
                }
            };
            if s < 60 {
                tracing::warn!("GBRAIN_AUTOPILOT_INTERVAL={}s < 60s，clamp 到 60s", s);
                config.autopilot_interval_secs = 60;
            } else {
                config.autopilot_interval_secs = s;
            }
        }

        // --- 单入口多投影融合架构 env vars ---
        if let Ok(dir) = std::env::var("GBRAIN_ARTIFACT_STORAGE_DIR") {
            config.artifact_storage_dir = Some(dir);
        }
        if let Ok(id) = std::env::var("GBRAIN_DEFAULT_KB_LIBRARY_ID") {
            if let Ok(library_id) = id.parse::<i64>() {
                config.default_kb_library_id = Some(library_id);
            }
        }
        if let Ok(policy) = std::env::var("GBRAIN_UPLOAD_PROMOTION_POLICY") {
            config.upload_default_promotion_policy = policy;
        }
        // artifact 默认意图（默认 "memory"，可选: memory, evidence, promote）
        if let Ok(intent) = std::env::var("GBRAIN_ARTIFACT_DEFAULT_INTENT") {
            config.artifact_default_intent = intent;
        }
        // 当 artifact_put 需要写入 KB 但没有 Inbox 库时，是否自动创建
        config.artifact_auto_create_inbox_library =
            std::env::var("GBRAIN_ARTIFACT_AUTO_CREATE_INBOX_LIBRARY")
                .map(|v| v != "false" && v != "0")
                .unwrap_or(config.artifact_auto_create_inbox_library);
        // artifact_put 的 memory 意图是否写入 KB（默认 true，设为 false 则仅写入 gbrain 页面）
        config.artifact_manual_memory_to_kb = std::env::var("GBRAIN_ARTIFACT_MANUAL_MEMORY_TO_KB")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(config.artifact_manual_memory_to_kb);

        // --- OCR 子系统环境变量 ---
        config.ocr_enabled = parse_env_bool("GBRAIN_OCR_ENABLED").unwrap_or(config.ocr_enabled);
        // API key: 只读取 GBRAIN_OCR_API_KEY，不再兼容旧别名
        if let Ok(key) = std::env::var("GBRAIN_OCR_API_KEY") {
            config.ocr_api_key = Some(key);
        }
        // base URL: 默认固定智谱 endpoint，需显式开启自定义
        if let Ok(url) = std::env::var("GBRAIN_OCR_BASE_URL") {
            config.ocr_base_url = url;
        }
        // 安全开关：仅接受环境变量显式放行，忽略配置文件中的该字段，
        // 防止配置文件将 PDF 内容与 Bearer API key 发往任意地址
        if let Some(allowed) = parse_env_bool("GBRAIN_OCR_ALLOW_CUSTOM_BASE_URL") {
            config.ocr_allow_custom_base_url = allowed;
        }
        // 模型: 只读取 GBRAIN_OCR_MODEL，不再兼容旧别名
        if let Ok(model) = std::env::var("GBRAIN_OCR_MODEL") {
            config.ocr_model = model;
        }
        if let Ok(profile) = std::env::var("GBRAIN_OCR_PROFILE") {
            let profile = profile.trim();
            if matches!(
                profile,
                "auto" | "general" | "table" | "formula" | "handwriting"
            ) {
                config.ocr_profile = profile.to_string();
            } else if !profile.is_empty() {
                tracing::warn!(
                    "GBRAIN_OCR_PROFILE 无效值 '{}'，有效值: auto/general/table/formula/handwriting，已忽略",
                    profile
                );
            }
        }
        // layout 开关: 只读取 GBRAIN_OCR_ENABLE_LAYOUT，不再兼容旧别名
        config.ocr_enable_layout =
            parse_env_bool("GBRAIN_OCR_ENABLE_LAYOUT").unwrap_or(config.ocr_enable_layout);
        if let Ok(mode) = std::env::var("GBRAIN_OCR_MODE") {
            config.ocr_mode = mode;
        }
        if let Ok(mode) = std::env::var("GBRAIN_OCR_SUBMIT_MODE") {
            config.ocr_submit_mode = mode;
        }
        if let Some(v) = first_valid_env_u64(&["GBRAIN_OCR_TEXT_DENSITY_THRESHOLD"]) {
            config.ocr_text_density_threshold = v as usize;
        }
        if let Some(v) = first_valid_env_f64(&["GBRAIN_OCR_MIN_LOW_DENSITY_RATIO"]) {
            #[allow(deprecated)]
            {
                config.ocr_min_low_density_ratio = v;
            }
        }
        if let Some(v) = first_valid_env_f64(&["GBRAIN_OCR_IMAGE_AREA_THRESHOLD"]) {
            config.ocr_image_area_threshold = v;
        }
        if let Some(v) = first_valid_env_u64(&["GBRAIN_OCR_IMAGE_COUNT_THRESHOLD"]) {
            config.ocr_image_count_threshold = v as usize;
        }
        // 超时: 只读取 GBRAIN_OCR_TIMEOUT_SECONDS_PER_PAGE，不再兼容旧别名
        if let Some(v) = first_valid_env_u64(&["GBRAIN_OCR_TIMEOUT_SECONDS_PER_PAGE"]) {
            config.ocr_timeout_seconds_per_page = v;
        }
        if let Some(v) = first_valid_env_u64(&["GBRAIN_OCR_MAX_PAGES_PER_REQUEST"]) {
            config.ocr_max_pages_per_request = v as usize;
        }
        if let Some(v) = first_valid_env_u64(&["GBRAIN_OCR_MAX_PDF_BYTES_PER_REQUEST"]) {
            config.ocr_max_pdf_bytes_per_request = v as usize;
        }
        if let Some(v) = first_valid_env_u64(&["GBRAIN_OCR_MAX_PAGES_PER_DOCUMENT"]) {
            config.ocr_max_pages_per_document = v as usize;
        }
        if let Some(v) = first_valid_env_u64(&["GBRAIN_OCR_MAX_CONCURRENCY"]) {
            config.ocr_max_concurrency = v as usize;
        }
        if let Some(v) = first_valid_env_u64(&["GBRAIN_OCR_TEMP_DIR_MAX_BYTES"]) {
            config.ocr_temp_dir_max_bytes = v;
        }
        config.ocr_return_crop_images = parse_env_bool("GBRAIN_OCR_RETURN_CROP_IMAGES")
            .unwrap_or(config.ocr_return_crop_images);
        config.ocr_need_layout_visualization =
            parse_env_bool("GBRAIN_OCR_NEED_LAYOUT_VISUALIZATION")
                .unwrap_or(config.ocr_need_layout_visualization);
        config.ocr_sync_inline =
            parse_env_bool("GBRAIN_OCR_SYNC_INLINE").unwrap_or(config.ocr_sync_inline);
        if config.ocr_max_pages_per_document == 0 {
            tracing::warn!("ocr_max_pages_per_document=0 无效，已 clamp 到 1");
            config.ocr_max_pages_per_document = 1;
        }
        if config.ocr_max_concurrency == 0 {
            tracing::warn!("ocr_max_concurrency=0 无效，已 clamp 到 1");
            config.ocr_max_concurrency = 1;
        }

        // 注意：load_partial 不调用 apply_required_external_model_env()，
        // 因此不会因缺失外部模型 env 而失败。严格校验留给真正需要外部模型
        // 的调用方（见 load() 与 apply_required_runtime_env_preserving_cli_db）。
        Ok(config)
    }

    /// 完整配置加载：先 `load_partial()`，再执行严格运行时校验
    /// （强制要求所有外部模型 env，如 embedding / expansion / chunker / raptor 等）。
    ///
    /// MCP server / worker / autopilot 等需要完整外部模型配置的入口应继续使用本方法，
    /// 行为与历史一致。CLI 入口改用 `load_partial()`，把严格校验延后到真正需要时。
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let mut config = Self::load_partial()?;

        config
            .apply_required_external_model_env()
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;

        info!(
            db_path = %config.db_path().display(),
            log_level = %config.log_level,
            log_to_file = config.log_to_file,
            log_to_console = config.log_to_console,
            embedding_model = %config.embedding_model,
            embedding_dimensions = config.embedding_dimensions,
            "Configuration loaded"
        );

        Ok(config)
    }

    /// Get the base directory for gbrain data
    pub fn base_dir() -> PathBuf {
        std::env::var("GBRAIN_DIR")
            .map(PathBuf::from)
            .ok()
            .or_else(|| dirs::home_dir().map(|h| h.join(".gbrain")))
            .unwrap_or_else(|| PathBuf::from(".gbrain"))
    }

    /// Get the database path
    pub fn db_path(&self) -> PathBuf {
        self.database_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| Self::base_dir().join("brain.db"))
    }

    /// Get the files storage directory
    pub fn files_dir(&self) -> PathBuf {
        Self::base_dir().join("files")
    }

    /// Get the cache directory
    pub fn cache_dir(&self) -> PathBuf {
        Self::base_dir().join("cache")
    }

    /// 获取 artifact store 目录（统一 resolver）
    ///
    /// 修复：上传用 engine.gbrain_dir()/artifacts（db_path 的父目录），
    /// 备份用 Config::base_dir()/artifacts（~/.gbrain/），两者不一致。
    /// 当用户配置了自定义 database_path 但没配置 artifact_storage_dir 时，
    /// 上传写入 db_path 父目录下的 artifacts，备份却从 ~/.gbrain/artifacts 读取，
    /// 导致原始文件漏备份。
    ///
    /// 统一规则：
    /// 1. 用户显式配置 artifact_storage_dir → 直接使用
    /// 2. 未配置 → 使用 db_path 的父目录 + "artifacts"（与上传一致）
    pub fn artifact_dir(&self) -> PathBuf {
        self.artifact_storage_dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let db_path = self.db_path();
                db_path
                    .parent()
                    .map(|p| p.join("artifacts"))
                    .unwrap_or_else(|| Self::base_dir().join("artifacts"))
            })
    }

    /// P2-7: Get the source of the database URL (mirrors TS getDbUrlSource).
    /// Returns a human-readable string indicating where the DB path comes from:
    /// - "env:GBRAIN_DB_PATH" if set via environment variable
    /// - "config_file" if set in config.json
    /// - "default" if using the default path ($GBRAIN_DIR/brain.db)
    pub fn db_url_source(&self) -> &'static str {
        if std::env::var("GBRAIN_DB_PATH").is_ok() {
            "env:GBRAIN_DB_PATH"
        } else if self.database_path.is_some() {
            // database_path was set but NOT from env (env check above failed),
            // so it must be from config file
            "config_file"
        } else {
            "default"
        }
    }

    // --- Resolved LLM config helpers ---

    /// Resolved API key for query expansion.
    pub fn expansion_api_key_resolved(&self) -> Option<&str> {
        self.expansion_api_key.as_deref()
    }

    /// Resolved base URL for query expansion.
    pub fn expansion_base_url_resolved(&self) -> Option<&str> {
        self.expansion_base_url.as_deref()
    }

    /// Resolved API key for LLM chunker.
    pub fn chunker_api_key_resolved(&self) -> Option<&str> {
        self.chunker_api_key.as_deref()
    }

    /// Resolved base URL for LLM chunker.
    pub fn chunker_base_url_resolved(&self) -> Option<&str> {
        self.chunker_base_url.as_deref()
    }

    /// Resolved API key for transcription.
    pub fn transcription_api_key_resolved(&self) -> Option<&str> {
        match self.transcription_provider.as_str() {
            "openai" => self.transcription_openai_api_key.as_deref(),
            _ => self.transcription_groq_api_key.as_deref(),
        }
    }

    /// Resolved base URL for transcription.
    pub fn transcription_base_url_resolved(&self) -> Option<&str> {
        match self.transcription_provider.as_str() {
            "openai" => self.transcription_openai_base_url.as_deref(),
            _ => self.transcription_groq_base_url.as_deref(),
        }
    }

    /// 解析 RAPTOR LLM 配置。RAPTOR 只读取自己的 GBRAIN_KB_RAPTOR_* 环境变量。
    pub fn raptor_config_resolved(&self) -> crate::error::Result<ResolvedRaptorConfig> {
        let api_key = required_env_value("GBRAIN_KB_RAPTOR_API_KEY")?;
        let base_url = required_env_value("GBRAIN_KB_RAPTOR_BASE_URL")?;
        let model = required_env_value("GBRAIN_KB_RAPTOR_MODEL")?;
        debug!(base_url = %base_url, model = %model, "RAPTOR config resolved");
        Ok(ResolvedRaptorConfig {
            api_key,
            base_url,
            model,
        })
    }

    fn apply_required_external_model_env(&mut self) -> crate::error::Result<()> {
        // 仅在尚未设置 database_path 时才强制要求 GBRAIN_DB_PATH。
        // load_partial 已按优先级填好 database_path（环境变量 GBRAIN_DB_PATH 或 config.json），
        // CLI 的 --db 也会在 run() 中回填到 database_path。此处若再无条件覆盖，
        // 会用 env 抹掉 CLI 指定的 --db，也违背 config.json 中显式配置的 DB 路径。
        if self.database_path.is_none() {
            self.database_path = Some(required_env_value("GBRAIN_DB_PATH")?);
        }

        self.embedding_api_key = Some(required_env_value("GBRAIN_EMBEDDING_API_KEY")?);
        self.embedding_base_url = Some(required_url_env("GBRAIN_EMBEDDING_BASE_URL")?);
        self.embedding_model = required_env_value("GBRAIN_EMBEDDING_MODEL")?;
        self.embedding_dimensions = required_env_usize("GBRAIN_EMBEDDING_DIMENSIONS")?;

        self.expansion_api_key = Some(required_env_value("GBRAIN_EXPANSION_API_KEY")?);
        self.expansion_base_url = Some(required_url_env("GBRAIN_EXPANSION_BASE_URL")?);
        self.expansion_model = required_env_value("GBRAIN_EXPANSION_MODEL")?;

        self.chunker_api_key = Some(required_env_value("GBRAIN_CHUNKER_API_KEY")?);
        self.chunker_base_url = Some(required_url_env("GBRAIN_CHUNKER_BASE_URL")?);
        self.chunker_model = required_env_value("GBRAIN_CHUNKER_MODEL")?;
        if let Some(mode) = optional_env_value("GBRAIN_PAGE_CHUNKER_MODE")? {
            self.page_chunker_mode = mode.to_lowercase();
        }

        required_env_value("GBRAIN_KB_RAPTOR_API_KEY")?;
        self.kb_raptor_secret_ref = Some("GBRAIN_KB_RAPTOR_API_KEY".to_string());
        self.kb_raptor_base_url = Some(required_url_env("GBRAIN_KB_RAPTOR_BASE_URL")?);
        self.kb_raptor_model = required_env_value("GBRAIN_KB_RAPTOR_MODEL")?;

        self.transcription_provider =
            required_env_value("GBRAIN_TRANSCRIPTION_PROVIDER")?.to_lowercase();
        match self.transcription_provider.as_str() {
            "groq" => {
                self.transcription_groq_api_key =
                    Some(required_env_value("GBRAIN_TRANSCRIPTION_GROQ_API_KEY")?);
                self.transcription_groq_base_url =
                    Some(required_url_env("GBRAIN_TRANSCRIPTION_GROQ_BASE_URL")?);
            }
            "openai" => {
                self.transcription_openai_api_key =
                    Some(required_env_value("GBRAIN_TRANSCRIPTION_OPENAI_API_KEY")?);
                self.transcription_openai_base_url =
                    Some(required_url_env("GBRAIN_TRANSCRIPTION_OPENAI_BASE_URL")?);
            }
            other => {
                return Err(config_error(format!(
                    "GBRAIN_TRANSCRIPTION_PROVIDER 无效值 '{}'，有效值: groq, openai",
                    other
                )));
            }
        }
        if let Some(value) = optional_env_value("GBRAIN_TRANSCRIPTION_GROQ_API_KEY")? {
            self.transcription_groq_api_key = Some(value);
        }
        if let Some(value) = optional_url_env("GBRAIN_TRANSCRIPTION_GROQ_BASE_URL")? {
            self.transcription_groq_base_url = Some(value);
        }
        if let Some(value) = optional_env_value("GBRAIN_TRANSCRIPTION_OPENAI_API_KEY")? {
            self.transcription_openai_api_key = Some(value);
        }
        if let Some(value) = optional_url_env("GBRAIN_TRANSCRIPTION_OPENAI_BASE_URL")? {
            self.transcription_openai_base_url = Some(value);
        }

        self.log_to_file = required_env_bool("GBRAIN_LOG_TO_FILE")?;
        self.log_to_console = required_env_bool("GBRAIN_LOG_TO_CONSOLE")?;
        self.log_level = required_env_enum(
            "GBRAIN_LOG_LEVEL",
            &["trace", "debug", "info", "warn", "error"],
        )?;
        if self.log_to_file {
            self.log_file_path = Some(required_env_value("GBRAIN_LOG_FILE_PATH")?);
        }

        self.auto_link = required_env_bool_loose("GBRAIN_AUTO_LINK")?;
        self.auto_timeline = required_env_bool_loose("GBRAIN_AUTO_TIMELINE")?;
        self.post_write_lint = required_env_bool("GBRAIN_POST_WRITE_LINT")?;

        self.kb_enabled = required_env_bool("GBRAIN_KB_ENABLED")?;
        self.kb_max_file_size_mb = required_env_usize("GBRAIN_KB_MAX_FILE_SIZE_MB")?;
        self.kb_allowed_extensions = parse_required_csv("GBRAIN_KB_ALLOWED_EXTENSIONS")?;
        self.kb_storage_dir = Some(required_env_value("GBRAIN_KB_STORAGE_DIR")?);
        self.kb_worker_enabled = required_env_bool("GBRAIN_KB_WORKER_ENABLED")?;
        self.kb_worker_poll_interval_secs = required_env_u64("GBRAIN_KB_WORKER_POLL_INTERVAL")?;
        self.autopilot_enabled = required_env_bool("GBRAIN_AUTOPILOT_ENABLED")?;
        self.autopilot_interval_secs = required_env_u64("GBRAIN_AUTOPILOT_INTERVAL")?;

        self.artifact_storage_dir = Some(required_env_value("GBRAIN_ARTIFACT_STORAGE_DIR")?);
        self.default_kb_library_id = optional_env_i64_allow_empty("GBRAIN_DEFAULT_KB_LIBRARY_ID")?;
        self.upload_default_promotion_policy =
            required_env_value("GBRAIN_UPLOAD_PROMOTION_POLICY")?;
        self.artifact_default_intent = required_env_enum(
            "GBRAIN_ARTIFACT_DEFAULT_INTENT",
            &["memory", "evidence", "promote"],
        )?;
        self.artifact_auto_create_inbox_library =
            required_env_bool_loose("GBRAIN_ARTIFACT_AUTO_CREATE_INBOX_LIBRARY")?;
        self.artifact_manual_memory_to_kb =
            required_env_bool("GBRAIN_ARTIFACT_MANUAL_MEMORY_TO_KB")?;

        self.ocr_enabled = required_env_bool("GBRAIN_OCR_ENABLED")?;
        if self.ocr_enabled {
            self.ocr_api_key = Some(required_env_value("GBRAIN_OCR_API_KEY")?);
            self.ocr_base_url = required_url_env("GBRAIN_OCR_BASE_URL")?;
            self.ocr_model = required_env_value("GBRAIN_OCR_MODEL")?;
        }
        self.ocr_allow_custom_base_url = required_env_bool("GBRAIN_OCR_ALLOW_CUSTOM_BASE_URL")?;
        self.ocr_profile = required_env_enum(
            "GBRAIN_OCR_PROFILE",
            &["auto", "general", "table", "formula", "handwriting"],
        )?;
        self.ocr_enable_layout = required_env_bool("GBRAIN_OCR_ENABLE_LAYOUT")?;
        self.ocr_mode = required_env_enum("GBRAIN_OCR_MODE", &["auto", "all_pages"])?;
        self.ocr_submit_mode =
            required_env_enum("GBRAIN_OCR_SUBMIT_MODE", &["pdf_first", "pdf_range"])?;
        self.ocr_text_density_threshold = required_env_usize("GBRAIN_OCR_TEXT_DENSITY_THRESHOLD")?;
        #[allow(deprecated)]
        {
            self.ocr_min_low_density_ratio = required_env_f64("GBRAIN_OCR_MIN_LOW_DENSITY_RATIO")?;
        }
        self.ocr_image_area_threshold = required_env_f64("GBRAIN_OCR_IMAGE_AREA_THRESHOLD")?;
        self.ocr_image_count_threshold = required_env_usize("GBRAIN_OCR_IMAGE_COUNT_THRESHOLD")?;
        self.ocr_timeout_seconds_per_page =
            required_env_u64("GBRAIN_OCR_TIMEOUT_SECONDS_PER_PAGE")?;
        self.ocr_max_pages_per_request = required_env_usize("GBRAIN_OCR_MAX_PAGES_PER_REQUEST")?;
        self.ocr_max_pdf_bytes_per_request =
            required_env_usize("GBRAIN_OCR_MAX_PDF_BYTES_PER_REQUEST")?;
        self.ocr_max_pages_per_document = required_env_usize("GBRAIN_OCR_MAX_PAGES_PER_DOCUMENT")?;
        self.ocr_max_concurrency = required_env_usize("GBRAIN_OCR_MAX_CONCURRENCY")?;
        self.ocr_temp_dir_max_bytes = required_env_u64("GBRAIN_OCR_TEMP_DIR_MAX_BYTES")?;
        self.ocr_return_crop_images = required_env_bool("GBRAIN_OCR_RETURN_CROP_IMAGES")?;
        self.ocr_need_layout_visualization =
            required_env_bool("GBRAIN_OCR_NEED_LAYOUT_VISUALIZATION")?;
        self.ocr_sync_inline = required_env_bool("GBRAIN_OCR_SYNC_INLINE")?;

        validate_optional_runtime_env()?;
        self.validate_loaded_config()
    }

    /// CLI 运行时严格校验入口：复用 `apply_required_external_model_env` 的全部校验，
    /// 但保留已由 CLI `--db` / 环境变量 `GBRAIN_DB_PATH` / config.json 设置的 `database_path`，
    /// 不再用 `GBRAIN_DB_PATH` 覆盖 CLI 指定的 `--db`（见上方 database_path 守卫）。
    ///
    /// 调用时机：仅在真正需要外部模型 / DB 的命令路径上调用（见 `src/bin/gbrain.rs::run`），
    /// 让 dry-run / config / init 等快速路径可在 partial 环境下运行。
    pub fn apply_required_runtime_env_preserving_cli_db(&mut self) -> crate::error::Result<()> {
        self.apply_required_external_model_env()
    }

    fn validate_loaded_config(&self) -> crate::error::Result<()> {
        required_env_value("GBRAIN_DIR")?;
        validate_nonzero("GBRAIN_EMBEDDING_DIMENSIONS", self.embedding_dimensions)?;
        validate_nonzero("GBRAIN_KB_MAX_FILE_SIZE_MB", self.kb_max_file_size_mb)?;
        validate_nonzero_u64(
            "GBRAIN_KB_WORKER_POLL_INTERVAL",
            self.kb_worker_poll_interval_secs,
        )?;
        if self.autopilot_interval_secs < 60 {
            return Err(config_error(format!(
                "GBRAIN_AUTOPILOT_INTERVAL 最小值为 60 秒，当前值: {}",
                self.autopilot_interval_secs
            )));
        }
        if self.chunk_size == 0 {
            return Err(config_error("chunk_size 必须大于 0"));
        }
        if self.chunk_overlap >= self.chunk_size {
            return Err(config_error(format!(
                "chunk_overlap 必须小于 chunk_size，当前 chunk_overlap={}, chunk_size={}",
                self.chunk_overlap, self.chunk_size
            )));
        }
        validate_enum_value(
            "GBRAIN_PAGE_CHUNKER_MODE",
            &self.page_chunker_mode,
            &["auto", "recursive", "llm"],
        )?;
        if self
            .kb_allowed_extensions
            .iter()
            .any(|s| s.trim().is_empty())
        {
            return Err(config_error(
                "GBRAIN_KB_ALLOWED_EXTENSIONS 不能包含空扩展名",
            ));
        }
        self.upload_default_promotion_policy
            .parse::<crate::artifact::types::PromotionPolicy>()
            .map_err(|_| {
                config_error(format!(
                    "GBRAIN_UPLOAD_PROMOTION_POLICY 无效值 '{}'，有效值: none, shadow, candidate, auto, auto-low-risk, auto_accept_low_risk, auto-apply, auto_apply, auto_all, auto-all, auto-apply-all",
                    self.upload_default_promotion_policy
                ))
            })?;
        if !(0.0..=1.0).contains(&self.ocr_image_area_threshold) {
            return Err(config_error(format!(
                "GBRAIN_OCR_IMAGE_AREA_THRESHOLD 必须在 0..=1 之间，当前值: {}",
                self.ocr_image_area_threshold
            )));
        }
        #[allow(deprecated)]
        {
            let ratio = self.ocr_min_low_density_ratio;
            if !(0.0..=1.0).contains(&ratio) {
                return Err(config_error(format!(
                    "GBRAIN_OCR_MIN_LOW_DENSITY_RATIO 必须在 0..=1 之间，当前值: {}",
                    ratio
                )));
            }
        }
        validate_nonzero(
            "GBRAIN_OCR_IMAGE_COUNT_THRESHOLD",
            self.ocr_image_count_threshold,
        )?;
        validate_nonzero_u64(
            "GBRAIN_OCR_TIMEOUT_SECONDS_PER_PAGE",
            self.ocr_timeout_seconds_per_page,
        )?;
        validate_nonzero(
            "GBRAIN_OCR_MAX_PAGES_PER_REQUEST",
            self.ocr_max_pages_per_request,
        )?;
        validate_nonzero(
            "GBRAIN_OCR_MAX_PDF_BYTES_PER_REQUEST",
            self.ocr_max_pdf_bytes_per_request,
        )?;
        validate_nonzero(
            "GBRAIN_OCR_MAX_PAGES_PER_DOCUMENT",
            self.ocr_max_pages_per_document,
        )?;
        validate_nonzero("GBRAIN_OCR_MAX_CONCURRENCY", self.ocr_max_concurrency)?;
        validate_nonzero_u64("GBRAIN_OCR_TEMP_DIR_MAX_BYTES", self.ocr_temp_dir_max_bytes)?;
        Ok(())
    }

    /// Save configuration to config.json with restrictive permissions (0o600 on Unix).
    /// Mirrors TS writeConfig() which uses mode: 0o600.
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let config_path = Self::base_dir().join("config.json");
        std::fs::create_dir_all(Self::base_dir())?;
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&config_path, json)?;
        // Set restrictive permissions on Unix (owner read/write only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&config_path, perms)?;
        }
        info!(path = %config_path.display(), "Saved config.json");
        Ok(())
    }

    /// 根据 key 读取配置值（用于 `gbrain config get`）。
    /// 返回 Some(value) 表示 key 是 Config 字段，None 表示 key 是 SQLite engine 专用 key。
    pub fn get_field(&self, key: &str) -> Option<String> {
        match key {
            "embedding_model" => Some(self.embedding_model.clone()),
            "embedding_dimensions" => Some(self.embedding_dimensions.to_string()),
            "expansion_model" => Some(self.expansion_model.clone()),
            "chunker_model" => Some(self.chunker_model.clone()),
            "page_chunker_mode" => Some(self.page_chunker_mode.clone()),
            "chunk_size" => Some(self.chunk_size.to_string()),
            "chunk_overlap" => Some(self.chunk_overlap.to_string()),
            "log_level" => Some(self.log_level.clone()),
            "log_to_file" => Some(self.log_to_file.to_string()),
            "log_to_console" => Some(self.log_to_console.to_string()),
            "auto_link" => Some(self.auto_link.to_string()),
            "auto_timeline" => Some(self.auto_timeline.to_string()),
            "post_write_lint" => Some(self.post_write_lint.to_string()),
            "kb_enabled" => Some(self.kb_enabled.to_string()),
            "kb_raptor_model" => Some(self.kb_raptor_model.clone()),
            "kb_max_file_size_mb" => Some(self.kb_max_file_size_mb.to_string()),
            "kb_worker_enabled" => Some(self.kb_worker_enabled.to_string()),
            "kb_worker_poll_interval_secs" => Some(self.kb_worker_poll_interval_secs.to_string()),
            "autopilot_enabled" => Some(self.autopilot_enabled.to_string()),
            "autopilot_interval_secs" => Some(self.autopilot_interval_secs.to_string()),
            "upload_default_promotion_policy" => Some(self.upload_default_promotion_policy.clone()),
            "artifact_default_intent" => Some(self.artifact_default_intent.clone()),
            "artifact_auto_create_inbox_library" => {
                Some(self.artifact_auto_create_inbox_library.to_string())
            }
            "artifact_manual_memory_to_kb" => Some(self.artifact_manual_memory_to_kb.to_string()),
            // OCR 子系统
            "ocr_enabled" => Some(self.ocr_enabled.to_string()),
            "ocr_base_url" => Some(self.ocr_base_url.clone()),
            "ocr_model" => Some(self.ocr_model.clone()),
            "ocr_mode" => Some(self.ocr_mode.clone()),
            "ocr_submit_mode" => Some(self.ocr_submit_mode.clone()),
            "ocr_profile" => Some(self.ocr_profile.clone()),
            // SQLite engine 专用 key，不在 Config 中
            "writer.lint_on_put_page" => None,
            _ => None,
        }
    }

    /// 根据 key 设置配置值（用于 `gbrain config set`）。
    /// 返回 Ok(()) 表示成功设置，Err(msg) 表示 key 未识别或值无效。
    pub fn apply_set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "embedding_model" => self.embedding_model = value.to_string(),
            "embedding_dimensions" => {
                let v: usize = value
                    .parse()
                    .map_err(|_| format!("{} 需要整数，不是: {}", key, value))?;
                self.embedding_dimensions = v;
            }
            "expansion_model" => self.expansion_model = value.to_string(),
            "chunker_model" => self.chunker_model = value.to_string(),
            "page_chunker_mode" => {
                let normalized = value.to_lowercase();
                validate_enum_value(
                    "page_chunker_mode",
                    &normalized,
                    &["auto", "recursive", "llm"],
                )
                .map_err(|e| e.to_string())?;
                self.page_chunker_mode = normalized;
            }
            "chunk_size" => {
                let v: usize = value
                    .parse()
                    .map_err(|_| format!("{} 需要整数，不是: {}", key, value))?;
                self.chunk_size = v;
            }
            "chunk_overlap" => {
                let v: usize = value
                    .parse()
                    .map_err(|_| format!("{} 需要整数，不是: {}", key, value))?;
                self.chunk_overlap = v;
            }
            "log_level" => {
                let valid = ["trace", "debug", "info", "warn", "error"];
                if !valid.contains(&value) {
                    return Err(format!(
                        "log_level 无效值: {}，有效值: {}",
                        value,
                        valid.join(", ")
                    ));
                }
                self.log_level = value.to_string();
            }
            "log_to_file" => self.log_to_file = parse_bool(key, value)?,
            "log_to_console" => self.log_to_console = parse_bool(key, value)?,
            "auto_link" => self.auto_link = parse_bool(key, value)?,
            "auto_timeline" => self.auto_timeline = parse_bool(key, value)?,
            "post_write_lint" => self.post_write_lint = parse_bool(key, value)?,
            "kb_enabled" => self.kb_enabled = parse_bool(key, value)?,
            "kb_raptor_model" => self.kb_raptor_model = value.to_string(),
            "kb_max_file_size_mb" => {
                let v: usize = value
                    .parse()
                    .map_err(|_| format!("{} 需要整数，不是: {}", key, value))?;
                self.kb_max_file_size_mb = v;
            }
            "kb_worker_enabled" => self.kb_worker_enabled = parse_bool(key, value)?,
            "kb_worker_poll_interval_secs" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| format!("{} 需要整数，不是: {}", key, value))?;
                self.kb_worker_poll_interval_secs = v;
            }
            "autopilot_enabled" => self.autopilot_enabled = parse_bool(key, value)?,
            "autopilot_interval_secs" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| format!("{} 需要整数，不是: {}", key, value))?;
                if v < 60 {
                    return Err(format!(
                        "autopilot_interval_secs 最小值为 60 秒，当前值: {}",
                        v
                    ));
                }
                self.autopilot_interval_secs = v;
            }
            "upload_default_promotion_policy" => {
                // 复用 PromotionPolicy 解析器，避免配置入口与 CLI/MCP 支持的别名漂移。
                if value
                    .parse::<crate::artifact::types::PromotionPolicy>()
                    .is_err()
                {
                    return Err(format!(
                        "upload_default_promotion_policy 无效值: {}，有效值: {}",
                        value,
                        "none, shadow, candidate, auto, auto-low-risk, auto_accept_low_risk, auto-apply, auto_apply, auto_all, auto-all, auto-apply-all"
                    ));
                }
                self.upload_default_promotion_policy = value.to_string()
            }
            "artifact_default_intent" => {
                // 校验合法枚举值，避免无效 intent 在 routing 被静默退回 Auto
                let valid = ["memory", "evidence", "promote"];
                if !valid.contains(&value) {
                    return Err(format!(
                        "artifact_default_intent 无效值: {}，有效值: {}",
                        value,
                        valid.join(", ")
                    ));
                }
                self.artifact_default_intent = value.to_string()
            }
            "artifact_auto_create_inbox_library" => {
                self.artifact_auto_create_inbox_library = parse_bool(key, value)?
            }
            "artifact_manual_memory_to_kb" => {
                self.artifact_manual_memory_to_kb = parse_bool(key, value)?
            }
            // OCR 子系统配置
            "ocr_enabled" => self.ocr_enabled = parse_bool(key, value)?,
            "ocr_model" => self.ocr_model = value.to_string(),
            "ocr_mode" => {
                let valid = ["auto", "all_pages"];
                if !valid.contains(&value) {
                    return Err(format!(
                        "ocr_mode 无效值: {}，有效值: {}",
                        value,
                        valid.join(", ")
                    ));
                }
                self.ocr_mode = value.to_string();
            }
            "ocr_submit_mode" => {
                let valid = ["pdf_first", "pdf_range"];
                if !valid.contains(&value) {
                    return Err(format!(
                        "ocr_submit_mode 无效值: {}，有效值: {}",
                        value,
                        valid.join(", ")
                    ));
                }
                self.ocr_submit_mode = value.to_string();
            }
            "ocr_profile" => {
                let valid = ["auto", "general", "table", "formula", "handwriting"];
                if !valid.contains(&value) {
                    return Err(format!(
                        "ocr_profile 无效值: {}，有效值: {}",
                        value,
                        valid.join(", ")
                    ));
                }
                self.ocr_profile = value.to_string();
            }
            "ocr_base_url" => self.ocr_base_url = value.to_string(),
            "ocr_text_density_threshold" => {
                let v: usize = value
                    .parse()
                    .map_err(|_| format!("{} 需要整数，不是: {}", key, value))?;
                self.ocr_text_density_threshold = v;
            }
            "ocr_timeout_seconds_per_page" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| format!("{} 需要整数，不是: {}", key, value))?;
                self.ocr_timeout_seconds_per_page = v;
            }
            // SQLite engine 专用 key（保留兼容），不在 Config 中，仅写入 DB
            "writer.lint_on_put_page" => {
                return Err(format!(
                    "{} 是 SQLite engine 专用 key，需连接数据库后才能设置",
                    key
                ))
            }
            _ => return Err(format!("未知配置 key: {}", key)),
        }
        info!(key = %key, value = %value, "Config key changed via apply_set");
        Ok(())
    }

    /// Merge another config into this one (other takes precedence for Some values)
    fn merge(&mut self, other: Config) {
        if other.database_path.is_some() {
            trace!("merge: overriding database_path");
            self.database_path = other.database_path;
        }
        if other.embedding_api_key.is_some() {
            self.embedding_api_key = other.embedding_api_key;
        }
        if other.embedding_base_url.is_some() {
            self.embedding_base_url = other.embedding_base_url;
        }
        // Always take config file values for non-Option fields (they represent
        // explicit user choices, even if they match defaults)
        trace!("merge: overriding embedding_model/embedding_dimensions");
        self.embedding_model = other.embedding_model;
        self.embedding_dimensions = other.embedding_dimensions;
        if other.expansion_api_key.is_some() {
            self.expansion_api_key = other.expansion_api_key;
        }
        if other.expansion_base_url.is_some() {
            self.expansion_base_url = other.expansion_base_url;
        }
        self.expansion_model = other.expansion_model;
        if other.chunker_api_key.is_some() {
            self.chunker_api_key = other.chunker_api_key;
        }
        if other.chunker_base_url.is_some() {
            self.chunker_base_url = other.chunker_base_url;
        }
        self.chunker_model = other.chunker_model;
        self.page_chunker_mode = other.page_chunker_mode;
        self.transcription_provider = other.transcription_provider;
        if other.transcription_groq_api_key.is_some() {
            self.transcription_groq_api_key = other.transcription_groq_api_key;
        }
        if other.transcription_groq_base_url.is_some() {
            self.transcription_groq_base_url = other.transcription_groq_base_url;
        }
        if other.transcription_openai_api_key.is_some() {
            self.transcription_openai_api_key = other.transcription_openai_api_key;
        }
        if other.transcription_openai_base_url.is_some() {
            self.transcription_openai_base_url = other.transcription_openai_base_url;
        }
        self.log_level = other.log_level;
        // Always take config file values for booleans (explicit choice, not magic comparison)
        self.log_to_file = other.log_to_file;
        if other.log_file_path.is_some() {
            self.log_file_path = other.log_file_path;
        }
        self.log_to_console = other.log_to_console;
        // P2-6: auto_link / auto_timeline — always take config file values
        self.auto_link = other.auto_link;
        self.auto_timeline = other.auto_timeline;
        // P2-3: post_write_lint — always take config file value
        self.post_write_lint = other.post_write_lint;
        // wal_mode and pool_size — always take config file values
        self.wal_mode = other.wal_mode;
        self.pool_size = other.pool_size;
        // chunk_size and chunk_overlap — always take config file values
        self.chunk_size = other.chunk_size;
        self.chunk_overlap = other.chunk_overlap;
        // KB subsystem — always take config file values
        self.kb_enabled = other.kb_enabled;
        if other.kb_raptor_secret_ref.is_some() {
            self.kb_raptor_secret_ref = other.kb_raptor_secret_ref;
        }
        if other.kb_raptor_base_url.is_some() {
            self.kb_raptor_base_url = other.kb_raptor_base_url;
        }
        self.kb_raptor_model = other.kb_raptor_model;
        self.kb_max_file_size_mb = other.kb_max_file_size_mb;
        if !other.kb_allowed_extensions.is_empty() {
            self.kb_allowed_extensions = other.kb_allowed_extensions;
        }
        if other.kb_storage_dir.is_some() {
            self.kb_storage_dir = other.kb_storage_dir;
        }
        self.kb_worker_enabled = other.kb_worker_enabled;
        self.kb_worker_poll_interval_secs = other.kb_worker_poll_interval_secs;
        self.autopilot_enabled = other.autopilot_enabled;
        self.autopilot_interval_secs = other.autopilot_interval_secs;
        if other.kb_synonyms_file.is_some() {
            self.kb_synonyms_file = other.kb_synonyms_file.clone();
        }
        if other.kb_aliases_file.is_some() {
            self.kb_aliases_file = other.kb_aliases_file.clone();
        }
        // 单入口多投影融合架构 — config file values
        if other.artifact_storage_dir.is_some() {
            self.artifact_storage_dir = other.artifact_storage_dir;
        }
        if other.default_kb_library_id.is_some() {
            self.default_kb_library_id = other.default_kb_library_id;
        }
        if !other.upload_default_promotion_policy.is_empty() {
            self.upload_default_promotion_policy = other.upload_default_promotion_policy;
        }
        // artifact_default_intent — always take config file value
        if !other.artifact_default_intent.is_empty() {
            self.artifact_default_intent = other.artifact_default_intent;
        }
        // artifact_auto_create_inbox_library — always take config file value
        self.artifact_auto_create_inbox_library = other.artifact_auto_create_inbox_library;
        // artifact_manual_memory_to_kb — always take config file value
        self.artifact_manual_memory_to_kb = other.artifact_manual_memory_to_kb;
        // OCR 子系统 — always take config file values
        self.ocr_enabled = other.ocr_enabled;
        // ocr_api_key 不从配置文件合并，只从环境变量读取
        if !other.ocr_base_url.is_empty() {
            self.ocr_base_url = other.ocr_base_url;
        }
        // ocr_allow_custom_base_url 不从配置文件合并（仅环境变量控制的安全开关）
        if !other.ocr_model.is_empty() {
            self.ocr_model = other.ocr_model;
        }
        if !other.ocr_profile.is_empty() {
            self.ocr_profile = other.ocr_profile;
        }
        self.ocr_enable_layout = other.ocr_enable_layout;
        if !other.ocr_mode.is_empty() {
            self.ocr_mode = other.ocr_mode;
        }
        if !other.ocr_submit_mode.is_empty() {
            self.ocr_submit_mode = other.ocr_submit_mode;
        }
        self.ocr_text_density_threshold = other.ocr_text_density_threshold;
        #[allow(deprecated)]
        {
            self.ocr_min_low_density_ratio = other.ocr_min_low_density_ratio;
        }
        self.ocr_image_area_threshold = other.ocr_image_area_threshold;
        self.ocr_image_count_threshold = other.ocr_image_count_threshold;
        self.ocr_timeout_seconds_per_page = other.ocr_timeout_seconds_per_page;
        self.ocr_max_pages_per_request = other.ocr_max_pages_per_request;
        self.ocr_max_pdf_bytes_per_request = other.ocr_max_pdf_bytes_per_request;
        self.ocr_max_pages_per_document = other.ocr_max_pages_per_document;
        self.ocr_max_concurrency = other.ocr_max_concurrency;
        self.ocr_temp_dir_max_bytes = other.ocr_temp_dir_max_bytes;
        self.ocr_return_crop_images = other.ocr_return_crop_images;
        self.ocr_need_layout_visualization = other.ocr_need_layout_visualization;
        self.ocr_sync_inline = other.ocr_sync_inline;
    }
}

fn config_error(message: impl Into<String>) -> GBrainError {
    GBrainError::Config(message.into())
}

fn required_env_value(name: &str) -> crate::error::Result<String> {
    match std::env::var(name) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Err(config_error(format!(
                    "缺少必需环境变量 {}：值不能为空",
                    name
                )))
            } else {
                Ok(trimmed.to_string())
            }
        }
        Err(std::env::VarError::NotPresent) => {
            Err(config_error(format!("缺少必需环境变量 {}", name)))
        }
        Err(e) => Err(config_error(format!("读取环境变量 {} 失败: {}", name, e))),
    }
}

fn optional_env_value(name: &str) -> crate::error::Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Err(config_error(format!("环境变量 {} 已配置但值为空", name)))
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(config_error(format!("读取环境变量 {} 失败: {}", name, e))),
    }
}

fn required_url_env(name: &str) -> crate::error::Result<String> {
    let value = required_env_value(name)?;
    validate_url(name, &value)?;
    Ok(value)
}

fn optional_url_env(name: &str) -> crate::error::Result<Option<String>> {
    let Some(value) = optional_env_value(name)? else {
        return Ok(None);
    };
    validate_url(name, &value)?;
    Ok(Some(value))
}

fn validate_url(name: &str, value: &str) -> crate::error::Result<()> {
    let url = reqwest::Url::parse(value).map_err(|e| {
        config_error(format!(
            "{} 必须是合法 URL，当前值 '{}': {}",
            name, value, e
        ))
    })?;
    match url.scheme() {
        "http" | "https" => Ok(()),
        scheme => Err(config_error(format!(
            "{} 只支持 http/https URL，当前 scheme: {}",
            name, scheme
        ))),
    }
}

fn required_env_bool(name: &str) -> crate::error::Result<bool> {
    parse_env_bool_str(name, &required_env_value(name)?)
}

fn required_env_bool_loose(name: &str) -> crate::error::Result<bool> {
    match required_env_value(name)?.as_str() {
        "false" | "0" => Ok(false),
        "true" | "1" => Ok(true),
        other => Err(config_error(format!(
            "{} 需要布尔值 true/false/1/0，当前值: {}",
            name, other
        ))),
    }
}

fn parse_env_bool_str(name: &str, value: &str) -> crate::error::Result<bool> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(config_error(format!(
            "{} 需要布尔值 true/false/1/0，当前值: {}",
            name, value
        ))),
    }
}

fn required_env_usize(name: &str) -> crate::error::Result<usize> {
    let value = required_env_value(name)?;
    value
        .parse::<usize>()
        .map_err(|e| config_error(format!("{} 需要非负整数，当前值 '{}': {}", name, value, e)))
}

fn required_env_u64(name: &str) -> crate::error::Result<u64> {
    let value = required_env_value(name)?;
    value
        .parse::<u64>()
        .map_err(|e| config_error(format!("{} 需要非负整数，当前值 '{}': {}", name, value, e)))
}

fn required_env_f64(name: &str) -> crate::error::Result<f64> {
    let value = required_env_value(name)?;
    let parsed = value
        .parse::<f64>()
        .map_err(|e| config_error(format!("{} 需要数字，当前值 '{}': {}", name, value, e)))?;
    if parsed.is_finite() {
        Ok(parsed)
    } else {
        Err(config_error(format!(
            "{} 需要有限数字，当前值: {}",
            name, value
        )))
    }
}

fn required_env_enum(name: &str, valid: &[&str]) -> crate::error::Result<String> {
    let value = required_env_value(name)?;
    validate_enum_value(name, &value, valid)?;
    Ok(value)
}

fn validate_enum_value(name: &str, value: &str, valid: &[&str]) -> crate::error::Result<()> {
    if valid.contains(&value) {
        Ok(())
    } else {
        Err(config_error(format!(
            "{} 无效值 '{}'，有效值: {}",
            name,
            value,
            valid.join(", ")
        )))
    }
}

fn parse_required_csv(name: &str) -> crate::error::Result<Vec<String>> {
    let value = required_env_value(name)?;
    let items: Vec<String> = value.split(',').map(|s| s.trim().to_string()).collect();
    if items.is_empty() || items.iter().any(|s| s.is_empty()) {
        return Err(config_error(format!("{} 必须是非空逗号分隔列表", name)));
    }
    Ok(items)
}

fn optional_env_i64_allow_empty(name: &str) -> crate::error::Result<Option<i64>> {
    match std::env::var(name) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            let parsed = trimmed.parse::<i64>().map_err(|e| {
                config_error(format!("{} 需要整数，当前值 '{}': {}", name, trimmed, e))
            })?;
            if parsed <= 0 {
                return Err(config_error(format!(
                    "{} 必须大于 0，当前值: {}",
                    name, parsed
                )));
            }
            Ok(Some(parsed))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(config_error(format!("读取环境变量 {} 失败: {}", name, e))),
    }
}

fn validate_nonzero(name: &str, value: usize) -> crate::error::Result<()> {
    if value == 0 {
        Err(config_error(format!("{} 必须大于 0", name)))
    } else {
        Ok(())
    }
}

fn validate_nonzero_u64(name: &str, value: u64) -> crate::error::Result<()> {
    if value == 0 {
        Err(config_error(format!("{} 必须大于 0", name)))
    } else {
        Ok(())
    }
}

fn validate_optional_runtime_env() -> crate::error::Result<()> {
    if let Ok(value) = std::env::var("GBRAIN_RUST_LOG") {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(config_error("GBRAIN_RUST_LOG 已配置但值为空"));
        }
        tracing_subscriber::EnvFilter::try_new(trimmed)
            .map_err(|e| config_error(format!("GBRAIN_RUST_LOG 无效值 '{}': {}", trimmed, e)))?;
    }

    if let Ok(value) = std::env::var("GBRAIN_PROGRESS_MODE") {
        let trimmed = value.trim();
        if !matches!(trimmed, "auto" | "human" | "json" | "quiet") {
            return Err(config_error(format!(
                "GBRAIN_PROGRESS_MODE 无效值 '{}'，有效值: auto, human, json, quiet",
                trimmed
            )));
        }
    }
    if let Ok(value) = std::env::var("GBRAIN_PROGRESS_JSON") {
        parse_env_bool_str("GBRAIN_PROGRESS_JSON", value.trim())?;
    }
    if let Ok(value) = std::env::var("GBRAIN_ASYNC_WORKER_THREADS") {
        let trimmed = value.trim();
        let threads = trimmed.parse::<usize>().map_err(|e| {
            config_error(format!(
                "GBRAIN_ASYNC_WORKER_THREADS 需要正整数，当前值 '{}': {}",
                trimmed, e
            ))
        })?;
        validate_nonzero("GBRAIN_ASYNC_WORKER_THREADS", threads)?;
    }
    if let Ok(value) = std::env::var("GBRAIN_SEARCH_DEBUG") {
        parse_env_bool_str("GBRAIN_SEARCH_DEBUG", value.trim())?;
    }
    if let Ok(value) = std::env::var("GBRAIN_SOURCE_BOOST") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            for pair in trimmed.split(',') {
                let Some((prefix, factor)) = pair.rsplit_once(':') else {
                    return Err(config_error(format!(
                        "GBRAIN_SOURCE_BOOST 条目 '{}' 无效，格式应为 前缀:系数",
                        pair
                    )));
                };
                if prefix.trim().is_empty() {
                    return Err(config_error("GBRAIN_SOURCE_BOOST 不能包含空前缀"));
                }
                let factor = factor.trim().parse::<f64>().map_err(|e| {
                    config_error(format!(
                        "GBRAIN_SOURCE_BOOST 系数 '{}' 无效: {}",
                        factor.trim(),
                        e
                    ))
                })?;
                if !factor.is_finite() || factor < 0.0 {
                    return Err(config_error(format!(
                        "GBRAIN_SOURCE_BOOST 系数必须是非负有限数，当前值: {}",
                        factor
                    )));
                }
            }
        }
    }
    Ok(())
}

/// 按优先级依次检查多个环境变量名，返回第一个有效 u64 的值。
/// 空字符串或无效值会跳过并记录 warning，继续尝试下一个别名。
fn first_valid_env_u64(names: &[&str]) -> Option<u64> {
    for &name in names {
        if let Ok(val) = std::env::var(name) {
            let trimmed = val.trim();
            if trimmed.is_empty() {
                continue;
            }
            match trimmed.parse::<u64>() {
                Ok(v) => return Some(v),
                Err(e) => {
                    tracing::warn!("{} 无效值 '{}': {}，继续尝试后续别名", name, trimmed, e);
                    continue;
                }
            }
        }
    }
    None
}

/// 按优先级依次检查多个环境变量名，返回第一个有效 f64 的值。
fn first_valid_env_f64(names: &[&str]) -> Option<f64> {
    for &name in names {
        if let Ok(val) = std::env::var(name) {
            let trimmed = val.trim();
            if trimmed.is_empty() {
                continue;
            }
            match trimmed.parse::<f64>() {
                Ok(v) => return Some(v),
                Err(e) => {
                    tracing::warn!("{} 无效值 '{}': {}，继续尝试后续别名", name, trimmed, e);
                    continue;
                }
            }
        }
    }
    None
}

pub(crate) fn parse_env_bool(var_name: &str) -> Option<bool> {
    let val = std::env::var(var_name).ok()?;
    match val.trim().to_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => {
            tracing::warn!(
                "{} 无效值 '{}'，有效值: true/false/1/0，已忽略",
                var_name,
                val
            );
            None
        }
    }
}

/// 严格解析布尔值，只接受 true/false/1/0，拒绝拼写错误。
/// 用于 `gbrain config set` 的参数校验，防止 `flase` 被静默视为 true。
fn parse_bool(key: &str, value: &str) -> Result<bool, String> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(format!(
            "{} 需要布尔值 (true/false/1/0)，不是: {}",
            key, value
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_default_promotion_policy_set_accepts_parser_aliases() {
        let aliases = [
            "none",
            "shadow",
            "candidate",
            "auto",
            "auto-low-risk",
            "auto_accept_low_risk",
            "auto-apply",
            "auto_apply",
            "auto_all",
            "auto-all",
            "auto-apply-all",
        ];

        let mut config = Config::default();
        for alias in aliases {
            config
                .apply_set("upload_default_promotion_policy", alias)
                .expect("promotion policy alias should be accepted");
            assert_eq!(config.upload_default_promotion_policy, alias);
        }

        assert!(config
            .apply_set("upload_default_promotion_policy", "not-a-policy")
            .is_err());
    }
}
