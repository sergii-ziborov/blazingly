# blazingly-di

Dependency-injection primitives for the
[Blazingly](https://github.com/sergii-ziborov/blazingly) framework: typed
providers, lifetimes, and the `Depends<T>` wrapper.

This crate defines `Provider` registration — singleton values, singleton,
request, and transient factories, fallible and async variants, and
reverse-order finalizers — plus the `DependencyKey` identity types,
`DependencyLifetime`, and `DependencyError`. Factories are plain closures; a
factory that takes `Depends<T>` arguments declares its edges, and those edges
are what the graph is compiled from. The compiler itself lives in
`blazingly-executor`: `ExecutableApp` validates the provider graph at build
time, plans numeric slots, and runs providers per invocation. Nothing in this
crate executes a provider.

It is an ordinary library — one dependency (`blazingly-contract`), no
runtime — and it works without the facade: pair it directly with
`blazingly-executor`, whose `Plugin::provide` consumes these `Provider`
values. The `blazingly` facade adds only re-exports and the `#[provider]`
attribute that generates a provider from a function. On its own the crate
registers and introspects providers, which is what the example shows.

## Direct use

```rust
use blazingly_di::{DependencyKey, DependencyLifetime, Depends, Provider};

struct Config {
    base_url: String,
}

fn main() {
    let config = Provider::value(Config {
        base_url: "https://api.internal".to_owned(),
    });
    let endpoint = Provider::request(|config: Depends<Config>| format!("{}/v1", config.base_url));

    assert_eq!(config.lifetime(), DependencyLifetime::Singleton);
    assert_eq!(endpoint.dependencies(), [DependencyKey::of::<Config>()]);
}
```

## Links

- [API documentation](https://docs.rs/blazingly-di)
- [Getting started with the framework](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md)
- [Dependency injection and plugin scopes](https://github.com/sergii-ziborov/blazingly/blob/main/docs/dependency-injection.md)
- [Repository](https://github.com/sergii-ziborov/blazingly)
