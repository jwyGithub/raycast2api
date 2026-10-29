//! ⑤ Anthropic Messages outbound: canonical output → Anthropic JSON / SSE.
//!
//! Two rendering paths, both fed by the shared pipeline:
//!
//! * **non-streaming** ([`message_response`]) folds a [`CanonicalResponse`] into
//!   one `Message` object whose `content[]` is an optional `thinking` block
//!   (with signature), an optional `text` block, then any `tool_use` blocks.
//! * **streaming** ([`StreamEncoder`]) turns the canonical event stream into
//!   Anthropic's block-structured SSE sequence: `message_start` →
//!   (`content_block_start` / `content_block_delta` / `content_block_stop`)* →
//!   `message_delta` (with `stop_reason` + `usage`) → `message_stop`.
//!
//! The encoder is a small state machine kept separate from the async plumbing so
//! its exact frame sequence can be unit-tested directly.
//!
//! Note: Raycast reports `usage` only at the end of the stream, so
//! `message_start` advertises `input_tokens: 0` and the real token counts land
//! in the terminating `message_delta` (`output_tokens`).

use std::convert::Infallible;

use async_stream::stream;
use axum::Json;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};

use crate::core::event::CanonicalEvent;
use crate::core::response::{CanonicalResponse, FinishReason};
use crate::core::usage::Usage as CanonicalUsage;
use crate::error::GatewayError;
use crate::protocol::OutboundAdapter;
use crate::util::new_uuid;

use super::wire::{MessageResponse, OutputBlock, OutputUsage};

/// Zero-sized marker selecting the Anthropic Messages outbound rendering.
pub struct MessagesOutbound;

impl OutboundAdapter for MessagesOutbound {
    fn render_complete(model: &str, response: &CanonicalResponse) -> Response {
        Json(message_response(model, response)).into_response()
    }

    fn stream<S>(model: &str, events: S) -> Response
    where
        S: Stream<Item = CanonicalEvent> + Send + 'static,
    {
        let id = format!("msg_{}", new_uuid());
        let model = model.to_string();

        let sse = stream! {
            let mut encoder = StreamEncoder::new();
            yield to_event(&encoder.message_start(&id, &model));

            let mut events = Box::pin(events);
            // Set when an in-stream upstream error terminated the loop: the
            // terminal `error` frame is already emitted, so `finalize` (which
            // would append message_delta/message_stop) must be skipped.
            let mut failed = false;
            while let Some(event) = events.next().await {
                let finished = matches!(event, CanonicalEvent::Finish { .. });
                failed |= matches!(event, CanonicalEvent::Error { .. });
                for frame in encoder.push(event) {
                    yield to_event(&frame);
                }
                if finished || failed {
                    break;
                }
            }

            // On an in-stream error the encoder already emitted the terminal
            // `error` frame — skip `finalize` so no trailing message frames
            // follow it.
            if !failed {
                for frame in encoder.finalize() {
                    yield to_event(&frame);
                }
            }

            for frame in encoder.finalize() {
                yield to_event(&frame);
            }
        };

        Sse::new(sse).into_response()
    }

    fn render_error(error: GatewayError) -> Response {
        super::error::render(error)
    }
}

/// Fold a fully-aggregated canonical response into an Anthropic `Message`.
pub fn message_response(model: &str, response: &CanonicalResponse) -> MessageResponse {
    let mut content = Vec::new();

    if !response.reasoning.is_empty() {
        content.push(OutputBlock::Thinking {
            thinking: response.reasoning.clone(),
            signature: response.thought_signature.clone(),
        });
    }
    if !response.text.is_empty() {
        content.push(OutputBlock::Text {
            text: response.text.clone(),
        });
    }
    for call in &response.tool_calls {
        content.push(OutputBlock::ToolUse {
            id: call.id.clone(),
            name: call.name.clone(),
            input: parse_arguments(&call.arguments),
        });
    }
    // A `Message` always carries at least one content block.
    if content.is_empty() {
        content.push(OutputBlock::Text {
            text: String::new(),
        });
    }

    MessageResponse {
        id: format!("msg_{}", new_uuid()),
        kind: "message",
        role: "assistant",
        model: model.to_string(),
        content,
        stop_reason: stop_reason(response.finish_reason),
        stop_sequence: None,
        usage: OutputUsage {
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            // Raycast only reports a single cached-token count, which is the
            // portion of the prompt read from cache. Cache creation is not
            // reported, so it stays 0.
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: response.usage.cached_tokens,
        },
    }
}

/// One Anthropic SSE frame: a named `event:` and its JSON `data:` payload.
#[derive(Debug, Clone)]
pub struct SseFrame {
    pub event: &'static str,
    pub data: Value,
}

/// Incremental encoder from canonical events to Anthropic's SSE frame sequence.
///
/// Anthropic requires each content block to be bracketed by
/// `content_block_start` / `content_block_stop`, so the encoder tracks which
/// block (if any) is currently open and closes it before opening the next of a
/// different kind. Whole tool calls open, fill, and close a block in one step.
#[derive(Debug)]
pub struct StreamEncoder {
    next_index: u64,
    open: Option<&'static str>,
    open_index: u64,
    stop_reason: &'static str,
    usage: CanonicalUsage,
    finalized: bool,
}

impl Default for StreamEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamEncoder {
    pub fn new() -> Self {
        Self {
            next_index: 0,
            open: None,
            open_index: 0,
            stop_reason: "end_turn",
            usage: CanonicalUsage::default(),
            finalized: false,
        }
    }

    /// The opening `message_start` frame (empty content; usage filled at the end).
    pub fn message_start(&self, id: &str, model: &str) -> SseFrame {
        SseFrame {
            event: "message_start",
            data: json!({
                "type": "message_start",
                "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    // Raycast reports usage only at stream end, so token counts
                    // are still unknown here; the shape matches Anthropic's
                    // four-field `usage` with zeros, and the real usage (input /
                    // output / cache-read tokens) lands in the terminating
                    // `message_delta`.
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0
                    }
                }
            }),
        }
    }

    /// Feed one canonical event, yielding the SSE frames it produces. `Finish`
    /// records the stop reason and usage but emits no frames — call
    /// [`finalize`](Self::finalize) to close the message.
    pub fn push(&mut self, event: CanonicalEvent) -> Vec<SseFrame> {
        match event {
            CanonicalEvent::ReasoningDelta(text) => {
                let mut frames = self.ensure_block("thinking");
                frames.push(self.delta(json!({ "type": "thinking_delta", "thinking": text })));
                frames
            }
            CanonicalEvent::ThoughtSignature(signature) => {
                let mut frames = self.ensure_block("thinking");
                frames
                    .push(self.delta(json!({ "type": "signature_delta", "signature": signature })));
                frames
            }
            CanonicalEvent::TextDelta(text) => {
                let mut frames = self.ensure_block("text");
                frames.push(self.delta(json!({ "type": "text_delta", "text": text })));
                frames
            }
            CanonicalEvent::ToolCall(call) => {
                let mut frames = self.close_current();
                self.stop_reason = "tool_use";
                let index = self.next_index;
                self.next_index += 1;
                frames.push(SseFrame {
                    event: "content_block_start",
                    data: json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name,
                            "input": {}
                        }
                    }),
                });
                frames.push(SseFrame {
                    event: "content_block_delta",
                    data: json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "input_json_delta", "partial_json": call.arguments }
                    }),
                });
                frames.push(SseFrame {
                    event: "content_block_stop",
                    data: json!({ "type": "content_block_stop", "index": index }),
                });
                frames
            }
            CanonicalEvent::Finish { reason, usage } => {
                if reason == FinishReason::ToolCalls {
                    self.stop_reason = "tool_use";
                }
                // Keep the full usage so the terminating `message_delta` carries
                // the same token accounting (incl. cached read tokens) as the
                // non-streaming `message_response`.
                self.usage = usage;
                Vec::new()
            }
            CanonicalEvent::Error { status, message } => {
                // Anthropic streams report failures as a terminal `error` event
                // (cf. `error_type` in `super::error` for the type taxonomy).
                let kind = match status {
                    400 => "invalid_request_error",
                    401 => "authentication_error",
                    403 => "permission_error",
                    404 => "not_found_error",
                    429 => "rate_limit_error",
                    529 => "overloaded_error",
                    _ => "api_error",
                };
                vec![SseFrame {
                    event: "error",
                    data: serde_json::json!({
                        "type": "error",
                        "error": { "type": kind, "message": message }
                    }),
                }]
            }
        }
    }

    /// Close any open block and emit the terminating `message_delta` +
    /// `message_stop`. Idempotent, so it is safe to call after a `Finish` event
    /// and again when the stream ends.
    pub fn finalize(&mut self) -> Vec<SseFrame> {
        if self.finalized {
            return Vec::new();
        }
        self.finalized = true;

        let mut frames = self.close_current();
        frames.push(SseFrame {
            event: "message_delta",
            data: json!({
                "type": "message_delta",
                "delta": { "stop_reason": self.stop_reason, "stop_sequence": null },
                // Full usage, matching the non-streaming `OutputUsage` shape:
                // Raycast only reports a single cached-token count (prompt read
                // from cache → `cache_read_input_tokens`); cache creation is
                // never reported, so it stays 0.
                "usage": {
                    "input_tokens": self.usage.input_tokens,
                    "output_tokens": self.usage.output_tokens,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": self.usage.cached_tokens
                }
            }),
        });
        frames.push(SseFrame {
            event: "message_stop",
            data: json!({ "type": "message_stop" }),
        });
        frames
    }

    /// Ensure a block of `kind` (`"thinking"` / `"text"`) is open, closing a
    /// different open block first. Returns any frames the transition emits.
    fn ensure_block(&mut self, kind: &'static str) -> Vec<SseFrame> {
        if self.open == Some(kind) {
            return Vec::new();
        }
        let mut frames = self.close_current();
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some(kind);
        self.open_index = index;

        let content_block = match kind {
            "thinking" => json!({ "type": "thinking", "thinking": "" }),
            _ => json!({ "type": "text", "text": "" }),
        };
        frames.push(SseFrame {
            event: "content_block_start",
            data: json!({
                "type": "content_block_start",
                "index": index,
                "content_block": content_block
            }),
        });
        frames
    }

    /// Emit a `content_block_stop` for the currently open block, if any.
    fn close_current(&mut self) -> Vec<SseFrame> {
        match self.open.take() {
            Some(_) => vec![SseFrame {
                event: "content_block_stop",
                data: json!({ "type": "content_block_stop", "index": self.open_index }),
            }],
            None => Vec::new(),
        }
    }

    /// A `content_block_delta` for the currently open block.
    fn delta(&self, delta: Value) -> SseFrame {
        SseFrame {
            event: "content_block_delta",
            data: json!({
                "type": "content_block_delta",
                "index": self.open_index,
                "delta": delta
            }),
        }
    }
}

/// Serialize a frame into a named SSE event.
fn to_event(frame: &SseFrame) -> Result<Event, Infallible> {
    Ok(Event::default()
        .event(frame.event)
        .data(frame.data.to_string()))
}

/// Map the canonical finish reason to Anthropic's `stop_reason`.
fn stop_reason(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::ToolCalls => "tool_use",
        FinishReason::Stop => "end_turn",
    }
}

/// Parse a tool call's raw arguments string into a JSON value for `input`
/// (falling back to an empty object if it isn't valid JSON).
fn parse_arguments(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({}))
}
