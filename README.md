# Blazingly

[![crates.io](https://img.shields.io/crates/v/blazingly.svg)](https://crates.io/crates/blazingly)
[![docs.rs](https://docs.rs/blazingly/badge.svg)](https://docs.rs/blazingly)
[![CI](https://github.com/sergii-ziborov/blazingly/actions/workflows/ci.yml/badge.svg)](https://github.com/sergii-ziborov/blazingly/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue.svg)](https://blog.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**An operation-first Rust API framework.** A handler signature and its Rust
types are the single source of truth: extraction, validation, typed responses,
OpenAPI, generated documentation, and native MCP tools all come from the same
declaration.

```console
cargo add blazingly --features native
```

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
#[mcp::tool(name = "create_user", risk = "write", confirmation = "required")]
async fn create_user(Json(input): Json<CreateUser>) -> Created<UserView> {
    Created(UserView { id: 1, email: input.email })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = ExecutableApp::new(routes![create_user])?;

    // One declaration, four consumers.
    let _openapi = blazingly::openapi::to_value(app.definition());
    let _agent_docs = blazingly::docs::mcp_markdown(app.definition());
    let _mcp = blazingly::mcp::JsonRpcServer::new(&app);
    let _http = HttpApp::new(app);

    Ok(())
}
```

That one operation is now an HTTP endpoint with a validated body, an entry in
a deterministic OpenAPI 3.1 document, an MCP tool a model can call in-process,
and a page of generated Markdown — none of it reconstructed from the others.

## Why not axum

Three reasons, and each is a trade-off rather than a free win.

**The contract is real.** An operation compiles down to a canonical, versioned
contract with a SHA-256 fingerprint. `cargo blazingly` can diff two revisions
and tell you which change is breaking and for whom — inputs, nested models,
security, responses, MCP exposure. That is a thing the framework knows, not a
convention you maintain.

**MCP is native, not generated.** An agent calling `create_user` goes through
the same executor, the same validation, and the same typed errors as an HTTP
client. Frameworks that emit MCP from an OpenAPI document inherit every lossy
step of that translation; here there is no translation.

**No Tokio, at any depth.** `deny.toml` bans Tokio, Hyper, and Axum, and CI
enforces it, so a pull request that pulls one in fails. The core and the public
handler model are socket- and runtime-neutral and impose no unconditional
`Send + Sync` bounds. The shipped server is a Compio adapter; a Cloudflare
adapter will sit over the same operation graph.

The cost side, stated plainly: a far smaller ecosystem than axum's, no 1.0
stability promise yet, and a performance story that is competitive but not
finished — see [Performance](#performance).

**Building something?** Start at [getting started](docs/getting-started.md):
install, a first application, a validated model, a typed error, dependency
injection, running it, the OpenAPI document, and MCP. The rest of `docs/` is
written for people working on the framework itself.

## Status

Blazingly `0.2.2` is on crates.io. It is pre-1.0: suitable for evaluation and
controlled production trials, not a promise that every public Rust API is
frozen. What that does and does not guarantee is in
[stability and SemVer](docs/stability.md); the release history is in
[CHANGELOG.md](CHANGELOG.md).

```console
cargo add blazingly --features native
cargo install cargo-blazingly
```

The workspace MSRV is Rust 1.88. To track unreleased work on `main`, use a Git
dependency instead — Cargo checks the submodules out itself:

```toml
blazingly = { git = "https://github.com/sergii-ziborov/blazingly" }
```

## What it does

### HTTP

- every standard method through `#[get]`, `#[head]`, `#[post]`, `#[put]`,
  `#[patch]`, `#[delete]`, `#[options]`, `#[trace]`, `#[connect]`, plus the
  universal `#[operation(method = ..., path = ..., id = ...)]`;
- `Path<T>`, `Query<T>`, `Header<T>`, `Cookie<T>`, `Json<T>`, `Form<T>`,
  `Multipart<T>`, and `File<T>`, in any combination of handler arguments;
- `Extract<T>` over the public `FromInvocation` trait for custom extraction,
  and `Extract<RequestParts>` for an owned snapshot of method, path, effective
  scheme and host, and peer address;
- `Accepted<T>`, `Created<T>`, `NoContent`, `Status<CODE, T>`, and validated
  response headers including repeated `Set-Cookie`;
- pull-based `UploadBody` request streaming with bounded native backpressure,
  and `UploadBody::into_multipart` for reading a `multipart/form-data` body
  field by field and chunk by chunk without holding the upload;
- pull-based streaming responses, HTTP/1 chunked framing, SSE, and native
  WebSocket upgrades over plaintext and TLS;
- runtime-neutral `Request`, `Response`, a compiled `Router`, and an in-memory
  `TestApp` that needs no socket.

### Validation and errors

- `#[api_model]` schemas with native field validation, aliases, custom
  validators, and rich nested error locations;
- typed UUID / URL / IP / date / date-time / decimal validation;
- value-type constraints that survive nesting: a `#[min_length]` on a newtype
  is projected into the items schema of a `Vec<T>` that uses it, at any depth;
- `#[api_error]` stable domain errors with optional typed details and declared
  response headers. Invalid response headers, serialization failures, and other
  framework-internal faults are redacted to a generic `500` over HTTP and a
  generic internal error over MCP.

### Composition and dependencies

- compiled dependency injection with direct typed handler arguments or
  `Depends<T>`, `singleton` / `request` / `transient` lifetimes, build-time
  diagnostics, sync and async fallible providers, and reverse-order finalizers.
  Provider graphs are compiled during `ExecutableApp` construction; request
  execution uses numeric slots, not a type-name registry or a per-request hash
  map. See [dependency injection and plugin scopes](docs/dependency-injection.md);
- request-aware providers: a `#[provider]` also takes `Path` / `Query` /
  `Header` / `Cookie` inputs beside `Depends<T>`, and each input folds into the
  consuming operation's contract exactly once — deduplicated across providers,
  validated with the same `422` envelope as a handler input, and visible to
  OpenAPI, MCP, documentation, and fingerprints;
- Fastify-style nested `Plugin` scopes with downward-only provider inheritance
  and local overrides, plus `Plugin::mount("/v1")` and `with_id_namespace("v1")`
  so one module serves under two prefixes with distinct operation identities and
  MCP tool names;
- inherited async plugin hooks compiled per operation — `on_request`,
  `pre_parse`, `pre_validate`, `pre_handler`, `pre_serialize`, reverse-order
  `on_error` / `on_response`, and child-before-parent shutdown hooks;
- typed test provider overrides, runtime-neutral cancellation, adapter-supplied
  timeout futures with finalizers shielded after abort, startup/shutdown
  lifespan hooks, and after-response background tasks.

### Contracts, OpenAPI, and MCP

- versioned canonical operation contracts, SHA-256 fingerprints, and semantic
  compatibility reports across inputs, nested models, dependencies, security,
  responses, MCP exposure, and agent policy;
- deterministic OpenAPI 3.1 / JSON Schema 2020-12, a precompiled
  `/openapi.json`, and Scalar or Swagger UI mounts;
- native MCP discovery and in-process tool invocation over the same executor,
  `CallToolResult` responses with confirmation, output-exposure, validation and
  typed-error handling, the JSON-RPC lifecycle, resources, prompts, a redacted
  audit log, stateful Streamable HTTP, supervised newline-delimited stdio, and a
  read-only `FrameworkManifest` control-plane resource;
- generated API and AI Markdown bundles, canonical contract manifests, HTTP and
  MCP examples, a Rust client starter, and a Tokio-free project scaffold.

Tools marked `confirmation = "required"` are rejected unless the MCP host sends
`_meta["dev.blazingly/confirmed"] = true` after obtaining user confirmation.

### Security and policy

- registered API-key, HTTP, OAuth2, OpenID Connect, and mutual-TLS schemes with
  operation-level scope requirements, enforced by named runtime verifiers.
  An operation that declares a scheme **fails closed**: if no registered layer
  can verify it, the request is rejected rather than served unauthenticated;
- ready HS256 JWT, OAuth2 bearer-scope, signed session-cookie, and
  constant-time API-key verification, with typed `Extension<SecurityContext>`
  handler access;
- runtime-neutral CORS, GZip/Brotli compression, trusted-host, trusted-proxy,
  and bounded global or per-client rate limiting. Contract security is enforced
  before body parsing by the same middleware pipeline in `TestApp`, in native
  HTTP/1, and in HTTP/2.

### Running it

- an optional Compio-based native adapter with no Tokio: HTTP/1 keep-alive,
  pipelining, Content-Length and chunked bodies, configurable limits, rustls
  TLS, graceful shutdown, a cached HTTP `Date`, bounded pipelined-response
  coalescing, and a thread-per-core launcher that places each connection on the
  least-loaded worker;
- opt-in elevated worker scheduling priority, which shortens the
  completion-to-wake gap on a contended host. Measured on a briefly quiet
  Windows host, in medians of three interleaved rounds: p99 −11%, p99.9 −26%;
- per-stage tail histograms behind `BLAZINGLY_NATIVE_STAGE_METRICS=1`, which
  split each keep-alive cycle at the socket write;
- request IDs, W3C trace context, structured access events, `tracing`, optional
  OpenTelemetry parent propagation, and Prometheus request/error/latency metrics;
- `cargo blazingly new / dev / run / build / check / openapi / routes /
  discover / doctor`, application discovery, `Blazingly.toml`, and polling
  autoreload;
- generated container and Kubernetes deployment with a shared HPA and a choice
  of maintained-NGINX or direct `LoadBalancer` exposure. See
  [deployment modes](docs/deployment.md);
- optional database/ORM pool contracts, queue contracts with an in-memory
  conformance adapter, compiled MiniJinja templates, and concrete
  JWT/OAuth2/API-key/signed-session auth providers.

## Things worth knowing before you build on it

**A synchronous handler runs inline on the worker that accepted the request.**
It is never moved to the blocking pool. Work that actually blocks — a file
read, a synchronous driver, a long CPU loop — must call `run_blocking` to reach
the bounded pool, or it stalls one thread-per-core worker and every connection
placed on it. A sync handler with no lifecycle hooks does use an
allocation-free direct executor path; the macro also generates an async
fallback so hooks, cancellation, timeouts, and finalizers keep identical
semantics.

**HTTP/2 sits outside the release contour.** It is off by default behind
`native-http2`, its pinned Sans-I/O codec is an upstream canary release, and no
release gate mentions it. A supported HTTP/2 will live in a separate
`blazingly-http2` repository; the reasoning is in
[stability and SemVer](docs/stability.md).

**A `#[provider]` cannot take `Extension<T>`,** so a dependency cannot yet see
the authenticated identity; read it in the handler instead.

**Still missing:** TLS certificate reload ergonomics, asymmetric JWT with
JWKS/OIDC discovery, key rotation, and CSRF helpers. The OTLP exporter speaks
plaintext transport only. There has been no external security audit.

## Performance

The first socket-level development baseline and the remaining acceptance gates
are in [benchmark status](docs/benchmark-status.md). In the latest matched
validated-operation result, Blazingly was ahead of Axum and behind Actix Web on
one busy Windows loopback host. That is a single measurement on one machine,
not a framework-wide speed ranking, and the 80k-request-per-second acceptance
gate has no qualifying run yet.

Energy efficiency is a target, not a result. Requests-per-watt or
server-replacement claims require matched hardware, whole-system power,
latency, error-rate, and sustained-load evidence, defined in the
[competitive roadmap](docs/competitive-roadmap.md).

## How the code is laid out

Three crates are developed in their own repositories and enter this workspace
as submodules under `crates/`, so a checkout and a CI run build one exact
reviewed revision:

| Crate | What it is |
| --- | --- |
| [`blazingly-contract`](https://github.com/sergii-ziborov/blazingly-contract) | portable operation contracts, fingerprints, compatibility |
| [`blazingly-wire`](https://github.com/sergii-ziborov/blazingly-wire) | HTTP/1 parsing and response framing, no framework or runtime deps |
| [`blazingly-json`](https://github.com/sergii-ziborov/blazingly-json) | the JSON engine every crate here encodes and decodes with |

`blazingly-wire` is consumed both by `blazingly-native` and by a
standard-library, thread-per-connection example server that uses no async at
all. `blazingly-native` contains the Compio adapter and no Tokio, Hyper, or
Axum. No Compio, socket, TLS, or native HTTP codec type crosses into
contract, core, or executor.

The framework workspace itself:

| Crate | What it is |
| --- | --- |
| `blazingly` | public facade and prelude |
| `blazingly-core` | application model and HTTP bindings |
| `blazingly-macros` | the Rust handler frontend |
| `blazingly-di` | typed providers, lifetimes, finalizers, compiled slots |
| `blazingly-executor` | shared handler decoding, validation, and execution |
| `blazingly-http` | runtime-neutral HTTP types, compiled routing, `TestApp` |
| `blazingly-native` | Tokio-free Compio HTTP/1 adapter, plus an HTTP/2 adapter kept outside the release contour |
| `blazingly-openapi` | the OpenAPI projection |
| `blazingly-mcp` | tools, resources, prompts, JSON-RPC, Streamable HTTP, sessions, audit |
| `blazingly-mcp-stdio` | bounded supervised newline-delimited stdio transport |
| `blazingly-docs` | API/AI bundles, examples, client starter, project scaffold |
| `blazingly-middleware` | CORS, compression, proxy/host policy, rate limits |
| `blazingly-security` | API-key, bearer/JWT/OAuth2, signed-session enforcement |
| `blazingly-validation` | advanced reusable validation types |
| `blazingly-observability` | access logging, request/trace IDs, OpenTelemetry, Prometheus |
| `blazingly-realtime` | SSE and WebSocket response models |
| `blazingly-templates` | compiled MiniJinja HTML responses |
| `blazingly-database` | bounded blocking-pool integration for sync DB and ORM pools |
| `blazingly-queue` | runtime-neutral queue contracts and test adapter |
| `blazingly-deploy` | Docker/Kubernetes/HPA artifact generation |
| `cargo-blazingly` | application discovery, autoreload, diagnostics, build/run CLI |

## Ecosystem

These live outside the workspace so vendor-specific code never enters the
framework tree. **They are published at `0.0.1` against Blazingly `0.1.x` and
have not yet been updated for `0.2`.**

- [`blazingly-sqlite`](https://github.com/sergii-ziborov/blazingly-sqlite) —
  SQLite over `rusqlite`: separate read and write connection lanes (one writer,
  many WAL readers), read-heavy pragma tuning, prepared-statement caching,
  dirty reads via `read_uncommitted` on shared-cache pools, migrations with
  drift detection;
- [`blazingly-postgres`](https://github.com/sergii-ziborov/blazingly-postgres) —
  the PostgreSQL frontend/backend protocol version 3 implemented directly over
  `std::net::TcpStream`: SCRAM-SHA-256, binary parameter binding, all four
  isolation levels, SQLSTATE error classification, advisory-locked migrations.
  No `postgres`/`tokio-postgres` dependency and no async runtime in its tree;
- [`blazingly-redis`](https://github.com/sergii-ziborov/blazingly-redis) —
  RESP implemented directly, backing three distributed seams: the Streams queue
  (consumer groups, at-least-once delivery with redelivery of dead consumers'
  work, dead-lettering with a bounded attempt count), a rate-limit store whose
  check-and-consume is one Lua script so two pods cannot both pass on the last
  token, and a session store with server-enforced expiry;
- [`blazingly-nats`](https://github.com/sergii-ziborov/blazingly-nats) — NATS
  JetStream for the queue seam with the core protocol and a JetStream JSON
  layer implemented directly: durable pull consumers, the server's own delivery
  count as the attempt number, nack-with-delay, publish dedup via `Nats-Msg-Id`;
- [`blazingly-examples`](https://github.com/sergii-ziborov/blazingly-examples) —
  six complete runnable applications, from a 15-minute CRUD to MCP tools over
  stdio and Streamable HTTP;
- [`blazingly-benchmarks`](https://github.com/sergii-ziborov/blazingly-benchmarks) —
  external conformance and performance comparisons.

## Facade features

On by default: `deploy`, `docs`, `mcp`, `middleware`, `observability`,
`openapi`, `realtime`, `security`, `validation`. Native socket and ecosystem
integrations are opt-in:

| Feature | Enables |
| --- | --- |
| `native` | the Tokio-free HTTP/1 server |
| `native-tls` | `native` plus rustls |
| `native-http2` | `native` plus experimental HTTP/2 |
| `mcp-stdio` | `mcp` plus the supervised stdio transport |
| `observability-otel` | `observability` plus OpenTelemetry parent propagation |
| `database`, `queue`, `templates` | the optional ecosystem integration crates |

`cargo check -p blazingly --no-default-features` verifies the minimal facade:
contract, core, DI, executor, HTTP, macros, and `blazingly-json`. Turning the
default features off does not remove the OpenAPI projection from the build —
`blazingly-http` serves `/openapi.json` and so depends on `blazingly-openapi`
unconditionally. The `openapi` feature gates the `blazingly::openapi`
re-export, not the compilation.

## Scope

Mesh and Cloudflare execution are future products outside this repository. See
[the architecture boundary](docs/architecture.md). Competitive gaps, platform
experiments, and the evidence required before making a performance or
efficiency claim are tracked in the
[competitive and platform roadmap](docs/competitive-roadmap.md).

## More

- [Getting started](docs/getting-started.md)
- [Developer CLI workflow](docs/developer-workflow.md)
- [Dependency injection and plugin scopes](docs/dependency-injection.md)
- [Stability and SemVer](docs/stability.md)
- [Ecosystem integration boundary](docs/ecosystem.md)
- [Architecture](docs/architecture.md)
- [Benchmark status](docs/benchmark-status.md)
- [Security policy](SECURITY.md) — reporting, fuzz/Miri/sanitizer coverage
- [Changelog](CHANGELOG.md)

## License

Licensed under the MIT License. See [LICENSE](LICENSE).
