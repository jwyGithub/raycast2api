//! Async client for the Raycast AI backend — transport only.
//!
//! Builds signed requests (see [`crate::signature`]) and talks to the upstream
//! endpoints with an *explicit* token. Which account to use, rotation,
//! cooldowns and token renewal belong to [`crate::account::dispatcher`].

use std::sync::Arc;

use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::config::{
    RAYCAST_AI_CREDITS_DETAILS_URL, RAYCAST_AI_CREDITS_URL, RAYCAST_API_URL, RAYCAST_ME_URL,
    RAYCAST_MODELS_URL, RAYCAST_TRIAL_STATUS_URL, Settings, USER_AGENT, USER_AGENT_POST,
};
use crate::error::GatewayError;
use crate::signature::calculate_signature_v2;
use crate::statistics::{Attempt, CallContext, store::StatisticsStore};
use crate::util::unix_secs;

use super::parser::content_policy_hint;
use super::wire::{
    RaycastCredits, RaycastCreditsDetails, RaycastMe, RaycastModelsResponse, RaycastRawModel,
    RaycastSseError, RaycastTrialStatus,
};

/// Cheap-to-clone handle to the Raycast backend (wraps a shared `reqwest::Client`).
#[derive(Clone)]
pub struct RaycastClient {
    pub(crate) http: Client,
    /// 代理客户端链,与 `settings.proxies` 一一对应;通道顺序由 `proxies_mode` 决定。
    pub(super) proxies: Vec<(String, Client)>,
    pub(crate) settings: Arc<Settings>,
    pub(crate) statistics: Arc<StatisticsStore>,
    #[cfg(test)]
    pub(crate) chat_url: Option<String>,
    #[cfg(test)]
    pub(crate) models_url: Option<String>,
}

impl RaycastClient {
    pub fn new(http: Client, settings: Arc<Settings>, statistics: Arc<StatisticsStore>) -> Self {
        let proxies = crate::proxies::build_clients(&settings.proxies, settings.upstream_timeout());
        Self {
            http,
            proxies,
            settings,
            statistics,
            #[cfg(test)]
            chat_url: None,
            #[cfg(test)]
            models_url: None,
        }
    }

    /// Common headers shared by every request. `signed` selects the POST
    /// header set the real client sends: it swaps in the "x-Windows Version"
    /// UA variant, attaches the DeviceId, and — on signed requests — the
    /// timestamp/signature pair plus the `autoModels` experimental flag. GETs
    /// in the real client carry *no* DeviceId and no signature pair.
    fn base_headers(&self, bearer: &str, signed: Option<(&str, &str)>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let bearer = format!("Bearer {bearer}");
        // These values come from config/HMAC output and are always valid header text.
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&bearer).expect("valid bearer header"),
        );
        headers.insert(
            "User-Agent",
            // The real client uses the `x-Windows Version` UA variant on
            // signed POSTs and `x-Windows-amd64` on GETs (captures 2026-11).
            HeaderValue::from_static(if signed.is_some() {
                USER_AGENT_POST
            } else {
                USER_AGENT
            }),
        );
        if let Some((timestamp, signature)) = signed {
            headers.insert(
                "X-Raycast-DeviceId",
                HeaderValue::from_str(&self.settings.raycast_device_id).expect("valid device id"),
            );
            headers.insert(
                "X-Raycast-Timestamp",
                HeaderValue::from_str(timestamp).expect("valid timestamp"),
            );
            headers.insert(
                "X-Raycast-Signature-v2",
                HeaderValue::from_str(signature).expect("valid signature"),
            );
            headers.insert(
                "X-Raycast-Experimental",
                HeaderValue::from_static("autoModels"),
            );
        }
        headers
    }

    /// One signed chat POST with an explicit account (`email` is only used to
    /// label statistics). Status is validated; account rotation, token renewal
    /// and in-stream error handling live in [`crate::account::dispatcher`].
    pub(crate) async fn chat(
        &self,
        email: &str,
        token: &str,
        body: &str,
        call: &CallContext,
        reserved: Option<Attempt>,
    ) -> Result<reqwest::Response, GatewayError> {
        let timestamp = unix_secs().to_string();
        let signature = calculate_signature_v2(
            &timestamp,
            &self.settings.raycast_device_id,
            body,
            &self.settings.raycast_signature_secret,
        );

        let mut headers = self.base_headers(token, Some((&timestamp, &signature)));
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        // The real client sends `application/json` even though the response is
        // SSE (capture 2026-11-20); `text/event-stream` is a fingerprint.
        headers.insert("Accept", HeaderValue::from_static("application/json"));

        #[cfg(test)]
        let url = self.chat_url.as_deref().unwrap_or(RAYCAST_API_URL);
        #[cfg(not(test))]
        let url = RAYCAST_API_URL;
        let request = self
            .http
            .post(url)
            .headers(headers)
            .body(body.to_owned())
            .build()?;
        let response = self.send(request, Some((call, email)), reserved).await?;
        let attempt = response.extensions().get::<Attempt>().cloned();
        let result = ensure_success(response).await;
        if let (Err(error), Some(attempt)) = (&result, attempt) {
            attempt.fail(error);
        }
        result
    }

    /// Fetch the raw list of models available to the account owning `token`.
    pub async fn fetch_models(&self, token: &str) -> Result<Vec<RaycastRawModel>, GatewayError> {
        #[cfg(test)]
        let url = self.models_url.as_deref().unwrap_or(RAYCAST_MODELS_URL);
        #[cfg(not(test))]
        let url = RAYCAST_MODELS_URL;
        let parsed: RaycastModelsResponse = self.get_json(url, token).await?;
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
        let mut headers = self.base_headers(token, None);
        headers.insert("Accept", HeaderValue::from_static("application/json"));
        let request = self
            .http
            .get(url)
            .headers(headers)
            .timeout(self.settings.upstream_timeout())
            .build()?;
        let response = self.send(request, None, None).await?;
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
    let attempt = response.extensions().get::<Attempt>().cloned();
    let mut message = response.text().await.unwrap_or_default();
    if let Some(attempt) = attempt {
        attempt.observe_payload(&message);
    }
    // 内容策略拦截可能以 HTTP 错误(而非流内 payload)形式出现:同样附上
    // 排查提示,并识别为"非账号故障",让上层不摘号不轮换。
    let hint = RaycastSseError {
        message: Some(message.clone()),
        code: None,
        category: None,
        subcode: None,
        http_status: status,
        retry_after: None,
    };
    if message.contains("content_policy_blocked")
        || message.contains("refusal_blocked")
        || hint.is_content_policy_blocked()
    {
        message.push_str(content_policy_hint());
    }
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
