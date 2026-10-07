import {
    ListTasksParametersSchema,
    GetNoteOutlineParametersSchema,
    SearchNotesParametersSchema,
    ReadNotesParametersSchema,
} from "../types/retrieval-tools.js";
import type { ReadResult } from "../types/retrieval-tools.js";
import type { ToolRegistrar } from "./tools.js";

import type { VaultBackend } from "../vault/vault-backend.js";
import { makeDeepLink } from "../notes/deeplink.js";
import { validateNotePath } from "../notes/note-path.js";
import { scanNotes, type ScanIndex } from "../notes/note-scan.js";
import { noteStructure, noteTasks } from "../notes/note-structure.js";

/** A note without a checkbox marker has no tasks; skip the Markdown parse. */
const TASK_MARKER = /\[[ \txX]\]/;

/** Excerpt context on each side of a match, in characters. */
const EXCERPT_RADIUS = 80;

/**
 * One excerpt per matching line, found with a single pass of the needle over
 * the whole note rather than a regex call per line. Line numbers are counted
 * only up to each match, so a note without matches costs one failed search.
 */
export function findLineMatches(content: string, needle: RegExp): { line: number; text: string }[] {
    const matches: { line: number; text: string }[] = [];
    let line = 1;
    let pos = 0;
    let lineStart = 0;
    needle.lastIndex = 0;
    for (let match = needle.exec(content); match; match = needle.exec(content)) {
        for (; pos < match.index; pos++) {
            const code = content.charCodeAt(pos);
            if (code === 10) {
                line++;
                lineStart = pos + 1;
            } else if (code === 13) {
                if (content.charCodeAt(pos + 1) === 10) pos++;
                line++;
                lineStart = pos + 1;
            }
        }
        let lineEnd = pos;
        for (; lineEnd < content.length; lineEnd++) {
            const code = content.charCodeAt(lineEnd);
            if (code === 10 || code === 13) break;
        }
        const text = content.slice(lineStart, lineEnd);
        const at = match.index - lineStart;
        const start = Math.max(0, at - EXCERPT_RADIUS);
        const end = Math.min(text.length, at + match[0].length + EXCERPT_RADIUS);
        matches.push({
            line,
            text: (start ? "…" : "") + text.slice(start, end) + (end < text.length ? "…" : ""),
        });
        // Resume after this line so it yields one excerpt at most.
        needle.lastIndex = lineEnd;
        pos = lineEnd;
    }
    return matches;
}

export function registerRetrievalTools(
    server: ToolRegistrar,
    vault: VaultBackend,
    vaultName: string,
    index?: ScanIndex,
) {
    server.addTool({
        name: "list_tasks",
        description:
            "List standard Markdown checkbox tasks with text, completion state, 1-based source lines, and Obsidian URLs. Defaults to incomplete tasks. Ignores frontmatter and code; plugin-specific statuses, recurrence, and due dates are not interpreted. Follow next_cursor until null; skipped notes are reported.",
        parameters: ListTasksParametersSchema,
        execute: async (args) =>
            scanNotes(
                vault,
                vaultName,
                args,
                JSON.stringify(["tasks", args.status]),
                (content) =>
                    TASK_MARKER.test(content)
                        ? noteTasks(content).filter(
                              (task) =>
                                  args.status === "all" ||
                                  task.completed === (args.status === "completed"),
                          )
                        : [],
                { index },
            ),
    });
    server.addTool({
        name: "get_note_outline",
        description:
            "Get document-level headings, full heading paths, and paragraph or standalone block IDs. Returns 1-based inclusive source line ranges; heading ranges include child sections. Ignores frontmatter and code. Use the returned heading paths or block IDs with read_note and edit_note.",
        parameters: GetNoteOutlineParametersSchema,
        execute: async ({ path }) => {
            const content = await vault.readNote(path);
            if (content === null) return JSON.stringify({ error: `Note not found: ${path}` });
            try {
                const outline = noteStructure(content);
                return JSON.stringify({
                    path,
                    url: makeDeepLink(vaultName, path),
                    headings: outline.headings.map(({ heading, level, start_line, end_line }) => ({
                        heading,
                        level,
                        start_line,
                        end_line,
                    })),
                    blocks: outline.blocks.map(({ id, start_line, end_line }) => ({
                        id,
                        start_line,
                        end_line,
                    })),
                });
            } catch (error) {
                return JSON.stringify({
                    error: error instanceof Error ? error.message : "Cannot parse note.",
                });
            }
        },
    });
    server.addTool({
        name: "search_notes",
        description:
            "Search note content for a literal, single-line phrase (not regex or Obsidian query syntax). Returns one excerpt per matching line, 1-based line numbers, and URLs. Reads current content from disk, the whole vault by default; follow next_cursor while it is not null. Reports skipped notes, including notes over 1 million characters.",
        parameters: SearchNotesParametersSchema,
        execute: async (args) => {
            const needle = new RegExp(
                args.query.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"),
                args.case_sensitive ? "gu" : "giu",
            );
            return scanNotes(
                vault,
                vaultName,
                args,
                JSON.stringify(["search", args.query, args.case_sensitive]),
                (content) => findLineMatches(content, needle),
                { index },
            );
        },
    });
    server.addTool({
        name: "read_notes",
        description:
            "Read up to 20 notes in requested order. Returns JSON with per-note status, Obsidian URLs, and missing_paths for notes that do not exist. max_chars caps the entire serialized response; truncated notes and omitted paths are explicit. Read omitted content with read_note.",
        parameters: ReadNotesParametersSchema,
        execute: async ({ paths, max_chars }) => {
            const notes: ReadResult[] = [];
            const serialize = (items: ReadResult[], omitted: string[]) =>
                JSON.stringify({
                    notes: items,
                    missing_paths: items
                        .filter((item) => item.status === "not_found")
                        .map((item) => item.path),
                    omitted_paths: omitted,
                });
            if (serialize([], paths).length > max_chars) {
                return JSON.stringify({
                    error: "max_chars is too small to report these paths. Increase it or request fewer paths.",
                });
            }
            for (let i = 0; i < paths.length; i++) {
                const path = paths[i];
                const note: ReadResult = { path, status: "ok" };
                try {
                    validateNotePath(path);
                    note.url = makeDeepLink(vaultName, path);
                    const content = await vault.readNote(path);
                    if (content === null) note.status = "not_found";
                    else note.content = content;
                } catch {
                    note.status = "error";
                }
                const remaining = paths.slice(i + 1);
                if (serialize([...notes, note], remaining).length <= max_chars) {
                    notes.push(note);
                    continue;
                }
                if (note.content !== undefined) {
                    const content = note.content;
                    note.status = "truncated";
                    note.content = "";
                    note.omitted_chars = content.length;
                    if (serialize([...notes, note], remaining).length <= max_chars) {
                        let lo = 0;
                        let hi = content.length;
                        while (lo < hi) {
                            const mid = Math.ceil((lo + hi) / 2);
                            note.content = content.slice(0, mid);
                            note.omitted_chars = content.length - mid;
                            if (serialize([...notes, note], remaining).length <= max_chars)
                                lo = mid;
                            else hi = mid - 1;
                        }
                        // Never end on the first half of a surrogate pair.
                        if (lo > 0 && /[\ud800-\udbff]/.test(content[lo - 1])) lo--;
                        note.content = content.slice(0, lo);
                        note.omitted_chars = content.length - lo;
                        notes.push(note);
                        return serialize(notes, remaining);
                    }
                }
                return serialize(notes, paths.slice(i));
            }
            return serialize(notes, []);
        },
    });
}
