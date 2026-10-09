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
