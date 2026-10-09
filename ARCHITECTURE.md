# Architecture

## Overview

```
Obsidian (phone/desktop)
    ↕ Remotely Save plugin (on save / on a schedule)
S3-compatible bucket (Cloudflare R2, AWS S3, B2, MinIO, ...)
    ↕ S3Mirror: poll + conditional uploads (AWS SDK for Rust)
Local mirror folder (VAULT_PATH, on a persistent volume)
    ↕ LocalVault
MCP Server (this project)
    ↕ MCP protocol over HTTP
AI Agents (Claude, Copilot, custom)
```

The MCP server reads notes from a local folder: either the vault itself (filesystem mode) or a mirror of the bucket Remotely Save syncs to (S3 mode). Every tool runs against that folder, so reads and content search never touch the network. Content search is a disk scan: 16 notes in flight at a time, consumed in path order, which covers a vault of ~10,000 notes in well under a second on local SSD.

## Modes

### Filesystem mode (`VAULT_PATH`)

- Reads `.md` files directly from a vault directory
- File watcher (`notify`, debounced 100ms per path) detects external edits. On Linux it uses one inotify watch per directory; past `fs.inotify.max_user_watches` the watcher logs an error and stops instead of taking the server down
- Startup: loads persisted index, diffs mtimes against filesystem, reads only changed files

### S3 mode (`S3_BUCKET` + `VAULT_PATH`)

For vaults synced by [Remotely Save](https://github.com/remotely-save/remotely-save) to an S3-compatible bucket. Remotely Save stores notes as plain files at their vault paths, so no document format or chunking is involved. Its end-to-end encryption is not supported.

- `S3Vault` (`src/vault/s3.rs`) wraps a `LocalVault` over `VAULT_PATH`, so reads, listing and search are the filesystem-mode code
- `S3Mirror` (`src/vault/mirror.rs`) lists the bucket on startup and every `S3_POLL_SECONDS`, downloads notes whose ETag changed, and deletes local notes removed from the bucket. Only `.md` notes that pass `validateNotePath` are mirrored (no `.obsidian/`, attachments, or Remotely Save state files)
- A manifest (`DATA_DIR/<vault-hash>/s3-manifest.json`, path → ETag + mtime) records what the mirror owns; local files it never downloaded or uploaded are never deleted
- Writes upload first (conditional on the last-seen ETag, or `If-None-Match: *` for new notes), then write locally. A rejected condition (412) means a device uploaded a newer version since the last poll; the tool reports it instead of overwriting. Move is `CopyObject` + `DeleteObject`
- Timestamps use Remotely Save's S3 metadata: `MTime`/`CTime` in seconds; legacy millisecond values and objects without metadata (LastModified) are also read
- Writes never wait for a poll. Listing and downloads run unlocked; only the local apply of a note (file + manifest entry) is serialized, and a poll leaves alone any note the server wrote after that poll's listing began, so a stale listing cannot delete, overwrite or resurrect it
- The S3 client fails a request after 5 s without a connection or 15 s without data, and only sends checksums when an operation requires them (R2, B2 and MinIO reject the SDK's newer defaults), so a keep-alive socket that died while the machine was suspended costs one retry instead of a hung poll or write
- The mirror talks to the bucket through the `ObjectStore` trait (`AwsStore` in production, an in-memory fake in tests)
- Each poll reports the notes it downloaded (content and mtime) or removed to the search index through `VaultBackend::subscribe`, so S3 mode runs no filesystem watcher and never reads a downloaded note a second time. The server's own uploads are not reported; the tool that wrote them updates the index itself

## Search Index (`src/search/index.rs`)

A single `SearchIndex` manages all indexed data in memory, behind one `RwLock`:

```
(no full-text search — metadata only)
known_paths: HashSet<String>              ─── all indexed note paths
mtimes: HashMap<path, f64>                ─── modification timestamps (ms)
tags: HashMap<path, Vec<String>>          ─── extracted from frontmatter + inline #tags
links: HashMap<path, Vec<String>>         ─── outgoing [[wikilinks]] and [markdown](links.md)
backlinks: HashMap<target, HashSet<path>> ─── reverse link index (case-insensitive keys)
```

Content search (`search_notes`, `list_tasks`) scans note bodies from an in-memory content cache (`contents: HashMap<path, Arc<str>>`, capped by `SEARCH_CONTENT_CACHE_MB`, default 32M characters). Every index update stores the note's content, so the cache is as fresh as the index; it is not persisted and refills from disk on the first scan after a restart (or for notes beyond the cap), 16 reads at a time in path order. Pages are bounded by note count (`max_notes`, default 10,000) and characters (50,000,000) with a continuation cursor. Once the index is ready it supplies the candidate paths, so `folder` and `tag` filters cost no reads; before that the folder is listed and tags are checked per note. `search_notes` runs its needle once over each note and counts line breaks only up to each match; `list_tasks` skips the Markdown parse for notes without a `[ ]`/`[x]` marker.

### Persistence

Everything is serialized to a single JSON file at `DATA_DIR/<vault-hash>/search-index.json`:

- No full-text index (removed FlexSearch for memory efficiency); content is cached in memory but never written to disk
- Metadata (mtimes, tags, links)
- Encrypted with AES-256-GCM when `INDEX_PASSPHRASE` is set
- Saved every 5 minutes + on graceful shutdown
- Saves are serialized; a save requested during another writes the latest state once that one finishes

### Startup flow

```
Load persisted index from disk
  ↓
(S3 mode) Mirror the bucket into VAULT_PATH; each note updates the index as it lands
(local mode) Start the watcher (debounced, reads through read_note for symlink safety)
  ↓
Diff mtimes against the folder
  → remove stale entries (deleted files)
  → read only changed/new files, 16 at a time
  ↓
(S3 mode) Poll the bucket every S3_POLL_SECONDS; each poll feeds the index directly
```

With no persisted index (first startup or corrupted), every note is read once.

### Fault tolerance

- Wrong passphrase / corrupted index / older schema version: `load_from_disk` falls back to a full rebuild
- Volume nuked: no persisted index or manifest; the mirror re-downloads every note and the index rebuilds; auth tokens lost (users re-authenticate)
- Bucket unreachable during a poll: the poll logs a warning and the next one retries; reads keep serving the mirror
- Crash during save: concurrent save guard prevents corruption; next restart rebuilds

## Semantic search (`src/search/ai_search.rs`, optional)

`semantic_search` is served by a Cloudflare AI Search instance that indexes the same R2 bucket Remotely Save syncs to. AI Search owns chunking, embeddings (bge-m3 by default), the vector and keyword indexes, and re-indexing; `AiSearchClient` only POSTs a query to the instance's `/search` endpoint, turns each returned object key into a vault path (strips `S3_PREFIX`, drops anything `is_mirrored_path` rejects) and returns passages with scores. After the server writes, edits, moves or deletes a note the client asks for an indexing job, coalescing a minute of writes into one request and keeping jobs 30 s apart (AI Search's limit), so the server's own changes are searchable within minutes rather than at the next scheduled sync. Nothing about this runs on the machine: no model, no vectors, no extra memory.

## Vault Backend (`src/vault/mod.rs`)

Interface shared by both modes:

```rust
#[async_trait]
pub trait VaultBackend: Send + Sync {
    async fn init(&self) -> Result<(), VaultError>;
    async fn close(&self);
    async fn read_note(&self, path: &str) -> Result<Option<String>, VaultError>;
    async fn write_note(&self, path: &str, content: &str) -> Result<bool, VaultError>;
    async fn delete_note(&self, path: &str) -> Result<bool, VaultError>;
    async fn move_note(&self, from: &str, to: &str) -> Result<bool, VaultError>;
    async fn get_metadata(&self, path: &str) -> Result<Option<NoteInfo>, VaultError>;
    async fn list_notes(&self, folder: Option<&str>) -> Result<Vec<String>, VaultError>;
    async fn list_notes_with_mtime(&self, folder: Option<&str>) -> Result<Vec<NoteListing>, VaultError>;
    fn subscribe(&self, listener: Arc<dyn VaultChangeListener>) -> Option<Subscription>;
}
```

`ReadOnlyVault` (`src/vault/read_only.rs`) wraps any backend and rejects every write when `READ_ONLY` is set.

### LocalVault (`src/vault/local.rs`)

- `safe_path()` resolves symlinks and blocks traversal, including through dangling symlinks and missing parents
- `list_notes_with_mtime()` walks the folder (`walkdir`, on a blocking thread) and stats each note
- Skips hidden folders such as `.obsidian/`

### S3Vault (`src/vault/s3.rs`)

- Delegates reads and listings to its `LocalVault`
- Routes writes, deletes and moves through `S3Mirror` before applying them locally
- `close()` stops the poll and waits for in-flight work
- Polls never overlap: the background poller and an explicit `poll()` take turns

## Authentication (`src/auth/oauth.rs`)

Self-contained OAuth 2.1 provider with PKCE:

```
Agent connects → /oauth/authorize → password page → /oauth/approve
  → redirect with code → /oauth/token (PKCE verified) → access + refresh tokens
```

- Rate limiting with exponential backoff (capped at ~85 min)
- CSRF tokens rotated on each failed attempt
- Token persistence to disk (0600 permissions)
- Periodic cleanup of expired tokens and unused clients
- Also accepts static `Bearer <MCP_AUTH_TOKEN>` for non-OAuth clients
- Without `MCP_AUTH_TOKEN`, `src/auth/host_guard.rs` allows only local `Host` and `Origin` headers (DNS rebinding and cross-origin protection)

## Tools (`src/tools/`)

Built in `src/tools/` and served by the MCP layer in `src/mcp.rs`. Arguments are typed structs (`src/tools/params.rs`); their JSON Schema comes from `schemars` and every call is validated against it before the tool runs.

| Tool                     | Reads from                             | Writes to     |
| ------------------------ | -------------------------------------- | ------------- |
| `read_note`              | vault                                  | —             |
| `read_notes`             | vault                                  | —             |
| `search_notes`           | vault (paged scan), index (candidates) | —             |
| `semantic_search`        | Cloudflare AI Search                   | —             |
| `write_note`             | —                                      | vault + index |
| `edit_note`              | vault                                  | vault + index |
| `update_note_properties` | vault                                  | vault + index |
| `get_note_outline`       | vault                                  | —             |
| `list_tasks`             | vault (paged scan), index (candidates) | —             |
| `list_notes`             | index (fallback: vault)                | —             |
| `list_folders`           | index (fallback: vault)                | —             |
| `list_tags`              | index                                  | —             |
| `get_note_metadata`      | vault + index (backlinks)              | —             |
| `move_note`              | vault                                  | vault + index |
| `delete_note`            | —                                      | vault + index |

## Build

`cargo build --release` produces one static-ish binary, `obsidian-crud-mcp`.
The Docker image builds it in `rust:slim` and copies it into `debian:bookworm-slim`
with CA certificates.

## Module map

| Path                  | Role                                                                    |
| --------------------- | ----------------------------------------------------------------------- |
| `src/main.rs`         | Startup, background tasks, graceful shutdown                            |
| `src/config.rs`       | Environment variables, parsed once                                      |
| `src/mcp.rs`          | Stateless JSON-RPC MCP layer and the tool registry                      |
| `src/server/`         | HTTP routes, request authentication, index bootstrap, folder watcher    |
| `src/auth/`           | OAuth provider and Host/Origin guard                                    |
| `src/tools/`          | Tool implementations, argument types, `list_notes` wording              |
| `src/notes/`          | Paths, frontmatter, tags and links, outlines and tasks, content scans   |
| `src/search/`         | Metadata index and Cloudflare AI Search client                          |
| `src/vault/`          | Backends: local folder, S3 mirror, read-only wrapper, write scope       |
| `src/logging.rs`      | stderr logging with credential redaction                                |

## Dependencies

- **aws-sdk-s3** — S3 mode bucket access
- **axum** + **tokio** — HTTP server and async runtime. The MCP endpoint runs stateless Streamable HTTP with plain JSON responses: each request stands alone, so hosted agents that open a new MCP session per tool call pay no session setup, and nothing streams, so results go out as a plain body
- **markdown** (markdown-rs) — CommonMark syntax tree with source positions for outlines, block IDs and tasks
- **serde_norway** — YAML frontmatter parsing
- **schemars** + **jsonschema** — tool argument schemas and validation
- **aes-gcm** + **scrypt** — optional at-rest encryption of the persisted index
- **notify** — filesystem watching in local mode
