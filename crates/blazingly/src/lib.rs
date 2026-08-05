#![forbid(unsafe_code)]
// The crate README is the docs.rs landing page, so the two cannot drift and
// the example in it is compiled as a doctest rather than trusted.
#![doc = include_str!("../README.md")]

// The repository README is the first Rust anyone reads, and until this was
// here nothing compiled it: its main example had not built for some time.
// `#[cfg(doctest)]` strips the item before `include_str!` expands, so the path
// out of the package root is never followed by `cargo package` or by docs.rs,
// both of which are plain builds.
#[cfg(doctest)]
#[doc = include_str!("../../../README.md")]
struct RepositoryReadme;

pub use blazingly_contract::*;
pub use blazingly_core::*;
#[cfg(feature = "database")]
pub use blazingly_database as database;
#[cfg(feature = "deploy")]
pub use blazingly_deploy as deploy;
pub use blazingly_di::*;
#[cfg(feature = "docs")]
pub use blazingly_docs as docs;
pub use blazingly_executor::{
    BlockingError, BlockingFuture, BlockingPool, BlockingPoolConfig, CancellationToken,
    ExecutableApp, ExecutableBuildError, ExecutableOperation, ExecutionOutcome, Extension, Extract,
    FromInvocation, HookContext, HookOutcome, HookOutcomeKind, HttpRequestParts, InputRejection,
    InvocationAbort, InvocationControl, InvocationInput, OperationFuture, OperationOutput, Plugin,
    ProviderInput, ProviderInputDecoder, RequestParts, RequestProvider, ResolvedDependencies,
    TestOverrides, UploadBody, blocking_error_outcome, dependency_error_outcome,
    install_global_blocking_pool, routes, run_blocking,
};
pub use blazingly_http as http;
pub use blazingly_http::{
    CollectBodyError, ConnectionInfo, HttpApp, HttpMiddleware, HttpRequestContext, HttpRequestView,
    MiddlewareScope, Request, Response, RouteError, RouteMatch, Router, TestApp,
    UnverifiedSecurity, UnverifiedSecurityError,
};
// Public, not `__private`: `AuthenticatedIdentity::claims`, `VerifiedToken::claims`
// and `PreparedJson::encode` name this crate's `Value` and `Error` in their
// signatures, so an application has to be able to name them too. While
// `serde_json` was the engine an application could reach it from crates.io on
// its own; `blazingly-json` is not published yet, so the facade re-exports it.
pub use blazingly_json as json;
pub use blazingly_macros::{
    api_error, api_model, connect, delete, get, head, operation, options, patch, post, provider,
    put, security, trace,
};
#[cfg(feature = "middleware")]
pub use blazingly_middleware as middleware;
#[cfg(feature = "observability")]
pub use blazingly_observability as observability;
#[cfg(feature = "openapi")]
pub use blazingly_openapi as openapi;
#[cfg(feature = "queue")]
pub use blazingly_queue as queue;
#[cfg(feature = "realtime")]
pub use blazingly_realtime as realtime;
#[cfg(feature = "security")]
pub use blazingly_security as security_runtime;
#[cfg(feature = "templates")]
pub use blazingly_templates as templates;
#[cfg(feature = "validation")]
pub use blazingly_validation as validation;

#[cfg(feature = "mcp")]
pub mod mcp {
    pub use blazingly_macros::tool;
    pub use blazingly_mcp::*;
    #[cfg(feature = "mcp-stdio")]
    pub use blazingly_mcp_stdio as stdio;
}

#[cfg(feature = "native")]
pub mod native {
    pub use blazingly_native::*;
}

#[doc(hidden)]
pub mod __private {
    pub use blazingly_json;
    pub use serde;
}

pub mod prelude {
    // `ErrorKind` is aliased so a glob import of the prelude does not collide
    // with `std::io::ErrorKind`; the plain name stays at `blazingly::database`.
    #[cfg(feature = "database")]
    pub use crate::database::{
        AppliedMigration, ConnectionPool, Database, DatabaseError, ErrorKind as DatabaseErrorKind,
        IsolationLevel, Migration, MigrationError, MigrationReport, MigrationRunner, MigrationSet,
        PoolHealth, RollbackFailure, TransactionOptions, TransactionPanic, Transactional,
    };
    #[cfg(feature = "mcp")]
    pub use crate::mcp;
    #[cfg(feature = "middleware")]
    pub use crate::middleware::{
        Compression, ContentEncoding, Cors, IpNetwork, MemoryRateLimitStore, ProxyHeaders,
        RateLimit, RateLimitDecision, RateLimitQuota, RateLimitStore, StaticFiles,
        StaticFilesError, TrustedHost,
    };
    #[cfg(feature = "observability")]
    pub use crate::observability::{
        AccessEvent, AccessLogSink, DEFAULT_DURATION_BUCKETS_SECONDS, MAX_LABEL_SETS_PER_METRIC,
        MetricError, Metrics, Observability, ObservabilityConfig, RequestId, TraceContext,
        TracingAccessLog,
    };
    #[cfg(feature = "queue")]
    pub use crate::queue::{
        DeadLetter, Delivery, JobError, MemoryQueue, Message, Queue, QueueClient, QueueError,
        RetryPolicy, Worker, WorkerStep,
    };
    #[cfg(feature = "realtime")]
    pub use crate::realtime::{
        Sse, SseEvent, WebSocket, WebSocketClose, WebSocketError, WebSocketMessage,
        WebSocketRequest, WebSocketUpgrade,
    };
    #[cfg(feature = "security")]
    pub use crate::security_runtime::{
        ApiKey, AuthMode, AuthenticatedIdentity, AuthenticationError, BasicAuth, BearerToken,
        CookieOptions, CredentialVerifier, JwtClaims, JwtHs256, JwtValidation, MemorySessionStore,
        OAuth2Bearer, SameSite, Security, SecurityConfigError, SecurityContext, Session,
        SessionLayer, SessionRecord, SessionStatus, SessionStore, SignedSession, StaticPasswords,
        TokenVerifier, VerifiedToken,
    };
    #[cfg(feature = "templates")]
    pub use crate::templates::{
        EscapeMode, Html, RenderError, TemplateError, TemplateErrorKind, Templates,
        TemplatesBuilder,
    };
    #[cfg(feature = "validation")]
    pub use crate::validation::{Date, DateTime, Decimal, IpAddress, Url, Uuid};
    pub use crate::{
        Accepted, ApiModel, App, Background, BackgroundExt, BackgroundTask, BackgroundTaskError,
        BlockingError, BlockingPoolConfig, BodyStream, BodyStreamError, CancellationToken,
        CollectBodyError, Cookie, Created, DependencyError, Depends, ExecutableApp, Extension,
        Extract, File, Form, Header, HookContext, HookOutcome, HookOutcomeKind, HttpApp,
        HttpMiddleware, HttpRequestContext, InvocationAbort, InvocationControl, Json, Multipart,
        NoContent, OperationDescriptor, OperationFailure, Path, Plugin, PreparedJson, Provider,
        Query, Request, RequestParts, Response, ResponseDescriptor, ResponseExt, Router, Status,
        StreamingBody, TestApp, TestOverrides, TypeDescriptor, UploadBody, UploadFile, WithHeaders,
        api_error, api_model, connect, delete, get, head, install_global_blocking_pool, operation,
        options, patch, post, provider, put, routes, security, trace,
    };
}
