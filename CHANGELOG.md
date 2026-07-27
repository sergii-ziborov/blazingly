# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning follows Semantic Versioning as qualified by
[docs/stability.md](docs/stability.md): the workspace is pre-1.0, so a minor
release may break the framework Rust API if the break is recorded here.

`blazingly-contract` lives in a separate repository with its own tags and its
own changelog. Entries below describe the framework workspace only; a change to
the pinned contract revision is recorded as a single entry.

## [Unreleased]

Nothing has been released yet. Every workspace crate is pinned at `0.0.1` with
`publish = false`. See "Release process" in [docs/stability.md](docs/stability.md)
for the gate that flips it.

### Added

- Runtime-neutral operation model, compiled router, and in-memory `TestApp`.
- All HTTP methods with typed path, query, header, cookie, JSON, form,
  multipart, and file extraction, plus the runtime-neutral `UploadBody` request
  stream.
- Typed success, error, and response-header pipeline, arbitrary
  `Status<CODE, T>`, and pull-based `StreamingBody` responses.
- Compiled dependency injection: plugin scopes, singleton/request/transient
  lifetimes, async providers and finalizers, test overrides, cancellation and
  timeouts, and per-operation numeric slot plans.
- Procedural macros for operation, model, and error declarations.
- OpenAPI document and browser UI generation, AI-oriented Markdown
  documentation, and container/Kubernetes deployment scaffolds.
- MCP server sharing the HTTP operation pipeline: JSON-RPC lifecycle,
  Streamable HTTP, resources, prompts, redacted bounded audit, and a supervised
  stdio transport.
- `blazingly-wire`: framework-independent HTTP/1 parsing, chunk decoding,
  bounded limits, request-smuggling checks, and response framing, plus the
  `blazingly-wire-standalone` smoke server.
- `blazingly-native`: Tokio-free Compio adapter with direct plaintext HTTP/1
  I/O, chunked requests, optional rustls TLS, graceful drain, a cached `Date`
  header, bounded pipelined-response write coalescing, and a thread-per-core
  launcher with one compiled app per worker.
- Experimental HTTP/2 adapter with prior-knowledge and TLS-ALPN entry,
  concurrent per-stream handlers, and pull-based response bodies.
- Runtime-neutral middleware: CORS, negotiated compression, trusted host and
  proxy normalisation, and bounded rate limiting.
- Security middleware binding JWT, OAuth2, API-key, and session verifiers to
  the named schemes in each operation descriptor, with `SecurityContext`
  reaching handlers as a request extension.
- Server-Sent Events and plaintext HTTP/1 WebSocket upgrades, lifespan and
  background tasks, a bounded blocking pool, and strong string-like validation
  types.
- Request identity, access events, tracing and OpenTelemetry propagation,
  Prometheus metrics, and error counters.
- `cargo blazingly` discovery, autoreload, and production commands, plus the
  database, queue, and template integration seams.
- Quality automation: HTTP/1 fuzz targets, Miri, AddressSanitizer,
  `cargo audit`, `cargo-semver-checks` against the contract, and `SECURITY.md`.
- Repository-root MIT `LICENSE`, matching the licence the contract repository
  already publishes under.
- Publishable package metadata: shared `license`, `repository`, `readme`,
  `keywords`, and `categories` in `[workspace.package]`, and a per-crate
  `description` on every workspace crate.
- CI `feature-combinations` matrix covering the transport, MCP, observability,
  and ecosystem feature subsets rather than only `--all-features` and
  `--no-default-features`, and a `rustdoc` job running with `-D warnings`.
- Advisory workspace-wide `workspace-semver` job alongside the blocking
  `contract-semver` gate.
- This changelog.

### Changed

- AddressSanitizer now covers `blazingly-native`, the crate that owns every
  socket read and write and the only workspace dependency tree containing a
  native I/O runtime. The Miri and AddressSanitizer job definitions now state
  which crates each is for and why `blazingly-native` cannot run under Miri.

### Known gaps

- The committed submodule pointer for `crates/blazingly-contract` is
  `02b2fae` (`version = "0.0.1"`), while `[workspace.dependencies]` requires
  `blazingly-contract = "0.3.0"`. A fresh `git clone --recursive` therefore
  does not build; the working tree is on `v0.3.0` (`b1b4b91`) and that pointer
  has to be committed.
- The `80,000` req/s validated-scenario acceptance gate is not met, and Actix
  Web leads on both throughput and tail latency. See
  [docs/benchmark-status.md](docs/benchmark-status.md).
- `repository` in `[workspace.package]` is a placeholder and must be confirmed
  before the first publish.
- HTTP/2 request-upload streaming, TLS WebSocket upgrades, JWKS/OIDC discovery,
  key rotation, CSRF helpers, and server-side session stores are not
  implemented.
- The pinned HTTP/2 codec is a canary release.
- No independent external security audit has been performed.
