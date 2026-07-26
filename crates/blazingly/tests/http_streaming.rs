use blazingly::prelude::*;
use futures_lite::future;
use std::cell::Cell;
use std::collections::VecDeque;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

#[derive(Clone)]
struct DemandProbe(Rc<Cell<usize>>);

struct DemandStream {
    chunks: VecDeque<Vec<u8>>,
    polls: Rc<Cell<usize>>,
}

impl BodyStream for DemandStream {
    fn poll_next(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Vec<u8>, BodyStreamError>>> {
        let stream = self.get_mut();
        stream.polls.set(stream.polls.get() + 1);
        Poll::Ready(stream.chunks.pop_front().map(Ok))
    }
}

#[get("/stream", id = "stream.read")]
#[allow(clippy::unused_async)]
async fn stream(probe: Depends<DemandProbe>) -> WithHeaders<StreamingBody> {
    StreamingBody::new(DemandStream {
        chunks: VecDeque::from([b"blazing".to_vec(), b"ly".to_vec()]),
        polls: Rc::clone(&probe.0),
    })
    .header("content-type", "text/plain")
}

#[test]
fn response_streams_are_pull_based_and_can_be_bounded_in_tests() {
    let polls = Rc::new(Cell::new(0));
    let executable = ExecutableApp::from_plugin(
        Plugin::new("streaming")
            .provide(Provider::value(DemandProbe(Rc::clone(&polls))))
            .routes(routes![stream]),
    )
    .expect("streaming operation should compile");
    let app = TestApp::new(&executable);

    let mut response = future::block_on(app.call(Request::get("/stream")));
    assert_eq!(response.status(), 200);
    assert_eq!(response.get_header("content-type"), Some("text/plain"));
    assert!(response.is_streaming());
    assert_eq!(response.exact_body_length(), None);
    assert_eq!(polls.get(), 0, "dispatch must not prefetch response chunks");

    let first = future::block_on(response.next_body_chunk())
        .expect("first chunk")
        .expect("producer should succeed");
    assert_eq!(first, b"blazing");
    assert_eq!(polls.get(), 1);

    let second = future::block_on(response.next_body_chunk())
        .expect("second chunk")
        .expect("producer should succeed");
    assert_eq!(second, b"ly");
    assert_eq!(polls.get(), 2);

    assert!(future::block_on(response.next_body_chunk()).is_none());
    assert_eq!(polls.get(), 3);

    let response = future::block_on(app.call(Request::get("/stream")));
    let body = future::block_on(response.collect_body(9)).expect("bounded collection should work");
    assert_eq!(body, b"blazingly");

    let response = future::block_on(app.call(Request::get("/stream")));
    let error = future::block_on(response.collect_body(8)).expect_err("limit must stop collection");
    assert_eq!(error, CollectBodyError::LimitExceeded { limit: 8 });
}
