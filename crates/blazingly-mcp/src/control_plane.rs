use crate::{McpRegistry, McpResource, RegistryError, ResourceDescriptor};
use blazingly_core::{
    AppDefinition, Confirmation, InputDescriptor, OperationDescriptor, OperationRisk,
    OutputExposure, ResponseDescriptor, SecurityRequirement, TypeDescriptor,
};
use blazingly_json::{Value, json};

/// Stable URI of the read-only framework control-plane manifest.
pub const FRAMEWORK_MANIFEST_URI: &str = "blazingly://framework/manifest";
/// Versioned schema identity embedded in the framework manifest.
pub const FRAMEWORK_MANIFEST_SCHEMA: &str = "blazingly.control-plane.v1";
/// Media type returned by the framework manifest resource.
pub const FRAMEWORK_MANIFEST_MIME_TYPE: &str = "application/json";

/// Builds and registers a read-only MCP snapshot of an application definition.
///
/// The manifest deliberately contains only static operation metadata: stable
/// identities, bindings, fingerprints, agent policy, type names, dependencies,
/// security requirements, and response shapes. It never reads environment or
/// runtime configuration and never includes security-scheme configuration,
/// response-header values, tool descriptions, or runtime state.
#[derive(Clone, Copy, Debug)]
pub struct FrameworkManifest<'app> {
    app: &'app AppDefinition,
}

impl<'app> FrameworkManifest<'app> {
    /// Creates a manifest builder over one immutable application definition.
    #[must_use]
    pub const fn new(app: &'app AppDefinition) -> Self {
        Self { app }
    }

    /// Returns the deterministic manifest document before MCP framing.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut operations = self.app.operations().iter().collect::<Vec<_>>();
        operations.sort_by(|left, right| left.contract.id.cmp(&right.contract.id));
        let operations = operations
            .into_iter()
            .map(operation_value)
            .collect::<Vec<_>>();

        json!({
            "schema": FRAMEWORK_MANIFEST_SCHEMA,
            "operations": operations
        })
    }

    /// Creates the immutable JSON resource registered with an MCP server.
    #[must_use]
    pub fn resource(&self) -> McpResource {
        let text = self.to_value().to_string();
        let size = u64::try_from(text.len()).unwrap_or(u64::MAX);
        McpResource::text(
            ResourceDescriptor::new(FRAMEWORK_MANIFEST_URI, "Blazingly framework manifest")
                .with_description(
                    "Read-only operation contracts and exposure policy; no runtime configuration",
                )
                .with_mime_type(FRAMEWORK_MANIFEST_MIME_TYPE)
                .with_size(size),
            text,
        )
    }

    /// Registers the stable manifest resource in an MCP registry.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::DuplicateResource`] when the stable URI is
    /// already registered.
    pub fn register(&self, registry: &mut McpRegistry) -> Result<(), RegistryError> {
        registry.register_resource(self.resource())
    }
}

fn operation_value(operation: &OperationDescriptor) -> Value {
    let contract = &operation.contract;
    let mut inputs = contract.inputs.iter().collect::<Vec<_>>();
    inputs.sort_by(|left, right| {
        crate::input_source_name(left.source)
            .cmp(crate::input_source_name(right.source))
            .then(left.name.cmp(&right.name))
    });
    let inputs = inputs.into_iter().map(input_value).collect::<Vec<_>>();

    let mut dependencies = contract
        .dependencies
        .iter()
        .map(|dependency| dependency.rust_name.as_str())
        .collect::<Vec<_>>();
    dependencies.sort_unstable();
    dependencies.dedup();

    let mut security = contract.security.iter().collect::<Vec<_>>();
    security.sort_by(|left, right| left.scheme.cmp(&right.scheme));
    let security = security.into_iter().map(security_value).collect::<Vec<_>>();

    let mut responses = contract.responses.iter().collect::<Vec<_>>();
    responses.sort_by(|left, right| {
        left.status
            .cmp(&right.status)
            .then(left.error_code.cmp(&right.error_code))
            .then_with(|| {
                left.body
                    .as_ref()
                    .map(|body| body.rust_name.as_str())
                    .cmp(&right.body.as_ref().map(|body| body.rust_name.as_str()))
            })
    });
    let responses = responses
        .into_iter()
        .map(response_value)
        .collect::<Vec<_>>();

    let (exposed, tool_name, output_exposure) =
        contract.mcp.as_ref().map_or((false, None, None), |tool| {
            (
                true,
                Some(tool.name.as_str()),
                Some(output_exposure_name(tool.expose_output)),
            )
        });

    json!({
        "id": contract.id.as_str(),
        "http": {
            "method": operation.http.method.as_str(),
            "path": operation.http.path
        },
        "contractFingerprint": contract.fingerprint().to_string(),
        "mcp": {
            "exposed": exposed,
            "toolName": tool_name,
            "outputExposure": output_exposure,
            "policy": {
                "risk": risk_name(contract.agent.risk),
                "confirmation": confirmation_name(contract.agent.confirmation),
                "idempotent": contract.agent.idempotent
            }
        },
        "inputs": inputs,
        "dependencies": dependencies,
        "security": security,
        "responses": responses
    })
}

fn input_value(input: &InputDescriptor) -> Value {
    json!({
        "name": input.name,
        "source": crate::input_source_name(input.source),
        "required": input.required,
        "type": type_value(&input.ty)
    })
}

fn type_value(ty: &TypeDescriptor) -> Value {
    json!({
        "rustName": ty.rust_name,
        "schema": ty.schema,
        "model": ty.model.as_ref().map(|model| model.name.as_str()),
        "items": ty.items.as_deref().map(type_value)
    })
}

fn security_value(requirement: &SecurityRequirement) -> Value {
    let mut scopes = requirement
        .scopes
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    scopes.sort_unstable();
    scopes.dedup();
    json!({
        "scheme": requirement.scheme,
        "scopes": scopes
    })
}

fn response_value(response: &ResponseDescriptor) -> Value {
    let mut headers = response
        .headers
        .iter()
        .map(|header| header.name.as_str())
        .collect::<Vec<_>>();
    headers.sort_unstable();
    headers.dedup();
    json!({
        "status": response.status,
        "body": response.body.as_ref().map(type_value),
        "errorCode": response.error_code,
        "headers": headers
    })
}

const fn risk_name(risk: OperationRisk) -> &'static str {
    match risk {
        OperationRisk::Read => "read",
        OperationRisk::Write => "write",
        OperationRisk::Destructive => "destructive",
    }
}

const fn confirmation_name(confirmation: Confirmation) -> &'static str {
    match confirmation {
        Confirmation::Never => "never",
        Confirmation::Required => "required",
    }
}

const fn output_exposure_name(exposure: OutputExposure) -> &'static str {
    match exposure {
        OutputExposure::Full => "full",
        OutputExposure::SummaryOnly => "summary_only",
        OutputExposure::None => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FRAMEWORK_MANIFEST_MIME_TYPE, FRAMEWORK_MANIFEST_SCHEMA, FRAMEWORK_MANIFEST_URI,
        FrameworkManifest,
    };
    use crate::{JsonRpcServer, McpRegistry, PROTOCOL_VERSION, RegistryError};
    use blazingly_core::{
        AgentPolicy, Confirmation, DependencyDescriptor, HttpMethod, InputDescriptor, InputSource,
        McpToolDescriptor, NoContent, OperationDescriptor, OperationRisk, OutputExposure,
        ResponseDescriptor, ResponseHeader, SchemaKind, SecurityLocation, SecurityRequirement,
        SecuritySchemeDescriptor, SecuritySchemeKind, TypeDescriptor,
    };
    use blazingly_executor::{ExecutableApp, ExecutableOperation};
    use blazingly_json::{Value, json};
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    #[test]
    fn manifest_is_listed_and_read_through_the_json_rpc_resource_lifecycle() {
        let app = executable();
        let manifest = FrameworkManifest::new(app.definition());
        let serialized = manifest.to_value().to_string();
        assert_eq!(serialized, manifest.to_value().to_string());

        let mut registry = McpRegistry::new();
        manifest
            .register(&mut registry)
            .expect("manifest should register");
        let duplicate = manifest
            .register(&mut registry)
            .expect_err("the stable URI cannot be registered twice");
        assert_eq!(
            duplicate,
            RegistryError::DuplicateResource {
                uri: FRAMEWORK_MANIFEST_URI.to_owned()
            }
        );

        let mut server = JsonRpcServer::new(&app).with_registry(registry);
        initialize(&mut server);
        let listed = request(
            &mut server,
            json!({"jsonrpc":"2.0","id":2,"method":"resources/list"}),
        );
        let descriptor = &listed["result"]["resources"][0];
        assert_eq!(descriptor["uri"], FRAMEWORK_MANIFEST_URI);
        assert_eq!(descriptor["mimeType"], FRAMEWORK_MANIFEST_MIME_TYPE);
        assert_eq!(
            descriptor["size"].as_u64(),
            u64::try_from(serialized.len()).ok()
        );

        let read = request(
            &mut server,
            json!({
                "jsonrpc":"2.0",
                "id":3,
                "method":"resources/read",
                "params":{"uri":FRAMEWORK_MANIFEST_URI}
            }),
        );
        let content = &read["result"]["contents"][0];
        assert_eq!(content["uri"], FRAMEWORK_MANIFEST_URI);
        assert_eq!(content["mimeType"], FRAMEWORK_MANIFEST_MIME_TYPE);
        let text = content["text"].as_str().expect("manifest text");
        assert_eq!(text, serialized);

        let document: Value = blazingly_json::from_str(text).expect("manifest JSON");
        assert_eq!(document["schema"], FRAMEWORK_MANIFEST_SCHEMA);
        assert_eq!(document["operations"][0]["id"], "admin.rotate");
        assert_eq!(document["operations"][1]["id"], "z.health");
        let admin = &document["operations"][0];
        assert_eq!(admin["http"]["method"], "POST");
        assert_eq!(admin["http"]["path"], "/z-admin");
        assert!(
            admin["contractFingerprint"]
                .as_str()
                .is_some_and(|value| value.starts_with("blazingly-contract-v"))
        );
        assert_eq!(admin["mcp"]["exposed"], true);
        assert_eq!(admin["mcp"]["toolName"], "rotate_admin");
        assert_eq!(admin["mcp"]["outputExposure"], "summary_only");
        assert_eq!(admin["mcp"]["policy"]["risk"], "write");
        assert_eq!(admin["mcp"]["policy"]["confirmation"], "required");
        assert_eq!(admin["dependencies"], json!(["Clock", "Repository"]));
        assert_eq!(admin["security"][0]["scheme"], "operator");
        assert_eq!(admin["security"][0]["scopes"], json!(["read", "write"]));
        assert_eq!(admin["responses"][0]["status"], 201);
        assert_eq!(admin["responses"][1]["errorCode"], "already_rotated");
        assert_eq!(admin["responses"][0]["headers"], json!(["location"]));
        assert!(!text.contains("must-not-appear"));
    }

    fn executable() -> ExecutableApp {
        let responses = vec![
            ResponseDescriptor::error(
                409,
                "already_rotated",
                "must-not-appear",
                Some(TypeDescriptor::scalar("Problem", SchemaKind::Object)),
            ),
            ResponseDescriptor::success(
                201,
                Some(TypeDescriptor::scalar("AdminView", SchemaKind::Object)),
            )
            .with_headers(vec![ResponseHeader::new("location", "must-not-appear")]),
        ];
        let admin = OperationDescriptor::new(
            HttpMethod::Post,
            "/z-admin",
            "admin.rotate",
            "must-not-appear",
            None,
            responses,
        )
        .expect("admin descriptor")
        .with_inputs(vec![
            InputDescriptor::new(
                "body",
                InputSource::Json,
                true,
                TypeDescriptor::scalar("RotateAdmin", SchemaKind::Object),
            ),
            InputDescriptor::new(
                "authorization",
                InputSource::Header,
                true,
                TypeDescriptor::scalar("String", SchemaKind::String),
            ),
        ])
        .with_dependencies(vec![
            DependencyDescriptor::new("Repository"),
            DependencyDescriptor::new("Clock"),
        ])
        .with_security(vec![
            SecurityRequirement::new("operator")
                .with_scopes(vec!["write".to_owned(), "read".to_owned()]),
        ])
        .with_mcp_tool(
            McpToolDescriptor::new("rotate_admin", "must-not-appear")
                .with_output_exposure(OutputExposure::SummaryOnly),
            AgentPolicy {
                risk: OperationRisk::Write,
                confirmation: Confirmation::Required,
                idempotent: false,
            },
        );
        let health = OperationDescriptor::new(
            HttpMethod::Get,
            "/a-health",
            "z.health",
            "Health",
            None,
            vec![ResponseDescriptor::success(204, None)],
        )
        .expect("health descriptor");
        let operations = [
            ExecutableOperation::empty(admin, || async { NoContent }),
            ExecutableOperation::empty(health, || async { NoContent }),
        ];
        // The requirement asks for scopes, and only OAuth2 declares any; the
        // key-carrying scheme stays registered so the manifest's promise not
        // to leak scheme configuration is still exercised.
        let schemes = [
            SecuritySchemeDescriptor::new(
                "operator",
                SecuritySchemeKind::OAuth2 {
                    authorization_url: None,
                    token_url: Some("https://auth.example/token".to_owned()),
                    scopes: vec!["write".to_owned(), "read".to_owned()],
                },
            ),
            SecuritySchemeDescriptor::new(
                "internal",
                SecuritySchemeKind::ApiKey {
                    location: SecurityLocation::Header,
                    name: "must-not-appear".to_owned(),
                },
            ),
        ];
        ExecutableApp::with_security_schemes(operations, schemes).expect("executable app")
    }

    fn initialize(server: &mut JsonRpcServer<'_>) {
        let response = request(
            server,
            json!({
                "jsonrpc":"2.0",
                "id":1,
                "method":"initialize",
                "params":{
                    "protocolVersion":PROTOCOL_VERSION,
                    "capabilities":{},
                    "clientInfo":{"name":"test","version":"1"}
                }
            }),
        );
        assert_eq!(response["result"]["protocolVersion"], PROTOCOL_VERSION);
        let notification = poll_ready(server.handle_value(json!({
            "jsonrpc":"2.0",
            "method":"notifications/initialized"
        })));
        assert!(notification.is_none());
    }

    fn request(server: &mut JsonRpcServer<'_>, message: Value) -> Value {
        poll_ready(server.handle_value(message)).expect("JSON-RPC response")
    }

    fn poll_ready<F: Future>(future: F) -> F::Output {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = pin!(future);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("the in-memory MCP test unexpectedly yielded"),
        }
    }
}
