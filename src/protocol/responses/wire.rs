//! The public, OpenAI-Responses-compatible JSON types (`POST /v1/responses`).
//!
//! These lock the external contract the gateway exposes to Responses clients
//! (e.g. Codex). The request types are deserialized by the handler and mapped
//! into the [canonical model](crate::core) by [`super::inbound`]; the response /
//! SSE types are produced by [`super::outbound`]. Keeping every wire shape here
//! (free of canonical- or Raycast-specific logic) lets the external surface stay
//! stable as internals change.
//!
//! The Responses `input` is a heterogeneous list: role messages (whose content
//! is a string or typed `input_text` / `output_text` parts), assistant
//! `function_call`s, `function_call_output`s (tool results), and `reasoning`
//! items. Message items may omit their `type`, so [`InputItem`] is a permissive
//! flat struct interpreted by the inbound adapter rather than a tagged enum.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// --- Request -------------------------------------------------------------

/// Incoming `POST /v1/responses` body. Unknown fields are ignored.
#[derive(Debug, Deserialize)]
pub struct ResponsesRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub input: Option<Input>,
    #[serde(default)]
    pub tools: Vec<InboundTool>,
    #[serde(default)]
    pub tool_choice: Option<InboundToolChoice>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
}

/// `input` is either a bare user prompt string or an ordered array of items.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Input {
    Text(String),
    Items(Vec<InputItem>),
}

/// One `input[]` item. Because the input is a heterogeneous union and message
/// items may omit `type`, this is a permissive flat struct read by the inbound
/// adapter (which branches on `kind` / `role`) rather than a tagged enum.
#[derive(Debug, Deserialize)]
pub struct InputItem {
    /// `message` / `function_call` / `function_call_output` / `reasoning`;
    /// absent for a bare `{ role, content }` message.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// For message items: `system` / `developer` / `user` / `assistant`.
    #[serde(default)]
    pub role: Option<String>,
    /// For message items: the message text (string or typed parts).
    #[serde(default)]
    pub content: Option<ItemContent>,
    /// For `function_call` / `function_call_output`: links the call to its result.
    #[serde(default)]
    pub call_id: Option<String>,
    /// For `function_call`: the invoked tool name.
    #[serde(default)]
    pub name: Option<String>,
    /// For `function_call`: the raw arguments JSON string.
    #[serde(default)]
    pub arguments: Option<String>,
    /// For `function_call_output`: the tool result (string or typed parts).
    #[serde(default)]
    pub output: Option<ItemContent>,
}

/// A message's `content` (or a function-call `output`): a plain string or an
/// array of typed parts. Only the `text` of each part is retained; non-text
/// parts (e.g. `input_image`) contribute nothing.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ItemContent {
    Text(String),
    Parts(Vec<ContentPartIn>),
}

impl ItemContent {
    /// Flatten the content into a single plain string.
    pub fn as_text(&self) -> String {
        match self {
            ItemContent::Text(text) => text.clone(),
            ItemContent::Parts(parts) => parts
                .iter()
                .map(|part| part.text.as_str())
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

/// A single `input_text` / `output_text` (or other) content part. Parts without
/// a `text` field default to empty and drop out on flattening.
#[derive(Debug, Deserialize)]
pub struct ContentPartIn {
    #[serde(default)]
    pub text: String,
}

/// A Responses tool definition. Function tools are flat (`type: "function"`
/// with `name` / `description` / `parameters` at the top level); built-in tools
/// (e.g. `{ "type": "web_search_preview" }`) carry only a type.
#[derive(Debug, Deserialize)]
pub struct InboundTool {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Value,
}

/// Responses `tool_choice`: a mode string (`"auto"` / `"none"` / `"required"`)
/// or an object selecting a specific tool (`{ "type": "function", "name": ... }`).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum InboundToolChoice {
    Mode(String),
    Object {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        name: Option<String>,
    },
}

/// Responses reasoning config: `{ "effort": "low" | "medium" | "high", ... }`.
#[derive(Debug, Deserialize)]
pub struct ReasoningConfig {
    #[serde(default)]
    pub effort: Option<String>,
}

// --- Non-streaming response ---------------------------------------------

/// A completed `response` object (the non-streaming response).
#[derive(Debug, Serialize)]
pub struct ResponseObject {
    pub id: String,
    pub object: &'static str,
    pub created_at: u64,
    pub status: &'static str,
    pub model: String,
    pub output: Vec<OutputItem>,
    pub usage: ResponseUsage,
}

/// One item in a completed response's `output[]`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    Reasoning {
        id: String,
        summary: Vec<SummaryPart>,
    },
    Message {
        id: String,
        status: &'static str,
        role: &'static str,
        content: Vec<OutputContent>,
    },
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        arguments: String,
        status: &'static str,
    },
}

/// A reasoning item's `summary[]` part.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SummaryPart {
    SummaryText { text: String },
}

/// A message item's `content[]` part.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputContent {
    OutputText {
        text: String,
        annotations: Vec<Value>,
    },
}

/// Responses reports `input_tokens` / `output_tokens` / `total_tokens`, plus
/// `input_tokens_details.cached_tokens` when the provider served part of the
/// prompt from cache. The details object is attached only when there are cached
/// tokens, so a cache-free response keeps its original three-field usage shape.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct ResponseUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<InputTokensDetails>,
}

/// The `usage.input_tokens_details` object, carrying the portion of the input
/// that hit the provider cache (`cached_tokens`).
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct InputTokensDetails {
    pub cached_tokens: u32,
}
