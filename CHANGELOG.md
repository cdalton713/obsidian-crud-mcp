# Changelog

### Changed

- The server runs as a single native Rust binary (`obsidian-crud-mcp`) built with Cargo.
- The persisted search index uses schema version 4. The server rebuilds an incompatible index from the vault.
- `update_note_properties` now keeps untouched frontmatter lines byte-for-byte, comments and spacing included.
- `list_notes` reads `modified_after` date-times without an offset as UTC.
- Releases include a Linux binary archive; the MCP registry entry points at the Docker image.

## 1.0.0

First release of `obsidian-crud-mcp`, an MCP server for Obsidian vaults synced with [Remotely Save](https://github.com/remotely-save/remotely-save).

### Backends

- **S3 mode:** mirrors the bucket Remotely Save syncs to (`S3_ENDPOINT`, `S3_BUCKET`, `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY`, `VAULT_PATH`). Polls for changes every `S3_POLL_SECONDS`; writes upload first with ETag preconditions, so a newer device edit is reported instead of overwritten.
- **Local mode:** reads and writes a vault folder directly (`VAULT_PATH`).

### Tools

- Notes: `read_note`, `read_notes`, `write_note`, `edit_note`, `move_note`, `delete_note`.
- Discovery: `list_notes`, `list_folders`, `list_tags`, `list_tasks`, `get_note_metadata`, `get_note_outline`, `update_note_properties`.
- Search: `search_notes` scans the whole vault from an in-memory content cache (`SEARCH_CONTENT_CACHE_MB`); optional `semantic_search` uses Cloudflare AI Search (`CF_ACCOUNT_ID`, `CF_AI_SEARCH_TOKEN`, `CF_AI_SEARCH_INSTANCE`).
- `READ_ONLY` disables write tools; `WRITE_FOLDERS` limits writes to chosen folders.

### Security

- OAuth with PKCE, confidential-client authentication and client-bound refresh tokens; the consent page shows the redirect destination.
- Note paths are validated in every backend: vault-relative `.md` only, no traversal, hidden folders or symlink escapes.
- The persisted metadata index can be encrypted at rest with `INDEX_PASSPHRASE`.

### Deploy

- Docker image at `ghcr.io/cdalton713/obsidian-crud-mcp`, `docker-compose` stack with a persistent data volume, and Fly.io config (`deploy/mcp-only`, `deploy/setup.sh`).
- Stateless MCP transport with plain JSON responses; every tool call logs its name and duration.
