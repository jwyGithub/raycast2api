//! Shared application state passed to every handler.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::config::{DEFAULT_INTERNAL_MODEL, DEFAULT_PROVIDER, Settings};
use crate::debug::DebugBus;
use crate::error::GatewayError;
use crate::raycast::client::RaycastClient;
use crate::raycast::wire::RaycastRawModel;

use super::health::BackendHealth;

/// How long a fetched model list stays fresh before it is re-fetched.
const MODELS_CACHE_TTL: Duration = Duration::from_secs(300);

/// Reference-counted handle to the shared state (axum `State`).
pub type SharedState = Arc<AppState>;

pub struct AppState {
    pub settings: Arc<Settings>,
    pub client: RaycastClient,
    pub health: BackendHealth,
    /// Event bus for the local dev console (`/debug`). Shares its channel with
    /// the [`RaycastClient`] so inbound and upstream events reach the same UI.
    pub debug: DebugBus,
    models_cache: RwLock<Option<ModelsCache>>,
}

struct ModelsCache {
    fetched_at: Instant,
    models: Vec<RaycastRawModel>,
}

impl AppState {
    pub fn new(settings: Arc<Settings>, client: RaycastClient, debug: DebugBus) -> Self {
        Self {
            settings,
            client,
            health: BackendHealth::default(),
            debug,
            models_cache: RwLock::new(None),
        }
    }

    /// Return the model list, serving a cached copy while it is fresh and
    /// otherwise fetching from upstream (updating health and the cache).
    pub async fn models(&self) -> Result<Vec<RaycastRawModel>, GatewayError> {
        if let Some(cache) = self.models_cache.read().await.as_ref()
            && cache.fetched_at.elapsed() < MODELS_CACHE_TTL
        {
            return Ok(cache.models.clone());
        }

        match self.client.fetch_models().await {
            Ok(models) => {
                self.health.mark_working();
                *self.models_cache.write().await = Some(ModelsCache {
                    fetched_at: Instant::now(),
                    models: models.clone(),
                });
                Ok(models)
            }
            Err(err) => {
                self.health.mark_failed();
                Err(err)
            }
        }
    }

    /// Resolve an OpenAI-facing model id to `(provider, internal_model)`,
    /// falling back to the configured defaults if it can't be found.
    pub async fn resolve_model(&self, requested_id: &str) -> (String, String) {
        if let Ok(models) = self.models().await
            && let Some(model) = models.iter().find(|m| m.id == requested_id)
        {
            return (model.provider.clone(), model.model.clone());
        }
        (
            DEFAULT_PROVIDER.to_string(),
            DEFAULT_INTERNAL_MODEL.to_string(),
        )
    }
}
