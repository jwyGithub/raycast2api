//! Console login returns the API key; all API authentication uses that key.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use raycast2api::account::store::AccountStore;
use raycast2api::config::Settings;
use raycast2api::db::Db;
use raycast2api::log::store::RequestLogStore;
use raycast2api::raycast::client::RaycastClient;
use raycast2api::server::{build_router, state::AppState};
use serde_json::{Value, json};
use tower::ServiceExt;

fn app(password: Option<&str>, api_key: Option<&str>) -> axum::Router {
    let db = Db::open(":memory:").unwrap();
    let accounts = Arc::new(AccountStore::new(db.clone()));
    let log = Arc::new(RequestLogStore::new(db));
    let settings = Arc::new(Settings {
        raycast_device_id: "device".into(),
        raycast_signature_secret: "secret".into(),
        api_key: api_key.map(str::to_owned),
        dashboard_password: password.map(str::to_owned),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        bind_addr_dual: None,
        model_overrides: Default::default(),
        upstream_timeout: std::time::Duration::from_secs(20),
        accounts_db: ":memory:".into(),
        proxies: Vec::new(),
        proxies_enabled: false,
    });
    let client = RaycastClient::new(
        reqwest::Client::new(),
        settings.clone(),
        accounts.clone(),
        log.clone(),
    );
    build_router(Arc::new(AppState::new(settings, client, accounts, log)))
}

async fn call(
    app: &axum::Router,
    path: &str,
    body: Value,
    header: Option<(&str, &str)>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json");
    if let Some((name, value)) = header {
        request = request.header(name, value);
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
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn login(app: &axum::Router, password: &str) -> Option<String> {
    let (status, body) = call(
        app,
        "/v1/login",
        json!({"dashboard_password": password}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["code"], 200);
    assert_eq!(body["message"], "success");
    assert!(body["data"].get("api_key").is_some());
    assert!(body["data"].get("token").is_none());
    assert!(body["data"].get("dashboard_password").is_none());
    body["data"]["api_key"].as_str().map(str::to_owned)
}

#[tokio::test]
async fn login_compares_exact_password_without_requiring_api_key() {
    let app = app(Some(" 控制台 password "), Some("api-key"));
    assert_eq!(
        login(&app, " 控制台 password ").await.as_deref(),
        Some("api-key")
    );
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
async fn login_returns_api_key_for_management_requests() {
    for api_key in [None, Some("api-key")] {
        let app = app(Some("password"), api_key);
        let returned_key = login(&app, "password").await;
        assert_eq!(returned_key.as_deref(), api_key);
        let authorization = returned_key.map(|key| format!("Bearer {key}"));
        for path in ["/v1/accounts/list", "/v1/logs/list"] {
            let (status, body) = call(
                &app,
                path,
                json!({}),
                authorization
                    .as_deref()
                    .map(|value| ("authorization", value)),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["data"]["items"], json!([]));
            for header in [None, Some(("authorization", "Bearer wrong-key"))] {
                let (status, _) = call(&app, path, json!({}), header).await;
                assert_eq!(
                    status,
                    if api_key.is_some() {
                        StatusCode::UNAUTHORIZED
                    } else {
                        StatusCode::OK
                    }
                );
            }
        }
        let mut request = Request::builder().uri("/v1/accounts/export");
        if let Some(value) = authorization {
            request = request.header("authorization", value);
        }
        let response = app
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if api_key.is_some() {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            }
        );
    }
}

#[tokio::test]
async fn dashboard_password_cannot_authorize_management_or_inference() {
    let app = app(Some("password"), Some("api-key"));
    for path in [
        "/v1/accounts/list",
        "/v1/logs/list",
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
