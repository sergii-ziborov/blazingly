#![cfg(all(feature = "openapi", feature = "validation"))]

//! The served document must say exactly what the generated checks enforce.
//!
//! Both cases here were found by building the same service on three frameworks:
//! a value type's bounds vanished from a collection item, and the 422 the input
//! pipeline actually returns was not documented at all.

use blazingly::openapi::OpenApiConfig;
use blazingly::prelude::*;
use blazingly_json::{Value, json};
use futures_lite::future;

#[api_model]
#[min_length(1)]
#[max_length(20)]
#[derive(Clone, Debug)]
struct Tag(String);

#[api_model]
#[derive(Clone, Debug)]
struct CreateNote {
    title: String,
    #[max_items(5)]
    tags: Vec<Tag>,
    primary: Tag,
    fallback: Option<Tag>,
    groups: Vec<Vec<Tag>>,
}

#[api_model]
#[derive(Clone, Debug)]
struct NoteView {
    title: String,
}

#[post("/notes", id = "notes.create")]
async fn create_note(Json(input): Json<CreateNote>) -> Status<201, Json<NoteView>> {
    Status(Json(NoteView { title: input.title }))
}

#[get("/health", id = "health.read")]
async fn health() -> Json<NoteView> {
    Json(NoteView {
        title: "ok".to_owned(),
    })
}

fn app() -> ExecutableApp {
    ExecutableApp::new(routes![create_note, health]).expect("fidelity app should compile")
}

fn document(executable: &ExecutableApp) -> Value {
    let served = future::block_on(
        TestApp::new(executable)
            .with_openapi(OpenApiConfig::new("Fidelity", "0.0.0"))
            .call(Request::get("/openapi.json")),
    );
    assert_eq!(served.status(), 200);
    served.json::<Value>().expect("document is JSON")
}

#[test]
fn a_value_types_bounds_survive_every_place_the_type_is_used() {
    let executable = app();
    let document = document(&executable);
    let properties = &document["components"]["schemas"]["CreateNote"]["properties"];

    let item = &properties["tags"]["items"];
    assert_eq!(item["x-rust-type"], "Tag");
    assert_eq!(item["minLength"], 1, "a collection item keeps Tag's bounds");
    assert_eq!(item["maxLength"], 20);
    assert_eq!(
        properties["tags"]["maxItems"], 5,
        "the field's own bound still describes the collection"
    );

    assert_eq!(properties["primary"]["minLength"], 1);
    assert_eq!(properties["primary"]["maxLength"], 20);
    assert_eq!(properties["fallback"]["minLength"], 1);
    assert_eq!(properties["fallback"]["maxLength"], 20);
    assert_eq!(
        properties["fallback"]["type"],
        json!(["string", "null"]),
        "nullability and the declared bounds coexist"
    );

    let nested = &properties["groups"]["items"]["items"];
    assert_eq!(nested["x-rust-type"], "Tag");
    assert_eq!(nested["minLength"], 1, "nesting does not lose the bounds");
    assert_eq!(nested["maxLength"], 20);
}

#[test]
fn the_documented_422_is_the_envelope_the_service_actually_returns() {
    let executable = app();
    let document = document(&executable);

    let documented = &document["paths"]["/notes"]["post"]["responses"]["422"];
    assert_eq!(documented["x-blazingly-error-code"], "validation_error");
    let schema = &documented["content"]["application/json"]["schema"];
    assert_eq!(
        schema["properties"]["error"]["properties"]["code"]["const"],
        "validation_error"
    );
    assert!(
        document["paths"]["/health"]["get"]["responses"]["422"].is_null(),
        "an operation with no validated input does not claim a 422"
    );

    let rejected = future::block_on(
        TestApp::new(&executable).call(
            Request::post("/notes")
                .json(&json!({
                    "title": "note",
                    "tags": ["123456789012345678901"],
                    "primary": "p",
                    "groups": []
                }))
                .expect("fixture should serialize"),
        ),
    );
    assert_eq!(rejected.status(), 422);
    let body = rejected.json::<Value>().expect("failure body is JSON");
    assert_eq!(body["error"]["code"], "validation_error");
    let violation = &body["error"]["details"]["violations"][0];
    assert_eq!(violation["field"], "tags[0]");
    assert_eq!(violation["code"], "max_length");

    assert_shaped_like(&body, schema);
}

/// Checks one payload against the object schema the document declares for it.
///
/// The point of the test is that the two agree, so the check walks the declared
/// properties rather than restating them.
fn assert_shaped_like(payload: &Value, schema: &Value) {
    let Some(properties) = schema["properties"].as_object() else {
        return;
    };
    for name in schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        assert!(
            !payload[name].is_null(),
            "the returned envelope is missing the documented `{name}`"
        );
    }
    for (name, property) in properties {
        let member = &payload[name.as_str()];
        if member.is_null() {
            continue;
        }
        if let Some(expected) = property["const"].as_str() {
            assert_eq!(member, expected);
        }
        assert_shaped_like(member, property);
    }
}
