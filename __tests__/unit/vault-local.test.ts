import { describe, it, beforeAll, afterAll, vi } from "vitest";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile, mkdir, unlink, readFile, symlink, chmod } from "fs/promises";
import { watch } from "fs";
import { tmpdir } from "os";
import { join } from "path";
import { LocalVault } from "../../src/vault/vault-local.js";
import { SearchIndex } from "../../src/search/search.js";

let tmpDir: string;
let vault: LocalVault;

beforeAll(async () => {
    tmpDir = await mkdtemp(join(tmpdir(), "vault-test-"));
    vault = new LocalVault(tmpDir);
});

afterAll(async () => {
    await rm(tmpDir, { recursive: true, force: true });
});

describe("local write folders", () => {
    for (const operation of ["write", "delete", "move-from", "move-to"] as const) {
        it(`blocks ${operation} through a directory alias to a protected folder`, async () => {
            const root = await mkdtemp(join(tmpdir(), "vault-scope-"));
            try {
                await mkdir(join(root, "MCP"));
                await mkdir(join(root, "private"));
                await writeFile(join(root, "private/note.md"), "protected");
                await writeFile(join(root, "MCP/source.md"), "source");
                await symlink(join(root, "private"), join(root, "MCP/alias"));
                const scoped = new LocalVault(root, ["MCP"]);
                const actions = {
                    write: () => scoped.writeNote("MCP/alias/new/deep.md", "changed"),
                    delete: () => scoped.deleteNote("MCP/alias/note.md"),
                    "move-from": () => scoped.moveNote("MCP/alias/note.md", "MCP/moved.md"),
                    "move-to": () => scoped.moveNote("MCP/source.md", "MCP/alias/new/deep.md"),
                };
                await assert.rejects(actions[operation], /Write access denied/);
                assert.equal(await scoped.readNote("private/note.md"), "protected");
                assert.equal(await scoped.readNote("MCP/source.md"), "source");
                assert.equal(await scoped.readNote("private/new/deep.md"), null);
            } finally {
                await rm(root, { recursive: true, force: true });
            }
        });
    }

    it("rejects dangling links and permits links within writable folders", async () => {
        const root = await mkdtemp(join(tmpdir(), "vault-scope-"));
        try {
            await mkdir(join(root, "MCP/notes"), { recursive: true });
            await mkdir(join(root, "private"));
            await symlink(join(root, "private/missing.md"), join(root, "MCP/dangling.md"));
            await symlink(join(root, "MCP/notes"), join(root, "MCP/allowed"));
            const scoped = new LocalVault(root, ["MCP"]);
            await assert.rejects(
                () => scoped.writeNote("MCP/dangling.md", "changed"),
                /dangling symlink/,
            );
            assert.equal(await scoped.readNote("private/missing.md"), null);
            assert.equal(await scoped.writeNote("MCP/allowed/new/note.md", "allowed"), true);
            assert.equal(await scoped.readNote("MCP/notes/new/note.md"), "allowed");
            await assert.rejects(
                () => scoped.writeNote("private/new.md", "changed"),
                /Write access denied/,
            );
        } finally {
            await rm(root, { recursive: true, force: true });
        }
    });

    it("blocks a writable folder's symlink to a protected note", async () => {
        const root = await mkdtemp(join(tmpdir(), "vault-scope-"));
        try {
            await mkdir(join(root, "MCP"));
            await mkdir(join(root, "private"));
            await writeFile(join(root, "private/note.md"), "protected");
            await symlink(join(root, "private/note.md"), join(root, "MCP/alias.md"));
            const scoped = new LocalVault(root, ["MCP"]);
            await assert.rejects(
                () => scoped.writeNote("MCP/alias.md", "changed"),
                /Write access denied/,
            );
            assert.equal(await scoped.readNote("private/note.md"), "protected");
        } finally {
            await rm(root, { recursive: true, force: true });
        }
    });
});

describe("safePath — path traversal prevention", () => {
    it("blocks ../ traversal", async () => {
        await assert.rejects(() => vault.readNote("../etc/passwd"), /Invalid note path/);
    });

    it("blocks ../../ traversal", async () => {
        await assert.rejects(() => vault.readNote("../../etc/shadow"), /Invalid note path/);
    });

    it("blocks write with traversal", async () => {
        await assert.rejects(() => vault.writeNote("../evil.md", "pwned"), /Invalid note path/);
    });

    it("blocks delete with traversal", async () => {
        await assert.rejects(() => vault.deleteNote("../../important.md"), /Invalid note path/);
    });

    it("blocks listNotes with traversal", async () => {
        await assert.rejects(() => vault.listNotes("../../etc"), /Path traversal blocked/);
    });

    it("allows nested paths within vault", async () => {
        await vault.writeNote("sub/dir/note.md", "ok");
        assert.equal(await vault.readNote("sub/dir/note.md"), "ok");
    });
});

describe("note-path enforcement (GHSA-hfcr-mrh3-c584)", () => {
    it("rejects writing executable code into .obsidian", async () => {
        await assert.rejects(
            () => vault.writeNote(".obsidian/plugins/evil/main.js", "pwned"),
            /Invalid note path/,
        );
    });

    it("rejects reading plugin credentials from .obsidian", async () => {
        await assert.rejects(
            () => vault.readNote(".obsidian/plugins/remotely-save/data.json"),
            /Invalid note path/,
        );
    });

    it("rejects non-.md writes", async () => {
        await assert.rejects(() => vault.writeNote("notes/data.json", "{}"), /Invalid note path/);
    });

    it("blocks a write that escapes via a symlinked directory", async () => {
        const outside = await mkdtemp(join(tmpdir(), "vault-outside-"));
        try {
            // A pre-existing symlink inside the vault pointing outside it.
            const { symlink } = await import("fs/promises");
            await symlink(outside, join(tmpDir, "link"));
            // Target file does not exist yet, so the old ENOENT path returned
            // the lexical path and followed the symlink out of the vault.
            await assert.rejects(
                () => vault.writeNote("link/escaped.md", "pwned"),
                /Path traversal blocked/,
            );
            await assert.equal(
                await readFile(join(outside, "escaped.md"), "utf-8").catch(() => null),
                null,
            );
        } finally {
            await unlink(join(tmpDir, "link")).catch(() => {});
            await rm(outside, { recursive: true, force: true });
        }
    });
});

describe("readNote / writeNote", () => {
    it("returns null for non-existent note", async () => {
        assert.equal(await vault.readNote("nope.md"), null);
    });

    it("writes and reads back a note", async () => {
        await vault.writeNote("hello.md", "# Hello");
        assert.equal(await vault.readNote("hello.md"), "# Hello");
    });

    it("creates intermediate directories", async () => {
        await vault.writeNote("a/b/c/deep.md", "deep");
        assert.equal(await vault.readNote("a/b/c/deep.md"), "deep");
    });

    it("overwrites existing note", async () => {
        await vault.writeNote("overwrite.md", "v1");
        await vault.writeNote("overwrite.md", "v2");
        assert.equal(await vault.readNote("overwrite.md"), "v2");
    });

    it("handles unicode content", async () => {
        const content = "# 日本語テスト\n\nEmoji: 🎉";
        await vault.writeNote("unicode.md", content);
        assert.equal(await vault.readNote("unicode.md"), content);
    });
});

describe("moveNote", () => {
    it("moves a note to a new path", async () => {
        await vault.writeNote("move/src.md", "content");
        assert.equal(await vault.moveNote("move/src.md", "move/dest.md"), true);
        assert.equal(await vault.readNote("move/src.md"), null);
        assert.equal(await vault.readNote("move/dest.md"), "content");
    });

    it("moves across folders", async () => {
        await vault.writeNote("folder-a/note.md", "hello");
        assert.equal(await vault.moveNote("folder-a/note.md", "folder-b/note.md"), true);
        assert.equal(await vault.readNote("folder-b/note.md"), "hello");
    });

    it("returns false if source doesn't exist", async () => {
        assert.equal(await vault.moveNote("nope.md", "dest.md"), false);
    });

    it("refuses to overwrite an existing destination", async () => {
        await vault.writeNote("move/keep-src.md", "source");
        await vault.writeNote("move/keep-dest.md", "destination");
        await assert.rejects(
            vault.moveNote("move/keep-src.md", "move/keep-dest.md"),
            /Destination already exists: move\/keep-dest\.md/,
        );
        assert.equal(await vault.readNote("move/keep-src.md"), "source");
        assert.equal(await vault.readNote("move/keep-dest.md"), "destination");
    });

    it("returns false for a missing source even when the destination exists", async () => {
        await vault.writeNote("move/present.md", "present");
        assert.equal(await vault.moveNote("move/absent.md", "move/present.md"), false);
        assert.equal(await vault.readNote("move/present.md"), "present");
    });

    it("treats a move onto the identical path as a no-op", async () => {
        await vault.writeNote("move/same.md", "same");
        assert.equal(await vault.moveNote("move/same.md", "move/same.md"), true);
        assert.equal(await vault.readNote("move/same.md"), "same");
    });
});

describe("getMetadata", () => {
    it("returns metadata for a note with frontmatter and tags", async () => {
        await vault.writeNote(
            "meta/test.md",
            `---
title: Test
tags: [foo, bar]
---

# Hello #inline-tag

See [[Other Note]]
`,
        );
        const meta = await vault.getMetadata("meta/test.md");
        assert.ok(meta);
        assert.equal(meta!.path, "meta/test.md");
        assert.ok(meta!.size > 0);
        assert.ok(meta!.ctime > 0);
        assert.ok(meta!.mtime > 0);
        assert.equal(meta!.frontmatter.title, "Test");
        assert.ok(meta!.tags.includes("foo"));
        assert.ok(meta!.tags.includes("bar"));
        assert.ok(meta!.tags.includes("inline-tag"));
        assert.ok(meta!.links.includes("Other Note"));
    });

    it("returns null for non-existent note", async () => {
        assert.equal(await vault.getMetadata("nope.md"), null);
    });
});

describe("deleteNote", () => {
    it("deletes an existing note", async () => {
        await vault.writeNote("del.md", "x");
        assert.equal(await vault.deleteNote("del.md"), true);
        assert.equal(await vault.readNote("del.md"), null);
    });

    it("returns false for non-existent note", async () => {
        assert.equal(await vault.deleteNote("nope.md"), false);
    });
});

describe("listNotes", () => {
    beforeAll(async () => {
        // Create a known set of files
        await vault.writeNote("list/a.md", "a");
        await vault.writeNote("list/b.md", "b");
        await vault.writeNote("list/sub/c.md", "c");
        // Non-md file should be excluded
        const txtPath = join(tmpDir, "list", "ignore.txt");
        await writeFile(txtPath, "not a note");
    });

    it("lists all .md files recursively", async () => {
        const notes = await vault.listNotes("list/");
        assert.ok(notes.includes("list/a.md"));
        assert.ok(notes.includes("list/b.md"));
        assert.ok(notes.includes("list/sub/c.md"));
        assert.ok(!notes.some((n) => n.includes(".txt")));
    });

    it("filters by folder", async () => {
        const notes = await vault.listNotes("list/sub/");
        assert.deepEqual(notes, ["list/sub/c.md"]);
    });

    it("normalizes folder without trailing slash", async () => {
        const with_ = await vault.listNotes("list/");
        const without = await vault.listNotes("list");
        assert.deepEqual(with_, without);
    });

    it("returns empty array for non-existent folder", async () => {
        assert.deepEqual(await vault.listNotes("nonexistent/"), []);
    });

    it("throws instead of returning [] when the vault cannot be listed", async (t) => {
        if (process.getuid?.() === 0) return t.skip("root ignores directory permissions");
        const root = await mkdtemp(join(tmpdir(), "vault-unlisted-"));
        try {
            await mkdir(join(root, "locked"));
            await writeFile(join(root, "locked/a.md"), "a");
            const local = new LocalVault(root);
            await chmod(join(root, "locked"), 0);
            await assert.rejects(local.listNotesWithMtime("locked"), /Failed to list notes/);
            await chmod(join(root, "locked"), 0o755);
            await rm(root, { recursive: true, force: true });
            await assert.rejects(local.listNotesWithMtime(), /Failed to list notes/);
        } finally {
            await chmod(join(root, "locked"), 0o755).catch(() => {});
            await rm(root, { recursive: true, force: true });
        }
    });

    it("returns sorted results", async () => {
        const notes = await vault.listNotes("list/");
        const sorted = [...notes].sort();
        assert.deepEqual(notes, sorted);
    });
});

describe("edit_note operations (string manipulation)", () => {
    // These test the same logic as the edit_note tool: read, transform, write back

    it("append adds content to end", async () => {
        await vault.writeNote("edit/append.md", "line one");
        const existing = (await vault.readNote("edit/append.md"))!;
        const updated = existing + "\nline two";
        await vault.writeNote("edit/append.md", updated);
        assert.equal(await vault.readNote("edit/append.md"), "line one\nline two");
    });

    it("append adds newline if missing", async () => {
        await vault.writeNote("edit/append-nl.md", "line one\n");
        const existing = (await vault.readNote("edit/append-nl.md"))!;
        const updated = existing.endsWith("\n") ? existing + "line two" : existing + "\nline two";
        await vault.writeNote("edit/append-nl.md", updated);
        assert.equal(await vault.readNote("edit/append-nl.md"), "line one\nline two");
    });

    it("prepend inserts after frontmatter", async () => {
        const original = "---\ntitle: Test\n---\nBody here";
        await vault.writeNote("edit/prepend.md", original);
        const existing = (await vault.readNote("edit/prepend.md"))!;
        const fmMatch = existing.match(/^---\r?\n[\s\S]*?\r?\n---\r?\n/);
        let updated: string;
        if (fmMatch) {
            const afterFm = fmMatch[0].length;
            updated = existing.slice(0, afterFm) + "New top line\n" + existing.slice(afterFm);
        } else {
            updated = "New top line\n" + existing;
        }
        await vault.writeNote("edit/prepend.md", updated);
        assert.equal(
            await vault.readNote("edit/prepend.md"),
            "---\ntitle: Test\n---\nNew top line\nBody here",
        );
    });

    it("prepend goes to top when no frontmatter", async () => {
        await vault.writeNote("edit/prepend-nofm.md", "Body here");
        const existing = (await vault.readNote("edit/prepend-nofm.md"))!;
        const fmMatch = existing.match(/^---\r?\n[\s\S]*?\r?\n---\r?\n/);
        const updated = fmMatch
            ? existing.slice(0, fmMatch[0].length) + "New top\n" + existing.slice(fmMatch[0].length)
            : "New top\n" + existing;
        await vault.writeNote("edit/prepend-nofm.md", updated);
        assert.equal(await vault.readNote("edit/prepend-nofm.md"), "New top\nBody here");
    });

    it("replace swaps exact match", async () => {
        await vault.writeNote("edit/replace.md", "Hello world, hello universe");
        const existing = (await vault.readNote("edit/replace.md"))!;
        const oldText = "world";
        const idx = existing.indexOf(oldText);
        assert.notEqual(idx, -1);
        const updated = existing.slice(0, idx) + "earth" + existing.slice(idx + oldText.length);
        await vault.writeNote("edit/replace.md", updated);
        assert.equal(await vault.readNote("edit/replace.md"), "Hello earth, hello universe");
    });

    it("replace fails if old_text not found", async () => {
        await vault.writeNote("edit/replace-miss.md", "Some content");
        const existing = (await vault.readNote("edit/replace-miss.md"))!;
        const idx = existing.indexOf("nonexistent");
        assert.equal(idx, -1);
    });

    it("replace detects multiple matches", async () => {
        await vault.writeNote("edit/replace-multi.md", "foo bar foo baz");
        const existing = (await vault.readNote("edit/replace-multi.md"))!;
        const oldText = "foo";
        const idx = existing.indexOf(oldText);
        const secondIdx = existing.indexOf(oldText, idx + 1);
        assert.notEqual(secondIdx, -1, "should find multiple matches");
    });
});

// macOS FSEvents can drop events written right after the stream opens; let it settle first.
async function createWatcher(dir: string, index: SearchIndex) {
    const watcher = watch(dir, { recursive: true }, async (_event, filename) => {
        if (!filename || !filename.endsWith(".md")) return;
        const notePath = filename.replace(/\\/g, "/");
        try {
            const content = await readFile(join(dir, notePath), "utf-8");
            index.update(notePath, content);
        } catch {
            index.remove(notePath);
        }
    });
    await new Promise((r) => setTimeout(r, 200));
    return watcher;
}

describe("file watcher integration", () => {
    it("detects new file and updates search index", async () => {
        const watchDir = await mkdtemp(join(tmpdir(), "watch-test-"));
        const searchIndex = new SearchIndex();
        const watcher = await createWatcher(watchDir, searchIndex);

        try {
            await writeFile(join(watchDir, "external.md"), "external edit keyword banana");
            // fs.watch delivery time varies under load; poll instead of sleeping a fixed time.
            await vi.waitFor(
                () =>
                    assert.ok(
                        searchIndex.listPaths().includes("external.md"),
                        "should index external.md",
                    ),
                { timeout: 3000 },
            );

            await unlink(join(watchDir, "external.md"));
            await vi.waitFor(
                () =>
                    assert.ok(
                        !searchIndex.listPaths().includes("external.md"),
                        "should remove deleted file",
                    ),
                { timeout: 3000 },
            );
        } finally {
            watcher.close();
            await rm(watchDir, { recursive: true, force: true });
        }
    });

    it("ignores non-md files", async () => {
        const watchDir = await mkdtemp(join(tmpdir(), "watch-test-"));
        const searchIndex = new SearchIndex();
        const watcher = await createWatcher(watchDir, searchIndex);

        try {
            await writeFile(join(watchDir, "image.png"), "not a note");
            await new Promise((r) => setTimeout(r, 500));
            assert.equal(searchIndex.size, 0);
        } finally {
            watcher.close();
            await rm(watchDir, { recursive: true, force: true });
        }
    });

    it("detects files in subdirectories", async () => {
        const watchDir = await mkdtemp(join(tmpdir(), "watch-test-"));
        const searchIndex = new SearchIndex();
        const watcher = await createWatcher(watchDir, searchIndex);

        try {
            await mkdir(join(watchDir, "sub", "dir"), { recursive: true });
            await writeFile(join(watchDir, "sub", "dir", "deep.md"), "deep nested content mango");
            await vi.waitFor(
                () =>
                    assert.ok(
                        searchIndex.listPaths().some((p) => p.includes("deep.md")),
                        "should index deep nested file",
                    ),
                { timeout: 3000 },
            );
        } finally {
            watcher.close();
            await rm(watchDir, { recursive: true, force: true });
        }
    });
});
