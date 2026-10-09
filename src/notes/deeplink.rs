use crate::util::encode_uri_component;

/// Obsidian deep link for a note, e.g. `obsidian://open?vault=My%20Vault&file=daily%2Fnote`.
/// Works on macOS and iOS.
pub fn make_deep_link(vault_name: &str, note_path: &str) -> String {
    let clean = note_path.strip_suffix(".md").unwrap_or(note_path);
    format!("obsidian://open?vault={}&file={}", encode_uri_component(vault_name), encode_uri_component(clean))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_extension_and_encodes() {
        assert_eq!(
            make_deep_link("My Vault", "daily/2026-03-23.md"),
            "obsidian://open?vault=My%20Vault&file=daily%2F2026-03-23"
        );
    }

    #[test]
    fn keeps_inner_md() {
        assert_eq!(make_deep_link("V", "a.md/b.md"), "obsidian://open?vault=V&file=a.md%2Fb");
    }

    #[test]
    fn encodes_special_characters() {
        assert_eq!(make_deep_link("V", "Q&A #1?.md"), "obsidian://open?vault=V&file=Q%26A%20%231%3F");
    }

    #[test]
    fn path_without_extension_is_used_as_is() {
        assert_eq!(make_deep_link("MyVault", "notes/hello"), "obsidian://open?vault=MyVault&file=notes%2Fhello");
    }

    #[test]
    fn encodes_spaces_and_slashes_in_path() {
        assert_eq!(
            make_deep_link("V", "folder/sub folder/note.md"),
            "obsidian://open?vault=V&file=folder%2Fsub%20folder%2Fnote"
        );
    }

    #[test]
    fn empty_path_gives_empty_file() {
        assert_eq!(make_deep_link("V", ""), "obsidian://open?vault=V&file=");
    }

    #[test]
    fn only_trailing_extension_is_stripped() {
        assert_eq!(make_deep_link("V", "my.md.backup/note.md"), "obsidian://open?vault=V&file=my.md.backup%2Fnote");
    }
}
