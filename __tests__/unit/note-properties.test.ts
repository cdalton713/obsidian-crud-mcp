import { test } from "vitest";
import assert from "node:assert/strict";
import { readProperties, updateProperties } from "../../src/notes/note-properties.js";
import { noteStructure, noteTasks, selectNoteRange } from "../../src/notes/note-structure.js";

test("property updates can replace an anchor without changing an unrelated alias value", () => {
    const updated = updateProperties(
        "---\nstatus: &s todo\nother: *s\n---\nBody",
        { status: "done" },
        [],
    );
    assert.deepEqual(readProperties(updated), { status: "done", other: "todo" });
    assert.ok(updated.endsWith("---\nBody"));
});

test("property updates preserve unrelated YAML values and the exact body", () => {
    const cases: [string, string][] = [
        ["tags: [a, b]\n", "flow list"],
        ["tags:\n- a\n- b\n", "unindented block list"],
        ['title:   "Spaced"\n', "extra spacing"],
        ["summary: >\n  a folded value that\n  spans lines\n\n", "folded scalar"],
        ["# leading comment\nother: 1 # trailing\n\n# before\n", "comments"],
    ];
    for (const [unrelated, label] of cases) {
        const before = `---\n${unrelated}status: draft\nafter:   [x,y]\n---\nBody\n`;
        const updated = updateProperties(before, { status: "done" }, []);
        assert.deepEqual(
            readProperties(updated),
            { ...readProperties(before), status: "done" },
            label,
        );
        assert.ok(updated.endsWith("---\nBody\n"), label);
    }
});

test("property updates serialize typed values without preserving YAML layout", () => {
    const updated = updateProperties(
        "---\nstatus: draft # workflow\ntitle: 'Original'\naliases: [x, y]\n---\n",
        { status: "done", title: "New", aliases: ["z", "w"] },
        [],
    );
    assert.deepEqual(readProperties(updated), {
        status: "done",
        title: "New",
        aliases: ["z", "w"],
    });
    assert.doesNotMatch(updated, /# workflow/);
});

test("property updates preserve CRLF and BOM and append new keys at the end of the block", () => {
    assert.equal(
        updateProperties(
            "﻿---\r\ntags: [a, b]\r\nstatus: draft\r\n---\r\nBody\r\n",
            { status: "done", count: 3 },
            [],
        ),
        "﻿---\r\ntags: [a, b]\r\nstatus: done\r\ncount: 3\r\n---\r\nBody\r\n",
    );
});

test("property removal deletes selected keys and preserves other values", () => {
    assert.equal(
        updateProperties(
            "---\n# keep\nnested:\n  k: [1, 2]\n  j: x\nstatus: draft\n---\nBody",
            {},
            ["nested"],
        ),
        "---\nstatus: draft\n---\nBody",
    );
    assert.equal(
        updateProperties("---\na:  1\nlast: 2 # z\n---\nBody", {}, ["last"]),
        "---\na: 1\n---\nBody",
    );
    assert.equal(updateProperties("---\nonly: x\n---\nBody", {}, ["only"]), "Body");
});

test("property updates keep top-level and inline comments", () => {
    assert.equal(
        updateProperties(
            "---\n# Keep me\ntitle: Original # inline\nstatus: draft\nold: remove\n---\n\n# Body\n",
            { status: "done", count: 3 },
            ["old"],
        ),
        "---\n# Keep me\ntitle: Original # inline\nstatus: done\ncount: 3\n---\n\n# Body\n",
    );
});

test("property updates keep the formatting of untouched keys", () => {
    const untouched =
        "quoted: \"double\"\nsingle: 'single'\nflow: [a, b]\nmap: {k: v}\nblock: |\n  line one\n  line two\n\nnum: 0x1f\n";
    assert.equal(
        updateProperties(`---\n${untouched}status: draft\n---\nBody`, { status: "done" }, []),
        `---\n${untouched}status: done\n---\nBody`,
    );
});

test("property updates keep key order and update keys in place", () => {
    assert.equal(
        updateProperties("---\nz: 1\nm: 2\na: 3\n---\n", { m: "two", b: true }, []),
        "---\nz: 1\nm: two\na: 3\nb: true\n---\n",
    );
});

test("property removal keeps neighbouring comments and blank lines", () => {
    assert.equal(
        updateProperties(
            "---\n# header\n\na: 1 # about a\n\n# about b\nb: 2\nc: 3 # about c\n# trailing\n---\nBody",
            {},
            ["b"],
        ),
        "---\n# header\n\na: 1 # about a\nc: 3 # about c\n\n# trailing\n---\nBody",
    );
    assert.equal(
        updateProperties("---\n# header\n\nonly: x\n---\nBody", {}, ["only"]),
        "---\n# header\n---\nBody",
    );
});

test("property updates create frontmatter in notes without it", () => {
    assert.equal(
        updateProperties("# Body\n", { status: "done", tags: ["a"] }, []),
        "---\nstatus: done\ntags:\n  - a\n---\n# Body\n",
    );
    assert.equal(
        updateProperties("---\n---\nBody", { status: "done" }, []),
        "---\nstatus: done\n---\nBody",
    );
});

test("an opening horizontal rule without a closing delimiter is Markdown, not frontmatter", () => {
    const content = "---\n# A\n- [ ] task\n";
    assert.deepEqual(
        noteStructure(content).headings.map((h) => h.heading),
        [["A"]],
    );
    assert.deepEqual(
        noteTasks(content).map((t) => [t.line, t.text]),
        [[3, "task"]],
    );
    assert.equal(content.slice(selectNoteRange(content, ["A"]).start), "- [ ] task\n");
    assert.deepEqual(readProperties(content), {});
    assert.throws(() => updateProperties(content, { status: "done" }, []), /closing/);
});
