//! Tool arguments: deserialized from the call, described to clients as JSON Schema.

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::mcp::ToolParams;
use crate::notes::ScanParameters;

/// Arguments for tools that take none.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct EmptyParams {}

impl ToolParams for EmptyParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadNoteParams {
    /// Vault-relative path to the note, e.g. 'daily/2026-03-23.md'
    pub path: String,
    /// Exact full heading path from get_note_outline. Selects section content, including child headings, but excludes the selected heading line.
    #[schemars(length(min = 1, max = 6))]
    #[serde(default)]
    pub heading: Option<Vec<String>>,
    /// Block ID without ^, from get_note_outline. Selects content without the block marker. Cannot be combined with heading.
    #[schemars(regex(pattern = r"^[A-Za-z0-9-]+$"))]
    #[serde(default)]
    pub block: Option<String>,
}

impl ToolParams for ReadNoteParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteNoteParams {
    /// Vault-relative path to the note, e.g. 'daily/2026-03-23.md'
    pub path: String,
    /// Full markdown content for the note
    pub content: String,
}

impl ToolParams for WriteNoteParams {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SortBy {
    Name,
    Modified,
}

impl SortBy {
    pub fn as_str(self) -> &'static str {
        match self {
            SortBy::Name => "name",
            SortBy::Modified => "modified",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListNotesParams {
    /// Folder to filter by, e.g. 'daily' or 'projects'. Omit for all notes.
    #[serde(default)]
    pub folder: Option<String>,
    /// Filter by name (case-insensitive substring match on path), e.g. 'meeting' or 'project-x'.
    #[serde(default)]
    pub name: Option<String>,
    /// Filter by tag, e.g. 'project' or 'daily'. Use list_tags to discover available tags.
    #[serde(default)]
    pub tag: Option<String>,
    /// Sort order: 'name' (default) or 'modified' (most recent first).
    #[serde(default)]
    pub sort_by: Option<SortBy>,
    /// Only include notes modified after this ISO date, e.g. '2026-03-25' or '2026-03-25T10:00'.
    #[serde(default)]
    pub modified_after: Option<String>,
    /// Max number of notes to return. Default 100.
    #[schemars(schema_with = "lenient_limit_schema")]
    #[serde(default, deserialize_with = "lenient_integer")]
    pub limit: Option<u32>,
}

impl ToolParams for ListNotesParams {
    fn check(&self) -> Result<(), String> {
        match self.limit {
            Some(limit) if !(1..=10_000).contains(&limit) => Err("limit must be between 1 and 10000".to_owned()),
            _ => Ok(()),
        }
    }
}

/// An integer, also accepted as a numeric string (some clients send every argument as text).
fn lenient_limit_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "description": "Max number of notes to return. Default 100.",
        "type": ["integer", "string"],
        "minimum": 1,
        "maximum": 10000,
        "pattern": "^\\s*\\d+\\s*$"
    })
}

fn lenient_integer<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    use serde::de::Error;
    match Option::<Value>::deserialize(deserializer)? {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| D::Error::custom("limit must be a whole number")),
        Some(Value::String(s)) => {
            s.trim().parse().map(Some).map_err(|_| D::Error::custom("limit must be a whole number"))
        }
        Some(_) => Err(D::Error::custom("limit must be a whole number")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EditOperation {
    Append,
    Prepend,
    Replace,
}

impl EditOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            EditOperation::Append => "append",
            EditOperation::Prepend => "prepend",
            EditOperation::Replace => "replace",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditNoteParams {
    /// Vault-relative path to the note, e.g. 'daily/2026-03-25.md'
    pub path: String,
    /// Exact full heading path from get_note_outline. Selects section content, including child headings, but excludes the selected heading line.
    #[schemars(length(min = 1, max = 6))]
    #[serde(default)]
    pub heading: Option<Vec<String>>,
    /// Block ID without ^, from get_note_outline. Selects content without the block marker. Cannot be combined with heading.
    #[schemars(regex(pattern = r"^[A-Za-z0-9-]+$"))]
    #[serde(default)]
    pub block: Option<String>,
    /// Text to append, prepend, or use as replacement for old_text
    pub content: String,
    /// 'append' (default): add to end. 'prepend': add after frontmatter. 'replace': swap old_text with content.
    #[serde(default)]
    pub operation: Option<EditOperation>,
    /// Required for replace operation. Exact text to find and replace. Must match exactly once.
    #[serde(default)]
    pub old_text: Option<String>,
}

impl ToolParams for EditNoteParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteNoteParams {
    /// Vault-relative path to the note to delete
    pub path: String,
}

impl ToolParams for DeleteNoteParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MoveNoteParams {
    /// Current path, e.g. 'daily/old-name.md'
    pub from: String,
    /// New path, e.g. 'projects/new-name.md'
    pub to: String,
}

impl ToolParams for MoveNoteParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetNoteMetadataParams {
    /// Vault-relative path to the note, e.g. 'projects/my-project.md'
    pub path: String,
}

impl ToolParams for GetNoteMetadataParams {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    #[default]
    Incomplete,
    Completed,
    All,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Incomplete => "incomplete",
            TaskStatus::Completed => "completed",
            TaskStatus::All => "all",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTasksParams {
    #[serde(flatten)]
    pub scan: ScanParameters,
    #[serde(default)]
    pub status: TaskStatus,
}

impl ToolParams for ListTasksParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetNoteOutlineParams {
    #[schemars(length(min = 1, max = 1000))]
    pub path: String,
}

impl ToolParams for GetNoteOutlineParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchNotesParams {
    #[serde(flatten)]
    pub scan: ScanParameters,
    #[schemars(length(min = 1, max = 200))]
    pub query: String,
    #[serde(default)]
    pub case_sensitive: bool,
}

impl ToolParams for SearchNotesParams {
    fn check(&self) -> Result<(), String> {
        if self.query.trim().is_empty() || self.query.contains(['\r', '\n']) {
            return Err("query: Use a non-empty single-line phrase.".to_owned());
        }
        Ok(())
    }
}

fn default_max_chars() -> u32 {
    20_000
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadNotesParams {
    #[schemars(length(min = 1, max = 20), inner(length(min = 1, max = 1000)))]
    pub paths: Vec<String>,
    #[schemars(range(min = 1024, max = 50_000))]
    #[serde(default = "default_max_chars")]
    pub max_chars: u32,
}

impl ToolParams for ReadNotesParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateNotePropertiesParams {
    #[schemars(length(min = 1, max = 1000))]
    pub path: String,
    #[schemars(schema_with = "property_map_schema")]
    #[serde(default)]
    pub set: Map<String, Value>,
    #[schemars(length(max = 100), inner(length(min = 1, max = 200)))]
    #[serde(default)]
    pub remove: Vec<String>,
}

fn property_map_schema(_: &mut SchemaGenerator) -> Schema {
    let scalar = serde_json::json!({ "type": ["string", "number", "boolean", "null"] });
    json_schema!({
        "type": "object",
        "default": {},
        "propertyNames": { "minLength": 1, "maxLength": 200 },
        "additionalProperties": {
            "anyOf": [scalar, { "type": "array", "items": scalar, "maxItems": 1000 }]
        }
    })
}

fn is_scalar(value: &Value) -> bool {
    match value {
        Value::Number(n) => n.as_f64().is_some_and(f64::is_finite),
        Value::String(_) | Value::Bool(_) | Value::Null => true,
        _ => false,
    }
}

impl ToolParams for UpdateNotePropertiesParams {
    fn check(&self) -> Result<(), String> {
        if self.set.keys().chain(&self.remove).any(|key| key.trim().is_empty()) {
            return Err("Property names cannot be blank.".to_owned());
        }
        let valid = self.set.values().all(|value| match value {
            Value::Array(items) => items.len() <= 1000 && items.iter().all(is_scalar),
            other => is_scalar(other),
        });
        if !valid {
            return Err("Property values must be strings, numbers, booleans, null, or lists of these.".to_owned());
        }
        if self.set.is_empty() && self.remove.is_empty() {
            return Err("Supply properties to set or remove.".to_owned());
        }
        Ok(())
    }
}

fn default_semantic_limit() -> u32 {
    10
}

fn default_min_score() -> f64 {
    0.4
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SemanticSearchParams {
    /// What you are looking for, in plain language. Meaning matters more than exact words.
    #[schemars(length(min = 1, max = 500))]
    pub query: String,
    /// Only notes inside this folder and its descendants.
    #[schemars(length(max = 1000))]
    #[serde(default)]
    pub folder: Option<String>,
    /// Passages to return, 1 to 50.
    #[schemars(range(min = 1, max = 50))]
    #[serde(default = "default_semantic_limit")]
    pub limit: u32,
    /// Drop passages scoring below this (0 to 1).
    #[schemars(range(min = 0.0, max = 1.0))]
    #[serde(default = "default_min_score")]
    pub min_score: f64,
}

impl ToolParams for SemanticSearchParams {}
