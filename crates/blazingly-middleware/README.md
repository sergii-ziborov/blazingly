# blazingly-middleware

Runtime-neutral HTTP middleware for the
[Blazingly](https://github.com/sergii-ziborov/blazingly) framework: CORS,
response compression, trusted-host and proxy-header normalization,
token-bucket rate limiting, and static file serving.

Every layer implements `blazingly_http::HttpMiddleware` and is synchronous
and thread-local by construction, so native, in-memory, and future Worker
adapters share the same behavior without a dependency on Tokio or any other
async runtime. The crate is an ordinary library and does not require the
`blazingly` facade: attach layers to `HttpApp` or `TestApp` from
`blazingly-http`, or to the native server, with `with_middleware`. The
rate-limit primitives (`RateLimitQuota`, `RateLimitStore`,
`MemoryRateLimitStore`) also work with no HTTP dispatch at all, and a
distributed backend implements the same `RateLimitStore` seam. The facade
re-exports this crate as `blazingly::middleware` (a default feature); it adds
re-exports, nothing more.

```rust
use blazingly_middleware::{Compression, Cors, MemoryRateLimitStore, RateLimitQuota, RateLimitStore};
use std::time::{Duration, Instant};

fn main() {
    // Layers are plain values; attach them to any blazingly-http dispatcher
    // with `with_middleware`. No server or facade is needed to build them.
    let _cors = Cors::new().allow_origin("https://app.example.com");
    let _compression = Compression::new().minimum_size(512);

    // The rate-limit primitives work with no HTTP dispatch at all.
    let store = MemoryRateLimitStore::new(1024, Duration::from_secs(60));
    let quota = RateLimitQuota::new(2, Duration::from_secs(60));
    let now = Instant::now();
    assert!(store.consume("203.0.113.9", quota, now).allowed());
    assert!(store.consume("203.0.113.9", quota, now).allowed());
    assert!(!store.consume("203.0.113.9", quota, now).allowed());
}
```

## Links

- [API documentation](https://docs.rs/blazingly-middleware)
- [Getting started](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md) — the framework picture
- [Repository](https://github.com/sergii-ziborov/blazingly)
