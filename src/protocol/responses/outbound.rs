//! ⑤ OpenAI Responses outbound: canonical output → Responses JSON / SSE.
//!
//! Two rendering paths, both fed by the shared pipeline:
//!
//! * **non-streaming** ([`response_object`]) folds a [`CanonicalResponse`] into
//!   one `response` object whose `output[]` is an optional `reasoning` item, an
//!   optional assistant `message` (one `output_text` part), then any
//!   `function_call` items.
//! * **streaming** ([`StreamEncoder`]) turns the canonical event stream into
//!   the Responses typed-event sequence: `response.created` →
//!   per-item (`output_item.added` → deltas → `output_item.done`)* →
//!   `response.completed` (carrying the fully assembled response + usage).
//!
//! The encoder is a small state machine kept separate from the async plumbing so
//! its exact frame sequence can be unit-tested directly. Every event carries a
//! monotonically increasing `sequence_number`, as Responses clients expect.
//!
//! Note: Raycast reports `usage` only at the end of the stream, so
//! `response.created` advertises `usage: null` and the real token counts land in
//! the terminating `response.completed`.

use std::convert::Infallible;

use async_stream::stream;
use axum::Json;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};

use crate::core::event::CanonicalEvent;
use crate::core::message::ToolCall;
use crate::core::response::CanonicalResponse;
use crate::error::GatewayError;
use crate::protocol::OutboundAdapter;
use crate::util::{new_uuid, unix_secs};

use super::wire::{
    InputTokensDetails, OutputContent, OutputItem, ResponseObject, ResponseUsage, SummaryPart,
};

/// Zero-sized marker selecting the OpenAI Responses outbound rendering.
pub struct ResponsesOutbound;

impl OutboundAdapter for ResponsesOutbound {
    fn render_complete(model: &str, response: &CanonicalResponse) -> Response {
        Json(response_object(model, response)).into_response()
    }

    fn stream<S>(model: &str, events: S) -> Response
    where
        S: Stream<Item = CanonicalEvent> + Send + 'static,
    {
        let id = format!("resp_{}", new_uuid());
        let created_at = unix_secs();
        let model = model.to_string();

        let sse = stream! {
            let mut encoder = StreamEncoder::new(id, model, created_at);
            yield to_event(&encoder.created());

            let mut events = Box::pin(events);
            while let Some(event) = events.next().await {
                let finished = matches!(event, CanonicalEvent::Finish { .. });
                for frame in encoder.push(event) {
                    yield to_event(&frame);
                }
                if finished {
                    break;
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

/// Fold a fully-aggregated canonical response into a Responses `response` object.
pub fn response_object(model: &str, response: &CanonicalResponse) -> ResponseObject {
    let mut output = Vec::new();

    if !response.reasoning.is_empty() {
        output.push(OutputItem::Reasoning {
            id: format!("rs_{}", new_uuid()),
            summary: vec![SummaryPart::SummaryText {
                text: response.reasoning.clone(),
            }],
        });
    }
    if !response.text.is_empty() {
        output.push(message_item(response.text.clone()));
    }
    for call in &response.tool_calls {
        output.push(OutputItem::FunctionCall {
            id: format!("fc_{}", new_uuid()),
            call_id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            status: "completed",
        });
    }
    // A completed response always carries at least one output item.
    if output.is_empty() {
        output.push(message_item(String::new()));
    }

    ResponseObject {
        id: format!("resp_{}", new_uuid()),
        object: "response",
        created_at: unix_secs(),
        status: "completed",
        model: model.to_string(),
        output,
        usage: map_usage(response.usage),
    }
}

/// A completed assistant `message` item wrapping `text` in one `output_text` part.
fn message_item(text: String) -> OutputItem {
    OutputItem::Message {
        id: format!("msg_{}", new_uuid()),
        status: "completed",
        role: "assistant",
        content: vec![OutputContent::OutputText {
            text,
            annotations: Vec::new(),
        }],
    }
}

/// Derive the Responses usage block (with `total_tokens`) from canonical usage.
/// Shared by the non-streaming response and the streaming `response.completed`
/// frame so both paths report identical token accounting. `input_tokens_details`
/// is attached only when the provider reported cached tokens, keeping the common
/// response's three-field usage shape unchanged.
fn map_usage(usage: crate::core::usage::Usage) -> ResponseUsage {
    ResponseUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.input_tokens.saturating_add(usage.output_tokens),
        input_tokens_details: (usage.cached_tokens > 0).then_some(InputTokensDetails {
            cached_tokens: usage.cached_tokens,
        }),
    }
}

/// One Responses SSE frame: a named `event:` and its JSON `data:` payload.
#[derive(Debug, Clone)]
pub struct SseFrame {
    pub event: &'static str,
    pub data: Value,
}

/// The kind of output item currently being streamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenKind {
    Reasoning,
    Message,
}

/// The output item currently open, accumulating its text for the closing frames.
#[derive(Debug)]
struct OpenItem {
    kind: OpenKind,
    id: String,
    index: u64,
    text: String,
}

/// Incremental encoder from canonical events to the Responses SSE event sequence.
///
/// Responses output is a list of items (`reasoning`, `message`, `function_call`),
/// each bracketed by `output_item.added` / `output_item.done`. The encoder tracks
/// the currently open streaming item (reasoning or message), closing it before a
/// different item begins; whole tool calls open, fill, and close their own item
/// in one step. It accumulates each finalized item so the terminating
/// `response.completed` can carry the fully assembled `output[]`.
#[derive(Debug)]
pub struct StreamEncoder {
    id: String,
    model: String,
    created_at: u64,
    seq: u64,
    next_index: u64,
    open: Option<OpenItem>,
    items: Vec<Value>,
    usage: ResponseUsage,
    finalized: bool,
}

impl StreamEncoder {
    pub fn new(id: String, model: String, created_at: u64) -> Self {
        Self {
            id,
            model,
            created_at,
            seq: 0,
            next_index: 0,
            open: None,
            items: Vec::new(),
            usage: ResponseUsage::default(),
            finalized: false,
        }
    }

    /// The opening `response.created` frame (empty output; usage filled at the end).
    pub fn created(&mut self) -> SseFrame {
        let response = self.response_snapshot("in_progress", Value::Null);
        self.frame(
            "response.created",
            json!({ "type": "response.created", "response": response }),
        )
    }

    /// Feed one canonical event, yielding the SSE frames it produces. `Finish`
    /// records usage but emits no frames — call [`finalize`](Self::finalize) to
    /// close the response.
    pub fn push(&mut self, event: CanonicalEvent) -> Vec<SseFrame> {
        match event {
            CanonicalEvent::ReasoningDelta(text) => {
                let mut frames = self.ensure_open(OpenKind::Reasoning);
                let (id, index) = {
                    let item = self.open.as_mut().expect("reasoning item open");
                    item.text.push_str(&text);
                    (item.id.clone(), item.index)
                };
                frames.push(self.frame(
                    "response.reasoning_summary_text.delta",
                    json!({
                        "type": "response.reasoning_summary_text.delta",
                        "item_id": id,
                        "output_index": index,
                        "summary_index": 0,
                        "delta": text,
                    }),
                ));
                frames
            }
            // Responses has no client-facing thought-signature surface.
            CanonicalEvent::ThoughtSignature(_) => Vec::new(),
            CanonicalEvent::TextDelta(text) => {
                let mut frames = self.ensure_open(OpenKind::Message);
                let (id, index) = {
                    let item = self.open.as_mut().expect("message item open");
                    item.text.push_str(&text);
                    (item.id.clone(), item.index)
                };
                frames.push(self.frame(
                    "response.output_text.delta",
                    json!({
                        "type": "response.output_text.delta",
                        "item_id": id,
                        "output_index": index,
                        "content_index": 0,
                        "delta": text,
                        "logprobs": [],
                    }),
                ));
                frames
            }
            CanonicalEvent::ToolCall(call) => {
                let mut frames = self.close_current();
                frames.extend(self.emit_function_call(call));
                frames
            }
            CanonicalEvent::Finish { usage, .. } => {
                // Same mapping as the non-streaming response, so the terminating
                // `response.completed` frame carries identical usage (incl.
                // `input_tokens_details.cached_tokens`).
                self.usage = map_usage(usage);
                Vec::new()
            }
        }
    }

    /// Close any open item and emit the terminating `response.completed`
    /// (carrying the assembled `output[]` and usage). Idempotent.
    pub fn finalize(&mut self) -> Vec<SseFrame> {
        if self.finalized {
            return Vec::new();
        }
        self.finalized = true;

        let mut frames = self.close_current();
        // Serialize the shared `ResponseUsage` so the streamed usage is byte-for-byte
        // the same object the non-streaming response carries (incl. the optional
        // `input_tokens_details`).
        let usage = serde_json::to_value(self.usage).unwrap_or(Value::Null);
        let response = self.response_snapshot("completed", usage);
        frames.push(self.frame(
            "response.completed",
            json!({ "type": "response.completed", "response": response }),
        ));
        frames
    }

    /// Ensure a streaming item of `kind` is open, closing a different open item
    /// first. Returns the frames the transition emits (`output_item.added` plus
    /// the item's opening part frame).
    fn ensure_open(&mut self, kind: OpenKind) -> Vec<SseFrame> {
        if matches!(&self.open, Some(item) if item.kind == kind) {
            return Vec::new();
        }
        let mut frames = self.close_current();

        let index = self.next_index;
        self.next_index += 1;
        let (prefix, item, part) = match kind {
            OpenKind::Reasoning => (
                "rs",
                json!({ "type": "reasoning", "summary": [] }),
                (
                    "response.reasoning_summary_part.added",
                    json!({ "type": "summary_text", "text": "" }),
                ),
            ),
            OpenKind::Message => (
                "msg",
                json!({
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                }),
                (
                    "response.content_part.added",
                    json!({ "type": "output_text", "text": "", "annotations": [] }),
                ),
            ),
        };
        let id = format!("{prefix}_{}", new_uuid());
        self.open = Some(OpenItem {
            kind,
            id: id.clone(),
            index,
            text: String::new(),
        });

        let mut item_value = item;
        if let Value::Object(map) = &mut item_value {
            map.insert("id".into(), json!(id));
        }
        frames.push(self.frame(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": index,
                "item": item_value,
            }),
        ));

        let (part_event, part_value) = part;
        let part_frame = match kind {
            OpenKind::Reasoning => json!({
                "type": "response.reasoning_summary_part.added",
                "item_id": id,
                "output_index": index,
                "summary_index": 0,
                "part": part_value,
            }),
            OpenKind::Message => json!({
                "type": "response.content_part.added",
                "item_id": id,
                "output_index": index,
                "content_index": 0,
                "part": part_value,
            }),
        };
        frames.push(self.frame(part_event, part_frame));
        frames
    }

    /// Close the currently open item (if any), emitting its `*.done` part frames
    /// and `output_item.done`, and recording the finalized item for the response.
    fn close_current(&mut self) -> Vec<SseFrame> {
        let item = match self.open.take() {
            Some(item) => item,
            None => return Vec::new(),
        };

        let mut frames = Vec::new();
        let final_item = match item.kind {
            OpenKind::Reasoning => {
                frames.push(self.frame(
                    "response.reasoning_summary_text.done",
                    json!({
                        "type": "response.reasoning_summary_text.done",
                        "item_id": item.id,
                        "output_index": item.index,
                        "summary_index": 0,
                        "text": item.text,
                    }),
                ));
                frames.push(self.frame(
                    "response.reasoning_summary_part.done",
                    json!({
                        "type": "response.reasoning_summary_part.done",
                        "item_id": item.id,
                        "output_index": item.index,
                        "summary_index": 0,
                        "part": { "type": "summary_text", "text": item.text },
                    }),
                ));
                json!({
                    "id": item.id,
                    "type": "reasoning",
                    "summary": [ { "type": "summary_text", "text": item.text } ],
                })
            }
            OpenKind::Message => {
                frames.push(self.frame(
                    "response.output_text.done",
                    json!({
                        "type": "response.output_text.done",
                        "item_id": item.id,
                        "output_index": item.index,
                        "content_index": 0,
                        "text": item.text,
                        "logprobs": [],
                    }),
                ));
                frames.push(self.frame(
                    "response.content_part.done",
                    json!({
                        "type": "response.content_part.done",
                        "item_id": item.id,
                        "output_index": item.index,
                        "content_index": 0,
                        "part": { "type": "output_text", "text": item.text, "annotations": [] },
                    }),
                ));
                json!({
                    "id": item.id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [ { "type": "output_text", "text": item.text, "annotations": [] } ],
                })
            }
        };

        self.items.push(final_item.clone());
        frames.push(self.frame(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": item.index,
                "item": final_item,
            }),
        ));
        frames
    }

    /// Emit a whole `function_call` item (added → arguments delta → arguments
    /// done → item done) in one step, recording it for the response.
    fn emit_function_call(&mut self, call: ToolCall) -> Vec<SseFrame> {
        let index = self.next_index;
        self.next_index += 1;
        let id = format!("fc_{}", new_uuid());

        let mut frames = Vec::new();
        frames.push(self.frame(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": index,
                "item": {
                    "id": id,
                    "type": "function_call",
                    "status": "in_progress",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": "",
                },
            }),
        ));
        frames.push(self.frame(
            "response.function_call_arguments.delta",
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": id,
                "output_index": index,
                "delta": call.arguments,
            }),
        ));
        frames.push(self.frame(
            "response.function_call_arguments.done",
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": id,
                "output_index": index,
                "arguments": call.arguments,
            }),
        ));

        let final_item = json!({
            "id": id,
            "type": "function_call",
            "status": "completed",
            "call_id": call.id,
            "name": call.name,
            "arguments": call.arguments,
        });
        self.items.push(final_item.clone());
        frames.push(self.frame(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": final_item,
            }),
        ));
        frames
    }

    /// A snapshot `response` object for `response.created` / `response.completed`,
    /// carrying the items assembled so far.
    fn response_snapshot(&self, status: &'static str, usage: Value) -> Value {
        json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": self.model,
            "output": self.items,
            "usage": usage,
        })
    }

    /// Stamp the next `sequence_number` onto a frame's top-level `data` object.
    fn frame(&mut self, event: &'static str, mut data: Value) -> SseFrame {
        let seq = self.seq;
        self.seq += 1;
        if let Value::Object(map) = &mut data {
            map.insert("sequence_number".into(), json!(seq));
        }
        SseFrame { event, data }
    }
}

/// Serialize a frame into a named SSE event.
fn to_event(frame: &SseFrame) -> Result<Event, Infallible> {
    Ok(Event::default()
        .event(frame.event)
        .data(frame.data.to_string()))
}
