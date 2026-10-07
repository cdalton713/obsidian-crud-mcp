import type { IndexStatus, ListingSummary, FolderCount } from "../types/list-format.js";

/**
 * Response wording for list_notes (issue #19). Kept free of MCP/vault types so
 * it can be unit-tested: the goal is that an LLM client can never mistake a
 * truncated, filtered or still-indexing listing for "the vault contains
 * nothing else".
 */

export const MAX_NAMED_FOLDERS = 10;

function plural(n: number, word: string): string {
    return `${n} ${word}${n === 1 ? "" : "s"}`;
}

function folderLabel(folder: string): string {
    return folder.endsWith("/") ? folder : folder + "/";
}

/** Group paths by their immediate parent folder; root-level notes count under "(root)". */
export function countByFolder(paths: string[]): FolderCount[] {
    const counts = new Map<string, number>();
    for (const p of paths) {
        const lastSlash = p.lastIndexOf("/");
        const folder = lastSlash === -1 ? "(root)" : p.slice(0, lastSlash);
        counts.set(folder, (counts.get(folder) ?? 0) + 1);
    }
    return [...counts.entries()]
        .map(([folder, count]) => ({ folder, count }))
        .sort((a, b) => b.count - a.count || a.folder.localeCompare(b.folder));
}

/** "Omitted 106: a/ (41), b/ (9), … and 3 more folders (56 notes)." — the counts always add up to the total. */
export function describeOmitted(omitted: string[]): string {
    const groups = countByFolder(omitted);
    const named = groups.slice(0, MAX_NAMED_FOLDERS);
    const rest = groups.slice(MAX_NAMED_FOLDERS);
    const parts = named.map(
        (g) => `${g.folder === "(root)" ? "(root)" : folderLabel(g.folder)} (${g.count})`,
    );
    let text = `Omitted ${omitted.length}: ${parts.join(", ")}`;
    if (rest.length > 0) {
        const restNotes = rest.reduce((sum, g) => sum + g.count, 0);
        text += `, and ${plural(rest.length, "more folder")} (${plural(restNotes, "note")})`;
    }
    return text + ".";
}

/** Appended to every response while the index is not known to be complete. */
export function describeIndexState(index: IndexStatus): string {
    const caveat = index.servedByVault
        ? "this list was read directly from the vault"
        : "this list may be incomplete";
    switch (index.state) {
        case "ready":
            return "";
        case "building":
            return ` Index: catching up (${plural(index.size, "note")} indexed so far); ${caveat}.`;
        case "failed":
            return ` Index: rebuild failed at startup (see server log); ${index.servedByVault ? caveat : "this list may be incomplete or stale"}.`;
    }
}

/**
 * "vault has N notes" / "folder has N notes" — only when the number can be
 * trusted: the index is ready, or the notes came straight from the vault.
 * While the index is still building, its size is a partial count and the
 * index caveat carries it instead.
 */
function describeScopeCount(
    s: Pick<ListingSummary, "vaultTotal" | "folder" | "folderTotal" | "index">,
): string {
    if (s.index.state !== "ready" && !s.index.servedByVault) return "";
    if (s.folder) return s.folderTotal != null ? `folder has ${plural(s.folderTotal, "note")}` : "";
    return s.vaultTotal != null ? `vault has ${plural(s.vaultTotal, "note")}` : "";
}

/** Response for a listing with zero matches. */
export function describeNoMatch(
    s: Omit<ListingSummary, "shown" | "matched" | "omitted" | "sortBy" | "limit">,
): string {
    const indexNote = describeIndexState(s.index);
    if (s.filters.length === 0) {
        if (s.folder) return `No notes found in folder: ${s.folder}${indexNote}`;
        return `Vault is empty.${indexNote}`;
    }
    const count = describeScopeCount(s);
    const scope =
        (s.folder ? ` in folder "${folderLabel(s.folder)}"` : "") + (count ? ` (${count})` : "");
    return `No notes match ${s.filters.join(", ")}${scope}.${indexNote}`;
}

/**
 * First line of a non-empty listing. Always present, so the index state and
 * the filter scope are visible even when nothing was cut.
 */
export function describeListing(s: ListingSummary): string {
    const truncated = s.matched > s.shown;
    // "5 notes match name=…" reads as a sentence; "Showing 100 of 150 notes matching …" needs the participle.
    const verb = truncated ? "matching" : s.matched === 1 ? "matches" : "match";
    const filterClause = s.filters.length > 0 ? ` ${verb} ${s.filters.join(", ")}` : "";
    const folderClause = s.folder ? ` in folder "${folderLabel(s.folder)}"` : "";
    const scopeParts: string[] = [];
    const count = s.filters.length > 0 || s.folder ? describeScopeCount(s) : "";
    if (count) scopeParts.push(count);
    // Every source (index, local glob) sorts by path with localeCompare, so "sorted by name" is literal.
    scopeParts.push(`sorted by ${s.sortBy}`);
    if (truncated) scopeParts.push(`limit=${s.limit}`);
    const scope = ` (${scopeParts.join(", ")})`;

    let line = truncated
        ? `Showing ${s.shown} of ${plural(s.matched, "note")}${filterClause}${folderClause}${scope}.`
        : `${plural(s.matched, "note")}${filterClause}${folderClause}${scope}.`;
    if (truncated) {
        const hint =
            s.folder || s.filters.length > 0
                ? "Raise `limit` or narrow the filter."
                : "Raise `limit` or add a `folder` filter.";
        line += ` ${describeOmitted(s.omitted)} ${hint}`;
    }
    return line + describeIndexState(s.index);
}
