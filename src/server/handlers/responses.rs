//! `POST /v1/responses` — OpenAI Responses, streaming or not.
//!
//! Like [`messages`](super::messages), the handler is deliberately thin: it names
//! the Responses adapters — inbound (②) and outbound (⑤) — and hands off to the
//! shared [`pipeline`](crate::server::pipeline). All Responses-specific mapping
//! lives in the adapters; the pipeline and Raycast layers are unchanged.
//!
//! Before the run, the handler consults [`compact::maybe_compact`]: when the
//! conversation has outgrown the model's context window, the history is
//! rewritten as *recent user messages + summary* (Codex-style auto compact).
//! The summarization call itself goes through the same Raycast path, reusing
//! the pipeline's aggregation helper.

use axum::Json;
use axum::extract::State;
use axum::response::Response;
use futures_util::StreamExt;

use crate::core::event::CanonicalEvent;
use crate::error::GatewayError;
use crate::protocol::InboundAdapter;
use crate::protocol::responses::wire::ResponsesRequest;
use crate::protocol::responses::{ResponsesInbound, ResponsesOutbound};
use crate::raycast::builder::build_chat_request;
use crate::raycast::stream::canonical_events;
use crate::server::compact;
use crate::server::pipeline;
use crate::server::state::SharedState;

pub async fn responses(
    State(state): State<SharedState>,
    Json(request): Json<ResponsesRequest>,
) -> Result<Response, GatewayError> {
    let canonical = ResponsesInbound::parse(request)?;
    // Auto compact: when the conversation crosses ~90% of the model context
    // window, summarize the prefix and rewrite the history first. The closure
    // runs the summarization completion through the upstream and folds it to
    // plain text (no recursion — `maybe_compact` is called only from here).
    let canonical = compact::maybe_compact(&state, canonical, |summary_request| {
        let state = state.clone();
        Box::pin(async move { summarize(&state, summary_request).await })
    })
    .await?;
    pipeline::run::<ResponsesOutbound>(&state, canonical).await
}

/// Run one plain completion through the upstream and return its text — used
/// for the compact summarization call.
async fn summarize(
    state: &SharedState,
    request: crate::core::request::CanonicalRequest,
) -> Result<String, GatewayError> {
    let (provider, model) = state.resolve_model(&request.model).await;
    let raycast_request = build_chat_request(&request, &provider, &model);
    let response = state.client.send_chat(&raycast_request).await?;
    let events = canonical_events(response, state.log.clone());
    let mut events = std::pin::pin!(events);
    let mut text = String::new();
    while let Some(event) = events.next().await {
        match event {
            CanonicalEvent::TextDelta(delta) => text.push_str(&delta),
            CanonicalEvent::Finish { .. } => break,
            CanonicalEvent::Error { status, message } => {
                return Err(GatewayError::Upstream { status, message });
            }
            _ => {}
        }
    }
    Ok(text)
}
