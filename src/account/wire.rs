//! 账号域的对外 JSON 形状:入站记录、额度信息、出站视图。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::GatewayError;
use crate::raycast::wire::{RaycastCredits, RaycastMe};

/// `POST /v1/accounts/create` 的账号请求体。
/// `user` 保留完整原始对象(存 `user_json` 列),email 从中提取。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AccountRecord {
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub user: Value,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub obtained_at: Option<String>,
    /// 上游明确提供的 access token 过期时间(unix 秒);未知时不推测。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token_expires_at: Option<i64>,
    /// 该账号专属的 `X-Raycast-DeviceId`(64 位小写 hex)。注册脚本在激活
    /// pro_trial 时生成并全程使用同一值;网关聊天必须沿用,否则服务端看到
    /// "换设备"。必填:导入时校验存在性与形态。
    #[serde(default)]
    pub device_id: Option<String>,
    /// 是否参与调度;缺省 `true`。已存在的账号只有显式给出时才被改写。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// 调度优先级,数值越大越优先;缺省 `0`。已存在的账号只有显式给出时才被改写。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
}

impl AccountRecord {
    /// 唯一标识:`user.email`。
    pub fn email(&self) -> Option<String> {
        self.user.get("email")?.as_str().map(str::to_string)
    }

    /// 校验单条记录:device_id 必填且为 64 位小写 hex;priority 须在 `i32` 范围内。
    /// 每个账号必须有自己的 device_id —— 没有就没有可用的签名身份。
    pub fn validate(&self) -> Result<(), GatewayError> {
        let id = self.device_id.as_deref().unwrap_or("");
        let ok = id.len() == 64
            && id.bytes().all(|b| b.is_ascii_hexdigit())
            && !id.bytes().any(|b| b.is_ascii_uppercase());
        if !ok {
            return Err(GatewayError::BadRequest(format!(
                "account entry must carry a device_id: 64-char lowercase hex (got {} chars)",
                id.len()
            )));
        }
        if self.access_token.trim().is_empty() {
            return Err(GatewayError::BadRequest(
                "account entry must carry a non-empty access_token".into(),
            ));
        }
        if let Some(priority) = self.priority {
            priority_i32(priority)?;
        }
        if self.access_token_expires_at.is_some_and(|value| value < 0) {
            return Err(GatewayError::BadRequest(
                "access_token_expires_at must be non-negative Unix seconds".into(),
            ));
        }
        Ok(())
    }
}

/// 把请求中的 priority 收窄为 `i32`;越界返回统一 400 错误。
pub fn priority_i32(value: i64) -> Result<i32, GatewayError> {
    i32::try_from(value).map_err(|_| {
        GatewayError::BadRequest(format!(
            "priority must be an integer between -2147483648 and 2147483647 (got {value})"
        ))
    })
}

/// 一次额度刷新拉到的信息(来自上游 `/me` + `/ai/credits`,非 Pro 号
/// 附带 `/me/trial_status` 的限额)。
#[derive(Debug, Clone, Default)]
pub struct AccountInfo {
    pub has_pro_features: Option<bool>,
    pub tier: Option<String>,
    pub sub_status: Option<String>,
    pub trial_days_left: Option<i64>,
    pub current_period_end: Option<i64>,
    /// 试用账号每小时请求限额(仅非 Pro 号有;Pro 号为 `None`)。
    pub trial_requests_per_hour: Option<f64>,
    pub remaining_credits: Option<f64>,
    pub total_credits: Option<f64>,
    pub remaining_usd: Option<f64>,
    pub next_credits_at: Option<String>,
}

impl From<(RaycastMe, RaycastCredits)> for AccountInfo {
    fn from((me, credits): (RaycastMe, RaycastCredits)) -> Self {
        let sub = me.subscription;
        Self {
            has_pro_features: me.has_pro_features,
            tier: sub.as_ref().and_then(|s| s.tier.clone()),
            sub_status: sub.as_ref().and_then(|s| s.status.clone()),
            trial_days_left: sub.as_ref().and_then(|s| s.trial_days_left),
            current_period_end: sub.as_ref().and_then(|s| s.current_period_end),
            trial_requests_per_hour: None,
            remaining_credits: credits.remaining_balance_credits,
            total_credits: credits.total_balance_credits,
            remaining_usd: credits.remaining_balance_usd,
            next_credits_at: credits.next_credits_at,
        }
    }
}

/// `POST /v1/accounts/list` 返回的一条账号视图:token 掩码,含额度与可用状态。
#[derive(Debug, Serialize)]
pub struct AccountView {
    pub email: String,
    pub tag: Option<String>,
    pub enabled: bool,
    pub priority: i32,
    /// 掩码输出:只暴露前 8 位,够比对注册/网关是否一致,不泄露全值。
    pub device_id: Option<String>,
    pub access_token: String,
    pub available: bool,
    /// 当前是否参与选号:未停用、未摘除且不在冷却。与列表筛选 `available` 的口径一致。
    /// `available` 仅表示 token/额度健康状态,停用账号可以 `available = true` 但不可调度。
    pub schedulable: bool,
    /// 调度状态:`available` / `cooldown` / `unavailable` / `disabled`,与列表筛选、删除同一口径。
    pub status: &'static str,
    pub unavailable_reason: Option<String>,
    pub obtained_at: Option<String>,
    pub access_token_expires_at: Option<i64>,
    pub has_pro_features: Option<bool>,
    pub tier: Option<String>,
    pub sub_status: Option<String>,
    pub trial_days_left: Option<i64>,
    pub current_period_end: Option<i64>,
    pub trial_requests_per_hour: Option<f64>,
    pub remaining_credits: Option<f64>,
    pub total_credits: Option<f64>,
    pub remaining_usd: Option<f64>,
    pub next_credits_at: Option<String>,
    pub info_fetched_at: Option<String>,
    /// 限额冷却截止(unix 秒);`None` = 不在冷却中。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

/// 掩码:token 前 8 位 + `...`。
pub fn mask_token(token: &str) -> String {
    let prefix: String = token.chars().take(8).collect();
    format!("{prefix}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_extracts_email_from_user() {
        let rec: AccountRecord = serde_json::from_str(
            r#"{ "tag": "a", "user": { "email": "x@y.z", "id": 1 },
                "access_token": "rca_abc", "refresh_token": "r",
                "obtained_at": "2026-09-14T00:00:00Z" }"#,
        )
        .unwrap();
        assert_eq!(rec.email().as_deref(), Some("x@y.z"));
    }

    #[test]
    fn record_without_email_or_user_is_tolerant() {
        // user 缺失或没有 email 都返回 None,而不是报错。
        let rec: AccountRecord = serde_json::from_str(r#"{ "access_token": "t" }"#).unwrap();
        assert_eq!(rec.email(), None);
    }

    #[test]
    fn token_masking() {
        assert_eq!(mask_token("rca_1234567890abcdef"), "rca_1234...");
        assert_eq!(mask_token("short"), "short...");
    }
}
