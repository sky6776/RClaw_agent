//! Contextual Embedding 文本生成
//!
//! Embedding 阶段使用带文档标题、章节路径、页码等上下文的增强文本，
//! 搜索结果展示仍使用原始 content，两者分离。

/// 构建用于 embedding 的上下文增强文本。
///
/// 格式：
/// ```text
/// 文档：{document_title}  （根据 title_weight 可能重复多次）
/// 章节：{title_path}
/// 页码：{page_number}
/// 内容：{content}
/// ```
///
/// 空字段会自动省略对应行。
///
/// `title_weight` 控制标题在 embedding 文本中的权重（0.0-1.0）。
/// 权重越高，标题重复次数越多，文档级检索越准确。
/// - 0.0: 不重复标题（仅出现一次）
/// - 0.2: 标题重复 1 次（默认）
/// - 0.5: 标题重复 2 次
/// - 1.0: 标题重复 3 次
pub fn build_embedding_text(
    document_title: &str,
    title_path: &str,
    page_number: Option<i32>,
    content: &str,
    title_weight: f32,
) -> String {
    let mut parts = Vec::with_capacity(6);

    if !document_title.is_empty() {
        // 根据 title_weight 计算标题重复次数，增加标题在 embedding 中的权重
        // 防御性处理：NaN 视为 0.0，然后 clamp 防止负数或极大值
        let w = if title_weight.is_nan() {
            0.0
        } else {
            title_weight
        };
        let repeat = if w > 0.0 {
            (w * 3.0).ceil().clamp(1.0, 3.0) as usize
        } else {
            1
        };
        for _ in 0..repeat {
            parts.push(format!("文档：{}", document_title));
        }
    }
    if !title_path.is_empty() {
        parts.push(format!("章节：{}", title_path));
    }
    if let Some(pn) = page_number {
        if pn > 0 {
            parts.push(format!("页码：{}", pn));
        }
    }
    parts.push(format!("内容：{}", content));

    parts.join("\n")
}

/// 为 micro 文档（全文节点）构建 embedding 文本。
pub fn build_micro_embedding_text(
    document_title: &str,
    content: &str,
    title_weight: f32,
) -> String {
    build_embedding_text(document_title, "", None, content, title_weight)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_full_context() {
        let text = build_embedding_text("测试文档", "第一章 / 概述", Some(42), "这是正文内容", 0.2);
        assert!(text.contains("文档：测试文档"));
        assert!(text.contains("章节：第一章 / 概述"));
        assert!(text.contains("页码：42"));
        assert!(text.contains("内容：这是正文内容"));
    }

    #[test]
    fn test_build_minimal() {
        let text = build_embedding_text("", "", None, "正文", 0.2);
        assert_eq!(text, "内容：正文");
    }

    #[test]
    fn test_build_micro() {
        let text = build_micro_embedding_text("微型文档", "全文内容", 0.2);
        assert!(text.contains("文档：微型文档"));
        assert!(text.contains("内容：全文内容"));
        assert!(!text.contains("章节："));
        assert!(!text.contains("页码："));
    }

    #[test]
    fn test_page_zero_skipped() {
        let text = build_embedding_text("标题", "路径", Some(0), "内容", 0.2);
        assert!(!text.contains("页码"));
    }

    #[test]
    fn test_title_weight_zero() {
        // 权重 0.0 时标题仍出现 1 次
        let text = build_embedding_text("标题", "", None, "内容", 0.0);
        assert_eq!(text.matches("文档：标题").count(), 1);
    }

    #[test]
    fn test_title_weight_high() {
        // 权重 1.0 时标题重复 3 次
        let text = build_embedding_text("标题", "", None, "内容", 1.0);
        assert_eq!(text.matches("文档：标题").count(), 3);
    }

    #[test]
    fn test_title_weight_medium() {
        // 权重 0.5 时标题重复 2 次 (ceil(0.5*3) = 2)
        let text = build_embedding_text("标题", "", None, "内容", 0.5);
        assert_eq!(text.matches("文档：标题").count(), 2);
    }
}
