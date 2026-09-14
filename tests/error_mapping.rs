//! Offline tests for `GatewayError`'s HTTP mapping.
//!
//! Locks the status code each variant renders as (OpenAI-style error surface)
//! and the human-readable message exposed via `Display`, including the composed
//! `Upstream` message. No network involved.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use raycast2api::error::GatewayError;

fn status_of(err: GatewayError) -> StatusCode {
    err.into_response().status()
}

#[test]
fn maps_variants_to_expected_status_codes() {
    assert_eq!(
        status_of(GatewayError::BadRequest("x".into())),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        status_of(GatewayError::Unauthorized("x".into())),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_of(GatewayError::NotFound("x".into())),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status_of(GatewayError::Config("x".into())),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        status_of(GatewayError::Internal("x".into())),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        status_of(GatewayError::Upstream {
            status: 429,
            message: "rate limited".into(),
        }),
        StatusCode::BAD_GATEWAY
    );
}

#[test]
fn no_account_maps_to_503() {
    assert_eq!(
        status_of(GatewayError::NoAccount("no available account".into())),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[test]
fn display_preserves_client_message() {
    let err = GatewayError::BadRequest("Missing or invalid 'messages' field".into());
    assert_eq!(err.to_string(), "Missing or invalid 'messages' field");
}

#[test]
fn upstream_display_composes_status_and_message() {
    let err = GatewayError::Upstream {
        status: 503,
        message: "unavailable".into(),
    };
    assert_eq!(err.to_string(), "Raycast API error (503): unavailable");
}
