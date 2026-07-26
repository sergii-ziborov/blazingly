use crate::{
    AuditEvent, AuditOutcome, AuditSink, McpCallContext, McpRegistry, McpRuntime, to_value,
};
use blazingly_executor::ExecutableApp;
use serde_json::{Map, Number, Value, json};
use std::sync::Arc;

pub const PROTOCOL_VERSION: &str = "2025-11-25";
pub const CONFIRMATION_META_KEY: &str = "dev.blazingly/confirmed";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub instructions: Option<String>,
}

impl Default for ServerInfo {
    fn default() -> Self {
        Self {
            name: "blazingly".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            description: "Agent-native API server powered by Blazingly.".to_owned(),
            instructions: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Lifecycle {
    #[default]
    Uninitialized,
    AwaitingInitializedNotification,
    Ready,
}

/// A transport-neutral MCP JSON-RPC server.
pub struct JsonRpcServer<'app> {
    app: &'app ExecutableApp,
    runtime: McpRuntime<'app>,
    info: ServerInfo,
    lifecycle: Lifecycle,
    registry: McpRegistry,
    audit: Option<Arc<dyn AuditSink>>,
    audit_session_id: Option<String>,
}

impl<'app> JsonRpcServer<'app> {
    #[must_use]
    pub fn new(app: &'app ExecutableApp) -> Self {
        Self {
            app,
            runtime: McpRuntime::new(app),
            info: ServerInfo::default(),
            lifecycle: Lifecycle::Uninitialized,
            registry: McpRegistry::new(),
            audit: None,
            audit_session_id: None,
        }
    }

    #[must_use]
    pub fn with_server_info(mut self, info: ServerInfo) -> Self {
        self.info = info;
        self
    }

    #[must_use]
    pub fn with_registry(mut self, registry: McpRegistry) -> Self {
        self.registry = registry;
        self
    }

    /// Sends metadata-only request audit events to `sink`.
    #[must_use]
    pub fn with_audit_sink(mut self, sink: impl AuditSink) -> Self {
        self.audit = Some(Arc::new(sink));
        self
    }

    pub(crate) fn with_shared_audit(
        mut self,
        sink: Option<Arc<dyn AuditSink>>,
        session_id: Option<String>,
    ) -> Self {
        self.audit = sink;
        self.audit_session_id = session_id;
        self
    }

    /// Handles one newline-free JSON-RPC message.
    #[must_use]
    pub async fn handle_line(&mut self, line: &str) -> Option<String> {
        let message = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(error) => {
                return Some(
                    error_response(
                        Value::Null,
                        -32_700,
                        "Parse error",
                        Some(json!({
                            "line": error.line(),
                            "column": error.column()
                        })),
                    )
                    .to_string(),
                );
            }
        };

        self.handle_value(message)
            .await
            .map(|response| response.to_string())
    }

    /// Handles one decoded JSON-RPC message.
    #[must_use]
    pub async fn handle_value(&mut self, message: Value) -> Option<Value> {
        let Some(object) = message.as_object() else {
            return Some(invalid_request(Value::Null));
        };
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(invalid_request(request_id_or_null(object)));
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            return Some(invalid_request(request_id_or_null(object)));
        };
        let id = object.get("id");

        if id.is_none() {
            self.handle_notification(method);
            return None;
        }
        let id = id?;
        if !valid_request_id(id) {
            return Some(invalid_request(Value::Null));
        }
        let id = id.clone();
        let params = object.get("params");

        let response = match method {
            "initialize" => Some(self.initialize(id, params)),
            "ping" => Some(success_response(id, json!({}))),
            "tools/list" => Some(self.tools_list(id, params)),
            "tools/call" => Some(self.tools_call(id, params).await),
            "resources/list" => Some(self.resources_list(id, params)),
            "resources/read" => Some(self.resources_read(id, params).await),
            "prompts/list" => Some(self.prompts_list(id, params)),
            "prompts/get" => Some(self.prompts_get(id, params).await),
            _ => Some(error_response(id, -32_601, "Method not found", None)),
        };
        if let Some(response) = response.as_ref() {
            self.record_audit(method, params, response);
        }
        response
    }

    fn initialize(&mut self, id: Value, params: Option<&Value>) -> Value {
        if self.lifecycle != Lifecycle::Uninitialized {
            return error_response(id, -32_600, "Server is already initialized", None);
        }
        let Some(params) = params.and_then(Value::as_object) else {
            return invalid_params(id, "initialize params must be an object");
        };
        let Some(requested_version) = params.get("protocolVersion").and_then(Value::as_str) else {
            return invalid_params(id, "protocolVersion must be a string");
        };
        if !params.get("capabilities").is_some_and(Value::is_object) {
            return invalid_params(id, "capabilities must be an object");
        }
        if !params.get("clientInfo").is_some_and(Value::is_object) {
            return invalid_params(id, "clientInfo must be an object");
        }

        let protocol_version = if requested_version == PROTOCOL_VERSION {
            requested_version
        } else {
            PROTOCOL_VERSION
        };
        let info = self.info.clone();
        let mut result = json!({
            "protocolVersion": protocol_version,
            "capabilities": {
                "tools": {
                    "listChanged": false
                }
            },
            "serverInfo": {
                "name": info.name,
                "version": info.version,
                "description": info.description
            }
        });
        if self.registry.has_resources() {
            result["capabilities"]["resources"] = json!({
                "subscribe": false,
                "listChanged": false
            });
        }
        if self.registry.has_prompts() {
            result["capabilities"]["prompts"] = json!({
                "listChanged": false
            });
        }
        if let Some(instructions) = info.instructions {
            result["instructions"] = Value::String(instructions);
        }
        self.lifecycle = Lifecycle::AwaitingInitializedNotification;

        success_response(id, result)
    }

    fn tools_list(&self, id: Value, params: Option<&Value>) -> Value {
        if self.lifecycle != Lifecycle::Ready {
            return not_initialized(id);
        }
        if params.is_some_and(|params| !params.is_object() && !params.is_null()) {
            return invalid_params(id, "tools/list params must be an object");
        }

        success_response(id, to_value(self.app.definition()))
    }

    async fn tools_call(&self, id: Value, params: Option<&Value>) -> Value {
        if self.lifecycle != Lifecycle::Ready {
            return not_initialized(id);
        }
        let Some(params) = params.and_then(Value::as_object) else {
            return invalid_params(id, "tools/call params must be an object");
        };
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return invalid_params(id, "tool name must be a string");
        };
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !arguments.is_object() {
            return invalid_params(id, "tool arguments must be an object");
        }
        let confirmed = params
            .get("_meta")
            .and_then(Value::as_object)
            .and_then(|meta| meta.get(CONFIRMATION_META_KEY))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        match self
            .runtime
            .call_tool(name, arguments, McpCallContext { confirmed })
            .await
        {
            Ok(result) => match serde_json::to_value(result) {
                Ok(result) => success_response(id, result),
                Err(_) => error_response(id, -32_603, "Internal error", None),
            },
            Err(error) => error_response(id, error.code, &error.message, None),
        }
    }

    fn resources_list(&self, id: Value, params: Option<&Value>) -> Value {
        if self.lifecycle != Lifecycle::Ready {
            return not_initialized(id);
        }
        if let Err(message) = validate_list_params(params, "resources/list") {
            return invalid_params(id, message);
        }
        let resources = self
            .registry
            .resources()
            .map(crate::McpResource::descriptor)
            .collect::<Vec<_>>();
        match serde_json::to_value(resources) {
            Ok(resources) => success_response(id, json!({ "resources": resources })),
            Err(_) => error_response(id, -32_603, "Internal error", None),
        }
    }

    async fn resources_read(&self, id: Value, params: Option<&Value>) -> Value {
        if self.lifecycle != Lifecycle::Ready {
            return not_initialized(id);
        }
        let Some(params) = params.and_then(Value::as_object) else {
            return invalid_params(id, "resources/read params must be an object");
        };
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return invalid_params(id, "resource URI must be a string");
        };
        let Some(resource) = self.registry.resource(uri) else {
            return error_response(id, -32_002, "Resource not found", None);
        };
        match resource.read().await {
            Ok(content) => match serde_json::to_value(content) {
                Ok(content) => success_response(id, json!({ "contents": [content] })),
                Err(_) => error_response(id, -32_603, "Internal error", None),
            },
            Err(error) => error_response(id, error.code, &error.message, None),
        }
    }

    fn prompts_list(&self, id: Value, params: Option<&Value>) -> Value {
        if self.lifecycle != Lifecycle::Ready {
            return not_initialized(id);
        }
        if let Err(message) = validate_list_params(params, "prompts/list") {
            return invalid_params(id, message);
        }
        let prompts = self
            .registry
            .prompts()
            .map(crate::McpPrompt::descriptor)
            .collect::<Vec<_>>();
        match serde_json::to_value(prompts) {
            Ok(prompts) => success_response(id, json!({ "prompts": prompts })),
            Err(_) => error_response(id, -32_603, "Internal error", None),
        }
    }

    async fn prompts_get(&self, id: Value, params: Option<&Value>) -> Value {
        if self.lifecycle != Lifecycle::Ready {
            return not_initialized(id);
        }
        let Some(params) = params.and_then(Value::as_object) else {
            return invalid_params(id, "prompts/get params must be an object");
        };
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return invalid_params(id, "prompt name must be a string");
        };
        let arguments = match params.get("arguments") {
            None => Map::new(),
            Some(Value::Object(arguments)) if arguments.values().all(Value::is_string) => {
                arguments.clone()
            }
            Some(_) => {
                return invalid_params(id, "prompt arguments must be an object of strings");
            }
        };
        let Some(prompt) = self.registry.prompt(name) else {
            return invalid_params(id, "unknown prompt");
        };
        match prompt.render(arguments).await {
            Ok(messages) => match serde_json::to_value(messages) {
                Ok(messages) => {
                    let mut result = json!({ "messages": messages });
                    if let Some(description) = &prompt.descriptor().description {
                        result["description"] = Value::String(description.clone());
                    }
                    success_response(id, result)
                }
                Err(_) => error_response(id, -32_603, "Internal error", None),
            },
            Err(error) => error_response(id, error.code, &error.message, None),
        }
    }

    fn record_audit(&self, method: &str, params: Option<&Value>, response: &Value) {
        let Some(audit) = self.audit.as_ref() else {
            return;
        };
        let subject = params.and_then(Value::as_object).and_then(|params| {
            match method {
                "tools/call" | "prompts/get" => params.get("name"),
                "resources/read" => params.get("uri"),
                _ => None,
            }
            .and_then(Value::as_str)
            .map(str::to_owned)
        });
        let outcome = response
            .get("error")
            .and_then(Value::as_object)
            .and_then(|error| error.get("code"))
            .and_then(Value::as_i64)
            .map_or(AuditOutcome::Success, |code| AuditOutcome::Error {
                code: i32::try_from(code).unwrap_or(-32_603),
            });
        audit.record(AuditEvent {
            method: method.to_owned(),
            subject,
            session_id: self.audit_session_id.clone(),
            outcome,
        });
    }

    fn handle_notification(&mut self, method: &str) {
        if method == "notifications/initialized"
            && self.lifecycle == Lifecycle::AwaitingInitializedNotification
        {
            self.lifecycle = Lifecycle::Ready;
        }
    }
}

fn validate_list_params<'message>(
    params: Option<&Value>,
    method: &'message str,
) -> Result<(), &'message str> {
    match params {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Object(params)) if !params.contains_key("cursor") => Ok(()),
        Some(Value::Object(_)) => Err("pagination cursors are not supported by this registry"),
        Some(_) if method == "resources/list" => Err("resources/list params must be an object"),
        Some(_) => Err("prompts/list params must be an object"),
    }
}

fn success_response(id: Value, result: Value) -> Value {
    Value::Object(Map::from_iter([
        ("jsonrpc".to_owned(), Value::String("2.0".to_owned())),
        ("id".to_owned(), id),
        ("result".to_owned(), result),
    ]))
}

fn invalid_request(id: Value) -> Value {
    error_response(id, -32_600, "Invalid Request", None)
}

fn invalid_params(id: Value, message: &str) -> Value {
    error_response(id, -32_602, message, None)
}

fn not_initialized(id: Value) -> Value {
    error_response(id, -32_000, "Server is not initialized", None)
}

fn error_response(id: Value, code: i32, message: &str, data: Option<Value>) -> Value {
    let mut error = Map::from_iter([
        ("code".to_owned(), Value::Number(Number::from(code))),
        ("message".to_owned(), Value::String(message.to_owned())),
    ]);
    if let Some(data) = data {
        error.insert("data".to_owned(), data);
    }

    Value::Object(Map::from_iter([
        ("jsonrpc".to_owned(), Value::String("2.0".to_owned())),
        ("id".to_owned(), id),
        ("error".to_owned(), Value::Object(error)),
    ]))
}

fn request_id_or_null(object: &Map<String, Value>) -> Value {
    object.get("id").cloned().unwrap_or(Value::Null)
}

fn valid_request_id(id: &Value) -> bool {
    id.is_string()
        || id
            .as_number()
            .is_some_and(|number| number.is_i64() || number.is_u64())
}
