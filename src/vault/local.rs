//! Filesystem mode: notes are files in a folder on local disk.

use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use tokio::fs;
use walkdir::WalkDir;

use super::{NoteInfo, NoteListing, VaultBackend, VaultError, is_path_writable};
use crate::notes::{is_valid_note_path, parse_frontmatter_and_links, validate_note_path};
use crate::util::{locale_cmp, system_time_ms};

/// A vault folder on local disk. Every path is checked to stay inside it,
/// symlinks included.
#[derive(Debug, Clone)]
pub struct LocalVault {
    root: PathBuf,
    write_folders: Option<Vec<String>>,
}

/// Resolve `path` against `base` without touching the disk.
pub(crate) fn lexical_resolve(base: &Path, path: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for component in Path::new(path).components() {
        match component {
            Component::Prefix(prefix) => out = PathBuf::from(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Strictly inside `root` (not `root` itself).
fn is_inside(path: &Path, root: &Path) -> bool {
    path != root && path.starts_with(root)
}

fn read_text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

impl LocalVault {
    /// Open the vault folder. Fails when the folder does not exist.
    pub fn new(vault_path: impl AsRef<Path>, write_folders: Option<Vec<String>>) -> io::Result<Self> {
        let root = std::fs::canonicalize(vault_path)?;
        Ok(Self { root, write_folders })
    }

    /// The vault folder with symlinks resolved.
    pub fn root_path(&self) -> &Path {
        &self.root
    }

    /// Absolute path for a vault path, refusing anything that resolves outside
    /// the vault, before or after following symlinks. Note operations must
    /// target a real note (`is_note`); folder listing passes a directory.
    async fn safe_path(&self, path: &str, is_note: bool) -> Result<PathBuf, VaultError> {
        if is_note {
            validate_note_path(path)?;
        }
        let full = lexical_resolve(&self.root, path);
        // Lexical check first (catches ../ without hitting disk).
        if !is_inside(&full, &self.root) {
            return Err(VaultError::Traversal);
        }
        // Resolve symlinks and re-check (catches symlink escapes).
        let real = match fs::canonicalize(&full).await {
            Ok(real) => real,
            Err(e) if e.kind() == ErrorKind::NotFound => self.resolve_missing_path(&full).await?,
            Err(e) => return Err(e.into()),
        };
        if !is_inside(&real, &self.root) {
            return Err(VaultError::Traversal);
        }
        Ok(real)
    }

    /// The file (or a parent) doesn't exist yet. Resolve the nearest existing
    /// ancestor, so a symlinked parent directory can't redirect a write outside
    /// the root, and refuse dangling symlinks on the way.
    async fn resolve_missing_path(&self, target: &Path) -> Result<PathBuf, VaultError> {
        let mut missing: Vec<std::ffi::OsString> = Vec::new();
        let mut current = target.to_path_buf();
        loop {
            match fs::canonicalize(&current).await {
                Ok(real) => {
                    return Ok(missing.iter().rev().fold(real, |path, part| path.join(part)));
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    match fs::symlink_metadata(&current).await {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            return Err(VaultError::DanglingSymlink);
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                    let (Some(parent), Some(name)) = (current.parent(), current.file_name()) else {
                        return Err(e.into());
                    };
                    missing.push(name.to_owned());
                    current = parent.to_path_buf();
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn to_vault_path(&self, full: &Path) -> String {
        let relative = full.strip_prefix(&self.root).unwrap_or(full);
        relative.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
    }

    /// Write folders as they are spelled on disk, so they compare against
    /// resolved note paths on case-insensitive filesystems. A folder that
    /// resolves elsewhere (it is itself a symlink) keeps its configured name:
    /// following it would widen the scope to the link target.
    async fn canonical_write_folders(&self) -> Option<Vec<String>> {
        let folders = self.write_folders.as_ref()?;
        let mut canonical = Vec::with_capacity(folders.len());
        for folder in folders {
            let real = fs::canonicalize(lexical_resolve(&self.root, folder)).await.ok();
            let spelled = real
                .filter(|real| real.starts_with(&self.root))
                .map(|real| self.to_vault_path(&real))
                .filter(|on_disk| on_disk.to_lowercase() == folder.to_lowercase());
            canonical.push(spelled.unwrap_or_else(|| folder.clone()));
        }
        Some(canonical)
    }

    async fn writable_path(&self, path: &str) -> Result<PathBuf, VaultError> {
        let full = self.safe_path(path, true).await?;
        let resolved = self.to_vault_path(&full);
        let canonical = self.canonical_write_folders().await;
        if !is_path_writable(path, self.write_folders.as_deref()) || !is_path_writable(&resolved, canonical.as_deref())
        {
            return Err(VaultError::WriteDenied(path.to_owned()));
        }
        Ok(full)
    }

    async fn exists(full: &Path) -> Result<bool, VaultError> {
        match fs::symlink_metadata(full).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

#[async_trait]
impl VaultBackend for LocalVault {
    async fn init(&self) -> Result<(), VaultError> {
        Ok(())
    }

    async fn close(&self) {}

    async fn read_note(&self, path: &str) -> Result<Option<String>, VaultError> {
        let full = self.safe_path(path, true).await?;
        Ok(fs::read(&full).await.ok().map(read_text))
    }

    async fn write_note(&self, path: &str, content: &str) -> Result<bool, VaultError> {
        let full = self.writable_path(path).await?;
        if let Some(parent) = full.parent()
            && fs::create_dir_all(parent).await.is_err()
        {
            return Ok(false);
        }
        Ok(fs::write(&full, content).await.is_ok())
    }

    async fn delete_note(&self, path: &str) -> Result<bool, VaultError> {
        let full = self.writable_path(path).await?;
        Ok(fs::remove_file(&full).await.is_ok())
    }

    async fn move_note(&self, from: &str, to: &str) -> Result<bool, VaultError> {
        let from_path = self.writable_path(from).await?;
        let to_path = self.writable_path(to).await?;
        if !Self::exists(&from_path).await? {
            return Ok(false);
        }
        let mut target = to_path.clone();
        if from_path == to_path {
            // Both names resolve to the same file: an identical path or a
            // case-only rename on a case-insensitive filesystem. Rename to the
            // requested spelling of the file name; canonicalize reports the old one.
            let requested = lexical_resolve(&self.root, to);
            if let (Some(parent), Some(name)) = (from_path.parent(), requested.file_name()) {
                target = parent.join(name);
            }
            if target == from_path {
                return Ok(true);
            }
        } else if Self::exists(&to_path).await? {
            return Err(VaultError::DestinationExists(to.to_owned()));
        }
        if let Some(parent) = target.parent()
            && fs::create_dir_all(parent).await.is_err()
        {
            return Ok(false);
        }
        match fs::rename(&from_path, &target).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::CrossesDevices => {
                // Cross-device: fall back to copy and delete.
                let Some(content) = self.read_note(from).await? else {
                    return Ok(false);
                };
                if !self.write_note(to, &content).await? {
                    return Ok(false);
                }
                self.delete_note(from).await
            }
            Err(_) => Ok(false),
        }
    }

    async fn get_metadata(&self, path: &str) -> Result<Option<NoteInfo>, VaultError> {
        let full = self.safe_path(path, true).await?;
        let (Ok(bytes), Ok(meta)) = (fs::read(&full).await, fs::metadata(&full).await) else {
            return Ok(None);
        };
        let content = read_text(bytes);
        let mtime = meta.modified().map(system_time_ms).unwrap_or_default();
        Ok(Some(NoteInfo {
            path: path.to_owned(),
            size: meta.len(),
            ctime: meta.created().map(system_time_ms).unwrap_or(mtime),
            mtime,
            metadata: parse_frontmatter_and_links(&content),
        }))
    }

    async fn list_notes_with_mtime(&self, folder: Option<&str>) -> Result<Vec<NoteListing>, VaultError> {
        let folder = folder
            .filter(|f| !f.is_empty())
            .map(|f| if f.ends_with('/') || f.ends_with('\\') { f.to_owned() } else { format!("{f}/") });
        let search_dir = match &folder {
            Some(folder) => self.safe_path(folder, false).await?,
            None => self.root.clone(),
        };
        // A walk swallows directory errors and yields nothing, which would look
        // like an empty vault; probe the directory so failures surface. A folder
        // that doesn't exist (or isn't a folder) just has no notes.
        if let Err(e) = fs::read_dir(&search_dir).await {
            if folder.is_some() && matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) {
                return Ok(Vec::new());
            }
            return Err(VaultError::List { folder, source: e });
        }
        let root = self.root.clone();
        let prefix = folder.unwrap_or_default();
        let listing = tokio::task::spawn_blocking(move || {
            let mut notes: Vec<NoteListing> = WalkDir::new(&search_dir)
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.'))
                .filter_map(Result::ok)
                .filter(|entry| !entry.file_type().is_dir())
                .filter_map(|entry| {
                    let relative = entry.path().strip_prefix(&search_dir).ok()?;
                    let relative: Vec<_> = relative.components().map(|c| c.as_os_str().to_string_lossy()).collect();
                    let path = format!("{prefix}{}", relative.join("/"));
                    if !is_valid_note_path(&path) {
                        return None;
                    }
                    let mtime = std::fs::metadata(lexical_resolve(&root, &path))
                        .and_then(|m| m.modified())
                        .map(system_time_ms)
                        .unwrap_or(0.0);
                    Some(NoteListing { path, mtime })
                })
                .collect();
            notes.sort_by(|a, b| locale_cmp(&a.path, &b.path));
            notes
        })
        .await
        .map_err(io::Error::other)?;
        Ok(listing)
    }
}

#[cfg(test)]
mod tests;
