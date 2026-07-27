use blazingly::prelude::*;
use futures_lite::future;
use serde_json::json;

#[api_model]
#[derive(Clone, Debug)]
struct MacroInput {
    #[min_length(1)]
    value: String,
}

#[api_model]
#[derive(Clone, Debug)]
struct MacroOutput {
    item_id: u64,
    message: String,
}

#[api_error]
enum MacroError {
    #[status(409)]
    #[code("value_already_exists")]
    #[message("The requested value already exists.")]
    ValueAlreadyExists,
}

#[derive(Clone)]
struct MacroPrefix(&'static str);

#[derive(Clone)]
struct MacroContext(String);

#[provider(singleton)]
fn macro_prefix() -> MacroPrefix {
    MacroPrefix("macro")
}

#[provider]
async fn macro_context(prefix: Depends<MacroPrefix>) -> Result<MacroContext, DependencyError> {
    Ok(MacroContext(format!("{}-context", prefix.0)))
}

#[operation(
    method = PUT,
    path = "/macro/{item_id}",
    id = "macro.upsert",
    summary = "Exercise the universal operation macro"
)]
#[mcp::tool(
    name = "macro_upsert",
    description = "Create or replace one macro fixture",
    risk = "write",
    idempotent = true
)]
async fn macro_upsert(
    Path(item_id): Path<u64>,
    Json(input): Json<MacroInput>,
    context: Depends<MacroContext>,
) -> Result<Json<MacroOutput>, MacroError> {
    if input.value == "taken" {
        return Err(MacroError::ValueAlreadyExists);
    }
    Ok(Json(MacroOutput {
        item_id,
        message: format!("{}:{}", context.0, input.value),
    }))
}

#[get(
    "/sync/{item_id}",
    id = "macro.sync",
    summary = "Exercise the synchronous operation fast path"
)]
fn sync_operation(Path(item_id): Path<u64>) -> Json<MacroOutput> {
    Json(MacroOutput {
        item_id,
        message: "sync".to_owned(),
    })
}

#[test]
fn synchronous_operations_use_the_same_typed_http_surface() {
    let executable =
        ExecutableApp::new(routes![sync_operation]).expect("sync operation should compile");
    let response = future::block_on(TestApp::new(&executable).call(Request::get("/sync/11")));

    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .json::<serde_json::Value>()
            .expect("sync response should be JSON"),
        json!({ "item_id": 11, "message": "sync" })
    );
}

#[test]
fn universal_operation_and_provider_macros_share_http_mcp_and_di() {
    assert_eq!(
        macro_prefix::provider().lifetime(),
        blazingly::DependencyLifetime::Singleton
    );
    assert_eq!(
        macro_context::provider().lifetime(),
        blazingly::DependencyLifetime::Request
    );

    let executable = ExecutableApp::from_plugin(
        Plugin::new("macro_surface")
            .provide(macro_prefix::provider())
            .provide(macro_context::provider())
            .routes(routes![macro_upsert]),
    )
    .expect("macro-generated provider graph should compile");

    let descriptor = &executable.definition().operations()[0];
    assert_eq!(descriptor.http.method, blazingly::HttpMethod::Put);
    assert_eq!(descriptor.contract.id.as_str(), "macro.upsert");
    assert_eq!(
        descriptor
            .mcp_tool()
            .expect("operation should have an MCP projection")
            .name,
        "macro_upsert"
    );

    let http = TestApp::new(&executable);
    let response = future::block_on(
        http.call(
            Request::put("/macro/7")
                .json(&json!({ "value": "ready" }))
                .expect("fixture should serialize"),
        ),
    );
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .json::<serde_json::Value>()
            .expect("HTTP response should be JSON"),
        json!({ "item_id": 7, "message": "macro-context:ready" })
    );

    let runtime = blazingly::mcp::McpRuntime::new(&executable);
    let result = future::block_on(runtime.call_tool(
        "macro_upsert",
        json!({ "item_id": 9, "value": "agent" }),
        blazingly::mcp::McpCallContext::default(),
    ))
    .expect("MCP should execute the same operation");
    assert!(!result.is_error);
    assert_eq!(
        result.structured_content,
        Some(
            json!({ "item_id": 9, "message": "macro-context:agent" })
                .as_object()
                .expect("fixture is an object")
                .clone()
        )
    );

    let conflict = future::block_on(
        http.call(
            Request::put("/macro/7")
                .json(&json!({ "value": "taken" }))
                .expect("fixture should serialize"),
        ),
    );
    assert_eq!(conflict.status(), 409);
    assert_eq!(
        conflict
            .json::<serde_json::Value>()
            .expect("typed error should be JSON")["error"]["code"],
        "value_already_exists"
    );
}
