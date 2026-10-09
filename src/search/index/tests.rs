use std::path::Path;

use tempfile::TempDir;

use super::*;

fn persisted(dir: &TempDir, name: &str, passphrase: Option<&str>) -> SearchIndex {
    SearchIndex::new(Some(dir.path().join(name)), passphrase.map(str::to_owned), SearchIndex::DEFAULT_MAX_CONTENT_CHARS)
}

fn content(index: &SearchIndex, path: &str) -> Option<String> {
    index.get_content(path).map(|c| c.to_string())
}

#[test]
fn tracks_size() {
    let index = SearchIndex::in_memory();
    assert_eq!(index.size(), 0);
    index.update("a.md", "content", None);
    assert_eq!(index.size(), 1);
    index.update("b.md", "content", None);
    assert_eq!(index.size(), 2);
    index.update("b.md", "changed", None);
    assert_eq!(index.size(), 2, "updating a note does not add it twice");
    index.remove("a.md");
    assert_eq!(index.size(), 1);
    assert!(!index.has("a.md") && index.has("b.md"));
}

#[test]
fn starts_building_and_reports_its_state() {
    let index = SearchIndex::in_memory();
    assert_eq!(index.state(), IndexState::Building);
    index.set_state(IndexState::Ready);
    assert_eq!(index.state(), IndexState::Ready);
    index.set_state(IndexState::Failed);
    assert_eq!(index.state(), IndexState::Failed);
}

#[test]
fn stores_and_retrieves_mtimes() {
    let index = SearchIndex::in_memory();
    index.update("note.md", "content", Some(1_234_567_890.5));
    assert_eq!(index.list_with_mtime(None), [NoteListing { path: "note.md".into(), mtime: 1_234_567_890.5 }]);
    assert_eq!(index.get_mtime("note.md"), 1_234_567_890.5);
    index.update("note.md", "changed", None);
    assert_eq!(index.get_mtime("note.md"), 1_234_567_890.5, "an update without an mtime keeps the old one");
    assert_eq!(index.get_mtime("missing.md"), 0.0);
}

#[test]
fn excludes_non_note_paths_from_listings() {
    let index = SearchIndex::in_memory();
    index.update("good.md", "x", Some(1.0));
    index.update(".obsidian/plugins/x.md", "x", Some(2.0));
    index.update("notes/.hidden.md", "x", Some(3.0));
    index.update("a:b.md", "x", Some(4.0));
    assert_eq!(index.list_paths(None), ["good.md"]);
}

#[test]
fn lists_paths_by_folder_in_name_order() {
    let index = SearchIndex::in_memory();
    for path in ["b/z.md", "B/y.md", "b/a.md", "bc/x.md", "top.md"] {
        index.update(path, "x", None);
    }
    assert_eq!(index.list_paths(Some("b")), ["b/a.md", "b/z.md"]);
    assert_eq!(index.list_paths(Some("b/")), ["b/a.md", "b/z.md"]);
    assert_eq!(index.list_paths(Some("")), index.list_paths(None));
    assert_eq!(index.list_paths(None), ["b/a.md", "B/y.md", "b/z.md", "bc/x.md", "top.md"]);
}

#[test]
fn extracts_tags_from_frontmatter_and_inline() {
    let index = SearchIndex::in_memory();
    index.update("tagged.md", "---\ntags: [project, urgent]\n---\n\nSome #inline content", Some(100.0));
    assert_eq!(index.get_tags("tagged.md"), ["project", "urgent", "inline"]);
    assert_eq!(index.get_tags("missing.md"), Vec::<String>::new());
}

#[test]
fn lists_all_tags_by_count_then_name() {
    let index = SearchIndex::in_memory();
    index.update("a.md", "---\ntags: [project, urgent]\n---\n", Some(100.0));
    index.update("b.md", "---\ntags: [project, alpha]\n---\n", Some(200.0));
    index.update("c.md", "No tags here", Some(300.0));
    let tag = |tag: &str, count| TagCount { tag: tag.into(), count };
    assert_eq!(index.list_all_tags(), [tag("project", 2), tag("alpha", 1), tag("urgent", 1)]);
}

#[test]
fn clears_and_replaces_tags() {
    let index = SearchIndex::in_memory();
    index.update("note.md", "---\ntags: [old]\n---\n", Some(100.0));
    assert_eq!(index.get_tags("note.md"), ["old"]);
    index.update("note.md", "---\ntags: [new]\n---\n", Some(200.0));
    assert_eq!(index.get_tags("note.md"), ["new"]);
    index.update("note.md", "no tags now", Some(300.0));
    assert_eq!(index.get_tags("note.md"), Vec::<String>::new());
    index.update("note.md", "#back", Some(400.0));
    index.remove("note.md");
    assert_eq!(index.get_tags("note.md"), Vec::<String>::new());
    assert_eq!(index.list_all_tags(), []);
}

#[test]
fn extracts_outgoing_links() {
    let index = SearchIndex::in_memory();
    index.update("a.md", "See [[b]] and [[folder/c]]", Some(100.0));
    assert_eq!(index.get_links("a.md"), ["b", "folder/c"]);
}

#[test]
fn builds_backlinks_from_wikilinks() {
    let index = SearchIndex::in_memory();
    index.update("c.md", "Also links to [[b]]", Some(200.0));
    index.update("a.md", "Links to [[b]]", Some(100.0));
    assert_eq!(index.get_backlinks("b.md"), ["a.md", "c.md"]);
    assert_eq!(index.get_backlinks("b"), ["a.md", "c.md"], "with or without the extension");
}

#[test]
fn matches_backlinks_by_file_name_full_path_and_case_insensitively() {
    let index = SearchIndex::in_memory();
    index.update("a.md", "Links to [[Project X]]", Some(100.0));
    index.update("b.md", "Links to [[projects/todo]]", Some(100.0));
    index.update("c.md", "Links to [[welcome]]", Some(100.0));
    index.update("d.md", "Links to [[elsewhere/todo.md]]", Some(100.0));
    assert_eq!(index.get_backlinks("Project X.md"), ["a.md"]);
    assert_eq!(index.get_backlinks("projects/todo.md"), ["b.md"]);
    assert_eq!(index.get_backlinks("Welcome.md"), ["c.md"]);
    assert_eq!(index.get_backlinks("Inbox/Welcome.md"), ["c.md"], "a bare name links to any folder");
    assert_eq!(index.get_backlinks("nothing.md"), Vec::<String>::new());
}

#[test]
fn clears_backlinks_when_the_source_is_removed_or_changes() {
    let index = SearchIndex::in_memory();
    index.update("a.md", "Links to [[b]]", Some(100.0));
    index.update("x.md", "Links to [[b]]", Some(100.0));
    index.remove("a.md");
    assert_eq!(index.get_backlinks("b.md"), ["x.md"]);
    index.update("x.md", "Now links to [[c]]", Some(200.0));
    assert_eq!(index.get_backlinks("b.md"), Vec::<String>::new());
    assert_eq!(index.get_backlinks("c.md"), ["x.md"]);
    index.update("x.md", "no links", Some(300.0));
    assert_eq!(index.get_backlinks("c.md"), Vec::<String>::new());
    assert_eq!(index.get_links("x.md"), Vec::<String>::new());
}

#[test]
fn applies_vault_change_notifications() {
    let index = SearchIndex::in_memory();
    let listener: &dyn VaultChangeListener = &index;
    listener.updated("a.md", "#tag", 42.0);
    assert_eq!(index.get_mtime("a.md"), 42.0);
    assert_eq!(index.get_tags("a.md"), ["tag"]);
    listener.removed("a.md");
    assert!(!index.has("a.md"));
}

#[test]
fn keeps_content_for_scans_and_drops_it_on_remove_or_overwrite() {
    let index = SearchIndex::in_memory();
    index.update("a.md", "alpha", Some(1.0));
    index.update("b.md", "beta", Some(1.0));
    assert_eq!(content(&index, "a.md").as_deref(), Some("alpha"));
    assert_eq!(index.content_size(), 9);
    index.update("a.md", "alphabet", Some(2.0));
    assert_eq!(content(&index, "a.md").as_deref(), Some("alphabet"));
    assert_eq!(index.content_size(), 12);
    index.remove("a.md");
    assert_eq!(content(&index, "a.md"), None);
    assert_eq!(index.content_size(), 4);
}

#[test]
fn counts_cached_content_in_characters() {
    let index = SearchIndex::in_memory();
    index.update("a.md", "日本語", None);
    assert_eq!(index.content_size(), 3);
}

#[test]
fn leaves_notes_out_beyond_the_cap_but_still_indexes_their_metadata() {
    let index = SearchIndex::new(None, None, 10);
    index.update("a.md", "123456", Some(1.0));
    index.update("b.md", "#tag 7890123", Some(1.0));
    assert_eq!(content(&index, "a.md").as_deref(), Some("123456"));
    assert_eq!(content(&index, "b.md"), None, "over the cap: not cached");
    assert_eq!(index.get_tags("b.md"), ["tag"], "metadata still indexed");
    assert_eq!(index.content_size(), 6);
    // Shrinking a note makes room again; growing one past the cap evicts its old copy.
    index.update("b.md", "ok", Some(2.0));
    assert_eq!(content(&index, "b.md").as_deref(), Some("ok"));
    index.update("a.md", &"x".repeat(20), Some(3.0));
    assert_eq!(content(&index, "a.md"), None);
    assert_eq!(index.content_size(), 2);

    let off = SearchIndex::new(None, None, 0);
    off.update("a.md", "a", Some(1.0));
    assert_eq!(content(&off, "a.md"), None, "a cap of zero disables caching");
}

#[tokio::test]
async fn does_not_persist_content_which_refills_from_the_vault() {
    let dir = tempfile::tempdir().unwrap();
    let index = persisted(&dir, "content-cache.json", None);
    index.update("a.md", "#t body", Some(1.0));
    index.save_to_disk().await;

    let loaded = persisted(&dir, "content-cache.json", None);
    assert!(loaded.load_from_disk().await);
    assert_eq!(loaded.get_tags("a.md"), ["t"]);
    assert_eq!(content(&loaded, "a.md"), None);
    loaded.cache_content("a.md", Arc::from("#t body"));
    assert_eq!(content(&loaded, "a.md").as_deref(), Some("#t body"));
}

#[tokio::test]
async fn saves_and_loads_mtimes_tags_and_backlinks() {
    let dir = tempfile::tempdir().unwrap();
    let first = persisted(&dir, "index.json", None);
    first.update("note1.md", "---\ntags: [foo]\n---\nHello world", Some(100.0));
    first.update("note2.md", "Goodbye world, see [[note1]]", Some(200.5));
    first.save_to_disk().await;

    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("index.json")).unwrap()).unwrap();
    assert_eq!(raw["version"], INDEX_SCHEMA_VERSION);
    assert_eq!(raw["mtimes"]["note2.md"], 200.5);
    assert_eq!(raw["tags"]["note1.md"], serde_json::json!(["foo"]));
    assert_eq!(raw["links"]["note2.md"], serde_json::json!(["note1"]));

    let second = persisted(&dir, "index.json", None);
    assert!(second.load_from_disk().await);
    assert_eq!(second.size(), 2);
    assert_eq!(
        second.list_with_mtime(None),
        [NoteListing { path: "note1.md".into(), mtime: 100.0 }, NoteListing { path: "note2.md".into(), mtime: 200.5 }]
    );
    assert_eq!(second.get_tags("note1.md"), ["foo"]);
    assert_eq!(second.get_tags("note2.md"), Vec::<String>::new());
    assert_eq!(second.get_backlinks("note1.md"), ["note2.md"]);
}

#[tokio::test]
async fn saves_and_loads_encrypted_with_a_passphrase() {
    let dir = tempfile::tempdir().unwrap();
    let first = persisted(&dir, "encrypted-index.json", Some("mypassphrase"));
    first.update("secret.md", "classified content", Some(999.0));
    first.save_to_disk().await;

    let raw = std::fs::read_to_string(dir.path().join("encrypted-index.json")).unwrap();
    assert!(!raw.contains("secret.md") && !raw.contains("classified"), "the file is not plaintext: {raw}");
    let parts: Vec<_> = raw.split(':').collect();
    assert_eq!(parts.len(), 4, "salt:iv:tag:ciphertext");
    assert_eq!([parts[0].len(), parts[1].len(), parts[2].len()], [32, 24, 32]);
    assert!(parts.iter().all(|p| hex::decode(p).is_ok()));

    let second = persisted(&dir, "encrypted-index.json", Some("mypassphrase"));
    assert!(second.load_from_disk().await);
    assert_eq!(second.size(), 1);
    assert_eq!(second.get_mtime("secret.md"), 999.0);
}

#[tokio::test]
async fn falls_back_to_a_rebuild_with_the_wrong_passphrase_or_none() {
    let dir = tempfile::tempdir().unwrap();
    let first = persisted(&dir, "index.json", Some("right"));
    first.update("a.md", "a", Some(1.0));
    first.save_to_disk().await;

    let wrong = persisted(&dir, "index.json", Some("wrong"));
    assert!(!wrong.load_from_disk().await);
    assert_eq!(wrong.size(), 0);
    let missing = persisted(&dir, "index.json", None);
    assert!(!missing.load_from_disk().await, "ciphertext is not JSON");
    assert_eq!(missing.size(), 0);
}

#[tokio::test]
async fn ignores_an_unencrypted_index_once_a_passphrase_is_set() {
    let dir = tempfile::tempdir().unwrap();
    let plain = persisted(&dir, "index.json", None);
    plain.update("a.md", "a", Some(1.0));
    plain.save_to_disk().await;
    let encrypted = persisted(&dir, "index.json", Some("secret"));
    assert!(!encrypted.load_from_disk().await);
}

#[test]
fn encryption_round_trips_and_detects_tampering() {
    let sealed = encrypt("{\"version\":4}", "pass");
    assert_eq!(decrypt(&sealed, "pass").as_deref(), Some("{\"version\":4}"));
    assert_eq!(decrypt(&format!("{sealed}\n"), "pass").as_deref(), Some("{\"version\":4}"), "trailing newline");
    assert_ne!(encrypt("same", "pass"), encrypt("same", "pass"), "fresh salt and IV each time");

    let mut tampered = sealed.clone();
    let last = tampered.pop().unwrap();
    tampered.push(if last == '0' { '1' } else { '0' });
    assert_eq!(decrypt(&tampered, "pass"), None);
    assert_eq!(decrypt("not:hex:at:all", "pass"), None);
    assert_eq!(decrypt("", "pass"), None);
}

/// Known answers from Node (`scryptSync("pass", salt, 32)` and `aes-256-gcm`),
/// so files written by the TypeScript server still decrypt.
#[test]
fn decrypts_what_node_encrypted() {
    let salt = [7u8; 16];
    assert_eq!(
        hex::encode(derive_key("pass", &salt).unwrap()),
        "f78d1095f8f34d688222cf2c172893b076c8dc6caa916ea7e9eb018dc546d6fc"
    );
    let sealed = "07070707070707070707070707070707:090909090909090909090909:cfaf1ae15a613952b98ac4b3ba5479fe:\
                  0cd6a186bb72e40d17a565ef6c2fc7ca3cf3f7e3e10df4d8c03382ad0fca766fdc4892";
    assert_eq!(decrypt(sealed, "pass").as_deref(), Some(r#"{"version":4,"mtimes":{"a.md":1.5}}"#));
}

async fn assert_rejected(dir: &TempDir, json: &str) {
    std::fs::write(dir.path().join("index.json"), json).unwrap();
    let index = persisted(dir, "index.json", None);
    assert!(!index.load_from_disk().await, "loaded {json}");
    assert_eq!(index.size(), 0, "nothing was loaded from {json}");
}

#[tokio::test]
async fn rejects_persisted_indexes_that_fail_validation_or_are_from_an_older_version() {
    let dir = tempfile::tempdir().unwrap();
    let v = INDEX_SCHEMA_VERSION;
    assert_rejected(&dir, &format!(r#"{{"version":{v},"mtimes":{{"a.md":"not a number"}}}}"#)).await;
    assert_rejected(&dir, &format!(r#"{{"version":{v},"tags":{{"a.md":"not a list"}}}}"#)).await;
    assert_rejected(&dir, r#"{"version":3,"mtimes":{"a.md":1},"tags":{},"links":{}}"#).await;
    assert_rejected(&dir, r#"{"mtimes":{"a.md":1}}"#).await;
    assert_rejected(&dir, "null").await;
    assert_rejected(&dir, "{truncated").await;
    // A current file with nothing in it loads but reports no notes.
    assert_rejected(&dir, &format!(r#"{{"version":{v}}}"#)).await;
}

#[tokio::test]
async fn returns_false_without_a_persisted_index_or_a_persist_path() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!persisted(&dir, "nonexistent.json", None).load_from_disk().await);
    let in_memory = SearchIndex::in_memory();
    assert!(!in_memory.load_from_disk().await);
    in_memory.update("a.md", "a", None);
    in_memory.save_to_disk().await;
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_saves_persist_the_latest_state_in_a_private_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(persisted(&dir, "index.json", None));
    let save = |index: &Arc<SearchIndex>| {
        let index = index.clone();
        tokio::spawn(async move { index.save_to_disk().await })
    };
    index.update("first.md", "one", Some(1.0));
    let first = save(&index);
    index.update("second.md", "two", Some(2.0));
    let second = save(&index);
    index.update("third.md", "three", Some(3.0));
    let third = save(&index);
    for task in [first, second, third] {
        task.await.unwrap();
    }

    let loaded = persisted(&dir, "index.json", None);
    assert!(loaded.load_from_disk().await);
    assert_eq!(loaded.list_paths(None), ["first.md", "second.md", "third.md"]);
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, ["index.json"], "no temporary files are left behind");
    let mode = std::fs::metadata(dir.path().join("index.json")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn creates_the_persist_folder_and_survives_an_unwritable_path() {
    let dir = tempfile::tempdir().unwrap();
    let nested = SearchIndex::new(Some(dir.path().join("a/b/index.json")), None, 10);
    nested.update("a.md", "a", Some(1.0));
    nested.save_to_disk().await;
    assert!(dir.path().join("a/b/index.json").exists());

    // A directory in the way makes the rename fail; the save logs instead of panicking.
    std::fs::create_dir(dir.path().join("blocked.json")).unwrap();
    std::fs::write(dir.path().join("blocked.json/keep"), "").unwrap();
    let blocked = SearchIndex::new(Some(dir.path().join("blocked.json")), None, 10);
    blocked.update("a.md", "a", Some(1.0));
    blocked.save_to_disk().await;
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert_eq!(leftovers, Vec::<String>::new(), "the temporary file is cleaned up");
    assert!(Path::new(&dir.path().join("blocked.json/keep")).exists());
}
