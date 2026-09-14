//! Component ⑦ — the **Raycast SSE parser**.
//!
//! Maps one decoded `data:` payload into zero or more provider-agnostic
//! [`CanonicalEvent`]s. Notification / ping / stream-complete payloads carry no
//! content and yield nothing; a single terminating payload can yield several
//! events (its tool calls, then its `Finish`).
//!
//! It emits reasoning, tool-call, and usage events even though the migrated
//! OpenAI outbound only consumes text and finish for now — so the richer
//! protocols can be filled in without touching this layer.

use crate::core::event::CanonicalEvent;
use crate::core::message::ToolCall;
use crate::core::response::FinishReason;
use crate::core::usage::Usage;

use super::wire::{RaycastSseData, RaycastUsage};

/// The sentinel some SSE producers use to mark end-of-stream.
pub const DONE_SENTINEL: &str = "[DONE]";

/// Parse one decoded `data:` payload into canonical events.
///
/// Returns an empty vec for anything that isn't content: the `[DONE]`
/// sentinel, `{"complete":true}`, `provider_status` / `ping` notifications, or
/// a payload that fails to decode.
pub fn parse_events(payload: &str) -> Vec<CanonicalEvent> {
    if payload == DONE_SENTINEL {
        return Vec::new();
    }

    let Ok(data) = serde_json::from_str::<RaycastSseData>(payload) else {
        return Vec::new();
    };

    // Notifications and pings set `notification_type` and carry no content.
    if data.notification_type.is_some() {
        return Vec::new();
    }

    let mut events = Vec::new();

    // Reasoning end delivers the thought signature (with empty reasoning/text).
    if let Some(signature) = data.thought_signature() {
        events.push(CanonicalEvent::ThoughtSignature(signature));
    }

    if let Some(reasoning) = data.reasoning.as_deref()
        && !reasoning.is_empty()
    {
        events.push(CanonicalEvent::ReasoningDelta(reasoning.to_string()));
    }

    if let Some(text) = data.text.as_deref()
        && !text.is_empty()
    {
        events.push(CanonicalEvent::TextDelta(text.to_string()));
    }

    for call in data.tool_calls {
        events.push(CanonicalEvent::ToolCall(ToolCall {
            id: call.id,
            name: call.name,
            arguments: call.arguments,
        }));
    }

    if let Some(reason) = data.finish_reason.as_deref() {
        events.push(CanonicalEvent::Finish {
            reason: finish_reason(reason),
            usage: usage(data.usage),
        });
    }

    events
}

/// Map Raycast's `finish_reason` string to the canonical reason. Anything other
/// than `"tool_calls"` (including `"stop"`) is treated as a normal stop.
fn finish_reason(raw: &str) -> FinishReason {
    match raw {
        "tool_calls" => FinishReason::ToolCalls,
        _ => FinishReason::Stop,
    }
}

/// Convert Raycast usage into canonical usage (dropping cost fields).
fn usage(raw: Option<RaycastUsage>) -> Usage {
    let raw = raw.unwrap_or_default();
    Usage {
        input_tokens: raw.input_tokens,
        output_tokens: raw.output_tokens,
        cached_tokens: raw.cached_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_notifications_pings_and_sentinels() {
        assert!(parse_events(DONE_SENTINEL).is_empty());
        assert!(parse_events(r#"{"complete":true}"#).is_empty());
        assert!(
            parse_events(r#"{"notification_type":"ping","notification":"","text":""}"#).is_empty()
        );
        assert!(parse_events("not json").is_empty());
    }

    #[test]
    fn text_delta_becomes_text_event() {
        let events = parse_events(r#"{"text":"hello"}"#);
        assert_eq!(events.len(), 1);
        matches!(&events[0], CanonicalEvent::TextDelta(t) if t == "hello");
    }

    #[test]
    fn reasoning_and_signature_events() {
        let events = parse_events(r#"{"reasoning":"think","text":""}"#);
        assert!(matches!(events.as_slice(), [CanonicalEvent::ReasoningDelta(t)] if t == "think"));

        let events = parse_events(
            r#"{"reasoning":"","text":"","extra_content":{"anthropic":{"thought_signature":"sig"}}}"#,
        );
        assert!(matches!(events.as_slice(), [CanonicalEvent::ThoughtSignature(s)] if s == "sig"));
    }

    #[test]
    fn finish_with_usage() {
        let events = parse_events(
            r#"{"text":"","finish_reason":"stop","usage":{"input_tokens":5,"output_tokens":7,"cached_tokens":1}}"#,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            CanonicalEvent::Finish { reason, usage } => {
                assert_eq!(*reason, FinishReason::Stop);
                assert_eq!(usage.input_tokens, 5);
                assert_eq!(usage.output_tokens, 7);
                assert_eq!(usage.cached_tokens, 1);
            }
            other => panic!("expected Finish, got {other:?}"),
        }
    }

    #[test]
    fn tool_calls_then_finish() {
        let events = parse_events(
            r#"{"finish_reason":"tool_calls","text":"","tool_calls":[{"name":"ask","arguments":"{}","id":"t1"}],"usage":{"input_tokens":1,"output_tokens":2,"cached_tokens":0}}"#,
        );
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[0], CanonicalEvent::ToolCall(c) if c.id == "t1" && c.name == "ask")
        );
        assert!(matches!(
            &events[1],
            CanonicalEvent::Finish {
                reason: FinishReason::ToolCalls,
                ..
            }
        ));
    }
}
