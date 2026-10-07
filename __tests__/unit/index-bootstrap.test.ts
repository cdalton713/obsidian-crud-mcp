import { describe, it } from "vitest";
import assert from "node:assert/strict";
import { SearchIndex } from "../../src/search/search.js";
import { syncSearchIndex } from "../../src/server/index-bootstrap.js";
import type { VaultBackend } from "../../src/vault/vault-backend.js";
import type { NoteListing } from "../../src/types/vault-backend.js";

function fakeVault(notes: Record<string, { content: string; mtime: number }>) {
    const reads: string[] = [];
    const vault: VaultBackend = {
        init: async () => {},
        close: async () => {},
        readNote: async (path) => {
            reads.push(path);
            return notes[path]?.content ?? null;
        },
        writeNote: async () => false,
        deleteNote: async () => false,
        moveNote: async () => false,
        getMetadata: async () => null,
        listNotes: async () => Object.keys(notes),
        listNotesWithMtime: async (): Promise<NoteListing[]> =>
            Object.entries(notes).map(([path, { mtime }]) => ({ path, mtime })),
    };
    return { vault, reads };
}

function countSaves(index: SearchIndex): () => number {
    let saves = 0;
    index.saveToDisk = async () => {
        saves++;
    };
    return () => saves;
}

describe("syncSearchIndex", () => {
    it("skips reading notes whose persisted mtime is unchanged and prunes deleted ones", async () => {
        const index = new SearchIndex();
        countSaves(index);
        index.update("same.md", "links to [[kept]]", 10);
        index.update("changed.md", "old", 10);
        index.update("deleted.md", "gone", 10);
        const { vault, reads } = fakeVault({
            "same.md": { content: "links to [[kept]]", mtime: 10 },
            "changed.md": { content: "#fresh", mtime: 20 },
            "new.md": { content: "new", mtime: 30 },
        });

        await syncSearchIndex(vault, index);

        assert.deepEqual(reads.sort(), ["changed.md", "new.md"]);
        assert.deepEqual(index.listPaths(), ["changed.md", "new.md", "same.md"]);
        assert.deepEqual(index.getTags("changed.md"), ["fresh"]);
        assert.deepEqual(index.getBacklinks("kept"), ["same.md"]);
        assert.equal(index.state, "ready");
    });

    it("reads changed notes concurrently", async () => {
        const index = new SearchIndex();
        countSaves(index);
        const { vault } = fakeVault(
            Object.fromEntries(
                Array.from({ length: 6 }, (_, i) => [
                    `n${i}.md`,
                    { content: `#t${i}`, mtime: i + 1 },
                ]),
            ),
        );
        let inFlight = 0;
        let peak = 0;
        const read = vault.readNote;
        vault.readNote = async (path) => {
            peak = Math.max(peak, ++inFlight);
            await new Promise((resolve) => setTimeout(resolve, 5));
            inFlight--;
            return read(path);
        };

        await syncSearchIndex(vault, index);

        assert.ok(peak > 1, `reads should overlap (peak ${peak})`);
        assert.equal(index.size, 6);
        assert.deepEqual(index.getTags("n3.md"), ["t3"]);
        assert.equal(index.getMtime("n3.md"), 4);
    });

    it("prunes every stale entry when the vault is empty", async () => {
        const index = new SearchIndex();
        countSaves(index);
        index.update("a.md", "a", 1);
        index.update("b.md", "b", 2);
        const { vault } = fakeVault({});

        await syncSearchIndex(vault, index);

        assert.equal(index.size, 0);
    });

    it("leaves the index intact when listing the vault fails", async () => {
        const index = new SearchIndex();
        countSaves(index);
        index.update("a.md", "a", 1);
        const { vault } = fakeVault({});
        vault.listNotesWithMtime = async () => {
            throw new Error("EACCES");
        };

        await assert.rejects(syncSearchIndex(vault, index), /EACCES/);
        assert.deepEqual(index.listPaths(), ["a.md"]);
    });
});
