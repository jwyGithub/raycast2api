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
//! * an `additional_tools` item (codex's multi-agent mode) → its namespaced
//!   tool groups are flattened into the canonical tool list, alongside the
//!   top-level `tools` array. Freeform (`type: "custom"`) tools become
//!   [`ToolDef::Custom`].
//! * a `custom_tool_call` item → an assistant message whose [`ToolCall`]
//!   arguments are the raw `input` JSON-encoded; a
//!   `custom_tool_call_output` item → a [`Role::Tool`] message answering it.
//! * `reasoning.effort` → [`ReasoningEffort`] (a present `reasoning` object with
//!   no recognized effort defaults to `Medium`, matching the plan's default).

use serde_json::{Value, json};
use std::collections::HashMap;

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
        // `instructions` seeds the system prompt; `system` / `developer` input
        // items append to it in order.
        let mut system_parts: Vec<String> = Vec::new();
        if let Some(instructions) = request.instructions.filter(|s| !s.is_empty()) {
            system_parts.push(instructions);
        }
        // Tools from `additional_tools` input items (codex's multi-agent mode),
        // merged with the top-level `tools` array below.
        let mut extra_tools: Vec<ToolDef> = Vec::new();

        let messages = match request.input {
            None => Vec::new(),
            Some(Input::Text(text)) => vec![Message::text(Role::User, text)],
            Some(Input::Items(items)) => conversation(items, &mut system_parts, &mut extra_tools),
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
        flatten_tools(request.tools, &mut canonical.tools);
        canonical.tools.extend(extra_tools);
        canonical.tool_choice = request.tool_choice.map(tool_choice);
        canonical.reasoning_effort = request.reasoning.map(reasoning_effort);

        Ok(canonical)
    }
}

/// Expand the `input[]` items into ordered canonical messages, folding
/// `system` / `developer` text into `system_parts` and `additional_tools`
/// items' tools into `extra_tools`.
fn conversation(
    items: Vec<InputItem>,
    system_parts: &mut Vec<String>,
    extra_tools: &mut Vec<ToolDef>,
) -> Vec<Message> {
    let mut out = Vec::with_capacity(items.len());
    // Maps each `function_call` id to its tool name, so the matching
    // `function_call_output` can carry it: the upstream `role: "tool"` message
    // needs the tool name (mirroring the real Raycast client, which always
    // sends it). The call always precedes its output, so one forward pass
    // resolves every name.
    let mut tool_names: HashMap<String, String> = HashMap::new();

    for item in items {
        match item.kind.as_deref() {
            // Codex (multi-agent mode) ships its tool set as a namespaced
            // `additional_tools` input item instead of the top-level array.
            Some("additional_tools") => flatten_tools(item.tools, extra_tools),
            Some("function_call") => {
                let name = item.name.clone().unwrap_or_default();
                if let Some(id) = &item.call_id {
                    tool_names.insert(id.clone(), name.clone());
                }
                out.push(Message {
                    role: Role::Assistant,
                    text: String::new(),
                    thinking: None,
                    tool_calls: vec![ToolCall {
                        id: item.call_id.unwrap_or_default(),
                        name,
                        arguments: item
                            .arguments
                            .filter(|a| !a.is_empty())
                            .unwrap_or_else(|| "{}".to_string()),
                    }],
                    tool_call_id: None,
                    tool_name: None,
                })
            }
            Some("function_call_output") => {
                let tool_name = item
                    .call_id
                    .as_deref()
                    .and_then(|id| tool_names.get(id).cloned());
                out.push(Message {
                    role: Role::Tool,
                    text: item.output.map(|o| o.as_text()).unwrap_or_default(),
                    thinking: None,
                    tool_calls: Vec::new(),
                    tool_call_id: item.call_id,
                    tool_name,
                })
            }
            // A replayed freeform call: the raw text becomes the JSON-string
            // arguments the upstream wire format carries.
            Some("custom_tool_call") => {
                let name = item.name.clone().unwrap_or_default();
                if let Some(id) = &item.call_id {
                    tool_names.insert(id.clone(), name.clone());
                }
                let input = item.input.unwrap_or_default();
                out.push(Message {
                    role: Role::Assistant,
                    text: String::new(),
                    thinking: None,
                    tool_calls: vec![ToolCall {
                        id: item.call_id.unwrap_or_default(),
                        name,
                        arguments: serde_json::to_string(&input).unwrap_or_default(),
                    }],
                    tool_call_id: None,
                    tool_name: None,
                })
            }
            Some("custom_tool_call_output") => {
                let tool_name = item
                    .call_id
                    .as_deref()
                    .and_then(|id| tool_names.get(id).cloned());
                out.push(Message {
                    role: Role::Tool,
                    text: item.output.map(|o| o.as_text()).unwrap_or_default(),
                    thinking: None,
                    tool_calls: Vec::new(),
                    tool_call_id: item.call_id,
                    tool_name,
                })
            }
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

/// Flatten tool definitions into the canonical model, recursing through the
/// namespace groups codex nests under `additional_tools`. `function` tools
/// (and untyped ones) carry a JSON Schema (locally executed); `custom` tools
/// carry a free-text input grammar, bridged as a single `input` string
/// parameter with the grammar folded into the description; any other typed
/// tool is a backend-provided tool referenced by its type name.
fn flatten_tools(tools: Vec<InboundTool>, out: &mut Vec<ToolDef>) {
    for tool in tools {
        // Namespace group: recurse into the members, the group itself is
        // only a label.
        if !tool.tools.is_empty() {
            flatten_tools(tool.tools, out);
            continue;
        }
        // An empty namespace group contributes nothing.
        if tool.kind.as_deref() == Some("namespace") {
            continue;
        }
        match tool.kind.as_deref() {
            // A freeform tool: the grammar rides in the description and the
            // raw text arrives as the single `input` argument.
            Some("custom") => {
                let Some(name) = tool.name else { continue };
                out.push(ToolDef::Custom {
                    name,
                    description: tool.description,
                    grammar: tool
                        .format
                        .as_ref()
                        .and_then(|f| f.get("definition"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
            }
            Some("function") | None => {
                let Some(name) = tool.name else { continue };
                out.push(ToolDef::Local {
                    name,
                    description: tool.description,
                    parameters: if tool.parameters.is_null() {
                        json!({})
                    } else {
                        tool.parameters
                    },
                })
            }
            // A built-in tool is referenced by its type name.
            Some(other) => out.push(ToolDef::Remote {
                name: other.to_string(),
            }),
        }
    }
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
