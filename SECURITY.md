# Security

This document describes the security posture of obsidian-crud-mcp.

---

## Authentication

### Password-gated OAuth 2.1

The server implements a self-contained OAuth 2.1 authorization server with PKCE. No third-party identity provider (Google, GitHub, etc.) is required. Users set a password via `MCP_AUTH_TOKEN` and enter it once when an agent connects.

- **OAuth 2.1 with PKCE (S256)** — authorization code flow with Proof Key for Code Exchange. Only S256 is accepted; plain PKCE and missing challenges are rejected.
- **Dynamic Client Registration (RFC 7591)** — agents register themselves automatically. No manual client setup.
- **Access tokens** expire after 1 hour. Agents refresh transparently — users don't re-enter the password.
- **Refresh tokens** expire after 14 days of inactivity (configurable via `MCP_REFRESH_DAYS`). After expiry, the user must re-authenticate.
- **Refresh token rotation** — each refresh issues a new refresh token and invalidates the old one. If a token is leaked and both parties try to refresh, the first one wins and the leaked token becomes invalid.
- **No auth mode** — when `MCP_AUTH_TOKEN` is not set, the server runs without authentication. Intended for local testing or use behind a private network.
- **Browser-attack protection (no auth mode)** — with no token, the server validates both the HTTP `Host` and `Origin` headers and rejects any request whose host/origin is not `localhost`/`127.0.0.1`/`::1` (extend with `MCP_ALLOWED_HOSTS`, comma-separated). The `Host` check blocks DNS rebinding (CWE-350) — a loopback bind alone does **not** stop this, since the browser sends the attacker's hostname in `Host`. The `Origin` check blocks the simpler variant where a page directly fetches `http://127.0.0.1:<port>/mcp`: that request has a genuine loopback `Host` but carries a cross-origin `Origin`, and the transport's wildcard CORS would otherwise expose the response. Non-browser MCP clients (CLI, desktop apps) send no `Origin`, so they are unaffected. These checks block _browser_-delivered attacks only: a direct non-browser network client can still forge both headers, so on an untrusted network set `MCP_AUTH_TOKEN`. When a token is set, no host/origin check is needed — a bearer token is not attached by browsers.

### Brute-force protection

- **Rate limiting with exponential backoff** — after 5 failed password attempts, the server locks out for 5 seconds. Each subsequent lockout doubles: 10s, 20s, 40s, 80s, and so on.
- **No counter reset on lockout** — the failed attempt counter persists across lockouts. Only a successful login resets it.
- **All failed attempts are logged** with attempt count for monitoring.

### Token security

- **Timing-safe comparison** — both password and CSRF token comparisons use `crypto.timingSafeEqual` to prevent timing side-channel attacks.
- **CSRF protection** — the OAuth approval form includes a per-request CSRF token. Submissions without a valid token are rejected.
- **Redirect URI validation** — the `/oauth/authorize` endpoint validates that the `redirect_uri` matches what the client registered, preventing authorization code theft via open redirect.
- **Token persistence** — OAuth clients and tokens are persisted to disk whenever they change (registration, code exchange, refresh), on clean shutdown and every 5 minutes, and loaded on restart, so sessions survive server restarts and deploys. Files are stored in `DATA_DIR/<vault-hash>/` with `0600` permissions (owner-only). Defaults to `~/.obsidian-mcp/` locally, or the persistent volume on Fly.io. Each vault gets an isolated subdirectory.

---

## S3 bucket (Remotely Save)

### Access control

- **Separate credentials** — the S3 access keys (shared with Remotely Save) and the MCP auth token (for agent access) are independent. Rotating one doesn't affect the other.
- **Scope the keys to one bucket** — create an API token limited to Object Read & Write on the vault bucket only (on Cloudflare R2: _Manage API tokens_ → _Object Read & Write_ → the one bucket), so leaked keys cannot reach other buckets or account settings.
- **Credentials from environment** — the keys are read from `S3_ACCESS_KEY_ID` and `S3_SECRET_ACCESS_KEY` (Fly.io secrets, or `.env` for Docker Compose), never from a config file. Docker Compose refuses to start without them.
- **Conditional writes** — agent writes use `If-Match` / `If-None-Match` on the last-seen ETag, so an edit a device synced since the last poll is never silently overwritten; the tool reports the conflict instead.

### Network

- **TLS** — the MCP server is served through Fly.io's TLS proxy, and the S3 endpoint is HTTPS. No plaintext traffic on the public internet.
- **HTTPS warning** — when `MCP_AUTH_TOKEN` is set and `BASE_URL` doesn't start with `https://` (and isn't localhost), the server logs a warning at startup.

---

## Filesystem (local mode and the S3 mirror)

- **Path traversal prevention** — all file operations resolve the full path and verify it stays within the vault root directory. Attempts to access `../` or absolute paths outside the vault throw an error before any I/O occurs.
- **Symlink resolution** — `fs.realpath()` resolves symlinks before the path check. A symlink inside the vault pointing to `/etc/passwd` is caught because the resolved path falls outside the vault root.

---

## Data handling

- **No end-to-end encryption** — the server reads plain files from the bucket, so Remotely Save's encryption password must stay empty. Notes are stored unencrypted in the bucket (encrypted at rest by the storage provider) and in the local mirror on the server's volume.
- **Text only** — binary attachments are not exposed through MCP tools, reducing the attack surface.
- **Metadata index** — the server keeps a metadata index (note paths, modification times, tags and links) for `list_notes`, `list_tags` and backlinks. It is persisted to `DATA_DIR/<vault-hash>/search-index.json` with `0600` permissions and, when `INDEX_PASSPHRASE` is set, encrypted at rest with AES-256-GCM (key derived from the passphrase with scrypt). Without a passphrase it is stored in plaintext. Note content is not indexed; it is read from the vault (or the S3 mirror) on demand.

---

## Graceful shutdown

- The server handles `SIGTERM` and `SIGINT` signals, stopping the bucket poll and saving the search index and OAuth tokens before exiting. Prevents data corruption on container stop.

---

## Reporting vulnerabilities

If you find a security issue, please open a GitHub issue or email the maintainer directly. Do not open a public issue for critical vulnerabilities — use private disclosure.
