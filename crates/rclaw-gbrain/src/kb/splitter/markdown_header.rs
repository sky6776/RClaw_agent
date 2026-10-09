//! Markdown header splitter using pulldown-cmark

use super::{recursive::RecursiveCharSplitter, Chunks, DocumentSplitter};
use crate::error::GBrainError;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};

const MAX_MARKDOWN_CHUNK_CHARS: usize = 1600;
const MIN_PARAGRAPH_SPLIT_CHARS: usize = 240;

pub struct MarkdownHeaderSplitter;

impl Default for MarkdownHeaderSplitter {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkdownHeaderSplitter {
    pub fn new() -> Self {
        Self
    }
}

impl DocumentSplitter for MarkdownHeaderSplitter {
    fn split(&self, text: &str) -> Result<Chunks, GBrainError> {
        let text = normalize_note_boundaries(text);
        let mut chunks = Vec::new();
        let mut current_section = String::new();

        let parser = Parser::new(&text);

        for event in parser {
            match event {
                Event::Start(Tag::Heading { level, .. }) => {
                    if !current_section.trim().is_empty() {
                        chunks.push(current_section.trim().to_string());
                    }
                    current_section = String::new();
                    // Add heading marker
                    let prefix = "#".repeat(level as usize);
                    current_section.push_str(&prefix);
                    current_section.push(' ');
                }
                Event::End(TagEnd::Heading(_)) => {
                    current_section.push('\n');
                }
                Event::Text(t) => {
                    current_section.push_str(&t);
                }
                Event::Code(code) => {
                    current_section.push_str("```\n");
                    current_section.push_str(&code);
                    current_section.push_str("\n```\n");
                }
                Event::SoftBreak | Event::HardBreak => {
                    current_section.push('\n');
                }
                _ => {}
            }
        }

        if !current_section.trim().is_empty() {
            chunks.push(current_section.trim().to_string());
        }

        if chunks.is_empty() && !text.trim().is_empty() {
            chunks.push(text.trim().to_string());
        }

        split_large_markdown_chunks(chunks)
    }
}

fn normalize_note_boundaries(text: &str) -> String {
    // 零宽字符直接过滤移除，避免引入人工断行干扰分块边界
    text.chars()
        .filter(|ch| !matches!(ch, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}'))
        .collect()
}

fn split_large_markdown_chunks(chunks: Vec<String>) -> Result<Chunks, GBrainError> {
    let mut out = Vec::new();
    for chunk in chunks {
        split_large_markdown_chunk(&chunk, &mut out)?;
    }
    Ok(out)
}

fn split_large_markdown_chunk(chunk: &str, out: &mut Vec<String>) -> Result<(), GBrainError> {
    if chunk.chars().count() <= MAX_MARKDOWN_CHUNK_CHARS {
        if !chunk.trim().is_empty() {
            out.push(chunk.trim().to_string());
        }
        return Ok(());
    }

    let boundary_chunks = split_on_note_boundaries(chunk);
    if boundary_chunks.len() > 1 {
        for part in boundary_chunks {
            split_large_markdown_chunk(&part, out)?;
        }
        return Ok(());
    }

    let fallback = RecursiveCharSplitter::new(MAX_MARKDOWN_CHUNK_CHARS, 0);
    for part in fallback.split(chunk)? {
        if !part.trim().is_empty() {
            out.push(part.trim().to_string());
        }
    }
    Ok(())
}

fn split_on_note_boundaries(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for line in text.lines() {
        let trimmed = line.trim();
        let is_boundary = is_explicit_note_boundary(trimmed);
        let current_len = current.chars().count();
        if !current.trim().is_empty()
            && (is_boundary || (trimmed.is_empty() && current_len >= MIN_PARAGRAPH_SPLIT_CHARS))
        {
            chunks.push(current.trim().to_string());
            current.clear();
        }
        if !trimmed.is_empty() || !current.trim().is_empty() {
            current.push_str(line);
            current.push('\n');
        }
    }

    if !current.trim().is_empty() {
        chunks.push(current.trim().to_string());
    }
    chunks
}

fn is_explicit_note_boundary(line: &str) -> bool {
    if line.starts_with('#') || line.starts_with("//BEGIN") || line.starts_with("//END") {
        return true;
    }
    let normalized = line.trim_start_matches(|ch: char| {
        ch == '-' || ch == '*' || ch == '+' || ch.is_ascii_digit() || ch == '.' || ch == ')'
    });
    let normalized = normalized.trim_start();
    normalized.starts_with("==") && (normalized.contains("==:") || normalized.contains("==："))
}
