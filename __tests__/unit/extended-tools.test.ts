import { test } from "vitest";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { ZodType } from "zod";
import { LocalVault } from "../../src/vault/vault-local.js";
import { SearchIndex } from "../../src/search/search.js";
import { registerTools, type ToolRegistrar } from "../../src/tools/tools.js";

type CapturedTool = {
    parameters: ZodType;
    execute: (args: never, context: never) => Promise<string>;
};

async function fixture(
    notes: Record<string, string>,
    readOnly = false,
    writeFolders: string[] | null = null,
    contentCacheChars?: number,
) {
    const dir = await mkdtemp(join(tmpdir(), "extended-tools-"));
    const vault = new LocalVault(dir);
    await vault.init();
    for (const [path, content] of Object.entries(notes)) await vault.writeNote(path, content);
    const index = new SearchIndex(undefined, undefined, contentCacheChars);
    const tools = new Map<string, CapturedTool>();
    const server: ToolRegistrar = {
        addTool: (tool) => void tools.set(tool.name, tool as unknown as CapturedTool),
    };
    registerTools(server, vault, index, "Test Vault", readOnly, writeFolders);
    return {
        vault,
        index,
        tools,
        async call(name: string, args: Record<string, unknown>) {
            const tool = tools.get(name);
            assert.ok(tool, `${name} is registered`);
            return tool.execute(tool.parameters.parse(args) as never, {} as never);
        },
        async close() {
            await vault.close();
            await rm(dir, { recursive: true, force: true });
        },
    };
}

test("read_notes returns note contents, missing paths, and a bounded response", async () => {
    const f = await fixture({ "a.md": "Alpha", "empty.md": "", "large.md": "x".repeat(5000) });
    try {
        const result = JSON.parse(
            await f.call("read_notes", { paths: ["a.md", "missing.md", "empty.md"] }),
        );
        assert.equal(result.notes[0].content, "Alpha");
        assert.equal(result.notes[1].status, "not_found");
        assert.equal(result.notes[2].content, "");
        assert.match(result.notes[0].url, /^obsidian:\/\/open/);
        const limited = await f.call("read_notes", {
            paths: ["large.md", "a.md"],
            max_chars: 1024,
        });
        assert.ok(limited.length <= 1024);
        const page = JSON.parse(limited);
        assert.equal(page.notes[0].status, "truncated");
        assert.deepEqual(page.omitted_paths, ["a.md"]);
    } finally {
        await f.close();
    }
});

test("search_notes finds literal content with fresh tag filters and resumable line results", async () => {
    const f = await fixture({
        "work/a.md": "---\r\ntags: [project]\r\n---\r\nA.B first\r\na.b second\r\naxb no",
        "work/b.md": "#project\na.b third",
        "workshop/c.md": "#project\na.b outside",
    });
    try {
        const args = { query: "a.b", folder: "work", tag: "project", limit: 1 };
        const first = JSON.parse(await f.call("search_notes", args));
        assert.equal(first.results[0].path, "work/a.md");
        assert.equal(first.results[0].line, 4);
        assert.match(first.results[0].text, /A\.B first/);
        assert.ok(first.next_cursor);
        const second = JSON.parse(
            await f.call("search_notes", { ...args, cursor: first.next_cursor }),
        );
        assert.equal(second.results[0].line, 5);
        const third = JSON.parse(
            await f.call("search_notes", { ...args, cursor: second.next_cursor }),
        );
        assert.equal(third.results[0].path, "work/b.md");
        assert.equal(f.index.size, 0, "search must work before the metadata index is populated");
        const wrong = JSON.parse(
            await f.call("search_notes", { ...args, query: "changed", cursor: first.next_cursor }),
        );
        assert.match(wrong.error, /cursor/i);
    } finally {
        await f.close();
    }
});

test("search_notes continues after a scan page without matches", async () => {
    const f = await fixture({ "a.md": "nothing", "b.md": "needle" });
    try {
        const first = JSON.parse(await f.call("search_notes", { query: "needle", max_notes: 1 }));
        assert.deepEqual(first.results, []);
        assert.equal(first.scanned_notes, 1);
        assert.ok(first.next_cursor);
        const next = JSON.parse(
            await f.call("search_notes", {
                query: "needle",
                max_notes: 1,
                cursor: first.next_cursor,
            }),
        );
        assert.equal(next.results[0].path, "b.md");
        assert.equal(next.next_cursor, null);
    } finally {
        await f.close();
    }
});

test("update_note_properties preserves typed values and the exact body", async () => {
    const body = "\r\n# Note\r\nDo not change this.\r\n";
    const f = await fixture({
        "note.md":
            "---\r\n# Keep me\r\ntitle: 'Original'\r\nstatus: draft # workflow\r\nold: remove\r\n---\r\n" +
            body,
    });
    try {
        const result = JSON.parse(
            await f.call("update_note_properties", {
                path: "note.md",
                set: { status: "done", count: 3, active: true, tags: ["project", "review"] },
                remove: ["old"],
            }),
        );
        assert.equal(result.status, "updated");
        const content = (await f.vault.readNote("note.md"))!;
        assert.ok(content.endsWith("---\r\n" + body));
        assert.match(content, /# Keep me/);
        assert.match(content, /title: 'Original'/);
        assert.match(content, /status: done/);
        assert.match(content, /count: 3/);
        assert.doesNotMatch(content, /old: remove/);
        const meta = await f.vault.getMetadata("note.md");
        assert.equal(meta!.frontmatter.active, true);
        assert.deepEqual(meta!.frontmatter.tags, ["project", "review"]);
        assert.deepEqual(f.index.getTags("note.md"), ["project", "review"]);
    } finally {
        await f.close();
    }
});

test("property writes reject unsafe input without changing notes and honor write access", async () => {
    const invalid = [
        "---\na: [broken\n---\nBody",
        "---\na: 1\na: 2\n---\nBody",
        "---\n- item\n---\nBody",
        "---\na: value\nBody",
    ];
    const f = await fixture({
        "plain.md": "Plain body",
        ...Object.fromEntries(invalid.map((content, i) => [`bad${i}.md`, content])),
    });
    try {
        for (const [i, original] of invalid.entries()) {
            assert.ok(
                JSON.parse(
                    await f.call("update_note_properties", {
                        path: `bad${i}.md`,
                        set: { status: "done" },
                    }),
                ).error,
            );
            assert.equal(await f.vault.readNote(`bad${i}.md`), original);
        }
        assert.equal(
            JSON.parse(
                await f.call("update_note_properties", { path: "plain.md", remove: ["missing"] }),
            ).status,
            "unchanged",
        );
        assert.equal(await f.vault.readNote("plain.md"), "Plain body");
        assert.ok(
            JSON.parse(
                await f.call("update_note_properties", {
                    path: "plain.md",
                    set: { status: "done" },
                    remove: ["status"],
                }),
            ).error,
        );
    } finally {
        await f.close();
    }
    const readOnly = await fixture({}, true);
    try {
        assert.equal(readOnly.tools.has("update_note_properties"), false);
    } finally {
        await readOnly.close();
    }
    const scoped = await fixture({ "outside.md": "Body" }, false, ["Inbox"]);
    try {
        assert.match(
            JSON.parse(
                await scoped.call("update_note_properties", {
                    path: "outside.md",
                    set: { status: "done" },
                }),
            ).error,
            /denied/,
        );
        assert.equal(await scoped.vault.readNote("outside.md"), "Body");
    } finally {
        await scoped.close();
    }
});

test("get_note_outline returns heading paths and block ranges without code or frontmatter", async () => {
    const content =
        "---\nsummary: fake\n---\n# Project\nIntro\n\n## Notes\nA paragraph\nover two lines. ^detail\n\n```md\n# Fake\nignore ^fake\n```\n\nNext\n====\nEnd\n";
    const f = await fixture({ "note.md": content });
    try {
        const result = JSON.parse(await f.call("get_note_outline", { path: "note.md" }));
        assert.deepEqual(
            result.headings.map((h: { heading: string[] }) => h.heading),
            [["Project"], ["Project", "Notes"], ["Next"]],
        );
        assert.equal(result.headings[0].start_line, 4);
        assert.equal(result.headings[0].end_line, 15);
        assert.equal(result.headings[1].start_line, 7);
        assert.deepEqual(
            result.blocks.map((b: { id: string }) => b.id),
            ["detail"],
        );
        assert.equal(result.blocks[0].start_line, 8);
        assert.equal(result.blocks[0].end_line, 9);
    } finally {
        await f.close();
    }
});

test("list_tasks finds standard and nested tasks while excluding code and frontmatter", async () => {
    const content = [
        "---",
        "example: '- [ ] metadata'",
        "---",
        "# Work",
        "- [ ] Open",
        "  + [X] Nested done",
        "1. [x] Ordered done",
        "",
        "~~~~md",
        "- [ ] Code",
        "~~~",
        "- [ ] Still code",
        "~~~~",
        "",
        "    - [ ] Indented code",
        "",
        "- \\[ ] Escaped",
        "- [-] Custom",
        "* [ ] Last",
    ].join("\r\n");
    const f = await fixture({ "work/tasks.md": content, "elsewhere.md": "- [ ] Outside" });
    try {
        const result = JSON.parse(await f.call("list_tasks", { folder: "work", status: "all" }));
        assert.deepEqual(
            result.results.map((t: { text: string }) => t.text),
            ["Open", "Nested done", "Ordered done", "Last"],
        );
        assert.deepEqual(
            result.results.map((t: { line: number }) => t.line),
            [5, 6, 7, 19],
        );
        const todo = JSON.parse(await f.call("list_tasks", { folder: "work" }));
        assert.deepEqual(
            todo.results.map((t: { text: string }) => t.text),
            ["Open", "Last"],
        );
        const done = JSON.parse(
            await f.call("list_tasks", { folder: "work", status: "completed", limit: 1 }),
        );
        assert.equal(done.results[0].text, "Nested done");
        const next = JSON.parse(
            await f.call("list_tasks", {
                folder: "work",
                status: "completed",
                limit: 1,
                cursor: done.next_cursor,
            }),
        );
        assert.equal(next.results[0].text, "Ordered done");
    } finally {
        await f.close();
    }
});

test("heading and block targets limit reads and edits to the selected content", async () => {
    const content =
        "# First\r\n## Notes\r\nKeep\r\n# Second\r\n## Notes\r\nChange\r\n# Last\r\nParagraph one\r\nline two. ^detail\r\n";
    const f = await fixture({ "note.md": content });
    try {
        const read = await f.call("read_note", { path: "note.md", heading: ["Second", "Notes"] });
        assert.match(read, /Change/);
        assert.doesNotMatch(read, /Keep|Paragraph one/);
        const append = await f.call("edit_note", {
            path: "note.md",
            heading: ["Second", "Notes"],
            content: "Added",
        });
        assert.match(append, /Note edited/);
        const expected = content.replace("Change\r\n", "Change\r\nAdded\r\n");
        assert.equal(await f.vault.readNote("note.md"), expected);
        const block = await f.call("read_note", { path: "note.md", block: "detail" });
        assert.match(block, /Paragraph one\r\nline two\./);
        assert.doesNotMatch(block, /\^detail|Added/);
        await f.call("edit_note", {
            path: "note.md",
            block: "detail",
            operation: "replace",
            old_text: "line two.",
            content: "Updated.",
        });
        assert.equal(await f.vault.readNote("note.md"), expected.replace("line two.", "Updated."));
        assert.match(
            await f.call("edit_note", {
                path: "note.md",
                heading: ["First", "Notes"],
                operation: "replace",
                old_text: "Updated.",
                content: "Bad",
            }),
            /not found/,
        );
    } finally {
        await f.close();
    }
});

test("targeted edits reject duplicate or missing targets without writing", async () => {
    const content = "# Same\nOne\n# Same\nTwo\n\nA ^duplicate\n\nB ^duplicate\n";
    const f = await fixture({ "note.md": content, "empty-section.md": "# Empty" });
    try {
        assert.match(
            await f.call("edit_note", { path: "note.md", heading: ["Same"], content: "Bad" }),
            /ambiguous/,
        );
        assert.match(
            await f.call("edit_note", { path: "note.md", block: "duplicate", content: "Bad" }),
            /ambiguous/,
        );
        assert.match(
            await f.call("edit_note", { path: "note.md", heading: ["Missing"], content: "Bad" }),
            /not found/,
        );
        assert.match(
            await f.call("edit_note", {
                path: "note.md",
                heading: ["Same"],
                block: "duplicate",
                content: "Bad",
            }),
            /either/,
        );
        assert.equal(await f.vault.readNote("note.md"), content);
        await f.call("edit_note", {
            path: "empty-section.md",
            heading: ["Empty"],
            content: "First line",
        });
        assert.equal(await f.vault.readNote("empty-section.md"), "# Empty\nFirst line");
    } finally {
        await f.close();
    }
});

test("search excerpts use original Unicode offsets and tasks can start on the next line", async () => {
    const f = await fixture({
        "unicode.md": "İ".repeat(200) + "needle" + "z".repeat(200),
        "tasks.md": "- [ ]\n  Buy milk\n",
    });
    try {
        const search = JSON.parse(await f.call("search_notes", { query: "needle" }));
        assert.match(search.results[0].text, /needle/);
        const tasks = JSON.parse(await f.call("list_tasks", {}));
        assert.equal(tasks.results[0].text, "Buy milk");
    } finally {
        await f.close();
    }
});

test("batch reads isolate invalid paths and thrown reads; task markers accept tabs", async () => {
    const f = await fixture({ "good.md": "Good", "tasks.md": "- [\t] Tab task\n" });
    const read = f.vault.readNote.bind(f.vault);
    f.vault.readNote = async (path) => {
        if (path === "failed.md") throw new Error("Read failed");
        return read(path);
    };
    try {
        const result = JSON.parse(
            await f.call("read_notes", {
                paths: ["failed.md", "\ud800.md", "../outside.md", "good.md"],
            }),
        );
        assert.deepEqual(
            result.notes.map((n: { status: string }) => n.status),
            ["error", "error", "error", "ok"],
        );
        assert.equal(result.notes[3].content, "Good");
        const tasks = JSON.parse(await f.call("list_tasks", {}));
        assert.equal(tasks.results[0].text, "Tab task");
        assert.equal(tasks.results[0].completed, false);
    } finally {
        await f.close();
    }
});

test("property edits preserve unrelated alias values and unchanged empty frontmatter", async () => {
    const alias = "---\nstatus: &s todo\nother: *s\n---\nBody";
    const empty = "---\n# Keep this\n---\nBody";
    const f = await fixture({
        "alias.md": alias,
        "empty.md": empty,
        "tags.md": "---\ntags: [one, two] # keep me\n---\nBody",
    });
    try {
        const result = JSON.parse(
            await f.call("update_note_properties", { path: "alias.md", set: { status: "done" } }),
        );
        assert.equal(result.status, "updated");
        assert.deepEqual((await f.vault.getMetadata("alias.md"))!.frontmatter, {
            status: "done",
            other: "todo",
        });
        assert.equal(
            JSON.parse(
                await f.call("update_note_properties", { path: "empty.md", remove: ["missing"] }),
            ).status,
            "unchanged",
        );
        assert.equal(await f.vault.readNote("empty.md"), empty);
        await f.call("update_note_properties", { path: "tags.md", set: { tags: "three" } });
        assert.equal((await f.vault.getMetadata("tags.md"))!.frontmatter.tags, "three");
    } finally {
        await f.close();
    }
});

test("setext heading hashes remain literal and block IDs work in lists and quotes", async () => {
    const f = await fixture({
        "note.md": "Heading #\n---\nContent\n\n- Item ^list-id\n\n> Quote ^quote-id\n",
    });
    try {
        const outline = JSON.parse(await f.call("get_note_outline", { path: "note.md" }));
        assert.deepEqual(outline.headings[0].heading, ["Heading #"]);
        assert.deepEqual(
            outline.blocks.map((b: { id: string }) => b.id),
            ["list-id", "quote-id"],
        );
        assert.match(await f.call("read_note", { path: "note.md", block: "list-id" }), /Item/);
        await f.call("edit_note", {
            path: "note.md",
            block: "quote-id",
            operation: "replace",
            old_text: "Quote",
            content: "Changed",
        });
        assert.match((await f.vault.readNote("note.md"))!, /> Changed \^quote-id/);
    } finally {
        await f.close();
    }
});

test("property creation preserves the body and existing nested values", async () => {
    const body = "\uFEFF# Plain\r\nKeep this.\r\n";
    const f = await fixture({
        "plain.md": body,
        "nested.md": "---\nnested:\n  keep: [one, two]\n---\nBody",
    });
    try {
        await f.call("update_note_properties", {
            path: "plain.md",
            set: { optional: null, aliases: ["Alpha", "Beta"] },
        });
        const content = (await f.vault.readNote("plain.md"))!;
        assert.ok(content.startsWith("\uFEFF---\r\n"));
        assert.ok(content.endsWith(body.slice(1)));
        const meta = await f.vault.getMetadata("plain.md");
        assert.equal(meta!.frontmatter.optional, null);
        assert.deepEqual(meta!.frontmatter.aliases, ["Alpha", "Beta"]);
        await f.call("update_note_properties", { path: "nested.md", set: { status: "done" } });
        assert.deepEqual((await f.vault.getMetadata("nested.md"))!.frontmatter.nested, {
            keep: ["one", "two"],
        });
    } finally {
        await f.close();
    }
});

test("targeted prepend and standalone block edits preserve surrounding text", async () => {
    const content = "# Start\nBody\n# End\n\n- One\n- Two\n\n^list\n\nAfter\n";
    const f = await fixture({ "note.md": content });
    try {
        await f.call("edit_note", {
            path: "note.md",
            heading: ["Start"],
            operation: "prepend",
            content: "Before",
        });
        assert.equal(await f.vault.readNote("note.md"), content.replace("Body", "Before\nBody"));
        const read = await f.call("read_note", { path: "note.md", block: "list" });
        assert.ok(read.endsWith("- One\n- Two"));
        await f.call("edit_note", {
            path: "note.md",
            block: "list",
            operation: "replace",
            old_text: "- Two",
            content: "- Three",
        });
        assert.equal(
            await f.vault.readNote("note.md"),
            content.replace("Body", "Before\nBody").replace("- Two", "- Three"),
        );
    } finally {
        await f.close();
    }
});

test("whole-note prepend lands after any frontmatter form", async () => {
    const cases: [string, string, string][] = [
        ["plain.md", "---\na: 1\n---\nBody\n", "---\na: 1\n---\nNew\nBody\n"],
        ["empty-fm.md", "---\n---\nBody\n", "---\n---\nNew\nBody\n"],
        ["eof.md", "---\na: 1\n---", "---\na: 1\n---\nNew\n"],
        ["eof-crlf.md", "---\r\na: 1\r\n---", "---\r\na: 1\r\n---\r\nNew\r\n"],
        ["trailing.md", "---  \na: 1\n--- \t\nBody\n", "---  \na: 1\n--- \t\nNew\nBody\n"],
        ["bom.md", "﻿---\na: 1\n---\nBody\n", "﻿---\na: 1\n---\nNew\nBody\n"],
        ["crlf.md", "---\r\na: 1\r\n---\r\nBody\r\n", "---\r\na: 1\r\n---\r\nNew\r\nBody\r\n"],
        ["none.md", "Body\n", "New\nBody\n"],
    ];
    const f = await fixture(Object.fromEntries(cases.map(([path, content]) => [path, content])));
    try {
        for (const [path, , expected] of cases) {
            await f.call("edit_note", { path, operation: "prepend", content: "New" });
            assert.equal(await f.vault.readNote(path), expected, path);
        }
    } finally {
        await f.close();
    }
});

test("list_notes tag filter ignores #, ignores case, and includes nested tags", async () => {
    const notes = {
        "a.md": "---\ntags: [Project]\n---\nA",
        "b.md": "Body #project/sub",
        "c.md": "Body #projects",
        "d.md": "Untagged",
    };
    const f = await fixture(notes);
    try {
        for (const [path, content] of Object.entries(notes)) f.index.update(path, content, 1);
        for (const tag of ["project", "#PROJECT"]) {
            const result = await f.call("list_notes", { tag });
            assert.match(result, /\[a\.md\]/, tag);
            assert.match(result, /\[b\.md\]/, tag);
            assert.doesNotMatch(result, /\[c\.md\]|\[d\.md\]/, tag);
        }
        const nested = await f.call("list_notes", { tag: "project/sub" });
        assert.doesNotMatch(nested, /\[a\.md\]/);
        assert.match(nested, /\[b\.md\]/);
    } finally {
        await f.close();
    }
});

test("list_notes rejects a non-integer or out-of-range limit", async () => {
    const f = await fixture({ "a.md": "A" });
    try {
        for (const limit of [0, -1, 1.5, 10_001])
            await assert.rejects(f.call("list_notes", { limit }), Error, String(limit));
        assert.match(await f.call("list_notes", { limit: "1" }), /\[a\.md\]/);
    } finally {
        await f.close();
    }
});

test("scans report oversized and unreadable notes and continue to later matches", async () => {
    const f = await fixture({ "a.md": "x".repeat(1_000_001), "b.md": "Failed", "c.md": "needle" });
    const read = f.vault.readNote.bind(f.vault);
    f.vault.readNote = async (path) => {
        if (path === "b.md") throw new Error("Unavailable");
        return read(path);
    };
    try {
        const page = JSON.parse(await f.call("search_notes", { query: "needle", max_notes: 2 }));
        assert.deepEqual(
            page.skipped_notes.map((n: { reason: string }) => n.reason),
            ["exceeds_1000000_char_scan_limit", "read_error"],
        );
        assert.ok(page.next_cursor);
        const next = JSON.parse(
            await f.call("search_notes", {
                query: "needle",
                max_notes: 2,
                cursor: page.next_cursor,
            }),
        );
        assert.equal(next.results[0].path, "c.md");
        assert.equal(next.next_cursor, null);
        await assert.rejects(f.call("search_notes", { query: " ", limit: -1 }));
        await assert.rejects(f.call("read_notes", { paths: ["c.md"], max_chars: 1 }));
    } finally {
        await f.close();
    }
});

test("scans take folder and tag candidates from a ready index and read only those notes", async () => {
    // Content cache off, so every served note is a visible disk read.
    const f = await fixture(
        {
            "work/tagged.md": "#project\nneedle here",
            "work/plain.md": "needle but no tag",
            "work/stale.md": "needle, tag removed on disk",
            "play/tagged.md": "#project\nneedle elsewhere",
        },
        false,
        null,
        0,
    );
    try {
        for (const path of await f.vault.listNotes())
            f.index.update(path, (await f.vault.readNote(path))!, 1);
        // The index is what answers the filter, even where the disk disagrees.
        f.index.update("work/stale.md", "#project\nneedle, tag removed on disk", 1);
        f.index.state = "ready";
        const reads: string[] = [];
        const read = f.vault.readNote.bind(f.vault);
        f.vault.readNote = async (path) => {
            reads.push(path);
            return read(path);
        };
        const args = { query: "needle", folder: "work", tag: "project" };
        const page = JSON.parse(await f.call("search_notes", args));
        assert.deepEqual(
            page.results.map((r: { path: string }) => r.path),
            ["work/stale.md", "work/tagged.md"],
        );
        assert.deepEqual(reads.sort(), ["work/stale.md", "work/tagged.md"]);
        assert.equal(page.scanned_notes, 2);
        assert.equal(page.next_cursor, null);

        // A note the index still lists but that is gone from disk is reported, not fatal.
        await f.vault.deleteNote("work/tagged.md");
        const after = JSON.parse(await f.call("search_notes", args));
        assert.deepEqual(after.skipped_notes, [
            { path: "work/tagged.md", reason: "not_found_or_unreadable" },
        ]);
        assert.equal(after.results.length, 1);
    } finally {
        await f.close();
    }
});

test("scan pages keep path order and resume at the cursor while reading ahead", async () => {
    const notes = Object.fromEntries(
        Array.from({ length: 40 }, (_, i) => [`n${String(i).padStart(2, "0")}.md`, `needle ${i}`]),
    );
    const f = await fixture(notes);
    try {
        // Pages end on the result limit in one run and on max_notes in the other.
        for (const bounds of [
            { limit: 7, max_notes: 9 },
            { limit: 50, max_notes: 9 },
        ]) {
            const seen: string[] = [];
            let cursor: string | undefined;
            let pages = 0;
            do {
                const page = JSON.parse(
                    await f.call("search_notes", {
                        query: "needle",
                        ...bounds,
                        ...(cursor && { cursor }),
                    }),
                );
                seen.push(...page.results.map((r: { path: string }) => r.path));
                cursor = page.next_cursor ?? undefined;
                pages++;
            } while (cursor);
            assert.deepEqual(seen, Object.keys(notes).sort());
            assert.equal(pages, bounds.limit === 7 ? 6 : 5);
        }
    } finally {
        await f.close();
    }
});

test("scans serve content from the index cache and fill it from disk only once", async () => {
    const f = await fixture({ "a.md": "needle one", "b.md": "needle two", "c.md": "nothing" });
    try {
        const reads: string[] = [];
        const read = f.vault.readNote.bind(f.vault);
        f.vault.readNote = async (path) => {
            reads.push(path);
            return read(path);
        };
        // Index still building: every note comes from disk, and is cached on the way.
        let page = JSON.parse(await f.call("search_notes", { query: "needle" }));
        assert.equal(page.results.length, 2);
        assert.deepEqual(reads, ["a.md", "b.md", "c.md"]);
        assert.equal(f.index.getContent("c.md"), "nothing");

        page = JSON.parse(await f.call("search_notes", { query: "needle" }));
        assert.equal(page.results.length, 2);
        assert.deepEqual(reads, ["a.md", "b.md", "c.md"], "second scan read nothing from disk");

        // A write through the tools refreshes the cache; search sees it without a disk read.
        await f.call("write_note", { path: "c.md", content: "needle three" });
        page = JSON.parse(await f.call("search_notes", { query: "needle" }));
        assert.deepEqual(
            page.results.map((r: { path: string }) => r.path),
            ["a.md", "b.md", "c.md"],
        );
        assert.deepEqual(reads, ["a.md", "b.md", "c.md"]);
        // ...and a delete drops it, so the note is neither served nor read.
        await f.call("delete_note", { path: "b.md" });
        page = JSON.parse(await f.call("search_notes", { query: "needle" }));
        assert.deepEqual(
            page.results.map((r: { path: string }) => r.path),
            ["a.md", "c.md"],
        );
        assert.deepEqual(
            reads,
            ["a.md", "b.md", "c.md", "b.md"],
            "delete_note reads before deleting",
        );
    } finally {
        await f.close();
    }
});

test("search excerpts report one match per line with correct numbers across line endings", async () => {
    const f = await fixture({
        "crlf.md": "a\r\nneedle needle\r\n\r\nneedle",
        "cr.md": "needle\rx\rneedle",
        "long.md": "x".repeat(200) + "needle" + "y".repeat(200) + "\nend needle",
    });
    try {
        const page = JSON.parse(await f.call("search_notes", { query: "needle", limit: 50 }));
        const byPath = (path: string) =>
            page.results
                .filter((r: { path: string }) => r.path === path)
                .map((r: { line: number; text: string }) => [r.line, r.text]);
        assert.deepEqual(byPath("cr.md"), [
            [1, "needle"],
            [3, "needle"],
        ]);
        assert.deepEqual(byPath("crlf.md"), [
            [2, "needle needle"],
            [4, "needle"],
        ]);
        const [first, second] = byPath("long.md");
        assert.equal(first[0], 1);
        assert.equal(first[1], "…" + "x".repeat(80) + "needle" + "y".repeat(80) + "…");
        assert.deepEqual(second, [2, "end needle"]);
    } finally {
        await f.close();
    }
});
