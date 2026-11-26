//! 账号调度:选号、401 续期、流内错误分类与冷却/摘号、换号重试。
//!
//! 依赖方向 `server -> account -> raycast`:[`RaycastClient`] 只做带显式 token 的
//! 传输,哪个账号发、失败后怎么处置都在这里决定。

use std::sync::Arc;

use futures_util::stream::Stream;

use crate::account::store::{AccountRow, AccountStore};
use crate::config::{DEFAULT_COOLDOWN_SECS, MAX_COOLDOWN_SECS, MIN_COOLDOWN_SECS};
use crate::core::event::CanonicalEvent;
use crate::error::GatewayError;
use crate::log::context as debug;
use crate::log::store::RequestLogStore;
use crate::raycast::client::RaycastClient;
use crate::raycast::peek::{peek_first_payload, restore_payload};
use crate::raycast::stream::canonical_events_with_retry;
use crate::raycast::wire::{
    RaycastAttachment, RaycastChatRequest, RaycastRawModel, RaycastSseError,
};
use crate::statistics::{Attempt, CallContext};
use crate::util::unix_secs;

/// 邮箱未确认的冷却时长:用户确认邮箱后,下次手动/定时刷新会恢复可用。
const UNCONFIRMED_COOLDOWN_SECS: u64 = 24 * 60 * 60;

/// 200 流内首个错误 payload 对账号池的处置。
#[derive(Debug, PartialEq)]
pub(crate) enum StreamReaction {
    /// 与账号无关(请求内容问题):终止,不换号。
    Abort { note: &'static str },
    /// 冷却该账号 `secs` 秒后换号重试。
    Cooldown {
        secs: u64,
        reason: String,
        note: &'static str,
    },
    /// 摘除该账号并换号重试。
    Remove { reason: String, note: &'static str },
    /// 不是账号类错误:原样交给解析层上报。
    PassThrough,
}

/// 纯函数:把上游流内错误映射为账号动作。
pub(crate) fn classify(err: &RaycastSseError, message: &str) -> StreamReaction {
    if err.is_content_policy_blocked() {
        // 内容策略拦截:换号重试纯属浪费。
        return StreamReaction::Abort {
            note: "upstream content-policy block (request content, not account)",
        };
    }
    if err.is_usage_limit() {
        let secs = err
            .retry_after
            .unwrap_or(DEFAULT_COOLDOWN_SECS)
            .clamp(MIN_COOLDOWN_SECS, MAX_COOLDOWN_SECS);
        return StreamReaction::Cooldown {
            secs,
            reason: format!("限额冷却 {secs}s ({message})"),
            note: "account rate-limited, cooling down and rotating",
        };
    }
    // 风控 / 订阅缺失:token 有效但账号用不了 AI,摘号。
    if err.is_flagged() {
        return StreamReaction::Remove {
            reason: format!("账号被风控 ({message})"),
            note: "account flagged by upstream, removing from pool",
        };
    }
    if err.is_subscription_required() {
        return StreamReaction::Remove {
            reason: format!("订阅缺失/已失效 ({message})"),
            note: "subscription required, removing account and rotating",
        };
    }
    if err.is_unconfirmed() {
        return StreamReaction::Cooldown {
            secs: UNCONFIRMED_COOLDOWN_SECS,
            reason: format!("邮箱未确认 ({message})"),
            note: "email not confirmed, cooling down 24h and rotating",
        };
    }
    StreamReaction::PassThrough
}

/// 池全空的 [`GatewayError::NoAccount`] 充实:查最早已恢复的冷却账号。
/// 有账号在冷却 → [`GatewayError::RateLimited`] (429 + `Retry-After`),
/// harness 读头精确等待并显示 "rate limited until …";全被摘除/池空 →
/// 保持 503 但附 `x-should-retry: false`,harness 立即放弃而不是指数退避
/// 反复打。
fn enrich_no_account(error: GatewayError, accounts: &AccountStore) -> GatewayError {
    let GatewayError::NoAccount(message) = error else {
        return error;
    };
    match accounts.earliest_cooldown_secs() {
        Some(secs) if secs > 0 => {
            let mins = (secs as f64 / 60.0).ceil() as u64;
            GatewayError::RateLimited {
                retry_after_secs: secs,
                message: format!(
                    "all Raycast accounts are rate-limited; earliest recovers in ~{secs}s (~{mins} min)"
                ),
            }
        }
        _ => GatewayError::NoAccount(format!(
            "{message}; no account is in cooldown — every account was removed (flagged/unavailable) or the pool is empty"
        )),
    }
}

/// 账号池 + 上游传输。克隆代价低(内部均为 `Arc`/句柄)。
#[derive(Clone)]
pub struct AccountDispatcher {
    accounts: Arc<AccountStore>,
    /// 传输层;额度刷新、token 换新等需要显式 token 的调用直接用它。
    pub(crate) client: RaycastClient,
    /// 出口地区解析器:选号后按账号出口(direct / 账号代理)解析
    /// locale + timezone,写回请求体。`None` 时保持请求体原样(测试)。
    regions: Option<Arc<crate::geo::EgressResolver>>,
}

impl AccountDispatcher {
    pub fn new(accounts: Arc<AccountStore>, client: RaycastClient) -> Self {
        Self {
            accounts,
            client,
            regions: None,
        }
    }

    /// Attach the request-log store (called at startup, after construction).
    pub fn with_log(mut self, log: Arc<RequestLogStore>) -> Self {
        self.client.log = Some(log);
        self
    }

    /// Attach the egress region resolver (called at startup).
    pub fn with_regions(mut self, regions: Arc<crate::geo::EgressResolver>) -> Self {
        self.regions = Some(regions);
        self
    }

    /// 用当前账号的 refresh_token 换新并写回 `account.access_token`。
    async fn renew(&self, account: &mut AccountRow) -> Result<(), GatewayError> {
        crate::trace_info!(
            "renewal",
            "{} -> token expired, renewing via refresh_token",
            account.email
        );
        account.access_token =
            crate::account::token_renewal::auto_renew(&self.accounts, &self.client, account)
                .await?;
        Ok(())
    }

    /// POST a chat request. Returns the streaming response on success (status
    /// already validated); the caller consumes `bytes_stream()`.
    ///
    /// 账号故障切换:401 续期后重试,续期失败摘号换下一个;200 但流内首
    /// payload 是账号类错误时按 [`classify`] 冷却/摘号再换号重试。池内全部
    /// 不可用时把最后一个真实错误透传给客户端。
    pub(crate) async fn send_chat(
        &self,
        request: &RaycastChatRequest,
        call: &CallContext,
        mut reserved: Option<Attempt>,
    ) -> Result<reqwest::Response, GatewayError> {
        let attempts = self.accounts.count_available().max(1);
        let mut last_error: Option<GatewayError> = None;

        for _ in 0..attempts {
            let mut account = match self.accounts.next_available() {
                Ok(account) => account,
                Err(error) => {
                    // 池空且从未拿到上游错误:用冷却 ETA 充实 503,让客户端
                    // 知道这是限流而非永久故障,以及什么时候能恢复。
                    return Err(
                        last_error.unwrap_or_else(|| enrich_no_account(error, &self.accounts))
                    );
                }
            };
            crate::trace_info!(
                "dispatcher",
                "using account={} tag={} device={}",
                account.email,
                account.tag.as_deref().unwrap_or("-"),
                account
                    .device_id
                    .as_deref()
                    .map(|d| &d[..8.min(d.len())])
                    .unwrap_or("-"),
            );
            // device_id 是账号必填字段(导入时校验),这里直接取;None 只
            // 可能来自配置文件遗留的旧行,标记摘除而不是静默回落。
            let Some(device_id) = account.device_id.clone() else {
                self.accounts.mark_unavailable(
                    &account.email,
                    "missing device_id (pre-migration row) — re-import the account",
                );
                last_error = Some(GatewayError::BadRequest(format!(
                    "account {} has no device_id; re-import it with one",
                    account.email
                )));
                continue;
            };
            // 图片附件上传:必须在账号选定之后(附件 ACL 绑定账号),且 body
            // 序列化之前(fileId 要进 wire)。上传失败按官方客户端行为降级:
            // 剥附件、文本加占位,请求照发,不让一张图废掉整个推理。
            let client = self.client.for_account(&account)?;
            let mut request = request.clone();
            // geo 出口对齐:选号后按该账号的出口(direct / 账号代理)解析
            // locale + timezone,写回请求体(幂等替换 user-preferences 块)。
            // 真客户端的 locale/timezone 来自运行机器,必须与实际出口 IP 的
            // 地理一致;解析有 TTL 缓存,常态零 IO,失败回落本机身份不阻塞。
            if let Some(regions) = &self.regions {
                let (cache_key, proxy_client) = client.geo_probe_channel()?;
                let region = regions.resolve_for(&cache_key, proxy_client).await;
                crate::trace_info!(
                    "geo",
                    "egress identity locale={} tz={} channel={}",
                    region.locale,
                    region.timezone,
                    cache_key,
                );
                crate::raycast::builder::apply_region(&mut request, &region);
            } else {
                // 忘接解析器的 dispatcher 会静默发出与本机身份绑定的
                // locale/user-preferences——恰是出口对齐要消除的交叉信号。
                crate::log::trace::warn_once(
                    "dispatcher-regions-missing",
                    "dispatcher has no egress region resolver; upstream requests will carry the machine locale (call with_regions at construction)",
                );
            }
            if !request.pending_uploads.is_empty() {
                self.upload_attachments(&client, &mut request, &account, &device_id)
                    .await;
            }
            let body = serde_json::to_string(&request)?;
            let mut result = client
                .chat(
                    &account.email,
                    &account.access_token,
                    &device_id,
                    &body,
                    call,
                    reserved.take(),
                )
                .await;
            if matches!(result, Err(GatewayError::TokenInvalid { .. })) {
                result = match self.renew(&mut account).await {
                    Ok(()) => {
                        client
                            .chat(
                                &account.email,
                                &account.access_token,
                                &device_id,
                                &body,
                                call,
                                reserved.take(),
                            )
                            .await
                    }
                    Err(error) => Err(error),
                };
            }
            match result {
                Ok(response) => {
                    // 上游部分错误(限额)藏在 200 SSE 流内:偷看首 payload。
                    let (peeked, response) =
                        peek_first_payload(response, self.client.settings.upstream_timeout()).await;
                    let Some(err) = peeked else {
                        let mut response = response;
                        response
                            .extensions_mut()
                            .insert(client.fetch_clients(crate::proxies::Transport::ChatFetch)?);
                        return Ok(response);
                    };
                    if let Some(log) = &self.client.log {
                        log.update_response(&call.request.request_id, &err.raw);
                    }
                    let message = err.message();
                    if let Some(attempt) = response.extensions().get::<Attempt>() {
                        attempt.observe_payload(&err.raw);
                        attempt.finish("failed", Some(&message));
                    }
                    let status = if err.parsed.http_status != 0 {
                        err.parsed.http_status
                    } else {
                        502
                    };
                    match classify(&err.parsed, &message) {
                        StreamReaction::Abort { note } => {
                            crate::trace_warn!(
                                "dispatcher",
                                "{} -> {}: {}",
                                account.email,
                                note,
                                message
                            );
                            return Err(GatewayError::Upstream {
                                status,
                                message: format!(
                                    "{message}{}",
                                    crate::raycast::parser::content_policy_hint()
                                ),
                            });
                        }
                        StreamReaction::Cooldown { secs, reason, note } => {
                            crate::trace_warn!(
                                "dispatcher",
                                "{} -> {} (cooldown {}s): {}",
                                account.email,
                                note,
                                secs,
                                message
                            );
                            self.accounts.mark_cooldown(
                                &account.email,
                                unix_secs() as i64 + secs as i64,
                                &reason,
                            );
                            last_error = Some(GatewayError::Upstream { status, message });
                        }
                        StreamReaction::Remove { reason, note } => {
                            crate::trace_warn!(
                                "dispatcher",
                                "{} -> {}: {}",
                                account.email,
                                note,
                                message
                            );
                            self.accounts.mark_unavailable(&account.email, &reason);
                            last_error = Some(GatewayError::Upstream { status, message });
                        }
                        StreamReaction::PassThrough => {
                            crate::trace_warn!(
                                "dispatcher",
                                "{} -> upstream in-stream error (not rate-limit): {}",
                                account.email,
                                message
                            );
                            // 其他流内错误:还原 payload,交由解析层上报。
                            return Ok(restore_payload(response, &err.raw));
                        }
                    }
                }
                Err(err @ GatewayError::TokenInvalid { .. }) => {
                    self.accounts
                        .mark_unavailable(&account.email, "token 失效，无法续期");
                    last_error = Some(err);
                }
                Err(err) => {
                    // HTTP 层错误(非流内):账号本身不可用(风控 403 / 订阅
                    // 缺失 402)则摘号换下一个,否则透传终止。
                    if err.is_account_dead() {
                        crate::trace_warn!(
                            "dispatcher",
                            "{} -> account unusable upstream (HTTP error), removing from pool: {}",
                            account.email,
                            err
                        );
                        self.accounts
                            .mark_unavailable(&account.email, &format!("账号不可用 ({err})"));
                        last_error = Some(err);
                        continue;
                    }
                    crate::trace_error!(
                        "dispatcher",
                        "{} -> upstream request failed: {}",
                        account.email,
                        err
                    );
                    return Err(err);
                }
            }
        }

        // 池内账号全部 401 / 限额:透传最后一个真实错误。
        crate::trace_error!(
            "dispatcher",
            "all {} available accounts failed; surfacing last error to client",
            attempts
        );
        Err(last_error.unwrap_or_else(|| {
            enrich_no_account(
                GatewayError::NoAccount(
                    "no available Raycast account; add or refresh one via POST /v1/accounts/create"
                        .into(),
                ),
                &self.accounts,
            )
        }))
    }

    /// 复用聊天已选账号的客户端下载远程图片、上传附件并回填文件 ID。
    /// 每次换号重新按该账号的代理设置执行，避免提前使用共享直连出口。
    ///
    /// `chat_id` is the request's `thread_id` — the upstream scopes uploads to
    /// a chat, and the builder already derives it for session affinity.
    ///
    /// Degradation: an attachment that fails to upload is dropped and its
    /// message text gains the client's omission placeholder, mirroring how the
    /// official client handles images it cannot attach. Requests still go out
    /// (text-only) when everything fails.
    async fn upload_attachments(
        &self,
        client: &RaycastClient,
        request: &mut RaycastChatRequest,
        account: &AccountRow,
        device_id: &str,
    ) {
        let pending = std::mem::take(&mut request.pending_uploads);
        for upload in pending {
            let result = async {
                let attachment = match upload.attachment {
                    crate::core::message::Attachment::Remote { url } => {
                        let clients = client.fetch_clients(crate::proxies::Transport::Got)?;
                        crate::core::attachment::fetch_url(&clients, &url)
                            .await
                            .ok_or_else(|| GatewayError::BadRequest("远程图片下载失败".into()))?
                    }
                    inline => inline,
                };
                let crate::core::message::Attachment::Inline { media_type, data } = attachment
                else {
                    return Err(GatewayError::BadRequest("图片内容无效".into()));
                };
                if !request.image_formats.contains(&media_type) {
                    return Err(GatewayError::BadRequest("模型不支持该图片格式".into()));
                }
                let id = client
                    .upload_file(
                        &account.access_token,
                        device_id,
                        &request.thread_id,
                        &crate::core::attachment::upload_filename(&media_type, &data),
                        &media_type,
                        &data,
                    )
                    .await?;
                Ok((id, media_type, data.len()))
            }
            .await;
            match result {
                Ok((id, media_type, bytes)) => {
                    crate::trace_info!(
                        "dispatcher",
                        "{} -> attachment uploaded file={} type={} bytes={}",
                        account.email,
                        id,
                        media_type,
                        bytes
                    );
                    if let Some(message) = request.messages.get_mut(upload.message_index) {
                        message
                            .content
                            .attachments
                            .get_or_insert_with(Vec::new)
                            .push(RaycastAttachment::image(id));
                    }
                }
                Err(error) => {
                    crate::trace_warn!(
                        "dispatcher",
                        "{} -> attachment download or upload failed, dropping image: {}",
                        account.email,
                        error
                    );
                    if let Some(message) = request.messages.get_mut(upload.message_index) {
                        message.content.text.push(' ');
                        message
                            .content
                            .text
                            .push_str(crate::core::message::IMAGE_OMITTED_PLACEHOLDER);
                        if message
                            .content
                            .attachments
                            .as_ref()
                            .is_some_and(Vec::is_empty)
                        {
                            message.content.attachments = None;
                        }
                    }
                }
            }
        }
        // Any message still carrying an empty attachments array had all its
        // uploads fail; strip the field to keep the wire shape honest.
        for message in &mut request.messages {
            if message
                .content
                .attachments
                .as_ref()
                .is_some_and(|ids| ids.is_empty())
            {
                message.content.attachments = None;
            }
        }
    }

    /// 拉模型列表。与 [`Self::send_chat`] 一致地做 401 续期与账号故障切换:
    /// 当前轮到的账号失效不应让整个模型列表失败。
    pub async fn fetch_models(&self) -> Result<Vec<RaycastRawModel>, GatewayError> {
        let attempts = self.accounts.count_available().max(1);
        let mut last_error: Option<GatewayError> = None;
        for _ in 0..attempts {
            let mut account = match self.accounts.next_available() {
                Ok(account) => account,
                Err(error) => return Err(last_error.unwrap_or(error)),
            };
            let client = self.client.for_account(&account)?;
            let mut result = client.fetch_models(&account.access_token).await;
            if matches!(result, Err(GatewayError::TokenInvalid { .. })) {
                result = match self.renew(&mut account).await {
                    Ok(()) => client.fetch_models(&account.access_token).await,
                    Err(error) => Err(error),
                };
            }
            match result {
                Ok(models) => return Ok(models),
                Err(err @ GatewayError::TokenInvalid { .. }) => {
                    self.accounts
                        .mark_unavailable(&account.email, "token 失效，无法续期");
                    last_error = Some(err);
                }
                Err(err) if err.is_account_dead() => {
                    crate::trace_warn!(
                        "dispatcher",
                        "{} -> account unusable upstream (models), removing from pool: {}",
                        account.email,
                        err
                    );
                    self.accounts
                        .mark_unavailable(&account.email, &format!("账号不可用 ({err})"));
                    last_error = Some(err);
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_error
            .unwrap_or_else(|| GatewayError::NoAccount("no available Raycast account".into())))
    }

    /// 把上游响应转成 canonical 事件流;流在输出前被截断时经本调度器重发一次。
    /// 响应携带已选账号的图片下载客户端，重试换号后随新响应更新。
    pub(crate) fn events(
        &self,
        response: reqwest::Response,
        request: RaycastChatRequest,
        log: Arc<RequestLogStore>,
        call: CallContext,
    ) -> impl Stream<Item = CanonicalEvent> + Send + 'static {
        let id = debug::current_request_id();
        let timeout = self.client.settings.upstream_timeout();
        let dispatcher = self.clone();
        canonical_events_with_retry(response, log, timeout, move || {
            Box::pin(async move {
                debug::REQUEST_ID
                    .scope(id, dispatcher.send_chat(&request, &call, None))
                    .await
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use serde_json::json;

    /// 测试用的合法 device_id(64 位小写 hex)。
    const TEST_DEVICE_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    fn sse(value: serde_json::Value) -> RaycastSseError {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn classify_maps_each_in_stream_error_to_one_account_action() {
        let reaction = |value| classify(&sse(value), "m");
        assert!(matches!(
            reaction(json!({"code":"content_policy_blocked","http_status":403})),
            StreamReaction::Abort { .. }
        ));
        // 限额:retry_after 被夹到配置的上下限内。
        assert!(matches!(
            reaction(json!({"http_status":429,"retry_after":999})),
            StreamReaction::Cooldown { secs: 999, .. }
        ));
        assert!(matches!(
            reaction(json!({"http_status":429,"retry_after":5})),
            StreamReaction::Cooldown { secs, .. } if secs == MIN_COOLDOWN_SECS
        ));
        assert!(matches!(
            reaction(json!({"http_status":429,"retry_after":30 * 60 * 60})),
            StreamReaction::Cooldown { secs, .. } if secs == MAX_COOLDOWN_SECS
        ));
        assert!(matches!(
            reaction(json!({"http_status":429})),
            StreamReaction::Cooldown { secs, .. } if secs == DEFAULT_COOLDOWN_SECS.clamp(MIN_COOLDOWN_SECS, MAX_COOLDOWN_SECS)
        ));
        assert!(matches!(
            reaction(json!({"http_status":403,"subcode":"account_unavailable"})),
            StreamReaction::Remove { .. }
        ));
        assert!(matches!(
            reaction(json!({"http_status":402})),
            StreamReaction::Remove { .. }
        ));
        assert!(matches!(
            reaction(json!({"http_status":502,"message":"other"})),
            StreamReaction::PassThrough
        ));
    }

    #[test]
    fn dead_account_http_errors_are_recognised() {
        let upstream = |status, message: &str| GatewayError::Upstream {
            status,
            message: message.into(),
        };
        assert!(upstream(402, "x").is_account_dead());
        assert!(upstream(403, "Account flagged for suspicious activity").is_account_dead());
        assert!(!upstream(500, "boom").is_account_dead());
        assert!(!GatewayError::NoAccount("x".into()).is_account_dead());
    }

    #[tokio::test]
    async fn statistics_follow_account_rotation_http_failures_and_sse_rejections() {
        for failure in ["quota", "http", "token", "policy", "other"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, axum::Router::new().route("/", axum::routing::post(
                    move |headers: axum::http::HeaderMap| async move {
                        if headers.get("authorization").unwrap() == "Bearer first" {
                            let (status, payload) = match failure {
                                "quota" => (200, json!({"error":{"http_status":429,"message":"quota"},"usage":{"total_cost":0.1}})),
                                "http" => (402, json!({"error":"subscription is required"})),
                                "token" => (401, json!({"error":"invalid token"})),
                                "policy" => (200, json!({"error":{"http_status":403,"code":"content_policy_blocked","message":"blocked"}})),
                                _ => (200, json!({"error":{"http_status":502,"message":"other"}})),
                            };
                            (axum::http::StatusCode::from_u16(status).unwrap(), format!("data: {payload}\n\n"))
                        } else {
                            (axum::http::StatusCode::OK, "data: {\"finish_reason\":\"stop\",\"usage\":{\"input_tokens\":3,\"total_credits\":2.5}}\n\n".into())
                        }
                    }
                ))).await.unwrap();
            });
            let state = crate::server::state::compact_test_state();
            for (email, token) in [("a@example.com", "first"), ("b@example.com", "second")] {
                state
                    .accounts
                    .upsert(
                        &serde_json::from_value(
                            json!({"user":{"email":email},"device_id":TEST_DEVICE_ID,"access_token":token}),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                state.accounts.set_use_status(email, "enable").unwrap();
            }
            let mut dispatcher = state.dispatcher.clone();
            dispatcher.client.http = reqwest::Client::builder().no_proxy().build().unwrap();
            // send_chat executes on the chat pool; replace it too or the
            // original (empty-settings) pool would silently be exercised.
            dispatcher.client.chat_http = reqwest::Client::builder().no_proxy().build().unwrap();
            dispatcher.client.chat_url = Some(format!("http://{address}/"));
            let request = crate::core::request::CanonicalRequest::new(
                crate::core::protocol::Protocol::Responses,
                "client",
            );
            let upstream = crate::raycast::builder::build_chat_request(
                &request,
                "openai",
                "upstream",
                &crate::geo::Region {
                    locale: "zh-CN".to_string(),
                    timezone: "Asia/Shanghai".to_string(),
                },
                false,
            );
            let call = CallContext::new(&request, &upstream.provider, &upstream.model, "inference");
            let result = dispatcher.send_chat(&upstream, &call, None).await;
            if failure == "policy" {
                assert!(result.is_err());
            } else {
                let stream = dispatcher.events(result.unwrap(), upstream, state.log.clone(), call);
                let _: Vec<_> = stream.collect().await;
            }
            let rows = state.statistics.list(&Default::default()).unwrap().items;
            let rotated = matches!(failure, "quota" | "http" | "token");
            assert_eq!(rows.len(), if rotated { 2 } else { 1 }, "{failure}");
            let failed = rows
                .iter()
                .find(|row| row.account_email == "a@example.com")
                .unwrap();
            assert_eq!(failed.status, "failed", "{failure}");
            assert!(failed.finished_at.is_some());
            assert_eq!(
                failed.http_status,
                Some(match failure {
                    "http" => 402,
                    "token" => 401,
                    _ => 200,
                })
            );
            if failure == "quota" {
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(failed.usage_json.as_ref().unwrap())
                        .unwrap()["total_cost"],
                    0.1
                );
            }
            if rotated {
                let completed = rows
                    .iter()
                    .find(|row| row.account_email == "b@example.com")
                    .unwrap();
                assert_eq!(completed.status, "completed");
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(
                        completed.usage_json.as_ref().unwrap()
                    )
                    .unwrap()["total_credits"],
                    2.5
                );
                assert_eq!(failed.request_id, completed.request_id);
            }
            server.abort();
        }
    }

    /// `fetch_models` 现在做换号:第一个账号被上游拒绝(402)时用下一个账号成功。
    #[tokio::test]
    async fn fetch_models_rotates_past_dead_account() {
        let _ = json!({});
        let state = crate::server::state::compact_test_state();
        for (email, token) in [("a@example.com", "first"), ("b@example.com", "second")] {
            state
                .accounts
                .upsert(
                    &serde_json::from_value(
                        json!({"user":{"email":email},"device_id":TEST_DEVICE_ID,"access_token":token}),
                    )
                    .unwrap(),
                )
                .unwrap();
            state.accounts.set_use_status(email, "enable").unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/",
                    axum::routing::get(|headers: axum::http::HeaderMap| async move {
                        if headers.get("authorization").unwrap() == "Bearer first" {
                            (
                                axum::http::StatusCode::PAYMENT_REQUIRED,
                                "subscription is required".to_string(),
                            )
                        } else {
                            (
                                axum::http::StatusCode::OK,
                                json!({"models":[{"id":"m","model":"m","provider":"openai"}]})
                                    .to_string(),
                            )
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let mut dispatcher = state.dispatcher.clone();
        dispatcher.client.http = reqwest::Client::builder().no_proxy().build().unwrap();
        dispatcher.client.models_url = Some(format!("http://{address}/"));
        let models = dispatcher.fetch_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(state.accounts.count_available(), 1);
        server.abort();
    }
}
