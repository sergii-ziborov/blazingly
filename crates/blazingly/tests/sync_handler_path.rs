use blazingly::prelude::*;
use futures_lite::future;

#[get("/direct", id = "sync.direct")]
fn direct_handler() -> Json<String> {
    Json(current_thread_name())
}

#[get("/offloaded", id = "sync.offloaded")]
async fn offloaded_handler() -> Json<String> {
    blazingly::run_blocking(current_thread_name)
        .await
        .map_or_else(|error| Json(format!("failed: {error}")), Json)
}

fn current_thread_name() -> String {
    std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_owned()
}

/// A synchronous handler runs inline, and offloading is an explicit choice.
///
/// This is the documented performance contract: a handler that is not `async`
/// completes without allocating a boxed future and without a hop through the
/// blocking pool. The framework cannot tell a CPU-bound sync handler from one
/// that opens a socket, so it does not guess: work that genuinely blocks says
/// so by calling [`run_blocking`], which is what `blazingly-database` does for
/// every query it hands to a synchronous driver.
///
/// The two assertions are a pair on purpose. Together they pin both halves, so
/// a later change cannot quietly move sync handlers back onto the pool, nor
/// drop the pool that explicit offloading depends on.
#[test]
fn a_sync_handler_runs_inline_while_run_blocking_still_reaches_the_pool() {
    let executable = ExecutableApp::new(routes![direct_handler, offloaded_handler])
        .expect("sync path routes should compile");
    let app = TestApp::new(&executable);

    let direct: String = future::block_on(app.call(Request::get("/direct")))
        .json()
        .expect("worker name");
    assert!(
        !direct.starts_with("blazingly-blocking-"),
        "a sync handler must not be offloaded to the blocking pool, ran on {direct}"
    );

    let offloaded: String = future::block_on(app.call(Request::get("/offloaded")))
        .json()
        .expect("worker name");
    assert!(
        offloaded.starts_with("blazingly-blocking-"),
        "run_blocking must still reach a named pool worker, ran on {offloaded}"
    );
}
