import { describe, it } from "vitest";
import assert from "node:assert/strict";
import { validateNotePath, isValidNotePath } from "../../src/notes/note-path.js";

describe("validateNotePath", () => {
    it("accepts ordinary note paths", () => {
        for (const p of [
            "note.md",
            "daily/2026-03-23.md",
            "a/b/c/deep note.md",
            "Проект/заметка.md",
        ]) {
            assert.doesNotThrow(() => validateNotePath(p), p);
        }
    });

    it("rejects paths that are not .md (attachments, config, code)", () => {
        for (const p of [
            "_remotely-save-metadata-on-remote.json",
            "note.txt",
            "drawing.canvas",
            "image.png",
            "folder/data.json",
        ]) {
            assert.throws(() => validateNotePath(p), /Invalid note path/, p);
        }
    });

    it("rejects colons, which Obsidian disallows in file names", () => {
        for (const p of ["a:b.md", "folder/12:30 meeting.md"]) {
            assert.throws(() => validateNotePath(p), /Invalid note path/, p);
        }
    });

    it("rejects hidden / dot-folder paths even when they end in .md", () => {
        for (const p of [
            ".obsidian/plugins/evil/main.js",
            ".obsidian/plugins/remotely-save/data.json",
            ".obsidian/config.md",
            ".hidden.md",
            "notes/.secret.md",
            ".trash/old.md",
        ]) {
            assert.throws(() => validateNotePath(p), /Invalid note path/, p);
        }
    });

    it("rejects traversal, absolute, NUL, and empty segments", () => {
        for (const p of [
            "../escape.md",
            "a/../../etc/passwd.md",
            "/abs/note.md",
            "a\0b.md",
            "a//b.md",
            "folder/.md",
        ]) {
            assert.throws(() => validateNotePath(p), /Invalid note path/, p);
        }
    });

    it("rejects empty and over-long paths", () => {
        assert.throws(() => validateNotePath(""), /Invalid note path/);
        assert.throws(() => validateNotePath("a".repeat(1001) + ".md"), /Invalid note path/);
    });

    it("rejects backslash dot-segments (Windows hidden dirs)", () => {
        assert.throws(() => validateNotePath("sub\\.obsidian\\evil.md"), /Invalid note path/);
    });
});

describe("isValidNotePath", () => {
    it("is the boolean form of validateNotePath", () => {
        assert.equal(isValidNotePath("daily/note.md"), true);
        assert.equal(isValidNotePath(".obsidian/x.md"), false);
        assert.equal(isValidNotePath("note.txt"), false);
    });
});
