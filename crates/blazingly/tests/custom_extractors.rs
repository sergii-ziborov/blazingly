use blazingly::prelude::*;
use blazingly::{FromInvocation, InputRejection, InvocationInput};
use futures_lite::future;

#[api_model]
#[derive(Clone, Debug)]
struct VersionView {
    version: String,
}

#[api_model]
#[derive(Clone, Debug)]
struct PartsView {
    method: String,
    path: String,
    scheme: String,
    host: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ClientVersion(String);

impl FromInvocation for ClientVersion {
    fn from_invocation(
        input: &InvocationInput<'_>,
        _name: &str,
        required: bool,
    ) -> Result<Self, InputRejection> {
        Header::<String>::from_invocation(input, "x-client-version", required)
            .map(|Header(version)| Self(version))
    }
}

#[get(
    "/version",
    id = "custom.version",
    summary = "Read an application-defined extractor"
)]
fn version(Extract(version): Extract<ClientVersion>) -> Json<VersionView> {
    Json(VersionView { version: version.0 })
}

#[get("/parts", id = "custom.parts", summary = "Read the raw request parts")]
fn parts(Extract(parts): Extract<RequestParts>) -> Json<PartsView> {
    Json(PartsView {
        method: parts.method.as_str().to_owned(),
        path: parts.path,
        scheme: parts.scheme.unwrap_or_default(),
        host: parts.host,
    })
}

fn application() -> ExecutableApp {
    ExecutableApp::new(routes![version, parts]).expect("custom extractor operation compiles")
}

#[test]
fn an_explicit_custom_extractor_is_not_misclassified_as_a_dependency() {
    let executable = application();
    let definition = executable.definition();
    let operation = definition
        .operations()
        .iter()
        .find(|operation| operation.contract.id.as_str() == "custom.version")
        .expect("operation is registered");

    assert!(
        operation.contract.dependencies.is_empty(),
        "Extract<T> must not become a DI request"
    );
    assert!(
        operation.contract.inputs.is_empty(),
        "an opaque custom extractor does not invent an OpenAPI schema"
    );
}

#[test]
fn a_downstream_extractor_runs_through_the_http_handler_surface() {
    let executable = application();
    let response = future::block_on(
        TestApp::new(&executable)
            .call(Request::get("/version").header("x-client-version", "2026-08")),
    );

    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .json::<blazingly::json::Value>()
            .expect("custom extractor response is JSON")["version"],
        "2026-08"
    );
}

#[test]
fn a_custom_extractor_rejection_stays_stable() {
    let executable = application();
    let response = future::block_on(TestApp::new(&executable).call(Request::get("/version")));

    assert_eq!(response.status(), 422);
    assert_eq!(
        response
            .json::<blazingly::json::Value>()
            .expect("custom extractor rejection is JSON")["error"]["code"],
        "missing_input"
    );
}

#[test]
fn request_parts_snapshot_the_line_the_adapter_received() {
    let executable = application();
    let response = future::block_on(
        TestApp::new(&executable)
            .call(Request::get("/parts?tail=ignored").header("host", "api.example")),
    );

    assert_eq!(response.status(), 200);
    let body = response
        .json::<blazingly::json::Value>()
        .expect("parts response is JSON");
    assert_eq!(body["method"], "GET");
    assert_eq!(body["path"], "/parts", "the query string is not the path");
    assert_eq!(body["scheme"], "http");
    assert_eq!(body["host"], "api.example");

    let operation = executable
        .definition()
        .operations()
        .iter()
        .find(|operation| operation.contract.id.as_str() == "custom.parts")
        .expect("operation is registered")
        .clone();
    assert!(
        operation.contract.inputs.is_empty(),
        "a raw-parts snapshot is not a documented input"
    );
}

/// The parts are HTTP's; a tool call carries no request line to snapshot.
#[test]
fn request_parts_reject_a_transport_without_a_request_line() {
    let arguments = blazingly::json::json!({ "anything": true });
    let rejection =
        RequestParts::from_invocation(&InvocationInput::Arguments(&arguments), "parts", true)
            .expect_err("an MCP-style invocation carries no request parts");

    let blazingly::ExecutionOutcome::Rejected { status, code, .. } =
        rejection.into_execution_outcome()
    else {
        panic!("a rejection renders as a rejected outcome");
    };
    assert_eq!(status, 400);
    assert_eq!(code, "transport_mismatch");
}
