//! Experimental HTTP/2 wire adapter.
//!
//! Protocol state lives here, outside Blazingly's contract, router, DI, and
//! execution crates. Replacing this Sans-I/O codec cannot change an operation.

use std::collections::{HashMap, HashSet};
use std::io;
use std::time::Duration;

use blazingly_core::HttpMethod;
use blazingly_executor::InvocationControl;
use blazingly_http::{HttpApp, HttpRequestView, Response};
use futures_lite::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use shiguredo_http2::{Connection, ErrorCode, Event, HeaderField, Limits, StreamId};

use crate::{READ_CHUNK_BYTES, ServerLimits, ShutdownState, parse_method, with_cached_date};

pub(super) async fn serve_connection<IO>(
    app: &HttpApp,
    limits: ServerLimits,
    io: &mut IO,
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
    let mut requests = HashMap::<StreamId, PendingRequest>::new();
    let mut rejected = HashSet::<StreamId>::new();
    connection.initiate().map_err(h2_error)?;
    flush(&mut connection, io).await?;

    if !initial.is_empty() {
        process_input(
            app,
            limits,
            &mut connection,
            &mut requests,
            &mut rejected,
            &initial,
            request_timeout,
        )
        .await?;
        flush(&mut connection, io).await?;
    }

    let mut read_chunk = vec![0_u8; READ_CHUNK_BYTES];
    loop {
        if shutdown.is_some_and(|state| state.requested.load(std::sync::atomic::Ordering::Acquire))
        {
            connection
                .send_goaway(ErrorCode::NoError, Vec::new())
                .map_err(h2_error)?;
            flush(&mut connection, io).await?;
            return Ok(());
        }

        let read = io.read(&mut read_chunk).await?;
        if read == 0 {
            return Ok(());
        }
        process_input(
            app,
            limits,
            &mut connection,
            &mut requests,
            &mut rejected,
            &read_chunk[..read],
            request_timeout,
        )
        .await?;
        flush(&mut connection, io).await?;
    }
}

async fn process_input(
    app: &HttpApp,
    limits: ServerLimits,
    connection: &mut Connection,
    requests: &mut HashMap<StreamId, PendingRequest>,
    rejected: &mut HashSet<StreamId>,
    bytes: &[u8],
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
                let Ok(request) = PendingRequest::from_headers(headers, limits.max_headers) else {
                    send_simple_response(
                        connection,
                        stream_id,
                        400,
                        b"{\"error\":{\"code\":\"bad_request\",\"message\":\"invalid HTTP/2 request headers\"}}",
                    )?;
                    if !end_stream {
                        rejected.insert(stream_id);
                    }
                    continue;
                };
                let content_length = request
                    .header("content-length", 0)
                    .and_then(|value| value.parse::<usize>().ok());
                if content_length.is_some_and(|length| length > limits.max_body_bytes) {
                    send_payload_too_large(connection, stream_id)?;
                    if !end_stream {
                        rejected.insert(stream_id);
                    }
                    continue;
                }
                requests.insert(stream_id, request);
                if end_stream {
                    dispatch(app, connection, requests, stream_id, request_timeout).await?;
                }
            }
            Event::DataReceived {
                stream_id,
                data,
                end_stream,
            } => {
                replenish_receive_window(connection, stream_id, data.len())?;
                if rejected.contains(&stream_id) {
                    if end_stream {
                        rejected.remove(&stream_id);
                    }
                    continue;
                }
                let Some(request) = requests.get_mut(&stream_id) else {
                    connection
                        .reset_stream(stream_id, ErrorCode::ProtocolError)
                        .map_err(h2_error)?;
                    continue;
                };
                if data.len() > limits.max_body_bytes.saturating_sub(request.body.len()) {
                    requests.remove(&stream_id);
                    send_payload_too_large(connection, stream_id)?;
                    if !end_stream {
                        rejected.insert(stream_id);
                    }
                    continue;
                }
                request.body.extend_from_slice(&data);
                if end_stream {
                    dispatch(app, connection, requests, stream_id, request_timeout).await?;
                }
            }
            Event::TrailersReceived { stream_id, .. } => {
                if rejected.remove(&stream_id) {
                    continue;
                }
                dispatch(app, connection, requests, stream_id, request_timeout).await?;
            }
            Event::StreamReset { stream_id, .. } | Event::StreamClosed { stream_id } => {
                requests.remove(&stream_id);
                rejected.remove(&stream_id);
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

async fn dispatch(
    app: &HttpApp,
    connection: &mut Connection,
    requests: &mut HashMap<StreamId, PendingRequest>,
    stream_id: StreamId,
    request_timeout: Option<Duration>,
) -> io::Result<()> {
    let Some(request) = requests.remove(&stream_id) else {
        return Ok(());
    };
    let method = request.method;
    let mut response = if let Some(timeout) = request_timeout {
        app.call_view_controlled(
            &request,
            InvocationControl::new().with_timeout(compio::time::sleep(timeout)),
        )
        .await
    } else {
        app.call_view(&request).await
    };
    send_response(connection, stream_id, method, &mut response).await
}

async fn send_response(
    connection: &mut Connection,
    stream_id: StreamId,
    method: HttpMethod,
    response: &mut Response,
) -> io::Result<()> {
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
    } else if send_body && streaming {
        let exact_length = response.exact_body_length();
        let mut written = 0_u64;
        while let Some(chunk) = response.next_body_chunk().await {
            let chunk = chunk.map_err(|error| io::Error::other(error.to_string()))?;
            if chunk.is_empty() {
                continue;
            }
            written = written
                .checked_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX))
                .ok_or_else(|| io::Error::other("streaming response length overflow"))?;
            if exact_length.is_some_and(|expected| written > expected) {
                return Err(io::Error::other(
                    "streaming response exceeded its declared exact length",
                ));
            }
            connection
                .send_data(stream_id, chunk, false)
                .map_err(h2_error)?;
        }
        if exact_length.is_some_and(|expected| written != expected) {
            return Err(io::Error::other(
                "streaming response did not match its declared exact length",
            ));
        }
        connection
            .send_data(stream_id, Vec::new(), true)
            .map_err(h2_error)?;
    }
    Ok(())
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
}

impl PendingRequest {
    fn from_headers(headers: Vec<HeaderField>, max_headers: usize) -> Result<Self, ()> {
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
}
