//! Component ⑥ (aggregated form) — the **Canonical Response**.
//!
//! For non-streaming clients the pipeline consumes the whole Raycast SSE stream
//! and folds the [`CanonicalEvent`]s into this single value, which the outbound
//! adapter (⑤) then serializes once into the target protocol's JSON.
//!
//! [`CanonicalEvent`]: crate::core::event::CanonicalEvent

use super::message::ToolCall;
use super::usage::Usage;

/// Why a completion ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FinishReason {
    /// Normal end of turn.
    #[default]
    Stop,
    /// The model wants one or more tools executed.
    ToolCalls,
}

/// A fully-aggregated completion.
#[derive(Debug, Clone, Default)]
pub struct CanonicalResponse {
    /// Concatenated assistant text.
    pub text: String,
    /// Concatenated reasoning/thinking text (empty for non-think models).
    pub reasoning: String,
    /// The provider's thought signature, if one was returned.
    pub thought_signature: Option<String>,
    /// Tool calls the model requested, if any.
    pub tool_calls: Vec<ToolCall>,
    /// How the completion ended.
    pub finish_reason: FinishReason,
    /// Token accounting.
    pub usage: Usage,
}
