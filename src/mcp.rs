//! A minimal Model Context Protocol server: JSON-RPC over stateless
//! Streamable HTTP with plain JSON responses, exposing tools only.
//!
//! Each request stands alone, so a client that opens a new MCP session per tool
//! call (as hosted agents do) pays no session setup, and nothing streams, so a
//! result never sits in a client's event-stream buffer until an idle timeout.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use futures::future::BoxFuture;
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use thiserror::Error;
use tracing::{Level, debug, enabled, info};

use crate::vault::VaultError;

/// Protocol revisions this server speaks, newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// JSON-RPC error codes.
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
}

/// A tool failure, reported to the client as a result with `isError: true`.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error(transparent)]
    Vault(#[from] VaultError),
    #[error("{0}")]
    Message(String),
    /// The arguments break a rule; reported as a JSON-RPC "invalid params" error.
    #[error("{0}")]
    InvalidParams(String),
}

/// Arguments of a tool: deserialized from JSON, described by a JSON Schema, and
/// checked for rules a schema cannot express.
pub trait ToolParams: DeserializeOwned + JsonSchema + Send + 'static {
    /// Extra validation after the schema passed; the error is shown to the client.
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

type Handler = Arc<dyn Fn(Value) -> BoxFuture<'static, Result<String, ToolError>> + Send + Sync>;

/// One callable tool.
pub struct Tool {
    pub name: &'static str,
    pub description: String,
    pub input_schema: Value,
    /// Extra `_meta` advertised in `tools/list`.
    pub meta: Option<Value>,
    validator: jsonschema::Validator,
    handler: Handler,
}

/// The JSON Schema for a parameter type, inlined (no `$ref`s) for MCP clients.
pub fn schema_for<P: JsonSchema>() -> Value {
    let generator = SchemaSettings::draft07()
        .with(|s| {
            s.inline_subschemas = true;
            s.meta_schema = None;
        })
        .into_generator();
    let mut schema = generator.into_root_schema_for::<P>().to_value();
    if let Some(object) = schema.as_object_mut() {
        object.remove("title");
        object.entry("properties").or_insert_with(|| json!({}));
    }
    schema
}

impl Tool {
    pub fn new<P, F, Fut>(name: &'static str, description: impl Into<String>, handler: F) -> Self
    where
        P: ToolParams,
        F: Fn(P) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String, ToolError>> + Send + 'static,
    {
        let input_schema = schema_for::<P>();
        let validator = jsonschema::validator_for(&input_schema).expect("generated schemas are valid");
        let handler = Arc::new(handler);
        Self {
            name,
            description: description.into(),
            input_schema,
            meta: None,
            validator,
            handler: Arc::new(move |args: Value| {
                let handler = handler.clone();
                Box::pin(async move {
                    let params: P =
                        serde_json::from_value(args).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
                    params.check().map_err(ToolError::InvalidParams)?;
                    handler(params).await
                })
            }),
        }
    }

    pub fn with_meta(mut self, meta: Value) -> Self {
        self.meta = Some(meta);
        self
    }

    /// Schema errors for `args`, one line each; empty when valid.
    fn validation_errors(&self, args: &Value) -> Vec<String> {
        self.validator
            .iter_errors(args)
            .map(|e| {
                let at = e.instance_path().to_string();
                if at.is_empty() { e.to_string() } else { format!("{at}: {e}") }
            })
            .collect()
    }

    fn describe(&self) -> Value {
        let mut tool = json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
        });
        if let Some(meta) = &self.meta {
            tool["_meta"] = meta.clone();
        }
        tool
    }
}

/// Name, version and instructions sent in the `initialize` response.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub instructions: String,
}

pub struct McpServer {
    info: ServerInfo,
    tools: Vec<Tool>,
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

fn result_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

impl McpServer {
    pub fn new(info: ServerInfo, tools: Vec<Tool>) -> Self {
        Self { info, tools }
    }

    pub fn tools(&self) -> impl Iterator<Item = &Tool> {
        self.tools.iter()
    }

    /// Handle one JSON-RPC message. Returns the response, or `None` for a notification.
    pub async fn handle(&self, message: Value) -> Option<Value> {
        let Value::Object(mut message) = message else {
            return Some(error_response(Value::Null, codes::INVALID_REQUEST, "Invalid Request"));
        };
        let id = message.remove("id");
        let Some(Value::String(method)) = message.remove("method") else {
            // A response from the client (to a request we never send) or garbage.
            return id.map(|id| error_response(id, codes::INVALID_REQUEST, "Invalid Request"));
        };
        let params = match message.remove("params") {
            Some(Value::Object(params)) => params,
            _ => Map::new(),
        };
        let id = id?; // Notifications need no answer.
        Some(match method.as_str() {
            "initialize" => result_response(id, self.initialize(&params)),
            "ping" | "logging/setLevel" => result_response(id, json!({})),
            "tools/list" => {
                let tools: Vec<Value> = self.tools.iter().map(Tool::describe).collect();
                result_response(id, json!({ "tools": tools }))
            }
            "tools/call" => match self.call_tool(params).await {
                Ok(result) => result_response(id, result),
                Err((code, message)) => error_response(id, code, message),
            },
            other => error_response(id, codes::METHOD_NOT_FOUND, format!("Method not found: {other}")),
        })
    }

    fn initialize(&self, params: &Map<String, Value>) -> Value {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let version =
            requested.filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v)).unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0]);
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": {}, "logging": {} },
            "serverInfo": { "name": self.info.name, "version": self.info.version },
            "instructions": self.info.instructions,
        })
    }

    async fn call_tool(&self, mut params: Map<String, Value>) -> Result<Value, (i64, String)> {
        let name = match params.remove("name") {
            Some(Value::String(name)) => name,
            _ => return Err((codes::INVALID_PARAMS, "Missing tool name".to_owned())),
        };
        let tool = self
            .tools
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| (codes::METHOD_NOT_FOUND, format!("Unknown tool: {name}")))?;
        let args = match params.remove("arguments") {
            Some(Value::Null) | None => json!({}),
            Some(args) => args,
        };
        let errors = tool.validation_errors(&args);
        if !errors.is_empty() {
            return Err((
                codes::INVALID_PARAMS,
                format!("Tool '{name}' parameter validation failed: {}", errors.join("; ")),
            ));
        }
        if enabled!(Level::DEBUG) {
            debug!("[tool] {name}({args})");
        }
        let start = Instant::now();
        let outcome = (tool.handler)(args).await;
        info!("[tool] {name} {}ms", start.elapsed().as_millis());
        Ok(match outcome {
            Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
            Err(ToolError::InvalidParams(message)) => {
                return Err((codes::INVALID_PARAMS, format!("Tool '{name}' parameter validation failed: {message}")));
            }
            Err(error) => json!({
                "content": [{ "type": "text", "text": format!("Tool '{name}' execution failed: {error}") }],
                "isError": true,
            }),
        })
    }
}
