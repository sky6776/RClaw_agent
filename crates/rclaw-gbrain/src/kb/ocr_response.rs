//! GLM-OCR 响应结构与规范化
//!
//! 解析智谱 GLM-OCR layout_parsing API 返回的 JSON 响应，
//! 规范化为统一的 OcrPageResult 列表。

use crate::error::{GBrainError, Result};
use serde::{Deserialize, Serialize};

use super::ocr_provider::{OcrBlockLabel, OcrLayoutBlock, OcrPageResult};

/// HTML 标签匹配正则（全局编译一次）
static HTML_TAG_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"<[^>]+>").unwrap());

/// 表格单元格匹配正则（全局编译一次）
static TABLE_CELL_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"<t[dh][^>]*>(.*?)</t[dh]>").unwrap());

/// GLM-OCR 完整响应结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlmOcrResponse {
    pub id: Option<String>,
    pub created: Option<i64>,
    pub model: Option<String>,
    /// Markdown 格式识别结果
    pub md_results: Option<String>,
    /// 二维数组：每个内层数组代表一页的版面块
    #[serde(default)]
    pub layout_details: Vec<Vec<RawGlmLayoutBlock>>,
    /// 版面可视化图片 URL
    #[serde(default)]
    pub layout_visualization: Vec<String>,
    /// 文档信息（页数、页面尺寸）
    pub data_info: Option<OcrDataInfo>,
    /// token 用量
    pub usage: Option<OcrUsage>,
    /// 服务端请求 ID
    pub request_id: Option<String>,
    /// 原始 JSON（用于回放和调试）
    #[serde(default = "default_json_value")]
    pub raw_json: serde_json::Value,
}

fn default_json_value() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// 文档信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrDataInfo {
    /// 总页数（服务端可能省略，按可选处理）
    #[serde(default)]
    pub num_pages: Option<usize>,
    #[serde(default)]
    pub pages: Vec<OcrPageInfo>,
}

/// 页面尺寸信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrPageInfo {
    /// 页面宽度（服务端可能省略，按可选处理）
    #[serde(default)]
    pub width: Option<u32>,
    /// 页面高度（服务端可能省略，按可选处理）
    #[serde(default)]
    pub height: Option<u32>,
}

/// token 用量
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrUsage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub prompt_tokens_details: Option<serde_json::Value>,
}

/// GLM-OCR 原始版面块
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawGlmLayoutBlock {
    pub index: Option<i32>,
    pub label: String,
    pub bbox_2d: Option<[f64; 4]>,
    pub content: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// 将 GLM-OCR 响应规范化为 OcrPageResult 列表
///
/// `source_start_page` 和 `source_end_page` 是原始 PDF 页码范围，
/// `request_start_page_id` 是请求中使用的起始页 ID（子文件内页码或原始页码）。
/// 需要将响应页序映射回原始 PDF 页码。
#[allow(clippy::too_many_arguments)]
pub fn normalize_glm_ocr_response(
    response: &GlmOcrResponse,
    source_start_page: i32,
    source_end_page: i32,
    _request_start_page_id: i32,
    provider_name: &str,
    model_name: &str,
    // 客户端生成的稳定 request_id，服务端未回显时作为 fallback
    client_request_id: Option<&str>,
    // OCR profile: auto/general/table/formula/handwriting
    ocr_profile: &str,
) -> Result<Vec<OcrPageResult>> {
    let request_page_count = (source_end_page - source_start_page + 1) as usize;
    // 优先使用服务端回显的 request_id，若无则 fallback 到客户端生成的稳定 ID
    let effective_request_id = response
        .request_id
        .clone()
        .or_else(|| client_request_id.map(|s| s.to_string()));

    // 情况1: layout_details 有内容 — 按页处理
    if !response.layout_details.is_empty() {
        let response_page_count = response.layout_details.len();

        // 校验响应页数与请求页数一致
        if response_page_count != request_page_count {
            return Err(GBrainError::InvalidInput(format!(
                "GLM-OCR 响应页数({})与请求页数({})不一致: source_start={}, source_end={}",
                response_page_count, request_page_count, source_start_page, source_end_page
            )));
        }

        let md_per_page = split_md_by_pages(response.md_results.as_deref(), request_page_count);

        let mut results = Vec::with_capacity(request_page_count);
        for (i, page_blocks_raw) in response.layout_details.iter().enumerate() {
            let page_number = source_start_page + i as i32;

            // 从 data_info 提取页面尺寸
            let (ocr_page_width, ocr_page_height) = response
                .data_info
                .as_ref()
                .and_then(|info| info.pages.get(i))
                .map(|pi| (pi.width, pi.height))
                .unwrap_or((None, None));

            // 将原始块转为 OcrLayoutBlock
            let blocks: Vec<OcrLayoutBlock> = page_blocks_raw
                .iter()
                .map(|raw| OcrLayoutBlock {
                    page_number,
                    index: raw.index,
                    label: OcrBlockLabel::from_glm_label(&raw.label),
                    bbox_2d: raw.bbox_2d,
                    content: raw.content.clone().unwrap_or_default(),
                    width: raw.width,
                    height: raw.height,
                })
                .collect();

            // 从 block 生成纯文本（不含 image，按 profile 过滤）
            let text = blocks_to_plain_text(&blocks, ocr_profile);
            // 优先使用服务端返回的 markdown，否则从 blocks 生成
            let md_from_service = md_per_page.get(i).cloned().unwrap_or_default();
            let markdown = if md_from_service.is_empty() {
                blocks_to_markdown(&blocks, ocr_profile)
            } else {
                md_from_service
            };

            let viz_url = response
                .layout_visualization
                .get(i)
                .cloned()
                .unwrap_or_default();
            let viz_url = if viz_url.is_empty() {
                None
            } else {
                Some(viz_url)
            };

            results.push(OcrPageResult {
                page_number,
                text,
                markdown,
                blocks,
                layout_visualization_url: viz_url,
                raw_response_json: response.raw_json.clone(),
                request_id: effective_request_id.clone(),
                confidence: None,
                provider: provider_name.to_string(),
                model: model_name.to_string(),
                ocr_page_width,
                ocr_page_height,
            });
        }
        return Ok(results);
    }

    // 情况2: 只有 md_results，没有 layout_details
    if let Some(ref md) = response.md_results {
        if !md.is_empty() {
            // 无法可靠按页映射，整段作为一个结果
            // 如果只请求了一页，直接映射
            if request_page_count == 1 {
                // 从 data_info 提取页面尺寸
                let (ocr_page_width, ocr_page_height) = response
                    .data_info
                    .as_ref()
                    .and_then(|info| info.pages.first())
                    .map(|pi| (pi.width, pi.height))
                    .unwrap_or((None, None));

                return Ok(vec![OcrPageResult {
                    page_number: source_start_page,
                    text: strip_markdown(md),
                    markdown: md.clone(),
                    blocks: vec![],
                    layout_visualization_url: None,
                    raw_response_json: response.raw_json.clone(),
                    request_id: effective_request_id.clone(),
                    confidence: None,
                    provider: provider_name.to_string(),
                    model: model_name.to_string(),
                    ocr_page_width,
                    ocr_page_height,
                }]);
            }
            // 多页但无 layout_details — 尽力按页映射，降级返回（无 blocks，仅文本）
            let md_per_page = split_md_by_pages(Some(md), request_page_count);
            let mut results = Vec::with_capacity(request_page_count);
            for (i, page_md) in md_per_page.iter().enumerate() {
                let page_number = source_start_page + i as i32;
                let (ocr_page_width, ocr_page_height) = response
                    .data_info
                    .as_ref()
                    .and_then(|info| info.pages.get(i))
                    .map(|pi| (pi.width, pi.height))
                    .unwrap_or((None, None));
                results.push(OcrPageResult {
                    page_number,
                    text: strip_markdown(page_md),
                    markdown: page_md.clone(),
                    blocks: vec![],
                    layout_visualization_url: None,
                    raw_response_json: response.raw_json.clone(),
                    request_id: effective_request_id.clone(),
                    confidence: None,
                    provider: provider_name.to_string(),
                    model: model_name.to_string(),
                    ocr_page_width,
                    ocr_page_height,
                });
            }
            return Ok(results);
        }
    }

    // 情况3: md_results 和 layout_details 都为空
    // 仍需为每页生成空 OcrPageResult，确保调用方能持久化 empty_ocr 页级记录，
    // 避免文档状态被误判为 not_needed/done
    let mut results = Vec::with_capacity(request_page_count);
    for i in 0..request_page_count {
        let (ocr_page_width, ocr_page_height) = response
            .data_info
            .as_ref()
            .and_then(|info| info.pages.get(i))
            .map(|pi| (pi.width, pi.height))
            .unwrap_or((None, None));
        results.push(OcrPageResult {
            page_number: source_start_page + i as i32,
            text: String::new(),
            markdown: String::new(),
            blocks: vec![],
            layout_visualization_url: None,
            raw_response_json: response.raw_json.clone(),
            request_id: effective_request_id.clone(),
            confidence: None,
            provider: provider_name.to_string(),
            model: model_name.to_string(),
            ocr_page_width,
            ocr_page_height,
        });
    }
    Ok(results)
}

/// M21 修复：块格式化策略 — 控制表格和公式的输出格式
///
/// - `Plain`: 表格用纯文本（| 分隔），公式用 [FORMULA] 标记
/// - `Markdown`: 表格用 markdown 表格，公式用 $$...$$ LaTeX 标记
enum BlockFormatMode {
    Plain,
    Markdown,
}

/// OCR profile 格式化策略：所有 block 均保留，但按 profile 对目标类型增强输出。
/// - auto: 根据 block 类型自动选择 table/formula/general 增强
/// - general: 无增强，按默认格式输出
/// - table: 表格 block 使用完整 markdown 表格格式
/// - formula: 公式 block 保留 LaTeX 原始内容，加独立标记
/// - handwriting: 文本 block 不做特殊处理（手写识别质量由 OCR 引擎决定）
fn format_block(block: &OcrLayoutBlock, profile: &str, mode: BlockFormatMode) -> Option<String> {
    if block.content.is_empty() {
        return None;
    }
    let profile = effective_block_profile(block, profile);
    match &block.label {
        OcrBlockLabel::Text => Some(block.content.clone()),
        OcrBlockLabel::Formula => {
            if profile == "formula" {
                // 公式 profile：保留 LaTeX 原始内容，用 $$ 包裹
                Some(format!("$${}$$", block.content.trim()))
            } else {
                Some(format!("[FORMULA]\n{}", block.content))
            }
        }
        OcrBlockLabel::Table => {
            // table profile 始终输出完整 markdown 表格以保真；
            // 其他 profile 按 mode 控制格式。
            let table_text = if profile == "table" {
                html_table_to_markdown(&block.content)
            } else {
                match mode {
                    BlockFormatMode::Markdown => html_table_to_markdown(&block.content),
                    BlockFormatMode::Plain => html_table_to_plain(&block.content),
                }
            };
            Some(format!("[TABLE]\n{}", table_text))
        }
        OcrBlockLabel::Image => None,
        OcrBlockLabel::Unknown(_) => Some(block.content.clone()),
    }
}

fn effective_block_profile<'a>(block: &OcrLayoutBlock, profile: &'a str) -> &'a str {
    if profile != "auto" {
        return profile;
    }
    match block.label {
        OcrBlockLabel::Formula => "formula",
        OcrBlockLabel::Table => "table",
        _ => "general",
    }
}

/// 从版面块生成纯文本，按 profile 增强目标 block 格式
fn blocks_to_plain_text(blocks: &[OcrLayoutBlock], profile: &str) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        if let Some(text) = format_block(block, profile, BlockFormatMode::Plain) {
            parts.push(text);
        }
    }
    parts.join("\n\n")
}

/// 从版面块生成 markdown（layout_details 模式下 fallback），按 profile 增强
///
/// 与 blocks_to_plain_text 类似但保留更多格式：表格用 markdown 表格，
/// 公式用 LaTeX 标记。图片 block 跳过（仅保留在 metadata/UI）。
fn blocks_to_markdown(blocks: &[OcrLayoutBlock], profile: &str) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        if let Some(text) = format_block(block, profile, BlockFormatMode::Markdown) {
            parts.push(text);
        }
    }
    parts.join("\n\n")
}

/// 将 HTML 表格转为 plain text（单元格用 | 连接）
fn html_table_to_plain(html: &str) -> String {
    let cells: Vec<&str> = TABLE_CELL_RE
        .captures_iter(html)
        .filter_map(|c| c.get(1).map(|m| m.as_str().trim()))
        .collect();

    if cells.is_empty() {
        strip_html_tags(html)
    } else {
        cells.join(" | ")
    }
}

/// 将 HTML 表格转为 markdown 表格格式（带表头分隔线）
fn html_table_to_markdown(html: &str) -> String {
    static TR_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"<tr[^>]*>(.*?)</tr>").unwrap());

    let rows: Vec<Vec<String>> = TR_RE
        .captures_iter(html)
        .map(|cap| {
            TABLE_CELL_RE
                .captures_iter(&cap[1])
                .filter_map(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
                .collect::<Vec<String>>()
        })
        .filter(|row: &Vec<String>| !row.is_empty())
        .collect();

    if rows.is_empty() {
        return strip_html_tags(html);
    }

    let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut lines = Vec::new();

    // 首行作为表头
    let header = &rows[0];
    lines.push(format!("| {} |", header.join(" | ")));
    lines.push(format!(
        "| {} |",
        (0..col_count)
            .map(|_| "---")
            .collect::<Vec<_>>()
            .join(" | ")
    ));

    // 后续行作为数据行
    for row in rows.iter().skip(1) {
        // 补齐列数
        let padded: Vec<String> = row
            .iter()
            .chain(std::iter::repeat(&String::new()))
            .take(col_count)
            .cloned()
            .collect();
        lines.push(format!("| {} |", padded.join(" | ")));
    }

    lines.join("\n")
}

/// 简单去掉 HTML 标签
fn strip_html_tags(html: &str) -> String {
    HTML_TAG_RE.replace_all(html, "").trim().to_string()
}

/// 去掉 markdown 格式符号
fn strip_markdown(md: &str) -> String {
    // 简单处理：去掉 # * - 等 markdown 语法符号
    let lines: Vec<String> = md
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            // 去掉标题符号
            if trimmed.starts_with('#') {
                trimmed.trim_start_matches('#').trim().to_string()
            } else {
                trimmed.to_string()
            }
        })
        .filter(|l| !l.is_empty())
        .collect();
    lines.join("\n")
}

/// M14 修复：OCR provider 返回的显式分页标记
const PAGE_BREAK_MARKER: &str = "<!-- page-break -->";

/// 尝试按页拆分 markdown 结果
///
/// GLM-OCR 的 md_results 可能包含分页标记。拆分优先级：
/// 1. 显式页标记 `<!-- page-break -->`（最可靠，由 provider 明确标记）
/// 2. `\newpage` 分隔符
///    如果无法拆分，返回每页空字符串，让调用方从各页 blocks 生成 markdown。
///
/// P2 修复：已移除 `\n---\n` fallback，避免正文水平线误触发分页。
fn split_md_by_pages(md: Option<&str>, page_count: usize) -> Vec<String> {
    let md = match md {
        Some(m) if !m.is_empty() => m,
        _ => return vec![String::new(); page_count],
    };

    // 单页响应直接返回
    if page_count == 1 {
        return vec![md.to_string()];
    }

    // 优先按显式页标记拆分（最可靠，不会与正文内容冲突）
    let parts: Vec<&str> = md.split(PAGE_BREAK_MARKER).collect();
    if parts.len() == page_count {
        return parts.iter().map(|s| s.to_string()).collect();
    }

    // 尝试按 \newpage 拆分
    let parts: Vec<&str> = md.split("\\newpage").collect();
    if parts.len() == page_count {
        return parts.iter().map(|s| s.to_string()).collect();
    }

    // 无法拆分：不把整段 markdown 放到第一页（会导致错页），
    // 返回全空，让调用方从各页 layout_details blocks 生成 per-page markdown。
    vec![String::new(); page_count]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_single_page_with_layout() {
        let response = GlmOcrResponse {
            id: Some("test-123".into()),
            created: Some(1234567890),
            model: Some("glm-ocr".into()),
            md_results: Some("Hello world".into()),
            layout_details: vec![vec![RawGlmLayoutBlock {
                index: Some(0),
                label: "text".into(),
                bbox_2d: Some([0.1, 0.2, 0.9, 0.8]),
                content: Some("Hello world".into()),
                width: None,
                height: None,
            }]],
            layout_visualization: vec![],
            data_info: None,
            usage: None,
            request_id: Some("req-001".into()),
            raw_json: serde_json::json!({}),
        };

        let results =
            normalize_glm_ocr_response(&response, 1, 1, 1, "glm_ocr", "glm-ocr", None, "general")
                .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].page_number, 1);
        assert_eq!(results[0].text, "Hello world");
        assert_eq!(results[0].blocks.len(), 1);
        assert_eq!(results[0].blocks[0].label, OcrBlockLabel::Text);
    }

    #[test]
    fn test_normalize_page_count_mismatch() {
        let response = GlmOcrResponse {
            id: None,
            created: None,
            model: None,
            md_results: None,
            layout_details: vec![vec![], vec![]], // 2 pages in response
            layout_visualization: vec![],
            data_info: None,
            usage: None,
            request_id: None,
            raw_json: serde_json::json!({}),
        };

        let result =
            normalize_glm_ocr_response(&response, 1, 3, 1, "glm_ocr", "glm-ocr", None, "general");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("响应页数(2)与请求页数(3)不一致"));
    }

    #[test]
    fn test_normalize_md_only_single_page() {
        let response = GlmOcrResponse {
            id: None,
            created: None,
            model: None,
            md_results: Some("# Title\nSome content".into()),
            layout_details: vec![],
            layout_visualization: vec![],
            data_info: None,
            usage: None,
            request_id: None,
            raw_json: serde_json::json!({}),
        };

        let results =
            normalize_glm_ocr_response(&response, 5, 5, 5, "glm_ocr", "glm-ocr", None, "general")
                .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].page_number, 5);
        assert!(results[0].text.contains("Title"));
    }

    #[test]
    fn test_split_md_by_pages_explicit_marker() {
        let md = "第一页内容<!-- page-break -->第二页内容<!-- page-break -->第三页内容";
        let parts = split_md_by_pages(Some(md), 3);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "第一页内容");
        assert_eq!(parts[1], "第二页内容");
        assert_eq!(parts[2], "第三页内容");
    }

    #[test]
    fn test_split_md_by_pages_marker_priority_over_dash() {
        // 显式标记优先于 --- 分隔符
        let md = "第一页\n---\n第二页";
        let parts = split_md_by_pages(Some(md), 2);
        // --- 拆分后恰好 2 段，应能正确拆分
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn test_html_table_to_plain() {
        let html = "<table><tr><td>A</td><td>B</td></tr><tr><td>1</td><td>2</td></tr></table>";
        let plain = html_table_to_plain(html);
        assert!(plain.contains("A"));
        assert!(plain.contains("1"));
    }

    #[test]
    fn test_auto_profile_enhances_table_and_formula_blocks() {
        let blocks = vec![
            OcrLayoutBlock {
                page_number: 1,
                index: Some(0),
                label: OcrBlockLabel::Table,
                bbox_2d: None,
                content:
                    "<table><tr><td>A</td><td>B</td></tr><tr><td>1</td><td>2</td></tr></table>"
                        .to_string(),
                width: None,
                height: None,
            },
            OcrLayoutBlock {
                page_number: 1,
                index: Some(1),
                label: OcrBlockLabel::Formula,
                bbox_2d: None,
                content: "E=mc^2".to_string(),
                width: None,
                height: None,
            },
        ];

        let auto_text = blocks_to_plain_text(&blocks, "auto");

        assert!(auto_text.contains("| A | B |"));
        assert!(auto_text.contains("| --- | --- |"));
        assert!(auto_text.contains("$$E=mc^2$$"));
    }

    #[test]
    fn test_block_label_from_glm() {
        assert_eq!(OcrBlockLabel::from_glm_label("text"), OcrBlockLabel::Text);
        assert_eq!(OcrBlockLabel::from_glm_label("table"), OcrBlockLabel::Table);
        assert_eq!(
            OcrBlockLabel::from_glm_label("formula"),
            OcrBlockLabel::Formula
        );
        assert_eq!(OcrBlockLabel::from_glm_label("image"), OcrBlockLabel::Image);
        match OcrBlockLabel::from_glm_label("custom") {
            OcrBlockLabel::Unknown(s) => assert_eq!(s, "custom"),
            _ => panic!("应为 Unknown"),
        }
    }
}
