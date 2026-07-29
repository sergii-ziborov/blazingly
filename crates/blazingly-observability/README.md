# blazingly-observability

Request identity, access events, tracing with OpenTelemetry propagation, and
Prometheus metrics for
[Blazingly](https://github.com/sergii-ziborov/blazingly) HTTP applications.

`Observability` is a `blazingly_http::HttpMiddleware` layer that mints
request ids (spoofing-safe by default: an inbound `x-request-id` is only
trusted from configured proxy peers), propagates W3C `traceparent` and
`tracestate`, records access events to a pluggable sink, and serves the
Prometheus exposition at a configurable path. `Metrics` is an ordinary
registry — counters, gauges, and histograms with bounded label cardinality,
plus process CPU and memory gauges — and works with no HTTP or framework
involvement at all, as the example shows. The optional `otel` feature adds
OTLP/HTTP span export without pulling an async runtime into the tree. The
crate is usable standalone with any `blazingly-http` dispatch path
(`TestApp`, `HttpApp`, or the native server); the facade re-exports it as
`blazingly::observability` (a default feature, with `observability-otel`
enabling export).

```rust
use blazingly_observability::{MetricError, Metrics};

fn main() -> Result<(), MetricError> {
    let metrics = Metrics::new();
    metrics.register_counter("jobs_processed_total", "Jobs finished by the worker")?;
    metrics.increment_counter("jobs_processed_total", &[("queue", "default")])?;
    assert_eq!(
        metrics.counter_value("jobs_processed_total", &[("queue", "default")]),
        Some(1)
    );
    assert!(metrics.prometheus().contains("jobs_processed_total"));
    Ok(())
}
```

## Links

- [API documentation](https://docs.rs/blazingly-observability)
- [Getting started](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md) — the framework picture
- [Repository](https://github.com/sergii-ziborov/blazingly)
