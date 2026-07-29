# blazingly-core

The runtime-neutral operation model of the
[Blazingly](https://github.com/sergii-ziborov/blazingly) framework.

This crate defines the data the rest of the framework acts on: the typed
extractor and response wrappers handlers use (`Json`, `Path`, `Query`,
`Header`, `Cookie`, `Form`, `Multipart`, `File`; `Created`, `Accepted`,
`NoContent`, `Status`, `WithHeaders`), streaming, upgrade, upload, and
background-task primitives, a pull-based `multipart/form-data` reader, the
HTTP projection of a protocol-neutral operation (`HttpMethod`, `HttpBinding`,
`OperationDescriptor`), and the `App` builder that validates routes into a
deterministic `AppDefinition`. It re-exports the contract vocabulary of
`blazingly-contract`. There is no server, no async runtime, and no macro in
this crate.

It is an ordinary library and usable standalone: `blazingly-executor`,
`blazingly-http`, the native server, and the OpenAPI, docs, and MCP crates
all consume it, and custom adapters or tooling over the operation model can
depend on it directly. The `blazingly` facade re-exports this crate at its
root and adds the attribute macros and the executor that turn plain functions
into operations.

## Direct use

```rust
use blazingly_core::{App, HttpMethod, OperationDescriptor, ResponseDescriptor};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let health = OperationDescriptor::new(
        HttpMethod::Get,
        "/health",
        "health.read",
        "Liveness probe",
        None,
        vec![ResponseDescriptor::success(200, None)],
    )?;

    let definition = App::new().route(health).build()?;
    assert_eq!(definition.operations().len(), 1);
    Ok(())
}
```

`build` rejects duplicate operation ids and duplicate or ambiguous HTTP
bindings, so an `AppDefinition` that exists is one every adapter can trust.

## Links

- [API documentation](https://docs.rs/blazingly-core)
- [Getting started with the framework](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md)
- [Repository](https://github.com/sergii-ziborov/blazingly)
