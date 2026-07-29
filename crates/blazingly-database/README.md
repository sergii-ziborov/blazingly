# blazingly-database

Trait seam that runs synchronous database and ORM clients on the
[Blazingly](https://github.com/sergii-ziborov/blazingly) framework's bounded
blocking pool.

This crate defines contracts only: vendor drivers implement
`ConnectionPool`, `Transactional`, and `MigrationRunner` in separate adapter
packages (`blazingly-sqlite` and `blazingly-postgres` exist today), and no
driver or cloud SDK code enters this crate. `Database<Pool>` is the cloneable
wrapper handlers inject: `run` acquires an owned connection and executes a
closure on the bounded blocking pool from `blazingly-executor`, so a slow
query never blocks an async worker; `transaction` adds begin/commit/rollback
with panic containment; `DatabaseError` classifies driver failures onto
stable `ErrorKind`s. Migrations are ordered, checksummed scripts
(`Migration`, `MigrationSet`) applied through `MigrationRunner`.

It is usable standalone: the only dependency is `blazingly-executor` (for
`run_blocking`), and `Database::run` returns an ordinary future, so it works
in any async context without the facade. The blocking pool initializes
itself with defaults on first use; `install_global_blocking_pool` sizes it
explicitly. The `blazingly` facade re-exports the crate as
`blazingly::database` behind the opt-in `database` feature.

## Direct use

The example implements the seam with a stub pool; a real adapter puts its
driver's pool and connection types behind the same three methods. It uses
`futures-lite` to drive the future; any executor works.

```rust
use blazingly_database::{ConnectionPool, Database};

struct StaticPool;

impl ConnectionPool for StaticPool {
    type Connection = Vec<&'static str>;
    type Error = std::convert::Infallible;

    fn acquire(&self) -> Result<Self::Connection, Self::Error> {
        Ok(vec!["alpha", "beta"])
    }
}

fn main() {
    let database = Database::new(StaticPool);
    let rows = futures_lite::future::block_on(
        database.run(|connection| Ok::<usize, std::io::Error>(connection.len())),
    )
    .expect("query");
    assert_eq!(rows, 2);
}
```

## Links

- [API documentation](https://docs.rs/blazingly-database)
- [Getting started with the framework](https://github.com/sergii-ziborov/blazingly/blob/main/docs/getting-started.md)
- [Ecosystem integration boundary](https://github.com/sergii-ziborov/blazingly/blob/main/docs/ecosystem.md)
- [Repository](https://github.com/sergii-ziborov/blazingly)
