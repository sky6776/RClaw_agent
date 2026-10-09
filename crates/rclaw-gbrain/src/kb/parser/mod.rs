//! Document parser registry

pub mod csv;
pub mod docx;
mod embedded_media;
pub mod html;
pub mod markdown;
pub mod pdf;
pub mod text;
pub mod xlsx;

use crate::error::GBrainError;
use crate::kb::types::{MediaRef, ParsedBlock};
use std::collections::HashMap;
use std::sync::Arc;

/// Parsed document result
#[derive(Debug, Clone)]
pub struct ParsedDocument {
    pub content: String,
    pub metadata: HashMap<String, String>,
    /// P1-010: 结构化 block 列表（含 title_path/page/source offsets/block_type）
    /// 解析器应尽量填充此字段；为 None 时 pipeline 回退到 content 字符串
    pub blocks: Option<Vec<ParsedBlock>>,
    /// P1-2: 富文本抽取出的媒体引用（图片/附件），持久化到 kb_media_assets。
    pub media_refs: Vec<MediaRef>,
}

/// P1-2: 富文本标准化结果。
///
/// `markdown` 是给 splitter 用的标准化文本；
/// `media_refs`/`attachments` 在 pipeline 阶段写入数据库。
#[derive(Debug, Clone, Default)]
pub struct NormalizedDocument {
    pub markdown: String,
    pub blocks: Vec<ParsedBlock>,
    pub media_refs: Vec<MediaRef>,
    pub attachments: Vec<MediaRef>,
}

/// P1-2: 富文本标准化器接口。
///
/// HTML parser 提供默认实现，把附件 span、task list、flow/mermaid、
/// 图片等转为 Markdown + `media_refs`。
pub trait RichContentNormalizer: Send + Sync {
    fn normalize(&self, parsed: ParsedDocument) -> crate::error::Result<NormalizedDocument>;
}

/// P1 修复: 默认富文本标准化器 — 不做任何处理，直接透传。
/// 用于 PDF/DOCX/Markdown 等无富文本语义的解析器。
struct IdentityNormalizer;

impl RichContentNormalizer for IdentityNormalizer {
    fn normalize(&self, parsed: ParsedDocument) -> crate::error::Result<NormalizedDocument> {
        Ok(NormalizedDocument {
            markdown: parsed.content,
            blocks: parsed.blocks.unwrap_or_default(),
            media_refs: parsed.media_refs.clone(),
            attachments: parsed.media_refs,
        })
    }
}

/// Document parser trait
pub trait DocumentParser: Send + Sync {
    fn parse(&self, data: &[u8]) -> Result<ParsedDocument, GBrainError>;
    fn extensions(&self) -> &[&str];
}

/// Parser registry: dispatches to the correct parser by extension
pub struct ParserRegistry {
    parsers: HashMap<String, Arc<dyn DocumentParser>>,
    /// P1 修复: 按扩展名注册的富文本标准化器
    normalizers: HashMap<String, Arc<dyn RichContentNormalizer>>,
    fallback: text::TextParser,
    fallback_normalizer: Arc<IdentityNormalizer>,
}

impl Default for ParserRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ParserRegistry {
    pub fn new() -> Self {
        let mut parsers_map: HashMap<String, Arc<dyn DocumentParser>> = HashMap::new();
        let mut normalizers_map: HashMap<String, Arc<dyn RichContentNormalizer>> = HashMap::new();

        let html_parser = Arc::new(html::HtmlParser::new());
        let html_normalizer: Arc<dyn RichContentNormalizer> = Arc::new(html::HtmlParser::new());

        let parsers: Vec<Arc<dyn DocumentParser>> = vec![
            Arc::new(pdf::PdfParser::new()),
            Arc::new(docx::DocxParser::new()),
            Arc::new(xlsx::XlsxParser::new()),
            Arc::new(csv::CsvParser::new()),
            Arc::clone(&html_parser) as Arc<dyn DocumentParser>,
            Arc::new(markdown::MarkdownParser::new()),
        ];

        for parser in parsers {
            for ext in parser.extensions() {
                parsers_map.insert(ext.to_string(), Arc::clone(&parser));
            }
        }

        // 为 HTML 扩展名注册富文本标准化器
        for ext in html_parser.extensions() {
            normalizers_map.insert(ext.to_string(), Arc::clone(&html_normalizer));
        }

        Self {
            parsers: parsers_map,
            normalizers: normalizers_map,
            fallback: text::TextParser::new(),
            fallback_normalizer: Arc::new(IdentityNormalizer),
        }
    }

    pub fn parse(&self, ext: &str, data: &[u8]) -> Result<ParsedDocument, GBrainError> {
        let key = ext.to_lowercase();
        match self.parsers.get(&key) {
            Some(p) => p.parse(data),
            None => self.fallback.parse(data),
        }
    }

    /// P1 修复: 获取富文本标准化器，用于将 ParsedDocument 转为
    /// NormalizedDocument（Markdown + structured blocks + media_refs）。
    pub fn get_normalizer(&self, ext: &str) -> &dyn RichContentNormalizer {
        let key = ext.to_lowercase();
        self.normalizers
            .get(&key)
            .map(|n| n.as_ref() as &dyn RichContentNormalizer)
            .unwrap_or(self.fallback_normalizer.as_ref() as &dyn RichContentNormalizer)
    }
}
