//! Blocking HTTP/1.1 transport used to POST OTLP payloads.

use async_trait::async_trait;
use http::{HeaderMap, Uri};
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Largest response body accepted from a collector.
const MAX_RESPONSE_BYTES: usize = 1 << 20;
const READ_CHUNK_BYTES: usize = 8 * 1024;
const HEAD_TERMINATOR: &[u8] = b"\r\n\r\n";

/// Headers this client writes itself, so they are never copied from the request.
const RESERVED_HEADERS: [&str; 4] = ["host", "content-length", "connection", "transfer-encoding"];

/// Failure while exchanging an OTLP payload with a collector.
#[derive(Debug)]
pub enum TransportError {
    /// The endpoint is not a plain `http://` URL carrying a host.
    UnsupportedEndpoint(String),
    /// The collector could not be reached, or the exchange timed out.
    Io(std::io::Error),
    /// The collector closed the connection before sending a complete response.
    IncompleteResponse,
    /// The response head or chunked body could not be parsed.
    MalformedResponse,
    /// The response exceeded the in-memory cap of one MiB.
    ResponseTooLarge,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedEndpoint(endpoint) => {
                write!(formatter, "endpoint `{endpoint}` is not a plain http URL")
            }
            Self::Io(error) => write!(formatter, "OTLP transport I/O failed: {error}"),
            Self::IncompleteResponse => formatter.write_str("collector closed mid-response"),
            Self::MalformedResponse => formatter.write_str("collector sent a malformed response"),
            Self::ResponseTooLarge => formatter.write_str("collector response exceeded 1 MiB"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TransportError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Blocking HTTP/1.1 client that POSTs OTLP payloads without an async runtime.
///
/// Each call opens a connection, writes the request, reads the response, and
/// closes the connection, all on the calling thread. That is safe under the SDK
/// batch span processor, which owns a dedicated export thread and blocks on the
/// export future there. TLS, proxies, redirects, and connection reuse are out of
/// scope; supply your own [`HttpClient`] through
/// [`install_with_client`](super::install_with_client) when you need them.
#[derive(Clone, Copy, Debug)]
pub struct BlockingHttpClient {
    timeout: Duration,
}

impl BlockingHttpClient {
    /// Creates a client that bounds connect, write, and read on `timeout`.
    #[must_use]
    pub const fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// Per-operation timeout applied to connect, write, and read.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    fn post(&self, request: Request<Bytes>) -> Result<Response<Bytes>, TransportError> {
        let (parts, body) = request.into_parts();
        let target = Endpoint::parse(&parts.uri)?;
        let mut stream = self.connect(&target)?;
        stream.write_all(&request_head(&target, &parts.headers, body.len()))?;
        stream.write_all(&body)?;
        stream.flush()?;

        let (head, body) = read_response(&mut stream)?;
        // The exporter classifies retries off the status and `retry-after`, so
        // a non-2xx answer is forwarded rather than turned into a transport
        // error here.
        let mut response = Response::builder().status(head.status);
        for (name, value) in &head.headers {
            response = response.header(name.as_str(), value.as_slice());
        }
        response
            .body(body)
            .map_err(|_| TransportError::MalformedResponse)
    }

    fn connect(&self, target: &Endpoint) -> Result<TcpStream, TransportError> {
        let addresses = (target.host.as_str(), target.port).to_socket_addrs()?;
        let mut last = None;
        for address in addresses {
            match TcpStream::connect_timeout(&address, self.timeout) {
                Ok(stream) => {
                    stream.set_read_timeout(Some(self.timeout))?;
                    stream.set_write_timeout(Some(self.timeout))?;
                    stream.set_nodelay(true)?;
                    return Ok(stream);
                }
                Err(error) => last = Some(error),
            }
        }
        Err(TransportError::Io(last.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                format!("`{}` resolved to no address", target.authority),
            )
        })))
    }
}

impl Default for BlockingHttpClient {
    fn default() -> Self {
        Self::new(super::DEFAULT_EXPORT_TIMEOUT)
    }
}

#[async_trait]
impl HttpClient for BlockingHttpClient {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        self.post(request).map_err(Into::into)
    }
}

/// Plain-HTTP destination parsed out of an OTLP endpoint URL.
#[derive(Debug, Eq, PartialEq)]
struct Endpoint {
    authority: String,
    host: String,
    port: u16,
    path: String,
}

impl Endpoint {
    fn parse(uri: &Uri) -> Result<Self, TransportError> {
        let unsupported = || TransportError::UnsupportedEndpoint(uri.to_string());
        if uri.scheme_str() != Some("http") {
            return Err(unsupported());
        }
        let host = uri.host().ok_or_else(unsupported)?;
        let port = uri.port_u16().unwrap_or(80);
        let authority = uri.authority().ok_or_else(unsupported)?.as_str();
        Ok(Self {
            authority: authority.to_owned(),
            host: host.to_owned(),
            port,
            path: uri
                .path_and_query()
                .map_or_else(|| "/".to_owned(), |target| target.as_str().to_owned()),
        })
    }
}

fn request_head(target: &Endpoint, headers: &HeaderMap, body_length: usize) -> Vec<u8> {
    let mut head = Vec::with_capacity(256);
    head.extend_from_slice(b"POST ");
    head.extend_from_slice(target.path.as_bytes());
    head.extend_from_slice(b" HTTP/1.1\r\nhost: ");
    head.extend_from_slice(target.authority.as_bytes());
    head.extend_from_slice(b"\r\n");
    for (name, value) in headers {
        if RESERVED_HEADERS.contains(&name.as_str()) {
            continue;
        }
        head.extend_from_slice(name.as_str().as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"content-length: ");
    head.extend_from_slice(body_length.to_string().as_bytes());
    head.extend_from_slice(b"\r\nconnection: close\r\n\r\n");
    head
}

/// Status line and headers of a collector response.
#[derive(Debug, Eq, PartialEq)]
struct ResponseHead {
    status: u16,
    headers: Vec<(String, Vec<u8>)>,
    content_length: Option<usize>,
    chunked: bool,
}

fn read_response(stream: &mut TcpStream) -> Result<(ResponseHead, Bytes), TransportError> {
    let mut raw = Vec::new();
    let head_end = loop {
        if let Some(position) = find_sequence(&raw, HEAD_TERMINATOR) {
            break position;
        }
        if !read_more(stream, &mut raw)? {
            return Err(TransportError::IncompleteResponse);
        }
    };
    let head = parse_head(&raw[..head_end]).ok_or(TransportError::MalformedResponse)?;
    let start = head_end + HEAD_TERMINATOR.len();

    let body = if head.chunked {
        loop {
            match decode_chunked(&raw[start..]) {
                ChunkedBody::Done(body) => break body,
                ChunkedBody::Malformed => return Err(TransportError::MalformedResponse),
                ChunkedBody::Incomplete => {
                    if !read_more(stream, &mut raw)? {
                        return Err(TransportError::IncompleteResponse);
                    }
                }
            }
        }
    } else if let Some(length) = head.content_length {
        while raw.len() - start < length {
            if !read_more(stream, &mut raw)? {
                return Err(TransportError::IncompleteResponse);
            }
        }
        raw[start..start + length].to_vec()
    } else {
        while read_more(stream, &mut raw)? {}
        raw[start..].to_vec()
    };

    Ok((head, Bytes::from(body)))
}

/// Appends one read to `raw`, reporting whether the peer is still sending.
fn read_more(stream: &mut TcpStream, raw: &mut Vec<u8>) -> Result<bool, TransportError> {
    let mut buffer = [0_u8; READ_CHUNK_BYTES];
    let read = stream.read(&mut buffer)?;
    if read == 0 {
        return Ok(false);
    }
    if raw.len() + read > MAX_RESPONSE_BYTES {
        return Err(TransportError::ResponseTooLarge);
    }
    raw.extend_from_slice(&buffer[..read]);
    Ok(true)
}

fn parse_head(head: &[u8]) -> Option<ResponseHead> {
    let mut lines = head.split(|byte| *byte == b'\n');
    let status = parse_status_line(lines.next()?)?;
    let mut headers = Vec::new();
    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let line = strip_carriage_return(line);
        if line.is_empty() {
            continue;
        }
        let separator = line.iter().position(|byte| *byte == b':')?;
        let name = std::str::from_utf8(&line[..separator]).ok()?.trim();
        if name.is_empty() {
            return None;
        }
        let name = name.to_ascii_lowercase();
        let value = trim_ascii(&line[separator + 1..]);
        match name.as_str() {
            "content-length" => {
                content_length = Some(std::str::from_utf8(value).ok()?.trim().parse().ok()?);
            }
            "transfer-encoding" => {
                chunked = std::str::from_utf8(value)
                    .ok()?
                    .to_ascii_lowercase()
                    .split(',')
                    .any(|encoding| encoding.trim() == "chunked");
            }
            _ => {}
        }
        headers.push((name, value.to_vec()));
    }
    Some(ResponseHead {
        status,
        headers,
        // A chunked body has no meaningful `content-length`.
        content_length: if chunked { None } else { content_length },
        chunked,
    })
}

fn parse_status_line(line: &[u8]) -> Option<u16> {
    let line = std::str::from_utf8(strip_carriage_return(line)).ok()?;
    let mut fields = line.split(' ');
    let version = fields.next()?;
    if !version.starts_with("HTTP/1.") {
        return None;
    }
    let status: u16 = fields.next()?.parse().ok()?;
    (100..=599).contains(&status).then_some(status)
}

/// Outcome of decoding a chunked body prefix.
#[derive(Debug, Eq, PartialEq)]
enum ChunkedBody {
    Done(Vec<u8>),
    Incomplete,
    Malformed,
}

fn decode_chunked(raw: &[u8]) -> ChunkedBody {
    let mut decoded = Vec::new();
    let mut cursor = 0;
    loop {
        let Some(line_end) = find_sequence(&raw[cursor..], b"\r\n") else {
            return ChunkedBody::Incomplete;
        };
        let line = &raw[cursor..cursor + line_end];
        let size_field = line.split(|byte| *byte == b';').next().unwrap_or(line);
        let Ok(size_field) = std::str::from_utf8(size_field) else {
            return ChunkedBody::Malformed;
        };
        let Ok(size) = usize::from_str_radix(size_field.trim(), 16) else {
            return ChunkedBody::Malformed;
        };
        cursor += line_end + 2;
        if size == 0 {
            // Trailers are accepted but not surfaced; OTLP does not use them.
            return ChunkedBody::Done(decoded);
        }
        if decoded.len() + size > MAX_RESPONSE_BYTES {
            return ChunkedBody::Malformed;
        }
        if raw.len() < cursor + size + 2 {
            return ChunkedBody::Incomplete;
        }
        decoded.extend_from_slice(&raw[cursor..cursor + size]);
        cursor += size + 2;
    }
}

fn find_sequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn strip_carriage_return(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn trim_ascii(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |position| position + 1);
    &value[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_must_be_plain_http_with_a_host() {
        let target = Endpoint::parse(&"http://collector:4318/v1/traces".parse().expect("uri"))
            .expect("plain http endpoint");
        assert_eq!(target.host, "collector");
        assert_eq!(target.port, 4318);
        assert_eq!(target.path, "/v1/traces");
        assert_eq!(target.authority, "collector:4318");

        let default_port =
            Endpoint::parse(&"http://collector/v1/traces".parse().expect("uri")).expect("endpoint");
        assert_eq!(default_port.port, 80);

        // TLS needs a caller-supplied client, so https is rejected up front
        // rather than silently sending plaintext.
        assert!(matches!(
            Endpoint::parse(&"https://collector:4318/v1/traces".parse().expect("uri")),
            Err(TransportError::UnsupportedEndpoint(_))
        ));
    }

    #[test]
    fn request_head_writes_one_authoritative_framing() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/x-protobuf".parse().expect("v"));
        headers.insert("host", "spoofed".parse().expect("v"));
        headers.insert("content-length", "999".parse().expect("v"));
        let target = Endpoint::parse(&"http://collector:4318/v1/traces".parse().expect("uri"))
            .expect("endpoint");

        let head = String::from_utf8(request_head(&target, &headers, 7)).expect("ascii head");
        assert!(head.starts_with("POST /v1/traces HTTP/1.1\r\n"));
        assert_eq!(head.matches("host: ").count(), 1);
        assert!(head.contains("host: collector:4318\r\n"));
        assert_eq!(head.matches("content-length: ").count(), 1);
        assert!(head.contains("content-length: 7\r\n"));
        assert!(head.contains("content-type: application/x-protobuf\r\n"));
        assert!(head.contains("connection: close\r\n"));
        assert!(!head.contains("spoofed"));
        assert!(head.ends_with("\r\n\r\n"));
    }

    #[test]
    fn response_heads_are_parsed_with_framing_metadata() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nRetry-After: 3")
            .expect("valid head");
        assert_eq!(head.status, 200);
        assert_eq!(head.content_length, Some(12));
        assert!(!head.chunked);
        assert_eq!(
            head.headers,
            vec![
                ("content-length".to_owned(), b"12".to_vec()),
                ("retry-after".to_owned(), b"3".to_vec()),
            ]
        );

        // Chunked framing wins: a body length would otherwise truncate it.
        let chunked =
            parse_head(b"HTTP/1.1 429 Too Many\r\nTransfer-Encoding: chunked\r\nContent-Length: 4")
                .expect("valid head");
        assert_eq!(chunked.status, 429);
        assert!(chunked.chunked);
        assert_eq!(chunked.content_length, None);

        assert!(parse_head(b"HTTP/2 200 OK").is_none());
        assert!(parse_head(b"HTTP/1.1 999 Nope").is_none());
        assert!(parse_head(b"HTTP/1.1 200 OK\r\nnocolon").is_none());
    }

    #[test]
    fn chunked_bodies_decode_only_once_complete() {
        assert_eq!(decode_chunked(b"5\r\nhello"), ChunkedBody::Incomplete);
        assert_eq!(decode_chunked(b"5\r\nhello\r\n"), ChunkedBody::Incomplete);
        assert_eq!(
            decode_chunked(b"5\r\nhello\r\n2\r\n!!\r\n0\r\n\r\n"),
            ChunkedBody::Done(b"hello!!".to_vec())
        );
        assert_eq!(
            decode_chunked(b"5;name=value\r\nhello\r\n0\r\n\r\n"),
            ChunkedBody::Done(b"hello".to_vec())
        );
        assert_eq!(decode_chunked(b"zz\r\n"), ChunkedBody::Malformed);
    }

    #[test]
    fn head_terminator_and_trimming_are_byte_exact() {
        assert_eq!(find_sequence(b"ab\r\n\r\ncd", HEAD_TERMINATOR), Some(2));
        assert_eq!(find_sequence(b"ab\r\ncd", HEAD_TERMINATOR), None);
        assert_eq!(trim_ascii(b"  value \t"), b"value");
        assert_eq!(trim_ascii(b"   "), b"");
        assert_eq!(strip_carriage_return(b"line\r"), b"line");
    }
}
