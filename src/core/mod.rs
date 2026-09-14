//! Components ③ & ⑥ — the **canonical (internal) model**.
//!
//! This is the protocol- *and* provider-agnostic heart of the gateway. Inbound
//! adapters normalize into [`CanonicalRequest`]; the Raycast layer builds from
//! it and parses upstream events back into [`CanonicalEvent`] /
//! [`CanonicalResponse`]; outbound adapters serialize those into the caller's
//! protocol.
//!
//! It depends on neither the client protocols nor Raycast, so both sides can
//! change independently as long as they meet in the middle here.

pub mod event;
pub mod message;
pub mod protocol;
pub mod request;
pub mod response;
pub mod tools;
pub mod usage;

pub use event::CanonicalEvent;
pub use message::{Message, Role, Thinking, ToolCall};
pub use protocol::Protocol;
pub use request::{CanonicalRequest, ReasoningEffort};
pub use response::{CanonicalResponse, FinishReason};
pub use tools::{ToolChoice, ToolDef};
pub use usage::Usage;
