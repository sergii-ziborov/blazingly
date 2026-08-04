# Blazingly

Blazingly is an operation-first Rust API framework prototype.

The first product is a FastAPI-style Rust framework: handler signatures and
Rust models define extraction, validation, typed responses, OpenAPI, and
generated documentation. The same operation model also defines native MCP
tools/resources and AI-oriented Markdown; MCP is not reconstructed from
OpenAPI.

Mesh and Cloudflare execution are future products outside the current
repository scope. See [the architecture boundary](docs/architecture.md).
Competitive API gaps, platform experiments, and the evidence required before
making performance or efficiency claims are tracked in the
[competitive and platform roadmap](docs/competitive-roadmap.md).

The product target is explicit: a fast router alone is 2/10; FastAPI-style
ergonomics plus OpenAPI is 7/10; Blazingly reaches 8.5/10 only when native MCP
executes the same typed operations and produces correct agent-safe responses
and AI documentation.

**Building an API with Blazingly?** Start at
[getting started](docs/getting-started.md): install, a first application, a
validated model, a typed error, dependency injection, running it, the OpenAPI
document, and MCP. The rest of `docs/` is written for people working on the
framework.

## Publication status

**Published.** Blazingly `0.2.0` is on crates.io; the release gate walked
before every tag is in [stability and SemVer](docs/stability.md).

```console
cargo add blazingly
cargo install cargo-blazingly
```

The workspace MSRV is Rust 1.88. The three submodule crates
(`blazingly-contract`, `blazingly-wire`, `blazingly-json`) publish from their
own repositories. To track unreleased work on `main`, use a Git dependency
instead: `blazingly = { git = "https://github.com/sergii-ziborov/blazingly" }`
— Cargo checks the submodules out itself. Release history is in
[CHANGELOG.md](CHANGELOG.md).

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
- pull-based `UploadBody` request streaming with bounded native backpressure
  and contract/OpenAPI projection, plus `UploadBody::into_multipart` for
  reading a `multipart/form-data` body field by field and chunk by chunk
  without holding the upload;
- aliases, custom validators, rich nested error locations, and typed
  UUID/URL/IP/date/date-time/decimal validation;
- `Accepted<T>`, `Created<T>`, `NoContent`, `Status<CODE, T>`, and validated
  response headers, including repeated `Set-Cookie`;
- `#[api_error]` stable domain errors with optional typed details and declared
  response headers;
- versioned canonical operation contracts, SHA-256 fingerprints, and semantic
  compatibility reports for inputs, nested models, dependencies, security,
  responses, MCP exposure, and agent policy;
- registered API-key, HTTP, OAuth2, OpenID Connect, and mutual-TLS security
  schemes plus operation-level scope requirements, enforced by named runtime
  verifiers;
- runtime-neutral CORS, GZip/Brotli compression, trusted-host, trusted-proxy,
  and bounded global/per-client rate-limit middleware;
- ready HS256 JWT, OAuth2 bearer-scope, signed session-cookie, and constant-time
  API-key verification, with typed `Extension<SecurityContext>` handler access;
- Fastify-style nested `Plugin` scopes with downward-only provider inheritance
  and local overrides, plus `Plugin::mount("/v1")` and
  `with_id_namespace("v1")` so one module serves under two prefixes with
  distinct operation identities and MCP tool names;
- compiled dependency injection with direct typed handler arguments or
  `Depends<T>`, `singleton`/`request`/`transient` lifetimes, build-time
  diagnostics, sync/async fallible providers, and sync/async reverse-order
  finalizers; typed factories can use `#[provider]`;
- request-aware providers: a `#[provider]` also takes `Path`/`Query`/
  `Header`/`Cookie` inputs beside `Depends<T>`, and each input folds into the
  consuming operation's contract exactly once — deduplicated across
  providers, validated with the same `422` envelope as a handler input, and
  visible to OpenAPI, MCP, documentation, and fingerprints; test overrides
  bypass a replaced provider's inputs;
- explicit custom extraction with `Extract<T>` over public `FromInvocation`,
  and `Extract<RequestParts>` for an owned snapshot of method, path,
  effective scheme and host, and peer address, with deterministic
  `transport_mismatch` rejection off HTTP;
- inherited async plugin hooks compiled per operation: `on_request`,
  `pre_parse`, `pre_validate`, `pre_handler`, `pre_serialize`, reverse-order
  `on_error`/`on_response`, plus child-before-parent shutdown hooks;
- typed test provider overrides plus runtime-neutral cancellation and
  adapter-supplied timeout futures, with finalizers shielded after abort;
- startup/shutdown lifespan hooks, after-response background tasks, and a
  bounded pool for synchronous handlers and blocking database/ORM work;
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
- generated container/Kubernetes deployment with a shared HPA and selectable
  maintained-NGINX or direct `LoadBalancer` exposure;
- runtime-neutral pull-based streaming responses with bounded TestApp
  collection, HTTP/1 chunked framing, and HTTP/2 DATA frames;
- SSE plus native HTTP/1 WebSocket upgrades over plaintext and TLS;
- request IDs, W3C trace context, structured access events, `tracing`,
  optional OpenTelemetry parent propagation, and Prometheus request/error/
  latency metrics;
- `cargo blazingly new/dev/run/build/check/openapi/routes/discover/doctor`,
  application discovery, `Blazingly.toml`, and polling autoreload;
- optional database/ORM pool contracts, queue contracts with an in-memory
  conformance adapter, compiled MiniJinja templates, and concrete JWT/OAuth2/
  API-key/signed-session auth providers;
- an optional Compio-based native adapter with no Tokio: HTTP/1 keep-alive,
  pipelining, Content-Length and chunked bodies, configurable limits, rustls
  TLS, graceful shutdown, cached HTTP `Date`, bounded pipelined-response
  coalescing, and a thread-per-core launcher that places each connection on
  the least-loaded worker, with opt-in elevated worker scheduling priority
  (measured minus 26% p99.9 on a contended host) and built-in per-stage tail
  histograms behind `BLAZINGLY_NATIVE_STAGE_METRICS=1`;
- experimental HTTP/2 prior-knowledge/ALPN support behind `native-http2`,
  using the same compiled `HttpApp`.

HTTP/1 parsing and framing are isolated in
[`blazingly-wire`](https://github.com/sergii-ziborov/blazingly-wire), a separate
repository that contains no framework or socket-runtime dependencies and enters
this workspace as a submodule. It is consumed both by `blazingly-native` and by
a standard-library, thread-per-connection example server that uses no async at
all.
`blazingly-native` contains the Compio adapter and no Tokio, Hyper, or Axum.
That is not a claim you have to take on trust: `deny.toml` bans those crates at
any depth and CI enforces it, so a pull request that would introduce one fails.
The core and public handler model remain socket- and runtime-neutral and impose
no unconditional `Send + Sync` bounds. Cloudflare will receive a separate
adapter over the same operation graph; no Compio, socket, TLS, or native HTTP
codec type crosses into contract/core/executor.

HTTP/2 sits outside the release contour: it is off by default behind
`native-http2`, its pinned Sans-I/O codec is an upstream canary release, and no
release gate mentions it. A supported HTTP/2 will live in a separate
`blazingly-http2` repository; the reasoning is in
[stability and SemVer](docs/stability.md). Request bodies reach the streaming
boundary on the plaintext socket, on the generic compatibility transport TLS
runs over, and on HTTP/2, for operations that declare a stream input; every
other operation is handed a buffered body. TLS certificate/reload ergonomics,
asymmetric JWT/JWKS discovery, key rotation, and CSRF helpers remain follow-up
work. Contract security is enforced before body parsing by the same middleware
pipeline in `TestApp`, native HTTP/1, and HTTP/2.

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

Security and HTTP policy are attached without a socket-runtime dependency:

```rust
let jwt = JwtHs256::new(b"replace-with-at-least-32-secret-bytes")?;
let http = HttpApp::new(app)
    .with_middleware(ProxyHeaders::new().trust("10.0.0.0/8".parse()?))
    .with_middleware(TrustedHost::new(["api.example.com"]))
    .with_middleware(RateLimit::per_client(1_000, std::time::Duration::from_secs(1)))
    .with_middleware(Compression::new())
    .with_middleware(
        Security::new().verifier("oauth", OAuth2Bearer::new(jwt)),
    );
# Ok::<(), Box<dyn std::error::Error>>(())
```

The same layers attach to the native server. A single-threaded server takes
them directly; the multicore launcher takes a factory, because middleware is
thread-local and each worker builds its own:

```rust,ignore
blazingly::native::Server::new(app)
    .with_middleware(Cors::permissive())
    .with_middleware(Security::new().verifier("oauth", OAuth2Bearer::new(jwt)))
    .serve(("0.0.0.0", 8080))?;

blazingly::native::MulticoreServer::new(workers, build_app)
    .with_middleware_factory(|| vec![std::rc::Rc::new(Cors::permissive()) as _])
    .serve(("0.0.0.0", 8080))?;
```

An operation that declares a security scheme fails closed: if no registered
layer can verify it, the request is rejected rather than served unauthenticated.

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

Route handlers may be synchronous or asynchronous. A synchronous handler uses
the allocation-free direct executor path when the operation has no lifecycle
hooks; the macro also generates an asynchronous fallback so plugin hooks,
cancellation, timeouts, and finalizers keep identical semantics.

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

The generated starter has two Kubernetes exposure modes over the same native
server and autoscaled pod set. See [deployment modes](docs/deployment.md).

The first socket-level development baseline and the remaining acceptance gates
are recorded in [benchmark status](docs/benchmark-status.md). In the latest
matched validated-operation result, Blazingly was ahead of Axum and behind
Actix Web on one busy Windows loopback host; this is not a framework-wide speed
ranking. Energy efficiency is likewise a target, not a current result:
requests-per-watt or server-replacement claims require matched hardware,
whole-system power, latency, error-rate, and sustained-load evidence defined in
the [competitive roadmap](docs/competitive-roadmap.md).

## Repositories

Three crates are developed in their own repositories and enter this workspace as
submodules under `crates/`:

- `blazingly-contract`: independent portable operation contracts, pinned to its
  `v0.4.0` tag and published as `blazingly-contract 0.4.0`;
- `blazingly-wire`: framework- and runtime-independent HTTP/1 parsing and
  response framing, pinned to its `v0.1.2` tag and published as
  `blazingly-wire 0.1.2`;
- `blazingly-json`: the JSON engine every crate here encodes and decodes with,
  pinned to its `v0.1.1` tag and published as `blazingly-json 0.1.1`.

Four more repositories are external and not submodules. The three adapters
implement the framework's database and queue seams against real backends and
are deliberately kept out of the workspace so vendor-specific code never
enters the framework tree:

- `blazingly-sqlite`: SQLite adapter over `rusqlite` — separate read and
  write connection lanes (one writer, many WAL readers), read-heavy pragma
  tuning, prepared-statement caching, dirty reads via `read_uncommitted` on
  shared-cache pools, migrations with drift detection;
- `blazingly-postgres`: PostgreSQL adapter with the frontend/backend
  protocol version 3 implemented directly over `std::net::TcpStream` —
  SCRAM-SHA-256, binary parameter binding, all four isolation levels,
  SQLSTATE error classification, advisory-locked migrations; no
  `postgres`/`tokio-postgres` dependency and no async runtime in its tree;
- `blazingly-redis`: Redis backends for three distributed seams with RESP
  implemented directly — the Streams queue (consumer groups, at-least-once
  delivery with redelivery of dead consumers' work, dead-lettering with a
  bounded attempt count), a rate-limit store whose check-and-consume is one
  Lua script so two pods cannot both pass on the last token, and a session
  store with server-enforced expiry;
- `blazingly-nats`: NATS JetStream adapter for the queue seam with the core
  protocol and a JetStream JSON layer implemented directly — durable pull
  consumers, the server's own delivery count as the attempt number,
  nack-with-delay, publish dedup via `Nats-Msg-Id`;
- `blazingly-examples`: a gallery of six complete runnable applications,
  from a 15-minute CRUD to MCP tools over stdio and Streamable HTTP;
- `blazingly-benchmarks`: external conformance and performance comparisons.

The framework workspace contains those three submodule crates plus:

- `blazingly-core`: application model and HTTP bindings;
- `blazingly-database`: bounded blocking pool integration for synchronous DB
  and ORM connection pools;
- `blazingly-deploy`: Docker/Kubernetes/HPA deployment artifact generation;
- `blazingly-di`: typed providers, lifetimes, finalizers, and compiled slots;
- `blazingly-executor`: shared handler decoding, validation, and execution;
- `blazingly-http`: runtime-neutral HTTP types, compiled routing, and `TestApp`;
- `blazingly-middleware`: CORS, compression, proxy/host policy, and rate limits;
- `blazingly-security`: API-key, bearer/JWT/OAuth2, signed-session enforcement,
  and typed identities;
- `blazingly-macros`: the Rust handler frontend;
- `blazingly-openapi`: an OpenAPI projection;
- `blazingly-observability`: access logging, request/trace IDs, OpenTelemetry,
  and Prometheus metrics;
- `blazingly-queue`: runtime-neutral queue contracts and test adapter;
- `blazingly-realtime`: SSE and WebSocket response models;
- `blazingly-templates`: compiled MiniJinja HTML responses;
- `blazingly-validation`: advanced reusable validation types;
- `blazingly-mcp`: tools/resources/prompts, JSON-RPC, Streamable HTTP,
  sessions, and audit;
- `blazingly-mcp-stdio`: bounded supervised newline-delimited stdio transport;
- `blazingly-native`: Tokio-free Compio HTTP/1 adapter, plus an HTTP/2 adapter
  kept outside the release contour (see [stability](docs/stability.md));
- `cargo-blazingly`: application discovery, autoreload, diagnostics, and
  production build/run CLI;
- `blazingly-docs`: API/AI bundles, examples, client starter, and project
  scaffold, composing `blazingly-deploy` for deployment files;
- `blazingly`: public facade and prelude.

## Facade features

The facade enables `deploy`, `docs`, `mcp`, `middleware`, `observability`,
`openapi`, `realtime`, `security`, and `validation` by default. Native socket
and ecosystem integrations remain opt-in:

- `deploy`: Docker/Kubernetes/HPA deployment generation;
- `native`: Tokio-free HTTP/1 server;
- `native-http2`: native plus experimental HTTP/2;
- `native-tls`: native plus rustls;
- `mcp-stdio`: MCP plus the supervised stdio transport;
- `middleware`: runtime-neutral HTTP policy and compression;
- `observability` / `observability-otel`: metrics/tracing with optional
  OpenTelemetry propagation;
- `security`: runtime-neutral credential verification and authorization;
- `database`, `queue`, `templates`: optional ecosystem integration crates.

`cargo check -p blazingly --no-default-features` verifies the minimal
contract/core/DI/executor/HTTP/macros facade.

Security reporting, fuzz/Miri/sanitizer coverage, and the pre-1.0 compatibility
policy are documented in [SECURITY.md](SECURITY.md) and
[stability and SemVer](docs/stability.md).
See also [getting started](docs/getting-started.md), the
[developer CLI workflow](docs/developer-workflow.md), and the
[ecosystem integration boundary](docs/ecosystem.md).

## License

Licensed under the MIT License. See [LICENSE](LICENSE).
