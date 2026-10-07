import { z } from "zod";

/** Connection to one Cloudflare AI Search instance that indexes the Remotely Save bucket. */
export interface AiSearchOptions {
    accountId: string;
    token: string;
    namespace: string;
    instance: string;
    /** S3_PREFIX: the part of every object key that is not the vault path. */
    prefix: string;
}

export const AiSearchChunkSchema = z
    .object({
        score: z.number(),
        text: z.string(),
        item: z.object({ key: z.string(), timestamp: z.number().optional() }).loose(),
    })
    .loose();

/** The REST API wraps the payload in `result`; the Workers binding does not. Accept both. */
export const AiSearchResponseSchema = z
    .object({
        result: z
            .object({ chunks: z.array(AiSearchChunkSchema).default([]) })
            .loose()
            .optional(),
        chunks: z.array(AiSearchChunkSchema).optional(),
    })
    .loose();

/** One matching passage, with the object key already turned into a vault path. */
export interface SemanticHit {
    path: string;
    score: number;
    text: string;
    timestamp?: number;
}

export const SemanticSearchParametersSchema = z.object({
    query: z
        .string()
        .min(1)
        .max(500)
        .describe(
            "What you are looking for, in plain language. Meaning matters more than exact words.",
        ),
    folder: z
        .string()
        .max(1000)
        .optional()
        .describe("Only notes inside this folder and its descendants."),
    limit: z.number().int().min(1).max(50).default(10).describe("Passages to return, 1 to 50."),
    min_score: z
        .number()
        .min(0)
        .max(1)
        .default(0.4)
        .describe("Drop passages scoring below this (0 to 1)."),
});
export type SemanticSearchParameters = z.infer<typeof SemanticSearchParametersSchema>;
