//! `/v1/accounts` — 账号池的查看、新增/更新、删除与手动额度刷新。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::account::store::{AccountStore, UpsertOutcome};
use crate::account::wire::{AccountRecord, AccountView};
use crate::error::GatewayError;
use crate::server::state::SharedState;

/// `POST /v1/accounts` 的结果回执。
#[derive(Serialize)]
pub struct UpsertResponse {
    pub result: &'static str,
}

/// `GET /v1/accounts` — 全部账号(token 掩码)。
pub async fn list(
    State(state): State<SharedState>,
) -> Result<Json<Vec<AccountView>>, GatewayError> {
    let views = state
        .accounts
        .list()?
        .iter()
        .map(|row| row.to_view())
        .collect();
    Ok(Json(views))
}

/// `POST /v1/accounts` — 新增或更新一个账号(按 email upsert)。
pub async fn upsert(
    State(state): State<SharedState>,
    Json(record): Json<AccountRecord>,
) -> Result<Json<UpsertResponse>, GatewayError> {
    let result = match state.accounts.upsert(&record)? {
        UpsertOutcome::Inserted => "inserted",
        UpsertOutcome::Updated => "updated",
        UpsertOutcome::Unchanged => "unchanged",
    };
    Ok(Json(UpsertResponse { result }))
}

/// `DELETE /v1/accounts/{email}` — 删除;不存在 404。
pub async fn delete(
    State(state): State<SharedState>,
    Path(email): Path<String>,
) -> Result<StatusCode, GatewayError> {
    if state.accounts.delete(&email)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(GatewayError::NotFound(format!("no account with email {email}")))
    }
}

/// `POST /v1/accounts/refresh` — 手动触发一次全量额度刷新(后台执行)。
pub async fn refresh(State(state): State<SharedState>) -> Response {
    let accounts: Arc<AccountStore> = state.accounts.clone();
    let client = state.client.clone();
    tokio::spawn(async move {
        crate::account::refresh::refresh_all(&accounts, &client).await;
    });
    StatusCode::ACCEPTED.into_response()
}
