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
use crate::util::{current_date, derive_ulid, derive_uuid, new_ulid, new_uuid};

use super::wire::{RaycastChatRequest, RaycastMessage, RaycastRequestToolCall};

/// Raycast's base system prompt. The real client always sends this as
/// `system_instructions` and forwards any client-supplied system prompt as
/// `additional_system_instructions`, so we mirror that: `system_instructions` is
/// always this constant, never the caller's prompt. Matches the reference
/// implementation (and the previous OpenAI behavior) exactly.
const DEFAULT_SYSTEM_INSTRUCTIONS: &str = "markdown";

/// Default sampling temperature when the caller didn't set one.
const DEFAULT_TEMPERATURE: f64 = 0.5;

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
        temperature: canonical.temperature.unwrap_or(DEFAULT_TEMPERATURE),
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
        message_id: Some(derive_message_id(&canonical.messages)),
        buffer_id: Some(new_uuid()),
        current_date: Some(current_date()),
    }
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

/// Render canonical messages into Raycast messages, expanding assistant
/// thinking into its own preceding `<think>` turn.
fn build_messages(messages: &[Message]) -> Vec<RaycastMessage> {
    let mut out = Vec::with_capacity(messages.len());

    for message in messages {
        // Assistant thinking is carried as a separate assistant message whose
        // body is the thought wrapped in <think>...</think>, optionally
        // replaying the provider's thought signature.
        if let Some(thinking) = &message.thinking {
            let mut think_msg =
                RaycastMessage::text("assistant", format!("<think>{}</think>", thinking.text));
            if let Some(signature) = &thinking.signature {
                think_msg.extra_content = Some(json!({
                    "anthropic": { "thought_signature": signature }
                }));
            }
            out.push(think_msg);
        }

        let role = role_wire(message.role);
        let mut wire = RaycastMessage::text(role, message.text.clone());

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

        if message.role == Role::Tool {
            wire.tool_call_id = message.tool_call_id.clone();
            wire.name = message.tool_name.clone();
        }

        out.push(wire);
    }

    out
}

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
            ToolDef::Remote { name } => json!({
                "type": "remote_tool",
                "name": name,
            }),
        })
        .collect()
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
