//! Console login and account, request-log, and usage-statistics management.

pub mod accounts;
pub mod login;
pub mod logs;
pub mod statistics;

use axum::{
    Router,
    routing::{get, post},
};

use crate::server::state::SharedState;

/// API-key-protected routes, relative to `/v1`; login is mounted outside auth.
pub(crate) fn routes() -> Router<SharedState> {
    Router::new()
        .route("/accounts/list", post(accounts::list))
        .route("/accounts/detail", post(accounts::detail))
        .route("/accounts/create", post(accounts::create))
        .route("/accounts/delete", post(accounts::delete))
        .route("/accounts/restore", post(accounts::restore))
        .route("/accounts/export", get(accounts::export))
        .route("/accounts/refresh", post(accounts::refresh))
        .route("/logs/list", post(logs::list))
        .route("/logs/detail", post(logs::detail))
        .route("/statistics/list", post(statistics::list))
        .route("/statistics/detail", post(statistics::detail))
}
