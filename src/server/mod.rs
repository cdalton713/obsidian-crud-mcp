//! The HTTP server and the background work around it.

mod authenticate;
mod bootstrap;
mod http;
mod watcher;

pub use authenticate::Authenticator;
pub use bootstrap::sync_search_index;
pub use http::router;
pub use watcher::{VaultWatcher, watch_vault};
