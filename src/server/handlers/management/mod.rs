//! Console login and account, request-log, and usage-statistics management.

pub mod accounts;
mod api_keys;
pub mod login;
pub mod logs;
pub mod models;
pub mod statistics;

use axum::{
    Router,
    routing::{get, post},
};

use crate::server::state::SharedState;

/// Session/API-key-protected routes, relative to /v1; login is public.
pub(crate) fn routes() -> Router<SharedState> {
    Router::new()
        .route("/accounts/list", post(accounts::list))
        .route("/accounts/detail", post(accounts::detail))
        .route("/accounts/create", post(accounts::create))
        .route("/accounts/delete", post(accounts::delete))
        .route("/accounts/restore", post(accounts::restore))
        .route("/accounts/export", get(accounts::export))
        .route("/accounts/refresh", post(accounts::refresh))
        .route("/api-keys/list", post(api_keys::list))
        .route("/api-keys/create", post(api_keys::create))
        .route("/api-keys/update", post(api_keys::update))
        .route("/api-keys/model-options", post(api_keys::model_options))
        .route("/api-keys/update-models", post(api_keys::update_models))
        .route("/api-keys/reset-usage", post(api_keys::reset_usage))
        .route("/api-keys/delete", post(api_keys::delete))
        .route("/logs/list", post(logs::list))
        .route("/logs/detail", post(logs::detail))
        .route("/models/list", post(models::list))
        .route("/models/alias/list", post(models::list_aliases))
        .route("/models/alias/create", post(models::create_alias))
        .route("/models/alias/delete", post(models::delete_alias))
        .route("/statistics/list", post(statistics::list))
        .route("/statistics/detail", post(statistics::detail))
}
