//! Canonical conversation message, independent of any wire protocol.
//!
//! Inbound adapters (②) flatten each protocol's message shape into this; the
//! Raycast builder (④) renders it back out (wrapping content in `{text}`,
//! carrying `<think>...</think>` history, and nesting tool calls).

/// Author of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// System / developer instructions.
    System,
    /// End user.
    User,
    /// The assistant/model.
    Assistant,
    /// A tool result being fed back to the model.
    Tool,
}

/// One assistant tool call (canonical, flat form).
///
/// `arguments` is the raw JSON **string** the model emitted, kept verbatim so
/// adapters can forward it without a lossy re-encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A block of assistant "thinking", optionally carrying the provider's
/// `thought_signature` so it can be replayed on a later turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thinking {
    pub text: String,
    pub signature: Option<String>,
}

/// A single conversation message.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    /// Plain text content (may be empty for a tool-call-only assistant turn).
    pub text: String,
    /// Assistant thinking segment, when present.
    pub thinking: Option<Thinking>,
    /// Assistant tool calls, when this turn requested tools.
    pub tool_calls: Vec<ToolCall>,
    /// For `Role::Tool` results: the id of the call this answers.
    pub tool_call_id: Option<String>,
    /// For `Role::Tool` results: the tool's name.
    pub tool_name: Option<String>,
}

impl Message {
    /// A plain text message with no tool/thinking metadata.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
            thinking: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
        }
    }
}
