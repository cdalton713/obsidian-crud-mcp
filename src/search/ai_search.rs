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
        // Fix the deadline now: the task may first run later than this call.
        let deadline = Instant::now() + delay;
        let client = Arc::clone(self);
        sync.timer = Some(tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode, Uri};
    use axum::response::IntoResponse;

    use super::*;

    #[derive(Debug, Clone)]
    struct Call {
        path: String,
        authorization: Option<String>,
        body: Value,
    }

    /// An HTTP server that records requests and answers from a queue.
    #[derive(Default)]
    struct FakeApi {
        calls: Mutex<Vec<Call>>,
        responses: Mutex<VecDeque<(u16, String)>>,
    }

    impl FakeApi {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().clone()
        }
    }

    async fn answer(State(api): State<Arc<FakeApi>>, uri: Uri, headers: HeaderMap, body: Bytes) -> impl IntoResponse {
        api.calls.lock().push(Call {
            path: uri.path().to_owned(),
            authorization: headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_owned),
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        });
        let (status, body) = api.responses.lock().pop_front().unwrap_or((500, "no response queued".to_owned()));
        (StatusCode::from_u16(status).unwrap(), [("content-type", "application/json")], body)
    }

    /// Serve `responses` on a local port and return a client pointed at it.
    async fn serve(responses: Vec<(u16, Value)>, prefix: &str) -> (Arc<FakeApi>, Arc<AiSearchClient>) {
        let api = Arc::new(FakeApi::default());
        api.responses.lock().extend(
            responses
                .into_iter()
                .map(|(status, body)| (status, body.as_str().map_or_else(|| body.to_string(), str::to_owned))),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().fallback(answer).with_state(api.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = AiSearchClient::new(AiSearchOptions {
            api_base: Some(format!("http://{address}/client/v4/")),
            prefix: prefix.to_owned(),
            ..options()
        });
        (api, Arc::new(client))
    }

    fn options() -> AiSearchOptions {
        AiSearchOptions {
            account_id: "acct".into(),
            token: "secret".into(),
            namespace: "default".into(),
            instance: "vault".into(),
            prefix: String::new(),
            api_base: None,
        }
    }

    fn chunk(key: &str, score: f64, text: &str) -> Value {
        json!({ "id": "c", "type": "text", "score": score, "text": text, "item": { "key": key, "timestamp": 1 } })
    }

    fn query(limit: u32, min_score: f64) -> SemanticQuery {
        SemanticQuery { query: "q".into(), limit, min_score }
    }

    #[test]
    fn targets_the_cloudflare_api_by_default() {
        let client = AiSearchClient::new(AiSearchOptions { instance: "my vault".into(), ..options() });
        assert_eq!(
            client.base,
            "https://api.cloudflare.com/client/v4/accounts/acct/ai-search/namespaces/default/instances/my%20vault"
        );
    }

    #[test]
    fn maps_object_keys_to_vault_paths() {
        let client = AiSearchClient::new(AiSearchOptions { prefix: "/notes/vault/".into(), ..options() });
        assert_eq!(client.path_of("notes/vault/a/b.md").as_deref(), Some("a/b.md"));
        assert_eq!(client.path_of("notes/vault/img.png"), None);
        assert_eq!(client.path_of("notes/vault/.obsidian/x.md"), None);
        assert_eq!(client.path_of("notes/vault/_debug_remotely_save/log.md"), None);
        assert_eq!(client.path_of("other/b.md"), None);
    }

    #[tokio::test]
    async fn sends_the_query_and_maps_keys_to_vault_paths() {
        let response = json!({
            "success": true,
            "result": { "chunks": [
                chunk("notes/vault/a.md", 0.9, "alpha"),
                chunk("notes/vault/img.png", 0.8, "binary"),
                chunk("notes/vault/.obsidian/x.md", 0.7, "config"),
                chunk("other/b.md", 0.6, "outside the prefix"),
            ] }
        });
        let (api, client) = serve(vec![(200, response)], "/notes/vault/").await;

        let hits = client.search(&query(5, 0.3)).await.unwrap();

        assert_eq!(hits, [SemanticHit { path: "a.md".into(), score: 0.9, text: "alpha".into(), timestamp: Some(1.0) }]);
        let calls = api.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].path, "/client/v4/accounts/acct/ai-search/namespaces/default/instances/vault/search");
        assert_eq!(calls[0].authorization.as_deref(), Some("Bearer secret"));
        assert_eq!(
            calls[0].body,
            json!({
                "messages": [{ "role": "user", "content": "q" }],
                "max_num_results": 5,
                "score_threshold": 0.3,
            })
        );
    }

    #[tokio::test]
    async fn accepts_chunks_without_the_result_wrapper() {
        let (_api, client) =
            serve(vec![(200, json!({ "chunks": [chunk("a.md", 0.5, "a")] })), (200, json!({}))], "").await;
        assert_eq!(client.search(&query(1, 0.0)).await.unwrap().len(), 1);
        assert_eq!(client.search(&query(1, 0.0)).await.unwrap(), [], "no chunks means no hits");
    }

    async fn search_error(status: u16, body: Value) -> String {
        let (_api, client) = serve(vec![(status, body)], "").await;
        client.search(&query(1, 0.0)).await.expect_err("the search should fail").to_string()
    }

    #[tokio::test]
    async fn explains_rejected_tokens_missing_instances_and_rate_limits() {
        let denied = search_error(403, json!("nope")).await;
        assert!(denied.contains("AI Search Edit and Run"), "{denied}");
        assert!(denied.starts_with("AI Search /search failed (403)"), "{denied}");
        assert!(search_error(401, json!("")).await.contains("AI Search Edit and Run"));
        assert!(search_error(404, json!("")).await.contains("CF_AI_SEARCH_INSTANCE"));
        assert!(search_error(429, json!("")).await.contains("rate limited"));
        let detail = search_error(500, json!("x".repeat(400))).await;
        assert!(detail.ends_with(&"x".repeat(300)), "detail is capped: {detail}");
        assert!(!detail.contains(&"x".repeat(301)), "detail is capped: {detail}");
        assert_eq!(search_error(502, json!("")).await, "AI Search /search failed (502): Bad Gateway");
    }

    #[tokio::test]
    async fn rejects_an_unexpected_response() {
        let message = search_error(200, json!({ "result": { "chunks": [{ "score": "high" }] } })).await;
        assert_eq!(message, "AI Search returned an unexpected response.");
        assert_eq!(search_error(200, json!("not json {")).await, "AI Search returned an unexpected response.");
    }

    #[tokio::test]
    async fn reports_an_unreachable_api() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let client = AiSearchClient::new(AiSearchOptions { api_base: Some(format!("http://{address}")), ..options() });
        let message = client.search(&query(1, 0.0)).await.unwrap_err().to_string();
        assert!(message.starts_with("AI Search unreachable:"), "{message}");
    }

    /// Let the server answer in real time, then pause the clock again.
    async fn settle(api: &FakeApi, calls: usize) {
        tokio::time::resume();
        for _ in 0..200 {
            if api.calls().len() >= calls {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::pause();
    }

    // Timers run on a paused clock; HTTP runs with the clock resumed so the
    // request timeout does not fire while the server answers.
    #[tokio::test(start_paused = true)]
    async fn coalesces_write_bursts_into_one_reindex_job() {
        let (api, client) = serve(vec![(200, json!({ "success": true })), (200, json!({ "success": true }))], "").await;
        client.request_sync();
        client.request_sync();
        client.request_sync();
        assert!(client.sync_pending());

        tokio::time::advance(Duration::from_secs(59)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(api.calls().len(), 0, "nothing before the debounce");

        tokio::time::advance(Duration::from_secs(1)).await;
        settle(&api, 1).await;
        let calls = api.calls();
        assert_eq!(calls.len(), 1, "one job for the burst");
        assert!(calls[0].path.ends_with("/instances/vault/jobs"), "{}", calls[0].path);
        assert_eq!(calls[0].body, json!({}));
        assert!(!client.sync_pending());

        client.request_sync();
        tokio::time::advance(Duration::from_secs(60)).await;
        settle(&api, 2).await;
        assert_eq!(api.calls().len(), 2);
        client.close();
    }

    #[tokio::test(start_paused = true)]
    async fn close_cancels_a_pending_reindex() {
        let (api, client) = serve(vec![(200, json!({}))], "").await;
        client.request_sync();
        client.close();
        assert!(!client.sync_pending());
        tokio::time::advance(Duration::from_secs(120)).await;
        settle(&api, 1).await;
        assert_eq!(api.calls().len(), 0);
    }
}
