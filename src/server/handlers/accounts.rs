//! `/v1/accounts` — 账号池的查看、新增/更新、删除与手动额度刷新。

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::account::store::{AccountList, AccountRow, AccountStore, UpsertOutcome};
use crate::account::wire::{AccountRecord, AccountView, mask_token};
use crate::core::page::Page;
use crate::error::GatewayError;
use crate::server::state::SharedState;

/// `POST /v1/accounts/create` 的请求体,支持单账号或账号数组。
#[derive(Deserialize)]
#[serde(untagged)]
pub enum UpsertRequest {
    One(AccountRecord),
    Many(Vec<AccountRecord>),
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum UpsertResponse {
    One { result: &'static str },
    Many { results: Vec<&'static str> },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountListParams {
    pub page: Option<u32>,
    pub page_size: Option<u32>,
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailRequest {
    pub email: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteParams {
    pub email: Option<String>,
    pub status: Option<String>,
}

/// `POST /v1/accounts/list` — 默认返回第一页,每页 10 个账号。
pub async fn list(
    State(state): State<SharedState>,
    Json(params): Json<AccountListParams>,
) -> Result<axum::response::Response, GatewayError> {
    let page = params.page.unwrap_or(1);
    let page_size = params.page_size.unwrap_or(10);
    if page == 0 || !(1..=100).contains(&page_size) {
        return Err(GatewayError::BadRequest(
            "page must be positive and page_size must be in 1..=100".into(),
        ));
    }
    let result = state
        .accounts
        .list(Some(page), Some(page_size), params.status.as_deref())?;
    Ok(match result {
        AccountList::All(_) => unreachable!("paged account query returns page"),
        AccountList::Page(page) => Json(Page::new(
            page.items.into_iter().map(account_view).collect(),
            page.total,
            page.page,
            page.page_size,
        ))
        .into_response(),
    })
}

fn account_view(row: AccountRow) -> AccountView {
    AccountView {
        email: row.email,
        tag: row.tag,
        access_token: mask_token(&row.access_token),
        available: row.available,
        unavailable_reason: row.unavailable_reason,
        obtained_at: row.obtained_at,
        has_pro_features: row.has_pro_features,
        tier: row.tier,
        sub_status: row.sub_status,
        trial_days_left: row.trial_days_left,
        current_period_end: row.current_period_end,
        trial_requests_per_hour: row.trial_requests_per_hour,
        remaining_credits: row.remaining_credits,
        total_credits: row.total_credits,
        remaining_usd: row.remaining_usd,
        next_credits_at: row.next_credits_at,
        info_fetched_at: row.info_fetched_at,
        cooldown_until: row.cooldown_until,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// GET /v1/accounts/export — 导出可直接导入的账号凭据。
pub async fn export(
    State(state): State<SharedState>,
) -> Result<Json<Vec<AccountRecord>>, GatewayError> {
    if state.settings.api_key.is_none() {
        return Err(GatewayError::Unauthorized(
            "account export requires API_KEY to be configured".into(),
        ));
    }
    Ok(Json(state.accounts.export_records()?))
}

/// `POST /v1/accounts/detail` — 查询单个账号详情。
pub async fn detail(
    State(state): State<SharedState>,
    Json(request): Json<EmailRequest>,
) -> Result<Json<AccountView>, GatewayError> {
    let row = state.accounts.get(&request.email)?.ok_or_else(|| {
        GatewayError::NotFound(format!("no account with email {}", request.email))
    })?;
    Ok(Json(account_view(row)))
}

/// `POST /v1/accounts/create` — 新增或更新账号(按 email upsert)。
pub async fn create(
    State(state): State<SharedState>,
    Json(request): Json<UpsertRequest>,
) -> Result<Json<UpsertResponse>, GatewayError> {
    match request {
        UpsertRequest::One(record) => Ok(Json(UpsertResponse::One {
            result: upsert_result(&state.accounts, &record)?,
        })),
        UpsertRequest::Many(records) => {
            if records.iter().any(|record| record.email().is_none()) {
                return Err(GatewayError::BadRequest(
                    "every account entry must contain user.email".into(),
                ));
            }
            let results = records
                .iter()
                .map(|record| upsert_result(&state.accounts, record))
                .collect::<Result<_, _>>()?;
            Ok(Json(UpsertResponse::Many { results }))
        }
    }
}

fn upsert_result(
    accounts: &AccountStore,
    record: &AccountRecord,
) -> Result<&'static str, GatewayError> {
    Ok(match accounts.upsert(record)? {
        UpsertOutcome::Inserted => "inserted",
        UpsertOutcome::Updated => "updated",
        UpsertOutcome::Unchanged => "unchanged",
    })
}

/// `POST /v1/accounts/delete` — 按邮箱或状态删除;邮箱与状态同时传入时须同时匹配。
pub async fn delete(
    State(state): State<SharedState>,
    Json(request): Json<DeleteParams>,
) -> Result<StatusCode, GatewayError> {
    let deleted = state
        .accounts
        .delete(request.email.as_deref(), request.status.as_deref())?;
    if request.email.is_none() || deleted > 0 {
        Ok(StatusCode::OK)
    } else {
        let message = match request.status {
            Some(status) => format!(
                "no account with email {} and status {status}",
                request.email.as_deref().unwrap_or_default()
            ),
            None => format!(
                "no account with email {}",
                request.email.as_deref().unwrap_or_default()
            ),
        };
        Err(GatewayError::NotFound(message))
    }
}

/// `POST /v1/accounts/restore` — 手动恢复账号:清冷却 + 置可用,
/// 并实时查一次上游验证。适用于已处理完封禁原因(如确认邮箱)的账号,
/// 不用等冷却自然到期。
///
/// 返回恢复后的账号视图;上游查询失败不阻塞恢复(冷却已清,下一个刷新
/// 周期会更新状态),只把错误附在响应里。
pub async fn restore(
    State(state): State<SharedState>,
    Json(request): Json<EmailRequest>,
) -> Result<Response, GatewayError> {
    if !state.accounts.reinstate(&request.email)? {
        return Err(GatewayError::NotFound(format!(
            "no account with email {}",
            request.email
        )));
    }

    // 实时验证:能查通则顺手更新额度信息;查不通不影响"已恢复"这个事实。
    let mut upstream_note: Option<String> = None;
    if let Some(row) = state.accounts.get(&request.email)? {
        match crate::account::quota_refresh::fetch_info(&state.client, &row.access_token).await {
            Ok(Some(info)) => {
                state.accounts.update_info(&request.email, &info)?;
            }
            Ok(None) => {
                // token 已死:恢复没意义,摘回去并明确告知。
                state
                    .accounts
                    .mark_unavailable(&request.email, "token 失效 (401)");
                upstream_note = Some("token 已失效 (401),恢复后仍不可用;需重新授权".into());
            }
            // Ok(None) 分支已覆盖 token 失效;此 match 穷尽性占位由编译器保证。
            Err(()) => {
                upstream_note =
                    Some("上游查询失败(网络/其他),账号已恢复可用但额度信息未更新".into());
            }
        }
    }

    let row = state.accounts.get(&request.email)?.ok_or_else(|| {
        GatewayError::NotFound(format!("no account with email {}", request.email))
    })?;
    let mut view = account_view(row);
    view.unavailable_reason = upstream_note.clone();
    // note 不该污染 available 语义:查询成功时 reason 已是 None。
    Ok(Json(view).into_response())
}

/// `POST /v1/accounts/refresh` — 刷新指定账号额度。
pub async fn refresh(
    State(state): State<SharedState>,
    Json(request): Json<EmailRequest>,
) -> Result<Json<AccountView>, GatewayError> {
    crate::account::quota_refresh::refresh_one(&state.accounts, &state.client, &request.email)
        .await?;
    let row = state.accounts.get(&request.email)?.ok_or_else(|| {
        GatewayError::NotFound(format!("no account with email {}", request.email))
    })?;
    Ok(Json(account_view(row)))
}
