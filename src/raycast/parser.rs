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

/// 排查提示,附在 content_policy_blocked 错误后。上游这类拦截由请求文本触发,
/// 换账号/重试都无效;根因通常是提示词里有"展示思考过程"之类的 CoT 提取指令。
const CONTENT_POLICY_HINT: &str = "\n\n[gateway hint] This block was triggered by the request content, not by the account or the gateway — retrying or switching accounts will not help. Common cause: the prompt (or a system-reminder injected into it) asks the model to reveal its thinking process / chain of thought, which the upstream provider's safety policy refuses. Rephrase those instructions (e.g. ask for structured conclusions instead of \"show your thinking process\") and resend.";

/// The content-policy troubleshooting hint, for error paths outside the SSE
/// parser (e.g. the peeked-first-payload rotation loop in the client).
pub fn content_policy_hint() -> &'static str {
    CONTENT_POLICY_HINT
}

/// Parse one decoded `data:` payload into canonical events.
///
/// Returns an empty vec for anything that isn't content: the `[DONE]`
/// sentinel, `{"complete":true}` (a normal stream end, recognized by
/// [`is_complete_sentinel`] so the stream layer treats it as termination),
/// `provider_status` / `ping` notifications, or a payload that fails to
/// decode.
pub fn parse_events(payload: &str, notifications: &mut Vec<String>) -> Vec<CanonicalEvent> {
    if payload == DONE_SENTINEL {
        return Vec::new();
    }

    let Ok(data) = serde_json::from_str::<RaycastSseData>(payload) else {
        return Vec::new();
    };

    // Remote-executed tool calls (`notification_type:"tool_calls"`): the
    // server has ALREADY run the tool inside the stream and its results ride
    // separately (generating_image + image URL for image gen). Forwarding the
    // call to the client would make harnesses (Codex/Claude Code) answer a
    // function_call the server already settled — drop it here, log it.
    if data.notification_type.as_deref() == Some("tool_calls") {
        for call in &data.tool_calls {
            tracing::info!("⇈ remote tool executed server-side: {} ({})", call.name, call.id);
        }
        return Vec::new();
    }

    // Notifications and pings set `notification_type` and carry no content.
    if data.notification_type.is_some()
        && let Some(notification) = data.notification.as_deref()
        && !notification.is_empty()
    {
        notifications.push(notification.to_string());
    }

    // Image generation lifecycle. The completion payload (top-level `image`
    // URL) becomes an [`CanonicalEvent::ImageGenerated`]; the stream layer
    // (decode_events) turns it into a markdown image, inlining the bytes as a
    // data URL. Failure arrives as a `tool_failed` notification with
    // `image_generation: true` — that one carries no user-actionable detail
    // beyond the notification text, which the error path already attaches.
    if data.notification_type.as_deref() == Some("generating_image") {
        return vec![CanonicalEvent::ImageGenerationStarted {
            size: data.image_size,
            count: data.image_count,
        }];
    }

    // An in-stream rejection (200 status, error payload inside the stream).
    // Surfaced as a first-class event so no outbound adapter can silently
    // swallow it — previously this arm returned `Vec::new()` and the client
    // saw an empty completion.
    if let Some(err) = data.error {
        let hint = err.is_content_policy_blocked();
        let mut message = err
            .message
            .unwrap_or_else(|| "upstream error (no message provided)".to_string());
        if hint {
            message.push_str(CONTENT_POLICY_HINT);
        }
        if !notifications.is_empty() {
            message.push_str(" (upstream notification: ");
            message.push_str(&notifications.join("; "));
            message.push(')');
            notifications.clear();
        }
        let status = if err.http_status != 0 {
            err.http_status
        } else {
            502
        };
        tracing::warn!(status, %message, "upstream in-stream error");
        return vec![CanonicalEvent::Error { status, message }];
    }

    if data.notification_type.is_some() {
        return Vec::new();
    }

    let mut events = Vec::new();

    // Reasoning end delivers the thought signature (with empty reasoning/text).
    if let Some(signature) = data.thought_signature() {
        events.push(CanonicalEvent::ThoughtSignature(signature));
    }

    if let Some(url) = data.image.as_deref()
        && !url.is_empty()
    {
        tracing::info!("→ image generated: {url}");
        events.push(CanonicalEvent::ImageGenerated { url: url.to_string() });
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
    } else if data.complete == Some(true) {
        // `{"complete":true}` with no finish_reason: the upstream's normal
        // end-of-stream on tool-heavy turns (image generation ends this way,
        // usage arriving on the preceding payload). Without a synthetic
        // Finish here the stream layer would treat the clean end as a
        // truncation and fail the request after all output was delivered.
        events.push(CanonicalEvent::Finish {
            reason: FinishReason::Stop,
            usage: Usage::default(),
        });
    }

    events
}

/// Preserve provider termination reasons through every outbound adapter.
fn finish_reason(raw: &str) -> FinishReason {
    match raw {
        "tool_calls" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
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
    fn in_stream_error_payload_becomes_error_event() {
        // Real capture: Raycast rejects an exhausted account inside a 200 SSE
        // stream instead of returning an HTTP error status.
        let payload = r#"{"error":{"message":"You've reached your free trial limit of 25 AI requests per hour. Upgrade to Raycast Pro to continue, or try again in 7 minutes.","type":"model_rate_limit_reached_error","category":"usage_limit","code":"usage_limit_reached","subcode":"model_usage_limited","retryable":true,"request_id":"334ff406","retry_after":447,"http_status":429}}"#;
        let events = parse_events(payload, &mut Vec::new());
        assert_eq!(events.len(), 1);
        match &events[0] {
            CanonicalEvent::Error { status, message } => {
                assert_eq!(*status, 429);
                assert!(message.contains("free trial limit"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn content_policy_block_gets_gateway_hint() {
        // Real capture: the provider's safety policy blocked the request
        // content (CoT-extraction phrasing in a Claude Code system reminder).
        let payload = r#"{"error":{"message":"Your request was blocked by the AI provider's safety policy. Please rephrase your message and try again.","type":"refusal_error","category":"provider","code":"content_policy_blocked","subcode":"refusal_blocked","retryable":false,"request_id":"415c5146","http_status":400}}"#;
        let events = parse_events(payload, &mut Vec::new());
        match &events[0] {
            CanonicalEvent::Error { status, message } => {
                assert_eq!(*status, 400);
                assert!(message.contains("safety policy"));
                assert!(message.contains("[gateway hint]"));
                assert!(message.contains("thinking process"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn content_policy_hint_not_appended_for_other_errors() {
        let payload = r#"{"error":{"message":"explicit error","http_status":502}}"#;
        let events = parse_events(payload, &mut Vec::new());
        match &events[0] {
            CanonicalEvent::Error { message, .. } => {
                assert!(!message.contains("[gateway hint]"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// 生图生命周期:`generating_image` 通知 → started 事件;带 `image` URL
    /// 的 chunk → markdown 图文 delta(三个协议客户端都能渲染)。
    #[test]
    fn image_generation_lifecycle_yields_started_and_image_event() {
        let mut notifications = Vec::new();
        let events = parse_events(
            r#"{"notification_type":"generating_image","image_size":"1024x1024","image_count":2}"#,
            &mut notifications,
        );
        match &events[0] {
            CanonicalEvent::ImageGenerationStarted { size, count } => {
                assert_eq!(size.as_deref(), Some("1024x1024"));
                assert_eq!(*count, Some(2));
            }
            other => panic!("expected ImageGenerationStarted, got {other:?}"),
        }

        // 生成完成现在发 ImageGenerated,由 stream 层下载转 data URL。
        let events = parse_events(
            r#"{"image":"https://cdn.example.com/gen/abc.png","text":""}"#,
            &mut Vec::new(),
        );
        match &events[0] {
            CanonicalEvent::ImageGenerated { url } => {
                assert_eq!(url, "https://cdn.example.com/gen/abc.png");
            }
            other => panic!("expected ImageGenerated, got {other:?}"),
        }
    }

    #[test]
    fn ignores_notifications_pings_and_sentinels() {
        let mut notifications = Vec::new();
        assert!(parse_events(DONE_SENTINEL, &mut notifications).is_empty());
        assert!(
            parse_events(
                r#"{"notification_type":"ping","notification":"","text":""}"#,
                &mut notifications
            )
            .is_empty()
        );
        assert!(parse_events("not json", &mut notifications).is_empty());
    }

    /// `{"complete":true}` 是生图流的正常收尾(usage 在前一 payload):
    /// 必须发 Finish,否则 stream 层把干净收尾误判为断流。
    #[test]
    fn complete_sentinel_terminates_the_stream_normally() {
        let events = parse_events(r#"{"complete":true}"#, &mut Vec::new());
        assert!(
            matches!(events.as_slice(), [CanonicalEvent::Finish { reason: FinishReason::Stop, usage } ]
                if usage.input_tokens == 0 && usage.output_tokens == 0)
        );
        // finish_reason 优先:两者同时出现时只发一个 Finish。
        let events = parse_events(
            r#"{"complete":true,"finish_reason":"stop","usage":{"input_tokens":5,"output_tokens":2}}"#,
            &mut Vec::new(),
        );
        assert!(matches!(events.as_slice(), [CanonicalEvent::Finish { usage, .. }] if usage.input_tokens == 5));
    }

    /// 服务端已执行的 remote tool_call(notification_type:"tool_calls"):
    /// 不下发 ToolCall 事件(harness 会去应答一个上游已结算的调用),
    /// 只留日志。
    #[test]
    fn server_executed_remote_tool_calls_are_dropped_not_forwarded() {
        let mut notifications = Vec::new();
        let events = parse_events(
            r#"{"text":"","notification":"","notification_type":"tool_calls","tool_calls":[{"name":"gpt_image","arguments":"{\"prompt\":\"cat\"}","id":"call_1"}]}"#,
            &mut notifications,
        );
        assert!(events.is_empty(), "remote tool calls must not reach the client");
    }

    #[test]
    fn preceding_notification_is_attached_to_stream_error() {
        let mut notifications = Vec::new();
        assert!(parse_events(
            r#"{"notification_type":"model_updated","notification":"No valid Advanced AI subscription. Using claude-haiku-5-5 instead"}"#,
            &mut notifications,
        )
        .is_empty());

        let events = parse_events(
            r#"{"error":{"message":"Something went wrong. Please try again in a few minutes.","http_status":500}}"#,
            &mut notifications,
        );
        assert!(matches!(
            events.as_slice(),
            [CanonicalEvent::Error { status: 500, message }]
                if message.contains("Something went wrong")
                    && message.contains("No valid Advanced AI subscription")
                    && message.contains("Using claude-haiku-5-5 instead")
        ));
        assert!(notifications.is_empty());
    }

    #[test]
    fn text_delta_becomes_text_event() {
        let events = parse_events(r#"{"text":"hello"}"#, &mut Vec::new());
        assert_eq!(events.len(), 1);
        matches!(&events[0], CanonicalEvent::TextDelta(t) if t == "hello");
    }

    #[test]
    fn reasoning_and_signature_events() {
        let events = parse_events(r#"{"reasoning":"think","text":""}"#, &mut Vec::new());
        assert!(matches!(events.as_slice(), [CanonicalEvent::ReasoningDelta(t)] if t == "think"));

        let events = parse_events(
            r#"{"reasoning":"","text":"","extra_content":{"anthropic":{"thought_signature":"sig"}}}"#,
            &mut Vec::new(),
        );
        assert!(matches!(events.as_slice(), [CanonicalEvent::ThoughtSignature(s)] if s == "sig"));
    }

    #[test]
    fn finish_with_usage() {
        let events = parse_events(
            r#"{"text":"","finish_reason":"stop","usage":{"input_tokens":5,"output_tokens":7,"cached_tokens":1}}"#,
            &mut Vec::new(),
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
            &mut Vec::new(),
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
