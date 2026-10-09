//! 关键词搜索（基于 FTS5）
//! 对应 gbrain 的 src/core/search/keyword.ts

use tracing::trace;

/// 从用户查询构建 FTS5 MATCH 表达式。
/// 使用 jieba 分词处理中文查询，支持中英混合输入。
/// 对空查询返回空字符串（调用方需自行处理）。
pub fn build_fts_query(query: &str) -> String {
    trace!(query = %query, "构建 FTS5 查询");
    crate::nlp::chinese::build_fts_match_query(query)
}

/// 转义 FTS5 特殊字符，防止查询语法注入。
/// 移除：引号、括号、花括号、冒号、脱字符、星号、点号、方括号。
/// FTS5 布尔运算符（AND、OR、NOT、NEAR）通过按空白拆分并以显式运算符拼接来处理，
/// 并在 build_fts_query 中对词项加双引号。
pub fn escape_fts_term(term: &str) -> String {
    // 将特殊字符替换为空格（而非直接删除），以保留搜索语义。
    // 例如 "C++" 变为 "C  " → 取第一个词 "C"，"state-of-the-art" → "state of the art" → "state"
    term.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '\'' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches('\'')
        .to_string()
}
