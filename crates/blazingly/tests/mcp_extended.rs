use blazingly::mcp::{
    AuditOutcome, BoundedAuditLog, JsonRpcServer, McpHttpMethod, McpPrompt, McpRegistry,
    McpResource, PromptArgument, PromptDescriptor, ResourceDescriptor, StreamableHttpConfig,
    StreamableHttpRequest, StreamableHttpServer,
};
use blazingly::prelude::*;
use blazingly_json::{Value, json};
use futures_lite::future;

#[test]
fn resources_prompts_and_redacted_audit_share_one_json_rpc_lifecycle() {
    let app = ExecutableApp::new(Vec::new()).expect("empty app should compile");
    let registry = registry();
    let audit = BoundedAuditLog::new(16);
    let mut server = JsonRpcServer::new(&app)
        .with_registry(registry)
        .with_audit_sink(audit.clone());

    initialize(&mut server);

    let resources = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":2,"method":"resources/list"}),
    );
    assert_eq!(
        resources["result"]["resources"][0]["uri"],
        "docs://blazingly/guide"
    );
    let resource = request(
        &mut server,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"resources/read",
            "params":{"uri":"docs://blazingly/guide"}
        }),
    );
    assert_eq!(
        resource["result"]["contents"][0]["text"],
        "Do not log secret-resource-body."
    );

    let prompts = request(
        &mut server,
        json!({"jsonrpc":"2.0","id":4,"method":"prompts/list"}),
    );
    assert_eq!(prompts["result"]["prompts"][0]["name"], "review_api");
    let prompt = request(
        &mut server,
        json!({
            "jsonrpc":"2.0",
            "id":5,
            "method":"prompts/get",
            "params":{
                "name":"review_api",
                "arguments":{"operation":"secret-operation-argument"}
            }
        }),
    );
    assert_eq!(
        prompt["result"]["messages"][0]["content"]["text"],
        "Review secret-operation-argument for contract safety."
    );

    let events = audit.events();
    assert!(events.iter().any(|event| {
        event.method == "resources/read"
            && event.subject.as_deref() == Some("docs://blazingly/guide")
            && event.outcome == AuditOutcome::Success
    }));
    assert!(events.iter().any(|event| {
        event.method == "prompts/get" && event.subject.as_deref() == Some("review_api")
    }));
    let audit_debug = format!("{events:?}");
    assert!(!audit_debug.contains("secret-resource-body"));
    assert!(!audit_debug.contains("secret-operation-argument"));
}

#[test]
fn streamable_http_enforces_sessions_versions_origins_and_delete() {
    let app = ExecutableApp::new(Vec::new()).expect("empty app should compile");
    let audit = BoundedAuditLog::new(16);
    let mut sequence = 0_u64;
    let mut server = StreamableHttpServer::new(&app)
        .with_registry(registry())
        .with_config(StreamableHttpConfig::new().allow_origin("https://agent.example"))
        .with_audit_sink(audit.clone())
        .with_session_id_factory(move || {
            sequence += 1;
            format!("test-session-{sequence}")
        });

    let forbidden = future::block_on(server.handle(
        StreamableHttpRequest::post(initialization()).header("origin", "https://attacker.example"),
    ));
    assert_eq!(forbidden.status(), 403);

    let initialized =
        future::block_on(server.handle(StreamableHttpRequest::post(initialization())));
    assert_eq!(initialized.status(), 200);
    assert_eq!(
        initialized.json().expect("initialize is JSON")["result"]["protocolVersion"],
        blazingly::mcp::PROTOCOL_VERSION
    );
    let session_id = initialized
        .get_header("mcp-session-id")
        .expect("session header")
        .to_owned();
    assert_eq!(server.active_sessions(), 1);

    let notification = future::block_on(
        server.handle(
            StreamableHttpRequest::post(
                br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            )
            .header("mcp-session-id", &session_id)
            .header("mcp-protocol-version", blazingly::mcp::PROTOCOL_VERSION),
        ),
    );
    assert_eq!(notification.status(), 202);
    assert!(notification.body().is_empty());

    let resources = future::block_on(
        server.handle(
            StreamableHttpRequest::post(br#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#)
                .header("mcp-session-id", &session_id)
                .header("mcp-protocol-version", blazingly::mcp::PROTOCOL_VERSION),
        ),
    );
    assert_eq!(resources.status(), 200);
    assert_eq!(
        resources.json().expect("resources are JSON")["result"]["resources"][0]["name"],
        "Blazingly guide"
    );

    let invalid_version = future::block_on(
        server.handle(
            StreamableHttpRequest::post(br#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#)
                .header("mcp-session-id", &session_id)
                .header("mcp-protocol-version", "unsupported"),
        ),
    );
    assert_eq!(invalid_version.status(), 400);

    let deleted = future::block_on(server.handle(
        StreamableHttpRequest::new(McpHttpMethod::Delete).header("mcp-session-id", &session_id),
    ));
    assert_eq!(deleted.status(), 204);
    assert_eq!(server.active_sessions(), 0);

    let stale = future::block_on(
        server.handle(
            StreamableHttpRequest::post(br#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#)
                .header("mcp-session-id", &session_id),
        ),
    );
    assert_eq!(stale.status(), 404);
    assert!(audit.events().iter().any(|event| {
        event.method == "resources/list" && event.session_id.as_deref() == Some(session_id.as_str())
    }));
}

fn registry() -> McpRegistry {
    let mut registry = McpRegistry::new();
    registry
        .register_resource(McpResource::text(
            ResourceDescriptor::new("docs://blazingly/guide", "Blazingly guide")
                .with_description("Agent-oriented framework guidance")
                .with_mime_type("text/markdown"),
            "Do not log secret-resource-body.",
        ))
        .expect("resource should register");
    registry
        .register_prompt(McpPrompt::template(
            PromptDescriptor::new("review_api")
                .with_description("Review an operation contract")
                .argument(PromptArgument::required("operation")),
            "Review {{operation}} for contract safety.",
        ))
        .expect("prompt should register");
    registry
}

fn initialize(server: &mut JsonRpcServer<'_>) {
    let response = request(server, initialization_value());
    assert_eq!(
        response["result"]["protocolVersion"],
        blazingly::mcp::PROTOCOL_VERSION
    );
    let notification = future::block_on(server.handle_value(json!({
        "jsonrpc":"2.0",
        "method":"notifications/initialized"
    })));
    assert!(notification.is_none());
}

fn request(server: &mut JsonRpcServer<'_>, message: Value) -> Value {
    future::block_on(server.handle_value(message)).expect("request should return a response")
}

fn initialization() -> Vec<u8> {
    blazingly_json::to_vec(&initialization_value()).expect("fixture should serialize")
}

fn initialization_value() -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":1,
        "method":"initialize",
        "params":{
            "protocolVersion":blazingly::mcp::PROTOCOL_VERSION,
            "capabilities":{},
            "clientInfo":{"name":"test","version":"1"}
        }
    })
}
