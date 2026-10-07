/**
 * Semantic search over the vault through Cloudflare AI Search.
 *
 * AI Search indexes the same R2 bucket Remotely Save syncs to (chunking,
 * embeddings and re-indexing are its job), so the server only sends queries
 * and maps the object keys it gets back to vault paths. After the server
 * writes a note it asks for a re-index, coalescing bursts, so an agent's own
 * edits turn up within minutes instead of at the next scheduled sync.
 */

import { logger } from "../logging/logger.js";
import { normalizePrefix, isMirroredPath } from "../vault/s3-mirror.js";
import {
    AiSearchResponseSchema,
    type AiSearchOptions,
    type SemanticHit,
} from "../types/ai-search.js";

const REQUEST_TIMEOUT_MS = 60_000;
/** Wait this long after the last write before asking for a re-index. */
const SYNC_DEBOUNCE_MS = 60_000;
/** AI Search accepts at most one indexing job per 30 s per instance. */
const SYNC_MIN_INTERVAL_MS = 30_000;

export class AiSearchError extends Error {}

export interface SemanticQuery {
    query: string;
    limit: number;
    minScore: number;
}

export class AiSearchClient {
    private readonly prefix: string;
    private readonly base: string;
    private syncTimer: NodeJS.Timeout | undefined;
    private lastSyncAt = 0;

    constructor(
        private readonly options: AiSearchOptions,
        private readonly fetchImpl: typeof fetch = fetch,
        private readonly now: () => number = Date.now,
    ) {
        this.prefix = normalizePrefix(options.prefix);
        this.base = `https://api.cloudflare.com/client/v4/accounts/${encodeURIComponent(options.accountId)}/ai-search/namespaces/${encodeURIComponent(options.namespace)}/instances/${encodeURIComponent(options.instance)}`;
    }

    private async call(path: string, body: unknown): Promise<unknown> {
        let response: Response;
        try {
            response = await this.fetchImpl(`${this.base}${path}`, {
                method: "POST",
                headers: {
                    authorization: `Bearer ${this.options.token}`,
                    "content-type": "application/json",
                },
                body: JSON.stringify(body),
                signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
            });
        } catch (error) {
            throw new AiSearchError(`AI Search unreachable: ${(error as Error).message}`);
        }

        if (!response.ok) {
            const detail = (await response.text().catch(() => "")).slice(0, 300);
            const hint =
                response.status === 401 || response.status === 403
                    ? "the token was rejected; it needs AI Search Edit and Run permissions"
                    : response.status === 404
                      ? "no such instance; check CF_AI_SEARCH_INSTANCE and CF_AI_SEARCH_NAMESPACE"
                      : response.status === 429
                        ? "rate limited; try again shortly"
                        : detail || response.statusText;
            throw new AiSearchError(`AI Search ${path} failed (${response.status}): ${hint}`);
        }
        return response.json();
    }

    /** Vault path for an object key, or null when the key is not a note of this vault. */
    pathOf(key: string): string | null {
        if (!key.startsWith(this.prefix)) return null;
        const path = key.slice(this.prefix.length);
        return isMirroredPath(path) ? path : null;
    }

    async search({ query, limit, minScore }: SemanticQuery): Promise<SemanticHit[]> {
        // The same body Wrangler's `ai-search search` sends; retrieval mode is an instance setting.
        const raw = await this.call("/search", {
            messages: [{ role: "user", content: query }],
            max_num_results: limit,
            score_threshold: minScore,
        });
        const parsed = AiSearchResponseSchema.safeParse(raw);
        if (!parsed.success) throw new AiSearchError("AI Search returned an unexpected response.");
        const chunks = parsed.data.result?.chunks ?? parsed.data.chunks ?? [];
        const hits: SemanticHit[] = [];
        for (const chunk of chunks) {
            const path = this.pathOf(chunk.item.key);
            if (path === null) continue;
            hits.push({
                path,
                score: chunk.score,
                text: chunk.text,
                timestamp: chunk.item.timestamp,
            });
        }
        return hits;
    }

    /** Start an indexing job now. */
    async createJob(): Promise<void> {
        await this.call("/jobs", {});
    }

    /**
     * A note changed on the server's side: re-index soon. Calls within a
     * minute collapse into one job, and jobs stay at least 30 s apart.
     */
    requestSync(): void {
        if (this.syncTimer) return;
        const sinceLast = this.now() - this.lastSyncAt;
        const delay = Math.max(SYNC_DEBOUNCE_MS, SYNC_MIN_INTERVAL_MS - sinceLast);
        this.syncTimer = setTimeout(() => {
            this.syncTimer = undefined;
            this.lastSyncAt = this.now();
            this.createJob().then(
                () => logger.info("AI Search: re-index requested after a write."),
                (error: unknown) =>
                    logger.warn(`AI Search: re-index request failed: ${(error as Error).message}`),
            );
        }, delay);
        this.syncTimer.unref();
    }

    close(): void {
        clearTimeout(this.syncTimer);
        this.syncTimer = undefined;
    }
}
