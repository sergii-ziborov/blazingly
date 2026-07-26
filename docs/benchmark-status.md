# Benchmark status

An executable socket harness now lives in the separate
`blazingly-benchmarks` repository. This keeps Axum's Tokio/Hyper stack, Actix
Web, Node.js, and future Python dependencies out of the framework workspace.

The first Windows development checkpoint compared one-worker typed JSON over
real HTTP/1 sockets with 128 persistent connections for 10 seconds. One
observed run produced 23,268.66 req/s for the former async-net Blazingly
adapter, 23,031.95 for Axum 0.8.9, 20,879.53 for Actix Web 4.14.0, and
11,704.67 for Fastify 5.6.2, with zero errors. That adapter has since been
replaced by Compio and the numbers are historical, not current framework
results.

Runtime-isolation controls on the same machine reached roughly 74,383 req/s for
a minimal four-worker Compio server and 65,400 req/s for an experimental
four-worker Blazingly pipeline. Repeated runs degraded under machine load, and
the production `MulticoreServer` now replaces the experimental harness.

Per-worker counters then exposed a long-lived connection scheduling problem:
one shared dispatcher produced request counts of
`[55,214, 27,747, 44,790, 236]`. The production launcher now owns one
single-thread dispatcher per worker and assigns accepted connections explicitly
round-robin. A diagnostic run after the change produced
`[122,702, 126,577, 92,817, 90,919]`.

Three clean transport-microbenchmark samples after that fix produced
100,381.11, 87,322.77, and 104,253.61 req/s with four workers, 128 connections,
and zero errors (median 100,381.11). This crosses 80k for the narrow typed JSON
transport workload. It does not close the acceptance gate below, which requires
validation, DI, and authorization too.

Those samples also omitted the HTTP `Date` response header emitted by the Axum
and Actix baselines. The native adapter now emits a cached `Date` header without
a per-request clock syscall, but post-change comparison attempts coincided with
31-100% unrelated host CPU load and are invalid. The benchmark runner now
records a three-sample CPU preflight and can reject a busy host. No public
Blazingly-versus-Axum/Actix win is claimed until the equivalent-wire run is
repeated on an idle host.

## Fair baselines

| Target | Required baseline |
| --- | --- |
| Blazingly | shared executor plus the transport being measured |
| Axum | `Json<T>`, equivalent validation, middleware, and typed JSON response |
| Actix Web | `web::Json<T>`, equivalent validation, middleware, and typed JSON response |
| Fastify | Ajv input schema and response schema serialization |
| FastAPI | Pydantic request/response models and equivalent dependencies |

Versions must be locked in the benchmark repository and reported with every
result.

## Workload matrix

| Workload | Blazingly status | Benchmark status |
| --- | --- | --- |
| Plaintext HTTP | Compio HTTP/1 plus balanced multicore launcher and cached `Date` implemented | narrow transport median crossed 80k before equivalent-wire rerun; strict idle-host matrix pending |
| HTTP/1 chunked request | implemented with decoded-size/chunk limits | scenario missing |
| TLS | optional Compio/rustls adapter implemented | handshake/throughput scenarios missing |
| HTTP/2 | experimental Sans-I/O adapter implemented | multiplexing conformance only; benchmark missing |
| Small/large JSON HTTP | borrowed native request and single response serialization | small typed JSON baseline runs |
| Validated JSON operation | implemented in the shared executor | equivalent Blazingly/Axum/Actix/Fastify/FastAPI scenario implemented; clean-host matrix pending |
| Typed domain error | implemented in the shared executor | harness missing |
| Path/query/header extraction | implemented with typed multiple arguments | harness missing |
| 1/10 dependencies | compiled numeric per-operation plans; inline slots for small graphs | one dependency/state scenario implemented; 10-dependency case missing |
| Authorization | typed header plus shared error projection | bearer-header scenario implemented; clean-host matrix pending |
| 1/10 hooks | inherited compiled async hooks implemented | harness missing |
| MCP discovery | implemented | harness missing |
| MCP tool call | implemented through JSON-RPC and stdio | harness missing |
| Streaming response | runtime-neutral pull stream; HTTP/1 chunked and HTTP/2 DATA framing | throughput, slow-reader, and producer-failure scenarios missing |
| Streaming upload | buffered extractors with decoded-size limits | zero-copy upload contract and benchmark missing |

The primary application benchmark remains:

```text
JSON parse
  + validation
  + dependency
  + authorization
  + handler
  + typed serialization
```

Hello-world routing is a secondary microbenchmark.

## Acceptance gates

- `80,000` comparable validated JSON requests/second is the minimum native
  adapter acceptance gate, not a marketing result.
- A public "faster than Axum/Actix" statement requires reproducible wins for
  equivalent routing, extraction, validation, handler, and serialization
  workloads, including p95/p99 and allocations.
- Million-request-per-second experiments are a stretch profile. They must
  report cores, connection count, payload size, load-generator headroom, NIC,
  operating system, and whether the number is in-process or socket-level.
- A regression that adds route scanning, operation-id lookup, an intermediate
  HTTP JSON value, double response serialization, or per-request DI lookup
  fails the performance contract even before the native benchmark exists.

## Measurement contract

- Send identical payloads and require equivalent status codes and response
  bodies.
- Measure release builds after warmup.
- Store raw samples and report medians plus p50, p95, and p99.
- Keep end-to-end socket measurements separate from in-process executor
  measurements.
- Compare throughput and latency only within the same operating system and
  machine class.
- Also report allocations, peak RSS, startup time, binary size, full compile
  time, and incremental compile time.
- Benchmark MCP discovery separately from `tools/call`.
