//! Tool behaviour end to end: calls go through the MCP server, so argument
//! validation, results and error reporting are what a client sees.

use std::collections::HashSet;
use std::io;
use std::path::Path;

use async_trait::async_trait;
use parking_lot::Mutex;
use rmcp::model::{CallToolRequestParams, ErrorCode};
use rmcp::service::{RunningService, ServiceError};
use rmcp::{RoleClient, RoleServer, ServiceExt};
use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;
use crate::mcp::{McpServer, ServerInfo};
use crate::search::{AiSearchOptions, IndexState};
use crate::vault::{LocalVault, NoteInfo, NoteListing, VaultError};

/// A local vault that records which notes are read and can be told to fail some reads.
struct ProbeVault {
    inner: LocalVault,
    reads: Mutex<Vec<String>>,
    failing: Mutex<HashSet<String>>,
}

#[async_trait]
impl VaultBackend for ProbeVault {
    async fn init(&self) -> Result<(), VaultError> {
        self.inner.init().await
    }

    async fn close(&self) {}

    async fn read_note(&self, path: &str) -> Result<Option<String>, VaultError> {
        self.reads.lock().push(path.to_owned());
        if self.failing.lock().contains(path) {
            return Err(VaultError::Io(io::Error::other("Read failed")));
        }
        self.inner.read_note(path).await
    }

    async fn write_note(&self, path: &str, content: &str) -> Result<bool, VaultError> {
        self.inner.write_note(path, content).await
    }

    async fn delete_note(&self, path: &str) -> Result<bool, VaultError> {
        self.inner.delete_note(path).await
    }

    async fn move_note(&self, from: &str, to: &str) -> Result<bool, VaultError> {
        self.inner.move_note(from, to).await
    }

    async fn get_metadata(&self, path: &str) -> Result<Option<NoteInfo>, VaultError> {
        self.inner.get_metadata(path).await
    }

    async fn list_notes_with_mtime(&self, folder: Option<&str>) -> Result<Vec<NoteListing>, VaultError> {
        self.inner.list_notes_with_mtime(folder).await
    }
}

struct Options {
    read_only: bool,
    write_folders: Option<Vec<String>>,
    content_cache_chars: usize,
    semantic: Option<Arc<AiSearchClient>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            read_only: false,
            write_folders: None,
            content_cache_chars: SearchIndex::DEFAULT_MAX_CONTENT_CHARS,
            semantic: None,
        }
    }
}

struct Fixture {
    _dir: TempDir,
    vault: Arc<ProbeVault>,
    index: Arc<SearchIndex>,
    ctx: Arc<ToolContext>,
    server: McpServer,
    client: RunningService<RoleClient, ()>,
    _service: RunningService<RoleServer, McpServer>,
}

async fn fixture(notes: &[(&str, &str)]) -> Fixture {
    fixture_with(notes, Options::default()).await
}

async fn fixture_with(notes: &[(&str, &str)], options: Options) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    // Written straight to disk so notes outside WRITE_FOLDERS can be set up too.
    for (path, content) in notes {
        write_file(dir.path(), path, content);
    }
    let vault = Arc::new(ProbeVault {
        inner: LocalVault::new(dir.path(), options.write_folders.clone()).unwrap(),
        reads: Mutex::default(),
        failing: Mutex::default(),
    });
    let index = Arc::new(SearchIndex::new(None, None, options.content_cache_chars));
    let context = || ToolContext {
        vault: vault.clone(),
        index: index.clone(),
        vault_name: "Test Vault".to_owned(),
        read_only: options.read_only,
        write_folders: options.write_folders.clone(),
        semantic: options.semantic.clone(),
    };
    let info = ServerInfo { name: "test".to_owned(), version: "0".to_owned(), instructions: String::new() };
    let server = McpServer::new(info, build_tools(context()));
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let serving = tokio::spawn(server.clone().serve(server_transport));
    let client = ().serve(client_transport).await.unwrap();
    let service = serving.await.unwrap().unwrap();
    Fixture { _dir: dir, ctx: Arc::new(context()), vault, index, server, client, _service: service }
}

fn write_file(root: &Path, path: &str, content: &str) {
    let full = root.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, content).unwrap();
}

impl Fixture {
    async fn respond(&self, name: &str, args: Value) -> Value {
        let request = CallToolRequestParams::new(name.to_owned())
            .with_arguments(args.as_object().expect("tool arguments are objects").clone());
        match self.client.call_tool(request).await {
            Ok(result) => json!({ "result": result }),
            Err(ServiceError::McpError(error)) => json!({ "error": error }),
            Err(error) => panic!("MCP transport failed: {error}"),
        }
    }

    /// The text of a successful call.
    async fn call(&self, name: &str, args: Value) -> String {
        let response = self.respond(name, args).await;
        let result = &response["result"];
        assert!(result.is_object() && result["isError"] != json!(true), "{name} failed: {response}");
        result["content"][0]["text"].as_str().expect("text content").to_owned()
    }

    async fn call_json(&self, name: &str, args: Value) -> Value {
        let text = self.call(name, args).await;
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} returned non-JSON ({e}): {text}"))
    }

    async fn rejects_params(&self, name: &str, args: Value) -> bool {
        self.respond(name, args).await["error"]["code"] == json!(ErrorCode::INVALID_PARAMS)
    }

    /// Note content on disk, without counting as a read.
    async fn read(&self, path: &str) -> Option<String> {
        self.vault.inner.read_note(path).await.unwrap()
    }

    async fn write(&self, path: &str, content: &str) {
        assert!(self.vault.inner.write_note(path, content).await.unwrap(), "write {path}");
    }

    fn sorted_reads(&self) -> Vec<String> {
        let mut reads = self.vault.reads.lock().clone();
        reads.sort();
        reads
    }

    fn tool_names(&self) -> Vec<&'static str> {
        self.server.tools().map(|t| t.name).collect()
    }
}

fn paths_of(page: &Value, key: &str) -> Vec<String> {
    page[key].as_array().unwrap().iter().map(|r| r["path"].as_str().unwrap().to_owned()).collect()
}

fn strings(value: &Value, field: &str) -> Vec<String> {
    value.as_array().unwrap().iter().map(|v| v[field].as_str().unwrap().to_owned()).collect()
}

fn headings(outline: &Value) -> Vec<Value> {
    outline["headings"].as_array().unwrap().iter().map(|h| h["heading"].clone()).collect()
}

#[tokio::test]
async fn every_tool_declares_all_four_behavior_hints_over_mcp() {
    for read_only in [false, true] {
        for has_semantic in [false, true] {
            let semantic = has_semantic.then(|| {
                Arc::new(AiSearchClient::new(AiSearchOptions {
                    account_id: "test".to_owned(),
                    token: "test".to_owned(),
                    namespace: "test".to_owned(),
                    instance: "test".to_owned(),
                    prefix: String::new(),
                    api_base: Some("http://127.0.0.1:1".to_owned()),
                }))
            });
            let f = fixture_with(&[], Options { read_only, semantic, ..Options::default() }).await;
            let listing = serde_json::to_value(f.client.list_all_tools().await.unwrap()).unwrap();
            let tools = listing.as_array().unwrap();
            let expected = [
                ("list_tasks", true, false, true, false),
                ("get_note_outline", true, false, true, false),
                ("search_notes", true, false, true, false),
                ("read_notes", true, false, true, false),
                ("read_note", true, false, true, false),
                ("list_notes", true, false, true, false),
                ("list_folders", true, false, true, false),
                ("list_tags", true, false, true, false),
                ("get_note_metadata", true, false, true, false),
                ("semantic_search", true, false, true, true),
                ("write_note", false, true, false, has_semantic),
                ("edit_note", false, true, false, has_semantic),
                ("delete_note", false, true, true, has_semantic),
                ("move_note", false, true, false, has_semantic),
                ("update_note_properties", false, true, true, has_semantic),
            ];
            let expected: Vec<_> = expected
                .into_iter()
                .filter(|(name, read, _, _, _)| (!read_only || *read) && (*name != "semantic_search" || has_semantic))
                .collect();
            assert_eq!(tools.len(), expected.len(), "read_only={read_only}, has_semantic={has_semantic}");
            for (name, read, destructive, idempotent, open_world) in expected {
                let tool = tools
                    .iter()
                    .find(|tool| tool["name"] == name)
                    .unwrap_or_else(|| panic!("{name} is missing from tools/list"));
                assert_eq!(
                    tool["annotations"],
                    json!({
                        "readOnlyHint": read,
                        "destructiveHint": destructive,
                        "idempotentHint": idempotent,
                        "openWorldHint": open_world,
                    }),
                    "{name}: read_only={read_only}, has_semantic={has_semantic}"
                );
            }
        }
    }
}

#[tokio::test]
async fn only_read_note_declares_a_result_size_limit() {
    const { assert!(READ_NOTE_MAX_RESULT_SIZE_CHARS > 50_000 && READ_NOTE_MAX_RESULT_SIZE_CHARS <= 500_000) };
    let f = fixture(&[]).await;
    let listing = serde_json::to_value(f.client.list_all_tools().await.unwrap()).unwrap();
    let tools = listing.as_array().unwrap();
    assert!(tools.iter().any(|t| t["name"] == "read_note"), "read_note is listed");
    for tool in tools {
        if tool["name"] == "read_note" {
            assert_eq!(tool["_meta"]["anthropic/maxResultSizeChars"], json!(READ_NOTE_MAX_RESULT_SIZE_CHARS));
        } else {
            assert!(tool.get("_meta").is_none(), "{} should not declare _meta", tool["name"]);
        }
    }
}

#[tokio::test]
async fn read_only_mode_hides_every_write_tool() {
    let f = fixture_with(&[], Options { read_only: true, ..Options::default() }).await;
    let names = f.tool_names();
    for write_tool in WRITE_TOOLS {
        assert!(!names.contains(&write_tool), "{write_tool} should be hidden: {names:?}");
    }
    assert!(names.contains(&"read_note") && names.contains(&"list_notes"), "{names:?}");
    assert!(!names.contains(&"semantic_search"), "semantic_search needs a client: {names:?}");
}

#[tokio::test]
async fn write_tool_descriptions_name_the_writable_folders() {
    let options = Options { write_folders: Some(vec!["MCP".to_owned(), "Inbox".to_owned()]), ..Options::default() };
    let f = fixture_with(&[], options).await;
    let write_note = f.server.tools().find(|t| t.name == "write_note").unwrap();
    assert!(
        write_note.description.ends_with(" Writes are only allowed inside: MCP/, Inbox/."),
        "{}",
        write_note.description
    );
}

#[test]
fn tag_matches_ignores_hash_and_case_and_includes_nested_tags() {
    let tags = |list: &[&str]| list.iter().map(|t| t.to_string()).collect::<Vec<_>>();
    assert!(tag_matches(&tags(&["project"]), "#project"));
    assert!(tag_matches(&tags(&["Project"]), "project"));
    assert!(tag_matches(&tags(&["project/sub"]), "project"));
    assert!(tag_matches(&tags(&["project/sub/deep"]), "PROJECT/Sub"));
    assert!(!tag_matches(&tags(&["projects"]), "project"));
    assert!(!tag_matches(&tags(&["project"]), "project/sub"));
    assert!(!tag_matches(&[], "project"));
}

#[test]
fn parses_iso_dates_as_utc() {
    let ms = |s: &str| parse_date(s).unwrap_or_else(|| panic!("{s} should parse"));
    let midnight = 1_774_396_800_000.0; // 2026-03-25T00:00:00Z
    assert_eq!(ms("2026-03-25"), midnight);
    assert_eq!(ms("2026-03-25T10:00"), midnight + 10.0 * 3_600_000.0);
    assert_eq!(ms("2026-03-25T10:00:30.5"), midnight + 10.0 * 3_600_000.0 + 30_500.0);
    assert_eq!(ms("2026-03-25T10:00:00Z"), midnight + 10.0 * 3_600_000.0);
    assert_eq!(ms("2026-03-25T12:00+02:00"), midnight + 10.0 * 3_600_000.0);
    assert_eq!(ms("2026-03"), ms("2026-03-01"));
    assert_eq!(ms("2026"), ms("2026-01-01"));
    for bad in ["yesterday", "2026-13-01", "25/03/2026", ""] {
        assert_eq!(parse_date(bad), None, "{bad:?}");
    }
}

#[test]
fn blank_line_padding_counts_existing_breaks() {
    assert_eq!(with_blank_line("a", "\n"), "a\n\n");
    assert_eq!(with_blank_line("a\n", "\n"), "a\n\n");
    assert_eq!(with_blank_line("a\n\n", "\n"), "a\n\n");
    assert_eq!(with_blank_line("a\r\n", "\r\n"), "a\r\n\r\n");
    assert_eq!(with_blank_line("a\r\n\r\n", "\r\n"), "a\r\n\r\n");
}

#[tokio::test]
async fn vault_errors_are_tool_failures_not_protocol_errors() {
    let f = fixture(&[]).await;
    let direct =
        read_note(f.ctx.clone(), ReadNoteParams { path: "../outside.md".into(), heading: None, block: None }).await;
    assert!(matches!(direct, Err(ToolError::Vault(VaultError::InvalidPath(_)))), "{direct:?}");

    let response = f.respond("read_note", json!({ "path": "../outside.md" })).await;
    assert_eq!(response["result"]["isError"], json!(true), "{response}");
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("Tool 'read_note' execution failed: Invalid note path"), "{text}");
}

#[tokio::test]
async fn missing_required_arguments_are_invalid_params() {
    let f = fixture(&[]).await;
    assert!(f.rejects_params("read_note", json!({})).await);
    assert!(f.rejects_params("edit_note", json!({ "path": "a.md", "operation": "rewrite", "content": "x" })).await);
    assert!(f.rejects_params("read_note", json!({ "path": "a.md", "block": "not valid" })).await);
}

#[tokio::test]
async fn get_note_metadata_renders_nested_frontmatter_as_json() {
    let f =
        fixture(&[("note.md", "---\ntitle: Plain\nnested:\n  keep: [one, two]\nlist: [a, b]\ncount: 3\n---\nBody")])
            .await;
    let text = f.call("get_note_metadata", json!({ "path": "note.md" })).await;
    for line in ["  title: Plain", r#"  nested: {"keep":["one","two"]}"#, r#"  list: ["a","b"]"#, "  count: 3"] {
        assert!(text.lines().any(|l| l == line), "missing {line:?} in:\n{text}");
    }
}

#[tokio::test]
async fn get_note_metadata_lists_tags_links_and_backlinks() {
    let f = fixture(&[("a.md", "#topic see [[b]]"), ("b.md", "Body")]).await;
    f.index.update("a.md", "#topic see [[b]]", Some(1.0));
    let text = f.call("get_note_metadata", json!({ "path": "b.md" })).await;
    assert!(text.starts_with("**b.md**\nSize: 4 bytes\n"), "{text}");
    assert!(text.contains("\nBacklinks: a.md"), "{text}");
    let text = f.call("get_note_metadata", json!({ "path": "a.md" })).await;
    assert!(text.contains("\nTags: #topic") && text.contains("\nOutgoing links: b"), "{text}");
    assert_eq!(f.call("get_note_metadata", json!({ "path": "nope.md" })).await, "Note not found: nope.md");
}

#[tokio::test]
async fn targeted_append_stays_out_of_a_following_setext_heading() {
    let f = fixture(&[("note.md", "# A\ntext\n\nB\n===\nbody\n")]).await;
    f.call("edit_note", json!({ "path": "note.md", "heading": ["A"], "content": "new" })).await;
    assert_eq!(f.read("note.md").await.unwrap(), "# A\ntext\n\nnew\n\nB\n===\nbody\n");
    let outline = f.call_json("get_note_outline", json!({ "path": "note.md" })).await;
    assert_eq!(headings(&outline), [json!(["A"]), json!(["B"])]);
}

#[tokio::test]
async fn targeted_prepend_stays_out_of_a_following_setext_heading() {
    let f = fixture(&[("note.md", "# A\nB\n---\nbody\n"), ("empty.md", "# A\nB\n===\n")]).await;
    let args = |path: &str| json!({ "path": path, "heading": ["A"], "operation": "prepend", "content": "new" });
    f.call("edit_note", args("note.md")).await;
    assert_eq!(f.read("note.md").await.unwrap(), "# A\nnew\n\nB\n---\nbody\n");
    f.call("edit_note", args("empty.md")).await;
    assert_eq!(f.read("empty.md").await.unwrap(), "# A\nnew\n\nB\n===\n");
}

#[tokio::test]
async fn targeted_append_before_an_atx_heading_keeps_the_layout() {
    let f = fixture(&[("note.md", "# A\ntext\n# B\nbody\n")]).await;
    f.call("edit_note", json!({ "path": "note.md", "heading": ["A"], "content": "new" })).await;
    assert_eq!(f.read("note.md").await.unwrap(), "# A\ntext\nnew\n# B\nbody\n");
}

#[tokio::test]
async fn block_replace_refuses_to_empty_the_block() {
    let content = "First\n\npara ^id\n";
    let f = fixture(&[("note.md", content)]).await;
    let text = f
        .call(
            "edit_note",
            json!({ "path": "note.md", "block": "id", "operation": "replace", "old_text": "para", "content": "" }),
        )
        .await;
    assert!(text.contains("empty"), "{text}");
    assert_eq!(f.read("note.md").await.unwrap(), content);
}

#[tokio::test]
async fn read_note_rejects_ambiguous_and_missing_targets() {
    let f = fixture(&[("note.md", "# Same\nOne\n# Same\nTwo\n\nA ^dup\n\nB ^dup\n")]).await;
    let read = |args: Value| f.call("read_note", args);
    assert!(read(json!({ "path": "note.md", "heading": ["Same"] })).await.contains("ambiguous"));
    assert!(read(json!({ "path": "note.md", "block": "dup" })).await.contains("ambiguous"));
    assert!(read(json!({ "path": "note.md", "heading": ["Missing"] })).await.contains("not found"));
    assert!(read(json!({ "path": "note.md", "block": "missing" })).await.contains("not found"));
    assert_eq!(read(json!({ "path": "gone.md" })).await, "Note not found: gone.md");
}

#[tokio::test]
async fn read_notes_returns_contents_missing_paths_and_bounded_pages() {
    let large = "x".repeat(5000);
    let f = fixture(&[("a.md", "Alpha"), ("empty.md", ""), ("large.md", &large)]).await;
    let result = f.call_json("read_notes", json!({ "paths": ["a.md", "missing.md", "empty.md", "gone.md"] })).await;
    assert_eq!(result["notes"][0]["content"], "Alpha");
    assert_eq!(result["notes"][1]["status"], "not_found");
    assert_eq!(result["notes"][2]["content"], "");
    assert!(result["notes"][0]["url"].as_str().unwrap().starts_with("obsidian://open"));
    assert_eq!(result["missing_paths"], json!(["missing.md", "gone.md"]));

    let limited = f.call("read_notes", json!({ "paths": ["large.md", "a.md"], "max_chars": 1024 })).await;
    assert!(limited.chars().count() <= 1024, "{} chars", limited.chars().count());
    let page: Value = serde_json::from_str(&limited).unwrap();
    assert_eq!(page["notes"][0]["status"], "truncated");
    assert_eq!(page["omitted_paths"], json!(["a.md"]));

    let page = f.call_json("read_notes", json!({ "paths": ["missing.md", "large.md"], "max_chars": 1024 })).await;
    assert_eq!(page["missing_paths"], json!(["missing.md"]));
    let shown = page["notes"][1]["content"].as_str().unwrap().chars().count();
    assert_eq!(page["notes"][1]["status"], "truncated");
    assert!(shown > 0);
    assert_eq!(page["notes"][1]["omitted_chars"], json!(5000 - shown));
}

#[tokio::test]
async fn read_notes_truncates_on_character_boundaries() {
    let f = fixture(&[]).await;
    for pad in 0..40 {
        let content = format!("{}{}", "x".repeat(800 + pad), "😀".repeat(200));
        f.write("emoji.md", &content).await;
        let text = f.call("read_notes", json!({ "paths": ["emoji.md"], "max_chars": 1024 })).await;
        assert!(text.chars().count() <= 1024, "pad {pad}: {} chars", text.chars().count());
        let page: Value = serde_json::from_str(&text).unwrap();
        let note = &page["notes"][0];
        let shown = note["content"].as_str().unwrap();
        assert_eq!(note["status"], "truncated", "pad {pad}");
        assert!(content.starts_with(shown), "pad {pad}: content is not a prefix");
        assert_eq!(note["omitted_chars"], json!(800 + pad + 200 - shown.chars().count()), "pad {pad}");
    }
}

#[tokio::test]
async fn read_notes_isolates_invalid_paths_and_failed_reads() {
    let f = fixture(&[("good.md", "Good"), ("tasks.md", "- [\t] Tab task\n")]).await;
    f.vault.failing.lock().insert("failed.md".to_owned());
    let result =
        f.call_json("read_notes", json!({ "paths": ["failed.md", "bad:name.md", "../outside.md", "good.md"] })).await;
    assert_eq!(strings(&result["notes"], "status"), ["error", "error", "error", "ok"]);
    assert_eq!(result["notes"][3]["content"], "Good");

    let tasks = f.call_json("list_tasks", json!({})).await;
    assert_eq!(tasks["results"][0]["text"], "Tab task");
    assert_eq!(tasks["results"][0]["completed"], false);
}

#[tokio::test]
async fn read_notes_rejects_out_of_range_arguments() {
    let f = fixture(&[("c.md", "C")]).await;
    assert!(f.rejects_params("read_notes", json!({ "paths": ["c.md"], "max_chars": 1 })).await);
    assert!(f.rejects_params("read_notes", json!({ "paths": [] })).await);
    let too_many: Vec<String> = (0..21).map(|i| format!("n{i}.md")).collect();
    assert!(f.rejects_params("read_notes", json!({ "paths": too_many })).await);
}

#[tokio::test]
async fn search_notes_matches_literally_with_tag_filters_and_resumable_lines() {
    let f = fixture(&[
        ("work/a.md", "---\r\ntags: [project]\r\n---\r\nA.B first\r\na.b second\r\naxb no"),
        ("work/b.md", "#project\na.b third"),
        ("workshop/c.md", "#project\na.b outside"),
    ])
    .await;
    let args = json!({ "query": "a.b", "folder": "work", "tag": "project", "limit": 1 });
    let with_cursor = |cursor: &Value| {
        let mut args = args.clone();
        args["cursor"] = cursor.clone();
        args
    };
    let first = f.call_json("search_notes", args.clone()).await;
    assert_eq!(first["results"][0]["path"], "work/a.md");
    assert_eq!(first["results"][0]["line"], 4);
    assert!(first["results"][0]["text"].as_str().unwrap().contains("A.B first"));
    assert!(first["next_cursor"].is_string());

    let second = f.call_json("search_notes", with_cursor(&first["next_cursor"])).await;
    assert_eq!(second["results"][0]["line"], 5);
    let third = f.call_json("search_notes", with_cursor(&second["next_cursor"])).await;
    assert_eq!(third["results"][0]["path"], "work/b.md");
    assert_eq!(f.index.size(), 0, "search must work before the metadata index is populated");

    let mut changed = with_cursor(&first["next_cursor"]);
    changed["query"] = json!("changed");
    let wrong = f.call_json("search_notes", changed).await;
    assert!(wrong["error"].as_str().unwrap().contains("cursor"), "{wrong}");
}

#[tokio::test]
async fn search_notes_continues_after_a_page_without_matches() {
    let f = fixture(&[("a.md", "nothing"), ("b.md", "needle")]).await;
    let first = f.call_json("search_notes", json!({ "query": "needle", "max_notes": 1 })).await;
    assert_eq!(first["results"], json!([]));
    assert_eq!(first["scanned_notes"], 1);
    let next =
        f.call_json("search_notes", json!({ "query": "needle", "max_notes": 1, "cursor": first["next_cursor"] })).await;
    assert_eq!(next["results"][0]["path"], "b.md");
    assert_eq!(next["next_cursor"], Value::Null);
}

#[tokio::test]
async fn search_notes_rejects_blank_or_multiline_queries() {
    let f = fixture(&[]).await;
    for args in [
        json!({ "query": " " }),
        json!({ "query": "a\nb" }),
        json!({ "query": "x", "limit": -1 }),
        json!({ "query": "x", "limit": 51 }),
        json!({ "query": "" }),
    ] {
        assert!(f.rejects_params("search_notes", args.clone()).await, "{args}");
    }
}

#[tokio::test]
async fn search_excerpts_count_characters_and_respect_boundaries() {
    let f = fixture(&[
        ("unicode.md", &format!("{}needle{}", "İ".repeat(200), "z".repeat(200))),
        ("tasks.md", "- [ ]\n  Buy milk\n"),
    ])
    .await;
    let search = f.call_json("search_notes", json!({ "query": "needle" })).await;
    assert_eq!(search["results"][0]["text"], format!("…{}needle{}…", "İ".repeat(80), "z".repeat(80)));
    let tasks = f.call_json("list_tasks", json!({})).await;
    assert_eq!(tasks["results"][0]["text"], "Buy milk");
}

#[tokio::test]
async fn search_excerpts_give_one_match_per_line_across_line_endings() {
    let long = format!("{}needle{}\nend needle", "x".repeat(200), "y".repeat(200));
    let f =
        fixture(&[("crlf.md", "a\r\nneedle needle\r\n\r\nneedle"), ("cr.md", "needle\rx\rneedle"), ("long.md", &long)])
            .await;
    let page = f.call_json("search_notes", json!({ "query": "needle", "limit": 50 })).await;
    let by_path = |path: &str| -> Vec<(u64, String)> {
        page["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["path"] == path)
            .map(|r| (r["line"].as_u64().unwrap(), r["text"].as_str().unwrap().to_owned()))
            .collect()
    };
    assert_eq!(by_path("cr.md"), [(1, "needle".to_owned()), (3, "needle".to_owned())]);
    assert_eq!(by_path("crlf.md"), [(2, "needle needle".to_owned()), (4, "needle".to_owned())]);
    let expected = format!("…{}needle{}…", "x".repeat(80), "y".repeat(80));
    assert_eq!(by_path("long.md"), [(1, expected), (2, "end needle".to_owned())]);
}

#[tokio::test]
async fn scans_report_oversized_and_unreadable_notes_and_keep_going() {
    let f = fixture(&[("a.md", &"x".repeat(1_000_001)), ("b.md", "Failed"), ("c.md", "needle")]).await;
    f.vault.failing.lock().insert("b.md".to_owned());
    let page = f.call_json("search_notes", json!({ "query": "needle", "max_notes": 2 })).await;
    assert_eq!(strings(&page["skipped_notes"], "reason"), ["exceeds_1000000_char_scan_limit", "read_error"]);
    assert!(page["next_cursor"].is_string());
    let next =
        f.call_json("search_notes", json!({ "query": "needle", "max_notes": 2, "cursor": page["next_cursor"] })).await;
    assert_eq!(next["results"][0]["path"], "c.md");
    assert_eq!(next["next_cursor"], Value::Null);
}

#[tokio::test]
async fn scans_take_candidates_from_a_ready_index_and_read_only_those() {
    let notes = [
        ("work/tagged.md", "#project\nneedle here"),
        ("work/plain.md", "needle but no tag"),
        ("work/stale.md", "needle, tag removed on disk"),
        ("play/tagged.md", "#project\nneedle elsewhere"),
    ];
    // Content cache off, so every note served is a visible disk read.
    let f = fixture_with(&notes, Options { content_cache_chars: 0, ..Options::default() }).await;
    for (path, content) in notes {
        f.index.update(path, content, Some(1.0));
    }
    // The index answers the filter, even where the disk disagrees.
    f.index.update("work/stale.md", "#project\nneedle, tag removed on disk", Some(1.0));
    f.index.set_state(IndexState::Ready);

    let args = json!({ "query": "needle", "folder": "work", "tag": "project" });
    let page = f.call_json("search_notes", args.clone()).await;
    assert_eq!(paths_of(&page, "results"), ["work/stale.md", "work/tagged.md"]);
    assert_eq!(f.sorted_reads(), ["work/stale.md", "work/tagged.md"]);
    assert_eq!(page["scanned_notes"], 2);
    assert_eq!(page["next_cursor"], Value::Null);

    // A note the index still lists but that is gone from disk is reported, not fatal.
    assert!(f.vault.inner.delete_note("work/tagged.md").await.unwrap());
    let after = f.call_json("search_notes", args).await;
    assert_eq!(after["skipped_notes"], json!([{ "path": "work/tagged.md", "reason": "not_found_or_unreadable" }]));
    assert_eq!(after["results"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn scan_pages_keep_path_order_and_resume_at_the_cursor() {
    let notes: Vec<(String, String)> = (0..40).map(|i| (format!("n{i:02}.md"), format!("needle {i}"))).collect();
    let refs: Vec<(&str, &str)> = notes.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
    let f = fixture(&refs).await;
    let expected: Vec<String> = notes.iter().map(|(p, _)| p.clone()).collect();
    // Pages end on the result limit in one run and on max_notes in the other.
    for (limit, max_notes, expected_pages) in [(7, 9, 6), (50, 9, 5)] {
        let mut seen: Vec<String> = Vec::new();
        let mut cursor = Value::Null;
        let mut pages = 0;
        loop {
            let mut args = json!({ "query": "needle", "limit": limit, "max_notes": max_notes });
            if !cursor.is_null() {
                args["cursor"] = cursor;
            }
            let page = f.call_json("search_notes", args).await;
            seen.extend(paths_of(&page, "results"));
            pages += 1;
            cursor = page["next_cursor"].clone();
            if cursor.is_null() {
                break;
            }
        }
        assert_eq!(seen, expected, "limit={limit} max_notes={max_notes}");
        assert_eq!(pages, expected_pages, "limit={limit} max_notes={max_notes}");
    }
}

#[tokio::test]
async fn scans_serve_cached_content_and_read_each_note_once() {
    let f = fixture(&[("a.md", "needle one"), ("b.md", "needle two"), ("c.md", "nothing")]).await;
    // Index still building: every note comes from disk and is cached on the way.
    let page = f.call_json("search_notes", json!({ "query": "needle" })).await;
    assert_eq!(page["results"].as_array().unwrap().len(), 2);
    assert_eq!(f.sorted_reads(), ["a.md", "b.md", "c.md"]);
    assert_eq!(f.index.get_content("c.md").as_deref(), Some("nothing"));

    let page = f.call_json("search_notes", json!({ "query": "needle" })).await;
    assert_eq!(page["results"].as_array().unwrap().len(), 2);
    assert_eq!(f.sorted_reads(), ["a.md", "b.md", "c.md"], "second scan read nothing from disk");

    // A write through the tools refreshes the cache; search sees it without a disk read.
    f.call("write_note", json!({ "path": "c.md", "content": "needle three" })).await;
    let page = f.call_json("search_notes", json!({ "query": "needle" })).await;
    assert_eq!(paths_of(&page, "results"), ["a.md", "b.md", "c.md"]);
    assert_eq!(f.sorted_reads(), ["a.md", "b.md", "c.md"]);

    // A delete drops it, so the note is neither served nor read.
    f.call("delete_note", json!({ "path": "b.md" })).await;
    let page = f.call_json("search_notes", json!({ "query": "needle" })).await;
    assert_eq!(paths_of(&page, "results"), ["a.md", "c.md"]);
    assert_eq!(f.sorted_reads(), ["a.md", "b.md", "b.md", "c.md"], "delete_note reads before deleting");
}

#[tokio::test]
async fn update_properties_keeps_typed_values_and_the_exact_body() {
    let body = "\r\n# Note\r\nDo not change this.\r\n";
    let original =
        format!("---\r\n# Keep me\r\ntitle: 'Original'\r\nstatus: draft # workflow\r\nold: remove\r\n---\r\n{body}");
    let f = fixture(&[("note.md", &original)]).await;
    let result = f
        .call_json(
            "update_note_properties",
            json!({
                "path": "note.md",
                "set": { "status": "done", "count": 3, "active": true, "tags": ["project", "review"] },
                "remove": ["old"],
            }),
        )
        .await;
    assert_eq!(result["status"], "updated");
    assert_eq!(result["url"], "obsidian://open?vault=Test%20Vault&file=note");
    let content = f.read("note.md").await.unwrap();
    assert!(content.ends_with(&format!("---\r\n{body}")), "{content:?}");
    for kept in ["# Keep me\r\n", "title: 'Original'\r\n", "status: done", "count: 3"] {
        assert!(content.contains(kept), "missing {kept:?} in {content:?}");
    }
    assert!(!content.contains("old: remove"), "{content:?}");
    let meta = f.vault.get_metadata("note.md").await.unwrap().unwrap();
    assert_eq!(meta.metadata.frontmatter["active"], true);
    assert_eq!(meta.metadata.frontmatter["tags"], json!(["project", "review"]));
    assert_eq!(f.index.get_tags("note.md"), ["project", "review"]);
}

#[tokio::test]
async fn update_properties_rejects_unsafe_input_without_writing() {
    let invalid =
        ["---\na: [broken\n---\nBody", "---\na: 1\na: 2\n---\nBody", "---\n- item\n---\nBody", "---\na: value\nBody"];
    let names: Vec<String> = (0..invalid.len()).map(|i| format!("bad{i}.md")).collect();
    let mut notes: Vec<(&str, &str)> = names.iter().map(String::as_str).zip(invalid).collect();
    notes.push(("plain.md", "Plain body"));
    let f = fixture(&notes).await;
    for (name, original) in names.iter().zip(invalid) {
        let result = f.call_json("update_note_properties", json!({ "path": name, "set": { "status": "done" } })).await;
        assert!(result["error"].is_string(), "{name}: {result}");
        assert_eq!(f.read(name).await.unwrap(), original, "{name}");
    }
    let unchanged = f.call_json("update_note_properties", json!({ "path": "plain.md", "remove": ["missing"] })).await;
    assert_eq!(unchanged["status"], "unchanged");
    assert_eq!(f.read("plain.md").await.unwrap(), "Plain body");
    let both = f
        .call_json(
            "update_note_properties",
            json!({ "path": "plain.md", "set": { "status": "done" }, "remove": ["status"] }),
        )
        .await;
    assert!(both["error"].is_string(), "{both}");
    let missing = f.call_json("update_note_properties", json!({ "path": "nope.md", "set": { "a": 1 } })).await;
    assert_eq!(missing["error"], "Note not found: nope.md");
}

#[tokio::test]
async fn update_properties_validates_arguments() {
    let f = fixture(&[("plain.md", "Body")]).await;
    for args in [
        json!({ "path": "plain.md" }),
        json!({ "path": "plain.md", "set": { " ": 1 } }),
        json!({ "path": "plain.md", "set": { "nested": { "a": 1 } } }),
        json!({ "path": "plain.md", "set": { "list": [[1]] } }),
    ] {
        assert!(f.rejects_params("update_note_properties", args.clone()).await, "{args}");
    }
}

#[tokio::test]
async fn update_properties_honours_write_access() {
    let f = fixture_with(&[], Options { read_only: true, ..Options::default() }).await;
    assert!(!f.tool_names().contains(&"update_note_properties"));

    let scoped = Options { write_folders: Some(vec!["Inbox".to_owned()]), ..Options::default() };
    let f = fixture_with(&[("outside.md", "Body")], scoped).await;
    let result =
        f.call_json("update_note_properties", json!({ "path": "outside.md", "set": { "status": "done" } })).await;
    assert!(result["error"].as_str().unwrap().contains("denied"), "{result}");
    assert_eq!(f.read("outside.md").await.unwrap(), "Body");
}

#[tokio::test]
async fn property_edits_keep_aliases_and_untouched_empty_frontmatter() {
    let alias = "---\nstatus: &s todo\nother: *s\n---\nBody";
    let empty = "---\n# Keep this\n---\nBody";
    let f =
        fixture(&[("alias.md", alias), ("empty.md", empty), ("tags.md", "---\ntags: [one, two] # keep me\n---\nBody")])
            .await;
    let result =
        f.call_json("update_note_properties", json!({ "path": "alias.md", "set": { "status": "done" } })).await;
    assert_eq!(result["status"], "updated");
    let meta = f.vault.get_metadata("alias.md").await.unwrap().unwrap();
    assert_eq!(Value::Object(meta.metadata.frontmatter), json!({ "status": "done", "other": "todo" }));

    let result = f.call_json("update_note_properties", json!({ "path": "empty.md", "remove": ["missing"] })).await;
    assert_eq!(result["status"], "unchanged");
    assert_eq!(f.read("empty.md").await.unwrap(), empty);

    f.call_json("update_note_properties", json!({ "path": "tags.md", "set": { "tags": "three" } })).await;
    let meta = f.vault.get_metadata("tags.md").await.unwrap().unwrap();
    assert_eq!(meta.metadata.frontmatter["tags"], "three");
}

#[tokio::test]
async fn property_creation_keeps_the_body_and_nested_values() {
    let body = "\u{FEFF}# Plain\r\nKeep this.\r\n";
    let f = fixture(&[("plain.md", body), ("nested.md", "---\nnested:\n  keep: [one, two]\n---\nBody")]).await;
    f.call_json(
        "update_note_properties",
        json!({ "path": "plain.md", "set": { "optional": null, "aliases": ["Alpha", "Beta"] } }),
    )
    .await;
    let content = f.read("plain.md").await.unwrap();
    assert!(content.starts_with("\u{FEFF}---\r\n"), "{content:?}");
    assert!(content.ends_with(body.trim_start_matches('\u{FEFF}')), "{content:?}");
    let meta = f.vault.get_metadata("plain.md").await.unwrap().unwrap().metadata.frontmatter;
    assert_eq!(meta["optional"], Value::Null);
    assert_eq!(meta["aliases"], json!(["Alpha", "Beta"]));

    f.call_json("update_note_properties", json!({ "path": "nested.md", "set": { "status": "done" } })).await;
    let meta = f.vault.get_metadata("nested.md").await.unwrap().unwrap().metadata.frontmatter;
    assert_eq!(meta["nested"], json!({ "keep": ["one", "two"] }));
}

#[tokio::test]
async fn outline_lists_heading_paths_and_blocks_outside_code_and_frontmatter() {
    let content = "---\nsummary: fake\n---\n# Project\nIntro\n\n## Notes\nA paragraph\nover two lines. ^detail\n\n```md\n# Fake\nignore ^fake\n```\n\nNext\n====\nEnd\n";
    let f = fixture(&[("note.md", content)]).await;
    let outline = f.call_json("get_note_outline", json!({ "path": "note.md" })).await;
    assert_eq!(headings(&outline), [json!(["Project"]), json!(["Project", "Notes"]), json!(["Next"])]);
    assert_eq!(outline["headings"][0]["start_line"], 4);
    assert_eq!(outline["headings"][0]["end_line"], 15);
    assert_eq!(outline["headings"][1]["start_line"], 7);
    assert_eq!(outline["blocks"], json!([{ "id": "detail", "start_line": 8, "end_line": 9 }]));
    assert_eq!(outline["url"], "obsidian://open?vault=Test%20Vault&file=note");
    let missing = f.call_json("get_note_outline", json!({ "path": "nope.md" })).await;
    assert_eq!(missing["error"], "Note not found: nope.md");
}

#[tokio::test]
async fn setext_hashes_stay_literal_and_block_ids_work_in_lists_and_quotes() {
    let f = fixture(&[("note.md", "Heading #\n---\nContent\n\n- Item ^list-id\n\n> Quote ^quote-id\n")]).await;
    let outline = f.call_json("get_note_outline", json!({ "path": "note.md" })).await;
    assert_eq!(outline["headings"][0]["heading"], json!(["Heading #"]));
    assert_eq!(strings(&outline["blocks"], "id"), ["list-id", "quote-id"]);
    assert!(f.call("read_note", json!({ "path": "note.md", "block": "list-id" })).await.contains("Item"));
    f.call(
        "edit_note",
        json!({ "path": "note.md", "block": "quote-id", "operation": "replace", "old_text": "Quote", "content": "Changed" }),
    )
    .await;
    assert!(f.read("note.md").await.unwrap().contains("> Changed ^quote-id"));
}

#[tokio::test]
async fn list_tasks_finds_nested_tasks_outside_code_and_frontmatter() {
    let content = [
        "---",
        "example: '- [ ] metadata'",
        "---",
        "# Work",
        "- [ ] Open",
        "  + [X] Nested done",
        "1. [x] Ordered done",
        "",
        "~~~~md",
        "- [ ] Code",
        "~~~",
        "- [ ] Still code",
        "~~~~",
        "",
        "    - [ ] Indented code",
        "",
        "- \\[ ] Escaped",
        "- [-] Custom",
        "* [ ] Last",
    ]
    .join("\r\n");
    let f = fixture(&[("work/tasks.md", &content), ("elsewhere.md", "- [ ] Outside")]).await;
    let all = f.call_json("list_tasks", json!({ "folder": "work", "status": "all" })).await;
    assert_eq!(strings(&all["results"], "text"), ["Open", "Nested done", "Ordered done", "Last"]);
    let lines: Vec<u64> = all["results"].as_array().unwrap().iter().map(|t| t["line"].as_u64().unwrap()).collect();
    assert_eq!(lines, [5, 6, 7, 19]);

    let todo = f.call_json("list_tasks", json!({ "folder": "work" })).await;
    assert_eq!(strings(&todo["results"], "text"), ["Open", "Last"]);

    let done = f.call_json("list_tasks", json!({ "folder": "work", "status": "completed", "limit": 1 })).await;
    assert_eq!(done["results"][0]["text"], "Nested done");
    let next = f
        .call_json(
            "list_tasks",
            json!({ "folder": "work", "status": "completed", "limit": 1, "cursor": done["next_cursor"] }),
        )
        .await;
    assert_eq!(next["results"][0]["text"], "Ordered done");
}

#[tokio::test]
async fn long_task_text_is_cut_at_a_character_boundary() {
    let f = fixture(&[("t.md", &format!("- [ ] {}", "é".repeat(600)))]).await;
    let page = f.call_json("list_tasks", json!({})).await;
    assert_eq!(page["results"][0]["text"], "é".repeat(500));
    assert_eq!(page["results"][0]["truncated"], true);
}

#[tokio::test]
async fn heading_and_block_targets_limit_reads_and_edits() {
    let content = "# First\r\n## Notes\r\nKeep\r\n# Second\r\n## Notes\r\nChange\r\n# Last\r\nParagraph one\r\nline two. ^detail\r\n";
    let f = fixture(&[("note.md", content)]).await;
    let read = f.call("read_note", json!({ "path": "note.md", "heading": ["Second", "Notes"] })).await;
    assert!(read.contains("Change") && !read.contains("Keep") && !read.contains("Paragraph one"), "{read}");

    let append =
        f.call("edit_note", json!({ "path": "note.md", "heading": ["Second", "Notes"], "content": "Added" })).await;
    assert!(append.starts_with("Note edited (append): note.md"), "{append}");
    let expected = content.replace("Change\r\n", "Change\r\nAdded\r\n");
    assert_eq!(f.read("note.md").await.unwrap(), expected);

    let block = f.call("read_note", json!({ "path": "note.md", "block": "detail" })).await;
    assert!(block.contains("Paragraph one\r\nline two."), "{block:?}");
    assert!(!block.contains("^detail") && !block.contains("Added"), "{block:?}");

    f.call(
        "edit_note",
        json!({ "path": "note.md", "block": "detail", "operation": "replace", "old_text": "line two.", "content": "Updated." }),
    )
    .await;
    assert_eq!(f.read("note.md").await.unwrap(), expected.replace("line two.", "Updated."));
    let outside = f
        .call(
            "edit_note",
            json!({ "path": "note.md", "heading": ["First", "Notes"], "operation": "replace", "old_text": "Updated.", "content": "Bad" }),
        )
        .await;
    assert!(outside.contains("not found"), "{outside}");
}

#[tokio::test]
async fn targeted_edits_reject_duplicate_or_missing_targets_without_writing() {
    let content = "# Same\nOne\n# Same\nTwo\n\nA ^duplicate\n\nB ^duplicate\n";
    let f = fixture(&[("note.md", content), ("empty-section.md", "# Empty")]).await;
    let edit = |args: Value| f.call("edit_note", args);
    assert!(edit(json!({ "path": "note.md", "heading": ["Same"], "content": "Bad" })).await.contains("ambiguous"));
    assert!(edit(json!({ "path": "note.md", "block": "duplicate", "content": "Bad" })).await.contains("ambiguous"));
    assert!(edit(json!({ "path": "note.md", "heading": ["Missing"], "content": "Bad" })).await.contains("not found"));
    let both = edit(json!({ "path": "note.md", "heading": ["Same"], "block": "duplicate", "content": "Bad" })).await;
    assert!(both.contains("either"), "{both}");
    assert_eq!(f.read("note.md").await.unwrap(), content);

    f.call("edit_note", json!({ "path": "empty-section.md", "heading": ["Empty"], "content": "First line" })).await;
    assert_eq!(f.read("empty-section.md").await.unwrap(), "# Empty\nFirst line");
}

#[tokio::test]
async fn replace_requires_a_unique_old_text() {
    let f = fixture(&[("note.md", "aaa and b")]).await;
    let replace = |old: Option<&str>| {
        let mut args = json!({ "path": "note.md", "operation": "replace", "content": "X" });
        if let Some(old) = old {
            args["old_text"] = json!(old);
        }
        f.call("edit_note", args)
    };
    assert_eq!(replace(None).await, "old_text is required for replace operation.");
    assert_eq!(replace(Some("zzz")).await, "old_text not found in note.");
    // Overlapping occurrences count as more than one.
    assert_eq!(replace(Some("aa")).await, "old_text matches multiple times. Provide a longer, unique string.");
    assert_eq!(f.read("note.md").await.unwrap(), "aaa and b");
    replace(Some("b")).await;
    assert_eq!(f.read("note.md").await.unwrap(), "aaa and X");
}

#[tokio::test]
async fn targeted_prepend_and_standalone_block_edits_keep_surrounding_text() {
    let content = "# Start\nBody\n# End\n\n- One\n- Two\n\n^list\n\nAfter\n";
    let f = fixture(&[("note.md", content)]).await;
    f.call(
        "edit_note",
        json!({ "path": "note.md", "heading": ["Start"], "operation": "prepend", "content": "Before" }),
    )
    .await;
    let prepended = content.replace("Body", "Before\nBody");
    assert_eq!(f.read("note.md").await.unwrap(), prepended);
    let read = f.call("read_note", json!({ "path": "note.md", "block": "list" })).await;
    assert!(read.ends_with("- One\n- Two"), "{read:?}");
    f.call(
        "edit_note",
        json!({ "path": "note.md", "block": "list", "operation": "replace", "old_text": "- Two", "content": "- Three" }),
    )
    .await;
    assert_eq!(f.read("note.md").await.unwrap(), prepended.replace("- Two", "- Three"));
}

#[tokio::test]
async fn whole_note_prepend_lands_after_any_frontmatter_form() {
    let cases = [
        ("plain.md", "---\na: 1\n---\nBody\n", "---\na: 1\n---\nNew\nBody\n"),
        ("empty-fm.md", "---\n---\nBody\n", "---\n---\nNew\nBody\n"),
        ("eof.md", "---\na: 1\n---", "---\na: 1\n---\nNew\n"),
        ("eof-crlf.md", "---\r\na: 1\r\n---", "---\r\na: 1\r\n---\r\nNew\r\n"),
        ("trailing.md", "---  \na: 1\n--- \t\nBody\n", "---  \na: 1\n--- \t\nNew\nBody\n"),
        ("bom.md", "\u{FEFF}---\na: 1\n---\nBody\n", "\u{FEFF}---\na: 1\n---\nNew\nBody\n"),
        ("crlf.md", "---\r\na: 1\r\n---\r\nBody\r\n", "---\r\na: 1\r\n---\r\nNew\r\nBody\r\n"),
        ("none.md", "Body\n", "New\nBody\n"),
    ];
    let notes: Vec<(&str, &str)> = cases.iter().map(|(path, content, _)| (*path, *content)).collect();
    let f = fixture(&notes).await;
    for (path, _, expected) in cases {
        f.call("edit_note", json!({ "path": path, "operation": "prepend", "content": "New" })).await;
        assert_eq!(f.read(path).await.unwrap(), expected, "{path}");
    }
}

#[tokio::test]
async fn whole_note_append_adds_a_line_break_only_when_needed() {
    let f = fixture(&[("a.md", "one"), ("b.md", "one\n")]).await;
    for path in ["a.md", "b.md"] {
        f.call("edit_note", json!({ "path": path, "content": "two" })).await;
        assert_eq!(f.read(path).await.unwrap(), "one\ntwo", "{path}");
    }
}

#[tokio::test]
async fn list_notes_tag_filter_ignores_hash_and_case_and_includes_nested_tags() {
    let notes = [
        ("a.md", "---\ntags: [Project]\n---\nA"),
        ("b.md", "Body #project/sub"),
        ("c.md", "Body #projects"),
        ("d.md", "Untagged"),
    ];
    let f = fixture(&notes).await;
    for (path, content) in notes {
        f.index.update(path, content, Some(1.0));
    }
    for tag in ["project", "#PROJECT"] {
        let result = f.call("list_notes", json!({ "tag": tag })).await;
        assert!(result.contains("[a.md]") && result.contains("[b.md]"), "{tag}: {result}");
        assert!(!result.contains("[c.md]") && !result.contains("[d.md]"), "{tag}: {result}");
    }
    let nested = f.call("list_notes", json!({ "tag": "project/sub" })).await;
    assert!(!nested.contains("[a.md]") && nested.contains("[b.md]"), "{nested}");
}

#[tokio::test]
async fn list_notes_rejects_a_non_integer_or_out_of_range_limit() {
    let f = fixture(&[("a.md", "A")]).await;
    for limit in [json!(0), json!(-1), json!(1.5), json!(10_001), json!("0"), json!("10001"), json!("ten")] {
        assert!(f.rejects_params("list_notes", json!({ "limit": limit })).await, "limit={limit}");
    }
    assert!(f.call("list_notes", json!({ "limit": "1" })).await.contains("[a.md]"));
}

#[tokio::test]
async fn list_notes_sorts_by_name_then_by_modified() {
    let f = fixture(&[("b.md", "B"), ("A.md", "A"), ("a.md", "a")]).await;
    f.index.update("b.md", "B", Some(3_000.0));
    f.index.update("A.md", "A", Some(1_000.0));
    f.index.update("a.md", "a", Some(2_000.0));
    let order = |text: &str| -> Vec<String> {
        text.lines().skip(1).map(|l| l.split('[').nth(1).unwrap().split(']').next().unwrap().to_owned()).collect()
    };
    let by_name = f.call("list_notes", json!({})).await;
    assert!(by_name.starts_with("3 notes (sorted by name)."), "{by_name}");
    assert_eq!(order(&by_name), ["a.md", "A.md", "b.md"]);
    let by_modified = f.call("list_notes", json!({ "sort_by": "modified" })).await;
    assert_eq!(order(&by_modified), ["b.md", "a.md", "A.md"]);
    assert!(by_modified.lines().nth(1).unwrap().starts_with("- 1970-01-01T00:00 [b.md]"), "{by_modified}");
}

#[tokio::test]
async fn list_notes_survives_an_out_of_range_mtime() {
    let f = fixture(&[]).await;
    f.index.update("odd.md", "x", Some(1e20));
    let text = f.call("list_notes", json!({})).await;
    assert!(text.ends_with("\n-  [odd.md](obsidian://open?vault=Test%20Vault&file=odd)"), "{text}");
}

#[tokio::test]
async fn list_notes_filters_by_modified_after() {
    let f = fixture(&[("old.md", "x"), ("new.md", "y")]).await;
    let cutoff = parse_date("2026-03-25").unwrap();
    f.index.update("old.md", "x", Some(cutoff - 1.0));
    f.index.update("new.md", "y", Some(cutoff));
    let result = f.call("list_notes", json!({ "modified_after": "2026-03-25" })).await;
    assert!(result.contains("[new.md]") && !result.contains("[old.md]"), "{result}");
    assert!(result.starts_with("1 note matches modified_after=\"2026-03-25\""), "{result}");
    let bad = f.call("list_notes", json!({ "modified_after": "soon" })).await;
    assert_eq!(bad, "Invalid date format: soon. Use ISO format like '2026-03-25'.");
}

#[tokio::test]
async fn list_notes_falls_back_to_the_vault_while_the_index_is_empty() {
    let f = fixture(&[("daily/a.md", "a"), ("b.md", "b")]).await;
    let all = f.call("list_notes", json!({})).await;
    assert!(all.starts_with("2 notes (sorted by name). Index: catching up (0 notes indexed so far); this list was read directly from the vault."), "{all}");
    let empty = f.call("list_notes", json!({ "folder": "nothing" })).await;
    assert!(empty.starts_with("No notes found in folder: nothing"), "{empty}");
}

#[tokio::test]
async fn list_folders_includes_parents_and_root() {
    let f = fixture(&[("a/b/c.md", "x"), ("a/d.md", "x"), ("top.md", "x")]).await;
    let text = f.call("list_folders", json!({})).await;
    assert_eq!(text, "- (root) (1 notes)\n- a (1 notes)\n- a/b (1 notes)");
    let empty = fixture(&[]).await;
    assert_eq!(empty.call("list_folders", json!({})).await, "Vault is empty.");
}

#[tokio::test]
async fn list_tags_orders_by_count_then_name() {
    let f = fixture(&[]).await;
    assert_eq!(f.call("list_tags", json!({})).await, "No tags found in the vault.");
    f.index.update("a.md", "#beta #alpha", None);
    f.index.update("b.md", "#beta #gamma", None);
    assert_eq!(f.call("list_tags", json!({})).await, "- #beta (2 notes)\n- #alpha (1 notes)\n- #gamma (1 notes)");
}

#[tokio::test]
async fn write_tools_respect_writable_folders() {
    let scoped = Options { write_folders: Some(vec!["Inbox".to_owned()]), ..Options::default() };
    let f = fixture_with(&[("outside.md", "Body"), ("Inbox/in.md", "In")], scoped).await;
    let denied = "Write access denied: 'outside.md' is outside the writable folders (Inbox/).";
    assert_eq!(f.call("write_note", json!({ "path": "outside.md", "content": "x" })).await, denied);
    assert_eq!(f.call("edit_note", json!({ "path": "outside.md", "content": "x" })).await, denied);
    assert_eq!(f.call("delete_note", json!({ "path": "outside.md" })).await, denied);
    assert_eq!(f.call("move_note", json!({ "from": "outside.md", "to": "Inbox/x.md" })).await, denied);
    let into_outside = f.call("move_note", json!({ "from": "Inbox/in.md", "to": "elsewhere.md" })).await;
    assert!(into_outside.starts_with("Write access denied: 'elsewhere.md'"), "{into_outside}");
    assert_eq!(f.read("outside.md").await.unwrap(), "Body");
    assert!(f.call("write_note", json!({ "path": "Inbox/new.md", "content": "x" })).await.starts_with("Note saved"));
}

#[tokio::test]
async fn write_move_and_delete_keep_the_index_in_step() {
    let f = fixture(&[]).await;
    let saved = f.call("write_note", json!({ "path": "a.md", "content": "#tag" })).await;
    assert_eq!(saved, "Note saved: a.md\n[Open in Obsidian](obsidian://open?vault=Test%20Vault&file=a)");
    assert_eq!(f.index.get_tags("a.md"), ["tag"]);

    let moved = f.call("move_note", json!({ "from": "a.md", "to": "dir/b.md" })).await;
    assert!(moved.starts_with("Moved: a.md → dir/b.md"), "{moved}");
    assert!(!f.index.has("a.md") && f.index.has("dir/b.md"));
    assert_eq!(f.read("dir/b.md").await.as_deref(), Some("#tag"));

    assert_eq!(f.call("delete_note", json!({ "path": "dir/b.md" })).await, "Deleted: dir/b.md");
    assert!(!f.index.has("dir/b.md"));
    assert_eq!(f.call("delete_note", json!({ "path": "dir/b.md" })).await, "Note not found: dir/b.md");
}

#[tokio::test]
async fn moving_onto_an_existing_note_is_a_tool_failure() {
    let f = fixture(&[("a.md", "A"), ("b.md", "B")]).await;
    let response = f.respond("move_note", json!({ "from": "a.md", "to": "b.md" })).await;
    assert_eq!(response["result"]["isError"], json!(true), "{response}");
    assert_eq!(
        response["result"]["content"][0]["text"],
        "Tool 'move_note' execution failed: Destination already exists: b.md"
    );
    assert_eq!(f.read("b.md").await.as_deref(), Some("B"));
}
