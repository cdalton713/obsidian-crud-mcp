import { readFileSync, statSync } from "node:fs";
import { logger } from "../logging/logger.js";
import { z } from "zod";
import { parseWriteFolders } from "../vault/write-scope.js";

// "true"/"1"/"yes"/"on" (any case) enable, "false"/"0"/"no"/"off" disable, and
// empty or unset means false. Anything else fails at startup instead of silently
// leaving a safety switch such as READ_ONLY off.
const envFlag = () =>
    z.preprocess((value) => (value === "" ? undefined : value), z.stringbool().default(false));

// Unset, empty, and whitespace-only all mean "not configured".
const optionalString = () =>
    z
        .string()
        .optional()
        .transform((value) => value?.trim() || undefined);

// Parse environment configuration once, before initializing the vault or server.
const envSchema = z.object({
    LOG_LEVEL: z.string().optional(),
    VAULT_PATH: z.string().optional(),
    INDEX_PASSPHRASE: optionalString(),
    S3_ENDPOINT: optionalString(),
    S3_BUCKET: optionalString(),
    S3_ACCESS_KEY_ID: z.string().optional(),
    S3_SECRET_ACCESS_KEY: z.string().optional(),
    S3_REGION: z.string().default("auto"),
    S3_PREFIX: z.string().default(""),
    S3_POLL_SECONDS: z.coerce.number().int().min(5).default(30),
    // Note content kept in memory for search, in millions of characters (roughly MB); 0 disables.
    SEARCH_CONTENT_CACHE_MB: z.coerce.number().min(0).default(32),
    // Optional semantic search: a Cloudflare AI Search instance over the Remotely Save bucket.
    CF_ACCOUNT_ID: optionalString(),
    CF_AI_SEARCH_TOKEN: optionalString(),
    CF_AI_SEARCH_INSTANCE: optionalString(),
    CF_AI_SEARCH_NAMESPACE: z.string().default("default"),
    VAULT_NAME: z.string().default("MyVault"),
    PORT: z
        .string()
        .regex(/^\d+$/)
        .default("8787")
        .transform(Number)
        .pipe(z.number().int().min(1).max(65535)),
    BASE_URL: z.string().optional(),
    MCP_AUTH_TOKEN: z.string().optional(),
    MCP_REFRESH_DAYS: z.coerce.number().int().min(1).default(14),
    READ_ONLY: envFlag(),
    WRITE_FOLDERS: z.string().optional().transform(parseWriteFolders),
    MCP_INSTRUCTIONS_FILE: optionalString(),
    MCP_INSTRUCTIONS: optionalString(),
    DATA_DIR: z.string().optional(),
    HOME: z.string().optional(),
    USERPROFILE: z.string().optional(),
    MCP_ALLOWED_HOSTS: z.string().optional(),
    HOST: z.string().default("0.0.0.0"),
});
export const env = envSchema.parse(process.env);

// Extra instructions appended to the MCP `instructions` string.
// File wins if both are set (loud warning); missing file is fatal.
let extraInstructions: string | undefined;
const MCP_INSTRUCTIONS_MAX_BYTES = 32 * 1024;
if (env.MCP_INSTRUCTIONS_FILE) {
    try {
        const size = statSync(env.MCP_INSTRUCTIONS_FILE).size;
        if (size > MCP_INSTRUCTIONS_MAX_BYTES) {
            throw new Error(
                `file is ${size} bytes, exceeds ${MCP_INSTRUCTIONS_MAX_BYTES} byte cap`,
            );
        }
        extraInstructions = readFileSync(env.MCP_INSTRUCTIONS_FILE, "utf8").trim() || undefined;
    } catch (err) {
        logger.error(
            `Failed to read MCP_INSTRUCTIONS_FILE (${env.MCP_INSTRUCTIONS_FILE}): ${(err as Error).message}`,
        );
        process.exit(1);
    }
    if (env.MCP_INSTRUCTIONS) {
        logger.warn("MCP_INSTRUCTIONS_FILE is set; ignoring MCP_INSTRUCTIONS env var.");
    }
} else if (env.MCP_INSTRUCTIONS) {
    extraInstructions = env.MCP_INSTRUCTIONS;
}

export const MCP_EXTRA_INSTRUCTIONS = extraInstructions;
