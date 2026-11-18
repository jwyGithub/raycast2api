//! Unified gateway error type, rendered as an OpenAI-style error response:
//! `{ "error": { "message", "type", "code" } }`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Everything that can go wrong while handling a gateway request.
#[derive(Debug)]
pub enum GatewayError {
    /// The client sent an invalid request (400).
    BadRequest(String),
    /// Missing or invalid gateway API key (401).
    Unauthorized(String),
    /// Unknown route (404).
    NotFound(String),
    /// Server misconfiguration, e.g. missing credentials (500).
    Config(String),
    /// An upstream error; account authentication failures remain gateway errors.
    Upstream { status: u16, message: String },
    /// The account's access_token was rejected (upstream 401). Callers use
    /// this as a semantic signal to rotate accounts / attempt a refresh,
    /// instead of destructuring `Upstream` and comparing the status field.
    TokenInvalid {
        /// Why the token is considered dead; goes into logs and the response.
        message: String,
    },
    /// No usable Raycast account in the store (503).
    NoAccount(String),
    /// Every account is rate-limited (429). Carries the cooldown ETA so the
    /// response can set `Retry-After`, which the Claude Code harness honors
    /// exactly (pausing until reset and showing "rate limited until …")
    /// instead of retrying a 503 on its own exponential backoff.
    RateLimited {
        /// Seconds until the earliest account recovers.
        retry_after_secs: i64,
        message: String,
    },
    /// Any other internal failure (500).
    Internal(String),
    /// A required service, such as the dashboard session store, is unavailable (503).
    Unavailable(String),
}

impl GatewayError {
    /// Map the error to an HTTP status, an OpenAI error `type`, and a message.
    fn parts(&self) -> (StatusCode, &'static str, String) {
        match self {
            GatewayError::BadRequest(m) => {
                (StatusCode::BAD_REQUEST, "invalid_request_error", m.clone())
            }
            GatewayError::Unauthorized(m) => {
                (StatusCode::UNAUTHORIZED, "authentication_error", m.clone())
            }
            GatewayError::NotFound(m) => {
                (StatusCode::NOT_FOUND, "invalid_request_error", m.clone())
            }
            GatewayError::Config(m) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "server_error", m.clone())
            }
            GatewayError::Upstream { status, message } => {
                let (http_status, kind) = match status {
                    400 | 413 | 422 => (
                        StatusCode::from_u16(*status).unwrap(),
                        "invalid_request_error",
                    ),
                    429 => (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error"),
                    503 => (StatusCode::SERVICE_UNAVAILABLE, "server_error"),
                    504 => (StatusCode::GATEWAY_TIMEOUT, "server_error"),
                    _ => (StatusCode::BAD_GATEWAY, "bad_gateway"),
                };
                (
                    http_status,
                    kind,
                    format!("Raycast API error ({status}): {message}"),
                )
            }
            GatewayError::TokenInvalid { message } => (
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                format!("Raycast account token invalid: {message}"),
            ),
            GatewayError::NoAccount(m) => {
                (StatusCode::SERVICE_UNAVAILABLE, "server_error", m.clone())
            }
            GatewayError::RateLimited {
                retry_after_secs,
                message,
            } => {
                // 429 + `rate_limit_error` is the quota shape the Claude Code
                // harness recognizes and does NOT blind-retry: it reads
                // `Retry-After` and waits precisely. `Retry-After` cannot be
                // negative; clamp for the degenerate race where the cooldown
                // expires between sampling and rendering.
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limit_error",
                    format!("{message} (retry-after: {}s)", retry_after_secs.max(&0)),
                )
            }
            GatewayError::Internal(m) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "relay_error", m.clone())
            }
            GatewayError::Unavailable(m) => {
                (StatusCode::SERVICE_UNAVAILABLE, "server_error", m.clone())
            }
        }
    }

    /// The HTTP status and human-readable message for this error, without any
    /// protocol-specific `type` label. Protocol error renderers (e.g. the
    /// Anthropic `{ "type": "error", ... }` shape) build their own envelope
    /// from these, keeping the status mapping centralized here.
    pub fn status_and_message(&self) -> (StatusCode, String) {
        let (status, _kind, message) = self.parts();
        (status, message)
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    message: String,
    #[serde(rename = "type")]
    kind: &'static str,
    code: Option<String>,
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let (status, kind, message) = self.parts();
        if status.is_server_error() {
            tracing::error!(status = %status, kind, %message, "gateway error");
        }
        let body = ErrorBody {
            error: ErrorDetail {
                message,
                kind,
                code: None,
            },
        };
        let mut response = (status, Json(body)).into_response();
        // Retry hints the Claude Code harness understands:
        // * `Retry-After` on 429 → it pauses exactly that long and shows
        //   "rate limited until <time>" instead of blind-retrying.
        // * `x-should-retry: false` on 5xx → its `shouldRetry` returns false
        //   immediately (a 503 without it is retried on exponential backoff,
        //   which is the "client waits forever" symptom).
        match &self {
            GatewayError::RateLimited {
                retry_after_secs, ..
            } => {
                if let Ok(value) =
                    axum::http::HeaderValue::from_str(&retry_after_secs.max(&0).to_string())
                {
                    response
                        .headers_mut()
                        .insert(axum::http::header::RETRY_AFTER, value);
                }
            }
            GatewayError::NoAccount(_) => {
                response.headers_mut().insert(
                    axum::http::HeaderName::from_static("x-should-retry"),
                    axum::http::HeaderValue::from_static("false"),
                );
            }
            _ => {}
        }
        response
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (_, _, message) = self.parts();
        f.write_str(&message)
    }
}

impl std::error::Error for GatewayError {}

/// reqwest 的 Display 只给最外层("error sending request for url ..."),
/// 沿 `source()` 走到底,才能看到 dns / tcp connect / tls 等真实根因。
pub(crate) fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    message
}

impl From<reqwest::Error> for GatewayError {
    fn from(e: reqwest::Error) -> Self {
        GatewayError::Internal(format!("upstream request error: {}", error_chain(&e)))
    }
}

impl From<serde_json::Error> for GatewayError {
    fn from(e: serde_json::Error) -> Self {
        GatewayError::Internal(format!("serialization error: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::GatewayError;

    #[tokio::test]
    async fn reqwest_error_keeps_source_chain() {
        let e = reqwest::Client::new()
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .unwrap_err();
        let head = format!("upstream request error: {e}");
        let GatewayError::Internal(message) = GatewayError::from(e) else {
            panic!("expected Internal");
        };
        assert!(message.len() > head.len(), "chain missing: {message}");
        println!("{message}");
    }
}
