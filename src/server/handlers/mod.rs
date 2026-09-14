//! Route handlers for the gateway.

pub mod accounts;
pub mod chat;
pub mod health;
pub mod logs;
pub mod messages;
pub mod models;
pub mod responses;

use crate::error::GatewayError;

/// Catch-all for unknown routes, rendered as an OpenAI-style 404.
pub async fn not_found() -> GatewayError {
    GatewayError::NotFound("Not Found".into())
}
