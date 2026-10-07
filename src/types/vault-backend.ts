import { z } from "zod";
import { NoteMetadataSchema } from "./parse.js";

export const NoteInfoSchema = NoteMetadataSchema.extend({
    path: z.string(),
    size: z.number(),
    ctime: z.number(),
    mtime: z.number(),
});
export type NoteInfo = z.infer<typeof NoteInfoSchema>;

export const NoteListingSchema = z.object({
    path: z.string(),
    mtime: z.number(),
});
export type NoteListing = z.infer<typeof NoteListingSchema>;

/**
 * Receives notes that changed outside this server (a device edit that the S3
 * mirror downloaded, or a note removed from the bucket). `mtime` is the local
 * file's modification time in ms, so an index fed from here agrees with a
 * later mtime-based rescan.
 */
export interface VaultChangeListener {
    updated(path: string, content: string, mtime: number): void;
    removed(path: string): void;
}
