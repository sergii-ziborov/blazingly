# blazingly-macros

Procedural macros for
[Blazingly](https://github.com/sergii-ziborov/blazingly) operation, model,
and error declarations.

This crate provides `#[operation]` and its method aliases (`#[get]`,
`#[head]`, `#[post]`, `#[put]`, `#[patch]`, `#[delete]`, `#[options]`,
`#[trace]`, `#[connect]`), `#[api_model]`, `#[api_error]`, `#[provider]`,
`#[security]`, and the MCP `#[tool]` attribute.

It is internal plumbing, not a standalone library. The generated code names
paths under `::blazingly`, so it only compiles inside a crate that depends on
the `blazingly` facade. Do not depend on `blazingly-macros` directly: depend
on `blazingly`, which re-exports every macro (`blazingly::get`,
`blazingly::api_model`, `blazingly::mcp::tool`, and so on) alongside the
runtime types the expansions refer to. For that reason this page carries no
usage example; see the getting-started guide for the macros in context.

## Links

- [API documentation](https://docs.rs/blazingly-macros)
- [Getting started with the framework](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md)
- [Repository](https://github.com/sergii-ziborov/blazingly)
