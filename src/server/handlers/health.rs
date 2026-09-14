//! Health and readiness probes (`GET /health`, `GET /ready`). Both are public
//! (exempt from API-key auth).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::server::state::SharedState;

/// Liveness: the required credentials were present at startup, so if we're
/// serving, config is OK.
pub async fn health() -> Response {
    Json(json!({ "status": "ok" })).into_response()
}

/// Readiness: report `ready` when the backend is believed to be working;
/// otherwise probe upstream at most once per rate-limit window.
pub async fn ready(State(state): State<SharedState>) -> Response {
    if state.health.is_working() {
        return Json(json!({ "status": "ready" })).into_response();
    }

    if !state.health.should_recheck() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "not ready", "message": "Backend not working, rate limited" })),
        )
            .into_response();
    }

    state.health.record_check();
    match state.client.fetch_models().await {
        Ok(models) if !models.is_empty() => {
            state.health.mark_working();
            Json(json!({ "status": "ready" })).into_response()
        }
        _ => {
            state.health.mark_failed();
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "not ready", "message": "No models available" })),
            )
                .into_response()
        }
    }
}
