use blazingly::ExecutableBuildError;
use blazingly::prelude::*;
use futures_lite::future;

#[api_model]
#[derive(Clone, Debug)]
struct OverrideView {
    value: String,
}

#[derive(Clone)]
struct Settings(&'static str);

struct Unregistered;

#[get("/override/users", id = "override.users")]
async fn users_settings(settings: Depends<Settings>) -> Json<OverrideView> {
    Json(OverrideView {
        value: settings.0.to_owned(),
    })
}

#[get("/override/billing", id = "override.billing")]
async fn billing_settings(settings: Depends<Settings>) -> Json<OverrideView> {
    Json(OverrideView {
        value: settings.0.to_owned(),
    })
}

fn plugin() -> Plugin {
    Plugin::new("app")
        .provide(Provider::value(Settings("production")))
        .plugin(Plugin::new("users").routes(routes![users_settings]))
        .plugin(Plugin::new("billing").routes(routes![billing_settings]))
}

#[test]
fn test_overrides_can_replace_globally_or_shadow_one_plugin_scope() {
    let production = ExecutableApp::from_plugin(plugin()).expect("production graph should compile");
    assert_eq!(read(&production, "/override/users"), "production");
    assert_eq!(read(&production, "/override/billing"), "production");

    let global = ExecutableApp::from_plugin_with_overrides(
        plugin(),
        TestOverrides::new().replace(Provider::value(Settings("global-mock"))),
    )
    .expect("global override should compile");
    assert_eq!(read(&global, "/override/users"), "global-mock");
    assert_eq!(read(&global, "/override/billing"), "global-mock");

    let scoped = ExecutableApp::from_plugin_with_overrides(
        plugin(),
        TestOverrides::new().replace_in("app/users", Provider::value(Settings("users-mock"))),
    )
    .expect("scoped override should compile");
    assert_eq!(read(&scoped, "/override/users"), "users-mock");
    assert_eq!(read(&scoped, "/override/billing"), "production");
}

#[test]
fn test_overrides_reject_unknown_targets_instead_of_silently_passing() {
    let unknown_type = ExecutableApp::from_plugin_with_overrides(
        plugin(),
        TestOverrides::new().replace(Provider::value(Unregistered)),
    )
    .err()
    .expect("an unmatched global override should fail");
    assert!(matches!(
        unknown_type,
        ExecutableBuildError::UnknownProviderOverride { plugin: None, .. }
    ));

    let unknown_scope = ExecutableApp::from_plugin_with_overrides(
        plugin(),
        TestOverrides::new().replace_in("app/missing", Provider::value(Settings("missing"))),
    )
    .err()
    .expect("an unknown scoped override should fail");
    assert!(matches!(
        unknown_scope,
        ExecutableBuildError::UnknownProviderOverride {
            plugin: Some(plugin),
            ..
        } if plugin == "app/missing"
    ));
}

fn read(app: &ExecutableApp, path: &str) -> String {
    let response = future::block_on(TestApp::new(app).call(Request::get(path)));
    assert_eq!(response.status(), 200);
    response
        .json::<serde_json::Value>()
        .expect("override response should be JSON")["value"]
        .as_str()
        .expect("override response should contain a string")
        .to_owned()
}
