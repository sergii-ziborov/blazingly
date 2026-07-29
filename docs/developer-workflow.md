# Developer workflow

Install the workspace CLI during development:

```console
cargo install --path crates/cargo-blazingly
cargo blazingly doctor
cargo blazingly discover
cargo blazingly dev
```

## Starting a project

`cargo blazingly new <name>` generates a minimal runnable project in
`./<name>`: a `Cargo.toml` depending on `blazingly` with the `native` feature,
a `src/main.rs` with one `#[get]` handler and a `MulticoreServer`, and a
`.gitignore`. It is the one command that works outside a Cargo workspace.

While the framework is unpublished, the generated dependency is a Git
dependency on the Blazingly repository with a comment to switch to a version
requirement once published. `--framework-path <dir>` emits a path dependency
on a local checkout instead; the flag accepts either the workspace root or the
`blazingly` crate directory.

## Contract introspection

`cargo blazingly openapi` prints the application's OpenAPI document to stdout
(`--out <file>` writes it to a file), and `cargo blazingly routes` prints the
operation table: method, path, operation id, summary. Both build the debug
profile — sharing the `dev` build cache — and then run the unmodified
application binary with the `BLAZINGLY_EMIT` environment variable set.

`BLAZINGLY_EMIT` is a contract implemented by `blazingly_http::HttpApp::new`,
which sits on every native serving path: set to `openapi` or `routes`, server
construction prints the document or table to stdout and exits with code 0
before serving; any other non-empty value exits with code 2 so a typo never
falls through to serving. Tests and `TestApp` never consult the variable. The
emitted OpenAPI document uses the default document configuration because an
application's `with_openapi` settings are not known at construction time.

The CLI also sets `BLAZINGLY_LISTEN_ADDRESS=127.0.0.1:0` and
`BLAZINGLY_WORKERS=1` for the emit run, so a multicore binary that binds
before worker startup cannot race an already-running dev server for the
listen port.

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
