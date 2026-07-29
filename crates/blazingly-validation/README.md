# blazingly-validation

Strong string-like API value types and the runtime checks behind
[Blazingly](https://github.com/sergii-ziborov/blazingly)'s declarative field
validation.

The crate has two halves. The first is validated value types comparable to
common Pydantic field types — `Uuid`, `Url`, `IpAddress`, `Date`, `DateTime`,
`Decimal` — each parsing on deserialization, serializing back to its string
form, and carrying an `ApiSchema` descriptor so it projects correctly into
OpenAPI and MCP schemas. The second is what `#[api_model]` field rules expand
to: the check functions (`check_minimum`, `check_pattern`,
`check_unique_items`, and the rest) and a small pattern engine (`Pattern`,
`matches_pattern`) that compiles a bounded regular-expression subset with
explicit size and depth limits and a per-thread compile cache.

It is an ordinary library and usable standalone: it depends on
`blazingly-core` for the schema vocabulary plus the usual parsing crates
(`uuid`, `url`, `time`, `rust_decimal`), and nothing in it requires the
framework to be running. The `blazingly` facade re-exports it as
`blazingly::validation` behind the default-on `validation` feature, which
also switches the executor, OpenAPI, MCP, and docs crates to project declared
constraints into schemas and error envelopes instead of opaque validator
strings.

## Direct use

```rust
use blazingly_validation::{matches_pattern, Date, Uuid};

fn main() {
    let id: Uuid = "550e8400-e29b-41d4-a716-446655440000"
        .parse()
        .expect("well-formed UUID");
    assert_eq!(id.to_string(), "550e8400-e29b-41d4-a716-446655440000");

    assert!("2025-02-30".parse::<Date>().is_err());
    assert!(matches_pattern("order-42", "^[a-z]+-\\d+$"));
}
```

## Links

- [API documentation](https://docs.rs/blazingly-validation)
- [Getting started with the framework](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md)
- [Repository](https://github.com/sergii-ziborov/blazingly)
