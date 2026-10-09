//! Response wording for `list_notes`. Kept free of MCP and vault types so it
//! can be unit-tested: the goal is that an LLM client can never mistake a
//! truncated, filtered or still-indexing listing for "the vault contains
//! nothing else".

use std::collections::HashMap;

use crate::search::IndexState;
use crate::util::locale_cmp;

/// Folders named in an "Omitted" summary before the rest are counted together.
pub const MAX_NAMED_FOLDERS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStatus {
    pub state: IndexState,
    pub size: usize,
    /// The notes were read straight from the vault because the index had none,
    /// so the list itself is complete.
    pub served_by_vault: bool,
}

/// What was listed, before paging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingScope<'a> {
    /// Notes in the whole vault, before any filter; `None` when unknown.
    pub vault_total: Option<usize>,
    /// Folder filter as given by the caller, if any.
    pub folder: Option<&'a str>,
    /// Notes inside `folder` before the other filters; `None` when unknown.
    pub folder_total: Option<usize>,
    /// Non-folder filters that were applied, e.g. `name="x"`, `tag="y"`.
    pub filters: Vec<String>,
    pub index: IndexStatus,
}

/// The page of a non-empty listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingPage<'a> {
    /// Notes actually returned (after the limit).
    pub shown: usize,
    /// Notes matching all filters (before the limit).
    pub matched: usize,
    pub sort_by: &'a str,
    pub limit: usize,
    /// Paths cut by the limit, used to name the omitted folders.
    pub omitted: &'a [String],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderCount {
    pub folder: String,
    pub count: usize,
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn folder_label(folder: &str) -> String {
    if folder.ends_with('/') { folder.to_owned() } else { format!("{folder}/") }
}

/// Group paths by their immediate parent folder; root-level notes count under "(root)".
pub fn count_by_folder(paths: &[String]) -> Vec<FolderCount> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for path in paths {
        let folder = path.rfind('/').map_or("(root)", |slash| &path[..slash]);
        *counts.entry(folder).or_default() += 1;
    }
    let mut groups: Vec<FolderCount> =
        counts.into_iter().map(|(folder, count)| FolderCount { folder: folder.to_owned(), count }).collect();
    groups.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| locale_cmp(&a.folder, &b.folder)));
    groups
}

/// "Omitted 106: a/ (41), b/ (9), and 3 more folders (56 notes)." The counts always add up to the total.
pub fn describe_omitted(omitted: &[String]) -> String {
    let groups = count_by_folder(omitted);
    let (named, rest) = groups.split_at(groups.len().min(MAX_NAMED_FOLDERS));
    let parts: Vec<String> = named
        .iter()
        .map(|g| {
            let label = if g.folder == "(root)" { g.folder.clone() } else { folder_label(&g.folder) };
            format!("{label} ({})", g.count)
        })
        .collect();
    let mut text = format!("Omitted {}: {}", omitted.len(), parts.join(", "));
    if !rest.is_empty() {
        let rest_notes: usize = rest.iter().map(|g| g.count).sum();
        text.push_str(&format!(", and {} ({})", plural(rest.len(), "more folder"), plural(rest_notes, "note")));
    }
    text + "."
}

/// Appended to every response while the index is not known to be complete.
pub fn describe_index_state(index: &IndexStatus) -> String {
    let caveat = if index.served_by_vault {
        "this list was read directly from the vault"
    } else {
        "this list may be incomplete"
    };
    match index.state {
        IndexState::Ready => String::new(),
        IndexState::Building => {
            format!(" Index: catching up ({} indexed so far); {caveat}.", plural(index.size, "note"))
        }
        IndexState::Failed => format!(
            " Index: rebuild failed at startup (see server log); {}.",
            if index.served_by_vault { caveat } else { "this list may be incomplete or stale" }
        ),
    }
}

/// "vault has N notes" / "folder has N notes", only when the number can be
/// trusted: the index is ready, or the notes came straight from the vault.
/// While the index is still building, its size is a partial count and the
/// index caveat carries it instead.
fn describe_scope_count(s: &ListingScope<'_>) -> String {
    if s.index.state != IndexState::Ready && !s.index.served_by_vault {
        return String::new();
    }
    match (s.folder, s.folder_total, s.vault_total) {
        (Some(_), Some(total), _) => format!("folder has {}", plural(total, "note")),
        (Some(_), None, _) => String::new(),
        (None, _, Some(total)) => format!("vault has {}", plural(total, "note")),
        (None, _, None) => String::new(),
    }
}

/// Response for a listing with zero matches.
pub fn describe_no_match(s: &ListingScope<'_>) -> String {
    let index_note = describe_index_state(&s.index);
    if s.filters.is_empty() {
        return match s.folder {
            Some(folder) => format!("No notes found in folder: {folder}{index_note}"),
            None => format!("Vault is empty.{index_note}"),
        };
    }
    let count = describe_scope_count(s);
    let mut scope = s.folder.map(|f| format!(" in folder \"{}\"", folder_label(f))).unwrap_or_default();
    if !count.is_empty() {
        scope.push_str(&format!(" ({count})"));
    }
    format!("No notes match {}{scope}.{index_note}", s.filters.join(", "))
}

/// First line of a non-empty listing. Always present, so the index state and
/// the filter scope are visible even when nothing was cut.
pub fn describe_listing(s: &ListingScope<'_>, page: &ListingPage<'_>) -> String {
    let truncated = page.matched > page.shown;
    // "5 notes match name=…" reads as a sentence; "Showing 100 of 150 notes matching …" needs the participle.
    let verb = if truncated {
        "matching"
    } else if page.matched == 1 {
        "matches"
    } else {
        "match"
    };
    let filter_clause = if s.filters.is_empty() { String::new() } else { format!(" {verb} {}", s.filters.join(", ")) };
    let folder_clause = s.folder.map(|f| format!(" in folder \"{}\"", folder_label(f))).unwrap_or_default();
    let mut scope_parts: Vec<String> = Vec::new();
    if !s.filters.is_empty() || s.folder.is_some() {
        let count = describe_scope_count(s);
        if !count.is_empty() {
            scope_parts.push(count);
        }
    }
    // Every source (index, vault walk) sorts by path, so "sorted by name" is literal.
    scope_parts.push(format!("sorted by {}", page.sort_by));
    if truncated {
        scope_parts.push(format!("limit={}", page.limit));
    }
    let scope = format!(" ({})", scope_parts.join(", "));

    let mut line = if truncated {
        format!("Showing {} of {}{filter_clause}{folder_clause}{scope}.", page.shown, plural(page.matched, "note"))
    } else {
        format!("{}{filter_clause}{folder_clause}{scope}.", plural(page.matched, "note"))
    };
    if truncated {
        let hint = if s.folder.is_some() || !s.filters.is_empty() {
            "Raise `limit` or narrow the filter."
        } else {
            "Raise `limit` or add a `folder` filter."
        };
        line.push_str(&format!(" {} {hint}", describe_omitted(page.omitted)));
    }
    line + &describe_index_state(&s.index)
}

#[cfg(test)]
mod tests;
