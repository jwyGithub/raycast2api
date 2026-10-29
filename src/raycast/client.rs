//! Async client for the Raycast AI backend.
//!
//! Builds signed requests (see [`crate::signature`]) and talks to the two
//! endpoints the gateway needs: streaming chat completions and the model list.

use std::sync::Arc;

use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::account::store::{AccountRow, AccountStore};
use crate::config::{
    DEFAULT_COOLDOWN_SECS, MAX_COOLDOWN_SECS, MIN_COOLDOWN_SECS, RAYCAST_AI_CREDITS_URL,
    RAYCAST_API_URL, RAYCAST_ME_URL, RAYCAST_MODELS_URL, Settings, USER_AGENT,
};
use crate::debug;
use crate::error::GatewayError;
use crate::log::store::RequestLogStore;
use crate::signature::calculate_signature_v2;
use crate::util::unix_secs;

use super::peek::{peek_first_payload, restore_payload};
use super::wire::{
    RaycastChatRequest, RaycastCredits, RaycastMe, RaycastModelsResponse, RaycastRawModel,
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
                let client = Client::builder().proxy(proxy).build().unwrap_or_else(|e| {
                    panic!("failed to build proxy client '{url}': {e}")
                });
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
                        if err.parsed.is_usage_limit() {
                            let cooldown = err
                                .parsed
                                .retry_after
                                .unwrap_or(DEFAULT_COOLDOWN_SECS)
                                .clamp(MIN_COOLDOWN_SECS, MAX_COOLDOWN_SECS);
                            let until = unix_secs() as i64 + cooldown as i64;
                            let message = err.message();
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
                            last_error = Some(GatewayError::Upstream {
                                status: if err.parsed.http_status != 0 {
                                    err.parsed.http_status
                                } else {
                                    429
                                },
                                message,
                            });
                            continue;
                        }
                        tracing::warn!(
                            account = %account.email,
                            "→ upstream in-stream error (not rate-limit): {}",
                            err.message()
                        );
                        // 非限额的流内错误:还原 payload,交由解析层上报。
                        return Ok(restore_payload(response, &err.raw));
                    }
                    return Ok(response);
                }
                // 401 = 该账号 token 失效:摘除后换号重试。
                Err(err @ GatewayError::Upstream { status: 401, .. }) => {
                    tracing::warn!(account = %account.email, "→ token invalid (401), removing account and rotating");
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

        // 池内账号全部 401 / 限额:透传最后一个真实错误。
        tracing::error!(attempts, "all available accounts failed; surfacing last error to client");
        Err(last_error
            .unwrap_or(GatewayError::NoAccount(
                "no available Raycast account; add or refresh one via POST /v1/accounts".into(),
            )))
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
        self.log
            .update_upstream(&debug::current_request_id(), &debug::body_to_value(body.as_bytes()));

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

/// Turn a non-2xx upstream response into a [`GatewayError::Upstream`].
async fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response, GatewayError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let message = response.text().await.unwrap_or_default();
    Err(GatewayError::Upstream { status, message })
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
