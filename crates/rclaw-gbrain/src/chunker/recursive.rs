//! Recursive text chunker
//! Mirrors gbrain's src/core/chunkers/recursive.ts
//!
//! Uses 5-level delimiter hierarchy with word-count-based chunking
//! and greedy merge to avoid chunks that are too small.
//! P2-3: Word-based chunk size (not character-based)
//! P2-4: Sentence boundary alignment for chunk overlap

use crate::types::{ChunkInput, ChunkSource};
use tracing::debug;

/// Default chunk size in words (matches TS: ~300 words)
pub const DEFAULT_CHUNK_SIZE: usize = 300;

/// Default chunk overlap in words (matches TS: ~50 words)
pub const DEFAULT_CHUNK_OVERLAP: usize = 50;

/// Average characters per word (rough estimate for English text)
const CHARS_PER_WORD: usize = 6;

/// Characters per token (rough estimate for English text)
const CHARS_PER_TOKEN: usize = 4;

/// P2-4: 句子边界模式，用于重叠对齐（包含中文标点）
const SENTENCE_BOUNDARY: &[char] = &['.', '!', '?', '。', '！', '？', '；'];

/// Chunk a text string into overlapping segments.
/// Uses 5-level delimiter hierarchy matching TS recursive chunker:
/// paragraph → line → sentence → clause → word.
/// P2-3: Chunk size is measured in words, not characters.
pub fn chunk_text(
    text: &str,
    chunk_size: Option<usize>,
    chunk_overlap: Option<usize>,
    source: ChunkSource,
) -> Vec<ChunkInput> {
    debug!(text_len = text.len(), chunk_size = chunk_size.unwrap_or(0), source = %source, "Chunking text (5-level, word-based)");
    let target_words = chunk_size.unwrap_or(DEFAULT_CHUNK_SIZE);
    let overlap_words = chunk_overlap.unwrap_or(DEFAULT_CHUNK_OVERLAP);

    if text.is_empty() {
        return Vec::new();
    }

    // Split using recursive delimiter hierarchy
    let segments: Vec<String> = split_recursive(text, target_words)
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect();

    // Greedy merge: combine segments to approach target word count
    let merged = greedy_merge(&segments, target_words, overlap_words);

    let _total = merged.len();
    merged
        .into_iter()
        .enumerate()
        .map(|(i, chunk_text)| {
            let token_count = estimate_tokens(&chunk_text);
            ChunkInput::text(i as i32, chunk_text, source.clone(), token_count as i32)
        })
        .collect()
}

/// 统计文本词数。
/// 中文文本使用 jieba 分词（中文无空格分隔），
/// 其他文本使用空格分隔计数。
fn word_count(text: &str) -> usize {
    if crate::nlp::chinese::has_chinese(text) {
        let tokens = crate::nlp::chinese::tokenize_content(text);
        tokens.split_whitespace().count()
    } else {
        text.split_whitespace().count()
    }
}

/// 5-level delimiter hierarchy matching TS:
/// 1. `\n\n` (paragraphs)
/// 2. `\n` (lines)
/// 3. Sentence boundaries: `. ` `! ` `? `
/// 4. Clause boundaries: `; ` `: ` `, `
/// 5. Words (whitespace)
fn split_recursive(text: &str, target_words: usize) -> Vec<String> {
    if word_count(text) <= target_words {
        return vec![text.to_string()];
    }

    // Level 1: paragraphs
    let paragraphs: Vec<&str> = text.split("\n\n").collect();
    if paragraphs.len() > 1 {
        let mut result = Vec::new();
        for para in paragraphs {
            result.extend(split_recursive(para, target_words));
        }
        return result;
    }

    // Level 2: lines
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() > 1 {
        let mut result = Vec::new();
        for line in lines {
            result.extend(split_recursive(line, target_words));
        }
        return result;
    }

    // Level 3: sentences (includes Chinese sentence punctuation)
    let sentences = split_by_regex(text, &['.', '!', '?', '。', '！', '？', '；'], &[' ']);
    if sentences.len() > 1 {
        let mut result = Vec::new();
        for sentence in sentences {
            result.extend(split_recursive(&sentence, target_words));
        }
        return result;
    }

    // Level 4: clauses (includes Chinese clause punctuation)
    let clauses = split_by_regex(text, &[';', ':', ',', '；', '：', '，'], &[' ']);
    if clauses.len() > 1 {
        let mut result = Vec::new();
        for clause in clauses {
            result.extend(split_recursive(&clause, target_words));
        }
        return result;
    }

    // Level 5: words (split by whitespace, group into chunks)
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut result = Vec::new();
    let mut current = String::new();
    for word in words {
        // C-04: Skip empty words (shouldn't happen after split_whitespace, but be safe)
        if word.is_empty() {
            continue;
        }
        if !current.is_empty() && word_count(&current) + 1 > target_words {
            result.push(current.trim().to_string());
            current = String::new();
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        result.push(current.trim().to_string());
    }
    result
}

/// Split text by punctuation followed by expected_next chars.
/// Chinese punctuation delimiters split immediately without checking expected_next,
/// since Chinese text does not use spaces after punctuation.
fn split_by_regex(text: &str, delimiters: &[char], expected_next: &[char]) -> Vec<String> {
    let mut result = Vec::new();
    let mut start = 0usize;
    let mut iter = text.char_indices().peekable();

    while let Some((idx, c)) = iter.next() {
        if delimiters.contains(&c) {
            let is_chinese_delim = is_cjk_punctuation(c);
            let end = idx + c.len_utf8();

            if is_chinese_delim {
                // Chinese punctuation: split immediately regardless of next char
                result.push(text[start..end].to_string());
                start = end;
            } else if iter
                .peek()
                .is_some_and(|(_, next)| expected_next.contains(next))
            {
                // English punctuation: delimiter followed by space
                result.push(text[start..end].to_string());
                start = end; // include the space in the next segment (preserves it)
            }
        }
    }

    // Last segment — I-12: trim leading whitespace from the trailing segment
    // after a delimiter, since the space character after punctuation is not
    // semantically meaningful at the start of a new segment.
    if start < text.len() {
        result.push(text[start..].trim_start().to_string());
    }

    result
}

/// Check if a character is CJK punctuation (Chinese full-width punctuation).
/// These characters do not require a following space to act as delimiters.
fn is_cjk_punctuation(c: char) -> bool {
    matches!(c, '。' | '！' | '？' | '；' | '：' | '，')
}

/// P2-4: Align overlap to sentence boundary.
/// Walks backward from the overlap start point to find the nearest
/// sentence-ending punctuation, avoiding mid-sentence splits.
/// Returns a char-boundary-safe position to prevent panics on multi-byte UTF-8.
fn align_to_sentence_boundary(text: &str, mut start_pos: usize) -> usize {
    if start_pos == 0 || start_pos >= text.len() {
        return start_pos;
    }

    // Ensure start_pos is on a char boundary before slicing
    while !text.is_char_boundary(start_pos) && start_pos > 0 {
        start_pos -= 1;
    }

    // Search backward from start_pos for a sentence boundary
    let before_start = &text[..start_pos];
    let mut best = start_pos;

    // Look for the last sentence-ending punctuation before start_pos
    for (i, c) in before_start.char_indices().rev() {
        if SENTENCE_BOUNDARY.contains(&c) {
            // Found a sentence boundary; align to the character after it + space
            let after = i + c.len_utf8();
            // S-05: Use char-based whitespace check instead of byte comparison
            // to correctly handle Unicode whitespace characters (e.g. non-breaking space)
            if after < text.len() && text[after..].starts_with(char::is_whitespace) {
                best = after + text[after..].chars().next().map_or(1, |ch| ch.len_utf8());
            } else {
                best = after;
            }
            break;
        }
        // Don't look back more than 2x the overlap distance
        if start_pos - i > start_pos / 2 {
            break;
        }
    }

    // Ensure best is a char boundary to prevent panic on multi-byte UTF-8 slicing
    while !text.is_char_boundary(best) && best > 0 {
        best -= 1;
    }

    best
}

/// Greedy merge: combine short chunks with neighbors to approach target word count.
/// Mirrors TS: avoids chunks > 1.5x target.
/// P2-3: Uses word count instead of character count for sizing.
/// P2-4: Aligns overlap to sentence boundaries.
fn greedy_merge(segments: &[String], target_words: usize, overlap_words: usize) -> Vec<String> {
    if segments.is_empty() {
        return Vec::new();
    }

    let max_words = (target_words as f64 * 1.5) as usize;
    let min_words = target_words / 2;
    let mut result = Vec::new();
    let mut current = segments[0].clone();

    for segment in &segments[1..] {
        let combined_words = word_count(&current) + word_count(segment) + 1;
        if combined_words <= max_words {
            // Can merge
            current.push('\n');
            current.push_str(segment);
        } else {
            // Flush current
            if word_count(&current) >= min_words || result.is_empty() {
                result.push(current);
            } else if let Some(last) = result.last_mut() {
                // Current too short, merge with previous
                last.push('\n');
                last.push_str(&current);
            }
            // Start new with overlap from end of previous
            // BUG FIX: The triggering segment must be included in the new chunk.
            // Previously, when overlap was extracted from the previous chunk,
            // the triggering segment was silently dropped (data loss).
            current = if overlap_words > 0 && !result.is_empty() {
                // S-06: Use if-let instead of unwrap on result.last()
                if let Some(prev) = result.last() {
                    let overlap_chars = overlap_words * CHARS_PER_WORD;
                    // C-05: Bound overlap to prevent full-chunk duplication.
                    // After align_to_sentence_boundary, the overlap may walk far
                    // back; cap it so the overlap never exceeds 2x the intended size.
                    let max_overlap_chars = overlap_chars * 2;
                    if prev.len() > overlap_chars {
                        let mut start = prev.len() - overlap_chars;
                        // P2-4: Align to sentence boundary
                        start = align_to_sentence_boundary(prev, start);
                        // C-05: Clamp overlap start so it doesn't exceed max_overlap_chars
                        let min_start = prev.len().saturating_sub(max_overlap_chars);
                        if start < min_start {
                            start = min_start;
                            // Re-align to char boundary after clamping
                            while !prev.is_char_boundary(start) && start < prev.len() {
                                start += 1;
                            }
                        }
                        // Char-boundary-safe slicing to avoid panic on multi-byte UTF-8
                        while !prev.is_char_boundary(start) && start < prev.len() {
                            start += 1;
                        }
                        let mut new_current = prev[start..].to_string();
                        new_current.push('\n');
                        new_current.push_str(segment);
                        new_current
                    } else {
                        segment.clone()
                    }
                } else {
                    segment.clone()
                }
            } else {
                segment.clone()
            };
        }
    }

    // Flush remaining — S-06: Use if-let instead of unwrap on result.last_mut()
    if word_count(&current) < min_words && !result.is_empty() {
        if let Some(last) = result.last_mut() {
            last.push('\n');
            last.push_str(&current);
        }
    } else if !current.is_empty() {
        result.push(current);
    }

    result
}

/// Estimate token count for a string
pub fn estimate_tokens(text: &str) -> usize {
    text.len() / CHARS_PER_TOKEN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_empty() {
        let chunks = chunk_text("", None, None, ChunkSource::CompiledTruth);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_chunk_short_text() {
        let chunks = chunk_text("Hello world", None, None, ChunkSource::CompiledTruth);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_text, "Hello world");
        assert_eq!(chunks[0].chunk_index, 0);
    }

    #[test]
    fn test_chunk_long_text() {
        let text = "word ".repeat(2000); // ~2000 words
        let chunks = chunk_text(&text, Some(100), Some(10), ChunkSource::CompiledTruth);
        assert!(chunks.len() > 1);
        // Check indices are sequential
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.chunk_index, i as i32);
        }
    }

    #[test]
    fn test_chunk_preserves_source() {
        let chunks = chunk_text("test", None, None, ChunkSource::Timeline);
        assert_eq!(chunks[0].source, ChunkSource::Timeline);
    }

    #[test]
    fn test_word_count() {
        assert_eq!(word_count("hello world"), 2);
        assert_eq!(word_count(""), 0);
        assert_eq!(word_count("one"), 1);
        assert_eq!(word_count("  spaces   between  "), 2);
    }

    #[test]
    fn test_sentence_boundary_alignment() {
        let text = "This is sentence one. This is sentence two. This is sentence three.";
        // Start at position 40 (somewhere in sentence two)
        let aligned = align_to_sentence_boundary(text, 40);
        // Should align to start of sentence two or three
        assert!(aligned > 0 && aligned < text.len());
        // The aligned position should be at a sentence start
        let prefix = &text[..aligned];
        assert!(prefix.ends_with(". ") || prefix.ends_with(".") || aligned == 0);
    }

    #[test]
    fn test_word_count_chinese() {
        // Chinese text without spaces should be counted via jieba, not return 1
        let count = word_count("这是一个中文句子");
        assert!(count > 1, "Chinese word count should be > 1, got {}", count);
    }

    #[test]
    fn test_word_count_mixed() {
        // Mixed Chinese and English
        let count = word_count("Hello 世界");
        assert!(count >= 2, "Mixed word count should be >= 2, got {}", count);
    }

    #[test]
    fn test_split_by_regex_chinese_sentence() {
        // Chinese sentence punctuation splits without requiring a following space
        let result = split_by_regex("第一句话。第二句话", &['。'], &[' ']);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "第一句话。");
        assert_eq!(result[1], "第二句话");
    }

    #[test]
    fn test_split_by_regex_chinese_clause() {
        // Chinese clause punctuation splits without requiring a following space
        let result = split_by_regex("前半句，后半句", &['，'], &[' ']);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "前半句，");
        assert_eq!(result[1], "后半句");
    }

    #[test]
    fn test_split_by_regex_english_still_requires_space() {
        // English punctuation still requires a following space to split
        let result = split_by_regex("Hello.World", &['.'], &[' ']);
        assert_eq!(result.len(), 1); // No split without space after period

        let result = split_by_regex("Hello. World", &['.'], &[' ']);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_is_cjk_punctuation() {
        assert!(is_cjk_punctuation('。'));
        assert!(is_cjk_punctuation('！'));
        assert!(is_cjk_punctuation('？'));
        assert!(is_cjk_punctuation('；'));
        assert!(is_cjk_punctuation('：'));
        assert!(is_cjk_punctuation('，'));
        assert!(!is_cjk_punctuation('.'));
        assert!(!is_cjk_punctuation('!'));
        assert!(!is_cjk_punctuation(','));
    }

    #[test]
    fn test_chunk_chinese_text() {
        // Chinese text should be chunkable with proper word counting
        let text = "这是第一段内容。这是第二段内容。这是第三段内容。";
        let chunks = chunk_text(text, Some(5), Some(1), ChunkSource::CompiledTruth);
        assert!(!chunks.is_empty());
    }
}
