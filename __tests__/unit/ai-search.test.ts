import { describe, it, vi, afterEach } from "vitest";
import assert from "node:assert/strict";
import { AiSearchClient, AiSearchError } from "../../src/search/ai-search.js";
import { registerTools, type ToolRegistrar } from "../../src/tools/tools.js";
import { SearchIndex } from "../../src/search/search.js";
import type { VaultBackend } from "../../src/vault/vault-backend.js";

type Call = { url: string; init: RequestInit };

/** A fetch that records calls and answers from a queue of responses. */
function fakeFetch(responses: Array<{ status?: number; body: unknown }>) {
    const calls: Call[] = [];
    const impl = (async (url: string, init: RequestInit) => {
        calls.push({ url, init });
        const next = responses.shift() ?? { status: 500, body: "no response queued" };
        const text = typeof next.body === "string" ? next.body : JSON.stringify(next.body);
        return new Response(text, {
            status: next.status ?? 200,
            headers: { "content-type": "application/json" },
        });
    }) as unknown as typeof fetch;
    return { impl, calls };
}

const options = {
    accountId: "acct",
    token: "secret",
    namespace: "default",
    instance: "vault",
    prefix: "",
};

const chunk = (key: string, score: number, text: string) => ({
    id: "c",
    type: "text",
    score,
    text,
    item: { key, timestamp: 1 },
});

describe("AiSearchClient", () => {
    afterEach(() => vi.useRealTimers());

    it("sends the query with retrieval options and maps keys to vault paths", async () => {
        const { impl, calls } = fakeFetch([
            {
                body: {
                    success: true,
                    result: {
                        chunks: [
                            chunk("notes/vault/a.md", 0.9, "alpha"),
                            chunk("notes/vault/img.png", 0.8, "binary"),
                            chunk("notes/vault/.obsidian/x.md", 0.7, "config"),
                            chunk("other/b.md", 0.6, "outside the prefix"),
                        ],
                    },
                },
            },
        ]);
        const client = new AiSearchClient({ ...options, prefix: "/notes/vault/" }, impl);
        const hits = await client.search({ query: "q", limit: 5, minScore: 0.3 });
        assert.deepEqual(hits, [{ path: "a.md", score: 0.9, text: "alpha", timestamp: 1 }]);

        assert.equal(calls.length, 1);
        assert.equal(
            calls[0].url,
            "https://api.cloudflare.com/client/v4/accounts/acct/ai-search/namespaces/default/instances/vault/search",
        );
        const headers = calls[0].init.headers as Record<string, string>;
        assert.equal(headers.authorization, "Bearer secret");
        assert.deepEqual(JSON.parse(calls[0].init.body as string), {
            messages: [{ role: "user", content: "q" }],
            max_num_results: 5,
            score_threshold: 0.3,
        });
    });

    it("explains rejected tokens and missing instances", async () => {
        const denied = new AiSearchClient(options, fakeFetch([{ status: 403, body: "nope" }]).impl);
        await assert.rejects(
            denied.search({ query: "q", limit: 1, minScore: 0 }),
            (error: unknown) =>
                error instanceof AiSearchError && /AI Search Edit and Run/.test(error.message),
        );
        const missing = new AiSearchClient(options, fakeFetch([{ status: 404, body: "" }]).impl);
        await assert.rejects(
            missing.search({ query: "q", limit: 1, minScore: 0 }),
            /CF_AI_SEARCH_INSTANCE/,
        );
    });

    it("coalesces write bursts into one re-index job, at least 30 s apart", async () => {
        vi.useFakeTimers();
        const { impl, calls } = fakeFetch([
            { body: { success: true } },
            { body: { success: true } },
        ]);
        const client = new AiSearchClient(options, impl, () => Date.now());
        client.requestSync();
        client.requestSync();
        client.requestSync();
        await vi.advanceTimersByTimeAsync(59_000);
        assert.equal(calls.length, 0, "nothing before the debounce");
        await vi.advanceTimersByTimeAsync(1_000);
        assert.equal(calls.length, 1, "one job for the burst");
        assert.match(calls[0].url, /\/instances\/vault\/jobs$/);
        client.requestSync();
        await vi.advanceTimersByTimeAsync(60_000);
        assert.equal(calls.length, 2);
        client.close();
    });
});

describe("semantic_search tool", () => {
    const vault = {
        readNote: async () => null,
        writeNote: async () => true,
    } as unknown as VaultBackend;

    function tools(client: AiSearchClient) {
        const captured = new Map<
            string,
            { parameters: any; execute: (a: any, c: any) => Promise<string> }
        >();
        const registrar: ToolRegistrar = {
            addTool: (tool) => void captured.set(tool.name, tool as never),
        };
        registerTools(registrar, vault, new SearchIndex(), "V", false, null, client);
        return {
            call: (name: string, args: Record<string, unknown>) => {
                const tool = captured.get(name)!;
                return tool.execute(tool.parameters.parse(args), {});
            },
            has: (name: string) => captured.has(name),
        };
    }

    it("returns passages with paths, rounded scores, excerpts and links, scoped to a folder", async () => {
        const long = "x".repeat(600);
        const { impl, calls } = fakeFetch([
            {
                body: {
                    result: {
                        chunks: [
                            chunk("work/plan.md", 0.91234, "  the   plan\n\nin   detail "),
                            chunk("home/todo.md", 0.8, "elsewhere"),
                            chunk("work/notes/long.md", 0.7, long),
                        ],
                    },
                },
            },
        ]);
        const t = tools(new AiSearchClient(options, impl));
        const page = JSON.parse(
            await t.call("semantic_search", { query: "plan", folder: "/work/" }),
        );
        assert.deepEqual(
            page.results.map((r: { path: string }) => r.path),
            ["work/plan.md", "work/notes/long.md"],
        );
        assert.equal(page.results[0].score, 0.912);
        assert.equal(page.results[0].excerpt, "the plan in detail");
        assert.match(page.results[0].url, /^obsidian:\/\/open/);
        assert.equal(page.results[1].excerpt.length, 501, "excerpt is capped");
        // A folder filter asks for the maximum so the page can still fill.
        const body = JSON.parse(calls[0].init.body as string);
        assert.equal(body.max_num_results, 50);
    });

    it("reports failures as a result instead of throwing, and is absent without a client", async () => {
        const t = tools(new AiSearchClient(options, fakeFetch([{ status: 429, body: "" }]).impl));
        const page = JSON.parse(await t.call("semantic_search", { query: "plan" }));
        assert.match(page.error, /rate limited/);
        const registrar: ToolRegistrar = { addTool: () => {} };
        const names: string[] = [];
        registrar.addTool = (tool) => void names.push(tool.name);
        registerTools(registrar, vault, new SearchIndex(), "V");
        assert.ok(!names.includes("semantic_search"));
    });
});
