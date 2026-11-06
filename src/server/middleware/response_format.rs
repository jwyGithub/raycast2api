use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::{StreamExt, TryStreamExt};
use serde_json::json;

const MAX_ERROR_RESPONSE_BYTES: usize = 1024 * 1024;

/// Wrap JSON responses from gateway API routes in the shared REST envelope.
/// Inference protocol routes and health probes keep their existing contracts.
pub async fn response_format(request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if !is_rest_api_path(path) || request.method() == Method::HEAD {
        return next.run(request).await;
    }

    let response = next.run(request).await;
    let status = response.status();
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    let (mut parts, body) = response.into_parts();
    let (body, error_message) = if status.is_client_error() || status.is_server_error() {
        let bytes = match to_bytes(body, MAX_ERROR_RESPONSE_BYTES).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::error!(%error, "API error response exceeded size limit");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({
                        "code": StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                        "message": "API error response exceeded size limit",
                        "data": null,
                    })),
                )
                    .into_response();
            }
        };
        let data = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_else(|_| {
                serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
            })
        };
        let message = if data.get("code").is_some() && data.get("data").is_some() {
            data.get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        } else {
            data.get("error")
                .and_then(|error| {
                    error
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .or_else(|| error.as_str())
                })
                .or_else(|| data.get("message").and_then(serde_json::Value::as_str))
                .or_else(|| data.as_str())
                .map(str::to_owned)
        };
        (Body::empty(), message)
    } else {
        (body, None)
    };
    let message = error_message.unwrap_or_else(|| {
        if status.is_success() {
            "success".to_owned()
        } else {
            status
                .canonical_reason()
                .unwrap_or("request failed")
                .to_owned()
        }
    });
    let response_code = status.as_u16();
    parts.headers.remove(CONTENT_LENGTH);
    parts.headers.remove(CONTENT_ENCODING);
    parts
        .headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let body = if status.is_client_error() || status.is_server_error() {
        let bytes = serde_json::to_vec(&json!({
            "code": response_code,
            "message": message,
            "data": null,
        }))
        .expect("REST API envelope serializes");
        Body::from(bytes)
    } else if is_json {
        let prefix = format!("{{\"code\":{response_code},\"data\":",);
        let suffix = format!(
            ",\"message\":{}}}",
            serde_json::to_string(&message).expect("message serializes")
        );
        let body = body
            .into_data_stream()
            .map_ok(axum::body::Bytes::from)
            .chain(futures_util::stream::once(async move {
                Ok::<axum::body::Bytes, axum::Error>(axum::body::Bytes::from(suffix))
            }));
        Body::from_stream(
            futures_util::stream::once(async move {
                Ok::<axum::body::Bytes, axum::Error>(axum::body::Bytes::from(prefix))
            })
            .chain(body),
        )
    } else {
        let bytes = serde_json::to_vec(&json!({
            "code": response_code,
            "message": message,
            "data": null,
        }))
        .expect("REST API envelope serializes");
        Body::from(bytes)
    };
    Response::from_parts(parts, body)
}

fn is_rest_api_path(path: &str) -> bool {
    let excluded = matches!(
        path,
        "/v1/chat/completions"
            | "/v1/messages"
            | "/v1/responses"
            | "/v1/models"
            | "/health"
            | "/ready"
    );
    !excluded && (path.starts_with("/v1/") || path.starts_with("/console/api/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_envelope_scope_excludes_protocol_and_probe_routes() {
        for path in [
            "/v1/chat/completions",
            "/v1/messages",
            "/v1/responses",
            "/v1/models",
            "/health",
            "/ready",
        ] {
            assert!(
                !is_rest_api_path(path),
                "{path} must keep its response format"
            );
        }

        for path in [
            "/v1/accounts",
            "/v1/logs/list",
            "/v1/logs/detail",
            "/console/api/overview",
            "/console/api/accounts",
        ] {
            assert!(is_rest_api_path(path), "{path} must use the REST envelope");
        }
    }
}
