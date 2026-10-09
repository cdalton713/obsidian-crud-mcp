//! Folder-scoped write access (`WRITE_FOLDERS`). When set, write tools only
//! accept paths inside the listed vault-relative folders.

/// Parse `WRITE_FOLDERS` into a normalized folder list, or `None` for unrestricted.
pub fn parse_write_folders(raw: Option<&str>) -> Option<Vec<String>> {
    let folders: Vec<String> =
        raw?.split(',').map(|f| f.trim().trim_matches('/').to_owned()).filter(|f| !f.is_empty()).collect();
    (!folders.is_empty()).then_some(folders)
}

/// Whether a vault-relative path falls inside one of the writable folders.
/// Matching is case-sensitive and folder-boundary-aware (`MCP` matches
/// `MCP/x.md`, not `MCP-private/x.md`). `None` means writes are unrestricted.
pub fn is_path_writable(path: &str, write_folders: Option<&[String]>) -> bool {
    let Some(folders) = write_folders else {
        return true;
    };
    let normalized = path.trim_start_matches('/');
    // Backends reject traversal too; deny here so the scope check can't be reasoned around.
    // Split on backslash as well: on Windows "MCP/..\x" resolves as traversal.
    if normalized.split(['/', '\\']).any(|segment| segment == "..") {
        return false;
    }
    folders.iter().any(|folder| normalized.strip_prefix(folder.as_str()).is_some_and(|rest| rest.starts_with('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folders(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_write_folders() {
        assert_eq!(parse_write_folders(None), None);
        assert_eq!(parse_write_folders(Some("")), None);
        assert_eq!(parse_write_folders(Some(" , /")), None);
        assert_eq!(parse_write_folders(Some("MCP, /inbox/ ,a/b")), Some(folders(&["MCP", "inbox", "a/b"])));
    }

    #[test]
    fn unrestricted_without_folders() {
        assert!(is_path_writable("anything.md", None));
    }

    #[test]
    fn respects_folder_boundaries() {
        let scope = folders(&["MCP", "a/b"]);
        let scope = Some(scope.as_slice());
        assert!(is_path_writable("MCP/x.md", scope));
        assert!(is_path_writable("/MCP/x.md", scope));
        assert!(is_path_writable("a/b/c/d.md", scope));
        assert!(!is_path_writable("MCP-private/x.md", scope));
        assert!(!is_path_writable("MCP", scope));
        assert!(!is_path_writable("mcp/x.md", scope));
        assert!(!is_path_writable("a/x.md", scope));
        assert!(!is_path_writable("MCP/../x.md", scope));
        assert!(!is_path_writable("MCP/..\\x.md", scope));
    }
}
