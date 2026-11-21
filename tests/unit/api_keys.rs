use super::*;
use crate::apikey::{ApiKeyStore, wire::Create};
use axum::http::Method;
use diesel::connection::SimpleConnection;

const KEY: &str = "sk-managed-test-123456";
const GLOBAL: &str = "test-key";

async fn request(
    app: &axum::Router,
    method: Method,
    path: &str,
    key: Option<&str>,
    body: Value,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(key) = key {
        request = request.header("x-api-key", key);
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

async fn api(app: &axum::Router, action: &str, body: Value, status: StatusCode) -> Value {
    let response = request(
        app,
        Method::POST,
        &format!("/v1/api-keys/{action}"),
        Some(GLOBAL),
        body,
    )
    .await;
    assert_eq!(response.headers()["cache-control"], "no-store");
    let actual = response.status();
    let body = json_body(response).await;
    assert_eq!(actual, status, "{action}: {body}");
    body["data"].clone()
}

async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}

#[tokio::test]
async fn management_crud_model_catalog_and_exact_route_authorization() {
    let state = compact_test_state();
    state.aliases.create("my-alias", "test-model").unwrap();
    let app = build_router(state.clone());
    for key in [
        "too-short",
        "包含非 ASCII 字符的任意长度无效测试字符串",
        "contains embedded whitespace",
        &"x".repeat(129),
    ] {
        api(
            &app,
            "create",
            json!({"mode":"custom", "api_key":key}),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    let random = api(&app, "create", json!({"mode":"random"}), StatusCode::OK).await;
    let key = random["api_key"].as_str().unwrap();
    assert_eq!(key.len(), 67);
    assert!(key.starts_with("sk-") && key[3..].bytes().all(|b| b.is_ascii_hexdigit()));
    let row = api(
        &app,
        "create",
        json!({"mode":"custom","api_key":KEY}),
        StatusCode::OK,
    )
    .await;
    let id = row["id"].clone();
    assert_eq!(row["quota_kind"], "unlimited");
    assert_eq!(row["concurrency_limit"], 1);
    assert!(row["model_count"].is_null() && row["ends_at"].is_null());
    api(
        &app,
        "create",
        json!({"mode":"custom","api_key":KEY}),
        StatusCode::CONFLICT,
    )
    .await;
    let conflict = ApiKeyStore::new(state.db.clone(), KEY).err().unwrap();
    assert!(conflict.contains(id.as_str().unwrap()) && !conflict.contains(KEY));

    for (method, path) in [
        (Method::HEAD, "/v1/models"),
        (Method::POST, "/v1/models"),
        (Method::POST, "/v1/models/list"),
        (Method::POST, "/v1/models/alias/list"),
        (Method::GET, "/v1/accounts/export"),
        (Method::POST, "/v1/api-keys/list"),
        (Method::POST, "/v1/api-keys/unknown"),
        (Method::GET, "/v1/chat/completions"),
    ] {
        let response = request(&app, method, path, Some(KEY), json!({})).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
    assert_eq!(
        request(&app, Method::HEAD, "/v1/models", Some(GLOBAL), json!(null))
            .await
            .status(),
        StatusCode::OK
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", "Bearer invalid")
                .header("x-api-key", KEY)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        json_body(response).await["error"]["code"],
        "invalid_api_key"
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", format!("Bearer {KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(response).await["data"][0]["id"], "test-model");
    api(
        &app,
        "update-models",
        json!({"id":id}),
        StatusCode::BAD_REQUEST,
    )
    .await;
    api(
        &app,
        "update-models",
        json!({"id":id,"model_ids":[]}),
        StatusCode::BAD_REQUEST,
    )
    .await;
    api(
        &app,
        "update-models",
        json!({"id":id,"model_ids":["my-alias"]}),
        StatusCode::BAD_REQUEST,
    )
    .await;
    let row = api(
        &app,
        "update-models",
        json!({"id":id,"model_ids":["test-model","test-model"]}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(row["model_count"], 1);
    let options = api(&app, "model-options", json!({"id":id}), StatusCode::OK).await;
    assert_eq!(options["items"][0]["aliases"][0]["alias"], "my-alias");
    assert_eq!(options["items"][0]["selected"], true);
    // 目录里没声明订阅等级的模型按 Pro 处理。
    assert_eq!(options["items"][0]["in_better_ai_subscription"], false);
    // Simulate a catalog refresh after this model was configured.
    state
        .api_keys
        .update_models(
            id.as_str().unwrap(),
            Some(vec!["old-model".into()]),
            &["old-model".into()].into_iter().collect(),
        )
        .unwrap();
    let options = api(&app, "model-options", json!({"id":id}), StatusCode::OK).await;
    assert_eq!(options["items"][1]["offline"], true);
    assert!(options["items"][1]["in_better_ai_subscription"].is_null());
    let response = request(&app, Method::GET, "/v1/models", Some(KEY), json!(null)).await;
    assert_eq!(json_body(response).await["data"], json!([]));
    let response = request(
        &app,
        Method::POST,
        "/v1/messages",
        Some(KEY),
        json!({"model":"my-alias","messages":[{"role":"user","content":"hello"}],"max_tokens":1}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "model_not_found"
    );
    api(
        &app,
        "update-models",
        json!({"id":id,"model_ids":["old-model","test-model"]}),
        StatusCode::OK,
    )
    .await;
    api(
        &app,
        "update-models",
        json!({"id":id,"model_ids":null}),
        StatusCode::OK,
    )
    .await;

    let now = unix_secs() as i64;
    api(
        &app,
        "update",
        json!({"id":id,"enabled":false,"starts_at":now+30,"ends_at":now+60}),
        StatusCode::OK,
    )
    .await;
    for (change, code) in [
        (json!({"id":id}), "api_key_disabled"),
        (json!({"id":id,"enabled":true}), "api_key_not_yet_valid"),
        (
            json!({"id":id,"starts_at":now-1,"ends_at":now}),
            "api_key_expired",
        ),
    ] {
        api(&app, "update", change, StatusCode::OK).await;
        let response = request(&app, Method::GET, "/v1/models", Some(KEY), json!(null)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(json_body(response).await["error"]["code"], code);
    }
    let filtered = api(
        &app,
        "list",
        json!({"api_key":KEY,"status":"expired","validity":"period"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(filtered["total"], 1);
    let row = api(
        &app,
        "update",
        json!({"id":id,"starts_at":null,"ends_at":null}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(row["status"], "active");
    assert!(row["starts_at"].is_null() && row["ends_at"].is_null());
    api(&app, "delete", json!({"id":id}), StatusCode::OK).await;
    let row = api(
        &app,
        "create",
        json!({"mode":"custom","api_key":KEY}),
        StatusCode::OK,
    )
    .await;
    assert_ne!(row["id"], id);
}

#[tokio::test]
async fn responses_holds_one_slot_through_compaction_and_settles_only_formal_usage() {
    let (state, server) = upstream_fixture().await;
    state.aliases.create("my-alias", "test-model").unwrap();
    let app = build_router(state.clone());
    let row = api(
        &app,
        "create",
        json!({"mode":"custom","api_key":KEY,"quota_kind":"cost","quota_limit":"0.0198526"}),
        StatusCode::OK,
    )
    .await;
    let id = row["id"].clone();
    let large = json!({"model":"my-alias","stream":true,"input":[{"role":"assistant","content":"x".repeat(90_000)},{"role":"user","content":"next"}]});
    let small = json!({"model":"my-alias","input":"hello","stream":true});
    let send = |payload| request(&app, Method::POST, "/v1/responses", Some(KEY), payload);
    api(
        &app,
        "update",
        json!({"id":id,"concurrency_limit":2}),
        StatusCode::OK,
    )
    .await;
    let other = send(small.clone()).await;
    assert_eq!(other.status(), StatusCode::OK);
    let discarded = send(small.clone()).await;
    assert_eq!(discarded.status(), StatusCode::OK);
    api(
        &app,
        "update",
        json!({"id":id,"concurrency_limit":1}),
        StatusCode::OK,
    )
    .await;
    drop(other);
    let limited = send(small.clone()).await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.headers()["retry-after"], "1");
    let body = json_body(limited).await;
    assert_eq!(body["error"]["code"], "api_key_concurrency_limit");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("1 in-flight")
    );
    let denied = send(json!({"model":"missing","input":"hi","stream":true})).await;
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        request(&app, Method::GET, "/v1/models", Some(KEY), json!(null))
            .await
            .status(),
        StatusCode::OK
    );
    drop(discarded);
    assert_eq!(
        state.statistics.list(&ListParams::default()).unwrap().total,
        0
    );
    let first = send(large).await;
    assert_eq!(first.status(), StatusCode::OK);
    // The stream must use the catalog entry authorized before the response was opened.
    state.aliases.delete("my-alias").unwrap();
    state
        .aliases
        .create("my-alias", "invalid-new-target")
        .unwrap();
    let body = to_bytes(first.into_body(), 1024 * 1024).await.unwrap();
    let frames = String::from_utf8_lossy(&body);
    assert!(frames.contains("上下文压缩完成") && frames.contains("response.completed"));
    assert!(frames.contains("my-alias"));
    let rows = state.statistics.list(&ListParams::default()).unwrap();
    assert_eq!(rows.total, 2);
    assert!(
        rows.items
            .iter()
            .all(|row| row.status == "completed" && row.upstream_model == "test")
    );
    assert_eq!(
        rows.items
            .iter()
            .filter(|row| row.call_type == "compact")
            .count(),
        1
    );
    let exhausted = send(small.clone()).await;
    assert_eq!(exhausted.status(), StatusCode::FORBIDDEN);
    assert!(!exhausted.headers().contains_key("retry-after"));
    assert_eq!(
        json_body(exhausted).await["error"]["code"],
        "insufficient_quota"
    );
    let row = api(
        &app,
        "update",
        json!({"id":id,"quota_kind":"credits","quota_limit":"2"}),
        StatusCode::OK,
    )
    .await;
    assert_eq!(row["used_cost"], "0.0198526");
    assert_eq!(row["used_credits"], "1.9853");
    assert_eq!(row["status"], "active");
    let row = api(&app, "reset-usage", json!({"id":id}), StatusCode::OK).await;
    assert_eq!(row["used_credits"], "0");
    assert_eq!(row["used_cost"], "0");
    assert_eq!(
        state.statistics.list(&ListParams::default()).unwrap().total,
        2
    );
    let compact_only = send(json!({
        "model":"test-model",
        "input":[{"role":"user","content":"hello"},{"type":"compaction_trigger"}]
    }))
    .await;
    assert_eq!(compact_only.status(), StatusCode::OK);
    let body = json_body(compact_only).await;
    assert_eq!(body["status"], "completed");
    assert_eq!(body["output"][0]["type"], "compaction");
    let row = api(&app, "list", json!({}), StatusCode::OK).await;
    assert_eq!(row["items"][0]["used_credits"], "0");
    assert_eq!(row["items"][0]["used_cost"], "0");
    server.abort();
}

#[tokio::test]
async fn insert_failure_refuses_limited_keys_before_sse_across_protocols() {
    let (state, server) = upstream_fixture().await;
    let app = build_router(state.clone());
    let row = api(
        &app,
        "create",
        json!({"mode":"custom","api_key":KEY,"quota_kind":"credits","quota_limit":"1"}),
        StatusCode::OK,
    )
    .await;
    state.db.lock().batch_execute("CREATE TRIGGER fail_start BEFORE INSERT ON statistics BEGIN SELECT RAISE(ABORT, 'test insert failure'); END;").unwrap();
    for (path, input) in [
        ("/v1/responses", json!({"input":"hello"})),
        (
            "/v1/chat/completions",
            json!({"messages":[{"role":"user","content":"hello"}]}),
        ),
        (
            "/v1/messages",
            json!({"messages":[{"role":"user","content":"hello"}],"max_tokens":10}),
        ),
    ] {
        let mut body = input;
        body["model"] = json!("test-model");
        body["stream"] = json!(true);
        let response = request(&app, Method::POST, path, Some(KEY), body).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["x-should-retry"], "false");
        assert_eq!(
            json_body(response).await["error"]["code"],
            "usage_tracking_unavailable"
        );
    }
    api(
        &app,
        "update",
        json!({"id":row["id"],"quota_kind":"unlimited"}),
        StatusCode::OK,
    )
    .await;
    for key in [KEY, GLOBAL] {
        let response = request(
            &app,
            Method::POST,
            "/v1/responses",
            Some(key),
            json!({"model":"test-model","input":"hello","stream":true}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            String::from_utf8_lossy(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .contains("response.completed")
        );
    }
    server.abort();
}

#[test]
fn settlement_retries_original_snapshot_and_cannot_charge_recreated_key() {
    let state = compact_test_state();
    let create = || {
        state
            .api_keys
            .create(
                serde_json::from_value::<Create>(json!({"mode":"custom","api_key":KEY})).unwrap(),
                GLOBAL,
            )
            .unwrap()
    };
    let old = create();
    let mut context = call("test", "inference");
    context.accounting = Some(Accounting {
        store: state.api_keys.clone(),
        id: state.api_keys.verify(KEY).unwrap(),
        required: false,
    });
    let start = || Attempt::start(state.statistics.clone(), &context, "a@example.com").unwrap();
    let sample = json!({"usage":{"total_credits":1.25,"total_cost":0.0091105}}).to_string();
    let attempt = start();
    let id = attempt.0.id.clone();
    attempt.observe_payload(&sample);
    state.db.lock().batch_execute("CREATE TRIGGER fail_settle BEFORE UPDATE OF used_cost ON api_keys BEGIN SELECT RAISE(ABORT, 'test rollback'); END;").unwrap();
    attempt.finish("completed", None);
    assert_eq!(
        state.statistics.detail(&id).unwrap().unwrap().status,
        "in_progress"
    );
    state
        .db
        .lock()
        .batch_execute("DROP TRIGGER fail_settle;")
        .unwrap();
    attempt.observe_payload(r#"{"usage":{"total_credits":99,"total_cost":99}}"#);
    drop(attempt);
    assert_eq!(
        state.statistics.detail(&id).unwrap().unwrap().status,
        "completed"
    );
    let list = || {
        state
            .api_keys
            .list(serde_json::from_value(json!({"api_key":KEY})).unwrap())
            .unwrap()
            .items
            .remove(0)
    };
    assert_eq!(list().used_cost, "0.0091105");
    assert_eq!(list().used_credits, "1.25");
    let reloaded = ApiKeyStore::new(state.db.clone(), GLOBAL).unwrap();
    assert_eq!(
        reloaded
            .list(serde_json::from_value(json!({})).unwrap())
            .unwrap()
            .items[0]
            .used_cost,
        "0.0091105"
    );

    let attempt = start();
    let id = attempt.0.id.clone();
    state.api_keys.delete(&old.id).unwrap();
    let recreated = create();
    assert_ne!(recreated.id, old.id);
    attempt.observe_payload(&sample);
    attempt.finish("completed", None);
    attempt.finish("completed", None);
    assert_eq!(
        state.statistics.detail(&id).unwrap().unwrap().status,
        "completed"
    );
    assert_eq!(list().used_cost, "0");
}
