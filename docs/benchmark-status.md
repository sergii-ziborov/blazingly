# Benchmark status

An executable socket harness now lives in the separate
`blazingly-benchmarks` repository. This keeps Axum's Tokio/Hyper stack, Actix
Web, Node.js, and future Python dependencies out of the framework workspace.

Current status: the `80,000` req/s acceptance gate has no qualifying run yet.
The qualifying 2026-07-27 matrix measured 65,650 req/s behind Actix Web; a
2026-08-03 single-sample checkpoint taken after the routing/pool/wire/json
optimization wave crossed the gate at 92,140 req/s ahead of Actix Web, but a
single noisy-host sample earns nothing under the measurement contract. The
idle-host, multi-sample, interleaved rerun is the outstanding gate-closer.
Everything between here and those sections is a historical checkpoint or a
transport microbenchmark.

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

## 2026-07-27 pipelined validated diagnostic

The high-load client now verifies every HTTP status and `Content-Length` while
keeping a configurable number of requests in flight on each connection. On the
same Windows 11 loopback machine, with four server workers, 32 connections,
pipeline depth 16, and a five-second sample, the native adapter produced:

| Native write mode | Requests/second | Errors | Host CPU before launch |
| --- | ---: | ---: | ---: |
| one response per write | 120,375.90 | 0 | 28.9% |
| bounded batch, at most 16 responses | 760,329.74 | 0 | 29.8% |

This matched-load change is a 6.32x improvement. It comes from bounded
response-write coalescing on top of direct plaintext Compio I/O, not from
skipping parsing, validation, authorization, dependency injection, handler
execution, or typed serialization.

The same client/configuration observed 602,261.85 req/s for Actix Web and
100,738.21 req/s for Axum, both with zero errors. A larger Blazingly stretch
sample reached 1,016,765.53 req/s with eight workers, 64 connections, and
pipeline depth 32. The corresponding competitor samples ran under materially
different host preflight load, so that larger set is not a fair ranking.

The ordinary single-in-flight `go-wrk` sample remained 74,781.37 req/s. The
6.32x result therefore applies specifically to high-load HTTP/1 pipelining; it
is not presented as a universal per-request or latency improvement. All values
are local engineering diagnostics, not publishable cross-platform claims. Raw
evidence and the exact comparison table live in the benchmark repository.

## 2026-08-04 tail attribution, first ladder

Which layer owns the p99? Three samples survived a short quiet window on the
loopback host (pipeline client, depth 1, 128 connections, 4 workers, json
scenario; every other sample of the series was rejected by the busy-host
preflight):

| Rung | Background | req/s | p99 | p99.9 | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| Bare compio loop, no framework | 23.5% | 122,707 | 3.58ms | 6.20ms | 10.9ms |
| + framework dispatch (minimal transport), run 1 | 14.2% | 103,719 | 5.83ms | 34.6ms | 188.6ms |
| + framework dispatch (minimal transport), run 2 | 28.3% | 78,848 | 5.95ms | 9.04ms | 15.1ms |

Findings, in confidence order:

- **The environment floor dominates p99.** A canned-response compio loop with
  zero framework code already spends 3.6ms at p99 on this host — roughly the
  same order as the full framework's 4.33ms p99 in the 2026-08-03 checkpoint.
- **Framework heap pressure is exonerated.** The new counting-allocator audit
  (`blazingly-alloc-audit` in the benchmark repository) measures the whole
  dispatch path — handler, compiled executor, full borrowed HTTP dispatch —
  at exactly 2 heap allocations and 133 bytes per request in steady state:
  the handler's own `String` and the response body. There is nothing left to
  cut on that axis.
- **The dispatch rung's own glue was contaminated.** The minimal-transport
  server assembled its response head through `core::fmt` per request — the
  formatting machinery `blazingly-wire` removed after measuring it 7.8x over
  the manual floor. The rung now encodes its head as bytes; the +2.3ms p99
  delta it showed over the floor must be re-measured before any of it is
  attributed to the framework libraries.
- The one 188.6ms outlier at 14% background remains unexplained and is the
  open tail question for the rerun.

**2026-08-04 evening: the priority lever, isolated and measured.** With the
host briefly quiet (10-18% background on every sample), three interleaved
rounds of base / elevated-priority / elevated-plus-1ms-timer produced, in
medians of three: p99 2.284ms -> 2.033ms (-11%), p99.9 3.799ms -> 2.815ms
(-26%), max -13%, throughput +2% — the elevated rounds beat their own round's
base three times out of three. The 1ms Windows timer interrupt added nothing
on top, which refutes the timer-quantum hypothesis for this completion-driven
server and keeps the shipped lever fully portable:
`MulticoreServer::with_worker_priority(WorkerPriority::Elevated)`.

A same-window interleaved head-to-head (three rounds, json, depth 1) then
measured medians of: Blazingly-elevated 119,621 req/s, p50 1.073ms, p99
2.497ms, p99.9 3.948ms; Actix Web 114,502 / 1.119ms / 2.211ms / 3.081ms
(its first round carried a background burst and read 5.6ms/12.7ms); Axum
78,807 / 1.490ms / 3.855ms / 4.906ms. Read plainly: Blazingly leads
throughput and p50 and now leads Axum on every metric including both tail
percentiles; Actix Web's clean samples still lead p99/p99.9 by roughly
10-25% — a sub-millisecond gap where a 1.7-3.2x gap stood a week ago — and
Blazingly's isolated-run best (p99 2.005ms, p99.9 2.716ms) touches Actix
Web's numbers. Three samples per side earn no ranking claim under this
document's rules; they set the next target: close the last fraction of a
millisecond to Actix Web's tail on a properly idle host.

**2026-08-04 late: the tail split at the socket write, in production code.**
With `BLAZINGLY_NATIVE_STAGE_METRICS=1`, the native loop's own histograms
over 786,432 keep-alive cycles (elevated priority, 116,760 req/s, client p99
2.609ms / p99.9 3.795ms, 22% background) split each cycle at the flush:
`service` — head parsed to response flushed, everything this framework does —
measured p50 &le; 16us, p99 &le; 65us, p99.9 &le; 131us; `wait` — response
flushed to the next head parsed, the peer's turnaround plus the kernel plus
the wake back onto the worker — measured p50 &le; 524us, p99 &le; 2.1ms.
Against a local pipeline client whose turnaround is microseconds, the wait
side is the kernel and scheduler. That closes the attribution opened this
morning with numbers at every level: the framework's share of a
millisecond-scale tail is roughly a twentieth, and further tail work is
driver, kernel, and scheduling work — worker priority (shipped), completion
batching, and eventually io_uring-class submission on the platforms that
offer it. The accept loop also now places each connection on the
least-loaded worker rather than rotating blindly; on this symmetric workload
it measured neutral, as designed — its value is skew robustness.

**Same-day verdict via thread cycles.** Wall-clock percentiles on a shared
host measure the Windows scheduler, so the audit also recorded
`QueryThreadCycleTime` around 200,000 full dispatches — thread cycles do not
advance while the thread is preempted, making the histogram immune to
background load. Measured: p50 3,264 cycles, p99 6,202, p99.9 63,648,
p99.99 111,955, max 241,210 — about 16 microseconds of the framework's own
CPU work at p99.9 and 60 microseconds at the absolute worst over 200,000
requests. Together with the 2-allocation audit, this acquits the request-path
libraries (`wire`, `json`, routing, executor, dispatch) of the
millisecond-scale socket tail by three orders of magnitude. The tail budget
lives between socket readiness and the framework being scheduled: the
adapter/runtime wake and completion path (`blazingly-native` + Compio + the
operating system), which is where tail optimization work goes next. The
`nightly-matrix.ps1` script remains available for an unattended interleaved
ladder when a quiet host exists; the scheduled task for it was removed by
request.

## 2026-08-03 single-sample checkpoint after the optimization wave

One 5-second pipeline-client sample per framework (depth 1 — one request in
flight per connection), four workers, 128 connections, on the same Windows 11
loopback host. Background CPU varied between 10% and 27% across launches, and
one sample is not a median, so this is a development checkpoint, not a
result: it does not supersede the 2026-07-27 matrix below and earns no claim
under the acceptance gates. What it records is the direction after the
routing, blocking-pool, wire, and json changes landed:

| Framework | Requests/second (1 sample) | p50 | p99 | p99.9 |
| --- | ---: | ---: | ---: | ---: |
| Blazingly | 92,140 | 1.266ms | 4.330ms | 7.698ms |
| Actix Web | 88,742 | 1.396ms | 4.080ms | 7.475ms |
| Axum | 71,373 | 1.639ms | 4.585ms | 6.266ms |
| Bun | 33,375 | 3.667ms | 6.399ms | 8.178ms |
| Fastify | 14,159 | 7.438ms | 33.609ms | 46.778ms |

Read plainly: the sample crossed the 80,000 req/s gate and finished ahead of
Actix Web on throughput and p50, and the p99.9 gap to Actix Web closed from
3.16x to about even — but a 3.8% lead from one noisy sample is far inside the
10%/no-overlap bar this document sets for any ranking claim, Actix Web's
launch happened under the worst background load of the run, and Axum leads
both tail percentiles. The gate-closing measurement remains the idle-host,
multi-sample, interleaved rerun.

The same day's in-process layer profile (1,000,000 iterations per layer):
httparse 116.1 ns, static router 52.5 ns, handler plus typed serialization
271.6 ns, compiled executor 429.0 ns, full borrowed HTTP dispatch 993.5 ns —
about 1.0M dispatches per second per core before sockets.

## 2026-07-27 competitor matrix, validated scenario

This was the previous headline result. Four server workers, 128 connections,
one request in flight per connection, on the same Windows 11 loopback host.
The host was not idle. Medians of the sampled runs:

| Framework | Requests/second (median) | p50 | p99 | p99.9 |
| --- | ---: | ---: | ---: | ---: |
| Actix Web | 74,886 | 944us | 2.093ms | 3.197ms |
| Blazingly | 65,650 | 1.028ms | 3.631ms | 10.101ms |
| Axum | 47,174 | - | - | - |
| FastAPI | 3,646 | - | - | - |

Read plainly:

- **The `80,000` req/s acceptance gate is NOT met.** Blazingly reached 65,650
  req/s, 82% of the gate. Closing it requires a further 21.9%.
- **Actix Web is ahead of Blazingly on both throughput and tail latency.** It
  is 14.1% faster, its p99 is 1.73x lower, and its p99.9 is 3.16x lower.
  Blazingly's p99.9 of 10.101ms is the worst number in this table.
- Blazingly is ahead of Axum by 39.2% and ahead of FastAPI by 18.0x. Beating
  Axum and FastAPI does not substitute for the two statements above.
- The host was not idle, and no competitor reached 80,000 req/s on it. That is
  context for a rerun, not a defence of the result: the gate is absolute, the
  run was matched-load, and Actix Web won it on the same machine at the same
  time.

No "faster than Actix Web" statement may be made. The idle-host rerun, the
allocation and RSS figures required by the measurement contract below, and a
tail-latency investigation are all outstanding.

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
| Plaintext HTTP | direct Compio HTTP/1 plus balanced multicore launcher, cached `Date`, and bounded pipelined-response coalescing implemented | validated pipeline crossed 1M in a stretch sample; strict idle-host matrix and latency percentiles pending |
| HTTP/1 chunked request | implemented with decoded-size/chunk limits | scenario missing |
| TLS | optional Compio/rustls adapter implemented | handshake/throughput scenarios missing |
| HTTP/2 | experimental Sans-I/O adapter implemented | multiplexing conformance only; benchmark missing |
| Small/large JSON HTTP | borrowed native request and single response serialization | small typed JSON baseline runs |
| Validated JSON operation | implemented in the shared executor | 2026-07-27 matrix run: 65,650 req/s median, behind Actix Web on throughput and tail latency, acceptance gate not met; idle-host rerun pending |
| Typed domain error | implemented in the shared executor | harness missing |
| Path/query/header extraction | implemented with typed multiple arguments | harness missing |
| 1/10 dependencies | compiled numeric per-operation plans; inline slots for small graphs | one dependency/state scenario implemented; 10-dependency case missing |
| Authorization | typed header plus shared error projection | bearer-header scenario runs inside the 2026-07-27 validated matrix; idle-host rerun pending |
| 1/10 hooks | inherited compiled async hooks implemented | harness missing |
| MCP discovery | implemented | harness missing |
| MCP tool call | implemented through JSON-RPC and stdio | harness missing |
| Streaming response | runtime-neutral pull stream; HTTP/1 chunked and HTTP/2 DATA framing | throughput, slow-reader, and producer-failure scenarios missing |
| Streaming upload | `UploadBody` plus the `UploadBody::into_multipart` reader, which yields fields and borrowed chunks over the adapter's stream under its existing decoded-size, chunk-count, and read-deadline limits; `Multipart<T>` and `File<UploadFile>` still buffer, by design | 5 MiB multipart scenario runs; see "2026-07-29 streaming multipart upload" below |

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

## 2026-07-29 streaming multipart upload

The upload scenario posts one 5 MiB `multipart/form-data` cover to
`POST /admin/articles/1/cover` from the `blazingly-apibench` harness. Three
servers were interleaved sample by sample, one round each in turn, and all
three were confirmed to answer the same body before anything was measured. The
only difference between the two Blazingly rows is the handler: one uses the
buffered `File<UploadFile>` extractor, the other `UploadBody::into_multipart`.

Peak RSS is the median of the samples in each cell — five at 8 and 32
connections, three at 64. A repeat of the eight-connection block with seven
rounds agreed within 1 MiB on every row. The idle floor is the same four-worker
process measured after startup and before any load, so the last column is what
the uploads themselves cost: Blazingly idles at 15.6 MiB and Axum at 15.0 MiB.

| Server | 8 conns | 32 conns | 64 conns | 8 → 64 |
| --- | --- | --- | --- | --- |
| Blazingly, `File<UploadFile>` | 106.2 MiB | 299.9 MiB | 556.9 MiB | 5.2x |
| Blazingly, streaming multipart | 25.8 MiB | 25.9 MiB | 26.5 MiB | 1.03x |
| Axum, streaming multipart | 34.2 MiB | 67.8 MiB | 105.9 MiB | 3.1x |

Above each server's own idle floor, the streaming reader costs 10.2, 10.3, and
10.9 MiB at the three concurrencies. Eight-folding the connections leaves its
working set where it was, so peak resident memory has stopped scaling with the
requests in flight — which is the property the row above claims and the only
one this run establishes. Nothing but the transport chunk in hand and a
delimiter of look-ahead is held per request, whatever the upload's size.

Throughput from this run is **not** comparable and is not reported as a
result: the host carried 55–86% background CPU from unrelated work throughout,
and every implementation's own best-to-worst spread across samples exceeded the
gaps between implementations. A quiet-host rerun is required before any
throughput claim is made about the streaming path.

## Acceptance gates

- `80,000` comparable validated JSON requests/second is the minimum native
  adapter acceptance gate, not a marketing result. **Status as of 2026-07-27:
  not met.** The validated scenario measured 65,650 req/s.
- A public "faster than Axum/Actix" statement requires reproducible wins for
  equivalent routing, extraction, validation, handler, and serialization
  workloads, including p95/p99 and allocations. **Status as of 2026-07-27: not
  earned against Actix Web**, which leads on throughput and on every measured
  latency percentile.
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
