# blazingly-mcp

Runtime-neutral MCP (Model Context Protocol) server for Blazingly operations.

MCP tools here are the typed operations the HTTP router serves, executed
through the shared `blazingly-executor` pipeline with the same validation,
dependency injection, and typed errors; nothing is reconstructed from
OpenAPI. The crate provides `McpRuntime` (direct in-process tool calls),
`JsonRpcServer` (the MCP JSON-RPC lifecycle), `StreamableHttpServer` (the
stateful Streamable HTTP transport), `McpRegistry` (resources and prompts),
and a bounded, redacted audit log. It works standalone: it depends on
`blazingly-core`, `blazingly-executor`, `blazingly-json`, and `serde`, opens
no sockets, and starts no runtime — each transport is handed one message and
returns the response, and the returned futures run on any single-threaded
executor. The [Blazingly](https://github.com/sergii-ziborov/blazingly)
framework facade re-exports it as `blazingly::mcp`; `blazingly-mcp-stdio`
adds a supervised newline-delimited stdio transport, and the optional
`validation` feature projects declarative `blazingly-validation` bounds into
tool schemas and decode failures.

## Direct use

```toml
[dependencies]
blazingly-executor = "0.1"
blazingly-mcp = "0.1"
futures-lite = "2" # any executor works; used here to drive the example
```

```rust
use blazingly_executor::{ExecutableApp, ExecutableOperation};
use blazingly_mcp::JsonRpcServer;

fn main() {
    // Operations normally come from the framework macros via `routes![...]`;
    // an empty application still speaks the full MCP lifecycle.
    let app = ExecutableApp::new(Vec::<ExecutableOperation>::new()).expect("empty application");
    let mut server = JsonRpcServer::new(&app);

    let response = futures_lite::future::block_on(server.handle_line(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"demo","version":"0"}}}"#,
    ));
    println!("{}", response.expect("initialize returns a response"));
}
```

## Links

- [API documentation](https://docs.rs/blazingly-mcp)
- [Getting started](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md)
  — the framework picture, including `#[mcp::tool]` on handlers
- [Repository](https://github.com/sergii-ziborov/blazingly)
