//! Query Planner — 按查询类型选择检索策略和权重 (P3-006, P3-007)
//!
//! 基于规则的初版分类器，后续可接入 ML 模型。

use serde::{Deserialize, Serialize};

/// 查询类型分类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryType {
    /// 精确查找：编号、代码、短关键词
    ExactLookup,
    /// 流程/操作指南："怎么/如何/流程/步骤"
    HowTo,
    /// 事实查询："是什么/定义/概念"
    FactLookup,
    /// 概念/开放式搜索
    Conceptual,
    /// 表格查询：含 "sheet/表/清单/xls/xlsx/csv"
    TableLookup,
    /// 时间范围查询：含日期/时间词
    RecentOrTimebound,
    /// 小文档优先
    SmallDocument,
}

impl QueryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ExactLookup => "exact_lookup",
            Self::HowTo => "how_to",
            Self::FactLookup => "fact_lookup",
            Self::Conceptual => "conceptual",
            Self::TableLookup => "table_lookup",
            Self::RecentOrTimebound => "recent_or_timebound",
            Self::SmallDocument => "small_document",
        }
    }
}

/// Retriever 类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetrieverType {
    TitleName,
    NodeFts,
    /// P1 修复: PassageFts 检索器（kb_passage_fts），
    /// 对短查询/keyword 提供段落级 FTS 兜底召回
    PassageFts,
    Vector,
    Summary,
    Table,
    Metadata,
}

/// Planner 输出：retriever 组合 + 权重
#[derive(Debug, Clone)]
pub struct PlannerOutput {
    pub query_type: QueryType,
    pub retrievers: Vec<(RetrieverType, f64)>,
}

/// 判断查询是否看起来像精确文件名/ID/路径查找（而非概念查询）。
///
/// P2 修复: 短查询不再无条件归为 ExactLookup。
/// 只有明显匹配文件名/扩展名/路径/编号/代码模式的查询才走精确查找，
/// "RAG"、"OCR"、"向量检索" 这类短概念查询保留语义召回能力。
fn is_exact_lookup_pattern(q: &str) -> bool {
    // 文件扩展名模式: report.pdf, data.xlsx, document.docx 等
    if q.contains('.')
        && q.split('.').next_back().is_some_and(|ext| {
            ext.len() >= 2 && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphabetic())
        })
    {
        return true;
    }

    // 路径模式: /path/to/file 或 \path\to\file 或 docs/api-guide
    if q.contains('/') || q.contains('\\') {
        return true;
    }

    // 纯编号/ID 模式: 工单号 "TICKET-1234", "ABC-123", 纯数字, 或类似代码格式
    let is_code_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '#';
    if q.chars().all(&is_code_char) {
        return q.chars().any(|c| c.is_ascii_digit()); // 至少包含一个数字
    }

    // 版本号/日期模式: v2.1, 2024-Q3, rev3
    let has_version_like =
        q.starts_with('v') && q.chars().skip(1).all(|c| c.is_ascii_digit() || c == '.');
    if has_version_like && q.len() >= 3 {
        return true;
    }

    // 其他模式不视为精确查找
    false
}

/// 基于规则识别查询类型
pub fn classify_query(query: &str) -> QueryType {
    let q = query.trim().to_lowercase();

    // 表格查询模式
    if q.contains("表")
        || q.contains("sheet")
        || q.contains("清单")
        || q.contains("xls")
        || q.contains("xlsx")
        || q.contains("csv")
        || q.contains("汇总")
    {
        return QueryType::TableLookup;
    }

    // 流程/操作指南
    if q.contains("怎么") || q.contains("如何") || q.contains("流程") || q.contains("步骤")
    {
        return QueryType::HowTo;
    }

    // 时间范围
    if q.contains("年")
        || q.contains("月")
        || q.contains("日")
        || q.contains("最近")
        || q.contains("去年")
        || q.contains("上季度")
        || q.contains("q1")
        || q.contains("q2")
    {
        return QueryType::RecentOrTimebound;
    }

    // 事实查询
    if q.contains("是什么") || q.contains("定义") || q.ends_with('?') || q.ends_with('？') {
        return QueryType::FactLookup;
    }

    // P2 修复: 精确查找只给明显文件名/ID/路径模式的查询，
    // 短概念查询（"RAG"、"OCR"、"向量检索" 等）归为 Conceptual 以保留语义召回。
    // 不再简单按长度 < 8 一刀切，而是用 is_exact_lookup_pattern 语义判断。
    if is_exact_lookup_pattern(&q) {
        return QueryType::ExactLookup;
    }

    QueryType::Conceptual
}

/// 根据查询类型输出 retriever 组合和权重
pub fn plan(query_type: QueryType) -> PlannerOutput {
    let retrievers = match query_type {
        // P2 修复: ExactLookup 保留低权重 Vector 和 PassageFts 召回。
        // 即使文件名/ID 模式匹配，关键词 retriever 返回弱结果时也需要语义兜底。
        QueryType::ExactLookup => vec![
            (RetrieverType::TitleName, 0.5),
            (RetrieverType::Metadata, 0.2),
            (RetrieverType::NodeFts, 0.2),
            (RetrieverType::Vector, 0.15),
            (RetrieverType::PassageFts, 0.1),
        ],
        QueryType::HowTo => vec![
            (RetrieverType::NodeFts, 0.3),
            (RetrieverType::Vector, 0.3),
            (RetrieverType::Summary, 0.2),
            (RetrieverType::TitleName, 0.2),
            (RetrieverType::PassageFts, 0.15),
        ],
        QueryType::FactLookup => vec![
            (RetrieverType::NodeFts, 0.4),
            (RetrieverType::Vector, 0.3),
            (RetrieverType::Metadata, 0.3),
            (RetrieverType::PassageFts, 0.2),
        ],
        QueryType::Conceptual => vec![
            (RetrieverType::Vector, 0.5),
            (RetrieverType::NodeFts, 0.3),
            (RetrieverType::Summary, 0.2),
            (RetrieverType::PassageFts, 0.1),
        ],
        QueryType::TableLookup => vec![
            (RetrieverType::Table, 0.5),
            (RetrieverType::NodeFts, 0.3),
            (RetrieverType::Metadata, 0.2),
        ],
        QueryType::RecentOrTimebound => vec![
            (RetrieverType::NodeFts, 0.4),
            (RetrieverType::Metadata, 0.4),
            (RetrieverType::Vector, 0.2),
        ],
        QueryType::SmallDocument => vec![
            (RetrieverType::TitleName, 0.4),
            (RetrieverType::NodeFts, 0.3),
            (RetrieverType::Vector, 0.3),
        ],
    };

    PlannerOutput {
        query_type,
        retrievers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_how_to() {
        assert_eq!(classify_query("差旅报销怎么申请"), QueryType::HowTo);
        assert_eq!(classify_query("如何配置SSL证书"), QueryType::HowTo);
    }

    #[test]
    fn test_classify_table() {
        assert_eq!(classify_query("2024 Q3 报销清单"), QueryType::TableLookup);
        assert_eq!(classify_query("导出xls报表"), QueryType::TableLookup);
        assert_eq!(classify_query("导出xlsx报表"), QueryType::TableLookup);
    }

    #[test]
    fn test_classify_exact() {
        // 字母数字代码模式仍应归类为 ExactLookup
        assert_eq!(classify_query("ABC-123"), QueryType::ExactLookup);
        // 文件扩展名模式
        assert_eq!(classify_query("report.pdf"), QueryType::ExactLookup);
        // 纯数字编号
        assert_eq!(classify_query("12345"), QueryType::ExactLookup);
    }

    #[test]
    fn test_classify_conceptual() {
        assert_eq!(
            classify_query("机器学习在自然语言处理中的应用"),
            QueryType::Conceptual
        );
        // P2 修复: 短概念查询不再归为 ExactLookup
        assert_eq!(classify_query("RAG"), QueryType::Conceptual);
        assert_eq!(classify_query("OCR"), QueryType::Conceptual);
        assert_eq!(classify_query("向量检索"), QueryType::Conceptual);
    }

    #[test]
    fn test_plan_output() {
        let plan = plan(QueryType::ExactLookup);
        // P2 修复: ExactLookup 现在包含 5 个 retriever（增加 Vector + PassageFts）
        assert_eq!(plan.retrievers.len(), 5);
        assert!((plan.retrievers[0].1 - 0.5).abs() < 0.01); // TitleName 权重降为 0.5
    }
}
