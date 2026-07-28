//! End-to-end coverage for `#[api_model(borrowed)]` and the synchronous
//! operation fast path.
//!
//! A proc-macro crate never compiles its own output, so everything the macro
//! emits for a borrowed view â€” the `Serialize` derive, the direct `ApiSchema`
//! impl, the generic envelope's descriptor name, the dependency taken by
//! reference, and the direct executor path that lets the borrow reach the
//! encoder at all â€” is exercised here through the facade.

use blazingly::prelude::*;
use blazingly_json::json;
use futures_lite::future;
use std::cell::RefCell;
use std::future::{Ready, ready};
use std::rc::Rc;
use std::thread::ThreadId;

// ---------------------------------------------------------------------------
// The owned rows a store already holds.
// ---------------------------------------------------------------------------

#[api_model]
#[derive(Clone, Debug)]
struct Tag {
    id: u32,
    name: String,
}

#[api_model]
#[derive(Clone, Debug)]
struct Article {
    id: u32,
    title: String,
    tags: Vec<Tag>,
}

#[api_model]
#[derive(Clone, Debug)]
struct Company {
    id: u32,
    name: String,
}

/// The store is a singleton and is never cloned into a handler.
struct Corpus {
    articles: Vec<Article>,
    companies: Vec<Company>,
}

#[provider(singleton)]
fn corpus() -> Corpus {
    Corpus {
        articles: vec![
            Article {
                id: 1,
                title: "First".to_owned(),
                tags: vec![Tag {
                    id: 10,
                    name: "rust".to_owned(),
                }],
            },
            Article {
                id: 2,
                title: "Second".to_owned(),
                tags: Vec::new(),
            },
        ],
        companies: vec![Company {
            id: 7,
            name: "Acme".to_owned(),
        }],
    }
}

/// Records which thread ran a handler body. An `Rc` is deliberate: the bounded
/// blocking pool requires `Send`, so a handler holding one cannot be on it.
#[derive(Clone, Default)]
struct Recorder(Rc<RefCell<Option<ThreadId>>>);

// ---------------------------------------------------------------------------
// Borrowed views.
// ---------------------------------------------------------------------------

/// One paginated envelope for every item type, written once.
#[api_model(borrowed)]
struct Page<'store, T> {
    items: Vec<&'store T>,
    total: usize,
}

/// A detail view that borrows both the row and a request-derived string.
#[api_model(borrowed, rename_all = "camelCase")]
struct ArticleView<'store> {
    article_id: u32,
    title: &'store str,
    tag_names: Vec<&'store str>,
    note: Option<&'store str>,
}

// ---------------------------------------------------------------------------
// Operations.
// ---------------------------------------------------------------------------

/// Synchronous, takes the store by reference, and answers with a view that
/// borrows it. Neither the rows nor their strings are cloned.
#[get(
    "/articles",
    id = "articles.list",
    summary = "List articles as a borrowed page"
)]
// Clippy sees a by-value argument that is only read through. `Depends<T>` is
// the framework's dependency handle and must be taken by value for the
// extractor to resolve it, so the lint is wrong here rather than the signature.
#[allow(clippy::needless_pass_by_value)]
fn list_articles(corpus: &Corpus, recorder: Depends<Recorder>) -> Json<Page<'_, Article>> {
    *recorder.0.borrow_mut() = Some(std::thread::current().id());
    Json(Page {
        items: corpus.articles.iter().collect(),
        total: corpus.articles.len(),
    })
}

/// The same envelope at a different item type.
#[get(
    "/companies",
    id = "companies.list",
    summary = "List companies as a borrowed page"
)]
fn list_companies(corpus: &Depends<Corpus>) -> Json<Page<'_, Company>> {
    Json(Page {
        items: corpus.companies.iter().collect(),
        total: corpus.companies.len(),
    })
}

#[get(
    "/articles/{id}",
    id = "articles.read",
    summary = "Read one article as a borrowed view"
)]
fn read_article(Path(id): Path<u32>, corpus: &Corpus) -> Json<ArticleView<'_>> {
    let article = corpus
        .articles
        .iter()
        .find(|article| article.id == id)
        .unwrap_or(&corpus.articles[0]);
    Json(ArticleView {
        article_id: article.id,
        title: &article.title,
        tag_names: article.tags.iter().map(|tag| tag.name.as_str()).collect(),
        note: None,
    })
}

fn app_with(recorder: Recorder) -> ExecutableApp {
    ExecutableApp::from_plugin(
        Plugin::new("borrowed")
            .provide(corpus::provider())
            .provide(Provider::value(recorder))
            .routes(routes![list_articles, list_companies, read_article]),
    )
    .expect("the borrowed-view graph should compile")
}

fn app() -> ExecutableApp {
    app_with(Recorder::default())
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn a_borrowed_view_serializes_the_rows_it_borrows() {
    let executable = app();
    let http = TestApp::new(&executable);

    let response = future::block_on(http.call(Request::get("/articles")));
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .json::<blazingly_json::Value>()
            .expect("the page should be JSON"),
        json!({
            "items": [
                { "id": 1, "title": "First", "tags": [{ "id": 10, "name": "rust" }] },
                { "id": 2, "title": "Second", "tags": [] }
            ],
            "total": 2
        })
    );

    let response = future::block_on(http.call(Request::get("/companies")));
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .json::<blazingly_json::Value>()
            .expect("the page should be JSON"),
        json!({ "items": [{ "id": 7, "name": "Acme" }], "total": 1 })
    );
}

#[test]
fn a_borrowed_view_honours_rename_all_and_optional_fields() {
    let executable = app();
    let response = future::block_on(TestApp::new(&executable).call(Request::get("/articles/1")));

    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .json::<blazingly_json::Value>()
            .expect("the view should be JSON"),
        json!({
            "articleId": 1,
            "title": "First",
            "tagNames": ["rust"],
            "note": null
        })
    );
}

#[test]
fn one_generic_envelope_documents_a_distinct_schema_per_item_type() {
    let executable = app();
    let operations = executable.definition().operations();
    let schema_of = |id: &str| {
        operations
            .iter()
            .find(|operation| operation.contract.id.as_str() == id)
            .and_then(|operation| operation.contract.responses.first())
            .and_then(|response| response.body.as_ref())
            .cloned()
            .expect("every listing documents a success body")
    };

    let articles = schema_of("articles.list");
    let companies = schema_of("companies.list");
    let article_model = articles.model.as_ref().expect("a page is a model");
    let company_model = companies.model.as_ref().expect("a page is a model");

    // One `Page<'store, T>` in Rust, two component schemas on the wire.
    assert_eq!(article_model.name, "Page_Article");
    assert_eq!(company_model.name, "Page_Company");
    assert_ne!(article_model.name, company_model.name);

    // The borrows are resolved: `Vec<&'store Article>` documents an array of
    // the owned `Article` model, not a reference type.
    let items = &article_model.fields[0];
    assert_eq!(items.name, "items");
    assert!(items.required);
    assert_eq!(items.ty.rust_name, "Vec<Article>");
    assert_eq!(
        items
            .ty
            .items
            .as_ref()
            .expect("an array carries its item contract")
            .rust_name,
        "Article"
    );

    // A borrowed view is an output type, so nothing about it is validated.
    assert!(
        article_model
            .fields
            .iter()
            .all(|field| field.validation.is_empty())
    );
}

#[test]
fn a_borrowed_field_documents_the_schema_its_owned_counterpart_documents() {
    let descriptor = <ArticleView<'_> as blazingly::ApiSchema>::type_descriptor();
    let model = descriptor.model.as_ref().expect("a view is a model");
    let named = |name: &str| {
        model
            .fields
            .iter()
            .find(|field| field.name == name)
            .expect("the field is declared")
    };

    assert_eq!(model.name, "ArticleView");
    assert_eq!(named("title").ty.schema, blazingly::SchemaKind::String);
    assert_eq!(
        named("tagNames").ty.rust_name,
        "Vec<&str>",
        "a borrowed string list documents an array of strings"
    );
    // `Option<&'store str>` is optional and still a string.
    assert!(!named("note").required);
    assert_eq!(named("note").ty.schema, blazingly::SchemaKind::String);
}

/// The direct path is what makes a borrowed response possible at all: a boxed
/// `'static` future cannot carry a borrow of the store out of the handler.
///
/// Two things prove the body did not go through the bounded blocking pool. It
/// ran on the calling thread rather than a pool worker, and it held `Depends`,
/// an `Rc` handle the pool's `Send` bound would have rejected outright.
#[test]
fn a_synchronous_handler_runs_inline_on_the_calling_thread() {
    let recorder = Recorder::default();
    let executable = app_with(recorder.clone());
    let response = future::block_on(TestApp::new(&executable).call(Request::get("/articles")));
    assert_eq!(response.status(), 200);

    assert_eq!(
        *recorder.0.borrow(),
        Some(std::thread::current().id()),
        "a synchronous handler must not hop to the blocking pool"
    );
}

// ---------------------------------------------------------------------------
// The asynchronous fallback.
// ---------------------------------------------------------------------------

type Events = Rc<RefCell<Vec<&'static str>>>;

#[derive(Clone)]
struct HookEvents(Events);

#[api_model]
#[derive(Clone, Debug)]
struct Marker {
    value: String,
}

#[get("/hooked", id = "hooked.read", summary = "Run under plugin hooks")]
#[allow(clippy::needless_pass_by_value)]
fn hooked(events: Depends<HookEvents>) -> Json<Marker> {
    events.0.borrow_mut().push("handler");
    Json(Marker {
        value: "sync".to_owned(),
    })
}

fn hook(
    events: Events,
    label: &'static str,
) -> impl Fn(HookContext) -> Ready<Result<(), DependencyError>> {
    move |_| {
        events.borrow_mut().push(label);
        ready(Ok(()))
    }
}

/// A hook makes the executor take the asynchronous fallback, and the body still
/// runs exactly where an async handler's body would.
#[test]
fn plugin_hooks_keep_their_ordering_around_a_synchronous_handler() {
    let events: Events = Rc::new(RefCell::new(Vec::new()));
    let executable = ExecutableApp::from_plugin(
        Plugin::new("hooked")
            .provide(Provider::value(HookEvents(Rc::clone(&events))))
            .pre_handler(hook(Rc::clone(&events), "pre_handler"))
            .pre_serialize(hook(Rc::clone(&events), "pre_serialize"))
            .routes(routes![hooked]),
    )
    .expect("the hooked graph should compile");

    let response = future::block_on(TestApp::new(&executable).call(Request::get("/hooked")));
    assert_eq!(response.status(), 200);
    assert_eq!(
        events.borrow().as_slice(),
        ["pre_handler", "handler", "pre_serialize"]
    );
}
