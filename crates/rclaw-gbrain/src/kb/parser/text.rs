//! Plain text parser (fallback)

use super::{DocumentParser, ParsedDocument};
use crate::error::GBrainError;
use std::collections::HashMap;

pub struct TextParser;

impl Default for TextParser {
    fn default() -> Self {
        Self::new()
    }
}

impl TextParser {
    pub fn new() -> Self {
        Self
    }
}

impl DocumentParser for TextParser {
    fn parse(&self, data: &[u8]) -> Result<ParsedDocument, GBrainError> {
        let content = std::str::from_utf8(data)
            .map_err(|e| GBrainError::InvalidInput(format!("UTF-8 decode failed: {}", e)))?
            .to_string();

        Ok(ParsedDocument {
            content,
            metadata: HashMap::new(),
            blocks: None,
            media_refs: Vec::new(),
        })
    }

    fn extensions(&self) -> &[&str] {
        &["txt"]
    }
}
