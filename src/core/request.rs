//! Component ③ — the **Canonical Request**.
//!
//! Every inbound adapter (②) normalizes its protocol's request into this one
//! type, so the Raycast builder (④) has a single shape to consume. It carries
//! the source [`Protocol`] and the `stream` flag, which together decide how the
//! response is serialized on the way out (⑤): *from whichever path it came in,
//! it goes back out in that path's protocol*.

use super::message::Message;
use super::protocol::Protocol;
use super::tools::{ToolChoice, ToolDef};

/// Requested reasoning depth for "think" models (maps to Raycast
/// `reasoning_effort`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    /// The Raycast wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        }
    }
}

/// The unified internal request. Protocol-agnostic; the only Raycast-specific
/// translation happens later, in the builder (④).
#[derive(Debug, Clone)]
pub struct CanonicalRequest {
    /// Which client protocol this arrived on (decides outbound serialization).
    pub protocol: Protocol,
    /// Whether the client asked for a streaming response.
    pub stream: bool,
    /// Client-facing model id; resolved to a Raycast `model` + `provider` later.
    pub model: String,
    /// System / developer instructions, if any.
    pub system: Option<String>,
    /// Conversation history in order.
    pub messages: Vec<Message>,
    /// Sampling temperature, if the client set one.
    pub temperature: Option<f64>,
    /// Output token cap, if the client set one.
    pub max_tokens: Option<u32>,
    /// Tool definitions available to the model.
    pub tools: Vec<ToolDef>,
    /// Tool-choice policy, if the client set one.
    pub tool_choice: Option<ToolChoice>,
    /// Reasoning depth when the client requested a "think" completion.
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl CanonicalRequest {
    /// Start a request for `protocol` with the given model, all optional fields
    /// empty. Adapters fill in the rest.
    pub fn new(protocol: Protocol, model: impl Into<String>) -> Self {
        Self {
            protocol,
            stream: false,
            model: model.into(),
            system: None,
            messages: Vec::new(),
            temperature: None,
            max_tokens: None,
            tools: Vec::new(),
            tool_choice: None,
            reasoning_effort: None,
        }
    }

    /// Whether this request wants a "think" (reasoning) completion.
    pub fn wants_thinking(&self) -> bool {
        self.reasoning_effort.is_some()
    }
}
