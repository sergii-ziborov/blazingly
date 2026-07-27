use blazingly::prelude::*;
use futures_lite::future;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone)]
struct Events(Rc<RefCell<Vec<&'static str>>>);

#[get("/work", id = "work.create")]
#[allow(clippy::unused_async)]
async fn work(events: Events) -> Background<Json<&'static str>> {
    Json("accepted").background(BackgroundTask::infallible(move || async move {
        events.0.borrow_mut().push("background");
    }))
}

#[test]
fn lifespan_orders_scopes_and_background_waits_for_response_completion() {
    let entries = Rc::new(RefCell::new(Vec::new()));
    let parent_start = Rc::clone(&entries);
    let parent_stop = Rc::clone(&entries);
    let child_start = Rc::clone(&entries);
    let child_stop = Rc::clone(&entries);
    let executable = ExecutableApp::from_plugin(
        Plugin::new("app")
            .on_startup(move || {
                let entries = Rc::clone(&parent_start);
                async move {
                    entries.borrow_mut().push("parent_start");
                    Ok(())
                }
            })
            .on_shutdown(move || {
                let entries = Rc::clone(&parent_stop);
                async move {
                    entries.borrow_mut().push("parent_stop");
                    Ok(())
                }
            })
            .plugin(
                Plugin::new("worker")
                    .provide(Provider::value(Events(Rc::clone(&entries))))
                    .on_startup(move || {
                        let entries = Rc::clone(&child_start);
                        async move {
                            entries.borrow_mut().push("child_start");
                            Ok(())
                        }
                    })
                    .on_shutdown(move || {
                        let entries = Rc::clone(&child_stop);
                        async move {
                            entries.borrow_mut().push("child_stop");
                            Ok(())
                        }
                    })
                    .routes(routes![work]),
            ),
    )
    .expect("lifespan app");
    let app = TestApp::new(&executable);

    future::block_on(app.startup()).expect("startup");
    let mut response = future::block_on(app.call(Request::get("/work")));
    assert_eq!(entries.borrow().as_slice(), ["parent_start", "child_start"]);
    future::block_on(response.run_background()).expect("background task");
    assert_eq!(
        entries.borrow().as_slice(),
        ["parent_start", "child_start", "background"]
    );
    future::block_on(app.shutdown()).expect("shutdown");
    assert_eq!(
        entries.borrow().as_slice(),
        [
            "parent_start",
            "child_start",
            "background",
            "child_stop",
            "parent_stop",
        ]
    );
}
