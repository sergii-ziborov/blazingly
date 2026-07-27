# Blazingly architecture

## Product boundary

Blazingly is a FastAPI-style Rust framework:

- handler signatures are the source of truth;
- Rust models define parsing, validation, schemas, and generated documentation;
- typed responses and errors define the complete public operation contract;
- MCP exposure, risk, confirmation, and idempotency are operation semantics,
  not data reconstructed from OpenAPI;
- AI-oriented Markdown is generated directly from the same operation model;
- the framework owns routing, extraction, validation, dependency injection,
  plugin scopes, lifecycle, error handling, testing, and documentation.

Blazingly is not an Axum wrapper and does not expose an Axum-compatible internal
model.

## Current repository boundary

This repository contains only the framework and its lockstep crates.
`blazingly-deploy` may generate a single-application container and Kubernetes
starter, and `blazingly-docs` composes those files into a project scaffold.
Neither crate contains Mesh planning, Cloudflare placement, Worker RPC,
multi-service deployment planning, or graph partitioning.

Mesh capabilities may later consume a stable Blazingly operation contract from
a separate repository. They must not determine the first framework API.

## Runtime boundary

The shared contract, model, macros, application graph, and handler API must not
depend on:

- Tokio;
- OS sockets or filesystem APIs;
- a specific HTTP server;
- unconditional `Send + Sync` bounds;
- Cloudflare or Workers APIs.

The shared crates keep this boundary. `blazingly-wire` owns HTTP/1 parsing,
request framing, chunk decoding, and response-head encoding with only
`httparse` as a dependency. It imports no Blazingly contract, router, executor,
OpenAPI, MCP, socket, or runtime crate. Both `blazingly-native` and the
standard-library `blazingly-wire-standalone` binary consume it.

`blazingly-native` is an optional adapter using Compio and futures-I/O
compatibility; its dependency tree contains no Tokio, Hyper, or Axum. Each
worker owns a local compiled app, so handlers and request-scoped DI do not
acquire unconditional `Send + Sync` bounds. HTTP/1 and experimental HTTP/2
both feed a borrowed request view into the same compiled router and executor
used by `TestApp`.

The future Cloudflare adapter sits beside `blazingly-native`, not underneath
it. It will consume `OperationDescriptor` and the runtime-neutral executor
without compiling Compio, sockets, TLS, or native HTTP codecs to Wasm.

## Contract boundary

`blazingly-contract` owns the portable semantic source of truth:

- versioned canonical encoding independent of declaration order;
- model/input/output shapes and nested collection item schemas;
- typed errors and response headers;
- dependencies and security requirements;
- MCP discovery metadata and agent risk/confirmation/idempotency policy;
- SHA-256 contract fingerprints;
- semantic compatibility changes classified as breaking, non-breaking, or
  informational.

It is maintained as an independent repository. During the pre-release phase,
the framework pins that repository as a Git submodule so every checkout and CI
run uses an exact reviewed contract revision. After the first registry release,
the framework will consume it as a normal SemVer dependency.

That pin is currently wrong. The committed submodule pointer is `02b2fae`,
whose `Cargo.toml` declares `version = "0.0.1"`, while the root
`[workspace.dependencies]` requires `blazingly-contract = "0.3.0"`. A fresh
`git clone --recursive` therefore fails to resolve the workspace. Local working
trees are on `v0.3.0` (`b1b4b91`) and build, which is why CI has not caught it.
Committing the updated pointer is a release-process step in
`docs/stability.md`.

The legacy single-input field is retained only for serialized-data migration
and is excluded from canonical identity. HTTP paths/methods remain projections
in core and therefore do not contaminate a service/RPC contract fingerprint.

## Performance boundary

Performance is an API constraint, not a cleanup phase. The production hot path
must preserve these invariants:

- routes are compiled once; requests never scan the operation list;
- a route resolves directly to a numeric executable-operation slot;
- static routes do not allocate;
- path, query, headers, and body enter the executor through borrowed views;
- middleware storage is empty by default and only allocates typed request
  extensions when a layer installs state such as an authenticated identity;
- HTTP JSON is decoded directly from request bytes without an intermediate
  `serde_json::Value`;
- a successful typed response is serialized once into final JSON bytes;
- dependency injection uses a compiled slot plan, never per-request type-name
  or hash-map lookup;
- OpenAPI, MCP, AI docs, and scaffold generation execute outside the request
  hot path.

`TestApp` and every future native adapter must use the same compiled routing and
execution plan. An adapter may provide a more specialized borrowed request
view, but may not introduce a second semantic pipeline.

## Source-of-truth flow

```text
Rust model + handler signature
            |
            v
     operation metadata
      /      / \       \
     v      v   v       v
validation HTTP OpenAPI MCP + AI Markdown
```

OpenAPI is an output. It is not interpreted at request time and is not used as
the primary internal schema.

MCP is a sibling projection of an operation, not an OpenAPI-to-MCP conversion.
The MCP transport can follow later, but its tool/resource semantics belong in
the initial operation descriptor.

## Product quality ladder

| Milestone | Meaning |
| --- | --- |
| Fast Rust router | 2/10 |
| FastAPI-style Rust framework | 6/10 |
| FastAPI-style framework with generated OpenAPI | 7/10 |
| Native MCP execution and AI-oriented docs | 8.5/10 |
| Excellent docs, stability, security, and conformance | 9/10 |

Schema generation alone does not satisfy the native MCP milestone. The current
vertical slice implements discovery, in-process tool invocation, the standard
MCP `CallToolResult` shape, resources, prompts, redacted audit, the JSON-RPC
lifecycle, stateful Streamable HTTP, and supervised bounded stdio. Both
transports sit over the same runtime-neutral MCP server.

A native MCP tool call must execute the same operation pipeline as HTTP:

```text
MCP call
  -> input decoding
  -> validation
  -> dependencies and authorization
  -> handler
  -> typed domain error or success
  -> agent-safe response projection
```

The MCP response must preserve stable domain error codes, redact internal
failures, respect output-exposure policy, and provide useful structured content
plus an agent-readable summary. HTTP and MCP conformance tests must prove that
both transports invoke the same operation semantics.

## Delivery order and current status

1. Runtime-neutral HTTP request/response, compiled router, and `TestApp`: done.
2. All HTTP methods and typed path/query/header/cookie/JSON/form/multipart/file
   extraction plus runtime-neutral `UploadBody`: done.
3. Typed success/error/header pipeline and pull-based streaming responses:
   done.
4. HTTP/MCP conformance, Streamable HTTP, resources/prompts, and supervised
   stdio: done.
5. Plugin scopes, test overrides, cancellation/timeouts, async
   providers/finalizers, full lifecycle hooks, and compiled DI: done.
6. Tokio-free native HTTP/1, direct plaintext Compio I/O, chunked requests,
   limits, TLS, graceful shutdown, cached `Date`, bounded pipelined-response
   coalescing, and balanced multicore launcher: implemented; benchmark
   hardening remains.
7. HTTP/2: experimental prior-knowledge and TLS-ALPN adapter with concurrent
   per-stream handlers and response-body polling implemented; request-upload
   streaming and production codec stabilization remain.
8. Reproducible competitor benchmarks: an equivalent Blazingly/Axum/Actix/
   FastAPI validated scenario now runs, but the result does not pass. The
   2026-07-27 matrix measured 65,650 req/s median for Blazingly against 74,886
   for Actix Web, with a worse tail at every percentile. The `80,000` req/s
   acceptance gate is not met and no "faster than Actix" claim is available.
   An idle-host rerun, allocation and RSS figures, and a tail-latency
   investigation remain. See `docs/benchmark-status.md`.
9. Single-application Kubernetes scaffold with direct/NGINX exposure, HPA,
   probes, disruption budget, and graceful `SIGTERM`: implemented.
10. Runtime-neutral CORS, compression, trusted host/proxy handling, bounded
    rate limiting, JWT/OAuth2/API-key/session verification, and typed security
    context: implemented; JWKS/OIDC discovery, key rotation, CSRF helpers, and
    server-side session stores remain.
11. SSE, HTTP/1 WebSocket upgrades over plaintext and TLS, lifespan/background
    tasks, a bounded blocking pool, and rich validation: implemented. Enabling
    the `http2` feature no longer removes upgrades or streaming uploads from
    plaintext connections.
12. Request IDs, access events, tracing/OpenTelemetry propagation, Prometheus
    metrics, and error counters: implemented.
13. `cargo blazingly` discovery/autoreload/production commands plus database,
    queue, template, and auth integration crates: implemented as the initial
    ecosystem surface; production vendor-specific adapters remain separate
    follow-up packages.
14. Fuzz targets, Miri, AddressSanitizer, dependency audit, security reporting,
    and SemVer checks/policy: configured. AddressSanitizer now covers
    `blazingly-native`; Miri cannot, because Compio reaches io_uring and IOCP
    through foreign functions Miri does not implement. CI also builds the
    meaningful feature subsets and runs rustdoc with `-D warnings`, and a
    workspace-wide `cargo-semver-checks` job reports Rust API breaks
    advisorily. An independent external security audit and a stable 1.0 API
    are explicit release gates.
15. Release readiness: repository-root MIT `LICENSE` matching the contract
    repository, publishable package metadata on every crate, `CHANGELOG.md`,
    and a release process in `docs/stability.md` whose gate names real CI
    jobs. `publish` stays `false`. Two blockers remain: the committed
    submodule pointer below, and a `repository` URL in `[workspace.package]`
    that is still a placeholder.

Mesh work starts only after the framework contract is stable.

The response/error stage includes typed `200`/`201`/`202`, arbitrary
`Status<CODE, T>`, bodyless `204`, validated response headers, unit and
payload-carrying `#[api_error]` variants, OpenAPI error envelopes, and matching
HTTP/MCP redaction. `StreamingBody` is a runtime-neutral pull stream. HTTP/1
requests the next chunk only after the previous async write, HTTP/2 emits DATA
frames, and exact-length mismatches terminate the wire response.

## Plugin scopes and compiled DI

`Plugin` scopes follow Fastify-style encapsulation:

- a child sees providers from its parents;
- a child may override a provider by output type;
- an override does not leak back to the parent or sideways to siblings;
- all routes and nested plugins are registered explicitly.

The DI compiler validates every registered provider graph before serving:
missing providers, cycles, duplicate providers in one scope, and singleton
dependencies on shorter-lived values are build errors. It then produces one
topologically sorted numeric plan per operation. Request execution performs no
type-name lookup, graph traversal, or hash-map lookup.

Provider lifetimes are:

- `Singleton`: initialized once while the executable app is built;
- `Request`: initialized at most once per operation invocation;
- `Transient`: initialized independently for each injection edge.

Request providers may declare reverse-order finalizers. Handler arguments can
use a directly cloned dependency handle or `Depends<T>` when shared ownership
is preferred. Fallible providers feed stable domain rejections into the same
HTTP/MCP error pipeline. The first eight request dependency slots use inline
storage; larger plans fall back to a heap slot array.

Request and transient providers may be synchronous or asynchronous. Request
providers may also declare synchronous or asynchronous finalizers. Plugin
`on_request`, `pre_parse`, `pre_validate`, `pre_handler`, `pre_serialize`,
`on_error`, and `on_response` hooks are inherited and compiled per operation;
the request path never walks the plugin tree. Shutdown cleanup runs child
scopes before parents.

The independent `blazingly-wire` codec supports persistent Content-Length and
chunked requests, bounded headers/bodies/chunk counts, borrowed target/header
views, request-smuggling checks, and safe response framing. The native adapter
adds sequential in-order pipelining, optional rustls TLS, graceful drain, and a
thread-per-core launcher with one compiled app per worker. Its plaintext HTTP/1
path uses Compio sockets directly, without a futures-I/O compatibility buffer,
and coalesces a bounded number of already ordered pipelined responses into each
write. TLS and experimental HTTP/2 retain the compatibility path. HTTP/2
supports concurrently scheduled handlers and pull-based response bodies on
independent streams. The pinned codec is still canary quality, and request
bodies remain buffered on HTTP/2 and the generic TLS compatibility path.

`blazingly-http` owns the adapter-neutral middleware hooks and request-local
extension carrier. `blazingly-middleware` implements HTTP policy and negotiated
compression; `blazingly-security` binds concrete credential verifiers to the
named schemes in each `OperationDescriptor`. Security runs after routing but
before body parsing, and authenticated state reaches handlers through
`Extension<SecurityContext>`. Native listeners attach the direct peer address
and actual transport scheme before trusted proxy normalization, so forwarded
headers are ignored unless the immediate peer matches configured CIDRs.

`UploadBody` is the transport-neutral request-stream extractor. Plaintext
native HTTP/1 dispatches after the request head and transfers Content-Length or
decoded chunked data through a bounded local channel, so socket reads follow
handler demand. `TestApp` provides a buffered semantic fallback. HTTP/2 and the
generic TLS compatibility path must adopt the same early-dispatch boundary
before they can claim streaming upload support.
