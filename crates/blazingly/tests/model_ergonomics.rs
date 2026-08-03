#![cfg(feature = "validation")]

//! The declaration ergonomics an application actually writes: field defaults,
//! a rule bundle reused by two models, a string enumeration, nullability in the
//! document, and the field path a custom validator reports.

use blazingly::prelude::*;
use blazingly::{
    ApiConstrained, ApiSchema, FieldDescriptor, FieldMetadata, ModelDescriptor, SchemaKind,
    ValidationErrors, ValidationRule,
};
use blazingly_json::{Value, json};
use futures_lite::future;

// ---------------------------------------------------------------------------
// Declarations.
// ---------------------------------------------------------------------------

/// One bundle of rules, written once and applied by name.
#[api_model]
#[min_length(8)]
#[max_length(200)]
#[derive(Clone, Debug)]
struct Title(String);

/// A closed set of strings, spelled as what it is instead of as a pattern.
#[api_model(rename_all = "lowercase")]
#[derive(Clone, Copy, Debug, PartialEq)]
enum Language {
    Uk,
    Ru,
    En,
}

/// Names the field it is declared on, which is the obvious way to write one.
// A field validator is handed the field by reference whatever its size, so the
// signature is the framework's rather than this test's to choose.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn check_publish_window(value: &u32) -> Result<(), ValidationErrors> {
    if *value <= 2100 {
        return Ok(());
    }
    let mut errors = ValidationErrors::new();
    errors.push(
        "published_at",
        "too_far_ahead",
        "must not be more than a century ahead",
    );
    Err(errors)
}

#[api_model]
#[derive(Clone, Debug)]
struct CreateArticle {
    title: Title,
    language: Language,
    #[validate_with(check_publish_window)]
    published_at: u32,
    summary: Option<String>,
}

/// The same `Title` rules, not re-typed.
#[api_model]
#[derive(Clone, Debug)]
struct UpdateArticle {
    title: Title,
    summary: Option<String>,
}

#[api_model]
#[derive(Clone, Debug)]
struct ListArticles {
    #[default(20)]
    #[minimum(1)]
    #[maximum(100)]
    limit: u32,
    #[default("draft")]
    status: String,
    #[default(false)]
    verbose: bool,
    language: Option<Language>,
}

#[api_model]
#[derive(Clone, Debug)]
struct ListingView {
    limit: u32,
    status: String,
    verbose: bool,
    language: Option<Language>,
}

#[api_model]
#[derive(Clone, Debug)]
struct ArticleView {
    title: String,
    language: Language,
}

#[api_model(borrowed)]
struct BorrowedArticleView<'store> {
    title: &'store str,
    summary: Option<&'store str>,
}

/// The same bundle again, one level down a collection.
#[api_model]
#[derive(Clone, Debug)]
struct RenameBatch {
    #[min_items(1)]
    titles: Vec<Title>,
}

// ---------------------------------------------------------------------------
// Operations.
// ---------------------------------------------------------------------------

/// Not one `unwrap_or` in sight: the defaults were declared, not applied here.
#[get("/articles", id = "articles.list", summary = "List articles")]
async fn list_articles(Query(query): Query<ListArticles>) -> Json<ListingView> {
    Json(ListingView {
        limit: query.limit,
        status: query.status,
        verbose: query.verbose,
        language: query.language,
    })
}

#[post("/articles", id = "articles.create", summary = "Create an article")]
async fn create_article(Json(input): Json<CreateArticle>) -> Json<ArticleView> {
    Json(ArticleView {
        title: input.title.into_inner(),
        language: input.language,
    })
}

#[patch(
    "/articles/{article_id}",
    id = "articles.update",
    summary = "Update an article"
)]
async fn update_article(
    Path(article_id): Path<u32>,
    Json(input): Json<UpdateArticle>,
) -> Json<ArticleView> {
    let _ = article_id;
    Json(ArticleView {
        title: input.title.into_inner(),
        language: Language::En,
    })
}

#[post("/titles", id = "articles.rename", summary = "Rename articles in bulk")]
async fn rename_articles(Json(input): Json<RenameBatch>) -> Json<ListingView> {
    Json(ListingView {
        limit: u32::try_from(input.titles.len()).unwrap_or(u32::MAX),
        status: "renamed".to_owned(),
        verbose: false,
        language: None,
    })
}

fn app() -> ExecutableApp {
    ExecutableApp::new(routes![
        list_articles,
        create_article,
        update_article,
        rename_articles
    ])
    .expect("the model operations should compile")
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn input_model(executable: &ExecutableApp, operation: &str) -> ModelDescriptor {
    let descriptor = executable
        .definition()
        .operations()
        .iter()
        .find(|candidate| candidate.contract.id.as_str() == operation)
        .expect("the operation is registered");
    *descriptor.contract.inputs[descriptor.contract.inputs.len() - 1]
        .ty
        .model
        .clone()
        .expect("the input is a model")
}

fn field<'model>(model: &'model ModelDescriptor, name: &str) -> &'model FieldDescriptor {
    model
        .fields
        .iter()
        .find(|field| field.name == name)
        .expect("the field is declared")
}

/// The metadata a schema projection recovers from the recorded rules.
fn metadata(field: &FieldDescriptor) -> Vec<FieldMetadata> {
    field
        .validation
        .iter()
        .filter_map(|rule| match rule {
            ValidationRule::Custom(encoded) => FieldMetadata::parse(encoded),
            _ => None,
        })
        .collect()
}

fn violations(body: &Value) -> Vec<(String, String)> {
    body["error"]["details"]["violations"]
        .as_array()
        .expect("violations are reported")
        .iter()
        .map(|violation| {
            (
                violation["field"].as_str().expect("field").to_owned(),
                violation["code"].as_str().expect("code").to_owned(),
            )
        })
        .collect()
}

fn post_article(body: &Value) -> (u16, Value) {
    let response = future::block_on(
        TestApp::new(&app()).call(
            Request::post("/articles")
                .json(body)
                .expect("fixture should serialize"),
        ),
    );
    let status = response.status();
    (status, response.json::<Value>().expect("a JSON response"))
}

fn valid_article() -> Value {
    json!({
        "title": "A title long enough",
        "language": "uk",
        "published_at": 2026,
        "summary": null
    })
}

// ---------------------------------------------------------------------------
// 1. Field-level defaults.
// ---------------------------------------------------------------------------

#[test]
fn a_declared_default_reaches_the_handler_without_an_unwrap_or() {
    let executable = app();
    let http = TestApp::new(&executable);

    let response = future::block_on(http.call(Request::get("/articles")));
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().expect("a JSON response"),
        json!({ "limit": 20, "status": "draft", "verbose": false, "language": null })
    );

    let response = future::block_on(http.call(Request::get("/articles?limit=5&verbose=true")));
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().expect("a JSON response"),
        json!({ "limit": 5, "status": "draft", "verbose": true, "language": null })
    );
}

#[test]
fn a_declared_default_is_recorded_as_optional_input_with_its_value() {
    let model = input_model(&app(), "articles.list");

    let limit = field(&model, "limit");
    assert!(
        !limit.required,
        "a field the server fills in is not required of the client"
    );
    assert_eq!(metadata(limit), [FieldMetadata::Default(json!(20))]);
    assert!(
        limit
            .validation
            .contains(&ValidationRule::Custom("minimum=1".to_owned())),
        "the declared bound survives beside the default"
    );

    assert_eq!(
        metadata(field(&model, "status")),
        [FieldMetadata::Default(json!("draft"))]
    );
    assert_eq!(
        metadata(field(&model, "verbose")),
        [FieldMetadata::Default(json!(false))]
    );
}

#[test]
fn a_default_still_answers_to_the_bounds_declared_beside_it() {
    let response = future::block_on(TestApp::new(&app()).call(Request::get("/articles?limit=0")));
    assert_eq!(response.status(), 422);
    assert_eq!(
        violations(&response.json::<Value>().expect("a JSON response")),
        [("limit".to_owned(), "minimum".to_owned())]
    );
}

// ---------------------------------------------------------------------------
// 2. Reusable constrained types.
// ---------------------------------------------------------------------------

#[test]
fn one_rule_bundle_is_declared_once_and_reused_by_two_models() {
    let created = input_model(&app(), "articles.create");
    let updated = input_model(&app(), "articles.update");

    for model in [&created, &updated] {
        let title = field(model, "title");
        assert!(
            title.validation.contains(&ValidationRule::MinLength(8))
                && title.validation.contains(&ValidationRule::MaxLength(200)),
            "{} did not inherit the declared rules: {:?}",
            model.name,
            title.validation
        );
    }

    let descriptor = <Title as ApiSchema>::type_descriptor();
    assert_eq!(descriptor.rust_name, "Title");
    assert_eq!(descriptor.schema, SchemaKind::String);
    assert_eq!(
        <Title as ApiConstrained>::constraint_rules(),
        [ValidationRule::MinLength(8), ValidationRule::MaxLength(200)]
    );
}

#[test]
fn a_bundle_is_enforced_wherever_the_type_appears() {
    let mut body = valid_article();
    body["title"] = json!("short");
    let (status, response) = post_article(&body);
    assert_eq!(status, 422);
    assert_eq!(
        violations(&response),
        [("title".to_owned(), "min_length".to_owned())]
    );

    let response = future::block_on(
        TestApp::new(&app()).call(
            Request::patch("/articles/1")
                .json(&json!({ "title": "tiny", "summary": null }))
                .expect("fixture should serialize"),
        ),
    );
    assert_eq!(response.status(), 422);
    assert_eq!(
        violations(&response.json::<Value>().expect("a JSON response")),
        [("title".to_owned(), "min_length".to_owned())]
    );
}

#[test]
fn a_bundle_reaches_the_items_of_a_collection() {
    let executable = app();
    let http = TestApp::new(&executable);
    let accepted = future::block_on(
        http.call(
            Request::post("/titles")
                .json(&json!({ "titles": ["A title long enough"] }))
                .expect("fixture should serialize"),
        ),
    );
    assert_eq!(accepted.status(), 200);

    let rejected = future::block_on(
        http.call(
            Request::post("/titles")
                .json(&json!({ "titles": ["A title long enough", "tiny"] }))
                .expect("fixture should serialize"),
        ),
    );
    assert_eq!(rejected.status(), 422);
    assert_eq!(
        violations(&rejected.json::<Value>().expect("a JSON response")),
        [("titles[1]".to_owned(), "min_length".to_owned())]
    );
}

/// The document has to publish the bound the request above was rejected by.
///
/// The bundle belongs to the item, not to the list: reading `min_length` off
/// the field would say the list needs eight entries, which is neither what the
/// validator enforces nor what the client has to satisfy.
#[test]
fn a_bundle_reaches_the_items_schema_the_document_publishes() {
    let executable = app();
    let document = blazingly::openapi::to_value(executable.definition());
    let titles = &document["components"]["schemas"]["RenameBatch"]["properties"]["titles"];

    assert_eq!(
        titles["items"]["minLength"],
        json!(8),
        "the item bundle must reach the item schema: {titles}"
    );
    assert_eq!(titles["items"]["maxLength"], json!(200));
    assert_eq!(
        titles["minItems"],
        json!(1),
        "the list keeps the bound declared on the field"
    );
    assert!(
        titles["minLength"].is_null() && titles["maxLength"].is_null(),
        "an item bound must not be read as a bound on the list: {titles}"
    );

    // The prose bundle reads as a bound on each element too, rather than
    // leaking the encoding the rule travelled in.
    let markdown = blazingly::docs::api_markdown(executable.definition());
    assert!(
        markdown.contains("each item min length 8")
            && markdown.contains("each item max length 200"),
        "the item bundle must read as prose, not as its encoding: {markdown}"
    );

    // The same rule, enforced and published: the item the runtime refuses is
    // the item the published schema refuses.
    let rejected = future::block_on(
        TestApp::new(&executable).call(
            Request::post("/titles")
                .json(&json!({ "titles": ["tiny"] }))
                .expect("fixture should serialize"),
        ),
    );
    assert_eq!(rejected.status(), 422);
    assert_eq!(
        violations(&rejected.json::<Value>().expect("a JSON response")),
        [("titles[0]".to_owned(), "min_length".to_owned())]
    );
}

/// A rejection the runtime can return is a response the document declares.
#[test]
fn the_rejection_every_decoded_input_can_produce_is_documented() {
    let document = blazingly::openapi::to_value(app().definition());
    let rejection = &document["paths"]["/titles"]["post"]["responses"]["422"];

    assert!(
        !rejection.is_null(),
        "an operation that decodes a body documents the rejection it can return"
    );
    let schema = &rejection["content"]["application/json"]["schema"];
    let codes = schema["properties"]["error"]["properties"]["code"]["enum"]
        .as_array()
        .expect("the codes a rejection can carry are a closed set");
    assert!(
        codes.contains(&json!("validation_error")),
        "the code the runtime reports must be documented: {codes:?}"
    );

    let violation = &schema["properties"]["error"]["properties"]["details"]["properties"]["violations"]
        ["items"];
    assert_eq!(violation["properties"]["field"]["type"], "string");
    assert_eq!(violation["properties"]["code"]["type"], "string");
    assert_eq!(
        rejection["x-blazingly-automatic"],
        json!(true),
        "a response the framework projects is marked as one it was not declared"
    );
}

// ---------------------------------------------------------------------------
// 3. Enumerations.
// ---------------------------------------------------------------------------

#[test]
fn an_enumeration_is_a_string_schema_with_a_closed_variant_set() {
    assert_eq!(Language::VARIANTS, ["uk", "ru", "en"]);
    assert_eq!(Language::Uk.as_str(), "uk");

    let descriptor = <Language as ApiSchema>::type_descriptor();
    assert_eq!(descriptor.rust_name, "Language");
    assert_eq!(descriptor.schema, SchemaKind::String);

    let created = input_model(&app(), "articles.create");
    let language = field(&created, "language");
    assert_eq!(language.ty.schema, SchemaKind::String);
    assert_eq!(
        metadata(language),
        [FieldMetadata::Enumeration(vec![
            "uk".to_owned(),
            "ru".to_owned(),
            "en".to_owned()
        ])]
    );
}

#[test]
fn an_enumeration_accepts_its_variants_and_refuses_everything_else() {
    let (status, response) = post_article(&valid_article());
    assert_eq!(status, 200);
    assert_eq!(response["language"], json!("uk"));

    let mut body = valid_article();
    body["language"] = json!("de");
    let (status, response) = post_article(&body);
    assert_eq!(status, 422);
    assert_eq!(response["error"]["details"]["field"], "language");
}

#[test]
fn an_enumeration_travels_through_a_query_string_too() {
    let response =
        future::block_on(TestApp::new(&app()).call(Request::get("/articles?language=ru")));
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().expect("a JSON response")["language"],
        json!("ru")
    );
}

// ---------------------------------------------------------------------------
// 4. Nullability.
// ---------------------------------------------------------------------------

#[test]
fn an_optional_field_is_marked_nullable_in_the_document() {
    let created = input_model(&app(), "articles.create");
    let summary = field(&created, "summary");
    assert!(!summary.required);
    assert_eq!(metadata(summary), [FieldMetadata::Nullable]);

    let required = field(&created, "title");
    assert!(required.required);
    assert!(
        metadata(required)
            .iter()
            .all(|entry| *entry != FieldMetadata::Nullable)
    );

    // A response view prints `null` for the same reason and says so as well.
    let view = <BorrowedArticleView<'_> as ApiSchema>::type_descriptor();
    let view = view.model.as_ref().expect("a view is a model");
    assert_eq!(metadata(field(view, "summary")), [FieldMetadata::Nullable]);
    assert!(metadata(field(view, "title")).is_empty());
}

// ---------------------------------------------------------------------------
// 5. The field path a custom validator reports.
// ---------------------------------------------------------------------------

#[test]
fn a_field_validator_that_names_its_field_is_reported_once() {
    let mut body = valid_article();
    body["published_at"] = json!(4000);
    let (status, response) = post_article(&body);

    assert_eq!(status, 422);
    assert_eq!(
        violations(&response),
        [("published_at".to_owned(), "too_far_ahead".to_owned())],
        "the field path must not repeat the field"
    );
}

// ---------------------------------------------------------------------------
// The document a projection builds from all of it.
// ---------------------------------------------------------------------------

#[cfg(feature = "openapi")]
#[test]
fn everything_recorded_reaches_the_openapi_document() {
    let executable = app();
    let document = blazingly::openapi::to_value(executable.definition());
    let parameters = document["paths"]["/articles"]["get"]["parameters"]
        .as_array()
        .expect("query parameters are projected");
    let parameter = |name: &str| {
        parameters
            .iter()
            .find(|parameter| parameter["name"] == json!(name))
            .expect("the parameter is projected")
    };

    let limit = parameter("limit");
    assert_eq!(limit["required"], json!(false));
    assert_eq!(limit["schema"]["default"], json!(20));
    assert_eq!(limit["schema"]["minimum"], json!(1));

    assert_eq!(
        parameter("language")["schema"]["enum"],
        json!(["uk", "ru", "en"])
    );

    let article = &document["components"]["schemas"]["CreateArticle"];
    assert_eq!(article["properties"]["title"]["minLength"], json!(8));
    assert_eq!(
        article["properties"]["summary"]["type"],
        json!(["string", "null"]),
        "an optional field is nullable in the document"
    );
}
