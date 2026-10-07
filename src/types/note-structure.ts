import { z } from "zod";

export const RangeSchema = z.object({
    start: z.number(),
    end: z.number(),
    start_line: z.number(),
    end_line: z.number(),
});
export type Range = z.infer<typeof RangeSchema>;

export const NoteHeadingSchema = z.intersection(
    RangeSchema,
    z.object({
        heading: z.array(z.string()),
        level: z.number(),
        content_start: z.number(),
    }),
);
export type NoteHeading = z.infer<typeof NoteHeadingSchema>;

export const NoteBlockSchema = z.intersection(
    RangeSchema,
    z.object({
        id: z.string(),
    }),
);
export type NoteBlock = z.infer<typeof NoteBlockSchema>;
