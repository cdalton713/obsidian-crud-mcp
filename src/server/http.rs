//! Routes: `/mcp` (JSON-RPC), `/health`, and the OAuth endpoints in token mode.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use tower_http::cors::{Any, CorsLayer};

use super::Authenticator;
use crate::auth::OAuthProvider;
use crate::mcp::{McpServer, codes};

struct AppState {
    mcp: McpServer,
    auth: Authenticator,
}

fn json_response(status: StatusCode, body: &Value) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

async fn handle_mcp(State(app): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(refusal) = app.auth.check(&headers) {
        return refusal;
    }
    let message: Value = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(e) => {
            let error = json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": codes::PARSE_ERROR, "message": format!("Parse error: {e}") },
            });
            return json_response(StatusCode::BAD_REQUEST, &error);
        }
    };
    match message {
        Value::Array(batch) => {
            let mut responses = Vec::new();
            for message in batch {
                responses.extend(app.mcp.handle(message).await);
            }
            if responses.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                json_response(StatusCode::OK, &Value::Array(responses))
            }
        }
        message => match app.mcp.handle(message).await {
            Some(response) => json_response(StatusCode::OK, &response),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

/// Stateless mode keeps no sessions and opens no server-to-client stream.
async fn method_not_allowed(State(app): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(refusal) = app.auth.check(&headers) {
        return refusal;
    }
    let error = json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": -32000, "message": "Method not allowed." },
    });
    let mut response = json_response(StatusCode::METHOD_NOT_ALLOWED, &error);
    response.headers_mut().insert(header::ALLOW, header::HeaderValue::from_static("POST"));
    response
}

async fn health() -> &'static str {
    "✓ Ok"
}

/// The whole HTTP surface. Responses carry wildcard CORS so browser-based MCP
/// clients work; the authenticator decides who gets through.
pub fn router(mcp: McpServer, auth: Authenticator, oauth: Option<&Arc<OAuthProvider>>) -> Router {
    let state = Arc::new(AppState { mcp, auth });
    let mut app = Router::new()
        .route("/mcp", get(method_not_allowed).post(handle_mcp).delete(method_not_allowed))
        .route("/health", get(health))
        .with_state(state);
    if let Some(oauth) = oauth {
        app = app.merge(oauth.router());
    }
    app.layer(
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
            .expose_headers([header::WWW_AUTHENTICATE, header::HeaderName::from_static("mcp-session-id")]),
    )
}
