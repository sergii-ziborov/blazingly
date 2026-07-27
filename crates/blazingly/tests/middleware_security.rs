use blazingly::prelude::*;
use blazingly::{HttpMethod, SecurityLocation, SecuritySchemeDescriptor, SecuritySchemeKind};
use flate2::read::GzDecoder;
use futures_lite::future;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[get("/public", id = "middleware.public")]
#[allow(clippy::unused_async)]
async fn public() -> Json<String> {
    Json("blazingly ".repeat(256))
}

#[get("/secure", id = "middleware.secure")]
#[security("oauth", scopes = ["orders:read"])]
#[allow(clippy::unused_async)]
async fn secure(Extension(security): Extension<SecurityContext>) -> Json<String> {
    Json(
        security
            .primary()
            .and_then(|identity| identity.subject.clone())
            .unwrap_or_default(),
    )
}

#[get("/session", id = "middleware.session")]
#[security("session")]
#[allow(clippy::unused_async)]
async fn session(Extension(security): Extension<SecurityContext>) -> Json<String> {
    Json(
        security
            .primary()
            .and_then(|identity| identity.subject.clone())
            .unwrap_or_default(),
    )
}

fn public_app() -> ExecutableApp {
    ExecutableApp::new(routes![public]).expect("public operation should compile")
}

fn secured_app() -> ExecutableApp {
    ExecutableApp::with_security_schemes(
        routes![secure, session],
        [
            SecuritySchemeDescriptor::new(
                "oauth",
                SecuritySchemeKind::OAuth2 {
                    authorization_url: None,
                    token_url: Some("/token".to_owned()),
                    scopes: vec!["orders:read".to_owned()],
                },
            ),
            SecuritySchemeDescriptor::new(
                "session",
                SecuritySchemeKind::ApiKey {
                    location: SecurityLocation::Cookie,
                    name: "blazingly_session".to_owned(),
                },
            ),
        ],
    )
    .expect("secured operation graph should compile")
}

fn expires_in(seconds: u64) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_secs()
        + seconds
}

#[test]
fn cors_preflight_short_circuits_before_routing() {
    let executable = public_app();
    let app = TestApp::new(&executable).with_middleware(
        Cors::new()
            .allow_origin("https://app.example")
            .allow_methods([HttpMethod::Get])
            .allow_header("authorization")
            .max_age(Duration::from_secs(600)),
    );
    let response = future::block_on(
        app.call(
            Request::options("/public")
                .header("origin", "https://app.example")
                .header("access-control-request-method", "GET")
                .header("access-control-request-headers", "authorization"),
        ),
    );
    assert_eq!(response.status(), 204);
    assert_eq!(
        response.get_header("access-control-allow-origin"),
        Some("https://app.example")
    );
    assert_eq!(response.get_header("access-control-max-age"), Some("600"));
}

#[test]
fn proxy_host_policy_and_per_client_rate_limit_share_effective_context() {
    let executable = public_app();
    let loopback = "127.0.0.0/8".parse::<IpNetwork>().expect("CIDR");
    let app = TestApp::new(&executable)
        .with_middleware(ProxyHeaders::new().trust(loopback))
        .with_middleware(TrustedHost::new(["api.example"]))
        .with_middleware(RateLimit::per_client(1, Duration::from_secs(60)));
    let request = |client: &str| {
        Request::get("/public")
            .peer_addr(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9000).into())
            .header("host", "internal:8080")
            .header("x-forwarded-for", client)
            .header("x-forwarded-host", "api.example")
            .header("x-forwarded-proto", "https")
    };

    assert_eq!(
        future::block_on(app.call(request("198.51.100.1"))).status(),
        200
    );
    assert_eq!(
        future::block_on(app.call(request("198.51.100.1"))).status(),
        429
    );
    assert_eq!(
        future::block_on(app.call(request("198.51.100.2"))).status(),
        200
    );
}

#[test]
fn gzip_compression_is_negotiated_and_reversible() {
    let executable = public_app();
    let app = TestApp::new(&executable).with_middleware(Compression::new().minimum_size(1));
    let response =
        future::block_on(app.call(Request::get("/public").header("accept-encoding", "gzip")));
    assert_eq!(response.status(), 200);
    assert_eq!(response.get_header("content-encoding"), Some("gzip"));
    let mut decoder = GzDecoder::new(response.body());
    let mut plain_body = String::new();
    decoder.read_to_string(&mut plain_body).expect("valid gzip");
    assert!(plain_body.contains("blazingly"));
}

#[test]
fn jwt_oauth_scopes_and_typed_identity_are_enforced() {
    let executable = secured_app();
    let jwt = JwtHs256::new(b"0123456789abcdef0123456789abcdef").expect("strong key");
    let app = TestApp::new(&executable)
        .with_middleware(Security::new().verifier("oauth", OAuth2Bearer::new(jwt.clone())));

    let missing = future::block_on(app.call(Request::get("/secure")));
    assert_eq!(missing.status(), 401);
    // RFC 6750 section 3: the challenge carries the scheme and a realm.
    assert_eq!(
        missing.get_header("www-authenticate"),
        Some("Bearer realm=\"api\"")
    );

    let token_without_scope = jwt
        .encode(&JwtClaims::new("agent-1", expires_in(60)))
        .expect("token");
    let forbidden = future::block_on(app.call(
        Request::get("/secure").header("authorization", format!("Bearer {token_without_scope}")),
    ));
    assert_eq!(forbidden.status(), 403);

    let token = jwt
        .encode(&JwtClaims::new("agent-1", expires_in(60)).scope("orders:read"))
        .expect("token");
    let response = future::block_on(
        app.call(Request::get("/secure").header("authorization", format!("Bearer {token}"))),
    );
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<String>().expect("identity"), "agent-1");
}

#[test]
fn signed_session_cookie_uses_the_same_security_pipeline() {
    let executable = secured_app();
    let jwt = JwtHs256::new(b"0123456789abcdef0123456789abcdef").expect("strong key");
    let sessions = SignedSession::new("blazingly_session", jwt);
    let cookie = sessions
        .cookie(&JwtClaims::new("user-9", expires_in(60)), 60)
        .expect("signed cookie");
    let request_cookie = cookie.split(';').next().expect("cookie pair");
    let app =
        TestApp::new(&executable).with_middleware(Security::new().verifier("session", sessions));

    let response =
        future::block_on(app.call(Request::get("/session").header("cookie", request_cookie)));
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<String>().expect("identity"), "user-9");
}
