import type { NoteHeading, NoteBlock } from "../types/note-structure.js";
import { fromMarkdown } from "mdast-util-from-markdown";
import type { Nodes } from "mdast";
import { splitFrontmatter } from "./note-properties.js";

/** Mask frontmatter without moving Markdown source positions. */
function markdownTree(content: string) {
    const { bodyOffset } = splitFrontmatter(content);
    const masked =
        content.slice(0, bodyOffset).replace(/[^\r\n]/g, " ") + content.slice(bodyOffset);
    return fromMarkdown(masked);
}

export function noteStructure(content: string): { headings: NoteHeading[]; blocks: NoteBlock[] } {
    const tree = markdownTree(content);
    const headings: NoteHeading[] = [];
    const blocks: NoteBlock[] = [];
    const parents: NoteHeading[] = [];
    const lastLine = content.length
        ? content.split(/\r\n|\n|\r/).length - (/[\r\n]$/.test(content) ? 1 : 0)
        : 1;
    for (const node of tree.children) {
        if (!node.position) continue;
        const start = node.position.start.offset!;
        const end = node.position.end.offset!;
        if (node.type === "heading") {
            const source = content.slice(start, end);
            const title = /^ {0,3}#{1,6}(?:[ \t]+|$)/.test(source)
                ? source
                      .replace(/^ {0,3}#{1,6}(?:[ \t]+|$)/, "")
                      .replace(/[ \t]+#+[ \t]*$/, "")
                      .trim()
                : source.replace(/\r?\n {0,3}(?:=+|-+)[ \t]*$/, "").trim();
            while (parents.length && parents[parents.length - 1].level >= node.depth) {
                const parent = parents.pop()!;
                parent.end = start;
                parent.end_line = node.position.start.line - 1;
            }
            const eol = content.slice(end).match(/^(?:\r\n|\n|\r)/)?.[0].length ?? 0;
            const heading: NoteHeading = {
                heading: [...parents.map((p) => p.heading[p.heading.length - 1]), title],
                level: node.depth,
                start,
                end: content.length,
                content_start: end + eol,
                start_line: node.position.start.line,
                end_line: lastLine,
            };
            headings.push(heading);
            parents.push(heading);
        }
    }
    const collectBlocks = (siblings: Nodes[]) => {
        for (const [i, node] of siblings.entries()) {
            if ("children" in node) collectBlocks(node.children);
            if (!node.position) continue;
            const start = node.position.start.offset!;
            const end = node.position.end.offset!;
            if (node.type !== "paragraph") continue;
            const last = node.children[node.children.length - 1];
            if (last?.type !== "text" || !last.position) continue;
            const raw = content.slice(last.position.start.offset!, end);
            const marker = /(?:^|[ \t])\^([A-Za-z0-9-]+)[ \t]*$/.exec(raw);
            if (!marker) continue;
            const markerStart = last.position.start.offset! + marker.index;
            let target: Nodes = node;
            if (content.slice(start, markerStart).trim() === "") {
                const previous = siblings[i - 1];
                if (
                    !previous ||
                    previous.type === "heading" ||
                    previous.type === "thematicBreak" ||
                    previous.type === "definition" ||
                    previous.type === "html"
                )
                    continue;
                target = previous;
            }
            blocks.push({
                id: marker[1],
                start: target.position!.start.offset!,
                end: target === node ? markerStart : target.position!.end.offset!,
                start_line: target.position!.start.line,
                end_line: target.position!.end.line,
            });
        }
    };
    collectBlocks(tree.children);
    blocks.sort((a, b) => a.start - b.start);
    return { headings, blocks };
}

export function selectNoteRange(
    content: string,
    heading?: string[],
    block?: string,
): { start: number; end: number } {
    if (heading && block) throw new Error("Use either heading or block, not both.");
    if (!heading && !block) return { start: 0, end: content.length };
    const outline = noteStructure(content);
    const matches = heading
        ? outline.headings
              .filter((item) => JSON.stringify(item.heading) === JSON.stringify(heading))
              .map((item) => ({ start: item.content_start, end: item.end }))
        : outline.blocks
              .filter((item) => item.id === block)
              .map((item) => ({ start: item.start, end: item.end }));
    if (matches.length === 0)
        throw new Error(
            "Target not found. Use get_note_outline to find heading paths or block IDs.",
        );
    if (matches.length !== 1)
        throw new Error("Target is ambiguous. Use a unique heading path or block ID.");
    return matches[0];
}

/** True when a line placed directly above content would join a setext heading, as title text or above an underline. */
export function startsWithSetextBoundary(content: string): boolean {
    if (/^ {0,3}(?:=+|-+)[ \t]*(?:\r\n|\n|\r|$)/.test(content)) return true;
    const first = fromMarkdown(content).children[0];
    return (
        first?.type === "heading" && first.position?.start.offset === 0 && !/^ {0,3}#/.test(content)
    );
}

export function noteTasks(
    content: string,
): { line: number; text: string; completed: boolean; truncated: boolean }[] {
    const tasks: { line: number; text: string; completed: boolean; truncated: boolean }[] = [];
    const visit = (node: Nodes) => {
        if (node.type === "listItem") {
            const first = node.children[0];
            if (first?.type === "paragraph" && first.position) {
                const source = content.slice(
                    first.position.start.offset!,
                    first.position.end.offset!,
                );
                const checkbox = /^\[([ \txX])\](?:\s+|$)/.exec(source);
                if (checkbox) {
                    const text = source.slice(checkbox[0].length);
                    tasks.push({
                        line: first.position.start.line,
                        text: text.slice(0, 500),
                        completed: /[xX]/.test(checkbox[1]),
                        truncated: text.length > 500,
                    });
                }
            }
        }
        if ("children" in node) for (const child of node.children) visit(child);
    };
    visit(markdownTree(content));
    return tasks;
}
