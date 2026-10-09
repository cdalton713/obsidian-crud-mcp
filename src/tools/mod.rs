//! The MCP tools over the vault and the search index.

pub mod list_format;
pub mod params;
mod properties;
mod retrieval;
mod semantic;

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, LazyLock};

use chrono::{DateTime, NaiveDate, NaiveDateTime};
use regex::Regex;
use serde_json::{Value, json};
use tracing::info;

use crate::mcp::{Tool, ToolError, ToolParams};
use crate::notes::{make_deep_link, select_note_range, split_frontmatter, starts_with_setext_boundary};
use crate::search::{AiSearchClient, SearchIndex};
use crate::util::{iso_timestamp, locale_cmp, now_ms};
use crate::vault::{VaultBackend, is_path_writable};
use list_format::{IndexStatus, ListingPage, ListingScope, describe_listing, describe_no_match};
use params::*;

pub use retrieval::find_line_matches;

/// Everything the tools work with.
pub struct ToolContext {
    pub vault: Arc<dyn VaultBackend>,
    pub index: Arc<SearchIndex>,
    pub vault_name: String,
    pub read_only: bool,
    pub write_folders: Option<Vec<String>>,
    pub semantic: Option<Arc<AiSearchClient>>,
}

impl ToolContext {
    /// After a write, ask the semantic index to catch up; harmless when unset.
    fn changed(&self) {
        if let Some(semantic) = &self.semantic {
            semantic.request_sync();
        }
    }

    fn writable_folders_list(&self) -> Option<String> {
        self.write_folders
            .as_ref()
            .map(|folders| folders.iter().map(|f| format!("{f}/")).collect::<Vec<_>>().join(", "))
    }

    fn deny_write(&self, path: &str) -> String {
        format!(
            "Write access denied: '{path}' is outside the writable folders ({}).",
            self.writable_folders_list().unwrap_or_default()
        )
    }

    fn can_write(&self, path: &str) -> bool {
        is_path_writable(path, self.write_folders.as_deref())
    }
}

/// Tools that change the vault; hidden in `READ_ONLY` mode.
pub const WRITE_TOOLS: [&str; 5] = ["write_note", "edit_note", "delete_note", "move_note", "update_note_properties"];

/// Claude Code persists any MCP tool result above ~50 000 characters to a file
/// and hands the model a short preview instead of the text. A tool can raise
/// its own threshold (hard ceiling 500 000) by declaring
/// `_meta["anthropic/maxResultSizeChars"]` in its `tools/list` entry; text
/// from such a tool is then also exempt from `MAX_MCP_OUTPUT_TOKENS`. See
/// <https://code.claude.com/docs/en/mcp#raise-the-limit-for-a-specific-tool>.
/// `read_note` returns whole notes, so it declares 100 000: enough for a large
/// note to arrive in one piece, small enough to keep one read inside a sane
/// context budget. Other clients ignore the key.
pub const READ_NOTE_MAX_RESULT_SIZE_CHARS: usize = 100_000;

fn tool<P, F, Fut>(ctx: &Arc<ToolContext>, name: &'static str, description: impl Into<String>, f: F) -> Tool
where
    P: ToolParams,
    F: Fn(Arc<ToolContext>, P) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String, ToolError>> + Send + 'static,
{
    let ctx = Arc::clone(ctx);
    Tool::new(name, description, move |params| f(Arc::clone(&ctx), params))
}

/// Build the tool list for this configuration.
pub fn build_tools(ctx: ToolContext) -> Vec<Tool> {
    if ctx.read_only {
        info!("READ_ONLY mode: write tools disabled ({}).", WRITE_TOOLS.join(", "));
    } else if let Some(folders) = ctx.writable_folders_list() {
        info!("WRITE_FOLDERS: writes restricted to {folders}.");
    }
    let scope_note = ctx
        .writable_folders_list()
        .map(|folders| format!(" Writes are only allowed inside: {folders}."))
        .unwrap_or_default();
    let read_only = ctx.read_only;
    let has_semantic = ctx.semantic.is_some();
    let ctx = Arc::new(ctx);
    let mut tools = vec![
        tool(&ctx, "list_tasks", "List standard Markdown checkbox tasks with text, completion state, 1-based source lines, and Obsidian URLs. Defaults to incomplete tasks. Ignores frontmatter and code; plugin-specific statuses, recurrence, and due dates are not interpreted. Follow next_cursor until null; skipped notes are reported.", retrieval::list_tasks),
        tool(&ctx, "get_note_outline", "Get document-level headings, full heading paths, and paragraph or standalone block IDs. Returns 1-based inclusive source line ranges; heading ranges include child sections. Ignores frontmatter and code. Use the returned heading paths or block IDs with read_note and edit_note.", retrieval::get_note_outline),
        tool(&ctx, "search_notes", "Search note content for a literal, single-line phrase (not regex or Obsidian query syntax). Returns one excerpt per matching line, 1-based line numbers, and URLs. Reads current content from disk, the whole vault by default; follow next_cursor while it is not null. Reports skipped notes, including notes over 1 million characters.", retrieval::search_notes),
        tool(&ctx, "read_notes", "Read up to 20 notes in requested order. Returns JSON with per-note status, Obsidian URLs, and missing_paths for notes that do not exist. max_chars caps the entire serialized response; truncated notes and omitted paths are explicit. Read omitted content with read_note.", retrieval::read_notes),
    ];
    if has_semantic {
        tools.push(tool(&ctx, "semantic_search", "Find notes by meaning, not exact wording: a ranked hybrid (embedding + keyword) search over the whole vault. Returns the best-matching passages with note path, score (0 to 1) and URL; several passages may come from one note. The index refreshes on a schedule and shortly after this server writes a note, so an edit made minutes ago may be missing; search_notes always reads current content. Use this first when you do not know the exact words.", semantic::semantic_search));
    }
    if !read_only {
        tools.push(tool(&ctx, "update_note_properties", "Set or remove top-level YAML properties in an existing note. Supports strings, numbers, booleans, null, and lists of these values. Untouched properties keep their exact text; set properties are rewritten, so their comments, quoting, and list style can change. The Markdown body stays byte-for-byte identical. Rejects malformed YAML and preserves unrelated property values. Obeys writable-folder restrictions.", properties::update_note_properties));
    }
    tools.push(
        tool(&ctx, "read_note", "Read a note or a selected heading section/block from the Obsidian vault. Returns Markdown and an Obsidian link. Omit heading and block to read the whole note; missing or ambiguous targets are rejected.", read_note)
            .with_meta(json!({ "anthropic/maxResultSizeChars": READ_NOTE_MAX_RESULT_SIZE_CHARS })),
    );
    if !read_only {
        tools.push(tool(&ctx, "write_note", format!("Write or update a note in the Obsidian vault. Creates the note if it doesn't exist. Replaces the entire content if it does — read first if you need to preserve existing content.{scope_note}"), write_note));
    }
    tools.push(tool(&ctx, "list_notes", "List markdown notes in the vault with modification timestamps. Examples: list_notes(sort_by='modified', limit=10) for 10 most recent notes. list_notes(name='meeting') to find notes by name. list_notes(folder='daily') for a specific folder. list_notes(tag='project') for notes with a specific tag. Returns up to 100 notes by default.", list_notes));
    tools.push(tool(&ctx, "list_folders", "List all folders in the vault. Use this to discover folder names before writing or listing notes. Returns the folder tree with note counts.", list_folders));
    tools.push(tool(&ctx, "list_tags", "List all tags used in the vault, sorted by frequency. Use this to discover tags before filtering with list_notes.", list_tags));
    if !read_only {
        tools.push(tool(&ctx, "edit_note", format!("Edit a note or a selected heading section/block. Use 'append' (default), 'prepend' (after frontmatter for whole notes), or 'replace' to swap old_text with new content. For replace, old_text must match exactly once within the selected content. Heading lines and block IDs are preserved. Missing or ambiguous targets are rejected.{scope_note}"), edit_note));
        tools.push(tool(&ctx, "delete_note", format!("Delete a note from the Obsidian vault.{scope_note}"), delete_note));
        tools.push(tool(&ctx, "move_note", format!("Move or rename a note. Use this to rename a note within the same folder, move it to a different folder, or both at once. Creates destination folders automatically.{scope_note}"), move_note));
    }
    tools.push(tool(&ctx, "get_note_metadata", "Get metadata about a note without reading its full content. Returns frontmatter, tags, outgoing links, backlinks (notes that link to this one), size, and timestamps. Use this to navigate the knowledge graph.", get_note_metadata));
    tools
}

/// Obsidian tag match: ignores a leading `#` and case; `project` also matches `project/sub`.
pub fn tag_matches(tags: &[String], wanted: &str) -> bool {
    let target = wanted.strip_prefix('#').unwrap_or(wanted).to_lowercase();
    tags.iter().any(|t| {
        let tag = t.strip_prefix('#').unwrap_or(t).to_lowercase();
        tag == target || tag.starts_with(&format!("{target}/"))
    })
}

static TRAILING_BREAKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:\r\n|\n|\r)*$").unwrap());

/// Pad text so it ends with a blank line.
fn with_blank_line(text: &str, eol: &str) -> String {
    let trailing = TRAILING_BREAKS.find(text).map_or("", |m| m.as_str());
    let breaks = trailing.replace("\r\n", "\n").len();
    format!("{text}{}", eol.repeat(2usize.saturating_sub(breaks)))
}

/// Parse an ISO date (`2026-03-25`) or date-time (`2026-03-25T10:00`, with
/// optional seconds and offset) into ms since the epoch. Times without an
/// offset are read as UTC.
pub fn parse_date(value: &str) -> Option<f64> {
    let value = value.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(value) {
        return Some(t.timestamp_millis() as f64);
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(value, format) {
            return Some(t.and_utc().timestamp_millis() as f64);
        }
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M%:z", "%Y-%m-%dT%H:%MZ"] {
        if let Ok(t) = DateTime::parse_from_str(value, format) {
            return Some(t.timestamp_millis() as f64);
        }
    }
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(&format!("{value}-01"), "%Y-%m-%d"))
        .or_else(|_| NaiveDate::parse_from_str(&format!("{value}-01-01"), "%Y-%m-%d"))
        .ok()?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis() as f64)
}

async fn read_note(ctx: Arc<ToolContext>, args: ReadNoteParams) -> Result<String, ToolError> {
    let Some(content) = ctx.vault.read_note(&args.path).await? else {
        return Ok(format!("Note not found: {}", args.path));
    };
    let link = make_deep_link(&ctx.vault_name, &args.path);
    match select_note_range(&content, args.heading.as_deref(), args.block.as_deref()) {
        Ok(range) => Ok(format!("[Open in Obsidian]({link})\n\n---\n\n{}", &content[range])),
        Err(error) => Ok(error.to_string()),
    }
}

async fn write_note(ctx: Arc<ToolContext>, args: WriteNoteParams) -> Result<String, ToolError> {
    let WriteNoteParams { path, content } = args;
    if !ctx.can_write(&path) {
        return Ok(ctx.deny_write(&path));
    }
    if !ctx.vault.write_note(&path, &content).await? {
        return Ok(format!("Failed to write note: {path}"));
    }
    ctx.index.update(&path, &content, Some(now_ms() as f64));
    ctx.changed();
    Ok(format!("Note saved: {path}\n[Open in Obsidian]({})", make_deep_link(&ctx.vault_name, &path)))
}

async fn list_notes(ctx: Arc<ToolContext>, args: ListNotesParams) -> Result<String, ToolError> {
    let folder = args.folder.as_deref().filter(|f| !f.is_empty());
    // Use the search index, falling back to the vault while it is empty.
    let mut notes = ctx.index.list_with_mtime(folder);
    let mut vault_total = Some(ctx.index.size());
    let mut served_by_vault = false;
    if notes.is_empty() {
        notes = ctx.vault.list_notes_with_mtime(folder).await?;
        served_by_vault = !notes.is_empty();
        // The fallback only tells us the vault total when it was unscoped.
        if vault_total == Some(0) {
            vault_total = if folder.is_some() { None } else { Some(notes.len()) };
        }
    }
    let folder_total = folder.map(|_| notes.len());
    let mut filters = Vec::new();
    if let Some(name) = args.name.as_deref().filter(|n| !n.is_empty()) {
        let lower = name.to_lowercase();
        notes.retain(|n| n.path.to_lowercase().contains(&lower));
        filters.push(format!("name=\"{name}\""));
    }
    if let Some(tag) = args.tag.as_deref().filter(|t| !t.is_empty()) {
        notes.retain(|n| tag_matches(&ctx.index.get_tags(&n.path), tag));
        filters.push(format!("tag=\"{tag}\""));
    }
    if let Some(after) = args.modified_after.as_deref().filter(|a| !a.is_empty()) {
        let Some(cutoff) = parse_date(after) else {
            return Ok(format!("Invalid date format: {after}. Use ISO format like '2026-03-25'."));
        };
        notes.retain(|n| n.mtime >= cutoff);
        filters.push(format!("modified_after=\"{after}\""));
    }
    let scope = ListingScope {
        vault_total,
        folder,
        folder_total,
        filters,
        index: IndexStatus { state: ctx.index.state(), size: ctx.index.size(), served_by_vault },
    };
    if notes.is_empty() {
        return Ok(describe_no_match(&scope));
    }
    let sort_by = args.sort_by.unwrap_or(SortBy::Name);
    if sort_by == SortBy::Modified {
        notes.sort_by(|a, b| b.mtime.total_cmp(&a.mtime));
    }
    let limit = args.limit.unwrap_or(100) as usize;
    let omitted: Vec<String> = notes.iter().skip(limit).map(|n| n.path.clone()).collect();
    let shown = &notes[..notes.len().min(limit)];
    let page = ListingPage { shown: shown.len(), matched: notes.len(), sort_by: sort_by.as_str(), limit, omitted: &omitted };
    let mut lines = vec![describe_listing(&scope, &page)];
    lines.extend(shown.iter().map(|n| {
        let date = if n.mtime != 0.0 { iso_timestamp(n.mtime)[..16].to_owned() } else { String::new() };
        format!("- {date} [{}]({})", n.path, make_deep_link(&ctx.vault_name, &n.path))
    }));
    Ok(lines.join("\n"))
}

async fn list_folders(ctx: Arc<ToolContext>, _: EmptyParams) -> Result<String, ToolError> {
    let mut paths = ctx.index.list_paths(None);
    if paths.is_empty() {
        paths = ctx.vault.list_notes(None).await?;
    }
    let mut folders: BTreeMap<String, usize> = BTreeMap::new();
    for path in &paths {
        match path.rfind('/') {
            None => *folders.entry("(root)".to_owned()).or_default() += 1,
            Some(slash) => {
                let folder = &path[..slash];
                *folders.entry(folder.to_owned()).or_default() += 1;
                // Make sure every parent folder appears in the list.
                let mut parent = folder;
                while let Some(slash) = parent.rfind('/') {
                    parent = &parent[..slash];
                    folders.entry(parent.to_owned()).or_default();
                }
            }
        }
    }
    if folders.is_empty() {
        return Ok("Vault is empty.".to_owned());
    }
    let mut sorted: Vec<(String, usize)> = folders.into_iter().collect();
    sorted.sort_by(|a, b| locale_cmp(&a.0, &b.0));
    Ok(sorted.iter().map(|(f, count)| format!("- {f} ({count} notes)")).collect::<Vec<_>>().join("\n"))
}

async fn list_tags(ctx: Arc<ToolContext>, _: EmptyParams) -> Result<String, ToolError> {
    let tags = ctx.index.list_all_tags();
    if tags.is_empty() {
        return Ok("No tags found in the vault.".to_owned());
    }
    Ok(tags.iter().map(|t| format!("- #{} ({} notes)", t.tag, t.count)).collect::<Vec<_>>().join("\n"))
}

async fn edit_note(ctx: Arc<ToolContext>, args: EditNoteParams) -> Result<String, ToolError> {
    let EditNoteParams { path, heading, block, content: new_content, operation, old_text } = args;
    if !ctx.can_write(&path) {
        return Ok(ctx.deny_write(&path));
    }
    let Some(full) = ctx.vault.read_note(&path).await? else {
        return Ok(format!("Note not found: {path}"));
    };
    let range = match select_note_range(&full, heading.as_deref(), block.as_deref()) {
        Ok(range) => range,
        Err(error) => return Ok(error.to_string()),
    };
    let existing = &full[range.clone()];
    let targeted = heading.is_some() || block.is_some();
    let eol = if targeted && full.contains("\r\n") { "\r\n" } else { "\n" };
    let op = operation.unwrap_or(EditOperation::Append);

    let mut updated = match op {
        EditOperation::Replace => {
            let Some(old_text) = old_text.filter(|t| !t.is_empty()) else {
                return Ok("old_text is required for replace operation.".to_owned());
            };
            let Some(at) = existing.find(&old_text) else {
                return Ok("old_text not found in note.".to_owned());
            };
            let after_first_char = at + existing[at..].chars().next().map_or(1, char::len_utf8);
            if existing[after_first_char..].contains(&old_text) {
                return Ok("old_text matches multiple times. Provide a longer, unique string.".to_owned());
            }
            let replaced = format!("{}{new_content}{}", &existing[..at], &existing[at + old_text.len()..]);
            // An empty block leaves its ^id marker behind, which then labels the previous block.
            if block.is_some() && replaced.trim().is_empty() {
                return Ok("Replacement would leave the block empty. Replace text that includes the ^block marker, without a block target, to delete it.".to_owned());
            }
            replaced
        }
        EditOperation::Prepend => {
            // Insert after the frontmatter, if the whole note was selected and has some.
            let frontmatter = (!targeted).then(|| split_frontmatter(existing)).filter(|fm| fm.yaml.is_some());
            if let Some(fm) = frontmatter {
                // A closing `---` at EOF has no line break to stand on.
                let gap = if fm.closing.ends_with('\n') { "" } else { fm.eol };
                format!("{}{gap}{new_content}{}{}", &existing[..fm.body_offset], fm.eol, fm.body)
            } else if heading.is_some() && !existing.is_empty() && starts_with_setext_boundary(existing) {
                with_blank_line(&new_content, eol) + existing
            } else {
                format!("{new_content}{eol}{existing}")
            }
        }
        EditOperation::Append => {
            if targeted && existing.is_empty() {
                new_content.clone()
            } else if existing.ends_with('\n') {
                format!("{existing}{new_content}")
            } else {
                format!("{existing}{eol}{new_content}")
            }
        }
    };

    let mut prefix = full[..range.start].to_owned();
    let suffix = &full[range.end..];
    // Text directly above a setext heading would become part of its title.
    if heading.is_some()
        && op != EditOperation::Replace
        && (op == EditOperation::Append || existing.is_empty())
        && !updated.is_empty()
        && !suffix.is_empty()
        && starts_with_setext_boundary(suffix)
    {
        updated = with_blank_line(&updated, eol);
    }
    let ends_with_break = |text: &str| text.ends_with(['\r', '\n']);
    if heading.is_some() && !prefix.is_empty() && !ends_with_break(&prefix) {
        prefix.push_str(eol);
    }
    if heading.is_some() && !suffix.is_empty() && !updated.is_empty() && !ends_with_break(&updated) {
        updated.push_str(eol);
    }
    let updated = format!("{prefix}{updated}{suffix}");

    if !ctx.vault.write_note(&path, &updated).await? {
        return Ok(format!("Failed to edit note: {path}"));
    }
    ctx.index.update(&path, &updated, Some(now_ms() as f64));
    ctx.changed();
    Ok(format!(
        "Note edited ({}): {path}\n[Open in Obsidian]({})",
        op.as_str(),
        make_deep_link(&ctx.vault_name, &path)
    ))
}

async fn delete_note(ctx: Arc<ToolContext>, args: DeleteNoteParams) -> Result<String, ToolError> {
    let path = args.path;
    if !ctx.can_write(&path) {
        return Ok(ctx.deny_write(&path));
    }
    // The sync layer can report success even when nothing existed at the path,
    // so "Deleted" would claim a cleanup that never happened. read_note returns
    // "" for an empty note and None only when absent.
    if ctx.vault.read_note(&path).await?.is_none() {
        return Ok(format!("Note not found: {path}"));
    }
    if !ctx.vault.delete_note(&path).await? {
        return Ok(format!("Failed to delete: {path}"));
    }
    ctx.index.remove(&path);
    ctx.changed();
    Ok(format!("Deleted: {path}"))
}

async fn move_note(ctx: Arc<ToolContext>, args: MoveNoteParams) -> Result<String, ToolError> {
    let MoveNoteParams { from, to } = args;
    // Moving out of a folder deletes there; moving in writes there: both ends must be writable.
    if !ctx.can_write(&from) {
        return Ok(ctx.deny_write(&from));
    }
    if !ctx.can_write(&to) {
        return Ok(ctx.deny_write(&to));
    }
    let content = ctx.vault.read_note(&from).await?;
    if !ctx.vault.move_note(&from, &to).await? {
        return Ok(format!("Failed to move: {from} → {to}"));
    }
    ctx.index.remove(&from);
    // An empty note is still a note: keep it indexed at the new path.
    if let Some(content) = content {
        ctx.index.update(&to, &content, Some(now_ms() as f64));
    }
    ctx.changed();
    Ok(format!("Moved: {from} → {to}\n[Open in Obsidian]({})", make_deep_link(&ctx.vault_name, &to)))
}

async fn get_note_metadata(ctx: Arc<ToolContext>, args: GetNoteMetadataParams) -> Result<String, ToolError> {
    let path = args.path;
    let Some(info) = ctx.vault.get_metadata(&path).await? else {
        return Ok(format!("Note not found: {path}"));
    };
    let mut lines = vec![
        format!("**{path}**"),
        format!("Size: {} bytes", info.size),
        format!("Created: {}", iso_timestamp(info.ctime)),
        format!("Modified: {}", iso_timestamp(info.mtime)),
    ];
    let meta = &info.metadata;
    if !meta.frontmatter.is_empty() {
        lines.push("\nFrontmatter:".to_owned());
        for (key, value) in &meta.frontmatter {
            let shown = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            lines.push(format!("  {key}: {shown}"));
        }
    }
    if !meta.tags.is_empty() {
        let tags: Vec<String> = meta.tags.iter().map(|t| format!("#{t}")).collect();
        lines.push(format!("\nTags: {}", tags.join(", ")));
    }
    if !meta.links.is_empty() {
        lines.push(format!("\nOutgoing links: {}", meta.links.join(", ")));
    }
    let backlinks = ctx.index.get_backlinks(&path);
    if !backlinks.is_empty() {
        lines.push(format!("\nBacklinks: {}", backlinks.join(", ")));
    }
    lines.push(format!("\n[Open in Obsidian]({})", make_deep_link(&ctx.vault_name, &path)));
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests;
