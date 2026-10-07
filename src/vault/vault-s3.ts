/**
 * S3 mode: a LocalVault over a folder that S3Mirror keeps in step with a
 * bucket (Remotely Save's S3 backend). Reads, listing and search all run
 * against the local copy; writes go to the bucket first and land locally only
 * after the upload succeeds.
 *
 * Writes never wait for a poll: the mirror runs its network calls unlocked
 * and refuses to let a listing taken before a write undo that write.
 *
 * Each poll reports what it downloaded or removed to subscribers (the search
 * index), content included, so S3 mode needs no filesystem watcher and never
 * reads a downloaded note a second time.
 */

import { lstat } from "node:fs/promises";
import { logger } from "../logging/logger.js";
import { validateNotePath } from "../notes/note-path.js";
import type { NoteInfo, NoteListing, VaultChangeListener } from "../types/vault-backend.js";
import type { S3MirrorOptions } from "../types/s3-mirror.js";
import { S3Mirror } from "./s3-mirror.js";
import type { VaultBackend } from "./vault-backend.js";
import { LocalVault } from "./vault-local.js";
import { isPathWritable } from "./write-scope.js";

export interface S3VaultOptions extends S3MirrorOptions {
    vaultPath: string;
    manifestPath: string;
    pollSeconds: number;
    writeFolders: string[] | null;
}

export class S3Vault implements VaultBackend {
    private readonly local: LocalVault;
    readonly mirror: S3Mirror;
    private timer: NodeJS.Timeout | undefined;

    constructor(
        private readonly options: S3VaultOptions,
        mirror?: S3Mirror,
    ) {
        this.local = new LocalVault(options.vaultPath, options.writeFolders);
        this.mirror = mirror ?? new S3Mirror(this.local.rootPath, options.manifestPath, options);
    }

    private exists = async (path: string): Promise<boolean> =>
        lstat(this.mirror.localPath(path)).then(
            () => true,
            () => false,
        );

    /** One poll; exposed for tests and for callers that want a refresh now. */
    poll(): Promise<number> {
        return this.mirror.sync(this.exists);
    }

    async init(): Promise<void> {
        await this.mirror.loadManifest();
        const start = performance.now();
        const changed = await this.poll();
        logger.info(
            `S3 mirror ready: ${changed} notes updated in ${((performance.now() - start) / 1000).toFixed(1)}s.`,
        );
        this.timer = setInterval(() => {
            this.poll().catch((error: unknown) => {
                logger.warn(`S3 mirror poll failed: ${(error as Error).message}`);
            });
        }, this.options.pollSeconds * 1000);
        this.timer.unref();
    }

    async close(): Promise<void> {
        clearInterval(this.timer);
        await this.mirror.idle();
    }

    subscribe(listener: VaultChangeListener): () => void {
        return this.mirror.subscribe(listener);
    }

    readNote(path: string): Promise<string | null> {
        return this.local.readNote(path);
    }

    getMetadata(path: string): Promise<NoteInfo | null> {
        return this.local.getMetadata(path);
    }

    listNotes(folder?: string): Promise<string[]> {
        return this.local.listNotes(folder);
    }

    listNotesWithMtime(folder?: string): Promise<NoteListing[]> {
        return this.local.listNotesWithMtime(folder);
    }

    private assertWritable(path: string): void {
        validateNotePath(path);
        if (!isPathWritable(path, this.options.writeFolders))
            throw new Error(`Write access denied: '${path}' is outside the writable folders.`);
    }

    async writeNote(path: string, content: string): Promise<boolean> {
        this.assertWritable(path);
        // Overwrite only the version this server last saw; create only if absent.
        // A version that lands in between fails the condition (412) instead of being lost.
        const known = (await this.exists(path)) ? this.mirror.etagOf(path) : null;
        await this.mirror.put(path, content, known);
        return true;
    }

    async deleteNote(path: string): Promise<boolean> {
        this.assertWritable(path);
        if (!(await this.exists(path))) return false;
        await this.mirror.remove(path);
        return this.local.deleteNote(path);
    }

    async moveNote(from: string, to: string): Promise<boolean> {
        this.assertWritable(from);
        this.assertWritable(to);
        if (from === to) return (await this.exists(from)) ? true : false;
        if (!(await this.exists(from))) return false;
        // A case-only rename finds its own source on a case-insensitive disk.
        const caseOnly = from.toLowerCase() === to.toLowerCase();
        if (!caseOnly && (await this.exists(to)))
            throw new Error(`Destination already exists: ${to}`);
        await this.mirror.copy(from, to);
        await this.mirror.remove(from);
        return this.local.moveNote(from, to);
    }
}
