#![forbid(unsafe_code)]

use blazingly_contract::{InvalidOperationId, OperationContract};
use core::fmt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::{Context, Poll};

pub use blazingly_contract::{
    AgentPolicy, ApiError, ApiModel, ApiSchema, CURRENT_CONTRACT_FORMAT_VERSION, Compatibility,
    CompatibilityChange, CompatibilityImpact, CompatibilityReport, Confirmation,
    ContractFingerprint, ContractFormatVersion, DependencyDescriptor, FieldDescriptor,
    FieldViolation, InputDescriptor, InputSource, McpToolDescriptor, ModelDescriptor,
    OperationFailure, OperationId, OperationRisk, OutputExposure, ResponseBuildError,
    ResponseDescriptor, ResponseHeader, SchemaKind, SecurityLocation, SecurityRequirement,
    SecuritySchemeDescriptor, SecuritySchemeKind, TypeDescriptor, ValidationErrors, ValidationRule,
};

/// A typed JSON request body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Json<T>(pub T);

/// A typed path argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Path<T>(pub T);

/// Typed URL query arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Query<T>(pub T);

/// A typed HTTP header argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header<T>(pub T);

/// A typed HTTP cookie argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cookie<T>(pub T);

/// A typed `application/x-www-form-urlencoded` request body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Form<T>(pub T);

/// A typed `multipart/form-data` request body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Multipart<T>(pub T);

/// A typed uploaded file argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct File<T>(pub T);

/// Runtime-neutral buffered upload metadata.
///
/// Native and Cloudflare adapters may obtain these bytes differently; neither
/// storage nor socket APIs leak into the operation contract.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UploadFile {
    pub field_name: String,
    pub file_name: Option<String>,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

impl UploadFile {
    #[must_use]
    pub fn new(field_name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            field_name: field_name.into(),
            file_name: None,
            content_type: None,
            bytes,
        }
    }

    #[must_use]
    pub fn with_file_name(mut self, file_name: impl Into<String>) -> Self {
        self.file_name = Some(file_name.into());
        self
    }

    #[must_use]
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }
}

impl ApiSchema for UploadFile {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::scalar("UploadFile", SchemaKind::Binary)
    }
}

/// A successful HTTP 201 response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Created<T>(pub T);

/// A successful HTTP 202 response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Accepted<T>(pub T);

/// A successful HTTP 204 response without a body.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NoContent;

/// One failure produced while an HTTP response body is being streamed.
///
/// The error is transport-neutral. Native servers can terminate the wire
/// stream, while in-memory adapters return it directly to tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BodyStreamError {
    pub code: String,
    pub message: String,
}

impl BodyStreamError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for BodyStreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BodyStreamError {}

/// Runtime-neutral, pull-based response byte stream.
///
/// A transport polls the next chunk only after it has capacity to write it.
/// That pull boundary is the framework's backpressure contract; producers do
/// not depend on Tokio, Compio, or a Cloudflare runtime.
pub trait BodyStream: 'static {
    fn poll_next(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Vec<u8>, BodyStreamError>>>;
}

/// Typed streaming HTTP response body.
///
/// `exact_length` is optional. HTTP/1 uses chunked transfer coding when it is
/// absent, while HTTP/2 emits DATA frames without a content-length field.
pub struct StreamingBody {
    stream: Pin<Box<dyn BodyStream>>,
    exact_length: Option<u64>,
}

impl StreamingBody {
    #[must_use]
    pub fn new(stream: impl BodyStream) -> Self {
        Self {
            stream: Box::pin(stream),
            exact_length: None,
        }
    }

    /// Builds a pull stream from already available chunks.
    #[must_use]
    pub fn from_chunks<I, Chunk>(chunks: I) -> Self
    where
        I: IntoIterator<Item = Chunk>,
        I::IntoIter: Unpin + 'static,
        Chunk: Into<Vec<u8>> + 'static,
    {
        Self::new(ChunkIterator {
            chunks: chunks.into_iter(),
        })
    }

    /// Builds a one-chunk stream with a known exact length.
    #[must_use]
    pub fn once(bytes: impl Into<Vec<u8>>) -> Self {
        let bytes = bytes.into();
        let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        Self::from_chunks([bytes]).with_exact_length(length)
    }

    #[must_use]
    pub const fn with_exact_length(mut self, length: u64) -> Self {
        self.exact_length = Some(length);
        self
    }

    #[must_use]
    pub const fn exact_length(&self) -> Option<u64> {
        self.exact_length
    }

    /// Waits until the producer yields one chunk.
    ///
    /// Calling this method is the consumer demand signal. Transports should
    /// not request another chunk until the previous one has been written.
    pub async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, BodyStreamError>> {
        poll_fn(|context| self.stream.as_mut().poll_next(context)).await
    }
}

impl fmt::Debug for StreamingBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamingBody")
            .field("exact_length", &self.exact_length)
            .finish_non_exhaustive()
    }
}

impl ApiSchema for StreamingBody {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::scalar("StreamingBody", SchemaKind::Binary)
    }
}

/// A transport error after an HTTP connection has switched protocols.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeIoError {
    pub code: String,
    pub message: String,
}

impl UpgradeIoError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for UpgradeIoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for UpgradeIoError {}

/// Runtime-neutral byte I/O owned after an HTTP protocol upgrade.
///
/// Native and edge adapters implement this trait without exposing their socket
/// or runtime types to handlers.
pub type UpgradeReadFuture<'io> =
    Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, UpgradeIoError>> + 'io>>;
pub type UpgradeWriteFuture<'io> = Pin<Box<dyn Future<Output = Result<(), UpgradeIoError>> + 'io>>;

pub trait UpgradedIo: 'static {
    fn read(&mut self) -> UpgradeReadFuture<'_>;

    fn write(&mut self, bytes: Vec<u8>) -> UpgradeWriteFuture<'_>;

    fn shutdown(&mut self) -> UpgradeWriteFuture<'_>;
}

pub type UpgradeFuture = Pin<Box<dyn Future<Output = Result<(), UpgradeIoError>> + 'static>>;
pub type UpgradeHandler = Box<dyn FnOnce(Box<dyn UpgradedIo>) -> UpgradeFuture + 'static>;

/// A validated HTTP protocol switch plus its post-handshake session handler.
pub struct HttpUpgrade {
    protocol: &'static str,
    headers: Vec<ResponseHeader>,
    handler: Option<UpgradeHandler>,
}

impl HttpUpgrade {
    #[must_use]
    pub fn new(
        protocol: &'static str,
        headers: Vec<ResponseHeader>,
        handler: impl FnOnce(Box<dyn UpgradedIo>) -> UpgradeFuture + 'static,
    ) -> Self {
        Self {
            protocol,
            headers,
            handler: Some(Box::new(handler)),
        }
    }

    #[must_use]
    pub const fn protocol(&self) -> &'static str {
        self.protocol
    }

    #[must_use]
    pub fn headers(&self) -> &[ResponseHeader] {
        &self.headers
    }

    pub fn extend_headers(&mut self, headers: impl IntoIterator<Item = ResponseHeader>) {
        self.headers.extend(headers);
    }

    /// Runs the one-shot upgraded protocol session.
    ///
    /// # Errors
    ///
    /// Returns the adapter or upgraded protocol error produced by the session.
    pub async fn run(mut self, io: Box<dyn UpgradedIo>) -> Result<(), UpgradeIoError> {
        let handler = self.handler.take().ok_or_else(|| {
            UpgradeIoError::new(
                "upgrade_already_consumed",
                "the protocol upgrade handler has already been consumed",
            )
        })?;
        handler(io).await
    }
}

impl fmt::Debug for HttpUpgrade {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpUpgrade")
            .field("protocol", &self.protocol)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

impl ApiSchema for HttpUpgrade {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::scalar("HttpUpgrade", SchemaKind::Binary)
    }
}

/// A failure produced by work scheduled after an HTTP response is sent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackgroundTaskError {
    pub code: String,
    pub message: String,
}

impl BackgroundTaskError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for BackgroundTaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BackgroundTaskError {}

pub type BackgroundFuture =
    Pin<Box<dyn Future<Output = Result<(), BackgroundTaskError>> + 'static>>;

/// One runtime-neutral task that begins after the response body is written.
pub struct BackgroundTask {
    task: Option<Box<dyn FnOnce() -> BackgroundFuture + 'static>>,
}

impl BackgroundTask {
    #[must_use]
    pub fn new<Task, TaskFuture>(task: Task) -> Self
    where
        Task: FnOnce() -> TaskFuture + 'static,
        TaskFuture: Future<Output = Result<(), BackgroundTaskError>> + 'static,
    {
        Self {
            task: Some(Box::new(move || Box::pin(task()))),
        }
    }

    #[must_use]
    pub fn infallible<Task, TaskFuture>(task: Task) -> Self
    where
        Task: FnOnce() -> TaskFuture + 'static,
        TaskFuture: Future<Output = ()> + 'static,
    {
        Self::new(move || async move {
            task().await;
            Ok(())
        })
    }

    /// Runs this task exactly once.
    ///
    /// # Errors
    ///
    /// Returns the task failure or an already-consumed error.
    pub async fn run(mut self) -> Result<(), BackgroundTaskError> {
        let task = self.task.take().ok_or_else(|| {
            BackgroundTaskError::new(
                "background_task_consumed",
                "background task has already been consumed",
            )
        })?;
        task().await
    }
}

impl fmt::Debug for BackgroundTask {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BackgroundTask")
            .finish_non_exhaustive()
    }
}

/// A typed response carrying work that starts after its wire body is sent.
#[derive(Debug)]
pub struct Background<T> {
    response: T,
    tasks: Vec<BackgroundTask>,
}

impl<T> Background<T> {
    #[must_use]
    pub fn new(response: T) -> Self {
        Self {
            response,
            tasks: Vec::new(),
        }
    }

    #[must_use]
    pub fn task(mut self, task: BackgroundTask) -> Self {
        self.tasks.push(task);
        self
    }

    #[must_use]
    pub fn into_parts(self) -> (T, Vec<BackgroundTask>) {
        (self.response, self.tasks)
    }
}

impl<T: ApiSchema> ApiSchema for Background<T> {
    fn type_descriptor() -> TypeDescriptor {
        T::type_descriptor()
    }
}

/// Ergonomic after-response task decoration.
pub trait BackgroundExt: Sized {
    #[must_use]
    fn background(self, task: BackgroundTask) -> Background<Self> {
        Background::new(self).task(task)
    }
}

impl<T> BackgroundExt for T {}

/// Prefixes nested model violations while preserving stable codes/messages.
pub fn merge_validation_errors(
    target: &mut ValidationErrors,
    prefix: &str,
    nested: &ValidationErrors,
) {
    for violation in nested.violations() {
        let field = if violation.field.is_empty() {
            prefix.to_owned()
        } else {
            format!("{prefix}.{}", violation.field)
        };
        target.push(field, violation.code.clone(), violation.message.clone());
    }
}

struct ChunkIterator<I> {
    chunks: I,
}

impl<I, Chunk> BodyStream for ChunkIterator<I>
where
    I: Iterator<Item = Chunk> + Unpin + 'static,
    Chunk: Into<Vec<u8>> + 'static,
{
    fn poll_next(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Vec<u8>, BodyStreamError>>> {
        Poll::Ready(self.get_mut().chunks.next().map(|chunk| Ok(chunk.into())))
    }
}

/// Overrides the successful status of another typed response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Status<const STATUS: u16, T>(pub T);

/// Adds response headers without changing the typed response body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WithHeaders<T> {
    response: T,
    headers: Vec<ResponseHeader>,
}

impl<T> WithHeaders<T> {
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(ResponseHeader::new(name, value));
        self
    }

    #[must_use]
    pub fn into_parts(self) -> (T, Vec<ResponseHeader>) {
        (self.response, self.headers)
    }
}

/// Ergonomic response decoration shared by typed success responses.
pub trait ResponseExt: Sized {
    #[must_use]
    fn header(self, name: impl Into<String>, value: impl Into<String>) -> WithHeaders<Self> {
        WithHeaders {
            response: self,
            headers: vec![ResponseHeader::new(name, value)],
        }
    }
}

impl<T> ResponseExt for T {}

/// HTTP methods supported by the operation frontend.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Options,
    Trace,
    Connect,
}

impl HttpMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Options => "OPTIONS",
            Self::Trace => "TRACE",
            Self::Connect => "CONNECT",
        }
    }

    #[must_use]
    pub const fn as_openapi_key(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Head => "head",
            Self::Post => "post",
            Self::Put => "put",
            Self::Patch => "patch",
            Self::Delete => "delete",
            Self::Options => "options",
            Self::Trace => "trace",
            // CONNECT is not a standard OpenAPI Path Item field.
            Self::Connect => "x-blazingly-connect",
        }
    }
}

/// The HTTP projection of a protocol-neutral operation contract.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HttpBinding {
    pub method: HttpMethod,
    pub path: String,
}

impl HttpBinding {
    #[must_use]
    pub fn new(method: HttpMethod, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
        }
    }
}

/// A protocol-neutral operation paired with its HTTP projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationDescriptor {
    pub contract: OperationContract,
    pub http: HttpBinding,
}

impl OperationDescriptor {
    /// Creates an HTTP projection of a protocol-neutral operation.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidOperationId`] when `id` is not a valid stable
    /// operation identity.
    pub fn new(
        method: HttpMethod,
        path: impl Into<String>,
        id: impl Into<String>,
        summary: impl Into<String>,
        input: Option<TypeDescriptor>,
        responses: Vec<ResponseDescriptor>,
    ) -> Result<Self, InvalidOperationId> {
        Ok(Self {
            contract: OperationContract::new(id, summary, input, responses)?,
            http: HttpBinding::new(method, path),
        })
    }

    #[must_use]
    pub fn with_mcp_tool(mut self, tool: McpToolDescriptor, policy: AgentPolicy) -> Self {
        self.contract = self.contract.with_agent_policy(policy).with_mcp_tool(tool);
        self
    }

    #[must_use]
    pub fn mcp_tool(&self) -> Option<&McpToolDescriptor> {
        self.contract.mcp.as_ref()
    }

    #[must_use]
    pub fn with_inputs(mut self, inputs: Vec<InputDescriptor>) -> Self {
        self.contract = self.contract.with_inputs(inputs);
        self
    }

    #[must_use]
    pub fn with_dependencies(mut self, dependencies: Vec<DependencyDescriptor>) -> Self {
        self.contract = self.contract.with_dependencies(dependencies);
        self
    }

    #[must_use]
    pub fn with_security(mut self, requirements: Vec<SecurityRequirement>) -> Self {
        self.contract = self.contract.with_security(requirements);
        self
    }
}

/// A validated, deterministic application description.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppDefinition {
    operations: Vec<OperationDescriptor>,
    security_schemes: Vec<SecuritySchemeDescriptor>,
}

impl AppDefinition {
    #[must_use]
    pub fn operations(&self) -> &[OperationDescriptor] {
        &self.operations
    }

    #[must_use]
    pub fn security_schemes(&self) -> &[SecuritySchemeDescriptor] {
        &self.security_schemes
    }
}

/// Builder for an application description.
#[derive(Clone, Debug, Default)]
pub struct App {
    operations: Vec<OperationDescriptor>,
    security_schemes: Vec<SecuritySchemeDescriptor>,
}

impl App {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            operations: Vec::new(),
            security_schemes: Vec::new(),
        }
    }

    #[must_use]
    pub fn route(mut self, operation: OperationDescriptor) -> Self {
        self.operations.push(operation);
        self
    }

    #[must_use]
    pub fn routes(mut self, operations: impl IntoIterator<Item = OperationDescriptor>) -> Self {
        self.operations.extend(operations);
        self
    }

    /// Registers a named security scheme referenced by operation contracts.
    #[must_use]
    pub fn security_scheme(mut self, scheme: SecuritySchemeDescriptor) -> Self {
        self.security_schemes.push(scheme);
        self
    }

    /// Validates and deterministically orders the application graph.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] when an operation identity or HTTP binding is
    /// registered more than once.
    pub fn build(mut self) -> Result<AppDefinition, BuildError> {
        let mut operation_ids = BTreeSet::new();
        let mut http_bindings = BTreeSet::new();
        let mut route_shapes = BTreeSet::new();
        let mut security_names = BTreeSet::new();

        for scheme in &self.security_schemes {
            if !security_names.insert(scheme.name.clone()) {
                return Err(BuildError::DuplicateSecurityScheme(scheme.name.clone()));
            }
        }

        for operation in &self.operations {
            validate_operation_inputs(operation)?;
            validate_operation_security(operation, &self.security_schemes)?;
            if !operation_ids.insert(operation.contract.id.clone()) {
                return Err(BuildError::DuplicateOperationId(
                    operation.contract.id.clone(),
                ));
            }

            let binding = (operation.http.method, operation.http.path.clone());
            if !http_bindings.insert(binding) {
                return Err(BuildError::DuplicateHttpBinding {
                    method: operation.http.method,
                    path: operation.http.path.clone(),
                });
            }
            if !route_shapes.insert((
                operation.http.method,
                canonical_route_shape(&operation.http.path),
            )) {
                return Err(BuildError::AmbiguousHttpBinding {
                    method: operation.http.method,
                    path: operation.http.path.clone(),
                });
            }
        }

        self.operations.sort_by(|left, right| {
            left.http
                .path
                .cmp(&right.http.path)
                .then(left.http.method.cmp(&right.http.method))
                .then(left.contract.id.cmp(&right.contract.id))
        });
        self.security_schemes
            .sort_by(|left, right| left.name.cmp(&right.name));

        Ok(AppDefinition {
            operations: self.operations,
            security_schemes: self.security_schemes,
        })
    }
}

/// An invalid application graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuildError {
    DuplicateOperationId(OperationId),
    DuplicateHttpBinding {
        method: HttpMethod,
        path: String,
    },
    AmbiguousHttpBinding {
        method: HttpMethod,
        path: String,
    },
    InvalidPathInputs {
        operation: OperationId,
    },
    DuplicateInputName {
        operation: OperationId,
        name: String,
    },
    DuplicateSecurityScheme(String),
    DuplicateSecurityRequirement {
        operation: OperationId,
        scheme: String,
    },
    UnknownSecurityScheme {
        operation: OperationId,
        scheme: String,
    },
    UnknownSecurityScope {
        operation: OperationId,
        scheme: String,
        scope: String,
    },
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateOperationId(id) => {
                write!(
                    formatter,
                    "operation id {id:?} is registered more than once"
                )
            }
            Self::DuplicateHttpBinding { method, path } => write!(
                formatter,
                "{} {path} is registered more than once",
                method.as_str()
            ),
            Self::AmbiguousHttpBinding { method, path } => write!(
                formatter,
                "{} {path} conflicts with another parameterized route",
                method.as_str()
            ),
            Self::InvalidPathInputs { operation } => write!(
                formatter,
                "operation {operation} path placeholders do not match its Path<T> inputs"
            ),
            Self::DuplicateInputName { operation, name } => write!(
                formatter,
                "operation {operation} exposes input name {name:?} more than once"
            ),
            Self::DuplicateSecurityScheme(name) => {
                write!(
                    formatter,
                    "security scheme {name:?} is registered more than once"
                )
            }
            Self::DuplicateSecurityRequirement { operation, scheme } => write!(
                formatter,
                "operation {operation} requires security scheme {scheme:?} more than once"
            ),
            Self::UnknownSecurityScheme { operation, scheme } => write!(
                formatter,
                "operation {operation} references unknown security scheme {scheme:?}"
            ),
            Self::UnknownSecurityScope {
                operation,
                scheme,
                scope,
            } => write!(
                formatter,
                "operation {operation} requires unknown scope {scope:?} from security scheme {scheme:?}"
            ),
        }
    }
}

impl std::error::Error for BuildError {}

fn canonical_route_shape(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if segment.starts_with('{') && segment.ends_with('}') {
                "{}"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn validate_operation_inputs(operation: &OperationDescriptor) -> Result<(), BuildError> {
    let placeholders = operation
        .http
        .path
        .split('/')
        .filter_map(|segment| {
            segment
                .strip_prefix('{')
                .and_then(|segment| segment.strip_suffix('}'))
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>();
    let path_inputs = operation
        .contract
        .inputs
        .iter()
        .filter(|input| input.source == InputSource::Path)
        .flat_map(input_public_names)
        .collect::<BTreeSet<_>>();
    if placeholders != path_inputs {
        return Err(BuildError::InvalidPathInputs {
            operation: operation.contract.id.clone(),
        });
    }

    let mut names = BTreeSet::new();
    for name in operation
        .contract
        .inputs
        .iter()
        .flat_map(input_public_names)
    {
        if !names.insert(name.clone()) {
            return Err(BuildError::DuplicateInputName {
                operation: operation.contract.id.clone(),
                name,
            });
        }
    }
    Ok(())
}

fn validate_operation_security(
    operation: &OperationDescriptor,
    schemes: &[SecuritySchemeDescriptor],
) -> Result<(), BuildError> {
    let mut required_schemes = BTreeSet::new();
    for requirement in &operation.contract.security {
        if !required_schemes.insert(requirement.scheme.as_str()) {
            return Err(BuildError::DuplicateSecurityRequirement {
                operation: operation.contract.id.clone(),
                scheme: requirement.scheme.clone(),
            });
        }
        let Some(scheme) = schemes
            .iter()
            .find(|scheme| scheme.name == requirement.scheme)
        else {
            return Err(BuildError::UnknownSecurityScheme {
                operation: operation.contract.id.clone(),
                scheme: requirement.scheme.clone(),
            });
        };
        let declared_scopes = match &scheme.kind {
            SecuritySchemeKind::OAuth2 { scopes, .. } => Some(scopes),
            SecuritySchemeKind::ApiKey { .. }
            | SecuritySchemeKind::Http { .. }
            | SecuritySchemeKind::OpenIdConnect { .. }
            | SecuritySchemeKind::MutualTls => None,
        };
        for scope in &requirement.scopes {
            if declared_scopes.is_none_or(|scopes| !scopes.contains(scope)) {
                return Err(BuildError::UnknownSecurityScope {
                    operation: operation.contract.id.clone(),
                    scheme: requirement.scheme.clone(),
                    scope: scope.clone(),
                });
            }
        }
    }
    Ok(())
}

fn input_public_names(input: &InputDescriptor) -> Vec<String> {
    input.ty.model.as_ref().map_or_else(
        || vec![input.name.clone()],
        |model| {
            model
                .fields
                .iter()
                .map(|field| field.name.clone())
                .collect()
        },
    )
}

#[macro_export]
macro_rules! descriptors {
    ($($operation:ident),* $(,)?) => {
        ::std::vec![$($operation::descriptor()),*]
    };
}

#[cfg(test)]
mod tests {
    use super::{
        App, BuildError, HttpMethod, InputDescriptor, InputSource, OperationDescriptor,
        ResponseDescriptor, SecurityRequirement, SecuritySchemeDescriptor, SecuritySchemeKind,
        TypeDescriptor,
    };

    fn operation(id: &str, method: HttpMethod, path: &str) -> OperationDescriptor {
        OperationDescriptor::new(
            method,
            path,
            id,
            id,
            None,
            vec![ResponseDescriptor::success(
                200,
                Some(TypeDescriptor::new("Output")),
            )],
        )
        .expect("test operation id should be valid")
    }

    #[test]
    fn app_rejects_duplicate_operation_ids() {
        let result = App::new()
            .route(operation("users.read", HttpMethod::Get, "/users/1"))
            .route(operation("users.read", HttpMethod::Get, "/users/2"))
            .build();

        assert!(matches!(result, Err(BuildError::DuplicateOperationId(_))));
    }

    #[test]
    fn app_rejects_duplicate_http_bindings() {
        let result = App::new()
            .route(operation("users.read", HttpMethod::Get, "/users"))
            .route(operation("users.list", HttpMethod::Get, "/users"))
            .build();

        assert!(matches!(
            result,
            Err(BuildError::DuplicateHttpBinding { .. })
        ));
    }

    #[test]
    fn app_rejects_parameter_routes_with_the_same_shape() {
        let result = App::new()
            .route(
                operation("users.by_id", HttpMethod::Get, "/users/{user_id}").with_inputs(vec![
                    InputDescriptor::new(
                        "user_id",
                        InputSource::Path,
                        true,
                        TypeDescriptor::new("u64"),
                    ),
                ]),
            )
            .route(
                operation("users.by_name", HttpMethod::Get, "/users/{user_name}").with_inputs(
                    vec![InputDescriptor::new(
                        "user_name",
                        InputSource::Path,
                        true,
                        TypeDescriptor::new("String"),
                    )],
                ),
            )
            .build();

        assert!(matches!(
            result,
            Err(BuildError::AmbiguousHttpBinding { .. })
        ));
    }

    #[test]
    fn app_order_is_deterministic() {
        let app = App::new()
            .route(operation("users.create", HttpMethod::Post, "/users"))
            .route(operation("health.read", HttpMethod::Get, "/health"))
            .build()
            .expect("application should be valid");

        let ids: Vec<_> = app
            .operations()
            .iter()
            .map(|operation| operation.contract.id.as_str())
            .collect();
        assert_eq!(ids, ["health.read", "users.create"]);
    }

    #[test]
    fn app_validates_and_orders_operation_security() {
        let secured = operation("users.write", HttpMethod::Put, "/users").with_security(vec![
            SecurityRequirement::new("oauth").with_scopes(vec!["users:write".to_owned()]),
        ]);
        let app = App::new()
            .route(secured)
            .security_scheme(SecuritySchemeDescriptor::new(
                "oauth",
                SecuritySchemeKind::OAuth2 {
                    authorization_url: Some("https://auth.example/authorize".to_owned()),
                    token_url: Some("https://auth.example/token".to_owned()),
                    scopes: vec!["users:read".to_owned(), "users:write".to_owned()],
                },
            ))
            .build()
            .expect("registered security requirements should compile");

        assert_eq!(app.security_schemes()[0].name, "oauth");
        assert_eq!(
            app.operations()[0].contract.security[0].scopes,
            ["users:write"]
        );
    }

    #[test]
    fn app_rejects_unknown_security_schemes_and_scopes() {
        let unknown_scheme = App::new()
            .route(
                operation("users.read", HttpMethod::Get, "/users")
                    .with_security(vec![SecurityRequirement::new("missing")]),
            )
            .build();
        assert!(matches!(
            unknown_scheme,
            Err(BuildError::UnknownSecurityScheme { .. })
        ));

        let unknown_scope = App::new()
            .route(
                operation("users.read", HttpMethod::Get, "/users").with_security(vec![
                    SecurityRequirement::new("oauth").with_scopes(vec!["users:write".to_owned()]),
                ]),
            )
            .security_scheme(SecuritySchemeDescriptor::new(
                "oauth",
                SecuritySchemeKind::OAuth2 {
                    authorization_url: None,
                    token_url: Some("https://auth.example/token".to_owned()),
                    scopes: vec!["users:read".to_owned()],
                },
            ))
            .build();
        assert!(matches!(
            unknown_scope,
            Err(BuildError::UnknownSecurityScope { .. })
        ));
    }
}
