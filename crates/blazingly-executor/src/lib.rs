#![forbid(unsafe_code)]

use base64::Engine;
use blazingly_core::{
    Accepted, ApiError, ApiModel, ApiSchema, App, AppDefinition, Cookie, Created, File, Form,
    Header, InputSource, Json, Multipart, NoContent, OperationDescriptor, OperationFailure,
    OperationId, Path, Query, ResponseBuildError, ResponseHeader, SchemaKind,
    SecuritySchemeDescriptor, Status, StreamingBody, TypeDescriptor, UploadFile, WithHeaders,
};
use blazingly_di::{
    CompiledProvider, DependencyError, DependencyLifetime, DependencyRequest, DependencySlot,
    DependencyValue, Depends, Provider,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

pub type OperationFuture = Pin<Box<dyn Future<Output = ExecutionOutcome> + 'static>>;
const INLINE_DEPENDENCY_SLOTS: usize = 8;
type Handler = Rc<
    dyn for<'input, 'dependencies> Fn(
            InvocationInput<'input>,
            &'dependencies ResolvedDependencies<'dependencies>,
        ) -> Result<OperationFuture, ExecutionOutcome>
        + 'static,
>;
type SingletonCompilation = (Vec<Option<DependencyValue>>, Vec<Option<usize>>);
type OperationHookFuture = Pin<Box<dyn Future<Output = Result<(), DependencyError>> + 'static>>;
type ResponseHookFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;
type OperationHook = Rc<dyn Fn(HookContext) -> OperationHookFuture>;
type ResponseHook = Rc<dyn Fn(HookContext, HookOutcome) -> ResponseHookFuture>;
type ShutdownHook = Rc<dyn Fn() -> OperationHookFuture>;
type AbortFuture = Pin<Box<dyn Future<Output = InvocationAbort> + 'static>>;

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
                let descriptor = T::type_descriptor();
                let parts = parse_multipart_request(*request)?;
                let value = multipart_argument_value(&parts, name, required, &descriptor)?;
                serde_json::from_value(value).map_err(|error| {
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
    },
    StreamingSuccess {
        status: u16,
        headers: Vec<ResponseHeader>,
        body: StreamingBody,
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
        !matches!(self, Self::Success { .. } | Self::StreamingSuccess { .. })
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

impl OperationOutput for NoContent {
    fn into_execution_outcome(self) -> ExecutionOutcome {
        ExecutionOutcome::Success {
            status: 204,
            headers: Vec::new(),
            body: None,
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
        }
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
            ExecutionOutcome::Rejected { .. }
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
            handler: Rc::new(move |input, _| {
                handler(input).map_err(InputRejection::into_execution_outcome)
            }),
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
            handler: Rc::new(handler),
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
        let requests = match plan.resolve().await {
            Ok(requests) => requests,
            Err(error) => return dependency_error_outcome(error),
        };
        let dependencies = ResolvedDependencies {
            singletons: &plan.singletons,
            requests: requests.as_slice(),
            slots: &plan.handler_slots,
        };
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
        let handler = match (self.handler)(input, &dependencies) {
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
        let handler = match (self.handler)(input, &dependencies) {
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
            ExecutionOutcome::Success { .. } | ExecutionOutcome::StreamingSuccess { .. }
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
    shutdown_hooks: Vec<ShutdownHook>,
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
    shutdown_hooks: Vec<ShutdownHook>,
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
        let mut shutdown_hooks = Vec::new();
        let mut collector = PluginCollector {
            registrations: &mut registrations,
            operations: &mut scoped_operations,
            security_schemes: &mut security_schemes,
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
    shutdown_hooks: &'collector mut Vec<ShutdownHook>,
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

fn serialize_success(status: u16, value: impl Serialize) -> ExecutionOutcome {
    match serde_json::to_vec(&value) {
        Ok(body) => ExecutionOutcome::Success {
            status,
            headers: Vec::new(),
            body: Some(body),
        },
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
        let mut properties = serde_json::Map::new();
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
        return serde_json::to_value(part.to_upload())
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
    serde_json::from_value(value.clone())
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

fn extract_argument<T>(
    input: &InvocationInput<'_>,
    name: &str,
    source: InputSource,
    required: bool,
) -> Result<T, InputRejection>
where
    T: ApiSchema + DeserializeOwned,
{
    let descriptor = T::type_descriptor();
    if let InvocationInput::Http(request) = input
        && source == InputSource::Json
    {
        let decoded = serde_json::from_slice::<T>(request.body())
            .map_err(|error| decode_rejection(name, source, &error.to_string()))?;
        return validate_decoded(decoded, source);
    }

    let value = match input {
        InvocationInput::Http(request) => {
            raw_argument_value(*request, name, source, required, &descriptor)?
        }
        InvocationInput::Arguments(arguments) => {
            structured_argument_value(arguments, name, source, required, &descriptor)?
        }
    };

    let decoded = serde_json::from_value::<T>(value)
        .map_err(|error| decode_rejection(name, source, &error.to_string()))?;
    validate_decoded(decoded, source)
}

fn validate_decoded<T: ApiSchema>(decoded: T, source: InputSource) -> Result<T, InputRejection> {
    decoded.validate_input().map_err(|errors| InputRejection {
        status: 422,
        code: "validation_error".to_owned(),
        message: format!("{} input failed validation", source_name(source)),
        details: serde_json::to_value(errors).ok(),
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
                raw_typed_value(request, source, &field.name, &field.ty)
                    .map(|value| (field.name.clone(), value))
            })
            .collect();
        let properties: serde_json::Map<String, Value> = properties;
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
        if value.as_object().is_some_and(serde_json::Map::is_empty) && !required {
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
                arguments
                    .get(&field.name)
                    .cloned()
                    .map(|value| (field.name.clone(), value))
            })
            .collect(),
    )
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
            serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_owned()))
        }
        SchemaKind::Object | SchemaKind::Any => {
            serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_owned()))
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
    let (code, message) = if source == InputSource::Json {
        ("invalid_json", "request body is not valid JSON".to_owned())
    } else {
        (
            "invalid_input",
            format!("{} input could not be decoded", source_name(source)),
        )
    };
    InputRejection {
        status: 422,
        code: code.to_owned(),
        message,
        details: Some(json!({
            "source": source_name(source),
            "name": name,
            "reason": reason
        })),
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
    }
}

#[macro_export]
macro_rules! routes {
    ($($operation:ident),* $(,)?) => {
        ::std::vec![$($operation::executable()),*]
    };
}
