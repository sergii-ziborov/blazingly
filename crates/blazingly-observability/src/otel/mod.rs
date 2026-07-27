//! OTLP/HTTP span export that needs no async runtime.
//!
//! The pipeline is deliberately assembled out of runtime-neutral parts. The SDK
//! [`BatchSpanProcessor`](opentelemetry_sdk::trace::BatchSpanProcessor) owns a
//! dedicated OS thread and drives each export future to completion with
//! `futures_executor::block_on`, and [`BlockingHttpClient`] performs plain
//! `std::net` I/O on that thread. No Tokio, and no reactor, appears anywhere in
//! the dependency graph.
//!
//! ```no_run
//! use blazingly_observability::otel::{OtlpConfig, install};
//!
//! let config = OtlpConfig::default()
//!     .with_service_name("checkout-api")
//!     .with_endpoint("http://collector.internal:4318/v1/traces");
//! let pipeline = install(&config).expect("telemetry pipeline installs");
//!
//! // ... serve requests ...
//!
//! // Flush the last batch before the process exits. Dropping the pipeline does
//! // the same thing, but only `shutdown` reports a failure.
//! pipeline.shutdown().expect("telemetry pipeline drains");
//! ```

mod http_client;

pub use http_client::{BlockingHttpClient, TransportError};

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_http::HttpClient;
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkError;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;
use tracing::Subscriber;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;

/// OTLP/HTTP traces endpoint of a collector running on the local host.
pub const DEFAULT_OTLP_ENDPOINT: &str = "http://localhost:4318/v1/traces";

/// Timeout applied to a single export request.
pub const DEFAULT_EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// Instrumentation scope recorded on exported spans.
pub const DEFAULT_TRACER_NAME: &str = "blazingly";

/// Failure while building, installing, or shutting down an export pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TelemetryError {
    /// The OTLP exporter could not be built from this configuration.
    Exporter(String),
    /// A global `tracing` subscriber was already installed by someone else.
    SubscriberAlreadySet,
    /// The tracer provider failed to flush or shut down.
    Shutdown(String),
}

impl std::fmt::Display for TelemetryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exporter(reason) => write!(formatter, "OTLP exporter build failed: {reason}"),
            Self::SubscriberAlreadySet => {
                formatter.write_str("a global tracing subscriber is already installed")
            }
            Self::Shutdown(reason) => write!(formatter, "telemetry shutdown failed: {reason}"),
        }
    }
}

impl std::error::Error for TelemetryError {}

/// Configuration for the OTLP/HTTP span export pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OtlpConfig {
    /// Full traces URL, including the `/v1/traces` path.
    pub endpoint: String,
    /// Value reported as the `service.name` resource attribute.
    pub service_name: String,
    /// Timeout applied to a single export request.
    pub timeout: Duration,
    /// Extra headers merged into every export request.
    pub headers: BTreeMap<String, String>,
    /// Instrumentation scope name recorded on exported spans.
    pub tracer_name: String,
}

impl OtlpConfig {
    /// Sets the full traces URL, including the `/v1/traces` path.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Sets the `service.name` resource attribute.
    #[must_use]
    pub fn with_service_name(mut self, service_name: impl Into<String>) -> Self {
        self.service_name = service_name.into();
        self
    }

    /// Sets the per-request export timeout.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Adds one header to every export request, replacing a previous value.
    ///
    /// Use this for collector credentials such as `authorization`.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Sets the instrumentation scope name recorded on exported spans.
    #[must_use]
    pub fn with_tracer_name(mut self, tracer_name: impl Into<String>) -> Self {
        self.tracer_name = tracer_name.into();
        self
    }
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_OTLP_ENDPOINT.to_owned(),
            service_name: "blazingly".to_owned(),
            timeout: DEFAULT_EXPORT_TIMEOUT,
            headers: BTreeMap::new(),
            tracer_name: DEFAULT_TRACER_NAME.to_owned(),
        }
    }
}

/// A built export pipeline that drains its tracer provider on drop.
///
/// Prefer [`TelemetryPipeline::shutdown`] at the end of the application
/// lifespan: it reports a failed drain, whereas dropping swallows it.
#[derive(Debug)]
pub struct TelemetryPipeline {
    provider: SdkTracerProvider,
    tracer_name: String,
    shut_down: bool,
}

impl TelemetryPipeline {
    /// Tracer provider backing this pipeline.
    #[must_use]
    pub const fn provider(&self) -> &SdkTracerProvider {
        &self.provider
    }

    /// Builds a `tracing-opentelemetry` layer feeding this pipeline.
    ///
    /// Use this when the application composes its own subscriber; [`install`]
    /// wires an equivalent layer into a plain registry for you.
    #[must_use]
    pub fn layer<S>(&self) -> OpenTelemetryLayer<S, SdkTracer>
    where
        S: Subscriber + for<'span> LookupSpan<'span>,
    {
        tracing_opentelemetry::layer().with_tracer(self.provider.tracer(self.tracer_name.clone()))
    }

    /// Exports every span buffered so far without shutting the pipeline down.
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryError::Shutdown`] when a span processor fails to
    /// flush.
    pub fn force_flush(&self) -> Result<(), TelemetryError> {
        self.provider
            .force_flush()
            .map_err(|error| TelemetryError::Shutdown(error.to_string()))
    }

    /// Drains buffered spans and shuts the tracer provider down.
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryError::Shutdown`] when the final export fails. An
    /// already completed shutdown is reported as success, so this is safe to
    /// call on a lifespan path that may run twice.
    pub fn shutdown(mut self) -> Result<(), TelemetryError> {
        self.shut_down = true;
        drain(&self.provider)
    }
}

impl Drop for TelemetryPipeline {
    fn drop(&mut self) {
        if self.shut_down {
            return;
        }
        if let Err(error) = drain(&self.provider) {
            tracing::warn!(
                target: "blazingly::otel",
                error = %error,
                "telemetry pipeline dropped without a clean shutdown"
            );
        }
    }
}

fn drain(provider: &SdkTracerProvider) -> Result<(), TelemetryError> {
    match provider.shutdown() {
        Ok(()) | Err(OTelSdkError::AlreadyShutdown) => Ok(()),
        Err(error) => Err(TelemetryError::Shutdown(error.to_string())),
    }
}

/// Builds an export pipeline driven by the built-in [`BlockingHttpClient`].
///
/// The pipeline is inert until its [`layer`](TelemetryPipeline::layer) is added
/// to a subscriber; [`install`] does both in one step.
///
/// # Errors
///
/// Returns [`TelemetryError::Exporter`] when the OTLP exporter rejects the
/// configuration.
pub fn build(config: &OtlpConfig) -> Result<TelemetryPipeline, TelemetryError> {
    build_with_client(config, BlockingHttpClient::new(config.timeout))
}

/// Builds an export pipeline driven by a caller-supplied HTTP client.
///
/// This is the seam for transports the framework will not take on: TLS,
/// authenticating proxies, or an existing connection pool. The client is called
/// from the SDK export thread and may block it.
///
/// # Errors
///
/// Returns [`TelemetryError::Exporter`] when the OTLP exporter rejects the
/// configuration.
pub fn build_with_client(
    config: &OtlpConfig,
    client: impl HttpClient + 'static,
) -> Result<TelemetryPipeline, TelemetryError> {
    let headers: HashMap<String, String> = config
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let exporter = SpanExporter::builder()
        .with_http()
        .with_http_client(client)
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(config.endpoint.clone())
        .with_timeout(config.timeout)
        .with_headers(headers)
        .build()
        .map_err(|error| TelemetryError::Exporter(error.to_string()))?;
    let resource = Resource::builder()
        .with_service_name(config.service_name.clone())
        .build();
    Ok(TelemetryPipeline {
        provider: SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource)
            .build(),
        tracer_name: config.tracer_name.clone(),
        shut_down: false,
    })
}

/// Builds a pipeline and installs it as the global `tracing` subscriber.
///
/// Hold the returned pipeline for the process lifetime and call
/// [`TelemetryPipeline::shutdown`] on the shutdown path, otherwise the last
/// batch of spans never reaches the collector.
///
/// # Errors
///
/// Returns [`TelemetryError::Exporter`] when the OTLP exporter rejects the
/// configuration, and [`TelemetryError::SubscriberAlreadySet`] when a global
/// subscriber is already installed. In the latter case, build the pipeline with
/// [`build`] and add its [`layer`](TelemetryPipeline::layer) to the subscriber
/// the application already owns.
pub fn install(config: &OtlpConfig) -> Result<TelemetryPipeline, TelemetryError> {
    install_with_client(config, BlockingHttpClient::new(config.timeout))
}

/// Installs a global subscriber over a pipeline using a caller-supplied client.
///
/// # Errors
///
/// Returns [`TelemetryError::Exporter`] when the OTLP exporter rejects the
/// configuration, and [`TelemetryError::SubscriberAlreadySet`] when a global
/// subscriber is already installed.
pub fn install_with_client(
    config: &OtlpConfig,
    client: impl HttpClient + 'static,
) -> Result<TelemetryPipeline, TelemetryError> {
    let pipeline = build_with_client(config, client)?;
    let subscriber = tracing_subscriber::registry().with(pipeline.layer());
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|_| TelemetryError::SubscriberAlreadySet)?;
    Ok(pipeline)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_configuration_targets_a_local_collector() {
        let config = OtlpConfig::default();
        assert_eq!(config.endpoint, "http://localhost:4318/v1/traces");
        assert_eq!(config.timeout, DEFAULT_EXPORT_TIMEOUT);
        assert_eq!(config.tracer_name, DEFAULT_TRACER_NAME);
        assert!(config.headers.is_empty());

        let tuned = OtlpConfig::default()
            .with_endpoint("http://collector:4318/v1/traces")
            .with_service_name("checkout-api")
            .with_timeout(Duration::from_secs(3))
            .with_tracer_name("checkout")
            .with_header("authorization", "Bearer token")
            .with_header("authorization", "Bearer rotated");
        assert_eq!(tuned.endpoint, "http://collector:4318/v1/traces");
        assert_eq!(tuned.service_name, "checkout-api");
        assert_eq!(tuned.timeout, Duration::from_secs(3));
        assert_eq!(tuned.tracer_name, "checkout");
        assert_eq!(
            tuned.headers.get("authorization").map(String::as_str),
            Some("Bearer rotated")
        );
    }

    #[test]
    fn a_pipeline_builds_and_shuts_down_without_a_runtime() {
        // No collector is listening; the point is that building the provider and
        // draining it need no reactor and no live endpoint.
        let pipeline = build(&OtlpConfig::default().with_service_name("test-service"))
            .expect("pipeline builds");
        pipeline.force_flush().expect("flush drains the batch");
        pipeline.shutdown().expect("shutdown drains the batch");
    }

    #[test]
    fn shutdown_is_idempotent_against_the_drop_guard() {
        let pipeline = build(&OtlpConfig::default()).expect("pipeline builds");
        let provider = pipeline.provider().clone();
        pipeline.shutdown().expect("first shutdown succeeds");
        // The provider is already down; a second drain must still report success
        // so a lifespan hook that runs twice does not fail the shutdown path.
        assert!(drain(&provider).is_ok());
    }

    #[test]
    fn a_built_pipeline_exposes_a_layer_for_an_application_subscriber() {
        let pipeline = build(&OtlpConfig::default()).expect("pipeline builds");
        let subscriber = tracing_subscriber::registry().with(pipeline.layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info_span!("probe").in_scope(|| {});
        });
        pipeline.shutdown().expect("shutdown drains the batch");
    }
}
