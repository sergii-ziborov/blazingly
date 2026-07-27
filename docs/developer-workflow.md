# Developer workflow

Install the workspace CLI during development:

```console
cargo install --path crates/cargo-blazingly
cargo blazingly doctor
cargo blazingly discover
cargo blazingly dev
```

`dev` discovers a binary that depends directly on `blazingly`, starts it, and
restarts it after Rust source, manifest, template, or explicitly watched files
change. The child receives `BLAZINGLY_LISTEN_ADDRESS` when an address is
configured.

A reload builds first and swaps the process only after the build succeeded, so
a compile error leaves the previous process serving instead of leaving nothing
running. The CLI supervises the application process itself rather than a
`cargo` wrapper, so a restart actually releases the listening socket, and it
asks the process to stop before escalating to a kill so `on_shutdown` hooks run.

For a workspace with multiple applications, create `Blazingly.toml`:

```toml
[app]
package = "users-api"
bin = "users-api"
address = "127.0.0.1:8000"
features = ["native"]
watch = ["src", "templates", "migrations"]

[env]
RUST_LOG = "info"
```

Useful commands:

- `cargo blazingly check` checks the discovered target;
- `cargo blazingly build` makes a release build;
- `cargo blazingly run` builds and then executes the release binary directly;
- `cargo blazingly run -- --app-argument` forwards arguments after `--`;
- `--package`, `--bin`, and `--example` resolve ambiguous discovery;
- `--debug` selects a debug build and `--no-reload` disables the watcher;
- `--features`, `--all-features`, and `--no-default-features` reach Cargo, and
  `--no-build` launches the existing binary.

The watcher is intentionally dependency-light and polling-based. It waits for a
quiet period rather than a fixed delay before rebuilding, and it watches path
dependencies inside the workspace as well as the selected package. Filesystem
notification backends and zero-downtime process handoff are later production
CLI work.
