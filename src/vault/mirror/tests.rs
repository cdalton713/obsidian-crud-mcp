use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use tempfile::TempDir;
use tokio::sync::{Notify, watch};

use super::*;
use crate::util::system_time_ms;
use crate::vault::{S3Vault, S3VaultOptions, VaultBackend};

#[derive(Debug, Clone)]
struct StoredObject {
    body: Vec<u8>,
    etag: String,
    metadata: HashMap<String, String>,
    last_modified: f64,
}

/// An in-memory bucket with S3's conditional-write rules, a log of the calls
/// it served, injectable failures, and gates that hold listings or downloads.
struct FakeStore {
    objects: Mutex<BTreeMap<String, StoredObject>>,
    version: Mutex<u64>,
    calls: Mutex<Vec<String>>,
    /// Operations ("list", "get", "put", "copy", "delete") that fail with a network error.
    failing: Mutex<HashSet<&'static str>>,
    /// While true, a listing snapshots the bucket and then waits before answering.
    hold_lists: watch::Sender<bool>,
    /// While true, a download fetches the object and then waits before answering.
    hold_gets: watch::Sender<bool>,
    /// Signalled once a held call has taken its snapshot.
    held: Notify,
}

impl FakeStore {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            objects: Mutex::default(),
            version: Mutex::default(),
            calls: Mutex::default(),
            failing: Mutex::default(),
            hold_lists: watch::channel(false).0,
            hold_gets: watch::channel(false).0,
            held: Notify::new(),
        })
    }

    fn next_etag(&self) -> String {
        let mut version = self.version.lock();
        *version += 1;
        format!("\"etag-{version}\"")
    }

    /// Remotely Save uploading a note from a device.
    fn device_upload(&self, key: &str, body: &str, mtime: f64) {
        let object = StoredObject {
            body: body.as_bytes().to_vec(),
            etag: self.next_etag(),
            metadata: remote_metadata(mtime, mtime),
            last_modified: now_ms() as f64,
        };
        self.objects.lock().insert(key.to_owned(), object);
    }

    fn device_delete(&self, key: &str) {
        self.objects.lock().remove(key);
    }

    fn body(&self, key: &str) -> Option<String> {
        self.objects.lock().get(key).map(|o| String::from_utf8(o.body.clone()).unwrap())
    }

    fn metadata(&self, key: &str) -> HashMap<String, String> {
        self.objects.lock().get(key).map(|o| o.metadata.clone()).unwrap_or_default()
    }

    fn calls_to(&self, operation: &str) -> Vec<String> {
        let prefix = format!("{operation}:");
        self.calls.lock().iter().filter_map(|c| c.strip_prefix(&prefix).map(str::to_owned)).collect()
    }

    fn fail(&self, operation: &'static str) {
        self.failing.lock().insert(operation);
    }

    fn heal(&self, operation: &'static str) {
        self.failing.lock().remove(operation);
    }

    fn record(&self, operation: &'static str, key: &str) -> Result<(), StoreError> {
        self.calls.lock().push(format!("{operation}:{key}"));
        if self.failing.lock().contains(operation) {
            return Err(StoreError::Request("network down".to_owned()));
        }
        Ok(())
    }

    async fn pass(&self, gate: &watch::Sender<bool>) {
        if *gate.borrow() {
            self.held.notify_one();
            let mut released = gate.subscribe();
            released.wait_for(|held| !held).await.unwrap();
        }
    }
}

#[async_trait]
impl ObjectStore for FakeStore {
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectSummary>, StoreError> {
        self.record("list", prefix)?;
        let snapshot: Vec<ObjectSummary> = self
            .objects
            .lock()
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, object)| ObjectSummary {
                key: key.clone(),
                etag: object.etag.clone(),
                last_modified: Some(object.last_modified),
            })
            .collect();
        self.pass(&self.hold_lists).await;
        Ok(snapshot)
    }

    async fn get(&self, key: &str) -> Result<ObjectData, StoreError> {
        self.record("get", key)?;
        let object = self.objects.lock().get(key).cloned().ok_or_else(|| StoreError::Request("NoSuchKey".into()))?;
        self.pass(&self.hold_gets).await;
        // S3 hands user metadata back with lowercased keys.
        let metadata = object.metadata.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect();
        Ok(ObjectData { body: object.body, etag: Some(object.etag), metadata })
    }

    async fn put(
        &self,
        key: &str,
        body: Vec<u8>,
        metadata: HashMap<String, String>,
        precondition: Precondition,
    ) -> Result<Option<String>, StoreError> {
        self.record("put", key)?;
        let mut objects = self.objects.lock();
        let existing = objects.get(key).map(|o| o.etag.clone());
        match precondition {
            Precondition::IfNoneMatch if existing.is_some() => return Err(StoreError::PreconditionFailed),
            Precondition::IfMatch(etag) if existing.as_ref() != Some(&etag) => {
                return Err(StoreError::PreconditionFailed);
            }
            _ => {}
        }
        let etag = self.next_etag();
        objects.insert(
            key.to_owned(),
            StoredObject { body, etag: etag.clone(), metadata, last_modified: now_ms() as f64 },
        );
        Ok(Some(etag))
    }

    async fn copy(&self, from_key: &str, to_key: &str) -> Result<Option<String>, StoreError> {
        self.record("copy", to_key)?;
        let mut objects = self.objects.lock();
        let source = objects.get(from_key).cloned().ok_or_else(|| StoreError::Request("NoSuchKey".into()))?;
        let etag = self.next_etag();
        objects.insert(to_key.to_owned(), StoredObject { etag: etag.clone(), ..source });
        Ok(Some(etag))
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.record("delete", key)?;
        self.objects.lock().remove(key);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Updated(String, String, f64),
    Removed(String),
}

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<Event>>,
}

impl Recorder {
    fn take(&self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock())
    }
}

impl VaultChangeListener for Recorder {
    fn updated(&self, path: &str, content: &str, mtime: f64) {
        self.events.lock().push(Event::Updated(path.to_owned(), content.to_owned(), mtime));
    }

    fn removed(&self, path: &str) {
        self.events.lock().push(Event::Removed(path.to_owned()));
    }
}

const MTIME: f64 = 1_700_000_000_000.0;

struct Fixture {
    dir: TempDir,
    store: Arc<FakeStore>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("vault")).unwrap();
        Self { dir, store: FakeStore::new() }
    }

    fn vault_path(&self) -> PathBuf {
        self.dir.path().join("vault")
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir.path().join("data").join("s3-manifest.json")
    }

    fn open_with(&self, prefix: &str, write_folders: Option<&[&str]>) -> Arc<S3Vault> {
        let options = S3VaultOptions {
            vault_path: self.vault_path(),
            manifest_path: self.manifest_path(),
            poll_seconds: 3600,
            prefix: prefix.to_owned(),
            write_folders: write_folders.map(|f| f.iter().map(|s| s.to_string()).collect()),
        };
        Arc::new(S3Vault::new(options, self.store.clone()).unwrap())
    }

    fn open(&self) -> Arc<S3Vault> {
        self.open_with("", None)
    }

    async fn open_ready(&self) -> Arc<S3Vault> {
        let vault = self.open();
        vault.init().await.unwrap();
        vault
    }

    fn local_mtime(&self, path: &str) -> f64 {
        system_time_ms(std::fs::metadata(self.vault_path().join(path)).unwrap().modified().unwrap())
    }
}

async fn read(vault: &S3Vault, path: &str) -> Option<String> {
    vault.read_note(path).await.unwrap()
}

fn assert_remote_changed<T: std::fmt::Debug>(result: Result<T, VaultError>, path: &str) {
    match result {
        Err(VaultError::RemoteChanged(p)) => assert_eq!(p, path),
        other => panic!("expected RemoteChanged({path}), got {other:?}"),
    }
}

#[test]
fn reads_remote_mtime_in_seconds_or_legacy_milliseconds() {
    let meta = |k: &str, v: &str| HashMap::from([(k.to_owned(), v.to_owned())]);
    assert_eq!(parse_remote_mtime(&meta("mtime", "1700000000.5")), Some(MTIME));
    assert_eq!(parse_remote_mtime(&meta("MTime", "1700000000123")), Some(1_700_000_000_123.0));
    assert_eq!(parse_remote_mtime(&meta("mtime", " 1700000000xyz")), Some(MTIME), "parsed like parseFloat");
    assert_eq!(parse_remote_mtime(&meta("mtime", "0")), None);
    assert_eq!(parse_remote_mtime(&meta("mtime", "soon")), None);
    assert_eq!(parse_remote_mtime(&HashMap::new()), None);
}

#[test]
fn writes_metadata_in_the_seconds_format_the_plugin_reads() {
    let meta = remote_metadata(1_700_000_000_500.0, MTIME);
    assert_eq!(meta.get("MTime").map(String::as_str), Some("1700000000.5"));
    assert_eq!(meta.get("CTime").map(String::as_str), Some("1700000000"));
    assert_eq!(parse_remote_mtime(&meta), Some(MTIME), "a round trip floors to whole seconds");
}

#[test]
fn normalizes_prefixes_to_empty_or_a_trailing_slash() {
    assert_eq!(normalize_prefix(""), "");
    assert_eq!(normalize_prefix("/"), "");
    assert_eq!(normalize_prefix("/vault/"), "vault/");
    assert_eq!(normalize_prefix("a/b"), "a/b/");
}

#[test]
fn mirrors_only_notes_outside_the_debug_folder() {
    assert!(is_mirrored_path("Daily/today.md"));
    assert!(!is_mirrored_path("_debug_remotely_save/log.md"));
    assert!(!is_mirrored_path(".obsidian/app.md"));
    assert!(!is_mirrored_path("image.png"));
}

#[test]
fn plans_downloads_for_new_changed_and_missing_notes_and_removes_only_tracked_ones() {
    let entry = || ManifestEntry { etag: "\"1\"".into(), mtime: 1.0 };
    let manifest = Manifest {
        version: 1,
        files: ["same.md", "changed.md", "missing-locally.md", "deleted-remotely.md"]
            .into_iter()
            .map(|p| (p.to_owned(), entry()))
            .collect(),
    };
    let note = |path: &str, etag: &str| RemoteNote { path: path.into(), etag: etag.into(), last_modified: 1.0 };
    let remote = [
        note("same.md", "\"1\""),
        note("changed.md", "\"2\""),
        note("missing-locally.md", "\"1\""),
        note("new.md", "\"1\""),
    ];
    let plan = plan_mirror(&remote, &manifest, |path| path != "missing-locally.md");
    let mut download: Vec<_> = plan.download.iter().map(|n| n.path.as_str()).collect();
    download.sort();
    assert_eq!(download, ["changed.md", "missing-locally.md", "new.md"]);
    assert_eq!(plan.remove, ["deleted-remotely.md"]);
}

#[tokio::test]
async fn mirrors_notes_on_init_with_the_device_mtime_skipping_non_notes() {
    let fx = Fixture::new();
    for (key, body) in [
        ("Daily/2026-10-06.md", "# today"),
        ("Inbox.md", "inbox"),
        ("image.png", "binary"),
        (".obsidian/app.md", "config"),
        ("_remotely-save-metadata-on-remote.json", "{}"),
        ("_debug_remotely_save/log.md", "debug"),
    ] {
        fx.store.device_upload(key, body, MTIME);
    }
    let vault = fx.open_ready().await;

    assert_eq!(vault.list_notes(None).await.unwrap(), ["Daily/2026-10-06.md", "Inbox.md"]);
    assert_eq!(read(&vault, "Daily/2026-10-06.md").await.as_deref(), Some("# today"));
    assert_eq!(fx.local_mtime("Inbox.md"), MTIME);
    assert!(!fx.vault_path().join("image.png").exists());
    assert!(!fx.vault_path().join("_debug_remotely_save").exists());
    vault.close().await;
}

#[tokio::test]
async fn falls_back_to_last_modified_without_mtime_metadata() {
    let fx = Fixture::new();
    fx.store.device_upload("a.md", "a", MTIME);
    fx.store.objects.lock().get_mut("a.md").unwrap().metadata.clear();
    let listed = fx.store.objects.lock()["a.md"].last_modified;
    let vault = fx.open_ready().await;
    assert_eq!(vault.mirror().manifest().await.files["a.md"].mtime, listed);
    vault.close().await;
}

#[tokio::test]
async fn strips_the_configured_prefix_and_ignores_keys_outside_it() {
    let fx = Fixture::new();
    fx.store.device_upload("vault/Note.md", "inside", MTIME);
    fx.store.device_upload("other/Note.md", "outside", MTIME);
    let vault = fx.open_with("vault", None);
    vault.init().await.unwrap();
    assert_eq!(vault.list_notes(None).await.unwrap(), ["Note.md"]);
    assert_eq!(read(&vault, "Note.md").await.as_deref(), Some("inside"));
    assert_eq!(fx.store.calls_to("list"), ["vault/"]);

    vault.write_note("New.md", "mine").await.unwrap();
    assert_eq!(fx.store.body("vault/New.md").as_deref(), Some("mine"), "writes carry the prefix too");
    vault.close().await;
}

#[tokio::test]
async fn picks_up_remote_edits_and_deletions_leaving_untracked_files_alone() {
    let fx = Fixture::new();
    fx.store.device_upload("a.md", "v1", MTIME);
    fx.store.device_upload("b.md", "keep", MTIME);
    let vault = fx.open_ready().await;
    std::fs::write(fx.vault_path().join("local-only.md"), "untracked").unwrap();

    fx.store.device_upload("a.md", "v2", MTIME + 100_000.0);
    fx.store.device_delete("b.md");
    assert_eq!(vault.poll().await.unwrap(), 2);

    assert_eq!(read(&vault, "a.md").await.as_deref(), Some("v2"));
    assert_eq!(read(&vault, "b.md").await, None);
    assert_eq!(read(&vault, "local-only.md").await.as_deref(), Some("untracked"));
    assert_eq!(vault.poll().await.unwrap(), 0);
    vault.close().await;
}

#[tokio::test]
async fn restores_a_tracked_note_deleted_locally() {
    let fx = Fixture::new();
    fx.store.device_upload("a.md", "v1", MTIME);
    let vault = fx.open_ready().await;
    std::fs::remove_file(fx.vault_path().join("a.md")).unwrap();
    assert_eq!(vault.poll().await.unwrap(), 1);
    assert_eq!(read(&vault, "a.md").await.as_deref(), Some("v1"));
    vault.close().await;
}

#[tokio::test]
async fn reports_poll_changes_to_subscribers_but_not_its_own_writes() {
    let fx = Fixture::new();
    fx.store.device_upload("a.md", "v1", MTIME);
    fx.store.device_upload("b.md", "bye", MTIME);
    let vault = fx.open();
    let recorder = Arc::new(Recorder::default());
    let subscription = vault.subscribe(recorder.clone()).expect("the S3 vault reports changes");
    vault.init().await.unwrap();

    let mut events = recorder.take();
    events.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    assert_eq!(
        events,
        [Event::Updated("a.md".into(), "v1".into(), MTIME), Event::Updated("b.md".into(), "bye".into(), MTIME)]
    );
    // The reported mtime is the mirrored file's, so an mtime-based rescan skips it.
    assert_eq!(fx.local_mtime("a.md"), MTIME);

    vault.write_note("c.md", "mine").await.unwrap();
    assert_eq!(recorder.take(), [], "the server's own write is not reported");

    fx.store.device_upload("a.md", "v2", MTIME + 100_000.0);
    fx.store.device_delete("b.md");
    vault.poll().await.unwrap();
    assert_eq!(
        recorder.take(),
        [Event::Updated("a.md".into(), "v2".into(), MTIME + 100_000.0), Event::Removed("b.md".into())]
    );

    drop(subscription);
    fx.store.device_upload("a.md", "v3", MTIME + 200_000.0);
    assert_eq!(vault.poll().await.unwrap(), 1);
    assert_eq!(recorder.take(), [], "nothing is reported after unsubscribing");
    vault.close().await;
}

#[tokio::test]
async fn writes_and_deletes_during_a_poll_are_not_undone_by_its_stale_listing() {
    let fx = Fixture::new();
    fx.store.device_upload("a.md", "v1", MTIME);
    fx.store.device_upload("gone.md", "bye", MTIME);
    let vault = fx.open_ready().await;

    fx.store.hold_lists.send_replace(true);
    // Its listing shows a.md=v1 and gone.md, and no new.md.
    let polling = tokio::spawn({
        let vault = vault.clone();
        async move { vault.poll().await }
    });
    fx.store.held.notified().await;
    // None of these may wait for the poll, or the test would hang.
    vault.write_note("new.md", "created meanwhile").await.unwrap();
    vault.write_note("a.md", "edited meanwhile").await.unwrap();
    assert!(vault.delete_note("gone.md").await.unwrap());
    assert_eq!(read(&vault, "a.md").await.as_deref(), Some("edited meanwhile"));
    assert_eq!(fx.store.body("a.md").as_deref(), Some("edited meanwhile"));

    fx.store.hold_lists.send_replace(false);
    assert_eq!(polling.await.unwrap().unwrap(), 0);
    assert_eq!(read(&vault, "new.md").await.as_deref(), Some("created meanwhile"));
    assert_eq!(read(&vault, "a.md").await.as_deref(), Some("edited meanwhile"));
    assert_eq!(read(&vault, "gone.md").await, None);
    let mut gets = fx.store.calls_to("get");
    gets.sort();
    assert_eq!(gets, ["a.md", "gone.md"], "only the initial mirror downloaded anything");
    // The manifest agrees with the bucket, so the next poll is a no-op.
    assert_eq!(vault.poll().await.unwrap(), 0);
    vault.close().await;
}

#[tokio::test]
async fn a_download_in_flight_does_not_overwrite_a_newer_write() {
    let fx = Fixture::new();
    let vault = fx.open_ready().await;
    // An untracked local note has no known ETag, so the server's write is unconditional.
    std::fs::write(fx.vault_path().join("x.md"), "local").unwrap();
    fx.store.device_upload("x.md", "device", MTIME);

    fx.store.hold_gets.send_replace(true);
    let polling = tokio::spawn({
        let vault = vault.clone();
        async move { vault.poll().await }
    });
    fx.store.held.notified().await;
    vault.write_note("x.md", "server").await.unwrap();
    fx.store.hold_gets.send_replace(false);

    assert_eq!(polling.await.unwrap().unwrap(), 0, "the stale download was dropped");
    assert_eq!(read(&vault, "x.md").await.as_deref(), Some("server"));
    assert_eq!(fx.store.body("x.md").as_deref(), Some("server"));
    assert_eq!(vault.poll().await.unwrap(), 0);
    vault.close().await;
}

#[tokio::test]
async fn a_second_poll_waits_for_the_first_instead_of_overlapping() {
    let fx = Fixture::new();
    fx.store.device_upload("a.md", "v1", MTIME);
    let vault = fx.open_ready().await;
    fx.store.device_upload("a.md", "v2", MTIME + 1000.0);

    fx.store.hold_lists.send_replace(true);
    let first = tokio::spawn({
        let vault = vault.clone();
        async move { vault.poll().await }
    });
    fx.store.held.notified().await;
    let second = tokio::spawn({
        let vault = vault.clone();
        async move { vault.poll().await }
    });
    tokio::task::yield_now().await;
    assert_eq!(fx.store.calls_to("list").len(), 2, "the second poll has not listed yet");
    fx.store.hold_lists.send_replace(false);

    assert_eq!(first.await.unwrap().unwrap(), 1);
    assert_eq!(second.await.unwrap().unwrap(), 0, "the second poll saw the first one's result");
    assert_eq!(fx.store.calls_to("list").len(), 3);
    vault.close().await;
}

#[tokio::test]
async fn does_not_redownload_unchanged_notes_after_a_restart() {
    let fx = Fixture::new();
    fx.store.device_upload("a.md", "v1", MTIME);
    let vault = fx.open_ready().await;
    vault.close().await;
    drop(vault);

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fx.manifest_path()).unwrap()).expect("the manifest is JSON");
    assert_eq!(manifest["version"], 1);
    assert_eq!(manifest["files"]["a.md"]["etag"], "\"etag-1\"");
    assert_eq!(manifest["files"]["a.md"]["mtime"], MTIME);

    let vault = fx.open_ready().await;
    assert_eq!(fx.store.calls_to("get").len(), 1);
    vault.close().await;
}

#[tokio::test]
async fn redownloads_everything_when_the_manifest_is_unreadable() {
    let fx = Fixture::new();
    fx.store.device_upload("a.md", "v1", MTIME);
    let vault = fx.open_ready().await;
    vault.close().await;
    drop(vault);
    std::fs::write(fx.manifest_path(), "{not json").unwrap();

    let vault = fx.open();
    vault.init().await.unwrap();
    assert_eq!(fx.store.calls_to("get"), ["a.md", "a.md"]);
    assert_eq!(read(&vault, "a.md").await.as_deref(), Some("v1"));
    vault.close().await;
}

#[tokio::test]
async fn a_failed_download_is_retried_on_the_next_poll() {
    let fx = Fixture::new();
    fx.store.device_upload("a.md", "v1", MTIME);
    fx.store.fail("get");
    let vault = fx.open();
    assert_eq!(vault.poll().await.unwrap(), 0, "a failed download does not fail the poll");
    assert_eq!(read(&vault, "a.md").await, None);

    fx.store.heal("get");
    assert_eq!(vault.poll().await.unwrap(), 1);
    assert_eq!(read(&vault, "a.md").await.as_deref(), Some("v1"));
}

#[tokio::test]
async fn a_failed_listing_fails_the_poll() {
    let fx = Fixture::new();
    fx.store.fail("list");
    let vault = fx.open();
    let result = vault.init().await;
    assert!(matches!(result, Err(VaultError::Store(StoreError::Request(_)))), "got {result:?}");
}

#[tokio::test]
async fn uploads_writes_with_remotely_save_metadata_before_writing_locally() {
    let fx = Fixture::new();
    let vault = fx.open_ready().await;
    assert!(vault.write_note("Notes/new.md", "hello").await.unwrap());

    assert_eq!(fx.store.body("Notes/new.md").as_deref(), Some("hello"));
    let mtime = fx.store.metadata("Notes/new.md").remove("MTime").expect("MTime metadata");
    assert!(mtime.chars().all(|c| c.is_ascii_digit() || c == '.'), "seconds as a number: {mtime}");
    assert_eq!(std::fs::read_to_string(fx.vault_path().join("Notes/new.md")).unwrap(), "hello");
    // The local copy carries the upload's mtime, which the plugin reads back to the second.
    let local = fx.local_mtime("Notes/new.md");
    assert_eq!(parse_remote_mtime(&fx.store.metadata("Notes/new.md")), Some((local / 1000.0).floor() * 1000.0));
    // The note is tracked, so the next poll neither downloads nor deletes it.
    assert_eq!(vault.poll().await.unwrap(), 0);
    vault.close().await;
}

#[tokio::test]
async fn leaves_the_local_copy_untouched_when_the_upload_fails() {
    let fx = Fixture::new();
    fx.store.device_upload("a.md", "v1", MTIME);
    let vault = fx.open_ready().await;
    fx.store.fail("put");
    let result = vault.write_note("a.md", "v2").await;
    match result {
        Err(error @ VaultError::Store(_)) => assert!(error.to_string().contains("network down"), "{error}"),
        other => panic!("expected a store error, got {other:?}"),
    }
    assert_eq!(read(&vault, "a.md").await.as_deref(), Some("v1"));
    vault.close().await;
}

#[tokio::test]
async fn refuses_to_overwrite_a_note_that_changed_in_the_bucket_since_the_last_poll() {
    let fx = Fixture::new();
    fx.store.device_upload("a.md", "v1", MTIME);
    let vault = fx.open_ready().await;
    fx.store.device_upload("a.md", "device edit", MTIME + 100_000.0);

    assert_remote_changed(vault.write_note("a.md", "agent edit").await, "a.md");
    assert_eq!(fx.store.body("a.md").as_deref(), Some("device edit"));
    assert_eq!(read(&vault, "a.md").await.as_deref(), Some("v1"), "the local copy is untouched");

    vault.poll().await.unwrap();
    assert!(vault.write_note("a.md", "agent edit").await.unwrap());
    assert_eq!(fx.store.body("a.md").as_deref(), Some("agent edit"));
    vault.close().await;
}

#[tokio::test]
async fn refuses_to_create_a_note_that_exists_remotely_but_is_not_mirrored_yet() {
    let fx = Fixture::new();
    let vault = fx.open_ready().await;
    fx.store.device_upload("race.md", "device", MTIME);
    assert_remote_changed(vault.write_note("race.md", "agent").await, "race.md");
    assert_eq!(fx.store.body("race.md").as_deref(), Some("device"));
    vault.close().await;
}

#[tokio::test]
async fn deletes_remotely_and_locally() {
    let fx = Fixture::new();
    fx.store.device_upload("a.md", "v1", MTIME);
    let vault = fx.open_ready().await;
    assert!(vault.delete_note("a.md").await.unwrap());
    assert_eq!(fx.store.body("a.md"), None);
    assert_eq!(read(&vault, "a.md").await, None);
    assert!(!vault.delete_note("a.md").await.unwrap());
    assert!(vault.mirror().manifest().await.files.is_empty());
    vault.close().await;
}

#[tokio::test]
async fn moves_with_copy_then_delete_keeping_the_device_mtime_metadata() {
    let fx = Fixture::new();
    fx.store.device_upload("a.md", "content", MTIME);
    let vault = fx.open_ready().await;
    assert!(vault.move_note("a.md", "Folder/b.md").await.unwrap());

    assert_eq!(fx.store.body("a.md"), None);
    assert_eq!(fx.store.body("Folder/b.md").as_deref(), Some("content"));
    assert_eq!(fx.store.metadata("Folder/b.md").get("MTime").map(String::as_str), Some("1700000000"));
    assert_eq!(vault.list_notes(None).await.unwrap(), ["Folder/b.md"]);
    let manifest = vault.mirror().manifest().await;
    assert_eq!(manifest.files.keys().collect::<Vec<_>>(), ["Folder/b.md"]);
    assert_eq!(manifest.files["Folder/b.md"].mtime, MTIME);
    assert_eq!(vault.poll().await.unwrap(), 0);
    vault.close().await;
}

#[tokio::test]
async fn moves_onto_the_same_path_or_from_a_missing_note_without_touching_the_bucket() {
    let fx = Fixture::new();
    fx.store.device_upload("a.md", "a", MTIME);
    let vault = fx.open_ready().await;
    assert!(vault.move_note("a.md", "a.md").await.unwrap());
    assert!(!vault.move_note("missing.md", "b.md").await.unwrap());
    assert_eq!(fx.store.calls_to("copy"), Vec::<String>::new());
    assert_eq!(fx.store.calls_to("delete"), Vec::<String>::new());
    vault.close().await;
}

#[tokio::test]
async fn refuses_to_move_onto_an_existing_note() {
    let fx = Fixture::new();
    fx.store.device_upload("a.md", "a", MTIME);
    fx.store.device_upload("b.md", "b", MTIME);
    let vault = fx.open_ready().await;
    let result = vault.move_note("a.md", "b.md").await;
    assert!(matches!(&result, Err(VaultError::DestinationExists(to)) if to == "b.md"), "got {result:?}");
    assert_eq!(fx.store.body("b.md").as_deref(), Some("b"));
    assert_eq!(fx.store.body("a.md").as_deref(), Some("a"));
    vault.close().await;
}

#[tokio::test]
async fn enforces_write_folders_and_note_paths_before_touching_the_bucket() {
    let fx = Fixture::new();
    let vault = fx.open_with("", Some(&["MCP"]));
    vault.init().await.unwrap();
    let denied = vault.write_note("private.md", "x").await;
    assert!(matches!(denied, Err(VaultError::WriteDenied(_))), "got {denied:?}");
    let invalid = vault.write_note(".obsidian/x.md", "x").await;
    assert!(matches!(invalid, Err(VaultError::InvalidPath(_))), "got {invalid:?}");
    let invalid = vault.delete_note("../x.md").await;
    assert!(matches!(invalid, Err(VaultError::InvalidPath(_))), "got {invalid:?}");
    assert!(vault.write_note("MCP/ok.md", "x").await.unwrap());
    assert_eq!(fx.store.calls_to("put"), ["MCP/ok.md"]);
    vault.close().await;
}

#[tokio::test]
async fn local_paths_stay_inside_the_mirror_root() {
    let fx = Fixture::new();
    let mirror = S3Mirror::new(fx.vault_path(), fx.manifest_path(), "", fx.store.clone());
    assert_eq!(mirror.local_path("a/b.md").unwrap(), fx.vault_path().join("a/b.md"));
    assert!(matches!(mirror.local_path("../escape.md"), Err(VaultError::Traversal)));
    assert!(matches!(mirror.local_path("."), Err(VaultError::Traversal)));
    assert!(!mirror.local_exists("../escape.md").await);
}
