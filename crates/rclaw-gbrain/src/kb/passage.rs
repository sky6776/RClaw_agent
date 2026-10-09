//! Passage view index for robust KB retrieval.
//!
//! This module builds format-agnostic retrieval spans from KB nodes. It is a
//! fallback surface for long PDFs, OCR text, fragmented notes, and documents
//! without reliable headings.

use crate::error::Result;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

const WINDOW_CHARS: usize = 720;
const WINDOW_OVERLAP: usize = 120;
const ATOMIC_MIN_CHARS: usize = 24;
const ATOMIC_MAX_CHARS: usize = 900;
const MAX_ATOMIC_PASSAGES_PER_NODE: usize = 512;
const MAX_WINDOW_PASSAGES_PER_NODE: usize = 512;

#[derive(Debug, Clone)]
struct PassageDraft {
    view_type: &'static str,
    passage_order: i32,
    source_start: i32,
    source_end: i32,
    content: String,
    quality_score: f64,
}

/// Normalize text for passage search while keeping content human-readable.
pub fn clean_text_for_search(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = false;
    let mut last_was_newline = false;

    for ch in text.chars() {
        let mapped = match ch {
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' => '\n',
            '\r' => '\n',
            _ => ch,
        };

        if mapped == '\n' {
            if !last_was_newline {
                out.push('\n');
            }
            last_was_newline = true;
            last_was_space = false;
        } else if mapped.is_whitespace() {
            if !last_was_space && !last_was_newline {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(mapped);
            last_was_space = false;
            last_was_newline = false;
        }
    }

    out.trim().to_string()
}

/// Rebuild all passage views for one KB node.
///
/// 修复：`node_source_start` 是该 node 在源文件中的字符偏移基址。
/// 之前 passage 的 `source_start/source_end` 是相对 node 内容的相对偏移，
/// 但 query/focused 输出把这些值当作源文件绝对偏移再加 snippet 偏移，
/// 导致多 chunk 文档里第二个及后续 node 的 passage 定位到错误位置。
/// 此处统一在写入前叠加 base offset，让 passage 的偏移与源文件对齐。
///
/// L4: 当前实现总是 DELETE + 重建全部 passage，无法增量更新。
/// 未来改进方向：比较新旧 passage draft 的 content hash，仅更新变化的行。
pub fn rebuild_passages_for_node(
    conn: &Connection,
    node_id: i64,
    library_id: i64,
    document_id: i64,
    content: &str,
    node_source_start: i32,
) -> Result<usize> {
    let drafts = build_passage_drafts(content);

    // L4 修复：查询现有 passage 数量和签名，若完全匹配则跳过重建
    let existing_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM kb_passage_spans WHERE node_id = ?1",
        params![node_id],
        |row| row.get(0),
    )?;

    if existing_count == drafts.len() as i64 && !drafts.is_empty() {
        // 比较 view_type、source 偏移、content 的 SHA256 hash 作为签名，
        // 避免仅靠长度判断导致长度相同但内容不同的 passage 被错误跳过，
        // 同时防止 node_source_start 变化后旧的绝对偏移继续残留。
        let mut stmt = conn.prepare(
            "SELECT view_type, content, source_start, source_end FROM kb_passage_spans WHERE node_id = ?1 ORDER BY CASE view_type WHEN 'atomic' THEN 0 WHEN 'window' THEN 1 WHEN 'raw' THEN 2 END, passage_order",
        )?;
        let rows: Vec<(String, String, i64, i64)> = stmt
            .query_map(params![node_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let unchanged = rows.len() == drafts.len()
            && drafts.iter().enumerate().all(|(i, draft)| {
                if rows[i].0 != draft.view_type {
                    return false;
                }
                // 比较绝对偏移：node_source_start + draft 相对偏移应等于 DB 中的绝对偏移
                let expected_start = node_source_start as i64 + draft.source_start as i64;
                let expected_end = node_source_start as i64 + draft.source_end as i64;
                if rows[i].2 != expected_start || rows[i].3 != expected_end {
                    return false;
                }
                // m-21 已知开销：对每个 passage 计算 SHA256 以判断内容是否变更。
                // 大文档批量重建时此开销可感知。未来改进方向：使用增量 hash（只计算变更部分）
                // 或先比较长度再按需计算 hash，以减少不必要的 SHA256 调用。
                let old_hash = Sha256::digest(rows[i].1.as_bytes());
                let new_hash = Sha256::digest(draft.content.as_bytes());
                old_hash == new_hash
            });

        if unchanged {
            return Ok(0);
        }
    }

    conn.execute(
        "DELETE FROM kb_passage_spans WHERE node_id = ?1",
        params![node_id],
    )?;

    for draft in &drafts {
        // P3 修复：passage 内容存"原文"（保留所有空白与零宽字符），
        // FTS 分词使用清洗后版本。这样 passage.source_start + excerpt.start 才能
        // 精确指回源文件位置，避免 query 端用清洗后的 char index 去叠加 raw base
        // 时因长度缩水而漂移。
        let cleaned_for_tokens = clean_text_for_search(&draft.content);
        let content_tokens = crate::nlp::chinese::tokenize_content(&cleaned_for_tokens);
        // 把 node 在源文件中的基址叠加到 passage 偏移上，保证写入的是源文件绝对偏移
        let absolute_start = node_source_start.saturating_add(draft.source_start);
        let absolute_end = node_source_start.saturating_add(draft.source_end);
        conn.execute(
            "INSERT INTO kb_passage_spans \
             (library_id, document_id, node_id, view_type, passage_order, \
              source_start, source_end, content, content_tokens, quality_score, metadata_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, '{}')",
            params![
                library_id,
                document_id,
                node_id,
                draft.view_type,
                draft.passage_order,
                absolute_start,
                absolute_end,
                draft.content,
                content_tokens,
                draft.quality_score,
            ],
        )?;
    }

    Ok(drafts.len())
}

/// Rebuild passage views for every level-0 node in a document.
pub fn rebuild_document_passages(conn: &Connection, document_id: i64) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id, library_id, document_id, content, source_start \
         FROM kb_document_nodes WHERE document_id = ?1 AND level = 0 \
         ORDER BY chunk_order",
    )?;
    let rows = stmt.query_map(params![document_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            // source_start 在 schema 中可能为 NULL（旧数据兼容），缺失时按 0 处理
            row.get::<_, Option<i64>>(4)?,
        ))
    })?;

    let mut count = 0;
    for row in rows {
        let (node_id, library_id, doc_id, content, node_source_start) = row?;
        // 修复：把 node 自身的源文件基址传入，让 passage 偏移成为源文件绝对偏移
        let base_offset = node_source_start.unwrap_or(0).max(0) as i32;
        count +=
            rebuild_passages_for_node(conn, node_id, library_id, doc_id, &content, base_offset)?;
    }
    Ok(count)
}

/// Ensure passage rows exist for a document. This is intentionally cheap and
/// only rebuilds when no rows exist, so manually seeded tests and imported data
/// from partial pipelines still get the robust retrieval surface.
pub fn ensure_document_passages(conn: &Connection, document_id: i64) -> Result<usize> {
    let existing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM kb_passage_spans WHERE document_id = ?1",
        params![document_id],
        |row| row.get(0),
    )?;
    if existing > 0 {
        return Ok(0);
    }
    rebuild_document_passages(conn, document_id)
}

fn build_passage_drafts(content: &str) -> Vec<PassageDraft> {
    let mut atomic = Vec::new();
    let mut windows = Vec::new();
    let char_count = content.chars().count();

    if char_count == 0 {
        return Vec::new();
    }

    add_atomic_passages(content, &mut atomic);
    add_window_passages(content, &mut windows);

    atomic.truncate(MAX_ATOMIC_PASSAGES_PER_NODE);
    windows.truncate(MAX_WINDOW_PASSAGES_PER_NODE);

    let mut drafts = Vec::with_capacity(atomic.len() + windows.len());
    drafts.extend(atomic);
    drafts.extend(windows);
    if drafts.is_empty() {
        // P3 修复：兜底 raw 视图同样存原文，offset 与源文件 1:1 对齐
        drafts.push(PassageDraft {
            view_type: "raw",
            passage_order: 0,
            source_start: 0,
            source_end: char_count as i32,
            content: content.to_string(),
            quality_score: estimate_quality(content),
        });
    }

    drafts
}

fn add_atomic_passages(content: &str, drafts: &mut Vec<PassageDraft>) {
    let mut current = String::new();
    let mut start: Option<usize> = None;
    let mut order = 0;

    for (idx, ch) in content.chars().enumerate() {
        let is_boundary = matches!(ch, '\n' | '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}');
        if is_boundary {
            if flush_atomic(&mut current, &mut start, idx, order, drafts) {
                order += 1;
            }
        } else {
            if start.is_none() {
                start = Some(idx);
            }
            current.push(ch);
        }
    }
    let _ = flush_atomic(
        &mut current,
        &mut start,
        content.chars().count(),
        order,
        drafts,
    );
}

fn flush_atomic(
    current: &mut String,
    start: &mut Option<usize>,
    end: usize,
    order: i32,
    drafts: &mut Vec<PassageDraft>,
) -> bool {
    let Some(s) = *start else {
        current.clear();
        return false;
    };
    // P3 修复：长度/质量门槛仍按清洗后内容判断（避免空白堆积过关），
    // 但写入 draft 的 content 用原文，保证 source_start + offset_in_content
    // 仍能精确映射回源文件位置。
    let cleaned = clean_text_for_search(current);
    let len = cleaned.chars().count();
    let inserted = if (ATOMIC_MIN_CHARS..=ATOMIC_MAX_CHARS).contains(&len) {
        drafts.push(PassageDraft {
            view_type: "atomic",
            passage_order: order,
            source_start: s as i32,
            source_end: end as i32,
            quality_score: estimate_quality(&cleaned),
            content: current.clone(),
        });
        true
    } else {
        false
    };
    current.clear();
    *start = None;
    inserted
}

fn add_window_passages(content: &str, drafts: &mut Vec<PassageDraft>) {
    let chars: Vec<char> = content.chars().collect();
    if chars.is_empty() {
        return;
    }

    let base_step = WINDOW_CHARS.saturating_sub(WINDOW_OVERLAP).max(1);
    let window_chars = if chars.len() > MAX_WINDOW_PASSAGES_PER_NODE * base_step {
        let required_step = chars.len().div_ceil(MAX_WINDOW_PASSAGES_PER_NODE);
        required_step + WINDOW_OVERLAP
    } else {
        WINDOW_CHARS
    };
    // 注意: 不对 window_chars 设硬上限。自适应逻辑已通过增大窗口确保大文档被完整覆盖。
    // 对超大窗口的搜索质量由 FTS 索引和 rerank 层保证，而非限制窗口大小。
    let step = window_chars.saturating_sub(WINDOW_OVERLAP).max(1);
    let mut start = 0;
    let mut order = 0;
    while start < chars.len() && drafts.len() < MAX_WINDOW_PASSAGES_PER_NODE {
        let end = (start + window_chars).min(chars.len());
        let raw: String = chars[start..end].iter().collect();
        let cleaned = clean_text_for_search(&raw);
        if cleaned.chars().count() >= ATOMIC_MIN_CHARS {
            // P3 修复：写入 draft 的内容用原文 raw，让 source_start + 内部偏移
            // 与源文件 char 位置严格对齐；cleaned 仅用于质量评分与门槛判断。
            drafts.push(PassageDraft {
                view_type: "window",
                passage_order: order,
                source_start: start as i32,
                source_end: end as i32,
                quality_score: estimate_quality(&cleaned),
                content: raw,
            });
        }
        if end == chars.len() {
            break;
        }
        start += step;
        order += 1;
    }
}

fn estimate_quality(text: &str) -> f64 {
    let total = text.chars().count();
    if total == 0 {
        return 0.0;
    }
    let useful = text
        .chars()
        .filter(|c| c.is_alphanumeric() || crate::nlp::chinese::is_chinese(*c))
        .count();
    ((useful as f64 / total as f64) * 1.2).clamp(0.1, 1.0)
}
