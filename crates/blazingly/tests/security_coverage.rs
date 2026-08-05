//! Declared security with no verifier is caught before the first request.
//!
//! The framework already refused to serve such an operation — fail-closed, 500,
//! correct. What it did not do was say which operation, what was missing, or
//! anything at all until a request arrived. For the one mistake a newcomer is
//! most likely to make with `#[security]`, the diagnostic is the documentation.

use blazingly::prelude::*;
use blazingly::{HttpMethod, MiddlewareScope, SecuritySchemeDescriptor, SecuritySchemeKind};
use futures_lite::future;

#[api_model]
#[derive(Clone, Debug)]
struct Order {
    id: u64,
}

#[get("/orders", id = "orders.list", summary = "List orders")]
#[security("oauth", scopes = ["orders:read"])]
async fn list_orders() -> Json<Vec<Order>> {
    Json(Vec::new())
}

#[get("/orders/{id}", id = "orders.read", summary = "Read one order")]
#[security("oauth")]
async fn read_order(Path(id): Path<u64>) -> Json<Order> {
    Json(Order { id })
}

#[get("/health", id = "health.read", summary = "Liveness probe")]
async fn health() -> Json<&'static str> {
    Json("ok")
}

fn application() -> ExecutableApp {
    ExecutableApp::with_security_schemes(
        routes![list_orders, read_order, health],
        [SecuritySchemeDescriptor::new(
            "oauth",
            SecuritySchemeKind::OAuth2 {
                authorization_url: None,
                token_url: Some("/token".to_owned()),
                scopes: vec!["orders:read".to_owned()],
            },
        )],
    )
    .expect("the operation graph compiles")
}

/// A layer that authenticates nothing, standing in for one that does.
struct Verifier;

impl HttpMiddleware for Verifier {
    fn verifies_security(&self) -> bool {
        true
    }
}

/// Compression, logging, anything that is not authentication.
struct Passive;

impl HttpMiddleware for Passive {
    fn verifies_security(&self) -> bool {
        false
    }
}

#[test]
fn an_unverified_scheme_is_reported_before_a_request_arrives() {
    let app = HttpApp::new(application());
    let error = app
        .check_security_coverage()
        .expect_err("nothing verifies `oauth`");

    let reported = error
        .unverified()
        .iter()
        .map(|entry| entry.operation.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        reported,
        vec!["orders.list", "orders.read"],
        "every unguarded operation is reported at once, and only those"
    );

    let text = error.to_string();
    assert!(text.contains("orders.list"), "{text}");
    assert!(text.contains("GET /orders"), "{text}");
    assert!(text.contains("`oauth`"), "{text}");
    assert!(
        text.contains("with_middleware"),
        "the message names the registration that is missing: {text}"
    );
}

#[test]
fn a_verifying_layer_covers_the_operations_it_reaches() {
    let app = HttpApp::new(application()).with_middleware(Verifier);
    assert!(app.check_security_coverage().is_ok());
}

#[test]
fn a_layer_that_does_not_authenticate_does_not_count() {
    let app = HttpApp::new(application()).with_middleware(Passive);
    assert_eq!(
        app.unverified_security().len(),
        2,
        "a compression or logging layer must not be mistaken for a verifier"
    );
}

#[test]
fn a_scoped_layer_covers_only_where_its_scope_reaches() {
    let app = HttpApp::new(application())
        .with_scoped_middleware(MiddlewareScope::operation("orders.read"), Verifier);

    assert_eq!(
        app.unverified_security()
            .iter()
            .map(|entry| entry.operation.as_str())
            .collect::<Vec<_>>(),
        vec!["orders.list"],
        "a layer scoped to one operation leaves the other unguarded, and the check says so"
    );
}

#[test]
fn opting_out_silences_the_check_the_same_way_it_silences_the_guard() {
    let app = HttpApp::new(application()).with_unverified_security_schemes(true);
    assert!(
        app.check_security_coverage().is_ok(),
        "an application that declares its schemes verified elsewhere still starts"
    );
}

#[test]
fn an_operation_without_security_is_never_reported() {
    let app = HttpApp::new(application());
    assert!(
        app.unverified_security()
            .iter()
            .all(|entry| entry.operation != "health.read")
    );
}

#[test]
fn the_runtime_guard_still_fails_closed() {
    // The startup check is the good first impression; it is not the safety
    // property. An application assembled without ever calling it must still
    // refuse to serve the operation rather than serve it unauthenticated.
    let app = HttpApp::new(application());
    let response = future::block_on(app.call(Request::new(HttpMethod::Get, "/orders")));

    assert_eq!(response.status(), 500);
    let body = String::from_utf8(response.body().to_vec()).expect("the body is UTF-8");
    assert!(body.contains("security_verifier_missing"), "{body}");
    assert!(
        !body.contains("oauth"),
        "the client learns that something is misconfigured, not which scheme is unguarded: {body}"
    );
}
