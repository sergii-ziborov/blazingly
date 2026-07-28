//! Cross-engine checks on the bytes the framework actually puts on the wire.
//!
//! Every crate in this workspace now encodes and decodes JSON with
//! `blazingly-json`, so a defect in that engine would be invisible to a test
//! that also reads the bytes back with it: a matching encoder and decoder agree
//! on a wrong format just as happily as on a right one. These tests deliberately
//! cross the boundary. The request is encoded by `serde_json` and the response
//! is parsed by `serde_json`, so the two independent implementations have to
//! agree on the literal bytes. `serde_json` is a dev-dependency of this crate
//! for exactly this reason and appears in no shipped dependency tree.

use blazingly::prelude::*;
use futures_lite::future;

#[api_model]
#[derive(Clone, Debug)]
struct WireEcho {
    text: String,
    signed: i64,
    unsigned: u64,
    ratio: f64,
    small: f64,
    flag: bool,
    tags: Vec<String>,
    nested: WireNested,
}

#[api_model]
#[derive(Clone, Debug)]
struct WireNested {
    label: String,
    counts: Vec<u64>,
}

#[post("/wire", id = "wire.echo")]
async fn wire_echo(Json(input): Json<WireEcho>) -> Json<WireEcho> {
    Json(input)
}

fn app() -> ExecutableApp {
    ExecutableApp::new(routes![wire_echo]).expect("the echo operation should compile")
}

/// Deliberately awkward: every string escape class JSON defines, the extreme
/// signed and unsigned integers, and two floats whose shortest round-trip
/// encoding differs from their literal form.
fn fixture() -> serde_json::Value {
    serde_json::json!({
        "text": "quote \" backslash \\ slash / newline \n tab \t control \u{1} snowman \u{2603} astral \u{1f680}",
        "signed": i64::MIN,
        "unsigned": u64::MAX,
        "ratio": 0.1_f64 + 0.2_f64,
        "small": 5e-324_f64,
        "flag": true,
        "tags": ["a", "", "\u{fc}n\u{ef}c\u{f6}d\u{e9}"],
        "nested": { "label": "inner", "counts": [0, 1, 9_007_199_254_740_993_u64] }
    })
}

fn echo(request_bytes: Vec<u8>) -> (u16, Vec<u8>) {
    let executable = app();
    let response = future::block_on(
        TestApp::new(&executable).call(
            Request::post("/wire")
                .header("content-type", "application/json")
                .body(request_bytes),
        ),
    );
    (response.status(), response.body().to_vec())
}

#[test]
fn serde_json_and_blazingly_json_agree_on_the_bytes_the_framework_emits() {
    let (status, body) = echo(serde_json::to_vec(&fixture()).expect("oracle encodes the fixture"));
    assert_eq!(status, 200);

    // The framework decoded a `serde_json` encoding and re-encoded it with
    // `blazingly-json`; the oracle has to read those bytes back as the same
    // document, exactly. `9007199254740993` also has to survive as an integer
    // rather than being widened to a float on the way through.
    let echoed: serde_json::Value =
        serde_json::from_slice(&body).expect("the oracle should parse the framework's own bytes");
    assert_eq!(echoed, fixture());

    // And the reverse direction: what the framework's own engine reads out of
    // those bytes must round-trip through the oracle's encoder unchanged.
    let native: blazingly_json::Value =
        blazingly_json::from_slice(&body).expect("the framework should parse its own bytes");
    let reencoded = blazingly_json::to_vec(&native).expect("re-encoding should succeed");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&reencoded).expect("oracle parses re-encoding"),
        fixture()
    );
}

#[test]
fn the_oracle_parses_the_served_openapi_document() {
    // The OpenAPI document is the largest JSON this workspace generates and the
    // one most likely to be read by a third-party tool, so it is worth asserting
    // that a foreign parser accepts the served bytes.
    let executable = app();
    let served = future::block_on(
        TestApp::new(&executable)
            .with_openapi(blazingly::openapi::OpenApiConfig::new("Oracle", "1.0.0"))
            .call(Request::get("/openapi.json")),
    );
    assert_eq!(served.status(), 200);

    let parsed: serde_json::Value = serde_json::from_slice(served.body())
        .expect("the oracle should parse the served OpenAPI document");
    assert_eq!(parsed["openapi"], "3.1.0");
    assert!(parsed["paths"]["/wire"]["post"].is_object());
}

/// The one place the two engines deliberately disagree, pinned so a change on
/// either side shows up in review rather than in a user's request.
///
/// `serde_json` without `arbitrary_precision` widens an integer literal that
/// fits neither `i64` nor `u64` into the nearest `f64` and accepts it, losing
/// precision silently. `blazingly-json` rejects it. That makes a body carrying
/// such a literal a 422 where it used to be accepted with a mangled value.
/// Non-finite results are rejected by both, and gradual underflow agrees.
#[test]
fn out_of_range_integers_are_rejected_rather_than_silently_widened() {
    for literal in [
        "99999999999999999999",  // > u64::MAX
        "-99999999999999999999", // < i64::MIN
        "18446744073709551616",  // u64::MAX + 1
        "-9223372036854775809",  // i64::MIN - 1
    ] {
        assert!(
            serde_json::from_str::<serde_json::Value>(literal)
                .expect("the oracle widens this to f64")
                .is_f64(),
            "oracle no longer widens {literal}"
        );
        assert!(
            blazingly_json::from_str::<blazingly_json::Value>(literal).is_err(),
            "framework engine no longer rejects {literal}"
        );
    }

    // A float literal that overflows f64 is refused by both engines.
    for literal in ["1e400", "1.7976931348623157e309"] {
        assert!(serde_json::from_str::<serde_json::Value>(literal).is_err());
        assert!(blazingly_json::from_str::<blazingly_json::Value>(literal).is_err());
    }

    // And gradual underflow to zero agrees, compared bit for bit so the
    // assertion is exact and does not lean on float equality.
    assert_eq!(
        blazingly_json::from_str::<f64>("1e-400")
            .expect("underflow parses")
            .to_bits(),
        serde_json::from_str::<f64>("1e-400")
            .expect("underflow parses")
            .to_bits()
    );
}

#[test]
fn a_body_the_oracle_rejects_is_rejected_by_the_framework_too() {
    for malformed in [
        &br#"{"text":"a",}"#[..],
        &br#"{"text":'a'}"#[..],
        &br#"{"text":"a" "signed":1}"#[..],
        &br#"{"signed":01}"#[..],
        &br#"{"ratio":NaN}"#[..],
        &br#"{"text":"unterminated}"#[..],
    ] {
        assert!(
            serde_json::from_slice::<serde_json::Value>(malformed).is_err(),
            "oracle unexpectedly accepted {:?}",
            String::from_utf8_lossy(malformed)
        );
        let (status, _) = echo(malformed.to_vec());
        assert_eq!(
            status,
            422,
            "framework accepted {:?}",
            String::from_utf8_lossy(malformed)
        );
    }
}
