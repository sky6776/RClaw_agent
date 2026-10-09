//! 文档元数据抽取框架
//!
//! 定义统一的 DocumentMetadata 结构体，为每种文件格式实现元数据抽取，
//! 并在 pipeline 中写入 kb_documents 的相应字段。

use std::path::Path;

/// 抽取的文档元数据
#[derive(Debug, Clone, Default)]
pub struct DocumentMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub source_uri: Option<String>,
    pub source_path: Option<String>,
    pub modified_at: Option<String>,
    pub document_date: Option<String>,
    pub keywords: Option<String>,
    pub entity_names: Option<String>,
    pub language: Option<String>,
    pub page_count: i32,
}

impl DocumentMetadata {
    /// 从文件系统路径提取元数据（modified_at、source_path、source_uri）
    pub fn from_file_path(path: &Path) -> Self {
        let modified_at = std::fs::metadata(path).ok().and_then(|m| {
            m.modified().ok().map(|t| {
                chrono::DateTime::<chrono::Utc>::from(t)
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string()
            })
        });

        let source_path = Some(path.to_string_lossy().to_string());
        let source_uri = Some(format!("file://{}", path.to_string_lossy()));

        // 注意：不从文件名推断 title。KB 存储路径是 hash 文件名（如 a1b2c3...），
        // file_stem 会得到无意义的 hash 值。title 应来自：
        // 1. 格式特定提取（markdown frontmatter、PDF Info、HTML <title>、DOCX core.xml）
        // 2. 回退到 kb_documents.original_name（由 resolve_doc_title 处理）

        DocumentMetadata {
            title: None,
            modified_at,
            source_path,
            source_uri,
            ..Default::default()
        }
    }

    /// 从 parser 在 parse() 阶段提取的 metadata HashMap 构造 DocumentMetadata。
    /// HTML parser 已正确解析原始 HTML 的 title/meta 标签（大小写不敏感、
    /// 属性顺序无关、fallback 到第二个 meta），此处直接映射到入库字段，
    /// 避免对已抽取的纯文本做无效的 HTML 标签匹配。
    pub fn from_parser_metadata(meta: &std::collections::HashMap<String, String>) -> Self {
        let mut doc = DocumentMetadata::default();
        if let Some(title) = meta.get("title") {
            doc.title = Some(title.clone());
        }
        // description 作为 keywords 的 fallback（与原 extract_html_metadata 行为一致）
        if let Some(desc) = meta.get("description") {
            doc.keywords = Some(desc.clone());
        }
        if let Some(kw) = meta.get("keywords") {
            doc.keywords = Some(kw.clone());
        }
        if let Some(author) = meta.get("author") {
            doc.author = Some(author.clone());
        }
        doc
    }

    /// 以高优先级字段覆盖低优先级字段
    pub fn merge_with(&mut self, other: &DocumentMetadata) {
        if other.title.is_some() {
            self.title = other.title.clone();
        }
        if other.author.is_some() {
            self.author = other.author.clone();
        }
        if other.modified_at.is_some() {
            self.modified_at = other.modified_at.clone();
        }
        if other.document_date.is_some() {
            self.document_date = other.document_date.clone();
        }
        if other.keywords.is_some() {
            self.keywords = other.keywords.clone();
        }
        if other.source_uri.is_some() {
            self.source_uri = other.source_uri.clone();
        }
        if other.language.is_some() {
            self.language = other.language.clone();
        }
        if other.page_count > 0 {
            self.page_count = other.page_count;
        }
    }
}

// --- Format-specific extractors ---

/// 从 Markdown 内容提取元数据（YAML frontmatter + 第一个 H1）
pub fn extract_markdown_metadata(content: &str, _raw_data: &[u8]) -> DocumentMetadata {
    let mut meta = DocumentMetadata::default();

    // 尝试解析 YAML frontmatter（以 --- 开头和结尾）
    if let Some(frontmatter) = extract_frontmatter(content) {
        // 简单行解析：key: value
        for line in frontmatter.lines() {
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim().to_lowercase();
                let value = value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string();
                if value.is_empty() {
                    continue;
                }
                match key.as_str() {
                    "title" => meta.title = Some(value),
                    "author" => meta.author = Some(value),
                    "date" => meta.document_date = Some(value),
                    "tags" | "keywords" => meta.keywords = Some(value),
                    "lang" | "language" => meta.language = Some(value),
                    _ => {}
                }
            }
        }
    }

    // Fallback: 使用第一个 H1 作为 title
    if meta.title.is_none() {
        meta.title = find_first_h1(content);
    }

    meta
}

/// 从 PDF 原始数据提取元数据
pub fn extract_pdf_metadata(_content: &str, raw_data: &[u8]) -> DocumentMetadata {
    let mut meta = DocumentMetadata::default();

    // 尝试解析 PDF Info dictionary
    if let Ok(doc) = lopdf::Document::load_mem(raw_data) {
        // 读取 PDF Info 字典
        if let Ok(lopdf::Object::Dictionary(info_dict)) = doc.trailer.get(b"Info").cloned() {
            if let Some(title) = get_pdf_str(&info_dict, b"Title") {
                meta.title = Some(title);
            }
            if let Some(author) = get_pdf_str(&info_dict, b"Author") {
                meta.author = Some(author);
            }
            if let Some(subject) = get_pdf_str(&info_dict, b"Subject") {
                if meta.keywords.is_none() {
                    meta.keywords = Some(subject);
                }
            }
            if let Some(kw) = get_pdf_str(&info_dict, b"Keywords") {
                meta.keywords = Some(kw);
            }
            if let Some(date) = get_pdf_str(&info_dict, b"CreationDate") {
                meta.document_date = Some(date);
            }
        }
        // 总页数
        meta.page_count = doc.page_iter().count() as i32;
    }

    meta
}

fn get_pdf_str(dict: &lopdf::Dictionary, key: &[u8]) -> Option<String> {
    dict.get(key).ok().and_then(|obj| match obj {
        lopdf::Object::String(s, _) => String::from_utf8(s.clone()).ok(),
        lopdf::Object::Name(name) => String::from_utf8(name.clone()).ok(),
        _ => None,
    })
}

/// 从 DOCX 原始数据提取元数据（core.xml properties）
pub fn extract_docx_metadata(_content: &str, raw_data: &[u8]) -> DocumentMetadata {
    let mut meta = DocumentMetadata::default();

    let reader = std::io::Cursor::new(raw_data);
    if let Ok(mut archive) = zip::ZipArchive::new(reader) {
        // 尝试读取 docProps/core.xml
        if let Ok(mut file) = archive.by_name("docProps/core.xml") {
            let mut buf = Vec::new();
            if std::io::Read::read_to_end(&mut file, &mut buf).is_ok() {
                if let Ok(xml_str) = std::str::from_utf8(&buf) {
                    meta = parse_docx_core_props(xml_str, meta);
                }
            }
        }
    }

    meta
}

fn parse_docx_core_props(xml: &str, mut meta: DocumentMetadata) -> DocumentMetadata {
    // 简单 XML 标签提取（避免引入完整 XML 解析器）
    let tags = [
        ("dc:title", "title"),
        ("dcterms:title", "title"),
        ("dc:creator", "author"),
        ("dcterms:creator", "author"),
        ("dc:subject", "keywords"),
        ("dcterms:subject", "keywords"),
        ("dc:description", ""),
        ("dcterms:created", "document_date"),
        ("dcterms:modified", "modified_at"),
    ];

    for (tag, field) in &tags {
        if let Some(val) = extract_xml_tag_content(xml, tag) {
            match *field {
                "title" if meta.title.is_none() => meta.title = Some(val),
                "author" if meta.author.is_none() => meta.author = Some(val),
                "keywords" if meta.keywords.is_none() => meta.keywords = Some(val),
                "modified_at" if meta.modified_at.is_none() => meta.modified_at = Some(val),
                "document_date" if meta.document_date.is_none() => meta.document_date = Some(val),
                "" => {} // dc:description — 暂不映射到具体字段
                _ => {}
            }
        }
    }

    meta
}

/// 从 HTML 内容提取元数据（title + meta tags）
pub fn extract_html_metadata(content: &str, _raw_data: &[u8]) -> DocumentMetadata {
    let mut meta = DocumentMetadata::default();

    // 提取 <title>
    if let Some(title) = extract_tag_content(content, "title") {
        meta.title = Some(title);
    }

    // 提取 <meta> 标签
    if let Some(desc) = extract_meta_content(content, "description") {
        if meta.keywords.is_none() {
            meta.keywords = Some(desc);
        }
    }
    if let Some(kw) = extract_meta_content(content, "keywords") {
        meta.keywords = Some(kw);
    }
    if let Some(author) = extract_meta_content(content, "author") {
        meta.author = Some(author);
    }

    meta
}

/// 从文本内容提取关键词和实体（初版：词频 + 规则）
pub fn extract_keywords_and_entities(content: &str, _language: &str) -> (String, String) {
    // 使用 jieba 分词取高频词作为关键词
    use jieba_rs::Jieba;
    static JIEBA: std::sync::OnceLock<Jieba> = std::sync::OnceLock::new();
    let jieba = JIEBA.get_or_init(Jieba::new);
    // HMM=true 启用隐马尔可夫模型，对新词（未登录词）识别更好，提升关键词质量
    let words: Vec<&str> = jieba.cut(content, true).into_iter().collect();

    // 停用词
    let stopwords: &[&str] = &[
        "的", "了", "在", "是", "我", "有", "和", "就", "不", "人", "都", "一", "一个", "上", "也",
        "很", "到", "说", "要", "去", "你", "会", "着", "没有", "看", "好", "自己", "这", "他",
        "她", "它", "们", "那", "些", "the", "a", "an", "is", "are", "was", "were", "be", "been",
        "being", "have", "has", "had", "do", "does", "did", "will", "would", "could", "should",
        "may", "might", "can", "shall", "to", "of", "in", "for", "on", "with", "at", "by", "from",
        "as", "into", "through", "during", "and", "but", "or", "nor", "not", "so", "yet", "both",
        "either",
    ];
    let stopwords_set: std::collections::HashSet<&str> = stopwords.iter().copied().collect();

    // 计算词频
    let mut freq: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for w in &words {
        let w_trimmed = w.trim();
        if w_trimmed.len() < 2 || stopwords_set.contains(w_trimmed) {
            continue;
        }
        *freq.entry(w_trimmed).or_insert(0) += 1;
    }

    // M9 修复：关键词数量按文档长度动态调整
    // 基础 5 个，每 5000 字增加 1 个，上限 15 个
    let content_char_count = content.chars().count();
    let keyword_count = (5 + (content_char_count / 5000).min(10)).min(15);
    let mut sorted: Vec<(&str, usize)> = freq.into_iter().collect();
    sorted.sort_by_key(|b| std::cmp::Reverse(b.1));
    let keywords: Vec<String> = sorted
        .iter()
        .take(keyword_count)
        .map(|(w, _)| w.to_string())
        .collect();

    // 实体初版：规则匹配（邮箱、日期、金额等模式）
    let mut entities: Vec<String> = Vec::new();
    // 日期模式：YYYY-MM-DD, YYYY/MM/DD, YYYY年MM月DD日
    let date_re = regex::Regex::new(r"\d{4}[-/年]\d{1,2}[-/月]\d{1,2}[日]?").unwrap();
    for m in date_re.find_iter(content).take(5) {
        entities.push(format!("DATE:{}", m.as_str()));
    }
    // 邮箱模式
    let email_re = regex::Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}").unwrap();
    for m in email_re.find_iter(content).take(5) {
        entities.push(format!("EMAIL:{}", m.as_str()));
    }

    (keywords.join(","), entities.join(","))
}

// --- Helper functions ---

/// 提取 YAML frontmatter（以 --- 包裹的顶部块）
fn extract_frontmatter(content: &str) -> Option<String> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_first = &trimmed[3..];
    if let Some(end_idx) = after_first.find("---") {
        return Some(after_first[..end_idx].trim().to_string());
    }
    None
}

/// 在 Markdown 中查找第一个 H1 标题
fn find_first_h1(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("# ") && !trimmed.starts_with("## ") {
            return Some(trimmed[2..].trim().to_string());
        }
    }
    None
}

/// 提取 XML 标签内容（简单正则实现）
fn extract_xml_tag_content(xml: &str, tag: &str) -> Option<String> {
    let pattern = format!(
        r"<{}[^>]*>(.*?)</{}>",
        regex::escape(tag),
        regex::escape(tag)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    re.captures(xml)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().trim().to_string())
}

/// 提取 HTML 标签内容
fn extract_tag_content(html: &str, tag: &str) -> Option<String> {
    let pattern = format!(
        r"<{}[^>]*>(.*?)</{}>",
        regex::escape(tag),
        regex::escape(tag)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    re.captures(html)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().trim().to_string())
}

/// 提取 HTML <meta name="X" content="Y"> 标签
fn extract_meta_content(html: &str, name: &str) -> Option<String> {
    let pattern = format!(
        r#"<meta\s+name=["']{}["']\s+content=["']([^"']+)["']"#,
        regex::escape(name)
    );
    let re = regex::Regex::new(&pattern).unwrap();
    re.captures(html)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_frontmatter() {
        let md = "---\ntitle: Test Doc\nauthor: Alice\ndate: 2024-01-15\n---\n\n# Content";
        let meta = extract_markdown_metadata(md, &[]);
        assert_eq!(meta.title.as_deref(), Some("Test Doc"));
        assert_eq!(meta.author.as_deref(), Some("Alice"));
        assert_eq!(meta.document_date.as_deref(), Some("2024-01-15"));
    }

    #[test]
    fn test_h1_fallback() {
        let md = "# My Document\n\nSome text";
        let meta = extract_markdown_metadata(md, &[]);
        assert_eq!(meta.title.as_deref(), Some("My Document"));
    }

    #[test]
    fn test_extract_html_title() {
        let html = "<html><head><title>Test Page</title></head><body></body></html>";
        let meta = extract_html_metadata(html, &[]);
        assert_eq!(meta.title.as_deref(), Some("Test Page"));
    }

    #[test]
    fn test_extract_html_meta() {
        let html =
            r#"<html><head><meta name="description" content="A great article"></head></html>"#;
        let meta = extract_html_metadata(html, &[]);
        assert_eq!(meta.keywords.as_deref(), Some("A great article"));
    }

    #[test]
    fn test_filename_title_not_set_from_storage_path() {
        // 存储路径是 hash 文件名，不应从中推断 title
        let meta = DocumentMetadata::from_file_path(Path::new("/data/a1b2c3d4.txt"));
        assert_eq!(meta.title, None);
        // source_path 仍然正确设置
        assert!(meta.source_path.is_some());
    }

    #[test]
    fn test_keyword_extraction() {
        let (keywords, entities) = extract_keywords_and_entities(
            "机器学习是人工智能的一个分支，机器学习技术正在改变世界。深度学习是机器学习的重要方法。",
            "zh",
        );
        // 关键词不应为空
        assert!(!keywords.is_empty(), "keywords should not be empty");
        // 应包含高频词（jieba 分词可能将"机器学习"拆分为多个 token）
        assert!(
            keywords.contains("学习") || keywords.contains("机器") || keywords.contains("人工智能")
        );
        // 无邮箱时 entities 应为空
        assert!(!entities.contains("EMAIL:"));
    }

    #[test]
    fn test_metadata_merge() {
        let mut base = DocumentMetadata::default();
        let override_meta = DocumentMetadata {
            title: Some("Override Title".into()),
            author: Some("Bob".into()),
            ..Default::default()
        };
        base.merge_with(&override_meta);
        assert_eq!(base.title.as_deref(), Some("Override Title"));
        assert_eq!(base.author.as_deref(), Some("Bob"));
    }

    #[test]
    fn test_merge_preserves_existing() {
        let mut base = DocumentMetadata {
            title: Some("Original".into()),
            ..Default::default()
        };
        let empty = DocumentMetadata::default();
        base.merge_with(&empty);
        assert_eq!(base.title.as_deref(), Some("Original"));
    }
}
