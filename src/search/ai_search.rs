//! Semantic search over the vault through Cloudflare AI Search.
//!
//! AI Search indexes the same R2 bucket Remotely Save syncs to (chunking,
//! embeddings and re-indexing are its job), so the server only sends queries
//! and maps the object keys it gets back to vault paths. After the server
//! writes a note it asks for a re-index, coalescing bursts, so an agent's own
//! edits turn up within minutes instead of at the next scheduled sync.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::util::{encode_uri_component, take_chars};
use crate::vault::{is_mirrored_path, normalize_prefix};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Wait this long after the last write before asking for a re-index.
const SYNC_DEBOUNCE: Duration = Duration::from_secs(60);
/// AI Search accepts at most one indexing job per 30 s per instance.
const SYNC_MIN_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Connection to one Cloudflare AI Search instance that indexes the Remotely Save bucket.
#[derive(Debug, Clone)]
pub struct AiSearchOptions {
    pub account_id: String,
    pub token: String,
    pub namespace: String,
    pub instance: String,
    /// `S3_PREFIX`: the part of every object key that is not the vault path.
    pub prefix: String,
    /// API root; the Cloudflare API unless a test points it elsewhere.
    pub api_base: Option<String>,
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct AiSearchError(pub String);

/// One query.
#[derive(Debug, Clone)]
pub struct SemanticQuery {
    pub query: String,
    pub limit: u32,
    pub min_score: f64,
}

/// One matching passage, with the object key already turned into a vault path.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticHit {
    pub path: String,
    pub score: f64,
    pub text: String,
    pub timestamp: Option<f64>,
}

#[derive(Deserialize)]
struct ChunkItem {
    key: String,
    timestamp: Option<f64>,
}

#[derive(Deserialize)]
struct Chunk {
    score: f64,
    text: String,
    item: ChunkItem,
}

#[derive(Deserialize)]
struct ChunkList {
    #[serde(default)]
    chunks: Vec<Chunk>,
}

/// The REST API wraps the payload in `result`; the Workers binding does not. Accept both.
#[derive(Deserialize)]
struct SearchResponse {
    result: Option<ChunkList>,
    chunks: Option<Vec<Chunk>>,
}

struct SyncState {
    timer: Option<JoinHandle<()>>,
    last_sync_at: Option<Instant>,
}

pub struct AiSearchClient {
    http: reqwest::Client,
    base: String,
    token: String,
    prefix: String,
    sync: Mutex<SyncState>,
}

impl AiSearchClient {
    pub fn new(options: AiSearchOptions) -> Self {
        let api = options.api_base.as_deref().unwrap_or(DEFAULT_API_BASE).trim_end_matches('/');
        let base = format!(
            "{api}/accounts/{}/ai-search/namespaces/{}/instances/{}",
            encode_uri_component(&options.account_id),
            encode_uri_component(&options.namespace),
            encode_uri_component(&options.instance)
        );
        Self {
            http: reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build().unwrap_or_default(),
            base,
            token: options.token,
            prefix: normalize_prefix(&options.prefix),
            sync: Mutex::new(SyncState { timer: None, last_sync_at: None }),
        }
    }

    async fn call(&self, path: &str, body: Value) -> Result<Value, AiSearchError> {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| AiSearchError(format!("AI Search unreachable: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            let hint = match status.as_u16() {
                401 | 403 => "the token was rejected; it needs AI Search Edit and Run permissions".to_owned(),
                404 => "no such instance; check CF_AI_SEARCH_INSTANCE and CF_AI_SEARCH_NAMESPACE".to_owned(),
                429 => "rate limited; try again shortly".to_owned(),
                _ if !detail.is_empty() => take_chars(&detail, 300).to_owned(),
                _ => status.canonical_reason().unwrap_or("").to_owned(),
            };
            return Err(AiSearchError(format!("AI Search {path} failed ({}): {hint}", status.as_u16())));
        }
        response.json().await.map_err(|_| AiSearchError("AI Search returned an unexpected response.".to_owned()))
    }

    /// Vault path for an object key, or `None` when the key is not a note of this vault.
    pub fn path_of(&self, key: &str) -> Option<String> {
        let path = key.strip_prefix(&self.prefix)?;
        is_mirrored_path(path).then(|| path.to_owned())
    }

    pub async fn search(&self, query: &SemanticQuery) -> Result<Vec<SemanticHit>, AiSearchError> {
        // The same body Wrangler's `ai-search search` sends; retrieval mode is an instance setting.
        let raw = self
            .call(
                "/search",
                json!({
                    "messages": [{ "role": "user", "content": query.query }],
                    "max_num_results": query.limit,
                    "score_threshold": query.min_score,
                }),
            )
            .await?;
        let parsed: SearchResponse = serde_json::from_value(raw)
            .map_err(|_| AiSearchError("AI Search returned an unexpected response.".to_owned()))?;
        let chunks = match parsed.result {
            Some(result) => result.chunks,
            None => parsed.chunks.unwrap_or_default(),
        };
        Ok(chunks
            .into_iter()
            .filter_map(|chunk| {
                Some(SemanticHit {
                    path: self.path_of(&chunk.item.key)?,
                    score: chunk.score,
                    text: chunk.text,
                    timestamp: chunk.item.timestamp,
                })
            })
            .collect())
    }

    /// Start an indexing job now.
    pub async fn create_job(&self) -> Result<(), AiSearchError> {
        self.call("/jobs", json!({})).await.map(drop)
    }

    /// A note changed on the server's side: re-index soon. Calls within a
    /// minute collapse into one job, and jobs stay at least 30 s apart.
    pub fn request_sync(self: &Arc<Self>) {
        let mut sync = self.sync.lock();
        if sync.timer.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let since_last = sync.last_sync_at.map(|t| t.elapsed());
        let delay = match since_last {
            Some(elapsed) => SYNC_DEBOUNCE.max(SYNC_MIN_INTERVAL.saturating_sub(elapsed)),
            None => SYNC_DEBOUNCE,
        };
        let client = Arc::clone(self);
        sync.timer = Some(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            {
                let mut sync = client.sync.lock();
                sync.timer = None;
                sync.last_sync_at = Some(Instant::now());
            }
            match client.create_job().await {
                Ok(()) => info!("AI Search: re-index requested after a write."),
                Err(error) => warn!("AI Search: re-index request failed: {error}"),
            }
        }));
    }

    /// Whether a re-index request is waiting to be sent.
    pub fn sync_pending(&self) -> bool {
        self.sync.lock().timer.as_ref().is_some_and(|t| !t.is_finished())
    }

    pub fn close(&self) {
        if let Some(timer) = self.sync.lock().timer.take() {
            timer.abort();
        }
    }
}
