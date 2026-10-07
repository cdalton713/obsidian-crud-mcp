import type { IndexState } from "./search.js";

export interface IndexStatus {
    state: IndexState;
    size: number;
    /** True when the notes were read straight from the vault because the index had nothing — the list itself is complete. */
    servedByVault?: boolean;
}

export interface ListingSummary {
    /** Notes actually returned (after the limit). */
    shown: number;
    /** Notes matching all filters (before the limit). */
    matched: number;
    /** Notes in the whole vault, before any filter; null when unknown. */
    vaultTotal: number | null;
    /** Folder filter as given by the caller, if any. */
    folder?: string;
    /** Notes inside `folder` before the other filters; null when unknown. */
    folderTotal?: number | null;
    /** Non-folder filters that were applied, e.g. `name="x"`, `tag="y"`. */
    filters: string[];
    sortBy: "name" | "modified";
    limit: number;
    /** Paths cut by the limit, used to name the omitted folders. */
    omitted: string[];
    index: IndexStatus;
}

export interface FolderCount {
    folder: string;
    count: number;
}
