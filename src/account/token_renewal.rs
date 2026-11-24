//! src/account/token_renewal.rs
//! 账号 token 换新:用 refresh_token 换新 access_token(逆向自
//! `scripts/refresh_token.mjs` 的实测协议,也验证于客户端 backend/index.mjs)。
//!
//! 用词约定:本模块管 **token 换新**(renewal,oauth token 轮换);
//! 额度刷新(quota refresh)在 [`crate::account::quota_refresh`]。
//! `refresh_token` 一词保留专指 oauth 凭据本身,不与"刷新"动词混用。
//!
//! 协议要点(2026-09-14 真账号实测):
//!   - POST https://backend.raycast.com/oauth/token,JSON body
//!     `{grant_type:"refresh_token", refresh_token, client_id}`
//!   - refresh_token 一次一换(rotation):响应带新 refresh_token,旧的立即作废
//!   - 换新同时会作废旧 access_token —— 所以调用方必须原子写回,否则账号即死
//!   - `invalid_grant` = refresh_token 已作废(被重新授权过/太久没用),不可恢复

use serde::Deserialize;

use crate::account::store::{AccountRow, AccountStore};
use crate::config::OAUTH_CLIENT_ID;
use crate::util::unix_secs;

/// 只检查本地记录;未知有效期不推测,已知有效期提前一分钟续期。
pub const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const RENEW_BEFORE_SECS: i64 = 60;

#[derive(Clone, Copy)]
enum RenewalReason {
    Unauthorized,
    Expiring,
}

/// 一次换新的结果。
#[derive(Debug)]
pub enum TokenRenewalOutcome {
    /// 成功:新 access_token + 新 refresh_token(rotation)。
    Renewed {
        access_token: String,
        refresh_token: Option<String>,
        expires_at: Option<i64>,
    },
    /// refresh_token 已作废,账号无法自动恢复。
    InvalidGrant,
    /// 其他失败(网络/5xx/响应异常),可重试。
    Failed(String),
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<serde_json::Value>,
    #[serde(default)]
    expires_at: Option<serde_json::Value>,
}

fn timestamp(value: Option<&serde_json::Value>) -> Option<i64> {
    value
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|value| *value >= 0)
}

/// 仅供上游明确返回 401 后调用。必要续期不受启停、冷却和优先级限制。
/// 按账号串行执行,锁内复用其他请求已换新的凭据。
pub async fn auto_renew(
    store: &AccountStore,
    client: &crate::raycast::client::RaycastClient,
    account: &AccountRow,
) -> Result<String, crate::error::GatewayError> {
    renew(store, client, account, RenewalReason::Unauthorized).await
}

fn expiring(account: &AccountRow, now: i64) -> bool {
    account
        .access_token_expires_at
        .is_some_and(|until| until <= now.saturating_add(RENEW_BEFORE_SECS))
        && account
            .refresh_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty())
}

/// 所有账号只按已知有效期进行必要续期,不查额度或验证 /me。
pub async fn renew_expiring(store: &AccountStore, client: &crate::raycast::client::RaycastClient) {
    let accounts = match store.list_all() {
        Ok(accounts) => accounts,
        Err(error) => {
            tracing::warn!(%error, "token expiry scan failed");
            return;
        }
    };
    for account in accounts {
        if expiring(&account, unix_secs() as i64)
            && let Err(error) = renew(store, client, &account, RenewalReason::Expiring).await
        {
            tracing::warn!(email = %account.email, %error, "necessary token renewal failed");
        }
    }
}

async fn renew(
    store: &AccountStore,
    client: &crate::raycast::client::RaycastClient,
    account: &AccountRow,
    reason: RenewalReason,
) -> Result<String, crate::error::GatewayError> {
    use crate::error::GatewayError;
    let lock = store.renewal_lock(&account.email);
    let _guard = lock.lock().await;
    let current = store
        .get(&account.email)?
        .ok_or_else(|| GatewayError::NotFound("account removed during renewal".into()))?;
    if current.access_token != account.access_token {
        return Ok(current.access_token);
    }
    if matches!(reason, RenewalReason::Expiring) && !expiring(&current, unix_secs() as i64) {
        return Ok(current.access_token);
    }
    let refresh = current
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GatewayError::TokenInvalid {
            message: "no refresh_token stored".into(),
        })?;
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(current.access_token.as_bytes());
    digest.update([0]);
    digest.update(refresh.as_bytes());
    let credentials = hex::encode(digest.finalize());
    if !store.claim_renewal(&current.email, &credentials, unix_secs() as i64) {
        if matches!(reason, RenewalReason::Expiring) {
            return Ok(current.access_token);
        }
        return Err(GatewayError::Upstream {
            status: 503,
            message: "token renewal deferred after an earlier failure; replace rejected credentials before retrying".into(),
        });
    }
    let outcome = renew_token_call(client, refresh).await;
    if matches!(&outcome, TokenRenewalOutcome::InvalidGrant) {
        store.reject_renewal(&current.email, &credentials);
    }
    persist_outcome(store, &current.email, outcome)
}

fn persist_outcome(
    store: &AccountStore,
    email: &str,
    outcome: TokenRenewalOutcome,
) -> Result<String, crate::error::GatewayError> {
    use crate::error::GatewayError;
    match outcome {
        TokenRenewalOutcome::Renewed {
            access_token,
            refresh_token,
            expires_at,
        } => {
            if !store.update_tokens(email, &access_token, refresh_token.as_deref(), expires_at)? {
                return Err(GatewayError::Internal(
                    "renewed account disappeared before persistence".into(),
                ));
            }
            Ok(access_token)
        }
        TokenRenewalOutcome::InvalidGrant => Err(GatewayError::TokenInvalid {
            message: "refresh_token invalid_grant".into(),
        }),
        TokenRenewalOutcome::Failed(reason) => Err(GatewayError::Upstream {
            status: 502,
            message: reason,
        }),
    }
}

/// 用 refresh_token 换新 token 对。成功前旧 refresh_token 仍有效,成功后
/// 立即作废 —— 一般应调 [`auto_renew`] 而不是直接用它。
async fn renew_token_call(
    client: &crate::raycast::client::RaycastClient,
    refresh_token: &str,
) -> TokenRenewalOutcome {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": OAUTH_CLIENT_ID,
    });
    let response = match client.oauth_token(&body).await {
        Ok(r) => r,
        Err(e) => return TokenRenewalOutcome::Failed(format!("network: {e}")),
    };

    // 400 + invalid_grant:refresh_token 死了,不可自动恢复。
    if response.status() == reqwest::StatusCode::BAD_REQUEST {
        let parsed: Result<serde_json::Value, _> = response.json().await;
        let err = parsed
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string));
        if err.as_deref() == Some("invalid_grant") {
            return TokenRenewalOutcome::InvalidGrant;
        }
        return TokenRenewalOutcome::Failed(format!("400 {}", err.unwrap_or_default()));
    }
    if !response.status().is_success() {
        return TokenRenewalOutcome::Failed(format!("HTTP {}", response.status()));
    }

    let tokens: OAuthTokenResponse = match response.json().await {
        Ok(t) => t,
        Err(e) => return TokenRenewalOutcome::Failed(format!("bad response body: {e}")),
    };
    let now = unix_secs() as i64;
    let expires_at = timestamp(tokens.expires_at.as_ref()).or_else(|| {
        timestamp(tokens.expires_in.as_ref()).and_then(|seconds| now.checked_add(seconds))
    });
    match tokens.access_token {
        Some(token) if token.starts_with("rca_") => TokenRenewalOutcome::Renewed {
            access_token: token,
            refresh_token: tokens
                .refresh_token
                .filter(|token| !token.trim().is_empty()),
            expires_at,
        },
        Some(_) => TokenRenewalOutcome::Failed("unexpected token format".into()),
        None => TokenRenewalOutcome::Failed("no access_token in response".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    async fn oauth_probe(
        status: axum::http::StatusCode,
        response: serde_json::Value,
    ) -> (
        String,
        Arc<Mutex<Vec<(String, String, serde_json::Value)>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let calls = observed.clone();
            let response = response.clone();
            async move {
                let method = request.method().to_string();
                let path = request.uri().path().to_owned();
                let bytes = axum::body::to_bytes(request.into_body(), 4096)
                    .await
                    .unwrap();
                let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
                calls.lock().unwrap().push((method, path, body));
                (status, axum::Json(response))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, calls, server)
    }

    #[tokio::test]
    async fn only_expiring_tokens_renew_and_paused_account_policy_is_unchanged() {
        let state = crate::server::state::compact_test_state();
        let (url, calls, server) = oauth_probe(
            axum::http::StatusCode::OK,
            serde_json::json!({
                "access_token":"rca_new", "refresh_token":"refresh_new", "expires_in":"3600"
            }),
        )
        .await;
        let mut client = state.dispatcher.client.clone();
        client.http = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(&url).unwrap())
            .build()
            .unwrap();
        client.oauth_url = Some(format!("{url}/oauth/token"));
        let now = unix_secs() as i64;
        for (email, expiry, enabled, priority) in [
            ("disabled@test", Some(now + 30), false, 0),
            ("cooldown@test", Some(now + 30), true, 0),
            ("standby@test", Some(now + 30), true, 0),
            ("unknown@test", None, false, 0),
            ("future@test", Some(now + 7200), false, 0),
            ("high@test", None, true, 10),
        ] {
            let record = serde_json::from_value(serde_json::json!({
                "user":{"email":email}, "device_id":"1111111111111111111111111111111111111111111111111111111111111111",
                "access_token":"rca_old", "refresh_token":"refresh_old",
                "access_token_expires_at":expiry, "enabled":enabled, "priority":priority
            })).unwrap();
            state.accounts.upsert(&record).unwrap();
        }
        state
            .accounts
            .mark_cooldown("cooldown@test", now + 3600, "limit");
        state
            .accounts
            .mark_unavailable("cooldown@test", "quota exhausted");
        let snapshots = state.accounts.list_all().unwrap();
        renew_expiring(&state.accounts, &client).await;
        assert_eq!(calls.lock().unwrap().len(), 3);
        renew_expiring(&state.accounts, &client).await;
        assert_eq!(
            calls.lock().unwrap().len(),
            3,
            "no repeat renewal for fresh or unknown tokens"
        );
        for before in snapshots {
            let after = state.accounts.get(&before.email).unwrap().unwrap();
            assert_eq!(after.enabled, before.enabled);
            assert_eq!(after.priority, before.priority);
            assert_eq!(after.available, before.available);
            assert_eq!(after.cooldown_until, before.cooldown_until);
            assert_eq!(after.unavailable_reason, before.unavailable_reason);
            assert_eq!(after.info_fetched_at, before.info_fetched_at);
            if before.access_token_expires_at == Some(now + 30) {
                assert_eq!(after.access_token, "rca_new");
                assert_eq!(after.refresh_token.as_deref(), Some("refresh_new"));
                assert!(after.access_token_expires_at.unwrap() >= now + 3600);
            } else {
                assert_eq!(after.access_token, before.access_token);
            }
        }
        for (method, path, body) in calls.lock().unwrap().iter() {
            assert_eq!(method, "POST");
            assert_eq!(path, "/oauth/token");
            assert_eq!(body["grant_type"], "refresh_token");
        }
        server.abort();
    }

    #[tokio::test]
    async fn unauthorized_renewal_deduplicates_and_failed_renewal_backs_off() {
        use crate::error::GatewayError;
        let state = crate::server::state::compact_test_state();
        let (url, calls, server) = oauth_probe(
            axum::http::StatusCode::OK,
            serde_json::json!({
                "access_token":"rca_new", "refresh_token":"refresh_new", "expires_in":3600
            }),
        )
        .await;
        let mut client = state.dispatcher.client.clone();
        client.oauth_url = Some(format!("{url}/oauth/token"));
        let record = serde_json::from_value(serde_json::json!({
            "user":{"email":"renew@test"}, "device_id":"1111111111111111111111111111111111111111111111111111111111111111",
            "access_token":"rca_old", "refresh_token":"refresh_old", "enabled":false
        })).unwrap();
        state.accounts.upsert(&record).unwrap();
        let snapshot = state.accounts.get("renew@test").unwrap().unwrap();
        let (a, b) = tokio::join!(
            auto_renew(&state.accounts, &client, &snapshot),
            auto_renew(&state.accounts, &client, &snapshot)
        );
        assert_eq!(a.unwrap(), "rca_new");
        assert_eq!(b.unwrap(), "rca_new");
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert!(!state.accounts.get("renew@test").unwrap().unwrap().enabled);
        server.abort();

        for (status, response) in [
            (
                axum::http::StatusCode::BAD_GATEWAY,
                serde_json::json!({"error":"network"}),
            ),
            (
                axum::http::StatusCode::BAD_REQUEST,
                serde_json::json!({"error":"invalid_grant"}),
            ),
        ] {
            let state = crate::server::state::compact_test_state();
            state.accounts.upsert(&record).unwrap();
            let old = state.accounts.get("renew@test").unwrap().unwrap();
            let (url, calls, server) = oauth_probe(status, response).await;
            client.oauth_url = Some(format!("{url}/oauth/token"));
            assert!(auto_renew(&state.accounts, &client, &old).await.is_err());
            assert!(matches!(
                auto_renew(&state.accounts, &client, &old).await,
                Err(GatewayError::Upstream { status: 503, .. })
            ));
            assert_eq!(calls.lock().unwrap().len(), 1);
            let after = state.accounts.get("renew@test").unwrap().unwrap();
            assert_eq!(after.access_token, old.access_token);
            assert_eq!(after.refresh_token, old.refresh_token);
            server.abort();
        }
    }

    #[test]
    fn renewal_preserves_error_kinds_and_requires_persistence() {
        use crate::error::GatewayError;
        let store = AccountStore::new(crate::storage::db::Db::open(":memory:").unwrap()).unwrap();
        assert!(matches!(
            persist_outcome(&store, "missing", TokenRenewalOutcome::InvalidGrant),
            Err(GatewayError::TokenInvalid { .. })
        ));
        assert!(matches!(
            persist_outcome(
                &store,
                "missing",
                TokenRenewalOutcome::Failed("network".into())
            ),
            Err(GatewayError::Upstream { status: 502, .. })
        ));
        assert!(matches!(
            persist_outcome(
                &store,
                "missing",
                TokenRenewalOutcome::Renewed {
                    access_token: "rca_new".into(),
                    refresh_token: Some("new".into()),
                    expires_at: None,
                }
            ),
            Err(GatewayError::Internal(_))
        ));
    }

    #[tokio::test]
    async fn concurrent_stale_snapshots_reuse_rotated_credentials() {
        let state = crate::server::state::compact_test_state();
        let record: crate::account::wire::AccountRecord = serde_json::from_value(serde_json::json!({"user":{"email":"renew@test"},"device_id":"1111111111111111111111111111111111111111111111111111111111111111","access_token":"rca_old","refresh_token":"old"})).unwrap();
        state.accounts.upsert(&record).unwrap();
        let old = state.accounts.get("renew@test").unwrap().unwrap();
        let lock = state.accounts.renewal_lock("renew@test");
        let guard = lock.lock().await;
        let a = auto_renew(&state.accounts, &state.dispatcher.client, &old);
        let b = auto_renew(&state.accounts, &state.dispatcher.client, &old);
        state
            .accounts
            .update_tokens("renew@test", "rca_new", Some("new"), None)
            .unwrap();
        drop(guard);
        let (a, b) = tokio::join!(a, b);
        assert_eq!(a.unwrap(), "rca_new");
        assert_eq!(b.unwrap(), "rca_new");
    }

    #[test]
    fn outcome_is_debug() {
        // 简单保证枚举可打印(日志用)。
        let o = TokenRenewalOutcome::Failed("x".into());
        assert!(format!("{o:?}").contains("x"));
    }
}
