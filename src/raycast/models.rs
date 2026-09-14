//! Raycast model registry helpers: resolve a client-facing model id to the
//! upstream `(provider, model)` pair, and reason about think/`-reasoning`
//! variants.
//!
//! The fetch + caching of the raw list lives in [`crate::server::state`] (so it
//! can share the app's HTTP client and serve-stale policy); this module holds
//! the pure resolution logic over an already-fetched list.

use crate::config::{DEFAULT_INTERNAL_MODEL, DEFAULT_PROVIDER};

use super::wire::RaycastRawModel;

/// Suffix Raycast uses to mark a model's reasoning ("think") variant.
pub const REASONING_SUFFIX: &str = "-reasoning";

/// The upstream `(provider, model)` a request will be sent with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub provider: String,
    pub model: String,
}

/// Resolve a client-facing model id to `(provider, upstream model)`, falling
/// back to the configured defaults when the id isn't in `models`.
pub fn resolve(models: &[RaycastRawModel], requested_id: &str) -> ResolvedModel {
    models
        .iter()
        .find(|m| m.id == requested_id)
        .map(|m| ResolvedModel {
            provider: m.provider.clone(),
            model: m.model.clone(),
        })
        .unwrap_or_else(|| ResolvedModel {
            provider: DEFAULT_PROVIDER.to_string(),
            model: DEFAULT_INTERNAL_MODEL.to_string(),
        })
}

/// Whether a raw model entry is a reasoning ("think") variant.
pub fn is_think_model(model: &RaycastRawModel) -> bool {
    model.model.ends_with(REASONING_SUFFIX) || model.id.ends_with(REASONING_SUFFIX)
}
