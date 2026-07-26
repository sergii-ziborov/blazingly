#![forbid(unsafe_code)]

use blazingly_core::{
    BodyStreamError, HttpMethod, InputSource, OperationDescriptor, StreamingBody,
};
use blazingly_executor::{
    ExecutableApp, ExecutionOutcome, HttpRequestParts as InvocationRequestParts, InvocationControl,
};
use blazingly_openapi::{OpenApiAssetResponse, OpenApiConfig, OpenApiService};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::str::Utf8Error;

pub const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;

/// A runtime-neutral HTTP request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    method: HttpMethod,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
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
}

impl Request {
    #[must_use]
    pub fn new(method: HttpMethod, target: impl Into<String>) -> Self {
        Self {
            method,
            target: target.into(),
            headers: BTreeMap::new(),
            body: Vec::new(),
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
}

/// A runtime-neutral HTTP response.
#[derive(Debug)]
pub struct Response {
    status: u16,
    headers: ResponseHeaders,
    body: Vec<u8>,
    stream: Option<StreamingBody>,
}

impl Response {
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

    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Returns whether this response owns a pull-based streaming body.
    #[must_use]
    pub const fn is_streaming(&self) -> bool {
        self.stream.is_some()
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
        }
    }

    #[must_use]
    pub const fn with_max_body_bytes(mut self, max_body_bytes: usize) -> Self {
        self.max_body_bytes = max_body_bytes;
        self
    }

    /// Mounts precompiled `OpenAPI` JSON and UI assets.
    #[must_use]
    pub fn with_openapi(mut self, config: OpenApiConfig) -> Self {
        self.openapi = Some(OpenApiService::new(self.app.definition(), config));
        self
    }

    pub async fn call(&self, request: Request) -> Response {
        self.call_view(&request).await
    }

    pub async fn call_view(&self, request: &impl HttpRequestView) -> Response {
        dispatch(
            &self.app,
            &self.router,
            self.max_body_bytes,
            self.openapi.as_ref(),
            request,
            None,
        )
        .await
    }

    pub async fn call_view_controlled(
        &self,
        request: &impl HttpRequestView,
        control: InvocationControl,
    ) -> Response {
        dispatch(
            &self.app,
            &self.router,
            self.max_body_bytes,
            self.openapi.as_ref(),
            request,
            Some(control),
        )
        .await
    }
}

/// An in-memory borrowed HTTP adapter over the shared executable operation graph.
pub struct TestApp<'app> {
    app: &'app ExecutableApp,
    router: Router,
    max_body_bytes: usize,
    openapi: Option<OpenApiService>,
}

impl<'app> TestApp<'app> {
    #[must_use]
    pub fn new(app: &'app ExecutableApp) -> Self {
        Self {
            app,
            router: Router::new(app),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            openapi: None,
        }
    }

    #[must_use]
    pub const fn with_max_body_bytes(mut self, max_body_bytes: usize) -> Self {
        self.max_body_bytes = max_body_bytes;
        self
    }

    /// Mounts precompiled `OpenAPI` JSON and UI assets in the in-memory adapter.
    #[must_use]
    pub fn with_openapi(mut self, config: OpenApiConfig) -> Self {
        self.openapi = Some(OpenApiService::new(self.app.definition(), config));
        self
    }

    pub async fn call(&self, request: Request) -> Response {
        dispatch(
            self.app,
            &self.router,
            self.max_body_bytes,
            self.openapi.as_ref(),
            &request,
            None,
        )
        .await
    }

    pub async fn call_controlled(&self, request: Request, control: InvocationControl) -> Response {
        dispatch(
            self.app,
            &self.router,
            self.max_body_bytes,
            self.openapi.as_ref(),
            &request,
            Some(control),
        )
        .await
    }
}

async fn dispatch<RequestView>(
    app: &ExecutableApp,
    router: &Router,
    max_body_bytes: usize,
    openapi: Option<&OpenApiService>,
    request: &RequestView,
    control: Option<InvocationControl>,
) -> Response
where
    RequestView: HttpRequestView + ?Sized,
{
    if validate_url_encoding(request.target()).is_err() {
        return error_response(
            400,
            "invalid_url_encoding",
            "request target contains invalid percent encoding",
            None,
        );
    }
    let path = request
        .target()
        .split_once('?')
        .map_or(request.target(), |(path, _)| path);
    if let Some(response) = openapi.and_then(|service| service.handle(request.method(), path)) {
        return openapi_response(response);
    }
    let recognized = match router.recognize(request.method(), path) {
        Ok(recognized) => recognized,
        Err(RouteError::MethodNotAllowed { allowed }) => {
            let allow = allowed
                .iter()
                .map(|method| method.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return error_response(405, "method_not_allowed", "HTTP method not allowed", None)
                .with_header("allow", allow);
        }
        Err(RouteError::NotFound) => {
            return error_response(404, "not_found", "HTTP route not found", None);
        }
    };
    if let Some(body_source) = recognized.body_source() {
        match validate_body(request, max_body_bytes, body_source) {
            Ok(()) => {}
            Err(rejection) => return rejection.into_response(),
        }
    }
    let Some(operation) = app.operation_at(recognized.operation_index()) else {
        return internal_error_response();
    };
    let request_parts = RoutedRequestParts {
        request,
        route: &recognized,
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

fn openapi_response(asset: OpenApiAssetResponse) -> Response {
    let mut response = Response {
        status: asset.status,
        headers: ResponseHeaders::empty(),
        body: asset.body,
        stream: None,
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

struct RoutedRequestParts<'request, 'router, 'path, RequestView: ?Sized> {
    request: &'request RequestView,
    route: &'request RouteMatch<'router, 'path>,
}

impl<RequestView> InvocationRequestParts for RoutedRequestParts<'_, '_, '_, RequestView>
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
            InputSource::Path | InputSource::Json | InputSource::Multipart | InputSource::File => {
                None
            }
        }
    }

    fn body(&self) -> &[u8] {
        self.request.body()
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
                InputSource::Json | InputSource::Form | InputSource::Multipart | InputSource::File
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

    let valid_media_type = request
        .header_value("content-type", 0)
        .is_some_and(|content_type| match source {
            InputSource::Json => is_json_media_type(content_type),
            InputSource::Form => media_type_is(content_type, "application/x-www-form-urlencoded"),
            InputSource::Multipart | InputSource::File => {
                media_type_is(content_type, "multipart/form-data")
            }
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
        } => {
            let response = match body {
                Some(body) => Response {
                    status,
                    headers: json_headers(),
                    body,
                    stream: None,
                },
                None => Response {
                    status,
                    headers: ResponseHeaders::empty(),
                    body: Vec::new(),
                    stream: None,
                },
            };
            with_outcome_headers(response, headers)
        }
        ExecutionOutcome::StreamingSuccess {
            status,
            headers,
            body,
        } => with_outcome_headers(
            Response {
                status,
                headers: ResponseHeaders::empty(),
                body: Vec::new(),
                stream: Some(body),
            },
            headers,
        ),
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
        };
    };

    Response {
        status,
        headers: json_headers(),
        body,
        stream: None,
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
    fn with_header(mut self, name: impl AsRef<str>, value: impl Into<String>) -> Self {
        self.headers.insert(
            Cow::Owned(normalize_header_name(name.as_ref())),
            Cow::Owned(value.into()),
        );
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
