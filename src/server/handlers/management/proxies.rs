//! 代理管理接口；沿用管理入口的身份验证与 JSON 响应格式。
use crate::{
    error::GatewayError,
    proxies::store::{
        ProxyInput, ProxyListItem, ProxyProbeResult, ProxyRow, ProxyStore, ProxyTestResult,
    },
    server::state::SharedState,
};
use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdRequest {
    pub id: String,
}
#[derive(Deserialize)]
pub struct UpdateRequest {
    pub id: String,
    #[serde(flatten)]
    pub input: ProxyInput,
}

pub async fn list(
    State(state): State<SharedState>,
) -> Result<Json<Vec<ProxyListItem>>, GatewayError> {
    Ok(Json(state.proxies.list()?))
}
pub async fn create(
    State(state): State<SharedState>,
    Json(input): Json<ProxyInput>,
) -> Result<Json<ProxyRow>, GatewayError> {
    Ok(Json(state.proxies.save(None, input)?))
}
pub async fn update(
    State(state): State<SharedState>,
    Json(input): Json<UpdateRequest>,
) -> Result<Json<ProxyRow>, GatewayError> {
    Ok(Json(state.proxies.save(Some(&input.id), input.input)?))
}
pub async fn delete(
    State(state): State<SharedState>,
    Json(input): Json<IdRequest>,
) -> Result<StatusCode, GatewayError> {
    state.proxies.delete(&input.id)?;
    Ok(StatusCode::OK)
}
pub async fn test(
    State(state): State<SharedState>,
    Json(input): Json<IdRequest>,
) -> Result<Json<ProxyTestResult>, GatewayError> {
    let result = state
        .proxies
        .test(&input.id, state.settings.upstream_timeout())
        .await?;
    Ok(Json(result))
}

/// 只测试提交的配置，不读取或写入代理记录。
pub async fn test_config(
    State(state): State<SharedState>,
    Json(input): Json<ProxyInput>,
) -> Result<Json<ProxyProbeResult>, GatewayError> {
    Ok(Json(
        ProxyStore::test_input(input, state.settings.upstream_timeout()).await?,
    ))
}

pub async fn exit_details(
    State(state): State<SharedState>,
    Json(input): Json<IdRequest>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    Ok(Json(
        state
            .proxies
            .exit_details(&input.id, state.settings.upstream_timeout())
            .await?,
    ))
}
