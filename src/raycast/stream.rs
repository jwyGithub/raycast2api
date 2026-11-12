//! Bridges the Raycast SSE transport to the canonical event stream.
//!
//! [`canonical_events`] owns the upstream [`reqwest::Response`] and yields a
//! provider-agnostic [`CanonicalEvent`] stream: it feeds the raw byte chunks
//! through the [`SseDecoder`], then maps each decoded `data:` payload with the
//! [`parser`](super::parser). This is the single place the byte transport meets
//! component ⑦, so every consumer — the streaming outbound adapters and the
//! non-streaming aggregation alike — observes exactly the same event sequence.
//!
//! Missing terminal events are distinguished from explicit upstream errors so
//! callers can retry a truncated response once without replaying delivered output.

use super::{client::RaycastClient, wire::RaycastChatRequest};
use crate::error::GatewayError;
use async_stream::stream;
use futures_util::{Stream, StreamExt, future::BoxFuture};
use std::collections::VecDeque;

use crate::core::event::CanonicalEvent;
use crate::debug;
use crate::log::format::{DeltaLogBuffer, truncate_for_log};
use crate::log::store::RequestLogStore;

use super::parser::parse_events;
use super::sse::SseDecoder;

use std::sync::Arc;

/// Decode a Raycast chat SSE response into a stream of [`CanonicalEvent`]s.
pub fn canonical_events(
    response: reqwest::Response,
    log: Arc<RequestLogStore>,
) -> impl Stream<Item = CanonicalEvent> {
    decode_events(response, log).map(|event| event.unwrap_or_else(|()| premature_error()))
}

/// Retry only unexpected SSE truncation, using the exact same upstream body.
pub fn canonical_events_with_retry(
    response: reqwest::Response,
    client: RaycastClient,
    request: RaycastChatRequest,
    log: Arc<RequestLogStore>,
) -> impl Stream<Item = CanonicalEvent> + Send + 'static {
    let id = debug::current_request_id();
    retry_once(response, log, move || {
        Box::pin(async move {
            debug::REQUEST_ID
                .scope(id, client.send_chat(&request))
                .await
        })
    })
}

fn premature_error() -> CanonicalEvent {
    CanonicalEvent::Error {
        status: 502,
        message: "upstream stream ended prematurely (connection closed without finish_reason; likely dropped by proxy or upstream)".into(),
    }
}

fn retry_once(
    response: reqwest::Response,
    log: Arc<RequestLogStore>,
    resend: impl FnOnce() -> BoxFuture<'static, Result<reqwest::Response, GatewayError>>
    + Send
    + 'static,
) -> impl Stream<Item = CanonicalEvent> + Send + 'static {
    let id = debug::current_request_id();
    let first = decode_events(response, log.clone());
    stream! {
        let mut delivered = VecDeque::new();
        let mut first = std::pin::pin!(first);
        let mut truncated = false;
        while let Some(result) = first.next().await {
            match result {
                Ok(event) => {
                    let terminal = matches!(event, CanonicalEvent::Finish { .. } | CanonicalEvent::Error { .. });
                    delivered.push_back(event.clone());
                    yield event;
                    if terminal { return; }
                }
                Err(()) => { truncated = true; break; }
            }
        }
        if !truncated { return; }
        tracing::warn!("upstream SSE truncated [{id}] — retrying identical request once");
        let retry_started = std::time::Instant::now();
        tracing::trace!(request_id = %id, attempt = 1, max_retries = 1, delivered_events = delivered.len(), "SSE retry: sending identical request");
        let response = match resend().await {
            Ok(response) => {
                tracing::trace!(request_id = %id, attempt = 1, status = %response.status(), elapsed_ms = retry_started.elapsed().as_millis(), "SSE retry: response received, checking delivered prefix");
                response
            }
            Err(error) => {
                tracing::trace!(request_id = %id, attempt = 1, elapsed_ms = retry_started.elapsed().as_millis(), %error, "SSE retry: request failed, no retries remaining");
                let (status, message) = error.status_and_message();
                yield CanonicalEvent::Error { status: status.as_u16(), message };
                return;
            }
        };
        let retry = debug::REQUEST_ID.scope(id.clone(), async { decode_events(response, log) }).await;
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
            let checking_prefix = !delivered.is_empty();
            match skip_delivered(&mut delivered, event) {
                Ok(Some(event)) => {
                    if checking_prefix {
                        tracing::trace!(request_id = %id, attempt = 1, "SSE retry: delivered prefix matched, forwarding new output");
                    }
                    let terminal = matches!(event, CanonicalEvent::Finish { .. });
                    if terminal {
                        tracing::trace!(request_id = %id, attempt = 1, elapsed_ms = retry_started.elapsed().as_millis(), "SSE retry: completed successfully");
                    }
                    yield event;
                    if terminal { return; }
                }
                Ok(None) => {
                    tracing::trace!(request_id = %id, attempt = 1, remaining_events = delivered.len(), "SSE retry: skipped matching prefix chunk");
                    if checking_prefix && delivered.is_empty() {
                        tracing::trace!(request_id = %id, attempt = 1, "SSE retry: delivered prefix matched, waiting for new output");
                    }
                }
                Err(()) => {
                    tracing::trace!(request_id = %id, attempt = 1, remaining_events = delivered.len(), elapsed_ms = retry_started.elapsed().as_millis(), "SSE retry: prefix mismatch, no retries remaining");
                    tracing::error!("upstream SSE retry prefix differs [{id}] — request failed");
                    yield CanonicalEvent::Error {
                        status: 502,
                        message: "upstream SSE retry differed from output already delivered; cannot safely resume".into(),
                    };
                    return;
                }
            }
        }
    }
}

/// Compare ordered events while allowing different text/reasoning chunk sizes.
fn skip_delivered(
    delivered: &mut VecDeque<CanonicalEvent>,
    mut event: CanonicalEvent,
) -> Result<Option<CanonicalEvent>, ()> {
    loop {
        let Some(expected) = delivered.front_mut() else {
            return Ok(Some(event));
        };
        match (expected, &mut event) {
            (CanonicalEvent::TextDelta(expected), CanonicalEvent::TextDelta(actual))
            | (CanonicalEvent::ReasoningDelta(expected), CanonicalEvent::ReasoningDelta(actual)) => {
                let matched = expected.len().min(actual.len());
                if expected.as_bytes()[..matched] != actual.as_bytes()[..matched] {
                    return Err(());
                }
                expected.drain(..matched);
                actual.drain(..matched);
                let consumed = expected.is_empty();
                let exhausted = actual.is_empty();
                if consumed {
                    delivered.pop_front();
                }
                if exhausted {
                    return Ok(None);
                }
            }
            (expected, actual) if expected == actual => {
                delivered.pop_front();
                return Ok(None);
            }
            _ => return Err(()),
        }
    }
}

/// Err(()) marks missing Finish/Error, distinct from an explicit upstream error.
fn decode_events(
    response: reqwest::Response,
    log: Arc<RequestLogStore>,
) -> impl Stream<Item = Result<CanonicalEvent, ()>> + Send + 'static {
    // Grab the correlation id *here*: the task-local REQUEST_ID scope is only
    // valid inside the handler, while the stream below is polled outside it.
    // The stream! block is an async move, so both are moved into it.
    let id = debug::current_request_id();

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

        while let Some(chunk) = bytes.next().await {
            let Ok(chunk) = chunk else {
                // 上游传输层断连(连接重置/代理掐断):流没有 error payload、
                // 没有 finish 就直接没了。必须留痕,否则排查时终端日志
                // 看起来像"正常结束但没内容"。
                let err = chunk.unwrap_err();
                tracing::error!(
                    "⇈ upstream transport error [{id}]: {err} — stream ended \
                     prematurely after {} payloads (no finish_reason seen)",
                    payloads.len()
                );
                break;
            };
            for payload in decoder.push_bytes(&chunk) {
                payloads.push(payload.clone());
                // ponytail: 打印原始 SSE 数据用于调试
                tracing::debug!("upstream SSE payload: {}", payload);

                // 上游通知(降级/限额提示等)挂在 notifications 里,只在
                // 出错时才被拼进错误消息。这里把新增的捞出来打到终端,
                // 否则"账号被降级到 luna"这种关键信息终端上完全不可见。
                if notifications.len() > seen_notifications {
                    for n in &notifications[seen_notifications..] {
                        tracing::warn!("⇈ upstream notification [{id}]: {n}");
                    }
                    seen_notifications = notifications.len();
                }

                for event in parse_events(&payload, &mut notifications) {
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
            tracing::error!(
                "⇈ upstream stream ended without finish [{id}] — synthesizing \
                 error event (payloads={})",
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
    use crate::{
        core::{message::ToolCall, response::FinishReason, usage::Usage},
        db::Db,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
    async fn retries_truncation_once_and_skips_prefix_across_chunk_boundaries() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let retried = attempts.clone();
        let events: Vec<_> = retry_once(
            response(&[
                json!({"reasoning":"思考"}),
                json!({"text":"你好"}),
                json!({"text":"世界"}),
            ]),
            log(),
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
                CanonicalEvent::ReasoningDelta("思考".into()),
                text("你好"),
                text("世界"),
                text("！")
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
            let events: Vec<_> = retry_once(response(&payloads), log(), || {
                panic!("must not retry explicit terminal events")
            })
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
        let events: Vec<_> = retry_once(response(&[json!({"text":"A"})]), log(), || {
            Box::pin(async { Ok(response(&[json!({"text":"AB"})])) })
        })
        .collect()
        .await;
        assert_eq!(&events[..2], &[text("A"), text("B")]);
        assert!(
            matches!(events.last(), Some(CanonicalEvent::Error { status: 502, message }) if message.contains("prematurely"))
        );
        assert_eq!(events.len(), 3);
    }

    #[tokio::test]
    async fn retry_divergence_or_shorter_answer_fails_before_emitting_bad_content() {
        for payload in ["AX", "A"] {
            let events: Vec<_> = retry_once(response(&[json!({"text":"AB"})]), log(), move || {
                Box::pin(async move { Ok(response(&[json!({"text":payload}), finish()])) })
            })
            .collect()
            .await;
            assert_eq!(events.len(), 2);
            assert_eq!(events[0], text("AB"));
            assert!(
                matches!(&events[1], CanonicalEvent::Error { status: 502, message } if message.contains("differed"))
            );
        }
    }

    #[tokio::test]
    async fn retry_connection_failure_or_business_error_is_terminal() {
        for connection_error in [true, false] {
            let events: Vec<_> = retry_once(response(&[]), log(), move || {
                Box::pin(async move {
                    if connection_error {
                        Err(GatewayError::NoAccount("no account".into()))
                    } else {
                        Ok(response(&[
                            json!({"error":{"http_status":429,"message":"limited"}}),
                        ]))
                    }
                })
            })
            .collect()
            .await;
            assert!(matches!(events.as_slice(), [CanonicalEvent::Error { .. }]));
        }
    }

    #[test]
    fn tools_signatures_and_event_order_must_match_exactly() {
        let call = CanonicalEvent::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "edit".into(),
            arguments: "{}".into(),
        });
        let signature = CanonicalEvent::ThoughtSignature("signature".into());
        let mut delivered = VecDeque::from([signature.clone(), call.clone()]);
        assert_eq!(skip_delivered(&mut delivered, signature), Ok(None));
        assert_eq!(skip_delivered(&mut delivered, call.clone()), Ok(None));
        assert!(delivered.is_empty());
        let mut delivered = VecDeque::from([call]);
        assert_eq!(
            skip_delivered(
                &mut delivered,
                CanonicalEvent::ToolCall(ToolCall {
                    id: "different".into(),
                    name: "edit".into(),
                    arguments: "{}".into()
                })
            ),
            Err(())
        );
        let mut delivered = VecDeque::from([text("A")]);
        assert_eq!(
            skip_delivered(&mut delivered, CanonicalEvent::ReasoningDelta("A".into())),
            Err(())
        );
    }

    #[tokio::test]
    async fn transport_error_is_retried_and_request_id_is_preserved() {
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
                retry_once(first, store.clone(), || {
                    Box::pin(async { Ok(response(&[json!({"text":"AB"}), finish()])) })
                })
            })
            .await;
        let events: Vec<_> = events.collect().await;
        assert_eq!(&events[..2], &[text("A"), text("B")]);
        assert!(matches!(events.last(), Some(CanonicalEvent::Finish { .. })));
        let detail = store.detail("retry-id").unwrap();
        assert!(detail.response_text.unwrap().contains("AB"));
    }

    #[tokio::test]
    async fn dropping_first_stream_never_starts_retry() {
        let events = retry_once(response(&[json!({"text":"A"})]), log(), || {
            panic!("cancelled stream must not retry")
        });
        let mut events = Box::pin(events);
        assert_eq!(events.next().await, Some(text("A")));
        drop(events);
    }

    #[test]
    fn matching_complete_prefix_allows_finish() {
        let mut delivered = VecDeque::from([text("A")]);
        assert_eq!(skip_delivered(&mut delivered, text("A")), Ok(None));
        let finish = CanonicalEvent::Finish {
            reason: FinishReason::Stop,
            usage: Usage::default(),
        };
        assert_eq!(
            skip_delivered(&mut delivered, finish.clone()),
            Ok(Some(finish))
        );
    }
    #[tokio::test]
    async fn responses_adapter_completes_recovered_stream_and_fails_diverged_stream() {
        use crate::protocol::{OutboundAdapter, responses::ResponsesOutbound};
        for divergent in [false, true] {
            let events = retry_once(response(&[json!({"text":"A"})]), log(), move || {
                Box::pin(async move {
                    Ok(response(&[
                        json!({"text": if divergent { "X" } else { "AB" }}),
                        finish(),
                    ]))
                })
            });
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
            if divergent {
                assert_eq!(terminal["type"], "response.failed");
                assert!(!frames.iter().any(|f| f["type"] == "response.completed"));
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
