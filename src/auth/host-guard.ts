/**
 * Host-header allowlisting to defeat DNS-rebinding attacks (CWE-350).
 *
 * When the server runs without MCP_AUTH_TOKEN ("local only" mode), a malicious
 * web page the operator visits can use DNS rebinding to point its hostname at
 * 127.0.0.1 and reach the MCP endpoint from the browser. Binding to loopback
 * does NOT help — the OS resolves the attacker hostname to loopback while the
 * page keeps its origin. The browser still sends the attacker's hostname in the
 * `Host` header, so validating Host against a localhost allowlist rejects the
 * rebound request. A bearer token (auth mode) already defeats this, since a
 * browser won't attach it; this guard protects the no-token path.
 *
 * Kept dependency-free so it is unit-testable in isolation.
 */

const LOCAL_HOSTS = ["localhost", "127.0.0.1", "::1"];

/** Extract a normalized, lowercased hostname from a raw `Host` header (strips port and IPv6 brackets). */
export function parseHostname(hostHeader: string | undefined | null): string | null {
    if (!hostHeader) return null;
    // A real Host header is a bare host[:port]. Reject anything carrying userinfo
    // or a path (e.g. "attacker.example@127.0.0.1", "127.0.0.1/x") so a crafted
    // value can't normalize into the allowlist.
    if (hostHeader.includes("@") || hostHeader.includes("/")) return null;
    try {
        // Wrapping in a URL parses `host:port` and `[ipv6]:port` correctly.
        const u = new URL(`http://${hostHeader}`);
        if (u.username || u.password || u.pathname !== "/" || u.search || u.hash) return null;
        return u.hostname.replace(/^\[|\]$/g, "").toLowerCase();
    } catch {
        return null;
    }
}

/** Build the set of allowed hostnames: always localhost, plus any from a comma-separated list. */
export function buildAllowedHosts(extra?: string): Set<string> {
    const set = new Set(LOCAL_HOSTS);
    for (const entry of (extra ?? "")
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean)) {
        set.add(parseHostname(entry) ?? entry.toLowerCase());
    }
    return set;
}

/** True if the request's Host header resolves to an allowed hostname. Missing/malformed Host is rejected. */
export function isHostAllowed(
    hostHeader: string | undefined | null,
    allowed: Set<string>,
): boolean {
    const hostname = parseHostname(hostHeader);
    return hostname !== null && allowed.has(hostname);
}

/**
 * True if the request has no browser `Origin`, or its Origin host is allowlisted.
 *
 * Host validation alone is not enough: the transport answers with a wildcard
 * `Access-Control-Allow-Origin: *`, so a page at any origin can skip DNS
 * rebinding and directly `fetch('http://127.0.0.1:<port>/mcp')` — that request
 * carries a genuine loopback Host (passes isHostAllowed) but an attacker Origin.
 * Non-browser MCP clients (CLI, desktop apps) send no Origin, so requiring
 * "no Origin, or an allowlisted Origin" blocks cross-origin browsers without
 * affecting legitimate local callers.
 */
export function isOriginAllowed(
    originHeader: string | undefined | null,
    allowed: Set<string>,
): boolean {
    if (!originHeader) return true; // non-browser client — no Origin header
    try {
        const hostname = new URL(originHeader).hostname.replace(/^\[|\]$/g, "").toLowerCase();
        return allowed.has(hostname);
    } catch {
        return false; // "null"/opaque or malformed Origin — reject
    }
}
