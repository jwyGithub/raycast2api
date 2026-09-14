//! Components ② & ⑤ — the **client-protocol adapters**.
//!
//! Each supported client protocol (OpenAI Chat Completions, Anthropic Messages,
//! and OpenAI Responses) provides two adapters that meet the rest of the
//! gateway at the [canonical model](crate::core):
//!
//! * an [`InboundAdapter`] (②) that normalizes the protocol's request into a
//!   [`CanonicalRequest`], and
//! * an [`OutboundAdapter`] (⑤) that serializes a [`CanonicalResponse`] (or a
//!   stream of [`CanonicalEvent`]s, or a [`GatewayError`]) back into the
//!   protocol's wire shape.
//!
//! Adapters are selected **per route at compile time** — a handler names the
//! concrete adapter types it uses, so there is no dynamic dispatch and the
//! canonical pipeline stays monomorphized.

use axum::response::Response;
use futures_util::Stream;

use crate::core::{CanonicalEvent, CanonicalRequest, CanonicalResponse};
use crate::error::GatewayError;

pub mod chat_completions;
pub mod messages;
pub mod responses;

/// ② Normalizes one client protocol's request into a [`CanonicalRequest`].
///
/// The associated [`Request`](InboundAdapter::Request) is the protocol's typed
/// request body (deserialized by the handler's `Json` extractor); `parse`
/// applies the protocol's mapping rules and any request-level validation.
pub trait InboundAdapter {
    /// The protocol's typed request body.
    type Request;

    /// Map a protocol request into the canonical request, or fail with a
    /// client-facing [`GatewayError`].
    fn parse(request: Self::Request) -> Result<CanonicalRequest, GatewayError>;
}

/// ⑤ Serializes canonical output back into one client protocol's wire shape.
///
/// The methods are associated functions (no `self`): an outbound adapter is a
/// zero-sized marker type chosen by the route. `model` is the *client-facing*
/// model id to echo back (not the resolved upstream model).
pub trait OutboundAdapter {
    /// Serialize a fully-aggregated response (the non-streaming path).
    fn render_complete(model: &str, response: &CanonicalResponse) -> Response;

    /// Serialize a stream of canonical events as this protocol's SSE response.
    fn stream<S>(model: &str, events: S) -> Response
    where
        S: Stream<Item = CanonicalEvent> + Send + 'static;

    /// Render a gateway error in this protocol's error shape.
    fn render_error(error: GatewayError) -> Response;
}
