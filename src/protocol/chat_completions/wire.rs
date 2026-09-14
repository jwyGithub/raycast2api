//! The public, OpenAI-compatible request/response/chunk/model JSON types.
//!
//! These lock the external contract the gateway exposes. The mapping *into* the
//! canonical model lives in [`super::inbound`]; the mapping *out of* it lives in
//! [`super::outbound`]. Keeping the wire types here (free of any canonical- or
//! Raycast-specific logic) lets the external surface stay stable as internals
//! change.

use serde::{Deserialize, Serialize};

// --- Requests ------------------------------------------------------------

/// Incoming `POST /v1/chat/completions` body. Extra fields are ignored.
#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub stream: bool,
}

/// A chat message. `content` is a plain string (the common case); richer
/// multi-part content is a possible future extension.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

// --- Non-streaming response ---------------------------------------------

#[derive(Debug, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Usage,
}

#[derive(Debug, Serialize)]
pub struct Choice {
    pub index: u32,
    pub message: AssistantMessage,
    pub finish_reason: &'static str,
}

#[derive(Debug, Serialize)]
pub struct AssistantMessage {
    pub role: &'static str,
    /// The assistant text, or `null` on a tool-call turn (OpenAI carries a null
    /// `content` alongside `tool_calls`).
    pub content: Option<String>,
    /// Tool calls the model requested; omitted entirely on a plain-text turn so
    /// the original text response stays byte-identical.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

/// One assistant tool call in a non-streaming response.
#[derive(Debug, Serialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FunctionCall,
}

/// The `function` payload shared by [`ToolCall`] and [`ToolCallDelta`].
#[derive(Debug, Serialize)]
pub struct FunctionCall {
    pub name: String,
    /// The raw JSON arguments string the model emitted, forwarded verbatim.
    pub arguments: String,
}

impl ToolCall {
    /// A `function`-type tool call carrying the raw arguments JSON string.
    pub fn function(id: String, name: String, arguments: String) -> Self {
        Self {
            id,
            kind: "function",
            function: FunctionCall { name, arguments },
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// Attached only when the provider reported cached prompt tokens, so the
    /// common response keeps its original three-field usage object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

/// The `usage.prompt_tokens_details` sub-object (cached-token accounting).
#[derive(Debug, Serialize)]
pub struct PromptTokensDetails {
    pub cached_tokens: u32,
}

impl ChatCompletionResponse {
    /// Build a completed (non-streaming) response from its parts: an optional
    /// assistant `content` (`None` → null, on a tool-call turn), any
    /// `tool_calls`, the `finish_reason`, and the mapped `usage`.
    pub fn new(
        id: String,
        created: u64,
        model: String,
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
        finish_reason: &'static str,
        usage: Usage,
    ) -> Self {
        Self {
            id,
            object: "chat.completion",
            created,
            model,
            choices: vec![Choice {
                index: 0,
                message: AssistantMessage {
                    role: "assistant",
                    content,
                    tool_calls,
                },
                finish_reason,
            }],
            usage,
        }
    }

    /// Build a plain-text completed response: the full assistant text,
    /// `finish_reason: "stop"`, no tool calls, and zeroed usage. This is the
    /// original byte-compatible text path.
    pub fn from_text(id: String, created: u64, model: String, content: String) -> Self {
        Self::new(
            id,
            created,
            model,
            Some(content),
            Vec::new(),
            "stop",
            Usage::default(),
        )
    }
}

// --- Streaming response chunks ------------------------------------------

#[derive(Debug, Serialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
    /// Token usage, carried only on the final streaming chunk (which has an
    /// empty `choices` array). Omitted on every other chunk so the content /
    /// tool-call / finish chunks stay byte-identical to before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Serialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: Delta,
    pub finish_reason: Option<&'static str>,
}

#[derive(Debug, Default, Serialize)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Tool-call deltas; omitted (so the delta stays `{}`-clean) unless this
    /// chunk carries one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallDelta>,
}

/// One streaming tool-call delta. Raycast delivers whole tool calls, so a single
/// delta carries the id, name, and full arguments string at its `index`.
#[derive(Debug, Serialize)]
pub struct ToolCallDelta {
    pub index: u32,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FunctionCall,
}

impl ChatCompletionChunk {
    /// A content (and optionally role) delta chunk.
    pub fn delta(
        id: &str,
        created: u64,
        model: &str,
        role: Option<&'static str>,
        content: Option<String>,
    ) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created,
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    role,
                    content,
                    tool_calls: Vec::new(),
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    /// A streaming tool-call delta chunk (folding in the `assistant` role when
    /// it is the first chunk emitted).
    #[allow(clippy::too_many_arguments)]
    pub fn tool_call_delta(
        id: &str,
        created: u64,
        model: &str,
        role: Option<&'static str>,
        index: u32,
        call_id: String,
        name: String,
        arguments: String,
    ) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created,
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    role,
                    content: None,
                    tool_calls: vec![ToolCallDelta {
                        index,
                        id: call_id,
                        kind: "function",
                        function: FunctionCall { name, arguments },
                    }],
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    /// The terminating chunk carrying the given `finish_reason`.
    pub fn finish(id: &str, created: u64, model: &str, finish_reason: &'static str) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created,
            model: model.to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta::default(),
                finish_reason: Some(finish_reason),
            }],
            usage: None,
        }
    }

    /// The terminating chunk carrying `finish_reason: "stop"`.
    pub fn stop(id: &str, created: u64, model: &str) -> Self {
        Self::finish(id, created, model, "stop")
    }

    /// A final usage-only chunk: an empty `choices` array plus the token
    /// `usage`. Sent after the finishing chunk so streaming clients receive the
    /// same token accounting the non-streaming response carries (OpenAI's
    /// `stream_options: { include_usage: true }` convention).
    pub fn usage(id: &str, created: u64, model: &str, usage: Usage) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk",
            created,
            model: model.to_string(),
            choices: Vec::new(),
            usage: Some(usage),
        }
    }
}

// --- Model listing -------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ModelList {
    pub object: &'static str,
    pub data: Vec<ModelEntry>,
}

#[derive(Debug, Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: String,
}
