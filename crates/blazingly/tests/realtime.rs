#![cfg(feature = "realtime")]

use blazingly::prelude::*;
use futures_lite::future;

#[get("/events", id = "events.stream")]
#[allow(clippy::unused_async)]
async fn events() -> Sse {
    Sse::from_events([
        SseEvent::data("ready")
            .with_event("state")
            .expect("static event type")
            .with_id("1")
            .expect("static event ID"),
        SseEvent::keep_alive("keepalive"),
    ])
}

#[test]
fn sse_uses_typed_routing_and_pull_based_event_framing() {
    let executable = ExecutableApp::new(routes![events]).expect("realtime app");
    let app = TestApp::new(&executable);
    let response = future::block_on(app.call(Request::get("/events")));
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.get_header("content-type"),
        Some("text/event-stream; charset=utf-8")
    );
    assert_eq!(response.get_header("cache-control"), Some("no-cache"));
    let body = future::block_on(response.collect_body(1024)).expect("bounded SSE body");
    assert_eq!(
        String::from_utf8(body).expect("UTF-8"),
        "event: state\nid: 1\ndata: ready\n\n: keepalive\n\n"
    );
}

#[cfg(feature = "native")]
#[get("/socket", id = "socket.connect")]
#[allow(clippy::unused_async)]
async fn socket(request: WebSocketRequest) -> WebSocketUpgrade {
    request.on_upgrade(|mut socket| async move {
        if let Some(WebSocketMessage::Text(text)) = socket.receive().await? {
            socket
                .send(WebSocketMessage::Text(format!("echo:{text}")))
                .await?;
        }
        socket
            .close(Some(WebSocketClose {
                code: 1000,
                reason: "complete".to_owned(),
            }))
            .await
    })
}

#[cfg(feature = "native")]
#[test]
fn native_websocket_upgrade_preserves_coalesced_frame_bytes() {
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    let probe = TcpListener::bind("127.0.0.1:0").expect("probe");
    let address = probe.local_addr().expect("address");
    drop(probe);

    let (shutdown, signal) = blazingly::native::shutdown_channel();
    let server = std::thread::spawn(move || {
        let executable = ExecutableApp::new(routes![socket]).expect("websocket app");
        blazingly::native::Server::new(executable).serve_gracefully(
            address,
            signal,
            Duration::from_secs(2),
        )
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut client = loop {
        match TcpStream::connect(address) {
            Ok(stream) => break stream,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                std::thread::yield_now();
            }
            Err(error) => panic!("server did not start: {error}"),
        }
    };
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");

    let mut request = concat!(
        "GET /socket HTTP/1.1\r\n",
        "host: localhost\r\n",
        "upgrade: websocket\r\n",
        "connection: Upgrade\r\n",
        "sec-websocket-version: 13\r\n",
        "sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
    )
    .as_bytes()
    .to_vec();
    request.extend(masked_text_frame("hello"));
    client.write_all(&request).expect("request and first frame");

    let mut response = Vec::new();
    client
        .read_to_end(&mut response)
        .expect("upgraded response");
    shutdown.shutdown();
    server
        .join()
        .expect("server thread")
        .expect("graceful server");

    let head_end = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .map(|index| index + 4)
        .expect("response head");
    let head = std::str::from_utf8(&response[..head_end]).expect("head UTF-8");
    assert!(head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
    assert!(head.contains("sec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));

    let frames = &response[head_end..];
    assert_eq!(frames[0], 0x81);
    let text_length = usize::from(frames[1] & 0x7F);
    assert_eq!(&frames[2..2 + text_length], b"echo:hello");
    assert_eq!(frames[2 + text_length], 0x88);
}

#[cfg(feature = "native")]
fn masked_text_frame(text: &str) -> Vec<u8> {
    let mask = [7_u8, 11, 13, 17];
    let mut frame = vec![
        0x81,
        0x80 | u8::try_from(text.len()).expect("small test frame"),
    ];
    frame.extend_from_slice(&mask);
    frame.extend(
        text.as_bytes()
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    frame
}
