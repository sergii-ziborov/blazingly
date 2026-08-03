#![forbid(unsafe_code)]

//! Trait seam for synchronous database and ORM clients.
//!
//! This crate defines contracts only. Vendor drivers live in separate adapter
//! packages outside this repository and implement [`ConnectionPool`],
//! [`Transactional`], and [`MigrationRunner`] there; Diesel, rusqlite,
//! `PostgreSQL`, and cloud SDK code never enters this crate. Work reaches the
//! driver through Blazingly's bounded blocking pool instead of an HTTP worker.

use blazingly_executor::{BlockingError, on_blocking_worker, run_blocking};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

/// Acquires one owned database or ORM connection.
pub trait ConnectionPool: Send + Sync + 'static {
    type Connection: Send + 'static;
    type Error: std::error::Error + Send + Sync + 'static;

    /// Acquires an owned connection for one blocking operation.
    ///
    /// # Errors
    ///
    /// Returns the adapter-specific pool acquisition failure.
    fn acquire(&self) -> Result<Self::Connection, Self::Error>;

    /// Acquires an owned connection, giving up once `timeout` elapses.
    ///
    /// The default ignores the deadline and forwards to [`Self::acquire`].
    /// Adapters whose driver supports a bounded wait must override it.
    ///
    /// # Errors
    ///
    /// Returns the adapter-specific acquisition or timeout failure.
    fn acquire_timeout(&self, _timeout: Duration) -> Result<Self::Connection, Self::Error> {
        self.acquire()
    }

    /// Reports current pool utilization, or `None` when untracked.
    fn health(&self) -> Option<PoolHealth> {
        None
    }

    /// Classifies an acquisition failure raised by this driver.
    fn classify_acquire(&self, _error: &Self::Error) -> ErrorKind {
        ErrorKind::Connection
    }

    /// Classifies an operation failure raised by this driver.
    ///
    /// The default reports [`ErrorKind::Other`]. Adapters downcast to their own
    /// error type and report timeouts and constraint violations.
    fn classify(&self, _error: &(dyn std::error::Error + 'static)) -> ErrorKind {
        ErrorKind::Other
    }
}

/// Point-in-time connection pool utilization for observability.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PoolHealth {
    /// Connections currently checked out.
    pub in_use: u32,
    /// Connections currently idle in the pool.
    pub idle: u32,
    /// Callers currently waiting for a connection.
    pub waiting: u32,
    /// Maximum connection count when the adapter caps the pool.
    pub max_size: Option<u32>,
}

impl PoolHealth {
    /// Reports whether no connection is immediately available.
    #[must_use]
    pub const fn is_saturated(self) -> bool {
        if self.waiting > 0 {
            return true;
        }
        match self.max_size {
            Some(max) => self.idle == 0 && self.in_use >= max,
            None => false,
        }
    }
}

/// Cloneable dependency wrapper around a concrete connection pool.
pub struct Database<Pool> {
    pool: Arc<Pool>,
    acquire_timeout: Option<Duration>,
}

impl<Pool> Database<Pool> {
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self {
            pool: Arc::new(pool),
            acquire_timeout: None,
        }
    }

    #[must_use]
    pub fn from_shared(pool: Arc<Pool>) -> Self {
        Self {
            pool,
            acquire_timeout: None,
        }
    }

    /// Bounds how long every operation waits for a connection.
    #[must_use]
    pub fn with_acquire_timeout(mut self, timeout: Duration) -> Self {
        self.acquire_timeout = Some(timeout);
        self
    }

    /// Returns the configured acquisition timeout.
    #[must_use]
    pub const fn acquire_timeout(&self) -> Option<Duration> {
        self.acquire_timeout
    }

    #[must_use]
    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }
}

impl<Pool> Clone for Database<Pool> {
    fn clone(&self) -> Self {
        Self {
            pool: Arc::clone(&self.pool),
            acquire_timeout: self.acquire_timeout,
        }
    }
}

impl<Pool> Database<Pool>
where
    Pool: ConnectionPool,
{
    /// Reports current pool utilization, or `None` when untracked.
    #[must_use]
    pub fn health(&self) -> Option<PoolHealth> {
        self.pool.health()
    }

    /// Acquires a connection and runs synchronous database work on the
    /// framework's bounded blocking pool.
    ///
    /// A caller that is already running on a blocking-pool worker runs the
    /// operation inline instead, on the worker it already occupies.
    ///
    /// # Errors
    ///
    /// Returns pool acquisition, query, saturation, or worker failures.
    pub async fn run<Operation, Output, QueryError>(
        &self,
        operation: Operation,
    ) -> Result<Output, DatabaseError>
    where
        Operation: FnOnce(&mut Pool::Connection) -> Result<Output, QueryError> + Send + 'static,
        Output: Send + 'static,
        QueryError: std::error::Error + Send + Sync + 'static,
    {
        if on_blocking_worker() {
            return run_inline(move || self.run_sync(operation));
        }
        let pool = Arc::clone(&self.pool);
        let timeout = self.acquire_timeout;
        run_blocking(move || run_on(pool.as_ref(), timeout, operation)).await?
    }

    /// Acquires a connection and runs synchronous database work on the calling
    /// thread.
    ///
    /// This is the seam adapters use when they are already on a thread that may
    /// block: a blocking-pool worker, or a dedicated driver thread they own.
    /// Unlike [`Self::run`] it neither queues nor isolates panics, so a
    /// panicking operation unwinds into the caller.
    ///
    /// # Errors
    ///
    /// Returns pool acquisition or query failures.
    pub fn run_sync<Operation, Output, QueryError>(
        &self,
        operation: Operation,
    ) -> Result<Output, DatabaseError>
    where
        Operation: FnOnce(&mut Pool::Connection) -> Result<Output, QueryError>,
        QueryError: std::error::Error + Send + Sync + 'static,
    {
        run_on(self.pool.as_ref(), self.acquire_timeout, operation)
    }
}

/// Runs work that is already holding a blocking-pool worker.
///
/// Invariant: the caller is on a pool worker, so this thread is one the pool
/// has already admitted. Running inline therefore adds no queue depth and
/// cannot push the pool above `workers` concurrent jobs, which is why it is
/// exempt from the bounded queue's admission check. Re-submitting instead would
/// be the unsafe choice: a worker awaiting its own re-submission deadlocks a
/// saturated pool. The operation must not itself wait on another pool
/// submission's completion.
fn run_inline<Output>(
    job: impl FnOnce() -> Result<Output, DatabaseError>,
) -> Result<Output, DatabaseError> {
    match catch_unwind(AssertUnwindSafe(job)) {
        Ok(result) => result,
        Err(_) => Err(DatabaseError::from(BlockingError::Panicked)),
    }
}

fn run_on<Pool, Operation, Output, QueryError>(
    pool: &Pool,
    timeout: Option<Duration>,
    operation: Operation,
) -> Result<Output, DatabaseError>
where
    Pool: ConnectionPool,
    Operation: FnOnce(&mut Pool::Connection) -> Result<Output, QueryError>,
    QueryError: std::error::Error + Send + Sync + 'static,
{
    let mut connection = acquire_connection(pool, timeout)?;
    operation(&mut connection).map_err(|error| operation_error(pool, error))
}

impl<Pool> Database<Pool>
where
    Pool: ConnectionPool,
    Pool::Connection: Transactional,
{
    /// Runs synchronous work inside one transaction on the bounded blocking
    /// pool.
    ///
    /// The transaction is committed when `operation` returns `Ok` and rolled
    /// back when it returns `Err` or panics. A panic is reported as a
    /// [`TransactionPanic`] source instead of unwinding into the pool worker.
    ///
    /// A caller that is already running on a blocking-pool worker runs the
    /// transaction inline instead, on the worker it already occupies.
    ///
    /// # Errors
    ///
    /// Returns pool acquisition, transaction control, operation, saturation, or
    /// worker failures. A rollback that fails after the operation failed is
    /// reported as a [`RollbackFailure`] source under the operation's class.
    pub async fn transaction<Operation, Output, QueryError>(
        &self,
        options: TransactionOptions,
        operation: Operation,
    ) -> Result<Output, DatabaseError>
    where
        Operation: FnOnce(&mut Pool::Connection) -> Result<Output, QueryError> + Send + 'static,
        Output: Send + 'static,
        QueryError: std::error::Error + Send + Sync + 'static,
    {
        if on_blocking_worker() {
            return run_inline(move || self.transaction_sync(options, operation));
        }
        let pool = Arc::clone(&self.pool);
        let timeout = self.acquire_timeout;
        run_blocking(move || transaction_on(pool.as_ref(), timeout, options, operation)).await?
    }

    /// Runs synchronous work inside one transaction on the calling thread.
    ///
    /// Commit, rollback, and panic handling match [`Self::transaction`]; only
    /// the scheduling differs. See [`Self::run_sync`] for when to reach for it.
    ///
    /// # Errors
    ///
    /// Returns pool acquisition, transaction control, or operation failures. A
    /// rollback that fails after the operation failed is reported as a
    /// [`RollbackFailure`] source under the operation's class.
    pub fn transaction_sync<Operation, Output, QueryError>(
        &self,
        options: TransactionOptions,
        operation: Operation,
    ) -> Result<Output, DatabaseError>
    where
        Operation: FnOnce(&mut Pool::Connection) -> Result<Output, QueryError>,
        QueryError: std::error::Error + Send + Sync + 'static,
    {
        transaction_on(self.pool.as_ref(), self.acquire_timeout, options, operation)
    }
}

fn transaction_on<Pool, Operation, Output, QueryError>(
    pool: &Pool,
    timeout: Option<Duration>,
    options: TransactionOptions,
    operation: Operation,
) -> Result<Output, DatabaseError>
where
    Pool: ConnectionPool,
    Pool::Connection: Transactional,
    Operation: FnOnce(&mut Pool::Connection) -> Result<Output, QueryError>,
    QueryError: std::error::Error + Send + Sync + 'static,
{
    let mut connection = acquire_connection(pool, timeout)?;
    connection
        .begin(options)
        .map_err(|error| operation_error(pool, error))?;
    match catch_unwind(AssertUnwindSafe(|| operation(&mut connection))) {
        Ok(Ok(output)) => connection
            .commit()
            .map(|()| output)
            .map_err(|error| operation_error(pool, error)),
        Ok(Err(error)) => {
            let rollback = connection.rollback();
            Err(rolled_back_error(pool, error, rollback))
        }
        Err(_) => {
            let rollback = connection.rollback();
            Err(rolled_back_error(pool, TransactionPanic, rollback))
        }
    }
}

impl<Pool> Database<Pool>
where
    Pool: ConnectionPool,
    Pool::Connection: MigrationRunner,
{
    /// Applies every pending migration on the bounded blocking pool.
    ///
    /// # Errors
    ///
    /// Returns pool acquisition, ledger, apply, saturation, or worker failures,
    /// and a [`MigrationError`] source when the ledger disagrees with `set`.
    pub async fn migrate(&self, set: MigrationSet) -> Result<MigrationReport, DatabaseError> {
        let pool = Arc::clone(&self.pool);
        let timeout = self.acquire_timeout;
        let job = move || {
            let mut connection = acquire_connection(pool.as_ref(), timeout)?;
            let applied = read_ledger(pool.as_ref(), &mut connection, &set)?;
            let mut report = MigrationReport {
                applied: Vec::new(),
                current_version: applied.last().map(|record| record.version),
            };
            for migration in set.pending(report.current_version) {
                connection
                    .apply(migration)
                    .map_err(|error| operation_error(pool.as_ref(), error))?;
                report.applied.push(migration.version);
                report.current_version = Some(migration.version);
            }
            Ok(report)
        };
        if on_blocking_worker() {
            return run_inline(job);
        }
        run_blocking(job).await?
    }

    /// Verifies the ledger matches `set` without applying anything.
    ///
    /// # Errors
    ///
    /// Returns pool acquisition, ledger, saturation, or worker failures, and a
    /// [`MigrationError`] source when a migration is pending, unknown, or its
    /// script changed after it was applied.
    pub async fn verify_migrations(&self, set: MigrationSet) -> Result<(), DatabaseError> {
        let pool = Arc::clone(&self.pool);
        let timeout = self.acquire_timeout;
        let job = move || {
            let mut connection = acquire_connection(pool.as_ref(), timeout)?;
            let applied = read_ledger(pool.as_ref(), &mut connection, &set)?;
            let pending = set.pending(applied.last().map(|record| record.version));
            match pending.first() {
                Some(migration) => Err(DatabaseError::new(
                    ErrorKind::Other,
                    MigrationError::Pending {
                        version: migration.version,
                    },
                )),
                None => Ok(()),
            }
        };
        if on_blocking_worker() {
            return run_inline(job);
        }
        run_blocking(job).await?
    }
}

fn acquire_connection<Pool>(
    pool: &Pool,
    timeout: Option<Duration>,
) -> Result<Pool::Connection, DatabaseError>
where
    Pool: ConnectionPool,
{
    match timeout {
        Some(timeout) => pool.acquire_timeout(timeout),
        None => pool.acquire(),
    }
    .map_err(|error| DatabaseError::new(pool.classify_acquire(&error), error))
}

fn operation_error<Pool, Error>(pool: &Pool, error: Error) -> DatabaseError
where
    Pool: ConnectionPool,
    Error: std::error::Error + Send + Sync + 'static,
{
    let kind = pool.classify(&error);
    DatabaseError::new(kind, error)
}

fn rolled_back_error<Pool, Error, RollbackError>(
    pool: &Pool,
    error: Error,
    rollback: Result<(), RollbackError>,
) -> DatabaseError
where
    Pool: ConnectionPool,
    Error: std::error::Error + Send + Sync + 'static,
    RollbackError: std::error::Error + Send + Sync + 'static,
{
    let kind = pool.classify(&error);
    match rollback {
        Ok(()) => DatabaseError::new(kind, error),
        Err(rollback) => DatabaseError::new(kind, RollbackFailure::new(error, rollback)),
    }
}

fn read_ledger<Pool>(
    pool: &Pool,
    connection: &mut Pool::Connection,
    set: &MigrationSet,
) -> Result<Vec<AppliedMigration>, DatabaseError>
where
    Pool: ConnectionPool,
    Pool::Connection: MigrationRunner,
{
    connection
        .ensure_ledger()
        .map_err(|error| operation_error(pool, error))?;
    let applied = connection
        .applied()
        .map_err(|error| operation_error(pool, error))?;
    set.verify(&applied)
        .map_err(|error| DatabaseError::new(ErrorKind::Other, error))?;
    Ok(applied)
}

/// Isolation requested when a transaction begins.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IsolationLevel {
    /// Whatever the adapter or server is already configured to use.
    #[default]
    Adapter,
    /// Uncommitted rows written by other transactions are visible.
    ReadUncommitted,
    /// Only committed rows are visible, and may change between statements.
    ReadCommitted,
    /// Rows read once stay stable for the whole transaction.
    RepeatableRead,
    /// The transaction runs as if it were the only one.
    Serializable,
}

/// Options applied when a transaction begins.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TransactionOptions {
    isolation: IsolationLevel,
    read_only: bool,
}

impl TransactionOptions {
    /// Creates options that keep the adapter's default isolation.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            isolation: IsolationLevel::Adapter,
            read_only: false,
        }
    }

    /// Requests an explicit isolation level.
    #[must_use]
    pub const fn with_isolation(mut self, isolation: IsolationLevel) -> Self {
        self.isolation = isolation;
        self
    }

    /// Declares the transaction read-only.
    #[must_use]
    pub const fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Returns the requested isolation level.
    #[must_use]
    pub const fn isolation(self) -> IsolationLevel {
        self.isolation
    }

    /// Returns whether the transaction was declared read-only.
    #[must_use]
    pub const fn is_read_only(self) -> bool {
        self.read_only
    }
}

/// Connection that can run explicit transactions.
///
/// Adapters implement this on their owned connection type.
/// [`Database::transaction`] drives it on one blocking worker, so `begin`,
/// the operation, and `commit` or `rollback` always share a single connection
/// and a single thread.
pub trait Transactional {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Begins a transaction with the requested options.
    ///
    /// # Errors
    ///
    /// Returns the driver failure raised by the begin statement, including an
    /// unsupported isolation level.
    fn begin(&mut self, options: TransactionOptions) -> Result<(), Self::Error>;

    /// Commits the open transaction.
    ///
    /// # Errors
    ///
    /// Returns the driver failure raised by the commit statement.
    fn commit(&mut self) -> Result<(), Self::Error>;

    /// Rolls back the open transaction.
    ///
    /// # Errors
    ///
    /// Returns the driver failure raised by the rollback statement.
    fn rollback(&mut self) -> Result<(), Self::Error>;
}

/// One ordered schema change identified by a version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Migration {
    version: u64,
    name: String,
    script: String,
    checksum: u64,
}

impl Migration {
    /// Creates a migration from an adapter-defined script.
    ///
    /// The script is opaque to this crate; the adapter decides how to execute
    /// it.
    #[must_use]
    pub fn new(version: u64, name: impl Into<String>, script: impl Into<String>) -> Self {
        let script = script.into();
        let checksum = checksum(&script);
        Self {
            version,
            name: name.into(),
            script,
            checksum,
        }
    }

    /// Returns the migration version.
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Returns the migration name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the adapter-defined script.
    #[must_use]
    pub fn script(&self) -> &str {
        &self.script
    }

    /// Returns the checksum recorded when the migration is applied.
    #[must_use]
    pub const fn checksum(&self) -> u64 {
        self.checksum
    }
}

/// An ordered migration set with strictly increasing versions.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MigrationSet {
    migrations: Vec<Migration>,
}

impl MigrationSet {
    /// Builds a set after checking versions strictly increase.
    ///
    /// # Errors
    ///
    /// Returns [`MigrationError::OutOfOrder`] when a version repeats or does
    /// not increase.
    pub fn new(migrations: impl IntoIterator<Item = Migration>) -> Result<Self, MigrationError> {
        let migrations: Vec<Migration> = migrations.into_iter().collect();
        for pair in migrations.windows(2) {
            if pair[1].version <= pair[0].version {
                return Err(MigrationError::OutOfOrder {
                    version: pair[1].version,
                });
            }
        }
        Ok(Self { migrations })
    }

    /// Returns every defined migration in version order.
    #[must_use]
    pub fn migrations(&self) -> &[Migration] {
        &self.migrations
    }

    /// Returns the highest defined version.
    #[must_use]
    pub fn latest_version(&self) -> Option<u64> {
        self.migrations.last().map(|migration| migration.version)
    }

    /// Returns the migrations defined after `applied_version`.
    #[must_use]
    pub fn pending(&self, applied_version: Option<u64>) -> &[Migration] {
        let Some(version) = applied_version else {
            return &self.migrations;
        };
        let index = self
            .migrations
            .partition_point(|migration| migration.version <= version);
        &self.migrations[index..]
    }

    /// Checks that `applied` is an unmodified prefix of this set.
    ///
    /// # Errors
    ///
    /// Returns [`MigrationError::Unknown`] for a recorded version this set does
    /// not define, [`MigrationError::Pending`] when a lower version was skipped,
    /// and [`MigrationError::ChecksumMismatch`] when a script changed after it
    /// was applied.
    pub fn verify(&self, applied: &[AppliedMigration]) -> Result<(), MigrationError> {
        for (index, record) in applied.iter().enumerate() {
            let Some(migration) = self.migrations.get(index) else {
                return Err(MigrationError::Unknown {
                    version: record.version,
                });
            };
            if migration.version != record.version {
                return Err(if self.defines(record.version) {
                    MigrationError::Pending {
                        version: migration.version,
                    }
                } else {
                    MigrationError::Unknown {
                        version: record.version,
                    }
                });
            }
            if migration.checksum != record.checksum {
                return Err(MigrationError::ChecksumMismatch {
                    version: record.version,
                });
            }
        }
        Ok(())
    }

    fn defines(&self, version: u64) -> bool {
        self.migrations
            .iter()
            .any(|migration| migration.version == version)
    }
}

/// A migration the adapter's ledger reports as already applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedMigration {
    /// Version recorded in the ledger.
    pub version: u64,
    /// Name recorded in the ledger.
    pub name: String,
    /// Checksum recorded when the migration ran.
    pub checksum: u64,
}

/// Version ledger an adapter maintains for [`Database::migrate`].
///
/// The adapter owns every statement: this crate never generates SQL. `apply`
/// must execute the script and record the version atomically so a crashed run
/// never leaves a half-applied version in the ledger.
pub trait MigrationRunner {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Creates the version ledger when it does not already exist.
    ///
    /// # Errors
    ///
    /// Returns the driver failure raised while creating the ledger.
    fn ensure_ledger(&mut self) -> Result<(), Self::Error>;

    /// Returns every recorded migration ordered by ascending version.
    ///
    /// # Errors
    ///
    /// Returns the driver failure raised while reading the ledger.
    fn applied(&mut self) -> Result<Vec<AppliedMigration>, Self::Error>;

    /// Applies one migration and records it in the ledger atomically.
    ///
    /// # Errors
    ///
    /// Returns the driver failure raised by the script or the ledger write.
    fn apply(&mut self, migration: &Migration) -> Result<(), Self::Error>;
}

/// Result of one [`Database::migrate`] run.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MigrationReport {
    /// Versions applied by this run, in order.
    pub applied: Vec<u64>,
    /// Highest version recorded in the ledger afterwards.
    pub current_version: Option<u64>,
}

/// Disagreement between a migration set and an adapter's ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationError {
    /// A version repeats or does not increase.
    OutOfOrder {
        /// The offending version.
        version: u64,
    },
    /// The ledger records a version the set does not define.
    Unknown {
        /// The recorded version.
        version: u64,
    },
    /// A recorded script changed after it was applied.
    ChecksumMismatch {
        /// The recorded version.
        version: u64,
    },
    /// A defined migration has not been applied.
    Pending {
        /// The unapplied version.
        version: u64,
    },
}

impl fmt::Display for MigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfOrder { version } => {
                write!(
                    formatter,
                    "migration {version} does not increase the version"
                )
            }
            Self::Unknown { version } => {
                write!(formatter, "ledger records undefined migration {version}")
            }
            Self::ChecksumMismatch { version } => {
                write!(
                    formatter,
                    "migration {version} changed after it was applied"
                )
            }
            Self::Pending { version } => {
                write!(formatter, "migration {version} has not been applied")
            }
        }
    }
}

impl std::error::Error for MigrationError {}

fn checksum(script: &str) -> u64 {
    script
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

/// Stable failure classes handlers match without knowing the driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    /// A connection could not be acquired or was lost mid-operation.
    Connection,
    /// An acquisition or an operation exceeded its deadline.
    Timeout,
    /// The driver rejected the statement for a constraint violation.
    Constraint,
    /// The connection pool or the blocking pool has no free capacity.
    Saturated,
    /// Any other adapter failure.
    Other,
}

impl ErrorKind {
    /// Returns a stable label for logs and metrics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Connection => "database connection failed",
            Self::Timeout => "database operation timed out",
            Self::Constraint => "database constraint violated",
            Self::Saturated => "database pool is saturated",
            Self::Other => "database operation failed",
        }
    }
}

/// A database failure carrying a stable class and the driver's own error.
///
/// The driver error is preserved rather than stringified, so an adapter's
/// concrete type is recoverable with [`DatabaseError::downcast_ref`] while
/// handlers keep matching on [`DatabaseError::kind`]. Blocking pool failures are
/// preserved the same way and downcast to [`BlockingError`].
#[derive(Debug)]
pub struct DatabaseError {
    kind: ErrorKind,
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl DatabaseError {
    /// Wraps a driver error under a stable class.
    #[must_use]
    pub fn new(kind: ErrorKind, error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            kind,
            source: Box::new(error),
        }
    }

    /// Returns the stable failure class.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Borrows the preserved driver error.
    #[must_use]
    pub fn source_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        &*self.source
    }

    /// Borrows the preserved driver error as its concrete type.
    #[must_use]
    pub fn downcast_ref<Error: std::error::Error + 'static>(&self) -> Option<&Error> {
        self.source.downcast_ref::<Error>()
    }

    /// Takes ownership of the preserved driver error.
    #[must_use]
    pub fn into_source(self) -> Box<dyn std::error::Error + Send + Sync> {
        self.source
    }
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = self.kind.label();
        let source = &self.source;
        write!(formatter, "{label}: {source}")
    }
}

impl std::error::Error for DatabaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

impl From<BlockingError> for DatabaseError {
    fn from(error: BlockingError) -> Self {
        let kind = match error {
            BlockingError::Saturated => ErrorKind::Saturated,
            BlockingError::Unavailable
            | BlockingError::Panicked
            | BlockingError::AlreadyConfigured => ErrorKind::Other,
        };
        Self::new(kind, error)
    }
}

/// A transaction operation that panicked and was rolled back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransactionPanic;

impl fmt::Display for TransactionPanic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("database transaction panicked and was rolled back")
    }
}

impl std::error::Error for TransactionPanic {}

/// An operation failure plus the rollback failure that followed it.
#[derive(Debug)]
pub struct RollbackFailure {
    operation: Box<dyn std::error::Error + Send + Sync>,
    rollback: Box<dyn std::error::Error + Send + Sync>,
}

impl RollbackFailure {
    fn new(
        operation: impl std::error::Error + Send + Sync + 'static,
        rollback: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            operation: Box::new(operation),
            rollback: Box::new(rollback),
        }
    }

    /// Borrows the operation error that started the rollback.
    #[must_use]
    pub fn operation(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        &*self.operation
    }

    /// Borrows the failure raised by the rollback itself.
    #[must_use]
    pub fn rollback(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        &*self.rollback
    }
}

impl fmt::Display for RollbackFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let operation = &self.operation;
        let rollback = &self.rollback;
        write!(formatter, "{operation}; rollback also failed: {rollback}")
    }
}

impl std::error::Error for RollbackFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.operation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::sync::Mutex;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum DriverError {
        Busy,
        Unique,
    }

    impl fmt::Display for DriverError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(match self {
                Self::Busy => "driver is busy",
                Self::Unique => "unique constraint violated",
            })
        }
    }

    impl std::error::Error for DriverError {}

    struct Pool;

    impl ConnectionPool for Pool {
        type Connection = usize;
        type Error = Infallible;

        fn acquire(&self) -> Result<Self::Connection, Self::Error> {
            Ok(40)
        }
    }

    #[derive(Default)]
    struct DriverPool {
        events: Arc<Mutex<Vec<String>>>,
        fail_rollback: bool,
    }

    struct DriverConnection {
        events: Arc<Mutex<Vec<String>>>,
        fail_rollback: bool,
        applied: Vec<AppliedMigration>,
        value: u64,
    }

    impl ConnectionPool for DriverPool {
        type Connection = DriverConnection;
        type Error = DriverError;

        fn acquire(&self) -> Result<Self::Connection, Self::Error> {
            Ok(DriverConnection {
                events: Arc::clone(&self.events),
                fail_rollback: self.fail_rollback,
                applied: Vec::new(),
                value: 40,
            })
        }

        fn acquire_timeout(&self, _timeout: Duration) -> Result<Self::Connection, Self::Error> {
            self.record("acquire_timeout");
            self.acquire()
        }

        fn health(&self) -> Option<PoolHealth> {
            Some(PoolHealth {
                in_use: 4,
                idle: 0,
                waiting: 1,
                max_size: Some(4),
            })
        }

        fn classify_acquire(&self, error: &Self::Error) -> ErrorKind {
            match error {
                DriverError::Busy => ErrorKind::Timeout,
                DriverError::Unique => ErrorKind::Other,
            }
        }

        fn classify(&self, error: &(dyn std::error::Error + 'static)) -> ErrorKind {
            match error.downcast_ref::<DriverError>() {
                Some(DriverError::Unique) => ErrorKind::Constraint,
                Some(DriverError::Busy) => ErrorKind::Timeout,
                None => ErrorKind::Other,
            }
        }
    }

    impl DriverPool {
        fn record(&self, event: &str) {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.to_owned());
        }
    }

    impl DriverConnection {
        fn record(&self, event: String) {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }
    }

    impl Transactional for DriverConnection {
        type Error = DriverError;

        fn begin(&mut self, options: TransactionOptions) -> Result<(), Self::Error> {
            let isolation = options.isolation();
            let read_only = options.is_read_only();
            self.record(format!("begin {isolation:?} read_only={read_only}"));
            Ok(())
        }

        fn commit(&mut self) -> Result<(), Self::Error> {
            self.record("commit".to_owned());
            Ok(())
        }

        fn rollback(&mut self) -> Result<(), Self::Error> {
            self.record("rollback".to_owned());
            if self.fail_rollback {
                return Err(DriverError::Busy);
            }
            Ok(())
        }
    }

    impl MigrationRunner for DriverConnection {
        type Error = DriverError;

        fn ensure_ledger(&mut self) -> Result<(), Self::Error> {
            self.record("ensure_ledger".to_owned());
            Ok(())
        }

        fn applied(&mut self) -> Result<Vec<AppliedMigration>, Self::Error> {
            Ok(self.applied.clone())
        }

        fn apply(&mut self, migration: &Migration) -> Result<(), Self::Error> {
            let version = migration.version();
            self.record(format!("apply {version}"));
            self.applied.push(AppliedMigration {
                version,
                name: migration.name().to_owned(),
                checksum: migration.checksum(),
            });
            Ok(())
        }
    }

    fn recorded(events: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn migration_set() -> MigrationSet {
        MigrationSet::new([
            Migration::new(1, "create", "create table jobs"),
            Migration::new(2, "index", "create index jobs_idx"),
        ])
        .expect("ordered set")
    }

    #[test]
    fn synchronous_orm_work_uses_the_bounded_pool() {
        let database = Database::new(Pool);
        let result = futures_lite::future::block_on(database.run(|connection| {
            *connection += 2;
            Ok::<_, Infallible>(*connection)
        }))
        .expect("database operation");
        assert_eq!(result, 42);
    }

    #[test]
    fn query_errors_keep_their_class_and_driver_type() {
        let database = Database::new(DriverPool::default());
        let error =
            futures_lite::future::block_on(database.run(|_| Err::<(), _>(DriverError::Unique)))
                .expect_err("constraint failure");
        assert_eq!(error.kind(), ErrorKind::Constraint);
        assert_eq!(
            error.downcast_ref::<DriverError>(),
            Some(&DriverError::Unique)
        );
    }

    #[test]
    fn configured_acquire_timeout_reaches_the_adapter() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let database = Database::new(DriverPool {
            events: Arc::clone(&events),
            fail_rollback: false,
        })
        .with_acquire_timeout(Duration::from_millis(250));
        assert_eq!(database.acquire_timeout(), Some(Duration::from_millis(250)));
        futures_lite::future::block_on(database.run(|_| Ok::<_, DriverError>(())))
            .expect("database operation");
        assert_eq!(recorded(&events), vec!["acquire_timeout".to_owned()]);
    }

    #[test]
    fn pool_health_reports_saturation() {
        let database = Database::new(DriverPool::default());
        let health = database.health().expect("reported health");
        assert!(health.is_saturated());
        assert!(!PoolHealth::default().is_saturated());
    }

    #[test]
    fn committed_transaction_reports_the_requested_isolation() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let database = Database::new(DriverPool {
            events: Arc::clone(&events),
            fail_rollback: false,
        });
        let options = TransactionOptions::new()
            .with_isolation(IsolationLevel::Serializable)
            .read_only();
        let value = futures_lite::future::block_on(database.transaction(options, |connection| {
            connection.value += 2;
            Ok::<_, DriverError>(connection.value)
        }))
        .expect("transaction");
        assert_eq!(value, 42);
        assert_eq!(
            recorded(&events),
            vec![
                "begin Serializable read_only=true".to_owned(),
                "commit".to_owned(),
            ]
        );
    }

    #[test]
    fn failed_transaction_rolls_back() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let database = Database::new(DriverPool {
            events: Arc::clone(&events),
            fail_rollback: false,
        });
        let error =
            futures_lite::future::block_on(database.transaction(TransactionOptions::new(), |_| {
                Err::<(), _>(DriverError::Unique)
            }))
            .expect_err("rolled back");
        assert_eq!(error.kind(), ErrorKind::Constraint);
        assert_eq!(
            recorded(&events),
            vec![
                "begin Adapter read_only=false".to_owned(),
                "rollback".to_owned(),
            ]
        );
    }

    #[test]
    fn rollback_failure_is_reported_with_the_operation_error() {
        let database = Database::new(DriverPool {
            events: Arc::new(Mutex::new(Vec::new())),
            fail_rollback: true,
        });
        let error =
            futures_lite::future::block_on(database.transaction(TransactionOptions::new(), |_| {
                Err::<(), _>(DriverError::Unique)
            }))
            .expect_err("rollback failure");
        assert_eq!(error.kind(), ErrorKind::Constraint);
        let failure = error
            .downcast_ref::<RollbackFailure>()
            .expect("preserved rollback failure");
        assert_eq!(
            failure.operation().to_string(),
            "unique constraint violated"
        );
        assert_eq!(failure.rollback().to_string(), "driver is busy");
    }

    #[test]
    fn panicking_transaction_rolls_back() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let database = Database::new(DriverPool {
            events: Arc::clone(&events),
            fail_rollback: false,
        });
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let error = futures_lite::future::block_on(
            database.transaction(TransactionOptions::new(), |_| -> Result<(), DriverError> {
                panic!("handler panic")
            }),
        )
        .expect_err("rolled back");
        std::panic::set_hook(previous);
        assert!(error.downcast_ref::<TransactionPanic>().is_some());
        assert_eq!(
            recorded(&events),
            vec![
                "begin Adapter read_only=false".to_owned(),
                "rollback".to_owned(),
            ]
        );
    }

    #[test]
    fn migrations_apply_in_order_and_verify() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let database = Database::new(DriverPool {
            events: Arc::clone(&events),
            fail_rollback: false,
        });
        let report = futures_lite::future::block_on(database.migrate(migration_set()))
            .expect("migration run");
        assert_eq!(report.applied, vec![1, 2]);
        assert_eq!(report.current_version, Some(2));
        assert_eq!(
            recorded(&events),
            vec![
                "ensure_ledger".to_owned(),
                "apply 1".to_owned(),
                "apply 2".to_owned(),
            ]
        );
    }

    #[test]
    fn verification_reports_pending_migrations() {
        let database = Database::new(DriverPool::default());
        let error = futures_lite::future::block_on(database.verify_migrations(migration_set()))
            .expect_err("pending migration");
        assert_eq!(
            error.downcast_ref::<MigrationError>(),
            Some(&MigrationError::Pending { version: 1 })
        );
    }

    #[test]
    fn changed_scripts_fail_verification() {
        let set = migration_set();
        let applied = vec![AppliedMigration {
            version: 1,
            name: "create".to_owned(),
            checksum: 0,
        }];
        assert_eq!(
            set.verify(&applied),
            Err(MigrationError::ChecksumMismatch { version: 1 })
        );
    }

    #[test]
    fn skipped_and_undefined_versions_fail_verification() {
        let set = migration_set();
        let skipped = vec![AppliedMigration {
            version: 2,
            name: "index".to_owned(),
            checksum: set.migrations()[1].checksum(),
        }];
        assert_eq!(
            set.verify(&skipped),
            Err(MigrationError::Pending { version: 1 })
        );
        let undefined = vec![AppliedMigration {
            version: 9,
            name: "ghost".to_owned(),
            checksum: 0,
        }];
        assert_eq!(
            set.verify(&undefined),
            Err(MigrationError::Unknown { version: 9 })
        );
    }

    #[test]
    fn migration_sets_reject_repeated_versions() {
        let error = MigrationSet::new([
            Migration::new(1, "create", "create table jobs"),
            Migration::new(1, "again", "create table jobs"),
        ])
        .expect_err("repeated version");
        assert_eq!(error, MigrationError::OutOfOrder { version: 1 });
    }

    #[test]
    fn pending_selects_migrations_after_the_ledger() {
        let set = migration_set();
        assert_eq!(set.pending(None).len(), 2);
        assert_eq!(set.pending(Some(1)).len(), 1);
        assert_eq!(set.pending(Some(2)).len(), 0);
        assert_eq!(set.latest_version(), Some(2));
    }

    /// Work reaching the pool from a pool worker runs on that worker.
    ///
    /// Re-submitting instead is what deadlocks a saturated pool: the worker
    /// would await a job that needs a worker to be free. The thread identity is
    /// the observable proof that no second hop happened.
    #[test]
    fn work_submitted_from_a_pool_worker_runs_inline() {
        let database = Database::new(Pool);
        let (worker, ran_on, value) = futures_lite::future::block_on(run_blocking(move || {
            let worker = std::thread::current().id();
            let (value, ran_on) = futures_lite::future::block_on(database.run(|connection| {
                *connection += 2;
                Ok::<_, Infallible>((*connection, std::thread::current().id()))
            }))
            .expect("inline database operation");
            (worker, ran_on, value)
        }))
        .expect("outer blocking job");
        assert_eq!(
            worker, ran_on,
            "the operation must not hop to a second worker"
        );
        assert_eq!(value, 42);
    }

    #[test]
    fn an_inline_panic_is_still_reported_as_a_blocking_failure() {
        let database = Database::new(Pool);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let error = futures_lite::future::block_on(run_blocking(move || {
            futures_lite::future::block_on(
                database.run(|_| -> Result<(), Infallible> { panic!("operation panic") }),
            )
            .expect_err("the panic is reported, not unwound into the worker")
        }))
        .expect("outer blocking job");
        std::panic::set_hook(previous);
        assert_eq!(error.kind(), ErrorKind::Other);
        assert_eq!(
            error.downcast_ref::<BlockingError>(),
            Some(&BlockingError::Panicked)
        );
    }

    #[test]
    fn the_sync_entry_points_run_on_the_calling_thread() {
        let caller = std::thread::current().id();
        let database = Database::new(Pool);
        let (value, ran_on) = database
            .run_sync(|connection| {
                *connection += 2;
                Ok::<_, Infallible>((*connection, std::thread::current().id()))
            })
            .expect("synchronous operation");
        assert_eq!(value, 42);
        assert_eq!(ran_on, caller);

        let events = Arc::new(Mutex::new(Vec::new()));
        let transactional = Database::new(DriverPool {
            events: Arc::clone(&events),
            fail_rollback: false,
        });
        let value = transactional
            .transaction_sync(TransactionOptions::new(), |connection| {
                connection.value += 2;
                Ok::<_, DriverError>(connection.value)
            })
            .expect("synchronous transaction");
        assert_eq!(value, 42);
        assert_eq!(
            recorded(&events),
            vec![
                "begin Adapter read_only=false".to_owned(),
                "commit".to_owned(),
            ]
        );
    }

    #[test]
    fn blocking_failures_keep_their_class() {
        let error = DatabaseError::from(BlockingError::Saturated);
        assert_eq!(error.kind(), ErrorKind::Saturated);
        assert_eq!(
            error.downcast_ref::<BlockingError>(),
            Some(&BlockingError::Saturated)
        );
    }
}
