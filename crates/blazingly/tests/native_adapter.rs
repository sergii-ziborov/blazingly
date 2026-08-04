#![cfg(feature = "native")]

use blazingly::{
    BodyStream, BodyStreamError, ExecutableApp, Json, StreamingBody, UploadBody, api_model, get,
    post, routes,
};
use futures_lite::future;
use futures_lite::io::Cursor;
#[cfg(feature = "native-http2")]
use futures_lite::io::{AsyncRead, AsyncWrite};
use std::io::{Read as _, Write as _};
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

#[get("/health", id = "health.read", summary = "Read health")]
#[allow(clippy::unused_async)]
async fn health() -> Json<&'static str> {
    Json("ok")
}

#[api_model]
#[derive(Clone, Debug)]
struct Echo {
    value: String,
}

#[api_model]
#[derive(Clone, Debug)]
struct UploadSummary {
    bytes: u64,
    chunks: u64,
}

static UPLOAD_CHUNKS_OBSERVED: AtomicUsize = AtomicUsize::new(0);

#[post(
    "/upload",
    id = "upload.stream",
    summary = "Consume a streaming upload"
)]
async fn upload(mut body: UploadBody) -> Json<UploadSummary> {
    let mut bytes = 0_u64;
    let mut chunks = 0_u64;
    while let Some(chunk) = body.next_chunk().await {
        let chunk = chunk.expect("native upload stream");
        bytes = bytes.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        chunks += 1;
        UPLOAD_CHUNKS_OBSERVED.fetch_add(1, Ordering::Release);
    }
    Json(UploadSummary { bytes, chunks })
}

#[post("/echo", id = "echo.create", summary = "Echo JSON")]
#[allow(clippy::unused_async)]
async fn echo(Json(input): Json<Echo>) -> Json<Echo> {
    Json(input)
}

#[get("/slow", id = "slow.read", summary = "Never finish without a timeout")]
async fn slow() -> Json<&'static str> {
    std::future::pending::<Json<&'static str>>().await
}

#[get("/stream", id = "stream.read", summary = "Stream raw bytes")]
#[allow(clippy::unused_async)]
async fn stream() -> StreamingBody {
    StreamingBody::from_chunks([b"blazing".to_vec(), b"ly".to_vec()])
}

static H2_FAST_RAN: AtomicBool = AtomicBool::new(false);
static H2_FAST_BODY_POLLED: AtomicBool = AtomicBool::new(false);

struct SlowH2Body(bool);

impl BodyStream for SlowH2Body {
    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Vec<u8>, BodyStreamError>>> {
        if self.0 {
            return Poll::Ready(None);
        }
        if H2_FAST_BODY_POLLED.load(Ordering::Acquire) {
            self.0 = true;
            return Poll::Ready(Some(Ok(b"slow-after-fast-body".to_vec())));
        }
        context.waker().wake_by_ref();
        Poll::Pending
    }
}

struct FastH2Body(bool);

impl BodyStream for FastH2Body {
    fn poll_next(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Vec<u8>, BodyStreamError>>> {
        if self.0 {
            return Poll::Ready(None);
        }
        self.0 = true;
        H2_FAST_BODY_POLLED.store(true, Ordering::Release);
        Poll::Ready(Some(Ok(b"fast-body".to_vec())))
    }
}

#[get("/h2-slow", id = "http2.slow", summary = "Wait for a sibling stream")]
async fn h2_slow() -> StreamingBody {
    for _ in 0..1_000 {
        if H2_FAST_RAN.load(Ordering::Acquire) {
            return StreamingBody::new(SlowH2Body(false));
        }
        future::yield_now().await;
    }
    StreamingBody::once("handler-blocked")
}

#[get("/h2-fast", id = "http2.fast", summary = "Release a sibling stream")]
#[allow(clippy::unused_async)]
async fn h2_fast() -> StreamingBody {
    H2_FAST_RAN.store(true, Ordering::Release);
    StreamingBody::new(FastH2Body(false))
}

fn server(max_body_bytes: usize) -> blazingly::native::Server {
    let app = ExecutableApp::new(routes![health, echo, stream, upload, h2_slow, h2_fast])
        .expect("operation graph should compile");
    blazingly::native::Server::new(app)
        .with_max_body_bytes(max_body_bytes)
        .with_openapi(blazingly::openapi::OpenApiConfig::new(
            "Native API",
            "1.0.0",
        ))
}

#[test]
fn native_http1_streams_with_chunked_backpressure_framing() {
    let response = exchange(
        b"GET /stream HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n",
        1024,
    );

    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response.contains("transfer-encoding: chunked\r\n"));
    assert!(response.ends_with("7\r\nblazing\r\n2\r\nly\r\n0\r\n\r\n"));
}

#[test]
fn native_http1_serves_precompiled_openapi_json() {
    let response = exchange(
        b"GET /openapi.json HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n",
        1024,
    );

    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response.contains("content-type: application/json\r\n"));
    assert!(response.contains("\"title\":\"Native API\""));
}

fn exchange(request: &[u8], max_body_bytes: usize) -> String {
    let request_bytes = request.len();
    let mut transport = Cursor::new(request.to_vec());
    future::block_on(server(max_body_bytes).serve_io(&mut transport))
        .expect("HTTP/1 exchange should succeed");
    let wire = transport.into_inner();
    String::from_utf8(wire[request_bytes..].to_vec()).expect("response should be UTF-8")
}

#[test]
fn native_http1_adapter_serves_keep_alive_requests_without_tokio() {
    let request = concat!(
        "GET /health HTTP/1.1\r\nhost: localhost\r\n\r\n",
        "GET /missing HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n",
    );
    let responses = exchange(request.as_bytes(), 1024);
    assert!(responses.starts_with("HTTP/1.1 200 OK\r\n"));
    assert_eq!(responses.matches("\r\ndate: ").count(), 2);
    assert!(responses.contains("\r\n\r\n\"ok\"HTTP/1.1 404 Not Found\r\n"));
    assert!(responses.contains("connection: close\r\n"));
}

#[test]
fn native_http1_adapter_dispatches_json_without_copying_the_request_view() {
    let body = r#"{"value":"yes"}"#;
    let request = format!(
        "POST /echo HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );

    let response = exchange(request.as_bytes(), 1024);

    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response.ends_with(r#"{"value":"yes"}"#));
}

#[test]
fn native_http1_adapter_rejects_oversized_and_decodes_chunked_requests() {
    let oversized = b"POST /echo HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: 5\r\nconnection: close\r\n\r\n";
    let oversized_response = exchange(oversized, 4);
    assert!(oversized_response.starts_with("HTTP/1.1 413 Payload Too Large\r\n"));
    assert!(oversized_response.contains("\r\ndate: "));

    let chunked = concat!(
        "POST /echo HTTP/1.1\r\n",
        "content-type: application/json\r\n",
        "transfer-encoding: chunked\r\n",
        "connection: close\r\n\r\n",
        "7\r\n{\"value\r\n",
        "8;extension=yes\r\n\":\"yes\"}\r\n",
        "0\r\nx-checksum: accepted\r\n\r\n",
    );
    let chunked_response = exchange(chunked.as_bytes(), 1024);
    assert!(chunked_response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(chunked_response.ends_with(r#"{"value":"yes"}"#));
}

/// A buffered JSON body larger than the connection read buffer must arrive.
///
/// Regression: the connection buffer was allocated once with
/// `Vec::with_capacity(READ_CHUNK_BYTES)` and handed to Compio's `append`,
/// which only ever fills a vector's spare capacity. Once the buffer was full
/// the next read returned zero bytes, the adapter read that as end of stream,
/// and every request with a body over roughly 8 KiB was answered
/// `400 incomplete_body`. That is the default path for every `Json<T>` handler,
/// so an ordinary bulk POST could not be received at all.
#[test]
fn native_http1_accepts_a_buffered_body_larger_than_the_read_buffer() {
    // Comfortably past one read buffer, and past two, so a single extra
    // reservation would not be enough to make this pass by accident.
    use std::net::{TcpListener, TcpStream};
    use std::time::Instant;

    // The defect lives in the plaintext socket path, which the in-memory
    // `exchange` helper does not exercise, so this drives a real connection.
    let probe = TcpListener::bind("127.0.0.1:0").expect("probe");
    let address = probe.local_addr().expect("address");
    drop(probe);
    let (shutdown, signal) = blazingly::native::shutdown_channel();
    let server_thread = std::thread::spawn(move || {
        server(1024 * 1024).serve_gracefully(address, signal, Duration::from_secs(2))
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut client = loop {
        match TcpStream::connect(address) {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => std::thread::yield_now(),
            Err(error) => panic!("server did not start: {error}"),
        }
    };
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");

    // Comfortably past one read buffer, and past two, so a single extra
    // reservation would not be enough to make this pass by accident.
    let filler = "x".repeat(40 * 1024);
    let body =
        blazingly_json::to_vec(&blazingly_json::json!({ "value": filler })).expect("request JSON");
    let head = format!(
        "POST /echo HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    client.write_all(head.as_bytes()).expect("request head");
    client.write_all(&body).expect("request body");

    let mut response = String::new();
    client.read_to_string(&mut response).expect("response");
    shutdown.shutdown();
    server_thread
        .join()
        .expect("server thread")
        .expect("graceful shutdown");

    assert!(
        response.starts_with("HTTP/1.1 200 "),
        "a {}-byte buffered body must be accepted, got: {}",
        body.len(),
        response.lines().next().unwrap_or_default()
    );
    assert!(response.contains(&filler), "echoed body should round-trip");
}

#[test]
fn native_http1_starts_streaming_handler_before_the_complete_upload_arrives() {
    use std::net::{TcpListener, TcpStream};
    use std::time::Instant;

    UPLOAD_CHUNKS_OBSERVED.store(0, Ordering::Release);
    let probe = TcpListener::bind("127.0.0.1:0").expect("probe");
    let address = probe.local_addr().expect("address");
    drop(probe);
    let (shutdown, signal) = blazingly::native::shutdown_channel();
    let server_thread = std::thread::spawn(move || {
        server(128 * 1024).serve_gracefully(address, signal, Duration::from_secs(2))
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut client = loop {
        match TcpStream::connect(address) {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => std::thread::yield_now(),
            Err(error) => panic!("server did not start: {error}"),
        }
    };
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");

    let total = 64 * 1024;
    let head = format!(
        "POST /upload HTTP/1.1\r\nhost: localhost\r\ncontent-length: {total}\r\nconnection: close\r\n\r\n"
    );
    client.write_all(head.as_bytes()).expect("request head");
    client.write_all(b"hello").expect("first upload bytes");
    let observed_deadline = Instant::now() + Duration::from_secs(2);
    while UPLOAD_CHUNKS_OBSERVED.load(Ordering::Acquire) == 0 {
        assert!(
            Instant::now() < observed_deadline,
            "handler did not observe a chunk before the complete body"
        );
        std::thread::yield_now();
    }
    client
        .write_all(&vec![b'x'; total - 5])
        .expect("remaining upload bytes");
    let mut response = String::new();
    client.read_to_string(&mut response).expect("response");
    shutdown.shutdown();
    server_thread
        .join()
        .expect("server thread")
        .expect("graceful server");

    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response.contains(&format!("\"bytes\":{total}")));
    assert!(response.contains("\"chunks\":"));
}

#[test]
fn multicore_launcher_builds_thread_local_apps_and_shuts_down_gracefully() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);

    let (shutdown, signal) = blazingly::native::shutdown_channel();
    let worker_apps = Arc::new(AtomicUsize::new(0));
    let server_worker_apps = Arc::clone(&worker_apps);
    let server_thread = std::thread::spawn(move || {
        blazingly::native::MulticoreServer::new(NonZeroUsize::new(2).unwrap(), move || {
            server_worker_apps.fetch_add(1, Ordering::Relaxed);
            ExecutableApp::new(routes![health, echo]).expect("worker app should compile")
        })
        .serve_gracefully(address, signal, Duration::from_secs(2))
        .expect("multicore server should stop cleanly");
    });

    for _ in 0..2 {
        let mut stream = (0..100)
            .find_map(|_| {
                if let Ok(stream) = std::net::TcpStream::connect(address) {
                    Some(stream)
                } else {
                    std::thread::sleep(Duration::from_millis(10));
                    None
                }
            })
            .expect("multicore listener should start");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .write_all(b"GET /health HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with("\"ok\""));
    }
    assert_eq!(worker_apps.load(Ordering::Relaxed), 2);

    shutdown.shutdown();
    server_thread.join().unwrap();
}

/// Elevated priority is a scheduling request, never a functional dependency.
///
/// CI runs this on Windows, macOS, and an unprivileged Linux runner, so both
/// the granted and the refused path must leave the server serving normally.
#[test]
fn elevated_worker_priority_serves_whether_or_not_the_system_grants_it() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);

    let (shutdown, signal) = blazingly::native::shutdown_channel();
    let server_thread = std::thread::spawn(move || {
        blazingly::native::MulticoreServer::new(NonZeroUsize::new(2).unwrap(), || {
            ExecutableApp::new(routes![health]).expect("worker app should compile")
        })
        .with_worker_priority(blazingly::native::WorkerPriority::Elevated)
        .serve_gracefully(address, signal, Duration::from_secs(2))
        .expect("an elevated server should stop cleanly");
    });

    let mut stream = (0..100)
        .find_map(|_| {
            std::net::TcpStream::connect(address).ok().or_else(|| {
                std::thread::sleep(Duration::from_millis(10));
                None
            })
        })
        .expect("elevated listener should start");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(b"GET /health HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));

    shutdown.shutdown();
    server_thread.join().unwrap();
}

#[test]
fn native_http1_coalesces_pipelined_responses_without_reordering() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);

    let (shutdown, signal) = blazingly::native::shutdown_channel();
    let server_thread = std::thread::spawn(move || {
        blazingly::native::MulticoreServer::new(NonZeroUsize::new(1).unwrap(), || {
            ExecutableApp::new(routes![health]).expect("pipeline app should compile")
        })
        .serve_gracefully(address, signal, Duration::from_secs(2))
        .expect("pipeline server should stop cleanly");
    });

    let mut stream = (0..100)
        .find_map(|_| {
            std::net::TcpStream::connect(address).ok().or_else(|| {
                std::thread::sleep(Duration::from_millis(10));
                None
            })
        })
        .expect("pipeline listener should start");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(
            concat!(
                "GET /health HTTP/1.1\r\nhost: localhost\r\n\r\n",
                "GET /health HTTP/1.1\r\nhost: localhost\r\n\r\n",
                "GET /health HTTP/1.1\r\nhost: localhost\r\n\r\n",
                "GET /health HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n",
            )
            .as_bytes(),
        )
        .unwrap();
    let mut responses = String::new();
    stream.read_to_string(&mut responses).unwrap();

    assert_eq!(responses.matches("HTTP/1.1 200 OK\r\n").count(), 4);
    assert_eq!(responses.matches("\r\n\r\n\"ok\"").count(), 4);

    shutdown.shutdown();
    server_thread.join().unwrap();
}

#[test]
fn native_request_timeout_cancels_the_operation_on_the_compio_timer() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);

    let (shutdown, signal) = blazingly::native::shutdown_channel();
    let server_thread = std::thread::spawn(move || {
        blazingly::native::MulticoreServer::new(NonZeroUsize::new(1).unwrap(), || {
            ExecutableApp::new(routes![slow]).expect("timeout app should compile")
        })
        .with_request_timeout(Duration::from_millis(20))
        .serve_gracefully(address, signal, Duration::from_secs(2))
        .expect("timeout server should stop cleanly");
    });

    let mut stream = (0..100)
        .find_map(|_| {
            std::net::TcpStream::connect(address).ok().or_else(|| {
                std::thread::sleep(Duration::from_millis(10));
                None
            })
        })
        .expect("timeout listener should start");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(b"GET /slow HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 504 Gateway Timeout\r\n"));
    assert!(response.contains("\"code\":\"invocation_timeout\""));

    shutdown.shutdown();
    server_thread.join().unwrap();
}

#[cfg(feature = "native-http2")]
#[test]
fn native_http2_adapter_multiplexes_prior_knowledge_streams() {
    use shiguredo_http2::{Connection, Event, HeaderField, Limits};

    H2_FAST_RAN.store(false, Ordering::Release);
    H2_FAST_BODY_POLLED.store(false, Ordering::Release);
    let mut client = Connection::client(Limits::default());
    client.initiate().unwrap();
    let first = client
        .start_stream(
            vec![
                HeaderField::new(":method", "GET").unwrap(),
                HeaderField::new(":path", "/h2-slow").unwrap(),
                HeaderField::new(":scheme", "http").unwrap(),
                HeaderField::new(":authority", "localhost").unwrap(),
            ],
            true,
        )
        .unwrap();
    let second = client
        .start_stream(
            vec![
                HeaderField::new(":method", "GET").unwrap(),
                HeaderField::new(":path", "/h2-fast").unwrap(),
                HeaderField::new(":scheme", "http").unwrap(),
                HeaderField::new(":authority", "localhost").unwrap(),
            ],
            true,
        )
        .unwrap();
    let mut request = Vec::new();
    while let Some(output) = client.poll_output() {
        request.extend_from_slice(&output);
    }

    let mut transport = ScriptedIo::new(request);
    future::block_on(server(1024).serve_io(&mut transport))
        .expect("HTTP/2 prior-knowledge exchange should succeed");

    client.feed(&transport.output).unwrap();
    client.process().unwrap();
    let mut first_status = None;
    let mut first_has_date = false;
    let mut first_body = Vec::new();
    let mut second_status = None;
    let mut second_body = Vec::new();
    while let Some(event) = client.poll_event() {
        match event {
            Event::HeadersReceived {
                stream_id, headers, ..
            } => {
                let status = headers
                    .iter()
                    .find(|header| header.name() == b":status")
                    .map(|header| header.value().to_vec());
                if stream_id == first {
                    first_status = status;
                    first_has_date = headers.iter().any(|header| header.name() == b"date");
                } else if stream_id == second {
                    second_status = status;
                }
            }
            Event::DataReceived {
                stream_id, data, ..
            } if stream_id == first => first_body.extend_from_slice(&data),
            Event::DataReceived {
                stream_id, data, ..
            } if stream_id == second => second_body.extend_from_slice(&data),
            _ => {}
        }
    }
    assert_eq!(first_status.as_deref(), Some(b"200".as_slice()));
    assert!(first_has_date);
    assert_eq!(first_body, b"slow-after-fast-body");
    assert_eq!(second_status.as_deref(), Some(b"200".as_slice()));
    assert_eq!(second_body, b"fast-body");
}

#[cfg(feature = "native-http2")]
struct ScriptedIo {
    input: Cursor<Vec<u8>>,
    output: Vec<u8>,
}

#[cfg(feature = "native-http2")]
impl ScriptedIo {
    fn new(input: Vec<u8>) -> Self {
        Self {
            input: Cursor::new(input),
            output: Vec::new(),
        }
    }
}

#[cfg(feature = "native-http2")]
impl AsyncRead for ScriptedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.input).poll_read(context, buffer)
    }
}

#[cfg(feature = "native-http2")]
impl AsyncWrite for ScriptedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.output.extend_from_slice(buffer);
        Poll::Ready(Ok(buffer.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
