//! Native HTTP/1 adapter for Blazingly.
//!
//! The adapter deliberately contains every socket-runtime and wire-protocol
//! dependency. The operation graph, router, DI, MCP, and documentation crates
//! remain runtime-neutral. Tokio is not part of this crate's dependency tree.

#[cfg(feature = "http2")]
mod http2;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::io::{self, Write as _};
use std::net::{SocketAddr, ToSocketAddrs};
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::rc::Rc;
#[cfg(feature = "tls")]
use std::sync::Arc as TlsArc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use blazingly_core::{BackgroundTask, BodyStream, BodyStreamError, HttpMethod, HttpUpgrade};
use blazingly_core::{StreamingBody, UpgradeIoError, UpgradedIo};
use blazingly_executor::{ExecutableApp, InvocationControl};
pub use blazingly_http::HttpMiddleware;
use blazingly_http::{HttpApp, HttpRequestView, Response};
use blazingly_openapi::OpenApiConfig;
use blazingly_wire::{BodyFraming, ChunkDecoder, HeaderPositions, reason_phrase};
use blazingly_wire::{StreamingChunk, StreamingChunkDecoder};
use compio::dispatcher::Dispatcher;
#[cfg(any(feature = "http2", feature = "tls"))]
use compio::io::compat::AsyncStream;
use compio::io::{AsyncReadExt as CompioAsyncReadExt, AsyncWriteExt as CompioAsyncWriteExt};
use compio::net::TcpListener;
use compio::net::TcpStream;
use compio::runtime::{Runtime, spawn};
use futures_lite::future;
use futures_lite::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Default maximum request body accepted by the socket adapter.
pub const DEFAULT_MAX_BODY_BYTES: usize = blazingly_wire::DEFAULT_MAX_BODY_BYTES;

const DEFAULT_MAX_HEADER_BYTES: usize = blazingly_wire::DEFAULT_MAX_HEADER_BYTES;
const DEFAULT_MAX_HEADERS: usize = blazingly_wire::DEFAULT_MAX_HEADERS;
const DEFAULT_MAX_CHUNKS: usize = blazingly_wire::DEFAULT_MAX_CHUNKS;
const DEFAULT_MAX_PIPELINE_BATCH: usize = 16;
const MAX_PIPELINE_WRITE_BYTES: usize = 64 * 1024;
const MAX_HEADER_CAPACITY: usize = blazingly_wire::MAX_HEADER_CAPACITY;
const READ_CHUNK_BYTES: usize = 8 * 1024;
const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(75);
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
static NEXT_MULTICORE_SERVER_ID: AtomicU64 = AtomicU64::new(1);
static DATE_UPDATER: Once = Once::new();
static DATE_GENERATION: AtomicU64 = AtomicU64::new(0);
static DATE_VALUE: Mutex<String> = Mutex::new(String::new());

thread_local! {
    static WORKER_APPS: RefCell<std::collections::HashMap<u64, Rc<HttpApp>>> =
        RefCell::new(std::collections::HashMap::new());
    static DRAIN_COUNTER: RefCell<Option<Arc<AtomicUsize>>> = const { RefCell::new(None) };
    static DATE_CACHE: RefCell<CachedDate> = const {
        RefCell::new(CachedDate {
            generation: u64::MAX,
            value: String::new(),
        })
    };
}

struct CachedDate {
    generation: u64,
    value: String,
}

/// Wire-level resource limits for the native protocol adapters.
///
/// These limits are enforced before an operation is dispatched. They therefore
/// protect every application independently of its extractors or business code.
/// Header/body limits apply to both protocols; chunk and keep-alive request
/// counts are HTTP/1-specific, while concurrent streams are HTTP/2-specific.
/// The socket deadlines bound a peer that opens a connection and then stops
/// making progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct ServerLimits {
    max_header_bytes: usize,
    max_headers: usize,
    max_body_bytes: usize,
    max_chunks: usize,
    max_pipeline_batch: usize,
    max_requests_per_connection: Option<NonZeroUsize>,
    header_read_timeout: Duration,
    body_read_timeout: Duration,
    idle_timeout: Duration,
    write_timeout: Duration,
    #[cfg(feature = "http2")]
    max_concurrent_streams: usize,
}

impl ServerLimits {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            max_headers: DEFAULT_MAX_HEADERS,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_chunks: DEFAULT_MAX_CHUNKS,
            max_pipeline_batch: DEFAULT_MAX_PIPELINE_BATCH,
            max_requests_per_connection: None,
            header_read_timeout: DEFAULT_HEADER_READ_TIMEOUT,
            body_read_timeout: DEFAULT_BODY_READ_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            write_timeout: DEFAULT_WRITE_TIMEOUT,
            #[cfg(feature = "http2")]
            max_concurrent_streams: 100,
        }
    }

    #[must_use]
    pub const fn with_max_header_bytes(mut self, bytes: usize) -> Self {
        assert!(bytes > 0, "max_header_bytes must be greater than zero");
        self.max_header_bytes = bytes;
        self
    }

    /// Sets the maximum header count.
    ///
    /// The native parser keeps its header slots on the stack, so this value is
    /// capped at 128 instead of allocating once per request.
    #[must_use]
    pub const fn with_max_headers(mut self, count: usize) -> Self {
        assert!(count > 0, "max_headers must be greater than zero");
        assert!(
            count <= MAX_HEADER_CAPACITY,
            "max_headers cannot exceed the native stack capacity"
        );
        self.max_headers = count;
        self
    }

    #[must_use]
    pub const fn with_max_body_bytes(mut self, bytes: usize) -> Self {
        self.max_body_bytes = bytes;
        self
    }

    /// Limits the number of chunks in one chunked HTTP/1 request.
    #[must_use]
    pub const fn with_max_chunks(mut self, count: usize) -> Self {
        assert!(count > 0, "max_chunks must be greater than zero");
        self.max_chunks = count;
        self
    }

    /// Bounds the number of pipelined HTTP/1 responses coalesced into one
    /// socket write. A value of one disables response coalescing.
    #[must_use]
    pub const fn with_max_pipeline_batch(mut self, count: NonZeroUsize) -> Self {
        self.max_pipeline_batch = count.get();
        self
    }

    /// Bounds keep-alive reuse. `None` allows requests until either peer closes.
    #[must_use]
    pub const fn with_max_requests_per_connection(mut self, count: Option<NonZeroUsize>) -> Self {
        self.max_requests_per_connection = count;
        self
    }

    /// Bounds how long a complete request head may take to arrive once its
    /// first byte has been buffered.
    #[must_use]
    pub const fn with_header_read_timeout(mut self, timeout: Duration) -> Self {
        self.header_read_timeout = timeout;
        self
    }

    /// Bounds how long one request-body read may stall.
    #[must_use]
    pub const fn with_body_read_timeout(mut self, timeout: Duration) -> Self {
        self.body_read_timeout = timeout;
        self
    }

    /// Bounds how long an idle keep-alive connection may wait for the first
    /// byte of its next request.
    #[must_use]
    pub const fn with_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Bounds how long one response write may stall.
    #[must_use]
    pub const fn with_write_timeout(mut self, timeout: Duration) -> Self {
        self.write_timeout = timeout;
        self
    }

    #[cfg(feature = "http2")]
    #[must_use]
    pub const fn with_max_concurrent_streams(mut self, count: NonZeroUsize) -> Self {
        self.max_concurrent_streams = count.get();
        self
    }

    #[must_use]
    pub const fn max_header_bytes(self) -> usize {
        self.max_header_bytes
    }

    #[must_use]
    pub const fn max_headers(self) -> usize {
        self.max_headers
    }

    #[must_use]
    pub const fn max_body_bytes(self) -> usize {
        self.max_body_bytes
    }

    #[must_use]
    pub const fn max_chunks(self) -> usize {
        self.max_chunks
    }

    #[must_use]
    pub const fn max_pipeline_batch(self) -> usize {
        self.max_pipeline_batch
    }

    #[must_use]
    pub const fn max_requests_per_connection(self) -> Option<NonZeroUsize> {
        self.max_requests_per_connection
    }

    #[must_use]
    pub const fn header_read_timeout(self) -> Duration {
        self.header_read_timeout
    }

    #[must_use]
    pub const fn body_read_timeout(self) -> Duration {
        self.body_read_timeout
    }

    #[must_use]
    pub const fn idle_timeout(self) -> Duration {
        self.idle_timeout
    }

    #[must_use]
    pub const fn write_timeout(self) -> Duration {
        self.write_timeout
    }

    #[cfg(feature = "http2")]
    #[must_use]
    pub const fn max_concurrent_streams(self) -> usize {
        self.max_concurrent_streams
    }
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe trigger used to stop accepting connections.
#[derive(Clone)]
pub struct ShutdownHandle {
    state: Arc<ShutdownState>,
}

/// Future resolved when its paired [`ShutdownHandle`] is triggered.
pub struct ShutdownSignal {
    state: Arc<ShutdownState>,
}

struct ShutdownState {
    requested: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

/// Creates one explicit graceful-shutdown channel.
#[must_use]
pub fn shutdown_channel() -> (ShutdownHandle, ShutdownSignal) {
    let state = Arc::new(ShutdownState {
        requested: AtomicBool::new(false),
        waker: Mutex::new(None),
    });
    (
        ShutdownHandle {
            state: Arc::clone(&state),
        },
        ShutdownSignal { state },
    )
}

/// Creates a graceful-shutdown channel triggered by process termination.
///
/// This installs one process-global handler for Ctrl-C and, on Unix,
/// `SIGTERM`/`SIGHUP`. It is suitable for Kubernetes pod termination and
/// returns an error when another library already installed a `ctrlc` handler.
///
/// # Errors
///
/// Returns an operating-system error when the process handler cannot be
/// installed.
pub fn termination_channel() -> io::Result<(ShutdownHandle, ShutdownSignal)> {
    let (handle, signal) = shutdown_channel();
    let termination = handle.clone();
    ctrlc::set_handler(move || termination.shutdown())
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok((handle, signal))
}

impl ShutdownHandle {
    pub fn shutdown(&self) {
        self.state.requested.store(true, Ordering::Release);
        if let Some(waker) = self
            .state
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            waker.wake();
        }
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.state.requested.load(Ordering::Acquire)
    }
}

impl Future for ShutdownSignal {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.state.requested.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        *self
            .state
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(context.waker().clone());
        if self.state.requested.load(Ordering::Acquire) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// A native HTTP/1 server whose request execution remains thread-local.
///
/// Keeping one compiled app per worker means operation futures, plugin hooks,
/// and request-scoped dependencies do not need `Send` or `Sync`.
pub struct Server {
    app: Rc<HttpApp>,
    limits: ServerLimits,
    request_timeout: Option<Duration>,
    max_connections: Option<NonZeroUsize>,
    tcp_nodelay: bool,
    #[cfg(feature = "tls")]
    tls_acceptor: Option<compio::tls::TlsAcceptor>,
}

impl Server {
    #[must_use]
    pub fn new(app: ExecutableApp) -> Self {
        Self {
            app: Rc::new(HttpApp::new(app)),
            limits: ServerLimits::new(),
            request_timeout: None,
            max_connections: None,
            tcp_nodelay: true,
            #[cfg(feature = "tls")]
            tls_acceptor: None,
        }
    }

    #[must_use]
    pub fn with_max_body_bytes(mut self, max_body_bytes: usize) -> Self {
        self.app = Rc::new(
            Rc::try_unwrap(self.app)
                .unwrap_or_else(|_| unreachable!("server app is not shared before serving"))
                .with_max_body_bytes(max_body_bytes),
        );
        self.limits = self.limits.with_max_body_bytes(max_body_bytes);
        self
    }

    /// Registers runtime-neutral HTTP middleware in registration order.
    ///
    /// Middleware runs for every request this server dispatches, so header
    /// policy, compression, rate limiting, and security layers apply on the
    /// socket path and not only in the in-memory test client.
    #[must_use]
    pub fn with_middleware(mut self, middleware: impl HttpMiddleware + 'static) -> Self {
        self.app = Rc::new(
            Rc::try_unwrap(self.app)
                .unwrap_or_else(|_| unreachable!("server app is not shared before serving"))
                .with_middleware(middleware),
        );
        self
    }

    /// Registers middleware whose state is shared with the caller.
    #[must_use]
    pub fn with_shared_middleware(mut self, middleware: Rc<dyn HttpMiddleware>) -> Self {
        self.app = Rc::new(
            Rc::try_unwrap(self.app)
                .unwrap_or_else(|_| unreachable!("server app is not shared before serving"))
                .with_shared_middleware(middleware),
        );
        self
    }

    /// Bounds concurrently served connections. `None` accepts without a cap.
    ///
    /// A connection accepted while the cap is reached is closed immediately
    /// instead of being dispatched.
    #[must_use]
    pub const fn with_max_connections(mut self, connections: Option<NonZeroUsize>) -> Self {
        self.max_connections = connections;
        self
    }

    /// Enables or disables `TCP_NODELAY` on accepted sockets. Enabled by
    /// default so a small response is not delayed by Nagle's algorithm.
    #[must_use]
    pub const fn with_tcp_nodelay(mut self, nodelay: bool) -> Self {
        self.tcp_nodelay = nodelay;
        self
    }

    #[must_use]
    pub fn with_limits(mut self, limits: ServerLimits) -> Self {
        self.app = Rc::new(
            Rc::try_unwrap(self.app)
                .unwrap_or_else(|_| unreachable!("server app is not shared before serving"))
                .with_max_body_bytes(limits.max_body_bytes),
        );
        self.limits = limits;
        self
    }

    /// Mounts precompiled `/openapi.json` and API reference UI assets.
    #[must_use]
    pub fn with_openapi(mut self, config: OpenApiConfig) -> Self {
        self.app = Rc::new(
            Rc::try_unwrap(self.app)
                .unwrap_or_else(|_| unreachable!("server app is not shared before serving"))
                .with_openapi(config),
        );
        self
    }

    /// Sets a wall-clock limit for provider, hook, and handler execution.
    ///
    /// Dependency finalizers and response hooks are shielded so cleanup still
    /// completes after the timeout fires.
    #[must_use]
    pub const fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    /// Enables TLS 1.2/1.3 using a caller-owned rustls server configuration.
    ///
    /// Set `alpn_protocols` to `http/1.1` (and later `h2`) on the supplied
    /// configuration before passing it to the server.
    #[cfg(feature = "tls")]
    #[must_use]
    pub fn with_tls_config(mut self, config: TlsArc<compio::tls::rustls::ServerConfig>) -> Self {
        self.tls_acceptor = Some(config.into());
        self
    }

    /// Binds and serves HTTP/1 connections until the process is stopped.
    ///
    /// # Errors
    ///
    /// Returns a socket error if address resolution, binding, or accepting a
    /// connection fails.
    pub fn serve(self, address: impl ToSocketAddrs) -> io::Result<()> {
        let address = resolve_address(address)?;
        let runtime = Runtime::new()?;
        runtime.block_on(async {
            let listener = TcpListener::bind(address).await?;
            self.serve_listener(listener, None, DEFAULT_DRAIN_TIMEOUT)
                .await
        })
    }

    /// Stops accepting when `shutdown` resolves, closes keep-alive
    /// connections after their current response, and drains active work.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if binding or accepting a connection fails.
    pub fn serve_gracefully(
        self,
        address: impl ToSocketAddrs,
        shutdown: ShutdownSignal,
        drain_timeout: Duration,
    ) -> io::Result<()> {
        let address = resolve_address(address)?;
        let runtime = Runtime::new()?;
        runtime.block_on(async {
            let listener = TcpListener::bind(address).await?;
            self.serve_listener(listener, Some(shutdown), drain_timeout)
                .await
        })
    }

    /// Serves HTTP/1 requests over a borrowed futures-I/O transport.
    ///
    /// This is useful for in-memory protocol tests. A borrowed transport cannot
    /// transfer ownership, so protocol upgrades are refused here; owned
    /// transports reach [`Self::serve`] and keep the upgrade capability.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the transport cannot be read or written.
    pub async fn serve_io<IO>(&self, io: &mut IO) -> io::Result<()>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        serve_connection(
            self.app.as_ref(),
            self.limits,
            io,
            None,
            "http",
            None,
            self.request_timeout,
            TransportOwnership::Borrowed,
        )
        .await
        .map(|_| ())
    }

    /// Serves HTTP/1 requests over an owned futures-I/O transport.
    ///
    /// Ownership stays with the connection loop, so a protocol upgrade moves
    /// the transport into its session handler instead of being refused. TLS
    /// connections and in-memory compatibility transports take this path.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the transport cannot be read or written, or the
    /// failure reported by an upgraded protocol session.
    pub async fn serve_owned_io<IO>(&self, io: IO) -> io::Result<()>
    where
        IO: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        serve_compat_connection(
            self.app.as_ref(),
            self.limits,
            io,
            None,
            "http",
            None,
            self.request_timeout,
        )
        .await
    }

    /// Serves an HTTP/2 connection over an arbitrary futures-I/O transport.
    ///
    /// This experimental surface uses a Sans-I/O HTTP/2 state machine. Normal
    /// socket serving also detects the HTTP/2 prior-knowledge preface and routes
    /// it here automatically; TLS clients select it with ALPN `h2`.
    ///
    /// # Errors
    ///
    /// Returns an I/O or HTTP/2 protocol error when the transport or peer
    /// cannot complete the connection.
    #[cfg(feature = "http2")]
    pub async fn serve_http2_io<IO>(&self, io: &mut IO) -> io::Result<()>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        http2::serve_connection(
            self.app.as_ref(),
            self.limits,
            io,
            None,
            "http",
            None,
            Vec::new(),
            self.request_timeout,
        )
        .await
    }

    fn connection_setup(&self) -> ConnectionSetup {
        ConnectionSetup {
            limits: self.limits,
            request_timeout: self.request_timeout,
            #[cfg(feature = "tls")]
            tls_acceptor: self.tls_acceptor.clone(),
        }
    }

    async fn serve_listener(
        &self,
        listener: TcpListener,
        shutdown: Option<ShutdownSignal>,
        drain_timeout: Duration,
    ) -> io::Result<()> {
        self.app
            .startup()
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;
        let active = Arc::new(AtomicUsize::new(0));
        let connections = Arc::new(AtomicUsize::new(0));
        let _drain_scope = DrainScope::new(&active);
        let shutdown_state = shutdown
            .as_ref()
            .map(|shutdown| Arc::clone(&shutdown.state));
        let mut shutdown = shutdown.map(Box::pin);
        loop {
            let accepted = if let Some(shutdown) = shutdown.as_mut() {
                future::race(
                    async { AcceptEvent::Connection(listener.accept().await) },
                    async {
                        shutdown.as_mut().await;
                        AcceptEvent::Shutdown
                    },
                )
                .await
            } else {
                AcceptEvent::Connection(listener.accept().await)
            };
            let (stream, peer_addr) = match accepted {
                AcceptEvent::Connection(result) => result?,
                AcceptEvent::Shutdown => break,
            };
            if self
                .max_connections
                .is_some_and(|limit| connections.load(Ordering::Acquire) >= limit.get())
            {
                drop(stream);
                continue;
            }
            // A kernel that refuses the option still serves correct HTTP.
            let _ = stream.set_nodelay(self.tcp_nodelay);
            let app = Rc::clone(&self.app);
            let setup = self.connection_setup();
            active.fetch_add(1, Ordering::AcqRel);
            connections.fetch_add(1, Ordering::AcqRel);
            let active_for_task = Arc::clone(&active);
            let connections_for_task = Arc::clone(&connections);
            let connection_shutdown = shutdown_state.clone();
            spawn(async move {
                let _work = ActiveWork::adopt(active_for_task);
                let _slot = ActiveWork::adopt(connections_for_task);
                serve_accepted(
                    app.as_ref(),
                    stream,
                    peer_addr,
                    connection_shutdown.as_deref(),
                    setup,
                )
                .await;
            })
            .detach();
        }
        let drain = async {
            while active.load(Ordering::Acquire) != 0 {
                compio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let _ = compio::time::timeout(drain_timeout, drain).await;
        self.app
            .shutdown()
            .await
            .map_err(|error| io::Error::other(error.to_string()))
    }
}

/// Thread-per-core launcher with one compiled, non-`Send` app per worker.
///
/// The factory runs lazily once on every worker. Requests execute as local
/// futures, preserving thread-local handlers and DI values.
pub struct MulticoreServer<Factory> {
    factory: Factory,
    workers: NonZeroUsize,
    limits: ServerLimits,
    request_timeout: Option<Duration>,
    openapi: Option<OpenApiConfig>,
    middleware: Option<MiddlewareFactory>,
    max_connections: Option<NonZeroUsize>,
    tcp_nodelay: bool,
    #[cfg(feature = "tls")]
    tls_acceptor: Option<compio::tls::TlsAcceptor>,
}

impl<Factory> MulticoreServer<Factory>
where
    Factory: Fn() -> ExecutableApp + Send + Sync + 'static,
{
    #[must_use]
    pub fn new(workers: NonZeroUsize, factory: Factory) -> Self {
        Self {
            factory,
            workers,
            limits: ServerLimits::new(),
            request_timeout: None,
            openapi: None,
            middleware: None,
            max_connections: None,
            tcp_nodelay: true,
            #[cfg(feature = "tls")]
            tls_acceptor: None,
        }
    }

    #[must_use]
    pub const fn with_max_body_bytes(mut self, max_body_bytes: usize) -> Self {
        self.limits = self.limits.with_max_body_bytes(max_body_bytes);
        self
    }

    #[must_use]
    pub const fn with_limits(mut self, limits: ServerLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets a wall-clock limit for provider, hook, and handler execution.
    #[must_use]
    pub const fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    /// Mounts the same precompiled `OpenAPI` assets in every worker-local app.
    #[must_use]
    pub fn with_openapi(mut self, config: OpenApiConfig) -> Self {
        self.openapi = Some(config);
        self
    }

    /// Builds worker-local middleware once per worker, in registration order.
    ///
    /// Middleware is `!Send`, so it cannot be shared across workers. The
    /// factory runs on each worker thread and its layers stay thread-local,
    /// exactly like the compiled app produced by the application factory.
    #[must_use]
    pub fn with_middleware_factory<Middleware>(mut self, middleware: Middleware) -> Self
    where
        Middleware: Fn() -> Vec<Rc<dyn HttpMiddleware>> + Send + Sync + 'static,
    {
        self.middleware = Some(Arc::new(middleware));
        self
    }

    /// Bounds concurrently served connections. `None` accepts without a cap.
    ///
    /// A connection accepted while the cap is reached is closed immediately
    /// instead of being dispatched to a worker.
    #[must_use]
    pub const fn with_max_connections(mut self, connections: Option<NonZeroUsize>) -> Self {
        self.max_connections = connections;
        self
    }

    /// Enables or disables `TCP_NODELAY` on accepted sockets. Enabled by
    /// default so a small response is not delayed by Nagle's algorithm.
    #[must_use]
    pub const fn with_tcp_nodelay(mut self, nodelay: bool) -> Self {
        self.tcp_nodelay = nodelay;
        self
    }

    #[cfg(feature = "tls")]
    #[must_use]
    pub fn with_tls_config(mut self, config: TlsArc<compio::tls::rustls::ServerConfig>) -> Self {
        self.tls_acceptor = Some(config.into());
        self
    }

    /// Runs until the listener fails or the process stops.
    ///
    /// Every worker's app is compiled and started before the first connection
    /// is accepted, so a failing startup hook refuses to boot instead of
    /// silently dropping connections.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when binding, worker startup, accepting, or
    /// dispatching fails.
    pub fn serve(self, address: impl ToSocketAddrs) -> io::Result<()> {
        self.serve_inner(address, None, DEFAULT_DRAIN_TIMEOUT)
    }

    /// Stops accepting after `shutdown`, asks keep-alive connections to close
    /// after the current response, and drains active work.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when binding, worker startup, accepting, or
    /// dispatching fails.
    #[allow(clippy::needless_pass_by_value)]
    pub fn serve_gracefully(
        self,
        address: impl ToSocketAddrs,
        shutdown: ShutdownSignal,
        drain_timeout: Duration,
    ) -> io::Result<()> {
        self.serve_inner(address, Some(&shutdown.state), drain_timeout)
    }

    #[allow(clippy::too_many_lines)]
    fn serve_inner(
        self,
        address: impl ToSocketAddrs,
        shutdown: Option<&Arc<ShutdownState>>,
        drain_timeout: Duration,
    ) -> io::Result<()> {
        let address = resolve_address(address)?;
        let listener = std::net::TcpListener::bind(address)?;
        listener.set_nonblocking(shutdown.is_some())?;
        // One independent queue/runtime per worker avoids the startup skew of
        // a shared MPMC receiver when tasks are long-lived keep-alive
        // connections. Explicit round-robin keeps connection ownership stable.
        let dispatchers = (0..self.workers.get())
            .map(|index| {
                Dispatcher::builder()
                    .worker_threads(NonZeroUsize::new(1).expect("one worker is always non-zero"))
                    .thread_names(move |_| format!("blazingly-worker-{index}"))
                    .build()
            })
            .collect::<io::Result<Vec<_>>>()?;
        let limits = self.limits;
        let request_timeout = self.request_timeout;
        let max_connections = self.max_connections;
        let tcp_nodelay = self.tcp_nodelay;
        #[cfg(feature = "tls")]
        let tls_acceptor = self.tls_acceptor;
        let config = Arc::new(WorkerConfig {
            factory: self.factory,
            max_body_bytes: limits.max_body_bytes,
            openapi: self.openapi,
            middleware: self.middleware,
        });
        let active = Arc::new(AtomicUsize::new(0));
        let connections = Arc::new(AtomicUsize::new(0));
        let server_id = NEXT_MULTICORE_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let mut next_worker = 0_usize;

        if let Err(error) = start_workers(&dispatchers, server_id, &config, &active) {
            for dispatcher in dispatchers {
                let _ = future::block_on(dispatcher.join());
            }
            return Err(error);
        }

        loop {
            if shutdown
                .as_ref()
                .is_some_and(|shutdown| shutdown.requested.load(Ordering::Acquire))
            {
                break;
            }
            let (stream, peer_addr) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if shutdown.is_some() && error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(error) => return Err(error),
            };

            if max_connections
                .is_some_and(|limit| connections.load(Ordering::Acquire) >= limit.get())
            {
                drop(stream);
                continue;
            }
            // A kernel that refuses the option still serves correct HTTP.
            let _ = stream.set_nodelay(tcp_nodelay);
            let config = Arc::clone(&config);
            let active_for_task = Arc::clone(&active);
            let connections_for_task = Arc::clone(&connections);
            let connection_shutdown = shutdown.cloned();
            let setup = ConnectionSetup {
                limits,
                request_timeout,
                #[cfg(feature = "tls")]
                tls_acceptor: tls_acceptor.clone(),
            };
            active.fetch_add(1, Ordering::AcqRel);
            connections.fetch_add(1, Ordering::AcqRel);
            let dispatcher = &dispatchers[next_worker];
            next_worker = (next_worker + 1) % dispatchers.len();
            if dispatcher
                .dispatch(move || async move {
                    let _work = ActiveWork::adopt(active_for_task);
                    let _slot = ActiveWork::adopt(connections_for_task);
                    let app = worker_app(server_id, config.as_ref()).0;
                    let Ok(stream) = compio::net::TcpStream::from_std(stream) else {
                        return;
                    };
                    serve_accepted(
                        app.as_ref(),
                        stream,
                        peer_addr,
                        connection_shutdown.as_deref(),
                        setup,
                    )
                    .await;
                })
                .is_err()
            {
                active.fetch_sub(1, Ordering::AcqRel);
                connections.fetch_sub(1, Ordering::AcqRel);
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "all Compio workers stopped",
                ));
            }
        }

        let deadline = Instant::now() + drain_timeout;
        while active.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut shutdown_tasks = Vec::with_capacity(dispatchers.len());
        for dispatcher in &dispatchers {
            let completion = dispatcher
                .dispatch(move || async move {
                    clear_drain_counter();
                    if let Some(app) = take_worker_app(server_id) {
                        let _ = app.shutdown().await;
                    }
                })
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "Compio worker stopped before application shutdown",
                    )
                })?;
            shutdown_tasks.push(completion);
        }
        for completion in shutdown_tasks {
            future::block_on(completion).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Compio worker did not complete application shutdown",
                )
            })?;
        }
        for dispatcher in dispatchers {
            future::block_on(dispatcher.join())?;
        }
        Ok(())
    }
}

/// Builds worker-local middleware once on every worker thread.
type MiddlewareFactory = Arc<dyn Fn() -> Vec<Rc<dyn HttpMiddleware>> + Send + Sync>;

/// Everything a worker needs to compile its own non-`Send` application.
struct WorkerConfig<Factory> {
    factory: Factory,
    max_body_bytes: usize,
    openapi: Option<OpenApiConfig>,
    middleware: Option<MiddlewareFactory>,
}

/// Compiles and starts one app per worker before the accept loop begins.
fn start_workers<Factory>(
    dispatchers: &[Dispatcher],
    server_id: u64,
    config: &Arc<WorkerConfig<Factory>>,
    active: &Arc<AtomicUsize>,
) -> io::Result<()>
where
    Factory: Fn() -> ExecutableApp + Send + Sync + 'static,
{
    let mut started = Vec::with_capacity(dispatchers.len());
    for dispatcher in dispatchers {
        let config = Arc::clone(config);
        let active = Arc::clone(active);
        let completion = dispatcher
            .dispatch(move || async move {
                install_drain_counter(&active);
                let (app, created) = worker_app(server_id, config.as_ref());
                if created {
                    return app.startup().await.map_err(|error| error.to_string());
                }
                Ok(())
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Compio worker stopped before application startup",
                )
            })?;
        started.push(completion);
    }
    for completion in started {
        future::block_on(completion)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Compio worker did not complete application startup",
                )
            })?
            .map_err(io::Error::other)?;
    }
    Ok(())
}

fn worker_app<Factory>(server_id: u64, config: &WorkerConfig<Factory>) -> (Rc<HttpApp>, bool)
where
    Factory: Fn() -> ExecutableApp,
{
    WORKER_APPS.with(|apps| {
        let mut apps = apps.borrow_mut();
        if let Some(app) = apps.get(&server_id) {
            return (Rc::clone(app), false);
        }
        let app = HttpApp::new((config.factory)()).with_max_body_bytes(config.max_body_bytes);
        let app = match &config.openapi {
            Some(openapi) => app.with_openapi(openapi.clone()),
            None => app,
        };
        let app = match &config.middleware {
            Some(middleware) => middleware()
                .into_iter()
                .fold(app, HttpApp::with_shared_middleware),
            None => app,
        };
        let app = Rc::new(app);
        apps.insert(server_id, Rc::clone(&app));
        (app, true)
    })
}

fn take_worker_app(server_id: u64) -> Option<Rc<HttpApp>> {
    WORKER_APPS.with(|apps| apps.borrow_mut().remove(&server_id))
}

fn install_drain_counter(active: &Arc<AtomicUsize>) {
    DRAIN_COUNTER.with(|counter| *counter.borrow_mut() = Some(Arc::clone(active)));
}

fn clear_drain_counter() {
    DRAIN_COUNTER.with(|counter| counter.borrow_mut().take());
}

fn drain_counter() -> Option<Arc<AtomicUsize>> {
    DRAIN_COUNTER.with(|counter| counter.borrow().clone())
}

/// Publishes the drain counter that background tasks spawned on this thread
/// join, so graceful shutdown waits for them like it waits for connections.
struct DrainScope;

impl DrainScope {
    fn new(active: &Arc<AtomicUsize>) -> Self {
        install_drain_counter(active);
        Self
    }
}

impl Drop for DrainScope {
    fn drop(&mut self) {
        clear_drain_counter();
    }
}

/// One unit of in-flight work counted by the graceful-drain accounting.
struct ActiveWork {
    active: Arc<AtomicUsize>,
}

impl ActiveWork {
    /// Counts one new unit of work.
    fn acquire(active: Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::AcqRel);
        Self { active }
    }

    /// Takes ownership of a unit the accept loop already counted.
    const fn adopt(active: Arc<AtomicUsize>) -> Self {
        Self { active }
    }
}

impl Drop for ActiveWork {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

enum AcceptEvent {
    Connection(io::Result<(compio::net::TcpStream, SocketAddr)>),
    Shutdown,
}

/// Transport settings carried from an accept loop into one connection task.
struct ConnectionSetup {
    limits: ServerLimits,
    request_timeout: Option<Duration>,
    #[cfg(feature = "tls")]
    tls_acceptor: Option<compio::tls::TlsAcceptor>,
}

/// Serves one accepted socket, selecting TLS or the plaintext native path.
///
/// The plaintext path stays native even when the `http2` feature is enabled;
/// it sniffs the HTTP/2 connection preface itself and only hands the socket to
/// the HTTP/2 codec when the peer actually speaks HTTP/2.
async fn serve_accepted(
    app: &HttpApp,
    stream: compio::net::TcpStream,
    peer_addr: SocketAddr,
    shutdown: Option<&ShutdownState>,
    setup: ConnectionSetup,
) {
    let limits = setup.limits;
    let request_timeout = setup.request_timeout;
    #[cfg(feature = "tls")]
    if let Some(tls_acceptor) = setup.tls_acceptor {
        match within(limits.header_read_timeout, tls_acceptor.accept(stream)).await {
            Ok(Ok(stream)) => {
                let _ = serve_compat_connection(
                    app,
                    limits,
                    Box::pin(AsyncStream::new(stream)),
                    Some(peer_addr),
                    "https",
                    shutdown,
                    request_timeout,
                )
                .await;
            }
            Ok(Err(error)) => report_failure("TLS handshake failed", &error),
            Err(Expired) => report_failure("TLS handshake", &"deadline expired"),
        }
        return;
    }
    let _ = serve_native_connection(
        app,
        limits,
        stream,
        Some(peer_addr),
        "http",
        shutdown,
        request_timeout,
    )
    .await;
}

/// Whether the connection loop may hand its transport to an upgraded protocol.
#[derive(Clone, Copy, Eq, PartialEq)]
enum TransportOwnership {
    /// The caller owns the transport and can move it into an upgrade session.
    Owned,
    /// The transport is only lent for the duration of the connection loop.
    Borrowed,
}

/// How one generic-transport connection ended.
enum ConnectionOutcome {
    /// The connection is finished and the transport may be dropped.
    Completed,
    /// The peer switched protocols after the `101` response was written.
    ///
    /// The caller still owns the transport and must run the session with the
    /// bytes that already arrived after the upgrade request.
    Upgraded {
        upgrade: HttpUpgrade,
        buffered: Vec<u8>,
    },
}

/// Serves one owned futures-I/O transport, including protocol upgrades.
///
/// TLS connections take this path. Ownership of the transport stays here, so
/// an upgrade moves it into [`HttpUpgrade::run`] exactly like the plaintext
/// socket path does instead of being refused as an unsupported transport.
async fn serve_compat_connection<IO>(
    app: &HttpApp,
    limits: ServerLimits,
    mut io: IO,
    peer_addr: Option<SocketAddr>,
    scheme: &'static str,
    shutdown: Option<&ShutdownState>,
    request_timeout: Option<Duration>,
) -> io::Result<()>
where
    IO: AsyncRead + AsyncWrite + Unpin + 'static,
{
    let outcome = serve_connection(
        app,
        limits,
        &mut io,
        peer_addr,
        scheme,
        shutdown,
        request_timeout,
        TransportOwnership::Owned,
    )
    .await?;
    match outcome {
        ConnectionOutcome::Completed => Ok(()),
        ConnectionOutcome::Upgraded { upgrade, buffered } => upgrade
            .run(Box::new(CompatUpgradedIo { io, buffered }))
            .await
            .map_err(|error| io::Error::other(error.to_string())),
    }
}

/// Marker for an expired socket deadline.
struct Expired;

/// Bounds one socket operation by a deadline when a Compio timer exists.
///
/// The Compio runtime owns the only timer source. In-memory transports used by
/// protocol tests run outside a runtime, so those connections stay unbounded
/// instead of panicking on a missing runtime.
async fn within<Operation>(
    deadline: Duration,
    operation: Operation,
) -> Result<Operation::Output, Expired>
where
    Operation: Future,
{
    if compio::runtime::Runtime::try_with_current(|_| ()).is_err() {
        return Ok(operation.await);
    }
    compio::time::timeout(deadline, operation)
        .await
        .map_err(|_| Expired)
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn deadline_error(operation: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("{operation} deadline expired"),
    )
}

/// Reports a connection-scoped failure that has no request to attach it to.
///
/// The adapter carries no logging dependency, so failures that would otherwise
/// be discarded are written to the process error stream.
fn report_failure(context: &str, detail: &dyn fmt::Display) {
    eprintln!("blazingly-native: {context}: {detail}");
}

fn resolve_address(address: impl ToSocketAddrs) -> io::Result<SocketAddr> {
    address.to_socket_addrs()?.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "address resolved to no sockets",
        )
    })
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn serve_connection<IO>(
    app: &HttpApp,
    limits: ServerLimits,
    io: &mut IO,
    peer_addr: Option<SocketAddr>,
    scheme: &'static str,
    shutdown: Option<&ShutdownState>,
    request_timeout: Option<Duration>,
    ownership: TransportOwnership,
) -> io::Result<ConnectionOutcome>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    ensure_date_updater();
    let mut buffer = Vec::with_capacity(READ_CHUNK_BYTES);
    let mut read_chunk = vec![0_u8; READ_CHUNK_BYTES];
    let mut wire_response = Vec::with_capacity(READ_CHUNK_BYTES);
    let mut completed_requests = 0_usize;

    loop {
        let mut head_deadline = None;
        let parsed = loop {
            #[cfg(feature = "http2")]
            {
                if buffer.starts_with(shiguredo_http2::CONNECTION_PREFACE) {
                    Box::pin(http2::serve_connection(
                        app,
                        limits,
                        io,
                        peer_addr,
                        scheme,
                        shutdown,
                        std::mem::take(&mut buffer),
                        request_timeout,
                    ))
                    .await?;
                    return Ok(ConnectionOutcome::Completed);
                }
            }

            if !is_partial_http2_preface(&buffer) {
                match parse_head(&buffer, limits) {
                    Ok(Some(parsed)) => break parsed,
                    Ok(None) if buffer.len() >= limits.max_header_bytes => {
                        write_rejection(
                            io,
                            &mut wire_response,
                            431,
                            "request_header_too_large",
                            "request headers exceed the configured limit",
                            limits.write_timeout,
                        )
                        .await?;
                        return Ok(ConnectionOutcome::Completed);
                    }
                    Ok(None) => {}
                    Err(rejection) => {
                        write_rejection(
                            io,
                            &mut wire_response,
                            rejection.status,
                            rejection.code,
                            rejection.message,
                            limits.write_timeout,
                        )
                        .await?;
                        return Ok(ConnectionOutcome::Completed);
                    }
                }
            }

            let receiving_head = !buffer.is_empty();
            let wait = if receiving_head {
                remaining(
                    *head_deadline
                        .get_or_insert_with(|| Instant::now() + limits.header_read_timeout),
                )
            } else {
                limits.idle_timeout
            };
            let Ok(read) = within(wait, io.read(&mut read_chunk)).await else {
                if receiving_head {
                    let _ = write_rejection(
                        io,
                        &mut wire_response,
                        408,
                        "request_timeout",
                        "the request head did not arrive within the configured deadline",
                        limits.write_timeout,
                    )
                    .await;
                }
                return Ok(ConnectionOutcome::Completed);
            };
            let read = read?;
            if read == 0 {
                return Ok(ConnectionOutcome::Completed);
            }
            buffer.extend_from_slice(&read_chunk[..read]);
        };

        let mut decoded_chunked = None;
        let request_bytes = match parsed.body {
            BodyFraming::ContentLength(content_length) => {
                if content_length > limits.max_body_bytes {
                    write_rejection(
                        io,
                        &mut wire_response,
                        413,
                        "payload_too_large",
                        "request body exceeds the configured limit",
                        limits.write_timeout,
                    )
                    .await?;
                    return Ok(ConnectionOutcome::Completed);
                }
                let request_bytes =
                    parsed
                        .head_bytes
                        .checked_add(content_length)
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidData, "request size overflow")
                        })?;
                while buffer.len() < request_bytes {
                    let Ok(read) = within(limits.body_read_timeout, io.read(&mut read_chunk)).await
                    else {
                        return Err(deadline_error("request body read"));
                    };
                    let read = read?;
                    if read == 0 {
                        write_rejection(
                            io,
                            &mut wire_response,
                            400,
                            "incomplete_body",
                            "request body ended before Content-Length bytes arrived",
                            limits.write_timeout,
                        )
                        .await?;
                        return Ok(ConnectionOutcome::Completed);
                    }
                    buffer.extend_from_slice(&read_chunk[..read]);
                }
                request_bytes
            }
            BodyFraming::Chunked => {
                let mut decoder = ChunkDecoder::new(parsed.head_bytes, wire_limits(limits));
                loop {
                    match decoder.advance(&buffer) {
                        Ok(Some(decoded_body)) => {
                            let consumed = decoded_body.consumed;
                            decoded_chunked = Some(decoded_body.body);
                            break consumed;
                        }
                        Ok(None) => {}
                        Err(rejection) => {
                            write_rejection(
                                io,
                                &mut wire_response,
                                rejection.status,
                                rejection.code,
                                rejection.message,
                                limits.write_timeout,
                            )
                            .await?;
                            return Ok(ConnectionOutcome::Completed);
                        }
                    }
                    let Ok(read) = within(limits.body_read_timeout, io.read(&mut read_chunk)).await
                    else {
                        return Err(deadline_error("request body read"));
                    };
                    let read = read?;
                    if read == 0 {
                        write_rejection(
                            io,
                            &mut wire_response,
                            400,
                            "incomplete_body",
                            "chunked request body ended before its final chunk",
                            limits.write_timeout,
                        )
                        .await?;
                        return Ok(ConnectionOutcome::Completed);
                    }
                    buffer.extend_from_slice(&read_chunk[..read]);
                }
            }
        };

        let body = decoded_chunked
            .as_deref()
            .unwrap_or(&buffer[parsed.head_bytes..request_bytes]);
        let target = std::str::from_utf8(&buffer[parsed.target.start..parsed.target.end])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let native_request = NativeRequest {
            method: framework_method(parsed.method),
            target,
            buffer: &buffer,
            headers: &parsed.headers,
            body,
            peer_addr,
            scheme,
        };
        let mut response = if let Some(timeout) = request_timeout {
            app.call_view_controlled(
                &native_request,
                InvocationControl::new().with_timeout(compio::time::sleep(timeout)),
            )
            .await
        } else {
            app.call_view(&native_request).await
        };
        completed_requests += 1;
        let request_limit_reached = limits
            .max_requests_per_connection
            .is_some_and(|limit| completed_requests >= limit.get());
        let keep_alive = parsed.keep_alive
            && !request_limit_reached
            && !shutdown.is_some_and(|shutdown| shutdown.requested.load(Ordering::Acquire));
        let send_body = parsed.method != blazingly_wire::Method::Head
            && !matches!(response.status(), 204 | 304)
            && !(parsed.method == blazingly_wire::Method::Connect
                && (200..300).contains(&response.status()));
        let send_content_length = response.status() != 204
            && !(parsed.method == blazingly_wire::Method::Connect
                && (200..300).contains(&response.status()));
        if let Some(upgrade) = response.take_upgrade() {
            if ownership == TransportOwnership::Borrowed {
                write_rejection(
                    io,
                    &mut wire_response,
                    501,
                    "upgrade_transport_unsupported",
                    "this borrowed transport cannot transfer ownership for an upgrade",
                    limits.write_timeout,
                )
                .await?;
                return Ok(ConnectionOutcome::Completed);
            }
            wire_response.clear();
            with_cached_date(|date| {
                blazingly_wire::encode_upgrade_response(
                    &mut wire_response,
                    response.headers(),
                    date,
                )
            })?;
            write_all_within(io, &wire_response, limits.write_timeout).await?;
            flush_within(io, limits.write_timeout).await?;
            schedule_background(response.take_background_tasks());
            let buffered = buffer
                .get(request_bytes..)
                .map_or_else(Vec::new, <[u8]>::to_vec);
            return Ok(ConnectionOutcome::Upgraded { upgrade, buffered });
        }
        write_response(
            io,
            &mut wire_response,
            &mut response,
            keep_alive,
            send_body,
            send_content_length,
            limits.write_timeout,
        )
        .await?;
        schedule_background(response.take_background_tasks());
        consume_prefix(&mut buffer, request_bytes);

        if !keep_alive {
            return Ok(ConnectionOutcome::Completed);
        }
    }
}

/// Upgraded byte I/O over a generic futures transport.
///
/// The bytes buffered behind the upgrade request are replayed before the
/// transport is read again, so a client that coalesced its first protocol
/// frame with the handshake does not lose it.
struct CompatUpgradedIo<IO> {
    io: IO,
    buffered: Vec<u8>,
}

impl<IO> UpgradedIo for CompatUpgradedIo<IO>
where
    IO: AsyncRead + AsyncWrite + Unpin + 'static,
{
    fn read(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, UpgradeIoError>> + '_>> {
        Box::pin(async move {
            if !self.buffered.is_empty() {
                return Ok(Some(std::mem::take(&mut self.buffered)));
            }
            let mut chunk = vec![0_u8; READ_CHUNK_BYTES];
            let read = self
                .io
                .read(&mut chunk)
                .await
                .map_err(|error| upgrade_io_error("upgrade_read_failed", &error))?;
            if read == 0 {
                return Ok(None);
            }
            chunk.truncate(read);
            Ok(Some(chunk))
        })
    }

    fn write(
        &mut self,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<(), UpgradeIoError>> + '_>> {
        Box::pin(async move {
            self.io
                .write_all(&bytes)
                .await
                .map_err(|error| upgrade_io_error("upgrade_write_failed", &error))?;
            // The compatibility transport buffers writes, so a frame that is
            // not flushed would never reach a TLS record.
            self.io
                .flush()
                .await
                .map_err(|error| upgrade_io_error("upgrade_write_failed", &error))
        })
    }

    fn shutdown(&mut self) -> Pin<Box<dyn Future<Output = Result<(), UpgradeIoError>> + '_>> {
        Box::pin(async move {
            self.io
                .close()
                .await
                .map_err(|error| upgrade_io_error("upgrade_shutdown_failed", &error))
        })
    }
}

/// Plaintext Compio fast path.
///
/// `compio::io::compat::AsyncStream` is required for generic TLS and HTTP/2
/// adapters, but it owns a second read/write buffer and boxes each in-flight
/// compatibility operation. Plain HTTP/1 can pass owned buffers directly to
/// Compio, avoiding those copies and per-request I/O allocations.
enum StreamingRequestError {
    Io(io::Error),
    Protocol(Rejection),
    Incomplete(&'static str),
}

impl From<io::Error> for StreamingRequestError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<Rejection> for StreamingRequestError {
    fn from(error: Rejection) -> Self {
        Self::Protocol(error)
    }
}

async fn send_streaming_chunk(
    sender: &IncomingBodySender,
    chunk: Vec<u8>,
    mut response_future: Pin<&mut dyn Future<Output = Response>>,
    response: &mut Option<Response>,
) -> bool {
    enum Activity {
        Sent(bool),
        Response(Box<Response>),
    }

    if response.is_some() {
        return false;
    }
    let mut delivery = Box::pin(sender.send(chunk));
    let activity = std::future::poll_fn(|context| {
        if let Poll::Ready(response) = response_future.as_mut().poll(context) {
            return Poll::Ready(Activity::Response(Box::new(response)));
        }
        delivery.as_mut().poll(context).map(Activity::Sent)
    })
    .await;
    match activity {
        Activity::Sent(sent) => sent,
        Activity::Response(completed) => {
            *response = Some(*completed);
            false
        }
    }
}

async fn native_stream_read(
    io: &mut TcpStream,
    mut response_future: Pin<&mut dyn Future<Output = Response>>,
    response: &mut Option<Response>,
) -> io::Result<Vec<u8>> {
    let mut read = Box::pin(async {
        let result = CompioAsyncReadExt::append(io, Vec::with_capacity(READ_CHUNK_BYTES)).await;
        result.0.map(|_| result.1)
    });
    if response.is_none() {
        enum Activity {
            Read(io::Result<Vec<u8>>),
            Response(Box<Response>),
        }
        let activity = std::future::poll_fn(|context| {
            if let Poll::Ready(response) = response_future.as_mut().poll(context) {
                return Poll::Ready(Activity::Response(Box::new(response)));
            }
            read.as_mut().poll(context).map(Activity::Read)
        })
        .await;
        match activity {
            Activity::Read(result) => return result,
            Activity::Response(completed) => *response = Some(*completed),
        }
    }
    read.await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn dispatch_streaming_native(
    app: &HttpApp,
    limits: ServerLimits,
    io: &mut TcpStream,
    buffer: &mut Vec<u8>,
    parsed: &ParsedHead,
    peer_addr: Option<SocketAddr>,
    scheme: &'static str,
    request_timeout: Option<Duration>,
) -> Result<Response, StreamingRequestError> {
    let exact_length = match parsed.body {
        BodyFraming::ContentLength(length) => {
            if length > limits.max_body_bytes {
                return Err(StreamingRequestError::Protocol(Rejection {
                    status: 413,
                    code: "payload_too_large",
                    message: "request body exceeds the configured limit",
                }));
            }
            Some(u64::try_from(length).unwrap_or(u64::MAX))
        }
        BodyFraming::Chunked => None,
    };
    let (sender, body) = incoming_body_channel(exact_length);
    let request = owned_request(parsed, buffer, body, peer_addr, scheme)?;
    consume_prefix(buffer, parsed.head_bytes);
    let mut response_future: Pin<Box<dyn Future<Output = Response>>> =
        if let Some(timeout) = request_timeout {
            Box::pin(app.call_view_controlled(
                &request,
                InvocationControl::new().with_timeout(compio::time::sleep(timeout)),
            ))
        } else {
            Box::pin(app.call_view(&request))
        };
    let mut response = None;
    let mut receiver_open = true;

    match parsed.body {
        BodyFraming::ContentLength(length) => {
            let mut remaining = length;
            while remaining > 0 {
                if buffer.is_empty() {
                    let Ok(chunk) = within(
                        limits.body_read_timeout,
                        native_stream_read(io, response_future.as_mut(), &mut response),
                    )
                    .await
                    else {
                        sender.fail(BodyStreamError::new(
                            "upload_timeout",
                            "request body stalled past the configured deadline",
                        ));
                        return Err(StreamingRequestError::Io(deadline_error(
                            "request body read",
                        )));
                    };
                    let chunk = chunk?;
                    if chunk.is_empty() {
                        sender.fail(BodyStreamError::new(
                            "incomplete_upload",
                            "request body ended before Content-Length bytes arrived",
                        ));
                        return Err(StreamingRequestError::Incomplete(
                            "request body ended before Content-Length bytes arrived",
                        ));
                    }
                    buffer.extend_from_slice(&chunk);
                }
                let available = remaining.min(buffer.len()).min(READ_CHUNK_BYTES);
                let chunk = buffer.drain(..available).collect::<Vec<_>>();
                remaining -= available;
                if receiver_open {
                    receiver_open = send_streaming_chunk(
                        &sender,
                        chunk,
                        response_future.as_mut(),
                        &mut response,
                    )
                    .await;
                }
            }
        }
        BodyFraming::Chunked => {
            let mut decoder = StreamingChunkDecoder::new(0, wire_limits(limits));
            loop {
                match decoder.advance(buffer)? {
                    StreamingChunk::Data(range) => {
                        let chunk = range
                            .bytes(buffer)
                            .expect("decoder ranges stay inside the receive buffer")
                            .to_vec();
                        let consumed = decoder.consumed_prefix();
                        consume_prefix(buffer, consumed);
                        decoder.discard_prefix(consumed);
                        if receiver_open {
                            receiver_open = send_streaming_chunk(
                                &sender,
                                chunk,
                                response_future.as_mut(),
                                &mut response,
                            )
                            .await;
                        }
                    }
                    StreamingChunk::Complete { consumed } => {
                        consume_prefix(buffer, consumed);
                        break;
                    }
                    StreamingChunk::NeedMore => {
                        let Ok(chunk) = within(
                            limits.body_read_timeout,
                            native_stream_read(io, response_future.as_mut(), &mut response),
                        )
                        .await
                        else {
                            sender.fail(BodyStreamError::new(
                                "upload_timeout",
                                "request body stalled past the configured deadline",
                            ));
                            return Err(StreamingRequestError::Io(deadline_error(
                                "request body read",
                            )));
                        };
                        let chunk = chunk?;
                        if chunk.is_empty() {
                            sender.fail(BodyStreamError::new(
                                "incomplete_upload",
                                "chunked request body ended before its final chunk",
                            ));
                            return Err(StreamingRequestError::Incomplete(
                                "chunked request body ended before its final chunk",
                            ));
                        }
                        buffer.extend_from_slice(&chunk);
                    }
                }
            }
        }
    }
    sender.close();
    Ok(match response {
        Some(response) => response,
        None => response_future.await,
    })
}

#[allow(clippy::too_many_lines)]
async fn serve_native_connection(
    app: &HttpApp,
    limits: ServerLimits,
    mut io: TcpStream,
    peer_addr: Option<SocketAddr>,
    scheme: &'static str,
    shutdown: Option<&ShutdownState>,
    request_timeout: Option<Duration>,
) -> io::Result<()> {
    ensure_date_updater();
    let mut buffer = Vec::with_capacity(READ_CHUNK_BYTES);
    let mut wire_response = Vec::with_capacity(READ_CHUNK_BYTES);
    let mut completed_requests = 0_usize;
    let mut buffered_responses = 0_usize;

    loop {
        let mut head_deadline = None;
        let parsed = loop {
            // The HTTP/2 preface can only start a connection. Sniffing it here
            // keeps HTTP/1 peers on the native socket path instead of routing
            // every plaintext connection through the compatibility transport.
            let connection_start = completed_requests == 0;
            #[cfg(feature = "http2")]
            if connection_start && buffer.starts_with(shiguredo_http2::CONNECTION_PREFACE) {
                let initial = std::mem::take(&mut buffer);
                let mut stream = Box::pin(AsyncStream::new(io));
                return Box::pin(http2::serve_connection(
                    app,
                    limits,
                    &mut stream,
                    peer_addr,
                    scheme,
                    shutdown,
                    initial,
                    request_timeout,
                ))
                .await;
            }

            if !connection_start || !is_partial_http2_preface(&buffer) {
                match parse_head(&buffer, limits) {
                    Ok(Some(parsed)) => break parsed,
                    Ok(None) if buffer.len() >= limits.max_header_bytes => {
                        write_rejection_native(
                            &mut io,
                            &mut wire_response,
                            431,
                            "request_header_too_large",
                            "request headers exceed the configured limit",
                            limits.write_timeout,
                        )
                        .await?;
                        return Ok(());
                    }
                    Ok(None) => {}
                    Err(rejection) => {
                        write_rejection_native(
                            &mut io,
                            &mut wire_response,
                            rejection.status,
                            rejection.code,
                            rejection.message,
                            limits.write_timeout,
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }

            flush_native_pending(&mut io, &mut wire_response, limits.write_timeout).await?;
            buffered_responses = 0;
            let receiving_head = !buffer.is_empty();
            let wait = if receiving_head {
                remaining(
                    *head_deadline
                        .get_or_insert_with(|| Instant::now() + limits.header_read_timeout),
                )
            } else {
                limits.idle_timeout
            };
            let Ok(read) = within(wait, native_read_more(&mut io, &mut buffer)).await else {
                if receiving_head {
                    let _ = write_rejection_native(
                        &mut io,
                        &mut wire_response,
                        408,
                        "request_timeout",
                        "the request head did not arrive within the configured deadline",
                        limits.write_timeout,
                    )
                    .await;
                }
                return Ok(());
            };
            if read? == 0 {
                return Ok(());
            }
        };

        let request_method = framework_method(parsed.method);
        let request_target = parsed
            .target
            .text(&buffer)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid request target"))?;
        if app.request_body_source(request_method, request_target)
            == Some(blazingly_core::InputSource::Stream)
        {
            flush_native_pending(&mut io, &mut wire_response, limits.write_timeout).await?;
            let mut response = match dispatch_streaming_native(
                app,
                limits,
                &mut io,
                &mut buffer,
                &parsed,
                peer_addr,
                scheme,
                request_timeout,
            )
            .await
            {
                Ok(response) => response,
                Err(StreamingRequestError::Io(error)) => return Err(error),
                Err(StreamingRequestError::Protocol(rejection)) => {
                    write_rejection_native(
                        &mut io,
                        &mut wire_response,
                        rejection.status,
                        rejection.code,
                        rejection.message,
                        limits.write_timeout,
                    )
                    .await?;
                    return Ok(());
                }
                Err(StreamingRequestError::Incomplete(message)) => {
                    write_rejection_native(
                        &mut io,
                        &mut wire_response,
                        400,
                        "incomplete_body",
                        message,
                        limits.write_timeout,
                    )
                    .await?;
                    return Ok(());
                }
            };
            if let Some(upgrade) = response.take_upgrade() {
                with_cached_date(|date| {
                    blazingly_wire::encode_upgrade_response(
                        &mut wire_response,
                        response.headers(),
                        date,
                    )
                })?;
                native_write_all(&mut io, &mut wire_response, limits.write_timeout).await?;
                schedule_background(response.take_background_tasks());
                return upgrade
                    .run(Box::new(NativeUpgradedIo {
                        io,
                        buffered: buffer,
                    }))
                    .await
                    .map_err(|error| io::Error::other(error.to_string()));
            }
            completed_requests += 1;
            let request_limit_reached = limits
                .max_requests_per_connection
                .is_some_and(|limit| completed_requests >= limit.get());
            let keep_alive = parsed.keep_alive
                && !request_limit_reached
                && !shutdown.is_some_and(|shutdown| shutdown.requested.load(Ordering::Acquire));
            let send_body = parsed.method != blazingly_wire::Method::Head
                && !matches!(response.status(), 204 | 304)
                && !(parsed.method == blazingly_wire::Method::Connect
                    && (200..300).contains(&response.status()));
            let send_content_length = response.status() != 204
                && !(parsed.method == blazingly_wire::Method::Connect
                    && (200..300).contains(&response.status()));
            let streaming_response = response.is_streaming();
            write_response_native(
                &mut io,
                &mut wire_response,
                &mut response,
                keep_alive,
                send_body,
                send_content_length,
                limits.write_timeout,
            )
            .await?;
            schedule_background(response.take_background_tasks());
            if streaming_response {
                buffered_responses = 0;
            } else {
                buffered_responses = 1;
            }
            if !keep_alive {
                flush_native_pending(&mut io, &mut wire_response, limits.write_timeout).await?;
                return Ok(());
            }
            continue;
        }

        let mut decoded_chunked = None;
        let request_bytes = match parsed.body {
            BodyFraming::ContentLength(content_length) => {
                if content_length > limits.max_body_bytes {
                    write_rejection_native(
                        &mut io,
                        &mut wire_response,
                        413,
                        "payload_too_large",
                        "request body exceeds the configured limit",
                        limits.write_timeout,
                    )
                    .await?;
                    return Ok(());
                }
                let request_bytes =
                    parsed
                        .head_bytes
                        .checked_add(content_length)
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidData, "request size overflow")
                        })?;
                while buffer.len() < request_bytes {
                    flush_native_pending(&mut io, &mut wire_response, limits.write_timeout).await?;
                    buffered_responses = 0;
                    let Ok(read) = within(
                        limits.body_read_timeout,
                        native_read_more(&mut io, &mut buffer),
                    )
                    .await
                    else {
                        return Err(deadline_error("request body read"));
                    };
                    if read? == 0 {
                        write_rejection_native(
                            &mut io,
                            &mut wire_response,
                            400,
                            "incomplete_body",
                            "request body ended before Content-Length bytes arrived",
                            limits.write_timeout,
                        )
                        .await?;
                        return Ok(());
                    }
                }
                request_bytes
            }
            BodyFraming::Chunked => {
                let mut decoder = ChunkDecoder::new(parsed.head_bytes, wire_limits(limits));
                loop {
                    match decoder.advance(&buffer) {
                        Ok(Some(decoded_body)) => {
                            let consumed = decoded_body.consumed;
                            decoded_chunked = Some(decoded_body.body);
                            break consumed;
                        }
                        Ok(None) => {}
                        Err(rejection) => {
                            write_rejection_native(
                                &mut io,
                                &mut wire_response,
                                rejection.status,
                                rejection.code,
                                rejection.message,
                                limits.write_timeout,
                            )
                            .await?;
                            return Ok(());
                        }
                    }
                    flush_native_pending(&mut io, &mut wire_response, limits.write_timeout).await?;
                    buffered_responses = 0;
                    let Ok(read) = within(
                        limits.body_read_timeout,
                        native_read_more(&mut io, &mut buffer),
                    )
                    .await
                    else {
                        return Err(deadline_error("request body read"));
                    };
                    if read? == 0 {
                        write_rejection_native(
                            &mut io,
                            &mut wire_response,
                            400,
                            "incomplete_body",
                            "chunked request body ended before its final chunk",
                            limits.write_timeout,
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }
        };

        let body = decoded_chunked
            .as_deref()
            .unwrap_or(&buffer[parsed.head_bytes..request_bytes]);
        let target = std::str::from_utf8(&buffer[parsed.target.start..parsed.target.end])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let native_request = NativeRequest {
            method: framework_method(parsed.method),
            target,
            buffer: &buffer,
            headers: &parsed.headers,
            body,
            peer_addr,
            scheme,
        };
        let mut response = if let Some(timeout) = request_timeout {
            app.call_view_controlled(
                &native_request,
                InvocationControl::new().with_timeout(compio::time::sleep(timeout)),
            )
            .await
        } else {
            app.call_view(&native_request).await
        };
        if let Some(upgrade) = response.take_upgrade() {
            with_cached_date(|date| {
                blazingly_wire::encode_upgrade_response(
                    &mut wire_response,
                    response.headers(),
                    date,
                )
            })?;
            native_write_all(&mut io, &mut wire_response, limits.write_timeout).await?;
            schedule_background(response.take_background_tasks());
            let buffered = buffer
                .get(request_bytes..)
                .map_or_else(Vec::new, <[u8]>::to_vec);
            return upgrade
                .run(Box::new(NativeUpgradedIo { io, buffered }))
                .await
                .map_err(|error| io::Error::other(error.to_string()));
        }
        completed_requests += 1;
        let request_limit_reached = limits
            .max_requests_per_connection
            .is_some_and(|limit| completed_requests >= limit.get());
        let keep_alive = parsed.keep_alive
            && !request_limit_reached
            && !shutdown.is_some_and(|shutdown| shutdown.requested.load(Ordering::Acquire));
        let send_body = parsed.method != blazingly_wire::Method::Head
            && !matches!(response.status(), 204 | 304)
            && !(parsed.method == blazingly_wire::Method::Connect
                && (200..300).contains(&response.status()));
        let send_content_length = response.status() != 204
            && !(parsed.method == blazingly_wire::Method::Connect
                && (200..300).contains(&response.status()));
        let streaming = response.is_streaming();
        write_response_native(
            &mut io,
            &mut wire_response,
            &mut response,
            keep_alive,
            send_body,
            send_content_length,
            limits.write_timeout,
        )
        .await?;
        schedule_background(response.take_background_tasks());
        if streaming {
            buffered_responses = 0;
        } else {
            buffered_responses += 1;
            if buffered_responses >= limits.max_pipeline_batch
                || wire_response.len() >= MAX_PIPELINE_WRITE_BYTES
            {
                flush_native_pending(&mut io, &mut wire_response, limits.write_timeout).await?;
                buffered_responses = 0;
            }
        }
        consume_prefix(&mut buffer, request_bytes);

        if !keep_alive {
            flush_native_pending(&mut io, &mut wire_response, limits.write_timeout).await?;
            return Ok(());
        }
    }
}

async fn native_read_more(io: &mut TcpStream, buffer: &mut Vec<u8>) -> io::Result<usize> {
    let result = CompioAsyncReadExt::append(io, std::mem::take(buffer)).await;
    *buffer = result.1;
    result.0
}

struct NativeUpgradedIo {
    io: TcpStream,
    buffered: Vec<u8>,
}

impl UpgradedIo for NativeUpgradedIo {
    fn read(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, UpgradeIoError>> + '_>> {
        Box::pin(async move {
            if !self.buffered.is_empty() {
                return Ok(Some(std::mem::take(&mut self.buffered)));
            }
            let result =
                CompioAsyncReadExt::append(&mut self.io, Vec::with_capacity(READ_CHUNK_BYTES))
                    .await;
            let read = result
                .0
                .map_err(|error| upgrade_io_error("upgrade_read_failed", &error))?;
            if read == 0 {
                return Ok(None);
            }
            Ok(Some(result.1))
        })
    }

    fn write(
        &mut self,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<(), UpgradeIoError>> + '_>> {
        Box::pin(async move {
            CompioAsyncWriteExt::write_all(&mut self.io, bytes)
                .await
                .0
                .map_err(|error| upgrade_io_error("upgrade_write_failed", &error))
        })
    }

    fn shutdown(&mut self) -> Pin<Box<dyn Future<Output = Result<(), UpgradeIoError>> + '_>> {
        Box::pin(async { Ok(()) })
    }
}

fn upgrade_io_error(code: &'static str, error: &io::Error) -> UpgradeIoError {
    UpgradeIoError::new(code, error.to_string())
}

fn schedule_background(tasks: Vec<BackgroundTask>) {
    if tasks.is_empty() {
        return;
    }
    let accounting = drain_counter().map(ActiveWork::acquire);
    spawn(async move {
        for task in tasks {
            if let Err(error) = task.run().await {
                report_failure("background task failed", &error);
            }
        }
        drop(accounting);
    })
    .detach();
}

#[cfg(feature = "http2")]
fn is_partial_http2_preface(buffer: &[u8]) -> bool {
    !buffer.is_empty()
        && buffer.len() < shiguredo_http2::CONNECTION_PREFACE.len()
        && shiguredo_http2::CONNECTION_PREFACE.starts_with(buffer)
}

#[cfg(not(feature = "http2"))]
const fn is_partial_http2_preface(_buffer: &[u8]) -> bool {
    false
}

type ParsedHead = blazingly_wire::RequestHead;
type Rejection = blazingly_wire::ParseError;

fn parse_head(buffer: &[u8], limits: ServerLimits) -> Result<Option<ParsedHead>, Rejection> {
    blazingly_wire::parse_request_head(buffer, wire_limits(limits))
}

const fn wire_limits(limits: ServerLimits) -> blazingly_wire::Limits {
    blazingly_wire::Limits::new()
        .with_max_header_bytes(limits.max_header_bytes)
        .with_max_headers(limits.max_headers)
        .with_max_body_bytes(limits.max_body_bytes)
        .with_max_chunks(limits.max_chunks)
}

const fn framework_method(method: blazingly_wire::Method) -> HttpMethod {
    match method {
        blazingly_wire::Method::Get => HttpMethod::Get,
        blazingly_wire::Method::Head => HttpMethod::Head,
        blazingly_wire::Method::Post => HttpMethod::Post,
        blazingly_wire::Method::Put => HttpMethod::Put,
        blazingly_wire::Method::Patch => HttpMethod::Patch,
        blazingly_wire::Method::Delete => HttpMethod::Delete,
        blazingly_wire::Method::Options => HttpMethod::Options,
        blazingly_wire::Method::Trace => HttpMethod::Trace,
        blazingly_wire::Method::Connect => HttpMethod::Connect,
    }
}

#[cfg(feature = "http2")]
fn parse_method(method: &str) -> Result<HttpMethod, Rejection> {
    blazingly_wire::Method::parse(method).map(framework_method)
}

struct IncomingBodyState {
    chunks: VecDeque<Result<Vec<u8>, BodyStreamError>>,
    queued_bytes: usize,
    /// Bytes the application pulled since the producer last observed progress.
    /// Only HTTP/2 needs it, to release receive-window credit on demand.
    #[cfg(feature = "http2")]
    consumed_bytes: usize,
    max_queued_bytes: usize,
    closed: bool,
    receiver_open: bool,
    consumer_waker: Option<Waker>,
    producer_waker: Option<Waker>,
}

#[derive(Clone)]
struct IncomingBodySender {
    state: Rc<RefCell<IncomingBodyState>>,
}

impl IncomingBodySender {
    async fn send(&self, bytes: Vec<u8>) -> bool {
        let mut bytes = Some(bytes);
        std::future::poll_fn(|context| {
            let mut state = self.state.borrow_mut();
            if !state.receiver_open {
                return Poll::Ready(false);
            }
            let length = bytes.as_ref().map_or(0, Vec::len);
            if state.chunks.is_empty()
                || state.queued_bytes.saturating_add(length) <= state.max_queued_bytes
            {
                state.queued_bytes = state.queued_bytes.saturating_add(length);
                state
                    .chunks
                    .push_back(Ok(bytes.take().expect("body chunk is sent once")));
                if let Some(waker) = state.consumer_waker.take() {
                    waker.wake();
                }
                return Poll::Ready(true);
            }
            state.producer_waker = Some(context.waker().clone());
            Poll::Pending
        })
        .await
    }

    /// Queues one chunk without waiting for the application to catch up.
    ///
    /// HTTP/2 bounds the producer with its own receive window instead of the
    /// queue limit, so this never applies backpressure of its own. Returns
    /// whether the application still holds the body.
    #[cfg(feature = "http2")]
    fn push(&self, bytes: Vec<u8>) -> bool {
        let mut state = self.state.borrow_mut();
        if !state.receiver_open {
            return false;
        }
        state.queued_bytes = state.queued_bytes.saturating_add(bytes.len());
        state.chunks.push_back(Ok(bytes));
        if let Some(waker) = state.consumer_waker.take() {
            waker.wake();
        }
        true
    }

    /// Waits until the application consumes queued bytes or lets the body go.
    ///
    /// Returns the byte count consumed since the previous call and whether no
    /// further consumption can happen.
    #[cfg(feature = "http2")]
    async fn consumed(&self) -> (usize, bool) {
        std::future::poll_fn(|context| {
            let mut state = self.state.borrow_mut();
            let consumed = std::mem::take(&mut state.consumed_bytes);
            let finished = !state.receiver_open || (state.closed && state.chunks.is_empty());
            if consumed > 0 || finished {
                return Poll::Ready((consumed, finished));
            }
            state.producer_waker = Some(context.waker().clone());
            Poll::Pending
        })
        .await
    }

    fn close(&self) {
        let mut state = self.state.borrow_mut();
        state.closed = true;
        if let Some(waker) = state.consumer_waker.take() {
            waker.wake();
        }
        if let Some(waker) = state.producer_waker.take() {
            waker.wake();
        }
    }

    fn fail(&self, error: BodyStreamError) {
        let mut state = self.state.borrow_mut();
        if state.receiver_open {
            state.chunks.push_back(Err(error));
        }
        state.closed = true;
        if let Some(waker) = state.consumer_waker.take() {
            waker.wake();
        }
        if let Some(waker) = state.producer_waker.take() {
            waker.wake();
        }
    }
}

struct NativeIncomingBody {
    state: Rc<RefCell<IncomingBodyState>>,
}

impl BodyStream for NativeIncomingBody {
    fn poll_next(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Vec<u8>, BodyStreamError>>> {
        let mut state = self.state.borrow_mut();
        if let Some(chunk) = state.chunks.pop_front() {
            if let Ok(bytes) = &chunk {
                state.queued_bytes = state.queued_bytes.saturating_sub(bytes.len());
                #[cfg(feature = "http2")]
                {
                    state.consumed_bytes = state.consumed_bytes.saturating_add(bytes.len());
                }
            }
            if let Some(waker) = state.producer_waker.take() {
                waker.wake();
            }
            return Poll::Ready(Some(chunk));
        }
        if state.closed {
            return Poll::Ready(None);
        }
        state.consumer_waker = Some(context.waker().clone());
        Poll::Pending
    }
}

impl Drop for NativeIncomingBody {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        state.receiver_open = false;
        if let Some(waker) = state.producer_waker.take() {
            waker.wake();
        }
    }
}

fn incoming_body_channel(exact_length: Option<u64>) -> (IncomingBodySender, StreamingBody) {
    let state = Rc::new(RefCell::new(IncomingBodyState {
        chunks: VecDeque::new(),
        queued_bytes: 0,
        #[cfg(feature = "http2")]
        consumed_bytes: 0,
        max_queued_bytes: READ_CHUNK_BYTES * 2,
        closed: false,
        receiver_open: true,
        consumer_waker: None,
        producer_waker: None,
    }));
    let sender = IncomingBodySender {
        state: Rc::clone(&state),
    };
    let mut body = StreamingBody::new(NativeIncomingBody { state });
    if let Some(exact_length) = exact_length {
        body = body.with_exact_length(exact_length);
    }
    (sender, body)
}

struct OwnedNativeRequest {
    method: HttpMethod,
    target: String,
    headers: Vec<(String, String)>,
    body: RefCell<Option<StreamingBody>>,
    peer_addr: Option<SocketAddr>,
    scheme: &'static str,
}

impl HttpRequestView for OwnedNativeRequest {
    fn method(&self) -> HttpMethod {
        self.method
    }

    fn target(&self) -> &str {
        &self.target
    }

    fn header_value(&self, name: &str, index: usize) -> Option<&str> {
        self.headers
            .iter()
            .filter(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
            .nth(index)
            .map(|(_, value)| value.as_str())
    }

    fn body(&self) -> &[u8] {
        &[]
    }

    fn take_body_stream(&self) -> Option<StreamingBody> {
        self.body.borrow_mut().take()
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer_addr
    }

    fn scheme(&self) -> &str {
        self.scheme
    }
}

fn owned_request(
    parsed: &ParsedHead,
    buffer: &[u8],
    body: StreamingBody,
    peer_addr: Option<SocketAddr>,
    scheme: &'static str,
) -> io::Result<OwnedNativeRequest> {
    let target = parsed
        .target
        .text(buffer)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "request target is not UTF-8"))?
        .to_owned();
    let headers = parsed
        .headers
        .iter()
        .map(|header| {
            let name = header.name.text(buffer).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "request header name is not UTF-8",
                )
            })?;
            let value = header.value.text(buffer).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "request header value is not UTF-8",
                )
            })?;
            Ok((name.to_owned(), value.to_owned()))
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok(OwnedNativeRequest {
        method: framework_method(parsed.method),
        target,
        headers,
        body: RefCell::new(Some(body)),
        peer_addr,
        scheme,
    })
}

struct NativeRequest<'request> {
    method: HttpMethod,
    target: &'request str,
    buffer: &'request [u8],
    headers: &'request HeaderPositions,
    body: &'request [u8],
    peer_addr: Option<SocketAddr>,
    scheme: &'static str,
}

impl HttpRequestView for NativeRequest<'_> {
    fn method(&self) -> HttpMethod {
        self.method
    }

    fn target(&self) -> &str {
        self.target
    }

    fn header_value(&self, name: &str, index: usize) -> Option<&str> {
        self.headers
            .iter()
            .filter(|header| {
                self.buffer
                    .get(header.name.start..header.name.end)
                    .and_then(|header| std::str::from_utf8(header).ok())
                    .is_some_and(|header| native_header_name_matches(header, name))
            })
            .nth(index)
            .and_then(|header| self.buffer.get(header.value.start..header.value.end))
            .and_then(|value| std::str::from_utf8(value).ok())
    }

    fn body(&self) -> &[u8] {
        self.body
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer_addr
    }

    fn scheme(&self) -> &str {
        self.scheme
    }
}

fn native_header_name_matches(header: &str, argument: &str) -> bool {
    header
        .bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .eq(argument
            .bytes()
            .map(|byte| if byte == b'_' { b'-' } else { byte })
            .map(|byte| byte.to_ascii_lowercase()))
}

async fn write_all_within<IO>(io: &mut IO, bytes: &[u8], write_timeout: Duration) -> io::Result<()>
where
    IO: AsyncWrite + Unpin,
{
    within(write_timeout, io.write_all(bytes))
        .await
        .map_err(|_| deadline_error("response write"))?
}

async fn flush_within<IO>(io: &mut IO, write_timeout: Duration) -> io::Result<()>
where
    IO: AsyncWrite + Unpin,
{
    within(write_timeout, io.flush())
        .await
        .map_err(|_| deadline_error("response flush"))?
}

#[allow(clippy::too_many_arguments)]
async fn write_response<IO>(
    io: &mut IO,
    wire: &mut Vec<u8>,
    response: &mut Response,
    keep_alive: bool,
    send_body: bool,
    send_content_length: bool,
    write_timeout: Duration,
) -> io::Result<()>
where
    IO: AsyncWrite + Unpin,
{
    wire.clear();
    let streaming = response.is_streaming();
    let exact_body_length = response.exact_body_length();
    let chunked = streaming && send_body && send_content_length && exact_body_length.is_none();
    let content_length = send_content_length.then_some(exact_body_length).flatten();
    with_cached_date(|date| {
        blazingly_wire::encode_response_head(
            wire,
            response.status(),
            response.headers(),
            content_length,
            chunked,
            keep_alive,
            date,
        )
    })?;
    if send_body && !streaming {
        wire.extend_from_slice(response.body());
    }
    write_all_within(io, wire, write_timeout).await?;

    if send_body && streaming {
        let mut written = 0_u64;
        while let Some(chunk) = response.next_body_chunk().await {
            let chunk = chunk.map_err(|error| io::Error::other(error.to_string()))?;
            if chunk.is_empty() {
                continue;
            }
            let chunk_length = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
            written = written
                .checked_add(chunk_length)
                .ok_or_else(|| io::Error::other("streaming response length overflow"))?;
            if exact_body_length.is_some_and(|expected| written > expected) {
                return Err(io::Error::other(
                    "streaming response exceeded its declared exact length",
                ));
            }
            if chunked {
                wire.clear();
                blazingly_wire::encode_chunk(wire, &chunk)?;
                write_all_within(io, wire, write_timeout).await?;
            } else {
                write_all_within(io, &chunk, write_timeout).await?;
            }
        }
        if exact_body_length.is_some_and(|expected| written != expected) {
            return Err(io::Error::other(
                "streaming response did not match its declared exact length",
            ));
        }
        if chunked {
            write_all_within(io, blazingly_wire::LAST_CHUNK, write_timeout).await?;
        }
    }
    flush_within(io, write_timeout).await
}

#[allow(clippy::too_many_arguments)]
async fn write_response_native(
    io: &mut TcpStream,
    wire: &mut Vec<u8>,
    response: &mut Response,
    keep_alive: bool,
    send_body: bool,
    send_content_length: bool,
    write_timeout: Duration,
) -> io::Result<()> {
    let streaming = response.is_streaming();
    let exact_body_length = response.exact_body_length();
    let chunked = streaming && send_body && send_content_length && exact_body_length.is_none();
    let content_length = send_content_length.then_some(exact_body_length).flatten();
    with_cached_date(|date| {
        blazingly_wire::encode_response_head(
            wire,
            response.status(),
            response.headers(),
            content_length,
            chunked,
            keep_alive,
            date,
        )
    })?;
    if send_body && !streaming {
        wire.extend_from_slice(response.body());
    }
    if streaming {
        native_write_all(io, wire, write_timeout).await?;
    }

    if send_body && streaming {
        let mut written = 0_u64;
        while let Some(chunk) = response.next_body_chunk().await {
            let chunk = chunk.map_err(|error| io::Error::other(error.to_string()))?;
            if chunk.is_empty() {
                continue;
            }
            let chunk_length = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
            written = written
                .checked_add(chunk_length)
                .ok_or_else(|| io::Error::other("streaming response length overflow"))?;
            if exact_body_length.is_some_and(|expected| written > expected) {
                return Err(io::Error::other(
                    "streaming response exceeded its declared exact length",
                ));
            }
            if chunked {
                wire.clear();
                blazingly_wire::encode_chunk(wire, &chunk)?;
                native_write_all(io, wire, write_timeout).await?;
            } else {
                let result = within(write_timeout, CompioAsyncWriteExt::write_all(io, chunk))
                    .await
                    .map_err(|_| deadline_error("response write"))?;
                result.0?;
            }
        }
        if exact_body_length.is_some_and(|expected| written != expected) {
            return Err(io::Error::other(
                "streaming response did not match its declared exact length",
            ));
        }
        if chunked {
            let result = within(
                write_timeout,
                CompioAsyncWriteExt::write_all(io, blazingly_wire::LAST_CHUNK),
            )
            .await
            .map_err(|_| deadline_error("response write"))?;
            result.0?;
        }
    }
    Ok(())
}

async fn native_write_all(
    io: &mut TcpStream,
    wire: &mut Vec<u8>,
    write_timeout: Duration,
) -> io::Result<()> {
    if wire.is_empty() {
        return Ok(());
    }
    let result = within(
        write_timeout,
        CompioAsyncWriteExt::write_all(io, std::mem::take(wire)),
    )
    .await
    .map_err(|_| deadline_error("response write"))?;
    let outcome = result.0;
    *wire = result.1;
    if outcome.is_ok() {
        wire.clear();
    }
    outcome
}

async fn flush_native_pending(
    io: &mut TcpStream,
    wire: &mut Vec<u8>,
    write_timeout: Duration,
) -> io::Result<()> {
    native_write_all(io, wire, write_timeout).await
}

async fn write_rejection<IO>(
    io: &mut IO,
    wire: &mut Vec<u8>,
    status: u16,
    code: &str,
    message: &str,
    write_timeout: Duration,
) -> io::Result<()>
where
    IO: AsyncWrite + Unpin,
{
    let body = format!(r#"{{"error":{{"code":"{code}","message":"{message}"}}}}"#);
    wire.clear();
    write!(
        wire,
        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
        reason_phrase(status),
        body.len()
    )?;
    wire.extend_from_slice(b"date: ");
    with_cached_date(|date| wire.extend_from_slice(date.as_bytes()));
    wire.extend_from_slice(b"\r\n");
    wire.extend_from_slice(b"connection: close\r\n\r\n");
    wire.extend_from_slice(body.as_bytes());
    write_all_within(io, wire, write_timeout).await?;
    flush_within(io, write_timeout).await
}

async fn write_rejection_native(
    io: &mut TcpStream,
    wire: &mut Vec<u8>,
    status: u16,
    code: &str,
    message: &str,
    write_timeout: Duration,
) -> io::Result<()> {
    flush_native_pending(io, wire, write_timeout).await?;
    let body = format!(r#"{{"error":{{"code":"{code}","message":"{message}"}}}}"#);
    wire.clear();
    write!(
        wire,
        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
        reason_phrase(status),
        body.len()
    )?;
    wire.extend_from_slice(b"date: ");
    with_cached_date(|date| wire.extend_from_slice(date.as_bytes()));
    wire.extend_from_slice(b"\r\n");
    wire.extend_from_slice(b"connection: close\r\n\r\n");
    wire.extend_from_slice(body.as_bytes());
    native_write_all(io, wire, write_timeout).await
}

fn consume_prefix(buffer: &mut Vec<u8>, consumed: usize) {
    if consumed == buffer.len() {
        buffer.clear();
    } else {
        buffer.copy_within(consumed.., 0);
        buffer.truncate(buffer.len() - consumed);
    }
}

fn with_cached_date<Result>(callback: impl FnOnce(&str) -> Result) -> Result {
    // The mutex publishes the actual string. The generation is only a cheap
    // change hint, so the request hot path does not need an acquire fence.
    let generation = DATE_GENERATION.load(Ordering::Relaxed);
    DATE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.generation != generation {
            let value = DATE_VALUE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.value.clone_from(&value);
            cache.generation = generation;
        }
        callback(&cache.value)
    })
}

fn ensure_date_updater() {
    DATE_UPDATER.call_once(|| {
        refresh_cached_date();
        std::thread::Builder::new()
            .name("blazingly-date".to_owned())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    refresh_cached_date();
                }
            })
            .expect("failed to start the HTTP Date updater");
    });
}

fn refresh_cached_date() {
    let value = httpdate::fmt_http_date(std::time::SystemTime::now());
    let mut cached = DATE_VALUE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *cached = value;
    DATE_GENERATION.fetch_add(1, Ordering::Relaxed);
}

impl fmt::Debug for Server {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Server")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HttpMiddleware, MulticoreServer, Rejection, Runtime, Server, ServerLimits, parse_head,
    };
    use blazingly_core::{
        HttpMethod, HttpUpgrade, InputDescriptor, InputSource, Json, OperationDescriptor,
        ResponseDescriptor, ResponseHeader, SchemaKind, TypeDescriptor,
    };
    use blazingly_executor::{
        DependencyError, ExecutableApp, ExecutableOperation, ExecutionOutcome, FromInvocation,
        OperationFuture, Plugin, UploadBody,
    };
    use blazingly_http::{HttpRequestContext, Request, Response};
    use compio::io::{
        AsyncReadExt as CompioAsyncReadExt, AsyncWrite as CompioAsyncWrite,
        AsyncWriteExt as CompioAsyncWriteExt,
    };
    use futures_lite::future;
    use futures_lite::io::{AsyncRead, AsyncWrite};
    use std::cell::RefCell;
    use std::io;
    use std::num::NonZeroUsize;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::task::{Context, Poll};
    use std::time::Duration;

    /// An in-memory transport that replays one script and then either closes
    /// or stalls, modelling a peer that opened a connection and went silent.
    struct ScriptedTransport {
        input: Vec<u8>,
        close_after_input: bool,
        written: Rc<RefCell<Vec<u8>>>,
    }

    impl ScriptedTransport {
        fn closing(input: &[u8]) -> Self {
            Self {
                input: input.to_vec(),
                close_after_input: true,
                written: Rc::new(RefCell::new(Vec::new())),
            }
        }

        fn half_open(input: &[u8]) -> Self {
            Self {
                input: input.to_vec(),
                close_after_input: false,
                written: Rc::new(RefCell::new(Vec::new())),
            }
        }

        /// Shares the recorded output so a moved transport can still be read.
        fn output(&self) -> Rc<RefCell<Vec<u8>>> {
            Rc::clone(&self.written)
        }

        fn written(&self) -> String {
            String::from_utf8_lossy(&self.written.borrow()).into_owned()
        }
    }

    impl AsyncRead for ScriptedTransport {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            if self.input.is_empty() {
                return if self.close_after_input {
                    Poll::Ready(Ok(0))
                } else {
                    Poll::Pending
                };
            }
            let read = self.input.len().min(buffer.len());
            buffer[..read].copy_from_slice(&self.input[..read]);
            self.input.drain(..read);
            Poll::Ready(Ok(read))
        }
    }

    impl AsyncWrite for ScriptedTransport {
        fn poll_write(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.written.borrow_mut().extend_from_slice(buffer);
            Poll::Ready(Ok(buffer.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct StampMiddleware;

    impl HttpMiddleware for StampMiddleware {
        fn on_response(
            &self,
            _context: &HttpRequestContext<'_>,
            _operation: Option<&OperationDescriptor>,
            response: &mut Response,
        ) {
            response.set_header("x-native-middleware", "applied");
        }
    }

    fn ping_operation() -> ExecutableOperation {
        let descriptor = OperationDescriptor::new(
            HttpMethod::Get,
            "/ping",
            "ping",
            "Ping",
            None,
            vec![ResponseDescriptor::success(200, None)],
        )
        .expect("the ping descriptor is valid");
        ExecutableOperation::empty(descriptor, || async { Json("pong") })
    }

    fn ping_app() -> ExecutableApp {
        ExecutableApp::new([ping_operation()]).expect("the ping app compiles")
    }

    /// An upgrade that echoes the first frame the peer sends after the
    /// handshake, including bytes coalesced with the upgrade request.
    fn echo_upgrade_operation() -> ExecutableOperation {
        let descriptor = OperationDescriptor::new(
            HttpMethod::Get,
            "/upgrade",
            "upgrade.echo",
            "Echo upgrade",
            None,
            vec![ResponseDescriptor::success(200, None)],
        )
        .expect("the upgrade descriptor is valid");
        ExecutableOperation::empty(descriptor, || async {
            HttpUpgrade::new(
                "echo",
                vec![
                    ResponseHeader::new("connection", "Upgrade"),
                    ResponseHeader::new("upgrade", "echo"),
                ],
                |mut io| {
                    Box::pin(async move {
                        while let Some(bytes) = io.read().await? {
                            if bytes.is_empty() {
                                continue;
                            }
                            io.write(bytes).await?;
                            break;
                        }
                        io.shutdown().await
                    })
                },
            )
        })
    }

    /// A streaming upload operation that reports how many bytes it pulled.
    ///
    /// `read_body` is false for the flow-control tests, where the handler must
    /// hold the body without consuming it.
    fn upload_operation(
        path: &'static str,
        id: &'static str,
        read_body: bool,
    ) -> ExecutableOperation {
        let descriptor = OperationDescriptor::new(
            HttpMethod::Post,
            path,
            id,
            "Upload",
            None,
            vec![ResponseDescriptor::success(200, None)],
        )
        .expect("the upload descriptor is valid")
        .with_inputs(vec![InputDescriptor::new(
            "body",
            InputSource::Stream,
            true,
            TypeDescriptor::scalar("UploadBody", SchemaKind::Binary),
        )]);
        ExecutableOperation::typed(descriptor, move |input| {
            let mut body = UploadBody::from_invocation(&input, "body", true)?;
            Ok(Box::pin(async move {
                if !read_body {
                    std::future::pending::<()>().await;
                }
                let mut bytes = 0_usize;
                while let Some(chunk) = body.next_chunk().await {
                    match chunk {
                        Ok(chunk) => bytes += chunk.len(),
                        Err(error) => {
                            return ExecutionOutcome::InternalError {
                                code: error.code,
                                message: error.message,
                            };
                        }
                    }
                }
                ExecutionOutcome::Success {
                    status: 200,
                    headers: vec![ResponseHeader::new("content-type", "text/plain")],
                    body: Some(format!("bytes={bytes}").into_bytes()),
                    background: Vec::new(),
                }
            }) as OperationFuture)
        })
    }

    #[cfg(feature = "http2")]
    fn never_completing_operation() -> ExecutableOperation {
        let descriptor = OperationDescriptor::new(
            HttpMethod::Get,
            "/forever",
            "forever.wait",
            "Never completes",
            None,
            vec![ResponseDescriptor::success(200, None)],
        )
        .expect("the waiting descriptor is valid");
        ExecutableOperation::empty(descriptor, || async {
            std::future::pending::<()>().await;
            blazingly_core::NoContent
        })
    }

    /// Runs one request against a real accepted socket through the same entry
    /// point the accept loop uses, and returns every byte the server wrote.
    fn native_exchange(operations: Vec<ExecutableOperation>, request: Vec<u8>) -> Vec<u8> {
        let runtime = Runtime::new().expect("the Compio runtime starts");
        runtime.block_on(async move {
            let listener = compio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("the loopback listener binds");
            let address = listener.local_addr().expect("the listener has an address");
            let app = Rc::new(super::HttpApp::new(
                ExecutableApp::new(operations).expect("the test app compiles"),
            ));
            let mut client = compio::net::TcpStream::connect(address)
                .await
                .expect("the client connects");
            let (stream, peer) = listener.accept().await.expect("the server accepts");
            let served = super::spawn(async move {
                super::serve_accepted(
                    app.as_ref(),
                    stream,
                    peer,
                    None,
                    super::ConnectionSetup {
                        limits: ServerLimits::new(),
                        request_timeout: None,
                        #[cfg(feature = "tls")]
                        tls_acceptor: None,
                    },
                )
                .await;
            });
            let write = CompioAsyncWriteExt::write_all(&mut client, request).await;
            write.0.expect("the request is written");
            CompioAsyncWrite::shutdown(&mut client)
                .await
                .expect("the client half-closes");
            let read = CompioAsyncReadExt::read_to_end(&mut client, Vec::new()).await;
            read.0.expect("the response is read");
            served.await.expect("the connection task finishes");
            read.1
        })
    }

    #[test]
    fn header_parser_honors_limits_below_the_inline_capacity() {
        let request = b"GET / HTTP/1.1\r\nhost: localhost\r\nx-extra: value\r\n\r\n";
        let result = parse_head(request, ServerLimits::new().with_max_headers(1));

        assert!(matches!(result, Err(Rejection { status: 431, .. })));
    }

    #[test]
    fn registered_middleware_runs_on_a_native_connection() {
        let server = Server::new(ping_app()).with_middleware(StampMiddleware);
        let mut transport =
            ScriptedTransport::closing(b"GET /ping HTTP/1.1\r\nhost: localhost\r\n\r\n");

        future::block_on(server.serve_io(&mut transport)).expect("the connection completes");

        let written = transport.written();
        assert!(written.starts_with("HTTP/1.1 200 "), "{written}");
        assert!(
            written.contains("x-native-middleware: applied"),
            "{written}"
        );
    }

    #[test]
    fn shared_middleware_runs_on_a_native_connection() {
        let server = Server::new(ping_app()).with_shared_middleware(Rc::new(StampMiddleware));
        let mut transport =
            ScriptedTransport::closing(b"GET /ping HTTP/1.1\r\nhost: localhost\r\n\r\n");

        future::block_on(server.serve_io(&mut transport)).expect("the connection completes");

        assert!(
            transport.written().contains("x-native-middleware: applied"),
            "{}",
            transport.written()
        );
    }

    #[test]
    fn header_read_deadline_answers_408_and_closes_a_half_open_request() {
        let limits = ServerLimits::new().with_header_read_timeout(Duration::from_millis(50));
        let server = Server::new(ping_app()).with_limits(limits);
        let runtime = Runtime::new().expect("the Compio runtime starts");

        let written = runtime.block_on(async {
            let mut transport = ScriptedTransport::half_open(b"GET /ping HTTP/1.1\r\nhost: loc");
            server
                .serve_io(&mut transport)
                .await
                .expect("an expired header deadline closes the connection cleanly");
            transport.written()
        });

        assert!(written.starts_with("HTTP/1.1 408 "), "{written}");
        assert!(written.contains("request_timeout"), "{written}");
    }

    #[test]
    fn idle_deadline_closes_a_silent_connection_without_a_response() {
        let limits = ServerLimits::new().with_idle_timeout(Duration::from_millis(50));
        let server = Server::new(ping_app()).with_limits(limits);
        let runtime = Runtime::new().expect("the Compio runtime starts");

        let written = runtime.block_on(async {
            let mut transport = ScriptedTransport::half_open(b"");
            server
                .serve_io(&mut transport)
                .await
                .expect("an expired idle deadline closes the connection cleanly");
            transport.written()
        });

        assert!(written.is_empty(), "{written}");
    }

    #[test]
    fn multicore_serve_refuses_to_boot_when_worker_startup_fails() {
        let workers = NonZeroUsize::new(1).expect("one worker is non-zero");
        let server = MulticoreServer::new(workers, || {
            ExecutableApp::from_plugin(Plugin::new("app").routes([ping_operation()]).on_startup(
                || async {
                    Err(DependencyError::internal(
                        "startup_failed",
                        "startup hook refused to boot",
                    ))
                },
            ))
            .expect("the failing app compiles")
        });

        let error = server
            .serve("127.0.0.1:0")
            .expect_err("a failing startup hook aborts serve");

        assert_eq!(error.to_string(), "startup hook refused to boot");
    }

    #[test]
    fn owned_compatibility_transport_hands_ownership_to_an_upgrade() {
        let server = Server::new(
            ExecutableApp::new([echo_upgrade_operation()]).expect("the upgrade app compiles"),
        );
        let transport = ScriptedTransport::closing(
            b"GET /upgrade HTTP/1.1\r\nhost: localhost\r\nconnection: Upgrade\r\nupgrade: echo\r\n\r\nPING",
        );
        let written = transport.output();

        future::block_on(server.serve_owned_io(transport)).expect("the upgraded session completes");

        let written = String::from_utf8_lossy(&written.borrow()).into_owned();
        assert!(
            written.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
            "{written}"
        );
        assert!(written.contains("upgrade: echo\r\n"), "{written}");
        assert!(
            written.ends_with("PING"),
            "the bytes buffered behind the handshake were lost: {written}"
        );
    }

    #[test]
    fn borrowed_transport_still_refuses_an_upgrade() {
        let server = Server::new(
            ExecutableApp::new([echo_upgrade_operation()]).expect("the upgrade app compiles"),
        );
        let mut transport = ScriptedTransport::closing(
            b"GET /upgrade HTTP/1.1\r\nhost: localhost\r\nconnection: Upgrade\r\nupgrade: echo\r\n\r\n",
        );

        future::block_on(server.serve_io(&mut transport)).expect("the connection completes");

        let written = transport.written();
        assert!(written.starts_with("HTTP/1.1 501 "), "{written}");
        assert!(
            written.contains("upgrade_transport_unsupported"),
            "{written}"
        );
    }

    #[cfg(any(feature = "http2", feature = "tls"))]
    #[test]
    fn compatibility_stream_upgrade_matches_the_tls_transport() {
        let runtime = Runtime::new().expect("the Compio runtime starts");
        let response = runtime.block_on(async {
            let listener = compio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("the loopback listener binds");
            let address = listener.local_addr().expect("the listener has an address");
            let app = Rc::new(super::HttpApp::new(
                ExecutableApp::new([echo_upgrade_operation()]).expect("the upgrade app compiles"),
            ));
            let mut client = compio::net::TcpStream::connect(address)
                .await
                .expect("the client connects");
            let (stream, peer) = listener.accept().await.expect("the server accepts");
            let served = super::spawn(async move {
                super::serve_compat_connection(
                    app.as_ref(),
                    ServerLimits::new(),
                    Box::pin(super::AsyncStream::new(stream)),
                    Some(peer),
                    "http",
                    None,
                    None,
                )
                .await
            });
            let request = b"GET /upgrade HTTP/1.1\r\nhost: localhost\r\nconnection: Upgrade\r\nupgrade: echo\r\n\r\nPING".to_vec();
            let write = CompioAsyncWriteExt::write_all(&mut client, request).await;
            write.0.expect("the request is written");
            let read = CompioAsyncReadExt::read_to_end(&mut client, Vec::new()).await;
            read.0.expect("the response is read");
            served
                .await
                .expect("the connection task finishes")
                .expect("the upgraded session completes");
            read.1
        });

        let response = String::from_utf8_lossy(&response).into_owned();
        assert!(
            response.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
            "{response}"
        );
        assert!(
            response.ends_with("PING"),
            "the compatibility transport lost the buffered frame: {response}"
        );
    }

    #[test]
    fn native_socket_upgrade_survives_every_feature_combination() {
        let response = native_exchange(
            vec![echo_upgrade_operation()],
            b"GET /upgrade HTTP/1.1\r\nhost: localhost\r\nconnection: Upgrade\r\nupgrade: echo\r\n\r\nPING".to_vec(),
        );

        let response = String::from_utf8_lossy(&response).into_owned();
        assert!(
            response.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
            "{response}"
        );
        assert!(response.ends_with("PING"), "{response}");
    }

    #[test]
    fn native_socket_streams_uploads_in_every_feature_combination() {
        let response = native_exchange(
            vec![upload_operation("/upload", "upload.consume", true)],
            b"POST /upload HTTP/1.1\r\nhost: localhost\r\ncontent-length: 11\r\nconnection: close\r\n\r\nhello world"
                .to_vec(),
        );

        let response = String::from_utf8_lossy(&response).into_owned();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(
            response.ends_with("bytes=11"),
            "the streaming upload seam did not run: {response}"
        );
    }

    #[cfg(feature = "http2")]
    fn h2_client(
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> (
        shiguredo_http2::Connection,
        shiguredo_http2::StreamId,
        Vec<u8>,
    ) {
        use shiguredo_http2::{Connection, HeaderField, Limits};

        let mut client = Connection::client(Limits::default());
        client.initiate().expect("the client preface is written");
        let mut headers = vec![
            HeaderField::new(":method", method).expect("method header"),
            HeaderField::new(":path", path).expect("path header"),
            HeaderField::new(":scheme", "http").expect("scheme header"),
            HeaderField::new(":authority", "localhost").expect("authority header"),
        ];
        if let Some(body) = body {
            headers.push(
                HeaderField::new("content-length", body.len().to_string())
                    .expect("content-length header"),
            );
        }
        let stream = client
            .start_stream(headers, body.is_none())
            .expect("the request stream starts");
        if let Some(body) = body {
            client
                .send_data(stream, body.to_vec(), true)
                .expect("the request body is sent");
        }
        let mut bytes = Vec::new();
        while let Some(output) = client.poll_output() {
            bytes.extend_from_slice(&output);
        }
        (client, stream, bytes)
    }

    #[cfg(feature = "http2")]
    fn h2_events(
        client: &mut shiguredo_http2::Connection,
        output: &[u8],
    ) -> Vec<shiguredo_http2::Event> {
        client.feed(output).expect("the server output is accepted");
        client.process().expect("the server output is processed");
        let mut events = Vec::new();
        while let Some(event) = client.poll_event() {
            events.push(event);
        }
        events
    }

    #[cfg(feature = "http2")]
    #[test]
    fn http2_preface_still_reaches_the_codec_on_a_plaintext_socket() {
        use shiguredo_http2::Event;

        let (mut client, stream, request) = h2_client("GET", "/ping", None);
        let response = native_exchange(vec![ping_operation()], request);
        let events = h2_events(&mut client, &response);

        let status = events.iter().find_map(|event| match event {
            Event::HeadersReceived {
                stream_id, headers, ..
            } if *stream_id == stream => headers
                .iter()
                .find(|header| header.name() == b":status")
                .map(|header| header.value().to_vec()),
            _ => None,
        });
        assert_eq!(status.as_deref(), Some(b"200".as_slice()));
    }

    #[cfg(feature = "http2")]
    #[test]
    fn http2_streams_request_bodies_through_the_upload_seam() {
        use shiguredo_http2::Event;

        let (mut client, stream, request) = h2_client("POST", "/upload", Some(b"hello world"));
        let server = Server::new(
            ExecutableApp::new([upload_operation("/upload", "upload.consume", true)])
                .expect("the upload app compiles"),
        );
        let mut transport = ScriptedTransport::closing(&request);
        future::block_on(server.serve_http2_io(&mut transport))
            .expect("the HTTP/2 exchange completes");
        let output = transport.output().borrow().clone();
        let events = h2_events(&mut client, &output);

        let mut body = Vec::new();
        for event in &events {
            if let Event::DataReceived {
                stream_id, data, ..
            } = event
                && *stream_id == stream
            {
                body.extend_from_slice(data);
            }
        }
        assert_eq!(String::from_utf8_lossy(&body), "bytes=11");
    }

    #[cfg(feature = "http2")]
    #[test]
    fn http2_returns_receive_window_credit_only_as_the_handler_consumes() {
        use shiguredo_http2::StreamId;

        let consuming = h2_window_credit(true);
        assert!(
            consuming.iter().any(|(stream_id, increment)| matches!(
                stream_id,
                StreamId::Connection
            ) && *increment == 11),
            "a consuming handler must return exactly the credit it read: {consuming:?}"
        );

        let idle = h2_window_credit(false);
        assert!(
            idle.is_empty(),
            "credit was returned for bytes the handler never read: {idle:?}"
        );
    }

    /// Drives one HTTP/2 upload and reports every `WINDOW_UPDATE` the server
    /// sent, with `read_body` selecting a handler that consumes or stalls.
    #[cfg(feature = "http2")]
    fn h2_window_credit(read_body: bool) -> Vec<(shiguredo_http2::StreamId, u32)> {
        use shiguredo_http2::Event;

        let (mut client, _, request) = h2_client("POST", "/upload", Some(b"hello world"));
        let server = Server::new(
            ExecutableApp::new([upload_operation("/upload", "upload.consume", read_body)])
                .expect("the upload app compiles"),
        );
        let runtime = Runtime::new().expect("the Compio runtime starts");
        let output = runtime.block_on(async {
            let mut transport = if read_body {
                ScriptedTransport::closing(&request)
            } else {
                ScriptedTransport::half_open(&request)
            };
            let recorded = transport.output();
            let _ = compio::time::timeout(
                Duration::from_millis(500),
                server.serve_http2_io(&mut transport),
            )
            .await;
            recorded.borrow().clone()
        });
        h2_events(&mut client, &output)
            .into_iter()
            .filter_map(|event| match event {
                Event::WindowUpdateReceived {
                    stream_id,
                    increment,
                } => Some((stream_id, increment)),
                _ => None,
            })
            .collect()
    }

    #[cfg(feature = "http2")]
    #[test]
    fn http2_stream_reset_drops_the_in_flight_handler() {
        use shiguredo_http2::ErrorCode;

        let (mut client, stream, mut request) = h2_client("GET", "/forever", None);
        client
            .reset_stream(stream, ErrorCode::Cancel)
            .expect("the client resets its stream");
        while let Some(output) = client.poll_output() {
            request.extend_from_slice(&output);
        }

        let server = Server::new(
            ExecutableApp::new([never_completing_operation()]).expect("the waiting app compiles"),
        );
        let runtime = Runtime::new().expect("the Compio runtime starts");
        let completed = runtime.block_on(async {
            let mut transport = ScriptedTransport::closing(&request);
            compio::time::timeout(
                Duration::from_secs(2),
                server.serve_http2_io(&mut transport),
            )
            .await
            .is_ok()
        });

        assert!(
            completed,
            "a reset stream must drop its handler instead of holding the connection open"
        );
    }

    #[test]
    fn worker_middleware_factory_is_applied_to_the_worker_app() {
        let config = super::WorkerConfig {
            factory: ping_app,
            max_body_bytes: super::DEFAULT_MAX_BODY_BYTES,
            openapi: None,
            middleware: Some(Arc::new(|| {
                vec![Rc::new(StampMiddleware) as Rc<dyn HttpMiddleware>]
            })),
        };
        let server_id = super::NEXT_MULTICORE_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let (app, created) = super::worker_app(server_id, &config);
        assert!(created);

        let response = future::block_on(app.call(Request::new(HttpMethod::Get, "/ping")));
        super::take_worker_app(server_id);

        assert_eq!(
            response.get_header("x-native-middleware"),
            Some("applied"),
            "the worker factory middleware did not run"
        );
    }
}
