//! Dashboard sessions and independent API-key authentication contracts.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use raycast2api::account::store::AccountStore;
use raycast2api::config::Settings;
use raycast2api::log::store::RequestLogStore;
use raycast2api::raycast::client::RaycastClient;
use raycast2api::server::{build_router, state::AppState};
use raycast2api::storage::db::Db;
use serde_json::{Value, json};
use tower::ServiceExt;

fn state(password: Option<&str>, api_key: Option<&str>) -> AppState {
    let db = Db::open(":memory:").unwrap();
    let accounts = Arc::new(AccountStore::new(db.clone()));
    let statistics = Arc::new(raycast2api::statistics::store::StatisticsStore::new(
        db.clone(),
    ));
    let log = Arc::new(RequestLogStore::new(db));
    let settings = Arc::new(Settings {
        raycast_device_id: "device".into(),
        raycast_signature_secret: "secret".into(),
        api_key: api_key.map(str::to_owned),
        dashboard_password: password.map(str::to_owned),
        dashboard_session_secret: Some("s".repeat(32)),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        bind_addr_dual: None,
        upstream_timeout_secs: 20,
        accounts_db: ":memory:".into(),
        proxies: Vec::new(),
        proxies_mode: raycast2api::proxies::ProxyMode::Direct,
    });
    let client = RaycastClient::new(
        reqwest::Client::new(),
        settings.clone(),
        accounts.clone(),
        log.clone(),
        statistics,
    );
    AppState::new(settings, client, accounts, log)
}

fn app(password: Option<&str>, api_key: Option<&str>) -> axum::Router {
    build_router(Arc::new(state(password, api_key)))
}

async fn call(
    app: &axum::Router,
    path: &str,
    body: Value,
    header: Option<(&str, &str)>,
) -> (StatusCode, Value) {
    let (status, _, body) = send(app, path, body, &header.into_iter().collect::<Vec<_>>()).await;
    (status, body)
}

async fn send(
    app: &axum::Router,
    path: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    if path == "/v1/login" && status.is_success() {
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
#[ignore = "requires redis-server on PATH; starts a private Unix-socket instance"]
async fn redis_session_lifecycle_and_authentication_boundaries() {
    use redis::AsyncCommands;
    use std::process::{Child, Command, Stdio};

    struct RedisServer(Child, std::path::PathBuf);
    impl Drop for RedisServer {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
            let _ = std::fs::remove_file(&self.1);
        }
    }
    let socket = std::env::temp_dir().join(format!("r2a-{}.sock", raycast2api::util::new_uuid()));
    let server = RedisServer(
        Command::new("redis-server")
            .args([
                "--port",
                "0",
                "--save",
                "",
                "--appendonly",
                "no",
                "--unixsocketperm",
                "700",
                "--unixsocket",
            ])
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        socket.clone(),
    );
    let client = redis::Client::open(format!("redis+unix://{}?db=1", socket.display())).unwrap();
    let mut connection = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match client.get_connection_manager().await {
                Ok(connection) => break connection,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .unwrap();
    let mut first = state(Some("password"), Some("api-key"));
    first.redis = Some(connection.clone());
    let app = build_router(Arc::new(first));
    let (status, headers, body) = send(
        &app,
        "/v1/login",
        json!({"dashboard_password": "password"}),
        &[("x-forwarded-proto", "https")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], json!({"expires_in": 86400}));
    let set_cookie = headers["set-cookie"].to_str().unwrap();
    for attribute in [
        "HttpOnly",
        "SameSite=Strict",
        "Max-Age=86400",
        "Secure",
        "Path=/v1",
    ] {
        assert!(set_cookie.contains(attribute), "{set_cookie}");
    }
    let cookie = set_cookie.split(';').next().unwrap().to_owned();
    let auth = [("cookie", cookie.as_str()), ("x-dashboard-request", "1")];
    let keys: Vec<String> = connection
        .keys("raycast2api:dashboard:session:*")
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);
    assert!(!keys[0].contains(cookie.split('=').nth(1).unwrap()));
    let ttl: i64 = connection.ttl(&keys[0]).await.unwrap();
    assert!((86390..=86400).contains(&ttl));
    for path in [
        "/v1/session",
        "/v1/accounts/list",
        "/v1/logs/list",
        "/v1/statistics/list",
    ] {
        assert_eq!(
            send(&app, path, json!({}), &auth).await.0,
            StatusCode::OK,
            "{path}"
        );
    }
    assert_eq!(
        send(&app, "/v1/accounts/list", json!({}), &[("cookie", &cookie)])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    for path in ["/v1/chat/completions", "/v1/messages", "/v1/responses"] {
        assert_eq!(
            send(&app, path, json!({}), &auth).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let mut tampered = cookie.clone();
    tampered.pop();
    tampered.push('g');
    assert_eq!(
        send(
            &app,
            "/v1/session",
            json!({}),
            &[("cookie", &tampered), ("x-dashboard-request", "1")]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );

    // A new application instance restores the session, even with no API key.
    let mut second = state(Some("password"), None);
    second.redis = Some(connection.clone());
    let second = build_router(Arc::new(second));
    assert_eq!(
        send(&second, "/v1/session", json!({}), &auth).await.0,
        StatusCode::OK
    );
    assert_eq!(
        send(&second, "/v1/accounts/list", json!({}), &auth).await.0,
        StatusCode::OK
    );
    for change_password in [true, false] {
        let mut changed = state(
            Some(if change_password {
                "new-password"
            } else {
                "password"
            }),
            None,
        );
        if !change_password {
            Arc::make_mut(&mut changed.settings).dashboard_session_secret = Some("x".repeat(32));
        }
        changed.redis = Some(connection.clone());
        let changed = build_router(Arc::new(changed));
        assert_eq!(
            send(&changed, "/v1/session", json!({}), &auth).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (status, headers, _) = send(&app, "/v1/logout", Value::Null, &auth).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    assert!(!connection.exists::<_, bool>(&keys[0]).await.unwrap());
    assert_eq!(
        send(&app, "/v1/session", json!({}), &auth).await.0,
        StatusCode::UNAUTHORIZED
    );

    let (_, headers, _) = send(
        &app,
        "/v1/login",
        json!({"dashboard_password": "password"}),
        &[],
    )
    .await;
    assert!(!headers["set-cookie"].to_str().unwrap().contains("Secure"));
    let cookie = headers["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let auth = [("cookie", cookie), ("x-dashboard-request", "1")];
    let keys: Vec<String> = connection
        .keys("raycast2api:dashboard:session:*")
        .await
        .unwrap();
    connection.expire::<_, ()>(&keys[0], 0).await.unwrap();
    assert_eq!(
        send(&app, "/v1/session", json!({}), &auth).await.0,
        StatusCode::UNAUTHORIZED
    );

    // Losing Redis denies session access while API-key management stays available.
    drop(server);
    assert_eq!(
        send(&app, "/v1/session", json!({}), &auth).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        call(
            &app,
            "/v1/accounts/list",
            json!({}),
            Some(("authorization", "Bearer api-key"))
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn login_compares_exact_password_without_requiring_api_key() {
    let app = app(Some(" 控制台 password "), Some("api-key"));
    let (status, body) = call(
        &app,
        "/v1/login",
        json!({"dashboard_password": " 控制台 password "}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body["message"].as_str().unwrap().contains("REDIS_HOST"));
    assert!(body["data"].is_null());
    for password in ["wrong", "", "控制台 password", "api-key"] {
        let (status, body) = call(
            &app,
            "/v1/login",
            json!({"dashboard_password": password}),
            Some(("authorization", "Bearer api-key")),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], 401);
        assert!(body["message"].as_str().unwrap().contains("密码错误"));
        assert!(body["data"].is_null());
    }
}

#[tokio::test]
async fn missing_or_empty_configuration_does_not_allow_login() {
    for password in [None, Some("")] {
        let app = app(password, None);
        let (status, body) = call(&app, "/v1/login", json!({"dashboard_password": ""}), None).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body["data"].is_null());
    }
}

#[tokio::test]
async fn login_requires_post_and_a_string_password() {
    let app = app(Some("password"), Some("api-key"));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    for body in [json!({}), json!({"dashboard_password": 123})] {
        let (status, body) = call(&app, "/v1/login", body, None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body["data"].is_null());
    }
}

#[tokio::test]
async fn management_requires_auth_even_without_an_api_key() {
    let app = app(Some("password"), None);
    for path in [
        "/v1/accounts/list",
        "/v1/logs/list",
        "/v1/statistics/list",
        "/v1/session",
        "/v1/logout",
    ] {
        let (status, _) = call(&app, path, json!({}), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn dashboard_password_cannot_authorize_management_or_inference() {
    let app = app(Some("password"), Some("api-key"));
    for path in [
        "/v1/accounts/list",
        "/v1/logs/list",
        "/v1/statistics/list",
        "/v1/chat/completions",
        "/v1/messages",
        "/v1/responses",
    ] {
        for header in [
            ("authorization", "Bearer password"),
            ("x-api-key", "password"),
            ("x-dashboard-token", "password"),
        ] {
            let (status, _) = call(&app, path, json!({}), Some(header)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
    }
}

#[tokio::test]
async fn api_authentication_is_independent_of_dashboard_password_and_login() {
    for password in [None, Some("password"), Some("new-password")] {
        let app = app(password, Some("api-key"));
        for name in ["authorization", "x-api-key"] {
            let value = if name == "authorization" {
                "Bearer api-key"
            } else {
                "api-key"
            };
            let (status, _) = call(&app, "/v1/accounts/list", json!({}), Some((name, value))).await;
            assert_eq!(status, StatusCode::OK);
        }
    }
}

#[tokio::test]
async fn model_discovery_requires_api_key_while_health_probes_are_public() {
    let app = app(Some("password"), Some("api-key"));
    for (path, authenticated, expected) in [
        ("/v1/models", false, StatusCode::UNAUTHORIZED),
        ("/v1/models", true, StatusCode::SERVICE_UNAVAILABLE),
        ("/health", false, StatusCode::OK),
        ("/ready", false, StatusCode::SERVICE_UNAVAILABLE),
    ] {
        let mut request = Request::builder().uri(path);
        if authenticated {
            request = request.header("authorization", "Bearer api-key");
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{path}");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body.get("code").is_none(), "{path}: {body}");
        if path == "/v1/models" {
            assert!(body.get("error").is_some(), "{path}: {body}");
        }
    }
}

#[tokio::test]
async fn console_page_and_assets_are_served_without_auth() {
    let app = app(Some("password"), Some("api-key"));
    for (path, content_type) in [
        ("/", "text/html"),
        ("/style.css", "text/css"),
        ("/utils.js", "text/javascript"),
        ("/components.js", "text/javascript"),
        ("/components/tooltip.js", "text/javascript"),
        ("/components/message.js", "text/javascript"),
        ("/components/button.js", "text/javascript"),
        ("/components/select.js", "text/javascript"),
        ("/components/date-picker.js", "text/javascript"),
        ("/views.js", "text/javascript"),
        ("/views/accounts.js", "text/javascript"),
        ("/views/logs.js", "text/javascript"),
        ("/views/statistics.js", "text/javascript"),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let actual = response.headers()["content-type"].to_str().unwrap();
        assert!(actual.starts_with(content_type), "{path}: {actual}");
        assert_eq!(response.headers()["cache-control"], "no-cache", "{path}");
        let etag = response.headers()["etag"].clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(!body.is_empty(), "{path}");

        // Revalidation with the ETag yields an empty 304 that keeps the cache headers.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("if-none-match", &etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{path}");
        assert_eq!(response.headers()["etag"], etag, "{path}");
        assert_eq!(response.headers()["cache-control"], "no-cache", "{path}");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(body.is_empty(), "{path}");

        // A stale ETag still gets the full body.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("if-none-match", "\"stale\"")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }

    // Only files under src/page are reachable; everything else falls through to 404.
    for path in ["/missing.js", "/../Cargo.toml", "/.DS_Store"] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}
