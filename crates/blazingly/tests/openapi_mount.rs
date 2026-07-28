use blazingly::openapi::{OpenApiConfig, OpenApiUi};
use blazingly::prelude::*;
use futures_lite::future;

#[test]
fn openapi_json_and_scalar_ui_are_precompiled_http_assets() {
    let executable = ExecutableApp::new(Vec::new()).expect("empty app should compile");
    let app = TestApp::new(&executable).with_openapi(
        OpenApiConfig::new("Example API", "1.2.3")
            .with_document_path("/spec.json")
            .with_ui_path("/reference")
            .with_ui(OpenApiUi::Scalar),
    );

    let document = future::block_on(app.call(Request::get("/spec.json")));
    assert_eq!(document.status(), 200);
    assert_eq!(
        document.get_header("content-type"),
        Some("application/json")
    );
    let document = document
        .json::<blazingly_json::Value>()
        .expect("OpenAPI document should be JSON");
    assert_eq!(document["openapi"], "3.1.0");
    assert_eq!(
        document["jsonSchemaDialect"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(document["info"]["title"], "Example API");
    assert_eq!(document["info"]["version"], "1.2.3");

    let ui = future::block_on(app.call(Request::get("/reference")));
    assert_eq!(ui.status(), 200);
    let html = ui.text().expect("UI should be UTF-8");
    assert!(html.contains("@scalar/api-reference"));
    assert!(html.contains("data-url=\"/spec.json\""));

    let rejected = future::block_on(app.call(Request::post("/reference")));
    assert_eq!(rejected.status(), 405);
    assert_eq!(rejected.get_header("allow"), Some("GET, HEAD"));
}
