/**
 * Password-gated OAuth provider for MCP.
 *
 * Implements a self-contained OAuth 2.1 flow:
 * - Claude connects -> gets 401 with metadata pointer
 * - Claude discovers /.well-known/oauth-protected-resource
 * - Claude registers via /oauth/register (DCR)
 * - Claude redirects user to /oauth/authorize
 * - User sees a password page, enters MCP_AUTH_TOKEN
 * - Claude exchanges code for access token via /oauth/token
 * - All subsequent requests carry Bearer token
 *
 * No external identity provider needed.
 */

import { randomUUID, randomBytes, createHash, timingSafeEqual } from "node:crypto";
import { readFile, writeFile, mkdir, rename } from "node:fs/promises";
import { dirname } from "node:path";
import type { Hono } from "hono";
import { logger } from "../logging/logger.js";
import { describeError } from "../logging/redact.js";
import {
    RegisteredClientSchema,
    TokenRecordSchema,
    type PendingAuth,
    type RegisteredClient,
    type TokenRecord,
} from "../types/auth.js";

const TOKEN_EXPIRY_MS = 3600 * 1000; // 1 hour
const DAY_MS = 24 * 3600 * 1000;
const DEFAULT_REFRESH_DAYS = 14;
const MAX_FAILED_BEFORE_LOCKOUT = 5;
const BASE_LOCKOUT_MS = 5 * 1000; // 5 seconds, doubles each lockout
const MAX_CLIENTS = 100;
const MAX_PENDING = 100;
const PENDING_TTL_MS = 10 * 60 * 1000; // 10 minutes

const newSecret = () => randomBytes(32).toString("hex");

/**
 * Constant-time string comparison for secrets. Comparing SHA-256 digests gives
 * timingSafeEqual equal-length inputs, so it never throws on a length or
 * encoding mismatch and the secret's length is not revealed.
 */
export function safeEqual(a: string, b: string): boolean {
    const da = createHash("sha256").update(a).digest();
    const db = createHash("sha256").update(b).digest();
    return timingSafeEqual(da, db);
}

export interface AuthOptions {
    /** Refresh-token lifetime in days. */
    refreshDays?: number;
}

export interface AuthHandle {
    validateToken: (auth: string | undefined) => boolean;
    saveTokens: () => Promise<void>;
    loadTokens: () => Promise<boolean>;
    cleanup: () => void;
}

export function mountPasswordAuth(
    app: Hono,
    baseUrl: string,
    password: string,
    persistPath?: string,
    { refreshDays = DEFAULT_REFRESH_DAYS }: AuthOptions = {},
): AuthHandle {
    const refreshExpiryMs = refreshDays * DAY_MS;
    const pendingAuths = new Map<string, PendingAuth>();
    const csrfTokens = new Map<string, string>(); // code -> csrf token
    const tokens = new Map<string, TokenRecord>();
    const refreshTokens = new Map<string, TokenRecord>();
    const clients = new Map<string, RegisteredClient>();

    // Cleanup expired pending auths and CSRF tokens
    function cleanupPending() {
        const now = Date.now();
        for (const [code, pending] of pendingAuths) {
            if (now - pending.createdAt > PENDING_TTL_MS) {
                pendingAuths.delete(code);
                csrfTokens.delete(code);
            }
        }
    }

    // Rate limiting: exponential backoff, never resets until success
    let failedAttempts = 0;
    let lockoutCount = 0;
    let lockedUntil = 0;

    // Persist clients + tokens. Called on every state change (not just the
    // periodic save): a restart or Fly suspend right after registration or
    // token issuance must not lose the new state.
    async function persist(): Promise<void> {
        if (!persistPath) return;
        try {
            await mkdir(dirname(persistPath), { recursive: true });
            const now = Date.now();
            const activeTokens = [...tokens.entries()].filter(([, r]) => r.expiresAt > now);
            const activeRefresh = [...refreshTokens.entries()].filter(
                ([, r]) => r.refreshExpiresAt > now,
            );
            const data = JSON.stringify({
                tokens: Object.fromEntries(activeTokens),
                refreshTokens: Object.fromEntries(activeRefresh),
                clients: Object.fromEntries(clients),
            });
            // Write-then-rename so a crash mid-write never leaves truncated JSON,
            // which loadTokens would discard along with every client registration.
            const tmpPath = `${persistPath}.${process.pid}.${randomUUID()}.tmp`;
            await writeFile(tmpPath, data, { encoding: "utf-8", mode: 0o600 });
            await rename(tmpPath, persistPath);
        } catch (err) {
            logger.error(
                `Failed to save auth tokens: ${describeError(err, logger.isLevelEnabled("debug"))}`,
            );
        }
    }

    // HTTPS warning
    if (!baseUrl.startsWith("https://") && !baseUrl.includes("localhost")) {
        logger.warn(
            "WARNING: BASE_URL is not HTTPS. OAuth tokens will be sent in cleartext. Use a tunnel (cloudflared, tailscale, ngrok) to provide TLS.",
        );
    }

    // --- Discovery endpoints ---

    app.get("/.well-known/oauth-protected-resource", (c) => {
        return c.json({
            resource: baseUrl,
            authorization_servers: [baseUrl],
            scopes_supported: ["mcp"],
        });
    });

    app.get("/.well-known/oauth-authorization-server", (c) => {
        return c.json({
            issuer: baseUrl,
            authorization_endpoint: `${baseUrl}/oauth/authorize`,
            token_endpoint: `${baseUrl}/oauth/token`,
            registration_endpoint: `${baseUrl}/oauth/register`,
            response_types_supported: ["code"],
            grant_types_supported: ["authorization_code", "refresh_token"],
            code_challenge_methods_supported: ["S256"],
            token_endpoint_auth_methods_supported: ["client_secret_post", "none"],
            scopes_supported: ["mcp"],
        });
    });

    // --- Dynamic Client Registration (RFC 7591) ---

    app.post("/oauth/register", async (c) => {
        if (clients.size >= MAX_CLIENTS) {
            // Evict the oldest client with no live tokens to make room; only
            // reject when every slot is held by a client with active tokens.
            const activeClientIds = new Set<string>();
            for (const r of tokens.values()) activeClientIds.add(r.clientId);
            for (const r of refreshTokens.values()) activeClientIds.add(r.clientId);
            let oldest: RegisteredClient | undefined;
            for (const client of clients.values()) {
                if (activeClientIds.has(client.clientId)) continue;
                if (!oldest || (client.createdAt ?? 0) < (oldest.createdAt ?? 0)) oldest = client;
            }
            if (!oldest) {
                return c.json({ error: "too_many_clients" }, 429);
            }
            clients.delete(oldest.clientId);
        }
        const body: unknown = await c.req.json().catch(() => null);
        if (!body || typeof body !== "object") {
            return c.json(
                { error: "invalid_client_metadata", error_description: "JSON body required" },
                400,
            );
        }
        const metadata = body as Record<string, unknown>;

        // Validate redirect_uris
        const redirectUris: unknown[] = Array.isArray(metadata.redirect_uris)
            ? metadata.redirect_uris.slice(0, 5)
            : [];
        if (redirectUris.length === 0) {
            return c.json(
                { error: "invalid_client_metadata", error_description: "redirect_uris required" },
                400,
            );
        }
        const safeUri = (u: unknown): u is string => {
            if (typeof u !== "string" || u.length > 2048) return false;
            const lower = u.toLowerCase();
            return (
                !lower.startsWith("javascript:") &&
                !lower.startsWith("data:") &&
                !lower.startsWith("file:")
            );
        };
        if (!redirectUris.every(safeUri)) {
            return c.json(
                { error: "invalid_client_metadata", error_description: "invalid redirect_uri" },
                400,
            );
        }

        const clientId = randomUUID();
        // Honor the client's requested auth method (RFC 7591 §2). Only
        // "client_secret_post" and "none" are advertised as supported, so
        // anything else falls back to the confidential-client default rather
        // than silently registering a method we don't understand.
        const tokenEndpointAuthMethod: "client_secret_post" | "none" =
            metadata.token_endpoint_auth_method === "none" ? "none" : "client_secret_post";
        const clientSecret = tokenEndpointAuthMethod === "none" ? undefined : newSecret();
        const clientName =
            typeof metadata.client_name === "string"
                ? metadata.client_name.slice(0, 256)
                : undefined;

        const client: RegisteredClient = {
            clientId,
            clientSecret,
            tokenEndpointAuthMethod,
            redirectUris: redirectUris as string[],
            clientName,
            createdAt: Date.now(),
        };
        clients.set(clientId, client);
        await persist();

        logger.info(
            `Auth: registered client_id=${clientId} redirect_uris=${JSON.stringify(redirectUris)} ` +
                `requested_auth_method=${JSON.stringify(metadata.token_endpoint_auth_method ?? "(unspecified)")} (responding with ${tokenEndpointAuthMethod})`,
        );

        return c.json(
            {
                client_id: clientId,
                ...(clientSecret ? { client_secret: clientSecret } : {}),
                redirect_uris: client.redirectUris,
                client_name: client.clientName,
                token_endpoint_auth_method: tokenEndpointAuthMethod,
            },
            201,
        );
    });

    // --- Authorization endpoint ---

    app.get("/oauth/authorize", (c) => {
        const clientId = c.req.query("client_id") ?? "";
        const redirectUri = c.req.query("redirect_uri") ?? "";
        const codeChallenge = c.req.query("code_challenge") ?? "";
        const codeChallengeMethod = c.req.query("code_challenge_method") ?? "S256";
        const state = c.req.query("state") ?? "";

        // Validate redirect URI against registered client
        const client = clients.get(clientId);
        if (!client) {
            logger.warn(`Auth: /oauth/authorize unknown client_id=${JSON.stringify(clientId)}`);
            return c.text("Unknown client", 400);
        }
        if (!client.redirectUris.includes(redirectUri)) {
            logger.warn(
                `Auth: /oauth/authorize redirect_uri mismatch. received=${JSON.stringify(redirectUri)} ` +
                    `registered=${JSON.stringify(client.redirectUris)}`,
            );
            return c.text("Invalid redirect URI", 400);
        }

        // Require S256 PKCE
        if (codeChallengeMethod !== "S256" || !codeChallenge) {
            logger.warn(
                `Auth: /oauth/authorize missing/unsupported PKCE. method=${JSON.stringify(codeChallengeMethod)} challenge_present=${!!codeChallenge}`,
            );
            return c.text("PKCE with S256 is required", 400);
        }

        cleanupPending();
        if (pendingAuths.size >= MAX_PENDING) {
            return c.text("Too many pending authorizations", 429);
        }

        const code = newSecret();
        pendingAuths.set(code, {
            clientId,
            redirectUri,
            codeChallenge,
            codeChallengeMethod,
            state,
            code,
            createdAt: Date.now(),
            approved: false,
        });

        logger.info(
            `Auth: /oauth/authorize accepted client_id=${clientId} redirect_uri=${JSON.stringify(redirectUri)}`,
        );

        const csrf = newSecret();
        csrfTokens.set(code, csrf);
        return c.html(renderPasswordPage(code, csrf, hostOf(redirectUri), client.clientName));
    });

    // --- Approval handler ---

    app.post("/oauth/approve", async (c) => {
        const body = await c.req.parseBody();
        const code = body["code"] as string;
        const submittedCsrf = body["csrf"] as string;
        const submittedPassword = body["password"] as string;

        const pending = pendingAuths.get(code);
        const expectedCsrf = csrfTokens.get(code);
        if (!pending || !expectedCsrf) {
            return c.html("<p>Invalid or expired authorization request.</p>", 400);
        }

        // Validate CSRF token
        if (typeof submittedCsrf !== "string" || !safeEqual(submittedCsrf, expectedCsrf)) {
            return c.html("<p>Invalid request.</p>", 403);
        }

        // Re-render the password form with a fresh CSRF token.
        const rerender = (message: string, status: 401 | 429) => {
            const csrf = newSecret();
            csrfTokens.set(code, csrf);
            return c.html(
                renderPasswordPage(
                    code,
                    csrf,
                    hostOf(pending.redirectUri),
                    clients.get(pending.clientId)?.clientName,
                    message,
                ),
                status,
            );
        };

        // Rate limiting: check lockout
        if (Date.now() < lockedUntil) {
            const waitSec = Math.ceil((lockedUntil - Date.now()) / 1000);
            logger.warn(`Auth: locked out, ${waitSec}s remaining`);
            return rerender(`Too many attempts. Try again in ${waitSec} seconds.`, 429);
        }

        if (typeof submittedPassword !== "string" || !safeEqual(submittedPassword, password)) {
            failedAttempts++;
            logger.warn(`Auth: failed attempt ${failedAttempts} total`);

            // rerender rotates the CSRF token on each failed attempt.
            if (failedAttempts >= MAX_FAILED_BEFORE_LOCKOUT) {
                lockoutCount = Math.min(lockoutCount + 1, 10);
                const lockoutMs = BASE_LOCKOUT_MS * Math.pow(2, lockoutCount - 1);
                lockedUntil = Date.now() + lockoutMs;
                logger.warn(`Auth: lockout #${lockoutCount}, ${lockoutMs / 1000}s`);
                return rerender(
                    `Too many attempts. Try again in ${Math.ceil(lockoutMs / 1000)} seconds.`,
                    429,
                );
            }

            return rerender("Wrong password.", 401);
        }

        // Password correct — reset everything
        failedAttempts = 0;
        lockoutCount = 0;
        lockedUntil = 0;
        csrfTokens.delete(code);
        // Mark the code redeemable only now that the password has been verified.
        // Without this, /oauth/token would accept the code straight out of the
        // authorize page before any password was entered.
        pending.approved = true;
        logger.info("Auth: password accepted, issuing authorization code.");

        const url = new URL(pending.redirectUri);
        url.searchParams.set("code", code);
        if (pending.state) url.searchParams.set("state", pending.state);
        const redirectUrl = url.toString();

        return c.redirect(redirectUrl);
    });

    // --- Token endpoint ---

    function authenticateClient(clientId: string, secret: unknown): boolean {
        const client = clients.get(clientId);
        if (!client) return false;
        if (client.tokenEndpointAuthMethod === "none") return true;
        // Older persisted registrations have a secret but no method field.
        return (
            typeof secret === "string" &&
            typeof client.clientSecret === "string" &&
            safeEqual(secret, client.clientSecret)
        );
    }

    /**
     * Client credentials from HTTP Basic (client_secret_basic, RFC 6749 §2.3.1)
     * or the request body (client_secret_post). Returns null for a malformed
     * Basic header or one that disagrees with credentials in the body.
     */
    function clientCredentials(
        authorization: string | undefined,
        body: Record<string, unknown>,
    ): { clientId: unknown; secret: unknown } | null {
        const bodyCredentials = { clientId: body["client_id"], secret: body["client_secret"] };
        const match = authorization?.match(/^Basic\s+(\S+)$/i);
        if (!match) return bodyCredentials;
        const decoded = Buffer.from(match[1], "base64").toString("utf-8");
        const colon = decoded.indexOf(":");
        if (colon < 0) return null;
        let clientId: string, secret: string;
        try {
            clientId = decodeURIComponent(decoded.slice(0, colon).replace(/\+/g, " "));
            secret = decodeURIComponent(decoded.slice(colon + 1).replace(/\+/g, " "));
        } catch {
            return null;
        }
        if (bodyCredentials.clientId !== undefined && bodyCredentials.clientId !== clientId)
            return null;
        if (bodyCredentials.secret !== undefined && bodyCredentials.secret !== secret) return null;
        return { clientId, secret };
    }

    async function issueTokens(clientId: string, refreshExpiresAt: number) {
        const record: TokenRecord = {
            accessToken: newSecret(),
            refreshToken: newSecret(),
            clientId,
            expiresAt: Date.now() + TOKEN_EXPIRY_MS,
            refreshExpiresAt,
        };
        tokens.set(record.accessToken, record);
        refreshTokens.set(record.refreshToken, record);
        await persist();
        return {
            access_token: record.accessToken,
            token_type: "Bearer",
            expires_in: TOKEN_EXPIRY_MS / 1000,
            refresh_token: record.refreshToken,
        };
    }

    app.post("/oauth/token", async (c) => {
        const body = await c.req.parseBody();
        const grantType = body["grant_type"] as string;
        logger.info(`Auth: /oauth/token request grant_type=${JSON.stringify(grantType)}`);
        const credentials = clientCredentials(c.req.header("Authorization"), body);
        if (!credentials) {
            logger.warn(
                "Auth: /oauth/token invalid_client. malformed or conflicting client credentials",
            );
            return c.json({ error: "invalid_client" }, 401);
        }

        if (grantType === "authorization_code") {
            const code = body["code"] as string;
            const clientId = credentials.clientId as string;
            const codeVerifier = body["code_verifier"];
            const redirectUri = body["redirect_uri"] as string;

            const pending = pendingAuths.get(code);
            if (!pending || Date.now() - pending.createdAt > PENDING_TTL_MS) {
                logger.warn(
                    `Auth: /oauth/token invalid_grant. code_known=${!!pending} ` +
                        `expired=${pending ? Date.now() - pending.createdAt > PENDING_TTL_MS : "n/a"}`,
                );
                if (pending) pendingAuths.delete(code);
                return c.json({ error: "invalid_grant" }, 400);
            }

            // The code is only redeemable once the password step succeeded.
            // It exists in pendingAuths from /oauth/authorize onward (and is
            // present in the authorize page HTML), so without this check the
            // password gate can be bypassed entirely.
            if (!pending.approved) {
                logger.warn(
                    "Auth: /oauth/token invalid_grant. code not yet approved (password step not completed)",
                );
                return c.json(
                    { error: "invalid_grant", error_description: "authorization not approved" },
                    400,
                );
            }

            // Verify client_id matches the original request
            if (clientId !== pending.clientId) {
                logger.warn(
                    `Auth: /oauth/token client_id mismatch. received=${JSON.stringify(clientId)} expected=${JSON.stringify(pending.clientId)}`,
                );
                return c.json(
                    { error: "invalid_grant", error_description: "client_id mismatch" },
                    400,
                );
            }

            if (!authenticateClient(clientId, credentials.secret)) {
                return c.json({ error: "invalid_client" }, 401);
            }

            // The authenticated client gets one redemption attempt (OAuth 2.1
            // §4.1.3): consume the code now so a failed redirect_uri or PKCE
            // check cannot be retried. A failed client authentication above
            // leaves the code intact.
            pendingAuths.delete(code);

            // Verify redirect_uri matches the original request
            if (redirectUri !== pending.redirectUri) {
                logger.warn(
                    `Auth: /oauth/token redirect_uri mismatch. received=${JSON.stringify(redirectUri)} expected=${JSON.stringify(pending.redirectUri)}`,
                );
                return c.json(
                    { error: "invalid_grant", error_description: "redirect_uri mismatch" },
                    400,
                );
            }

            // Verify PKCE (/oauth/authorize only accepts S256)
            if (
                typeof codeVerifier !== "string" ||
                !safeEqual(
                    createHash("sha256").update(codeVerifier).digest("base64url"),
                    pending.codeChallenge,
                )
            ) {
                logger.warn("Auth: /oauth/token PKCE verification failed");
                return c.json(
                    { error: "invalid_grant", error_description: "PKCE verification failed" },
                    400,
                );
            }

            logger.info(`Auth: /oauth/token issuing access token client_id=${pending.clientId}`);
            return c.json(await issueTokens(pending.clientId, Date.now() + refreshExpiryMs));
        }

        if (grantType === "refresh_token") {
            const refreshToken = body["refresh_token"] as string;
            const clientId = credentials.clientId as string;
            const old = refreshTokens.get(refreshToken);
            if (!old) {
                logger.warn("Auth: /oauth/token refresh_token unknown");
                return c.json({ error: "invalid_grant" }, 400);
            }

            if (!authenticateClient(clientId, credentials.secret)) {
                return c.json({ error: "invalid_client" }, 401);
            }
            if (clientId !== old.clientId) {
                return c.json(
                    { error: "invalid_grant", error_description: "client_id mismatch" },
                    400,
                );
            }

            // Check refresh token expiry
            if (Date.now() > old.refreshExpiresAt) {
                tokens.delete(old.accessToken);
                refreshTokens.delete(refreshToken);
                logger.info("Auth: refresh token expired, user must re-authenticate.");
                return c.json(
                    { error: "invalid_grant", error_description: "Refresh token expired" },
                    400,
                );
            }

            tokens.delete(old.accessToken);
            refreshTokens.delete(refreshToken);
            // Rotation keeps the original refresh expiry.
            return c.json(await issueTokens(old.clientId, old.refreshExpiresAt));
        }

        logger.warn(`Auth: /oauth/token unsupported_grant_type=${JSON.stringify(grantType)}`);
        return c.json({ error: "unsupported_grant_type" }, 400);
    });

    return {
        validateToken(authHeader: string | undefined): boolean {
            // RFC 6750: the auth scheme is case-insensitive.
            const token = authHeader?.match(/^Bearer\s+(\S+)$/i)?.[1];
            if (!token) return false;
            const record = tokens.get(token);
            if (!record) return false;
            if (Date.now() > record.expiresAt) {
                tokens.delete(token);
                return false;
            }
            return true;
        },

        async saveTokens(): Promise<void> {
            await persist();
        },

        cleanup(): void {
            cleanupPending();
            const now = Date.now();
            for (const [k, r] of tokens) {
                if (r.expiresAt <= now) tokens.delete(k);
            }
            for (const [k, r] of refreshTokens) {
                if (r.refreshExpiresAt <= now) refreshTokens.delete(k);
            }
            // Registered clients are NOT evicted here: AI clients cache their
            // client_id indefinitely and retry it after token expiry, so
            // dropping a registration means "Unknown client" until the user
            // deletes and re-adds the connector (issue #13). The clients map
            // is bounded at registration time instead.
        },

        async loadTokens(): Promise<boolean> {
            if (!persistPath) return false;
            try {
                const raw = await readFile(persistPath, "utf-8");
                const data = JSON.parse(raw) as Partial<Record<string, Record<string, unknown>>>;
                const now = Date.now();
                for (const [k, v] of Object.entries(data.tokens ?? {})) {
                    const record = TokenRecordSchema.safeParse(v);
                    if (record.success && record.data.expiresAt > now) tokens.set(k, record.data);
                }
                for (const [k, v] of Object.entries(data.refreshTokens ?? {})) {
                    const record = TokenRecordSchema.safeParse(v);
                    if (record.success && record.data.refreshExpiresAt > now)
                        refreshTokens.set(k, record.data);
                }
                for (const [k, v] of Object.entries(data.clients ?? {})) {
                    const client = RegisteredClientSchema.safeParse(v);
                    if (client.success) clients.set(k, client.data);
                }
                logger.info(`Auth tokens loaded from disk (${tokens.size} sessions).`);
                return tokens.size > 0;
            } catch (err) {
                if ((err as NodeJS.ErrnoException).code !== "ENOENT") {
                    logger.warn(
                        `Failed to load auth tokens; starting with none: ${describeError(err, logger.isLevelEnabled("debug"))}`,
                    );
                }
                return false;
            }
        },
    };
}

function hostOf(uri: string): string {
    try {
        return new URL(uri).host;
    } catch {
        return uri;
    }
}

const esc = (s: string) =>
    s
        .replaceAll("&", "&amp;")
        .replaceAll("<", "&lt;")
        .replaceAll(">", "&gt;")
        .replaceAll('"', "&quot;");

function renderPasswordPage(
    code: string,
    csrf: string,
    redirectHost: string,
    clientName: string | undefined,
    error?: string,
): string {
    const who = clientName ? `<b>${esc(clientName)}</b> (name self-reported)` : "An application";
    return `<!DOCTYPE html>
<html lang="en"><head><title>Obsidian CRUD MCP - Authorize</title>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">
<style>
  :root {
    color-scheme: light dark;
    --paper: #f6f5f8;
    --card: #fdfcfe;
    --ink: #272330;
    --muted: #665f73;
    --line: #e6e1ed;
    --inset: #f2eff6;
    --accent: #6941c6;
    --accent-hover: #5935ad;
    --focus: #9875e0;
    --warning: #785217;
    --warning-bg: #f8f1e3;
    --error: #a12d43;
    --error-bg: #fbeef1;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; min-height: 100vh; min-height: 100svh;
    display: grid; place-items: center; padding: 32px 20px;
    background: var(--paper); color: var(--ink);
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    font-size: 15px; line-height: 1.6;
  }
  main { width: 100%; max-width: 440px; }
  .brand { display: flex; align-items: center; gap: 12px; margin-bottom: 24px; font-size: 14px; font-weight: 650; }
  .mark { width: 32px; height: 36px; background: var(--accent); clip-path: polygon(50% 0, 94% 28%, 78% 85%, 38% 100%, 6% 58%, 18% 20%); }
  .card { padding: 32px; border: 1px solid var(--line); border-radius: 20px; background: var(--card); }
  .eyebrow { margin: 0 0 8px; color: var(--accent); font-size: 11px; font-weight: 700; letter-spacing: .12em; text-transform: uppercase; }
  h1 { margin: 0 0 12px; font-size: 28px; line-height: 1.2; letter-spacing: -.035em; font-weight: 650; }
  .intro { margin: 0 0 24px; color: var(--muted); }
  b { color: var(--ink); font-weight: 650; }
  .dest { margin-bottom: 16px; padding: 12px 16px; border: 1px solid var(--line); border-radius: 10px; background: var(--inset); }
  .dest span { display: block; margin-bottom: 4px; color: var(--muted); font-size: 12px; }
  .dest b { font-family: ui-monospace, SFMono-Regular, Consolas, monospace; font-size: 14px; overflow-wrap: anywhere; }
  .warn { margin: 0 0 24px; padding: 12px 16px; border-radius: 10px; background: var(--warning-bg); color: var(--warning); font-size: 13px; line-height: 1.5; }
  .error { margin: 0 0 20px; padding: 12px 16px; border-radius: 10px; background: var(--error-bg); color: var(--error); font-size: 13px; }
  label { display: block; margin-bottom: 8px; font-size: 13px; font-weight: 650; }
  input[type=password] {
    display: block; width: 100%; min-height: 48px; padding: 12px 14px;
    border: 1px solid var(--line); border-radius: 10px;
    background: var(--inset); color: var(--ink); font: inherit;
  }
  input::placeholder { color: var(--muted); opacity: 1; }
  input[aria-invalid=true] { border-color: var(--error); }
  input:focus-visible, button:focus-visible { outline: 3px solid var(--focus); outline-offset: 3px; }
  button {
    width: 100%; min-height: 48px; margin-top: 16px; padding: 12px 20px;
    border: 0; border-radius: 10px; background: var(--accent); color: #fff;
    font: inherit; font-weight: 650; cursor: pointer;
  }
  button:hover { background: var(--accent-hover); }
  button:active { transform: translateY(1px); }
  .footnote { margin: 20px 0 0; text-align: center; color: var(--muted); font-size: 12px; }
  .username { position: absolute; opacity: 0; width: 1px; height: 1px; pointer-events: none; }
  @media (max-width: 480px) { body { padding: 24px 16px; } .card { padding: 24px; } }
  @media (prefers-color-scheme: dark) {
    :root {
      --paper: #18161d; --card: #201d27; --ink: #eeeaf5;
      --muted: #b1a9bf; --line: #38313f; --inset: #1b1822;
      --accent: #a384ee; --accent-hover: #b397f4; --focus: #b397f4;
      --warning: #e1c087; --warning-bg: #30291e;
      --error: #f3a0b0; --error-bg: #35212a;
    }
    button { color: #211638; }
  }
</style></head>
<body>
  <main>
    <div class="brand"><span class="mark" aria-hidden="true"></span>Obsidian CRUD MCP</div>
    <section class="card" aria-labelledby="page-title">
      <p class="eyebrow">Vault access</p>
      <h1 id="page-title">Connect your vault</h1>
      <p class="intro">${who} is requesting access to your vault. After you approve, your browser will be sent to:</p>
      <div class="dest"><span>Return destination</span><b>${esc(redirectHost)}</b></div>
      <p class="warn" id="destination-warning">Only enter your password if you recognize this destination. If you did not start this sign-in, close this page.</p>
      ${error ? `<p class="error" id="password-error" role="alert">${esc(error)}</p>` : ""}
      <form method="POST" action="/oauth/approve" autocomplete="on">
        <input type="hidden" name="code" value="${esc(code)}">
        <input type="hidden" name="csrf" value="${esc(csrf)}">
        <input type="text" name="username" id="username" value="obsidian-crud-mcp" autocomplete="username" class="username" tabindex="-1" aria-hidden="true">
        <label for="password">Vault password</label>
        <input type="password" name="password" id="password" placeholder="Enter your password" autocomplete="current-password" aria-describedby="destination-warning${error ? " password-error" : ""}"${error ? ' aria-invalid="true"' : ""} autofocus required>
        <button type="submit">Authorize</button>
      </form>
    </section>
    <p class="footnote">Your notes. Your vault. Your approval.</p>
  </main>
</body></html>`;
}
