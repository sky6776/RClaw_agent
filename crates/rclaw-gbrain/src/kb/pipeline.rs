//! KB 文档处理管道
//!
//! 5阶段异步管道: 解析 → 分割 → 嵌入 → RAPTOR → 持久化
//!
//! 每个阶段通过可选的 `on_progress` 回调报告进度。

use crate::embedding::Embedder;
use crate::error::{GBrainError, Result};
use crate::kb::engine::KbEngine;
use crate::kb::jobs::KbProcessPayload;

/// Block span tuple: (title_path, page_number, source_start, source_end).
type BlockSpan = (String, Option<i32>, Option<i32>, Option<i32>);
use crate::kb::parser::{ParsedDocument, ParserRegistry};
use crate::kb::raptor::{self, RaptorConfig};
use crate::kb::splitter::{create_async_splitter, create_splitter, SplitterConfig};
use crate::kb::types::*;
use crate::nlp::chinese;
use futures_util::stream::{FuturesUnordered, StreamExt};
use rusqlite::Connection;
use std::path::Path;
use std::sync::Arc;

// M-8 修复：MAX_OCR_IMAGE_BYTES 统一定义在 ocr.rs，此处引用
use crate::kb::ocr::{OcrStatus, MAX_OCR_IMAGE_BYTES};
const OCR_IMAGE_TOTAL_PAGES: i32 = 1;
const EMBEDDED_IMAGE_OCR_SECTION_TITLE: &str = "Embedded Image OCR";
const AUGMENT_MAX_CONCURRENCY: usize = 8;
const AUGMENT_PROGRESS_EVERY_NODES: usize = 8;
const AUGMENT_PROGRESS_START: i32 = 30;
const AUGMENT_PROGRESS_END: i32 = 70;

fn augment_concurrency_for(node_count: usize) -> usize {
    match node_count {
        0 | 1 => 1,
        2..=16 => 2,
        17..=64 => 4,
        65..=128 => 6,
        _ => AUGMENT_MAX_CONCURRENCY,
    }
}

struct AugmentTaskResult {
    index: usize,
    input_chars: i32,
    latency_ms: i32,
    result: std::result::Result<Option<crate::kb::augment::ChunkAugmentation>, String>,
}

async fn run_augment_task(
    index: usize,
    content: String,
    api_key: String,
    base_url: String,
    model: String,
) -> AugmentTaskResult {
    let input_chars = content.chars().count() as i32;
    let start = std::time::Instant::now();
    let result = crate::kb::augment::augment_chunk(&content, &api_key, &base_url, &model)
        .await
        .map_err(|e| e.to_string());
    AugmentTaskResult {
        index,
        input_chars,
        latency_ms: start.elapsed().as_millis() as i32,
        result,
    }
}

fn is_ocr_image_extension(ext: &str) -> bool {
    crate::artifact::types::is_ocr_image_file(&ext.to_lowercase())
}

/// 稳定标题源解析：优先使用格式解析提取的 title，其次使用 original_name（去掉扩展名）。
/// 同步和异步管道共享此逻辑，避免重复代码。
fn resolve_doc_title<'a>(format_title: &'a str, original_name: &str) -> std::borrow::Cow<'a, str> {
    if !format_title.is_empty() {
        std::borrow::Cow::Borrowed(format_title)
    } else if !original_name.is_empty() {
        // 去掉扩展名作为标题
        std::borrow::Cow::Owned(
            original_name
                .rsplit_once('.')
                .map(|(name, _)| name.to_string())
                .unwrap_or_else(|| original_name.to_string()),
        )
    } else {
        std::borrow::Cow::Borrowed("")
    }
}

/// 节点元数据中的分块策略标记
#[derive(Debug, Clone, Copy, Default)]
pub struct ChunkMeta {
    /// 分块策略："adaptive"
    pub strategy: &'static str,
    /// 段落类型：heading_section / paragraph / table / code / image_ocr / pdf_page / whole_document
    pub segment_kind: &'static str,
    /// 是否经过语义细分
    pub semantic_refined: bool,
    /// 内容模态：text / image_ocr / table / pdf_page / code
    pub modality: &'static str,
}

pub(crate) fn serialize_node_metadata_ex(
    media_refs: &[MediaRef],
    node_type: Option<&str>,
    include_media_refs: bool,
    chunk_meta: Option<ChunkMeta>,
) -> String {
    let mut obj = serde_json::Map::new();
    if let Some(node_type) = node_type {
        obj.insert(
            "node_type".to_string(),
            serde_json::Value::String(node_type.to_string()),
        );
    }
    if include_media_refs || !media_refs.is_empty() {
        obj.insert(
            "media_refs".to_string(),
            serde_json::to_value(media_refs).unwrap_or_else(|_| serde_json::json!([])),
        );
    }
    // P2: 写入自适应分块元数据标记
    if let Some(meta) = chunk_meta {
        obj.insert(
            "chunk_strategy".to_string(),
            serde_json::Value::String(meta.strategy.to_string()),
        );
        obj.insert(
            "segment_kind".to_string(),
            serde_json::Value::String(meta.segment_kind.to_string()),
        );
        if meta.semantic_refined {
            obj.insert(
                "semantic_refined".to_string(),
                serde_json::Value::Bool(true),
            );
        }
        obj.insert(
            "modality".to_string(),
            serde_json::Value::String(meta.modality.to_string()),
        );
    }
    if obj.is_empty() {
        String::new()
    } else {
        serde_json::Value::Object(obj).to_string()
    }
}

/// P2: 根据文件扩展名推断内容模态
fn infer_modality(ext: &str) -> &'static str {
    match ext {
        "pdf" => "pdf_page",
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "tiff" | "webp" => "image_ocr",
        "csv" | "tsv" | "xlsx" | "xls" => "table",
        // 代码文件（P4: 列表与 infer_segment_kind / is_code_extension 对齐）
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "java" | "c" | "cpp" | "h" | "hpp"
        | "rb" | "php" | "sh" | "bash" | "zsh" | "sql" | "toml" | "yaml" | "yml" | "json"
        | "xml" => "code",
        _ => "text",
    }
}

/// P2: 根据文件扩展名和 block 信息推断段落类型
fn infer_segment_kind(ext: &str, block_spans: &[BlockSpan]) -> &'static str {
    // 表格文件
    if matches!(ext, "csv" | "tsv" | "xlsx" | "xls") {
        return "table";
    }
    // 代码文件（P4: 扩展列表与 adaptive.rs is_code_extension 对齐）
    if matches!(
        ext,
        "rs" | "py"
            | "js"
            | "ts"
            | "tsx"
            | "jsx"
            | "go"
            | "java"
            | "c"
            | "cpp"
            | "h"
            | "hpp"
            | "rb"
            | "php"
            | "sh"
            | "bash"
            | "zsh"
            | "sql"
            | "toml"
            | "yaml"
            | "yml"
            | "json"
            | "xml"
    ) {
        return "code";
    }
    // Markdown 文件：始终标记为 heading_section（M1 修复）
    if matches!(ext, "md" | "markdown") {
        return "heading_section";
    }
    // PDF 按页
    if ext == "pdf" {
        return "pdf_page";
    }
    // 图片 OCR
    if matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "tiff" | "webp"
    ) {
        return "image_ocr";
    }
    // 有标题路径的块 → 标题段落（HTML/DOCX 等可能有结构化标题）
    if !block_spans.is_empty() && block_spans.iter().any(|(tp, _, _, _)| !tp.is_empty()) {
        return "heading_section";
    }
    "paragraph"
}

/// 无页码媒体的文本探针（storage_path / alt / caption / OCR 文本），trim + 小写，与
/// `media_refs` 一一对应；有页码的媒体按页对齐、不需要探针，留空。
///
/// 每个文档只算一次：原先每个 (chunk, media) 组合都重新 `to_lowercase()` 整块 chunk 和
/// OCR 文本，几百张图 × 几万块就是上百 GB 的小写化，唯一的索引 worker 会被钉死数小时。
fn build_media_probes(media_refs: &[MediaRef]) -> Vec<Vec<String>> {
    media_refs
        .iter()
        .map(|media| {
            if media.page_number.is_some() {
                return Vec::new();
            }
            [
                Some(media.storage_path.as_str()),
                media.alt_text.as_deref(),
                media.caption.as_deref(),
                media.ocr_text.as_deref(),
            ]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|probe| !probe.is_empty())
            .map(str::to_lowercase)
            .collect()
        })
        .collect()
}

fn media_refs_for_chunk(
    media_refs: &[MediaRef],
    media_probes: &[Vec<String>],
    page_num: Option<i32>,
    chunk: &str,
) -> Vec<MediaRef> {
    // chunk 只在真的需要文本对齐时小写化一次。
    let mut chunk_lower: Option<String> = None;
    media_refs
        .iter()
        .zip(media_probes)
        .filter(|(media, probes)| {
            if let Some(media_page) = media.page_number {
                return page_num == Some(media_page);
            }
            // HTML/Markdown 通常没有页码，只能用 chunk 文本中的 URL/alt/caption/OCR 命中做节点级对齐。
            if page_num.is_some() {
                return false;
            }
            let chunk_lower = chunk_lower.get_or_insert_with(|| chunk.to_lowercase());
            probes.iter().any(|probe| chunk_lower.contains(probe.as_str()))
        })
        .map(|(media, _)| media.clone())
        .collect()
}

#[derive(Debug, Clone)]
struct EmbeddedImageOcrCandidate {
    media_index: usize,
    storage_path: String,
    mime_type: String,
    base64: String,
}

#[derive(Debug, Clone, Copy)]
struct EmbeddedImageOcrOutcome {
    status: OcrStatus,
    coverage: f64,
}

fn embedded_image_ocr_outcome(total: usize, success_count: usize) -> EmbeddedImageOcrOutcome {
    let coverage = if total == 0 {
        0.0
    } else {
        success_count as f64 / total as f64
    };
    let status = if total == 0 {
        OcrStatus::NotNeeded
    } else if success_count == total {
        OcrStatus::Done
    } else if success_count > 0 {
        OcrStatus::Partial
    } else {
        OcrStatus::Failed
    };
    EmbeddedImageOcrOutcome { status, coverage }
}

fn finish_embedded_image_ocr(
    conn: &Connection,
    doc_id: i64,
    run_id: &str,
    outcome: Option<EmbeddedImageOcrOutcome>,
) -> Result<()> {
    if let Some(outcome) = outcome {
        KbEngine::new(conn).update_document_ocr_with_run_guard(
            doc_id,
            outcome.status.as_str(),
            outcome.coverage,
            Some(run_id),
        )?;
    }
    Ok(())
}

fn supported_embedded_image_ocr_mime(mime_type: Option<&str>) -> Option<&'static str> {
    let mime_type = mime_type?.trim().to_ascii_lowercase();
    match mime_type.as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        _ => None,
    }
}

fn embedded_image_ocr_candidates(media_refs: &[MediaRef]) -> Vec<EmbeddedImageOcrCandidate> {
    media_refs
        .iter()
        .enumerate()
        .filter_map(|(media_index, media)| {
            if media.media_type != "image"
                || media
                    .ocr_text
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty())
            {
                return None;
            }
            let base64 = media.embedded_data_base64.as_ref()?;
            let mime_type = supported_embedded_image_ocr_mime(media.mime_type.as_deref())?;
            let byte_size = match media.byte_size {
                Some(size) if size >= 0 => size as usize,
                _ => base64.len().saturating_mul(3) / 4,
            };
            if byte_size > MAX_OCR_IMAGE_BYTES {
                tracing::warn!(
                    media_path = %media.storage_path,
                    byte_size,
                    max_bytes = MAX_OCR_IMAGE_BYTES,
                    "Skipping embedded image OCR because image is too large"
                );
                return None;
            }
            Some(EmbeddedImageOcrCandidate {
                media_index,
                storage_path: media.storage_path.clone(),
                mime_type: mime_type.to_string(),
                base64: base64.clone(),
            })
        })
        .collect()
}

fn embedded_image_ocr_result_text(results: &[crate::kb::ocr_provider::OcrPageResult]) -> String {
    results
        .iter()
        .filter_map(|result| {
            let text = if result.text.trim().is_empty() {
                result.markdown.trim()
            } else {
                result.text.trim()
            };
            (!text.is_empty()).then(|| text.to_string())
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn append_embedded_image_ocr_markdown(
    normalized: &mut crate::kb::parser::NormalizedDocument,
    storage_path: &str,
    text: &str,
) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let source_start = normalized.markdown.chars().count() as i32;
    let prefix = if normalized.markdown.trim().is_empty() {
        ""
    } else {
        "\n\n"
    };
    let section =
        format!("{prefix}## {EMBEDDED_IMAGE_OCR_SECTION_TITLE}\n\n### {storage_path}\n\n{text}\n");
    normalized.markdown.push_str(&section);
    let source_end = normalized.markdown.chars().count() as i32;
    normalized.blocks.push(ParsedBlock {
        text: text.to_string(),
        title_path: EMBEDDED_IMAGE_OCR_SECTION_TITLE.to_string(),
        page_number: None,
        source_start: Some(source_start),
        source_end: Some(source_end),
        block_type: "image_ocr".to_string(),
        metadata: serde_json::json!({ "storage_path": storage_path }).to_string(),
    });
}

#[allow(clippy::too_many_arguments)]
fn apply_embedded_image_ocr(
    conn: &Connection,
    doc_id: i64,
    lib_id: i64,
    run_id: &str,
    normalized: &mut crate::kb::parser::NormalizedDocument,
    config: &crate::config::Config,
) -> Result<Option<EmbeddedImageOcrOutcome>> {
    let candidates = embedded_image_ocr_candidates(&normalized.media_refs);
    if candidates.is_empty() {
        return Ok(None);
    }

    let kb = KbEngine::new(conn);
    if !config.ocr_enabled {
        tracing::warn!(
            doc_id,
            image_count = candidates.len(),
            "Embedded image OCR needed but OCR is disabled"
        );
        kb.update_document_ocr_with_run_guard(
            doc_id,
            OcrStatus::Needed.as_str(),
            0.0,
            Some(run_id),
        )?;
        return Ok(None);
    }

    let Some(api_key) = config.ocr_api_key.as_deref() else {
        tracing::warn!(
            doc_id,
            image_count = candidates.len(),
            "Embedded image OCR needed but OCR API key is not configured"
        );
        kb.update_document_ocr_with_run_guard(
            doc_id,
            OcrStatus::Needed.as_str(),
            0.0,
            Some(run_id),
        )?;
        return Ok(None);
    };

    use crate::kb::ocr_glm::{build_ocr_options_from_config, GlmOcrProvider};
    use crate::kb::ocr_provider::{OcrFilePayload, OcrInput};

    kb.update_document_ocr_with_run_guard(
        doc_id,
        OcrStatus::Processing.as_str(),
        0.0,
        Some(run_id),
    )?;

    let options = build_ocr_options_from_config(config);
    let provider = GlmOcrProvider::new(api_key);
    let total = candidates.len();
    let mut success_count = 0usize;

    for candidate in candidates {
        let input = OcrInput::Image {
            file: OcrFilePayload::Base64(candidate.base64),
            mime_type: candidate.mime_type.clone(),
            document_id: doc_id,
            run_id: format!("{}-embedded-image-{}", run_id, candidate.media_index + 1),
        };

        let mut retry_count = 0u32;
        loop {
            let Some(_ocr_permit) = crate::kb::worker::try_acquire_ocr_permit(
                config.ocr_max_concurrency,
                std::time::Duration::from_secs(30),
            ) else {
                tracing::warn!(
                    doc_id,
                    media_path = %candidate.storage_path,
                    "Embedded image OCR skipped because concurrency slots are full"
                );
                break;
            };

            let call_started = std::time::Instant::now();
            let recognition = provider.recognize_off_async_context(&input, &options);
            let latency_ms = call_started.elapsed().as_millis().min(i32::MAX as u128) as i32;

            match recognition {
                Ok(results) => {
                    let results =
                        crate::kb::ocr::sanitize_ocr_page_results(&results, Some(api_key));
                    crate::kb::ocr::log_ocr_external_model_call(
                        conn,
                        lib_id,
                        doc_id,
                        "glm_ocr",
                        &config.ocr_model,
                        latency_ms,
                        true,
                        "",
                        &results,
                        Some(api_key),
                    );
                    let text = embedded_image_ocr_result_text(&results);
                    if text.trim().is_empty() {
                        tracing::warn!(
                            doc_id,
                            media_path = %candidate.storage_path,
                            "Embedded image OCR returned empty text"
                        );
                    } else {
                        if let Some(media) = normalized.media_refs.get_mut(candidate.media_index) {
                            media.ocr_text = Some(text.clone());
                        }
                        append_embedded_image_ocr_markdown(
                            normalized,
                            &candidate.storage_path,
                            &text,
                        );
                        success_count += 1;
                    }
                    break;
                }
                Err(e) => {
                    let error_str = e.to_string();
                    let safe_error =
                        crate::kb::ocr::sanitize_error_text_with_secret(&error_str, Some(api_key));
                    crate::kb::ocr::log_ocr_external_model_call(
                        conn,
                        lib_id,
                        doc_id,
                        "glm_ocr",
                        &config.ocr_model,
                        latency_ms,
                        false,
                        &error_str,
                        &[],
                        Some(api_key),
                    );
                    let lower = error_str.to_lowercase();
                    let retryable = lower.contains("429")
                        || lower.contains("rate limit")
                        || lower.contains("503")
                        || lower.contains("timeout")
                        || lower.contains("timed out")
                        || lower.contains("connection")
                        || lower.contains("hyper");

                    if retryable && retry_count < 3 {
                        retry_count += 1;
                        let backoff_secs = 2u64 * 2u64.pow(retry_count - 1);
                        tracing::warn!(
                            doc_id,
                            media_path = %candidate.storage_path,
                            retry = retry_count,
                            backoff_secs,
                            error = %safe_error,
                            "Embedded image OCR hit retryable error"
                        );
                        std::thread::sleep(std::time::Duration::from_secs(backoff_secs));
                        continue;
                    }

                    tracing::warn!(
                        doc_id,
                        media_path = %candidate.storage_path,
                        error = %safe_error,
                        "Embedded image OCR failed"
                    );
                    break;
                }
            }
        }
    }

    Ok(Some(embedded_image_ocr_outcome(total, success_count)))
}

/// FIX10-R1: 在全文中定位每个 chunk 的字符偏移范围
///
/// 严格区分 byte offset 和 char offset：
/// - `String` 切片和 `find()` 返回的都是 byte offset
/// - 存储到 chunk_offsets 的是 char offset（字符偏移），与 source_start/source_end 语义一致
/// - 更新 byte_cursor 时确保落在 UTF-8 字符边界上
/// - fallback 路径用字符 offset 推进，再通过 char_indices 找对应 byte offset，始终 clamp 到 full_text.len()
///
/// 游标推进策略：匹配当前 chunk 后，游标推进到 end_char（无回退）。
/// 搜索下一 chunk 前，按相邻 chunk 的实际 suffix/prefix overlap 回退游标到重叠区，
/// 使 overlap chunk 能从重叠区域精确匹配，而非 fallback 到错误位置。
///
/// `max_overlap` 为 splitter 可能产生的最大重叠字符数，作为 overlap 上限：
/// - 防止偶然的 suffix/prefix 相同被误判为 splitter overlap
/// - 例：chunk_overlap=0 或 Markdown splitter 无 overlap 时传 0，
///   即使 chunks[i-1] 尾部和 chunks[i] 头部偶然相同也不回退
/// - Recursive splitter 传 config.chunk_overlap（splitter 内部已 cap 到 chunk_size/2）
///
/// 回退时机必须在搜索当前块之前，而非搜索之后：
/// - 搜索之后回退只能影响下一块，当前块已经从错误位置搜索过了
/// - 例：full_text="abcdef"、chunks ["abcd","cdef"]
///   若第一块后 cursor=4，第二块从 4 开始 find("cdef") 找不到，fallback 记录 4..8
///   正确做法：搜索第二块前回退 cursor 到 2，从 2 开始 find("cdef") 精确匹配 2..6
/// 精确定位块时的搜索窗口：块字节长的倍数，与最小窗口字节数取大者。
const CHUNK_LOCATE_WINDOW_FACTOR: usize = 8;
const CHUNK_LOCATE_WINDOW_MIN_BYTES: usize = 64 * 1024;

/// `index` 向前退到最近的 char 边界（`index <= text.len()`）。
fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub fn locate_chunk_char_offsets(
    full_text: &str,
    chunks: &[String],
    max_overlap: usize,
) -> Vec<(usize, usize)> {
    let mut offsets = Vec::with_capacity(chunks.len());
    let mut byte_cursor: usize = 0;
    let mut char_cursor: usize = 0;
    // 只数一次全文字符数（O(n)），供回退游标时判断是否已越过文末。
    let total_chars = full_text.chars().count();

    for (i, chunk) in chunks.iter().enumerate() {
        // 搜索当前块前，按与前一块的实际 overlap 回退游标到重叠区
        // 实际 overlap 取 suffix/prefix 匹配长度，但不超过 max_overlap 上限
        // max_overlap=0 时（Markdown splitter / chunk_overlap=0）不回退，避免偶然相同误判
        if i > 0 && max_overlap > 0 {
            let overlap_chars = actual_overlap_chars(&chunks[i - 1], chunk, max_overlap);
            // 允许 overlap_chars == char_cursor 回退到 0：
            // 例 full_text="abcdef"、chunks=["a","abcdef"]、max_overlap=1
            // 第一块后 char_cursor=1，overlap_chars=1，应回退到 0 才能精确匹配
            if overlap_chars > 0 && overlap_chars <= char_cursor {
                // 从游标处倒退到 back_char，而不是从文本开头数到 back_char：后者每块都要
                // 扫一遍全文，几万块的大文档会把唯一的索引 worker 钉死数小时。
                // 不变量：byte_cursor 是第 min(char_cursor, total_chars) 个字符的字节偏移
                // （推算路径可能把 char_cursor 推到文末之后，此时 byte_cursor == len）。
                let back_char = char_cursor - overlap_chars;
                if back_char >= total_chars {
                    byte_cursor = full_text.len();
                } else {
                    let chars_back = char_cursor.min(total_chars) - back_char;
                    byte_cursor = full_text[..byte_cursor]
                        .char_indices()
                        .rev()
                        .nth(chars_back - 1)
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                char_cursor = back_char;
            }
        }

        // FIX10-R1: 先检查 byte_cursor 不越界，再尝试 find
        // find 成功 → 精确匹配路径；find 失败或 byte_cursor 越界 → fallback 推算路径
        // 搜索窗口有界：块不是全文的精确子串时（splitter 用不同分隔符重拼了段落），
        // 无界 find 会为每一块扫到文末，同样是 O(全文 × 块数)。窗口取块长的若干倍，
        // 覆盖正常的分隔符漂移；超出窗口按推算偏移处理（与原本 find 失败的路径一致）。
        let found = if byte_cursor <= full_text.len() {
            let window_end = byte_cursor
                .saturating_add(chunk.len().saturating_mul(CHUNK_LOCATE_WINDOW_FACTOR))
                .max(byte_cursor.saturating_add(CHUNK_LOCATE_WINDOW_MIN_BYTES))
                .min(full_text.len());
            let window_end = floor_char_boundary(full_text, window_end);
            full_text[byte_cursor..window_end].find(chunk.as_str())
        } else {
            None
        };
        if let Some(pos) = found {
            let start_byte = byte_cursor + pos;

            // byte offset 转字符 offset
            let start_char = char_cursor + full_text[byte_cursor..start_byte].chars().count();
            let end_char = start_char + chunk.chars().count();
            offsets.push((start_char, end_char));

            // 匹配成功后，游标推进到 end_char（不回退）
            // 下一次迭代开始时会根据与当前块的 overlap 回退
            // 精确匹配时块就是全文的子串，终点字节 = 起点 + 块字节长，O(1)。
            byte_cursor = (start_byte + chunk.len()).min(full_text.len());
            char_cursor = end_char;
        } else {
            // 无法找到精确位置，用推算偏移
            // fallback 路径：用字符 offset 推进，再通过 char_indices 找对应 byte offset
            let start_char = char_cursor;
            let chunk_char_len = chunk.chars().count();
            let end_char = start_char + chunk_char_len;
            offsets.push((start_char, end_char));

            // fallback 也推进到 end_char
            let skip_chars = end_char.saturating_sub(char_cursor);
            byte_cursor = full_text[byte_cursor..]
                .char_indices()
                .nth(skip_chars)
                .map(|(i, _)| byte_cursor + i)
                .unwrap_or(full_text.len());
            // 始终 clamp 到 full_text.len()，防止越界
            byte_cursor = byte_cursor.min(full_text.len());
            char_cursor = end_char;
        }
    }

    offsets
}

/// 计算两个相邻 chunk 的实际 suffix/prefix overlap 字符数
///
/// 取前一个 chunk 的尾部和当前 chunk 的头部，找到最大公共子串长度。
/// 结果不超过 `max_overlap` 上限，防止偶然的 suffix/prefix 相同被误判为 splitter overlap。
/// 例：max_overlap=0 时直接返回 0；max_overlap=2 时最多回退 2 个字符。
fn actual_overlap_chars(prev: &str, curr: &str, max_overlap: usize) -> usize {
    if max_overlap == 0 {
        return 0;
    }
    let prev_chars: Vec<char> = prev.chars().collect();
    let curr_chars: Vec<char> = curr.chars().collect();
    let max_possible = prev_chars.len().min(curr_chars.len()).min(max_overlap);

    // 从最大可能重叠开始递减，找到第一个匹配的长度
    for overlap in (1..=max_possible).rev() {
        let prev_suffix = &prev_chars[prev_chars.len() - overlap..];
        let curr_prefix = &curr_chars[..overlap];
        if prev_suffix == curr_prefix {
            return overlap;
        }
    }
    0
}

/// 根据 splitter 配置计算最大可能 overlap 字符数
///
/// 自适应分割策略下的 overlap 计算：
/// 1. 有 embedder → AdaptiveSplitter 大块可能走 SemanticSplitter → 返回 chunk_overlap
/// 2. Markdown → 先 header 切（无 overlap），大块可能走 recursive/semantic → 返回 chunk_overlap
/// 3. 其余 → RecursiveCharSplitter，内部 cap 到 chunk_size/2 → 返回 chunk_overlap.min(chunk_size/2)
pub fn splitter_max_overlap(config: &SplitterConfig, has_embedder: bool) -> usize {
    let ext = std::path::Path::new(&config.file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if has_embedder {
        // 有 embedder 时，adaptive splitter 的大块会用 SemanticSplitter（带 overlap）
        config.chunk_overlap
    } else if ext == "md" || ext == "markdown" {
        // Markdown：header 切无 overlap，但大块 recursive refine 有 overlap
        config.chunk_overlap
    } else {
        config.chunk_overlap.min(config.chunk_size / 2)
    }
}

/// 进度回调类型: 接收阶段名称和进度消息
pub type ProgressCallback = Box<dyn Fn(&str, &str) + Send + Sync>;

/// 管道配置
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// 分割器配置
    pub splitter: SplitterConfig,
    /// RAPTOR 配置
    pub raptor: RaptorConfig,
    /// 是否在嵌入后构建 RAPTOR 树
    pub enable_raptor: bool,
    /// 是否生成嵌入向量
    pub enable_embedding: bool,
}

// ---------------------------------------------------------------------------
// 同步管道 (保留原有接口兼容性)
// ---------------------------------------------------------------------------

/// 处理 KB 文档的同步管道 (原有接口)
///
/// 阶段:
/// 1. 解析  - 使用 ParserRegistry 从文档文件提取文本
/// 2. 分割  - 使用 create_splitter 将文本分块
/// 3. 构建  - 构造 level-0 RaptorNode 对象
/// 4. RAPTOR - 构建抽象树 (如启用, 延迟到嵌入完成后)
/// 5. 持久化 - 在一个事务中写入节点、向量和 FTS5 条目
///
/// 成功时返回词数/分块数。失败时文档状态设为 STATUS_FAILED。
pub fn process_document(conn: &Connection, payload: &KbProcessPayload) -> Result<ProcessResult> {
    // PDF 必须走异步 OCR 流程（process_document_async），同步入口拒绝 PDF
    let payload_ext = payload.extension.to_lowercase();
    if payload_ext == "pdf" || is_ocr_image_extension(&payload_ext) {
        return Err(GBrainError::InvalidInput(
            "PDF/image OCR documents must use process_document_async; sync processing is not supported"
                .to_string(),
        ));
    }

    let kb = KbEngine::new(conn);
    let doc_id = payload.document_id;
    let lib_id = payload.library_id;
    let run_id = &payload.processing_run_id;

    // 守卫: 确保此运行仍是当前的 (防止过期作业执行)
    kb.ensure_document_run_current(doc_id, run_id)?;

    // 加载库配置
    let library = kb.get_library(lib_id)?;

    // --- 阶段 1: 解析 ---
    // 修复：所有中间状态更新传入 run_id，防止旧 job 污染新 run 的文档状态
    kb.update_document_status_with_run_guard(
        doc_id,
        Some(STATUS_PROCESSING),
        Some(10),
        None,
        None,
        None,
        None,
        Some(run_id),
    )?;

    let registry = ParserRegistry::new();
    let ext = &payload.extension;
    let storage_path = &payload.storage_path;

    let file_data = std::fs::read(storage_path).map_err(|e| {
        let _ = kb.update_document_status_with_run_guard(
            doc_id,
            Some(STATUS_FAILED),
            None,
            Some(&format!("无法读取 {}: {}", storage_path, e)),
            None,
            None,
            None,
            Some(run_id),
        );
        GBrainError::FileError(format!("无法读取 {}: {}", storage_path, e))
    })?;

    let parsed = registry.parse(ext, &file_data).inspect_err(|e| {
        let _ = kb.update_document_status_with_run_guard(
            doc_id,
            Some(STATUS_FAILED),
            None,
            Some(&e.to_string()),
            None,
            None,
            None,
            Some(run_id),
        );
    })?;

    let normalizer = registry.get_normalizer(ext);
    let mut normalized = normalizer.normalize(parsed.clone()).unwrap_or_else(|e| {
        tracing::warn!(
            doc_id,
            error = %e,
            "富文本标准化失败，回退到原始解析结果"
        );
        crate::kb::parser::NormalizedDocument {
            markdown: parsed.content.clone(),
            blocks: parsed.blocks.clone().unwrap_or_default(),
            media_refs: parsed.media_refs.clone(),
            attachments: parsed.media_refs.clone(),
        }
    });

    let embedded_ocr_outcome = if normalized
        .media_refs
        .iter()
        .any(|media| media.media_type == "image" && media.embedded_data_base64.is_some())
    {
        let ocr_config = crate::config::Config::load_partial()
            .map_err(|e| crate::error::GBrainError::Config(e.to_string()))?;
        apply_embedded_image_ocr(conn, doc_id, lib_id, run_id, &mut normalized, &ocr_config)?
    } else {
        None
    };

    let word_total: i32 = count_words(&normalized.markdown) as i32;
    kb.update_document_status_with_run_guard(
        doc_id,
        Some(STATUS_PROCESSING),
        Some(30),
        None,
        None,
        None,
        None,
        Some(run_id),
    )?;

    // --- 阶段 2: 分割 ---
    let splitter_config = SplitterConfig {
        file_path: storage_path.to_string(),
        chunk_size: library.chunk_size,
        chunk_overlap: library.chunk_overlap,
    };

    let splitter = create_splitter(&splitter_config);
    let chunks = splitter.split(&normalized.markdown).inspect_err(|e| {
        let _ = kb.update_document_status_with_run_guard(
            doc_id,
            Some(STATUS_FAILED),
            None,
            Some(&e.to_string()),
            None,
            None,
            None,
            Some(run_id),
        );
    })?;

    let split_total: i32 = chunks.len() as i32;
    kb.update_document_status_with_run_guard(
        doc_id,
        Some(STATUS_COMPLETED),
        Some(100),
        None,
        Some(STATUS_PROCESSING),
        Some(10),
        None,
        Some(run_id),
    )?;

    // --- 阶段 3: 构建 Level-0 节点 ---
    // 稳定标题源：优先使用 metadata 中的 title，其次使用数据库中的 original_name
    let format_title = parsed
        .metadata
        .get("title")
        .map(|s| s.as_str())
        .unwrap_or("");
    let original_name: String = conn
        .query_row(
            "SELECT original_name FROM kb_documents WHERE id = ?1",
            [doc_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default();
    let doc_title = resolve_doc_title(format_title, &original_name);
    let title_weight = library.title_weight;
    // P2: 同步管道 ChunkMeta — 根据文件扩展名推断模态和段落类型
    let sync_ext = payload_ext.to_lowercase();
    let sync_chunk_meta = ChunkMeta {
        strategy: "adaptive",
        segment_kind: infer_segment_kind(&sync_ext, &[]), // 同步管道无 block_spans，传空列表
        semantic_refined: false,
        modality: infer_modality(&sync_ext),
    };

    let media_probes = build_media_probes(&normalized.media_refs);
    let raptor_nodes: Vec<RaptorNode> = chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let node_media_refs =
                media_refs_for_chunk(&normalized.media_refs, &media_probes, None, chunk);
            let embedding_text = crate::kb::context::build_embedding_text(
                &doc_title,
                "",   // 同步管道无章节路径
                None, // 同步管道无页码
                chunk,
                title_weight,
            );
            RaptorNode {
                id: -((i as i64) + 1), // 临时负 ID
                library_id: lib_id,
                document_id: doc_id,
                content: chunk.clone(),
                level: 0,
                parent_id: None,
                chunk_order: i as i32,
                vector: None,
                title_path: String::new(),
                page_number: None,
                source_start: None,
                source_end: None,
                node_metadata: serialize_node_metadata_ex(
                    &node_media_refs,
                    None,
                    !normalized.media_refs.is_empty(),
                    Some(sync_chunk_meta),
                ),
                embedding_text,
            }
        })
        .collect();

    // --- 阶段 4: RAPTOR (可选, 延迟) ---
    if library.raptor_enabled && raptor_nodes.len() >= 3 {
        tracing::info!(
            doc_id,
            node_count = raptor_nodes.len(),
            "RAPTOR 已启用; 树构建延迟到嵌入完成后"
        );
    }

    // --- 阶段 5: 持久化 ---
    // 修复：持久化前再次校验 run_id，防止 stale job 在通过初始校验后
    // 继续跑完后续阶段（新上传可能已更新 run_id）
    kb.ensure_document_run_current(doc_id, run_id)?;
    // 修复：将 persist_nodes_and_vectors 和 update_document_stats_with_run_guard
    // 合入同一事务，消除两者之间的竞态窗口。旧代码先单独提交 nodes（事务 1），
    // 再单独更新 stats（事务 2）。若新上传在两者之间更新 processing_run_id，
    // stats 会失败（run_guard 拒绝），但旧 run 的 nodes 已经留下。
    // 合入同一事务后，如果 stats 的 run_guard 检查失败，整个事务回滚，
    // nodes 也不会留下。
    {
        let tx = conn.unchecked_transaction()?;
        let result = (|| -> Result<()> {
            // P1-1: active version 生命周期
            // 1) 开启新版本（building）；同 hash 复用旧 ready 版本
            // 2) 仅清理同 version 残留节点后写入新节点
            // 3) 激活新版本，旧版本标记 retired
            let content_hash = query_document_content_hash(conn, doc_id)?;
            let version_id = begin_document_version(conn, doc_id, Some(run_id), &content_hash)?;
            persist_nodes_for_version_inner(
                conn,
                doc_id,
                version_id,
                lib_id,
                &raptor_nodes,
                Some(run_id),
            )?;
            activate_document_version(conn, doc_id, Some(run_id), version_id)?;
            // P1-2: 持久化媒体引用（图片/附件）
            persist_media_assets(conn, doc_id, lib_id, version_id, &normalized.media_refs)?;
            // 摘要数据由 SummarizeDocument 命令或外部 engine.insert_summary 管理，
            // pipeline 不负责生成或清理摘要，避免误删外部写入的摘要数据。
            // 修复：外层已开事务，调用 _inner 避免嵌套 BEGIN（SQLite 不支持）
            kb.update_document_stats_with_run_guard_inner(
                doc_id,
                word_total,
                split_total,
                None,
                Some(run_id),
            )?;
            Ok(())
        })();
        match result {
            Ok(_) => tx.commit()?,
            Err(e) => {
                let _ = tx.rollback();
                return Err(e);
            }
        }
    }

    finish_embedded_image_ocr(conn, doc_id, run_id, embedded_ocr_outcome)?;

    Ok(ProcessResult {
        word_total,
        split_total,
        deferred_ocr: false,
    })
}

// ---------------------------------------------------------------------------
// 异步管道 (5阶段: 解析 → 分割 → 嵌入 → RAPTOR → 持久化)
// ---------------------------------------------------------------------------

/// 异步处理 KB 文档的 5 阶段管道
///
/// 阶段:
/// 1. **解析** — 检测格式, 提取文本内容
/// 2. **分割** — 使用配置的分割器将文本分块
/// 3. **嵌入** — 为每个节点生成嵌入向量
/// 4. **RAPTOR** — 构建层次化摘要树 (可选)
/// 5. **持久化** — 将所有节点写入数据库
///
/// # 安全性说明：`&Connection` 跨越 `.await` 边界
///
/// 本函数接收 `&Connection`（rusqlite 同步连接）并在多个 `.await` 点之间使用。
/// 这是安全的，因为：
/// - 所有数据库操作（`conn.execute`、`conn.unchecked_transaction` 等）是同步调用，
///   不会与 `.await` 交叉执行；
/// - `.await` 点（`split_async`、`embed_batch`、`summarize_cluster`）仅涉及纯 CPU/IO，
///   不访问数据库连接；
/// - 此函数在单线程 tokio 任务中运行，不存在并发访问同一连接的风险。
///   若未来需要并发数据库访问，应改用 `deadpool` 等连接池。
#[allow(clippy::too_many_arguments)]
pub async fn process_document_async(
    conn: &Connection,
    payload: &KbProcessPayload,
    embedder: Option<Arc<Embedder>>,
    raptor_config: Option<&RaptorConfig>,
    kb_raptor_secret_ref: Option<&str>,
    kb_raptor_base_url: Option<&str>,
    kb_raptor_model: Option<&str>,
    on_progress: Option<&ProgressCallback>,
    // P3 修复: 完整 resolved RAPTOR config（api_key+base_url+model 均已合并 config 文件+环境变量）
    resolved_raptor_config: Option<&crate::config::ResolvedRaptorConfig>,
) -> Result<ProcessResult> {
    let kb = KbEngine::new(conn);
    let doc_id = payload.document_id;
    let lib_id = payload.library_id;
    let run_id = &payload.processing_run_id;

    // M-7 修复：统一在此处加载一次 OCR 配置，避免图片/PDF 分支各自重复调用 Config::load()。
    // TODO: 调用方已持有 config: &Config，理想做法是将 &Config 作为参数传入本函数，
    // 但签名变更影响面较广（所有调用方需同步修改），当前先用单次加载缓解。
    //
    // P2 修复：改用 load_partial() 而非 load()。worker 已传 resolved_raptor_config=None
    // 让 RAPTOR 配置延迟到真正需要时（nodes>=3 或 augmentation）才解析；若此处仍用完整
    // load()，会在进入实际 RAPTOR 条件前就因强制要求 GBRAIN_KB_RAPTOR_* 等env 而失败，
    // 抵消延迟解析。load_partial 不做严格校验，OCR 相关字段仍从 env/config.json 正常读取；
    // OCR 子系统对“未配置/已关闭”本身有跳过与兜底逻辑，不受影响。
    let ocr_config = crate::config::Config::load_partial()
        .map_err(|e| crate::error::GBrainError::Config(e.to_string()))?;

    kb.ensure_document_run_current(doc_id, run_id)?;
    let library = kb.get_library(lib_id)?;

    // --- 阶段 1: 解析 ---
    report_progress(
        on_progress,
        "parsing",
        &format!("解析 {}", payload.storage_path),
    );
    // 修复：中间状态更新传入 run_id，防止旧 job 污染新 run 的文档状态
    kb.update_document_status_with_run_guard(
        doc_id,
        Some(STATUS_PROCESSING),
        Some(10),
        None,
        None,
        None,
        None,
        Some(run_id),
    )?;

    let registry = ParserRegistry::new();
    let ext = &payload.extension;
    let storage_path = &payload.storage_path;

    let file_data = std::fs::read(storage_path).map_err(|e| {
        let _ = kb.update_document_status_with_run_guard(
            doc_id,
            Some(STATUS_FAILED),
            None,
            Some(&format!("无法读取 {}: {}", storage_path, e)),
            None,
            None,
            None,
            Some(run_id),
        );
        GBrainError::FileError(format!("无法读取 {}: {}", storage_path, e))
    })?;

    if is_ocr_image_extension(ext) {
        enqueue_image_ocr(
            conn,
            doc_id,
            lib_id,
            run_id,
            storage_path,
            ext,
            &library,
            &file_data,
            &ocr_config,
        )?;
        tracing::info!(
            doc_id,
            "Image OCR job queued; main KB pipeline returned early"
        );
        return Ok(ProcessResult {
            word_total: 0,
            split_total: 0,
            deferred_ocr: true,
        });
    }

    let parsed = registry.parse(ext, &file_data).inspect_err(|e| {
        let _ = kb.update_document_status_with_run_guard(
            doc_id,
            Some(STATUS_FAILED),
            None,
            Some(&e.to_string()),
            None,
            None,
            None,
            Some(run_id),
        );
    })?;

    // 通过 RichContentNormalizer 标准化富文本
    // HTML 等富文本格式会在此步完成表格/流程图/附件处理，
    // 生成结构化的 NormalizedDocument（markdown + blocks + media_refs）。
    // 标准化后的产物将作为后续 split/embed/media 阶段的主输入，
    // 替代 parsed.content / parsed.blocks，确保富文本格式获得更好的结构化数据。
    let normalizer = registry.get_normalizer(ext);
    let mut normalized = normalizer.normalize(parsed.clone()).unwrap_or_else(|e| {
        tracing::warn!(
            doc_id,
            error = %e,
            "富文本标准化失败，回退到原始解析结果"
        );
        crate::kb::parser::NormalizedDocument {
            markdown: parsed.content.clone(),
            blocks: parsed.blocks.clone().unwrap_or_default(),
            media_refs: parsed.media_refs.clone(),
            attachments: parsed.media_refs.clone(),
        }
    });

    // Phase 1+2: PDF OCR 分支 — 解析后、分割前执行 OCR
    // 返回 None 表示异步 OCR 已入队，主 pipeline 应在此提前返回，不执行后续 split/embed/persist
    let parsed = if ext == "pdf" {
        // M-7 修复：复用函数顶部已加载的 ocr_config，不再重复 Config::load()
        match maybe_apply_pdf_ocr(
            conn,
            doc_id,
            lib_id,
            run_id,
            storage_path,
            &parsed,
            &library,
            &file_data,
            &ocr_config,
        )? {
            Some(p) => {
                // PDF OCR 后更新 normalized 的 markdown，因为 OCR 可能已修改文本内容
                normalized.markdown = p.content.clone();
                p
            }
            None => {
                // 异步 OCR 已入队，且 maybe_apply_pdf_ocr 已在同一事务中
                // 写入 document_status=ocr_pending / ocr_status=queued。
                // 这里仅提前返回，避免后续 split/embed/persist 和 artifact 流程抢跑。
                tracing::info!(doc_id, "PDF OCR 已异步入队，主 pipeline 提前返回");
                return Ok(ProcessResult {
                    word_total: 0,
                    split_total: 0,
                    deferred_ocr: true, // P2 修复：显式标记 OCR 已延后
                });
            }
        }
    } else {
        parsed
    };

    let embedded_ocr_outcome = if ext != "pdf" {
        apply_embedded_image_ocr(conn, doc_id, lib_id, run_id, &mut normalized, &ocr_config)?
    } else {
        None
    };

    let effective_content = normalized.markdown.as_str();
    let word_total: i32 = count_words(effective_content) as i32;

    // P1-013: 元数据抽取（文件系统 + 格式特定）
    let storage = std::path::Path::new(storage_path);
    let mut doc_meta = crate::kb::metadata::DocumentMetadata::from_file_path(storage);
    let format_meta = match ext.as_str() {
        "md" => crate::kb::metadata::extract_markdown_metadata(&parsed.content, &file_data),
        "pdf" => crate::kb::metadata::extract_pdf_metadata(&parsed.content, &file_data),
        "docx" => crate::kb::metadata::extract_docx_metadata(&parsed.content, &file_data),
        "html" | "htm" => {
            // HTML parser 已在 parse() 阶段从原始 HTML 正确提取了 title/meta 标签，
            // 此处直接从 parsed.metadata 转换为 DocumentMetadata，
            // 避免对已抽取的纯文本做无效的 HTML 标签匹配。
            crate::kb::metadata::DocumentMetadata::from_parser_metadata(&parsed.metadata)
        }
        _ => crate::kb::metadata::DocumentMetadata::default(),
    };
    // 原始文件字节到此不再使用：提前释放（最大 kb_max_file_size_mb），别让它陪着全部
    // 节点和向量一起撑到版本激活事务——安卓进程内存被 LMK 盯着。
    drop(file_data);
    doc_meta.merge_with(&format_meta);
    // P1-019: 关键词和实体抽取
    let (keywords, entities) = crate::kb::metadata::extract_keywords_and_entities(
        effective_content,
        doc_meta.language.as_deref().unwrap_or("zh"),
    );
    // 合并 meta keywords（如 HTML <meta name="keywords">）与正文提取的关键词。
    // meta keywords 是作者指定的权威标签，优先保留；正文提取的关键词补充追加。
    let final_keywords = match doc_meta.keywords.as_deref() {
        Some(meta_kw) if !meta_kw.is_empty() && !keywords.is_empty() => {
            format!("{}, {}", meta_kw, keywords)
        }
        Some(meta_kw) if !meta_kw.is_empty() => meta_kw.to_string(),
        _ => keywords,
    };
    // 落库元数据
    // FIX11-04: 元数据更新失败不应静默吞下，至少记录警告
    // 修复：传入 run_id，防止旧 job 污染新 run 的文档元数据
    if let Err(e) = kb.update_document_metadata_with_run_guard(
        doc_id,
        doc_meta.title.as_deref().unwrap_or(""),
        doc_meta.author.as_deref().unwrap_or(""),
        &final_keywords,
        &entities,
        doc_meta.source_uri.as_deref().unwrap_or(""),
        doc_meta.document_date.as_deref(),
        doc_meta.modified_at.as_deref(),
        Some(run_id),
    ) {
        tracing::warn!("文档 {} 元数据更新失败: {}", doc_id, e);
    }

    // P1-014: 文档粒度分类（解析完成后立即判定）
    let char_count = effective_content.chars().count();
    let page_count: usize = parsed
        .metadata
        .get("total_pages")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let granularity = crate::kb::granularity::classify_granularity(ext, char_count, page_count);
    let chunk_strategy = crate::kb::granularity::chunk_strategy_for(granularity);
    // P0-4: 使用统一的启发式 token counter 计算精确 token 数
    let token_count = crate::kb::token_counter::count_tokens_heuristic(effective_content) as i32;
    // 修复：传入 run_id，防止旧 job 污染新 run 的 granularity
    kb.update_document_granularity_with_run_guard(
        doc_id,
        granularity.as_str(),
        chunk_strategy,
        char_count as i32,
        page_count as i32,
        Some(token_count),
        Some(run_id),
    )?;

    // 修复：中间状态更新传入 run_id，防止旧 job 污染新 run 的文档状态
    kb.update_document_status_with_run_guard(
        doc_id,
        Some(STATUS_PROCESSING),
        Some(30),
        None,
        None,
        None,
        None,
        Some(run_id),
    )?;

    // --- 阶段 2: 分割 ---
    report_progress(on_progress, "chunking", "分割为节点");
    let splitter_config = SplitterConfig {
        file_path: storage_path.to_string(),
        chunk_size: library.chunk_size,
        chunk_overlap: library.chunk_overlap,
    };

    let splitter = create_async_splitter(&splitter_config, embedder.clone()).inspect_err(|e| {
        let _ = kb.update_document_status_with_run_guard(
            doc_id,
            Some(STATUS_FAILED),
            None,
            Some(&e.to_string()),
            None,
            None,
            None,
            Some(run_id),
        );
    })?;
    let chunks = splitter
        .split_async(&normalized.markdown)
        .await
        .inspect_err(|e| {
            let _ = kb.update_document_status_with_run_guard(
                doc_id,
                Some(STATUS_FAILED),
                None,
                Some(&e.to_string()),
                None,
                None,
                None,
                Some(run_id),
            );
        })?;

    let split_total: i32 = chunks.len() as i32;

    // 从标准化后的 blocks 中提取每块的元数据（优先使用 normalized.blocks，
    // 若标准化器未产出 blocks 则回退到 parsed.blocks），用 span 匹配而非下标硬匹配
    #[allow(clippy::type_complexity)]
    let source_blocks: &[crate::kb::types::ParsedBlock] = if normalized.blocks.is_empty() {
        parsed.blocks.as_deref().unwrap_or(&[])
    } else {
        &normalized.blocks
    };
    let block_spans: Vec<BlockSpan> = source_blocks
        .iter()
        .map(|b| {
            (
                b.title_path.clone(),
                b.page_number,
                b.source_start,
                b.source_end,
            )
        })
        .collect();

    // FIX10-R1: 使用统一的 helper 定位 chunk 字符偏移，max_overlap 按 splitter 类型计算
    // semantic → chunk_overlap; Markdown → 0; Recursive → chunk_overlap.min(chunk_size/2)
    let full_text = &normalized.markdown;
    let max_overlap = splitter_max_overlap(&splitter_config, embedder.is_some());
    let chunk_offsets: Vec<(usize, usize)> =
        locate_chunk_char_offsets(full_text, &chunks, max_overlap);

    // 对每个 chunk，找与其 span 重叠最多的 block
    #[allow(clippy::type_complexity)]
    fn find_best_block(
        chunk_start: usize,
        chunk_end: usize,
        spans: &[(String, Option<i32>, Option<i32>, Option<i32>)],
    ) -> (String, Option<i32>, Option<i32>, Option<i32>) {
        let mut best: (usize, (String, Option<i32>, Option<i32>, Option<i32>)) =
            (0, spans.first().cloned().unwrap_or_default());
        for (idx, (_title, _page, s_start, s_end)) in spans.iter().enumerate() {
            let bs = s_start.unwrap_or(0) as usize;
            let be = s_end.unwrap_or(i32::MAX) as usize;
            let overlap = if chunk_start < be && chunk_end > bs {
                // parser 若给出 end < start 的畸形 span，这里不能下溢（debug panic / release 绕回成巨大值选错块）
                chunk_end.min(be).saturating_sub(chunk_start.max(bs))
            } else {
                0
            };
            if overlap > best.0 {
                best = (overlap, spans[idx].clone());
            }
        }
        best.1
    }

    // P1-006/P1-007: 根据粒度应用节点策略 + P1-011: 生成 contextual embedding 文本
    // 稳定标题源：优先使用格式解析提取的 title，其次使用数据库中的 original_name
    let format_title = doc_meta.title.as_deref().unwrap_or("");
    let original_name: String = conn
        .query_row(
            "SELECT original_name FROM kb_documents WHERE id = ?1",
            [doc_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default();
    let doc_title = resolve_doc_title(format_title, &original_name);
    let title_weight = library.title_weight;
    // P2: 根据文件扩展名推断默认 modality 和 segment_kind
    let file_ext = std::path::Path::new(storage_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let default_chunk_meta = ChunkMeta {
        strategy: "adaptive",
        segment_kind: infer_segment_kind(&file_ext, &block_spans),
        // M2 修复：有 embedder 时大块可能走语义细分，标记为 true
        semantic_refined: embedder.is_some(),
        modality: infer_modality(&file_ext),
    };
    let media_probes = build_media_probes(&normalized.media_refs);
    let mut nodes: Vec<RaptorNode> = chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let (c_start, c_end) = chunk_offsets[i];
            // P2 修复：node 的 source_start/source_end 必须是 chunk 在源文件中的字符偏移，
            // 而不是其重叠最佳 block 的 src_start/src_end。
            // 旧实现的问题：
            //   - markdown/text/csv/docx 等 parser 没有 blocks（blocks=None / block_spans 为空），
            //     除第一个 chunk 外其余 chunk 的 source_start 全部退化为 None / 0，
            //     导致 passage 计算的绝对偏移定位错误。
            //   - PDF/HTML/XLSX 在同一个长 block 内的多个 chunk，全部继承同一个 block 起始，
            //     失去 chunk 粒度的定位精度。
            // 修复后：source_start/source_end 始终取 c_start/c_end；page_number/title_path
            // 继续使用 block 信息（这些字段语义本就是关联到 block）。
            let (title_path, page_num) = if block_spans.is_empty() {
                (String::new(), None)
            } else {
                let (tp, pn, _bs, _be) = find_best_block(c_start, c_end, &block_spans);
                (tp, pn)
            };
            let src_start = Some(c_start as i32);
            let src_end = Some(c_end as i32);
            let embedding_text = crate::kb::context::build_embedding_text(
                &doc_title,
                &title_path,
                page_num,
                chunk,
                title_weight,
            );
            // P1-2/P2: 媒体引用写入 node_metadata.media_refs。
            // PDF 优先按 page_number 对齐；HTML/Markdown 这类无页码文档按 chunk 文本中的
            // URL/alt/caption/OCR 命中对齐，避免文档级全量媒体污染 prompt。
            let node_media_refs =
                media_refs_for_chunk(&normalized.media_refs, &media_probes, page_num, chunk);
            let node_meta = serialize_node_metadata_ex(
                &node_media_refs,
                None,
                !normalized.media_refs.is_empty(),
                Some(default_chunk_meta),
            );
            RaptorNode {
                id: -((i as i64) + 1),
                library_id: lib_id,
                document_id: doc_id,
                content: chunk.clone(),
                level: 0,
                parent_id: None,
                chunk_order: i as i32,
                vector: None,
                title_path,
                page_number: page_num,
                source_start: src_start,
                source_end: src_end,
                node_metadata: node_meta,
                embedding_text,
            }
        })
        .collect();

    // P1 修复：表格写入已移入版本激活事务内（见下方事务块），
    // 与节点/版本原子绑定，不再在此处删除旧数据。
    // 旧表格数据的清理由 cleanup_retired_version 按 version_id 精确删除。

    // P1-006: micro 文档策略 — 仅保留一个 whole-document node
    if granularity == crate::kb::granularity::DocumentGranularity::Micro && !chunks.is_empty() {
        let full_text = normalized.markdown.clone();
        let embedding_text =
            crate::kb::context::build_micro_embedding_text(&doc_title, &full_text, title_weight);
        let micro_meta = ChunkMeta {
            strategy: "adaptive",
            segment_kind: "whole_document",
            semantic_refined: false,
            modality: infer_modality(&file_ext),
        };
        nodes = vec![RaptorNode {
            id: -1,
            library_id: lib_id,
            document_id: doc_id,
            content: full_text,
            level: 0,
            parent_id: None,
            chunk_order: 0,
            vector: None,
            title_path: String::new(),
            page_number: None,
            source_start: None,
            source_end: None,
            node_metadata: serialize_node_metadata_ex(
                &normalized.media_refs,
                Some("whole_document"),
                !normalized.media_refs.is_empty(),
                Some(micro_meta),
            ),
            embedding_text,
        }];
    }

    // --- 阶段 2.5: 自动关键词与问题生成（可选） ---
    // 需要库级 augmentation_enabled 开关 + 外部摘要允许（增强是外部 LLM 调用）
    if library.augmentation_enabled && !nodes.is_empty() {
        // 解析 LLM 配置（复用 RAPTOR 的配置解析逻辑）
        let llm_cfg = raptor::resolve_raptor_llm_config(
            Some(&library),
            kb_raptor_secret_ref,
            kb_raptor_base_url,
            kb_raptor_model,
            resolved_raptor_config,
        );

        if let Ok(cfg) = llm_cfg {
            report_progress(
                on_progress,
                "augment",
                &format!("增强 {} 个节点（关键词+问题生成）", nodes.len()),
            );

            let mut aug_count = 0usize;
            let mut processed_aug_count = 0usize;
            let augment_inputs: Vec<(usize, String)> = nodes
                .iter()
                .enumerate()
                .filter(|(_, node)| node.content.chars().count() >= 100)
                .map(|(i, node)| (i, node.content.clone()))
                .collect();
            let total_aug_count = augment_inputs.len();
            let mut consecutive_failures = 0usize;
            let augment_concurrency = augment_concurrency_for(total_aug_count);

            let mut tasks = FuturesUnordered::new();
            let mut next_augment_input = 0usize;

            while next_augment_input < total_aug_count && tasks.len() < augment_concurrency {
                let (index, content) = augment_inputs[next_augment_input].clone();
                tasks.push(run_augment_task(
                    index,
                    content,
                    cfg.api_key.clone(),
                    cfg.base_url.clone(),
                    cfg.model.clone(),
                ));
                next_augment_input += 1;
            }

            while let Some(task_result) = tasks.next().await {
                processed_aug_count += 1;

                let (success, error_msg): (bool, String) = match &task_result.result {
                    Ok(_) => (true, String::new()),
                    Err(e) => (false, e.clone()),
                };
                let _ = crate::kb::privacy::log_external_model_call(
                    conn,
                    Some(lib_id),
                    Some(doc_id),
                    "augment",
                    "custom",
                    &cfg.model,
                    task_result.input_chars,
                    0,
                    task_result.latency_ms,
                    0.0,
                    success,
                    &error_msg,
                );

                match task_result.result {
                    Ok(Some(aug)) => {
                        if let Some(node) = nodes.get_mut(task_result.index) {
                            // 合并到 node_metadata
                            node.node_metadata =
                                crate::kb::augment::merge_augmentation_into_metadata(
                                    &node.node_metadata,
                                    &aug,
                                );
                            aug_count += 1;
                        }
                        consecutive_failures = 0; // 成功后重置连续失败计数
                    }
                    Ok(None) => {} // LLM 返回空结果，正常跳过
                    Err(e) => {
                        consecutive_failures += 1;
                        tracing::debug!(consecutive_failures, "节点增强失败: {}", e);
                    }
                }

                while next_augment_input < total_aug_count && tasks.len() < augment_concurrency {
                    let (index, content) = augment_inputs[next_augment_input].clone();
                    tasks.push(run_augment_task(
                        index,
                        content,
                        cfg.api_key.clone(),
                        cfg.base_url.clone(),
                        cfg.model.clone(),
                    ));
                    next_augment_input += 1;
                }

                if total_aug_count > 0
                    && (processed_aug_count.is_multiple_of(AUGMENT_PROGRESS_EVERY_NODES)
                        || processed_aug_count == total_aug_count)
                {
                    let span = (AUGMENT_PROGRESS_END - AUGMENT_PROGRESS_START) as usize;
                    let progress = AUGMENT_PROGRESS_START
                        + ((span * processed_aug_count) / total_aug_count) as i32;
                    let _ = kb.update_document_status_with_run_guard(
                        doc_id,
                        Some(STATUS_PROCESSING),
                        Some(progress.min(AUGMENT_PROGRESS_END)),
                        None,
                        None,
                        None,
                        None,
                        Some(run_id),
                    );
                    report_progress(
                        on_progress,
                        "augment",
                        &format!(
                            "已处理增强 {}/{} 个节点（成功 {} 个）",
                            processed_aug_count, total_aug_count, aug_count
                        ),
                    );
                }
            }

            if total_aug_count == 0 {
                report_progress(on_progress, "augment", "无可增强节点，跳过增强");
            }

            if aug_count > 0 {
                report_progress(
                    on_progress,
                    "augment",
                    &format!("成功增强 {} 个节点", aug_count),
                );
            }
        } else {
            report_progress(on_progress, "augment", "无可用 LLM 配置，跳过增强");
        }
    }

    // --- 阶段 3: 嵌入 ---
    // FIX9-02: 区分 embedding_failed 和 embedding_skipped（因隐私策略跳过）
    let mut embedding_failed = false;
    let embedding_skipped = false;
    if let Some(emb) = embedder.as_ref() {
        report_progress(
            on_progress,
            "embedding",
            &format!("嵌入 {} 个节点", nodes.len()),
        );
        // 修复：中间状态更新传入 run_id，防止旧 job 污染新 run 的文档状态
        kb.update_document_status_with_run_guard(
            doc_id,
            None,
            None,
            None,
            Some(STATUS_PROCESSING),
            Some(10),
            None,
            Some(run_id),
        )?;

        // 外部 embedding 始终允许
        // P1-012: embedding 使用 embedding_text，为空时 fallback 到 content
        let texts: Vec<&str> = nodes
            .iter()
            .map(|n| {
                if n.embedding_text.is_empty() {
                    n.content.as_str()
                } else {
                    n.embedding_text.as_str()
                }
            })
            .collect();
        match emb.embed_batch(&texts).await {
            Ok(vectors) => {
                // 向量按值移入节点，不再逐个 clone：几万节点 × 1536 维时 clone 会让峰值内存翻倍
                //（安卓上整条管道本就把全部节点与向量留在内存里到版本激活事务）。
                for (node, vector) in nodes.iter_mut().zip(vectors) {
                    node.vector = Some(vector);
                }
                // 修复：中间状态更新传入 run_id，防止旧 job 污染新 run 的文档状态
                kb.update_document_status_with_run_guard(
                    doc_id,
                    None,
                    None,
                    None,
                    Some(STATUS_PROCESSING),
                    Some(80),
                    None,
                    Some(run_id),
                )?;
            }
            Err(e) => {
                embedding_failed = true;
                report_progress(
                    on_progress,
                    "embedding",
                    &format!("嵌入失败: {}, 标记为 embedding_failed", e),
                );
                // 修复：中间状态更新传入 run_id，防止旧 job 污染新 run 的文档状态
                kb.update_document_status_with_run_guard(
                    doc_id,
                    None,
                    None,
                    None,
                    Some(STATUS_FAILED),
                    Some(80),
                    Some(&format!("embedding failed: {}", e)),
                    Some(run_id),
                )?;
            }
        }
    }

    // --- 阶段 4: RAPTOR ---
    // 外部摘要始终允许
    if library.raptor_enabled && nodes.len() >= 3 {
        if let Some(rc) = raptor_config {
            report_progress(on_progress, "raptor", "构建 RAPTOR 树");

            match raptor::resolve_raptor_llm_config(
                Some(&library),
                kb_raptor_secret_ref,
                kb_raptor_base_url,
                kb_raptor_model,
                resolved_raptor_config,
            ) {
                Ok(llm_config) => {
                    let max_tokens = rc.max_tokens_per_summary;
                    let llm_cfg = llm_config.clone();

                    // 将节点内容克隆为拥有的数据, 避免闭包中的生命周期问题

                    let result = raptor::build_raptor_tree(rc, &mut nodes, |cluster| {
                        let cfg = llm_cfg.clone();
                        // 克隆集群节点内容用于 LLM 调用
                        let cluster_texts: Vec<String> =
                            cluster.iter().map(|n| n.content.clone()).collect();
                        async move {
                            // 构造临时 RaptorNode 列表用于 summarize_cluster
                            let temp_nodes: Vec<RaptorNode> = cluster_texts
                                .iter()
                                .enumerate()
                                .map(|(i, content)| RaptorNode {
                                    id: i as i64,
                                    library_id: 0,
                                    document_id: 0,
                                    content: content.clone(),
                                    level: 0,
                                    parent_id: None,
                                    chunk_order: i as i32,
                                    vector: None,
                                    title_path: String::new(),
                                    page_number: None,
                                    source_start: None,
                                    source_end: None,
                                    node_metadata: String::new(),
                                    embedding_text: String::new(),
                                })
                                .collect();
                            let refs: Vec<&RaptorNode> = temp_nodes.iter().collect();
                            raptor::summarize_cluster(&refs, &cfg, max_tokens).await
                        }
                    })
                    .await;

                    match result {
                        Ok(()) => {
                            report_progress(
                                on_progress,
                                "raptor",
                                &format!("RAPTOR 树已构建: {} 个总节点", nodes.len()),
                            );
                        }
                        Err(e) => {
                            report_progress(
                                on_progress,
                                "raptor",
                                &format!("RAPTOR 失败: {}, 继续无摘要", e),
                            );
                        }
                    }
                }
                Err(e) => {
                    report_progress(
                        on_progress,
                        "raptor",
                        &format!("RAPTOR LLM 未配置: {}, 跳过", e),
                    );
                }
            }
        }
    }

    // --- 阶段 5: 持久化 ---
    // 修复：持久化前再次校验 run_id，防止 stale job 在通过初始校验后
    // 继续跑完后续阶段（新上传可能已更新 run_id）
    kb.ensure_document_run_current(doc_id, run_id)?;
    report_progress(
        on_progress,
        "persist",
        &format!("持久化 {} 个节点", nodes.len()),
    );
    // 修复：将 persist_nodes_and_vectors 和 update_document_stats_with_run_guard
    // 合入同一事务，消除两者之间的竞态窗口。旧代码先单独提交 nodes（事务 1），
    // 再单独更新 stats（事务 2）。若新上传在两者之间更新 processing_run_id，
    // stats 会失败（run_guard 拒绝），但旧 run 的 nodes 已经留下。
    // 合入同一事务后，如果 stats 的 run_guard 检查失败，整个事务回滚，
    // nodes 也不会留下。
    {
        let emb_status = if embedding_failed {
            Some(STATUS_FAILED)
        } else if embedding_skipped {
            Some(STATUS_SKIPPED)
        } else {
            None
        };
        let tx = conn.unchecked_transaction()?;
        let result = (|| -> Result<()> {
            // P1-1: active version 生命周期（异步路径）
            let content_hash = query_document_content_hash(conn, doc_id)?;
            let version_id = begin_document_version(conn, doc_id, Some(run_id), &content_hash)?;
            persist_nodes_for_version_inner(
                conn,
                doc_id,
                version_id,
                lib_id,
                &nodes,
                Some(run_id),
            )?;
            // P1 修复：表格索引移入版本激活事务，与节点/版本原子绑定。
            // Table granularity 的文档在此写入 kb_tables/kb_table_rows，
            // 通过 version_id 精确关联当前版本，避免版本切换时的数据不一致。
            if granularity == crate::kb::granularity::DocumentGranularity::Table {
                // 先清理同 version 的旧表格数据（重试场景）
                let _ = conn.execute(
                    "DELETE FROM kb_table_rows WHERE table_id IN \
                     (SELECT id FROM kb_tables WHERE document_id = ?1 AND version_id = ?2)",
                    rusqlite::params![doc_id, version_id],
                );
                let _ = conn.execute(
                    "DELETE FROM kb_tables WHERE document_id = ?1 AND version_id = ?2",
                    rusqlite::params![doc_id, version_id],
                );
                let table_blocks: &[crate::kb::types::ParsedBlock] = if normalized.blocks.is_empty()
                {
                    parsed.blocks.as_deref().unwrap_or(&[])
                } else {
                    &normalized.blocks
                };
                for block in table_blocks {
                    if block.block_type == "table" && !block.metadata.is_empty() {
                        if let Ok(sheet_data) =
                            serde_json::from_str::<serde_json::Value>(&block.metadata)
                        {
                            if let Some(name) = sheet_data.get("name").and_then(|v| v.as_str()) {
                                let headers: Vec<String> = sheet_data
                                    .get("headers")
                                    .and_then(|v| v.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                let row_count = sheet_data
                                    .get("row_count")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0)
                                    as i32;
                                match crate::kb::table_index::insert_table(
                                    conn, doc_id, version_id, name, &headers, row_count,
                                ) {
                                    Ok(table_id) => {
                                        if let Some(rows) =
                                            sheet_data.get("rows").and_then(|v| v.as_array())
                                        {
                                            for (ri, row) in rows.iter().enumerate() {
                                                let row_text = headers
                                                    .iter()
                                                    .filter_map(|h| {
                                                        row.get(h)
                                                            .and_then(|v| v.as_str())
                                                            .map(|s| format!("{}: {}", h, s))
                                                    })
                                                    .collect::<Vec<_>>()
                                                    .join(" ");
                                                let row_json =
                                                    serde_json::to_string(row).unwrap_or_default();
                                                if let Err(e) =
                                                    crate::kb::table_index::insert_table_row(
                                                        conn, table_id, ri as i32, &row_text,
                                                        &row_json,
                                                    )
                                                {
                                                    tracing::warn!(
                                                        "表格行插入失败 table_id={} row={}: {}",
                                                        table_id,
                                                        ri,
                                                        e
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "表格元数据创建失败 doc_id={} table_name={}: {}",
                                            doc_id,
                                            name,
                                            e
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
            activate_document_version(conn, doc_id, Some(run_id), version_id)?;
            // P1-2: 持久化媒体引用
            persist_media_assets(conn, doc_id, lib_id, version_id, &normalized.media_refs)?;
            // 摘要数据由 SummarizeDocument 命令或外部 engine.insert_summary 管理，
            // pipeline 不负责生成或清理摘要，避免误删外部写入的摘要数据。
            // 修复：外层已开事务，调用 _inner 避免嵌套 BEGIN（SQLite 不支持）
            kb.update_document_stats_with_run_guard_inner(
                doc_id,
                word_total,
                split_total,
                emb_status,
                Some(run_id),
            )?;
            Ok(())
        })();
        match result {
            Ok(_) => tx.commit()?,
            Err(e) => {
                let _ = tx.rollback();
                return Err(e);
            }
        }
    }

    // 同步内联 OCR 状态延迟更新：仅在节点持久化成功后才标记 OCR 完成，
    // 防止节点写入失败时残留 ocr_status=done。
    if ext == "pdf"
        && parsed
            .metadata
            .get("needs_ocr")
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(false)
    {
        let pdf_total: i32 = parsed
            .metadata
            .get("total_pages")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        crate::kb::ocr::update_document_ocr_status(conn, doc_id, pdf_total, Some(run_id))?;
    }
    finish_embedded_image_ocr(conn, doc_id, run_id, embedded_ocr_outcome)?;

    report_progress(
        on_progress,
        "done",
        &format!("文档处理完成: {} 个节点", nodes.len()),
    );

    Ok(ProcessResult {
        word_total,
        split_total,
        deferred_ocr: false,
    })
}

// ---------------------------------------------------------------------------
// 文档摘要刷新
// ---------------------------------------------------------------------------

/// 基于当前版本节点为文档生成摘要。
///
/// 读取 current_version 且未退役的 level-0 节点，取前 N 个节点的内容
/// 拼接为提取式摘要，写入 `kb_document_summaries`。
/// 调用方（SummarizeDocument worker）应先在事务外删除旧摘要，
/// 再调用本函数写入新摘要。
pub fn summarize_current_version(conn: &Connection, document_id: i64) -> Result<usize> {
    let kb = KbEngine::new(conn);

    // 读取当前版本的前几个节点内容作为摘要素材
    let nodes: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT n.content, COALESCE(d.title, d.original_name) AS doc_title \
             FROM kb_document_nodes n \
             JOIN kb_documents d ON d.id = n.document_id \
             WHERE n.document_id = ?1 \
             AND d.current_version_id IS NOT NULL \
             AND n.version_id = d.current_version_id \
             AND n.retired_at IS NULL \
             AND n.level = 0 \
             ORDER BY n.chunk_order, n.id \
             LIMIT 5",
        )?;
        let rows = stmt.query_map(rusqlite::params![document_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };

    if nodes.is_empty() {
        return Ok(0);
    }

    let doc_title = &nodes[0].1;
    // 提取式摘要：拼接前几个 chunk 的前 300 字符
    let summary_text: String = nodes
        .iter()
        .map(|(content, _)| content.chars().take(300).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n---\n");

    let full_summary = format!("文档: {}\n\n{}", doc_title, summary_text);

    // 清理旧摘要后写入新摘要
    let _ = conn.execute(
        "DELETE FROM kb_document_summaries WHERE document_id = ?1",
        rusqlite::params![document_id],
    );

    kb.insert_summary(document_id, None, "document", &full_summary, "extractive")?;
    Ok(1)
}

// ---------------------------------------------------------------------------
// 目录批量导入
// ---------------------------------------------------------------------------

/// 批量导入目录中所有支持的文件
///
/// 递归遍历目录, 解析每个支持的文件, 并通过 5 阶段管道处理。
/// 返回成功处理的文档数量。
pub async fn ingest_directory(
    conn: &Connection,
    library_id: i64,
    folder_id: Option<i64>,
    dir_path: &Path,
    _embedder: Option<Arc<Embedder>>,
    _raptor_config: Option<&RaptorConfig>,
    on_progress: Option<&ProgressCallback>,
) -> Result<usize> {
    if !dir_path.is_dir() {
        return Err(GBrainError::FileError(format!(
            "不是目录: {}",
            dir_path.display()
        )));
    }

    let supported_extensions: &[&str] = &[
        "pdf", "docx", "xls", "xlsx", "csv", "html", "htm", "txt", "md", "png", "jpg", "jpeg",
    ];

    let mut files: Vec<std::path::PathBuf> = Vec::new();
    collect_supported_files(dir_path, supported_extensions, &mut files);

    if files.is_empty() {
        report_progress(on_progress, "done", "未找到支持的文件");
        return Ok(0);
    }

    report_progress(
        on_progress,
        "ingest",
        &format!("找到 {} 个文件待导入", files.len()),
    );

    let mut doc_payloads: Vec<(String, KbProcessPayload)> = Vec::new();

    for (i, file_path) in files.iter().enumerate() {
        let original_name = file_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("file_{}", i));

        report_progress(
            on_progress,
            "ingest",
            &format!("[{}/{}] 创建文档记录 {}", i + 1, files.len(), original_name),
        );

        let ext = file_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("txt")
            .to_string();

        let run_id = crate::kb::jobs::new_run_id();

        let kb = KbEngine::new(conn);
        let file_data = match std::fs::read(file_path) {
            Ok(data) => data,
            Err(e) => {
                report_progress(
                    on_progress,
                    "ingest",
                    &format!("无法读取 {}: {}", original_name, e),
                );
                continue;
            }
        };
        let content_hash = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(&file_data))
        };
        let name_tokens = crate::nlp::chinese::tokenize_name(&original_name);
        let doc = Document {
            library_id,
            folder_id,
            original_name: original_name.clone(),
            name_tokens,
            file_size: file_data.len() as i64,
            content_hash,
            extension: ext.clone(),
            mime_type: crate::kb::types::mime_type_for_ext(&ext).to_string(),
            source_type: "ingest".to_string(),
            storage_path: file_path.to_string_lossy().to_string(),
            original_path: file_path.to_string_lossy().to_string(),
            job_id: String::new(),
            processing_run_id: run_id.clone(),
            ..Default::default()
        };
        let doc_id = match kb.create_document(&doc) {
            Ok(id) => id,
            Err(e) => {
                report_progress(
                    on_progress,
                    "ingest",
                    &format!("无法创建文档记录 {}: {}", original_name, e),
                );
                continue;
            }
        };

        let payload = KbProcessPayload {
            kind: "kb_process_document".to_string(),
            document_id: doc_id,
            library_id,
            processing_run_id: run_id,
            storage_path: file_path.to_string_lossy().to_string(),
            extension: ext,
        };
        doc_payloads.push((original_name, payload));
    }

    // 批量入队：所有文档创建完毕后统一入队 kb_process_document job，
    // 由 worker pool 异步并发处理，实现目录导入的吞吐目标。
    let mut enqueued = 0usize;
    for (original_name, payload) in &doc_payloads {
        match crate::kb::jobs::enqueue_kb_process_job(conn, payload) {
            Ok(job_id) => {
                enqueued += 1;
                tracing::info!(
                    doc_name = %original_name,
                    doc_id = payload.document_id,
                    job_id,
                    "ingest_directory: 已入队处理 job"
                );
            }
            Err(e) => {
                report_progress(
                    on_progress,
                    "ingest",
                    &format!("入队失败 {}: {}", original_name, e),
                );
            }
        }
    }

    report_progress(
        on_progress,
        "done",
        &format!(
            "已创建 {} 个文档记录，入队 {} 个处理 job",
            doc_payloads.len(),
            enqueued
        ),
    );

    Ok(enqueued)
}

/// 递归收集支持扩展名的文件
fn collect_supported_files(dir: &Path, extensions: &[&str], files: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_supported_files(&path, extensions, files);
            } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if extensions.contains(&ext.to_lowercase().as_str()) {
                    files.push(path);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 持久化与嵌入辅助函数
// ---------------------------------------------------------------------------

/// 在单个逻辑事务中持久化文档节点及其向量。
///
/// P1-1: 内部使用 active version 生命周期。
/// 调用方应先通过 `begin_document_version` 拿到 version_id，
/// 完成后再调用 `activate_document_version` 切换 active 指针。
pub fn persist_nodes_and_vectors(
    conn: &Connection,
    doc_id: i64,
    lib_id: i64,
    nodes: &[RaptorNode],
    run_id: Option<&str>,
) -> Result<()> {
    // 修复：如果提供了 run_id，在删除旧节点前校验 processing_run_id 仍匹配。
    // 这把 run 校验和写入放进同一事务，消除竞态窗口：
    // 旧 job 通过 ensure_document_run_current 后，新 run 可能立刻更新 processing_run_id，
    // 旧 job 的 persist 仍会删除新 run 的节点。条件删除确保 run_id 不匹配时操作被拒绝。
    let tx = conn.unchecked_transaction()?;
    let result = (|| -> Result<()> {
        // P1-1: 在事务内开新版本（status=building），并清理同版本残留节点
        let content_hash = query_document_content_hash(conn, doc_id)?;
        let version_id = begin_document_version(conn, doc_id, run_id, &content_hash)?;
        persist_nodes_for_version_inner(conn, doc_id, version_id, lib_id, nodes, run_id)?;
        activate_document_version(conn, doc_id, run_id, version_id)?;
        Ok(())
    })();
    match result {
        Ok(_) => tx.commit()?,
        Err(_) => {
            let _ = tx.rollback();
        }
    }
    result
}

/// 读取 kb_documents.content_hash（文档级指纹，用于版本去重）。
pub fn query_document_content_hash(conn: &Connection, doc_id: i64) -> Result<String> {
    conn.query_row(
        "SELECT content_hash FROM kb_documents WHERE id = ?1",
        [doc_id],
        |row| row.get(0),
    )
    .map_err(|e| GBrainError::Database(format!("查询 content_hash 失败: {}", e)))
}

/// P1-1: 开启一个新的 document version，状态为 building。
///
/// - 同一 (document_id, content_hash, index_status='ready') 已存在时，直接复用并复活该版本
///   （把 retired_at 清空、状态改回 building），避免重复重建。
/// - 否则插入新行，返回 version_id。
///
/// 调用方应在事务内调用，并随后调用 `persist_nodes_for_version_inner`
/// 与 `activate_document_version` 完成生命周期。
pub(crate) fn begin_document_version(
    conn: &Connection,
    doc_id: i64,
    run_id: Option<&str>,
    content_hash: &str,
) -> Result<i64> {
    // 校验 processing_run_id 仍是当前 run（与 persist 同样的 stale-job 守卫）
    if let Some(rid) = run_id {
        let current_run_id: String = conn
            .query_row(
                "SELECT processing_run_id FROM kb_documents WHERE id = ?1",
                [doc_id],
                |row| row.get(0),
            )
            .map_err(|e| GBrainError::Database(format!("查询 processing_run_id 失败: {}", e)))?;
        if current_run_id != rid {
            return Err(GBrainError::InvalidInput(
                "stale KB processing job; document has a newer run (begin_document_version)"
                    .to_string(),
            ));
        }
    }

    // 优先复用已 ready 且 content_hash 相同的历史版本（幂等重建场景）
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM kb_document_versions \
             WHERE document_id = ?1 AND content_hash = ?2 AND index_status = 'ready' \
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![doc_id, content_hash],
            |row| row.get(0),
        )
        .ok();
    if let Some(vid) = existing {
        // 复活已有版本：清退役时间，状态改回 building，等待重新写入节点
        conn.execute(
            "UPDATE kb_document_versions \
             SET index_status = 'building', retired_at = NULL, activated_at = NULL, \
                 processing_run_id = ?1 \
             WHERE id = ?2",
            rusqlite::params![run_id.unwrap_or(""), vid],
        )?;
        return Ok(vid);
    }

    // 否则插入新版本行
    conn.execute(
        "INSERT INTO kb_document_versions (document_id, version_label, processing_run_id, content_hash, index_status) \
         VALUES (?1, ?2, ?3, ?4, 'building')",
        rusqlite::params![doc_id, "", run_id.unwrap_or(""), content_hash],
    )?;
    Ok(conn.last_insert_rowid())
}

/// P1-1: 持久化指定 version 的节点。
///
/// - 仅清理同一 document + 同一 version 下的残留节点（重复处理同一 hash 时复用 version_id）。
///   绝不删除 current_version_id 指向的活跃版本节点，保证检索路径稳定。
/// - 插入新节点时写入 `version_id` 列。
pub(crate) fn persist_nodes_for_version_inner(
    conn: &Connection,
    doc_id: i64,
    version_id: i64,
    _lib_id: i64,
    nodes: &[RaptorNode],
    run_id: Option<&str>,
) -> Result<()> {
    // 再次校验 run_id（防止 begin 与 persist 之间被新 run 抢占）
    if let Some(rid) = run_id {
        let current_run_id: String = conn
            .query_row(
                "SELECT processing_run_id FROM kb_documents WHERE id = ?1",
                [doc_id],
                |row| row.get(0),
            )
            .map_err(|e| GBrainError::Database(format!("查询 processing_run_id 失败: {}", e)))?;
        if current_run_id != rid {
            return Err(GBrainError::InvalidInput(
                "stale KB processing job; document has a newer run (persist 阶段)".to_string(),
            ));
        }
    }

    // 仅清理同 version 的残留节点（重试场景：上次 building 失败留有部分节点）
    {
        let node_ids: Vec<i64> = {
            let mut stmt = conn.prepare(
                "SELECT id FROM kb_document_nodes WHERE document_id = ?1 AND version_id = ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![doc_id, version_id], |row| row.get(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        for &node_id in &node_ids {
            crate::kb::engine::cleanup_node_vectors(conn, node_id);
        }
        conn.execute(
            "DELETE FROM kb_document_nodes WHERE document_id = ?1 AND version_id = ?2",
            rusqlite::params![doc_id, version_id],
        )?;
    }

    // 按 level ASC, chunk_order ASC 排序插入, 确保父节点先于子节点
    let mut sorted_nodes: Vec<&RaptorNode> = nodes.iter().collect();
    sorted_nodes.sort_by_key(|n| (n.level, n.chunk_order));

    // 临时内存 ID → 数据库行 ID 映射
    let mut id_map: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();

    for node in &sorted_nodes {
        let mut content_tokens = chinese::tokenize_content(&node.content);

        // 如果 node_metadata 中包含增强的关键词/问题，追加到 content_tokens 以参与 FTS5 索引
        // 关键词和问题也需要经过中文分词，确保 FTS5 匹配一致性
        if !node.node_metadata.is_empty() && node.node_metadata != "{}" {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&node.node_metadata) {
                let extra_tokens: Vec<String> = ["keywords", "questions"]
                    .iter()
                    .filter_map(|&key| meta.get(key).and_then(|v| v.as_array()))
                    .flat_map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(crate::nlp::chinese::tokenize_content))
                    })
                    .collect();
                if !extra_tokens.is_empty() {
                    content_tokens = format!("{} {}", content_tokens, extra_tokens.join(" "));
                }
            }
        }

        conn.execute(
            "INSERT INTO kb_document_nodes \
             (library_id, document_id, version_id, content, content_tokens, level, chunk_order, \
              title_path, page_number, source_start, source_end, node_metadata, embedding_text) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                node.library_id,
                doc_id,
                version_id,
                node.content,
                content_tokens,
                node.level,
                node.chunk_order,
                node.title_path,
                node.page_number,
                node.source_start,
                node.source_end,
                node.node_metadata,
                node.embedding_text,
            ],
        )?;

        let db_id = conn.last_insert_rowid();
        id_map.insert(node.id, db_id);

        if node.level == 0 {
            // 修复：传入 node 在源文件中的字符基址，让 passage 的 source_start/source_end
            // 成为源文件绝对偏移；缺失时退化为 0，与历史行为兼容
            let node_base_offset = node.source_start.unwrap_or(0).max(0);
            crate::kb::passage::rebuild_passages_for_node(
                conn,
                db_id,
                node.library_id,
                doc_id,
                &node.content,
                node_base_offset,
            )?;
        }
    }

    // 更新 parent_id 关系 (临时 ID → 数据库 ID)
    for node in &sorted_nodes {
        if let Some(parent_temp_id) = node.parent_id {
            if let Some(&parent_db_id) = id_map.get(&parent_temp_id) {
                let &db_id = id_map.get(&node.id).ok_or_else(|| {
                    GBrainError::Database(format!(
                        "节点临时 ID {} 在 id_map 中不存在 (插入后应存在)",
                        node.id
                    ))
                })?;
                conn.execute(
                    "UPDATE kb_document_nodes SET parent_id = ?1 WHERE id = ?2",
                    rusqlite::params![parent_db_id, db_id],
                )?;
            }
        }
    }

    // 将向量写入 BLOB 回退表和 per-index vec 虚表（使用统一函数）
    for node in &sorted_nodes {
        if let Some(ref vector) = node.vector {
            let &db_id = id_map.get(&node.id).ok_or_else(|| {
                GBrainError::Database(format!(
                    "节点临时 ID {} 在 id_map 中不存在 (插入后应存在)",
                    node.id
                ))
            })?;

            // 解析该节点所属 library 的 active embedding index（含模型名）
            let (active_index_id, index_model): (i64, String) = conn
                .query_row(
                    "SELECT ei.id, ei.model FROM kb_embedding_indexes ei \
                 INNER JOIN kb_document_nodes dn ON dn.library_id = ei.library_id \
                 WHERE dn.id = ?1 AND ei.is_active = 1 LIMIT 1",
                    rusqlite::params![db_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|_| {
                    GBrainError::InvalidInput(format!(
                        "节点 {} 所属 library 没有 active embedding index",
                        db_id
                    ))
                })?;

            // P1 修复: 使用库 active index 的实际模型名，不再硬编码
            // 统一写入（BLOB 表 + per-index vec 表）
            crate::kb::embedding_index::upsert_node_embedding_for_index(
                conn,
                db_id,
                active_index_id,
                vector,
                vector.len() as i32,
                &index_model,
            )?;

            // 向后兼容：同步写入 legacy vec_kb_nodes
            let blob = embedding_to_blob(vector);
            // L1: legacy 向量索引清理失败时记录警告
            if let Err(e) = conn.execute(
                "DELETE FROM vec_kb_nodes WHERE node_id = ?1",
                rusqlite::params![db_id],
            ) {
                tracing::warn!(node_id = db_id, error = %e, "清理 legacy vec_kb_nodes 失败");
            }
            if let Err(e) = conn.execute(
                "INSERT INTO vec_kb_nodes (node_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![db_id, &blob],
            ) {
                tracing::warn!(node_id = db_id, error = %e, "写入 legacy vec_kb_nodes 失败");
            }
        }
    }

    // 更新 version 行的 node_count / char_count 统计，便于排查与监控
    let node_count = sorted_nodes.len() as i64;
    let char_count: i64 = sorted_nodes
        .iter()
        .map(|n| n.content.chars().count() as i64)
        .sum();
    conn.execute(
        "UPDATE kb_document_versions SET node_count = ?1, char_count = ?2 WHERE id = ?3",
        rusqlite::params![node_count, char_count, version_id],
    )?;

    Ok(())
}

/// P1-1: 激活版本，原子地把 `current_version_id` 切到新版本。
///
/// 操作步骤：
/// 1. 校验 run_id（同上）。
/// 2. 读取旧 current_version_id，标记其为 retired（retired_at = now, index_status='retired'）。
/// 3. 把新版本设为 ready（activated_at = now, index_status='ready'）。
/// 4. 更新 kb_documents.current_version_id 指向新版本。
/// 5. 递增 kb_index_state.index_version，使搜索缓存键失效。
///
/// 旧版本节点的退役清理（vec/embedding/passage）由后续 cleanup job 异步执行，
/// 当前函数仅做指针切换，保证检索路径原子可见性。
pub(crate) fn activate_document_version(
    conn: &Connection,
    doc_id: i64,
    run_id: Option<&str>,
    version_id: i64,
) -> Result<()> {
    if let Some(rid) = run_id {
        let current_run_id: String = conn
            .query_row(
                "SELECT processing_run_id FROM kb_documents WHERE id = ?1",
                [doc_id],
                |row| row.get(0),
            )
            .map_err(|e| GBrainError::Database(format!("查询 processing_run_id 失败: {}", e)))?;
        if current_run_id != rid {
            return Err(GBrainError::InvalidInput(
                "stale KB processing job; document has a newer run (activate_document_version)"
                    .to_string(),
            ));
        }
    }

    // 读取旧 current_version_id
    let old_version_id: Option<i64> = conn
        .query_row(
            "SELECT current_version_id FROM kb_documents WHERE id = ?1",
            [doc_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    // 标记旧版本为 retired（仅当与新版本不同时）
    if let Some(old) = old_version_id {
        if old != version_id {
            conn.execute(
                "UPDATE kb_document_versions \
                 SET index_status = 'retired', retired_at = datetime('now') \
                 WHERE id = ?1",
                rusqlite::params![old],
            )?;
        }
    }

    // 激活新版本
    conn.execute(
        "UPDATE kb_document_versions \
         SET index_status = 'ready', activated_at = datetime('now'), retired_at = NULL \
         WHERE id = ?1",
        rusqlite::params![version_id],
    )?;

    // 切换 kb_documents.current_version_id
    conn.execute(
        "UPDATE kb_documents SET current_version_id = ?1, updated_at = datetime('now') WHERE id = ?2",
        rusqlite::params![version_id, doc_id],
    )?;

    // 递增 index_version 使搜索缓存失效
    crate::kb::embedding_index::increment_index_version(conn, "kb_nodes")?;

    // P0 修复: 版本激活后异步清理退役版本的向量/节点数据。
    // 入队失败不阻塞版本激活（清理可后续手动触发）。
    if let Err(e) = crate::kb::jobs::enqueue_cleanup_retired_jobs(conn) {
        tracing::warn!(
            doc_id,
            version_id,
            error = %e,
            "入队退役版本清理作业失败（非致命）"
        );
    }

    Ok(())
}

/// P1-2: 把文档级别的媒体引用（图片/附件）持久化到 kb_media_assets。
///
/// 调用时机：在 `activate_document_version` 后，文档级 metadata 已稳定时。
/// 每个 media_ref 对应一行；`node_id` 可为 None（文档级），检索阶段可按
/// (document_id, media_type) JOIN 把媒体信息附加到结果。
///
/// 重复调用：先按 (document_id, version_id) 清理旧记录再插入，保证幂等。
pub(crate) fn persist_media_assets(
    conn: &Connection,
    doc_id: i64,
    lib_id: i64,
    version_id: i64,
    media_refs: &[crate::kb::types::MediaRef],
) -> Result<()> {
    // 先清理同版本旧记录（重试场景）
    conn.execute(
        "DELETE FROM kb_media_assets WHERE document_id = ?1 AND version_id = ?2",
        rusqlite::params![doc_id, version_id],
    )?;
    if media_refs.is_empty() {
        return Ok(());
    }
    for (idx, m) in media_refs.iter().enumerate() {
        conn.execute(
            "INSERT INTO kb_media_assets \
             (library_id, document_id, version_id, node_id, media_type, storage_path, \
              alt_text, ocr_text, caption, page_number, sort_order, mime_type, byte_size) \
             VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                lib_id,
                doc_id,
                version_id,
                m.media_type,
                m.storage_path,
                m.alt_text,
                m.ocr_text,
                m.caption,
                m.page_number,
                idx as i64,
                m.mime_type.as_deref().unwrap_or(""),
                m.byte_size.unwrap_or(0),
            ],
        )?;
    }
    Ok(())
}

pub async fn embed_nodes_for_document_version(
    conn: &Connection,
    document_id: i64,
    library_id: i64,
    processing_run_id: &str,
    embedder: Arc<Embedder>,
) -> Result<usize> {
    if !processing_run_id.is_empty() {
        KbEngine::new(conn).ensure_document_run_current(document_id, processing_run_id)?;
    }

    let active_index = crate::kb::embedding_index::get_active_index_for_library(conn, library_id)?
        .ok_or_else(|| {
            GBrainError::InvalidInput(format!(
                "library {} 没有 active embedding index",
                library_id
            ))
        })?;
    let Some(version_id) = staged_or_current_version_id(conn, document_id, processing_run_id)?
    else {
        return Err(GBrainError::InvalidInput(format!(
            "document {} 没有可嵌入的 building/current 版本",
            document_id
        )));
    };

    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT n.id, COALESCE(NULLIF(n.embedding_text, ''), n.content) \
             FROM kb_document_nodes n \
             WHERE n.document_id = ?1 AND n.version_id = ?2 AND n.retired_at IS NULL \
             AND NOT EXISTS (SELECT 1 FROM kb_node_embeddings ne \
                 WHERE ne.node_id = n.id AND ne.embedding_index_id = ?3) \
             ORDER BY n.level, n.chunk_order, n.id",
        )?;
        let mapped = stmt.query_map(
            rusqlite::params![document_id, version_id, active_index.id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )?;
        mapped.filter_map(|r| r.ok()).collect()
    };
    if rows.is_empty() {
        return Ok(0);
    }

    let texts: Vec<String> = rows.iter().map(|(_, text)| text.clone()).collect();
    let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let vectors = embedder.embed_batch(&text_refs).await?;
    if vectors.len() != rows.len() {
        return Err(GBrainError::Embedding(format!(
            "embedding 返回数量不匹配: expected {}, got {}",
            rows.len(),
            vectors.len()
        )));
    }

    let tx = conn.unchecked_transaction()?;
    let result = (|| -> Result<usize> {
        if !processing_run_id.is_empty() {
            KbEngine::new(conn).ensure_document_run_current(document_id, processing_run_id)?;
        }
        for ((node_id, _), vector) in rows.iter().zip(vectors.iter()) {
            crate::kb::embedding_index::upsert_node_embedding_for_index(
                conn,
                *node_id,
                active_index.id,
                vector,
                vector.len() as i32,
                &active_index.model,
            )?;

            let blob = embedding_to_blob(vector);
            if let Err(e) = conn.execute(
                "DELETE FROM vec_kb_nodes WHERE node_id = ?1",
                rusqlite::params![node_id],
            ) {
                tracing::warn!(node_id, error = %e, "清理 legacy vec_kb_nodes 失败");
            }
            if let Err(e) = conn.execute(
                "INSERT INTO vec_kb_nodes (node_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![node_id, &blob],
            ) {
                tracing::warn!(node_id, error = %e, "写入 legacy vec_kb_nodes 失败");
            }
        }
        // 外层已在 unchecked_transaction 内：必须用不开事务的 _inner 变体（嵌套 BEGIN 会失败并
        // 把上面刚写入的全部向量一起回滚，embedding 白付费）。
        KbEngine::new(conn).update_document_status_with_run_guard_inner(
            document_id,
            None,
            None,
            None,
            Some(STATUS_COMPLETED),
            Some(100),
            None,
            if processing_run_id.is_empty() {
                None
            } else {
                Some(processing_run_id)
            },
        )?;
        Ok(rows.len())
    })();
    match result {
        Ok(_) => tx.commit()?,
        Err(_) => {
            let _ = tx.rollback();
        }
    }
    result
}

pub fn finalize_index_version(
    conn: &Connection,
    document_id: i64,
    _library_id: i64,
    processing_run_id: &str,
) -> Result<i64> {
    let Some(version_id) = latest_building_version_id(conn, document_id, processing_run_id)? else {
        return Err(GBrainError::InvalidInput(format!(
            "document {} 没有可 finalize 的 building 版本",
            document_id
        )));
    };
    let run_guard = if processing_run_id.is_empty() {
        None
    } else {
        Some(processing_run_id)
    };
    let kb = KbEngine::new(conn);
    let tx = conn.unchecked_transaction()?;
    let result = (|| -> Result<i64> {
        activate_document_version(conn, document_id, run_guard, version_id)?;
        let split_total: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM kb_document_nodes \
                 WHERE document_id = ?1 AND version_id = ?2 AND level = 0 AND retired_at IS NULL",
                rusqlite::params![document_id, version_id],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let word_total: i32 = conn
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(content)), 0) FROM kb_document_nodes \
                 WHERE document_id = ?1 AND version_id = ?2 AND retired_at IS NULL",
                rusqlite::params![document_id, version_id],
                |row| row.get(0),
            )
            .unwrap_or(0);
        kb.update_document_stats_with_run_guard_inner(
            document_id,
            word_total,
            split_total,
            None,
            run_guard,
        )?;
        Ok(version_id)
    })();
    match result {
        Ok(_) => tx.commit()?,
        Err(_) => {
            let _ = tx.rollback();
        }
    }
    result
}

fn staged_or_current_version_id(
    conn: &Connection,
    document_id: i64,
    processing_run_id: &str,
) -> Result<Option<i64>> {
    if let Some(version_id) = latest_building_version_id(conn, document_id, processing_run_id)? {
        return Ok(Some(version_id));
    }
    let current = conn
        .query_row(
            "SELECT current_version_id FROM kb_documents WHERE id = ?1",
            rusqlite::params![document_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten();
    Ok(current)
}

fn latest_building_version_id(
    conn: &Connection,
    document_id: i64,
    processing_run_id: &str,
) -> Result<Option<i64>> {
    let mut sql = String::from(
        "SELECT id FROM kb_document_versions \
         WHERE document_id = ?1 AND index_status = 'building'",
    );
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(document_id)];
    if !processing_run_id.is_empty() {
        sql.push_str(" AND processing_run_id = ?2");
        params.push(Box::new(processing_run_id.to_string()));
    }
    sql.push_str(" ORDER BY id DESC LIMIT 1");
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(param_refs.as_slice())?;
    if let Some(row) = rows.next()? {
        Ok(Some(row.get(0)?))
    } else {
        Ok(None)
    }
}

/// P1-1 + P2: 清理退役版本（retired version）的物理数据。
///
/// 调用时机：active version 切换后，由 `kb_cleanup_retired_versions` job 异步触发，
/// 不阻塞前台检索。仅在 version.index_status='retired' 时执行，避免误删。
///
/// 清理顺序：
/// 1. 收集该版本下所有 node_id；
/// 2. 调用 `cleanup_node_vectors` 清理 vec 表 + BLOB；
/// 3. 删除 kb_document_nodes（CASCADE 触发 kb_passage_spans / FTS 等）；
/// 4. 删除 kb_media_assets（同 version_id）；
/// 5. 删除 kb_document_versions 行。
pub fn cleanup_retired_version(conn: &Connection, version_id: i64) -> Result<()> {
    // 校验版本状态，避免误删 building/ready 版本
    // 版本可能已被并发清理删除，幂等返回 Ok 而非报错
    let status: String = match conn.query_row(
        "SELECT index_status FROM kb_document_versions WHERE id = ?1",
        rusqlite::params![version_id],
        |row| row.get(0),
    ) {
        Ok(s) => s,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            tracing::warn!(
                version_id,
                "cleanup_retired_version: 版本不存在，可能已被并发清理，跳过"
            );
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    if status != "retired" {
        return Err(GBrainError::InvalidInput(format!(
            "version {} 状态为 {}，仅 retired 版本可清理",
            version_id, status
        )));
    }

    // 收集所有节点
    let node_ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM kb_document_nodes WHERE version_id = ?1")?;
        let rows = stmt.query_map(rusqlite::params![version_id], |row| row.get(0))?;
        rows.filter_map(|r| r.ok()).collect()
    };

    // 清理向量
    for &node_id in &node_ids {
        crate::kb::engine::cleanup_node_vectors(conn, node_id);
    }

    // 删除节点（CASCADE 触发 passage_spans/FTS）
    conn.execute(
        "DELETE FROM kb_document_nodes WHERE version_id = ?1",
        rusqlite::params![version_id],
    )?;

    // 删除表格索引（P1 修复：按 version_id 清理表格数据）
    conn.execute(
        "DELETE FROM kb_table_rows WHERE table_id IN \
         (SELECT id FROM kb_tables WHERE version_id = ?1)",
        rusqlite::params![version_id],
    )?;
    conn.execute(
        "DELETE FROM kb_tables WHERE version_id = ?1",
        rusqlite::params![version_id],
    )?;

    // 删除媒体资产
    conn.execute(
        "DELETE FROM kb_media_assets WHERE version_id = ?1",
        rusqlite::params![version_id],
    )?;

    // 删除版本行
    conn.execute(
        "DELETE FROM kb_document_versions WHERE id = ?1",
        rusqlite::params![version_id],
    )?;

    tracing::info!(version_id, node_count = node_ids.len(), "已清理退役版本");
    Ok(())
}

/// P2 修复: 删除文档索引（保留文件，仅清理节点/向量/版本/FTS）。
/// 调用时机：文档从库中移除或重新上传前清理旧索引。
pub fn delete_document_index(conn: &Connection, document_id: i64) -> Result<()> {
    // 收集所有节点 ID 用于清理向量
    let node_ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM kb_document_nodes WHERE document_id = ?1")?;
        let rows = stmt.query_map(rusqlite::params![document_id], |row| row.get(0))?;
        rows.filter_map(|r| r.ok()).collect()
    };

    // 清理向量
    for &node_id in &node_ids {
        crate::kb::engine::cleanup_node_vectors(conn, node_id);
    }

    // 删除表格索引（P1 修复：按 document_id 清理表格数据）
    conn.execute(
        "DELETE FROM kb_table_rows WHERE table_id IN \
         (SELECT id FROM kb_tables WHERE document_id = ?1)",
        rusqlite::params![document_id],
    )?;
    conn.execute(
        "DELETE FROM kb_tables WHERE document_id = ?1",
        rusqlite::params![document_id],
    )?;

    // 删除媒体资产
    conn.execute(
        "DELETE FROM kb_media_assets WHERE document_id = ?1",
        rusqlite::params![document_id],
    )?;

    // 删除节点（CASCADE 触发 passage_spans/FTS）
    conn.execute(
        "DELETE FROM kb_document_nodes WHERE document_id = ?1",
        rusqlite::params![document_id],
    )?;

    // 删除版本记录
    conn.execute(
        "DELETE FROM kb_document_versions WHERE document_id = ?1",
        rusqlite::params![document_id],
    )?;

    // FTS 内容触发器会级联清理，此处额外确保 FTS content 同步
    conn.execute(
        "INSERT INTO kb_doc_fts(kb_doc_fts, rowid, tokens, library_id, level) \
         VALUES ('delete', 0, '', 0, 0)",
        [],
    )
    .ok(); // 刷新 FTS 索引

    tracing::info!(document_id, node_count = node_ids.len(), "已删除文档索引");
    Ok(())
}

/// P2 修复: 巡检修复 library 下所有文档的 index_status。
/// 遍历 library 的文档，根据节点/向量/版本状态修正 index_status。
pub fn reconcile_library_index_status(conn: &Connection, library_id: i64) -> Result<()> {
    // 查找存在当前版本节点但 index_status 不一致的文档。
    // 仅检查 current_version 且未退役的节点（retired 节点不应影响 status 判定）。
    let inconsistent: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT d.id, d.index_status, \
             CASE WHEN EXISTS(\
               SELECT 1 FROM kb_document_nodes n \
               WHERE n.document_id = d.id \
               AND n.version_id = d.current_version_id \
               AND n.retired_at IS NULL\
             ) THEN 'ready' ELSE 'pending' END AS expected_status \
             FROM kb_documents d \
             WHERE d.library_id = ?1 AND d.deleted_at IS NULL AND d.document_status != 'deleted' \
             AND d.index_status != CASE WHEN EXISTS(\
               SELECT 1 FROM kb_document_nodes n \
               WHERE n.document_id = d.id \
               AND n.version_id = d.current_version_id \
               AND n.retired_at IS NULL\
             ) THEN 'ready' ELSE 'pending' END",
        )?;
        let rows = stmt.query_map(rusqlite::params![library_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let mut fixed = 0usize;
    for (doc_id, _current, expected) in &inconsistent {
        conn.execute(
            "UPDATE kb_documents SET index_status = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![expected, doc_id],
        )?;
        fixed += 1;
    }

    tracing::info!(
        library_id,
        total = inconsistent.len(),
        fixed,
        "已完成 library index_status 巡检"
    );
    Ok(())
}

// P0 修复: 在 activate_document_version 后入队 retired version 清理作业
pub fn enqueue_retired_cleanup_if_needed(conn: &Connection) -> Result<usize> {
    crate::kb::jobs::enqueue_cleanup_retired_jobs(conn)
}

/// 将 f32 嵌入向量转换为小端序 BLOB 用于 SQLite 存储
pub fn embedding_to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Count words in text, using jieba tokenization for Chinese content
/// and whitespace chunking for other text.
fn count_words(text: &str) -> usize {
    if crate::nlp::chinese::has_chinese(text) {
        let tokens = crate::nlp::chinese::tokenize_content(text);
        tokens.split_whitespace().count()
    } else {
        text.split_whitespace().count()
    }
}

/// 报告进度 (如果提供了回调)
fn report_progress(on_progress: Option<&ProgressCallback>, phase: &str, message: &str) {
    if let Some(cb) = on_progress {
        cb(phase, message);
    }
}

// ---------------------------------------------------------------------------
// PDF OCR 分支（Phase 1+2+3）
// ---------------------------------------------------------------------------

/// PDF OCR 分支入口：检测 → 规划 → 执行 OCR → 合并文本
///
/// 在 parse 后、split 前调用。行为：
/// 1. 读取 parsed.metadata.page_analyses 并运行 OcrDetector
/// 2. 如果 ocr_scope=none，更新 ocr_status=not_needed，返回原 parsed
/// 3. 检查 OCR 是否启用、外部 OCR 是否允许
/// 4. OCR 或外部 OCR 被关闭时，更新 ocr_status=needed，返回原 parsed
/// 5. 同步内联模式：规划 → 调用 GLM-OCR → 规范化 → 合并 → 返回新 parsed
/// 6. 异步模式：入队 kb_ocr_document job，返回 None 阻断后续本地索引
#[allow(clippy::too_many_arguments)]
fn enqueue_image_ocr(
    conn: &Connection,
    doc_id: i64,
    lib_id: i64,
    run_id: &str,
    storage_path: &str,
    ext: &str,
    _library: &Library,
    file_data: &[u8],
    config: &crate::config::Config,
) -> Result<()> {
    let ext_lower = ext.to_lowercase();
    // 校验文件内容 MIME，防止伪装扩展名的非图片文件进入外部 OCR
    let mime_type = match crate::kb::security::detect_and_validate_mime(file_data, &ext_lower) {
        Ok(mime) => mime,
        Err(e) => {
            // MIME 校验失败时标记文档为 failed，防止伪装文件在目录导入时留下处理中状态
            let fallback_mime = crate::kb::types::mime_type_for_ext(&ext_lower);
            // M-9 修复：不再用 let _ = 吞掉错误，改为记录警告
            if let Err(e2) = mark_image_ocr_unavailable(
                conn,
                doc_id,
                run_id,
                &ext_lower,
                fallback_mime,
                config,
                "failed",
                crate::kb::ocr::OcrStatus::Failed,
                &format!("图片 MIME 校验失败: {}", e),
            ) {
                tracing::warn!("标记图片 OCR 不可用失败: {}", e2);
            }
            return Err(e);
        }
    };

    if file_data.len() > MAX_OCR_IMAGE_BYTES {
        let reason = format!(
            "GLM-OCR image input exceeds 10MB limit ({} bytes)",
            file_data.len()
        );
        mark_image_ocr_unavailable(
            conn,
            doc_id,
            run_id,
            &ext_lower,
            &mime_type,
            config,
            "failed",
            crate::kb::ocr::OcrStatus::Failed,
            &reason,
        )?;
        return Err(GBrainError::InvalidInput(reason));
    }

    if !config.ocr_enabled {
        let reason = "global OCR is disabled (GBRAIN_OCR_ENABLED=false)";
        mark_image_ocr_unavailable(
            conn,
            doc_id,
            run_id,
            &ext_lower,
            &mime_type,
            config,
            "needed",
            crate::kb::ocr::OcrStatus::Needed,
            reason,
        )?;
        return Err(GBrainError::InvalidInput(reason.to_string()));
    }

    if config.ocr_api_key.is_none() {
        let reason = "missing OCR API key (GBRAIN_OCR_API_KEY)";
        mark_image_ocr_unavailable(
            conn,
            doc_id,
            run_id,
            &ext_lower,
            &mime_type,
            config,
            "failed",
            crate::kb::ocr::OcrStatus::Failed,
            reason,
        )?;
        return Err(GBrainError::InvalidInput(reason.to_string()));
    }

    let needs_ocr_pages = serde_json::json!([1]).to_string();
    let reasons_by_page = serde_json::json!({ "1": ["image_input"] }).to_string();
    let ocr_payload = crate::kb::jobs::KbOcrPayload {
        kind: "kb_ocr_document".to_string(),
        document_id: doc_id,
        library_id: lib_id,
        processing_run_id: run_id.to_string(),
        storage_path: storage_path.to_string(),
        pages: vec![1],
        submit_mode: config.ocr_submit_mode.clone(),
        provider: "glm_ocr".to_string(),
        model: config.ocr_model.clone(),
        return_crop_images: config.ocr_return_crop_images,
        need_layout_visualization: config.ocr_need_layout_visualization,
    };

    let tx = conn.unchecked_transaction()?;
    let enqueue_result = (|| -> Result<i64> {
        let rows = conn.execute(
            "UPDATE kb_documents SET document_granularity = 'micro', chunk_strategy = 'whole_document', \
             content_char_count = 0, page_count = ?1, parsing_status = ?2, parsing_progress = ?3, \
             document_status = 'ocr_pending', ocr_status = ?4, ocr_text_coverage = 0.0, \
             metadata_json = json_set(COALESCE(metadata_json, '{}'), \
             '$.needs_ocr_pages', json(?5), '$.ocr_reasons_by_page', json(?6), \
             '$.ocr_scope', 'full', '$.ocr_input_format', ?7, '$.ocr_input_mime', ?8), \
             updated_at = datetime('now') WHERE id = ?9 AND processing_run_id = ?10",
            rusqlite::params![
                OCR_IMAGE_TOTAL_PAGES,
                STATUS_PROCESSING,
                30,
                crate::kb::ocr::OcrStatus::Queued.as_str(),
                needs_ocr_pages,
                reasons_by_page,
                ext_lower,
                mime_type,
                doc_id,
                run_id
            ],
        )?;
        if rows == 0 {
            return Err(GBrainError::InvalidInput(
                "document processing_run_id changed; skipping image OCR enqueue".to_string(),
            ));
        }

        crate::kb::ocr::update_ocr_page_status(
            conn,
            doc_id,
            1,
            "pending",
            "image upload queued for OCR",
            "glm_ocr",
            &config.ocr_model,
            run_id,
        )?;
        crate::kb::jobs::enqueue_kb_ocr_job(conn, &ocr_payload)
    })();
    match enqueue_result {
        Ok(_) => tx.commit()?,
        Err(e) => {
            let _ = tx.rollback();
            return Err(e);
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn mark_image_ocr_unavailable(
    conn: &Connection,
    doc_id: i64,
    run_id: &str,
    ext: &str,
    mime_type: &str,
    config: &crate::config::Config,
    page_status: &str,
    ocr_status: crate::kb::ocr::OcrStatus,
    reason: &str,
) -> Result<()> {
    let needs_ocr_pages = serde_json::json!([1]).to_string();
    let reasons_by_page = serde_json::json!({ "1": ["image_input"] }).to_string();

    let tx = conn.unchecked_transaction()?;
    let result = (|| -> Result<()> {
        // 图片 OCR 不可用时，文档无法提取任何文本，必须将 document_status 和 index_status
        // 也设为 failed，否则文档会卡在 queued/ocr_pending 状态
        let rows = conn.execute(
            "UPDATE kb_documents SET document_granularity = 'micro', chunk_strategy = 'whole_document', \
             content_char_count = 0, page_count = ?1, parsing_status = ?2, parsing_progress = ?3, \
             parsing_error = ?4, ocr_status = ?5, ocr_text_coverage = 0.0, \
             document_status = 'failed', index_status = 'failed', \
             metadata_json = json_set(COALESCE(metadata_json, '{}'), \
             '$.needs_ocr_pages', json(?6), '$.ocr_reasons_by_page', json(?7), \
             '$.ocr_scope', 'full', '$.ocr_input_format', ?8, '$.ocr_input_mime', ?9), \
             updated_at = datetime('now') WHERE id = ?10 AND processing_run_id = ?11",
            rusqlite::params![
                OCR_IMAGE_TOTAL_PAGES,
                STATUS_FAILED,
                100,
                reason,
                ocr_status.as_str(),
                needs_ocr_pages,
                reasons_by_page,
                ext,
                mime_type,
                doc_id,
                run_id
            ],
        )?;
        if rows == 0 {
            return Err(GBrainError::InvalidInput(
                "document processing_run_id changed; skipping image OCR state update".to_string(),
            ));
        }
        crate::kb::ocr::update_ocr_page_status(
            conn,
            doc_id,
            1,
            page_status,
            reason,
            "glm_ocr",
            &config.ocr_model,
            run_id,
        )
    })();
    match result {
        Ok(_) => tx.commit()?,
        Err(e) => {
            let _ = tx.rollback();
            return Err(e);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn maybe_apply_pdf_ocr(
    conn: &Connection,
    doc_id: i64,
    lib_id: i64,
    run_id: &str,
    storage_path: &str,
    parsed: &ParsedDocument,
    _library: &crate::kb::types::Library,
    file_data: &[u8],
    config: &crate::config::Config,
) -> Result<Option<ParsedDocument>> {
    use crate::kb::ocr::OcrStatus;
    use crate::kb::ocr_detector::{detect_ocr_pages, PdfImageRegion, PdfPageAnalysis};
    use crate::kb::ocr_provider::OcrMode;

    let ocr_enabled = config.ocr_enabled;
    // 外部 OCR 始终允许，不再检查 library.external_ocr_allowed

    // 从 metadata 中读取页级分析
    let page_analyses_raw = parsed
        .metadata
        .get("page_analyses")
        .and_then(|v| serde_json::from_str::<Vec<serde_json::Value>>(v).ok())
        .unwrap_or_default();

    let total_pages: usize = parsed
        .metadata
        .get("total_pages")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    if total_pages == 0 || page_analyses_raw.is_empty() {
        // 无法确认 PDF 页数或页分析结果为空，标记 failed 而非 not_needed
        // not_needed 意味着明确不需要 OCR，但页数为 0 可能是解析异常
        let kb = KbEngine::new(conn);
        kb.update_document_ocr_with_run_guard(
            doc_id,
            OcrStatus::Failed.as_str(),
            0.0,
            Some(run_id),
        )?;
        return Ok(Some(parsed.clone()));
    }

    // 转换为 PdfPageAnalysis
    let page_analyses: Vec<PdfPageAnalysis> = page_analyses_raw
        .iter()
        .map(|pa| {
            let page_number = pa.get("page_number").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let text = pa
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let char_count = pa.get("char_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let image_area_ratio = pa
                .get("image_area_ratio")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let image_count = pa.get("image_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let has_vector = pa
                .get("has_vector_or_unknown_objects")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let image_regions: Vec<PdfImageRegion> = if image_count > 0 {
                (0..image_count)
                    .map(|_| PdfImageRegion {
                        bbox: None,
                        area_ratio: image_area_ratio / image_count as f64,
                    })
                    .collect()
            } else if image_area_ratio > 0.0 {
                vec![PdfImageRegion {
                    bbox: None,
                    area_ratio: image_area_ratio,
                }]
            } else {
                vec![]
            };

            PdfPageAnalysis {
                page_number,
                text,
                text_blocks: vec![],
                char_count,
                image_regions,
                image_area_ratio,
                has_vector_or_unknown_objects: has_vector,
                width: pa.get("width").and_then(|v| v.as_u64()).map(|v| v as u32),
                height: pa.get("height").and_then(|v| v.as_u64()).map(|v| v as u32),
                content_parse_failed: pa
                    .get("content_parse_failed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                has_vector_drawing_ops: pa
                    .get("has_vector_drawing_ops")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                has_invisible_text: pa
                    .get("has_invisible_text")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                font_encoding_suspected: pa
                    .get("font_encoding_suspected")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            }
        })
        .collect();

    // 运行 OCR 检测
    let ocr_mode = OcrMode::from_str(&config.ocr_mode);
    let mut detection = detect_ocr_pages(
        &page_analyses,
        config.ocr_text_density_threshold,
        config.ocr_image_area_threshold,
        config.ocr_image_count_threshold,
        &ocr_mode,
    );

    let kb = KbEngine::new(conn);

    let reasons_json: std::collections::BTreeMap<String, Vec<String>> = detection
        .reasons_by_page
        .iter()
        .map(|(page, reasons)| {
            let labels: Vec<String> = reasons
                .iter()
                .map(|r| {
                    serde_json::to_value(r)
                        .ok()
                        .and_then(|v| v.as_str().map(|s| s.to_string()))
                        .unwrap_or_default()
                })
                .collect();
            (page.to_string(), labels)
        })
        .collect();
    let needs_ocr_pages_str =
        serde_json::to_string(&detection.ocr_pages).unwrap_or_else(|_| "[]".to_string());
    let reasons_str = serde_json::to_string(&reasons_json).unwrap_or_else(|_| "{}".to_string());
    let ocr_scope = match detection.scope() {
        crate::kb::ocr_detector::OcrScope::None => "none",
        crate::kb::ocr_detector::OcrScope::Partial => "partial",
        crate::kb::ocr_detector::OcrScope::Full => "full",
    };

    // 在处理分支前统一持久化 OCR 检测元数据到文档 metadata_json，
    // 确保所有路径（not_needed、策略阻断、失败、inline OCR、异步 OCR）的状态接口
    // 均可查询 needs_ocr_pages、ocr_reasons_by_page、ocr_scope。
    conn.execute(
        "UPDATE kb_documents SET metadata_json = json_set(COALESCE(metadata_json, '{}'), \
         '$.needs_ocr_pages', json(?2), '$.ocr_reasons_by_page', json(?3), '$.ocr_scope', ?4), \
         updated_at = datetime('now') WHERE id = ?1 AND processing_run_id = ?5",
        rusqlite::params![doc_id, needs_ocr_pages_str, reasons_str, ocr_scope, run_id],
    )?;

    // 如果不需要 OCR
    if !detection.needs_ocr {
        tracing::info!(doc_id, "PDF OCR 检测: 不需要 OCR (ocr_scope=none)");
        kb.update_document_ocr_with_run_guard(
            doc_id,
            OcrStatus::NotNeeded.as_str(),
            0.0,
            Some(run_id),
        )?;
        return Ok(Some(parsed.clone()));
    }

    tracing::info!(
        doc_id,
        ocr_pages = ?detection.ocr_pages,
        "PDF OCR 检测: 需要 OCR"
    );

    // 限制 OCR 页数上限
    let max_pages = config.ocr_max_pages_per_document.max(1);
    if detection.ocr_pages.len() > max_pages {
        tracing::warn!(
            doc_id,
            ocr_page_count = detection.ocr_pages.len(),
            max_pages,
            "OCR 页数超过单文档上限，截断为前 {} 页",
            max_pages
        );
        // 超出上限的页标记为 skipped，确保有可见状态
        let skipped_pages: Vec<i32> = detection.ocr_pages[max_pages..].to_vec();
        crate::kb::ocr::update_ocr_pages_status(
            conn,
            doc_id,
            &skipped_pages,
            "skipped",
            &format!("超出单文档 OCR 页数上限 ({})", max_pages),
            "glm_ocr",
            &config.ocr_model,
            run_id,
        )?;
        tracing::info!(
            doc_id,
            skipped_count = skipped_pages.len(),
            "已将超出上限的 {} 页标记为 skipped",
            skipped_pages.len()
        );
        detection.ocr_pages.truncate(max_pages);
        detection.needs_ocr = !detection.ocr_pages.is_empty();
    }

    // 计算文档总页数，供后续分支的 OCR 状态聚合使用
    let doc_page_count: i32 = parsed
        .metadata
        .get("total_pages")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // 检查全局 OCR 是否启用
    if !ocr_enabled {
        tracing::warn!(doc_id, ocr_enabled, "PDF 需要 OCR 但全局 OCR 已关闭");
        let reason = "全局 OCR 已关闭 (GBRAIN_OCR_ENABLED=false)";
        crate::kb::ocr::update_ocr_pages_status(
            conn,
            doc_id,
            &detection.ocr_pages,
            "needed",
            reason,
            "glm_ocr",
            &config.ocr_model,
            run_id,
        )?;
        // 通过聚合函数更新文档 OCR 状态，确保 skipped + needed → partial
        crate::kb::ocr::update_document_ocr_status(conn, doc_id, doc_page_count, Some(run_id))?;
        return Ok(Some(parsed.clone()));
    }

    // 脱敏已关闭，外部 OCR 始终允许

    // 检查 API key
    if config.ocr_api_key.is_none() {
        tracing::error!(doc_id, "PDF 需要 OCR 但未配置 GBRAIN_OCR_API_KEY");
        crate::kb::ocr::update_ocr_pages_status(
            conn,
            doc_id,
            &detection.ocr_pages,
            "failed",
            "未配置 OCR API key (GBRAIN_OCR_API_KEY)",
            "glm_ocr",
            &config.ocr_model,
            run_id,
        )?;
        // 通过聚合函数更新文档 OCR 状态，确保 skipped + failed → partial
        crate::kb::ocr::update_document_ocr_status(conn, doc_id, doc_page_count, Some(run_id))?;
        // 将错误同步写入文档级 parsing_error，确保状态接口可展示失败原因
        kb.update_document_status_with_run_guard(
            doc_id,
            None,
            None,
            Some("未配置 OCR API key (GBRAIN_OCR_API_KEY)"),
            None,
            None,
            None,
            Some(run_id),
        )?;
        return Ok(Some(parsed.clone()));
    }

    // 异步模式：入队 kb_ocr_document job，返回 None 阻断后续 split/embed/persist
    if !config.ocr_sync_inline {
        tracing::info!(doc_id, "PDF OCR: 入队异步 OCR job");

        // 主 pipeline 会在异步 OCR 入队后提前返回，因此这里必须持久化
        // PDF 页数；否则 kb_ocr_retry 的严格页码校验拿不到总页数。
        // 这些文档状态、页状态和 job 入队必须同事务提交，否则 OCR worker
        // 可能在 job 可见后立刻跑完，又被外层/后续状态更新覆盖回 queued/ocr_pending。
        let char_count = parsed.content.chars().count();
        // P0-4: 同步写入 content_token_count(精确启发式)
        let token_count = crate::kb::token_counter::count_tokens_heuristic(&parsed.content);
        let granularity =
            crate::kb::granularity::classify_granularity("pdf", char_count, total_pages);
        let chunk_strategy = crate::kb::granularity::chunk_strategy_for(granularity);

        // 入队 OCR job
        let ocr_payload = crate::kb::jobs::KbOcrPayload {
            kind: "kb_ocr_document".to_string(),
            document_id: doc_id,
            library_id: lib_id,
            processing_run_id: run_id.to_string(),
            storage_path: storage_path.to_string(),
            pages: detection.ocr_pages.clone(),
            submit_mode: config.ocr_submit_mode.clone(),
            provider: "glm_ocr".to_string(),
            model: config.ocr_model.clone(),
            return_crop_images: config.ocr_return_crop_images,
            need_layout_visualization: config.ocr_need_layout_visualization,
        };

        let ocr_payload_json = serde_json::to_value(&ocr_payload)
            .map_err(|e| GBrainError::Serialization(e.to_string()))?;

        let tx = conn.unchecked_transaction()?;
        let enqueue_result = (|| -> Result<i64> {
            let rows = conn.execute(
                "UPDATE kb_documents SET document_granularity = ?1, chunk_strategy = ?2, \
                 content_char_count = ?3, content_token_count = ?4, page_count = ?5, parsing_status = ?6, \
                 parsing_progress = ?7, document_status = 'ocr_pending', \
                 ocr_status = ?8, ocr_text_coverage = 0.0, updated_at = datetime('now') \
                 WHERE id = ?9 AND processing_run_id = ?10",
                rusqlite::params![
                    granularity.as_str(),
                    chunk_strategy,
                    char_count as i32,
                    token_count as i32,
                    total_pages as i32,
                    STATUS_PROCESSING,
                    30,
                    OcrStatus::Queued.as_str(),
                    doc_id,
                    run_id
                ],
            )?;
            if rows == 0 {
                return Err(GBrainError::InvalidInput(
                    "文档 processing_run_id 已变化，跳过异步 OCR 入队".to_string(),
                ));
            }

            // 为每个 OCR 页创建 pending 状态记录，并持久化该页的 OCR 检测原因。
            // CLI 状态查询（kb ocr-status）读页表，写入原因后用户可看到"为什么该页需要 OCR"。
            for &page_num in &detection.ocr_pages {
                let page_reasons = detection
                    .reasons_by_page
                    .get(&page_num)
                    .map(|reasons| {
                        let labels: Vec<String> = reasons
                            .iter()
                            .filter_map(|r| serde_json::to_value(r).ok())
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect();
                        labels.join(", ")
                    })
                    .unwrap_or_default();
                let reason_desc = if page_reasons.is_empty() {
                    "等待异步 OCR 处理".to_string()
                } else {
                    format!("OCR 原因: {} | 等待异步 OCR 处理", page_reasons)
                };
                crate::kb::ocr::update_ocr_page_status(
                    conn,
                    doc_id,
                    page_num,
                    "pending",
                    &reason_desc,
                    "glm_ocr",
                    &config.ocr_model,
                    run_id,
                )?;
            }

            let queue = crate::jobs::JobQueue::new(conn);
            queue.enqueue(crate::jobs::JobInput {
                job_type: "kb_ocr_document".to_string(),
                payload: ocr_payload_json,
                priority: Some(0),
                max_attempts: Some(3),
            })
        })();
        match enqueue_result {
            Ok(_) => tx.commit()?,
            Err(e) => {
                let _ = tx.rollback();
                return Err(e);
            }
        };

        // 异步 OCR 入队成功：返回 None 阻断后续 split/embed/persist
        // 文档状态保持 ocr_status=queued，document_status 不会被标为 ready
        // OCR 完成后由 OCR worker 调用 writeback_ocr_results 完成后续流程
        return Ok(None);
    }

    // 同步内联模式：直接执行 OCR
    tracing::info!(doc_id, "PDF OCR: 同步内联执行 OCR");
    kb.update_document_ocr_with_run_guard(
        doc_id,
        OcrStatus::Processing.as_str(),
        0.0,
        Some(run_id),
    )?;

    match execute_inline_ocr(
        conn,
        doc_id,
        lib_id,
        run_id,
        &page_analyses,
        &detection,
        file_data,
        total_pages as i32,
        config,
    ) {
        Ok(merged_results) => {
            // 用合并后的内容替换原 parsed 的 content
            let merged_content = crate::kb::ocr_merge::merged_results_to_content(&merged_results);
            let mut new_parsed = parsed.clone();
            new_parsed.content = merged_content;

            // 修复：从合并结果重建 blocks，使后续 split 的页码/source span 匹配正确。
            // 原代码只更新 content 不重建 blocks，导致 split 用新 OCR 正文切块时
            // 仍用原 PDF 文本层 blocks 做页码/span 匹配，扫描页容易得到错误页码。
            new_parsed.blocks = Some(build_blocks_from_merged(&merged_results));

            // 更新 page_texts metadata（从 page_analyses 重新生成）
            let page_texts: Vec<String> = page_analyses.iter().map(|pa| pa.text.clone()).collect();
            new_parsed.metadata.insert(
                "page_texts".to_string(),
                serde_json::to_string(&page_texts).unwrap_or_default(),
            );
            new_parsed
                .metadata
                .insert("needs_ocr".to_string(), detection.needs_ocr.to_string());
            new_parsed.metadata.insert(
                "ocr_pages".to_string(),
                serde_json::to_string(&detection.ocr_pages).unwrap_or_default(),
            );
            new_parsed.metadata.insert(
                "ocr_reasons_by_page".to_string(),
                serde_json::to_string(&reasons_json).unwrap_or_default(),
            );

            // 聚合 OCR 页状态，判断是否有成功结果。
            // execute_inline_ocr 在所有 chunk 都失败时仍返回 Ok，
            // 因此必须在此处根据实际页状态决定是否清空文档级错误。
            // 只读聚合：仅在节点持久化前判断是否需要清空文档错误，
            // 不写入 ocr_status，防止后续 split/节点写入失败时残留错误终态。
            let (final_ocr_status, _coverage) =
                crate::kb::ocr::compute_ocr_status(conn, doc_id, total_pages as i32, Some(run_id))?;
            if final_ocr_status == OcrStatus::Failed {
                // 全失败：将脱敏后的错误写入文档级 parsing_error
                let failed_error = "同步内联 OCR 全部页面处理失败";
                kb.update_document_status_with_run_guard(
                    doc_id,
                    None,
                    None,
                    Some(failed_error),
                    None,
                    None,
                    None,
                    Some(run_id),
                )?;
            } else {
                // 存在成功结果：清除可能由上一次失败写入的文档级错误
                kb.update_document_status_with_run_guard(
                    doc_id,
                    None,
                    None,
                    Some(""),
                    None,
                    None,
                    None,
                    Some(run_id),
                )?;
            }

            Ok(Some(new_parsed))
        }
        Err(e) => {
            let safe_error = crate::kb::ocr::sanitize_error_text_with_secret(
                &e.to_string(),
                config.ocr_api_key.as_deref(),
            );
            tracing::error!(doc_id, error = %safe_error, "PDF OCR 执行失败");
            kb.update_document_ocr_with_run_guard(
                doc_id,
                OcrStatus::Failed.as_str(),
                0.0,
                Some(run_id),
            )?;
            // 全失败时将脱敏后的错误写入文档级 parsing_error
            kb.update_document_status_with_run_guard(
                doc_id,
                None,
                None,
                Some(&safe_error),
                None,
                None,
                None,
                Some(run_id),
            )?;
            // OCR 失败时返回原始 parsed，不阻塞后续文本层索引
            Ok(Some(parsed.clone()))
        }
    }
}

/// 从 OCR 合并结果构建 ParsedBlock 列表，offset 与 merged_results_to_content 输出对齐
///
/// 每个非空页生成一个 ParsedBlock，source_start/source_end 基于
/// `[PAGE:N]\n{text}` 格式在全文中的字符偏移计算，确保后续 split
/// 的 span 匹配能正确定位页码。
fn build_blocks_from_merged(merged: &[crate::kb::ocr_merge::MergedPageResult]) -> Vec<ParsedBlock> {
    let mut blocks = Vec::new();
    let mut offset = 0i32;
    for page in merged {
        if page.text.trim().is_empty() {
            continue;
        }
        let marker = format!("[PAGE:{}]\n", page.page_number);
        let entry_chars = marker.chars().count() as i32 + page.text.chars().count() as i32;
        blocks.push(ParsedBlock {
            text: page.text.clone(),
            title_path: String::new(),
            page_number: Some(page.page_number),
            source_start: Some(offset),
            source_end: Some(offset + entry_chars),
            block_type: if page.used_ocr { "ocr_text" } else { "text" }.to_string(),
            metadata: String::new(),
        });
        offset += entry_chars + 2; // +2 for "\n\n" between pages
    }
    blocks
}

/// 同步内联执行 OCR：规划 → 调用 GLM-OCR → 合并文本
#[allow(clippy::too_many_arguments)]
fn execute_inline_ocr(
    conn: &Connection,
    doc_id: i64,
    lib_id: i64,
    run_id: &str,
    page_analyses: &[crate::kb::ocr_detector::PdfPageAnalysis],
    detection: &crate::kb::ocr_detector::OcrDetection,
    file_data: &[u8],
    total_pages: i32,
    config: &crate::config::Config,
) -> Result<Vec<crate::kb::ocr_merge::MergedPageResult>> {
    use crate::kb::ocr_glm::{build_ocr_options_from_config, pdf_to_base64, GlmOcrProvider};
    use crate::kb::ocr_merge::merge_text_and_ocr;
    use crate::kb::ocr_planner::plan_ocr_requests;
    use crate::kb::ocr_provider::{OcrFilePayload, OcrInput, OcrSubmitMode};

    let options = build_ocr_options_from_config(config);
    let submit_mode = OcrSubmitMode::from_str(&config.ocr_submit_mode);

    // 创建临时目录：包含 run_id 避免同一文档的并发 job 相互覆盖/删除
    // 使用 RAII 守卫确保任何退出路径都自动清理
    let mut temp_guard = crate::kb::temp_guard::TempOcrDir::create(
        &format!("gbrain_ocr_{}_{}", doc_id, run_id),
        file_data.len() as u64,
        config.ocr_temp_dir_max_bytes,
    )?;

    // 规划 OCR 请求（内部按实际输出文件字节数申请临时目录预算）
    let plan = plan_ocr_requests(
        doc_id,
        "",
        file_data,
        total_pages,
        &detection.ocr_pages,
        options.max_pages_per_request,
        options.max_pdf_bytes_per_request,
        &submit_mode,
        config.ocr_temp_dir_max_bytes,
        &mut temp_guard,
    )?;

    // 执行每个请求块
    let api_key = config.ocr_api_key.as_deref().unwrap_or("");
    let provider = GlmOcrProvider::new(api_key);
    let mut all_ocr_results: Vec<crate::kb::ocr_provider::OcrPageResult> = Vec::new();

    for chunk in &plan.chunks {
        // 拆分失败的页段：跳过 OCR 请求，直接标记为 failed
        if chunk.split_failed {
            tracing::warn!(
                doc_id,
                start = chunk.source_start_page,
                end = chunk.source_end_page,
                "同步 OCR: 页段拆分失败，跳过 OCR 并标记为 failed"
            );
            // 持久化失败时向上返回错误，避免静默产生不可信状态
            for page_num in chunk.source_start_page..=chunk.source_end_page {
                crate::kb::ocr::update_ocr_page_status(
                    conn,
                    doc_id,
                    page_num,
                    "failed",
                    "PDF 页段拆分失败，无法提交 OCR",
                    "glm_ocr",
                    &config.ocr_model,
                    run_id,
                )?;
            }
            continue;
        }

        // 准备输入
        let file_payload = if let Some(ref split_path) = chunk.split_pdf_path {
            // 使用拆分后的 PDF 子文件
            let split_data = std::fs::read(split_path)?;
            OcrFilePayload::Base64(pdf_to_base64(&split_data))
        } else {
            // 使用原始 PDF
            OcrFilePayload::Base64(pdf_to_base64(file_data))
        };

        let input = OcrInput::PdfRange {
            file: file_payload,
            request_start_page_id: chunk.request_start_page_id,
            request_end_page_id: chunk.request_end_page_id,
            source_start_page: chunk.source_start_page,
            source_end_page: chunk.source_end_page,
            document_id: doc_id,
            run_id: run_id.to_string(),
        };

        // 指数退避重试：对可重试错误（429/503/timeout/网络）重试最多 3 次
        let mut retry_count = 0u32;
        loop {
            // 获取 OCR 并发许可（与异步 worker 共享同一信号量），
            // 防止同步内联路径绕过 ocr_max_concurrency 限制
            let _ocr_permit = match crate::kb::worker::try_acquire_ocr_permit(
                config.ocr_max_concurrency,
                std::time::Duration::from_secs(30),
            ) {
                Some(permit) => permit,
                None => {
                    retry_count += 1;
                    if retry_count < 3 {
                        let backoff_secs = 2u64 * 2u64.pow(retry_count - 1);
                        tracing::warn!(
                            doc_id,
                            retry_count,
                            backoff_secs,
                            "同步 OCR: 并发槽位已满，等待后重试"
                        );
                        std::thread::sleep(std::time::Duration::from_secs(backoff_secs));
                        continue;
                    }
                    return Err(GBrainError::Config(
                        "OCR 并发槽位已满，同步内联 OCR 超时放弃".to_string(),
                    ));
                }
            };

            let call_started = std::time::Instant::now();
            let recognition = provider.recognize_off_async_context(&input, &options);
            let latency_ms = call_started.elapsed().as_millis().min(i32::MAX as u128) as i32;

            match recognition {
                Ok(results) => {
                    let results =
                        crate::kb::ocr::sanitize_ocr_page_results(&results, Some(api_key));
                    crate::kb::ocr::log_ocr_external_model_call(
                        conn,
                        lib_id,
                        doc_id,
                        "glm_ocr",
                        &config.ocr_model,
                        latency_ms,
                        true,
                        "",
                        &results,
                        Some(api_key),
                    );
                    // 持久化页级和块级结果
                    // 持久化 OCR 结果失败时向上返回错误，避免页面结果丢失但文档显示完成
                    if !results.is_empty() {
                        crate::kb::ocr::persist_ocr_page_results(
                            conn,
                            doc_id,
                            run_id,
                            &results,
                            Some(api_key),
                        )?;
                        crate::kb::ocr::persist_ocr_blocks(
                            conn,
                            doc_id,
                            run_id,
                            &results,
                            Some(api_key),
                        )?;
                    }
                    // 标记未返回的页为 failed（处理多页请求部分失败的情况）
                    let returned_pages: std::collections::HashSet<i32> =
                        results.iter().map(|r| r.page_number).collect();
                    for page_num in chunk.source_start_page..=chunk.source_end_page {
                        if !returned_pages.contains(&page_num) {
                            crate::kb::ocr::update_ocr_page_status(
                                conn,
                                doc_id,
                                page_num,
                                "failed",
                                "OCR 请求未返回该页结果（部分失败）",
                                "glm_ocr",
                                &config.ocr_model,
                                run_id,
                            )?;
                        }
                    }
                    all_ocr_results.extend(results);
                    break;
                }
                Err(e) => {
                    let error_str = e.to_string();
                    let safe_error_str =
                        crate::kb::ocr::sanitize_error_text_with_secret(&error_str, Some(api_key));
                    crate::kb::ocr::log_ocr_external_model_call(
                        conn,
                        lib_id,
                        doc_id,
                        "glm_ocr",
                        &config.ocr_model,
                        latency_ms,
                        false,
                        &error_str,
                        &[],
                        Some(api_key),
                    );
                    let lower = error_str.to_lowercase();
                    let retryable = lower.contains("429")
                        || lower.contains("rate limit")
                        || lower.contains("503")
                        || lower.contains("timeout")
                        || lower.contains("timed out")
                        || lower.contains("connection")
                        || lower.contains("hyper");

                    if retryable && retry_count < 3 {
                        retry_count += 1;
                        let backoff_secs = 2u64 * 2u64.pow(retry_count - 1);
                        tracing::warn!(
                            doc_id,
                            start = chunk.source_start_page,
                            end = chunk.source_end_page,
                            retry = retry_count,
                            backoff_secs,
                            error = %safe_error_str,
                            "同步 OCR 请求遇到可重试错误，指数退避重试"
                        );
                        std::thread::sleep(std::time::Duration::from_secs(backoff_secs));
                        continue;
                    }

                    tracing::warn!(
                        doc_id,
                        start = chunk.source_start_page,
                        end = chunk.source_end_page,
                        error = %safe_error_str,
                        "OCR 请求失败"
                    );
                    // 标记失败页（脱敏错误信息，防止 API 响应正文泄露到数据库）
                    for page_num in chunk.source_start_page..=chunk.source_end_page {
                        crate::kb::ocr::update_ocr_page_status(
                            conn,
                            doc_id,
                            page_num,
                            "failed",
                            &safe_error_str,
                            "glm_ocr",
                            &config.ocr_model,
                            run_id,
                        )?;
                    }
                    break;
                }
            }
        }
    }

    // 合并文本层与 OCR 结果
    let merged = merge_text_and_ocr(page_analyses, &all_ocr_results, &detection.ocr_pages);

    // 注意：不在此处更新文档 OCR 状态，状态更新延迟到节点持久化完成后执行，
    // 防止 ocr_status=done 但节点写入失败导致的不一致。

    // temp_guard 在此处 drop，自动清理临时目录
    drop(temp_guard);

    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_media_ref(
        storage_path: &str,
        mime_type: Option<&str>,
        embedded_data_base64: Option<&str>,
    ) -> crate::kb::types::MediaRef {
        crate::kb::types::MediaRef {
            media_type: "image".to_string(),
            storage_path: storage_path.to_string(),
            mime_type: mime_type.map(str::to_string),
            byte_size: Some(3),
            embedded_data_base64: embedded_data_base64.map(str::to_string),
            alt_text: None,
            ocr_text: None,
            caption: None,
            page_number: None,
        }
    }

    #[test]
    fn embedded_image_ocr_candidates_keep_supported_inline_images() {
        let refs = vec![
            test_media_ref(
                "embedded://docx/word/media/image1.png",
                Some("image/png"),
                Some("abc"),
            ),
            test_media_ref(
                "embedded://docx/word/media/image2.gif",
                Some("image/gif"),
                Some("abc"),
            ),
            test_media_ref("https://example.com/image.jpg", Some("image/jpeg"), None),
        ];

        let candidates = embedded_image_ocr_candidates(&refs);

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].storage_path,
            "embedded://docx/word/media/image1.png"
        );
        assert_eq!(candidates[0].mime_type, "image/png");
    }

    #[test]
    fn embedded_image_ocr_outcome_tracks_final_status() {
        let done = embedded_image_ocr_outcome(2, 2);
        assert_eq!(done.status, OcrStatus::Done);
        assert!((done.coverage - 1.0).abs() < f64::EPSILON);

        let partial = embedded_image_ocr_outcome(2, 1);
        assert_eq!(partial.status, OcrStatus::Partial);
        assert!((partial.coverage - 0.5).abs() < f64::EPSILON);

        let failed = embedded_image_ocr_outcome(2, 0);
        assert_eq!(failed.status, OcrStatus::Failed);
        assert!((failed.coverage - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn append_embedded_image_ocr_markdown_adds_searchable_block() {
        let mut normalized = crate::kb::parser::NormalizedDocument {
            markdown: "Document text".to_string(),
            ..Default::default()
        };

        append_embedded_image_ocr_markdown(
            &mut normalized,
            "embedded://spreadsheet/media/image1.jpg",
            "OCR picture text",
        );

        assert!(normalized
            .markdown
            .contains(EMBEDDED_IMAGE_OCR_SECTION_TITLE));
        assert!(normalized
            .markdown
            .contains("embedded://spreadsheet/media/image1.jpg"));
        assert!(normalized.markdown.contains("OCR picture text"));
        assert_eq!(normalized.blocks.len(), 1);
        assert_eq!(normalized.blocks[0].block_type, "image_ocr");
    }

    #[test]
    fn test_embedding_to_blob_roundtrip() {
        let original: Vec<f32> = vec![0.1, -0.2, 0.3, 0.0, 1.0];
        let blob = embedding_to_blob(&original);

        assert_eq!(blob.len(), original.len() * 4);

        let decoded: Vec<f32> = blob
            .chunks_exact(4)
            .filter_map(|chunk| {
                let bytes: [u8; 4] = chunk.try_into().ok()?;
                Some(f32::from_le_bytes(bytes))
            })
            .collect();

        assert_eq!(decoded.len(), original.len());
        for (a, b) in original.iter().zip(decoded.iter()) {
            assert!(a - b < 1e-6);
        }
    }

    #[test]
    fn test_embedding_to_blob_empty() {
        let empty: Vec<f32> = vec![];
        let blob = embedding_to_blob(&empty);
        assert!(blob.is_empty());
    }

    #[test]
    fn test_collect_supported_files() {
        let dir = std::env::temp_dir();
        let sub = dir.join("gbrain_test_collect");
        let _ = std::fs::create_dir_all(&sub);
        let _ = std::fs::write(sub.join("test.md"), "hello");
        let _ = std::fs::write(sub.join("test.exe"), "binary");

        let mut files = Vec::new();
        collect_supported_files(&sub, &["md"], &mut files);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("test.md"));

        let _ = std::fs::remove_dir_all(&sub);
    }

    #[test]
    fn test_locate_chunk_overlap_boundary() {
        // P2 修复：overlap_chars == char_cursor 时应允许回退到 0
        // full_text="abcdef"、chunks=["a","abcdef"]、max_overlap=1
        // 第一块 "a" 匹配后 char_cursor=1，第二块与第一块 overlap=1（'a'）
        // overlap_chars(1) == char_cursor(1)，应回退到 0，从 0 开始 find("abcdef")
        let full_text = "abcdef".to_string();
        let chunks = vec!["a".to_string(), "abcdef".to_string()];
        let offsets = locate_chunk_char_offsets(&full_text, &chunks, 1);
        assert_eq!(offsets[0], (0, 1)); // "a" at 0..1
        assert_eq!(offsets[1], (0, 6)); // "abcdef" at 0..6（回退到 0 后精确匹配）
    }

    #[test]
    fn test_locate_chunk_overlap_normal() {
        // 正常 overlap：full_text="abcdef"、chunks=["abcd","cdef"]、max_overlap=2
        let full_text = "abcdef".to_string();
        let chunks = vec!["abcd".to_string(), "cdef".to_string()];
        let offsets = locate_chunk_char_offsets(&full_text, &chunks, 2);
        assert_eq!(offsets[0], (0, 4)); // "abcd" at 0..4
        assert_eq!(offsets[1], (2, 6)); // "cdef" at 2..6（overlap=2 回退到 2）
    }

    #[test]
    fn test_locate_chunk_no_overlap() {
        // max_overlap=0 不回退
        let full_text = "abcdef".to_string();
        let chunks = vec!["abcd".to_string(), "ef".to_string()];
        let offsets = locate_chunk_char_offsets(&full_text, &chunks, 0);
        assert_eq!(offsets[0], (0, 4)); // "abcd" at 0..4
        assert_eq!(offsets[1], (4, 6)); // "ef" at 4..6（不回退）
    }
}
