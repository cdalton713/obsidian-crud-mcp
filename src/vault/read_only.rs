//! `READ_ONLY` enforcement at the backend layer.
//!
//! `READ_ONLY=true` already keeps the write tools from being registered.
//! Wrapping the backend as well means a write fails even if some future code
//! path reaches `write_note`/`delete_note`/`move_note` without going through a tool.

use std::sync::Arc;

use async_trait::async_trait;

use super::{NoteInfo, NoteListing, Subscription, VaultBackend, VaultChangeListener, VaultError};

pub const READ_ONLY_MESSAGE: &str = "READ_ONLY is enabled: the vault cannot be modified.";

/// Delegates reads to the inner backend and rejects every write.
pub struct ReadOnlyVault {
    inner: Arc<dyn VaultBackend>,
}

impl ReadOnlyVault {
    pub fn new(inner: Arc<dyn VaultBackend>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl VaultBackend for ReadOnlyVault {
    async fn init(&self) -> Result<(), VaultError> {
        self.inner.init().await
    }

    async fn close(&self) {
        self.inner.close().await;
    }

    async fn read_note(&self, path: &str) -> Result<Option<String>, VaultError> {
        self.inner.read_note(path).await
    }

    async fn write_note(&self, _path: &str, _content: &str) -> Result<bool, VaultError> {
        Err(VaultError::ReadOnly)
    }

    async fn delete_note(&self, _path: &str) -> Result<bool, VaultError> {
        Err(VaultError::ReadOnly)
    }

    async fn move_note(&self, _from: &str, _to: &str) -> Result<bool, VaultError> {
        Err(VaultError::ReadOnly)
    }

    async fn get_metadata(&self, path: &str) -> Result<Option<NoteInfo>, VaultError> {
        self.inner.get_metadata(path).await
    }

    async fn list_notes(&self, folder: Option<&str>) -> Result<Vec<String>, VaultError> {
        self.inner.list_notes(folder).await
    }

    async fn list_notes_with_mtime(&self, folder: Option<&str>) -> Result<Vec<NoteListing>, VaultError> {
        self.inner.list_notes_with_mtime(folder).await
    }

    fn subscribe(&self, listener: Arc<dyn VaultChangeListener>) -> Option<Subscription> {
        self.inner.subscribe(listener)
    }
}
