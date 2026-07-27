#![cfg(feature = "observability")]

use blazingly::prelude::*;
use futures_lite::future;
use std::sync::{Arc, Mutex};

#[get("/observed", id = "observability.observed")]
async fn observed(
    Extension(request_id): Extension<RequestId>,
    Extension(trace): Extension<TraceContext>,
) -> Json<String> {
    Json(format!("{request_id}:{}", trace.trace_id()))
}

#[derive(Clone, Default)]
struct CapturingSink(Arc<Mutex<Vec<AccessEvent>>>);

impl AccessLogSink for CapturingSink {
    fn emit(&self, event: &AccessEvent) {
        self.0
            .lock()
            .expect("capture mutex should not be poisoned")
            .push(event.clone());
    }
}

#[test]
fn request_identity_trace_access_events_and_prometheus_share_one_context() {
    let executable =
        ExecutableApp::new(routes![observed]).expect("observability route should compile");
    let sink = CapturingSink::default();
    // An inbound request id is untrusted by default; this case asserts the
    // propagating behaviour, so it opts in explicitly.
    let observer = Observability::new(
        ObservabilityConfig {
            detailed_route_metrics: true,
            ..ObservabilityConfig::default()
        }
        .trust_incoming_request_id_from_any_peer(),
    )
    .with_access_sink(sink.clone());
    let metrics = observer.metrics();
    let app = TestApp::new(&executable).with_middleware(observer);
    let incoming_trace = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    let response = future::block_on(
        app.call(
            Request::get("/observed")
                .header("x-request-id", "browser-42")
                .header("traceparent", incoming_trace),
        ),
    );
    assert_eq!(response.status(), 200);
    assert_eq!(response.get_header("x-request-id"), Some("browser-42"));
    let outgoing_trace = response
        .get_header("traceparent")
        .expect("traceparent response header");
    assert!(outgoing_trace.starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-"));
    assert!(!outgoing_trace.contains("00f067aa0ba902b7-01"));
    assert!(
        response
            .json::<String>()
            .expect("response JSON")
            .starts_with("browser-42:4bf92f3577b34da6a3ce929d0e0e4736")
    );

    let not_found = future::block_on(app.call(Request::get("/missing")));
    assert_eq!(not_found.status(), 404);
    assert_eq!(metrics.requests_total(), 2);
    assert_eq!(metrics.errors_total(), 1);
    assert_eq!(metrics.in_flight(), 0);

    let prometheus = future::block_on(app.call(Request::get("/metrics")));
    assert_eq!(prometheus.status(), 200);
    assert_eq!(
        prometheus.get_header("content-type"),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );
    let text = prometheus.text().expect("Prometheus output");
    // The scrape itself is excluded from the application's own counters, so the
    // exposition still reports the two real requests served above.
    assert!(text.contains("blazingly_http_requests_total 2"));
    assert!(text.contains("blazingly_http_errors_total 1"));
    assert!(text.contains(
        "blazingly_http_route_responses_total{method=\"GET\",route=\"/observed\",status=\"200\"} 1"
    ));

    let events = sink.0.lock().expect("capture mutex should not be poisoned");
    assert_eq!(events.len(), 3);
    assert_eq!(
        events[0].operation_id.as_deref(),
        Some("observability.observed")
    );
    assert_eq!(events[1].status, 404);
}
