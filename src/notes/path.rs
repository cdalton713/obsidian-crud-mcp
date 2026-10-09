//! Shared validation for note paths, applied by the local and S3 backends
//! before any read, write, delete or move.
//!
//! The note model is markdown-only: `list_notes` only ever surfaces `*.md`
//! files, and binary attachments are not supported. Enforcing that here keeps
//! the tools from reaching anything that isn't a note: a path like
//! `.obsidian/plugins/x/main.js` would otherwise write executable plugin code
//! into the vault (and sync it to every device), and
//! `.obsidian/plugins/remotely-save/data.json` would expose the bucket keys.
//!
//! Rules: vault-relative, ends in `.md`, no `..`, no leading `/`, no `:`
//! (Obsidian rejects it in file names on Windows, iOS and Android), no NUL, and
//! no path segment (split on either slash) that starts with `.` (hidden files
//! and the `.obsidian` config dir).

use thiserror::Error;

/// Why a path is not an acceptable note path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InvalidNotePath {
    #[error("Invalid note path")]
    Malformed,
    #[error("Invalid note path: ':' is not allowed")]
    Colon,
    #[error("Invalid note path: must be a vault-relative path ending in .md")]
    NotMarkdown,
    #[error("Invalid note path: empty path segment")]
    EmptySegment,
    #[error("Invalid note path: dot-folders and hidden files (e.g. .obsidian) are not allowed")]
    Hidden,
}

/// Check that `path` names a note inside the vault.
pub fn validate_note_path(path: &str) -> Result<(), InvalidNotePath> {
    if path.is_empty() || path.encode_utf16().count() > 1000 {
        return Err(InvalidNotePath::Malformed);
    }
    if path.starts_with('/') || path.contains('\0') || path.contains("..") {
        return Err(InvalidNotePath::Malformed);
    }
    if path.contains(':') {
        return Err(InvalidNotePath::Colon);
    }
    if !path.ends_with(".md") {
        return Err(InvalidNotePath::NotMarkdown);
    }
    for segment in path.split(['/', '\\']) {
        if segment.is_empty() {
            return Err(InvalidNotePath::EmptySegment);
        }
        if segment.starts_with('.') {
            return Err(InvalidNotePath::Hidden);
        }
    }
    Ok(())
}

/// Boolean form of [`validate_note_path`], for filtering listings and the index.
pub fn is_valid_note_path(path: &str) -> bool {
    validate_note_path(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_notes() {
        for path in ["a.md", "daily/2026-03-23.md", "deep/nested/folder/note.md", "with space.md"] {
            assert!(is_valid_note_path(path), "{path}");
        }
    }

    #[test]
    fn rejects_non_notes() {
        let cases = [
            ("", InvalidNotePath::Malformed),
            ("/abs.md", InvalidNotePath::Malformed),
            ("../up.md", InvalidNotePath::Malformed),
            ("a/../b.md", InvalidNotePath::Malformed),
            ("nul\0.md", InvalidNotePath::Malformed),
            ("C:/x.md", InvalidNotePath::Colon),
            ("a.txt", InvalidNotePath::NotMarkdown),
            (".obsidian/plugins/x/main.js", InvalidNotePath::NotMarkdown),
            ("a//b.md", InvalidNotePath::EmptySegment),
            (".obsidian/x.md", InvalidNotePath::Hidden),
            ("a\\.hidden\\b.md", InvalidNotePath::Hidden),
            (".hidden.md", InvalidNotePath::Hidden),
        ];
        for (path, expected) in cases {
            assert_eq!(validate_note_path(path), Err(expected), "{path:?}");
        }
        assert!(!is_valid_note_path(&format!("{}.md", "a".repeat(1000))));
    }
}
