//! Document structure from the Markdown syntax tree: heading sections, block
//! IDs (`^id`) and checkbox tasks, with source offsets and 1-based lines.

use std::ops::Range;
use std::sync::LazyLock;

use markdown::mdast::Node;
use markdown::{ParseOptions, to_mdast};
use regex::Regex;
use serde::Serialize;
use thiserror::Error;

use super::properties::split_frontmatter;
use crate::util::{char_len, take_chars};

static ATX_OPEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ {0,3}#{1,6}(?:[ \t]+|$)").unwrap());
static ATX_CLOSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]+#+[ \t]*$").unwrap());
static SETEXT_UNDERLINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\r?\n {0,3}(?:=+|-+)[ \t]*$").unwrap());
static SETEXT_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ {0,3}(?:=+|-+)[ \t]*(?:\r\n|\n|\r|$)").unwrap());
static ATX_START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ {0,3}#").unwrap());
static BLOCK_MARKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|[ \t])\^([A-Za-z0-9-]+)[ \t]*$").unwrap());
static CHECKBOX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[([ \txX])\](?:\s+|$)").unwrap());
static LINE_BREAK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:\r\n|\n|\r)").unwrap());

/// Characters of task text returned before it is cut.
const TASK_TEXT_MAX_CHARS: usize = 500;

/// A heading and the section it opens (up to the next heading of the same or a higher level).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NoteHeading {
    /// Titles from the top-level heading down to this one.
    pub heading: Vec<String>,
    pub level: u8,
    /// Byte offset of the heading line.
    pub start: usize,
    /// Byte offset where the section ends.
    pub end: usize,
    /// Byte offset of the first line after the heading.
    pub content_start: usize,
    pub start_line: usize,
    pub end_line: usize,
}

/// A paragraph or block labelled with a `^block-id` marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NoteBlock {
    pub id: String,
    /// Byte offset of the block.
    pub start: usize,
    /// Byte offset where the block ends, before its marker.
    pub end: usize,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoteStructure {
    pub headings: Vec<NoteHeading>,
    pub blocks: Vec<NoteBlock>,
}

/// A Markdown checkbox list item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteTask {
    pub line: usize,
    /// Text after the checkbox, cut at 500 characters.
    pub text: String,
    pub completed: bool,
    pub truncated: bool,
}

/// Why a heading or block target could not be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TargetError {
    #[error("Use either heading or block, not both.")]
    Both,
    #[error("Target not found. Use get_note_outline to find heading paths or block IDs.")]
    NotFound,
    #[error("Target is ambiguous. Use a unique heading path or block ID.")]
    Ambiguous,
}

fn parse(content: &str) -> Node {
    // CommonMark has no syntax errors; the parser only fails on MDX input, which is off.
    to_mdast(content, &ParseOptions::default()).expect("CommonMark parsing cannot fail")
}

/// Parse with frontmatter blanked out, so source positions stay where they are.
fn markdown_tree(content: &str) -> Node {
    let body_offset = split_frontmatter(content).body_offset;
    let mut masked: String =
        content[..body_offset].bytes().map(|b| if b == b'\r' || b == b'\n' { b as char } else { ' ' }).collect();
    masked.push_str(&content[body_offset..]);
    parse(&masked)
}

/// A node's byte range and first and last line. Some nodes (lists) end after
/// their final line break; the range is trimmed to end before it, like mdast.
fn span(content: &str, node: &Node) -> Option<(Range<usize>, usize, usize)> {
    let p = node.position()?;
    let source = &content[p.start.offset..p.end.offset];
    let trimmed = source.trim_end_matches(['\r', '\n']);
    if trimmed.len() == source.len() {
        return Some((p.start.offset..p.end.offset, p.start.line, p.end.line));
    }
    let end_line = p.start.line + trimmed.split("\r\n").flat_map(|l| l.split(['\n', '\r'])).count() - 1;
    Some((p.start.offset..p.start.offset + trimmed.len(), p.start.line, end_line))
}

fn children(node: &Node) -> &[Node] {
    node.children().map_or(&[], Vec::as_slice)
}

/// Headings (document level only) and block IDs (at any depth) of a note.
pub fn note_structure(content: &str) -> NoteStructure {
    let tree = markdown_tree(content);
    let mut headings: Vec<NoteHeading> = Vec::new();
    // Indices into `headings` of the sections still open.
    let mut parents: Vec<usize> = Vec::new();
    let last_line = if content.is_empty() {
        1
    } else {
        let lines = content.split("\r\n").flat_map(|l| l.split(['\n', '\r'])).count();
        lines - usize::from(content.ends_with(['\r', '\n']))
    };
    for node in children(&tree) {
        let Node::Heading(heading) = node else { continue };
        let Some((range, start_line, _)) = span(content, node) else { continue };
        let source = &content[range.clone()];
        let title = if ATX_OPEN.is_match(source) {
            let without_open = ATX_OPEN.replace(source, "");
            ATX_CLOSE.replace(&without_open, "").trim().to_owned()
        } else {
            SETEXT_UNDERLINE.replace(source, "").trim().to_owned()
        };
        while let Some(&parent) = parents.last() {
            if headings[parent].level < heading.depth {
                break;
            }
            parents.pop();
            headings[parent].end = range.start;
            headings[parent].end_line = start_line - 1;
        }
        let eol = LINE_BREAK.find(&content[range.end..]).map_or(0, |m| m.len());
        let mut path: Vec<String> = parents.iter().filter_map(|&p| headings[p].heading.last().cloned()).collect();
        path.push(title);
        headings.push(NoteHeading {
            heading: path,
            level: heading.depth,
            start: range.start,
            end: content.len(),
            content_start: range.end + eol,
            start_line,
            end_line: last_line,
        });
        parents.push(headings.len() - 1);
    }
    let mut blocks = Vec::new();
    collect_blocks(content, children(&tree), &mut blocks);
    blocks.sort_by_key(|b| b.start);
    NoteStructure { headings, blocks }
}

/// Paragraphs ending in a `^id` marker label themselves; a marker alone in a
/// paragraph labels the block before it.
fn collect_blocks(content: &str, siblings: &[Node], blocks: &mut Vec<NoteBlock>) {
    for (i, node) in siblings.iter().enumerate() {
        collect_blocks(content, children(node), blocks);
        let Node::Paragraph(paragraph) = node else { continue };
        let Some((range, start_line, end_line)) = span(content, node) else { continue };
        let Some(last @ Node::Text(_)) = paragraph.children.last() else { continue };
        let Some((last_range, ..)) = span(content, last) else { continue };
        let raw = &content[last_range.start..range.end];
        let Some(marker) = BLOCK_MARKER.captures(raw) else { continue };
        let marker_start = last_range.start + marker.get(0).map_or(0, |m| m.start());
        let id = marker[1].to_owned();
        if content[range.start..marker_start].trim().is_empty() {
            let Some(previous) = i.checked_sub(1).map(|p| &siblings[p]) else { continue };
            if matches!(previous, Node::Heading(_) | Node::ThematicBreak(_) | Node::Definition(_) | Node::Html(_)) {
                continue;
            }
            let Some((target, target_start, target_end)) = span(content, previous) else { continue };
            blocks.push(NoteBlock {
                id,
                start: target.start,
                end: target.end,
                start_line: target_start,
                end_line: target_end,
            });
        } else {
            blocks.push(NoteBlock { id, start: range.start, end: marker_start, start_line, end_line });
        }
    }
}

/// Byte range of a heading section's content (without the heading line) or of
/// a block (without its marker); the whole note when neither is given.
pub fn select_note_range(
    content: &str,
    heading: Option<&[String]>,
    block: Option<&str>,
) -> Result<Range<usize>, TargetError> {
    let matches: Vec<Range<usize>> = match (heading, block) {
        (Some(_), Some(_)) => return Err(TargetError::Both),
        (None, None) => return Ok(0..content.len()),
        (Some(heading), None) => note_structure(content)
            .headings
            .into_iter()
            .filter(|h| h.heading == heading)
            .map(|h| h.content_start..h.end)
            .collect(),
        (None, Some(block)) => {
            note_structure(content).blocks.into_iter().filter(|b| b.id == block).map(|b| b.start..b.end).collect()
        }
    };
    match matches.as_slice() {
        [] => Err(TargetError::NotFound),
        [only] => Ok(only.clone()),
        _ => Err(TargetError::Ambiguous),
    }
}

/// True when a line placed directly above `content` would join a setext
/// heading, as title text or above an underline.
pub fn starts_with_setext_boundary(content: &str) -> bool {
    if SETEXT_LINE.is_match(content) {
        return true;
    }
    let tree = parse(content);
    match children(&tree).first() {
        Some(first @ Node::Heading(_)) => {
            span(content, first).is_some_and(|(range, ..)| range.start == 0) && !ATX_START.is_match(content)
        }
        _ => false,
    }
}

/// Checkbox tasks (`- [ ] text`, `- [x] text`) anywhere in the note, outside frontmatter and code.
pub fn note_tasks(content: &str) -> Vec<NoteTask> {
    fn visit(content: &str, node: &Node, tasks: &mut Vec<NoteTask>) {
        if let Node::ListItem(item) = node {
            if let Some(first @ Node::Paragraph(_)) = item.children.first() {
                if let Some((range, line, _)) = span(content, first) {
                    let source = &content[range];
                    if let Some(checkbox) = CHECKBOX.captures(source) {
                        let text = &source[checkbox.get(0).map_or(0, |m| m.end())..];
                        tasks.push(NoteTask {
                            line,
                            text: take_chars(text, TASK_TEXT_MAX_CHARS).to_owned(),
                            completed: matches!(&checkbox[1], "x" | "X"),
                            truncated: char_len(text) > TASK_TEXT_MAX_CHARS,
                        });
                    }
                }
            }
        }
        for child in children(node) {
            visit(content, child, tasks);
        }
    }
    let mut tasks = Vec::new();
    visit(content, &markdown_tree(content), &mut tasks);
    tasks
}

#[cfg(test)]
mod tests;
