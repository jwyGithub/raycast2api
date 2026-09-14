//! The OpenAI Responses error surface: `{ "error": { "message", "type", "code" } }`.
//!
//! The Responses API uses the same top-level error envelope as the other OpenAI
//! families, which is exactly the shape [`GatewayError`] renders natively, so
//! this is a thin pass-through. It lives in its own module (rather than inline in
//! [`super::outbound`]) to mirror the other protocol folders, whose error
//! envelopes differ and need their own translation.

use axum::response::{IntoResponse, Response};

use crate::error::GatewayError;

/// Render a gateway error in the OpenAI Responses error shape.
pub fn render(error: GatewayError) -> Response {
    error.into_response()
}
