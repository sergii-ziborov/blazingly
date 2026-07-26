use blazingly::prelude::*;
use futures_lite::future;
use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

#[api_model]
#[derive(Clone, Debug)]
struct ControlledView {
    value: String,
}

struct ControlledResource;

#[get("/controlled", id = "controlled.read")]
async fn controlled(_resource: Depends<ControlledResource>) -> Json<ControlledView> {
    std::future::pending::<Json<ControlledView>>().await
}

#[test]
fn controlled_invocations_distinguish_cancellation_and_timeout_and_finalize() {
    let finalizers = Rc::new(Cell::new(0_u32));
    let finalizer_probe = Rc::clone(&finalizers);
    let executable = ExecutableApp::from_plugin(
        Plugin::new("controlled")
            .provide(Provider::request_scoped(
                || ControlledResource,
                move |_resource: Depends<ControlledResource>| {
                    finalizer_probe.set(finalizer_probe.get() + 1);
                },
            ))
            .routes(routes![controlled]),
    )
    .expect("controlled graph should compile");
    let http = TestApp::new(&executable);

    let token = CancellationToken::new();
    token.cancel();
    let cancelled = future::block_on(http.call_controlled(
        Request::get("/controlled"),
        InvocationControl::new().with_cancellation(token),
    ));
    assert_error(&cancelled, 499, "invocation_cancelled");
    assert_eq!(finalizers.get(), 0);

    // The first five polls let on_request, provider resolution, pre_parse,
    // pre_validate, and pre_handler finish. The sixth poll aborts the pending
    // handler.
    let timed_out = future::block_on(http.call_controlled(
        Request::get("/controlled"),
        InvocationControl::new().with_timeout(PollAfter::new(5)),
    ));
    assert_error(&timed_out, 504, "invocation_timeout");
    assert_eq!(finalizers.get(), 1);
}

struct PollAfter {
    pending_polls: usize,
}

impl PollAfter {
    const fn new(pending_polls: usize) -> Self {
        Self { pending_polls }
    }
}

impl Future for PollAfter {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.pending_polls == 0 {
            return Poll::Ready(());
        }
        self.pending_polls -= 1;
        context.waker().wake_by_ref();
        Poll::Pending
    }
}

fn assert_error(response: &Response, status: u16, code: &str) {
    assert_eq!(response.status(), status);
    assert_eq!(
        response
            .json::<serde_json::Value>()
            .expect("controlled error should be JSON")["error"]["code"],
        code
    );
}
