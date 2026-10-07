import { describe, it } from "vitest";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile, mkdir, symlink, access, readdir } from "fs/promises";
import { tmpdir } from "os";
import { join } from "path";
import { LocalVault } from "../../src/vault/vault-local.js";

async function withRoot(fn: (root: string) => Promise<void>) {
    const root = await mkdtemp(join(tmpdir(), "vault-case-"));
    try {
        await fn(root);
    } finally {
        await rm(root, { recursive: true, force: true });
    }
}

async function isCaseInsensitive(root: string): Promise<boolean> {
    await writeFile(join(root, "probe"), "");
    return access(join(root, "PROBE")).then(
        () => true,
        () => false,
    );
}

describe("local write folders — canonical folder comparison", () => {
    it("accepts a write folder whose on-disk name differs only in case", async (t) => {
        await withRoot(async (root) => {
            if (!(await isCaseInsensitive(root))) return t.skip("filesystem is case-sensitive");
            await mkdir(join(root, "mcp"));
            await writeFile(join(root, "mcp/existing.md"), "old");
            const scoped = new LocalVault(root, ["MCP"]);
            assert.equal(await scoped.writeNote("MCP/new.md", "new"), true);
            assert.equal(await scoped.writeNote("MCP/existing.md", "changed"), true);
            assert.equal(await scoped.readNote("mcp/existing.md"), "changed");
            assert.equal(await scoped.moveNote("MCP/new.md", "MCP/sub/moved.md"), true);
            assert.equal(await scoped.deleteNote("MCP/sub/moved.md"), true);
        });
    });

    it("performs a case-only rename instead of refusing it as an existing destination", async (t) => {
        await withRoot(async (root) => {
            if (!(await isCaseInsensitive(root))) return t.skip("filesystem is case-sensitive");
            await writeFile(join(root, "Note.md"), "keep");
            const local = new LocalVault(root);
            assert.equal(await local.moveNote("Note.md", "note.md"), true);
            assert.ok((await readdir(root)).includes("note.md"));
            assert.ok(!(await readdir(root)).includes("Note.md"));
            assert.equal(await local.readNote("note.md"), "keep");
        });
    });

    it("refuses to move onto a different existing note on a case-insensitive filesystem", async (t) => {
        await withRoot(async (root) => {
            if (!(await isCaseInsensitive(root))) return t.skip("filesystem is case-sensitive");
            await writeFile(join(root, "a.md"), "a");
            await writeFile(join(root, "B.md"), "b");
            const local = new LocalVault(root);
            await assert.rejects(local.moveNote("a.md", "b.md"), /Destination already exists/);
            assert.equal(await local.readNote("B.md"), "b");
            assert.equal(await local.readNote("a.md"), "a");
        });
    });

    it("still denies symlink escapes out of a write folder", async () => {
        await withRoot(async (root) => {
            await mkdir(join(root, "w"));
            await mkdir(join(root, "other"));
            await writeFile(join(root, "other/note.md"), "protected");
            await symlink(join(root, "other"), join(root, "w/link"));
            await symlink(join(root, "other/note.md"), join(root, "w/link.md"));
            const scoped = new LocalVault(root, ["w"]);
            await assert.rejects(
                () => scoped.writeNote("w/link.md", "changed"),
                /Write access denied/,
            );
            await assert.rejects(
                () => scoped.writeNote("w/link/new.md", "changed"),
                /Write access denied/,
            );
            assert.equal(await scoped.readNote("other/note.md"), "protected");
        });
    });

    it("does not widen scope when the write folder itself is a symlink", async () => {
        await withRoot(async (root) => {
            await mkdir(join(root, "private"));
            await writeFile(join(root, "private/note.md"), "protected");
            await symlink(join(root, "private"), join(root, "MCP"));
            const scoped = new LocalVault(root, ["MCP"]);
            await assert.rejects(
                () => scoped.writeNote("private/note.md", "changed"),
                /Write access denied/,
            );
            assert.equal(await scoped.readNote("private/note.md"), "protected");
        });
    });
});
