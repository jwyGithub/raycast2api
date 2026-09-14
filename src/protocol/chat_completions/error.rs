//! The OpenAI-style error surface: `{ "error": { "message", "type", "code" } }`.
//!
//! This is exactly the shape [`GatewayError`] renders natively, so OpenAI-facing
//! error rendering is a thin pass-through. It lives in its own module (rather
//! than inline in [`super::outbound`]) to mirror the other protocols, whose
//! error envelopes differ and need their own translation here.

use axum::response::{IntoResponse, Response};

use crate::error::GatewayError;

/// Render a gateway error in the OpenAI error shape.
pub fn render(error: GatewayError) -> Response {
    error.into_response()
}
