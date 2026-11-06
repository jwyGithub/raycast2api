//! `/v1/accounts` CRUD 的契约测试(内存库;refresh 端点只验证状态码)。

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
    app_with_api_key(Some("test-key"))
}

fn app_with_api_key(api_key: Option<&str>) -> (axum::Router, Arc<AccountStore>) {
    let store = Arc::new(AccountStore::new(Db::open(":memory:").unwrap()));
    let settings = Arc::new(Settings {
        raycast_device_id: "device".into(),
        raycast_signature_secret: None,
        api_key: api_key.map(str::to_owned),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        bind_addr_dual: None,
        model_overrides: Default::default(),
        accounts_db: ":memory:".into(),
        proxies: Vec::new(),
        proxies_enabled: false,
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
    let (status, body) = call(
        &app,
        Method::POST,
        "/v1/accounts/list",
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["code"], 200);
    assert_eq!(body["message"], "success");
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 0);

    let (status, error) = call(
        &app,
        Method::POST,
        "/v1/accounts/list",
        Some(serde_json::json!({"status": "invalid"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["code"], 400);
    assert_eq!(
        error["message"],
        "status must be one of all, available, cooldown, unavailable"
    );
    assert!(error["data"].is_null());

    // 新增
    let (status, body) = call(
        &app,
        Method::POST,
        "/v1/accounts/create",
        Some(account_body("a@x", "rca_token123")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["result"], "inserted");

    // 重复同 token → unchanged
    let (_, body) = call(
        &app,
        Method::POST,
        "/v1/accounts/create",
        Some(account_body("a@x", "rca_token123")),
    )
    .await;
    assert_eq!(body["data"]["result"], "unchanged");

    // token 变化 → updated
    let (_, body) = call(
        &app,
        Method::POST,
        "/v1/accounts/create",
        Some(account_body("a@x", "rca_token999")),
    )
    .await;
    assert_eq!(body["data"]["result"], "updated");

    // 列表:token 掩码
    let (status, body) = call(
        &app,
        Method::POST,
        "/v1/accounts/list",
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let arr = body["data"]["items"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["email"], "a@x");
    assert_eq!(arr[0]["access_token"], "rca_toke...");
    assert_eq!(arr[0]["available"], true);

    // 删除 → 200 + REST envelope;再删 → 404
    let (status, deleted) = call(
        &app,
        Method::POST,
        "/v1/accounts/delete",
        Some(serde_json::json!({"email": "a@x"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted["code"], 200);
    assert!(deleted["data"].is_null());
    assert_eq!(deleted["message"], "success");
    let (status, missing) = call(
        &app,
        Method::POST,
        "/v1/accounts/delete",
        Some(serde_json::json!({"email": "a@x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["message"], "no account with email a@x");
    assert!(missing["data"].is_null());
    let (status, _) = call(&app, Method::DELETE, "/v1/accounts/delete", None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn account_routes_follow_api_contract_paths_and_methods() {
    let (app, store) = app();
    store
        .upsert(&serde_json::from_value(account_body("route@x", "rca_token123")).unwrap())
        .unwrap();

    let (status, detail) = call(
        &app,
        Method::POST,
        "/v1/accounts/detail",
        Some(serde_json::json!({"email": "route@x"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["data"]["email"], "route@x");

    for path in ["list", "detail", "create", "delete", "restore", "refresh"] {
        let (status, _) = call(&app, Method::GET, &format!("/v1/accounts/{path}"), None).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{path}");
    }
    let (status, _) = call(&app, Method::POST, "/v1/accounts", None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn list_uses_default_paging_and_snake_case_page_size() {
    let (app, store) = app();
    store
        .upsert(&serde_json::from_value(account_body("a@x", "rca_secret")).unwrap())
        .unwrap();
    store
        .upsert(&serde_json::from_value(account_body("b@x", "rca_secret2")).unwrap())
        .unwrap();

    let (status, all) = call(
        &app,
        Method::POST,
        "/v1/accounts/list",
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(all["data"]["page"], 1);
    assert_eq!(all["data"]["page_size"], 10);
    assert_eq!(all["data"]["items"].as_array().unwrap().len(), 2);
    let (status, default_size) = call(
        &app,
        Method::POST,
        "/v1/accounts/list",
        Some(serde_json::json!({"page": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(default_size["data"]["page_size"], 10);
    let (status, camel_case) = call(
        &app,
        Method::POST,
        "/v1/accounts/list",
        Some(serde_json::json!({"pageSize": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(camel_case["data"].is_null());

    let (status, page) = call(
        &app,
        Method::POST,
        "/v1/accounts/list",
        Some(serde_json::json!({"page": 2, "page_size": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["data"]["total"], 2);
    assert_eq!(page["data"]["page"], 2);
    assert_eq!(page["data"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["data"]["items"][0]["email"], "b@x");
    assert_eq!(page["data"]["items"][0]["access_token"], "rca_secr...");
}

#[tokio::test]
async fn export_can_be_posted_directly_to_upsert() {
    let (source_app, source_store) = app();
    source_store
        .upsert(&serde_json::from_value(account_body("a@x", "rca_secret")).unwrap())
        .unwrap();
    source_store
        .upsert(
            &serde_json::from_value(serde_json::json!({
                "tag": "second",
                "user": { "email": "b@x", "id": "user-2" },
                "access_token": "rca_second",
                "refresh_token": "refresh-2",
                "obtained_at": "2026-11-04T00:00:00Z"
            }))
            .unwrap(),
        )
        .unwrap();

    let (status, body) = call(&source_app, Method::GET, "/v1/accounts/export", None).await;
    assert_eq!(status, StatusCode::OK);
    let export = body["data"].clone();
    assert_eq!(export.as_array().unwrap().len(), 2);
    assert_eq!(export[0]["access_token"], "rca_secret");
    assert_eq!(export[1]["user"]["id"], "user-2");

    let (destination_app, destination_store) = app();
    let (status, response) = call(
        &destination_app,
        Method::POST,
        "/v1/accounts/create",
        Some(export),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        response["data"]["results"],
        serde_json::json!(["inserted", "inserted"])
    );
    let imported = match destination_store.list(None, None, None).unwrap() {
        raycast2api::account::store::AccountList::All(rows) => rows,
        raycast2api::account::store::AccountList::Page(_) => unreachable!(),
    };
    assert_eq!(imported.len(), 2);
    assert_eq!(imported[0].access_token, "rca_secret");
    assert_eq!(imported[1].refresh_token.as_deref(), Some("refresh-2"));
}

#[tokio::test]
async fn export_requires_configured_api_key() {
    let (app, _) = app_with_api_key(None);
    let (status, _) = call(&app, Method::GET, "/v1/accounts/export", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn detail_delete_restore_and_refresh_use_email_request_bodies() {
    let (app, _store) = app();
    let request = serde_json::json!({"email": "missing@x"});
    for path in ["detail", "delete", "restore", "refresh"] {
        let (status, body) = call(
            &app,
            Method::POST,
            &format!("/v1/accounts/{path}"),
            Some(request.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(body["data"].is_null(), "{path}");
        assert_eq!(body["message"], "no account with email missing@x", "{path}");
    }
}

#[tokio::test]
async fn missing_api_key_is_unauthorized() {
    let (app, _store) = app();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/accounts/list")
        .header("content-type", "application/json")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn anthropic_x_api_key_is_accepted() {
    let (app, _store) = app();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/accounts/list")
        .header("x-api-key", "test-key")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn wrong_x_api_key_is_unauthorized() {
    let (app, _store) = app();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/accounts/list")
        .header("x-api-key", "wrong-key")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
