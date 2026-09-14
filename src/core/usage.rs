//! Canonical token-usage accounting, provider- and protocol-agnostic.
//!
//! Raycast reports `input_tokens` / `output_tokens` / `cached_tokens`; outbound
//! adapters (⑤) translate these into each protocol's field names (Anthropic
//! keeps the names; the OpenAI families rename to `prompt_tokens` /
//! `completion_tokens` with `prompt_tokens_details.cached_tokens`).

/// Token counts for one completion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Prompt/input tokens consumed.
    pub input_tokens: u32,
    /// Completion/output tokens produced.
    pub output_tokens: u32,
    /// Portion of the input that hit the provider cache.
    pub cached_tokens: u32,
}
