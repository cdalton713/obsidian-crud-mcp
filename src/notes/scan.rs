//! Paged, resumable scans over note content (`search_notes`, `list_tasks`).

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::{DecodePaddingMode, general_purpose::URL_SAFE_NO_PAD};
use futures::StreamExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{is_valid_note_path, make_deep_link, parse_frontmatter_and_links};
use crate::search::{IndexState, SearchIndex};
use crate::util::{char_len, trim_slashes};
use crate::vault::{VaultBackend, VaultError};

/// Notes read at once. Reads come from local disk, so this mostly hides syscall latency.
pub const SCAN_READ_CONCURRENCY: usize = 16;
/// A page stops before a note that would take it past this many characters.
pub const SCAN_PAGE_CHARS: usize = 50_000_000;
/// Notes longer than this are reported in `skipped_notes` instead of scanned.
pub const SCAN_NOTE_MAX_CHARS: usize = 1_000_000;

/// Base64url that also accepts padding, for cursors handed back by clients.
const CURSOR_DECODER: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

fn default_limit() -> u32 {
    20
}

fn default_max_notes() -> u32 {
    10_000
}

/// Filters and paging shared by every content scan.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct ScanParameters {
    /// Folder and its descendants; folder boundaries are respected.
    #[schemars(length(max = 1000))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// Exact tag, with or without a leading #.
    #[schemars(length(min = 1, max = 200))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Results per page, 1 to 50.
    #[schemars(range(min = 1, max = 50))]
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Maximum notes read per call; the default covers a whole vault of typical size. Follow next_cursor even when this page has no results.
    #[schemars(range(min = 1, max = 100_000))]
    #[serde(default = "default_max_notes")]
    pub max_notes: u32,
    /// Opaque next_cursor from the previous page using the same filters. Results are live, not a snapshot.
    #[schemars(length(max = 3000))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// One result line within a note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LineMatch {
    pub line: usize,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}

/// Where a page stopped; serialized (base64url JSON) as the opaque cursor.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cursor {
    key: String,
    path: String,
    line: usize,
}

/// Tuning knobs, mostly for tests.
#[derive(Clone, Copy)]
pub struct ScanOptions<'a> {
    /// When ready, supplies the candidates (folder and tag already applied) and cached content.
    pub index: Option<&'a SearchIndex>,
    pub page_chars: usize,
    pub concurrency: usize,
}

impl Default for ScanOptions<'_> {
    fn default() -> Self {
        Self { index: None, page_chars: SCAN_PAGE_CHARS, concurrency: SCAN_READ_CONCURRENCY }
    }
}

#[derive(Serialize)]
struct ResultItem {
    #[serde(flatten)]
    found: LineMatch,
    path: String,
    url: String,
}

#[derive(Serialize)]
struct Skipped {
    path: String,
    reason: String,
}

enum Read {
    Content(Option<Arc<str>>),
    Error,
}

/// Which notes a scan should read, in cursor order (byte order, so comparisons
/// against the cursor path agree with it). A ready index answers folder and tag
/// filters from memory; otherwise the vault is listed and the tag is checked
/// against each note's content as it is read.
async fn candidates(
    vault: &dyn VaultBackend,
    index: Option<&SearchIndex>,
    folder: Option<&str>,
    tag: Option<&str>,
) -> Result<(Vec<String>, bool), VaultError> {
    if let Some(index) = index.filter(|i| i.state() == IndexState::Ready) {
        let mut paths = index.list_paths(folder);
        if let Some(tag) = tag {
            paths.retain(|path| index.get_tags(path).iter().any(|t| t == tag));
        }
        paths.sort();
        return Ok((paths, true));
    }
    let mut paths: Vec<String> = vault
        .list_notes(None)
        .await?
        .into_iter()
        .filter(|path| is_valid_note_path(path) && folder.is_none_or(|f| path.starts_with(&format!("{f}/"))))
        .collect();
    paths.sort();
    paths.dedup();
    Ok((paths, false))
}

/// Scan current note content, with bounded reads and resumable positions.
///
/// Content comes from the index's in-memory cache when it holds the note and
/// from disk otherwise (the disk copy is then cached for the next scan). Disk
/// reads run a few at a time (`concurrency`) but notes are consumed in path
/// order, so pages and cursors are deterministic regardless of which read
/// finishes first. A page ends at `max_notes` notes, at `page_chars`
/// characters, or when `limit` results are collected; `next_cursor` resumes
/// exactly there. Returns the page as JSON; fails only when the vault cannot be listed.
pub async fn scan_notes(
    vault: &dyn VaultBackend,
    vault_name: &str,
    options: &ScanParameters,
    filter_key: &str,
    find_matches: impl Fn(&str) -> Vec<LineMatch>,
    scan: ScanOptions<'_>,
) -> Result<String, VaultError> {
    let folder = options.folder.as_deref().map(trim_slashes).filter(|f| !f.is_empty());
    // A bare "#" names no tag; filtering on "" would hide every note.
    let tag = options.tag.as_deref().map(|t| t.strip_prefix('#').unwrap_or(t)).filter(|t| !t.is_empty());
    let key = hex::encode(Sha256::digest(json!([filter_key, folder.unwrap_or(""), tag.unwrap_or("")]).to_string()));
    let cursor = match options.cursor.as_deref() {
        None => None,
        Some(raw) => match decode_cursor(raw, &key) {
            Some(cursor) => Some(cursor),
            None => {
                return Ok(
                    json!({ "error": "Invalid cursor or changed filters. Start again without cursor." }).to_string()
                );
            }
        },
    };
    let (paths, tag_applied) = candidates(vault, scan.index, folder, tag).await?;

    let mut results: Vec<ResultItem> = Vec::new();
    let mut skipped: Vec<Skipped> = Vec::new();
    let mut scanned = 0usize;
    let mut chars = 0usize;
    let limit = options.limit.max(1) as usize;
    let max_notes = options.max_notes.max(1) as usize;
    let start_line = |path: &str| cursor.as_ref().filter(|c| c.path == path).map_or(1, |c| c.line);
    let finish = |results: &[ResultItem], skipped: &[Skipped], scanned: usize, next: Option<(&str, usize)>| {
        let next_cursor = next.map(|(path, line)| {
            let cursor = Cursor { key: key.clone(), path: path.to_owned(), line };
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor).unwrap_or_default())
        });
        json!({
            "results": results,
            "scanned_notes": scanned,
            "skipped_notes": skipped,
            "next_cursor": next_cursor,
        })
        .to_string()
    };

    let first = cursor.as_ref().map_or(0, |c| paths.partition_point(|p| p.as_str() < c.path.as_str()));
    // Read ahead of the in-order walk, never past what this page can take.
    let last = paths.len().min(first + max_notes);
    let index = scan.index;
    let mut reads = futures::stream::iter(first..last)
        .map(|i| read_for_scan(vault, index, paths[i].clone()))
        .buffered(scan.concurrency.max(1));

    let mut i = first;
    while let Some(outcome) = reads.next().await {
        let path = &paths[i];
        let from_line = start_line(path);
        let content = match outcome {
            Read::Error => {
                scanned += 1;
                skipped.push(Skipped { path: path.clone(), reason: "read_error".into() });
                i += 1;
                continue;
            }
            Read::Content(content) => content,
        };
        let length = content.as_deref().map(char_len);
        // Leave a note that would push this page past the cap for the next page, which always takes at least one.
        if let Some(length) = length {
            if scanned > 0 && length <= SCAN_NOTE_MAX_CHARS && chars + length > scan.page_chars {
                return Ok(finish(&results, &skipped, scanned, Some((path, from_line))));
            }
        }
        scanned += 1;
        let (Some(content), Some(length)) = (content, length) else {
            skipped.push(Skipped { path: path.clone(), reason: "not_found_or_unreadable".into() });
            i += 1;
            continue;
        };
        if length > SCAN_NOTE_MAX_CHARS {
            skipped
                .push(Skipped { path: path.clone(), reason: format!("exceeds_{SCAN_NOTE_MAX_CHARS}_char_scan_limit") });
            i += 1;
            continue;
        }
        chars += length;
        if let Some(tag) = tag.filter(|_| !tag_applied) {
            let tags = parse_frontmatter_and_links(&content.replace("\r\n", "\n")).tags;
            if !tags.iter().any(|t| t == tag) {
                i += 1;
                continue;
            }
        }
        let matches: Vec<LineMatch> = find_matches(&content).into_iter().filter(|m| m.line >= from_line).collect();
        let count = matches.len();
        for (j, found) in matches.into_iter().enumerate() {
            let line = found.line;
            results.push(ResultItem { found, path: path.clone(), url: make_deep_link(vault_name, path) });
            if results.len() >= limit {
                if j + 1 < count {
                    return Ok(finish(&results, &skipped, scanned, Some((path, line + 1))));
                }
                let next = paths.get(i + 1).map(|p| (p.as_str(), 1));
                return Ok(finish(&results, &skipped, scanned, next));
            }
        }
        i += 1;
    }
    match paths.get(last) {
        Some(path) => Ok(finish(&results, &skipped, scanned, Some((path, start_line(path))))),
        None => Ok(finish(&results, &skipped, scanned, None)),
    }
}

/// Content for a scan: from the index's cache when it holds the note, else from disk (then cached).
async fn read_for_scan(vault: &dyn VaultBackend, index: Option<&SearchIndex>, path: String) -> Read {
    if let Some(cached) = index.and_then(|i| i.get_content(&path)) {
        return Read::Content(Some(cached));
    }
    match vault.read_note(&path).await {
        Ok(content) => {
            let content: Option<Arc<str>> = content.map(Arc::from);
            // Only fill a gap: a change that landed meanwhile already cached newer content.
            if let (Some(index), Some(content)) = (index, &content) {
                if index.get_content(&path).is_none() {
                    index.cache_content(&path, content.clone());
                }
            }
            Read::Content(content)
        }
        Err(_) => Read::Error,
    }
}

fn decode_cursor(raw: &str, key: &str) -> Option<Cursor> {
    let bytes = CURSOR_DECODER.decode(raw).ok()?;
    let cursor: Cursor = serde_json::from_slice(&bytes).ok()?;
    let valid = cursor.key == key && is_valid_note_path(&cursor.path) && (1..=1_000_001).contains(&cursor.line);
    valid.then_some(cursor)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::vault::LocalVault;

    async fn vault(notes: &[(&str, &str)]) -> (tempfile::TempDir, LocalVault) {
        let dir = tempfile::tempdir().unwrap();
        let vault = LocalVault::new(dir.path(), None).unwrap();
        for (path, content) in notes {
            assert!(vault.write_note(path, content).await.unwrap(), "write {path}");
        }
        (dir, vault)
    }

    fn params(cursor: Option<&Value>) -> ScanParameters {
        ScanParameters {
            limit: 20,
            max_notes: 100,
            cursor: cursor.and_then(Value::as_str).map(str::to_owned),
            ..ScanParameters::default()
        }
    }

    async fn scan(
        vault: &LocalVault,
        params: &ScanParameters,
        page_chars: usize,
        find: impl Fn(&str) -> Vec<LineMatch>,
    ) -> Value {
        let options = ScanOptions { page_chars, ..ScanOptions::default() };
        let page = scan_notes(vault, "V", params, "test", find, options).await.unwrap();
        serde_json::from_str(&page).unwrap()
    }

    fn first_line(content: &str) -> Vec<LineMatch> {
        let text = content.chars().next().map(String::from).unwrap_or_default();
        vec![LineMatch { line: 1, text, completed: None, truncated: None }]
    }

    #[tokio::test]
    async fn pages_stop_before_a_note_would_exceed_the_character_cap() {
        let notes = ["a", "b", "c"].map(|c| (format!("{c}.md"), c.repeat(900_000)));
        let refs: Vec<(&str, &str)> = notes.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
        let (_dir, vault) = vault(&refs).await;
        let first = scan(&vault, &params(None), 2_000_000, |_| Vec::new()).await;
        assert_eq!(first["scanned_notes"], 2);
        assert!(first["next_cursor"].is_string());
        let second = scan(&vault, &params(Some(&first["next_cursor"])), 2_000_000, first_line).await;
        assert_eq!(second["scanned_notes"], 1);
        assert_eq!(second["results"][0]["path"], "c.md");
        assert_eq!(second["next_cursor"], Value::Null);
    }

    #[tokio::test]
    async fn pages_always_process_at_least_one_note() {
        let notes = ["a", "b", "c"].map(|c| (format!("{c}.md"), c.repeat(1_000_000)));
        let refs: Vec<(&str, &str)> = notes.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
        let (_dir, vault) = vault(&refs).await;
        let first = scan(&vault, &params(None), 2_000_000, |_| Vec::new()).await;
        assert_eq!(first["scanned_notes"], 2);
        let second = scan(&vault, &params(Some(&first["next_cursor"])), 2_000_000, |_| Vec::new()).await;
        assert_eq!(second["scanned_notes"], 1);
        assert_eq!(second["next_cursor"], Value::Null);
        // A single note larger than the page still gets scanned on its own page.
        let tiny = scan(&vault, &params(None), 10, |_| Vec::new()).await;
        assert_eq!(tiny["scanned_notes"], 1);
    }

    #[tokio::test]
    async fn page_cap_counts_characters_not_bytes() {
        let wide = "é".repeat(600_000);
        let (_dir, vault) = vault(&[("a.md", &wide), ("b.md", &wide)]).await;
        let page = scan(&vault, &params(None), 1_300_000, |_| Vec::new()).await;
        assert_eq!(page["scanned_notes"], 2, "{page}");
        assert_eq!(page["next_cursor"], Value::Null);
    }

    #[tokio::test]
    async fn bad_or_mismatched_cursors_are_reported() {
        let (_dir, vault) = vault(&[("a.md", "x"), ("b.md", "y")]).await;
        let one = ScanParameters { limit: 1, ..params(None) };
        let page = scan(&vault, &one, SCAN_PAGE_CHARS, first_line).await;
        let cursor = page["next_cursor"].as_str().unwrap().to_owned();

        // Clients may hand back a padded cursor.
        let padded = format!("{cursor}{}", "=".repeat((4 - cursor.len() % 4) % 4));
        let resumed = ScanParameters { cursor: Some(padded), ..one.clone() };
        assert_eq!(scan(&vault, &resumed, SCAN_PAGE_CHARS, first_line).await["results"][0]["path"], "b.md");

        let forged = URL_SAFE_NO_PAD.encode(br#"{"key":"x","path":"b.md","line":1}"#);
        for cursor in ["not base64!", "e30", forged.as_str()] {
            let bad = ScanParameters { cursor: Some(cursor.to_owned()), ..one.clone() };
            let page = scan(&vault, &bad, SCAN_PAGE_CHARS, first_line).await;
            assert_eq!(page["error"], "Invalid cursor or changed filters. Start again without cursor.", "{cursor}");
        }
        let other_folder = ScanParameters { folder: Some("elsewhere".into()), cursor: Some(cursor), ..one };
        assert!(scan(&vault, &other_folder, SCAN_PAGE_CHARS, first_line).await["error"].is_string());
    }

    #[tokio::test]
    async fn folder_slashes_and_a_bare_hash_tag_do_not_filter_everything_out() {
        let (_dir, vault) = vault(&[("work/a.md", "x"), ("workshop/b.md", "y"), ("c.md", "z")]).await;
        let folder = ScanParameters { folder: Some("/work/".into()), ..params(None) };
        let page = scan(&vault, &folder, SCAN_PAGE_CHARS, first_line).await;
        assert_eq!(page["results"].as_array().unwrap().len(), 1, "{page}");
        assert_eq!(page["results"][0]["path"], "work/a.md");

        let hash = ScanParameters { tag: Some("#".into()), ..params(None) };
        let page = scan(&vault, &hash, SCAN_PAGE_CHARS, first_line).await;
        assert_eq!(page["results"].as_array().unwrap().len(), 3, "{page}");
    }
}
