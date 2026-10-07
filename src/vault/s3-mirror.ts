/**
 * Keeps a local folder in step with an S3 bucket that Remotely Save syncs to.
 *
 * The bucket is the source of truth. Each poll lists it, downloads notes whose
 * ETag changed, and deletes local notes that were removed remotely. Only notes
 * the mirror itself downloaded or uploaded (the manifest) are ever deleted, so
 * an unrelated local file is never touched.
 *
 * Network calls run unlocked, so a write never waits for a poll. Only the
 * local apply of a note (its file and manifest entry) is serialized, and a
 * poll leaves alone any note the server wrote after that poll's listing began:
 * a listing that predates a write must never undo it.
 *
 * Timestamps follow Remotely Save's S3 backend: user metadata `MTime`/`CTime`
 * in seconds (a float string), read back case-insensitively, with values of
 * 1e12 or more treated as milliseconds (written by plugin versions before
 * March 2024). Objects without the metadata fall back to LastModified.
 */

import { mkdir, readFile, rename, unlink, utimes, writeFile } from "node:fs/promises";
import { dirname, resolve, sep } from "node:path";
import {
    CopyObjectCommand,
    DeleteObjectCommand,
    GetObjectCommand,
    ListObjectsV2Command,
    PutObjectCommand,
    S3Client,
} from "@aws-sdk/client-s3";
import { logger } from "../logging/logger.js";
import { isValidNotePath } from "../notes/note-path.js";
import type { VaultChangeListener } from "../types/vault-backend.js";
import {
    ManifestSchema,
    type Manifest,
    type MirrorPlan,
    type RemoteNote,
    type S3MirrorOptions,
} from "../types/s3-mirror.js";

const DOWNLOAD_CONCURRENCY = 8;
/** Remotely Save's debug-output folder; it can contain .md files that are not notes. */
const SKIPPED_PREFIXES = ["_debug_remotely_save/"];
/**
 * Fail fast on a socket that stopped answering (a keep-alive connection that
 * died while the machine was suspended, for instance) so the SDK retries on a
 * fresh one instead of a poll or write hanging for minutes.
 */
const HTTP_TIMEOUTS = { connectionTimeout: 5_000, socketTimeout: 15_000 };

/** Thrown when a conditional write loses to a newer version in the bucket. */
export class RemoteChangedError extends Error {
    constructor(path: string) {
        super(`'${path}' changed in the bucket since it was last synced. Read it again and retry.`);
    }
}

/** Normalize S3_PREFIX to "" or "folder/" so keys are always `${prefix}${path}`. */
export function normalizePrefix(prefix: string): string {
    const trimmed = prefix.replace(/^\/+|\/+$/g, "");
    return trimmed ? `${trimmed}/` : "";
}

/** Parse Remotely Save's `MTime` metadata into ms, or undefined when absent or zero. */
export function parseRemoteMtime(metadata: Record<string, string> | undefined): number | undefined {
    if (!metadata) return undefined;
    const raw = Object.entries(metadata).find(([key]) => key.toLowerCase() === "mtime")?.[1];
    const value = Math.floor(Number.parseFloat(raw ?? ""));
    if (!Number.isFinite(value) || value <= 0) return undefined;
    return value >= 1_000_000_000_000 ? value : value * 1000;
}

/** The metadata Remotely Save writes, so the plugin reads our uploads' times correctly. */
export function remoteMetadata(mtime: number, ctime = mtime): Record<string, string> {
    return { MTime: `${mtime / 1000}`, CTime: `${ctime / 1000}` };
}

/** Whether a vault path is a note the mirror should carry. */
export function isMirroredPath(path: string): boolean {
    return isValidNotePath(path) && !SKIPPED_PREFIXES.some((prefix) => path.startsWith(prefix));
}

/**
 * Decide what one poll must do. A note is downloaded when it is new, its
 * ETag changed, or the local copy disappeared; a note is removed locally
 * only when the manifest has it and the listing no longer does.
 */
export function planMirror(
    remote: RemoteNote[],
    manifest: Manifest,
    localExists: (path: string) => boolean,
): MirrorPlan {
    const seen = new Set<string>();
    const download: RemoteNote[] = [];
    for (const note of remote) {
        seen.add(note.path);
        const known = manifest.files[note.path];
        if (!known || known.etag !== note.etag || !localExists(note.path)) download.push(note);
    }
    const remove = Object.keys(manifest.files).filter((path) => !seen.has(path));
    return { download, remove };
}

export class S3Mirror {
    readonly client: S3Client;
    private readonly bucket: string;
    private readonly prefix: string;
    private manifest: Manifest = { version: 1, files: {} };
    private readonly listeners = new Set<VaultChangeListener>();
    /** Serializes local state changes: note files and the manifest. */
    private lock: Promise<unknown> = Promise.resolve();
    private polling: Promise<number> | null = null;
    /** Per path, the sequence number of the server's latest write; see `touchedSince`. */
    private writeSeq = 0;
    private readonly lastWrite = new Map<string, number>();

    constructor(
        private readonly root: string,
        private readonly manifestPath: string,
        options: S3MirrorOptions,
        client?: S3Client,
    ) {
        this.bucket = options.bucket;
        this.prefix = normalizePrefix(options.prefix);
        this.client =
            client ??
            new S3Client({
                endpoint: options.endpoint,
                region: options.region,
                forcePathStyle: true,
                credentials:
                    options.accessKeyId && options.secretAccessKey
                        ? {
                              accessKeyId: options.accessKeyId,
                              secretAccessKey: options.secretAccessKey,
                          }
                        : undefined,
                requestHandler: HTTP_TIMEOUTS,
            });
    }

    etagOf(path: string): string | undefined {
        return this.manifest.files[path]?.etag;
    }

    /**
     * Hear about notes a poll downloaded or removed. The mirror's own uploads
     * are not reported: the caller that wrote them already knows.
     */
    subscribe(listener: VaultChangeListener): () => void {
        this.listeners.add(listener);
        return () => void this.listeners.delete(listener);
    }

    /** Resolves once no poll or local apply is in flight. */
    async idle(): Promise<void> {
        await this.polling?.catch(() => {});
        await this.lock;
    }

    /** Run `task` after every earlier local apply has finished. */
    private exclusive<T>(task: () => Promise<T>): Promise<T> {
        const run = this.lock.then(task, task);
        this.lock = run.catch(() => {});
        return run;
    }

    /** Record that the server is writing `path` now. */
    private touch(path: string): void {
        this.lastWrite.set(path, ++this.writeSeq);
    }

    /** Whether the server wrote `path` after sequence number `seq` was taken. */
    private touchedSince(path: string, seq: number): boolean {
        return (this.lastWrite.get(path) ?? 0) > seq;
    }

    async loadManifest(): Promise<void> {
        try {
            const raw: unknown = JSON.parse(await readFile(this.manifestPath, "utf8"));
            this.manifest = ManifestSchema.parse(raw);
        } catch (error) {
            if ((error as NodeJS.ErrnoException).code !== "ENOENT")
                logger.warn("S3 mirror manifest is unreadable; re-downloading every note.");
            this.manifest = { version: 1, files: {} };
        }
    }

    /** Callers hold the lock. */
    private async saveManifest(): Promise<void> {
        await mkdir(dirname(this.manifestPath), { recursive: true });
        const tmp = `${this.manifestPath}.tmp`;
        await writeFile(tmp, JSON.stringify(this.manifest));
        await rename(tmp, this.manifestPath);
    }

    /** Absolute local path for a note; refuses anything that resolves outside the mirror root. */
    localPath(path: string): string {
        const full = resolve(this.root, path);
        if (!full.startsWith(this.root + sep)) throw new Error("Path traversal blocked");
        return full;
    }

    private key(path: string): string {
        return `${this.prefix}${path}`;
    }

    async list(): Promise<RemoteNote[]> {
        const notes: RemoteNote[] = [];
        let token: string | undefined;
        do {
            const page = await this.client.send(
                new ListObjectsV2Command({
                    Bucket: this.bucket,
                    Prefix: this.prefix || undefined,
                    ContinuationToken: token,
                }),
            );
            for (const object of page.Contents ?? []) {
                if (!object.Key || !object.ETag) continue;
                const path = object.Key.slice(this.prefix.length);
                if (!isMirroredPath(path)) continue;
                notes.push({
                    path,
                    etag: object.ETag,
                    lastModified: object.LastModified?.getTime() ?? Date.now(),
                });
            }
            token = page.IsTruncated ? page.NextContinuationToken : undefined;
        } while (token);
        return notes;
    }

    /**
     * One full pass: list, download changes, delete removed notes. Returns the
     * number of notes changed locally. A call during a poll joins that poll.
     */
    sync(localExists: (path: string) => Promise<boolean>): Promise<number> {
        if (!this.polling) {
            this.polling = this.runSync(localExists).finally(() => {
                this.polling = null;
            });
        }
        return this.polling;
    }

    private async runSync(localExists: (path: string) => Promise<boolean>): Promise<number> {
        // Anything the server writes from here on postdates the listing below.
        const since = this.writeSeq;
        const remote = await this.list();
        const present = new Set<string>();
        await Promise.all(
            Object.keys(this.manifest.files).map(async (path) => {
                if (await localExists(path)) present.add(path);
            }),
        );
        const plan = planMirror(remote, this.manifest, (path) => present.has(path));

        let changed = 0;
        const queue = plan.download.filter((note) => !this.touchedSince(note.path, since));
        const worker = async () => {
            for (let note = queue.shift(); note; note = queue.shift()) {
                try {
                    if (await this.download(note, since)) changed++;
                } catch (error) {
                    logger.warn(`S3 mirror: download failed: ${(error as Error).message}`);
                }
            }
        };
        await Promise.all(Array.from({ length: DOWNLOAD_CONCURRENCY }, worker));

        for (const path of plan.remove) {
            if (await this.forget(path, since)) changed++;
        }
        if (changed > 0) await this.exclusive(() => this.saveManifest());
        // Writes older than this listing can no longer collide with a poll.
        for (const [path, seq] of this.lastWrite) if (seq <= since) this.lastWrite.delete(path);
        return changed;
    }

    /** Fetch one note and apply it locally, unless the server wrote it after `since`. */
    private async download(note: RemoteNote, since: number): Promise<boolean> {
        const object = await this.client.send(
            new GetObjectCommand({ Bucket: this.bucket, Key: this.key(note.path) }),
        );
        if (!object.Body) throw new Error("empty response body");
        const bytes = await object.Body.transformToByteArray();
        const mtime = parseRemoteMtime(object.Metadata) ?? note.lastModified;
        return this.exclusive(async () => {
            if (this.touchedSince(note.path, since)) return false;
            await this.writeLocal(note.path, bytes, mtime);
            this.manifest.files[note.path] = { etag: object.ETag ?? note.etag, mtime };
            if (this.listeners.size > 0) {
                // Same decoding as readFile(path, "utf8"): a BOM, if any, stays in the text.
                const content = Buffer.from(
                    bytes.buffer,
                    bytes.byteOffset,
                    bytes.byteLength,
                ).toString();
                for (const listener of this.listeners) listener.updated(note.path, content, mtime);
            }
            return true;
        });
    }

    /** Drop a note that left the bucket, unless the server wrote it after `since`. */
    private forget(path: string, since: number): Promise<boolean> {
        return this.exclusive(async () => {
            if (this.touchedSince(path, since) || !this.manifest.files[path]) return false;
            await unlink(this.localPath(path)).catch((error: NodeJS.ErrnoException) => {
                if (error.code !== "ENOENT") throw error;
            });
            delete this.manifest.files[path];
            for (const listener of this.listeners) listener.removed(path);
            return true;
        });
    }

    private async writeLocal(path: string, data: Uint8Array | string, mtime: number) {
        const full = this.localPath(path);
        await mkdir(dirname(full), { recursive: true });
        const tmp = `${full}.s3-mirror.tmp`;
        await writeFile(tmp, data);
        await utimes(tmp, mtime / 1000, mtime / 1000);
        await rename(tmp, full);
    }

    /**
     * Upload a note, then mirror it locally. When `ifMatch` is given the write
     * only succeeds if the bucket still holds that version; `null` requires
     * that the key does not exist yet.
     */
    async put(path: string, content: string, ifMatch?: string | null): Promise<void> {
        this.touch(path);
        const mtime = Date.now();
        let etag: string | undefined;
        try {
            const result = await this.client.send(
                new PutObjectCommand({
                    Bucket: this.bucket,
                    Key: this.key(path),
                    Body: content,
                    ContentType: "text/markdown; charset=utf-8",
                    Metadata: remoteMetadata(mtime),
                    IfMatch: ifMatch ?? undefined,
                    IfNoneMatch: ifMatch === null ? "*" : undefined,
                }),
            );
            etag = result.ETag;
        } catch (error) {
            if (
                (error as { $metadata?: { httpStatusCode?: number } }).$metadata?.httpStatusCode ===
                412
            )
                throw new RemoteChangedError(path);
            throw error;
        }
        this.touch(path);
        await this.exclusive(async () => {
            await this.writeLocal(path, content, mtime);
            this.manifest.files[path] = { etag: etag ?? "", mtime };
            await this.saveManifest();
        });
    }

    async copy(from: string, to: string): Promise<void> {
        this.touch(to);
        const source = this.key(from).split("/").map(encodeURIComponent).join("/");
        const result = await this.client.send(
            new CopyObjectCommand({
                Bucket: this.bucket,
                Key: this.key(to),
                CopySource: `${this.bucket}/${source}`,
                MetadataDirective: "COPY",
            }),
        );
        this.touch(to);
        const mtime = this.manifest.files[from]?.mtime ?? Date.now();
        this.manifest.files[to] = { etag: result.CopyObjectResult?.ETag ?? "", mtime };
    }

    /** Delete a note in the bucket and forget it; the caller removes the local file. */
    async remove(path: string): Promise<void> {
        this.touch(path);
        await this.client.send(
            new DeleteObjectCommand({ Bucket: this.bucket, Key: this.key(path) }),
        );
        this.touch(path);
        delete this.manifest.files[path];
        await this.exclusive(() => this.saveManifest());
    }
}
