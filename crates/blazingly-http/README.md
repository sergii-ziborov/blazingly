# blazingly-http

Adapter-neutral HTTP dispatch for the [Blazingly](https://github.com/sergii-ziborov/blazingly)
framework: a compiled router, typed request and response values, synchronous
middleware hooks, and an in-memory test client over the executable operation
graph.

This crate sits between `blazingly-executor`, which compiles the operation
graph, and the byte-level adapters (`blazingly-native` today). It defines
`Request` and `Response`, the borrowed `HttpRequestView` that adapters
implement over their receive buffers, the `HttpMiddleware` interception
points, `HttpApp` for serving paths, and `TestApp` for in-memory dispatch. It
is an ordinary library and is usable standalone: given an `ExecutableApp`, it
routes and dispatches requests entirely in memory with no socket, no async
runtime, and no facade — the example below is a complete program. The
`blazingly` facade re-exports these types and adds the macro layer that
derives operations from handler signatures; without it, operations are built
explicitly, as shown.

```toml
[dependencies]
blazingly-core = "0.1"
blazingly-executor = "0.1"
blazingly-http = "0.1"
futures-lite = "2"
```

```rust
use blazingly_core::{HttpMethod, OperationDescriptor, ResponseDescriptor, TypeDescriptor};
use blazingly_executor::{ExecutableApp, ExecutableOperation, ExecutionOutcome, OperationFuture};
use blazingly_http::{Request, TestApp};

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

fn main() {
    let app = ExecutableApp::new([health()]).expect("operation graph compiles");
    let test = TestApp::new(&app);
    let response = futures_lite::future::block_on(test.call(Request::get("/health")));
    assert_eq!(response.status(), 200);
    assert_eq!(response.body(), b"\"ok\"");
}
```

`futures-lite` only blocks on the returned future; dispatch is a plain
`Future`, so any executor works.

## Links

- [API documentation](https://docs.rs/blazingly-http)
- [Getting started](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md) — the framework picture
- [Repository](https://github.com/sergii-ziborov/blazingly)
