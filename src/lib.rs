//! Give any AI agent access to an Obsidian vault over MCP.
//!
//! The server reads notes from a local folder: either the vault itself
//! (filesystem mode) or a mirror of the S3 bucket Remotely Save syncs to
//! (S3 mode). See `ARCHITECTURE.md` for how the pieces fit together.

pub mod auth;
pub mod config;
pub mod logging;
pub mod mcp;
pub mod notes;
pub mod search;
pub mod server;
pub mod tools;
pub mod util;
pub mod vault;
