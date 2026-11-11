//! Bridges the Raycast SSE transport to the canonical event stream.
//!
//! [`canonical_events`] owns the upstream [`reqwest::Response`] and yields a
//! provider-agnostic [`CanonicalEvent`] stream: it feeds the raw byte chunks
//! through the [`SseDecoder`], then maps each decoded `data:` payload with the
//! [`parser`](super::parser). This is the single place the byte transport meets
//! component ⑦, so every consumer — the streaming outbound adapters and the
//! non-streaming aggregation alike — observes exactly the same event sequence.
//!
//! A mid-stream transport error simply ends the stream, keeping the events
//! decoded so far. This matches the previous inline handler: the streaming path
//! stops emitting, and the non-streaming path returns whatever it aggregated.

use async_stream::stream;
use futures_util::{Stream, StreamExt};

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
                    yield event;
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
            yield CanonicalEvent::Error {
                status: 502,
                message: "upstream stream ended prematurely (connection closed \
                          without finish_reason; likely dropped by proxy or \
                          upstream)"
                    .to_string(),
            };
        }

        // One raw `data:` payload per line, exactly as the upstream sent it
        // (non-streaming aggregation path polls the stream to completion).
        log.update_response(&id, &payloads.join("\n"));
    }
}
