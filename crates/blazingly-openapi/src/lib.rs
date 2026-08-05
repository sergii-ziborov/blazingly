#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

use blazingly_core::{
    AppDefinition, InputDescriptor, InputSource, ModelDescriptor, OperationDescriptor, SchemaKind,
    SecurityLocation, SecuritySchemeDescriptor, SecuritySchemeKind, TypeDescriptor, ValidationRule,
};
use blazingly_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Recursion budget for a schema-derived example payload.
///
/// A model reached through more than this many `$ref` or property hops
/// contributes `null` instead of another nesting level, so a self-referential
/// schema cannot make document generation diverge.
const MAX_EXAMPLE_DEPTH: usize = 8;

/// Browser UI rendered by [`OpenApiService`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenApiUi {
    Scalar,
    Swagger,
    Disabled,
}

/// `OpenAPI` document metadata and well-known HTTP paths.
// Not `Eq`: the overlay is arbitrary JSON, and JSON numbers are not.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenApiConfig {
    pub title: String,
    pub version: String,
    pub document_path: String,
    pub ui_path: String,
    pub ui: OpenApiUi,
    /// Prose for the document as a whole, shown above the operation list.
    pub description: Option<String>,
    /// The base URLs this document describes.
    pub servers: Vec<OpenApiServer>,
    /// Prose for a tag, keyed by tag name. A tag with no entry is still listed;
    /// it simply carries no description.
    pub tag_descriptions: BTreeMap<String, String>,
    /// Anything the projection does not generate, merged into the finished
    /// document. See [`OpenApiConfig::with_overlay`].
    pub overlay: Option<Value>,
}

/// One base URL the document describes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenApiServer {
    pub url: String,
    pub description: Option<String>,
}

impl OpenApiServer {
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            description: None,
        }
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

impl OpenApiConfig {
    #[must_use]
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            version: version.into(),
            document_path: "/openapi.json".to_owned(),
            ui_path: "/docs".to_owned(),
            ui: OpenApiUi::Scalar,
            description: None,
            servers: Vec::new(),
            tag_descriptions: BTreeMap::new(),
            overlay: None,
        }
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    #[must_use]
    pub fn with_server(mut self, server: OpenApiServer) -> Self {
        self.servers.push(server);
        self
    }

    #[must_use]
    pub fn with_tag_description(
        mut self,
        tag: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        self.tag_descriptions.insert(tag.into(), description.into());
        self
    }

    /// Merges arbitrary OpenAPI into the generated document.
    ///
    /// This is the escape hatch for everything the framework cannot derive from
    /// a handler signature — `callbacks`, `webhooks`, `info.contact`,
    /// `info.license`, vendor extensions, prose on an individual response.
    ///
    /// The merge is **additive**: it writes a key only where the generated
    /// document has none, recursing into objects it shares. It can therefore
    /// never overwrite a schema, a status code, a parameter, or a security
    /// requirement that was projected from the code, so the property that makes
    /// the document trustworthy — the machine-checkable parts cannot drift from
    /// what the runtime enforces — survives having an escape hatch at all.
    #[must_use]
    pub fn with_overlay(mut self, overlay: Value) -> Self {
        self.overlay = Some(overlay);
        self
    }

    #[must_use]
    pub fn with_document_path(mut self, path: impl Into<String>) -> Self {
        self.document_path = path.into();
        self
    }

    #[must_use]
    pub fn with_ui_path(mut self, path: impl Into<String>) -> Self {
        self.ui_path = path.into();
        self
    }

    #[must_use]
    pub const fn with_ui(mut self, ui: OpenApiUi) -> Self {
        self.ui = ui;
        self
    }
}

impl Default for OpenApiConfig {
    fn default() -> Self {
        Self::new("Blazingly application", env!("CARGO_PKG_VERSION"))
    }
}

/// One runtime-neutral `OpenAPI` HTTP asset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenApiAssetResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

/// Precompiled `/openapi.json` and Scalar/Swagger UI assets.
///
/// Document generation and HTML assembly happen once during application
/// construction, never on the request hot path.
#[derive(Clone, Debug)]
pub struct OpenApiService {
    config: OpenApiConfig,
    document: Vec<u8>,
    ui: Option<Vec<u8>>,
}

impl OpenApiService {
    #[must_use]
    pub fn new(app: &AppDefinition, config: OpenApiConfig) -> Self {
        let document = to_value_with_config(app, &config).to_string().into_bytes();
        let ui = match config.ui {
            OpenApiUi::Scalar => Some(scalar_html(&config).into_bytes()),
            OpenApiUi::Swagger => Some(swagger_html(&config).into_bytes()),
            OpenApiUi::Disabled => None,
        };
        Self {
            config,
            document,
            ui,
        }
    }

    /// Returns a precompiled response when `path` belongs to this service.
    #[must_use]
    pub fn handle(
        &self,
        method: blazingly_core::HttpMethod,
        path: &str,
    ) -> Option<OpenApiAssetResponse> {
        let (body, content_type) = if path == self.config.document_path {
            (&self.document, "application/json")
        } else if path == self.config.ui_path {
            (self.ui.as_ref()?, "text/html; charset=utf-8")
        } else {
            return None;
        };
        if !matches!(
            method,
            blazingly_core::HttpMethod::Get | blazingly_core::HttpMethod::Head
        ) {
            return Some(OpenApiAssetResponse {
                status: 405,
                headers: BTreeMap::from([
                    ("allow".to_owned(), "GET, HEAD".to_owned()),
                    (
                        "content-type".to_owned(),
                        "text/plain; charset=utf-8".to_owned(),
                    ),
                ]),
                body: b"OpenAPI assets only support GET and HEAD".to_vec(),
            });
        }
        Some(OpenApiAssetResponse {
            status: 200,
            headers: BTreeMap::from([
                ("content-type".to_owned(), content_type.to_owned()),
                (
                    "cache-control".to_owned(),
                    "no-cache, no-store, must-revalidate".to_owned(),
                ),
            ]),
            body: body.clone(),
        })
    }

    #[must_use]
    pub const fn config(&self) -> &OpenApiConfig {
        &self.config
    }
}

/// Generates a deterministic `OpenAPI` 3.1 document from the application model.
#[must_use]
pub fn to_value(app: &AppDefinition) -> Value {
    to_value_with_config(app, &OpenApiConfig::default())
}

/// Generates a deterministic `OpenAPI` document with explicit application info.
#[must_use]
pub fn to_value_with_config(app: &AppDefinition, config: &OpenApiConfig) -> Value {
    let mut schemas = Map::new();
    for operation in app.operations() {
        for input in &operation.contract.inputs {
            collect_model(&input.ty, &mut schemas);
        }
        for response in &operation.contract.responses {
            if let Some(body) = &response.body {
                collect_model(body, &mut schemas);
            }
        }
    }

    // Examples resolve `$ref` against the component schemas, so every model is
    // collected before the first operation is projected.
    let mut paths = Map::new();
    for operation in app.operations() {
        let path = paths
            .entry(operation.http.path.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        let Value::Object(path_item) = path else {
            unreachable!("path entries are always OpenAPI path objects");
        };
        path_item.insert(
            operation.http.method.as_openapi_key().to_owned(),
            operation_value(operation, &schemas),
        );
    }

    let mut info = json!({
        "title": config.title,
        "version": config.version
    });
    if let Some(description) = &config.description {
        info["description"] = Value::String(description.clone());
    }
    let mut document = json!({
        "openapi": "3.1.0",
        "jsonSchemaDialect": "https://json-schema.org/draft/2020-12/schema",
        "info": info,
        "paths": paths
    });
    if !config.servers.is_empty() {
        document["servers"] = Value::Array(
            config
                .servers
                .iter()
                .map(|server| {
                    let mut value = json!({ "url": server.url });
                    if let Some(description) = &server.description {
                        value["description"] = Value::String(description.clone());
                    }
                    value
                })
                .collect(),
        );
    }
    let tags = app
        .operations()
        .iter()
        .flat_map(operation_tags)
        .collect::<BTreeSet<_>>();
    if !tags.is_empty() {
        document["tags"] = Value::Array(
            tags.into_iter()
                .map(|name| {
                    let mut value = json!({ "name": name });
                    if let Some(description) = config.tag_descriptions.get(name) {
                        value["description"] = Value::String(description.clone());
                    }
                    value
                })
                .collect(),
        );
    }
    let security_schemes = app
        .security_schemes()
        .iter()
        .map(|scheme| (scheme.name.clone(), security_scheme_value(scheme)))
        .collect::<Map<_, _>>();
    if !schemas.is_empty() || !security_schemes.is_empty() {
        let mut components = Map::new();
        if !schemas.is_empty() {
            components.insert("schemas".to_owned(), Value::Object(schemas));
        }
        if !security_schemes.is_empty() {
            components.insert(
                "securitySchemes".to_owned(),
                Value::Object(security_schemes),
            );
        }
        document["components"] = Value::Object(components);
    }
    if let Some(overlay) = &config.overlay {
        merge_additively(&mut document, overlay);
    }
    document
}

/// Writes `overlay` into `target` wherever `target` says nothing.
///
/// Objects present on both sides recurse. A key the projection already produced
/// is left alone, at every depth, which is what keeps the overlay from being
/// able to contradict the code.
fn merge_additively(target: &mut Value, overlay: &Value) {
    let (Value::Object(target), Value::Object(overlay)) = (target, overlay) else {
        return;
    };
    for (key, value) in overlay {
        match target.get_mut(key) {
            Some(existing) => merge_additively(existing, value),
            None => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
fn operation_value(operation: &OperationDescriptor, components: &Map<String, Value>) -> Value {
    let mut responses = operation
        .contract
        .responses
        .iter()
        .map(|response| {
            let mut value = json!({
                "description": response.error_message.as_deref().unwrap_or("Successful response")
            });

            if response.error_code.is_some() {
                value["content"] = json!({
                    "application/json": {
                        "schema": error_schema(response),
                        "example": error_example(response, components)
                    }
                });
            } else if let Some(body) = &response.body {
                value["content"] = json!({
                    (response_media_type(body)): media_type_value(schema_value(body), components)
                });
            }
            if let Some(code) = &response.error_code {
                value["x-blazingly-error-code"] = Value::String(code.clone());
            }
            if !response.headers.is_empty() {
                value["headers"] = Value::Object(
                    response
                        .headers
                        .iter()
                        .map(|header| {
                            (
                                header.name.clone(),
                                json!({
                                    "schema": { "type": "string" },
                                    "example": header.value
                                }),
                            )
                        })
                        .collect(),
                );
            }

            (response.status.to_string(), value)
        })
        .collect::<Map<_, _>>();

    // Derived, never declared: an input that is decoded at all can be
    // rejected before the handler runs — a malformed JSON body needs no rule
    // to fail — and an operation that declares its own 422 keeps the one it
    // declared.
    if let Some(codes) = rejection_codes(operation)
        && !responses.contains_key(REJECTION_STATUS)
    {
        responses.insert(
            REJECTION_STATUS.to_owned(),
            rejection_response(&codes, components),
        );
    }

    // Same rule as the 422: the framework answers these itself, so the document
    // should say so, and an operation that declares one keeps its own.
    for (status, response) in security_responses(operation) {
        responses.entry(status.to_owned()).or_insert(response);
    }

    let mut value = json!({
        "operationId": operation.contract.id.as_str(),
        "summary": operation.contract.summary,
        "responses": responses,
        "x-blazingly-agent": operation.contract.agent
    });
    let tags = operation_tags(operation);
    if !tags.is_empty() {
        value["tags"] = json!(tags);
    }
    if let Some(description) = operation_description(operation) {
        value["description"] = Value::String(description.to_owned());
    }
    if operation.documentation.deprecated {
        value["deprecated"] = Value::Bool(true);
    }
    if let Some(external) = &operation.documentation.external_docs {
        let mut docs = json!({ "url": external.url });
        if let Some(description) = &external.description {
            docs["description"] = Value::String(description.clone());
        }
        value["externalDocs"] = docs;
    }
    if !operation.contract.dependencies.is_empty() {
        value["x-blazingly-dependencies"] = Value::Array(
            operation
                .contract
                .dependencies
                .iter()
                .map(|dependency| Value::String(dependency.rust_name.clone()))
                .collect(),
        );
    }
    if !operation.contract.security.is_empty() {
        let requirements = operation
            .contract
            .security
            .iter()
            .map(|requirement| (requirement.scheme.clone(), json!(requirement.scopes)))
            .collect::<Map<_, _>>();
        value["security"] = Value::Array(vec![Value::Object(requirements)]);
    }

    let parameters = operation
        .contract
        .inputs
        .iter()
        .filter(|input| {
            matches!(
                input.source,
                InputSource::Path | InputSource::Query | InputSource::Header | InputSource::Cookie
            )
        })
        .flat_map(|input| parameter_values(input, components))
        .collect::<Vec<_>>();
    if !parameters.is_empty() {
        value["parameters"] = Value::Array(parameters);
    }

    if let Some(input) = operation.contract.inputs.iter().find(|input| {
        matches!(
            input.source,
            InputSource::Json
                | InputSource::Form
                | InputSource::Multipart
                | InputSource::File
                | InputSource::Stream
        )
    }) {
        value["requestBody"] = json!({
            "required": input.required,
            "content": {
                (request_media_type(input.source)):
                    media_type_value(schema_value(&input.ty), components)
            }
        });
    }

    if let Some(tool) = &operation.contract.mcp {
        value["x-blazingly-mcp"] = json!({
            "name": tool.name,
            "description": tool.description,
            "risk": operation.contract.agent.risk,
            "confirmation": operation.contract.agent.confirmation,
            "idempotent": operation.contract.agent.idempotent,
            "outputExposure": tool.expose_output
        });
    }

    value
}

/// The section a browser UI groups this operation under.
///
/// The operation model has no tag field, so the group is the namespace of the
/// stable operation identity: `users.create` and `users.list` both belong to
/// `users`, and `billing.invoices.void` belongs to `billing.invoices`. An
/// identity without a namespace stays untagged rather than becoming a section
/// of its own.
/// The groups an operation files under.
///
/// A declared list wins outright. Nothing is declared for most operations, and
/// for those the namespace of the operation id is the tag — `users.create`
/// files under `users` — which is why the inferred form stays.
fn operation_tags(operation: &OperationDescriptor) -> Vec<&str> {
    if !operation.documentation.tags.is_empty() {
        return operation
            .documentation
            .tags
            .iter()
            .map(String::as_str)
            .collect();
    }
    operation
        .contract
        .id
        .as_str()
        .rsplit_once('.')
        .map(|(namespace, _)| namespace)
        .filter(|namespace| !namespace.is_empty())
        .into_iter()
        .collect()
}

/// Prose shown below the summary in a browser UI.
///
/// An explicitly declared description wins. Otherwise the contract carries one
/// long-form description, the one an operation declares for agents; it defaults
/// to the summary, so it is only projected when it says something the summary
/// does not.
fn operation_description(operation: &OperationDescriptor) -> Option<&str> {
    if let Some(declared) = &operation.documentation.description
        && !declared.is_empty()
    {
        return Some(declared.as_str());
    }
    let description = operation.contract.mcp.as_ref()?.description.as_str();
    (!description.is_empty() && description != operation.contract.summary).then_some(description)
}

/// The statuses the security pipeline itself answers with, before the handler.
///
/// These are derived from the same `security` declaration that drives
/// enforcement, exactly as the `422` is derived from what the operation
/// decodes: an operation that requires a scheme can be answered `401`, and one
/// that additionally requires a scope can be answered `403`. An operation that
/// declares either status itself keeps the one it declared.
fn security_responses(operation: &OperationDescriptor) -> Vec<(&'static str, Value)> {
    if operation.contract.security.is_empty() {
        return Vec::new();
    }
    let mut derived = vec![(
        UNAUTHORIZED_STATUS,
        security_response(
            "The request carried no acceptable credential for a security scheme this operation requires.",
        ),
    )];
    if operation
        .contract
        .security
        .iter()
        .any(|requirement| !requirement.scopes.is_empty())
    {
        derived.push((
            FORBIDDEN_STATUS,
            security_response(
                "The credential was accepted but does not carry every scope this operation requires.",
            ),
        ));
    }
    derived
}

fn security_response(description: &str) -> Value {
    json!({
        "description": description,
        "x-blazingly-automatic": true
    })
}

/// A media type entry carrying a schema and, when derivable, a sample payload.
fn media_type_value(schema: Value, components: &Map<String, Value>) -> Value {
    let example = example_for_schema(&schema, components, MAX_EXAMPLE_DEPTH);
    let mut value = Map::new();
    if !example.is_null() {
        value.insert("example".to_owned(), example);
    }
    value.insert("schema".to_owned(), schema);
    Value::Object(value)
}

/// The error envelope a failing operation actually returns.
fn error_example(
    response: &blazingly_core::ResponseDescriptor,
    components: &Map<String, Value>,
) -> Value {
    let mut error = json!({
        "code": response.error_code.as_deref().unwrap_or_default(),
        "message": response.error_message.as_deref().unwrap_or_default()
    });
    if let Some(details) = &response.body {
        error["details"] =
            example_for_schema(&schema_value(details), components, MAX_EXAMPLE_DEPTH);
    }
    json!({ "error": error })
}

/// Builds a sample payload from a generated schema node.
///
/// Deriving the example from the schema rather than from the descriptor means
/// every keyword the schema already carries — `format`, `minLength`, `const`,
/// `minimum`, and anything a later projection adds — constrains the sample
/// without a second traversal of the operation model.
fn example_for_schema(schema: &Value, components: &Map<String, Value>, depth: usize) -> Value {
    let (Some(object), 1..) = (schema.as_object(), depth) else {
        return Value::Null;
    };
    for keyword in ["example", "default", "const"] {
        if let Some(value) = object.get(keyword) {
            return value.clone();
        }
    }
    if let Some(first) = object
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
    {
        return first.clone();
    }
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        let name = reference
            .rsplit_once('/')
            .map_or(reference, |(_, name)| name);
        return components.get(name).map_or(Value::Null, |target| {
            example_for_schema(target, components, depth - 1)
        });
    }

    match schema_type(object) {
        Some("object") => example_object(object, components, depth),
        Some("array") => example_array(object, components, depth),
        Some("string") => Value::String(example_string(object)),
        Some("integer") => json!(example_integer(object)),
        Some("number") => json!(example_number(object)),
        Some("boolean") => Value::Bool(true),
        _ => Value::Null,
    }
}

/// The declared type, skipping the `"null"` member of a nullable union.
fn schema_type(schema: &Map<String, Value>) -> Option<&str> {
    match schema.get("type")? {
        Value::String(name) => Some(name.as_str()),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .find(|name| *name != "null"),
        _ => None,
    }
}

fn example_object(
    schema: &Map<String, Value>,
    components: &Map<String, Value>,
    depth: usize,
) -> Value {
    let Some(Value::Object(properties)) = schema.get("properties") else {
        return Value::Object(Map::new());
    };
    Value::Object(
        properties
            .iter()
            .map(|(name, property)| {
                (
                    name.clone(),
                    example_for_schema(property, components, depth - 1),
                )
            })
            .collect(),
    )
}

fn example_array(
    schema: &Map<String, Value>,
    components: &Map<String, Value>,
    depth: usize,
) -> Value {
    let item = schema.get("items").map_or(Value::Null, |items| {
        example_for_schema(items, components, depth - 1)
    });
    let items = schema
        .get("minItems")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .clamp(1, 3);
    Value::Array(vec![item; usize::try_from(items).unwrap_or(1)])
}

/// A sample string honouring the format, then the declared length window.
///
/// A formatted sample is returned verbatim: trimming an address to a
/// `maxLength` would only produce a payload the same document rejects.
fn example_string(schema: &Map<String, Value>) -> String {
    let sample = match schema.get("format").and_then(Value::as_str) {
        Some("email") => return "user@example.com".to_owned(),
        Some("uuid") => return "00000000-0000-4000-8000-000000000000".to_owned(),
        Some("uri") => return "https://example.com".to_owned(),
        Some("ip") => return "192.0.2.1".to_owned(),
        Some("date") => return "2024-01-01".to_owned(),
        Some("date-time") => return "2024-01-01T00:00:00Z".to_owned(),
        Some("decimal") => return "1.00".to_owned(),
        Some("binary") => return "ZXhhbXBsZQ==".to_owned(),
        _ => "example",
    };

    let minimum = schema
        .get("minLength")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(64);
    let maximum = schema
        .get("maxLength")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX);
    let mut value = sample.to_owned();
    while u64::try_from(value.len()).unwrap_or(u64::MAX) < minimum {
        value.push('x');
    }
    if u64::try_from(value.len()).unwrap_or(u64::MAX) > maximum {
        value.truncate(usize::try_from(maximum).unwrap_or(usize::MAX));
    }
    value
}

fn example_integer(schema: &Map<String, Value>) -> i64 {
    let mut value = 1_i64;
    if let Some(minimum) = schema.get("minimum").and_then(Value::as_i64) {
        value = value.max(minimum);
    }
    if let Some(minimum) = schema.get("exclusiveMinimum").and_then(Value::as_i64) {
        value = value.max(minimum.saturating_add(1));
    }
    if let Some(maximum) = schema.get("maximum").and_then(Value::as_i64) {
        value = value.min(maximum);
    }
    if let Some(maximum) = schema.get("exclusiveMaximum").and_then(Value::as_i64) {
        value = value.min(maximum.saturating_sub(1));
    }
    value
}

fn example_number(schema: &Map<String, Value>) -> f64 {
    let mut value = 1.0_f64;
    if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64) {
        value = value.max(minimum);
    }
    if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64) {
        value = value.min(maximum);
    }
    value
}

fn response_media_type(descriptor: &TypeDescriptor) -> &'static str {
    if matches!(descriptor.schema, SchemaKind::Binary) {
        "application/octet-stream"
    } else {
        "application/json"
    }
}

fn scalar_html(config: &OpenApiConfig) -> String {
    let title = escape_html(&config.title);
    let document_path = blazingly_json::to_string(&config.document_path)
        .unwrap_or_else(|_| "\"/openapi.json\"".into());
    format!(
        concat!(
            "<!doctype html><html><head><meta charset=\"utf-8\">",
            "<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">",
            "<title>{title}</title></head><body>",
            "<script id=\"api-reference\" data-url={document_path}></script>",
            "<script src=\"https://cdn.jsdelivr.net/npm/@scalar/api-reference\"></script>",
            "</body></html>"
        ),
        title = title,
        document_path = document_path,
    )
}

fn swagger_html(config: &OpenApiConfig) -> String {
    let title = escape_html(&config.title);
    let document_path = blazingly_json::to_string(&config.document_path)
        .unwrap_or_else(|_| "\"/openapi.json\"".into());
    format!(
        concat!(
            "<!doctype html><html><head><meta charset=\"utf-8\">",
            "<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">",
            "<title>{title}</title>",
            "<link rel=\"stylesheet\" href=\"https://cdn.jsdelivr.net/npm/swagger-ui-dist/swagger-ui.css\">",
            "</head><body><div id=\"swagger-ui\"></div>",
            "<script src=\"https://cdn.jsdelivr.net/npm/swagger-ui-dist/swagger-ui-bundle.js\"></script>",
            "<script>SwaggerUIBundle({{url:{document_path},dom_id:'#swagger-ui'}});</script>",
            "</body></html>"
        ),
        title = title,
        document_path = document_path,
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The status every input rejection is reported under.
const REJECTION_STATUS: &str = "422";
const UNAUTHORIZED_STATUS: &str = "401";
const FORBIDDEN_STATUS: &str = "403";

/// Every code a rejection can carry, in the order a reader most wants them.
///
/// `validation_error` leads because it is the failure a well-formed request
/// still meets, and the sample payload is derived from the first entry.
const REJECTION_CODES: [&str; 6] = [
    "validation_error",
    "missing_input",
    "invalid_json",
    "invalid_input",
    "invalid_multipart",
    "invalid_file_count",
];

/// The codes an input from this source can be rejected with.
///
/// This mirrors the executor: a value is decoded, then validated, and each step
/// answers with its own code. Bytes that reach the handler untouched have
/// neither step, so a stream can produce none of them.
fn source_rejection_codes(source: InputSource) -> &'static [&'static str] {
    match source {
        InputSource::Json => &["validation_error", "invalid_json"],
        InputSource::Path
        | InputSource::Query
        | InputSource::Header
        | InputSource::Cookie
        | InputSource::Form => &["validation_error", "invalid_input"],
        InputSource::Multipart => &["validation_error", "invalid_multipart", "invalid_input"],
        // An upload is read out of the same multipart document, then counted.
        // It is never validated against rules, and the decode that answers
        // `invalid_input` is reachable only from structured arguments.
        InputSource::File => &["invalid_multipart", "invalid_file_count"],
        InputSource::Stream => &[],
    }
}

/// The stable codes an operation's own inputs can be rejected with.
///
/// Returns `None` when nothing about the request is decoded, so an operation
/// that only streams bytes is not given a failure it cannot produce. The set is
/// closed and derived from the operation's own inputs: a body that is not JSON
/// cannot fail as `invalid_json`, and an operation with nothing required cannot
/// report `missing_input`.
fn rejection_codes(operation: &OperationDescriptor) -> Option<Vec<&'static str>> {
    let mut reachable = BTreeSet::new();
    for input in &operation.contract.inputs {
        let codes = source_rejection_codes(input.source);
        if codes.is_empty() {
            continue;
        }
        reachable.extend(codes);
        // Only a value read out of the request one key at a time can be found
        // missing. A JSON body is decoded whole and fails as `invalid_json`, a
        // model is assembled from whichever of its fields arrived and fails on
        // the field, and an upload reports its own absence by count.
        if !matches!(input.source, InputSource::Json | InputSource::File)
            && input.ty.model.is_none()
            && (input.required || input.source == InputSource::Path)
        {
            reachable.insert("missing_input");
        }
    }
    if reachable.is_empty() {
        return None;
    }
    Some(
        REJECTION_CODES
            .into_iter()
            .filter(|code| reachable.contains(code))
            .collect(),
    )
}

/// The rejection envelope the runtime returns before the handler is reached.
///
/// This response is projected from the framework's own input handling rather
/// than declared by the operation, which `x-blazingly-automatic` records. The
/// `violations` array is the shape a rule failure reports, one entry per broken
/// rule, each naming the field path that broke it.
fn rejection_response(codes: &[&str], components: &Map<String, Value>) -> Value {
    let schema = json!({
        "type": "object",
        "properties": {
            "error": {
                "type": "object",
                "properties": {
                    "code": { "type": "string", "enum": codes },
                    "message": { "type": "string" },
                    "details": {
                        "type": "object",
                        "properties": {
                            "violations": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "field": { "type": "string" },
                                        "code": { "type": "string" },
                                        "message": { "type": "string" }
                                    },
                                    "required": ["field", "code", "message"],
                                    "additionalProperties": false
                                }
                            }
                        }
                    }
                },
                "required": ["code", "message"],
                "additionalProperties": false
            }
        },
        "required": ["error"],
        "additionalProperties": false
    });
    json!({
        "description": "The request was rejected before the handler ran: an input could not be decoded, or failed the rules the operation declares.",
        "content": { "application/json": media_type_value(schema, components) },
        "x-blazingly-automatic": true
    })
}

fn error_schema(response: &blazingly_core::ResponseDescriptor) -> Value {
    let mut error_properties = json!({
        "code": {
            "type": "string",
            "const": response.error_code
        },
        "message": {
            "type": "string"
        }
    });
    let mut required = vec!["code", "message"];
    if let Some(details) = &response.body {
        error_properties["details"] = schema_value(details);
        required.push("details");
    }
    json!({
        "type": "object",
        "properties": {
            "error": {
                "type": "object",
                "properties": error_properties,
                "required": required,
                "additionalProperties": false
            }
        },
        "required": ["error"],
        "additionalProperties": false
    })
}

fn parameter_values(input: &InputDescriptor, components: &Map<String, Value>) -> Vec<Value> {
    let location = input_source_name(input.source);
    if let Some(model) = &input.ty.model {
        return model
            .fields
            .iter()
            .map(|field| {
                let mut schema = schema_value(&field.ty);
                apply_validation(&mut schema, &field.validation);
                parameter_value(
                    parameter_name(input.source, &field.name),
                    location,
                    input.source == InputSource::Path || (input.required && field.required),
                    schema,
                    components,
                )
            })
            .collect();
    }

    vec![parameter_value(
        parameter_name(input.source, &input.name),
        location,
        input.source == InputSource::Path || input.required,
        schema_value(&input.ty),
        components,
    )]
}

fn parameter_value(
    name: String,
    location: &'static str,
    required: bool,
    schema: Value,
    components: &Map<String, Value>,
) -> Value {
    let example = example_for_schema(&schema, components, MAX_EXAMPLE_DEPTH);
    let mut value = Map::new();
    value.insert("name".to_owned(), Value::String(name));
    value.insert("in".to_owned(), Value::String(location.to_owned()));
    value.insert("required".to_owned(), Value::Bool(required));
    value.insert("schema".to_owned(), schema);
    if !example.is_null() {
        value.insert("example".to_owned(), example);
    }
    Value::Object(value)
}

fn parameter_name(source: InputSource, name: &str) -> String {
    if source == InputSource::Header {
        name.replace('_', "-")
    } else {
        name.to_owned()
    }
}

fn input_source_name(source: InputSource) -> &'static str {
    match source {
        InputSource::Path => "path",
        InputSource::Query => "query",
        InputSource::Header => "header",
        InputSource::Cookie => "cookie",
        InputSource::Json
        | InputSource::Form
        | InputSource::Multipart
        | InputSource::File
        | InputSource::Stream => {
            unreachable!("body inputs are OpenAPI request bodies")
        }
    }
}

fn request_media_type(source: InputSource) -> &'static str {
    match source {
        InputSource::Json => "application/json",
        InputSource::Form => "application/x-www-form-urlencoded",
        InputSource::Multipart | InputSource::File => "multipart/form-data",
        InputSource::Stream => "application/octet-stream",
        InputSource::Path | InputSource::Query | InputSource::Header | InputSource::Cookie => {
            unreachable!("parameter inputs do not have a request body media type")
        }
    }
}

/// The format decisions that make the shared projection an `OpenAPI` one.
///
/// A model appears as a `$ref` into `#/components/schemas` — the component
/// itself is written once by [`collect_model`] — and raw bytes are spelled
/// `format: "binary"`. Declarative constraints that predate a contract
/// variant are decoded by the optional constraint reader.
struct OpenApiDialect;

impl blazingly_core::schema::SchemaDialect for OpenApiDialect {
    fn model_node(&self, descriptor: &TypeDescriptor, model: &ModelDescriptor) -> Value {
        json!({
            "$ref": format!("#/components/schemas/{}", model.name),
            "x-rust-type": descriptor.rust_name
        })
    }

    fn binary_node(&self) -> Value {
        json!({ "type": "string", "format": "binary" })
    }

    #[cfg(feature = "validation")]
    fn project_custom_validator(&self, schema: &mut Value, validator: &str) -> bool {
        let Some(constraint) = blazingly_validation::Constraint::parse(validator) else {
            return false;
        };
        constraint.apply_json_schema(schema);
        true
    }
}

fn schema_value(descriptor: &TypeDescriptor) -> Value {
    blazingly_core::schema::schema_value(&OpenApiDialect, descriptor)
}

fn collect_model(descriptor: &TypeDescriptor, schemas: &mut Map<String, Value>) {
    if let Some(items) = &descriptor.items {
        collect_model(items, schemas);
    }
    if let Some(model) = &descriptor.model {
        if schemas.contains_key(&model.name) {
            return;
        }
        schemas.insert(model.name.clone(), model_schema(model));
        for field in &model.fields {
            collect_model(&field.ty, schemas);
        }
    }
}

fn security_scheme_value(scheme: &SecuritySchemeDescriptor) -> Value {
    let mut value = match &scheme.kind {
        SecuritySchemeKind::ApiKey { location, name } => json!({
            "type": "apiKey",
            "in": match location {
                SecurityLocation::Header => "header",
                SecurityLocation::Query => "query",
                SecurityLocation::Cookie => "cookie",
            },
            "name": name
        }),
        SecuritySchemeKind::Http {
            scheme,
            bearer_format,
        } => {
            let mut value = json!({ "type": "http", "scheme": scheme });
            if let Some(bearer_format) = bearer_format {
                value["bearerFormat"] = Value::String(bearer_format.clone());
            }
            value
        }
        SecuritySchemeKind::OAuth2 {
            authorization_url,
            token_url,
            scopes,
        } => {
            let scopes = scopes
                .iter()
                .map(|scope| (scope.clone(), Value::String(String::new())))
                .collect::<Map<_, _>>();
            let mut flows = Map::new();
            match (authorization_url, token_url) {
                (Some(authorization_url), Some(token_url)) => {
                    flows.insert(
                        "authorizationCode".to_owned(),
                        json!({
                            "authorizationUrl": authorization_url,
                            "tokenUrl": token_url,
                            "scopes": scopes
                        }),
                    );
                }
                (Some(authorization_url), None) => {
                    flows.insert(
                        "implicit".to_owned(),
                        json!({ "authorizationUrl": authorization_url, "scopes": scopes }),
                    );
                }
                (None, Some(token_url)) => {
                    flows.insert(
                        "clientCredentials".to_owned(),
                        json!({ "tokenUrl": token_url, "scopes": scopes }),
                    );
                }
                (None, None) => {}
            }
            json!({ "type": "oauth2", "flows": flows })
        }
        SecuritySchemeKind::OpenIdConnect { discovery_url } => {
            json!({ "type": "openIdConnect", "openIdConnectUrl": discovery_url })
        }
        SecuritySchemeKind::MutualTls => json!({ "type": "mutualTLS" }),
    };
    if let Some(description) = &scheme.description {
        value["description"] = Value::String(description.clone());
    }
    value
}

fn model_schema(model: &ModelDescriptor) -> Value {
    blazingly_core::schema::model_schema(&OpenApiDialect, model)
}

fn apply_validation(schema: &mut Value, validation: &[ValidationRule]) {
    blazingly_core::schema::apply_validation(&OpenApiDialect, schema, validation);
}

#[cfg(test)]
mod tests {
    use blazingly_core::{
        AgentPolicy, App, FieldDescriptor, HttpMethod, InputDescriptor, InputSource,
        McpToolDescriptor, ModelDescriptor, OperationDescriptor, ResponseDescriptor, SchemaKind,
        SecurityRequirement, SecuritySchemeDescriptor, SecuritySchemeKind, TypeDescriptor,
        ValidationRule,
    };

    fn create_user_model() -> ModelDescriptor {
        ModelDescriptor::new(
            "CreateUser",
            vec![
                FieldDescriptor::new(
                    "name",
                    true,
                    TypeDescriptor::scalar("String", SchemaKind::String),
                    vec![ValidationRule::MinLength(12)],
                ),
                FieldDescriptor::new(
                    "email",
                    true,
                    TypeDescriptor::scalar("String", SchemaKind::String),
                    vec![ValidationRule::Email],
                ),
                FieldDescriptor::new(
                    "age",
                    false,
                    TypeDescriptor::scalar("u8", SchemaKind::Integer),
                    Vec::new(),
                ),
            ],
        )
    }

    #[test]
    fn openapi_is_projected_from_the_operation_model() {
        let operation = OperationDescriptor::new(
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
        .expect("operation should be valid");
        let app = App::new()
            .route(operation)
            .build()
            .expect("application should be valid");

        let document = super::to_value(&app);

        assert_eq!(
            document["paths"]["/users"]["post"]["operationId"],
            "users.create"
        );
        assert_eq!(
            document["paths"]["/users"]["post"]["requestBody"]["content"]["application/json"]["schema"]
                ["x-rust-type"],
            "CreateUser"
        );
        assert_eq!(
            document["paths"]["/users"]["post"]["responses"]["201"]["content"]["application/json"]
                ["schema"]["x-rust-type"],
            "UserView"
        );
    }

    #[test]
    fn openapi_projects_registered_security_and_operation_scopes() {
        let operation = OperationDescriptor::new(
            HttpMethod::Get,
            "/users",
            "users.list",
            "List users",
            None,
            vec![ResponseDescriptor::success(200, None)],
        )
        .unwrap()
        .with_security(vec![
            SecurityRequirement::new("oauth").with_scopes(vec!["users:read".to_owned()]),
        ]);
        let app = App::new()
            .route(operation)
            .security_scheme(SecuritySchemeDescriptor::new(
                "oauth",
                SecuritySchemeKind::OAuth2 {
                    authorization_url: Some("https://auth.example/authorize".to_owned()),
                    token_url: Some("https://auth.example/token".to_owned()),
                    scopes: vec!["users:read".to_owned()],
                },
            ))
            .build()
            .unwrap();

        let document = super::to_value(&app);
        assert_eq!(
            document["components"]["securitySchemes"]["oauth"]["flows"]["authorizationCode"]["tokenUrl"],
            "https://auth.example/token"
        );
        assert_eq!(
            document["paths"]["/users"]["get"]["security"][0]["oauth"][0],
            "users:read"
        );
    }

    #[test]
    fn operations_are_grouped_by_the_namespace_of_their_identity() {
        let create = OperationDescriptor::new(
            HttpMethod::Post,
            "/users",
            "users.create",
            "Create a user",
            None,
            vec![ResponseDescriptor::success(201, None)],
        )
        .unwrap();
        let list = OperationDescriptor::new(
            HttpMethod::Get,
            "/users",
            "users.list",
            "List users",
            None,
            vec![ResponseDescriptor::success(200, None)],
        )
        .unwrap();
        let health = OperationDescriptor::new(
            HttpMethod::Get,
            "/health",
            "health",
            "Report health",
            None,
            vec![ResponseDescriptor::success(200, None)],
        )
        .unwrap();
        let app = App::new()
            .route(create)
            .route(list)
            .route(health)
            .build()
            .unwrap();

        let document = super::to_value(&app);

        assert_eq!(document["paths"]["/users"]["post"]["tags"][0], "users");
        assert_eq!(document["paths"]["/users"]["get"]["tags"][0], "users");
        assert_eq!(
            document["tags"].as_array().map(Vec::len),
            Some(1),
            "one section per namespace: {}",
            document["tags"]
        );
        assert_eq!(document["tags"][0]["name"], "users");
        assert!(
            document["paths"]["/health"]["get"]["tags"].is_null(),
            "an identity without a namespace stays untagged"
        );
    }

    #[test]
    fn a_long_description_is_projected_only_when_it_adds_to_the_summary() {
        let described = OperationDescriptor::new(
            HttpMethod::Post,
            "/users",
            "users.create",
            "Create a user",
            None,
            vec![ResponseDescriptor::success(201, None)],
        )
        .unwrap()
        .with_mcp_tool(
            McpToolDescriptor::new("create_user", "Registers one user and returns its view."),
            AgentPolicy::default(),
        );
        let echoed = OperationDescriptor::new(
            HttpMethod::Get,
            "/users",
            "users.list",
            "List users",
            None,
            vec![ResponseDescriptor::success(200, None)],
        )
        .unwrap()
        .with_mcp_tool(
            McpToolDescriptor::new("list_users", "List users"),
            AgentPolicy::default(),
        );
        let app = App::new().route(described).route(echoed).build().unwrap();

        let document = super::to_value(&app);

        assert_eq!(
            document["paths"]["/users"]["post"]["description"],
            "Registers one user and returns its view."
        );
        assert!(document["paths"]["/users"]["get"]["description"].is_null());
    }

    #[test]
    fn bodies_and_parameters_carry_examples_that_satisfy_their_own_schema() {
        let operation = OperationDescriptor::new(
            HttpMethod::Post,
            "/tenants/{tenant_id}/users",
            "users.create",
            "Create a user",
            None,
            vec![
                ResponseDescriptor::success(201, Some(TypeDescriptor::model(create_user_model()))),
                ResponseDescriptor::error(
                    409,
                    "email_already_exists",
                    "A user with this email already exists.",
                    None,
                ),
            ],
        )
        .unwrap()
        .with_inputs(vec![
            InputDescriptor::new(
                "tenant_id",
                InputSource::Path,
                true,
                TypeDescriptor::scalar("Uuid", SchemaKind::String),
            ),
            InputDescriptor::new(
                "body",
                InputSource::Json,
                true,
                TypeDescriptor::model(create_user_model()),
            ),
        ]);
        let app = App::new().route(operation).build().unwrap();

        let document = super::to_value(&app);
        let operation = &document["paths"]["/tenants/{tenant_id}/users"]["post"];

        let request = &operation["requestBody"]["content"]["application/json"]["example"];
        assert_eq!(request["email"], "user@example.com");
        assert_eq!(
            request["name"], "examplexxxxx",
            "a sample must reach its own minLength"
        );
        assert_eq!(request["age"], 1);
        assert_eq!(
            operation["responses"]["201"]["content"]["application/json"]["example"]["email"],
            "user@example.com"
        );
        assert_eq!(operation["parameters"][0]["name"], "tenant_id");
        assert_eq!(
            operation["parameters"][0]["example"],
            "00000000-0000-4000-8000-000000000000"
        );

        let failure = &operation["responses"]["409"]["content"]["application/json"]["example"];
        assert_eq!(failure["error"]["code"], "email_already_exists");
        assert_eq!(
            failure["error"]["message"],
            "A user with this email already exists."
        );
    }

    #[test]
    fn every_declared_tag_and_example_keeps_the_document_well_formed() {
        let operation = OperationDescriptor::new(
            HttpMethod::Post,
            "/users",
            "users.create",
            "Create a user",
            Some(TypeDescriptor::model(create_user_model())),
            vec![
                ResponseDescriptor::success(201, Some(TypeDescriptor::model(create_user_model()))),
                ResponseDescriptor::error(409, "conflict", "Already exists.", None),
            ],
        )
        .unwrap();
        let app = App::new().route(operation).build().unwrap();

        let document = super::to_value(&app);
        let declared = document["tags"]
            .as_array()
            .expect("a grouped document declares its tags")
            .iter()
            .map(|tag| {
                tag["name"]
                    .as_str()
                    .expect("every tag object names a section")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(declared, ["users"]);

        for (_, path_item) in document["paths"].as_object().expect("paths is an object") {
            for (_, operation) in path_item.as_object().expect("a path item is an object") {
                for tag in operation["tags"].as_array().into_iter().flatten() {
                    let tag = tag.as_str().expect("an operation tag is a string");
                    assert!(
                        declared.iter().any(|declared| declared == tag),
                        "operation tag {tag} is not declared at the document root"
                    );
                }
                for (_, response) in operation["responses"]
                    .as_object()
                    .expect("responses is an object")
                {
                    for (_, media) in response["content"].as_object().into_iter().flatten() {
                        assert!(
                            !media["schema"].is_null(),
                            "an example must accompany a schema, not replace it"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn recorded_defaults_enumerations_and_nullability_use_openapi_31_spelling() {
        let model = ModelDescriptor::new(
            "Article",
            vec![
                FieldDescriptor::new(
                    "status",
                    false,
                    TypeDescriptor::scalar("String", SchemaKind::String),
                    vec![
                        ValidationRule::Custom("enum=draft|published".to_owned()),
                        ValidationRule::Custom("default=\"draft\"".to_owned()),
                    ],
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
                    TypeDescriptor::model(create_user_model()),
                    vec![ValidationRule::Custom("nullable=true".to_owned())],
                ),
            ],
        );
        let operation = OperationDescriptor::new(
            HttpMethod::Post,
            "/articles",
            "articles.create",
            "Create an article",
            Some(TypeDescriptor::model(model)),
            vec![ResponseDescriptor::success(201, None)],
        )
        .unwrap();
        let app = App::new().route(operation).build().unwrap();

        let document = super::to_value(&app);
        let properties = &document["components"]["schemas"]["Article"]["properties"];

        assert_eq!(properties["status"]["default"], "draft");
        assert_eq!(properties["status"]["enum"][0], "draft");
        assert_eq!(properties["status"]["enum"][1], "published");
        assert_eq!(
            properties["subtitle"]["type"],
            blazingly_json::json!(["string", "null"]),
            "3.1 has no `nullable` keyword"
        );
        assert_eq!(
            properties["author"]["anyOf"][0]["$ref"], "#/components/schemas/CreateUser",
            "a nullable reference widens through anyOf"
        );
        assert_eq!(properties["author"]["anyOf"][1]["type"], "null");
        assert!(
            properties["status"]["x-blazingly-validators"].is_null(),
            "recovered metadata must not also appear as an opaque validator"
        );
        assert_eq!(
            document["paths"]["/articles"]["post"]["requestBody"]["content"]["application/json"]["example"]
                ["status"],
            "draft",
            "a declared default is the most useful sample value"
        );
    }

    /// `#[api_model] #[min_length(1)] #[max_length(20)] struct Tag(String);`
    fn tag() -> TypeDescriptor {
        TypeDescriptor::scalar("Tag", SchemaKind::String).with_constraints(vec![
            ValidationRule::MinLength(1),
            ValidationRule::MaxLength(20),
        ])
    }

    fn collection_of(item: TypeDescriptor) -> TypeDescriptor {
        TypeDescriptor {
            rust_name: format!("Vec<{}>", item.rust_name),
            schema: SchemaKind::Array(Box::new(item.schema.clone())),
            model: None,
            items: Some(Box::new(item)),
            constraints: Vec::new(),
        }
    }

    fn create_note_model() -> ModelDescriptor {
        ModelDescriptor::new(
            "CreateNote",
            vec![
                FieldDescriptor::new(
                    "tags",
                    true,
                    collection_of(tag()),
                    vec![ValidationRule::Custom("max_items=5".to_owned())],
                ),
                FieldDescriptor::new("primary", true, tag(), Vec::new()),
                FieldDescriptor::new(
                    "groups",
                    true,
                    collection_of(collection_of(tag())),
                    Vec::new(),
                ),
            ],
        )
    }

    fn note_operation() -> OperationDescriptor {
        OperationDescriptor::new(
            HttpMethod::Post,
            "/notes",
            "notes.create",
            "Create a note",
            Some(TypeDescriptor::model(create_note_model())),
            vec![ResponseDescriptor::success(201, None)],
        )
        .unwrap()
    }

    #[test]
    fn a_value_types_bounds_reach_every_place_the_type_appears() {
        let app = App::new().route(note_operation()).build().unwrap();

        let document = super::to_value(&app);
        let properties = &document["components"]["schemas"]["CreateNote"]["properties"];

        let item = &properties["tags"]["items"];
        assert_eq!(item["x-rust-type"], "Tag");
        assert_eq!(item["minLength"], 1, "a collection item keeps its bounds");
        assert_eq!(item["maxLength"], 20);
        // `max_items` travels in the `Custom` channel, which only the
        // constraint reader turned on by `validation` can decode.
        #[cfg(feature = "validation")]
        assert_eq!(
            properties["tags"]["maxItems"], 5,
            "the field's own bound still describes the collection"
        );

        assert_eq!(properties["primary"]["minLength"], 1);
        assert_eq!(properties["primary"]["maxLength"], 20);

        let nested = &properties["groups"]["items"]["items"];
        assert_eq!(nested["x-rust-type"], "Tag");
        assert_eq!(nested["minLength"], 1, "nesting does not lose the bounds");
        assert_eq!(nested["maxLength"], 20);
    }

    #[test]
    fn an_inherited_rule_is_not_listed_twice() {
        let validated = TypeDescriptor::scalar("Slug", SchemaKind::String)
            .with_constraints(vec![ValidationRule::Custom("check_slug".to_owned())]);
        let model = ModelDescriptor::new(
            "Page",
            vec![FieldDescriptor::new(
                "slug",
                true,
                validated,
                // What `#[api_model]` records on a field declared with the type.
                vec![ValidationRule::Custom("check_slug".to_owned())],
            )],
        );
        let operation = OperationDescriptor::new(
            HttpMethod::Post,
            "/pages",
            "pages.create",
            "Create a page",
            Some(TypeDescriptor::model(model)),
            vec![ResponseDescriptor::success(201, None)],
        )
        .unwrap();
        let app = App::new().route(operation).build().unwrap();

        let document = super::to_value(&app);
        assert_eq!(
            document["components"]["schemas"]["Page"]["properties"]["slug"]["x-blazingly-validators"],
            blazingly_json::json!(["check_slug"])
        );
    }

    /// An item's whole bundle projects onto the item, and only onto the item.
    #[test]
    fn an_items_bundle_stays_off_the_collection_that_holds_it() {
        let channel = TypeDescriptor::scalar("Channel", SchemaKind::String).with_constraints(vec![
            ValidationRule::MaxLength(16),
            ValidationRule::Custom("enum=news|sport".to_owned()),
            ValidationRule::Custom("pattern=^[a-z]+$".to_owned()),
        ]);
        let model = ModelDescriptor::new(
            "Subscribe",
            vec![FieldDescriptor::new(
                "channels",
                true,
                collection_of(channel),
                Vec::new(),
            )],
        );
        let operation = OperationDescriptor::new(
            HttpMethod::Post,
            "/subscriptions",
            "subscriptions.create",
            "Subscribe",
            Some(TypeDescriptor::model(model)),
            vec![ResponseDescriptor::success(201, None)],
        )
        .unwrap();
        let app = App::new().route(operation).build().unwrap();

        let document = super::to_value(&app);
        let channels = &document["components"]["schemas"]["Subscribe"]["properties"]["channels"];

        assert_eq!(channels["items"]["maxLength"], 16);
        assert_eq!(
            channels["items"]["enum"],
            blazingly_json::json!(["news", "sport"]),
            "a recovered enumeration reaches the item schema: {channels}"
        );
        #[cfg(feature = "validation")]
        assert_eq!(channels["items"]["pattern"], "^[a-z]+$");
        assert!(
            channels["maxLength"].is_null(),
            "an item bound must not be read as a bound on the list: {channels}"
        );
        assert!(
            channels["items"]["x-blazingly-validators"].is_null() || !cfg!(feature = "validation"),
            "a recovered item rule must not also appear as an opaque validator: {channels}"
        );
    }

    #[test]
    fn an_operation_that_decodes_input_documents_the_rejection_it_can_return() {
        let undecoded = OperationDescriptor::new(
            HttpMethod::Get,
            "/notes",
            "notes.list",
            "List notes",
            None,
            vec![ResponseDescriptor::success(200, None)],
        )
        .unwrap();
        let streaming = OperationDescriptor::new(
            HttpMethod::Post,
            "/uploads",
            "uploads.create",
            "Upload bytes",
            None,
            vec![ResponseDescriptor::success(201, None)],
        )
        .unwrap()
        .with_inputs(vec![InputDescriptor::new(
            "body",
            InputSource::Stream,
            true,
            TypeDescriptor::scalar("StreamingBody", SchemaKind::Binary),
        )]);
        let app = App::new()
            .route(note_operation())
            .route(undecoded)
            .route(streaming)
            .build()
            .unwrap();

        let document = super::to_value(&app);
        let failure = &document["paths"]["/notes"]["post"]["responses"]["422"];

        assert_eq!(
            failure["x-blazingly-automatic"], true,
            "a projected rejection is marked as one the operation did not declare"
        );
        let schema = &failure["content"]["application/json"]["schema"];
        let codes = schema["properties"]["error"]["properties"]["code"]["enum"]
            .as_array()
            .expect("a rejection carries a closed set of codes");
        assert!(codes.contains(&blazingly_json::json!("validation_error")));
        assert!(
            codes.contains(&blazingly_json::json!("invalid_json")),
            "a JSON body can fail to decode before any rule runs: {codes:?}"
        );
        let violation = &schema["properties"]["error"]["properties"]["details"]["properties"]["violations"]
            ["items"];
        assert_eq!(violation["properties"]["field"]["type"], "string");
        assert_eq!(violation["properties"]["code"]["type"], "string");
        assert_eq!(violation["properties"]["message"]["type"], "string");
        assert_eq!(
            violation["required"],
            blazingly_json::json!(["field", "code", "message"])
        );
        assert!(
            !failure["content"]["application/json"]["example"]["error"]["details"]["violations"][0]
                .is_null(),
            "the envelope carries a sample violation"
        );

        assert!(
            document["paths"]["/notes"]["get"]["responses"]["422"].is_null(),
            "an operation that decodes nothing does not claim a 422"
        );
        assert!(
            document["paths"]["/uploads"]["post"]["responses"]["422"].is_null(),
            "bytes that reach the handler untouched cannot be rejected by a rule"
        );
    }

    /// Each source is documented with the codes that source actually produces.
    ///
    /// The runtime answers a failed input differently depending on how it read
    /// it, so a code that source cannot reach must not appear in the closed set
    /// the document publishes.
    #[test]
    fn a_rejection_names_the_codes_the_inputs_it_has_can_produce() {
        let codes = |source: InputSource, ty: TypeDescriptor, path: &str, id: &str| {
            let operation = OperationDescriptor::new(
                HttpMethod::Post,
                path,
                id,
                "Accept input",
                None,
                vec![ResponseDescriptor::success(201, None)],
            )
            .unwrap()
            .with_inputs(vec![InputDescriptor::new("body", source, true, ty)]);
            let app = App::new().route(operation).build().unwrap();
            super::to_value(&app)["paths"][path]["post"]["responses"]["422"]["content"]
                ["application/json"]["schema"]["properties"]["error"]["properties"]["code"]["enum"]
                .as_array()
                .map(|codes| {
                    codes
                        .iter()
                        .filter_map(|code| code.as_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                })
        };
        let model = || TypeDescriptor::model(create_user_model());
        let scalar = || TypeDescriptor::scalar("String", SchemaKind::String);

        assert_eq!(
            codes(InputSource::Json, model(), "/users", "users.create"),
            Some(vec![
                "validation_error".to_owned(),
                "invalid_json".to_owned(),
            ]),
            "a body decoded whole reports a missing body as invalid JSON"
        );
        assert_eq!(
            codes(InputSource::Multipart, model(), "/parts", "parts.create"),
            Some(vec![
                "validation_error".to_owned(),
                "invalid_input".to_owned(),
                "invalid_multipart".to_owned(),
            ]),
            "a model is assembled from the fields that arrived, never found missing"
        );
        assert_eq!(
            codes(InputSource::File, model(), "/files", "files.create"),
            Some(vec![
                "invalid_multipart".to_owned(),
                "invalid_file_count".to_owned(),
            ]),
            "an upload is read out of a multipart document, then counted"
        );
        assert_eq!(
            codes(InputSource::Query, scalar(), "/search", "search.run"),
            Some(vec![
                "validation_error".to_owned(),
                "missing_input".to_owned(),
                "invalid_input".to_owned(),
            ]),
            "a value read one key at a time is the one that can be found missing"
        );
    }

    #[test]
    fn a_declared_422_is_not_replaced_by_the_derived_one() {
        let operation = OperationDescriptor::new(
            HttpMethod::Post,
            "/notes",
            "notes.create",
            "Create a note",
            Some(TypeDescriptor::model(create_note_model())),
            vec![
                ResponseDescriptor::success(201, None),
                ResponseDescriptor::error(
                    422,
                    "unprocessable_note",
                    "The note is not usable.",
                    None,
                ),
            ],
        )
        .unwrap();
        let app = App::new().route(operation).build().unwrap();

        let document = super::to_value(&app);
        let declared = &document["paths"]["/notes"]["post"]["responses"]["422"];
        assert_eq!(declared["x-blazingly-error-code"], "unprocessable_note");
        assert!(
            declared["x-blazingly-automatic"].is_null(),
            "a declared response keeps its own description and schema"
        );
    }

    #[test]
    fn a_body_without_a_documented_shape_carries_no_example() {
        let operation = OperationDescriptor::new(
            HttpMethod::Post,
            "/users",
            "users.create",
            "Create a user",
            Some(TypeDescriptor::new("CreateUser")),
            vec![ResponseDescriptor::success(201, None)],
        )
        .unwrap();
        let app = App::new().route(operation).build().unwrap();

        let document = super::to_value(&app);

        assert!(
            document["paths"]["/users"]["post"]["requestBody"]["content"]["application/json"]
                ["example"]
                .is_null(),
            "an unconstrained schema must not invent a payload"
        );
    }
}
