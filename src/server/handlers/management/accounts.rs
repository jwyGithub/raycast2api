//! `/v1/accounts` — 账号池的查看、新增/更新、删除与手动额度刷新。

use axum::Json;
use axum::extract::{State, rejection::JsonRejection};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::account::store::{AccountRow, AccountStatus, UpsertOutcome};
use crate::account::wire::{AccountRecord, AccountView, mask_token, priority_i32};
use crate::core::page::{Page, page_params};
use crate::error::GatewayError;
use crate::server::state::SharedState;
use crate::util::unix_secs;

/// `POST /v1/accounts/create` 的请求体,支持单账号或账号数组。
#[derive(Deserialize)]
#[serde(untagged)]
pub enum UpsertRequest {
    One(AccountRecord),
    /// 数组条目保留为原始 JSON,逐条反序列化,使错误能带上条目序号。
    Many(Vec<serde_json::Value>),
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
) -> Result<Json<Page<AccountView>>, GatewayError> {
    let (page, page_size) = page_params(params.page, params.page_size)?;
    let page = state
        .accounts
        .list_page(page, page_size, params.status.as_deref())?;
    Ok(Json(Page::new(
        page.items.into_iter().map(account_view).collect(),
        page.total,
        page.page,
        page.page_size,
    )))
}

fn account_view(row: AccountRow) -> AccountView {
    let status = row.status_at(unix_secs() as i64);
    let schedulable = status == AccountStatus::Available;
    AccountView {
        email: row.email,
        tag: row.tag,
        enabled: row.enabled,
        priority: row.priority,
        // 只露前 8 位:够核对注册/网关是否一致,不泄露全值。
        device_id: row
            .device_id
            .as_deref()
            .map(|id| id.chars().take(8).collect::<String>() + "…"),
        access_token: mask_token(&row.access_token),
        available: row.available,
        schedulable,
        status: status.as_str(),
        unavailable_reason: row.unavailable_reason,
        obtained_at: row.obtained_at,
        access_token_expires_at: row.access_token_expires_at,
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
///
/// 数组请求是原子的:任一条目非法(缺 email、device_id 不合法、priority 越界)
/// 或写入失败,整批回滚,响应为错误而非部分成功。
pub async fn create(
    State(state): State<SharedState>,
    Json(request): Json<UpsertRequest>,
) -> Result<Json<UpsertResponse>, GatewayError> {
    match request {
        UpsertRequest::One(record) => Ok(Json(UpsertResponse::One {
            result: upsert_label(state.accounts.upsert(&record)?),
        })),
        UpsertRequest::Many(items) => {
            if items.is_empty() {
                return Err(GatewayError::BadRequest(
                    "account array must contain at least one entry".into(),
                ));
            }
            let records = items
                .into_iter()
                .enumerate()
                .map(|(index, item)| {
                    let record: AccountRecord = serde_json::from_value(item).map_err(|error| {
                        GatewayError::BadRequest(format!("entries[{index}]: {error}"))
                    })?;
                    if record.email().is_none() {
                        return Err(GatewayError::BadRequest(format!(
                            "entries[{index}]: every account entry must contain user.email"
                        )));
                    }
                    Ok(record)
                })
                .collect::<Result<Vec<_>, GatewayError>>()?;
            let results = state
                .accounts
                .upsert_batch(&records)?
                .into_iter()
                .map(upsert_label)
                .collect();
            Ok(Json(UpsertResponse::Many { results }))
        }
    }
}

fn upsert_label(outcome: UpsertOutcome) -> &'static str {
    match outcome {
        UpsertOutcome::Inserted => "inserted",
        UpsertOutcome::Updated => "updated",
        UpsertOutcome::Unchanged => "unchanged",
    }
}

#[derive(Serialize)]
pub struct DeleteResponse {
    /// 实际删除的账号数量(按状态批量删除时可能大于 1)。
    pub deleted: usize,
}

/// `POST /v1/accounts/delete` — 按邮箱或状态删除;邮箱与状态同时传入时须同时匹配。
///
/// 状态与列表筛选使用同一套归类:同时停用又不可用的账号归为 `disabled`。
/// 指定邮箱但不匹配时,404 信息会说明该账号的实际状态。
pub async fn delete(
    State(state): State<SharedState>,
    Json(request): Json<DeleteParams>,
) -> Result<Json<DeleteResponse>, GatewayError> {
    let deleted = state
        .accounts
        .delete(request.email.as_deref(), request.status.as_deref())?;
    if request.email.is_none() || deleted > 0 {
        return Ok(Json(DeleteResponse { deleted }));
    }
    let email = request.email.as_deref().unwrap_or_default();
    let status = request.status.as_deref().filter(|value| !value.is_empty());
    match (status, state.accounts.get(email)?) {
        // 账号存在但状态不符:机器可读的 code 供客户端区分,文案仅供人读。
        (Some(status), Some(row)) => Err(GatewayError::Admission {
            status: StatusCode::NOT_FOUND,
            code: "account_status_mismatch",
            message: format!(
                "account {email} is {} (requested status: {status})",
                row.status_at(unix_secs() as i64).as_str()
            ),
        }),
        (Some(status), None) => Err(GatewayError::NotFound(format!(
            "no account with email {email} and status {status}"
        ))),
        (None, _) => Err(GatewayError::NotFound(format!(
            "no account with email {email}"
        ))),
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
    let (row, upstream_note) =
        crate::account::service::restore(&state.accounts, &state.dispatcher.client, &request.email)
            .await?;
    let mut view = account_view(row);
    view.unavailable_reason = upstream_note.clone();
    // note 不该污染 available 语义:查询成功时 reason 已是 None。
    Ok(Json(view).into_response())
}

/// `POST /v1/accounts/update` 的请求体:开关和/或优先级至少给一个,缺省字段保持不变。
/// 只能修改调度策略,不会触碰 token、tag、device_id。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRequest {
    pub email: String,
    pub enabled: Option<bool>,
    /// 用 `i64` 接收,越界时由 [`priority_i32`] 返回统一的 400 错误而不是提取器的原始拒绝。
    pub priority: Option<i64>,
}

/// `POST /v1/accounts/update` — 修改账号的启停开关与调度优先级。
///
/// `enabled` / `priority` 传 `null` 等同于省略;两者都省略返回 400;
/// `priority` 须在 `i32` 范围内,越界返回 400;请求体结构错误(类型不符、
/// 未知字段、缺少 email)同样统一返回 400 的错误信封,而不是 422。
pub async fn update(
    State(state): State<SharedState>,
    body: Result<Json<UpdateRequest>, JsonRejection>,
) -> Result<Json<AccountView>, GatewayError> {
    let Json(request) =
        body.map_err(|rejection| GatewayError::BadRequest(rejection.body_text()))?;
    if request.enabled.is_none() && request.priority.is_none() {
        return Err(GatewayError::BadRequest(
            "enabled or priority is required".into(),
        ));
    }
    let priority = request.priority.map(priority_i32).transpose()?;
    if !state
        .accounts
        .update_policy(&request.email, request.enabled, priority)?
    {
        return Err(GatewayError::NotFound(format!(
            "no account with email {}",
            request.email
        )));
    }
    let row = state.accounts.get(&request.email)?.ok_or_else(|| {
        GatewayError::NotFound(format!("no account with email {}", request.email))
    })?;
    Ok(Json(account_view(row)))
}

/// `POST /v1/accounts/refresh` — 刷新指定账号额度。
pub async fn refresh(
    State(state): State<SharedState>,
    Json(request): Json<EmailRequest>,
) -> Result<Json<AccountView>, GatewayError> {
    crate::account::quota_refresh::refresh_one(
        &state.accounts,
        &state.dispatcher.client,
        &request.email,
    )
    .await?;
    let row = state.accounts.get(&request.email)?.ok_or_else(|| {
        GatewayError::NotFound(format!("no account with email {}", request.email))
    })?;
    Ok(Json(account_view(row)))
}
