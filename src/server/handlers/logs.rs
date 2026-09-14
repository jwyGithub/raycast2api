//! `/v1/logs` — 请求日志查询(列表 + 详情)。

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;

use crate::error::GatewayError;
use crate::log::wire::{LogDetail, LogEntry};
use crate::server::state::SharedState;

/// 列表查询参数。
#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub limit: Option<u32>,
    /// 翻页:取 id 小于该值的更旧记录。
    pub before_id: Option<i64>,
}

/// `GET /v1/logs` — 列表(降序,不含 body)。
pub async fn list(
    State(state): State<SharedState>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<LogEntry>>, GatewayError> {
    let limit = params.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(GatewayError::BadRequest(format!(
            "limit must be in 1..=200, got {limit}"
        )));
    }
    Ok(Json(state.log.list(limit, params.before_id)))
}

/// `GET /v1/logs/{request_id}` — 详情(三体全文);不存在 404。
pub async fn detail(
    State(state): State<SharedState>,
    Path(request_id): Path<String>,
) -> Result<Json<LogDetail>, GatewayError> {
    state
        .log
        .detail(&request_id)
        .map(Json)
        .ok_or_else(|| GatewayError::NotFound(format!("no request log {request_id}")))
}
