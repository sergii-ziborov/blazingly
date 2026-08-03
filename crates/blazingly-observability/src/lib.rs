#![forbid(unsafe_code)]

//! Runtime-neutral request identity, access events, tracing, and metrics.

#[cfg(feature = "otel")]
pub mod otel;

use blazingly_core::{HttpMethod, OperationDescriptor, SecuritySchemeDescriptor};
use blazingly_http::{HttpMiddleware, HttpRequestContext, Response};
#[cfg(feature = "otel")]
use opentelemetry::Context as OtelContext;
#[cfg(feature = "otel")]
use opentelemetry::trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
#[cfg(feature = "otel")]
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

/// Default request duration histogram upper bounds, in seconds.
pub const DEFAULT_DURATION_BUCKETS_SECONDS: [f64; 10] =
    [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0];

/// Maximum distinct label sets retained per metric family.
///
/// Built-in HTTP metrics fold a new label set past the cap into an
/// `<overflow>` route series. Application metrics drop it and log one warning
/// per family, so a mislabelled call site cannot exhaust process memory.
pub const MAX_LABEL_SETS_PER_METRIC: usize = 128;

/// Longest accepted background queue name.
pub const MAX_QUEUE_NAME_BYTES: usize = 128;

const UNMATCHED_ROUTE: &str = "<unmatched>";
const AGGREGATE_ROUTE: &str = "<all>";
const OVERFLOW_ROUTE: &str = "<overflow>";
const RESERVED_METRIC_PREFIX: &str = "blazingly_";
/// Built-in families without the reserved prefix, closed to application use so
/// a scrape cannot carry two declarations of the same family.
const RESERVED_METRIC_NAMES: [&str; 2] =
    ["process_resident_memory_bytes", "process_cpu_seconds_total"];
const QUEUE_DEPTH_METRIC: &str = "blazingly_background_queue_depth";
const MAX_TRACESTATE_ENTRIES: usize = 32;
const MAX_TRACESTATE_KEY_BYTES: usize = 256;
const MAX_TRACESTATE_VALUE_BYTES: usize = 256;
/// `/proc/self/stat` reports CPU time in `USER_HZ`, fixed at 100 by the procfs
/// ABI regardless of the kernel's internal tick rate.
#[cfg(target_os = "linux")]
const PROC_USER_HZ: u64 = 100;

type LabelSet = Vec<(String, String)>;
type QueueDepthSource = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Request identifier available through `Extension<RequestId>`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RequestId(String);

impl RequestId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// W3C-compatible trace identity available through `Extension<TraceContext>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TraceContext {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    sampled: bool,
    tracestate: Vec<(String, String)>,
}

impl TraceContext {
    #[must_use]
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    #[must_use]
    pub fn span_id(&self) -> &str {
        &self.span_id
    }

    #[must_use]
    pub fn parent_span_id(&self) -> Option<&str> {
        self.parent_span_id.as_deref()
    }

    #[must_use]
    pub const fn sampled(&self) -> bool {
        self.sampled
    }

    #[must_use]
    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{}",
            self.trace_id,
            self.span_id,
            if self.sampled { "01" } else { "00" }
        )
    }

    /// Validated inbound `tracestate` list members, in received order.
    #[must_use]
    pub fn tracestate_entries(&self) -> &[(String, String)] {
        &self.tracestate
    }

    /// Renders the `tracestate` header value when the request carried one.
    #[must_use]
    pub fn tracestate(&self) -> Option<String> {
        if self.tracestate.is_empty() {
            return None;
        }
        let mut rendered = String::new();
        for (index, (key, value)) in self.tracestate.iter().enumerate() {
            if index > 0 {
                rendered.push(',');
            }
            let _ = write!(rendered, "{key}={value}");
        }
        Some(rendered)
    }
}

/// Completed request information delivered to an access log sink.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessEvent {
    pub request_id: RequestId,
    pub trace: TraceContext,
    pub method: HttpMethod,
    pub target: String,
    pub route: Option<String>,
    pub operation_id: Option<String>,
    pub status: u16,
    pub duration: Duration,
    pub client_ip: Option<IpAddr>,
    pub response_bytes: Option<u64>,
}

/// Destination for structured access events.
pub trait AccessLogSink: Send + Sync {
    fn emit(&self, event: &AccessEvent);
}

/// Emits structured access events through the `tracing` ecosystem.
#[derive(Clone, Copy, Debug, Default)]
pub struct TracingAccessLog;

impl AccessLogSink for TracingAccessLog {
    fn emit(&self, event: &AccessEvent) {
        if event.status >= 500 {
            tracing::error!(
                target: "blazingly::access",
                request_id = %event.request_id,
                trace_id = %event.trace.trace_id(),
                method = event.method.as_str(),
                target = event.target,
                route = event.route.as_deref().unwrap_or(""),
                operation_id = event.operation_id.as_deref().unwrap_or(""),
                status = event.status,
                duration_micros = event.duration.as_micros(),
                response_bytes = event.response_bytes,
                client_ip = event.client_ip.map(|ip| ip.to_string()).as_deref().unwrap_or(""),
                "request failed"
            );
        } else {
            tracing::info!(
                target: "blazingly::access",
                request_id = %event.request_id,
                trace_id = %event.trace.trace_id(),
                method = event.method.as_str(),
                target = event.target,
                route = event.route.as_deref().unwrap_or(""),
                operation_id = event.operation_id.as_deref().unwrap_or(""),
                status = event.status,
                duration_micros = event.duration.as_micros(),
                response_bytes = event.response_bytes,
                client_ip = event.client_ip.map(|ip| ip.to_string()).as_deref().unwrap_or(""),
                "request completed"
            );
        }
    }
}

/// Runtime configuration for [`Observability`].
#[derive(Clone, Debug)]
pub struct ObservabilityConfig {
    pub request_id_header: String,
    /// Reuse the inbound request id header instead of minting one.
    ///
    /// Defaults to `false`: an untrusted client would otherwise choose the
    /// correlation id of every log line. Enable it through
    /// [`ObservabilityConfig::trust_incoming_request_id_from`].
    pub accept_incoming_request_id: bool,
    /// Peer addresses whose request id header is trusted. Empty trusts every
    /// peer once `accept_incoming_request_id` is set.
    pub trusted_request_id_peers: Vec<IpAddr>,
    pub metrics_path: Option<String>,
    pub detailed_route_metrics: bool,
    pub access_log: bool,
    /// Request duration histogram upper bounds, in seconds.
    pub duration_buckets_seconds: Vec<f64>,
}

impl ObservabilityConfig {
    /// Trusts the inbound request id header only from these peer addresses.
    ///
    /// Only safe when a reverse proxy at those addresses overwrites the header
    /// on every hop; otherwise a client picks its own correlation id.
    #[must_use]
    pub fn trust_incoming_request_id_from(
        mut self,
        peers: impl IntoIterator<Item = IpAddr>,
    ) -> Self {
        self.accept_incoming_request_id = true;
        self.trusted_request_id_peers = peers.into_iter().collect();
        self
    }

    /// Trusts the inbound request id header from every peer.
    ///
    /// Only safe when the process is unreachable except through a trusted
    /// proxy, because the header is otherwise attacker controlled.
    #[must_use]
    pub fn trust_incoming_request_id_from_any_peer(mut self) -> Self {
        self.accept_incoming_request_id = true;
        self.trusted_request_id_peers = Vec::new();
        self
    }

    /// Replaces the request duration histogram upper bounds, in seconds.
    ///
    /// Non-finite bounds are ignored; an empty result keeps
    /// [`DEFAULT_DURATION_BUCKETS_SECONDS`].
    #[must_use]
    pub fn with_duration_buckets(mut self, buckets: impl IntoIterator<Item = f64>) -> Self {
        self.duration_buckets_seconds = sanitized_buckets(buckets);
        self
    }
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            request_id_header: "x-request-id".to_owned(),
            accept_incoming_request_id: false,
            trusted_request_id_peers: Vec::new(),
            metrics_path: Some("/metrics".to_owned()),
            detailed_route_metrics: false,
            access_log: true,
            duration_buckets_seconds: DEFAULT_DURATION_BUCKETS_SECONDS.to_vec(),
        }
    }
}

/// Failure while registering or recording an application metric.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetricError {
    /// Metric or label name is not a valid Prometheus identifier, is reserved,
    /// or repeats a label name.
    InvalidName(String),
    /// A metric family with this name is already registered.
    AlreadyRegistered(String),
    /// No metric family with this name is registered.
    NotRegistered(String),
    /// The registered metric family has a different type.
    TypeMismatch(String),
    /// The registry is already holding [`MAX_LABEL_SETS_PER_METRIC`] entries.
    CapacityExceeded(String),
}

impl std::fmt::Display for MetricError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(name) => write!(formatter, "invalid metric or label name `{name}`"),
            Self::AlreadyRegistered(name) => write!(formatter, "metric `{name}` already exists"),
            Self::NotRegistered(name) => write!(formatter, "metric `{name}` is not registered"),
            Self::TypeMismatch(name) => write!(formatter, "metric `{name}` has another type"),
            Self::CapacityExceeded(name) => {
                write!(formatter, "registry is full, rejected `{name}`")
            }
        }
    }
}

impl std::error::Error for MetricError {}

#[derive(Clone, Debug, Default)]
struct HistogramSeries {
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl HistogramSeries {
    fn new(bounds: usize) -> Self {
        Self {
            counts: vec![0; bounds + 1],
            sum: 0.0,
            count: 0,
        }
    }

    fn observe(&mut self, bounds: &[f64], value: f64) {
        let index = bounds
            .iter()
            .position(|bound| value <= *bound)
            .unwrap_or(bounds.len());
        self.counts[index] += 1;
        self.sum += value;
        self.count += 1;
    }
}

enum ApplicationMetricKind {
    Counter(BTreeMap<LabelSet, u64>),
    Gauge(BTreeMap<LabelSet, f64>),
    Histogram {
        bounds: Vec<f64>,
        series: BTreeMap<LabelSet, HistogramSeries>,
    },
}

struct ApplicationMetric {
    help: String,
    kind: ApplicationMetricKind,
    overflow_warned: bool,
}

/// Atomic HTTP metrics with Prometheus text exposition.
pub struct Metrics {
    requests: AtomicU64,
    in_flight: AtomicU64,
    responses_2xx: AtomicU64,
    responses_3xx: AtomicU64,
    responses_4xx: AtomicU64,
    responses_5xx: AtomicU64,
    errors: AtomicU64,
    dropped_label_sets: AtomicU64,
    duration_bounds: Vec<f64>,
    duration: Mutex<BTreeMap<LabelSet, HistogramSeries>>,
    detailed: Mutex<BTreeMap<LabelSet, u64>>,
    application: Mutex<BTreeMap<String, ApplicationMetric>>,
    queues: Mutex<BTreeMap<String, QueueDepthSource>>,
}

impl Metrics {
    #[must_use]
    pub fn new() -> Self {
        Self::with_duration_buckets(DEFAULT_DURATION_BUCKETS_SECONDS)
    }

    /// Creates metrics with explicit duration histogram upper bounds, in
    /// seconds.
    #[must_use]
    pub fn with_duration_buckets(buckets: impl IntoIterator<Item = f64>) -> Self {
        Self {
            requests: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            responses_2xx: AtomicU64::new(0),
            responses_3xx: AtomicU64::new(0),
            responses_4xx: AtomicU64::new(0),
            responses_5xx: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            dropped_label_sets: AtomicU64::new(0),
            duration_bounds: sanitized_buckets(buckets),
            duration: Mutex::new(BTreeMap::new()),
            detailed: Mutex::new(BTreeMap::new()),
            application: Mutex::new(BTreeMap::new()),
            queues: Mutex::new(BTreeMap::new()),
        }
    }

    #[must_use]
    pub fn requests_total(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn errors_total(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Label sets folded into `<overflow>` or dropped past
    /// [`MAX_LABEL_SETS_PER_METRIC`].
    #[must_use]
    pub fn dropped_label_sets_total(&self) -> u64 {
        self.dropped_label_sets.load(Ordering::Relaxed)
    }

    /// Registers an application counter family.
    ///
    /// # Errors
    ///
    /// Returns [`MetricError::InvalidName`] for a name that is not a valid
    /// Prometheus identifier or uses the reserved `blazingly_` prefix, and
    /// [`MetricError::AlreadyRegistered`] when the family exists.
    pub fn register_counter(&self, name: &str, help: &str) -> Result<(), MetricError> {
        self.register(name, help, ApplicationMetricKind::Counter(BTreeMap::new()))
    }

    /// Registers an application gauge family.
    ///
    /// # Errors
    ///
    /// Returns [`MetricError::InvalidName`] for a name that is not a valid
    /// Prometheus identifier, uses the reserved `blazingly_` prefix, or collides
    /// with a built-in family, and [`MetricError::AlreadyRegistered`] when the
    /// family exists.
    pub fn register_gauge(&self, name: &str, help: &str) -> Result<(), MetricError> {
        self.register(name, help, ApplicationMetricKind::Gauge(BTreeMap::new()))
    }

    /// Registers an application histogram family with explicit upper bounds.
    ///
    /// # Errors
    ///
    /// Returns [`MetricError::InvalidName`] for a name that is not a valid
    /// Prometheus identifier or uses the reserved `blazingly_` prefix, and
    /// [`MetricError::AlreadyRegistered`] when the family exists.
    pub fn register_histogram(
        &self,
        name: &str,
        help: &str,
        buckets: impl IntoIterator<Item = f64>,
    ) -> Result<(), MetricError> {
        self.register(
            name,
            help,
            ApplicationMetricKind::Histogram {
                bounds: sanitized_buckets(buckets),
                series: BTreeMap::new(),
            },
        )
    }

    /// Adds one to a registered counter series.
    ///
    /// # Errors
    ///
    /// Returns [`MetricError::InvalidName`] for a rejected label name,
    /// [`MetricError::NotRegistered`] for an unknown family, and
    /// [`MetricError::TypeMismatch`] when the family is a histogram.
    pub fn increment_counter(
        &self,
        name: &str,
        labels: &[(&str, &str)],
    ) -> Result<(), MetricError> {
        self.add_to_counter(name, labels, 1)
    }

    /// Adds `value` to a registered counter series.
    ///
    /// Label sets past [`MAX_LABEL_SETS_PER_METRIC`] are dropped and warned
    /// about once per family.
    ///
    /// # Errors
    ///
    /// Returns [`MetricError::InvalidName`] for a rejected label name,
    /// [`MetricError::NotRegistered`] for an unknown family, and
    /// [`MetricError::TypeMismatch`] when the family is a histogram.
    pub fn add_to_counter(
        &self,
        name: &str,
        labels: &[(&str, &str)],
        value: u64,
    ) -> Result<(), MetricError> {
        let labels = label_set(labels)?;
        let mut application = self
            .application
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let metric = application
            .get_mut(name)
            .ok_or_else(|| MetricError::NotRegistered(name.to_owned()))?;
        let ApplicationMetricKind::Counter(series) = &mut metric.kind else {
            return Err(MetricError::TypeMismatch(name.to_owned()));
        };
        if series.len() >= MAX_LABEL_SETS_PER_METRIC && !series.contains_key(&labels) {
            note_overflow(&self.dropped_label_sets, &mut metric.overflow_warned, name);
            return Ok(());
        }
        *series.entry(labels).or_default() += value;
        Ok(())
    }

    /// Records one observation in a registered histogram series.
    ///
    /// Label sets past [`MAX_LABEL_SETS_PER_METRIC`] are dropped and warned
    /// about once per family.
    ///
    /// # Errors
    ///
    /// Returns [`MetricError::InvalidName`] for a rejected label name,
    /// [`MetricError::NotRegistered`] for an unknown family, and
    /// [`MetricError::TypeMismatch`] when the family is a counter.
    pub fn observe_histogram(
        &self,
        name: &str,
        labels: &[(&str, &str)],
        value: f64,
    ) -> Result<(), MetricError> {
        let labels = label_set(labels)?;
        let mut application = self
            .application
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let metric = application
            .get_mut(name)
            .ok_or_else(|| MetricError::NotRegistered(name.to_owned()))?;
        let ApplicationMetricKind::Histogram { bounds, series } = &mut metric.kind else {
            return Err(MetricError::TypeMismatch(name.to_owned()));
        };
        if series.len() >= MAX_LABEL_SETS_PER_METRIC && !series.contains_key(&labels) {
            note_overflow(&self.dropped_label_sets, &mut metric.overflow_warned, name);
            return Ok(());
        }
        series
            .entry(labels)
            .or_insert_with(|| HistogramSeries::new(bounds.len()))
            .observe(bounds, value);
        Ok(())
    }

    /// Sets a registered gauge series to `value`.
    ///
    /// Label sets past [`MAX_LABEL_SETS_PER_METRIC`] are dropped and warned
    /// about once per family.
    ///
    /// # Errors
    ///
    /// Returns [`MetricError::InvalidName`] for a rejected label name,
    /// [`MetricError::NotRegistered`] for an unknown family, and
    /// [`MetricError::TypeMismatch`] when the family is not a gauge.
    pub fn set_gauge(
        &self,
        name: &str,
        labels: &[(&str, &str)],
        value: f64,
    ) -> Result<(), MetricError> {
        let labels = label_set(labels)?;
        let mut application = self
            .application
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let metric = application
            .get_mut(name)
            .ok_or_else(|| MetricError::NotRegistered(name.to_owned()))?;
        let ApplicationMetricKind::Gauge(series) = &mut metric.kind else {
            return Err(MetricError::TypeMismatch(name.to_owned()));
        };
        if series.len() >= MAX_LABEL_SETS_PER_METRIC && !series.contains_key(&labels) {
            note_overflow(&self.dropped_label_sets, &mut metric.overflow_warned, name);
            return Ok(());
        }
        series.insert(labels, value);
        Ok(())
    }

    /// Returns the current value of a registered gauge series.
    #[must_use]
    pub fn gauge_value(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        let labels = label_set(labels).ok()?;
        let application = self
            .application
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match &application.get(name)?.kind {
            ApplicationMetricKind::Gauge(series) => series.get(&labels).copied(),
            ApplicationMetricKind::Counter(_) | ApplicationMetricKind::Histogram { .. } => None,
        }
    }

    /// Registers a background queue whose depth is sampled on every scrape.
    ///
    /// This crate cannot see any queue on its own, so `depth` is the hook: a
    /// worker pool, a job runner, or the OTLP export backlog supplies a closure
    /// reading its own pending count, and it surfaces as
    /// `blazingly_background_queue_depth{queue="..."}`. The closure runs during
    /// [`Metrics::prometheus`], so it must be cheap, must not block, and must not
    /// call back into this [`Metrics`].
    ///
    /// # Errors
    ///
    /// Returns [`MetricError::InvalidName`] when `queue` is empty or longer than
    /// [`MAX_QUEUE_NAME_BYTES`], [`MetricError::AlreadyRegistered`] when the
    /// queue is already tracked, and [`MetricError::CapacityExceeded`] once
    /// [`MAX_LABEL_SETS_PER_METRIC`] queues are registered.
    pub fn track_queue_depth(
        &self,
        queue: &str,
        depth: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Result<(), MetricError> {
        if queue.is_empty() || queue.len() > MAX_QUEUE_NAME_BYTES {
            return Err(MetricError::InvalidName(queue.to_owned()));
        }
        let mut queues = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
        if queues.contains_key(queue) {
            return Err(MetricError::AlreadyRegistered(queue.to_owned()));
        }
        if queues.len() >= MAX_LABEL_SETS_PER_METRIC {
            return Err(MetricError::CapacityExceeded(queue.to_owned()));
        }
        queues.insert(queue.to_owned(), Arc::new(depth));
        Ok(())
    }

    /// Samples a tracked queue's depth now.
    #[must_use]
    pub fn queue_depth(&self, queue: &str) -> Option<u64> {
        let source = {
            let queues = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(queues.get(queue)?)
        };
        Some(source())
    }

    /// Returns the current value of a registered counter series.
    #[must_use]
    pub fn counter_value(&self, name: &str, labels: &[(&str, &str)]) -> Option<u64> {
        let labels = label_set(labels).ok()?;
        let application = self
            .application
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match &application.get(name)?.kind {
            ApplicationMetricKind::Counter(series) => series.get(&labels).copied(),
            ApplicationMetricKind::Gauge(_) | ApplicationMetricKind::Histogram { .. } => None,
        }
    }

    /// Returns a Prometheus 0.0.4 text exposition snapshot.
    #[must_use]
    pub fn prometheus(&self) -> String {
        let mut output = String::with_capacity(4_096);
        self.write_http_counters(&mut output);
        self.write_duration_histogram(&mut output);
        self.write_route_counters(&mut output);
        self.write_queue_depths(&mut output);
        write_process_metrics(&mut output);
        self.write_application_metrics(&mut output);
        output
    }

    fn register(
        &self,
        name: &str,
        help: &str,
        kind: ApplicationMetricKind,
    ) -> Result<(), MetricError> {
        if !valid_metric_name(name)
            || name.starts_with(RESERVED_METRIC_PREFIX)
            || RESERVED_METRIC_NAMES.contains(&name)
        {
            return Err(MetricError::InvalidName(name.to_owned()));
        }
        let mut application = self
            .application
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if application.contains_key(name) {
            return Err(MetricError::AlreadyRegistered(name.to_owned()));
        }
        application.insert(
            name.to_owned(),
            ApplicationMetric {
                help: help.to_owned(),
                kind,
                overflow_warned: false,
            },
        );
        Ok(())
    }

    fn write_http_counters(&self, output: &mut String) {
        write_counter(
            output,
            "blazingly_http_requests_total",
            "Total HTTP requests accepted by middleware",
            self.requests_total(),
        );
        write_gauge(
            output,
            "blazingly_http_requests_in_flight",
            "HTTP requests currently in flight",
            self.in_flight(),
        );
        write_counter(
            output,
            "blazingly_http_responses_2xx_total",
            "HTTP responses with a 2xx status",
            self.responses_2xx.load(Ordering::Relaxed),
        );
        write_counter(
            output,
            "blazingly_http_responses_3xx_total",
            "HTTP responses with a 3xx status",
            self.responses_3xx.load(Ordering::Relaxed),
        );
        write_counter(
            output,
            "blazingly_http_responses_4xx_total",
            "HTTP responses with a 4xx status",
            self.responses_4xx.load(Ordering::Relaxed),
        );
        write_counter(
            output,
            "blazingly_http_responses_5xx_total",
            "HTTP responses with a 5xx status",
            self.responses_5xx.load(Ordering::Relaxed),
        );
        write_counter(
            output,
            "blazingly_http_errors_total",
            "HTTP responses with a 4xx or 5xx status",
            self.errors_total(),
        );
        write_counter(
            output,
            "blazingly_metrics_dropped_label_sets_total",
            "Metric label sets folded or dropped past the cardinality cap",
            self.dropped_label_sets_total(),
        );
    }

    fn write_duration_histogram(&self, output: &mut String) {
        let series = self.duration.lock().unwrap_or_else(PoisonError::into_inner);
        write_histogram(
            output,
            "blazingly_http_request_duration_seconds",
            "HTTP request duration",
            &self.duration_bounds,
            &series,
        );
    }

    fn write_route_counters(&self, output: &mut String) {
        let detailed = self.detailed.lock().unwrap_or_else(PoisonError::into_inner);
        write_labeled_counter(
            output,
            "blazingly_http_route_responses_total",
            "HTTP responses by route and status",
            &detailed,
        );
    }

    fn write_queue_depths(&self, output: &mut String) {
        // The closures are cloned out before sampling so a source that touches
        // this `Metrics` cannot deadlock against the registry lock.
        let sources: Vec<(String, QueueDepthSource)> = {
            let queues = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
            queues
                .iter()
                .map(|(queue, source)| (queue.clone(), Arc::clone(source)))
                .collect()
        };
        if sources.is_empty() {
            return;
        }
        write_family_header(
            output,
            QUEUE_DEPTH_METRIC,
            "Items waiting in a registered background queue",
            "gauge",
        );
        for (queue, source) in sources {
            let _ = write!(output, "{QUEUE_DEPTH_METRIC}");
            write_label_set(output, &[("queue".to_owned(), queue)], None);
            let _ = writeln!(output, " {}", source());
        }
    }

    fn write_application_metrics(&self, output: &mut String) {
        let application = self
            .application
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (name, metric) in &*application {
            match &metric.kind {
                ApplicationMetricKind::Counter(series) => {
                    write_labeled_counter(output, name, &metric.help, series);
                }
                ApplicationMetricKind::Gauge(series) => {
                    write_labeled_gauge(output, name, &metric.help, series);
                }
                ApplicationMetricKind::Histogram { bounds, series } => {
                    write_histogram(output, name, &metric.help, bounds, series);
                }
            }
        }
    }

    fn start(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.in_flight.fetch_add(1, Ordering::Relaxed);
    }

    fn finish(
        &self,
        method: HttpMethod,
        route: Option<&str>,
        status: u16,
        duration: Duration,
        detailed: bool,
    ) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        match status / 100 {
            2 => &self.responses_2xx,
            3 => &self.responses_3xx,
            4 => &self.responses_4xx,
            _ => &self.responses_5xx,
        }
        .fetch_add(1, Ordering::Relaxed);
        if status >= 400 {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        self.observe_duration(method, route, status, duration, detailed);
        if detailed {
            self.count_route_response(method, route, status);
        }
    }

    fn observe_duration(
        &self,
        method: HttpMethod,
        route: Option<&str>,
        status: u16,
        duration: Duration,
        detailed: bool,
    ) {
        let class = status_class(status);
        let route = if detailed {
            route.unwrap_or(UNMATCHED_ROUTE)
        } else {
            AGGREGATE_ROUTE
        };
        let mut labels = http_labels(method, route, class);
        let mut series = self.duration.lock().unwrap_or_else(PoisonError::into_inner);
        if series.len() >= MAX_LABEL_SETS_PER_METRIC && !series.contains_key(&labels) {
            self.dropped_label_sets.fetch_add(1, Ordering::Relaxed);
            labels = http_labels(method, OVERFLOW_ROUTE, class);
        }
        series
            .entry(labels)
            .or_insert_with(|| HistogramSeries::new(self.duration_bounds.len()))
            .observe(&self.duration_bounds, duration.as_secs_f64());
    }

    fn count_route_response(&self, method: HttpMethod, route: Option<&str>, status: u16) {
        let status = status.to_string();
        let mut labels = http_labels(method, route.unwrap_or(UNMATCHED_ROUTE), &status);
        let mut counters = self.detailed.lock().unwrap_or_else(PoisonError::into_inner);
        if counters.len() >= MAX_LABEL_SETS_PER_METRIC && !counters.contains_key(&labels) {
            self.dropped_label_sets.fetch_add(1, Ordering::Relaxed);
            labels = http_labels(method, OVERFLOW_ROUTE, &status);
        }
        *counters.entry(labels).or_default() += 1;
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

struct RequestTelemetry {
    started: Instant,
    request_id: RequestId,
    trace: TraceContext,
    span: tracing::Span,
    counted: bool,
}

/// Runtime-neutral middleware combining request IDs, tracing, access logs, and metrics.
#[derive(Clone)]
pub struct Observability {
    config: ObservabilityConfig,
    metrics: Arc<Metrics>,
    sink: Arc<dyn AccessLogSink>,
}

impl Observability {
    #[must_use]
    pub fn new(config: ObservabilityConfig) -> Self {
        let metrics =
            Metrics::with_duration_buckets(config.duration_buckets_seconds.iter().copied());
        Self {
            config,
            metrics: Arc::new(metrics),
            sink: Arc::new(TracingAccessLog),
        }
    }

    #[must_use]
    pub fn with_access_sink(mut self, sink: impl AccessLogSink + 'static) -> Self {
        self.sink = Arc::new(sink);
        self
    }

    #[must_use]
    pub fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.metrics)
    }

    fn is_metrics_request(&self, method: HttpMethod, target: &str) -> bool {
        let path = target.split_once('?').map_or(target, |(path, _)| path);
        method == HttpMethod::Get
            && self
                .config
                .metrics_path
                .as_deref()
                .is_some_and(|metrics_path| metrics_path == path)
    }

    fn request_id(&self, context: &HttpRequestContext<'_>) -> RequestId {
        let incoming = context
            .request()
            .header_value(&self.config.request_id_header, 0);
        self.resolve_request_id(incoming, context.client_ip())
    }

    fn resolve_request_id(&self, incoming: Option<&str>, peer: Option<IpAddr>) -> RequestId {
        if self.trusts_peer(peer)
            && let Some(value) = incoming
            && valid_request_id(value)
        {
            return RequestId(value.to_owned());
        }
        RequestId(Uuid::new_v4().to_string())
    }

    fn trusts_peer(&self, peer: Option<IpAddr>) -> bool {
        if !self.config.accept_incoming_request_id {
            return false;
        }
        if self.config.trusted_request_id_peers.is_empty() {
            return true;
        }
        peer.is_some_and(|peer| self.config.trusted_request_id_peers.contains(&peer))
    }
}

impl Default for Observability {
    fn default() -> Self {
        Self::new(ObservabilityConfig::default())
    }
}

impl HttpMiddleware for Observability {
    fn on_request(&self, context: &mut HttpRequestContext<'_>) -> Option<Response> {
        let scrape =
            self.is_metrics_request(context.request().method(), context.request().target());
        if !scrape {
            self.metrics.start();
        }
        let request_id = self.request_id(context);
        let trace = build_trace_context(
            context.request().header_value("traceparent", 0),
            context.request().header_value("tracestate", 0),
        );
        let span = tracing::info_span!(
            target: "blazingly::http",
            "http.server.request",
            request_id = %request_id,
            trace_id = %trace.trace_id(),
            span_id = %trace.span_id(),
            http.request.method = context.request().method().as_str(),
            url.path = context.request().target(),
            http.route = tracing::field::Empty,
            blazingly.operation_id = tracing::field::Empty,
            http.response.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        );
        set_remote_parent(&span, &trace);
        context.insert_extension(request_id.clone());
        context.insert_extension(trace.clone());
        context.insert_extension(RequestTelemetry {
            started: Instant::now(),
            request_id,
            trace,
            span,
            counted: !scrape,
        });

        if scrape {
            let mut response = Response::from_bytes(200, self.metrics.prometheus());
            response.set_header("content-type", "text/plain; version=0.0.4; charset=utf-8");
            return Some(response);
        }
        None
    }

    fn on_operation(
        &self,
        _context: &mut HttpRequestContext<'_>,
        _operation: &OperationDescriptor,
        _security_schemes: &[SecuritySchemeDescriptor],
    ) -> Option<Response> {
        None
    }

    fn on_response(
        &self,
        context: &HttpRequestContext<'_>,
        operation: Option<&OperationDescriptor>,
        response: &mut Response,
    ) {
        let Some(telemetry) = context.extension::<RequestTelemetry>() else {
            return;
        };
        let duration = telemetry.started.elapsed();
        let route = operation.map(|operation| operation.http.path.as_str());
        let operation_id = operation.map(|operation| operation.contract.id.as_str());
        response.set_header(
            &self.config.request_id_header,
            telemetry.request_id.as_str(),
        );
        response.set_header("traceparent", telemetry.trace.traceparent());
        if let Some(tracestate) = telemetry.trace.tracestate() {
            response.set_header("tracestate", tracestate);
        }
        telemetry
            .span
            .record("http.response.status_code", response.status());
        if let Some(route) = route {
            telemetry.span.record("http.route", route);
        }
        if let Some(operation_id) = operation_id {
            telemetry
                .span
                .record("blazingly.operation_id", operation_id);
        }
        if response.status() >= 400 {
            telemetry
                .span
                .record("error.type", format!("http_{}", response.status()));
        }
        if telemetry.counted {
            self.metrics.finish(
                context.request().method(),
                route,
                response.status(),
                duration,
                self.config.detailed_route_metrics,
            );
        }

        if self.config.access_log {
            let event = AccessEvent {
                request_id: telemetry.request_id.clone(),
                trace: telemetry.trace.clone(),
                method: context.request().method(),
                target: context.request().target().to_owned(),
                route: route.map(str::to_owned),
                operation_id: operation_id.map(str::to_owned),
                status: response.status(),
                duration,
                client_ip: context.client_ip(),
                response_bytes: response.exact_body_length(),
            };
            let _entered = telemetry.span.enter();
            self.sink.emit(&event);
        }
    }
}

fn build_trace_context(traceparent: Option<&str>, tracestate: Option<&str>) -> TraceContext {
    if let Some((trace_id, parent_span_id, sampled)) = traceparent.and_then(parse_traceparent) {
        return TraceContext {
            trace_id,
            span_id: random_span_id(),
            parent_span_id: Some(parent_span_id),
            sampled,
            tracestate: tracestate.map(parse_tracestate).unwrap_or_default(),
        };
    }
    TraceContext {
        trace_id: random_trace_id(),
        span_id: random_span_id(),
        parent_span_id: None,
        sampled: true,
        tracestate: Vec::new(),
    }
}

fn random_trace_id() -> String {
    let mut value = Uuid::new_v4().as_u128();
    while value == 0 {
        value = Uuid::new_v4().as_u128();
    }
    format!("{value:032x}")
}

fn random_span_id() -> String {
    let mut value = Uuid::new_v4().as_u64_pair().1;
    while value == 0 {
        value = Uuid::new_v4().as_u64_pair().1;
    }
    format!("{value:016x}")
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn parse_traceparent(value: &str) -> Option<(String, String, bool)> {
    let mut parts = value.split('-');
    let version = parts.next()?;
    let trace_id = parts.next()?;
    let span_id = parts.next()?;
    let flags = parts.next()?;
    if parts.next().is_some()
        || version != "00"
        || trace_id.len() != 32
        || span_id.len() != 16
        || flags.len() != 2
        || !trace_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !span_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !flags.bytes().all(|byte| byte.is_ascii_hexdigit())
        || trace_id.bytes().all(|byte| byte == b'0')
        || span_id.bytes().all(|byte| byte == b'0')
    {
        return None;
    }
    let sampled = u8::from_str_radix(flags, 16).ok()? & 1 == 1;
    Some((
        trace_id.to_ascii_lowercase(),
        span_id.to_ascii_lowercase(),
        sampled,
    ))
}

fn parse_tracestate(value: &str) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = Vec::new();
    for member in value.split(',') {
        let Some((key, entry)) = member.split_once('=') else {
            continue;
        };
        let (key, entry) = (key.trim(), entry.trim());
        if !valid_tracestate_key(key)
            || !valid_tracestate_value(entry)
            || entries.iter().any(|(existing, _)| existing == key)
        {
            continue;
        }
        entries.push((key.to_owned(), entry.to_owned()));
        if entries.len() == MAX_TRACESTATE_ENTRIES {
            break;
        }
    }
    entries
}

fn valid_tracestate_key(key: &str) -> bool {
    if key.is_empty() || key.len() > MAX_TRACESTATE_KEY_BYTES {
        return false;
    }
    match key.split_once('@') {
        Some((tenant, vendor)) => {
            tenant.len() <= 241
                && vendor.len() <= 14
                && valid_tracestate_key_part(tenant)
                && valid_tracestate_key_part(vendor)
        }
        None => valid_tracestate_key_part(key),
    }
}

fn valid_tracestate_key_part(part: &str) -> bool {
    let mut bytes = part.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'-' | b'*' | b'/')
        })
}

fn valid_tracestate_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TRACESTATE_VALUE_BYTES
        && value
            .bytes()
            .all(|byte| (0x20..=0x7e).contains(&byte) && byte != b',' && byte != b'=')
}

#[cfg(feature = "otel")]
fn set_remote_parent(span: &tracing::Span, trace: &TraceContext) {
    let Some(parent_span_id) = trace.parent_span_id() else {
        return;
    };
    let (Ok(trace_id), Ok(span_id)) = (
        TraceId::from_hex(trace.trace_id()),
        SpanId::from_hex(parent_span_id),
    ) else {
        return;
    };
    let flags = if trace.sampled() {
        TraceFlags::SAMPLED
    } else {
        TraceFlags::default()
    };
    let state = TraceState::from_key_value(
        trace
            .tracestate_entries()
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    )
    .unwrap_or_default();
    let remote = SpanContext::new(trace_id, span_id, flags, true, state);
    let parent = OtelContext::new().with_remote_span_context(remote);
    let _ = span.set_parent(parent);
}

#[cfg(not(feature = "otel"))]
fn set_remote_parent(_span: &tracing::Span, _trace: &TraceContext) {}

/// Resident set size of this process, in bytes.
///
/// Linux reads `/proc/self/status`; Windows reads the working set through
/// `GetProcessMemoryInfo`; macOS reads the resident size through Mach
/// `task_info`. The platform APIs on Windows and macOS are accessed through the
/// safe `memory-stats` wrapper because this workspace forbids unsafe code.
/// Returns `None` on unsupported platforms or when the platform query fails,
/// and the `process_resident_memory_bytes` family is then absent from a scrape.
#[must_use]
pub fn process_resident_memory_bytes() -> Option<u64> {
    platform_resident_memory_bytes()
}

/// Total user plus system CPU time consumed by this process.
///
/// Exposed as `process_cpu_seconds_total`. Linux reads `/proc/self/stat`;
/// Windows reads `GetProcessTimes`; macOS reads `CLOCK_PROCESS_CPUTIME_ID`.
/// The latter two APIs are accessed through the safe `cpu-time` wrapper.
/// Returns `None` on unsupported platforms or when the platform query fails.
/// A [`Duration`] is returned rather than seconds so each platform's native
/// resolution survives exactly.
#[must_use]
pub fn process_cpu_time() -> Option<Duration> {
    platform_cpu_time()
}

#[cfg(target_os = "linux")]
fn platform_resident_memory_bytes() -> Option<u64> {
    read_proc("/proc/self/status")
        .as_deref()
        .and_then(parse_vm_rss_kib)
        .map(|kib| kib * 1024)
}

#[cfg(any(windows, target_os = "macos"))]
fn platform_resident_memory_bytes() -> Option<u64> {
    memory_stats::memory_stats().and_then(|stats| resident_memory_bytes(stats.physical_mem))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_resident_memory_bytes() -> Option<u64> {
    None
}

#[cfg(any(windows, target_os = "macos", test))]
fn resident_memory_bytes(physical_memory_bytes: usize) -> Option<u64> {
    u64::try_from(physical_memory_bytes).ok()
}

#[cfg(target_os = "linux")]
fn platform_cpu_time() -> Option<Duration> {
    process_cpu_ticks().map(|ticks| Duration::from_millis(ticks * (1_000 / PROC_USER_HZ)))
}

#[cfg(any(windows, target_os = "macos"))]
fn platform_cpu_time() -> Option<Duration> {
    cpu_time::ProcessTime::try_now()
        .ok()
        .map(|time| time.as_duration())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_cpu_time() -> Option<Duration> {
    None
}

#[cfg(target_os = "linux")]
fn process_cpu_ticks() -> Option<u64> {
    read_proc("/proc/self/stat")
        .as_deref()
        .and_then(parse_stat_cpu_ticks)
}

#[cfg(target_os = "linux")]
fn read_proc(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Reads `VmRSS` out of `/proc/self/status`, which reports it in kibibytes and
/// so needs no page size lookup.
#[cfg(any(target_os = "linux", test))]
fn parse_vm_rss_kib(status: &str) -> Option<u64> {
    let line = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))?
        .strip_prefix("VmRSS:")?;
    let mut fields = line.split_whitespace();
    let value: u64 = fields.next()?.parse().ok()?;
    match fields.next() {
        Some("kB") | None => Some(value),
        Some(_) => None,
    }
}

/// Sums `utime` and `stime` out of `/proc/self/stat`.
///
/// The second field is the executable name in parentheses and may itself
/// contain spaces and parentheses, so the fixed-position fields are counted
/// from the final `)` rather than from the start of the line.
#[cfg(any(target_os = "linux", test))]
fn parse_stat_cpu_ticks(stat: &str) -> Option<u64> {
    let tail = &stat[stat.rfind(')')? + 1..];
    let mut fields = tail.split_whitespace();
    // `tail` starts at field 3 (`state`), so `utime` and `stime` are fields 14
    // and 15, which is 11 and 12 positions along.
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    utime.checked_add(stime)
}

fn write_process_metrics(output: &mut String) {
    if let Some(bytes) = process_resident_memory_bytes() {
        write_gauge(
            output,
            "process_resident_memory_bytes",
            "Resident set size of this process in bytes",
            bytes,
        );
    }
    if let Some(cpu) = process_cpu_time() {
        write_family_header(
            output,
            "process_cpu_seconds_total",
            "Total user and system CPU time of this process in seconds",
            "counter",
        );
        // Rendered from the integer nanosecond count so the value is exact.
        let _ = writeln!(output, "process_cpu_seconds_total {}", format_seconds(cpu));
    }
}

/// Renders a [`Duration`] as decimal seconds without a float round-trip.
fn format_seconds(duration: Duration) -> String {
    let nanos = duration.subsec_nanos();
    if nanos == 0 {
        return duration.as_secs().to_string();
    }
    let mut rendered = format!("{}.{nanos:09}", duration.as_secs());
    while rendered.ends_with('0') {
        rendered.pop();
    }
    rendered
}

fn sanitized_buckets(buckets: impl IntoIterator<Item = f64>) -> Vec<f64> {
    let mut bounds = buckets
        .into_iter()
        .filter(|bound| bound.is_finite())
        .collect::<Vec<_>>();
    bounds.sort_by(f64::total_cmp);
    bounds.dedup_by(|left, right| left.total_cmp(right).is_eq());
    if bounds.is_empty() {
        bounds.extend_from_slice(&DEFAULT_DURATION_BUCKETS_SECONDS);
    }
    bounds
}

fn http_labels(method: HttpMethod, route: &str, status: &str) -> LabelSet {
    vec![
        ("method".to_owned(), method.as_str().to_owned()),
        ("route".to_owned(), route.to_owned()),
        ("status".to_owned(), status.to_owned()),
    ]
}

const fn status_class(status: u16) -> &'static str {
    match status / 100 {
        1 => "1xx",
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        _ => "5xx",
    }
}

fn label_set(labels: &[(&str, &str)]) -> Result<LabelSet, MetricError> {
    let mut set = labels
        .iter()
        .map(|(name, value)| {
            if valid_label_name(name) {
                Ok(((*name).to_owned(), (*value).to_owned()))
            } else {
                Err(MetricError::InvalidName((*name).to_owned()))
            }
        })
        .collect::<Result<LabelSet, MetricError>>()?;
    set.sort_by(|left, right| left.0.cmp(&right.0));
    if let Some(duplicate) = set
        .windows(2)
        .find(|pair| pair[0].0 == pair[1].0)
        .map(|pair| pair[0].0.clone())
    {
        return Err(MetricError::InvalidName(duplicate));
    }
    Ok(set)
}

fn note_overflow(dropped: &AtomicU64, warned: &mut bool, name: &str) {
    dropped.fetch_add(1, Ordering::Relaxed);
    if !*warned {
        *warned = true;
        tracing::warn!(
            target: "blazingly::metrics",
            metric = name,
            cap = MAX_LABEL_SETS_PER_METRIC,
            "metric label cardinality cap reached, dropping new label sets"
        );
    }
}

fn valid_metric_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || matches!(byte, b'_' | b':'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':'))
}

fn valid_label_name(name: &str) -> bool {
    if name == "le" || name.starts_with("__") {
        return false;
    }
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn write_counter(output: &mut String, name: &str, help: &str, value: u64) {
    write_family_header(output, name, help, "counter");
    let _ = writeln!(output, "{name} {value}");
}

fn write_gauge(output: &mut String, name: &str, help: &str, value: u64) {
    write_family_header(output, name, help, "gauge");
    let _ = writeln!(output, "{name} {value}");
}

fn write_labeled_counter(
    output: &mut String,
    name: &str,
    help: &str,
    series: &BTreeMap<LabelSet, u64>,
) {
    write_family_header(output, name, help, "counter");
    for (labels, value) in series {
        let _ = write!(output, "{name}");
        write_label_set(output, labels, None);
        let _ = writeln!(output, " {value}");
    }
}

fn write_labeled_gauge(
    output: &mut String,
    name: &str,
    help: &str,
    series: &BTreeMap<LabelSet, f64>,
) {
    write_family_header(output, name, help, "gauge");
    for (labels, value) in series {
        let _ = write!(output, "{name}");
        write_label_set(output, labels, None);
        let _ = writeln!(output, " {}", format_float(*value));
    }
}

fn write_histogram(
    output: &mut String,
    name: &str,
    help: &str,
    bounds: &[f64],
    series: &BTreeMap<LabelSet, HistogramSeries>,
) {
    write_family_header(output, name, help, "histogram");
    for (labels, data) in series {
        let mut cumulative = 0;
        for (index, bound) in bounds.iter().enumerate() {
            cumulative += data.counts[index];
            let _ = write!(output, "{name}_bucket");
            write_label_set(output, labels, Some(("le", &bound.to_string())));
            let _ = writeln!(output, " {cumulative}");
        }
        cumulative += data.counts[bounds.len()];
        let _ = write!(output, "{name}_bucket");
        write_label_set(output, labels, Some(("le", "+Inf")));
        let _ = writeln!(output, " {cumulative}");
        let _ = write!(output, "{name}_sum");
        write_label_set(output, labels, None);
        let _ = writeln!(output, " {}", format_float(data.sum));
        let _ = write!(output, "{name}_count");
        write_label_set(output, labels, None);
        let _ = writeln!(output, " {}", data.count);
    }
}

fn write_family_header(output: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(output, "# HELP {name} {}", help_escape(help));
    let _ = writeln!(output, "# TYPE {name} {kind}");
}

fn write_label_set(output: &mut String, labels: &[(String, String)], extra: Option<(&str, &str)>) {
    if labels.is_empty() && extra.is_none() {
        return;
    }
    output.push('{');
    for (index, (name, value)) in labels.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        let _ = write!(output, "{name}=\"{}\"", prometheus_escape(value));
    }
    if let Some((name, value)) = extra {
        if !labels.is_empty() {
            output.push(',');
        }
        let _ = write!(output, "{name}=\"{}\"", prometheus_escape(value));
    }
    output.push('}');
}

/// Renders a sample value in the spellings Prometheus accepts, which are not
/// the ones `f64::to_string` produces for the non-finite cases.
fn format_float(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value.is_sign_positive() {
            "+Inf".to_owned()
        } else {
            "-Inf".to_owned()
        }
    } else {
        value.to_string()
    }
}

fn prometheus_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn help_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::net::Ipv4Addr;

    const SAMPLE_TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn traceparent_parser_is_strict() {
        let parsed = parse_traceparent(SAMPLE_TRACEPARENT).expect("valid traceparent");
        assert_eq!(parsed.0, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(parsed.1, "00f067aa0ba902b7");
        assert!(parsed.2);
        assert!(parse_traceparent("00-00000000000000000000000000000000-a-b").is_none());
    }

    #[test]
    fn identifiers_are_random_and_never_zero() {
        let first = build_trace_context(None, None);
        let second = build_trace_context(None, None);
        assert_ne!(first.trace_id(), second.trace_id());
        assert_ne!(first.span_id(), second.span_id());
        assert_eq!(first.trace_id().len(), 32);
        assert_eq!(first.span_id().len(), 16);
        assert!(
            first
                .trace_id()
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        );
        assert!(first.span_id().bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(first.trace_id().bytes().any(|byte| byte != b'0'));
        assert!(first.span_id().bytes().any(|byte| byte != b'0'));

        let child = build_trace_context(Some(SAMPLE_TRACEPARENT), None);
        assert_eq!(child.trace_id(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(child.parent_span_id(), Some("00f067aa0ba902b7"));
        assert_ne!(child.span_id(), "00f067aa0ba902b7");
    }

    #[test]
    fn request_ids_are_not_trusted_by_default() {
        let observer = Observability::default();
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let minted = observer.resolve_request_id(Some("browser-42"), Some(peer));
        assert_ne!(minted.as_str(), "browser-42");
        assert_eq!(minted.as_str().len(), 36);
    }

    #[test]
    fn trusted_peers_may_supply_a_request_id() {
        let proxy = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let other = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9));
        let observer = Observability::new(
            ObservabilityConfig::default().trust_incoming_request_id_from([proxy]),
        );
        assert_eq!(
            observer
                .resolve_request_id(Some("browser-42"), Some(proxy))
                .as_str(),
            "browser-42"
        );
        assert_ne!(
            observer
                .resolve_request_id(Some("browser-42"), Some(other))
                .as_str(),
            "browser-42"
        );
        assert_ne!(
            observer
                .resolve_request_id(Some("not valid!"), Some(proxy))
                .as_str(),
            "not valid!"
        );

        let open = Observability::new(
            ObservabilityConfig::default().trust_incoming_request_id_from_any_peer(),
        );
        assert_eq!(
            open.resolve_request_id(Some("browser-42"), None).as_str(),
            "browser-42"
        );
    }

    #[test]
    fn tracestate_is_parsed_and_bounded() {
        let entries = parse_tracestate("congo=t61rcWkgMzE, rojo=00f067aa0ba902b7,BAD=x,congo=dup");
        assert_eq!(
            entries,
            vec![
                ("congo".to_owned(), "t61rcWkgMzE".to_owned()),
                ("rojo".to_owned(), "00f067aa0ba902b7".to_owned()),
            ]
        );
        let many = (0..40)
            .map(|index| format!("vendor{index}=value"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(parse_tracestate(&many).len(), MAX_TRACESTATE_ENTRIES);

        let trace = build_trace_context(Some(SAMPLE_TRACEPARENT), Some("congo=t61rcWkgMzE"));
        assert_eq!(trace.tracestate().as_deref(), Some("congo=t61rcWkgMzE"));
        assert_eq!(trace.tracestate_entries().len(), 1);

        let orphan = build_trace_context(None, Some("congo=t61rcWkgMzE"));
        assert!(orphan.tracestate().is_none());
    }

    #[test]
    fn duration_histogram_carries_method_route_and_status_class() {
        let metrics = Metrics::new();
        metrics.start();
        metrics.finish(
            HttpMethod::Get,
            Some("/items/{id}"),
            200,
            Duration::from_millis(7),
            true,
        );
        let text = metrics.prometheus();
        assert!(text.contains(
            "blazingly_http_request_duration_seconds_bucket{method=\"GET\",route=\"/items/{id}\",status=\"2xx\",le=\"0.01\"} 1"
        ));
        assert!(text.contains(
            "blazingly_http_request_duration_seconds_count{method=\"GET\",route=\"/items/{id}\",status=\"2xx\"} 1"
        ));
        assert!(text.contains(
            "blazingly_http_route_responses_total{method=\"GET\",route=\"/items/{id}\",status=\"200\"} 1"
        ));
        assert_valid_exposition(&text);
    }

    #[test]
    fn coarse_metrics_keep_a_low_cardinality_label_set() {
        let metrics = Metrics::new();
        metrics.start();
        metrics.finish(
            HttpMethod::Post,
            Some("/items/{id}"),
            503,
            Duration::from_millis(1),
            false,
        );
        let text = metrics.prometheus();
        assert!(text.contains(
            "blazingly_http_request_duration_seconds_count{method=\"POST\",route=\"<all>\",status=\"5xx\"} 1"
        ));
        assert!(!text.contains("/items/{id}"));
        assert_valid_exposition(&text);
    }

    #[test]
    fn duration_buckets_are_configurable() {
        let metrics = Metrics::with_duration_buckets([0.5, 0.25, 0.25, f64::NAN]);
        metrics.start();
        metrics.finish(
            HttpMethod::Get,
            None,
            200,
            Duration::from_millis(300),
            false,
        );
        let text = metrics.prometheus();
        assert!(text.contains("le=\"0.25\"} 0"));
        assert!(text.contains("le=\"0.5\"} 1"));
        assert!(text.contains("le=\"+Inf\"} 1"));
        assert!(!text.contains("le=\"5\""));
        assert_valid_exposition(&text);
    }

    #[test]
    fn metrics_endpoint_is_excluded_from_its_own_accounting() {
        let observer = Observability::default();
        assert!(observer.is_metrics_request(HttpMethod::Get, "/metrics"));
        assert!(observer.is_metrics_request(HttpMethod::Get, "/metrics?format=text"));
        assert!(!observer.is_metrics_request(HttpMethod::Post, "/metrics"));
        assert!(!observer.is_metrics_request(HttpMethod::Get, "/metricsx"));

        let metrics = observer.metrics();
        metrics.start();
        metrics.finish(HttpMethod::Get, Some("/items"), 200, Duration::ZERO, false);
        let first = metrics.prometheus();
        let second = metrics.prometheus();
        // Only the application-owned families must be identical across scrapes.
        // The `process_*` families read live resident set size and CPU time, so
        // they legitimately differ between two consecutive renders.
        assert_eq!(
            application_families(&first),
            application_families(&second),
            "scraping the metrics endpoint must not change the app's own series"
        );
        assert_eq!(metrics.requests_total(), 1);
        assert_eq!(metrics.in_flight(), 0);
        assert!(second.contains("blazingly_http_requests_total 1"));
    }

    /// Keeps the `blazingly_*` families and drops the live process gauges.
    fn application_families(exposition: &str) -> String {
        exposition
            .lines()
            .filter(|line| !line.contains("process_resident_memory_bytes"))
            .filter(|line| !line.contains("process_cpu_seconds_total"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn application_metrics_are_registered_and_recorded() {
        let metrics = Metrics::new();
        metrics
            .register_counter("orders_total", "Orders accepted")
            .expect("counter registers");
        assert_eq!(
            metrics.register_counter("orders_total", "Orders accepted"),
            Err(MetricError::AlreadyRegistered("orders_total".to_owned()))
        );
        assert_eq!(
            metrics.register_counter("blazingly_orders_total", "reserved"),
            Err(MetricError::InvalidName(
                "blazingly_orders_total".to_owned()
            ))
        );
        metrics
            .register_histogram("order_value", "Order value", [1.0, 10.0])
            .expect("histogram registers");

        metrics
            .increment_counter("orders_total", &[("tier", "gold")])
            .expect("counter increments");
        metrics
            .add_to_counter("orders_total", &[("tier", "gold")], 4)
            .expect("counter adds");
        metrics
            .observe_histogram("order_value", &[("tier", "gold")], 5.0)
            .expect("histogram observes");

        assert_eq!(
            metrics.counter_value("orders_total", &[("tier", "gold")]),
            Some(5)
        );
        assert_eq!(
            metrics.increment_counter("missing_total", &[]),
            Err(MetricError::NotRegistered("missing_total".to_owned()))
        );
        assert_eq!(
            metrics.observe_histogram("orders_total", &[], 1.0),
            Err(MetricError::TypeMismatch("orders_total".to_owned()))
        );
        assert_eq!(
            metrics.increment_counter("orders_total", &[("le", "1")]),
            Err(MetricError::InvalidName("le".to_owned()))
        );

        let text = metrics.prometheus();
        assert!(text.contains("# TYPE orders_total counter"));
        assert!(text.contains("orders_total{tier=\"gold\"} 5"));
        assert!(text.contains("order_value_bucket{tier=\"gold\",le=\"10\"} 1"));
        assert!(text.contains("order_value_sum{tier=\"gold\"} 5"));
        assert_valid_exposition(&text);
    }

    #[test]
    fn application_label_cardinality_is_capped() {
        let metrics = Metrics::new();
        metrics
            .register_counter("visits_total", "Visits")
            .expect("counter registers");
        for index in 0..(MAX_LABEL_SETS_PER_METRIC + 20) {
            let value = index.to_string();
            metrics
                .increment_counter("visits_total", &[("user", value.as_str())])
                .expect("counter increments");
        }
        assert_eq!(metrics.dropped_label_sets_total(), 20);
        assert_eq!(
            metrics.counter_value("visits_total", &[("user", "0")]),
            Some(1)
        );
        assert_eq!(
            metrics.counter_value(
                "visits_total",
                &[("user", MAX_LABEL_SETS_PER_METRIC.to_string().as_str())]
            ),
            None
        );
        assert_valid_exposition(&metrics.prometheus());
    }

    #[test]
    fn http_label_cardinality_folds_into_an_overflow_series() {
        let metrics = Metrics::new();
        for index in 0..(MAX_LABEL_SETS_PER_METRIC + 5) {
            let route = format!("/route/{index}");
            metrics.start();
            metrics.finish(
                HttpMethod::Get,
                Some(route.as_str()),
                200,
                Duration::from_millis(1),
                true,
            );
        }
        assert!(metrics.dropped_label_sets_total() > 0);
        let text = metrics.prometheus();
        assert!(text.contains("route=\"<overflow>\""));
        assert_valid_exposition(&text);
    }

    #[test]
    fn exposition_escapes_label_values() {
        let metrics = Metrics::new();
        metrics.start();
        metrics.finish(
            HttpMethod::Get,
            Some("/quote\"back\\slash"),
            200,
            Duration::from_millis(1),
            true,
        );
        let text = metrics.prometheus();
        assert!(text.contains("route=\"/quote\\\"back\\\\slash\""));
        assert_valid_exposition(&text);
    }

    #[test]
    fn gauges_are_registered_set_and_exposed() {
        let metrics = Metrics::new();
        metrics
            .register_gauge("pool_connections", "Live pool connections")
            .expect("gauge registers");
        metrics
            .set_gauge("pool_connections", &[("pool", "primary")], 7.0)
            .expect("gauge is set");
        metrics
            .set_gauge("pool_connections", &[("pool", "primary")], 4.0)
            .expect("gauge is overwritten, not accumulated");
        assert_eq!(
            metrics.gauge_value("pool_connections", &[("pool", "primary")]),
            Some(4.0)
        );

        assert_eq!(
            metrics.gauge_value("pool_connections", &[("pool", "replica")]),
            None
        );
        assert_eq!(
            metrics.set_gauge("missing", &[], 1.0),
            Err(MetricError::NotRegistered("missing".to_owned()))
        );
        metrics
            .register_counter("jobs_total", "Jobs")
            .expect("counter registers");
        assert_eq!(
            metrics.set_gauge("jobs_total", &[], 1.0),
            Err(MetricError::TypeMismatch("jobs_total".to_owned()))
        );
        assert_eq!(metrics.gauge_value("jobs_total", &[]), None);
        assert_eq!(metrics.counter_value("pool_connections", &[]), None);

        let text = metrics.prometheus();
        assert!(text.contains("# TYPE pool_connections gauge"));
        assert!(text.contains("pool_connections{pool=\"primary\"} 4"));
        assert_valid_exposition(&text);
    }

    #[test]
    fn non_finite_sample_values_use_prometheus_spellings() {
        assert_eq!(format_float(1.5), "1.5");
        assert_eq!(format_float(f64::INFINITY), "+Inf");
        assert_eq!(format_float(f64::NEG_INFINITY), "-Inf");
        assert_eq!(format_float(f64::NAN), "NaN");

        let metrics = Metrics::new();
        metrics
            .register_gauge("saturation_ratio", "Saturation")
            .expect("gauge registers");
        metrics
            .set_gauge("saturation_ratio", &[], f64::INFINITY)
            .expect("gauge is set");
        let text = metrics.prometheus();
        assert!(text.contains("saturation_ratio +Inf"));
        // `f64::to_string` would have written `inf`, which Prometheus rejects.
        assert!(!text.contains(" inf"));
        assert_valid_exposition(&text);
    }

    #[test]
    fn background_queue_depth_is_sampled_at_scrape_time() {
        let metrics = Metrics::new();
        assert!(!metrics.prometheus().contains(QUEUE_DEPTH_METRIC));

        let depth = Arc::new(AtomicU64::new(3));
        let observed = Arc::clone(&depth);
        metrics
            .track_queue_depth("export", move || observed.load(Ordering::Relaxed))
            .expect("queue registers");
        assert_eq!(metrics.queue_depth("export"), Some(3));
        assert!(
            metrics
                .prometheus()
                .contains("blazingly_background_queue_depth{queue=\"export\"} 3")
        );

        // The closure is re-read on every scrape rather than snapshotted once.
        depth.store(11, Ordering::Relaxed);
        let text = metrics.prometheus();
        assert!(text.contains("blazingly_background_queue_depth{queue=\"export\"} 11"));
        assert_eq!(metrics.queue_depth("export"), Some(11));
        assert_eq!(metrics.queue_depth("absent"), None);
        assert_valid_exposition(&text);
    }

    #[test]
    fn queue_registration_rejects_bad_names_duplicates_and_overflow() {
        let metrics = Metrics::new();
        assert_eq!(
            metrics.track_queue_depth("", || 0),
            Err(MetricError::InvalidName(String::new()))
        );
        let long = "q".repeat(MAX_QUEUE_NAME_BYTES + 1);
        assert_eq!(
            metrics.track_queue_depth(&long, || 0),
            Err(MetricError::InvalidName(long.clone()))
        );

        metrics.track_queue_depth("jobs", || 1).expect("registers");
        assert_eq!(
            metrics.track_queue_depth("jobs", || 2),
            Err(MetricError::AlreadyRegistered("jobs".to_owned()))
        );

        for index in 1..MAX_LABEL_SETS_PER_METRIC {
            metrics
                .track_queue_depth(&format!("queue-{index}"), || 0)
                .expect("registers below the cap");
        }
        assert_eq!(
            metrics.track_queue_depth("one-too-many", || 0),
            Err(MetricError::CapacityExceeded("one-too-many".to_owned()))
        );
    }

    #[test]
    fn built_in_family_names_are_closed_to_applications() {
        let metrics = Metrics::new();
        for reserved in RESERVED_METRIC_NAMES {
            assert_eq!(
                metrics.register_gauge(reserved, "shadowing a built-in"),
                Err(MetricError::InvalidName(reserved.to_owned())),
                "{reserved} must not be registrable twice in one scrape"
            );
        }
        assert_eq!(
            metrics.register_gauge("blazingly_background_queue_depth", "reserved prefix"),
            Err(MetricError::InvalidName(
                "blazingly_background_queue_depth".to_owned()
            ))
        );
    }

    #[test]
    fn procfs_process_metrics_are_parsed_from_their_fixed_layout() {
        let status = "Name:\tserver\nVmPeak:\t  212345 kB\nVmRSS:\t   14528 kB\nThreads:\t4\n";
        assert_eq!(parse_vm_rss_kib(status), Some(14_528));
        assert_eq!(parse_vm_rss_kib("Name:\tserver\nThreads:\t4\n"), None);
        assert_eq!(parse_vm_rss_kib("VmRSS:\t 14528 MB\n"), None);

        // Field 2 is the executable name and may contain spaces and parens, so
        // the offsets are counted from the last `)`.
        let stat = "42 (odd ) name) S 1 42 42 0 -1 4194304 900 0 0 0 731 219 0 0 20 0 8 0 99";
        assert_eq!(parse_stat_cpu_ticks(stat), Some(950));
        assert_eq!(parse_stat_cpu_ticks("42 (server) S 1 2 3"), None);
        assert_eq!(parse_stat_cpu_ticks("no parenthesis here"), None);

        // Both readers agree on availability: procfs answers on Linux, while
        // the safe process-information wrappers answer on Windows and macOS.
        let supported = cfg!(any(target_os = "linux", target_os = "macos", windows));
        assert_eq!(process_resident_memory_bytes().is_some(), supported);
        assert_eq!(process_cpu_time().is_some(), supported);
        if cfg!(target_os = "linux")
            && let Some(cpu) = process_cpu_time()
        {
            assert_eq!(cpu.subsec_millis() % 10, 0, "procfs resolution is 10ms");
        }
        assert_valid_exposition(&Metrics::new().prometheus());
    }

    #[test]
    fn resident_memory_byte_conversion_is_checked_and_portable() {
        assert_eq!(resident_memory_bytes(0), Some(0));
        assert_eq!(resident_memory_bytes(14_528), Some(14_528));
        assert_eq!(
            resident_memory_bytes(usize::MAX),
            u64::try_from(usize::MAX).ok()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_process_metrics_are_live_and_plausible() {
        let rss = process_resident_memory_bytes().expect("GetProcessMemoryInfo answers");
        assert!(rss > 1 << 20, "test process RSS at or under 1 MiB: {rss}");
        assert!(rss < 1 << 40, "test process RSS at or over 1 TiB: {rss}");

        // GetProcessTimes ticks in scheduler quanta, so burn cycles until it
        // moves off zero rather than asserting against a freshly started
        // process.
        let mut spin = 0_u64;
        while process_cpu_time() == Some(Duration::ZERO) {
            spin = spin.wrapping_add(1);
            std::hint::black_box(spin);
        }
        let cpu = process_cpu_time().expect("GetProcessTimes answers");
        assert!(cpu > Duration::ZERO);

        let text = Metrics::new().prometheus();
        assert!(text.contains("process_resident_memory_bytes "));
        assert!(text.contains("process_cpu_seconds_total "));
        assert_valid_exposition(&text);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_process_metrics_are_live_and_plausible() {
        let rss = process_resident_memory_bytes().expect("Mach task_info answers");
        assert!(rss > 1 << 20, "test process RSS at or under 1 MiB: {rss}");
        assert!(rss < 1 << 40, "test process RSS at or over 1 TiB: {rss}");

        let before = process_cpu_time().expect("CLOCK_PROCESS_CPUTIME_ID answers");
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut after = before;
        let mut spin = 0_u64;
        while after <= before && std::time::Instant::now() < deadline {
            for _ in 0..10_000 {
                spin = spin.wrapping_add(1);
                std::hint::black_box(spin);
            }
            after = process_cpu_time().expect("CLOCK_PROCESS_CPUTIME_ID keeps answering");
        }
        assert!(after > before, "process CPU time did not advance");

        let text = Metrics::new().prometheus();
        assert!(text.contains("process_resident_memory_bytes "));
        assert!(text.contains("process_cpu_seconds_total "));
        assert_valid_exposition(&text);
    }

    #[test]
    fn cpu_seconds_render_exactly_from_integer_nanoseconds() {
        assert_eq!(format_seconds(Duration::from_secs(9)), "9");
        assert_eq!(format_seconds(Duration::from_millis(7_310)), "7.31");
        assert_eq!(format_seconds(Duration::new(1, 100)), "1.0000001");
    }

    #[test]
    fn exposition_is_valid_for_a_populated_registry() {
        let metrics = Metrics::with_duration_buckets([0.01, 0.1]);
        metrics
            .register_counter("orders_total", "Orders accepted")
            .expect("counter registers");
        metrics
            .register_histogram("order_value", "Order \\ value", [1.0, 2.0])
            .expect("histogram registers");
        metrics
            .increment_counter("orders_total", &[("tier", "go\"ld"), ("region", "eu")])
            .expect("counter increments");
        metrics
            .observe_histogram("order_value", &[("tier", "gold")], 1.5)
            .expect("histogram observes");
        for status in [200_u16, 404, 500] {
            metrics.start();
            metrics.finish(
                HttpMethod::Get,
                Some("/items/{id}"),
                status,
                Duration::from_millis(20),
                true,
            );
        }
        assert_valid_exposition(&metrics.prometheus());
    }

    fn assert_valid_exposition(text: &str) {
        let mut helps: BTreeMap<String, usize> = BTreeMap::new();
        let mut types: BTreeMap<String, usize> = BTreeMap::new();
        let mut kinds: BTreeMap<String, String> = BTreeMap::new();
        let mut buckets: BTreeSet<(String, String)> = BTreeSet::new();
        let mut infinity: BTreeSet<(String, String)> = BTreeSet::new();
        let mut sums: BTreeSet<(String, String)> = BTreeSet::new();
        let mut counts: BTreeSet<(String, String)> = BTreeSet::new();
        let mut samples: Vec<String> = Vec::new();

        for line in text.lines() {
            assert!(!line.is_empty(), "exposition must not contain blank lines");
            if let Some(rest) = line.strip_prefix("# HELP ") {
                let (name, help) = rest.split_once(' ').expect("HELP carries text");
                assert!(!help.is_empty(), "HELP text for {name} is empty");
                assert!(!help.contains('\n'), "HELP text for {name} has a newline");
                *helps.entry(name.to_owned()).or_default() += 1;
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (name, kind) = rest.split_once(' ').expect("TYPE carries a kind");
                assert!(
                    matches!(kind, "counter" | "gauge" | "histogram"),
                    "unexpected metric type {kind}"
                );
                *types.entry(name.to_owned()).or_default() += 1;
                kinds.insert(name.to_owned(), kind.to_owned());
            } else {
                assert!(!line.starts_with('#'), "unexpected comment line: {line}");
                let (series, value) = line.rsplit_once(' ').expect("sample carries a value");
                value.parse::<f64>().expect("sample value is numeric");
                samples.push(series.to_owned());
            }
        }

        for (name, count) in &helps {
            assert_eq!(*count, 1, "{name} declares HELP more than once");
        }
        for (name, count) in &types {
            assert_eq!(*count, 1, "{name} declares TYPE more than once");
        }
        assert_eq!(
            helps.keys().collect::<Vec<_>>(),
            types.keys().collect::<Vec<_>>(),
            "every family needs one HELP and one TYPE"
        );

        for series in &samples {
            let (name, labels) = split_series(series);
            assert_valid_labels(&labels);
            if let Some(kind) = kinds.get(&name) {
                assert_ne!(kind, "histogram", "{name} is a bare histogram sample");
                continue;
            }
            let (family, suffix) = split_suffix(&name);
            assert_eq!(
                kinds.get(&family).map(String::as_str),
                Some("histogram"),
                "{name} belongs to no declared family"
            );
            match suffix {
                "_bucket" => {
                    let (base, le) = split_le(&labels);
                    buckets.insert((family.clone(), base.clone()));
                    if le == "+Inf" {
                        infinity.insert((family, base));
                    }
                }
                "_sum" => {
                    sums.insert((family, labels));
                }
                _ => {
                    counts.insert((family, labels));
                }
            }
        }

        for series in &buckets {
            assert!(infinity.contains(series), "{series:?} has no +Inf bucket");
            assert!(sums.contains(series), "{series:?} has no _sum");
            assert!(counts.contains(series), "{series:?} has no _count");
        }
    }

    fn split_series(series: &str) -> (String, String) {
        series.split_once('{').map_or_else(
            || (series.to_owned(), String::new()),
            |(name, labels)| {
                (
                    name.to_owned(),
                    labels
                        .strip_suffix('}')
                        .expect("label set is closed")
                        .to_owned(),
                )
            },
        )
    }

    fn split_suffix(name: &str) -> (String, &'static str) {
        for suffix in ["_bucket", "_sum", "_count"] {
            if let Some(family) = name.strip_suffix(suffix) {
                return (family.to_owned(), suffix);
            }
        }
        (name.to_owned(), "")
    }

    fn split_le(labels: &str) -> (String, String) {
        let position = labels.rfind("le=\"").expect("bucket carries an le label");
        let value = labels[position + 4..]
            .strip_suffix('"')
            .expect("le value is quoted");
        (
            labels[..position].trim_end_matches(',').to_owned(),
            value.to_owned(),
        )
    }

    fn assert_valid_labels(labels: &str) {
        let mut names: Vec<&str> = Vec::new();
        let mut rest = labels;
        while !rest.is_empty() {
            let (name, tail) = rest.split_once('=').expect("label name");
            assert!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
                "invalid label name {name}"
            );
            let tail = tail.strip_prefix('"').expect("label value is quoted");
            let bytes = tail.as_bytes();
            let mut index = 0;
            let end = loop {
                assert!(index < bytes.len(), "label value is not terminated");
                match bytes[index] {
                    b'\\' => {
                        assert!(
                            matches!(bytes.get(index + 1), Some(b'\\' | b'"' | b'n')),
                            "invalid escape in {labels}"
                        );
                        index += 2;
                    }
                    b'"' => break index,
                    byte => {
                        assert_ne!(byte, b'\n', "raw newline in {labels}");
                        index += 1;
                    }
                }
            };
            names.push(name);
            let remainder = &tail[end + 1..];
            rest = remainder.strip_prefix(',').unwrap_or(remainder);
        }
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            names.len(),
            "duplicate label name in {labels}"
        );
    }
}
