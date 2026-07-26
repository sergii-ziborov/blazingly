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

This repository contains only the framework and its lockstep crates. It does not
contain Mesh planning, Cloudflare placement, Worker RPC, deployment generation,
or graph partitioning.

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

The shared crates keep this boundary. `blazingly-native` is an optional adapter
using Compio, futures-I/O compatibility, and `httparse`; its dependency tree
contains no Tokio, Hyper, or Axum. Each worker owns a local compiled app, so
handlers and request-scoped DI do not acquire unconditional `Send + Sync`
bounds. HTTP/1 and experimental HTTP/2 both feed a borrowed request view into
the same compiled router and executor used by `TestApp`.

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
   extraction: done for buffered requests.
3. Typed success/error/header pipeline and pull-based streaming responses:
   done.
4. HTTP/MCP conformance, Streamable HTTP, resources/prompts, and supervised
   stdio: done.
5. Plugin scopes, test overrides, cancellation/timeouts, async
   providers/finalizers, full lifecycle hooks, and compiled DI: done.
6. Tokio-free native HTTP/1, chunked requests, limits, TLS, graceful shutdown,
   cached `Date`, and balanced multicore launcher: implemented; benchmark
   hardening remains.
7. HTTP/2: experimental prior-knowledge and TLS-ALPN adapter implemented;
   concurrent handler scheduling and production codec stabilization remain.
8. Reproducible competitor benchmarks: provisional diagnostics exist, final
   fair multicore matrix remains.

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

The native HTTP/1 adapter supports persistent Content-Length and chunked
requests, sequential in-order pipelining, bounded headers/bodies/chunk counts,
borrowed target/header/body views, optional rustls TLS, graceful drain, and a
thread-per-core launcher with one compiled app per worker. HTTP/2 supports
multiplexed wire streams but currently dispatches completed operations
sequentially within one connection; concurrent per-stream handler execution is
still required before calling it production-ready.

Streaming request uploads still need a transport-neutral extractor contract.
They must not be added by leaking native runtime stream types into handler
signatures.
