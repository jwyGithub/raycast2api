//! The protocol-agnostic request pipeline — component ① orchestration.
//!
//! Every entry handler funnels through [`run`], which owns the flow that is the
//! same for every client protocol: resolve the client-facing model to a Raycast
//! `provider`/`model`, build the upstream request (④), send it, and turn the
//! response into canonical events (⑦). From there the route's chosen
//! [`OutboundAdapter`] (⑤) serializes the result in the caller's protocol —
//! streaming the events directly, or folding them into a [`CanonicalResponse`]
//! for the non-streaming path.
//!
//! `run` is generic over the outbound adapter and names no protocol itself, so
//! adding a protocol is just a new handler that selects its adapters; this glue
//! stays untouched.

use axum::response::Response;
use futures_util::{Stream, StreamExt};

use crate::core::event::CanonicalEvent;
use crate::core::request::CanonicalRequest;
use crate::core::response::CanonicalResponse;
use crate::error::GatewayError;
use crate::protocol::OutboundAdapter;
use crate::raycast::builder::build_chat_request;
use crate::raycast::stream::canonical_events;

use super::state::SharedState;

/// Drive one canonical request all the way to a response rendered in the
/// protocol selected by outbound adapter `O`.
pub async fn run<O: OutboundAdapter>(
    state: &SharedState,
    request: CanonicalRequest,
) -> Result<Response, GatewayError> {
    let (provider, model) = state.resolve_model(&request.model).await;
    tracing::info!(
        client_model = %request.model,
        upstream = %format!("{provider}/{model}"),
        stream = request.stream,
        messages = request.messages.len(),
        "inference request"
    );
    let raycast_request = build_chat_request(&request, &provider, &model);

    let response = state.client.send_chat(&raycast_request).await?;
    state.health.mark_working();

    // The client-facing model id is echoed back verbatim — never the resolved
    // upstream model.
    let client_model = request.model;
    let events = canonical_events(response, state.log.clone());

    if request.stream {
        Ok(O::stream_with_tools(&client_model, &request.tools, events))
    } else {
        match aggregate(events).await {
            Ok(aggregated) => Ok(O::render_complete_with_tools(
                &client_model,
                &request.tools,
                &aggregated,
            )),
            // The upstream failed inside its 200 stream — render the error in
            // the client protocol's own envelope with the real status.
            Err(error) => Ok(O::render_error(error)),
        }
    }
}

/// Consume the whole canonical event stream, folding it into a single
/// [`CanonicalResponse`] for the non-streaming path. An in-stream upstream
/// error aborts the fold with a [`GatewayError::Upstream`].
async fn aggregate(
    events: impl Stream<Item = CanonicalEvent>,
) -> Result<CanonicalResponse, GatewayError> {
    let mut events = std::pin::pin!(events);
    let mut response = CanonicalResponse::default();

    while let Some(event) = events.next().await {
        match event {
            CanonicalEvent::TextDelta(text) => response.text.push_str(&text),
            CanonicalEvent::ReasoningDelta(text) => response.reasoning.push_str(&text),
            CanonicalEvent::ThoughtSignature(signature) => {
                response.thought_signature = Some(signature)
            }
            CanonicalEvent::ToolCall(call) => response.tool_calls.push(call),
            CanonicalEvent::Finish { reason, usage } => {
                response.finish_reason = reason;
                response.usage = usage;
            }
            CanonicalEvent::Error { status, message } => {
                return Err(GatewayError::Upstream { status, message });
            }
        }
    }

    Ok(response)
}
