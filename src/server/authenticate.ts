import type { IncomingMessage } from "node:http";
import { logger } from "../logging/logger.js";
import { safeEqual, type AuthHandle } from "../auth/auth.js";
import { buildAllowedHosts, isHostAllowed, isOriginAllowed } from "../auth/host-guard.js";

type Authenticate = (req: IncomingMessage) => Promise<{ authenticated: true }>;

/**
 * Token mode: accept the static bearer token (curl, MCP Inspector, custom agents)
 * or an OAuth-issued token (Claude Web/Desktop/Mobile). `getAuth` is resolved per
 * request because the OAuth routes are mounted after the server is constructed.
 */
export function tokenAuthenticator(
    authToken: string,
    baseUrl: string,
    getAuth: () => AuthHandle | null,
): Authenticate {
    const expected = `Bearer ${authToken}`;
    logger.info("Auth enabled (password-gated OAuth).");
    return async (req) => {
        const header = req.headers.authorization;
        if (header && safeEqual(header, expected)) return { authenticated: true };
        if (getAuth()?.validateToken(header)) return { authenticated: true };
        // RFC 9728: point strict clients (e.g. Gemini) at the resource
        // metadata; Claude probes /.well-known directly but others rely on this.
        throw new Response("Unauthorized", {
            status: 401,
            headers: {
                "WWW-Authenticate": `Bearer resource_metadata="${baseUrl}/.well-known/oauth-protected-resource"`,
            },
        });
    };
}

/**
 * No-token mode: enforce a Host-header allowlist so the "local only" precondition
 * actually holds. Without this, DNS rebinding lets any website the operator
 * visits reach the tool surface (CWE-350) even on a loopback bind, because the
 * browser still sends the attacker's hostname in Host. Defaults to localhost;
 * MCP_ALLOWED_HOSTS extends it for legit LAN/private-network use.
 */
export function localOnlyAuthenticator(extraHosts: string | undefined, host: string): Authenticate {
    const allowedHosts = buildAllowedHosts(extraHosts);
    logger.info(
        `Auth disabled — accepting only local Host/Origin headers: ${[...allowedHosts].join(", ")}. Set MCP_ALLOWED_HOSTS to add hosts, or MCP_AUTH_TOKEN for authenticated remote access.`,
    );
    if (host === "0.0.0.0") {
        logger.warn(
            "WARNING: No authentication and listening on all interfaces. Browser attacks (DNS rebinding and cross-origin fetch) are blocked by the Host/Origin checks, but any non-browser client that can reach this port has full vault access. Set MCP_AUTH_TOKEN, or HOST=127.0.0.1 to bind to loopback only.",
        );
    }
    return async (req) => {
        // Host check defeats DNS rebinding; Origin check defeats a direct
        // cross-origin browser fetch to loopback (the transport sends wildcard CORS).
        if (!isHostAllowed(req.headers.host, allowedHosts)) {
            throw new Response("Forbidden: Host not allowed", { status: 403 });
        }
        if (!isOriginAllowed(req.headers.origin, allowedHosts)) {
            throw new Response("Forbidden: cross-origin request rejected", { status: 403 });
        }
        return { authenticated: true };
    };
}
