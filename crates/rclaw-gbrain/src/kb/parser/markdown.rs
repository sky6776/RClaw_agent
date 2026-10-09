//! Markdown parser (passthrough — structure handled by splitter)

use super::{DocumentParser, ParsedDocument};
use crate::error::GBrainError;
use crate::kb::types::MediaRef;
use std::collections::HashMap;
use std::sync::LazyLock;

static MD_IMAGE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"!\[([^\]]*)\]\(([^)\s]+)(?:\s+["']([^"']*)["'])?\)"#).unwrap()
});
static MD_LINK_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"\[([^\]]+)\]\(([^)\s]+)(?:\s+["']([^"']*)["'])?\)"#).unwrap()
});

pub struct MarkdownParser;

impl Default for MarkdownParser {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkdownParser {
    pub fn new() -> Self {
        Self
    }
}

impl DocumentParser for MarkdownParser {
    fn parse(&self, data: &[u8]) -> Result<ParsedDocument, GBrainError> {
        let content = std::str::from_utf8(data)
            .map_err(|e| GBrainError::InvalidInput(format!("UTF-8 decode failed: {}", e)))?
            .to_string();

        Ok(ParsedDocument {
            media_refs: extract_markdown_media_refs(&content),
            content,
            metadata: HashMap::new(),
            blocks: None,
        })
    }

    fn extensions(&self) -> &[&str] {
        &["md"]
    }
}

fn extract_markdown_media_refs(markdown: &str) -> Vec<MediaRef> {
    let mut refs = Vec::new();
    for cap in MD_IMAGE_RE.captures_iter(markdown) {
        let Some(path) = cap.get(2).map(|m| m.as_str()) else {
            continue;
        };
        if should_skip_media_path(path) {
            continue;
        }
        refs.push(MediaRef {
            media_type: "image".to_string(),
            storage_path: path.to_string(),
            mime_type: None,
            byte_size: None,
            embedded_data_base64: None,
            alt_text: cap
                .get(1)
                .map(|m| m.as_str().to_string())
                .filter(|s| !s.is_empty()),
            ocr_text: None,
            caption: cap
                .get(3)
                .map(|m| m.as_str().to_string())
                .filter(|s| !s.is_empty()),
            page_number: None,
        });
    }

    for cap in MD_LINK_RE.captures_iter(markdown) {
        let Some(full) = cap.get(0) else {
            continue;
        };
        if full.start() > 0 && markdown.as_bytes()[full.start() - 1] == b'!' {
            continue;
        }
        let Some(path) = cap.get(2).map(|m| m.as_str()) else {
            continue;
        };
        if should_skip_media_path(path) || !is_attachment_path(path) {
            continue;
        }
        let label = cap.get(1).map(|m| m.as_str()).unwrap_or("").trim();
        refs.push(MediaRef {
            media_type: "attachment".to_string(),
            storage_path: path.to_string(),
            mime_type: None,
            byte_size: None,
            embedded_data_base64: None,
            alt_text: if label.is_empty() {
                None
            } else {
                Some(label.to_string())
            },
            ocr_text: None,
            caption: cap
                .get(3)
                .map(|m| m.as_str().to_string())
                .filter(|s| !s.is_empty()),
            page_number: None,
        });
    }
    refs
}

fn should_skip_media_path(path: &str) -> bool {
    let lower = path.trim().to_lowercase();
    lower.is_empty() || lower.starts_with("data:") || lower.starts_with("javascript:")
}

fn is_attachment_path(path: &str) -> bool {
    let lower = path.split('?').next().unwrap_or(path).to_lowercase();
    [
        ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".csv", ".txt", ".zip", ".rar",
        ".7z",
    ]
    .iter()
    .any(|suffix| lower.ends_with(suffix))
}
