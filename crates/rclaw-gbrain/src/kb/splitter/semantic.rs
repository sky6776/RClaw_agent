//! 基于嵌入相似度的语义分割器

use super::{AsyncDocumentSplitter, Chunks};
use crate::embedding::Embedder;
use crate::error::GBrainError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// 基于嵌入相似度的语义分割器
///
/// 通过计算相邻段落的嵌入向量余弦相似度，
/// 在相似度低于百分位阈值且当前块达到最小长度时进行分割。
pub struct SemanticSplitter {
    embedder: Arc<Embedder>,
    percentile_threshold: f64,
    min_chunk_size: usize,
    chunk_size: usize,
    chunk_overlap: usize,
    /// P1 修复: 语义分块最大允许的 chunk 大小（chunk_size 的倍数）。
    /// 超过此值的 chunk 说明语义分割器无法有效切分该文本，
    /// 调用方应回退到 recursive char splitter。
    max_chunk_size: usize,
}

impl SemanticSplitter {
    /// P1 修复: 语义分块最大允许的 chunk 倍数。超过 chunk_size * 3 视为不可切分。
    const MAX_CHUNK_MULTIPLIER: usize = 3;

    pub fn new(embedder: Arc<Embedder>) -> Self {
        Self {
            embedder,
            percentile_threshold: 0.6,
            min_chunk_size: 300,
            chunk_size: 512,
            chunk_overlap: 50,
            max_chunk_size: 512 * Self::MAX_CHUNK_MULTIPLIER,
        }
    }

    pub fn with_config(embedder: Arc<Embedder>, chunk_size: usize, chunk_overlap: usize) -> Self {
        let max_chunk_size = chunk_size
            .saturating_mul(Self::MAX_CHUNK_MULTIPLIER)
            .max(chunk_size * 2);
        Self {
            embedder,
            percentile_threshold: 0.6,
            min_chunk_size: chunk_size / 2,
            chunk_size,
            chunk_overlap,
            max_chunk_size,
        }
    }

    pub async fn split(&self, text: &str) -> Result<Chunks, GBrainError> {
        // P0-4: 双换行切分不足时,启用 CJK 感知句界分割,
        // 避免中文长段落被整体作为一个 semantic paragraph。
        let paragraphs: Vec<String> = split_units(text);

        // P1 修复: 单段落/缺少 units 时，检查文本长度是否超过 max_chunk_size。
        // 超过则回退到空结果（由调用方降级到 recursive char splitter），
        // 避免无空行中文、长 OCR、压缩文本等产生超大 chunk。
        if paragraphs.len() <= 1 {
            if text.chars().count() > self.max_chunk_size {
                return Ok(Vec::new()); // 信号：语义分割器无法切分
            }
            return Ok(vec![text.to_string()]);
        }

        let paragraph_refs: Vec<&str> = paragraphs.iter().map(|s| s.as_str()).collect();
        let embeddings = self.embedder.embed_batch(&paragraph_refs).await?;

        if embeddings.len() < 2 {
            if text.chars().count() > self.max_chunk_size {
                return Ok(Vec::new()); // 信号：语义分割器无法切分
            }
            return Ok(vec![text.to_string()]);
        }

        let mut similarities = Vec::new();
        for i in 0..embeddings.len() - 1 {
            let sim = cosine_similarity(&embeddings[i], &embeddings[i + 1]);
            similarities.push(sim);
        }

        let mut sorted_sims = similarities.clone();
        sorted_sims.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let threshold_idx = ((sorted_sims.len() as f64 * self.percentile_threshold) as usize)
            .min(sorted_sims.len().saturating_sub(1));
        let threshold = sorted_sims.get(threshold_idx).copied().unwrap_or(0.5);

        let mut chunks = Vec::new();
        let mut current = String::new();

        for (i, para) in paragraphs.iter().enumerate() {
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(para.as_str());

            let should_split = i < similarities.len()
                && similarities[i] < threshold
                && current.chars().count() >= self.min_chunk_size;

            // Also split if chunk exceeds chunk_size
            let exceeds_size = current.chars().count() > self.chunk_size;

            if should_split || exceeds_size {
                chunks.push(current.trim().to_string());
                if self.chunk_overlap > 0 && !chunks.is_empty() {
                    let prev = chunks.last().unwrap();
                    current = take_tail(prev, self.chunk_overlap);
                } else {
                    current = String::new();
                }
            }
        }

        if !current.trim().is_empty() {
            chunks.push(current.trim().to_string());
        }

        // P1 修复: 语义分割后验证每个 chunk 不超过 max_chunk_size。
        // 如果有超大 chunk（如相似度不足以切分的同质长文本），
        // 清空结果让调用方回退到 recursive char splitter。
        let has_oversized = chunks
            .iter()
            .any(|c| c.chars().count() > self.max_chunk_size);
        if has_oversized {
            return Ok(Vec::new()); // 信号：产出超大 chunk，无法信任语义分块结果
        }

        Ok(chunks)
    }
}

impl AsyncDocumentSplitter for SemanticSplitter {
    fn split_async<'a>(
        &'a self,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Chunks, GBrainError>> + Send + 'a>> {
        Box::pin(self.split(text))
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    let len = a.len().min(b.len());
    if len == 0 {
        return 0.0;
    }
    let dot: f64 = a[..len]
        .iter()
        .zip(b[..len].iter())
        .map(|(x, y)| (*x as f64) * (*y as f64))
        .sum();
    let norm_a: f64 = a[..len]
        .iter()
        .map(|x| (*x as f64).powi(2))
        .sum::<f64>()
        .sqrt();
    let norm_b: f64 = b[..len]
        .iter()
        .map(|x| (*x as f64).powi(2))
        .sum::<f64>()
        .sqrt();
    if !norm_a.is_finite() || !norm_b.is_finite() || norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    let result = dot / (norm_a * norm_b);
    if result.is_finite() {
        result
    } else {
        0.0
    }
}

fn take_tail(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start = chars.len().saturating_sub(max_chars);
    chars[start..].iter().collect()
}

/// P0-4: 段落分割单元。优先按双换行切分;若文本无空行(中文长段落常见),
/// 回退到 CJK 感知句界切分(。!?!?;)。
///
/// 修正前问题:中文文档常无空行,split("\n\n") 仅返回 1 个段落,
/// 导致 semantic splitter 把整篇文档作为一个 paragraph,
/// 失去语义分块能力,且后续 embedding 会因过长被截断。
pub fn split_units(text: &str) -> Vec<String> {
    let paragraphs: Vec<String> = text
        .split("\n\n")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect();

    if paragraphs.len() > 1 {
        return paragraphs;
    }

    // 整体作为一个段落时,做 CJK 感知句界切分
    let single = paragraphs.first().map(String::as_str).unwrap_or("");
    split_sentences_cjk_aware(single)
}

/// 在 CJK 句末标点(。!?!??;)之后切分,保留标点在前一段。
/// 同时考虑 ASCII 句末标点(. ! ?)并避免在"3.14"这种小数点处误切。
pub fn split_sentences_cjk_aware(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut result: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut prev_ch: Option<char> = None;
    let chars: Vec<char> = text.chars().collect();
    for (idx, ch) in chars.iter().enumerate() {
        current.push(*ch);
        let is_sentence_end = matches!(ch, '。' | '！' | '？' | '；' | '!' | '?' | ';');
        // ASCII 句点需要排除小数/版本号场景:前驱是数字且后继也是数字则不切
        let is_ascii_dot_safe = *ch == '.' && {
            let prev_is_digit = prev_ch.map(|c| c.is_ascii_digit()).unwrap_or(false);
            let next_is_digit = chars
                .get(idx + 1)
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false);
            !(prev_is_digit && next_is_digit)
        };
        if (is_sentence_end || is_ascii_dot_safe) && !current.trim().is_empty() {
            result.push(std::mem::take(&mut current).trim().to_string());
        }
        prev_ch = Some(*ch);
    }
    if !current.trim().is_empty() {
        result.push(current.trim().to_string());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_units_paragraphs() {
        let text = "段落一\n\n段落二\n\n段落三";
        let units = split_units(text);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0], "段落一");
        assert_eq!(units[2], "段落三");
    }

    #[test]
    fn test_split_units_cjk_fallback() {
        let text = "这是第一句。这是第二句!这是第三句?";
        let units = split_units(text);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0], "这是第一句。");
        assert_eq!(units[1], "这是第二句!");
        assert_eq!(units[2], "这是第三句?");
    }

    #[test]
    fn test_split_sentences_decimal_no_split() {
        // 小数点不应触发切分
        let text = "Pi is 3.14 and that's it.";
        let units = split_sentences_cjk_aware(text);
        // 最后的 . 是句末,应切一刀,但 3.14 不应切
        assert!(units.len() <= 2);
        // 至少 3.14 不应被切散
        let combined = units.join(" ");
        assert!(combined.contains("3.14"));
    }

    #[test]
    fn test_split_sentences_empty() {
        assert!(split_sentences_cjk_aware("").is_empty());
        assert!(split_units("").is_empty());
    }
}
