use axum::{Json, extract::State};
use serde::Serialize;
use serde_json::Value;

use crate::{
    apikey::wire::{Create, Id, Item, ListParams, Update, UpdateModels},
    core::page::Page,
    error::GatewayError,
    server::state::SharedState,
};

pub(super) async fn list(
    State(state): State<SharedState>,
    Json(params): Json<ListParams>,
) -> Result<Json<Page<Item>>, GatewayError> {
    state.api_keys.list(params).map(Json)
}

pub(super) async fn create(
    State(state): State<SharedState>,
    Json(params): Json<Create>,
) -> Result<Json<Item>, GatewayError> {
    state
        .api_keys
        .create(params, &state.settings.api_key)
        .map(Json)
}

pub(super) async fn update(
    State(state): State<SharedState>,
    Json(params): Json<Update>,
) -> Result<Json<Item>, GatewayError> {
    state.api_keys.update(params).map(Json)
}

#[derive(Serialize)]
pub(super) struct ModelOptions {
    unrestricted: bool,
    items: Vec<ModelOption>,
}

#[derive(Serialize)]
struct ModelOption {
    id: String,
    name: Option<String>,
    /// 仅 Pro Plus 可用;已不在目录中的下线模型无此信息,为 null。
    in_better_ai_subscription: Option<bool>,
    abilities: Option<Value>,
    aliases: Vec<crate::model::store::AliasEntry>,
    selected: bool,
    offline: bool,
}

pub(super) async fn model_options(
    State(state): State<SharedState>,
    Json(params): Json<Id>,
) -> Result<Json<ModelOptions>, GatewayError> {
    state.api_keys.models(&params.id)?;
    let models = state.models().await?;
    let selected = state.api_keys.models(&params.id)?;
    let unrestricted = selected.is_none();
    let mut unavailable = selected.clone().unwrap_or_default();
    let mut aliases = state.aliases.by_model();
    let mut items: Vec<_> = models
        .into_iter()
        .map(|mut model| {
            let selected = unrestricted || unavailable.remove(&model.id);
            ModelOption {
                aliases: aliases.remove(&model.id).unwrap_or_default(),
                in_better_ai_subscription: Some(super::models::plus_only(&model.extra)),
                id: model.id,
                name: model
                    .extra
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                abilities: model.extra.remove("abilities"),
                selected,
                offline: false,
            }
        })
        .collect();
    items.extend(unavailable.into_iter().map(|id| ModelOption {
        aliases: aliases.remove(&id).unwrap_or_default(),
        id,
        name: None,
        in_better_ai_subscription: None,
        abilities: None,
        selected: true,
        offline: true,
    }));
    items.sort_unstable_by(|a, b| (a.offline, &a.id).cmp(&(b.offline, &b.id)));
    Ok(Json(ModelOptions {
        unrestricted,
        items,
    }))
}

pub(super) async fn update_models(
    State(state): State<SharedState>,
    Json(params): Json<UpdateModels>,
) -> Result<Json<Item>, GatewayError> {
    state.api_keys.models(&params.id)?;
    let available = if params
        .model_ids
        .as_ref()
        .is_some_and(|models| !models.is_empty())
    {
        state
            .models()
            .await?
            .into_iter()
            .map(|model| model.id)
            .collect()
    } else {
        Default::default()
    };
    state
        .api_keys
        .update_models(&params.id, params.model_ids, &available)
        .map(Json)
}

pub(super) async fn reset_usage(
    State(state): State<SharedState>,
    Json(params): Json<Id>,
) -> Result<Json<Item>, GatewayError> {
    state.api_keys.reset_usage(&params.id).map(Json)
}

pub(super) async fn delete(
    State(state): State<SharedState>,
    Json(params): Json<Id>,
) -> Result<Json<Value>, GatewayError> {
    state.api_keys.delete(&params.id)?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}
