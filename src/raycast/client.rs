//! Async client for the Raycast AI backend.
//!
//! Builds signed requests (see [`crate::signature`]) and talks to the two
//! endpoints the gateway needs: streaming chat completions and the model list.

use std::sync::Arc;

use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::account::store::AccountStore;
use crate::config::{RAYCAST_API_URL, RAYCAST_MODELS_URL, Settings, USER_AGENT};
use crate::debug;
use crate::error::GatewayError;
use crate::signature::calculate_signature_v2;
use crate::util::unix_secs;

use super::wire::{RaycastChatRequest, RaycastModelsResponse, RaycastRawModel};

/// Cheap-to-clone handle to the Raycast backend (wraps a shared `reqwest::Client`).
#[derive(Clone)]
pub struct RaycastClient {
    http: Client,
    settings: Arc<Settings>,
    /// 账号池:每次请求轮询取一个可用账号的 token。
    accounts: Arc<AccountStore>,
}

impl RaycastClient {
    pub fn new(http: Client, settings: Arc<Settings>, accounts: Arc<AccountStore>) -> Self {
        Self {
            http,
            settings,
            accounts,
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
    pub async fn send_chat(
        &self,
        request: &RaycastChatRequest,
    ) -> Result<reqwest::Response, GatewayError> {
        let account = self.accounts.next_available()?;
        let body = serde_json::to_string(request)?;
        let timestamp = unix_secs().to_string();
        let signature = calculate_signature_v2(
            &timestamp,
            &self.settings.raycast_device_id,
            &body,
            self.settings.signature_secret(),
        );

        let mut headers =
            self.base_headers(&account.access_token, Some(&timestamp), Some(&signature));
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        headers.insert("Accept", HeaderValue::from_static("text/event-stream"));

        // Dump the outgoing upstream body for offline analysis, enabled by
        // DEBUG_DUMP_DIR and correlated to the inbound request via the
        // task-local id set by `debug_capture`.
        let body_value = debug::body_to_value(body.as_bytes());
        if let Ok(text) = serde_json::to_string_pretty(&body_value) {
            debug::dump(
                &format!("{}-upstream.json", debug::current_request_id()),
                &text,
            );
        }

        let response = self
            .http
            .post(RAYCAST_API_URL)
            .headers(headers)
            .body(body)
            .send()
            .await?;

        match ensure_success(response).await {
            Ok(response) => Ok(response),
            // 401 = 该账号 token 失效:摘除后返回错误,下次请求自动换号。
            Err(err @ GatewayError::Upstream { status: 401, .. }) => {
                self.accounts
                    .mark_unavailable(&account.email, "token 失效 (401)");
                Err(err)
            }
            Err(err) => Err(err),
        }
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
