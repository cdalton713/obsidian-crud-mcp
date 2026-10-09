//! Metadata index for vault notes.
//!
//! Tracks paths, mtimes, tags, links, and backlinks, and keeps note content in
//! memory (up to a cap) so content scans need no disk reads. Persists metadata
//! to disk (encrypted with AES-256-GCM when `INDEX_PASSPHRASE` is set); content
//! is not persisted and refills from the vault after a restart. There is no
//! full-text index: scans run over the cached content.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use parking_lot::RwLock;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::logging::describe_error;
use crate::notes::{is_valid_note_path, parse_frontmatter_and_links};
use crate::util::locale_cmp;
use crate::vault::{NoteListing, VaultChangeListener};

/// Bump when parsed metadata changes meaning; older persisted indexes are discarded and rebuilt.
const INDEX_SCHEMA_VERSION: u32 = 4;

/// Lifecycle of the in-memory index: `Building` from construction until the
/// startup rebuild finishes, `Ready` afterwards, `Failed` if the rebuild failed.
/// Read by `list_notes` so a client can tell a partial index from a complete one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IndexState {
    Building = 0,
    Ready = 1,
    Failed = 2,
}

/// A tag and how many notes carry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagCount {
    pub tag: String,
    pub count: usize,
}

/// On-disk shape of the persisted metadata (after decryption).
#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedIndex {
    version: u32,
    #[serde(default)]
    mtimes: HashMap<String, f64>,
    #[serde(default)]
    tags: HashMap<String, Vec<String>>,
    #[serde(default)]
    links: HashMap<String, Vec<String>>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Note content by path, bounded by `max_content_chars`; see `cache_content`.
    contents: HashMap<String, Arc<str>>,
    content_chars: usize,
    content_cap_warned: bool,
    mtimes: HashMap<String, f64>,
    tags: HashMap<String, Vec<String>>,
    links: HashMap<String, Vec<String>>,
    /// Lowercased link target to the notes linking to it.
    backlinks: HashMap<String, HashSet<String>>,
    known_paths: HashSet<String>,
}

impl Inner {
    fn cache_content(&mut self, path: &str, content: Arc<str>, max: usize) {
        self.drop_content(path);
        let length = content.chars().count();
        if self.content_chars + length > max {
            if !self.content_cap_warned {
                self.content_cap_warned = true;
                warn!(
                    "Search content cache is full ({max} chars); notes beyond it are scanned from disk. Raise SEARCH_CONTENT_CACHE_MB to cache the whole vault."
                );
            }
            return;
        }
        self.contents.insert(path.to_owned(), content);
        self.content_chars += length;
    }

    fn drop_content(&mut self, path: &str) {
        if let Some(previous) = self.contents.remove(path) {
            self.content_chars -= previous.chars().count();
        }
    }

    fn add_backlinks(&mut self, path: &str, targets: &[String]) {
        for target in targets {
            self.backlinks.entry(target.to_lowercase()).or_default().insert(path.to_owned());
        }
    }

    /// Remove all backlink entries where `path` is the source.
    fn clear_backlinks(&mut self, path: &str) {
        let Some(old) = self.links.remove(path) else { return };
        for target in old {
            let key = target.to_lowercase();
            if let Some(sources) = self.backlinks.get_mut(&key) {
                sources.remove(path);
                if sources.is_empty() {
                    self.backlinks.remove(&key);
                }
            }
        }
    }
}

pub struct SearchIndex {
    inner: RwLock<Inner>,
    state: AtomicU8,
    persist_path: Option<PathBuf>,
    passphrase: Option<String>,
    max_content_chars: usize,
    /// Serializes saves; a save requested during another waits, then writes the latest state.
    saving: tokio::sync::Mutex<()>,
}

impl SearchIndex {
    pub const DEFAULT_MAX_CONTENT_CHARS: usize = 32 * 1024 * 1024;

    pub fn new(persist_path: Option<PathBuf>, passphrase: Option<String>, max_content_chars: usize) -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
            state: AtomicU8::new(IndexState::Building as u8),
            persist_path,
            passphrase,
            max_content_chars,
            saving: tokio::sync::Mutex::new(()),
        }
    }

    /// An index that is never persisted, with the default content cap.
    pub fn in_memory() -> Self {
        Self::new(None, None, Self::DEFAULT_MAX_CONTENT_CHARS)
    }

    pub fn state(&self) -> IndexState {
        match self.state.load(Ordering::Acquire) {
            0 => IndexState::Building,
            1 => IndexState::Ready,
            _ => IndexState::Failed,
        }
    }

    pub fn set_state(&self, state: IndexState) {
        self.state.store(state as u8, Ordering::Release);
    }

    /// Number of indexed notes.
    pub fn size(&self) -> usize {
        self.inner.read().known_paths.len()
    }

    /// Cached content for a path, if the cache holds it.
    pub fn get_content(&self, path: &str) -> Option<Arc<str>> {
        self.inner.read().contents.get(path).cloned()
    }

    /// Characters of note content held in memory.
    pub fn content_size(&self) -> usize {
        self.inner.read().content_chars
    }

    /// Keep `content` in memory for scans. Beyond the cap the note is left out
    /// (and any older copy dropped), so scans read it from disk instead; memory
    /// stays bounded while the vault stays searchable.
    pub fn cache_content(&self, path: &str, content: Arc<str>) {
        self.inner.write().cache_content(path, content, self.max_content_chars);
    }

    /// Load metadata from disk. Returns whether any notes were loaded.
    pub async fn load_from_disk(&self) -> bool {
        let Some(path) = &self.persist_path else { return false };
        let Ok(raw) = tokio::fs::read_to_string(path).await else { return false };
        let json = match &self.passphrase {
            Some(passphrase) => match decrypt(&raw, passphrase) {
                Some(json) => json,
                None => return false,
            },
            None => raw,
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) else { return false };
        if value.get("version").and_then(|v| v.as_u64()) != Some(u64::from(INDEX_SCHEMA_VERSION)) {
            info!("Persisted search metadata uses an older format; rebuilding.");
            return false;
        }
        let data: PersistedIndex = match serde_json::from_value(value) {
            Ok(data) => data,
            Err(_) => {
                warn!("Persisted search metadata is malformed; rebuilding.");
                return false;
            }
        };
        let mut inner = self.inner.write();
        for (path, mtime) in data.mtimes {
            inner.known_paths.insert(path.clone());
            inner.mtimes.insert(path, mtime);
        }
        inner.tags.extend(data.tags);
        for (path, targets) in data.links {
            inner.add_backlinks(&path, &targets);
            inner.links.insert(path, targets);
        }
        info!("Search metadata loaded from disk ({} notes).", inner.known_paths.len());
        !inner.known_paths.is_empty()
    }

    /// Save metadata to disk, encrypted if a passphrase is set.
    ///
    /// A call made while a save is in flight waits for it and then saves once
    /// more, so the latest state (e.g. the shutdown save) is never dropped.
    pub async fn save_to_disk(&self) {
        let Some(persist_path) = &self.persist_path else { return };
        let _saving = self.saving.lock().await;
        let (data, count) = {
            let inner = self.inner.read();
            let snapshot = PersistedIndex {
                version: INDEX_SCHEMA_VERSION,
                mtimes: inner.mtimes.clone(),
                tags: inner.tags.clone(),
                links: inner.links.clone(),
            };
            (serde_json::to_string(&snapshot).unwrap_or_default(), inner.known_paths.len())
        };
        let data = match &self.passphrase {
            Some(passphrase) => encrypt(&data, passphrase),
            None => data,
        };
        let file_name = persist_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = persist_path.with_file_name(format!(".{file_name}.{}.{}.tmp", std::process::id(), uuid::Uuid::new_v4()));
        match write_private(persist_path, &tmp, data.as_bytes()).await {
            Ok(()) => info!(
                "Search index saved to disk ({count} notes{}).",
                if self.passphrase.is_some() { ", encrypted" } else { "" }
            ),
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                error!("Failed to save search index: {}", describe_error(&e));
            }
        }
    }

    /// Add or update a note in the index.
    pub fn update(&self, path: &str, content: &str, mtime: Option<f64>) {
        let parsed = parse_frontmatter_and_links(content);
        let mut inner = self.inner.write();
        if inner.known_paths.contains(path) {
            inner.clear_backlinks(path);
        }
        inner.known_paths.insert(path.to_owned());
        if let Some(mtime) = mtime {
            inner.mtimes.insert(path.to_owned(), mtime);
        }
        inner.cache_content(path, Arc::from(content), self.max_content_chars);
        if parsed.tags.is_empty() {
            inner.tags.remove(path);
        } else {
            inner.tags.insert(path.to_owned(), parsed.tags);
        }
        if parsed.links.is_empty() {
            inner.links.remove(path);
        } else {
            inner.add_backlinks(path, &parsed.links);
            inner.links.insert(path.to_owned(), parsed.links);
        }
    }

    /// Remove a note from the index.
    pub fn remove(&self, path: &str) {
        let mut inner = self.inner.write();
        inner.drop_content(path);
        if inner.known_paths.remove(path) {
            inner.mtimes.remove(path);
            inner.tags.remove(path);
            inner.clear_backlinks(path);
        }
    }

    /// Indexed paths sorted by name, optionally inside `folder`.
    pub fn list_paths(&self, folder: Option<&str>) -> Vec<String> {
        self.list_with_mtime(folder).into_iter().map(|n| n.path).collect()
    }

    /// Indexed paths with mtimes sorted by name, optionally inside `folder`.
    pub fn list_with_mtime(&self, folder: Option<&str>) -> Vec<NoteListing> {
        let prefix = folder
            .filter(|f| !f.is_empty())
            .map(|f| if f.ends_with('/') { f.to_owned() } else { format!("{f}/") });
        let inner = self.inner.read();
        let mut entries: Vec<NoteListing> = inner
            .known_paths
            .iter()
            .filter(|p| is_valid_note_path(p))
            .filter(|p| prefix.as_ref().is_none_or(|prefix| p.starts_with(prefix.as_str())))
            .map(|p| NoteListing { path: p.clone(), mtime: inner.mtimes.get(p).copied().unwrap_or(0.0) })
            .collect();
        entries.sort_by(|a, b| locale_cmp(&a.path, &b.path));
        entries
    }

    pub fn has(&self, path: &str) -> bool {
        self.inner.read().known_paths.contains(path)
    }

    pub fn get_mtime(&self, path: &str) -> f64 {
        self.inner.read().mtimes.get(path).copied().unwrap_or(0.0)
    }

    pub fn get_tags(&self, path: &str) -> Vec<String> {
        self.inner.read().tags.get(path).cloned().unwrap_or_default()
    }

    pub fn get_links(&self, path: &str) -> Vec<String> {
        self.inner.read().links.get(path).cloned().unwrap_or_default()
    }

    /// Notes that link to `path`, sorted. Case-insensitive; matches links by
    /// full path (with or without `.md`) or by file name.
    pub fn get_backlinks(&self, path: &str) -> Vec<String> {
        let lower = path.to_lowercase();
        let without_md = lower.strip_suffix(".md").unwrap_or(&lower).to_owned();
        let with_md = format!("{without_md}.md");
        let name_only = without_md.rsplit('/').next().unwrap_or(&without_md).to_owned();
        let inner = self.inner.read();
        let mut results: Vec<String> = [with_md, without_md, name_only]
            .iter()
            .filter_map(|target| inner.backlinks.get(target))
            .flatten()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        results.sort();
        results
    }

    /// Every tag in the vault with its note count, most used first.
    pub fn list_all_tags(&self) -> Vec<TagCount> {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        let inner = self.inner.read();
        for tag in inner.tags.values().flatten() {
            *counts.entry(tag).or_default() += 1;
        }
        let mut tags: Vec<TagCount> =
            counts.into_iter().map(|(tag, count)| TagCount { tag: tag.to_owned(), count }).collect();
        tags.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.tag.cmp(&b.tag)));
        tags
    }
}

impl VaultChangeListener for SearchIndex {
    fn updated(&self, path: &str, content: &str, mtime: f64) {
        self.update(path, content, Some(mtime));
    }

    fn removed(&self, path: &str) {
        self.remove(path);
    }
}

/// Write a fresh file only this user can read, then rename it into place.
async fn write_private(path: &std::path::Path, tmp: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(tmp).await?;
    tokio::io::AsyncWriteExt::write_all(&mut file, data).await?;
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    drop(file);
    tokio::fs::rename(tmp, path).await
}

/// Node's `scryptSync(passphrase, salt, 32)` defaults: N = 2^14, r = 8, p = 1.
fn derive_key(passphrase: &str, salt: &[u8]) -> Option<[u8; 32]> {
    let params = scrypt::Params::new(14, 8, 1, 32).ok()?;
    let mut key = [0u8; 32];
    scrypt::scrypt(passphrase.as_bytes(), salt, &params, &mut key).ok()?;
    Some(key)
}

/// `salt:iv:tag:ciphertext`, all hex: AES-256-GCM under a scrypt-derived key.
fn encrypt(text: &str, passphrase: &str) -> String {
    let mut salt = [0u8; 16];
    let mut iv = [0u8; 12];
    rand::rng().fill_bytes(&mut salt);
    rand::rng().fill_bytes(&mut iv);
    let key = derive_key(passphrase, &salt).expect("valid scrypt parameters");
    let cipher = Aes256Gcm::new(&key.into());
    let sealed = cipher
        .encrypt(Nonce::from_slice(&iv), Payload { msg: text.as_bytes(), aad: &[] })
        .expect("AES-GCM encryption cannot fail for in-memory data");
    let (ciphertext, tag) = sealed.split_at(sealed.len() - 16);
    format!("{}:{}:{}:{}", hex::encode(salt), hex::encode(iv), hex::encode(tag), hex::encode(ciphertext))
}

/// The plaintext, or `None` for a wrong passphrase or corrupted data.
fn decrypt(data: &str, passphrase: &str) -> Option<String> {
    let mut parts = data.trim().split(':');
    let mut next = || parts.next().and_then(|p| hex::decode(p).ok());
    let (salt, iv, tag, mut ciphertext) = (next()?, next()?, next()?, next()?);
    if iv.len() != 12 || tag.len() != 16 {
        return None;
    }
    let key = derive_key(passphrase, &salt)?;
    ciphertext.extend_from_slice(&tag);
    let plain = Aes256Gcm::new(&key.into())
        .decrypt(Nonce::from_slice(&iv), Payload { msg: &ciphertext, aad: &[] })
        .ok()?;
    String::from_utf8(plain).ok()
}

#[cfg(test)]
mod tests;
