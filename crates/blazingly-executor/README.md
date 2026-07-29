# blazingly-executor

Runtime-neutral operation executor for the
[Blazingly](https://github.com/sergii-ziborov/blazingly) framework:
extraction, validation, dependency resolution, and typed response projection.

This crate makes the operation model of `blazingly-core` executable.
`ExecutableOperation` pairs an `OperationDescriptor` with a handler; `Plugin`
scopes group operations with providers (from `blazingly-di`), lifecycle
hooks, and security schemes; `ExecutableApp::from_plugin` validates and
compiles the whole graph once, and `invoke` runs the full pipeline — hooks,
typed extraction, validation (behind the `validation` feature), the handler,
and projection into an `ExecutionOutcome` — for one operation. The same
pipeline serves HTTP requests (routed by `blazingly-http`) and MCP tool
calls. The crate also owns the bounded blocking pool (`run_blocking`,
`install_global_blocking_pool`) used by synchronous handlers and by
`blazingly-database`.

Standalone use is real: there is no HTTP transport, no macro, and no async
runtime here. `invoke` returns an ordinary future you can drive with any
executor, which is how applications are tested in memory. The `blazingly`
facade adds the attribute macros that generate `ExecutableOperation`s from
function signatures; without them, the `typed`, `json`, and `empty`
constructors build operations by hand, as below.

## Direct use

The example depends on `blazingly-core` and `blazingly-json` for the
descriptor and invocation value types, and uses `futures-lite` as the
executor; any executor works.

```rust
use blazingly_core::{HttpMethod, Json, OperationDescriptor, OperationId, ResponseDescriptor};
use blazingly_executor::{ExecutableApp, ExecutableOperation, ExecutionOutcome, Plugin};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let descriptor = OperationDescriptor::new(
        HttpMethod::Get,
        "/health",
        "health.read",
        "Liveness probe",
        None,
        vec![ResponseDescriptor::success(200, None)],
    )?;

    let app = ExecutableApp::from_plugin(
        Plugin::new("app")
            .operation(ExecutableOperation::empty(descriptor, || async { Json("ok") })),
    )?;

    let id = OperationId::new("health.read")?;
    let outcome = futures_lite::future::block_on(app.invoke(&id, blazingly_json::Value::Null));
    assert!(matches!(outcome, ExecutionOutcome::Success { status: 200, .. }));
    Ok(())
}
```

## Links

- [API documentation](https://docs.rs/blazingly-executor)
- [Getting started with the framework](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md)
- [Repository](https://github.com/sergii-ziborov/blazingly)
