use crate::{AuditSink, JsonRpcServer, McpRegistry, PROTOCOL_VERSION, ServerInfo};
use blazingly_executor::ExecutableApp;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::BuildHasher;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// HTTP methods understood by the MCP Streamable HTTP endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpHttpMethod {
    Get,
    Post,
    Delete,
}

/// Runtime-neutral Streamable HTTP request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamableHttpRequest {
    method: McpHttpMethod,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl StreamableHttpRequest {
    #[must_use]
    pub fn new(method: McpHttpMethod) -> Self {
        Self {
            method,
            headers: BTreeMap::new(),
            body: Vec::new(),
        }
    }

    #[must_use]
    pub fn post(body: impl Into<Vec<u8>>) -> Self {
        Self::new(McpHttpMethod::Post)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(body)
    }

    #[must_use]
    pub fn header(mut self, name: impl AsRef<str>, value: impl Into<String>) -> Self {
        self.headers
            .insert(name.as_ref().to_ascii_lowercase(), value.into());
        self
    }

    #[must_use]
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// Runtime-neutral Streamable HTTP response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamableHttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl StreamableHttpResponse {
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    #[must_use]
    pub fn get_header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    #[must_use]
    pub fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }

    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Decodes the response body as JSON.
    ///
    /// # Errors
    ///
    /// Returns a serde error for non-JSON responses.
    pub fn json(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }
}

/// Security and resource limits for Streamable HTTP.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamableHttpConfig {
    max_message_bytes: usize,
    allowed_origins: BTreeSet<String>,
}

impl StreamableHttpConfig {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            allowed_origins: BTreeSet::new(),
        }
    }

    #[must_use]
    pub const fn with_max_message_bytes(mut self, bytes: usize) -> Self {
        self.max_message_bytes = bytes;
        self
    }

    /// Allows one exact browser `Origin` value.
    ///
    /// Requests without an `Origin` header are accepted. Any supplied origin
    /// is rejected with 403 unless explicitly allowlisted.
    #[must_use]
    pub fn allow_origin(mut self, origin: impl Into<String>) -> Self {
        self.allowed_origins.insert(origin.into());
        self
    }
}

impl Default for StreamableHttpConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Stateful MCP 2025-11-25 Streamable HTTP endpoint.
///
/// Each session owns an independent JSON-RPC lifecycle. The runtime-neutral
/// type can be mounted by the Tokio-free native adapter or by a future
/// Cloudflare adapter without changing MCP semantics.
pub struct StreamableHttpServer<'app> {
    app: &'app ExecutableApp,
    info: ServerInfo,
    registry: McpRegistry,
    config: StreamableHttpConfig,
    sessions: HashMap<String, JsonRpcServer<'app>>,
    audit: Option<Arc<dyn AuditSink>>,
    session_id_factory: Box<dyn FnMut() -> String>,
}

impl<'app> StreamableHttpServer<'app> {
    #[must_use]
    pub fn new(app: &'app ExecutableApp) -> Self {
        let mut counter = 0_u64;
        Self {
            app,
            info: ServerInfo::default(),
            registry: McpRegistry::new(),
            config: StreamableHttpConfig::new(),
            sessions: HashMap::new(),
            audit: None,
            session_id_factory: Box::new(move || {
                counter = counter.wrapping_add(1);
                default_session_id(counter)
            }),
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

    #[must_use]
    pub fn with_config(mut self, config: StreamableHttpConfig) -> Self {
        self.config = config;
        self
    }

    #[must_use]
    pub fn with_audit_sink(mut self, sink: impl AuditSink) -> Self {
        self.audit = Some(Arc::new(sink));
        self
    }

    /// Replaces session ID generation, primarily for platform entropy sources
    /// and deterministic transport tests.
    #[must_use]
    pub fn with_session_id_factory(mut self, factory: impl FnMut() -> String + 'static) -> Self {
        self.session_id_factory = Box::new(factory);
        self
    }

    #[must_use]
    pub fn active_sessions(&self) -> usize {
        self.sessions.len()
    }

    /// Handles one request to the single MCP endpoint.
    pub async fn handle(&mut self, request: StreamableHttpRequest) -> StreamableHttpResponse {
        if request.body.len() > self.config.max_message_bytes {
            return plain_response(413, "MCP message exceeds the configured size limit");
        }
        if request
            .header_value("origin")
            .is_some_and(|origin| !self.config.allowed_origins.contains(origin))
        {
            return plain_response(403, "Origin is not allowed");
        }

        match request.method {
            McpHttpMethod::Get => method_not_allowed("POST, DELETE"),
            McpHttpMethod::Delete => self.delete_session(&request),
            McpHttpMethod::Post => self.post(request).await,
        }
    }

    fn delete_session(&mut self, request: &StreamableHttpRequest) -> StreamableHttpResponse {
        let Some(session_id) = request.header_value("mcp-session-id") else {
            return plain_response(400, "MCP-Session-Id is required");
        };
        if self.sessions.remove(session_id).is_some() {
            empty_response(204)
        } else {
            plain_response(404, "MCP session was not found")
        }
    }

    async fn post(&mut self, request: StreamableHttpRequest) -> StreamableHttpResponse {
        if !request
            .header_value("content-type")
            .is_some_and(|value| media_type(value, "application/json"))
        {
            return plain_response(415, "Content-Type must be application/json");
        }
        if !request
            .header_value("accept")
            .is_some_and(accepts_streamable_http)
        {
            return plain_response(
                406,
                "Accept must include application/json and text/event-stream",
            );
        }
        let message = match serde_json::from_slice::<Value>(&request.body) {
            Ok(message) => message,
            Err(error) => {
                return json_response(
                    400,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": {
                            "code": -32700,
                            "message": "Parse error",
                            "data": {
                                "line": error.line(),
                                "column": error.column()
                            }
                        }
                    }),
                );
            }
        };
        let method = message.get("method").and_then(Value::as_str);
        if method == Some("initialize") {
            return self.initialize_session(request, message).await;
        }
        if method.is_none()
            && message.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
            && (message.get("result").is_some() || message.get("error").is_some())
        {
            return empty_response(202);
        }

        let Some(session_id) = request.header_value("mcp-session-id") else {
            return plain_response(400, "MCP-Session-Id is required after initialization");
        };
        if request
            .header_value("mcp-protocol-version")
            .is_some_and(|version| version != PROTOCOL_VERSION)
        {
            return plain_response(400, "unsupported MCP-Protocol-Version");
        }
        let Some(server) = self.sessions.get_mut(session_id) else {
            return plain_response(404, "MCP session was not found");
        };
        match server.handle_value(message).await {
            Some(response) => json_response(200, &response),
            None => empty_response(202),
        }
    }

    async fn initialize_session(
        &mut self,
        request: StreamableHttpRequest,
        message: Value,
    ) -> StreamableHttpResponse {
        if request.header_value("mcp-session-id").is_some() {
            return plain_response(400, "initialize must not reuse an MCP session");
        }
        let Some(session_id) = self.unique_session_id() else {
            return plain_response(500, "could not allocate a unique MCP session");
        };
        let mut server = JsonRpcServer::new(self.app)
            .with_server_info(self.info.clone())
            .with_registry(self.registry.clone())
            .with_shared_audit(self.audit.clone(), Some(session_id.clone()));
        let Some(response) = server.handle_value(message).await else {
            return plain_response(400, "initialize must be a JSON-RPC request");
        };
        if response.get("error").is_some() {
            return json_response(400, &response);
        }
        self.sessions.insert(session_id.clone(), server);
        json_response(200, &response)
            .with_header("mcp-session-id", session_id)
            .with_header("mcp-protocol-version", PROTOCOL_VERSION)
    }

    fn unique_session_id(&mut self) -> Option<String> {
        for _ in 0..8 {
            let session_id = (self.session_id_factory)();
            if valid_session_id(&session_id) && !self.sessions.contains_key(&session_id) {
                return Some(session_id);
            }
        }
        None
    }
}

fn default_session_id(counter: u64) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_u128, |duration| duration.as_nanos());
    let first = std::collections::hash_map::RandomState::new();
    let second = std::collections::hash_map::RandomState::new();
    let left = first.hash_one((timestamp, counter));
    let right = second.hash_one((counter, timestamp.rotate_left(47)));
    format!("{left:016x}{right:016x}")
}

fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty() && session_id.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

fn accepts_streamable_http(value: &str) -> bool {
    let mut json = false;
    let mut event_stream = false;
    for accepted in value.split(',').map(str::trim) {
        json |= media_type(accepted, "application/json");
        event_stream |= media_type(accepted, "text/event-stream");
    }
    json && event_stream
}

fn media_type(value: &str, expected: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
}

fn json_response(status: u16, value: &Value) -> StreamableHttpResponse {
    match serde_json::to_vec(value) {
        Ok(body) => StreamableHttpResponse {
            status,
            headers: BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]),
            body,
        },
        Err(_) => plain_response(500, "could not serialize MCP response"),
    }
}

fn plain_response(status: u16, message: &str) -> StreamableHttpResponse {
    StreamableHttpResponse {
        status,
        headers: BTreeMap::from([(
            "content-type".to_owned(),
            "text/plain; charset=utf-8".to_owned(),
        )]),
        body: message.as_bytes().to_vec(),
    }
}

fn empty_response(status: u16) -> StreamableHttpResponse {
    StreamableHttpResponse {
        status,
        headers: BTreeMap::new(),
        body: Vec::new(),
    }
}

fn method_not_allowed(allow: &str) -> StreamableHttpResponse {
    plain_response(405, "HTTP method not supported by this MCP endpoint")
        .with_header("allow", allow)
}

impl StreamableHttpResponse {
    fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }
}
