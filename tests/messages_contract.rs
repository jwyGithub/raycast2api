//! Offline tests pinning the Anthropic **Messages** (`POST /v1/messages`) contract.
//!
//! The gateway is drop-in for Anthropic clients (e.g. Claude Code), so these
//! lock the external surface:
//!
//! * **inbound** — flattening Anthropic's block-structured messages (`system`,
//!   `thinking`, `tool_use`, `tool_result`) into the canonical model,
//! * **non-streaming** — the `Message` JSON shape built by [`message_response`],
//! * **streaming** — the SSE frame sequence produced by [`StreamEncoder`], and
//! * **errors** — the `{ "type": "error", ... }` envelope.
//!
//! All but the error test are pure serde plus the pure encoder/builder — no
//! network and no HTTP plumbing. The error test only needs to read a rendered
//! response body, so it uses a tokio runtime to drain it.

use raycast2api::core::event::CanonicalEvent;
use raycast2api::core::message::{Role, ToolCall};
use raycast2api::core::request::{CanonicalRequest, ReasoningEffort};
use raycast2api::core::response::{CanonicalResponse, FinishReason};
use raycast2api::core::tools::{ToolChoice, ToolDef};
use raycast2api::core::usage::Usage;
use raycast2api::error::GatewayError;
use raycast2api::protocol::messages::outbound::{StreamEncoder, message_response};
use raycast2api::protocol::messages::wire::MessagesRequest;
use raycast2api::protocol::messages::{MessagesInbound, MessagesOutbound};
use raycast2api::protocol::{InboundAdapter, OutboundAdapter};
use serde_json::{Value, json};

/// Deserialize a `/v1/messages` body and run the inbound adapter.
fn parse(body: Value) -> Result<CanonicalRequest, GatewayError> {
    let request: MessagesRequest = serde_json::from_value(body).unwrap();
    MessagesInbound::parse(request)
}

// --- Inbound: request mapping & block flattening -------------------------

#[test]
fn empty_messages_is_rejected() {
    let err = parse(json!({ "model": "claude-x", "messages": [] })).unwrap_err();
    // The pipeline renders this as a 400; here we just pin that it's rejected.
    assert!(matches!(err, GatewayError::BadRequest(_)));
}

#[test]
fn string_content_maps_in_order_and_drops_unknown_roles() {
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": "hello" },
            { "role": "developer", "content": "ignored" },
            { "role": "user", "content": "bye" }
        ]
    }))
    .unwrap();

    let roles: Vec<Role> = canonical.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, vec![Role::User, Role::Assistant, Role::User]);
    assert_eq!(canonical.messages[0].text, "hi");
    assert_eq!(canonical.messages[2].text, "bye");
}

#[test]
fn assistant_blocks_fold_thinking_text_and_tool_use() {
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [{
            "role": "assistant",
            "content": [
                { "type": "thinking", "thinking": "hmm", "signature": "sig-1" },
                { "type": "text", "text": "Let me check." },
                { "type": "tool_use", "id": "call_1", "name": "get_weather", "input": { "city": "NYC" } }
            ]
        }]
    }))
    .unwrap();

    // The whole block-structured assistant turn folds into ONE canonical message.
    assert_eq!(canonical.messages.len(), 1);
    let msg = &canonical.messages[0];
    assert_eq!(msg.role, Role::Assistant);
    assert_eq!(msg.text, "Let me check.");

    let thinking = msg.thinking.as_ref().expect("thinking preserved");
    assert_eq!(thinking.text, "hmm");
    assert_eq!(thinking.signature.as_deref(), Some("sig-1"));

    assert_eq!(
        msg.tool_calls,
        vec![ToolCall {
            id: "call_1".into(),
            name: "get_weather".into(),
            // `input` is kept verbatim as the raw arguments JSON string.
            arguments: r#"{"city":"NYC"}"#.into(),
        }]
    );
}

#[test]
fn user_tool_result_splits_into_its_own_tool_message() {
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [{
            "role": "user",
            "content": [
                { "type": "tool_result", "tool_use_id": "call_1", "content": "42" },
                { "type": "text", "text": "thanks" }
            ]
        }]
    }))
    .unwrap();

    // A `tool_result` block becomes a standalone Role::Tool message; the sibling
    // text becomes a separate user turn, order preserved.
    assert_eq!(canonical.messages.len(), 2);

    let tool = &canonical.messages[0];
    assert_eq!(tool.role, Role::Tool);
    assert_eq!(tool.text, "42");
    assert_eq!(tool.tool_call_id.as_deref(), Some("call_1"));

    let user = &canonical.messages[1];
    assert_eq!(user.role, Role::User);
    assert_eq!(user.text, "thanks");
}

#[test]
fn system_blocks_join_and_thinking_enables_reasoning() {
    let canonical = parse(json!({
        "model": "claude-x",
        "system": [
            { "type": "text", "text": "line one" },
            { "type": "text", "text": "line two" }
        ],
        "messages": [{ "role": "user", "content": "hi" }],
        "thinking": { "type": "enabled", "budget_tokens": 2048 }
    }))
    .unwrap();

    assert_eq!(canonical.system.as_deref(), Some("line one\nline two"));
    // A small thinking budget buckets to `low` (see reasoning_effort mapping);
    // the point here is that thinking on ⇒ some effort is set.
    assert!(canonical.reasoning_effort.is_some());
}

#[test]
fn tools_and_tool_choice_map_to_canonical() {
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [{ "role": "user", "content": "hi" }],
        "tools": [{
            "name": "get_weather",
            "description": "Look up weather",
            "input_schema": { "type": "object", "properties": { "city": { "type": "string" } } }
        }],
        "tool_choice": { "type": "tool", "name": "get_weather" }
    }))
    .unwrap();

    match canonical.tools.as_slice() {
        [
            ToolDef::Local {
                name,
                description,
                parameters,
            },
        ] => {
            assert_eq!(name, "get_weather");
            assert_eq!(description.as_deref(), Some("Look up weather"));
            assert_eq!(parameters["properties"]["city"]["type"], "string");
        }
        other => panic!("expected one local tool, got {other:?}"),
    }
    assert_eq!(
        canonical.tool_choice,
        Some(ToolChoice::Named("get_weather".into()))
    );
}

// --- Non-streaming: the `Message` response shape -------------------------

#[test]
fn non_streaming_message_shape() {
    let response = CanonicalResponse {
        text: "The weather is sunny.".into(),
        reasoning: "Let me look that up.".into(),
        thought_signature: Some("sig-1".into()),
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

    let value = serde_json::to_value(message_response("claude-x", &response)).unwrap();

    assert_eq!(value["type"], "message");
    assert_eq!(value["role"], "assistant");
    assert_eq!(value["model"], "claude-x");
    assert!(
        value["id"].as_str().unwrap().starts_with("msg_"),
        "id must be prefixed msg_, got {}",
        value["id"]
    );

    // content[]: thinking (with signature) → text → tool_use, in that order.
    let content = value["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["thinking"], "Let me look that up.");
    assert_eq!(content[0]["signature"], "sig-1");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[1]["text"], "The weather is sunny.");
    assert_eq!(content[2]["type"], "tool_use");
    assert_eq!(content[2]["id"], "call_1");
    assert_eq!(content[2]["name"], "get_weather");
    // The raw arguments string is re-parsed into a structured `input` object.
    assert_eq!(content[2]["input"], json!({ "city": "NYC" }));

    assert_eq!(value["stop_reason"], "tool_use");
    assert!(value["stop_sequence"].is_null());
    assert_eq!(
        value["usage"],
        json!({
            "input_tokens": 12,
            "output_tokens": 7,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        })
    );
}

#[test]
fn cached_tokens_map_to_cache_read_input_tokens() {
    // Raycast's single `cached_tokens` count surfaces as `cache_read_input_tokens`;
    // cache creation is never reported, so it stays 0.
    let response = CanonicalResponse {
        usage: Usage {
            input_tokens: 100,
            output_tokens: 20,
            cached_tokens: 80,
        },
        ..CanonicalResponse::default()
    };

    let value = serde_json::to_value(message_response("claude-x", &response)).unwrap();

    assert_eq!(
        value["usage"],
        json!({
            "input_tokens": 100,
            "output_tokens": 20,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 80
        })
    );
}

#[test]
fn empty_response_still_carries_one_text_block() {
    // A `Message` must always have at least one content block.
    let value =
        serde_json::to_value(message_response("claude-x", &CanonicalResponse::default())).unwrap();

    assert_eq!(value["content"].as_array().unwrap().len(), 1);
    assert_eq!(value["content"][0]["type"], "text");
    assert_eq!(value["content"][0]["text"], "");
    assert_eq!(value["stop_reason"], "end_turn");
}

#[test]
fn thinking_block_omits_signature_when_absent() {
    let response = CanonicalResponse {
        reasoning: "thinking...".into(),
        ..CanonicalResponse::default()
    };
    let value = serde_json::to_value(message_response("claude-x", &response)).unwrap();

    assert_eq!(value["content"][0]["type"], "thinking");
    // No signature was produced, so the field must be omitted (not null).
    assert!(value["content"][0].get("signature").is_none());
}

// --- Streaming: the SSE frame sequence -----------------------------------

#[test]
fn message_start_frame_shape() {
    let encoder = StreamEncoder::new();
    let frame = encoder.message_start("msg_1", "claude-x");

    assert_eq!(frame.event, "message_start");
    assert_eq!(frame.data["type"], "message_start");
    assert_eq!(frame.data["message"]["id"], "msg_1");
    assert_eq!(frame.data["message"]["role"], "assistant");
    assert_eq!(frame.data["message"]["model"], "claude-x");
    assert_eq!(frame.data["message"]["content"], json!([]));
    // Raycast reports usage only at the end, so all counts start at 0, but the
    // object still matches Anthropic's four-field `usage` shape.
    assert_eq!(
        frame.data["message"]["usage"],
        json!({
            "input_tokens": 0,
            "output_tokens": 0,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        })
    );
}

#[test]
fn text_stream_brackets_a_single_block() {
    let mut encoder = StreamEncoder::new();

    let opening = encoder.push(CanonicalEvent::TextDelta("Hi".into()));
    // A text delta opens a text block (index 0) then emits the delta.
    assert_eq!(
        events(&opening),
        ["content_block_start", "content_block_delta"]
    );
    assert_eq!(opening[0].data["content_block"]["type"], "text");
    assert_eq!(opening[0].data["index"], 0);
    assert_eq!(
        opening[1].data["delta"],
        json!({ "type": "text_delta", "text": "Hi" })
    );

    // `Finish` records state but emits no frames.
    assert!(
        encoder
            .push(CanonicalEvent::Finish {
                reason: FinishReason::Stop,
                usage: Usage {
                    output_tokens: 5,
                    ..Usage::default()
                },
            })
            .is_empty()
    );

    let closing = encoder.finalize();
    assert_eq!(
        events(&closing),
        ["content_block_stop", "message_delta", "message_stop"]
    );
    assert_eq!(closing[0].data["index"], 0);
    assert_eq!(closing[1].data["delta"]["stop_reason"], "end_turn");
    assert_eq!(closing[1].data["usage"]["output_tokens"], 5);

    // finalize is idempotent.
    assert!(encoder.finalize().is_empty());
}

#[test]
fn streaming_message_delta_carries_full_usage_with_cached_tokens() {
    // The terminating `message_delta` reports the same four-field usage as the
    // non-streaming `message_response`, so streaming clients see identical token
    // accounting — including `cache_read_input_tokens`.
    let mut encoder = StreamEncoder::new();
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
    let message_delta = closing
        .iter()
        .find(|frame| frame.event == "message_delta")
        .expect("a message_delta frame");
    assert_eq!(
        message_delta.data["usage"],
        json!({
            "input_tokens": 100,
            "output_tokens": 20,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 80
        })
    );
}

#[test]
fn switching_from_thinking_to_text_closes_and_reopens_blocks() {
    let mut encoder = StreamEncoder::new();

    let think = encoder.push(CanonicalEvent::ReasoningDelta("pondering".into()));
    assert_eq!(think[0].data["content_block"]["type"], "thinking");
    assert_eq!(think[0].data["index"], 0);
    assert_eq!(
        think[1].data["delta"],
        json!({ "type": "thinking_delta", "thinking": "pondering" })
    );

    // The first text delta must close block 0 and open a new block 1.
    let text = encoder.push(CanonicalEvent::TextDelta("answer".into()));
    assert_eq!(
        events(&text),
        [
            "content_block_stop",
            "content_block_start",
            "content_block_delta"
        ]
    );
    assert_eq!(text[0].data["index"], 0);
    assert_eq!(text[1].data["content_block"]["type"], "text");
    assert_eq!(text[1].data["index"], 1);
    assert_eq!(text[2].data["index"], 1);
}

#[test]
fn tool_call_stream_is_self_contained_and_sets_stop_reason() {
    let mut encoder = StreamEncoder::new();

    let frames = encoder.push(CanonicalEvent::ToolCall(ToolCall {
        id: "call_1".into(),
        name: "get_weather".into(),
        arguments: r#"{"city":"NYC"}"#.into(),
    }));

    // A whole tool call opens, fills, and closes its own block in one step.
    assert_eq!(
        events(&frames),
        [
            "content_block_start",
            "content_block_delta",
            "content_block_stop"
        ]
    );
    assert_eq!(frames[0].data["content_block"]["type"], "tool_use");
    assert_eq!(frames[0].data["content_block"]["id"], "call_1");
    assert_eq!(frames[0].data["content_block"]["name"], "get_weather");
    assert_eq!(
        frames[1].data["delta"],
        json!({ "type": "input_json_delta", "partial_json": r#"{"city":"NYC"}"# })
    );

    // Even without a ToolCalls finish reason, a tool call forces stop_reason.
    let closing = encoder.finalize();
    assert_eq!(closing[0].data["delta"]["stop_reason"], "tool_use");
}

// --- Errors: the Anthropic envelope --------------------------------------

#[tokio::test]
async fn error_envelope_shape() {
    let response = MessagesOutbound::render_error(GatewayError::BadRequest("bad input".into()));
    assert_eq!(response.status(), 400);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["message"], "bad input");
}

/// The ordered list of SSE event names in a batch of frames.
fn events(frames: &[raycast2api::protocol::messages::outbound::SseFrame]) -> Vec<&str> {
    frames.iter().map(|f| f.event).collect()
}

// --- Effort resolution (`output_config.effort` + thinking budget) ---------

#[test]
fn output_config_effort_maps_verbatim() {
    for (sent, expected) in [
        ("low", ReasoningEffort::Low),
        ("medium", ReasoningEffort::Medium),
        ("high", ReasoningEffort::High),
        ("xhigh", ReasoningEffort::XHigh),
        // `max` has no Raycast wire value; clamps to the top known effort.
        ("max", ReasoningEffort::XHigh),
        // Anything outside low/medium/high/xhigh/max falls back to high.
        ("minimal", ReasoningEffort::High),
    ] {
        let canonical = parse(json!({
            "model": "claude-x",
            "messages": [{ "role": "user", "content": "hi" }],
            "output_config": { "effort": sent }
        }))
        .unwrap();
        assert_eq!(
            canonical.reasoning_effort,
            Some(expected),
            "output_config.effort={sent}"
        );
    }
}

#[test]
fn output_config_effort_wins_over_thinking_budget() {
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [{ "role": "user", "content": "hi" }],
        "output_config": { "effort": "xhigh" },
        "thinking": { "type": "enabled", "budget_tokens": 2048 }
    }))
    .unwrap();
    assert_eq!(canonical.reasoning_effort, Some(ReasoningEffort::XHigh));
}

#[test]
fn thinking_budget_buckets_to_effort() {
    for (budget, expected) in [
        (33_000, ReasoningEffort::XHigh),
        (16_000, ReasoningEffort::High),
        (8_192, ReasoningEffort::Medium),
        (1_024, ReasoningEffort::Low),
    ] {
        let canonical = parse(json!({
            "model": "claude-x",
            "messages": [{ "role": "user", "content": "hi" }],
            "thinking": { "type": "enabled", "budget_tokens": budget }
        }))
        .unwrap();
        assert_eq!(
            canonical.reasoning_effort,
            Some(expected),
            "budget_tokens={budget}"
        );
    }
}

#[test]
fn adaptive_thinking_without_budget_is_high() {
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [{ "role": "user", "content": "hi" }],
        "thinking": { "type": "adaptive" }
    }))
    .unwrap();
    assert_eq!(canonical.reasoning_effort, Some(ReasoningEffort::High));
}

#[test]
fn disabled_thinking_sets_no_effort() {
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [{ "role": "user", "content": "hi" }],
        "thinking": { "type": "disabled" }
    }))
    .unwrap();
    assert_eq!(canonical.reasoning_effort, None);
}

#[test]
fn tool_result_is_error_is_kept_and_marked_upstream() {
    // is_error=true is preserved on the canonical message…
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [{
            "role": "user",
            "content": [
                { "type": "tool_result", "tool_use_id": "call_1", "content": "boom", "is_error": true },
                { "type": "tool_result", "tool_use_id": "call_2", "content": "ok", "is_error": false }
            ]
        }]
    }))
    .unwrap();
    assert!(canonical.messages[0].tool_error);
    assert_eq!(canonical.messages[0].text, "boom");
    assert!(!canonical.messages[1].tool_error);

    // …and rendered in-band for the upstream (which has no error flag).
    let mut messages = canonical.messages;
    messages.insert(
        0,
        raycast2api::core::message::Message {
            role: raycast2api::core::message::Role::Assistant,
            text: String::new(),
            thinking: None,
            tool_calls: vec![raycast2api::core::message::ToolCall {
                id: "call_1".into(),
                name: "Bash".into(),
                arguments: "{}".into(),
            }],
            tool_call_id: None,
            tool_name: None,
            tool_error: false,
        },
    );
    let mut canonical = raycast2api::core::request::CanonicalRequest::new(
        raycast2api::core::protocol::Protocol::Messages,
        "claude-x",
    );
    canonical.messages = messages;
    let built =
        raycast2api::raycast::builder::build_chat_request(&canonical, "anthropic", "claude-x");
    let err_msg = built
        .messages
        .iter()
        .find(|m| m.role == "tool" && m.content.text.contains("boom"))
        .expect("tool message with error present");
    assert!(
        err_msg.content.text.starts_with("[tool error] "),
        "failed result must be marked in-band: {:?}",
        err_msg.content.text
    );
    let ok_msg = built
        .messages
        .iter()
        .find(|m| m.role == "tool" && m.content.text.contains("ok"))
        .expect("tool message with success present");
    assert!(!ok_msg.content.text.starts_with("[tool error] "));
}

// --- Auto-mode safeguard results -----------------------------------------

#[test]
fn safeguards_field_flags_server_safety_check() {
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [{ "role": "user", "content": "hi" }],
        "safeguards": [{ "type": "dangerous_tool_use", "classifier_context": {"k": "v"} }]
    }))
    .unwrap();
    assert!(canonical.server_safety_check);

    let plain = parse(json!({
        "model": "claude-x",
        "messages": [{ "role": "user", "content": "hi" }]
    }))
    .unwrap();
    assert!(!plain.server_safety_check);
}

#[test]
fn streaming_tool_call_with_safety_check_carries_safeguard_results() {
    use raycast2api::core::event::CanonicalEvent as CE;
    use raycast2api::core::message::ToolCall;
    use raycast2api::core::response::FinishReason as FR;
    use raycast2api::core::usage::Usage;
    use raycast2api::protocol::messages::outbound::{MessagesOutbound, StreamEncoder};
    use raycast2api::protocol::OutboundAdapter;

    let events = futures_util::stream::iter(vec![
        CE::ToolCall(ToolCall {
            id: "toolu_1".into(),
            name: "Bash".into(),
            arguments: "{}".into(),
        }),
        CE::Finish {
            reason: FR::ToolCalls,
            usage: Usage::default(),
        },
    ]);
    // Safety check requested: the terminating message_delta carries
    // safeguard_results with an explicit verdict per tool call.
    let rendered = MessagesOutbound::stream_with_request(
        "claude-x",
        &{
            let mut r = raycast2api::core::request::CanonicalRequest::new(
                raycast2api::core::protocol::Protocol::Messages,
                "claude-x",
            );
            r.server_safety_check = true;
            r
        },
        events,
    );
    let bytes = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async { axum::body::to_bytes(rendered.into_body(), usize::MAX).await.unwrap() });
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let delta_line = text
        .lines()
        .find(|l| l.contains("safeguard_results"))
        .expect("safeguard_results on message_delta");
    let frame: serde_json::Value =
        serde_json::from_str(delta_line.trim_start_matches("data: ")).unwrap();
    let results = &frame["delta"]["safeguard_results"];
    assert_eq!(results["type"], "available");
    assert_eq!(
        results["tool_uses"]["toolu_1"]["type"],
        "unavailable",
        "gateway cannot classify; explicit unavailable keeps auto mode alive"
    );

    // No safety check requested: no safeguard_results in any frame.
    let events2 = futures_util::stream::iter(vec![CE::Finish {
        reason: FR::Stop,
        usage: Usage::default(),
    }]);
    let rendered2 = MessagesOutbound::stream_with_request(
        "claude-x",
        &raycast2api::core::request::CanonicalRequest::new(
            raycast2api::core::protocol::Protocol::Messages,
            "claude-x",
        ),
        events2,
    );
    let bytes2 = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async { axum::body::to_bytes(rendered2.into_body(), usize::MAX).await.unwrap() });
    assert!(!String::from_utf8(bytes2.to_vec()).unwrap().contains("safeguard_results"));

    // Encoder direct: finalize() without safeguards stays unchanged.
    let mut enc = StreamEncoder::new();
    let frames = enc.finalize();
    let dumped = frames
        .iter()
        .map(|f| f.data.to_string())
        .collect::<Vec<_>>()
        .join("");
    assert!(!dumped.contains("safeguard_results"));
}
