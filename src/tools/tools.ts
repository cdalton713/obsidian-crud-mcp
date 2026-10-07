import { logger } from "../logging/logger.js";
import {
    ReadNoteParametersSchema,
    WriteNoteParametersSchema,
    ListNotesParametersSchema,
    EmptyParametersSchema,
    EditNoteParametersSchema,
    DeleteNoteParametersSchema,
    MoveNoteParametersSchema,
    GetNoteMetadataParametersSchema,
} from "../types/tools.js";
import type { FastMCP } from "fastmcp";

import { makeDeepLink } from "../notes/deeplink.js";
import type { VaultBackend } from "../vault/vault-backend.js";
import type { SearchIndex } from "../search/search.js";
import { isPathWritable } from "../vault/write-scope.js";
import type { AiSearchClient } from "../search/ai-search.js";
import { describeListing, describeNoMatch } from "./list-format.js";
import { registerRetrievalTools } from "./retrieval-tools.js";
import { registerPropertyTools } from "./property-tools.js";
import { registerSemanticTools } from "./semantic-tools.js";
import { selectNoteRange, startsWithSetextBoundary } from "../notes/note-structure.js";
import { splitFrontmatter } from "../notes/note-properties.js";

/** The slice of FastMCP the register* functions need; tests pass a fake. */
export type ToolRegistrar = Pick<FastMCP, "addTool">;

/**
 * Wrap `server.addTool` so every tool call logs its name and duration (info),
 * and its arguments at debug level. The duration line is what separates time
 * spent in this server from time spent in the client.
 */
function withCallLogging(server: ToolRegistrar): ToolRegistrar {
    return {
        addTool: (tool) =>
            server.addTool({
                ...tool,
                execute: async (args, ctx) => {
                    if (logger.isLevelEnabled("debug"))
                        logger.debug(`[tool] ${tool.name}(${JSON.stringify(args)})`);
                    const start = performance.now();
                    try {
                        return await tool.execute(args, ctx);
                    } finally {
                        logger.info(
                            `[tool] ${tool.name} ${(performance.now() - start).toFixed(0)}ms`,
                        );
                    }
                },
            }),
    };
}

/** Obsidian tag match: ignores a leading `#` and case; `project` also matches `project/sub`. */
export function tagMatches(tags: readonly string[], wanted: string): boolean {
    const target = wanted.replace(/^#/, "").toLowerCase();
    return tags.some((t) => {
        const tag = t.replace(/^#/, "").toLowerCase();
        return tag === target || tag.startsWith(target + "/");
    });
}

/** Pad text so it ends with a blank line. */
function withBlankLine(text: string, eol: string): string {
    const breaks = /(?:\r\n|\n|\r)*$/.exec(text)![0].replace(/\r\n/g, "\n").length;
    return text + eol.repeat(Math.max(0, 2 - breaks));
}

const WRITE_TOOLS = [
    "write_note",
    "edit_note",
    "delete_note",
    "move_note",
    "update_note_properties",
] as const;

// Claude Code persists any MCP tool result above ~50 000 characters to a file
// and hands the model a 2 kB preview instead of the text. A tool can raise its
// own threshold (hard ceiling 500 000) by declaring
// `_meta["anthropic/maxResultSizeChars"]` in its tools/list entry; text from
// such a tool is then also exempt from MAX_MCP_OUTPUT_TOKENS. See
// https://code.claude.com/docs/en/mcp#raise-the-limit-for-a-specific-tool
// read_note returns whole notes, so it declares 100 000: enough for a large
// note to arrive in one piece, small enough to keep one read inside a sane
// context budget. Other clients ignore the key.
export const READ_NOTE_MAX_RESULT_SIZE_CHARS = 100_000;

export function registerTools(
    registrar: ToolRegistrar,
    vault: VaultBackend,
    searchIndex: SearchIndex,
    vaultName: string,
    readOnly = false,
    writeFolders: string[] | null = null,
    semantic?: AiSearchClient,
) {
    if (readOnly) {
        logger.info(`READ_ONLY mode: write tools disabled (${WRITE_TOOLS.join(", ")}).`);
    } else if (writeFolders) {
        logger.info(
            `WRITE_FOLDERS: writes restricted to ${writeFolders.map((f) => f + "/").join(", ")}.`,
        );
    }
    const writeScopeNote = writeFolders
        ? ` Writes are only allowed inside: ${writeFolders.map((f) => f + "/").join(", ")}.`
        : "";
    const denyWrite = (path: string) =>
        `Write access denied: '${path}' is outside the writable folders (${writeFolders!.map((f) => f + "/").join(", ")}).`;
    const server = withCallLogging(registrar);
    registerRetrievalTools(server, vault, vaultName, searchIndex);
    if (semantic) registerSemanticTools(server, semantic, vaultName);
    // After a write, ask the semantic index to catch up; harmless when unset.
    const changed = () => semantic?.requestSync();
    if (!readOnly)
        registerPropertyTools(server, vault, searchIndex, vaultName, writeFolders, changed);
    server.addTool({
        name: "read_note",
        description:
            "Read a note or a selected heading section/block from the Obsidian vault. Returns Markdown and an Obsidian link. Omit heading and block to read the whole note; missing or ambiguous targets are rejected.",
        _meta: { "anthropic/maxResultSizeChars": READ_NOTE_MAX_RESULT_SIZE_CHARS },
        parameters: ReadNoteParametersSchema,
        execute: async ({ path, heading, block }) => {
            const content = await vault.readNote(path);
            if (content === null) {
                return `Note not found: ${path}`;
            }
            const deepLink = makeDeepLink(vaultName, path);
            try {
                const range = selectNoteRange(content, heading, block);
                return `[Open in Obsidian](${deepLink})\n\n---\n\n${content.slice(range.start, range.end)}`;
            } catch (error) {
                return error instanceof Error ? error.message : "Cannot select note target.";
            }
        },
    });

    if (!readOnly)
        server.addTool({
            name: "write_note",
            description:
                "Write or update a note in the Obsidian vault. Creates the note if it doesn't exist. Replaces the entire content if it does — read first if you need to preserve existing content." +
                writeScopeNote,
            parameters: WriteNoteParametersSchema,
            execute: async ({ path, content }) => {
                if (!isPathWritable(path, writeFolders)) return denyWrite(path);
                const ok = await vault.writeNote(path, content);
                if (!ok) {
                    return `Failed to write note: ${path}`;
                }
                searchIndex.update(path, content, Date.now());
                changed();
                const deepLink = makeDeepLink(vaultName, path);
                return `Note saved: ${path}\n[Open in Obsidian](${deepLink})`;
            },
        });

    server.addTool({
        name: "list_notes",
        description:
            "List markdown notes in the vault with modification timestamps. Examples: list_notes(sort_by='modified', limit=10) for 10 most recent notes. list_notes(name='meeting') to find notes by name. list_notes(folder='daily') for a specific folder. list_notes(tag='project') for notes with a specific tag. Returns up to 100 notes by default.",
        parameters: ListNotesParametersSchema,
        execute: async ({ folder, name, tag, sort_by, modified_after, limit }) => {
            // Use search index (works with encrypted vaults), fall back to vault
            let notes = searchIndex.listWithMtime(folder);
            let vaultTotal: number | null = searchIndex.size;
            let servedByVault = false;
            if (notes.length === 0) {
                notes = await vault.listNotesWithMtime(folder);
                servedByVault = notes.length > 0;
                // The fallback only tells us the vault total when it was unscoped.
                if (vaultTotal === 0) vaultTotal = folder ? null : notes.length;
            }
            const folderTotal = folder ? notes.length : null;
            const filters: string[] = [];
            if (name) {
                const lower = name.toLowerCase();
                notes = notes.filter((n) => n.path.toLowerCase().includes(lower));
                filters.push(`name="${name}"`);
            }
            if (tag) {
                notes = notes.filter((n) => tagMatches(searchIndex.getTags(n.path), tag));
                filters.push(`tag="${tag}"`);
            }
            if (modified_after) {
                const cutoff = new Date(modified_after).getTime();
                if (isNaN(cutoff))
                    return `Invalid date format: ${modified_after}. Use ISO format like '2026-03-25'.`;
                notes = notes.filter((n) => n.mtime >= cutoff);
                filters.push(`modified_after="${modified_after}"`);
            }
            const index = { state: searchIndex.state, size: searchIndex.size, servedByVault };
            if (notes.length === 0) {
                return describeNoMatch({ vaultTotal, folder, folderTotal, filters, index });
            }
            const sortBy = sort_by === "modified" ? "modified" : "name";
            if (sortBy === "modified") {
                notes.sort((a, b) => b.mtime - a.mtime);
            }
            const cap = limit ?? 100;
            const total = notes.length;
            const capped = notes.slice(0, cap);
            const header = describeListing({
                shown: capped.length,
                matched: total,
                vaultTotal,
                folder,
                folderTotal,
                filters,
                sortBy,
                limit: cap,
                omitted: notes.slice(cap).map((n) => n.path),
                index,
            });
            const lines = capped.map((n) => {
                const deepLink = makeDeepLink(vaultName, n.path);
                const date = n.mtime ? new Date(n.mtime).toISOString().slice(0, 16) : "";
                return `- ${date} [${n.path}](${deepLink})`;
            });
            return [header, ...lines].join("\n");
        },
    });

    server.addTool({
        name: "list_folders",
        description:
            "List all folders in the vault. Use this to discover folder names before writing or listing notes. Returns the folder tree with note counts.",
        parameters: EmptyParametersSchema,
        execute: async () => {
            let paths = searchIndex.listPaths();
            if (paths.length === 0) {
                paths = await vault.listNotes();
            }
            const folders = new Map<string, number>();
            for (const p of paths) {
                const lastSlash = p.lastIndexOf("/");
                if (lastSlash === -1) {
                    folders.set("(root)", (folders.get("(root)") ?? 0) + 1);
                } else {
                    const folder = p.slice(0, lastSlash);
                    folders.set(folder, (folders.get(folder) ?? 0) + 1);
                    // Ensure all parent folders appear in the list
                    let parent = folder;
                    while (parent.includes("/")) {
                        parent = parent.slice(0, parent.lastIndexOf("/"));
                        if (!folders.has(parent)) folders.set(parent, 0);
                    }
                }
            }
            if (folders.size === 0) {
                return "Vault is empty.";
            }
            const sorted = [...folders.entries()].sort((a, b) => a[0].localeCompare(b[0]));
            return sorted.map(([f, count]) => `- ${f} (${count} notes)`).join("\n");
        },
    });

    server.addTool({
        name: "list_tags",
        description:
            "List all tags used in the vault, sorted by frequency. Use this to discover tags before filtering with list_notes.",
        parameters: EmptyParametersSchema,
        execute: async () => {
            const tags = searchIndex.listAllTags();
            if (tags.length === 0) {
                return "No tags found in the vault.";
            }
            return tags.map(({ tag, count }) => `- #${tag} (${count} notes)`).join("\n");
        },
    });

    if (!readOnly)
        server.addTool({
            name: "edit_note",
            description:
                "Edit a note or a selected heading section/block. Use 'append' (default), 'prepend' (after frontmatter for whole notes), or 'replace' to swap old_text with new content. For replace, old_text must match exactly once within the selected content. Heading lines and block IDs are preserved. Missing or ambiguous targets are rejected." +
                writeScopeNote,
            parameters: EditNoteParametersSchema,
            execute: async ({ path, content: newContent, operation, old_text, heading, block }) => {
                if (!isPathWritable(path, writeFolders)) return denyWrite(path);
                const fullContent = await vault.readNote(path);
                if (fullContent === null) {
                    return `Note not found: ${path}`;
                }
                let range: { start: number; end: number };
                try {
                    range = selectNoteRange(fullContent, heading, block);
                } catch (error) {
                    return error instanceof Error ? error.message : "Cannot select note target.";
                }
                const existing = fullContent.slice(range.start, range.end);
                const targeted = Boolean(heading || block);
                const eol = targeted && fullContent.includes("\r\n") ? "\r\n" : "\n";

                let updated: string;
                const op = operation ?? "append";

                if (op === "replace") {
                    if (!old_text) {
                        return "old_text is required for replace operation.";
                    }
                    const idx = existing.indexOf(old_text);
                    if (idx === -1) {
                        return "old_text not found in note.";
                    }
                    if (existing.indexOf(old_text, idx + 1) !== -1) {
                        return "old_text matches multiple times. Provide a longer, unique string.";
                    }
                    updated =
                        existing.slice(0, idx) + newContent + existing.slice(idx + old_text.length);
                    // An empty block leaves its ^id marker behind, which then labels the previous block.
                    if (block && updated.trim() === "") {
                        return "Replacement would leave the block empty. Replace text that includes the ^block marker, without a block target, to delete it.";
                    }
                } else if (op === "prepend") {
                    // Insert after frontmatter if present
                    const fm = targeted ? null : splitFrontmatter(existing);
                    if (fm && fm.yaml !== null) {
                        // A closing `---` at EOF has no line break to stand on.
                        const gap = fm.closing.endsWith("\n") ? "" : fm.eol;
                        updated =
                            existing.slice(0, fm.bodyOffset) + gap + newContent + fm.eol + fm.body;
                    } else if (heading && existing && startsWithSetextBoundary(existing)) {
                        updated = withBlankLine(newContent, eol) + existing;
                    } else {
                        updated = newContent + eol + existing;
                    }
                } else {
                    // append
                    updated =
                        targeted && existing.length === 0
                            ? newContent
                            : existing.endsWith("\n")
                              ? existing + newContent
                              : existing + eol + newContent;
                }

                let prefix = fullContent.slice(0, range.start);
                const suffix = fullContent.slice(range.end);
                // Text directly above a setext heading would become part of its title.
                if (
                    heading &&
                    op !== "replace" &&
                    (op === "append" || !existing) &&
                    updated &&
                    suffix &&
                    startsWithSetextBoundary(suffix)
                ) {
                    updated = withBlankLine(updated, eol);
                }
                if (heading && prefix && !/[\r\n]$/.test(prefix)) prefix += eol;
                if (heading && suffix && updated && !/[\r\n]$/.test(updated)) updated += eol;
                updated = prefix + updated + suffix;

                const ok = await vault.writeNote(path, updated);
                if (!ok) {
                    return `Failed to edit note: ${path}`;
                }
                searchIndex.update(path, updated, Date.now());
                changed();
                const deepLink = makeDeepLink(vaultName, path);
                return `Note edited (${op}): ${path}\n[Open in Obsidian](${deepLink})`;
            },
        });

    if (!readOnly)
        server.addTool({
            name: "delete_note",
            description: "Delete a note from the Obsidian vault." + writeScopeNote,
            parameters: DeleteNoteParametersSchema,
            execute: async ({ path }) => {
                if (!isPathWritable(path, writeFolders)) return denyWrite(path);
                // The sync layer reports success even when nothing existed at the
                // path, so "Deleted" would claim a cleanup that never happened.
                // readNote returns "" for an empty note and null only when absent.
                const existing = await vault.readNote(path);
                if (existing === null) return `Note not found: ${path}`;
                const ok = await vault.deleteNote(path);
                if (ok) {
                    searchIndex.remove(path);
                    changed();
                }
                return ok ? `Deleted: ${path}` : `Failed to delete: ${path}`;
            },
        });

    if (!readOnly)
        server.addTool({
            name: "move_note",
            description:
                "Move or rename a note. Use this to rename a note within the same folder, move it to a different folder, or both at once. Creates destination folders automatically." +
                writeScopeNote,
            parameters: MoveNoteParametersSchema,
            execute: async ({ from, to }) => {
                // Moving out of a folder deletes there; moving in writes there — both ends must be writable.
                if (!isPathWritable(from, writeFolders)) return denyWrite(from);
                if (!isPathWritable(to, writeFolders)) return denyWrite(to);
                const content = await vault.readNote(from);
                const ok = await vault.moveNote(from, to);
                if (!ok) {
                    return `Failed to move: ${from} → ${to}`;
                }
                searchIndex.remove(from);
                // content === "" is an empty-but-present note: keep it indexed at the new path.
                if (content !== null) searchIndex.update(to, content, Date.now());
                changed();
                const deepLink = makeDeepLink(vaultName, to);
                return `Moved: ${from} → ${to}\n[Open in Obsidian](${deepLink})`;
            },
        });

    server.addTool({
        name: "get_note_metadata",
        description:
            "Get metadata about a note without reading its full content. Returns frontmatter, tags, outgoing links, backlinks (notes that link to this one), size, and timestamps. Use this to navigate the knowledge graph.",
        parameters: GetNoteMetadataParametersSchema,
        execute: async ({ path }) => {
            const meta = await vault.getMetadata(path);
            if (!meta) {
                return `Note not found: ${path}`;
            }
            const deepLink = makeDeepLink(vaultName, path);
            const lines = [
                `**${path}**`,
                `Size: ${meta.size} bytes`,
                `Created: ${new Date(meta.ctime).toISOString()}`,
                `Modified: ${new Date(meta.mtime).toISOString()}`,
            ];
            if (Object.keys(meta.frontmatter).length > 0) {
                lines.push(`\nFrontmatter:`);
                for (const [k, v] of Object.entries(meta.frontmatter)) {
                    lines.push(`  ${k}: ${typeof v === "string" ? v : JSON.stringify(v)}`);
                }
            }
            if (meta.tags.length > 0) {
                lines.push(`\nTags: ${meta.tags.map((t) => `#${t}`).join(", ")}`);
            }
            if (meta.links.length > 0) {
                lines.push(`\nOutgoing links: ${meta.links.join(", ")}`);
            }
            const backlinks = searchIndex.getBacklinks(path);
            if (backlinks.length > 0) {
                lines.push(`\nBacklinks: ${backlinks.join(", ")}`);
            }
            lines.push(`\n[Open in Obsidian](${deepLink})`);
            return lines.join("\n");
        },
    });
}
