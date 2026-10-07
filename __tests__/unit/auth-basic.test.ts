import { describe, it } from "vitest";
import assert from "node:assert/strict";
import { Hono } from "hono";
import { createHash, randomBytes } from "node:crypto";
import { mountPasswordAuth } from "../../src/auth/auth.js";

const PASSWORD = "test-password";
const REDIRECT_URI = "https://app.example.com/callback";

function setup() {
    const app = new Hono();
    const auth = mountPasswordAuth(app, "https://example.com", PASSWORD);
    return { app, validateToken: auth.validateToken };
}

function basic(clientId: string, secret: string) {
    const encoded = Buffer.from(
        `${encodeURIComponent(clientId)}:${encodeURIComponent(secret)}`,
    ).toString("base64");
    return `Basic ${encoded}`;
}

async function approvedCode(app: Hono) {
    const verifier = randomBytes(32).toString("base64url");
    const challenge = createHash("sha256").update(verifier).digest("base64url");
    const reg = await app.request("/oauth/register", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ client_name: "test", redirect_uris: [REDIRECT_URI] }),
    });
    const client = (await reg.json()) as { client_id: string; client_secret: string };
    const page = await app.request(
        `/oauth/authorize?${new URLSearchParams({
            client_id: client.client_id,
            redirect_uri: REDIRECT_URI,
            code_challenge: challenge,
            code_challenge_method: "S256",
            state: "s",
            response_type: "code",
        })}`,
    );
    const html = await page.text();
    const field = (name: string) => html.match(new RegExp(`name="${name}"\\s+value="([^"]*)"`))![1];
    const approve = await app.request("/oauth/approve", {
        method: "POST",
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams({
            code: field("code"),
            csrf: field("csrf"),
            password: PASSWORD,
        }).toString(),
    });
    assert.equal(approve.status, 302);
    const code = new URL(approve.headers.get("location")!).searchParams.get("code")!;
    return { client, code, verifier };
}

function token(app: Hono, params: Record<string, string>, authorization?: string) {
    return app.request("/oauth/token", {
        method: "POST",
        headers: {
            "Content-Type": "application/x-www-form-urlencoded",
            ...(authorization ? { Authorization: authorization } : {}),
        },
        body: new URLSearchParams(params).toString(),
    });
}

function codeParams(code: string, verifier: string, extra: Record<string, string> = {}) {
    return {
        grant_type: "authorization_code",
        code,
        code_verifier: verifier,
        redirect_uri: REDIRECT_URI,
        ...extra,
    };
}

describe("Token endpoint — client_secret_basic", () => {
    it("exchanges a code and refreshes with HTTP Basic credentials", async () => {
        const { app, validateToken } = setup();
        const { client, code, verifier } = await approvedCode(app);
        const auth = basic(client.client_id, client.client_secret);

        const exchange = await token(app, codeParams(code, verifier), auth);
        assert.equal(exchange.status, 200);
        const tokens = (await exchange.json()) as { access_token: string; refresh_token: string };
        assert.equal(validateToken(`Bearer ${tokens.access_token}`), true);

        const refresh = await token(
            app,
            { grant_type: "refresh_token", refresh_token: tokens.refresh_token },
            auth,
        );
        assert.equal(refresh.status, 200);
        assert.equal(validateToken(`Bearer ${((await refresh.json()) as any).access_token}`), true);
    });

    it("rejects a wrong Basic secret without consuming the code", async () => {
        const { app } = setup();
        const { client, code, verifier } = await approvedCode(app);

        const wrong = await token(
            app,
            codeParams(code, verifier),
            basic(client.client_id, "wrong-secret"),
        );
        assert.equal(wrong.status, 401);
        assert.equal(((await wrong.json()) as any).error, "invalid_client");

        const right = await token(
            app,
            codeParams(code, verifier),
            basic(client.client_id, client.client_secret),
        );
        assert.equal(right.status, 200);
    });

    it("rejects a wrong Basic secret on refresh", async () => {
        const { app } = setup();
        const { client, code, verifier } = await approvedCode(app);
        const exchange = await token(
            app,
            codeParams(code, verifier),
            basic(client.client_id, client.client_secret),
        );
        const tokens = (await exchange.json()) as { refresh_token: string };

        const wrong = await token(
            app,
            { grant_type: "refresh_token", refresh_token: tokens.refresh_token },
            basic(client.client_id, "wrong-secret"),
        );
        assert.equal(wrong.status, 401);
        assert.equal(((await wrong.json()) as any).error, "invalid_client");
    });

    it("rejects Basic credentials that disagree with body credentials", async () => {
        const { app } = setup();
        const { client, code, verifier } = await approvedCode(app);
        const auth = basic(client.client_id, client.client_secret);

        const mismatches: Record<string, string>[] = [
            { client_id: "other-client" },
            { client_secret: "other-secret" },
        ];
        for (const extra of mismatches) {
            const resp = await token(app, codeParams(code, verifier, extra), auth);
            assert.equal(resp.status, 401, JSON.stringify(extra));
            assert.equal(((await resp.json()) as any).error, "invalid_client");
        }

        // Agreeing body credentials alongside Basic are fine.
        const ok = await token(
            app,
            codeParams(code, verifier, {
                client_id: client.client_id,
                client_secret: client.client_secret,
            }),
            auth,
        );
        assert.equal(ok.status, 200);
    });

    it("rejects a malformed Basic header", async () => {
        const { app } = setup();
        const { code, verifier } = await approvedCode(app);
        const resp = await token(
            app,
            codeParams(code, verifier),
            "Basic !!!not-base64-without-colon",
        );
        assert.equal(resp.status, 401);
        assert.equal(((await resp.json()) as any).error, "invalid_client");
    });
});
