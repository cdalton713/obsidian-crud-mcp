import { describe, it } from "vitest";
import assert from "node:assert/strict";
import {
    countByFolder,
    describeIndexState,
    describeListing,
    describeNoMatch,
    describeOmitted,
    MAX_NAMED_FOLDERS,
} from "../../src/tools/list-format.js";

/**
 * Wording tests for list_notes responses (issue #19): a truncated, filtered
 * or still-indexing listing must say so on the first line, and a zero-hit
 * filter must not claim the vault is empty.
 */

const ready = { state: "ready" as const, size: 206 };

function paths(folder: string, n: number): string[] {
    return Array.from({ length: n }, (_, i) => `${folder}/note-${i}.md`);
}

describe("countByFolder", () => {
    it("groups by immediate parent, largest first, root as (root)", () => {
        const groups = countByFolder([...paths("a/b", 2), ...paths("a", 3), "top.md"]);
        assert.deepEqual(groups, [
            { folder: "a", count: 3 },
            { folder: "a/b", count: 2 },
            { folder: "(root)", count: 1 },
        ]);
    });
});

describe("describeOmitted", () => {
    it("names folders with counts", () => {
        assert.equal(
            describeOmitted([...paths("folder-a", 41), ...paths("folder-b", 9), "loose.md"]),
            "Omitted 51: folder-a/ (41), folder-b/ (9), (root) (1).",
        );
    });

    it("caps the named folders and keeps the arithmetic intact", () => {
        const omitted: string[] = [];
        for (let i = 0; i < MAX_NAMED_FOLDERS + 3; i++)
            omitted.push(...paths(`f${String(i).padStart(2, "0")}`, 2));
        const text = describeOmitted(omitted);
        assert.ok(text.startsWith(`Omitted ${omitted.length}: `));
        assert.equal((text.match(/\(2\)/g) ?? []).length, MAX_NAMED_FOLDERS);
        assert.ok(text.endsWith(", and 3 more folders (6 notes)."));
    });
});

describe("describeIndexState", () => {
    it("is silent when ready", () => {
        assert.equal(describeIndexState(ready), "");
    });
    it("flags a partial index while building", () => {
        assert.equal(
            describeIndexState({ state: "building", size: 57 }),
            " Index: catching up (57 notes indexed so far); this list may be incomplete.",
        );
    });
    it("does not call a vault-served list incomplete", () => {
        assert.equal(
            describeIndexState({ state: "building", size: 0, servedByVault: true }),
            " Index: catching up (0 notes indexed so far); this list was read directly from the vault.",
        );
    });
    it("flags a failed rebuild instead of claiming progress", () => {
        const text = describeIndexState({ state: "failed", size: 0 });
        assert.ok(text.includes("rebuild failed"));
        assert.ok(!text.includes("catching up"));
    });
});

describe("describeNoMatch", () => {
    it("reserves 'Vault is empty.' for an unfiltered empty vault", () => {
        assert.equal(
            describeNoMatch({ vaultTotal: 0, filters: [], index: { state: "ready", size: 0 } }),
            "Vault is empty.",
        );
    });
    it("says what a zero-hit filter tested and how big the vault is", () => {
        assert.equal(
            describeNoMatch({
                vaultTotal: 206,
                filters: ['name="something-misspelled"'],
                index: ready,
            }),
            'No notes match name="something-misspelled" (vault has 206 notes).',
        );
    });
    it("scopes a filtered zero-hit to the folder", () => {
        assert.equal(
            describeNoMatch({
                vaultTotal: 206,
                folder: "y",
                folderTotal: 41,
                filters: ['tag="x"'],
                index: ready,
            }),
            'No notes match tag="x" in folder "y/" (folder has 41 notes).',
        );
    });
    it("keeps the plain folder wording when only the folder filter missed", () => {
        assert.equal(
            describeNoMatch({
                vaultTotal: 206,
                folder: "y",
                folderTotal: 0,
                filters: [],
                index: ready,
            }),
            "No notes found in folder: y",
        );
    });
    it("appends the index state to an empty answer while building", () => {
        const text = describeNoMatch({
            vaultTotal: 0,
            filters: [],
            index: { state: "building", size: 0 },
        });
        assert.ok(text.startsWith("Vault is empty."));
        assert.ok(text.includes("catching up"));
    });
});

describe("describeListing", () => {
    const base = {
        vaultTotal: 206,
        filters: [],
        sortBy: "name" as const,
        limit: 100,
        omitted: [],
        index: ready,
    };

    it("states the total for a complete unfiltered listing", () => {
        assert.equal(
            describeListing({ ...base, shown: 206, matched: 206 }),
            "206 notes (sorted by name).",
        );
    });

    it("uses the singular", () => {
        assert.equal(
            describeListing({ ...base, shown: 1, matched: 1, vaultTotal: 1 }),
            "1 note (sorted by name).",
        );
    });

    it("uses the singular verb for one match", () => {
        assert.equal(
            describeListing({
                ...base,
                vaultTotal: 3,
                filters: ['tag="intro"'],
                shown: 1,
                matched: 1,
            }),
            '1 note matches tag="intro" (vault has 3 notes, sorted by name).',
        );
    });

    it("keeps the filter and the vault total visible on a complete filtered listing", () => {
        assert.equal(
            describeListing({ ...base, shown: 5, matched: 5, filters: ['name="sop"'] }),
            '5 notes match name="sop" (vault has 206 notes, sorted by name).',
        );
    });

    it("scopes a folder listing to the folder count", () => {
        assert.equal(
            describeListing({ ...base, shown: 41, matched: 41, folder: "y", folderTotal: 41 }),
            '41 notes in folder "y/" (folder has 41 notes, sorted by name).',
        );
        assert.equal(
            describeListing({
                ...base,
                shown: 5,
                matched: 5,
                folder: "y",
                folderTotal: 41,
                filters: ['tag="x"'],
            }),
            '5 notes match tag="x" in folder "y/" (folder has 41 notes, sorted by name).',
        );
    });

    it("does not present a partial index size as the vault total", () => {
        assert.equal(
            describeListing({
                ...base,
                shown: 5,
                matched: 5,
                filters: ['name="sop"'],
                vaultTotal: 57,
                index: { state: "building", size: 57 },
            }),
            '5 notes match name="sop" (sorted by name). Index: catching up (57 notes indexed so far); this list may be incomplete.',
        );
        assert.equal(
            describeNoMatch({
                vaultTotal: 57,
                filters: ['name="x"'],
                index: { state: "building", size: 57 },
            }),
            'No notes match name="x". Index: catching up (57 notes indexed so far); this list may be incomplete.',
        );
    });

    it("keeps the vault total when the vault itself served the list", () => {
        assert.equal(
            describeListing({
                ...base,
                shown: 5,
                matched: 5,
                filters: ['name="sop"'],
                vaultTotal: 206,
                index: { state: "building", size: 0, servedByVault: true },
            }),
            '5 notes match name="sop" (vault has 206 notes, sorted by name). Index: catching up (0 notes indexed so far); this list was read directly from the vault.',
        );
    });

    it("puts the truncation status first with the omitted folders", () => {
        const omitted = [
            ...paths("folder-a", 41),
            ...paths("folder-b", 9),
            ...paths("folder-c", 56),
        ];
        assert.equal(
            describeListing({ ...base, shown: 100, matched: 206, omitted }),
            "Showing 100 of 206 notes (sorted by name, limit=100). Omitted 106: folder-c/ (56), folder-a/ (41), folder-b/ (9). Raise `limit` or add a `folder` filter.",
        );
    });

    it("combines filter, truncation and index state on one line", () => {
        const text = describeListing({
            ...base,
            shown: 2,
            matched: 3,
            filters: ['tag="x"'],
            sortBy: "modified",
            limit: 2,
            omitted: ["daily/2026-03-24.md"],
            index: { state: "building", size: 57 },
        });
        assert.equal(
            text,
            'Showing 2 of 3 notes matching tag="x" (sorted by modified, limit=2). Omitted 1: daily/ (1). Raise `limit` or narrow the filter. Index: catching up (57 notes indexed so far); this list may be incomplete.',
        );
    });
});
