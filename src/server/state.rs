//! Shared application state passed to every handler.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// In-process (L1) freshness of the model list; bounds Redis round-trips.
const MODELS_CACHE_TTL: Duration = Duration::from_secs(300);
/// Shared Redis (L2) lifetime of the model list. Delete the key to force a refresh.
const MODELS_REDIS_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Reference-counted handle to the shared state (axum `State`).
pub type SharedState = Arc<AppState>;

pub struct AppState {
    pub settings: Arc<Settings>,
    /// 出口地区解析器:请求体的 locale / `<user-preferences>` 来源。
    pub regions: Arc<crate::geo::EgressResolver>,
    /// Redis 句柄(会话撤销使用);compact 缓存持有它的另一份克隆。
    pub redis: Option<RedisKv>,
    /// 账号调度 + 上游传输(`dispatcher.client` 是纯传输层)。
    pub dispatcher: AccountDispatcher,
    pub health: BackendHealth,
    pub inference_slots: std::sync::Arc<tokio::sync::Semaphore>,
    /// 账号池(账号 CRUD 接口与刷新触发使用)。
    pub accounts: Arc<AccountStore>,
    pub proxies: Arc<crate::proxies::store::ProxyStore>,
    /// 请求日志存储(推理请求三体采集写入)。
    pub log: Arc<RequestLogStore>,
    /// 模型别名(控制台维护,请求路径读内存缓存)。
    pub aliases: ModelAliasStore,
    pub(crate) api_keys: Arc<crate::key::ApiKeyStore>,
    pub statistics: Arc<crate::statistics::store::StatisticsStore>,
    /// auto-compact 的摘要 / 实测 token 缓存(本地 L1 + 可选 Redis L2)。
    pub compact: CompactCache,
    /// 仅测试:直接操作底层 SQLite(建触发器、重载 store)。
    #[cfg(test)]
    pub(crate) db: crate::storage::db::Db,
    models_cache: RwLock<Option<ModelsCache>>,
    models_refresh: Mutex<()>,
    models_failed_at: Mutex<Option<Instant>>,
    /// The last Redis write of the model list failed, so the Redis copy may be
    /// older than L1 and must not overwrite it.
    models_redis_stale: AtomicBool,
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
    /// 出口地区解析器(请求体 locale/user-preferences 的来源);main 构造。
    pub regions: Arc<crate::geo::EgressResolver>,
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
            regions,
        } = deps;
        let proxies = Arc::new(crate::proxies::store::ProxyStore::new(db.clone())?);
        let mut client = client;
        client.proxy_store = Some(proxies.clone());
        let aliases = ModelAliasStore::new(db.clone());
        let api_keys = Arc::new(crate::key::ApiKeyStore::new(db.clone(), &settings.api_key)?);
        Ok(Self {
            statistics,
            settings,
            regions: regions.clone(),
            redis: None,
            dispatcher: AccountDispatcher::new(accounts.clone(), client)
                .with_log(log.clone())
                .with_regions(regions),
            health: BackendHealth::default(),
            inference_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(64)),
            accounts,
            proxies,
            log,
            aliases,
            api_keys,
            compact: CompactCache::default(),
            #[cfg(test)]
            db,
            models_cache: RwLock::new(None),
            models_refresh: Mutex::new(()),
            models_failed_at: Mutex::new(None),
            models_redis_stale: AtomicBool::new(false),
        })
    }

    pub fn with_redis(mut self, redis: Option<RedisKv>) -> Self {
        self.compact = std::mem::take(&mut self.compact).with_redis(redis.clone());
        self.redis = redis;
        self
    }

    /// 本机地理身份:locale / `<user-preferences>` 在选号前的占位来源。
    /// dispatcher 选号后会按账号出口探测并幂等改写([`crate::raycast::builder::apply_region`])。
    pub(crate) fn machine_identity(&self) -> crate::geo::Region {
        self.regions.machine_identity()
    }

    fn models_redis_key(&self) -> Option<(&RedisKv, String)> {
        self.redis.as_ref().map(|r| (r, r.key("models", "v1")))
    }

    /// Shared (L2) copy. Empty, undecodable, or possibly outdated copies count
    /// as a miss so a bad list can never be pinned for a day.
    async fn redis_models(&self) -> Option<Vec<RaycastRawModel>> {
        if self.models_redis_stale.load(Ordering::Relaxed) {
            return None;
        }
        let (redis, key) = self.models_redis_key()?;
        let bytes = redis.get(key).await?;
        match serde_json::from_slice::<Vec<RaycastRawModel>>(&bytes) {
            Ok(models) if !models.is_empty() => Some(models),
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(%error, "ignoring undecodable model list in Redis");
                None
            }
        }
    }

    async fn store_redis_models(&self, models: &[RaycastRawModel]) {
        let Some((redis, key)) = self.models_redis_key() else {
            return;
        };
        if models.is_empty() {
            return;
        }
        let stored = match serde_json::to_vec(models) {
            Ok(bytes) => redis.set_ex(key, bytes, MODELS_REDIS_TTL).await,
            Err(error) => {
                tracing::warn!(%error, "failed to serialize model list for Redis");
                false
            }
        };
        self.models_redis_stale.store(!stored, Ordering::Relaxed);
        if !stored {
            tracing::warn!("failed to cache model list in Redis");
        }
    }

    /// Return the model list: the in-process copy while fresh, else the shared
    /// Redis copy (1 day), else upstream. Upstream failures fall back to the
    /// last known list and back off for 30s; backoff skips Redis entirely.
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
        if let Some(models) = self.redis_models().await {
            *self.models_cache.write().await = Some(ModelsCache {
                fetched_at: Instant::now(),
                models: models.clone(),
            });
            return Ok(models);
        }
        match self.dispatcher.fetch_models().await {
            Ok(models) => {
                *self.models_failed_at.lock().await = None;
                *self.models_cache.write().await = Some(ModelsCache {
                    fetched_at: Instant::now(),
                    models: models.clone(),
                });
                self.store_redis_models(&models).await;
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

    /// 仅测试:让进程内模型缓存过期,迫使下一次读取走 Redis / 上游。
    #[cfg(test)]
    pub(crate) fn expire_models_l1(&mut self) {
        if let Some(cache) = self.models_cache.get_mut() {
            cache.fetched_at = Instant::now() - MODELS_CACHE_TTL;
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
            .ok_or_else(|| crate::key::model_not_found(client_id))
    }
}

#[cfg(test)]
pub fn test_regions() -> Arc<crate::geo::EgressResolver> {
    Arc::new(crate::geo::EgressResolver::new(
        reqwest::Client::new(),
        crate::geo::machine_region(),
    ))
}

#[cfg(test)]
pub(crate) fn compact_test_state() -> SharedState {
    let db = crate::storage::db::Db::open(":memory:").unwrap();
    let accounts = Arc::new(AccountStore::new(db.clone()));
    let statistics = Arc::new(crate::statistics::store::StatisticsStore::new(db.clone()));
    let log = Arc::new(RequestLogStore::new(db.clone()));
    let settings = Arc::new(Settings {
        raycast_signature_secret: "secret".into(),
        api_key: "test-key".into(),
        dashboard_password: None,
        dashboard_session_secret: None,
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        bind_addr_dual: None,
        upstream_timeout_secs: 20,
        accounts_db: ":memory:".into(),
    });
    let client = RaycastClient::new(settings.clone(), statistics.clone());
    let mut state = AppState::new(AppDeps {
        settings,
        client,
        accounts,
        log,
        statistics,
        db,
        regions: test_regions(),
    })
    .unwrap();
    state.models_cache = RwLock::new(Some(ModelsCache {
        fetched_at: Instant::now(),
        models: vec![
            serde_json::from_value(serde_json::json!({
                "id": "test-model", "model": "test", "provider": "openai", "context": 32,
                "abilities": {
                    "tools": {"supported": true},
                    "reasoning_effort": {"supported": true, "options": ["low", "high"], "default": "high"}
                }
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
    async fn model_alias_can_override_an_existing_model_id() {
        use axum::{
            Json,
            body::{Body, to_bytes},
            extract::State,
            http::{Request, StatusCode},
        };
        use tower::ServiceExt;

        use crate::server::handlers::management::models::{DeleteAliasParams, delete_alias};

        let state = compact_test_state();
        let opus = "claude-opus-5-5";
        let sonnet = "claude-sonnet-5-5";
        let haiku = "claude-haiku-5-5";
        *state.models_cache.write().await = Some(ModelsCache {
            fetched_at: Instant::now(),
            models: [opus, sonnet, haiku]
                .into_iter()
                .map(|id| {
                    serde_json::from_value(serde_json::json!({
                        "id": id, "model": id, "provider": "anthropic",
                        "owned_id": format!("anthropic-{id}")
                    }))
                    .unwrap()
                })
                .collect(),
        });
        assert_eq!(state.resolve_model(opus).await.unwrap().model, opus);
        let app = crate::server::build_router(state.clone());
        let create = |target| {
            app.clone().oneshot(
                Request::post("/v1/models/alias/create")
                    .header("authorization", "Bearer test-key")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"alias": opus, "model_id": target}).to_string(),
                    ))
                    .unwrap(),
            )
        };
        assert_eq!(create(sonnet).await.unwrap().status(), StatusCode::OK);
        let conflict = create(haiku).await.unwrap();
        assert_eq!(conflict.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(conflict.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body["message"],
            format!("映射「{opus}」已指向「{sonnet}」,请先删除原映射,再重新添加")
        );
        assert_eq!(state.resolve_model(opus).await.unwrap().model, sonnet);
        let reopened = ModelAliasStore::new(state.db.clone());
        assert_eq!(reopened.resolve(opus).as_deref(), Some(sonnet));
        delete_alias(
            State(state.clone()),
            Json(DeleteAliasParams { alias: opus.into() }),
        )
        .await
        .unwrap();
        assert_eq!(state.resolve_model(opus).await.unwrap().model, opus);

        state
            .aliases
            .create(opus, "anthropic-claude-sonnet-5-5")
            .unwrap();
        let response = create(sonnet).await.unwrap();
        assert!(response.status().is_success());
        assert_eq!(state.resolve_model(opus).await.unwrap().model, sonnet);
        let reopened = ModelAliasStore::new(state.db.clone());
        assert_eq!(reopened.resolve(opus).as_deref(), Some(sonnet));
    }

    #[tokio::test]
    async fn failed_model_refresh_keeps_known_model_instead_of_default() {
        let mut state = compact_test_state();
        let inner = Arc::get_mut(&mut state).unwrap();
        inner.expire_models_l1();
        let model = state.resolve_model("test-model").await.unwrap();
        assert_eq!(
            (model.provider.as_str(), model.model.as_str()),
            ("openai", "test")
        );
    }
}
