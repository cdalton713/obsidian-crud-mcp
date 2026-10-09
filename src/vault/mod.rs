//! Where notes live: a local folder ([`LocalVault`]) or a local mirror of a
//! Remotely Save S3 bucket ([`S3Vault`]), behind one [`VaultBackend`] trait.

mod local;
mod mirror;
mod read_only;
mod s3;
mod write_scope;

use std::io;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use parking_lot::Mutex;
use thiserror::Error;

use crate::notes::{InvalidNotePath, NoteMetadata};

pub use local::LocalVault;
pub use mirror::{
    AwsStore, Manifest, ManifestEntry, MirrorPlan, ObjectData, ObjectStore, ObjectSummary, Precondition, RemoteNote,
    S3Mirror, S3Options, StoreError, is_mirrored_path, normalize_prefix, parse_remote_mtime, plan_mirror,
    remote_metadata,
};
pub use read_only::{READ_ONLY_MESSAGE, ReadOnlyVault};
pub use s3::{S3Vault, S3VaultOptions};
pub use write_scope::{is_path_writable, parse_write_folders};

/// Why a vault operation failed.
#[derive(Debug, Error)]
pub enum VaultError {
    #[error(transparent)]
    InvalidPath(#[from] InvalidNotePath),
    #[error("Path traversal blocked")]
    Traversal,
    #[error("Path traversal blocked: dangling symlink")]
    DanglingSymlink,
    #[error("Write access denied: '{0}' is outside the writable folders.")]
    WriteDenied(String),
    #[error("{READ_ONLY_MESSAGE}")]
    ReadOnly,
    #[error("Destination already exists: {0}")]
    DestinationExists(String),
    #[error("'{0}' changed in the bucket since it was last synced. Read it again and retry.")]
    RemoteChanged(String),
    #[error("Failed to list notes{}", folder.as_ref().map(|f| format!(" in '{f}'")).unwrap_or_default())]
    List {
        folder: Option<String>,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// A note's file facts plus what its content says about it.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteInfo {
    pub path: String,
    pub size: u64,
    /// Creation time in ms since the epoch (modification time where unsupported).
    pub ctime: f64,
    /// Modification time in ms since the epoch.
    pub mtime: f64,
    pub metadata: NoteMetadata,
}

/// A note path with its modification time in ms since the epoch (0 if unknown).
#[derive(Debug, Clone, PartialEq)]
pub struct NoteListing {
    pub path: String,
    pub mtime: f64,
}

/// Receives notes that changed outside this server (a device edit that the S3
/// mirror downloaded, or a note removed from the bucket). `mtime` is the local
/// file's modification time in ms, so an index fed from here agrees with a
/// later mtime-based rescan.
pub trait VaultChangeListener: Send + Sync {
    fn updated(&self, path: &str, content: &str, mtime: f64);
    fn removed(&self, path: &str);
}

#[async_trait]
pub trait VaultBackend: Send + Sync {
    async fn init(&self) -> Result<(), VaultError>;
    async fn close(&self);
    /// The note's content, or `None` when it does not exist or cannot be read.
    async fn read_note(&self, path: &str) -> Result<Option<String>, VaultError>;
    async fn write_note(&self, path: &str, content: &str) -> Result<bool, VaultError>;
    async fn delete_note(&self, path: &str) -> Result<bool, VaultError>;
    async fn move_note(&self, from: &str, to: &str) -> Result<bool, VaultError>;
    async fn get_metadata(&self, path: &str) -> Result<Option<NoteInfo>, VaultError>;
    /// Note paths sorted by name, optionally inside `folder`. Fails when the vault cannot be listed.
    async fn list_notes_with_mtime(&self, folder: Option<&str>) -> Result<Vec<NoteListing>, VaultError>;

    async fn list_notes(&self, folder: Option<&str>) -> Result<Vec<String>, VaultError> {
        let notes = self.list_notes_with_mtime(folder).await?;
        Ok(notes.into_iter().map(|n| n.path).collect())
    }

    /// Report changes that arrive from outside this server, with their content.
    /// A backend that knows what changed (the S3 mirror) implements this so the
    /// search index needs no filesystem watcher; others return `None`.
    fn subscribe(&self, _listener: Arc<dyn VaultChangeListener>) -> Option<Subscription> {
        None
    }
}

type ListenerList = Mutex<Vec<(u64, Arc<dyn VaultChangeListener>)>>;

/// A set of change listeners that a backend notifies.
#[derive(Default)]
pub struct Listeners {
    inner: Arc<ListenerList>,
    next_id: Mutex<u64>,
}

impl Listeners {
    pub fn subscribe(&self, listener: Arc<dyn VaultChangeListener>) -> Subscription {
        let id = {
            let mut next = self.next_id.lock();
            *next += 1;
            *next
        };
        self.inner.lock().push((id, listener));
        Subscription { id, listeners: Arc::downgrade(&self.inner) }
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    /// Call `f` for every listener.
    pub fn notify(&self, f: impl Fn(&dyn VaultChangeListener)) {
        let listeners: Vec<_> = self.inner.lock().iter().map(|(_, l)| l.clone()).collect();
        listeners.iter().for_each(|l| f(l.as_ref()));
    }
}

/// Keeps a listener registered; dropping it unsubscribes.
#[must_use = "dropping a Subscription unsubscribes the listener"]
pub struct Subscription {
    id: u64,
    listeners: Weak<ListenerList>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(listeners) = self.listeners.upgrade() {
            listeners.lock().retain(|(id, _)| *id != self.id);
        }
    }
}
