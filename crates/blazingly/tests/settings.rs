//! Typed configuration, checked at startup.
//!
//! The property that matters is not that a settings struct can be filled — it
//! is that a misconfigured deployment learns everything wrong with it from one
//! failed boot, and that a value someone deliberately wrote is never silently
//! discarded in favour of a default.

use blazingly::config::{ConfigProblem, ConfigSource, Layered, MapSource, Settings};
use blazingly::prelude::*;
use blazingly::settings;
use std::time::Duration;

#[settings(prefix = "APP_")]
#[derive(Debug)]
struct AppSettings {
    #[min_length(1)]
    database_url: String,
    #[default("8080")]
    port: u16,
    #[default("30s")]
    request_timeout: Duration,
    #[default("")]
    allowed_origins: Vec<String>,
    #[default("false")]
    debug: bool,
    sentry_dsn: Option<String>,
    #[env("LEGACY_SECRET")]
    #[min_length(8)]
    #[max_length(64)]
    secret: String,
}

fn complete() -> MapSource {
    MapSource::new()
        .with("APP_DATABASE_URL", "postgres://localhost/app")
        .with("LEGACY_SECRET", "correct-horse-battery")
}

#[test]
fn defaults_fill_what_the_deployment_did_not_set() {
    let settings = AppSettings::load(&complete()).expect("the required variables are present");

    assert_eq!(settings.database_url, "postgres://localhost/app");
    assert_eq!(settings.port, 8080);
    assert_eq!(settings.request_timeout, Duration::from_secs(30));
    assert!(settings.allowed_origins.is_empty());
    assert!(!settings.debug);
    assert_eq!(settings.sentry_dsn, None);
}

#[test]
fn a_field_name_becomes_a_prefixed_variable_unless_it_says_otherwise() {
    let variables = AppSettings::variables()
        .into_iter()
        .map(|setting| setting.variable)
        .collect::<Vec<_>>();

    assert_eq!(
        variables,
        [
            "APP_DATABASE_URL",
            "APP_PORT",
            "APP_REQUEST_TIMEOUT",
            "APP_ALLOWED_ORIGINS",
            "APP_DEBUG",
            "APP_SENTRY_DSN",
            // `#[env]` opts out of the prefix entirely, which is how a service
            // keeps reading a variable it inherited.
            "LEGACY_SECRET",
        ]
    );
}

#[test]
fn every_missing_variable_is_reported_from_one_failed_load() {
    let error = AppSettings::load(&MapSource::new()).expect_err("nothing is set");

    assert_eq!(
        error.missing().collect::<Vec<_>>(),
        ["APP_DATABASE_URL", "LEGACY_SECRET"],
        "a deployment missing two variables learns both at once, not one per boot"
    );
    let text = error.to_string();
    assert!(text.contains("APP_DATABASE_URL"), "{text}");
    assert!(text.contains("LEGACY_SECRET"), "{text}");
}

#[test]
fn a_value_that_does_not_parse_says_what_it_should_have_been() {
    let source = complete().with("APP_PORT", "eighty-eighty");
    let error = AppSettings::load(&source).expect_err("the port is not a number");

    let (variable, problem) = &error.problems()[0];
    assert_eq!(variable, "APP_PORT");
    assert_eq!(
        problem,
        &ConfigProblem::Unparsable {
            value: "eighty-eighty".to_owned(),
            expected: "16-bit unsigned integer",
        }
    );
}

#[test]
fn an_optional_variable_that_is_set_but_wrong_is_still_an_error() {
    // The failure this crate exists to prevent: quietly discarding a value a
    // deployment deliberately wrote.
    #[settings]
    #[derive(Debug)]
    struct Optional {
        retries: Option<u8>,
    }

    let source = MapSource::new().with("RETRIES", "many");
    let error = Optional::load(&source).expect_err("`many` is not a `u8`");
    assert_eq!(error.problems().len(), 1);

    let absent = Optional::load(&MapSource::new()).expect("an unset optional is `None`");
    assert_eq!(absent.retries, None);
}

#[test]
fn a_value_within_its_bounds_reaches_the_field() {
    let settings = AppSettings::load(&complete()).expect("the secret is long enough");
    assert_eq!(settings.secret, "correct-horse-battery");
}

#[test]
fn declared_bounds_are_checked_and_reported_with_the_offending_value() {
    let source = complete().with("LEGACY_SECRET", "short");
    let error = AppSettings::load(&source).expect_err("the secret is under eight characters");

    let (variable, problem) = &error.problems()[0];
    assert_eq!(variable, "LEGACY_SECRET");
    assert!(
        matches!(problem, ConfigProblem::Invalid { value, .. } if value == "short"),
        "{problem:?}"
    );
}

#[test]
fn durations_read_the_way_deployments_write_them() {
    for (written, expected) in [
        ("500ms", Duration::from_millis(500)),
        ("45s", Duration::from_secs(45)),
        ("5m", Duration::from_secs(300)),
        ("2h", Duration::from_secs(7_200)),
        // A bare number is seconds, which is what most deployments mean.
        ("90", Duration::from_secs(90)),
    ] {
        let source = complete().with("APP_REQUEST_TIMEOUT", written);
        let settings = AppSettings::load(&source).expect("the duration parses");
        assert_eq!(settings.request_timeout, expected, "`{written}`");
    }
}

#[test]
fn booleans_accept_what_deployments_actually_write() {
    for written in ["true", "1", "yes", "on", "TRUE", "Yes"] {
        let source = complete().with("APP_DEBUG", written);
        assert!(
            AppSettings::load(&source).expect("parses").debug,
            "`{written}` should be true"
        );
    }
    for written in ["false", "0", "no", "off"] {
        let source = complete().with("APP_DEBUG", written);
        assert!(
            !AppSettings::load(&source).expect("parses").debug,
            "`{written}` should be false"
        );
    }
    let source = complete().with("APP_DEBUG", "perhaps");
    assert!(AppSettings::load(&source).is_err());
}

#[test]
fn a_list_is_comma_separated_and_trimmed() {
    let source = complete().with(
        "APP_ALLOWED_ORIGINS",
        " https://a.example , https://b.example ",
    );
    let settings = AppSettings::load(&source).expect("the list parses");

    assert_eq!(
        settings.allowed_origins,
        ["https://a.example", "https://b.example"]
    );
}

#[test]
fn layered_sources_let_an_earlier_layer_win() {
    let overrides = MapSource::new().with("APP_PORT", "9000");
    let base = complete().with("APP_PORT", "8080");
    let layered = Layered::new().under(&overrides).under(&base);

    assert_eq!(
        AppSettings::load(&layered)
            .expect("the layers resolve")
            .port,
        9000
    );
}

/// The point of the crate: a handler asks for its configuration the same way it
/// asks for anything else, and the process refused to start without it.
#[get("/config", id = "config.read", summary = "Report the configured port")]
async fn read_config(settings: Depends<AppSettings>) -> Json<u16> {
    Json(settings.port)
}

#[test]
fn settings_reach_a_handler_as_an_injected_dependency() {
    let settings = AppSettings::load(&complete()).expect("the configuration is complete");
    let application = ExecutableApp::from_plugin(
        Plugin::new("configured")
            .provide(Provider::value(settings))
            .routes(routes![read_config]),
    )
    .expect("the operation graph compiles");

    let response =
        futures_lite::future::block_on(TestApp::new(&application).call(Request::get("/config")));
    assert_eq!(response.status(), 200);
    assert_eq!(response.body(), b"8080");
}

#[test]
fn the_descriptor_says_what_a_deployment_has_to_provide() {
    let required = AppSettings::variables()
        .into_iter()
        .filter(|setting| setting.required)
        .map(|setting| setting.variable)
        .collect::<Vec<_>>();

    assert_eq!(
        required,
        ["APP_DATABASE_URL", "LEGACY_SECRET"],
        "an Option field and a defaulted field are both not required"
    );
}

#[test]
fn the_environment_source_reads_a_variable_the_test_runner_set() {
    // Not `set_var`: it is unsafe in Rust 2024 and this workspace forbids
    // unsafe. Cargo sets this one for every test process, which is enough to
    // prove the real source reads the real environment.
    assert!(Environment.get("CARGO_PKG_NAME").is_some());
    assert_eq!(Environment.get("BLAZINGLY_DEFINITELY_UNSET_XYZ"), None);
}
