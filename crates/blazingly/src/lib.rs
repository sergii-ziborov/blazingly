#![forbid(unsafe_code)]

pub use blazingly_contract::*;
pub use blazingly_core::*;
pub use blazingly_di::*;
#[cfg(feature = "docs")]
pub use blazingly_docs as docs;
pub use blazingly_executor::{
    CancellationToken, ExecutableApp, ExecutableBuildError, ExecutableOperation, ExecutionOutcome,
    FromInvocation, HookContext, HookOutcome, HookOutcomeKind, HttpRequestParts, InputRejection,
    InvocationAbort, InvocationControl, InvocationInput, OperationFuture, OperationOutput, Plugin,
    ResolvedDependencies, TestOverrides, dependency_error_outcome, routes,
};
pub use blazingly_http as http;
pub use blazingly_http::{
    CollectBodyError, HttpApp, HttpRequestView, Request, Response, RouteError, RouteMatch, Router,
    TestApp,
};
pub use blazingly_macros::{
    api_error, api_model, connect, delete, get, head, operation, options, patch, post, provider,
    put, security, trace,
};
#[cfg(feature = "openapi")]
pub use blazingly_openapi as openapi;

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
    pub use serde;
    pub use serde_json;
}

pub mod prelude {
    #[cfg(feature = "mcp")]
    pub use crate::mcp;
    pub use crate::{
        Accepted, ApiModel, App, BodyStream, BodyStreamError, CancellationToken, CollectBodyError,
        Cookie, Created, DependencyError, Depends, ExecutableApp, File, Form, Header, HookContext,
        HookOutcome, HookOutcomeKind, HttpApp, InvocationAbort, InvocationControl, Json, Multipart,
        NoContent, OperationDescriptor, OperationFailure, Path, Plugin, Provider, Query, Request,
        Response, ResponseDescriptor, ResponseExt, Router, Status, StreamingBody, TestApp,
        TestOverrides, TypeDescriptor, UploadFile, WithHeaders, api_error, api_model, connect,
        delete, get, head, operation, options, patch, post, provider, put, routes, security, trace,
    };
}
