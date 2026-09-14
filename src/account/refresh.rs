//! src/account/refresh.rs
//! 账号额度刷新:遍历账号调上游 /me + /ai/credits,结果入库并调整可用性。
//!
//! 由 `main` 装配调用(避免 raycast ↔ account 循环依赖):启动立即一遍,
//! 之后每 [`REFRESH_INTERVAL`] 一遍;`POST /v1/accounts/refresh` 也可手动触发。

use std::sync::Arc;
use std::time::Duration;

use crate::account::store::AccountStore;
use crate::account::wire::AccountInfo;
use crate::error::GatewayError;
use crate::raycast::client::RaycastClient;

/// 定时刷新间隔(6 小时)。
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// 刷新后一个账号的可用性决策。
#[derive(Debug, Clone, PartialEq)]
pub enum AvailabilityDecision {
    /// 摘除,携带原因(额度耗尽)。
    MarkUnavailable(&'static str),
    /// 恢复可用(查询成功即证明 token 有效,且有余额)。
    MarkAvailable,
    /// 额度未知,不动当前状态。
    Keep,
}

/// 纯决策:剩余额度 <= 0 摘除;> 0 恢复;未知保持。
pub fn decide(info: &AccountInfo) -> AvailabilityDecision {
    match info.remaining_credits {
        Some(remaining) if remaining <= 0.0 => {
            AvailabilityDecision::MarkUnavailable("额度耗尽")
        }
        Some(_) => AvailabilityDecision::MarkAvailable,
        None => AvailabilityDecision::Keep,
    }
}

/// 刷新所有账号(含不可用的 —— 它们的 token 可能已恢复)。单个账号
/// 失败只记日志,不影响其余。
pub async fn refresh_all(store: &Arc<AccountStore>, client: &RaycastClient) {
    let accounts = match store.list() {
        Ok(accounts) => accounts,
        Err(err) => {
            tracing::error!("account refresh: list failed: {err}");
            return;
        }
    };

    for account in &accounts {
        match fetch_info(client, &account.access_token).await {
            Ok(Some(info)) => {
                if let Err(err) = store.update_info(&account.email, &info) {
                    tracing::warn!("account refresh: {} update failed: {err}", account.email);
                    continue;
                }
                match decide(&info) {
                    AvailabilityDecision::MarkUnavailable(reason) => {
                        store.mark_unavailable(&account.email, reason);
                    }
                    AvailabilityDecision::MarkAvailable => {
                        store.set_available(&account.email, true, None);
                    }
                    AvailabilityDecision::Keep => {}
                }
            }
            // 401:token 失效,摘除。
            Ok(None) => {
                store.mark_unavailable(&account.email, "token 失效 (401)");
            }
            // 其他错误:不动状态,已记日志。
            Err(()) => {}
        }
    }
    tracing::info!("account refresh: {} accounts processed", accounts.len());
}

/// 拉一个账号的 me + credits。`Ok(None)` = 401;`Err(())` = 其他错误(已记日志)。
async fn fetch_info(client: &RaycastClient, token: &str) -> Result<Option<AccountInfo>, ()> {
    let me = match client.fetch_me(token).await {
        Ok(me) => me,
        Err(GatewayError::Upstream { status: 401, .. }) => return Ok(None),
        Err(err) => {
            tracing::warn!("account refresh: /me failed: {err}");
            return Err(());
        }
    };
    let credits = match client.fetch_credits(token).await {
        Ok(credits) => credits,
        Err(GatewayError::Upstream { status: 401, .. }) => return Ok(None),
        Err(err) => {
            tracing::warn!("account refresh: /ai/credits failed: {err}");
            return Err(());
        }
    };
    Ok(Some(AccountInfo::from((me, credits))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::wire::AccountInfo;

    fn info(remaining: Option<f64>) -> AccountInfo {
        AccountInfo {
            remaining_credits: remaining,
            ..AccountInfo::default()
        }
    }

    #[test]
    fn zero_or_negative_credits_mark_unavailable() {
        assert!(matches!(
            decide(&info(Some(0.0))),
            AvailabilityDecision::MarkUnavailable(_)
        ));
        assert!(matches!(
            decide(&info(Some(-1.0))),
            AvailabilityDecision::MarkUnavailable(_)
        ));
    }

    #[test]
    fn positive_credits_restore() {
        assert_eq!(decide(&info(Some(0.01))), AvailabilityDecision::MarkAvailable);
    }

    #[test]
    fn unknown_credits_keep_state() {
        assert_eq!(decide(&info(None)), AvailabilityDecision::Keep);
    }
}
