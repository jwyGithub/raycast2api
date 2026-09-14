//! The HTTP gateway layer: shared state, health tracking, middleware, route
//! handlers, and router assembly.

pub mod handlers;
pub mod health;
pub mod middleware;
pub mod pipeline;
pub mod state;

use axum::Router;
use axum::http::Method;
use axum::routing::{delete, get, post};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use self::state::SharedState;

/// Build the gateway router with all routes, auth, CORS, and tracing wired up.
pub fn build_router(state: SharedState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS, Method::DELETE])
        .allow_headers(Any)
        .max_age(std::time::Duration::from_secs(86_400));

    // The authenticated API surface: account-pool management plus inference
    // and the read-only endpoints. The debug-capture layer sits inside (after)
    // the auth layer, so unauthenticated requests are never captured.
    let api = Router::new()
        .route(
            "/v1/accounts",
            get(handlers::accounts::list).post(handlers::accounts::upsert),
        )
        .route("/v1/accounts/refresh", post(handlers::accounts::refresh))
        .route("/v1/accounts/{email}", delete(handlers::accounts::delete))
        .route("/v1/logs", get(handlers::logs::list))
        .route("/v1/logs/{request_id}", get(handlers::logs::detail))
        .route(
            "/v1/chat/completions",
            post(handlers::chat::chat_completions),
        )
        .route("/v1/messages", post(handlers::messages::messages))
        .route("/v1/responses", post(handlers::responses::responses))
        .route("/v1/models", get(handlers::models::list_models))
        .route("/health", get(handlers::health::health))
        .route("/ready", get(handlers::health::ready))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::debug_capture,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::require_api_key,
        ));

    api.fallback(handlers::not_found)
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state)
}
