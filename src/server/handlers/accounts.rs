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

/// `GET /v1/accounts/{email}` — 单个账号。默认读库(零上游开销);
/// `?live=1` 实时调上游 /me + /ai/credits 拉最新并把结果写回库。
pub async fn get_one(
    State(state): State<SharedState>,
    Path(email): Path<String>,
    raw_query: axum::extract::RawQuery,
) -> Result<Json<AccountView>, GatewayError> {
    let live = raw_query
        .0
        .as_deref()
        .map(|q| q.split('&').any(|kv| kv == "live=1" || kv == "live=true"))
        .unwrap_or(false);

    if !live {
        let row = state
            .accounts
            .get(&email)?
            .ok_or_else(|| GatewayError::NotFound(format!("no account with email {email}")))?;
        return Ok(Json(row.to_view()));
    }

    // live: 取该账号的 token 实时查询;401 时先尝试自动刷新再重查一次。
    let row = state
        .accounts
        .get(&email)?
        .ok_or_else(|| GatewayError::NotFound(format!("no account with email {email}")))?;
    let token = row.access_token.clone();
    let (info_result, renewed) =
        match crate::account::quota_refresh::fetch_info(&state.client, &token).await {
            Ok(Some(info)) => (Ok(Some(info)), false),
            Ok(None) => {
                // token 死了:能自动续命就续,续不上按 401 摘号处理。
                match crate::account::token_renewal::auto_renew(
                    &state.accounts,
                    state.client.http(),
                    &row,
                )
                .await
                {
                    Ok(true) => {
                        let new_token = state
                            .accounts
                            .get(&email)?
                            .map(|r| r.access_token)
                            .unwrap_or_else(|| token.clone());
                        (
                            crate::account::quota_refresh::fetch_info(&state.client, &new_token)
                                .await,
                            true,
                        )
                    }
                    Ok(false) => (Ok(None), false),
                    Err(()) => (Ok(None), false),
                }
            }
            Err(()) => (Err(()), false),
        };

    match info_result {
        Ok(Some(info)) => {
            state.accounts.update_info(&email, &info)?;
            let row = state.accounts.get(&email)?.unwrap_or(row);
            Ok(Json(row.to_view()))
        }
        Ok(None) => {
            state.accounts.mark_unavailable(
                &email,
                if renewed {
                    "token 自动刷新后仍失效 (401)"
                } else {
                    "token 失效 (401)"
                },
            );
            Err(GatewayError::TokenInvalid {
                message: format!(
                    "token for {email} is no longer valid{}; re-authorize this account",
                    if renewed { " after auto-refresh" } else { "" }
                ),
            })
        }
        Err(()) => Err(GatewayError::Internal(format!(
            "upstream query failed for {email}; check logs"
        ))),
    }
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
        Err(GatewayError::NotFound(format!(
            "no account with email {email}"
        )))
    }
}

/// `POST /v1/accounts/{email}/restore` — 手动恢复账号:清冷却 + 置可用,
/// 并实时查一次上游验证。适用于已处理完封禁原因(如确认邮箱)的账号,
/// 不用等冷却自然到期。
///
/// 返回恢复后的账号视图;上游查询失败不阻塞恢复(冷却已清,下一个刷新
/// 周期会更新状态),只把错误附在响应里。
pub async fn restore(
    State(state): State<SharedState>,
    Path(email): Path<String>,
) -> Result<Response, GatewayError> {
    if !state.accounts.reinstate(&email)? {
        return Err(GatewayError::NotFound(format!(
            "no account with email {email}"
        )));
    }

    // 实时验证:能查通则顺手更新额度信息;查不通不影响"已恢复"这个事实。
    let mut upstream_note: Option<String> = None;
    if let Some(row) = state.accounts.get(&email)? {
        match crate::account::quota_refresh::fetch_info(&state.client, &row.access_token).await {
            Ok(Some(info)) => {
                state.accounts.update_info(&email, &info)?;
            }
            Ok(None) => {
                // token 已死:恢复没意义,摘回去并明确告知。
                state.accounts.mark_unavailable(&email, "token 失效 (401)");
                upstream_note = Some("token 已失效 (401),恢复后仍不可用;需重新授权".into());
            }
            // Ok(None) 分支已覆盖 token 失效;此 match 穷尽性占位由编译器保证。
            Err(()) => {
                upstream_note =
                    Some("上游查询失败(网络/其他),账号已恢复可用但额度信息未更新".into());
            }
        }
    }

    let row = state
        .accounts
        .get(&email)?
        .ok_or_else(|| GatewayError::NotFound(format!("no account with email {email}")))?;
    let mut view = row.to_view();
    view.unavailable_reason = upstream_note.clone();
    // note 不该污染 available 语义:查询成功时 reason 已是 None。
    Ok(Json(view).into_response())
}

/// `POST /v1/accounts/refresh` — 手动触发一次全量额度刷新(后台执行)。
pub async fn refresh(State(state): State<SharedState>) -> Response {
    let accounts: Arc<AccountStore> = state.accounts.clone();
    let client = state.client.clone();
    tokio::spawn(async move {
        crate::account::quota_refresh::refresh_all(&accounts, &client).await;
    });
    StatusCode::ACCEPTED.into_response()
}
