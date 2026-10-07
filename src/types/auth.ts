import { z } from "zod";

export const PendingAuthSchema = z.object({
    clientId: z.string(),
    redirectUri: z.string(),
    codeChallenge: z.string(),
    codeChallengeMethod: z.string(),
    state: z.string(),
    code: z.string(),
    createdAt: z.number(),
    approved: z.boolean(),
});
export type PendingAuth = z.infer<typeof PendingAuthSchema>;

export const TokenRecordSchema = z.object({
    accessToken: z.string(),
    refreshToken: z.string(),
    clientId: z.string(),
    expiresAt: z.number(),
    refreshExpiresAt: z.number(),
});
export type TokenRecord = z.infer<typeof TokenRecordSchema>;

export const RegisteredClientSchema = z.object({
    clientId: z.string(),
    clientSecret: z.string().optional(),
    // Registrations persisted before this field existed were confidential clients.
    tokenEndpointAuthMethod: z.enum(["client_secret_post", "none"]).default("client_secret_post"),
    redirectUris: z.array(z.string()),
    clientName: z.string().optional(),
    createdAt: z.number().optional(),
});
export type RegisteredClient = z.infer<typeof RegisteredClientSchema>;
