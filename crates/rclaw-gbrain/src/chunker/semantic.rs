//! Semantic chunker with Savitzky-Golay smoothing for topic boundary detection
//! Mirrors gbrain's src/core/chunker/semantic.ts
//!
//! Uses sentence embedding similarity to detect topic boundaries.
//! Applies Savitzky-Golay smoothing to reduce noise in similarity scores.

#![allow(
    clippy::needless_range_loop,
    clippy::manual_is_multiple_of,
    clippy::collapsible_if,
    clippy::len_zero
)]
//! Boundaries are detected where smoothed similarity drops below a threshold.

use crate::types::{ChunkInput, ChunkSource};
use tracing::debug;

/// Default window size for Savitzky-Golay filter (must be odd)
const DEFAULT_SG_WINDOW: usize = 5;

/// Default polynomial order for Savitzky-Golay filter
const DEFAULT_SG_POLY_ORDER: usize = 2;

/// Default similarity threshold for boundary detection
const DEFAULT_BOUNDARY_THRESHOLD: f64 = 0.5;

/// Default minimum chunk size in characters
const DEFAULT_MIN_CHUNK_SIZE: usize = 200;

/// Default maximum chunk size in characters
const DEFAULT_MAX_CHUNK_SIZE: usize = 2000;

/// Semantic chunker configuration
#[derive(Debug, Clone)]
pub struct SemanticChunkerConfig {
    /// Window size for Savitzky-Golay filter (must be odd, >= 3)
    pub sg_window: usize,
    /// Polynomial order for Savitzky-Golay filter (must be < sg_window)
    pub sg_poly_order: usize,
    /// Similarity threshold for boundary detection (0.0 - 1.0)
    pub boundary_threshold: f64,
    /// Minimum chunk size in characters
    pub min_chunk_size: usize,
    /// Maximum chunk size in characters
    pub max_chunk_size: usize,
    /// Use sentence embeddings for boundary detection (default: false, uses heuristics)
    pub use_embeddings: bool,
}

impl Default for SemanticChunkerConfig {
    fn default() -> Self {
        Self {
            sg_window: DEFAULT_SG_WINDOW,
            sg_poly_order: DEFAULT_SG_POLY_ORDER,
            boundary_threshold: DEFAULT_BOUNDARY_THRESHOLD,
            min_chunk_size: DEFAULT_MIN_CHUNK_SIZE,
            max_chunk_size: DEFAULT_MAX_CHUNK_SIZE,
            use_embeddings: false,
        }
    }
}

/// Chunk text using semantic boundaries detected via embedding similarity
///
/// This is a text-only version that uses paragraph breaks and content patterns
/// to detect topic boundaries. For full embedding-based chunking, use the
/// async version that calls the embedding API.
///
/// The algorithm:
/// 1. Split text into paragraphs
/// 2. Compute paragraph similarity signals (heading detection, paragraph length changes)
/// 3. Apply Savitzky-Golay smoothing to reduce noise
/// 4. Detect boundaries where smoothed signal drops below threshold
/// 5. Group paragraphs into chunks respecting min/max size constraints
pub fn chunk_semantic(
    text: &str,
    slug: &str,
    source: ChunkSource,
    config: Option<SemanticChunkerConfig>,
) -> Vec<ChunkInput> {
    let config = config.unwrap_or_default();
    let paragraphs = split_into_paragraphs(text);

    if paragraphs.is_empty() {
        return Vec::new();
    }

    // Compute boundary signals for each paragraph transition
    let signals = compute_boundary_signals(&paragraphs);

    // Apply Savitzky-Golay smoothing
    let smoothed = savitzky_golay(&signals, config.sg_window, config.sg_poly_order);

    // Detect boundaries
    let boundaries = detect_boundaries(&smoothed, config.boundary_threshold);

    // Group paragraphs into chunks
    let chunks = group_into_chunks(
        &paragraphs,
        &boundaries,
        config.min_chunk_size,
        config.max_chunk_size,
    );

    // L24: 语义切分完成日志降级为 debug，避免高频文档处理时刷屏
    debug!(
        slug = %slug,
        paragraph_count = paragraphs.len(),
        boundary_count = boundaries.len(),
        chunk_count = chunks.len(),
        "Semantic chunking complete"
    );

    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk_text)| {
            ChunkInput::text(
                i as i32,
                chunk_text.clone(),
                source.clone(),
                crate::chunker::estimate_tokens(&chunk_text) as i32,
            )
        })
        .collect()
}

/// Split text into paragraphs (non-empty lines)
fn split_into_paragraphs(text: &str) -> Vec<String> {
    text.split("\n\n")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .collect()
}

/// Compute boundary signals between consecutive paragraphs
///
/// Returns a signal value for each paragraph transition (n-1 values for n paragraphs).
/// Higher values indicate stronger boundary signals.
fn compute_boundary_signals(paragraphs: &[String]) -> Vec<f64> {
    if paragraphs.len() <= 1 {
        return Vec::new();
    }

    let mut signals = Vec::with_capacity(paragraphs.len() - 1);

    for i in 0..paragraphs.len() - 1 {
        let mut signal = 0.0;

        let curr = &paragraphs[i];
        let next = &paragraphs[i + 1];

        // Heading detection: next paragraph starts with #
        if next.starts_with('#') {
            signal += 0.8;
        }

        // Heading level change: different # counts
        let curr_heading_level = heading_level(curr);
        let next_heading_level = heading_level(next);
        if next_heading_level > 0 && next_heading_level < curr_heading_level {
            signal += 0.6;
        } else if next_heading_level > 0 && curr_heading_level == 0 {
            signal += 0.4;
        }

        // Length discontinuity: sudden change in paragraph length
        let curr_len = curr.len() as f64;
        let next_len = next.len() as f64;
        if curr_len > 0.0 && next_len > 0.0 {
            let ratio = (curr_len / next_len).max(next_len / curr_len);
            if ratio > 3.0 {
                signal += 0.3;
            }
        }

        // Empty line or separator detection
        if curr.trim().is_empty() || curr.trim().matches('-').count() > 5 {
            signal += 0.2;
        }

        // List detection: next paragraph starts with - or *
        if next.trim().starts_with("- ") || next.trim().starts_with("* ") {
            signal += 0.2;
        }

        signals.push(signal);
    }

    signals
}

/// Get the heading level of a paragraph (0 = not a heading)
fn heading_level(paragraph: &str) -> usize {
    let trimmed = paragraph.trim();
    if !trimmed.starts_with('#') {
        return 0;
    }
    trimmed.chars().take_while(|c| *c == '#').count()
}

/// Apply Savitzky-Golay smoothing to a signal
///
/// Uses a simplified moving average with polynomial fit weights.
/// For a proper implementation, this would compute the SG coefficients
/// from the window size and polynomial order. Here we use a weighted
/// moving average as an approximation.
pub fn savitzky_golay(signal: &[f64], window: usize, poly_order: usize) -> Vec<f64> {
    if signal.is_empty() || window < 3 || window.is_multiple_of(2) {
        return signal.to_vec();
    }

    let half_window = window / 2;
    let n = signal.len();
    let mut result = Vec::with_capacity(n);

    // Compute SG-like weights (simplified: use binomial-like weights)
    let weights = compute_sg_weights(window, poly_order);

    for i in 0..n {
        let start = i.saturating_sub(half_window);
        let end = (i + half_window + 1).min(n);

        let mut weighted_sum = 0.0;
        let mut weight_sum = 0.0;

        for j in start..end {
            let weight_idx = j as isize - i as isize + half_window as isize;
            if weight_idx >= 0 && (weight_idx as usize) < weights.len() {
                weighted_sum += signal[j] * weights[weight_idx as usize];
                weight_sum += weights[weight_idx as usize];
            }
        }

        if weight_sum > 0.0 {
            result.push(weighted_sum / weight_sum);
        } else {
            result.push(signal[i]);
        }
    }

    result
}

/// Compute Savitzky-Golay weights using Vandermonde matrix pseudo-inverse.
/// Mirrors TS semantic.ts: builds Vandermonde matrix, computes (A^T A)^-1 A^T,
/// extracts first row (= smoothing coefficients for order 0 derivative).
fn compute_sg_weights(window: usize, poly_order: usize) -> Vec<f64> {
    if window < 3 || window.is_multiple_of(2) || poly_order >= window {
        // Fallback: uniform weights
        return vec![1.0 / window as f64; window];
    }

    let half = (window / 2) as isize;
    let x_vals: Vec<f64> = (-half..=half).map(|i| i as f64).collect();

    // Build Vandermonde matrix: A[i][j] = x_i^j
    let rows = window;
    let cols = poly_order + 1;
    let mut a = vec![vec![0.0; cols]; rows];
    for i in 0..rows {
        for j in 0..cols {
            a[i][j] = x_vals[i].powi(j as i32);
        }
    }

    // Compute A^T * A
    let mut ata = vec![vec![0.0; cols]; cols];
    for i in 0..cols {
        for j in 0..cols {
            for k in 0..rows {
                ata[i][j] += a[k][i] * a[k][j];
            }
        }
    }

    // Invert ATA (small matrix, use simple Gaussian elimination)
    let atainv = invert_matrix(&ata);

    // Compute (A^T A)^-1 * A^T, taking the first row (order=0 smoothing)
    let mut coefs = vec![0.0; rows];
    for i in 0..rows {
        for j in 0..cols {
            coefs[i] += atainv[0][j] * a[i][j];
        }
    }

    coefs
}

/// Simple matrix inversion via Gaussian elimination with partial pivoting.
/// For small matrices (window=5, cols=3), this is accurate enough.
fn invert_matrix(m: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let n = m.len();
    let mut aug = vec![vec![0.0; 2 * n]; n];

    // Build augmented matrix [M | I]
    for i in 0..n {
        for j in 0..n {
            aug[i][j] = m[i][j];
        }
        aug[i][n + i] = 1.0;
    }

    // Gaussian elimination with partial pivoting
    for col in 0..n {
        // Find pivot
        let mut pivot_row = col;
        let mut pivot_val = aug[col][col].abs();
        for row in (col + 1)..n {
            let val = aug[row][col].abs();
            if val > pivot_val {
                pivot_val = val;
                pivot_row = row;
            }
        }
        if pivot_val < 1e-12 {
            // Singular matrix, return identity as fallback
            let mut identity = vec![vec![0.0; n]; n];
            for i in 0..n {
                identity[i][i] = 1.0;
            }
            return identity;
        }
        aug.swap(col, pivot_row);

        // Normalize pivot row
        let pivot = aug[col][col];
        for j in 0..2 * n {
            aug[col][j] /= pivot;
        }

        // Eliminate other rows
        for row in 0..n {
            if row != col {
                let factor = aug[row][col];
                for j in 0..2 * n {
                    aug[row][j] -= factor * aug[col][j];
                }
            }
        }
    }

    // Extract inverse from right half
    let mut inverse = vec![vec![0.0; n]; n];
    for i in 0..n {
        for j in 0..n {
            inverse[i][j] = aug[i][n + j];
        }
    }
    inverse
}

/// Detect boundaries from smoothed signal
///
/// A boundary is detected where the signal exceeds the threshold.
fn detect_boundaries(smoothed: &[f64], threshold: f64) -> Vec<usize> {
    let mut boundaries = Vec::new();

    for (i, &signal) in smoothed.iter().enumerate() {
        if signal >= threshold {
            // Boundary is after paragraph i
            boundaries.push(i + 1);
        }
    }

    debug!(
        threshold,
        boundary_count = boundaries.len(),
        "Detected semantic boundaries"
    );
    boundaries
}

/// Group paragraphs into chunks based on detected boundaries
///
/// Respects min/max chunk size constraints.
fn group_into_chunks(
    paragraphs: &[String],
    boundaries: &[usize],
    min_chunk_size: usize,
    max_chunk_size: usize,
) -> Vec<String> {
    let boundary_set: std::collections::HashSet<usize> = boundaries.iter().copied().collect();

    let mut chunks = Vec::new();
    let mut current_chunk = Vec::new();
    let mut current_size = 0;

    for (i, paragraph) in paragraphs.iter().enumerate() {
        let para_size = paragraph.len();

        // Check if this is a boundary point
        let is_boundary = boundary_set.contains(&i) && !current_chunk.is_empty();

        // Check if adding this paragraph would exceed max size
        let would_exceed =
            current_size + para_size + 1 > max_chunk_size && !current_chunk.is_empty();

        if is_boundary || would_exceed {
            // Finalize current chunk
            let chunk_text = current_chunk.join("\n\n");
            if !chunk_text.is_empty() {
                chunks.push(chunk_text);
            }
            current_chunk = Vec::new();
            current_size = 0;
        }

        current_chunk.push(paragraph.clone());
        current_size += para_size + 2; // +2 for \n\n

        // If current chunk exceeds max size, force split
        if current_size > max_chunk_size {
            let chunk_text = current_chunk.join("\n\n");
            if !chunk_text.is_empty() {
                chunks.push(chunk_text);
            }
            current_chunk = Vec::new();
            current_size = 0;
        }
    }

    // Don't forget the last chunk
    if !current_chunk.is_empty() {
        let chunk_text = current_chunk.join("\n\n");

        // If the last chunk is too small, merge with previous
        if chunk_text.len() < min_chunk_size && !chunks.is_empty() {
            let prev = chunks.pop().unwrap();
            chunks.push(format!("{}\n\n{}", prev, chunk_text));
        } else {
            chunks.push(chunk_text);
        }
    }

    chunks
}

/// Chunk text using sentence embeddings for boundary detection.
///
/// Algorithm:
/// 1. Split text into sentences (split on `. `, `! `, `? `, `\n`)
/// 2. Get embeddings for all sentences via embedder
/// 3. Compute cosine similarity between adjacent sentences
/// 4. Apply Savitzky-Golay smoothing to similarity scores
/// 5. Detect boundaries where smoothed similarity < threshold
/// 6. Group sentences into chunks respecting min/max size
pub fn chunk_semantic_with_embeddings(
    text: &str,
    _slug: &str,
    source: ChunkSource,
    sentence_embeddings: &[Vec<f32>],
    config: Option<SemanticChunkerConfig>,
) -> Vec<ChunkInput> {
    let cfg = config.unwrap_or_default();
    let sentences = split_sentences(text);

    if sentences.len() <= 1 || sentence_embeddings.len() != sentences.len() {
        // Fall back to single chunk if not enough sentences or embeddings mismatch
        let token_count = (text.len() / 3).max(1) as i32;
        return vec![ChunkInput::text(0, text.to_string(), source, token_count)];
    }

    // Compute cosine similarity between adjacent sentences
    let similarities: Vec<f64> = (0..sentences.len() - 1)
        .map(|i| {
            super::super::search::vector::cosine_similarity(
                &sentence_embeddings[i],
                &sentence_embeddings[i + 1],
            ) as f64
        })
        .collect();

    // Convert similarity to "dissimilarity" signal (1.0 - similarity)
    // High dissimilarity = likely topic boundary
    let signals: Vec<f64> = similarities.iter().map(|s| 1.0 - s).collect();

    // Savitzky-Golay smoothing
    let smoothed = savitzky_golay_smooth(&signals, cfg.sg_window, cfg.sg_poly_order);

    // Detect boundaries where smoothed dissimilarity > threshold
    let mut boundaries = Vec::new();
    for (i, &signal) in smoothed.iter().enumerate() {
        if signal >= cfg.boundary_threshold {
            boundaries.push(i + 1); // boundary after sentence i
        }
    }

    debug!(
        sentence_count = sentences.len(),
        boundary_count = boundaries.len(),
        "Embedding-based boundary detection complete"
    );

    // Group sentences into chunks
    let chunks = group_sentences_into_chunks(
        &sentences,
        &boundaries,
        cfg.min_chunk_size,
        cfg.max_chunk_size,
    );

    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk_text)| {
            let token_count = (chunk_text.len() / 3).max(1) as i32;
            ChunkInput::text(i as i32, chunk_text, source.clone(), token_count)
        })
        .collect()
}

/// Split text into sentences.
/// H24 修复: 使用 Peekable<Chars> 惰性迭代替代 Vec<char> 收集，
/// 避免大文本场景下的内存分配（原始实现对每段文本分配完整 char 数组）。
fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    let mut iter = text.chars().peekable();

    while let Some(ch) = iter.next() {
        current.push(ch);

        // 句子边界: 标点符号（. ! ?）后跟空格或换行
        if ch == '.' || ch == '!' || ch == '?' {
            if let Some(&next) = iter.peek() {
                if next == ' ' || next == '\n' {
                    if !current.trim().is_empty() {
                        sentences.push(current.trim().to_string());
                    }
                    current = String::new();
                    iter.next(); // 消费空格/换行
                    continue;
                }
            }
        }

        // 双换行边界
        if ch == '\n' {
            if let Some(&next) = iter.peek() {
                if next == '\n' {
                    if !current.trim().is_empty() {
                        sentences.push(current.trim().to_string());
                    }
                    current = String::new();
                    iter.next(); // 消费第二个换行
                    continue;
                }
            }
        }
    }

    if !current.trim().is_empty() {
        sentences.push(current.trim().to_string());
    }

    sentences
}

/// Group sentences into chunks respecting min/max size constraints
fn group_sentences_into_chunks(
    sentences: &[String],
    boundaries: &[usize],
    min_size: usize,
    max_size: usize,
) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut current_size = 0;

    for (i, sentence) in sentences.iter().enumerate() {
        let sent_len = sentence.len();

        // Check if adding this sentence would exceed max
        if current_size + sent_len > max_size && !current.is_empty() {
            chunks.push(current.join(" "));
            current = Vec::new();
            current_size = 0;
        }

        current.push(sentence.clone());
        current_size += sent_len;

        // Check if we hit a boundary
        if boundaries.contains(&(i + 1)) && current_size >= min_size {
            chunks.push(current.join(" "));
            current = Vec::new();
            current_size = 0;
        }
    }

    // Don't forget the last chunk
    if !current.is_empty() {
        let chunk_text = current.join(" ");
        if chunk_text.len() < min_size && !chunks.is_empty() {
            let prev = chunks.pop().unwrap();
            chunks.push(format!("{} {}", prev, chunk_text));
        } else {
            chunks.push(chunk_text);
        }
    }

    chunks
}

// Re-export savitzky_golay_smooth for use by the embedding path
fn savitzky_golay_smooth(signal: &[f64], window: usize, _poly_order: usize) -> Vec<f64> {
    if signal.is_empty() || window < 3 || window.is_multiple_of(2) {
        return signal.to_vec();
    }
    if signal.len() < window {
        return signal.to_vec();
    }

    let weights = compute_sg_weights(window, _poly_order);
    let half = window / 2;
    let mut smoothed = vec![0.0; signal.len()];

    for i in 0..signal.len() {
        if i < half || i >= signal.len() - half {
            smoothed[i] = signal[i];
        } else {
            let mut sum = 0.0;
            let mut weight_sum = 0.0;
            for j in 0..window {
                let idx = i + j - half;
                sum += signal[idx] * weights[j];
                weight_sum += weights[j];
            }
            smoothed[i] = if weight_sum > 0.0 {
                sum / weight_sum
            } else {
                signal[i]
            };
        }
    }

    smoothed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_into_paragraphs() {
        let text = "First paragraph.\n\nSecond paragraph.\n\nThird paragraph.";
        let paragraphs = split_into_paragraphs(text);
        assert_eq!(paragraphs.len(), 3);
    }

    #[test]
    fn test_heading_level() {
        assert_eq!(heading_level("# Heading 1"), 1);
        assert_eq!(heading_level("## Heading 2"), 2);
        assert_eq!(heading_level("### Heading 3"), 3);
        assert_eq!(heading_level("Not a heading"), 0);
    }

    #[test]
    fn test_boundary_signals_heading() {
        let paragraphs = vec![
            "Some text here.".to_string(),
            "# New Section".to_string(),
            "More text.".to_string(),
        ];
        let signals = compute_boundary_signals(&paragraphs);
        assert!(
            signals[0] > 0.5,
            "Heading should produce strong boundary signal"
        );
    }

    #[test]
    fn test_savitzky_golay() {
        let signal = vec![0.1, 0.2, 0.8, 0.2, 0.1];
        let smoothed = savitzky_golay(&signal, 3, 1);
        assert_eq!(smoothed.len(), 5);
        // The peak should be smoothed down
        assert!(smoothed[2] < signal[2]);
    }

    #[test]
    fn test_detect_boundaries() {
        let smoothed = vec![0.1, 0.2, 0.8, 0.3, 0.1];
        let boundaries = detect_boundaries(&smoothed, 0.5);
        assert_eq!(boundaries, vec![3]); // boundary after paragraph 2
    }

    #[test]
    fn test_group_into_chunks() {
        let paragraphs = vec![
            "This is the first paragraph with enough text to be meaningful.".to_string(),
            "This is the second paragraph with enough text to be meaningful.".to_string(),
            "This is the third paragraph with enough text to be meaningful.".to_string(),
        ];
        let boundaries = vec![2]; // boundary after paragraph 1
        let chunks = group_into_chunks(&paragraphs, &boundaries, 10, 1000);
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn test_chunk_semantic_basic() {
        let text = "# Section 1\n\nSome content here.\n\n# Section 2\n\nDifferent content.";
        let chunks = chunk_semantic(
            text,
            "test/page",
            ChunkSource::CompiledTruth,
            Some(SemanticChunkerConfig {
                boundary_threshold: 0.3,
                ..Default::default()
            }),
        );
        assert!(!chunks.is_empty());
    }

    #[test]
    fn test_sg_weights() {
        let weights = compute_sg_weights(5, 2);
        assert_eq!(weights.len(), 5);
        // Center weight should be highest
        assert!(weights[2] > weights[0]);
        assert!(weights[2] > weights[4]);
    }
}
