# cargo-blazingly

Development and production CLI for Blazingly applications.

This is a binary crate, not a library: `cargo install cargo-blazingly`
installs a `cargo blazingly` subcommand. It shells out to Cargo and to the
application's own binary — `openapi` and `routes` run the app with
`BLAZINGLY_EMIT` set, and the framework prints the requested document during
server construction and exits before serving, so neither command can race a
serving instance. The [Blazingly](https://github.com/sergii-ziborov/blazingly)
framework itself never depends on this crate; the CLI composes
`blazingly-docs` for the project scaffold.

## Commands

```console
cargo blazingly new hello-api   # generate a minimal runnable project
cargo blazingly dev             # build, run, rebuild on change
cargo blazingly run             # build the release binary and launch it
cargo blazingly build           # release build
cargo blazingly check           # type-check
cargo blazingly openapi         # build the app, print its OpenAPI document
cargo blazingly routes          # build the app, print its operation table
cargo blazingly discover        # list discoverable Blazingly binary targets
cargo blazingly doctor          # verify Cargo, Rust, config, app discovery
```

`dev` swaps the running process only once a rebuild succeeded, so a compile
error leaves the previous binary serving. `new --framework-path <DIR>` emits
a path dependency on a local framework checkout instead of a Git dependency.
`cargo blazingly --help` documents the remaining options.

## Blazingly.toml

Optional per-project configuration, read from the working directory:

```toml
[app]
package = "api"
address = "127.0.0.1:8000"
features = ["native"]
watch = ["src", "templates"]

[env]
RUST_LOG = "info"
```

`address` sets `BLAZINGLY_LISTEN_ADDRESS`, the variable the scaffolded
application main reads and the generated Deployment manifest sets.

## Links

- [Developer CLI workflow](https://github.com/sergii-ziborov/blazingly/blob/main/docs/developer-workflow.md)
- [Getting started](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md)
  — the framework picture
- [API documentation](https://docs.rs/cargo-blazingly)
- [Repository](https://github.com/sergii-ziborov/blazingly)
