//! Recursive character splitter with Chinese separator support

use super::{Chunks, DocumentSplitter};
use crate::error::GBrainError;

const DEFAULT_SEPARATORS: &[&str] = &[
    "\n\n", "\n", "。", "！", "？", "；", ".", "!", "?", ";", " ", "",
];

pub struct RecursiveCharSplitter {
    chunk_size: usize,
    chunk_overlap: usize,
    separators: Vec<String>,
}

impl RecursiveCharSplitter {
    pub fn new(chunk_size: usize, chunk_overlap: usize) -> Self {
        Self {
            chunk_size: chunk_size.max(100),
            chunk_overlap: chunk_overlap.min(chunk_size / 2),
            separators: DEFAULT_SEPARATORS.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn len_func(text: &str) -> usize {
        text.chars().count()
    }
}

impl DocumentSplitter for RecursiveCharSplitter {
    fn split(&self, text: &str) -> Result<Chunks, GBrainError> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut chunks = Vec::new();
        recursive_split(
            text,
            &self.separators,
            self.chunk_size,
            self.chunk_overlap,
            &mut chunks,
        );
        Ok(chunks)
    }
}

fn recursive_split(
    text: &str,
    separators: &[String],
    chunk_size: usize,
    chunk_overlap: usize,
    result: &mut Vec<String>,
) {
    if text.is_empty() {
        return;
    }

    if RecursiveCharSplitter::len_func(text) <= chunk_size {
        result.push(text.to_string());
        return;
    }

    let sep_idx = separators
        .iter()
        .position(|sep| !sep.is_empty() && text.contains(sep.as_str()))
        .unwrap_or(separators.len() - 1);

    let separator = &separators[sep_idx];
    let next_separators = &separators[sep_idx + 1..];

    let parts: Vec<String> = if separator.is_empty() {
        text.chars().map(|c| c.to_string()).collect()
    } else {
        text.split(separator).map(|s| s.to_string()).collect()
    };

    let mut current_chunk = String::new();

    for part in parts {
        let part_len = RecursiveCharSplitter::len_func(&part);

        if current_chunk.is_empty() {
            current_chunk = part.clone();
        } else if RecursiveCharSplitter::len_func(&current_chunk)
            + part_len
            + RecursiveCharSplitter::len_func(separator)
            <= chunk_size
        {
            current_chunk.push_str(separator);
            current_chunk.push_str(&part);
        } else {
            if !current_chunk.is_empty() {
                result.push(current_chunk.clone());
            }
            current_chunk = if chunk_overlap > 0 && !result.is_empty() {
                let prev = result.last().expect(
                    "分块结果不应为空：chunk_overlap > 0 且 result 不为空时至少应有一个 chunk",
                );
                take_tail(prev, chunk_overlap)
            } else {
                String::new()
            };
            current_chunk.push_str(&part);
        }
    }

    if !current_chunk.is_empty() {
        if RecursiveCharSplitter::len_func(&current_chunk) > chunk_size
            && !next_separators.is_empty()
        {
            recursive_split(
                &current_chunk,
                next_separators,
                chunk_size,
                chunk_overlap,
                result,
            );
        } else {
            result.push(current_chunk);
        }
    }
}

fn take_tail(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start = chars.len().saturating_sub(max_chars);
    chars[start..].iter().collect()
}
