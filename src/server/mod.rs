//! The HTTP gateway layer: shared state, health tracking, middleware, route
//! handlers, and router assembly.

pub mod handlers;
pub mod health;
pub mod middleware;
pub mod pipeline;
pub mod state;

use axum::Router;
use axum::http::Method;
use axum::routing::{get, post};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use self::state::SharedState;

/// Build the gateway router with all routes, auth, CORS, and tracing wired up.
pub fn build_router(state: SharedState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any)
        .max_age(std::time::Duration::from_secs(86_400));

    // Inference endpoints additionally get the debug-capture layer, which
    // records the inbound request (file dump via DEBUG_DUMP_DIR) and scopes
    // the request id for correlating upstream dumps. It sits *inside* the
    // auth layer (added below), so unauthenticated requests are never captured.
    let inference = Router::new()
        .route(
            "/v1/chat/completions",
            post(handlers::chat::chat_completions),
        )
        .route("/v1/messages", post(handlers::messages::messages))
        .route("/v1/responses", post(handlers::responses::responses))
        .layer(axum::middleware::from_fn(middleware::debug_capture));

    // The authenticated API surface: inference plus the read-only endpoints.
    let api = inference
        .route("/v1/models", get(handlers::models::list_models))
        .route("/health", get(handlers::health::health))
        .route("/ready", get(handlers::health::ready))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::require_api_key,
        ));

    api.fallback(handlers::not_found)
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state)
}
