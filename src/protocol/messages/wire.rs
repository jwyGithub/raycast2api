//! The public, Anthropic-Messages-compatible JSON types (`POST /v1/messages`).
//!
//! These lock the external contract the gateway exposes to Anthropic clients
//! (e.g. Claude Code). The request types are deserialized by the handler and
//! mapped into the [canonical model](crate::core) by [`super::inbound`]; the
//! response / SSE types are produced by [`super::outbound`]. Keeping every wire
//! shape here (free of canonical- or Raycast-specific logic) lets the external
//! surface stay stable as internals change.
//!
//! Anthropic's content is *block-structured*: a message's `content` is either a
//! plain string or an array of typed blocks (`text`, `thinking`, `tool_use`,
//! `tool_result`). The inbound adapter flattens those blocks into canonical
//! messages; the outbound adapter re-materializes them on the way out.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// --- Request -------------------------------------------------------------

/// Incoming `POST /v1/messages` body. Unknown fields are ignored.
#[derive(Debug, Deserialize)]
pub struct MessagesRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub system: Option<SystemPrompt>,
    #[serde(default)]
    pub messages: Vec<InboundMessage>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub tools: Vec<InboundTool>,
    #[serde(default)]
    pub tool_choice: Option<InboundToolChoice>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
}

/// `system` may be a bare string or an array of text blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    Text(String),
    Blocks(Vec<TextBlock>),
}

/// A minimal `{ "type": "text", "text": ... }` block (the `type` tag is
/// ignored on the way in). Used for `system` blocks and `tool_result` content.
#[derive(Debug, Deserialize)]
pub struct TextBlock {
    #[serde(default)]
    pub text: String,
}

/// One inbound conversation message.
#[derive(Debug, Deserialize)]
pub struct InboundMessage {
    pub role: String,
    pub content: MessageContent,
}

/// A message's `content`: a plain string or an array of typed blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// A typed content block inside a message, discriminated by `type`. Block kinds
/// the gateway doesn't model (e.g. `image`) deserialize to [`ContentBlock::Other`]
/// and are dropped by the inbound adapter.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: ToolResultContent,
    },
    #[serde(other)]
    Other,
}

/// `tool_result.content`: a string or an array of text blocks. Defaults to an
/// empty string when absent.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<TextBlock>),
}

impl Default for ToolResultContent {
    fn default() -> Self {
        ToolResultContent::Text(String::new())
    }
}

impl ToolResultContent {
    /// Flatten the tool-result content into a single plain string.
    pub fn as_text(&self) -> String {
        match self {
            ToolResultContent::Text(text) => text.clone(),
            ToolResultContent::Blocks(blocks) => blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// An Anthropic tool definition.
#[derive(Debug, Deserialize)]
pub struct InboundTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Value,
}

/// Anthropic `tool_choice`: `{ "type": "auto" | "any" | "none" | "tool", ... }`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InboundToolChoice {
    Auto,
    Any,
    None,
    Tool { name: String },
}

/// Anthropic extended-thinking config: `{ "type": "enabled" | "adaptive" | "disabled", ... }`.
#[derive(Debug, Deserialize)]
pub struct ThinkingConfig {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub budget_tokens: Option<u32>,
}

impl ThinkingConfig {
    /// Whether extended thinking is turned on. Anthropic clients turn it on with
    /// either `"enabled"` (an explicit token budget) or `"adaptive"` (the client
    /// lets the model manage the budget, as Claude Code sends); both mean
    /// "think", so both count as on. Anything else (e.g. `"disabled"`) is off.
    pub fn is_enabled(&self) -> bool {
        matches!(self.kind.as_str(), "enabled" | "adaptive")
    }
}

// --- Non-streaming response ---------------------------------------------

/// A completed `Message` object (the non-streaming response).
#[derive(Debug, Serialize)]
pub struct MessageResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub role: &'static str,
    pub model: String,
    pub content: Vec<OutputBlock>,
    pub stop_reason: &'static str,
    pub stop_sequence: Option<String>,
    pub usage: OutputUsage,
}

/// An output content block in a completed `Message`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputBlock {
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

/// Anthropic keeps the canonical `input_tokens` / `output_tokens` names and
/// additionally reports cache accounting. Raycast only surfaces a single
/// `cached_tokens` count (the portion of the prompt served from cache), which
/// maps to `cache_read_input_tokens`; cache **creation** is never reported, so
/// `cache_creation_input_tokens` is always 0.
#[derive(Debug, Default, Serialize)]
pub struct OutputUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: u32,
    pub cache_read_input_tokens: u32,
}
