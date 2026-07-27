//! Experimental HTTP/2 wire adapter.
//!
//! Protocol state lives here, outside Blazingly's contract, router, DI, and
//! execution crates. Replacing this Sans-I/O codec cannot change an operation.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use blazingly_core::{BodyStreamError, HttpMethod, InputSource, StreamingBody};
use blazingly_executor::InvocationControl;
use blazingly_http::{HttpApp, HttpRequestView, Response};
use futures_lite::future;
use futures_lite::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use futures_util::stream::{FuturesUnordered, StreamExt};
use shiguredo_http2::{Connection, ErrorCode, Event, HeaderField, Limits, StreamId};

use crate::{
    IncomingBodySender, READ_CHUNK_BYTES, ServerLimits, ShutdownState, incoming_body_channel,
    parse_method, with_cached_date,
};

enum ConnectionActivity {
    Read(io::Result<usize>),
    Work(Option<Box<CompletedWork>>),
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn serve_connection<IO>(
    app: &HttpApp,
    limits: ServerLimits,
    io: &mut IO,
    peer_addr: Option<SocketAddr>,
    scheme: &'static str,
    shutdown: Option<&ShutdownState>,
    initial: Vec<u8>,
    request_timeout: Option<Duration>,
) -> io::Result<()>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    super::ensure_date_updater();
    let protocol_limits = Limits::builder()
        .max_concurrent_streams(Some(
            u32::try_from(limits.max_concurrent_streams).unwrap_or(u32::MAX),
        ))
        .max_header_list_size(Some(
            u32::try_from(limits.max_header_bytes).unwrap_or(u32::MAX),
        ))
        .build()
        .map_err(h2_error)?;
    let mut connection = Connection::server(protocol_limits);
    let mut streams = Streams::new();
    connection.initiate().map_err(h2_error)?;
    flush(&mut connection, io).await?;

    if !initial.is_empty() {
        process_input(
            app,
            limits,
            &mut connection,
            &mut streams,
            &initial,
            peer_addr,
            scheme,
            request_timeout,
        )?;
        flush(&mut connection, io).await?;
    }

    let mut read_chunk = vec![0_u8; READ_CHUNK_BYTES];
    let mut input_closed = false;
    while !input_closed || !streams.in_flight.is_empty() {
        if shutdown.is_some_and(|state| state.requested.load(std::sync::atomic::Ordering::Acquire))
        {
            connection
                .send_goaway(ErrorCode::NoError, Vec::new())
                .map_err(h2_error)?;
            flush(&mut connection, io).await?;
            return Ok(());
        }

        let activity = if input_closed {
            ConnectionActivity::Work(streams.in_flight.next().await.map(Box::new))
        } else if streams.in_flight.is_empty() {
            ConnectionActivity::Read(io.read(&mut read_chunk).await)
        } else {
            let in_flight = &mut streams.in_flight;
            future::race(
                async { ConnectionActivity::Read(io.read(&mut read_chunk).await) },
                async { ConnectionActivity::Work(in_flight.next().await.map(Box::new)) },
            )
            .await
        };
        match activity {
            ConnectionActivity::Read(Ok(0)) => input_closed = true,
            ConnectionActivity::Read(Ok(read)) => {
                process_input(
                    app,
                    limits,
                    &mut connection,
                    &mut streams,
                    &read_chunk[..read],
                    peer_addr,
                    scheme,
                    request_timeout,
                )?;
            }
            ConnectionActivity::Read(Err(error)) => return Err(error),
            ConnectionActivity::Work(Some(work)) => {
                handle_completed_work(&mut connection, *work, &mut streams)?;
            }
            ConnectionActivity::Work(None) => {}
        }
        flush(&mut connection, io).await?;
    }
    Ok(())
}

/// Per-connection stream bookkeeping.
struct Streams<'app> {
    /// Requests whose head arrived but whose handler has not started.
    requests: HashMap<StreamId, PendingRequest>,
    /// Producer side of request bodies the application is consuming.
    bodies: HashMap<StreamId, RequestBody>,
    /// Streams already answered, whose remaining DATA is discarded.
    rejected: HashSet<StreamId>,
    /// Streams with work in flight.
    active: HashSet<StreamId>,
    /// Streams reset while their completed work was already queued.
    cancelled: HashSet<StreamId>,
    /// Cancellation tokens for the work currently in flight per stream.
    tokens: HashMap<StreamId, Rc<StreamCancel>>,
    in_flight: FuturesUnordered<StreamWork<'app>>,
}

impl Streams<'_> {
    fn new() -> Self {
        Self {
            requests: HashMap::new(),
            bodies: HashMap::new(),
            rejected: HashSet::new(),
            active: HashSet::new(),
            cancelled: HashSet::new(),
            tokens: HashMap::new(),
            in_flight: FuturesUnordered::new(),
        }
    }

    /// Forgets every record of a stream whose work has left the queue.
    fn retire(&mut self, stream_id: StreamId) {
        self.active.remove(&stream_id);
        self.cancelled.remove(&stream_id);
        self.tokens.remove(&stream_id);
    }
}

/// Producer side of one streaming HTTP/2 request body.
struct RequestBody {
    sender: IncomingBodySender,
    /// Bytes handed to the application so far.
    received: usize,
    /// Bytes whose receive-window credit has been returned to the peer.
    credited: usize,
    /// Whether `END_STREAM` or trailers ended the request body.
    ended: bool,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn process_input<'app>(
    app: &'app HttpApp,
    limits: ServerLimits,
    connection: &mut Connection,
    streams: &mut Streams<'app>,
    bytes: &[u8],
    peer_addr: Option<SocketAddr>,
    scheme: &'static str,
    request_timeout: Option<Duration>,
) -> io::Result<()> {
    connection.feed(bytes).map_err(h2_error)?;
    connection.process().map_err(h2_error)?;

    while let Some(event) = connection.poll_event() {
        match event {
            Event::HeadersReceived {
                stream_id,
                headers,
                end_stream,
                ..
            } => {
                let Ok(request) =
                    PendingRequest::from_headers(headers, limits.max_headers, peer_addr, scheme)
                else {
                    send_simple_response(
                        connection,
                        stream_id,
                        400,
                        b"{\"error\":{\"code\":\"bad_request\",\"message\":\"invalid HTTP/2 request headers\"}}",
                    )?;
                    if !end_stream {
                        streams.rejected.insert(stream_id);
                    }
                    continue;
                };
                let content_length = request
                    .header("content-length", 0)
                    .and_then(|value| value.parse::<usize>().ok());
                if content_length.is_some_and(|length| length > limits.max_body_bytes) {
                    send_payload_too_large(connection, stream_id)?;
                    if !end_stream {
                        streams.rejected.insert(stream_id);
                    }
                    continue;
                }
                let streaming = app.request_body_source(request.method, &request.target)
                    == Some(InputSource::Stream);
                streams.requests.insert(stream_id, request);
                if streaming {
                    begin_streaming_request(
                        app,
                        streams,
                        stream_id,
                        content_length,
                        end_stream,
                        request_timeout,
                    );
                } else if end_stream {
                    enqueue_dispatch(app, streams, stream_id, request_timeout);
                }
            }
            Event::DataReceived {
                stream_id,
                data,
                end_stream,
            } => {
                if streams.rejected.contains(&stream_id) {
                    replenish_receive_window(connection, stream_id, data.len())?;
                    if end_stream {
                        streams.rejected.remove(&stream_id);
                    }
                    continue;
                }
                if streams.bodies.contains_key(&stream_id) {
                    receive_streaming_data(
                        limits, connection, streams, stream_id, data, end_stream,
                    )?;
                    continue;
                }
                // A buffered body is already bounded by `max_body_bytes`, so
                // its credit can be returned as the bytes arrive.
                replenish_receive_window(connection, stream_id, data.len())?;
                let Some(request) = streams.requests.get_mut(&stream_id) else {
                    connection
                        .reset_stream(stream_id, ErrorCode::ProtocolError)
                        .map_err(h2_error)?;
                    continue;
                };
                if data.len() > limits.max_body_bytes.saturating_sub(request.body.len()) {
                    streams.requests.remove(&stream_id);
                    send_payload_too_large(connection, stream_id)?;
                    if !end_stream {
                        streams.rejected.insert(stream_id);
                    }
                    continue;
                }
                request.body.extend_from_slice(&data);
                if end_stream {
                    enqueue_dispatch(app, streams, stream_id, request_timeout);
                }
            }
            Event::TrailersReceived { stream_id, .. } => {
                if streams.rejected.remove(&stream_id) {
                    continue;
                }
                if let Some(body) = streams.bodies.get_mut(&stream_id) {
                    body.ended = true;
                    body.sender.close();
                    continue;
                }
                enqueue_dispatch(app, streams, stream_id, request_timeout);
            }
            Event::StreamReset { stream_id, .. } => {
                streams.requests.remove(&stream_id);
                streams.rejected.remove(&stream_id);
                if let Some(body) = streams.bodies.remove(&stream_id) {
                    body.sender.fail(BodyStreamError::new(
                        "stream_reset",
                        "the peer reset this HTTP/2 stream",
                    ));
                    // Bytes nobody will read now must still return their
                    // connection-level credit or the connection starves.
                    replenish_connection_window(
                        connection,
                        body.received.saturating_sub(body.credited),
                    )?;
                }
                cancel_stream(streams, stream_id);
            }
            Event::StreamClosed { stream_id } => {
                streams.requests.remove(&stream_id);
                streams.rejected.remove(&stream_id);
                if streams.active.contains(&stream_id) {
                    streams.cancelled.insert(stream_id);
                }
            }
            Event::ConnectionError { reason, .. } => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, reason));
            }
            Event::GoawayReceived { .. } => return Ok(()),
            Event::ConnectionPreface
            | Event::SettingsReceived { .. }
            | Event::PingReceived { .. }
            | Event::WindowUpdateReceived { .. }
            | Event::PriorityUpdateReceived { .. } => {}
        }
    }
    Ok(())
}

/// Starts a streaming operation as soon as its request head is validated.
///
/// The handler observes DATA frames through the same `StreamingBody` seam the
/// HTTP/1 adapter uses, so an operation does not change shape per protocol.
fn begin_streaming_request<'app>(
    app: &'app HttpApp,
    streams: &mut Streams<'app>,
    stream_id: StreamId,
    content_length: Option<usize>,
    end_stream: bool,
    request_timeout: Option<Duration>,
) {
    let exact_length = if end_stream {
        Some(0)
    } else {
        content_length.map(|length| u64::try_from(length).unwrap_or(u64::MAX))
    };
    let (sender, body) = incoming_body_channel(exact_length);
    if let Some(request) = streams.requests.get_mut(&stream_id) {
        *request.body_stream.borrow_mut() = Some(body);
    }
    if end_stream {
        sender.close();
    } else {
        streams.bodies.insert(
            stream_id,
            RequestBody {
                sender: sender.clone(),
                received: 0,
                credited: 0,
                ended: false,
            },
        );
    }
    enqueue_dispatch(app, streams, stream_id, request_timeout);
    if !end_stream {
        enqueue_credit_poll(streams, stream_id, sender);
    }
}

/// Hands one DATA frame to a streaming handler without returning credit.
///
/// Receive-window credit is returned only once the application has actually
/// consumed the bytes, so a handler that stops reading stops the peer.
fn receive_streaming_data(
    limits: ServerLimits,
    connection: &mut Connection,
    streams: &mut Streams<'_>,
    stream_id: StreamId,
    data: Vec<u8>,
    end_stream: bool,
) -> io::Result<()> {
    let length = data.len();
    let Some(body) = streams.bodies.get_mut(&stream_id) else {
        return Ok(());
    };
    if length > limits.max_body_bytes.saturating_sub(body.received) {
        let outstanding = body.received.saturating_sub(body.credited);
        body.sender.fail(BodyStreamError::new(
            "payload_too_large",
            "request body exceeds the configured limit",
        ));
        streams.bodies.remove(&stream_id);
        cancel_stream(streams, stream_id);
        replenish_connection_window(connection, outstanding.saturating_add(length))?;
        send_payload_too_large(connection, stream_id)?;
        if !end_stream {
            streams.rejected.insert(stream_id);
        }
        return Ok(());
    }
    body.received = body.received.saturating_add(length);
    body.ended |= end_stream;
    let delivered = body.sender.push(data);
    if end_stream {
        body.sender.close();
    }
    if delivered {
        return Ok(());
    }
    // The handler let the body go, so the discarded bytes cost nothing and
    // their credit is returned immediately.
    body.credited = body.credited.saturating_add(length);
    let ended = body.ended;
    if ended {
        replenish_connection_window(connection, length)
    } else {
        replenish_receive_window(connection, stream_id, length)
    }
}

/// Drops the work in flight for a stream the peer or the adapter gave up on.
fn cancel_stream(streams: &mut Streams<'_>, stream_id: StreamId) {
    if streams.active.contains(&stream_id) {
        streams.cancelled.insert(stream_id);
    }
    if let Some(token) = streams.tokens.get(&stream_id) {
        token.cancel();
    }
}

type WorkFuture<'app> = Pin<Box<dyn Future<Output = CompletedWork> + 'app>>;

/// Cooperative cancellation shared with one unit of in-flight stream work.
#[derive(Default)]
struct StreamCancel {
    cancelled: Cell<bool>,
    waker: RefCell<Option<Waker>>,
}

impl StreamCancel {
    fn cancel(&self) {
        self.cancelled.set(true);
        if let Some(waker) = self.waker.borrow_mut().take() {
            waker.wake();
        }
    }
}

/// One unit of stream work that a peer `RST_STREAM` drops instead of awaiting.
///
/// Returning `Cancelled` makes `FuturesUnordered` drop this wrapper, and with
/// it the handler future, so a reset stream stops consuming CPU and request
/// resources immediately instead of running to completion unobserved.
struct StreamWork<'app> {
    stream_id: StreamId,
    cancel: Rc<StreamCancel>,
    work: WorkFuture<'app>,
}

impl<'app> StreamWork<'app> {
    fn cancellable(stream_id: StreamId, cancel: Rc<StreamCancel>, work: WorkFuture<'app>) -> Self {
        Self {
            stream_id,
            cancel,
            work,
        }
    }

    /// Work that finishes on its own; it observes a reset through the request
    /// body rather than through the stream's cancellation token.
    fn uninterruptible(stream_id: StreamId, work: WorkFuture<'app>) -> Self {
        Self {
            stream_id,
            cancel: Rc::new(StreamCancel::default()),
            work,
        }
    }
}

impl Future for StreamWork<'_> {
    type Output = CompletedWork;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let work = self.get_mut();
        if work.cancel.cancelled.get() {
            return Poll::Ready(CompletedWork::Cancelled(work.stream_id));
        }
        *work.cancel.waker.borrow_mut() = Some(context.waker().clone());
        work.work.as_mut().poll(context)
    }
}

enum CompletedWork {
    Response(CompletedResponse),
    Body(StreamBodyWork),
    Credit(ConsumedCredit),
    Cancelled(StreamId),
}

/// Request-body bytes the application consumed on one stream.
#[derive(Clone, Copy)]
struct ConsumedCredit {
    stream_id: StreamId,
    consumed: usize,
    finished: bool,
}

struct CompletedResponse {
    stream_id: StreamId,
    method: HttpMethod,
    response: Response,
}

struct StreamingResponse {
    stream_id: StreamId,
    response: Response,
    exact_length: Option<u64>,
    written: u64,
}

enum StreamBodyWork {
    Chunk {
        state: StreamingResponse,
        bytes: Vec<u8>,
    },
    End(StreamingResponse),
    Error(StreamingResponse),
}

fn enqueue_dispatch<'app>(
    app: &'app HttpApp,
    streams: &mut Streams<'app>,
    stream_id: StreamId,
    request_timeout: Option<Duration>,
) {
    let Some(request) = streams.requests.remove(&stream_id) else {
        return;
    };
    let method = request.method;
    let cancel = Rc::new(StreamCancel::default());
    streams.active.insert(stream_id);
    streams.tokens.insert(stream_id, Rc::clone(&cancel));
    streams.in_flight.push(StreamWork::cancellable(
        stream_id,
        cancel,
        Box::pin(async move {
            let response = if let Some(timeout) = request_timeout {
                app.call_view_controlled(
                    &request,
                    InvocationControl::new().with_timeout(compio::time::sleep(timeout)),
                )
                .await
            } else {
                app.call_view(&request).await
            };
            CompletedWork::Response(CompletedResponse {
                stream_id,
                method,
                response,
            })
        }),
    ));
}

fn enqueue_credit_poll(streams: &mut Streams<'_>, stream_id: StreamId, sender: IncomingBodySender) {
    streams.in_flight.push(StreamWork::uninterruptible(
        stream_id,
        Box::pin(async move {
            let (consumed, finished) = sender.consumed().await;
            CompletedWork::Credit(ConsumedCredit {
                stream_id,
                consumed,
                finished,
            })
        }),
    ));
}

fn handle_completed_work(
    connection: &mut Connection,
    work: CompletedWork,
    streams: &mut Streams<'_>,
) -> io::Result<()> {
    match work {
        CompletedWork::Cancelled(stream_id) => streams.retire(stream_id),
        CompletedWork::Credit(credit) => {
            return handle_consumed_credit(connection, streams, credit);
        }
        CompletedWork::Response(completed) => {
            let stream_id = completed.stream_id;
            if streams.cancelled.remove(&stream_id) {
                streams.retire(stream_id);
                return Ok(());
            }
            if let Some(streaming) = begin_response(connection, completed)? {
                enqueue_body_poll(streams, streaming);
            } else {
                streams.retire(stream_id);
            }
        }
        CompletedWork::Body(StreamBodyWork::Chunk { state, bytes }) => {
            if streams.cancelled.remove(&state.stream_id) {
                streams.retire(state.stream_id);
                return Ok(());
            }
            connection
                .send_data(state.stream_id, bytes, false)
                .map_err(h2_error)?;
            enqueue_body_poll(streams, state);
        }
        CompletedWork::Body(StreamBodyWork::End(mut state)) => {
            let stream_id = state.stream_id;
            let cancelled = streams.cancelled.contains(&stream_id);
            streams.retire(stream_id);
            if !cancelled {
                connection
                    .send_data(stream_id, Vec::new(), true)
                    .map_err(h2_error)?;
                super::schedule_background(state.response.take_background_tasks());
            }
        }
        CompletedWork::Body(StreamBodyWork::Error(mut state)) => {
            let stream_id = state.stream_id;
            streams.retire(stream_id);
            connection
                .reset_stream(stream_id, ErrorCode::InternalError)
                .map_err(h2_error)?;
            super::schedule_background(state.response.take_background_tasks());
        }
    }
    Ok(())
}

/// Returns receive-window credit for request bytes the handler has read.
fn handle_consumed_credit(
    connection: &mut Connection,
    streams: &mut Streams<'_>,
    credit: ConsumedCredit,
) -> io::Result<()> {
    let Some(body) = streams.bodies.get_mut(&credit.stream_id) else {
        return Ok(());
    };
    let sender = body.sender.clone();
    let ended = body.ended;
    let increment = if credit.finished {
        let outstanding = body.received.saturating_sub(body.credited);
        body.credited = body.received;
        outstanding
    } else {
        body.credited = body.credited.saturating_add(credit.consumed);
        credit.consumed
    };
    if credit.finished {
        streams.bodies.remove(&credit.stream_id);
    } else {
        enqueue_credit_poll(streams, credit.stream_id, sender);
    }
    if ended {
        replenish_connection_window(connection, increment)
    } else {
        replenish_receive_window(connection, credit.stream_id, increment)
    }
}

fn enqueue_body_poll(streams: &mut Streams<'_>, mut state: StreamingResponse) {
    let stream_id = state.stream_id;
    let cancel = Rc::clone(streams.tokens.entry(stream_id).or_default());
    streams.in_flight.push(StreamWork::cancellable(
        stream_id,
        cancel,
        Box::pin(async move {
            let work = match state.response.next_body_chunk().await {
                Some(Ok(bytes)) => {
                    let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                    let Some(written) = state.written.checked_add(length) else {
                        return CompletedWork::Body(StreamBodyWork::Error(state));
                    };
                    if state
                        .exact_length
                        .is_some_and(|expected| written > expected)
                    {
                        return CompletedWork::Body(StreamBodyWork::Error(state));
                    }
                    state.written = written;
                    StreamBodyWork::Chunk { state, bytes }
                }
                Some(Err(_)) => StreamBodyWork::Error(state),
                None if state
                    .exact_length
                    .is_some_and(|expected| state.written != expected) =>
                {
                    StreamBodyWork::Error(state)
                }
                None => StreamBodyWork::End(state),
            };
            CompletedWork::Body(work)
        }),
    ));
}

fn begin_response(
    connection: &mut Connection,
    mut completed: CompletedResponse,
) -> io::Result<Option<StreamingResponse>> {
    let stream_id = completed.stream_id;
    let method = completed.method;
    let response = &mut completed.response;
    let send_body = method != HttpMethod::Head
        && !matches!(response.status(), 204 | 304)
        && !(method == HttpMethod::Connect && (200..300).contains(&response.status()));
    let mut headers = Vec::new();
    headers.push(HeaderField::new(":status", response.status().to_string()).map_err(h2_error)?);
    let mut has_date = false;
    for (name, value) in response.headers() {
        if is_connection_specific(name)
            || name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
        {
            continue;
        }
        has_date |= name.eq_ignore_ascii_case("date");
        headers.push(HeaderField::new(name.to_ascii_lowercase(), value).map_err(h2_error)?);
    }
    if response.status() != 204
        && !(method == HttpMethod::Connect && (200..300).contains(&response.status()))
    {
        if let Some(length) = response.exact_body_length() {
            headers.push(HeaderField::new("content-length", length.to_string()).map_err(h2_error)?);
        }
    }
    if !has_date {
        headers.push(with_cached_date(|date| HeaderField::new("date", date)).map_err(h2_error)?);
    }
    let streaming = response.is_streaming();
    connection
        .send_response(
            stream_id,
            headers,
            !send_body || (!streaming && response.body().is_empty()),
        )
        .map_err(h2_error)?;
    if send_body && !streaming && !response.body().is_empty() {
        connection
            .send_data(stream_id, response.body().to_vec(), true)
            .map_err(h2_error)?;
    }
    if send_body && streaming {
        let exact_length = completed.response.exact_body_length();
        return Ok(Some(StreamingResponse {
            stream_id,
            response: completed.response,
            exact_length,
            written: 0,
        }));
    }
    super::schedule_background(completed.response.take_background_tasks());
    Ok(None)
}

fn send_payload_too_large(connection: &mut Connection, stream_id: StreamId) -> io::Result<()> {
    send_simple_response(
        connection,
        stream_id,
        413,
        b"{\"error\":{\"code\":\"payload_too_large\",\"message\":\"request body exceeds the configured limit\"}}",
    )
}

fn send_simple_response(
    connection: &mut Connection,
    stream_id: StreamId,
    status: u16,
    body: &[u8],
) -> io::Result<()> {
    let headers = vec![
        HeaderField::new(":status", status.to_string()).map_err(h2_error)?,
        HeaderField::new("content-type", "application/json").map_err(h2_error)?,
        HeaderField::new("content-length", body.len().to_string()).map_err(h2_error)?,
        with_cached_date(|date| HeaderField::new("date", date)).map_err(h2_error)?,
    ];
    connection
        .send_response(stream_id, headers, body.is_empty())
        .map_err(h2_error)?;
    if !body.is_empty() {
        connection
            .send_data(stream_id, body.to_vec(), true)
            .map_err(h2_error)?;
    }
    Ok(())
}

fn replenish_receive_window(
    connection: &mut Connection,
    stream_id: StreamId,
    consumed: usize,
) -> io::Result<()> {
    let increment = u32::try_from(consumed).unwrap_or(u32::MAX);
    if increment == 0 {
        return Ok(());
    }
    connection
        .send_window_update(StreamId::Connection, increment)
        .map_err(h2_error)?;
    connection
        .send_window_update(stream_id, increment)
        .map_err(h2_error)
}

/// Returns connection-level credit for a stream that can no longer send DATA.
fn replenish_connection_window(connection: &mut Connection, consumed: usize) -> io::Result<()> {
    let increment = u32::try_from(consumed).unwrap_or(u32::MAX);
    if increment == 0 {
        return Ok(());
    }
    connection
        .send_window_update(StreamId::Connection, increment)
        .map_err(h2_error)
}

async fn flush<IO>(connection: &mut Connection, io: &mut IO) -> io::Result<()>
where
    IO: AsyncWrite + Unpin,
{
    while let Some(output) = connection.poll_output() {
        io.write_all(&output).await?;
    }
    io.flush().await
}

fn is_connection_specific(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "upgrade"
    )
}

fn h2_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

struct PendingRequest {
    method: HttpMethod,
    target: String,
    authority: Option<String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    body_stream: RefCell<Option<StreamingBody>>,
    peer_addr: Option<SocketAddr>,
    scheme: &'static str,
}

impl PendingRequest {
    fn from_headers(
        headers: Vec<HeaderField>,
        max_headers: usize,
        peer_addr: Option<SocketAddr>,
        scheme: &'static str,
    ) -> Result<Self, ()> {
        if headers.len() > max_headers {
            return Err(());
        }
        let mut method = None;
        let mut target = None;
        let mut authority = None;
        let mut regular = Vec::new();
        for header in headers {
            let name = std::str::from_utf8(header.name()).map_err(|_| ())?;
            let value = std::str::from_utf8(header.value()).map_err(|_| ())?;
            match name {
                ":method" => method = Some(parse_method(value).map_err(|_| ())?),
                ":path" => target = Some(value.to_owned()),
                ":authority" => authority = Some(value.to_owned()),
                ":scheme" | ":protocol" => {}
                _ if name.starts_with(':') => return Err(()),
                _ => regular.push((name.to_owned(), value.to_owned())),
            }
        }
        let method = method.ok_or(())?;
        let target = if method == HttpMethod::Connect {
            target.or_else(|| authority.clone()).ok_or(())?
        } else {
            target.ok_or(())?
        };
        Ok(Self {
            method,
            target,
            authority,
            headers: regular,
            body: Vec::new(),
            body_stream: RefCell::new(None),
            peer_addr,
            scheme,
        })
    }

    fn header(&self, name: &str, index: usize) -> Option<&str> {
        if name.eq_ignore_ascii_case("host") {
            return index.eq(&0).then_some(self.authority.as_deref()).flatten();
        }
        self.headers
            .iter()
            .filter(|(header, _)| super::native_header_name_matches(header, name))
            .nth(index)
            .map(|(_, value)| value.as_str())
    }
}

impl HttpRequestView for PendingRequest {
    fn method(&self) -> HttpMethod {
        self.method
    }

    fn target(&self) -> &str {
        &self.target
    }

    fn header_value(&self, name: &str, index: usize) -> Option<&str> {
        self.header(name, index)
    }

    fn body(&self) -> &[u8] {
        &self.body
    }

    fn take_body_stream(&self) -> Option<StreamingBody> {
        self.body_stream.borrow_mut().take()
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer_addr
    }

    fn scheme(&self) -> &str {
        self.scheme
    }
}
