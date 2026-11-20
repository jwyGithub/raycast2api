//! OpenAI Chat Completions → canonical request.
//!
//! Pass-through by design: this only reshapes the body into the shared model
//! (the Raycast builder then wraps it for the upstream). It does not police
//! options, tool definitions or tool-call history — whatever the upstream can't
//! handle it reports itself. The only local rejections are the ones the gateway
//! cannot route or represent: a missing `model` and an unknown message role.

use serde_json::{Value, json};
use std::collections::HashMap;

use super::wire::{ChatCompletionRequest, Content, InboundToolChoice};
use crate::core::message::{Message, Role, ToolCall};
use crate::core::protocol::Protocol;
use crate::core::request::{CanonicalRequest, ReasoningEffort};
use crate::core::tools::{ToolChoice, ToolDef};
use crate::error::GatewayError;
use crate::protocol::InboundAdapter;
use crate::protocol::validate::require_model;

pub struct ChatCompletionsInbound;

impl InboundAdapter for ChatCompletionsInbound {
    type Request = ChatCompletionRequest;

    fn parse(request: ChatCompletionRequest) -> Result<CanonicalRequest, GatewayError> {
        let model = require_model(request.model)?;
        let mut canonical = CanonicalRequest::new(Protocol::ChatCompletions, model);
        canonical.stream = request.stream;
        // Forwarded verbatim; no default. The real Raycast client never sends
        // `temperature` upstream (captures 2026-11-13/20), so injecting the
        // Chat-API default of 1.0 here would put a field on the wire the
        // official client never carries — a fingerprint. The upstream default
        // applies whenever the caller omitted it, exactly as it does for them.
        canonical.temperature = request.temperature;
        // Values the upstream has no slot for (e.g. "minimal") are not forwarded.
        canonical.reasoning_effort = match request.reasoning_effort.as_deref() {
            Some("low") => Some(ReasoningEffort::Low),
            Some("medium") => Some(ReasoningEffort::Medium),
            Some("high") => Some(ReasoningEffort::High),
            Some("xhigh") => Some(ReasoningEffort::XHigh),
            _ => None,
        };
        canonical.requested_effort = request.reasoning_effort;
        canonical.include_usage = request.stream_options.is_some_and(|o| o.include_usage);

        for tool in request.tools.unwrap_or_default() {
            // Raycast takes function tools only; other kinds have no upstream shape.
            if tool.kind != "function" {
                continue;
            }
            let Some(function) = tool.function else {
                continue;
            };
            canonical.tools.push(ToolDef::Local {
                name: function.name,
                description: function.description,
                parameters: function
                    .parameters
                    .unwrap_or_else(|| json!({"type":"object","properties":{}})),
            });
        }
        canonical.tool_choice = match request.tool_choice {
            Some(InboundToolChoice::Mode(mode)) => match mode.as_str() {
                "none" => Some(ToolChoice::None),
                "auto" => Some(ToolChoice::Auto),
                "required" => Some(ToolChoice::Required),
                _ => None,
            },
            Some(InboundToolChoice::Named { function, .. }) => {
                Some(ToolChoice::Named(function.name))
            }
            Some(InboundToolChoice::Other(_)) | None => None,
        };

        // Raycast tool results carry the tool's name; recover it from the call.
        let mut call_names: HashMap<String, String> = HashMap::new();
        for message in request.messages {
            let role = match message.role.as_str() {
                "system" | "developer" => Role::System,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "tool" => Role::Tool,
                _ => {
                    return Err(invalid(format!(
                        "Unsupported message role '{}'",
                        message.role
                    )));
                }
            };
            let mut converted = Message::text(role, content_text(message.content));
            for call in message.tool_calls.unwrap_or_default() {
                call_names.insert(call.id.clone(), call.function.name.clone());
                converted.tool_calls.push(ToolCall {
                    id: call.id,
                    name: call.function.name,
                    arguments: call.function.arguments,
                });
            }
            if role == Role::Tool {
                converted.tool_name = message.name.or_else(|| {
                    message
                        .tool_call_id
                        .as_ref()
                        .and_then(|id| call_names.get(id).cloned())
                });
                converted.tool_call_id = message.tool_call_id;
            }
            if role == Role::System {
                // Raycast accepts instructions separately from conversation messages.
                match &mut canonical.system {
                    Some(system) => {
                        system.push('\n');
                        system.push_str(&converted.text);
                    }
                    None => canonical.system = Some(converted.text),
                }
            } else {
                canonical.messages.push(converted);
            }
        }
        Ok(canonical)
    }
}

/// Flatten message content to text. Raycast message bodies are `{text}` only,
/// so non-text parts (images, audio, ...) have no upstream slot and are skipped.
fn content_text(content: Option<Content>) -> String {
    match content {
        Some(Content::Text(text)) => text,
        Some(Content::Parts(parts)) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect(),
        None => String::new(),
    }
}

fn invalid(message: impl Into<String>) -> GatewayError {
    GatewayError::BadRequest(message.into())
}
