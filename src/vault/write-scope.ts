// Folder-scoped write access (WRITE_FOLDERS env var).
// When set, write tools only accept paths inside the listed vault-relative folders.

/** Parse the WRITE_FOLDERS env var into a normalized folder list, or null for unrestricted. */
export function parseWriteFolders(raw: string | undefined): string[] | null {
    if (raw === undefined) return null;
    const folders = raw
        .split(",")
        .map((f) => f.trim().replace(/^\/+|\/+$/g, ""))
        .filter(Boolean);
    return folders.length > 0 ? folders : null;
}

/**
 * Check whether a vault-relative path falls inside one of the writable folders.
 * Matching is case-sensitive and folder-boundary-aware ("MCP" matches "MCP/x.md",
 * not "MCP-private/x.md"). A null folder list means writes are unrestricted.
 */
export function isPathWritable(path: string, writeFolders: string[] | null): boolean {
    if (writeFolders === null) return true;
    const normalized = path.replace(/^\/+/, "");
    // Backends reject traversal too; deny here so the scope check can't be reasoned around.
    // Split on backslash as well: on Windows "MCP/..\x" resolves as traversal.
    if (normalized.split(/[/\\]/).includes("..")) return false;
    return writeFolders.some((folder) => normalized.startsWith(folder + "/"));
}
