//! `POST /v1/messages` — Anthropic Messages, streaming or not.
//!
//! Like [`chat`](super::chat), the handler is deliberately thin: it names the
//! Anthropic adapters — inbound (②) and outbound (⑤) — and hands off to the
//! shared [`pipeline`](crate::server::pipeline). All Anthropic-specific mapping
//! lives in the adapters; the pipeline and Raycast layers are unchanged.

use axum::Json;
use axum::extract::State;
use axum::response::Response;

use crate::error::GatewayError;
use crate::protocol::InboundAdapter;
use crate::protocol::messages::wire::MessagesRequest;
use crate::protocol::messages::{MessagesInbound, MessagesOutbound};
use crate::server::pipeline;
use crate::server::state::SharedState;

pub async fn messages(
    State(state): State<SharedState>,
    Json(request): Json<MessagesRequest>,
) -> Result<Response, GatewayError> {
    let canonical = MessagesInbound::parse(request)?;
    pipeline::run::<MessagesOutbound>(&state, canonical).await
}
