//! The Anthropic error surface: `{ "type": "error", "error": { "type", "message" } }`.
//!
//! Unlike the OpenAI families (whose shape matches [`GatewayError`]'s native
//! rendering), Anthropic wraps errors in its own envelope with a distinct
//! `type` taxonomy, so this module translates. The HTTP status and message come
//! from [`GatewayError::status_and_message`]; only the `type` label is
//! Anthropic-specific and is derived from the status.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::error::GatewayError;

/// Render a gateway error in Anthropic's error envelope.
pub fn render(error: GatewayError) -> Response {
    let (status, message) = error.status_and_message();
    let body = json!({
        "type": "error",
        "error": { "type": error_type(status), "message": message }
    });
    (status, Json(body)).into_response()
}

/// Anthropic's error `type` taxonomy, derived from the HTTP status.
fn error_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}
