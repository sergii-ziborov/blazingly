use blazingly::prelude::*;
use blazingly::{FromInvocation, InputRejection, InvocationInput};
use futures_lite::future;

#[api_model]
#[derive(Clone, Debug)]
struct VersionView {
    version: String,
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

fn application() -> ExecutableApp {
    ExecutableApp::new(routes![version]).expect("custom extractor operation compiles")
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
