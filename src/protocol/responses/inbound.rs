//! ② OpenAI Responses inbound: [`ResponsesRequest`] → [`CanonicalRequest`].
//!
//! The Responses request packs the conversation into a flat `input[]` union, so
//! this adapter *expands* it into ordered canonical messages:
//!
//! * `instructions` (and any `system` / `developer` message item) → the
//!   canonical system prompt (joined in order).
//! * a `user` / `assistant` message item → a canonical [`Role::User`] /
//!   [`Role::Assistant`] message, flattening its `input_text` / `output_text`
//!   parts into plain text.
//! * a `function_call` item → an assistant message carrying one [`ToolCall`]
//!   (its `arguments` kept verbatim as the raw JSON string).
//! * a `function_call_output` item → a [`Role::Tool`] message answering that
//!   call. `reasoning` items are dropped on the way in.
//! * `reasoning.effort` → [`ReasoningEffort`] (a present `reasoning` object with
//!   no recognized effort defaults to `Medium`, matching the plan's default).

use serde_json::json;

use crate::config::DEFAULT_MODEL_ID;
use crate::core::message::{Message, Role, ToolCall};
use crate::core::protocol::Protocol;
use crate::core::request::{CanonicalRequest, ReasoningEffort};
use crate::core::tools::{ToolChoice, ToolDef};
use crate::error::GatewayError;
use crate::protocol::InboundAdapter;

use super::wire::{
    InboundTool, InboundToolChoice, Input, InputItem, ReasoningConfig, ResponsesRequest,
};

/// Zero-sized marker selecting the OpenAI Responses inbound mapping.
pub struct ResponsesInbound;

impl InboundAdapter for ResponsesInbound {
    type Request = ResponsesRequest;

    fn parse(request: ResponsesRequest) -> Result<CanonicalRequest, GatewayError> {
        let model = request
            .model
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL_ID.to_string());

        // `instructions` seeds the system prompt; `system` / `developer` input
        // items append to it in order.
        let mut system_parts: Vec<String> = Vec::new();
        if let Some(instructions) = request.instructions.filter(|s| !s.is_empty()) {
            system_parts.push(instructions);
        }

        let messages = match request.input {
            None => Vec::new(),
            Some(Input::Text(text)) => vec![Message::text(Role::User, text)],
            Some(Input::Items(items)) => conversation(items, &mut system_parts),
        };

        // A request must carry at least one conversational turn.
        if messages.is_empty() {
            return Err(GatewayError::BadRequest(
                "Missing or invalid 'input' field".into(),
            ));
        }

        let mut canonical = CanonicalRequest::new(Protocol::Responses, model);
        canonical.stream = request.stream;
        canonical.temperature = request.temperature;
        canonical.max_tokens = request.max_output_tokens;
        canonical.system = (!system_parts.is_empty()).then(|| system_parts.join("\n"));
        canonical.messages = messages;
        canonical.tools = tools(request.tools);
        canonical.tool_choice = request.tool_choice.map(tool_choice);
        canonical.reasoning_effort = request.reasoning.map(reasoning_effort);

        Ok(canonical)
    }
}

/// Expand the `input[]` items into ordered canonical messages, folding
/// `system` / `developer` text into `system_parts`.
fn conversation(items: Vec<InputItem>, system_parts: &mut Vec<String>) -> Vec<Message> {
    let mut out = Vec::with_capacity(items.len());

    for item in items {
        match item.kind.as_deref() {
            Some("function_call") => out.push(Message {
                role: Role::Assistant,
                text: String::new(),
                thinking: None,
                tool_calls: vec![ToolCall {
                    id: item.call_id.unwrap_or_default(),
                    name: item.name.unwrap_or_default(),
                    arguments: item
                        .arguments
                        .filter(|a| !a.is_empty())
                        .unwrap_or_else(|| "{}".to_string()),
                }],
                tool_call_id: None,
                tool_name: None,
            }),
            Some("function_call_output") => out.push(Message {
                role: Role::Tool,
                text: item.output.map(|o| o.as_text()).unwrap_or_default(),
                thinking: None,
                tool_calls: Vec::new(),
                tool_call_id: item.call_id,
                tool_name: None,
            }),
            // Reasoning items are provider bookkeeping; they don't map to a turn.
            Some("reasoning") => {}
            // `message` or a bare `{ role, content }` item.
            _ => push_message(item, system_parts, &mut out),
        }
    }

    out
}

/// Route a message item by role: `system` / `developer` fold into the system
/// prompt; `user` / `assistant` become canonical messages; other roles drop.
fn push_message(item: InputItem, system_parts: &mut Vec<String>, out: &mut Vec<Message>) {
    let text = item.content.map(|c| c.as_text()).unwrap_or_default();
    match item.role.as_deref() {
        Some("system") | Some("developer") => {
            if !text.is_empty() {
                system_parts.push(text);
            }
        }
        Some("user") => out.push(Message::text(Role::User, text)),
        Some("assistant") => out.push(Message::text(Role::Assistant, text)),
        _ => {}
    }
}

/// Map Responses tool definitions onto the canonical model: `function` tools
/// carry a JSON Schema (locally executed); any other typed tool is a
/// backend-provided tool referenced by its type name.
fn tools(tools: Vec<InboundTool>) -> Vec<ToolDef> {
    tools
        .into_iter()
        .filter_map(|tool| match tool.kind.as_deref() {
            Some("function") | None => Some(ToolDef::Local {
                name: tool.name?,
                description: tool.description,
                parameters: if tool.parameters.is_null() {
                    json!({})
                } else {
                    tool.parameters
                },
            }),
            Some(other) => Some(ToolDef::Remote {
                name: other.to_string(),
            }),
        })
        .collect()
}

/// Map Responses' `tool_choice` taxonomy onto the canonical policy.
fn tool_choice(choice: InboundToolChoice) -> ToolChoice {
    match choice {
        InboundToolChoice::Mode(mode) => match mode.as_str() {
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => ToolChoice::Auto,
        },
        InboundToolChoice::Object { kind, name } => match (kind.as_str(), name) {
            ("function", Some(name)) => ToolChoice::Named(name),
            _ => ToolChoice::Auto,
        },
    }
}

/// Map `reasoning.effort` onto the canonical depth. A present `reasoning`
/// object without a recognized effort enables thinking at the middle effort.
fn reasoning_effort(reasoning: ReasoningConfig) -> ReasoningEffort {
    match reasoning.effort.as_deref() {
        Some("minimal") | Some("low") => ReasoningEffort::Low,
        Some("high") => ReasoningEffort::High,
        _ => ReasoningEffort::Medium,
    }
}
