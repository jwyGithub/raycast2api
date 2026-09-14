//! The Anthropic **Messages** protocol adapters (`POST /v1/messages`).
//!
//! * [`wire`] — the public request / `Message` / SSE JSON types.
//! * [`inbound`] — [`MessagesRequest`](wire::MessagesRequest) →
//!   [`CanonicalRequest`](crate::core::CanonicalRequest), flattening Anthropic's
//!   block-structured messages (thinking, tool_use, tool_result).
//! * [`outbound`] — canonical response / event stream → Anthropic `Message`
//!   JSON and block-structured SSE.
//! * [`error`] — the Anthropic `{ "type": "error", ... }` surface.
//!
//! Like the other protocol folders, this layer is independent of the upstream
//! (Raycast) so the external Anthropic contract stays stable as internals change.

pub mod error;
pub mod inbound;
pub mod outbound;
pub mod wire;

pub use inbound::MessagesInbound;
pub use outbound::MessagesOutbound;
