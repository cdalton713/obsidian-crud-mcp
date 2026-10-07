import { z } from "zod";

export const NoteMetadataSchema = z.object({
    frontmatter: z.record(z.string(), z.unknown()),
    tags: z.array(z.string()),
    links: z.array(z.string()),
});
export type NoteMetadata = z.infer<typeof NoteMetadataSchema>;
