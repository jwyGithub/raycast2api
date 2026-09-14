//! The OpenAI **Chat Completions** protocol adapters (`POST /v1/chat/completions`).
//!
//! * [`wire`] — the public request/response/chunk/model JSON types.
//! * [`inbound`] — [`ChatCompletionRequest`](wire::ChatCompletionRequest) →
//!   [`CanonicalRequest`](crate::core::CanonicalRequest).
//! * [`outbound`] — canonical response / event stream → OpenAI JSON + SSE.
//! * [`error`] — the OpenAI-style error surface.
//!
//! This layer is intentionally independent of the upstream (Raycast) so the
//! external OpenAI contract stays stable as internals evolve.

pub mod error;
pub mod inbound;
pub mod outbound;
pub mod wire;

pub use inbound::ChatCompletionsInbound;
pub use outbound::ChatCompletionsOutbound;
