# blazingly-realtime

Runtime-neutral Server-Sent Events and WebSocket sessions for the
[Blazingly](https://github.com/sergii-ziborov/blazingly) framework.

`SseEvent` encodes single events under the event-stream grammar, and `Sse`
wraps a pull-based `StreamingBody`, so backpressure is native rather than
buffered. `WebSocketRequest` validates the upgrade handshake, negotiates
subprotocols, and hands the accepted connection to a handler as a `WebSocket`
session with receive, send, and close, automatic pong replies, fragmentation
handling, and a message size limit. The crate is an ordinary library: SSE
encoding is plain bytes usable anywhere — the example below is a complete
program — and the session types run over the `UpgradedIo` transport seam
from `blazingly-core`, so any adapter that implements it can drive them.
There is no dependency on the native server, Tokio, or the facade. The
response and upgrade types implement `OperationOutput`, which is how a
handler returns them; the facade re-exports the crate as
`blazingly::realtime` (a default feature).

```rust
use blazingly_realtime::{Sse, SseEvent, SseEventError};

fn main() -> Result<(), SseEventError> {
    let event = SseEvent::data("tick").with_event("clock")?.with_id("1")?;
    assert_eq!(event.encode(), b"event: clock\nid: 1\ndata: tick\n\n");

    // A pull-based stream body; an operation returns this as its response.
    let _stream = Sse::from_events([SseEvent::data("one"), SseEvent::data("two")]);
    Ok(())
}
```

## Links

- [API documentation](https://docs.rs/blazingly-realtime)
- [Getting started](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md) — the framework picture
- [Repository](https://github.com/sergii-ziborov/blazingly)
