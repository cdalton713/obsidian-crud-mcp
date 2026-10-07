import { z } from "zod";

export const IndexStateSchema = z.enum(["building", "ready", "failed"]);
export type IndexState = z.infer<typeof IndexStateSchema>;

/** On-disk shape of the persisted metadata index (after decryption). */
export const PersistedIndexSchema = z.object({
    version: z.number(),
    mtimes: z.record(z.string(), z.number()).default({}),
    tags: z.record(z.string(), z.array(z.string())).default({}),
    links: z.record(z.string(), z.array(z.string())).default({}),
});
export type PersistedIndex = z.infer<typeof PersistedIndexSchema>;
