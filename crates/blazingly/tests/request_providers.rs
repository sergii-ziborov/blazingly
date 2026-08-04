#![cfg(all(feature = "openapi", feature = "validation"))]

//! Request-aware providers: a `#[provider]` consumes typed request inputs,
//! the inputs fold into the operation contract exactly once, validation
//! failures answer like handler-declared inputs, and a test override
//! bypasses the replaced provider together with its inputs.

use blazingly::prelude::*;
use blazingly::{Provider, TestOverrides};
use blazingly_json::{Value, json};
use futures_lite::future;

#[api_model]
#[derive(Clone, Debug)]
struct Paging {
    #[default(20)]
    #[minimum(1)]
    #[maximum(100)]
    limit: u32,
}

#[derive(Clone, Debug)]
struct Session {
    user: String,
}

#[derive(Clone, Debug)]
struct Tenant {
    label: String,
    limit: u32,
}

/// The nested provider: one cookie, no dependencies.
#[provider]
fn session(Cookie(sid): Cookie<String>) -> Session {
    Session {
        user: format!("user-{sid}"),
    }
}

/// The outer provider: a header, a validated query model, and a dependency
/// on the nested provider.
// A provider receives its dependency by value whatever the body does with
// it, so the signature is the framework's rather than this test's to choose.
#[allow(clippy::needless_pass_by_value)]
#[provider]
fn tenant(
    Header(tenant): Header<String>,
    Query(paging): Query<Paging>,
    session: Depends<Session>,
) -> Tenant {
    Tenant {
        label: format!("{tenant}/{}", session.user),
        limit: paging.limit,
    }
}

#[api_model]
#[derive(Clone, Debug)]
struct TenantView {
    label: String,
    limit: u32,
}

/// The handler declares nothing about the wire: every input below arrives
/// through the provider chain.
#[get("/tenant", id = "tenant.read", summary = "Read the resolved tenant")]
async fn read_tenant(tenant: Tenant) -> Json<TenantView> {
    Json(TenantView {
        label: tenant.label,
        limit: tenant.limit,
    })
}

fn app() -> ExecutableApp {
    ExecutableApp::from_plugin(
        Plugin::new("app")
            .provide(session::provider())
            .provide(tenant::provider())
            .routes(routes![read_tenant]),
    )
    .expect("request-aware providers compile")
}

fn call(request: Request) -> (u16, Value) {
    let executable = app();
    let response = future::block_on(TestApp::new(&executable).call(request));
    let status = response.status();
    (status, response.json::<Value>().unwrap_or(json!(null)))
}

#[test]
fn provider_inputs_arrive_decoded_and_validated() {
    let (status, body) = call(
        Request::get("/tenant?limit=5")
            .header("tenant", "acme")
            .header("cookie", "sid=42"),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["label"], "acme/user-42");
    assert_eq!(body["limit"], 5);

    // The declared default applies when the query is absent entirely.
    let (status, body) = call(
        Request::get("/tenant")
            .header("tenant", "acme")
            .header("cookie", "sid=42"),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["limit"], 20);
}

#[test]
fn a_provider_input_fails_exactly_like_a_handler_input() {
    // A missing required header is found missing before any provider runs.
    let (status, body) = call(Request::get("/tenant").header("cookie", "sid=42"));
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["code"], "missing_input");

    // A declared bound on the provider's query model is enforced.
    let (status, body) = call(
        Request::get("/tenant?limit=0")
            .header("tenant", "acme")
            .header("cookie", "sid=42"),
    );
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["code"], "validation_error");
    assert_eq!(
        body["error"]["details"]["violations"][0]["field"], "limit",
        "{body}"
    );
}

#[test]
fn provider_inputs_fold_into_the_contract_and_the_document_once() {
    let executable = app();
    let operation = executable
        .definition()
        .operations()
        .iter()
        .find(|operation| operation.contract.id.as_str() == "tenant.read")
        .expect("operation is registered");

    let mut declared = operation
        .contract
        .inputs
        .iter()
        .map(|input| input.name.clone())
        .collect::<Vec<_>>();
    declared.sort();
    assert_eq!(
        declared,
        ["paging", "sid", "tenant"],
        "each dependency-origin input appears exactly once"
    );

    let document = blazingly::openapi::to_value(executable.definition());
    let parameters = document["paths"]["/tenant"]["get"]["parameters"]
        .as_array()
        .expect("dependency-origin inputs become parameters");
    let mut names = parameters
        .iter()
        .map(|parameter| {
            (
                parameter["name"].as_str().unwrap_or_default().to_owned(),
                parameter["in"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        [
            ("limit".to_owned(), "query".to_owned()),
            ("sid".to_owned(), "cookie".to_owned()),
            ("tenant".to_owned(), "header".to_owned()),
        ],
        "{parameters:?}"
    );
    let limit = parameters
        .iter()
        .find(|parameter| parameter["name"] == "limit")
        .expect("the query model flattens");
    assert_eq!(limit["schema"]["minimum"], 1, "bounds travel: {limit}");
    assert!(
        !document["paths"]["/tenant"]["get"]["responses"]["422"].is_null(),
        "an operation whose providers decode input documents its rejection"
    );
}

#[test]
fn an_override_bypasses_the_provider_and_its_inputs() {
    let executable = ExecutableApp::from_plugin_with_overrides(
        Plugin::new("app")
            .provide(session::provider())
            .provide(tenant::provider())
            .routes(routes![read_tenant]),
        TestOverrides::new().replace(Provider::value(Tenant {
            label: "mocked".to_owned(),
            limit: 7,
        })),
    )
    .expect("an override over a request-aware provider compiles");

    // No header, no cookie, no query: the mock supplies the value, so the
    // replaced provider's inputs are neither decoded nor required.
    let response = future::block_on(TestApp::new(&executable).call(Request::get("/tenant")));
    assert_eq!(response.status(), 200);
    let body = response.json::<Value>().expect("a JSON response");
    assert_eq!(body["label"], "mocked");
    assert_eq!(body["limit"], 7);
}

#[derive(Clone, Debug)]
struct Audit {
    stamp: String,
}

#[provider(transient)]
async fn audit(Header(tenant): Header<String>) -> Audit {
    Audit {
        stamp: format!("audit-{tenant}"),
    }
}

#[api_model]
#[derive(Clone, Debug)]
struct AuditView {
    stamp: String,
    label: String,
}

#[get("/audited", id = "tenant.audited", summary = "Async provider input")]
async fn read_audited(audit: Audit, tenant: Tenant) -> Json<AuditView> {
    Json(AuditView {
        stamp: audit.stamp,
        label: tenant.label,
    })
}

/// An async provider and a sync chain share one decoded header.
#[test]
fn one_wire_input_feeds_two_providers_through_one_decode() {
    let executable = ExecutableApp::from_plugin(
        Plugin::new("app")
            .provide(session::provider())
            .provide(tenant::provider())
            .provide(audit::provider())
            .routes(routes![read_audited]),
    )
    .expect("shared provider inputs compile");

    let operation = executable
        .definition()
        .operations()
        .iter()
        .find(|operation| operation.contract.id.as_str() == "tenant.audited")
        .expect("operation is registered");
    let tenant_inputs = operation
        .contract
        .inputs
        .iter()
        .filter(|input| input.name == "tenant")
        .count();
    assert_eq!(tenant_inputs, 1, "the shared header folds once");

    let response = future::block_on(
        TestApp::new(&executable).call(
            Request::get("/audited")
                .header("tenant", "acme")
                .header("cookie", "sid=9"),
        ),
    );
    assert_eq!(response.status(), 200);
    let body = response.json::<Value>().expect("a JSON response");
    assert_eq!(body["stamp"], "audit-acme");
    assert_eq!(body["label"], "acme/user-9");
}
