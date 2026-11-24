//! src/account/quota_refresh.rs
//! 账号额度刷新:遍历账号调上游 /me + /ai/credits,结果入库并调整可用性。
//!
//! 用词约定:本模块管 **额度刷新**(quota refresh,查额度/决策可用性);
//! token 换新(renewal,oauth token 轮换)在 [`crate::account::token_renewal`]。
//!
//! 由 `main` 定时调用，启动后等待 [`REFRESH_INTERVAL`] 再执行首次刷新。
//! `POST /v1/accounts/refresh` 可手动刷新单个账号。

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
    if trial_expired(info) {
        return AvailabilityDecision::MarkUnavailable("试用已到期");
    }
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

/// 刷新未停用且不在冷却的账号(含不可用账号,它们的 token 可能已恢复)。
/// 单个账号失败只记日志,不影响其余。
pub async fn refresh_all(store: &Arc<AccountStore>, client: &RaycastClient) {
    let accounts = match store.list_all() {
        Ok(accounts) => accounts,
        Err(err) => {
            tracing::error!("account refresh: list failed: {err}");
            return;
        }
    };
    let mut skipped = 0;
    for account in &accounts {
        let account = match store.get_for_upstream(&account.email) {
            Ok(account) => account,
            Err(GatewayError::Admission { .. }) => {
                skipped += 1;
                continue;
            }
            Err(err) => {
                tracing::warn!("account refresh: {} failed: {err}", account.email);
                continue;
            }
        };
        if let Err(err) = refresh_account(store, client, &account).await {
            tracing::warn!("account refresh: {} failed: {err}", account.email);
        }
    }
    tracing::info!(
        "account refresh: {} accounts processed, {skipped} skipped",
        accounts.len() - skipped
    );
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
    let account = store.get_for_upstream(&account.email)?;
    match fetch_info(store, client, &account.email).await {
        Ok(Some(info)) => {
            store.update_info(&account.email, &info)?;
            apply_decision(store, &account.email, &info);
            Ok(())
        }
        // 401:token 失效 —— 有 refresh_token 就自动换新重试一次,
        // 换新失败(invalid_grant/网络)才摘号。
        Ok(None) => {
            match crate::account::token_renewal::auto_renew(store, client, &account).await {
                Ok(_) => {
                    // 新 token 生效:重查一次额度信息,走同一套决策。
                    match fetch_info(store, client, &account.email).await {
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
                        Err(err) => Err(record_refresh_error(store, &account.email, err)),
                    }
                }
                Err(err @ GatewayError::TokenInvalid { .. }) => {
                    store.mark_unavailable(&account.email, "token 失效，无法续期");
                    Err(err)
                }
                Err(err) => Err(err),
            }
        }
        Err(err) => Err(record_refresh_error(store, &account.email, err)),
    }
}

/// 明确的风控/订阅拒绝必须摘号;网络和其他上游错误保留原状态。
pub(crate) fn record_refresh_error(
    store: &AccountStore,
    email: &str,
    err: GatewayError,
) -> GatewayError {
    if err.is_account_dead() {
        store.mark_unavailable(email, &format!("账号不可用 ({err})"));
    }
    err
}

/// 把一次刷新的决策落库(所有刷新路径共用,保证判定规则唯一)。
fn apply_decision(store: &Arc<AccountStore>, email: &str, info: &AccountInfo) {
    match decide(info) {
        AvailabilityDecision::MarkUnavailable(reason) => {
            store.mark_unavailable(email, reason);
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
    store: &AccountStore,
    client: &RaycastClient,
    email: &str,
) -> Result<Option<AccountInfo>, GatewayError> {
    let account = store.get_for_upstream(email)?;
    let me = match client.fetch_me(&account.access_token).await {
        Ok(me) => me,
        Err(GatewayError::TokenInvalid { .. }) => return Ok(None),
        Err(err) => {
            tracing::warn!("account refresh: /me failed: {err}");
            return Err(err);
        }
    };
    let account = store.get_for_upstream(email)?;
    let credits = match client.fetch_credits(&account.access_token).await {
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
        let account = store.get_for_upstream(email)?;
        match client.fetch_trial_status(&account.access_token).await {
            Ok(status) => {
                info.trial_requests_per_hour =
                    status.trial_limits.and_then(|l| l.ai_requests_per_hour);
            }
            Err(GatewayError::TokenInvalid { .. }) => return Ok(None),
            Err(err) if err.is_account_dead() => return Err(err),
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
    use crate::util::unix_secs;

    #[tokio::test]
    async fn paused_accounts_never_query_upstream_even_from_stale_snapshots() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let state = crate::server::state::compact_test_state();
        let store = &state.accounts;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = state.dispatcher.client.clone();
        // 所有误发的网络请求都只会到本地探针,不会接触真实上游。
        client.http = reqwest::Client::builder()
            .proxy(
                reqwest::Proxy::all(format!("http://{}", listener.local_addr().unwrap())).unwrap(),
            )
            .connect_timeout(Duration::from_millis(100))
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        let probe = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                observed.fetch_add(1, Ordering::SeqCst);
                let mut buffer = [0; 1024];
                let _ = stream.read(&mut buffer).await;
                let _ = stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            }
        });
        let record = serde_json::from_value(serde_json::json!({
            "user": {"email": "paused@test"},
            "device_id": "1111111111111111111111111111111111111111111111111111111111111111",
            "access_token": "rca_old", "refresh_token": "refresh_old"
        }))
        .unwrap();
        store.upsert(&record).unwrap();

        for code in ["account_disabled", "account_cooldown"] {
            store
                .update_policy("paused@test", Some(true), None)
                .unwrap();
            store.reinstate("paused@test").unwrap();
            let snapshot = store.get("paused@test").unwrap().unwrap();
            if code == "account_disabled" {
                store
                    .update_policy("paused@test", Some(false), None)
                    .unwrap();
            } else {
                store.mark_cooldown("paused@test", unix_secs() as i64 + 3600, "limit");
                // 即使还被标记为不可用,冷却依然禁止查询。
                store.mark_unavailable("paused@test", "invalid token");
            }
            refresh_all(store, &client).await;
            crate::account::token_renewal::renew_expiring(store, &client).await;
            for error in [
                refresh_one(store, &client, "paused@test")
                    .await
                    .unwrap_err(),
                refresh_account(store, &client, &snapshot)
                    .await
                    .unwrap_err(),
                fetch_info(store, &client, "paused@test").await.unwrap_err(),
            ] {
                assert!(
                    matches!(error, GatewayError::Admission { code: actual, .. } if actual == code)
                );
            }
            if code == "account_disabled" {
                assert!(matches!(
                    crate::account::service::restore(store, &client, "paused@test").await,
                    Err(GatewayError::Admission {
                        code: "account_disabled",
                        ..
                    })
                ));
            }
            let current = store.get("paused@test").unwrap().unwrap();
            assert_eq!(current.access_token, snapshot.access_token);
            assert_eq!(current.refresh_token, snapshot.refresh_token);
            assert!(current.info_fetched_at.is_none());
        }
        assert_eq!(requests.load(Ordering::SeqCst), 0);

        // 高档仍可用时,低档也不得查询,包括已经取得的旧快照。
        store
            .update_policy("paused@test", Some(true), None)
            .unwrap();
        store.reinstate("paused@test").unwrap();
        let mut high = record.clone();
        high.user = serde_json::json!({"email": "high@test"});
        high.priority = Some(10);
        store.upsert(&high).unwrap();
        for error in [
            refresh_one(store, &client, "paused@test")
                .await
                .unwrap_err(),
            fetch_info(store, &client, "paused@test").await.unwrap_err(),
            crate::account::service::restore(store, &client, "paused@test")
                .await
                .unwrap_err(),
        ] {
            assert!(matches!(
                error,
                GatewayError::Admission {
                    code: "account_standby",
                    ..
                }
            ));
        }
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        refresh_all(store, &client).await;
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "only the high tier may query upstream"
        );

        // 重新启用且冷却已到期时,应恢复正常查询,不能把所有刷新一概跳过。
        store.update_policy("high@test", Some(false), None).unwrap();
        store
            .update_policy("paused@test", Some(true), None)
            .unwrap();
        store.mark_cooldown("paused@test", unix_secs() as i64 - 1, "expired");
        assert!(refresh_one(store, &client, "paused@test").await.is_err());
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        probe.abort();
    }

    fn info(remaining: Option<f64>) -> AccountInfo {
        AccountInfo {
            remaining_credits: remaining,
            ..AccountInfo::default()
        }
    }

    #[test]
    fn expired_trial_is_removed_even_without_pro_credits() {
        let store =
            Arc::new(AccountStore::new(crate::storage::db::Db::open(":memory:").unwrap()).unwrap());
        let record = serde_json::from_value(
            serde_json::json!({"user":{"email":"trial@test"},"device_id":"1111111111111111111111111111111111111111111111111111111111111111","access_token":"rca_trial"}),
        )
        .unwrap();
        store.upsert(&record).unwrap();
        let info = AccountInfo {
            has_pro_features: Some(false),
            remaining_credits: Some(0.0),
            trial_days_left: Some(0),
            ..Default::default()
        };
        apply_decision(&store, "trial@test", &info);
        assert!(!store.get("trial@test").unwrap().unwrap().available);
    }

    #[test]
    fn refresh_errors_only_remove_explicitly_rejected_accounts() {
        let store = AccountStore::new(crate::storage::db::Db::open(":memory:").unwrap()).unwrap();
        let record = serde_json::from_value(serde_json::json!({
            "user": { "email": "refresh@x" },
            "device_id": "1111111111111111111111111111111111111111111111111111111111111111", "access_token": "test-token"
        }))
        .unwrap();
        store.upsert(&record).unwrap();

        for (status, message, unavailable) in [
            (
                403,
                r#"{"error":"Account flagged for suspicious activity, please contact support."}"#,
                true,
            ),
            (403, r#"{"subcode":"account_unavailable"}"#, true),
            (402, "A Raycast Pro subscription is required", true),
            (403, "<html>Forbidden</html>", false),
            (429, "Too many requests", false),
            (503, "Service unavailable", false),
        ] {
            store.set_available("refresh@x", true, None);
            let err = record_refresh_error(
                &store,
                "refresh@x",
                GatewayError::Upstream {
                    status,
                    message: message.into(),
                },
            );
            assert!(matches!(err, GatewayError::Upstream { status: s, .. } if s == status));
            let row = store.get("refresh@x").unwrap().unwrap();
            assert_eq!(row.available, !unavailable, "{status}: {message}");
            if unavailable {
                assert!(row.unavailable_reason.unwrap().contains(message));
                assert!(matches!(
                    store.next_available(),
                    Err(GatewayError::NoAccount(_))
                ));
            }
        }

        record_refresh_error(
            &store,
            "refresh@x",
            GatewayError::Internal("network error".into()),
        );
        assert!(store.get("refresh@x").unwrap().unwrap().available);
        store.mark_unavailable("refresh@x", "existing reason");
        record_refresh_error(
            &store,
            "refresh@x",
            GatewayError::Internal("network error".into()),
        );
        assert!(!store.get("refresh@x").unwrap().unwrap().available);
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
