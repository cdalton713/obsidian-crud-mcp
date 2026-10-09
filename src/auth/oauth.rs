//! Password-gated OAuth provider for MCP.
//!
//! A self-contained OAuth 2.1 flow:
//! - the client connects and gets a 401 pointing at the resource metadata
//! - it discovers `/.well-known/oauth-protected-resource`
//! - it registers via `/oauth/register` (dynamic client registration)
//! - it sends the user to `/oauth/authorize`
//! - the user sees a password page and enters `MCP_AUTH_TOKEN`
//! - the client exchanges the code for an access token via `/oauth/token`
//! - every later request carries `Bearer <token>`
//!
//! No external identity provider needed.

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use parking_lot::Mutex;
use rand::RngCore;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};

use crate::logging::describe_error;
use crate::util::{now_ms, take_chars};

const TOKEN_EXPIRY_MS: i64 = 3600 * 1000;
const DAY_MS: i64 = 24 * 3600 * 1000;
const MAX_FAILED_BEFORE_LOCKOUT: u32 = 5;
/// Doubles with each lockout, up to ten doublings (about 85 minutes).
const BASE_LOCKOUT_MS: i64 = 5 * 1000;
const MAX_CLIENTS: usize = 100;
const MAX_PENDING: usize = 100;
const PENDING_TTL_MS: i64 = 10 * 60 * 1000;

const PASSWORD_PAGE: &str = include_str!("password_page.html");

static BEARER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^Bearer\s+(\S+)$").unwrap());
static BASIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^Basic\s+(\S+)$").unwrap());

/// Lenient base64 for Basic credentials: padding optional.
const BASIC_DECODER: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

fn new_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Constant-time comparison for secrets. Comparing SHA-256 digests gives equal
/// lengths, so the secret's length is not revealed either.
pub fn safe_equal(a: &str, b: &str) -> bool {
    let (da, db) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    da.iter().zip(db.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Debug, Clone)]
struct PendingAuth {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    state: String,
    created_at: i64,
    /// Set only once the password step succeeded; until then the code cannot be redeemed.
    approved: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenRecord {
    access_token: String,
    refresh_token: String,
    client_id: String,
    expires_at: i64,
    refresh_expires_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AuthMethod {
    /// Registrations persisted before the field existed were confidential clients.
    #[default]
    ClientSecretPost,
    None,
}

impl AuthMethod {
    fn as_str(self) -> &'static str {
        match self {
            AuthMethod::ClientSecretPost => "client_secret_post",
            AuthMethod::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisteredClient {
    client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    #[serde(default)]
    token_endpoint_auth_method: AuthMethod,
    redirect_uris: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<i64>,
}

#[derive(Default)]
struct AuthState {
    pending: HashMap<String, PendingAuth>,
    /// Authorization code to the CSRF token of its password form.
    csrf: HashMap<String, String>,
    tokens: HashMap<String, TokenRecord>,
    refresh_tokens: HashMap<String, TokenRecord>,
    clients: HashMap<String, RegisteredClient>,
    // Rate limiting: exponential backoff that only resets on success.
    failed_attempts: u32,
    lockout_count: u32,
    locked_until: i64,
}

impl AuthState {
    /// Drop expired pending authorizations and their CSRF tokens.
    fn cleanup_pending(&mut self, now: i64) {
        let expired: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| now - p.created_at > PENDING_TTL_MS)
            .map(|(code, _)| code.clone())
            .collect();
        for code in expired {
            self.pending.remove(&code);
            self.csrf.remove(&code);
        }
    }
}

/// On-disk shape of the persisted OAuth state.
#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Persisted {
    #[serde(default)]
    tokens: HashMap<String, Value>,
    #[serde(default)]
    refresh_tokens: HashMap<String, Value>,
    #[serde(default)]
    clients: HashMap<String, Value>,
}

pub struct OAuthProvider {
    base_url: String,
    password: String,
    persist_path: Option<PathBuf>,
    refresh_expiry_ms: i64,
    state: Mutex<AuthState>,
    /// Serializes writes of the persisted state.
    persisting: tokio::sync::Mutex<()>,
}

fn host_of(uri: &str) -> String {
    match url::Url::parse(uri) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_owned(),
            _ => String::new(),
        },
        Err(_) => uri.to_owned(),
    }
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn render_password_page(
    code: &str,
    csrf: &str,
    redirect_host: &str,
    client_name: Option<&str>,
    error: Option<&str>,
) -> String {
    let who = match client_name {
        Some(name) => format!("<b>{}</b> (name self-reported)", escape_html(name)),
        None => "An application".to_owned(),
    };
    let error_html = error
        .map(|e| format!(r#"<p class="error" id="password-error" role="alert">{}</p>"#, escape_html(e)))
        .unwrap_or_default();
    PASSWORD_PAGE
        .replace("{{WHO}}", &who)
        .replace("{{REDIRECT_HOST}}", &escape_html(redirect_host))
        .replace("{{ERROR}}", &error_html)
        .replace("{{CODE}}", &escape_html(code))
        .replace("{{CSRF}}", &escape_html(csrf))
        .replace("{{DESCRIBED_BY}}", if error.is_some() { " password-error" } else { "" })
        .replace("{{INVALID}}", if error.is_some() { r#" aria-invalid="true""# } else { "" })
}

fn json_error(status: StatusCode, error: &str, description: Option<&str>) -> Response {
    let mut body = json!({ "error": error });
    if let Some(description) = description {
        body["error_description"] = json!(description);
    }
    (status, Json(body)).into_response()
}

/// Strict `decodeURIComponent`: `None` for a malformed escape or invalid UTF-8.
fn decode_uri_component(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Form fields from a urlencoded (or, leniently, JSON) body.
fn parse_body(headers: &HeaderMap, body: &[u8]) -> HashMap<String, String> {
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    if content_type.starts_with("application/json") {
        let fields: HashMap<String, Value> = serde_json::from_slice(body).unwrap_or_default();
        return fields
            .into_iter()
            .filter_map(|(k, v)| match v {
                Value::String(s) => Some((k, s)),
                _ => None,
            })
            .collect();
    }
    serde_urlencoded::from_bytes::<Vec<(String, String)>>(body)
        .map(|pairs| pairs.into_iter().collect())
        .unwrap_or_default()
}

struct ClientCredentials {
    client_id: Option<String>,
    secret: Option<String>,
}

/// Client credentials from HTTP Basic (`client_secret_basic`, RFC 6749 §2.3.1)
/// or the request body (`client_secret_post`). `None` for a malformed Basic
/// header or one that disagrees with credentials in the body.
fn client_credentials(authorization: Option<&str>, body: &HashMap<String, String>) -> Option<ClientCredentials> {
    let from_body =
        ClientCredentials { client_id: body.get("client_id").cloned(), secret: body.get("client_secret").cloned() };
    let Some(encoded) = authorization.and_then(|a| BASIC.captures(a)).map(|c| c[1].to_owned()) else {
        return Some(from_body);
    };
    let decoded = BASIC_DECODER.decode(&encoded).or_else(|_| STANDARD.decode(&encoded)).ok()?;
    let decoded = String::from_utf8_lossy(&decoded);
    let (id, secret) = decoded.split_once(':')?;
    let client_id = decode_uri_component(&id.replace('+', " "))?;
    let secret = decode_uri_component(&secret.replace('+', " "))?;
    if from_body.client_id.as_ref().is_some_and(|b| *b != client_id) {
        return None;
    }
    if from_body.secret.as_ref().is_some_and(|b| *b != secret) {
        return None;
    }
    Some(ClientCredentials { client_id: Some(client_id), secret: Some(secret) })
}

impl OAuthProvider {
    pub fn new(base_url: &str, password: &str, persist_path: Option<PathBuf>, refresh_days: u32) -> Arc<Self> {
        if !base_url.starts_with("https://") && !base_url.contains("localhost") {
            warn!(
                "WARNING: BASE_URL is not HTTPS. OAuth tokens will be sent in cleartext. Use a tunnel (cloudflared, tailscale, ngrok) to provide TLS."
            );
        }
        Arc::new(Self {
            base_url: base_url.to_owned(),
            password: password.to_owned(),
            persist_path,
            refresh_expiry_ms: i64::from(refresh_days) * DAY_MS,
            state: Mutex::new(AuthState::default()),
            persisting: tokio::sync::Mutex::new(()),
        })
    }

    /// The discovery, registration, authorization and token routes.
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/.well-known/oauth-protected-resource", get(protected_resource))
            .route("/.well-known/oauth-authorization-server", get(authorization_server))
            .route("/oauth/register", post(register))
            .route("/oauth/authorize", get(authorize))
            .route("/oauth/approve", post(approve))
            .route("/oauth/token", post(token))
            .with_state(Arc::clone(self))
    }

    /// Whether an `Authorization` header carries a live access token.
    pub fn validate_token(&self, authorization: Option<&str>) -> bool {
        // RFC 6750: the auth scheme is case-insensitive.
        let Some(token) = authorization.and_then(|a| BEARER.captures(a)).map(|c| c[1].to_owned()) else {
            return false;
        };
        let mut state = self.state.lock();
        match state.tokens.get(&token) {
            None => false,
            Some(record) if now_ms() > record.expires_at => {
                state.tokens.remove(&token);
                false
            }
            Some(_) => true,
        }
    }

    /// Drop expired tokens and pending authorizations.
    ///
    /// Registered clients are not evicted here: AI clients cache their
    /// client_id indefinitely and retry it after token expiry, so dropping a
    /// registration means "Unknown client" until the user deletes and re-adds
    /// the connector. The client list is bounded at registration time instead.
    pub fn cleanup(&self) {
        let now = now_ms();
        let mut state = self.state.lock();
        state.cleanup_pending(now);
        state.tokens.retain(|_, r| r.expires_at > now);
        state.refresh_tokens.retain(|_, r| r.refresh_expires_at > now);
    }

    /// Persist clients and tokens. Called on every state change (not just the
    /// periodic save): a restart or suspend right after registration or token
    /// issuance must not lose the new state.
    pub async fn save_tokens(&self) {
        let Some(path) = &self.persist_path else { return };
        let _persisting = self.persisting.lock().await;
        let data = {
            let now = now_ms();
            let state = self.state.lock();
            let to_values = |records: Vec<(&String, &TokenRecord)>| -> HashMap<String, Value> {
                records.into_iter().map(|(k, r)| (k.clone(), json!(r))).collect()
            };
            let persisted = Persisted {
                tokens: to_values(state.tokens.iter().filter(|(_, r)| r.expires_at > now).collect()),
                refresh_tokens: to_values(
                    state.refresh_tokens.iter().filter(|(_, r)| r.refresh_expires_at > now).collect(),
                ),
                clients: state.clients.iter().map(|(k, c)| (k.clone(), json!(c))).collect(),
            };
            serde_json::to_vec(&persisted).unwrap_or_default()
        };
        // Write-then-rename so a crash mid-write never leaves truncated JSON,
        // which load_tokens would discard along with every client registration.
        let mut tmp = path.clone().into_os_string();
        tmp.push(format!(".{}.{}.tmp", std::process::id(), uuid::Uuid::new_v4()));
        let result = async {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&tmp).await?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &data).await?;
            drop(file);
            tokio::fs::rename(&tmp, path).await
        }
        .await;
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&tmp).await;
            error!("Failed to save auth tokens: {}", describe_error(&e));
        }
    }

    /// Load persisted clients and live tokens. Returns whether any sessions were restored.
    pub async fn load_tokens(&self) -> bool {
        let Some(path) = &self.persist_path else { return false };
        let raw = match tokio::fs::read(path).await {
            Ok(raw) => raw,
            Err(e) => {
                if e.kind() != ErrorKind::NotFound {
                    warn!("Failed to load auth tokens; starting with none: {}", describe_error(&e));
                }
                return false;
            }
        };
        let data: Persisted = match serde_json::from_slice(&raw) {
            Ok(data) => data,
            Err(e) => {
                warn!("Failed to load auth tokens; starting with none: {}", describe_error(&e));
                return false;
            }
        };
        let now = now_ms();
        let mut state = self.state.lock();
        for (key, value) in data.tokens {
            if let Ok(record) = serde_json::from_value::<TokenRecord>(value) {
                if record.expires_at > now {
                    state.tokens.insert(key, record);
                }
            }
        }
        for (key, value) in data.refresh_tokens {
            if let Ok(record) = serde_json::from_value::<TokenRecord>(value) {
                if record.refresh_expires_at > now {
                    state.refresh_tokens.insert(key, record);
                }
            }
        }
        for (key, value) in data.clients {
            if let Ok(client) = serde_json::from_value::<RegisteredClient>(value) {
                state.clients.insert(key, client);
            }
        }
        info!("Auth tokens loaded from disk ({} sessions).", state.tokens.len());
        !state.tokens.is_empty()
    }

    fn authenticate_client(&self, client_id: Option<&str>, secret: Option<&str>) -> bool {
        let state = self.state.lock();
        let Some(client) = client_id.and_then(|id| state.clients.get(id)) else { return false };
        if client.token_endpoint_auth_method == AuthMethod::None {
            return true;
        }
        // Older persisted registrations have a secret but no method field.
        matches!((secret, &client.client_secret), (Some(given), Some(expected)) if safe_equal(given, expected))
    }

    async fn issue_tokens(&self, client_id: &str, refresh_expires_at: i64) -> Value {
        let record = TokenRecord {
            access_token: new_secret(),
            refresh_token: new_secret(),
            client_id: client_id.to_owned(),
            expires_at: now_ms() + TOKEN_EXPIRY_MS,
            refresh_expires_at,
        };
        {
            let mut state = self.state.lock();
            state.tokens.insert(record.access_token.clone(), record.clone());
            state.refresh_tokens.insert(record.refresh_token.clone(), record.clone());
        }
        self.save_tokens().await;
        json!({
            "access_token": record.access_token,
            "token_type": "Bearer",
            "expires_in": TOKEN_EXPIRY_MS / 1000,
            "refresh_token": record.refresh_token,
        })
    }
}

// --- Discovery endpoints ---

async fn protected_resource(State(auth): State<Arc<OAuthProvider>>) -> Json<Value> {
    Json(json!({
        "resource": auth.base_url,
        "authorization_servers": [auth.base_url],
        "scopes_supported": ["mcp"],
    }))
}

async fn authorization_server(State(auth): State<Arc<OAuthProvider>>) -> Json<Value> {
    let base = &auth.base_url;
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "none"],
        "scopes_supported": ["mcp"],
    }))
}

// --- Dynamic Client Registration (RFC 7591) ---

fn is_safe_redirect(uri: &Value) -> bool {
    let Some(uri) = uri.as_str() else { return false };
    let lower = uri.to_lowercase();
    uri.encode_utf16().count() <= 2048
        && !lower.starts_with("javascript:")
        && !lower.starts_with("data:")
        && !lower.starts_with("file:")
}

async fn register(State(auth): State<Arc<OAuthProvider>>, body: Bytes) -> Response {
    {
        let mut state = auth.state.lock();
        if state.clients.len() >= MAX_CLIENTS {
            // Evict the oldest client with no live tokens to make room; only
            // reject when every slot is held by a client with active tokens.
            let active: HashSet<&str> =
                state.tokens.values().chain(state.refresh_tokens.values()).map(|r| r.client_id.as_str()).collect();
            let oldest = state
                .clients
                .values()
                .filter(|c| !active.contains(c.client_id.as_str()))
                .min_by_key(|c| c.created_at.unwrap_or(0))
                .map(|c| c.client_id.clone());
            match oldest {
                Some(id) => {
                    state.clients.remove(&id);
                }
                None => return json_error(StatusCode::TOO_MANY_REQUESTS, "too_many_clients", None),
            }
        }
    }
    let Ok(Value::Object(metadata)) = serde_json::from_slice::<Value>(&body) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid_client_metadata", Some("JSON body required"));
    };
    let redirect_uris: Vec<Value> = match metadata.get("redirect_uris") {
        Some(Value::Array(uris)) => uris.iter().take(5).cloned().collect(),
        _ => Vec::new(),
    };
    if redirect_uris.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "invalid_client_metadata", Some("redirect_uris required"));
    }
    if !redirect_uris.iter().all(is_safe_redirect) {
        return json_error(StatusCode::BAD_REQUEST, "invalid_client_metadata", Some("invalid redirect_uri"));
    }
    let redirect_uris: Vec<String> = redirect_uris.iter().filter_map(|u| u.as_str().map(str::to_owned)).collect();

    // Honor the client's requested auth method (RFC 7591 §2). Only
    // "client_secret_post" and "none" are advertised as supported, so anything
    // else falls back to the confidential-client default rather than silently
    // registering a method we don't understand.
    let requested = metadata.get("token_endpoint_auth_method");
    let method =
        if requested.and_then(Value::as_str) == Some("none") { AuthMethod::None } else { AuthMethod::ClientSecretPost };
    let client = RegisteredClient {
        client_id: uuid::Uuid::new_v4().to_string(),
        client_secret: (method != AuthMethod::None).then(new_secret),
        token_endpoint_auth_method: method,
        redirect_uris,
        client_name: metadata.get("client_name").and_then(Value::as_str).map(|n| take_chars(n, 256).to_owned()),
        created_at: Some(now_ms()),
    };
    auth.state.lock().clients.insert(client.client_id.clone(), client.clone());
    auth.save_tokens().await;
    info!(
        "Auth: registered client_id={} redirect_uris={} requested_auth_method={} (responding with {})",
        client.client_id,
        json!(client.redirect_uris),
        requested.cloned().unwrap_or_else(|| json!("(unspecified)")),
        method.as_str()
    );
    let mut response = json!({
        "client_id": client.client_id,
        "redirect_uris": client.redirect_uris,
        "client_name": client.client_name,
        "token_endpoint_auth_method": method.as_str(),
    });
    if let Some(secret) = &client.client_secret {
        response["client_secret"] = json!(secret);
    }
    (StatusCode::CREATED, Json(response)).into_response()
}

// --- Authorization endpoint ---

async fn authorize(State(auth): State<Arc<OAuthProvider>>, Query(query): Query<HashMap<String, String>>) -> Response {
    let param = |name: &str| query.get(name).cloned().unwrap_or_default();
    let (client_id, redirect_uri, code_challenge, state_param) =
        (param("client_id"), param("redirect_uri"), param("code_challenge"), param("state"));
    let method = query.get("code_challenge_method").cloned().unwrap_or_else(|| "S256".to_owned());

    let mut state = auth.state.lock();
    // Validate the redirect URI against the registered client.
    let Some(client) = state.clients.get(&client_id).cloned() else {
        warn!("Auth: /oauth/authorize unknown client_id={}", json!(client_id));
        return (StatusCode::BAD_REQUEST, "Unknown client").into_response();
    };
    if !client.redirect_uris.contains(&redirect_uri) {
        warn!(
            "Auth: /oauth/authorize redirect_uri mismatch. received={} registered={}",
            json!(redirect_uri),
            json!(client.redirect_uris)
        );
        return (StatusCode::BAD_REQUEST, "Invalid redirect URI").into_response();
    }
    // Require S256 PKCE.
    if method != "S256" || code_challenge.is_empty() {
        warn!(
            "Auth: /oauth/authorize missing/unsupported PKCE. method={} challenge_present={}",
            json!(method),
            !code_challenge.is_empty()
        );
        return (StatusCode::BAD_REQUEST, "PKCE with S256 is required").into_response();
    }
    state.cleanup_pending(now_ms());
    if state.pending.len() >= MAX_PENDING {
        return (StatusCode::TOO_MANY_REQUESTS, "Too many pending authorizations").into_response();
    }
    let code = new_secret();
    state.pending.insert(
        code.clone(),
        PendingAuth {
            client_id: client_id.clone(),
            redirect_uri: redirect_uri.clone(),
            code_challenge,
            state: state_param,
            created_at: now_ms(),
            approved: false,
        },
    );
    info!("Auth: /oauth/authorize accepted client_id={client_id} redirect_uri={}", json!(redirect_uri));
    let csrf = new_secret();
    state.csrf.insert(code.clone(), csrf.clone());
    Html(render_password_page(&code, &csrf, &host_of(&redirect_uri), client.client_name.as_deref(), None))
        .into_response()
}

// --- Approval handler ---

async fn approve(State(auth): State<Arc<OAuthProvider>>, headers: HeaderMap, body: Bytes) -> Response {
    let form = parse_body(&headers, &body);
    let field = |name: &str| form.get(name).map(String::as_str);
    let code = field("code").unwrap_or_default().to_owned();

    let mut state = auth.state.lock();
    let (Some(pending), Some(expected_csrf)) = (state.pending.get(&code).cloned(), state.csrf.get(&code).cloned())
    else {
        return (StatusCode::BAD_REQUEST, Html("<p>Invalid or expired authorization request.</p>")).into_response();
    };
    if !field("csrf").is_some_and(|csrf| safe_equal(csrf, &expected_csrf)) {
        return (StatusCode::FORBIDDEN, Html("<p>Invalid request.</p>")).into_response();
    }

    // Re-render the password form with a fresh CSRF token.
    let client_name = state.clients.get(&pending.client_id).and_then(|c| c.client_name.clone());
    let rerender = |state: &mut AuthState, message: &str, status: StatusCode| {
        let csrf = new_secret();
        state.csrf.insert(code.clone(), csrf.clone());
        let page =
            render_password_page(&code, &csrf, &host_of(&pending.redirect_uri), client_name.as_deref(), Some(message));
        (status, Html(page)).into_response()
    };

    let now = now_ms();
    if now < state.locked_until {
        let wait = (state.locked_until - now + 999) / 1000;
        warn!("Auth: locked out, {wait}s remaining");
        return rerender(
            &mut state,
            &format!("Too many attempts. Try again in {wait} seconds."),
            StatusCode::TOO_MANY_REQUESTS,
        );
    }
    if !field("password").is_some_and(|p| safe_equal(p, &auth.password)) {
        state.failed_attempts += 1;
        warn!("Auth: failed attempt {} total", state.failed_attempts);
        if state.failed_attempts >= MAX_FAILED_BEFORE_LOCKOUT {
            state.lockout_count = (state.lockout_count + 1).min(10);
            let lockout_ms = BASE_LOCKOUT_MS * (1 << (state.lockout_count - 1));
            state.locked_until = now + lockout_ms;
            warn!("Auth: lockout #{}, {}s", state.lockout_count, lockout_ms / 1000);
            let message = format!("Too many attempts. Try again in {} seconds.", lockout_ms / 1000);
            return rerender(&mut state, &message, StatusCode::TOO_MANY_REQUESTS);
        }
        return rerender(&mut state, "Wrong password.", StatusCode::UNAUTHORIZED);
    }

    // Password correct: reset the rate limit and mark the code redeemable.
    // Until now /oauth/token refuses it, even though the code was in the page.
    state.failed_attempts = 0;
    state.lockout_count = 0;
    state.locked_until = 0;
    state.csrf.remove(&code);
    if let Some(pending) = state.pending.get_mut(&code) {
        pending.approved = true;
    }
    drop(state);
    info!("Auth: password accepted, issuing authorization code.");

    let Ok(mut url) = url::Url::parse(&pending.redirect_uri) else {
        return (StatusCode::BAD_REQUEST, Html("<p>Invalid redirect URI.</p>")).into_response();
    };
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "code" && (pending.state.is_empty() || k != "state"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    {
        let mut pairs = url.query_pairs_mut();
        pairs.clear().extend_pairs(kept).append_pair("code", &code);
        if !pending.state.is_empty() {
            pairs.append_pair("state", &pending.state);
        }
    }
    (StatusCode::FOUND, [(header::LOCATION, url.to_string())]).into_response()
}

// --- Token endpoint ---

async fn token(State(auth): State<Arc<OAuthProvider>>, headers: HeaderMap, body: Bytes) -> Response {
    let form = parse_body(&headers, &body);
    let grant_type = form.get("grant_type").cloned();
    info!("Auth: /oauth/token request grant_type={}", json!(grant_type));
    let authorization = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    let Some(credentials) = client_credentials(authorization, &form) else {
        warn!("Auth: /oauth/token invalid_client. malformed or conflicting client credentials");
        return json_error(StatusCode::UNAUTHORIZED, "invalid_client", None);
    };
    match grant_type.as_deref() {
        Some("authorization_code") => authorization_code_grant(&auth, &form, credentials).await,
        Some("refresh_token") => refresh_token_grant(&auth, &form, credentials).await,
        _ => {
            warn!("Auth: /oauth/token unsupported_grant_type={}", json!(grant_type));
            json_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", None)
        }
    }
}

async fn authorization_code_grant(
    auth: &OAuthProvider,
    form: &HashMap<String, String>,
    credentials: ClientCredentials,
) -> Response {
    let code = form.get("code").cloned().unwrap_or_default();
    let now = now_ms();
    let pending = {
        let mut state = auth.state.lock();
        let pending = state.pending.get(&code).cloned();
        match pending {
            Some(p) if now - p.created_at <= PENDING_TTL_MS => p,
            other => {
                warn!(
                    "Auth: /oauth/token invalid_grant. code_known={} expired={}",
                    other.is_some(),
                    other.as_ref().map_or("n/a".to_owned(), |_| "true".to_owned())
                );
                if other.is_some() {
                    state.pending.remove(&code);
                }
                return json_error(StatusCode::BAD_REQUEST, "invalid_grant", None);
            }
        }
    };
    // The code is only redeemable once the password step succeeded. It exists
    // from /oauth/authorize onward (and is in the authorize page HTML), so
    // without this check the password gate could be bypassed entirely.
    if !pending.approved {
        warn!("Auth: /oauth/token invalid_grant. code not yet approved (password step not completed)");
        return json_error(StatusCode::BAD_REQUEST, "invalid_grant", Some("authorization not approved"));
    }
    if credentials.client_id.as_deref() != Some(pending.client_id.as_str()) {
        warn!(
            "Auth: /oauth/token client_id mismatch. received={} expected={}",
            json!(credentials.client_id),
            json!(pending.client_id)
        );
        return json_error(StatusCode::BAD_REQUEST, "invalid_grant", Some("client_id mismatch"));
    }
    if !auth.authenticate_client(credentials.client_id.as_deref(), credentials.secret.as_deref()) {
        return json_error(StatusCode::UNAUTHORIZED, "invalid_client", None);
    }
    // The authenticated client gets one redemption attempt (OAuth 2.1 §4.1.3):
    // consume the code now so a failed redirect_uri or PKCE check cannot be
    // retried. A failed client authentication above leaves the code intact.
    auth.state.lock().pending.remove(&code);

    let redirect_uri = form.get("redirect_uri").cloned().unwrap_or_default();
    if redirect_uri != pending.redirect_uri {
        warn!(
            "Auth: /oauth/token redirect_uri mismatch. received={} expected={}",
            json!(redirect_uri),
            json!(pending.redirect_uri)
        );
        return json_error(StatusCode::BAD_REQUEST, "invalid_grant", Some("redirect_uri mismatch"));
    }
    // Verify PKCE (/oauth/authorize only accepts S256).
    let verified = form.get("code_verifier").is_some_and(|verifier| {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        safe_equal(&challenge, &pending.code_challenge)
    });
    if !verified {
        warn!("Auth: /oauth/token PKCE verification failed");
        return json_error(StatusCode::BAD_REQUEST, "invalid_grant", Some("PKCE verification failed"));
    }
    info!("Auth: /oauth/token issuing access token client_id={}", pending.client_id);
    Json(auth.issue_tokens(&pending.client_id, now_ms() + auth.refresh_expiry_ms).await).into_response()
}

async fn refresh_token_grant(
    auth: &OAuthProvider,
    form: &HashMap<String, String>,
    credentials: ClientCredentials,
) -> Response {
    let refresh_token = form.get("refresh_token").cloned().unwrap_or_default();
    let Some(old) = auth.state.lock().refresh_tokens.get(&refresh_token).cloned() else {
        warn!("Auth: /oauth/token refresh_token unknown");
        return json_error(StatusCode::BAD_REQUEST, "invalid_grant", None);
    };
    if !auth.authenticate_client(credentials.client_id.as_deref(), credentials.secret.as_deref()) {
        return json_error(StatusCode::UNAUTHORIZED, "invalid_client", None);
    }
    if credentials.client_id.as_deref() != Some(old.client_id.as_str()) {
        return json_error(StatusCode::BAD_REQUEST, "invalid_grant", Some("client_id mismatch"));
    }
    let expired = now_ms() > old.refresh_expires_at;
    {
        let mut state = auth.state.lock();
        state.tokens.remove(&old.access_token);
        state.refresh_tokens.remove(&refresh_token);
    }
    if expired {
        info!("Auth: refresh token expired, user must re-authenticate.");
        return json_error(StatusCode::BAD_REQUEST, "invalid_grant", Some("Refresh token expired"));
    }
    // Rotation keeps the original refresh expiry.
    Json(auth.issue_tokens(&old.client_id, old.refresh_expires_at).await).into_response()
}

#[cfg(test)]
mod tests;
