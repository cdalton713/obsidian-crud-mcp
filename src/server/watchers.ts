import { watch } from "node:fs";
import { stat } from "node:fs/promises";
import { join } from "node:path";
import { logger } from "../logging/logger.js";
import type { SearchIndex } from "../search/search.js";
import type { VaultBackend } from "../vault/vault-backend.js";

/** Obsidian fires 2-3 filesystem events per save; coalesce them per file. */
const DEBOUNCE_MS = 100;

/**
 * Keep the search index in sync with edits made outside this server: by
 * Obsidian in local mode, or by the S3 mirror writing downloaded notes into
 * the folder. Returns a stop function.
 */
export function watchVault(vault: VaultBackend, index: SearchIndex, vaultPath: string): () => void {
    const pending = new Map<string, NodeJS.Timeout>();

    const refresh = async (notePath: string) => {
        pending.delete(notePath);
        try {
            const content = await vault.readNote(notePath);
            if (content === null) {
                index.remove(notePath);
                return;
            }
            const { mtimeMs } = await stat(join(vaultPath, notePath));
            index.update(notePath, content, mtimeMs);
        } catch {
            // File deleted or path blocked by safePath
            index.remove(notePath);
        }
    };

    const watcher = watch(vaultPath, { recursive: true }, (_event, filename) => {
        if (!filename?.endsWith(".md")) return;
        const notePath = filename.replaceAll("\\", "/");
        if (notePath.startsWith(".obsidian/") || notePath.includes("/.obsidian/")) return;

        clearTimeout(pending.get(notePath));
        pending.set(
            notePath,
            setTimeout(() => void refresh(notePath), DEBOUNCE_MS),
        );
    });
    // Node's recursive watch adds one inotify watch per file on Linux; past the
    // kernel limit it emits ENOSPC, which must not take the server down.
    watcher.on("error", (error: Error) => {
        logger.error(
            `Vault watcher failed: ${error.message}. External edits are not tracked until restart; on Linux, raise fs.inotify.max_user_watches.`,
        );
        watcher.close();
    });
    logger.info("Watching vault for external changes.");

    return () => {
        watcher.close();
        for (const timer of pending.values()) clearTimeout(timer);
        pending.clear();
    };
}
