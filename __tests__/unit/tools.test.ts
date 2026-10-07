import { test } from "vitest";
import assert from "node:assert/strict";
import {
    registerTools,
    tagMatches,
    READ_NOTE_MAX_RESULT_SIZE_CHARS,
    type ToolRegistrar,
} from "../../src/tools/tools.js";

function captureTools(): { name: string; _meta?: Record<string, unknown> }[] {
    const tools: { name: string; _meta?: Record<string, unknown> }[] = [];
    const server: ToolRegistrar = { addTool: (tool) => void tools.push(tool) };
    registerTools(server, {} as never, {} as never, "vault");
    return tools;
}

test("read_note declares anthropic/maxResultSizeChars so Claude Code returns whole notes inline", () => {
    const readNote = captureTools().find((t) => t.name === "read_note");
    assert.ok(readNote, "read_note registered");
    assert.equal(readNote._meta?.["anthropic/maxResultSizeChars"], READ_NOTE_MAX_RESULT_SIZE_CHARS);
    assert.ok(
        READ_NOTE_MAX_RESULT_SIZE_CHARS > 50_000 && READ_NOTE_MAX_RESULT_SIZE_CHARS <= 500_000,
    );
});

test("no other tool carries the annotation", () => {
    for (const tool of captureTools().filter((t) => t.name !== "read_note")) {
        assert.equal(tool._meta, undefined, `${tool.name} should not declare _meta`);
    }
});

test("tagMatches ignores a leading #, ignores case, and includes nested tags", () => {
    assert.equal(tagMatches(["project"], "#project"), true);
    assert.equal(tagMatches(["Project"], "project"), true);
    assert.equal(tagMatches(["project/sub"], "project"), true);
    assert.equal(tagMatches(["project/sub/deep"], "PROJECT/Sub"), true);
    assert.equal(tagMatches(["projects"], "project"), false);
    assert.equal(tagMatches(["project"], "project/sub"), false);
    assert.equal(tagMatches([], "project"), false);
});
