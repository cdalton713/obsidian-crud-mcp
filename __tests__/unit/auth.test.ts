import { describe, it } from "vitest";
import assert from "node:assert/strict";
import { Hono } from "hono";
import { createHash, randomBytes } from "node:crypto";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { mountPasswordAuth, safeEqual } from "../../src/auth/auth.js";

function setup(password = "test-password") {
    const app = new Hono();
    const baseUrl = "https://example.com";
    const auth = mountPasswordAuth(app, baseUrl, password);
    return { app, baseUrl, validateToken: auth.validateToken };
}

function generatePKCE() {
    const verifier = randomBytes(32).toString("base64url");
    const challenge = createHash("sha256").update(verifier).digest("base64url");
    return { verifier, challenge };
}

async function registerClient(app: Hono, redirectUri = "https://app.example.com/callback") {
    const resp = await app.request("/oauth/register", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ client_name: "test", redirect_uris: [redirectUri] }),
    });
    return (await resp.json()) as { client_id: string; client_secret: string };
}

function extractHiddenFields(html: string): Record<string, string> {
    const fields: Record<string, string> = {};
    const re = /name="(\w+)"\s+value="([^"]*)"/g;
    let m;
    while ((m = re.exec(html)) !== null) {
        fields[m[1]] = m[2];
    }
    return fields;
}

async function getAuthorizePage(
    app: Hono,
    clientId: string,
    challenge: string,
    redirectUri = "https://app.example.com/callback",
) {
    const params = new URLSearchParams({
        client_id: clientId,
        redirect_uri: redirectUri,
        code_challenge: challenge,
        code_challenge_method: "S256",
        state: "test-state",
        response_type: "code",
    });
    const resp = await app.request(`/oauth/authorize?${params}`);
    const html = await resp.text();
    return { resp, html, fields: extractHiddenFields(html) };
}

async function submitPassword(app: Hono, code: string, csrf: string, password: string) {
    return app.request("/oauth/approve", {
        method: "POST",
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams({ code, csrf, password }).toString(),
    });
}

async function completeOAuthFlow(app: Hono, password: string) {
    const pkce = generatePKCE();
    const client = await registerClient(app);
    const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
    const approveResp = await submitPassword(app, fields.code, fields.csrf, password);
    assert.equal(approveResp.status, 302, "approve should redirect");
    const location = approveResp.headers.get("location")!;
    const authCode = new URL(location).searchParams.get("code")!;

    const tokenResp = await app.request("/oauth/token", {
        method: "POST",
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams({
            grant_type: "authorization_code",
            code: authCode,
            client_id: client.client_id,
            client_secret: client.client_secret,
            code_verifier: pkce.verifier,
            redirect_uri: "https://app.example.com/callback",
        }).toString(),
    });
    assert.equal(tokenResp.status, 200);
    return { ...((await tokenResp.json()) as object), client } as {
        client: typeof client;
        access_token: string;
        refresh_token: string;
        expires_in: number;
    };
}

// --- Tests ---

describe("OAuth Discovery", () => {
    it("serves protected resource metadata", async () => {
        const { app, baseUrl } = setup();
        const resp = await app.request("/.well-known/oauth-protected-resource");
        const body = (await resp.json()) as any;
        assert.equal(body.resource, baseUrl);
        assert.deepEqual(body.authorization_servers, [baseUrl]);
    });

    it("serves authorization server metadata with S256", async () => {
        const { app, baseUrl } = setup();
        const resp = await app.request("/.well-known/oauth-authorization-server");
        const body = (await resp.json()) as any;
        assert.deepEqual(body.code_challenge_methods_supported, ["S256"]);
        assert.equal(body.token_endpoint, `${baseUrl}/oauth/token`);
        assert.equal(body.registration_endpoint, `${baseUrl}/oauth/register`);
    });
});

describe("Dynamic Client Registration", () => {
    it("returns 201 with client_id and client_secret", async () => {
        const { app } = setup();
        const resp = await app.request("/oauth/register", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ client_name: "test", redirect_uris: ["https://x.com/cb"] }),
        });
        assert.equal(resp.status, 201);
        const body = (await resp.json()) as any;
        assert.ok(body.client_id);
        assert.ok(body.client_secret);
        assert.deepEqual(body.redirect_uris, ["https://x.com/cb"]);
        assert.equal(body.token_endpoint_auth_method, "client_secret_post");
    });

    it("honors token_endpoint_auth_method: none — no client_secret issued", async () => {
        const { app } = setup();
        const resp = await app.request("/oauth/register", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({
                client_name: "test",
                redirect_uris: ["https://x.com/cb"],
                token_endpoint_auth_method: "none",
            }),
        });
        assert.equal(resp.status, 201);
        const body = (await resp.json()) as any;
        assert.equal(body.token_endpoint_auth_method, "none");
        assert.equal(body.client_secret, undefined);
    });

    it("falls back to client_secret_post for an unrecognized auth method", async () => {
        const { app } = setup();
        const resp = await app.request("/oauth/register", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({
                client_name: "test",
                redirect_uris: ["https://x.com/cb"],
                token_endpoint_auth_method: "client_secret_basic",
            }),
        });
        assert.equal(resp.status, 201);
        const body = (await resp.json()) as any;
        assert.equal(body.token_endpoint_auth_method, "client_secret_post");
        assert.ok(body.client_secret);
    });
});

describe("/oauth/authorize", () => {
    it("rejects unknown client_id", async () => {
        const { app } = setup();
        const pkce = generatePKCE();
        const params = new URLSearchParams({
            client_id: "unknown",
            redirect_uri: "https://x.com/cb",
            code_challenge: pkce.challenge,
            code_challenge_method: "S256",
        });
        const resp = await app.request(`/oauth/authorize?${params}`);
        assert.equal(resp.status, 400);
        assert.ok((await resp.text()).includes("Unknown client"));
    });

    it("rejects unregistered redirect_uri", async () => {
        const { app } = setup();
        const client = await registerClient(app, "https://legit.com/cb");
        const pkce = generatePKCE();
        const params = new URLSearchParams({
            client_id: client.client_id,
            redirect_uri: "https://evil.com/steal",
            code_challenge: pkce.challenge,
            code_challenge_method: "S256",
        });
        const resp = await app.request(`/oauth/authorize?${params}`);
        assert.equal(resp.status, 400);
        assert.ok((await resp.text()).includes("Invalid redirect URI"));
    });

    it("rejects missing code_challenge", async () => {
        const { app } = setup();
        const client = await registerClient(app);
        const params = new URLSearchParams({
            client_id: client.client_id,
            redirect_uri: "https://app.example.com/callback",
            code_challenge_method: "S256",
        });
        const resp = await app.request(`/oauth/authorize?${params}`);
        assert.equal(resp.status, 400);
        assert.ok((await resp.text()).includes("PKCE"));
    });

    it("rejects code_challenge_method other than S256", async () => {
        const { app } = setup();
        const client = await registerClient(app);
        const params = new URLSearchParams({
            client_id: client.client_id,
            redirect_uri: "https://app.example.com/callback",
            code_challenge: "test",
            code_challenge_method: "plain",
        });
        const resp = await app.request(`/oauth/authorize?${params}`);
        assert.equal(resp.status, 400);
    });

    it("returns HTML form with code and csrf fields", async () => {
        const { app } = setup();
        const client = await registerClient(app);
        const pkce = generatePKCE();
        const { resp, fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
        assert.equal(resp.status, 200);
        assert.ok(fields.code);
        assert.ok(fields.csrf);
    });

    it("shows the redirect host on the consent page (phishing defense, GHSA-49hr-4pv9-75q6)", async () => {
        const { app } = setup();
        const pkce = generatePKCE();
        const client = await registerClient(app, "https://evil.example/cb");
        const { html } = await getAuthorizePage(
            app,
            client.client_id,
            pkce.challenge,
            "https://evil.example/cb",
        );
        assert.ok(html.includes("evil.example"), "consent page must display the redirect host");
    });
});

describe("/oauth/approve — password validation", () => {
    it("rejects invalid code", async () => {
        const { app } = setup();
        const resp = await submitPassword(app, "bad-code", "bad-csrf", "test-password");
        assert.equal(resp.status, 400);
    });

    it("rejects wrong CSRF token", async () => {
        const { app } = setup();
        const client = await registerClient(app);
        const pkce = generatePKCE();
        const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
        const resp = await submitPassword(app, fields.code, "wrong-csrf", "test-password");
        assert.equal(resp.status, 403);
    });

    it("rejects wrong password", async () => {
        const { app } = setup();
        const client = await registerClient(app);
        const pkce = generatePKCE();
        const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
        const resp = await submitPassword(app, fields.code, fields.csrf, "wrong");
        assert.equal(resp.status, 401);
        assert.ok((await resp.text()).includes("Wrong password"));
    });

    // Regression guard: this path compared byte lengths and never threw; it keeps
    // safeEqual from changing that. The bug fixed on this path is the missing field below.
    it("rejects a same-length non-ASCII password with 401", async () => {
        const { app } = setup("test-password");
        const client = await registerClient(app);
        const pkce = generatePKCE();
        const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
        const resp = await submitPassword(app, fields.code, fields.csrf, "test-passworé");
        assert.equal(resp.status, 401);
        assert.ok((await resp.text()).includes("Wrong password"));
    });

    it("rejects a missing password field with 401, not 500", async () => {
        const { app } = setup();
        const client = await registerClient(app);
        const pkce = generatePKCE();
        const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
        const resp = await app.request("/oauth/approve", {
            method: "POST",
            headers: { "Content-Type": "application/x-www-form-urlencoded" },
            body: new URLSearchParams({ code: fields.code, csrf: fields.csrf }).toString(),
        });
        assert.equal(resp.status, 401);
    });

    it("redirects with code and state on correct password", async () => {
        const { app } = setup();
        const client = await registerClient(app);
        const pkce = generatePKCE();
        const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
        const resp = await submitPassword(app, fields.code, fields.csrf, "test-password");
        assert.equal(resp.status, 302);
        const location = resp.headers.get("location")!;
        assert.ok(location.includes("code="));
        assert.ok(location.includes("state=test-state"));
    });
});

describe("/oauth/approve — rate limiting", () => {
    it("locks out after 5 failed attempts", async () => {
        const { app } = setup();
        const client = await registerClient(app);
        const pkce = generatePKCE();

        for (let i = 0; i < 5; i++) {
            const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
            const resp = await submitPassword(app, fields.code, fields.csrf, "wrong");
            if (i < 4) {
                assert.equal(resp.status, 401, `attempt ${i + 1} should be 401`);
            } else {
                assert.equal(resp.status, 429, `attempt ${i + 1} should trigger lockout`);
                assert.ok((await resp.text()).includes("Too many attempts"));
            }
        }
    });

    it("resets counters on successful login", async () => {
        const { app } = setup("mypass");
        const client = await registerClient(app);
        const pkce = generatePKCE();

        // Fail 4 times (just under lockout)
        for (let i = 0; i < 4; i++) {
            const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
            await submitPassword(app, fields.code, fields.csrf, "wrong");
        }

        // Succeed
        const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
        const resp = await submitPassword(app, fields.code, fields.csrf, "mypass");
        assert.equal(resp.status, 302);

        // Fail 4 more times — should NOT lock out (counter was reset)
        for (let i = 0; i < 4; i++) {
            const { fields: f } = await getAuthorizePage(app, client.client_id, pkce.challenge);
            const r = await submitPassword(app, f.code, f.csrf, "wrong");
            assert.equal(r.status, 401, `post-reset attempt ${i + 1} should be 401, not 429`);
        }
    });
});

describe("Token Exchange", () => {
    it("requires the registered secret without consuming the authorization code", async () => {
        const { app } = setup();
        const client = await registerClient(app);
        const pkce = generatePKCE();
        const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
        await submitPassword(app, fields.code, fields.csrf, "test-password");
        const params = {
            grant_type: "authorization_code",
            code: fields.code,
            client_id: client.client_id,
            code_verifier: pkce.verifier,
            redirect_uri: "https://app.example.com/callback",
        };
        const invalidCredentials: Record<string, string>[] = [
            {},
            { client_secret: "wrong-secret" },
        ];
        for (const credentials of invalidCredentials) {
            const response = await app.request("/oauth/token", {
                method: "POST",
                body: new URLSearchParams({ ...params, ...credentials }),
            });
            assert.equal(response.status, 401);
            assert.equal(((await response.json()) as any).error, "invalid_client");
        }
        const response = await app.request("/oauth/token", {
            method: "POST",
            body: new URLSearchParams({ ...params, client_secret: client.client_secret }),
        });
        assert.equal(response.status, 200);
        assert.ok(((await response.json()) as any).access_token);
    });

    it("issues tokens with correct PKCE", async () => {
        const { app } = setup();
        const tokens = await completeOAuthFlow(app, "test-password");
        assert.ok(tokens.access_token);
        assert.ok(tokens.refresh_token);
        assert.equal(tokens.expires_in, 3600);
    });

    it("rejects incorrect PKCE verifier", async () => {
        const { app } = setup();
        const pkce = generatePKCE();
        const client = await registerClient(app);
        const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
        const approveResp = await submitPassword(app, fields.code, fields.csrf, "test-password");
        const location = approveResp.headers.get("location")!;
        const authCode = new URL(location).searchParams.get("code")!;

        const tokenResp = await app.request("/oauth/token", {
            method: "POST",
            headers: { "Content-Type": "application/x-www-form-urlencoded" },
            body: new URLSearchParams({
                grant_type: "authorization_code",
                code: authCode,
                client_id: client.client_id,
                client_secret: client.client_secret,
                code_verifier: "wrong-verifier",
                redirect_uri: "https://app.example.com/callback",
            }).toString(),
        });
        assert.equal(tokenResp.status, 400);
        const body = (await tokenResp.json()) as any;
        assert.equal(body.error, "invalid_grant");
    });

    it("rejects wrong client_id at token exchange", async () => {
        const { app } = setup();
        const pkce = generatePKCE();
        const client = await registerClient(app);
        const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
        const approveResp = await submitPassword(app, fields.code, fields.csrf, "test-password");
        const location = approveResp.headers.get("location")!;
        const authCode = new URL(location).searchParams.get("code")!;

        const tokenResp = await app.request("/oauth/token", {
            method: "POST",
            headers: { "Content-Type": "application/x-www-form-urlencoded" },
            body: new URLSearchParams({
                grant_type: "authorization_code",
                code: authCode,
                client_id: "wrong-client-id",
                code_verifier: pkce.verifier,
                redirect_uri: "https://app.example.com/callback",
            }).toString(),
        });
        assert.equal(tokenResp.status, 400);
        const body = (await tokenResp.json()) as any;
        assert.equal(body.error, "invalid_grant");
    });

    it("authorization code is single-use", async () => {
        const { app } = setup();
        const pkce = generatePKCE();
        const client = await registerClient(app);
        const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
        const approveResp = await submitPassword(app, fields.code, fields.csrf, "test-password");
        const location = approveResp.headers.get("location")!;
        const authCode = new URL(location).searchParams.get("code")!;

        // First exchange: success
        const resp1 = await app.request("/oauth/token", {
            method: "POST",
            headers: { "Content-Type": "application/x-www-form-urlencoded" },
            body: new URLSearchParams({
                grant_type: "authorization_code",
                code: authCode,
                client_id: client.client_id,
                client_secret: client.client_secret,
                code_verifier: pkce.verifier,
                redirect_uri: "https://app.example.com/callback",
            }).toString(),
        });
        assert.equal(resp1.status, 200);

        // Second exchange: fail
        const resp2 = await app.request("/oauth/token", {
            method: "POST",
            headers: { "Content-Type": "application/x-www-form-urlencoded" },
            body: new URLSearchParams({
                grant_type: "authorization_code",
                code: authCode,
                client_id: client.client_id,
                client_secret: client.client_secret,
                code_verifier: pkce.verifier,
                redirect_uri: "https://app.example.com/callback",
            }).toString(),
        });
        assert.equal(resp2.status, 400);
    });

    it("rejects the authorize-page code when the password step was skipped (GHSA-cc9w-6w4g-hqv7)", async () => {
        const { app } = setup();
        const pkce = generatePKCE();
        const client = await registerClient(app);
        // Read the code straight out of the authorize page, as an attacker would,
        // without ever POSTing to /oauth/approve.
        const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);

        const tokenResp = await app.request("/oauth/token", {
            method: "POST",
            headers: { "Content-Type": "application/x-www-form-urlencoded" },
            body: new URLSearchParams({
                grant_type: "authorization_code",
                code: fields.code,
                client_id: client.client_id,
                client_secret: client.client_secret,
                code_verifier: pkce.verifier,
                redirect_uri: "https://app.example.com/callback",
            }).toString(),
        });
        assert.equal(tokenResp.status, 400);
        const body = (await tokenResp.json()) as any;
        assert.equal(body.error, "invalid_grant");

        // And no usable token was minted.
        assert.equal(body.access_token, undefined);
    });

    it("rejects unsupported grant_type", async () => {
        const { app } = setup();
        const resp = await app.request("/oauth/token", {
            method: "POST",
            headers: { "Content-Type": "application/x-www-form-urlencoded" },
            body: new URLSearchParams({ grant_type: "client_credentials" }).toString(),
        });
        assert.equal(resp.status, 400);
    });
});

describe("Token Refresh", () => {
    it("allows public clients to exchange and refresh without a secret", async () => {
        const { app, validateToken } = setup();
        const registration = await app.request("/oauth/register", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({
                redirect_uris: ["https://app.example.com/callback"],
                token_endpoint_auth_method: "none",
            }),
        });
        const client = (await registration.json()) as any;
        const pkce = generatePKCE();
        const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
        await submitPassword(app, fields.code, fields.csrf, "test-password");
        const exchange = await app.request("/oauth/token", {
            method: "POST",
            body: new URLSearchParams({
                grant_type: "authorization_code",
                code: fields.code,
                client_id: client.client_id,
                code_verifier: pkce.verifier,
                redirect_uri: "https://app.example.com/callback",
            }),
        });
        assert.equal(exchange.status, 200);
        const tokens = (await exchange.json()) as any;
        const refresh = await app.request("/oauth/token", {
            method: "POST",
            body: new URLSearchParams({
                grant_type: "refresh_token",
                refresh_token: tokens.refresh_token,
                client_id: client.client_id,
            }),
        });
        assert.equal(refresh.status, 200);
        assert.equal(validateToken(`Bearer ${((await refresh.json()) as any).access_token}`), true);
    });

    it("requires credentials for persisted clients that predate the auth method field", async () => {
        const directory = await mkdtemp(join(tmpdir(), "auth-legacy-"));
        try {
            const persistPath = join(directory, "tokens.json");
            await writeFile(
                persistPath,
                JSON.stringify({
                    clients: {
                        legacy: {
                            clientId: "legacy",
                            clientSecret: "legacy-secret",
                            redirectUris: ["https://app.example.com/callback"],
                        },
                    },
                    refreshTokens: {
                        refresh: {
                            clientId: "legacy",
                            accessToken: "old-access",
                            refreshToken: "refresh",
                            expiresAt: Date.now() + 60_000,
                            refreshExpiresAt: Date.now() + 60_000,
                        },
                    },
                }),
            );
            const app = new Hono();
            const auth = mountPasswordAuth(
                app,
                "https://example.com",
                "test-password",
                persistPath,
            );
            await auth.loadTokens();
            for (const [secret, status] of [
                ["wrong", 401],
                ["legacy-secret", 200],
            ] as const) {
                const response = await app.request("/oauth/token", {
                    method: "POST",
                    body: new URLSearchParams({
                        grant_type: "refresh_token",
                        refresh_token: "refresh",
                        client_id: "legacy",
                        client_secret: secret,
                    }),
                });
                assert.equal(response.status, status);
            }
        } finally {
            await rm(directory, { recursive: true, force: true });
        }
    });

    it("requires the issuing client's credentials without consuming the refresh token", async () => {
        const { app, validateToken } = setup();
        const tokens = await completeOAuthFlow(app, "test-password");
        const other = await registerClient(app);
        const invalidCredentials: Record<string, string>[] = [
            { client_id: tokens.client.client_id },
            { client_id: tokens.client.client_id, client_secret: "wrong-secret" },
            { client_id: other.client_id, client_secret: other.client_secret },
            {},
        ];
        for (const credentials of invalidCredentials) {
            const response = await app.request("/oauth/token", {
                method: "POST",
                body: new URLSearchParams({
                    grant_type: "refresh_token",
                    refresh_token: tokens.refresh_token,
                    ...credentials,
                }),
            });
            const wrongClient = credentials.client_id === other.client_id;
            assert.equal(response.status, wrongClient ? 400 : 401);
            const body = (await response.json()) as any;
            assert.equal(body.error, wrongClient ? "invalid_grant" : "invalid_client");
            assert.equal(body.access_token, undefined);
            assert.equal(validateToken(`Bearer ${tokens.access_token}`), true);
        }
        const response = await app.request("/oauth/token", {
            method: "POST",
            body: new URLSearchParams({
                grant_type: "refresh_token",
                refresh_token: tokens.refresh_token,
                client_id: tokens.client.client_id,
                client_secret: tokens.client.client_secret,
            }),
        });
        assert.equal(response.status, 200);
        assert.equal(
            validateToken(`Bearer ${((await response.json()) as any).access_token}`),
            true,
        );
    });

    it("rotates tokens — old ones invalidated", async () => {
        const { app, validateToken } = setup();
        const tokens = await completeOAuthFlow(app, "test-password");

        // Old token works
        assert.ok(validateToken(`Bearer ${tokens.access_token}`));

        // Refresh
        const refreshResp = await app.request("/oauth/token", {
            method: "POST",
            headers: { "Content-Type": "application/x-www-form-urlencoded" },
            body: new URLSearchParams({
                grant_type: "refresh_token",
                refresh_token: tokens.refresh_token,
                client_id: tokens.client.client_id,
                client_secret: tokens.client.client_secret,
            }).toString(),
        });
        assert.equal(refreshResp.status, 200);
        const newTokens = (await refreshResp.json()) as any;
        assert.ok(newTokens.access_token);
        assert.notEqual(newTokens.access_token, tokens.access_token);

        // Old token no longer works
        assert.equal(validateToken(`Bearer ${tokens.access_token}`), false);

        // New token works
        assert.ok(validateToken(`Bearer ${newTokens.access_token}`));

        // Old refresh token no longer works
        const resp2 = await app.request("/oauth/token", {
            method: "POST",
            headers: { "Content-Type": "application/x-www-form-urlencoded" },
            body: new URLSearchParams({
                grant_type: "refresh_token",
                refresh_token: tokens.refresh_token,
                client_id: tokens.client.client_id,
                client_secret: tokens.client.client_secret,
            }).toString(),
        });
        assert.equal(resp2.status, 400);
    });
});

describe("validateToken", () => {
    it("returns false for undefined", () => {
        const { validateToken } = setup();
        assert.equal(validateToken(undefined), false);
    });

    it("returns false for non-Bearer header", () => {
        const { validateToken } = setup();
        assert.equal(validateToken("Basic abc"), false);
    });

    it("returns false for unknown token", () => {
        const { validateToken } = setup();
        assert.equal(validateToken("Bearer bad-token"), false);
    });

    it("returns true for valid token from OAuth flow", async () => {
        const { app, validateToken } = setup();
        const tokens = await completeOAuthFlow(app, "test-password");
        assert.ok(validateToken(`Bearer ${tokens.access_token}`));
    });
});

describe("safeEqual", () => {
    it("matches identical strings", () => {
        assert.equal(safeEqual("Bearer abc", "Bearer abc"), true);
    });

    it("rejects different strings of equal and unequal length", () => {
        assert.equal(safeEqual("Bearer abc", "Bearer abd"), false);
        assert.equal(safeEqual("Bearer abc", "Bearer abcd"), false);
        assert.equal(safeEqual("", "Bearer abc"), false);
    });

    it("does not throw when byte lengths differ but string lengths match", () => {
        // "é" is one UTF-16 unit but two UTF-8 bytes: the case that made timingSafeEqual throw.
        assert.equal(safeEqual("Bearer abcé", "Bearer abcd"), false);
    });
});

describe("Hardening", () => {
    async function approvedCode(app: Hono) {
        const pkce = generatePKCE();
        const client = await registerClient(app);
        const { fields } = await getAuthorizePage(app, client.client_id, pkce.challenge);
        const approveResp = await submitPassword(app, fields.code, fields.csrf, "test-password");
        const code = new URL(approveResp.headers.get("location")!).searchParams.get("code")!;
        return { client, pkce, code };
    }

    function exchange(
        app: Hono,
        client: { client_id: string; client_secret: string },
        code: string,
        verifier?: string,
    ) {
        const params: Record<string, string> = {
            grant_type: "authorization_code",
            code,
            client_id: client.client_id,
            client_secret: client.client_secret,
            redirect_uri: "https://app.example.com/callback",
        };
        if (verifier !== undefined) params.code_verifier = verifier;
        return app.request("/oauth/token", {
            method: "POST",
            headers: { "Content-Type": "application/x-www-form-urlencoded" },
            body: new URLSearchParams(params).toString(),
        });
    }

    it("returns invalid_grant, not 500, when code_verifier is missing", async () => {
        const { app } = setup();
        const { client, code } = await approvedCode(app);
        const resp = await exchange(app, client, code);
        assert.equal(resp.status, 400);
        assert.equal(((await resp.json()) as { error: string }).error, "invalid_grant");
    });

    it("consumes the code after a failed PKCE check", async () => {
        const { app } = setup();
        const { client, pkce, code } = await approvedCode(app);
        assert.equal((await exchange(app, client, code, "wrong-verifier")).status, 400);
        assert.equal((await exchange(app, client, code, pkce.verifier)).status, 400);
    });

    it("rejects malformed JSON on /oauth/register with 400", async () => {
        const { app } = setup();
        const resp = await app.request("/oauth/register", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: "{not json",
        });
        assert.equal(resp.status, 400);
    });

    it("accepts a lowercase bearer scheme", async () => {
        const { app, validateToken } = setup();
        const tokens = await completeOAuthFlow(app, "test-password");
        assert.ok(validateToken(`bearer ${tokens.access_token}`));
    });

    it("keeps client registrations after a reload", async () => {
        const dir = await mkdtemp(join(tmpdir(), "auth-"));
        try {
            const path = join(dir, "tokens.json");
            const client = await registerClient(
                (() => {
                    const app = new Hono();
                    mountPasswordAuth(app, "https://example.com", "pw", path);
                    return app;
                })(),
            );
            const third = new Hono();
            await mountPasswordAuth(third, "https://example.com", "pw", path).loadTokens();
            const { resp } = await getAuthorizePage(third, client.client_id, "x");
            assert.equal(resp.status, 200);
        } finally {
            await rm(dir, { recursive: true, force: true });
        }
    });
});
