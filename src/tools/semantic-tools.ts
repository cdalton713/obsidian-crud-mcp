import { makeDeepLink } from "../notes/deeplink.js";
import type { AiSearchClient } from "../search/ai-search.js";
import { SemanticSearchParametersSchema } from "../types/ai-search.js";
import type { ToolRegistrar } from "./tools.js";

const EXCERPT_MAX_CHARS = 500;

export function registerSemanticTools(
    server: ToolRegistrar,
    client: AiSearchClient,
    vaultName: string,
) {
    server.addTool({
        name: "semantic_search",
        description:
            "Find notes by meaning, not exact wording: a ranked hybrid (embedding + keyword) search over the whole vault. Returns the best-matching passages with note path, score (0 to 1) and URL; several passages may come from one note. The index refreshes on a schedule and shortly after this server writes a note, so an edit made minutes ago may be missing; search_notes always reads current content. Use this first when you do not know the exact words.",
        parameters: SemanticSearchParametersSchema,
        execute: async ({ query, folder, limit, min_score }) => {
            const scope = folder?.replace(/^\/+|\/+$/g, "");
            try {
                // A folder filter is applied here, so ask for more to fill the page.
                const hits = await client.search({
                    query,
                    limit: scope ? 50 : limit,
                    minScore: min_score,
                });
                const results = hits
                    .filter((hit) => !scope || hit.path.startsWith(scope + "/"))
                    .slice(0, limit)
                    .map((hit) => {
                        const text = hit.text.replace(/\s+/g, " ").trim();
                        return {
                            path: hit.path,
                            score: Math.round(hit.score * 1000) / 1000,
                            excerpt:
                                text.length > EXCERPT_MAX_CHARS
                                    ? text.slice(0, EXCERPT_MAX_CHARS) + "…"
                                    : text,
                            url: makeDeepLink(vaultName, hit.path),
                        };
                    });
                return JSON.stringify({ query, results });
            } catch (error) {
                return JSON.stringify({
                    error: error instanceof Error ? error.message : "Semantic search failed.",
                });
            }
        },
    });
}
