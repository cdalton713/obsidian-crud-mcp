//! Keep the search index in step with edits made outside this server
//! (Obsidian, in filesystem mode).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info};

use crate::search::SearchIndex;
use crate::util::system_time_ms;
use crate::vault::VaultBackend;

/// Obsidian fires two or three filesystem events per save; coalesce them per file.
const DEBOUNCE: Duration = Duration::from_millis(100);

/// A running watcher; dropping it (or calling [`VaultWatcher::stop`]) stops watching.
pub struct VaultWatcher {
    _watcher: Option<RecommendedWatcher>,
    task: JoinHandle<()>,
}

impl VaultWatcher {
    pub fn stop(self) {}
}

impl Drop for VaultWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The vault path for a changed file, or `None` for files that are not notes.
fn note_path(root: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(root).ok()?;
    let path = relative.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
    if !path.ends_with(".md") || path.starts_with(".obsidian/") || path.contains("/.obsidian/") {
        return None;
    }
    Some(path)
}

async fn refresh(vault: &dyn VaultBackend, index: &SearchIndex, root: &Path, path: &str) {
    match vault.read_note(path).await {
        Ok(Some(content)) => {
            let mtime = tokio::fs::metadata(root.join(path)).await.and_then(|m| m.modified());
            match mtime {
                Ok(mtime) => index.update(path, &content, Some(system_time_ms(mtime))),
                Err(_) => index.remove(path),
            }
        }
        // Deleted, or blocked by the vault's path checks.
        Ok(None) | Err(_) => index.remove(path),
    }
}

/// Watch `vault_path` recursively and refresh changed notes in the index.
/// A watcher that cannot start (or fails later, e.g. past the inotify watch
/// limit) logs an error instead of taking the server down.
pub fn watch_vault(vault: Arc<dyn VaultBackend>, index: Arc<SearchIndex>, vault_path: &Path) -> VaultWatcher {
    let root: PathBuf = std::fs::canonicalize(vault_path).unwrap_or_else(|_| vault_path.to_path_buf());
    let (events, mut changes) = mpsc::unbounded_channel::<notify::Result<Event>>();
    let watcher = notify::recommended_watcher(move |event| {
        let _ = events.send(event);
    })
    .and_then(|mut watcher| watcher.watch(&root, RecursiveMode::Recursive).map(|()| watcher));
    let watcher = match watcher {
        Ok(watcher) => {
            info!("Watching vault for external changes.");
            Some(watcher)
        }
        Err(e) => {
            error!(
                "Vault watcher failed: {e}. External edits are not tracked until restart; on Linux, raise fs.inotify.max_user_watches."
            );
            None
        }
    };

    let task = tokio::spawn(async move {
        let mut pending: HashMap<String, JoinHandle<()>> = HashMap::new();
        while let Some(event) = changes.recv().await {
            let event = match event {
                Ok(event) => event,
                Err(e) => {
                    error!(
                        "Vault watcher failed: {e}. External edits are not tracked until restart; on Linux, raise fs.inotify.max_user_watches."
                    );
                    break;
                }
            };
            pending.retain(|_, task| !task.is_finished());
            for path in event.paths.iter().filter_map(|file| note_path(&root, file)) {
                if let Some(previous) = pending.remove(&path) {
                    previous.abort();
                }
                let (vault, index, root, key) = (vault.clone(), index.clone(), root.clone(), path.clone());
                pending.insert(
                    path,
                    tokio::spawn(async move {
                        tokio::time::sleep(DEBOUNCE).await;
                        refresh(vault.as_ref(), &index, &root, &key).await;
                    }),
                );
            }
        }
        pending.values().for_each(JoinHandle::abort);
    });
    VaultWatcher { _watcher: watcher, task }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_files_to_note_paths() {
        let root = Path::new("/vault");
        assert_eq!(note_path(root, Path::new("/vault/a/b.md")).as_deref(), Some("a/b.md"));
        assert_eq!(note_path(root, Path::new("/vault/a/b.txt")), None);
        assert_eq!(note_path(root, Path::new("/vault/.obsidian/x.md")), None);
        assert_eq!(note_path(root, Path::new("/vault/a/.obsidian/x.md")), None);
        assert_eq!(note_path(root, Path::new("/elsewhere/x.md")), None);
    }
}
