#![cfg(feature = "openapi")]

//! One module, mounted twice: the composition surface `Plugin::mount` and
//! `with_id_namespace` add, verified through routing, the operation graph,
//! and the `OpenAPI` projection.

use blazingly::prelude::*;
use blazingly::{ExecutableBuildError, ExecutableOperation};
use blazingly_json::Value;
use futures_lite::future;

#[api_model]
#[derive(Clone, Debug)]
struct NoteView {
    text: String,
}

#[get("/notes/{note_id}", id = "notes.read", summary = "Read one note")]
async fn read_note(Path(note_id): Path<u32>) -> Json<NoteView> {
    Json(NoteView {
        text: format!("note {note_id}"),
    })
}

#[get("/", id = "notes.home", summary = "The module root")]
async fn home() -> Json<NoteView> {
    Json(NoteView {
        text: "home".to_owned(),
    })
}

/// The module: written once, mounted wherever a caller decides.
fn notes_module() -> Plugin {
    Plugin::new("notes").routes(routes![read_note, home])
}

fn mounted_app() -> ExecutableApp {
    let root = Plugin::new("app")
        .plugin(notes_module().mount("/v1").with_id_namespace("v1"))
        .plugin(notes_module().mount("/v2").with_id_namespace("v2"));
    ExecutableApp::from_plugin(root).expect("two mounts of one module compile")
}

#[test]
fn one_module_serves_under_two_prefixes_without_restating_handlers() {
    let executable = mounted_app();
    let http = TestApp::new(&executable);

    for (path, expected) in [
        ("/v1/notes/7", "note 7"),
        ("/v2/notes/9", "note 9"),
        ("/v1", "home"),
        ("/v2", "home"),
    ] {
        let response = future::block_on(http.call(Request::get(path)));
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(
            response.json::<Value>().expect("a JSON response")["text"],
            expected,
            "{path}"
        );
    }

    let unprefixed = future::block_on(http.call(Request::get("/notes/7")));
    assert_eq!(
        unprefixed.status(),
        404,
        "the unmounted path must not exist"
    );
}

#[test]
fn a_mount_renames_the_identity_and_the_document_follows() {
    let executable = mounted_app();
    let ids = executable
        .definition()
        .operations()
        .iter()
        .map(|operation| operation.contract.id.as_str().to_owned())
        .collect::<Vec<_>>();
    assert!(ids.contains(&"v1.notes.read".to_owned()), "{ids:?}");
    assert!(ids.contains(&"v2.notes.read".to_owned()), "{ids:?}");

    let document = blazingly::openapi::to_value(executable.definition());
    assert!(
        !document["paths"]["/v1/notes/{note_id}"]["get"].is_null(),
        "the document serves the mounted path"
    );
    assert!(
        !document["paths"]["/v2/notes/{note_id}"]["get"].is_null(),
        "both mounts appear"
    );
    assert_eq!(
        document["paths"]["/v1/notes/{note_id}"]["get"]["operationId"],
        "v1.notes.read"
    );
    assert_eq!(
        document["paths"]["/v1/notes/{note_id}"]["get"]["tags"][0], "v1.notes",
        "the namespace becomes the section"
    );
}

#[test]
fn nested_mounts_join_their_prefixes_and_namespaces() {
    let root = Plugin::new("app").plugin(
        Plugin::new("api")
            .mount("/api")
            .with_id_namespace("api")
            .plugin(notes_module().mount("/v1").with_id_namespace("v1")),
    );
    let executable = ExecutableApp::from_plugin(root).expect("nested mounts compile");

    let response =
        future::block_on(TestApp::new(&executable).call(Request::get("/api/v1/notes/3")));
    assert_eq!(response.status(), 200);
    assert!(
        executable
            .definition()
            .operations()
            .iter()
            .any(|operation| operation.contract.id.as_str() == "api.v1.notes.read"),
        "namespaces join with dots"
    );
}

#[test]
fn mounting_one_module_twice_without_distinct_identities_fails_at_build() {
    let root = Plugin::new("app")
        .plugin(notes_module().mount("/v1"))
        .plugin(notes_module().mount("/v2"));

    let Err(error) = ExecutableApp::from_plugin(root) else {
        panic!("duplicate ids must not compile");
    };
    assert!(
        matches!(error, ExecutableBuildError::Definition(_)),
        "the existing duplicate-id build check catches it: {error}"
    );
}

#[test]
fn a_malformed_prefix_or_namespace_is_rejected_before_anything_runs() {
    for prefix in ["v1", "/v1/", "/v{1}", "//v1"] {
        let root = Plugin::new("app").plugin(notes_module().mount(prefix));
        let Err(error) = ExecutableApp::from_plugin(root) else {
            panic!("prefix {prefix} must be rejected");
        };
        assert!(
            matches!(error, ExecutableBuildError::InvalidMountPrefix { .. }),
            "{prefix}: {error}"
        );
    }

    let root = Plugin::new("app").plugin(notes_module().with_id_namespace("v1.api"));
    let Err(error) = ExecutableApp::from_plugin(root) else {
        panic!("a dotted namespace must be rejected");
    };
    assert!(
        matches!(error, ExecutableBuildError::InvalidIdNamespace { .. }),
        "{error}"
    );
}

#[test]
fn a_mounted_mcp_tool_stays_a_distinct_tool_per_mount() {
    let operation = ExecutableOperation::empty(
        blazingly::OperationDescriptor::new(
            blazingly::HttpMethod::Get,
            "/ping",
            "tools.ping",
            "Ping",
            None,
            vec![blazingly::ResponseDescriptor::success(204, None)],
        )
        .expect("descriptor is valid")
        .with_mcp_tool(
            blazingly::McpToolDescriptor::new("ping", "Answers with nothing."),
            blazingly::AgentPolicy::default(),
        ),
        || async { NoContent },
    );
    let root = Plugin::new("app").plugin(
        Plugin::new("tools")
            .mount("/v1")
            .with_id_namespace("v1")
            .operation(operation),
    );
    let executable = ExecutableApp::from_plugin(root).expect("mounted tool compiles");

    let tool = executable.definition().operations()[0]
        .contract
        .mcp
        .as_ref()
        .expect("the tool survives the mount");
    assert_eq!(tool.name, "v1_ping");
}
