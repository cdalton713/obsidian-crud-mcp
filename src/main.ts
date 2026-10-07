import { createHash } from "node:crypto";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { FastMCP } from "fastmcp";
import { mountPasswordAuth, type AuthHandle } from "./auth/auth.js";
import { env, MCP_EXTRA_INSTRUCTIONS } from "./config/env.js";
import { logger, mcpLogger } from "./logging/logger.js";
import { describeError } from "./logging/redact.js";
import { AiSearchClient } from "./search/ai-search.js";
import { SearchIndex } from "./search/search.js";
import { localOnlyAuthenticator, tokenAuthenticator } from "./server/authenticate.js";
import { syncSearchIndex } from "./server/index-bootstrap.js";
import { watchVault } from "./server/watchers.js";
import { registerTools } from "./tools/tools.js";
import type { VaultBackend } from "./vault/vault-backend.js";
import { readOnlyVault } from "./vault/vault-readonly.js";

const verbose = env.LOG_LEVEL === "debug";
const { PORT, MCP_AUTH_TOKEN, READ_ONLY, WRITE_FOLDERS, VAULT_NAME } = env;
const BASE_URL = env.BASE_URL ?? `http://localhost:${PORT}`;
const SAVE_INTERVAL_MS = 5 * 60 * 1000;

function fatal(message: string): never {
    logger.error(message);
    process.exit(1);
}

const VAULT_PATH =
    env.VAULT_PATH ||
    fatal("Set VAULT_PATH: the vault folder, or the local mirror folder when S3_BUCKET is set.");

// --- Per-vault data directory ---
const baseDataDir = env.DATA_DIR ?? join(env.HOME ?? env.USERPROFILE ?? "/tmp", ".obsidian-mcp");
const vaultId = createHash("sha256").update(VAULT_NAME).digest("hex").slice(0, 12);
const dataDir = join(baseDataDir, vaultId);

// --- Vault (local folder, or a local mirror of a Remotely Save S3 bucket) ---
async function openVault(): Promise<VaultBackend> {
    if (env.S3_BUCKET) {
        mkdirSync(VAULT_PATH, { recursive: true });
        const { S3Vault } = await import("./vault/vault-s3.js");
        logger.info(`S3 mode: bucket ${env.S3_BUCKET}, mirror ${VAULT_PATH}`);
        return new S3Vault({
            vaultPath: VAULT_PATH,
            manifestPath: join(dataDir, "s3-manifest.json"),
            pollSeconds: env.S3_POLL_SECONDS,
            writeFolders: WRITE_FOLDERS,
            endpoint: env.S3_ENDPOINT,
            region: env.S3_REGION,
            bucket: env.S3_BUCKET,
            prefix: env.S3_PREFIX,
            accessKeyId: env.S3_ACCESS_KEY_ID,
            secretAccessKey: env.S3_SECRET_ACCESS_KEY,
        });
    }
    const { LocalVault } = await import("./vault/vault-local.js");
    logger.info(`Local mode: ${VAULT_PATH}`);
    return new LocalVault(VAULT_PATH, WRITE_FOLDERS);
}

// --- Search index: load the snapshot before the vault opens so changes feed it from the start ---
const searchIndex = new SearchIndex(
    join(dataDir, "search-index.json"),
    env.INDEX_PASSPHRASE,
    Math.round(env.SEARCH_CONTENT_CACHE_MB * 1024 * 1024),
);
await searchIndex.loadFromDisk();
logger.debug(`Persisted metadata: ${searchIndex.size} notes`);

let vault = await openVault();
// S3 mode: each poll reports what it downloaded or removed, content included,
// so the index is updated without re-reading or watching the mirror folder.
// Local mode: watch the folder for edits Obsidian makes.
const stopWatching = vault.subscribe
    ? vault.subscribe({
          updated: (path, content, mtime) => searchIndex.update(path, content, mtime),
          removed: (path) => searchIndex.remove(path),
      })
    : watchVault(vault, searchIndex, VAULT_PATH);
try {
    await vault.init();
} catch (err) {
    fatal(`Failed to open the vault: ${describeError(err, verbose)}`);
}
logger.info("Vault ready.");

// READ_ONLY also hides the write tools (see registerTools); wrapping the backend
// makes any write that bypasses the tools fail too.
if (READ_ONLY) vault = readOnlyVault(vault);

// Reconcile the index with the folder in the background (prunes deleted notes,
// reads any whose mtime changed while the server was down), then mark it ready.
syncSearchIndex(vault, searchIndex).catch((err: unknown) => {
    searchIndex.state = "failed";
    logger.error(`Index rebuild failed: ${describeError(err, verbose)}`);
});

// --- MCP server ---
const BASE_INSTRUCTIONS =
    "Access and manage an Obsidian vault. You can read, write, list, search, move, and delete markdown notes. Every tool response includes an Obsidian deep link. Always show this link to the user using the format [obsidian://open?vault=...&file=...](obsidian://open?vault=...&file=...) so it is both clickable and visible as a URL.";
// tsdown replaces this expression with the package version at build time; pnpm
// sets it at runtime for `pnpm dev`. Read it directly so the define applies.
const packageVersion = process.env.npm_package_version ?? "0.0.0";
const isSemver = (value: string): value is `${number}.${number}.${number}` =>
    /^\d+\.\d+\.\d+$/.test(value);

let auth: AuthHandle | null = null;
const server = new FastMCP({
    logger: mcpLogger,
    name: "obsidian-crud-mcp",
    version: isSemver(packageVersion) ? packageVersion : "0.0.0",
    instructions: MCP_EXTRA_INSTRUCTIONS
        ? `${BASE_INSTRUCTIONS}\n\n${MCP_EXTRA_INSTRUCTIONS}`
        : BASE_INSTRUCTIONS,
    authenticate: MCP_AUTH_TOKEN
        ? tokenAuthenticator(MCP_AUTH_TOKEN, BASE_URL, () => auth)
        : localOnlyAuthenticator(env.MCP_ALLOWED_HOSTS, env.HOST),
});

if (MCP_AUTH_TOKEN) {
    auth = mountPasswordAuth(
        server.getApp(),
        BASE_URL,
        MCP_AUTH_TOKEN,
        join(dataDir, "auth-tokens.json"),
        { refreshDays: env.MCP_REFRESH_DAYS },
    );
    await auth.loadTokens();
}

// --- Semantic search (optional): Cloudflare AI Search over the same bucket ---
const cfSettings = [env.CF_ACCOUNT_ID, env.CF_AI_SEARCH_TOKEN, env.CF_AI_SEARCH_INSTANCE];
if (cfSettings.some(Boolean) && !cfSettings.every(Boolean))
    fatal(
        "Semantic search needs all of CF_ACCOUNT_ID, CF_AI_SEARCH_TOKEN and CF_AI_SEARCH_INSTANCE.",
    );
const aiSearch = cfSettings.every(Boolean)
    ? new AiSearchClient({
          accountId: env.CF_ACCOUNT_ID!,
          token: env.CF_AI_SEARCH_TOKEN!,
          namespace: env.CF_AI_SEARCH_NAMESPACE,
          instance: env.CF_AI_SEARCH_INSTANCE!,
          prefix: env.S3_PREFIX,
      })
    : undefined;
logger.info(
    aiSearch
        ? `Semantic search: Cloudflare AI Search instance '${env.CF_AI_SEARCH_INSTANCE}'.`
        : "Semantic search off (set CF_ACCOUNT_ID, CF_AI_SEARCH_TOKEN, CF_AI_SEARCH_INSTANCE to enable).",
);

registerTools(server, vault, searchIndex, VAULT_NAME, READ_ONLY, WRITE_FOLDERS, aiSearch);

// --- Persistence and shutdown ---
async function persist() {
    await searchIndex.saveToDisk();
    if (auth) {
        auth.cleanup();
        await auth.saveTokens();
    }
}
setInterval(() => {
    persist().catch((err: unknown) => {
        logger.error(`Periodic save failed: ${describeError(err, verbose)}`);
    });
}, SAVE_INTERVAL_MS).unref();

async function shutdown() {
    logger.info("Shutting down...");
    aiSearch?.close();
    stopWatching();
    await persist();
    await vault.close();
    process.exit(0);
}
process.once("SIGTERM", shutdown);
process.once("SIGINT", shutdown);

// Prevent unhandled rejections from crashing the server (e.g. a failed background poll).
process.on("unhandledRejection", (err) => {
    logger.error(`Unhandled rejection: ${describeError(err, verbose)}`);
});

// Stateless: every request stands alone, so a client that opens a new MCP
// session per tool call (as hosted agents do) pays no session setup. FastMCP
// otherwise waits up to 1 s per session for client capabilities that never
// arrive before the initialize response is sent. JSON responses instead of
// SSE streams: nothing here streams, and a plain body cannot sit in a client's
// event-stream buffer until an idle timeout.
await server.start({
    transportType: "httpStream",
    httpStream: {
        port: PORT,
        endpoint: "/mcp",
        host: env.HOST,
        stateless: true,
        enableJsonResponse: true,
    },
});
logger.info(`obsidian-crud-mcp v${packageVersion} listening on port ${PORT}`);
