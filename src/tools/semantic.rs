//! `semantic_search`: meaning-based search through Cloudflare AI Search.

use std::sync::{Arc, LazyLock};

use regex::Regex;
use serde_json::json;

use super::ToolContext;
use super::params::SemanticSearchParams;
use crate::mcp::ToolError;
use crate::notes::make_deep_link;
use crate::search::SemanticQuery;
use crate::util::{char_len, take_chars, trim_slashes};

const EXCERPT_MAX_CHARS: usize = 500;
static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

pub async fn semantic_search(ctx: Arc<ToolContext>, args: SemanticSearchParams) -> Result<String, ToolError> {
    let Some(client) = &ctx.semantic else {
        return Ok(json!({ "error": "Semantic search is not configured." }).to_string());
    };
    let scope = args.folder.as_deref().map(trim_slashes).filter(|s| !s.is_empty());
    // A folder filter is applied here, so ask for more to fill the page.
    let query = SemanticQuery {
        query: args.query.clone(),
        limit: if scope.is_some() { 50 } else { args.limit },
        min_score: args.min_score,
    };
    let hits = match client.search(&query).await {
        Ok(hits) => hits,
        Err(error) => return Ok(json!({ "error": error.to_string() }).to_string()),
    };
    let results: Vec<_> = hits
        .into_iter()
        .filter(|hit| scope.is_none_or(|s| hit.path.starts_with(&format!("{s}/"))))
        .take(args.limit as usize)
        .map(|hit| {
            let text = WHITESPACE.replace_all(&hit.text, " ").trim().to_owned();
            let excerpt = if char_len(&text) > EXCERPT_MAX_CHARS {
                format!("{}…", take_chars(&text, EXCERPT_MAX_CHARS))
            } else {
                text
            };
            json!({
                "path": hit.path,
                "score": (hit.score * 1000.0).round() / 1000.0,
                "excerpt": excerpt,
                "url": make_deep_link(&ctx.vault_name, &hit.path),
            })
        })
        .collect();
    Ok(json!({ "query": args.query, "results": results }).to_string())
}
