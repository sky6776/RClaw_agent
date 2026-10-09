//! Extensible, health-aware web search provider pool.
//!
//! Provider-specific transport and response parsing live in separate modules.
//! The router only understands normalized search/read capabilities and errors.

mod control;
mod doubao;
mod render;
mod router;
mod standalone;
mod tavily;
mod types;
mod zhipu;

pub use control::{BackendStatus, HealthStatus, WebSearchControl};
pub use doubao::{DoubaoBackend, DoubaoSearchConfig, DoubaoVariant};
pub use router::{WebSearch, WebSearchConfig};
pub use standalone::ProviderSearchTool;
pub use tavily::{TavilyClient, TavilyExtract, TavilySearchConfig};
pub use types::{
    BackendError, BackendErrorKind, BackendId, ReadResponse, ReaderBackend, SearchBackend,
    SearchHit, SearchRequest, SearchResponse,
};
pub use zhipu::{ZhipuBackend, ZhipuSearchConfig};
