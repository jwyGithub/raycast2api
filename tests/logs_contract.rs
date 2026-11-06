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
        model_overrides: Default::default(),
        accounts_db: ":memory:".into(),
        proxies: Vec::new(),
        proxies_enabled: false,
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

async fn request(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    key: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    if let Some(key) = key {
        builder = builder.header("authorization", format!("Bearer {key}"));
    }
    let request = builder
        .body(Body::from(
            body.map(|body| body.to_string()).unwrap_or_default(),
        ))
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

async fn get(app: &axum::Router, uri: &str, key: Option<&str>) -> (StatusCode, Value) {
    request(app, Method::GET, uri, None, key).await
}

#[tokio::test]
async fn list_and_detail_round_trip() {
    let (app, log) = app();
    log.insert_request("req_1", "POST", "/v1/responses", &json!({"model": "m"}));
    log.update_upstream("req_1", &json!({"provider": "openai"}));
    log.update_response("req_1", "data: {\"text\":\"hi\"}");

    // 列表:不含任何 body 字段,含大小。
    let (status, body) = request(
        &app,
        Method::POST,
        "/v1/logs/list",
        Some(json!({})),
        Some("test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["page"], 1);
    assert_eq!(body["data"]["page_size"], 10);
    let (status, camel_case) = request(
        &app,
        Method::POST,
        "/v1/logs/list",
        Some(json!({"pageSize": 1})),
        Some("test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(camel_case["data"].is_null());
    let arr = body["data"]["items"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["request_id"], "req_1");
    assert_eq!(arr[0]["method"], "POST");
    assert!(arr[0].get("inbound_json").is_none());
    assert!(arr[0].get("response_text").is_none());
    assert!(arr[0]["response_bytes"].as_i64().unwrap() > 0);

    // 详情:三体齐全。
    let (status, body) = request(
        &app,
        Method::POST,
        "/v1/logs/detail",
        Some(json!({"request_id": "req_1"})),
        Some("test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["request_id"], "req_1");
    assert!(
        body["data"]["inbound_json"]
            .as_str()
            .unwrap()
            .contains("\"model\"")
    );
    assert!(
        body["data"]["upstream_json"]
            .as_str()
            .unwrap()
            .contains("openai")
    );
    assert_eq!(body["data"]["response_text"], "data: {\"text\":\"hi\"}");
}

#[tokio::test]
async fn detail_unknown_id_is_404() {
    let (app, _log) = app();
    let (status, _) = request(
        &app,
        Method::POST,
        "/v1/logs/detail",
        Some(json!({"request_id": "nope"})),
        Some("test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_filters_by_created_at_and_request_id() {
    let (app, log) = app();
    log.insert_request("filter-old", "POST", "/a", &json!({}));
    let created_at = log.detail("filter-old").unwrap().created_at;
    log.insert_request("filter-new", "POST", "/b", &json!({}));

    for (filter, total, expected) in [
        (json!({ "created_at": created_at }), 2, "filter-new"),
        (json!({ "request_id": "filter-new" }), 1, "filter-new"),
        (
            json!({ "created_at": created_at, "request_id": "filter-old" }),
            1,
            "filter-old",
        ),
    ] {
        let (status, body) = request(
            &app,
            Method::POST,
            "/v1/logs/list",
            Some(filter),
            Some("test-key"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["total"], total);
        assert_eq!(body["data"]["items"][0]["request_id"], expected);
    }
}

#[tokio::test]
async fn list_supports_paging_and_rejects_console_route() {
    let (app, log) = app();
    for index in 0..55 {
        log.insert_request(
            &format!("request-{index}"),
            "POST",
            "/a",
            &json!({"v": index}),
        );
    }

    let (status, all) = request(
        &app,
        Method::POST,
        "/v1/logs/list",
        Some(json!({})),
        Some("test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(all["data"]["items"].as_array().unwrap().len(), 10);
    assert_eq!(all["data"]["total"], 55);

    let (status, _) = request(
        &app,
        Method::POST,
        "/console/api/logs/list",
        Some(json!({"page": 1, "page_size": 1})),
        Some("test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn page_size_out_of_range_is_400() {
    let (app, _log) = app();
    let (status, _) = request(
        &app,
        Method::POST,
        "/v1/logs/list",
        Some(json!({"page_size": 0})),
        Some("test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(
        &app,
        Method::POST,
        "/v1/logs/list",
        Some(json!({"page_size": 101})),
        Some("test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn missing_api_key_is_unauthorized() {
    let (app, _log) = app();
    let (status, _) = request(&app, Method::POST, "/v1/logs/list", Some(json!({})), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn non_inference_routes_are_not_captured() {
    let (app, log) = app();
    // /health is auth-exempt; a probe must not write a log row.
    let (status, _) = get(&app, "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, health) = get(&app, "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["status"], "ok");
    assert!(health.get("code").is_none());
    assert!(
        matches!(log.list((1, 10), None, None).unwrap(), page if page.total == 0 && page.items.is_empty())
    );
    // An authenticated non-inference route is not captured either.
    let (status, _) = request(
        &app,
        Method::POST,
        "/v1/logs/list",
        Some(json!({})),
        Some("test-key"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        matches!(log.list((1, 10), None, None).unwrap(), page if page.total == 0 && page.items.is_empty())
    );

    let (status, error) = get(&app, "/v1/unknown", Some("test-key")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error["code"], 404);
    assert_eq!(error["message"], "Not Found");
    assert!(error["data"].is_null());
}
