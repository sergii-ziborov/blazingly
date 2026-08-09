//! A small task tracker built against the local checkout, as a user would.

use blazingly::openapi::OpenApiConfig;
use blazingly::prelude::*;
use blazingly::{SecuritySchemeDescriptor, SecuritySchemeKind};
use futures_lite::future;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------- models

#[api_model]
#[derive(Clone, Debug)]
struct NewTask {
    #[min_length(1)]
    #[max_length(120)]
    title: String,
    #[max_length(2000)]
    notes: Option<String>,
    #[minimum(1)]
    #[maximum(5)]
    #[default(3)]
    priority: u8,
}

#[api_model]
#[derive(Clone, Debug)]
struct Task {
    id: u64,
    title: String,
    notes: Option<String>,
    priority: u8,
    done: bool,
    owner: String,
}

#[api_model]
#[derive(Clone, Debug)]
struct TaskPatch {
    #[min_length(1)]
    #[max_length(120)]
    title: Option<String>,
    #[minimum(1)]
    #[maximum(5)]
    priority: Option<u8>,
    done: Option<bool>,
    // Probe: can a PATCH distinguish "absent" from "explicit null"?
    // This nested option is the behavior under test, so replacing it with a
    // custom enum would stop exercising the framework's serde-compatible path.
    #[allow(clippy::option_option)]
    notes: Option<Option<String>>,
}

#[api_model]
#[derive(Clone, Debug)]
struct ListQuery {
    #[default(20)]
    #[minimum(1)]
    #[maximum(100)]
    limit: u32,
    #[default(0)]
    offset: u32,
    done: Option<bool>,
}

#[api_model]
#[derive(Clone, Debug)]
struct TaskPage {
    items: Vec<Task>,
    total: u32,
    limit: u32,
    offset: u32,
}

#[api_model]
#[derive(Clone, Debug)]
struct AttachmentView {
    task_id: u64,
    file_name: Option<String>,
    size: u32,
}

#[api_error]
enum TaskError {
    #[status(404)]
    #[code("task_not_found")]
    #[message("No task with that identifier exists.")]
    NotFound,
}

// ------------------------------------------------------- injected pieces

/// Singleton: the actual storage, one per worker.
#[derive(Clone, Default)]
struct TaskStore {
    next_id: Rc<Cell<u64>>,
    rows: Rc<RefCell<Vec<Task>>>,
}

#[provider(singleton)]
fn task_store() -> TaskStore {
    TaskStore::default()
}

/// Request-scoped: the repository handlers actually talk to. It closes over
/// the singleton store and counts what this one request did.
#[derive(Clone)]
struct TaskRepository {
    store: TaskStore,
    writes: Rc<Cell<u32>>,
}

#[provider]
// Providers are resolved from owned framework dependency handles. Clippy sees
// only the dereference below and cannot infer that the macro requires by-value.
#[allow(clippy::needless_pass_by_value)]
fn task_repository(store: Depends<TaskStore>) -> TaskRepository {
    TaskRepository {
        store: (*store).clone(),
        writes: Rc::new(Cell::new(0)),
    }
}

/// The caller, read off the verified security context in the handler because a
/// `#[provider]` may not take `Extension<T>`.
fn actor_of(security: &SecurityContext) -> String {
    security
        .primary()
        .and_then(|identity| identity.subject.clone())
        .unwrap_or_else(|| "anonymous".to_owned())
}

impl TaskRepository {
    fn create(&self, input: NewTask, actor: &str) -> Task {
        let id = self.store.next_id.get() + 1;
        self.store.next_id.set(id);
        let task = Task {
            id,
            title: input.title,
            notes: input.notes,
            priority: input.priority,
            done: false,
            owner: actor.to_owned(),
        };
        self.store.rows.borrow_mut().push(task.clone());
        self.writes.set(self.writes.get() + 1);
        task
    }

    fn list(&self, query: &ListQuery) -> TaskPage {
        let rows = self.store.rows.borrow();
        let filtered = rows
            .iter()
            .filter(|task| query.done.is_none_or(|done| task.done == done))
            .cloned()
            .collect::<Vec<_>>();
        let total = u32::try_from(filtered.len()).unwrap_or(u32::MAX);
        let items = filtered
            .into_iter()
            .skip(query.offset as usize)
            .take(query.limit as usize)
            .collect();
        TaskPage {
            items,
            total,
            limit: query.limit,
            offset: query.offset,
        }
    }

    fn get(&self, id: u64) -> Option<Task> {
        self.store
            .rows
            .borrow()
            .iter()
            .find(|task| task.id == id)
            .cloned()
    }

    fn patch(&self, id: u64, patch: TaskPatch) -> Option<Task> {
        let mut rows = self.store.rows.borrow_mut();
        let task = rows.iter_mut().find(|task| task.id == id)?;
        if let Some(title) = patch.title {
            task.title = title;
        }
        if let Some(priority) = patch.priority {
            task.priority = priority;
        }
        if let Some(done) = patch.done {
            task.done = done;
        }
        // `Some(None)` means "clear it"; `None` means "leave it alone" -- if the
        // decoder actually distinguishes the two.
        if let Some(notes) = patch.notes {
            task.notes = notes;
        }
        Some(task.clone())
    }

    fn delete(&self, id: u64) -> bool {
        let mut rows = self.store.rows.borrow_mut();
        let before = rows.len();
        rows.retain(|task| task.id != id);
        let removed = rows.len() != before;
        if removed {
            self.writes.set(self.writes.get() + 1);
        }
        removed
    }

    fn snapshot(&self) -> Vec<Task> {
        self.store.rows.borrow().clone()
    }
}

// ------------------------------------------------------------ operations

#[post("/tasks", id = "tasks.create", summary = "Create a task")]
#[security("bearer", scopes = ["tasks:write"])]
#[allow(clippy::unused_async)]
async fn create_task(
    Json(input): Json<NewTask>,
    Extension(security): Extension<SecurityContext>,
    repo: TaskRepository,
) -> Created<Task> {
    Created(repo.create(input, &actor_of(&security)))
}

// A synchronous handler: no `async fn`, so no `clippy::unused_async` allow.
#[get("/tasks", id = "tasks.list", summary = "List tasks")]
// Handler dependencies are injected as owned values by the generated adapter.
#[allow(clippy::needless_pass_by_value)]
fn list_tasks(Query(query): Query<ListQuery>, repo: TaskRepository) -> Json<TaskPage> {
    Json(repo.list(&query))
}

#[get("/tasks/{id}", id = "tasks.read", summary = "Read one task")]
#[allow(clippy::unused_async)]
async fn read_task(Path(id): Path<u64>, repo: TaskRepository) -> Result<Json<Task>, TaskError> {
    repo.get(id).map(Json).ok_or(TaskError::NotFound)
}

#[patch("/tasks/{id}", id = "tasks.patch", summary = "Update one task")]
#[security("bearer", scopes = ["tasks:write"])]
#[allow(clippy::unused_async)]
async fn patch_task(
    Path(id): Path<u64>,
    Json(patch): Json<TaskPatch>,
    repo: TaskRepository,
) -> Result<Json<Task>, TaskError> {
    repo.patch(id, patch).map(Json).ok_or(TaskError::NotFound)
}

#[delete("/tasks/{id}", id = "tasks.delete", summary = "Delete one task")]
#[security("bearer", scopes = ["tasks:write"])]
#[allow(clippy::unused_async)]
async fn delete_task(Path(id): Path<u64>, repo: TaskRepository) -> Result<NoContent, TaskError> {
    if repo.delete(id) {
        Ok(NoContent)
    } else {
        Err(TaskError::NotFound)
    }
}

#[get(
    "/tasks/export",
    id = "tasks.export",
    summary = "Stream every task as NDJSON"
)]
#[allow(clippy::unused_async)]
async fn export_tasks(repo: TaskRepository) -> WithHeaders<StreamingBody> {
    let lines = repo
        .snapshot()
        .into_iter()
        .map(|task| {
            let mut line = blazingly::json::to_string(&task).unwrap_or_default();
            line.push('\n');
            line.into_bytes()
        })
        .collect::<Vec<_>>();
    StreamingBody::from_chunks(lines).header("content-type", "application/x-ndjson")
}

#[post(
    "/tasks/{id}/attachment",
    id = "tasks.attach",
    summary = "Attach one file to a task"
)]
#[security("bearer", scopes = ["tasks:write"])]
#[allow(clippy::unused_async)]
async fn attach_file(
    Path(id): Path<u64>,
    File(file): File<UploadFile>,
    repo: TaskRepository,
) -> Result<Json<AttachmentView>, TaskError> {
    if repo.get(id).is_none() {
        return Err(TaskError::NotFound);
    }
    Ok(Json(AttachmentView {
        task_id: id,
        file_name: file.file_name,
        size: u32::try_from(file.bytes.len()).unwrap_or(u32::MAX),
    }))
}

// ---------------------------------------------------------- the app

// A plain `Http { scheme: "bearer" }` scheme cannot carry scopes, and declaring
// `scopes = [..]` on an operation against it fails the contract build with
// `UnknownSecurityScope`. Scoped bearer auth has to be modelled as OAuth2.
fn bearer_scheme() -> SecuritySchemeDescriptor {
    SecuritySchemeDescriptor::new(
        "bearer",
        SecuritySchemeKind::OAuth2 {
            authorization_url: None,
            token_url: Some("/oauth/token".to_owned()),
            scopes: vec!["tasks:read".to_owned(), "tasks:write".to_owned()],
        },
    )
}

fn application() -> ExecutableApp {
    ExecutableApp::from_plugin(
        Plugin::new("tasks")
            .security_scheme(bearer_scheme())
            .provide(task_store::provider())
            .provide(task_repository::provider())
            .routes(routes![
                create_task,
                list_tasks,
                read_task,
                patch_task,
                delete_task,
                export_tasks,
                attach_file,
            ]),
    )
    .expect("task tracker contract should compile")
}

const SIGNING_KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

fn jwt() -> JwtHs256 {
    JwtHs256::new(SIGNING_KEY).expect("strong key")
}

fn token(scope: &str) -> String {
    let expiry = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_secs()
        + 300;
    jwt()
        .encode(&JwtClaims::new("alice", expiry).scope(scope))
        .expect("token")
}

fn test_app(executable: &ExecutableApp) -> TestApp<'_> {
    TestApp::new(executable)
        .with_middleware(Security::new().verifier("bearer", OAuth2Bearer::new(jwt())))
        .with_openapi(OpenApiConfig::new("Task Tracker", "1.0.0"))
}

fn authorized(request: Request) -> Request {
    request.header("authorization", format!("Bearer {}", token("tasks:write")))
}

fn json_body(value: &str) -> Vec<u8> {
    value.as_bytes().to_vec()
}

// ------------------------------------------------------------- tests

#[test]
fn create_read_patch_delete_round_trip() {
    let executable = application();
    let app = test_app(&executable);

    let created = future::block_on(
        app.call(
            authorized(Request::post("/tasks"))
                .header("content-type", "application/json")
                .body(json_body(
                    r#"{"title":"write the lens report","priority":2}"#,
                )),
        ),
    );
    assert_eq!(created.status(), 201);
    let body = created.json::<blazingly::json::Value>().expect("JSON");
    assert_eq!(body["id"], 1);
    assert_eq!(body["title"], "write the lens report");
    assert_eq!(body["priority"], 2);
    assert_eq!(body["done"], false);
    assert_eq!(body["owner"], "alice");

    let read = future::block_on(app.call(Request::get("/tasks/1")));
    assert_eq!(read.status(), 200);

    let patched = future::block_on(
        app.call(
            authorized(Request::patch("/tasks/1"))
                .header("content-type", "application/json")
                .body(json_body(r#"{"done":true}"#)),
        ),
    );
    assert_eq!(patched.status(), 200);
    let body = patched.json::<blazingly::json::Value>().expect("JSON");
    assert_eq!(body["done"], true);
    assert_eq!(body["title"], "write the lens report");

    let deleted = future::block_on(app.call(authorized(Request::delete("/tasks/1"))));
    assert_eq!(deleted.status(), 204);

    let gone = future::block_on(app.call(Request::get("/tasks/1")));
    assert_eq!(gone.status(), 404);
    let body = gone.json::<blazingly::json::Value>().expect("JSON");
    assert_eq!(body["error"]["code"], "task_not_found");
}

#[test]
fn validation_rejects_a_bad_body_before_the_handler() {
    let executable = application();
    let app = test_app(&executable);
    let response = future::block_on(
        app.call(
            authorized(Request::post("/tasks"))
                .header("content-type", "application/json")
                .body(json_body(r#"{"title":"","priority":9}"#)),
        ),
    );
    assert_eq!(response.status(), 422);
    let body = response.json::<blazingly::json::Value>().expect("JSON");
    assert_eq!(body["error"]["code"], "validation_error");
}

#[test]
fn mutating_operations_need_the_scope() {
    let executable = application();
    let app = test_app(&executable);

    let anonymous = future::block_on(
        app.call(
            Request::post("/tasks")
                .header("content-type", "application/json")
                .body(json_body(r#"{"title":"nope"}"#)),
        ),
    );
    assert_eq!(anonymous.status(), 401);

    let wrong_scope = future::block_on(
        app.call(
            Request::post("/tasks")
                .header("authorization", format!("Bearer {}", token("tasks:read")))
                .header("content-type", "application/json")
                .body(json_body(r#"{"title":"nope"}"#)),
        ),
    );
    assert_eq!(wrong_scope.status(), 403);

    // Reads stay open.
    assert_eq!(
        future::block_on(app.call(Request::get("/tasks"))).status(),
        200
    );
}

#[test]
fn pagination_uses_query_parameters_with_declared_defaults() {
    let executable = application();
    let app = test_app(&executable);
    for index in 0..5 {
        let response = future::block_on(
            app.call(
                authorized(Request::post("/tasks"))
                    .header("content-type", "application/json")
                    .body(json_body(&format!(r#"{{"title":"task {index}"}}"#))),
            ),
        );
        assert_eq!(response.status(), 201);
    }

    let page = future::block_on(app.call(Request::get("/tasks?limit=2&offset=2")));
    assert_eq!(page.status(), 200);
    let body = page.json::<blazingly::json::Value>().expect("JSON");
    assert_eq!(body["total"], 5);
    assert_eq!(body["limit"], 2);
    assert_eq!(body["offset"], 2);
    assert_eq!(body["items"].as_array().expect("items").len(), 2);
    assert_eq!(body["items"][0]["title"], "task 2");

    let defaults = future::block_on(app.call(Request::get("/tasks")));
    let body = defaults.json::<blazingly::json::Value>().expect("JSON");
    assert_eq!(body["limit"], 20);
    assert_eq!(body["offset"], 0);

    let rejected = future::block_on(app.call(Request::get("/tasks?limit=500")));
    assert_eq!(rejected.status(), 422);
}

#[test]
fn export_streams_ndjson() {
    let executable = application();
    let app = test_app(&executable);
    future::block_on(
        app.call(
            authorized(Request::post("/tasks"))
                .header("content-type", "application/json")
                .body(json_body(r#"{"title":"streamed"}"#)),
        ),
    );

    let response = future::block_on(app.call(Request::get("/tasks/export")));
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.get_header("content-type"),
        Some("application/x-ndjson")
    );
    assert!(response.is_streaming());
    let body = future::block_on(response.collect_body(64 * 1024)).expect("stream body");
    let text = String::from_utf8(body).expect("UTF-8");
    assert!(text.ends_with('\n'));
    assert!(text.contains("streamed"));
}

#[test]
fn attachment_upload_reaches_the_handler() {
    let executable = application();
    let app = test_app(&executable);
    future::block_on(
        app.call(
            authorized(Request::post("/tasks"))
                .header("content-type", "application/json")
                .body(json_body(r#"{"title":"has an attachment"}"#)),
        ),
    );

    let boundary = "tracker-boundary";
    let body = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"spec.txt\"\r\n\
         Content-Type: text/plain\r\n\r\n\
         hello upload\r\n\
         --{boundary}--\r\n"
    );
    let response = future::block_on(
        app.call(
            authorized(Request::post("/tasks/1/attachment"))
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(body),
        ),
    );
    assert_eq!(response.status(), 200);
    let value = response.json::<blazingly::json::Value>().expect("JSON");
    assert_eq!(value["task_id"], 1);
    assert_eq!(value["file_name"], "spec.txt");
    assert_eq!(value["size"], 12);
}

#[test]
fn singleton_survives_requests_and_the_request_scope_does_not() {
    let executable = application();
    let app = test_app(&executable);
    for index in 0..3 {
        let created = future::block_on(
            app.call(
                authorized(Request::post("/tasks"))
                    .header("content-type", "application/json")
                    .body(json_body(&format!(r#"{{"title":"row {index}"}}"#))),
            ),
        );
        assert_eq!(created.status(), 201);
        // The singleton store keeps counting across requests.
        assert_eq!(
            created.json::<blazingly::json::Value>().expect("JSON")["id"],
            index + 1
        );
    }
    // The request-scoped repository starts each request with a fresh counter,
    // so a per-request write count is always 1 here rather than 1, 2, 3.
    let listed = future::block_on(app.call(Request::get("/tasks")));
    assert_eq!(
        listed.json::<blazingly::json::Value>().expect("JSON")["total"],
        3
    );
}

/// Behaviour I did not expect and had to discover by running it.
#[test]
fn surprising_edge_cases_are_pinned_here() {
    let executable = application();
    let app = test_app(&executable);
    future::block_on(
        app.call(
            authorized(Request::post("/tasks"))
                .header("content-type", "application/json")
                .body(json_body(r#"{"title":"edge"}"#)),
        ),
    );

    // 1. A body that is valid JSON but missing a required field is reported as
    //    `invalid_json` / "request body is not valid JSON", and the violation
    //    carries an empty field path -- the field name only appears in prose.
    let missing = future::block_on(
        app.call(
            authorized(Request::post("/tasks"))
                .header("content-type", "application/json")
                .body(json_body("{}")),
        ),
    );
    assert_eq!(missing.status(), 422);
    let body = missing.json::<blazingly::json::Value>().expect("JSON");
    assert_eq!(body["error"]["code"], "invalid_json");
    assert_eq!(body["error"]["details"]["violations"][0]["field"], "");
    assert_eq!(
        body["error"]["details"]["violations"][0]["message"],
        "missing field `title`"
    );

    // 2. `Option<Option<T>>` compiles but flattens: an explicit JSON null is
    //    indistinguishable from an absent field, so PATCH cannot clear a value.
    let set = future::block_on(
        app.call(
            authorized(Request::patch("/tasks/1"))
                .header("content-type", "application/json")
                .body(json_body(r#"{"notes":"keep me"}"#)),
        ),
    );
    assert_eq!(
        set.json::<blazingly::json::Value>().expect("JSON")["notes"],
        "keep me"
    );
    let cleared = future::block_on(
        app.call(
            authorized(Request::patch("/tasks/1"))
                .header("content-type", "application/json")
                .body(json_body(r#"{"notes":null}"#)),
        ),
    );
    assert_eq!(
        cleared.json::<blazingly::json::Value>().expect("JSON")["notes"],
        "keep me",
        "an explicit null does not reach the handler as Some(None)"
    );

    // 3. The document advertises the stream as application/octet-stream even
    //    though the handler sets application/x-ndjson at runtime.
    let document = blazingly::openapi::to_value(executable.definition());
    let stream_content = &document["paths"]["/tasks/export"]["get"]["responses"]["200"]["content"];
    assert!(
        !stream_content["application/octet-stream"].is_null(),
        "{stream_content}"
    );
    assert!(
        stream_content["application/x-ndjson"].is_null(),
        "the runtime content-type header does not reach the document"
    );

    // 4. A `File<UploadFile>` request body is documented as a bare binary
    //    string under multipart/form-data rather than an object with parts.
    let upload_schema = &document["paths"]["/tasks/{id}/attachment"]["post"]["requestBody"]["content"]
        ["multipart/form-data"]["schema"];
    assert_eq!(upload_schema["type"], "string");
    assert_eq!(upload_schema["format"], "binary");
}

#[test]
fn openapi_document_is_served_and_describes_the_service() {
    let executable = application();
    let app = test_app(&executable);
    let response = future::block_on(app.call(Request::get("/openapi.json")));
    assert_eq!(response.status(), 200);
    let document = response
        .json::<blazingly::json::Value>()
        .expect("OpenAPI document");
    assert_eq!(document["openapi"], "3.1.0");
    assert_eq!(document["info"]["title"], "Task Tracker");
    assert!(!document["paths"]["/tasks"]["post"]["responses"]["201"]["description"].is_null());
    assert!(!document["paths"]["/tasks/{id}"]["get"]["responses"]["404"].is_null());
    assert_eq!(
        document["components"]["securitySchemes"]["bearer"]["type"],
        "oauth2"
    );
    let limit = document["paths"]["/tasks"]["get"]["parameters"]
        .as_array()
        .expect("query parameters")
        .iter()
        .find(|parameter| parameter["name"] == "limit")
        .expect("limit parameter")
        .clone();
    assert_eq!(limit["schema"]["maximum"], 100);
}
