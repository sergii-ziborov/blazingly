# Blazingly

Blazingly is an operation-first Rust API framework prototype.

The first product is a FastAPI-style Rust framework: handler signatures and
Rust models define extraction, validation, typed responses, OpenAPI, and
generated documentation. The same operation model also defines native MCP
tools/resources and AI-oriented Markdown; MCP is not reconstructed from
OpenAPI.

Mesh and Cloudflare execution are future products outside the current
repository scope. See [the architecture boundary](docs/architecture.md).

The product target is explicit: a fast router alone is 2/10; FastAPI-style
ergonomics plus OpenAPI is 7/10; Blazingly reaches 8.5/10 only when native MCP
executes the same typed operations and produces correct agent-safe responses
and AI documentation.

## Current milestone

The first executable vertical slice now includes:

- `#[api_model]` schemas and native field validation;
- every standard HTTP method through `#[get]`, `#[head]`, `#[post]`,
  `#[put]`, `#[patch]`, `#[delete]`, `#[options]`, `#[trace]`, and
  `#[connect]`, plus universal
  `#[operation(method = ..., path = ..., id = ...)]`;
- runtime-neutral `Request`, `Response`, compiled `Router`, and in-memory
  `TestApp`;
- `Path<T>`, `Query<T>`, `Header<T>`, `Cookie<T>`, `Json<T>`, `Form<T>`,
  `Multipart<T>`, and `File<T>` with multiple handler arguments;
- `Accepted<T>`, `Created<T>`, `NoContent`, `Status<CODE, T>`, and validated
  response headers, including repeated `Set-Cookie`;
- `#[api_error]` stable domain errors with optional typed details and declared
  response headers;
- versioned canonical operation contracts, SHA-256 fingerprints, and semantic
  compatibility reports for inputs, nested models, dependencies, security,
  responses, MCP exposure, and agent policy;
- registered API-key, HTTP, OAuth2, OpenID Connect, and mutual-TLS security
  schemes plus operation-level scope requirements;
- Fastify-style nested `Plugin` scopes with downward-only provider inheritance
  and local overrides;
- compiled dependency injection with direct typed handler arguments or
  `Depends<T>`, `singleton`/`request`/`transient` lifetimes, build-time
  diagnostics, sync/async fallible providers, and sync/async reverse-order
  finalizers; typed factories can use `#[provider]`;
- inherited async plugin hooks compiled per operation: `on_request`,
  `pre_parse`, `pre_validate`, `pre_handler`, `pre_serialize`, reverse-order
  `on_error`/`on_response`, plus child-before-parent shutdown hooks;
- typed test provider overrides plus runtime-neutral cancellation and
  adapter-supplied timeout futures, with finalizers shielded after abort;
- explicit `routes![...]` registration and duplicate detection;
- a runtime-neutral, local async operation executor;
- deterministic OpenAPI 3.1/JSON Schema 2020-12 plus precompiled
  `/openapi.json` and Scalar/Swagger UI mounts;
- native MCP discovery and in-process tool invocation over the same executor;
- MCP `CallToolResult` responses with confirmation, output-exposure, validation,
  and typed-error handling;
- MCP JSON-RPC lifecycle, resources, prompts, redacted audit, stateful
  Streamable HTTP, and supervised newline-delimited stdio;
- generated API/AI Markdown bundles, canonical contract manifests, HTTP/MCP
  examples, a Rust client starter, and a Tokio-free project scaffold;
- runtime-neutral pull-based streaming responses with bounded TestApp
  collection, HTTP/1 chunked framing, and HTTP/2 DATA frames;
- an optional Compio-based native adapter with no Tokio: HTTP/1 keep-alive,
  pipelining, Content-Length and chunked bodies, configurable limits, rustls
  TLS, graceful shutdown, cached HTTP `Date`, and a balanced thread-per-core
  launcher;
- experimental HTTP/2 prior-knowledge/ALPN support behind `native-http2`,
  using the same compiled `HttpApp`.

The native adapter is isolated in `blazingly-native`; its dependency tree
contains no Tokio, Hyper, or Axum. The core and public handler model remain
socket- and runtime-neutral and impose no unconditional `Send + Sync` bounds.
Cloudflare will receive a separate adapter over the same operation graph; no
Compio, socket, TLS, or HTTP codec type crosses into contract/core/executor.

HTTP/2 is intentionally marked experimental because its pinned Sans-I/O codec
is currently a canary release. Streaming uploads, TLS certificate/reload
ergonomics, and production security verifier middleware remain follow-up work.
The current security surface validates and documents schemes/scopes;
applications still enforce credentials through typed dependencies or plugin
hooks.

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

#[api_error]
enum CreateUserError {
    #[status(409)]
    #[code("email_already_exists")]
    #[message("A user with this email already exists.")]
    EmailAlreadyExists,
}

#[post(
    "/users",
    id = "users.create",
    summary = "Create a user"
)]
#[mcp::tool(
    name = "create_user",
    risk = "write",
    confirmation = "required",
    expose_output = "full"
)]
async fn create_user(
    Json(_input): Json<CreateUser>,
) -> Result<Created<UserView>, CreateUserError> {
    todo!()
}

let app = ExecutableApp::new(routes![create_user])?;
let test_app = TestApp::new(&app);

let openapi = blazingly::openapi::to_value(app.definition());
let agent_docs = blazingly::docs::mcp_markdown(app.definition());
let bundle = blazingly::docs::bundle(
    app.definition(),
    &blazingly::docs::DocsBundleConfig::new("Users API"),
)?;

let http = HttpApp::new(app).with_openapi(
    blazingly::openapi::OpenApiConfig::default(),
);

// With `features = ["mcp-stdio"]` in Cargo.toml:
let app = ExecutableApp::new(routes![create_user])?;
let mut server = blazingly::mcp::JsonRpcServer::new(&app);
blazingly::mcp::stdio::serve_stdio(&mut server)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Method-specific attributes are aliases over the universal form:

```rust
#[operation(
    method = PUT,
    path = "/users/{id}",
    id = "users.replace",
    summary = "Replace a user"
)]
async fn replace_user(Path(id): Path<u64>, Json(input): Json<CreateUser>) -> Json<UserView> {
    todo!()
}
```

Tools marked `confirmation = "required"` are rejected unless the MCP host sends
`_meta["dev.blazingly/confirmed"] = true` after obtaining user confirmation.

Typed response composition stays in ordinary Rust:

```rust
async fn accepted() -> WithHeaders<Accepted<UserView>> {
    Accepted(user)
        .header("location", "/jobs/7")
        .header("x-request-id", "req-7")
}

#[api_error]
enum CreateError {
    #[status(429)]
    #[code("rate_limited")]
    #[header("retry-after", "30")]
    RateLimited(RateLimitDetails),
}
```

Invalid response headers, serialization failures, and other framework-internal
failures are redacted to a generic `500` over HTTP and a generic internal MCP
protocol error.

Dependencies remain ordinary typed Rust:

```rust
#[derive(Clone)]
struct UsersRepository;

#[provider(singleton)]
fn users_repository() -> UsersRepository {
    UsersRepository
}

#[get("/users/{id}", id = "users.read", summary = "Read a user")]
async fn read_user(
    Path(id): Path<u64>,
    users: UsersRepository,
) -> Json<UserView> {
    todo!()
}

let users = Plugin::new("users")
    .provide(users_repository::provider())
    .operation(read_user::executable());

let app = ExecutableApp::from_plugin(
    Plugin::new("app").plugin(users),
)?;
```

Provider graphs are compiled during `ExecutableApp` construction. Request
execution uses numeric slots, not a type-name registry or per-request hash map.
See [dependency injection and plugin scopes](docs/dependency-injection.md).

The first socket-level development baseline and the remaining acceptance gates
are recorded in [benchmark status](docs/benchmark-status.md).

## Repositories

- `blazingly-contract`: independent portable operation contracts, pinned here
  as a Git submodule until its first registry release;
- `blazingly-benchmarks`: external conformance and performance comparisons.

The framework workspace contains:

- `blazingly-core`: application model and HTTP bindings;
- `blazingly-di`: typed providers, lifetimes, finalizers, and compiled slots;
- `blazingly-executor`: shared handler decoding, validation, and execution;
- `blazingly-http`: runtime-neutral HTTP types, compiled routing, and `TestApp`;
- `blazingly-macros`: the Rust handler frontend;
- `blazingly-openapi`: an OpenAPI projection;
- `blazingly-mcp`: tools/resources/prompts, JSON-RPC, Streamable HTTP,
  sessions, and audit;
- `blazingly-mcp-stdio`: bounded supervised newline-delimited stdio transport;
- `blazingly-native`: Tokio-free Compio HTTP/1 and experimental HTTP/2 adapter;
- `blazingly-docs`: API/AI bundles, examples, client starter, and scaffold;
- `blazingly`: public facade and prelude.

## Facade features

The facade enables `docs`, `mcp`, and `openapi` by default. Native socket code
remains opt-in:

- `native`: Tokio-free HTTP/1 server;
- `native-http2`: native plus experimental HTTP/2;
- `native-tls`: native plus rustls;
- `mcp-stdio`: MCP plus the supervised stdio transport.

`cargo check -p blazingly --no-default-features` verifies the minimal
contract/core/DI/executor/HTTP/macros facade.
