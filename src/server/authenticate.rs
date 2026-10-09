//! Who may call `/mcp`.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tracing::{info, warn};

use crate::auth::host_guard::{build_allowed_hosts, is_host_allowed, is_origin_allowed};
use crate::auth::{OAuthProvider, safe_equal};

pub enum Authenticator {
    /// Accept the static bearer token (curl, MCP Inspector, custom agents) or an
    /// OAuth-issued token (Claude Web, Desktop and Mobile).
    Token { expected: String, base_url: String, oauth: Arc<OAuthProvider> },
    /// No token configured: enforce a Host/Origin allowlist so the "local only"
    /// precondition actually holds. Without it, DNS rebinding lets any website
    /// the operator visits reach the tools (CWE-350) even on a loopback bind,
    /// because the browser still sends the attacker's hostname in Host.
    LocalOnly { allowed_hosts: BTreeSet<String> },
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

impl Authenticator {
    pub fn token(auth_token: &str, base_url: &str, oauth: Arc<OAuthProvider>) -> Self {
        info!("Auth enabled (password-gated OAuth).");
        Self::Token { expected: format!("Bearer {auth_token}"), base_url: base_url.to_owned(), oauth }
    }

    pub fn local_only(extra_hosts: Option<&str>, bind_host: &str) -> Self {
        let allowed_hosts = build_allowed_hosts(extra_hosts);
        let list: Vec<&str> = allowed_hosts.iter().map(String::as_str).collect();
        info!(
            "Auth disabled: accepting only local Host/Origin headers: {}. Set MCP_ALLOWED_HOSTS to add hosts, or MCP_AUTH_TOKEN for authenticated remote access.",
            list.join(", ")
        );
        if bind_host == "0.0.0.0" {
            warn!(
                "WARNING: No authentication and listening on all interfaces. Browser attacks (DNS rebinding and cross-origin fetch) are blocked by the Host/Origin checks, but any non-browser client that can reach this port has full vault access. Set MCP_AUTH_TOKEN, or HOST=127.0.0.1 to bind to loopback only."
            );
        }
        Self::LocalOnly { allowed_hosts }
    }

    /// `Ok` to let the request through, or the response that refuses it.
    pub fn check(&self, headers: &HeaderMap) -> Result<(), Box<Response>> {
        match self {
            Self::Token { expected, base_url, oauth } => {
                let authorization = header_str(headers, header::AUTHORIZATION);
                if authorization.is_some_and(|h| safe_equal(h, expected)) || oauth.validate_token(authorization) {
                    return Ok(());
                }
                // RFC 9728: point strict clients at the resource metadata; Claude
                // probes /.well-known directly but others rely on this.
                let challenge = format!("Bearer resource_metadata=\"{base_url}/.well-known/oauth-protected-resource\"");
                let mut response = (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
                if let Ok(value) = HeaderValue::from_str(&challenge) {
                    response.headers_mut().insert(header::WWW_AUTHENTICATE, value);
                }
                Err(Box::new(response))
            }
            Self::LocalOnly { allowed_hosts } => {
                // The Host check defeats DNS rebinding; the Origin check defeats a
                // direct cross-origin browser fetch to loopback (responses carry wildcard CORS).
                if !is_host_allowed(header_str(headers, header::HOST), allowed_hosts) {
                    return Err(Box::new((StatusCode::FORBIDDEN, "Forbidden: Host not allowed").into_response()));
                }
                if !is_origin_allowed(header_str(headers, header::ORIGIN), allowed_hosts) {
                    return Err(Box::new(
                        (StatusCode::FORBIDDEN, "Forbidden: cross-origin request rejected").into_response(),
                    ));
                }
                Ok(())
            }
        }
    }
}
