//! Shared application state passed to every handler.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, RwLock};

use crate::account::dispatcher::AccountDispatcher;
use crate::account::store::AccountStore;
use crate::config::Settings;
use crate::error::GatewayError;
use crate::log::store::RequestLogStore;
use crate::model::store::ModelAliasStore;
use crate::raycast::client::RaycastClient;
use crate::raycast::wire::RaycastRawModel;
use crate::storage::redis_store::RedisKv;

use super::compact::CompactCache;
use super::health::BackendHealth;

/// How long a fetched model list stays fresh before it is re-fetched.
const MODELS_CACHE_TTL: Duration = Duration::from_secs(300);

/// Reference-counted handle to the shared state (axum `State`).
pub type SharedState = Arc<AppState>;

pub struct AppState {
    pub settings: Arc<Settings>,
    /// Redis 句柄(会话撤销使用);compact 缓存持有它的另一份克隆。
    pub redis: Option<RedisKv>,
    /// 账号调度 + 上游传输(`dispatcher.client` 是纯传输层)。
    pub dispatcher: AccountDispatcher,
    pub health: BackendHealth,
    pub inference_slots: std::sync::Arc<tokio::sync::Semaphore>,
    /// 账号池(账号 CRUD 接口与刷新触发使用)。
    pub accounts: Arc<AccountStore>,
    /// 请求日志存储(推理请求三体采集写入)。
    pub log: Arc<RequestLogStore>,
    /// 模型别名(控制台维护,请求路径读内存缓存)。
    pub aliases: ModelAliasStore,
    pub(crate) api_keys: Arc<crate::apikey::ApiKeyStore>,
    pub statistics: Arc<crate::statistics::store::StatisticsStore>,
    /// auto-compact 的摘要 / 实测 token 缓存(本地 L1 + 可选 Redis L2)。
    pub compact: CompactCache,
    /// 仅测试:直接操作底层 SQLite(建触发器、重载 store)。
    #[cfg(test)]
    pub(crate) db: crate::storage::db::Db,
    models_cache: RwLock<Option<ModelsCache>>,
    models_refresh: Mutex<()>,
    models_failed_at: Mutex<Option<Instant>>,
}

struct ModelsCache {
    fetched_at: Instant,
    models: Vec<RaycastRawModel>,
}

/// 构造 `AppState` 所需的依赖;各 store 由调用方(main)持有同一个 `Db` 创建。
pub struct AppDeps {
    pub settings: Arc<Settings>,
    pub client: RaycastClient,
    pub accounts: Arc<AccountStore>,
    pub log: Arc<RequestLogStore>,
    pub statistics: Arc<crate::statistics::store::StatisticsStore>,
    /// 与上面各 store 共用的 SQLite 连接,用于创建别名与 API Key store。
    pub db: crate::storage::db::Db,
}

impl AppState {
    pub fn new(deps: AppDeps) -> Result<Self, String> {
        let AppDeps {
            settings,
            client,
            accounts,
            log,
            statistics,
            db,
        } = deps;
        let aliases = ModelAliasStore::new(db.clone());
        let api_keys = Arc::new(crate::apikey::ApiKeyStore::new(
            db.clone(),
            &settings.api_key,
        )?);
        Ok(Self {
            statistics,
            settings,
            redis: None,
            dispatcher: AccountDispatcher::new(accounts.clone(), client),
            health: BackendHealth::default(),
            inference_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(64)),
            accounts,
            log,
            aliases,
            api_keys,
            compact: CompactCache::default(),
            #[cfg(test)]
            db,
            models_cache: RwLock::new(None),
            models_refresh: Mutex::new(()),
            models_failed_at: Mutex::new(None),
        })
    }

    pub fn with_redis(mut self, redis: Option<RedisKv>) -> Self {
        self.compact = std::mem::take(&mut self.compact).with_redis(redis.clone());
        self.redis = redis;
        self
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
        match self.dispatcher.fetch_models().await {
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

    /// Resolve an OpenAI-facing model id to its catalog entry,
    /// applying the console-managed model aliases first, then looking up
    /// the upstream model list. Unknown models are rejected before inference.
    pub async fn resolve_model(&self, requested_id: &str) -> Result<RaycastRawModel, GatewayError> {
        let client_id = requested_id;
        let requested_id = self.apply_model_override(requested_id);
        self.models()
            .await?
            .into_iter()
            .find(|model| model.id == requested_id)
            .ok_or_else(|| crate::apikey::model_not_found(client_id))
    }
}

#[cfg(test)]
pub(crate) fn compact_test_state() -> SharedState {
    let db = crate::storage::db::Db::open(":memory:").unwrap();
    let accounts = Arc::new(AccountStore::new(db.clone()));
    let statistics = Arc::new(crate::statistics::store::StatisticsStore::new(db.clone()));
    let log = Arc::new(RequestLogStore::new(db.clone()));
    let settings = Arc::new(Settings {
        raycast_device_id: "device".into(),
        raycast_signature_secret: "secret".into(),
        api_key: "test-key".into(),
        dashboard_password: None,
        dashboard_session_secret: None,
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        bind_addr_dual: None,
        upstream_timeout_secs: 20,
        accounts_db: ":memory:".into(),
        proxies: Vec::new(),
        proxies_mode: crate::proxies::ProxyMode::Direct,
    });
    let client = RaycastClient::new(reqwest::Client::new(), settings.clone(), statistics.clone());
    let mut state = AppState::new(AppDeps {
        settings,
        client,
        accounts,
        log,
        statistics,
        db,
    })
    .unwrap();
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
        let model = state.resolve_model("alias").await.unwrap();
        assert_eq!(
            (
                model.id.as_str(),
                model.provider.as_str(),
                model.model.as_str()
            ),
            ("test-model", "openai", "test")
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
        let model = state.resolve_model("test-model").await.unwrap();
        assert_eq!(
            (model.provider.as_str(), model.model.as_str()),
            ("openai", "test")
        );
    }
}
