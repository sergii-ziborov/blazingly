#![forbid(unsafe_code)]

use base64::Engine;
use blazingly_core::{
    Accepted, ApiError, ApiModel, ApiSchema, App, AppDefinition, Background, BackgroundTask,
    BodyStreamError, Cookie, Created, File, Form, Header, HttpUpgrade, InputSource, Json,
    Multipart, NoContent, OperationDescriptor, OperationFailure, OperationId, Path, PreparedJson,
    Query, ResponseBuildError, ResponseHeader, SchemaKind, SecuritySchemeDescriptor, Status,
    StreamingBody, TypeDescriptor, UploadFile, WithHeaders,
};
pub use blazingly_di::DependencyError;
use blazingly_di::{
    CompiledProvider, DependencyLifetime, DependencyRequest, DependencySlot, DependencyValue,
    Depends, Provider,
};
use blazingly_json::{Value, json};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Poll, Waker};

pub type OperationFuture = Pin<Box<dyn Future<Output = ExecutionOutcome> + 'static>>;
const INLINE_DEPENDENCY_SLOTS: usize = 8;
type AsyncHandler = Rc<
    dyn for<'input, 'dependencies> Fn(
            InvocationInput<'input>,
            &'dependencies ResolvedDependencies<'dependencies>,
        ) -> Result<OperationFuture, ExecutionOutcome>
        + 'static,
>;
type SyncHandler = Rc<
    dyn for<'input, 'dependencies> Fn(
            InvocationInput<'input>,
            &'dependencies ResolvedDependencies<'dependencies>,
        ) -> Result<ExecutionOutcome, ExecutionOutcome>
        + 'static,
>;
enum Handler {
    Async(AsyncHandler),
    Sync {
        direct: SyncHandler,
        fallback: AsyncHandler,
    },
}

impl Handler {
    fn prepare(
        &self,
        input: InvocationInput<'_>,
        dependencies: &ResolvedDependencies<'_>,
    ) -> Result<OperationFuture, ExecutionOutcome> {
        match self {
            Self::Async(handler) => handler(input, dependencies),
            Self::Sync { fallback, .. } => fallback(input, dependencies),
        }
    }

    fn invoke_sync(
        &self,
        input: InvocationInput<'_>,
        dependencies: &ResolvedDependencies<'_>,
    ) -> Option<Result<ExecutionOutcome, ExecutionOutcome>> {
        match self {
            Self::Async(_) => None,
            Self::Sync { direct, .. } => Some(direct(input, dependencies)),
        }
    }
}

type SingletonCompilation = (Vec<Option<DependencyValue>>, Vec<Option<usize>>);
type OperationHookFuture = Pin<Box<dyn Future<Output = Result<(), DependencyError>> + 'static>>;
type ResponseHookFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;
type OperationHook = Rc<dyn Fn(HookContext) -> OperationHookFuture>;
type ResponseHook = Rc<dyn Fn(HookContext, HookOutcome) -> ResponseHookFuture>;
type LifecycleHook = Rc<dyn Fn() -> OperationHookFuture>;
type AbortFuture = Pin<Box<dyn Future<Output = InvocationAbort> + 'static>>;
type BlockingJob = Box<dyn FnOnce() + Send + 'static>;

static GLOBAL_BLOCKING_POOL: OnceLock<BlockingPool> = OnceLock::new();

/// Capacity and worker count for synchronous blocking handlers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockingPoolConfig {
    workers: NonZeroUsize,
    queue_capacity: NonZeroUsize,
}

impl BlockingPoolConfig {
    #[must_use]
    pub const fn new(workers: NonZeroUsize, queue_capacity: NonZeroUsize) -> Self {
        Self {
            workers,
            queue_capacity,
        }
    }

    #[must_use]
    pub const fn workers(self) -> NonZeroUsize {
        self.workers
    }

    #[must_use]
    pub const fn queue_capacity(self) -> NonZeroUsize {
        self.queue_capacity
    }
}

impl Default for BlockingPoolConfig {
    fn default() -> Self {
        let workers = std::thread::available_parallelism()
            .unwrap_or(NonZeroUsize::MIN)
            .get()
            .max(2);
        Self {
            workers: NonZeroUsize::new(workers).expect("worker count is non-zero"),
            queue_capacity: NonZeroUsize::new(1024).expect("queue capacity is non-zero"),
        }
    }
}

/// A bounded process-wide pool used only by explicitly synchronous handlers.
#[derive(Clone)]
pub struct BlockingPool {
    sender: SyncSender<BlockingJob>,
}

impl BlockingPool {
    /// Starts a bounded worker pool.
    ///
    /// # Errors
    ///
    /// Returns an OS error when a worker thread cannot be started.
    pub fn new(config: BlockingPoolConfig) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<BlockingJob>(config.queue_capacity.get());
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..config.workers.get() {
            let receiver = Arc::clone(&receiver);
            std::thread::Builder::new()
                .name(format!("blazingly-blocking-{index}"))
                .spawn(move || {
                    loop {
                        let job = {
                            let Ok(receiver) = receiver.lock() else {
                                return;
                            };
                            receiver.recv()
                        };
                        let Ok(job) = job else {
                            return;
                        };
                        job();
                    }
                })?;
        }
        Ok(Self { sender })
    }

    fn submit(&self, job: BlockingJob) -> Result<(), BlockingError> {
        self.sender.try_send(job).map_err(|error| match error {
            TrySendError::Full(_) => BlockingError::Saturated,
            TrySendError::Disconnected(_) => BlockingError::Unavailable,
        })
    }
}

impl fmt::Debug for BlockingPool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BlockingPool")
            .finish_non_exhaustive()
    }
}

/// Installs the process-wide blocking pool before the first sync invocation.
///
/// # Errors
///
/// Returns [`BlockingError::AlreadyConfigured`] after a pool has already been
/// installed or initialized.
pub fn install_global_blocking_pool(config: BlockingPoolConfig) -> Result<(), BlockingError> {
    let pool = BlockingPool::new(config).map_err(|_| BlockingError::Unavailable)?;
    GLOBAL_BLOCKING_POOL
        .set(pool)
        .map_err(|_| BlockingError::AlreadyConfigured)
}

/// Failure to schedule or execute a synchronous blocking handler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockingError {
    Saturated,
    Unavailable,
    Panicked,
    AlreadyConfigured,
}

impl fmt::Display for BlockingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Saturated => "blocking handler queue is saturated",
            Self::Unavailable => "blocking handler pool is unavailable",
            Self::Panicked => "blocking handler panicked",
            Self::AlreadyConfigured => "blocking handler pool is already configured",
        })
    }
}

impl std::error::Error for BlockingError {}

struct BlockingState<T> {
    result: Option<Result<T, BlockingError>>,
    waker: Option<Waker>,
}

/// Future resolved by a bounded blocking worker.
pub struct BlockingFuture<T> {
    state: Arc<Mutex<BlockingState<T>>>,
}

impl<T> Future for BlockingFuture<T> {
    type Output = Result<T, BlockingError>;

    fn poll(self: Pin<&mut Self>, context: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(result) = state.result.take() {
            return Poll::Ready(result);
        }
        state.waker = Some(context.waker().clone());
        Poll::Pending
    }
}

/// Schedules owned synchronous work without blocking an async worker.
#[must_use]
pub fn run_blocking<Task, Output>(task: Task) -> BlockingFuture<Output>
where
    Task: FnOnce() -> Output + Send + 'static,
    Output: Send + 'static,
{
    let state = Arc::new(Mutex::new(BlockingState {
        result: None,
        waker: None,
    }));
    let future = BlockingFuture {
        state: Arc::clone(&state),
    };
    let pool = match global_blocking_pool() {
        Ok(pool) => pool,
        Err(error) => {
            complete_blocking(&state, Err(error));
            return future;
        }
    };
    let job_state = Arc::clone(&state);
    let job = Box::new(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task))
            .map_err(|_| BlockingError::Panicked);
        complete_blocking(&job_state, result);
    });
    if let Err(error) = pool.submit(job) {
        complete_blocking(&state, Err(error));
    }
    future
}

fn global_blocking_pool() -> Result<&'static BlockingPool, BlockingError> {
    if let Some(pool) = GLOBAL_BLOCKING_POOL.get() {
        return Ok(pool);
    }
    let pool =
        BlockingPool::new(BlockingPoolConfig::default()).map_err(|_| BlockingError::Unavailable)?;
    let _ = GLOBAL_BLOCKING_POOL.set(pool);
    GLOBAL_BLOCKING_POOL.get().ok_or(BlockingError::Unavailable)
}

fn complete_blocking<T>(state: &Arc<Mutex<BlockingState<T>>>, result: Result<T, BlockingError>) {
    let waker = {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.result = Some(result);
        state.waker.take()
    };
    if let Some(waker) = waker {
        waker.wake();
    }
}

#[must_use]
pub fn blocking_error_outcome(error: BlockingError) -> ExecutionOutcome {
    match error {
        BlockingError::Saturated => ExecutionOutcome::Rejected {
            status: 503,
            code: "blocking_pool_saturated".to_owned(),
            message: error.to_string(),
            details: None,
        },
        BlockingError::Unavailable | BlockingError::Panicked | BlockingError::AlreadyConfigured => {
            ExecutionOutcome::InternalError {
                code: "blocking_handler_failed".to_owned(),
                message: error.to_string(),
            }
        }
    }
}

struct CancellationState {
    cancelled: AtomicBool,
    wakers: Mutex<Vec<Waker>>,
}

/// Runtime-neutral cooperative cancellation shared by adapters and operation
/// execution.
#[derive(Clone)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

impl CancellationToken {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(CancellationState {
                cancelled: AtomicBool::new(false),
                wakers: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Marks the token cancelled and wakes every controlled invocation.
    pub fn cancel(&self) {
        if self.state.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut wakers = self
            .state
            .wakers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for waker in wakers.drain(..) {
            waker.wake();
        }
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn cancelled(&self) -> Cancelled {
        Cancelled {
            token: self.clone(),
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Future completed when a [`CancellationToken`] is cancelled.
pub struct Cancelled {
    token: CancellationToken,
}

impl Future for Cancelled {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if self.token.is_cancelled() {
            return Poll::Ready(());
        }
        let mut wakers = self
            .token
            .state
            .wakers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.token.is_cancelled() {
            return Poll::Ready(());
        }
        if !wakers.iter().any(|waker| waker.will_wake(context.waker())) {
            wakers.push(context.waker().clone());
        }
        Poll::Pending
    }
}

/// Reason a controlled invocation stopped before completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvocationAbort {
    Cancelled,
    TimedOut,
}

impl InvocationAbort {
    fn into_execution_outcome(self) -> ExecutionOutcome {
        match self {
            Self::Cancelled => ExecutionOutcome::Rejected {
                status: 499,
                code: "invocation_cancelled".to_owned(),
                message: "operation invocation was cancelled".to_owned(),
                details: None,
            },
            Self::TimedOut => ExecutionOutcome::Rejected {
                status: 504,
                code: "invocation_timeout".to_owned(),
                message: "operation invocation exceeded its time limit".to_owned(),
                details: None,
            },
        }
    }
}

/// Adapter-supplied cancellation and timeout signals for one invocation.
///
/// A timeout is represented as a future so native, Cloudflare, tests, and
/// other adapters can use their own clock/runtime without leaking it into the
/// operation graph.
#[derive(Default)]
pub struct InvocationControl {
    signals: Vec<AbortFuture>,
}

impl InvocationControl {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            signals: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.signals.push(Box::pin(async move {
            token.cancelled().await;
            InvocationAbort::Cancelled
        }));
        self
    }

    #[must_use]
    pub fn with_timeout<Timeout>(mut self, timeout: Timeout) -> Self
    where
        Timeout: Future<Output = ()> + 'static,
    {
        self.signals.push(Box::pin(async move {
            timeout.await;
            InvocationAbort::TimedOut
        }));
        self
    }

    async fn run<Output>(
        &mut self,
        future: impl Future<Output = Output>,
    ) -> Result<Output, InvocationAbort> {
        let mut future = Box::pin(future);
        std::future::poll_fn(|context| {
            for signal in &mut self.signals {
                if let Poll::Ready(abort) = signal.as_mut().poll(context) {
                    return Poll::Ready(Err(abort));
                }
            }
            future.as_mut().poll(context).map(Ok)
        })
        .await
    }
}

/// Runtime-neutral metadata passed to compiled plugin hooks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookContext {
    operation_id: Rc<str>,
}

impl HookContext {
    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
}

/// A body-free result summary passed to `on_response` hooks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HookOutcome {
    pub status: u16,
    pub kind: HookOutcomeKind,
}

/// Stable result classes visible to plugin response hooks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookOutcomeKind {
    Success,
    Rejected,
    DomainError,
    InternalError,
}

impl From<&ExecutionOutcome> for HookOutcome {
    fn from(outcome: &ExecutionOutcome) -> Self {
        match outcome {
            ExecutionOutcome::Success { status, .. }
            | ExecutionOutcome::StreamingSuccess { status, .. } => Self {
                status: *status,
                kind: HookOutcomeKind::Success,
            },
            ExecutionOutcome::Upgrade { .. } => Self {
                status: 101,
                kind: HookOutcomeKind::Success,
            },
            ExecutionOutcome::Rejected { status, .. } => Self {
                status: *status,
                kind: HookOutcomeKind::Rejected,
            },
            ExecutionOutcome::DomainError(failure) => Self {
                status: failure.status,
                kind: HookOutcomeKind::DomainError,
            },
            ExecutionOutcome::InternalError { .. } => Self {
                status: 500,
                kind: HookOutcomeKind::InternalError,
            },
        }
    }
}

/// Borrowed HTTP request values used by the compiled executor.
///
/// Production adapters implement this view over their native request without
/// copying headers, query strings, or request bodies.
pub trait HttpRequestParts {
    fn value(&self, source: InputSource, name: &str, index: usize) -> Option<Cow<'_, str>>;
    fn body(&self) -> &[u8];

    /// Transfers an adapter-owned pull request body to a streaming extractor.
    fn take_body_stream(&self) -> Option<StreamingBody> {
        None
    }

    /// Returns transport context installed by HTTP middleware.
    ///
    /// The default keeps non-HTTP transports and existing adapters allocation
    /// free. HTTP adapters only allocate extension storage when middleware
    /// actually installs a value.
    fn extension(&self, _type_id: std::any::TypeId) -> Option<&dyn std::any::Any> {
        None
    }
}

/// Pull-based request body with adapter-enforced transport limits.
///
/// Each `next_chunk` call is the backpressure boundary. The native adapter
/// does not read and queue another network chunk until the handler pulls.
#[derive(Debug)]
pub struct UploadBody {
    stream: StreamingBody,
    bytes_read: u64,
}

impl UploadBody {
    #[must_use]
    pub fn new(stream: StreamingBody) -> Self {
        Self {
            stream,
            bytes_read: 0,
        }
    }

    #[must_use]
    pub const fn exact_length(&self) -> Option<u64> {
        self.stream.exact_length()
    }

    #[must_use]
    pub const fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Pulls the next upload chunk.
    pub async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, BodyStreamError>> {
        let chunk = self.stream.next_chunk().await;
        if let Some(Ok(bytes)) = &chunk {
            self.bytes_read = self
                .bytes_read
                .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        }
        chunk
    }

    /// Deliberately buffers the remaining upload with an explicit limit.
    ///
    /// # Errors
    ///
    /// Returns a producer error or `upload_collect_limit_exceeded`.
    pub async fn collect(mut self, limit: usize) -> Result<Vec<u8>, BodyStreamError> {
        let mut body = Vec::new();
        while let Some(chunk) = self.next_chunk().await {
            let chunk = chunk?;
            if body.len().saturating_add(chunk.len()) > limit {
                return Err(BodyStreamError::new(
                    "upload_collect_limit_exceeded",
                    format!("streaming upload exceeds the {limit}-byte collection limit"),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

impl ApiSchema for UploadBody {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::scalar("UploadBody", SchemaKind::Binary)
    }
}

impl FromInvocation for UploadBody {
    fn from_invocation(
        input: &InvocationInput<'_>,
        _name: &str,
        _required: bool,
    ) -> Result<Self, InputRejection> {
        let InvocationInput::Http(request) = input else {
            return Err(InputRejection::new(
                400,
                "streaming_input_requires_http",
                "streaming request bodies are available only through HTTP",
            ));
        };
        let stream = request
            .take_body_stream()
            .unwrap_or_else(|| StreamingBody::once(request.body().to_vec()));
        Ok(Self::new(stream))
    }
}

/// Transport-neutral values supplied to typed operation extractors.
#[derive(Clone, Copy)]
pub enum InvocationInput<'input> {
    Http(&'input dyn HttpRequestParts),
    Arguments(&'input Value),
}

impl<'input> InvocationInput<'input> {
    #[must_use]
    pub const fn http(request: &'input dyn HttpRequestParts) -> Self {
        Self::Http(request)
    }

    #[must_use]
    pub const fn arguments(arguments: &'input Value) -> Self {
        Self::Arguments(arguments)
    }
}

/// Slot-based dependency values visible to one operation handler.
///
/// Slots are compiled when the application is built. Calling [`Self::get`]
/// performs one bounds check and one safe type check; it never looks up a type
/// name or hashes a key on the request path.
pub struct ResolvedDependencies<'values> {
    singletons: &'values [Option<DependencyValue>],
    requests: &'values [Option<DependencyValue>],
    slots: &'values [DependencySlot],
}

impl ResolvedDependencies<'_> {
    /// Reads a typed dependency from the handler's compiled argument slot.
    ///
    /// # Errors
    ///
    /// Returns an internal dependency error if generated metadata and the
    /// compiled plan disagree.
    pub fn get<T: 'static>(&self, index: usize) -> Result<Depends<T>, DependencyError> {
        let slot = self.slots.get(index).copied().ok_or_else(|| {
            DependencyError::internal(
                "invalid_dependency_argument",
                "handler requested an unknown compiled dependency argument",
            )
        })?;
        resolve_dependency(slot, self.singletons, self.requests)
    }

    /// Clones a dependency handle directly into a handler argument.
    ///
    /// # Errors
    ///
    /// Returns an internal dependency error if generated metadata and the
    /// compiled plan disagree.
    pub fn get_cloned<T: Clone + 'static>(&self, index: usize) -> Result<T, DependencyError> {
        self.get::<T>(index).map(|dependency| (*dependency).clone())
    }
}

/// A stable client-visible failure produced while extracting an argument.
#[derive(Clone, Debug, PartialEq)]
pub struct InputRejection {
    status: u16,
    code: String,
    message: String,
    details: Option<Value>,
}

impl InputRejection {
    #[must_use]
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    #[must_use]
    pub fn into_execution_outcome(self) -> ExecutionOutcome {
        ExecutionOutcome::Rejected {
            status: self.status,
            code: self.code,
            message: self.message,
            details: self.details,
        }
    }
}

/// Decodes one typed handler argument from an invocation.
pub trait FromInvocation: Sized {
    /// Extracts one handler argument.
    ///
    /// # Errors
    ///
    /// Returns a stable rejection when the value is missing, cannot be
    /// decoded, or fails model validation.
    fn from_invocation(
        input: &InvocationInput<'_>,
        name: &str,
        required: bool,
    ) -> Result<Self, InputRejection>;
}

/// Typed request-local value installed by transport middleware.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Extension<T>(pub T);

impl<T> FromInvocation for Extension<T>
where
    T: Clone + 'static,
{
    fn from_invocation(
        input: &InvocationInput<'_>,
        name: &str,
        _required: bool,
    ) -> Result<Self, InputRejection> {
        let InvocationInput::Http(request) = input else {
            return Err(InputRejection::new(
                500,
                "extension_transport_mismatch",
                "request extension is unavailable on this transport",
            ));
        };
        request
            .extension(std::any::TypeId::of::<T>())
            .and_then(|value| value.downcast_ref::<T>())
            .cloned()
            .map(Self)
            .ok_or_else(|| {
                InputRejection::new(
                    500,
                    "missing_request_extension",
                    format!("request middleware did not install extension `{name}`"),
                )
            })
    }
}

impl<T> FromInvocation for Json<T>
where
    T: ApiSchema + DeserializeOwned,
{
    fn from_invocation(
        input: &InvocationInput<'_>,
        name: &str,
        required: bool,
    ) -> Result<Self, InputRejection> {
        extract_argument(input, name, InputSource::Json, required).map(Self)
    }
}

impl<T> FromInvocation for Path<T>
where
    T: ApiSchema + DeserializeOwned,
{
    fn from_invocation(
        input: &InvocationInput<'_>,
        name: &str,
        required: bool,
    ) -> Result<Self, InputRejection> {
        extract_argument(input, name, InputSource::Path, required).map(Self)
    }
}

impl<T> FromInvocation for Query<T>
where
    T: ApiSchema + DeserializeOwned,
{
    fn from_invocation(
        input: &InvocationInput<'_>,
        name: &str,
        required: bool,
    ) -> Result<Self, InputRejection> {
        extract_argument(input, name, InputSource::Query, required).map(Self)
    }
}

impl<T> FromInvocation for Header<T>
where
    T: ApiSchema + DeserializeOwned,
{
    fn from_invocation(
        input: &InvocationInput,
        name: &str,
        required: bool,
    ) -> Result<Self, InputRejection> {
        extract_argument(input, name, InputSource::Header, required).map(Self)
    }
}

impl<T> FromInvocation for Cookie<T>
where
    T: ApiSchema + DeserializeOwned,
{
    fn from_invocation(
        input: &InvocationInput,
        name: &str,
        required: bool,
    ) -> Result<Self, InputRejection> {
        extract_argument(input, name, InputSource::Cookie, required).map(Self)
    }
}

impl<T> FromInvocation for Form<T>
where
    T: ApiSchema + DeserializeOwned,
{
    fn from_invocation(
        input: &InvocationInput,
        name: &str,
        required: bool,
    ) -> Result<Self, InputRejection> {
        extract_argument(input, name, InputSource::Form, required).map(Self)
    }
}

impl<T> FromInvocation for Multipart<T>
where
    T: ApiSchema + DeserializeOwned,
{
    fn from_invocation(
        input: &InvocationInput,
        name: &str,
        required: bool,
    ) -> Result<Self, InputRejection> {
        let decoded = match input {
            InvocationInput::Http(request) => {
                let descriptor = cached_type_descriptor::<T>();
                let parts = parse_multipart_request(*request)?;
                let value = multipart_argument_value(&parts, name, required, &descriptor)?;
                blazingly_json::from_value(value).map_err(|error| {
                    decode_rejection(name, InputSource::Multipart, &error.to_string())
                })?
            }
            InvocationInput::Arguments(_) => {
                return extract_argument(input, name, InputSource::Multipart, required).map(Self);
            }
        };
        validate_decoded(decoded, InputSource::Multipart).map(Self)
    }
}

/// Types accepted by the typed [`File`] extractor.
pub trait FilePayload: Sized {
    #[doc(hidden)]
    fn from_uploads(uploads: Vec<UploadFile>, required: bool) -> Result<Self, InputRejection>;
}

impl FilePayload for UploadFile {
    fn from_uploads(mut uploads: Vec<UploadFile>, required: bool) -> Result<Self, InputRejection> {
        if uploads.len() == 1 {
            return Ok(uploads.remove(0));
        }
        Err(file_count_rejection(required, uploads.len(), "exactly one"))
    }
}

impl FilePayload for Option<UploadFile> {
    fn from_uploads(mut uploads: Vec<UploadFile>, required: bool) -> Result<Self, InputRejection> {
        match uploads.len() {
            0 if !required => Ok(None),
            1 => Ok(Some(uploads.remove(0))),
            count => Err(file_count_rejection(required, count, "zero or one")),
        }
    }
}

impl FilePayload for Vec<UploadFile> {
    fn from_uploads(uploads: Vec<UploadFile>, required: bool) -> Result<Self, InputRejection> {
        if required && uploads.is_empty() {
            Err(file_count_rejection(required, 0, "one or more"))
        } else {
            Ok(uploads)
        }
    }
}

impl<T: FilePayload> FromInvocation for File<T> {
    fn from_invocation(
        input: &InvocationInput,
        name: &str,
        required: bool,
    ) -> Result<Self, InputRejection> {
        let uploads = match input {
            InvocationInput::Http(request) => parse_multipart_request(*request)?
                .into_iter()
                .filter(|part| part.name == name)
                .map(MultipartPart::into_upload)
                .collect::<Vec<_>>(),
            InvocationInput::Arguments(arguments) => upload_arguments(arguments, name, required)?,
        };
        T::from_uploads(uploads, required).map(Self)
    }
}

/// The protocol-neutral result of executing one operation.
#[derive(Debug)]
pub enum ExecutionOutcome {
    Success {
        status: u16,
        headers: Vec<ResponseHeader>,
        body: Option<Vec<u8>>,
        background: Vec<BackgroundTask>,
    },
    StreamingSuccess {
        status: u16,
        headers: Vec<ResponseHeader>,
        body: StreamingBody,
        background: Vec<BackgroundTask>,
    },
    Upgrade {
        upgrade: HttpUpgrade,
        background: Vec<BackgroundTask>,
    },
    Rejected {
        status: u16,
        code: String,
        message: String,
        details: Option<Value>,
    },
    DomainError(OperationFailure),
    InternalError {
        code: String,
        message: String,
    },
}

impl ExecutionOutcome {
    #[must_use]
    pub const fn is_error(&self) -> bool {
        !matches!(
            self,
            Self::Success { .. } | Self::StreamingSuccess { .. } | Self::Upgrade { .. }
        )
    }
}

/// A typed handler result that can become a shared operation outcome.
pub trait OperationOutput {
    fn into_execution_outcome(self) -> ExecutionOutcome;
}

impl<T: Serialize> OperationOutput for Json<T> {
    fn into_execution_outcome(self) -> ExecutionOutcome {
        serialize_success(200, self.0)
    }
}

impl<T: Serialize> OperationOutput for Created<T> {
    fn into_execution_outcome(self) -> ExecutionOutcome {
        serialize_success(201, self.0)
    }
}

impl<T: Serialize> OperationOutput for Accepted<T> {
    fn into_execution_outcome(self) -> ExecutionOutcome {
        serialize_success(202, self.0)
    }
}

impl<T> OperationOutput for PreparedJson<T> {
    fn into_execution_outcome(self) -> ExecutionOutcome {
        ExecutionOutcome::Success {
            status: 200,
            headers: Vec::new(),
            body: Some(self.into_bytes()),
            background: Vec::new(),
        }
    }
}

impl OperationOutput for NoContent {
    fn into_execution_outcome(self) -> ExecutionOutcome {
        ExecutionOutcome::Success {
            status: 204,
            headers: Vec::new(),
            body: None,
            background: Vec::new(),
        }
    }
}

impl OperationOutput for StreamingBody {
    fn into_execution_outcome(self) -> ExecutionOutcome {
        ExecutionOutcome::StreamingSuccess {
            status: 200,
            headers: vec![ResponseHeader::new(
                "content-type",
                "application/octet-stream",
            )],
            body: self,
            background: Vec::new(),
        }
    }
}

impl OperationOutput for HttpUpgrade {
    fn into_execution_outcome(self) -> ExecutionOutcome {
        ExecutionOutcome::Upgrade {
            upgrade: self,
            background: Vec::new(),
        }
    }
}

impl<T: OperationOutput> OperationOutput for Background<T> {
    fn into_execution_outcome(self) -> ExecutionOutcome {
        let (response, tasks) = self.into_parts();
        let mut outcome = response.into_execution_outcome();
        match &mut outcome {
            ExecutionOutcome::Success { background, .. }
            | ExecutionOutcome::StreamingSuccess { background, .. }
            | ExecutionOutcome::Upgrade { background, .. } => background.extend(tasks),
            ExecutionOutcome::Rejected { .. }
            | ExecutionOutcome::DomainError(_)
            | ExecutionOutcome::InternalError { .. } => {}
        }
        outcome
    }
}

impl<const STATUS: u16, T: OperationOutput> OperationOutput for Status<STATUS, T> {
    fn into_execution_outcome(self) -> ExecutionOutcome {
        if !(200..=399).contains(&STATUS) {
            return ExecutionOutcome::InternalError {
                code: "invalid_response_status".to_owned(),
                message: "typed success status must be between 200 and 399".to_owned(),
            };
        }
        let mut outcome = self.0.into_execution_outcome();
        match &mut outcome {
            ExecutionOutcome::Success { status, .. }
            | ExecutionOutcome::StreamingSuccess { status, .. } => *status = STATUS,
            ExecutionOutcome::Upgrade { .. }
            | ExecutionOutcome::Rejected { .. }
            | ExecutionOutcome::DomainError(_)
            | ExecutionOutcome::InternalError { .. } => {}
        }
        outcome
    }
}

impl<T: OperationOutput> OperationOutput for WithHeaders<T> {
    fn into_execution_outcome(self) -> ExecutionOutcome {
        let (response, headers) = self.into_parts();
        if !headers.iter().all(valid_response_header) {
            return ExecutionOutcome::InternalError {
                code: "invalid_response_header".to_owned(),
                message: "operation produced an invalid response header".to_owned(),
            };
        }
        let mut outcome = response.into_execution_outcome();
        match &mut outcome {
            ExecutionOutcome::Success {
                headers: outcome_headers,
                ..
            }
            | ExecutionOutcome::StreamingSuccess {
                headers: outcome_headers,
                ..
            } => outcome_headers.extend(headers),
            ExecutionOutcome::Upgrade { upgrade, .. } => upgrade.extend_headers(headers),
            ExecutionOutcome::DomainError(error) => error.headers.extend(headers),
            ExecutionOutcome::Rejected { .. } | ExecutionOutcome::InternalError { .. } => {}
        }
        outcome
    }
}

impl<S, E> OperationOutput for Result<S, E>
where
    S: OperationOutput,
    E: ApiError,
{
    fn into_execution_outcome(self) -> ExecutionOutcome {
        match self {
            Ok(success) => success.into_execution_outcome(),
            Err(error) => match error.into_failure() {
                Ok(error) if error.headers.iter().all(valid_response_header) => {
                    ExecutionOutcome::DomainError(error)
                }
                Ok(_) => ExecutionOutcome::InternalError {
                    code: "invalid_response_header".to_owned(),
                    message: "operation produced an invalid response header".to_owned(),
                },
                Err(error) => internal_build_error(error),
            },
        }
    }
}

/// A handler plus the operation descriptor shared by HTTP and MCP.
pub struct ExecutableOperation {
    descriptor: OperationDescriptor,
    dependency_requests: Vec<DependencyRequest>,
    dependency_plan: Option<CompiledOperationDependencies>,
    hooks: CompiledHooks,
    handler: Handler,
}

impl ExecutableOperation {
    #[must_use]
    pub fn typed<F>(descriptor: OperationDescriptor, handler: F) -> Self
    where
        F: for<'input> Fn(InvocationInput<'input>) -> Result<OperationFuture, InputRejection>
            + 'static,
    {
        Self {
            descriptor,
            dependency_requests: Vec::new(),
            dependency_plan: Some(CompiledOperationDependencies::empty()),
            hooks: CompiledHooks::empty(),
            handler: Handler::Async(Rc::new(move |input, _| {
                handler(input).map_err(InputRejection::into_execution_outcome)
            })),
        }
    }

    /// Creates an operation whose generated handler uses compiled DI slots.
    #[doc(hidden)]
    #[must_use]
    pub fn typed_with_dependencies<F>(
        descriptor: OperationDescriptor,
        dependency_requests: Vec<DependencyRequest>,
        handler: F,
    ) -> Self
    where
        F: for<'input, 'dependencies> Fn(
                InvocationInput<'input>,
                &'dependencies ResolvedDependencies<'dependencies>,
            ) -> Result<OperationFuture, ExecutionOutcome>
            + 'static,
    {
        let dependency_plan = dependency_requests
            .is_empty()
            .then(CompiledOperationDependencies::empty);
        Self {
            descriptor,
            dependency_requests,
            dependency_plan,
            hooks: CompiledHooks::empty(),
            handler: Handler::Async(Rc::new(handler)),
        }
    }

    /// Creates an operation with an allocation-free synchronous fast path.
    ///
    /// The fallback preserves the complete async hook and cancellation
    /// lifecycle when plugins add hooks around the operation.
    #[doc(hidden)]
    #[must_use]
    pub fn typed_sync_with_dependencies<Direct, Fallback>(
        descriptor: OperationDescriptor,
        dependency_requests: Vec<DependencyRequest>,
        direct: Direct,
        fallback: Fallback,
    ) -> Self
    where
        Direct: for<'input, 'dependencies> Fn(
                InvocationInput<'input>,
                &'dependencies ResolvedDependencies<'dependencies>,
            ) -> Result<ExecutionOutcome, ExecutionOutcome>
            + 'static,
        Fallback: for<'input, 'dependencies> Fn(
                InvocationInput<'input>,
                &'dependencies ResolvedDependencies<'dependencies>,
            ) -> Result<OperationFuture, ExecutionOutcome>
            + 'static,
    {
        let dependency_plan = dependency_requests
            .is_empty()
            .then(CompiledOperationDependencies::empty);
        Self {
            descriptor,
            dependency_requests,
            dependency_plan,
            hooks: CompiledHooks::empty(),
            handler: Handler::Sync {
                direct: Rc::new(direct),
                fallback: Rc::new(fallback),
            },
        }
    }

    #[must_use]
    pub fn json<I, O, F, Fut>(descriptor: OperationDescriptor, handler: F) -> Self
    where
        I: ApiModel + DeserializeOwned + 'static,
        O: OperationOutput + 'static,
        F: Fn(Json<I>) -> Fut + 'static,
        Fut: Future<Output = O> + 'static,
    {
        Self::typed(descriptor, move |input| {
            let input = Json::<I>::from_invocation(&input, "body", true)?;
            let output = handler(input);
            Ok(Box::pin(async move { output.await.into_execution_outcome() }) as OperationFuture)
        })
    }

    #[must_use]
    pub fn empty<O, F, Fut>(descriptor: OperationDescriptor, handler: F) -> Self
    where
        O: OperationOutput + 'static,
        F: Fn() -> Fut + 'static,
        Fut: Future<Output = O> + 'static,
    {
        Self::typed(descriptor, move |_| {
            let output = handler();
            Ok(Box::pin(async move { output.await.into_execution_outcome() }) as OperationFuture)
        })
    }

    #[must_use]
    pub const fn descriptor(&self) -> &OperationDescriptor {
        &self.descriptor
    }

    pub async fn invoke(&self, input: Value) -> ExecutionOutcome {
        self.invoke_input(InvocationInput::arguments(&input)).await
    }

    pub async fn invoke_controlled(
        &self,
        input: Value,
        control: InvocationControl,
    ) -> ExecutionOutcome {
        self.invoke_input_controlled(InvocationInput::arguments(&input), control)
            .await
    }

    pub async fn invoke_http(&self, request: &dyn HttpRequestParts) -> ExecutionOutcome {
        self.invoke_input(InvocationInput::http(request)).await
    }

    pub async fn invoke_http_controlled(
        &self,
        request: &dyn HttpRequestParts,
        control: InvocationControl,
    ) -> ExecutionOutcome {
        self.invoke_input_controlled(InvocationInput::http(request), control)
            .await
    }

    async fn invoke_input(&self, input: InvocationInput<'_>) -> ExecutionOutcome {
        let outcome = self.invoke_pipeline(input).await;
        if self.hooks.is_empty() {
            return outcome;
        }
        self.hooks.on_error(&outcome).await;
        self.hooks.on_response(&outcome).await;
        outcome
    }

    async fn invoke_input_controlled(
        &self,
        input: InvocationInput<'_>,
        mut control: InvocationControl,
    ) -> ExecutionOutcome {
        let outcome = self.invoke_pipeline_controlled(input, &mut control).await;
        // Response hooks and dependency finalizers are cleanup. Once started,
        // they are shielded from the invocation cancellation signal.
        if self.hooks.is_empty() {
            return outcome;
        }
        self.hooks.on_error(&outcome).await;
        self.hooks.on_response(&outcome).await;
        outcome
    }

    async fn invoke_pipeline(&self, input: InvocationInput<'_>) -> ExecutionOutcome {
        if let Err(error) = self.hooks.on_request().await {
            return dependency_error_outcome(error);
        }
        let Some(plan) = self.dependency_plan.as_ref() else {
            return internal_dependency_error(
                "uncompiled_dependency_plan",
                "operation dependency plan was not compiled",
            );
        };
        if self.hooks.is_empty() && plan.request_providers.is_empty() {
            let dependencies = ResolvedDependencies {
                singletons: &plan.singletons,
                requests: &[],
                slots: &plan.handler_slots,
            };
            if let Some(outcome) = self.handler.invoke_sync(input, &dependencies) {
                return outcome.unwrap_or_else(|outcome| outcome);
            }
            return match self.handler.prepare(input, &dependencies) {
                Ok(handler) => handler.await,
                Err(outcome) => outcome,
            };
        }
        let requests = match plan.resolve().await {
            Ok(requests) => requests,
            Err(error) => return dependency_error_outcome(error),
        };
        let dependencies = ResolvedDependencies {
            singletons: &plan.singletons,
            requests: requests.as_slice(),
            slots: &plan.handler_slots,
        };
        if self.hooks.is_empty()
            && let Some(outcome) = self.handler.invoke_sync(input, &dependencies)
        {
            let outcome = outcome.unwrap_or_else(|outcome| outcome);
            if let Err(finalizer_error) = plan.finalize(&requests).await {
                return dependency_error_outcome(finalizer_error);
            }
            return outcome;
        }
        if let Err(error) = self.hooks.pre_parse().await {
            if let Err(finalizer_error) = plan.finalize(&requests).await {
                return dependency_error_outcome(finalizer_error);
            }
            return dependency_error_outcome(error);
        }
        if let Err(error) = self.hooks.pre_validate().await {
            if let Err(finalizer_error) = plan.finalize(&requests).await {
                return dependency_error_outcome(finalizer_error);
            }
            return dependency_error_outcome(error);
        }
        let handler = match self.handler.prepare(input, &dependencies) {
            Ok(handler) => handler,
            Err(outcome) => {
                if let Err(finalizer_error) = plan.finalize(&requests).await {
                    return dependency_error_outcome(finalizer_error);
                }
                return outcome;
            }
        };
        if let Err(error) = self.hooks.pre_handler().await {
            if let Err(finalizer_error) = plan.finalize(&requests).await {
                return dependency_error_outcome(finalizer_error);
            }
            return dependency_error_outcome(error);
        }
        let outcome = handler.await;
        if let Err(error) = self.hooks.pre_serialize().await {
            if let Err(finalizer_error) = plan.finalize(&requests).await {
                return dependency_error_outcome(finalizer_error);
            }
            return dependency_error_outcome(error);
        }
        if let Err(error) = plan.finalize(&requests).await {
            return dependency_error_outcome(error);
        }
        outcome
    }

    async fn invoke_pipeline_controlled(
        &self,
        input: InvocationInput<'_>,
        control: &mut InvocationControl,
    ) -> ExecutionOutcome {
        match control.run(self.hooks.on_request()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return dependency_error_outcome(error),
            Err(abort) => return abort.into_execution_outcome(),
        }
        let Some(plan) = self.dependency_plan.as_ref() else {
            return internal_dependency_error(
                "uncompiled_dependency_plan",
                "operation dependency plan was not compiled",
            );
        };
        let requests = match plan.resolve_controlled(control).await {
            Ok(requests) => requests,
            Err(ControlledInvocationError::Dependency(error)) => {
                return dependency_error_outcome(error);
            }
            Err(ControlledInvocationError::Abort(abort)) => {
                return abort.into_execution_outcome();
            }
        };
        if let Err(outcome) =
            run_controlled_hook(control, plan, &requests, self.hooks.pre_parse()).await
        {
            return outcome;
        }
        if let Err(outcome) =
            run_controlled_hook(control, plan, &requests, self.hooks.pre_validate()).await
        {
            return outcome;
        }
        let dependencies = ResolvedDependencies {
            singletons: &plan.singletons,
            requests: requests.as_slice(),
            slots: &plan.handler_slots,
        };
        let handler = match self.handler.prepare(input, &dependencies) {
            Ok(handler) => handler,
            Err(outcome) => {
                if let Err(finalizer_error) = plan.finalize(&requests).await {
                    return dependency_error_outcome(finalizer_error);
                }
                return outcome;
            }
        };
        if let Err(outcome) =
            run_controlled_hook(control, plan, &requests, self.hooks.pre_handler()).await
        {
            return outcome;
        }
        let outcome = match control.run(handler).await {
            Ok(outcome) => outcome,
            Err(abort) => {
                if let Err(finalizer_error) = plan.finalize(&requests).await {
                    return dependency_error_outcome(finalizer_error);
                }
                return abort.into_execution_outcome();
            }
        };
        if let Err(hook_outcome) =
            run_controlled_hook(control, plan, &requests, self.hooks.pre_serialize()).await
        {
            return hook_outcome;
        }
        if let Err(error) = plan.finalize(&requests).await {
            return dependency_error_outcome(error);
        }
        outcome
    }
}

async fn run_controlled_hook<HookFuture>(
    control: &mut InvocationControl,
    plan: &CompiledOperationDependencies,
    requests: &RequestDependencyValues,
    hook: HookFuture,
) -> Result<(), ExecutionOutcome>
where
    HookFuture: Future<Output = Result<(), DependencyError>>,
{
    let outcome = match control.run(hook).await {
        Ok(Ok(())) => return Ok(()),
        Ok(Err(error)) => dependency_error_outcome(error),
        Err(abort) => abort.into_execution_outcome(),
    };
    if let Err(finalizer_error) = plan.finalize(requests).await {
        return Err(dependency_error_outcome(finalizer_error));
    }
    Err(outcome)
}

#[derive(Clone, Default)]
struct HookScope {
    on_request: Vec<OperationHook>,
    pre_parse: Vec<OperationHook>,
    pre_validate: Vec<OperationHook>,
    pre_handler: Vec<OperationHook>,
    pre_serialize: Vec<OperationHook>,
    on_error: Vec<ResponseHook>,
    on_response: Vec<ResponseHook>,
}

impl HookScope {
    fn is_empty(&self) -> bool {
        self.on_request.is_empty()
            && self.pre_parse.is_empty()
            && self.pre_validate.is_empty()
            && self.pre_handler.is_empty()
            && self.pre_serialize.is_empty()
            && self.on_error.is_empty()
            && self.on_response.is_empty()
    }

    fn inherited(&self, plugin: &PluginHooks) -> Self {
        let mut hooks = self.clone();
        hooks.on_request.extend(plugin.on_request.iter().cloned());
        hooks.pre_parse.extend(plugin.pre_parse.iter().cloned());
        hooks
            .pre_validate
            .extend(plugin.pre_validate.iter().cloned());
        hooks.pre_handler.extend(plugin.pre_handler.iter().cloned());
        hooks
            .pre_serialize
            .extend(plugin.pre_serialize.iter().cloned());
        hooks.on_error.extend(plugin.on_error.iter().cloned());
        hooks.on_response.extend(plugin.on_response.iter().cloned());
        hooks
    }
}

struct PluginHooks {
    on_request: Vec<OperationHook>,
    pre_parse: Vec<OperationHook>,
    pre_validate: Vec<OperationHook>,
    pre_handler: Vec<OperationHook>,
    pre_serialize: Vec<OperationHook>,
    on_error: Vec<ResponseHook>,
    on_response: Vec<ResponseHook>,
}

struct CompiledHooks {
    context: Option<HookContext>,
    scope: HookScope,
}

impl CompiledHooks {
    fn empty() -> Self {
        Self {
            context: None,
            scope: HookScope::default(),
        }
    }

    fn compile(operation: &OperationDescriptor, scope: HookScope) -> Self {
        Self {
            context: Some(HookContext {
                operation_id: Rc::from(operation.contract.id.as_str()),
            }),
            scope,
        }
    }

    fn is_empty(&self) -> bool {
        self.scope.is_empty()
    }

    async fn on_request(&self) -> Result<(), DependencyError> {
        for hook in &self.scope.on_request {
            hook(self.context()).await?;
        }
        Ok(())
    }

    async fn pre_handler(&self) -> Result<(), DependencyError> {
        for hook in &self.scope.pre_handler {
            hook(self.context()).await?;
        }
        Ok(())
    }

    async fn pre_parse(&self) -> Result<(), DependencyError> {
        for hook in &self.scope.pre_parse {
            hook(self.context()).await?;
        }
        Ok(())
    }

    async fn pre_validate(&self) -> Result<(), DependencyError> {
        for hook in &self.scope.pre_validate {
            hook(self.context()).await?;
        }
        Ok(())
    }

    async fn pre_serialize(&self) -> Result<(), DependencyError> {
        for hook in &self.scope.pre_serialize {
            hook(self.context()).await?;
        }
        Ok(())
    }

    async fn on_error(&self, outcome: &ExecutionOutcome) {
        if matches!(
            outcome,
            ExecutionOutcome::Success { .. }
                | ExecutionOutcome::StreamingSuccess { .. }
                | ExecutionOutcome::Upgrade { .. }
        ) {
            return;
        }
        let outcome = HookOutcome::from(outcome);
        for hook in self.scope.on_error.iter().rev() {
            hook(self.context(), outcome).await;
        }
    }

    async fn on_response(&self, outcome: &ExecutionOutcome) {
        let outcome = HookOutcome::from(outcome);
        for hook in self.scope.on_response.iter().rev() {
            hook(self.context(), outcome).await;
        }
    }

    fn context(&self) -> HookContext {
        self.context.clone().unwrap_or_else(|| HookContext {
            operation_id: Rc::from("uncompiled"),
        })
    }
}

#[derive(Clone)]
struct CompiledOperationDependencies {
    singletons: Rc<Vec<Option<DependencyValue>>>,
    request_providers: Vec<CompiledProvider>,
    handler_slots: Vec<DependencySlot>,
}

impl CompiledOperationDependencies {
    fn empty() -> Self {
        Self {
            singletons: Rc::new(Vec::new()),
            request_providers: Vec::new(),
            handler_slots: Vec::new(),
        }
    }

    async fn resolve(&self) -> Result<RequestDependencyValues, DependencyError> {
        let mut requests = RequestDependencyValues::new(self.request_providers.len());
        for (index, provider) in self.request_providers.iter().enumerate() {
            let value = match provider.run(&self.singletons, requests.as_slice()).await {
                Ok(value) => value,
                Err(error) => {
                    self.finalize_prefix(&requests, index).await?;
                    return Err(error);
                }
            };
            requests.set(index, value)?;
        }
        Ok(requests)
    }

    async fn resolve_controlled(
        &self,
        control: &mut InvocationControl,
    ) -> Result<RequestDependencyValues, ControlledInvocationError> {
        let mut requests = RequestDependencyValues::new(self.request_providers.len());
        for (index, provider) in self.request_providers.iter().enumerate() {
            let value = match control
                .run(provider.run(&self.singletons, requests.as_slice()))
                .await
            {
                Ok(Ok(value)) => value,
                Ok(Err(error)) => {
                    self.finalize_prefix(&requests, index)
                        .await
                        .map_err(ControlledInvocationError::Dependency)?;
                    return Err(ControlledInvocationError::Dependency(error));
                }
                Err(abort) => {
                    self.finalize_prefix(&requests, index)
                        .await
                        .map_err(ControlledInvocationError::Dependency)?;
                    return Err(ControlledInvocationError::Abort(abort));
                }
            };
            requests
                .set(index, value)
                .map_err(ControlledInvocationError::Dependency)?;
        }
        Ok(requests)
    }

    async fn finalize(&self, requests: &RequestDependencyValues) -> Result<(), DependencyError> {
        self.finalize_prefix(requests, self.request_providers.len())
            .await
    }

    async fn finalize_prefix(
        &self,
        requests: &RequestDependencyValues,
        initialized: usize,
    ) -> Result<(), DependencyError> {
        for (index, provider) in self.request_providers[..initialized]
            .iter()
            .enumerate()
            .rev()
        {
            let value = requests
                .as_slice()
                .get(index)
                .and_then(Option::as_ref)
                .ok_or_else(|| {
                    DependencyError::internal(
                        "invalid_dependency_slot",
                        "compiled finalizer could not read its dependency slot",
                    )
                })?;
            provider.finalize(value).await?;
        }
        Ok(())
    }
}

enum ControlledInvocationError {
    Dependency(DependencyError),
    Abort(InvocationAbort),
}

enum RequestDependencyValues {
    Inline {
        slots: [Option<DependencyValue>; INLINE_DEPENDENCY_SLOTS],
        len: usize,
    },
    Heap(Vec<Option<DependencyValue>>),
}

impl RequestDependencyValues {
    fn new(len: usize) -> Self {
        if len <= INLINE_DEPENDENCY_SLOTS {
            Self::Inline {
                slots: core::array::from_fn(|_| None),
                len,
            }
        } else {
            Self::Heap(vec![None; len])
        }
    }

    fn as_slice(&self) -> &[Option<DependencyValue>] {
        match self {
            Self::Inline { slots, len } => &slots[..*len],
            Self::Heap(slots) => slots,
        }
    }

    fn set(&mut self, index: usize, value: DependencyValue) -> Result<(), DependencyError> {
        let slot = match self {
            Self::Inline { slots, len } => {
                slots.get_mut(..*len).and_then(|slots| slots.get_mut(index))
            }
            Self::Heap(slots) => slots.get_mut(index),
        }
        .ok_or_else(|| {
            DependencyError::internal(
                "invalid_dependency_slot",
                "compiled request provider produced an unknown slot",
            )
        })?;
        *slot = Some(value);
        Ok(())
    }
}

/// A lexical provider scope containing operations and nested plugins.
pub struct Plugin {
    name: String,
    providers: Vec<Provider>,
    security_schemes: Vec<SecuritySchemeDescriptor>,
    operations: Vec<ExecutableOperation>,
    plugins: Vec<Self>,
    hooks: PluginHooks,
    startup_hooks: Vec<LifecycleHook>,
    shutdown_hooks: Vec<LifecycleHook>,
}

impl Plugin {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            providers: Vec::new(),
            security_schemes: Vec::new(),
            operations: Vec::new(),
            plugins: Vec::new(),
            hooks: PluginHooks {
                on_request: Vec::new(),
                pre_parse: Vec::new(),
                pre_validate: Vec::new(),
                pre_handler: Vec::new(),
                pre_serialize: Vec::new(),
                on_error: Vec::new(),
                on_response: Vec::new(),
            },
            startup_hooks: Vec::new(),
            shutdown_hooks: Vec::new(),
        }
    }

    #[must_use]
    pub fn provide(mut self, provider: Provider) -> Self {
        self.providers.push(provider);
        self
    }

    /// Registers an application security scheme from this plugin scope.
    #[must_use]
    pub fn security_scheme(mut self, scheme: SecuritySchemeDescriptor) -> Self {
        self.security_schemes.push(scheme);
        self
    }

    #[must_use]
    pub fn operation(mut self, operation: ExecutableOperation) -> Self {
        self.operations.push(operation);
        self
    }

    #[must_use]
    pub fn routes(mut self, operations: impl IntoIterator<Item = ExecutableOperation>) -> Self {
        self.operations.extend(operations);
        self
    }

    #[must_use]
    pub fn plugin(mut self, plugin: Self) -> Self {
        self.plugins.push(plugin);
        self
    }

    /// Adds a fallible async hook that runs before dependency resolution.
    #[must_use]
    pub fn on_request<Hook, HookFuture>(mut self, hook: Hook) -> Self
    where
        Hook: Fn(HookContext) -> HookFuture + 'static,
        HookFuture: Future<Output = Result<(), DependencyError>> + 'static,
    {
        self.hooks
            .on_request
            .push(Rc::new(move |context| Box::pin(hook(context))));
        self
    }

    /// Adds a fallible async hook before typed input extraction begins.
    #[must_use]
    pub fn pre_parse<Hook, HookFuture>(mut self, hook: Hook) -> Self
    where
        Hook: Fn(HookContext) -> HookFuture + 'static,
        HookFuture: Future<Output = Result<(), DependencyError>> + 'static,
    {
        self.hooks
            .pre_parse
            .push(Rc::new(move |context| Box::pin(hook(context))));
        self
    }

    /// Adds a fallible async hook immediately before input validation.
    #[must_use]
    pub fn pre_validate<Hook, HookFuture>(mut self, hook: Hook) -> Self
    where
        Hook: Fn(HookContext) -> HookFuture + 'static,
        HookFuture: Future<Output = Result<(), DependencyError>> + 'static,
    {
        self.hooks
            .pre_validate
            .push(Rc::new(move |context| Box::pin(hook(context))));
        self
    }

    /// Adds a fallible async hook after input preparation and before the user
    /// handler future is polled.
    #[must_use]
    pub fn pre_handler<Hook, HookFuture>(mut self, hook: Hook) -> Self
    where
        Hook: Fn(HookContext) -> HookFuture + 'static,
        HookFuture: Future<Output = Result<(), DependencyError>> + 'static,
    {
        self.hooks
            .pre_handler
            .push(Rc::new(move |context| Box::pin(hook(context))));
        self
    }

    /// Adds a fallible async hook after the handler and before transport
    /// response projection.
    #[must_use]
    pub fn pre_serialize<Hook, HookFuture>(mut self, hook: Hook) -> Self
    where
        Hook: Fn(HookContext) -> HookFuture + 'static,
        HookFuture: Future<Output = Result<(), DependencyError>> + 'static,
    {
        self.hooks
            .pre_serialize
            .push(Rc::new(move |context| Box::pin(hook(context))));
        self
    }

    /// Adds an async observer for rejected, domain-error, and internal-error
    /// outcomes.
    #[must_use]
    pub fn on_error<Hook, HookFuture>(mut self, hook: Hook) -> Self
    where
        Hook: Fn(HookContext, HookOutcome) -> HookFuture + 'static,
        HookFuture: Future<Output = ()> + 'static,
    {
        self.hooks.on_error.push(Rc::new(move |context, outcome| {
            Box::pin(hook(context, outcome))
        }));
        self
    }

    /// Adds an async observer that runs after the handler and DI finalizers.
    #[must_use]
    pub fn on_response<Hook, HookFuture>(mut self, hook: Hook) -> Self
    where
        Hook: Fn(HookContext, HookOutcome) -> HookFuture + 'static,
        HookFuture: Future<Output = ()> + 'static,
    {
        self.hooks
            .on_response
            .push(Rc::new(move |context, outcome| {
                Box::pin(hook(context, outcome))
            }));
        self
    }

    /// Adds a fallible async application startup hook.
    ///
    /// Startup executes parent hooks before child hooks. A failure stops
    /// startup before the server accepts requests.
    #[must_use]
    pub fn on_startup<Hook, HookFuture>(mut self, hook: Hook) -> Self
    where
        Hook: Fn() -> HookFuture + 'static,
        HookFuture: Future<Output = Result<(), DependencyError>> + 'static,
    {
        self.startup_hooks.push(Rc::new(move || Box::pin(hook())));
        self
    }

    /// Adds a fallible async application shutdown hook.
    ///
    /// Shutdown executes child hooks before parent hooks and continues after a
    /// failure so every registered cleanup receives a chance to run.
    #[must_use]
    pub fn on_shutdown<Hook, HookFuture>(mut self, hook: Hook) -> Self
    where
        Hook: Fn() -> HookFuture + 'static,
        HookFuture: Future<Output = Result<(), DependencyError>> + 'static,
    {
        self.shutdown_hooks.push(Rc::new(move || Box::pin(hook())));
        self
    }
}

struct TestOverrideEntry {
    plugin: Option<String>,
    provider: Provider,
    applied: bool,
}

/// Typed provider replacements applied only while compiling a test app.
///
/// Global replacements substitute every registered provider with the same
/// output type. Scoped replacements use a full plugin path such as
/// `app/users`; they can also shadow an inherited provider inside that scope.
#[derive(Default)]
pub struct TestOverrides {
    entries: Vec<TestOverrideEntry>,
}

impl TestOverrides {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Replaces every registered provider with the same output type.
    #[must_use]
    pub fn replace(mut self, provider: Provider) -> Self {
        self.insert(None, provider);
        self
    }

    /// Replaces or shadows a provider inside one exact plugin path.
    #[must_use]
    pub fn replace_in(mut self, plugin: impl Into<String>, provider: Provider) -> Self {
        self.insert(Some(plugin.into()), provider);
        self
    }

    fn insert(&mut self, plugin: Option<String>, provider: Provider) {
        let type_id = provider.key().type_id();
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|entry| entry.plugin == plugin && entry.provider.key().type_id() == type_id)
        {
            existing.provider = provider;
            existing.applied = false;
        } else {
            self.entries.push(TestOverrideEntry {
                plugin,
                provider,
                applied: false,
            });
        }
    }

    fn apply(&mut self, plugin: &str, providers: Vec<Provider>) -> Vec<Provider> {
        let mut providers = providers
            .into_iter()
            .map(|provider| {
                let type_id = provider.key().type_id();
                let replacement = self
                    .entries
                    .iter()
                    .position(|entry| {
                        entry.plugin.as_deref() == Some(plugin)
                            && entry.provider.key().type_id() == type_id
                    })
                    .or_else(|| {
                        self.entries.iter().position(|entry| {
                            entry.plugin.is_none() && entry.provider.key().type_id() == type_id
                        })
                    });
                replacement.map_or(provider, |index| {
                    let entry = &mut self.entries[index];
                    entry.applied = true;
                    entry.provider.clone()
                })
            })
            .collect::<Vec<_>>();

        for index in 0..self.entries.len() {
            let should_add = {
                let entry = &self.entries[index];
                entry.plugin.as_deref() == Some(plugin)
                    && !providers
                        .iter()
                        .any(|provider| provider.key().type_id() == entry.provider.key().type_id())
            };
            if !should_add {
                continue;
            }
            let entry = &mut self.entries[index];
            entry.applied = true;
            providers.push(entry.provider.clone());
        }
        providers
    }

    fn validate(&self) -> Result<(), ExecutableBuildError> {
        let Some(entry) = self.entries.iter().find(|entry| !entry.applied) else {
            return Ok(());
        };
        Err(ExecutableBuildError::UnknownProviderOverride {
            plugin: entry.plugin.clone(),
            dependency: entry.provider.key().type_name(),
        })
    }
}

/// An application-definition or dependency-compilation failure.
#[derive(Debug)]
pub enum ExecutableBuildError {
    Definition(blazingly_core::BuildError),
    InvalidPluginName {
        plugin: String,
    },
    DuplicateProvider {
        plugin: String,
        dependency: &'static str,
    },
    MissingProvider {
        plugin: String,
        consumer: String,
        dependency: &'static str,
    },
    ProviderCycle {
        plugin: String,
        dependencies: Vec<&'static str>,
    },
    InvalidLifetime {
        plugin: String,
        singleton: &'static str,
        shorter_lived_dependency: &'static str,
    },
    ProviderCompilation {
        plugin: String,
        dependency: &'static str,
        message: String,
    },
    SingletonProviderFailed {
        plugin: String,
        dependency: &'static str,
        message: String,
    },
    UnknownProviderOverride {
        plugin: Option<String>,
        dependency: &'static str,
    },
}

impl fmt::Display for ExecutableBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Definition(error) => error.fmt(formatter),
            Self::InvalidPluginName { plugin } => {
                write!(formatter, "invalid plugin name `{plugin}`")
            }
            Self::DuplicateProvider { plugin, dependency } => {
                write!(
                    formatter,
                    "plugin `{plugin}` registers dependency `{dependency}` more than once"
                )
            }
            Self::MissingProvider {
                plugin,
                consumer,
                dependency,
            } => write!(
                formatter,
                "plugin `{plugin}` cannot resolve dependency `{dependency}` required by `{consumer}`"
            ),
            Self::ProviderCycle {
                plugin,
                dependencies,
            } => write!(
                formatter,
                "plugin `{plugin}` contains a dependency cycle: {}",
                dependencies.join(" -> ")
            ),
            Self::InvalidLifetime {
                plugin,
                singleton,
                shorter_lived_dependency,
            } => write!(
                formatter,
                "singleton `{singleton}` in plugin `{plugin}` cannot depend on shorter-lived `{shorter_lived_dependency}`"
            ),
            Self::ProviderCompilation {
                plugin,
                dependency,
                message,
            } => write!(
                formatter,
                "provider `{dependency}` in plugin `{plugin}` could not be compiled: {message}"
            ),
            Self::SingletonProviderFailed {
                plugin,
                dependency,
                message,
            } => write!(
                formatter,
                "singleton provider `{dependency}` in plugin `{plugin}` failed: {message}"
            ),
            Self::UnknownProviderOverride { plugin, dependency } => match plugin {
                Some(plugin) => write!(
                    formatter,
                    "test override for `{dependency}` targets unknown plugin scope `{plugin}`"
                ),
                None => write!(
                    formatter,
                    "test override for `{dependency}` did not match a registered provider"
                ),
            },
        }
    }
}

impl std::error::Error for ExecutableBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Definition(error) => Some(error),
            _ => None,
        }
    }
}

impl From<blazingly_core::BuildError> for ExecutableBuildError {
    fn from(error: blazingly_core::BuildError) -> Self {
        Self::Definition(error)
    }
}

struct ProviderRegistration {
    provider: Provider,
    visible: HashMap<core::any::TypeId, usize>,
    plugin: String,
}

struct ScopedOperation {
    operation: ExecutableOperation,
    visible: HashMap<core::any::TypeId, usize>,
    plugin: String,
    hooks: HookScope,
}

/// A validated executable operation graph.
pub struct ExecutableApp {
    definition: AppDefinition,
    operations: Vec<ExecutableOperation>,
    by_id: BTreeMap<OperationId, usize>,
    startup_hooks: Vec<LifecycleHook>,
    shutdown_hooks: Vec<LifecycleHook>,
}

impl ExecutableApp {
    /// Validates and compiles executable operations.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutableBuildError`] for invalid routes or unresolved
    /// dependencies.
    pub fn new(
        operations: impl IntoIterator<Item = ExecutableOperation>,
    ) -> Result<Self, ExecutableBuildError> {
        Self::from_plugin(Plugin::new("app").routes(operations))
    }

    /// Compiles executable operations with application security schemes.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutableBuildError`] for invalid routes, security
    /// requirements, or unresolved dependencies.
    pub fn with_security_schemes(
        operations: impl IntoIterator<Item = ExecutableOperation>,
        schemes: impl IntoIterator<Item = SecuritySchemeDescriptor>,
    ) -> Result<Self, ExecutableBuildError> {
        let plugin = schemes
            .into_iter()
            .fold(Plugin::new("app"), Plugin::security_scheme)
            .routes(operations);
        Self::from_plugin(plugin)
    }

    /// Compiles plugin scopes, singleton values, request provider plans, and
    /// executable operations.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutableBuildError`] for invalid routes, plugin names,
    /// missing providers, cycles, invalid lifetimes, or singleton failures.
    pub fn from_plugin(plugin: Plugin) -> Result<Self, ExecutableBuildError> {
        Self::from_plugin_with_overrides(plugin, TestOverrides::new())
    }

    /// Compiles a plugin graph after applying typed test-only provider
    /// replacements.
    ///
    /// Overrides are applied before graph validation and compilation, so mocks
    /// cannot bypass missing dependency, cycle, or lifetime diagnostics.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutableBuildError`] for invalid overrides or for any normal
    /// plugin graph compilation failure.
    pub fn from_plugin_with_overrides(
        plugin: Plugin,
        mut overrides: TestOverrides,
    ) -> Result<Self, ExecutableBuildError> {
        let mut registrations = Vec::new();
        let mut scoped_operations = Vec::new();
        let mut security_schemes = Vec::new();
        let mut startup_hooks = Vec::new();
        let mut shutdown_hooks = Vec::new();
        let mut collector = PluginCollector {
            registrations: &mut registrations,
            operations: &mut scoped_operations,
            security_schemes: &mut security_schemes,
            startup_hooks: &mut startup_hooks,
            shutdown_hooks: &mut shutdown_hooks,
            overrides: &mut overrides,
        };
        collect_plugin(
            plugin,
            &HashMap::new(),
            &HookScope::default(),
            "",
            &mut collector,
        )?;
        overrides.validate()?;
        validate_provider_graph(&registrations)?;
        let (singletons, singleton_slots) = compile_singletons(&registrations)?;
        let singletons = Rc::new(singletons);
        let mut operations = Vec::with_capacity(scoped_operations.len());
        for mut scoped in scoped_operations {
            scoped.operation.hooks =
                CompiledHooks::compile(&scoped.operation.descriptor, scoped.hooks.clone());
            scoped.operation.dependency_plan = Some(compile_operation_dependencies(
                &scoped,
                &registrations,
                &singleton_slots,
                Rc::clone(&singletons),
            )?);
            operations.push(scoped.operation);
        }
        let definition = security_schemes
            .into_iter()
            .fold(App::new(), App::security_scheme)
            .routes(
                operations
                    .iter()
                    .map(|operation| operation.descriptor.clone()),
            )
            .build()?;
        let by_id = operations
            .iter()
            .enumerate()
            .map(|(index, operation)| (operation.descriptor.contract.id.clone(), index))
            .collect();

        Ok(Self {
            definition,
            operations,
            by_id,
            startup_hooks,
            shutdown_hooks,
        })
    }

    #[must_use]
    pub const fn definition(&self) -> &AppDefinition {
        &self.definition
    }

    #[must_use]
    pub fn operation(&self, id: &OperationId) -> Option<&ExecutableOperation> {
        self.by_id
            .get(id)
            .and_then(|index| self.operations.get(*index))
    }

    #[must_use]
    pub fn operation_index(&self, id: &OperationId) -> Option<usize> {
        self.by_id.get(id).copied()
    }

    #[must_use]
    pub fn operation_at(&self, index: usize) -> Option<&ExecutableOperation> {
        self.operations.get(index)
    }

    #[must_use]
    pub fn operation_for_mcp_tool(&self, name: &str) -> Option<&ExecutableOperation> {
        self.operations.iter().find(|operation| {
            operation
                .descriptor
                .contract
                .mcp
                .as_ref()
                .is_some_and(|tool| tool.name == name)
        })
    }

    pub async fn invoke(&self, id: &OperationId, input: Value) -> ExecutionOutcome {
        let Some(operation) = self.operation(id) else {
            return ExecutionOutcome::Rejected {
                status: 404,
                code: "operation_not_found".to_owned(),
                message: "operation not found".to_owned(),
                details: None,
            };
        };
        operation.invoke(input).await
    }

    pub async fn invoke_controlled(
        &self,
        id: &OperationId,
        input: Value,
        control: InvocationControl,
    ) -> ExecutionOutcome {
        let Some(operation) = self.operation(id) else {
            return ExecutionOutcome::Rejected {
                status: 404,
                code: "operation_not_found".to_owned(),
                message: "operation not found".to_owned(),
                details: None,
            };
        };
        operation.invoke_controlled(input, control).await
    }

    /// Runs application startup hooks in parent-before-child order.
    ///
    /// # Errors
    ///
    /// Returns the first startup hook failure and stops the remaining startup
    /// sequence.
    pub async fn startup(&self) -> Result<(), DependencyError> {
        for hook in &self.startup_hooks {
            hook().await?;
        }
        Ok(())
    }

    /// Runs application shutdown hooks in child-before-parent order.
    ///
    /// Every hook runs even when an earlier hook fails. The first error in
    /// execution order is returned after all cleanup has completed.
    ///
    /// # Errors
    ///
    /// Returns the first shutdown hook failure.
    pub async fn shutdown(&self) -> Result<(), DependencyError> {
        let mut first_error = None;
        for hook in self.shutdown_hooks.iter().rev() {
            if let Err(error) = hook().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

struct PluginCollector<'collector> {
    registrations: &'collector mut Vec<ProviderRegistration>,
    operations: &'collector mut Vec<ScopedOperation>,
    security_schemes: &'collector mut Vec<SecuritySchemeDescriptor>,
    startup_hooks: &'collector mut Vec<LifecycleHook>,
    shutdown_hooks: &'collector mut Vec<LifecycleHook>,
    overrides: &'collector mut TestOverrides,
}

fn collect_plugin(
    plugin: Plugin,
    inherited: &HashMap<core::any::TypeId, usize>,
    inherited_hooks: &HookScope,
    parent_path: &str,
    collector: &mut PluginCollector<'_>,
) -> Result<(), ExecutableBuildError> {
    let Plugin {
        name,
        providers,
        security_schemes: plugin_security_schemes,
        operations: plugin_operations,
        plugins,
        hooks: plugin_hooks,
        startup_hooks: plugin_startup_hooks,
        shutdown_hooks: plugin_shutdown_hooks,
    } = plugin;
    let path = if parent_path.is_empty() {
        name.clone()
    } else {
        format!("{parent_path}/{name}")
    };
    if !valid_plugin_name(&name) {
        return Err(ExecutableBuildError::InvalidPluginName { plugin: path });
    }

    let mut visible = inherited.clone();
    collector.security_schemes.extend(plugin_security_schemes);
    collector.startup_hooks.extend(plugin_startup_hooks);
    collector.shutdown_hooks.extend(plugin_shutdown_hooks);
    let hooks = inherited_hooks.inherited(&plugin_hooks);
    let providers = collector.overrides.apply(&path, providers);
    let mut local = HashSet::new();
    let mut registration_ids = Vec::with_capacity(providers.len());
    for provider in providers {
        let key = provider.key();
        if !local.insert(key.type_id()) {
            return Err(ExecutableBuildError::DuplicateProvider {
                plugin: path,
                dependency: key.type_name(),
            });
        }
        let id = collector.registrations.len();
        collector.registrations.push(ProviderRegistration {
            provider,
            visible: HashMap::new(),
            plugin: path.clone(),
        });
        visible.insert(key.type_id(), id);
        registration_ids.push(id);
    }
    for id in registration_ids {
        collector.registrations[id].visible.clone_from(&visible);
    }
    collector.operations.extend(
        plugin_operations
            .into_iter()
            .map(|operation| ScopedOperation {
                operation,
                visible: visible.clone(),
                plugin: path.clone(),
                hooks: hooks.clone(),
            }),
    );
    for child in plugins {
        collect_plugin(child, &visible, &hooks, &path, collector)?;
    }
    Ok(())
}

fn valid_plugin_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn validate_provider_graph(
    registrations: &[ProviderRegistration],
) -> Result<(), ExecutableBuildError> {
    let mut states = vec![0_u8; registrations.len()];
    let mut stack = Vec::new();
    for id in 0..registrations.len() {
        validate_provider(id, registrations, &mut states, &mut stack)?;
    }
    Ok(())
}

fn validate_provider(
    id: usize,
    registrations: &[ProviderRegistration],
    states: &mut [u8],
    stack: &mut Vec<usize>,
) -> Result<(), ExecutableBuildError> {
    if states[id] == 2 {
        return Ok(());
    }
    if states[id] == 1 {
        let cycle_start = stack
            .iter()
            .position(|candidate| *candidate == id)
            .unwrap_or(0);
        let mut dependencies = stack[cycle_start..]
            .iter()
            .map(|candidate| registrations[*candidate].provider.key().type_name())
            .collect::<Vec<_>>();
        dependencies.push(registrations[id].provider.key().type_name());
        return Err(ExecutableBuildError::ProviderCycle {
            plugin: registrations[id].plugin.clone(),
            dependencies,
        });
    }

    states[id] = 1;
    stack.push(id);
    let registration = &registrations[id];
    for dependency in registration.provider.dependencies() {
        let Some(dependency_id) = registration.visible.get(&dependency.type_id()).copied() else {
            return Err(ExecutableBuildError::MissingProvider {
                plugin: registration.plugin.clone(),
                consumer: registration.provider.key().type_name().to_owned(),
                dependency: dependency.type_name(),
            });
        };
        if registration.provider.lifetime() == DependencyLifetime::Singleton
            && registrations[dependency_id].provider.lifetime() != DependencyLifetime::Singleton
        {
            return Err(ExecutableBuildError::InvalidLifetime {
                plugin: registration.plugin.clone(),
                singleton: registration.provider.key().type_name(),
                shorter_lived_dependency: dependency.type_name(),
            });
        }
        validate_provider(dependency_id, registrations, states, stack)?;
    }
    stack.pop();
    states[id] = 2;
    Ok(())
}

fn compile_singletons(
    registrations: &[ProviderRegistration],
) -> Result<SingletonCompilation, ExecutableBuildError> {
    let mut singleton_slots = vec![None; registrations.len()];
    let mut singleton_count = 0;
    for (id, registration) in registrations.iter().enumerate() {
        if registration.provider.lifetime() == DependencyLifetime::Singleton {
            singleton_slots[id] = Some(singleton_count);
            singleton_count += 1;
        }
    }
    let mut values = vec![None; singleton_count];
    let mut built = vec![false; registrations.len()];
    for id in 0..registrations.len() {
        if singleton_slots[id].is_some() {
            compile_singleton(id, registrations, &singleton_slots, &mut values, &mut built)?;
        }
    }
    Ok((values, singleton_slots))
}

fn compile_singleton(
    id: usize,
    registrations: &[ProviderRegistration],
    singleton_slots: &[Option<usize>],
    values: &mut [Option<DependencyValue>],
    built: &mut [bool],
) -> Result<(), ExecutableBuildError> {
    if built[id] {
        return Ok(());
    }
    let registration = &registrations[id];
    let mut dependency_slots = Vec::with_capacity(registration.provider.dependencies().len());
    for dependency in registration.provider.dependencies() {
        let dependency_id = registration.visible[&dependency.type_id()];
        compile_singleton(dependency_id, registrations, singleton_slots, values, built)?;
        let Some(slot) = singleton_slots[dependency_id] else {
            return Err(ExecutableBuildError::ProviderCompilation {
                plugin: registration.plugin.clone(),
                dependency: registration.provider.key().type_name(),
                message: "validated singleton dependency has no compiled slot".to_owned(),
            });
        };
        dependency_slots.push(DependencySlot::Singleton(slot));
    }
    let compiled = registration
        .provider
        .compile(&dependency_slots)
        .map_err(|error| ExecutableBuildError::ProviderCompilation {
            plugin: registration.plugin.clone(),
            dependency: registration.provider.key().type_name(),
            message: error.to_string(),
        })?;
    let value = compiled.run_sync(values, &[]).map_err(|error| {
        ExecutableBuildError::SingletonProviderFailed {
            plugin: registration.plugin.clone(),
            dependency: registration.provider.key().type_name(),
            message: error.to_string(),
        }
    })?;
    let Some(slot) = singleton_slots[id] else {
        return Err(ExecutableBuildError::ProviderCompilation {
            plugin: registration.plugin.clone(),
            dependency: registration.provider.key().type_name(),
            message: "singleton provider has no compiled slot".to_owned(),
        });
    };
    values[slot] = Some(value);
    built[id] = true;
    Ok(())
}

fn compile_operation_dependencies(
    scoped: &ScopedOperation,
    registrations: &[ProviderRegistration],
    singleton_slots: &[Option<usize>],
    singletons: Rc<Vec<Option<DependencyValue>>>,
) -> Result<CompiledOperationDependencies, ExecutableBuildError> {
    let mut request_slots = HashMap::new();
    let mut request_providers = Vec::new();
    let mut handler_slots = Vec::with_capacity(scoped.operation.dependency_requests.len());
    for request in &scoped.operation.dependency_requests {
        let key = request.key();
        let Some(provider_id) = scoped.visible.get(&key.type_id()).copied() else {
            return Err(ExecutableBuildError::MissingProvider {
                plugin: scoped.plugin.clone(),
                consumer: scoped.operation.descriptor.contract.id.as_str().to_owned(),
                dependency: key.type_name(),
            });
        };
        handler_slots.push(compile_request_provider(
            provider_id,
            registrations,
            singleton_slots,
            &mut request_slots,
            &mut request_providers,
        )?);
    }
    Ok(CompiledOperationDependencies {
        singletons,
        request_providers,
        handler_slots,
    })
}

fn compile_request_provider(
    id: usize,
    registrations: &[ProviderRegistration],
    singleton_slots: &[Option<usize>],
    request_slots: &mut HashMap<usize, usize>,
    request_providers: &mut Vec<CompiledProvider>,
) -> Result<DependencySlot, ExecutableBuildError> {
    let registration = &registrations[id];
    if registration.provider.lifetime() == DependencyLifetime::Singleton {
        let Some(slot) = singleton_slots[id] else {
            return Err(ExecutableBuildError::ProviderCompilation {
                plugin: registration.plugin.clone(),
                dependency: registration.provider.key().type_name(),
                message: "singleton provider has no compiled slot".to_owned(),
            });
        };
        return Ok(DependencySlot::Singleton(slot));
    }
    if registration.provider.lifetime() == DependencyLifetime::Request
        && let Some(slot) = request_slots.get(&id)
    {
        return Ok(DependencySlot::Request(*slot));
    }

    let mut dependency_slots = Vec::with_capacity(registration.provider.dependencies().len());
    for dependency in registration.provider.dependencies() {
        let dependency_id = registration.visible[&dependency.type_id()];
        dependency_slots.push(compile_request_provider(
            dependency_id,
            registrations,
            singleton_slots,
            request_slots,
            request_providers,
        )?);
    }
    let provider = registration
        .provider
        .compile(&dependency_slots)
        .map_err(|error| ExecutableBuildError::ProviderCompilation {
            plugin: registration.plugin.clone(),
            dependency: registration.provider.key().type_name(),
            message: error.to_string(),
        })?;
    let slot = request_providers.len();
    request_providers.push(provider);
    if registration.provider.lifetime() == DependencyLifetime::Request {
        request_slots.insert(id, slot);
    }
    Ok(DependencySlot::Request(slot))
}

fn resolve_dependency<T: 'static>(
    slot: DependencySlot,
    singletons: &[Option<DependencyValue>],
    requests: &[Option<DependencyValue>],
) -> Result<Depends<T>, DependencyError> {
    let value = match slot {
        DependencySlot::Singleton(index) => singletons.get(index),
        DependencySlot::Request(index) => requests.get(index),
    }
    .and_then(Option::as_ref)
    .ok_or_else(|| {
        DependencyError::internal(
            "invalid_dependency_slot",
            "compiled handler dependency slot was not initialized",
        )
    })?;
    Rc::clone(value)
        .downcast::<T>()
        .map(Depends::from_rc)
        .map_err(|_| {
            DependencyError::internal(
                "dependency_type_mismatch",
                "compiled handler dependency slot contained an unexpected type",
            )
        })
}

#[doc(hidden)]
#[must_use]
pub fn dependency_error_outcome(error: DependencyError) -> ExecutionOutcome {
    match error {
        DependencyError::Rejected(failure) => ExecutionOutcome::DomainError(failure),
        DependencyError::Internal { code, message } => internal_dependency_error(code, message),
    }
}

fn internal_dependency_error(
    code: impl Into<String>,
    message: impl Into<String>,
) -> ExecutionOutcome {
    ExecutionOutcome::InternalError {
        code: code.into(),
        message: message.into(),
    }
}

/// Encodes a success body, reserving what the previous body of this shape
/// needed.
///
/// `blazingly_json::to_vec` starts from a 128-byte buffer and doubles, so an 18 KB
/// listing pays about nine reallocations and copies its own body roughly twice
/// over before it reaches the transport. The shape key is per-monomorphization,
/// so each response type learns its own size independently; a stale or
/// colliding hint changes only the initial capacity, never the bytes produced.
fn serialize_success<T: Serialize>(status: u16, value: T) -> ExecutionOutcome {
    let mut body = Vec::with_capacity(blazingly_core::response_size_hint::<T>());
    match blazingly_json::to_writer(&mut body, &value) {
        Ok(()) => {
            blazingly_core::record_response_size::<T>(body.len());
            ExecutionOutcome::Success {
                status,
                headers: Vec::new(),
                body: Some(body),
                background: Vec::new(),
            }
        }
        Err(_) => ExecutionOutcome::InternalError {
            code: "serialization_failed".to_owned(),
            message: "operation response could not be serialized".to_owned(),
        },
    }
}

fn internal_build_error(error: ResponseBuildError) -> ExecutionOutcome {
    ExecutionOutcome::InternalError {
        code: error.code,
        message: error.message,
    }
}

fn valid_response_header(header: &ResponseHeader) -> bool {
    !header.name.is_empty()
        && header.name.bytes().all(is_header_name_byte)
        && header
            .value
            .bytes()
            .all(|byte| byte == b'\t' || (byte >= b' ' && byte != 127))
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

const MAX_MULTIPART_PARTS: usize = 256;
const MAX_MULTIPART_HEADER_BYTES: usize = 16 * 1024;

struct MultipartPart<'body> {
    name: String,
    file_name: Option<String>,
    content_type: Option<String>,
    bytes: &'body [u8],
}

impl MultipartPart<'_> {
    fn to_upload(&self) -> UploadFile {
        UploadFile {
            field_name: self.name.clone(),
            file_name: self.file_name.clone(),
            content_type: self.content_type.clone(),
            bytes: self.bytes.to_vec(),
        }
    }

    fn into_upload(self) -> UploadFile {
        UploadFile {
            field_name: self.name,
            file_name: self.file_name,
            content_type: self.content_type,
            bytes: self.bytes.to_vec(),
        }
    }
}

fn parse_multipart_request(
    request: &dyn HttpRequestParts,
) -> Result<Vec<MultipartPart<'_>>, InputRejection> {
    let content_type = request
        .value(InputSource::Header, "content-type", 0)
        .ok_or_else(|| multipart_rejection("missing multipart Content-Type header"))?;
    let boundary = multipart_boundary(&content_type)
        .ok_or_else(|| multipart_rejection("multipart boundary is missing or invalid"))?;
    parse_multipart(request.body(), &boundary)
}

fn multipart_boundary(content_type: &str) -> Option<String> {
    let mut parameters = header_parameters(content_type);
    if !parameters
        .next()?
        .trim()
        .eq_ignore_ascii_case("multipart/form-data")
    {
        return None;
    }
    for parameter in parameters {
        let (name, value) = parameter.split_once('=')?;
        if name.trim().eq_ignore_ascii_case("boundary") {
            let boundary = unquote_header_value(value.trim())?;
            if boundary.is_empty()
                || boundary.len() > 70
                || boundary.bytes().any(|byte| byte <= b' ' || byte >= 127)
            {
                return None;
            }
            return Some(boundary);
        }
    }
    None
}

fn parse_multipart<'body>(
    body: &'body [u8],
    boundary: &str,
) -> Result<Vec<MultipartPart<'body>>, InputRejection> {
    let delimiter = format!("--{boundary}").into_bytes();
    if !body.starts_with(&delimiter) {
        return Err(multipart_rejection(
            "multipart body does not start with its declared boundary",
        ));
    }
    let mut position = delimiter.len();
    if body.get(position..position + 2) == Some(b"--") {
        return Ok(Vec::new());
    }
    if body.get(position..position + 2) != Some(b"\r\n") {
        return Err(multipart_rejection("multipart boundary is malformed"));
    }
    position += 2;

    let mut parts = Vec::new();
    loop {
        let header_end = find_bytes(body, b"\r\n\r\n", position)
            .ok_or_else(|| multipart_rejection("multipart part headers are incomplete"))?;
        if header_end - position > MAX_MULTIPART_HEADER_BYTES {
            return Err(multipart_rejection(
                "multipart part headers exceed the configured limit",
            ));
        }
        let headers = std::str::from_utf8(&body[position..header_end])
            .map_err(|_| multipart_rejection("multipart part headers are not valid UTF-8"))?;
        let (name, file_name, content_type) = multipart_part_headers(headers)?;
        let data_start = header_end + 4;
        let boundary_start = find_multipart_boundary(body, &delimiter, data_start)
            .ok_or_else(|| multipart_rejection("multipart part has no closing boundary"))?;
        parts.push(MultipartPart {
            name,
            file_name,
            content_type,
            bytes: &body[data_start..boundary_start],
        });
        if parts.len() > MAX_MULTIPART_PARTS {
            return Err(multipart_rejection(
                "multipart body contains too many parts",
            ));
        }

        position = boundary_start + 2 + delimiter.len();
        if body.get(position..position + 2) == Some(b"--") {
            return Ok(parts);
        }
        if body.get(position..position + 2) != Some(b"\r\n") {
            return Err(multipart_rejection("multipart boundary is malformed"));
        }
        position += 2;
    }
}

fn multipart_part_headers(
    headers: &str,
) -> Result<(String, Option<String>, Option<String>), InputRejection> {
    let mut name = None;
    let mut file_name = None;
    let mut content_type = None;
    for line in headers.split("\r\n") {
        let (header_name, value) = line
            .split_once(':')
            .ok_or_else(|| multipart_rejection("multipart part header is malformed"))?;
        if header_name.eq_ignore_ascii_case("content-disposition") {
            let mut parameters = header_parameters(value);
            if !parameters
                .next()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("form-data"))
            {
                return Err(multipart_rejection(
                    "multipart Content-Disposition must be form-data",
                ));
            }
            for parameter in parameters {
                let Some((parameter_name, parameter_value)) = parameter.split_once('=') else {
                    continue;
                };
                if parameter_name.trim().eq_ignore_ascii_case("name") {
                    name = unquote_header_value(parameter_value.trim());
                } else if parameter_name.trim().eq_ignore_ascii_case("filename") {
                    file_name = unquote_header_value(parameter_value.trim());
                }
            }
        } else if header_name.eq_ignore_ascii_case("content-type") {
            content_type = Some(value.trim().to_owned());
        }
    }
    let name = name
        .filter(|name| !name.is_empty())
        .ok_or_else(|| multipart_rejection("multipart part has no field name"))?;
    Ok((name, file_name, content_type))
}

fn header_parameters(value: &str) -> impl Iterator<Item = &str> {
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    let mut ranges = Vec::new();
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' && quoted {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character == ';' && !quoted {
            ranges.push((start, index));
            start = index + character.len_utf8();
        }
    }
    ranges.push((start, value.len()));
    ranges
        .into_iter()
        .map(move |(start, end)| &value[start..end])
}

fn unquote_header_value(value: &str) -> Option<String> {
    if let Some(value) = value.strip_prefix('"') {
        let value = value.strip_suffix('"')?;
        let mut output = String::with_capacity(value.len());
        let mut escaped = false;
        for character in value.chars() {
            if escaped {
                output.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else {
                output.push(character);
            }
        }
        if escaped {
            return None;
        }
        Some(output)
    } else {
        Some(value.to_owned())
    }
}

fn find_multipart_boundary(body: &[u8], delimiter: &[u8], from: usize) -> Option<usize> {
    let mut position = from;
    while let Some(found) = find_bytes(body, b"\r\n--", position) {
        let delimiter_start = found + 2;
        if body.get(delimiter_start..delimiter_start + delimiter.len()) == Some(delimiter) {
            let suffix = delimiter_start + delimiter.len();
            if matches!(body.get(suffix..suffix + 2), Some(b"\r\n" | b"--")) {
                return Some(found);
            }
        }
        position = found + 2;
    }
    None
}

fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| from + position)
}

fn multipart_argument_value(
    parts: &[MultipartPart<'_>],
    name: &str,
    required: bool,
    descriptor: &TypeDescriptor,
) -> Result<Value, InputRejection> {
    if let Some(model) = &descriptor.model {
        let mut properties = blazingly_json::Map::new();
        for field in &model.fields {
            let matching = parts
                .iter()
                .filter(|part| part.name == field.name)
                .collect::<Vec<_>>();
            if let Some(value) = multipart_parts_value(&matching, &field.ty)? {
                properties.insert(field.name.clone(), value);
            }
        }
        if properties.is_empty() && !required {
            return Ok(Value::Null);
        }
        return Ok(Value::Object(properties));
    }

    let matching = parts
        .iter()
        .filter(|part| part.name == name)
        .collect::<Vec<_>>();
    multipart_parts_value(&matching, descriptor)?.map_or_else(
        || {
            if required {
                Err(missing_input(name, InputSource::Multipart))
            } else {
                Ok(Value::Null)
            }
        },
        Ok,
    )
}

fn multipart_parts_value(
    parts: &[&MultipartPart<'_>],
    descriptor: &TypeDescriptor,
) -> Result<Option<Value>, InputRejection> {
    if let SchemaKind::Array(item_schema) = &descriptor.schema {
        let values = if let Some(item) = &descriptor.items {
            parts
                .iter()
                .map(|part| multipart_part_value(part, item))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            parts
                .iter()
                .map(|part| multipart_scalar_value(part, item_schema))
                .collect::<Result<Vec<_>, _>>()?
        };
        return Ok((!values.is_empty()).then_some(Value::Array(values)));
    }
    parts
        .first()
        .map(|part| multipart_part_value(part, descriptor))
        .transpose()
}

fn multipart_part_value(
    part: &MultipartPart<'_>,
    descriptor: &TypeDescriptor,
) -> Result<Value, InputRejection> {
    if descriptor.schema == SchemaKind::Binary {
        return blazingly_json::to_value(part.to_upload())
            .map_err(|_| multipart_rejection("uploaded file metadata could not be decoded"));
    }
    multipart_scalar_value(part, &descriptor.schema)
}

fn multipart_scalar_value(
    part: &MultipartPart<'_>,
    schema: &SchemaKind,
) -> Result<Value, InputRejection> {
    let value = std::str::from_utf8(part.bytes)
        .map_err(|_| multipart_rejection("multipart text field is not valid UTF-8"))?;
    Ok(raw_scalar_value(value, schema))
}

fn upload_arguments(
    arguments: &Value,
    name: &str,
    required: bool,
) -> Result<Vec<UploadFile>, InputRejection> {
    let value = arguments
        .as_object()
        .and_then(|arguments| arguments.get(name))
        .unwrap_or(arguments);
    if value.is_null() {
        return if required {
            Err(missing_input(name, InputSource::File))
        } else {
            Ok(Vec::new())
        };
    }
    match value {
        Value::Array(values) => values
            .iter()
            .map(|value| upload_from_value(value, name))
            .collect(),
        value => upload_from_value(value, name).map(|upload| vec![upload]),
    }
}

fn upload_from_value(value: &Value, name: &str) -> Result<UploadFile, InputRejection> {
    if let Value::String(encoded) = value {
        return decode_base64_upload(encoded, name, None, None);
    }
    if let Value::Object(object) = value {
        let encoded = object
            .get("base64")
            .or_else(|| object.get("data"))
            .or_else(|| object.get("content"))
            .and_then(Value::as_str);
        if let Some(encoded) = encoded {
            return decode_base64_upload(
                encoded,
                object
                    .get("field_name")
                    .and_then(Value::as_str)
                    .unwrap_or(name),
                object
                    .get("file_name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                object
                    .get("content_type")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            );
        }
    }
    blazingly_json::from_value(value.clone())
        .map_err(|error| decode_rejection(name, InputSource::File, &error.to_string()))
}

fn decode_base64_upload(
    encoded: &str,
    field_name: &str,
    file_name: Option<String>,
    content_type: Option<String>,
) -> Result<UploadFile, InputRejection> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| decode_rejection(field_name, InputSource::File, &error.to_string()))?;
    Ok(UploadFile {
        field_name: field_name.to_owned(),
        file_name,
        content_type,
        bytes,
    })
}

fn file_count_rejection(required: bool, actual: usize, expected: &str) -> InputRejection {
    InputRejection {
        status: 422,
        code: "invalid_file_count".to_owned(),
        message: format!("file input expected {expected} upload"),
        details: Some(json!({
            "source": "file",
            "required": required,
            "expected": expected,
            "actual": actual
        })),
    }
}

fn multipart_rejection(reason: &str) -> InputRejection {
    InputRejection {
        status: 422,
        code: "invalid_multipart".to_owned(),
        message: "request body is not valid multipart form data".to_owned(),
        details: Some(json!({
            "source": "multipart",
            "reason": reason
        })),
    }
}

thread_local! {
    /// Type descriptors already built on this thread, keyed by the address of
    /// the monomorphized `type_descriptor` function.
    ///
    /// A descriptor is a compile-time constant of its type, but building one
    /// allocates a `String` per field name, a nested `TypeDescriptor` per field
    /// and a `Vec` per rule list — around thirty allocations for a
    /// seven-field query model, repeated on every single request.
    ///
    /// Keying on the function address is sound because two distinct types can
    /// only share one address if the linker folded their `type_descriptor`
    /// bodies together, and folding requires the bodies to be byte-identical
    /// and therefore to produce identical descriptors. Extra entries (the same
    /// type instantiated in two crates) cost only a duplicate cache line.
    static TYPE_DESCRIPTORS: std::cell::RefCell<Vec<(usize, Rc<TypeDescriptor>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Returns the descriptor for `T`, building it at most once per thread.
fn cached_type_descriptor<T: ApiSchema>() -> Rc<TypeDescriptor> {
    let key = <T as ApiSchema>::type_descriptor as fn() -> TypeDescriptor as usize;
    let cached = TYPE_DESCRIPTORS.with_borrow(|cache| {
        cache
            .iter()
            .find(|(cached_key, _)| *cached_key == key)
            .map(|(_, descriptor)| Rc::clone(descriptor))
    });
    if let Some(descriptor) = cached {
        return descriptor;
    }
    // Built outside the borrow: a generated descriptor may itself reach back
    // into this cache for a nested model.
    let descriptor = Rc::new(T::type_descriptor());
    TYPE_DESCRIPTORS.with_borrow_mut(|cache| cache.push((key, Rc::clone(&descriptor))));
    descriptor
}

fn extract_argument<T>(
    input: &InvocationInput<'_>,
    name: &str,
    source: InputSource,
    required: bool,
) -> Result<T, InputRejection>
where
    T: ApiSchema + DeserializeOwned,
{
    // A JSON body is decoded straight from the request bytes and never
    // consults the descriptor, so it is not built at all on that path.
    if let InvocationInput::Http(request) = input
        && source == InputSource::Json
    {
        let mut deserializer = blazingly_json::Deserializer::from_slice(request.body());
        let decoded =
            serde_path_to_error::deserialize::<_, T>(&mut deserializer).map_err(|error| {
                decode_path_rejection(
                    name,
                    source,
                    &error.path().to_string(),
                    &error.inner().to_string(),
                )
            })?;
        return validate_decoded(decoded, source);
    }

    let descriptor = cached_type_descriptor::<T>();
    let value = match input {
        InvocationInput::Http(request) => {
            raw_argument_value(*request, name, source, required, &descriptor)?
        }
        InvocationInput::Arguments(arguments) => {
            structured_argument_value(arguments, name, source, required, &descriptor)?
        }
    };

    let decoded = blazingly_json::from_value::<T>(value)
        .map_err(|error| decode_rejection(name, source, &error.to_string()))?;
    validate_decoded(decoded, source)
}

fn validate_decoded<T: ApiSchema>(decoded: T, source: InputSource) -> Result<T, InputRejection> {
    decoded.validate_input().map_err(|errors| InputRejection {
        status: 422,
        code: "validation_error".to_owned(),
        message: format!("{} input failed validation", source_name(source)),
        details: blazingly_json::to_value(errors).ok(),
    })?;
    Ok(decoded)
}

fn raw_argument_value(
    request: &dyn HttpRequestParts,
    name: &str,
    source: InputSource,
    required: bool,
    descriptor: &TypeDescriptor,
) -> Result<Value, InputRejection> {
    if let Some(model) = &descriptor.model {
        let properties = model
            .fields
            .iter()
            .filter_map(|field| {
                raw_field_value(request, source, field)
                    .map(|(name, value)| (name.to_owned(), value))
            })
            .collect();
        let properties: blazingly_json::Map<String, Value> = properties;
        if properties.is_empty() && !required {
            return Ok(Value::Null);
        }
        return Ok(Value::Object(properties));
    }

    let Some(raw) = raw_typed_value(request, source, name, descriptor) else {
        return if required {
            Err(missing_input(name, source))
        } else {
            Ok(Value::Null)
        };
    };
    Ok(raw)
}

fn structured_argument_value(
    arguments: &Value,
    name: &str,
    source: InputSource,
    required: bool,
    descriptor: &TypeDescriptor,
) -> Result<Value, InputRejection> {
    if descriptor.model.is_some() {
        let value = select_model_fields(arguments, name, descriptor);
        if value.as_object().is_some_and(blazingly_json::Map::is_empty) && !required {
            return Ok(Value::Null);
        }
        return Ok(value);
    }

    let value = arguments
        .as_object()
        .and_then(|arguments| arguments.get(name))
        .cloned();
    if required {
        value.ok_or_else(|| missing_input(name, source))
    } else {
        Ok(value.unwrap_or(Value::Null))
    }
}

fn select_model_fields(arguments: &Value, name: &str, descriptor: &TypeDescriptor) -> Value {
    let Some(arguments) = arguments.as_object() else {
        return arguments.clone();
    };
    if let Some(Value::Object(nested)) = arguments.get(name) {
        return Value::Object(nested.clone());
    }
    let Some(model) = &descriptor.model else {
        return Value::Object(arguments.clone());
    };
    Value::Object(
        model
            .fields
            .iter()
            .filter_map(|field| {
                field_input_names(field).find_map(|field_name| {
                    arguments
                        .get(field_name)
                        .cloned()
                        .map(|value| (field_name.to_owned(), value))
                })
            })
            .collect(),
    )
}

fn raw_field_value<'field>(
    request: &dyn HttpRequestParts,
    source: InputSource,
    field: &'field blazingly_core::FieldDescriptor,
) -> Option<(&'field str, Value)> {
    field_input_names(field).find_map(|field_name| {
        raw_typed_value(request, source, field_name, &field.ty).map(|value| (field_name, value))
    })
}

fn field_input_names(field: &blazingly_core::FieldDescriptor) -> impl Iterator<Item = &str> {
    std::iter::once(field.name.as_str()).chain(field.validation.iter().filter_map(
        |rule| match rule {
            blazingly_core::ValidationRule::Alias(alias) => Some(alias.as_str()),
            blazingly_core::ValidationRule::MinLength(_)
            | blazingly_core::ValidationRule::MaxLength(_)
            | blazingly_core::ValidationRule::Email
            | blazingly_core::ValidationRule::Custom(_)
            | blazingly_core::ValidationRule::Nested => None,
        },
    ))
}

fn raw_typed_value(
    request: &dyn HttpRequestParts,
    source: InputSource,
    name: &str,
    descriptor: &TypeDescriptor,
) -> Option<Value> {
    if let SchemaKind::Array(item) = &descriptor.schema {
        let mut values = Vec::new();
        let mut index = 0;
        while let Some(value) = request.value(source, name, index) {
            values.push(raw_scalar_value(&value, item));
            index += 1;
        }
        return (!values.is_empty()).then_some(Value::Array(values));
    }

    request
        .value(source, name, 0)
        .map(|value| raw_scalar_value(&value, &descriptor.schema))
}

fn raw_scalar_value(value: &str, schema: &SchemaKind) -> Value {
    match schema {
        SchemaKind::String | SchemaKind::Binary | SchemaKind::Array(_) => {
            Value::String(value.to_owned())
        }
        SchemaKind::Integer | SchemaKind::Number | SchemaKind::Boolean => {
            blazingly_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_owned()))
        }
        SchemaKind::Object | SchemaKind::Any => {
            blazingly_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_owned()))
        }
    }
}

fn missing_input(name: &str, source: InputSource) -> InputRejection {
    InputRejection {
        status: 422,
        code: "missing_input".to_owned(),
        message: format!("required {} input is missing", source_name(source)),
        details: Some(json!({
            "source": source_name(source),
            "name": name
        })),
    }
}

fn decode_rejection(name: &str, source: InputSource, reason: &str) -> InputRejection {
    decode_path_rejection(name, source, "", reason)
}

fn decode_path_rejection(
    name: &str,
    source: InputSource,
    path: &str,
    reason: &str,
) -> InputRejection {
    let (code, message) = if source == InputSource::Json {
        ("invalid_json", "request body is not valid JSON".to_owned())
    } else {
        (
            "invalid_input",
            format!("{} input could not be decoded", source_name(source)),
        )
    };
    let mut details = json!({
        "source": source_name(source),
        "name": name,
        "reason": reason
    });
    if !path.is_empty()
        && let Some(details) = details.as_object_mut()
    {
        // A decode failure and a rule failure describe the same field, so they
        // report one `violations` shape and one path syntax. `field` is kept
        // alongside it for readers that predate the unified shape.
        #[cfg(feature = "validation")]
        {
            let violations = blazingly_validation::decode_violations(path, reason);
            details.insert(
                "field".to_owned(),
                Value::String(blazingly_validation::normalize_field_path(path)),
            );
            if let Ok(rendered) = blazingly_json::to_value(violations.violations()) {
                details.insert("violations".to_owned(), rendered);
            }
        }
        #[cfg(not(feature = "validation"))]
        details.insert("field".to_owned(), Value::String(path.to_owned()));
    }
    InputRejection {
        status: 422,
        code: code.to_owned(),
        message,
        details: Some(details),
    }
}

const fn source_name(source: InputSource) -> &'static str {
    match source {
        InputSource::Path => "path",
        InputSource::Query => "query",
        InputSource::Header => "header",
        InputSource::Cookie => "cookie",
        InputSource::Json => "json",
        InputSource::Form => "form",
        InputSource::Multipart => "multipart",
        InputSource::File => "file",
        InputSource::Stream => "stream",
    }
}

#[macro_export]
macro_rules! routes {
    ($($operation:ident),* $(,)?) => {
        ::std::vec![$($operation::executable()),*]
    };
}
