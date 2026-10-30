//! `GET /v1/models` — OpenAI-compatible model listing.

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};

use crate::error::GatewayError;
use crate::protocol::chat_completions::wire::{ModelEntry, ModelList};
use crate::server::state::SharedState;
use crate::util::unix_secs;

pub async fn list_models(State(state): State<SharedState>) -> Result<Response, GatewayError> {
    let models = state.models().await?;
    let created = unix_secs();

    // 不做任何过滤:上游返回什么,网关就原样返回什么。
    let mut data: Vec<ModelEntry> = models
        .iter()
        .map(|m| ModelEntry {
            id: m.id.clone(),
            object: "model",
            created,
            owned_by: m.provider.clone(),
            extra: m.extra.clone(),
        })
        .collect();
    data.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(Json(ModelList {
        object: "list",
        data,
    })
    .into_response())
}
