//! src/account/quota_refresh.rs
//! 账号额度刷新:遍历账号调上游 /me + /ai/credits,结果入库并调整可用性。
//!
//! 用词约定:本模块管 **额度刷新**(quota refresh,查额度/决策可用性);
//! token 换新(renewal,oauth token 轮换)在 [`crate::account::token_renewal`]。
//!
//! 由 `main` 装配调用(避免 raycast ↔ account 循环依赖):启动立即一遍,
//! 之后每 [`REFRESH_INTERVAL`] 一遍;`POST /v1/accounts/refresh` 可手动刷新单个账号。

use std::sync::Arc;
use std::time::Duration;

use crate::account::store::{AccountRow, AccountStore};
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
///
/// 例外:非 Pro 号(`has_pro_features == false`)的 `/ai/credits` 恒为
/// `0.0`(试用额度按请求次数计,不走 credits),credits=0 是常态而非
/// 耗尽 —— 不能据此摘号,保持 `Keep` 交给冷却/试用到期逻辑处理。
/// `has_pro_features` 未知(`None`)时维持旧行为,按 credits 判定。
pub fn decide(info: &AccountInfo) -> AvailabilityDecision {
    if info.has_pro_features == Some(false) {
        return AvailabilityDecision::Keep;
    }
    match info.remaining_credits {
        Some(remaining) if remaining <= 0.0 => AvailabilityDecision::MarkUnavailable("额度耗尽"),
        Some(_) => AvailabilityDecision::MarkAvailable,
        None => AvailabilityDecision::Keep,
    }
}

/// 试用号是否已过免费窗口:试用天数用尽(`trial_days_left <= 0`)视为
/// 不再提供额度。`None`(非试用或未知)不摘。
pub fn trial_expired(info: &AccountInfo) -> bool {
    matches!(info.trial_days_left, Some(days) if days <= 0)
}

/// 刷新所有账号(含不可用的 —— 它们的 token 可能已恢复)。单个账号
/// 失败只记日志,不影响其余。
pub async fn refresh_all(store: &Arc<AccountStore>, client: &RaycastClient) {
    let accounts = match store.list(None, None, None) {
        Ok(accounts) => accounts,
        Err(err) => {
            tracing::error!("account refresh: list failed: {err}");
            return;
        }
    };

    let accounts = match accounts {
        crate::account::store::AccountList::All(accounts) => accounts,
        crate::account::store::AccountList::Page(_) => unreachable!("refresh query is unpaged"),
    };
    for account in &accounts {
        if let Err(err) = refresh_account(store, client, account).await {
            tracing::warn!("account refresh: {} failed: {err}", account.email);
        }
    }
    tracing::info!("account refresh: {} accounts processed", accounts.len());
}

/// 刷新指定账号,供管理接口调用。
pub async fn refresh_one(
    store: &Arc<AccountStore>,
    client: &RaycastClient,
    email: &str,
) -> Result<(), GatewayError> {
    let account = store
        .get(email)?
        .ok_or_else(|| GatewayError::NotFound(format!("no account with email {email}")))?;
    refresh_account(store, client, &account).await
}

async fn refresh_account(
    store: &Arc<AccountStore>,
    client: &RaycastClient,
    account: &AccountRow,
) -> Result<(), GatewayError> {
    match fetch_info(client, &account.access_token).await {
        Ok(Some(info)) => {
            store.update_info(&account.email, &info)?;
            apply_decision(store, &account.email, &info);
            Ok(())
        }
        // 401:token 失效 —— 有 refresh_token 就自动换新重试一次,
        // 换新失败(invalid_grant/网络)才摘号。
        Ok(None) => {
            match crate::account::token_renewal::auto_renew(store, client.http(), account).await {
                Ok(true) => {
                    // 新 token 生效:重查一次额度信息,走同一套决策。
                    let token = store
                        .get(&account.email)
                        .ok()
                        .flatten()
                        .map(|r| r.access_token)
                        .unwrap_or_else(|| account.access_token.clone());
                    match fetch_info(client, &token).await {
                        Ok(Some(info)) => {
                            store.update_info(&account.email, &info)?;
                            apply_decision(store, &account.email, &info);
                            tracing::info!(
                                "account refresh: {} auto-renewed (available={})",
                                account.email,
                                store
                                    .get(&account.email)
                                    .ok()
                                    .flatten()
                                    .map(|r| r.available)
                                    .unwrap_or(false)
                            );
                            Ok(())
                        }
                        Ok(None) => {
                            store.mark_unavailable(&account.email, "token 自动刷新后仍失效 (401)");
                            Err(GatewayError::TokenInvalid {
                                message: "token 自动刷新后仍失效 (401)".into(),
                            })
                        }
                        Err(err) => {
                            store.mark_unavailable(&account.email, "token 自动刷新后查询失败");
                            Err(err)
                        }
                    }
                }
                Ok(false) => {
                    store.mark_unavailable(&account.email, "token 失效 (401),没有 refresh_token");
                    Err(GatewayError::TokenInvalid {
                        message: "token 失效 (401),且账号没有 refresh_token".into(),
                    })
                }
                // 刷新失败:摘号,原因已记日志。
                Err(()) => {
                    store.mark_unavailable(&account.email, "token 失效 (401),自动刷新失败");
                    Err(GatewayError::TokenInvalid {
                        message: "token 失效 (401),自动刷新失败".into(),
                    })
                }
            }
        }
        // 网络或其他上游错误不改变账号可用状态,交给调用方处理。
        Err(err) => Err(err),
    }
}

/// 把一次刷新的决策落库(所有刷新路径共用,保证判定规则唯一)。
fn apply_decision(store: &Arc<AccountStore>, email: &str, info: &AccountInfo) {
    match decide(info) {
        AvailabilityDecision::MarkUnavailable(reason) => {
            store.mark_unavailable(email, reason);
        }
        // 试用天数用尽:额度即使有剩余也不再续,摘除。
        AvailabilityDecision::MarkAvailable if trial_expired(info) => {
            store.mark_unavailable(email, "试用已到期");
        }
        AvailabilityDecision::MarkAvailable => {
            store.set_available(email, true, None);
        }
        AvailabilityDecision::Keep => {}
    }
}

/// 拉一个账号的 me + credits(非 Pro 号再查 trial_status)。
/// `Ok(None)` = token 失效;`Err(_)` = 其他错误(已记日志)。
pub async fn fetch_info(
    client: &RaycastClient,
    token: &str,
) -> Result<Option<AccountInfo>, GatewayError> {
    let me = match client.fetch_me(token).await {
        Ok(me) => me,
        Err(GatewayError::TokenInvalid { .. }) => return Ok(None),
        Err(err) => {
            tracing::warn!("account refresh: /me failed: {err}");
            return Err(err);
        }
    };
    let credits = match client.fetch_credits(token).await {
        Ok(credits) => credits,
        Err(GatewayError::TokenInvalid { .. }) => return Ok(None),
        Err(err) => {
            tracing::warn!("account refresh: /ai/credits failed: {err}");
            return Err(err);
        }
    };
    let mut info = AccountInfo::from((me, credits));
    // 试用号限额:作为正式字段入库(Pro 号没有,保持 None)。
    // 404/其他错误按"未知"处理,不阻塞信息落库。
    if !info.has_pro_features.unwrap_or(false) {
        match client.fetch_trial_status(token).await {
            Ok(status) => {
                info.trial_requests_per_hour =
                    status.trial_limits.and_then(|l| l.ai_requests_per_hour);
            }
            Err(GatewayError::TokenInvalid { .. }) => return Ok(None),
            Err(GatewayError::Upstream { status, .. }) => {
                tracing::debug!(
                    "account refresh: /me/trial_status returned {status} (非试用号属正常)"
                );
            }
            Err(err) => {
                tracing::debug!("account refresh: /me/trial_status failed: {err}");
            }
        }
    }
    Ok(Some(info))
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
        assert_eq!(
            decide(&info(Some(0.01))),
            AvailabilityDecision::MarkAvailable
        );
    }

    #[test]
    fn unknown_credits_keep_state() {
        assert_eq!(decide(&info(None)), AvailabilityDecision::Keep);
    }

    #[test]
    fn non_pro_zero_credits_keep_state() {
        // 试用号 credits 恒为 0:不摘、不恢复,交给冷却/试用到期逻辑。
        let mut i = info(Some(0.0));
        i.has_pro_features = Some(false);
        assert_eq!(decide(&i), AvailabilityDecision::Keep);

        // 正余额的试用号同样不触发 MarkAvailable(credits 对其无意义)。
        let mut j = info(Some(5.0));
        j.has_pro_features = Some(false);
        assert_eq!(decide(&j), AvailabilityDecision::Keep);
    }

    #[test]
    fn unknown_pro_flag_keeps_credit_decision() {
        // has_pro_features = None:维持旧行为,credits 判定生效。
        assert!(matches!(
            decide(&info(Some(0.0))),
            AvailabilityDecision::MarkUnavailable(_)
        ));

        // 显式 Pro 号 credits=0 照旧摘除。
        let mut i = info(Some(0.0));
        i.has_pro_features = Some(true);
        assert!(matches!(
            decide(&i),
            AvailabilityDecision::MarkUnavailable(_)
        ));

        // Pro 号有余额照旧恢复。
        let mut j = info(Some(1.0));
        j.has_pro_features = Some(true);
        assert_eq!(decide(&j), AvailabilityDecision::MarkAvailable);
    }

    #[test]
    fn trial_expiry_only_for_exhausted_days() {
        let mut i = info(Some(10.0));
        assert!(!trial_expired(&i));
        i.trial_days_left = Some(0);
        assert!(trial_expired(&i));
        i.trial_days_left = Some(-1);
        assert!(trial_expired(&i));
        i.trial_days_left = Some(3);
        assert!(!trial_expired(&i));
        i.trial_days_left = None;
        assert!(!trial_expired(&i));
    }
}
