//! Component ⑥ (streaming form) — **Canonical Events**.
//!
//! The Raycast SSE parser (⑦) turns each upstream event into one of these
//! provider-agnostic deltas. Streaming outbound adapters (⑤) translate them
//! into their protocol's SSE event sequence; the non-streaming path folds them
//! into a [`CanonicalResponse`].
//!
//! [`CanonicalResponse`]: crate::core::response::CanonicalResponse

use super::message::ToolCall;
use super::response::FinishReason;
use super::usage::Usage;

/// One increment of a completion, normalized across providers.
#[derive(Debug, Clone)]
pub enum CanonicalEvent {
    /// A chunk of reasoning/thinking text (think models only).
    ReasoningDelta(String),
    /// The provider's thought signature, emitted once reasoning ends.
    ThoughtSignature(String),
    /// A chunk of assistant body text.
    TextDelta(String),
    /// A complete tool call (Raycast delivers these whole, not char-by-char).
    ToolCall(ToolCall),
    /// The terminating event carrying the finish reason and usage.
    Finish { reason: FinishReason, usage: Usage },
    /// An upstream failure delivered *inside* a 200 SSE stream (e.g. a
    /// usage-limit rejection). Raycast reports some errors this way instead of
    /// an HTTP error status, so the parser surfaces them as a first-class event
    /// that no adapter may swallow.
    Error { status: u16, message: String },
}
