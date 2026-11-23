use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use futures_util::StreamExt;
use std::net::SocketAddr;

use crate::log::context::{self as debug, REQUEST_ID};
use crate::statistics::{REQUEST_CONTEXT, RequestContext};
use crate::util;

use super::super::state::SharedState;
use super::auth::extract_api_key;

/// Buffer the inbound request body so it can be recorded in the request-log
/// store, then run the rest of the stack inside a [`REQUEST_ID`] scope so the
/// upstream and response writes correlate with it.
///
/// Scope is inference-only (see the request-log spec). Log writes run in
/// blocking sections; failures never fail the request.
pub async fn request_logging(
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Response {
    // Only inference requests are logged (see the request-log spec): probe
    // endpoints must not write rows, and read routes (models/accounts/logs)
    // would pollute the log and shift pagination under the reader.
    let path = request.uri().path();
    let is_inference = matches!(
        path,
        "/v1/chat/completions" | "/v1/messages" | "/v1/responses"
    );
    if !is_inference {
        return next.run(request).await;
    }

    let permit = match state.inference_slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            use axum::response::IntoResponse;
            return (axum::http::StatusCode::SERVICE_UNAVAILABLE, axum::Json(serde_json::json!({"error":{"message":"gateway concurrency limit reached","type":"overloaded_error"}}))).into_response();
        }
    };
    let context = request_context(&request);
    let response = REQUEST_CONTEXT
        .scope(context, capture(state, request, next))
        .await;
    let (parts, body) = response.into_parts();
    let stream = async_stream::stream! {
        let _permit = permit;
        let mut chunks = body.into_data_stream();
        while let Some(chunk) = chunks.next().await { yield chunk; }
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

fn request_context(request: &Request) -> RequestContext {
    let key = extract_api_key(request.headers());
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|peer| peer.0.ip());
    RequestContext {
        request_id: String::new(),
        requested_at: util::unix_secs() as i64,
        api_key: key.clone(),
        managed_key_id: request.extensions().get::<crate::apikey::KeyId>().cloned(),
        api_key_masked: key.as_deref().map(util::mask_api_key),
        request_ip: peer.map(|ip| ip.to_string()),
    }
}

async fn capture(state: SharedState, request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    let mut body = Box::pin(body.into_data_stream());
    let mut captured = Vec::new();

    while let Some(chunk) = body.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                let id = util::new_ulid();
                state.log.insert_inbound(
                    &id,
                    parts.method.as_str(),
                    parts.uri.path(),
                    Some(&parts.headers),
                    None,
                );
                let prefix = Bytes::from(captured);
                let replay = futures_util::stream::iter([Ok(prefix), Err(error)]).chain(body);
                let request = Request::from_parts(parts, Body::from_stream(replay));
                return run_with_request_id(id, request, next).await;
            }
        };
        captured.extend_from_slice(&chunk);
    }

    let id = util::new_ulid();
    state.log.insert_inbound(
        &id,
        parts.method.as_str(),
        parts.uri.path(),
        Some(&parts.headers),
        Some(&debug::body_to_value(&captured)),
    );

    let request = Request::from_parts(parts, Body::from(captured));
    run_with_request_id(id, request, next).await
}

async fn run_with_request_id(id: String, request: Request, next: Next) -> Response {
    let started = std::time::Instant::now();
    // handler 的 Err(GatewayError) 已由 axum 渲染为错误响应,这里只看状态码。
    let mut context = REQUEST_CONTEXT.with(Clone::clone);
    context.request_id = id.clone();
    let response = REQUEST_CONTEXT
        .scope(context, REQUEST_ID.scope(id.clone(), next.run(request)))
        .await;

    // 请求/响应配对的完成日志:状态码 + 耗时,与 stream 层的 `[id]` 前缀
    // 和 client 层的账号日志对得上。配置了代理时,上游客户端会登记实际
    // 走的通道("direct" 或代理 URL),这里带出来方便排查走线;未登记
    // (无代理配置)保持原格式。
    let status = response.status();
    let elapsed = started.elapsed();
    match debug::take_channel(&id) {
        Some(channel) if status.is_success() => {
            tracing::info!("← {status} [{id}] via {channel} in {elapsed:?}");
        }
        Some(channel) => {
            tracing::warn!("← {status} [{id}] via {channel} in {elapsed:?}");
        }
        None if status.is_success() => {
            tracing::info!("← {status} [{id}] in {elapsed:?}");
        }
        None => {
            tracing::warn!("← {status} [{id}] in {elapsed:?}");
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Bytes;
    use axum::extract::DefaultBodyLimit;
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::account::store::AccountStore;
    use crate::config::Settings;
    use crate::log::store::RequestLogStore;
    use crate::raycast::client::RaycastClient;
    use crate::server::state::AppState;
    use crate::storage::db::Db;

    use super::*;

    async fn echo_size(Json(body): Json<Value>) -> Json<Value> {
        Json(json!({"payload_len": body["payload"].as_str().unwrap().len()}))
    }

    #[test]
    fn bearer_fingerprint_and_forwarded_header_ignored() {
        let mut request = Request::builder()
            .header("authorization", "Bearer abcdefghijklmnop")
            .header("x-api-key", "ignored-key")
            .header("x-forwarded-for", "192.0.2.99, 198.51.100.7, 10.0.0.2")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo("10.0.0.1:1234".parse::<SocketAddr>().unwrap()));
        let direct = request_context(&request);
        assert_eq!(direct.request_ip.as_deref(), Some("10.0.0.1"));
        assert_eq!(direct.api_key_masked.as_deref(), Some("abcd****mnop"));
        assert_eq!(direct.api_key.as_deref(), Some("abcdefghijklmnop"));
        request
            .headers_mut()
            .insert("x-forwarded-for", "spoofed".parse().unwrap());
        request.headers_mut().remove("authorization");
        request
            .headers_mut()
            .insert("x-api-key", "tiny".parse().unwrap());
        let invalid = request_context(&request);
        assert_eq!(invalid.request_ip.as_deref(), Some("10.0.0.1"));
        assert_eq!(invalid.api_key_masked.as_deref(), Some("****"));
    }

    #[tokio::test]
    async fn admission_permit_lives_until_response_body_is_dropped() {
        let state = crate::server::state::compact_test_state();
        let app = Router::new()
            .route("/v1/responses", post(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                request_logging,
            ));
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .body(Body::empty())
                .unwrap()
        };
        let response = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(state.inference_slots.available_permits(), 63);
        drop(response);
        assert_eq!(state.inference_slots.available_permits(), 64);
        let permits = state
            .inference_slots
            .clone()
            .acquire_many_owned(64)
            .await
            .unwrap();
        assert_eq!(
            app.oneshot(request()).await.unwrap().status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        drop(permits);
    }

    #[tokio::test]
    async fn oversized_inference_body_is_fully_captured_and_reaches_handler() {
        let db = Db::open(":memory:").unwrap();
        let accounts = Arc::new(AccountStore::new(db.clone()));
        let statistics = Arc::new(crate::statistics::store::StatisticsStore::new(db.clone()));
        let log = Arc::new(RequestLogStore::new(db.clone()));
        let settings = Arc::new(Settings {
            raycast_signature_secret: "secret".into(),
            api_key: "test-key".into(),
            dashboard_password: None,
            dashboard_session_secret: None,
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bind_addr_dual: None,
            upstream_timeout_secs: 20,
            accounts_db: ":memory:".into(),
            proxies: Vec::new(),
            proxies_mode: crate::proxies::ProxyMode::Direct,
        });
        let client =
            RaycastClient::new(settings.clone(), statistics.clone());
        let state = Arc::new(
            AppState::new(crate::server::state::AppDeps {
                settings,
                client,
                accounts,
                log: log.clone(),
                statistics,
                db,
            })
            .unwrap(),
        );
        let app = Router::new()
            .route("/v1/chat/completions", post(echo_size))
            .layer(DefaultBodyLimit::disable())
            .layer(axum::middleware::from_fn_with_state(state, request_logging));
        let body_size = 3 * 1024 * 1024;
        let body = format!("{{\"payload\":\"{}\"}}", "x".repeat(body_size));
        let body = body.into_bytes();
        let split = body_size / 2;
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from_stream(futures_util::stream::iter([
                Ok::<_, axum::Error>(Bytes::from(body[..split].to_vec())),
                Ok(Bytes::from(body[split..].to_vec())),
            ])))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let response = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&response).unwrap()["payload_len"],
            body_size
        );

        let page = log.list((1, 1), None, None).unwrap();
        let detail = log.detail(&page.items[0].request_id).unwrap();
        // 存储走 to_string_pretty 重序列化,字节数与原始 body 不同;验证内容完整即可。
        let inbound: Value =
            serde_json::from_str(&detail.inbound_json.expect("large body should be logged"))
                .unwrap();
        assert_eq!(inbound["payload"].as_str().unwrap().len(), body_size);
    }
}
