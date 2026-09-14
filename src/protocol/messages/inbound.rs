//! ② Anthropic Messages inbound: [`MessagesRequest`] → [`CanonicalRequest`].
//!
//! Anthropic's message model is block-structured and packs several canonical
//! concepts into one message, so this adapter *expands* as it flattens:
//!
//! * `system` (string or text blocks) → the canonical system prompt.
//! * an **assistant** message's blocks fold into one canonical assistant
//!   message — `text` blocks concatenate, a `thinking` block becomes
//!   [`Thinking`] (carrying its `signature`), and each `tool_use` block becomes
//!   a [`ToolCall`] (its `input` kept as the raw arguments JSON string).
//! * a **user** message's `tool_result` blocks each split off into their own
//!   canonical [`Role::Tool`] message (in order), carrying the tool's `name`
//!   (resolved from the matching earlier `tool_use`, since a `tool_result` only
//!   carries the call id), while its `text` blocks form the user turn.
//! * a **system**-role conversation message becomes a canonical [`Role::User`]
//!   message. Claude Code injects context (agent instructions, environment
//!   notes, ...) as `system` turns *inside* `messages[]`; the real client keeps
//!   them by forwarding them as user turns, so dropping them would silently lose
//!   a large slice of the conversation.
//! * multiple `text` blocks within one message are joined with `"\n"`.
//! * `thinking.type` of `"enabled"` or `"adaptive"` → [`ReasoningEffort::Medium`]
//!   (Anthropic expresses depth as a token budget; the gateway maps "on" to the
//!   middle effort, matching the plan's default).

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::config::DEFAULT_MODEL_ID;
use crate::core::message::{Message, Role, Thinking, ToolCall};
use crate::core::protocol::Protocol;
use crate::core::request::{CanonicalRequest, ReasoningEffort};
use crate::core::tools::{ToolChoice, ToolDef};
use crate::error::GatewayError;
use crate::protocol::InboundAdapter;

use super::wire::{
    ContentBlock, InboundMessage, InboundTool, InboundToolChoice, MessageContent, MessagesRequest,
    SystemPrompt,
};

/// Zero-sized marker selecting the Anthropic Messages inbound mapping.
pub struct MessagesInbound;

impl InboundAdapter for MessagesInbound {
    type Request = MessagesRequest;

    fn parse(request: MessagesRequest) -> Result<CanonicalRequest, GatewayError> {
        if request.messages.is_empty() {
            return Err(GatewayError::BadRequest(
                "Missing or invalid 'messages' field".into(),
            ));
        }

        let model = request
            .model
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL_ID.to_string());

        let mut canonical = CanonicalRequest::new(Protocol::Messages, model);
        canonical.stream = request.stream;
        canonical.temperature = request.temperature;
        canonical.max_tokens = request.max_tokens;
        canonical.system = system_prompt(request.system);
        canonical.messages = conversation(request.messages);
        canonical.tools = tools(request.tools);
        canonical.tool_choice = request.tool_choice.map(tool_choice);
        canonical.reasoning_effort = request
            .thinking
            .filter(|t| t.is_enabled())
            .map(|_| ReasoningEffort::Medium);

        Ok(canonical)
    }
}

/// Flatten `system` (string or text blocks) into a single prompt; an empty
/// result is left unset so the Raycast builder applies its default.
fn system_prompt(system: Option<SystemPrompt>) -> Option<String> {
    let text = match system? {
        SystemPrompt::Text(text) => text,
        SystemPrompt::Blocks(blocks) => blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    };
    (!text.is_empty()).then_some(text)
}

/// Expand each inbound message into one or more canonical messages, preserving
/// order. `system`-role turns are kept as user messages (see module docs);
/// other unknown roles are dropped.
fn conversation(messages: Vec<InboundMessage>) -> Vec<Message> {
    let mut out = Vec::with_capacity(messages.len());
    // Maps each `tool_use` id to its tool name, filled in as assistant turns are
    // seen. A `tool_result` only carries the call id, but the Raycast `role:
    // "tool"` message needs the tool `name`; the matching `tool_use` always
    // precedes its result, so a single forward pass resolves every name.
    let mut tool_names: HashMap<String, String> = HashMap::new();

    for message in messages {
        match message.role.as_str() {
            "assistant" => push_assistant(message.content, &mut out, &mut tool_names),
            "user" => push_user(message.content, &mut out, &tool_names),
            "system" => push_system(message.content, &mut out),
            _ => {}
        }
    }

    out
}

/// Fold an assistant turn (string, or `text`/`thinking`/`tool_use` blocks) into
/// a single canonical assistant message. Each `tool_use` also records its
/// `id → name` in `tool_names` so a later `tool_result` can be labelled.
fn push_assistant(
    content: MessageContent,
    out: &mut Vec<Message>,
    tool_names: &mut HashMap<String, String>,
) {
    let blocks = match content {
        MessageContent::Text(text) => {
            out.push(Message::text(Role::Assistant, text));
            return;
        }
        MessageContent::Blocks(blocks) => blocks,
    };

    let mut text = String::new();
    let mut thinking: Option<Thinking> = None;
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text { text: chunk } => append_text(&mut text, &chunk),
            ContentBlock::Thinking {
                thinking: chunk,
                signature,
            } => match thinking.as_mut() {
                Some(existing) => {
                    existing.text.push_str(&chunk);
                    if existing.signature.is_none() {
                        existing.signature = signature;
                    }
                }
                None => {
                    thinking = Some(Thinking {
                        text: chunk,
                        signature,
                    })
                }
            },
            ContentBlock::ToolUse { id, name, input } => {
                tool_names.insert(id.clone(), name.clone());
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: arguments_string(input),
                });
            }
            // `tool_result` / unknown blocks don't belong on an assistant turn.
            ContentBlock::ToolResult { .. } | ContentBlock::Other => {}
        }
    }

    out.push(Message {
        role: Role::Assistant,
        text,
        thinking,
        tool_calls,
        tool_call_id: None,
        tool_name: None,
    });
}

/// Expand a user turn: `tool_result` blocks each become their own canonical
/// [`Role::Tool`] message (labelled with the tool name looked up from
/// `tool_names`); `text` blocks form the user message. Order between the two is
/// preserved.
fn push_user(
    content: MessageContent,
    out: &mut Vec<Message>,
    tool_names: &HashMap<String, String>,
) {
    let blocks = match content {
        MessageContent::Text(text) => {
            out.push(Message::text(Role::User, text));
            return;
        }
        MessageContent::Blocks(blocks) => blocks,
    };

    let mut text = String::new();

    for block in blocks {
        match block {
            ContentBlock::Text { text: chunk } => append_text(&mut text, &chunk),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
            } => {
                // Flush any user text accumulated before this result.
                if !text.is_empty() {
                    out.push(Message::text(Role::User, std::mem::take(&mut text)));
                }
                out.push(Message {
                    role: Role::Tool,
                    text: content.as_text(),
                    thinking: None,
                    tool_calls: Vec::new(),
                    tool_name: tool_names.get(&tool_use_id).cloned(),
                    tool_call_id: Some(tool_use_id),
                });
            }
            // `thinking` / `tool_use` / unknown blocks don't belong on a user turn.
            ContentBlock::Thinking { .. } | ContentBlock::ToolUse { .. } | ContentBlock::Other => {}
        }
    }

    if !text.is_empty() {
        out.push(Message::text(Role::User, text));
    }
}

/// Turn a `system`-role conversation message into a canonical [`Role::User`]
/// message. Claude Code injects context this way *inside* `messages[]`; the real
/// client forwards these as user turns rather than dropping them (matching the
/// top-level `system` prompt, they carry only text — no tool metadata). One
/// system message maps to exactly one user message, so the turn count is
/// preserved even when the body is empty.
fn push_system(content: MessageContent, out: &mut Vec<Message>) {
    let text = match content {
        MessageContent::Text(text) => text,
        MessageContent::Blocks(blocks) => {
            let mut text = String::new();
            for block in blocks {
                if let ContentBlock::Text { text: chunk } = block {
                    append_text(&mut text, &chunk);
                }
            }
            text
        }
    };
    out.push(Message::text(Role::User, text));
}

/// Append one `text` block to an accumulating buffer, inserting a `"\n"` between
/// blocks. A single block is emitted unchanged; consecutive blocks join with a
/// newline (matching the real client). A leading empty block contributes no
/// separator, so a lone non-empty block never gains a spurious leading newline.
fn append_text(buf: &mut String, chunk: &str) {
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(chunk);
}

/// Anthropic tools are always locally-executed, carrying a JSON Schema.
fn tools(tools: Vec<InboundTool>) -> Vec<ToolDef> {
    tools
        .into_iter()
        .map(|tool| ToolDef::Local {
            name: tool.name,
            description: tool.description,
            parameters: if tool.input_schema.is_null() {
                json!({})
            } else {
                tool.input_schema
            },
        })
        .collect()
}

/// Map Anthropic's `tool_choice` taxonomy onto the canonical policy.
fn tool_choice(choice: InboundToolChoice) -> ToolChoice {
    match choice {
        InboundToolChoice::Auto => ToolChoice::Auto,
        InboundToolChoice::Any => ToolChoice::Required,
        InboundToolChoice::None => ToolChoice::None,
        InboundToolChoice::Tool { name } => ToolChoice::Named(name),
    }
}

/// Render a `tool_use.input` value as the raw arguments JSON string the
/// canonical model carries (an absent/null input becomes `{}`).
fn arguments_string(input: Value) -> String {
    if input.is_null() {
        "{}".to_string()
    } else {
        input.to_string()
    }
}
