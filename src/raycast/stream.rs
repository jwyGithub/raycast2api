//! Bridges the Raycast SSE transport to the canonical event stream.
//!
//! [`canonical_events_with_retry`] owns the upstream [`reqwest::Response`] and yields a
//! provider-agnostic [`CanonicalEvent`] stream: it feeds the raw byte chunks
//! through the [`SseDecoder`], then maps each decoded `data:` payload with the
//! [`parser`](super::parser). This is the single place the byte transport meets
//! component ⑦, so every consumer — the streaming outbound adapters and the
//! non-streaming aggregation alike — observes exactly the same event sequence.
//!
//! Missing terminal events are distinguished from explicit upstream errors so
//! callers can retry a truncated response once, but only before any output is delivered.

use crate::error::GatewayError;
use async_stream::stream;
use futures_util::{Stream, StreamExt, future::BoxFuture};

use crate::core::event::CanonicalEvent;
use crate::log::context as debug;
use crate::log::format::{DeltaLogBuffer, truncate_for_log};
use crate::log::store::RequestLogStore;

use super::parser::parse_events;
use super::sse::SseDecoder;

use std::sync::Arc;

fn premature_error() -> CanonicalEvent {
    CanonicalEvent::Error {
        status: 502,
        message: "upstream stream ended prematurely without finish_reason; any partial output has been preserved. Send a new request to continue.".into(),
    }
}

/// Retry unexpected SSE truncation once only if no output has been delivered;
/// `resend` re-issues the identical upstream request.
/// Drive one upstream chat SSE to canonical events, retrying an identical
/// request at most once when the stream truncates before any output.
///
/// `response` is the chat SSE body; `fetch_clients` are the proxy-ordered
/// clients for downloading generated images (inline data-URL conversion) —
/// kept separate from `response` because the image CDN is a different host
/// and must respect the same proxy policy as the chat request.
/// `resend` re-issues the identical upstream request on retry.
pub(crate) fn canonical_events_with_retry(
    response: reqwest::Response,
    log: Arc<RequestLogStore>,
    timeout: std::time::Duration,
    fetch_clients: Vec<reqwest::Client>,
    resend: impl FnOnce() -> BoxFuture<'static, Result<reqwest::Response, GatewayError>>
    + Send
    + 'static,
) -> impl Stream<Item = CanonicalEvent> + Send + 'static {
    let id = debug::current_request_id();
    let first = decode_events(response, log.clone(), timeout, fetch_clients.clone());
    stream! {
        let mut output_started = false;
        let mut first = std::pin::pin!(first);
        let mut truncated = false;
        while let Some(result) = first.next().await {
            match result {
                Ok(event) => {
                    let terminal = matches!(event, CanonicalEvent::Finish { .. } | CanonicalEvent::Error { .. });
                    output_started = true;
                    yield event;
                    if terminal { return; }
                }
                Err(()) => { truncated = true; break; }
            }
        }
        if !truncated { return; }
        // Replaying generation cannot reliably reproduce text, reasoning, signatures or tool calls.
        if output_started {
            tracing::warn!("upstream SSE truncated after output started [{id}] — preserving partial output, not retrying");
            yield premature_error();
            return;
        }
        tracing::warn!("upstream SSE truncated before output [{id}] — retrying identical request once");
        let retry_started = std::time::Instant::now();
        tracing::trace!(request_id = %id, attempt = 1, max_retries = 1, "SSE retry: sending identical request before any output");
        let response = match resend().await {
            Ok(response) => {
                tracing::trace!(request_id = %id, attempt = 1, status = %response.status(), elapsed_ms = retry_started.elapsed().as_millis(), "SSE retry: response received");
                response
            }
            Err(error) => {
                tracing::trace!(request_id = %id, attempt = 1, elapsed_ms = retry_started.elapsed().as_millis(), %error, "SSE retry: request failed, no retries remaining");
                let (status, message) = error.status_and_message();
                yield CanonicalEvent::Error { status: status.as_u16(), message };
                return;
            }
        };
        let retry = debug::REQUEST_ID.scope(id.clone(), async { decode_events(response, log, timeout, fetch_clients) }).await;
        let mut retry = std::pin::pin!(retry);
        while let Some(result) = retry.next().await {
            let event = match result {
                Ok(event) => event,
                Err(()) => {
                    tracing::trace!(request_id = %id, attempt = 1, elapsed_ms = retry_started.elapsed().as_millis(), "SSE retry: stream truncated again, no retries remaining");
                    tracing::error!("upstream SSE retry also truncated [{id}] — request failed");
                    yield premature_error();
                    return;
                }
            };
            if let CanonicalEvent::Error { status, message } = &event {
                tracing::trace!(request_id = %id, attempt = 1, status, %message, elapsed_ms = retry_started.elapsed().as_millis(), "SSE retry: upstream reported failure, no retries remaining");
                yield event;
                return;
            }
            let terminal = matches!(event, CanonicalEvent::Finish { .. });
            if terminal {
                tracing::trace!(request_id = %id, attempt = 1, elapsed_ms = retry_started.elapsed().as_millis(), "SSE retry: completed successfully");
            }
            yield event;
            if terminal { return; }
        }
    }
}

/// Download a generated image and wrap it as an inline `data:` URL, so clients
/// that cannot reach `ai-images.raycast.com` (S3/CloudFront, often blocked or
/// geo-fenced) can still render it. Tries the proxy channels in order — same
/// policy as the chat request — so Proxy mode never falls back to direct.
/// Returns a `data:<mime>;base64,...` string; the caller falls back to the
/// raw upstream URL on `Err`.
async fn fetch_image_data_url(
    fetch_clients: &[reqwest::Client],
    url: &str,
) -> Result<String, GatewayError> {
    // 60s / 20 MiB are deliberately independent of the upstream chat timeout:
    // image CDN latency and chat SSE inter-arrival have different profiles.
    const IMAGE_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
    let mut last_error = GatewayError::Internal("no fetch channel configured".into());
    let mut response = None;
    for client in fetch_clients {
        match client
            .get(url)
            .timeout(IMAGE_FETCH_TIMEOUT)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                response = Some(r);
                break;
            }
            // HTTP-level failure: another channel is worth trying (the chat
            // path only rotates on transport errors, but a CDN edge reject
            // is channel-dependent too).
            Ok(r) => {
                last_error = GatewayError::Upstream {
                    status: r.status().as_u16(),
                    message: "image fetch failed".into(),
                };
            }
            Err(error) => last_error = GatewayError::from(error),
        }
    }
    let response = response.ok_or(last_error)?.error_for_status()?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/png")
        .split(';')
        .next()
        .unwrap_or("image/png")
        .trim()
        .to_string();
    // The URL originates from upstream wire data; only inline actual images.
    if !content_type.starts_with("image/") {
        return Err(GatewayError::Internal(format!(
            "generated image fetch returned non-image content type: {content_type}"
        )));
    }
    // Stream the body instead of buffering it whole: the cap must hold even
    // if a redirect target serves something unexpectedly huge.
    use base64::Engine;
    use futures_util::StreamExt;
    let mut bytes: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len() + chunk.len() > MAX_IMAGE_BYTES {
            return Err(GatewayError::Internal(format!(
                "generated image exceeds the {MAX_IMAGE_BYTES}-byte inline cap"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(format!(
        "data:{content_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    ))
}

/// Err(()) marks missing Finish/Error, distinct from an explicit upstream error.
fn decode_events(
    mut response: reqwest::Response,
    log: Arc<RequestLogStore>,
    timeout: std::time::Duration,
    fetch_clients: Vec<reqwest::Client>,
) -> impl Stream<Item = Result<CanonicalEvent, ()>> + Send + 'static {
    // Grab the correlation id *here*: the task-local REQUEST_ID scope is only
    // valid inside the handler, while the stream below is polled outside it.
    // The stream! block is an async move, so both are moved into it.
    let id = debug::current_request_id();
    let attempt = response
        .extensions_mut()
        .remove::<crate::statistics::Attempt>();

    stream! {
        let mut payloads: Vec<String> = Vec::new();
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::new();
        let mut text_log = DeltaLogBuffer::default();
        let mut notifications = Vec::new();
        // 流是否以 Finish/Error 事件正常终结;断流时用来发合成错误。
        let mut stream_terminated = false;
        // 已打印过的通知数:每次增量打印新通知,避免重复刷屏。
        let mut seen_notifications = 0;

        // 读循环的退出原因:EOF / 读超时 / 传输错误,断流时写进日志和 attempt。
        let mut exit_reason = String::from("EOF (upstream closed the connection)");
        loop {
            let chunk = match tokio::time::timeout(timeout, bytes.next()).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(_) => {
                    exit_reason = format!("read timeout (no data for {}s)", timeout.as_secs());
                    break;
                }
            };
            let Ok(chunk) = chunk else {
                // 上游传输层断连(连接重置/代理掐断):流没有 error payload、
                // 没有 finish 就直接没了。必须留痕,否则排查时终端日志
                // 看起来像"正常结束但没内容"。
                let err = crate::error::error_chain(&chunk.unwrap_err());
                tracing::error!(
                    "⇈ upstream transport error [{id}]: {err} — stream ended \
                     prematurely after {} payloads (no finish_reason seen)",
                    payloads.len()
                );
                exit_reason = format!("transport error: {err}");
                break;
            };
            for payload in decoder.push_bytes(&chunk) {
                payloads.push(payload.clone());
                // ponytail: 打印原始 SSE 数据用于调试
                tracing::debug!("upstream SSE payload: {}", payload);
                if let Some(attempt) = &attempt { attempt.observe_payload(&payload); }

                // 上游通知(降级/限额提示等)挂在 notifications 里,只在
                // 出错时才被拼进错误消息。这里把新增的捞出来打到终端,
                // 否则"账号被降级到 luna"这种关键信息终端上完全不可见。
                if notifications.len() > seen_notifications {
                    for n in &notifications[seen_notifications..] {
                        tracing::warn!("⇈ upstream notification [{id}]: {n}");
                    }
                    seen_notifications = notifications.len();
                }

                let events = parse_events(&payload, &mut notifications);
                // Settle before yielding any delta from a payload that also carries Finish.
                if let Some(attempt) = &attempt {
                    for event in &events {
                        match event {
                            CanonicalEvent::Finish { .. } => attempt.finish("completed", None),
                            CanonicalEvent::Error { message, .. } => attempt.finish("failed", Some(message)),
                            _ => {}
                        }
                    }
                }
                for event in events {
                    // The outbound adapters stop polling the stream at `Finish`,
                    // so this generator's tail code never runs on the streaming
                    // path — record as soon as the terminating payload is seen.
                    // Anything after it is `[DONE]` / notifications.
                    if matches!(
                        event,
                        CanonicalEvent::Finish { .. } | CanonicalEvent::Error { .. }
                    ) {
                        stream_terminated = true;
                        log.update_response(&id, &payloads.join("\n"));
                    }
                    // 打印解析后的规范事件:碎片 delta 攒行,其余逐条。
                    match &event {
                        CanonicalEvent::TextDelta(text) => text_log.push("text", text, &id),
                        CanonicalEvent::ReasoningDelta(text) => {
                            text_log.push("reasoning", text, &id)
                        }
                        CanonicalEvent::ThoughtSignature(sig) => {
                            text_log.flush(&id);
                            tracing::info!("→ thought_signature [{id}]: {}", truncate_for_log(sig));
                        }
                        CanonicalEvent::ToolCall(call) => {
                            text_log.flush(&id);
                            tracing::info!("→ tool_call [{id}]: {} (id: {})", call.name, call.id);
                        }
                        CanonicalEvent::ImageGenerationStarted { size, count } => {
                            tracing::info!(
                                "→ image_generation [{id}]: started (size={size:?}, count={count:?})"
                            );
                        }
                        CanonicalEvent::ImageGenerated { url } => {
                            text_log.flush(&id);
                            tracing::info!("→ image generated [{id}]: {url}");
                            // 客户端常无法直接访问 ai-images.raycast.com(境外
                            // S3/CloudFront):下载转 data URL 内联进 markdown,
                            // 客户端无需再发请求。失败时退回原始 URL,图链仍在。
                            match fetch_image_data_url(&fetch_clients, url).await {
                                Ok(data_url) => {
                                    yield Ok(CanonicalEvent::TextDelta(format!(
                                        "\n\n![generated image]({data_url})\n\n"
                                    )));
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        "→ image fetch failed [{id}], falling back to raw URL: {error}"
                                    );
                                    // Angle-bracket link form: survives `)` and
                                    // spaces in URLs (S3 keys can carry both).
                                    yield Ok(CanonicalEvent::TextDelta(format!(
                                        "\n\n![generated image](<{url}>)\n\n"
                                    )));
                                }
                            }
                            continue;
                        }
                        CanonicalEvent::Finish { reason, usage } => {
                            text_log.flush(&id);
                            tracing::info!("→ finish [{id}]: reason={:?}, tokens={{input: {}, output: {}, cached: {}}}",
                                reason, usage.input_tokens, usage.output_tokens, usage.cached_tokens);
                        }
                        CanonicalEvent::Error { status, message } => {
                            text_log.flush(&id);
                            tracing::error!("→ upstream error [{id}]: status={status}, message={message}");
                        }
                    }
                    yield Ok(event);
                }
            }
        }

        // 流结束:刷出残留的缓冲。
        text_log.flush(&id);

        // 断流检测:整个流里既没有 Finish 也没有 Error 事件,就是上游
        // 静默掐断(连接被代理/上游重置)。发一个合成 Error 事件,让
        // 下游适配器能如实把失败传给客户端(Responses 会发
        // `response.failed` 而不是谎报 completed)。
        if !stream_terminated {
            if let Some(attempt) = &attempt {
                attempt.finish(
                    "interrupted",
                    Some(&format!("upstream stream ended without a terminal event: {exit_reason}")),
                );
            }
            tracing::error!(
                "⇈ upstream stream ended without finish [{id}]: {exit_reason} — \
                 synthesizing error event (payloads={})",
                payloads.len()
            );
            log.update_response(&id, &payloads.join("\n"));
            yield Err(());
        }

        // One raw `data:` payload per line, exactly as the upstream sent it
        // (non-streaming aggregation path polls the stream to completion).
        log.update_response(&id, &payloads.join("\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{core::message::ToolCall, storage::db::Db};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn large_output_is_preserved_without_retry() {
        let text = "x".repeat(2 * 1024 * 1024 + 1);
        let events: Vec<_> = canonical_events_with_retry(
            response(&[json!({"text":text})]),
            log(),
            std::time::Duration::from_secs(20),
            vec![reqwest::Client::new()],
            || panic!("oversized output must not be replayed"),
        )
        .collect()
        .await;
        assert_eq!(events[0], CanonicalEvent::TextDelta(text));
        assert!(matches!(
            events.last(),
            Some(CanonicalEvent::Error { status: 502, .. })
        ));
    }

    fn log() -> Arc<RequestLogStore> {
        Arc::new(RequestLogStore::new(Db::open(":memory:").unwrap()))
    }

    fn response(payloads: &[serde_json::Value]) -> reqwest::Response {
        let body: String = payloads.iter().map(|p| format!("data: {p}\n\n")).collect();
        reqwest::Response::from(
            axum::http::Response::builder()
                .status(200)
                .body(reqwest::Body::from(body))
                .unwrap(),
        )
    }

    fn finish() -> serde_json::Value {
        json!({"finish_reason":"stop","usage":{"input_tokens":1,"output_tokens":2}})
    }

    fn text(s: &str) -> CanonicalEvent {
        CanonicalEvent::TextDelta(s.into())
    }

    #[tokio::test]
    async fn statistics_keep_truncated_attempt_separate_from_successful_retry() {
        let state = crate::server::state::compact_test_state();
        let request = crate::core::request::CanonicalRequest::new(
            crate::core::protocol::Protocol::Responses,
            "client",
        );
        let call = crate::statistics::CallContext::new(&request, "openai", "upstream", "compact");
        let first =
            crate::statistics::Attempt::start(state.statistics.clone(), &call, "first@example.com")
                .unwrap();
        let mut first_response = response(&[]);
        first.set_http_status(200);
        first_response.extensions_mut().insert(first);
        let second =
            crate::statistics::Attempt::start(state.statistics.clone(), &call, "retry@example.com")
                .unwrap();
        let mut second_response = response(&[json!({"text":"partial done"}), finish()]);
        second.set_http_status(200);
        second_response.extensions_mut().insert(second);
        let events: Vec<_> = canonical_events_with_retry(
            first_response,
            state.log.clone(),
            std::time::Duration::from_secs(1),
            vec![reqwest::Client::new()],
            move || Box::pin(async move { Ok(second_response) }),
        )
        .collect()
        .await;
        assert!(matches!(events.last(), Some(CanonicalEvent::Finish { .. })));
        let page = state.statistics.list(&Default::default()).unwrap();
        assert_eq!(page.total, 2);
        let interrupted = page
            .items
            .iter()
            .find(|row| row.account_email == "first@example.com")
            .unwrap();
        assert_eq!(interrupted.status, "interrupted");
        assert!(
            interrupted
                .error_message
                .as_deref()
                .unwrap()
                .contains("EOF"),
            "{:?}",
            interrupted.error_message
        );
        assert!(interrupted.usage_json.is_none());
        let completed = page
            .items
            .iter()
            .find(|row| row.account_email == "retry@example.com")
            .unwrap();
        assert_eq!(completed.status, "completed");
        assert!(completed.usage_json.is_some());
        assert_eq!(interrupted.request_id, completed.request_id);
        assert_eq!(completed.call_type, "compact");
    }

    #[tokio::test]
    async fn read_timeout_is_reported_as_exit_reason() {
        let state = crate::server::state::compact_test_state();
        let request = crate::core::request::CanonicalRequest::new(
            crate::core::protocol::Protocol::Responses,
            "client",
        );
        let call = crate::statistics::CallContext::new(&request, "openai", "upstream", "inference");
        let attempt =
            crate::statistics::Attempt::start(state.statistics.clone(), &call, "slow@example.com")
                .unwrap();
        let mut stalled = reqwest::Response::from(
            axum::http::Response::builder()
                .status(200)
                .body(reqwest::Body::wrap_stream(futures_util::stream::pending::<
                    Result<axum::body::Bytes, std::io::Error>,
                >()))
                .unwrap(),
        );
        attempt.set_http_status(200);
        stalled.extensions_mut().insert(attempt);
        // 超时后会重发一次;让重发返回 Err,保持测试只关心首个 attempt。
        let events: Vec<_> = canonical_events_with_retry(
            stalled,
            state.log.clone(),
            std::time::Duration::from_millis(50),
            vec![reqwest::Client::new()],
            || Box::pin(async { Err(GatewayError::Internal("no retry".into())) }),
        )
        .collect()
        .await;
        assert!(matches!(events.last(), Some(CanonicalEvent::Error { .. })));
        let page = state.statistics.list(&Default::default()).unwrap();
        let message = page.items[0].error_message.as_deref().unwrap();
        assert!(message.contains("read timeout"), "{message}");
    }

    #[tokio::test]
    async fn retries_truncation_before_output_once_ignoring_empty_payloads_and_pings() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let retried = attempts.clone();
        let events: Vec<_> = canonical_events_with_retry(
            response(&[
                json!({"reasoning":"", "text":""}),
                json!({"notification_type":"ping", "text":""}),
            ]),
            log(),
            std::time::Duration::from_secs(20),
            vec![reqwest::Client::new()],
            move || {
                Box::pin(async move {
                    retried.fetch_add(1, Ordering::SeqCst);
                    Ok(response(&[
                        json!({"reasoning":"思"}),
                        json!({"reasoning":"考"}),
                        json!({"text":"你"}),
                        json!({"text":"好世界！"}),
                        finish(),
                    ]))
                })
            },
        )
        .collect()
        .await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            &events[..4],
            &[
                CanonicalEvent::ReasoningDelta("思".into()),
                CanonicalEvent::ReasoningDelta("考".into()),
                text("你"),
                text("好世界！")
            ]
        );
        assert!(
            matches!(events.last(), Some(CanonicalEvent::Finish { usage, .. }) if usage.input_tokens == 1 && usage.output_tokens == 2)
        );
    }

    #[tokio::test]
    async fn normal_finish_and_explicit_upstream_error_never_retry() {
        for payloads in [
            vec![json!({"text":"ok"}), finish()],
            vec![json!({"error":{"http_status":502,"message":"explicit error"}})],
        ] {
            let events: Vec<_> = canonical_events_with_retry(
                response(&payloads),
                log(),
                std::time::Duration::from_secs(20),
                vec![reqwest::Client::new()],
                || panic!("must not retry explicit terminal events"),
            )
            .collect()
            .await;
            assert!(matches!(
                events.last(),
                Some(CanonicalEvent::Finish { .. } | CanonicalEvent::Error { .. })
            ));
        }
    }

    #[tokio::test]
    async fn second_truncation_is_failure_without_third_attempt() {
        let events: Vec<_> = canonical_events_with_retry(
            response(&[]),
            log(),
            std::time::Duration::from_secs(20),
            vec![reqwest::Client::new()],
            || Box::pin(async { Ok(response(&[json!({"text":"AB"})])) }),
        )
        .collect()
        .await;
        assert_eq!(events[0], text("AB"));
        assert!(
            matches!(events.last(), Some(CanonicalEvent::Error { status: 502, message }) if message.contains("prematurely"))
        );
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn any_delivered_content_prevents_retry_and_preserves_partial_output() {
        for (payload, expected) in [
            (json!({"text":"半截代码"}), text("半截代码")),
            (
                json!({"reasoning":"thinking"}),
                CanonicalEvent::ReasoningDelta("thinking".into()),
            ),
            (
                json!({"extra_content":{"anthropic":{"thought_signature":"sig"}}}),
                CanonicalEvent::ThoughtSignature("sig".into()),
            ),
            (
                json!({"tool_calls":[{"id":"call_1","name":"edit","arguments":"{}"}]}),
                CanonicalEvent::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "edit".into(),
                    arguments: "{}".into(),
                }),
            ),
        ] {
            let events: Vec<_> = canonical_events_with_retry(
                response(&[payload]),
                log(),
                std::time::Duration::from_secs(20),
                vec![reqwest::Client::new()],
                || panic!("delivered content must never be replayed"),
            )
            .collect()
            .await;
            assert_eq!(events.len(), 2);
            assert_eq!(events[0], expected);
            assert!(
                matches!(&events[1], CanonicalEvent::Error { status: 502, message } if message.contains("prematurely") && message.contains("continue"))
            );
        }
    }

    #[tokio::test]
    async fn retry_connection_failure_or_business_error_is_terminal() {
        for connection_error in [true, false] {
            let events: Vec<_> = canonical_events_with_retry(
                response(&[]),
                log(),
                std::time::Duration::from_secs(20),
                vec![reqwest::Client::new()],
                move || {
                    Box::pin(async move {
                        if connection_error {
                            Err(GatewayError::NoAccount("no account".into()))
                        } else {
                            Ok(response(&[
                                json!({"error":{"http_status":429,"message":"limited"}}),
                            ]))
                        }
                    })
                },
            )
            .collect()
            .await;
            assert!(matches!(events.as_slice(), [CanonicalEvent::Error { .. }]));
        }
    }

    #[tokio::test]
    async fn transport_error_after_output_preserves_partial_log_and_request_id_without_retry() {
        let bytes = futures_util::stream::iter(vec![
            Ok(axum::body::Bytes::from_static(
                b"data: {\"text\":\"A\"}\n\n",
            )),
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "reset",
            )),
        ]);
        let first = reqwest::Response::from(
            axum::http::Response::builder()
                .status(200)
                .body(reqwest::Body::wrap_stream(bytes))
                .unwrap(),
        );
        let store = log();
        store.insert_request("retry-id", "POST", "/v1/responses", &json!({}));
        let events = debug::REQUEST_ID
            .scope("retry-id".into(), async {
                canonical_events_with_retry(
                    first,
                    store.clone(),
                    std::time::Duration::from_secs(20),
                    vec![reqwest::Client::new()],
                    || panic!("transport errors after output must not replay the request"),
                )
            })
            .await;
        let events: Vec<_> = events.collect().await;
        assert_eq!(events, vec![text("A"), premature_error()]);
        let detail = store.detail("retry-id").unwrap();
        assert_eq!(detail.response_text.unwrap(), r#"{"text":"A"}"#);
    }

    #[tokio::test]
    async fn read_timeout_retries_only_before_output_and_preserves_request_id() {
        for partial in [false, true] {
            let chunk = if partial {
                "data: {\"text\":\"A\"}\n\n"
            } else {
                "data: {\"notification_type\":\"ping\"}\n\n"
            };
            let bytes = futures_util::stream::iter([Ok::<_, std::io::Error>(
                axum::body::Bytes::from(chunk),
            )])
            .chain(futures_util::stream::pending());
            let first = reqwest::Response::from(
                axum::http::Response::builder()
                    .status(200)
                    .body(reqwest::Body::wrap_stream(bytes))
                    .unwrap(),
            );
            let store = log();
            store.insert_request("timeout-id", "POST", "/v1/responses", &json!({}));
            let events = debug::REQUEST_ID
                .scope("timeout-id".into(), async {
                    canonical_events_with_retry(
                        first,
                        store.clone(),
                        std::time::Duration::from_millis(5),
                        vec![reqwest::Client::new()],
                        move || {
                            Box::pin(async move {
                                assert!(!partial, "read timeout after output must not retry");
                                Ok(response(&[json!({"text":"recovered"}), finish()]))
                            })
                        },
                    )
                })
                .await;
            let events: Vec<_> = events.collect().await;
            if partial {
                assert_eq!(events, vec![text("A"), premature_error()]);
                assert_eq!(
                    store.detail("timeout-id").unwrap().response_text.unwrap(),
                    r#"{"text":"A"}"#
                );
            } else {
                assert_eq!(events[0], text("recovered"));
                assert!(matches!(events.last(), Some(CanonicalEvent::Finish { .. })));
                assert!(
                    store
                        .detail("timeout-id")
                        .unwrap()
                        .response_text
                        .unwrap()
                        .contains("recovered")
                );
            }
        }
    }

    #[tokio::test]
    async fn dropping_first_stream_never_starts_retry() {
        let events = canonical_events_with_retry(
            response(&[json!({"text":"A"})]),
            log(),
            std::time::Duration::from_secs(20),
            vec![reqwest::Client::new()],
            || panic!("cancelled stream must not retry"),
        );
        let mut events = Box::pin(events);
        assert_eq!(events.next().await, Some(text("A")));
        drop(events);
    }

    #[tokio::test]
    async fn responses_adapter_completes_retry_before_output_and_reports_partial_stream_failure() {
        use crate::protocol::{OutboundAdapter, responses::ResponsesOutbound};
        for partial in [false, true] {
            let events = canonical_events_with_retry(
                response(&if partial {
                    vec![json!({"text":"A"})]
                } else {
                    vec![]
                }),
                log(),
                std::time::Duration::from_secs(20),
                vec![reqwest::Client::new()],
                move || {
                    Box::pin(async move {
                        assert!(!partial, "partial output must not be replayed");
                        Ok(response(&[json!({"text":"AB"}), finish()]))
                    })
                },
            );
            let rendered = ResponsesOutbound::stream("model", events);
            let bytes = axum::body::to_bytes(rendered.into_body(), usize::MAX)
                .await
                .unwrap();
            let frames: Vec<serde_json::Value> = std::str::from_utf8(&bytes)
                .unwrap()
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .map(|s| serde_json::from_str(s).unwrap())
                .collect();
            let terminal = frames.last().unwrap();
            if partial {
                assert_eq!(terminal["type"], "response.failed");
                assert!(!frames.iter().any(|f| f["type"] == "response.completed"));
                assert!(
                    frames
                        .iter()
                        .any(|f| f["type"] == "response.output_text.delta" && f["delta"] == "A")
                );
                assert!(
                    terminal["response"]["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("continue")
                );
            } else {
                assert_eq!(terminal["type"], "response.completed");
                assert_eq!(
                    terminal["response"]["output"][0]["content"][0]["text"],
                    "AB"
                );
                assert_eq!(terminal["response"]["usage"]["output_tokens"], 2);
            }
        }
    }
}
