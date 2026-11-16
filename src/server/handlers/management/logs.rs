//! `/v1/logs` — 请求日志查询(列表 + 详情)。

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use serde::Deserialize;

use crate::error::GatewayError;
use crate::log::wire::LogDetail;
use crate::server::state::SharedState;

/// `POST /v1/logs/list` 请求体。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    pub page: Option<u32>,
    pub page_size: Option<u32>,
    pub created_at: Option<i64>,
    pub request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetailParams {
    pub request_id: String,
}

/// `POST /v1/logs/list` — 分页查询日志(降序,不含 body)。
pub async fn list(
    State(state): State<SharedState>,
    Json(params): Json<ListParams>,
) -> Result<axum::response::Response, GatewayError> {
    let page = params.page.unwrap_or(1);
    let page_size = params.page_size.unwrap_or(10);
    if page == 0 || !(1..=100).contains(&page_size) {
        return Err(GatewayError::BadRequest(
            "page must be positive and page_size must be in 1..=100".into(),
        ));
    }
    let result = state
        .log
        .list(
            (page, page_size),
            params.created_at,
            params.request_id.as_deref(),
        )
        .map_err(GatewayError::Internal)?;
    Ok(Json(result).into_response())
}

/// `POST /v1/logs/detail` — 详情(三体全文);不存在 404。
pub async fn detail(
    State(state): State<SharedState>,
    Json(params): Json<DetailParams>,
) -> Result<Json<LogDetail>, GatewayError> {
    state
        .log
        .detail(&params.request_id)
        .map(Json)
        .ok_or_else(|| GatewayError::NotFound(format!("no request log {}", params.request_id)))
}
