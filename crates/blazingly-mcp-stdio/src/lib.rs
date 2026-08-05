#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

use blazingly_mcp::JsonRpcServer;
use std::future::Future;
use std::io::{self, BufRead, Write};
use std::num::NonZeroU64;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

const MESSAGE_TOO_LARGE: &str = concat!(
    r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"MCP message exceeds "#,
    r#"the configured size limit"}}"#
);
const INVALID_UTF8: &str = concat!(
    r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"MCP message must be "#,
    r#"valid UTF-8"}}"#
);

/// Resource limits for a stdio MCP connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StdioConfig {
    pub max_message_bytes: usize,
    pub max_messages: Option<NonZeroU64>,
    pub max_rejected_frames: Option<NonZeroU64>,
}

impl Default for StdioConfig {
    fn default() -> Self {
        Self {
            max_message_bytes: 1024 * 1024,
            max_messages: None,
            max_rejected_frames: NonZeroU64::new(32),
        }
    }
}

/// Thread-safe request to stop a supervised stdio loop between frames.
#[derive(Clone, Default)]
pub struct StdioSupervisor {
    stopped: Arc<AtomicBool>,
}

impl StdioSupervisor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

/// Why a supervised stdio connection stopped accepting messages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StdioTermination {
    EndOfInput,
    Supervisor,
    MessageLimit,
    RejectedFrameLimit,
}

/// Counters returned when a supervised stdio connection ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StdioReport {
    pub messages_read: u64,
    pub responses_written: u64,
    pub notifications: u64,
    pub rejected_frames: u64,
    pub termination: StdioTermination,
}

/// Serves MCP over the process standard input and output streams.
///
/// # Errors
///
/// Returns an I/O error when stdin cannot be read or stdout cannot be written.
pub fn serve_stdio(server: &mut JsonRpcServer<'_>) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve(server, stdin.lock(), stdout.lock(), StdioConfig::default())
}

/// Serves newline-delimited MCP messages over caller-provided streams.
///
/// # Errors
///
/// Returns an I/O error when the input cannot be read or the output cannot be
/// written.
pub fn serve(
    server: &mut JsonRpcServer<'_>,
    reader: impl BufRead,
    writer: impl Write,
    config: StdioConfig,
) -> io::Result<()> {
    serve_supervised(server, reader, writer, config, &StdioSupervisor::new()).map(|_| ())
}

/// Serves stdio with bounded abuse handling and cooperative shutdown.
///
/// A supervisor is checked between frames. A blocking `BufRead` implementation
/// still owns how an in-progress read is interrupted; process supervisors
/// should close stdin to wake such readers immediately.
///
/// # Errors
///
/// Returns an I/O error when input cannot be read or output cannot be written.
pub fn serve_supervised(
    server: &mut JsonRpcServer<'_>,
    mut reader: impl BufRead,
    mut writer: impl Write,
    config: StdioConfig,
    supervisor: &StdioSupervisor,
) -> io::Result<StdioReport> {
    let mut report = StdioReport {
        messages_read: 0,
        responses_written: 0,
        notifications: 0,
        rejected_frames: 0,
        termination: StdioTermination::EndOfInput,
    };
    loop {
        if supervisor.is_stopped() {
            report.termination = StdioTermination::Supervisor;
            return Ok(report);
        }
        if config
            .max_messages
            .is_some_and(|maximum| report.messages_read >= maximum.get())
        {
            report.termination = StdioTermination::MessageLimit;
            return Ok(report);
        }
        let Some(frame) = read_frame(&mut reader, config.max_message_bytes)? else {
            report.termination = StdioTermination::EndOfInput;
            return Ok(report);
        };
        report.messages_read += 1;
        let rejected = !matches!(&frame, Frame::Message(_));
        let response = match frame {
            Frame::Message(message) => block_on(server.handle_line(&message)),
            Frame::TooLarge => Some(MESSAGE_TOO_LARGE.to_owned()),
            Frame::InvalidUtf8 => Some(INVALID_UTF8.to_owned()),
        };
        if rejected {
            report.rejected_frames += 1;
        }
        if let Some(response) = response {
            writer.write_all(response.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            report.responses_written += 1;
        } else {
            report.notifications += 1;
        }
        if config
            .max_rejected_frames
            .is_some_and(|maximum| report.rejected_frames >= maximum.get())
        {
            report.termination = StdioTermination::RejectedFrameLimit;
            return Ok(report);
        }
    }
}

enum Frame {
    Message(String),
    TooLarge,
    InvalidUtf8,
}

fn read_frame(reader: &mut impl BufRead, maximum: usize) -> io::Result<Option<Frame>> {
    let mut bytes = Vec::new();
    let mut too_large = false;
    let mut received_anything = false;

    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            if !received_anything {
                return Ok(None);
            }
            break;
        }
        received_anything = true;
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |position| position + 1);

        if !too_large {
            if bytes.len().saturating_add(consumed) > maximum.saturating_add(2) {
                too_large = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&buffer[..consumed]);
            }
        }
        reader.consume(consumed);
        if newline.is_some() {
            break;
        }
    }

    if too_large {
        return Ok(Some(Frame::TooLarge));
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    if bytes.len() > maximum {
        return Ok(Some(Frame::TooLarge));
    }

    Ok(Some(match String::from_utf8(bytes) {
        Ok(mut message) => {
            if message.starts_with('\u{feff}') {
                message.drain(..'\u{feff}'.len_utf8());
            }
            Frame::Message(message)
        }
        Err(_) => Frame::InvalidUtf8,
    }))
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);

    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

#[cfg(test)]
mod tests {
    use super::{StdioConfig, StdioSupervisor, StdioTermination, serve, serve_supervised};
    use blazingly_executor::{ExecutableApp, ExecutableOperation};
    use blazingly_json::Value;
    use blazingly_mcp::JsonRpcServer;
    use std::io::Cursor;
    use std::num::NonZeroU64;

    #[test]
    fn stdio_preserves_lifecycle_and_writes_only_json_rpc_responses() {
        let app = ExecutableApp::new(Vec::<ExecutableOperation>::new())
            .expect("empty applications are valid");
        let mut server = JsonRpcServer::new(&app);
        let input = concat!(
            "\u{feff}",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":",
            "{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},",
            "\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":\"ping-1\",\"method\":\"ping\"}\n"
        );
        let mut output = Vec::new();

        serve(
            &mut server,
            Cursor::new(input.as_bytes()),
            &mut output,
            StdioConfig::default(),
        )
        .expect("in-memory stdio should succeed");

        let output = String::from_utf8(output).expect("responses are UTF-8");
        let responses = output
            .lines()
            .map(|line| blazingly_json::from_str::<Value>(line).expect("stdout contains JSON only"))
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(responses[1]["id"], "ping-1");
        assert_eq!(responses[1]["result"], blazingly_json::json!({}));
    }

    #[test]
    fn stdio_rejects_oversized_messages_and_continues() {
        let app = ExecutableApp::new(Vec::<ExecutableOperation>::new())
            .expect("empty applications are valid");
        let mut server = JsonRpcServer::new(&app);
        let input = concat!(
            "0123456789012345678901234567890123456789012345678901234567890123456789\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n"
        );
        let mut output = Vec::new();

        serve(
            &mut server,
            Cursor::new(input.as_bytes()),
            &mut output,
            StdioConfig {
                max_message_bytes: 64,
                ..StdioConfig::default()
            },
        )
        .expect("in-memory stdio should succeed");

        let output = String::from_utf8(output).expect("responses are UTF-8");
        let responses = output.lines().collect::<Vec<_>>();
        assert_eq!(responses.len(), 2);
        assert!(responses[0].contains("size limit"));
        assert!(responses[1].contains("\"id\":2"));
    }

    #[test]
    fn supervised_stdio_reports_limits_and_cooperative_stop() {
        let app = ExecutableApp::new(Vec::<ExecutableOperation>::new())
            .expect("empty applications are valid");
        let mut server = JsonRpcServer::new(&app);
        let input = concat!(
            "oversized-frame\n",
            "another-oversized-frame\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n"
        );
        let mut output = Vec::new();
        let report = serve_supervised(
            &mut server,
            Cursor::new(input.as_bytes()),
            &mut output,
            StdioConfig {
                max_message_bytes: 8,
                max_messages: None,
                max_rejected_frames: NonZeroU64::new(2),
            },
            &StdioSupervisor::new(),
        )
        .expect("bounded stdio should stop normally");
        assert_eq!(report.messages_read, 2);
        assert_eq!(report.rejected_frames, 2);
        assert_eq!(report.termination, StdioTermination::RejectedFrameLimit);

        let supervisor = StdioSupervisor::new();
        supervisor.stop();
        let report = serve_supervised(
            &mut server,
            Cursor::new(b""),
            Vec::new(),
            StdioConfig::default(),
            &supervisor,
        )
        .expect("pre-stopped supervision should not read");
        assert_eq!(report.termination, StdioTermination::Supervisor);
        assert_eq!(report.messages_read, 0);
    }
}
