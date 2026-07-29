# blazingly-mcp-stdio

Supervised, bounded newline-delimited stdio transport for the Blazingly MCP
server.

This crate drives a `blazingly_mcp::JsonRpcServer` over the process standard
streams (`serve_stdio`), over any `BufRead`/`Write` pair (`serve`), or under
a supervisor (`serve_supervised`) with per-connection `StdioConfig` limits —
maximum message bytes, message count, and rejected frames — a cooperative
stop between frames, and a `StdioReport` of counters and the termination
reason. Responses are the only bytes written to the output, so the host's
JSON-RPC stream stays clean. It contains its own thread-parking future driver
and therefore needs no async runtime. It is an ordinary library over
`blazingly-mcp` and works standalone; the
[Blazingly](https://github.com/sergii-ziborov/blazingly) framework facade
exposes it as `blazingly::mcp::stdio` behind the `mcp-stdio` feature.

## Direct use

```toml
[dependencies]
blazingly-executor = "0.1"
blazingly-mcp = "0.1"
blazingly-mcp-stdio = "0.1"
```

```rust
use blazingly_executor::{ExecutableApp, ExecutableOperation};
use blazingly_mcp::JsonRpcServer;
use blazingly_mcp_stdio::serve_stdio;

fn main() -> std::io::Result<()> {
    // Operations normally come from the framework macros via `routes![...]`.
    let app = ExecutableApp::new(Vec::<ExecutableOperation>::new()).expect("empty application");
    let mut server = JsonRpcServer::new(&app);
    serve_stdio(&mut server)
}
```

```console
$ echo '{"jsonrpc":"2.0","id":"ping-1","method":"ping"}' | ./target/debug/app
{"id":"ping-1","jsonrpc":"2.0","result":{}}
```

## Links

- [API documentation](https://docs.rs/blazingly-mcp-stdio)
- [Getting started](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md)
  — the framework picture
- [Repository](https://github.com/sergii-ziborov/blazingly)
