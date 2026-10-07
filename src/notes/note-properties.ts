import type { PropertyValue } from "../types/note-properties.js";
import matter from "@11ty/gray-matter";
import {
    type Document,
    type Scalar,
    type YAMLMap,
    type YAMLSeq,
    isMap,
    isNode,
    parseDocument,
    stringify,
    visit,
} from "yaml";
import { isDeepStrictEqual } from "node:util";

const OPENING = /^﻿?---[ \t]*(?:\r?\n|$)/;

const MATTER_OPTIONS = {
    engines: {
        yaml: {
            parse(source: string): Record<string, unknown> {
                const doc = parseDocument(source.replace(/\r\n?/g, "\n"), { stringKeys: true });
                if (doc.errors.length || doc.warnings.length)
                    throw new Error("Invalid or unsupported YAML frontmatter; no changes made.");
                if (doc.contents !== null && !isMap(doc.contents))
                    throw new Error("Frontmatter must be a YAML mapping.");
                return (doc.toJS({ maxAliasCount: 100 }) ?? {}) as Record<string, unknown>;
            },
            stringify: (data: object) => stringify(data, { lineWidth: 0 }),
        },
    },
};

/**
 * Keep the body and delimiters separate so YAML edits cannot change Markdown.
 * An opening --- without a closing one is a Markdown horizontal rule, not frontmatter.
 */
export function splitFrontmatter(content: string) {
    const opening = content.match(OPENING);
    const eol = content.includes("\r\n") ? "\r\n" : "\n";
    const rest = opening ? content.slice(opening[0].length) : "";
    const closing = opening ? /^---[ \t]*(?:\r?\n|$)/m.exec(rest) : null;
    if (!opening || !closing)
        return { yaml: null, body: content, bodyOffset: 0, opening: "", closing: "", eol };
    const bodyOffset = opening[0].length + closing.index + closing[0].length;

    return {
        yaml: rest.slice(0, closing.index),
        body: content.slice(bodyOffset),
        bodyOffset,
        opening: opening[0],
        closing: closing[0],
        eol,
    };
}

export function readProperties(content: string): Record<string, unknown> {
    try {
        const { yaml } = splitFrontmatter(content);
        if (yaml === null) return {};
        return matter(`---\n${yaml}---\n`, MATTER_OPTIONS).data as Record<string, unknown>;
    } catch {
        return {};
    }
}

/** Edit frontmatter in place as a YAML Document so untouched keys keep comments, order and style. */
export function updateProperties(
    content: string,
    set: Record<string, PropertyValue>,
    remove: string[],
): string {
    if (remove.some((key) => Object.hasOwn(set, key)))
        throw new Error("A property cannot be set and removed in the same call.");
    const parts = splitFrontmatter(content);
    if (parts.yaml === null && OPENING.test(content))
        throw new Error("Frontmatter has no closing --- delimiter.");
    if (parts.yaml === null && Object.keys(set).length === 0) return content;
    const before = matter(`---\n${parts.yaml ?? ""}---\n`, MATTER_OPTIONS).data as Record<
        string,
        unknown
    >;
    if (
        Object.entries(set).every(
            ([key, value]) => Object.hasOwn(before, key) && isDeepStrictEqual(before[key], value),
        ) &&
        remove.every((key) => !Object.hasOwn(before, key))
    )
        return content;

    // The matter() call above has already rejected invalid YAML and non-mapping roots.
    const doc: Document = parseDocument((parts.yaml ?? "").replace(/\r\n?/g, "\n"), {
        stringKeys: true,
    });
    if (!isMap(doc.contents)) doc.contents = doc.createNode({});
    const map = doc.contents as YAMLMap;
    const changed = [...remove, ...Object.keys(set)];
    detachAliases(doc, changed);
    for (const key of remove) doc.delete(key);
    // Replace (not mutate) changed values so stale tags, anchors and comments do not leak.
    for (const [key, value] of Object.entries(set)) doc.set(key, doc.createNode(value));

    // With no keys left, keep surviving comments but drop the frontmatter if there are none.
    const yaml =
        map.items.length === 0
            ? remainingComments(doc, map)
            : doc.toString({ lineWidth: 0, flowCollectionPadding: false });
    const toEol = (text: string) => text.replace(/\n/g, parts.eol);
    if (parts.yaml === null) {
        const bom = content.startsWith("\uFEFF") ? "\uFEFF" : "";
        return bom + toEol(`---\n${yaml}---\n`) + content.slice(bom.length);
    }
    if (yaml === "") return parts.body;
    return parts.opening + toEol(yaml) + parts.closing + parts.body;
}

/** Replace aliases into values that will be removed or replaced with copies of what they resolved to. */
function detachAliases(doc: Document, keys: string[]) {
    const anchored = new Set<unknown>();
    for (const key of keys) {
        const node = doc.get(key, true);
        if (!isNode(node)) continue;
        visit(node, (_, n) => {
            if (isNode(n) && n.anchor) anchored.add(n);
        });
    }
    if (anchored.size === 0) return;
    visit(doc, {
        Alias(_, alias) {
            const target = alias.resolve(doc);
            if (!target || !anchored.has(target)) return;
            const copy = target.clone() as Scalar | YAMLMap | YAMLSeq;
            copy.anchor = undefined;
            return copy;
        },
    });
}

function remainingComments(doc: Document, map: YAMLMap): string {
    const comments = [doc.commentBefore, map.commentBefore, map.comment, doc.comment];
    return comments
        .filter((comment): comment is string => Boolean(comment))
        .flatMap((comment) => comment.split("\n"))
        .map((line) => (line ? `#${line}\n` : "\n"))
        .join("");
}
