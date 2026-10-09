//! Keeps a local folder in step with an S3 bucket that Remotely Save syncs to.
//!
//! The bucket is the source of truth. Each poll lists it, downloads notes whose
//! ETag changed, and deletes local notes that were removed remotely. Only notes
//! the mirror itself downloaded or uploaded (the manifest) are ever deleted, so
//! an unrelated local file is never touched.
//!
//! Network calls run unlocked, so a write never waits for a poll. Only the
//! local apply of a note (its file and manifest entry) is serialized, and a
//! poll leaves alone any note the server wrote after that poll's listing began:
//! a listing that predates a write must never undo it.
//!
//! Timestamps follow Remotely Save's S3 backend: user metadata `MTime`/`CTime`
//! in seconds (a float string), read back case-insensitively, with values of
//! 1e12 or more treated as milliseconds (written by plugin versions before
//! March 2024). Objects without the metadata fall back to LastModified.

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

use super::local::lexical_resolve;
use super::{Listeners, Subscription, VaultChangeListener, VaultError};
use crate::notes::is_valid_note_path;
use crate::util::{encode_uri_component, now_ms};

const DOWNLOAD_CONCURRENCY: usize = 8;
/// Remotely Save's debug-output folder; it can contain .md files that are not notes.
const SKIPPED_PREFIXES: &[&str] = &["_debug_remotely_save/"];

/// What the mirror last downloaded or uploaded for one note.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub etag: String,
    pub mtime: f64,
}

/// The notes the mirror owns, keyed by vault path. Persisted as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub files: HashMap<String, ManifestEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self { version: 1, files: HashMap::new() }
    }
}

/// One note object in the bucket, with the key already stripped of the prefix.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteNote {
    pub path: String,
    pub etag: String,
    /// LastModified in ms; the precise mtime comes from object metadata on download.
    pub last_modified: f64,
}

/// What one poll must do.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MirrorPlan {
    pub download: Vec<RemoteNote>,
    pub remove: Vec<String>,
}

/// Connection settings for the bucket.
#[derive(Debug, Clone, Default)]
pub struct S3Options {
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
}

/// An object as listed.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectSummary {
    pub key: String,
    pub etag: String,
    pub last_modified: Option<f64>,
}

/// An object as downloaded.
#[derive(Debug, Clone, Default)]
pub struct ObjectData {
    pub body: Vec<u8>,
    pub etag: Option<String>,
    pub metadata: HashMap<String, String>,
}

/// The condition an upload must meet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    None,
    /// The bucket still holds this ETag.
    IfMatch(String),
    /// The key does not exist yet.
    IfNoneMatch,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("the object changed since it was last read (HTTP 412)")]
    PreconditionFailed,
    #[error("S3 request failed: {0}")]
    Request(String),
}

/// The S3 operations the mirror uses; [`AwsStore`] talks to a real bucket and
/// tests substitute an in-memory one.
#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// Every object under `prefix`, across all pages.
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectSummary>, StoreError>;
    async fn get(&self, key: &str) -> Result<ObjectData, StoreError>;
    /// Upload `body`; returns the new ETag when the store reports one.
    async fn put(
        &self,
        key: &str,
        body: Vec<u8>,
        metadata: HashMap<String, String>,
        precondition: Precondition,
    ) -> Result<Option<String>, StoreError>;
    /// Server-side copy, keeping metadata; returns the new ETag when reported.
    async fn copy(&self, from_key: &str, to_key: &str) -> Result<Option<String>, StoreError>;
    async fn delete(&self, key: &str) -> Result<(), StoreError>;
}

/// [`ObjectStore`] over the AWS SDK, for any S3-compatible service.
pub struct AwsStore {
    client: Client,
    bucket: String,
}

impl AwsStore {
    /// Fail fast on a socket that stopped answering (a keep-alive connection
    /// that died while the machine was suspended, for instance) so the SDK
    /// retries on a fresh one instead of a poll or write hanging for minutes.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
    const READ_TIMEOUT: Duration = Duration::from_secs(15);

    pub async fn new(options: &S3Options) -> Self {
        use aws_sdk_s3::config::{
            BehaviorVersion, Credentials, Region, RequestChecksumCalculation,
            ResponseChecksumValidation, timeout::TimeoutConfig,
        };
        let mut loader = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(options.region.clone()));
        if let (Some(id), Some(secret)) = (&options.access_key_id, &options.secret_access_key) {
            loader = loader.credentials_provider(Credentials::new(id, secret, None, None, "environment"));
        }
        let shared = loader.load().await;
        let mut config = aws_sdk_s3::config::Builder::from(&shared)
            .force_path_style(true)
            .timeout_config(
                TimeoutConfig::builder()
                    .connect_timeout(Self::CONNECT_TIMEOUT)
                    .read_timeout(Self::READ_TIMEOUT)
                    .build(),
            )
            // S3-compatible services (R2, B2, MinIO) reject or ignore the newer default checksums.
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
        if let Some(endpoint) = &options.endpoint {
            config = config.endpoint_url(endpoint);
        }
        Self { client: Client::from_conf(config.build()), bucket: options.bucket.clone() }
    }
}

fn request_error<E, R>(error: SdkError<E, R>) -> StoreError
where
    E: std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug,
{
    StoreError::Request(DisplayErrorContext(&error).to_string())
}

fn is_precondition_failure<E: ProvideErrorMetadata>(
    error: &SdkError<E, aws_sdk_s3::config::http::HttpResponse>,
) -> bool {
    error.raw_response().is_some_and(|r| r.status().as_u16() == 412)
        || error.as_service_error().and_then(|e| e.code()) == Some("PreconditionFailed")
}

#[async_trait]
impl ObjectStore for AwsStore {
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectSummary>, StoreError> {
        let mut objects = Vec::new();
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .set_prefix((!prefix.is_empty()).then(|| prefix.to_owned()))
            .into_paginator()
            .send();
        while let Some(page) = pages.next().await {
            let page = page.map_err(request_error)?;
            for object in page.contents() {
                let (Some(key), Some(etag)) = (object.key(), object.e_tag()) else { continue };
                objects.push(ObjectSummary {
                    key: key.to_owned(),
                    etag: etag.to_owned(),
                    last_modified: object.last_modified().and_then(|t| t.to_millis().ok()).map(|ms| ms as f64),
                });
            }
        }
        Ok(objects)
    }

    async fn get(&self, key: &str) -> Result<ObjectData, StoreError> {
        let object = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(request_error)?;
        let etag = object.e_tag().map(str::to_owned);
        let metadata = object.metadata().cloned().unwrap_or_default();
        let body = object
            .body
            .collect()
            .await
            .map_err(|e| StoreError::Request(format!("reading the response body failed: {e}")))?
            .into_bytes()
            .to_vec();
        Ok(ObjectData { body, etag, metadata })
    }

    async fn put(
        &self,
        key: &str,
        body: Vec<u8>,
        metadata: HashMap<String, String>,
        precondition: Precondition,
    ) -> Result<Option<String>, StoreError> {
        let mut request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(body))
            .content_type("text/markdown; charset=utf-8")
            .set_metadata(Some(metadata));
        request = match precondition {
            Precondition::None => request,
            Precondition::IfMatch(etag) => request.if_match(etag),
            Precondition::IfNoneMatch => request.if_none_match("*"),
        };
        match request.send().await {
            Ok(result) => Ok(result.e_tag().map(str::to_owned)),
            Err(error) if is_precondition_failure(&error) => Err(StoreError::PreconditionFailed),
            Err(error) => Err(request_error(error)),
        }
    }

    async fn copy(&self, from_key: &str, to_key: &str) -> Result<Option<String>, StoreError> {
        let source: Vec<String> = from_key.split('/').map(encode_uri_component).collect();
        let result = self
            .client
            .copy_object()
            .bucket(&self.bucket)
            .key(to_key)
            .copy_source(format!("{}/{}", self.bucket, source.join("/")))
            .metadata_directive(aws_sdk_s3::types::MetadataDirective::Copy)
            .send()
            .await
            .map_err(request_error)?;
        Ok(result.copy_object_result().and_then(|r| r.e_tag()).map(str::to_owned))
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(request_error)?;
        Ok(())
    }
}

/// Normalize `S3_PREFIX` to `""` or `"folder/"` so keys are always `{prefix}{path}`.
pub fn normalize_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() { String::new() } else { format!("{trimmed}/") }
}

/// Parse Remotely Save's `MTime` metadata into ms, or `None` when absent or zero.
pub fn parse_remote_mtime(metadata: &HashMap<String, String>) -> Option<f64> {
    let raw = metadata.iter().find(|(key, _)| key.eq_ignore_ascii_case("mtime"))?.1;
    let value = parse_float_prefix(raw)?.floor();
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    Some(if value >= 1e12 { value } else { value * 1000.0 })
}

/// Like JavaScript's `parseFloat`: the longest numeric prefix, after leading whitespace.
fn parse_float_prefix(raw: &str) -> Option<f64> {
    let text = raw.trim_start();
    (1..=text.len())
        .rev()
        .filter(|&end| text.is_char_boundary(end))
        .find_map(|end| text[..end].parse::<f64>().ok().filter(|_| !text[..end].ends_with(['+', '-'])))
}

/// The metadata Remotely Save writes, so the plugin reads our uploads' times correctly.
pub fn remote_metadata(mtime: f64, ctime: f64) -> HashMap<String, String> {
    HashMap::from([
        ("MTime".to_owned(), format_seconds(mtime / 1000.0)),
        ("CTime".to_owned(), format_seconds(ctime / 1000.0)),
    ])
}

fn format_seconds(seconds: f64) -> String {
    if seconds.fract() == 0.0 { format!("{}", seconds as i64) } else { format!("{seconds}") }
}

/// Whether a vault path is a note the mirror should carry.
pub fn is_mirrored_path(path: &str) -> bool {
    is_valid_note_path(path) && !SKIPPED_PREFIXES.iter().any(|prefix| path.starts_with(prefix))
}

/// Decide what one poll must do. A note is downloaded when it is new, its ETag
/// changed, or the local copy disappeared; a note is removed locally only when
/// the manifest has it and the listing no longer does.
pub fn plan_mirror(remote: &[RemoteNote], manifest: &Manifest, local_exists: impl Fn(&str) -> bool) -> MirrorPlan {
    let seen: HashSet<&str> = remote.iter().map(|n| n.path.as_str()).collect();
    let download = remote
        .iter()
        .filter(|note| {
            manifest
                .files
                .get(&note.path)
                .is_none_or(|known| known.etag != note.etag || !local_exists(&note.path))
        })
        .cloned()
        .collect();
    let mut remove: Vec<String> = manifest
        .files
        .keys()
        .filter(|path| !seen.contains(path.as_str()))
        .cloned()
        .collect();
    remove.sort();
    MirrorPlan { download, remove }
}

/// Per path, the sequence number of the server's latest write.
#[derive(Default)]
struct WriteLog {
    seq: u64,
    last: HashMap<String, u64>,
}

/// The mirror of one bucket prefix in one local folder.
pub struct S3Mirror {
    root: PathBuf,
    manifest_path: PathBuf,
    prefix: String,
    store: Arc<dyn ObjectStore>,
    /// The manifest; holding it serializes local state changes (note files and the manifest).
    state: tokio::sync::Mutex<Manifest>,
    listeners: Listeners,
    writes: parking_lot::Mutex<WriteLog>,
    /// Held for the duration of a poll.
    polling: tokio::sync::Mutex<()>,
}

impl S3Mirror {
    pub fn new(root: impl Into<PathBuf>, manifest_path: impl Into<PathBuf>, prefix: &str, store: Arc<dyn ObjectStore>) -> Self {
        Self {
            root: root.into(),
            manifest_path: manifest_path.into(),
            prefix: normalize_prefix(prefix),
            store,
            state: tokio::sync::Mutex::new(Manifest::default()),
            listeners: Listeners::default(),
            writes: parking_lot::Mutex::new(WriteLog::default()),
            polling: tokio::sync::Mutex::new(()),
        }
    }

    pub async fn etag_of(&self, path: &str) -> Option<String> {
        self.state.lock().await.files.get(path).map(|e| e.etag.clone())
    }

    /// A copy of the manifest, for inspection.
    pub async fn manifest(&self) -> Manifest {
        self.state.lock().await.clone()
    }

    /// Hear about notes a poll downloaded or removed. The mirror's own uploads
    /// are not reported: the caller that wrote them already knows.
    pub fn subscribe(&self, listener: Arc<dyn VaultChangeListener>) -> Subscription {
        self.listeners.subscribe(listener)
    }

    /// Resolves once no poll or local apply is in flight.
    pub async fn idle(&self) {
        drop(self.polling.lock().await);
        drop(self.state.lock().await);
    }

    /// Record that the server is writing `path` now.
    fn touch(&self, path: &str) {
        let mut writes = self.writes.lock();
        writes.seq += 1;
        let seq = writes.seq;
        writes.last.insert(path.to_owned(), seq);
    }

    /// Whether the server wrote `path` after sequence number `seq` was taken.
    fn touched_since(&self, path: &str, seq: u64) -> bool {
        self.writes.lock().last.get(path).is_some_and(|&s| s > seq)
    }

    pub async fn load_manifest(&self) {
        let loaded = match tokio::fs::read(&self.manifest_path).await {
            Ok(bytes) => match serde_json::from_slice::<Manifest>(&bytes) {
                Ok(manifest) if manifest.version == 1 => manifest,
                _ => {
                    warn!("S3 mirror manifest is unreadable; re-downloading every note.");
                    Manifest::default()
                }
            },
            Err(e) => {
                if e.kind() != ErrorKind::NotFound {
                    warn!("S3 mirror manifest is unreadable; re-downloading every note.");
                }
                Manifest::default()
            }
        };
        *self.state.lock().await = loaded;
    }

    /// Callers hold the state lock and pass the manifest it guards.
    async fn save_manifest(&self, manifest: &Manifest) -> Result<(), VaultError> {
        if let Some(parent) = self.manifest_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut tmp = self.manifest_path.clone().into_os_string();
        tmp.push(".tmp");
        tokio::fs::write(&tmp, serde_json::to_vec(manifest).map_err(std::io::Error::other)?).await?;
        tokio::fs::rename(&tmp, &self.manifest_path).await?;
        Ok(())
    }

    /// Absolute local path for a note; refuses anything that resolves outside the mirror root.
    pub fn local_path(&self, path: &str) -> Result<PathBuf, VaultError> {
        let full = lexical_resolve(&self.root, path);
        if full == self.root || !full.starts_with(&self.root) {
            return Err(VaultError::Traversal);
        }
        Ok(full)
    }

    /// Whether the note exists in the mirror folder.
    pub async fn local_exists(&self, path: &str) -> bool {
        match self.local_path(path) {
            Ok(full) => tokio::fs::symlink_metadata(full).await.is_ok(),
            Err(_) => false,
        }
    }

    fn key(&self, path: &str) -> String {
        format!("{}{path}", self.prefix)
    }

    /// Every note in the bucket under the prefix.
    pub async fn list(&self) -> Result<Vec<RemoteNote>, StoreError> {
        let objects = self.store.list(&self.prefix).await?;
        Ok(objects
            .into_iter()
            .filter_map(|object| {
                let path = object.key.strip_prefix(&self.prefix)?.to_owned();
                is_mirrored_path(&path).then(|| RemoteNote {
                    path,
                    etag: object.etag,
                    last_modified: object.last_modified.unwrap_or_else(|| now_ms() as f64),
                })
            })
            .collect())
    }

    /// One full pass: list, download changes, delete removed notes. Returns the
    /// number of notes changed locally. Polls never overlap.
    pub async fn sync(&self) -> Result<usize, VaultError> {
        let _polling = self.polling.lock().await;
        // Anything the server writes from here on postdates the listing below.
        let since = self.writes.lock().seq;
        let remote = self.list().await?;
        let known: Vec<String> = self.state.lock().await.files.keys().cloned().collect();
        let mut present = HashSet::new();
        for path in known {
            if self.local_exists(&path).await {
                present.insert(path);
            }
        }
        let plan = {
            let manifest = self.state.lock().await;
            plan_mirror(&remote, &manifest, |path| present.contains(path))
        };

        let queue: Vec<RemoteNote> =
            plan.download.into_iter().filter(|note| !self.touched_since(&note.path, since)).collect();
        let mut changed = futures::stream::iter(queue)
            .map(|note| async move {
                match self.download(&note, since).await {
                    Ok(applied) => usize::from(applied),
                    Err(error) => {
                        warn!("S3 mirror: download failed: {error}");
                        0
                    }
                }
            })
            .buffer_unordered(DOWNLOAD_CONCURRENCY)
            .fold(0, |sum, n| async move { sum + n })
            .await;

        for path in &plan.remove {
            if self.forget(path, since).await? {
                changed += 1;
            }
        }
        if changed > 0 {
            let manifest = self.state.lock().await;
            self.save_manifest(&manifest).await?;
        }
        // Writes older than this listing can no longer collide with a poll.
        self.writes.lock().last.retain(|_, seq| *seq > since);
        Ok(changed)
    }

    /// Fetch one note and apply it locally, unless the server wrote it after `since`.
    async fn download(&self, note: &RemoteNote, since: u64) -> Result<bool, VaultError> {
        let object = self.store.get(&self.key(&note.path)).await?;
        let mtime = parse_remote_mtime(&object.metadata).unwrap_or(note.last_modified);
        let mut manifest = self.state.lock().await;
        if self.touched_since(&note.path, since) {
            return Ok(false);
        }
        self.write_local(&note.path, &object.body, mtime).await?;
        let etag = object.etag.unwrap_or_else(|| note.etag.clone());
        manifest.files.insert(note.path.clone(), ManifestEntry { etag, mtime });
        if !self.listeners.is_empty() {
            // A BOM, if any, stays in the text, as when the file is read from disk.
            let content = String::from_utf8_lossy(&object.body);
            self.listeners.notify(|l| l.updated(&note.path, &content, mtime));
        }
        Ok(true)
    }

    /// Drop a note that left the bucket, unless the server wrote it after `since`.
    async fn forget(&self, path: &str, since: u64) -> Result<bool, VaultError> {
        let mut manifest = self.state.lock().await;
        if self.touched_since(path, since) || !manifest.files.contains_key(path) {
            return Ok(false);
        }
        match tokio::fs::remove_file(self.local_path(path)?).await {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        manifest.files.remove(path);
        self.listeners.notify(|l| l.removed(path));
        Ok(true)
    }

    async fn write_local(&self, path: &str, data: &[u8], mtime: f64) -> Result<(), VaultError> {
        let full = self.local_path(path)?;
        let data = data.to_vec();
        tokio::task::spawn_blocking(move || write_with_mtime(&full, &data, mtime))
            .await
            .map_err(std::io::Error::other)??;
        Ok(())
    }

    /// Upload a note, then mirror it locally. The upload only succeeds if the
    /// bucket meets `precondition`; a lost race is reported as
    /// [`VaultError::RemoteChanged`] instead of overwriting the newer version.
    pub async fn put(&self, path: &str, content: &str, precondition: Precondition) -> Result<(), VaultError> {
        self.touch(path);
        let mtime = now_ms() as f64;
        let etag = match self
            .store
            .put(&self.key(path), content.as_bytes().to_vec(), remote_metadata(mtime, mtime), precondition)
            .await
        {
            Ok(etag) => etag,
            Err(StoreError::PreconditionFailed) => return Err(VaultError::RemoteChanged(path.to_owned())),
            Err(error) => return Err(error.into()),
        };
        self.touch(path);
        let mut manifest = self.state.lock().await;
        self.write_local(path, content.as_bytes(), mtime).await?;
        manifest.files.insert(path.to_owned(), ManifestEntry { etag: etag.unwrap_or_default(), mtime });
        self.save_manifest(&manifest).await
    }

    /// Copy a note within the bucket, keeping its metadata; the caller moves the local file.
    pub async fn copy(&self, from: &str, to: &str) -> Result<(), VaultError> {
        self.touch(to);
        let etag = self.store.copy(&self.key(from), &self.key(to)).await?;
        self.touch(to);
        let mut manifest = self.state.lock().await;
        let mtime = manifest.files.get(from).map_or(now_ms() as f64, |e| e.mtime);
        manifest.files.insert(to.to_owned(), ManifestEntry { etag: etag.unwrap_or_default(), mtime });
        Ok(())
    }

    /// Delete a note in the bucket and forget it; the caller removes the local file.
    pub async fn remove(&self, path: &str) -> Result<(), VaultError> {
        self.touch(path);
        self.store.delete(&self.key(path)).await?;
        self.touch(path);
        let mut manifest = self.state.lock().await;
        manifest.files.remove(path);
        self.save_manifest(&manifest).await
    }
}

/// Write `data` next to `full`, stamp it with `mtime` (ms), then rename it into place.
fn write_with_mtime(full: &Path, data: &[u8], mtime: f64) -> std::io::Result<()> {
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = full.as_os_str().to_owned();
    tmp.push(".s3-mirror.tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, data)?;
    let time = UNIX_EPOCH + Duration::from_secs_f64((mtime / 1000.0).max(0.0));
    let times = std::fs::FileTimes::new().set_accessed(time).set_modified(time);
    std::fs::File::options().write(true).open(&tmp)?.set_times(times)?;
    std::fs::rename(&tmp, full)?;
    Ok(())
}

#[cfg(test)]
mod tests;
