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

Production adapters live in separate repositories so vendor-specific code
never enters the framework tree. Three exist today, each verified against a
live backend:

- `blazingly-sqlite` implements `ConnectionPool`, `Transactional`, and
  `MigrationRunner` over `rusqlite`. It separates read and write capacity —
  SQLite permits one writer and many WAL readers, so the pool hands out a
  bounded write lane plus an independently sized read lane of `query_only`
  connections (`SqlitePool::reads()`), with read-heavy pragma tuning and
  per-connection prepared-statement caching. Measured on the development
  host, 64 concurrent readers went from 15,216 to 199,537 ops/s (256.1 to
  43.0 µs CPU per operation) with the read lane sized to the reader count.
  `IsolationLevel::ReadUncommitted` is honored where SQLite can honestly
  deliver it — between connections of a shared-cache in-memory pool via
  `PRAGMA read_uncommitted` — and rejected with an error naming the cache
  mode on file-backed private-cache pools. The shared-cache price is
  table-level locking: a reader without the pragma does not see a stale row,
  it fails to see the table while a write transaction holds it.
- `blazingly-postgres` implements the same seam for PostgreSQL with the
  frontend/backend protocol version 3 written directly over
  `std::net::TcpStream` — no `postgres`/`tokio-postgres` dependency, no
  async runtime. SCRAM-SHA-256 authentication, extended-query parameter
  binding in the binary format for 14 types (integers, floats, bool,
  text, bytea, timestamptz, uuid, json/jsonb), all four isolation levels
  expressed exactly (the server treats `READ UNCOMMITTED` as
  `READ COMMITTED`, per its documentation), SQLSTATE-keyed error
  classification onto the seam's stable kinds, a bounded deadline-bounded
  pool, prepared-statement caching with transparent re-prepare on plan
  invalidation, and advisory-locked migrations. TLS is out of scope by
  design; the README documents the sidecar/PgBouncer pattern.
- `blazingly-redis` implements the queue seam on Redis Streams with RESP
  written directly — no Redis crate, no Tokio. `XADD` publishes; consumer
  groups with `XREADGROUP` give competing consumers; `XPENDING`+`XCLAIM`
  (chosen over `XAUTOCLAIM`, which does not report a delivery count) reclaim
  the work of dead consumers while preserving the attempt count the worker's
  retry ceiling compares against. Delayed nacks park the message in a
  sorted set scored by server time and promote it on receive. The guarantee
  is at-least-once, stated as such: transitions between keys are single Lua
  scripts, so nothing is silently lost, and duplicates remain possible in
  the windows the README names.

The seam crates in this workspace define and test the stable contracts; the
adapter repositories carry the vendor code. NATS, RabbitMQ, Kafka, and SQS
adapters remain follow-up packages of the same shape.
