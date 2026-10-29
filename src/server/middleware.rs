//! Cross-cutting HTTP middleware.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::debug::{self, REQUEST_ID};
use crate::error::GatewayError;
use crate::util;

use super::state::SharedState;

/// Largest inbound body we buffer for capture (2 MiB). A larger body is
/// rejected rather than partially logged; raise this if you test big payloads.
const MAX_CAPTURE_BYTES: usize = 2 * 1024 * 1024;

/// Enforce the gateway's own API key (from `API_KEY`) on all routes except the
/// public health probes. When no `API_KEY` is configured, all callers pass.
pub async fn require_api_key(
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Result<Response, GatewayError> {
    let path = request.uri().path();
    if path == "/health" || path == "/ready" {
        return Ok(next.run(request).await);
    }

    if let Some(expected) = state.settings.api_key.as_deref() {
        let provided = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));

        if provided != Some(expected) {
            return Err(GatewayError::Unauthorized(
                "Invalid API key provided.".into(),
            ));
        }
    }

    Ok(next.run(request).await)
}

/// Buffer the inbound request body so it can be recorded in the request-log
/// store, then run the rest of the stack inside a [`REQUEST_ID`] scope so the
/// upstream and response writes correlate with it.
///
/// Scope is inference-only (see the request-log spec): only the three
/// inference POST routes are captured. The body has to be buffered to be
/// observed, so the request is reassembled from the captured bytes before it
/// continues — the downstream `Json` extractor sees exactly what the client
/// sent. Log writes are fire-and-forget and never fail the request.
pub async fn debug_capture(
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Result<Response, GatewayError> {
    // Only inference requests are logged (see the request-log spec): probe
    // endpoints must not write rows, and read routes (models/accounts/logs)
    // would pollute the log and shift pagination under the reader.
    let path = request.uri().path();
    let is_inference = matches!(
        path,
        "/v1/chat/completions" | "/v1/messages" | "/v1/responses"
    );
    if !is_inference {
        return Ok(next.run(request).await);
    }

    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_CAPTURE_BYTES)
        .await
        .map_err(|_| GatewayError::BadRequest("request body too large or unreadable".into()))?;

    let id = util::new_ulid();
    state.log.insert_request(
        &id,
        parts.method.as_str(),
        parts.uri.path(),
        &debug::body_to_value(&bytes),
    );

    // Rebuild the request with the buffered body and run the handler within the
    // correlation-id scope; the upstream send happens inside this await.
    let request = Request::from_parts(parts, Body::from(bytes));
    let started = std::time::Instant::now();
    // handler 的 Err(GatewayError) 已由 axum 渲染为错误响应,这里只看状态码。
    let response = REQUEST_ID.scope(id.clone(), next.run(request)).await;

    // 请求/响应配对的完成日志:状态码 + 耗时,与 stream 层的 `[id]` 前缀
    // 和 client 层的账号日志对得上。
    let status = response.status();
    if status.is_success() {
        tracing::info!("← {status} [{id}] in {:?}", started.elapsed());
    } else {
        tracing::warn!("← {status} [{id}] in {:?}", started.elapsed());
    }
    Ok(response)
}
