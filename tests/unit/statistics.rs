use super::*;
use crate::{
    core::{event::CanonicalEvent, protocol::Protocol},
    protocol::{InboundAdapter, messages::MessagesInbound, responses::ResponsesInbound},
    raycast::{builder::build_chat_request, stream::canonical_events_with_retry},
    server::{build_router, state::compact_test_state},
    storage::db::Db,
};
use axum::{
    body::{Body, to_bytes},
    extract::{ConnectInfo, Request},
    http::StatusCode,
};
use futures_util::StreamExt;
use serde_json::json;
use std::net::SocketAddr;
use tower::ServiceExt;
use wire::ListParams;

fn call(request_id: &str, kind: &'static str) -> CallContext {
    let mut request = CanonicalRequest::new(Protocol::Responses, "client-model");
    request.requested_effort = Some("max".into());
    let upstream = build_chat_request(&request, "openai", "upstream-model");
    let mut call = CallContext::new(&request, &upstream, kind);
    call.request.request_id = request_id.into();
    call.request.requested_at = 0;
    call
}

fn response(attempt: Attempt, payloads: &[Value]) -> reqwest::Response {
    let body: String = payloads
        .iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect();
    let mut response = reqwest::Response::from(
        axum::http::Response::builder()
            .status(200)
            .body(reqwest::Body::from(body))
            .unwrap(),
    );
    attempt.set_http_status(200);
    response.extensions_mut().insert(attempt);
    response
}

fn usage() -> Value {
    json!({"input_tokens":116861,"output_tokens":680,"cached_tokens":116291,
        "input_cost":0.0130526,"output_cost":0.0068,"input_credits":1.3053,
        "output_credits":0.68,"total_cost":0.0198526,"total_credits":1.9853,
        "future_field":{"preserved":true}})
}

#[test]
fn idempotent_completion_retains_costs_and_is_independent_of_log_cleanup() {
    use diesel::{RunQueryDsl, connection::SimpleConnection};
    let db = Db::open(":memory:").unwrap();
    let log = crate::log::store::RequestLogStore::new(db.clone());
    let store = Arc::new(StatisticsStore::new(db.clone()));
    let context = call("same-request", "compact");
    store
        .start("summary", &context, "summary@example.com")
        .unwrap();
    assert!(
        store
            .start("summary", &context, "other@example.com")
            .is_err()
    );
    store
        .finish("summary", "completed", Some(200), None, Some(&usage()))
        .unwrap();
    store
        .finish("summary", "failed", Some(500), Some("duplicate"), None)
        .unwrap();
    store
        .start(
            "inference",
            &call("same-request", "inference"),
            "main@example.com",
        )
        .unwrap();
    db.lock()
        .batch_execute(
            "INSERT INTO request_logs(request_id, created_at) VALUES ('same-request', 0)",
        )
        .unwrap();
    assert_eq!(log.cleanup_expired(), 1);
    let row = store.detail("summary").unwrap().unwrap();
    assert_eq!(row.status, "completed");
    assert_eq!(row.account_email, "summary@example.com");
    let json = serde_json::to_value(row).unwrap();
    assert_eq!(json["usage_json"], usage());
    assert_eq!(json["requested_at"], "1970-01-01 08:00:00");
    assert_eq!(store.list(&ListParams::default()).unwrap().total, 2);
    assert_eq!(store.interrupt_unfinished().unwrap(), 1);
    assert_eq!(store.interrupt_unfinished().unwrap(), 0);
    let unfinished = store.detail("inference").unwrap().unwrap();
    assert_eq!(unfinished.status, "interrupted");
    assert!(unfinished.usage_json.is_none());
    assert!(store.detail("missing").unwrap().is_none());
    diesel::sql_query("DROP TABLE statistics")
        .execute(&mut *db.lock())
        .unwrap();
    assert!(store.detail("summary").is_err());
}

#[test]
fn original_effort_and_thinking_survive_protocol_normalization() {
    let request = MessagesInbound::parse(
        serde_json::from_value(json!({
            "model":"anthropic-claude-test-think","messages":[{"role":"user","content":"hi"}],
            "output_config":{"effort":"max"},"thinking":{"type":"enabled","budget_tokens":32000}
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(request.requested_effort.as_deref(), Some("max"));
    assert_eq!(request.reasoning_effort.unwrap().as_str(), "xhigh");
    assert_eq!(request.requested_thinking.unwrap()["budget_tokens"], 32000);
    let request = ResponsesInbound::parse(
        serde_json::from_value(json!({
            "model":"anthropic-claude-test-think","input":"hi","reasoning":{"effort":"max"}
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(request.requested_effort.as_deref(), Some("max"));
}

#[tokio::test]
async fn costs_settle_before_terminal_delta_and_survive_response_peeking() {
    let state = compact_test_state();
    let context = call("finish", "inference");
    let attempt = Attempt::start(state.statistics.clone(), &context, "a@example.com").unwrap();
    let response = response(
        attempt,
        &[json!({"text":"done","finish_reason":"stop","usage":usage()})],
    );
    let (error, response) =
        crate::raycast::peek::peek_first_payload(response, std::time::Duration::from_secs(1)).await;
    assert!(error.is_none());
    assert_eq!(
        state.statistics.list(&ListParams::default()).unwrap().items[0].status,
        "completed"
    );
    let upstream = build_chat_request(
        &CanonicalRequest::new(Protocol::Responses, "m"),
        "openai",
        "m",
    );
    let events = canonical_events_with_retry(
        response,
        state.client.clone(),
        upstream,
        state.log.clone(),
        context,
    );
    let mut events = Box::pin(events);
    assert_eq!(
        events.next().await,
        Some(CanonicalEvent::TextDelta("done".into()))
    );
    let row = state
        .statistics
        .list(&ListParams::default())
        .unwrap()
        .items
        .remove(0);
    assert_eq!(row.status, "completed");
    assert_eq!(
        serde_json::from_str::<Value>(&row.usage_json.unwrap()).unwrap(),
        usage()
    );
    drop(events);
    assert_eq!(
        state.statistics.list(&ListParams::default()).unwrap().items[0].status,
        "completed"
    );
}

#[tokio::test]
async fn cancellation_records_unknown_usage_before_and_during_stream_polling() {
    for poll in [false, true] {
        let state = compact_test_state();
        let context = call("cancel", "inference");
        let attempt = Attempt::start(state.statistics.clone(), &context, "a@example.com").unwrap();
        let upstream = build_chat_request(
            &CanonicalRequest::new(Protocol::Responses, "m"),
            "openai",
            "m",
        );
        let mut events = Box::pin(canonical_events_with_retry(
            response(attempt, &[json!({"text":"partial"})]),
            state.client.clone(),
            upstream,
            state.log.clone(),
            context,
        ));
        if poll {
            assert!(matches!(
                events.next().await,
                Some(CanonicalEvent::TextDelta(_))
            ));
        }
        drop(events);
        let row = state
            .statistics
            .list(&ListParams::default())
            .unwrap()
            .items
            .remove(0);
        assert_eq!(row.status, "cancelled");
        assert!(row.usage_json.is_none());
        assert!(row.finished_at.is_some());
    }
}

#[tokio::test]
async fn successful_finish_without_usage_remains_null_and_partial_usage_survives_cancellation() {
    for terminal in [false, true] {
        let state = compact_test_state();
        let context = call("unknown", "inference");
        let attempt = Attempt::start(state.statistics.clone(), &context, "a@example.com").unwrap();
        let payload = if terminal {
            json!({"finish_reason":"stop"})
        } else {
            json!({"text":"partial","usage":usage()})
        };
        let upstream = build_chat_request(
            &CanonicalRequest::new(Protocol::Responses, "m"),
            "openai",
            "m",
        );
        let mut events = Box::pin(canonical_events_with_retry(
            response(attempt, &[payload]),
            state.client.clone(),
            upstream,
            state.log.clone(),
            context,
        ));
        assert!(events.next().await.is_some());
        drop(events);
        let row = state
            .statistics
            .list(&ListParams::default())
            .unwrap()
            .items
            .remove(0);
        assert_eq!(row.status, if terminal { "completed" } else { "cancelled" });
        if terminal {
            assert!(row.usage_json.is_none());
        } else {
            assert_eq!(
                serde_json::from_str::<Value>(&row.usage_json.unwrap()).unwrap(),
                usage()
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_attempts_keep_account_and_usage_together() {
    let state = compact_test_state();
    let mut tasks = Vec::new();
    for n in 0..12 {
        let store = state.statistics.clone();
        tasks.push(tokio::spawn(async move {
            let context = call(&format!("request-{n}"), "inference");
            let attempt =
                Attempt::start(store, &context, &format!("account-{n}@example.com")).unwrap();
            tokio::task::yield_now().await;
            attempt.observe_payload(&json!({"usage":{"total_credits":n}}).to_string());
            attempt.finish("completed", None);
            attempt.finish("failed", Some("duplicate"));
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let page = state
        .statistics
        .list(&ListParams {
            page_size: Some(100),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.total, 12);
    for row in page.items {
        let n: i64 = row
            .request_id
            .strip_prefix("request-")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(row.account_email, format!("account-{n}@example.com"));
        assert_eq!(
            serde_json::from_str::<Value>(&row.usage_json.unwrap()).unwrap()["total_credits"],
            n
        );
        assert_eq!(row.status, "completed");
    }
}

async fn query(
    app: &axum::Router,
    path: &str,
    body: Value,
    authenticated: bool,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if authenticated {
        builder = builder.header("authorization", "Bearer test-key");
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn upstream_fixture() -> (
    crate::server::state::SharedState,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/",
                axum::routing::post(|| async {
                    format!(
                        "data: {}\n\n",
                        json!({"text":"summary or answer", "finish_reason":"stop","usage":usage()})
                    )
                }),
            ),
        )
        .await
        .unwrap();
    });
    let mut state = compact_test_state();
    let inner = Arc::get_mut(&mut state).unwrap();
    inner.client = crate::raycast::client::RaycastClient::new(
        reqwest::Client::builder().no_proxy().build().unwrap(),
        inner.settings.clone(),
        inner.accounts.clone(),
        inner.log.clone(),
        inner.statistics.clone(),
    );
    inner.client.chat_url = Some(format!("http://{address}/"));
    inner
        .accounts
        .upsert(
            &serde_json::from_value(
                json!({"user":{"email":"actual@example.com"},"access_token":"fixture-token"}),
            )
            .unwrap(),
        )
        .unwrap();
    (state, server)
}

#[tokio::test]
async fn three_protocols_capture_metadata_for_streaming_and_non_streaming_requests() {
    for streaming in [false, true] {
        for (path, mut body) in [
            (
                "/v1/chat/completions",
                json!({"messages":[{"role":"user","content":"hello"}],"reasoning_effort":"xhigh"}),
            ),
            (
                "/v1/messages",
                json!({"messages":[{"role":"user","content":"hello"}],"output_config":{"effort":"max"},"thinking":{"type":"enabled","budget_tokens":32000}}),
            ),
            (
                "/v1/responses",
                json!({"input":"hello","reasoning":{"effort":"max"}}),
            ),
        ] {
            let (state, server) = upstream_fixture().await;
            body["stream"] = json!(streaming);
            body["model"] = json!("test-model");
            let mut request = Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .header("x-api-key", "caller-key-123456")
                .header("x-forwarded-for", "192.0.2.1")
                .body(Body::from(body.to_string()))
                .unwrap();
            request.extensions_mut().insert(ConnectInfo(
                "198.51.100.8:5678".parse::<SocketAddr>().unwrap(),
            ));
            let response = build_router(state.clone()).oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{path} streaming={streaming}"
            );
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            assert!(String::from_utf8_lossy(&bytes).contains("summary or answer"));
            let page = state.statistics.list(&ListParams::default()).unwrap();
            assert_eq!(page.total, 1, "{path} streaming={streaming}");
            let row = &page.items[0];
            assert_eq!(row.status, "completed");
            assert_eq!(row.request_ip.as_deref(), Some("198.51.100.8"));
            assert_eq!(row.account_email, "actual@example.com");
            assert_eq!(row.requested_model, "test-model");
            assert_eq!(row.upstream_model, "test");
            assert_eq!(
                row.requested_effort.as_deref(),
                Some(if path == "/v1/chat/completions" {
                    "xhigh"
                } else {
                    "max"
                })
            );
            assert_eq!(row.api_key.as_deref(), Some("caller-key-123456"));
            assert_ne!(row.request_id, "-");
            assert!(state.log.detail(&row.request_id).is_some());
            assert_eq!(
                serde_json::from_str::<Value>(row.usage_json.as_ref().unwrap()).unwrap(),
                usage()
            );
            server.abort();
        }
    }
}

#[tokio::test]
async fn responses_summary_and_inference_share_request_but_keep_separate_usage() {
    for streaming in [false, true] {
        let (state, server) = upstream_fixture().await;
        let body = json!({"model":"test-model","stream":streaming,"reasoning":{"effort":"max"},"input":[
            {"role":"assistant","content":"x".repeat(90_000)},
            {"role":"user","content":"current task"}
        ]});
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .header("x-api-key", "caller-key-123456")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = build_router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let page = state.statistics.list(&ListParams::default()).unwrap();
        assert_eq!(page.total, 2, "streaming={streaming}");
        assert_eq!(
            page.items
                .iter()
                .filter(|row| row.call_type == "compact")
                .count(),
            1
        );
        assert_eq!(
            page.items
                .iter()
                .filter(|row| row.call_type == "inference")
                .count(),
            1
        );
        assert_eq!(page.items[0].request_id, page.items[1].request_id);
        assert_eq!(page.items[0].api_key, page.items[1].api_key);
        assert!(page.items[0].api_key.is_some());
        assert_eq!(page.items[0].requested_at, page.items[1].requested_at);
        for row in page.items {
            assert_eq!(row.status, "completed");
            assert_eq!(row.requested_effort.as_deref(), Some("max"));
            assert_eq!(
                serde_json::from_str::<Value>(row.usage_json.as_ref().unwrap()).unwrap(),
                usage()
            );
        }
        server.abort();
    }
}

#[tokio::test]
async fn authenticated_endpoints_paginate_filter_validate_and_return_details() {
    let mut state = compact_test_state();
    Arc::make_mut(&mut Arc::get_mut(&mut state).unwrap().settings).api_key =
        Some("test-key".into());
    let mut context = call("request", "inference");
    context.request.api_key = Some("fingerprint".into());
    state
        .statistics
        .start("first", &context, "a@example.com")
        .unwrap();
    state
        .statistics
        .finish("first", "completed", Some(200), None, Some(&usage()))
        .unwrap();
    state
        .statistics
        .start("second", &call("other", "compact"), "b@example.com")
        .unwrap();
    let app = build_router(state.clone());
    let (status, page) = query(&app, "/v1/statistics/list", json!({}), true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["code"], 200);
    assert_eq!(page["data"]["total"], 2);
    assert_eq!(page["data"]["page_size"], 10);
    for filter in [
        json!({"request_id":"request"}),
        json!({"account_email":"a@example.com"}),
        json!({"api_key":"fingerprint"}),
        json!({"call_type":"inference"}),
        json!({"status":"completed"}),
    ] {
        let (status, page) = query(&app, "/v1/statistics/list", filter, true).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page["data"]["total"], 1);
        assert_eq!(page["data"]["items"][0]["id"], "first");
    }
    let (_, page) = query(&app, "/v1/statistics/list", json!({"requested_at_from":0,"requested_at_to":0,"requested_model":"client-model","upstream_model":"upstream-model","page":2,"page_size":1}), true).await;
    assert_eq!(page["data"]["total"], 2);
    assert_eq!(page["data"]["items"].as_array().unwrap().len(), 1);
    for filter in [
        json!({"requested_model":"missing"}),
        json!({"upstream_model":"missing"}),
        json!({"requested_at_from":1}),
        json!({"page":3,"page_size":1}),
    ] {
        let (_, page) = query(&app, "/v1/statistics/list", filter, true).await;
        assert!(page["data"]["items"].as_array().unwrap().is_empty());
    }
    let (_, detail) = query(&app, "/v1/statistics/detail", json!({"id":"first"}), true).await;
    assert_eq!(detail["data"]["usage_json"], usage());
    assert_eq!(detail["data"]["requested_at"], "1970-01-01 08:00:00");
    assert_eq!(
        query(&app, "/v1/statistics/detail", json!({"id":"missing"}), true)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    for path in ["/v1/statistics/list", "/v1/statistics/detail"] {
        assert_eq!(
            query(&app, path, json!({}), false).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    for filter in [
        json!({"page":0}),
        json!({"page_size":101}),
        json!({"page_size":0}),
        json!({"requested_at_from":2,"requested_at_to":1}),
        json!({"requested_at_from":-1}),
    ] {
        assert_eq!(
            query(&app, "/v1/statistics/list", filter, true).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        query(&app, "/v1/statistics/list", json!({"pageSize":1}), true)
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(state.log.count().unwrap(), 0);
}
