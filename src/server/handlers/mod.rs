//! Management and inference HTTP entry points, plus shared health and fallback handlers.

pub mod health;
pub mod inference;
pub mod management;

use crate::error::GatewayError;

/// Catch-all for unknown routes, rendered as an OpenAI-style 404.
pub async fn not_found() -> GatewayError {
    GatewayError::NotFound("Not Found".into())
}
