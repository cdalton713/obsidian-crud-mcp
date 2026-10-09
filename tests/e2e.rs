//! End-to-end tests: spawn the real binary against a temporary vault and talk
//! JSON-RPC to it over HTTP, the way an MCP client does.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderValue, LOCATION, WWW_AUTHENTICATE};
use serde_json::{Value, json};
use tempfile::TempDir;

const AUTH: &str = "ci-test-token";
const VAULT_NAME: &str = "TestVault";
const BASE_INSTRUCTIONS: &str = "Access and manage an Obsidian vault";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// A vault folder plus a data folder, both removed on drop. One server at a
/// time may run against them, and restarts reuse both.
struct Fixture {
    vault: TempDir,
    data: TempDir,
}

impl Fixture {
    /// The vault the TypeScript suite used: three notes, a tag, and a link.
    fn new() -> Self {
        let fixture = Self { vault: tempfile::tempdir().unwrap(), data: tempfile::tempdir().unwrap() };
        fixture.write("Welcome.md", "---\ntitle: Welcome\ntags: [intro]\n---\n# Welcome\nHello world");
        fixture.write("daily/2026-03-24.md", "# Daily Note");
        fixture.write("projects/test.md", "See [[Welcome]]\n\n#project");
        fixture
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.vault.path().join(relative)
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.path(relative)).unwrap()
    }

    fn exists(&self, relative: &str) -> bool {
        self.path(relative).exists()
    }

    /// Start a server with the standard settings, overridden or extended by `extra`.
    async fn start(&self, extra: &[(&str, &str)]) -> ServerGuard {
        let mut env: Vec<(String, String)> = [
            ("VAULT_PATH", self.vault.path().to_str().unwrap()),
            ("DATA_DIR", self.data.path().to_str().unwrap()),
            ("HOME", self.data.path().to_str().unwrap()),
            ("VAULT_NAME", VAULT_NAME),
            ("MCP_AUTH_TOKEN", AUTH),
            ("HOST", "127.0.0.1"),
            ("LOG_LEVEL", "info"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        for (key, value) in extra {
            env.retain(|(k, _)| k != key);
            env.push(((*key).to_owned(), (*value).to_owned()));
        }
        ServerGuard::start(env).await
    }
}

/// A running server process, killed on drop. Its stderr (the log) is collected
/// in the background for assertions.
struct ServerGuard {
    child: Child,
    port: u16,
    token: Option<String>,
    logs: Arc<Mutex<String>>,
    http: reqwest::Client,
    initialize_result: Value,
}

fn unused_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn initialize_request(protocol_version: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": protocol_version,
            "capabilities": {},
            "clientInfo": { "name": "e2e", "version": "1.0" },
        },
    })
}

impl ServerGuard {
    async fn start(env: Vec<(String, String)>) -> Self {
        let port = unused_port();
        let token = env.iter().find(|(k, _)| k == "MCP_AUTH_TOKEN").map(|(_, v)| v.clone()).filter(|t| !t.is_empty());
        // A clean environment keeps the developer's own MCP_*, S3_* or CF_*
        // variables from changing what the server does.
        let mut child = Command::new(env!("CARGO_BIN_EXE_obsidian-crud-mcp"))
            .env_clear()
            .envs(env)
            .env("PORT", port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the server binary");
        let logs = Arc::new(Mutex::new(String::new()));
        let stderr = child.stderr.take().unwrap();
        let sink = Arc::clone(&logs);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                let mut logs = sink.lock().unwrap();
                logs.push_str(&line);
                logs.push('\n');
            }
        });
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let mut server = Self { child, port, token, logs, http, initialize_result: Value::Null };

        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if let Some(status) = server.child.try_wait().unwrap() {
                panic!("server exited during startup ({status}):\n{}", server.logs());
            }
            if let Ok(response) = server.post_mcp(&initialize_request("2024-11-05")).await {
                if response.status() == StatusCode::OK {
                    server.initialize_result = response.json::<Value>().await.unwrap()["result"].clone();
                    return server;
                }
            }
            assert!(Instant::now() < deadline, "server did not start in time:\n{}", server.logs());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn logs(&self) -> String {
        self.logs.lock().unwrap().clone()
    }

    /// Wait until the log contains `needle`; false on timeout.
    async fn wait_for_log(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.logs().contains(needle) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// Send SIGTERM and wait for a clean exit; returns the full log.
    async fn terminate(mut self) -> String {
        let pid = self.child.id().to_string();
        let status = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
        assert!(status.success(), "kill -TERM failed");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "server exited with {status}:\n{}", self.logs());
                break;
            }
            assert!(Instant::now() < deadline, "server ignored SIGTERM:\n{}", self.logs());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // The reader thread may still be draining the pipe.
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.logs()
    }

    async fn post_mcp(&self, body: &Value) -> reqwest::Result<reqwest::Response> {
        let mut request = self.http.post(self.url("/mcp")).json(body);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        request.send().await
    }

    /// One JSON-RPC request; returns the whole response object.
    async fn rpc(&self, method: &str, params: Value) -> Value {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let response = self.post_mcp(&body).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{method} failed");
        response.json().await.unwrap()
    }

    async fn call_tool_raw(&self, name: &str, arguments: Value) -> Value {
        self.rpc("tools/call", json!({ "name": name, "arguments": arguments })).await
    }

    /// The text of a successful tool call.
    async fn call_tool(&self, name: &str, arguments: Value) -> String {
        let response = self.call_tool_raw(name, arguments).await;
        let result = &response["result"];
        assert!(result["isError"].is_null(), "tool {name} failed: {response}");
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool {name} returned no text: {response}"))
            .to_owned()
    }

    async fn call_tool_json(&self, name: &str, arguments: Value) -> Value {
        let text = self.call_tool(name, arguments).await;
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("tool {name} returned non-JSON {text:?}: {e}"))
    }

    async fn tool_names(&self) -> Vec<String> {
        let list = self.rpc("tools/list", json!({})).await;
        list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect()
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// POST an `initialize` with a hand-written Host (and optional Origin) header.
/// Raw TCP because HTTP clients derive Host from the URL, and a forged Host is
/// exactly what a DNS-rebinding browser sends.
fn initialize_with_host(port: u16, host: &str, origin: Option<&str>) -> u16 {
    let body = initialize_request("2024-11-05").to_string();
    let origin = origin.map(|o| format!("Origin: {o}\r\n")).unwrap_or_default();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {host}\r\n{origin}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status_line = response.lines().next().unwrap_or_default();
    status_line.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or_else(|| panic!("bad response: {response:?}"))
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

// --- Auth and transport ---

#[tokio::test]
async fn rejects_unauthenticated_requests_with_a_resource_metadata_challenge() {
    let fixture = Fixture::new();
    let server = fixture.start(&[]).await;
    let response = server.http.post(server.url("/mcp")).json(&initialize_request("2024-11-05")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let challenge = response.headers()[WWW_AUTHENTICATE].to_str().unwrap();
    let expected =
        format!("resource_metadata=\"http://localhost:{}/.well-known/oauth-protected-resource\"", server.port);
    assert!(challenge.contains(&expected), "challenge: {challenge}");

    let wrong = server.http.post(server.url("/mcp")).bearer_auth("wrong").json(&json!({})).send().await.unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
}

// Same character count as the real header but more UTF-8 bytes; the TypeScript
// version once threw inside the comparison and leaked the crypto error.
#[tokio::test]
async fn rejects_a_non_ascii_bearer_token_with_a_normal_401() {
    let fixture = Fixture::new();
    let server = fixture.start(&[]).await;
    let forged = format!("Bearer {}é", &AUTH[..AUTH.len() - 1]);
    let response = server
        .http
        .post(server.url("/mcp"))
        .header(AUTHORIZATION, HeaderValue::from_bytes(forged.as_bytes()).unwrap())
        .json(&initialize_request("2024-11-05"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers()[WWW_AUTHENTICATE].to_str().unwrap().contains("resource_metadata="));
    assert!(!response.text().await.unwrap().contains("byte length"));
}

#[tokio::test]
async fn health_and_transport_basics() {
    let fixture = Fixture::new();
    let server = fixture.start(&[]).await;

    let health = server.http.get(server.url("/health")).send().await.unwrap();
    assert_eq!(health.status(), StatusCode::OK, "/health needs no token");
    assert_eq!(health.text().await.unwrap(), "✓ Ok");

    let get = server.http.get(server.url("/mcp")).bearer_auth(AUTH).send().await.unwrap();
    assert_eq!(get.status(), StatusCode::METHOD_NOT_ALLOWED);
    let delete = server.http.delete(server.url("/mcp")).bearer_auth(AUTH).send().await.unwrap();
    assert_eq!(delete.status(), StatusCode::METHOD_NOT_ALLOWED);

    // Browser clients preflight before sending the token.
    let preflight = server
        .http
        .request(reqwest::Method::OPTIONS, server.url("/mcp"))
        .header("Origin", "https://claude.ai")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "authorization, content-type")
        .send()
        .await
        .unwrap();
    assert!(preflight.status().is_success(), "preflight got {}", preflight.status());
    assert_eq!(preflight.headers()["access-control-allow-origin"], "*");

    let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    assert_eq!(server.post_mcp(&notification).await.unwrap().status(), StatusCode::ACCEPTED);

    let garbage = server.http.post(server.url("/mcp")).bearer_auth(AUTH).body("{not json").send().await.unwrap();
    assert_eq!(garbage.status(), StatusCode::BAD_REQUEST);
    assert_eq!(garbage.json::<Value>().await.unwrap()["error"]["code"], -32700);

    assert_eq!(server.rpc("no/such/method", json!({})).await["error"]["code"], -32601);
    assert_eq!(server.call_tool_raw("no_such_tool", json!({})).await["error"]["code"], -32601);
    let invalid = server.call_tool_raw("read_note", json!({ "path": 42 })).await;
    assert_eq!(invalid["error"]["code"], -32602, "schema violations are invalid params: {invalid}");
    assert_eq!(server.rpc("ping", json!({})).await["result"], json!({}));

    let batch = json!([
        { "jsonrpc": "2.0", "id": 1, "method": "ping" },
        { "jsonrpc": "2.0", "method": "notifications/initialized" },
        { "jsonrpc": "2.0", "id": 2, "method": "tools/list" },
    ]);
    let replies: Value = server.post_mcp(&batch).await.unwrap().json().await.unwrap();
    let ids: Vec<_> = replies.as_array().unwrap().iter().map(|r| r["id"].clone()).collect();
    assert_eq!(ids, vec![json!(1), json!(2)], "notifications in a batch get no reply");
}

#[tokio::test]
async fn oauth_flow_issues_a_token_that_opens_mcp_and_survives_a_restart() {
    let fixture = Fixture::new();
    let server = fixture.start(&[]).await;
    let http = &server.http;

    let metadata: Value =
        http.get(server.url("/.well-known/oauth-protected-resource")).send().await.unwrap().json().await.unwrap();
    assert_eq!(metadata["resource"], format!("http://localhost:{}", server.port));

    let client: Value = http
        .post(server.url("/oauth/register"))
        .json(&json!({ "client_name": "e2e", "redirect_uris": ["http://127.0.0.1/cb"], "token_endpoint_auth_method": "none" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client_id = client["client_id"].as_str().unwrap();

    // challenge = BASE64URL(SHA-256(verifier)), computed once offline.
    let verifier = "dBjftJeZ4CVP-mJ92ZZ8SgVzEcm9BC0FzfRBVv4fZDQr4gZqiCtKoaszI0Hcbx";
    let challenge = "8RBxlQVfFeIxpIILZSKq5UgBfHQ1HAFiIqL0o3fc57s";
    let page = http
        .get(server.url("/oauth/authorize"))
        .query(&[
            ("client_id", client_id),
            ("redirect_uri", "http://127.0.0.1/cb"),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", "xyz"),
        ])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let field = |name: &str| {
        let start = page.find(&format!("name=\"{name}\" value=\"")).unwrap() + name.len() + 15;
        page[start..start + page[start..].find('"').unwrap()].to_owned()
    };
    let approve = http
        .post(server.url("/oauth/approve"))
        .form(&[("code", field("code")), ("csrf", field("csrf")), ("password", AUTH.to_owned())])
        .send()
        .await
        .unwrap();
    assert_eq!(approve.status(), StatusCode::FOUND);
    let location = query_pairs(approve.headers()[LOCATION].to_str().unwrap());
    assert_eq!(location.get("state").map(String::as_str), Some("xyz"));

    let tokens: Value = http
        .post(server.url("/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &location["code"]),
            ("client_id", client_id),
            ("code_verifier", verifier),
            ("redirect_uri", "http://127.0.0.1/cb"),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let access_token = tokens["access_token"].as_str().unwrap_or_else(|| panic!("no token: {tokens}")).to_owned();

    let tools_list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let with_token =
        |server: &ServerGuard| server.http.post(server.url("/mcp")).bearer_auth(&access_token).json(&tools_list).send();
    assert_eq!(with_token(&server).await.unwrap().status(), StatusCode::OK);

    server.terminate().await;
    let restarted = fixture.start(&[]).await;
    assert_eq!(with_token(&restarted).await.unwrap().status(), StatusCode::OK, "OAuth sessions persist");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let file = find_file(fixture.data.path(), "auth-tokens.json").expect("auth state persisted");
        assert_eq!(std::fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

fn query_pairs(url: &str) -> std::collections::HashMap<String, String> {
    reqwest::Url::parse(url).unwrap().query_pairs().into_owned().collect()
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == name) {
            return Some(path);
        }
    }
    None
}

// --- Tools ---

#[tokio::test]
async fn list_notes_filters_sorts_and_reports_what_it_omitted() {
    let fixture = Fixture::new();
    let server = fixture.start(&[]).await;

    let all = server.call_tool("list_notes", json!({})).await;
    for path in ["Welcome.md", "daily/2026-03-24.md", "projects/test.md"] {
        assert!(all.contains(path), "missing {path}: {all}");
    }
    assert_eq!(first_line(&all), "3 notes (sorted by name).");

    let daily = server.call_tool("list_notes", json!({ "folder": "daily" })).await;
    assert!(daily.contains("2026-03-24.md") && !daily.contains("Welcome.md"), "{daily}");

    let recent = server.call_tool("list_notes", json!({ "sort_by": "modified", "limit": 2 })).await;
    assert!(recent.starts_with("Showing 2 of 3 notes"), "{recent}");

    // Regression for #19: a truncated or filtered listing must say so.
    let limited = server.call_tool("list_notes", json!({ "limit": 2 })).await;
    let line = first_line(&limited);
    assert!(line.starts_with("Showing 2 of 3 notes"), "{line}");
    assert!(line.contains("Omitted 1: (root) (1)"), "{line}");

    let tagged = server.call_tool("list_notes", json!({ "tag": "intro" })).await;
    assert_eq!(first_line(&tagged), r#"1 note matches tag="intro" (vault has 3 notes, sorted by name)."#);
    assert!(tagged.contains("Welcome.md") && !tagged.contains("projects/test.md"), "{tagged}");

    let none = server.call_tool("list_notes", json!({ "name": "no-such-note" })).await;
    assert_eq!(none, r#"No notes match name="no-such-note" (vault has 3 notes)."#);
}

#[tokio::test]
async fn reads_writes_and_edits_notes() {
    let fixture = Fixture::new();
    let server = fixture.start(&[]).await;

    let read = server.call_tool("read_note", json!({ "path": "Welcome.md" })).await;
    assert!(read.contains("Hello world"), "{read}");
    assert!(read.contains("obsidian://open"), "every response carries a deep link: {read}");

    let saved =
        server.call_tool("write_note", json!({ "path": "ci-test.md", "content": "# CI Test\nWritten by e2e" })).await;
    assert!(saved.contains("Note saved"), "{saved}");
    assert_eq!(fixture.read("ci-test.md"), "# CI Test\nWritten by e2e");

    let appended = server.call_tool("edit_note", json!({ "path": "Welcome.md", "content": "Appended line" })).await;
    assert!(appended.contains("Note edited"), "{appended}");
    assert!(fixture.read("Welcome.md").ends_with("Appended line"));

    server
        .call_tool("edit_note", json!({ "path": "Welcome.md", "content": "Prepended line", "operation": "prepend" }))
        .await;
    let content = fixture.read("Welcome.md");
    assert!(content.starts_with("---\ntitle: Welcome\ntags: [intro]\n---\nPrepended line\n"), "{content}");

    let replaced = server
        .call_tool(
            "edit_note",
            json!({ "path": "Welcome.md", "content": "Goodbye world", "operation": "replace", "old_text": "Hello world" }),
        )
        .await;
    assert!(replaced.contains("Note edited"), "{replaced}");
    let read = server.call_tool("read_note", json!({ "path": "Welcome.md" })).await;
    assert!(read.contains("Goodbye world") && !read.contains("Hello world"), "{read}");
}

#[tokio::test]
async fn lists_folders_tags_and_link_metadata() {
    let fixture = Fixture::new();
    let server = fixture.start(&[]).await;

    let folders = server.call_tool("list_folders", json!({})).await;
    assert!(folders.contains("daily") && folders.contains("projects"), "{folders}");
    let tags = server.call_tool("list_tags", json!({})).await;
    assert!(tags.contains("intro") && tags.contains("project"), "{tags}");

    let welcome = server.call_tool("get_note_metadata", json!({ "path": "Welcome.md" })).await;
    assert!(welcome.contains("intro"), "{welcome}");
    assert!(welcome.contains("Backlinks"), "{welcome}");
    assert!(welcome.contains("projects/test.md"), "backlink from projects/test.md: {welcome}");

    let project = server.call_tool("get_note_metadata", json!({ "path": "projects/test.md" })).await;
    assert!(project.contains("Outgoing links") && project.contains("Welcome"), "{project}");
}

#[tokio::test]
async fn moves_and_deletes_notes() {
    let fixture = Fixture::new();
    let server = fixture.start(&[]).await;
    fixture.write("ci-test.md", "# CI Test");

    let moved = server.call_tool("move_note", json!({ "from": "ci-test.md", "to": "archive/ci-test.md" })).await;
    assert!(moved.contains("Moved"), "{moved}");
    assert!(!fixture.exists("ci-test.md") && fixture.exists("archive/ci-test.md"));

    let missing = server.call_tool("delete_note", json!({ "path": "archive/never-existed.md" })).await;
    assert_eq!(missing, "Note not found: archive/never-existed.md", "must not claim a delete that never happened");

    let deleted = server.call_tool("delete_note", json!({ "path": "archive/ci-test.md" })).await;
    assert!(deleted.contains("Deleted"), "{deleted}");
    assert!(!fixture.exists("archive/ci-test.md"));
}

#[tokio::test]
async fn extended_tools_and_section_targets_work_over_mcp() {
    let fixture = Fixture::new();
    let server = fixture.start(&[]).await;
    let path = "tool-check/note.md";
    server
        .call_tool(
            "write_note",
            json!({
                "path": path,
                "content": "---\ntags: [tool-check]\n---\n# Plan\nFindable content\n\n- [ ] Review this\n\nDetails ^detail\n",
            }),
        )
        .await;

    let search = server.call_tool_json("search_notes", json!({ "query": "Findable", "folder": "tool-check" })).await;
    assert_eq!(search["results"][0]["path"], path, "{search}");

    let batch = server.call_tool_json("read_notes", json!({ "paths": [path, "tool-check/missing.md"] })).await;
    assert_eq!(batch["notes"][0]["status"], "ok", "{batch}");
    assert_eq!(batch["notes"][1]["status"], "not_found", "{batch}");

    let properties =
        server.call_tool_json("update_note_properties", json!({ "path": path, "set": { "status": "done" } })).await;
    assert_eq!(properties["status"], "updated", "{properties}");
    assert!(fixture.read(path).starts_with("---\ntags: [tool-check]\nstatus: done\n---\n"), "{}", fixture.read(path));

    let outline = server.call_tool_json("get_note_outline", json!({ "path": path })).await;
    assert_eq!(outline["headings"][0]["heading"], json!(["Plan"]), "{outline}");
    assert_eq!(outline["blocks"][0]["id"], "detail", "{outline}");

    let tasks = server.call_tool_json("list_tasks", json!({ "folder": "tool-check" })).await;
    assert_eq!(tasks["results"][0]["text"], "Review this", "{tasks}");

    let block = server.call_tool("read_note", json!({ "path": path, "block": "detail" })).await;
    assert!(block.ends_with("Details"), "{block}");

    server
        .call_tool(
            "edit_note",
            json!({ "path": path, "heading": ["Plan"], "operation": "prepend", "content": "First" }),
        )
        .await;
    let read = server.call_tool("read_note", json!({ "path": path })).await;
    assert!(read.contains("# Plan\nFirst\nFindable"), "{read}");

    server.call_tool("delete_note", json!({ "path": path })).await;
    assert!(!fixture.exists(path));
}

/// The manual smoke test (`__tests__/e2e/smoke-test.md` in the TypeScript repo), automated.
#[tokio::test]
async fn smoke_test_script() {
    let fixture = Fixture::new();
    let server = fixture.start(&[]).await;

    server
        .call_tool(
            "write_note",
            json!({
                "path": "test-smoke/project.md",
                "content": "---\ntitle: Smoke Test Project\ntags: [smoke-test, project]\nstatus: active\n---\n\n# Smoke Test Project\n\nA project for testing MCP tools.\n\n## Links\n- [[test-smoke/notes]]\n- [[test-smoke/daily]]\n",
            }),
        )
        .await;
    server
        .call_tool(
            "write_note",
            json!({
                "path": "test-smoke/notes.md",
                "content": "---\ntags: [smoke-test, reference]\n---\n\n# Notes\n\nReference material for [[test-smoke/project]].\n",
            }),
        )
        .await;
    server
        .call_tool(
            "write_note",
            json!({
                "path": "test-smoke/daily.md",
                "content": "---\ntags: [smoke-test, daily]\n---\n\n# Daily Log\n\n- Ran smoke test on MCP tools\n\nSee [[test-smoke/project]] for the main project.\n\n#standup\n",
            }),
        )
        .await;

    let folders = server.call_tool("list_folders", json!({})).await;
    assert!(folders.contains("test-smoke"), "{folders}");
    let tags = server.call_tool("list_tags", json!({})).await;
    for tag in ["smoke-test", "project", "reference", "daily", "standup"] {
        assert!(tags.contains(tag), "missing tag {tag}: {tags}");
    }

    let smoke = server.call_tool("list_notes", json!({ "tag": "smoke-test" })).await;
    assert!(first_line(&smoke).starts_with("3 notes match"), "{smoke}");
    let reference = server.call_tool("list_notes", json!({ "tag": "reference" })).await;
    assert!(reference.contains("test-smoke/notes.md") && !reference.contains("test-smoke/daily.md"), "{reference}");
    let daily = server.call_tool("list_notes", json!({ "tag": "daily", "folder": "test-smoke" })).await;
    assert!(daily.contains("test-smoke/daily.md"), "{daily}");

    let project = server.call_tool("get_note_metadata", json!({ "path": "test-smoke/project.md" })).await;
    for expected in ["status", "test-smoke/notes", "test-smoke/daily", "test-smoke/notes.md", "test-smoke/daily.md"] {
        assert!(project.contains(expected), "missing {expected}: {project}");
    }

    server
        .call_tool(
            "edit_note",
            json!({ "path": "test-smoke/project.md", "content": "status: complete", "operation": "replace", "old_text": "status: active" }),
        )
        .await;
    assert!(fixture.read("test-smoke/project.md").contains("status: complete"));

    server.call_tool("move_note", json!({ "from": "test-smoke/notes.md", "to": "test-smoke/archive/notes.md" })).await;
    assert!(server.call_tool("list_folders", json!({})).await.contains("test-smoke/archive"));

    let recent = server
        .call_tool("list_notes", json!({ "modified_after": "2020-01-01", "sort_by": "modified", "limit": 3 }))
        .await;
    assert!(first_line(&recent).starts_with("Showing 3 of"), "{recent}");
    let invalid = server.call_tool("list_notes", json!({ "modified_after": "invalid-date" })).await;
    assert!(invalid.starts_with("Invalid date format: invalid-date."), "{invalid}");

    for path in ["test-smoke/project.md", "test-smoke/daily.md", "test-smoke/archive/notes.md"] {
        server.call_tool("delete_note", json!({ "path": path })).await;
    }
    let left = server.call_tool("list_notes", json!({ "folder": "test-smoke" })).await;
    assert!(!left.contains(".md"), "{left}");
}

// --- Protocol versions ---

// Regression for #18: requests speaking a newer protocol revision must get a
// JSON-RPC answer, never a crash or an internal error.
#[tokio::test]
async fn modern_protocol_requests_get_a_proper_answer() {
    let fixture = Fixture::new();
    let server = fixture.start(&[]).await;

    let discover = json!({
        "jsonrpc": "2.0",
        "id": 90,
        "method": "server/discover",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            },
            "clientInfo": { "name": "e2e-modern", "version": "1.0" },
        },
    });
    let response = server
        .http
        .post(server.url("/mcp"))
        .bearer_auth(AUTH)
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "server/discover")
        .json(&discover)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], -32601, "{body}");

    let modern = server.post_mcp(&initialize_request("2026-07-28")).await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(modern["result"]["protocolVersion"], "2025-11-25", "unknown versions get our newest: {modern}");
    for version in ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"] {
        let reply = server.post_mcp(&initialize_request(version)).await.unwrap().json::<Value>().await.unwrap();
        assert_eq!(reply["result"]["protocolVersion"], version);
    }
    assert_eq!(server.initialize_result["serverInfo"]["name"], "obsidian-crud-mcp");

    assert!(!server.call_tool("list_notes", json!({})).await.is_empty(), "still serving afterwards");
}

// --- Configuration ---

#[tokio::test]
async fn read_only_mode_hides_and_blocks_write_tools() {
    let fixture = Fixture::new();
    let server = fixture.start(&[("READ_ONLY", "true")]).await;
    assert!(server.logs().contains("READ_ONLY mode"), "should log READ_ONLY mode at startup:\n{}", server.logs());

    let tools = server.tool_names().await;
    for write in ["write_note", "edit_note", "delete_note", "move_note", "update_note_properties"] {
        assert!(!tools.contains(&write.to_owned()), "{write} should not be registered in READ_ONLY mode");
    }
    for read in [
        "read_note",
        "list_notes",
        "list_folders",
        "list_tags",
        "get_note_metadata",
        "read_notes",
        "search_notes",
        "get_note_outline",
        "list_tasks",
    ] {
        assert!(tools.contains(&read.to_owned()), "{read} should remain available in READ_ONLY mode");
    }

    let response = server.call_tool_raw("write_note", json!({ "path": "blocked.md", "content": "x" })).await;
    assert!(response["error"].is_object(), "write_note call should return an error: {response}");
    assert!(!fixture.exists("blocked.md"), "no file should be created when write is blocked");
}

#[cfg(unix)]
#[tokio::test]
async fn write_folders_are_enforced_after_resolving_symlinks() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.path("MCP")).unwrap();
    std::os::unix::fs::symlink(fixture.path("Welcome.md"), fixture.path("MCP/alias.md")).unwrap();
    let original = fixture.read("Welcome.md");
    let server = fixture.start(&[("WRITE_FOLDERS", "MCP")]).await;

    let response = server.call_tool_raw("write_note", json!({ "path": "MCP/alias.md", "content": "overwrite" })).await;
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert_eq!(fixture.read("Welcome.md"), original, "the symlink target outside MCP/ must be untouched");

    let outside = server.call_tool("write_note", json!({ "path": "elsewhere.md", "content": "x" })).await;
    assert!(outside.starts_with("Write access denied: 'elsewhere.md'"), "{outside}");
    assert!(!fixture.exists("elsewhere.md"));

    let saved = server.call_tool("write_note", json!({ "path": "MCP/allowed.md", "content": "allowed" })).await;
    assert!(saved.contains("Note saved"), "{saved}");
    assert_eq!(fixture.read("MCP/allowed.md"), "allowed");
}

#[tokio::test]
async fn mcp_instructions_are_appended() {
    let fixture = Fixture::new();
    let server = fixture.start(&[("MCP_INSTRUCTIONS", "inline-rule-XYZ")]).await;
    let instructions = server.initialize_result["instructions"].as_str().unwrap();
    assert!(instructions.contains(BASE_INSTRUCTIONS), "base instructions still present");
    assert!(instructions.contains("inline-rule-XYZ"), "inline env contents appended");
}

#[tokio::test]
async fn mcp_instructions_file_wins_over_the_inline_variable() {
    let fixture = Fixture::new();
    let file = fixture.data.path().join("agent-rules.md");
    std::fs::write(&file, "file-rule-ABC\nfile-rule-DEF").unwrap();
    let server = fixture
        .start(&[("MCP_INSTRUCTIONS", "inline-rule-XYZ"), ("MCP_INSTRUCTIONS_FILE", file.to_str().unwrap())])
        .await;
    let instructions = server.initialize_result["instructions"].as_str().unwrap();
    assert!(instructions.contains(BASE_INSTRUCTIONS), "base instructions still present");
    assert!(instructions.contains("file-rule-ABC\nfile-rule-DEF"), "file contents appended: {instructions}");
    assert!(!instructions.contains("inline-rule-XYZ"), "inline env ignored when file is set");
    assert!(server.logs().contains("ignoring MCP_INSTRUCTIONS env var"), "should warn about precedence");
}

#[tokio::test]
async fn an_unreadable_instructions_file_stops_startup() {
    let fixture = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_obsidian-crud-mcp"))
        .env_clear()
        .env("VAULT_PATH", fixture.vault.path())
        .env("DATA_DIR", fixture.data.path())
        .env("PORT", unused_port().to_string())
        .env("MCP_INSTRUCTIONS_FILE", fixture.data.path().join("missing.md"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("MCP_INSTRUCTIONS_FILE"));
}

// --- Restart ---

#[tokio::test]
async fn cold_restart_picks_up_changes_made_while_down() {
    let fixture = Fixture::new();
    let server = fixture.start(&[]).await;
    assert!(server.wait_for_log("Search index built", Duration::from_secs(10)).await, "{}", server.logs());
    let logs = server.terminate().await;
    assert!(logs.contains("Search index saved"), "should save the index on shutdown:\n{logs}");

    // Edits Obsidian makes while the server is down.
    fixture.write("new-while-down.md", "# Created while MCP was down\nfreshcontent");
    fixture.write("daily/2026-03-24.md", "# Daily Note\nUpdated while down uniqueword");
    std::fs::remove_file(fixture.path("projects/test.md")).unwrap();

    let server = fixture.start(&[]).await;
    assert!(server.logs().contains("Search metadata loaded"), "persisted index reused:\n{}", server.logs());
    // Connections are accepted while the rebuild runs in the background.
    assert!(server.wait_for_log("Search index built", Duration::from_secs(10)).await, "{}", server.logs());

    let fresh = server.call_tool("list_notes", json!({ "name": "new-while-down" })).await;
    assert!(fresh.contains("new-while-down.md"), "new note should be found: {fresh}");
    let all = server.call_tool("list_notes", json!({})).await;
    assert!(all.contains("daily/2026-03-24.md"), "{all}");
    assert!(!all.contains("projects/test.md"), "deleted note should not appear: {all}");
    let search = server.call_tool("search_notes", json!({ "query": "uniqueword" })).await;
    assert!(search.contains("daily/2026-03-24.md"), "updated content is searchable: {search}");
}

// --- No-auth mode: Host/Origin allowlist (DNS rebinding) ---

#[tokio::test]
async fn no_auth_mode_enforces_the_host_and_origin_allowlist() {
    let fixture = Fixture::new();
    let server = fixture.start(&[("MCP_AUTH_TOKEN", "")]).await;
    let port = server.port;
    let check = move |host: String, origin: Option<String>| {
        tokio::task::spawn_blocking(move || initialize_with_host(port, &host, origin.as_deref()))
    };
    assert_eq!(check("attacker.example".into(), None).await.unwrap(), 403, "forged (DNS-rebound) Host");
    assert_eq!(check("attacker.example@127.0.0.1".into(), None).await.unwrap(), 403, "userinfo smuggling");
    assert_eq!(check(format!("127.0.0.1:{port}"), None).await.unwrap(), 200, "genuine local Host");
    assert_eq!(
        check(format!("127.0.0.1:{port}"), Some("http://attacker.example".into())).await.unwrap(),
        403,
        "cross-origin browser request with a loopback Host"
    );
    assert_eq!(
        check(format!("127.0.0.1:{port}"), Some(format!("http://localhost:{port}"))).await.unwrap(),
        200,
        "local browser Origin such as MCP Inspector"
    );
    let oauth = server.http.get(server.url("/.well-known/oauth-protected-resource")).send().await.unwrap();
    assert_eq!(oauth.status(), StatusCode::NOT_FOUND, "no OAuth endpoints without a token");
}

#[tokio::test]
async fn no_auth_mode_honors_mcp_allowed_hosts() {
    let fixture = Fixture::new();
    let server = fixture.start(&[("MCP_AUTH_TOKEN", ""), ("MCP_ALLOWED_HOSTS", "myhost.local")]).await;
    let port = server.port;
    let check = move |host: &'static str| tokio::task::spawn_blocking(move || initialize_with_host(port, host, None));
    assert_eq!(check("myhost.local").await.unwrap(), 200);
    assert_eq!(check("attacker.example").await.unwrap(), 403);
}
