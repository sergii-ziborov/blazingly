# blazingly-native

Native Compio-based HTTP/1 server adapter for the
[Blazingly](https://github.com/sergii-ziborov/blazingly) framework, with
optional rustls TLS (`tls` feature) and an experimental HTTP/2 path (`http2`
feature).

The adapter deliberately owns every socket-runtime and wire-protocol
dependency, so the operation graph, router, DI, and documentation crates stay
runtime-neutral; Tokio, Hyper, and Axum are not in this crate's tree. Parsing
and framing come from `blazingly-wire`, sockets and completion I/O from
Compio. `Server` serves one compiled app; `MulticoreServer` runs
thread-per-core with one non-`Send` app per worker; `ServerLimits` enforces
header, body, chunk, pipeline, and socket-deadline limits before an operation
is dispatched; `shutdown_channel` and `termination_channel` drive graceful
drain. The crate is usable without the `blazingly` facade: anything that
produces an `ExecutableApp` — the facade's macros, or explicit construction
as below — can be served. The facade gates this adapter behind its
non-default `native` feature and exposes it as `blazingly::native`.

```toml
[dependencies]
blazingly-core = "0.2"
blazingly-executor = "0.2"
blazingly-native = "0.2"
```

```rust
use blazingly_core::{HttpMethod, OperationDescriptor, ResponseDescriptor, TypeDescriptor};
use blazingly_executor::{ExecutableApp, ExecutableOperation, ExecutionOutcome, OperationFuture};
use blazingly_native::Server;

fn health() -> ExecutableOperation {
    let descriptor = OperationDescriptor::new(
        HttpMethod::Get,
        "/health",
        "app.health",
        "Reports liveness",
        None,
        vec![ResponseDescriptor::success(
            200,
            Some(TypeDescriptor::new("Health")),
        )],
    )
    .expect("operation id is valid");
    ExecutableOperation::typed(descriptor, |_input| {
        Ok(Box::pin(async {
            ExecutionOutcome::Success {
                status: 200,
                headers: Vec::new(),
                body: Some(b"\"ok\"".to_vec()),
                background: Vec::new(),
            }
        }) as OperationFuture)
    })
}

fn main() -> std::io::Result<()> {
    let app = ExecutableApp::new([health()]).expect("operation graph compiles");
    Server::new(app).serve("127.0.0.1:3000")
}
```

## Links

- [API documentation](https://docs.rs/blazingly-native)
- [Getting started](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md) — the framework picture
- [Repository](https://github.com/sergii-ziborov/blazingly)
