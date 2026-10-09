//! Page-level chunking policy for gbrain pages.
//!
//! KB documents use `src/kb/splitter`; this module is for ordinary brain
//! pages stored in `pages/chunks`.

use crate::config::Config;
use crate::types::{ChunkInput, ChunkSource, PageType};
use tracing::{debug, warn};

use super::llm::{llm_chunk, MAX_LLM_CHUNK_INPUT_CHARS};
use super::{chunk_text, estimate_tokens};

const MAX_PAGE_LLM_CHUNKS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageChunkerMode {
    Auto,
    Recursive,
    Llm,
}

impl PageChunkerMode {
    fn parse(value: &str) -> Self {
        match value {
            "recursive" => Self::Recursive,
            "llm" => Self::Llm,
            _ => Self::Auto,
        }
    }
}

/// Chunk ordinary brain page content.
///
/// `auto` and `llm` use the configured LLM-guided chunker for non-code pages,
/// then fall back to the recursive chunker when the model output is unsuitable.
/// Code pages keep the deterministic body chunk plus tree-sitter symbol chunks
/// added by the caller.
pub fn chunk_page_content(content: &str, config: &Config, page_type: &PageType) -> Vec<ChunkInput> {
    let fallback = || chunk_text(content, None, None, ChunkSource::CompiledTruth);
    let recursive_chunks = fallback();
    let mode = PageChunkerMode::parse(config.page_chunker_mode.as_str());

    if *page_type == PageType::Code || mode == PageChunkerMode::Recursive {
        return recursive_chunks;
    }

    if !can_attempt_llm(content, config) {
        return recursive_chunks;
    }

    match run_llm_chunker(content, config, recursive_chunks.len()) {
        Some(chunks) => chunks,
        None => recursive_chunks,
    }
}

/// Chunk page content without making external LLM calls.
///
/// Use this inside database transactions or direct SQL maintenance paths. The
/// caller may still append tree-sitter/fenced-code chunks after this base body
/// chunking step.
pub fn chunk_page_content_transactional(content: &str, page_type: &PageType) -> Vec<ChunkInput> {
    if *page_type != PageType::Code {
        debug!(
            page_type = %page_type,
            "Page LLM chunker skipped: transactional chunking uses recursive mode"
        );
    }
    chunk_text(content, None, None, ChunkSource::CompiledTruth)
}

fn can_attempt_llm(content: &str, config: &Config) -> bool {
    if content.trim().is_empty() {
        return false;
    }
    if content.len() > MAX_LLM_CHUNK_INPUT_CHARS {
        debug!(
            content_len = content.len(),
            max = MAX_LLM_CHUNK_INPUT_CHARS,
            "Page LLM chunker skipped: content too large"
        );
        return false;
    }
    config
        .chunker_api_key
        .as_deref()
        .is_some_and(|v| !v.trim().is_empty())
        && config
            .chunker_base_url
            .as_deref()
            .is_some_and(|v| !v.trim().is_empty())
        && !config.chunker_model.trim().is_empty()
}

fn run_llm_chunker(
    content: &str,
    config: &Config,
    recursive_chunk_count: usize,
) -> Option<Vec<ChunkInput>> {
    let api_key = config.chunker_api_key.as_deref()?;
    let base_url = config.chunker_base_url.as_deref()?;
    let model = config.chunker_model.as_str();

    let rt = crate::runtime::shared_runtime();
    let llm_chunks = rt.block_on(llm_chunk(
        content,
        api_key,
        base_url,
        model,
        Some(MAX_PAGE_LLM_CHUNKS),
    ));

    let converted = llm_chunks_to_inputs(llm_chunks);
    if converted.is_empty() {
        warn!("Page LLM chunker returned no valid chunks; falling back to recursive chunker");
        return None;
    }

    if recursive_chunk_count > 1 && converted.len() == 1 {
        warn!(
            recursive_chunk_count,
            "Page LLM chunker returned a single chunk for multi-chunk content; falling back"
        );
        return None;
    }

    Some(converted)
}

fn llm_chunks_to_inputs(chunks: Vec<super::llm::LLMChunk>) -> Vec<ChunkInput> {
    chunks
        .into_iter()
        .enumerate()
        .filter_map(|(idx, chunk)| {
            let text = chunk.content.trim().to_string();
            if text.is_empty() {
                return None;
            }
            let mut input = ChunkInput::text(
                idx as i32,
                text.clone(),
                ChunkSource::CompiledTruth,
                estimate_tokens(&text) as i32,
            );
            input.start_line = Some(chunk.start_line as i32);
            input.end_line = Some(chunk.end_line as i32);
            Some(input)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_mode(mode: &str) -> Config {
        Config {
            page_chunker_mode: mode.to_string(),
            chunker_api_key: Some("test-key".to_string()),
            chunker_base_url: Some("https://example.test/v1".to_string()),
            chunker_model: "test-model".to_string(),
            ..Config::default()
        }
    }

    #[test]
    fn recursive_mode_uses_recursive_chunks() {
        let config = config_with_mode("recursive");
        let chunks = chunk_page_content("第一段内容。\n\n第二段内容。", &config, &PageType::Note);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|c| c.start_line.is_none()));
    }

    #[test]
    fn code_pages_do_not_use_llm_chunker() {
        let config = config_with_mode("llm");
        let chunks = chunk_page_content(
            "fn main() {\n    println!(\"hello\");\n}",
            &config,
            &PageType::Code,
        );
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].start_line.is_none());
    }

    #[test]
    fn auto_skips_llm_when_content_is_too_large() {
        let config = config_with_mode("auto");
        let content = "word ".repeat(MAX_LLM_CHUNK_INPUT_CHARS / 5 + 10);
        let chunks = chunk_page_content(&content, &config, &PageType::Note);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|c| c.start_line.is_none()));
    }
}
