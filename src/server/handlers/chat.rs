//! `POST /v1/chat/completions` — OpenAI-compatible chat, streaming or not.
//!
//! The handler is deliberately thin: it names the OpenAI adapters — inbound (②)
//! and outbound (⑤) — and hands off to the shared
//! [`pipeline`](crate::server::pipeline), which owns the protocol-agnostic flow
//! (resolve the model, build the upstream request, send it, and turn the
//! response into canonical events). All OpenAI-specific mapping lives in the
//! adapters; all provider-specific work lives in the Raycast layer.

use axum::Json;
use axum::extract::State;
use axum::response::Response;

use crate::error::GatewayError;
use crate::protocol::InboundAdapter;
use crate::protocol::chat_completions::wire::ChatCompletionRequest;
use crate::protocol::chat_completions::{ChatCompletionsInbound, ChatCompletionsOutbound};
use crate::server::pipeline;
use crate::server::state::SharedState;

pub async fn chat_completions(
    State(state): State<SharedState>,
    Json(request): Json<ChatCompletionRequest>,
) -> Result<Response, GatewayError> {
    let canonical = ChatCompletionsInbound::parse(request)?;
    pipeline::run::<ChatCompletionsOutbound>(&state, canonical).await
}
