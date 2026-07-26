#![forbid(unsafe_code)]

mod audit;
mod http;
mod jsonrpc;
mod registry;

pub use audit::{AuditEvent, AuditOutcome, AuditSink, BoundedAuditLog};
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
    AppDefinition, Confirmation, InputDescriptor, InputSource, ModelDescriptor,
    OperationDescriptor, OperationRisk, OutputExposure, SchemaKind, TypeDescriptor, ValidationRule,
};
use blazingly_executor::{ExecutableApp, ExecutionOutcome};
use serde::Serialize;
use serde_json::Map;
use serde_json::{Value, json};
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
            } => {
                let body = body
                    .map(|body| {
                        serde_json::from_slice(&body).map_err(|error| McpProtocolError {
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
                        serde_json::from_slice::<Value>(&details).map_err(|decode| {
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
    }
}

fn schema_value(descriptor: &TypeDescriptor) -> Value {
    if let Some(model) = &descriptor.model {
        return model_schema(model);
    }

    let mut value = match (&descriptor.schema, &descriptor.items) {
        (SchemaKind::Array(_), Some(items)) => {
            json!({ "type": "array", "items": schema_value(items) })
        }
        _ => schema_kind_value(&descriptor.schema),
    };
    value["x-rust-type"] = Value::String(descriptor.rust_name.clone());
    value
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

fn apply_validation(schema: &mut Value, validation: &[ValidationRule]) {
    for rule in validation {
        match rule {
            ValidationRule::MinLength(value) => schema["minLength"] = json!(value),
            ValidationRule::MaxLength(value) => schema["maxLength"] = json!(value),
            ValidationRule::Email => schema["format"] = json!("email"),
        }
    }
}

#[cfg(test)]
mod tests {
    use blazingly_core::{
        AgentPolicy, App, Confirmation, HttpMethod, McpToolDescriptor, OperationDescriptor,
        OperationRisk, ResponseDescriptor, TypeDescriptor,
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
}
