/**
 * Metadata index for vault notes.
 *
 * Tracks paths, mtimes, tags, links, and backlinks, and keeps note content in
 * memory (up to a cap) so content scans need no disk reads.
 * Persists metadata to disk (encrypted with AES-256-GCM when INDEX_PASSPHRASE
 * is set); content is not persisted and refills from the vault after a restart.
 * No full-text index — scans run over the cached content.
 */

import { readFile, writeFile, mkdir, rename, unlink } from "node:fs/promises";
import { dirname, basename, join } from "node:path";
import { createCipheriv, createDecipheriv, randomBytes, randomUUID, scryptSync } from "node:crypto";
import { logger, mcpLogger } from "../logging/logger.js";
import { parseFrontmatterAndLinks } from "../notes/parse.js";
import { isValidNotePath } from "../notes/note-path.js";
import { PersistedIndexSchema, type IndexState } from "../types/search.js";

/** Bump when parsed metadata changes meaning; older persisted indexes are discarded and rebuilt. */
const INDEX_SCHEMA_VERSION = 3;

function encrypt(text: string, passphrase: string): string {
    const salt = randomBytes(16);
    const key = scryptSync(passphrase, salt, 32);
    const iv = randomBytes(12);
    const cipher = createCipheriv("aes-256-gcm", key, iv);
    const encrypted = Buffer.concat([cipher.update(text, "utf-8"), cipher.final()]);
    const tag = cipher.getAuthTag();
    return (
        salt.toString("hex") +
        ":" +
        iv.toString("hex") +
        ":" +
        tag.toString("hex") +
        ":" +
        encrypted.toString("hex")
    );
}

function decrypt(data: string, passphrase: string): string {
    const [saltHex, ivHex, tagHex, encryptedHex] = data.split(":");
    const key = scryptSync(passphrase, Buffer.from(saltHex, "hex"), 32);
    const decipher = createDecipheriv("aes-256-gcm", key, Buffer.from(ivHex, "hex"));
    decipher.setAuthTag(Buffer.from(tagHex, "hex"));
    return Buffer.concat([
        decipher.update(Buffer.from(encryptedHex, "hex")),
        decipher.final(),
    ]).toString("utf-8");
}

/**
 * Lifecycle of the in-memory index. "building" from construction until the
 * startup rebuild finishes, "ready" afterwards, "failed" if the rebuild threw.
 * Read by list_notes so a client can tell a partial index from a complete one.
 */

export class SearchIndex {
    private _state: IndexState = "building";
    /** Note content by path, bounded by `maxContentChars`; see `cacheContent`. */
    private contents = new Map<string, string>();
    private contentChars = 0;
    private contentCapWarned = false;
    private mtimes = new Map<string, number>();
    private tags = new Map<string, string[]>();
    private links = new Map<string, string[]>();
    private backlinks = new Map<string, Set<string>>();
    private knownPaths = new Set<string>();
    /** The save currently writing, if any. */
    private saveInFlight: Promise<void> | null = null;
    /** A save was requested while one was in flight; it runs once that finishes. */
    private saveQueued: Promise<void> | null = null;
    private persistPath: string | null;
    private passphrase: string | null;
    private readonly maxContentChars: number;

    constructor(persistPath?: string, passphrase?: string, maxContentChars = 32 * 1024 * 1024) {
        this.persistPath = persistPath ?? null;
        this.passphrase = passphrase ?? null;
        this.maxContentChars = maxContentChars;
    }

    /** Cached content for a path, if the cache holds it. */
    getContent(path: string): string | undefined {
        return this.contents.get(path);
    }

    /** Characters of note content held in memory. */
    get contentSize(): number {
        return this.contentChars;
    }

    /**
     * Keep `content` in memory for scans. Beyond the cap the note is left out
     * (and any older copy dropped), so scans read it from disk instead; memory
     * stays bounded while the vault stays searchable.
     */
    cacheContent(path: string, content: string): void {
        const previous = this.contents.get(path);
        if (previous !== undefined) {
            this.contentChars -= previous.length;
            this.contents.delete(path);
        }
        if (this.contentChars + content.length > this.maxContentChars) {
            if (!this.contentCapWarned) {
                this.contentCapWarned = true;
                logger.warn(
                    `Search content cache is full (${this.maxContentChars} chars); notes beyond it are scanned from disk. Raise SEARCH_CONTENT_CACHE_MB to cache the whole vault.`,
                );
            }
            return;
        }
        this.contents.set(path, content);
        this.contentChars += content.length;
    }

    private dropContent(path: string): void {
        const previous = this.contents.get(path);
        if (previous === undefined) return;
        this.contentChars -= previous.length;
        this.contents.delete(path);
    }

    /** Load metadata from disk. */
    async loadFromDisk(): Promise<boolean> {
        if (!this.persistPath) return false;
        try {
            let raw = await readFile(this.persistPath, "utf-8");
            if (this.passphrase) {
                raw = decrypt(raw, this.passphrase);
            }
            const json: unknown = JSON.parse(raw);
            const version = (json as { version?: unknown } | null)?.version;
            if (version !== INDEX_SCHEMA_VERSION) {
                logger.info("Persisted search metadata uses an older format; rebuilding.");
                return false;
            }
            const parsed = PersistedIndexSchema.safeParse(json);
            if (!parsed.success) {
                logger.warn("Persisted search metadata is malformed; rebuilding.");
                return false;
            }
            const data = parsed.data;
            for (const [path, mtime] of Object.entries(data.mtimes)) {
                this.mtimes.set(path, mtime);
                this.knownPaths.add(path);
            }
            for (const [path, t] of Object.entries(data.tags)) {
                this.tags.set(path, t);
            }
            for (const [path, targets] of Object.entries(data.links)) {
                this.links.set(path, targets);
                for (const target of targets) {
                    const key = target.toLowerCase();
                    if (!this.backlinks.has(key)) this.backlinks.set(key, new Set());
                    this.backlinks.get(key)!.add(path);
                }
            }
            logger.info(`Search metadata loaded from disk (${this.knownPaths.size} notes).`);
            return this.knownPaths.size > 0;
        } catch {
            return false;
        }
    }

    /**
     * Save metadata to disk. Encrypted if passphrase is set.
     *
     * A call made while a save is in flight waits for it and then saves once
     * more, so the latest state (e.g. the shutdown save) is never dropped.
     * Concurrent callers during the same in-flight save share that one re-save.
     */
    saveToDisk(): Promise<void> {
        if (!this.persistPath) return Promise.resolve();
        if (this.saveQueued) return this.saveQueued;
        if (this.saveInFlight) {
            const queued = this.saveInFlight.then(() => {
                this.saveQueued = null;
                return this.startSave();
            });
            this.saveQueued = queued;
            return queued;
        }
        return this.startSave();
    }

    private startSave(): Promise<void> {
        const save = this.writeSnapshot().finally(() => {
            if (this.saveInFlight === save) this.saveInFlight = null;
        });
        this.saveInFlight = save;
        return save;
    }

    /** Write the current state to a private temp file, then rename it into place. */
    private async writeSnapshot(): Promise<void> {
        const persistPath = this.persistPath!;
        const tmpPath = join(
            dirname(persistPath),
            `.${basename(persistPath)}.${process.pid}.${randomUUID()}.tmp`,
        );
        try {
            await mkdir(dirname(persistPath), { recursive: true });
            let data = JSON.stringify({
                version: INDEX_SCHEMA_VERSION,
                mtimes: Object.fromEntries(this.mtimes),
                tags: Object.fromEntries(this.tags),
                links: Object.fromEntries(this.links),
            });
            if (this.passphrase) {
                data = encrypt(data, this.passphrase);
            }
            // "wx" + 0o600: a fresh file only this user can read, never a reused one.
            await writeFile(tmpPath, data, { encoding: "utf-8", mode: 0o600, flag: "wx" });
            await rename(tmpPath, persistPath);
            logger.info(
                `Search index saved to disk (${this.knownPaths.size} notes${this.passphrase ? ", encrypted" : ""}).`,
            );
        } catch (err) {
            await unlink(tmpPath).catch(() => {});
            // Variadic console-style logger: the pino hook redacts and describes err once.
            mcpLogger.error("Failed to save search index:", err);
        }
    }

    /** Add or update a note in the index. */
    update(path: string, content: string, mtime?: number): void {
        if (this.knownPaths.has(path)) {
            this.clearBacklinks(path);
        }
        this.knownPaths.add(path);
        if (mtime !== undefined) this.mtimes.set(path, mtime);
        this.cacheContent(path, content);
        const parsed = parseFrontmatterAndLinks(content);
        if (parsed.tags.length > 0) {
            this.tags.set(path, parsed.tags);
        } else {
            this.tags.delete(path);
        }
        if (parsed.links.length > 0) {
            this.links.set(path, parsed.links);
            for (const target of parsed.links) {
                const key = target.toLowerCase();
                if (!this.backlinks.has(key)) this.backlinks.set(key, new Set());
                this.backlinks.get(key)!.add(path);
            }
        } else {
            this.links.delete(path);
        }
    }

    /** Remove a note from the index. */
    remove(path: string): void {
        this.dropContent(path);
        if (this.knownPaths.has(path)) {
            this.knownPaths.delete(path);
            this.mtimes.delete(path);
            this.tags.delete(path);
            this.clearBacklinks(path);
        }
    }

    /** Remove all backlink entries where path is the source. */
    private clearBacklinks(path: string): void {
        const oldLinks = this.links.get(path);
        if (oldLinks) {
            for (const target of oldLinks) {
                const key = target.toLowerCase();
                this.backlinks.get(key)?.delete(path);
                if (this.backlinks.get(key)?.size === 0) this.backlinks.delete(key);
            }
        }
        this.links.delete(path);
    }

    /** List all indexed paths, optionally filtered by folder prefix. */
    listPaths(folder?: string): string[] {
        return this.listWithMtime(folder).map((n) => n.path);
    }

    /** List all indexed paths with mtimes, optionally filtered by folder prefix. */
    listWithMtime(folder?: string): Array<{ path: string; mtime: number }> {
        const prefix = folder && !folder.endsWith("/") ? folder + "/" : folder;
        const entries = [...this.knownPaths]
            .filter(isValidNotePath)
            .filter((p) => !prefix || p.startsWith(prefix))
            .map((p) => ({ path: p, mtime: this.mtimes.get(p) ?? 0 }));
        return entries.sort((a, b) => a.path.localeCompare(b.path));
    }

    /** Whether a path is in the index. */
    has(path: string): boolean {
        return this.knownPaths.has(path);
    }

    /** Get mtime for a path. */
    getMtime(path: string): number {
        return this.mtimes.get(path) ?? 0;
    }

    /** Get tags for a path. */
    getTags(path: string): string[] {
        return this.tags.get(path) ?? [];
    }

    /** Get outgoing links for a path. */
    getLinks(path: string): string[] {
        return this.links.get(path) ?? [];
    }

    /** Get backlinks for a path (notes that link to it). Case-insensitive, matches by full path or filename. */
    getBacklinks(path: string): string[] {
        const results = new Set<string>();
        const withMd = (path.endsWith(".md") ? path : path + ".md").toLowerCase();
        const withoutMd = (path.endsWith(".md") ? path.slice(0, -3) : path).toLowerCase();
        const nameOnly = withoutMd.includes("/")
            ? withoutMd.slice(withoutMd.lastIndexOf("/") + 1)
            : withoutMd;

        for (const target of [withMd, withoutMd, nameOnly]) {
            const sources = this.backlinks.get(target);
            if (sources) {
                for (const s of sources) results.add(s);
            }
        }
        return [...results].sort();
    }

    /** List all tags across the vault with counts. */
    listAllTags(): Array<{ tag: string; count: number }> {
        const counts = new Map<string, number>();
        for (const tags of this.tags.values()) {
            for (const t of tags) {
                counts.set(t, (counts.get(t) ?? 0) + 1);
            }
        }
        return [...counts.entries()]
            .map(([tag, count]) => ({ tag, count }))
            .sort((a, b) => b.count - a.count);
    }

    get state(): IndexState {
        return this._state;
    }

    set state(value: IndexState) {
        this._state = value;
    }

    get size(): number {
        return this.knownPaths.size;
    }
}
