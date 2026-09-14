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
    let show_advanced = state.settings.show_advanced;
    let include_deprecated = state.settings.include_deprecated;

    let mut data: Vec<ModelEntry> = models
        .iter()
        .filter(|m| show_advanced || !m.requires_better_ai)
        .filter(|m| include_deprecated || m.availability.as_deref() != Some("deprecated"))
        .map(|m| ModelEntry {
            id: m.id.clone(),
            object: "model",
            created,
            owned_by: m.provider.clone(),
        })
        .collect();
    data.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(Json(ModelList {
        object: "list",
        data,
    })
    .into_response())
}
