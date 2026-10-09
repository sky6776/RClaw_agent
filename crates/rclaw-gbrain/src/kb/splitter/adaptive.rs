//! 自适应分割器 — 结构优先、长块局部细分
//!
//! 核心策略：
//! 1. 结构优先：Markdown 按标题切，PDF 按页/段落切，表格/代码走专门逻辑
//! 2. 短块保留：小于阈值的结构块保持完整
//! 3. 长块递归细分：超过阈值的块用 RecursiveCharSplitter 细分
//! 4. 长块且有 embedder 时可语义细分：用 SemanticSplitter 细分
//! 5. 表格/代码不走普通语义细分

use super::markdown_header::MarkdownHeaderSplitter;
use super::recursive::RecursiveCharSplitter;
use super::semantic::SemanticSplitter;
use super::{AsyncDocumentSplitter, Chunks, DocumentSplitter};
use crate::embedding::Embedder;
use crate::error::GBrainError;
use regex::Regex;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// 大块阈值（字符数）：超过此值才进入细分流程
const LARGE_CHUNK_THRESHOLD: usize = 1600;

/// 自适应分割器配置
#[derive(Debug, Clone)]
pub struct AdaptiveConfig {
    /// 文件扩展名（小写），用于判断格式
    pub extension: String,
    /// 目标 chunk 最大字符数
    pub chunk_size: usize,
    /// chunk 重叠字符数
    pub chunk_overlap: usize,
}

/// 自适应分割器
///
/// 根据文件格式自动选择最佳分割策略，
/// 结构优先、长块才细分，避免语义分割覆盖文档结构。
pub struct AdaptiveSplitter {
    config: AdaptiveConfig,
    embedder: Option<Arc<Embedder>>,
}

impl AdaptiveSplitter {
    pub fn new(config: AdaptiveConfig, embedder: Option<Arc<Embedder>>) -> Self {
        Self { config, embedder }
    }

    /// 执行自适应分割
    pub async fn split(&self, text: &str) -> Result<Chunks, GBrainError> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }

        let ext = self.config.extension.as_str();

        // 表格文件：递归分割，不走语义细分
        if is_table_extension(ext) {
            return split_table_or_code(text, self.config.chunk_size, self.config.chunk_overlap);
        }

        // 代码文件：先按符号边界切，再递归细分（P4: 符号/函数/类优先）
        if is_code_extension(ext) {
            return self.split_code(text);
        }

        // PDF：按页面标记切分，页内大块再细分（P4: 页码/layout/段落优先）
        if ext == "pdf" {
            return self.split_pdf(text).await;
        }

        // Markdown：先按标题切 section，大 section 再细分
        if ext == "md" || ext == "markdown" {
            return self.split_markdown(text).await;
        }

        // 其他格式（TXT/HTML/DOCX 等）：先尝试段落切分，大段落再细分
        self.split_generic(text).await
    }

    /// PDF 分割：按 [PAGE:N] 标记切分，页内大块再细分（P4）
    async fn split_pdf(&self, text: &str) -> Result<Chunks, GBrainError> {
        // 阶段 1：按页面标记切分
        let page_pattern = "[PAGE:";
        if !text.contains(page_pattern) {
            // 无页面标记，回退到通用分割
            return self.split_generic(text).await;
        }

        // 按 [PAGE:N] 拆分页面
        let mut pages: Vec<String> = Vec::new();
        let mut current_page = String::new();
        let mut in_page = false;

        for line in text.lines() {
            if line.starts_with(page_pattern) {
                // 保存上一页内容
                if in_page && !current_page.trim().is_empty() {
                    pages.push(current_page.trim().to_string());
                }
                current_page = line.to_string();
                in_page = true;
            } else if in_page {
                current_page.push('\n');
                current_page.push_str(line);
            }
        }
        // 最后一页
        if in_page && !current_page.trim().is_empty() {
            pages.push(current_page.trim().to_string());
        }

        if pages.is_empty() {
            return self.split_generic(text).await;
        }

        // 单页直接判定
        if pages.len() == 1 {
            let page_text = &pages[0];
            if page_text.chars().count() <= LARGE_CHUNK_THRESHOLD {
                return Ok(vec![page_text.to_string()]);
            }
            // 页内按段落再切
            let page_chunks = self.split_page_content(page_text).await?;
            return Ok(page_chunks);
        }

        // 阶段 2：逐页处理，大页再细分
        let mut result = Vec::new();
        for page_text in &pages {
            if page_text.chars().count() <= LARGE_CHUNK_THRESHOLD {
                result.push(page_text.to_string());
                continue;
            }
            // 大页：页内按段落结构细分
            let sub = self.split_page_content(page_text).await?;
            result.extend(sub);
        }

        Ok(result)
    }

    /// PDF 页内内容分割：先按双换行切段落，大段落再细分（P4）
    async fn split_page_content(&self, page_text: &str) -> Result<Chunks, GBrainError> {
        let paragraphs: Vec<&str> = page_text
            .split("\n\n")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        if paragraphs.len() <= 1 {
            if page_text.chars().count() <= self.config.chunk_size {
                return Ok(vec![page_text.to_string()]);
            }
            return self.refine_large_section(page_text).await;
        }

        let mut result = Vec::new();
        let mut current = String::new();

        for para in paragraphs {
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(para);

            if current.chars().count() > self.config.chunk_size {
                if current.chars().count() > LARGE_CHUNK_THRESHOLD {
                    let refined = self.refine_large_section(&current).await?;
                    result.extend(refined);
                } else if !current.trim().is_empty() {
                    result.push(current.trim().to_string());
                }
                current.clear();
            }
        }

        // 剩余内容
        if !current.trim().is_empty() {
            if current.chars().count() > LARGE_CHUNK_THRESHOLD {
                let refined = self.refine_large_section(&current).await?;
                result.extend(refined);
            } else {
                result.push(current.trim().to_string());
            }
        }

        Ok(result)
    }

    /// 代码分割：按函数/类符号边界切分，再递归细分（P4）
    fn split_code(&self, text: &str) -> Result<Chunks, GBrainError> {
        // P4: 代码符号边界正则（懒编译，全局复用）
        static CODE_SYMBOL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
            let patterns = [
                // Rust: fn, pub fn, impl, struct, enum, trait, mod
                r"^(pub(\s*\(\s*crate\s*\))?\s+)?(async\s+)?fn\s",
                r"^(pub\s+)?(unsafe\s+)?impl\b",
                r"^(pub\s+)?struct\s",
                r"^(pub\s+)?enum\s",
                r"^(pub\s+)?trait\s",
                r"^(pub\s+)?mod\s",
                // Python: def, class
                r"^(async\s+)?def\s",
                r"^class\s",
                // JavaScript/TypeScript: function, class, export
                r"^(export\s+)?(async\s+)?function\s",
                r"^(export\s+)?class\s",
                r"^(export\s+)?const\s+\w+\s*=\s*(async\s*)?\(",
                // Go: func, type
                r"^func\s",
                r"^type\s",
                // Java/C/C++: public/private/protected class/void/int/String
                r"^(public|private|protected|static)\s+(class|void|int|long|String|bool|float|double)",
                // 通用注释分隔行
                r"^//[-=]{3,}",
                r"^#[-=]{3,}",
            ];
            Regex::new(&patterns.join("|")).unwrap()
        });

        // 按符号行边界切分
        let mut sections: Vec<String> = Vec::new();
        let mut current = String::new();

        for line in text.lines() {
            if CODE_SYMBOL_RE.is_match(line) && !current.trim().is_empty() {
                sections.push(current.trim().to_string());
                current = line.to_string();
            } else {
                if !current.is_empty() {
                    current.push('\n');
                }
                current.push_str(line);
            }
        }
        if !current.trim().is_empty() {
            sections.push(current.trim().to_string());
        }

        // 如果符号切分无效（只有 1 个 section），回退递归分割
        if sections.len() <= 1 {
            let splitter =
                RecursiveCharSplitter::new(self.config.chunk_size, self.config.chunk_overlap);
            return splitter.split(text);
        }

        // 阶段 2：逐个 section 检查，过大则递归细分
        let mut result = Vec::new();
        for section in &sections {
            if section.chars().count() <= self.config.chunk_size {
                result.push(section.to_string());
            } else {
                let splitter =
                    RecursiveCharSplitter::new(self.config.chunk_size, self.config.chunk_overlap);
                let sub = splitter.split(section)?;
                result.extend(sub);
            }
        }

        Ok(result)
    }

    /// Markdown 分割：先按标题切，大 section 再细分
    async fn split_markdown(&self, text: &str) -> Result<Chunks, GBrainError> {
        // 阶段 1：按标题结构切 section
        let header_splitter = MarkdownHeaderSplitter::new();
        let sections = header_splitter.split(text)?;

        // 阶段 2：对每个 section 检查大小，过大则细分
        let mut result = Vec::new();
        for section in sections {
            if section.chars().count() <= LARGE_CHUNK_THRESHOLD {
                // 短块保留
                if !section.trim().is_empty() {
                    result.push(section.trim().to_string());
                }
                continue;
            }

            // 大 section 细分
            let refined = self.refine_large_section(&section).await?;
            result.extend(refined);
        }

        Ok(result)
    }

    /// 通用分割：先按段落/句界切，大段落再细分（P4: 中文句界识别）
    async fn split_generic(&self, text: &str) -> Result<Chunks, GBrainError> {
        // P4: 中文文本先用句界预切（句号、问号、感叹号、分号后换行）
        if crate::nlp::chinese::has_chinese(text) {
            let processed = insert_sentence_breaks(text);
            return self.split_by_paragraphs(&processed).await;
        }
        self.split_by_paragraphs(text).await
    }

    /// 按双换行和中文句界切段落（P4）
    async fn split_by_paragraphs(&self, text: &str) -> Result<Chunks, GBrainError> {
        // 先尝试按双换行切段落
        let paragraphs: Vec<&str> = text
            .split("\n\n")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        if paragraphs.is_empty() {
            return Ok(vec![text.trim().to_string()]);
        }

        // P1 修复: 段落数 <= 1 时，如果文本包含 CJK 且长度超过阈值，
        // 尝试用单 \n 切分（insert_sentence_breaks 插入的是单 \n）。
        // 处理无空行中文、长 OCR、压缩文本等边界场景。
        if paragraphs.len() <= 1 {
            let full = paragraphs.first().copied().unwrap_or(text.trim());
            if full.chars().count() <= LARGE_CHUNK_THRESHOLD {
                return Ok(vec![full.to_string()]);
            }
            // CJK 文本：尝试按单 \n 切分（中文断句后的段落边界）
            if crate::nlp::chinese::has_chinese(full) {
                let single_nl_paras: Vec<&str> = full
                    .split('\n')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect();
                if single_nl_paras.len() > 1 {
                    // 用单 \n 切分后的段落重新走 split_by_paragraphs 逻辑
                    return self.split_by_paragraphs_core(&single_nl_paras).await;
                }
            }
            return self.refine_large_section(full).await;
        }

        self.split_by_paragraphs_core(&paragraphs).await
    }

    /// 提取的多段落核心处理逻辑（按大小分组合并）。
    async fn split_by_paragraphs_core(&self, paragraphs: &[&str]) -> Result<Chunks, GBrainError> {
        // 多段落：按大小分组，同组段落合并为一个 chunk
        let mut result = Vec::new();
        let mut current = String::new();

        for para in paragraphs {
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(para);

            // 当前累积文本超过阈值，先产出
            if current.chars().count() > self.config.chunk_size {
                if current.chars().count() > LARGE_CHUNK_THRESHOLD {
                    // 超过阈值需要细分
                    let refined = self.refine_large_section(&current).await?;
                    result.extend(refined);
                } else if !current.trim().is_empty() {
                    result.push(current.trim().to_string());
                }
                current.clear();
            }
        }

        // 处理剩余内容
        if !current.trim().is_empty() {
            if current.chars().count() > LARGE_CHUNK_THRESHOLD {
                let refined = self.refine_large_section(&current).await?;
                result.extend(refined);
            } else {
                result.push(current.trim().to_string());
            }
        }

        Ok(result)
    }

    /// 对大 section 进行细分：有 embedder 时语义细分，否则递归细分。
    ///
    /// P1 修复: 语义分块后增加硬性 chunk_size/max_chunk_size 校验。
    /// 当 SemanticSplitter 返回空（无法切分）或产出超大 chunk 时，
    /// 回退到 RecursiveCharSplitter 确保每个 chunk 不超过目标大小。
    async fn refine_large_section(&self, section: &str) -> Result<Chunks, GBrainError> {
        if let Some(ref embedder) = self.embedder {
            // 有 embedder → 语义细分
            let semantic = SemanticSplitter::with_config(
                embedder.clone(),
                self.config.chunk_size,
                self.config.chunk_overlap,
            );
            let chunks = semantic.split(section).await?;
            let filtered: Chunks = chunks
                .into_iter()
                .filter(|c| !c.trim().is_empty())
                .collect();

            // P1 修复: 语义分块返回空结果或产出超大 chunk 时，
            // 回退到 recursive char splitter。这处理以下场景：
            // - 无空行中文 / 长 OCR / 压缩文本 / 表格导出文本
            // - 篇幅远超 chunk_size 但只有单个 paragraph unit 的内容
            let max_chunk_size = self
                .config
                .chunk_size
                .saturating_mul(3)
                .max(self.config.chunk_size * 2);
            let needs_recursive_fallback =
                filtered.is_empty() || filtered.iter().any(|c| c.chars().count() > max_chunk_size);

            if needs_recursive_fallback {
                tracing::debug!(
                    semantic_chunks = filtered.len(),
                    section_len = section.chars().count(),
                    chunk_size = self.config.chunk_size,
                    "语义分块产出空/超大 chunk，回退到 recursive char splitter"
                );
                let recursive =
                    RecursiveCharSplitter::new(self.config.chunk_size, self.config.chunk_overlap);
                let fallback = recursive.split(section)?;
                return Ok(fallback
                    .into_iter()
                    .filter(|c| !c.trim().is_empty())
                    .collect());
            }

            Ok(filtered)
        } else {
            // 无 embedder → 递归长度细分
            let recursive =
                RecursiveCharSplitter::new(self.config.chunk_size, self.config.chunk_overlap);
            let chunks = recursive.split(section)?;
            Ok(chunks
                .into_iter()
                .filter(|c| !c.trim().is_empty())
                .collect())
        }
    }
}

impl AsyncDocumentSplitter for AdaptiveSplitter {
    fn split_async<'a>(
        &'a self,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Chunks, GBrainError>> + Send + 'a>> {
        Box::pin(self.split(text))
    }
}

/// 判断是否为表格格式扩展名
fn is_table_extension(ext: &str) -> bool {
    matches!(ext, "csv" | "tsv" | "xlsx" | "xls")
}

/// 判断是否为代码格式扩展名
fn is_code_extension(ext: &str) -> bool {
    matches!(
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
    )
}

/// P4: 中文句界插入换行，使 split_generic 能按句界切分
///
/// 在中文标点符号（。！？；）后插入换行，帮助段落分割器识别句边界。
/// 不处理逗号，避免过度切分。
/// M3 修复：句号后紧跟右括号/引号（」』》）时，在括号后再换行，避免拆分"好的。」"
fn insert_sentence_breaks(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len() + text.len() / 20);
    let mut i = 0;
    while i < chars.len() {
        result.push(chars[i]);
        if matches!(chars[i], '。' | '！' | '？' | '；') {
            // 向前看：如果下一个字符是闭合括号/引号，延迟换行到括号后
            if i + 1 < chars.len()
                && matches!(chars[i + 1], '」' | '』' | '》' | '）' | ')' | '"' | '\'')
            {
                result.push(chars[i + 1]);
                i += 1;
            }
            result.push('\n');
        }
        i += 1;
    }
    result
}

/// 表格/代码文件的分割逻辑
fn split_table_or_code(
    text: &str,
    chunk_size: usize,
    chunk_overlap: usize,
) -> Result<Chunks, GBrainError> {
    // 表格/代码用递归分割，不走语义细分
    let splitter = RecursiveCharSplitter::new(chunk_size, chunk_overlap);
    splitter.split(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_table_extension() {
        assert!(is_table_extension("csv"));
        assert!(is_table_extension("xls"));
        assert!(is_table_extension("xlsx"));
        assert!(!is_table_extension("md"));
        assert!(!is_table_extension("txt"));
    }

    #[test]
    fn test_is_code_extension() {
        assert!(is_code_extension("rs"));
        assert!(is_code_extension("py"));
        assert!(!is_code_extension("md"));
        assert!(!is_code_extension("txt"));
    }

    #[tokio::test]
    async fn test_adaptive_markdown_preserves_sections() {
        // 不同标题 section 不应合并
        let config = AdaptiveConfig {
            extension: "md".to_string(),
            chunk_size: 512,
            chunk_overlap: 50,
        };
        let splitter = AdaptiveSplitter::new(config, None);

        let md = "# 第一节\n\n短内容A\n\n# 第二节\n\n短内容B\n\n# 第三节\n\n短内容C";
        let chunks = splitter.split(md).await.unwrap();

        // 3 个标题应产出 3 个独立 section
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].contains("第一节"));
        assert!(chunks[1].contains("第二节"));
        assert!(chunks[2].contains("第三节"));
    }

    #[tokio::test]
    async fn test_adaptive_small_sections_kept_whole() {
        let config = AdaptiveConfig {
            extension: "md".to_string(),
            chunk_size: 512,
            chunk_overlap: 50,
        };
        let splitter = AdaptiveSplitter::new(config, None);

        let md = "# 小节\n\n这段内容很短，不应被细分。";
        let chunks = splitter.split(md).await.unwrap();

        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("小节"));
    }

    #[tokio::test]
    async fn test_adaptive_large_section_refined_without_embedder() {
        let config = AdaptiveConfig {
            extension: "md".to_string(),
            chunk_size: 200,
            chunk_overlap: 20,
        };
        let splitter = AdaptiveSplitter::new(config, None);

        // MarkdownHeaderSplitter 内部已有大块细分逻辑（MAX_MARKDOWN_CHUNK_CHARS=1600），
        // 所以超大 section 在 header 阶段就会被切分。
        // 构造一个超过 header splitter 阈值的内容
        let long_content: String = "这是第一句话。".repeat(300); // ~2100 字符
        let md = format!("# 大节\n\n{}", long_content);
        let chunks = splitter.split(&md).await.unwrap();

        // MarkdownHeaderSplitter 内部已经将大块细分，所以应得到多个 chunk
        assert!(
            chunks.len() > 1,
            "大 section 应被细分，实际得到 {} 个 chunk",
            chunks.len()
        );
        // 第一个 chunk 应包含标题
        assert!(chunks[0].contains("大节"));
    }

    #[tokio::test]
    async fn test_adaptive_empty_input() {
        let config = AdaptiveConfig {
            extension: "txt".to_string(),
            chunk_size: 512,
            chunk_overlap: 50,
        };
        let splitter = AdaptiveSplitter::new(config, None);
        let chunks = splitter.split("").await.unwrap();
        assert!(chunks.is_empty());
    }

    #[tokio::test]
    async fn test_adaptive_table_skips_semantic() {
        let config = AdaptiveConfig {
            extension: "csv".to_string(),
            chunk_size: 512,
            chunk_overlap: 50,
        };
        let splitter = AdaptiveSplitter::new(config, None);

        let csv = "name,age,city\nAlice,30,Beijing\nBob,25,Shanghai\n";
        let chunks = splitter.split(csv).await.unwrap();
        assert!(!chunks.is_empty());
    }

    #[tokio::test]
    async fn test_adaptive_generic_text() {
        let config = AdaptiveConfig {
            extension: "txt".to_string(),
            chunk_size: 200,
            chunk_overlap: 20,
        };
        let splitter = AdaptiveSplitter::new(config, None);

        let text =
            "第一段内容。这是一些文字。\n\n第二段内容。更多文字在这里。\n\n第三段内容。继续写。";
        let chunks = splitter.split(text).await.unwrap();
        assert!(!chunks.is_empty());
    }

    // ── P4 测试 ──

    #[test]
    fn test_insert_sentence_breaks_chinese() {
        let input = "这是第一句话。这是第二句话！这是第三句话？";
        let result = insert_sentence_breaks(input);
        // 每个句号、感叹号、问号后应有换行
        assert!(result.contains("。\n"));
        assert!(result.contains("！\n"));
        assert!(result.contains("？\n"));
    }

    #[test]
    fn test_insert_sentence_breaks_no_chinese() {
        let input = "Hello world. This is a test!";
        let result = insert_sentence_breaks(input);
        // 英文标点不应插入换行（仅匹配中文标点）
        assert!(!result.contains(".\n"));
        assert!(!result.contains("!\n"));
        // 无中文标点时应与原文相等
        assert_eq!(result, input);
    }

    #[tokio::test]
    async fn test_adaptive_code_splits_on_functions() {
        let config = AdaptiveConfig {
            extension: "rs".to_string(),
            chunk_size: 200,
            chunk_overlap: 20,
        };
        let splitter = AdaptiveSplitter::new(config, None);

        let code = "fn first() {\n    let x = 1;\n}\n\nfn second() {\n    let y = 2;\n}\n\nfn third() {\n    let z = 3;\n}";
        let chunks = splitter.split(code).await.unwrap();
        // 每个函数应该被切分成独立段（或至少各函数边界可识别）
        assert!(!chunks.is_empty());
        // 至少应产生多个 chunk（3 个函数应被分开）
        assert!(
            chunks.len() >= 2,
            "代码应按函数边界切分，得到 {} 个 chunk",
            chunks.len()
        );
    }

    #[tokio::test]
    async fn test_adaptive_pdf_page_split() {
        let config = AdaptiveConfig {
            extension: "pdf".to_string(),
            chunk_size: 500,
            chunk_overlap: 50,
        };
        let splitter = AdaptiveSplitter::new(config, None);

        let pdf_text = "[PAGE:1]\n第一页的内容在这里。\n\n[PAGE:2]\n第二页的内容在这里。\n\n[PAGE:3]\n第三页的内容在这里。";
        let chunks = splitter.split(pdf_text).await.unwrap();
        // PDF 应按页切分
        assert!(
            chunks.len() >= 3,
            "PDF 应按页切分，得到 {} 个 chunk",
            chunks.len()
        );
        assert!(chunks[0].contains("第一页"));
        assert!(chunks[1].contains("第二页"));
        assert!(chunks[2].contains("第三页"));
    }

    #[tokio::test]
    async fn test_adaptive_pdf_no_page_markers_falls_back() {
        let config = AdaptiveConfig {
            extension: "pdf".to_string(),
            chunk_size: 200,
            chunk_overlap: 20,
        };
        let splitter = AdaptiveSplitter::new(config, None);

        // 无 [PAGE:N] 标记的文本，应回退到通用分割
        let text =
            "这是一段没有页面标记的文本。内容可能来自纯文本提取。它应该被正常分割而不是报错。";
        let chunks = splitter.split(text).await.unwrap();
        assert!(!chunks.is_empty());
    }

    #[test]
    fn test_insert_sentence_breaks_semicolon() {
        let input = "第一点；第二点；第三点";
        let result = insert_sentence_breaks(input);
        assert!(result.contains("；\n"));
    }
}
