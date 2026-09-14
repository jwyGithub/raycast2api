//! Offline tests pinning the OpenAI **Responses** (`POST /v1/responses`) contract.
//!
//! The gateway is drop-in for Responses clients (e.g. Codex), so these lock the
//! external surface:
//!
//! * **inbound** — flattening the Responses `input[]` union (`instructions`,
//!   role messages with typed parts, `function_call`, `function_call_output`)
//!   into the canonical model,
//! * **non-streaming** — the `response` JSON shape built by [`response_object`],
//! * **streaming** — the typed SSE event sequence produced by [`StreamEncoder`],
//! * **errors** — the OpenAI-shaped error envelope.

use raycast2api::core::event::CanonicalEvent;
use raycast2api::core::message::{Role, ToolCall};
use raycast2api::core::request::{CanonicalRequest, ReasoningEffort};
use raycast2api::core::response::{CanonicalResponse, FinishReason};
use raycast2api::core::tools::{ToolChoice, ToolDef};
use raycast2api::core::usage::Usage;
use raycast2api::error::GatewayError;
use raycast2api::protocol::responses::outbound::{SseFrame, StreamEncoder, response_object};
use raycast2api::protocol::responses::wire::ResponsesRequest;
use raycast2api::protocol::responses::{ResponsesInbound, ResponsesOutbound};
use raycast2api::protocol::{InboundAdapter, OutboundAdapter};
use serde_json::{Value, json};

/// Deserialize a `/v1/responses` body and run the inbound adapter.
fn parse(body: Value) -> Result<CanonicalRequest, GatewayError> {
    let request: ResponsesRequest = serde_json::from_value(body).unwrap();
    ResponsesInbound::parse(request)
}

/// The ordered list of SSE event names in a batch of frames.
fn events(frames: &[SseFrame]) -> Vec<&str> {
    frames.iter().map(|f| f.event).collect()
}

// --- Inbound: request mapping & input flattening -------------------------

#[test]
fn empty_input_is_rejected() {
    // No input at all, and an empty item list, both fail as bad requests.
    assert!(matches!(
        parse(json!({ "model": "gpt-x" })).unwrap_err(),
        GatewayError::BadRequest(_)
    ));
    assert!(matches!(
        parse(json!({ "model": "gpt-x", "input": [] })).unwrap_err(),
        GatewayError::BadRequest(_)
    ));
}

#[test]
fn string_input_becomes_a_single_user_message() {
    let canonical = parse(json!({ "model": "gpt-x", "input": "hello" })).unwrap();

    assert_eq!(canonical.messages.len(), 1);
    assert_eq!(canonical.messages[0].role, Role::User);
    assert_eq!(canonical.messages[0].text, "hello");
}

#[test]
fn instructions_and_system_items_join_and_parts_flatten() {
    let canonical = parse(json!({
        "model": "gpt-x",
        "instructions": "be terse",
        "input": [
            { "role": "developer", "content": "internal note" },
            {
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "hi " },
                    { "type": "input_text", "text": "there" }
                ]
            },
            { "role": "assistant", "content": [ { "type": "output_text", "text": "hello" } ] }
        ]
    }))
    .unwrap();

    // `instructions` seeds the system prompt; a `developer` item appends to it.
    assert_eq!(canonical.system.as_deref(), Some("be terse\ninternal note"));

    let roles: Vec<Role> = canonical.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, vec![Role::User, Role::Assistant]);
    // Typed content parts flatten into one plain string.
    assert_eq!(canonical.messages[0].text, "hi there");
    assert_eq!(canonical.messages[1].text, "hello");
}

#[test]
fn function_call_and_output_map_to_tool_turns() {
    let canonical = parse(json!({
        "model": "gpt-x",
        "input": [
            { "type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"NYC\"}" },
            { "type": "function_call_output", "call_id": "call_1", "output": "42" }
        ]
    }))
    .unwrap();

    assert_eq!(canonical.messages.len(), 2);

    // A `function_call` becomes an assistant turn carrying the tool call.
    let assistant = &canonical.messages[0];
    assert_eq!(assistant.role, Role::Assistant);
    assert_eq!(
        assistant.tool_calls,
        vec![ToolCall {
            id: "call_1".into(),
            name: "get_weather".into(),
            // Arguments are kept verbatim as the raw JSON string.
            arguments: r#"{"city":"NYC"}"#.into(),
        }]
    );

    // A `function_call_output` becomes a standalone Role::Tool message.
    let tool = &canonical.messages[1];
    assert_eq!(tool.role, Role::Tool);
    assert_eq!(tool.text, "42");
    assert_eq!(tool.tool_call_id.as_deref(), Some("call_1"));
}

#[test]
fn tools_tool_choice_and_reasoning_map_to_canonical() {
    let canonical = parse(json!({
        "model": "gpt-x",
        "input": "hi",
        "tools": [
            {
                "type": "function",
                "name": "get_weather",
                "description": "Look up weather",
                "parameters": { "type": "object", "properties": { "city": { "type": "string" } } }
            },
            { "type": "web_search_preview" }
        ],
        "tool_choice": { "type": "function", "name": "get_weather" },
        "reasoning": { "effort": "high" }
    }))
    .unwrap();

    // A `function` tool is local (schema-carrying); a built-in tool is remote.
    match canonical.tools.as_slice() {
        [
            ToolDef::Local {
                name,
                description,
                parameters,
            },
            ToolDef::Remote { name: remote },
        ] => {
            assert_eq!(name, "get_weather");
            assert_eq!(description.as_deref(), Some("Look up weather"));
            assert_eq!(parameters["properties"]["city"]["type"], "string");
            assert_eq!(remote, "web_search_preview");
        }
        other => panic!("expected one local + one remote tool, got {other:?}"),
    }

    assert_eq!(
        canonical.tool_choice,
        Some(ToolChoice::Named("get_weather".into()))
    );
    assert!(matches!(
        canonical.reasoning_effort,
        Some(ReasoningEffort::High)
    ));
}

// --- Non-streaming: the `response` object shape --------------------------

#[test]
fn non_streaming_response_shape() {
    let response = CanonicalResponse {
        text: "The weather is sunny.".into(),
        reasoning: "Let me look that up.".into(),
        thought_signature: None,
        tool_calls: vec![ToolCall {
            id: "call_1".into(),
            name: "get_weather".into(),
            arguments: r#"{"city":"NYC"}"#.into(),
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: Usage {
            input_tokens: 12,
            output_tokens: 7,
            cached_tokens: 0,
        },
    };

    let value = serde_json::to_value(response_object("gpt-x", &response)).unwrap();

    assert_eq!(value["object"], "response");
    assert_eq!(value["status"], "completed");
    assert_eq!(value["model"], "gpt-x");
    assert!(
        value["id"].as_str().unwrap().starts_with("resp_"),
        "id must be prefixed resp_, got {}",
        value["id"]
    );

    // output[]: reasoning → message(output_text) → function_call, in that order.
    let output = value["output"].as_array().unwrap();
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[0]["summary"][0]["type"], "summary_text");
    assert_eq!(output[0]["summary"][0]["text"], "Let me look that up.");
    assert_eq!(output[1]["type"], "message");
    assert_eq!(output[1]["role"], "assistant");
    assert_eq!(output[1]["content"][0]["type"], "output_text");
    assert_eq!(output[1]["content"][0]["text"], "The weather is sunny.");
    assert_eq!(output[2]["type"], "function_call");
    assert_eq!(output[2]["call_id"], "call_1");
    assert_eq!(output[2]["name"], "get_weather");
    assert_eq!(output[2]["arguments"], r#"{"city":"NYC"}"#);

    // total_tokens is derived from input + output.
    assert_eq!(
        value["usage"],
        json!({ "input_tokens": 12, "output_tokens": 7, "total_tokens": 19 })
    );
}

#[test]
fn non_streaming_usage_reports_cached_tokens_in_input_tokens_details() {
    // When the provider served part of the prompt from cache, the usage block
    // gains `input_tokens_details.cached_tokens` (attached only when non-zero).
    let response = CanonicalResponse {
        text: "Hi".into(),
        reasoning: String::new(),
        thought_signature: None,
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Usage {
            input_tokens: 100,
            output_tokens: 20,
            cached_tokens: 80,
        },
    };

    let value = serde_json::to_value(response_object("gpt-x", &response)).unwrap();
    assert_eq!(
        value["usage"],
        json!({
            "input_tokens": 100,
            "output_tokens": 20,
            "total_tokens": 120,
            "input_tokens_details": { "cached_tokens": 80 }
        })
    );
}

#[test]
fn empty_response_still_carries_one_message_item() {
    let value =
        serde_json::to_value(response_object("gpt-x", &CanonicalResponse::default())).unwrap();

    let output = value["output"].as_array().unwrap();
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["content"][0]["text"], "");
    assert_eq!(value["status"], "completed");
}

// --- Streaming: the typed SSE event sequence -----------------------------

#[test]
fn created_frame_shape() {
    let mut encoder = StreamEncoder::new("resp_1".into(), "gpt-x".into(), 123);
    let frame = encoder.created();

    assert_eq!(frame.event, "response.created");
    assert_eq!(frame.data["type"], "response.created");
    // Every event is numbered; `created` is the first.
    assert_eq!(frame.data["sequence_number"], 0);
    assert_eq!(frame.data["response"]["id"], "resp_1");
    assert_eq!(frame.data["response"]["status"], "in_progress");
    assert_eq!(frame.data["response"]["model"], "gpt-x");
    assert_eq!(frame.data["response"]["output"], json!([]));
    // Raycast reports usage only at the end, so it starts null.
    assert!(frame.data["response"]["usage"].is_null());
}

#[test]
fn text_stream_brackets_a_message_item() {
    let mut encoder = StreamEncoder::new("resp_1".into(), "gpt-x".into(), 0);
    let _ = encoder.created();

    let opening = encoder.push(CanonicalEvent::TextDelta("Hi".into()));
    // A text delta opens a message item (index 0) and its content part, then
    // emits the delta.
    assert_eq!(
        events(&opening),
        [
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta"
        ]
    );
    assert_eq!(opening[0].data["item"]["type"], "message");
    assert_eq!(opening[0].data["output_index"], 0);
    assert_eq!(opening[0].data["sequence_number"], 1);
    assert_eq!(opening[2].data["delta"], "Hi");

    // `Finish` records usage but emits no frames.
    assert!(
        encoder
            .push(CanonicalEvent::Finish {
                reason: FinishReason::Stop,
                usage: Usage {
                    input_tokens: 3,
                    output_tokens: 5,
                    cached_tokens: 0,
                },
            })
            .is_empty()
    );

    let closing = encoder.finalize();
    assert_eq!(
        events(&closing),
        [
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed"
        ]
    );
    assert_eq!(closing[0].data["text"], "Hi");

    let completed = &closing[3];
    assert_eq!(completed.data["sequence_number"], 7);
    assert_eq!(completed.data["response"]["status"], "completed");
    assert_eq!(completed.data["response"]["output"][0]["type"], "message");
    assert_eq!(
        completed.data["response"]["output"][0]["content"][0]["text"],
        "Hi"
    );
    assert_eq!(
        completed.data["response"]["usage"],
        json!({ "input_tokens": 3, "output_tokens": 5, "total_tokens": 8 })
    );

    // finalize is idempotent.
    assert!(encoder.finalize().is_empty());
}

#[test]
fn streaming_completed_frame_reports_cached_tokens() {
    // The streaming `response.completed` usage is byte-identical to the
    // non-streaming response's usage, including `input_tokens_details`.
    let mut encoder = StreamEncoder::new("resp_1".into(), "gpt-x".into(), 0);
    let _ = encoder.created();
    let _ = encoder.push(CanonicalEvent::TextDelta("Hi".into()));
    let _ = encoder.push(CanonicalEvent::Finish {
        reason: FinishReason::Stop,
        usage: Usage {
            input_tokens: 100,
            output_tokens: 20,
            cached_tokens: 80,
        },
    });

    let closing = encoder.finalize();
    let completed = closing
        .iter()
        .find(|frame| frame.event == "response.completed")
        .expect("a response.completed frame");
    assert_eq!(
        completed.data["response"]["usage"],
        json!({
            "input_tokens": 100,
            "output_tokens": 20,
            "total_tokens": 120,
            "input_tokens_details": { "cached_tokens": 80 }
        })
    );
}

#[test]
fn switching_from_reasoning_to_text_closes_and_reopens_items() {
    let mut encoder = StreamEncoder::new("resp_1".into(), "gpt-x".into(), 0);
    let _ = encoder.created();

    let think = encoder.push(CanonicalEvent::ReasoningDelta("pondering".into()));
    assert_eq!(
        events(&think),
        [
            "response.output_item.added",
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_text.delta"
        ]
    );
    assert_eq!(think[0].data["item"]["type"], "reasoning");
    assert_eq!(think[0].data["output_index"], 0);
    assert_eq!(think[2].data["delta"], "pondering");

    // The first text delta must close reasoning item 0 and open message item 1.
    let text = encoder.push(CanonicalEvent::TextDelta("answer".into()));
    assert_eq!(
        events(&text),
        [
            "response.reasoning_summary_text.done",
            "response.reasoning_summary_part.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta"
        ]
    );
    assert_eq!(text[2].data["item"]["type"], "reasoning");
    assert_eq!(text[2].data["output_index"], 0);
    assert_eq!(text[3].data["item"]["type"], "message");
    assert_eq!(text[3].data["output_index"], 1);
    assert_eq!(text[5].data["delta"], "answer");
}

#[test]
fn tool_call_stream_is_a_self_contained_item() {
    let mut encoder = StreamEncoder::new("resp_1".into(), "gpt-x".into(), 0);
    let _ = encoder.created();

    let frames = encoder.push(CanonicalEvent::ToolCall(ToolCall {
        id: "call_1".into(),
        name: "get_weather".into(),
        arguments: r#"{"city":"NYC"}"#.into(),
    }));

    // A whole tool call opens, fills, and closes its own function_call item.
    assert_eq!(
        events(&frames),
        [
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done"
        ]
    );
    assert_eq!(frames[0].data["item"]["type"], "function_call");
    assert_eq!(frames[0].data["item"]["call_id"], "call_1");
    assert_eq!(frames[0].data["item"]["name"], "get_weather");
    assert_eq!(frames[1].data["delta"], r#"{"city":"NYC"}"#);
    assert_eq!(frames[2].data["arguments"], r#"{"city":"NYC"}"#);

    // The finalized call lands in the completed response's output[].
    let closing = encoder.finalize();
    let completed = closing.last().unwrap();
    assert_eq!(completed.event, "response.completed");
    assert_eq!(
        completed.data["response"]["output"][0]["type"],
        "function_call"
    );
}

// --- Errors: the OpenAI envelope -----------------------------------------

#[tokio::test]
async fn error_envelope_shape() {
    let response = ResponsesOutbound::render_error(GatewayError::BadRequest("bad input".into()));
    assert_eq!(response.status(), 400);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();

    // Responses reuses the OpenAI `{ "error": { ... } }` envelope.
    assert_eq!(body["error"]["message"], "bad input");
}
