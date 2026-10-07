import { z } from "zod";

/** What the mirror last downloaded or uploaded for one note, keyed by vault path. */
export const ManifestEntrySchema = z.object({
    etag: z.string(),
    mtime: z.number(),
});
export type ManifestEntry = z.infer<typeof ManifestEntrySchema>;

export const ManifestSchema = z.object({
    version: z.literal(1),
    files: z.record(z.string(), ManifestEntrySchema),
});
export type Manifest = z.infer<typeof ManifestSchema>;

/** One note object in the bucket, with the key already stripped of S3_PREFIX. */
export interface RemoteNote {
    path: string;
    etag: string;
    /** LastModified in ms; the precise mtime comes from object metadata on download. */
    lastModified: number;
}

export interface MirrorPlan {
    download: RemoteNote[];
    remove: string[];
}

export interface S3MirrorOptions {
    endpoint?: string;
    region: string;
    bucket: string;
    prefix: string;
    accessKeyId?: string;
    secretAccessKey?: string;
}
