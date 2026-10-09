//! OCR 文本合并 — 将文本层与 OCR 版面块按页级合并
//!
//! 核心原则：
//! 1. 页面是否 OCR 与如何合并要分离
//! 2. OCR 过的页优先使用 layout_details 阅读顺序
//! 3. 文本层只作为补充来源
//! 4. 表格、公式、图片 block 保留结构

use crate::kb::ocr_detector::PdfPageAnalysis;
use crate::kb::ocr_provider::{OcrBlockLabel, OcrPageResult};

/// 合并后的页级结果
#[derive(Debug, Clone)]
pub struct MergedPageResult {
    /// 页码
    pub page_number: i32,
    /// 合并后的纯文本
    pub text: String,
    /// 合并后的 markdown
    pub markdown: String,
    /// 是否使用了 OCR
    pub used_ocr: bool,
    /// 文本层是否作为补充
    pub native_supplemented: bool,
}

/// 合并文本层与 OCR 结果
///
/// 对每页按策略合并：
/// - 未 OCR 页：使用文本层
/// - 已 OCR 且有 layout_details：按 block 阅读顺序，文本层补充
/// - 已 OCR 仅有 md_results：使用 md_results，文本层补充
/// - 已 OCR 但结果为空：保留文本层，标记 empty_ocr
pub fn merge_text_and_ocr(
    page_analyses: &[PdfPageAnalysis],
    ocr_results: &[OcrPageResult],
    ocr_page_numbers: &[i32],
) -> Vec<MergedPageResult> {
    let ocr_set: std::collections::HashSet<i32> = ocr_page_numbers.iter().copied().collect();

    // 建立 page_number -> OcrPageResult 的映射
    let mut ocr_by_page: std::collections::HashMap<i32, &OcrPageResult> =
        std::collections::HashMap::new();
    for ocr in ocr_results {
        ocr_by_page.insert(ocr.page_number, ocr);
    }

    let mut results = Vec::with_capacity(page_analyses.len());

    for page in page_analyses {
        let page_num = page.page_number;

        if !ocr_set.contains(&page_num) {
            // 未 OCR 页：直接使用文本层
            results.push(MergedPageResult {
                page_number: page_num,
                text: page.text.clone(),
                markdown: page.text.clone(),
                used_ocr: false,
                native_supplemented: false,
            });
            continue;
        }

        // 已 OCR 页
        let ocr_page = ocr_by_page.get(&page_num);

        match ocr_page {
            None => {
                // OCR 结果中无此页（可能是 failed），保留文本层
                results.push(MergedPageResult {
                    page_number: page_num,
                    text: page.text.clone(),
                    markdown: page.text.clone(),
                    used_ocr: false,
                    native_supplemented: false,
                });
            }
            Some(ocr) => {
                let ocr_text_norm = normalize_text(&ocr.text);
                let native_text_norm = normalize_text(&page.text);

                if ocr_text_norm.is_empty() && !ocr.markdown.is_empty() {
                    // 有 markdown 但 text 为空，用 markdown 生成 text
                    let text = strip_markdown_simple(&ocr.markdown);
                    let native_supp = should_supplement_native(&text, &page.text);
                    let final_text = if native_supp {
                        format!("{}\n\n{}", text, find_uncovered_native(&text, &page.text))
                    } else {
                        text
                    };
                    results.push(MergedPageResult {
                        page_number: page_num,
                        text: final_text,
                        markdown: ocr.markdown.clone(),
                        used_ocr: true,
                        native_supplemented: native_supp,
                    });
                } else if ocr_text_norm.is_empty() && native_text_norm.is_empty() {
                    // OCR 和文本层都为空
                    results.push(MergedPageResult {
                        page_number: page_num,
                        text: String::new(),
                        markdown: String::new(),
                        used_ocr: true,
                        native_supplemented: false,
                    });
                } else if ocr_text_norm.is_empty() {
                    // OCR 为空但有文本层
                    results.push(MergedPageResult {
                        page_number: page_num,
                        text: page.text.clone(),
                        markdown: page.text.clone(),
                        used_ocr: true,
                        native_supplemented: false,
                    });
                } else {
                    // OCR 有内容
                    let native_supp = should_supplement_native(&ocr.text, &page.text);
                    let final_text = if native_supp {
                        format!(
                            "{}\n\n{}",
                            ocr.text,
                            find_uncovered_native(&ocr.text, &page.text)
                        )
                    } else {
                        ocr.text.clone()
                    };

                    // 生成 markdown：优先用 OCR markdown，再用 block 生成
                    let markdown = if !ocr.markdown.is_empty() {
                        ocr.markdown.clone()
                    } else {
                        blocks_to_markdown(&ocr.blocks)
                    };

                    results.push(MergedPageResult {
                        page_number: page_num,
                        text: final_text,
                        markdown,
                        used_ocr: true,
                        native_supplemented: native_supp,
                    });
                }
            }
        }
    }

    results
}

/// 生成合并后的全文内容，按页码标记
pub fn merged_results_to_content(results: &[MergedPageResult]) -> String {
    let mut parts = Vec::new();
    for result in results {
        if !result.text.trim().is_empty() {
            parts.push(format!("[PAGE:{}]\n{}", result.page_number, result.text));
        }
    }
    parts.join("\n\n")
}

/// 判断文本层是否需要作为补充
fn should_supplement_native(ocr_text: &str, native_text: &str) -> bool {
    let native_norm = normalize_text(native_text);
    if native_norm.is_empty() {
        return false;
    }
    let ocr_norm = normalize_text(ocr_text);
    let similarity = text_similarity(&ocr_norm, &native_norm);
    similarity < 0.90
}

/// 查找文本层中未被 OCR 覆盖的片段
fn find_uncovered_native(ocr_text: &str, native_text: &str) -> String {
    let ocr_norm = normalize_text(ocr_text);
    let native_norm = normalize_text(native_text);

    if native_norm.is_empty() {
        return String::new();
    }

    let mut uncovered: Vec<String> = native_text
        .lines()
        .map(|l| l.trim())
        .filter(|l| {
            let line_norm = normalize_text(l);
            !line_norm.is_empty() && !ocr_norm.contains(&line_norm)
        })
        .map(|l| l.to_string())
        .collect();

    if uncovered.is_empty() && !ocr_norm.contains(&native_norm) {
        uncovered.push(native_text.trim().to_string());
    }

    if uncovered.is_empty() {
        String::new()
    } else {
        format!("[native_supplement]\n{}", uncovered.join("\n"))
    }
}

/// 文本相似度计算（基于 trigram 的 Jaccard 系数）
///
/// 使用 trigram（连续 3 字符子串）而非单字符集合，
/// 避免 "hello world" 和 "world hello" 误判为完全相同。
fn text_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let a_trigrams: std::collections::HashSet<String> = trigrams(a);
    let b_trigrams: std::collections::HashSet<String> = trigrams(b);

    let intersection = a_trigrams.intersection(&b_trigrams).count() as f64;
    let union = a_trigrams.union(&b_trigrams).count() as f64;

    if union == 0.0 {
        return 0.0;
    }
    intersection / union
}

/// 生成文本的 trigram 集合
fn trigrams(text: &str) -> std::collections::HashSet<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut set = std::collections::HashSet::new();
    if chars.len() < 3 {
        // 短文本用整体作为唯一元素
        set.insert(chars.into_iter().collect());
    } else {
        for window in chars.windows(3) {
            set.insert(window.iter().collect());
        }
    }
    set
}

/// 标准化文本用于比较
fn normalize_text(text: &str) -> String {
    text.trim()
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// 简单去掉 markdown 格式
fn strip_markdown_simple(md: &str) -> String {
    md.lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                trimmed.trim_start_matches('#').trim().to_string()
            } else if trimmed.starts_with('|') {
                // 表格行保留
                trimmed.to_string()
            } else {
                trimmed.to_string()
            }
        })
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 从版面块生成 markdown
fn blocks_to_markdown(blocks: &[crate::kb::ocr_provider::OcrLayoutBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match &block.label {
            OcrBlockLabel::Text => {
                if !block.content.is_empty() {
                    parts.push(block.content.clone());
                }
            }
            OcrBlockLabel::Table => {
                if !block.content.is_empty() {
                    parts.push(format!("[TABLE]\n{}", block.content));
                }
            }
            OcrBlockLabel::Formula => {
                if !block.content.is_empty() {
                    parts.push(format!("[FORMULA]\n{}", block.content));
                }
            }
            // 图片 block 仅保留在 blocks/metadata/UI，不参与 markdown
            OcrBlockLabel::Image => {}
            OcrBlockLabel::Unknown(_) => {
                if !block.content.is_empty() {
                    parts.push(block.content.clone());
                }
            }
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_analysis(page: i32, text: &str) -> PdfPageAnalysis {
        PdfPageAnalysis {
            page_number: page,
            text: text.to_string(),
            text_blocks: vec![],
            char_count: text.chars().count(),
            image_regions: vec![],
            image_area_ratio: 0.0,
            has_vector_or_unknown_objects: false,
            width: None,
            height: None,
            content_parse_failed: false,
            has_vector_drawing_ops: false,
            has_invisible_text: false,
            font_encoding_suspected: false,
        }
    }

    fn make_ocr_result(page: i32, text: &str, markdown: &str) -> OcrPageResult {
        OcrPageResult {
            page_number: page,
            text: text.to_string(),
            markdown: markdown.to_string(),
            blocks: vec![],
            layout_visualization_url: None,
            raw_response_json: serde_json::json!({}),
            request_id: None,
            confidence: None,
            provider: "glm_ocr".to_string(),
            model: "glm-ocr".to_string(),
            ocr_page_width: None,
            ocr_page_height: None,
        }
    }

    #[test]
    fn test_merge_no_ocr_pages() {
        let analyses = vec![
            make_analysis(1, "文本层内容"),
            make_analysis(2, "第二页文本"),
        ];
        let results = merge_text_and_ocr(&analyses, &[], &[]);
        assert_eq!(results.len(), 2);
        assert!(!results[0].used_ocr);
        assert_eq!(results[0].text, "文本层内容");
    }

    #[test]
    fn test_merge_scanned_page() {
        let analyses = vec![make_analysis(1, "")];
        let ocr = vec![make_ocr_result(1, "OCR 识别文本", "# OCR 识别文本")];
        let results = merge_text_and_ocr(&analyses, &ocr, &[1]);
        assert_eq!(results.len(), 1);
        assert!(results[0].used_ocr);
        assert!(results[0].text.contains("OCR 识别文本"));
    }

    #[test]
    fn test_merge_empty_ocr_with_native() {
        let analyses = vec![make_analysis(1, "原生文本层")];
        let ocr = vec![make_ocr_result(1, "", "")];
        let results = merge_text_and_ocr(&analyses, &ocr, &[1]);
        assert!(results[0].used_ocr);
        assert_eq!(results[0].text, "原生文本层");
    }

    #[test]
    fn test_native_supplement_preserves_original_spacing() {
        let analyses = vec![make_analysis(1, "Invoice total: USD 1 234\nCode: A B C")];
        let ocr = vec![make_ocr_result(1, "发票扫描文本", "发票扫描文本")];
        let results = merge_text_and_ocr(&analyses, &ocr, &[1]);
        assert!(results[0].native_supplemented);
        assert!(results[0].text.contains("Invoice total: USD 1 234"));
        assert!(results[0].text.contains("Code: A B C"));
        assert!(!results[0].text.contains("Invoicetotal:USD1234"));
    }

    #[test]
    fn test_merged_results_to_content() {
        let results = vec![
            MergedPageResult {
                page_number: 1,
                text: "第一页".to_string(),
                markdown: "第一页".to_string(),
                used_ocr: false,
                native_supplemented: false,
            },
            MergedPageResult {
                page_number: 2,
                text: "第二页".to_string(),
                markdown: "第二页".to_string(),
                used_ocr: true,
                native_supplemented: false,
            },
        ];
        let content = merged_results_to_content(&results);
        assert!(content.contains("[PAGE:1]"));
        assert!(content.contains("[PAGE:2]"));
        assert!(content.contains("第一页"));
        assert!(content.contains("第二页"));
    }

    #[test]
    fn test_text_similarity() {
        assert!(text_similarity("hello world", "hello world") > 0.99);
        assert!(text_similarity("hello", "world") < 0.3);
    }
}
