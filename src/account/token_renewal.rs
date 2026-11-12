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

/// 一次换新的结果。
#[derive(Debug)]
pub enum TokenRenewalOutcome {
    /// 成功:新 access_token + 新 refresh_token(rotation)。
    Renewed {
        access_token: String,
        refresh_token: Option<String>,
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
}

/// 按账号串行续期；锁内重新读库并复用其他请求已经换新的凭据。
pub async fn auto_renew(
    store: &AccountStore,
    client: &crate::raycast::client::RaycastClient,
    account: &AccountRow,
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
    let refresh = current
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GatewayError::TokenInvalid {
            message: "no refresh_token stored".into(),
        })?;
    persist_outcome(
        store,
        &current.email,
        renew_token_call(client, refresh).await,
    )
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
        } => {
            if !store.update_tokens(email, &access_token, refresh_token.as_deref())? {
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
    match tokens.access_token {
        Some(token) if token.starts_with("rca_") => TokenRenewalOutcome::Renewed {
            access_token: token,
            refresh_token: tokens.refresh_token,
        },
        Some(_) => TokenRenewalOutcome::Failed("unexpected token format".into()),
        None => TokenRenewalOutcome::Failed("no access_token in response".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renewal_preserves_error_kinds_and_requires_persistence() {
        use crate::error::GatewayError;
        let store = AccountStore::new(crate::db::Db::open(":memory:").unwrap());
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
                    refresh_token: Some("new".into())
                }
            ),
            Err(GatewayError::Internal(_))
        ));
    }

    #[tokio::test]
    async fn concurrent_stale_snapshots_reuse_rotated_credentials() {
        let state = crate::server::state::compact_test_state();
        let record: crate::account::wire::AccountRecord = serde_json::from_value(serde_json::json!({"user":{"email":"renew@test"},"access_token":"rca_old","refresh_token":"old"})).unwrap();
        state.accounts.upsert(&record).unwrap();
        let old = state.accounts.get("renew@test").unwrap().unwrap();
        let lock = state.accounts.renewal_lock("renew@test");
        let guard = lock.lock().await;
        let a = auto_renew(&state.accounts, &state.client, &old);
        let b = auto_renew(&state.accounts, &state.client, &old);
        state
            .accounts
            .update_tokens("renew@test", "rca_new", Some("new"))
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
