//! Search module: keyword + vector + hybrid + intent + dedup + expansion + fuzzy + two-pass

pub mod dedup;
pub mod eval;
pub mod expansion;
pub mod fuzzy;
pub mod hybrid;
pub mod intent;
pub mod keyword;
pub mod two_pass;
pub mod vector;

pub use dedup::{dedup_results, DedupOpts};
pub use expansion::{sanitize_expansion_output, sanitize_query_for_prompt};
// H20: expand_query 是 pub async fn，当前未被外部调用方使用。
// 保留导出供未来查询扩展功能使用（通过 LLM API 扩展用户查询词以提升召回率）。
// 若确认不需要，可移除此函数及其 HTTP 调用逻辑。
pub use expansion::expand_query;
pub use fuzzy::trigram_similarity;
pub use hybrid::{hybrid_search, HybridOpts};
pub use intent::{classify_intent, detail_for_intent, Intent, Intent as QueryIntent};
pub use two_pass::{expand_anchors, hydrate_chunks, ExpandedChunk, TwoPassOpts};
