//! PDF parser with page-level extraction, header/footer cleaning, and OCR tagging
//!
//! P2-004: Outputs page_number metadata per block
//! P2-005: Heuristic header/footer removal
//! P2-006: Text density detection → needs_ocr flag
//! Phase 1: 增强页级分析 — 图片检测、矢量对象、完整 OCR metadata

use super::{DocumentParser, ParsedDocument};
use crate::error::GBrainError;
use std::collections::HashMap;

pub struct PdfParser;

/// 快速获取 PDF 页数，不解析文本内容
pub fn count_pdf_pages(data: &[u8]) -> Result<usize, GBrainError> {
    let pdf = lopdf::Document::load_mem(data)
        .map_err(|e| GBrainError::FileError(format!("PDF load failed: {}", e)))?;
    Ok(pdf.get_pages().len())
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
        let pdf = lopdf::Document::load_mem(data)
            .map_err(|e| GBrainError::FileError(format!("PDF load failed: {}", e)))?;

        let pages = pdf.get_pages();
        let total_pages = pages.len();
        // get_pages() 返回 BTreeMap<u32, ObjectId>，value 是 PDF 内真实的 page object ID
        let page_ids: Vec<(u32, lopdf::ObjectId)> = pages.into_iter().collect();

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

        for (page_idx, (page_num, page_obj_id)) in page_ids.iter().enumerate() {
            // Phase 1: 分析页面对象，检测图片、矢量对象、不可见文本、字体编码
            let analysis = analyze_page_objects(&pdf, *page_obj_id);

            let page_num_i32 = (page_idx + 1) as i32;

            if analysis.image_area_ratio >= 0.08 || analysis.image_count >= 1 {
                image_rich_pages.push(page_num_i32);
            }
            if analysis.has_vector_objects || analysis.content_parse_failed {
                uncertain_pages.push(page_num_i32);
            }

            match pdf.extract_text(&[*page_num]) {
                Ok(text) => {
                    let cleaned = clean_text(&text);
                    let deduped = remove_header_footer(&cleaned, &page_texts);

                    let density = deduped.chars().count();
                    if density < 50 {
                        low_density_pages += 1;
                    }

                    // 字体编码异常检测：content stream 有文本操作符但提取文本极少或为乱码
                    // 不限制字符数阈值，长乱码文本（大量私有区/替换字符）也需触发 OCR
                    let font_suspected = analysis.font_encoding_suspected
                        || (!analysis.content_parse_failed
                            && !deduped.trim().is_empty()
                            && is_text_garbled(&deduped));

                    // Phase 1: 记录页级分析
                    page_analyses.push(serde_json::json!({
                        "page_number": page_num_i32,
                        "text": deduped,
                        "char_count": density,
                        "image_area_ratio": analysis.image_area_ratio,
                        "image_count": analysis.image_count,
                        "has_vector_or_unknown_objects": analysis.has_vector_objects,
                        "has_annotations": analysis.has_annotations,
                        "width": analysis.page_width,
                        "height": analysis.page_height,
                        "content_parse_failed": analysis.content_parse_failed,
                        "has_vector_drawing_ops": analysis.has_vector_drawing_ops,
                        "has_invisible_text": analysis.has_invisible_text,
                        "font_encoding_suspected": font_suspected,
                    }));

                    page_texts.push(deduped.clone());
                    if !deduped.trim().is_empty() {
                        let page_block = format!("[PAGE:{}]\n{}", page_num, deduped);
                        let start = global_offset;
                        let block_len = page_block.chars().count();
                        let end = start + block_len;
                        page_spans.push((start, end));
                        global_offset += block_len + 2;
                        all_text.push(page_block);
                    } else {
                        page_spans.push((global_offset, global_offset));
                    }
                }
                Err(_) => {
                    // Phase 1: 解析失败的页也要记录分析
                    page_analyses.push(serde_json::json!({
                        "page_number": page_num_i32,
                        "text": "",
                        "char_count": 0,
                        "image_area_ratio": analysis.image_area_ratio,
                        "image_count": analysis.image_count,
                        "has_vector_or_unknown_objects": analysis.has_vector_objects,
                        "has_annotations": analysis.has_annotations,
                        "width": analysis.page_width,
                        "height": analysis.page_height,
                        "content_parse_failed": true,
                        "has_vector_drawing_ops": analysis.has_vector_drawing_ops,
                        "has_invisible_text": analysis.has_invisible_text,
                        "font_encoding_suspected": analysis.font_encoding_suspected,
                    }));
                    // 空页也要记录 span 占位
                    page_spans.push((global_offset, global_offset));
                    page_texts.push(String::new());
                    continue;
                }
            }
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

/// 解析 XObject 字典中的间接引用，返回实际对象
fn resolve_xobject<'a>(
    pdf: &'a lopdf::Document,
    obj: &'a lopdf::Object,
) -> Option<&'a lopdf::Object> {
    match obj {
        lopdf::Object::Reference(obj_id) => pdf.get_object(*obj_id).ok(),
        other => Some(other),
    }
}

/// 将 Object 解析为字典引用，处理间接引用
fn resolve_as_dict<'a>(
    pdf: &'a lopdf::Document,
    obj: &'a lopdf::Object,
) -> Option<&'a lopdf::Dictionary> {
    match obj {
        lopdf::Object::Reference(obj_id) => pdf.get_object(*obj_id).ok()?.as_dict().ok(),
        lopdf::Object::Dictionary(d) => Some(d),
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

/// 沿父 Pages 节点向上解析继承的 MediaBox 或 CropBox。
/// 当页面自身未直接携带 MediaBox 时用于获取页面尺寸。
/// 返回 [x1, y1, x2, y2] 坐标数组。
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
        // 优先检查 MediaBox
        if let Ok(mediabox) = dict.get(b"MediaBox") {
            if let Ok(arr) = mediabox.as_array() {
                if arr.len() >= 4 {
                    // 四个坐标全部成功解析且为有限数时才返回，任一异常返回 None，
                    // 由 detector 路由到 GeometryUncertain 保守进入 OCR
                    let x1 = object_to_f64(&arr[0]);
                    let y1 = object_to_f64(&arr[1]);
                    let x2 = object_to_f64(&arr[2]);
                    let y2 = object_to_f64(&arr[3]);
                    if let (Some(x1), Some(y1), Some(x2), Some(y2)) = (x1, y1, x2, y2) {
                        if x1.is_finite() && y1.is_finite() && x2.is_finite() && y2.is_finite() {
                            return Some([x1, y1, x2, y2]);
                        }
                    }
                    // 坐标存在但无法解析为有效数字，不再用默认值替代，
                    // 返回 None 让 detector 将此页归入 uncertain
                    return None;
                }
            }
        }
        // 回退到 CropBox
        if let Ok(cropbox) = dict.get(b"CropBox") {
            if let Ok(arr) = cropbox.as_array() {
                if arr.len() >= 4 {
                    let x1 = object_to_f64(&arr[0]);
                    let y1 = object_to_f64(&arr[1]);
                    let x2 = object_to_f64(&arr[2]);
                    let y2 = object_to_f64(&arr[3]);
                    if let (Some(x1), Some(y1), Some(x2), Some(y2)) = (x1, y1, x2, y2) {
                        if x1.is_finite() && y1.is_finite() && x2.is_finite() && y2.is_finite() {
                            return Some([x1, y1, x2, y2]);
                        }
                    }
                    return None;
                }
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

/// 检测 PDF 页面 content stream 中的 inline image（BI/ID/EI 操作符）
///
/// XObject 只能检测命名的 Image 资源，inline image 通过 BI/ID/EI
/// 操作符直接嵌入在 content stream 中，需要解析 stream 才能发现。
fn detect_inline_images(pdf: &lopdf::Document, page_obj_id: lopdf::ObjectId) -> usize {
    let page_obj = match pdf.get_object(page_obj_id) {
        Ok(obj) => obj,
        // 解析失败时返回最大值，触发不确定页标记（保守策略）
        Err(_) => return usize::MAX / 2,
    };
    let page_dict = match page_obj.as_dict() {
        Ok(d) => d,
        // 解析失败时返回最大值
        Err(_) => return usize::MAX / 2,
    };

    let contents = match page_dict.get(b"Contents") {
        Ok(c) => c,
        // 无 Contents 不一定没有 inline image，但无法检测，保守返回 0
        Err(_) => return 0,
    };

    let collected = collect_content_stream(pdf, contents);
    if collected.had_failure {
        // 任一内容流不可读取时都无法可靠排除 inline image。
        return usize::MAX / 2;
    }

    if collected.data.is_empty() {
        return 0;
    }

    match lopdf::content::Content::decode(&collected.data) {
        Ok(content) => content
            .operations
            .iter()
            .filter(|operation| operation.operator == "BI")
            .count(),
        // An inline image stream whose binary payload cannot be decoded is
        // intentionally classified as uncertain and routed through OCR.
        Err(_) => usize::MAX / 2,
    }
}

/// 汇总页面 content stream 数据（处理单个 stream 或 stream 数组）
/// 保留任一成员解析/解压失败的状态，避免部分成功被误当成完整页面内容。
struct CollectedContentStream {
    data: Vec<u8>,
    had_failure: bool,
}

fn collect_content_stream(
    pdf: &lopdf::Document,
    contents: &lopdf::Object,
) -> CollectedContentStream {
    match contents {
        lopdf::Object::Stream(stream) => match stream.decompressed_content() {
            Ok(data) => CollectedContentStream {
                data,
                had_failure: false,
            },
            Err(_) => CollectedContentStream {
                data: vec![],
                had_failure: true,
            },
        },
        lopdf::Object::Reference(id) => match pdf.get_object(*id) {
            Ok(lopdf::Object::Stream(s)) => match s.decompressed_content() {
                Ok(data) => CollectedContentStream {
                    data,
                    had_failure: false,
                },
                Err(_) => CollectedContentStream {
                    data: vec![],
                    had_failure: true,
                },
            },
            _ => CollectedContentStream {
                data: vec![],
                had_failure: true,
            },
        },
        lopdf::Object::Array(arr) => {
            let mut data = Vec::new();
            let mut had_failure = false;
            for obj in arr {
                let stream_data = match obj {
                    lopdf::Object::Reference(id) => match pdf.get_object(*id) {
                        Ok(lopdf::Object::Stream(s)) => match s.decompressed_content() {
                            Ok(data) => data,
                            Err(_) => {
                                had_failure = true;
                                continue;
                            }
                        },
                        _ => {
                            had_failure = true;
                            continue;
                        }
                    },
                    lopdf::Object::Stream(s) => match s.decompressed_content() {
                        Ok(data) => data,
                        Err(_) => {
                            had_failure = true;
                            continue;
                        }
                    },
                    _ => {
                        had_failure = true;
                        continue;
                    }
                };
                if !stream_data.is_empty() {
                    if !data.is_empty() {
                        data.push(b' ');
                    }
                    data.extend_from_slice(&stream_data);
                }
            }
            CollectedContentStream { data, had_failure }
        }
        _ => CollectedContentStream {
            data: vec![],
            had_failure: true,
        },
    }
}

/// 页面分析扩展结果
struct PageAnalysisResult {
    image_count: usize,
    image_area_ratio: f64,
    has_vector_objects: bool,
    /// 独立于 has_vector_objects 的批注不确定标志，
    /// 不受 Resources 解析流程重置影响，最终合并到 has_vector_objects
    has_annotations: bool,
    page_width: Option<u32>,
    page_height: Option<u32>,
    content_parse_failed: bool,
    has_vector_drawing_ops: bool,
    has_invisible_text: bool,
    font_encoding_suspected: bool,
}

/// 分析 PDF 页面中的对象，检测图片、矢量对象、不可见文本、字体编码异常
///
/// `page_obj_id` 为 get_pages() 返回的真实 ObjectId，不可用页码构造
fn analyze_page_objects(pdf: &lopdf::Document, page_obj_id: lopdf::ObjectId) -> PageAnalysisResult {
    let mut image_count = 0usize;
    let mut total_image_area: f64 = 0.0;
    // 保守策略：默认 has_vector_objects=true，无法确认时加入 uncertain_pages
    let mut has_vector_objects = true;
    // 独立于 has_vector_objects 的批注标志，不受后续 Resources 重置影响
    let mut has_annotations = false;
    let mut page_area: f64 = 1.0; // 默认 1.0 避免除零
    let mut resources_resolved = false;
    let mut page_width: Option<u32> = None;
    let mut page_height: Option<u32> = None;
    let mut content_parse_failed = false;
    let mut has_vector_drawing_ops = false;
    let mut has_invisible_text = false;
    let font_encoding_suspected = false;

    // 获取页面尺寸：先尝试页面自身的 MediaBox，缺失时沿父 Pages 节点继承
    if let Ok(page_obj) = pdf.get_object(page_obj_id) {
        if let Ok(page_dict) = page_obj.as_dict() {
            // 尝试直接 MediaBox
            let mut mediabox_found = false;
            if let Ok(mediabox) = page_dict.get(b"MediaBox") {
                if let Ok(arr) = mediabox.as_array() {
                    if arr.len() >= 4 {
                        // 四个坐标全部成功解析且为有限数时才设置尺寸，
                        // 任一异常保留 None，由 detector 路由到 GeometryUncertain
                        let x1 = object_to_f64(&arr[0]);
                        let y1 = object_to_f64(&arr[1]);
                        let x2 = object_to_f64(&arr[2]);
                        let y2 = object_to_f64(&arr[3]);
                        if let (Some(x1), Some(y1), Some(x2), Some(y2)) = (x1, y1, x2, y2) {
                            if x1.is_finite() && y1.is_finite() && x2.is_finite() && y2.is_finite()
                            {
                                let w = x2 - x1;
                                let h = y2 - y1;
                                page_area = (w * h).max(1.0);
                                page_width = Some(w as u32);
                                page_height = Some(h as u32);
                                mediabox_found = true;
                            }
                        }
                    }
                }
            }
            // 页面自身无 MediaBox 时继承父节点
            if !mediabox_found {
                if let Some([x1, y1, x2, y2]) = get_inherited_media_box(pdf, page_obj_id) {
                    let w = (x2 - x1).max(0.0);
                    let h = (y2 - y1).max(0.0);
                    page_area = (w * h).max(1.0);
                    page_width = Some(w as u32);
                    page_height = Some(h as u32);
                }
            }

            // 检查页面批注（/Annots）：印章、签名图、表单控件等可见内容
            // 可能包含 appearance stream (/AP)，未参与文本层但应在 OCR 判定中考虑
            if let Ok(annots_obj) = page_dict.get(b"Annots") {
                // 先尝试直接数组，再尝试间接引用
                let annots_arr: Option<&Vec<lopdf::Object>> = if let Ok(arr) = annots_obj.as_array()
                {
                    Some(arr)
                } else if let lopdf::Object::Reference(ref_id) = annots_obj {
                    pdf.get_object(*ref_id).ok().and_then(|o| o.as_array().ok())
                } else {
                    None
                };
                if let Some(arr) = annots_arr {
                    if !arr.is_empty() {
                        // 存在未解析的可见批注，使用独立标志避免被 Resources 重置覆盖
                        has_annotations = true;
                    }
                }
            }
        }
    }

    // 遍历页面资源中的 XObject，检测图片
    // 使用 get_inherited_resources 解析间接引用并沿父 Pages 节点继承资源
    if let Some(res_dict) = get_inherited_resources(pdf, page_obj_id) {
        resources_resolved = true;
        // 成功获取 Resources 后重置 has_vector_objects，仅在实际检测到矢量对象时设为 true
        has_vector_objects = false;

        if let Ok(xobjects) = res_dict.get(b"XObject") {
            if let Some(xobj_dict) = resolve_as_dict(pdf, xobjects) {
                for (_name, obj) in xobj_dict.iter() {
                    let Some(xobj) = resolve_xobject(pdf, obj) else {
                        // 无法解析的 XObject 保守标记为不确定
                        has_vector_objects = true;
                        continue;
                    };
                    if let Ok(xobj_dict) = xobj.as_dict() {
                        // 检查 Subtype
                        if let Ok(subtype) = xobj_dict.get(b"Subtype") {
                            match subtype {
                                lopdf::Object::Name(name) if name == b"Image" => {
                                    image_count += 1;
                                    // 估算图片面积
                                    let img_w = xobj_dict
                                        .get(b"Width")
                                        .ok()
                                        .and_then(object_to_f64)
                                        .unwrap_or(100.0);
                                    let img_h = xobj_dict
                                        .get(b"Height")
                                        .ok()
                                        .and_then(object_to_f64)
                                        .unwrap_or(100.0);
                                    total_image_area += img_w * img_h;
                                }
                                lopdf::Object::Name(name) if name == b"Form" => {
                                    // Form XObject 可能包含矢量图形
                                    has_vector_objects = true;
                                }
                                // 未知 Subtype，保守标记为不确定
                                _ => {
                                    has_vector_objects = true;
                                }
                            }
                        } else {
                            // XObject 无 Subtype 字段，保守标记为不确定
                            has_vector_objects = true;
                        }
                    } else {
                        // XObject 对象本身无法解析为字典，保守标记为不确定
                        has_vector_objects = true;
                    }
                }
            } else {
                // /XObject 无法解析为字典，保守标记为不确定
                has_vector_objects = true;
            }
        }

        // 检查是否有 Graphics State（可能指示复杂矢量图形）
        if res_dict.get(b"ExtGState").is_ok() {
            // 有扩展图形状态，可能是矢量对象
            has_vector_objects = true;
        }
    }

    // 检测 content stream 中的 inline image (BI 操作符)
    // XObject 检测无法覆盖 inline image，需要解析 content stream
    let inline_image_count = detect_inline_images(pdf, page_obj_id);
    // 保守值（usize::MAX / 2）表示解析失败，标记为不确定
    if inline_image_count > 0 && inline_image_count < usize::MAX / 4 {
        image_count += inline_image_count;
        // 无法精确估算 inline image 面积，保守按每张占页面 50% 估算
        total_image_area += page_area * 0.5 * inline_image_count as f64;
    } else if inline_image_count >= usize::MAX / 4 {
        // content stream 解析失败，保守标记为不确定
        has_vector_objects = true;
    }

    // 如果无法获取 Resources 字典，保守标记为不确定
    if !resources_resolved {
        has_vector_objects = true;
    }

    // 将独立保存的批注不确定状态合并回 has_vector_objects
    // 必须在 Resources 处理之后合并，避免被第 626 行重置覆盖
    if has_annotations {
        has_vector_objects = true;
    }

    // 计算图片面积占比（图片面积之和 / 页面面积）
    // 由于图片坐标可能未归一化，使用简单估算
    let image_area_ratio = if page_area > 0.0 {
        (total_image_area / page_area).min(1.0)
    } else {
        0.0
    };

    // 分析 content stream：检测绘图操作符、不可见文本、字体编码异常
    if let Ok(page_obj) = pdf.get_object(page_obj_id) {
        if let Ok(page_dict) = page_obj.as_dict() {
            if let Ok(contents) = page_dict.get(b"Contents") {
                let collected = collect_content_stream(pdf, contents);
                if collected.had_failure {
                    content_parse_failed = true;
                }
                if !collected.data.is_empty() {
                    analyze_content_stream(
                        &collected.data,
                        &mut has_vector_drawing_ops,
                        &mut has_invisible_text,
                        &mut content_parse_failed,
                    );
                }
            }
        }
    } else {
        content_parse_failed = true;
    }

    // 简单字体编码异常检测：如果页面对象存在文本操作符但 extract_text 得到的字符极少或全是乱码
    // 在调用方结合 text 长度与 content stream 文本操作符密度对比来判断
    // 此处仅设置 content_parse_failed 供上层使用

    PageAnalysisResult {
        image_count,
        image_area_ratio,
        has_vector_objects,
        has_annotations,
        page_width,
        page_height,
        content_parse_failed,
        has_vector_drawing_ops,
        has_invisible_text,
        font_encoding_suspected,
    }
}

/// 简易乱码检测：如果文本中非 ASCII 字符占比异常高且多为控制字符/私有区字符，
/// 则判定提取的文本可能是乱码（字体编码异常）
fn is_text_garbled(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let total = text.chars().count();
    if total == 0 {
        return false;
    }
    // 统计可疑字符（私有区 U+E000-U+F8FF、控制字符、替换字符）
    let suspicious = text
        .chars()
        .filter(|c| {
            (&'\u{E000}'..=&'\u{F8FF}').contains(&c) // 私有使用区
                || c == &'\u{FFFD}' // 替换字符
                || (c.is_control() && *c != '\n' && *c != '\r' && *c != '\t')
        })
        .count();
    // 可疑字符占比超过 30% 视为乱码
    suspicious as f64 / total as f64 > 0.3
}

/// 分析 content stream 检测绘图操作符和不可见文本
///
/// 检测内容：
/// - 矢量绘图操作符：m, l, c, re, S, f, B（路径构造和填充/描边）
/// - 不可见文本：文本渲染模式 Tr=3（不可见但可搜索）
///
/// 使用 lopdf 内容流解析而非固定子串匹配，
/// 以正确处理合法 PDF token 格式，且不会扫描文字字符串内部。
/// 解析失败时标记为不确定，保守触发 OCR。
/// 分析 content stream 检测不可见文本、路径绘制文字风险和解析错误。
///
/// 保守策略：只要内容流执行了可见路径填充或描边，就无法仅凭语法证明
/// 该绘图不是路径文字，因此将页面纳入不确定范围。
fn analyze_content_stream(
    data: &[u8],
    has_vector_drawing_ops: &mut bool,
    has_invisible_text: &mut bool,
    content_parse_failed: &mut bool,
) {
    let content = match lopdf::content::Content::decode(data) {
        Ok(content) => content,
        Err(_) => {
            *content_parse_failed = true;
            return;
        }
    };

    // 路径绘制操作符：S/s(描边), f/F(填充), B/b(填充+描边)
    const PATH_PAINT_OPS: &[&str] = &["S", "s", "f", "F", "f*", "B", "B*", "b", "b*"];

    for operation in &content.operations {
        let op = operation.operator.as_str();
        if PATH_PAINT_OPS.contains(&op) {
            *has_vector_drawing_ops = true;
        }
        if op == "Tr" {
            match operation.operands.first().and_then(object_to_f64) {
                Some(mode) if (mode - 3.0).abs() < f64::EPSILON => {
                    *has_invisible_text = true;
                }
                Some(_) => {}
                None => {
                    *content_parse_failed = true;
                }
            }
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
}
