# Getting started

This guide builds one small API with Blazingly: a validated model, a typed
domain error, an injected repository, a native server, an OpenAPI document, and
an MCP tool over the same handlers. Everything here has been pasted into a
freshly scaffolded project and run.

The rest of `docs/` is written for people working on the framework. This page is
for people using it.

## Requirements

- Rust 1.88 or newer. That is the workspace MSRV; the example below type-checks
  under 1.88.0 and builds under current stable.
- Nothing else. The native server is Compio-based, and `tokio`, `hyper`, and
  `axum` are banned from the dependency graph at any depth by a CI job, so
  adding Blazingly does not add an async runtime you did not ask for.

## Install the CLI

```console
cargo install cargo-blazingly
```

The framework repository uses submodules; Cargo checks them out itself, so there
is no separate clone step.

## Create a project

```console
cargo blazingly new hello-api
cd hello-api
```

That writes `Cargo.toml`, `src/main.rs`, and `.gitignore`. The manifest is:

```toml
[package]
name = "hello-api"
version = "0.1.0"
edition = "2024"

[workspace]

[dependencies]
# To track unreleased work on `main` instead:
# blazingly = { git = "https://github.com/sergii-ziborov/blazingly", features = ["native"] }
blazingly = { version = "0.2.2", features = ["native"] }
```

The version is the one the CLI itself was built against, so `cargo install
cargo-blazingly` and the project it scaffolds cannot disagree. Uncommenting the
line above tracks unreleased work on `main` instead; Cargo checks the
submodules out itself.

`[workspace]` is deliberate: it stops the project from being adopted by a parent
workspace if you generated it inside one.

If you have a local checkout of the framework, `cargo blazingly new hello-api
--framework-path /path/to/blazingly` emits a path dependency instead.

## Run it

```console
cargo blazingly dev
```

The first build takes a few minutes. After that the CLI watches the project,
rebuilds on change, and swaps the process only once the build succeeded, so a
compile error leaves the previous binary serving.

```console
curl http://127.0.0.1:3000/health
```

```
"ok"
```

`--address 127.0.0.1:3100` moves the port. `cargo blazingly run` builds release
and runs without the watcher.

## A first operation

Open `src/main.rs`. The generated handler is the whole pattern:

```rust
use blazingly::prelude::*;

#[get("/health", id = "health.read", summary = "Liveness probe")]
fn health() -> Json<&'static str> {
    Json("ok")
}
```

The attribute carries the path, a stable operation id, and a one-line summary.
The id is not decoration: it names the operation in the contract, in OpenAPI,
in the generated documentation, and in compatibility reports, and it is the one
thing that must stay stable as the path or the function name change. The
summary is what reaches the OpenAPI description, the `cargo blazingly routes`
table, and the MCP tool description.

Handlers may be `fn` or `async fn`. A synchronous handler with no lifecycle
hooks runs on a direct, allocation-free path; the macro also emits an async form
so hooks, cancellation, and timeouts behave identically either way.

Every method has an attribute — `#[get]`, `#[post]`, `#[put]`, `#[patch]`,
`#[delete]`, `#[head]`, `#[options]`, `#[trace]`, `#[connect]` — and they are all
aliases over `#[operation(method = ..., path = ..., id = ...)]`.

## Add a validated model

`#[api_model]` turns a plain struct into a request/response model with
serialization, validation, and a schema. Add this above `health`:

```rust
#[api_model]
#[derive(Clone, Debug)]
struct NewTask {
    #[min_length(1)]
    #[max_length(120)]
    title: String,
    #[email]
    owner: String,
}

#[api_model]
#[derive(Clone, Debug)]
struct Task {
    id: u64,
    title: String,
    owner: String,
}
```

Field attributes are the validation rules. Beyond `min_length`, `max_length`,
and `email` there are `pattern`, `minimum`, `maximum`, `exclusive_minimum`,
`exclusive_maximum`, `multiple_of`, `min_items`, `max_items`, `unique_items`,
`default`, `alias`, `nested`, and `validate_with` for a custom function. With
the `validation` feature — on by default — `Uuid`, `Url`, `IpAddress`, `Date`,
`DateTime`, and `Decimal` are strong field types rather than `String` plus a
pattern.

Validation failures never reach the handler. They come back as `422` with a
field path per violation:

```console
curl -s -X POST http://127.0.0.1:3000/tasks \
  -H 'content-type: application/json' \
  -d '{"title":"","owner":"not-an-email"}'
```

```json
{"error":{"code":"validation_error","details":{"violations":[{"code":"min_length","field":"title","message":"must contain at least 1 characters"},{"code":"email","field":"owner","message":"must be a valid email address"}]},"message":"json input failed validation"}}
```

Nested models and collections produce paths such as `address.street` and
`items[0].street`, so a client can point at the offending input.

## Add an error type

Domain failures are a declared enum, not a status code chosen at the point of
return:

```rust
#[api_error]
enum TaskError {
    #[status(404)]
    #[code("task_not_found")]
    #[message("No task with that identifier exists.")]
    NotFound,
}
```

A handler returns `Result<T, TaskError>`, and the status, the stable `code`, and
the message all reach OpenAPI, the generated documentation, and MCP without
being restated:

```console
curl -s http://127.0.0.1:3000/tasks/99
```

```json
{"error":{"code":"task_not_found","message":"No task with that identifier exists."}}
```

Variants can carry typed details and declare response headers, for example
`#[header("retry-after", "30")]` on a rate-limit variant. Framework-internal
failures — a bad response header, a serialization error — are redacted to a
generic `500` instead of leaking.

## Inject a dependency

A provider is an ordinary function. `#[provider(singleton)]` builds the value
once, while the application is compiled; the default lifetime is `request`, and
`transient` rebuilds on every injection edge.

```rust
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Clone, Default)]
struct Tasks {
    next_id: Rc<Cell<u64>>,
    stored: Rc<RefCell<Vec<Task>>>,
}

impl Tasks {
    fn insert(&self, title: String, owner: String) -> Task {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        let task = Task { id, title, owner };
        self.stored.borrow_mut().push(task.clone());
        task
    }

    fn get(&self, id: u64) -> Option<Task> {
        self.stored
            .borrow()
            .iter()
            .find(|task| task.id == id)
            .cloned()
    }
}

#[provider(singleton)]
fn tasks() -> Tasks {
    Tasks::default()
}
```

`Rc` and `RefCell` are fine here: the core imposes no unconditional
`Send + Sync` bound on handlers or injected values, so nothing forces an atomic
where a cell will do.

Handlers take the value as a plain typed argument, or as `Depends<T>` when you
want the injection to be visible in the signature:

```rust
#[post("/tasks", id = "tasks.create", summary = "Create a task")]
async fn create_task(Json(input): Json<NewTask>, tasks: Tasks) -> Created<Task> {
    Created(tasks.insert(input.title, input.owner))
}

#[get("/tasks/{id}", id = "tasks.read", summary = "Read one task")]
async fn read_task(Path(id): Path<u64>, tasks: Tasks) -> Result<Json<Task>, TaskError> {
    tasks.get(id).map(Json).ok_or(TaskError::NotFound)
}
```

`Path<T>`, `Query<T>`, `Header<T>`, `Cookie<T>`, `Json<T>`, `Form<T>`,
`Multipart<T>`, and `File<T>` are the extractors, and a handler may take several.
For anything they do not cover, `Extract<T>` accepts any type implementing the
public `FromInvocation` trait, and `Extract<RequestParts>` hands you an owned
snapshot of the method, path, effective scheme and host, and peer address.
`Created<T>`, `Accepted<T>`, `NoContent`, and `Status<CODE, T>` are the typed
responses; `WithHeaders` adds validated response headers.

A provider may also read the request itself — `Path<T>`, `Query<T>`,
`Header<T>`, and `Cookie<T>` work beside `Depends<T>`:

```rust
#[provider]
fn current_user(Header(authorization): Header<String>, tasks: Depends<Tasks>) -> CurrentUser {
    // decode the token, look the user up ...
}
```

Every input a provider declares folds into the operations that use it,
exactly once: the same header consumed by two providers is decoded once,
appears once in `/openapi.json` and the MCP tool schema, and fails validation
with the same `422` envelope a handler-declared input produces — before any
provider runs. A test override that replaces the provider supplies the value
directly, so the mock needs neither the header nor the cookie.

Providers are registered on a plugin, and the plugin owns the routes:

```rust
fn application() -> ExecutableApp {
    ExecutableApp::from_plugin(
        Plugin::new("app")
            .provide(tasks::provider())
            .routes(routes![health, create_task, read_task]),
    )
    .expect("application contract should compile")
}
```

The same module can be mounted twice — `Plugin::mount("/v1")` joins the path
prefix at compile time and `with_id_namespace("v1")` keeps the operation
identities and MCP tool names distinct per mount.

The provider graph is compiled once, during `ExecutableApp` construction, which
is why that call returns a `Result`: a dependency nothing provides is reported
there rather than as a per-request lookup failure. Request execution then
resolves through numeric slots rather than a type-name map. Plugins nest, and a
child scope inherits its parent's providers and can override them locally — see
[dependency injection and plugin scopes](dependency-injection.md).

Replace the generated `application()` with the one above and keep `main` as
generated. The complete file at this point serves:

```console
curl -s -X POST http://127.0.0.1:3000/tasks \
  -H 'content-type: application/json' \
  -d '{"title":"Write the guide","owner":"dev@example.com"}'
```

```json
{"id":1,"title":"Write the guide","owner":"dev@example.com"}
```

with a `201` status, and `GET /tasks/1` returns the same body with `200`.

### One caveat about state

`MulticoreServer::new(workers, application)` calls `application` once per worker
thread, so a `singleton` provider is a singleton per worker, not per process.
The in-memory `Tasks` above is a tutorial device: with more than one worker,
which worker answers a request decides what it sees. Anything that must be
consistent belongs in a store shared across workers — a database, a cache, a
queue. `BLAZINGLY_WORKERS=1` pins the example to one worker while you are
experimenting.

## See the OpenAPI document

The generated `main` already mounts it with
`OpenApiConfig::default()`. Name the document by replacing that one line in the
builder chain:

```rust
    .with_openapi(blazingly::openapi::OpenApiConfig::new("Tasks API", "0.1.0"))
```

The document is served at `/openapi.json` and a Scalar reference UI at `/docs`.
`with_document_path`, `with_ui_path`, and `with_ui(OpenApiUi::Swagger)` move or
change them.

### Prose the code cannot supply

The machine-checkable parts of the document — parameters, status codes, bodies,
validation constraints — come from the handler signature and cannot drift from
it. Prose cannot be derived that way, so an operation declares it:

```rust
#[get(
    "/tasks",
    id = "tasks.list",
    summary = "List tasks",
    tags = ["tasks"],
    description = "Returns one page of tasks, newest first.",
    external_docs = "https://example.com/tasks"
)]
```

`tags` decides which group the operation files under, in the browser UI and in
the generated Markdown; without it the namespace of the operation id is used, so
`tasks.list` files under `tasks` on its own. `deprecated` marks an operation as
still served but no longer recommended. None of this enters the operation
contract, so adding a tag does not change a fingerprint.

The document as a whole takes the same treatment:

```rust
blazingly::openapi::OpenApiConfig::new("Tasks API", "0.1.0")
    .with_description("Everything a task tracker needs.")
    .with_server(
        blazingly::openapi::OpenApiServer::new("https://api.example.com")
            .with_description("Production"),
    )
    .with_tag_description("tasks", "Creating, reading, and closing tasks.")
```

For anything the projection does not generate at all — `callbacks`, `webhooks`,
`info.contact`, a description on one individual response — `with_overlay` takes
raw OpenAPI and merges it in:

```rust
    .with_overlay(blazingly_json::json!({
        "info": { "contact": { "name": "API team", "email": "api@example.com" } }
    }))
```

The merge is additive: it writes a key only where the generated document has
none, at every depth. An overlay can therefore add to the document but never
overwrite a schema, a status code, or a security requirement that came from the
code — which is what keeps the document worth trusting even with an escape
hatch in it.

### Responses the framework answers itself

Two kinds of response are in the document without being declared, marked
`x-blazingly-automatic` so a reader can tell them apart from the ones the
handler returns. An operation that decodes any input can be answered `422`
before the handler runs. An operation that declares `#[security(...)]` can be
answered `401`, and if the requirement names scopes, `403` as well. Declaring
either status yourself keeps yours.

```console
curl -s http://127.0.0.1:3000/openapi.json
```

The document is OpenAPI 3.1 with JSON Schema 2020-12, generated from the same
operation model the handlers use — the validation rules above appear as
`minLength`, `maxLength`, and `format: email` on `NewTask`, and `TaskError`
appears as the `404` response of `tasks.read`. It is not hand-written and cannot
drift from the code.

That applies to rules a reusable value type declares, too. A `Vec<Tag>` field
publishes `Tag`'s own bounds on the item schema rather than on the array, which
is the scope the validator enforces them at and the scope a client has to
satisfy.

Every operation whose input is decoded also documents the `422` it can return
before the handler runs, without declaring it: the rejection envelope, the codes
that operation's inputs can produce, and the `violations` array naming the field
path and the rule that broke. It is marked `x-blazingly-automatic`, so a reader
can tell it apart from a response the operation declared itself.

Two commands print the same information without a browser:

```console
cargo blazingly routes
```

```
METHOD  PATH            OPERATION       SUMMARY
GET     /health         health.read     Liveness probe
POST    /tasks          tasks.create    Create a task
GET     /tasks/{id}     tasks.read      Read one task
```

```console
cargo blazingly openapi --out openapi.json
```

Both build the binary and run it with `BLAZINGLY_EMIT` set, so it prints and
exits before binding a socket. One honest limit: `cargo blazingly openapi`
emits the document with the default title and version, because `with_openapi` is
applied after the app is constructed and the emit happens during construction.
The served `/openapi.json` carries whatever you configured.

## Cutting the rebuild loop

Rust has no answer to `uvicorn --reload`, and pretending otherwise wastes your
time. What follows is what was measured, on one Windows machine, over a project
the size of this guide's: three repetitions of each edit, alternating, reporting
the fastest of each set because that is the number least contaminated by
whatever else the machine was doing.

| configuration | fastest rebuild |
|---|---|
| stock `cargo build` | 3.50 s |
| `debug = "line-tables-only"` alone | 3.45 s |
| `rust-lld` alone | 3.52 s |
| **both together** | **3.11 s** |

About a tenth off the floor, and neither half does it alone: `lld`'s advantage
shows up once there is less debug info to link. The generated `Cargo.toml`
already carries the profile. The linker is one file, because a linker cannot be
chosen from `Cargo.toml`:

```toml
# .cargo/config.toml
[target.x86_64-pc-windows-msvc]
linker = "rust-lld.exe"
```

`rust-lld` ships with the toolchain but is not on `PATH`; add
`$(rustc --print sysroot)/lib/rustlib/<target>/bin` to it, or name the binary by
its full path. On Linux use `linker = "clang"` with
`rustflags = ["-Clink-arg=-fuse-ld=lld"]`; on macOS the platform linker is
already fast enough that this buys little.

One thing measured and worth **not** doing: it is sometimes suggested that
splitting the framework's generics would stop an edit to `main` from
re-monomorphising the server. On the same project, editing a handler body and
editing `main` cost the same — 4.45 s against 4.83 s, ranges overlapping. They
are the same crate, so any edit recompiles all of it and relinks, and there is
no crate boundary for a generics split to protect. Splitting your own
application across crates would change that; changing the framework's generics
would not.

## Expose the same operations to agents

MCP is not generated from OpenAPI. It projects the same operation model, so a
tool call runs the same handler, the same validation, and the same typed errors.
Mark an operation:

```rust
#[post("/tasks", id = "tasks.create", summary = "Create a task")]
#[mcp::tool(
    name = "create_task",
    description = "Create one task",
    risk = "write",
    confirmation = "required"
)]
async fn create_task(Json(input): Json<NewTask>, tasks: Tasks) -> Created<Task> {
    Created(tasks.insert(input.title, input.owner))
}
```

`confirmation = "required"` means the call is rejected unless the host sends
`_meta["dev.blazingly/confirmed"] = true` after asking the user. `risk`,
`idempotent`, and `expose_output` are the other annotations.

To speak MCP over stdio — the transport MCP hosts launch as a subprocess — add
the feature:

```toml
blazingly = { version = "0.2.0", features = ["native", "mcp-stdio"] }
```

and branch in `main` before the server starts:

```rust
fn main() -> std::io::Result<()> {
    if std::env::args().any(|argument| argument == "--mcp") {
        let app = application();
        let mut server = blazingly::mcp::JsonRpcServer::new(&app);
        return blazingly::mcp::stdio::serve_stdio(&mut server);
    }
    // ... the generated server code, unchanged
}
```

Now `./hello-api --mcp` is an MCP server. Feeding it an `initialize` request and
then `tools/list` on stdin returns `create_task` with the validation rules
already in the input schema — abridged here, the real entry also carries an
output schema and `x-` provenance extensions:

```json
{"name":"create_task","description":"Create one task",
 "annotations":{"readOnlyHint":false,"idempotentHint":false,"destructiveHint":false},
 "inputSchema":{"type":"object","additionalProperties":false,
  "properties":{"title":{"type":"string","minLength":1,"maxLength":120},
                "owner":{"type":"string","format":"email"}},
  "required":["title","owner"]}}
```

For MCP over HTTP instead, `blazingly::mcp::StreamableHttpServer` serves the
same registry with sessions and a bounded redacted audit log.

## Where to go next

- [dependency injection and plugin scopes](dependency-injection.md) — lifetimes,
  finalizers, nested scopes, and test overrides.
- [developer CLI workflow](developer-workflow.md) — `Blazingly.toml`, discovery,
  autoreload, and the `BLAZINGLY_EMIT` contract.
- [deployment modes](deployment.md) — the generated container and Kubernetes
  artifacts.
- [ecosystem integration boundary](ecosystem.md) — database, queue, and template
  seams.
- [stability and SemVer](stability.md) — what the pre-1.0 series does and does
  not promise.
- [architecture](architecture.md) — why the operation model, the wire codec, and
  the adapters are separated the way they are.

Testing an application needs no socket. `TestApp` wraps an `ExecutableApp` and
runs the whole pipeline in memory — routing, extraction, validation, middleware,
handler, typed response — returning a `Response` you assert against; `call`
returns a future, so drive it with any executor, for example
`futures_lite::future::block_on`. `TestOverrides` replaces a provider globally
or inside one named plugin scope, so a test can swap the repository without
touching the handler.
