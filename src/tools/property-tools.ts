import { UpdateNotePropertiesParametersSchema } from "../types/property-tools.js";
import type { ToolRegistrar } from "./tools.js";

import type { VaultBackend } from "../vault/vault-backend.js";
import type { SearchIndex } from "../search/search.js";
import { makeDeepLink } from "../notes/deeplink.js";
import { isPathWritable } from "../vault/write-scope.js";
import { updateProperties } from "../notes/note-properties.js";

export function registerPropertyTools(
    server: ToolRegistrar,
    vault: VaultBackend,
    index: SearchIndex,
    vaultName: string,
    writeFolders: string[] | null,
    onChange?: () => void,
) {
    server.addTool({
        name: "update_note_properties",
        description:
            "Set or remove top-level YAML properties in an existing note. Supports strings, numbers, booleans, null, and lists of these values. Frontmatter is serialized again, so comments, spacing, quoting, and list style can change. The Markdown body stays byte-for-byte identical. Rejects malformed YAML and preserves unrelated property values. Obeys writable-folder restrictions.",
        parameters: UpdateNotePropertiesParametersSchema,
        execute: async ({ path, set, remove }) => {
            if (!isPathWritable(path, writeFolders))
                return JSON.stringify({
                    error: "Write access denied: path is outside the writable folders.",
                });
            const existing = await vault.readNote(path);
            if (existing === null) return JSON.stringify({ error: `Note not found: ${path}` });
            let updated: string;
            try {
                updated = updateProperties(existing, set, remove);
            } catch (error) {
                return JSON.stringify({
                    error: error instanceof Error ? error.message : "Invalid frontmatter.",
                });
            }
            if (updated === existing)
                return JSON.stringify({
                    status: "unchanged",
                    path,
                    url: makeDeepLink(vaultName, path),
                });
            if (!(await vault.writeNote(path, updated)))
                return JSON.stringify({ error: `Failed to write note: ${path}` });
            index.update(path, updated, Date.now());
            onChange?.();
            return JSON.stringify({ status: "updated", path, url: makeDeepLink(vaultName, path) });
        },
    });
}
