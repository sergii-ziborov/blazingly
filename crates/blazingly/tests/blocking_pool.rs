use blazingly::prelude::*;
use futures_lite::future;
use std::num::NonZeroUsize;

/// The pool is bounded, and saturation is rejected rather than queued forever.
///
/// This test owns the whole binary because it installs a one-worker,
/// one-slot pool process-wide. Anything else scheduled here would be rejected
/// by that configuration rather than by its own behaviour, so the companion
/// contract lives in `sync_handler_path.rs`.
#[test]
fn the_blocking_pool_is_bounded_and_rejects_saturation() {
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
}
