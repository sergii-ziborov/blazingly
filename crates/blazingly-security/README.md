# blazingly-security

Authentication, authorization, and session middleware for the
[Blazingly](https://github.com/sergii-ziborov/blazingly) framework: HS256
JWT, OAuth2 bearer scopes, HTTP Basic, API keys, and signed-cookie or
server-side sessions.

The contract's security scheme descriptors stay the canonical source of
truth. The `Security` layer attaches concrete `CredentialVerifier`s to those
named schemes and enforces every requirement before request bodies are parsed
or handlers run, failing closed when a declared scheme has no verifier.
`SessionLayer` adds the write half of a session: a handler mutates `Session`
and the layer emits the resulting `Set-Cookie` header; the default backend is
a stateless signed cookie, and attaching a `SessionStore` moves the state
server side so a session can be revoked before it expires. The crate is
usable standalone: verifiers such as `JwtHs256` are plain values — the
example below encodes and verifies a token with no HTTP involved — and
`Security` and `SessionLayer` are `blazingly_http::HttpMiddleware` layers
that attach to `HttpApp`, `TestApp`, or the native server without the
facade. The facade re-exports this crate as `blazingly::security_runtime`
(a default feature).

```rust
use blazingly_security::{JwtClaims, JwtHs256, TokenVerifier};
use std::time::{SystemTime, UNIX_EPOCH};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 32-byte development secret; load a real one from configuration.
    let jwt = JwtHs256::new("0123456789abcdef0123456789abcdef")?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let token = jwt.encode(&JwtClaims::new("user-1", now + 3600).scope("read"))?;
    let verified = jwt.verify_token(&token)?;
    assert_eq!(verified.subject.as_deref(), Some("user-1"));
    assert_eq!(verified.scopes, ["read"]);
    Ok(())
}
```

## Links

- [API documentation](https://docs.rs/blazingly-security)
- [Getting started](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md) — the framework picture
- [Repository](https://github.com/sergii-ziborov/blazingly)
