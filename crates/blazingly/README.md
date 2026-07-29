# Blazingly

Blazingly is an operation-first Rust API framework: handler signatures and
Rust models define extraction, validation, typed responses, OpenAPI, and
generated documentation — and the same operation model natively defines MCP
tools, resources, and prompts, not a reconstruction from OpenAPI.

This crate is the facade. `cargo add blazingly` re-exports the framework
crates under one name and one `prelude`; each underlying crate
(`blazingly-core`, `blazingly-openapi`, `blazingly-mcp`, and the rest) is an
ordinary library usable on its own, and the facade adds the curated surface,
the feature wiring, and the macros. MSRV is Rust 1.88. `tokio`, `hyper`, and
`axum` are banned from the dependency graph at any depth and CI enforces it.

## What you get

- FastAPI-style handlers: `#[get]`/`#[post]`/... on plain functions,
  `#[api_model]` validated models, `#[api_error]` stable typed errors;
- compiled dependency injection (`#[provider]`, `Depends<T>`) and nested
  `Plugin` scopes with lifecycle hooks;
- runtime-neutral `Request`, `Response`, compiled `Router`, and an in-memory
  `TestApp`;
- deterministic OpenAPI 3.1 / JSON Schema 2020-12 with precompiled
  `/openapi.json` and Scalar/Swagger UI mounts;
- native MCP: the same typed operations served over JSON-RPC, Streamable
  HTTP, and supervised stdio, with confirmation and output-exposure policy;
- generated API/AI Markdown bundles, project scaffolds, deployment files,
  and versioned operation contracts with compatibility reports;
- middleware (CORS, compression, rate limits, trusted host/proxy), security
  verifiers (JWT, OAuth2 bearer, API key, signed sessions), and
  observability (request IDs, W3C trace context, `tracing`, Prometheus);
- an opt-in Tokio-free Compio native HTTP/1 server with rustls TLS, SSE, and
  WebSocket upgrades.

## Features

`deploy`, `docs`, `mcp`, `middleware`, `observability`, `openapi`,
`realtime`, `security`, and `validation` are enabled by default. Opt-in:
`native`, `native-tls`, `native-http2` (experimental), `mcp-stdio`,
`database`, `queue`, `templates`, and `observability-otel`.
`cargo check -p blazingly --no-default-features` verifies the minimal
contract/core/DI/executor/HTTP/macros surface.

## Example

```rust
use blazingly::prelude::*;

#[api_model]
struct CreateUser {
    #[email]
    email: String,
}

#[api_model]
struct UserView {
    id: u64,
    email: String,
}

#[post("/users", id = "users.create", summary = "Create a user")]
#[mcp::tool(name = "create_user", risk = "write")]
async fn create_user(Json(input): Json<CreateUser>) -> Created<UserView> {
    Created(UserView {
        id: 1,
        email: input.email,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = ExecutableApp::new(routes![create_user])?;

    // The same typed operation, projected without a server:
    let openapi = blazingly::openapi::to_value(app.definition());
    let agent_docs = blazingly::docs::mcp_markdown(app.definition());
    println!("{openapi}\n{agent_docs}");
    Ok(())
}
```

`TestApp` exercises the same application entirely in memory; the opt-in
`native` feature serves it over the Tokio-free HTTP/1 socket server.

## Links

- [Getting started](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md)
  — install, a first application, validation, DI, OpenAPI, and MCP
- [API documentation](https://docs.rs/blazingly)
- [Repository](https://github.com/sergii-ziborov/blazingly) — the
  framework-internal documentation lives in `docs/`
