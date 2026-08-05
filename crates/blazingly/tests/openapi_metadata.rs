//! Declarable documentation metadata, and the responses the security pipeline
//! answers with.
//!
//! Everything an operation can declare here is prose a reader sees and nothing
//! enforces, so none of it can contradict the handler. The two properties worth
//! holding are that it reaches the document at all, and that it stays out of
//! the contract — a tag must not move a fingerprint.

use blazingly::prelude::*;
use blazingly_json::{Value, json};

#[api_model]
#[derive(Clone, Debug)]
struct Note {
    id: u64,
    body: String,
}

#[api_model]
#[derive(Clone, Debug)]
struct NewNote {
    #[min_length(1)]
    body: String,
}

#[get("/notes/{id}", id = "notes.read", summary = "Read one note")]
async fn read_note(Path(id): Path<u64>) -> Json<Note> {
    Json(Note {
        id,
        body: String::new(),
    })
}

#[post(
    "/notes",
    id = "notes.create",
    summary = "Create a note",
    // Deliberately not `notes`: the first declared tag has to beat the
    // namespace of the operation id, or the declaration is not doing anything.
    tags = ["writing", "notes"],
    description = "Stores one note and returns it with the identifier it was given.",
    external_docs = "https://example.com/notes",
    external_docs_description = "The note lifecycle"
)]
async fn create_note(Json(input): Json<NewNote>) -> Created<Note> {
    Created(Note {
        id: 1,
        body: input.body,
    })
}

#[get(
    "/notes/legacy",
    id = "notes.legacy",
    summary = "Read notes the old way",
    deprecated
)]
async fn legacy_notes() -> Json<Vec<Note>> {
    Json(Vec::new())
}

#[operation(
    method = DELETE,
    path = "/notes/{id}",
    id = "notes.delete",
    summary = "Delete a note",
    tags = ["notes"],
    deprecated = false
)]
#[security("oauth", scopes = ["notes:write"])]
async fn delete_note(Path(id): Path<u64>) -> NoContent {
    let _ = id;
    NoContent
}

#[get("/notes", id = "notes.list", summary = "List notes")]
#[security("oauth")]
async fn list_notes() -> Json<Vec<Note>> {
    Json(Vec::new())
}

fn application() -> ExecutableApp {
    ExecutableApp::with_security_schemes(
        routes![
            read_note,
            create_note,
            legacy_notes,
            delete_note,
            list_notes
        ],
        [blazingly::SecuritySchemeDescriptor::new(
            "oauth",
            blazingly::SecuritySchemeKind::OAuth2 {
                authorization_url: None,
                token_url: Some("/token".to_owned()),
                scopes: vec!["notes:write".to_owned()],
            },
        )],
    )
    .expect("the application contract compiles")
}

fn document() -> Value {
    blazingly::openapi::to_value(application().definition())
}

#[test]
fn a_declared_tag_list_replaces_the_inferred_namespace_tag() {
    let document = document();

    assert_eq!(
        document["paths"]["/notes"]["post"]["tags"],
        json!(["writing", "notes"]),
        "a declared list is projected verbatim and in order"
    );
    assert_eq!(
        document["paths"]["/notes/{id}"]["get"]["tags"],
        json!(["notes"]),
        "an operation that declares nothing still files under its id namespace"
    );

    let names = document["tags"]
        .as_array()
        .expect("the document lists its tags")
        .iter()
        .map(|tag| tag["name"].as_str().expect("a tag has a name").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec!["notes".to_owned(), "writing".to_owned()],
        "a declared tag joins the document-level list exactly once, sorted"
    );
}

#[test]
fn declared_prose_reaches_the_operation() {
    let document = document();
    let create = &document["paths"]["/notes"]["post"];

    assert_eq!(
        create["description"],
        "Stores one note and returns it with the identifier it was given."
    );
    assert_eq!(create["externalDocs"]["url"], "https://example.com/notes");
    assert_eq!(create["externalDocs"]["description"], "The note lifecycle");
    assert!(
        create["deprecated"].is_null(),
        "an operation that is not deprecated does not say so"
    );
}

#[test]
fn deprecation_is_projected_only_when_declared_true() {
    let document = document();

    assert_eq!(
        document["paths"]["/notes/legacy"]["get"]["deprecated"],
        true
    );
    assert!(
        document["paths"]["/notes/{id}"]["delete"]["deprecated"].is_null(),
        "`deprecated = false` is the default and stays out of the document"
    );
}

#[test]
fn a_declared_security_scheme_documents_the_401_it_will_answer() {
    let document = document();
    let list = &document["paths"]["/notes"]["get"];

    assert_eq!(
        list["responses"]["401"]["x-blazingly-automatic"], true,
        "the 401 is derived from the security declaration, not written by hand"
    );
    assert!(
        list["responses"]["403"].is_null(),
        "a requirement with no scopes cannot be answered 403"
    );
}

#[test]
fn a_scope_requirement_additionally_documents_the_403() {
    let document = document();
    let delete = &document["paths"]["/notes/{id}"]["delete"];

    assert_eq!(delete["responses"]["401"]["x-blazingly-automatic"], true);
    assert_eq!(delete["responses"]["403"]["x-blazingly-automatic"], true);
}

#[test]
fn an_operation_without_security_claims_neither_status() {
    let document = document();
    let read = &document["paths"]["/notes/{id}"]["get"];

    assert!(read["responses"]["401"].is_null());
    assert!(read["responses"]["403"].is_null());
}

#[test]
fn documentation_metadata_stays_out_of_the_contract() {
    // The whole reason this lives beside the contract instead of inside it: a
    // filing decision must not register as a change to the operation.
    let definition = application().definition().clone();
    let tagged = definition
        .operations()
        .iter()
        .find(|operation| operation.contract.id.as_str() == "notes.create")
        .expect("the tagged operation is registered")
        .clone();

    let mut untagged = tagged.clone();
    untagged.documentation = blazingly::OperationDocumentation::default();

    assert_eq!(
        tagged.contract.fingerprint(),
        untagged.contract.fingerprint(),
        "adding tags, prose, and a deprecation flag leaves the fingerprint alone"
    );
}

#[test]
fn the_generated_markdown_agrees_with_the_document() {
    // The two projections read the same declaration, so a tag that files an
    // operation one way in the browser must file it the same way on the page.
    let application = application();
    let markdown = blazingly::docs::api_markdown(application.definition());

    assert!(
        markdown.contains("**Deprecated.**"),
        "a deprecated operation is marked on the page, not only in the document"
    );
    assert!(
        markdown.contains("https://example.com/notes"),
        "an external documentation link reaches the page"
    );
    assert!(
        markdown.contains("Stores one note and returns it with the identifier it was given."),
        "declared prose reaches the page"
    );
    assert!(
        markdown.contains("writing"),
        "the first declared tag opens the section, beating the operation-id namespace"
    );
}

#[test]
fn the_overlay_adds_what_the_projection_cannot_derive() {
    let config = blazingly::openapi::OpenApiConfig::new("Notes", "1.0.0")
        .with_description("Everything about notes.")
        .with_server(
            blazingly::openapi::OpenApiServer::new("https://api.example.com")
                .with_description("Production"),
        )
        .with_tag_description("notes", "Reading and writing notes.")
        .with_overlay(json!({
            "info": {
                "contact": { "name": "API team", "email": "api@example.com" },
                "license": { "name": "MIT" },
                // The projection owns this one; the overlay must lose.
                "title": "Overwritten"
            },
            "webhooks": {
                "noteCreated": { "post": { "responses": { "200": { "description": "Delivered" } } } }
            }
        }));
    let document = blazingly::openapi::to_value_with_config(application().definition(), &config);

    assert_eq!(document["info"]["title"], "Notes");
    assert_eq!(document["info"]["description"], "Everything about notes.");
    assert_eq!(document["info"]["contact"]["name"], "API team");
    assert_eq!(document["info"]["license"]["name"], "MIT");
    assert_eq!(document["servers"][0]["url"], "https://api.example.com");
    assert_eq!(document["servers"][0]["description"], "Production");
    assert_eq!(
        document["webhooks"]["noteCreated"]["post"]["responses"]["200"]["description"],
        "Delivered"
    );

    let notes_tag = document["tags"]
        .as_array()
        .expect("the document lists its tags")
        .iter()
        .find(|tag| tag["name"] == "notes")
        .expect("the notes tag is listed");
    assert_eq!(notes_tag["description"], "Reading and writing notes.");
}

#[test]
fn the_overlay_cannot_overwrite_anything_the_code_decided() {
    let config = blazingly::openapi::OpenApiConfig::new("Notes", "1.0.0").with_overlay(json!({
        "paths": {
            "/notes": {
                "post": {
                    "operationId": "something.else",
                    "responses": {
                        "201": { "description": "Overwritten" },
                        // A status the projection does not produce is additive
                        // and therefore allowed through.
                        "503": { "description": "Maintenance" }
                    }
                }
            }
        }
    }));
    let document = blazingly::openapi::to_value_with_config(application().definition(), &config);
    let create = &document["paths"]["/notes"]["post"];

    assert_eq!(create["operationId"], "notes.create");
    assert_ne!(create["responses"]["201"]["description"], "Overwritten");
    assert_eq!(create["responses"]["503"]["description"], "Maintenance");
}
