//! 账号域的对外 JSON 形状:入站记录、额度信息、出站视图。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::raycast::wire::{RaycastCredits, RaycastMe};

/// account.json 的一条账号(也是 `POST /v1/accounts` 的请求体)。
/// `user` 保留完整原始对象(存 `user_json` 列),email 从中提取。
#[derive(Debug, Clone, Deserialize)]
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
}

impl AccountRecord {
    /// 唯一标识:`user.email`。
    pub fn email(&self) -> Option<String> {
        self.user.get("email")?.as_str().map(str::to_string)
    }
}

/// 一次额度刷新拉到的信息(来自上游 `/me` + `/ai/credits`)。
#[derive(Debug, Clone, Default)]
pub struct AccountInfo {
    pub tier: Option<String>,
    pub sub_status: Option<String>,
    pub trial_days_left: Option<i64>,
    pub current_period_end: Option<i64>,
    pub remaining_credits: Option<f64>,
    pub total_credits: Option<f64>,
    pub remaining_usd: Option<f64>,
    pub next_credits_at: Option<String>,
}

impl From<(RaycastMe, RaycastCredits)> for AccountInfo {
    fn from((me, credits): (RaycastMe, RaycastCredits)) -> Self {
        let sub = me.subscription;
        Self {
            tier: sub.as_ref().and_then(|s| s.tier.clone()),
            sub_status: sub.as_ref().and_then(|s| s.status.clone()),
            trial_days_left: sub.as_ref().and_then(|s| s.trial_days_left),
            current_period_end: sub.as_ref().and_then(|s| s.current_period_end),
            remaining_credits: credits.remaining_balance_credits,
            total_credits: credits.total_balance_credits,
            remaining_usd: credits.remaining_balance_usd,
            next_credits_at: credits.next_credits_at,
        }
    }
}

/// `GET /v1/accounts` 的一条响应:token 掩码,含额度与可用状态。
#[derive(Debug, Serialize)]
pub struct AccountView {
    pub email: String,
    pub tag: Option<String>,
    pub access_token: String,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub obtained_at: Option<String>,
    pub tier: Option<String>,
    pub sub_status: Option<String>,
    pub trial_days_left: Option<i64>,
    pub current_period_end: Option<i64>,
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
