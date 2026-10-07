import type { NoteInfo, NoteListing, VaultChangeListener } from "../types/vault-backend.js";

export interface VaultBackend {
    init(): Promise<void>;
    close(): Promise<void>;
    readNote(path: string): Promise<string | null>;
    writeNote(path: string, content: string): Promise<boolean>;
    deleteNote(path: string): Promise<boolean>;
    moveNote(from: string, to: string): Promise<boolean>;
    getMetadata(path: string): Promise<NoteInfo | null>;
    listNotes(folder?: string): Promise<string[]>;
    listNotesWithMtime(folder?: string): Promise<NoteListing[]>;
    /**
     * Report changes that arrive from outside this server, with their content.
     * A backend that knows what changed (the S3 mirror) implements this so the
     * search index needs no filesystem watcher. Returns an unsubscribe function.
     */
    subscribe?(listener: VaultChangeListener): () => void;
}
