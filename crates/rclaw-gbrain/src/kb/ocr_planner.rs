//! OCR 请求规划 — 根据 PDF 大小和页数规划 GLM-OCR 请求
//!
//! 规划规则：
//! 1. 合并目标页为连续页段，每段最多 max_pages_per_request 页
//! 2. 原 PDF ≤ max_bytes 且总页数 ≤ max_pages_per_request 时可直接复用原 PDF
//! 3. 超过字节限制或页数限制时需物理拆分 PDF 子文件
//! 4. 单页子文件仍超限时标记 failed

use crate::error::Result;
use crate::kb::ocr_pdf_splitter::split_pdf_for_ocr;
use crate::kb::temp_guard::TempOcrDir;

/// OCR 请求计划
#[derive(Debug, Clone)]
pub struct OcrRequestPlan {
    /// 文档 ID
    pub document_id: i64,
    /// 处理运行 ID
    pub processing_run_id: String,
    /// 请求块列表
    pub chunks: Vec<OcrRequestChunk>,
}

/// 单个 OCR 请求块
#[derive(Debug, Clone)]
pub struct OcrRequestChunk {
    /// 原始 PDF 中的起始页码（1-based）
    pub source_start_page: i32,
    /// 原始 PDF 中的结束页码（1-based）
    pub source_end_page: i32,
    /// 请求中的起始页 ID（子文件内页码或原始页码）
    pub request_start_page_id: i32,
    /// 请求中的结束页 ID
    pub request_end_page_id: i32,
    /// 拆分后的 PDF 子文件路径（None 表示使用原 PDF）
    pub split_pdf_path: Option<std::path::PathBuf>,
    /// 估算字节大小
    pub estimated_bytes: Option<usize>,
    /// 拆分是否失败。为 true 时调用方应跳过 OCR 请求并标记该页段为 failed
    pub split_failed: bool,
}

/// 生成 OCR 请求计划
///
/// 根据 PDF 文件大小、总页数、目标 OCR 页集合和配置限制，
/// 规划如何拆分和提交 OCR 请求。拆分生成的文件和预算均归
/// `temp_guard` 管理，以便任何失败路径均由目录守卫平账。
#[allow(clippy::too_many_arguments)]
pub fn plan_ocr_requests(
    document_id: i64,
    processing_run_id: &str,
    pdf_data: &[u8],
    total_pages: i32,
    ocr_pages: &[i32],
    max_pages_per_request: usize,
    max_pdf_bytes: usize,
    submit_mode: &crate::kb::ocr_provider::OcrSubmitMode,
    temp_budget_max_bytes: u64,
    temp_guard: &mut TempOcrDir,
) -> Result<OcrRequestPlan> {
    if ocr_pages.is_empty() {
        return Ok(OcrRequestPlan {
            document_id,
            processing_run_id: processing_run_id.to_string(),
            chunks: vec![],
        });
    }

    let pdf_bytes = pdf_data.len();

    // H25 修复：校验 ocr_pages 中的页码是否在合法范围 [1, total_pages] 内
    if total_pages > 0 {
        for &page in ocr_pages {
            if page < 1 || page > total_pages {
                return Err(crate::error::GBrainError::InvalidInput(format!(
                    "OCR 页码 {} 超出合法范围 [1, {}]，总页数: {}",
                    page, total_pages, total_pages
                )));
            }
        }
    } else if !ocr_pages.is_empty() {
        // total_pages <= 0 但仍指定了页码，无法校验，应拒绝
        return Err(crate::error::GBrainError::InvalidInput(format!(
            "total_pages 无效 ({}) 但 ocr_pages 非空 ({} 页)，无法校验页码范围",
            total_pages,
            ocr_pages.len()
        )));
    }

    // H25 修复：根据 submit_mode 决定是否需要物理拆分
    // - PdfFirst: 仅在超过字节限制时拆分（原逻辑）
    // - PdfRange: 强制按页段拆分为子文件，即使未超字节限制
    let needs_physical_split = match submit_mode {
        crate::kb::ocr_provider::OcrSubmitMode::PdfRange => true,
        crate::kb::ocr_provider::OcrSubmitMode::PdfFirst => pdf_bytes > max_pdf_bytes,
    };

    // 将目标页合并为连续页段
    let segments = merge_pages_to_segments(ocr_pages, max_pages_per_request);

    let mut chunks = Vec::new();

    for (seg_start, seg_end) in &segments {
        if needs_physical_split {
            // 需要物理拆分 PDF
            // 先尝试直接拆出这个页段
            match split_pdf_for_ocr(
                pdf_data,
                *seg_start,
                *seg_end,
                max_pdf_bytes,
                temp_budget_max_bytes,
                temp_guard,
            ) {
                Ok(split_results) => {
                    for split in split_results {
                        chunks.push(OcrRequestChunk {
                            source_start_page: split.source_start_page,
                            source_end_page: split.source_end_page,
                            // 子文件内页码从 1 开始
                            request_start_page_id: 1,
                            request_end_page_id: split.child_page_count,
                            split_pdf_path: Some(split.path),
                            estimated_bytes: Some(split.bytes),
                            split_failed: false,
                        });
                    }
                }
                Err(e) => {
                    // 拆分失败，标记 split_failed，调用方将跳过 OCR 并标记该页段为 failed
                    tracing::warn!(
                        document_id,
                        start = seg_start,
                        end = seg_end,
                        error = %e,
                        "PDF 页段拆分失败，该页段将标记为 failed"
                    );
                    chunks.push(OcrRequestChunk {
                        source_start_page: *seg_start,
                        source_end_page: *seg_end,
                        request_start_page_id: *seg_start,
                        request_end_page_id: *seg_end,
                        split_pdf_path: None,
                        estimated_bytes: None,
                        split_failed: true,
                    });
                }
            }
        } else {
            // 直接使用原 PDF，无需拆分
            chunks.push(OcrRequestChunk {
                source_start_page: *seg_start,
                source_end_page: *seg_end,
                // 原始页码直接作为请求页 ID
                request_start_page_id: *seg_start,
                request_end_page_id: *seg_end,
                split_pdf_path: None,
                estimated_bytes: Some(pdf_bytes),
                split_failed: false,
            });
        }
    }

    Ok(OcrRequestPlan {
        document_id,
        processing_run_id: processing_run_id.to_string(),
        chunks,
    })
}

/// 将目标页合并为连续页段，每段最多 max_pages 页
fn merge_pages_to_segments(pages: &[i32], max_pages: usize) -> Vec<(i32, i32)> {
    if pages.is_empty() {
        return vec![];
    }

    let mut sorted: Vec<i32> = pages.to_vec();
    sorted.sort();
    sorted.dedup();

    let mut segments = Vec::new();
    let mut seg_start = sorted[0];
    let mut seg_end = sorted[0];
    let mut seg_count = 1usize;

    for &page in &sorted[1..] {
        let is_continuous = page == seg_end + 1;
        let within_limit = seg_count < max_pages;

        if is_continuous && within_limit {
            seg_end = page;
            seg_count += 1;
        } else {
            // 保存当前段
            segments.push((seg_start, seg_end));
            seg_start = page;
            seg_end = page;
            seg_count = 1;
        }
    }

    // 保存最后一段
    segments.push((seg_start, seg_end));
    segments
}

/// 生成稳定的 request_id
///
/// 格式: `ocr_{document_id}_{run_hash}_{source_start}_{source_end}`
/// 长度在 6-64 字符之间。
pub fn generate_request_id(
    document_id: i64,
    run_id: &str,
    source_start: i32,
    source_end: i32,
) -> String {
    // M29 修复：安全提取 run_id 的后 8 个字符（按字符而非字节切片，避免 UTF-8 边界 panic）
    let run_hash: String = if run_id.chars().count() > 8 {
        run_id
            .chars()
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect()
    } else {
        run_id.to_string()
    };
    format!(
        "ocr_{}_{}_{}_{}",
        document_id, run_hash, source_start, source_end
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_pages_to_segments_continuous() {
        let pages = vec![1, 2, 3, 4, 5];
        let segments = merge_pages_to_segments(&pages, 100);
        assert_eq!(segments, vec![(1, 5)]);
    }

    #[test]
    fn test_merge_pages_to_segments_discontinuous() {
        let pages = vec![2, 3, 9, 10];
        let segments = merge_pages_to_segments(&pages, 100);
        assert_eq!(segments, vec![(2, 3), (9, 10)]);
    }

    #[test]
    fn test_merge_pages_to_segments_max_pages() {
        let pages = vec![1, 2, 3, 4, 5];
        let segments = merge_pages_to_segments(&pages, 2);
        assert_eq!(segments, vec![(1, 2), (3, 4), (5, 5)]);
    }

    #[test]
    fn test_small_over_100_page_pdf_reuses_source_with_ranges() {
        let pages: Vec<i32> = (1..=101).collect();
        let pdf_data = vec![0u8; 1024];
        let mut temp_guard =
            crate::kb::temp_guard::TempOcrDir::create("ocr_plan_test", 0, 1_073_741_824).unwrap();
        let plan = plan_ocr_requests(
            1,
            "run_test",
            &pdf_data,
            101,
            &pages,
            100,
            52_428_800,
            &crate::kb::ocr_provider::OcrSubmitMode::PdfFirst,
            1_073_741_824, // temp budget: 1GB for test
            &mut temp_guard,
        )
        .unwrap();

        assert_eq!(plan.chunks.len(), 2);
        assert!(plan.chunks.iter().all(|c| c.split_pdf_path.is_none()));
        assert_eq!(plan.chunks[0].request_start_page_id, 1);
        assert_eq!(plan.chunks[0].request_end_page_id, 100);
        assert_eq!(plan.chunks[1].request_start_page_id, 101);
        assert_eq!(plan.chunks[1].request_end_page_id, 101);
    }

    #[test]
    fn test_merge_pages_to_segments_sparse() {
        let pages = vec![1, 5, 10];
        let segments = merge_pages_to_segments(&pages, 100);
        assert_eq!(segments, vec![(1, 1), (5, 5), (10, 10)]);
    }

    #[test]
    fn test_merge_pages_to_segments_unsorted() {
        let pages = vec![5, 3, 1, 2];
        let segments = merge_pages_to_segments(&pages, 100);
        assert_eq!(segments, vec![(1, 3), (5, 5)]);
    }

    #[test]
    fn test_merge_pages_to_segments_dedup() {
        let pages = vec![1, 1, 2, 2, 3];
        let segments = merge_pages_to_segments(&pages, 100);
        assert_eq!(segments, vec![(1, 3)]);
    }

    #[test]
    fn test_generate_request_id() {
        let id = generate_request_id(123, "run_abc12345", 1, 5);
        // 验证 ID 结构：ocr_{doc_id}_{run_hash}_{segment}_{total}
        assert!(
            id.starts_with("ocr_123_"),
            "ID 应以 ocr_123_ 开头，实际: {}",
            id
        );
        assert!(id.ends_with("_1_5"), "ID 应以 _1_5 结尾，实际: {}", id);
        assert!(id.len() >= 6 && id.len() <= 64, "ID 长度应在 6-64 之间");
    }
}
