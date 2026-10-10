//! Obsidian tools exposed through the rmcp server SDK.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use futures::future::BoxFuture;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorCode, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use thiserror::Error;
use tracing::{Level, debug, enabled, info};

use crate::vault::VaultError;

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
    /// Behavior hints advertised in `tools/list`.
    pub annotations: Option<ToolAnnotations>,
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
            annotations: None,
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

    pub fn with_annotations(mut self, annotations: ToolAnnotations) -> Self {
        self.annotations = Some(annotations);
        self
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

    fn describe(&self) -> rmcp::model::Tool {
        let mut tool = rmcp::model::Tool::new(
            self.name,
            self.description.clone(),
            self.input_schema.as_object().expect("tool schemas are objects").clone(),
        );
        tool.annotations = self.annotations.clone();
        if let Some(meta) = &self.meta {
            tool.meta = Some(meta.as_object().expect("tool metadata is an object").clone().into());
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

#[derive(Clone)]
pub struct McpServer {
    info: ServerInfo,
    tools: Arc<Vec<Tool>>,
}

impl McpServer {
    pub fn new(info: ServerInfo, tools: Vec<Tool>) -> Self {
        Self { info, tools: Arc::new(tools) }
    }

    pub fn tools(&self) -> impl Iterator<Item = &Tool> {
        self.tools.iter()
    }
}

impl ServerHandler for McpServer {
    #[allow(deprecated)] // Keep logging/setLevel available for legacy MCP clients.
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_logging().build())
            .with_server_info(Implementation::new(&self.info.name, &self.info.version))
            .with_instructions(&self.info.instructions)
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        self.tools.iter().find(|tool| tool.name == name).map(Tool::describe)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools.iter().map(Tool::describe).collect()))
    }

    #[allow(deprecated)] // Keep logging/setLevel available for legacy MCP clients.
    async fn set_level(
        &self,
        _request: rmcp::model::SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        Ok(())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name;
        let tool = self
            .tools
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| ErrorData::new(ErrorCode::METHOD_NOT_FOUND, format!("Unknown tool: {name}"), None))?;
        let args = Value::Object(request.arguments.unwrap_or_default());
        let errors = tool.validation_errors(&args);
        if !errors.is_empty() {
            return Err(ErrorData::invalid_params(
                format!("Tool '{name}' parameter validation failed: {}", errors.join("; ")),
                None,
            ));
        }
        if enabled!(Level::DEBUG) {
            debug!("[tool] {name}({args})");
        }
        let start = Instant::now();
        let outcome = (tool.handler)(args).await;
        info!("[tool] {name} {}ms", start.elapsed().as_millis());
        Ok(match outcome {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(ToolError::InvalidParams(message)) => {
                return Err(ErrorData::invalid_params(
                    format!("Tool '{name}' parameter validation failed: {message}"),
                    None,
                ));
            }
            Err(error) => {
                CallToolResult::error(vec![ContentBlock::text(format!("Tool '{name}' execution failed: {error}"))])
            }
        }
        .into())
    }
}
