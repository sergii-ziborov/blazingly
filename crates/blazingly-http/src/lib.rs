#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

use blazingly_core::{
    AppDefinition, BackgroundTask, BackgroundTaskError, BodyStreamError, HttpMethod, HttpUpgrade,
    InputSource, OperationDescriptor, ResponseHeader, SecuritySchemeDescriptor, StreamingBody,
};
use blazingly_executor::{
    DependencyError, ExecutableApp, ExecutionOutcome, FromInvocation,
    HttpRequestParts as InvocationRequestParts, InputRejection, InvocationControl, InvocationInput,
};
use blazingly_json::{Value, json};
use blazingly_openapi::{OpenApiAssetResponse, OpenApiConfig, OpenApiService};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::any::{Any, TypeId};
use std::borrow::Cow;
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::Future;
use std::hash::{BuildHasherDefault, Hasher};
use std::net::{IpAddr, SocketAddr};
use std::rc::Rc;
use std::str::Utf8Error;

pub const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;

/// Environment variable that turns server construction into a print-and-exit
/// introspection run. See [`HttpApp::new`] for the contract.
pub const EMIT_VARIABLE: &str = "BLAZINGLY_EMIT";

/// A runtime-neutral HTTP request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    method: HttpMethod,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
    peer_addr: Option<SocketAddr>,
    scheme: String,
}

/// Borrowed request access used by in-memory and native HTTP adapters.
///
/// Adapters can implement this trait directly over their receive buffer so
/// dispatch does not require copying the target, headers, or body.
pub trait HttpRequestView {
    fn method(&self) -> HttpMethod;
    fn target(&self) -> &str;
    fn header_value(&self, name: &str, index: usize) -> Option<&str>;
    fn body(&self) -> &[u8];

    /// Transfers a pull-based request body to the operation, when supported.
    fn take_body_stream(&self) -> Option<StreamingBody> {
        None
    }

    /// Address of the direct network peer, when known by the adapter.
    fn peer_addr(&self) -> Option<SocketAddr> {
        None
    }

    /// Original transport scheme before trusted proxy normalization.
    #[allow(clippy::unnecessary_literal_bound)]
    fn scheme(&self) -> &str {
        "http"
    }
}

/// Mutable, request-local context shared by runtime-neutral HTTP middleware.
///
/// The base request remains borrowed. Proxy middleware can replace the
/// effective client IP, scheme, and host without rewriting the adapter's
/// receive buffer. Typed extensions are allocated only when inserted.
pub struct HttpRequestContext<'request> {
    request: &'request dyn HttpRequestView,
    client_ip: Option<IpAddr>,
    scheme: Cow<'request, str>,
    host: Option<Cow<'request, str>>,
    extensions: Vec<(TypeId, Box<dyn Any>)>,
}

impl<'request> HttpRequestContext<'request> {
    fn new(request: &'request dyn HttpRequestView) -> Self {
        Self {
            request,
            client_ip: request.peer_addr().map(|address| address.ip()),
            scheme: Cow::Borrowed(request.scheme()),
            host: None,
            extensions: Vec::new(),
        }
    }

    #[must_use]
    pub fn request(&self) -> &dyn HttpRequestView {
        self.request
    }

    #[must_use]
    pub fn client_ip(&self) -> Option<IpAddr> {
        self.client_ip
    }

    pub fn set_client_ip(&mut self, client_ip: IpAddr) {
        self.client_ip = Some(client_ip);
    }

    #[must_use]
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    pub fn set_scheme(&mut self, scheme: impl Into<String>) {
        self.scheme = Cow::Owned(scheme.into());
    }

    #[must_use]
    pub fn host(&self) -> Option<&str> {
        self.host
            .as_deref()
            .or_else(|| self.request.header_value("host", 0))
    }

    pub fn set_host(&mut self, host: impl Into<String>) {
        self.host = Some(Cow::Owned(host.into()));
    }

    pub fn insert_extension<T: 'static>(&mut self, value: T) {
        let type_id = TypeId::of::<T>();
        if let Some((_, existing)) = self
            .extensions
            .iter_mut()
            .find(|(existing, _)| *existing == type_id)
        {
            *existing = Box::new(value);
        } else {
            self.extensions.push((type_id, Box::new(value)));
        }
    }

    #[must_use]
    pub fn extension<T: 'static>(&self) -> Option<&T> {
        self.extension_by_id(TypeId::of::<T>())?.downcast_ref()
    }

    /// Snapshots the normalized client IP, scheme, and host so the dispatch
    /// path can carry them into the operation request context.
    #[must_use]
    pub fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            client_ip: self.client_ip,
            scheme: self.scheme.as_ref().to_owned(),
            host: self.host().map(str::to_owned),
        }
    }

    fn extension_by_id(&self, type_id: TypeId) -> Option<&dyn Any> {
        self.extensions
            .iter()
            .find(|(existing, _)| *existing == type_id)
            .map(|(_, value)| value.as_ref())
    }
}

/// Normalized transport values readable by a handler extractor.
///
/// Dispatch exposes this as a request extension, so `Extension<ConnectionInfo>`
/// observes the values left by proxy middleware, or the raw adapter values when
/// no middleware runs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionInfo {
    client_ip: Option<IpAddr>,
    scheme: String,
    host: Option<String>,
}

impl ConnectionInfo {
    /// Effective client IP after trusted proxy normalization.
    #[must_use]
    pub const fn client_ip(&self) -> Option<IpAddr> {
        self.client_ip
    }

    /// Effective request scheme after trusted proxy normalization.
    #[must_use]
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// Effective host after trusted proxy normalization.
    #[must_use]
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    fn from_request(request: &(impl HttpRequestView + ?Sized)) -> Self {
        Self {
            client_ip: request.peer_addr().map(|address| address.ip()),
            scheme: request.scheme().to_owned(),
            host: request.header_value("host", 0).map(str::to_owned),
        }
    }
}

/// Synchronous middleware interception points shared by every HTTP adapter.
///
/// Middleware is deliberately runtime-neutral: cryptographic verification,
/// header policy, compression, and rate limiting do not require Tokio or any
/// other async runtime.
pub trait HttpMiddleware {
    /// Runs before routing. Returning a response short-circuits dispatch.
    fn on_request(&self, _context: &mut HttpRequestContext<'_>) -> Option<Response> {
        None
    }

    /// Runs after routing and before body parsing/handler invocation.
    fn on_operation(
        &self,
        _context: &mut HttpRequestContext<'_>,
        _operation: &OperationDescriptor,
        _security_schemes: &[SecuritySchemeDescriptor],
    ) -> Option<Response> {
        None
    }

    /// Runs in reverse registration order for normal and short-circuit
    /// responses.
    fn on_response(
        &self,
        _context: &HttpRequestContext<'_>,
        _operation: Option<&OperationDescriptor>,
        _response: &mut Response,
    ) {
    }

    /// Returns whether this layer can verify contract security requirements.
    ///
    /// Dispatch fails closed when an operation declares a security scheme and
    /// no registered layer can verify it. The default is `true` so an unknown
    /// layer is assumed capable and never turns the guard into a false 500;
    /// layers that never authenticate should return `false` so a dispatch path
    /// without a verifier is detected.
    fn verifies_security(&self) -> bool {
        true
    }
}

type OperationFilter = Rc<dyn Fn(&str) -> bool>;

#[derive(Clone)]
enum OperationPredicate {
    Exact(Box<str>),
    Prefix(Box<str>),
    Filter(OperationFilter),
}

impl OperationPredicate {
    fn matches(&self, operation_id: &str) -> bool {
        match self {
            Self::Exact(expected) => operation_id == expected.as_ref(),
            Self::Prefix(prefix) => operation_id.starts_with(prefix.as_ref()),
            Self::Filter(filter) => filter(operation_id),
        }
    }
}

/// Selects the requests one registered middleware layer observes.
///
/// An empty scope matches every request, which is what
/// [`HttpApp::with_middleware`] registers. Path prefixes and operation
/// predicates combine as `AND` between the two categories and `OR` inside one
/// category.
///
/// The selected operation is unknown before routing, so a scope that declares
/// an operation predicate never matches [`HttpMiddleware::on_request`]; that
/// layer sees [`HttpMiddleware::on_operation`] and
/// [`HttpMiddleware::on_response`] instead. A layer whose scope does not match
/// is also not counted by the security guard, so a scoped verifier cannot
/// silently authorize an operation outside its subtree.
#[derive(Clone, Default)]
pub struct MiddlewareScope {
    prefixes: Vec<Box<str>>,
    operations: Vec<OperationPredicate>,
}

impl MiddlewareScope {
    /// A scope that constrains nothing.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            prefixes: Vec::new(),
            operations: Vec::new(),
        }
    }

    /// A scope limited to one path prefix.
    #[must_use]
    pub fn prefix(prefix: &str) -> Self {
        Self::all().with_prefix(prefix)
    }

    /// A scope limited to one operation id.
    #[must_use]
    pub fn operation(operation_id: &str) -> Self {
        Self::all().with_operation(operation_id)
    }

    /// Adds an accepted path prefix, matched on segment boundaries.
    ///
    /// `/ingest` matches `/ingest` and `/ingest/events`, never `/ingested`.
    #[must_use]
    pub fn with_prefix(mut self, prefix: &str) -> Self {
        self.prefixes.push(normalize_prefix(prefix));
        self
    }

    /// Adds one accepted operation id.
    #[must_use]
    pub fn with_operation(mut self, operation_id: &str) -> Self {
        self.operations
            .push(OperationPredicate::Exact(Box::from(operation_id)));
        self
    }

    /// Adds an accepted operation id prefix, for id namespaces such as
    /// `ingest.`.
    #[must_use]
    pub fn with_operation_prefix(mut self, prefix: &str) -> Self {
        self.operations
            .push(OperationPredicate::Prefix(Box::from(prefix)));
        self
    }

    /// Adds an operation id predicate for a selection the other constraints
    /// cannot express.
    #[must_use]
    pub fn with_operation_filter<Filter>(mut self, filter: Filter) -> Self
    where
        Filter: Fn(&str) -> bool + 'static,
    {
        self.operations
            .push(OperationPredicate::Filter(Rc::new(filter)));
        self
    }

    /// Returns whether this scope constrains nothing.
    #[must_use]
    pub fn is_global(&self) -> bool {
        self.prefixes.is_empty() && self.operations.is_empty()
    }

    /// Matches before routing, when only the request path is known.
    #[must_use]
    pub fn matches_request(&self, path: &str) -> bool {
        self.operations.is_empty() && self.matches_path(path)
    }

    /// Matches after routing, when the selected operation is known.
    #[must_use]
    pub fn matches_operation(&self, path: &str, operation_id: &str) -> bool {
        self.matches_path(path)
            && (self.operations.is_empty()
                || self
                    .operations
                    .iter()
                    .any(|predicate| predicate.matches(operation_id)))
    }

    fn matches_path(&self, path: &str) -> bool {
        self.prefixes.is_empty()
            || self
                .prefixes
                .iter()
                .any(|prefix| path_has_prefix(path, prefix))
    }
}

impl fmt::Debug for MiddlewareScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MiddlewareScope")
            .field("prefixes", &self.prefixes)
            .field("operations", &self.operations.len())
            .finish()
    }
}

fn normalize_prefix(prefix: &str) -> Box<str> {
    let trimmed = prefix.trim_end_matches('/');
    if trimmed.is_empty() {
        Box::from("")
    } else if trimmed.starts_with('/') {
        Box::from(trimmed)
    } else {
        Box::from(format!("/{trimmed}"))
    }
}

fn path_has_prefix(path: &str, prefix: &str) -> bool {
    let Some(rest) = path.strip_prefix(prefix) else {
        return false;
    };
    rest.is_empty() || rest.starts_with('/')
}

struct ScopedMiddleware {
    scope: MiddlewareScope,
    layer: Rc<dyn HttpMiddleware>,
}

impl ScopedMiddleware {
    fn matches(&self, path: &str, operation: Option<&OperationDescriptor>) -> bool {
        operation.map_or_else(
            || self.scope.matches_request(path),
            |operation| {
                self.scope
                    .matches_operation(path, operation.contract.id.as_str())
            },
        )
    }
}

/// Where a failure response produced by dispatch came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpErrorSource {
    /// No route matched the request method and path.
    Routing,
    /// Dispatch rejected the request before the operation ran.
    Request,
    /// Argument extraction or model validation rejected the request.
    Rejection,
    /// A typed `#[api_error]` variant declared by the operation contract.
    Domain,
    /// A server-side failure, including a security scheme no registered layer
    /// can verify.
    Internal,
}

/// The failure an application error handler is asked to rewrite.
pub struct HttpError<'dispatch> {
    source: HttpErrorSource,
    status: u16,
    code: &'dispatch str,
    message: &'dispatch str,
    method: HttpMethod,
    path: &'dispatch str,
    operation: Option<&'dispatch OperationDescriptor>,
}

impl HttpError<'_> {
    /// Origin of this failure.
    #[must_use]
    pub const fn source(&self) -> HttpErrorSource {
        self.source
    }

    /// Status of the response dispatch built before any handler ran.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Stable error code of the response dispatch built.
    #[must_use]
    pub const fn code(&self) -> &str {
        self.code
    }

    /// Message of the response dispatch built.
    #[must_use]
    pub const fn message(&self) -> &str {
        self.message
    }

    /// Request method.
    #[must_use]
    pub const fn method(&self) -> HttpMethod {
        self.method
    }

    /// Request path without its query string.
    #[must_use]
    pub const fn path(&self) -> &str {
        self.path
    }

    /// Operation the router selected, when the failure happened after routing.
    #[must_use]
    pub const fn operation(&self) -> Option<&OperationDescriptor> {
        self.operation
    }
}

/// Application-level rewriting of failure responses.
///
/// Handlers run in registration order after dispatch has produced a failure
/// response and before middleware `on_response`, so an application can give
/// every failure one house style without touching the typed errors each
/// operation declares.
///
/// The status of a [`HttpErrorSource::Domain`] failure is restored after the
/// handlers run: a `#[api_error]` variant publishes its status in the contract,
/// and this seam must not make that published status a lie. Body, headers, and
/// the status of every other source are the handler's to change.
///
/// A response a middleware layer returns to short-circuit dispatch is that
/// layer's own and does not reach these handlers; the layer shapes it in
/// [`HttpMiddleware::on_response`].
pub trait HttpErrorHandler {
    /// Rewrites one failure response.
    fn on_error(&self, error: &HttpError<'_>, response: &mut Response);
}

/// A request-scoped handle for scheduling work that runs after the response.
///
/// A handler injects it with `Extension<BackgroundTasks>` and schedules work
/// anywhere in its body, instead of restructuring its return type around
/// [`Background<T>`](blazingly_core::Background).
///
/// A scheduled task is attached to the response dispatch produces whatever the
/// outcome is, including a rejection, a typed domain error, and an aborted
/// invocation, so an adapter runs it once the body has been written. This is
/// the one behavioral difference from `Background<T>`, whose tasks ride on a
/// success value and are therefore discarded when the operation fails.
#[derive(Clone, Debug, Default)]
pub struct BackgroundTasks {
    tasks: Rc<RefCell<Vec<BackgroundTask>>>,
}

impl BackgroundTasks {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Schedules a prepared task.
    pub fn add_task(&self, task: BackgroundTask) {
        self.tasks.borrow_mut().push(task);
    }

    /// Schedules a fallible after-response task.
    pub fn add<Task, TaskFuture>(&self, task: Task)
    where
        Task: FnOnce() -> TaskFuture + 'static,
        TaskFuture: Future<Output = Result<(), BackgroundTaskError>> + 'static,
    {
        self.add_task(BackgroundTask::new(task));
    }

    /// Schedules an after-response task that cannot fail.
    pub fn add_infallible<Task, TaskFuture>(&self, task: Task)
    where
        Task: FnOnce() -> TaskFuture + 'static,
        TaskFuture: Future<Output = ()> + 'static,
    {
        self.add_task(BackgroundTask::infallible(task));
    }

    /// Number of tasks scheduled so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tasks.borrow().len()
    }

    /// Returns whether nothing has been scheduled yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tasks.borrow().is_empty()
    }

    /// Takes the scheduled tasks, leaving the handle empty.
    #[must_use]
    pub fn take(&self) -> Vec<BackgroundTask> {
        std::mem::take(&mut self.tasks.borrow_mut())
    }
}

impl FromInvocation for BackgroundTasks {
    fn from_invocation(
        input: &InvocationInput<'_>,
        name: &str,
        _required: bool,
    ) -> Result<Self, InputRejection> {
        let InvocationInput::Http(request) = input else {
            return Err(InputRejection::new(
                500,
                "background_tasks_transport_mismatch",
                "after-response tasks are available only through HTTP",
            ));
        };
        request
            .extension(TypeId::of::<Self>())
            .and_then(<dyn Any>::downcast_ref::<Self>)
            .cloned()
            .ok_or_else(|| {
                InputRejection::new(
                    500,
                    "background_tasks_unavailable",
                    format!("this transport installed no after-response tasks for `{name}`"),
                )
            })
    }
}

impl Request {
    #[must_use]
    pub fn new(method: HttpMethod, target: impl Into<String>) -> Self {
        Self {
            method,
            target: target.into(),
            headers: BTreeMap::new(),
            body: Vec::new(),
            peer_addr: None,
            scheme: "http".to_owned(),
        }
    }

    #[must_use]
    pub fn get(target: impl Into<String>) -> Self {
        Self::new(HttpMethod::Get, target)
    }

    #[must_use]
    pub fn head(target: impl Into<String>) -> Self {
        Self::new(HttpMethod::Head, target)
    }

    #[must_use]
    pub fn post(target: impl Into<String>) -> Self {
        Self::new(HttpMethod::Post, target)
    }

    #[must_use]
    pub fn put(target: impl Into<String>) -> Self {
        Self::new(HttpMethod::Put, target)
    }

    #[must_use]
    pub fn patch(target: impl Into<String>) -> Self {
        Self::new(HttpMethod::Patch, target)
    }

    #[must_use]
    pub fn delete(target: impl Into<String>) -> Self {
        Self::new(HttpMethod::Delete, target)
    }

    #[must_use]
    pub fn options(target: impl Into<String>) -> Self {
        Self::new(HttpMethod::Options, target)
    }

    #[must_use]
    pub fn trace(target: impl Into<String>) -> Self {
        Self::new(HttpMethod::Trace, target)
    }

    #[must_use]
    pub fn connect(target: impl Into<String>) -> Self {
        Self::new(HttpMethod::Connect, target)
    }

    #[must_use]
    pub const fn method(&self) -> HttpMethod {
        self.method
    }

    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    #[must_use]
    pub fn path(&self) -> &str {
        self.target
            .split_once('?')
            .map_or(self.target.as_str(), |(path, _)| path)
    }

    #[must_use]
    pub fn header(mut self, name: impl AsRef<str>, value: impl Into<String>) -> Self {
        self.headers
            .insert(normalize_header_name(name.as_ref()), value.into());
        self
    }

    #[must_use]
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// Sets the direct peer address for in-memory adapter tests.
    #[must_use]
    pub const fn peer_addr(mut self, peer_addr: SocketAddr) -> Self {
        self.peer_addr = Some(peer_addr);
        self
    }

    /// Sets the original transport scheme for in-memory adapter tests.
    #[must_use]
    pub fn scheme(mut self, scheme: impl Into<String>) -> Self {
        self.scheme = scheme.into();
        self
    }

    /// Serializes a JSON body and sets its media type.
    ///
    /// # Errors
    ///
    /// Returns the serialization error if `value` cannot be encoded as JSON.
    pub fn json(mut self, value: &impl Serialize) -> Result<Self, blazingly_json::Error> {
        self.body = blazingly_json::to_vec(value)?;
        self.headers
            .insert("content-type".to_owned(), "application/json".to_owned());
        Ok(self)
    }

    #[must_use]
    pub fn get_header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(name)
            .or_else(|| {
                self.headers
                    .iter()
                    .find(|(header, _)| header.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value)
            })
            .map(String::as_str)
    }

    #[must_use]
    pub fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }

    #[must_use]
    pub fn body_bytes(&self) -> &[u8] {
        &self.body
    }
}

impl HttpRequestView for Request {
    fn method(&self) -> HttpMethod {
        self.method()
    }

    fn target(&self) -> &str {
        self.target()
    }

    fn header_value(&self, name: &str, index: usize) -> Option<&str> {
        self.headers
            .iter()
            .filter(|(header, _)| header_name_matches(header, name))
            .nth(index)
            .map(|(_, value)| value.as_str())
    }

    fn body(&self) -> &[u8] {
        self.body_bytes()
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer_addr
    }

    fn scheme(&self) -> &str {
        &self.scheme
    }
}

/// A runtime-neutral HTTP response.
#[derive(Debug)]
pub struct Response {
    status: u16,
    headers: ResponseHeaders,
    body: Vec<u8>,
    stream: Option<StreamingBody>,
    upgrade: Option<HttpUpgrade>,
    background: Vec<BackgroundTask>,
}

impl Response {
    /// Creates a buffered response with no headers.
    #[must_use]
    pub fn from_bytes(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: ResponseHeaders::empty(),
            body: body.into(),
            stream: None,
            upgrade: None,
            background: Vec::new(),
        }
    }

    /// Creates an empty buffered response.
    #[must_use]
    pub fn empty(status: u16) -> Self {
        Self::from_bytes(status, Vec::new())
    }

    /// Creates the canonical response used when an adapter rejects an
    /// oversized body before dispatch.
    #[must_use]
    pub fn payload_too_large(max_body_bytes: usize) -> Self {
        BodyRejection::PayloadTooLarge { max_body_bytes }.into_response()
    }

    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Replaces the response status.
    pub const fn set_status(&mut self, status: u16) {
        self.status = status;
    }

    #[must_use]
    pub fn get_header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }

    pub fn headers(&self) -> impl Iterator<Item = (&str, &str)> {
        self.headers.iter()
    }

    /// Inserts or replaces a response header. `Set-Cookie` is appended so
    /// independent cookie mutations are preserved.
    pub fn set_header(&mut self, name: impl AsRef<str>, value: impl Into<String>) {
        self.headers.insert(
            Cow::Owned(normalize_header_name(name.as_ref())),
            Cow::Owned(value.into()),
        );
    }

    /// Removes every response header with the supplied name.
    pub fn remove_header(&mut self, name: &str) {
        self.headers.remove(name);
    }

    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Replaces a buffered response body. Streaming responses are left
    /// untouched and return `false`.
    pub fn replace_body(&mut self, body: impl Into<Vec<u8>>) -> bool {
        if self.stream.is_some() {
            return false;
        }
        self.body = body.into();
        true
    }

    /// Returns whether this response owns a pull-based streaming body.
    #[must_use]
    pub const fn is_streaming(&self) -> bool {
        self.stream.is_some()
    }

    /// Takes the pull-based streaming body, leaving the response buffered.
    ///
    /// A middleware layer that transforms a streamed body, such as a
    /// chunk-wise content encoder, takes the source stream here and installs
    /// its wrapper with [`Response::set_body_stream`].
    #[must_use]
    pub fn take_body_stream(&mut self) -> Option<StreamingBody> {
        self.stream.take()
    }

    /// Installs a pull-based streaming body, discarding any buffered bytes.
    ///
    /// The caller owns the framing headers: a streamed body has no known
    /// length, so `content-length` must be removed rather than left stale.
    pub fn set_body_stream(&mut self, stream: StreamingBody) {
        self.body.clear();
        self.stream = Some(stream);
    }

    #[must_use]
    pub const fn is_upgrade(&self) -> bool {
        self.upgrade.is_some()
    }

    /// Takes ownership of a validated one-shot protocol upgrade.
    pub fn take_upgrade(&mut self) -> Option<HttpUpgrade> {
        self.upgrade.take()
    }

    /// Takes the after-response tasks so a network adapter can schedule them
    /// after the body has been written.
    pub fn take_background_tasks(&mut self) -> Vec<BackgroundTask> {
        std::mem::take(&mut self.background)
    }

    /// Runs after-response tasks sequentially and gives every task a chance to
    /// complete.
    ///
    /// # Errors
    ///
    /// Returns the first task failure after all tasks have run.
    pub async fn run_background(&mut self) -> Result<(), BackgroundTaskError> {
        let mut first_error = None;
        for task in self.take_background_tasks() {
            if let Err(error) = task.run().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Returns the wire body length when it is known before streaming starts.
    #[must_use]
    pub fn exact_body_length(&self) -> Option<u64> {
        self.stream.as_ref().map_or_else(
            || u64::try_from(self.body.len()).ok(),
            StreamingBody::exact_length,
        )
    }

    /// Pulls one streaming body chunk.
    ///
    /// Calling this method is the backpressure demand boundary. Buffered
    /// responses return `None`; use [`Self::body`] for their bytes.
    pub async fn next_body_chunk(&mut self) -> Option<Result<Vec<u8>, BodyStreamError>> {
        match self.stream.as_mut() {
            Some(stream) => stream.next_chunk().await,
            None => None,
        }
    }

    /// Collects buffered or streaming bytes with an explicit memory bound.
    ///
    /// This is intended for tests and clients that deliberately want to turn a
    /// stream back into one allocation. Network adapters should pull and write
    /// chunks directly.
    ///
    /// # Errors
    ///
    /// Returns a producer error or [`CollectBodyError::LimitExceeded`].
    pub async fn collect_body(mut self, limit: usize) -> Result<Vec<u8>, CollectBodyError> {
        if self.stream.is_none() {
            if self.body.len() > limit {
                return Err(CollectBodyError::LimitExceeded { limit });
            }
            return Ok(self.body);
        }

        let initial_capacity = self
            .exact_body_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0)
            .min(limit);
        let mut body = Vec::with_capacity(initial_capacity);
        while let Some(chunk) = self.next_body_chunk().await {
            let chunk = chunk.map_err(CollectBodyError::Stream)?;
            if body.len().saturating_add(chunk.len()) > limit {
                return Err(CollectBodyError::LimitExceeded { limit });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Decodes the response body as UTF-8.
    ///
    /// # Errors
    ///
    /// Returns an error when the response body is not valid UTF-8.
    pub fn text(&self) -> Result<&str, Utf8Error> {
        std::str::from_utf8(&self.body)
    }

    /// Deserializes the response body as JSON.
    ///
    /// # Errors
    ///
    /// Returns an error when the response is not valid JSON for `T`.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, blazingly_json::Error> {
        blazingly_json::from_slice(&self.body)
    }
}

/// Failure while deliberately buffering an HTTP response stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollectBodyError {
    Stream(BodyStreamError),
    LimitExceeded { limit: usize },
}

impl fmt::Display for CollectBodyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stream(error) => error.fmt(formatter),
            Self::LimitExceeded { limit } => {
                write!(
                    formatter,
                    "response body exceeds the {limit}-byte collection limit"
                )
            }
        }
    }
}

impl std::error::Error for CollectBodyError {}

/// An owned, runtime-neutral HTTP application compiled from the operation graph.
///
/// Network adapters own this type so the router is compiled once and request
/// dispatch stays independent from the selected socket runtime.
pub struct HttpApp {
    app: ExecutableApp,
    router: Router,
    max_body_bytes: usize,
    openapi: Option<OpenApiService>,
    middleware: Vec<ScopedMiddleware>,
    error_handlers: Vec<Rc<dyn HttpErrorHandler>>,
    allow_unverified_security_schemes: bool,
}

impl HttpApp {
    /// Compiles the router for an owned application.
    ///
    /// # Introspection contract
    ///
    /// When the [`EMIT_VARIABLE`] environment variable (`BLAZINGLY_EMIT`) is
    /// set to `openapi` or `routes`, construction prints the `OpenAPI`
    /// document or the operation table to stdout and terminates the process
    /// with exit code 0 instead of returning, before any socket is served.
    /// Every native serving path constructs an `HttpApp`, so
    /// `cargo blazingly openapi` and `cargo blazingly routes` run an
    /// unmodified application binary as a printer through this seam. The exit
    /// inside a constructor is deliberate and is part of the CLI contract.
    ///
    /// The document is rendered with [`OpenApiConfig::default`]; an
    /// application's own [`HttpApp::with_openapi`] configuration is not known
    /// at construction time. Any other non-empty value terminates with exit
    /// code 2, so a typo never falls through to serving. An unset or empty
    /// variable leaves construction unaffected, and [`TestApp`] never
    /// consults the variable.
    #[must_use]
    pub fn new(app: ExecutableApp) -> Self {
        emit_and_exit_if_requested(&app);
        let router = Router::new(&app);
        Self {
            app,
            router,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            openapi: None,
            middleware: Vec::new(),
            error_handlers: Vec::new(),
            allow_unverified_security_schemes: false,
        }
    }

    #[must_use]
    pub const fn with_max_body_bytes(mut self, max_body_bytes: usize) -> Self {
        self.max_body_bytes = max_body_bytes;
        self
    }

    /// Allows operations that declare a security scheme to run without a
    /// registered verifier. Defaults to `false`.
    ///
    /// Passing `true` disables the fail-closed guard: a declared scheme then
    /// only documents the contract and this adapter performs no
    /// authentication. Enable it for tests, or for an application that
    /// deliberately enforces the scheme outside this dispatch path.
    #[must_use]
    pub const fn with_unverified_security_schemes(mut self, allow: bool) -> Self {
        self.allow_unverified_security_schemes = allow;
        self
    }

    /// Runs compiled application startup hooks.
    ///
    /// # Errors
    ///
    /// Returns the first startup failure.
    pub async fn startup(&self) -> Result<(), DependencyError> {
        self.app.startup().await
    }

    /// Runs compiled application shutdown hooks.
    ///
    /// # Errors
    ///
    /// Returns the first cleanup failure after all shutdown hooks have run.
    pub async fn shutdown(&self) -> Result<(), DependencyError> {
        self.app.shutdown().await
    }

    /// Mounts precompiled `OpenAPI` JSON and UI assets.
    #[must_use]
    pub fn with_openapi(mut self, config: OpenApiConfig) -> Self {
        self.openapi = Some(OpenApiService::new(self.app.definition(), config));
        self
    }

    /// Registers runtime-neutral HTTP middleware for every request.
    #[must_use]
    pub fn with_middleware(mut self, middleware: impl HttpMiddleware + 'static) -> Self {
        self.middleware.push(ScopedMiddleware {
            scope: MiddlewareScope::all(),
            layer: Rc::new(middleware),
        });
        self
    }

    /// Registers shared middleware state for every request.
    #[must_use]
    pub fn with_shared_middleware(mut self, middleware: Rc<dyn HttpMiddleware>) -> Self {
        self.middleware.push(ScopedMiddleware {
            scope: MiddlewareScope::all(),
            layer: middleware,
        });
        self
    }

    /// Registers runtime-neutral HTTP middleware for one path prefix or
    /// operation selection.
    #[must_use]
    pub fn with_scoped_middleware(
        mut self,
        scope: MiddlewareScope,
        middleware: impl HttpMiddleware + 'static,
    ) -> Self {
        self.middleware.push(ScopedMiddleware {
            scope,
            layer: Rc::new(middleware),
        });
        self
    }

    /// Registers shared middleware state for one path prefix or operation
    /// selection.
    #[must_use]
    pub fn with_shared_scoped_middleware(
        mut self,
        scope: MiddlewareScope,
        middleware: Rc<dyn HttpMiddleware>,
    ) -> Self {
        self.middleware.push(ScopedMiddleware {
            scope,
            layer: middleware,
        });
        self
    }

    /// Registers an application-level handler for failure responses.
    #[must_use]
    pub fn with_error_handler(mut self, handler: impl HttpErrorHandler + 'static) -> Self {
        self.error_handlers.push(Rc::new(handler));
        self
    }

    /// Registers shared application-level error handler state.
    #[must_use]
    pub fn with_shared_error_handler(mut self, handler: Rc<dyn HttpErrorHandler>) -> Self {
        self.error_handlers.push(handler);
        self
    }

    pub async fn call(&self, request: Request) -> Response {
        self.call_view(&request).await
    }

    pub async fn call_view(&self, request: &impl HttpRequestView) -> Response {
        let dispatcher = self.dispatcher();
        dispatcher.dispatch(request, None).await
    }

    pub async fn call_view_controlled(
        &self,
        request: &impl HttpRequestView,
        control: InvocationControl,
    ) -> Response {
        let dispatcher = self.dispatcher();
        dispatcher.dispatch(request, Some(control)).await
    }

    fn dispatcher(&self) -> Dispatcher<'_> {
        Dispatcher {
            app: &self.app,
            router: &self.router,
            max_body_bytes: self.max_body_bytes,
            openapi: self.openapi.as_ref(),
            middleware: &self.middleware,
            error_handlers: &self.error_handlers,
            allow_unverified_security_schemes: self.allow_unverified_security_schemes,
        }
    }

    /// Returns the compiled body source for a recognized request.
    ///
    /// Native adapters use this before buffering a body so streaming
    /// operations can start as soon as the request head is validated.
    #[must_use]
    pub fn request_body_source(&self, method: HttpMethod, target: &str) -> Option<InputSource> {
        let path = target.split_once('?').map_or(target, |(path, _)| path);
        self.router
            .recognize(method, path)
            .ok()
            .and_then(|route| route.body_source())
    }
}

/// Prints the requested introspection output and terminates the process when
/// [`EMIT_VARIABLE`] is set. Runs on every [`HttpApp::new`] call; a normal
/// serving process returns immediately from the unset-variable check.
///
/// A multicore server constructs one `HttpApp` per worker, so the emission is
/// guarded by a process-wide [`std::sync::Once`]: exactly one thread prints,
/// every caller exits.
fn emit_and_exit_if_requested(app: &ExecutableApp) {
    static EMITTED: std::sync::Once = std::sync::Once::new();
    let Some(mode) = std::env::var_os(EMIT_VARIABLE) else {
        return;
    };
    if mode.is_empty() {
        return;
    }
    let mut code = 0;
    EMITTED.call_once(|| code = emit(app, &mode.to_string_lossy()));
    std::process::exit(code);
}

/// Writes one introspection document to stdout and returns the process exit
/// code: 0 on success, 1 on a serialization or write failure, 2 on an
/// unrecognized mode.
fn emit(app: &ExecutableApp, mode: &str) -> i32 {
    use std::io::Write as _;
    let output = match mode {
        "openapi" => match openapi_document_text(app.definition()) {
            Ok(document) => document,
            Err(error) => {
                eprintln!("error: the OpenAPI document could not be serialized: {error}");
                return 1;
            }
        },
        "routes" => routes_table_text(app.definition()),
        unknown => {
            eprintln!("error: {EMIT_VARIABLE} must be `openapi` or `routes`, not `{unknown}`");
            return 2;
        }
    };
    let mut stdout = std::io::stdout().lock();
    if stdout.write_all(output.as_bytes()).is_err() || stdout.flush().is_err() {
        return 1;
    }
    0
}

/// The pretty-printed `OpenAPI` document emitted for `BLAZINGLY_EMIT=openapi`.
fn openapi_document_text(definition: &AppDefinition) -> Result<String, blazingly_json::Error> {
    let document = blazingly_openapi::to_value(definition);
    let mut text = blazingly_json::to_string_pretty(&document)?;
    text.push('\n');
    Ok(text)
}

/// The tab-separated operation table emitted for `BLAZINGLY_EMIT=routes`.
fn routes_table_text(definition: &AppDefinition) -> String {
    use std::fmt::Write as _;
    let mut table = String::from("METHOD\tPATH\tOPERATION\tSUMMARY\n");
    for operation in definition.operations() {
        let _ = writeln!(
            table,
            "{}\t{}\t{}\t{}",
            operation.http.method.as_str(),
            operation.http.path,
            operation.contract.id.as_str(),
            operation.contract.summary
        );
    }
    table
}

/// An in-memory borrowed HTTP adapter over the shared executable operation graph.
pub struct TestApp<'app> {
    app: &'app ExecutableApp,
    router: Router,
    max_body_bytes: usize,
    openapi: Option<OpenApiService>,
    middleware: Vec<ScopedMiddleware>,
    error_handlers: Vec<Rc<dyn HttpErrorHandler>>,
    allow_unverified_security_schemes: bool,
}

impl<'app> TestApp<'app> {
    #[must_use]
    pub fn new(app: &'app ExecutableApp) -> Self {
        Self {
            app,
            router: Router::new(app),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            openapi: None,
            middleware: Vec::new(),
            error_handlers: Vec::new(),
            allow_unverified_security_schemes: false,
        }
    }

    #[must_use]
    pub const fn with_max_body_bytes(mut self, max_body_bytes: usize) -> Self {
        self.max_body_bytes = max_body_bytes;
        self
    }

    /// Allows operations that declare a security scheme to run without a
    /// registered verifier. Defaults to `false`.
    ///
    /// Passing `true` disables the fail-closed guard, so an in-memory test can
    /// exercise a secured operation without registering a verifier.
    #[must_use]
    pub const fn with_unverified_security_schemes(mut self, allow: bool) -> Self {
        self.allow_unverified_security_schemes = allow;
        self
    }

    /// Runs application startup hooks for an in-memory test lifespan.
    ///
    /// # Errors
    ///
    /// Returns the first startup failure.
    pub async fn startup(&self) -> Result<(), DependencyError> {
        self.app.startup().await
    }

    /// Runs application shutdown hooks for an in-memory test lifespan.
    ///
    /// # Errors
    ///
    /// Returns the first cleanup failure after all shutdown hooks have run.
    pub async fn shutdown(&self) -> Result<(), DependencyError> {
        self.app.shutdown().await
    }

    /// Mounts precompiled `OpenAPI` JSON and UI assets in the in-memory adapter.
    #[must_use]
    pub fn with_openapi(mut self, config: OpenApiConfig) -> Self {
        self.openapi = Some(OpenApiService::new(self.app.definition(), config));
        self
    }

    /// Registers runtime-neutral HTTP middleware for every request.
    #[must_use]
    pub fn with_middleware(mut self, middleware: impl HttpMiddleware + 'static) -> Self {
        self.middleware.push(ScopedMiddleware {
            scope: MiddlewareScope::all(),
            layer: Rc::new(middleware),
        });
        self
    }

    /// Registers shared middleware state for every request.
    #[must_use]
    pub fn with_shared_middleware(mut self, middleware: Rc<dyn HttpMiddleware>) -> Self {
        self.middleware.push(ScopedMiddleware {
            scope: MiddlewareScope::all(),
            layer: middleware,
        });
        self
    }

    /// Registers runtime-neutral HTTP middleware for one path prefix or
    /// operation selection.
    #[must_use]
    pub fn with_scoped_middleware(
        mut self,
        scope: MiddlewareScope,
        middleware: impl HttpMiddleware + 'static,
    ) -> Self {
        self.middleware.push(ScopedMiddleware {
            scope,
            layer: Rc::new(middleware),
        });
        self
    }

    /// Registers shared middleware state for one path prefix or operation
    /// selection.
    #[must_use]
    pub fn with_shared_scoped_middleware(
        mut self,
        scope: MiddlewareScope,
        middleware: Rc<dyn HttpMiddleware>,
    ) -> Self {
        self.middleware.push(ScopedMiddleware {
            scope,
            layer: middleware,
        });
        self
    }

    /// Registers an application-level handler for failure responses.
    #[must_use]
    pub fn with_error_handler(mut self, handler: impl HttpErrorHandler + 'static) -> Self {
        self.error_handlers.push(Rc::new(handler));
        self
    }

    /// Registers shared application-level error handler state.
    #[must_use]
    pub fn with_shared_error_handler(mut self, handler: Rc<dyn HttpErrorHandler>) -> Self {
        self.error_handlers.push(handler);
        self
    }

    pub async fn call(&self, request: Request) -> Response {
        let dispatcher = self.dispatcher();
        dispatcher.dispatch(&request, None).await
    }

    pub async fn call_controlled(&self, request: Request, control: InvocationControl) -> Response {
        let dispatcher = self.dispatcher();
        dispatcher.dispatch(&request, Some(control)).await
    }

    fn dispatcher(&self) -> Dispatcher<'_> {
        Dispatcher {
            app: self.app,
            router: &self.router,
            max_body_bytes: self.max_body_bytes,
            openapi: self.openapi.as_ref(),
            middleware: &self.middleware,
            error_handlers: &self.error_handlers,
            allow_unverified_security_schemes: self.allow_unverified_security_schemes,
        }
    }
}

/// Compiled dispatch inputs shared by the owned and borrowed HTTP adapters.
struct Dispatcher<'app> {
    app: &'app ExecutableApp,
    router: &'app Router,
    max_body_bytes: usize,
    openapi: Option<&'app OpenApiService>,
    middleware: &'app [ScopedMiddleware],
    error_handlers: &'app [Rc<dyn HttpErrorHandler>],
    allow_unverified_security_schemes: bool,
}

/// The request coordinates every scoped layer and error handler is selected by.
struct DispatchSite<'dispatch> {
    method: HttpMethod,
    path: &'dispatch str,
    operation: Option<&'dispatch OperationDescriptor>,
}

impl Dispatcher<'_> {
    async fn dispatch<RequestView>(
        &self,
        request: &RequestView,
        control: Option<InvocationControl>,
    ) -> Response
    where
        RequestView: HttpRequestView,
    {
        if self.middleware.is_empty() {
            return self.dispatch_unlayered(request, control).await;
        }
        let middleware = self.middleware;
        let target = request.target();
        let mut site = DispatchSite {
            method: request.method(),
            path: target.split_once('?').map_or(target, |(path, _)| path),
            operation: None,
        };
        let mut context = HttpRequestContext::new(request);
        for layer in middleware {
            if layer.scope.matches_request(site.path)
                && let Some(response) = layer.layer.on_request(&mut context)
            {
                return complete_response(middleware, &context, &site, response);
            }
        }

        if validate_url_encoding(target).is_err() {
            let response = self.fail(&site, invalid_url_encoding_failure());
            return complete_response(middleware, &context, &site, response);
        }
        if let Some(response) = self
            .openapi
            .and_then(|service| service.handle(site.method, site.path))
        {
            return complete_response(middleware, &context, &site, openapi_response(response));
        }
        let recognized = match self.router.recognize(site.method, site.path) {
            Ok(recognized) => recognized,
            Err(error) => {
                let response = self.fail(&site, route_miss_failure(&error));
                return complete_response(middleware, &context, &site, response);
            }
        };
        let Some(operation) = self.app.operation_at(recognized.operation_index()) else {
            let response = self.fail(&site, internal_failure());
            return complete_response(middleware, &context, &site, response);
        };
        let descriptor = operation.descriptor();
        site.operation = Some(descriptor);
        for layer in middleware {
            if layer.matches(site.path, site.operation)
                && let Some(response) = layer.layer.on_operation(
                    &mut context,
                    descriptor,
                    self.app.definition().security_schemes(),
                )
            {
                return complete_response(middleware, &context, &site, response);
            }
        }
        if let Some(failure) = self.security_guard(site.path, descriptor) {
            let response = self.fail(&site, failure);
            return complete_response(middleware, &context, &site, response);
        }
        if let Some(body_source) = recognized.body_source() {
            match validate_body(request, self.max_body_bytes, body_source) {
                Ok(()) => {}
                Err(rejection) => {
                    let response = self.fail(&site, rejection.into_failure());
                    return complete_response(middleware, &context, &site, response);
                }
            }
        }
        let request_parts = RoutedRequestParts {
            request,
            route: &recognized,
            context: Some(&context),
            connection: OnceCell::new(),
            background: OnceCell::new(),
        };
        let outcome = if let Some(control) = control {
            operation
                .invoke_http_controlled(&request_parts, control)
                .await
        } else {
            operation.invoke_http(&request_parts).await
        };
        let mut response = match outcome_result(outcome) {
            Ok(response) => response,
            Err(failure) => self.fail(&site, failure),
        };
        response.background.extend(request_parts.scheduled_tasks());
        complete_response(middleware, &context, &site, response)
    }

    async fn dispatch_unlayered<RequestView>(
        &self,
        request: &RequestView,
        control: Option<InvocationControl>,
    ) -> Response
    where
        RequestView: HttpRequestView + ?Sized,
    {
        let target = request.target();
        let mut site = DispatchSite {
            method: request.method(),
            path: target.split_once('?').map_or(target, |(path, _)| path),
            operation: None,
        };
        if validate_url_encoding(target).is_err() {
            return self.fail(&site, invalid_url_encoding_failure());
        }
        if let Some(response) = self
            .openapi
            .and_then(|service| service.handle(site.method, site.path))
        {
            return openapi_response(response);
        }
        let recognized = match self.router.recognize(site.method, site.path) {
            Ok(recognized) => recognized,
            Err(error) => return self.fail(&site, route_miss_failure(&error)),
        };
        let Some(operation) = self.app.operation_at(recognized.operation_index()) else {
            return self.fail(&site, internal_failure());
        };
        site.operation = Some(operation.descriptor());
        if let Some(failure) = self.security_guard(site.path, operation.descriptor()) {
            return self.fail(&site, failure);
        }
        if let Some(body_source) = recognized.body_source() {
            match validate_body(request, self.max_body_bytes, body_source) {
                Ok(()) => {}
                Err(rejection) => return self.fail(&site, rejection.into_failure()),
            }
        }
        let request_parts = RoutedRequestParts {
            request,
            route: &recognized,
            context: None,
            connection: OnceCell::new(),
            background: OnceCell::new(),
        };
        let outcome = if let Some(control) = control {
            operation
                .invoke_http_controlled(&request_parts, control)
                .await
        } else {
            operation.invoke_http(&request_parts).await
        };
        let mut response = match outcome_result(outcome) {
            Ok(response) => response,
            Err(failure) => self.fail(&site, failure),
        };
        response.background.extend(request_parts.scheduled_tasks());
        response
    }

    /// Builds a failure response and offers it to the application error
    /// handlers.
    fn fail(&self, site: &DispatchSite<'_>, mut failure: Failure) -> Response {
        let mut response = failure.response();
        if self.error_handlers.is_empty() {
            return response;
        }
        let error = HttpError {
            source: failure.source,
            status: failure.status,
            code: &failure.code,
            message: &failure.message,
            method: site.method,
            path: site.path,
            operation: site.operation,
        };
        for handler in self.error_handlers {
            handler.on_error(&error, &mut response);
        }
        // A typed `#[api_error]` variant publishes its status in the contract.
        if failure.source == HttpErrorSource::Domain {
            response.status = failure.status;
        }
        response
    }

    /// Fails closed when the matched operation declares a security scheme that
    /// no layer on this dispatch path can verify.
    ///
    /// Both dispatch paths run this before invoking an operation, so an
    /// unlayered path never serves a declared scheme unauthenticated. A scoped
    /// layer counts only where its scope reaches.
    fn security_guard(&self, path: &str, descriptor: &OperationDescriptor) -> Option<Failure> {
        if self.allow_unverified_security_schemes || descriptor.contract.security.is_empty() {
            return None;
        }
        if self
            .middleware
            .iter()
            .any(|layer| layer.layer.verifies_security() && layer.matches(path, Some(descriptor)))
        {
            return None;
        }
        Some(Failure::new(
            HttpErrorSource::Internal,
            500,
            "security_verifier_missing",
            "the operation declares a security scheme with no registered verifier",
        ))
    }
}

/// A failure response before it reaches the application error handlers.
struct Failure {
    source: HttpErrorSource,
    status: u16,
    code: Cow<'static, str>,
    message: Cow<'static, str>,
    details: Option<Value>,
    headers: Vec<ResponseHeader>,
}

impl Failure {
    fn new(
        source: HttpErrorSource,
        status: u16,
        code: impl Into<Cow<'static, str>>,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            source,
            status,
            code: code.into(),
            message: message.into(),
            details: None,
            headers: Vec::new(),
        }
    }

    fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    fn with_headers(mut self, headers: Vec<ResponseHeader>) -> Self {
        self.headers = headers;
        self
    }

    fn response(&mut self) -> Response {
        let response = error_response(self.status, &self.code, &self.message, self.details.take());
        with_outcome_headers(response, std::mem::take(&mut self.headers))
    }

    fn into_response(mut self) -> Response {
        self.response()
    }
}

fn invalid_url_encoding_failure() -> Failure {
    Failure::new(
        HttpErrorSource::Request,
        400,
        "invalid_url_encoding",
        "request target contains invalid percent encoding",
    )
}

fn route_miss_failure(error: &RouteError) -> Failure {
    match error {
        RouteError::MethodNotAllowed { allowed } => {
            let allow = allowed
                .iter()
                .map(|method| method.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Failure::new(
                HttpErrorSource::Routing,
                405,
                "method_not_allowed",
                "HTTP method not allowed",
            )
            .with_headers(vec![ResponseHeader::new("allow", allow)])
        }
        RouteError::NotFound => Failure::new(
            HttpErrorSource::Routing,
            404,
            "not_found",
            "HTTP route not found",
        ),
    }
}

fn complete_response(
    middleware: &[ScopedMiddleware],
    context: &HttpRequestContext<'_>,
    site: &DispatchSite<'_>,
    mut response: Response,
) -> Response {
    for layer in middleware.iter().rev() {
        if layer.matches(site.path, site.operation) {
            layer
                .layer
                .on_response(context, site.operation, &mut response);
        }
    }
    response
}

fn openapi_response(asset: OpenApiAssetResponse) -> Response {
    let mut response = Response {
        status: asset.status,
        headers: ResponseHeaders::empty(),
        body: asset.body,
        stream: None,
        upgrade: None,
        background: Vec::new(),
    };
    for (name, value) in asset.headers {
        response = response.with_header(name, value);
    }
    response
}

/// Every [`HttpMethod`] variant, in `Ord` order.
///
/// The `Allow` header of a 405 is rendered by walking this table low bit
/// first, so the order is what makes that list sorted; it must stay in sync
/// with the `HttpMethod` declaration order and with [`method_index`].
const METHODS: [HttpMethod; METHOD_COUNT] = [
    HttpMethod::Get,
    HttpMethod::Head,
    HttpMethod::Post,
    HttpMethod::Put,
    HttpMethod::Patch,
    HttpMethod::Delete,
    HttpMethod::Options,
    HttpMethod::Trace,
    HttpMethod::Connect,
];

const METHOD_COUNT: usize = 9;

const fn method_index(method: HttpMethod) -> usize {
    match method {
        HttpMethod::Get => 0,
        HttpMethod::Head => 1,
        HttpMethod::Post => 2,
        HttpMethod::Put => 3,
        HttpMethod::Patch => 4,
        HttpMethod::Delete => 5,
        HttpMethod::Options => 6,
        HttpMethod::Trace => 7,
        HttpMethod::Connect => 8,
    }
}

const PATH_HASH_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// `FxHash`-style mixer for route paths and their segments.
///
/// The router hashes a path on every request; `SipHash` key setup and its
/// per-byte round dominated that probe. This consumes eight bytes per
/// multiply, and is hand-rolled because the workspace ships no hasher
/// dependency. It is not collision-resistant and must never be used on
/// attacker-chosen keys that are stored — the tables here are built once from
/// the operation graph and only probed at runtime.
#[derive(Default)]
struct PathHasher {
    hash: u64,
}

impl PathHasher {
    fn mix(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(PATH_HASH_SEED);
    }
}

impl Hasher for PathHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut word = [0_u8; 8];
        let mut index = 0;
        while index + 8 <= bytes.len() {
            word.copy_from_slice(&bytes[index..index + 8]);
            self.mix(u64::from_le_bytes(word));
            index += 8;
        }
        let tail = &bytes[index..];
        if !tail.is_empty() {
            word = [0; 8];
            word[..tail.len()].copy_from_slice(tail);
            self.mix(u64::from_le_bytes(word));
        }
        // Zero padding makes "a" and "a\0" hash alike without this.
        self.mix(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    }

    fn finish(&self) -> u64 {
        self.hash
    }
}

type PathMap<Value> = HashMap<Box<str>, Value, BuildHasherDefault<PathHasher>>;

/// One static path segment and the trie node it leads to.
struct StaticChild {
    head: u8,
    node: usize,
    segment: Box<str>,
}

#[derive(Default)]
struct RouteNode {
    /// Only parameterized routes reach the trie, so these sets are tiny; a
    /// scan filtered by the first segment byte beats hashing every segment.
    static_children: Vec<StaticChild>,
    parameter_child: Option<usize>,
    /// Bit [`method_index`] set when `endpoints[method_index]` is bound.
    methods: u16,
    endpoints: [usize; METHOD_COUNT],
}

impl RouteNode {
    fn static_child(&self, segment: &str) -> Option<usize> {
        let head = head_byte(segment);
        self.static_children
            .iter()
            .find(|child| child.head == head && &*child.segment == segment)
            .map(|child| child.node)
    }
}

#[derive(Clone)]
struct CompiledEndpoint {
    operation_index: usize,
    parameter_names: Vec<String>,
    body_source: Option<InputSource>,
}

/// What one static path resolves to, for every method at once.
///
/// Holding the whole method row behind a single key is what lets a miss
/// decide 404 versus 405 without a second hash: the probe that failed to find
/// the requested method already reported which methods the path does answer.
#[derive(Default)]
struct StaticSlot {
    /// Bit [`method_index`] set when `endpoints[method_index]` is bound.
    methods: u16,
    endpoints: [usize; METHOD_COUNT],
}

/// A runtime-neutral router compiled once from the operation graph.
pub struct Router {
    nodes: Vec<RouteNode>,
    /// Endpoints reached through either table; both hold slots into this.
    endpoints: Vec<CompiledEndpoint>,
    static_routes: PathMap<StaticSlot>,
    /// Rejection filter over `(second path byte, path length)` for every
    /// static path. A path whose bit is clear cannot be static, so dynamic
    /// requests skip the static probe outright.
    static_filter: [u64; 2],
}

impl Router {
    #[must_use]
    pub fn new(app: &ExecutableApp) -> Self {
        let mut router = Self {
            nodes: vec![RouteNode::default()],
            endpoints: Vec::new(),
            static_routes: PathMap::default(),
            static_filter: [0; 2],
        };
        for descriptor in app.definition().operations() {
            let Some(operation_index) = app.operation_index(&descriptor.contract.id) else {
                continue;
            };
            router.insert(descriptor, operation_index);
        }
        router
    }

    fn insert(&mut self, descriptor: &OperationDescriptor, operation_index: usize) {
        let endpoint = CompiledEndpoint {
            operation_index,
            parameter_names: route_segments(&descriptor.http.path)
                .filter_map(path_parameter_name)
                .map(str::to_owned)
                .collect(),
            body_source: body_source(descriptor),
        };
        let method = method_index(descriptor.http.method);
        if endpoint.parameter_names.is_empty() {
            let path = descriptor.http.path.as_str();
            let (word, bit) = static_filter_slot(path);
            self.static_filter[word] |= bit;
            let slot = self.endpoints.len();
            self.endpoints.push(endpoint);
            let entry = self.static_routes.entry(path.into()).or_default();
            entry.endpoints[method] = slot;
            entry.methods |= 1_u16 << method;
            return;
        }

        let mut node_index = 0;
        for segment in route_segments(&descriptor.http.path) {
            node_index = if path_parameter_name(segment).is_some() {
                if let Some(child) = self.nodes[node_index].parameter_child {
                    child
                } else {
                    let child = self.nodes.len();
                    self.nodes.push(RouteNode::default());
                    self.nodes[node_index].parameter_child = Some(child);
                    child
                }
            } else if let Some(child) = self.nodes[node_index].static_child(segment) {
                child
            } else {
                let child = self.nodes.len();
                self.nodes.push(RouteNode::default());
                self.nodes[node_index].static_children.push(StaticChild {
                    head: head_byte(segment),
                    node: child,
                    segment: segment.into(),
                });
                child
            };
        }
        let slot = self.endpoints.len();
        self.endpoints.push(endpoint);
        let node = &mut self.nodes[node_index];
        node.endpoints[method] = slot;
        node.methods |= 1_u16 << method;
    }

    /// Resolves an HTTP method and path to a direct executable operation slot.
    ///
    /// # Errors
    ///
    /// Returns [`RouteError::NotFound`] when no path matches, or
    /// [`RouteError::MethodNotAllowed`] when the path exists for other methods.
    pub fn recognize<'router, 'path>(
        &'router self,
        method: HttpMethod,
        path: &'path str,
    ) -> Result<RouteMatch<'router, 'path>, RouteError> {
        let method = method_index(method);
        let (word, bit) = static_filter_slot(path);
        let slot = if self.static_filter[word] & bit == 0 {
            None
        } else {
            self.static_routes.get(path)
        };
        if let Some(slot) = slot
            && slot.methods & (1_u16 << method) != 0
            && let Some(endpoint) = self.endpoints.get(slot.endpoints[method])
        {
            return Ok(RouteMatch {
                endpoint,
                captures: CapturedSegments::new(),
            });
        }

        let mut captures = CapturedSegments::new();
        let mut other_methods = false;
        if let Some(endpoint) = self.walk(
            0,
            Some(trim_leading_slash(path)),
            method,
            &mut captures,
            &mut other_methods,
        ) {
            return Ok(RouteMatch { endpoint, captures });
        }

        let static_methods = slot.map_or(0, |slot| slot.methods);
        if static_methods == 0 && !other_methods {
            return Err(RouteError::NotFound);
        }
        Err(RouteError::MethodNotAllowed {
            allowed: self.allowed_methods(path, static_methods),
        })
    }

    /// Walks the parameter trie for `method`, recording in `other_methods`
    /// whether the path exists under a method that was not asked for. That one
    /// bit is what lets a 404 answer without a second walk.
    ///
    /// `captures` is a stack: a parameter descent pushes and pops on failure,
    /// so backtracking never copies it.
    fn walk<'router, 'path>(
        &'router self,
        node_index: usize,
        rest: Option<&'path str>,
        method: usize,
        captures: &mut CapturedSegments<'path>,
        other_methods: &mut bool,
    ) -> Option<&'router CompiledEndpoint> {
        let node = &self.nodes[node_index];
        let Some(rest) = rest else {
            if node.methods & (1_u16 << method) == 0 {
                *other_methods |= node.methods != 0;
                return None;
            }
            return self.endpoints.get(node.endpoints[method]);
        };
        let (segment, tail) = split_segment(rest);

        if let Some(child) = node.static_child(segment)
            && let Some(found) = self.walk(child, tail, method, captures, other_methods)
        {
            return Some(found);
        }
        let child = node.parameter_child?;
        captures.push(segment);
        if let Some(found) = self.walk(child, tail, method, captures, other_methods) {
            return Some(found);
        }
        captures.pop();
        None
    }

    /// Renders the sorted `Allow` list of a 405. Only reached when a 405 is
    /// actually returned, never on the 404 path.
    fn allowed_methods(&self, path: &str, static_methods: u16) -> Vec<HttpMethod> {
        let mut methods = static_methods;
        self.collect_methods(0, Some(trim_leading_slash(path)), &mut methods);
        METHODS
            .iter()
            .enumerate()
            .filter(|(index, _)| methods & (1_u16 << index) != 0)
            .map(|(_, method)| *method)
            .collect()
    }

    fn collect_methods(&self, node_index: usize, rest: Option<&str>, methods: &mut u16) {
        let node = &self.nodes[node_index];
        let Some(rest) = rest else {
            *methods |= node.methods;
            return;
        };
        let (segment, tail) = split_segment(rest);
        if let Some(child) = node.static_child(segment) {
            self.collect_methods(child, tail, methods);
        }
        if let Some(child) = node.parameter_child {
            self.collect_methods(child, tail, methods);
        }
    }
}

fn route_segments(path: &str) -> std::str::Split<'_, char> {
    trim_leading_slash(path).split('/')
}

fn trim_leading_slash(path: &str) -> &str {
    path.strip_prefix('/').unwrap_or(path)
}

/// Splits the leading segment off a slash-separated remainder. `None` as the
/// tail means the segment just taken was the last one.
fn split_segment(rest: &str) -> (&str, Option<&str>) {
    match rest.as_bytes().iter().position(|byte| *byte == b'/') {
        Some(index) => (&rest[..index], Some(&rest[index + 1..])),
        None => (rest, None),
    }
}

/// First byte of a segment, NUL standing in for the empty segment. Only a
/// prefilter for the child scan; the full comparison still decides.
const fn head_byte(segment: &str) -> u8 {
    match segment.as_bytes().first() {
        Some(byte) => *byte,
        None => 0,
    }
}

/// Word and bit of the static-path rejection filter. Every path starts with
/// `/`, so the second byte is what discriminates; the length separates the
/// rest. Collisions only cost a wasted probe, but a set bit must never be
/// missed, so the same function fills the filter at build time.
fn static_filter_slot(path: &str) -> (usize, u64) {
    let byte = path.as_bytes().get(1).copied().unwrap_or(0);
    let slot = (usize::from(byte).wrapping_mul(3) ^ path.len()) & 127;
    (slot >> 6, 1_u64 << (slot & 63))
}

/// A router miss that distinguishes an unknown path from a wrong method.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteError {
    MethodNotAllowed { allowed: Vec<HttpMethod> },
    NotFound,
}

/// A compiled route match with its direct operation slot and path captures.
pub struct RouteMatch<'router, 'path> {
    endpoint: &'router CompiledEndpoint,
    captures: CapturedSegments<'path>,
}

impl RouteMatch<'_, '_> {
    #[must_use]
    pub const fn operation_index(&self) -> usize {
        self.endpoint.operation_index
    }

    #[must_use]
    pub const fn requires_json(&self) -> bool {
        matches!(self.endpoint.body_source, Some(InputSource::Json))
    }

    #[must_use]
    pub const fn body_source(&self) -> Option<InputSource> {
        self.endpoint.body_source
    }

    #[must_use]
    pub fn path_parameter(&self, name: &str) -> Option<Cow<'_, str>> {
        self.endpoint
            .parameter_names
            .iter()
            .position(|parameter| parameter == name)
            .and_then(|position| self.captures.get(position))
            .and_then(|value| decode_url_component(value, false).ok())
    }
}

const INLINE_PATH_PARAMETERS: usize = 8;

/// A capture stack. `Empty` is not just `Inline` with zero length: a static
/// match constructs one per request and must not pay for zeroing the inline
/// array it will never write.
#[derive(Clone)]
enum CapturedSegments<'path> {
    Empty,
    Inline {
        values: [Option<&'path str>; INLINE_PATH_PARAMETERS],
        len: usize,
    },
    Heap(Vec<&'path str>),
}

impl<'path> CapturedSegments<'path> {
    const fn new() -> Self {
        Self::Empty
    }

    fn push(&mut self, value: &'path str) {
        match self {
            Self::Empty => {
                let mut values = [None; INLINE_PATH_PARAMETERS];
                values[0] = Some(value);
                *self = Self::Inline { values, len: 1 };
            }
            Self::Inline { values, len } if *len < INLINE_PATH_PARAMETERS => {
                values[*len] = Some(value);
                *len += 1;
            }
            Self::Inline { values, len } => {
                let mut heap = values[..*len]
                    .iter()
                    .filter_map(|value| *value)
                    .collect::<Vec<_>>();
                heap.push(value);
                *self = Self::Heap(heap);
            }
            Self::Heap(values) => values.push(value),
        }
    }

    /// Undoes the most recent [`Self::push`], so a failed parameter descent
    /// can backtrack without the caller having cloned the stack.
    fn pop(&mut self) {
        match self {
            Self::Empty => {}
            Self::Inline { len, .. } => *len = len.saturating_sub(1),
            Self::Heap(values) => {
                values.pop();
            }
        }
    }

    fn get(&self, index: usize) -> Option<&'path str> {
        match self {
            Self::Inline { values, len } if index < *len => values[index],
            Self::Empty | Self::Inline { .. } => None,
            Self::Heap(values) => values.get(index).copied(),
        }
    }
}

struct RoutedRequestParts<'request, 'router, 'path, 'context, RequestView: ?Sized> {
    request: &'request RequestView,
    route: &'request RouteMatch<'router, 'path>,
    context: Option<&'context HttpRequestContext<'request>>,
    connection: OnceCell<ConnectionInfo>,
    background: OnceCell<BackgroundTasks>,
}

impl<RequestView> RoutedRequestParts<'_, '_, '_, '_, RequestView>
where
    RequestView: HttpRequestView + ?Sized,
{
    /// Materializes the normalized transport values on first extractor use.
    fn connection_info(&self) -> &ConnectionInfo {
        self.connection.get_or_init(|| {
            self.context.map_or_else(
                || ConnectionInfo::from_request(self.request),
                HttpRequestContext::connection_info,
            )
        })
    }

    /// Materializes the after-response task handle on first extractor use, so
    /// an operation that never injects one allocates nothing.
    fn background_tasks(&self) -> &BackgroundTasks {
        self.background.get_or_init(BackgroundTasks::new)
    }

    /// Takes what the handler scheduled through the injected handle.
    fn scheduled_tasks(&self) -> Vec<BackgroundTask> {
        self.background
            .get()
            .map_or_else(Vec::new, BackgroundTasks::take)
    }
}

impl<RequestView> InvocationRequestParts for RoutedRequestParts<'_, '_, '_, '_, RequestView>
where
    RequestView: HttpRequestView + ?Sized,
{
    fn value(&self, source: InputSource, name: &str, index: usize) -> Option<Cow<'_, str>> {
        match source {
            InputSource::Path if index == 0 => self.route.path_parameter(name),
            InputSource::Query => query_value(self.request.target(), name, index),
            InputSource::Header => self.request.header_value(name, index).map(Cow::Borrowed),
            InputSource::Cookie => cookie_value(self.request, name, index),
            InputSource::Form => form_value(self.request.body(), name, index),
            InputSource::Path
            | InputSource::Json
            | InputSource::Multipart
            | InputSource::File
            | InputSource::Stream => None,
        }
    }

    fn body(&self) -> &[u8] {
        self.request.body()
    }

    fn take_body_stream(&self) -> Option<StreamingBody> {
        self.request.take_body_stream()
    }

    fn extension(&self, type_id: TypeId) -> Option<&dyn Any> {
        if let Some(value) = self
            .context
            .and_then(|context| context.extension_by_id(type_id))
        {
            return Some(value);
        }
        if type_id == TypeId::of::<ConnectionInfo>() {
            return Some(self.connection_info());
        }
        if type_id == TypeId::of::<BackgroundTasks>() {
            return Some(self.background_tasks());
        }
        None
    }

    fn method(&self) -> Option<HttpMethod> {
        Some(self.request.method())
    }

    fn path(&self) -> Option<&str> {
        let target = self.request.target();
        Some(target.split('?').next().unwrap_or(target))
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        self.request.peer_addr()
    }

    fn scheme(&self) -> Option<&str> {
        Some(self.connection_info().scheme())
    }

    fn host(&self) -> Option<&str> {
        self.connection_info().host()
    }
}

fn body_source(descriptor: &OperationDescriptor) -> Option<InputSource> {
    descriptor
        .contract
        .inputs
        .iter()
        .map(|input| input.source)
        .find(|source| {
            matches!(
                source,
                InputSource::Json
                    | InputSource::Form
                    | InputSource::Multipart
                    | InputSource::File
                    | InputSource::Stream
            )
        })
}

fn cookie_value<'request>(
    request: &'request (impl HttpRequestView + ?Sized),
    name: &str,
    index: usize,
) -> Option<Cow<'request, str>> {
    let mut header_index = 0;
    let mut found = 0;
    while let Some(header) = request.header_value("cookie", header_index) {
        for cookie in header.split(';') {
            let (cookie_name, value) = cookie.trim().split_once('=').unwrap_or((cookie.trim(), ""));
            if cookie_name == name {
                if found == index {
                    return Some(Cow::Borrowed(value));
                }
                found += 1;
            }
        }
        header_index += 1;
    }
    None
}

fn form_value<'body>(body: &'body [u8], name: &str, index: usize) -> Option<Cow<'body, str>> {
    let body = std::str::from_utf8(body).ok()?;
    let mut found = 0;
    for pair in body.split('&').filter(|pair| !pair.is_empty()) {
        let (raw_name, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let decoded_name = decode_url_component(raw_name, true).ok()?;
        if decoded_name == name {
            if found == index {
                return decode_url_component(raw_value, true).ok();
            }
            found += 1;
        }
    }
    None
}

fn path_parameter_name(segment: &str) -> Option<&str> {
    segment
        .strip_prefix('{')
        .and_then(|segment| segment.strip_suffix('}'))
        .filter(|name| !name.is_empty())
}

fn query_value<'target>(
    target: &'target str,
    name: &str,
    index: usize,
) -> Option<Cow<'target, str>> {
    let (_, query) = target.split_once('?')?;
    let mut found = 0;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (raw_name, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let decoded_name = decode_url_component(raw_name, true).ok()?;
        if decoded_name == name {
            if found == index {
                return decode_url_component(raw_value, true).ok();
            }
            found += 1;
        }
    }
    None
}

fn header_name_matches(header: &str, argument: &str) -> bool {
    header.bytes().eq(argument.bytes().map(|byte| {
        if byte == b'_' {
            b'-'
        } else {
            byte.to_ascii_lowercase()
        }
    }))
}

fn validate_url_encoding(target: &str) -> Result<(), ()> {
    let path = target.split_once('?').map_or(target, |(path, _)| path);
    decode_url_component(path, false)?;
    if let Some((_, query)) = target.split_once('?') {
        for pair in query.split('&') {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            decode_url_component(name, true)?;
            decode_url_component(value, true)?;
        }
    }
    Ok(())
}

fn decode_url_component(value: &str, plus_as_space: bool) -> Result<Cow<'_, str>, ()> {
    if !value.as_bytes().contains(&b'%') && (!plus_as_space || !value.as_bytes().contains(&b'+')) {
        return Ok(Cow::Borrowed(value));
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let high = bytes
                    .get(index + 1)
                    .copied()
                    .and_then(hex_value)
                    .ok_or(())?;
                let low = bytes
                    .get(index + 2)
                    .copied()
                    .and_then(hex_value)
                    .ok_or(())?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            b'+' if plus_as_space => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).map(Cow::Owned).map_err(|_| ())
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

enum BodyRejection {
    PayloadTooLarge { max_body_bytes: usize },
    UnsupportedMediaType,
}

impl BodyRejection {
    fn into_failure(self) -> Failure {
        match self {
            Self::PayloadTooLarge { max_body_bytes } => Failure::new(
                HttpErrorSource::Request,
                413,
                "payload_too_large",
                "request body exceeds the configured limit",
            )
            .with_details(json!({ "maxBytes": max_body_bytes })),
            Self::UnsupportedMediaType => Failure::new(
                HttpErrorSource::Request,
                415,
                "unsupported_media_type",
                "request body media type does not match the operation input",
            ),
        }
    }

    fn into_response(self) -> Response {
        self.into_failure().into_response()
    }
}

fn validate_body(
    request: &(impl HttpRequestView + ?Sized),
    max_body_bytes: usize,
    source: InputSource,
) -> Result<(), BodyRejection> {
    if request.body().len() > max_body_bytes {
        return Err(BodyRejection::PayloadTooLarge { max_body_bytes });
    }
    if source == InputSource::Stream {
        return Ok(());
    }

    let valid_media_type = request
        .header_value("content-type", 0)
        .is_some_and(|content_type| match source {
            InputSource::Json => is_json_media_type(content_type),
            InputSource::Form => media_type_is(content_type, "application/x-www-form-urlencoded"),
            InputSource::Multipart | InputSource::File => {
                media_type_is(content_type, "multipart/form-data")
            }
            InputSource::Stream => unreachable!("streaming bodies do not require a media type"),
            InputSource::Path | InputSource::Query | InputSource::Header | InputSource::Cookie => {
                true
            }
        });
    if !valid_media_type {
        return Err(BodyRejection::UnsupportedMediaType);
    }

    Ok(())
}

fn media_type_is(value: &str, expected: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case(expected))
}

fn outcome_result(outcome: ExecutionOutcome) -> Result<Response, Failure> {
    match outcome {
        ExecutionOutcome::Success {
            status,
            headers,
            body,
            background,
        } => {
            let response = match body {
                Some(body) => Response {
                    status,
                    headers: json_headers(),
                    body,
                    stream: None,
                    upgrade: None,
                    background: Vec::new(),
                },
                None => Response {
                    status,
                    headers: ResponseHeaders::empty(),
                    body: Vec::new(),
                    stream: None,
                    upgrade: None,
                    background: Vec::new(),
                },
            };
            let mut response = with_outcome_headers(response, headers);
            response.background = background;
            Ok(response)
        }
        ExecutionOutcome::StreamingSuccess {
            status,
            headers,
            body,
            background,
        } => Ok(with_outcome_headers(
            Response {
                status,
                headers: ResponseHeaders::empty(),
                body: Vec::new(),
                stream: Some(body),
                upgrade: None,
                background,
            },
            headers,
        )),
        ExecutionOutcome::Upgrade {
            upgrade,
            background,
        } => {
            let headers = upgrade.headers().to_vec();
            Ok(with_outcome_headers(
                Response {
                    status: 101,
                    headers: ResponseHeaders::empty(),
                    body: Vec::new(),
                    stream: None,
                    upgrade: Some(upgrade),
                    background,
                },
                headers,
            ))
        }
        ExecutionOutcome::Rejected {
            status,
            code,
            message,
            details,
        } => Err(Failure {
            source: HttpErrorSource::Rejection,
            status,
            code: Cow::Owned(code),
            message: Cow::Owned(message),
            details,
            headers: Vec::new(),
        }),
        ExecutionOutcome::DomainError(error) => {
            let details = match error.details {
                Some(details) => match blazingly_json::from_slice(&details) {
                    Ok(details) => Some(details),
                    Err(_) => return Err(internal_failure()),
                },
                None => None,
            };
            Err(Failure {
                source: HttpErrorSource::Domain,
                status: error.status,
                code: Cow::Owned(error.code),
                message: Cow::Owned(error.message),
                details,
                headers: error.headers,
            })
        }
        ExecutionOutcome::InternalError { .. } => Err(internal_failure()),
    }
}

fn with_outcome_headers(mut response: Response, headers: Vec<ResponseHeader>) -> Response {
    for header in headers {
        response = response.with_header(header.name, header.value);
    }
    response
}

fn error_response(status: u16, code: &str, message: &str, details: Option<Value>) -> Response {
    let mut error = json!({
        "error": {
            "code": code,
            "message": message,
        }
    });
    if let Some(details) = details {
        error["error"]["details"] = details;
    }
    json_response(status, &error)
}

fn internal_failure() -> Failure {
    Failure::new(
        HttpErrorSource::Internal,
        500,
        "internal_error",
        "the operation could not be completed",
    )
}

fn json_response(status: u16, value: &Value) -> Response {
    let Ok(body) = blazingly_json::to_vec(value) else {
        return Response {
            status: 500,
            headers: json_headers(),
            body: br#"{"error":{"code":"internal_error","message":"the operation could not be completed"}}"#
                .to_vec(),
            stream: None,
            upgrade: None,
            background: Vec::new(),
        };
    };

    Response {
        status,
        headers: json_headers(),
        body,
        stream: None,
        upgrade: None,
        background: Vec::new(),
    }
}

fn json_headers() -> ResponseHeaders {
    ResponseHeaders::one("content-type", "application/json")
}

fn is_json_media_type(value: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
}

fn normalize_header_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

impl Response {
    #[must_use]
    pub fn with_header(mut self, name: impl AsRef<str>, value: impl Into<String>) -> Self {
        self.set_header(name, value);
        self
    }
}

const INLINE_RESPONSE_HEADERS: usize = 4;
type OwnedHeader = (Cow<'static, str>, Cow<'static, str>);

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResponseHeaders {
    inline: [Option<OwnedHeader>; INLINE_RESPONSE_HEADERS],
    overflow: Vec<OwnedHeader>,
}

impl ResponseHeaders {
    fn empty() -> Self {
        Self {
            inline: std::array::from_fn(|_| None),
            overflow: Vec::new(),
        }
    }

    fn one(name: &'static str, value: &'static str) -> Self {
        let mut headers = Self::empty();
        headers.inline[0] = Some((Cow::Borrowed(name), Cow::Borrowed(value)));
        headers
    }

    fn insert(&mut self, name: Cow<'static, str>, value: Cow<'static, str>) {
        // Set-Cookie is not a comma-joinable field: one response may carry
        // several independent cookie mutations.
        if !name.eq_ignore_ascii_case("set-cookie") {
            if let Some((_, existing)) = self
                .inline
                .iter_mut()
                .filter_map(Option::as_mut)
                .chain(self.overflow.iter_mut())
                .find(|(existing, _)| existing.eq_ignore_ascii_case(&name))
            {
                *existing = value;
                return;
            }
        }
        if let Some(slot) = self.inline.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some((name, value));
        } else {
            self.overflow.push((name, value));
        }
    }

    fn remove(&mut self, name: &str) {
        for slot in &mut self.inline {
            if slot
                .as_ref()
                .is_some_and(|(existing, _)| existing.eq_ignore_ascii_case(name))
            {
                *slot = None;
            }
        }
        self.overflow
            .retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.inline
            .iter()
            .filter_map(Option::as_ref)
            .chain(&self.overflow)
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_ref())
    }

    fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.inline
            .iter()
            .filter_map(Option::as_ref)
            .chain(&self.overflow)
            .map(|(name, value)| (name.as_ref(), value.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BackgroundTasks, ConnectionInfo, HttpApp, HttpError, HttpErrorHandler, HttpErrorSource,
        HttpMiddleware, HttpRequestContext, MiddlewareScope, Request, Response, RouteError, Router,
        TestApp,
    };
    use blazingly_core::{
        HttpMethod, InputDescriptor, InputSource, OperationDescriptor, OperationFailure,
        PreparedJson, ResponseDescriptor, SecurityLocation, SecurityRequirement,
        SecuritySchemeDescriptor, SecuritySchemeKind, TypeDescriptor,
    };
    use blazingly_executor::{
        ExecutableApp, ExecutableOperation, ExecutionOutcome, Extension, FromInvocation,
        InputRejection, InvocationControl, InvocationInput, OperationFuture, OperationOutput,
    };
    use blazingly_json::{Value, json};
    use futures_lite::future;
    use std::cell::{Cell, RefCell};
    use std::future::Future;
    use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::{Context, Poll};

    struct PassthroughLayer;

    impl HttpMiddleware for PassthroughLayer {}

    struct AuditLayer;

    impl HttpMiddleware for AuditLayer {
        fn verifies_security(&self) -> bool {
            false
        }
    }

    struct NormalizingProxy;

    impl HttpMiddleware for NormalizingProxy {
        fn on_request(&self, context: &mut HttpRequestContext<'_>) -> Option<Response> {
            context.set_client_ip(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)));
            context.set_scheme("https");
            context.set_host("api.example");
            None
        }
    }

    fn connection_operation(
        path: &str,
        id: &str,
        security: Vec<SecurityRequirement>,
    ) -> ExecutableOperation {
        let descriptor = OperationDescriptor::new(
            HttpMethod::Get,
            path,
            id,
            "Reports the normalized connection",
            None,
            vec![ResponseDescriptor::success(
                200,
                Some(TypeDescriptor::new("Connection")),
            )],
        )
        .expect("test operation id should be valid")
        .with_security(security);
        ExecutableOperation::typed(descriptor, |input| {
            let Extension(connection) =
                Extension::<ConnectionInfo>::from_invocation(&input, "connection", true)?;
            let body = json!({
                "scheme": connection.scheme(),
                "host": connection.host(),
                "clientIp": connection.client_ip().map(|address| address.to_string()),
            });
            Ok(Box::pin(async move {
                ExecutionOutcome::Success {
                    status: 200,
                    headers: Vec::new(),
                    body: Some(blazingly_json::to_vec(&body).expect("connection body")),
                    background: Vec::new(),
                }
            }) as OperationFuture)
        })
    }

    fn executable() -> ExecutableApp {
        ExecutableApp::with_security_schemes(
            [
                connection_operation(
                    "/secure",
                    "http.secure",
                    vec![SecurityRequirement::new("api_key")],
                ),
                connection_operation("/plain", "http.plain", Vec::new()),
            ],
            [SecuritySchemeDescriptor::new(
                "api_key",
                SecuritySchemeKind::ApiKey {
                    location: SecurityLocation::Header,
                    name: "x-api-key".to_owned(),
                },
            )],
        )
        .expect("secured operation graph should compile")
    }

    fn error_code(response: &Response) -> String {
        let body: Value = response.json().expect("json error body");
        body["error"]["code"]
            .as_str()
            .expect("stable error code")
            .to_owned()
    }

    /// An operation that encodes a view borrowed from a value it owns, which
    /// `Json<T>` cannot express because the borrow ends when the operation
    /// returns.
    fn prepared_operation() -> ExecutableOperation {
        let descriptor = OperationDescriptor::new(
            HttpMethod::Get,
            "/prepared",
            "http.prepared",
            "Encodes a borrowed view inside the operation",
            None,
            vec![ResponseDescriptor::success(
                200,
                Some(TypeDescriptor::new("Titles")),
            )],
        )
        .expect("test operation id should be valid");
        ExecutableOperation::typed(descriptor, |_| {
            Ok(Box::pin(async move {
                let owned = [String::from("first"), String::from("second")];
                let borrowed: Vec<&str> = owned.iter().map(String::as_str).collect();
                let body = PreparedJson::<TitlesSchema>::encode(&borrowed)
                    .expect("the borrowed view encodes");
                OperationOutput::into_execution_outcome(body)
            }) as OperationFuture)
        })
    }

    struct TitlesSchema;

    impl blazingly_core::ApiSchema for TitlesSchema {
        fn type_descriptor() -> TypeDescriptor {
            TypeDescriptor::new("Titles")
        }
    }

    #[test]
    fn a_prepared_body_reaches_the_wire_verbatim_as_json() {
        let executable = ExecutableApp::new([prepared_operation()])
            .expect("prepared operation graph should compile");
        let response = future::block_on(TestApp::new(&executable).call(Request::get("/prepared")));

        assert_eq!(response.status(), 200);
        assert_eq!(
            response.get_header("content-type"),
            Some("application/json")
        );
        assert_eq!(response.body(), br#"["first","second"]"#);
    }

    #[test]
    fn unlayered_dispatch_fails_closed_for_a_declared_scheme() {
        let executable = executable();
        let response = future::block_on(TestApp::new(&executable).call(Request::get("/secure")));

        assert_eq!(response.status(), 500);
        assert_eq!(error_code(&response), "security_verifier_missing");
    }

    #[test]
    fn owned_adapter_fails_closed_for_a_declared_scheme() {
        let app = HttpApp::new(executable());
        let response = future::block_on(app.call(Request::get("/secure")));

        assert_eq!(response.status(), 500);
        assert_eq!(error_code(&response), "security_verifier_missing");
    }

    #[test]
    fn unverified_scheme_opt_out_executes_the_operation() {
        let executable = executable();
        let response = future::block_on(
            TestApp::new(&executable)
                .with_unverified_security_schemes(true)
                .call(Request::get("/secure")),
        );
        assert_eq!(response.status(), 200);

        let owned = HttpApp::new(executable).with_unverified_security_schemes(true);
        let response = future::block_on(owned.call(Request::get("/secure")));
        assert_eq!(response.status(), 200);
    }

    #[test]
    fn routes_emission_lists_every_operation() {
        let executable = executable();
        let table = super::routes_table_text(executable.definition());
        assert!(table.starts_with("METHOD\tPATH\tOPERATION\tSUMMARY\n"));
        assert!(table.contains("GET\t/secure\thttp.secure\tReports the normalized connection\n"));
        assert!(table.contains("GET\t/plain\thttp.plain\tReports the normalized connection\n"));
    }

    #[test]
    fn openapi_emission_serializes_the_default_document() {
        let executable = executable();
        let text = super::openapi_document_text(executable.definition())
            .expect("the OpenAPI document serializes");
        assert!(text.ends_with('\n'));
        let document: Value = blazingly_json::from_str(&text).expect("emitted JSON parses");
        assert_eq!(document["openapi"].as_str(), Some("3.1.0"));
        assert_eq!(
            document["paths"]["/secure"]["get"]["operationId"].as_str(),
            Some("http.secure")
        );
        assert_eq!(
            document["paths"]["/plain"]["get"]["operationId"].as_str(),
            Some("http.plain")
        );
    }

    #[test]
    fn unsecured_operations_are_untouched_by_the_guard() {
        let executable = executable();
        let response = future::block_on(TestApp::new(&executable).call(Request::get("/plain")));

        assert_eq!(response.status(), 200);
    }

    #[test]
    fn layered_dispatch_runs_the_same_guard() {
        let executable = executable();
        let audited = future::block_on(
            TestApp::new(&executable)
                .with_middleware(AuditLayer)
                .call(Request::get("/secure")),
        );
        assert_eq!(audited.status(), 500);
        assert_eq!(error_code(&audited), "security_verifier_missing");

        let verified = future::block_on(
            TestApp::new(&executable)
                .with_middleware(PassthroughLayer)
                .call(Request::get("/secure")),
        );
        assert_eq!(verified.status(), 200);
    }

    type EventLog = Rc<RefCell<Vec<String>>>;
    type HandleExtractor = fn(&InvocationInput<'_>) -> Result<BackgroundTasks, InputRejection>;

    struct RecordingLayer {
        name: &'static str,
        events: EventLog,
    }

    impl HttpMiddleware for RecordingLayer {
        fn on_request(&self, _context: &mut HttpRequestContext<'_>) -> Option<Response> {
            self.events
                .borrow_mut()
                .push(format!("{}:request", self.name));
            None
        }

        fn on_operation(
            &self,
            _context: &mut HttpRequestContext<'_>,
            operation: &OperationDescriptor,
            _security_schemes: &[SecuritySchemeDescriptor],
        ) -> Option<Response> {
            self.events.borrow_mut().push(format!(
                "{}:operation:{}",
                self.name,
                operation.contract.id.as_str()
            ));
            None
        }

        fn on_response(
            &self,
            _context: &HttpRequestContext<'_>,
            _operation: Option<&OperationDescriptor>,
            _response: &mut Response,
        ) {
            self.events
                .borrow_mut()
                .push(format!("{}:response", self.name));
        }
    }

    struct StatusStamp;

    impl HttpMiddleware for StatusStamp {
        fn on_response(
            &self,
            _context: &HttpRequestContext<'_>,
            _operation: Option<&OperationDescriptor>,
            response: &mut Response,
        ) {
            let status = response.status().to_string();
            response.set_header("x-final-status", status);
        }

        fn verifies_security(&self) -> bool {
            false
        }
    }

    struct HouseStyle;

    impl HttpErrorHandler for HouseStyle {
        fn on_error(&self, error: &HttpError<'_>, response: &mut Response) {
            response.set_header("x-error-source", format!("{:?}", error.source()));
            match error.source() {
                HttpErrorSource::Internal => {
                    response.set_status(503);
                    response.replace_body(br#"{"error":{"code":"unavailable"}}"#.to_vec());
                }
                // Deliberately illegal: a typed status is contract, not style.
                HttpErrorSource::Domain => response.set_status(500),
                HttpErrorSource::Routing
                | HttpErrorSource::Request
                | HttpErrorSource::Rejection => {}
            }
        }
    }

    fn scoped_executable() -> ExecutableApp {
        ExecutableApp::new([
            connection_operation("/ingest/events", "ingest.events", Vec::new()),
            connection_operation("/status", "status.read", Vec::new()),
        ])
        .expect("scoped operation graph should compile")
    }

    fn ack_descriptor(path: &str, id: &str) -> OperationDescriptor {
        OperationDescriptor::new(
            HttpMethod::Get,
            path,
            id,
            "Reports an acknowledgement",
            None,
            vec![ResponseDescriptor::success(
                200,
                Some(TypeDescriptor::new("Ack")),
            )],
        )
        .expect("test operation id should be valid")
    }

    fn outcome_operation(
        path: &str,
        id: &str,
        outcome: fn() -> ExecutionOutcome,
    ) -> ExecutableOperation {
        ExecutableOperation::typed(ack_descriptor(path, id), move |_| {
            Ok(Box::pin(async move { outcome() }) as OperationFuture)
        })
    }

    fn extension_handle(input: &InvocationInput<'_>) -> Result<BackgroundTasks, InputRejection> {
        Extension::<BackgroundTasks>::from_invocation(input, "background", true)
            .map(|Extension(tasks)| tasks)
    }

    fn bare_handle(input: &InvocationInput<'_>) -> Result<BackgroundTasks, InputRejection> {
        BackgroundTasks::from_invocation(input, "background", true)
    }

    /// An operation that decides mid-body to schedule after-response work,
    /// which the `Background<T>` return type cannot express.
    fn scheduling_operation(
        path: &str,
        id: &str,
        log: &EventLog,
        extract: HandleExtractor,
        outcome: fn() -> ExecutionOutcome,
    ) -> ExecutableOperation {
        let log = Rc::clone(log);
        ExecutableOperation::typed(ack_descriptor(path, id), move |input| {
            let tasks = extract(&input)?;
            let log = Rc::clone(&log);
            tasks.add_infallible(move || async move {
                log.borrow_mut().push("task".to_owned());
            });
            Ok(Box::pin(async move { outcome() }) as OperationFuture)
        })
    }

    fn accepted() -> ExecutionOutcome {
        ExecutionOutcome::Success {
            status: 200,
            headers: Vec::new(),
            body: None,
            background: Vec::new(),
        }
    }

    fn conflict() -> ExecutionOutcome {
        ExecutionOutcome::DomainError(OperationFailure::new(
            409,
            "conflict",
            "the event was already ingested",
        ))
    }

    #[test]
    fn a_path_prefix_matches_only_on_segment_boundaries() {
        let scope = MiddlewareScope::prefix("/ingest");
        assert!(scope.matches_request("/ingest"));
        assert!(scope.matches_request("/ingest/events"));
        assert!(!scope.matches_request("/ingested"));
        assert!(!scope.matches_request("/"));
        assert!(MiddlewareScope::prefix("ingest/").matches_request("/ingest/events"));
        assert!(MiddlewareScope::all().is_global());
        assert!(!scope.is_global());
    }

    #[test]
    fn operation_predicates_select_by_id_after_routing() {
        let scope = MiddlewareScope::all().with_operation_prefix("ingest.");
        assert!(scope.matches_operation("/anything", "ingest.events"));
        assert!(!scope.matches_operation("/anything", "status.read"));
        assert!(!scope.matches_request("/anything"));

        let scope = MiddlewareScope::all()
            .with_operation_filter(|id| id.split('.').next_back() == Some("read"));
        assert!(scope.matches_operation("/anything", "status.read"));
        assert!(!scope.matches_operation("/anything", "status.write"));

        let scope = MiddlewareScope::prefix("/ingest").with_operation("status.read");
        assert!(!scope.matches_operation("/status", "status.read"));
    }

    #[test]
    fn a_prefix_scoped_layer_observes_only_its_subtree() {
        let executable = scoped_executable();
        let events: EventLog = Rc::new(RefCell::new(Vec::new()));
        let app = TestApp::new(&executable).with_scoped_middleware(
            MiddlewareScope::prefix("/ingest"),
            RecordingLayer {
                name: "ingest",
                events: Rc::clone(&events),
            },
        );

        future::block_on(app.call(Request::get("/ingest/events")));
        let recorded = events.borrow().clone();
        assert_eq!(
            recorded,
            [
                "ingest:request",
                "ingest:operation:ingest.events",
                "ingest:response"
            ]
        );

        events.borrow_mut().clear();
        future::block_on(app.call(Request::get("/status")));
        assert!(events.borrow().is_empty());
    }

    #[test]
    fn an_operation_scoped_layer_starts_after_routing() {
        let executable = scoped_executable();
        let events: EventLog = Rc::new(RefCell::new(Vec::new()));
        let app = TestApp::new(&executable).with_scoped_middleware(
            MiddlewareScope::operation("ingest.events"),
            RecordingLayer {
                name: "op",
                events: Rc::clone(&events),
            },
        );

        future::block_on(app.call(Request::get("/ingest/events")));
        let recorded = events.borrow().clone();
        assert_eq!(recorded, ["op:operation:ingest.events", "op:response"]);

        events.borrow_mut().clear();
        future::block_on(app.call(Request::get("/status")));
        assert!(events.borrow().is_empty());
    }

    #[test]
    fn a_scoped_verifier_does_not_cover_operations_outside_its_scope() {
        let executable = executable();
        let outside = future::block_on(
            TestApp::new(&executable)
                .with_scoped_middleware(MiddlewareScope::prefix("/other"), PassthroughLayer)
                .call(Request::get("/secure")),
        );
        assert_eq!(outside.status(), 500);
        assert_eq!(error_code(&outside), "security_verifier_missing");

        let inside = future::block_on(
            TestApp::new(&executable)
                .with_scoped_middleware(MiddlewareScope::prefix("/secure"), PassthroughLayer)
                .call(Request::get("/secure")),
        );
        assert_eq!(inside.status(), 200);
    }

    #[test]
    fn an_injected_handle_schedules_work_from_the_handler_body() {
        let log: EventLog = Rc::new(RefCell::new(Vec::new()));
        let executable = ExecutableApp::new([scheduling_operation(
            "/ingest",
            "tasks.ok",
            &log,
            extension_handle,
            accepted,
        )])
        .expect("scheduling operation graph should compile");

        let mut response =
            future::block_on(TestApp::new(&executable).call(Request::get("/ingest")));
        assert_eq!(response.status(), 200);
        assert!(log.borrow().is_empty());

        future::block_on(response.run_background()).expect("scheduled task");
        assert_eq!(log.borrow().clone(), ["task"]);
    }

    #[test]
    fn scheduled_work_survives_a_failed_outcome() {
        let log: EventLog = Rc::new(RefCell::new(Vec::new()));
        let executable = ExecutableApp::new([scheduling_operation(
            "/ingest",
            "tasks.conflict",
            &log,
            bare_handle,
            conflict,
        )])
        .expect("scheduling operation graph should compile");

        let mut response =
            future::block_on(TestApp::new(&executable).call(Request::get("/ingest")));
        assert_eq!(response.status(), 409);
        assert_eq!(error_code(&response), "conflict");

        future::block_on(response.run_background()).expect("scheduled task");
        assert_eq!(log.borrow().clone(), ["task"]);
    }

    /// An adapter timeout that fires as soon as the handler has scheduled its
    /// after-response work.
    struct ReadyWhenScheduled {
        scheduled: Rc<Cell<bool>>,
    }

    impl Future for ReadyWhenScheduled {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<()> {
            if self.scheduled.get() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    fn aborting_operation(log: &EventLog, scheduled: &Rc<Cell<bool>>) -> ExecutableOperation {
        let log = Rc::clone(log);
        let scheduled = Rc::clone(scheduled);
        ExecutableOperation::typed(ack_descriptor("/ingest", "tasks.aborted"), move |input| {
            let tasks = bare_handle(&input)?;
            let log = Rc::clone(&log);
            tasks.add_infallible(move || async move {
                log.borrow_mut().push("task".to_owned());
            });
            scheduled.set(true);
            Ok(Box::pin(async move { accepted() }) as OperationFuture)
        })
    }

    #[test]
    fn scheduled_work_survives_an_aborted_invocation() {
        let log: EventLog = Rc::new(RefCell::new(Vec::new()));
        let scheduled = Rc::new(Cell::new(false));
        let executable = ExecutableApp::new([aborting_operation(&log, &scheduled)])
            .expect("scheduling operation graph should compile");
        let control = InvocationControl::new().with_timeout(ReadyWhenScheduled {
            scheduled: Rc::clone(&scheduled),
        });

        let mut response = future::block_on(
            TestApp::new(&executable).call_controlled(Request::get("/ingest"), control),
        );
        assert_eq!(response.status(), 504);

        future::block_on(response.run_background()).expect("scheduled task");
        assert_eq!(log.borrow().clone(), ["task"]);
    }

    #[test]
    fn an_unused_handle_leaves_the_response_untouched() {
        let executable = scoped_executable();
        let mut response =
            future::block_on(TestApp::new(&executable).call(Request::get("/status")));

        assert_eq!(response.status(), 200);
        assert!(response.take_background_tasks().is_empty());
    }

    #[test]
    fn an_error_handler_restyles_every_dispatch_failure() {
        let executable = executable();
        let app = TestApp::new(&executable).with_error_handler(HouseStyle);

        let missing = future::block_on(app.call(Request::get("/nope")));
        assert_eq!(missing.status(), 404);
        assert_eq!(missing.get_header("x-error-source"), Some("Routing"));

        let unverified = future::block_on(app.call(Request::get("/secure")));
        assert_eq!(unverified.status(), 503);
        assert_eq!(unverified.get_header("x-error-source"), Some("Internal"));
        assert_eq!(error_code(&unverified), "unavailable");

        let allowed = future::block_on(app.call(Request::post("/plain")));
        assert_eq!(allowed.status(), 405);
        assert_eq!(allowed.get_header("allow"), Some("GET"));
        assert_eq!(allowed.get_header("x-error-source"), Some("Routing"));
    }

    #[test]
    fn a_typed_domain_status_survives_the_error_handler() {
        let executable = ExecutableApp::new([
            outcome_operation("/conflict", "errors.conflict", conflict),
            outcome_operation("/broken", "errors.broken", || {
                ExecutionOutcome::InternalError {
                    code: "boom".to_owned(),
                    message: "boom".to_owned(),
                }
            }),
        ])
        .expect("error operation graph should compile");
        let app = TestApp::new(&executable).with_error_handler(HouseStyle);

        let domain = future::block_on(app.call(Request::get("/conflict")));
        assert_eq!(domain.status(), 409);
        assert_eq!(error_code(&domain), "conflict");
        assert_eq!(domain.get_header("x-error-source"), Some("Domain"));

        let internal = future::block_on(app.call(Request::get("/broken")));
        assert_eq!(internal.status(), 503);
        assert_eq!(error_code(&internal), "unavailable");
    }

    #[test]
    fn error_handlers_run_before_middleware_sees_the_response() {
        let executable = executable();
        let response = future::block_on(
            TestApp::new(&executable)
                .with_error_handler(HouseStyle)
                .with_middleware(StatusStamp)
                .call(Request::get("/secure")),
        );

        assert_eq!(response.status(), 503);
        assert_eq!(response.get_header("x-final-status"), Some("503"));
    }

    #[test]
    fn the_owned_adapter_registers_the_same_seams() {
        let app = HttpApp::new(executable())
            .with_scoped_middleware(MiddlewareScope::prefix("/other"), PassthroughLayer)
            .with_error_handler(HouseStyle);
        let response = future::block_on(app.call(Request::get("/secure")));

        assert_eq!(response.status(), 503);
        assert_eq!(response.get_header("x-error-source"), Some("Internal"));
    }

    #[test]
    fn normalized_connection_reaches_the_operation_context() {
        let executable = executable();
        let request = || {
            Request::get("/plain")
                .peer_addr(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9000).into())
                .header("host", "internal:8080")
        };

        let unlayered = future::block_on(TestApp::new(&executable).call(request()));
        let body: Value = unlayered.json().expect("connection body");
        assert_eq!(body["scheme"], "http");
        assert_eq!(body["host"], "internal:8080");
        assert_eq!(body["clientIp"], "127.0.0.1");

        let layered = future::block_on(
            TestApp::new(&executable)
                .with_middleware(NormalizingProxy)
                .call(request()),
        );
        let body: Value = layered.json().expect("connection body");
        assert_eq!(body["scheme"], "https");
        assert_eq!(body["host"], "api.example");
        assert_eq!(body["clientIp"], "198.51.100.7");
    }

    fn router_operation(method: HttpMethod, path: &str, id: &str) -> ExecutableOperation {
        let inputs = path
            .split('/')
            .filter_map(|segment| {
                segment
                    .strip_prefix('{')
                    .and_then(|segment| segment.strip_suffix('}'))
            })
            .map(|name| {
                InputDescriptor::new(name, InputSource::Path, true, TypeDescriptor::new("String"))
            })
            .collect::<Vec<_>>();
        let descriptor = OperationDescriptor::new(
            method,
            path,
            id,
            "Router fixture",
            None,
            vec![ResponseDescriptor::success(200, None)],
        )
        .expect("test operation id should be valid")
        .with_inputs(inputs);
        ExecutableOperation::typed(descriptor, |_| {
            Ok(Box::pin(async move {
                ExecutionOutcome::Success {
                    status: 200,
                    headers: Vec::new(),
                    body: None,
                    background: Vec::new(),
                }
            }) as OperationFuture)
        })
    }

    fn router(routes: &[(HttpMethod, &str)]) -> Router {
        let operations = routes
            .iter()
            .enumerate()
            .map(|(index, (method, path))| {
                router_operation(*method, path, &format!("router.op{index}"))
            })
            .collect::<Vec<_>>();
        let app = ExecutableApp::new(operations).expect("router fixture compiles");
        Router::new(&app)
    }

    fn allowed(router: &Router, method: HttpMethod, path: &str) -> Vec<HttpMethod> {
        match router.recognize(method, path) {
            Err(RouteError::MethodNotAllowed { allowed }) => allowed,
            other => panic!("{method:?} {path} should be a 405, got {:?}", other.is_ok()),
        }
    }

    /// The 405 `Allow` list is rendered by walking the method table low bit
    /// first, so its order is the enum's `Ord`, not alphabetical.
    #[test]
    fn the_method_table_matches_the_http_method_order() {
        let mut sorted = super::METHODS;
        sorted.sort_unstable();
        assert_eq!(sorted, super::METHODS);
        for (index, method) in super::METHODS.into_iter().enumerate() {
            assert_eq!(super::method_index(method), index);
        }
    }

    #[test]
    fn a_static_path_outranks_a_parameter_of_the_same_shape() {
        let router = router(&[
            (HttpMethod::Get, "/items"),
            (HttpMethod::Post, "/items"),
            (HttpMethod::Get, "/items/latest"),
            (HttpMethod::Put, "/items/latest"),
            (HttpMethod::Get, "/items/{id}"),
            (HttpMethod::Delete, "/items/{id}"),
        ]);

        let statically = router
            .recognize(HttpMethod::Get, "/items/latest")
            .expect("static route");
        assert_eq!(statically.path_parameter("id"), None);

        // No static DELETE for that path, so the walk falls through to the
        // parameter route and captures the segment the static table matched.
        let dynamically = router
            .recognize(HttpMethod::Delete, "/items/latest")
            .expect("parameter route");
        assert_eq!(
            dynamically.path_parameter("id").as_deref(),
            Some("latest"),
            "the static probe must not consume the segment"
        );
    }

    #[test]
    fn an_allow_list_unions_static_and_parameter_routes_in_enum_order() {
        let router = router(&[
            (HttpMethod::Get, "/items"),
            (HttpMethod::Post, "/items"),
            (HttpMethod::Get, "/items/latest"),
            (HttpMethod::Put, "/items/latest"),
            (HttpMethod::Get, "/items/{id}"),
            (HttpMethod::Delete, "/items/{id}"),
        ]);

        assert_eq!(
            allowed(&router, HttpMethod::Patch, "/items"),
            vec![HttpMethod::Get, HttpMethod::Post]
        );
        assert_eq!(
            allowed(&router, HttpMethod::Post, "/items/latest"),
            vec![HttpMethod::Get, HttpMethod::Put, HttpMethod::Delete],
            "PUT precedes DELETE in the enum even though it follows alphabetically"
        );
        assert_eq!(
            allowed(&router, HttpMethod::Post, "/items/7"),
            vec![HttpMethod::Get, HttpMethod::Delete]
        );
    }

    #[test]
    fn an_unknown_path_is_a_miss_rather_than_a_wrong_method() {
        let router = router(&[
            (HttpMethod::Get, "/items"),
            (HttpMethod::Get, "/items/{id}/tags/{tag}"),
        ]);

        for path in ["/nope", "/items/7/tags", "/items/7/tags/x/y", "/", ""] {
            assert_eq!(
                router.recognize(HttpMethod::Get, path).err(),
                Some(RouteError::NotFound),
                "{path} should not exist"
            );
        }
    }

    #[test]
    fn a_failed_static_descent_backtracks_into_the_parameter_child() {
        let router = router(&[
            (HttpMethod::Get, "/a/{p}/x"),
            (HttpMethod::Post, "/a/{p}/x"),
            (HttpMethod::Get, "/a/b/y"),
        ]);

        // `b` matches the static child, which dead-ends at `x`; the walk has to
        // unwind and take the parameter child with the same segment.
        let matched = router
            .recognize(HttpMethod::Get, "/a/b/x")
            .expect("parameter route after backtracking");
        assert_eq!(matched.path_parameter("p").as_deref(), Some("b"));

        let statically = router
            .recognize(HttpMethod::Get, "/a/b/y")
            .expect("static branch");
        assert_eq!(statically.path_parameter("p"), None);

        assert_eq!(
            router.recognize(HttpMethod::Get, "/a/c/y").err(),
            Some(RouteError::NotFound)
        );
        assert_eq!(
            allowed(&router, HttpMethod::Delete, "/a/b/x"),
            vec![HttpMethod::Get, HttpMethod::Post]
        );
    }

    #[test]
    fn captures_survive_a_deep_backtrack() {
        let router = router(&[
            (HttpMethod::Get, "/{one}/{two}/{three}/leaf"),
            (HttpMethod::Get, "/a/b/c/other"),
        ]);

        let matched = router
            .recognize(HttpMethod::Get, "/a/b/c/leaf")
            .expect("parameter route");
        assert_eq!(matched.path_parameter("one").as_deref(), Some("a"));
        assert_eq!(matched.path_parameter("two").as_deref(), Some("b"));
        assert_eq!(matched.path_parameter("three").as_deref(), Some("c"));
        assert_eq!(matched.path_parameter("four"), None);
    }

    #[test]
    fn a_captured_segment_is_percent_decoded() {
        let router = router(&[(HttpMethod::Get, "/files/{name}")]);

        let matched = router
            .recognize(HttpMethod::Get, "/files/a%20b%2Fc")
            .expect("parameter route");
        assert_eq!(matched.path_parameter("name").as_deref(), Some("a b/c"));

        let plus = router
            .recognize(HttpMethod::Get, "/files/a+b")
            .expect("parameter route");
        assert_eq!(
            plus.path_parameter("name").as_deref(),
            Some("a+b"),
            "a path segment is not form encoded"
        );

        let invalid = router
            .recognize(HttpMethod::Get, "/files/a%2")
            .expect("parameter route");
        assert_eq!(invalid.path_parameter("name"), None);
    }

    #[test]
    fn empty_segments_are_matched_literally() {
        let router = router(&[(HttpMethod::Get, "/files/{name}/meta")]);

        let matched = router
            .recognize(HttpMethod::Get, "/files//meta")
            .expect("empty segments are still segments");
        assert_eq!(matched.path_parameter("name").as_deref(), Some(""));
    }

    #[test]
    fn every_method_reaches_its_own_static_slot() {
        let bindings = super::METHODS
            .into_iter()
            .map(|method| (method, "/one"))
            .collect::<Vec<_>>();
        let router = router(&bindings);

        let mut seen = Vec::new();
        for method in super::METHODS {
            let matched = router.recognize(method, "/one").expect("static route");
            seen.push(matched.operation_index());
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), super::METHOD_COUNT, "slots must not alias");
    }
}
