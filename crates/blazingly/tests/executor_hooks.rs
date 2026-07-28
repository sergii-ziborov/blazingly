use blazingly::prelude::*;
use blazingly_json::json;
use futures_lite::future;
use std::cell::RefCell;
use std::future::{Ready, ready};
use std::rc::Rc;

type Events = Rc<RefCell<Vec<&'static str>>>;

#[api_model]
#[derive(Clone, Debug)]
struct HookInput {
    #[min_length(2)]
    value: String,
}

#[api_model]
#[derive(Clone, Debug)]
struct HookOutput {
    value: String,
}

#[derive(Clone)]
struct HookEvents(Events);

#[post("/hooks", id = "hooks.run")]
async fn hook_operation(
    Json(input): Json<HookInput>,
    events: Depends<HookEvents>,
) -> Json<HookOutput> {
    events.0.borrow_mut().push("handler");
    Json(HookOutput { value: input.value })
}

#[test]
fn inherited_hooks_follow_the_compiled_operation_lifecycle() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let executable = ExecutableApp::from_plugin(
        Plugin::new("app")
            .provide(Provider::value(HookEvents(Rc::clone(&events))))
            .on_request(operation_hook(Rc::clone(&events), "root:on_request"))
            .pre_parse(operation_hook(Rc::clone(&events), "root:pre_parse"))
            .pre_validate(operation_hook(Rc::clone(&events), "root:pre_validate"))
            .pre_handler(operation_hook(Rc::clone(&events), "root:pre_handler"))
            .pre_serialize(operation_hook(Rc::clone(&events), "root:pre_serialize"))
            .on_error(response_hook(Rc::clone(&events), "root:on_error"))
            .on_response(response_hook(Rc::clone(&events), "root:on_response"))
            .plugin(
                Plugin::new("feature")
                    .on_request(operation_hook(Rc::clone(&events), "child:on_request"))
                    .pre_parse(operation_hook(Rc::clone(&events), "child:pre_parse"))
                    .pre_validate(operation_hook(Rc::clone(&events), "child:pre_validate"))
                    .pre_handler(operation_hook(Rc::clone(&events), "child:pre_handler"))
                    .pre_serialize(operation_hook(Rc::clone(&events), "child:pre_serialize"))
                    .on_error(response_hook(Rc::clone(&events), "child:on_error"))
                    .on_response(response_hook(Rc::clone(&events), "child:on_response"))
                    .routes(routes![hook_operation]),
            ),
    )
    .expect("hook graph should compile");
    let http = TestApp::new(&executable);

    let response = future::block_on(
        http.call(
            Request::post("/hooks")
                .json(&json!({ "value": "ready" }))
                .expect("fixture should serialize"),
        ),
    );
    assert_eq!(response.status(), 200);
    assert_eq!(
        events.borrow().as_slice(),
        [
            "root:on_request",
            "child:on_request",
            "root:pre_parse",
            "child:pre_parse",
            "root:pre_validate",
            "child:pre_validate",
            "root:pre_handler",
            "child:pre_handler",
            "handler",
            "root:pre_serialize",
            "child:pre_serialize",
            "child:on_response",
            "root:on_response",
        ]
    );

    events.borrow_mut().clear();
    let invalid = future::block_on(
        http.call(
            Request::post("/hooks")
                .json(&json!({ "value": "x" }))
                .expect("fixture should serialize"),
        ),
    );
    assert_eq!(invalid.status(), 422);
    assert_eq!(
        events.borrow().as_slice(),
        [
            "root:on_request",
            "child:on_request",
            "root:pre_parse",
            "child:pre_parse",
            "root:pre_validate",
            "child:pre_validate",
            "child:on_error",
            "root:on_error",
            "child:on_response",
            "root:on_response",
        ]
    );
}

#[test]
fn shutdown_runs_child_first_and_continues_after_failure() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let executable = ExecutableApp::from_plugin(
        Plugin::new("app")
            .on_shutdown(shutdown_hook(Rc::clone(&events), "root:shutdown", false))
            .plugin(Plugin::new("feature").on_shutdown(shutdown_hook(
                Rc::clone(&events),
                "child:shutdown",
                true,
            ))),
    )
    .expect("shutdown hook graph should compile");

    let error = future::block_on(executable.shutdown()).expect_err("child hook should fail");
    assert!(error.to_string().contains("child shutdown failed"));
    assert_eq!(
        events.borrow().as_slice(),
        ["child:shutdown", "root:shutdown"]
    );
}

fn operation_hook(
    events: Events,
    event: &'static str,
) -> impl Fn(HookContext) -> Ready<Result<(), DependencyError>> {
    move |_| {
        events.borrow_mut().push(event);
        ready(Ok(()))
    }
}

fn response_hook(
    events: Events,
    event: &'static str,
) -> impl Fn(HookContext, HookOutcome) -> Ready<()> {
    move |_, _| {
        events.borrow_mut().push(event);
        ready(())
    }
}

fn shutdown_hook(
    events: Events,
    event: &'static str,
    fails: bool,
) -> impl Fn() -> Ready<Result<(), DependencyError>> {
    move || {
        events.borrow_mut().push(event);
        ready(if fails {
            Err(DependencyError::internal(
                "shutdown_failed",
                "child shutdown failed",
            ))
        } else {
            Ok(())
        })
    }
}
