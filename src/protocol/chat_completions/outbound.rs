//! ⑤ OpenAI Chat Completions outbound: canonical output → OpenAI JSON / SSE.
//!
//! This is the streaming/aggregation logic that previously lived inline in the
//! handler, now expressed over [`CanonicalEvent`]s so it is fed by the shared
//! pipeline. The plain-text path stays byte-compatible with the original
//! implementation; on top of that this stage renders tool calls (as OpenAI
//! `tool_calls`), maps real token usage (`prompt_tokens` / `completion_tokens`,
//! plus `prompt_tokens_details.cached_tokens` when present), and reports the
//! canonical finish reason. Reasoning / thought-signature deltas stay off the
//! byte-compatible chat/completions stream (available as an opt-in extension).

use std::convert::Infallible;

use async_stream::stream;
use axum::Json;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::{Stream, StreamExt};

use crate::core::event::CanonicalEvent;
use crate::core::response::{CanonicalResponse, FinishReason};
use crate::core::usage::Usage as CanonicalUsage;
use crate::error::GatewayError;
use crate::protocol::OutboundAdapter;
use crate::util::{new_uuid, unix_secs};

use super::wire::{
    ChatCompletionChunk, ChatCompletionResponse, PromptTokensDetails, ToolCall, Usage,
};

/// Zero-sized marker selecting the OpenAI Chat Completions outbound rendering.
pub struct ChatCompletionsOutbound;

impl OutboundAdapter for ChatCompletionsOutbound {
    fn render_complete(model: &str, response: &CanonicalResponse) -> Response {
        // Map canonical tool calls into OpenAI's `tool_calls` array.
        let tool_calls: Vec<ToolCall> = response
            .tool_calls
            .iter()
            .map(|call| {
                ToolCall::function(call.id.clone(), call.name.clone(), call.arguments.clone())
            })
            .collect();

        // OpenAI carries a null `content` on a tool-call turn; otherwise the
        // aggregated assistant text.
        let content = if tool_calls.is_empty() {
            Some(response.text.clone())
        } else {
            None
        };

        let body = ChatCompletionResponse::new(
            format!("chatcmpl-{}", new_uuid()),
            unix_secs(),
            model.to_string(),
            content,
            tool_calls,
            finish_reason_str(response.finish_reason),
            map_usage(response.usage),
        );
        Json(body).into_response()
    }

    fn stream<S>(model: &str, events: S) -> Response
    where
        S: Stream<Item = CanonicalEvent> + Send + 'static,
    {
        let id = format!("chatcmpl-{}", new_uuid());
        let created = unix_secs();
        let model = model.to_string();

        let sse = stream! {
            let mut events = Box::pin(events);
            // The `assistant` role is announced once, on the first emitted
            // chunk (folded into the first text or tool-call delta, or a lone
            // role delta if the completion finishes with no output).
            let mut role_sent = false;
            // Each tool call occupies its own slot in the streamed `tool_calls`
            // array.
            let mut tool_index: u32 = 0;

            while let Some(event) = events.next().await {
                match event {
                    CanonicalEvent::TextDelta(text) => {
                        let role = (!role_sent).then_some("assistant");
                        role_sent = true;
                        let chunk =
                            ChatCompletionChunk::delta(&id, created, &model, role, Some(text));
                        yield sse_json(&chunk);
                    }
                    CanonicalEvent::ToolCall(call) => {
                        let role = (!role_sent).then_some("assistant");
                        role_sent = true;
                        let chunk = ChatCompletionChunk::tool_call_delta(
                            &id,
                            created,
                            &model,
                            role,
                            tool_index,
                            call.id,
                            call.name,
                            call.arguments,
                        );
                        tool_index += 1;
                        yield sse_json(&chunk);
                    }
                    CanonicalEvent::Finish { reason, usage } => {
                        if !role_sent {
                            let chunk = ChatCompletionChunk::delta(
                                &id,
                                created,
                                &model,
                                Some("assistant"),
                                None,
                            );
                            yield sse_json(&chunk);
                        }
                        let finish = ChatCompletionChunk::finish(
                            &id,
                            created,
                            &model,
                            finish_reason_str(reason),
                        );
                        yield sse_json(&finish);
                        // Emit a trailing usage-only chunk so streaming clients
                        // get the same token accounting as the non-streaming
                        // response (same `map_usage`, incl. cached tokens).
                        let usage_chunk =
                            ChatCompletionChunk::usage(&id, created, &model, map_usage(usage));
                        yield sse_json(&usage_chunk);
                        break;
                    }
                    CanonicalEvent::Error { status, message } => {
                        // Terminal in-stream upstream failure: OpenAI streams
                        // report this as an `error` data event instead of a
                        // final chunk, so the client sees the real cause.
                        let err = serde_json::json!({
                            "error": {
                                "message": message,
                                "type": "upstream_error",
                                "code": status.to_string(),
                            }
                        });
                        yield Ok(Event::default().data(err.to_string()));
                        return;
                    }
                    // Reasoning and thought signatures stay off the byte-compatible
                    // chat/completions stream (available as an opt-in extension).
                    _ => {}
                }
            }

            yield Ok(Event::default().data("[DONE]"));
        };

        Sse::new(sse).into_response()
    }

    fn render_error(error: GatewayError) -> Response {
        // OpenAI clients already expect the gateway's `{ "error": { ... } }`
        // shape, which is exactly what `GatewayError` renders.
        error.into_response()
    }
}

/// Map the canonical finish reason to the OpenAI wire value.
fn finish_reason_str(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::ToolCalls => "tool_calls",
    }
}

/// Map canonical token usage into the OpenAI `usage` shape.
/// `prompt_tokens_details` is attached only when the provider reported cached
/// tokens, so the common response keeps its original three-field usage object.
fn map_usage(usage: CanonicalUsage) -> Usage {
    Usage {
        prompt_tokens: usage.input_tokens,
        completion_tokens: usage.output_tokens,
        total_tokens: usage.input_tokens.saturating_add(usage.output_tokens),
        prompt_tokens_details: (usage.cached_tokens > 0).then_some(PromptTokensDetails {
            cached_tokens: usage.cached_tokens,
        }),
    }
}

/// Serialize a chunk into an SSE event. Serialization of these fixed structs
/// cannot fail, so on the impossible error path we emit an empty event.
fn sse_json(chunk: &ChatCompletionChunk) -> Result<Event, Infallible> {
    let data = serde_json::to_string(chunk).unwrap_or_default();
    Ok(Event::default().data(data))
}
