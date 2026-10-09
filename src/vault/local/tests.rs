use std::os::unix::fs::symlink;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;

use super::*;
use crate::search::SearchIndex;
use crate::server::watch_vault;

fn vault() -> (TempDir, LocalVault) {
    let dir = tempfile::tempdir().unwrap();
    let vault = LocalVault::new(dir.path(), None).unwrap();
    (dir, vault)
}

fn scoped(root: &Path, folders: &[&str]) -> LocalVault {
    LocalVault::new(root, Some(folders.iter().map(|f| f.to_string()).collect())).unwrap()
}

fn write(root: &Path, path: &str, content: &str) {
    let full = root.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, content).unwrap();
}

fn assert_denied<T: std::fmt::Debug>(result: Result<T, VaultError>) {
    assert!(matches!(result, Err(VaultError::WriteDenied(_))), "expected WriteDenied, got {result:?}");
}

fn assert_invalid<T: std::fmt::Debug>(result: Result<T, VaultError>) {
    assert!(matches!(result, Err(VaultError::InvalidPath(_))), "expected InvalidPath, got {result:?}");
}

fn assert_traversal<T: std::fmt::Debug>(result: Result<T, VaultError>) {
    assert!(matches!(result, Err(VaultError::Traversal)), "expected Traversal, got {result:?}");
}

/// A root with `MCP/source.md`, `private/note.md` and `MCP/alias` linking to `private`.
fn aliased_root() -> (TempDir, LocalVault) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "private/note.md", "protected");
    write(root, "MCP/source.md", "source");
    symlink(root.join("private"), root.join("MCP/alias")).unwrap();
    let vault = scoped(root, &["MCP"]);
    (dir, vault)
}

async fn assert_alias_untouched(vault: &LocalVault) {
    assert_eq!(vault.read_note("private/note.md").await.unwrap().as_deref(), Some("protected"));
    assert_eq!(vault.read_note("MCP/source.md").await.unwrap().as_deref(), Some("source"));
    assert_eq!(vault.read_note("private/new/deep.md").await.unwrap(), None);
}

#[tokio::test]
async fn blocks_write_through_a_directory_alias_to_a_protected_folder() {
    let (_dir, vault) = aliased_root();
    assert_denied(vault.write_note("MCP/alias/new/deep.md", "changed").await);
    assert_alias_untouched(&vault).await;
}

#[tokio::test]
async fn blocks_delete_through_a_directory_alias_to_a_protected_folder() {
    let (_dir, vault) = aliased_root();
    assert_denied(vault.delete_note("MCP/alias/note.md").await);
    assert_alias_untouched(&vault).await;
}

#[tokio::test]
async fn blocks_move_from_a_directory_alias_to_a_protected_folder() {
    let (_dir, vault) = aliased_root();
    assert_denied(vault.move_note("MCP/alias/note.md", "MCP/moved.md").await);
    assert_alias_untouched(&vault).await;
}

#[tokio::test]
async fn blocks_move_into_a_directory_alias_to_a_protected_folder() {
    let (_dir, vault) = aliased_root();
    assert_denied(vault.move_note("MCP/source.md", "MCP/alias/new/deep.md").await);
    assert_alias_untouched(&vault).await;
}

#[tokio::test]
async fn rejects_dangling_links_and_permits_links_within_writable_folders() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("MCP/notes")).unwrap();
    std::fs::create_dir(root.join("private")).unwrap();
    symlink(root.join("private/missing.md"), root.join("MCP/dangling.md")).unwrap();
    symlink(root.join("MCP/notes"), root.join("MCP/allowed")).unwrap();
    let vault = scoped(root, &["MCP"]);

    let dangling = vault.write_note("MCP/dangling.md", "changed").await;
    assert!(matches!(dangling, Err(VaultError::DanglingSymlink)), "got {dangling:?}");
    assert_eq!(vault.read_note("private/missing.md").await.unwrap(), None);
    assert!(vault.write_note("MCP/allowed/new/note.md", "allowed").await.unwrap());
    assert_eq!(vault.read_note("MCP/notes/new/note.md").await.unwrap().as_deref(), Some("allowed"));
    assert_denied(vault.write_note("private/new.md", "changed").await);
}

#[tokio::test]
async fn blocks_a_writable_folders_symlink_to_a_protected_note() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "private/note.md", "protected");
    std::fs::create_dir(root.join("MCP")).unwrap();
    symlink(root.join("private/note.md"), root.join("MCP/alias.md")).unwrap();
    let vault = scoped(root, &["MCP"]);
    assert_denied(vault.write_note("MCP/alias.md", "changed").await);
    assert_eq!(vault.read_note("private/note.md").await.unwrap().as_deref(), Some("protected"));
}

#[tokio::test]
async fn still_denies_symlink_escapes_out_of_a_write_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "other/note.md", "protected");
    std::fs::create_dir(root.join("w")).unwrap();
    symlink(root.join("other"), root.join("w/link")).unwrap();
    symlink(root.join("other/note.md"), root.join("w/link.md")).unwrap();
    let vault = scoped(root, &["w"]);
    assert_denied(vault.write_note("w/link.md", "changed").await);
    assert_denied(vault.write_note("w/link/new.md", "changed").await);
    assert_eq!(vault.read_note("other/note.md").await.unwrap().as_deref(), Some("protected"));
}

#[tokio::test]
async fn does_not_widen_scope_when_the_write_folder_itself_is_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "private/note.md", "protected");
    symlink(root.join("private"), root.join("MCP")).unwrap();
    let vault = scoped(root, &["MCP"]);
    assert_denied(vault.write_note("private/note.md", "changed").await);
    assert_eq!(vault.read_note("private/note.md").await.unwrap().as_deref(), Some("protected"));
    assert_eq!(
        vault.canonical_write_folders().await,
        Some(vec!["MCP".to_owned()]),
        "a symlinked write folder keeps its configured name"
    );
}

// Check folder canonicalization directly without requiring a case-insensitive filesystem.
#[tokio::test]
async fn canonical_write_folders_use_the_on_disk_spelling() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("Inbox/Sub")).unwrap();
    let scoped_vault = scoped(dir.path(), &["Inbox/Sub", "missing", "Inbox"]);
    assert_eq!(
        scoped_vault.canonical_write_folders().await,
        Some(vec!["Inbox/Sub".to_owned(), "missing".to_owned(), "Inbox".to_owned()])
    );
    let (_unscoped_dir, unscoped) = vault();
    assert_eq!(unscoped.canonical_write_folders().await, None);
}

#[tokio::test]
async fn writes_inside_a_scoped_folder() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "MCP/existing.md", "old");
    let vault = scoped(dir.path(), &["MCP"]);
    assert!(vault.write_note("MCP/new.md", "new").await.unwrap());
    assert!(vault.write_note("MCP/existing.md", "changed").await.unwrap());
    assert_eq!(vault.read_note("MCP/existing.md").await.unwrap().as_deref(), Some("changed"));
    assert!(vault.move_note("MCP/new.md", "MCP/sub/moved.md").await.unwrap());
    assert!(vault.delete_note("MCP/sub/moved.md").await.unwrap());
    assert_denied(vault.write_note("mcp/new.md", "x").await);
}

#[tokio::test]
async fn blocks_parent_traversal() {
    let (_dir, vault) = vault();
    assert_invalid(vault.read_note("../etc/passwd").await);
    assert_invalid(vault.read_note("../../etc/shadow").await);
    assert_invalid(vault.write_note("../evil.md", "pwned").await);
    assert_invalid(vault.delete_note("../../important.md").await);
    assert_traversal(vault.list_notes(Some("../../etc")).await);
}

#[tokio::test]
async fn allows_nested_paths_within_the_vault() {
    let (_dir, vault) = vault();
    assert!(vault.write_note("sub/dir/note.md", "ok").await.unwrap());
    assert_eq!(vault.read_note("sub/dir/note.md").await.unwrap().as_deref(), Some("ok"));
}

#[tokio::test]
async fn rejects_paths_that_are_not_notes() {
    let (_dir, vault) = vault();
    assert_invalid(vault.write_note(".obsidian/plugins/evil/main.js", "pwned").await);
    assert_invalid(vault.read_note(".obsidian/plugins/remotely-save/data.json").await);
    assert_invalid(vault.write_note("notes/data.json", "{}").await);
}

#[tokio::test]
async fn blocks_a_write_that_escapes_via_a_symlinked_directory() {
    let (dir, vault) = vault();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), dir.path().join("link")).unwrap();
    // The target file does not exist yet, so only resolving the parent catches the escape.
    assert_traversal(vault.write_note("link/escaped.md", "pwned").await);
    assert!(!outside.path().join("escaped.md").exists(), "nothing was written outside the vault");
}

#[tokio::test]
async fn reads_and_writes_notes() {
    let (_dir, vault) = vault();
    assert_eq!(vault.read_note("nope.md").await.unwrap(), None);
    assert!(vault.write_note("hello.md", "# Hello").await.unwrap());
    assert_eq!(vault.read_note("hello.md").await.unwrap().as_deref(), Some("# Hello"));
    assert!(vault.write_note("a/b/c/deep.md", "deep").await.unwrap());
    assert_eq!(vault.read_note("a/b/c/deep.md").await.unwrap().as_deref(), Some("deep"));
    vault.write_note("overwrite.md", "v1").await.unwrap();
    vault.write_note("overwrite.md", "v2").await.unwrap();
    assert_eq!(vault.read_note("overwrite.md").await.unwrap().as_deref(), Some("v2"));
    let unicode = "# 日本語テスト\n\nEmoji: 🎉";
    vault.write_note("unicode.md", unicode).await.unwrap();
    assert_eq!(vault.read_note("unicode.md").await.unwrap().as_deref(), Some(unicode));
}

#[tokio::test]
async fn moves_notes() {
    let (_dir, vault) = vault();
    vault.write_note("move/src.md", "content").await.unwrap();
    assert!(vault.move_note("move/src.md", "move/dest.md").await.unwrap());
    assert_eq!(vault.read_note("move/src.md").await.unwrap(), None);
    assert_eq!(vault.read_note("move/dest.md").await.unwrap().as_deref(), Some("content"));

    vault.write_note("folder-a/note.md", "hello").await.unwrap();
    assert!(vault.move_note("folder-a/note.md", "folder-b/note.md").await.unwrap());
    assert_eq!(vault.read_note("folder-b/note.md").await.unwrap().as_deref(), Some("hello"));

    assert!(!vault.move_note("nope.md", "dest.md").await.unwrap(), "a missing source is not moved");
}

#[tokio::test]
async fn refuses_to_overwrite_an_existing_destination() {
    let (_dir, vault) = vault();
    vault.write_note("move/keep-src.md", "source").await.unwrap();
    vault.write_note("move/keep-dest.md", "destination").await.unwrap();
    let result = vault.move_note("move/keep-src.md", "move/keep-dest.md").await;
    match result {
        Err(error @ VaultError::DestinationExists(_)) => {
            assert_eq!(error.to_string(), "Destination already exists: move/keep-dest.md");
        }
        other => panic!("expected DestinationExists, got {other:?}"),
    }
    assert_eq!(vault.read_note("move/keep-src.md").await.unwrap().as_deref(), Some("source"));
    assert_eq!(vault.read_note("move/keep-dest.md").await.unwrap().as_deref(), Some("destination"));
}

#[tokio::test]
async fn returns_false_for_a_missing_source_even_when_the_destination_exists() {
    let (_dir, vault) = vault();
    vault.write_note("move/present.md", "present").await.unwrap();
    assert!(!vault.move_note("move/absent.md", "move/present.md").await.unwrap());
    assert_eq!(vault.read_note("move/present.md").await.unwrap().as_deref(), Some("present"));
}

#[tokio::test]
async fn treats_a_move_onto_the_identical_path_as_a_no_op() {
    let (_dir, vault) = vault();
    vault.write_note("move/same.md", "same").await.unwrap();
    assert!(vault.move_note("move/same.md", "move/same.md").await.unwrap());
    assert_eq!(vault.read_note("move/same.md").await.unwrap().as_deref(), Some("same"));
}

#[tokio::test]
async fn case_only_rename_on_a_case_sensitive_filesystem_is_a_plain_move() {
    let (dir, vault) = vault();
    vault.write_note("Note.md", "keep").await.unwrap();
    assert!(vault.move_note("Note.md", "note.md").await.unwrap());
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, ["note.md"]);
    assert_eq!(vault.read_note("note.md").await.unwrap().as_deref(), Some("keep"));
}

#[tokio::test]
async fn returns_metadata_with_frontmatter_tags_and_links() {
    let (_dir, vault) = vault();
    let content = "---\ntitle: Test\ntags: [foo, bar]\n---\n\n# Hello #inline-tag\n\nSee [[Other Note]]\n";
    vault.write_note("meta/test.md", content).await.unwrap();
    let meta = vault.get_metadata("meta/test.md").await.unwrap().expect("metadata for an existing note");
    assert_eq!(meta.path, "meta/test.md");
    assert_eq!(meta.size, content.len() as u64);
    assert!(meta.ctime > 0.0 && meta.mtime > 0.0, "times are set: {meta:?}");
    assert_eq!(meta.metadata.frontmatter.get("title"), Some(&serde_json::json!("Test")));
    assert_eq!(meta.metadata.tags, ["foo", "bar", "inline-tag"]);
    assert_eq!(meta.metadata.links, ["Other Note"]);
    assert_eq!(vault.get_metadata("nope.md").await.unwrap(), None);
}

#[tokio::test]
async fn deletes_notes() {
    let (_dir, vault) = vault();
    vault.write_note("del.md", "x").await.unwrap();
    assert!(vault.delete_note("del.md").await.unwrap());
    assert_eq!(vault.read_note("del.md").await.unwrap(), None);
    assert!(!vault.delete_note("nope.md").await.unwrap());
}

async fn listing_vault() -> (TempDir, LocalVault) {
    let (dir, vault) = vault();
    vault.write_note("list/b.md", "b").await.unwrap();
    vault.write_note("list/a.md", "a").await.unwrap();
    vault.write_note("list/sub/c.md", "c").await.unwrap();
    vault.write_note("list/B2.md", "b2").await.unwrap();
    write(dir.path(), "list/ignore.txt", "not a note");
    write(dir.path(), "list/.hidden/x.md", "hidden");
    write(dir.path(), "top.md", "top");
    (dir, vault)
}

#[tokio::test]
async fn lists_notes_recursively_and_sorted_by_name() {
    let (_dir, vault) = listing_vault().await;
    assert_eq!(
        vault.list_notes(Some("list/")).await.unwrap(),
        ["list/a.md", "list/b.md", "list/B2.md", "list/sub/c.md"]
    );
    assert_eq!(vault.list_notes(Some("list")).await.unwrap(), vault.list_notes(Some("list/")).await.unwrap());
    assert_eq!(vault.list_notes(Some("list/sub/")).await.unwrap(), ["list/sub/c.md"]);
    assert_eq!(vault.list_notes(None).await.unwrap().len(), 5);
    assert_eq!(vault.list_notes(Some("nonexistent/")).await.unwrap(), Vec::<String>::new());
    assert_eq!(vault.list_notes(Some("top.md")).await.unwrap(), Vec::<String>::new(), "a file is not a folder");
}

#[tokio::test]
async fn lists_mtimes_of_each_note() {
    let (dir, vault) = listing_vault().await;
    let expected = std::fs::metadata(dir.path().join("list/sub/c.md")).unwrap().modified().unwrap();
    let notes = vault.list_notes_with_mtime(Some("list/sub")).await.unwrap();
    assert_eq!(notes, [NoteListing { path: "list/sub/c.md".to_owned(), mtime: system_time_ms(expected) }]);
}

#[tokio::test]
async fn fails_instead_of_listing_nothing_when_the_vault_cannot_be_read() {
    let (dir, vault) = vault();
    let root = dir.path().to_path_buf();
    drop(dir);
    let result = vault.list_notes_with_mtime(None).await;
    match result {
        Err(error @ VaultError::List { .. }) => assert_eq!(error.to_string(), "Failed to list notes"),
        other => panic!("expected a List error, got {other:?}"),
    }
    assert!(!root.exists());
}

#[tokio::test]
async fn fails_to_list_an_unreadable_folder() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, vault) = vault();
    write(dir.path(), "locked/a.md", "a");
    let locked = dir.path().join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Root ignores directory permissions, so only check when the lock holds.
    let effective = std::fs::read_dir(&locked).is_err();
    let result = vault.list_notes_with_mtime(Some("locked")).await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if effective {
        match result {
            Err(error @ VaultError::List { .. }) => {
                assert_eq!(error.to_string(), "Failed to list notes in 'locked/'");
            }
            other => panic!("expected a List error, got {other:?}"),
        }
    }
}

#[test]
fn resolves_paths_lexically() {
    let base = Path::new("/vault");
    assert_eq!(lexical_resolve(base, "a/./b/../c.md"), Path::new("/vault/a/c.md"));
    assert_eq!(lexical_resolve(base, "../x.md"), Path::new("/x.md"));
    assert_eq!(lexical_resolve(base, "/etc/passwd"), Path::new("/etc/passwd"));
    assert!(is_inside(Path::new("/vault/a.md"), base));
    assert!(!is_inside(base, base));
    assert!(!is_inside(Path::new("/vault2/a.md"), base));
}

async fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..300 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting until {what}");
}

/// A watcher on a fresh vault, given time to settle before the test writes.
async fn watched() -> (TempDir, Arc<SearchIndex>, crate::server::VaultWatcher) {
    let dir = tempfile::tempdir().unwrap();
    let vault: Arc<dyn VaultBackend> = Arc::new(LocalVault::new(dir.path(), None).unwrap());
    let index = Arc::new(SearchIndex::in_memory());
    let watcher = watch_vault(vault, index.clone(), dir.path());
    tokio::time::sleep(Duration::from_millis(100)).await;
    (dir, index, watcher)
}

#[tokio::test]
async fn watcher_indexes_new_notes_and_drops_deleted_ones() {
    let (dir, index, _watcher) = watched().await;
    std::fs::write(dir.path().join("external.md"), "external edit #banana").unwrap();
    wait_for("external.md is indexed", || index.has("external.md")).await;
    assert_eq!(index.get_tags("external.md"), ["banana"]);
    let on_disk = std::fs::metadata(dir.path().join("external.md")).unwrap().modified().unwrap();
    assert_eq!(index.get_mtime("external.md"), system_time_ms(on_disk), "the index records the file's mtime");

    std::fs::remove_file(dir.path().join("external.md")).unwrap();
    wait_for("external.md is removed", || !index.has("external.md")).await;
}

#[tokio::test]
async fn watcher_ignores_files_that_are_not_notes() {
    let (dir, index, _watcher) = watched().await;
    std::fs::write(dir.path().join("image.png"), "not a note").unwrap();
    write(dir.path(), ".obsidian/workspace.md", "config");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(index.size(), 0);
}

#[tokio::test]
async fn watcher_indexes_notes_in_new_subfolders() {
    let (dir, index, _watcher) = watched().await;
    std::fs::create_dir_all(dir.path().join("sub/dir")).unwrap();
    // inotify watches a new folder only once its creation event is handled.
    tokio::time::sleep(Duration::from_millis(200)).await;
    std::fs::write(dir.path().join("sub/dir/deep.md"), "deep nested content").unwrap();
    wait_for("sub/dir/deep.md is indexed", || index.has("sub/dir/deep.md")).await;
}
