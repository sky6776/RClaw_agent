//! OCR 检测器 — 判断 PDF 页面是否需要 OCR
//!
//! 基于文本密度、图片覆盖率、嵌入图片数量、矢量对象等指标，
//! 输出需要 OCR 的页集合和原因。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// OCR 判定原因
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrReason {
    /// 文本密度低
    LowTextDensity,
    /// 文本层为空
    EmptyTextLayer,
    /// 图片面积占比高
    ImageArea,
    /// 嵌入图片数量多
    EmbeddedImage,
    /// 存在矢量或未知对象
    VectorOrUnknownObjects,
    /// 解析错误
    ParserError,
    /// 字体编码问题
    FontEncodingIssue,
    /// 隐藏或不可见文本层
    HiddenOrInvisibleTextLayer,
    /// 强制全页 OCR
    ForcedAllPages,
    /// 页面几何信息缺失或无效（尺寸无法确认，保守进入 OCR）
    GeometryUncertain,
}

/// OCR 判定结果
#[derive(Debug, Clone)]
pub struct OcrDetection {
    /// 总页数
    pub total_pages: usize,
    /// 低文本密度页
    pub low_density_pages: Vec<i32>,
    /// 图片丰富页
    pub image_rich_pages: Vec<i32>,
    /// 不确定页（需要 OCR 以确认）
    pub uncertain_pages: Vec<i32>,
    /// 实际需要 OCR 的页
    pub ocr_pages: Vec<i32>,
    /// 是否需要 OCR
    pub needs_ocr: bool,
    /// 低密度页占比
    pub low_density_ratio: f64,
    /// 每页触发 OCR 的原因
    pub reasons_by_page: BTreeMap<i32, Vec<OcrReason>>,
}

/// OCR 范围
#[derive(Debug, Clone, PartialEq)]
pub enum OcrScope {
    /// 无需 OCR
    None,
    /// 部分页需要 OCR
    Partial,
    /// 全部页需要 OCR
    Full,
}

impl OcrDetection {
    /// 获取 OCR 范围
    pub fn scope(&self) -> OcrScope {
        if self.ocr_pages.is_empty() {
            OcrScope::None
        } else if self.ocr_pages.len() == self.total_pages {
            OcrScope::Full
        } else {
            OcrScope::Partial
        }
    }
}

/// 页内文本块（预留，lopdf 当前无法可靠获取文本块坐标时为空 Vec）
#[derive(Debug, Clone)]
pub struct PdfTextBlock {
    pub page_number: i32,
    pub text: String,
    pub bbox: Option<[f64; 4]>,
    pub source_start: usize,
    pub source_end: usize,
}

/// 页级分析结果（由 PdfParser 输出）
#[derive(Debug, Clone)]
pub struct PdfPageAnalysis {
    /// 页码（1-based）
    pub page_number: i32,
    /// 提取的文本
    pub text: String,
    /// 文本块列表（当前 lopdf 无法可靠获取坐标，预留为空 Vec）
    pub text_blocks: Vec<PdfTextBlock>,
    /// 字符数
    pub char_count: usize,
    /// 图片区域列表
    pub image_regions: Vec<PdfImageRegion>,
    /// 图片面积占比
    pub image_area_ratio: f64,
    /// 是否存在矢量或未知对象
    pub has_vector_or_unknown_objects: bool,
    /// 页面宽度（来自 MediaBox/CropBox）
    pub width: Option<u32>,
    /// 页面高度（来自 MediaBox/CropBox）
    pub height: Option<u32>,
    /// 页面 content stream 解析失败（强制 OCR）
    pub content_parse_failed: bool,
    /// 存在矢量绘图操作符（m/l/c/re/S/f/B 等）
    pub has_vector_drawing_ops: bool,
    /// 存在不可见文本（Tr=3 等文本渲染模式）
    pub has_invisible_text: bool,
    /// 字体编码疑似异常（无 Unicode 映射或提取文本明显异常）
    pub font_encoding_suspected: bool,
}

/// PDF 图片区域
#[derive(Debug, Clone)]
pub struct PdfImageRegion {
    /// 归一化边界框 [x1, y1, x2, y2]
    pub bbox: Option<[f64; 4]>,
    /// 面积占比
    pub area_ratio: f64,
}

/// 执行 OCR 检测
///
/// 根据配置的阈值和每页分析结果，判断哪些页需要 OCR。
pub fn detect_ocr_pages(
    pages: &[PdfPageAnalysis],
    text_density_threshold: usize,
    image_area_threshold: f64,
    image_count_threshold: usize,
    mode: &crate::kb::ocr_provider::OcrMode,
) -> OcrDetection {
    let total_pages = pages.len();
    let mut reasons_by_page: BTreeMap<i32, Vec<OcrReason>> = BTreeMap::new();
    let mut low_density_pages = Vec::new();
    let mut image_rich_pages = Vec::new();
    let mut uncertain_pages = Vec::new();

    let is_all_pages = matches!(mode, crate::kb::ocr_provider::OcrMode::AllPages);

    for page in pages {
        let mut reasons = Vec::new();

        // 空文本层
        if page.text.trim().is_empty() {
            reasons.push(OcrReason::EmptyTextLayer);
        }

        // 低文本密度
        if page.text.trim().chars().count() < text_density_threshold {
            reasons.push(OcrReason::LowTextDensity);
            low_density_pages.push(page.page_number);
        }

        // 图片面积占比
        if page.image_area_ratio >= image_area_threshold {
            reasons.push(OcrReason::ImageArea);
            image_rich_pages.push(page.page_number);
        }

        // 嵌入图片数量
        if page.image_regions.len() >= image_count_threshold {
            reasons.push(OcrReason::EmbeddedImage);
        }

        // 页面几何信息缺失或无效：尺寸无法确认时保守进入 uncertain，
        // 满足设计要求"尺寸无法确认则保守进入识别"
        if page.width.is_none()
            || page.height.is_none()
            || page.width == Some(0)
            || page.height == Some(0)
        {
            reasons.push(OcrReason::GeometryUncertain);
            uncertain_pages.push(page.page_number);
        }

        // 矢量或未知对象
        if page.has_vector_or_unknown_objects {
            reasons.push(OcrReason::VectorOrUnknownObjects);
            uncertain_pages.push(page.page_number);
        }

        // 页面 content stream 解析失败 → 强制 OCR
        if page.content_parse_failed {
            reasons.push(OcrReason::ParserError);
            uncertain_pages.push(page.page_number);
        }

        // 不可见文本（Tr=3 等） → 文本层为假象，必须 OCR
        if page.has_invisible_text {
            reasons.push(OcrReason::HiddenOrInvisibleTextLayer);
            uncertain_pages.push(page.page_number);
        }

        // 路径绘制文字风险：content stream 中存在密集路径操作但无文本操作，
        // 无法排除页面使用轮廓路径绘制文字的可能，纳入不确定范围
        if page.has_vector_drawing_ops {
            reasons.push(OcrReason::VectorOrUnknownObjects);
            uncertain_pages.push(page.page_number);
        }

        // 字体编码异常 → 提取文本不可靠
        if page.font_encoding_suspected {
            reasons.push(OcrReason::FontEncodingIssue);
            uncertain_pages.push(page.page_number);
        }

        // 强制全页模式
        if is_all_pages {
            reasons.push(OcrReason::ForcedAllPages);
        }

        if !reasons.is_empty() {
            reasons_by_page.insert(page.page_number, reasons);
        }
    }

    let low_density_ratio = if total_pages > 0 {
        low_density_pages.len() as f64 / total_pages as f64
    } else {
        0.0
    };

    // P1 修复：ratio 仅作统计信息，不再否决单页 OCR 判定。
    // 之前的逻辑是 low_density_ratio < min_low_density_ratio 时移除仅有 LowTextDensity
    // 原因的页，但这导致混合 PDF 中少数真正需要 OCR 的低密度页被跳过。
    // ratio 更适合作为整体策略提示（如日志/仪表盘），而非否决单页判定的依据。
    // 单页是否需要 OCR 应由该页自身的特征（文本密度、图片、矢量对象）决定。

    let ocr_pages: Vec<i32> = reasons_by_page.keys().copied().collect();
    let needs_ocr = !ocr_pages.is_empty();

    OcrDetection {
        total_pages,
        low_density_pages,
        image_rich_pages,
        uncertain_pages,
        ocr_pages,
        needs_ocr,
        low_density_ratio,
        reasons_by_page,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_page(
        page_number: i32,
        text: &str,
        image_ratio: f64,
        image_count: usize,
        has_vec: bool,
    ) -> PdfPageAnalysis {
        let regions: Vec<PdfImageRegion> = (0..image_count)
            .map(|_| PdfImageRegion {
                bbox: Some([0.0, 0.0, 0.5, 0.5]),
                area_ratio: image_ratio,
            })
            .collect();
        PdfPageAnalysis {
            page_number,
            text: text.to_string(),
            text_blocks: vec![],
            char_count: text.chars().count(),
            image_regions: regions,
            image_area_ratio: image_ratio,
            has_vector_or_unknown_objects: has_vec,
            width: Some(612), // 默认 Letter 尺寸
            height: Some(792),
            content_parse_failed: false,
            has_vector_drawing_ops: false,
            has_invisible_text: false,
            font_encoding_suspected: false,
        }
    }

    #[test]
    fn test_detect_pure_text_pdf() {
        // 文本需 >= 50 字符才不被视为低密度
        let long_text = "这是一段足够长的文本内容用于测试纯文本".repeat(3); // 19*3=57 chars
        let pages = vec![
            make_page(1, &long_text, 0.0, 0, false),
            make_page(2, &long_text, 0.0, 0, false),
        ];
        let result = detect_ocr_pages(&pages, 50, 0.08, 1, &crate::kb::ocr_provider::OcrMode::Auto);
        assert!(!result.needs_ocr);
        assert_eq!(result.scope(), OcrScope::None);
        assert!(result.ocr_pages.is_empty());
    }

    #[test]
    fn test_detect_scanned_page() {
        let pages = vec![
            make_page(1, "", 0.92, 1, false),
            make_page(2, "", 0.85, 1, false),
        ];
        let result = detect_ocr_pages(&pages, 50, 0.08, 1, &crate::kb::ocr_provider::OcrMode::Auto);
        assert!(result.needs_ocr);
        assert_eq!(result.ocr_pages, vec![1, 2]);
        assert_eq!(result.scope(), OcrScope::Full);
    }

    #[test]
    fn test_detect_mixed_pdf() {
        let long_text =
            "这页文字很充足而且没有图片，文本字符数量超过五十个字符的最低阈值要求".repeat(2); // 34*2=68
        let pages = vec![
            make_page(1, &long_text, 0.0, 0, false),
            make_page(2, "", 0.9, 1, false),
            make_page(3, "短", 0.0, 0, false),
        ];
        let result = detect_ocr_pages(&pages, 50, 0.08, 1, &crate::kb::ocr_provider::OcrMode::Auto);
        assert!(result.needs_ocr);
        assert_eq!(result.ocr_pages, vec![2, 3]);
        assert_eq!(result.scope(), OcrScope::Partial);
    }

    #[test]
    fn test_detect_image_rich_page_with_text() {
        // 文本充足但图片占比高
        let pages = vec![make_page(1, &"文本内容".repeat(20), 0.5, 2, false)];
        let result = detect_ocr_pages(&pages, 50, 0.08, 1, &crate::kb::ocr_provider::OcrMode::Auto);
        assert!(result.needs_ocr);
        assert!(result.reasons_by_page[&1]
            .iter()
            .any(|r| matches!(r, OcrReason::ImageArea)));
        assert!(result.reasons_by_page[&1]
            .iter()
            .any(|r| matches!(r, OcrReason::EmbeddedImage)));
    }

    #[test]
    fn test_detect_all_pages_mode() {
        let pages = vec![make_page(
            1,
            "文字足够长的页面内容用于测试检测功能",
            0.0,
            0,
            false,
        )];
        let result = detect_ocr_pages(
            &pages,
            50,
            0.08,
            1,
            &crate::kb::ocr_provider::OcrMode::AllPages,
        );
        assert!(result.needs_ocr);
        assert!(result.reasons_by_page[&1]
            .iter()
            .any(|r| matches!(r, OcrReason::ForcedAllPages)));
    }

    #[test]
    fn test_detect_vector_objects() {
        let pages = vec![make_page(1, "有文字但有矢量对象", 0.0, 0, true)];
        let result = detect_ocr_pages(&pages, 50, 0.08, 1, &crate::kb::ocr_provider::OcrMode::Auto);
        assert!(result.needs_ocr);
        assert!(result.uncertain_pages.contains(&1));
    }

    /// P1 修复：低密度页比例低于阈值但页有其他原因时，仍应触发 OCR
    #[test]
    fn test_low_density_ratio_below_threshold_but_has_other_reasons() {
        // 3 页 PDF：2 页文本充足 + 1 页低密度但有图片
        // 低密度比例 = 1/3 ≈ 0.33，刚好低于 0.4 阈值
        // 但低密度页同时有图片（ImageArea），应保留 OCR
        let long_text = "这是一段足够长的文本内容用于测试正常页面".repeat(3); // 20*3=60
        let pages = vec![
            make_page(1, &long_text, 0.0, 0, false),
            make_page(2, &long_text, 0.0, 0, false),
            make_page(3, "短", 0.5, 1, false), // 低密度 + 图片丰富
        ];
        let result = detect_ocr_pages(&pages, 50, 0.08, 1, &crate::kb::ocr_provider::OcrMode::Auto);
        // 页 3 应该仍在 OCR 列表中（因为有 ImageArea 原因）
        assert!(result.needs_ocr);
        assert!(
            result.ocr_pages.contains(&3),
            "低密度但有图片的页应保留 OCR，实际 ocr_pages={:?}",
            result.ocr_pages
        );
    }

    /// P1 修复：ratio 不再否决单页 OCR，即使低密度比例低于阈值也应触发
    #[test]
    fn test_low_density_below_ratio_still_triggers_ocr() {
        // 5 页 PDF：4 页文本充足 + 1 页仅低密度（无图片/矢量）
        // 低密度比例 = 1/5 = 0.2，低于 0.3 阈值
        // P1 修复后：ratio 仅作统计，单页低密度仍应触发 OCR
        let long_text = "这是一段足够长的文本内容用于测试正常页面".repeat(3); // 20*3=60
        let pages = vec![
            make_page(1, &long_text, 0.0, 0, false),
            make_page(2, &long_text, 0.0, 0, false),
            make_page(3, &long_text, 0.0, 0, false),
            make_page(4, &long_text, 0.0, 0, false),
            make_page(5, "短", 0.0, 0, false), // 仅低密度，无其他原因
        ];
        let result = detect_ocr_pages(&pages, 50, 0.08, 1, &crate::kb::ocr_provider::OcrMode::Auto);
        // P1 修复：ratio 不再否决单页判定，低密度页仍需 OCR
        assert!(
            result.needs_ocr,
            "低密度页应触发 OCR（ratio 不再否决单页），实际 needs_ocr={}",
            result.needs_ocr
        );
        assert!(
            result.ocr_pages.contains(&5),
            "页 5 应在 OCR 列表中，实际 ocr_pages={:?}",
            result.ocr_pages
        );
    }
}
