//! OpenAI Chat Completions validation and normalization into the shared model.

use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

use super::wire::{ChatCompletionRequest, ChatMessage, Content, InboundToolChoice};
use crate::core::message::{Message, Role, ToolCall};
use crate::core::protocol::Protocol;
use crate::core::request::{CanonicalRequest, ReasoningEffort};
use crate::core::tools::{ToolChoice, ToolDef};
use crate::error::GatewayError;
use crate::protocol::InboundAdapter;

pub struct ChatCompletionsInbound;

impl InboundAdapter for ChatCompletionsInbound {
    type Request = ChatCompletionRequest;

    fn parse(request: ChatCompletionRequest) -> Result<CanonicalRequest, GatewayError> {
        let model = request
            .model
            .filter(|m| !m.trim().is_empty())
            .ok_or_else(|| invalid("'model' is required and must not be empty"))?;
        if request.messages.is_empty() {
            return Err(invalid("'messages' must not be empty"));
        }
        validate_options(&request.extra)?;
        if let Some(temperature) = request.temperature
            && !(0.0..=2.0).contains(&temperature)
        {
            return Err(invalid("'temperature' must be between 0 and 2"));
        }
        let mut canonical = CanonicalRequest::new(Protocol::ChatCompletions, model);
        canonical.stream = request.stream;
        // Match the Chat API default without changing the other protocols.
        canonical.temperature = Some(request.temperature.unwrap_or(1.0));
        canonical.requested_effort = request.reasoning_effort.clone();
        canonical.reasoning_effort = request
            .reasoning_effort
            .as_deref()
            .map(|effort| match effort {
                "low" => Ok(ReasoningEffort::Low),
                "medium" => Ok(ReasoningEffort::Medium),
                "high" => Ok(ReasoningEffort::High),
                "xhigh" => Ok(ReasoningEffort::XHigh),
                _ => Err(invalid(
                    "Supported 'reasoning_effort' values: low, medium, high, xhigh",
                )),
            })
            .transpose()?;
        if let Some(options) = request.stream_options {
            if !request.stream {
                return Err(invalid("'stream_options' requires 'stream: true'"));
            }
            for (key, value) in options.extra {
                if key != "include_obfuscation" || value != false {
                    return Err(invalid(format!("Unsupported stream option '{key}'")));
                }
            }
            canonical.include_usage = options.include_usage;
        }

        let mut names = HashSet::new();
        let tools = request.tools.unwrap_or_default();
        if tools.len() > 128 {
            return Err(invalid("At most 128 function tools are supported"));
        }
        for tool in tools {
            if tool.kind != "function" {
                return Err(invalid("Only 'function' tools are supported"));
            }
            let function = tool.function;
            validate_name(&function.name)?;
            if !names.insert(function.name.clone()) {
                return Err(invalid(format!("Duplicate tool name '{}'", function.name)));
            }
            if function.strict == Some(true) {
                return Err(invalid("'strict: true' is not supported by the upstream"));
            }
            let parameters = function
                .parameters
                .unwrap_or_else(|| json!({"type":"object","properties":{}}));
            if !parameters.is_object() {
                return Err(invalid("Tool 'parameters' must be a JSON Schema object"));
            }
            canonical.tools.push(ToolDef::Local {
                name: function.name,
                description: function.description,
                parameters,
            });
        }
        canonical.tool_choice = request
            .tool_choice
            .map(|choice| match choice {
                InboundToolChoice::Mode(mode) => match mode.as_str() {
                    "none" => Ok(ToolChoice::None),
                    "auto" => Ok(ToolChoice::Auto),
                    "required" if !names.is_empty() => Ok(ToolChoice::Required),
                    _ => Err(invalid("Invalid 'tool_choice'; 'required' needs tools")),
                },
                InboundToolChoice::Named { kind, function } => {
                    if kind != "function" || !names.contains(&function.name) {
                        return Err(invalid("'tool_choice' must select a defined function tool"));
                    }
                    Ok(ToolChoice::Named(function.name))
                }
            })
            .transpose()?;

        let mut pending = HashMap::new();
        let mut seen = HashSet::new();
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
            if role != Role::Tool && !pending.is_empty() {
                return Err(invalid(
                    "Assistant 'tool_calls' must be followed by all corresponding tool results",
                ));
            }
            validate_message_fields(&message, role)?;
            if message.tool_calls.as_ref().is_some_and(Vec::is_empty) {
                return Err(invalid("'tool_calls' must not be empty when provided"));
            }
            let calls = message.tool_calls.unwrap_or_default();
            let text = content_text(
                message.content,
                role == Role::Assistant && !calls.is_empty(),
            )?;
            let mut converted = Message::text(role, text);
            for call in calls {
                if call.kind != "function" || call.id.trim().is_empty() {
                    return Err(invalid(
                        "Tool calls require type 'function' and a nonempty 'id'",
                    ));
                }
                validate_name(&call.function.name)?;
                if !seen.insert(call.id.clone()) {
                    return Err(invalid(format!("Duplicate tool call id '{}'", call.id)));
                }
                pending.insert(call.id.clone(), call.function.name.clone());
                converted.tool_calls.push(ToolCall {
                    id: call.id,
                    name: call.function.name,
                    arguments: call.function.arguments,
                });
            }
            if role == Role::Tool {
                let id = message
                    .tool_call_id
                    .ok_or_else(|| invalid("Tool messages require 'tool_call_id'"))?;
                let name = pending.remove(&id).ok_or_else(|| {
                    invalid(format!(
                        "Unknown or already answered 'tool_call_id': '{id}'"
                    ))
                })?;
                if message.name.as_ref().is_some_and(|n| n != &name) {
                    return Err(invalid("Tool message 'name' does not match the tool call"));
                }
                converted.tool_call_id = Some(id);
                converted.tool_name = Some(name);
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
        if !pending.is_empty() {
            return Err(invalid("Missing tool results for assistant 'tool_calls'"));
        }
        Ok(canonical)
    }
}

fn content_text(content: Option<Content>, optional: bool) -> Result<String, GatewayError> {
    match content {
        Some(Content::Text(text)) => Ok(text),
        Some(Content::Parts(parts)) => {
            if parts.is_empty() {
                return Err(invalid("Message content parts must not be empty"));
            }
            let mut text = String::new();
            for part in parts {
                if part.get("type").and_then(Value::as_str) != Some("text") {
                    return Err(invalid("Only 'text' content parts are supported"));
                }
                text.push_str(
                    part.get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("Text content parts require a string 'text'"))?,
                );
            }
            Ok(text)
        }
        None if optional => Ok(String::new()),
        None => Err(invalid(
            "Message 'content' is required except on assistant tool-call turns",
        )),
    }
}

fn validate_message_fields(message: &ChatMessage, role: Role) -> Result<(), GatewayError> {
    if message.tool_calls.is_some() && role != Role::Assistant {
        return Err(invalid("'tool_calls' is only valid on assistant messages"));
    }
    if message.tool_call_id.is_some() && role != Role::Tool {
        return Err(invalid("'tool_call_id' is only valid on tool messages"));
    }
    if message.name.is_some() && role != Role::Tool {
        return Err(invalid(
            "Message 'name' is currently supported only on tool results",
        ));
    }
    for (key, value) in &message.extra {
        // Client-side reasoning extensions are deliberately not replayed upstream.
        if !value.is_null() && key != "reasoning_content" {
            return Err(invalid(format!("Unsupported message field '{key}'")));
        }
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), GatewayError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Err(invalid(
            "Function names must contain 1-64 ASCII letters, digits, '_' or '-'",
        ));
    }
    Ok(())
}

fn validate_options(options: &BTreeMap<String, Value>) -> Result<(), GatewayError> {
    for (key, value) in options {
        if value.is_null() {
            continue;
        }
        let supported_default = match key.as_str() {
            "n" => value == 1,
            "top_p" => value.as_f64() == Some(1.0),
            "frequency_penalty" | "presence_penalty" => value.as_f64() == Some(0.0),
            "logprobs" | "store" => value == false,
            "parallel_tool_calls" => value == true,
            "response_format" => value == &json!({"type":"text"}),
            "modalities" => value == &json!(["text"]),
            "user" | "safety_identifier" => value.is_string(),
            _ => false,
        };
        if !supported_default {
            return Err(invalid(format!(
                "Unsupported parameter '{key}': the Raycast upstream cannot guarantee this option"
            )));
        }
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> GatewayError {
    GatewayError::BadRequest(message.into())
}
