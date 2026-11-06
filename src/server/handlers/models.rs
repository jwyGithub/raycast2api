//! `GET /v1/models` — OpenAI-compatible model listing.

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use crate::error::GatewayError;
use crate::protocol::chat_completions::wire::{ModelEntry, ModelList};
use crate::server::state::SharedState;
use crate::util::unix_secs;

pub async fn list_models(State(state): State<SharedState>) -> Result<Response, GatewayError> {
    let models = state.models().await?;
    let created = unix_secs();

    // 保留 Raycast 扩展字段，并补充 Sub2API 可读取的通用模型元数据。
    let mut data: Vec<ModelEntry> = models
        .iter()
        .map(|m| {
            let mut extra = m.extra.clone();
            add_sub2api_metadata(&mut extra, &m.id);
            ModelEntry {
                id: m.id.clone(),
                object: "model",
                created,
                owned_by: m.provider.clone(),
                extra,
            }
        })
        .collect();
    data.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(Json(ModelList {
        object: "list",
        data,
    })
    .into_response())
}

fn add_sub2api_metadata(extra: &mut Map<String, Value>, model_id: &str) {
    extra.entry("type").or_insert(json!("model"));
    let display_name = extra
        .get("name")
        .cloned()
        .unwrap_or_else(|| json!(model_id));
    extra.entry("display_name").or_insert(display_name);

    if !extra.contains_key("reasoning")
        && let Some(supported) = extra
            .get("abilities")
            .and_then(|abilities| abilities.get("thinking"))
            .and_then(|ability| ability.get("supported"))
            .or_else(|| {
                extra
                    .get("abilities")
                    .and_then(|abilities| abilities.get("reasoning_effort"))
                    .and_then(|ability| ability.get("supported"))
            })
    {
        extra.insert("reasoning".into(), supported.clone());
    }

    if !extra.contains_key("supported_reasoning_levels") {
        if let Some(options) = extra
            .get("abilities")
            .and_then(|abilities| abilities.get("reasoning_effort"))
            .and_then(|ability| ability.get("options"))
        {
            extra.insert("supported_reasoning_levels".into(), options.clone());
        }
    }

    if !extra.contains_key("input_modalities") {
        let has_vision = extra
            .get("abilities")
            .and_then(|abilities| abilities.get("vision"))
            .and_then(|vision| vision.get("formats"))
            .and_then(Value::as_array)
            .is_some_and(|formats| !formats.is_empty());
        let mut modalities = vec![json!("text")];
        if has_vision {
            modalities.push(json!("image"));
        }
        extra.insert("input_modalities".into(), Value::Array(modalities));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_sub2api_metadata_without_dropping_raycast_fields() {
        let mut extra = serde_json::from_value(json!({
            "name": "Example Model",
            "context": 200,
            "abilities": {
                "thinking": {"supported": true},
                "reasoning_effort": {"supported": true, "options": ["low", "high"]},
                "vision": {"formats": ["image/png"]}
            }
        }))
        .unwrap();

        add_sub2api_metadata(&mut extra, "example-model");

        assert_eq!(extra["type"], "model");
        assert_eq!(extra["display_name"], "Example Model");
        assert_eq!(extra["reasoning"], true);
        assert_eq!(extra["supported_reasoning_levels"], json!(["low", "high"]));
        assert_eq!(extra["input_modalities"], json!(["text", "image"]));
        assert_eq!(extra["context"], 200);
        assert!(!extra.contains_key("context_window"));
    }

    #[test]
    fn does_not_invent_capabilities_when_upstream_omits_abilities() {
        let mut extra = Map::new();

        add_sub2api_metadata(&mut extra, "example-model");

        assert_eq!(extra["display_name"], "example-model");
        assert_eq!(extra["input_modalities"], json!(["text"]));
        assert!(!extra.contains_key("reasoning"));
        assert!(!extra.contains_key("supported_reasoning_levels"));
    }
}
