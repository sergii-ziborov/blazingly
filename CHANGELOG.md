# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning follows Semantic Versioning as qualified by
[docs/stability.md](docs/stability.md): the workspace is pre-1.0, so a minor
release may break the framework Rust API if the break is recorded here.

`blazingly-contract`, `blazingly-wire`, and `blazingly-json` live in separate
repositories with their own tags and changelogs, and enter this workspace as
submodules. Entries below describe the framework workspace only; a change to a
pinned submodule revision is recorded as a single entry.

## [Unreleased]

Nothing yet.

## [0.1.0] - 2026-07-29

First release. Everything below is new; there is no prior published version to
compare against, so nothing here is a breaking change.

Blazingly is an operation-first Rust API framework. One typed declaration per
operation produces HTTP extraction, validation, typed responses, OpenAPI, agent
documentation, and native MCP tools. It is pre-1.0: suitable for evaluation and
controlled production trials, not a promise that the Rust API is frozen. Read
[docs/getting-started.md](docs/getting-started.md) first and
[docs/stability.md](docs/stability.md) before depending on it.

### Added

#### Declaring operations

- `#[get]`, `#[head]`, `#[post]`, `#[put]`, `#[patch]`, `#[delete]`,
  `#[options]`, `#[trace]`, and `#[connect]`, all aliases over
  `#[operation(method = ..., path = ..., id = ...)]`. Each operation carries a
  stable id that names it in the contract, the OpenAPI document, the generated
  documentation, and compatibility reports.
- Handlers may be synchronous or asynchronous. A synchronous handler on an
  operation with no lifecycle hooks takes a direct, allocation-free path; the
  macro also emits an asynchronous form so hooks, cancellation, timeouts, and
  finalizers behave identically either way.
- `Path<T>`, `Query<T>`, `Header<T>`, `Cookie<T>`, `Json<T>`, `Form<T>`,
  `Multipart<T>`, and `File<T>`, with several extractors per handler.
- `Accepted<T>`, `Created<T>`, `NoContent`, `Status<CODE, T>`, `WithHeaders`
  with validated response headers including repeated `Set-Cookie`, and
  pull-based `StreamingBody` responses.
- `PreparedJson<T>` encodes a response inside the handler's own scope, so a
  listing can serialize borrowed data while the store guard is still held
  instead of building an owned copy first. `Json<T>` is unchanged.
- `routes![...]` registration with duplicate detection.

#### Models and validation

- `#[api_model]` structs with native field validation: `min_length`,
  `max_length`, `pattern`, `minimum`, `maximum`, `exclusive_minimum`,
  `exclusive_maximum`, `multiple_of`, `min_items`, `max_items`, `unique_items`,
  `email`, `default`, `alias`, `nested`, and `validate_with` for a custom
  function.
- Three model shapes: an ordinary struct, a one-field tuple struct declaring a
  reusable rule bundle, and a unit-variant enum declaring a string enumeration
  with its variant set in the schema.
- `#[default(...)]` reaches the handler as `T` rather than `Option<T>` and is
  stated in the document. `Option` fields are marked nullable where the document
  needs it.
- Strong value types behind the default `validation` feature: `Uuid`, `Url`,
  `IpAddress`, `Date`, `DateTime`, `Decimal`.
- Nested models and collections recurse automatically, and decode failures and
  rule failures share one field-path syntax (`address.street`,
  `items[0].street`).
- A borrowed `#[api_model]` form, so a response can be described without owning
  the data it describes.

#### Errors

- `#[api_error]` declares stable domain errors: HTTP status, machine-readable
  code, message, optional typed details, and declared response headers. The
  declaration reaches OpenAPI, the Markdown bundle, and MCP without restating
  it.
- Invalid response headers, serialization failures, and other
  framework-internal failures are redacted to a generic `500` over HTTP and a
  generic internal protocol error over MCP.
- An application-level error-handler seam can give a service one house style;
  after it runs, a domain failure's status is restored to the one its
  `#[api_error]` variant declared, so the seam cannot silently rewrite a
  documented status code.

#### Application structure and dependency injection

- Compiled dependency injection: direct typed handler arguments or
  `Depends<T>`, `singleton`/`request`/`transient` lifetimes, synchronous and
  asynchronous fallible providers, reverse-order finalizers, and `#[provider]`
  for typed factories. The graph is compiled during `ExecutableApp`
  construction and resolved per request through numeric slots rather than a
  type-name registry.
- Fastify-style nested `Plugin` scopes with downward-only provider inheritance
  and local overrides.
- Async lifecycle hooks compiled per operation — `on_request`, `pre_parse`,
  `pre_validate`, `pre_handler`, `pre_serialize`, reverse-order `on_error` and
  `on_response`, and child-before-parent shutdown hooks. The request path never
  walks the plugin tree.
- Startup and shutdown lifespan hooks, after-response background tasks with an
  injectable `BackgroundTasks`, and a bounded pool for synchronous handlers and
  blocking database work.
- Runtime-neutral cancellation and adapter-supplied timeouts, with finalizers
  shielded after abort.
- `TestApp` runs the whole pipeline in memory, and `TestOverrides` replaces a
  provider globally or inside one named plugin scope; an override that matches
  nothing is a build error rather than a silent pass.

#### HTTP policy and security

- Runtime-neutral middleware: CORS, negotiated GZip/Brotli compression
  including streamed responses, trusted-host and trusted-proxy normalization,
  bounded global and per-client rate limiting, and static files with single
  `Range` and `If-Range` support. Middleware can be scoped to a path prefix or
  an operation id.
- Registered API-key, HTTP, OAuth2, OpenID Connect, and mutual-TLS security
  schemes with operation-level scope requirements, enforced by named runtime
  verifiers. Ready HS256 JWT, OAuth2 bearer-scope, HTTP Basic, signed
  session-cookie with an optional server-side store, and constant-time API-key
  verification; authenticated state reaches handlers as
  `Extension<SecurityContext>`.
- An operation that declares a security scheme fails closed: with no registered
  verifier the request is rejected rather than served unauthenticated.
- Security runs after routing and before body parsing, by the same middleware
  pipeline in `TestApp`, native HTTP/1, and HTTP/2.

#### Native server

- A Compio-based HTTP/1 adapter with no Tokio, no Hyper, and no Axum:
  keep-alive, pipelining, Content-Length and chunked bodies, `Expect:
  100-continue` per RFC 9110, header/body/idle/write deadlines, `TCP_NODELAY`,
  an optional connection cap, a bounded TLS handshake, rustls TLS, graceful
  drain, a cached `Date` header without a per-request clock syscall, bounded
  pipelined-response write coalescing, and a thread-per-core launcher with one
  compiled application per worker.
- Request bodies reach the streaming boundary on the plaintext socket, the
  generic compatibility transport TLS runs over, and HTTP/2, for operations
  that declare a stream input.
- `UploadBody` request streaming with bounded backpressure, and
  `UploadBody::into_multipart` for reading a `multipart/form-data` body field
  by field and chunk by chunk without holding the upload.
- Server-Sent Events and native WebSocket upgrades over plaintext and TLS.
- The Tokio-free claim is enforced, not asserted: `deny.toml` bans `tokio`,
  `hyper`, and `axum` at any depth and a CI job checks it on every push.

#### OpenAPI, documentation, and deployment

- Deterministic OpenAPI 3.1 with JSON Schema 2020-12, a precompiled
  `/openapi.json`, and Scalar or Swagger UI mounts. Operations carry tags
  derived from the namespace of their id, and long-form descriptions, examples,
  defaults, nullability, and enumerations all reach the document.
- Generated API and AI-oriented Markdown bundles, canonical contract
  manifests, HTTP and MCP examples, a Rust client starter, and a Tokio-free
  project scaffold.
- Container and Kubernetes deployment generation with a shared HPA and a choice
  of maintained-NGINX or direct `LoadBalancer` exposure.

#### MCP

- Native MCP discovery and in-process tool invocation over the same executor
  that serves HTTP. MCP is projected from the operation model, not
  reconstructed from OpenAPI, so a tool call runs the same handler, validation,
  and typed errors.
- `CallToolResult` responses with confirmation, output-exposure, validation,
  and typed-error handling. `confirmation = "required"` rejects a call unless
  the host sends `_meta["dev.blazingly/confirmed"] = true`.
- JSON-RPC lifecycle, resources, prompts, a bounded redacted audit log,
  stateful Streamable HTTP with sessions, and a supervised newline-delimited
  stdio transport with message, size, and rejected-frame limits.
- Tool input schemas carry defaults, enumerations, and nullability as
  machine-readable JSON Schema rather than opaque validator strings.

#### Observability

- Request ids, W3C trace context, structured access events, `tracing`
  integration, optional OpenTelemetry parent propagation, and Prometheus
  request, error, and latency metrics with method, route, and status labels and
  configurable histogram buckets.
- Request, trace, and span ids come from a CSPRNG, so replicas do not all
  restart the same counter at 1.
- Process resident-memory and CPU metrics on Linux and Windows. The Windows
  path reaches `GetProcessMemoryInfo` and `GetProcessTimes` through minimal
  safe wrappers, because `unsafe_code` is forbidden workspace-wide.
- The OTLP exporter takes a pluggable `BlockingTransport`. The bundled
  transport is plaintext HTTP/1.1 to a local collector sidecar; an application
  that needs HTTPS injects its own.

#### Contracts and compatibility

- Versioned canonical operation contracts with SHA-256 fingerprints and
  semantic compatibility reports covering inputs, nested models, dependencies,
  security, responses, MCP exposure, and agent policy. A canonical-format
  change also increments the format version, so a fingerprint never changes
  silently.

#### Tooling

- `cargo blazingly new`, `dev`, `run`, `build`, `check`, `openapi`, `routes`,
  `discover`, and `doctor`, with application discovery, `Blazingly.toml`, and
  polling autoreload that builds before swapping the process, so a compile
  error leaves the previous binary serving.
- `openapi` and `routes` run the unmodified application binary as a printer
  through the documented `BLAZINGLY_EMIT` seam, on an ephemeral port with one
  worker so they cannot race a running dev server.

#### Ecosystem seams

- `blazingly-database` schedules synchronous database and ORM pools on the
  bounded blocking pool, with transactions, typed error classification, and a
  migration seam.
- `blazingly-queue` defines publish/receive/ack/nack contracts and ships a
  deterministic in-memory conformance adapter and a worker runtime with bounded
  retry, backoff, and dead-lettering.
- `blazingly-templates` compiles MiniJinja templates once and returns typed,
  escaped `text/html`. Autoescaping is forced regardless of template name.

#### Performance work in this release

Measurements below were taken on the project's own benchmark harness; see
[docs/benchmark-status.md](docs/benchmark-status.md) for hosts, methods, and
what is deliberately not claimed.

- The framework owns its JSON engine. All shipped crates encode and decode with
  `blazingly-json`; `serde_json` remains only as a dev-dependency of one crate,
  as an independent oracle so a matching encoder and decoder cannot agree on a
  wrong wire format unnoticed.
- Pattern matching compiles to one of three engines instead of always
  simulating. Declarative `#[pattern]` rules went from 61% of throughput on the
  bulk-ingest scenario to about 1%.
- Streaming multipart uploads hold one transport chunk plus a delimiter of
  look-ahead. Over an eightfold rise in concurrency, peak resident memory rose
  from 106.2 MiB to 556.9 MiB with the buffered extractor and stayed flat
  between 25.8 MiB and 26.5 MiB with the streaming reader. Allocations per
  request fell from 3,235 to 114.
- `PreparedJson` removed roughly 200 string clones per listing request on the
  benchmark application.

#### Quality automation

- CI on Linux, Windows, and macOS at the 1.88 MSRV: `cargo fmt --check`,
  `clippy --workspace --all-targets --all-features -D warnings`, and
  `cargo test --workspace --all-features --locked`.
- Sixteen feature-subset builds covering each transport, MCP over stdio,
  OpenTelemetry, the ecosystem group, the realistic production superset, and
  the contract under `no_std`, rather than only `--all-features` and
  `--no-default-features`.
- `cargo doc` with `-D warnings`, `cargo audit`, `cargo deny check bans
  licenses sources`, Miri over the portable crates, AddressSanitizer over the
  socket-facing crates including `blazingly-native`, bounded fuzz targets for
  HTTP/1 request parsing and chunk decoding, and `cargo-semver-checks` against
  the previous contract revision.
- `unsafe_code = "forbid"` workspace-wide.

### Known limitations

- The framework Rust API is experimental under
  [docs/stability.md](docs/stability.md). A pre-1.0 minor release may break it;
  the break will be recorded here.
- HTTP/2 is deliberately outside the release contour. It is off by default
  behind `native-http2`, its Sans-I/O codec is pinned to an upstream canary
  release, and no release gate mentions it. A supported HTTP/2 will live in a
  separate `blazingly-http2` repository. The reasoning is in
  [docs/stability.md](docs/stability.md).
- The `80,000` req/s validated-scenario acceptance gate is not met: the
  validated scenario measured 65,650 req/s on 2026-07-27, and Actix Web leads
  on throughput and on every measured latency percentile. No
  "faster than Axum or Actix Web" claim is made.
- `blazingly-database` and `blazingly-queue` are seams, not drivers. Concrete
  adapters for Diesel, rusqlite, PostgreSQL, NATS, RabbitMQ, Kafka, and SQS
  belong in separate packages and are not part of this release; the only queue
  adapter that ships is the in-memory conformance one.
- Process metrics are implemented on Linux and Windows. macOS is deliberately
  not implemented rather than shipped unverified.
- TLS certificate reload ergonomics, asymmetric JWT and JWKS/OIDC discovery,
  key rotation, and CSRF helpers are follow-up work.
- The bundled OTLP transport is plaintext HTTP/1.1.
- No independent external security audit has been performed. Reporting process:
  [SECURITY.md](SECURITY.md).
- The advisory `workspace-semver` job currently fails to resolve its baseline
  and therefore reports no API diff. It has no baseline to compare against for
  this release; it must produce a report before the next one.
