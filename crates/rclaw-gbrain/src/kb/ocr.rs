//! OCR 模块 — 状态管理、OCR 结果回写与页级/块级持久化
//!
//! Phase 1~4: 支持 GLM-OCR 版面识别、页级状态、块级融合、异步 job。

use crate::error::{GBrainError, Result};

/// M-8 修复：OCR 图片大小上限（10 MB），pipeline.rs 和 worker.rs 共用此常量。
pub const MAX_OCR_IMAGE_BYTES: usize = 10 * 1024 * 1024;
use crate::kb::context;
use crate::kb::engine::KbEngine;
use crate::kb::pipeline::{serialize_node_metadata_ex, ChunkMeta};
use crate::kb::types::*;
use crate::nlp::chinese;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// 检查 OCR 写入的 run guard：比较文档当前的 processing_run_id 与期望值。
/// 不匹配时返回 Err，调用方可据此跳过写入，避免 stale job 污染新 run 的数据。
pub fn check_ocr_run_guard(
    conn: &Connection,
    document_id: i64,
    expected_run_id: &str,
) -> Result<()> {
    // 使用 ? 传播 SQL 错误，而非 unwrap_or_default() 掩盖数据库异常。
    // 若文档不存在（QueryReturnedNoRows）或 SQL 执行失败，直接返回错误。
    let current_run: String = conn
        .query_row(
            "SELECT processing_run_id FROM kb_documents WHERE id = ?1",
            rusqlite::params![document_id],
            |row| row.get(0),
        )
        .map_err(|e| {
            GBrainError::Database(format!(
                "查询文档 {} processing_run_id 失败: {}",
                document_id, e
            ))
        })?;
    if current_run != expected_run_id {
        return Err(GBrainError::InvalidInput(format!(
            "OCR run guard 失败: 文档 {} 当前 run={}, 期望 run={}",
            document_id, current_run, expected_run_id
        )));
    }
    Ok(())
}

/// OCR 文档级处理状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrStatus {
    /// OCR 尚未进入判定阶段（例如文档仍在 KB worker 队列中）。
    NotEvaluated,
    /// 不需要 OCR（纯文本 PDF）
    NotNeeded,
    /// 需要 OCR，但 OCR 或外部 OCR 被显式关闭，或等待排队
    Needed,
    /// OCR job 已入队
    Queued,
    /// OCR 正在执行
    Processing,
    /// OCR 完成并已进入 KB 索引
    Done,
    /// 部分页成功，部分页失败
    Partial,
    /// OCR 完全失败
    Failed,
}

impl OcrStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotEvaluated => "not_evaluated",
            Self::NotNeeded => "not_needed",
            Self::Needed => "needed",
            Self::Queued => "queued",
            Self::Processing => "processing",
            Self::Done => "done",
            Self::Partial => "partial",
            Self::Failed => "failed",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "not_evaluated" => Self::NotEvaluated,
            "not_needed" => Self::NotNeeded,
            "needed" => Self::Needed,
            "queued" => Self::Queued,
            "processing" => Self::Processing,
            "done" => Self::Done,
            "partial" => Self::Partial,
            "failed" => Self::Failed,
            _ => {
                // M49: 未知 OCR 状态字符串回退到 NotNeeded，记录警告便于排查
                tracing::warn!(input = %s, "未知 OcrStatus 值，回退为 NotNeeded");
                Self::NotNeeded
            }
        }
    }
}

/// OCR 是否可通过配置启用（仅取决于全局开关，外部 OCR 始终允许）
pub fn is_ocr_enabled(global_ocr_enabled: bool) -> bool {
    global_ocr_enabled
}

/// 判断是否需要 OCR（基于文本密度）
pub fn needs_ocr(text_density: f64, threshold: f64) -> bool {
    text_density < threshold
}

// ---------------------------------------------------------------------------
// Phase 3: 页级/块级 OCR 结果持久化
// ---------------------------------------------------------------------------

/// 将 OCR 页级结果写入 kb_document_ocr_pages 表
/// `run_id` 用于 run 隔离：UNIQUE 约束包含 (document_id, page_number, processing_run_id)，
/// 不同 run 的页级结果互不干扰。
pub fn persist_ocr_page_results(
    conn: &Connection,
    document_id: i64,
    run_id: &str,
    page_results: &[crate::kb::ocr_provider::OcrPageResult],
    sensitive_value: Option<&str>,
) -> Result<()> {
    // C5 fix: 用 SAVEPOINT 包裹所有页面的写入，确保原子性。
    // SAVEPOINT 兼容外层已有事务的场景（不会产生 "cannot start a transaction within a transaction" 错误）。
    // 若中途失败（如第 5/10 页），所有页面回滚，避免部分状态不一致。
    conn.execute("SAVEPOINT sp_persist_ocr_pages", [])?;
    let result =
        persist_ocr_page_results_inner(conn, document_id, run_id, page_results, sensitive_value);
    if result.is_ok() {
        // 成功路径：RELEASE 失败意味着数据可能未真正提交，必须传播错误
        conn.execute("RELEASE sp_persist_ocr_pages", [])
            .map_err(|e| {
                GBrainError::Database(format!(
                    "SAVEPOINT sp_persist_ocr_pages RELEASE 失败: {}",
                    e
                ))
            })?;
    } else {
        if let Err(e) = conn.execute("ROLLBACK TO sp_persist_ocr_pages", []) {
            tracing::warn!(error = %e, "SAVEPOINT sp_persist_ocr_pages ROLLBACK 失败");
        }
        if let Err(e) = conn.execute("RELEASE sp_persist_ocr_pages", []) {
            tracing::warn!(error = %e, "SAVEPOINT sp_persist_ocr_pages RELEASE (回滚后) 失败");
        }
    }
    result
}

fn persist_ocr_page_results_inner(
    conn: &Connection,
    document_id: i64,
    run_id: &str,
    page_results: &[crate::kb::ocr_provider::OcrPageResult],
    sensitive_value: Option<&str>,
) -> Result<()> {
    for page in page_results {
        let sanitized_page = sanitize_ocr_page_result(page, sensitive_value);
        let page = &sanitized_page;
        // 检查是否为携带错误信息的失败页（单页降级重试部分失败场景）
        let (status, error_msg) = if page.raw_response_json.get("_ocr_failed").is_some() {
            let err = page
                .raw_response_json
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("OCR 失败")
                .to_string();
            // 脱敏错误信息
            (
                "failed",
                Some(sanitize_error_text_with_secret(&err, sensitive_value)),
            )
        } else if page.text.trim().is_empty() && page.markdown.trim().is_empty() {
            ("empty_ocr", None)
        } else {
            ("done", None)
        };

        let layout_json = serde_json::to_string(&page.blocks).unwrap_or_default();

        // 脱敏 raw_response_json：移除敏感字段、截断超长内容
        let sanitized_raw_json =
            sanitize_json_for_storage(&page.raw_response_json, sensitive_value);
        let raw_response_for_db = serde_json::to_string(&sanitized_raw_json).unwrap_or_default();

        conn.execute(
            "INSERT OR REPLACE INTO kb_document_ocr_pages \
             (document_id, page_number, processing_run_id, status, error, provider, model, text, markdown, \
              layout_json, layout_visualization_url, raw_response_json, request_id, \
              confidence, ocr_page_width, ocr_page_height, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, datetime('now'))",
            rusqlite::params![
                document_id,
                page.page_number,
                run_id,
                status,
                error_msg.unwrap_or_default(),
                page.provider,
                page.model,
                page.text,
                page.markdown,
                layout_json,
                page.layout_visualization_url.as_deref().unwrap_or(""),
                raw_response_for_db,
                page.request_id.as_deref().unwrap_or(""),
                page.confidence,
                page.ocr_page_width,
                page.ocr_page_height,
            ],
        )?;
    }
    Ok(())
}

/// 将 OCR 版面块写入 kb_document_ocr_blocks 表
/// `run_id` 用于 run 隔离：仅删除和插入同一 run 的 blocks，不影响其他 run 的数据。
pub fn persist_ocr_blocks(
    conn: &Connection,
    document_id: i64,
    run_id: &str,
    page_results: &[crate::kb::ocr_provider::OcrPageResult],
    sensitive_value: Option<&str>,
) -> Result<()> {
    // C5 fix: 用 SAVEPOINT 包裹删除+插入，确保原子性。
    // SAVEPOINT 兼容外层已有事务的场景。
    conn.execute("SAVEPOINT sp_persist_ocr_blocks", [])?;
    let result = persist_ocr_blocks_inner(conn, document_id, run_id, page_results, sensitive_value);
    if result.is_ok() {
        // 成功路径：RELEASE 失败意味着数据可能未真正提交，必须传播错误
        conn.execute("RELEASE sp_persist_ocr_blocks", [])
            .map_err(|e| {
                GBrainError::Database(format!(
                    "SAVEPOINT sp_persist_ocr_blocks RELEASE 失败: {}",
                    e
                ))
            })?;
    } else {
        if let Err(e) = conn.execute("ROLLBACK TO sp_persist_ocr_blocks", []) {
            tracing::warn!(error = %e, "SAVEPOINT sp_persist_ocr_blocks ROLLBACK 失败");
        }
        if let Err(e) = conn.execute("RELEASE sp_persist_ocr_blocks", []) {
            tracing::warn!(error = %e, "SAVEPOINT sp_persist_ocr_blocks RELEASE (回滚后) 失败");
        }
    }
    result
}

fn persist_ocr_blocks_inner(
    conn: &Connection,
    document_id: i64,
    run_id: &str,
    page_results: &[crate::kb::ocr_provider::OcrPageResult],
    sensitive_value: Option<&str>,
) -> Result<()> {
    // 先删除每页同一 run 的旧 blocks，防止重试后残留高序号旧 block
    for page in page_results {
        conn.execute(
            "DELETE FROM kb_document_ocr_blocks WHERE document_id = ?1 AND page_number = ?2 AND processing_run_id = ?3",
            rusqlite::params![document_id, page.page_number, run_id],
        )?;
    }

    for page in page_results {
        for (block_idx, block) in page.blocks.iter().enumerate() {
            let safe_content = redact_sensitive_value(&block.content, sensitive_value);
            let bbox_json = block
                .bbox_2d
                .map(|b| serde_json::to_string(&b).unwrap_or_default())
                .unwrap_or_default();

            // 生成 plain_text：text/formula 直接用 content，table 去 HTML 标签
            let plain_text = match &block.label {
                crate::kb::ocr_provider::OcrBlockLabel::Table => strip_html_tags(&safe_content),
                crate::kb::ocr_provider::OcrBlockLabel::Image => String::new(),
                _ => safe_content.clone(),
            };

            let raw_json = serde_json::json!({
                "page_number": block.page_number,
                "index": block.index,
                "label": block.label.as_str(),
                "width": block.width,
                "height": block.height,
            });

            conn.execute(
                "INSERT OR REPLACE INTO kb_document_ocr_blocks \
                 (document_id, page_number, processing_run_id, block_index, label, bbox_json, \
                  content, plain_text, source, raw_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'glm_ocr', ?9)",
                rusqlite::params![
                    document_id,
                    block.page_number,
                    run_id,
                    block_idx as i32,
                    block.label.as_str(),
                    bbox_json,
                    safe_content,
                    plain_text,
                    raw_json.to_string(),
                ],
            )?;
        }
    }
    Ok(())
}

/// 更新页级 OCR 状态（用于标记失败/跳过页）
///
/// 使用 UPSERT 确保即使页级记录尚不存在也能写入状态，同时保留该页已有
/// OCR 正文与版面块。这样手动重跑已成功页面失败时，检索正文不会倒退丢失。
/// 防止 OCR 完全失败时无任何页级行，导致 update_document_ocr_status
/// 将 total_ocr_pages==0 误判为 NotNeeded。
///
/// `provider` / `model` 记录实际使用（或意图使用）的 OCR 提供者和模型，
/// 用于失败诊断和审计。API 错误、超时、拆分失败等场景均应传入。
#[allow(clippy::too_many_arguments)]
pub fn update_ocr_page_status(
    conn: &Connection,
    document_id: i64,
    page_number: i32,
    status: &str,
    error: &str,
    provider: &str,
    model: &str,
    run_id: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO kb_document_ocr_pages \
         (document_id, page_number, processing_run_id, status, error, provider, model, text, markdown, \
          layout_json, layout_visualization_url, raw_response_json, request_id, \
          confidence, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '', '', '[]', '', '{}', '', NULL, datetime('now')) \
         ON CONFLICT(document_id, page_number, processing_run_id) DO UPDATE SET \
         status = excluded.status, error = excluded.error, provider = excluded.provider, \
         model = excluded.model, updated_at = datetime('now')",
        rusqlite::params![document_id, page_number, run_id, status, error, provider, model],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn update_ocr_pages_status(
    conn: &Connection,
    document_id: i64,
    pages: &[i32],
    status: &str,
    error: &str,
    provider: &str,
    model: &str,
    run_id: &str,
) -> Result<()> {
    for &page_number in pages {
        update_ocr_page_status(
            conn,
            document_id,
            page_number,
            status,
            error,
            provider,
            model,
            run_id,
        )?;
    }
    Ok(())
}

/// HTML 标签匹配正则（全局编译一次，避免热路径重复编译）
static HTML_TAG_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"<[^>]+>").unwrap());

/// 简单去掉 HTML 标签
fn strip_html_tags(html: &str) -> String {
    HTML_TAG_RE.replace_all(html, "").trim().to_string()
}

/// 只读聚合 OCR 页状态，返回 (OcrStatus, coverage)，不写入 kb_documents。
///
/// 用于需要在节点持久化完成前判断终态的场景（如同步内联 OCR 的文档错误清空判断），
/// 避免在 split/节点写入失败时残留错误的 ocr_status。
pub fn compute_ocr_status(
    conn: &Connection,
    document_id: i64,
    total_pages: i32,
    run_id: Option<&str>,
) -> Result<(OcrStatus, f64)> {
    // m-16 修复：用两个分支直接查询，避免 Vec<Box<dyn ToSql>> + rid.to_string() 的堆分配
    let mut status_counts: std::collections::HashMap<String, i32> =
        std::collections::HashMap::new();
    let mut total_ocr_pages: i32 = 0;

    {
        let mut stmt = if run_id.is_some() {
            conn.prepare(
                "SELECT status, COUNT(*) as cnt FROM kb_document_ocr_pages \
                 WHERE document_id = ?1 AND processing_run_id = ?2 GROUP BY status",
            )?
        } else {
            conn.prepare(
                "SELECT status, COUNT(*) as cnt FROM kb_document_ocr_pages \
                 WHERE document_id = ?1 GROUP BY status",
            )?
        };

        let mut rows = if let Some(rid) = run_id {
            stmt.query(rusqlite::params![document_id, rid])?
        } else {
            stmt.query(rusqlite::params![document_id])?
        };

        while let Some(row) = rows.next()? {
            let status: String = row.get(0)?;
            let cnt: i32 = row.get(1)?;
            total_ocr_pages += cnt;
            status_counts.insert(status, cnt);
        }
    }

    let done_count = status_counts.get("done").copied().unwrap_or(0);
    let failed_count = status_counts.get("failed").copied().unwrap_or(0);
    let empty_ocr_count = status_counts.get("empty_ocr").copied().unwrap_or(0);
    let skipped_count = status_counts.get("skipped").copied().unwrap_or(0);
    let needed_count = status_counts.get("needed").copied().unwrap_or(0);

    let success_count = done_count;
    let terminal_count = done_count + failed_count + empty_ocr_count + skipped_count + needed_count;

    let (status, coverage) = if total_ocr_pages == 0 {
        (OcrStatus::NotNeeded, 0.0)
    } else if terminal_count < total_ocr_pages {
        let cov = if total_pages > 0 {
            success_count as f64 / total_pages as f64
        } else {
            0.0
        };
        (OcrStatus::Processing, cov)
    } else if needed_count == total_ocr_pages {
        (OcrStatus::Needed, 0.0)
    } else if failed_count == total_ocr_pages {
        (OcrStatus::Failed, 0.0)
    } else if failed_count > 0 || empty_ocr_count > 0 || skipped_count > 0 || needed_count > 0 {
        let cov = if total_pages > 0 {
            success_count as f64 / total_pages as f64
        } else {
            0.0
        };
        (OcrStatus::Partial, cov)
    } else {
        let cov = if total_pages > 0 {
            success_count as f64 / total_pages as f64
        } else {
            0.0
        };
        (OcrStatus::Done, cov)
    };

    Ok((status, coverage))
}

/// 计算并更新文档级 OCR 状态和文本覆盖率。
/// `run_id` 用于双重保障：(1) 聚合查询按 run 过滤，避免统计旧 run 的页记录；
/// (2) 文档状态更新使用 run guard，防止 stale job 覆盖新 run 的状态。
pub fn update_document_ocr_status(
    conn: &Connection,
    document_id: i64,
    total_pages: i32,
    run_id: Option<&str>,
) -> Result<OcrStatus> {
    let (status, coverage) = compute_ocr_status(conn, document_id, total_pages, run_id)?;
    let kb = KbEngine::new(conn);
    kb.update_document_ocr_with_run_guard(document_id, status.as_str(), coverage, run_id)?;
    Ok(status)
}

/// 内部版本：在调用者已有事务的情况下更新文档 OCR 状态，避免嵌套 BEGIN。
/// 供 writeback_ocr_results 等外层事务内调用。
pub fn update_document_ocr_status_inner(
    conn: &Connection,
    document_id: i64,
    total_pages: i32,
    run_id: Option<&str>,
) -> Result<OcrStatus> {
    let (status, coverage) = compute_ocr_status(conn, document_id, total_pages, run_id)?;
    let kb = KbEngine::new(conn);
    kb.update_document_ocr_with_run_guard_inner(document_id, status.as_str(), coverage, run_id)?;
    Ok(status)
}

/// 记录一次 GLM-OCR 外部调用审计。审计写入失败只记录 warn，不影响 OCR 主流程。
#[allow(clippy::too_many_arguments)]
pub fn log_ocr_external_model_call(
    conn: &Connection,
    library_id: i64,
    document_id: i64,
    provider: &str,
    model: &str,
    latency_ms: i32,
    success: bool,
    error_message: &str,
    page_results: &[crate::kb::ocr_provider::OcrPageResult],
    sensitive_value: Option<&str>,
) {
    let (input_tokens, output_tokens) = ocr_usage_tokens(page_results);
    let safe_error = sanitize_error_text_with_secret(error_message, sensitive_value);
    if let Err(e) = crate::kb::privacy::log_external_model_call(
        conn,
        Some(library_id),
        Some(document_id),
        "ocr",
        provider,
        model,
        input_tokens,
        output_tokens,
        latency_ms,
        0.0,
        success,
        &safe_error,
    ) {
        tracing::warn!(
            document_id,
            provider,
            model,
            error = %e,
            "OCR 外部模型调用审计写入失败"
        );
    }
}

fn ocr_usage_tokens(page_results: &[crate::kb::ocr_provider::OcrPageResult]) -> (i32, i32) {
    let mut seen = std::collections::HashSet::new();
    let mut input_tokens = 0i32;
    let mut output_tokens = 0i32;

    for result in page_results {
        let key = result
            .request_id
            .clone()
            .or_else(|| {
                result
                    .raw_response_json
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| result.raw_response_json.to_string());
        if !seen.insert(key) {
            continue;
        }

        if let Some(usage) = result.raw_response_json.get("usage") {
            let prompt_tokens = usage
                .get("prompt_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                .clamp(0, i32::MAX as i64) as i32;
            let completion_tokens = usage
                .get("completion_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                .clamp(0, i32::MAX as i64) as i32;
            input_tokens = input_tokens.saturating_add(prompt_tokens);
            output_tokens = output_tokens.saturating_add(completion_tokens);
        }
    }

    (input_tokens, output_tokens)
}

// ---------------------------------------------------------------------------
// P2-019: OCR 结果回写
// ---------------------------------------------------------------------------

/// OCR 回写用的简化页级结果（仅含页码和文本）
///
/// 与 ocr_provider::OcrPageResult（含 markdown/blocks/provider 等完整字段）不同，
/// 此结构体仅用于 writeback 路径，聚焦于文本回写到 KB 索引。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrWritebackPage {
    /// 页码（1-based）
    pub page_number: i32,
    /// 该页 OCR 识别出的文本
    pub text: String,
}

/// P2-019: 将 OCR 结果转换为标准 ParsedBlock 列表。
///
/// 每个 `OcrWritebackPage` 生成一个 `ParsedBlock`，block_type 为 "ocr_text"，
/// 携带 page_number 和基于累计字符偏移的 source_start/source_end。
/// P2-019: 将 OCR 结果转换为标准 ParsedBlock 列表。
///
/// 每个 `OcrWritebackPage` 生成一个 `ParsedBlock`，block_type 为 "ocr_text"，
/// 携带 page_number 和基于累计字符偏移的 source_start/source_end。
/// offset 计算与 full_text 的 `[PAGE:N]\n{text}` + `\n\n` 拼接格式保持一致。
pub fn ocr_to_parsed_blocks(ocr_pages: &[OcrWritebackPage]) -> Vec<ParsedBlock> {
    let mut blocks = Vec::with_capacity(ocr_pages.len());
    let mut offset = 0i32;

    for (i, page) in ocr_pages.iter().enumerate() {
        // full_text 格式: `[PAGE:N]\n{text}`，页间以 `\n\n` 分隔
        let marker = format!("[PAGE:{}]\n", page.page_number);
        let entry_chars = marker.chars().count() as i32 + page.text.chars().count() as i32;
        blocks.push(ParsedBlock {
            text: page.text.clone(),
            title_path: String::new(),
            page_number: Some(page.page_number),
            source_start: Some(offset),
            source_end: Some(offset + entry_chars),
            block_type: "ocr_text".to_string(),
            metadata: String::new(),
        });
        offset += entry_chars;
        // 非最后一页后面有 `\n\n` 分隔符（2 个字符）
        if i < ocr_pages.len() - 1 {
            offset += 2;
        }
    }

    blocks
}

/// P2-019: OCR 结果回写主函数。
///
/// 接收 OCR 识别结果（或合并后的文本层+OCR 结果），将其转换为 ParsedBlock，
/// 再分割为 RaptorNode，持久化到数据库，并更新文档的 OCR 状态和文本覆盖率。
///
/// 流程：
/// 1. 将 OCR 结果转为 ParsedBlock 列表
/// 2. 拼接全文，按配置分割为 chunks
/// 3. 为每个 chunk 构建 RaptorNode（含 page_number / source offsets）
/// 4. 持久化节点
/// 5. 更新文档 ocr_status = "done" 和 ocr_text_coverage
///
/// 注意：此函数不执行 OCR 识别本身，仅处理识别后的文本回写。
/// 调用方应在 OCR 完成后调用此函数。
///
/// `run_id` 用于防止过期 OCR 覆盖新上传产生的节点，传 None 则不做守卫。
/// `embedder` 用于语义分割器计算嵌入相似度；传 None 时使用递归分割。
/// 在同一事务内完成空内容回写的 OCR 状态与文档统计更新。
///
/// 正常含正文路径已将节点、统计与 OCR 状态放在一个事务内；
/// 空内容早退路径也须使用同一事务，避免 OCR 终态已提交而文档统计失败
/// 或 run_id 变化导致的不一致。
fn finalize_empty_writeback_in_tx(
    conn: &Connection,
    doc_id: i64,
    total_page_count: i32,
    run_id: Option<&str>,
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let result = (|| -> Result<()> {
        update_document_ocr_status_inner(conn, doc_id, total_page_count, run_id)?;
        let kb = KbEngine::new(conn);
        kb.update_document_stats_with_run_guard_inner(
            doc_id,
            0,
            0,
            Some(crate::kb::types::STATUS_COMPLETED),
            run_id,
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            tx.commit()?;
            Ok(())
        }
        Err(e) => {
            let _ = tx.rollback();
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn writeback_ocr_results(
    conn: &Connection,
    doc_id: i64,
    lib_id: i64,
    ocr_pages: &[OcrWritebackPage],
    chunk_size: usize,
    chunk_overlap: usize,
    doc_title: &str,
    total_page_count: i32,
    run_id: Option<&str>,
    embedder: Option<std::sync::Arc<crate::embedding::Embedder>>,
    title_weight: f32,
) -> Result<WritebackResult> {
    if ocr_pages.is_empty() {
        return Ok(WritebackResult {
            blocks_created: 0,
            nodes_created: 0,
            ocr_text_coverage: 0.0,
        });
    }

    // Empty OCR/native pages keep their page status but must not create
    // searchable marker-only content.
    let searchable_pages: Vec<OcrWritebackPage> = ocr_pages
        .iter()
        .filter(|page| !page.text.trim().is_empty())
        .cloned()
        .collect();
    if searchable_pages.is_empty() {
        // 所有页面正文均为空：在同一事务内完成 OCR 状态与文档统计更新，
        // 避免先提交 OCR 终态后文档统计失败导致不一致
        finalize_empty_writeback_in_tx(conn, doc_id, total_page_count, run_id)?;
        return Ok(WritebackResult {
            blocks_created: 0,
            nodes_created: 0,
            ocr_text_coverage: 0.0,
        });
    }

    // 1. 转换有正文的页面为 ParsedBlock
    let blocks = ocr_to_parsed_blocks(&searchable_pages);

    // 2. 拼接全文并分割（保留 [PAGE:N] 标记，确保 splitter 能识别页面边界）
    let full_text: String = blocks
        .iter()
        .map(|b| {
            if let Some(pn) = b.page_number {
                format!("[PAGE:{}]\n{}", pn, b.text)
            } else {
                b.text.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if full_text.trim().is_empty() {
        // 全文为空：在同一事务内完成 OCR 状态与文档统计更新，
        // 避免先提交 OCR 终态后文档统计失败导致不一致
        finalize_empty_writeback_in_tx(conn, doc_id, total_page_count, run_id)?;
        return Ok(WritebackResult {
            blocks_created: blocks.len(),
            nodes_created: 0,
            ocr_text_coverage: 0.0,
        });
    }

    // 自适应分割：有 embedder 时大块可语义细分，否则递归细分
    let splitter_config = crate::kb::splitter::SplitterConfig {
        file_path: String::new(), // OCR 回写无文件路径，走通用分割
        chunk_size,
        chunk_overlap,
    };
    // FIX10-R1: 在 embedder 被 move 之前记录是否有 embedder，用于后续 overlap 计算
    let has_embedder = embedder.is_some();
    let splitter = crate::kb::splitter::create_async_splitter(&splitter_config, embedder)?;
    let rt = tokio::runtime::Handle::try_current();
    let chunks = match rt {
        Ok(handle) => handle.block_on(splitter.split_async(&full_text)),
        Err(_) => {
            // 无 tokio runtime（如单元测试）：直接用 block_on 创建临时 runtime
            tokio::runtime::Runtime::new()
                .map_err(|e| GBrainError::Config(format!("创建 tokio runtime 失败: {}", e)))?
                .block_on(splitter.split_async(&full_text))
        }
    }
    .map_err(|e| GBrainError::InvalidInput(format!("OCR 回写分割失败: {}", e)))?;

    // FIX9-05: 为每个 chunk 通过 span overlap 匹配对应的 block 元数据，
    // 而非按 chunk 下标直接对应 page index。
    // 当一个 OCR 页被切成多个 chunk，或多个短页合并成一个 chunk 时，
    // 按 chunk 在全文中的真实位置与 block 的 source span 重叠度匹配。

    // FIX10-R1: 使用统一的 helper 定位 chunk 字符偏移，max_overlap 按 splitter 类型计算
    // 传 has_embedder 以匹配 create_async_splitter 的实际 splitter 选择：
    // 有 embedder 时语义 splitter 使用 chunk_overlap，否则 Recursive splitter cap 到 chunk_size/2
    let max_overlap = crate::kb::pipeline::splitter_max_overlap(&splitter_config, has_embedder);
    let chunk_spans: Vec<(usize, usize)> =
        crate::kb::pipeline::locate_chunk_char_offsets(&full_text, &chunks, max_overlap);

    #[allow(clippy::type_complexity)]
    let block_meta_vec: Vec<(String, Option<i32>, Option<i32>, Option<i32>)> = blocks
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

    // 4. 构建 RaptorNode 列表
    let nodes: Vec<RaptorNode> = chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            // FIX9-05: 用 span overlap 匹配最相关的 block，而非 block_meta_vec.get(i)
            let (chunk_start, chunk_end) = chunk_spans[i];

            // 收集所有与 chunk 有重叠的 block，提取跨页信息
            let mut overlapping_pages: Vec<i32> = Vec::new();
            let mut best_overlap = 0usize;
            let mut best_title = String::new();
            let mut best_page_num: Option<i32> = None;

            for meta in &block_meta_vec {
                let (title_path, page_num, src_start, src_end) = meta;
                let bs = src_start.unwrap_or(0) as usize;
                let be = src_end.unwrap_or(i32::MAX / 2) as usize;
                let overlap_start = chunk_start.max(bs);
                let overlap_end = chunk_end.min(be);
                let overlap = overlap_end.saturating_sub(overlap_start);
                if overlap > 0 {
                    if let Some(pn) = page_num {
                        if !overlapping_pages.contains(pn) {
                            overlapping_pages.push(*pn);
                        }
                    }
                    if overlap > best_overlap {
                        best_overlap = overlap;
                        best_title = title_path.clone();
                        best_page_num = *page_num;
                    }
                }
            }

            overlapping_pages.sort();
            // 主页码取重叠最大的 block（best_page_num），与同步 pipeline 的最大重叠策略一致；
            // 排序页号列表仅用于跨页来源信息的 metadata
            let page_num = best_page_num;
            let title_path = best_title;
            // P2 修复：node 的 source_start/source_end 必须使用 chunk 在源文件中的
            // 字符偏移（chunk_spans[i]），而不是重叠最佳 block 的 src_start/src_end。
            // 原因：持久化阶段 pipeline.rs 将 node.source_start 当作 chunk 基址传给
            // passage rebuild；若沿用 block 起始，会导致同一 OCR 页被切成多个 chunk 时，
            // 后续 chunk 的 passage 绝对偏移从页首重复叠加，focused/evidence 命中位置错位。
            // block 信息只继续用于 page_number / title_path 这种"块级关联"语义。
            let src_start = Some(chunk_start as i32);
            let src_end = Some(chunk_end as i32);

            // 跨页 chunk：在 metadata 中记录所有来源页
            // P2: 使用 ChunkMeta 写入自适应分块元数据
            let ocr_chunk_meta = ChunkMeta {
                strategy: "adaptive",
                segment_kind: "pdf_page",
                semantic_refined: false,
                modality: "pdf_page",
            };
            let mut node_metadata = serialize_node_metadata_ex(
                &[], // OCR writeback 无 media_refs 传入
                None,
                false,
                Some(ocr_chunk_meta),
            );
            // 跨页 chunk：合并 page_numbers 到已有的 metadata JSON 中
            if overlapping_pages.len() > 1 {
                if let Ok(mut meta_obj) = serde_json::from_str::<
                    serde_json::Map<String, serde_json::Value>,
                >(&node_metadata)
                {
                    meta_obj.insert(
                        "page_numbers".to_string(),
                        serde_json::json!(overlapping_pages),
                    );
                    node_metadata = serde_json::Value::Object(meta_obj).to_string();
                } else {
                    // fallback：metadata 解析失败时直接写入 page_numbers
                    node_metadata =
                        serde_json::json!({"page_numbers": overlapping_pages}).to_string();
                }
            }

            let embedding_text = context::build_embedding_text(
                doc_title,
                &title_path,
                page_num,
                chunk,
                title_weight,
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
                node_metadata,
                embedding_text,
            }
        })
        .collect();

    let nodes_created = nodes.len();

    // 5. 计算 ocr_text_coverage
    let ocr_pages_with_text = ocr_pages
        .iter()
        .filter(|p| !p.text.trim().is_empty())
        .count() as i32;
    let coverage = if total_page_count > 0 {
        ocr_pages_with_text as f64 / total_page_count as f64
    } else {
        0.0
    };

    // 6. 原子提交：在同一事务中持久化 nodes、更新统计信息、更新 OCR 状态
    // 确保不会出现 nodes 未写入但 OCR 状态已标记为 done 的情况
    let kb = KbEngine::new(conn);
    let word_total: i32 = if chinese::has_chinese(&full_text) {
        let tokens = chinese::tokenize_content(&full_text);
        tokens.split_whitespace().count() as i32
    } else {
        full_text.split_whitespace().count() as i32
    };
    // 单一事务：持久化 nodes → 更新统计 → 更新 OCR 状态
    // 任一写入失败时回滚，禁止先暴露 done
    let tx = conn.unchecked_transaction()?;
    let result = (|| -> Result<()> {
        // P1-1: active version 生命周期（OCR 完成路径）
        let content_hash = crate::kb::pipeline::query_document_content_hash(conn, doc_id)?;
        let version_id =
            crate::kb::pipeline::begin_document_version(conn, doc_id, run_id, &content_hash)?;
        crate::kb::pipeline::persist_nodes_for_version_inner(
            conn, doc_id, version_id, lib_id, &nodes, run_id,
        )?;
        crate::kb::pipeline::activate_document_version(conn, doc_id, run_id, version_id)?;
        // 更新文档统计（词数/分块数），embedding 标记为 pending
        kb.update_document_stats_with_run_guard_inner(
            doc_id,
            word_total,
            nodes_created as i32,
            Some(crate::kb::types::STATUS_PENDING),
            run_id,
        )?;
        // 最后更新 OCR 状态（仅在 nodes 和统计写入成功后）
        // 使用 _inner 版本避免嵌套事务（当前已在 unchecked_transaction 内）
        update_document_ocr_status_inner(conn, doc_id, total_page_count, run_id)?;
        Ok(())
    })();
    match result {
        Ok(()) => tx.commit()?,
        Err(e) => {
            let _ = tx.rollback();
            return Err(e);
        }
    }

    Ok(WritebackResult {
        blocks_created: blocks.len(),
        nodes_created,
        ocr_text_coverage: coverage,
    })
}

/// OCR 回写结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WritebackResult {
    /// 创建的 ParsedBlock 数量
    pub blocks_created: usize,
    /// 创建的 RaptorNode 数量
    pub nodes_created: usize,
    /// OCR 文本覆盖率（0.0~1.0）
    pub ocr_text_coverage: f64,
}

// ---------------------------------------------------------------------------
// OCR 响应脱敏工具
// ---------------------------------------------------------------------------

/// 需要脱敏的鉴权字段名（精确小写匹配）
/// 使用精确匹配而非子串匹配，避免将 prompt_tokens、completion_tokens
/// 等合法的用量统计字段误脱敏为 "***REDACTED***"。
const SENSITIVE_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "access_token",
    "refresh_token",
    "token",
    "authorization",
    "bearer_token",
    "secret",
    "api_secret",
    "password",
    "credential",
];

/// 脱敏后的替换文本
const MASKED_VALUE: &str = "***REDACTED***";

/// 错误文本最大长度（超出截断）
const MAX_ERROR_TEXT_LEN: usize = 500;

fn redact_sensitive_value(text: &str, sensitive_value: Option<&str>) -> String {
    if let Some(secret) = sensitive_value.filter(|value| !value.is_empty()) {
        text.replace(secret, MASKED_VALUE)
    } else {
        text.to_string()
    }
}

fn sanitize_ocr_page_result(
    page: &crate::kb::ocr_provider::OcrPageResult,
    sensitive_value: Option<&str>,
) -> crate::kb::ocr_provider::OcrPageResult {
    let mut sanitized = page.clone();
    sanitized.text = redact_sensitive_value(&sanitized.text, sensitive_value);
    sanitized.markdown = redact_sensitive_value(&sanitized.markdown, sensitive_value);
    sanitized.layout_visualization_url = sanitized
        .layout_visualization_url
        .as_deref()
        .map(|value| sanitize_string(value, sensitive_value));
    for block in &mut sanitized.blocks {
        // 仅替换真实秘密值（API key），保留表格 HTML、公式、图片 URL 等原始版面内容。
        // 路径/URL 脱敏已限定在 raw_response_json，普通 block 不应破坏正文中的合法路径。
        block.content = redact_sensitive_value(&block.content, sensitive_value);
    }
    sanitized.raw_response_json =
        sanitize_json_for_storage(&sanitized.raw_response_json, sensitive_value);
    sanitized
}

pub fn sanitize_ocr_page_results(
    page_results: &[crate::kb::ocr_provider::OcrPageResult],
    sensitive_value: Option<&str>,
) -> Vec<crate::kb::ocr_provider::OcrPageResult> {
    page_results
        .iter()
        .map(|page| sanitize_ocr_page_result(page, sensitive_value))
        .collect()
}

/// 递归脱敏 JSON 值中的鉴权字段和敏感内容
///
/// 处理规则：
/// - 字段名与鉴权关键字精确匹配时，值替换为 `***REDACTED***`
/// - Windows 绝对路径和 file:// URL 进行掩码
/// - OCR 临时目录路径进行掩码
/// - 环回地址（127.0.0.1/localhost/[::1]）和私网地址 URL 进行掩码
/// - UNC 路径和 POSIX 绝对路径进行掩码
/// - 不再截断超长字符串，确保 raw_response_json 可用于回放修复
pub fn sanitize_json_for_storage(
    value: &serde_json::Value,
    sensitive_value: Option<&str>,
) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sanitized: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| {
                    let key_lower = k.to_lowercase();
                    if SENSITIVE_KEYS.iter().any(|sk| key_lower == *sk) {
                        (
                            k.clone(),
                            serde_json::Value::String(MASKED_VALUE.to_string()),
                        )
                    } else {
                        (k.clone(), sanitize_json_for_storage(v, sensitive_value))
                    }
                })
                .collect();
            serde_json::Value::Object(sanitized)
        }
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|v| sanitize_json_for_storage(v, sensitive_value))
                .collect(),
        ),
        serde_json::Value::String(s) => {
            let sanitized = sanitize_string(s, sensitive_value);
            serde_json::Value::String(sanitized)
        }
        other => other.clone(),
    }
}

/// 脱敏字符串中的敏感内容
fn sanitize_string(s: &str, sensitive_value: Option<&str>) -> String {
    // 匹配 Windows 绝对路径，如 C:\Users\... 或 D:\path\...
    static WIN_PATH_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new("(?i)[A-Z]:[\\\\/][^\\s'\"]+").unwrap());
    static FILE_URL_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new("(?i)file:/+[^\\s'\"]+").unwrap());
    static TEMP_DIR_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"gbrain_ocr_[^\\/\s'"?]+"#).unwrap());
    // 匹配 Bearer token 和 Authorization header
    static BEARER_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new("(?i)(Bearer\\s+)\\S+").unwrap());

    let mut result = s.to_string();
    if let Some(secret) = sensitive_value.filter(|value| !value.is_empty()) {
        result = result.replace(secret, MASKED_VALUE);
    }
    // 掩码 Bearer token
    result = BEARER_RE
        .replace_all(&result, "${1}***REDACTED***")
        .to_string();
    // 掩码类似 API key 的长十六进制/Base64 字符串（常见于 GLM-OCR 等服务的响应正文）
    static API_KEY_LIKE_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"(?i)(api[_-]?key|token|secret|authorization)["'\s:=]+["']?([a-zA-Z0-9_\-]{20,})"#,
        )
        .unwrap()
    });
    result = API_KEY_LIKE_RE
        .replace_all(&result, "${1}***REDACTED***")
        .to_string();
    // 掩码 Windows 绝对路径
    result = WIN_PATH_RE.replace_all(&result, "***PATH***").to_string();
    // 掩码 file:// URL
    result = FILE_URL_RE
        .replace_all(&result, "***FILE_URL***")
        .to_string();
    // 掩码 OCR 临时目录路径
    result = TEMP_DIR_RE
        .replace_all(&result, "***TEMP_DIR***")
        .to_string();
    // 掩码内部/私网/环回/链路本地 URL（路径可选，覆盖 http://localhost:8080 等无路径 URL）
    // 覆盖：127.0.0.0/8、[::1]、localhost、10.0.0.0/8、192.168.0.0/16、
    //        172.16.0.0/12、169.254.0.0/16（link-local）、
    //        [fe80::/10] IPv6 链路本地、[fc00::/7] IPv6 唯一本地地址
    static INTERNAL_URL_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
                r#"(?i)https?://(127\.\d{1,3}\.\d{1,3}\.\d{1,3}|\[::1\]|\[fe[89ab][0-9a-f:.]+\]|\[f[cd][0-9a-f]{2}[0-9a-f:.]+\]|localhost|10\.\d{1,3}\.\d{1,3}\.\d{1,3}|192\.168\.\d{1,3}\.\d{1,3}|172\.(1[6-9]|2\d|3[01])\.\d{1,3}\.\d{1,3}|169\.254\.\d{1,3}\.\d{1,3})(:\d+)?(/[^\s'"<>]*)?"#,
            )
            .unwrap()
    });
    result = INTERNAL_URL_RE
        .replace_all(&result, "***INTERNAL_URL***")
        .to_string();
    // 掩码 UNC 路径（\\server\share\...）
    static UNC_PATH_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"\\\\[a-zA-Z0-9_.$-]+(\\[^\s'"<>]*)+"#).unwrap()
    });
    result = UNC_PATH_RE
        .replace_all(&result, "***UNC_PATH***")
        .to_string();
    // 掩码 POSIX 绝对路径：通用跨平台识别，不限于固定目录前缀。
    // 匹配以 / 开头的多级路径（如 /data/ocr/a.pdf、/private/tmp/x、
    // /tmp/x、/home/user/file），要求至少两级目录以确保是路径而非普通文本。
    // 前缀匹配支持空白、等号、括号、冒号、引号等常见分隔符，
    // 覆盖 path=/data/ocr/a.pdf、open(/private/tmp/x) 等绕过场景。
    static POSIX_PATH_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?m)(^|[\s=(:\x60'"<>])(/([a-zA-Z0-9_.-]+/)+[a-zA-Z0-9_.-]+)"#)
            .unwrap()
    });
    result = POSIX_PATH_RE
        .replace_all(&result, "${1}***POSIX_PATH***")
        .to_string();
    // 掩码带常见扩展名的单级 POSIX 绝对路径（如 /report.pdf、/image.png）。
    // 前缀匹配与 POSIX_PATH_RE 一致，覆盖 path=/tmp/x.pdf 等绕过场景。
    static POSIX_SINGLE_FILE_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(
        || {
            regex::Regex::new(
                r#"(?m)(^|[\s=(:\x60'"<>])(/([a-zA-Z0-9_.-]+\.(?:pdf|png|jpe?g|gif|bmp|svg|webp|tiff?|json|xml|csv|txt|log|tmp|dat|bin|html?|css|js|py|rs|go|java|md|zip|tar|gz|bz2|xz|7z|docx?|xlsx?|pptx?)))"#,
            )
            .unwrap()
        },
    );
    result = POSIX_SINGLE_FILE_RE
        .replace_all(&result, "${1}***POSIX_PATH***")
        .to_string();
    result
}

/// 脱敏错误文本：截断并清理敏感内容
pub fn sanitize_error_text(error: &str) -> String {
    sanitize_error_text_with_secret(error, None)
}

/// Redact an error before logging or persistence, including a configured secret
/// if a provider has echoed it without an identifying field name.
pub fn sanitize_error_text_with_secret(error: &str, sensitive_value: Option<&str>) -> String {
    let mut sanitized = sanitize_string(error, sensitive_value);
    // 截断（确保在 UTF-8 字符边界处截断）
    if sanitized.len() > MAX_ERROR_TEXT_LEN {
        let mut end = MAX_ERROR_TEXT_LEN;
        while !sanitized.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        sanitized.truncate(end);
        sanitized.push_str("...[TRUNCATED]");
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ocr_disabled() {
        assert!(!is_ocr_enabled(false));
        assert!(is_ocr_enabled(true));
    }

    #[test]
    fn test_needs_ocr() {
        assert!(needs_ocr(0.01, 0.05));
        assert!(!needs_ocr(0.1, 0.05));
    }

    #[test]
    fn test_ocr_to_parsed_blocks_empty() {
        let blocks = ocr_to_parsed_blocks(&[]);
        assert!(blocks.is_empty());
    }

    #[test]
    fn test_ocr_to_parsed_blocks_single_page() {
        let pages = vec![OcrWritebackPage {
            page_number: 1,
            text: "Hello world".to_string(),
        }];
        let blocks = ocr_to_parsed_blocks(&pages);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "Hello world");
        assert_eq!(blocks[0].page_number, Some(1));
        assert_eq!(blocks[0].block_type, "ocr_text");
        assert_eq!(blocks[0].source_start, Some(0));
        assert_eq!(blocks[0].source_end, Some(20));
    }

    #[test]
    fn test_ocr_to_parsed_blocks_multi_page() {
        let pages = vec![
            OcrWritebackPage {
                page_number: 1,
                text: "Page one".to_string(),
            },
            OcrWritebackPage {
                page_number: 2,
                text: "Page two".to_string(),
            },
        ];
        let blocks = ocr_to_parsed_blocks(&pages);
        assert_eq!(blocks.len(), 2);
        // Spans include the page marker and the two-character separator.
        assert_eq!(blocks[0].source_start, Some(0));
        assert_eq!(blocks[1].source_start, Some(19));
        assert_eq!(blocks[1].source_end, Some(36));
    }

    #[test]
    fn test_ocr_to_parsed_blocks_chinese() {
        let pages = vec![
            OcrWritebackPage {
                page_number: 1,
                text: "第一页内容".to_string(),
            },
            OcrWritebackPage {
                page_number: 2,
                text: "第二页内容".to_string(),
            },
        ];
        let blocks = ocr_to_parsed_blocks(&pages);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].source_start, Some(0));
        assert_eq!(blocks[0].source_end, Some(14));
        assert_eq!(blocks[1].source_start, Some(16));
        assert_eq!(blocks[1].source_end, Some(30));
    }

    #[test]
    fn test_ocr_status_as_str() {
        assert_eq!(OcrStatus::NotEvaluated.as_str(), "not_evaluated");
        assert_eq!(OcrStatus::Done.as_str(), "done");
        assert_eq!(OcrStatus::Needed.as_str(), "needed");
        assert_eq!(OcrStatus::Failed.as_str(), "failed");
    }
}
