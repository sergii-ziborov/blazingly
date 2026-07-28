use blazingly::prelude::*;
use blazingly_json::{Value, json};
use futures_lite::future;

#[api_model]
struct UploadResult {
    bytes: u64,
}

#[post("/uploads", id = "uploads.consume")]
async fn consume_upload(mut body: UploadBody) -> Json<UploadResult> {
    let mut bytes = 0_u64;
    while let Some(chunk) = body.next_chunk().await {
        let chunk = chunk.expect("upload chunk");
        bytes = bytes.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
    }
    Json(UploadResult { bytes })
}

#[test]
fn streaming_upload_has_testapp_contract_and_openapi_projections() {
    let executable =
        ExecutableApp::new(routes![consume_upload]).expect("streaming operation should compile");
    let descriptor = &executable.definition().operations()[0];
    assert_eq!(
        descriptor.contract.inputs[0].source,
        blazingly::InputSource::Stream
    );

    let response = future::block_on(
        TestApp::new(&executable).call(Request::post("/uploads").body(vec![7_u8; 17])),
    );
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().expect("JSON"),
        json!({ "bytes": 17 })
    );

    let document = blazingly::openapi::to_value(executable.definition());
    assert_eq!(
        document["paths"]["/uploads"]["post"]["requestBody"]["content"]["application/octet-stream"]
            ["schema"]["format"],
        "binary"
    );
}
