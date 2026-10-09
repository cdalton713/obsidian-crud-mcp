//! Routes: `/mcp` (JSON-RPC), `/health`, and the OAuth endpoints in token mode.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::header;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use tower_http::cors::{Any, CorsLayer};

use super::Authenticator;
use crate::auth::OAuthProvider;
use crate::mcp::McpServer;

async fn authenticate(State(auth): State<Arc<Authenticator>>, request: Request, next: Next) -> Response {
    if let Err(refusal) = auth.check(request.headers()) {
        return *refusal;
    }
    next.run(request).await
}

async fn health() -> &'static str {
    "✓ Ok"
}

/// The whole HTTP surface. Responses carry wildcard CORS so browser-based MCP
/// clients work; the authenticator decides who gets through.
pub fn router(mcp: McpServer, auth: Authenticator, oauth: Option<&Arc<OAuthProvider>>) -> Router {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        // Authenticator owns Host/Origin checks, including authenticated remote access.
        .with_allowed_hosts(Vec::<String>::new());
    let service = StreamableHttpService::new(move || Ok(mcp.clone()), Arc::new(NeverSessionManager::default()), config);
    let mcp_routes = Router::new()
        .route_service("/mcp", service)
        .route_layer(middleware::from_fn_with_state(Arc::new(auth), authenticate));
    let mut app = mcp_routes.route("/health", get(health));
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
