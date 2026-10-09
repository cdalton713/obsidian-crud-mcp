//! Wording for `list_notes` responses: a truncated, filtered or still-indexing
//! listing must say so on the first line, and a zero-hit filter must not claim
//! the vault is empty.

use super::*;

const READY: IndexStatus = IndexStatus { state: IndexState::Ready, size: 206, served_by_vault: false };

fn index(state: IndexState, size: usize) -> IndexStatus {
    IndexStatus { state, size, served_by_vault: false }
}

fn paths(folder: &str, n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{folder}/note-{i}.md")).collect()
}

fn scope<'a>(vault_total: usize, filters: &[&str], index: IndexStatus) -> ListingScope<'a> {
    ListingScope {
        vault_total: Some(vault_total),
        folder: None,
        folder_total: None,
        filters: filters.iter().map(|f| f.to_string()).collect(),
        index,
    }
}

fn page(shown: usize, matched: usize) -> ListingPage<'static> {
    ListingPage { shown, matched, sort_by: "name", limit: 100, omitted: &[] }
}

#[test]
fn counts_by_immediate_parent_largest_first() {
    let mut all = paths("a/b", 2);
    all.extend(paths("a", 3));
    all.push("top.md".to_owned());
    let groups = count_by_folder(&all);
    let expected =
        [("a", 3), ("a/b", 2), ("(root)", 1)].map(|(folder, count)| FolderCount { folder: folder.to_owned(), count });
    assert_eq!(groups, expected);
}

#[test]
fn equal_folder_counts_sort_by_name() {
    let mut all = paths("b", 1);
    all.extend(paths("A", 1));
    all.extend(paths("a", 1));
    let names: Vec<String> = count_by_folder(&all).into_iter().map(|g| g.folder).collect();
    assert_eq!(names, ["a", "A", "b"]);
}

#[test]
fn omitted_summary_names_folders_with_counts() {
    let mut omitted = paths("folder-a", 41);
    omitted.extend(paths("folder-b", 9));
    omitted.push("loose.md".to_owned());
    assert_eq!(describe_omitted(&omitted), "Omitted 51: folder-a/ (41), folder-b/ (9), (root) (1).");
}

#[test]
fn omitted_summary_caps_named_folders_and_keeps_the_arithmetic() {
    let omitted: Vec<String> = (0..MAX_NAMED_FOLDERS + 3).flat_map(|i| paths(&format!("f{i:02}"), 2)).collect();
    let text = describe_omitted(&omitted);
    assert!(text.starts_with(&format!("Omitted {}: ", omitted.len())), "{text}");
    assert_eq!(text.matches("(2)").count(), MAX_NAMED_FOLDERS, "{text}");
    assert!(text.ends_with(", and 3 more folders (6 notes)."), "{text}");
}

#[test]
fn index_state_is_silent_when_ready() {
    assert_eq!(describe_index_state(&READY), "");
}

#[test]
fn index_state_flags_a_partial_index_while_building() {
    assert_eq!(
        describe_index_state(&index(IndexState::Building, 57)),
        " Index: catching up (57 notes indexed so far); this list may be incomplete."
    );
}

#[test]
fn index_state_does_not_call_a_vault_served_list_incomplete() {
    let status = IndexStatus { state: IndexState::Building, size: 0, served_by_vault: true };
    assert_eq!(
        describe_index_state(&status),
        " Index: catching up (0 notes indexed so far); this list was read directly from the vault."
    );
}

#[test]
fn index_state_flags_a_failed_rebuild() {
    let text = describe_index_state(&index(IndexState::Failed, 0));
    assert!(text.contains("rebuild failed"), "{text}");
    assert!(!text.contains("catching up"), "{text}");
}

#[test]
fn vault_is_empty_only_without_filters() {
    assert_eq!(describe_no_match(&scope(0, &[], index(IndexState::Ready, 0))), "Vault is empty.");
}

#[test]
fn zero_hit_filter_names_the_filter_and_vault_size() {
    assert_eq!(
        describe_no_match(&scope(206, &["name=\"something-misspelled\""], READY)),
        "No notes match name=\"something-misspelled\" (vault has 206 notes)."
    );
}

#[test]
fn zero_hit_filter_is_scoped_to_the_folder() {
    let s = ListingScope { folder: Some("y"), folder_total: Some(41), ..scope(206, &["tag=\"x\""], READY) };
    assert_eq!(describe_no_match(&s), "No notes match tag=\"x\" in folder \"y/\" (folder has 41 notes).");
}

#[test]
fn empty_folder_keeps_the_plain_wording() {
    let s = ListingScope { folder: Some("y"), folder_total: Some(0), ..scope(206, &[], READY) };
    assert_eq!(describe_no_match(&s), "No notes found in folder: y");
}

#[test]
fn empty_answer_carries_the_index_state_while_building() {
    let text = describe_no_match(&scope(0, &[], index(IndexState::Building, 0)));
    assert!(text.starts_with("Vault is empty."), "{text}");
    assert!(text.contains("catching up"), "{text}");
}

#[test]
fn complete_unfiltered_listing_states_the_total() {
    assert_eq!(describe_listing(&scope(206, &[], READY), &page(206, 206)), "206 notes (sorted by name).");
    assert_eq!(describe_listing(&scope(1, &[], READY), &page(1, 1)), "1 note (sorted by name).");
}

#[test]
fn one_match_uses_the_singular_verb() {
    assert_eq!(
        describe_listing(&scope(3, &["tag=\"intro\""], READY), &page(1, 1)),
        "1 note matches tag=\"intro\" (vault has 3 notes, sorted by name)."
    );
}

#[test]
fn complete_filtered_listing_keeps_filter_and_vault_total() {
    assert_eq!(
        describe_listing(&scope(206, &["name=\"sop\""], READY), &page(5, 5)),
        "5 notes match name=\"sop\" (vault has 206 notes, sorted by name)."
    );
}

#[test]
fn folder_listing_uses_the_folder_count() {
    let folder = ListingScope { folder: Some("y"), folder_total: Some(41), ..scope(206, &[], READY) };
    assert_eq!(
        describe_listing(&folder, &page(41, 41)),
        "41 notes in folder \"y/\" (folder has 41 notes, sorted by name)."
    );
    let filtered = ListingScope { filters: vec!["tag=\"x\"".to_owned()], ..folder };
    assert_eq!(
        describe_listing(&filtered, &page(5, 5)),
        "5 notes match tag=\"x\" in folder \"y/\" (folder has 41 notes, sorted by name)."
    );
}

#[test]
fn partial_index_size_is_not_presented_as_the_vault_total() {
    let building = index(IndexState::Building, 57);
    assert_eq!(
        describe_listing(&scope(57, &["name=\"sop\""], building), &page(5, 5)),
        "5 notes match name=\"sop\" (sorted by name). Index: catching up (57 notes indexed so far); this list may be incomplete."
    );
    assert_eq!(
        describe_no_match(&scope(57, &["name=\"x\""], building)),
        "No notes match name=\"x\". Index: catching up (57 notes indexed so far); this list may be incomplete."
    );
}

#[test]
fn vault_served_listing_keeps_the_vault_total() {
    let status = IndexStatus { state: IndexState::Building, size: 0, served_by_vault: true };
    assert_eq!(
        describe_listing(&scope(206, &["name=\"sop\""], status), &page(5, 5)),
        "5 notes match name=\"sop\" (vault has 206 notes, sorted by name). Index: catching up (0 notes indexed so far); this list was read directly from the vault."
    );
}

#[test]
fn truncation_comes_first_with_the_omitted_folders() {
    let mut omitted = paths("folder-a", 41);
    omitted.extend(paths("folder-b", 9));
    omitted.extend(paths("folder-c", 56));
    let page = ListingPage { omitted: &omitted, ..page(100, 206) };
    assert_eq!(
        describe_listing(&scope(206, &[], READY), &page),
        "Showing 100 of 206 notes (sorted by name, limit=100). Omitted 106: folder-c/ (56), folder-a/ (41), folder-b/ (9). Raise `limit` or add a `folder` filter."
    );
}

#[test]
fn filter_truncation_and_index_state_share_one_line() {
    let omitted = ["daily/2026-03-24.md".to_owned()];
    let page = ListingPage { shown: 2, matched: 3, sort_by: "modified", limit: 2, omitted: &omitted };
    assert_eq!(
        describe_listing(&scope(206, &["tag=\"x\""], index(IndexState::Building, 57)), &page),
        "Showing 2 of 3 notes matching tag=\"x\" (sorted by modified, limit=2). Omitted 1: daily/ (1). Raise `limit` or narrow the filter. Index: catching up (57 notes indexed so far); this list may be incomplete."
    );
}
