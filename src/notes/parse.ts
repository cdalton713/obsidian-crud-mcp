import type { NoteMetadata } from "../types/parse.js";
/**
 * Parse Obsidian markdown content for frontmatter, tags, and links.
 */

import { readProperties, splitFrontmatter } from "./note-properties.js";

export function parseFrontmatterAndLinks(content: string): NoteMetadata {
    const frontmatter = readProperties(content);
    const tags = new Set<string>();
    const links: string[] = [];
    const propertyTags = frontmatter.tags;
    const tagValues = Array.isArray(propertyTags)
        ? propertyTags
        : typeof propertyTags === "string"
          ? propertyTags.split(",")
          : [];

    for (const value of tagValues) {
        if (typeof value !== "string" && typeof value !== "number") continue;
        const tag = String(value).trim().replace(/^#/, "");
        if (tag) tags.add(tag);
    }

    // Properties are parsed above; inline tags belong only to the Markdown body.
    let yaml = "";
    let body = content;
    try {
        const split = splitFrontmatter(content);
        yaml = split.yaml ?? "";
        body = split.body;
    } catch {
        /* Leave malformed notes readable. */
    }
    const maskedBody = maskCode(body);
    for (const match of maskedBody.matchAll(/(^|\s)#([\p{L}\p{N}_/-][\p{L}\p{M}\p{N}_/-]*)/gu)) {
        if (/^\p{Nd}+$/u.test(match[2])) continue;
        tags.add(match[2]);
    }
    // Links count in properties (e.g. `related: "[[Note]]"`) and in the body,
    // but not inside code. Scanned separately so a match never spans the two.
    for (const text of [yaml, maskedBody]) {
        for (const match of text.matchAll(/\[\[([^\]|]+)(?:\|[^\]]+)?\]\]/g)) {
            links.push(match[1]);
        }
        for (const match of text.matchAll(/\[([^\]]+)\]\(([^)]+\.md)\)/g)) {
            links.push(match[2]);
        }
    }
    return { frontmatter, tags: [...tags], links: [...new Set(links)] };
}

/**
 * Replace fenced code blocks (``` or ~~~, opener at line start with up to three
 * spaces of indent, closer of the same character and at least the same length;
 * an unclosed fence runs to the end) and inline code spans (a backtick run of
 * length n closes at the next run of exactly n within the same paragraph — a
 * span never crosses a blank line; an unmatched run is literal)
 * with dots, so offsets are preserved and nothing inside can start or extend a
 * tag. Indented code blocks, %% comments and math are not masked.
 */
export function maskCode(content: string): string {
    const lines = content.split("\n");
    const out: string[] = [];
    let fence: { char: string; len: number } | null = null;
    const inline: string[] = [];
    const flushInline = () => {
        if (inline.length === 0) return;
        out.push(...maskInlineSpans(inline.join("\n")).split("\n"));
        inline.length = 0;
    };
    for (const line of lines) {
        const open = line.match(/^ {0,3}(`{3,}|~{3,})/);
        if (fence) {
            const close =
                open &&
                open[1][0] === fence.char &&
                open[1].length >= fence.len &&
                line.trim() === open[1];
            out.push(".".repeat(line.length));
            if (close) fence = null;
            continue;
        }
        if (open && (open[1][0] === "~" || !line.slice(open[0].length).includes("`"))) {
            flushInline();
            fence = { char: open[1][0], len: open[1].length };
            out.push(".".repeat(line.length));
            continue;
        }
        if (line.trim() === "") {
            flushInline();
            out.push(line);
            continue;
        }
        inline.push(line);
    }
    flushInline();
    return out.join("\n");
}

function maskInlineSpans(text: string): string {
    // Collect backtick runs in one pass, then link each run to the next run of
    // the same length (scanning right to left) so matching stays linear.
    const runs: { at: number; len: number }[] = [];
    for (let i = 0; i < text.length;) {
        if (text[i] !== "`") {
            i++;
            continue;
        }
        let n = 0;
        while (text[i + n] === "`") n++;
        runs.push({ at: i, len: n });
        i += n;
    }
    const next: number[] = [];
    const seen = new Map<number, number>();
    for (let r = runs.length - 1; r >= 0; r--) {
        next[r] = seen.get(runs[r].len) ?? -1;
        seen.set(runs[r].len, r);
    }
    let result = "";
    let from = 0;
    for (let r = 0; r < runs.length;) {
        const c = next[r];
        if (c === -1) {
            r++;
            continue;
        }
        const start = runs[r].at;
        const end = runs[c].at + runs[c].len;
        result += text.slice(from, start) + text.slice(start, end).replace(/[^\n]/g, ".");
        from = end;
        r = c + 1;
    }
    return result + text.slice(from);
}
