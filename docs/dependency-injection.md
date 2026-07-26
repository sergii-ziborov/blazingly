# Dependency injection and plugin scopes

Blazingly combines FastAPI-style typed handler arguments with Fastify-style
encapsulation. Provider graphs are compiled when `ExecutableApp` is built, not
interpreted on every request.

## Handler ergonomics

A cloneable dependency handle can appear directly in a handler:

```rust
#[derive(Clone)]
struct UsersRepository;

#[get("/users/{id}", id = "users.read", summary = "Read a user")]
async fn read_user(
    Path(id): Path<u64>,
    users: UsersRepository,
) -> Json<UserView> {
    todo!()
}
```

Use `Depends<T>` when the dependency itself should not be cloned or when shared
ownership should be explicit:

```rust
async fn health(database: Depends<DatabasePool>) -> Json<Health> {
    todo!()
}
```

Dependencies are recorded in operation metadata but are not exposed as HTTP
parameters or MCP tool arguments. OpenAPI includes them under the
`x-blazingly-dependencies` extension.

## Providers

```rust
let users = Plugin::new("users")
    .provide(Provider::value(Settings::production()))
    .provide(Provider::singleton(
        |settings: Depends<Settings>| DatabasePool::new(&settings),
    ))
    .provide(Provider::request(
        |database: Depends<DatabasePool>| UsersRepository::new(database),
    ))
    .routes(routes![read_user]);
```

Provider closures may depend on up to eight typed `Depends<T>` arguments.
Fallible factories use `Provider::try_singleton`,
`Provider::try_request`, or `Provider::try_transient` and return
`Result<T, DependencyError>`.

The `#[provider]` frontend generates the same compiled provider without
changing the factory into framework-specific code:

```rust
#[provider(singleton)]
fn database(settings: Depends<Settings>) -> DatabasePool {
    DatabasePool::new(&settings)
}

#[provider] // request scope by default
async fn users(
    database: Depends<DatabasePool>,
) -> Result<UsersRepository, DependencyError> {
    UsersRepository::connect(database).await
}

let users_plugin = Plugin::new("users")
    .provide(database::provider())
    .provide(users::provider())
    .routes(routes![read_user]);
```

The macro accepts `singleton`, `request`, and `transient`, infers async and
`Result<T, DependencyError>`, and keeps the original function callable.
Singleton initialization remains synchronous; async singleton providers are a
compile-time error. Providers with sync or async finalizers continue to use the
explicit `Provider::*_scoped` constructors so lifecycle ownership stays
visible at registration.

A stable rejection joins the normal typed error pipeline:

```rust
Provider::try_request(|| -> Result<Authorization, DependencyError> {
    Err(DependencyError::rejected(OperationFailure::new(
        401,
        "missing_token",
        "Authentication is required.",
    )))
})
```

HTTP receives the declared status and error envelope. MCP receives an
agent-readable tool error with the same stable code. Internal provider failures
remain redacted.

## Lifetimes

- `Provider::value` and `Provider::singleton` create application singletons.
- `Provider::request` creates one value for each invocation that reaches it,
  then shares that value across downstream consumers.
- `Provider::transient` creates a value for every injection edge.
- `Provider::request_scoped` and `Provider::try_request_scoped` add a finalizer.

Finalizers execute after the handler completes, in reverse provider order:

```rust
Provider::request_scoped(
    || Transaction::begin(),
    |transaction: Depends<Transaction>| transaction.close(),
)
```

A singleton may depend only on other singletons. Request and transient
providers may depend on longer-lived or equally short-lived providers.

## Scope inheritance

```rust
let app = Plugin::new("app")
    .provide(Provider::value(GlobalSettings))
    .plugin(
        Plugin::new("users")
            .provide(Provider::value(UsersSettings))
            .routes(routes![read_user]),
    )
    .plugin(
        Plugin::new("billing")
            .routes(routes![read_invoice]),
    );
```

Both children inherit `GlobalSettings`. A provider registered in `users` is
invisible to `billing` and `app`. Registering the same output type in a child
overrides the parent only inside that child.

## Build-time diagnostics

`ExecutableApp::from_plugin` rejects:

- missing handler or provider dependencies;
- provider cycles;
- duplicate provider output types in one plugin;
- singleton-to-request or singleton-to-transient edges;
- invalid plugin names;
- failed singleton construction.

Diagnostics include the plugin path, consumer, and Rust dependency type.

## Test overrides and production aborts

Tests replace providers before graph validation, so mocks cannot bypass
lifetime, cycle, or missing-dependency diagnostics:

```rust
let overrides = TestOverrides::new()
    .replace(Provider::value(FakeClock))
    .replace_in("app/billing", Provider::value(FakeGateway));

let app = ExecutableApp::from_plugin_with_overrides(plugin, overrides)?;
```

`InvocationControl` races an invocation against runtime-neutral cancellation
and timeout futures. Adapters supply their own clock; the native server uses
Compio and does not pull Tokio into executor:

```rust
let control = InvocationControl::new()
    .with_cancellation(token)
    .with_timeout(platform_timeout);
```

Already-created request dependencies are finalized after cancellation or
timeout. Finalizers, `on_error`, and `on_response` are cleanup and are shielded
once abort handling begins.

## Hot-path contract

Build time may use type identities and hash maps. Request execution may not.
Each operation stores:

1. a topologically sorted provider array;
2. numeric sources for every provider input;
3. numeric sources for every handler dependency.

The first eight request slots use inline storage. Larger graphs allocate one
slot array. No provider graph traversal, type-name lookup, or hash-map lookup
occurs per request.

Async request and transient factories use `Provider::request_async`,
`Provider::transient_async`, `Provider::try_request_async`, and
`Provider::try_transient_async`. Async request finalizers use
`Provider::request_async_scoped` or `Provider::try_request_async_scoped`.
Singleton initialization remains synchronous and deterministic at build time.

Plugin lifecycle hooks are also async and compiled into each operation:

```text
parent on_request -> child on_request
  -> dependencies
  -> parent pre_parse -> child pre_parse
  -> parent pre_validate -> child pre_validate
  -> parent pre_handler -> child pre_handler
  -> handler
  -> parent pre_serialize -> child pre_serialize
  -> dependency finalizers in reverse order
  -> child on_error -> parent on_error (errors only)
  -> child on_response -> parent on_response
```

Pre-phase hooks can return `DependencyError`, which uses the same stable
HTTP/MCP rejection and internal-error redaction pipeline. Resolved dependencies
are finalized if a later phase fails. `on_error` observes non-success outcomes;
`on_response` observes every result. Application shutdown hooks execute
child-before-parent and continue after failures before returning the first
error.
