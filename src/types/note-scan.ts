import { z } from "zod";

export const ScanParametersSchema = z.object({
    folder: z
        .string()
        .max(1000)
        .optional()
        .describe("Folder and its descendants; folder boundaries are respected."),
    tag: z.string().min(1).max(200).optional().describe("Exact tag, with or without a leading #."),
    limit: z.number().int().min(1).max(50).default(20).describe("Results per page, 1 to 50."),
    max_notes: z
        .number()
        .int()
        .min(1)
        .max(100_000)
        .default(10_000)
        .describe(
            "Maximum notes read per call; the default covers a whole vault of typical size. Follow next_cursor even when this page has no results.",
        ),
    cursor: z
        .string()
        .max(3000)
        .optional()
        .describe(
            "Opaque next_cursor from the previous page using the same filters. Results are live, not a snapshot.",
        ),
});
export type ScanParameters = z.infer<typeof ScanParametersSchema>;

export const CursorSchema = z.object({
    key: z.string(),
    path: z.string(),
    line: z.number().int().min(1).max(1_000_001),
});
export type Cursor = z.infer<typeof CursorSchema>;

export const LineMatchSchema = z.object({
    line: z.number(),
    text: z.string(),
    completed: z.boolean().optional(),
    truncated: z.boolean().optional(),
});
export type LineMatch = z.infer<typeof LineMatchSchema>;
