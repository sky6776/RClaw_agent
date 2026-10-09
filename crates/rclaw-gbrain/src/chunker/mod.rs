//! Text chunking module
//!
//! Chunking strategies:
//! - **Recursive** (`recursive.rs`): Fast, deterministic, splits by paragraph and character count
//! - **LLM-guided** (`llm.rs`): Uses an LLM to identify natural section boundaries
//! - **Semantic** (`semantic.rs`): Savitzky-Golay smoothed boundary detection from heading/paragraph signals
//! - **Tree-sitter** (`tree_sitter.rs`): AST-based code chunking with rich metadata (parent scopes, qualified names, doc comments)

pub mod llm;
pub mod page;
pub mod recursive;
pub mod semantic;
pub mod tree_sitter;

// Re-export the primary recursive chunker at module level for backward compatibility
pub use page::{chunk_page_content, chunk_page_content_transactional};
pub use recursive::{chunk_text, estimate_tokens, DEFAULT_CHUNK_OVERLAP, DEFAULT_CHUNK_SIZE};
pub use semantic::{chunk_semantic, SemanticChunkerConfig};
pub use tree_sitter::chunk_code_tree_sitter;
