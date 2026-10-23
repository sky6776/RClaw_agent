//! PDF 解析器 —— 与 rclaw-tools `read` 工具同一套引擎（pdf-inspector，底层 lopdf 0.42）
//!
//! 分工：
//! - pdf-inspector 负责加载（`%PDF-` 头校验、坏容器修复、加密文档空口令重试）、版式感知的
//!   文本提取（ToUnicode/CID 字体、阅读顺序、表格 → Markdown）以及每页 OCR 路由信号
//!   （scanned / vector_text / suspected_garbled_text / GID 字体 / 空文本）；
//! - lopdf 只做字典级的页几何与资源统计（MediaBox/CropBox、图片 XObject、批注），
//!   不解码任何内容流；
//! - 本模块把两者映射成 KB 管线既有的 page_analyses / page_texts / blocks 契约，
//!   下游 ocr_detector / pipeline / worker / mcp 不感知引擎更换。
//!
//! 解析器自身的防护（对象嵌套深度、字符串括号深度、PNG 预测器 try_reserve、对象流条目上限、
//! 内容流算子上限）由 lopdf 0.42 / pdf-inspector 内置，这里不再做与解析器无关的预扫描；
//! 解压体积与 rclaw-tools 同口径，由上游的文件大小上限（kb_max_file_size_mb）兜底。
//!
//! 已知残留（与 rclaw-tools 同等暴露）：lopdf 0.42 的 `Reader::get_compressed_object` 解析
//! 类型 2 交叉引用条目时不带调用方的环路集合，容器指向自身的条目会无限递归直接栈溢出；
//! 上游 lopdf 0.46 已修复，需等 pdf-inspector 升级其 lopdf 依赖后一并升级。
//!
//! P2-004: Outputs page_number metadata per block
//! P2-005: Heuristic header/footer removal
//! P2-006: Text density detection → needs_ocr flag
//! Phase 1: 增强页级分析 — 图片检测、矢量对象、完整 OCR metadata

use super::{DocumentParser, ParsedDocument};
use crate::error::GBrainError;
use std::collections::HashMap;

pub struct PdfParser;

/// 单个 PDF 允许解析的最大页数：pdf-inspector 组装每页 Markdown 时都要扫一遍全文档的文本项
/// （O(页数 × 文本项)），几 MB 的空白页 PDF 就能塞下 10 万页，把唯一的 KB worker 占住很久。
const MAX_PDF_PAGES: usize = 5000;

/// pdf-inspector 的每页 OCR 原因标签（与 `pdf_inspector::OCR_REASON_*` 一致）
const OCR_SIGNAL_VECTOR_TEXT: &str = pdf_inspector::OCR_REASON_VECTOR_TEXT;
const OCR_SIGNAL_GARBLED_TEXT: &str = pdf_inspector::OCR_REASON_SUSPECTED_GARBLED_TEXT;

/// 与 pdf-inspector 的加载策略对齐：先做同样的裸结构名修正（见 normalize_pdf_bytes），
/// 普通加载后若文档仍处于加密态（或加载直接报加密错误），用空口令重试一次——
/// 只设了 owner 口令的"受保护"PDF 是最常见情形。
pub(crate) fn load_document(data: &[u8]) -> Result<lopdf::Document, GBrainError> {
    let data = normalize_pdf_bytes(data);
    let data: &[u8] = data.as_ref();
    let loaded = match lopdf::Document::load_mem(data) {
        Ok(doc) if doc.is_encrypted() => load_with_empty_password(data),
        Ok(doc) => Ok(doc),
        Err(e) if is_encryption_error(&e) => load_with_empty_password(data),
        Err(e) => Err(e),
    };
    loaded.map_err(|e| GBrainError::FileError(format!("PDF load failed: {}", e)))
}

fn load_with_empty_password(data: &[u8]) -> Result<lopdf::Document, lopdf::Error> {
    lopdf::Document::load_mem_with_options(data, lopdf::LoadOptions::with_password(""))
}

fn is_encryption_error(e: &lopdf::Error) -> bool {
    matches!(
        e,
        lopdf::Error::Decryption(_)
            | lopdf::Error::InvalidPassword
            | lopdf::Error::AlreadyEncrypted
            | lopdf::Error::UnsupportedSecurityHandler(_)
    ) || matches!(e, lopdf::Error::Unimplemented(msg) if msg.contains("encrypted"))
}

/// 文件头定位窗口：规范实现（Acrobat 等）容忍 `%PDF-` 之前至多 1024 字节的前导垃圾
const PDF_HEADER_SEARCH_WINDOW: usize = 1024;

/// 让 lopdf 与 pdf-inspector 看同一份字节，否则两条加载路径会失配：
/// - 前导垃圾字节（HTTP/MIME 前言等）：lopdf 会自行跳到首个 `%PDF-`，pdf-inspector 只容忍 BOM/空白，
///   否则直接判 NotAPdf——这里在规范容忍的窗口内定位文件头并切片；
/// - 裸结构名（`/S Code` → `/S /Code`）：pdf-inspector 加载前会修正，lopdf 则丢弃整个对象，
///   含裸名的页树节点会让页数变成 0（MAX_PDF_PAGES 形同虚设）、OCR 拆分子文件页码对不上。
///
/// 两步都是幂等的，pdf-inspector 自己再做一遍无副作用；无需修正时借用原切片，不复制。
fn normalize_pdf_bytes(data: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let window_end = data.len().min(PDF_HEADER_SEARCH_WINDOW + b"%PDF-".len());
    let body = match data[..window_end].windows(5).position(|w| w == b"%PDF-") {
        Some(pos) if pos > 0 => &data[pos..],
        _ => data,
    };
    pdf_inspector::structure_tree::fix_bare_struct_names(body)
}

/// 快速获取 PDF 页数，不解析文本内容
pub fn count_pdf_pages(data: &[u8]) -> Result<usize, GBrainError> {
    // pdf-inspector 回退路径也要看规范化后的字节（load_document 内部的再次规范化是幂等的）
    let data = normalize_pdf_bytes(data);
    let data: &[u8] = data.as_ref();
    match load_document(data) {
        Ok(doc) => Ok(doc.get_pages().len()),
        // lopdf 直接加载失败时回退到 pdf-inspector（它还会尝试修复缺 %%EOF / startxref 错位 /
        // 前导垃圾字节的容器），与 PdfParser::parse 能解析的文档集合保持一致。
        Err(err) => pdf_inspector::detect_pdf_mem(data)
            .map(|result| result.page_count as usize)
            .map_err(|_| err),
    }
}

fn too_many_pages_error(total_pages: usize) -> GBrainError {
    GBrainError::InvalidInput(format!(
        "PDF 页数 {total_pages} 超过上限 {MAX_PDF_PAGES}，请拆分文件后重试"
    ))
}

/// 整篇提取失败后二分重试的调用次数上限：每次重试都是一遍全文档提取，
/// 坏页很多的（恶意）文档必须有界；预算耗尽时剩余未定位的页按解析失败处理。
const MAX_EXTRACTION_RETRY_CALLS: usize = 16;

/// pdf-inspector 只在"被请求的页"出错时才整体失败，未被请求的坏页会被跳过；
/// 据此按 0 基页序区间二分，把坏页隔离出来而不丢掉其余页的文本。
/// 返回成功提取到的页结果；确认损坏或预算耗尽未定位的页不在返回值中，
/// 调用方按 content_parse_failed 处理。
fn extract_pages_isolating_failures(
    data: &[u8],
    total_pages: usize,
) -> (
    Vec<pdf_inspector::PageMarkdown>,
    Vec<pdf_inspector::PageOcrReasons>,
) {
    let mut pages = Vec::new();
    let mut reasons = Vec::new();
    let total = total_pages as u32;
    // 整篇已失败：单页文档即该页损坏，多页文档直接从两半开始。左闭右开区间，按层（FIFO）处理：
    // 预算耗尽时未定位的只是仍含坏页的那几个小区间，而不是深度优先时整个后半篇
    let mut pending: std::collections::VecDeque<(u32, u32)> = std::collections::VecDeque::new();
    if total >= 2 {
        let mid = total / 2;
        pending.push_back((0, mid));
        pending.push_back((mid, total));
    }
    let mut calls = 0usize;
    while let Some((start, end)) = pending.pop_front() {
        if calls >= MAX_EXTRACTION_RETRY_CALLS {
            let unresolved: u32 = pending.iter().map(|(s, e)| e - s).sum::<u32>() + (end - start);
            tracing::warn!(
                unresolved_pages = unresolved,
                "pdf-inspector 二分重试预算耗尽，剩余未定位的页按内容流解析失败处理"
            );
            break;
        }
        calls += 1;
        let range: Vec<u32> = (start..end).collect();
        match pdf_inspector::extract_pages_markdown_mem(data, Some(&range)) {
            Ok(result) => {
                pages.extend(result.pages);
                reasons.extend(result.ocr_reasons_by_page);
            }
            Err(e) if end - start == 1 => {
                tracing::warn!(page = start + 1, error = %e, "该页内容流解析失败，按 content_parse_failed 处理");
            }
            Err(_) => {
                let mid = start + (end - start) / 2;
                pending.push_back((start, mid));
                pending.push_back((mid, end));
            }
        }
    }
    (pages, reasons)
}

impl Default for PdfParser {
    fn default() -> Self {
        Self::new()
    }
}

impl PdfParser {
    pub fn new() -> Self {
        Self
    }
}

impl DocumentParser for PdfParser {
    fn parse(&self, data: &[u8]) -> Result<ParsedDocument, GBrainError> {
        // 两条加载路径必须看同一份字节；pdf-inspector 不会自行跳过前导垃圾，所以这里先规范化
        // （load_document 内部的再次规范化是幂等的）
        let data = normalize_pdf_bytes(data);
        let data: &[u8] = data.as_ref();

        // 1) lopdf 字典级加载：页数上限前置检查 + 每页几何 / 图片 / 批注统计。
        //    加载失败不直接判死——pdf-inspector 还会尝试修复坏容器；此时几何信息缺失，
        //    由 ocr_detector 的 GeometryUncertain 保守送 OCR。
        let doc = match load_document(data) {
            Ok(doc) => Some(doc),
            Err(e) => {
                tracing::warn!(error = %e, "lopdf 直接加载失败，改由 pdf-inspector 单独解析");
                None
            }
        };
        // get_pages() 返回 BTreeMap<u32, ObjectId>，按页码升序；value 是 PDF 内真实的 page object ID
        let lopdf_page_ids: Vec<lopdf::ObjectId> = doc
            .as_ref()
            .map(|d| d.get_pages().into_values().collect())
            .unwrap_or_default();
        if lopdf_page_ids.len() > MAX_PDF_PAGES {
            return Err(too_many_pages_error(lopdf_page_ids.len()));
        }
        // lopdf 加载失败时页数未知：先用 pdf-inspector 的线性检测拿页数并核对上限，
        // 否则一个 startxref 被改坏的超多页文档会绕过 MAX_PDF_PAGES 直接进入全量提取。
        if doc.is_none() {
            let detected_pages = pdf_inspector::detect_pdf_mem(data)
                .map_err(|e| GBrainError::FileError(format!("PDF load failed: {}", e)))?
                .page_count as usize;
            if detected_pages > MAX_PDF_PAGES {
                return Err(too_many_pages_error(detected_pages));
            }
        }

        // 2) pdf-inspector：每页 Markdown + OCR 路由信号。needs_ocr 的页其 markdown 已被置空
        //    （不可靠文本不进入索引），与 rclaw-tools 的 read 工具回退为附件的口径一致。
        //    整篇提取只要有一页内容流解码失败就整体报错，此时按页区间二分重试隔离坏页，
        //    其余页的文本层照常保留。
        let (total_pages, inspector_pages, inspector_reasons) =
            match pdf_inspector::extract_pages_markdown_mem(data, None) {
                Ok(result) => (result.pages.len(), result.pages, result.ocr_reasons_by_page),
                Err(e) => {
                    // 只有页级解析错误（某页内容流解码失败，PdfError::Parse）值得按页隔离；
                    // NotAPdf / 加密 / 结构损坏等文档级失败在重试里只会原样重现，
                    // 必须直接报错，不能退化成"每页都坏"而静默丢掉全文
                    if doc.is_none() || !matches!(e, pdf_inspector::PdfError::Parse(_)) {
                        return Err(GBrainError::FileError(format!("PDF load failed: {}", e)));
                    }
                    tracing::warn!(error = %e, "pdf-inspector 整篇提取失败，按页区间二分重试以隔离坏页");
                    let (pages, reasons) =
                        extract_pages_isolating_failures(data, lopdf_page_ids.len());
                    (lopdf_page_ids.len(), pages, reasons)
                }
            };
        if total_pages > MAX_PDF_PAGES {
            return Err(too_many_pages_error(total_pages));
        }
        if doc.is_some() && total_pages != lopdf_page_ids.len() {
            tracing::warn!(
                lopdf_pages = lopdf_page_ids.len(),
                inspector_pages = total_pages,
                "两种加载路径页数不一致，几何信息按页序对齐、缺失部分视为不确定"
            );
        }
        // pdf-inspector 的页结果按 0 基页序返回，OCR 原因按 1 基页码返回
        let pages_by_index: HashMap<u32, &pdf_inspector::PageMarkdown> =
            inspector_pages.iter().map(|p| (p.page, p)).collect();
        let signals_by_page: HashMap<u32, &[String]> = inspector_reasons
            .iter()
            .map(|entry| (entry.page, entry.reasons.as_slice()))
            .collect();

        // FIX9-04: 在拼接全文时累计字符偏移，为每页 block 写入 source span
        let mut page_texts: Vec<String> = Vec::new();
        let mut all_text = Vec::new();
        let mut low_density_pages = 0u32;
        // 记录每页 block 在全文中的起止偏移
        let mut page_spans: Vec<(usize, usize)> = Vec::new();
        let mut global_offset: usize = 0;

        // Phase 1: 页级分析结果
        let mut page_analyses: Vec<serde_json::Value> = Vec::new();
        let mut image_rich_pages: Vec<i32> = Vec::new();
        let mut uncertain_pages: Vec<i32> = Vec::new();

        for page_idx in 0..total_pages {
            let page_num_i32 = (page_idx + 1) as i32;

            // 字典级几何 / 图片 / 批注统计；lopdf 未加载或页序对不上时按"不确定"处理
            let geometry = doc
                .as_ref()
                .zip(lopdf_page_ids.get(page_idx))
                .map(|(pdf, page_id)| analyze_page_geometry(pdf, *page_id))
                .unwrap_or_default();

            let page = pages_by_index.get(&(page_idx as u32)).copied();
            // pdf-inspector 整体失败（或缺页）时该页视为内容流解析失败，强制 OCR
            let content_parse_failed = page.is_none();
            let raw_text = page.map(|p| p.markdown.as_str()).unwrap_or("");
            let signals: &[String] = signals_by_page
                .get(&(page_idx as u32 + 1))
                .copied()
                .unwrap_or(&[]);
            // 路径勾勒的文字（矢量字形）无法按字符提取
            let has_vector_drawing_ops = signals.iter().any(|s| s == OCR_SIGNAL_VECTOR_TEXT);
            // 字体解码失败 / 乱码（pdf-inspector 的文本质量分析，替代旧的私有区字符占比启发式）
            let font_encoding_suspected = signals.iter().any(|s| s == OCR_SIGNAL_GARBLED_TEXT);
            let has_vector_or_unknown_objects =
                geometry.has_unknown_objects || geometry.has_annotations;

            if geometry.image_area_ratio >= 0.08 || geometry.image_count >= 1 {
                image_rich_pages.push(page_num_i32);
            }
            if has_vector_or_unknown_objects || content_parse_failed {
                uncertain_pages.push(page_num_i32);
            }

            let cleaned = clean_text(raw_text);
            let deduped = remove_header_footer(&cleaned, &page_texts);

            let density = deduped.chars().count();
            if density < 50 {
                low_density_pages += 1;
            }

            // Phase 1: 记录页级分析（字段集合保持不变；ocr_signals 为 pdf-inspector 原始标签，仅供诊断）
            page_analyses.push(serde_json::json!({
                "page_number": page_num_i32,
                "text": deduped,
                "char_count": density,
                "image_area_ratio": geometry.image_area_ratio,
                "image_count": geometry.image_count,
                "has_vector_or_unknown_objects": has_vector_or_unknown_objects,
                "has_annotations": geometry.has_annotations,
                "width": geometry.width,
                "height": geometry.height,
                "content_parse_failed": content_parse_failed,
                "has_vector_drawing_ops": has_vector_drawing_ops,
                // pdf-inspector 不提取 Tr=3 的不可见文本层，这类页会以空文本 + 整页图片的形态进入 OCR 判定
                "has_invisible_text": false,
                "font_encoding_suspected": font_encoding_suspected,
                "ocr_signals": signals,
            }));

            if !deduped.trim().is_empty() {
                let page_block = format!("[PAGE:{}]\n{}", page_num_i32, deduped);
                let start = global_offset;
                let block_len = page_block.chars().count();
                let end = start + block_len;
                page_spans.push((start, end));
                global_offset += block_len + 2;
                all_text.push(page_block);
            } else {
                page_spans.push((global_offset, global_offset));
            }
            page_texts.push(deduped);
        }

        let content = all_text.join("\n\n");

        // Conservative parser result; the configured detector later persists
        // the authoritative selection used for execution.
        let mut needs_ocr_pages: Vec<i32> = Vec::new();
        let mut ocr_reasons_by_page: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for page in &page_analyses {
            let page_number = page
                .get("page_number")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;
            let mut reasons = Vec::new();
            let text_empty = page
                .get("text")
                .and_then(|v| v.as_str())
                .map(|v| v.trim().is_empty())
                .unwrap_or(true);
            let char_count = page.get("char_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if text_empty {
                reasons.push("empty_text_layer".to_string());
            }
            if char_count < 50 {
                reasons.push("low_text_density".to_string());
            }
            if page
                .get("image_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                > 0
            {
                reasons.push("embedded_image".to_string());
            }
            if page
                .get("image_area_ratio")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                >= 0.08
            {
                reasons.push("image_area".to_string());
            }
            for (key, reason) in [
                ("has_vector_or_unknown_objects", "vector_or_unknown_objects"),
                ("has_annotations", "has_annotations"),
                ("content_parse_failed", "parser_error"),
                ("has_vector_drawing_ops", "vector_or_unknown_objects"),
                ("has_invisible_text", "hidden_or_invisible_text_layer"),
                ("font_encoding_suspected", "font_encoding_issue"),
            ] {
                if page.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
                    && !reasons.iter().any(|existing| existing == reason)
                {
                    reasons.push(reason.to_string());
                }
            }
            if !reasons.is_empty() {
                needs_ocr_pages.push(page_number);
                ocr_reasons_by_page.insert(page_number.to_string(), reasons);
            }
        }
        let needs_ocr = !needs_ocr_pages.is_empty();
        let ocr_scope = if needs_ocr_pages.is_empty() {
            "none"
        } else if needs_ocr_pages.len() == total_pages {
            "full"
        } else {
            "partial"
        };

        let mut metadata = HashMap::new();
        metadata.insert("total_pages".to_string(), total_pages.to_string());
        // P2-004: 每页文本以 JSON 数组记录（含 page_number）
        metadata.insert(
            "page_texts".to_string(),
            serde_json::to_string(&page_texts).unwrap_or_default(),
        );
        // P2-006: 文本密度标记
        metadata.insert("needs_ocr".to_string(), needs_ocr.to_string());
        metadata.insert(
            "low_density_pages".to_string(),
            low_density_pages.to_string(),
        );

        // Phase 1: 完整 OCR metadata
        metadata.insert(
            "page_analyses".to_string(),
            serde_json::to_string(&page_analyses).unwrap_or_default(),
        );
        metadata.insert(
            "image_rich_pages".to_string(),
            serde_json::to_string(&image_rich_pages).unwrap_or_default(),
        );
        metadata.insert(
            "uncertain_pages".to_string(),
            serde_json::to_string(&uncertain_pages).unwrap_or_default(),
        );
        metadata.insert(
            "needs_ocr_pages".to_string(),
            serde_json::to_string(&needs_ocr_pages).unwrap_or_default(),
        );
        metadata.insert(
            "ocr_reasons_by_page".to_string(),
            serde_json::to_string(&ocr_reasons_by_page).unwrap_or_default(),
        );
        metadata.insert("ocr_scope".to_string(), ocr_scope.to_string());

        // FIX9-04: 为每页 block 写入真实的 source span（基于 page_spans）
        let blocks: Vec<crate::kb::types::ParsedBlock> = page_texts
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let (start, end) = page_spans.get(i).copied().unwrap_or((0, 0));
                crate::kb::types::ParsedBlock {
                    text: text.clone(),
                    title_path: String::new(),
                    page_number: Some((i + 1) as i32),
                    source_start: Some(start as i32),
                    source_end: Some(end as i32),
                    block_type: "page".to_string(),
                    metadata: String::new(),
                }
            })
            .collect();

        Ok(ParsedDocument {
            content,
            metadata,
            blocks: Some(blocks),
            media_refs: Vec::new(),
        })
    }

    fn extensions(&self) -> &[&str] {
        &["pdf"]
    }
}

/// 将 lopdf Object 转为 f64（支持 Integer 和 Real 两种变体）
fn object_to_f64(obj: &lopdf::Object) -> Option<f64> {
    match obj {
        lopdf::Object::Real(f) => Some(*f as f64),
        lopdf::Object::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

/// 把（可能是间接引用的）对象解析为字典；XObject 本体是流对象，取其流字典
fn resolve_as_dict<'a>(
    pdf: &'a lopdf::Document,
    obj: &'a lopdf::Object,
) -> Option<&'a lopdf::Dictionary> {
    let target = match obj {
        lopdf::Object::Reference(obj_id) => pdf.get_object(*obj_id).ok()?,
        other => other,
    };
    match target {
        lopdf::Object::Dictionary(d) => Some(d),
        lopdf::Object::Stream(s) => Some(&s.dict),
        _ => None,
    }
}

/// 获取页面的 Resources 字典，解析间接引用并沿父 Pages 节点继承资源
fn get_inherited_resources(
    pdf: &lopdf::Document,
    page_obj_id: lopdf::ObjectId,
) -> Option<&lopdf::Dictionary> {
    let mut current_id = page_obj_id;
    let mut visited: std::collections::HashSet<lopdf::ObjectId> = std::collections::HashSet::new();
    let mut depth = 0usize;
    const MAX_PARENT_DEPTH: usize = 64;
    loop {
        if depth > MAX_PARENT_DEPTH {
            // 遍历深度异常，返回 None 由上层保守处理
            return None;
        }
        if !visited.insert(current_id) {
            // 检测到 Parent 环（自引用或成环），返回 None 由上层保守处理
            return None;
        }
        depth += 1;
        let obj = pdf.get_object(current_id).ok()?;
        let dict = obj.as_dict().ok()?;
        if let Ok(resources) = dict.get(b"Resources") {
            return resolve_as_dict(pdf, resources);
        }
        // 当前节点无 Resources，沿 Parent 向上查找
        let parent = dict.get(b"Parent").ok()?;
        match parent {
            lopdf::Object::Reference(id) => current_id = *id,
            _ => return None,
        }
    }
}

/// 读取 [x1, y1, x2, y2] 坐标数组：四个坐标全部可解析且为有限数时才返回
fn parse_rect(obj: &lopdf::Object) -> Option<[f64; 4]> {
    let arr = obj.as_array().ok()?;
    if arr.len() < 4 {
        return None;
    }
    let x1 = object_to_f64(&arr[0])?;
    let y1 = object_to_f64(&arr[1])?;
    let x2 = object_to_f64(&arr[2])?;
    let y2 = object_to_f64(&arr[3])?;
    if x1.is_finite() && y1.is_finite() && x2.is_finite() && y2.is_finite() {
        Some([x1, y1, x2, y2])
    } else {
        None
    }
}

/// 沿父 Pages 节点向上解析继承的 MediaBox 或 CropBox。
/// 坐标存在但无法解析为有效数字时不用默认值替代，返回 None 让 detector 将此页归入 uncertain。
fn get_inherited_media_box(
    pdf: &lopdf::Document,
    page_obj_id: lopdf::ObjectId,
) -> Option<[f64; 4]> {
    let mut current_id = page_obj_id;
    let mut visited: std::collections::HashSet<lopdf::ObjectId> = std::collections::HashSet::new();
    let mut depth = 0usize;
    const MAX_PARENT_DEPTH: usize = 64;
    loop {
        if depth > MAX_PARENT_DEPTH {
            // 遍历深度异常，返回 None 由 detector 保守处理
            return None;
        }
        if !visited.insert(current_id) {
            // 检测到 Parent 环（自引用或成环），返回 None 由 detector 保守处理
            return None;
        }
        depth += 1;
        let obj = pdf.get_object(current_id).ok()?;
        let dict = obj.as_dict().ok()?;
        // 优先 MediaBox，回退 CropBox；键存在但坐标无效即判为不确定
        for key in [&b"MediaBox"[..], &b"CropBox"[..]] {
            if let Ok(rect) = dict.get(key) {
                return parse_rect(rect);
            }
        }
        // 沿 Parent 向上
        let parent = dict.get(b"Parent").ok()?;
        match parent {
            lopdf::Object::Reference(id) => current_id = *id,
            _ => return None,
        }
    }
}

/// 批注是否为纯导航用的 Link：pdf-inspector 会提取 Link 批注，其余批注（印章、签名图、
/// 自由文本、表单控件外观等）的 appearance stream 不参与文本层。无法解析的批注按非 Link 处理。
fn is_link_annotation(pdf: &lopdf::Document, annot: &lopdf::Object) -> bool {
    resolve_as_dict(pdf, annot)
        .and_then(|dict| dict.get(b"Subtype").ok())
        .and_then(|subtype| subtype.as_name().ok())
        .map(|name| name == b"Link")
        .unwrap_or(false)
}

/// lopdf 字典级的页面统计：只读页对象 / Pages 链 / Resources / XObject / Annots 字典，
/// 不解码任何内容流。
struct PageGeometry {
    /// 页面宽度（来自 MediaBox/CropBox，继承后仍缺失则为 None → GeometryUncertain）
    width: Option<u32>,
    height: Option<u32>,
    /// 图片 XObject 数量，含 Form XObject 内嵌套的图片；内容流里的 inline image（BI/ID/EI，
    /// 规范建议 ≤4KB 的小位图）需要解码内容流才能发现，不在此统计
    image_count: usize,
    /// 图片像素面积之和 / 页面面积的粗略占比（上限 1.0）
    image_area_ratio: f64,
    /// Resources 不可解析、XObject 不可解析或子类型未知：无法证明页面没有承载文字的对象，
    /// 保守纳入 uncertain
    has_unknown_objects: bool,
    /// 存在 Link 以外的批注
    has_annotations: bool,
}

impl Default for PageGeometry {
    fn default() -> Self {
        Self {
            width: None,
            height: None,
            image_count: 0,
            image_area_ratio: 0.0,
            // 无法分析时默认不确定
            has_unknown_objects: true,
            has_annotations: false,
        }
    }
}

/// 分析 PDF 页面对象字典：页面尺寸、图片 XObject、批注
///
/// `page_obj_id` 为 get_pages() 返回的真实 ObjectId，不可用页码构造
fn analyze_page_geometry(pdf: &lopdf::Document, page_obj_id: lopdf::ObjectId) -> PageGeometry {
    let mut geometry = PageGeometry::default();
    let Some(page_dict) = pdf
        .get_object(page_obj_id)
        .ok()
        .and_then(|obj| obj.as_dict().ok())
    else {
        return geometry;
    };

    // 页面尺寸：先页面自身的 MediaBox/CropBox，缺失时沿父 Pages 节点继承
    let mut page_area: f64 = 1.0; // 默认 1.0 避免除零
    if let Some([x1, y1, x2, y2]) = get_inherited_media_box(pdf, page_obj_id) {
        let w = (x2 - x1).abs();
        let h = (y2 - y1).abs();
        page_area = (w * h).max(1.0);
        geometry.width = Some(w as u32);
        geometry.height = Some(h as u32);
    }

    // 批注（/Annots）：印章、签名图、表单控件等可见内容可能只存在于 appearance stream
    if let Ok(annots_obj) = page_dict.get(b"Annots") {
        let annots: Option<&Vec<lopdf::Object>> = match annots_obj {
            lopdf::Object::Array(arr) => Some(arr),
            lopdf::Object::Reference(id) => {
                pdf.get_object(*id).ok().and_then(|o| o.as_array().ok())
            }
            _ => None,
        };
        geometry.has_annotations = match annots {
            Some(arr) => arr.iter().any(|annot| !is_link_annotation(pdf, annot)),
            // /Annots 存在却解析不了，保守视为有未知批注
            None => true,
        };
    }

    // 资源：统计图片 XObject。Form XObject 的文本由 pdf-inspector 递归提取，不再视为矢量对象，
    // 但其自身 Resources 里嵌套的图片仍要计入（排版软件放置的图像/PDF 页常以 Form 包一层）
    let mut total_image_area: f64 = 0.0;
    if let Some(res_dict) = get_inherited_resources(pdf, page_obj_id) {
        geometry.has_unknown_objects = false;
        if let Ok(xobjects) = res_dict.get(b"XObject") {
            match resolve_as_dict(pdf, xobjects) {
                Some(xobj_dict) => count_xobject_images(
                    pdf,
                    xobj_dict,
                    &mut std::collections::HashSet::new(),
                    0,
                    &mut geometry,
                    &mut total_image_area,
                ),
                // /XObject 无法解析为字典，保守标记为不确定
                None => geometry.has_unknown_objects = true,
            }
        }
    }

    // 图片面积占比（图片坐标未归一化，仅作粗略估算）
    geometry.image_area_ratio = if page_area > 0.0 {
        (total_image_area / page_area).min(1.0)
    } else {
        0.0
    };
    geometry
}

/// Form XObject 嵌套递归的最大层数，超过即按不确定处理
const MAX_FORM_XOBJECT_DEPTH: usize = 8;

/// 遍历一个 /XObject 字典：Image 计数并累加像素面积；Form 递归其自身 Resources/XObject；
/// 不可解析或子类型未知则标记不确定。`visited` 以对象 ID 防环并保证每个对象只访问一次。
fn count_xobject_images(
    pdf: &lopdf::Document,
    xobj_dict: &lopdf::Dictionary,
    visited: &mut std::collections::HashSet<lopdf::ObjectId>,
    depth: usize,
    geometry: &mut PageGeometry,
    total_image_area: &mut f64,
) {
    for (_name, obj) in xobj_dict.iter() {
        if let lopdf::Object::Reference(id) = obj {
            if !visited.insert(*id) {
                continue;
            }
        }
        let Some(xobj) = resolve_as_dict(pdf, obj) else {
            // 无法解析的 XObject 保守标记为不确定
            geometry.has_unknown_objects = true;
            continue;
        };
        match xobj.get(b"Subtype") {
            Ok(lopdf::Object::Name(name)) if name == b"Image" => {
                geometry.image_count += 1;
                // 估算图片面积（像素尺寸，缺失时按 100×100）
                let img_w = xobj
                    .get(b"Width")
                    .ok()
                    .and_then(object_to_f64)
                    .unwrap_or(100.0);
                let img_h = xobj
                    .get(b"Height")
                    .ok()
                    .and_then(object_to_f64)
                    .unwrap_or(100.0);
                *total_image_area += img_w * img_h;
            }
            Ok(lopdf::Object::Name(name)) if name == b"Form" => {
                if depth >= MAX_FORM_XOBJECT_DEPTH {
                    geometry.has_unknown_objects = true;
                    continue;
                }
                // Form 自身没有 Resources 时沿用页面资源（PDF 1.1 兼容写法），已在上层统计过
                let Ok(form_resources) = xobj.get(b"Resources") else {
                    continue;
                };
                let Some(form_res_dict) = resolve_as_dict(pdf, form_resources) else {
                    // Resources 无法解析，保守标记为不确定
                    geometry.has_unknown_objects = true;
                    continue;
                };
                // Form 的 Resources 没有 XObject：只有文本/路径，pdf-inspector 已覆盖
                let Ok(nested) = form_res_dict.get(b"XObject") else {
                    continue;
                };
                match resolve_as_dict(pdf, nested) {
                    Some(nested_dict) => count_xobject_images(
                        pdf,
                        nested_dict,
                        visited,
                        depth + 1,
                        geometry,
                        total_image_area,
                    ),
                    // 嵌套 /XObject 无法解析为字典，保守标记为不确定
                    None => geometry.has_unknown_objects = true,
                }
            }
            // 未知 Subtype 或缺失，保守标记为不确定
            _ => geometry.has_unknown_objects = true,
        }
    }
}

fn clean_text(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut result = Vec::new();
    let mut prev_empty = false;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !prev_empty {
                result.push(String::new());
                prev_empty = true;
            }
        } else {
            let normalized: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
            result.push(normalized);
            prev_empty = false;
        }
    }

    result.join("\n")
}

/// P2-005: 启发式去除页眉页脚 — 检测与前页重复的首/尾行
fn remove_header_footer(text: &str, previous_pages: &[String]) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= 2 || previous_pages.is_empty() {
        return text.to_string();
    }

    let first = lines[0].trim();
    let last = lines.last().map(|l| l.trim()).unwrap_or("");

    let mut remove_first = false;
    let mut remove_last = false;

    // 检查与先前页面的重复
    for prev in previous_pages.iter().rev().take(3) {
        let prev_lines: Vec<&str> = prev.lines().collect();
        if !prev_lines.is_empty() && prev_lines[0].trim() == first {
            remove_first = true;
        }
        if let Some(prev_last) = prev_lines.last() {
            if prev_last.trim() == last && last.chars().count() < 50 {
                remove_last = true;
            }
        }
    }

    // 检测纯数字行（页码）
    if first.chars().all(|c| c.is_ascii_digit()) {
        remove_first = true;
    }
    if last.chars().all(|c| c.is_ascii_digit()) {
        remove_last = true;
    }

    let range = if remove_first { 1 } else { 0 }..if remove_last {
        lines.len().saturating_sub(1)
    } else {
        lines.len()
    };

    lines[range].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remove_header_footer_repeat() {
        let page1 = "Chapter 1\nSome content\nPage 1";
        let page2 = "Chapter 1\nMore content\nPage 2";
        let previous = vec![page1.to_string()];
        let result = remove_header_footer(page2, &previous);
        assert!(!result.contains("Chapter 1"));
        assert!(result.contains("More content"));
        // 页码被移除（footer 变了所以不会被移除，因为不匹配 previous）
    }

    #[test]
    fn test_clean_text() {
        let text = "Hello   world\n\n   \nFoo bar";
        let result = clean_text(text);
        assert!(result.contains("Hello world"));
        assert!(result.contains("Foo bar"));
    }

    #[test]
    fn rejects_non_pdf_bytes() {
        let err = PdfParser::new().parse(b"not a pdf").unwrap_err();
        assert!(err.to_string().contains("PDF load failed"), "实际: {err}");
        assert!(count_pdf_pages(b"not a pdf").is_err());
    }

    #[test]
    fn parses_lopdf_generated_document() {
        use lopdf::content::{Content, Operation};
        use lopdf::{dictionary, Document, Object, Stream};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let resources_id =
            doc.add_object(dictionary! { "Font" => dictionary! { "F1" => font_id } });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 48.into()]),
                Operation::new("Td", vec![100.into(), 600.into()]),
                Operation::new("Tj", vec![Object::string_literal("Hello gbrain reader")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        doc.compress();
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        assert_eq!(count_pdf_pages(&bytes).unwrap(), 1);

        let parsed = PdfParser::new().parse(&bytes).unwrap();
        assert_eq!(
            parsed.metadata.get("total_pages").map(String::as_str),
            Some("1")
        );
        assert_eq!(parsed.blocks.as_ref().map(Vec::len), Some(1));
        assert!(
            parsed.content.contains("Hello gbrain reader"),
            "实际: {}",
            parsed.content
        );

        // 继承自 Pages 节点的 MediaBox 必须被解析到，否则每页都会被判成 GeometryUncertain
        let analyses: Vec<serde_json::Value> =
            serde_json::from_str(parsed.metadata.get("page_analyses").unwrap()).unwrap();
        assert_eq!(analyses[0]["width"], serde_json::json!(595));
        assert_eq!(analyses[0]["height"], serde_json::json!(842));
        assert_eq!(
            analyses[0]["has_vector_or_unknown_objects"],
            serde_json::json!(false)
        );
    }
}
