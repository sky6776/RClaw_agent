//! Frontmatter parsing + body split
//! Mirrors gbrain's src/core/markdown.ts
//!
//! Supports YAML frontmatter extraction, timeline sentinel detection,
//! and PageType inference from slug paths.

use crate::types::PageType;
use regex::Regex;
use std::sync::OnceLock;

fn timeline_separator_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^---\s*\n(## (Timeline|History))").unwrap())
}

/// Parsed markdown document with frontmatter and body
#[derive(Debug, Clone)]
pub struct ParsedMarkdown {
    pub frontmatter: serde_json::Value,
    pub body: String,
    pub timeline: String,
}

/// Parse a markdown document with optional YAML frontmatter
pub fn parse_markdown(content: &str) -> ParsedMarkdown {
    let (frontmatter, body) = split_frontmatter(content);
    let (body, timeline) = split_timeline(&body);

    ParsedMarkdown {
        frontmatter,
        body,
        timeline,
    }
}

/// Split YAML frontmatter from body
/// Frontmatter is delimited by `---` at the start of the document
pub fn split_frontmatter(content: &str) -> (serde_json::Value, String) {
    let trimmed = content.trim_start();

    if !trimmed.starts_with("---") {
        return (
            serde_json::Value::Object(Default::default()),
            content.to_string(),
        );
    }

    // Find closing ---
    // Handle both LF (\n---) and CRLF (\r\n---) line endings
    let after_first = &trimmed[3..];
    let end_idx = after_first
        .find("\n---")
        .or_else(|| after_first.find("\r\n---"));
    if let Some(end_idx) = end_idx {
        let yaml_str = after_first[..end_idx].trim();
        // Skip past the closing --- delimiter (handle both \n--- and \r\n---)
        let delimiter_end = if after_first[end_idx..].starts_with("\r\n---") {
            end_idx + 5 // \r\n---
        } else {
            end_idx + 4 // \n---
        };
        let body = after_first[delimiter_end..].trim_start().to_string();

        // Parse YAML frontmatter to JSON
        let frontmatter = parse_yaml_frontmatter(yaml_str);
        return (frontmatter, body);
    }

    (
        serde_json::Value::Object(Default::default()),
        content.to_string(),
    )
}

/// Parse YAML frontmatter string to serde_json::Value
fn parse_yaml_frontmatter(yaml_str: &str) -> serde_json::Value {
    // Use serde_yaml for proper YAML parsing
    match serde_yaml::from_str::<serde_yaml::Value>(yaml_str) {
        Ok(yaml_value) => yaml_to_json(yaml_value),
        Err(e) => {
            // M39: YAML 解析失败时记录警告，帮助排查格式错误的 frontmatter
            tracing::warn!(error = %e, "YAML frontmatter 解析失败，返回空对象");
            serde_json::Value::Object(Default::default())
        }
    }
}

/// Convert serde_yaml::Value to serde_json::Value
fn yaml_to_json(yaml: serde_yaml::Value) -> serde_json::Value {
    match yaml {
        serde_yaml::Value::Null => serde_json::Value::Null,
        serde_yaml::Value::Bool(b) => serde_json::Value::Bool(b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::Value::Number(i.into())
            } else if let Some(f) = n.as_f64() {
                serde_json::json!(f)
            } else {
                serde_json::Value::Null
            }
        }
        serde_yaml::Value::String(s) => serde_json::Value::String(s),
        serde_yaml::Value::Sequence(seq) => {
            serde_json::Value::Array(seq.into_iter().map(yaml_to_json).collect())
        }
        serde_yaml::Value::Mapping(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                if let serde_yaml::Value::String(key) = k {
                    obj.insert(key, yaml_to_json(v));
                }
            }
            serde_json::Value::Object(obj)
        }
        serde_yaml::Value::Tagged(tagged) => yaml_to_json(tagged.value),
    }
}

/// Split body from timeline section
/// Timeline is delimited by: `<!-- timeline -->`, `--- timeline ---`,
/// or `---` immediately before `## Timeline` / `## History`
pub fn split_timeline(body: &str) -> (String, String) {
    // Check for explicit timeline sentinels
    let sentinels = ["<!-- timeline -->", "--- timeline ---"];

    for sentinel in &sentinels {
        if let Some(idx) = body.find(sentinel) {
            let before = body[..idx].trim_end().to_string();
            let after = body[idx + sentinel.len()..].trim_start().to_string();
            return (before, after);
        }
    }

    // Check for `---` before `## Timeline` or `## History`
    let re = timeline_separator_regex();
    if let Some(caps) = re.captures(body) {
        if let Some(m) = caps.get(0) {
            let idx = m.start();
            let before = body[..idx].trim_end().to_string();
            let after = body[idx..].to_string();
            // Remove the leading --- from the timeline section
            let after = after.trim_start_matches('-').trim_start().to_string();
            return (before, after);
        }
    }

    (body.to_string(), String::new())
}

/// Infer PageType from a slug path
/// Mirrors gbrain's inferType() in markdown.ts
pub fn infer_type(slug: &str) -> PageType {
    let slug_lower = slug.to_lowercase();

    if slug_lower.starts_with("people/") || slug_lower.starts_with("person/") {
        PageType::Person
    } else if slug_lower.starts_with("companies/") || slug_lower.starts_with("company/") {
        PageType::Company
    } else if slug_lower.starts_with("deals/") || slug_lower.starts_with("deal/") {
        PageType::Deal
    } else if slug_lower.starts_with("yc/") {
        PageType::Yc
    } else if slug_lower.starts_with("projects/") || slug_lower.starts_with("project/") {
        PageType::Project
    } else if slug_lower.starts_with("concepts/") || slug_lower.starts_with("concept/") {
        PageType::Concept
    } else if slug_lower.starts_with("wiki/analysis/") {
        PageType::Analysis
    } else if slug_lower.starts_with("wiki/guides/") || slug_lower.starts_with("wiki/guide/") {
        PageType::Guide
    } else if slug_lower.starts_with("wiki/hardware/") {
        PageType::Hardware
    } else if slug_lower.starts_with("wiki/architecture/") {
        PageType::Architecture
    } else if slug_lower.starts_with("meetings/") || slug_lower.starts_with("meeting/") {
        PageType::Meeting
    } else if slug_lower.starts_with("writing/") {
        PageType::Writing
    } else if slug_lower.starts_with("media/") {
        PageType::Media
    } else if slug_lower.starts_with("source/") || slug_lower.starts_with("sources/") {
        PageType::Source
    } else if slug_lower.starts_with("code/") {
        PageType::Code
    } else if slug_lower.starts_with("civic/") {
        PageType::Civic
    } else {
        PageType::Note
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_no_frontmatter() {
        let parsed = parse_markdown("Hello world");
        assert!(parsed.frontmatter.is_object());
        assert_eq!(parsed.body, "Hello world");
    }

    #[test]
    fn test_parse_yaml_frontmatter() {
        let content = "---\ntitle: Test\nauthor: Alice\n---\nBody content";
        let parsed = parse_markdown(content);
        assert_eq!(parsed.frontmatter["title"], "Test");
        assert_eq!(parsed.frontmatter["author"], "Alice");
        assert_eq!(parsed.body, "Body content");
    }

    #[test]
    fn test_split_timeline_sentinel() {
        let body = "Main content\n\n<!-- timeline -->\n- 2024-01-01: Event";
        let (main, timeline) = split_timeline(body);
        assert_eq!(main, "Main content");
        assert!(timeline.contains("2024-01-01"));
    }

    #[test]
    fn test_infer_type_people() {
        assert_eq!(infer_type("people/alice"), PageType::Person);
    }

    #[test]
    fn test_infer_type_company() {
        assert_eq!(infer_type("companies/acme"), PageType::Company);
    }

    #[test]
    fn test_infer_type_wiki_analysis() {
        assert_eq!(infer_type("wiki/analysis/market"), PageType::Analysis);
    }

    #[test]
    fn test_infer_type_default() {
        assert_eq!(infer_type("random-slug"), PageType::Note);
    }
}
