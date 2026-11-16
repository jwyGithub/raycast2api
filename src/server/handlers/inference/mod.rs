//! Client-facing inference protocols and model discovery.

pub mod chat;
pub mod messages;
pub mod models;
pub mod responses;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post},
};

use crate::server::state::SharedState;

/// Client API routes, relative to `/v1`.
pub(crate) fn routes() -> Router<SharedState> {
    Router::new()
        .route(
            "/chat/completions",
            post(chat::chat_completions).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route(
            "/messages",
            post(messages::messages).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route(
            "/responses",
            post(responses::responses).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route("/models", get(models::list_models))
}
