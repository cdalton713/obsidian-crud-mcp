import { z } from "zod";

export const NoteTargetsSchema = z.object({
    heading: z
        .array(z.string())
        .min(1)
        .max(6)
        .optional()
        .describe(
            "Exact full heading path from get_note_outline. Selects section content, including child headings, but excludes the selected heading line.",
        ),
    block: z
        .string()
        .regex(/^[A-Za-z0-9-]+$/)
        .optional()
        .describe(
            "Block ID without ^, from get_note_outline. Selects content without the block marker. Cannot be combined with heading.",
        ),
});

export const ReadNoteParametersSchema = z.object({
    path: z.string().describe("Vault-relative path to the note, e.g. 'daily/2026-03-23.md'"),
    ...NoteTargetsSchema.shape,
});

export const WriteNoteParametersSchema = z.object({
    path: z.string().describe("Vault-relative path to the note, e.g. 'daily/2026-03-23.md'"),
    content: z.string().describe("Full markdown content for the note"),
});

export const ListNotesParametersSchema = z.object({
    folder: z
        .string()
        .optional()
        .describe("Folder to filter by, e.g. 'daily' or 'projects'. Omit for all notes."),
    name: z
        .string()
        .optional()
        .describe(
            "Filter by name (case-insensitive substring match on path), e.g. 'meeting' or 'project-x'.",
        ),
    tag: z
        .string()
        .optional()
        .describe(
            "Filter by tag, e.g. 'project' or 'daily'. Use list_tags to discover available tags.",
        ),
    sort_by: z
        .enum(["name", "modified"])
        .optional()
        .describe("Sort order: 'name' (default) or 'modified' (most recent first)."),
    modified_after: z
        .string()
        .optional()
        .describe(
            "Only include notes modified after this ISO date, e.g. '2026-03-25' or '2026-03-25T10:00'.",
        ),
    limit: z.coerce
        .number()
        .int()
        .min(1)
        .max(10_000)
        .optional()
        .describe("Max number of notes to return. Default 100."),
});

/** For tools that take no arguments. */
export const EmptyParametersSchema = z.object({});

export const EditNoteParametersSchema = z.object({
    path: z.string().describe("Vault-relative path to the note, e.g. 'daily/2026-03-25.md'"),
    ...NoteTargetsSchema.shape,
    content: z.string().describe("Text to append, prepend, or use as replacement for old_text"),
    operation: z
        .enum(["append", "prepend", "replace"])
        .optional()
        .describe(
            "'append' (default): add to end. 'prepend': add after frontmatter. 'replace': swap old_text with content.",
        ),
    old_text: z
        .string()
        .optional()
        .describe(
            "Required for replace operation. Exact text to find and replace. Must match exactly once.",
        ),
});

export const DeleteNoteParametersSchema = z.object({
    path: z.string().describe("Vault-relative path to the note to delete"),
});

export const MoveNoteParametersSchema = z.object({
    from: z.string().describe("Current path, e.g. 'daily/old-name.md'"),
    to: z.string().describe("New path, e.g. 'projects/new-name.md'"),
});

export const GetNoteMetadataParametersSchema = z.object({
    path: z.string().describe("Vault-relative path to the note, e.g. 'projects/my-project.md'"),
});
