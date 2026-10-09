//! 文档大小分类 — 决定处理策略（全文/段落/章节/结构化）
//!
//! 规则初版：
//! - xls/xlsx/csv → Table
//! - 0-800 chars → Micro
//! - 801-3000 chars → Small
//! - 3001-30000 chars → Medium
//! - >30000 chars 或 page_count > 30 → Large

use serde::{Deserialize, Serialize};

/// 文档粒度分类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentGranularity {
    /// 表格文档（xls/xlsx/csv），有结构化的行和列
    Table,
    /// 0-800 字符：作为单个 whole-document node 处理
    Micro,
    /// 801-3000 字符：whole-document node + 少量段落 nodes
    Small,
    /// 3001-30000 字符：按章节分块
    Medium,
    /// >30000 字符或 >30 页：结构化章节 + 分块
    Large,
}

impl DocumentGranularity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::Micro => "micro",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }

    pub fn parse_granularity(s: &str) -> Self {
        match s {
            "table" => Self::Table,
            "micro" => Self::Micro,
            "small" => Self::Small,
            "medium" => Self::Medium,
            "large" => Self::Large,
            _ => Self::Micro, // 安全默认值
        }
    }
}

/// 根据文档特征判定粒度。
///
/// # 参数
/// - `extension`: 文件扩展名（小写），用于检测表格格式
/// - `char_count`: 文本字符数
/// - `page_count`: PDF 等格式的页数（非页面型格式传 0）
pub fn classify_granularity(
    extension: &str,
    char_count: usize,
    page_count: usize,
) -> DocumentGranularity {
    // 表格检测（按扩展名）
    if matches!(extension, "xls" | "xlsx" | "csv") {
        return DocumentGranularity::Table;
    }

    // char_count==0 通常意味着内容提取失败（空文件/解析异常），归为 Micro 作为兜底。
    // 调用方应在上游检查空内容并跳过，而非依赖此处；若未来需要区分"空"与"极短"，
    // 可返回 Option<DocumentGranularity> 并在 None 时由调用方决定处理策略。
    if char_count == 0 {
        return DocumentGranularity::Micro;
    }

    // 大文档判定：字符数 > 30000 或页数 > 30
    if page_count > 30 || char_count > 30000 {
        return DocumentGranularity::Large;
    }

    if char_count > 3000 {
        return DocumentGranularity::Medium;
    }

    if char_count > 800 {
        return DocumentGranularity::Small;
    }

    DocumentGranularity::Micro
}

/// 根据粒度返回推荐的分块策略标识。
pub fn chunk_strategy_for(granularity: DocumentGranularity) -> &'static str {
    match granularity {
        DocumentGranularity::Table => "table",
        DocumentGranularity::Micro => "whole_document",
        DocumentGranularity::Small => "whole_document_plus_paragraphs",
        DocumentGranularity::Medium => "recursive_semantic",
        DocumentGranularity::Large => "structured_sections",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_micro() {
        assert_eq!(
            classify_granularity("txt", 100, 1),
            DocumentGranularity::Micro
        );
        assert_eq!(
            classify_granularity("md", 800, 1),
            DocumentGranularity::Micro
        );
    }

    #[test]
    fn test_classify_small() {
        assert_eq!(
            classify_granularity("txt", 2000, 1),
            DocumentGranularity::Small
        );
        assert_eq!(
            classify_granularity("md", 3000, 5),
            DocumentGranularity::Small
        );
    }

    #[test]
    fn test_classify_medium() {
        assert_eq!(
            classify_granularity("pdf", 5000, 10),
            DocumentGranularity::Medium
        );
    }

    #[test]
    fn test_classify_large_by_chars() {
        assert_eq!(
            classify_granularity("md", 100000, 1),
            DocumentGranularity::Large
        );
    }

    #[test]
    fn test_classify_large_by_pages() {
        assert_eq!(
            classify_granularity("pdf", 5000, 40),
            DocumentGranularity::Large
        );
    }

    #[test]
    fn test_classify_table() {
        assert_eq!(
            classify_granularity("xls", 10000, 0),
            DocumentGranularity::Table
        );
        assert_eq!(
            classify_granularity("xlsx", 0, 0),
            DocumentGranularity::Table
        );
        assert_eq!(
            classify_granularity("csv", 10000, 0),
            DocumentGranularity::Table
        );
    }

    #[test]
    fn test_chunk_strategies() {
        assert_eq!(
            chunk_strategy_for(DocumentGranularity::Micro),
            "whole_document"
        );
        assert_eq!(
            chunk_strategy_for(DocumentGranularity::Small),
            "whole_document_plus_paragraphs"
        );
        assert_eq!(
            chunk_strategy_for(DocumentGranularity::Medium),
            "recursive_semantic"
        );
        assert_eq!(
            chunk_strategy_for(DocumentGranularity::Large),
            "structured_sections"
        );
        assert_eq!(chunk_strategy_for(DocumentGranularity::Table), "table");
    }

    #[test]
    fn test_roundtrip() {
        for g in [
            DocumentGranularity::Table,
            DocumentGranularity::Micro,
            DocumentGranularity::Small,
            DocumentGranularity::Medium,
            DocumentGranularity::Large,
        ] {
            assert_eq!(DocumentGranularity::parse_granularity(g.as_str()), g);
        }
    }
}
