# blazingly-config

Typed configuration for Blazingly services, read from the environment and
checked at startup.

A misconfigured container should fail to start with a list of what is wrong,
not start with a silently wrong default and fail later somewhere unrelated.
`#[settings]` derives a loader that reads every field, collects **every**
problem before failing, and reports them together — so three missing variables
cost one failed boot rather than three.

Reading the environment is a global side effect, and `std::env::set_var` is
unsafe in Rust 2024, which this workspace forbids. So every loader takes a
[`ConfigSource`]: the process environment is one implementation, and a test
supplies a map instead of mutating the world.

## Direct use

```toml
[dependencies]
blazingly-config = "0.3"
blazingly-macros = "0.3"
```

```rust
use blazingly_config::{MapSource, Settings};
use blazingly_macros::settings;

#[settings(prefix = "APP_")]
#[derive(Debug)]
struct AppSettings {
    /// Reads `APP_DATABASE_URL`.
    #[min_length(1)]
    database_url: String,
    /// Reads `APP_PORT`, or 8080 when it is unset.
    #[default("8080")]
    port: u16,
    /// Reads `APP_REQUEST_TIMEOUT` as `30s`, `500ms`, `2h`, or bare seconds.
    #[default("30s")]
    request_timeout: std::time::Duration,
    /// Comma-separated; an unset variable is an empty list.
    #[default("")]
    allowed_origins: Vec<String>,
    /// Unset is `None`. A value that is set but unparsable is still an error.
    sentry_dsn: Option<String>,
}

let source = MapSource::new().with("APP_DATABASE_URL", "postgres://localhost/app");
let settings = AppSettings::load(&source).expect("the configuration is complete");
assert_eq!(settings.port, 8080);
assert_eq!(settings.request_timeout.as_secs(), 30);
assert!(settings.allowed_origins.is_empty());

// In a real service: `AppSettings::from_env()`.
let incomplete = AppSettings::load(&MapSource::new()).expect_err("nothing is set");
assert_eq!(incomplete.missing().collect::<Vec<_>>(), ["APP_DATABASE_URL"]);
```

Booleans accept what deployments actually write: `true`/`false`, `1`/`0`,
`yes`/`no`, `on`/`off`. `Settings::variables()` lists every variable a type
reads, so a deployment can be documented without running the service.

## Links

- [API documentation](https://docs.rs/blazingly-config)
- [Getting started](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md)
- [Repository](https://github.com/sergii-ziborov/blazingly)
