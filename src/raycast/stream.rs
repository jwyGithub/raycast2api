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

use super::parser::parse_events;
use super::sse::SseDecoder;

/// Decode a Raycast chat SSE response into a stream of [`CanonicalEvent`]s.
pub fn canonical_events(response: reqwest::Response) -> impl Stream<Item = CanonicalEvent> {
    stream! {
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::new();

        while let Some(chunk) = bytes.next().await {
            let Ok(chunk) = chunk else { break };
            for payload in decoder.push_bytes(&chunk) {
                // ponytail: 打印原始 SSE 数据用于调试
                tracing::debug!("upstream SSE payload: {}", payload);

                for event in parse_events(&payload) {
                    // 打印解析后的规范事件
                    match &event {
                        CanonicalEvent::TextDelta(text) => {
                            tracing::info!("→ text: {}", text);
                        }
                        CanonicalEvent::ReasoningDelta(text) => {
                            tracing::info!("→ reasoning: {}", text);
                        }
                        CanonicalEvent::ThoughtSignature(sig) => {
                            tracing::info!("→ thought_signature: {}", sig);
                        }
                        CanonicalEvent::ToolCall(call) => {
                            tracing::info!("→ tool_call: {} (id: {})", call.name, call.id);
                        }
                        CanonicalEvent::Finish { reason, usage } => {
                            tracing::info!("→ finish: reason={:?}, tokens={{input: {}, output: {}, cached: {}}}",
                                reason, usage.input_tokens, usage.output_tokens, usage.cached_tokens);
                        }
                    }
                    yield event;
                }
            }
        }
    }
}
