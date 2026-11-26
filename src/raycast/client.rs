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

pub use crate::proxies::Transport;

/// 当前账号的单个代理及其回退选项，由数据库中的账号设置生成。
#[derive(Clone)]
pub(crate) struct AccountProxy {
    pub id: String,
    pub fallback_direct: bool,
    pub http: Client,
    pub chat_http: Client,
}

/// 共享客户端尚未选号；只有 for_account 返回的客户端可以发起账号请求。
#[derive(Clone)]
pub(crate) enum AccountRoute {
    Unselected,
    Direct,
    Proxy(AccountProxy),
}

/// Shared direct pools and per-account proxy clients for both upstream transports.
#[derive(Clone)]
pub struct RaycastClient {
    pub(crate) http: Client,
    pub(crate) account_route: AccountRoute,
    pub(crate) proxy_store: Option<Arc<crate::proxies::store::ProxyStore>>,
    /// chat 专用直连客户端(h2 能力,对齐 undici fetch)。
    pub(crate) chat_http: Client,
    pub(crate) settings: Arc<Settings>,
    pub(crate) statistics: Arc<StatisticsStore>,
    pub(crate) log: Option<Arc<RequestLogStore>>,
    #[cfg(test)]
    pub(crate) chat_url: Option<String>,
    #[cfg(test)]
    pub(crate) models_url: Option<String>,
}

impl RaycastClient {
    /// Build shared direct pools; account proxy clients are cached by ProxyStore.
    pub fn new(settings: Arc<Settings>, statistics: Arc<StatisticsStore>) -> Self {
        let timeout = settings.upstream_timeout();
        Self {
            http: crate::proxies::direct_client(timeout),
            account_route: AccountRoute::Unselected,
            proxy_store: None,
            chat_http: crate::proxies::direct_chat_client(timeout),
            settings,
            statistics,
            log: None,
            #[cfg(test)]
            chat_url: None,
            #[cfg(test)]
            models_url: None,
        }
    }

    pub fn with_proxy_store(mut self, store: Arc<crate::proxies::store::ProxyStore>) -> Self {
        self.proxy_store = Some(store);
        self
    }

    /// 按当前账号的 proxy 和 proxy_fallback_direct 选择出口，不读取全局代理配置。
    /// 指定代理不存在或无法构造客户端时返回错误，不隐式改为直连。
    pub(crate) fn for_account(
        &self,
        account: &crate::account::store::AccountRow,
    ) -> Result<Self, GatewayError> {
        let mut client = self.clone();
        if account.proxy == "direct" {
            client.account_route = AccountRoute::Direct;
        } else {
            let store = self
                .proxy_store
                .as_ref()
                .ok_or_else(|| GatewayError::Internal("proxy store unavailable".into()))?;
            let (http, chat) = store.clients(&account.proxy, self.settings.upstream_timeout())?;
            client.account_route = AccountRoute::Proxy(AccountProxy {
                id: account.proxy.clone(),
                fallback_direct: account.proxy_fallback_direct,
                http,
                chat_http: chat,
            });
        }
        Ok(client)
    }

    /// 当前账号的请求出口：直连，或指定代理；仅勾选回退时追加直连。
    /// 共享客户端未选号时拒绝请求，避免绕过账号设置。
    pub(crate) fn request_channels(
        &self,
        transport: Transport,
    ) -> Result<Vec<(&str, &Client)>, GatewayError> {
        let direct = match transport {
            Transport::Got => &self.http,
            Transport::ChatFetch => &self.chat_http,
        };
        match &self.account_route {
            AccountRoute::Unselected => Err(GatewayError::Internal(
                "upstream account must be selected before request".into(),
            )),
            AccountRoute::Direct => Ok(vec![("direct", direct)]),
            AccountRoute::Proxy(proxy) => {
                let http = match transport {
                    Transport::Got => &proxy.http,
                    Transport::ChatFetch => &proxy.chat_http,
                };
                let mut channels = vec![(proxy.id.as_str(), http)];
                if proxy.fallback_direct {
                    channels.push(("direct", direct));
                }
                Ok(channels)
            }
        }
    }

    pub(crate) fn fallback_direct(&self) -> bool {
        matches!(&self.account_route, AccountRoute::Proxy(proxy) if proxy.fallback_direct)
    }

    /// geo 出口探测入口(选号后调用):`(缓存键, 可选代理 got 客户端)`。
    /// direct 账号 → `("direct", None)`(resolver 用自己的直连客户端);
    /// 代理账号 → `(proxy_id, Some(该代理的 got 客户端))`——探测请求走
    /// 账号同一条代理,出口与 chat 一致。未选号返回错误。
    pub(crate) fn geo_probe_channel(&self) -> Result<(String, Option<&Client>), GatewayError> {
        match &self.account_route {
            AccountRoute::Unselected => Err(GatewayError::Internal(
                "upstream account must be selected before geo probe".into(),
            )),
            AccountRoute::Direct => Ok(("direct".to_string(), None)),
            AccountRoute::Proxy(proxy) => Ok((proxy.id.clone(), Some(&proxy.http))),
        }
    }

    /// 下载客户端也由已选账号生成。生成图片使用 chat 池，输入图片使用 got 池。
    pub(crate) fn fetch_clients(&self, transport: Transport) -> Result<Vec<Client>, GatewayError> {
        Ok(self
            .request_channels(transport)?
            .into_iter()
            .map(|(_, client)| client.clone())
            .collect())
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

        let mut headers = self.base_headers(
            Transport::ChatFetch,
            token,
            Some(device_id),
            Some((&timestamp, &signature)),
        );
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        // The real client sends `application/json` even though the response is
        // SSE (capture 2026-11-20); `text/event-stream` is a fingerprint.
        headers.insert("Accept", HeaderValue::from_static("application/json"));

        #[cfg(test)]
        let url = self.chat_url.as_deref().unwrap_or(RAYCAST_API_URL);
        #[cfg(not(test))]
        let url = RAYCAST_API_URL;
        // Build and execute on the same (chat) pool: RequestBuilder::build()
        // is pool-independent today, but pairing them keeps any future
        // pool-level defaults from silently not applying.
        let request = self
            .chat_http
            .post(url)
            .headers(headers)
            .body(body.to_owned())
            .build()?;
        let response = self
            .send(request, Transport::ChatFetch, Some((call, email)), reserved)
            .await?;
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

    /// Fetch the account's model list and format the gateway model IDs.
    pub async fn fetch_models(&self, token: &str) -> Result<Vec<RaycastRawModel>, GatewayError> {
        #[cfg(test)]
        let url = self.models_url.as_deref().unwrap_or(RAYCAST_MODELS_URL);
        #[cfg(not(test))]
        let url = RAYCAST_MODELS_URL;
        // The real client's `GET /ai/models` (Cjn in backend/index.mjs) carries
        // `X-Raycast-Experimental: autoModels` on every call — omitting it is
        // a fingerprint. Other GETs (`/me`, `/ai/credits`, ...) do not carry it.
        let mut parsed: RaycastModelsResponse = self
            .get_json_with_experimental(url, token, Some("autoModels"))
            .await?;
        let is_id_part = |part: &str| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
        };
        for model in &mut parsed.models {
            model
                .extra
                .insert("owned_id".into(), model.id.clone().into());
            if let Some(name) = model.extra.get("name").cloned() {
                model.extra.insert("display_name".into(), name);
            } else {
                model.extra.remove("display_name");
            }
            if let Some((namespace, id)) = model.id.split_once('/')
                && is_id_part(namespace)
                && is_id_part(id)
            {
                model.id = id.to_owned();
            } else if let Some(brand) = model.extra.get("provider_brand").and_then(|v| v.as_str()) {
                model.id = model.id.replacen(&format!("{brand}-"), "", 1);
            }
        }
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
        let mut headers = self.base_headers(
            Transport::Got,
            token,
            Some(device_id),
            Some((&timestamp, &signature)),
        );
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
        let response = self.send_got(request, None, None).await?;
        let registered: FileRegistration = ensure_success(response, None).await?.json().await?;

        // Raw PUT to the presigned URL with exactly the headers the server
        // handed us (they carry the auth). The real client issues this PUT
        // through a bare got instance: no default UA/auth headers, but got
        // still adds its automatic `accept-encoding`. Failure here fails the
        // upload.
        let mut put = self.http.put(&registered.direct_upload.url);
        put = put.header(
            "Accept-Encoding",
            HeaderValue::from_static("gzip, deflate, br, zstd"),
        );
        for (key, value) in &registered.direct_upload.headers {
            let value = value.as_str().unwrap_or_default();
            put = put.header(key.as_str(), value);
        }
        let put_request: reqwest::Request = put
            .body(bytes.to_vec())
            .timeout(self.settings.upstream_timeout())
            .build()?;
        self.send_got(put_request, None, None)
            .await?
            .error_for_status()?;

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
    /// these through the got instance (`Transport::Got`); no signature pair,
    /// no DeviceId. `experimental` adds the `X-Raycast-Experimental` header —
    /// only `/ai/models` sends it in the real client.
    async fn get_json_with_experimental<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        token: &str,
        experimental: Option<&str>,
    ) -> Result<T, GatewayError> {
        let mut headers = self.base_headers(Transport::Got, token, None, None);
        headers.insert("Accept", HeaderValue::from_static("application/json"));
        if let Some(flag) = experimental {
            headers.insert(
                "X-Raycast-Experimental",
                HeaderValue::from_str(flag).expect("valid experimental header"),
            );
        }
        let request = self
            .http
            .get(url)
            .headers(headers)
            .timeout(self.settings.upstream_timeout())
            .build()?;
        let response = self.send_got(request, None, None).await?;
        let response = ensure_success(response, None).await?;
        Ok(response.json().await?)
    }

    /// Bearer-only GET with no `X-Raycast-Experimental` header.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        token: &str,
    ) -> Result<T, GatewayError> {
        self.get_json_with_experimental(url, token, None).await
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

#[cfg(test)]
mod account_proxy_tests {
    use super::*;
    use axum::{Router, routing::get};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn both_transports_use_account_proxy_and_failure_never_reaches_direct() {
        let state = crate::server::state::compact_test_state();
        let record = serde_json::from_value(serde_json::json!({"user":{"email":"proxy@test"},"device_id":"1".repeat(64),"access_token":"fixture"})).unwrap();
        state.accounts.upsert(&record).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hit_count = hits.clone();
        let direct = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/test", direct.local_addr().unwrap());
        let direct_server = tokio::spawn(async move {
            axum::serve(
                direct,
                Router::new().route(
                    "/test",
                    get(move || {
                        let hits = hit_count.clone();
                        async move {
                            hits.fetch_add(1, Ordering::SeqCst);
                            "direct"
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let account = state.accounts.get("proxy@test").unwrap().unwrap();
        let root = state.dispatcher.client.clone();
        let client = root.for_account(&account).unwrap();
        let response = client
            .send(
                client.http.get(&url).build().unwrap(),
                Transport::Got,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "direct");
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = proxy.local_addr().unwrap().port();
        let input = crate::proxies::store::ProxyInput {
            kind: "http".into(),
            host: "127.0.0.1".into(),
            port,
            user_name: String::new(),
            password: String::new(),
        };
        let row = state.proxies.save(None, input.clone()).unwrap();
        state
            .proxies
            .set_account_proxy("proxy@test", &row.id)
            .unwrap();
        let proxy_server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            for _ in 0..2 {
                let (mut socket, _) = proxy.accept().await.unwrap();
                let mut bytes = [0; 4096];
                let n = socket.read(&mut bytes).await.unwrap();
                assert!(String::from_utf8_lossy(&bytes[..n]).starts_with("GET http://127.0.0.1:"));
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nproxy",
                    )
                    .await
                    .unwrap();
            }
        });
        let account = state.accounts.get("proxy@test").unwrap().unwrap();
        let client = root.for_account(&account).unwrap();
        for transport in [Transport::Got, Transport::ChatFetch] {
            let response = client
                .send(
                    client.http.get(&url).build().unwrap(),
                    transport,
                    None,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(response.text().await.unwrap(), "proxy");
        }
        proxy_server.await.unwrap();
        state
            .proxies
            .save(
                Some(&row.id),
                crate::proxies::store::ProxyInput { port: 1, ..input },
            )
            .unwrap();
        let client = root.for_account(&account).unwrap();
        assert!(
            client
                .send(
                    client.http.get(&url).build().unwrap(),
                    Transport::Got,
                    None,
                    None
                )
                .await
                .is_err()
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        direct_server.abort();
    }
}
