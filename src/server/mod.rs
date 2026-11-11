//! The HTTP gateway layer: shared state, health tracking, middleware, route
//! handlers, and router assembly.

pub mod compact;
pub mod handlers;
pub mod health;
pub mod middleware;
pub mod pipeline;
pub mod state;

use axum::Router;
use axum::http::Method;
use axum::response::Html;
use axum::routing::{get, post};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use self::state::SharedState;

/// Build the gateway router with all routes, auth, CORS, and tracing wired up.
pub fn build_router(state: SharedState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers(Any)
        .max_age(std::time::Duration::from_secs(86_400));

    let management = Router::new()
        .route("/accounts/list", post(handlers::accounts::list))
        .route("/accounts/detail", post(handlers::accounts::detail))
        .route("/accounts/create", post(handlers::accounts::create))
        .route("/accounts/delete", post(handlers::accounts::delete))
        .route("/accounts/restore", post(handlers::accounts::restore))
        .route("/accounts/export", get(handlers::accounts::export))
        .route("/accounts/refresh", post(handlers::accounts::refresh))
        .route("/logs/list", post(handlers::logs::list))
        .route("/logs/detail", post(handlers::logs::detail));

    // The authenticated API surface: shared management routes plus inference
    // and health endpoints. The debug-capture layer sits inside the auth layer.
    let api = Router::new()
        .nest("/v1", management)
        .route(
            "/v1/chat/completions",
            post(handlers::chat::chat_completions)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/messages",
            post(handlers::messages::messages).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/responses",
            post(handlers::responses::responses).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/v1/models", get(handlers::models::list_models))
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

    api.route("/v1/{*path}", get(handlers::not_found))
        .layer(axum::middleware::from_fn(middleware::response_format))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state)
        .route(
            "/",
            get(|| async { Html(include_str!("../page/index.html")) }),
        )
}
