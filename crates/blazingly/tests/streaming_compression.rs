#![cfg(all(feature = "realtime", feature = "middleware"))]

//! Compression must reach streamed responses, not only buffered ones.
//!
//! Server-Sent Events are the case that matters: the body is `text/event-stream`,
//! it is highly compressible, and it is produced incrementally, so an encoder
//! that only runs over a fully buffered body never touches it.

use blazingly::prelude::*;
use flate2::read::GzDecoder;
use futures_lite::future;
use std::io::Read as _;

#[get("/events", id = "compression.events")]
#[allow(clippy::unused_async)]
async fn events() -> Sse {
    Sse::from_events([
        SseEvent::data("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        SseEvent::data("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
    ])
}

fn app() -> ExecutableApp {
    ExecutableApp::new(routes![events]).expect("streaming compression app")
}

#[test]
fn a_streamed_response_is_compressed_and_stays_streamed() {
    let executable = app();
    let test_app = TestApp::new(&executable).with_middleware(Compression::new());
    let response =
        future::block_on(test_app.call(Request::get("/events").header("accept-encoding", "gzip")));

    assert_eq!(response.status(), 200);
    assert_eq!(response.get_header("content-encoding"), Some("gzip"));
    // An encoded stream has no length known in advance.
    assert_eq!(response.get_header("content-length"), None);
    assert!(
        response
            .get_header("vary")
            .is_some_and(|value| value.to_ascii_lowercase().contains("accept-encoding"))
    );
    assert!(
        response.is_streaming(),
        "compression must wrap the pull stream, not buffer it"
    );

    let body = future::block_on(response.collect_body(64 * 1024)).expect("bounded body");
    let mut decoded = String::new();
    GzDecoder::new(body.as_slice())
        .read_to_string(&mut decoded)
        .expect("gzip stream decodes");
    assert_eq!(
        decoded,
        "data: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\n\
         data: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\n"
    );
    assert!(
        body.len() < decoded.len(),
        "compressed stream should be smaller than its plaintext: {} vs {}",
        body.len(),
        decoded.len()
    );
}

#[test]
fn a_client_that_accepts_nothing_still_gets_the_plain_stream() {
    let executable = app();
    let test_app = TestApp::new(&executable).with_middleware(Compression::new());
    let response = future::block_on(test_app.call(Request::get("/events")));

    assert_eq!(response.status(), 200);
    assert_eq!(response.get_header("content-encoding"), None);
    let body = future::block_on(response.collect_body(64 * 1024)).expect("bounded body");
    assert!(
        String::from_utf8(body)
            .expect("UTF-8")
            .starts_with("data: a")
    );
}
