use blazingly::prelude::*;
use blazingly_json::json;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

#[api_model(rename_all = "camelCase")]
#[derive(Clone, Debug)]
struct CreateUser {
    #[min_length(2)]
    #[max_length(100)]
    name: String,

    #[email]
    email: String,
}

#[api_model]
#[derive(Clone, Debug)]
struct UserView {
    id: u64,
    name: String,
    email: String,
}

#[api_model(rename_all = "camelCase")]
#[derive(Clone, Debug)]
struct RequestOptions {
    verbose: bool,
}

#[api_model(rename_all = "camelCase")]
#[derive(Clone, Debug)]
struct ContextualUserView {
    tenant_id: u64,
    user_id: u64,
    request_id: String,
    verbose: bool,
    name: String,
    email: String,
}

#[api_error]
#[derive(Clone, Copy, Debug)]
enum CreateUserError {
    #[status(409)]
    #[code("email_already_exists")]
    #[message("A user with this email already exists.")]
    EmailAlreadyExists,
}

#[api_model(rename_all = "camelCase")]
#[derive(Clone, Debug)]
struct RateLimitDetails {
    retry_after_seconds: u64,
    limit: u64,
}

#[api_error]
#[derive(Clone, Debug)]
enum PipelineError {
    #[status(429)]
    #[code("rate_limited")]
    #[message("The operation rate limit was exceeded.")]
    #[header("retry-after", "30")]
    RateLimited(RateLimitDetails),
}

#[post("/users", id = "users.create", summary = "Create a user")]
#[mcp::tool(
    name = "create_user",
    description = "Create one application user",
    risk = "write",
    confirmation = "required",
    idempotent = false,
    expose_output = "full"
)]
#[allow(clippy::unused_async)]
async fn create_user(Json(input): Json<CreateUser>) -> Result<Created<UserView>, CreateUserError> {
    if input.email == "exists@example.com" {
        return Err(CreateUserError::EmailAlreadyExists);
    }

    Ok(Created(UserView {
        id: 1,
        name: input.name,
        email: input.email,
    }))
}

#[get("/health", id = "health.read", summary = "Read application health")]
#[allow(clippy::unused_async)]
async fn health() -> Json<&'static str> {
    Json("ok")
}

#[post(
    "/tenants/{tenant_id}/users/{user_id}",
    id = "users.contextual_create",
    summary = "Create a contextual user"
)]
#[mcp::tool(
    name = "create_contextual_user",
    description = "Create a user with path, query, header, and JSON inputs",
    risk = "write",
    confirmation = "never",
    idempotent = false,
    expose_output = "full"
)]
#[allow(clippy::unused_async)]
async fn create_contextual_user(
    Path(tenant_id): Path<u64>,
    Path(user_id): Path<u64>,
    Query(options): Query<RequestOptions>,
    Header(request_id): Header<String>,
    Json(input): Json<CreateUser>,
) -> Json<ContextualUserView> {
    Json(ContextualUserView {
        tenant_id,
        user_id,
        request_id,
        verbose: options.verbose,
        name: input.name,
        email: input.email,
    })
}

#[get(
    "/pipeline/accepted",
    id = "pipeline.accepted",
    summary = "Return an accepted response"
)]
#[allow(clippy::unused_async)]
async fn accepted_response() -> WithHeaders<Accepted<UserView>> {
    Accepted(UserView {
        id: 7,
        name: "Ada".to_owned(),
        email: "ada@example.com".to_owned(),
    })
    .header("location", "/jobs/7")
    .header("x-request-id", "req-7")
}

#[get(
    "/pipeline/partial",
    id = "pipeline.partial",
    summary = "Return a custom success status"
)]
#[allow(clippy::unused_async)]
async fn partial_response() -> Status<206, Json<UserView>> {
    Status(Json(UserView {
        id: 7,
        name: "Ada".to_owned(),
        email: "ada@example.com".to_owned(),
    }))
}

#[post(
    "/pipeline/no-content",
    id = "pipeline.no_content",
    summary = "Return no content"
)]
#[mcp::tool(
    name = "complete_without_content",
    description = "Complete an operation without a response body",
    risk = "write",
    confirmation = "never",
    idempotent = true,
    expose_output = "full"
)]
#[allow(clippy::unused_async)]
async fn no_content_response() -> NoContent {
    NoContent
}

#[get(
    "/pipeline/rate-limited",
    id = "pipeline.rate_limited",
    summary = "Return a typed rate-limit error"
)]
#[mcp::tool(
    name = "read_rate_limited",
    description = "Exercise the typed error response pipeline",
    risk = "read",
    confirmation = "never",
    idempotent = true,
    expose_output = "full"
)]
#[allow(clippy::unused_async)]
async fn rate_limited_response() -> Result<Json<UserView>, PipelineError> {
    Err(PipelineError::RateLimited(RateLimitDetails {
        retry_after_seconds: 30,
        limit: 100,
    }))
}

#[get(
    "/pipeline/unsafe-header",
    id = "pipeline.unsafe_header",
    summary = "Exercise response header validation"
)]
#[mcp::tool(
    name = "read_unsafe_header",
    description = "Exercise internal response redaction",
    risk = "read",
    confirmation = "never",
    idempotent = true,
    expose_output = "full"
)]
#[allow(clippy::unused_async)]
async fn unsafe_header_response() -> WithHeaders<Json<&'static str>> {
    Json("must be redacted").header("x-test", "safe\r\nx-injected: true")
}

struct GreetingPrefix(&'static str);

struct InvocationCounter(Rc<Cell<u64>>);

#[derive(Clone)]
struct RequestGreeting {
    text: String,
    invocation: u64,
}

#[derive(Clone)]
struct AuditLabel(String);

#[derive(Clone)]
struct Authorization;

#[api_model(rename_all = "camelCase")]
#[derive(Clone, Debug)]
struct DependencyView {
    user_id: u64,
    message: String,
    audit_label: String,
    invocation: u64,
}

#[get(
    "/di/users/{user_id}",
    id = "di.greeting",
    summary = "Resolve a compiled dependency graph"
)]
#[mcp::tool(
    name = "dependency_greeting",
    description = "Resolve the same dependency graph through MCP",
    risk = "read",
    confirmation = "never",
    idempotent = true,
    expose_output = "full"
)]
#[allow(clippy::unused_async)]
async fn dependency_greeting(
    Path(user_id): Path<u64>,
    greeting: RequestGreeting,
    audit: AuditLabel,
) -> Json<DependencyView> {
    Json(DependencyView {
        user_id,
        message: greeting.text.clone(),
        audit_label: audit.0.clone(),
        invocation: greeting.invocation,
    })
}

#[get(
    "/di/authorized",
    id = "di.authorized",
    summary = "Reject through a dependency"
)]
#[mcp::tool(
    name = "dependency_authorized",
    description = "Exercise dependency rejection through MCP",
    risk = "read",
    confirmation = "never",
    idempotent = true,
    expose_output = "full"
)]
#[allow(clippy::unused_async)]
async fn dependency_authorized(_authorization: Authorization) -> Json<&'static str> {
    Json("authorized")
}

#[derive(Clone)]
struct MissingDependency;
struct CycleA;
struct CycleB;
struct RequestOnly;
struct InvalidSingleton;
#[derive(Clone)]
struct TransientValue(u64);
#[derive(Clone)]
struct BaseResource;
#[derive(Clone)]
struct ChildResource;
#[derive(Clone)]
struct FailingResource;
#[derive(Clone)]
struct AsyncResource(u64);
#[derive(Clone)]
struct HookLog(Rc<RefCell<Vec<&'static str>>>);
#[derive(Clone)]
struct HookResource;
const ORDERED_HOOK_EVENTS: [&str; 9] = [
    "root.request",
    "child.request",
    "provider",
    "root.pre",
    "child.pre",
    "handler",
    "finalizer",
    "child.response",
    "root.response",
];

#[api_model]
#[derive(Clone, Debug)]
struct AsyncDependencyView {
    value: u64,
}

#[api_model(rename_all = "camelCase")]
#[derive(Clone, Debug)]
struct LifecycleView {
    first_transient: u64,
    second_transient: u64,
}

#[get(
    "/di/missing",
    id = "di.missing",
    summary = "Require an unregistered dependency"
)]
#[allow(clippy::unused_async)]
async fn missing_dependency(_missing: MissingDependency) -> NoContent {
    NoContent
}

#[get(
    "/di/lifecycle",
    id = "di.lifecycle",
    summary = "Exercise transient and finalized dependencies"
)]
#[allow(clippy::unused_async)]
async fn dependency_lifecycle(
    first: TransientValue,
    second: TransientValue,
    _child: ChildResource,
) -> Json<LifecycleView> {
    Json(LifecycleView {
        first_transient: first.0,
        second_transient: second.0,
    })
}

#[get(
    "/di/failing-lifecycle",
    id = "di.failing_lifecycle",
    summary = "Finalize a partial dependency graph"
)]
#[allow(clippy::unused_async)]
async fn failing_dependency_lifecycle(_resource: FailingResource) -> NoContent {
    NoContent
}

#[get(
    "/di/async",
    id = "di.async",
    summary = "Resolve and finalize an async dependency"
)]
#[mcp::tool(
    name = "async_dependency",
    description = "Resolve the same async dependency through MCP",
    risk = "read",
    confirmation = "never",
    idempotent = true,
    expose_output = "full"
)]
#[allow(clippy::unused_async)]
async fn async_dependency(resource: AsyncResource) -> Json<AsyncDependencyView> {
    Json(AsyncDependencyView { value: resource.0 })
}

#[get(
    "/hooks/ordered",
    id = "hooks.ordered",
    summary = "Execute inherited plugin hooks"
)]
#[mcp::tool(
    name = "ordered_hooks",
    description = "Execute inherited plugin hooks through MCP",
    risk = "read",
    confirmation = "never",
    idempotent = true,
    expose_output = "full"
)]
#[allow(clippy::unused_async)]
async fn ordered_hooks(log: HookLog, _resource: HookResource) -> Json<&'static str> {
    log.0.borrow_mut().push("handler");
    Json("ok")
}

#[get(
    "/hooks/rejected",
    id = "hooks.rejected",
    summary = "Reject from an inherited plugin hook"
)]
#[mcp::tool(
    name = "rejected_hook",
    description = "Reject from a plugin hook through MCP",
    risk = "read",
    confirmation = "never",
    idempotent = true,
    expose_output = "full"
)]
#[allow(clippy::unused_async)]
async fn rejected_hook(log: HookLog) -> NoContent {
    log.0.borrow_mut().push("handler");
    NoContent
}

#[test]
fn macros_and_explicit_routes_create_one_operation_graph() {
    let executable =
        ExecutableApp::new(routes![create_user, health]).expect("application should be valid");
    let app = executable.definition();

    let operations = app.operations();
    assert_eq!(operations.len(), 2);
    assert_eq!(operations[0].contract.id.as_str(), "health.read");
    assert_eq!(operations[1].contract.id.as_str(), "users.create");
    assert_eq!(
        operations[1]
            .contract
            .input
            .as_ref()
            .expect("POST operation should have an input")
            .rust_name,
        "CreateUser"
    );
    assert_eq!(operations[1].contract.responses[0].status, 201);
    assert_eq!(
        operations[1].contract.responses[0]
            .body
            .as_ref()
            .expect("response should have a body")
            .rust_name,
        "UserView"
    );

    let openapi = blazingly::openapi::to_value(app);
    assert_eq!(
        openapi["paths"]["/users"]["post"]["operationId"],
        "users.create"
    );
    assert_eq!(
        openapi["paths"]["/health"]["get"]["operationId"],
        "health.read"
    );
    assert_eq!(
        openapi["components"]["schemas"]["CreateUser"]["properties"]["name"]["minLength"],
        2
    );
    assert_eq!(
        openapi["components"]["schemas"]["CreateUser"]["properties"]["email"]["format"],
        "email"
    );
    assert_eq!(
        openapi["paths"]["/users"]["post"]["responses"]["409"]["x-blazingly-error-code"],
        "email_already_exists"
    );
    assert_eq!(
        openapi["paths"]["/users"]["post"]["responses"]["409"]["description"],
        "A user with this email already exists."
    );
    assert_eq!(
        openapi["paths"]["/users"]["post"]["responses"]["422"]["x-blazingly-automatic"], true,
        "an operation whose body is decoded documents the rejection it can return"
    );
    assert!(
        openapi["paths"]["/health"]["get"]["responses"]["422"].is_null(),
        "an operation that decodes no input cannot be rejected before it runs"
    );

    let mcp = blazingly::mcp::to_value(app);
    assert_eq!(mcp["tools"][0]["name"], "create_user");
    assert_eq!(mcp["tools"][0]["x-blazingly"]["confirmation"], "required");
    assert_eq!(
        mcp["tools"][0]["x-blazingly"]["confirmationMetaKey"],
        "dev.blazingly/confirmed"
    );
    assert_eq!(
        mcp["tools"][0]["inputSchema"]["properties"]["name"]["maxLength"],
        100
    );
    assert_eq!(
        mcp["tools"][0]["outputSchema"]["properties"]["id"]["type"],
        "integer"
    );

    let api_markdown = blazingly::docs::api_markdown(app);
    let mcp_markdown = blazingly::docs::mcp_markdown(app);
    assert!(api_markdown.contains("POST /users"));
    assert!(mcp_markdown.contains("Create one application user"));
    assert!(mcp_markdown.contains("min length 2"));
    assert!(mcp_markdown.contains("email_already_exists"));
}

#[test]
fn handlers_remain_regular_typed_rust_functions() {
    let response = poll_ready(create_user(Json(CreateUser {
        name: "Ada".to_owned(),
        email: "ada@example.com".to_owned(),
    })))
    .expect("valid user should be created");

    assert_eq!(response.0.id, 1);
    assert_eq!(response.0.name, "Ada");
    assert_eq!(response.0.email, "ada@example.com");
    assert_eq!(poll_ready(health()).0, "ok");
}

#[test]
fn native_mcp_uses_the_shared_validation_handler_and_error_pipeline() {
    let executable =
        ExecutableApp::new(routes![create_user, health]).expect("application should be valid");
    let runtime = blazingly::mcp::McpRuntime::new(&executable);

    let confirmation = poll_ready(runtime.call_tool(
        "create_user",
        json!({ "name": "Ada", "email": "ada@example.com" }),
        blazingly::mcp::McpCallContext::default(),
    ))
    .expect("confirmation is a tool result, not a protocol failure");
    assert!(confirmation.is_error);
    assert!(text_content(&confirmation).contains("confirmation_required"));

    let invalid = poll_ready(runtime.call_tool(
        "create_user",
        json!({ "name": "A", "email": "not-an-email" }),
        blazingly::mcp::McpCallContext::confirmed(),
    ))
    .expect("validation is a tool result, not a protocol failure");
    assert!(invalid.is_error);
    assert!(text_content(&invalid).contains("validation_error"));
    assert!(text_content(&invalid).contains("min_length"));

    let domain_error = poll_ready(runtime.call_tool(
        "create_user",
        json!({ "name": "Ada", "email": "exists@example.com" }),
        blazingly::mcp::McpCallContext::confirmed(),
    ))
    .expect("domain errors are visible to the agent");
    assert!(domain_error.is_error);
    assert!(text_content(&domain_error).contains("email_already_exists"));

    let success = poll_ready(runtime.call_tool(
        "create_user",
        json!({ "name": "Ada", "email": "ada@example.com" }),
        blazingly::mcp::McpCallContext::confirmed(),
    ))
    .expect("known tool should execute");
    assert!(!success.is_error);
    let structured = success
        .structured_content
        .as_ref()
        .expect("full exposure should return structured content");
    assert_eq!(structured["id"], 1);
    assert_eq!(structured["name"], "Ada");

    let wire = blazingly_json::to_value(&success).expect("MCP result should serialize");
    assert_eq!(wire["content"][0]["type"], "text");
    assert_eq!(wire["structuredContent"]["email"], "ada@example.com");
    assert!(wire.get("isError").is_none());

    let unknown = poll_ready(runtime.call_tool(
        "missing_tool",
        json!({}),
        blazingly::mcp::McpCallContext::default(),
    ))
    .expect_err("unknown tools are protocol errors");
    assert_eq!(unknown.code, -32_602);
}

#[test]
fn http_test_app_and_mcp_execute_the_same_operation() {
    let executable =
        ExecutableApp::new(routes![create_user, health]).expect("application should be valid");
    let http = TestApp::new(&executable);
    let mcp = blazingly::mcp::McpRuntime::new(&executable);

    let http_success = poll_ready(
        http.call(
            Request::post("/users")
                .json(&json!({ "name": "Ada", "email": "ada@example.com" }))
                .expect("test input should serialize"),
        ),
    );
    assert_eq!(http_success.status(), 201);
    assert_eq!(
        http_success.get_header("content-type"),
        Some("application/json")
    );
    let http_body = http_success
        .json::<blazingly_json::Value>()
        .expect("HTTP success should be JSON");

    let mcp_success = poll_ready(runtime_call_create_user(
        &mcp,
        json!({ "name": "Ada", "email": "ada@example.com" }),
    ));
    assert!(!mcp_success.is_error);
    assert_eq!(
        mcp_success
            .structured_content
            .as_ref()
            .expect("MCP success should expose structured content"),
        http_body
            .as_object()
            .expect("HTTP success should be a JSON object")
    );

    let http_domain_error = poll_ready(
        http.call(
            Request::post("/users")
                .json(&json!({ "name": "Ada", "email": "exists@example.com" }))
                .expect("test input should serialize"),
        ),
    );
    assert_eq!(http_domain_error.status(), 409);
    assert_eq!(
        http_domain_error
            .json::<blazingly_json::Value>()
            .expect("HTTP error should be JSON")["error"]["code"],
        "email_already_exists"
    );
    let mcp_domain_error = poll_ready(runtime_call_create_user(
        &mcp,
        json!({ "name": "Ada", "email": "exists@example.com" }),
    ));
    assert!(mcp_domain_error.is_error);
    assert!(text_content(&mcp_domain_error).contains("email_already_exists"));
}

#[test]
fn http_test_app_enforces_routing_body_and_error_contracts() {
    let executable =
        ExecutableApp::new(routes![create_user, health]).expect("application should be valid");
    let http = TestApp::new(&executable).with_max_body_bytes(128);

    let health_response = poll_ready(http.call(Request::get("/health")));
    assert_eq!(health_response.status(), 200);
    assert_eq!(
        health_response
            .json::<blazingly_json::Value>()
            .expect("health response should be JSON"),
        "ok"
    );

    let missing = poll_ready(http.call(Request::get("/missing")));
    assert_http_error(&missing, 404, "not_found");

    let wrong_method = poll_ready(http.call(Request::get("/users")));
    assert_http_error(&wrong_method, 405, "method_not_allowed");
    assert_eq!(wrong_method.get_header("allow"), Some("POST"));

    let missing_media_type = poll_ready(http.call(
        Request::post("/users").body(br#"{"name":"Ada","email":"ada@example.com"}"#.to_vec()),
    ));
    assert_http_error(&missing_media_type, 415, "unsupported_media_type");

    let invalid_json = poll_ready(
        http.call(
            Request::post("/users")
                .header("Content-Type", "application/json; charset=utf-8")
                .body(b"{".to_vec()),
        ),
    );
    assert_http_error(&invalid_json, 422, "invalid_json");

    let invalid_model = poll_ready(
        http.call(
            Request::post("/users")
                .json(&json!({ "name": "A", "email": "not-an-email" }))
                .expect("test input should serialize"),
        ),
    );
    assert_http_error(&invalid_model, 422, "validation_error");

    let too_large = poll_ready(
        http.call(
            Request::post("/users")
                .header("content-type", "application/json")
                .body(vec![b'x'; 129]),
        ),
    );
    assert_http_error(&too_large, 413, "payload_too_large");
}

#[test]
fn multiple_typed_arguments_share_http_mcp_and_documentation_contracts() {
    let executable =
        ExecutableApp::new(routes![create_contextual_user]).expect("application should be valid");
    let http = TestApp::new(&executable);

    let response = poll_ready(
        http.call(
            Request::post("/tenants/41/users/7?verbose=true")
                .header("request-id", "req-123")
                .json(&json!({ "name": "Ada", "email": "ada@example.com" }))
                .expect("test input should serialize"),
        ),
    );
    assert_eq!(response.status(), 200);
    let http_body = response
        .json::<blazingly_json::Value>()
        .expect("HTTP response should be JSON");
    assert_eq!(http_body["tenantId"], 41);
    assert_eq!(http_body["userId"], 7);
    assert_eq!(http_body["requestId"], "req-123");
    assert_eq!(http_body["verbose"], true);

    let runtime = blazingly::mcp::McpRuntime::new(&executable);
    let mcp_result = poll_ready(runtime.call_tool(
        "create_contextual_user",
        json!({
            "tenant_id": 41,
            "user_id": 7,
            "verbose": true,
            "request_id": "req-123",
            "name": "Ada",
            "email": "ada@example.com"
        }),
        blazingly::mcp::McpCallContext::default(),
    ))
    .expect("known MCP tool should execute");
    assert!(!mcp_result.is_error);
    assert_eq!(
        mcp_result
            .structured_content
            .as_ref()
            .expect("MCP should return structured content"),
        http_body
            .as_object()
            .expect("HTTP result should be an object")
    );

    let openapi = blazingly::openapi::to_value(executable.definition());
    let operation = &openapi["paths"]["/tenants/{tenant_id}/users/{user_id}"]["post"];
    assert_eq!(operation["parameters"][0]["name"], "tenant_id");
    assert_eq!(operation["parameters"][0]["in"], "path");
    assert_eq!(operation["parameters"][2]["name"], "verbose");
    assert_eq!(operation["parameters"][2]["in"], "query");
    assert_eq!(operation["parameters"][3]["name"], "request-id");
    assert_eq!(operation["parameters"][3]["in"], "header");
    assert_eq!(
        operation["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/CreateUser"
    );

    let tools = blazingly::mcp::to_value(executable.definition());
    let schema = &tools["tools"][0]["inputSchema"];
    assert_eq!(
        schema["properties"]["tenant_id"]["x-blazingly-source"],
        "path"
    );
    assert_eq!(
        schema["properties"]["verbose"]["x-blazingly-source"],
        "query"
    );
    assert_eq!(
        schema["properties"]["request_id"]["x-blazingly-source"],
        "header"
    );
    assert_eq!(schema["properties"]["name"]["x-blazingly-source"], "json");

    let invalid_path = poll_ready(
        http.call(
            Request::post("/tenants/not-a-number/users/7?verbose=true")
                .header("request-id", "req-123")
                .json(&json!({ "name": "Ada", "email": "ada@example.com" }))
                .expect("test input should serialize"),
        ),
    );
    assert_http_error(&invalid_path, 422, "invalid_input");
}

#[test]
fn complete_response_and_error_pipeline_is_transport_consistent() {
    let executable = ExecutableApp::new(routes![
        accepted_response,
        partial_response,
        no_content_response,
        rate_limited_response,
        unsafe_header_response,
    ])
    .expect("response pipeline application should be valid");
    let http = TestApp::new(&executable);

    let accepted = poll_ready(http.call(Request::get("/pipeline/accepted")));
    assert_eq!(accepted.status(), 202);
    assert_eq!(accepted.get_header("location"), Some("/jobs/7"));
    assert_eq!(accepted.get_header("x-request-id"), Some("req-7"));
    assert_eq!(
        accepted
            .json::<blazingly_json::Value>()
            .expect("accepted response should be JSON")["id"],
        7
    );

    let partial = poll_ready(http.call(Request::get("/pipeline/partial")));
    assert_eq!(partial.status(), 206);

    let no_content = poll_ready(http.call(Request::post("/pipeline/no-content")));
    assert_eq!(no_content.status(), 204);
    assert!(no_content.body().is_empty());
    assert_eq!(no_content.get_header("content-type"), None);

    let typed_error = poll_ready(http.call(Request::get("/pipeline/rate-limited")));
    assert_http_error(&typed_error, 429, "rate_limited");
    assert_eq!(typed_error.get_header("retry-after"), Some("30"));
    let error_body = typed_error
        .json::<blazingly_json::Value>()
        .expect("typed error should be JSON");
    assert_eq!(error_body["error"]["details"]["retryAfterSeconds"], 30);
    assert_eq!(error_body["error"]["details"]["limit"], 100);

    let unsafe_header = poll_ready(http.call(Request::get("/pipeline/unsafe-header")));
    assert_http_error(&unsafe_header, 500, "internal_error");
    assert_eq!(unsafe_header.get_header("x-injected"), None);
    assert!(
        !unsafe_header
            .text()
            .expect("internal error should be UTF-8")
            .contains("must be redacted")
    );

    let runtime = blazingly::mcp::McpRuntime::new(&executable);
    let mcp_error = poll_ready(runtime.call_tool(
        "read_rate_limited",
        json!({}),
        blazingly::mcp::McpCallContext::default(),
    ))
    .expect("typed errors are MCP tool results");
    assert!(mcp_error.is_error);
    assert!(text_content(&mcp_error).contains("rate_limited"));
    assert!(text_content(&mcp_error).contains("retryAfterSeconds"));

    let mcp_no_content = poll_ready(runtime.call_tool(
        "complete_without_content",
        json!({}),
        blazingly::mcp::McpCallContext::default(),
    ))
    .expect("no-content operation should execute");
    assert!(!mcp_no_content.is_error);
    assert!(mcp_no_content.structured_content.is_none());
    assert!(text_content(&mcp_no_content).contains("status 204"));

    let mcp_internal = poll_ready(runtime.call_tool(
        "read_unsafe_header",
        json!({}),
        blazingly::mcp::McpCallContext::default(),
    ))
    .expect_err("internal response failures are MCP protocol errors");
    assert_eq!(mcp_internal.code, -32_603);
    assert_eq!(mcp_internal.message, "the operation could not be completed");
    assert!(!mcp_internal.message.contains("header"));

    let openapi = blazingly::openapi::to_value(executable.definition());
    assert_eq!(
        openapi["paths"]["/pipeline/accepted"]["get"]["responses"]["202"]["content"]["application/json"]
            ["schema"]["x-rust-type"],
        "UserView"
    );
    assert_eq!(
        openapi["paths"]["/pipeline/no-content"]["post"]["responses"]["204"]["description"],
        "Successful response"
    );
    let rate_limit = &openapi["paths"]["/pipeline/rate-limited"]["get"]["responses"]["429"];
    assert_eq!(
        rate_limit["content"]["application/json"]["schema"]["properties"]["error"]["properties"]["details"]
            ["$ref"],
        "#/components/schemas/RateLimitDetails"
    );
    assert_eq!(rate_limit["headers"]["retry-after"]["example"], "30");
}

#[test]
fn plugin_scopes_compile_di_once_and_share_it_across_http_and_mcp() {
    let invocations = Rc::new(Cell::new(0));
    let application = Plugin::new("app")
        .provide(Provider::value(GreetingPrefix("root")))
        .provide(Provider::value(InvocationCounter(Rc::clone(&invocations))))
        .plugin(
            Plugin::new("users")
                .provide(Provider::value(GreetingPrefix("users")))
                .provide(Provider::request(
                    |prefix: Depends<GreetingPrefix>, counter: Depends<InvocationCounter>| {
                        let invocation = counter.0.get() + 1;
                        counter.0.set(invocation);
                        RequestGreeting {
                            text: format!("{}-hello", prefix.0),
                            invocation,
                        }
                    },
                ))
                .provide(Provider::request(|greeting: Depends<RequestGreeting>| {
                    AuditLabel(format!("{}:{}", greeting.text, greeting.invocation))
                }))
                .provide(Provider::try_request(
                    || -> Result<Authorization, DependencyError> {
                        Err(DependencyError::rejected(OperationFailure::new(
                            401,
                            "missing_token",
                            "Authentication is required.",
                        )))
                    },
                ))
                .routes(routes![dependency_greeting, dependency_authorized]),
        );
    let executable =
        ExecutableApp::from_plugin(application).expect("plugin dependency graph should compile");
    let http = TestApp::new(&executable);

    let response = poll_ready(http.call(Request::get("/di/users/42")));
    assert_eq!(response.status(), 200);
    let body = response
        .json::<blazingly_json::Value>()
        .expect("dependency response should be JSON");
    assert_eq!(body["userId"], 42);
    assert_eq!(body["message"], "users-hello");
    assert_eq!(body["auditLabel"], "users-hello:1");
    assert_eq!(body["invocation"], 1);

    let rejected = poll_ready(http.call(Request::get("/di/authorized")));
    assert_http_error(&rejected, 401, "missing_token");

    let runtime = blazingly::mcp::McpRuntime::new(&executable);
    let mcp = poll_ready(runtime.call_tool(
        "dependency_greeting",
        json!({ "user_id": 7 }),
        blazingly::mcp::McpCallContext::default(),
    ))
    .expect("MCP should execute the compiled dependency plan");
    assert!(!mcp.is_error);
    assert_eq!(
        mcp.structured_content
            .as_ref()
            .expect("full output should be structured")["message"],
        "users-hello"
    );
    assert_eq!(invocations.get(), 2);

    let mcp_rejected = poll_ready(runtime.call_tool(
        "dependency_authorized",
        json!({}),
        blazingly::mcp::McpCallContext::default(),
    ))
    .expect("dependency rejection is an agent-visible tool error");
    assert!(mcp_rejected.is_error);
    assert!(text_content(&mcp_rejected).contains("missing_token"));

    let descriptor = dependency_greeting::descriptor();
    assert_eq!(descriptor.contract.inputs.len(), 1);
    assert_eq!(descriptor.contract.inputs[0].name, "user_id");
    assert_eq!(descriptor.contract.dependencies.len(), 2);
    assert!(
        descriptor.contract.dependencies[0]
            .rust_name
            .ends_with("RequestGreeting")
    );
    let openapi = blazingly::openapi::to_value(executable.definition());
    let parameters = &openapi["paths"]["/di/users/{user_id}"]["get"]["parameters"];
    assert_eq!(parameters.as_array().map(Vec::len), Some(1));
    assert_eq!(
        openapi["paths"]["/di/users/{user_id}"]["get"]["x-blazingly-dependencies"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    let api_docs = blazingly::docs::api_markdown(executable.definition());
    assert!(api_docs.contains("Dependency:"));
    assert!(api_docs.contains("RequestGreeting"));
    let tools = blazingly::mcp::to_value(executable.definition());
    let greeting_tool = tools["tools"]
        .as_array()
        .expect("tools should be an array")
        .iter()
        .find(|tool| tool["name"] == "dependency_greeting")
        .expect("dependency tool should be documented");
    assert!(greeting_tool["inputSchema"]["properties"]["greeting"].is_null());
    assert!(greeting_tool["inputSchema"]["properties"]["audit"].is_null());
}

#[test]
fn compiled_di_honors_transient_lifetime_and_reverse_finalizer_order() {
    let transient_count = Rc::new(Cell::new(0));
    let cleanup = Rc::new(RefCell::new(Vec::new()));
    let transient_counter = Rc::clone(&transient_count);
    let base_cleanup = Rc::clone(&cleanup);
    let child_cleanup = Rc::clone(&cleanup);
    let executable = ExecutableApp::from_plugin(
        Plugin::new("lifecycle")
            .provide(Provider::transient(move || {
                let value = transient_counter.get() + 1;
                transient_counter.set(value);
                TransientValue(value)
            }))
            .provide(Provider::request_scoped(
                || BaseResource,
                move |_resource: Depends<BaseResource>, _outcome: RequestOutcome<'_>| {
                    base_cleanup.borrow_mut().push("base");
                },
            ))
            .provide(Provider::request_scoped(
                |_base: Depends<BaseResource>| ChildResource,
                move |_resource: Depends<ChildResource>, _outcome: RequestOutcome<'_>| {
                    child_cleanup.borrow_mut().push("child");
                },
            ))
            .operation(dependency_lifecycle::executable()),
    )
    .expect("lifecycle dependency graph should compile");

    let response = poll_ready(TestApp::new(&executable).call(Request::get("/di/lifecycle")));
    assert_eq!(response.status(), 200);
    let body = response
        .json::<blazingly_json::Value>()
        .expect("lifecycle response should be JSON");
    assert_eq!(body["firstTransient"], 1);
    assert_eq!(body["secondTransient"], 2);
    assert_eq!(transient_count.get(), 2);
    assert_eq!(&*cleanup.borrow(), &["child", "base"]);

    cleanup.borrow_mut().clear();
    let failed_cleanup = Rc::clone(&cleanup);
    let failing = ExecutableApp::from_plugin(
        Plugin::new("failing_lifecycle")
            .provide(Provider::request_scoped(
                || BaseResource,
                // Records the outcome, not just the fact of teardown: when a
                // later provider rejects, this one must be told the rejection
                // that answered the request rather than a bare "it ended".
                move |_resource: Depends<BaseResource>, outcome: RequestOutcome<'_>| {
                    failed_cleanup.borrow_mut().push(
                        if outcome.code() == Some("dependency_unavailable") {
                            "base:dependency_unavailable"
                        } else {
                            "base:unexpected"
                        },
                    );
                },
            ))
            .provide(Provider::try_request(
                |_base: Depends<BaseResource>| -> Result<FailingResource, DependencyError> {
                    Err(DependencyError::rejected(OperationFailure::new(
                        503,
                        "dependency_unavailable",
                        "Dependency is unavailable.",
                    )))
                },
            ))
            .operation(failing_dependency_lifecycle::executable()),
    )
    .expect("fallible lifecycle dependency graph should compile");
    let rejected = poll_ready(TestApp::new(&failing).call(Request::get("/di/failing-lifecycle")));
    assert_http_error(&rejected, 503, "dependency_unavailable");
    assert_eq!(
        &*cleanup.borrow(),
        &["base:dependency_unavailable"],
        "an already-built dependency is torn down knowing which rejection ended the request"
    );
}

#[test]
fn async_providers_and_finalizers_are_runtime_neutral_across_http_and_mcp() {
    let created = Rc::new(Cell::new(0));
    let finalized = Rc::new(Cell::new(0));
    let factory_counter = Rc::clone(&created);
    let finalizer_counter = Rc::clone(&finalized);
    let executable = ExecutableApp::from_plugin(
        Plugin::new("async_dependencies")
            .provide(Provider::request_async_scoped(
                move || {
                    let counter = Rc::clone(&factory_counter);
                    async move {
                        YieldOnce::new().await;
                        let value = counter.get() + 1;
                        counter.set(value);
                        AsyncResource(value)
                    }
                },
                move |resource: Depends<AsyncResource>, outcome: RequestOutcome<'_>| {
                    let counter = Rc::clone(&finalizer_counter);
                    // The outcome is read before the future is built: it borrows
                    // the failure, so an async finalizer must take what it needs
                    // rather than carry the borrow across an await.
                    let succeeded = outcome.succeeded();
                    async move {
                        YieldOnce::new().await;
                        counter.set(if succeeded { resource.0 } else { 0 });
                    }
                },
            ))
            .operation(async_dependency::executable()),
    )
    .expect("async dependency graph should compile");

    let http = TestApp::new(&executable);
    let response = poll_until_ready(http.call(Request::get("/di/async")));
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .json::<blazingly_json::Value>()
            .expect("async dependency response should be JSON")["value"],
        1
    );
    assert_eq!(finalized.get(), 1);

    let runtime = blazingly::mcp::McpRuntime::new(&executable);
    let mcp = poll_until_ready(runtime.call_tool(
        "async_dependency",
        json!({}),
        blazingly::mcp::McpCallContext::default(),
    ))
    .expect("MCP should execute async dependencies");
    assert!(!mcp.is_error);
    assert_eq!(
        mcp.structured_content
            .as_ref()
            .expect("full output should be structured")["value"],
        2
    );
    assert_eq!(created.get(), 2);
    assert_eq!(finalized.get(), 2);
}

#[test]
fn inherited_async_plugin_hooks_compile_in_lifecycle_order() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let hook_log = HookLog(Rc::clone(&events));
    let provider_log = hook_log.clone();
    let finalizer_log = hook_log.clone();
    let root_request_log = hook_log.clone();
    let root_pre_log = hook_log.clone();
    let root_response_log = hook_log.clone();
    let child_request_log = hook_log.clone();
    let child_pre_log = hook_log.clone();
    let child_response_log = hook_log.clone();

    let application = Plugin::new("app")
        .provide(Provider::value(hook_log))
        .provide(Provider::request_scoped(
            move || {
                provider_log.0.borrow_mut().push("provider");
                HookResource
            },
            move |_resource: Depends<HookResource>, _outcome: RequestOutcome<'_>| {
                finalizer_log.0.borrow_mut().push("finalizer");
            },
        ))
        .on_request(move |context| {
            let log = root_request_log.clone();
            async move {
                YieldOnce::new().await;
                assert_eq!(context.operation_id(), "hooks.ordered");
                log.0.borrow_mut().push("root.request");
                Ok(())
            }
        })
        .pre_handler(move |context| {
            let log = root_pre_log.clone();
            async move {
                YieldOnce::new().await;
                assert_eq!(context.operation_id(), "hooks.ordered");
                log.0.borrow_mut().push("root.pre");
                Ok(())
            }
        })
        .on_response(move |context, outcome| {
            let log = root_response_log.clone();
            async move {
                YieldOnce::new().await;
                assert_eq!(context.operation_id(), "hooks.ordered");
                assert_eq!(outcome.status, 200);
                assert_eq!(outcome.kind, HookOutcomeKind::Success);
                log.0.borrow_mut().push("root.response");
            }
        })
        .plugin(
            Plugin::new("child")
                .on_request(move |_context| {
                    let log = child_request_log.clone();
                    async move {
                        YieldOnce::new().await;
                        log.0.borrow_mut().push("child.request");
                        Ok(())
                    }
                })
                .pre_handler(move |_context| {
                    let log = child_pre_log.clone();
                    async move {
                        YieldOnce::new().await;
                        log.0.borrow_mut().push("child.pre");
                        Ok(())
                    }
                })
                .on_response(move |_context, _outcome| {
                    let log = child_response_log.clone();
                    async move {
                        YieldOnce::new().await;
                        log.0.borrow_mut().push("child.response");
                    }
                })
                .operation(ordered_hooks::executable()),
        );
    let executable = ExecutableApp::from_plugin(application).expect("hook scopes should compile");
    let response = poll_until_ready(TestApp::new(&executable).call(Request::get("/hooks/ordered")));
    assert_eq!(response.status(), 200);
    assert_eq!(&*events.borrow(), &ORDERED_HOOK_EVENTS);

    events.borrow_mut().clear();
    let runtime = blazingly::mcp::McpRuntime::new(&executable);
    let result = poll_until_ready(runtime.call_tool(
        "ordered_hooks",
        json!({}),
        blazingly::mcp::McpCallContext::default(),
    ))
    .expect("MCP should execute compiled hooks");
    assert!(!result.is_error);
    assert_eq!(&*events.borrow(), &ORDERED_HOOK_EVENTS);
}

#[test]
fn rejecting_plugin_hook_uses_the_shared_http_mcp_error_pipeline() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let hook_log = HookLog(Rc::clone(&events));
    let rejection_log = hook_log.clone();
    let response_log = hook_log.clone();
    let executable = ExecutableApp::from_plugin(
        Plugin::new("rejecting_hooks")
            .provide(Provider::value(hook_log))
            .on_request(move |_context| {
                let log = rejection_log.clone();
                async move {
                    log.0.borrow_mut().push("reject");
                    Err(DependencyError::rejected(OperationFailure::new(
                        403,
                        "hook_forbidden",
                        "The hook rejected this operation.",
                    )))
                }
            })
            .on_response(move |_context, outcome| {
                let log = response_log.clone();
                async move {
                    assert_eq!(outcome.status, 403);
                    assert_eq!(outcome.kind, HookOutcomeKind::DomainError);
                    log.0.borrow_mut().push("response");
                }
            })
            .operation(rejected_hook::executable()),
    )
    .expect("rejecting hook graph should compile");

    let response =
        poll_until_ready(TestApp::new(&executable).call(Request::get("/hooks/rejected")));
    assert_http_error(&response, 403, "hook_forbidden");
    assert_eq!(&*events.borrow(), &["reject", "response"]);

    events.borrow_mut().clear();
    let runtime = blazingly::mcp::McpRuntime::new(&executable);
    let result = poll_until_ready(runtime.call_tool(
        "rejected_hook",
        json!({}),
        blazingly::mcp::McpCallContext::default(),
    ))
    .expect("hook rejection should be an MCP tool error");
    assert!(result.is_error);
    assert!(text_content(&result).contains("hook_forbidden"));
    assert_eq!(&*events.borrow(), &["reject", "response"]);
}

#[test]
fn compiled_di_reports_missing_cycles_and_invalid_lifetimes_at_build_time() {
    let missing = ExecutableApp::new(routes![missing_dependency])
        .err()
        .expect("missing dependency should fail compilation");
    assert!(matches!(
        missing,
        blazingly::ExecutableBuildError::MissingProvider {
            dependency,
            ..
        } if dependency.ends_with("MissingDependency")
    ));

    let cycle = ExecutableApp::from_plugin(
        Plugin::new("cycles")
            .provide(Provider::request(|_b: Depends<CycleB>| CycleA))
            .provide(Provider::request(|_a: Depends<CycleA>| CycleB)),
    )
    .err()
    .expect("provider cycle should fail compilation");
    assert!(matches!(
        cycle,
        blazingly::ExecutableBuildError::ProviderCycle { .. }
    ));

    let invalid_lifetime = ExecutableApp::from_plugin(
        Plugin::new("lifetimes")
            .provide(Provider::request(|| RequestOnly))
            .provide(Provider::singleton(|_request: Depends<RequestOnly>| {
                InvalidSingleton
            })),
    )
    .err()
    .expect("singleton to request edge should fail compilation");
    assert!(matches!(
        invalid_lifetime,
        blazingly::ExecutableBuildError::InvalidLifetime { .. }
    ));
}

#[test]
fn json_rpc_mcp_enforces_lifecycle_and_preserves_tool_results() {
    let executable =
        ExecutableApp::new(routes![create_user, health]).expect("application should be valid");
    let mut server = blazingly::mcp::JsonRpcServer::new(&executable);

    let too_early = poll_ready(server.handle_value(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    })))
    .expect("requests receive responses");
    assert_eq!(too_early["error"]["code"], -32_000);

    let initialized = poll_ready(server.handle_value(json!({
        "jsonrpc": "2.0",
        "id": "init-1",
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {
                "name": "conformance-test",
                "version": "1"
            }
        }
    })))
    .expect("initialize receives a response");
    assert_eq!(initialized["id"], "init-1");
    assert_eq!(initialized["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(
        initialized["result"]["capabilities"]["tools"]["listChanged"],
        false
    );

    let notification = poll_ready(server.handle_value(json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    })));
    assert!(notification.is_none());

    let listed = poll_ready(server.handle_value(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    })))
    .expect("tools/list receives a response");
    assert_eq!(listed["result"]["tools"][0]["name"], "create_user");

    let confirmation = poll_ready(server.handle_value(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "create_user",
            "arguments": {
                "name": "Ada",
                "email": "ada@example.com"
            }
        }
    })))
    .expect("tools/call receives a response");
    assert_eq!(confirmation["result"]["isError"], true);
    assert!(
        confirmation["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("confirmation_required"))
    );

    let success = poll_ready(server.handle_value(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "create_user",
            "arguments": {
                "name": "Ada",
                "email": "ada@example.com"
            },
            "_meta": {
                "dev.blazingly/confirmed": true
            }
        }
    })))
    .expect("tools/call receives a response");
    assert_eq!(success["result"]["structuredContent"]["id"], 1);
    assert_eq!(
        success["result"]["structuredContent"]["email"],
        "ada@example.com"
    );
}

#[test]
fn api_model_generates_validation_and_shared_schema_metadata() {
    let invalid = CreateUser {
        name: "A".to_owned(),
        email: "not-an-email".to_owned(),
    };

    let errors = invalid
        .validate()
        .expect_err("invalid model should report every field violation");
    let codes: Vec<_> = errors
        .violations()
        .iter()
        .map(|violation| violation.code.as_str())
        .collect();
    assert_eq!(codes, ["min_length", "email"]);

    let descriptor = CreateUser::model_descriptor();
    assert_eq!(descriptor.name, "CreateUser");
    assert_eq!(descriptor.fields.len(), 2);
    assert_eq!(descriptor.fields[0].name, "name");
    assert_eq!(descriptor.fields[1].name, "email");
}

fn text_content(result: &blazingly::mcp::CallToolResult) -> &str {
    let blazingly::mcp::ContentBlock::Text { text } = &result.content[0];
    text
}

async fn runtime_call_create_user(
    runtime: &blazingly::mcp::McpRuntime<'_>,
    input: blazingly_json::Value,
) -> blazingly::mcp::CallToolResult {
    runtime
        .call_tool(
            "create_user",
            input,
            blazingly::mcp::McpCallContext::confirmed(),
        )
        .await
        .expect("known MCP tool should execute")
}

fn assert_http_error(response: &Response, status: u16, code: &str) {
    assert_eq!(response.status(), status);
    let body = response
        .json::<blazingly_json::Value>()
        .expect("HTTP error should be JSON");
    assert_eq!(body["error"]["code"], code);
}

fn poll_ready<F: Future>(future: F) -> F::Output {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = pin!(future);

    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the test handler unexpectedly yielded"),
    }
}

struct YieldOnce {
    yielded: bool,
}

impl YieldOnce {
    const fn new() -> Self {
        Self { yielded: false }
    }
}

impl Future for YieldOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

fn poll_until_ready<F: Future>(future: F) -> F::Output {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = pin!(future);

    for _ in 0..16 {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
    }
    panic!("test future did not become ready")
}
