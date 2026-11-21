//! Component ④ — the **Raycast request builder**.
//!
//! Turns a protocol-agnostic [`CanonicalRequest`] into the concrete
//! [`RaycastChatRequest`] the backend expects, applying every Raycast-specific
//! rule in one place: wrapping message bodies in `{text}`, emitting thinking as
//! a standalone `<think>...</think>` assistant turn, nesting tool calls in the
//! OpenAI request shape, rendering tool/`tool_choice` definitions, and carrying
//! `reasoning_effort` for think models.
//!
//! Per the backend contract (`doc/raycast-backend-protocol.md`) every request
//! carries a `thread_id`, a `message_id`, a `buffer_id`, and a UTC
//! `current_date`. These mirror the real client's behaviour, confirmed against a
//! captured session (`charles/think_tools.har`). `thread_id` is one value for
//! the whole conversation (session affinity), derived from the first user
//! message (see [`derive_thread_id`]). `message_id` is one value per user turn:
//! held constant across the assistant/tool round-trips that resolve a single
//! user message and changing only when a new user message arrives, derived from
//! the transcript prefix up to and including the last user message (see
//! [`derive_message_id`]). `buffer_id` is minted fresh for every request. The
//! remaining enrichment fields (tools, thinking, tool_choice, ...) are all
//! `skip_serializing_if`-guarded, so a plain OpenAI text request stays minimal.

use serde_json::{Value, json};

use crate::core::message::{Message, Role};
use crate::core::request::CanonicalRequest;
use crate::core::tools::{ToolChoice, ToolDef};
use crate::util::{current_timestamp, derive_ulid, derive_uuid, new_ulid, new_uuid};

use super::wire::{PendingUpload, RaycastChatRequest, RaycastMessage, RaycastRequestToolCall};

/// Raycast's base system prompt. The real client always sends this as
/// `system_instructions` and forwards any client-supplied system prompt as
/// `additional_system_instructions`, so we mirror that: `system_instructions` is
/// always this constant, never the caller's prompt. Matches the reference
/// implementation (and the previous OpenAI behavior) exactly.
const DEFAULT_SYSTEM_INSTRUCTIONS: &str = "markdown";

/// Build the Raycast chat request from a canonical request and the already
/// resolved upstream `provider` / `model`.
pub fn build_chat_request(
    canonical: &CanonicalRequest,
    provider: &str,
    model: &str,
) -> RaycastChatRequest {
    RaycastChatRequest {
        model: model.to_string(),
        provider: provider.to_string(),
        messages: build_messages(&canonical.messages),
        // Raycast's base prompt is always `system_instructions`; the caller's
        // system prompt (if any) rides along as `additional_system_instructions`.
        system_instructions: DEFAULT_SYSTEM_INSTRUCTIONS.to_string(),
        // The real client never sends `temperature` on chat requests (confirmed
        // in the 2026-11-13/20 captures); sending it is a fingerprint, so it is
        // forwarded only when the caller explicitly set one.
        temperature: canonical.temperature,
        source: "ai_chat".to_string(),
        locale: "zh-CN".to_string(),
        thread_id: derive_thread_id(&canonical.messages),
        tools: build_tools(&canonical.tools),
        additional_system_instructions: canonical.system.clone(),
        reasoning_effort: canonical.reasoning_effort.map(|e| e.as_str().to_string()),
        // Mirror the real client: when tools are offered but the caller didn't
        // pick a policy, default to `auto` rather than omitting the field.
        tool_choice: canonical
            .tool_choice
            .as_ref()
            .map(tool_choice_wire)
            .or_else(|| (!canonical.tools.is_empty()).then(|| "auto".to_string())),
        // The real client prefixes the turn uuid with `run-` (captures
        // 2026-11-13/20: `run-97c33d75-...`).
        message_id: Some(format!("run-{}", derive_message_id(&canonical.messages))),
        buffer_id: Some(new_uuid()),
        // Full ISO-8601 UTC timestamp, as the real client sends it since at
        // least 2.12.0 (`2026-11-13T01:45:10.062Z`), not a bare date.
        current_date: Some(current_timestamp()),
        pending_uploads: pending_uploads(&canonical.messages),
    }
}

/// Collect the image bytes the dispatcher must upload, in wire-message order.
/// Wire indices match `build_messages` output positionally (one canonical
/// message in, one wire message out). Each attachment becomes its own
/// `PendingUpload`; the dispatcher appends the file id to the message's
/// `attachments` array as uploads complete.
///
/// Shares [`IMAGE_REPLAY_LIMIT`] with `build_messages`: the newest 20
/// attachments queue for upload, older ones were already degraded to text
/// placeholders there and are not uploaded. Walks the same newest-first
/// direction as `build_messages` so both agree on which attachments survive.
fn pending_uploads(messages: &[Message]) -> Vec<PendingUpload> {
    let mut uploads = Vec::new();
    let mut replay_budget = IMAGE_REPLAY_LIMIT;
    for (index, message) in messages.iter().enumerate().rev() {
        if message.role != Role::User {
            continue;
        }
        for attachment in &message.attachments {
            if replay_budget == 0 {
                continue;
            }
            let crate::core::message::Attachment::Inline { media_type, data } = attachment
            else {
                // Remote references are resolved by the pipeline before it
                // reaches here; a leftover one has no bytes to upload.
                continue;
            };
            replay_budget -= 1;
            uploads.push(PendingUpload {
                message_index: index,
                media_type: media_type.clone(),
                data: data.clone(),
            });
        }
    }
    // Uploads execute oldest-first (stable file ids per message position).
    uploads.reverse();
    uploads
}

/// Derive a **stable** `thread_id` so every turn of the same conversation maps
/// to the same value (session affinity: lets the backend group a thread's turns
/// and preserves prompt-cache locality). We anchor on the first user message,
/// which is invariant across turns; hashing the whole transcript instead would
/// change every turn (it grows as the chat proceeds) and defeat the purpose.
/// With no user message to anchor on, fall back to a fresh random ULID.
fn derive_thread_id(messages: &[Message]) -> String {
    match messages.iter().find(|m| m.role == Role::User) {
        Some(anchor) => derive_ulid(anchor.text.as_bytes()),
        None => new_ulid(),
    }
}

/// Derive the `message_id` for the current **user turn**. The real client keeps
/// this constant across the assistant/tool round-trips that resolve one user
/// message and only rotates it when a new user message is sent — confirmed in
/// `charles/think_tools.har`, where `message_id` stayed fixed while tool calls
/// resolved (message counts grew, id unchanged) and changed on each new user
/// turn. We reproduce that from a stateless gateway by hashing the transcript
/// prefix up to and including the last user message: invariant across the
/// trailing assistant/tool messages a round-trip appends, yet distinct once a
/// new user message extends the prefix. With no user message, fall back to a
/// fresh random UUID.
fn derive_message_id(messages: &[Message]) -> String {
    match messages.iter().rposition(|m| m.role == Role::User) {
        Some(last_user) => {
            let mut seed = Vec::new();
            for m in &messages[..=last_user] {
                seed.extend_from_slice(role_wire(m.role).as_bytes());
                seed.push(b':');
                seed.extend_from_slice(m.text.as_bytes());
                seed.push(b'\n');
            }
            derive_uuid(&seed)
        }
        None => new_uuid(),
    }
}

/// Render canonical messages into Raycast messages. Assistant thinking is
/// **never replayed**: the upstream content filter re-scans `<think>` text and
/// probabilistically blocks whole requests with `content_policy_blocked`
/// (observed 2026-11-12/13; stripping all thinking blocks from the offending
/// 452KB request let it pass verbatim). Neither official client replays it
/// either — the Raycast app drops it across turns, Codex even mid-loop — and
/// the Raycast backend never validates `thought_signature` on replay (both
/// captures succeed with signatures entirely absent). The signature therefore
/// rides only on the terminal assistant message of the CURRENT turn, where it
/// keeps the tool round-trip alive; a naive "keep the loop after the last
/// user message" gate failed because client-adjacent user texts like
/// "Tool loaded." (emitted alongside tool results) masquerade as a new turn.
///
/// Image attachments: the newest [`IMAGE_REPLAY_LIMIT`] attachments ride as
/// `pending_uploads` (bytes sidecar, see [`pending_uploads`]); the dispatcher
/// uploads after account selection and fills the ids. Older ones are stripped
/// here and their text gains the same omission placeholder the client uses.
fn build_messages(messages: &[Message]) -> Vec<RaycastMessage> {
    let mut out = Vec::with_capacity(messages.len());

    // The final assistant message of the last turn — the one with tool_calls,
    // else the last assistant text turn. Per the captured protocol
    // (doc/raycast-backend-protocol.md §2.2) the signature rides there. Only
    // that message keeps its signature; attaching one to content upstream
    // never saw would make the backend reject the request.
    let signature_index = messages
        .iter()
        .rposition(|m| m.role == Role::Assistant)
        .unwrap_or(usize::MAX);

    // Replay limit: only the newest IMAGE_REPLAY_LIMIT attachments survive.
    // Walk newest-first, counting; older image-bearing messages degrade.
    let mut replay_budget = IMAGE_REPLAY_LIMIT;
    let degrade = |text: &mut String, count: usize| {
        for _ in 0..count {
            text.push(' ');
            text.push_str(crate::core::message::IMAGE_OMITTED_PLACEHOLDER);
        }
    };

    for (index, message) in messages.iter().enumerate().rev() {
        let image_count = message.attachments.len();
        let degrade_count = if message.role == Role::User && image_count > 0 {
            let kept = image_count.min(replay_budget);
            replay_budget -= kept;
            image_count - kept
        } else {
            0
        };

        // Assistant thinking is carried as a separate assistant message whose
        // body is the thought wrapped in <think>...</think>. The provider's
        // thought signature is NOT attached here: upstream validates the
        // signature against the message that ended the assistant turn —
        // replaying it on the `<think>` text message makes the backend reject
        // the whole request with a 500 ("Something went wrong").
        let signature_json = if index == signature_index {
            message.thinking.as_ref().and_then(|t| {
                t.signature.as_ref().map(|s| {
                    json!({
                        "anthropic": { "thought_signature": s }
                    })
                })
            })
        } else {
            None
        };

        let role = role_wire(message.role);
        let mut wire = RaycastMessage::text(role, message.text.clone());

        // A failed tool result is marked in-band: the upstream wire carries no
        // error flag on `role:"tool"` messages, so the failure rides in the
        // text the model reads.
        if message.role == Role::Tool && message.tool_error {
            wire.content.text = format!("{}{}", Message::error_marker(true), wire.content.text);
        }

        // Dropped (over-limit) attachments become omission placeholders;
        // kept ones ride via pending_uploads — no wire field yet.
        if degrade_count > 0 {
            degrade(&mut wire.content.text, degrade_count);
        }

        if !message.tool_calls.is_empty() {
            wire.tool_calls = message
                .tool_calls
                .iter()
                .map(|call| {
                    RaycastRequestToolCall::function(
                        call.id.clone(),
                        call.name.clone(),
                        call.arguments.clone(),
                    )
                })
                .collect();
        }

        if !wire.tool_calls.is_empty() || message.thinking.is_none() {
            wire.extra_content = signature_json;
        }

        if message.role == Role::Tool {
            wire.tool_call_id = message.tool_call_id.clone();
            wire.name = message.tool_name.clone();
        }

        out.push(wire);
    }

    // The loop walked newest-first; restore wire order.
    out.reverse();
    out
}

/// How many image attachments a request replays (the real client's
/// `lOt(messages, 20)`); older ones are replaced with omission placeholders.
const IMAGE_REPLAY_LIMIT: usize = 20;

/// Render canonical tool definitions into Raycast's `tools[]` JSON.
fn build_tools(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| match tool {
            ToolDef::Local {
                name,
                description,
                parameters,
            } => json!({
                "type": "local_tool",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                }
            }),
            // A freeform tool has no JSON Schema; bridge it as a single
            // `input` string parameter with the grammar in the description.
            ToolDef::Custom {
                name,
                description,
                grammar,
            } => json!({
                "type": "local_tool",
                "function": {
                    "name": name,
                    "description": custom_description(description, grammar),
                    "parameters": {
                        "type": "object",
                        "properties": { "input": { "type": "string" } },
                        "required": ["input"],
                        "additionalProperties": false,
                    },
                }
            }),
            ToolDef::Remote { name } => json!({
                "type": "remote_tool",
                "name": name,
            }),
        })
        .collect()
}

/// A freeform tool's description with its input grammar appended, so the
/// model sees how to format the `input` string.
fn custom_description(description: &Option<String>, grammar: &str) -> String {
    match description {
        Some(d) if !d.is_empty() => format!("{d}\n\nInput grammar:\n{grammar}"),
        _ => format!("Input grammar:\n{grammar}"),
    }
}

/// The Raycast wire value for a role.
fn role_wire(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// The Raycast wire value for a tool-choice policy.
fn tool_choice_wire(choice: &ToolChoice) -> String {
    match choice {
        ToolChoice::Auto => "auto".to_string(),
        ToolChoice::None => "none".to_string(),
        ToolChoice::Required => "required".to_string(),
        ToolChoice::Named(name) => name.clone(),
    }
}

#[cfg(test)]
mod image_tests {
    use super::*;
    use base64::Engine;
    use crate::core::message::Attachment;

    fn sample_png() -> Attachment {
        // 1x1 PNG bytes.
        let data = base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
            .unwrap();
        Attachment::Inline { media_type: "image/png".into(), data }
    }

    fn request_with(messages: Vec<Message>) -> RaycastChatRequest {
        let mut canonical =
            crate::core::request::CanonicalRequest::new(crate::core::protocol::Protocol::ChatCompletions, "m");
        canonical.messages = messages;
        build_chat_request(&canonical, "openai", "test")
    }

    /// 指纹回归:无图请求的 wire 字节必须与加图功能之前完全一致。
    /// pending_uploads 是 serde(skip) 侧车,不参与序列化。
    #[test]
    fn plain_text_request_wire_is_unchanged_by_image_support() {
        let mut canonical =
            crate::core::request::CanonicalRequest::new(crate::core::protocol::Protocol::ChatCompletions, "m");
        canonical.messages = vec![
            Message::text(Role::System, "Be brief"),
            Message::text(Role::User, "hello"),
        ];
        canonical.system = Some("Be brief".into());
        let request = build_chat_request(&canonical, "openai", "test");
        let json = serde_json::to_value(&request).unwrap();
        let messages = json["messages"].as_array().unwrap();
        // 每条消息只有 role/content,content 只有 text — 没有 attachments 字段。
        for message in messages {
            let obj = message.as_object().unwrap();
            assert!(obj.keys().all(|k| k == "role" || k == "content"), "{obj:?}");
            assert!(obj["content"].as_object().unwrap().contains_key("text"));
            assert!(!obj["content"].as_object().unwrap().contains_key("attachments"));
        }
        assert!(request.pending_uploads.is_empty());
    }

    /// 带图用户消息:pending_uploads 按消息序号排队,序列化时不出现
    /// (fileId 由 dispatcher 上传后回填才入 wire)。
    #[test]
    fn image_messages_queue_pending_uploads_with_message_index() {
        let request = request_with(vec![
            Message::text(Role::User, "first"),
            Message::with_attachments(Role::User, "look", vec![sample_png()]),
        ]);
        assert_eq!(request.pending_uploads.len(), 1);
        assert_eq!(request.pending_uploads[0].message_index, 1);
        assert_eq!(request.pending_uploads[0].media_type, "image/png");
        let json = serde_json::to_value(&request).unwrap();
        let messages = json["messages"].as_array().unwrap();
        // wire 上暂无 attachments(等 fileId 回填),文本未动。
        assert!(!messages[1]["content"].as_object().unwrap().contains_key("attachments"));
        assert_eq!(messages[1]["content"]["text"], "look");
    }

    /// 回放限制:超过 20 张图的更早附件剥除,文本加占位符;最近的保留。
    #[test]
    fn older_images_over_replay_limit_get_omission_placeholder() {
        let mut messages = Vec::new();
        for i in 0..25 {
            messages.push(Message::with_attachments(
                Role::User,
                format!("msg{i}"),
                vec![sample_png()],
            ));
        }
        let request = request_with(messages);
        // 20 个进 pending_uploads(最新的)。
        assert_eq!(request.pending_uploads.len(), 20);
        assert_eq!(request.pending_uploads[0].message_index, 5);
        let json = serde_json::to_value(&request).unwrap();
        let wire = json["messages"].as_array().unwrap();
        // msg0..msg4 被降级:文本带占位符。
        for (index, wire_message) in wire.iter().enumerate().take(5) {
            let text = wire_message["content"]["text"].as_str().unwrap();
            assert!(text.contains(crate::core::message::IMAGE_OMITTED_PLACEHOLDER), "msg{index}: {text}");
        }
        // msg5 之后文本原样。
        assert_eq!(wire[5]["content"]["text"], "msg5");
    }

    /// thread_id 派生不受附件影响(附件不进 seed)——同一会话发图前后
    /// thread_id 稳定,与官方客户端的 chat 生命周期一致。
    #[test]
    fn thread_id_ignores_attachments() {
        let plain = request_with(vec![Message::text(Role::User, "hi")]);
        let with_image =
            request_with(vec![Message::with_attachments(Role::User, "hi", vec![sample_png()])]);
        assert_eq!(plain.thread_id, with_image.thread_id);
    }
}
