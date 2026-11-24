//! `GET /v1/models` — OpenAI-compatible model listing.

use axum::Json;
use axum::extract::{Extension, State};
use axum::response::{IntoResponse, Response};

use crate::error::GatewayError;
use crate::protocol::chat_completions::wire::{ModelEntry, ModelList};
use crate::raycast::wire::RaycastRawModel;
use crate::server::state::SharedState;
use crate::util::unix_secs;

/// 上游模型 → 对外条目(按 id 排序);`/v1/models` 与控制台模型面板共用。
pub(crate) fn model_entries(models: &[RaycastRawModel]) -> Vec<ModelEntry> {
    let created = unix_secs();
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
    data
}

pub(crate) async fn list_models(
    State(state): State<SharedState>,
    key: Option<Extension<crate::key::KeyId>>,
) -> Result<Response, GatewayError> {
    let mut models = state.models().await?;
    if let Some(Extension(key)) = key
        && let Some(allowed) = state.api_keys.permitted_models(&key)?
    {
        models.retain(|model| allowed.contains(&model.id));
    }

    Ok(Json(ModelList {
        object: "list",
        data: model_entries(&models),
    })
    .into_response())
}
