import { createHash } from "node:crypto";
import { CursorSchema } from "../types/note-scan.js";
import type { ScanParameters, LineMatch, Cursor } from "../types/note-scan.js";
import type { IndexState } from "../types/search.js";
import type { VaultBackend } from "../vault/vault-backend.js";
import { isValidNotePath } from "./note-path.js";
import { makeDeepLink } from "./deeplink.js";
import { parseFrontmatterAndLinks } from "./parse.js";

/** Notes read at once. Reads come from local disk, so this mostly hides syscall latency. */
export const SCAN_READ_CONCURRENCY = 16;
/** A page stops before a note that would take it past this many characters. */
export const SCAN_PAGE_CHARS = 50_000_000;
/** Notes longer than this are reported in skipped_notes instead of scanned. */
export const SCAN_NOTE_MAX_CHARS = 1_000_000;

/** The slice of SearchIndex a scan uses to pick candidates and serve content without touching the disk. */
export interface ScanIndex {
    readonly state: IndexState;
    listPaths(folder?: string): string[];
    getTags(path: string): string[];
    getContent(path: string): string | undefined;
    cacheContent(path: string, content: string): void;
}

export interface ScanOptions {
    /** When ready, supplies the candidates (folder and tag already applied) instead of a listing. */
    index?: ScanIndex;
    pageChars?: number;
    concurrency?: number;
}

type ReadOutcome = { content: string | null } | { error: true };

/**
 * Which notes a scan should read, in cursor order (code-unit sort, so `<`
 * comparisons against the cursor path agree with it). A ready index answers
 * folder and tag filters from memory; otherwise the vault is listed and the
 * tag is checked against each note's content as it is read.
 */
async function candidates(
    vault: VaultBackend,
    index: ScanIndex | undefined,
    folder: string | undefined,
    tag: string | undefined,
): Promise<{ paths: string[]; tagApplied: boolean }> {
    if (index?.state === "ready") {
        let paths = index.listPaths(folder || undefined);
        if (tag) paths = paths.filter((path) => index.getTags(path).includes(tag));
        return { paths: paths.sort(), tagApplied: true };
    }
    const listed = [...new Set(await vault.listNotes())]
        .filter((path) => isValidNotePath(path) && (!folder || path.startsWith(folder + "/")))
        .sort();
    return { paths: listed, tagApplied: false };
}

/** Index of the first path that is not before `cursorPath`. */
function lowerBound(paths: string[], cursorPath: string): number {
    let lo = 0;
    let hi = paths.length;
    while (lo < hi) {
        const mid = (lo + hi) >> 1;
        if (paths[mid] < cursorPath) lo = mid + 1;
        else hi = mid;
    }
    return lo;
}

/**
 * Scan current note content, with bounded reads and resumable positions.
 *
 * Content comes from the index's in-memory cache when it holds the note and
 * from disk otherwise (the disk copy is then cached for the next scan). Disk
 * reads run a few at a time (`concurrency`) but notes are consumed in path
 * order, so pages and cursors are deterministic regardless of which read
 * finishes first. A page ends at `max_notes` notes, at `pageChars`
 * characters, or when `limit` results are collected; `next_cursor` resumes
 * exactly there.
 */
export async function scanNotes(
    vault: VaultBackend,
    vaultName: string,
    options: ScanParameters,
    filterKey: string,
    findMatches: (content: string) => LineMatch[],
    { index, pageChars = SCAN_PAGE_CHARS, concurrency = SCAN_READ_CONCURRENCY }: ScanOptions = {},
): Promise<string> {
    const folder = options.folder?.replace(/^\/+|\/+$/g, "");
    const tag = options.tag?.replace(/^#/, "");
    const key = createHash("sha256")
        .update(JSON.stringify([filterKey, folder ?? "", tag ?? ""]))
        .digest("hex");
    let cursor: Cursor | undefined;
    if (options.cursor) {
        try {
            const value: unknown = JSON.parse(Buffer.from(options.cursor, "base64url").toString());
            const parsed = CursorSchema.parse(value);
            if (parsed.key !== key || !isValidNotePath(parsed.path))
                throw new Error("Invalid path");
            cursor = parsed;
        } catch {
            return JSON.stringify({
                error: "Invalid cursor or changed filters. Start again without cursor.",
            });
        }
    }
    const { paths, tagApplied } = await candidates(vault, index, folder, tag);

    const results: (LineMatch & { path: string; url: string })[] = [];
    const skipped: { path: string; reason: string }[] = [];
    let scanned = 0;
    let chars = 0;

    const finish = (next?: { path: string; line: number }) =>
        JSON.stringify({
            results,
            scanned_notes: scanned,
            skipped_notes: skipped,
            next_cursor: next
                ? Buffer.from(JSON.stringify({ key, ...next })).toString("base64url")
                : null,
        });

    // Read ahead of the in-order walk, never past what this page can take.
    const first = cursor ? lowerBound(paths, cursor.path) : 0;
    const last = Math.min(paths.length, first + options.max_notes);
    const reads = new Map<number, Promise<ReadOutcome>>();
    const read = (path: string): Promise<ReadOutcome> => {
        const cached = index?.getContent(path);
        if (cached !== undefined) return Promise.resolve({ content: cached });
        return vault.readNote(path).then(
            (content) => {
                // Only fill a gap: a change that landed meanwhile already cached newer content.
                if (content !== null && index && index.getContent(path) === undefined)
                    index.cacheContent(path, content);
                return { content };
            },
            () => ({ error: true as const }),
        );
    };
    const prefetch = (i: number) => {
        if (i < last) reads.set(i, read(paths[i]));
    };
    for (let i = first; i < first + concurrency; i++) prefetch(i);

    for (let i = first; i < paths.length; i++) {
        const path = paths[i];
        const startLine = cursor?.path === path ? cursor.line : 1;
        if (scanned >= options.max_notes) return finish({ path, line: startLine });
        const outcome = await reads.get(i)!;
        reads.delete(i);
        prefetch(i + concurrency);
        if ("error" in outcome) {
            scanned++;
            skipped.push({ path, reason: "read_error" });
            continue;
        }
        const { content } = outcome;
        // Leave a note that would push this page past the cap for the next page, which always takes at least one.
        if (
            scanned > 0 &&
            content !== null &&
            content.length <= SCAN_NOTE_MAX_CHARS &&
            chars + content.length > pageChars
        ) {
            return finish({ path, line: startLine });
        }
        scanned++;
        if (content === null) {
            skipped.push({ path, reason: "not_found_or_unreadable" });
            continue;
        }
        if (content.length > SCAN_NOTE_MAX_CHARS) {
            skipped.push({ path, reason: `exceeds_${SCAN_NOTE_MAX_CHARS}_char_scan_limit` });
            continue;
        }
        chars += content.length;
        if (
            tag &&
            !tagApplied &&
            !parseFrontmatterAndLinks(content.replace(/\r\n/g, "\n")).tags.includes(tag)
        )
            continue;
        let matches: LineMatch[];

        try {
            matches = findMatches(content).filter((match) => match.line >= startLine);
        } catch {
            skipped.push({ path, reason: "parse_error" });
            continue;
        }

        for (let j = 0; j < matches.length; j++) {
            results.push({ ...matches[j], path, url: makeDeepLink(vaultName, path) });
            if (results.length >= options.limit) {
                if (j + 1 < matches.length) return finish({ path, line: matches[j].line + 1 });
                return finish(paths[i + 1] ? { path: paths[i + 1], line: 1 } : undefined);
            }
        }
    }
    return finish();
}
