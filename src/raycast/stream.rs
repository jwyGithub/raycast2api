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

        while let Some(chunk) = bytes.next().await {
            let Ok(chunk) = chunk else { break };
            for payload in decoder.push_bytes(&chunk) {
                payloads.push(payload.clone());
                // ponytail: 打印原始 SSE 数据用于调试
                tracing::debug!("upstream SSE payload: {}", payload);

                for event in parse_events(&payload) {
                    // The outbound adapters stop polling the stream at `Finish`,
                    // so this generator's tail code never runs on the streaming
                    // path — record as soon as the terminating payload is seen.
                    // Anything after it is `[DONE]` / notifications.
                    if matches!(
                        event,
                        CanonicalEvent::Finish { .. } | CanonicalEvent::Error { .. }
                    ) {
                        log.update_response(&id, &payloads.join("\n"));
                    }
                    // 打印解析后的规范事件
                    match &event {
                        CanonicalEvent::TextDelta(text) => {
                            tracing::info!("→ text [{id}]: {}", text);
                        }
                        CanonicalEvent::ReasoningDelta(text) => {
                            tracing::info!("→ reasoning [{id}]: {}", text);
                        }
                        CanonicalEvent::ThoughtSignature(sig) => {
                            tracing::info!("→ thought_signature [{id}]: {}", sig);
                        }
                        CanonicalEvent::ToolCall(call) => {
                            tracing::info!("→ tool_call [{id}]: {} (id: {})", call.name, call.id);
                        }
                        CanonicalEvent::Finish { reason, usage } => {
                            tracing::info!("→ finish [{id}]: reason={:?}, tokens={{input: {}, output: {}, cached: {}}}",
                                reason, usage.input_tokens, usage.output_tokens, usage.cached_tokens);
                        }
                        CanonicalEvent::Error { status, message } => {
                            tracing::error!("→ upstream error [{id}]: status={status}, message={message}");
                        }
                    }
                    yield event;
                }
            }
        }

        // One raw `data:` payload per line, exactly as the upstream sent it
        // (non-streaming aggregation path polls the stream to completion).
        log.update_response(&id, &payloads.join("\n"));
    }
}
