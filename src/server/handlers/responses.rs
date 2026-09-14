//! `POST /v1/responses` — OpenAI Responses, streaming or not.
//!
//! Like [`messages`](super::messages), the handler is deliberately thin: it names
//! the Responses adapters — inbound (②) and outbound (⑤) — and hands off to the
//! shared [`pipeline`](crate::server::pipeline). All Responses-specific mapping
//! lives in the adapters; the pipeline and Raycast layers are unchanged.

use axum::Json;
use axum::extract::State;
use axum::response::Response;

use crate::error::GatewayError;
use crate::protocol::InboundAdapter;
use crate::protocol::responses::wire::ResponsesRequest;
use crate::protocol::responses::{ResponsesInbound, ResponsesOutbound};
use crate::server::pipeline;
use crate::server::state::SharedState;

pub async fn responses(
    State(state): State<SharedState>,
    Json(request): Json<ResponsesRequest>,
) -> Result<Response, GatewayError> {
    let canonical = ResponsesInbound::parse(request)?;
    pipeline::run::<ResponsesOutbound>(&state, canonical).await
}
