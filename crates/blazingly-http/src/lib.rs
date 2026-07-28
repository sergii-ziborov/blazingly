#![forbid(unsafe_code)]

use blazingly_core::{
    BackgroundTask, BackgroundTaskError, BodyStreamError, HttpMethod, HttpUpgrade, InputSource,
    OperationDescriptor, SecuritySchemeDescriptor, StreamingBody,
};
use blazingly_executor::{
    DependencyError, ExecutableApp, ExecutionOutcome, HttpRequestParts as InvocationRequestParts,
    InvocationControl,
};
use blazingly_openapi::{OpenApiAssetResponse, OpenApiConfig, OpenApiService};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::any::{Any, TypeId};
use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::rc::Rc;
use std::str::Utf8Error;

pub const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;

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
    pub fn json(mut self, value: &impl Serialize) -> Result<Self, serde_json::Error> {
        self.body = serde_json::to_vec(value)?;
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
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
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
    middleware: Vec<Rc<dyn HttpMiddleware>>,
    allow_unverified_security_schemes: bool,
}

impl HttpApp {
    #[must_use]
    pub fn new(app: ExecutableApp) -> Self {
        let router = Router::new(&app);
        Self {
            app,
            router,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            openapi: None,
            middleware: Vec::new(),
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

    /// Registers runtime-neutral HTTP middleware.
    #[must_use]
    pub fn with_middleware(mut self, middleware: impl HttpMiddleware + 'static) -> Self {
        self.middleware.push(Rc::new(middleware));
        self
    }

    /// Registers shared middleware state.
    #[must_use]
    pub fn with_shared_middleware(mut self, middleware: Rc<dyn HttpMiddleware>) -> Self {
        self.middleware.push(middleware);
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

/// An in-memory borrowed HTTP adapter over the shared executable operation graph.
pub struct TestApp<'app> {
    app: &'app ExecutableApp,
    router: Router,
    max_body_bytes: usize,
    openapi: Option<OpenApiService>,
    middleware: Vec<Rc<dyn HttpMiddleware>>,
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

    /// Registers runtime-neutral HTTP middleware.
    #[must_use]
    pub fn with_middleware(mut self, middleware: impl HttpMiddleware + 'static) -> Self {
        self.middleware.push(Rc::new(middleware));
        self
    }

    /// Registers shared middleware state.
    #[must_use]
    pub fn with_shared_middleware(mut self, middleware: Rc<dyn HttpMiddleware>) -> Self {
        self.middleware.push(middleware);
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
    middleware: &'app [Rc<dyn HttpMiddleware>],
    allow_unverified_security_schemes: bool,
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
        let mut context = HttpRequestContext::new(request);
        for layer in middleware {
            if let Some(response) = layer.on_request(&mut context) {
                return complete_response(middleware, &context, None, response);
            }
        }

        let target = request.target();
        if validate_url_encoding(target).is_err() {
            return complete_response(
                middleware,
                &context,
                None,
                error_response(
                    400,
                    "invalid_url_encoding",
                    "request target contains invalid percent encoding",
                    None,
                ),
            );
        }
        let path = target.split_once('?').map_or(target, |(path, _)| path);
        if let Some(response) = self
            .openapi
            .and_then(|service| service.handle(request.method(), path))
        {
            return complete_response(middleware, &context, None, openapi_response(response));
        }
        let recognized = match self.router.recognize(request.method(), path) {
            Ok(recognized) => recognized,
            Err(error) => {
                return complete_response(middleware, &context, None, route_miss_response(error));
            }
        };
        let Some(operation) = self.app.operation_at(recognized.operation_index()) else {
            return complete_response(middleware, &context, None, internal_error_response());
        };
        let descriptor = operation.descriptor();
        for layer in middleware {
            if let Some(response) = layer.on_operation(
                &mut context,
                descriptor,
                self.app.definition().security_schemes(),
            ) {
                return complete_response(middleware, &context, Some(descriptor), response);
            }
        }
        if let Some(response) = self.security_guard(descriptor) {
            return complete_response(middleware, &context, Some(descriptor), response);
        }
        if let Some(body_source) = recognized.body_source() {
            match validate_body(request, self.max_body_bytes, body_source) {
                Ok(()) => {}
                Err(rejection) => {
                    return complete_response(
                        middleware,
                        &context,
                        Some(descriptor),
                        rejection.into_response(),
                    );
                }
            }
        }
        let request_parts = RoutedRequestParts {
            request,
            route: &recognized,
            context: Some(&context),
            connection: OnceCell::new(),
        };
        let outcome = if let Some(control) = control {
            operation
                .invoke_http_controlled(&request_parts, control)
                .await
        } else {
            operation.invoke_http(&request_parts).await
        };
        complete_response(
            middleware,
            &context,
            Some(descriptor),
            outcome_response(outcome),
        )
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
        if validate_url_encoding(target).is_err() {
            return error_response(
                400,
                "invalid_url_encoding",
                "request target contains invalid percent encoding",
                None,
            );
        }
        let path = target.split_once('?').map_or(target, |(path, _)| path);
        if let Some(response) = self
            .openapi
            .and_then(|service| service.handle(request.method(), path))
        {
            return openapi_response(response);
        }
        let recognized = match self.router.recognize(request.method(), path) {
            Ok(recognized) => recognized,
            Err(error) => return route_miss_response(error),
        };
        let Some(operation) = self.app.operation_at(recognized.operation_index()) else {
            return internal_error_response();
        };
        if let Some(response) = self.security_guard(operation.descriptor()) {
            return response;
        }
        if let Some(body_source) = recognized.body_source() {
            match validate_body(request, self.max_body_bytes, body_source) {
                Ok(()) => {}
                Err(rejection) => return rejection.into_response(),
            }
        }
        let request_parts = RoutedRequestParts {
            request,
            route: &recognized,
            context: None,
            connection: OnceCell::new(),
        };
        let outcome = if let Some(control) = control {
            operation
                .invoke_http_controlled(&request_parts, control)
                .await
        } else {
            operation.invoke_http(&request_parts).await
        };
        outcome_response(outcome)
    }

    /// Fails closed when the matched operation declares a security scheme that
    /// no layer on this dispatch path can verify.
    ///
    /// Both dispatch paths run this before invoking an operation, so an
    /// unlayered path never serves a declared scheme unauthenticated.
    fn security_guard(&self, descriptor: &OperationDescriptor) -> Option<Response> {
        if self.allow_unverified_security_schemes || descriptor.contract.security.is_empty() {
            return None;
        }
        if self
            .middleware
            .iter()
            .any(|layer| layer.verifies_security())
        {
            return None;
        }
        Some(error_response(
            500,
            "security_verifier_missing",
            "the operation declares a security scheme with no registered verifier",
            None,
        ))
    }
}

fn route_miss_response(error: RouteError) -> Response {
    match error {
        RouteError::MethodNotAllowed { allowed } => {
            let allow = allowed
                .iter()
                .map(|method| method.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            error_response(405, "method_not_allowed", "HTTP method not allowed", None)
                .with_header("allow", allow)
        }
        RouteError::NotFound => error_response(404, "not_found", "HTTP route not found", None),
    }
}

fn complete_response(
    middleware: &[Rc<dyn HttpMiddleware>],
    context: &HttpRequestContext<'_>,
    operation: Option<&OperationDescriptor>,
    mut response: Response,
) -> Response {
    for layer in middleware.iter().rev() {
        layer.on_response(context, operation, &mut response);
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

#[derive(Default)]
struct RouteNode {
    static_children: HashMap<String, usize>,
    parameter_child: Option<usize>,
    endpoints: BTreeMap<HttpMethod, CompiledEndpoint>,
}

#[derive(Clone)]
struct CompiledEndpoint {
    operation_index: usize,
    parameter_names: Vec<String>,
    body_source: Option<InputSource>,
}

/// A runtime-neutral router compiled once from the operation graph.
pub struct Router {
    nodes: Vec<RouteNode>,
    static_routes: HashMap<HttpMethod, HashMap<String, CompiledEndpoint>>,
}

impl Router {
    #[must_use]
    pub fn new(app: &ExecutableApp) -> Self {
        let mut router = Self {
            nodes: vec![RouteNode::default()],
            static_routes: HashMap::new(),
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
        if endpoint.parameter_names.is_empty() {
            self.static_routes
                .entry(descriptor.http.method)
                .or_default()
                .insert(descriptor.http.path.clone(), endpoint);
            return;
        }

        let mut node_index = 0;
        for segment in route_segments(&descriptor.http.path) {
            if path_parameter_name(segment).is_some() {
                node_index = if let Some(child) = self.nodes[node_index].parameter_child {
                    child
                } else {
                    let child = self.nodes.len();
                    self.nodes.push(RouteNode::default());
                    self.nodes[node_index].parameter_child = Some(child);
                    child
                };
            } else if let Some(child) = self.nodes[node_index].static_children.get(segment) {
                node_index = *child;
            } else {
                let child = self.nodes.len();
                self.nodes.push(RouteNode::default());
                self.nodes[node_index]
                    .static_children
                    .insert(segment.to_owned(), child);
                node_index = child;
            }
        }
        self.nodes[node_index]
            .endpoints
            .insert(descriptor.http.method, endpoint);
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
        if let Some(endpoint) = self
            .static_routes
            .get(&method)
            .and_then(|routes| routes.get(path))
        {
            return Ok(RouteMatch {
                endpoint,
                captures: CapturedSegments::new(),
            });
        }

        if let Some((endpoint, captures)) =
            self.find_dynamic(0, route_segments(path), method, CapturedSegments::new())
        {
            return Ok(RouteMatch { endpoint, captures });
        }

        let mut allowed = self
            .static_routes
            .iter()
            .filter_map(|(allowed_method, routes)| {
                routes.contains_key(path).then_some(*allowed_method)
            })
            .collect::<Vec<_>>();
        self.collect_dynamic_methods(0, route_segments(path), &mut allowed);
        allowed.sort_unstable();
        allowed.dedup();
        if allowed.is_empty() {
            Err(RouteError::NotFound)
        } else {
            Err(RouteError::MethodNotAllowed { allowed })
        }
    }

    fn find_dynamic<'router, 'path, I>(
        &'router self,
        node_index: usize,
        mut segments: I,
        method: HttpMethod,
        captures: CapturedSegments<'path>,
    ) -> Option<(&'router CompiledEndpoint, CapturedSegments<'path>)>
    where
        I: Iterator<Item = &'path str> + Clone,
    {
        let Some(segment) = segments.next() else {
            return self.nodes[node_index]
                .endpoints
                .get(&method)
                .map(|endpoint| (endpoint, captures));
        };

        if let Some(child) = self.nodes[node_index].static_children.get(segment)
            && let Some(found) =
                self.find_dynamic(*child, segments.clone(), method, captures.clone())
        {
            return Some(found);
        }
        let child = self.nodes[node_index].parameter_child?;
        let mut captures = captures;
        captures.push(segment);
        self.find_dynamic(child, segments, method, captures)
    }

    fn collect_dynamic_methods<'path, I>(
        &self,
        node_index: usize,
        mut segments: I,
        methods: &mut Vec<HttpMethod>,
    ) where
        I: Iterator<Item = &'path str> + Clone,
    {
        let Some(segment) = segments.next() else {
            methods.extend(self.nodes[node_index].endpoints.keys().copied());
            return;
        };
        if let Some(child) = self.nodes[node_index].static_children.get(segment) {
            self.collect_dynamic_methods(*child, segments.clone(), methods);
        }
        if let Some(child) = self.nodes[node_index].parameter_child {
            self.collect_dynamic_methods(child, segments, methods);
        }
    }
}

fn route_segments(path: &str) -> std::str::Split<'_, char> {
    path.strip_prefix('/').unwrap_or(path).split('/')
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

#[derive(Clone)]
enum CapturedSegments<'path> {
    Inline {
        values: [Option<&'path str>; INLINE_PATH_PARAMETERS],
        len: usize,
    },
    Heap(Vec<&'path str>),
}

impl<'path> CapturedSegments<'path> {
    const fn new() -> Self {
        Self::Inline {
            values: [None; INLINE_PATH_PARAMETERS],
            len: 0,
        }
    }

    fn push(&mut self, value: &'path str) {
        match self {
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

    fn get(&self, index: usize) -> Option<&'path str> {
        match self {
            Self::Inline { values, len } if index < *len => values[index],
            Self::Inline { .. } => None,
            Self::Heap(values) => values.get(index).copied(),
        }
    }
}

struct RoutedRequestParts<'request, 'router, 'path, 'context, RequestView: ?Sized> {
    request: &'request RequestView,
    route: &'request RouteMatch<'router, 'path>,
    context: Option<&'context HttpRequestContext<'request>>,
    connection: OnceCell<ConnectionInfo>,
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
        None
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
    fn into_response(self) -> Response {
        match self {
            Self::PayloadTooLarge { max_body_bytes } => error_response(
                413,
                "payload_too_large",
                "request body exceeds the configured limit",
                Some(json!({ "maxBytes": max_body_bytes })),
            ),
            Self::UnsupportedMediaType => error_response(
                415,
                "unsupported_media_type",
                "request body media type does not match the operation input",
                None,
            ),
        }
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

fn outcome_response(outcome: ExecutionOutcome) -> Response {
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
            response
        }
        ExecutionOutcome::StreamingSuccess {
            status,
            headers,
            body,
            background,
        } => with_outcome_headers(
            Response {
                status,
                headers: ResponseHeaders::empty(),
                body: Vec::new(),
                stream: Some(body),
                upgrade: None,
                background,
            },
            headers,
        ),
        ExecutionOutcome::Upgrade {
            upgrade,
            background,
        } => {
            let headers = upgrade.headers().to_vec();
            with_outcome_headers(
                Response {
                    status: 101,
                    headers: ResponseHeaders::empty(),
                    body: Vec::new(),
                    stream: None,
                    upgrade: Some(upgrade),
                    background,
                },
                headers,
            )
        }
        ExecutionOutcome::Rejected {
            status,
            code,
            message,
            details,
        } => error_response(status, &code, &message, details),
        ExecutionOutcome::DomainError(error) => {
            let details = match error.details {
                Some(details) => match serde_json::from_slice(&details) {
                    Ok(details) => Some(details),
                    Err(_) => return internal_error_response(),
                },
                None => None,
            };
            let response = error_response(error.status, &error.code, &error.message, details);
            with_outcome_headers(response, error.headers)
        }
        ExecutionOutcome::InternalError { .. } => internal_error_response(),
    }
}

fn with_outcome_headers(
    mut response: Response,
    headers: Vec<blazingly_core::ResponseHeader>,
) -> Response {
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

fn internal_error_response() -> Response {
    error_response(
        500,
        "internal_error",
        "the operation could not be completed",
        None,
    )
}

fn json_response(status: u16, value: &Value) -> Response {
    let Ok(body) = serde_json::to_vec(value) else {
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
        ConnectionInfo, HttpApp, HttpMiddleware, HttpRequestContext, Request, Response, TestApp,
    };
    use blazingly_core::{
        HttpMethod, OperationDescriptor, PreparedJson, ResponseDescriptor, SecurityLocation,
        SecurityRequirement, SecuritySchemeDescriptor, SecuritySchemeKind, TypeDescriptor,
    };
    use blazingly_executor::{
        ExecutableApp, ExecutableOperation, ExecutionOutcome, Extension, FromInvocation,
        OperationFuture, OperationOutput,
    };
    use futures_lite::future;
    use serde_json::{Value, json};
    use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};

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
                    body: Some(serde_json::to_vec(&body).expect("connection body")),
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
}
