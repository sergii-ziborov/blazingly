# Competitive and platform roadmap

This document is the delivery plan for closing Blazingly's observable gaps
against FastAPI, Axum, and Actix Web, and for evaluating Apple Silicon,
Raspberry Pi, ESP-IDF, and Weavatrix integration. It records acceptance gates,
not shipped features or performance promises.

The architectural constraints in [architecture](architecture.md) remain in
force: the operation contract is canonical, contract/core/executor stay
runtime-neutral, and the native server remains free of Tokio, Hyper, and Axum.
Release claims remain subject to [stability and SemVer](stability.md).

## Evidence boundary

The latest matched validated-operation result recorded in
[benchmark status](benchmark-status.md) measured 65,650 requests/second for
Blazingly, 47,174 for Axum, 74,886 for Actix Web, and 3,646 for FastAPI on one
busy Windows loopback host. It is evidence for that scenario only: it neither
establishes a universal Axum win nor an Actix Web win for Blazingly. The
80,000 requests/second project gate was not met, and the tail-latency,
allocation, energy, and cross-machine matrices remain incomplete.

The same rule applies to efficiency. "Green computing" is a product goal.
Requests per watt, joules per successful request, or server-replacement claims
must not be published until the hardware and measurement gates below pass.

## Competitive priority matrix

| Priority | Surface | Competitor baseline | Current gap | Acceptance gate |
| --- | --- | --- | --- | --- |
| P0 | Request-aware dependency injection | FastAPI dependencies can declare request parameters and subdependencies | A `#[provider]` accepts only `Depends<T>` inputs, so providers cannot declare path, query, header, cookie, or request context inputs | Nested providers consume typed request inputs; those inputs appear once in the operation contract, OpenAPI, MCP metadata, and generated docs; overrides bypass the replaced provider and its inputs |
| P0 | Custom extraction | Axum exposes `FromRequest`; FastAPI permits direct request access | `FromInvocation` is public, but handler macros interpret an unknown bare type as a DI dependency and expose no explicit custom-extractor syntax | A downstream crate implements `FromInvocation` and uses `Extract<T>` in an HTTP handler; a borrowed raw request-parts extractor is available; unsupported transports reject deterministically |
| P0 | Reusable API composition | FastAPI `APIRouter` and Axum `Router` support prefixes, nesting, defaults, and reuse | Nested `Plugin`s have lexical DI and hooks but no mount prefix, explicit tags, default policies, or reusable route inclusion | The same module mounts under two prefixes without cloning handlers; duplicate paths and operation ids fail at build time; OpenAPI/MCP paths, tags, dependencies, and policies match the mounted graph |
| P0 | Reproducible runtime evidence | Axum and Actix Web publish broad runtime ecosystems and benchmarkable service paths | The validated HTTP/1 result is encouraging against Axum but behind Actix Web; most protocol and workload rows are unmeasured | The workload and measurement gates below pass on dedicated hardware; every public speed statement names its exact scenario and machine |
| P1 | Response contracts | FastAPI validates and filters response models | Rust types constrain shape, but generated field constraints are not validated when an application constructs a response | Invalid constrained output becomes a deterministic internal contract failure; valid output is serialized once; response validation has an explicit opt-out and measured cost |
| P1 | Polymorphic schemas | FastAPI/Pydantic model unions and discriminators | API models cover structs, value types, and string enums, but not data-carrying variants, unions, or discriminators | Tagged Rust enums project to OpenAPI 3.1 `oneOf` plus discriminator, round-trip through JSON, participate in compatibility reports, and share HTTP/MCP validation |
| P1 | Middleware composition | Axum uses Tower layers/services; Actix Web supports async middleware | Runtime-neutral HTTP middleware is synchronous and async plugin hooks are operation-specific | A runtime-neutral async layer contract supports route, router, and handler scope without imposing one executor; cancellation, backpressure, and response-finalization tests pass |
| P1 | Response ergonomics | FastAPI and Axum ship text, redirect, file, stream, and raw response paths | JSON, status, headers, streaming, upgrades, and template HTML exist, but common response types require more assembly | First-class text, redirect, file/range, and explicit raw responses have typed contracts, correct headers, OpenAPI projection where applicable, and slow-reader tests |
| P1 | OpenAPI expression | FastAPI exposes explicit tags, deprecation, callbacks, webhooks, servers, examples, and response metadata | Route metadata is primarily id and summary; tags are inferred from the operation-id namespace | Metadata is represented in the shared contract, survives compatibility comparison, and is rendered identically by OpenAPI/docs/MCP where meaningful |
| P1 | Production protocol/security contour | Axum and Actix Web have mature HTTP/2 and surrounding security integrations | HTTP/2 remains experimental; JWKS/OIDC discovery, key rotation, CSRF helpers, and TLS reload ergonomics remain follow-up work | HTTP/2 upload streaming, cancellation, limits, slow-peer behavior, and TLS ALPN pass conformance and load gates; security additions include expiry, rotation, redaction, and adversarial tests |

P2 work starts only after the relevant P0/P1 contract is stable. Examples are
additional database/queue adapters, convenience integrations, and device-
specific tuning packages. A larger crate count is not itself a completion
metric.

## Next P0 vertical slice: request-aware DI and `Extract<T>`

This is the next API slice after the current router, blocking-pool, database,
OpenAPI, and CLI changes complete their normal quality gates.

### Scope

1. Add an explicit `Extract<T>` handler wrapper backed by `FromInvocation`.
   Preserve bare `T` as the existing direct-DI spelling so this is additive.
2. Add a borrowed request-parts extractor for method, path, headers, peer,
   scheme, host, and extensions. Body ownership remains with body extractors.
3. Permit `#[provider]` inputs from `Depends`, `Path`, `Query`, `Header`,
   `Cookie`, and `Extension`. Body extractors are intentionally excluded from
   this first slice.
4. Fold provider-declared request inputs into each consuming operation during
   application compilation. Deduplicate the same logical input and fail on
   incompatible declarations.
5. Project the compiled inputs from the canonical operation graph to OpenAPI,
   MCP, generated documentation, and compatibility fingerprints.
6. Keep numeric DI slots and precompiled decode plans. The slice must not add a
   per-request type-name map, reflection, route scan, or second JSON value.

### Acceptance gates

- A provider consumes a query model and header while a nested provider consumes
  a cookie; HTTP success and every validation failure are integration-tested.
- OpenAPI contains each dependency-origin parameter exactly once with its
  required/default/validation schema; generated MCP/docs fixtures agree.
- Global and plugin-scoped test overrides can replace the provider without
  evaluating the original provider or requiring its request inputs.
- A custom extractor implemented outside the framework crate works through
  `Extract<T>` and can return a stable typed rejection.
- HTTP-only extraction attempted through MCP returns a documented transport
  mismatch rather than a panic or missing-dependency error.
- One- and ten-dependency external benchmarks report throughput, p50/p95/p99,
  allocations, and peak RSS. The ten-dependency path performs no per-request
  hash lookup and regresses the current one-dependency median by no more than
  5% before it is accepted.

Expected implementation areas are `blazingly-contract`, `blazingly-di`,
`blazingly-executor`, `blazingly-macros`, `blazingly-openapi`, and focused
facade integration tests. The operation graph changes before projections; no
projection may invent semantics absent from the contract.

## Performance and claim gates

### Functional equivalence

Every compared server must pass byte-equivalent fixtures for successful
responses, validation failures, authorization failures, response headers, and
configured limits before throughput is measured. A load sample with protocol
errors, incomplete responses, timeouts, or semantic mismatches is rejected.

### Measurement quality

- Use separate client and server machines for publishable network results.
- Pin versions, build release artifacts, warm each server, and run at least five
  interleaved samples per scenario.
- Record background CPU, worker count, core topology, clock/power mode,
  operating system, payload, connection count, and load-generator headroom.
- Report median throughput, p50/p95/p99/p99.9, error count, CPU time,
  allocations, peak RSS, startup time, binary size, full compile time, and
  incremental compile time.
- Keep in-process, loopback, and cross-machine results in separate tables.
- Store raw samples and the exact server/client commits with the report.

The existing 80,000 requests/second validated HTTP/1 gate remains a local
project gate. A scenario-specific "faster than Axum" or "faster than Actix Web"
claim additionally requires at least a 10% median throughput lead, no overlap
between the winner's slowest accepted sample and the competitor's fastest, and
p99 no worse than the competitor. If those conditions are absent, publish the
measurements without a ranking.

The required matrix includes static and parameter routing; small and large
JSON; validated input and output; 1/10 dependencies; 1/10 hooks; typed errors;
HTTP/1 keep-alive, chunked bodies, TLS, and pipelining; HTTP/2 multiplexing;
streaming response/upload with slow peers; WebSocket; SSE; MCP discovery and
tool calls; database-bound work; and overload/backpressure behavior.

## Apple Silicon and green-computing boundary

Apple Silicon work starts CPU-first. HTTP routing, extraction, JSON, TLS,
scheduling, and most database traffic are latency-sensitive CPU and I/O work;
moving them to a GPU or Neural Engine without a measured kernel usually adds
copying, batching, synchronization, and tail latency.

### Phase A: establish macOS evidence

- Implement and test macOS process CPU/RSS metrics before publishing Mac
  efficiency results.
- Use a dedicated 10 GbE load generator so a Mac mini is measured as the
  server, not as both client and server.
- Record Apple SoC, memory, macOS version, performance mode, ambient
  temperature, thermal throttling, fan state, and steady-state clock behavior.
- Measure whole-system wall power with an external meter. OS telemetry such as
  `powermetrics` is supporting evidence, not a substitute for wall power.
- Report total and idle-adjusted watts separately, plus requests/watt and
  joules per successful request at fixed p99 and error-rate targets.

### Phase B: CPU-first optimization

Profile worker placement across performance and efficiency cores, socket and
accept distribution, allocator behavior, copies, JSON/wire paths, TLS, DI,
cache locality, and backpressure. Changes must improve end-to-end results, not
only a microbenchmark. A Mac-specific scheduler or metrics implementation stays
inside a native adapter; no Apple type enters contract/core/executor.

### Phase C: accelerator eligibility

Metal, Core ML, MLX, or another GPU/NPU adapter is considered only for an
application workload with a separately identifiable offloadable kernel, such as
model inference or large batch/vector work. An accelerator path is accepted
only when profiling shows that kernel consumes at least 20% of request CPU
time, and a prototype including transfer and batching overhead improves either
steady-state throughput or joules/request by at least 10% without worsening
p99 by more than 5%. It must retain a CPU fallback and bounded queues.

No claim that a 155 W Mac mini replaces a 10 kW server is permitted from
nominal power or synthetic router throughput. Such a comparison requires the
same complete application, dataset, availability model, network, storage,
latency SLO, and sustained-load test on both systems.

## ESP-IDF and Raspberry Pi architecture

ESP-IDF and Raspberry Pi are different products, not two feature flags over the
same socket adapter.

| Boundary | ESP-IDF target | Raspberry Pi target |
| --- | --- | --- |
| Operating environment | ESP-IDF/FreeRTOS on a supported microcontroller, with device-specific networking and strict memory limits | Linux/aarch64, using the normal Rust standard library and the existing native architecture where dependencies support it |
| Framework shape | A deliberately bounded embedded subset and separate adapter; no Compio assumption | The full operation model with a Pi-tested native build and optional tuning/deployment profile |
| Contract/docs | Contracts and OpenAPI can be generated at build time or off-device; serving interactive docs is optional | Normal OpenAPI, docs, MCP, middleware, observability, and security subject to measured resource limits |
| Runtime resources | Fixed capacities for routes, headers, body, connections, queues, and timeouts; watchdog-safe execution; no unbounded buffering | Bounded production defaults sized for Pi memory and cores; ordinary streaming/backpressure model |
| Ecosystem | Device services behind explicit adapters; database, templates, dynamic plugins, and general process control are out unless separately proven | SQLite and network services may use existing adapters; deployment should prefer systemd/container profiles based on evidence |
| Candidate package | `blazingly-esp-idf`, only after the feasibility gate | Prefer an aarch64 support profile in the main/native packages; create `blazingly-pi` only if reusable Pi-specific code justifies a package |

The ESP-IDF feasibility gate is a real-device build and a socket-level typed
operation with validation, limits, deterministic out-of-memory handling,
watchdog survival, reconnect behavior, and a 24-hour soak. The report records
flash, static RAM, peak heap, connection limit, throughput, latency, and power.
Failure of that gate means the framework remains unsuitable for ESP-IDF rather
than silently weakening its guarantees.

The Raspberry Pi gate runs the shared conformance suite on supported Pi
hardware, then a 24-hour HTTP/MCP soak and the performance matrix at at least
two concurrency levels. It records model, RAM, storage, cooling, throttling,
kernel, peak RSS, watts, requests/watt, and p99. A public Pi-specific repository
is created only when the spike produces code that cannot live cleanly in the
main native/deployment packages.

## Weavatrix MCP control plane

Weavatrix integration is an adapter over Blazingly's existing operation graph,
not a second application model and not a privileged back door. The framework
must remain usable without Weavatrix, and the control plane must not add a
runtime dependency to contract/core/executor.

### Stage 0: capability and threat contract

Define stable capability ids, read/write classification, required authority,
confirmation policy, input/output schemas, redaction rules, audit fields,
timeouts, cancellation behavior, and idempotency. Secrets are referenced by
opaque handles and never returned through MCP, logs, OpenAPI, or generated
documentation.

Gate: a checked-in threat model and contract fixtures cover unauthorized calls,
tenant/scope isolation, replay, cancellation, oversized input/output, slow
clients, audit redaction, and confirmation bypass attempts.

### Stage 1: read-only administration

Expose operation discovery, contract fingerprints, route and plugin topology,
health/readiness, redacted effective configuration, feature inventory,
dependency status, and bounded metrics snapshots. Pagination and output budgets
are mandatory for every collection.

Gate: HTTP/OpenAPI/MCP discovery derives from the same application definition;
Inspector/subprocess tests verify pagination, malformed frames, cancellation,
timeouts, and redaction without changing server state.

### Stage 2: bounded operational actions

Add explicitly authorized actions such as graceful drain/shutdown, safe
configuration or certificate reload, queue pause/resume, and migration status.
Each action is typed, cancellable where safe, idempotent or protected by an
operation token, and emits an immutable audit record. Destructive or externally
visible operations require confirmation and expose a dry-run when meaningful.

Gate: concurrent/replayed calls cannot execute an action twice, partial failure
is reported with recovery instructions, and overload cannot starve the data
plane. There is no arbitrary shell, file, SQL, or Rust-code execution tool.

### Stage 3: planned changes and application generation

Weavatrix may propose routes, models, policies, deployments, and compatibility
changes from the canonical graph. Generation writes to a reviewable workspace,
runs the normal format/test/contract gates, and returns a diff and evidence; it
does not deploy, publish, commit, or push implicitly.

Gate: generated changes preserve stable operation ids, pass compatibility and
transport conformance, include tests, and require a separate authorized action
for commit, deployment, migration, or publication.

### Stage 4: fleet control plane

Only after stages 0-3 are stable may a separate `blazingly-weavatrix` package
coordinate multiple instances. Fleet actions need per-instance identity,
bounded concurrency, deadlines, resumable status, rollback or forward-recovery
plans, and an audit trail that survives controller restart.

Gate: canary, partial-outage, stale-controller, duplicate-delivery, and
credential-rotation exercises pass without losing data-plane availability.

## Delivery order

1. Finish and validate the current contract/OpenAPI/router/DI/blocking/database/
   CLI batch; update its benchmark baseline before attributing a speed change.
2. Deliver request-aware DI and `Extract<T>` as the next P0 vertical slice.
3. Add reusable API composition, response validation, and polymorphic schemas
   through the shared contract.
4. Close the benchmark matrix and tail-latency investigation in parallel with
   macOS process and power measurement support.
5. Start Apple CPU tuning, Raspberry Pi conformance, and the ESP-IDF feasibility
   spike only after their measurement harnesses exist.
6. Deliver Weavatrix stages incrementally, beginning with read-only discovery;
   do not bundle write authority with initial integration.

Every row moves from roadmap to shipped documentation only after its functional,
adversarial, and measurement gates pass. A passing unit test, microbenchmark,
Git tag, or generated project alone is not completion evidence.
