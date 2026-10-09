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

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use super::*;
    use crate::notes::NoteMetadata;

    /// Records every call that reaches it.
    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<String>>,
    }

    impl Recorder {
        fn record(&self, call: String) {
            self.calls.lock().push(call);
        }
    }

    #[async_trait]
    impl VaultBackend for Recorder {
        async fn init(&self) -> Result<(), VaultError> {
            self.record("init".into());
            Ok(())
        }
        async fn close(&self) {
            self.record("close".into());
        }
        async fn read_note(&self, path: &str) -> Result<Option<String>, VaultError> {
            self.record(format!("read:{path}"));
            Ok(Some("content".into()))
        }
        async fn write_note(&self, path: &str, _content: &str) -> Result<bool, VaultError> {
            self.record(format!("write:{path}"));
            Ok(true)
        }
        async fn delete_note(&self, path: &str) -> Result<bool, VaultError> {
            self.record(format!("delete:{path}"));
            Ok(true)
        }
        async fn move_note(&self, from: &str, to: &str) -> Result<bool, VaultError> {
            self.record(format!("move:{from}->{to}"));
            Ok(true)
        }
        async fn get_metadata(&self, path: &str) -> Result<Option<NoteInfo>, VaultError> {
            self.record(format!("meta:{path}"));
            Ok(Some(NoteInfo { path: path.into(), size: 7, ctime: 1.0, mtime: 2.0, metadata: NoteMetadata::default() }))
        }
        async fn list_notes(&self, _folder: Option<&str>) -> Result<Vec<String>, VaultError> {
            self.record("list".into());
            Ok(vec!["a.md".into()])
        }
        async fn list_notes_with_mtime(&self, _folder: Option<&str>) -> Result<Vec<NoteListing>, VaultError> {
            self.record("listMtime".into());
            Ok(vec![NoteListing { path: "a.md".into(), mtime: 1.0 }])
        }
    }

    struct NoListener;

    impl VaultChangeListener for NoListener {
        fn updated(&self, _path: &str, _content: &str, _mtime: f64) {}
        fn removed(&self, _path: &str) {}
    }

    fn wrapped() -> (Arc<Recorder>, ReadOnlyVault) {
        let recorder = Arc::new(Recorder::default());
        let vault = ReadOnlyVault::new(recorder.clone());
        (recorder, vault)
    }

    fn assert_read_only(result: Result<bool, VaultError>) {
        match result {
            Err(error @ VaultError::ReadOnly) => assert_eq!(error.to_string(), READ_ONLY_MESSAGE),
            other => panic!("expected ReadOnly, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_writes_without_touching_the_backend() {
        let (recorder, vault) = wrapped();
        assert_read_only(vault.write_note("a.md", "x").await);
        assert_read_only(vault.delete_note("a.md").await);
        assert_read_only(vault.move_note("a.md", "b.md").await);
        assert!(recorder.calls.lock().is_empty(), "backend saw {:?}", recorder.calls.lock());
    }

    #[tokio::test]
    async fn delegates_reads_listing_and_lifecycle() {
        let (recorder, vault) = wrapped();
        vault.init().await.unwrap();
        assert_eq!(vault.read_note("a.md").await.unwrap().as_deref(), Some("content"));
        assert_eq!(vault.get_metadata("a.md").await.unwrap().map(|m| m.size), Some(7));
        assert_eq!(vault.list_notes(None).await.unwrap(), ["a.md"]);
        assert_eq!(vault.list_notes_with_mtime(None).await.unwrap(), [NoteListing { path: "a.md".into(), mtime: 1.0 }]);
        vault.close().await;
        assert_eq!(*recorder.calls.lock(), ["init", "read:a.md", "meta:a.md", "list", "listMtime", "close"]);
        assert!(vault.subscribe(Arc::new(NoListener)).is_none(), "the inner backend has no change feed");
    }
}
