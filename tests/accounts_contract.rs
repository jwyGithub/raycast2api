//! `/v1/accounts` CRUD 的契约测试(内存库;refresh 端点只验证状态码)。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use raycast2api::account::store::AccountStore;
use raycast2api::config::Settings;
use raycast2api::server::build_router;
use raycast2api::storage::db::Db;
use serde_json::Value;
use tower::ServiceExt;

fn app() -> (axum::Router, Arc<AccountStore>) {
    app_with_api_key("test-key")
}

fn app_with_api_key(api_key: &str) -> (axum::Router, Arc<AccountStore>) {
    let db = Db::open(":memory:").unwrap();
    let store = Arc::new(AccountStore::new(db.clone()).unwrap());
    let statistics = Arc::new(raycast2api::statistics::store::StatisticsStore::new(
        db.clone(),
    ));
    let settings = Arc::new(Settings {
        raycast_signature_secret: "secret".into(),
        api_key: api_key.into(),
        dashboard_password: None,
        dashboard_session_secret: None,
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        bind_addr_dual: None,
        upstream_timeout_secs: 20,
        accounts_db: ":memory:".into(),
        proxies: Vec::new(),
        proxies_mode: raycast2api::proxies::ProxyMode::Direct,
    });
    let log_store = Arc::new(raycast2api::log::store::RequestLogStore::new(db.clone()));
    let client =
        raycast2api::raycast::client::RaycastClient::new(settings.clone(), statistics.clone());
    let state = Arc::new(
        raycast2api::server::state::AppState::new(raycast2api::server::state::AppDeps {
            settings,
            client,
            accounts: store.clone(),
            log: log_store,
            statistics,
            db,
        })
        .unwrap(),
    );
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
        "device_id": "1111111111111111111111111111111111111111111111111111111111111111",
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
        "status must be one of all, available, cooldown, unavailable, disabled"
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
    assert_eq!(deleted["data"]["deleted"], 1);
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

    for path in [
        "list", "detail", "create", "update", "delete", "restore", "refresh",
    ] {
        let (status, _) = call(&app, Method::GET, &format!("/v1/accounts/{path}"), None).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{path}");
    }
    let (status, _) = call(&app, Method::POST, "/v1/accounts", None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn root_serves_account_console_without_api_authentication() {
    let (app, _) = app();
    let response = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("<title>Raycast 管理控制台</title>"));
    assert!(html.contains("<account-console"));
    assert!(html.contains("src=\"/views.js\""));
    assert!(html.contains("href=\"/style.css\""));

    // The entry wires shared requests to independently loaded panel modules.
    let response = app
        .oneshot(
            Request::builder()
                .uri("/views.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let js = String::from_utf8(body.to_vec()).unwrap();
    assert!(js.contains("async request(path, body, resource = 'accounts')"));
    assert!(js.contains("'/v1/' + resource + '/' + path"));
    for panel in ["accounts", "logs", "statistics"] {
        assert!(js.contains(&format!("import './views/{panel}.js'")));
    }
}

#[tokio::test]
async fn delete_supports_status_and_requires_email_status_match() {
    let (app, store) = app();
    for (email, token) in [
        ("available@x", "rca_available"),
        ("cooldown@x", "rca_cooldown"),
        ("unavailable@x", "rca_unavailable"),
    ] {
        store
            .upsert(&serde_json::from_value(account_body(email, token)).unwrap())
            .unwrap();
    }
    store.mark_cooldown(
        "cooldown@x",
        raycast2api::util::unix_secs() as i64 + 600,
        "test",
    );
    store.mark_unavailable("unavailable@x", "test");

    let (status, _) = call(
        &app,
        Method::POST,
        "/v1/accounts/delete",
        Some(serde_json::json!({"status": "cooldown"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(store.get("cooldown@x").unwrap().is_none());
    assert!(store.get("available@x").unwrap().is_some());
    assert!(store.get("unavailable@x").unwrap().is_some());

    let (status, mismatch) = call(
        &app,
        Method::POST,
        "/v1/accounts/delete",
        Some(serde_json::json!({"email": "unavailable@x", "status": "available"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        mismatch["message"],
        "account unavailable@x is unavailable (requested status: available)"
    );
    assert_eq!(mismatch["error_code"], "account_status_mismatch");
    assert!(store.get("unavailable@x").unwrap().is_some());

    let (status, _) = call(
        &app,
        Method::POST,
        "/v1/accounts/delete",
        Some(serde_json::json!({"email": "unavailable@x", "status": "unavailable"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(store.get("unavailable@x").unwrap().is_none());

    for body in [serde_json::json!({}), serde_json::json!({"status": "all"})] {
        let (status, _) = call(&app, Method::POST, "/v1/accounts/delete", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
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
                "device_id": "2222222222222222222222222222222222222222222222222222222222222222",
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
    let imported = destination_store.list_all().unwrap();
    assert_eq!(imported.len(), 2);
    assert_eq!(imported[0].access_token, "rca_secret");
    assert_eq!(imported[1].refresh_token.as_deref(), Some("refresh-2"));
}

#[tokio::test]
async fn export_requires_authentication() {
    let (app, _) = app();
    let request = Request::get("/v1/accounts/export")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(request).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
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

#[tokio::test]
async fn policy_changes_only_policy_fields_and_validates_input() {
    let (app, store) = app();
    store
        .upsert(&serde_json::from_value(account_body("p@x", "rca_policy")).unwrap())
        .unwrap();

    // 两个字段都缺省 → 400,且不触碰数据库。
    let (status, body) = call(
        &app,
        Method::POST,
        "/v1/accounts/update",
        Some(serde_json::json!({"email": "p@x"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["message"], "enabled or priority is required");

    // 未知字段(如试图改 token)被拒绝,token 保持不变。
    let (status, _) = call(
        &app,
        Method::POST,
        "/v1/accounts/update",
        Some(serde_json::json!({"email": "p@x", "priority": 1, "access_token": "rca_hijack"})),
    )
    .await;
    assert_ne!(status, StatusCode::OK);
    assert_eq!(
        store.get("p@x").unwrap().unwrap().access_token,
        "rca_policy"
    );

    // 不存在的邮箱 → 404。
    let (status, _) = call(
        &app,
        Method::POST,
        "/v1/accounts/update",
        Some(serde_json::json!({"email": "missing@x", "enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // 正常更新:返回新视图;null 等同于省略。
    let (status, body) = call(
        &app,
        Method::POST,
        "/v1/accounts/update",
        Some(serde_json::json!({"email": "p@x", "enabled": false, "priority": 4})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["enabled"], false);
    assert_eq!(body["data"]["priority"], 4);
    let (status, body) = call(
        &app,
        Method::POST,
        "/v1/accounts/update",
        Some(serde_json::json!({"email": "p@x", "enabled": null, "priority": 6})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["enabled"], false);
    assert_eq!(body["data"]["priority"], 6);
}

#[tokio::test]
async fn create_accepts_policy_fields_and_delete_404_names_actual_status() {
    let (app, store) = app();
    let mut record = account_body("c@x", "rca_create");
    record["enabled"] = serde_json::json!(false);
    record["priority"] = serde_json::json!(3);
    let (status, body) = call(&app, Method::POST, "/v1/accounts/create", Some(record)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["result"], "inserted");
    let row = store.get("c@x").unwrap().unwrap();
    assert!(!row.enabled);
    assert_eq!(row.priority, 3);

    // 同时停用又不可用:归为 disabled,按 unavailable 删除应 404 并说明实际状态。
    store.mark_unavailable("c@x", "test");
    let (status, body) = call(
        &app,
        Method::POST,
        "/v1/accounts/delete",
        Some(serde_json::json!({"email": "c@x", "status": "unavailable"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["message"],
        "account c@x is disabled (requested status: unavailable)"
    );
    assert!(store.get("c@x").unwrap().is_some());

    // 列表与删除使用同一归类。
    let (_, page) = call(
        &app,
        Method::POST,
        "/v1/accounts/list",
        Some(serde_json::json!({"status": "disabled"})),
    )
    .await;
    assert_eq!(page["data"]["total"], 1);
    let (status, _) = call(
        &app,
        Method::POST,
        "/v1/accounts/delete",
        Some(serde_json::json!({"status": "disabled"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(store.get("c@x").unwrap().is_none());
}

#[tokio::test]
async fn policy_rejects_out_of_range_priority_with_envelope() {
    let (app, store) = app();
    store
        .upsert(&serde_json::from_value(account_body("r@x", "rca_range")).unwrap())
        .unwrap();
    let (status, body) = call(
        &app,
        Method::POST,
        "/v1/accounts/update",
        Some(serde_json::json!({"email": "r@x", "priority": 3_000_000_000i64})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], 400);
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .starts_with("priority must be")
    );
    assert_eq!(store.get("r@x").unwrap().unwrap().priority, 0);
}

#[tokio::test]
async fn batch_create_is_atomic_when_any_entry_is_invalid() {
    let (app, store) = app();
    let mut bad = account_body("bad@x", "rca_bad");
    bad["priority"] = serde_json::json!(3_000_000_000i64);
    let (status, body) = call(
        &app,
        Method::POST,
        "/v1/accounts/create",
        Some(serde_json::json!([account_body("good@x", "rca_good"), bad])),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], 400);
    assert!(
        store.get("good@x").unwrap().is_none(),
        "valid sibling must roll back"
    );
    assert!(store.get("bad@x").unwrap().is_none());
}

#[tokio::test]
async fn schedulable_reflects_policy_not_raw_health() {
    let (app, store) = app();
    store
        .upsert(&serde_json::from_value(account_body("s@x", "rca_sched")).unwrap())
        .unwrap();
    let (_, body) = call(
        &app,
        Method::POST,
        "/v1/accounts/detail",
        Some(serde_json::json!({"email": "s@x"})),
    )
    .await;
    assert_eq!(body["data"]["available"], true);
    assert_eq!(body["data"]["schedulable"], true);

    let (_, body) = call(
        &app,
        Method::POST,
        "/v1/accounts/update",
        Some(serde_json::json!({"email": "s@x", "enabled": false})),
    )
    .await;
    assert_eq!(body["data"]["available"], true, "raw health unchanged");
    assert_eq!(
        body["data"]["schedulable"], false,
        "disabled is not schedulable"
    );
}

#[tokio::test]
async fn paused_accounts_reject_manual_refresh_and_disabled_restore() {
    let (app, store) = app();
    store
        .upsert(&serde_json::from_value(account_body("paused@x", "rca_paused")).unwrap())
        .unwrap();
    for code in ["account_disabled", "account_cooldown"] {
        if code == "account_disabled" {
            store.update_policy("paused@x", Some(false), None).unwrap();
        } else {
            store.update_policy("paused@x", Some(true), None).unwrap();
            store.mark_cooldown(
                "paused@x",
                raycast2api::util::unix_secs() as i64 + 600,
                "limit",
            );
        }
        let (status, body) = call(
            &app,
            Method::POST,
            "/v1/accounts/refresh",
            Some(serde_json::json!({"email": "paused@x"})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error_code"], code);
        if code == "account_disabled" {
            let (status, body) = call(
                &app,
                Method::POST,
                "/v1/accounts/restore",
                Some(serde_json::json!({"email": "paused@x"})),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(body["error_code"], code);
            assert!(!store.get("paused@x").unwrap().unwrap().enabled);
        }
    }
}

#[tokio::test]
async fn standby_accounts_reject_manual_refresh_and_restore() {
    let (app, store) = app();
    let mut high = account_body("high@x", "rca_high");
    high["priority"] = serde_json::json!(10);
    store
        .upsert(&serde_json::from_value(high).unwrap())
        .unwrap();
    store
        .upsert(&serde_json::from_value(account_body("standby@x", "rca_standby")).unwrap())
        .unwrap();
    for path in ["refresh", "restore"] {
        let (status, body) = call(
            &app,
            Method::POST,
            &format!("/v1/accounts/{path}"),
            Some(serde_json::json!({"email": "standby@x"})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error_code"], "account_standby");
    }
    assert!(
        store
            .get("standby@x")
            .unwrap()
            .unwrap()
            .info_fetched_at
            .is_none()
    );
}
