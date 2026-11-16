//! Authenticated statistics detail queries; protocol response envelopes are unchanged.
use crate::{
    error::GatewayError,
    server::state::SharedState,
    statistics::wire::{DetailParams, ListParams, Statistic},
};
use axum::{Json, extract::State};

pub async fn list(
    State(state): State<SharedState>,
    Json(params): Json<ListParams>,
) -> Result<Json<crate::core::page::Page<Statistic>>, GatewayError> {
    params.pagination().map_err(GatewayError::BadRequest)?;
    state
        .statistics
        .list(&params)
        .map(Json)
        .map_err(GatewayError::Internal)
}

pub async fn detail(
    State(state): State<SharedState>,
    Json(params): Json<DetailParams>,
) -> Result<Json<Statistic>, GatewayError> {
    state
        .statistics
        .detail(&params.id)
        .map_err(GatewayError::Internal)?
        .map(Json)
        .ok_or_else(|| GatewayError::NotFound(format!("no statistic {}", params.id)))
}
