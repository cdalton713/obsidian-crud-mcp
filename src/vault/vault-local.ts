import {
    readFile,
    writeFile,
    unlink,
    mkdir,
    stat,
    realpath,
    rename,
    lstat,
    glob,
    opendir,
} from "node:fs/promises";
import { dirname, resolve, sep, relative, join, basename } from "node:path";
import { realpathSync } from "node:fs";
import { parseFrontmatterAndLinks } from "../notes/parse.js";
import { validateNotePath, isValidNotePath } from "../notes/note-path.js";
import type { VaultBackend } from "./vault-backend.js";
import type { NoteInfo, NoteListing } from "../types/vault-backend.js";

import { isPathWritable } from "./write-scope.js";

export class LocalVault implements VaultBackend {
    private root: string;

    constructor(
        vaultPath: string,
        private writeFolders: string[] | null = null,
    ) {
        this.root = realpathSync(resolve(vaultPath));
    }

    /** The vault folder with symlinks resolved. */
    get rootPath(): string {
        return this.root;
    }

    private async safePath(path: string, isNote = false): Promise<string> {
        // Note operations must target a real note (.md, no dot-folders, etc.);
        // folder listing passes a directory and skips this.
        if (isNote) validateNotePath(path);
        const full = resolve(this.root, path);
        // Lexical check first (catches ../ without hitting disk)
        if (!full.startsWith(this.root + sep)) {
            throw new Error("Path traversal blocked");
        }
        // Resolve symlinks and re-check (catches symlink escapes)
        try {
            const real = await realpath(full);
            if (!real.startsWith(this.root + sep)) {
                throw new Error("Path traversal blocked");
            }
            return real;
        } catch (e: any) {
            if (e.code !== "ENOENT") throw e;
            // The file (or a parent) doesn't exist yet. Confirm the nearest
            // existing ancestor still resolves inside the vault, so a symlinked
            // parent directory can't redirect a write outside the root.
            const real = await this.resolveMissingPath(full);
            if (!real.startsWith(this.root + sep)) {
                throw new Error("Path traversal blocked");
            }
            return real;
        }
    }

    private async resolveMissingPath(target: string): Promise<string> {
        try {
            return await realpath(target);
        } catch (error: any) {
            if (error.code !== "ENOENT") throw error;
            const entry = await lstat(target).catch((statError: NodeJS.ErrnoException) => {
                if (statError.code !== "ENOENT") throw statError;
                return null;
            });
            if (entry?.isSymbolicLink())
                throw new Error("Path traversal blocked: dangling symlink");
            const parent = dirname(target);
            if (parent === target) throw error;
            return join(await this.resolveMissingPath(parent), basename(target));
        }
    }

    async init(): Promise<void> {}

    private toVaultPath(full: string): string {
        return relative(this.root, full).split(sep).join("/");
    }

    /**
     * Write folders as they are spelled on disk, so they compare against
     * realpath-resolved note paths on case-insensitive filesystems. A folder
     * that resolves elsewhere (it is itself a symlink) keeps its configured
     * name: following it would widen the scope to the link target.
     */
    private async canonicalWriteFolders(): Promise<string[] | null> {
        if (this.writeFolders === null) return null;
        return Promise.all(
            this.writeFolders.map(async (folder) => {
                const real = await realpath(resolve(this.root, folder)).catch(() => null);
                const canonical = real === null ? null : this.toVaultPath(real);
                return canonical !== null && canonical.toLowerCase() === folder.toLowerCase()
                    ? canonical
                    : folder;
            }),
        );
    }

    private async writablePath(path: string): Promise<string> {
        const full = await this.safePath(path, true);
        const resolved = this.toVaultPath(full);
        if (
            !isPathWritable(path, this.writeFolders) ||
            !isPathWritable(resolved, await this.canonicalWriteFolders())
        ) {
            throw new Error(`Write access denied: '${path}' is outside the writable folders.`);
        }
        return full;
    }

    async close(): Promise<void> {}

    async readNote(path: string): Promise<string | null> {
        const fullPath = await this.safePath(path, true);
        try {
            return await readFile(fullPath, "utf-8");
        } catch {
            return null;
        }
    }

    async writeNote(path: string, content: string): Promise<boolean> {
        const fullPath = await this.writablePath(path);
        try {
            await mkdir(dirname(fullPath), { recursive: true });
            await writeFile(fullPath, content, "utf-8");
            return true;
        } catch {
            return false;
        }
    }

    async deleteNote(path: string): Promise<boolean> {
        const fullPath = await this.writablePath(path);
        try {
            await unlink(fullPath);
            return true;
        } catch {
            return false;
        }
    }

    async moveNote(from: string, to: string): Promise<boolean> {
        const fromPath = await this.writablePath(from);
        const toPath = await this.writablePath(to);
        if (!(await this.exists(fromPath))) return false;
        let target = toPath;
        if (fromPath === toPath) {
            // Both names resolve to the same file: an identical path or a
            // case-only rename on a case-insensitive filesystem. Rename to the
            // requested spelling of the file name; realpath() reports the old one.
            target = join(dirname(fromPath), basename(resolve(this.root, to)));
            if (target === fromPath) return true;
        } else if (await this.exists(toPath)) {
            throw new Error(`Destination already exists: ${to}`);
        }
        try {
            await mkdir(dirname(target), { recursive: true });
            await rename(fromPath, target);
            return true;
        } catch (e: any) {
            if (e.code === "EXDEV") {
                // Cross-device: fall back to copy-delete
                const content = await this.readNote(from);
                if (content === null) return false;
                const wrote = await this.writeNote(to, content);
                if (!wrote) return false;
                return await this.deleteNote(from);
            }
            return false;
        }
    }

    private async exists(full: string): Promise<boolean> {
        return lstat(full).then(
            () => true,
            (e: NodeJS.ErrnoException) => {
                if (e.code === "ENOENT") return false;
                throw e;
            },
        );
    }

    async getMetadata(path: string): Promise<NoteInfo | null> {
        const fullPath = await this.safePath(path, true);
        try {
            const [content, s] = await Promise.all([readFile(fullPath, "utf-8"), stat(fullPath)]);
            return {
                path,
                size: s.size,
                ctime: s.birthtimeMs,
                mtime: s.mtimeMs,
                ...parseFrontmatterAndLinks(content),
            };
        } catch {
            return null;
        }
    }

    async listNotes(folder?: string): Promise<string[]> {
        const notes = await this.listNotesWithMtime(folder);
        return notes.map((n) => n.path);
    }

    async listNotesWithMtime(folder?: string): Promise<NoteListing[]> {
        if (folder && !folder.endsWith("/") && !folder.endsWith("\\")) folder += "/";
        const searchDir = folder ? await this.safePath(folder) : this.root;
        const entries: string[] = [];
        try {
            // glob() swallows directory errors and yields nothing, which would
            // look like an empty vault; probe the directory so failures surface.
            // A folder that doesn't exist (or isn't a folder) just has no notes.
            try {
                await (await opendir(searchDir)).close();
            } catch (error) {
                const code = (error as NodeJS.ErrnoException).code;
                if (folder && (code === "ENOENT" || code === "ENOTDIR")) return [];
                throw error;
            }
            for await (const entry of glob("**/*.md", { cwd: searchDir })) {
                const full = folder ? `${folder}${entry}` : entry;
                if (!isValidNotePath(full)) continue;
                entries.push(full);
            }
        } catch (error) {
            throw new Error(`Failed to list notes${folder ? ` in '${folder}'` : ""}`, {
                cause: error,
            });
        }
        const results = await Promise.all(
            entries.map(async (p) => {
                try {
                    const s = await stat(resolve(this.root, p));
                    return { path: p, mtime: s.mtimeMs };
                } catch {
                    return { path: p, mtime: 0 };
                }
            }),
        );
        return results.sort((a, b) => a.path.localeCompare(b.path));
    }
}
