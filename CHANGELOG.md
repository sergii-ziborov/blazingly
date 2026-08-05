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

This breaks the framework Rust API, which pre-1.0 permits when the break is
recorded, so it releases as 0.3.0 rather than a patch. One break, mechanical:
`OperationDescriptor` gains a public `documentation` field, so a struct literal
that names every field no longer compiles. Use `OperationDescriptor::new` and
the builders, or add `documentation: OperationDocumentation::default()`.

### Added

- An operation declares the prose a reader needs and the code cannot supply:
  `tags`, `description`, `deprecated`, `external_docs`, and
  `external_docs_description` on every method attribute and on `#[operation]`.
  `tags` replaces the group otherwise taken from the namespace of the operation
  id, and its first entry names the section in the generated Markdown.
  `deprecated` stands alone or takes `= true` / `= false`.

  None of this enters `blazingly-contract`. It lives in a new
  `OperationDocumentation` beside the contract, for the same reason HTTP paths
  and methods already do: it is unverifiable against a handler signature, so it
  cannot drift from the code, and it must not move an operation fingerprint or
  register as a compatibility change. Adding a tag to a shipped operation is
  not a change to that operation.
- The document as a whole takes `OpenApiConfig::with_description`,
  `with_server` (with `OpenApiServer`), and `with_tag_description`.
- `OpenApiConfig::with_overlay` merges raw OpenAPI into the generated document
  — the escape hatch for `callbacks`, `webhooks`, `info.contact`, prose on an
  individual response, and vendor extensions. The merge is **additive**: it
  writes a key only where the projection produced none, recursing into shared
  objects. An overlay can therefore add to the document but can never overwrite
  a schema, a status code, a parameter, or a security requirement that came
  from the code, so the property that makes the document worth trusting
  survives the framework having an escape hatch at all.
- An operation that declares `#[security(...)]` now documents the responses the
  security pipeline itself answers with: `401`, and `403` as well when the
  requirement names scopes. These carry `x-blazingly-automatic`, the same
  marker the derived `422` uses, and an operation that declares either status
  keeps its own. The framework already enforced this fail-closed; it simply did
  not say so, which left a hand-written `utoipa` annotation more complete than
  the generated document on exactly the axis this project claims to win.

## [0.2.2] - 2026-08-05

No Rust API changes. A second, adversarial pass over 0.2.1 found that the
release correcting the documentation had itself shipped two false statements,
and that the primary API surface was undocumented. Submodule crate published
alongside: `blazingly-json` 0.1.3.

### Fixed

- Every HTTP-method attribute macro — `#[get]`, `#[post]`, `#[put]`,
  `#[patch]`, `#[delete]`, `#[head]`, `#[options]`, `#[trace]`, `#[connect]` —
  and `#[api_error]`, `#[mcp::tool]`, `#[security]`, and `routes!` had no
  rustdoc at all. These are the framework's primary surface, so its docs.rs
  entry point listed the macros a user writes first and said nothing about any
  of them. `#[security]` was documented in no file in the repository.
- `cargo check -p blazingly --no-default-features` was described, in both the
  repository README and the facade's crates.io page, as verifying a minimal
  contract/core/DI/executor/HTTP/macros build. It does not: `blazingly-json` is
  an unconditional dependency, and `blazingly-http` depends on
  `blazingly-openapi` unconditionally because it serves `/openapi.json`, so the
  OpenAPI projection is compiled either way. The `openapi` feature gates the
  re-export, not the compilation.
- The README's feature table listed `deploy` as opt-in two lines below the
  sentence naming it as on by default.
- `blazingly-json`'s crates.io page called the crate a 0.1.0 release candidate
  with no production consumer, at version 0.1.2, while being the engine every
  crate in this framework encodes and decodes with.
- `docs/competitive-roadmap.md` presented the request-aware DI, custom
  extraction, and composition slices as the next work to build, in a section
  below the matrix rows recording all three as shipped. It now describes what
  is actually left, including that TLS is unreachable from shipped code.

### Changed

- Each crate declares keywords describing what it is. All 21 shared one
  inherited set (`framework`, `http`, `api`, `openapi`, `mcp`), so a crates.io
  search for `queue`, `websocket`, `jwt`, `prometheus`, `kubernetes`, or
  `cargo-subcommand` returned none of them, and all 21 competed with each other
  for the same five words.
- The scaffold generated by `cargo blazingly new` emits `summary` on its
  handler, and `docs/getting-started.md` no longer shows a `cargo blazingly
  routes` transcript the tool does not produce.

### Known limitations

Added to the list carried at 0.2.1: TLS cannot be configured with shipped code.
`Server::with_tls_config` takes a `compio::tls::rustls::ServerConfig`, but
nothing re-exports a way to name that type and no crate here loads a
certificate from disk, so `native-tls` is today a builder method an application
cannot call without adding `compio` and `rustls` as direct dependencies itself.
There is also no typed application-configuration capability: no settings type,
no environment or file loading, no startup validation. Both are scoped in
[the competitive roadmap](docs/competitive-roadmap.md).

## [0.2.1] - 2026-08-05

No Rust API changes. Packaging and documentation corrections only. Published
tarballs are immutable, so everything below reaches users at this release
rather than retroactively repairing 0.2.0.

Submodule crates published alongside: `blazingly-contract` 0.4.1,
`blazingly-json` 0.1.2, `blazingly-wire` 0.1.3 — each carrying the same class
of correction to its own crates.io page.

### Fixed

- Every workspace crate now ships the MIT LICENSE file its manifest declares.
  The 21 crates published at 0.2.0 declare `license = "MIT"` and contain no
  LICENSE file, because only files inside a crate directory reach its tarball.
- docs.rs builds default features unless the manifest says otherwise, and this
  workspace puts most of its surface behind optional features. Every
  publishable manifest now carries `[package.metadata.docs.rs] all-features =
  true`. Before this, `blazingly::native::MulticoreServer` — the type
  getting-started tells a new user to reach for first — did not appear on
  docs.rs at all, and neither did `database`, `mcp-stdio`, `native-http2`,
  `native-tls`, `observability-otel`, `queue`, or `templates`.
- Eleven crates, the facade among them, had no crate-level documentation, so
  their docs.rs landing page was a bare item list. Each now includes its own
  README as the crate doc, which also means every README example is compiled
  as a doctest from here on.
- The repository README's main example had stopped compiling: it declared
  items and then issued top-level `let ... ?` statements, so it was valid at
  neither module nor function scope. It is now a `fn main` and is compiled by
  the workspace test run through a `#[cfg(doctest)]` include in the facade.
- The README described a published release as a prototype, miscounted the
  external repositories in the sentence that introduces them, attributed the
  worker-priority measurement to a contended host when
  [benchmark status](docs/benchmark-status.md) records a quiet one, and claimed
  a bounded pool runs synchronous handlers. That last one was the load-bearing
  error: a synchronous handler runs inline on the worker that accepted the
  request and is never moved to the blocking pool, so blocking work must call
  `run_blocking` or it stalls one thread-per-core worker and every connection
  placed on it. README and crate docs now say so at the point of use.
- `docs/architecture.md` described a submodule pin mismatch that no longer
  exists and listed release readiness as blocked with `publish = false`.
- `docs/getting-started.md`'s extractor list omitted `Extract<T>` and
  `Extract<RequestParts>`, so the documented path to the peer address and the
  raw request line was missing.
- The advisory `workspace-semver` job compares each crate against its own
  latest crates.io release instead of `HEAD^`, so it reports the diff someone
  upgrading actually sees rather than the contents of one push. The 0.1.0
  known limitations recorded it as unable to resolve a baseline.
- `.cargo/audit.toml` ignores RUSTSEC-2026-0235 (rkyv 0.7.46, out-of-bounds
  reads). `cargo audit` reads the feature-agnostic `Cargo.lock`; rkyv arrives
  only as an optional dependency of `rust_decimal`, whose enabled features are
  exactly `default`, `serde`, `std`, so it is never compiled. A CI step
  re-proves that from the feature-resolved graph on every run and fails the day
  it stops holding.
- The release process in [docs/stability.md](docs/stability.md) is rewritten
  around two silent failures the 0.2.0 pre-tag audit caught: a submodule whose
  code changed without a version bump is skipped by the publisher, and a stale
  `[workspace.dependencies]` requirement resolves siblings from the previous
  release. It also names `blazingly-json`, which it had omitted.

### Changed

- `api-bindings` is dropped from the workspace categories. crates.io defines it
  as an idiomatic wrapper around somebody else's web service, which is the
  opposite relationship to a framework for authoring one.
- `cargo-blazingly` no longer inherits the workspace categories. As a cargo
  subcommand it belongs under `development-tools::cargo-plugins` and
  `command-line-utilities`, the two lists a developer browses to find
  `cargo blazingly new`; it was carrying `web-programming::http-server` and
  appearing in neither.
- Every crate declares a `homepage`.

### Known limitations

Carried forward and re-verified at this release: the Rust API is pre-1.0 and
may break in a minor release when the break is recorded here; HTTP/2 is opt-in
behind `native-http2` and outside the release contour; the 80k-request-per-second
acceptance gate has no qualifying run yet; TLS certificate reload, asymmetric
JWT with JWKS/OIDC discovery, key rotation, and CSRF helpers are absent; the
OTLP exporter speaks plaintext transport only; there has been no external
security audit.

New to this list: a `#[provider]` cannot take `Extension<T>`, so a dependency
cannot see the authenticated identity — read it in the handler instead. The
published ecosystem adapters (`blazingly-sqlite`, `blazingly-postgres`,
`blazingly-redis`, `blazingly-nats`) and `blazingly-examples` target 0.1.x and
have not been updated for 0.2.

## [0.2.0] - 2026-08-04

This release breaks the framework Rust API, which pre-1.0 permits when the
break is recorded. Three breaks, all mechanical:

- `blazingly-contract` 0.4.0 adds a public `constraints` field to
  `TypeDescriptor`, whose fields are all public: a struct literal that names
  every field no longer compiles. Use `TypeDescriptor::scalar`/`model`/`new`,
  or add `constraints: Vec::new()`.
- The portable contract format moves v1.2 to v1.3, so **every operation
  fingerprint changes**. Recorded compatibility baselines must be recaptured;
  a diff taken across this release reports changes that are not real.
- `ExecutableBuildError` gains `InvalidMountPrefix`, `InvalidIdNamespace`,
  `SingletonRequestInputs`, and `ConflictingProviderInput`. An exhaustive
  `match` over it needs the new arms or a wildcard.

Submodule crates published alongside this release: `blazingly-contract`
0.4.0, `blazingly-json` 0.1.1, `blazingly-wire` 0.1.2. The json and wire
bumps carry performance work that landed after their previous publish and had
never reached the registry.

### Added

- Request-aware providers: a `#[provider]` may now declare `Path<T>`,
  `Query<T>`, `Header<T>`, and `Cookie<T>` arguments beside `Depends<T>`,
  including async and transient providers. Each declared input folds into the
  consuming operation's contract exactly once — the same header read by two
  providers is decoded once, appears once in `OpenAPI` parameters, MCP tool
  schemas, generated documentation, and fingerprints, and fails validation
  with the same `422` envelope a handler-declared input produces, before any
  provider runs. A test override replacing a provider bypasses its inputs
  entirely; a singleton provider cannot declare request inputs, and one wire
  input consumed at two different types fails the build. Dependency
  resolution keeps its compiled numeric slots: inputs occupy pre-decoded
  slots at the front of the request slice, and providers without inputs
  compile exactly as before.
- `MulticoreServer::with_worker_priority(WorkerPriority::Elevated)`: workers
  request elevated scheduling priority when they start, shortening the gap
  between an I/O completion arriving and the worker running again on a
  contended host — the layer the tail-attribution work identified as owning
  the millisecond-scale latency tail, after the request path itself measured
  microseconds. Best effort on every platform (Windows above-normal priority;
  macOS and Linux through the portable priority API); a system that refuses
  the request keeps the inherited priority and the server serves normally.
  The default stays `Inherited`: a framework should not quietly outrank the
  rest of a shared machine.
- `Plugin::mount("/v1")` and `Plugin::with_id_namespace("v1")`: a module
  written once mounts under two path prefixes without restating a handler.
  Prefixes and namespaces nest, identities and MCP tool names stay distinct
  per mount, and malformed or colliding mounts fail at build time.
- `Extract<RequestParts>`: an owned snapshot of the raw request line and
  connection — method, path, effective scheme and host, peer address — taken
  before the handler runs. `HttpRequestParts` gained the matching borrowed
  accessors (defaulted, so existing adapters keep compiling), and a transport
  without a request line rejects with `400 transport_mismatch`.

- `blazingly_mcp::FrameworkManifest`: a read-only MCP resource at the stable
  URI `blazingly://framework/manifest` that publishes the operation graph to
  an agent — identities, HTTP bindings, contract fingerprints, agent policy,
  inputs, dependencies, security requirements, and response shapes. It is
  deliberately static metadata only: no environment or runtime configuration,
  no security-scheme configuration, no response-header values, no tool
  descriptions. Mutating tools are not part of it and will need
  authentication, confirmation, audit, and rollback before they exist.
- Process CPU and resident-memory metrics on macOS, which the 0.1.0 known
  limitations recorded as deliberately unimplemented. All three first-class
  platforms now report the same observability surface.
- Operations whose input is decoded now document the `422` they can return
  before the handler runs, the way the runtime already answers. The response
  carries the rejection envelope, the closed set of codes that operation's own
  inputs can produce — derived per input source from the executor's actual
  behaviour, so a JSON body can fail as `invalid_json` even with no declared
  rule — and the `violations` array naming the field path and code of each
  broken rule. An operation that only streams bytes documents no `422`, and an
  operation that declares its own `422` keeps it. The projected response is
  marked `x-blazingly-automatic` so a reader can tell it from a declared one.

### Changed

- The multicore accept loop now places each connection on the worker with
  the fewest live connections instead of rotating blindly, with ties still
  rotating — a fresh or evenly loaded server distributes exactly as before,
  and the counts only change placement when load is actually skewed. One
  slow worker no longer accumulates queueing tail while its neighbours idle.
- `BLAZINGLY_NATIVE_STAGE_METRICS=1` makes the native HTTP/1 loop record two
  log2 histograms per keep-alive cycle — head-parsed to response-flushed, and
  response-flushed to next head-parsed — and print snapshots periodically, so
  tail latency can be attributed to one side of the socket write in
  production code. Disabled, it costs one static boolean check per request.
- The `OpenAPI` and MCP schema projections now share one traversal in
  `blazingly-core`'s hidden `schema` module, parameterised by a small dialect
  trait; both generated documents are unchanged. The duplication had let the
  two documents drift twice.
- Advanced the portable contract format to v1.3 (`blazingly-contract` 0.4.0):
  value-type constraints are retained on `TypeDescriptor`, including collection
  items and nested values, and participate in fingerprints and compatibility
  reports. Every fingerprint moves with the format version, so recorded
  baselines must be recaptured when upgrading; in exchange, tightening an
  item's own bounds is now reported at the item path instead of as an opaque
  custom-validator change.
- Reworked compiled routing around a static-path table, compact method slots,
  a parameter trie with allocation-free backtracking for small captures, and a
  cheaper path hasher. `404`/`405` resolution and sorted `Allow` construction
  share the compiled route data.
- Synchronous dependency chains now use the synchronous compiled-provider path
  and cache whether a chain requires async fallback, avoiding ready-future
  allocation on subsequent synchronous resolutions.
- Reworked the bounded blocking pool's worker wake/park and shutdown paths,
  preserved workers after a task panic, and exposed worker-context detection so
  nested blocking work can avoid resubmitting behind itself.
- Added synchronous database `run_sync` and `transaction_sync` entry points;
  async database calls already running on a blocking-pool worker execute inline
  with the same error and rollback classification instead of risking a
  saturated-pool self-deadlock.
- `cargo blazingly new` now generates a dependency on the crates.io framework
  version matching the installed CLI, while `--framework-path` retains the
  local-checkout workflow and the scaffold documents the opt-in Git form.
- Advanced `blazingly-json` to 0.1.1, the parser revision that jumps between
  string escapes instead of scanning decoded strings byte by byte: 1 MiB of
  roughly 1% escapes parsed in 2.59ms and now parses in 0.88ms, which is
  parity with `serde_json` on the same document.
- Advanced `blazingly-wire` to 0.1.2, which removes formatting machinery from
  response encoding, adds reusable prepared headers, and removes quadratic
  inline-header insertion during parsing: a 200 response with six headers
  encoded in 521ns, encodes in 187ns, and in 70ns through prepared headers
  against a 67ns hand-written floor.

### Fixed

- The rules a value type declares now reach the item schema wherever the type
  appears: `#[api_model] #[max_length(20)] struct Tag(String);` used as
  `tags: Vec<Tag>` published items as a bare `{"type": "string"}` in
  `/openapi.json`, in MCP tool schemas, and in the generated Markdown, while
  the validator rejected an over-long element at `tags[0]`. The bounds now
  survive as collection items, behind `Option`, and at any nesting depth. The
  same gap dropped an enumeration's variants, so a generated sample for
  `Vec<Language>` was a payload the server refused.
- Kept blocking workers available after panicking jobs and ensured pool drop
  wakes workers so queued work drains before shutdown.
- Avoided a nested database scheduling deadlock when database work is invoked
  from a saturated blocking pool.

## [0.1.1] - 2026-07-29

No Rust API changes.

- Every first-party crate now ships its own README describing what the
  crate is and whether it is usable without the facade, replacing the
  workspace README that all crates.io pages previously shared. Eleven
  crates reached crates.io as 0.1.0 before this landed; their pages are
  corrected by this version, because a published version's files are
  immutable.
- The release workflow asks the registry before publishing, so a
  resumed release skips already-published versions in seconds.

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
