//! ⑤ OpenAI Chat Completions outbound: canonical output → OpenAI JSON / SSE.
//!
//! Renders text, function calls, finish reasons and usage from the shared
//! pipeline. Stream usage is opt-in; reasoning and thought signatures are
//! deliberately omitted from the Chat wire contract.

use std::convert::Infallible;

use async_stream::stream;
use axum::Json;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::{Stream, StreamExt};

use crate::core::event::CanonicalEvent;
use crate::core::request::CanonicalRequest;
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

        let content = if tool_calls.is_empty() || !response.text.is_empty() {
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
        Self::stream_impl(model, false, events)
    }

    fn stream_with_request<S>(model: &str, request: &CanonicalRequest, events: S) -> Response
    where
        S: Stream<Item = CanonicalEvent> + Send + 'static,
    {
        Self::stream_impl(model, request.include_usage, events)
    }

    fn render_error(error: GatewayError) -> Response {
        super::error::render(error)
    }
}

impl ChatCompletionsOutbound {
    fn stream_impl<S>(model: &str, include_usage: bool, events: S) -> Response
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
                        yield sse_json(&chunk, include_usage);
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
                        yield sse_json(&chunk, include_usage);
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
                            yield sse_json(&chunk, include_usage);
                        }
                        let finish = ChatCompletionChunk::finish(
                            &id,
                            created,
                            &model,
                            finish_reason_str(reason),
                        );
                        yield sse_json(&finish, include_usage);
                        if include_usage {
                            let usage_chunk =
                                ChatCompletionChunk::usage(&id, created, &model, map_usage(usage));
                            yield sse_json(&usage_chunk, true);
                        }
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
                    // Chat has no standard reasoning / thought-signature delta.
                    _ => {}
                }
            }

            yield Ok(Event::default().data("[DONE]"));
        };

        Sse::new(sse).into_response()
    }
}

/// Map the canonical finish reason to the OpenAI wire value.
fn finish_reason_str(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::Length => "length",
        FinishReason::ContentFilter => "content_filter",
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

/// Requested usage is null on normal chunks and populated on the final chunk.
fn sse_json(chunk: &ChatCompletionChunk, include_usage: bool) -> Result<Event, Infallible> {
    let mut value = serde_json::to_value(chunk).expect("chat chunk is serializable");
    if include_usage && chunk.usage.is_none() {
        value["usage"] = serde_json::Value::Null;
    }
    let data = value.to_string();
    Ok(Event::default().data(data))
}
