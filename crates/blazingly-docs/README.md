# blazingly-docs

Markdown documentation bundles and project scaffolds generated from a
Blazingly application definition.

`bundle` turns a `blazingly_core::AppDefinition` into a deterministic
in-memory file set: a human API reference (`api.md`), agent-oriented
instructions (`ai.md`), the canonical contract manifest (`contracts.json`),
HTTP and MCP request examples, and a Rust client starter.
`api_markdown`, `mcp_markdown`, and `ai_markdown` expose the individual
documents; `scaffold` generates a minimal compilable Tokio-free native
project, with container/Kubernetes files composed from `blazingly-deploy`.
Everything is a pure function returning strings — the caller decides what
reaches disk (`examples/scaffold.rs` shows the write loop). The crate works
standalone: it depends only on `blazingly-core`, `blazingly-deploy`, and
`blazingly-json`, and does not require the facade. The
[Blazingly](https://github.com/sergii-ziborov/blazingly) framework facade
re-exports it as `blazingly::docs`, `cargo blazingly new` writes its
scaffold, and the optional `validation` feature projects declarative
`blazingly-validation` bounds into the generated documents.

## Direct use

```toml
[dependencies]
blazingly-core = "0.2"
blazingly-docs = "0.2"
```

```rust
use blazingly_core::App;
use blazingly_docs::{DocsBundleConfig, bundle};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // An application built with the framework macros supplies a populated
    // definition; an empty one keeps this example self-contained.
    let definition = App::new().build()?;
    let bundle = bundle(&definition, &DocsBundleConfig::new("Users API"))?;
    for (path, contents) in bundle.files() {
        println!("{path}: {} bytes", contents.len());
    }
    Ok(())
}
```

In a framework application, `definition()` on an `ExecutableApp` supplies the
populated definition.

## Links

- [API documentation](https://docs.rs/blazingly-docs)
- [Getting started](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md)
  — the framework picture
- [Repository](https://github.com/sergii-ziborov/blazingly)
