use blazingly::prelude::*;
use blazingly::{ExecutableApp, HttpMethod};
use futures_lite::future;

#[api_model]
#[derive(Clone, Debug)]
struct FormInput {
    name: String,
    count: u32,
}

#[api_model]
#[derive(Clone, Debug)]
struct FormView {
    item_id: u64,
    session: String,
    name: String,
    count: u32,
}

#[api_model]
#[derive(Clone, Debug)]
struct UploadView {
    field_name: String,
    file_name: Option<String>,
    content_type: Option<String>,
    size: usize,
}

#[api_model]
#[derive(Clone, Debug)]
struct MultipartInput {
    title: String,
    attachments: Vec<UploadFile>,
}

#[put(
    "/items/{item_id}",
    id = "items.update",
    summary = "Update one item from an HTML form"
)]
#[allow(clippy::unused_async)]
async fn update_item(
    Path(item_id): Path<u64>,
    Cookie(session): Cookie<String>,
    Form(input): Form<FormInput>,
) -> Json<FormView> {
    Json(FormView {
        item_id,
        session,
        name: input.name,
        count: input.count,
    })
}

#[post("/upload", id = "files.upload", summary = "Upload one file")]
#[security("upload_key")]
#[mcp::tool(
    name = "upload_file",
    description = "Upload one base64-encoded file",
    risk = "write",
    confirmation = "never",
    idempotent = false,
    expose_output = "full"
)]
#[allow(clippy::unused_async)]
async fn upload_file(File(file): File<UploadFile>) -> Json<UploadView> {
    Json(upload_view(file))
}

#[post(
    "/multipart",
    id = "files.multipart",
    summary = "Decode typed multipart data"
)]
#[allow(clippy::unused_async)]
async fn multipart_upload(Multipart(input): Multipart<MultipartInput>) -> Json<UploadView> {
    let mut file = input.attachments.into_iter().next().unwrap();
    file.field_name = input.title;
    Json(upload_view(file))
}

#[head("/methods", id = "methods.head")]
#[allow(clippy::unused_async)]
async fn head_method() -> NoContent {
    NoContent
}

#[patch("/methods", id = "methods.patch")]
#[allow(clippy::unused_async)]
async fn patch_method() -> NoContent {
    NoContent
}

#[delete("/methods", id = "methods.delete")]
#[allow(clippy::unused_async)]
async fn delete_method() -> NoContent {
    NoContent
}

#[options("/methods", id = "methods.options")]
#[allow(clippy::unused_async)]
async fn options_method() -> NoContent {
    NoContent
}

#[trace("/methods", id = "methods.trace")]
#[allow(clippy::unused_async)]
async fn trace_method() -> NoContent {
    NoContent
}

#[connect("/methods", id = "methods.connect")]
#[allow(clippy::unused_async)]
async fn connect_method() -> NoContent {
    NoContent
}

#[get("/cookies", id = "cookies.write")]
#[allow(clippy::unused_async)]
async fn write_cookies() -> WithHeaders<Json<&'static str>> {
    Json("ok")
        .header("set-cookie", "session=fast; Path=/; HttpOnly")
        .header("set-cookie", "theme=dark; Path=/")
}

fn upload_view(file: UploadFile) -> UploadView {
    UploadView {
        field_name: file.field_name,
        file_name: file.file_name,
        content_type: file.content_type,
        size: file.bytes.len(),
    }
}

fn executable() -> ExecutableApp {
    ExecutableApp::with_security_schemes(
        routes![
            update_item,
            upload_file,
            multipart_upload,
            head_method,
            patch_method,
            delete_method,
            options_method,
            trace_method,
            connect_method,
            write_cookies,
        ],
        [blazingly::SecuritySchemeDescriptor::new(
            "upload_key",
            blazingly::SecuritySchemeKind::ApiKey {
                location: blazingly::SecurityLocation::Header,
                name: "x-upload-key".to_owned(),
            },
        )],
    )
    .expect("operation graph should compile")
}

#[test]
fn response_pipeline_preserves_multiple_set_cookie_fields() {
    let executable = executable();
    let response = future::block_on(TestApp::new(&executable).call(Request::get("/cookies")));
    let cookies = response
        .headers()
        .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
        .map(|(_, value)| value)
        .collect::<Vec<_>>();
    assert_eq!(
        cookies,
        ["session=fast; Path=/; HttpOnly", "theme=dark; Path=/"]
    );
}

#[test]
fn form_cookie_and_every_http_method_share_the_compiled_router() {
    let executable = executable();
    let app = TestApp::new(&executable);
    let response = future::block_on(
        app.call(
            Request::put("/items/42")
                .header("cookie", "session=fast; theme=dark")
                .header("content-type", "application/x-www-form-urlencoded")
                .body("name=blazingly&count=7"),
        ),
    );

    assert_eq!(response.status(), 200);
    let value: serde_json::Value = response.json().unwrap();
    assert_eq!(value["item_id"], 42);
    assert_eq!(value["session"], "fast");
    assert_eq!(value["name"], "blazingly");
    assert_eq!(value["count"], 7);

    let expected = [
        (head_method::descriptor(), HttpMethod::Head),
        (patch_method::descriptor(), HttpMethod::Patch),
        (delete_method::descriptor(), HttpMethod::Delete),
        (options_method::descriptor(), HttpMethod::Options),
        (trace_method::descriptor(), HttpMethod::Trace),
        (connect_method::descriptor(), HttpMethod::Connect),
    ];
    for (descriptor, method) in expected {
        assert_eq!(descriptor.http.method, method);
    }
}

#[test]
fn file_upload_has_the_same_typed_http_and_mcp_operation() {
    let executable = executable();
    let app = TestApp::new(&executable);
    let boundary = "blazingly-boundary";
    let body = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"speed.txt\"\r\n\
         Content-Type: text/plain\r\n\r\n\
         millions\r\n\
         --{boundary}--\r\n"
    );
    let response = future::block_on(
        app.call(
            Request::post("/upload")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(body),
        ),
    );

    assert_eq!(response.status(), 200);
    let http_value: serde_json::Value = response.json().unwrap();
    assert_eq!(http_value["file_name"], "speed.txt");
    assert_eq!(http_value["content_type"], "text/plain");
    assert_eq!(http_value["size"], 8);

    let runtime = blazingly::mcp::McpRuntime::new(&executable);
    let mcp = future::block_on(runtime.call_tool(
        "upload_file",
        serde_json::json!({
            "file": {
                "base64": "bWlsbGlvbnM=",
                "file_name": "speed.txt",
                "content_type": "text/plain"
            }
        }),
        blazingly::mcp::McpCallContext::default(),
    ))
    .unwrap();
    assert!(!mcp.is_error);
    assert_eq!(mcp.structured_content.unwrap()["size"], 8);
}

#[test]
fn typed_multipart_models_include_text_and_multiple_files() {
    let executable = executable();
    let app = TestApp::new(&executable);
    let boundary = "typed";
    let body = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"title\"\r\n\r\n\
         release\r\n\
         --{boundary}\r\n\
         Content-Disposition: form-data; name=\"attachments\"; filename=\"a.txt\"\r\n\
         Content-Type: text/plain\r\n\r\n\
         first\r\n\
         --{boundary}\r\n\
         Content-Disposition: form-data; name=\"attachments\"; filename=\"b.txt\"\r\n\
         Content-Type: text/plain\r\n\r\n\
         second\r\n\
         --{boundary}--\r\n"
    );
    let response = future::block_on(
        app.call(
            Request::post("/multipart")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary=\"{boundary}\""),
                )
                .body(body),
        ),
    );

    assert_eq!(response.status(), 200);
    let value: serde_json::Value = response.json().unwrap();
    assert_eq!(value["field_name"], "release");
    assert_eq!(value["file_name"], "a.txt");
    assert_eq!(value["size"], 5);
}
