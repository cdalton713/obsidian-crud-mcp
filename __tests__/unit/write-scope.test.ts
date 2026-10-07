import { test } from "vitest";
import assert from "node:assert/strict";
import { parseWriteFolders, isPathWritable } from "../../src/vault/write-scope.js";

test("parseWriteFolders: unset returns null (unrestricted)", () => {
    assert.equal(parseWriteFolders(undefined), null);
});

test("parseWriteFolders: empty or whitespace-only returns null", () => {
    assert.equal(parseWriteFolders(""), null);
    assert.equal(parseWriteFolders("  "), null);
    assert.equal(parseWriteFolders(","), null);
    assert.equal(parseWriteFolders(" , "), null);
});

test("parseWriteFolders: splits, trims, strips slashes", () => {
    assert.deepEqual(parseWriteFolders("MCP"), ["MCP"]);
    assert.deepEqual(parseWriteFolders("MCP, Inbox"), ["MCP", "Inbox"]);
    assert.deepEqual(parseWriteFolders("/MCP/, Inbox/"), ["MCP", "Inbox"]);
    assert.deepEqual(parseWriteFolders("projects/active"), ["projects/active"]);
});

test("isPathWritable: null folders allows everything", () => {
    assert.equal(isPathWritable("anything/at/all.md", null), true);
});

test("isPathWritable: allows paths inside a writable folder", () => {
    const folders = ["MCP", "Inbox"];
    assert.equal(isPathWritable("MCP/note.md", folders), true);
    assert.equal(isPathWritable("MCP/deep/nested/note.md", folders), true);
    assert.equal(isPathWritable("Inbox/todo.md", folders), true);
});

test("isPathWritable: denies paths outside writable folders", () => {
    const folders = ["MCP"];
    assert.equal(isPathWritable("daily/2026-07-30.md", folders), false);
    assert.equal(isPathWritable("root-note.md", folders), false);
});

test("isPathWritable: folder-boundary aware — no partial prefix match", () => {
    const folders = ["MCP"];
    assert.equal(isPathWritable("MCP-private/note.md", folders), false);
    assert.equal(isPathWritable("MCPx/note.md", folders), false);
});

test("isPathWritable: a root file named like the folder is not inside it", () => {
    assert.equal(isPathWritable("MCP", ["MCP"]), false);
    assert.equal(isPathWritable("MCP.md", ["MCP"]), false);
});

test("isPathWritable: matching is case-sensitive", () => {
    assert.equal(isPathWritable("mcp/note.md", ["MCP"]), false);
});

test("isPathWritable: leading slashes are normalized", () => {
    assert.equal(isPathWritable("/MCP/note.md", ["MCP"]), true);
});

test("isPathWritable: traversal segments are denied", () => {
    const folders = ["MCP"];
    assert.equal(isPathWritable("MCP/../secret.md", folders), false);
    assert.equal(isPathWritable("MCP/../../etc/passwd", folders), false);
    assert.equal(isPathWritable("../MCP/note.md", folders), false);
    // Windows resolves backslash traversal even after a valid prefix
    assert.equal(isPathWritable("MCP/..\\daily\\evil.md", folders), false);
    assert.equal(isPathWritable("MCP\\..\\daily\\evil.md", folders), false);
});

test("isPathWritable: nested writable folder", () => {
    const folders = ["projects/active"];
    assert.equal(isPathWritable("projects/active/note.md", folders), true);
    assert.equal(isPathWritable("projects/archive/note.md", folders), false);
    assert.equal(isPathWritable("projects/note.md", folders), false);
});
