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
use crate::config::{OAUTH_CLIENT_ID, OAUTH_TOKEN_URL};

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

/// 换新并落盘:换新 token 对后立即原子写回账号存储(rotation 要求,
/// 不落盘 = 账号即死)。这是所有 401 自动恢复路径的唯一入口。
///
/// 返回新 access_token 供调用方立即重试;`Err(())` 表示换新失败或
/// 落盘失败(原因已记日志)。
pub async fn renew_and_persist(
    http: &reqwest::Client,
    store: &AccountStore,
    email: &str,
    refresh_token: &str,
) -> Result<String, ()> {
    match renew_token_call(http, refresh_token).await {
        TokenRenewalOutcome::Renewed {
            access_token,
            refresh_token: new_refresh,
        } => store
            .update_tokens(email, &access_token, new_refresh.as_deref())
            .map(|_| {
                tracing::info!(account = %email, "→ token renewed and persisted");
                access_token
            })
            .map_err(|err| {
                tracing::error!(
                    account = %email,
                    "→ CRITICAL: renewed token but failed to persist: {err}"
                );
            }),
        TokenRenewalOutcome::InvalidGrant => {
            tracing::warn!(account = %email, "→ refresh_token invalid_grant, account dead");
            Err(())
        }
        TokenRenewalOutcome::Failed(reason) => {
            tracing::warn!(account = %email, "→ token renewal failed: {reason}");
            Err(())
        }
    }
}

/// 账号没有可用的 refresh_token。集中判断,调用方据此决定走哪条恢复路径。
pub fn extract_refresh_token(account: &AccountRow) -> Option<String> {
    account
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// 401 后的自动续命:账号有 refresh_token 就换新并原子写回。
/// 复用调用方传入的 HTTP 客户端(与主链路同一代理配置)。
///
/// `Ok(true)` = 换新成功且已落盘;`Ok(false)` = 没有 refresh_token,跳过;
/// `Err(())` = 换新失败(invalid_grant 或网络),账号不可自动恢复。
pub async fn auto_renew(
    store: &AccountStore,
    http: &reqwest::Client,
    account: &AccountRow,
) -> Result<bool, ()> {
    let Some(refresh_token) = extract_refresh_token(account) else {
        tracing::warn!(
            "account renewal: {} no refresh_token, cannot auto-renew",
            account.email
        );
        return Ok(false);
    };
    renew_and_persist(http, store, &account.email, &refresh_token)
        .await
        .map(|_| true)
}

/// 用 refresh_token 换新 token 对。成功前旧 refresh_token 仍有效,成功后
/// 立即作废 —— 一般应调 [`renew_and_persist`] / [`auto_renew`] 而不是直接用它。
async fn renew_token_call(http: &reqwest::Client, refresh_token: &str) -> TokenRenewalOutcome {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": OAUTH_CLIENT_ID,
    });
    let response = match http
        .post(OAUTH_TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
    {
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
        Some(token) => TokenRenewalOutcome::Failed(format!("unexpected token format: {token}")),
        None => TokenRenewalOutcome::Failed("no access_token in response".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_is_debug() {
        // 简单保证枚举可打印(日志用)。
        let o = TokenRenewalOutcome::Failed("x".into());
        assert!(format!("{o:?}").contains("x"));
    }
}
