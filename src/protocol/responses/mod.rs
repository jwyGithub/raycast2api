//! The OpenAI **Responses** protocol adapters (`POST /v1/responses`).
//!
//! * [`wire`] — the public request / `response` / typed-SSE JSON types.
//! * [`inbound`] — [`ResponsesRequest`](wire::ResponsesRequest) →
//!   [`CanonicalRequest`](crate::core::CanonicalRequest), flattening the
//!   Responses `input[]` union (messages, function calls, function-call
//!   outputs, reasoning).
//! * [`outbound`] — canonical response / event stream → the Responses
//!   `response` object and typed SSE event sequence.
//! * [`error`] — the OpenAI-shaped `{ "error": { ... } }` surface.
//!
//! Like the other protocol folders, this layer is independent of the upstream
//! (Raycast) so the external Responses contract stays stable as internals change.

pub mod error;
pub mod inbound;
pub mod outbound;
pub mod wire;

pub use inbound::ResponsesInbound;
pub use outbound::ResponsesOutbound;
