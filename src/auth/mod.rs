//! Access control: the password-gated OAuth provider (token mode) and the
//! Host/Origin allowlist (local-only mode).

pub mod host_guard;
mod oauth;

pub use oauth::{OAuthProvider, safe_equal};
