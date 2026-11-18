//! Shared application state passed to every handler.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, RwLock};

use crate::account::store::AccountStore;
use crate::config::Settings;
use crate::error::GatewayError;
use crate::log::store::RequestLogStore;
use crate::model::store::ModelAliasStore;
use crate::raycast::client::RaycastClient;
use crate::raycast::wire::RaycastRawModel;

use super::compact::{SummaryCache, UsageCache};
use super::health::BackendHealth;

/// How long a fetched model list stays fresh before it is re-fetched.
const MODELS_CACHE_TTL: Duration = Duration::from_secs(300);

/// Reference-counted handle to the shared state (axum `State`).
pub type SharedState = Arc<AppState>;

pub struct AppState {
    pub settings: Arc<Settings>,
    pub redis: Option<redis::aio::ConnectionManager>,
    pub redis_key_prefix: String,
    pub client: RaycastClient,
    pub health: BackendHealth,
    pub inference_slots: std::sync::Arc<tokio::sync::Semaphore>,
    /// 账号池(账号 CRUD 接口与刷新触发使用)。
    pub accounts: Arc<AccountStore>,
    /// 请求日志存储(推理请求三体采集写入)。
    pub log: Arc<RequestLogStore>,
    /// 模型别名(控制台维护,请求路径读内存缓存)。
    pub aliases: ModelAliasStore,
    pub statistics: Arc<crate::statistics::store::StatisticsStore>,
    /// auto-compact 本地摘要缓存(Redis 不可用时保留)。
    pub summary_cache: SummaryCache,
    /// auto-compact 本地实测输入 token 缓存(Redis 不可用时保留)。
    pub usage_cache: UsageCache,
    models_cache: RwLock<Option<ModelsCache>>,
    models_refresh: Mutex<()>,
    models_failed_at: Mutex<Option<Instant>>,
}

struct ModelsCache {
    fetched_at: Instant,
    models: Vec<RaycastRawModel>,
}

impl AppState {
    pub fn new(
        settings: Arc<Settings>,
        client: RaycastClient,
        accounts: Arc<AccountStore>,
        log: Arc<RequestLogStore>,
    ) -> Self {
        // 与其他 store 共用同一个 SQLite 连接。
        let aliases = ModelAliasStore::new(log.db.clone());
        Self {
            statistics: client.statistics.clone(),
            settings,
            redis: None,
            redis_key_prefix: "raycast2api".into(),
            client,
            health: BackendHealth::default(),
            inference_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(64)),
            accounts,
            log,
            aliases,
            summary_cache: SummaryCache::default(),
            usage_cache: UsageCache::default(),
            models_cache: RwLock::new(None),
            models_refresh: Mutex::new(()),
            models_failed_at: Mutex::new(None),
        }
    }

    /// Return the model list, serving a cached copy while it is fresh and
    /// otherwise fetching from upstream, with stale-on-error and failure backoff.
    pub async fn models(&self) -> Result<Vec<RaycastRawModel>, GatewayError> {
        if let Some(cache) = self.models_cache.read().await.as_ref()
            && cache.fetched_at.elapsed() < MODELS_CACHE_TTL
        {
            return Ok(cache.models.clone());
        }

        let _refresh = self.models_refresh.lock().await;
        if let Some(cache) = self.models_cache.read().await.as_ref()
            && cache.fetched_at.elapsed() < MODELS_CACHE_TTL
        {
            return Ok(cache.models.clone());
        }
        if self
            .models_failed_at
            .lock()
            .await
            .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
        {
            if let Some(cache) = self.models_cache.read().await.as_ref() {
                return Ok(cache.models.clone());
            }
            return Err(GatewayError::Upstream {
                status: 503,
                message: "model registry refresh temporarily unavailable".into(),
            });
        }
        match self.client.fetch_models().await {
            Ok(models) => {
                *self.models_failed_at.lock().await = None;
                *self.models_cache.write().await = Some(ModelsCache {
                    fetched_at: Instant::now(),
                    models: models.clone(),
                });
                Ok(models)
            }
            Err(err) => {
                *self.models_failed_at.lock().await = Some(Instant::now());
                if let Some(cache) = self.models_cache.read().await.as_ref() {
                    tracing::warn!(%err, "using stale model cache");
                    return Ok(cache.models.clone());
                }
                Err(err)
            }
        }
    }

    /// Apply a console-managed model alias (exact match, one hop) to a
    /// client-facing model id. Every lookup into the upstream model list must
    /// go through this so aliases behave the same everywhere.
    pub fn apply_model_override(&self, requested_id: &str) -> String {
        self.aliases
            .resolve(requested_id)
            .unwrap_or_else(|| requested_id.to_owned())
    }

    /// Resolve an OpenAI-facing model id to `(provider, internal_model)`,
    /// applying the console-managed model aliases first, then looking up
    /// the upstream model list. Unknown models are rejected before inference.
    pub async fn resolve_model(
        &self,
        requested_id: &str,
    ) -> Result<(String, String), GatewayError> {
        let client_id = requested_id;
        let requested_id = self.apply_model_override(requested_id);
        let resolved = crate::raycast::models::resolve(&self.models().await?, &requested_id)
            .ok_or_else(|| GatewayError::NotFound(format!("Model '{client_id}' was not found")))?;
        Ok((resolved.provider, resolved.model))
    }
}

#[cfg(test)]
pub(crate) fn compact_test_state() -> SharedState {
    let db = crate::storage::db::Db::open(":memory:").unwrap();
    let accounts = Arc::new(AccountStore::new(db.clone()));
    let statistics = Arc::new(crate::statistics::store::StatisticsStore::new(db.clone()));
    let log = Arc::new(RequestLogStore::new(db));
    let settings = Arc::new(Settings {
        raycast_device_id: "device".into(),
        raycast_signature_secret: "secret".into(),
        api_key: None,
        dashboard_password: None,
        dashboard_session_secret: None,
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        bind_addr_dual: None,
        upstream_timeout: std::time::Duration::from_secs(20),
        accounts_db: ":memory:".into(),
        proxies: Vec::new(),
        proxies_enabled: false,
    });
    let client = RaycastClient::new(
        reqwest::Client::new(),
        settings.clone(),
        accounts.clone(),
        log.clone(),
        statistics,
    );
    let mut state = AppState::new(settings, client, accounts, log);
    state.models_cache = RwLock::new(Some(ModelsCache {
        fetched_at: Instant::now(),
        models: vec![
            serde_json::from_value(serde_json::json!({
                "id": "test-model", "model": "test", "provider": "openai", "context": 32
            }))
            .unwrap(),
        ],
    }));
    Arc::new(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn model_aliases_resolve_and_unknown_models_are_rejected() {
        let state = compact_test_state();
        state.aliases.create("alias", "test-model").unwrap();
        state.aliases.create("bad-alias", "missing").unwrap();
        assert_eq!(
            state.resolve_model("alias").await.unwrap(),
            ("openai".into(), "test".into())
        );
        for model in ["missing", "bad-alias"] {
            let error = state.resolve_model(model).await.unwrap_err();
            assert_eq!(
                error.status_and_message().0,
                axum::http::StatusCode::NOT_FOUND
            );
            assert!(error.to_string().contains(model));
        }
    }

    #[tokio::test]
    async fn failed_model_refresh_keeps_known_model_instead_of_default() {
        let mut state = compact_test_state();
        let inner = Arc::get_mut(&mut state).unwrap();
        inner.models_cache.get_mut().as_mut().unwrap().fetched_at =
            Instant::now() - MODELS_CACHE_TTL;
        let (provider, model) = state.resolve_model("test-model").await.unwrap();
        assert_eq!((provider.as_str(), model.as_str()), ("openai", "test"));
    }
}
