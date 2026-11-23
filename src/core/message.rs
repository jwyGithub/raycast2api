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

impl Role {
    /// Stable wire-ish name (used for cache-key hashing in compact).
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// One assistant tool call (canonical, flat form).
///
/// `arguments` is the raw JSON **string** the model emitted, kept verbatim so
/// adapters can forward it without a lossy re-encode.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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

/// An image attachment on a user message.
///
/// Canonical form holds the **raw bytes**: upstream (Raycast) references
/// uploaded files by id and never accepts inline base64, so the concrete
/// transport (upload → fileId) is the Raycast builder's job, not this type's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attachment {
    /// Inline image: media type + raw bytes (base64 already decoded).
    Inline { media_type: String, data: Vec<u8> },
    /// A remote image URL the client referenced. Resolved (fetched) by the
    /// handler layer before the request enters the pipeline; failures drop
    /// the attachment rather than failing the request.
    Remote { url: String },
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
    /// For `Role::Tool` results: whether the tool execution failed
    /// (Anthropic `tool_result.is_error`). The Raycast wire has no error
    /// flag, so the builder marks it textually (see `error_marker`).
    pub tool_error: bool,
    /// Image attachments. Only meaningful on `Role::User` messages (the real
    /// client attaches files to user turns only); empty everywhere else.
    pub attachments: Vec<Attachment>,
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
            tool_error: false,
            attachments: Vec::new(),
        }
    }

    /// A user message carrying image attachments.
    pub fn with_attachments(
        role: Role,
        text: impl Into<String>,
        attachments: Vec<Attachment>,
    ) -> Self {
        let mut message = Self::text(role, text);
        message.attachments = attachments;
        message
    }

    /// The textual prefix marking a failed tool result. The upstream wire
    /// carries no error flag on `role:"tool"` messages, so a failed result is
    /// marked in-band; the model reads this and knows the output is an error.
    pub fn error_marker(is_error: bool) -> &'static str {
        if is_error { "[tool error] " } else { "" }
    }
}

/// Placeholder the real client substitutes for dropped image attachments
/// (model without vision, replay limit exceeded, upload failure). Keeping the
/// exact string matters: it is what the official client sends upstream, so
/// reusing it keeps the gateway's requests indistinguishable.
pub const IMAGE_OMITTED_PLACEHOLDER: &str =
    "[image omitted: the selected model has no vision support]";
