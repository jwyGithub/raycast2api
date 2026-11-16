//! The HTTP gateway layer: shared state, health tracking, middleware, route
//! handlers, and router assembly.

pub mod compact;
pub mod handlers;
pub mod health;
pub mod middleware;
pub mod page;
pub mod pipeline;
pub mod state;

use axum::Router;
use axum::http::Method;
use axum::routing::{get, post};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use self::handlers::{inference, management};
use self::state::SharedState;

/// Build the gateway router with all routes, auth, CORS, and tracing wired up.
pub fn build_router(state: SharedState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers(Any)
        .max_age(std::time::Duration::from_secs(86_400));

    // The authenticated API surface: shared management routes plus inference
    // and health endpoints. The debug-capture layer sits inside the auth layer.
    let api = Router::new()
        .nest("/v1", management::routes().merge(inference::routes()))
        .route("/health", get(handlers::health::health))
        .route("/ready", get(handlers::health::ready))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::request_logging,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::auth,
        ));

    api.route("/v1/login", post(management::login::login))
        .route("/v1/{*path}", get(handlers::not_found))
        .layer(axum::middleware::from_fn(middleware::response_format))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state)
        // Added after the layers on purpose: console files stay outside auth,
        // CORS, and the JSON response wrapper.
        .fallback(page::serve)
}
