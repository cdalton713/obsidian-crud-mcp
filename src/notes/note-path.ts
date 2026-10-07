/**
 * Shared validation for note paths, applied by the local and S3 backends
 * before any read, write, delete or move.
 *
 * The note model is markdown-only: `list_notes` only ever surfaces `*.md`
 * files, and binary attachments are not supported. Enforcing that here keeps
 * the tools from reaching anything that isn't a note: a path like
 * `.obsidian/plugins/x/main.js` would otherwise write executable plugin code
 * into the vault (and sync it to every device), and
 * `.obsidian/plugins/remotely-save/data.json` would expose the bucket keys.
 *
 * Rules: vault-relative, ends in `.md`, no `..`, no leading `/`, no `:`
 * (Obsidian rejects it in file names on Windows, iOS and Android), no NUL, and
 * no path segment (split on either slash) that starts with `.` (hidden files
 * and the `.obsidian` config dir).
 */

export function validateNotePath(path: string): void {
    if (!path || path.length > 1000) {
        throw new Error("Invalid note path");
    }
    if (path.startsWith("/") || path.includes("\0") || path.includes("..")) {
        throw new Error("Invalid note path");
    }
    if (path.includes(":")) {
        throw new Error("Invalid note path: ':' is not allowed");
    }
    if (!path.endsWith(".md")) {
        throw new Error("Invalid note path: must be a vault-relative path ending in .md");
    }
    for (const segment of path.split(/[/\\]/)) {
        if (segment === "") {
            throw new Error("Invalid note path: empty path segment");
        }
        if (segment.startsWith(".")) {
            throw new Error(
                "Invalid note path: dot-folders and hidden files (e.g. .obsidian) are not allowed",
            );
        }
    }
}

/** Boolean form of validateNotePath, for filtering listings and the index. */
export function isValidNotePath(path: string): boolean {
    try {
        validateNotePath(path);
        return true;
    } catch {
        return false;
    }
}
