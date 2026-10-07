import { test } from "vitest";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { ZodType } from "zod";
import { LocalVault } from "../../src/vault/vault-local.js";
import { SearchIndex } from "../../src/search/search.js";
import { registerTools, type ToolRegistrar } from "../../src/tools/tools.js";
import { scanNotes } from "../../src/notes/note-scan.js";
import type { VaultBackend } from "../../src/vault/vault-backend.js";

type CapturedTool = {
    parameters: ZodType;
    execute: (args: never, context: never) => Promise<string>;
};

async function fixture(notes: Record<string, string>) {
    const dir = await mkdtemp(join(tmpdir(), "review-fixes-"));
    const vault = new LocalVault(dir);
    await vault.init();
    for (const [path, content] of Object.entries(notes)) await vault.writeNote(path, content);
    const index = new SearchIndex();
    const tools = new Map<string, CapturedTool>();
    const server: ToolRegistrar = {
        addTool: (tool) => void tools.set(tool.name, tool as unknown as CapturedTool),
    };
    registerTools(server, vault, index, "Test Vault");
    return {
        vault,
        index,
        tools,
        dir,
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

test("get_note_metadata renders nested frontmatter values unambiguously", async () => {
    const f = await fixture({
        "note.md":
            "---\ntitle: Plain\nnested:\n  keep: [one, two]\nlist: [a, b]\ncount: 3\n---\nBody",
    });
    try {
        const result = await f.call("get_note_metadata", { path: "note.md" });
        assert.match(result, /^ {2}title: Plain$/m);
        assert.match(result, /^ {2}nested: \{"keep":\["one","two"\]\}$/m);
        assert.match(result, /^ {2}list: \["a","b"\]$/m);
        assert.match(result, /^ {2}count: 3$/m);
        assert.doesNotMatch(result, /\[object Object\]/);
    } finally {
        await f.close();
    }
});

test("targeted append keeps inserted content out of a following setext heading", async () => {
    const f = await fixture({ "note.md": "# A\ntext\n\nB\n===\nbody\n" });
    try {
        await f.call("edit_note", { path: "note.md", heading: ["A"], content: "new" });
        assert.equal(await f.vault.readNote("note.md"), "# A\ntext\n\nnew\n\nB\n===\nbody\n");
        const outline = JSON.parse(await f.call("get_note_outline", { path: "note.md" }));
        assert.deepEqual(
            outline.headings.map((h: { heading: string[] }) => h.heading),
            [["A"], ["B"]],
        );
    } finally {
        await f.close();
    }
});

test("targeted prepend keeps inserted content out of a following setext heading", async () => {
    const f = await fixture({ "note.md": "# A\nB\n---\nbody\n", "empty.md": "# A\nB\n===\n" });
    try {
        await f.call("edit_note", {
            path: "note.md",
            heading: ["A"],
            operation: "prepend",
            content: "new",
        });
        assert.equal(await f.vault.readNote("note.md"), "# A\nnew\n\nB\n---\nbody\n");
        await f.call("edit_note", {
            path: "empty.md",
            heading: ["A"],
            operation: "prepend",
            content: "new",
        });
        assert.equal(await f.vault.readNote("empty.md"), "# A\nnew\n\nB\n===\n");
    } finally {
        await f.close();
    }
});

test("targeted append before an ATX heading keeps its existing layout", async () => {
    const f = await fixture({ "note.md": "# A\ntext\n# B\nbody\n" });
    try {
        await f.call("edit_note", { path: "note.md", heading: ["A"], content: "new" });
        assert.equal(await f.vault.readNote("note.md"), "# A\ntext\nnew\n# B\nbody\n");
    } finally {
        await f.close();
    }
});

test("block replace rejects emptying the block so its ID does not move", async () => {
    const content = "First\n\npara ^id\n";
    const f = await fixture({ "note.md": content });
    try {
        const result = await f.call("edit_note", {
            path: "note.md",
            block: "id",
            operation: "replace",
            old_text: "para",
            content: "",
        });
        assert.match(result, /empty/i);
        assert.equal(await f.vault.readNote("note.md"), content);
    } finally {
        await f.close();
    }
});

test("read_note rejects ambiguous and missing targets", async () => {
    const f = await fixture({ "note.md": "# Same\nOne\n# Same\nTwo\n\nA ^dup\n\nB ^dup\n" });
    try {
        assert.match(
            await f.call("read_note", { path: "note.md", heading: ["Same"] }),
            /ambiguous/,
        );
        assert.match(await f.call("read_note", { path: "note.md", block: "dup" }), /ambiguous/);
        assert.match(
            await f.call("read_note", { path: "note.md", heading: ["Missing"] }),
            /not found/i,
        );
        assert.match(
            await f.call("read_note", { path: "note.md", block: "missing" }),
            /not found/i,
        );
    } finally {
        await f.close();
    }
});

test("read_notes reports missing paths separately and counts omitted characters", async () => {
    const f = await fixture({ "a.md": "Alpha", "large.md": "x".repeat(5000) });
    try {
        const result = JSON.parse(
            await f.call("read_notes", { paths: ["a.md", "missing.md", "gone.md"] }),
        );
        assert.deepEqual(result.missing_paths, ["missing.md", "gone.md"]);
        assert.equal(result.notes[1].status, "not_found");
        const page = JSON.parse(
            await f.call("read_notes", { paths: ["missing.md", "large.md"], max_chars: 1024 }),
        );
        assert.deepEqual(page.missing_paths, ["missing.md"]);
        const large = page.notes[1];
        assert.equal(large.status, "truncated");
        assert.ok(large.content.length > 0);
        assert.equal(large.omitted_chars, 5000 - large.content.length);
    } finally {
        await f.close();
    }
});

test("read_notes truncation never splits a surrogate pair", async () => {
    const f = await fixture({});
    try {
        for (let pad = 0; pad < 40; pad++) {
            await f.vault.writeNote("emoji.md", "x".repeat(800 + pad) + "😀".repeat(200));
            const page = JSON.parse(
                await f.call("read_notes", { paths: ["emoji.md"], max_chars: 1024 }),
            );
            const content: string = page.notes[0].content;
            assert.equal(page.notes[0].status, "truncated");
            assert.ok(
                !/[\ud800-\udbff]$/.test(content),
                `pad ${pad} ends with a lone high surrogate`,
            );
            assert.equal(page.notes[0].omitted_chars, 800 + pad + 400 - content.length);
        }
    } finally {
        await f.close();
    }
});

test("scan pages stop before a note would exceed the character cap", async () => {
    const notes: Record<string, string> = {
        "a.md": "a".repeat(900_000),
        "b.md": "b".repeat(900_000),
        "c.md": "c".repeat(900_000),
    };
    const vault = {
        listNotes: async () => Object.keys(notes),
        readNote: async (path: string) => notes[path] ?? null,
    } as unknown as VaultBackend;
    const options = { limit: 20, max_notes: 100 };
    const page = { pageChars: 2_000_000 };
    const first = JSON.parse(await scanNotes(vault, "Test Vault", options, "test", () => [], page));
    assert.equal(first.scanned_notes, 2);
    assert.ok(first.next_cursor);
    const second = JSON.parse(
        await scanNotes(
            vault,
            "Test Vault",
            { ...options, cursor: first.next_cursor },
            "test",
            (content) => [{ line: 1, text: content[0] }],
            page,
        ),
    );
    assert.equal(second.scanned_notes, 1);
    assert.equal(second.results[0].path, "c.md");
    assert.equal(second.next_cursor, null);
});

test("scan pages always process at least one note", async () => {
    const notes: Record<string, string> = {
        "a.md": "a".repeat(1_000_000),
        "b.md": "b".repeat(1_000_000),
        "c.md": "c".repeat(1_000_000),
    };
    const vault = {
        listNotes: async () => Object.keys(notes),
        readNote: async (path: string) => notes[path] ?? null,
    } as unknown as VaultBackend;
    const page = { pageChars: 2_000_000 };
    const first = JSON.parse(
        await scanNotes(vault, "Test Vault", { limit: 20, max_notes: 100 }, "test", () => [], page),
    );
    assert.equal(first.scanned_notes, 2);
    const second = JSON.parse(
        await scanNotes(
            vault,
            "Test Vault",
            { limit: 20, max_notes: 100, cursor: first.next_cursor },
            "test",
            () => [],
            page,
        ),
    );
    assert.equal(second.scanned_notes, 1);
    assert.equal(second.next_cursor, null);
});

test("persisted search index from an older schema is discarded", async () => {
    const dir = await mkdtemp(join(tmpdir(), "index-schema-"));
    try {
        const path = join(dir, "index.json");
        await writeFile(
            path,
            JSON.stringify({
                mtimes: { "a.md": 1 },
                tags: { "a.md": ["stale"] },
                links: {},
            }),
        );
        const stale = new SearchIndex(path);
        assert.equal(await stale.loadFromDisk(), false);
        assert.equal(stale.size, 0);
        assert.deepEqual(stale.listAllTags(), []);

        const current = new SearchIndex(path);
        current.update("a.md", "#fresh", 1);
        await current.saveToDisk();
        const reloaded = new SearchIndex(path);
        assert.equal(await reloaded.loadFromDisk(), true);
        assert.deepEqual(reloaded.getTags("a.md"), ["fresh"]);
    } finally {
        await rm(dir, { recursive: true, force: true });
    }
});
