# blazingly-queue

Runtime-neutral queue seam for the
[Blazingly](https://github.com/sergii-ziborov/blazingly) framework:
publish/receive/ack/nack contracts, an in-memory conformance adapter, and a
shared worker loop.

This crate defines the `Queue` trait vendor adapters implement, the
`Message` and `Delivery` payload types, the `QueueClient` wrapper that keeps
application code vendor-neutral, `MemoryQueue` — a deterministic in-memory
adapter whose visibility delays run on a logical clock, for tests and local
development — and `Worker`, which owns the retry ceiling, exponential backoff
(`RetryPolicy`), and dead-letter routing every adapter shares. Vendor
adapters live in separate repositories; `blazingly-redis` (Redis Streams) and
`blazingly-nats` (NATS JetStream) exist today.

It is fully standalone: the crate has no dependencies at all, and every
future it returns is `Send` and drivable by any executor — no framework, no
facade, and no specific runtime required. The `blazingly` facade re-exports
it unchanged as `blazingly::queue` behind the opt-in `queue` feature.

## Direct use

The example uses `futures-lite` to drive the futures; any executor works.

```rust
use blazingly_queue::{MemoryQueue, Message, Queue};

fn main() {
    futures_lite::future::block_on(async {
        let queue = MemoryQueue::default();
        queue
            .publish("jobs", Message::new("payload"))
            .await
            .expect("publish");

        let delivery = queue
            .receive("jobs")
            .await
            .expect("receive")
            .expect("one delivery");
        assert_eq!(delivery.message.body, b"payload");

        queue.ack(&delivery.receipt).await.expect("ack");
    });
}
```

## Links

- [API documentation](https://docs.rs/blazingly-queue)
- [Getting started with the framework](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md)
- [Ecosystem integration boundary](https://github.com/sergii-ziborov/blazingly/blob/main/docs/ecosystem.md)
- [Repository](https://github.com/sergii-ziborov/blazingly)
