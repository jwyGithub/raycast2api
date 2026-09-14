//! The client-facing protocol a request arrived on.
//!
//! Protocol is decided **by the entry path** (component ①), never by sniffing
//! headers or body fields. It is carried on the [`CanonicalRequest`] so the
//! outbound stage (⑤) can serialize the response back in the same protocol the
//! request came in on.
//!
//! [`CanonicalRequest`]: crate::core::request::CanonicalRequest

/// Which client protocol an entry route speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Anthropic Messages API — `POST /v1/messages` (Claude Code).
    Messages,
    /// OpenAI Responses API — `POST /v1/responses` (Codex).
    Responses,
    /// OpenAI Chat Completions API — `POST /v1/chat/completions`.
    ChatCompletions,
}

impl Protocol {
    /// The entry path this protocol is served on.
    pub fn path(self) -> &'static str {
        match self {
            Protocol::Messages => "/v1/messages",
            Protocol::Responses => "/v1/responses",
            Protocol::ChatCompletions => "/v1/chat/completions",
        }
    }
}
