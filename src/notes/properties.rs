//! Frontmatter (Obsidian "properties"): splitting it from the body, reading it
//! as JSON-like values, and editing top-level keys without touching the body.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};
use thiserror::Error;

/// Parsed frontmatter: top-level keys in document order.
pub type Frontmatter = Map<String, Value>;

static OPENING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\x{FEFF}?---[ \t]*(?:\r?\n|$)").unwrap());
static CLOSING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^---[ \t]*(?:\r?\n|$)").unwrap());

/// Why a property edit was refused. No changes are made in any of these cases.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PropertyError {
    #[error("A property cannot be set and removed in the same call.")]
    SetAndRemove,
    #[error("Frontmatter has no closing --- delimiter.")]
    NoClosingDelimiter,
    #[error("Invalid or unsupported YAML frontmatter; no changes made.")]
    InvalidYaml,
    #[error("Frontmatter must be a YAML mapping.")]
    NotAMapping,
}

/// A note split into frontmatter and body. The delimiters are kept separately
/// so YAML edits cannot change the Markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterParts<'a> {
    /// The YAML between the delimiters, or `None` when the note has no frontmatter.
    pub yaml: Option<&'a str>,
    pub body: &'a str,
    /// Byte offset of the body in the note.
    pub body_offset: usize,
    /// The opening delimiter line, BOM included.
    pub opening: &'a str,
    /// The closing delimiter line.
    pub closing: &'a str,
    /// The note's line ending: CRLF if it uses any, LF otherwise.
    pub eol: &'static str,
}

/// Split a note into frontmatter and body. An opening `---` without a closing
/// one is a Markdown horizontal rule, not frontmatter.
pub fn split_frontmatter(content: &str) -> FrontmatterParts<'_> {
    let eol = if content.contains("\r\n") { "\r\n" } else { "\n" };
    let none = FrontmatterParts { yaml: None, body: content, body_offset: 0, opening: "", closing: "", eol };
    let Some(opening) = OPENING.find(content) else {
        return none;
    };
    let rest = &content[opening.end()..];
    let Some(closing) = CLOSING.find(rest) else {
        return none;
    };
    let body_offset = opening.end() + closing.end();
    FrontmatterParts {
        yaml: Some(&rest[..closing.start()]),
        body: &content[body_offset..],
        body_offset,
        opening: opening.as_str(),
        closing: closing.as_str(),
        eol,
    }
}

/// The note's properties, or an empty map when it has none or they are malformed.
pub fn read_properties(content: &str) -> Frontmatter {
    split_frontmatter(content).yaml.and_then(|yaml| parse_mapping(yaml).ok()).unwrap_or_default()
}

/// Parse frontmatter YAML into a map of top-level keys.
fn parse_mapping(source: &str) -> Result<Frontmatter, PropertyError> {
    let source = source.replace("\r\n", "\n").replace('\r', "\n");
    if source.lines().all(|line| {
        let line = line.trim_start();
        line.is_empty() || line.starts_with('#')
    }) {
        return Ok(Frontmatter::new());
    }
    let value: serde_norway::Value = serde_norway::from_str(&source).map_err(|_| PropertyError::InvalidYaml)?;
    match yaml_to_json(value) {
        Value::Object(map) => Ok(map),
        Value::Null => Ok(Frontmatter::new()),
        _ => Err(PropertyError::NotAMapping),
    }
}

fn yaml_to_json(value: serde_norway::Value) -> Value {
    use serde_norway::Value as Yaml;
    match value {
        Yaml::Null => Value::Null,
        Yaml::Bool(b) => Value::Bool(b),
        Yaml::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::from(i)
            } else if let Some(u) = n.as_u64() {
                Value::from(u)
            } else {
                n.as_f64().and_then(serde_json::Number::from_f64).map_or(Value::Null, Value::Number)
            }
        }
        Yaml::String(s) => Value::String(s),
        Yaml::Sequence(items) => Value::Array(items.into_iter().map(yaml_to_json).collect()),
        Yaml::Mapping(map) => Value::Object(map.into_iter().map(|(k, v)| (key_string(k), yaml_to_json(v))).collect()),
        Yaml::Tagged(tagged) => yaml_to_json(tagged.value),
    }
}

/// Mapping keys are always strings.
fn key_string(key: serde_norway::Value) -> String {
    match yaml_to_json(key) {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Deep equality where `1` and `1.0` are the same number.
pub(crate) fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(x), Some(y)) => x == y,
            _ => x.as_f64() == y.as_f64(),
        },
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_eq(a, b)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|other| json_eq(v, other)))
        }
        _ => a == b,
    }
}

/// Set and remove top-level properties.
///
/// Untouched keys keep their exact text, comments included; set keys are
/// written in place (new ones after the last key) and removed keys go with the
/// comments directly above them. If the frontmatter is too unusual to edit line
/// by line (anchors shared with changed keys, flow-style roots, complex keys),
/// it is serialized again from its values instead. The body is never changed.
pub fn update_properties(content: &str, set: &Frontmatter, remove: &[String]) -> Result<String, PropertyError> {
    if remove.iter().any(|key| set.contains_key(key)) {
        return Err(PropertyError::SetAndRemove);
    }
    let parts = split_frontmatter(content);
    if parts.yaml.is_none() && OPENING.is_match(content) {
        return Err(PropertyError::NoClosingDelimiter);
    }
    if parts.yaml.is_none() && set.is_empty() {
        return Ok(content.to_owned());
    }
    let before = parse_mapping(parts.yaml.unwrap_or(""))?;
    let unchanged = set.iter().all(|(key, value)| before.get(key).is_some_and(|old| json_eq(old, value)))
        && remove.iter().all(|key| !before.contains_key(key));
    if unchanged {
        return Ok(content.to_owned());
    }

    let mut expected = before;
    for key in remove {
        expected.shift_remove(key);
    }
    for (key, value) in set {
        expected.insert(key.clone(), value.clone());
    }

    let to_eol = |text: &str| text.replace('\n', parts.eol);
    let Some(yaml) = parts.yaml else {
        let bom = if content.starts_with('\u{FEFF}') { "\u{FEFF}" } else { "" };
        let yaml = serialize_mapping(&expected);
        return Ok(format!("{bom}{}{}", to_eol(&format!("---\n{yaml}---\n")), &content[bom.len()..]));
    };

    let normalized = yaml.replace("\r\n", "\n").replace('\r', "\n");
    let yaml = edit_in_place(&normalized, set, remove)
        .filter(|edited| {
            parse_mapping(edited).is_ok_and(|parsed| {
                parsed.len() == expected.len()
                    && parsed.iter().zip(&expected).all(|((k1, v1), (k2, v2))| k1 == k2 && json_eq(v1, v2))
            })
        })
        .unwrap_or_else(|| serialize_mapping(&expected));
    if yaml.is_empty() {
        return Ok(parts.body.to_owned());
    }
    Ok(format!("{}{}{}{}", parts.opening, to_eol(&yaml), parts.closing, parts.body))
}

/// How one line of block-style YAML relates to the top-level mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Blank,
    /// A comment starting in column 0.
    Comment,
    /// A top-level `key: value` line.
    Key,
    /// Part of the previous key's value (indented, or an unindented `- item`).
    Continuation,
}

fn classify(line: &str) -> LineKind {
    let text = line.trim_end_matches('\n');
    if text.trim().is_empty() {
        return LineKind::Blank;
    }
    let mut chars = text.chars();
    match chars.next() {
        Some('#') => LineKind::Comment,
        Some(' ' | '\t') => LineKind::Continuation,
        Some('-') if matches!(chars.next(), None | Some(' ' | '\t')) => LineKind::Continuation,
        _ => LineKind::Key,
    }
}

/// One top-level key: the comment and blank lines directly above it, then the key line and its value.
struct Entry {
    key: String,
    leading: Vec<String>,
    body: Vec<String>,
}

enum Segment {
    Free(Vec<String>),
    Entry(Entry),
}

/// The key named on a top-level key line, unquoted.
fn key_of(line: &str) -> Option<String> {
    let line = line.trim_end_matches('\n');
    let quote = line.chars().next().filter(|c| *c == '"' || *c == '\'');
    let end = match quote {
        Some(q) => line[1..].find(q)? + 2,
        None => line.find(": ").or_else(|| line.strip_suffix(':').map(str::len))?,
    };
    let probe = format!("{}: 0\n", &line[..end]);
    let map = parse_mapping(&probe).ok()?;
    map.keys().next().cloned()
}

/// Edit the mapping line by line, or `None` when its layout is not plain block style.
fn edit_in_place(yaml: &str, set: &Frontmatter, remove: &[String]) -> Option<String> {
    let lines: Vec<String> = yaml.split_inclusive('\n').map(str::to_owned).collect();
    let kinds: Vec<LineKind> = lines.iter().map(|l| classify(l)).collect();
    let mut segments: Vec<Segment> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        match kinds[i] {
            LineKind::Blank | LineKind::Comment => {
                pending.push(lines[i].clone());
                i += 1;
            }
            LineKind::Continuation => return None,
            LineKind::Key => {
                // Comments directly above the key, and blank lines directly above those, belong to it.
                let comments = pending.iter().rev().take_while(|l| classify(l) == LineKind::Comment).count();
                let blanks = pending[..pending.len() - comments]
                    .iter()
                    .rev()
                    .take_while(|l| classify(l) == LineKind::Blank)
                    .count();
                let leading = pending.split_off(pending.len() - comments - blanks);
                if !pending.is_empty() {
                    segments.push(Segment::Free(std::mem::take(&mut pending)));
                }
                let key = key_of(&lines[i])?;
                let mut body = vec![lines[i].clone()];
                i += 1;
                // The value runs on through blank lines and comments that more of it follows.
                while i < lines.len() {
                    let next_value = kinds[i..]
                        .iter()
                        .position(|k| !matches!(k, LineKind::Blank | LineKind::Comment))
                        .map(|offset| kinds[i + offset]);
                    match (kinds[i], next_value) {
                        (LineKind::Continuation, _) => {}
                        (LineKind::Blank | LineKind::Comment, Some(LineKind::Continuation)) => {}
                        _ => break,
                    }
                    body.push(lines[i].clone());
                    i += 1;
                }
                segments.push(Segment::Entry(Entry { key, leading, body }));
            }
        }
    }
    if !pending.is_empty() {
        segments.push(Segment::Free(pending));
    }

    let mut written = std::collections::HashSet::new();
    let mut out: Vec<Segment> = Vec::with_capacity(segments.len());
    for segment in segments {
        match segment {
            Segment::Entry(entry) if remove.contains(&entry.key) => {}
            Segment::Entry(mut entry) => {
                if let Some(value) = set.get(&entry.key) {
                    entry.body = vec![serialize_entry(&entry.key, value)];
                    written.insert(entry.key.clone());
                }
                out.push(Segment::Entry(entry));
            }
            free => out.push(free),
        }
    }
    let additions: String = set
        .iter()
        .filter(|(key, _)| !written.contains(key.as_str()))
        .map(|(key, value)| serialize_entry(key, value))
        .collect();
    if !additions.is_empty() {
        let at = out.iter().rposition(|s| matches!(s, Segment::Entry(_))).map_or(out.len(), |p| p + 1);
        out.insert(at, Segment::Free(vec![additions]));
    }

    let has_keys = out.iter().any(|s| matches!(s, Segment::Entry(_))) || !set.is_empty();
    let mut text = String::new();
    for segment in out {
        match segment {
            Segment::Entry(entry) => {
                entry.leading.iter().chain(&entry.body).for_each(|l| text.push_str(l));
            }
            // With no keys left, keep surviving comments but drop the frontmatter if there are none.
            Segment::Free(free) if !has_keys => {
                free.iter().filter(|l| classify(l) == LineKind::Comment).for_each(|l| text.push_str(l))
            }
            Segment::Free(free) => free.iter().for_each(|l| text.push_str(l)),
        }
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    Some(text)
}

/// Serialize a whole mapping as block-style YAML.
fn serialize_mapping(map: &Frontmatter) -> String {
    map.iter().map(|(key, value)| serialize_entry(key, value)).collect()
}

/// One `key: value` entry, with nested values as indented blocks.
fn serialize_entry(key: &str, value: &Value) -> String {
    let mut out = String::new();
    write_entry(&mut out, 0, key, value);
    out
}

fn write_entry(out: &mut String, indent: usize, key: &str, value: &Value) {
    out.push_str(&" ".repeat(indent));
    out.push_str(&yaml_key(key));
    out.push(':');
    match value {
        Value::Array(items) if !items.is_empty() => {
            out.push('\n');
            items.iter().for_each(|item| write_item(out, indent + 2, item));
        }
        Value::Object(map) if !map.is_empty() => {
            out.push('\n');
            map.iter().for_each(|(k, v)| write_entry(out, indent + 2, k, v));
        }
        other => {
            out.push(' ');
            out.push_str(&yaml_scalar(other));
            out.push('\n');
        }
    }
}

fn write_item(out: &mut String, indent: usize, item: &Value) {
    out.push_str(&" ".repeat(indent));
    match item {
        Value::Array(items) if !items.is_empty() => {
            out.push_str("-\n");
            items.iter().for_each(|i| write_item(out, indent + 2, i));
        }
        Value::Object(map) if !map.is_empty() => {
            out.push_str("-\n");
            map.iter().for_each(|(k, v)| write_entry(out, indent + 2, k, v));
        }
        other => {
            out.push_str("- ");
            out.push_str(&yaml_scalar(other));
            out.push('\n');
        }
    }
}

/// A scalar (or empty collection) in YAML: plain when that reads back unchanged, double-quoted otherwise.
fn yaml_scalar(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            _ => n.to_string(),
        },
        Value::String(s) if plain_reads_back(&format!("k: {s}\n"), |v| v.as_str() == Some(s)) => s.clone(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
        Value::Array(_) => "[]".to_owned(),
        Value::Object(_) => "{}".to_owned(),
    }
}

fn yaml_key(key: &str) -> String {
    let plain = format!("{key}: 0\n");
    let reads_back = !key.is_empty() && parse_mapping(&plain).is_ok_and(|map| map.len() == 1 && map.contains_key(key));
    if reads_back { key.to_owned() } else { serde_json::to_string(key).unwrap_or_default() }
}

fn plain_reads_back(probe: &str, check: impl Fn(&Value) -> bool) -> bool {
    let text = probe.trim_start_matches("k: ").trim_end_matches('\n');
    if text.is_empty() || text != text.trim() || text.contains(['\n', '\r', '\t']) {
        return false;
    }
    parse_mapping(probe).is_ok_and(|map| map.len() == 1 && map.get("k").is_some_and(&check))
}

#[cfg(test)]
mod tests;
