//! `/v1/logs` 查询接口的契约测试(内存库)。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use raycast2api::account::store::AccountStore;
use raycast2api::config::Settings;
use raycast2api::db::Db;
use raycast2api::log::store::RequestLogStore;
use raycast2api::server::build_router;
use raycast2api::server::state::AppState;
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> (axum::Router, Arc<RequestLogStore>) {
    let db = Db::open(":memory:").unwrap();
    let store = Arc::new(AccountStore::new(db.clone()));
    let log_store = Arc::new(RequestLogStore::new(db));
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
    });
    let client = raycast2api::raycast::client::RaycastClient::new(
        reqwest::Client::new(),
        settings.clone(),
        store.clone(),
        log_store.clone(),
    );
    let state = Arc::new(AppState::new(settings, client, store, log_store.clone()));
    (build_router(state), log_store)
}

async fn get(app: &axum::Router, uri: &str, key: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(Method::GET).uri(uri);
    if let Some(key) = key {
        builder = builder.header("authorization", format!("Bearer {key}"));
    }
    let request = builder.body(Body::empty()).unwrap();
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

#[tokio::test]
async fn list_and_detail_round_trip() {
    let (app, log) = app();
    log.insert_request("req_1", "POST", "/v1/responses", &json!({"model": "m"}));
    log.update_upstream("req_1", &json!({"provider": "openai"}));
    log.update_response("req_1", "data: {\"text\":\"hi\"}");

    // 列表:不含任何 body 字段,含大小。
    let (status, body) = get(&app, "/v1/logs", Some("test-key")).await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["request_id"], "req_1");
    assert_eq!(arr[0]["method"], "POST");
    assert!(arr[0].get("inbound_json").is_none());
    assert!(arr[0].get("response_text").is_none());
    assert!(arr[0]["response_bytes"].as_i64().unwrap() > 0);

    // 详情:三体齐全。
    let (status, body) = get(&app, "/v1/logs/req_1", Some("test-key")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["request_id"], "req_1");
    assert!(body["inbound_json"].as_str().unwrap().contains("\"model\""));
    assert!(body["upstream_json"].as_str().unwrap().contains("openai"));
    assert_eq!(body["response_text"], "data: {\"text\":\"hi\"}");
}

#[tokio::test]
async fn detail_unknown_id_is_404() {
    let (app, _log) = app();
    let (status, _) = get(&app, "/v1/logs/nope", Some("test-key")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn limit_out_of_range_is_400() {
    let (app, _log) = app();
    let (status, _) = get(&app, "/v1/logs?limit=0", Some("test-key")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get(&app, "/v1/logs?limit=201", Some("test-key")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn missing_api_key_is_unauthorized() {
    let (app, _log) = app();
    let (status, _) = get(&app, "/v1/logs", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn non_inference_routes_are_not_captured() {
    let (app, log) = app();
    // /health is auth-exempt; a probe must not write a log row.
    let (status, _) = get(&app, "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(log.list(50, None).is_empty());
    // An authenticated non-inference route is not captured either.
    let (status, _) = get(&app, "/v1/logs", Some("test-key")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(log.list(50, None).is_empty());
}
