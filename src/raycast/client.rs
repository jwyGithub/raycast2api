//! Async client for the Raycast AI backend — transport only.
//!
//! Builds signed requests (see [`crate::signature`]) and talks to the upstream
//! endpoints with an *explicit* token. Which account to use, rotation,
//! cooldowns and token renewal belong to [`crate::account::dispatcher`].

use std::sync::Arc;

use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::config::{
    RAYCAST_AI_CREDITS_DETAILS_URL, RAYCAST_AI_CREDITS_URL, RAYCAST_API_URL, RAYCAST_FILES_URL,
    RAYCAST_ME_URL, RAYCAST_MODELS_URL, RAYCAST_TRIAL_STATUS_URL, Settings, USER_AGENT,
    USER_AGENT_POST,
};
use crate::error::GatewayError;
use crate::log::store::RequestLogStore;
use crate::signature::calculate_signature_v2;
use crate::statistics::{Attempt, CallContext, store::StatisticsStore};
use crate::util::unix_secs;

use super::parser::content_policy_hint;
use super::wire::{
    FileRegistration, RaycastCredits, RaycastCreditsDetails, RaycastMe, RaycastModelsResponse,
    RaycastRawModel, RaycastSseError, RaycastTrialStatus,
};

/// 真实客户端的两条出站 HTTP 管道(逆向 `backend/index.mjs` + 内嵌 Node 实测,
/// 2026-11-23):两者对 User-Agent 变体和自动头(`accept-encoding` 等)的处理
/// 不同,请求头必须按管道对齐。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// AI 对话(`POST /ai/chat_completions`):内嵌 Node 的 undici `fetch`。
    /// UA 用 `x-Windows Version` 变体;undici 自动补 `accept-language: *`、
    /// `sec-fetch-mode: cors`、`accept-encoding: br, gzip, deflate, zstd`。
    ChatFetch,
    /// 其余 API(got 实例,`prefixUrl .../api/v1`):/me、/ai/credits、
    /// models、`ai/files` 注册等。UA 用 `x-Windows-amd64` 变体;got 自动补
    /// `accept-encoding: gzip, deflate, br, zstd`(brotli/zstd 能力开关按
    /// Node 版本),不补 accept-language / sec-fetch-mode。
    Got,
}

impl Transport {
    /// got/undici 自动写入的 `accept-encoding`(实测,顺序即发包顺序)。
    fn accept_encoding(self) -> &'static str {
        match self {
            Self::ChatFetch => "br, gzip, deflate, zstd",
            Self::Got => "gzip, deflate, br, zstd",
        }
    }
}

/// Cheap-to-clone handle to the Raycast backend (wraps a shared `reqwest::Client`).
#[derive(Clone)]
pub struct RaycastClient {
    pub(crate) http: Client,
    /// 代理客户端链,与 `settings.proxies` 一一对应;通道顺序由 `proxies_mode` 决定。
    pub(super) proxies: Vec<(String, Client)>,
    pub(crate) settings: Arc<Settings>,
    pub(crate) statistics: Arc<StatisticsStore>,
    pub(crate) log: Option<Arc<RequestLogStore>>,
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
            log: None,
            #[cfg(test)]
            chat_url: None,
            #[cfg(test)]
            models_url: None,
        }
    }

    /// Fetch clients in proxy-channel order, for auxiliary upstream downloads
    /// (generated-image inlining) that must respect the proxy policy the way
    /// [`chat`](Self::chat) does: Proxy mode never direct, fallback orders
    /// proxies first.
    pub(crate) fn fetch_channels(&self) -> Vec<Client> {
        crate::proxies::channels(self.settings.proxies_mode, &self.http, &self.proxies)
            .into_iter()
            .map(|(_, client)| client.clone())
            .collect()
    }

    /// Common headers shared by every request, aligned to the real client's
    /// transport for this endpoint kind. The chat path (`ChatFetch`) selects
    /// the "x-Windows Version" UA variant, attaches the per-account DeviceId,
    /// the timestamp/signature pair and the `autoModels` experimental flag,
    /// plus undici fetch's automatic headers. All other endpoints (`Got`) use
    /// the `x-Windows-amd64` UA variant and got's automatic headers; GETs in
    /// the real client carry *no* DeviceId and no signature pair, so
    /// `device_id` is `None` there.
    ///
    /// The `accept-encoding` values are the exact strings got/undici generate
    /// (measured against the bundled Node's stacks, 2026-11-23). They are set
    /// explicitly so reqwest sends byte-identical headers; the tower-http
    /// decompression layer only injects its own value when the header is
    /// absent, so responses still get transparently decompressed.
    fn base_headers(
        &self,
        transport: Transport,
        bearer: &str,
        device_id: Option<&str>,
        signed: Option<(&str, &str)>,
    ) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let bearer = format!("Bearer {bearer}");
        // These values come from config/HMAC output and are always valid header text.
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&bearer).expect("valid bearer header"),
        );
        headers.insert(
            "User-Agent",
            HeaderValue::from_static(match transport {
                Transport::ChatFetch => USER_AGENT_POST,
                Transport::Got => USER_AGENT,
            }),
        );
        // Automatic headers the real client's HTTP stack adds on top of the
        // ones the application code sets (undici fetch / got defaults).
        headers.insert(
            "Accept-Encoding",
            HeaderValue::from_static(transport.accept_encoding()),
        );
        if transport == Transport::ChatFetch {
            headers.insert("Accept-Language", HeaderValue::from_static("*"));
            headers.insert("Sec-Fetch-Mode", HeaderValue::from_static("cors"));
        }
        if let Some((timestamp, signature)) = signed {
            let device_id = device_id.expect("signed requests always carry a device id");
            headers.insert(
                "X-Raycast-DeviceId",
                // Per-account device id (registration generates one and uses
                // it for activation; chat must reuse it or the server sees a
                // "device change").
                HeaderValue::from_str(device_id).expect("valid device id"),
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
    ///
    /// `device_id` is the account's own device id (registration generated it
    /// and used it for pro-trial activation); the signature and
    /// `X-Raycast-DeviceId` must reuse it. There is no global fallback: the
    /// dispatcher takes accounts without a stored id out of rotation.
    pub(crate) async fn chat(
        &self,
        email: &str,
        token: &str,
        device_id: &str,
        body: &str,
        call: &CallContext,
        reserved: Option<Attempt>,
    ) -> Result<reqwest::Response, GatewayError> {
        let timestamp = unix_secs().to_string();
        let signature = calculate_signature_v2(
            &timestamp,
            device_id,
            body,
            &self.settings.raycast_signature_secret,
        );

        let mut headers =
            self.base_headers(Transport::ChatFetch, token, Some(device_id), Some((&timestamp, &signature)));
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
        let result = ensure_success(
            response,
            self.log
                .as_deref()
                .map(|log| (log, call.request.request_id.as_str())),
        )
        .await;
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

    /// Register an attachment upload and push the bytes to the presigned
    /// target, returning the upstream file id the chat request then references
    /// (`content.attachments:[{id, type:"image"}]`).
    ///
    /// Mirrors the real client's `ai/files` flow: a signed POST registers the
    /// blob (md5-base64 checksum), the response carries a presigned `direct_upload`
    /// URL + headers, and a raw PUT pushes the bytes there — no signature
    /// headers on the PUT, exactly as the client does it.
    ///
    /// `chat_id` is the conversation's `thread_id` (the client scopes uploads
    /// to the chat). The signed POST uses the same account's device id as chat.
    pub(crate) async fn upload_file(
        &self,
        token: &str,
        device_id: &str,
        chat_id: &str,
        filename: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<String, GatewayError> {
        let checksum = crate::core::attachment::upload_checksum(bytes);
        let body = serde_json::json!({
            "chat_id": chat_id,
            "blob": {
                "filename": filename,
                "byte_size": bytes.len(),
                "content_type": content_type,
                "checksum": checksum,
            }
        })
        .to_string();

        let timestamp = unix_secs().to_string();
        let signature = calculate_signature_v2(
            &timestamp,
            device_id,
            &body,
            &self.settings.raycast_signature_secret,
        );
        // The real client registers uploads through the got instance (same
        // pipeline as /me): amd64 UA variant, no Experimental flag.
        let mut headers =
            self.base_headers(Transport::Got, token, Some(device_id), Some((&timestamp, &signature)));
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        headers.insert("Accept", HeaderValue::from_static("application/json"));

        let request = self
            .http
            .post(RAYCAST_FILES_URL)
            .headers(headers)
            .body(body)
            .build()?;
        // No CallContext here: a file upload is not an inference call and must
        // not create a statistics Attempt — send() only finalizes chat-stream
        // attempts, so one started here would be dropped un-finished and
        // recorded as "cancelled / upstream attempt dropped before a terminal
        // event" on every image attachment.
        let response = self.send(request, None, None).await?;
        let registered: FileRegistration = ensure_success(response, None).await?.json().await?;

        // Raw PUT to the presigned URL with exactly the headers the server
        // handed us (they carry the auth). The real client issues this PUT
        // through a bare got instance: no default UA/auth headers, but got
        // still adds its automatic `accept-encoding`. Failure here fails the
        // upload.
        let mut put = self.http.put(&registered.direct_upload.url);
        put = put.header("Accept-Encoding", HeaderValue::from_static("gzip, deflate, br, zstd"));
        for (key, value) in &registered.direct_upload.headers {
            let value = value.as_str().unwrap_or_default();
            put = put.header(key.as_str(), value);
        }
        let put_request: reqwest::Request = put
            .body(bytes.to_vec())
            .timeout(self.settings.upstream_timeout())
            .build()?;
        self.http.execute(put_request).await?.error_for_status()?;

        Ok(registered.id)
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

    /// Shared bearer-only GET returning parsed JSON. The real client issues
    /// these through the got instance (`Transport::Got`).
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        token: &str,
    ) -> Result<T, GatewayError> {
        let mut headers = self.base_headers(Transport::Got, token, None, None);
        headers.insert("Accept", HeaderValue::from_static("application/json"));
        let request = self
            .http
            .get(url)
            .headers(headers)
            .timeout(self.settings.upstream_timeout())
            .build()?;
        let response = self.send(request, None, None).await?;
        let response = ensure_success(response, None).await?;
        Ok(response.json().await?)
    }
}

/// Turn a non-2xx upstream response into a [`GatewayError::Upstream`]
/// (401 gets the dedicated [`GatewayError::TokenInvalid`] variant).
async fn ensure_success(
    response: reqwest::Response,
    log: Option<(&RequestLogStore, &str)>,
) -> Result<reqwest::Response, GatewayError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let attempt = response.extensions().get::<Attempt>().cloned();
    let mut message = response.text().await.unwrap_or_default();
    if let Some((log, id)) = log {
        log.update_response(id, &message);
    }
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
