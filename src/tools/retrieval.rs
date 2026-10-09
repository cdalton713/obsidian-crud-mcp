//! Read-side tools that return JSON: `list_tasks`, `get_note_outline`,
//! `search_notes` and `read_notes`.

use std::sync::{Arc, LazyLock};

use regex::{Regex, RegexBuilder};
use serde::Serialize;
use serde_json::json;

use super::ToolContext;
use super::params::{GetNoteOutlineParams, ListTasksParams, ReadNotesParams, SearchNotesParams, TaskStatus};
use crate::mcp::ToolError;
use crate::notes::{LineMatch, ScanOptions, make_deep_link, note_structure, note_tasks, scan_notes, validate_note_path};
use crate::util::char_len;

/// A note without a checkbox marker has no tasks; skip the Markdown parse.
static TASK_MARKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[ \txX]\]").unwrap());

/// Excerpt context on each side of a match, in characters.
const EXCERPT_RADIUS: usize = 80;

/// One excerpt per matching line, found with a single pass of the needle over
/// the whole note rather than a regex call per line. Line numbers are counted
/// only up to each match, so a note without matches costs one failed search.
pub fn find_line_matches(content: &str, needle: &Regex) -> Vec<LineMatch> {
    let bytes = content.as_bytes();
    let mut matches = Vec::new();
    let mut line = 1;
    let mut pos = 0;
    let mut line_start = 0;
    let mut search_from = 0;
    while let Some(found) = needle.find_at(content, search_from) {
        while pos < found.start() {
            match bytes[pos] {
                b'\n' => {
                    line += 1;
                    line_start = pos + 1;
                }
                b'\r' => {
                    if bytes.get(pos + 1) == Some(&b'\n') {
                        pos += 1;
                    }
                    line += 1;
                    line_start = pos + 1;
                }
                _ => {}
            }
            pos += 1;
        }
        let line_end = content[pos..].find(['\n', '\r']).map_or(content.len(), |i| pos + i);
        let text = &content[line_start..line_end];
        let at = found.start() - line_start;
        let before: Vec<(usize, char)> = text[..at].char_indices().collect();
        let start = before.len().checked_sub(EXCERPT_RADIUS).map_or(0, |i| before[i].0);
        let after_match = found.end() - line_start;
        let end = text[after_match..].char_indices().nth(EXCERPT_RADIUS).map_or(text.len(), |(i, _)| after_match + i);
        matches.push(LineMatch {
            line,
            text: format!(
                "{}{}{}",
                if start > 0 { "…" } else { "" },
                &text[start..end],
                if end < text.len() { "…" } else { "" }
            ),
            completed: None,
            truncated: None,
        });
        // Resume after this line so it yields one excerpt at most.
        search_from = line_end;
        pos = line_end;
        if line_end >= content.len() {
            break;
        }
    }
    matches
}

pub async fn list_tasks(ctx: Arc<ToolContext>, args: ListTasksParams) -> Result<String, ToolError> {
    let status = args.status;
    let filter_key = json!(["tasks", status.as_str()]).to_string();
    let find = |content: &str| -> Vec<LineMatch> {
        if !TASK_MARKER.is_match(content) {
            return Vec::new();
        }
        note_tasks(content)
            .into_iter()
            .filter(|task| status == TaskStatus::All || task.completed == (status == TaskStatus::Completed))
            .map(|task| LineMatch {
                line: task.line,
                text: task.text,
                completed: Some(task.completed),
                truncated: Some(task.truncated),
            })
            .collect()
    };
    let options = ScanOptions { index: Some(&ctx.index), ..ScanOptions::default() };
    Ok(scan_notes(ctx.vault.as_ref(), &ctx.vault_name, &args.scan, &filter_key, find, options).await?)
}

pub async fn get_note_outline(ctx: Arc<ToolContext>, args: GetNoteOutlineParams) -> Result<String, ToolError> {
    let Some(content) = ctx.vault.read_note(&args.path).await? else {
        return Ok(json!({ "error": format!("Note not found: {}", args.path) }).to_string());
    };
    let outline = note_structure(&content);
    let headings: Vec<_> = outline
        .headings
        .iter()
        .map(|h| json!({ "heading": h.heading, "level": h.level, "start_line": h.start_line, "end_line": h.end_line }))
        .collect();
    let blocks: Vec<_> = outline
        .blocks
        .iter()
        .map(|b| json!({ "id": b.id, "start_line": b.start_line, "end_line": b.end_line }))
        .collect();
    Ok(json!({
        "path": args.path,
        "url": make_deep_link(&ctx.vault_name, &args.path),
        "headings": headings,
        "blocks": blocks,
    })
    .to_string())
}

pub async fn search_notes(ctx: Arc<ToolContext>, args: SearchNotesParams) -> Result<String, ToolError> {
    let needle = RegexBuilder::new(&regex::escape(&args.query))
        .case_insensitive(!args.case_sensitive)
        .build()
        .map_err(|e| ToolError::Message(e.to_string()))?;
    let filter_key = json!(["search", args.query, args.case_sensitive]).to_string();
    let options = ScanOptions { index: Some(&ctx.index), ..ScanOptions::default() };
    let find = |content: &str| find_line_matches(content, &needle);
    Ok(scan_notes(ctx.vault.as_ref(), &ctx.vault_name, &args.scan, &filter_key, find, options).await?)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReadStatus {
    Ok,
    NotFound,
    Error,
    Truncated,
}

#[derive(Debug, Clone, Serialize)]
struct ReadResult {
    path: String,
    status: ReadStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    omitted_chars: Option<usize>,
}

fn serialize_batch(notes: &[ReadResult], omitted: &[String]) -> String {
    let missing: Vec<&str> = notes
        .iter()
        .filter(|n| matches!(n.status, ReadStatus::NotFound))
        .map(|n| n.path.as_str())
        .collect();
    json!({ "notes": notes, "missing_paths": missing, "omitted_paths": omitted }).to_string()
}

pub async fn read_notes(ctx: Arc<ToolContext>, args: ReadNotesParams) -> Result<String, ToolError> {
    let ReadNotesParams { paths, max_chars } = args;
    let max_chars = max_chars as usize;
    let fits = |notes: &[ReadResult], omitted: &[String]| char_len(&serialize_batch(notes, omitted)) <= max_chars;
    if !fits(&[], &paths) {
        return Ok(json!({
            "error": "max_chars is too small to report these paths. Increase it or request fewer paths."
        })
        .to_string());
    }
    let mut notes: Vec<ReadResult> = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        let mut note = ReadResult { path: path.clone(), status: ReadStatus::Ok, url: None, content: None, omitted_chars: None };
        match validate_note_path(path) {
            Err(_) => note.status = ReadStatus::Error,
            Ok(()) => {
                note.url = Some(make_deep_link(&ctx.vault_name, path));
                match ctx.vault.read_note(path).await {
                    Ok(Some(content)) => note.content = Some(content),
                    Ok(None) => note.status = ReadStatus::NotFound,
                    Err(_) => note.status = ReadStatus::Error,
                }
            }
        }
        let remaining = &paths[i + 1..];
        notes.push(note);
        if fits(&notes, remaining) {
            continue;
        }
        let mut note = notes.pop().expect("just pushed");
        if let Some(content) = note.content.take() {
            let total = char_len(&content);
            note.status = ReadStatus::Truncated;
            note.content = Some(String::new());
            note.omitted_chars = Some(total);
            notes.push(note);
            if fits(&notes, remaining) {
                // Binary search for the longest prefix (in characters) that still fits.
                let boundaries: Vec<usize> = content.char_indices().map(|(b, _)| b).chain([content.len()]).collect();
                let (mut lo, mut hi) = (0, total);
                while lo < hi {
                    let mid = (lo + hi).div_ceil(2);
                    let note = notes.last_mut().expect("just pushed");
                    note.content = Some(content[..boundaries[mid]].to_owned());
                    note.omitted_chars = Some(total - mid);
                    if fits(&notes, remaining) { lo = mid } else { hi = mid - 1 }
                }
                let note = notes.last_mut().expect("just pushed");
                note.content = Some(content[..boundaries[lo]].to_owned());
                note.omitted_chars = Some(total - lo);
                return Ok(serialize_batch(&notes, remaining));
            }
            notes.pop();
        }
        return Ok(serialize_batch(&notes, &paths[i..]));
    }
    Ok(serialize_batch(&notes, &[]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(content: &str, query: &str) -> Vec<(usize, String)> {
        let needle = RegexBuilder::new(&regex::escape(query)).case_insensitive(true).build().unwrap();
        find_line_matches(content, &needle).into_iter().map(|m| (m.line, m.text)).collect()
    }

    #[test]
    fn one_excerpt_per_matching_line() {
        assert_eq!(
            matches("alpha\nbeta beta\r\ngamma\rbeta", "beta"),
            vec![(2, "beta beta".to_string()), (4, "beta".to_string())]
        );
        assert_eq!(matches("no hits", "zzz"), vec![]);
        assert_eq!(matches("ÉCOLE école", "école"), vec![(1, "ÉCOLE école".to_string())]);
    }

    #[test]
    fn long_lines_are_cut_around_the_match() {
        let line = format!("{}needle{}", "a".repeat(100), "b".repeat(100));
        let found = matches(&line, "needle");
        assert_eq!(found[0].1, format!("…{}needle{}…", "a".repeat(80), "b".repeat(80)));
    }

    #[test]
    fn special_characters_are_literal() {
        assert_eq!(matches("cost (USD) $5.00", "(usd) $5"), vec![(1, "cost (USD) $5.00".to_string())]);
    }
}
