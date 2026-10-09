//! HTML parser with structure cleaning and heading hierarchy preservation.
//!
//! P2-009: Removes script, style, nav, footer before text extraction.
//! P2-010: Preserves h1-h6 hierarchy and title/meta extraction.

use super::{DocumentParser, ParsedDocument};
use crate::error::GBrainError;
use std::collections::HashMap;

// M-11: 预编译高频正则，避免在循环/每次调用中重复编译

/// 匹配 HTML 开标签（捕获组1=标签名，组2=属性）
static TAG_OPEN_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?is)<(\w+)([^>]*)>").unwrap());

/// 匹配 HTML 闭标签
static TAG_CLOSE_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"<[^>]*>").unwrap());

/// 匹配 class 属性值（用于隐藏 class 检测）
static CLASS_ATTR_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r#"(?i)class\s*=\s*["']([^"']*)["']"#).unwrap());

// --- 富文本转换预编译正则（避免每次调用重新编译） ---

/// 匹配 <table>...</table>
static TABLE_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?is)<table\b[^>]*>(.*?)</table>").unwrap());
/// 匹配 <tr>...</tr>
static TR_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?is)<tr\b[^>]*>(.*?)</tr>").unwrap());
/// 匹配 <th>/<td> 单元格
static CELL_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?is)<t[hd]\b[^>]*>(.*?)</t[hd]>").unwrap());
/// 匹配 colspan 属性
static COLSPAN_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r#"(?i)colspan\s*=\s*["']?(\d+)"#).unwrap());
/// 匹配流程图 div（data-type="flow" 或 data-tag="flow"）
static FLOW_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r#"(?is)<div\b[^>]*(?:data-type\s*=\s*["']flow["']|data-tag\s*=\s*["']flow["'])[^>]*>(.*?)</div>"#,
    ).unwrap()
});
/// 匹配 data-code 属性（提取流程图代码）
static DATA_CODE_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"(?i)data-code\s*=\s*["']([^"']*)["']"#).unwrap()
});
/// 匹配任务列表 <ul data-type="taskList">...</ul>
static TASKLIST_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"(?is)<ul\b[^>]*data-type\s*=\s*["']taskList["'][^>]*>(.*?)</ul>"#).unwrap()
});
/// 匹配 <li>...</li>
static LI_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?is)<li\b[^>]*>(.*?)</li>").unwrap());
/// 匹配 data-checked 属性
static CHECKED_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"(?i)data-checked\s*=\s*["']?(true|1|yes)["']?"#).unwrap()
});

pub struct HtmlParser;

impl Default for HtmlParser {
    fn default() -> Self {
        Self::new()
    }
}

impl HtmlParser {
    pub fn new() -> Self {
        Self
    }
}

impl DocumentParser for HtmlParser {
    fn parse(&self, data: &[u8]) -> Result<ParsedDocument, GBrainError> {
        let raw = std::str::from_utf8(data)
            .map_err(|e| GBrainError::FileError(format!("HTML is not valid UTF-8: {}", e)))?;

        // P2-009: 清理非内容元素
        let cleaned = clean_html(raw);

        // 富文本语义保留：将表格、流程图、任务列表转为 Markdown 格式
        let enriched = preserve_rich_text(&cleaned);

        // 提取 metadata（title, meta 标签）
        let mut metadata = HashMap::new();
        extract_metadata(raw, &mut metadata);

        // P2-010: 提取标题层级
        let headings = extract_headings(raw);
        if !headings.is_empty() {
            metadata.insert("headings".to_string(), headings.join(" > "));
        }

        // 正文提取
        let text = html2text::from_read(enriched.as_bytes(), 80)
            .map_err(|e| GBrainError::FileError(format!("HTML parse failed: {}", e)))?;
        let content = clean_html_text(&text);

        // P1-010/P2-010: 构建结构化 blocks（按标题层级分段）
        let blocks = build_html_blocks(&text, &headings);
        // P1-2: 抽取图片/附件等媒体引用
        let media_refs = extract_media_refs(&cleaned);
        Ok(ParsedDocument {
            content,
            metadata,
            blocks: Some(blocks),
            media_refs,
        })
    }

    fn extensions(&self) -> &[&str] {
        &["html", "htm"]
    }
}

/// P1 修复: HtmlParser 的 RichContentNormalizer 实现。
/// 将 ParsedDocument 转为标准化 NormalizedDocument，保留已有的
/// 表格/流程图/任务列表预处理结果，并附加 media_refs。
impl crate::kb::parser::RichContentNormalizer for HtmlParser {
    fn normalize(
        &self,
        parsed: crate::kb::parser::ParsedDocument,
    ) -> crate::error::Result<crate::kb::parser::NormalizedDocument> {
        Ok(crate::kb::parser::NormalizedDocument {
            markdown: parsed.content,
            blocks: parsed.blocks.unwrap_or_default(),
            media_refs: parsed.media_refs.clone(),
            attachments: parsed.media_refs, // 当前实现中 attachment 与 media_ref 相同
        })
    }
}

/// 富文本语义保留：在 html2text 处理之前，将 HTML 中的表格、流程图、任务列表
/// 转换为 Markdown 格式，保留结构语义以支持向量化检索。
fn preserve_rich_text(html: &str) -> String {
    let mut result = html.to_string();
    result = convert_images_to_markdown(&result);
    result = convert_attachment_spans_to_markdown(&result);
    result = convert_tables_to_markdown(&result);
    result = convert_flowcharts_to_mermaid(&result);
    result = convert_tasklists_to_checkbox(&result);
    result
}

fn convert_images_to_markdown(html: &str) -> String {
    IMG_TAG_RE
        .replace_all(html, |caps: &regex::Captures| {
            let attrs = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let (src, alt, title) = parse_img_attrs(attrs);
            let Some(src) = src else {
                return String::new();
            };
            if src.starts_with("data:") || src.starts_with("javascript:") {
                return String::new();
            }
            match title {
                Some(title) if !title.is_empty() => {
                    format!("\n![{}]({} \"{}\")\n", alt.unwrap_or_default(), src, title)
                }
                _ => format!("\n![{}]({})\n", alt.unwrap_or_default(), src),
            }
        })
        .to_string()
}

fn convert_attachment_spans_to_markdown(html: &str) -> String {
    ATTACHMENT_SPAN_RE
        .replace_all(html, |caps: &regex::Captures| {
            let full = caps.get(0).map(|m| m.as_str()).unwrap_or("");
            let href = parse_attr(full, "data-href")
                .or_else(|| parse_attr(full, "href"))
                .or_else(|| parse_attr(full, "data-url"));
            let Some(href) = href else {
                return strip_tags(caps.get(1).map(|m| m.as_str()).unwrap_or(""));
            };
            if href.starts_with("data:") || href.starts_with("javascript:") {
                return String::new();
            }
            let label = strip_tags(caps.get(1).map(|m| m.as_str()).unwrap_or(""))
                .trim()
                .to_string();
            if label.is_empty() {
                format!("\n[{}]({})\n", href, href)
            } else {
                format!("\n[{}]({})\n", label, href)
            }
        })
        .to_string()
}

/// 将 HTML <table> 转换为 Markdown 表格格式。
///
/// 处理 <thead>/<tbody>、<th>/<td>、colspan 等常见结构。
/// 转换后的 Markdown 表格能被向量化引擎保留表格语义。
fn convert_tables_to_markdown(html: &str) -> String {
    TABLE_RE
        .replace_all(html, |caps: &regex::Captures| {
            let table_content = caps.get(1).unwrap().as_str();
            let mut rows: Vec<Vec<String>> = Vec::new();

            for tr_cap in TR_RE.captures_iter(table_content) {
                let tr_content = tr_cap.get(1).unwrap().as_str();
                let mut cells: Vec<String> = Vec::new();

                for cell_cap in CELL_RE.captures_iter(tr_content) {
                    let cell_text = strip_tags(cell_cap.get(1).unwrap().as_str())
                        .trim()
                        .to_string();
                    // 处理 colspan：重复内容填满跨列
                    let full_tag = cell_cap.get(0).unwrap().as_str();
                    let span = COLSPAN_RE
                        .captures(full_tag)
                        .and_then(|c| c.get(1).and_then(|m| m.as_str().parse::<usize>().ok()))
                        .unwrap_or(1);
                    for _ in 0..span {
                        cells.push(cell_text.clone());
                    }
                }
                if !cells.is_empty() {
                    rows.push(cells);
                }
            }

            if rows.is_empty() {
                return String::new();
            }

            // 统一列数为最大值
            let max_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
            for row in &mut rows {
                while row.len() < max_cols {
                    row.push(String::new());
                }
            }

            // 构建 Markdown 表格
            let mut md = String::new();
            for (i, row) in rows.iter().enumerate() {
                md.push_str("| ");
                md.push_str(&row.join(" | "));
                md.push_str(" |\n");
                // 第一行后添加分隔线
                if i == 0 {
                    md.push_str(&format!(
                        "| {} |\n",
                        (0..max_cols).map(|_| "---").collect::<Vec<_>>().join(" | ")
                    ));
                }
            }
            md
        })
        .to_string()
}

/// 将流程图 div（data-type="flow" 或 data-tag="flow"）转为 Mermaid 代码块。
///
/// 保留流程图的语义信息，使其能被向量化引擎检索。
fn convert_flowcharts_to_mermaid(html: &str) -> String {
    FLOW_RE
        .replace_all(html, |caps: &regex::Captures| {
            let content = caps.get(1).unwrap().as_str();
            // 提取 data-code 属性或直接使用内容
            let code = DATA_CODE_RE
                .captures(caps.get(0).unwrap().as_str())
                .map(|c| c.get(1).unwrap().as_str().to_string())
                .unwrap_or_else(|| strip_tags(content));

            // 解码 HTML 实体和转义换行
            let decoded = code
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&")
                .replace("&quot;", "\"")
                .replace("&#39;", "'")
                .replace("\\n", "\n");

            if decoded.trim().is_empty() {
                String::new()
            } else {
                format!("\n```mermaid\n{}\n```\n", decoded.trim())
            }
        })
        .to_string()
}

/// 将任务列表（data-type="taskList" 或 data-checked 属性）转为 Markdown checkbox。
///
/// 保留任务状态信息，支持待办事项类文档的语义检索。
fn convert_tasklists_to_checkbox(html: &str) -> String {
    TASKLIST_RE
        .replace_all(html, |caps: &regex::Captures| {
            let ul_content = caps.get(1).unwrap().as_str();
            let mut md = String::new();

            for li_cap in LI_RE.captures_iter(ul_content) {
                let li_tag = li_cap.get(0).unwrap().as_str();
                let li_content = strip_tags(li_cap.get(1).unwrap().as_str())
                    .trim()
                    .to_string();
                if li_content.is_empty() {
                    continue;
                }
                let checked = CHECKED_RE.is_match(li_tag);
                let checkbox = if checked { "[x]" } else { "[ ]" };
                md.push_str(&format!("- {} {}\n", checkbox, li_content));
            }
            md
        })
        .to_string()
}

/// P2-009: 移除 script/style/nav/footer/隐藏元素
fn clean_html(raw: &str) -> String {
    let mut result = raw.to_string();

    // 移除 <script>...</script>
    result = remove_tags(&result, "script");
    // 移除 <style>...</style>
    result = remove_tags(&result, "style");
    // 移除 <nav>...</nav>
    result = remove_tags(&result, "nav");
    // 移除 <footer>...</footer>
    result = remove_tags(&result, "footer");
    // 移除 <header>...</header>
    result = remove_tags(&result, "header");
    // 移除 <noscript>...</noscript>
    result = remove_tags(&result, "noscript");

    // 移除 hidden/display:none 元素
    // 覆盖范围：内联 style 中的 display:none / visibility:hidden、
    // aria-hidden="true"，以及常见 CSS 隐藏 class（hidden, invisible, d-none,
    // visually-hidden, sr-only）。
    // 不支持：外部 CSS 文件中的 .hidden { display:none }、<style> 块中的样式规则、
    // JavaScript 动态修改 class/style、HTML 注释包裹的隐藏元素。

    // 第一步：匹配 inline style / aria-hidden（子树匹配，正确处理嵌套内容）
    let Ok(style_re) = regex::Regex::new(
        r#"(?i)(?:display\s*:\s*none|visibility\s*:\s*hidden|aria-hidden\s*=\s*["']true["'])"#,
    ) else {
        return result;
    };
    result = remove_elements_matching(&result, |tag| style_re.is_match(tag));

    // 第二步：匹配隐藏 CSS class（按空格精确匹配 class token，子树匹配）
    result = remove_hidden_class_elements(&result);

    result
}

/// 删除指定标签的完整子树。
/// 对普通标签（nav, footer 等）使用嵌套深度追踪，正确处理同名嵌套。
/// 对 raw-text 元素（script, style 等）直接匹配第一个闭标签，
/// 因为这些元素的内容中出现的 `<tag>` 是文本而非真实标签，不应增加嵌套深度。
fn remove_tags(html: &str, tag: &str) -> String {
    let open_pattern = format!(r"(?is)<{}\b[^>]*>", regex::escape(tag));
    let Ok(open_re) = regex::Regex::new(&open_pattern) else {
        return html.to_string();
    };
    let tag_lower = tag.to_lowercase();

    // raw-text 元素：内容中出现的 `<tag>` 是文本而非真实标签，不追踪嵌套深度
    let is_raw_text = matches!(
        tag_lower.as_str(),
        "script" | "style" | "textarea" | "title"
    );
    let close_re = if is_raw_text {
        let close_pattern = format!(r"(?i)</{}\s*>", regex::escape(&tag_lower));
        regex::Regex::new(&close_pattern).ok()
    } else {
        None
    };

    let mut result = String::with_capacity(html.len());
    let mut pos = 0;

    while pos < html.len() {
        if let Some(m) = open_re.find(&html[pos..]) {
            let open_start = pos + m.start();
            let open_end = pos + m.end();
            result.push_str(&html[pos..open_start]);

            if m.as_str().ends_with("/>") {
                // 自闭合标签：直接跳过
                pos = open_end;
            } else if let Some(ref cre) = close_re {
                // raw-text 元素：找第一个闭标签，不追踪嵌套
                if let Some(close_m) = cre.find(&html[open_end..]) {
                    pos = open_end + close_m.end();
                } else {
                    result.push_str(m.as_str());
                    pos = open_end;
                }
            } else if let Some(subtree_end) = find_matching_close(html, open_end, &tag_lower) {
                // 普通标签：嵌套深度追踪
                pos = subtree_end;
            } else {
                result.push_str(m.as_str());
                pos = open_end;
            }
        } else {
            result.push_str(&html[pos..]);
            break;
        }
    }

    result
}

/// 从 `start` 位置开始，查找与开标签匹配的闭标签位置。
/// 使用深度计数追踪嵌套同名标签，返回匹配闭标签之后的全局偏移量。
/// 未找到匹配时返回 None。
/// m-15: 添加最大嵌套深度限制，防止超深层嵌套 HTML 导致 O(n²)
const MAX_TAG_NESTING_DEPTH: usize = 256;

fn find_matching_close(html: &str, start: usize, tag_name: &str) -> Option<usize> {
    let open_pat = format!(r"(?i)<{}[\s>/]", regex::escape(tag_name));
    let close_pat = format!(r"(?i)</{}\s*>", regex::escape(tag_name));
    let Ok(open_re) = regex::Regex::new(&open_pat) else {
        return None;
    };
    let Ok(close_re) = regex::Regex::new(&close_pat) else {
        return None;
    };

    let mut depth: usize = 1;
    let mut pos = start;

    while depth > 0 && pos < html.len() {
        let next_open = open_re
            .find(&html[pos..])
            .map(|m| (pos + m.start(), pos + m.end()));
        let next_close = close_re
            .find(&html[pos..])
            .map(|m| (pos + m.start(), pos + m.end()));

        let closest = match (next_open, next_close) {
            (Some(o), Some(c)) if o.0 < c.0 => (o.0, o.1, true),
            (Some(_), Some(c)) => (c.0, c.1, false),
            (Some(o), None) => (o.0, o.1, true),
            (None, Some(c)) => (c.0, c.1, false),
            (None, None) => return None,
        };

        if closest.2 {
            depth += 1;
            // m-15: 超过最大嵌套深度时停止追踪，防止恶意/畸形 HTML 导致 O(n²)
            if depth > MAX_TAG_NESTING_DEPTH {
                return None;
            }
        } else {
            depth -= 1;
        }
        pos = closest.1;
    }

    if depth == 0 {
        Some(pos)
    } else {
        None
    }
}

/// 删除匹配谓词的 HTML 元素及其完整子树。
/// `detect(open_tag)` 接收开标签文本，返回 true 则删除该元素及所有子内容。
/// 正确处理嵌套同名标签：`<div class="hidden"><div>inner</div></div>` 会被完整删除。
fn remove_elements_matching(html: &str, detect: impl Fn(&str) -> bool) -> String {
    let mut result = String::with_capacity(html.len());
    let mut pos = 0;

    while pos < html.len() {
        let Some(cap) = TAG_OPEN_RE.captures(&html[pos..]) else {
            result.push_str(&html[pos..]);
            break;
        };
        let full = cap.get(0).unwrap();
        let open_start = pos + full.start();
        let open_end = pos + full.end();
        let tag_name = cap.get(1).unwrap().as_str().to_lowercase();

        result.push_str(&html[pos..open_start]);

        if full.as_str().ends_with("/>") {
            // s-9: 自闭合标签 — 仅在未命中时保留
            if !detect(full.as_str()) {
                result.push_str(full.as_str());
            }
            pos = open_end;
        } else if detect(full.as_str()) {
            // 非自闭合标签 + 需要删除：找匹配闭标签，跳过整棵子树
            if let Some(subtree_end) = find_matching_close(html, open_end, &tag_name) {
                pos = subtree_end;
            } else {
                result.push_str(full.as_str());
                pos = open_end;
            }
        } else {
            result.push_str(full.as_str());
            pos = open_end;
        }
    }

    result
}

/// 移除含有隐藏 CSS class 的 HTML 元素及其完整子树。
/// 按空格拆分 class 属性值后精确匹配 token，避免误匹配连字符类名（如 `not-hidden`）。
/// 正确处理嵌套同名标签，确保子树内容不会泄漏到 KB 索引。
fn remove_hidden_class_elements(html: &str) -> String {
    static HIDDEN_TOKENS: &[&str] = &[
        "hidden",
        "invisible",
        "d-none",
        "visually-hidden",
        "sr-only",
    ];
    remove_elements_matching(html, |open_tag| {
        if let Some(cap) = CLASS_ATTR_RE.captures(open_tag) {
            if let Some(classes) = cap.get(1) {
                return classes
                    .as_str()
                    .split_whitespace()
                    .any(|token| HIDDEN_TOKENS.iter().any(|h| token.eq_ignore_ascii_case(h)));
            }
        }
        false
    })
}

/// P2-010: 提取 h1-h6 标题层级
fn extract_headings(raw: &str) -> Vec<String> {
    let mut headings = Vec::new();
    for level in 1..=6 {
        let pattern = format!(r"<h{}[^>]*>(.*?)</h{}>", level, level);
        if let Ok(re) = regex::Regex::new(&pattern) {
            for cap in re.captures_iter(raw) {
                if let Some(m) = cap.get(1) {
                    let text = strip_tags(m.as_str()).trim().to_string();
                    if !text.is_empty() {
                        headings.push(text);
                    }
                }
            }
        }
    }
    headings
}

/// 提取 <title> 和 <meta> 标签内容到 metadata
fn extract_metadata(raw: &str, meta: &mut HashMap<String, String>) {
    // <title>（大小写不敏感，支持多行内容）
    if let Ok(re) = regex::RegexBuilder::new(r"<title[^>]*>(.*?)</title>")
        .case_insensitive(true)
        .dot_matches_new_line(true)
        .build()
    {
        if let Some(cap) = re.captures(raw) {
            if let Some(m) = cap.get(1) {
                let title = strip_tags(m.as_str()).trim().to_string();
                if !title.is_empty() {
                    meta.insert("title".to_string(), title);
                }
            }
        }
    }
    // <meta name="description" content="...">
    // H18 fix: 使用两步法 — 先匹配含有正确 name 属性的 <meta> 标签整体，
    // 再从该标签中提取 content 属性值，天然支持属性以任意顺序出现。
    let content_re = regex::RegexBuilder::new(r#"content\s*=\s*["']([^"']+)["']"#)
        .case_insensitive(true)
        .build();

    for name in &["description", "keywords", "author"] {
        // 第一步：匹配含有正确 name 的所有 <meta> 标签
        let tag_pattern = format!(
            r#"(?i)<meta\s+[^>]*?name\s*=\s*["']{}["'][^>]*?/?>"#,
            regex::escape(name)
        );

        if let Ok(tag_re) = regex::Regex::new(&tag_pattern) {
            if let Ok(ref content_re) = content_re {
                // 遍历所有匹配的 meta 标签，取第一个含有有效 content 的
                for tag_cap in tag_re.captures_iter(raw) {
                    if let Some(full_tag) = tag_cap.get(0) {
                        // 第二步：从标签中提取 content（大小写不敏感）
                        if let Some(content_cap) = content_re.captures(full_tag.as_str()) {
                            if let Some(m) = content_cap.get(1) {
                                let val = m.as_str().trim().to_string();
                                if !val.is_empty() {
                                    meta.insert(name.to_string(), val);
                                    break; // 找到有效 content 后停止
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn strip_tags(html: &str) -> String {
    TAG_CLOSE_RE.replace_all(html, "").to_string()
}

fn clean_html_text(text: &str) -> String {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// P2-010: 从纯文本和标题列表构建 ParsedBlock
fn build_html_blocks(text: &str, headings: &[String]) -> Vec<crate::kb::types::ParsedBlock> {
    let heading_str = if headings.is_empty() {
        String::new()
    } else {
        headings.join(" > ")
    };
    let paragraphs: Vec<&str> = text
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .collect();
    if paragraphs.is_empty() {
        return vec![crate::kb::types::ParsedBlock::paragraph(text)];
    }
    let mut offset = 0usize;
    paragraphs
        .iter()
        .map(|p| {
            let start = offset as i32;
            // FIX10-08: 统一使用字符偏移（chars().count()），禁止混用 byte 长度
            offset += p.chars().count() + 2; // +2 for \n\n separator
            crate::kb::types::ParsedBlock {
                text: p.to_string(),
                title_path: heading_str.clone(),
                page_number: None,
                source_start: Some(start),
                source_end: Some(offset as i32 - 2),
                block_type: "paragraph".to_string(),
                metadata: String::new(),
            }
        })
        .collect()
}

// --- P1-2: 富文本媒体引用抽取 ---

/// 匹配 <img ...> 标签
static IMG_TAG_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?is)<img\b([^>]*)/?>").unwrap());

/// 匹配 HTML 属性（key=value 或 key='value' 或 key="value"）
static ATTR_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"(?i)([\w:-]+)\s*=\s*(?:["']([^"']*)["']|([^\s>]+))"#).unwrap()
});

/// 匹配附件链接（href 指向 pdf/docx/xlsx/zip 等）
static ATTACHMENT_LINK_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r#"(?is)<a\b([^>]*)>([^<]*)</a>"#).unwrap());

/// 已知附件扩展名
fn is_attachment_href(href: &str) -> bool {
    let lower = href.to_lowercase();
    const EXTS: &[&str] = &[
        ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".zip", ".rar", ".7z", ".tar",
        ".gz", ".txt", ".csv", ".json", ".md", ".mp3", ".mp4", ".wav", ".mov",
    ];
    EXTS.iter().any(|e| lower.ends_with(e))
}

/// P1 修复: 匹配 PandaWiki 富文本附件 span（data-tag=attachment）
static ATTACHMENT_SPAN_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"(?is)<span\b[^>]*data-tag\s*=\s*["']attachment["'][^>]*>(.*?)</span>"#)
        .unwrap()
});

/// 从 HTML 中抽取图片/附件引用，作为 MediaRef 列表。
///
/// - `<img src="..." alt="...">` → `media_type="image"`
/// - `<a href="*.pdf|docx|...">label</a>` → `media_type="attachment"`
/// - `<span data-tag="attachment" data-href="...">label</span>` → `media_type="attachment"`（PandaWiki）
///
/// 内嵌 data: URL 与 javascript: 会被忽略，避免污染。
pub fn extract_media_refs(html: &str) -> Vec<crate::kb::types::MediaRef> {
    let mut refs: Vec<crate::kb::types::MediaRef> = Vec::new();

    // 抽取图片
    for cap in IMG_TAG_RE.captures_iter(html) {
        let attrs = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let (src, alt, title) = parse_img_attrs(attrs);
        let Some(src) = src else { continue };
        if src.starts_with("data:") || src.starts_with("javascript:") {
            continue;
        }
        refs.push(crate::kb::types::MediaRef {
            media_type: "image".to_string(),
            storage_path: src,
            mime_type: None,
            byte_size: None,
            embedded_data_base64: None,
            alt_text: alt,
            ocr_text: None,
            caption: title,
            page_number: None,
        });
    }

    // 抽取附件链接（标准 <a> 标签）
    for cap in ATTACHMENT_LINK_RE.captures_iter(html) {
        let attrs = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let label = cap
            .get(2)
            .map(|m| m.as_str().trim())
            .unwrap_or("")
            .to_string();
        let href = parse_attr(attrs, "href");
        let Some(href) = href else { continue };
        if !is_attachment_href(&href) {
            continue;
        }
        refs.push(crate::kb::types::MediaRef {
            media_type: "attachment".to_string(),
            storage_path: href,
            mime_type: None,
            byte_size: None,
            embedded_data_base64: None,
            alt_text: if label.is_empty() { None } else { Some(label) },
            ocr_text: None,
            caption: None,
            page_number: None,
        });
    }

    // P1 修复: 抽取 PandaWiki 富文本附件 span（data-tag="attachment"）
    for cap in ATTACHMENT_SPAN_RE.captures_iter(html) {
        let full_tag = cap.get(0).map(|m| m.as_str()).unwrap_or("");
        let inner_text = cap.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        // 从 span 标签中提取 data-href 或 data-url 属性
        let href = parse_attr(full_tag, "data-href").or_else(|| parse_attr(full_tag, "data-url"));
        let Some(href) = href else { continue };
        if href.starts_with("data:") || href.starts_with("javascript:") {
            continue;
        }
        refs.push(crate::kb::types::MediaRef {
            media_type: "attachment".to_string(),
            storage_path: href,
            mime_type: None,
            byte_size: None,
            embedded_data_base64: None,
            alt_text: if inner_text.is_empty() {
                None
            } else {
                Some(inner_text.to_string())
            },
            ocr_text: None,
            caption: None,
            page_number: None,
        });
    }

    refs
}

fn parse_img_attrs(attrs: &str) -> (Option<String>, Option<String>, Option<String>) {
    let mut src = None;
    let mut alt = None;
    let mut title = None;
    for cap in ATTR_RE.captures_iter(attrs) {
        let key = cap
            .get(1)
            .map(|m| m.as_str().to_lowercase())
            .unwrap_or_default();
        let val = cap
            .get(2)
            .or_else(|| cap.get(3))
            .map(|m| m.as_str().to_string());
        if let Some(v) = val {
            match key.as_str() {
                "src" => src = Some(v),
                "alt" => alt = Some(v),
                "title" => title = Some(v),
                _ => {}
            }
        }
    }
    (src, alt, title)
}

fn parse_attr(attrs: &str, name: &str) -> Option<String> {
    for cap in ATTR_RE.captures_iter(attrs) {
        let key = cap
            .get(1)
            .map(|m| m.as_str().to_lowercase())
            .unwrap_or_default();
        if key == name {
            return cap
                .get(2)
                .or_else(|| cap.get(3))
                .map(|m| m.as_str().to_string());
        }
    }
    None
}

/// HTML 默认富文本标准化器：把 ParsedDocument 转为 NormalizedDocument。
pub struct HtmlRichContentNormalizer;

impl super::RichContentNormalizer for HtmlRichContentNormalizer {
    fn normalize(
        &self,
        parsed: super::ParsedDocument,
    ) -> crate::error::Result<super::NormalizedDocument> {
        Ok(super::NormalizedDocument {
            markdown: parsed.content,
            blocks: parsed.blocks.unwrap_or_default(),
            media_refs: parsed.media_refs,
            attachments: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remove_script() {
        let html = "<html><head><script>alert('xss')</script></head><body>Hello</body></html>";
        let cleaned = clean_html(html);
        assert!(!cleaned.contains("alert"));
        assert!(cleaned.contains("Hello"));
    }

    #[test]
    fn test_remove_nav_footer() {
        let html = "<html><body><nav>Menu</nav><main>Content</main><footer>Copyright</footer></body></html>";
        let cleaned = clean_html(html);
        assert!(!cleaned.contains("Menu"));
        assert!(!cleaned.contains("Copyright"));
        assert!(cleaned.contains("Content"));
    }

    #[test]
    fn test_extract_headings() {
        let html = "<html><body><h1>Title</h1><h2>Section A</h2><h3>Detail</h3></body></html>";
        let headings = extract_headings(html);
        assert_eq!(headings, vec!["Title", "Section A", "Detail"]);
    }

    #[test]
    fn test_extract_title_meta() {
        let html = r#"<html><head><title>My Page</title><meta name="description" content="A test page"></head></html>"#;
        let mut meta = HashMap::new();
        extract_metadata(html, &mut meta);
        assert_eq!(meta.get("title").unwrap(), "My Page");
        assert_eq!(meta.get("description").unwrap(), "A test page");
    }

    #[test]
    fn test_extract_meta_content_before_name() {
        // H18: content 在 name 之前的属性顺序
        let html = r#"<html><head><meta content="A test page" name="description"></head></html>"#;
        let mut meta = HashMap::new();
        extract_metadata(html, &mut meta);
        assert_eq!(meta.get("description").unwrap(), "A test page");
    }

    #[test]
    fn test_extract_meta_uppercase_attrs() {
        // CONTENT 大写属性应正确提取
        let html =
            r#"<html><head><meta NAME="description" CONTENT="Upper case test"></head></html>"#;
        let mut meta = HashMap::new();
        extract_metadata(html, &mut meta);
        assert_eq!(meta.get("description").unwrap(), "Upper case test");
    }

    #[test]
    fn test_extract_meta_fallback_to_second() {
        // 第一个匹配 name 但无 content，应取第二个有效 meta
        let html = r#"<html><head><meta name="description"><meta name="description" content="fallback"></head></html>"#;
        let mut meta = HashMap::new();
        extract_metadata(html, &mut meta);
        assert_eq!(meta.get("description").unwrap(), "fallback");
    }

    #[test]
    fn test_remove_hidden_class() {
        // M16 测试：常见 CSS 隐藏 class 应被移除
        let html = r#"<html><body><div class="hidden">隐藏内容</div><p>可见内容</p><span class="d-none">隐藏2</span></body></html>"#;
        let cleaned = clean_html(html);
        assert!(!cleaned.contains("隐藏内容"));
        assert!(!cleaned.contains("隐藏2"));
        assert!(cleaned.contains("可见内容"));
    }

    #[test]
    fn test_remove_sr_only_class() {
        let html = r#"<html><body><span class="sr-only">屏幕阅读器文本</span><p>正常文本</p></body></html>"#;
        let cleaned = clean_html(html);
        assert!(!cleaned.contains("屏幕阅读器文本"));
        assert!(cleaned.contains("正常文本"));
    }

    #[test]
    fn test_not_hidden_class_preserved() {
        // 连字符类名 not-hidden 不应被误删
        let html = r#"<html><body><div class="not-hidden">保留内容</div></body></html>"#;
        let cleaned = clean_html(html);
        assert!(cleaned.contains("保留内容"));
    }

    #[test]
    fn test_remove_hidden_nested_subtree() {
        // 嵌套子树应被完整删除，不应残留内部可见内容
        let html =
            r#"<html><body><div class="hidden"><p>A</p><p>B</p></div><p>可见</p></body></html>"#;
        let cleaned = clean_html(html);
        assert!(!cleaned.contains("A"), "嵌套的 A 应被删除");
        assert!(!cleaned.contains("B"), "嵌套的 B 应被删除");
        assert!(cleaned.contains("可见"));
    }

    #[test]
    fn test_full_parse() {
        let html = r#"<html><head><title>Test</title><style>body{color:red}</style></head><body><nav>Menu</nav><h1>Hello</h1><p>World</p><script>evil()</script><footer>bye</footer></body></html>"#;
        let parser = HtmlParser::new();
        let result = parser.parse(html.as_bytes()).unwrap();
        assert!(result.content.contains("Hello"));
        assert!(result.content.contains("World"));
        assert!(!result.content.contains("evil"));
        assert!(!result.content.contains("Menu"));
        assert!(!result.content.contains("bye"));
        assert!(result.metadata.contains_key("title"));
    }

    #[test]
    fn test_table_to_markdown() {
        let html = r#"<html><body><table><tr><th>名称</th><th>数值</th></tr><tr><td>温度</td><td>36.5</td></tr></table></body></html>"#;
        let result = convert_tables_to_markdown(html);
        assert!(result.contains("| 名称 | 数值 |"));
        assert!(result.contains("| 温度 | 36.5 |"));
        assert!(result.contains("---"));
    }

    #[test]
    fn test_table_with_colspan() {
        let html = r#"<html><body><table><tr><th colspan="2">合并标题</th></tr><tr><td>A</td><td>B</td></tr></table></body></html>"#;
        let result = convert_tables_to_markdown(html);
        assert!(result.contains("| 合并标题 | 合并标题 |"));
    }

    #[test]
    fn test_flowchart_to_mermaid() {
        let html = r#"<html><body><div data-type="flow" data-code="graph TD; A-->B">流程图</div><p>其他内容</p></body></html>"#;
        let result = convert_flowcharts_to_mermaid(html);
        assert!(result.contains("```mermaid"));
        assert!(result.contains("graph TD; A-->B"));
        assert!(result.contains("其他内容"));
    }

    #[test]
    fn test_tasklist_to_checkbox() {
        let html = r#"<html><body><ul data-type="taskList"><li data-checked="true">已完成</li><li data-checked="false">未完成</li></ul></body></html>"#;
        let result = convert_tasklists_to_checkbox(html);
        assert!(result.contains("- [x] 已完成"));
        assert!(result.contains("- [ ] 未完成"));
    }

    #[test]
    fn test_preserve_rich_text_integration() {
        let html = r#"<html><body><table><tr><th>列A</th><th>列B</th></tr><tr><td>1</td><td>2</td></tr></table><ul data-type="taskList"><li data-checked="true">任务1</li></ul></body></html>"#;
        let result = preserve_rich_text(html);
        assert!(result.contains("| 列A | 列B |"));
        assert!(result.contains("- [x] 任务1"));
    }

    #[test]
    fn test_full_parse_with_table() {
        let html = r#"<html><head><title>表格测试</title></head><body><h1>数据表</h1><table><tr><th>名称</th><th>分数</th></tr><tr><td>语文</td><td>95</td></tr><tr><td>数学</td><td>88</td></tr></table></body></html>"#;
        let parser = HtmlParser::new();
        let result = parser.parse(html.as_bytes()).unwrap();
        // 表格应被保留为 Markdown 格式
        assert!(result.content.contains("语文"));
        assert!(result.content.contains("95"));
        assert!(result.content.contains("数学"));
        assert!(result.content.contains("88"));
    }
}
