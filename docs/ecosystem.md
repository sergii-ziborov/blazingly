# Ecosystem integrations

The first ecosystem crates preserve Blazingly's runtime-neutral operation API:

- `blazingly-database` adapts synchronous database or ORM connection pools to
  the bounded blocking pool;
- `blazingly-queue` defines publish/receive/ack/nack contracts and provides a
  deterministic in-memory adapter;
- `blazingly-templates` compiles MiniJinja templates once and returns typed,
  escaped `text/html`;
- `blazingly-security` provides HS256 JWT, OAuth2 bearer-scope, API-key, and
  signed-session credential verifiers.

All of them are ordinary compiled DI values:

```rust
let app = ExecutableApp::from_plugin(
    Plugin::new("app")
        .provide(Provider::value(Database::new(pool)))
        .provide(Provider::value(QueueClient::new(queue)))
        .provide(Provider::value(Templates::compile(template_sources)?))
        .routes(routes![handler]),
)?;
```

A database pool implements `ConnectionPool`; its owned connection and query
result cross the bounded blocking-worker boundary. A queue vendor implements
`Queue` without imposing a global runtime on handlers. This keeps Tokio-based
vendor SDKs out of the framework core and allows future Cloudflare adapters to
provide native service bindings instead.

Production integrations for individual Diesel/rusqlite pools, PostgreSQL
drivers, NATS, RabbitMQ, Kafka, SQS, and template loaders should live in
separate adapter packages. The current crates define and test the stable seam;
they do not claim every vendor adapter already exists.
