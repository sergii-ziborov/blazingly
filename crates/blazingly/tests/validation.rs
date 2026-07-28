#![cfg(feature = "validation")]

use blazingly::prelude::*;
use blazingly::{ValidationErrors, ValidationRule};
use blazingly_json::{Value, json};
use futures_lite::future;

#[api_model]
struct Address {
    #[min_length(3)]
    street: String,
}

fn validate_code(value: &str) -> Result<(), ValidationErrors> {
    if value.starts_with("BZ-") {
        return Ok(());
    }
    let mut errors = ValidationErrors::new();
    errors.push("", "code_prefix", "must start with BZ-");
    Err(errors)
}

#[api_model(rename_all = "camelCase")]
struct ValidationInput {
    #[alias("legacyName")]
    public_name: String,
    #[nested]
    address: Address,
    #[nested]
    items: Vec<Address>,
    #[validate_with(validate_code)]
    code: String,
    id: Uuid,
    site: Url,
    ip: IpAddress,
    date: Date,
    at: DateTime,
    amount: Decimal,
}

#[post("/validation", id = "validation.create")]
async fn validate_input(Json(input): Json<ValidationInput>) -> Json<ValidationInput> {
    Json(input)
}

fn app() -> ExecutableApp {
    ExecutableApp::new(routes![validate_input]).expect("validation operation should compile")
}

fn valid_body() -> Value {
    json!({
        "legacyName": "public",
        "address": { "street": "Main" },
        "items": [{ "street": "First" }],
        "code": "BZ-42",
        "id": "550e8400-e29b-41d4-a716-446655440000",
        "site": "https://example.com/items/42",
        "ip": "192.0.2.4",
        "date": "2026-07-27",
        "at": "2026-07-27T10:00:00Z",
        "amount": "999.2500"
    })
}

#[test]
fn aliases_strong_types_and_contract_metadata_share_one_model() {
    let executable = app();
    let response = future::block_on(
        TestApp::new(&executable).call(
            Request::post("/validation")
                .json(&valid_body())
                .expect("fixture should serialize"),
        ),
    );
    assert_eq!(response.status(), 200);
    let body = response.json::<Value>().expect("response JSON");
    assert_eq!(body["publicName"], "public");
    assert_eq!(body["amount"], "999.2500");

    let model = executable.definition().operations()[0].contract.inputs[0]
        .ty
        .model
        .as_ref()
        .expect("input model");
    let public_name = model
        .fields
        .iter()
        .find(|field| field.name == "publicName")
        .expect("publicName field");
    assert!(
        public_name
            .validation
            .contains(&ValidationRule::Alias("legacyName".to_owned()))
    );
    assert!(
        model
            .fields
            .iter()
            .find(|field| field.name == "address")
            .expect("address field")
            .validation
            .contains(&ValidationRule::Nested)
    );
    assert!(
        model
            .fields
            .iter()
            .find(|field| field.name == "code")
            .expect("code field")
            .validation
            .contains(&ValidationRule::Custom("validate_code".to_owned()))
    );
}

#[test]
fn nested_and_custom_validation_collect_precise_field_paths() {
    let mut body = valid_body();
    body["address"]["street"] = json!("x");
    body["items"][0]["street"] = json!("");
    body["code"] = json!("wrong");

    let response = future::block_on(
        TestApp::new(&app()).call(
            Request::post("/validation")
                .json(&body)
                .expect("fixture should serialize"),
        ),
    );
    assert_eq!(response.status(), 422);
    let body = response.json::<Value>().expect("validation error JSON");
    assert_eq!(body["error"]["code"], "validation_error");
    let fields = body["error"]["details"]["violations"]
        .as_array()
        .expect("violations")
        .iter()
        .map(|violation| violation["field"].as_str().expect("field"))
        .collect::<Vec<_>>();
    assert_eq!(fields, ["address.street", "items[0].street", "code"]);
}

#[test]
fn strong_value_decode_errors_include_the_nested_json_path() {
    let mut body = valid_body();
    body["id"] = json!("not-a-uuid");

    let response = future::block_on(
        TestApp::new(&app()).call(
            Request::post("/validation")
                .json(&body)
                .expect("fixture should serialize"),
        ),
    );
    assert_eq!(response.status(), 422);
    let body = response.json::<Value>().expect("decode error JSON");
    assert_eq!(body["error"]["code"], "invalid_json");
    assert_eq!(body["error"]["details"]["field"], "id");

    // A one-segment path is reachable without any real path tracking. This
    // case forces `serde_path_to_error` to reconstruct a map key, a sequence
    // index, and a nested struct field from the JSON deserializer, which is
    // the part of the contract that depends on how the deserializer drives
    // `MapAccess` and `SeqAccess`.
    let mut body = valid_body();
    body["items"][0]["street"] = json!(7);

    let response = future::block_on(
        TestApp::new(&app()).call(
            Request::post("/validation")
                .json(&body)
                .expect("fixture should serialize"),
        ),
    );
    assert_eq!(response.status(), 422);
    let body = response.json::<Value>().expect("decode error JSON");
    assert_eq!(body["error"]["code"], "invalid_json");
    assert_eq!(body["error"]["details"]["field"], "items[0].street");
    assert_eq!(
        body["error"]["details"]["violations"][0]["field"],
        "items[0].street"
    );
}
