# blazingly-openapi

Deterministic OpenAPI 3.1 document generation for the Blazingly operation
model.

This crate projects a `blazingly_core::AppDefinition` — the framework's
validated operation graph — into an OpenAPI 3.1 / JSON Schema 2020-12
document (`to_value`, `to_value_with_config`) and precompiles the
`/openapi.json` and Scalar or Swagger UI responses as ready HTTP assets
(`OpenApiService`), so nothing is generated on the request hot path. It is an
ordinary library and works standalone: it depends only on `blazingly-core`
and `blazingly-json`, performs no I/O, and needs neither the facade nor a
server. The [Blazingly](https://github.com/sergii-ziborov/blazingly)
framework facade re-exports it as `blazingly::openapi` and mounts
`OpenApiService` through `HttpApp::with_openapi`; the optional `validation`
feature projects declarative `blazingly-validation` bounds into the generated
schemas.

## Direct use

```toml
[dependencies]
blazingly-core = "0.1"
blazingly-openapi = "0.1"
```

```rust
use blazingly_core::App;
use blazingly_openapi::{OpenApiConfig, to_value_with_config};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // An application built with the framework macros supplies a populated
    // definition; an empty one keeps this example self-contained.
    let definition = App::new().build()?;
    let document =
        to_value_with_config(&definition, &OpenApiConfig::new("Users API", "1.0.0"));
    println!("{document}");
    Ok(())
}
```

In a framework application, `definition()` on an `ExecutableApp` supplies the
populated definition.

## Links

- [API documentation](https://docs.rs/blazingly-openapi)
- [Getting started](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md)
  — the framework picture
- [Repository](https://github.com/sergii-ziborov/blazingly)
