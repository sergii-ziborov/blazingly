#![forbid(unsafe_code)]

mod audit;
mod control_plane;
mod http;
mod jsonrpc;
mod registry;

pub use audit::{AuditEvent, AuditOutcome, AuditSink, BoundedAuditLog};
pub use control_plane::{
    FRAMEWORK_MANIFEST_MIME_TYPE, FRAMEWORK_MANIFEST_SCHEMA, FRAMEWORK_MANIFEST_URI,
    FrameworkManifest,
};
pub use http::{
    McpHttpMethod, StreamableHttpConfig, StreamableHttpRequest, StreamableHttpResponse,
    StreamableHttpServer,
};
pub use jsonrpc::{CONFIRMATION_META_KEY, JsonRpcServer, PROTOCOL_VERSION, ServerInfo};
pub use registry::{
    McpPrompt, McpRegistry, McpResource, PromptArgument, PromptDescriptor, PromptMessage,
    PromptRole, RegistryError, ResourceContent, ResourceDescriptor,
};

use blazingly_core::{
    AppDefinition, Confirmation, FieldMetadata, InputDescriptor, InputSource, ModelDescriptor,
    OperationDescriptor, OperationRisk, OutputExposure, SchemaKind, TypeDescriptor, ValidationRule,
};
use blazingly_executor::{ExecutableApp, ExecutionOutcome};
use blazingly_json::Map;
use blazingly_json::{Value, json};
use serde::Serialize;
use std::fmt;

/// Context supplied by an MCP host when it invokes a tool.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct McpCallContext {
    pub confirmed: bool,
}

impl McpCallContext {
    #[must_use]
    pub const fn confirmed() -> Self {
        Self { confirmed: true }
    }
}

/// An MCP content block returned to an agent.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
}

/// The protocol-level `CallToolResult` shape.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "is_false")]
    pub is_error: bool,
}

/// An MCP JSON-RPC error, distinct from an agent-visible tool execution error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpProtocolError {
    pub code: i32,
    pub message: String,
}

impl fmt::Display for McpProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for McpProtocolError {}

/// Native in-process MCP projection over the shared operation executor.
pub struct McpRuntime<'app> {
    app: &'app ExecutableApp,
}

impl<'app> McpRuntime<'app> {
    #[must_use]
    pub const fn new(app: &'app ExecutableApp) -> Self {
        Self { app }
    }

    /// Invokes an MCP tool through the same validation and handler pipeline as
    /// every other transport.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for an unknown tool or an internal framework
    /// failure. Validation and domain failures are successful MCP protocol
    /// responses with `isError: true`, so an agent can inspect and correct them.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        context: McpCallContext,
    ) -> Result<CallToolResult, McpProtocolError> {
        let operation = self
            .app
            .operation_for_mcp_tool(name)
            .ok_or_else(|| McpProtocolError {
                code: -32_602,
                message: format!("unknown tool: {name}"),
            })?;
        let descriptor = operation.descriptor();
        let Some(tool) = descriptor.contract.mcp.as_ref() else {
            return Err(McpProtocolError {
                code: -32_603,
                message: "MCP operation index is inconsistent".to_owned(),
            });
        };
        let operation_id = descriptor.contract.id.as_str().to_owned();

        if descriptor.contract.agent.confirmation == Confirmation::Required && !context.confirmed {
            return Ok(tool_error(
                "confirmation_required",
                "This operation requires explicit user confirmation before it can run.",
                Some(json!({
                    "operationId": operation_id,
                    "confirmationRequired": true
                })),
            ));
        }

        let exposure = tool.expose_output;
        match operation.invoke(arguments).await {
            ExecutionOutcome::Success {
                status,
                headers: _,
                body,
                background: _,
            } => {
                let body = body
                    .map(|body| {
                        blazingly_json::from_slice(&body).map_err(|error| McpProtocolError {
                            code: -32_603,
                            message: format!("operation response is not valid JSON: {error}"),
                        })
                    })
                    .transpose()?;
                Ok(success_result(&operation_id, status, body, exposure))
            }
            ExecutionOutcome::StreamingSuccess { .. } => Err(McpProtocolError {
                code: -32_603,
                message: "streaming HTTP responses cannot be projected as MCP tool results"
                    .to_owned(),
            }),
            ExecutionOutcome::Upgrade { .. } => Err(McpProtocolError {
                code: -32_603,
                message: "HTTP protocol upgrades cannot be projected as MCP tool results"
                    .to_owned(),
            }),
            ExecutionOutcome::Rejected {
                status,
                code,
                message,
                details,
            } => Ok(tool_error(
                &code,
                &message,
                Some(json!({
                    "status": status,
                    "details": details
                })),
            )),
            ExecutionOutcome::DomainError(error) => {
                let details = error
                    .details
                    .map(|details| {
                        blazingly_json::from_slice::<Value>(&details).map_err(|decode| {
                            McpProtocolError {
                                code: -32_603,
                                message: format!(
                                    "operation error response is not valid JSON: {decode}"
                                ),
                            }
                        })
                    })
                    .transpose()?;
                Ok(tool_error(
                    &error.code,
                    &error.message,
                    Some(json!({
                        "status": error.status,
                        "details": details
                    })),
                ))
            }
            ExecutionOutcome::InternalError { .. } => Err(McpProtocolError {
                code: -32_603,
                message: "the operation could not be completed".to_owned(),
            }),
        }
    }
}

/// Generates the MCP tool-discovery document for the application.
///
/// This is the discovery half of native MCP. Invocation will use the same
/// operation executor as HTTP rather than a generated adapter handler.
#[must_use]
pub fn to_value(app: &AppDefinition) -> Value {
    let tools = app
        .operations()
        .iter()
        .filter_map(tool_value)
        .collect::<Vec<_>>();

    json!({ "tools": tools })
}

fn tool_value(operation: &OperationDescriptor) -> Option<Value> {
    let tool = operation.contract.mcp.as_ref()?;
    let input_schema = combined_input_schema(&operation.contract.inputs);
    let output_schema = operation
        .contract
        .responses
        .iter()
        .find(|response| response.error_code.is_none())
        .and_then(|response| response.body.as_ref())
        .map(schema_value);
    let policy = &operation.contract.agent;

    let mut value = json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": input_schema,
        "annotations": {
            "readOnlyHint": policy.risk == OperationRisk::Read,
            "destructiveHint": policy.risk == OperationRisk::Destructive,
            "idempotentHint": policy.idempotent
        },
        "x-blazingly": {
            "operationId": operation.contract.id.as_str(),
            "confirmation": match policy.confirmation {
                Confirmation::Never => "never",
                Confirmation::Required => "required"
            },
            "confirmationMetaKey": CONFIRMATION_META_KEY,
            "outputExposure": tool.expose_output,
            "outputSchema": output_schema
        }
    });
    if tool.expose_output == OutputExposure::Full
        && let Some(output_schema) = output_schema
    {
        value["outputSchema"] = result_schema(output_schema);
    }

    Some(value)
}

fn result_schema(schema: Value) -> Value {
    if schema["type"] == "object" {
        return schema;
    }

    json!({
        "type": "object",
        "properties": {
            "result": schema
        },
        "required": ["result"],
        "additionalProperties": false
    })
}

fn success_result(
    operation_id: &str,
    status: u16,
    body: Option<Value>,
    exposure: OutputExposure,
) -> CallToolResult {
    match exposure {
        OutputExposure::Full if body.is_some() => {
            let body = body.expect("the match guard verified the response body");
            let structured_content = object_result(body);
            let text = Value::Object(structured_content.clone()).to_string();
            CallToolResult {
                content: vec![ContentBlock::Text { text }],
                structured_content: Some(structured_content),
                is_error: false,
            }
        }
        OutputExposure::Full | OutputExposure::SummaryOnly => CallToolResult {
            content: vec![ContentBlock::Text {
                text: format!(
                    "Operation `{operation_id}` completed successfully with status {status}."
                ),
            }],
            structured_content: None,
            is_error: false,
        },
        OutputExposure::None => CallToolResult {
            content: vec![ContentBlock::Text {
                text: "Operation completed successfully.".to_owned(),
            }],
            structured_content: None,
            is_error: false,
        },
    }
}

fn object_result(body: Value) -> Map<String, Value> {
    match body {
        Value::Object(object) => object,
        value => Map::from_iter([("result".to_owned(), value)]),
    }
}

fn tool_error(code: &str, message: &str, details: Option<Value>) -> CallToolResult {
    let mut error = Map::from_iter([
        ("code".to_owned(), Value::String(code.to_owned())),
        ("message".to_owned(), Value::String(message.to_owned())),
    ]);
    if let Some(details) = details {
        error.insert("details".to_owned(), details);
    }
    let payload = json!({ "error": error });

    CallToolResult {
        content: vec![ContentBlock::Text {
            text: payload.to_string(),
        }],
        structured_content: None,
        is_error: true,
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

fn empty_object_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

fn combined_input_schema(inputs: &[InputDescriptor]) -> Value {
    if inputs.is_empty() {
        return empty_object_schema();
    }

    let mut properties = Map::new();
    let mut required = Vec::new();
    for input in inputs {
        if let Some(model) = &input.ty.model {
            for field in &model.fields {
                let mut schema = schema_value(&field.ty);
                apply_validation(&mut schema, &field.validation);
                schema["x-blazingly-source"] = json!(input_source_name(input.source));
                properties.insert(field.name.clone(), schema);
                if input.required && field.required {
                    required.push(field.name.clone());
                }
            }
        } else {
            let mut schema = schema_value(&input.ty);
            schema["x-blazingly-source"] = json!(input_source_name(input.source));
            properties.insert(input.name.clone(), schema);
            if input.required {
                required.push(input.name.clone());
            }
        }
    }

    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

const fn input_source_name(source: InputSource) -> &'static str {
    match source {
        InputSource::Path => "path",
        InputSource::Query => "query",
        InputSource::Header => "header",
        InputSource::Cookie => "cookie",
        InputSource::Json => "json",
        InputSource::Form => "form",
        InputSource::Multipart => "multipart",
        InputSource::File => "file",
        InputSource::Stream => "stream",
    }
}

/// Projects one type, including the rules the type itself declares.
///
/// The recursion is what carries a value type's bounds into a `Vec<Tag>` item
/// and into every deeper nesting: the item is a descriptor of its own, so it is
/// projected by the same code that projects a bare field of that type.
fn schema_value(descriptor: &TypeDescriptor) -> Value {
    let mut value = if let Some(model) = &descriptor.model {
        model_schema(model)
    } else {
        let mut value = match (&descriptor.schema, &descriptor.items) {
            (SchemaKind::Array(_), Some(items)) => {
                json!({ "type": "array", "items": schema_value(items) })
            }
            _ => schema_kind_value(&descriptor.schema),
        };
        apply_known_string_format(&mut value, &descriptor.rust_name);
        value["x-rust-type"] = Value::String(descriptor.rust_name.clone());
        value
    };
    apply_validation(&mut value, &descriptor.constraints);
    value
}

fn apply_known_string_format(schema: &mut Value, rust_name: &str) {
    let format = match rust_name {
        "Uuid" => "uuid",
        "Url" => "uri",
        "IpAddress" => "ip",
        "Date" => "date",
        "DateTime" => "date-time",
        "Decimal" => "decimal",
        _ => return,
    };
    schema["format"] = Value::String(format.to_owned());
}

fn schema_kind_value(schema: &SchemaKind) -> Value {
    match schema {
        SchemaKind::String => json!({ "type": "string" }),
        SchemaKind::Binary => json!({ "type": "string", "contentEncoding": "base64" }),
        SchemaKind::Integer => json!({ "type": "integer" }),
        SchemaKind::Number => json!({ "type": "number" }),
        SchemaKind::Boolean => json!({ "type": "boolean" }),
        SchemaKind::Array(item) => {
            json!({ "type": "array", "items": schema_kind_value(item) })
        }
        SchemaKind::Object => json!({ "type": "object" }),
        SchemaKind::Any => json!({}),
    }
}

fn model_schema(model: &ModelDescriptor) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();

    for field in &model.fields {
        let mut schema = schema_value(&field.ty);
        apply_validation(&mut schema, &field.validation);
        properties.insert(field.name.clone(), schema);
        if field.required {
            required.push(field.name.clone());
        }
    }

    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// Projects a recovered default, enumeration, or nullability marker.
///
/// MCP tool schemas follow JSON Schema 2020-12, which has no `nullable`
/// keyword: a value that also accepts `null` widens its own `type` into a
/// union instead. This projection inlines every model schema, so unlike the
/// `OpenAPI` document there is no `$ref` node to wrap in an `anyOf`.
fn apply_field_metadata(schema: &mut Value, metadata: &FieldMetadata) {
    match metadata {
        FieldMetadata::Default(value) => schema["default"] = value.clone(),
        FieldMetadata::Enumeration(values) => {
            schema["enum"] = Value::Array(
                values
                    .iter()
                    .map(|value| Value::String(value.clone()))
                    .collect(),
            );
        }
        FieldMetadata::Nullable => widen_with_null(schema),
    }
}

fn widen_with_null(schema: &mut Value) {
    match schema.get("type").cloned() {
        Some(Value::String(name)) => schema["type"] = json!([name, "null"]),
        Some(Value::Array(mut names)) => {
            if !names.iter().any(|name| name.as_str() == Some("null")) {
                names.push(Value::String("null".to_owned()));
                schema["type"] = Value::Array(names);
            }
        }
        // A schema with no declared type constrains nothing, so it already
        // accepts `null`.
        Some(_) | None => {}
    }
}

fn apply_validation(schema: &mut Value, validation: &[ValidationRule]) {
    for rule in validation {
        match rule {
            ValidationRule::MinLength(value) => schema["minLength"] = json!(value),
            ValidationRule::MaxLength(value) => schema["maxLength"] = json!(value),
            ValidationRule::Email => schema["format"] = json!("email"),
            ValidationRule::Alias(alias) => push_extension(schema, "x-blazingly-aliases", alias),
            ValidationRule::Custom(validator) => {
                // Declarative constraints are encoded as `keyword=value` inside
                // `Custom`; project the ones that map to a JSON Schema keyword
                // so an agent reads the real bound, not an opaque string.
                if let Some(metadata) = FieldMetadata::parse(validator) {
                    apply_field_metadata(schema, &metadata);
                    continue;
                }
                #[cfg(feature = "validation")]
                if let Some(constraint) = blazingly_validation::Constraint::parse(validator) {
                    constraint.apply_json_schema(schema);
                    continue;
                }
                push_extension(schema, "x-blazingly-validators", validator);
            }
            ValidationRule::Nested => {
                schema["x-blazingly-nested-validation"] = Value::Bool(true);
            }
        }
    }
}

/// Appends one name to a document extension array, at most once.
///
/// A field declared with a value type is projected twice — once from the type's
/// own constraints, once from the rules the field inherited from it — and a
/// keyword that overwrites is idempotent where an array is not.
fn push_extension(schema: &mut Value, keyword: &str, name: &str) {
    let names = schema
        .as_object_mut()
        .expect("validation schema must be an object")
        .entry(keyword)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .expect("a document extension list must be an array");
    if !names.iter().any(|declared| declared.as_str() == Some(name)) {
        names.push(Value::String(name.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use blazingly_core::{
        AgentPolicy, App, Confirmation, FieldDescriptor, HttpMethod, McpToolDescriptor,
        ModelDescriptor, OperationDescriptor, OperationRisk, ResponseDescriptor, SchemaKind,
        TypeDescriptor, ValidationRule,
    };

    #[test]
    fn discovery_contains_only_explicit_mcp_tools() {
        let tool = OperationDescriptor::new(
            HttpMethod::Post,
            "/users",
            "users.create",
            "Create a user",
            Some(TypeDescriptor::new("CreateUser")),
            vec![ResponseDescriptor::success(
                201,
                Some(TypeDescriptor::new("UserView")),
            )],
        )
        .expect("operation should be valid")
        .with_mcp_tool(
            McpToolDescriptor::new("create_user", "Create one user"),
            AgentPolicy {
                risk: OperationRisk::Write,
                confirmation: Confirmation::Required,
                idempotent: false,
            },
        );
        let http_only = OperationDescriptor::new(
            HttpMethod::Get,
            "/health",
            "health.read",
            "Read health",
            None,
            vec![ResponseDescriptor::success(
                200,
                Some(TypeDescriptor::new("Health")),
            )],
        )
        .expect("operation should be valid");
        let app = App::new()
            .routes([tool, http_only])
            .build()
            .expect("application should be valid");

        let discovery = super::to_value(&app);

        assert_eq!(discovery["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(discovery["tools"][0]["name"], "create_user");
        assert_eq!(
            discovery["tools"][0]["x-blazingly"]["confirmation"],
            "required"
        );
    }

    /// An agent has to read the bound on each element, not only on the list.
    ///
    /// A tool schema that omits the item contract invites a call the server
    /// then rejects, so a rule scoped to the items lands on the item schema
    /// rather than beside the array or in the opaque validator list.
    #[test]
    fn an_item_bundle_reaches_the_item_schema_a_tool_publishes() {
        let tag = TypeDescriptor::scalar("Tag", SchemaKind::String).with_constraints(vec![
            ValidationRule::MinLength(1),
            ValidationRule::MaxLength(20),
            ValidationRule::Custom("enum=news|sport".to_owned()),
        ]);
        let tags = TypeDescriptor {
            rust_name: "Vec<Tag>".to_owned(),
            schema: SchemaKind::Array(Box::new(SchemaKind::String)),
            model: None,
            items: Some(Box::new(tag)),
            constraints: Vec::new(),
        };
        let model = ModelDescriptor::new(
            "CreatePost",
            vec![FieldDescriptor::new(
                "tags",
                true,
                tags,
                vec![ValidationRule::Custom("max_items=5".to_owned())],
            )],
        );
        let operation = OperationDescriptor::new(
            HttpMethod::Post,
            "/posts",
            "posts.create",
            "Create a post",
            Some(TypeDescriptor::model(model)),
            vec![ResponseDescriptor::success(201, None)],
        )
        .expect("operation should be valid")
        .with_mcp_tool(
            McpToolDescriptor::new("create_post", "Create one post"),
            AgentPolicy {
                risk: OperationRisk::Write,
                confirmation: Confirmation::Never,
                idempotent: false,
            },
        );
        let app = App::new()
            .route(operation)
            .build()
            .expect("application should be valid");

        let discovery = super::to_value(&app);
        let tags = &discovery["tools"][0]["inputSchema"]["properties"]["tags"];

        // `max_items` travels in the `Custom` channel, which only the
        // constraint reader turned on by `validation` can decode.
        #[cfg(feature = "validation")]
        assert_eq!(tags["maxItems"], blazingly_json::json!(5));
        assert_eq!(
            tags["items"]["minLength"],
            blazingly_json::json!(1),
            "an item bound belongs to the item schema: {tags}"
        );
        assert_eq!(tags["items"]["maxLength"], blazingly_json::json!(20));
        assert_eq!(
            tags["items"]["enum"],
            blazingly_json::json!(["news", "sport"])
        );
        assert!(
            tags["maxLength"].is_null(),
            "an item bound must not be read as a bound on the list: {tags}"
        );
        #[cfg(feature = "validation")]
        assert!(
            tags["x-blazingly-validators"].is_null()
                && tags["items"]["x-blazingly-validators"].is_null(),
            "a recovered item rule must not also appear as an opaque validator: {tags}"
        );
    }

    #[test]
    fn recovered_metadata_projects_real_json_schema_keywords() {
        let author = ModelDescriptor::new(
            "Author",
            vec![FieldDescriptor::new(
                "name",
                true,
                TypeDescriptor::scalar("String", SchemaKind::String),
                Vec::new(),
            )],
        );
        let model = ModelDescriptor::new(
            "SearchArticles",
            vec![
                FieldDescriptor::new(
                    "limit",
                    false,
                    TypeDescriptor::scalar("u32", SchemaKind::Integer),
                    vec![ValidationRule::Custom("default=20".to_owned())],
                ),
                FieldDescriptor::new(
                    "language",
                    false,
                    TypeDescriptor::scalar("String", SchemaKind::String),
                    vec![ValidationRule::Custom("enum=uk|ru|en".to_owned())],
                ),
                FieldDescriptor::new(
                    "subtitle",
                    false,
                    TypeDescriptor::scalar("String", SchemaKind::String),
                    vec![ValidationRule::Custom("nullable=true".to_owned())],
                ),
                FieldDescriptor::new(
                    "author",
                    false,
                    TypeDescriptor::model(author),
                    vec![ValidationRule::Custom("nullable=true".to_owned())],
                ),
                FieldDescriptor::new(
                    "code",
                    false,
                    TypeDescriptor::scalar("String", SchemaKind::String),
                    vec![ValidationRule::Custom("validate_code".to_owned())],
                ),
            ],
        );
        let operation = OperationDescriptor::new(
            HttpMethod::Post,
            "/articles/search",
            "articles.search",
            "Search articles",
            Some(TypeDescriptor::model(model)),
            vec![ResponseDescriptor::success(200, None)],
        )
        .expect("operation should be valid")
        .with_mcp_tool(
            McpToolDescriptor::new("search_articles", "Search articles"),
            AgentPolicy {
                risk: OperationRisk::Read,
                confirmation: Confirmation::Never,
                idempotent: true,
            },
        );
        let app = App::new()
            .route(operation)
            .build()
            .expect("application should be valid");

        let discovery = super::to_value(&app);
        let properties = &discovery["tools"][0]["inputSchema"]["properties"];

        assert_eq!(properties["limit"]["default"], blazingly_json::json!(20));
        assert_eq!(
            properties["language"]["enum"],
            blazingly_json::json!(["uk", "ru", "en"])
        );
        assert_eq!(
            properties["subtitle"]["type"],
            blazingly_json::json!(["string", "null"]),
            "JSON Schema 2020-12 has no `nullable` keyword"
        );
        assert_eq!(
            properties["author"]["type"],
            blazingly_json::json!(["object", "null"]),
            "an inlined model schema widens its own type"
        );
        for field in ["limit", "language", "subtitle", "author"] {
            assert!(
                properties[field]["x-blazingly-validators"].is_null(),
                "recovered metadata on `{field}` must not also appear as an opaque validator"
            );
        }
        assert_eq!(
            properties["code"]["x-blazingly-validators"],
            blazingly_json::json!(["validate_code"]),
            "an opaque custom validator stays in the extension array"
        );
    }
}
