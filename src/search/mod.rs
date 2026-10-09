//! Search: the in-memory metadata index and the optional semantic search client.

mod ai_search;
mod index;

pub use ai_search::{AiSearchClient, AiSearchError, AiSearchOptions, SemanticHit, SemanticQuery};
pub use index::{IndexState, SearchIndex, TagCount};
