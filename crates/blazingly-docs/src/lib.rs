#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

use blazingly_core::{
    AppDefinition, FieldDescriptor, FieldMetadata, InputDescriptor, InputSource, ModelDescriptor,
    OperationDescriptor, SchemaKind, TypeDescriptor, ValidationRule,
};
use blazingly_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fmt::Write;

pub use blazingly_deploy::KubernetesConfig;

/// Configuration for a generated human/agent documentation bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocsBundleConfig {
    pub title: String,
    pub base_url: String,
    pub mcp_endpoint: String,
}

impl DocsBundleConfig {
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            base_url: "http://127.0.0.1:3000".to_owned(),
            mcp_endpoint: "http://127.0.0.1:3000/mcp".to_owned(),
        }
    }

    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    #[must_use]
    pub fn with_mcp_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.mcp_endpoint = endpoint.into();
        self
    }
}

/// Deterministic generated files for humans, agents, and client authors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocsBundle {
    files: BTreeMap<String, String>,
}

impl DocsBundle {
    #[must_use]
    pub fn file(&self, path: &str) -> Option<&str> {
        self.files.get(path).map(String::as_str)
    }

    pub fn files(&self) -> impl Iterator<Item = (&str, &str)> {
        self.files
            .iter()
            .map(|(path, contents)| (path.as_str(), contents.as_str()))
    }

    #[must_use]
    pub fn into_files(self) -> BTreeMap<String, String> {
        self.files
    }
}

/// A minimal compilable native project scaffold.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScaffoldConfig {
    pub package_name: String,
    pub blazingly_dependency: String,
    pub listen_address: String,
    pub kubernetes: Option<KubernetesConfig>,
}

impl ScaffoldConfig {
    #[must_use]
    pub fn new(package_name: impl Into<String>) -> Self {
        let package_name = package_name.into();
        Self {
            kubernetes: Some(KubernetesConfig::new(&package_name)),
            package_name,
            blazingly_dependency: format!(
                "{{ version = \"{}\", features = [\"native\"] }}",
                env!("CARGO_PKG_VERSION")
            ),
            listen_address: "127.0.0.1:3000".to_owned(),
        }
    }

    /// Uses an exact Cargo dependency expression, such as
    /// `{ path = "../blazingly", features = ["native"] }`.
    #[must_use]
    pub fn with_dependency(mut self, dependency: impl Into<String>) -> Self {
        self.blazingly_dependency = dependency.into();
        self
    }

    #[must_use]
    pub fn with_listen_address(mut self, address: impl Into<String>) -> Self {
        self.listen_address = address.into();
        self
    }

    /// Replaces the generated container/Kubernetes settings.
    #[must_use]
    pub fn with_kubernetes(mut self, kubernetes: KubernetesConfig) -> Self {
        self.kubernetes = Some(kubernetes);
        self
    }

    /// Generates only the local Rust project.
    #[must_use]
    pub fn without_kubernetes(mut self) -> Self {
        self.kubernetes = None;
        self
    }
}

/// Builds a deterministic API/AI/client bundle from one operation graph.
///
/// # Errors
///
/// Returns a serialization error only if the canonical contract manifest
/// cannot be encoded.
pub fn bundle(
    app: &AppDefinition,
    config: &DocsBundleConfig,
) -> Result<DocsBundle, blazingly_json::Error> {
    let contracts = app
        .operations()
        .iter()
        .map(|operation| &operation.contract)
        .collect::<Vec<_>>();
    let mut files = BTreeMap::new();
    files.insert("api.md".to_owned(), api_markdown(app));
    files.insert("ai.md".to_owned(), ai_markdown(app, config));
    files.insert(
        "contracts.json".to_owned(),
        blazingly_json::to_string_pretty(&contracts)?,
    );
    files.insert("examples/http.md".to_owned(), http_examples(app, config));
    files.insert("examples/mcp.md".to_owned(), mcp_examples(app, config));
    files.insert(
        "clients/rust.rs".to_owned(),
        rust_client_source(app, config),
    );
    Ok(DocsBundle { files })
}

/// Generates a Tokio-free native project and optional Kubernetes deployment.
#[must_use]
pub fn scaffold(config: &ScaffoldConfig) -> DocsBundle {
    let cargo = format!(
        concat!(
            "[package]\n",
            "name = \"{}\"\n",
            "version = \"0.1.0\"\n",
            "edition = \"2024\"\n\n",
            "[workspace]\n\n",
            "[dependencies]\n",
            "blazingly = {}\n"
        ),
        config.package_name, config.blazingly_dependency
    );
    let main = format!(
        concat!(
            "use blazingly::prelude::*;\n",
            "use std::num::NonZeroUsize;\n",
            "use std::time::Duration;\n\n",
            // The summary is not decoration in a scaffold: it is what reaches
            // OpenAPI, the `cargo blazingly routes` table, and the MCP tool
            // description, so a generated project should demonstrate it.
            "#[get(\"/health\", id = \"health.read\", summary = \"Liveness probe\")]\n",
            "fn health() -> Json<&'static str> {{\n",
            "    Json(\"ok\")\n",
            "}}\n\n",
            "fn application() -> ExecutableApp {{\n",
            "    ExecutableApp::new(routes![health])\n",
            "        .expect(\"application contract should compile\")\n",
            "}}\n\n",
            "fn main() -> std::io::Result<()> {{\n",
            "    let address = std::env::var(\"BLAZINGLY_LISTEN_ADDRESS\")\n",
            "        .unwrap_or_else(|_| \"{}\".to_owned());\n",
            "    let workers = std::env::var(\"BLAZINGLY_WORKERS\")\n",
            "        .ok()\n",
            "        .and_then(|value| value.parse::<NonZeroUsize>().ok())\n",
            "        .or_else(|| std::thread::available_parallelism().ok())\n",
            "        .unwrap_or(NonZeroUsize::MIN);\n",
            "    let max_requests_per_connection =\n",
            "        std::env::var(\"BLAZINGLY_MAX_REQUESTS_PER_CONNECTION\")\n",
            "            .ok()\n",
            "            .and_then(|value| value.parse::<NonZeroUsize>().ok());\n",
            "    let limits = blazingly::native::ServerLimits::new()\n",
            "        .with_max_requests_per_connection(max_requests_per_connection);\n",
            "    let (_shutdown, signal) = blazingly::native::termination_channel()?;\n",
            "    blazingly::native::MulticoreServer::new(workers, application)\n",
            "        .with_limits(limits)\n",
            "        .with_openapi(blazingly::openapi::OpenApiConfig::default())\n",
            "        .serve_gracefully(address, signal, Duration::from_secs(25))\n",
            "}}\n"
        ),
        config.listen_address
    );
    let mut files = BTreeMap::from([
        ("Cargo.toml".to_owned(), cargo),
        ("src/main.rs".to_owned(), main),
    ]);
    if let Some(kubernetes) = &config.kubernetes {
        files.extend(blazingly_deploy::scaffold_files(
            &config.package_name,
            kubernetes,
        ));
    }
    DocsBundle { files }
}

/// Generates concise Markdown describing every public API operation.
///
/// Operations are grouped by the namespace of their identity, so a large
/// surface reads as sections rather than as one flat list.
#[must_use]
pub fn api_markdown(app: &AppDefinition) -> String {
    let mut output = String::from("# API\n\n");

    let mut sections: BTreeMap<&str, Vec<&OperationDescriptor>> = BTreeMap::new();
    let mut ungrouped = Vec::new();
    for operation in app.operations() {
        match operation_tag(operation) {
            Some(tag) => sections.entry(tag).or_default().push(operation),
            None => ungrouped.push(operation),
        }
    }

    let grouped = !sections.is_empty();
    let heading = if grouped { "###" } else { "##" };
    for (tag, operations) in sections {
        let _ = writeln!(output, "## {tag}\n");
        for operation in operations {
            write_operation(&mut output, operation, heading);
        }
    }
    if grouped && !ungrouped.is_empty() {
        output.push_str("## Other\n\n");
    }
    for operation in ungrouped {
        write_operation(&mut output, operation, heading);
    }

    output
}

/// The section an operation belongs to.
///
/// The first tag it declares, when it declares any — a page has one section per
/// operation, so a multi-tag operation files under the first. Otherwise the
/// namespace of its identity: `users.create` and `users.list` both belong to
/// `users`, and an identity without a namespace belongs to no section.
fn operation_tag(operation: &OperationDescriptor) -> Option<&str> {
    if let Some(tag) = operation.documentation.tags.first() {
        return Some(tag.as_str());
    }
    operation
        .contract
        .id
        .as_str()
        .rsplit_once('.')
        .map(|(namespace, _)| namespace)
        .filter(|namespace| !namespace.is_empty())
}

/// The long-form description an operation declares beyond its summary.
///
/// An explicitly declared description wins over the one an MCP tool carries,
/// which itself defaults to the summary and is therefore only worth printing
/// when it says something the summary does not.
fn operation_description(operation: &OperationDescriptor) -> Option<&str> {
    if let Some(declared) = &operation.documentation.description
        && !declared.is_empty()
    {
        return Some(declared.as_str());
    }
    let description = operation.contract.mcp.as_ref()?.description.as_str();
    (!description.is_empty() && description != operation.contract.summary).then_some(description)
}

/// Generates agent-oriented Markdown for native MCP tools.
#[must_use]
pub fn mcp_markdown(app: &AppDefinition) -> String {
    let mut output = String::from("# MCP tools\n\n");

    for operation in app.operations() {
        let Some(tool) = &operation.contract.mcp else {
            continue;
        };

        let _ = writeln!(output, "## `{}`\n", tool.name);
        let _ = writeln!(output, "{}\n", tool.description);
        let _ = writeln!(output, "- Operation: `{}`", operation.contract.id.as_str());
        let _ = writeln!(output, "- Risk: `{:?}`", operation.contract.agent.risk);
        let _ = writeln!(
            output,
            "- Confirmation: `{:?}`",
            operation.contract.agent.confirmation
        );
        let _ = writeln!(
            output,
            "- Idempotent: `{}`",
            operation.contract.agent.idempotent
        );
        let _ = writeln!(output, "- Output exposure: `{:?}`\n", tool.expose_output);
        for input in &operation.contract.inputs {
            if let Some(model) = input.ty.model.as_deref() {
                write_model(&mut output, model);
            }
        }
        for response in &operation.contract.responses {
            if let Some(code) = &response.error_code {
                let message = response.error_message.as_deref().unwrap_or("");
                let _ = writeln!(
                    output,
                    "- Error `{}` (`{code}`): {message}",
                    response.status
                );
            }
        }
        output.push('\n');
    }

    output
}

/// Generates an agent-first contract document including policy, inputs,
/// outputs, failures, and concrete MCP arguments.
#[must_use]
pub fn ai_markdown(app: &AppDefinition, config: &DocsBundleConfig) -> String {
    let mut output = format!("# {} agent contract\n\n", config.title);
    output.push_str(concat!(
        "Use only the operations listed here. Treat typed validation and domain errors as ",
        "correctable tool feedback. Never retry a destructive or non-idempotent operation ",
        "without user intent. Operations marked as requiring confirmation must receive ",
        "explicit confirmation before invocation.\n\n"
    ));
    for operation in app.operations() {
        let _ = writeln!(
            output,
            "## `{}`\n\n{}\n",
            operation.contract.id.as_str(),
            operation.contract.summary
        );
        if let Some(description) = operation_description(operation) {
            let _ = writeln!(output, "{description}\n");
        }
        let _ = writeln!(
            output,
            "- HTTP: `{} {}`",
            operation.http.method.as_str(),
            operation.http.path
        );
        let _ = writeln!(
            output,
            "- Agent policy: risk `{:?}`, confirmation `{:?}`, idempotent `{}`",
            operation.contract.agent.risk,
            operation.contract.agent.confirmation,
            operation.contract.agent.idempotent
        );
        if let Some(tool) = &operation.contract.mcp {
            let _ = writeln!(
                output,
                "- MCP: `{}`; output exposure `{:?}`",
                tool.name, tool.expose_output
            );
            let _ = writeln!(
                output,
                "- Example arguments: `{}`",
                example_arguments(&operation.contract.inputs)
            );
        } else {
            output.push_str("- MCP: not exposed\n");
        }
        for input in &operation.contract.inputs {
            let _ = writeln!(
                output,
                "- Input `{}` from {}: `{}` ({})",
                input.name,
                input_source_name(input.source),
                input.ty.rust_name,
                if input.required {
                    "required"
                } else {
                    "optional"
                }
            );
        }
        for response in &operation.contract.responses {
            if let Some(code) = &response.error_code {
                let _ = writeln!(
                    output,
                    "- Correctable error `{}` / HTTP {}: {}",
                    code,
                    response.status,
                    response.error_message.as_deref().unwrap_or("")
                );
            } else {
                let _ = writeln!(
                    output,
                    "- Success HTTP {}: `{}`",
                    response.status,
                    response
                        .body
                        .as_ref()
                        .map_or("empty", |body| body.rust_name.as_str())
                );
            }
        }
        output.push('\n');
    }
    output
}

fn http_examples(app: &AppDefinition, config: &DocsBundleConfig) -> String {
    let mut output = String::from("# HTTP examples\n\n");
    for operation in app.operations() {
        let mut path = operation.http.path.clone();
        let arguments = example_arguments_map(&operation.contract.inputs);
        for input in operation
            .contract
            .inputs
            .iter()
            .filter(|input| input.source == InputSource::Path)
        {
            for name in input_names(input) {
                let replacement = scalar_text(arguments.get(name).unwrap_or(&Value::Null));
                path = path.replace(&format!("{{{name}}}"), &replacement);
            }
        }
        let _ = writeln!(output, "## `{}`\n", operation.contract.id.as_str());
        let _ = write!(
            output,
            "```bash\ncurl -X {} '{}{}'",
            operation.http.method.as_str(),
            config.base_url.trim_end_matches('/'),
            path
        );
        for input in &operation.contract.inputs {
            if input.source == InputSource::Header {
                for name in input_names(input) {
                    let value = scalar_text(arguments.get(name).unwrap_or(&Value::Null));
                    let _ = write!(output, " \\\n  -H '{}: {}'", name.replace('_', "-"), value);
                }
            }
        }
        if let Some(body) = operation.contract.inputs.iter().find(|input| {
            matches!(
                input.source,
                InputSource::Json
                    | InputSource::Form
                    | InputSource::Multipart
                    | InputSource::File
                    | InputSource::Stream
            )
        }) {
            let media_type = match body.source {
                InputSource::Json => "application/json",
                InputSource::Form => "application/x-www-form-urlencoded",
                InputSource::Multipart | InputSource::File => "multipart/form-data",
                InputSource::Stream => "application/octet-stream",
                _ => unreachable!("body input was selected above"),
            };
            let _ = write!(
                output,
                " \\\n  -H 'content-type: {media_type}' \\\n  --data '{}'",
                example_value(&body.ty)
            );
        }
        output.push_str("\n```\n\n");
    }
    output
}

fn mcp_examples(app: &AppDefinition, config: &DocsBundleConfig) -> String {
    let mut output = format!(
        "# MCP examples\n\nStreamable HTTP endpoint: `{}`\n\n",
        config.mcp_endpoint
    );
    for operation in app.operations() {
        let Some(tool) = &operation.contract.mcp else {
            continue;
        };
        let mut params = json!({
            "name": tool.name,
            "arguments": example_arguments_map(&operation.contract.inputs)
        });
        if operation.contract.agent.confirmation == blazingly_core::Confirmation::Required {
            params["_meta"] = json!({ "dev.blazingly/confirmed": true });
        }
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": params
        });
        let _ = writeln!(
            output,
            "## `{}`\n\n```json\n{}\n```\n",
            tool.name,
            blazingly_json::to_string_pretty(&request)
                .unwrap_or_else(|_| "{\"error\":\"example generation failed\"}".to_owned())
        );
    }
    output
}

fn rust_client_source(app: &AppDefinition, config: &DocsBundleConfig) -> String {
    let mut output = format!(
        concat!(
            // The starter is a standalone reqwest project, not a Blazingly
            // crate. `reqwest::Response::json` deserializes through
            // `serde_json`, so the generated signatures name that crate.
            "// Generated starter for {}. Dependencies: reqwest, serde_json.\n",
            "pub struct Client {{\n",
            "    base_url: String,\n",
            "    http: reqwest::Client,\n",
            "}}\n\n",
            "impl Client {{\n",
            "    pub fn new(base_url: impl Into<String>) -> Self {{\n",
            "        Self {{ base_url: base_url.into(), http: reqwest::Client::new() }}\n",
            "    }}\n\n"
        ),
        config.base_url
    );
    for operation in app.operations() {
        let function = rust_identifier(operation.contract.id.as_str());
        let _ = writeln!(
            output,
            concat!(
                "    /// Calls `{}`. Replace path placeholders before passing `path`.\n",
                "    pub async fn {}(\n",
                "        &self,\n",
                "        path: &str,\n",
                "        body: Option<&serde_json::Value>,\n",
                "    ) -> Result<serde_json::Value, reqwest::Error> {{\n",
                "        let request = self.http.request(\n",
                "            reqwest::Method::{},\n",
                "            format!(\"{{}}{{}}\", self.base_url.trim_end_matches('/'), path),\n",
                "        );\n",
                "        let request = match body {{ Some(body) => request.json(body), None => request }};\n",
                "        request.send().await?.error_for_status()?.json().await\n",
                "    }}\n"
            ),
            operation.contract.id.as_str(),
            function,
            operation.http.method.as_str()
        );
    }
    output.push_str("}\n");
    output
}

fn rust_identifier(operation_id: &str) -> String {
    operation_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn example_arguments(inputs: &[InputDescriptor]) -> Value {
    Value::Object(example_arguments_map(inputs))
}

fn example_arguments_map(inputs: &[InputDescriptor]) -> Map<String, Value> {
    let mut arguments = Map::new();
    for input in inputs {
        if let Some(model) = &input.ty.model {
            for field in &model.fields {
                arguments.insert(field.name.clone(), field_example(field));
            }
        } else {
            arguments.insert(input.name.clone(), example_value(&input.ty));
        }
    }
    arguments
}

fn input_names(input: &InputDescriptor) -> Vec<&str> {
    input.ty.model.as_ref().map_or_else(
        || vec![input.name.as_str()],
        |model| {
            model
                .fields
                .iter()
                .map(|field| field.name.as_str())
                .collect()
        },
    )
}

/// A sample field value, preferring what the field itself declares.
fn field_example(field: &FieldDescriptor) -> Value {
    let metadata = field
        .validation
        .iter()
        .filter_map(|rule| match rule {
            ValidationRule::Custom(validator) => FieldMetadata::parse(validator),
            _ => None,
        })
        .collect::<Vec<_>>();
    for candidate in &metadata {
        if let FieldMetadata::Default(value) = candidate {
            return value.clone();
        }
    }
    for candidate in &metadata {
        if let FieldMetadata::Enumeration(values) = candidate {
            if let Some(first) = values.first() {
                return Value::String(first.clone());
            }
        }
    }
    example_value(&field.ty)
}

fn example_value(descriptor: &TypeDescriptor) -> Value {
    if let Some(model) = &descriptor.model {
        return Value::Object(
            model
                .fields
                .iter()
                .map(|field| (field.name.clone(), field_example(field)))
                .collect(),
        );
    }
    match &descriptor.schema {
        SchemaKind::String => Value::String("example".to_owned()),
        SchemaKind::Binary => Value::String("ZXhhbXBsZQ==".to_owned()),
        SchemaKind::Integer => json!(1),
        SchemaKind::Number => json!(1.0),
        SchemaKind::Boolean => Value::Bool(true),
        SchemaKind::Array(item) => Value::Array(vec![
            descriptor
                .items
                .as_deref()
                .map_or_else(|| schema_kind_example(item), example_value),
        ]),
        SchemaKind::Object => json!({}),
        SchemaKind::Any => Value::Null,
    }
}

fn schema_kind_example(schema: &SchemaKind) -> Value {
    match schema {
        SchemaKind::String | SchemaKind::Binary => Value::String("example".to_owned()),
        SchemaKind::Integer => json!(1),
        SchemaKind::Number => json!(1.0),
        SchemaKind::Boolean => Value::Bool(true),
        SchemaKind::Array(item) => Value::Array(vec![schema_kind_example(item)]),
        SchemaKind::Object => json!({}),
        SchemaKind::Any => Value::Null,
    }
}

fn scalar_text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

fn write_operation(output: &mut String, operation: &OperationDescriptor, heading: &str) {
    let _ = writeln!(
        output,
        "{heading} `{}`\n\n{}\n",
        operation.contract.id.as_str(),
        operation.contract.summary
    );
    // Above the description, not below: a reader deciding whether to use this
    // operation should learn it is on the way out before reading how it works.
    if operation.documentation.deprecated {
        let _ = writeln!(
            output,
            "**Deprecated.** Still served, no longer recommended.\n"
        );
    }
    if let Some(description) = operation_description(operation) {
        let _ = writeln!(output, "{description}\n");
    }
    let _ = writeln!(
        output,
        "- HTTP: `{} {}`",
        operation.http.method.as_str(),
        operation.http.path
    );
    if let Some(external) = &operation.documentation.external_docs {
        let label = external
            .description
            .as_deref()
            .unwrap_or("Further documentation");
        let _ = writeln!(output, "- See also: [{label}]({})", external.url);
    }

    for input in &operation.contract.inputs {
        let requirement = if input.required {
            "required"
        } else {
            "optional"
        };
        let _ = writeln!(
            output,
            "- {} input `{}`: `{}` ({requirement})",
            input_source_name(input.source),
            input.name,
            input.ty.rust_name
        );
        if let Some(model) = input.ty.model.as_deref() {
            write_model(output, model);
        }
    }
    for dependency in &operation.contract.dependencies {
        let _ = writeln!(output, "- Dependency: `{}`", dependency.rust_name);
    }
    for security in &operation.contract.security {
        if security.scopes.is_empty() {
            let _ = writeln!(output, "- Security: `{}`", security.scheme);
        } else {
            let _ = writeln!(
                output,
                "- Security: `{}` with scopes `{}`",
                security.scheme,
                security.scopes.join("`, `")
            );
        }
    }

    for response in &operation.contract.responses {
        let body = response
            .body
            .as_ref()
            .map_or("empty", |descriptor| descriptor.rust_name.as_str());
        if let Some(code) = &response.error_code {
            let message = response.error_message.as_deref().unwrap_or("");
            let _ = writeln!(
                output,
                "- Error `{}` (`{code}`): {message}",
                response.status
            );
            if let Some(details) = &response.body {
                let _ = writeln!(output, "  - Typed details: `{}`", details.rust_name);
            }
        } else {
            let _ = writeln!(output, "- Response `{}`: `{body}`", response.status);
        }
        for header in &response.headers {
            let _ = writeln!(output, "  - Header `{}`: `{}`", header.name, header.value);
        }
    }

    if let Some(tool) = &operation.contract.mcp {
        let _ = writeln!(output, "- MCP tool: `{}`", tool.name);
    }

    output.push('\n');
}

const fn input_source_name(source: InputSource) -> &'static str {
    match source {
        InputSource::Path => "Path",
        InputSource::Query => "Query",
        InputSource::Header => "Header",
        InputSource::Cookie => "Cookie",
        InputSource::Json => "JSON",
        InputSource::Form => "Form",
        InputSource::Multipart => "Multipart",
        InputSource::File => "File",
        InputSource::Stream => "Stream",
    }
}

/// The prose a `keyword=value` rule reads as.
///
/// A declarative constraint reads better as its own keyword than as an opaque
/// validator name, and the same keywords describe a field and a collection's
/// items alike, so both scopes share one reading.
fn describe_encoded_rule(encoded: &str) -> String {
    if let Some(metadata) = FieldMetadata::parse(encoded) {
        return describe_metadata(&metadata);
    }
    #[cfg(feature = "validation")]
    if let Some(constraint) = blazingly_validation::Constraint::parse(encoded) {
        return constraint.to_string();
    }
    format!("validator `{encoded}`")
}

/// Renders a recovered default, nullability marker, or enumeration as prose.
fn describe_metadata(metadata: &FieldMetadata) -> String {
    match metadata {
        FieldMetadata::Default(value) => format!("default `{value}`"),
        FieldMetadata::Nullable => "nullable".to_owned(),
        FieldMetadata::Enumeration(values) => format!("one of `{}`", values.join("`, `")),
    }
}

fn write_model(output: &mut String, model: &ModelDescriptor) {
    let _ = writeln!(output, "\n### `{}` fields\n", model.name);

    for field in &model.fields {
        let requirement = if field.required {
            "required"
        } else {
            "optional"
        };
        let _ = write!(
            output,
            "- `{}`: `{}` ({requirement})",
            field.name, field.ty.rust_name
        );
        for rule in &field.validation {
            let _ = write!(output, ", {}", describe_rule(rule));
        }
        // The rules a value type declares for a collection's elements ride on
        // the item descriptor, one level of scope per level of nesting.
        let mut scope = String::new();
        let mut items = field.ty.items.as_deref();
        while let Some(item) = items {
            scope.push_str("each item ");
            for rule in &item.constraints {
                let _ = write!(output, ", {scope}{}", describe_rule(rule));
            }
            items = item.items.as_deref();
        }
        output.push('\n');
    }
    output.push('\n');
}

/// One recorded rule as prose, whichever contract variant carries it.
fn describe_rule(rule: &ValidationRule) -> String {
    match rule {
        ValidationRule::MinLength(value) => format!("min length {value}"),
        ValidationRule::MaxLength(value) => format!("max length {value}"),
        ValidationRule::Email => "email".to_owned(),
        ValidationRule::Alias(alias) => format!("alias `{alias}`"),
        ValidationRule::Custom(validator) => describe_encoded_rule(validator),
        ValidationRule::Nested => "nested validation".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use blazingly_core::{
        AgentPolicy, App, FieldDescriptor, HttpMethod, McpToolDescriptor, ModelDescriptor,
        OperationDescriptor, ResponseDescriptor, SchemaKind, TypeDescriptor, ValidationRule,
    };

    #[test]
    fn markdown_is_generated_from_operation_metadata() {
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
        .expect("operation should be valid")
        .with_mcp_tool(
            McpToolDescriptor::new("create_user", "Create one user"),
            AgentPolicy::default(),
        );
        let app = App::new()
            .route(operation)
            .build()
            .expect("application should be valid");

        let api_document = super::api_markdown(&app);
        let mcp_document = super::mcp_markdown(&app);

        assert!(api_document.contains("POST /users"));
        assert!(api_document.contains("CreateUser"));
        assert!(mcp_document.contains("create_user"));
        assert!(mcp_document.contains("users.create"));
    }

    #[test]
    fn bundle_and_scaffold_emit_deterministic_agent_and_starter_files() {
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
        .unwrap()
        .with_mcp_tool(
            McpToolDescriptor::new("create_user", "Create one user"),
            AgentPolicy::default(),
        );
        let app = App::new().route(operation).build().unwrap();
        let bundle = super::bundle(
            &app,
            &super::DocsBundleConfig::new("Users API").with_base_url("https://api.example"),
        )
        .unwrap();

        assert!(bundle.file("api.md").unwrap().contains("POST /users"));
        assert!(bundle.file("ai.md").unwrap().contains("Agent policy"));
        assert!(
            bundle
                .file("examples/mcp.md")
                .unwrap()
                .contains("\"method\": \"tools/call\"")
        );
        assert!(
            bundle
                .file("clients/rust.rs")
                .unwrap()
                .contains("pub async fn users_create")
        );
        assert!(
            bundle
                .file("contracts.json")
                .unwrap()
                .contains("users.create")
        );

        let scaffold = super::scaffold(
            &super::ScaffoldConfig::new("hello-blazingly")
                .with_dependency("{ path = \"../blazingly\", features = [\"native\"] }"),
        );
        assert!(
            scaffold
                .file("Cargo.toml")
                .unwrap()
                .contains("blazingly = { path = \"../blazingly\", features = [\"native\"] }")
        );
        assert!(
            scaffold
                .file("src/main.rs")
                .unwrap()
                .contains("blazingly::native::MulticoreServer")
        );
        assert!(
            scaffold
                .file("src/main.rs")
                .unwrap()
                .contains("termination_channel")
        );
        assert!(
            scaffold
                .file("deploy/kubernetes/base/hpa.yaml")
                .unwrap()
                .contains("apiVersion: autoscaling/v2")
        );
        assert!(
            scaffold
                .file("deploy/kubernetes/overlays/direct/service-load-balancer.yaml")
                .unwrap()
                .contains("type: LoadBalancer")
        );
        assert!(
            scaffold
                .file("deploy/kubernetes/overlays/nginx/ingress.yaml")
                .unwrap()
                .contains("ingressClassName: nginx")
        );
    }

    #[test]
    fn api_markdown_groups_operations_and_prints_their_long_description() {
        let create = OperationDescriptor::new(
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
        let health = OperationDescriptor::new(
            HttpMethod::Get,
            "/health",
            "health",
            "Report health",
            None,
            vec![ResponseDescriptor::success(200, None)],
        )
        .unwrap();
        let app = App::new().route(create).route(health).build().unwrap();

        let document = super::api_markdown(&app);

        assert!(document.contains("## users\n"), "{document}");
        assert!(document.contains("### `users.create`"), "{document}");
        assert!(document.contains("## Other\n"), "{document}");
        assert!(
            document.contains("Registers one user and returns its view."),
            "{document}"
        );
    }

    #[test]
    fn recorded_field_metadata_reads_as_prose_and_seeds_the_examples() {
        let model = ModelDescriptor::new(
            "CreateArticle",
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

        let document = super::api_markdown(&app);
        assert!(
            document.contains("one of `draft`, `published`"),
            "{document}"
        );
        assert!(document.contains("default `\"draft\"`"), "{document}");
        assert!(document.contains(", nullable"), "{document}");
        assert!(
            !document.contains("validator `default"),
            "recovered metadata must not read as an opaque validator: {document}"
        );

        let bundle = super::bundle(&app, &super::DocsBundleConfig::new("Articles")).unwrap();
        assert!(
            bundle
                .file("examples/http.md")
                .unwrap()
                .contains("\"draft\""),
            "a declared default belongs in the sample request"
        );
    }

    /// A bound on each element reads as one, not as a bound on the list.
    #[test]
    fn an_item_bundle_reads_as_a_rule_about_each_element() {
        let title = TypeDescriptor::scalar("Title", SchemaKind::String).with_constraints(vec![
            ValidationRule::MinLength(8),
            ValidationRule::Custom("enum=news|sport".to_owned()),
        ]);
        let titles = TypeDescriptor {
            rust_name: "Vec<Title>".to_owned(),
            schema: SchemaKind::Array(Box::new(SchemaKind::String)),
            model: None,
            items: Some(Box::new(title)),
            constraints: Vec::new(),
        };
        let model = ModelDescriptor::new(
            "RenameBatch",
            vec![FieldDescriptor::new(
                "titles",
                true,
                titles,
                vec![ValidationRule::Custom("min_items=1".to_owned())],
            )],
        );
        let operation = OperationDescriptor::new(
            HttpMethod::Post,
            "/titles",
            "articles.rename",
            "Rename articles",
            Some(TypeDescriptor::model(model)),
            vec![ResponseDescriptor::success(200, None)],
        )
        .unwrap();
        let app = App::new().route(operation).build().unwrap();

        let document = super::api_markdown(&app);
        assert!(
            document.contains("each item min length 8"),
            "an item bound says which scope it bounds: {document}"
        );
        assert!(
            document.contains("each item one of `news`, `sport`"),
            "{document}"
        );
        assert!(
            document.contains("min_items=1") || document.contains("min items 1"),
            "{document}"
        );
        assert!(
            !document.contains("validator `items."),
            "a recovered item rule must not read as an opaque validator: {document}"
        );
    }
}
