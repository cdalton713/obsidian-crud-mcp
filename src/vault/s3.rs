//! S3 mode: a [`LocalVault`] over a folder that [`S3Mirror`] keeps in step
//! with a bucket (Remotely Save's S3 backend). Reads, listing and search all
//! run against the local copy; writes go to the bucket first and land locally
//! only after the upload succeeds.
//!
//! Writes never wait for a poll: the mirror runs its network calls unlocked
//! and refuses to let a listing taken before a write undo that write.
//!
//! Each poll reports what it downloaded or removed to subscribers (the search
//! index), content included, so S3 mode needs no filesystem watcher and never
//! reads a downloaded note a second time.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::{
    LocalVault, NoteInfo, NoteListing, ObjectStore, Precondition, S3Mirror, Subscription,
    VaultBackend, VaultChangeListener, VaultError, is_path_writable,
};
use crate::notes::validate_note_path;

/// Where the mirror lives and how often it polls.
#[derive(Debug, Clone)]
pub struct S3VaultOptions {
    pub vault_path: PathBuf,
    pub manifest_path: PathBuf,
    pub poll_seconds: u64,
    pub prefix: String,
    pub write_folders: Option<Vec<String>>,
}

pub struct S3Vault {
    local: LocalVault,
    mirror: Arc<S3Mirror>,
    poll_interval: Duration,
    write_folders: Option<Vec<String>>,
    poller: Mutex<Option<JoinHandle<()>>>,
}

impl S3Vault {
    pub fn new(options: S3VaultOptions, store: Arc<dyn ObjectStore>) -> std::io::Result<Self> {
        let local = LocalVault::new(&options.vault_path, options.write_folders.clone())?;
        let mirror = Arc::new(S3Mirror::new(local.root_path(), options.manifest_path, &options.prefix, store));
        Ok(Self {
            local,
            mirror,
            poll_interval: Duration::from_secs(options.poll_seconds),
            write_folders: options.write_folders,
            poller: Mutex::new(None),
        })
    }

    pub fn mirror(&self) -> &S3Mirror {
        &self.mirror
    }

    /// One poll; exposed for tests and for callers that want a refresh now.
    pub async fn poll(&self) -> Result<usize, VaultError> {
        self.mirror.sync().await
    }

    fn assert_writable(&self, path: &str) -> Result<(), VaultError> {
        validate_note_path(path)?;
        if !is_path_writable(path, self.write_folders.as_deref()) {
            return Err(VaultError::WriteDenied(path.to_owned()));
        }
        Ok(())
    }
}

#[async_trait]
impl VaultBackend for S3Vault {
    async fn init(&self) -> Result<(), VaultError> {
        self.mirror.load_manifest().await;
        let start = Instant::now();
        let changed = self.poll().await?;
        info!("S3 mirror ready: {changed} notes updated in {:.1}s.", start.elapsed().as_secs_f64());
        let mirror = self.mirror.clone();
        let period = self.poll_interval;
        let poller = tokio::spawn(async move {
            let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                if let Err(error) = mirror.sync().await {
                    warn!("S3 mirror poll failed: {error}");
                }
            }
        });
        if let Some(previous) = self.poller.lock().replace(poller) {
            previous.abort();
        }
        Ok(())
    }

    async fn close(&self) {
        if let Some(poller) = self.poller.lock().take() {
            poller.abort();
        }
        self.mirror.idle().await;
    }

    async fn read_note(&self, path: &str) -> Result<Option<String>, VaultError> {
        self.local.read_note(path).await
    }

    async fn write_note(&self, path: &str, content: &str) -> Result<bool, VaultError> {
        self.assert_writable(path)?;
        // Overwrite only the version this server last saw; create only if absent.
        // A version that lands in between fails the condition (412) instead of being lost.
        let precondition = if self.mirror.local_exists(path).await {
            self.mirror.etag_of(path).await.map_or(Precondition::None, Precondition::IfMatch)
        } else {
            Precondition::IfNoneMatch
        };
        self.mirror.put(path, content, precondition).await?;
        Ok(true)
    }

    async fn delete_note(&self, path: &str) -> Result<bool, VaultError> {
        self.assert_writable(path)?;
        if !self.mirror.local_exists(path).await {
            return Ok(false);
        }
        self.mirror.remove(path).await?;
        self.local.delete_note(path).await
    }

    async fn move_note(&self, from: &str, to: &str) -> Result<bool, VaultError> {
        self.assert_writable(from)?;
        self.assert_writable(to)?;
        if !self.mirror.local_exists(from).await {
            return Ok(false);
        }
        if from == to {
            return Ok(true);
        }
        // A case-only rename finds its own source on a case-insensitive disk.
        let case_only = from.to_lowercase() == to.to_lowercase();
        if !case_only && self.mirror.local_exists(to).await {
            return Err(VaultError::DestinationExists(to.to_owned()));
        }
        self.mirror.copy(from, to).await?;
        self.mirror.remove(from).await?;
        self.local.move_note(from, to).await
    }

    async fn get_metadata(&self, path: &str) -> Result<Option<NoteInfo>, VaultError> {
        self.local.get_metadata(path).await
    }

    async fn list_notes_with_mtime(&self, folder: Option<&str>) -> Result<Vec<NoteListing>, VaultError> {
        self.local.list_notes_with_mtime(folder).await
    }

    fn subscribe(&self, listener: Arc<dyn VaultChangeListener>) -> Option<Subscription> {
        Some(self.mirror.subscribe(listener))
    }
}

impl Drop for S3Vault {
    fn drop(&mut self) {
        if let Some(poller) = self.poller.lock().take() {
            poller.abort();
        }
    }
}
