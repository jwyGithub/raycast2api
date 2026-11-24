//! 控制台模型面板:`/v1/models/list`(筛选 + 分页 + 供应商枚举)与
//! `/v1/models/alias/{list,create,delete}`(模型别名)。

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::core::page::{Page, page_params};
use crate::error::GatewayError;
use crate::model::store::CreateError;
use crate::protocol::chat_completions::wire::ModelEntry;
use crate::server::handlers::inference::models::model_entries;
use crate::server::state::SharedState;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    pub page: Option<u32>,
    pub page_size: Option<u32>,
    pub provider_brand: Option<String>,
    /// true = 仅 Pro Plus 可用;false = Pro 与 Pro Plus 均可使用;缺省 = 不筛选。
    pub in_better_ai_subscription: Option<bool>,
}

/// 供应商下拉项:`label` = provider_name,`value` = provider_brand。
#[derive(Debug, Serialize)]
pub struct Provider {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct ModelsPage {
    #[serde(flatten)]
    page: Page<ModelEntry>,
    /// 始终基于全量模型,不随筛选条件收缩。
    providers: Vec<Provider>,
}

fn str_field<'a>(entry: &'a ModelEntry, key: &str) -> Option<&'a str> {
    entry.extra.get(key).and_then(serde_json::Value::as_str)
}

/// 模型是否仅 Pro Plus 可用(`in_better_ai_subscription`);字段缺失视为 Pro 与 Pro Plus 均可使用(false)。
/// 模型面板的筛选与 API Key 面板的模型标识共用这一处判断。
pub(super) fn plus_only(extra: &serde_json::Map<String, serde_json::Value>) -> bool {
    extra
        .get("in_better_ai_subscription")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// 条目是否满足筛选条件。
fn matches(entry: &ModelEntry, brand: Option<&str>, plus: Option<bool>) -> bool {
    brand.is_none_or(|b| str_field(entry, "provider_brand") == Some(b))
        && plus.is_none_or(|plus| plus_only(&entry.extra) == plus)
}

/// 按 provider_brand 去重。同一 brand 下有多个 provider_name(openai → OpenAI/Groq)
/// 用 " / " 拼接;不同 brand 的 label 重名(多个 "Vercel")时追加 "(brand)" 以便区分。
fn providers(entries: &[ModelEntry]) -> Vec<Provider> {
    let mut by_brand: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for entry in entries {
        let (Some(brand), Some(name)) = (
            str_field(entry, "provider_brand"),
            str_field(entry, "provider_name"),
        ) else {
            continue;
        };
        let names = by_brand.entry(brand).or_default();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let labelled: Vec<(&str, String)> = by_brand
        .into_iter()
        .map(|(brand, names)| (brand, names.join(" / ")))
        .collect();
    let mut out: Vec<Provider> = labelled
        .iter()
        .map(|(brand, label)| {
            let shared = labelled.iter().filter(|(_, other)| other == label).count() > 1;
            Provider {
                label: if shared {
                    format!("{label} ({brand})")
                } else {
                    label.clone()
                },
                value: (*brand).to_string(),
            }
        })
        .collect();
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

/// `POST /v1/models/list` — 分页查询模型(按 id 升序),可按 provider_brand 与订阅等级筛选。
pub async fn list(
    State(state): State<SharedState>,
    Json(params): Json<ListParams>,
) -> Result<Json<ModelsPage>, GatewayError> {
    let (page, page_size) = page_params(params.page, params.page_size)?;

    let entries = model_entries(&state.models().await?);
    let providers = providers(&entries);
    let brand = params.provider_brand.as_deref();
    let matched: Vec<ModelEntry> = entries
        .into_iter()
        .filter(|entry| matches(entry, brand, params.in_better_ai_subscription))
        .collect();
    let total = matched.len() as u64;
    let mut aliases = state.aliases.by_model();
    let items = matched
        .into_iter()
        .skip((page as usize - 1) * page_size as usize)
        .take(page_size as usize)
        .map(|mut entry| {
            let own = aliases.remove(&entry.id).unwrap_or_default();
            entry.extra.insert("aliases".into(), serde_json::json!(own));
            entry
        })
        .collect();

    Ok(Json(ModelsPage {
        page: Page::new(items, total, page, page_size),
        providers,
    }))
}

#[derive(Debug, Serialize)]
pub struct AliasRow {
    pub alias: String,
    pub model_id: String,
    /// 目标模型已不在模型表里时为 null。
    pub model_name: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
pub struct AliasList {
    items: Vec<AliasRow>,
}

/// `POST /v1/models/alias/list` — 全部别名映射(按别名升序),给控制台"映射预览"使用。
pub async fn list_aliases(
    State(state): State<SharedState>,
) -> Result<Json<AliasList>, GatewayError> {
    let entries = model_entries(&state.models().await?);
    let mut items: Vec<AliasRow> = state
        .aliases
        .by_model()
        .into_iter()
        .flat_map(|(model_id, aliases)| {
            let model_name = entries
                .iter()
                .find(|entry| entry.id == model_id)
                .and_then(|entry| str_field(entry, "name"))
                .map(str::to_owned);
            aliases.into_iter().map(move |a| AliasRow {
                alias: a.alias,
                model_id: model_id.clone(),
                model_name: model_name.clone(),
                created_at: a.created_at,
            })
        })
        .collect();
    items.sort_by(|a, b| a.alias.cmp(&b.alias));
    Ok(Json(AliasList { items }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAliasParams {
    pub alias: String,
    pub model_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteAliasParams {
    pub alias: String,
}

/// `POST /v1/models/alias/create` — 新增别名，或更新同一目标的旧模型 ID。
pub async fn create_alias(
    State(state): State<SharedState>,
    Json(params): Json<CreateAliasParams>,
) -> Result<StatusCode, GatewayError> {
    let alias = params.alias.trim();
    if alias.is_empty() || alias.len() > 200 {
        return Err(GatewayError::BadRequest(
            "alias must be 1..=200 characters".into(),
        ));
    }
    let models = state.models().await?;
    let Some(target) = models.iter().find(|m| m.id == params.model_id) else {
        return Err(GatewayError::NotFound(format!(
            "Model '{}' was not found",
            params.model_id
        )));
    };
    // 允许用目录中的模型 ID 作为映射来源，推理时映射优先。
    if alias == params.model_id {
        return Err(GatewayError::BadRequest(
            "alias and target model id must differ".into(),
        ));
    }
    let previous_id = target
        .extra
        .get("owned_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !models.iter().any(|model| model.id == *id));
    state
        .aliases
        .create_with_previous_id(alias, &target.id, previous_id)
        .map_err(|error| match error {
            // 控制台原样展示这条消息,所以用中文,并区分"已经加过"和"名字被别的模型占用"。
            CreateError::Exists(target) if target == params.model_id => {
                GatewayError::BadRequest(format!("映射「{alias}」已存在,无需重复添加"))
            }
            CreateError::Exists(target) => GatewayError::BadRequest(format!(
                "映射「{alias}」已指向「{target}」,请先删除原映射,再重新添加"
            )),
            CreateError::Db(error) => GatewayError::Internal(error),
        })?;
    Ok(StatusCode::OK)
}

/// `POST /v1/models/alias/delete` — 删除别名;不存在 404。
pub async fn delete_alias(
    State(state): State<SharedState>,
    Json(params): Json<DeleteAliasParams>,
) -> Result<StatusCode, GatewayError> {
    if state
        .aliases
        .delete(&params.alias)
        .map_err(GatewayError::Internal)?
    {
        Ok(StatusCode::OK)
    } else {
        Err(GatewayError::NotFound(format!(
            "no alias '{}'",
            params.alias
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(brand: &str, name: &str) -> ModelEntry {
        let extra = serde_json::json!({ "provider_brand": brand, "provider_name": name })
            .as_object()
            .unwrap()
            .clone();
        ModelEntry {
            id: format!("{brand}-{name}"),
            object: "model",
            created: 0,
            owned_by: String::new(),
            extra,
        }
    }

    #[test]
    fn providers_dedupe_by_brand_and_disambiguate_shared_labels() {
        let entries = vec![
            entry("openai", "OpenAI"),
            entry("openai", "Groq"),
            entry("meta", "Vercel"),
            entry("zai", "Vercel"),
            entry("meta", "Vercel"),
        ];
        let got: Vec<_> = providers(&entries)
            .into_iter()
            .map(|p| (p.value, p.label))
            .collect();
        assert_eq!(
            got,
            [
                ("openai".to_string(), "OpenAI / Groq".to_string()),
                ("meta".to_string(), "Vercel (meta)".to_string()),
                ("zai".to_string(), "Vercel (zai)".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn alias_endpoints_enforce_single_mapping_and_validate_models() {
        // compact_test_state 的模型表里只有 "test-model"。
        let state = crate::server::state::compact_test_state();
        let create = |alias: &str, model_id: &str| {
            create_alias(
                State(state.clone()),
                Json(CreateAliasParams {
                    alias: alias.into(),
                    model_id: model_id.into(),
                }),
            )
        };
        let status = |r: Result<StatusCode, GatewayError>| match r {
            Ok(code) => code,
            Err(e) => e.status_and_message().0,
        };

        assert_eq!(
            status(create("claude-sonnet-5-5", "test-model").await),
            StatusCode::OK
        );
        // 同一别名重复添加(无论目标是否相同)→ 400,且原映射不变。
        assert_eq!(
            status(create("claude-sonnet-5-5", "test-model").await),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            state.aliases.resolve("claude-sonnet-5-5").as_deref(),
            Some("test-model")
        );
        // 重复添加同一映射 → 提示"已存在";别名已指向别的模型 → 提示先删除。
        let message = |r: Result<StatusCode, GatewayError>| r.unwrap_err().status_and_message().1;
        assert!(message(create("claude-sonnet-5-5", "test-model").await).contains("无需重复添加"));
        state.aliases.create("moved", "elsewhere").unwrap();
        assert!(message(create("moved", "test-model").await).contains("请先删除原映射"));
        state.aliases.delete("moved").unwrap();
        // 目标模型不存在 → 404，自映射或空别名 → 400。
        assert_eq!(
            status(create("other", "missing").await),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(create("test-model", "test-model").await),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status(create("  ", "test-model").await),
            StatusCode::BAD_REQUEST
        );

        let delete = |alias: &str| {
            delete_alias(
                State(state.clone()),
                Json(DeleteAliasParams {
                    alias: alias.into(),
                }),
            )
        };
        assert_eq!(status(delete("claude-sonnet-5-5").await), StatusCode::OK);
        assert_eq!(
            status(delete("claude-sonnet-5-5").await),
            StatusCode::NOT_FOUND
        );
        // 删除后别名可以重新指向别的模型。
        assert_eq!(
            status(create("claude-sonnet-5-5", "test-model").await),
            StatusCode::OK
        );
    }

    #[test]
    fn matches_combines_brand_and_tier_filters() {
        let mut plus = entry("openai", "OpenAI");
        plus.extra
            .insert("in_better_ai_subscription".into(), true.into());
        let mut pro = entry("openai", "OpenAI");
        pro.extra
            .insert("in_better_ai_subscription".into(), false.into());
        let missing = entry("meta", "Vercel"); // 字段缺失 → 按 Pro 处理

        assert!(matches(&plus, None, None));
        assert!(matches(&plus, None, Some(true)) && !matches(&plus, None, Some(false)));
        assert!(matches(&pro, None, Some(false)) && !matches(&pro, None, Some(true)));
        assert!(matches(&missing, None, Some(false)) && !matches(&missing, None, Some(true)));
        assert!(matches(&plus, Some("openai"), Some(true)));
        assert!(!matches(&plus, Some("meta"), Some(true)));
    }
}
