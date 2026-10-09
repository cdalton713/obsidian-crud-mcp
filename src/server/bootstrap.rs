//! Bring the persisted search index up to date with the vault at startup.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use futures::StreamExt;
use tracing::{debug, info};

use crate::search::{IndexState, SearchIndex};
use crate::vault::{VaultBackend, VaultError};

/// Notes read at once; reads come from local disk, so this mainly hides syscall latency.
const READ_CONCURRENCY: usize = 16;

/// Reconcile the index with the vault, save it, then mark it ready. Notes
/// whose mtime matches the persisted index keep their metadata; the rest are re-read.
pub async fn sync_search_index(vault: Arc<dyn VaultBackend>, index: Arc<SearchIndex>) -> Result<(), VaultError> {
    let start = Instant::now();
    rescan_vault(vault.as_ref(), &index, start).await?;
    index.save_to_disk().await;
    index.set_state(IndexState::Ready);
    Ok(())
}

async fn rescan_vault(vault: &dyn VaultBackend, index: &SearchIndex, start: Instant) -> Result<(), VaultError> {
    let notes = vault.list_notes_with_mtime(None).await?;
    debug!("Vault has {} notes", notes.len());

    // Listing fails when the vault cannot be read, so an empty result really
    // is an empty vault and stale entries can be pruned.
    let vault_paths: HashSet<&str> = notes.iter().map(|n| n.path.as_str()).collect();
    let mut pruned = 0;
    for path in index.list_paths(None) {
        if !vault_paths.contains(path.as_str()) {
            index.remove(&path);
            pruned += 1;
        }
    }
    if pruned > 0 {
        info!("Removed {pruned} deleted notes from the search index.");
    }
    if notes.is_empty() {
        info!("Vault has no notes; search index is empty.");
        return Ok(());
    }

    // Unchanged since the persisted index was written: keep its metadata.
    let stale: Vec<_> = notes
        .iter()
        .filter(|n| !(n.mtime > 0.0 && index.has(&n.path) && index.get_mtime(&n.path) == n.mtime))
        .collect();
    info!("Building search index ({} notes, {} to read)...", notes.len(), stale.len());
    let done = AtomicUsize::new(0);
    let total = stale.len();
    futures::stream::iter(&stale)
        .for_each_concurrent(READ_CONCURRENCY, |note| {
            let done = &done;
            async move {
                // Index empty notes too; read_note returns None only if the note is gone.
                if let Ok(Some(content)) = vault.read_note(&note.path).await {
                    index.update(&note.path, &content, Some(note.mtime));
                }
                let finished = done.fetch_add(1, Ordering::Relaxed) + 1;
                if total > 100 && finished % 500 == 0 {
                    info!("  indexed {finished}/{total}...");
                }
            }
        })
        .await;
    info!(
        "Search index built: {} notes in {:.1}s ({} unchanged).",
        index.size(),
        start.elapsed().as_secs_f64(),
        notes.len() - total
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use async_trait::async_trait;
    use parking_lot::Mutex;

    use super::*;
    use crate::vault::{NoteInfo, NoteListing};

    /// A vault of in-memory notes that records reads and how many overlap.
    #[derive(Default)]
    struct FakeVault {
        notes: BTreeMap<String, (String, f64)>,
        reads: Mutex<Vec<String>>,
        read_delay: Option<Duration>,
        in_flight: AtomicUsize,
        peak: AtomicUsize,
        unlistable: bool,
    }

    impl FakeVault {
        fn with(notes: &[(&str, &str, f64)]) -> Self {
            let notes = notes.iter().map(|&(p, c, m)| (p.to_owned(), (c.to_owned(), m))).collect();
            Self { notes, ..Self::default() }
        }

        fn reads(&self) -> Vec<String> {
            let mut reads = self.reads.lock().clone();
            reads.sort();
            reads
        }
    }

    #[async_trait]
    impl VaultBackend for FakeVault {
        async fn init(&self) -> Result<(), VaultError> {
            Ok(())
        }
        async fn close(&self) {}
        async fn read_note(&self, path: &str) -> Result<Option<String>, VaultError> {
            self.reads.lock().push(path.to_owned());
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            if let Some(delay) = self.read_delay {
                tokio::time::sleep(delay).await;
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(self.notes.get(path).map(|(content, _)| content.clone()))
        }
        async fn write_note(&self, _path: &str, _content: &str) -> Result<bool, VaultError> {
            Ok(false)
        }
        async fn delete_note(&self, _path: &str) -> Result<bool, VaultError> {
            Ok(false)
        }
        async fn move_note(&self, _from: &str, _to: &str) -> Result<bool, VaultError> {
            Ok(false)
        }
        async fn get_metadata(&self, _path: &str) -> Result<Option<NoteInfo>, VaultError> {
            Ok(None)
        }
        async fn list_notes_with_mtime(&self, _folder: Option<&str>) -> Result<Vec<NoteListing>, VaultError> {
            if self.unlistable {
                let source = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
                return Err(VaultError::List { folder: None, source });
            }
            Ok(self.notes.iter().map(|(path, (_, mtime))| NoteListing { path: path.clone(), mtime: *mtime }).collect())
        }
    }

    #[tokio::test]
    async fn skips_unchanged_notes_and_prunes_deleted_ones() {
        let index = Arc::new(SearchIndex::in_memory());
        index.update("same.md", "links to [[kept]]", Some(10.0));
        index.update("changed.md", "old", Some(10.0));
        index.update("deleted.md", "gone", Some(10.0));
        let vault = Arc::new(FakeVault::with(&[
            ("same.md", "links to [[kept]]", 10.0),
            ("changed.md", "#fresh", 20.0),
            ("new.md", "new", 30.0),
        ]));

        sync_search_index(vault.clone(), index.clone()).await.unwrap();

        assert_eq!(vault.reads(), ["changed.md", "new.md"]);
        assert_eq!(index.list_paths(None), ["changed.md", "new.md", "same.md"]);
        assert_eq!(index.get_tags("changed.md"), ["fresh"]);
        assert_eq!(index.get_mtime("changed.md"), 20.0);
        assert_eq!(index.get_backlinks("kept"), ["same.md"]);
        assert_eq!(index.state(), IndexState::Ready);
    }

    #[tokio::test]
    async fn rereads_notes_without_a_known_mtime() {
        let index = Arc::new(SearchIndex::in_memory());
        index.update("zero.md", "old", Some(0.0));
        let vault = Arc::new(FakeVault::with(&[("zero.md", "#new", 0.0), ("empty.md", "", 5.0)]));

        sync_search_index(vault.clone(), index.clone()).await.unwrap();

        assert_eq!(vault.reads(), ["empty.md", "zero.md"], "an mtime of 0 can't prove a note unchanged");
        assert_eq!(index.get_tags("zero.md"), ["new"]);
        assert!(index.has("empty.md"), "empty notes are indexed too");
    }

    #[tokio::test]
    async fn reads_changed_notes_concurrently() {
        let index = Arc::new(SearchIndex::in_memory());
        let contents: Vec<_> = (0..6).map(|i| (format!("n{i}.md"), format!("#t{i}"), f64::from(i + 1))).collect();
        let notes: Vec<_> = contents.iter().map(|(p, c, m)| (p.as_str(), c.as_str(), *m)).collect();
        let vault = Arc::new(FakeVault { read_delay: Some(Duration::from_millis(5)), ..FakeVault::with(&notes) });

        sync_search_index(vault.clone(), index.clone()).await.unwrap();

        let peak = vault.peak.load(Ordering::SeqCst);
        assert!(peak > 1, "reads should overlap (peak {peak})");
        assert_eq!(index.size(), 6);
        assert_eq!(index.get_tags("n3.md"), ["t3"]);
        assert_eq!(index.get_mtime("n3.md"), 4.0);
    }

    #[tokio::test]
    async fn prunes_every_stale_entry_when_the_vault_is_empty() {
        let index = Arc::new(SearchIndex::in_memory());
        index.update("a.md", "a", Some(1.0));
        index.update("b.md", "b", Some(2.0));

        sync_search_index(Arc::new(FakeVault::default()), index.clone()).await.unwrap();

        assert_eq!(index.size(), 0);
        assert_eq!(index.state(), IndexState::Ready);
    }

    #[tokio::test]
    async fn leaves_the_index_intact_when_listing_the_vault_fails() {
        let index = Arc::new(SearchIndex::in_memory());
        index.update("a.md", "a", Some(1.0));
        let vault = Arc::new(FakeVault { unlistable: true, ..FakeVault::default() });

        let result = sync_search_index(vault, index.clone()).await;

        assert!(matches!(result, Err(VaultError::List { .. })), "got {result:?}");
        assert_eq!(index.list_paths(None), ["a.md"]);
        assert_eq!(index.state(), IndexState::Building, "a failed rebuild is not marked ready");
    }

    #[tokio::test]
    async fn saves_the_reconciled_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let index = Arc::new(SearchIndex::new(Some(path.clone()), None, 100));
        let vault = Arc::new(FakeVault::with(&[("a.md", "#saved", 1.0)]));

        sync_search_index(vault, index).await.unwrap();

        let reloaded = SearchIndex::new(Some(path), None, 100);
        assert!(reloaded.load_from_disk().await);
        assert_eq!(reloaded.get_tags("a.md"), ["saved"]);
    }
}
