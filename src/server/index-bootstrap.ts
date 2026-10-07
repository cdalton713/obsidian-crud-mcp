import { logger } from "../logging/logger.js";
import type { SearchIndex } from "../search/search.js";
import type { VaultBackend } from "../vault/vault-backend.js";

/** Notes read at once; reads come from local disk, so this mainly hides syscall latency. */
const READ_CONCURRENCY = 16;

const elapsed = (start: number) => `${((performance.now() - start) / 1000).toFixed(1)}s`;

/**
 * Bring the persisted search index up to date with the vault, then mark it ready.
 * Notes whose mtime matches the persisted index keep their metadata; the rest are re-read.
 */
export async function syncSearchIndex(vault: VaultBackend, index: SearchIndex): Promise<void> {
    const start = performance.now();
    await rescanVault(vault, index, start);
    await index.saveToDisk();
    index.state = "ready";
}

async function rescanVault(vault: VaultBackend, index: SearchIndex, start: number): Promise<void> {
    const notes = await vault.listNotesWithMtime();
    logger.debug(`Vault has ${notes.length} notes`);

    // listNotesWithMtime throws when the vault cannot be listed, so an empty
    // result really is an empty vault and stale entries can be pruned.
    const vaultPaths = new Set(notes.map((n) => n.path));
    let pruned = 0;
    for (const path of index.listPaths()) {
        if (!vaultPaths.has(path)) {
            index.remove(path);
            pruned++;
        }
    }
    if (pruned > 0) logger.info(`Removed ${pruned} deleted notes from the search index.`);
    if (notes.length === 0) {
        logger.info("Vault has no notes; search index is empty.");
        return;
    }

    // Unchanged since the persisted index was written: keep its metadata.
    const stale = notes.filter(
        ({ path, mtime }) => !(mtime > 0 && index.has(path) && index.getMtime(path) === mtime),
    );
    logger.info(`Building search index (${notes.length} notes, ${stale.length} to read)...`);
    let next = 0;
    let done = 0;
    const worker = async () => {
        for (let note = stale[next++]; note; note = stale[next++]) {
            const content = await vault.readNote(note.path);
            // Index empty notes too (content === ""); readNote returns null only if absent.
            if (content !== null) index.update(note.path, content, note.mtime);
            done++;
            if (stale.length > 100 && done % 500 === 0) {
                logger.info(`  indexed ${done}/${stale.length}...`);
            }
        }
    };
    await Promise.all(Array.from({ length: READ_CONCURRENCY }, worker));
    logger.info(
        `Search index built: ${index.size} notes in ${elapsed(start)} (${notes.length - stale.length} unchanged).`,
    );
}
