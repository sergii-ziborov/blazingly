#![forbid(unsafe_code)]

//! Runtime-neutral Server-Sent Events and WebSocket sessions.

use base64::Engine as _;
use blazingly_core::{
    ApiSchema, BodyStream, BodyStreamError, HttpUpgrade, InputSource, ResponseHeader, SchemaKind,
    StreamingBody, TypeDescriptor, UpgradeIoError, UpgradedIo,
};
use blazingly_executor::{
    ExecutionOutcome, FromInvocation, InputRejection, InvocationInput, OperationOutput,
};
use serde::Serialize;
use sha1::{Digest as _, Sha1};
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const DEFAULT_MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// One Server-Sent Events record.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SseEvent {
    data: String,
    event: Option<String>,
    id: Option<String>,
    retry: Option<Duration>,
    comment: Option<String>,
}

impl SseEvent {
    #[must_use]
    pub fn data(data: impl Into<String>) -> Self {
        Self {
            data: data.into(),
            ..Self::default()
        }
    }

    /// Serializes a JSON value into the event data field.
    ///
    /// # Errors
    ///
    /// Returns the serializer error when the value cannot be encoded.
    pub fn json(value: &impl Serialize) -> Result<Self, blazingly_json::Error> {
        blazingly_json::to_string(value).map(Self::data)
    }

    /// Sets the event type.
    ///
    /// # Errors
    ///
    /// Rejects carriage returns and line feeds because they would create
    /// additional protocol fields.
    pub fn with_event(mut self, event: impl Into<String>) -> Result<Self, SseEventError> {
        self.event = Some(single_line("event", event.into())?);
        Ok(self)
    }

    /// Sets the event ID.
    ///
    /// # Errors
    ///
    /// Rejects NUL, carriage returns, and line feeds as required by the event
    /// stream grammar.
    pub fn with_id(mut self, id: impl Into<String>) -> Result<Self, SseEventError> {
        let id = single_line("id", id.into())?;
        if id.contains('\0') {
            return Err(SseEventError::InvalidField("id"));
        }
        self.id = Some(id);
        Ok(self)
    }

    #[must_use]
    pub const fn with_retry(mut self, retry: Duration) -> Self {
        self.retry = Some(retry);
        self
    }

    #[must_use]
    pub fn with_comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    #[must_use]
    pub fn keep_alive(comment: impl Into<String>) -> Self {
        Self::default().with_comment(comment)
    }

    /// Encodes the complete event including its terminating blank line.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(self.data.len().saturating_add(64));
        if let Some(comment) = &self.comment {
            for line in normalized_lines(comment) {
                output.extend_from_slice(b":");
                if !line.is_empty() {
                    output.extend_from_slice(b" ");
                    output.extend_from_slice(line.as_bytes());
                }
                output.extend_from_slice(b"\n");
            }
        }
        if let Some(event) = &self.event {
            field(&mut output, "event", event);
        }
        if let Some(id) = &self.id {
            field(&mut output, "id", id);
        }
        if let Some(retry) = self.retry {
            field(
                &mut output,
                "retry",
                &retry.as_millis().min(u128::from(u64::MAX)).to_string(),
            );
        }
        let comment_only = self.comment.is_some()
            && self.data.is_empty()
            && self.event.is_none()
            && self.id.is_none()
            && self.retry.is_none();
        if !comment_only {
            for line in normalized_lines(&self.data) {
                field(&mut output, "data", line);
            }
        }
        output.extend_from_slice(b"\n");
        output
    }
}

impl ApiSchema for SseEvent {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::scalar("SseEvent", SchemaKind::String)
    }
}

/// Invalid SSE metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SseEventError {
    InvalidField(&'static str),
}

impl fmt::Display for SseEventError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidField(field) => {
                write!(formatter, "SSE {field} must be a single safe line")
            }
        }
    }
}

impl std::error::Error for SseEventError {}

/// A pull-based event stream with native backpressure.
#[derive(Debug)]
pub struct Sse {
    body: StreamingBody,
}

impl Sse {
    #[must_use]
    pub fn new(body: StreamingBody) -> Self {
        Self { body }
    }

    #[must_use]
    pub fn from_events<I>(events: I) -> Self
    where
        I: IntoIterator<Item = SseEvent>,
        I::IntoIter: Unpin + 'static,
    {
        Self::new(StreamingBody::new(SseIterator {
            events: events.into_iter(),
        }))
    }

    #[must_use]
    pub fn once(event: SseEvent) -> Self {
        Self::from_events([event])
    }
}

impl ApiSchema for Sse {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::scalar("Sse", SchemaKind::String)
    }
}

impl OperationOutput for Sse {
    fn into_execution_outcome(self) -> ExecutionOutcome {
        ExecutionOutcome::StreamingSuccess {
            status: 200,
            headers: vec![
                ResponseHeader::new("content-type", "text/event-stream; charset=utf-8"),
                ResponseHeader::new("cache-control", "no-cache"),
                ResponseHeader::new("x-accel-buffering", "no"),
            ],
            body: self.body,
            background: Vec::new(),
        }
    }
}

struct SseIterator<I> {
    events: I,
}

impl<I> BodyStream for SseIterator<I>
where
    I: Iterator<Item = SseEvent> + Unpin + 'static,
{
    fn poll_next(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Vec<u8>, BodyStreamError>>> {
        Poll::Ready(self.get_mut().events.next().map(|event| Ok(event.encode())))
    }
}

/// Validated headers from an incoming WebSocket handshake.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebSocketRequest {
    key: String,
    protocols: Vec<String>,
}

impl WebSocketRequest {
    #[must_use]
    pub fn protocols(&self) -> &[String] {
        &self.protocols
    }

    /// Selects the first offered protocol supported by the application.
    #[must_use]
    pub fn select_protocol<'protocol>(
        &self,
        supported: impl IntoIterator<Item = &'protocol str>,
    ) -> Option<String> {
        supported
            .into_iter()
            .find(|supported| self.protocols.iter().any(|offered| offered == supported))
            .map(str::to_owned)
    }

    #[must_use]
    pub fn on_upgrade<Handler, Future>(self, handler: Handler) -> WebSocketUpgrade
    where
        Handler: FnOnce(WebSocket) -> Future + 'static,
        Future: std::future::Future<Output = Result<(), WebSocketError>> + 'static,
    {
        self.on_upgrade_with_protocol(None, handler)
    }

    /// Creates an upgrade and echoes a protocol that the client offered.
    ///
    /// # Panics
    ///
    /// Panics when `protocol` was not present in the request. Use
    /// [`Self::select_protocol`] when negotiating from multiple choices.
    #[must_use]
    pub fn on_upgrade_with_protocol<Handler, Future>(
        self,
        protocol: Option<String>,
        handler: Handler,
    ) -> WebSocketUpgrade
    where
        Handler: FnOnce(WebSocket) -> Future + 'static,
        Future: std::future::Future<Output = Result<(), WebSocketError>> + 'static,
    {
        if let Some(protocol) = &protocol {
            assert!(
                self.protocols.iter().any(|offered| offered == protocol),
                "selected WebSocket protocol was not offered by the client"
            );
        }
        let mut headers = vec![
            ResponseHeader::new("connection", "Upgrade"),
            ResponseHeader::new("upgrade", "websocket"),
            ResponseHeader::new("sec-websocket-accept", websocket_accept(&self.key)),
        ];
        if let Some(protocol) = protocol {
            headers.push(ResponseHeader::new("sec-websocket-protocol", protocol));
        }
        let upgrade = HttpUpgrade::new("websocket", headers, move |io| {
            Box::pin(async move {
                handler(WebSocket::new(io))
                    .await
                    .map_err(WebSocketError::into_upgrade_error)
            })
        });
        WebSocketUpgrade { upgrade }
    }
}

impl ApiSchema for WebSocketRequest {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::scalar("WebSocketRequest", SchemaKind::Binary)
    }
}

impl FromInvocation for WebSocketRequest {
    fn from_invocation(
        input: &InvocationInput<'_>,
        _name: &str,
        _required: bool,
    ) -> Result<Self, InputRejection> {
        let InvocationInput::Http(parts) = input else {
            return Err(InputRejection::new(
                400,
                "websocket_requires_http",
                "WebSocket upgrades are available only through HTTP",
            ));
        };
        let upgrade = header(*parts, "upgrade").unwrap_or_default();
        let connection = header(*parts, "connection").unwrap_or_default();
        let version = header(*parts, "sec-websocket-version").unwrap_or_default();
        let key = header(*parts, "sec-websocket-key").unwrap_or_default();
        if !contains_token(&upgrade, "websocket") || !contains_token(&connection, "upgrade") {
            return Err(handshake_rejection(
                "websocket_upgrade_required",
                "request does not contain a WebSocket Upgrade handshake",
            ));
        }
        if version.trim() != "13" {
            return Err(handshake_rejection(
                "unsupported_websocket_version",
                "Sec-WebSocket-Version must be 13",
            ));
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(key.trim())
            .map_err(|_| {
                handshake_rejection(
                    "invalid_websocket_key",
                    "Sec-WebSocket-Key is not valid base64",
                )
            })?;
        if decoded.len() != 16 {
            return Err(handshake_rejection(
                "invalid_websocket_key",
                "Sec-WebSocket-Key must decode to 16 bytes",
            ));
        }
        let protocols = header(*parts, "sec-websocket-protocol")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|protocol| !protocol.is_empty())
            .map(str::to_owned)
            .collect();
        Ok(Self {
            key: key.trim().to_owned(),
            protocols,
        })
    }
}

/// Typed response that switches the current HTTP/1 connection to WebSocket.
#[derive(Debug)]
pub struct WebSocketUpgrade {
    upgrade: HttpUpgrade,
}

impl ApiSchema for WebSocketUpgrade {
    fn type_descriptor() -> TypeDescriptor {
        TypeDescriptor::scalar("WebSocketUpgrade", SchemaKind::Binary)
    }
}

impl OperationOutput for WebSocketUpgrade {
    fn into_execution_outcome(self) -> ExecutionOutcome {
        ExecutionOutcome::Upgrade {
            upgrade: self.upgrade,
            background: Vec::new(),
        }
    }
}

/// One application-visible WebSocket message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WebSocketMessage {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<WebSocketClose>),
}

/// A WebSocket close status and reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebSocketClose {
    pub code: u16,
    pub reason: String,
}

/// Runtime-neutral WebSocket session.
pub struct WebSocket {
    io: Box<dyn UpgradedIo>,
    read_buffer: Vec<u8>,
    fragmented: Option<(u8, Vec<u8>)>,
    max_message_bytes: usize,
    close_sent: bool,
}

impl WebSocket {
    fn new(io: Box<dyn UpgradedIo>) -> Self {
        Self {
            io,
            read_buffer: Vec::new(),
            fragmented: None,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            close_sent: false,
        }
    }

    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics when `bytes` is zero.
    pub fn with_max_message_bytes(mut self, bytes: usize) -> Self {
        assert!(
            bytes > 0,
            "WebSocket message limit must be greater than zero"
        );
        self.max_message_bytes = bytes;
        self
    }

    /// Receives the next complete message.
    ///
    /// Ping frames are answered automatically and are still returned so
    /// applications can observe liveness traffic.
    ///
    /// # Errors
    ///
    /// Returns a protocol, UTF-8, size, or adapter I/O error.
    pub async fn receive(&mut self) -> Result<Option<WebSocketMessage>, WebSocketError> {
        loop {
            if let Some(frame) = decode_frame(&mut self.read_buffer, self.max_message_bytes)? {
                if let Some(message) = self.process_frame(frame).await? {
                    return Ok(Some(message));
                }
                continue;
            }
            let Some(bytes) = self.io.read().await.map_err(WebSocketError::Io)? else {
                return Ok(None);
            };
            if bytes.is_empty() {
                continue;
            }
            if self.read_buffer.len().saturating_add(bytes.len()) > self.max_message_bytes + 14 {
                return Err(WebSocketError::MessageTooLarge);
            }
            self.read_buffer.extend_from_slice(&bytes);
        }
    }

    /// Sends one complete server message.
    ///
    /// # Errors
    ///
    /// Returns a size, protocol, or adapter I/O error.
    pub async fn send(&mut self, message: WebSocketMessage) -> Result<(), WebSocketError> {
        let (opcode, payload) = message_payload(message)?;
        if payload.len() > self.max_message_bytes {
            return Err(WebSocketError::MessageTooLarge);
        }
        self.io
            .write(encode_frame(opcode, &payload))
            .await
            .map_err(WebSocketError::Io)?;
        self.close_sent |= opcode == 0x8;
        Ok(())
    }

    /// Sends a close frame and closes the upgraded transport.
    ///
    /// # Errors
    ///
    /// Returns an invalid close-code/reason or adapter I/O error.
    pub async fn close(&mut self, close: Option<WebSocketClose>) -> Result<(), WebSocketError> {
        if !self.close_sent {
            self.send(WebSocketMessage::Close(close)).await?;
        }
        self.io.shutdown().await.map_err(WebSocketError::Io)
    }

    async fn process_frame(
        &mut self,
        frame: Frame,
    ) -> Result<Option<WebSocketMessage>, WebSocketError> {
        match frame.opcode {
            0x0 => {
                let Some((_, payload)) = self.fragmented.as_mut() else {
                    return Err(WebSocketError::Protocol(
                        "continuation frame has no initial data frame",
                    ));
                };
                if payload.len().saturating_add(frame.payload.len()) > self.max_message_bytes {
                    return Err(WebSocketError::MessageTooLarge);
                }
                payload.extend_from_slice(&frame.payload);
                if frame.fin {
                    let (opcode, payload) = self.fragmented.take().expect("fragment exists");
                    Ok(Some(data_message(opcode, payload)?))
                } else {
                    Ok(None)
                }
            }
            opcode @ (0x1 | 0x2) if frame.fin => Ok(Some(data_message(opcode, frame.payload)?)),
            opcode @ (0x1 | 0x2) => {
                if self.fragmented.is_some() {
                    return Err(WebSocketError::Protocol(
                        "a fragmented message is already in progress",
                    ));
                }
                self.fragmented = Some((opcode, frame.payload));
                Ok(None)
            }
            0x8 => {
                let close = decode_close(&frame.payload)?;
                if !self.close_sent {
                    self.send(WebSocketMessage::Close(close.clone())).await?;
                }
                Ok(Some(WebSocketMessage::Close(close)))
            }
            0x9 => {
                self.send(WebSocketMessage::Pong(frame.payload.clone()))
                    .await?;
                Ok(Some(WebSocketMessage::Ping(frame.payload)))
            }
            0xA => Ok(Some(WebSocketMessage::Pong(frame.payload))),
            _ => Err(WebSocketError::Protocol("unsupported WebSocket opcode")),
        }
    }
}

impl fmt::Debug for WebSocket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSocket")
            .field("buffered_bytes", &self.read_buffer.len())
            .field("max_message_bytes", &self.max_message_bytes)
            .field("close_sent", &self.close_sent)
            .finish_non_exhaustive()
    }
}

/// WebSocket handshake, framing, or adapter failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WebSocketError {
    Io(UpgradeIoError),
    Protocol(&'static str),
    InvalidUtf8,
    InvalidClose,
    MessageTooLarge,
}

impl WebSocketError {
    fn into_upgrade_error(self) -> UpgradeIoError {
        UpgradeIoError::new("websocket_error", self.to_string())
    }
}

impl fmt::Display for WebSocketError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Protocol(message) => formatter.write_str(message),
            Self::InvalidUtf8 => formatter.write_str("WebSocket text is not valid UTF-8"),
            Self::InvalidClose => formatter.write_str("WebSocket close payload is invalid"),
            Self::MessageTooLarge => formatter.write_str("WebSocket message exceeds its limit"),
        }
    }
}

impl std::error::Error for WebSocketError {}

struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

fn decode_frame(
    buffer: &mut Vec<u8>,
    max_message_bytes: usize,
) -> Result<Option<Frame>, WebSocketError> {
    if buffer.len() < 2 {
        return Ok(None);
    }
    let first = buffer[0];
    let second = buffer[1];
    if first & 0x70 != 0 {
        return Err(WebSocketError::Protocol(
            "WebSocket extensions were not negotiated",
        ));
    }
    let fin = first & 0x80 != 0;
    let opcode = first & 0x0F;
    let control = opcode >= 0x8;
    if control && !fin {
        return Err(WebSocketError::Protocol(
            "control frames cannot be fragmented",
        ));
    }
    if second & 0x80 == 0 {
        return Err(WebSocketError::Protocol(
            "client WebSocket frames must be masked",
        ));
    }
    let mut offset = 2_usize;
    let marker = usize::from(second & 0x7F);
    let length = match marker {
        126 => {
            if buffer.len() < 4 {
                return Ok(None);
            }
            offset = 4;
            usize::from(u16::from_be_bytes([buffer[2], buffer[3]]))
        }
        127 => {
            if buffer.len() < 10 {
                return Ok(None);
            }
            offset = 10;
            usize::try_from(u64::from_be_bytes(
                buffer[2..10].try_into().expect("fixed length"),
            ))
            .map_err(|_| WebSocketError::MessageTooLarge)?
        }
        length => length,
    };
    if control && length > 125 {
        return Err(WebSocketError::Protocol(
            "control frame payload exceeds 125 bytes",
        ));
    }
    if length > max_message_bytes {
        return Err(WebSocketError::MessageTooLarge);
    }
    let frame_bytes = offset
        .checked_add(4)
        .and_then(|value| value.checked_add(length))
        .ok_or(WebSocketError::MessageTooLarge)?;
    if buffer.len() < frame_bytes {
        return Ok(None);
    }
    let mask: [u8; 4] = buffer[offset..offset + 4]
        .try_into()
        .expect("mask length checked");
    offset += 4;
    let mut payload = buffer[offset..frame_bytes].to_vec();
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    buffer.drain(..frame_bytes);
    Ok(Some(Frame {
        fin,
        opcode,
        payload,
    }))
}

fn encode_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len().saturating_add(10));
    frame.push(0x80 | opcode);
    match payload.len() {
        length @ 0..=125 => frame.push(u8::try_from(length).expect("bounded")),
        length @ 126..=65_535 => {
            frame.push(126);
            frame.extend_from_slice(&u16::try_from(length).expect("bounded").to_be_bytes());
        }
        length => {
            frame.push(127);
            frame.extend_from_slice(&u64::try_from(length).unwrap_or(u64::MAX).to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    frame
}

fn message_payload(message: WebSocketMessage) -> Result<(u8, Vec<u8>), WebSocketError> {
    match message {
        WebSocketMessage::Text(text) => Ok((0x1, text.into_bytes())),
        WebSocketMessage::Binary(bytes) => Ok((0x2, bytes)),
        WebSocketMessage::Ping(bytes) if bytes.len() <= 125 => Ok((0x9, bytes)),
        WebSocketMessage::Pong(bytes) if bytes.len() <= 125 => Ok((0xA, bytes)),
        WebSocketMessage::Ping(_) | WebSocketMessage::Pong(_) => Err(WebSocketError::Protocol(
            "control frame payload exceeds 125 bytes",
        )),
        WebSocketMessage::Close(close) => Ok((0x8, encode_close(close)?)),
    }
}

fn data_message(opcode: u8, payload: Vec<u8>) -> Result<WebSocketMessage, WebSocketError> {
    match opcode {
        0x1 => String::from_utf8(payload)
            .map(WebSocketMessage::Text)
            .map_err(|_| WebSocketError::InvalidUtf8),
        0x2 => Ok(WebSocketMessage::Binary(payload)),
        _ => Err(WebSocketError::Protocol("invalid fragmented opcode")),
    }
}

fn encode_close(close: Option<WebSocketClose>) -> Result<Vec<u8>, WebSocketError> {
    let Some(close) = close else {
        return Ok(Vec::new());
    };
    if !valid_close_code(close.code) || close.reason.len() > 123 {
        return Err(WebSocketError::InvalidClose);
    }
    let mut payload = close.code.to_be_bytes().to_vec();
    payload.extend_from_slice(close.reason.as_bytes());
    Ok(payload)
}

fn decode_close(payload: &[u8]) -> Result<Option<WebSocketClose>, WebSocketError> {
    if payload.is_empty() {
        return Ok(None);
    }
    if payload.len() == 1 {
        return Err(WebSocketError::InvalidClose);
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    if !valid_close_code(code) {
        return Err(WebSocketError::InvalidClose);
    }
    let reason = std::str::from_utf8(&payload[2..]).map_err(|_| WebSocketError::InvalidUtf8)?;
    Ok(Some(WebSocketClose {
        code,
        reason: reason.to_owned(),
    }))
}

const fn valid_close_code(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
        && !matches!(code, 1004 | 1005 | 1006 | 1015)
}

fn websocket_accept(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WEBSOCKET_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

fn header(parts: &dyn blazingly_executor::HttpRequestParts, name: &str) -> Option<String> {
    parts
        .value(InputSource::Header, name, 0)
        .map(std::borrow::Cow::into_owned)
}

fn contains_token(value: &str, expected: &str) -> bool {
    value
        .split(',')
        .map(str::trim)
        .any(|token| token.eq_ignore_ascii_case(expected))
}

fn handshake_rejection(code: &'static str, message: &'static str) -> InputRejection {
    InputRejection::new(400, code, message)
}

fn single_line(field: &'static str, value: String) -> Result<String, SseEventError> {
    if value.contains(['\r', '\n']) {
        return Err(SseEventError::InvalidField(field));
    }
    Ok(value)
}

fn normalized_lines(value: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if matches!(bytes[index], b'\r' | b'\n') {
            lines.push(&value[start..index]);
            if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
                index += 1;
            }
            start = index + 1;
        }
        index += 1;
    }
    lines.push(&value[start..]);
    lines
}

fn field(output: &mut Vec<u8>, name: &str, value: &str) {
    output.extend_from_slice(name.as_bytes());
    output.extend_from_slice(b": ");
    output.extend_from_slice(value.as_bytes());
    output.extend_from_slice(b"\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn sse_encodes_multiline_fields_and_keep_alive_comments() {
        let event = SseEvent::data("one\r\ntwo")
            .with_event("update")
            .expect("event")
            .with_id("42")
            .expect("id")
            .with_retry(Duration::from_millis(1500))
            .with_comment("keepalive");
        assert_eq!(
            String::from_utf8(event.encode()).expect("UTF-8"),
            ": keepalive\nevent: update\nid: 42\nretry: 1500\ndata: one\ndata: two\n\n"
        );
    }

    #[test]
    fn websocket_accept_matches_rfc_example() {
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn websocket_decoder_handles_masked_fragmented_text() {
        let mut buffer = Vec::new();
        buffer.extend(masked_frame(false, 0x1, b"hel"));
        buffer.extend(masked_frame(true, 0x0, b"lo"));
        let first = decode_frame(&mut buffer, 1024)
            .expect("frame")
            .expect("complete");
        let second = decode_frame(&mut buffer, 1024)
            .expect("frame")
            .expect("complete");
        assert!(!first.fin);
        assert_eq!(first.payload, b"hel");
        assert!(second.fin);
        assert_eq!(second.payload, b"lo");
    }

    fn masked_frame(final_frame: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [1_u8, 2, 3, 4];
        let mut frame = VecDeque::from([
            (if final_frame { 0x80 } else { 0 }) | opcode,
            0x80 | u8::try_from(payload.len()).expect("small"),
        ]);
        frame.extend(mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        frame.into()
    }
}
