#![forbid(unsafe_code)]

use blazingly_core::{
    AppDefinition, InputDescriptor, InputSource, ModelDescriptor, OperationDescriptor, SchemaKind,
    SecurityLocation, SecuritySchemeDescriptor, SecuritySchemeKind, TypeDescriptor, ValidationRule,
};
use blazingly_json::{Map, Value, json};
use std::collections::BTreeMap;

/// Browser UI rendered by [`OpenApiService`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenApiUi {
    Scalar,
    Swagger,
    Disabled,
}

/// `OpenAPI` document metadata and well-known HTTP paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenApiConfig {
    pub title: String,
    pub version: String,
    pub document_path: String,
    pub ui_path: String,
    pub ui: OpenApiUi,
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
        }
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
    let mut paths = Map::new();
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

        let path = paths
            .entry(operation.http.path.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        let Value::Object(path_item) = path else {
            unreachable!("path entries are always OpenAPI path objects");
        };
        path_item.insert(
            operation.http.method.as_openapi_key().to_owned(),
            operation_value(operation),
        );
    }

    let mut document = json!({
        "openapi": "3.1.0",
        "jsonSchemaDialect": "https://json-schema.org/draft/2020-12/schema",
        "info": {
            "title": config.title,
            "version": config.version
        },
        "paths": paths
    });
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
    document
}

#[allow(clippy::too_many_lines)]
fn operation_value(operation: &OperationDescriptor) -> Value {
    let responses = operation
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
                        "schema": error_schema(response)
                    }
                });
            } else if let Some(body) = &response.body {
                value["content"] = json!({
                    (response_media_type(body)): {
                        "schema": schema_value(body)
                    }
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

    let mut value = json!({
        "operationId": operation.contract.id.as_str(),
        "summary": operation.contract.summary,
        "responses": responses,
        "x-blazingly-agent": operation.contract.agent
    });
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
        .flat_map(parameter_values)
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
                (request_media_type(input.source)): {
                    "schema": schema_value(&input.ty)
                }
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

fn parameter_values(input: &InputDescriptor) -> Vec<Value> {
    let location = input_source_name(input.source);
    if let Some(model) = &input.ty.model {
        return model
            .fields
            .iter()
            .map(|field| {
                let mut schema = schema_value(&field.ty);
                apply_validation(&mut schema, &field.validation);
                json!({
                    "name": parameter_name(input.source, &field.name),
                    "in": location,
                    "required": input.source == InputSource::Path
                        || (input.required && field.required),
                    "schema": schema
                })
            })
            .collect();
    }

    vec![json!({
        "name": parameter_name(input.source, &input.name),
        "in": location,
        "required": input.source == InputSource::Path || input.required,
        "schema": schema_value(&input.ty)
    })]
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

fn schema_value(descriptor: &TypeDescriptor) -> Value {
    if let Some(model) = &descriptor.model {
        return json!({
            "$ref": format!("#/components/schemas/{}", model.name),
            "x-rust-type": descriptor.rust_name
        });
    }

    let mut value = match (&descriptor.schema, &descriptor.items) {
        (SchemaKind::Array(_), Some(items)) => {
            json!({ "type": "array", "items": schema_value(items) })
        }
        _ => schema_kind_value(&descriptor.schema),
    };
    apply_known_string_format(&mut value, &descriptor.rust_name);
    value["x-rust-type"] = Value::String(descriptor.rust_name.clone());
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
        SchemaKind::Binary => json!({ "type": "string", "format": "binary" }),
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
            ValidationRule::Alias(alias) => {
                let aliases = schema
                    .as_object_mut()
                    .expect("validation schema must be an object")
                    .entry("x-blazingly-aliases")
                    .or_insert_with(|| Value::Array(Vec::new()));
                aliases
                    .as_array_mut()
                    .expect("alias extension must be an array")
                    .push(Value::String(alias.clone()));
            }
            ValidationRule::Custom(validator) => {
                // Declarative constraints are encoded as `keyword=value` inside
                // `Custom`; project the ones that map to a JSON Schema keyword
                // instead of leaving them as opaque validator strings.
                #[cfg(feature = "validation")]
                if let Some(constraint) = blazingly_validation::Constraint::parse(validator) {
                    constraint.apply_json_schema(schema);
                    continue;
                }
                let validators = schema
                    .as_object_mut()
                    .expect("validation schema must be an object")
                    .entry("x-blazingly-validators")
                    .or_insert_with(|| Value::Array(Vec::new()));
                validators
                    .as_array_mut()
                    .expect("validator extension must be an array")
                    .push(Value::String(validator.clone()));
            }
            ValidationRule::Nested => {
                schema["x-blazingly-nested-validation"] = Value::Bool(true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use blazingly_core::{
        App, HttpMethod, OperationDescriptor, ResponseDescriptor, SecurityRequirement,
        SecuritySchemeDescriptor, SecuritySchemeKind, TypeDescriptor,
    };

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
}
