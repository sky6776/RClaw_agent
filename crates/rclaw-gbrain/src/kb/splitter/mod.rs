//! 文档分割器模块
//!
//! 自适应分割策略：结构优先、短块保留、长块局部细分。
//! 删除了旧的 `semantic_segmentation_enabled` 开关，
//! 改为始终使用 adaptive 行为。

pub mod adaptive;
pub mod markdown_header;
pub mod recursive;
pub mod semantic;

use crate::embedding::Embedder;
use crate::error::GBrainError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type Chunks = Vec<String>;

/// 同步文档分割器 trait
pub trait DocumentSplitter: Send + Sync {
    fn split(&self, text: &str) -> Result<Chunks, GBrainError>;
}

/// 异步文档分割器 trait (用于基于嵌入的分割器)
///
/// 使用 `Pin<Box<dyn Future>>` 返回类型以支持 dyn trait object。
pub trait AsyncDocumentSplitter: Send + Sync {
    fn split_async<'a>(
        &'a self,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Chunks, GBrainError>> + Send + 'a>>;
}

/// 分割器配置
#[derive(Debug, Clone)]
pub struct SplitterConfig {
    pub file_path: String,
    pub chunk_size: usize,
    pub chunk_overlap: usize,
}

/// 根据配置创建同步分割器。
/// 优先级: Markdown header > 递归字符分割
pub fn create_splitter(config: &SplitterConfig) -> Box<dyn DocumentSplitter> {
    let ext = std::path::Path::new(&config.file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext == "md" || ext == "markdown" {
        return Box::new(markdown_header::MarkdownHeaderSplitter::new());
    }

    Box::new(recursive::RecursiveCharSplitter::new(
        config.chunk_size,
        config.chunk_overlap,
    ))
}

/// 根据配置创建异步分割器。
/// 使用自适应分割策略：结构优先、长块细分。
/// 若提供了 embedder，大块可语义细分；否则递归细分。
pub fn create_async_splitter(
    config: &SplitterConfig,
    embedder: Option<Arc<Embedder>>,
) -> Result<Box<dyn AsyncDocumentSplitter>, GBrainError> {
    let ext = std::path::Path::new(&config.file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let adaptive_config = adaptive::AdaptiveConfig {
        extension: ext,
        chunk_size: config.chunk_size,
        chunk_overlap: config.chunk_overlap,
    };

    Ok(Box::new(adaptive::AdaptiveSplitter::new(
        adaptive_config,
        embedder,
    )))
}

pub use markdown_header::MarkdownHeaderSplitter;
pub use recursive::RecursiveCharSplitter;
pub use semantic::SemanticSplitter;
