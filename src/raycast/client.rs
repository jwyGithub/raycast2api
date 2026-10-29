//! Async client for the Raycast AI backend.
//!
//! Builds signed requests (see [`crate::signature`]) and talks to the two
//! endpoints the gateway needs: streaming chat completions and the model list.

use std::sync::Arc;

use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::account::store::{AccountRow, AccountStore};
use crate::config::{
    DEFAULT_COOLDOWN_SECS, MAX_COOLDOWN_SECS, MIN_COOLDOWN_SECS, RAYCAST_AI_CREDITS_DETAILS_URL,
    RAYCAST_AI_CREDITS_URL, RAYCAST_API_URL, RAYCAST_ME_URL, RAYCAST_MODELS_URL,
    RAYCAST_TRIAL_STATUS_URL, Settings, USER_AGENT,
};
use crate::debug;
use crate::error::GatewayError;
use crate::log::store::RequestLogStore;
use crate::signature::calculate_signature_v2;
use crate::util::unix_secs;

use super::peek::{peek_first_payload, restore_payload};
use super::wire::{
    RaycastChatRequest, RaycastCredits, RaycastCreditsDetails, RaycastMe, RaycastModelsResponse,
    RaycastRawModel, RaycastTrialStatus,
};

/// Cheap-to-clone handle to the Raycast backend (wraps a shared `reqwest::Client`).
#[derive(Clone)]
pub struct RaycastClient {
    http: Client,
    /// 代理客户端链,与 `settings.proxies` 一一对应;直连失败时按顺序换用。
    proxies: Vec<(String, Client)>,
    settings: Arc<Settings>,
    /// 账号池:每次请求轮询取一个可用账号的 token。
    accounts: Arc<AccountStore>,
    /// 请求日志存储:补写发给上游的请求体。
    log: Arc<RequestLogStore>,
}

impl RaycastClient {
    pub fn new(
        http: Client,
        settings: Arc<Settings>,
        accounts: Arc<AccountStore>,
        log: Arc<RequestLogStore>,
    ) -> Self {
        // 每个代理一个共享客户端(连接池复用);构造失败视为配置错误,启动即崩。
        let proxies = settings
            .proxies
            .iter()
            .map(|url| {
                let proxy = reqwest::Proxy::all(url)
                    .unwrap_or_else(|e| panic!("invalid proxy '{url}': {e}"));
                let client = Client::builder()
                    .proxy(proxy)
                    .build()
                    .unwrap_or_else(|e| panic!("failed to build proxy client '{url}': {e}"));
                (url.clone(), client)
            })
            .collect();
        Self {
            http,
            proxies,
            settings,
            accounts,
            log,
        }
    }

    /// 账号池(刷新任务与测试使用)。
    pub fn accounts(&self) -> &Arc<AccountStore> {
        &self.accounts
    }

    /// 共享 HTTP 客户端(含代理配置):供 token 自动刷新等复用主链路通道。
    pub fn http(&self) -> &Client {
        &self.http
    }

    /// Common headers shared by every request. When `signature` is `Some`, the
    /// timestamp/signature pair (present only for signed POSTs) is included.
    fn base_headers(
        &self,
        bearer: &str,
        timestamp: Option<&str>,
        signature: Option<&str>,
    ) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let bearer = format!("Bearer {bearer}");
        // These values come from config/HMAC output and are always valid header text.
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&bearer).expect("valid bearer header"),
        );
        headers.insert(
            "X-Raycast-DeviceId",
            HeaderValue::from_str(&self.settings.raycast_device_id).expect("valid device id"),
        );
        headers.insert("User-Agent", HeaderValue::from_static(USER_AGENT));
        if let (Some(ts), Some(sig)) = (timestamp, signature) {
            headers.insert(
                "X-Raycast-Timestamp",
                HeaderValue::from_str(ts).expect("valid timestamp"),
            );
            headers.insert(
                "X-Raycast-Signature-v2",
                HeaderValue::from_str(sig).expect("valid signature"),
            );
        }
        headers
    }

    /// POST a chat request. Returns the streaming response on success (status
    /// already validated); the caller consumes `bytes_stream()`.
    ///
    /// 账号故障切换:401 摘号换下一个重试;200 但流内首 payload 是限额错误
    /// 时给账号设冷却期(取上游 `retry_after`)再换号重试。池内全部不可用
    /// 时把最后一个真实错误透传给客户端。
    pub async fn send_chat(
        &self,
        request: &RaycastChatRequest,
    ) -> Result<reqwest::Response, GatewayError> {
        let body = serde_json::to_string(request)?;
        let attempts = self.accounts.count_available().max(1);
        let mut last_error: Option<GatewayError> = None;

        for _ in 0..attempts {
            let account = self.accounts.next_available()?;
            tracing::info!(account = %account.email, tag = ?account.tag, "→ using account");
            match self.try_chat_with_fallback(&account, &body).await {
                Ok(response) => {
                    // 上游部分错误(限额)藏在 200 SSE 流内:偷看首 payload,
                    // 是限额错误则冷却该账号并换号重试,否则把流原样还给调用方。
                    let (peeked, response) = peek_first_payload(response).await;
                    if let Some(err) = peeked {
                        let message = err.message();
                        let status = if err.parsed.http_status != 0 {
                            err.parsed.http_status
                        } else {
                            502
                        };
                        if err.parsed.is_usage_limit() {
                            // 限额:冷却该账号(到期自动恢复进池)并换号重试。
                            let cooldown = err
                                .parsed
                                .retry_after
                                .unwrap_or(DEFAULT_COOLDOWN_SECS)
                                .clamp(MIN_COOLDOWN_SECS, MAX_COOLDOWN_SECS);
                            let until = unix_secs() as i64 + cooldown as i64;
                            tracing::warn!(
                                account = %account.email,
                                cooldown_secs = cooldown,
                                "→ account rate-limited, cooling down and rotating: {message}"
                            );
                            self.accounts.mark_cooldown(
                                &account.email,
                                until,
                                &format!("限额冷却 {}s ({message})", cooldown),
                            );
                            last_error = Some(GatewayError::Upstream { status, message });
                            continue;
                        }
                        if err.parsed.is_unconfirmed() {
                            // 邮箱未确认:不摘号(token 有效),但冷却 24h ——
                            // 用户去确认邮箱后,下次手动/定时刷新会恢复可用。
                            tracing::warn!(
                                account = %account.email,
                                "→ email not confirmed, cooling down 24h and rotating: {message}"
                            );
                            self.accounts.mark_cooldown(
                                &account.email,
                                unix_secs() as i64 + 24 * 60 * 60,
                                &format!("邮箱未确认 ({message})"),
                            );
                            last_error = Some(GatewayError::Upstream { status, message });
                            continue;
                        }
                        tracing::warn!(
                            account = %account.email,
                            "→ upstream in-stream error (not rate-limit): {message}"
                        );
                        // 其他流内错误:还原 payload,交由解析层上报。
                        return Ok(restore_payload(response, &err.raw));
                    }
                    return Ok(response);
                }
                // token 失效:有 refresh_token 就自动刷新重试,
                // 刷新失败(或没有 refresh_token)才摘号换下一个。
                Err(GatewayError::TokenInvalid { .. }) => {
                    match self.try_refresh_account(&account).await {
                        Ok(new_token) => {
                            // 刷新成功:DB 里 token 已更新,本账号立即用新 token 重试。
                            let mut fresh = account.clone();
                            fresh.access_token = new_token;
                            tracing::info!(account = %account.email, "→ token refreshed, retrying with new token");
                            match self.try_chat_with_fallback(&fresh, &body).await {
                                Ok(response) => return Ok(response),
                                Err(err @ GatewayError::TokenInvalid { .. }) => {
                                    // 新 token 也失效:账号本身有问题,摘号。
                                    tracing::warn!(account = %account.email, "→ refreshed token still invalid, removing account");
                                    self.accounts.mark_unavailable(
                                        &account.email,
                                        "刷新后 token 仍失效 (401)",
                                    );
                                    last_error = Some(err);
                                }
                                Err(err) => {
                                    tracing::error!(account = %account.email, "→ upstream request failed after refresh: {err}");
                                    return Err(err);
                                }
                            }
                        }
                        Err(err @ GatewayError::TokenInvalid { .. }) => {
                            tracing::warn!(account = %account.email, "→ token invalid, no refresh possible, removing account and rotating");
                            self.accounts
                                .mark_unavailable(&account.email, "token 失效 (401)");
                            last_error = Some(err);
                        }
                        Err(err) => {
                            tracing::error!(account = %account.email, "→ upstream request failed: {err}");
                            return Err(err);
                        }
                    }
                }
                Err(err) => {
                    tracing::error!(account = %account.email, "→ upstream request failed: {err}");
                    return Err(err);
                }
            }
        }

        // 池内账号全部 401 / 限额:透传最后一个真实错误。
        tracing::error!(
            attempts,
            "all available accounts failed; surfacing last error to client"
        );
        Err(last_error.unwrap_or(GatewayError::NoAccount(
            "no available Raycast account; add or refresh one via POST /v1/accounts".into(),
        )))
    }

    /// 401 自动恢复:用账号的 refresh_token 换新 token 并原子写回 DB。
    /// 返回 `Ok(new_token)` 表示已刷新且入库,调用方可立即用新 token 重试;
    /// `Err(GatewayError::TokenInvalid{..})` = 没有 refresh_token 或已作废
    /// (`invalid_grant`,账号只能重新走注册授权);`Err(其他)` = 落盘/网络异常。
    async fn try_refresh_account(&self, account: &AccountRow) -> Result<String, GatewayError> {
        let Some(refresh_token) = crate::account::token_renewal::extract_refresh_token(account)
        else {
            return Err(GatewayError::TokenInvalid {
                message: "no refresh_token stored for this account".into(),
            });
        };
        crate::account::token_renewal::renew_and_persist(
            &self.http,
            &self.accounts,
            &account.email,
            &refresh_token,
        )
        .await
        .map_err(|()| {
            // 刷新失败的细分原因已由 renew_and_persist 记日志;这里统一按
            // token 失效语义上报,让 send_chat 走摘号换号路径。
            GatewayError::TokenInvalid {
                message: "refresh_token is no longer valid (invalid_grant)".into(),
            }
        })
    }

    /// 直连 + 代理回退的上游请求:先直连,网络层失败(连接不上/超时,即
    /// `GatewayError::Internal`)时按配置顺序逐个换代理重试。代理失败也继续
    /// 换下一个;所有代理都失败才把最后的网络错误返回。HTTP 4xx/5xx 不触发
    /// 代理回退(那是上游的业务响应,换通道没用)。
    async fn try_chat_with_fallback(
        &self,
        account: &AccountRow,
        body: &str,
    ) -> Result<reqwest::Response, GatewayError> {
        let mut last_err = match self.try_chat(&self.http, account, body).await {
            Ok(response) => return Ok(response),
            Err(err @ GatewayError::Internal(_)) if !self.proxies.is_empty() => err,
            Err(err) => return Err(err),
        };

        for (url, client) in &self.proxies {
            tracing::warn!(
                proxy = %url,
                account = %account.email,
                "→ direct connection failed, retrying via proxy"
            );
            match self.try_chat(client, account, body).await {
                Ok(response) => return Ok(response),
                Err(err @ GatewayError::Internal(_)) => {
                    tracing::warn!(proxy = %url, "→ proxy request failed: {err}");
                    last_err = err;
                }
                // 经代理拿到了上游响应(含 HTTP 错误状态):比网络错误更有价值,直接返回。
                Err(err) => return Err(err),
            }
        }
        Err(last_err)
    }

    /// 单次上游请求(签名 + 发送 + 状态校验),供 [`Self::send_chat`] 轮换调用。
    async fn try_chat(
        &self,
        http: &Client,
        account: &AccountRow,
        body: &str,
    ) -> Result<reqwest::Response, GatewayError> {
        let timestamp = unix_secs().to_string();
        let signature = calculate_signature_v2(
            &timestamp,
            &self.settings.raycast_device_id,
            body,
            self.settings.signature_secret(),
        );

        let mut headers =
            self.base_headers(&account.access_token, Some(&timestamp), Some(&signature));
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        headers.insert("Accept", HeaderValue::from_static("text/event-stream"));

        // Record the outgoing upstream body in the request-log store,
        // correlated to the inbound request via the task-local id set by
        // `debug_capture`. Fire-and-forget.
        self.log.update_upstream(
            &debug::current_request_id(),
            &debug::body_to_value(body.as_bytes()),
        );

        let response = http
            .post(RAYCAST_API_URL)
            .headers(headers)
            .body(body.to_owned())
            .send()
            .await?;

        ensure_success(response).await
    }

    /// Fetch the raw list of models available to the account.
    pub async fn fetch_models(&self) -> Result<Vec<RaycastRawModel>, GatewayError> {
        let account = self.accounts.next_available()?;
        let mut headers = self.base_headers(&account.access_token, None, None);
        headers.insert("Accept", HeaderValue::from_static("application/json"));

        let response = self
            .http
            .get(RAYCAST_MODELS_URL)
            .headers(headers)
            .send()
            .await?;

        let response = ensure_success(response).await?;
        let parsed: RaycastModelsResponse = response.json().await?;
        Ok(parsed.models)
    }

    /// Fetch the account profile + subscription for an explicit token.
    pub async fn fetch_me(&self, token: &str) -> Result<RaycastMe, GatewayError> {
        self.get_json(RAYCAST_ME_URL, token).await
    }

    /// Fetch the AI credit balance for an explicit token.
    pub async fn fetch_credits(&self, token: &str) -> Result<RaycastCredits, GatewayError> {
        self.get_json(RAYCAST_AI_CREDITS_URL, token).await
    }

    /// Fetch credit usage details (buckets + grants) for a time range
    /// (`today` | `7d` | `30d`,同客户端设置页)。
    pub async fn fetch_credits_details(
        &self,
        token: &str,
        range: &str,
    ) -> Result<RaycastCreditsDetails, GatewayError> {
        let url = format!(
            "{RAYCAST_AI_CREDITS_DETAILS_URL}?range={range}&tz={}",
            encode_query_value(&tz_name())
        );
        self.get_json(&url, token).await
    }

    /// Fetch trial status (limits + organizations). 试用号专用;Pro 号可能
    /// 404,由调用方按错误分类处理。
    pub async fn fetch_trial_status(
        &self,
        token: &str,
    ) -> Result<RaycastTrialStatus, GatewayError> {
        self.get_json(RAYCAST_TRIAL_STATUS_URL, token).await
    }

    /// Shared bearer-only GET returning parsed JSON.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        token: &str,
    ) -> Result<T, GatewayError> {
        let mut headers = self.base_headers(token, None, None);
        headers.insert("Accept", HeaderValue::from_static("application/json"));
        let response = self.http.get(url).headers(headers).send().await?;
        let response = ensure_success(response).await?;
        Ok(response.json().await?)
    }
}

/// Turn a non-2xx upstream response into a [`GatewayError::Upstream`]
/// (401 gets the dedicated [`GatewayError::TokenInvalid`] variant).
async fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response, GatewayError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let message = response.text().await.unwrap_or_default();
    if status == 401 {
        return Err(GatewayError::TokenInvalid { message });
    }
    Err(GatewayError::Upstream { status, message })
}

/// 时区名(IANA,如 `Asia/Shanghai`),与客户端一致;取不到回退 UTC。
fn tz_name() -> String {
    // `TZ` 环境变量是容器/服务器场景下最可靠的信号;Windows 上通常没设,
    // 直接用 UTC(上游只拿它做展示对齐,不影响数值)。
    std::env::var("TZ").unwrap_or_else(|_| "UTC".into())
}

/// 最小 query 值转义:保留字母数字与 `-._~`,其余转 `%XX`。
fn encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_clamps_to_configured_bounds() {
        // 上游给的 retry_after 会被夹到配置的上下限内。
        assert_eq!(999.clamp(MIN_COOLDOWN_SECS, MAX_COOLDOWN_SECS), 999);
        assert_eq!(
            5.clamp(MIN_COOLDOWN_SECS, MAX_COOLDOWN_SECS),
            MIN_COOLDOWN_SECS
        );
        assert_eq!(
            (30 * 60 * 60).clamp(MIN_COOLDOWN_SECS, MAX_COOLDOWN_SECS),
            MAX_COOLDOWN_SECS
        );
    }
}
