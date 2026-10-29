//! `/v1/accounts` CRUD 的契约测试(内存库;refresh 端点只验证 202)。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use raycast2api::account::store::AccountStore;
use raycast2api::config::Settings;
use raycast2api::db::Db;
use raycast2api::server::build_router;
use serde_json::Value;
use tower::ServiceExt;

fn app() -> (axum::Router, Arc<AccountStore>) {
    let store = Arc::new(AccountStore::new(Db::open(":memory:").unwrap()));
    let settings = Arc::new(Settings {
        raycast_device_id: "device".into(),
        raycast_signature_secret: None,
        api_key: Some("test-key".into()),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        bind_addr_dual: None,
        show_advanced: true,
        include_deprecated: true,
        model_overrides: Default::default(),
        accounts_db: ":memory:".into(),
        proxies: Vec::new(),
    });
    let log_store = Arc::new(raycast2api::log::store::RequestLogStore::new(
        Db::open(":memory:").unwrap(),
    ));
    let client = raycast2api::raycast::client::RaycastClient::new(
        reqwest::Client::new(),
        settings.clone(),
        store.clone(),
        log_store.clone(),
    );
    let state = Arc::new(raycast2api::server::state::AppState::new(
        settings,
        client,
        store.clone(),
        log_store,
    ));
    (build_router(state), store)
}

async fn call(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    builder = builder.header("authorization", "Bearer test-key");
    let request = builder
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

fn account_body(email: &str, token: &str) -> Value {
    serde_json::json!({
        "tag": "t",
        "user": { "email": email },
        "access_token": token
    })
}

#[tokio::test]
async fn list_upsert_delete_round_trip() {
    let (app, _store) = app();

    // 空列表
    let (status, body) = call(&app, Method::GET, "/v1/accounts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);

    // 新增
    let (status, body) = call(
        &app,
        Method::POST,
        "/v1/accounts",
        Some(account_body("a@x", "rca_token123")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"], "inserted");

    // 重复同 token → unchanged
    let (_, body) = call(
        &app,
        Method::POST,
        "/v1/accounts",
        Some(account_body("a@x", "rca_token123")),
    )
    .await;
    assert_eq!(body["result"], "unchanged");

    // token 变化 → updated
    let (_, body) = call(
        &app,
        Method::POST,
        "/v1/accounts",
        Some(account_body("a@x", "rca_token999")),
    )
    .await;
    assert_eq!(body["result"], "updated");

    // 列表:token 掩码
    let (status, body) = call(&app, Method::GET, "/v1/accounts", None).await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["email"], "a@x");
    assert_eq!(arr[0]["access_token"], "rca_toke...");
    assert_eq!(arr[0]["available"], true);

    // 删除 → 204;再删 → 404
    let (status, _) = call(&app, Method::DELETE, "/v1/accounts/a@x", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app, Method::DELETE, "/v1/accounts/a@x", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn refresh_endpoint_returns_accepted() {
    let (app, _store) = app();
    let (status, _) = call(&app, Method::POST, "/v1/accounts/refresh", None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn missing_api_key_is_unauthorized() {
    let (app, _store) = app();
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/accounts")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn anthropic_x_api_key_is_accepted() {
    let (app, _store) = app();
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/accounts")
        .header("x-api-key", "test-key")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn wrong_x_api_key_is_unauthorized() {
    let (app, _store) = app();
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/accounts")
        .header("x-api-key", "wrong-key")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
