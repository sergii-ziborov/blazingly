use blazingly::prelude::*;
use futures_lite::future;
use std::num::NonZeroUsize;

#[get("/blocking", id = "blocking.run")]
fn blocking_handler() -> Json<String> {
    Json(
        std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned(),
    )
}

#[test]
fn sync_handlers_use_a_bounded_named_worker_pool() {
    install_global_blocking_pool(BlockingPoolConfig::new(
        NonZeroUsize::MIN,
        NonZeroUsize::MIN,
    ))
    .expect("test owns its process-wide blocking pool");

    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let (started_sender, started_receiver) = std::sync::mpsc::channel();
    let first = blazingly::run_blocking(move || {
        started_sender.send(()).expect("signal first task");
        release_receiver.recv().expect("release first task");
    });
    started_receiver.recv().expect("first task started");
    let second = blazingly::run_blocking(|| 2_u8);
    let third = blazingly::run_blocking(|| 3_u8);
    assert_eq!(
        future::block_on(third),
        Err(BlockingError::Saturated),
        "the worker and its one queued slot are occupied"
    );
    release_sender.send(()).expect("release worker");
    future::block_on(first).expect("first task");
    assert_eq!(future::block_on(second), Ok(2));

    let executable = ExecutableApp::new(routes![blocking_handler]).expect("blocking route");
    let response = future::block_on(TestApp::new(&executable).call(Request::get("/blocking")));
    assert_eq!(response.status(), 200);
    let worker_name: String = response.json().expect("worker name");
    assert!(worker_name.starts_with("blazingly-blocking-"));
}
