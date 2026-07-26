//! Native HTTP/1 adapter for Blazingly.
//!
//! The adapter deliberately contains every socket-runtime and wire-protocol
//! dependency. The operation graph, router, DI, MCP, and documentation crates
//! remain runtime-neutral. Tokio is not part of this crate's dependency tree.

#[cfg(feature = "http2")]
mod http2;

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
use std::time::Duration;

use blazingly_core::HttpMethod;
use blazingly_executor::{ExecutableApp, InvocationControl};
use blazingly_http::{HttpApp, HttpRequestView, Response};
use blazingly_openapi::OpenApiConfig;
use compio::dispatcher::Dispatcher;
use compio::io::compat::AsyncStream;
use compio::net::TcpListener;
use compio::runtime::{Runtime, spawn};
use futures_lite::future;
use futures_lite::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Default maximum request body accepted by the socket adapter.
pub const DEFAULT_MAX_BODY_BYTES: usize = blazingly_http::DEFAULT_MAX_BODY_BYTES;

const DEFAULT_MAX_HEADER_BYTES: usize = 32 * 1024;
const DEFAULT_MAX_HEADERS: usize = 64;
const DEFAULT_MAX_CHUNKS: usize = 8 * 1024;
const MAX_HEADER_CAPACITY: usize = 128;
const READ_CHUNK_BYTES: usize = 8 * 1024;
const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
static NEXT_MULTICORE_SERVER_ID: AtomicU64 = AtomicU64::new(1);
static DATE_UPDATER: Once = Once::new();
static DATE_GENERATION: AtomicU64 = AtomicU64::new(0);
static DATE_VALUE: Mutex<String> = Mutex::new(String::new());

thread_local! {
    static WORKER_APPS: std::cell::RefCell<std::collections::HashMap<u64, Rc<HttpApp>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    static DATE_CACHE: std::cell::RefCell<CachedDate> = const {
        std::cell::RefCell::new(CachedDate {
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct ServerLimits {
    max_header_bytes: usize,
    max_headers: usize,
    max_body_bytes: usize,
    max_chunks: usize,
    max_requests_per_connection: Option<NonZeroUsize>,
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
            max_requests_per_connection: None,
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

    /// Bounds keep-alive reuse. `None` allows requests until either peer closes.
    #[must_use]
    pub const fn with_max_requests_per_connection(mut self, count: Option<NonZeroUsize>) -> Self {
        self.max_requests_per_connection = count;
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
    pub const fn max_requests_per_connection(self) -> Option<NonZeroUsize> {
        self.max_requests_per_connection
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

    /// Serves HTTP/1 requests over an arbitrary futures-I/O transport.
    ///
    /// This is useful for in-memory protocol tests and for future TLS adapters.
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
            Vec::new(),
            self.request_timeout,
        )
        .await
    }

    async fn serve_listener(
        &self,
        listener: TcpListener,
        shutdown: Option<ShutdownSignal>,
        drain_timeout: Duration,
    ) -> io::Result<()> {
        let active = Rc::new(std::cell::Cell::new(0_usize));
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
            let stream = match accepted {
                AcceptEvent::Connection(result) => result?.0,
                AcceptEvent::Shutdown => break,
            };
            let app = Rc::clone(&self.app);
            let limits = self.limits;
            let request_timeout = self.request_timeout;
            let active_for_task = Rc::clone(&active);
            active.set(active.get() + 1);
            let connection_shutdown = shutdown_state.clone();
            #[cfg(feature = "tls")]
            let tls_acceptor = self.tls_acceptor.clone();
            spawn(async move {
                let _guard = ActiveConnection::new(active_for_task);
                #[cfg(feature = "tls")]
                if let Some(tls_acceptor) = tls_acceptor {
                    if let Ok(stream) = tls_acceptor.accept(stream).await {
                        let mut stream = Box::pin(AsyncStream::new(stream));
                        let _ = serve_connection(
                            app.as_ref(),
                            limits,
                            &mut stream,
                            connection_shutdown.as_deref(),
                            request_timeout,
                        )
                        .await;
                    }
                    return;
                }
                let mut stream = Box::pin(AsyncStream::new(stream));
                let _ = serve_connection(
                    app.as_ref(),
                    limits,
                    &mut stream,
                    connection_shutdown.as_deref(),
                    request_timeout,
                )
                .await;
            })
            .detach();
        }
        let drain = async {
            while active.get() != 0 {
                compio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let _ = compio::time::timeout(drain_timeout, drain).await;
        Ok(())
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

    #[cfg(feature = "tls")]
    #[must_use]
    pub fn with_tls_config(mut self, config: TlsArc<compio::tls::rustls::ServerConfig>) -> Self {
        self.tls_acceptor = Some(config.into());
        self
    }

    /// Runs until the listener fails or the process stops.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when binding, accepting, or dispatching fails.
    pub fn serve(self, address: impl ToSocketAddrs) -> io::Result<()> {
        self.serve_inner(address, None, DEFAULT_DRAIN_TIMEOUT)
    }

    /// Stops accepting after `shutdown`, asks keep-alive connections to close
    /// after the current response, and drains active work.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when binding, accepting, or dispatching fails.
    #[allow(clippy::needless_pass_by_value)]
    pub fn serve_gracefully(
        self,
        address: impl ToSocketAddrs,
        shutdown: ShutdownSignal,
        drain_timeout: Duration,
    ) -> io::Result<()> {
        self.serve_inner(address, Some(&shutdown.state), drain_timeout)
    }

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
        let factory = Arc::new(self.factory);
        let active = Arc::new(AtomicUsize::new(0));
        let server_id = NEXT_MULTICORE_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let mut next_worker = 0_usize;

        loop {
            if shutdown
                .as_ref()
                .is_some_and(|shutdown| shutdown.requested.load(Ordering::Acquire))
            {
                break;
            }
            let (stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if shutdown.is_some() && error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(error) => return Err(error),
            };

            let factory = Arc::clone(&factory);
            let active_for_task = Arc::clone(&active);
            let connection_shutdown = shutdown.cloned();
            let limits = self.limits;
            let request_timeout = self.request_timeout;
            let openapi = self.openapi.clone();
            #[cfg(feature = "tls")]
            let tls_acceptor = self.tls_acceptor.clone();
            active.fetch_add(1, Ordering::AcqRel);
            let dispatcher = &dispatchers[next_worker];
            next_worker = (next_worker + 1) % dispatchers.len();
            if dispatcher
                .dispatch(move || async move {
                    let _guard = MulticoreActiveConnection::new(active_for_task);
                    let app = worker_app(
                        server_id,
                        factory.as_ref(),
                        limits.max_body_bytes,
                        openapi.as_ref(),
                    );
                    let Ok(stream) = compio::net::TcpStream::from_std(stream) else {
                        return;
                    };
                    #[cfg(feature = "tls")]
                    if let Some(tls_acceptor) = tls_acceptor {
                        if let Ok(stream) = tls_acceptor.accept(stream).await {
                            let mut stream = Box::pin(AsyncStream::new(stream));
                            let _ = serve_connection(
                                app.as_ref(),
                                limits,
                                &mut stream,
                                connection_shutdown.as_deref(),
                                request_timeout,
                            )
                            .await;
                        }
                        return;
                    }
                    let mut stream = Box::pin(AsyncStream::new(stream));
                    let _ = serve_connection(
                        app.as_ref(),
                        limits,
                        &mut stream,
                        connection_shutdown.as_deref(),
                        request_timeout,
                    )
                    .await;
                })
                .is_err()
            {
                active.fetch_sub(1, Ordering::AcqRel);
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "all Compio workers stopped",
                ));
            }
        }

        let deadline = std::time::Instant::now() + drain_timeout;
        while active.load(Ordering::Acquire) != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        for dispatcher in dispatchers {
            future::block_on(dispatcher.join())?;
        }
        Ok(())
    }
}

fn worker_app<Factory>(
    server_id: u64,
    factory: &Factory,
    max_body_bytes: usize,
    openapi: Option<&OpenApiConfig>,
) -> Rc<HttpApp>
where
    Factory: Fn() -> ExecutableApp,
{
    WORKER_APPS.with(|apps| {
        let mut apps = apps.borrow_mut();
        Rc::clone(apps.entry(server_id).or_insert_with(|| {
            let app = HttpApp::new(factory()).with_max_body_bytes(max_body_bytes);
            let app = match openapi {
                Some(config) => app.with_openapi(config.clone()),
                None => app,
            };
            Rc::new(app)
        }))
    })
}

struct MulticoreActiveConnection {
    active: Arc<AtomicUsize>,
}

impl MulticoreActiveConnection {
    fn new(active: Arc<AtomicUsize>) -> Self {
        Self { active }
    }
}

impl Drop for MulticoreActiveConnection {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

enum AcceptEvent {
    Connection(io::Result<(compio::net::TcpStream, SocketAddr)>),
    Shutdown,
}

struct ActiveConnection {
    active: Rc<std::cell::Cell<usize>>,
}

impl ActiveConnection {
    fn new(active: Rc<std::cell::Cell<usize>>) -> Self {
        Self { active }
    }
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.active.set(self.active.get().saturating_sub(1));
    }
}

fn resolve_address(address: impl ToSocketAddrs) -> io::Result<SocketAddr> {
    address.to_socket_addrs()?.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "address resolved to no sockets",
        )
    })
}

#[allow(clippy::too_many_lines)]
async fn serve_connection<IO>(
    app: &HttpApp,
    limits: ServerLimits,
    io: &mut IO,
    shutdown: Option<&ShutdownState>,
    request_timeout: Option<Duration>,
) -> io::Result<()>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    ensure_date_updater();
    let mut buffer = Vec::with_capacity(READ_CHUNK_BYTES);
    let mut read_chunk = vec![0_u8; READ_CHUNK_BYTES];
    let mut wire_response = Vec::with_capacity(READ_CHUNK_BYTES);
    let mut completed_requests = 0_usize;

    loop {
        let parsed = loop {
            #[cfg(feature = "http2")]
            {
                if buffer.starts_with(shiguredo_http2::CONNECTION_PREFACE) {
                    return Box::pin(http2::serve_connection(
                        app,
                        limits,
                        io,
                        shutdown,
                        std::mem::take(&mut buffer),
                        request_timeout,
                    ))
                    .await;
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
                        )
                        .await?;
                        return Ok(());
                    }
                    Ok(None) => {}
                    Err(rejection) => {
                        write_rejection(
                            io,
                            &mut wire_response,
                            rejection.status,
                            rejection.code,
                            rejection.message,
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }

            let read = io.read(&mut read_chunk).await?;
            if read == 0 {
                return Ok(());
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
                    let read = io.read(&mut read_chunk).await?;
                    if read == 0 {
                        write_rejection(
                            io,
                            &mut wire_response,
                            400,
                            "incomplete_body",
                            "request body ended before Content-Length bytes arrived",
                        )
                        .await?;
                        return Ok(());
                    }
                    buffer.extend_from_slice(&read_chunk[..read]);
                }
                request_bytes
            }
            BodyFraming::Chunked => {
                let mut decoder = ChunkDecoder::new(parsed.head_bytes, limits);
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
                            )
                            .await?;
                            return Ok(());
                        }
                    }
                    let read = io.read(&mut read_chunk).await?;
                    if read == 0 {
                        write_rejection(
                            io,
                            &mut wire_response,
                            400,
                            "incomplete_body",
                            "chunked request body ended before its final chunk",
                        )
                        .await?;
                        return Ok(());
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
            method: parsed.method,
            target,
            buffer: &buffer,
            headers: &parsed.headers,
            body,
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
        let send_body = parsed.method != HttpMethod::Head
            && !matches!(response.status(), 204 | 304)
            && !(parsed.method == HttpMethod::Connect && (200..300).contains(&response.status()));
        let send_content_length = response.status() != 204
            && !(parsed.method == HttpMethod::Connect && (200..300).contains(&response.status()));
        write_response(
            io,
            &mut wire_response,
            &mut response,
            keep_alive,
            send_body,
            send_content_length,
        )
        .await?;
        consume_prefix(&mut buffer, request_bytes);

        if !keep_alive {
            return Ok(());
        }
    }
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

struct ParsedHead {
    method: HttpMethod,
    target: ByteRange,
    headers: HeaderPositions,
    head_bytes: usize,
    body: BodyFraming,
    keep_alive: bool,
}

#[derive(Clone, Copy)]
struct ByteRange {
    start: usize,
    end: usize,
}

#[derive(Clone, Copy)]
struct HeaderPosition {
    name: ByteRange,
    value: ByteRange,
}

const INLINE_REQUEST_HEADERS: usize = 16;

struct HeaderPositions {
    inline: [Option<HeaderPosition>; INLINE_REQUEST_HEADERS],
    overflow: Vec<HeaderPosition>,
}

impl HeaderPositions {
    fn new() -> Self {
        Self {
            inline: [None; INLINE_REQUEST_HEADERS],
            overflow: Vec::new(),
        }
    }

    fn push(&mut self, header: HeaderPosition) {
        if let Some(slot) = self.inline.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(header);
        } else {
            self.overflow.push(header);
        }
    }

    fn iter(&self) -> impl Iterator<Item = HeaderPosition> + '_ {
        self.inline
            .iter()
            .filter_map(|header| *header)
            .chain(self.overflow.iter().copied())
    }
}

#[derive(Clone, Copy)]
enum BodyFraming {
    ContentLength(usize),
    Chunked,
}

struct DecodedChunkedBody {
    consumed: usize,
    body: Vec<u8>,
}

struct ChunkDecoder {
    position: usize,
    pending_size: Option<usize>,
    body: Vec<u8>,
    max_body_bytes: usize,
    max_header_bytes: usize,
    max_chunks: usize,
    chunks: usize,
}

impl ChunkDecoder {
    fn new(position: usize, limits: ServerLimits) -> Self {
        Self {
            position,
            pending_size: None,
            body: Vec::new(),
            max_body_bytes: limits.max_body_bytes,
            max_header_bytes: limits.max_header_bytes,
            max_chunks: limits.max_chunks,
            chunks: 0,
        }
    }

    fn advance(&mut self, buffer: &[u8]) -> Result<Option<DecodedChunkedBody>, Rejection> {
        loop {
            if let Some(size) = self.pending_size {
                let end = self
                    .position
                    .checked_add(size)
                    .ok_or_else(Rejection::bad_request)?;
                let chunk_end = end.checked_add(2).ok_or_else(Rejection::bad_request)?;
                if buffer.len() < chunk_end {
                    return Ok(None);
                }
                if buffer.get(end..chunk_end) != Some(b"\r\n") {
                    return Err(Rejection::bad_request());
                }
                self.body.extend_from_slice(&buffer[self.position..end]);
                self.position = chunk_end;
                self.pending_size = None;
                continue;
            }

            let Some(line_end) = find_bytes(buffer, b"\r\n", self.position) else {
                if buffer.len().saturating_sub(self.position) > self.max_header_bytes {
                    return Err(Rejection::bad_request());
                }
                return Ok(None);
            };
            let size_line = std::str::from_utf8(&buffer[self.position..line_end])
                .map_err(|_| Rejection::bad_request())?;
            let size = size_line
                .split(';')
                .next()
                .map(str::trim)
                .filter(|size| !size.is_empty())
                .and_then(|size| usize::from_str_radix(size, 16).ok())
                .ok_or_else(Rejection::bad_request)?;
            self.position = line_end + 2;

            if size == 0 {
                let consumed = if buffer.get(self.position..self.position + 2) == Some(b"\r\n") {
                    self.position + 2
                } else {
                    let Some(trailer_end) = find_bytes(buffer, b"\r\n\r\n", self.position) else {
                        if buffer.len().saturating_sub(self.position) > self.max_header_bytes {
                            return Err(Rejection::headers_too_large());
                        }
                        return Ok(None);
                    };
                    if trailer_end - self.position > self.max_header_bytes {
                        return Err(Rejection::headers_too_large());
                    }
                    validate_trailers(&buffer[self.position..trailer_end])?;
                    trailer_end + 4
                };
                return Ok(Some(DecodedChunkedBody {
                    consumed,
                    body: std::mem::take(&mut self.body),
                }));
            }

            if size > self.max_body_bytes.saturating_sub(self.body.len()) {
                return Err(Rejection::payload_too_large());
            }
            self.chunks += 1;
            if self.chunks > self.max_chunks {
                return Err(Rejection {
                    status: 413,
                    code: "too_many_chunks",
                    message: "chunk count exceeds the configured limit",
                });
            }
            self.pending_size = Some(size);
        }
    }
}

fn validate_trailers(trailers: &[u8]) -> Result<(), Rejection> {
    let trailers = std::str::from_utf8(trailers).map_err(|_| Rejection::bad_request())?;
    for line in trailers.split("\r\n") {
        let (name, value) = line.split_once(':').ok_or_else(Rejection::bad_request)?;
        if name.is_empty()
            || !name.bytes().all(is_header_name_byte)
            || value
                .bytes()
                .any(|byte| byte != b'\t' && (byte < b' ' || byte == 127))
        {
            return Err(Rejection::bad_request());
        }
    }
    Ok(())
}

fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| from + position)
}

const fn is_header_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn parse_head(buffer: &[u8], limits: ServerLimits) -> Result<Option<ParsedHead>, Rejection> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADER_CAPACITY];
    let mut request = httparse::Request::new(&mut headers[..limits.max_headers]);
    let status = request.parse(buffer).map_err(|error| match error {
        httparse::Error::TooManyHeaders => Rejection::headers_too_large(),
        _ => Rejection::bad_request(),
    })?;
    let httparse::Status::Complete(head_bytes) = status else {
        if buffer.len() > limits.max_header_bytes {
            return Err(Rejection::headers_too_large());
        }
        return Ok(None);
    };
    if head_bytes > limits.max_header_bytes {
        return Err(Rejection::headers_too_large());
    }
    let method = parse_method(request.method.ok_or_else(Rejection::bad_request)?)?;
    let target = byte_range(
        buffer,
        request.path.ok_or_else(Rejection::bad_request)?.as_bytes(),
    )?;
    let version = request.version.ok_or_else(Rejection::bad_request)?;

    let mut content_length = None;
    let mut connection_close = false;
    let mut connection_keep_alive = false;
    let mut transfer_encodings = Vec::new();
    let mut parsed_headers = HeaderPositions::new();
    for header in request.headers.iter() {
        parsed_headers.push(HeaderPosition {
            name: byte_range(buffer, header.name.as_bytes())?,
            value: byte_range(buffer, header.value)?,
        });
        let value = std::str::from_utf8(header.value).map_err(|_| Rejection::bad_request())?;
        if header.name.eq_ignore_ascii_case("content-length") {
            let length = value
                .trim()
                .parse::<usize>()
                .map_err(|_| Rejection::bad_request())?;
            if content_length.is_some_and(|previous| previous != length) {
                return Err(Rejection::bad_request());
            }
            content_length = Some(length);
        } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
            transfer_encodings.extend(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|token| !token.is_empty())
                    .map(str::to_ascii_lowercase),
            );
        } else if header.name.eq_ignore_ascii_case("connection") {
            for token in value.split(',').map(str::trim) {
                connection_close |= token.eq_ignore_ascii_case("close");
                connection_keep_alive |= token.eq_ignore_ascii_case("keep-alive");
            }
        }
    }

    if !transfer_encodings.is_empty() && content_length.is_some() {
        return Err(Rejection::bad_request());
    }
    let body = if transfer_encodings.is_empty() {
        BodyFraming::ContentLength(content_length.unwrap_or(0))
    } else if transfer_encodings.len() == 1 && transfer_encodings[0] == "chunked" {
        BodyFraming::Chunked
    } else {
        return Err(Rejection {
            status: 501,
            code: "unsupported_transfer_encoding",
            message: "only chunked transfer encoding is supported",
        });
    };

    Ok(Some(ParsedHead {
        method,
        target,
        headers: parsed_headers,
        head_bytes,
        body,
        keep_alive: if version == 1 {
            !connection_close
        } else {
            connection_keep_alive && !connection_close
        },
    }))
}

fn byte_range(buffer: &[u8], slice: &[u8]) -> Result<ByteRange, Rejection> {
    let start = (slice.as_ptr() as usize)
        .checked_sub(buffer.as_ptr() as usize)
        .ok_or_else(Rejection::bad_request)?;
    let end = start
        .checked_add(slice.len())
        .filter(|end| *end <= buffer.len())
        .ok_or_else(Rejection::bad_request)?;
    Ok(ByteRange { start, end })
}

fn parse_method(method: &str) -> Result<HttpMethod, Rejection> {
    match method {
        "GET" => Ok(HttpMethod::Get),
        "HEAD" => Ok(HttpMethod::Head),
        "POST" => Ok(HttpMethod::Post),
        "PUT" => Ok(HttpMethod::Put),
        "PATCH" => Ok(HttpMethod::Patch),
        "DELETE" => Ok(HttpMethod::Delete),
        "OPTIONS" => Ok(HttpMethod::Options),
        "TRACE" => Ok(HttpMethod::Trace),
        "CONNECT" => Ok(HttpMethod::Connect),
        _ => Err(Rejection {
            status: 405,
            code: "method_not_allowed",
            message: "HTTP method is not supported by this build",
        }),
    }
}

struct NativeRequest<'request> {
    method: HttpMethod,
    target: &'request str,
    buffer: &'request [u8],
    headers: &'request HeaderPositions,
    body: &'request [u8],
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

async fn write_response<IO>(
    io: &mut IO,
    wire: &mut Vec<u8>,
    response: &mut Response,
    keep_alive: bool,
    send_body: bool,
    send_content_length: bool,
) -> io::Result<()>
where
    IO: AsyncWrite + Unpin,
{
    wire.clear();
    let streaming = response.is_streaming();
    let exact_body_length = response.exact_body_length();
    let chunked = streaming && send_body && send_content_length && exact_body_length.is_none();
    write!(
        wire,
        "HTTP/1.1 {} {}\r\n",
        response.status(),
        reason_phrase(response.status())
    )?;
    let mut has_date = false;
    for (name, value) in response.headers() {
        if name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        has_date |= name.eq_ignore_ascii_case("date");
        write!(wire, "{name}: {value}\r\n")?;
    }
    if send_content_length {
        if let Some(length) = exact_body_length {
            write!(wire, "content-length: {length}\r\n")?;
        } else if chunked {
            wire.extend_from_slice(b"transfer-encoding: chunked\r\n");
        }
    }
    if !has_date {
        wire.extend_from_slice(b"date: ");
        with_cached_date(|date| wire.extend_from_slice(date.as_bytes()));
        wire.extend_from_slice(b"\r\n");
    }
    if !keep_alive {
        wire.extend_from_slice(b"connection: close\r\n");
    }
    wire.extend_from_slice(b"\r\n");
    if send_body && !streaming {
        wire.extend_from_slice(response.body());
    }
    io.write_all(wire).await?;

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
                write!(wire, "{:X}\r\n", chunk.len())?;
                wire.extend_from_slice(&chunk);
                wire.extend_from_slice(b"\r\n");
                io.write_all(wire).await?;
            } else {
                io.write_all(&chunk).await?;
            }
        }
        if exact_body_length.is_some_and(|expected| written != expected) {
            return Err(io::Error::other(
                "streaming response did not match its declared exact length",
            ));
        }
        if chunked {
            io.write_all(b"0\r\n\r\n").await?;
        }
    }
    io.flush().await
}

async fn write_rejection<IO>(
    io: &mut IO,
    wire: &mut Vec<u8>,
    status: u16,
    code: &str,
    message: &str,
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
    io.write_all(wire).await?;
    io.flush().await
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

const fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        499 => "Client Closed Request",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

#[derive(Clone, Copy)]
struct Rejection {
    status: u16,
    code: &'static str,
    message: &'static str,
}

impl Rejection {
    const fn bad_request() -> Self {
        Self {
            status: 400,
            code: "bad_request",
            message: "invalid HTTP/1 request",
        }
    }

    const fn headers_too_large() -> Self {
        Self {
            status: 431,
            code: "request_header_too_large",
            message: "request headers exceed the configured limit",
        }
    }

    const fn payload_too_large() -> Self {
        Self {
            status: 413,
            code: "payload_too_large",
            message: "request body exceeds the configured limit",
        }
    }
}

impl fmt::Debug for Server {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Server")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}
